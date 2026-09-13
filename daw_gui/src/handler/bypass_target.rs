//! handler::bypass_target — `Q` の宛先 (`docs/plan_rack_native_devices.md` §10.12、K24)。
//!
//! Mixer 帯 / マスターパネル / Rack の行が hover として書く値の型と、そこから `Q` の
//! イベントを作る口。判定順 (マスターパネル → Mixer → 変調ラック → Rack の行 → …) の
//! 前半 2 段がここ、残りは `view::bypass_toggle::dispatch_toggle_mute`。

use crate::app::AppEvent;
use crate::event_device::DeviceEvent;
use crate::event_native::MasterLimiterEdit;
use crate::state::AppData;

/// `Q` で ON/OFF を切り替えられる対象。device は安定 id で指す (不変条件 1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BypassTarget {
    /// チェーン上の device (内蔵・plugin・Parallel)。
    Device(u64),
    /// master のフェーダー後 Limiter (チェーン外)。
    MasterLimiter,
}

impl AppData {
    /// カーソル直下の Mixer 帯 / マスターパネルの対象。マスターパネルは常時描かれるので先に見る。
    /// Mixer の hover は帯を描いたフレームにしか更新されないので、`mixer_active` (Mixer タブが
    /// 選ばれていて pointer が下部パネル内) で毎回ゲートする。
    pub(crate) fn hovered_bypass_target(&self, mixer_active: bool) -> Option<BypassTarget> {
        self.cur
            .peph
            .master_panel_hovered
            .or_else(|| mixer_active.then_some(self.cur.peph.mixer_hovered_native.map(BypassTarget::Device)).flatten())
    }

    /// `target` を反転するイベント。
    pub(crate) fn bypass_toggle_event(&self, target: BypassTarget) -> AppEvent {
        match target {
            BypassTarget::Device(id) => AppEvent::Device(DeviceEvent::SetDevicesBypassed {
                device_ids: vec![id],
                bypassed: !self.all_devices_bypassed(&[id]),
            }),
            BypassTarget::MasterLimiter => AppEvent::Device(DeviceEvent::MasterLimiterEdit(MasterLimiterEdit::On(
                !self.cur.song_doc.song().master_limiter.on,
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::NativeKind;

    /// F-G5: マスターパネル → Mixer の順、Mixer は `mixer_active` でゲートする。
    #[test]
    fn hovered_bypass_target_prefers_master_panel_and_gates_mixer() {
        let mut app = crate::test_support::headless_app();
        let song = app.cur.song_doc.song();
        let bus = song.builtin_native(common::model::MASTER_TRACK_ID, NativeKind::BusComp).expect("master の組み込み").id;
        let cur = song.native_by_id(bus).expect("bus").bypassed;
        let limiter_on = song.master_limiter.on;

        app.cur.peph.master_panel_hovered = Some(BypassTarget::Device(bus));
        app.cur.peph.mixer_hovered_native = Some(12345);
        assert_eq!(app.hovered_bypass_target(true), Some(BypassTarget::Device(bus)));
        assert_eq!(
            app.bypass_toggle_event(BypassTarget::Device(bus)),
            AppEvent::Device(DeviceEvent::SetDevicesBypassed { device_ids: vec![bus], bypassed: !cur })
        );
        app.cur.peph.master_panel_hovered = Some(BypassTarget::MasterLimiter);
        assert_eq!(app.hovered_bypass_target(false), Some(BypassTarget::MasterLimiter));
        assert_eq!(
            app.bypass_toggle_event(BypassTarget::MasterLimiter),
            AppEvent::Device(DeviceEvent::MasterLimiterEdit(MasterLimiterEdit::On(!limiter_on)))
        );

        app.cur.peph.master_panel_hovered = None;
        assert_eq!(app.hovered_bypass_target(true), Some(BypassTarget::Device(12345)));
        assert_eq!(app.hovered_bypass_target(false), None, "Mixer の hover は mixer_active でしか効かない");
    }
}
