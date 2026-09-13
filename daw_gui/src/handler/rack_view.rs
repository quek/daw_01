//! handler::rack_view — Rack の Par パネルの開閉 (`docs/plan_rack_native_devices.md` §10.5 / Q18)。
//!
//! 開閉は **見方の都合** (`ProjectView.open_rack_panels`、ViewState に保存、`*` なし)。
//! device ごとに独立して開き、他の Par を閉じない。

use common::model::RackPanelKey;

use crate::state::AppData;

impl AppData {
    /// `DeviceEvent::ToggleRackPanel`。
    pub(crate) fn toggle_rack_panel(&mut self, key: RackPanelKey) {
        let panels = &mut self.cur.view.open_rack_panels;
        if !panels.remove(&key) {
            panels.insert(key);
        }
    }

    /// `key` の Par が開いているか。
    pub(crate) fn rack_panel_open(&self, key: RackPanelKey) -> bool {
        self.cur.view.open_rack_panels.contains(&key)
    }
}
