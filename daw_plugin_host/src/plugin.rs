use std::ffi::{CStr, CString, c_char, c_void};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap_sys::entry::clap_plugin_entry;
use clap_sys::ext::audio_ports::{CLAP_EXT_AUDIO_PORTS, clap_plugin_audio_ports};
use clap_sys::ext::note_ports::{CLAP_EXT_NOTE_PORTS, clap_plugin_note_ports};
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::host::clap_host;
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::version::clap_version_is_compatible;
use libloading::{Library, Symbol};

use crate::clap_host::Host;

/// Loaded CLAP plugin instance. Holds every resource alive until dropped; the
/// drop order (custom cleanup → fields → Library) ensures `destroy` / `deinit`
/// run before the DLL is unloaded.
pub struct Plugin {
    _library: Library,
    entry: *const clap_plugin_entry,
    plugin: *const clap_plugin,
    _host: Box<Host>,
    name: String,
    path: PathBuf,
}

// The plugin holds raw pointers but ownership is exclusive within the struct.
unsafe impl Send for Plugin {}

impl Plugin {
    /// Tries to load a plugin from `path`. Scans all descriptors in the file and
    /// instantiates the first one for which `matches(features)` returns true.
    /// Returns `Ok(None)` if no descriptor matches (library is unloaded cleanly).
    pub fn load_matching<F>(path: &Path, matches: F) -> Result<Option<Self>>
    where
        F: Fn(&[String]) -> bool,
    {
        let library = unsafe { Library::new(path) }
            .with_context(|| format!("failed to load CLAP library at {}", path.display()))?;

        let entry_ptr: *const clap_plugin_entry = unsafe {
            let sym: Symbol<*const clap_plugin_entry> = library
                .get(b"clap_entry\0")
                .context("CLAP library does not export clap_entry symbol")?;
            *sym
        };
        anyhow::ensure!(!entry_ptr.is_null(), "clap_entry symbol is null");
        let entry = unsafe { &*entry_ptr };

        anyhow::ensure!(
            clap_version_is_compatible(entry.clap_version),
            "CLAP version {}.{}.{} is incompatible with host",
            entry.clap_version.major,
            entry.clap_version.minor,
            entry.clap_version.revision
        );

        let path_str = path.to_string_lossy();
        let c_path = CString::new(path_str.as_bytes())
            .context("plugin path contains interior nul byte")?;
        let init_fn = entry.init.context("clap_plugin_entry::init is null")?;
        anyhow::ensure!(
            unsafe { init_fn(c_path.as_ptr()) },
            "clap_entry.init returned false for {}",
            path.display()
        );

        let get_factory = entry
            .get_factory
            .context("clap_plugin_entry::get_factory is null")?;
        let factory_ptr = unsafe { get_factory(CLAP_PLUGIN_FACTORY_ID.as_ptr()) }
            as *const clap_plugin_factory;
        anyhow::ensure!(!factory_ptr.is_null(), "clap_plugin_factory is null");
        let factory = unsafe { &*factory_ptr };

        let get_count = factory
            .get_plugin_count
            .context("factory.get_plugin_count is null")?;
        let get_desc = factory
            .get_plugin_descriptor
            .context("factory.get_plugin_descriptor is null")?;
        let create = factory
            .create_plugin
            .context("factory.create_plugin is null")?;

        let count = unsafe { get_count(factory_ptr) };
        tracing::info!(path = %path.display(), count, "plugins in factory");

        let mut selected: Option<u32> = None;
        for i in 0..count {
            let desc_ptr = unsafe { get_desc(factory_ptr, i) };
            if desc_ptr.is_null() {
                continue;
            }
            let desc = unsafe { &*desc_ptr };
            log_descriptor(i, desc);
            if selected.is_none() {
                let features = read_feature_list(desc.features);
                if matches(&features) {
                    selected = Some(i);
                }
            }
        }

        let Some(index) = selected else {
            // No descriptor matched — unload cleanly and report no match.
            if let Some(deinit) = entry.deinit {
                unsafe { deinit() };
            }
            drop(library);
            return Ok(None);
        };

        let desc_ptr = unsafe { get_desc(factory_ptr, index) };
        anyhow::ensure!(!desc_ptr.is_null(), "selected descriptor became null");
        let desc = unsafe { &*desc_ptr };
        let plugin_id = desc.id;
        let name = c_str_to_string(desc.name);

        let host = Host::new();
        let host_ptr: *const clap_host = &host.clap;

        let plugin_ptr = unsafe { create(factory_ptr, host_ptr, plugin_id) };
        anyhow::ensure!(!plugin_ptr.is_null(), "create_plugin returned null");

        let plugin_init = unsafe { (*plugin_ptr).init }.context("clap_plugin::init is null")?;
        anyhow::ensure!(
            unsafe { plugin_init(plugin_ptr) },
            "clap_plugin.init returned false"
        );
        tracing::info!(%name, index, "plugin initialized");

        let get_ext = unsafe { (*plugin_ptr).get_extension }
            .context("clap_plugin::get_extension is null")?;
        log_audio_ports(plugin_ptr, get_ext);
        log_note_ports(plugin_ptr, get_ext);

        Ok(Some(Self {
            _library: library,
            entry: entry_ptr,
            plugin: plugin_ptr,
            _host: host,
            name,
            path: path.to_path_buf(),
        }))
    }

