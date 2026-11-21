//! オーディオデバイスの GUI イベント (r.md #156、`docs/plan_audio_io.md`)。
//!
//! [`AppEvent::AudioDevice`](crate::event::AppEvent::AudioDevice) が包む [`AudioDeviceEvent`] 1 本に集約する
//! (`AppEvent` へ variant を並べない理由は `event_sampler` と同じ)。処理は
//! [`AppData::handle_audio_device_event`](crate::state::AppData::handle_audio_device_event)。

use common::audio_device::AudioDeviceConfig;

#[derive(Debug, Clone, PartialEq)]
pub enum AudioDeviceEvent {
    /// 開きたいデバイスの設定を **丸ごと** 置き換える。今の設定と違えば保存し、daw_audio を起こし直して
    /// 開き直す (再生は止まる)。同じなら何もしない。
    ///
    /// view は `app.audio_device.config` を clone して 1 項目だけ変え、これを出す (デバイスを替えるときは
    /// `AudioDeviceConfig::set_driver` / `set_system_output` / `set_asio_driver` を使うと、前のデバイスに
    /// 依るバッファサイズ・ch の指定が「デバイスの既定」へ戻る)。
    SetConfig(AudioDeviceConfig),
    /// デバイスの一覧を取り直す (返事が来ると `app.audio_device.devices` が埋まる)。設定画面のオーディオの
    /// ページを開いたときと、「再スキャン」で出す。
    RefreshDevices,
    /// 開いているデバイスのコントロールパネル (ASIO ドライバーの設定画面) を開く。
    /// `info.has_control_panel` が false なら何もしない。
    OpenControlPanel,
}

impl AudioDeviceEvent {
    /// `AppEvent::undo_label` からの委譲。どれも Song を変えない (undo step を積まない) ので使われない。
    #[must_use]
    pub fn undo_label(&self) -> &'static str {
        crate::state::song_doc::GENERIC_UNDO_LABEL
    }
}
