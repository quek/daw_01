//! トラックヘッダの右クリックメニューと、改名中の text_input overlay。
//!
//! widget (`widgets::arrangement`) は `track_header_rects` の収集までを担い、
//! メニュー項目の発行と改名 overlay は view 側 (ここ) が重ねる。
//! `arrangement_view::draw` から切り出した (r.md #130 / #131 が項目を足すため、
//! ファイル budget 1,000 行に余地を作った。`docs/plan_rmd_130_133_index.md`)。

use crate::widgets::arrangement::ArrangementResponse;
use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent, ColorPickerTarget};

/// track header の右クリックメニュー (Rename / 複製 / 色 / 無効化 / Delete) と改名 overlay。
///
/// rename 対象は安定 ID で直接持つ (index 経由の解決はしない = reorder/delete で
/// 別 track にすり替わらない、 SSoT)。
pub(crate) fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, resp: &ArrangementResponse) {
    let renaming_track_id = app.cur.peph.track_rename_id;
    for (track_id, rect) in &resp.track_header_rects {
        let track_id = *track_id;
        let rect = *rect;
        // r.md #131: 右クリックしたトラック自身の状態で文言と向きを決める (無効な group の子でも自分の値)。
        // master 行は `song.tracks` に居ない = 「無効化」を出し、選ぶと handler が理由を status に出す。
        let enabled = app.cur.song_doc.song().track_by_id(track_id).is_none_or(|t| t.enabled);
        ui.context_menu_for(
            rect,
            &[
                "Rename",
                "複製 (独立)",
                "複製 (リンク)",
                "色...",
                "クリップ色をトラックに揃える",
                if enabled { "無効化" } else { "有効化" },
                "Delete",
            ],
            move |idx, ui| {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| apply_choice(app, idx, track_id, rect, enabled)));
            },
        );

        if Some(track_id) == renaming_track_id {
            draw_rename_input(app, ui, track_id, rect);
        }
    }
}

/// メニューの `idx` 番目を選んだ (`draw` の項目の並びと 1 対 1)。`enabled` = 右クリックした track 自身の
/// `Track::enabled` (メニューを開いたフレームの値)。`draw` の closure から切り出した (入れ子の budget、不変条件 9)。
fn apply_choice(app: &mut AppData, idx: usize, track_id: u32, rect: Rect, enabled: bool) {
    // 複製 (r.md #30) / 無効化 (r.md #131) / 削除 (r.md #43) の対象: 右クリック track が
    // 選択集合に含まれるなら選択全体、 含まれないなら右クリック track
    // 単独 (REAPER / Ableton 流)。 メニュー内で規則を割らない。
    let target_ids = || {
        if app.cur.selection.selected_track_ids.contains(&track_id) {
            app.cur.selection.selected_track_ids.clone()
        } else {
            vec![track_id]
        }
    };
    match idx {
        0 => app.handle_event(AppEvent::BeginRenameTrack(track_id)),
        // 独立複製 (Alt+D 相当): 元と切り離した別コピー。
        1 => app.handle_event(AppEvent::DuplicateTracksUnique(target_ids())),
        // リンク複製 (D 相当): クリップ中身を元と content_id 共有。
        2 => app.handle_event(AppEvent::DuplicateTracksShared(target_ids())),
        // v18 (`docs/plan_track_clip_color.md`): color_picker を開く
        // (anchor = 右クリックした track header rect)。
        3 => app.open_color_picker(ColorPickerTarget::Track(track_id), rect),
        // Ableton 流: track の全 clip の色上書きを外して track 色継承に戻す。
        4 => app.handle_event(AppEvent::ResetTrackClipColors { track: track_id }),
        // r.md #131: 対象全体を右クリック track の反対へ揃える。
        5 => app.handle_event(AppEvent::SetTracksEnabled { track_ids: target_ids(), enabled: !enabled }),
        6 => app.handle_event(AppEvent::DeleteTracks(target_ids())),
        _ => {}
    }
}

/// 改名中の track header に text_input を重ねる。
///
/// text_input は track header rect の上端に被せる (M/S トグル等は隠れる)。
/// text_input widget が click で focus を取る。Enter で commit、Esc は
/// root の escape shortcut handler が CancelRenameTrack を発行する。
fn draw_rename_input(app: &AppData, ui: &mut Ui<'_, AppData>, track_id: u32, rect: Rect) {
    let input_rect = Rect {
        x: rect.x + 2.0,
        y: rect.y + 2.0,
        w: rect.w - 4.0,
        h: 22.0,
    };
    let resp = ui.text_input_at_focused(
        ("track_rename", track_id),
        input_rect,
        &app.cur.peph.track_rename_text,
        &ui.text_input_style(),
        |new| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::RenameTrackChanged(new.clone()));
            })
        },
    );
    // Enter (committed) でも外クリック (blurred = focus loss) でも確定する。
    // Esc は root の escape handler が CancelRenameTrack を出す (blurred には乗らない)。
    if resp.committed || resp.blurred {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::CommitRenameTrack);
        }));
    }
}