    /// Loads the first plugin in the file (any type).
    pub fn load(path: &Path) -> Result<Self> {
        Self::load_matching(path, |_| true)?
            .ok_or_else(|| anyhow::anyhow!("no plugins in {}", path.display()))
    }
}

/// Returns true when the plugin declares the CLAP `instrument` feature.
pub fn is_instrument_features(features: &[String]) -> bool {
    features.iter().any(|f| f == "instrument")
}

impl Drop for Plugin {
    fn drop(&mut self) {
        unsafe {
            if let Some(destroy) = (*self.plugin).destroy {
                destroy(self.plugin);
            }
            if let Some(deinit) = (*self.entry).deinit {
                deinit();
            }
        }
        tracing::info!(name = %self.name, path = %self.path.display(), "plugin destroyed");
    }
}

fn c_str_to_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }
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

fn log_descriptor(index: u32, desc: &clap_plugin_descriptor) {
    let id = c_str_to_string(desc.id);
    let name = c_str_to_string(desc.name);
    let vendor = c_str_to_string(desc.vendor);
    let version = c_str_to_string(desc.version);
    let features = read_feature_list(desc.features);
    tracing::info!(index, %id, %name, %vendor, %version, ?features, "plugin descriptor");
}

type GetExtFn = unsafe extern "C" fn(*const clap_plugin, *const c_char) -> *const c_void;

fn log_audio_ports(plugin: *const clap_plugin, get_ext: GetExtFn) {
    let ext_ptr = unsafe { get_ext(plugin, CLAP_EXT_AUDIO_PORTS.as_ptr()) }
        as *const clap_plugin_audio_ports;
    if ext_ptr.is_null() {
        tracing::info!("audio-ports extension: not provided");
        return;
    }
    let ext = unsafe { &*ext_ptr };
    let Some(count_fn) = ext.count else {
        tracing::warn!("audio-ports.count is null");
        return;
    };
    let inputs = unsafe { count_fn(plugin, true) };
    let outputs = unsafe { count_fn(plugin, false) };
    tracing::info!(inputs, outputs, "audio-ports");
}

fn log_note_ports(plugin: *const clap_plugin, get_ext: GetExtFn) {
    let ext_ptr = unsafe { get_ext(plugin, CLAP_EXT_NOTE_PORTS.as_ptr()) }
        as *const clap_plugin_note_ports;
    if ext_ptr.is_null() {
        tracing::info!("note-ports extension: not provided");
        return;
    }
    let ext = unsafe { &*ext_ptr };
    let Some(count_fn) = ext.count else {
        tracing::warn!("note-ports.count is null");
        return;
    };
    let inputs = unsafe { count_fn(plugin, true) };
    let outputs = unsafe { count_fn(plugin, false) };
    tracing::info!(inputs, outputs, "note-ports");
}
