//! パラメーターのジェスチャー (`ParamGestureBegin / End`) を view から申告する唯一の口
//! (r.md #129 §7.6)。
//!
//! ## 所有者は面つき、寿命は「その面がこのフレームも描いたか」
//!
//! 同じ `(track, target)` を複数の面が描く (Mixer 帯と Rack Par の同じつまみ / アレンジの
//! ヘッダ音量と Mixer フェーダー) と、共有集合の「前フレームにドラッグしていたか」から edge を
//! 取る旧方式では、ドラッグしていない面が毎フレーム End を出して undo が 2 フレームごとに
//! 1 step 積まれ、録音も途切れた。
//!
//! 1. 面は 1 param につき毎フレーム [`push_param_gesture`] を 1 回だけ呼ぶ (dragging=false でも)。
//!    `dragging` はその面でその param を動かす全 widget の OR — 別々に呼ぶと片方の「非ドラッグ」が
//!    もう片方を End で閉じてしまう。
//! 2. 所有者が自分の面なら在席印 ([`ProjectEphemeral::param_gesture_seen`]) を立てる。
//! 3. フレーム末に [`sweep_param_gestures`] が、印の無い gesture (描かれなくなった = Delete /
//!    Ctrl+Tab / Ctrl+Z でつまみが消えた) を閉じる。PluginWindow / VideoPreview は描画と無関係に
//!    閉じるので対象外 ([`ParamSurface::swept`])。
//!
//! ## Begin は同じフレームの値より先に効く (prelude キュー)
//!
//! 申告は widget の応答 (`dragging`) を見てから積むので、widget が同じフレームに既に積んだ値の
//! Edit より **後ろ** に並ぶ — press でクリック位置へ飛ぶ (アレンジのヘッダ音量)、ドラッグが閾値を
//! 越えたフレームで最初の値を出す (数値欄)、ホイールの最初の notch (EQ 点の Q)。そのまま適用すると
//! 最初の値だけが gesture の外で 1 undo step 積まれ、1 操作が 2 step に割れる。
//!
//! そこで **Begin だけ** を daw-ui core の prelude キュー (`Ui::push_prelude_edit`) に積み、呼び出し側の
//! 描画順に依らず値より先に適用する。在席印と End は通常キューのまま (離したフレームに widget が
//! 出す最後の値を、閉じる前に適用するため)。呼び出し側は「widget を描いてから申告」のままでよい。
//!
//! [`ProjectEphemeral::param_gesture_seen`]: crate::state::ProjectEphemeral::param_gesture_seen

use common::model::AutomationTarget;
use daw_ui_core::{Edit, Ui};

use crate::app::{AppData, AppEvent, ParamSurface};

/// 1 面・1 param の毎フレームの申告。
///
/// | 所有者 | dragging | 動作 |
/// |---|---|---|
/// | 自分の面 | true | 在席印 |
/// | 自分の面 | false | `ParamGestureEnd` |
/// | 誰もいない | true | `ParamGestureBegin` (prelude = 同じフレームの値より先) |
/// | それ以外 (別の面が所有 / 誰も所有せず非ドラッグ) | — | 何もしない |
pub(crate) fn push_param_gesture(
    ui: &mut Ui<'_, AppData>,
    app: &AppData,
    surface: ParamSurface,
    track_id: u32,
    target: AutomationTarget,
    dragging: bool,
) {
    let owner = app.cur.recording.active_param_gestures.get(&(track_id, target.clone())).copied();
    match (owner, dragging) {
        (Some(s), true) if s == surface => {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.cur.peph.param_gesture_seen.insert((track_id, target));
            }));
        }
        (Some(s), false) if s == surface => {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ParamGestureEnd { surface, track_id, target });
            }));
        }
        (None, true) => {
            ui.push_prelude_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ParamGestureBegin { surface, track_id, target });
            }));
        }
        _ => {}
    }
}

/// フレーム末に 1 回だけ呼ぶ ([`crate::view::scrub_gesture::sweep`] の末尾から)。
/// `swept()` な面が持ち、今フレーム在席印の無い gesture を閉じ、印を空にする。
pub(crate) fn sweep_param_gestures(app: &mut AppData) {
    let stale: Vec<(ParamSurface, u32, AutomationTarget)> = app
        .cur
        .recording
        .active_param_gestures
        .iter()
        .filter(|(key, surface)| surface.swept() && !app.cur.peph.param_gesture_seen.contains(*key))
        .map(|((track_id, target), surface)| (*surface, *track_id, target.clone()))
        .collect();
    for (surface, track_id, target) in stale {
        app.end_param_gesture(surface, track_id, target);
    }
    app.cur.peph.param_gesture_seen.clear();
}

#[cfg(test)]
mod tests {
    use common::model::{AutomationTarget, TrackBuiltinParam};

    use super::sweep_param_gestures;
    use crate::app::{AppData, AppEvent, ParamSurface};

    fn vol() -> AutomationTarget {
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume)
    }

    /// F-G4: 面つきの所有者と sweep による寿命。
    #[test]
    fn param_gestures_are_owned_per_surface_and_swept_when_not_drawn() {
        let mut app = crate::test_support::headless_app();
        let begin = |app: &mut AppData, surface| {
            app.handle_event(AppEvent::ParamGestureBegin { surface, track_id: 1, target: vol() });
        };
        let end = |app: &mut AppData, surface| {
            app.handle_event(AppEvent::ParamGestureEnd { surface, track_id: 1, target: vol() });
        };
        let owner = |app: &AppData| app.cur.recording.active_param_gestures.get(&(1, vol())).copied();

        begin(&mut app, ParamSurface::Rack);
        assert_eq!(owner(&app), Some(ParamSurface::Rack));
        assert!(app.cur.song_doc.gesture_active());
        begin(&mut app, ParamSurface::MixerStrip);
        assert_eq!(owner(&app), Some(ParamSurface::Rack), "別の面は所有者を奪わない");
        end(&mut app, ParamSurface::MixerStrip);
        assert_eq!(owner(&app), Some(ParamSurface::Rack), "別の面の End は no-op");
        end(&mut app, ParamSurface::Rack);
        assert_eq!(owner(&app), None);
        assert!(!app.cur.song_doc.gesture_active());

        // Begin 直後の同じフレームの sweep では閉じない (Begin が在席印を立てる)。
        begin(&mut app, ParamSurface::Rack);
        sweep_param_gestures(&mut app);
        assert_eq!(owner(&app), Some(ParamSurface::Rack));
        // 在席印の無い sweep (= 描かれなかったフレーム) で閉じる。
        sweep_param_gestures(&mut app);
        assert_eq!(owner(&app), None);
        assert!(!app.cur.song_doc.gesture_active());

        // PluginWindow / VideoPreview は描画と無関係に閉じるので sweep で消えない。
        for surface in [ParamSurface::PluginWindow, ParamSurface::VideoPreview] {
            begin(&mut app, surface);
            sweep_param_gestures(&mut app);
            sweep_param_gestures(&mut app);
            assert_eq!(owner(&app), Some(surface));
            end(&mut app, surface);
            assert_eq!(owner(&app), None);
        }
    }
}
