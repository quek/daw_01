//! 設定 window (r.md #48 / #156) の開閉と、左のカテゴリの切り替え。
//!
//! 開閉 / 位置 / 選んでいるカテゴリの SSoT は `AppData.ui_prefs` で、`app_config.json` に永続する。
//! 各ページの中身 (先読み / VOICEVOX / テーマ / オーディオデバイス) は、それぞれの event と handler が持つ。

use crate::app::AppData;
use crate::event_audio_device::AudioDeviceEvent;
use crate::state::SettingsCategory;

impl AppData {
    /// 設定 window の開閉トグル (Edit メニュー / `Ctrl+,` / ✕ / Esc)。
    pub(crate) fn toggle_settings(&mut self) {
        self.ui_prefs.settings_open = !self.ui_prefs.settings_open;
        if self.ui_prefs.settings_open {
            self.settings_page_shown();
        }
        self.persist_app_config();
    }

    /// 左のカテゴリを選ぶ。
    pub(crate) fn set_settings_category(&mut self, category: SettingsCategory) {
        if self.ui_prefs.settings_category == category {
            return;
        }
        self.ui_prefs.settings_category = category;
        if self.ui_prefs.settings_open {
            self.settings_page_shown();
        }
        self.persist_app_config();
    }

    /// いま選んでいるページが画面に出るとき (window を開いた / カテゴリを替えた / 開いたまま起動した) に、
    /// そのページが表示に使う一覧を取り直す。**描画ループからは呼ばない** (ディスクや engine を
    /// フレームごとに叩くことになる)。開き直す / カテゴリを選び直すと新しい内容が出る。
    pub(crate) fn settings_page_shown(&mut self) {
        match self.ui_prefs.settings_category {
            // テーマ一覧の実体は `themes/` の read_dir + JSON パース。
            SettingsCategory::Theme => self.refresh_available_themes(),
            // 繋いだ / 外したデバイスが、ページを開き直すと一覧に出る。
            SettingsCategory::Audio => self.handle_audio_device_event(AudioDeviceEvent::RefreshDevices),
            SettingsCategory::Voicevox => {}
        }
    }
}
