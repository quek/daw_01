//! VST3 plugin discovery on Windows.
//!
//! The VST3 SDK (since 3.6.10) specifies that plugins ship as bundles —
//! directories with a `.vst3` extension containing the actual DLL inside a
//! platform-specific subfolder (`Contents/x86_64-win/<name>.vst3` on
//! Windows). A handful of legacy plugins still ship as single `.vst3` DLLs
//! placed directly under `Common Files\VST3`, so we support both shapes.
//!
//! Beyond path discovery, [`scan_vst3_classes`] loads each `.vst3` DLL just
//! long enough to enumerate its factory (`countClasses` / `getClassInfo`
//! / `getClassInfo2`) so the plugin database can offer each Audio Module
//! Class as its own picker entry — mirroring how CLAP exposes multiple
//! descriptors inside a single `.clap` file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use libloading::{Library, Symbol};
use vst3::{
    ComPtr,
    Steinberg::{
        IPluginFactory, IPluginFactory2, IPluginFactory2Trait, IPluginFactoryTrait,
        PClassInfo, PClassInfo2, TUID, kResultOk,
    },
};

/// Discovered VST3 entry: the folder/file the user sees plus the actual
/// DLL libloading should load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vst3Entry {
    /// The `.vst3` bundle directory (or single DLL on legacy installs).
    /// This is what gets persisted in `PluginEntry.path` so projects stay
    /// portable across machines that happen to keep the same bundle name.
    pub bundle_path: PathBuf,
    /// Absolute path to the PE32+ DLL inside the bundle (or equal to
    /// `bundle_path` for the legacy single-DLL layout).
    pub dll_path: PathBuf,
}

/// Re-entry safety upper bound for the directory recursion. VST3 install
/// trees are vendor/category-deep but flat; 8 absorbs symlink loops without
/// imposing a real limit.
const MAX_DEPTH: u8 = 8;

/// Scans **every** VST3 location the spec defines — user
/// (`FOLDERID_UserProgramFilesCommon\VST3`), global
/// (`FOLDERID_ProgramFilesCommon\VST3`) and application (`<exe dir>\VST3`),
/// in that priority order (see [`crate::plugin_paths`] for the spec text).
///
/// Each root is walked **recursively** for `.vst3` entries (vendor
/// subfolders are standard per the VST3 SDK, e.g.
/// `…\VST3\Steinberg\HALion 7.vst3`) and each one's DLL path is resolved.
/// The `.vst3` bundle itself is treated as a leaf — we don't recurse into
/// `Contents/x86_64-win/` because that's the bundle internals, not a search
/// path. Individual unresolvable entries are logged and skipped.
///
/// Roots that don't exist are not an error; a root that exists but can't be
/// read is logged and skipped, so "not installed" and "couldn't look" stay
/// distinguishable.
pub fn scan_system_vst3_directory() -> Result<Vec<Vst3Entry>> {
    let roots = crate::plugin_paths::vst3_search_roots();
    if roots.is_empty() {
        tracing::info!("no VST3 search root exists on this machine");
        return Ok(Vec::new());
    }
    let mut plugins = Vec::new();
    for root in &roots {
        match scan_directory(root) {
            Ok(found) => plugins.extend(found),
            Err(e) => tracing::warn!(
                error = ?e,
                path = %root.display(),
                "VST3 search root scan failed, skipping"
            ),
        }
    }
    plugins.sort_by(|a, b| a.bundle_path.cmp(&b.bundle_path));
    // 仕様の優先度は User → Global → Application で「同じ Processor UID なら
    // 最初に見つかったもの」。 ここは bundle_path が同一のものだけを畳む
    // (別ルートの同名 bundle は別プラグインでありうるので UID では畳まない)。
    plugins.dedup_by(|a, b| a.bundle_path == b.bundle_path);
    Ok(plugins)
}

fn scan_directory(dir: &Path) -> Result<Vec<Vst3Entry>> {
    let mut plugins = Vec::new();
    walk(dir, &mut plugins, 0)?;
    plugins.sort_by(|a, b| a.bundle_path.cmp(&b.bundle_path));
    Ok(plugins)
}

