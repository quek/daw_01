//! この PC のオーディオデバイスの列挙 (`AudioCommand::ListAudioDevices`)。デバイスは開かない。
//!
//! - OS 標準: cpal (WASAPI) の出力 / 入力デバイス。id は cpal の `DeviceId` の文字列 (`wasapi:<endpoint id>`)。
//! - ASIO: registry に登録されたドライバー (load しない)。id は CLSID の文字列。

use common::audio_device::{AudioDeviceEntry, AudioDeviceList, AudioDeviceRef};
use cpal::traits::{DeviceTrait, HostTrait};

/// cpal のデバイスの住所 (安定 id + 表示名)。id が取れないデバイスは選べないので `None`。
pub fn system_device_ref(device: &cpal::Device) -> Option<AudioDeviceRef> {
    let id = device.id().ok()?.to_string();
    let name = device.description().map_or_else(|_| id.clone(), |d| d.name().to_string());
    Some(AudioDeviceRef { id, name })
}

/// id (`system_device_ref` の `id`) から cpal のデバイスを引く。見つからなければ `None`。
pub fn find_system_device(host: &cpal::Host, id: &str) -> Option<cpal::Device> {
    let id: cpal::DeviceId = id.parse().ok()?;
    host.device_by_id(&id)
}

fn system_entries(
    devices: Option<impl Iterator<Item = cpal::Device>>,
    default: Option<cpal::Device>,
) -> Vec<AudioDeviceEntry> {
    let default_id = default.and_then(|d| d.id().ok()).map(|id| id.to_string());
    devices
        .into_iter()
        .flatten()
        .filter_map(|d| system_device_ref(&d))
        .map(|device| AudioDeviceEntry { is_default: Some(&device.id) == default_id.as_ref(), device })
        .collect()
}

/// ASIO ドライバーの CLSID の文字列 (`{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`、大文字)。
#[cfg(all(windows, target_pointer_width = "64"))]
pub fn clsid_string(clsid: &windows::core::GUID) -> String {
    format!("{{{clsid:?}}}").to_ascii_uppercase()
}

#[cfg(all(windows, target_pointer_width = "64"))]
fn asio_entries() -> Vec<AudioDeviceEntry> {
    crate::asio::registry::enumerate()
        .into_iter()
        .map(|d| AudioDeviceEntry {
            device: AudioDeviceRef { id: clsid_string(&d.clsid), name: d.name },
            is_default: false,
        })
        .collect()
}

#[cfg(not(all(windows, target_pointer_width = "64")))]
fn asio_entries() -> Vec<AudioDeviceEntry> {
    Vec::new()
}

/// 一覧を作る (数十 ms かかりうるので RT スレッドから呼ばない)。
pub fn list() -> AudioDeviceList {
    let host = cpal::default_host();
    AudioDeviceList {
        system_outputs: system_entries(host.output_devices().ok(), host.default_output_device()),
        system_inputs: system_entries(host.input_devices().ok(), host.default_input_device()),
        asio_drivers: asio_entries(),
    }
}
