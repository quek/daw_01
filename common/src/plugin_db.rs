//! CLAP plugin database.
//!
//! Scans system CLAP directories, enumerates every descriptor inside each
//! `.clap` file, and caches the resulting ID → (path, descriptor index) map
//! on disk so subsequent launches avoid the scan cost.
//!
//! Plugin state files save **plugin IDs**, not paths, so projects are
//! portable across machines as long as the same plugin (any path) is
//! installed.

use std::ffi::{CStr, CString, c_char};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap_sys::entry::clap_plugin_entry;
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::version::clap_version_is_compatible;
use libloading::{Library, Symbol};
use serde::{Deserialize, Serialize};

use crate::plugin_format::PluginFormat;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginEntry {
    /// Stable plugin identifier. For CLAP this is
    /// `clap_plugin_descriptor.id` (e.g. `com.vcvrack.rack`). For VST3 it
    /// is the class UUID rendered as 32 hex chars (no dashes).
    pub id: String,
    /// Which backend hosts this plugin. Defaults to CLAP so pre-VST3
    /// caches upgrade cleanly.
    #[serde(default)]
    pub format: PluginFormat,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub vendor: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// Absolute path to the plugin binary: a `.clap` file or, for VST3, the
    /// `.vst3` bundle directory (or legacy single DLL).
    pub path: PathBuf,
    /// Index within the factory. A single `.clap`/`.vst3` can host multiple
    /// descriptors (e.g. VCV Rack's `rack` / `rack.fx` / `rack.generator`,
    /// or a VST3 vendor shipping several classes in one DLL).
    pub descriptor_index: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginDatabase {
    pub entries: Vec<PluginEntry>,
    /// UNIX timestamp (seconds) of the last successful scan. Used for
    /// "rescan if older than X" heuristics in the future.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scanned_at: Option<u64>,
}

impl PluginDatabase {
    /// Returns the entry with matching `id`, or `None` if absent.
    pub fn find_by_id(&self, id: &str) -> Option<&PluginEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Load the cached database from disk. Missing file / invalid JSON
    /// returns `Ok(None)` rather than an error so callers can fall back
    /// to a fresh scan.
    pub fn load_from_file(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let db: PluginDatabase = serde_json::from_str(&data)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(Some(db))
    }

    /// Persist the database atomically (write to temp file + rename).
    pub fn save_to_file(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_string_pretty(self)
            .context("failed to serialize plugin database")?;
        fs::write(&tmp, data)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, path)
            .with_context(|| format!("failed to rename {} to {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

/// Standard cache location: `%LOCALAPPDATA%\daw_01\plugin_database.json`
/// on Windows, equivalent on other platforms via the `dirs` crate.
pub fn default_cache_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|p| p.join("daw_01").join("plugin_database.json"))
}

/// Stable identifiers for daw_01-bundled (`PluginFormat::Builtin`) plugins.
/// These URIs are used as both the descriptor `id` and the `path` field
/// in the cached `PluginEntry`, mirroring how external plugins use a
/// real filesystem path. The plugin host's `builtin` module dispatches
/// on the URI to construct the Rust implementation.
pub const BUILTIN_ID_SILENCE: &str = "builtin://daw_01.silence";
pub const BUILTIN_ID_VOICEVOX: &str = "builtin://daw_01.voicevox";

/// Returns the canonical list of daw_01-bundled plugin descriptors.
/// `scan_system` appends these unconditionally so the picker UI sees
/// them on every fresh scan. Order here is the order the picker
/// receives them.
pub fn builtin_descriptors() -> Vec<PluginEntry> {
    let version = env!("CARGO_PKG_VERSION");
    let instrument_features = vec![
        "instrument".to_string(),
        "synthesizer".to_string(),
    ];
    vec![
        PluginEntry {
            id: BUILTIN_ID_SILENCE.to_string(),
            format: PluginFormat::Builtin,
            name: "Silence (builtin)".to_string(),
            vendor: "daw_01".to_string(),
            version: version.to_string(),
            // Mirror CLAP feature taxonomy so pickers that filter by
            // category route this to the instrument list.
            features: instrument_features.clone(),
            path: PathBuf::from(BUILTIN_ID_SILENCE),
            descriptor_index: 0,
        },
        PluginEntry {
            id: BUILTIN_ID_VOICEVOX.to_string(),
            format: PluginFormat::Builtin,
            name: "VOICEVOX (builtin)".to_string(),
            vendor: "daw_01".to_string(),
            version: version.to_string(),
            features: {
                let mut f = instrument_features;
                f.push("vocal".to_string());
                f
            },
            path: PathBuf::from(BUILTIN_ID_VOICEVOX),
            descriptor_index: 0,
        },
    ]
}

/// Scan every `.clap` under the system CLAP directory plus every `.vst3`
/// bundle/DLL under the system VST3 directory, enumerating descriptors
/// where cheap (CLAP) and falling back to file-name metadata otherwise
/// (VST3 — scanning a VST3 bundle means loading the DLL + instantiating a
/// component, which we defer to load-time to keep startup fast).
///
/// daw_01-bundled plugins (`PluginFormat::Builtin`) are appended
/// unconditionally from [`builtin_descriptors`] so the picker UI always
/// shows them, even on a freshly-installed system with no external
/// plugins.
///
/// Errors for individual files are logged and skipped so a single broken
/// plugin doesn't block the whole database.
pub fn scan_system() -> Result<PluginDatabase> {
    let mut entries = builtin_descriptors();

    // --- CLAP branch: full descriptor enumeration (cheap).
    match crate::clap_scan::scan_system_clap_directory() {
        Ok(paths) => {
            for path in paths {
                match scan_one_file(&path) {
                    Ok(descs) => {
                        tracing::info!(
                            path = %path.display(),
                            count = descs.len(),
                            "scanned CLAP file"
                        );
                        entries.extend(descs);
                    }
                    Err(e) => {
                        tracing::warn!(error = ?e, path = %path.display(), "CLAP scan failed, skipping");
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = ?e, "CLAP directory enumeration failed");
        }
    }

    // --- VST3 branch: load each .vst3 briefly to enumerate its factory so
    // the picker sees each Audio Module Class as its own entry (matching
    // the CLAP behaviour). Per-plugin load cost is tens of ms, amortised
    // by the on-disk cache at `default_cache_path()`.
    match crate::vst3_scan::scan_vst3_classes() {
        Ok(classes) => {
            for c in classes {
                let features = c.features();
                entries.push(PluginEntry {
                    id: c.cid_hex,
                    format: PluginFormat::Vst3,
                    name: c.name,
                    vendor: c.vendor,
                    version: c.version,
                    features,
                    path: c.bundle_path,
                    descriptor_index: c.descriptor_index,
                });
            }
        }
        Err(e) => {
            tracing::warn!(error = ?e, "VST3 directory enumeration failed");
        }
    }

    Ok(PluginDatabase {
        entries,
        scanned_at: Some(now_secs()),
    })
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Loads a `.clap` just long enough to query its descriptors, then unloads
/// it cleanly. Does NOT instantiate any plugin (no `create_plugin` call).
fn scan_one_file(path: &Path) -> Result<Vec<PluginEntry>> {
    let library = unsafe { Library::new(path) }
        .with_context(|| format!("failed to load {}", path.display()))?;

    let entry_ptr: *const clap_plugin_entry = unsafe {
        let sym: Symbol<*const clap_plugin_entry> = library
            .get(b"clap_entry\0")
            .context("missing clap_entry symbol")?;
        *sym
    };
    anyhow::ensure!(!entry_ptr.is_null(), "clap_entry is null");
    let entry = unsafe { &*entry_ptr };

    if !clap_version_is_compatible(entry.clap_version) {
        anyhow::bail!(
            "incompatible CLAP version {}.{}.{}",
            entry.clap_version.major,
            entry.clap_version.minor,
            entry.clap_version.revision
        );
    }

    let path_str = path.to_string_lossy();
    let c_path = CString::new(path_str.as_bytes())
        .context("plugin path contains interior nul byte")?;
    let init_fn = entry.init.context("entry.init is null")?;
    anyhow::ensure!(
        unsafe { init_fn(c_path.as_ptr()) },
        "entry.init returned false"
    );

    // Always deinit + drop the library in this function's scope via a
    // guard, even if later steps fail — otherwise the DLL leaks.
    struct EntryGuard<'a> {
        entry: &'a clap_plugin_entry,
    }
    impl<'a> Drop for EntryGuard<'a> {
        fn drop(&mut self) {
            if let Some(deinit) = self.entry.deinit {
                unsafe { deinit() };
            }
        }
    }
    let _guard = EntryGuard { entry };

    let get_factory = entry.get_factory.context("get_factory is null")?;
    let factory_ptr = unsafe { get_factory(CLAP_PLUGIN_FACTORY_ID.as_ptr()) }
        as *const clap_plugin_factory;
    if factory_ptr.is_null() {
        return Ok(Vec::new());
    }
    let factory = unsafe { &*factory_ptr };
    let get_count = factory
        .get_plugin_count
        .context("factory.get_plugin_count is null")?;
    let get_desc = factory
        .get_plugin_descriptor
        .context("factory.get_plugin_descriptor is null")?;
    let count = unsafe { get_count(factory_ptr) };

    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let desc_ptr = unsafe { get_desc(factory_ptr, i) };
        if desc_ptr.is_null() {
            continue;
        }
        let desc = unsafe { &*desc_ptr };
        let id = cstr_to_string(desc.id);
        if id.is_empty() {
            tracing::warn!(index = i, path = %path.display(), "descriptor with empty id, skipping");
            continue;
        }
        out.push(PluginEntry {
            id,
            format: PluginFormat::Clap,
            name: cstr_to_string(desc.name),
            vendor: cstr_to_string(desc.vendor),
            version: cstr_to_string(desc.version),
            features: read_feature_list(desc.features),
            path: path.to_path_buf(),
            descriptor_index: i,
        });
    }

    Ok(out)
    // EntryGuard drops here: deinit runs, then library drops via the
    // function-scope `library` variable (which lives for the whole fn).
}

fn cstr_to_string(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p) }
        .to_string_lossy()
        .into_owned()
}

fn read_feature_list(ptr: *const *const c_char) -> Vec<String> {
    let mut out = Vec::new();
    if ptr.is_null() {
        return out;
    }
    let mut p = ptr;
    unsafe {
        loop {
            let s_ptr = *p;
            if s_ptr.is_null() {
                break;
            }
            out.push(CStr::from_ptr(s_ptr).to_string_lossy().into_owned());
            p = p.add(1);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_by_id_hit() {
        let db = PluginDatabase {
            entries: vec![PluginEntry {
                id: "com.example.foo".into(),
                format: PluginFormat::Clap,
                name: "Foo".into(),
                vendor: "Example".into(),
                version: "1.0".into(),
                features: vec!["instrument".into()],
                path: PathBuf::from("C:\\foo.clap"),
                descriptor_index: 0,
            }],
            scanned_at: Some(42),
        };
        assert_eq!(
            db.find_by_id("com.example.foo").map(|e| e.name.as_str()),
            Some("Foo")
        );
    }

    #[test]
    fn find_by_id_miss() {
        let db = PluginDatabase::default();
        assert!(db.find_by_id("missing").is_none());
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.json");
        let db = PluginDatabase {
            entries: vec![PluginEntry {
                id: "com.test.x".into(),
                format: PluginFormat::Clap,
                name: "X".into(),
                vendor: String::new(),
                version: "0.1.0".into(),
                features: vec![],
                path: PathBuf::from("/tmp/x.clap"),
                descriptor_index: 2,
            }],
            scanned_at: Some(100),
        };
        db.save_to_file(&path).unwrap();
        let loaded = PluginDatabase::load_from_file(&path).unwrap().unwrap();
        assert_eq!(loaded.entries, db.entries);
        assert_eq!(loaded.scanned_at, db.scanned_at);
    }

    #[test]
    fn load_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.json");
        assert!(PluginDatabase::load_from_file(&path).unwrap().is_none());
    }
}