fn walk(dir: &Path, out: &mut Vec<Vst3Entry>, depth: u8) -> Result<()> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading directory {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "vst3") {
            // bundle (file or dir) は leaf として処理。 中の Contents/… には入らない
            match resolve_vst3_dll(&path) {
                Ok(dll) => out.push(Vst3Entry {
                    bundle_path: path,
                    dll_path: dll,
                }),
                Err(e) => {
                    tracing::warn!(
                        error = ?e,
                        path = %path.display(),
                        "VST3 entry unresolved, skipping"
                    );
                }
            }
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            // 非 .vst3 のサブディレクトリは vendor/category folder の
            // 可能性。 再帰して中の .vst3 を拾う。
            if let Err(e) = walk(&path, out, depth + 1) {
                tracing::warn!(
                    error = ?e,
                    path = %path.display(),
                    "VST3 subdirectory scan failed, skipping"
                );
            }
        }
    }
    Ok(())
}

/// Returns the actual DLL path for the given `.vst3` bundle or legacy file.
///
/// Bundle layout (Windows x86_64):
///   `<name>.vst3/Contents/x86_64-win/<name>.vst3`
///
/// Legacy: `<name>.vst3` directly as a single PE32+ DLL.
pub fn resolve_vst3_dll(path: &Path) -> Result<PathBuf> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?;
    if meta.is_file() {
        return Ok(path.to_path_buf());
    }
    anyhow::ensure!(
        meta.is_dir(),
        "{} is neither a .vst3 DLL nor a bundle directory",
        path.display()
    );
    let Some(stem) = path.file_stem() else {
        anyhow::bail!("bundle {} has no file stem", path.display());
    };
    let mut dll = stem.to_os_string();
    dll.push(".vst3");
    let candidate = path
        .join("Contents")
        .join("x86_64-win")
        .join(&dll);
    anyhow::ensure!(
        candidate.exists(),
        "expected {} inside VST3 bundle {} but did not find it",
        candidate.display(),
        path.display()
    );
    Ok(candidate)
}

/// One Audio Module Class discovered inside a VST3 bundle. Populated by
/// [`scan_vst3_classes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vst3ClassEntry {
    /// The `.vst3` bundle (or legacy single DLL) the class lives in.
    pub bundle_path: PathBuf,
    /// 32-hex-digit form of the class CID. Stable across machines;
    /// persisted in `PluginEntry.id` and used at load time to pick the
    /// right descriptor.
    pub cid_hex: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    /// Pipe-separated subCategories from `PClassInfo2`, e.g. "Fx|Distortion".
    /// Empty when the factory only exposes `IPluginFactory` (PClassInfo).
    pub subcategories: String,
    /// Zero-based index within the factory's class list — informational
    /// only; we always load by CID.
    pub descriptor_index: u32,
    /// (r.md #5 ARA2) Whether a paired `ARA::IMainFactory` class (same name)
    /// exists in this binary — i.e. the plug-in supports ARA.
    pub is_ara: bool,
}

impl Vst3ClassEntry {
    /// Derives daw_gui picker `features` from VST3 `subcategories`. Maps
    /// VST3's "Instrument" / "Fx" to CLAP-flavoured tags so the existing
    /// picker filter (`instrument` / `audio-effect` / `note-effect`) keeps
    /// working without a format-aware rewrite.
    pub fn features(&self) -> Vec<String> {
        let mut out = Vec::new();
        let lower = self.subcategories.to_ascii_lowercase();
        if lower.split('|').any(|s| s == "instrument") {
            out.push("instrument".into());
        } else {
            // Default anything non-instrument (Fx, Spatial, Dynamics, etc.)
            // to the audio-effect bucket so the picker's effect filter
            // catches it.
            out.push("audio-effect".into());
        }
        if self.is_ara {
            // r.md #5: VST3 exposes no feature list, so normalise ARA support to
            // the same tag CLAP uses (`ara:supported`) — `PluginEntry::is_ara`
            // then derives uniformly for both formats.
            out.push("ara:supported".into());
        }
        out
    }
}

