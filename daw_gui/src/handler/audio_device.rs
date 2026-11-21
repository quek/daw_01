//! オーディオデバイスの設定変更 / 列挙 / daw_audio からの報告 (r.md #156、`docs/plan_audio_io.md`)。
//!
//! 切り替えの経路は 1 本: 設定 (`AppData::audio_device.config`) を保存し、**daw_audio を起こし直す**
//! (`AppData::restart_audio_child`)。新しい daw_audio は handshake で設定を受け取ってデバイスを開き、開けたものを
//! 報告する (`AudioDeviceInfo`)。開けなければ daw_audio が OS 標準の既定デバイスへ戻し、理由を
//! `info.fallback_reason` に入れる (設定は変えない)。

use common::audio_device::{AudioDeviceConfig, AudioDeviceInfo, AudioDeviceList};
use common::protocol::AudioCommand;

use crate::event_audio_device::AudioDeviceEvent;
use crate::state::AppData;

impl AppData {
    pub(crate) fn handle_audio_device_event(&mut self, ev: AudioDeviceEvent) {
        match ev {
            AudioDeviceEvent::SetConfig(config) => self.set_audio_device_config(config),
            AudioDeviceEvent::RefreshDevices => {
                self.audio_device.listing = self.ipc.audio_tx.is_some();
                self.send_audio(AudioCommand::ListAudioDevices);
            }
            AudioDeviceEvent::OpenControlPanel => {
                if self.audio_device.info.as_ref().is_some_and(|i| i.has_control_panel) {
                    // パネルの窓を出すのは daw_audio (の中のドライバー)。今はこちらが前面のプロセスなので、
                    // 前面に出る権利を渡す (渡さないと Windows の focus-steal 保護で本体窓の裏に出る。
                    // プラグインのエディタ窓と同じ — `handler::devices`)。
                    #[cfg(windows)]
                    unsafe {
                        use windows::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
                        let _ = AllowSetForegroundWindow(ASFW_ANY);
                    }
                    self.send_audio(AudioCommand::OpenDeviceControlPanel);
                }
            }
        }
    }

    /// 開きたいデバイスを替える。今と同じなら何もしない (デバイスを開き直さない)。
    fn set_audio_device_config(&mut self, config: AudioDeviceConfig) {
        if self.audio_device.config == config {
            return;
        }
        self.audio_device.config = config;
        // 「この PC の機材」なので app_config へ。
        self.persist_app_config();
        self.reopen_audio_device();
    }

    /// 今の設定でデバイスを開き直す (= daw_audio を起こし直す)。結果は `audio_device.info` / `error` に入る。
    pub(crate) fn reopen_audio_device(&mut self) {
        let Some(supervisor) = self.ipc.supervisor.clone() else {
            // 子プロセスを持たない経路 (単体テスト)。
            return;
        };
        supervisor.set_device_config(self.audio_device.config.clone());
        self.restart_audio_child();
        self.ui_ephemeral.status_message = match (&self.audio_device.error, &self.audio_device.info) {
            (Some(e), _) => format!("オーディオデバイスを開けませんでした: {e}"),
            (None, Some(info)) => match &info.fallback_reason {
                Some(reason) => format!("{reason} — {} で動いています", info.output_name),
                None => format!(
                    "オーディオデバイス: {} ({} Hz / {} frames)",
                    info.output_name, info.sample_rate, info.buffer_frames
                ),
            },
            (None, None) => "オーディオデバイスを開けませんでした".to_string(),
        };
    }

    /// 起こし直した daw_audio が handshake で報告したデバイス。
    pub(crate) fn on_audio_child_respawned(&mut self, info: AudioDeviceInfo) {
        self.on_audio_device_info(info);
        // 列挙の返事を待っている途中だったら聞き直す (古い daw_audio の返事はもう来ない)。
        if self.audio_device.listing {
            self.send_audio(AudioCommand::ListAudioDevices);
        }
    }

    /// daw_audio が居なくなり、起こし直せなかった (= どのデバイスも開いていない)。`error` はその理由。
    pub(crate) fn on_audio_child_gone(&mut self, error: Option<String>) {
        self.audio_device.info = None;
        self.audio_device.listing = false;
        if error.is_some() {
            self.audio_device.error = error;
        }
    }

    /// daw_audio が開いている (開き直した) デバイスを報告してきた。
    pub(crate) fn on_audio_device_info(&mut self, info: AudioDeviceInfo) {
        tracing::info!(?info, "audio device info");
        self.audio_device.info = Some(info);
        self.audio_device.error = None;
    }

    pub(crate) fn on_audio_devices_listed(&mut self, list: AudioDeviceList) {
        self.audio_device.devices = Some(list);
        self.audio_device.listing = false;
    }

    /// ドライバー側の変更 (コントロールパネルでレートを変えた等) で、今の session のまま続けられなくなった。
    /// 同じ設定のまま開き直す。
    pub(crate) fn on_audio_device_restart_required(&mut self, reason: String) {
        tracing::info!(%reason, "audio device asked for a restart");
        self.reopen_audio_device();
    }
}
