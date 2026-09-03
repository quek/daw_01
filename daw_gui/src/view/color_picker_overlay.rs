//! color_picker overlay (gui_01 #058): `color_picker_target` が `Some` の間、開いた場所
//! (header / clip / inspector swatch の rect) に picker を重ね、`picked` を対象種別ごとの
//! `Set*Color` イベントへ流す。対象は [`ColorPickerTarget`] (track / clip / automation clip /
//! section / scene)。`arrangement_view.rs` から切り出し (不変条件 9 のサイズ budget)。

use daw_ui_core::{ColorPickerStyle, Edit, Ui};
use daw_ui_renderer::Color;

use crate::app::{AppData, AppEvent, ColorPickerTarget};
use crate::view::track_color;

/// v18 (`docs/plan_track_clip_color.md`, gui_01 #058): `color_picker_target` が
/// `Some` の間、保存した anchor (開いた場所 = header / clip / inspector swatch の
/// rect) に color_picker overlay を描画する。`picked` は live で
/// `SetTrackColor`/`SetClipColor` に流す (open 中 widget 側は `current` を無視
/// するので flicker しない)、`dismissed` で target を `None` に戻す。対象 track /
/// clip が削除された (= 現在色を引けない) ときは picker を閉じる。
pub(crate) fn render(app: &AppData, ui: &mut Ui<'_, AppData>) {
    let (Some(target), Some(anchor)) = (app.ui_ephemeral.color_picker_target, app.ui_ephemeral.color_picker_anchor)
    else {
        return;
    };
    let style = ColorPickerStyle::from_palette(&app.theme.core);
    let palette = track_color::palette_colors();

    // 対象の現在色を引く。対象が消えていれば picker を閉じる。
    let current: Option<Color> = match target {
        ColorPickerTarget::Track(track_id) => app
            .song_doc.song()
            .track_by_id(track_id)
            .map(|t| track_color::to_renderer(track_color::effective_track_color(t))),
        ColorPickerTarget::Clip(clip_ref) => app
            .song_doc.song()
            .track_by_id(clip_ref.track_id)
            .and_then(|t| {
                t.clip_by_id(clip_ref.clip_id).map(|c| {
                    track_color::to_renderer(track_color::effective_clip_color(t, c))
                })
            }),
        ColorPickerTarget::AutomationClip(k) => app
            .song_doc
            .song()
            .automation_lane_by_key(k.track, k.lane)
            .and_then(|lane| {
                lane.clip_by_id(k.clip).map(|c| {
                    track_color::to_renderer(track_color::effective_automation_clip_color(lane, c))
                })
            }),
        ColorPickerTarget::AutomationLane(k) => app
            .song_doc
            .song()
            .automation_lane_by_key(k.track, k.lane)
            .map(|lane| track_color::to_renderer(track_color::effective_lane_color(lane))),
        ColorPickerTarget::Section(id) => app
            .song_doc.song()
            .sections
            .iter()
            .find(|s| s.id == id)
            .map(|s| Color { r: s.color[0], g: s.color[1], b: s.color[2], a: 1.0 }),
        // r.md #87: ランチャーの列。`Scene::color` は `None` = パレット既定なので、
        // 未設定のときは並び順から導いた既定色を初期値に見せる (トラック色と同流儀)。
        ColorPickerTarget::Scene(id) => app.song_doc.song().scenes.iter().position(|s| s.id == id).map(|i| {
            let s = &app.song_doc.song().scenes[i];
            let rgb = s.color.unwrap_or(track_color::PALETTE[i % track_color::PALETTE.len()]);
            Color { r: rgb[0], g: rgb[1], b: rgb[2], a: 1.0 }
        }),
    };

    let Some(current) = current else {
        ui.push_edit(Edit::mutate(|app: &mut AppData| app.close_color_picker()));
        return;
    };

    let r = ui.color_picker(("arr_color_picker", target_id_hash(target)), anchor, current, &palette, &style);
    if let Some(c) = r.picked {
        let rgb = track_color::from_renderer(c);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| match target {
            ColorPickerTarget::Track(track) => {
                app.handle_event(AppEvent::SetTrackColor { track, color: Some(rgb) });
            }
            ColorPickerTarget::Clip(clip_ref) => {
                app.handle_event(AppEvent::SetClipColor { target: clip_ref, color: Some(rgb) });
            }
            ColorPickerTarget::AutomationClip(k) => {
                app.handle_event(AppEvent::SetAutomationClipColor { target: k, color: Some(rgb) });
            }
            ColorPickerTarget::AutomationLane(k) => {
                app.handle_event(AppEvent::SetAutomationLaneColor { lane: k, color: Some(rgb) });
            }
            ColorPickerTarget::Section(id) => {
                app.handle_event(AppEvent::SetSectionColor { id, color: rgb });
            }
            // r.md #87: ランチャーの列。
            ColorPickerTarget::Scene(scene_id) => {
                app.handle_event(AppEvent::Launcher(
                    crate::event_launcher::LauncherEvent::SetSceneColor { scene_id, color: rgb },
                ));
            }
        }));
    }
    if r.dismissed {
        ui.push_edit(Edit::mutate(|app: &mut AppData| app.close_color_picker()));
    }
}

/// color_picker の widget id 用に target を一意な数値へ畳む (track / clip で衝突
/// しないよう track は最上位 bit を立てる)。
fn target_id_hash(target: ColorPickerTarget) -> u64 {
    match target {
        ColorPickerTarget::Track(id) => (1u64 << 63) | id as u64,
        ColorPickerTarget::Clip(r) => ((r.track_id as u64) << 32) | r.clip_id as u64,
        // 3 つの u32 を 60 bit に畳む (同時に開く picker は 1 つなので衝突しても実害無し)。
        ColorPickerTarget::AutomationClip(k) => {
            (1u64 << 60) | ((u64::from(k.track) << 40) ^ (u64::from(k.lane) << 20) ^ u64::from(k.clip))
        }
        ColorPickerTarget::AutomationLane(k) => {
            (1u64 << 59) | (u64::from(k.track) << 32) | u64::from(k.lane)
        }
        ColorPickerTarget::Section(id) => (1u64 << 62) | id as u64,
        ColorPickerTarget::Scene(id) => (1u64 << 61) | id as u64,
    }
}