/// Scans every `.vst3` under the system VST3 directory, loads each one
/// briefly to enumerate its factory, and returns a flat list of Audio
/// Module Class entries. Failures for individual files are logged and
/// skipped (so one broken plugin doesn't hide the rest of the library).
pub fn scan_vst3_classes() -> Result<Vec<Vst3ClassEntry>> {
    let entries = scan_system_vst3_directory()?;
    let mut out = Vec::new();
    for entry in entries {
        match scan_one_vst3_file(&entry.bundle_path, &entry.dll_path) {
            Ok(mut classes) => {
                tracing::info!(
                    path = %entry.bundle_path.display(),
                    count = classes.len(),
                    "scanned VST3 bundle"
                );
                out.append(&mut classes);
            }
            Err(e) => {
                tracing::warn!(error = ?e, path = %entry.bundle_path.display(), "VST3 scan failed, skipping");
            }
        }
    }
    Ok(out)
}

fn scan_one_vst3_file(bundle_path: &Path, dll_path: &Path) -> Result<Vec<Vst3ClassEntry>> {
    let library = unsafe { Library::new(dll_path) }
        .with_context(|| format!("LoadLibrary {}", dll_path.display()))?;

    // Some VST3 DLLs require InitDll()/ExitDll() around factory access. No
    // export = fine. Once InitDll succeeds we must call ExitDll on every
    // exit path — including a panic during factory enumeration — so we pair
    // it with a Drop guard (same pattern as the CLAP EntryGuard).
    struct ExitDllGuard<'a> {
        library: &'a Library,
    }
    impl<'a> Drop for ExitDllGuard<'a> {
        fn drop(&mut self) {
            unsafe {
                if let Ok(exit_dll) =
                    self.library.get::<Symbol<extern "system" fn() -> bool>>(b"ExitDll\0")
                {
                    let _ = exit_dll();
                }
            }
        }
    }

    let mut _exit_guard = None;
    unsafe {
        if let Ok(init_dll) = library.get::<Symbol<extern "system" fn() -> bool>>(b"InitDll\0") {
            if !init_dll() {
                anyhow::bail!("InitDll returned false for {}", dll_path.display());
            }
            _exit_guard = Some(ExitDllGuard { library: &library });
        }
    }

    let result = (|| -> Result<Vec<Vst3ClassEntry>> {
        let get_factory: Symbol<extern "system" fn() -> *mut IPluginFactory> = unsafe {
            library
                .get(b"GetPluginFactory\0")
                .context("missing GetPluginFactory export")?
        };
        let factory_raw = get_factory();
        anyhow::ensure!(!factory_raw.is_null(), "GetPluginFactory returned null");
        let factory = unsafe { ComPtr::<IPluginFactory>::from_raw(factory_raw) }
            .context("factory ComPtr::from_raw returned None")?;
        let factory2 = factory.cast::<IPluginFactory2>();

        let count = unsafe { factory.countClasses() };
        // A malformed/hostile factory could report an absurd class count;
        // cap the pre-allocation so we don't trigger an allocation bomb.
        const MAX_CLASSES: i32 = 4096;
        let mut out = Vec::with_capacity(count.clamp(0, MAX_CLASSES) as usize);

        // (r.md #5 ARA2) First pass: collect the names of `ARA::IMainFactory`
        // classes. Their presence means the binary supports ARA; ARAVST3.h pairs
        // the main factory to the audio-module class of the same name.
        let mut ara_class_names: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for i in 0..count {
            let mut info = std::mem::MaybeUninit::<PClassInfo>::zeroed();
            if unsafe { factory.getClassInfo(i, info.as_mut_ptr()) } != kResultOk {
                continue;
            }
            let info = unsafe { info.assume_init() };
            if c_array_to_string(&info.category) == "ARA Main Factory Class" {
                ara_class_names.insert(c_array_to_string(&info.name));
            }
        }

        for i in 0..count {
            let mut info = std::mem::MaybeUninit::<PClassInfo>::zeroed();
            let res = unsafe { factory.getClassInfo(i, info.as_mut_ptr()) };
            if res != kResultOk {
                continue;
            }
            let info = unsafe { info.assume_init() };
            let category = c_array_to_string(&info.category);
            if category != "Audio Module Class" {
                continue;
            }
            let name = c_array_to_string(&info.name);
            let cid_hex = tuid_to_hex(&info.cid);

            let (vendor, version, subcategories) = if let Some(f2) = factory2.as_ref() {
                let mut info2 = std::mem::MaybeUninit::<PClassInfo2>::zeroed();
                let res2 = unsafe { f2.getClassInfo2(i, info2.as_mut_ptr()) };
                if res2 == kResultOk {
                    let info2 = unsafe { info2.assume_init() };
                    (
                        c_array_to_string(&info2.vendor),
                        c_array_to_string(&info2.version),
                        c_array_to_string(&info2.subCategories),
                    )
                } else {
                    (String::new(), String::new(), String::new())
                }
            } else {
                (String::new(), String::new(), String::new())
            };

            let is_ara = ara_class_names.contains(&name);
            out.push(Vst3ClassEntry {
                bundle_path: bundle_path.to_path_buf(),
                cid_hex,
                name,
                vendor,
                version,
                subcategories,
                descriptor_index: i as u32,
                is_ara,
            });
        }
        Ok(out)
    })();

    // The factory ComPtr is dropped at the closure's scope end above, before
    // `_exit_guard` runs ExitDll (guard drops here, then `library` frees the
    // DLL). A panic inside the closure unwinds the ComPtr first too, so
    // ExitDll is still ordered correctly on the panic path.
    result
}

