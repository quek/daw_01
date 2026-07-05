//! プラグイン DLL scan — CLAP/VST3 の descriptor 列挙 (`--scan-plugins` one-shot モード)。
//!
//! arch-refactor S5-3 で `common::plugin_db` から移設した。DLL 実ロード
//! (`libloading` + `clap-sys` + `vst3`) を要するので plugin-host 側の責務であり、
//! **GUI プロセスは dlopen しない** (daw_gui は `--scan-plugins` サブプロセスを起動して
//! enumerated DB を受け取るだけ、`daw_gui::subprocess::scan_plugins`)。純データ
//! (`PluginDatabase` / `PluginEntry` / `builtin_descriptors`) は `common::plugin_db` に残る。

use std::ffi::{CStr, CString, c_char};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap_sys::entry::clap_plugin_entry;
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::version::clap_version_is_compatible;
use libloading::{Library, Symbol};

use common::plugin_db::{PluginDatabase, PluginEntry, builtin_descriptors};
use common::plugin_format::PluginFormat;

/// システムの CLAP/VST3 プラグインを列挙し、builtin と合わせた `PluginDatabase` を返す。
/// CLAP は descriptor をフル列挙 (安価)、VST3 は各 bundle の factory を読んで class ごとに
/// entry 化する。個別ファイルのエラーはログ + skip (1 つの壊れたプラグインが全体を止めない)。
/// port 構成は probe しない (`port_probe_version = 0`)。GUI 側 rescan が probe subprocess で上書きする。
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

    // --- VST3 branch: load each .vst3 briefly to enumerate its factory.
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
                    // VST3 は scan 時に bus を読めない (probe は別 subprocess)。probe 前の
                    // 保守的暫定値: 純 audio 扱い。probe が正確値で上書きする。
                    has_note_input: false,
                    has_note_output: false,
                    has_audio_output: true,
                    has_audio_input: false,
                    // 外部 VST3 は映像 device ではない。
                    has_video_input: false,
                    has_video_output: false,
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
        // scan_system は port を probe しない (GUI の rescan が probe 後に版を立てる)。0 = 未 probe。
        port_probe_version: 0,
    })
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Loads a `.clap` just long enough to query its descriptors, then unloads it cleanly.
/// Does NOT instantiate any plugin (no `create_plugin` call).
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

    // Always deinit + drop the library in this function's scope via a guard, even if later
    // steps fail — otherwise the DLL leaks.
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

    // Pre-reserve, but cap the up-front allocation so a hostile/garbage factory count can't
    // trigger a huge allocation. The loop below still honours the real `count`.
    const MAX_DESCRIPTORS: u32 = 4096;
    let mut out = Vec::with_capacity(count.min(MAX_DESCRIPTORS) as usize);
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
        // CLAP descriptor には port 有無が無いので、ここでは feature 由来の保守的暫定値。
        // 正確な port 構成は CLAP probe (rescan) が上書きする。
        let features = read_feature_list(desc.features, path);
        let has_note_eff = features.iter().any(|f| f == "note-effect");
        let has_instr = features.iter().any(|f| f == "instrument");
        out.push(PluginEntry {
            id,
            format: PluginFormat::Clap,
            name: cstr_to_string(desc.name),
            vendor: cstr_to_string(desc.vendor),
            version: cstr_to_string(desc.version),
            features,
            path: path.to_path_buf(),
            descriptor_index: i,
            // 純 MIDI FX (note-effect かつ instrument でない) 以外は audio 出力ありと仮定。
            has_note_input: has_note_eff || has_instr,
            has_note_output: has_note_eff,
            has_audio_output: !has_note_eff || has_instr,
            // audio 入力 (= audio エフェクト) は note-effect でも instrument でもない場合のみ true。
            has_audio_input: !has_note_eff && !has_instr,
            // 外部 CLAP は映像 device ではない。
            has_video_input: false,
            has_video_output: false,
        });
    }

    Ok(out)
    // EntryGuard drops here: deinit runs, then library drops via the function-scope `library`.
}

fn cstr_to_string(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p) }
        .to_string_lossy()
        .into_owned()
}

fn read_feature_list(ptr: *const *const c_char, path: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if ptr.is_null() {
        return out;
    }
    const MAX_FEATURES: usize = 256;
    let mut p = ptr;
    unsafe {
        let mut hit_cap = true;
        for _ in 0..MAX_FEATURES {
            let s_ptr = *p;
            if s_ptr.is_null() {
                hit_cap = false;
                break;
            }
            out.push(CStr::from_ptr(s_ptr).to_string_lossy().into_owned());
            p = p.add(1);
        }
        if hit_cap {
            tracing::warn!(
                path = %path.display(),
                "feature list reached {MAX_FEATURES} entries without NULL terminator, truncating",
            );
        }
    }
    out
}
