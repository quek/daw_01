//! オーディオデバイスの層 (r.md #156、設計は `docs/plan_audio_io.md`)。
//!
//! - [`input`]: RT でデバイスの入力 ch の音を読む口 (録音の取り出しと、描画の中のモニター)。
//! - [`enumerate`]: この PC のデバイスの列挙 (開かない)。

pub mod enumerate;
pub mod input;

use common::audio_device::{AudioChannelInfo, AudioDeviceConfig, AudioDeviceInfo, AudioDriverKind};

/// OS 標準の既定の出力デバイスの情報 (stream は開かない)。
pub fn provisional_info(config: &AudioDeviceConfig) -> AudioDeviceInfo {
    use cpal::traits::{DeviceTrait, HostTrait};
    let device = cpal::default_host().default_output_device();
    let stream_config = device.as_ref().and_then(|d| d.default_output_config().ok());
    let channels = stream_config.as_ref().map_or(0, cpal::SupportedStreamConfig::channels);
    AudioDeviceInfo {
        driver: AudioDriverKind::System,
        output_name: device
            .as_ref()
            .and_then(enumerate::system_device_ref)
            .map(|d| d.name)
            .unwrap_or_default(),
        sample_rate: stream_config.as_ref().map_or(0, cpal::SupportedStreamConfig::sample_rate),
        output_channels: (0..channels).map(|index| AudioChannelInfo { index, name: String::new() }).collect(),
        fallback_reason: (config.driver != AudioDriverKind::System)
            .then(|| "このビルドは OS 標準の既定デバイスだけを開きます".to_string()),
        ..Default::default()
    }
}