/// Decode a NUL-terminated VST3 `c_char` name buffer into a `String`.
/// Shared SSoT — `daw_plugin_host` re-uses this so scan-side and load-side
/// string decoding can't drift.
pub fn c_array_to_string(buf: &[std::ffi::c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| b as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Format a VST3 class id (`TUID`) as uppercase hex. Shared SSoT — scan-side
/// (writes `cid_hex` to the plugin DB) and load-side (`daw_plugin_host`,
/// matches against it) MUST produce identical hex, so both call this.
pub fn tuid_to_hex(tuid: &TUID) -> String {
    let mut s = String::with_capacity(32);
    for b in tuid {
        s.push_str(&format!("{:02X}", *b as u8));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn resolves_legacy_single_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("legacy.vst3");
        std::fs::write(&path, b"").unwrap();
        assert_eq!(resolve_vst3_dll(&path).unwrap(), path);
    }

    #[test]
    fn resolves_bundle_layout() {
        let dir = tempdir().unwrap();
        let bundle = dir.path().join("foo.vst3");
        let dll_dir = bundle.join("Contents").join("x86_64-win");
        std::fs::create_dir_all(&dll_dir).unwrap();
        let dll = dll_dir.join("foo.vst3");
        std::fs::write(&dll, b"").unwrap();
        assert_eq!(resolve_vst3_dll(&bundle).unwrap(), dll);
    }

    #[test]
    fn missing_bundle_dll_errors() {
        let dir = tempdir().unwrap();
        let bundle = dir.path().join("broken.vst3");
        std::fs::create_dir_all(bundle.join("Contents").join("x86_64-win")).unwrap();
        assert!(resolve_vst3_dll(&bundle).is_err());
    }

    /// 0 件の root を walk する基本テスト。
    #[test]
    fn walk_empty_dir_yields_nothing() {
        let dir = tempdir().unwrap();
        assert!(scan_directory(dir.path()).unwrap().is_empty());
    }

    /// vendor subfolder 内の .vst3 bundle が拾えることを検証 (本修正の主目的)。
    #[test]
    fn walk_recurses_into_vendor_subfolder() {
        let dir = tempdir().unwrap();
        let vendor = dir.path().join("Steinberg");
        let bundle = vendor.join("HALion.vst3");
        let dll_dir = bundle.join("Contents").join("x86_64-win");
        std::fs::create_dir_all(&dll_dir).unwrap();
        std::fs::write(dll_dir.join("HALion.vst3"), b"").unwrap();
        let v = scan_directory(dir.path()).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].bundle_path, bundle);
    }

    /// .vst3 bundle 内の `Contents/` 等には再帰しない (内部ファイルを別 entry
    /// として誤検出しないこと)。
    #[test]
    fn walk_does_not_recurse_into_vst3_bundle() {
        let dir = tempdir().unwrap();
        let bundle = dir.path().join("foo.vst3");
        let dll_dir = bundle.join("Contents").join("x86_64-win");
        std::fs::create_dir_all(&dll_dir).unwrap();
        std::fs::write(dll_dir.join("foo.vst3"), b"").unwrap();
        // bundle 直下の dll も `.vst3` 拡張子だが、 walk は bundle に入らないので
        // 検出される entry は bundle 自体の 1 つだけ。
        let v = scan_directory(dir.path()).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].bundle_path, bundle);
    }
}
