//! CLAP `audio-ports` 拡張への問い合わせ (main / aux の channel 数)。`clap_plugin.rs` から機械的に切り出した
//! (挙動は元と同一、サイズ budget = 不変条件 9)。

use std::ffi::{c_char, c_void};

use clap_sys::ext::audio_ports::{
    CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS, clap_audio_port_info, clap_plugin_audio_ports,
};
use clap_sys::plugin::clap_plugin;

/// `clap_plugin::get_extension`。
pub(crate) type GetExtFn = unsafe extern "C" fn(*const clap_plugin, *const c_char) -> *const c_void;

pub(crate) fn query_output_channel_count(plugin: *const clap_plugin, get_ext: GetExtFn) -> u32 {
    query_port_channel_count(plugin, get_ext, false)
}

/// PR4 sidechain: enumerate the plugin's `is_main=false` input ports and
/// return their channel counts in declaration order (capped at MAX_AUX_IN).
pub(crate) fn query_aux_input_channels(plugin: *const clap_plugin, get_ext: GetExtFn) -> Vec<u32> {
    let ext_ptr =
        unsafe { get_ext(plugin, CLAP_EXT_AUDIO_PORTS.as_ptr()) } as *const clap_plugin_audio_ports;
    if ext_ptr.is_null() {
        return Vec::new();
    }
    let ext = unsafe { &*ext_ptr };
    let Some(count_fn) = ext.count else {
        return Vec::new();
    };
    let Some(get) = ext.get else {
        return Vec::new();
    };
    let port_count = unsafe { count_fn(plugin, true) };
    let mut aux: Vec<u32> = Vec::new();
    for i in 0..port_count {
        let mut info = std::mem::MaybeUninit::<clap_audio_port_info>::zeroed();
        let ok = unsafe { get(plugin, i, true, info.as_mut_ptr()) };
        if !ok {
            continue;
        }
        let info = unsafe { info.assume_init() };
        if info.flags & CLAP_AUDIO_PORT_IS_MAIN == 0 {
            if aux.len() >= common::process_data::MAX_AUX_IN {
                tracing::warn!(
                    port_index = i,
                    cap = common::process_data::MAX_AUX_IN,
                    "plugin declared more aux input ports than the host caps to"
                );
                break;
            }
            // shmem aux buffer は MAX_CHANNELS plane しか持たないので clamp。
            aux.push(info.channel_count.min(common::process_data::MAX_CHANNELS as u32));
        }
    }
    aux
}

/// パラアウト: enumerate the plugin's `is_main=false` **output** ports
/// (capped at MAX_AUX_OUT). Symmetric to `query_aux_input_channels`.
pub(crate) fn query_aux_output_channels(plugin: *const clap_plugin, get_ext: GetExtFn) -> Vec<u32> {
    let ext_ptr =
        unsafe { get_ext(plugin, CLAP_EXT_AUDIO_PORTS.as_ptr()) } as *const clap_plugin_audio_ports;
    if ext_ptr.is_null() {
        return Vec::new();
    }
    let ext = unsafe { &*ext_ptr };
    let Some(count_fn) = ext.count else {
        return Vec::new();
    };
    let Some(get) = ext.get else {
        return Vec::new();
    };
    let port_count = unsafe { count_fn(plugin, false) };
    let mut aux: Vec<u32> = Vec::new();
    for i in 0..port_count {
        let mut info = std::mem::MaybeUninit::<clap_audio_port_info>::zeroed();
        let ok = unsafe { get(plugin, i, false, info.as_mut_ptr()) };
        if !ok {
            continue;
        }
        let info = unsafe { info.assume_init() };
        if info.flags & CLAP_AUDIO_PORT_IS_MAIN == 0 {
            if aux.len() >= common::process_data::MAX_AUX_OUT {
                tracing::warn!(
                    port_index = i,
                    cap = common::process_data::MAX_AUX_OUT,
                    "plugin declared more aux output ports than the host caps to"
                );
                break;
            }
            aux.push(info.channel_count.min(common::process_data::MAX_CHANNELS as u32));
        }
    }
    aux
}

/// Queries the plugin's first audio port in the given direction. Returns `0`
/// when the plugin declares no port of that direction.
pub(crate) fn query_port_channel_count(
    plugin: *const clap_plugin,
    get_ext: GetExtFn,
    is_input: bool,
) -> u32 {
    let ext_ptr =
        unsafe { get_ext(plugin, CLAP_EXT_AUDIO_PORTS.as_ptr()) } as *const clap_plugin_audio_ports;
    if ext_ptr.is_null() {
        // Extension missing entirely — assume stereo output, no input.
        return if is_input { 0 } else { 2 };
    }
    let ext = unsafe { &*ext_ptr };
    let count_fn = match ext.count {
        Some(f) => f,
        None => return if is_input { 0 } else { 2 },
    };
    let port_count = unsafe { count_fn(plugin, is_input) };
    if port_count == 0 {
        return 0;
    }
    let Some(get) = ext.get else {
        return if is_input { 0 } else { 2 };
    };
    let mut info = std::mem::MaybeUninit::<clap_audio_port_info>::zeroed();
    let ok = unsafe { get(plugin, 0, is_input, info.as_mut_ptr()) };
    if !ok {
        return if is_input { 0 } else { 2 };
    }
    unsafe { info.assume_init() }.channel_count
}
