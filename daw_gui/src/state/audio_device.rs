//! オーディオデバイスの GUI 側の状態 (r.md #156、`docs/plan_audio_io.md`)。
//!
//! - `config`: **開きたいデバイス** (SSoT。`app_config.json` に永続)。変えるのは
//!   [`AudioDeviceEvent::SetConfig`](crate::event_audio_device::AudioDeviceEvent::SetConfig) だけ。
//! - `devices`: この PC にあるデバイスの一覧 (daw_audio が列挙した直近の結果)。
//! - `info`: **実際に開いているデバイス** (daw_audio の報告)。設定どおりに開けなかったときは
//!   `info.fallback_reason` に理由が入る (`config` は変えない)。

use common::audio_device::{AudioDeviceConfig, AudioDeviceInfo, AudioDeviceList};

#[derive(Debug, Clone, Default)]
pub struct AudioDeviceState {
    /// 開きたいデバイス。
    pub config: AudioDeviceConfig,
    /// デバイスの一覧。`None` = まだ列挙していない (`RefreshDevices` を投げると埋まる)。
    pub devices: Option<AudioDeviceList>,
    /// 実際に開いているデバイス。`None` = daw_audio と繋がっていない (起動に失敗した等)。
    pub info: Option<AudioDeviceInfo>,
    /// 列挙を頼んで返事を待っている (設定画面が「取得中」を出すため)。
    pub listing: bool,
    /// 直近の切り替えが失敗した理由 (daw_audio を起こし直せなかった等、`info` に載らない失敗)。成功で消える。
    pub error: Option<String>,
}

impl AudioDeviceState {
    pub fn new(config: AudioDeviceConfig, info: Option<AudioDeviceInfo>) -> Self {
        Self { config, devices: None, info, listing: false, error: None }
    }

    /// 設定画面に 1 行で出す「うまく行っていない理由」(切り替えの失敗 > 既定へ戻した理由)。無ければ `None`。
    pub fn problem(&self) -> Option<&str> {
        self.error
            .as_deref()
            .or_else(|| self.info.as_ref().and_then(|i| i.fallback_reason.as_deref()))
    }
}
