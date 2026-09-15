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
use crate::view::checked;

/// メニュー項目。**並び順は [`TrackMenuItem::ALL`] が SSoT** (ラベルと発行を index で割らない。
/// 順は `docs/plan_rmd_130_133_index.md` の約束)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackMenuItem {
    Rename,
    DuplicateUnique,
    DuplicateShared,
    Color,
    ResetClipColors,
    /// r.md #130: チェック付きの「移調に追従」。
    FollowTranspose,
    /// r.md #131: 「無効化」/「有効化」(右クリックしたトラックの状態で文言が変わる)。
    ToggleEnabled,
    Delete,
}

impl TrackMenuItem {
    const ALL: [Self; 8] = [
        Self::Rename,
        Self::DuplicateUnique,
        Self::DuplicateShared,
        Self::Color,
        Self::ResetClipColors,
        Self::FollowTranspose,
        Self::ToggleEnabled,
        Self::Delete,
    ];

    /// 表示ラベル。`state` は右クリックしたトラック自身の状態 (メニューを開いたフレームの値)。
    fn label(self, state: TrackMenuState) -> String {
        match self {
            Self::Rename => "Rename".into(),
            Self::DuplicateUnique => "複製 (独立)".into(),
            Self::DuplicateShared => "複製 (リンク)".into(),
            Self::Color => "色...".into(),
            Self::ResetClipColors => "クリップ色をトラックに揃える".into(),
            Self::FollowTranspose => checked("移調に追従", state.follows),
            Self::ToggleEnabled => if state.enabled { "無効化" } else { "有効化" }.into(),
            Self::Delete => "Delete".into(),
        }
    }
}

/// 右クリックしたトラック**自身**の値 (無効な group / 追従しない group の子でも自分の値で文言と向きを決める)。
#[derive(Debug, Clone, Copy)]
struct TrackMenuState {
    /// `Track::follow_transpose`。
    follows: bool,
    /// `Track::enabled`。
    enabled: bool,
}

/// track header の右クリックメニュー (Rename / 複製 / 色 / 移調に追従 / 無効化 / Delete) と改名 overlay。
///
/// rename 対象は安定 ID で直接持つ (index 経由の解決はしない = reorder/delete で
/// 別 track にすり替わらない、 SSoT)。
pub(crate) fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, resp: &ArrangementResponse) {
    let renaming_track_id = app.cur.peph.track_rename_id;
    let song = app.cur.song_doc.song();
    for &(track_id, rect) in &resp.track_header_rects {
        // master 行は `song.tracks` に居ない = 既定の値 (追従 / 有効) で出し、「無効化」を選ぶと handler が
        // 理由を status に出す (r.md #131)。
        let track = song.track_by_id(track_id);
        let state = TrackMenuState {
            follows: track.is_none_or(|t| t.follow_transpose),
            enabled: track.is_none_or(|t| t.enabled),
        };
        let labels = TrackMenuItem::ALL.map(|item| item.label(state));
        let items = labels.each_ref().map(String::as_str);
        ui.context_menu_for(rect, &items, move |idx, ui| {
            if let Some(&item) = TrackMenuItem::ALL.get(idx) {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| apply(app, item, track_id, rect, state)));
            }
        });

        if Some(track_id) == renaming_track_id {
            draw_rename_input(app, ui, track_id, rect);
        }
    }
}

/// メニュー項目 1 つを発行する。`state` はメニューを開いたときの右クリックしたトラックの値。
fn apply(app: &mut AppData, item: TrackMenuItem, track_id: u32, rect: Rect, state: TrackMenuState) {
    // 複製 (r.md #30) / 移調に追従 (r.md #130) / 無効化 (r.md #131) / 削除 (r.md #43) の対象: 右クリック track が
    // 選択集合に含まれるなら選択全体、 含まれないなら右クリック track 単独 (REAPER / Ableton 流)。
    // メニュー内で規則を割らない。
    let target_ids = |app: &AppData| {
        if app.cur.selection.selected_track_ids.contains(&track_id) {
            app.cur.selection.selected_track_ids.clone()
        } else {
            vec![track_id]
        }
    };
    match item {
        TrackMenuItem::Rename => app.handle_event(AppEvent::BeginRenameTrack(track_id)),
        // 独立複製 (Alt+D 相当): 元と切り離した別コピー。
        TrackMenuItem::DuplicateUnique => app.handle_event(AppEvent::DuplicateTracksUnique(target_ids(app))),
        // リンク複製 (D 相当): クリップ中身を元と content_id 共有。
        TrackMenuItem::DuplicateShared => app.handle_event(AppEvent::DuplicateTracksShared(target_ids(app))),
        // v18 (`docs/plan_track_clip_color.md`): color_picker を開く
        // (anchor = 右クリックした track header rect)。
        TrackMenuItem::Color => app.open_color_picker(ColorPickerTarget::Track(track_id), rect),
        // Ableton 流: track の全 clip の色上書きを外して track 色継承に戻す。
        TrackMenuItem::ResetClipColors => app.handle_event(AppEvent::ResetTrackClipColors { track: track_id }),
        // 右クリックしたトラックのチェックを反転した値に、対象全部を揃える (混在していても 1 回で揃う)。
        TrackMenuItem::FollowTranspose => {
            app.handle_event(AppEvent::SetTracksFollowTranspose { track_ids: target_ids(app), follow: !state.follows });
        }
        // r.md #131: 対象全体を右クリック track の反対へ揃える。
        TrackMenuItem::ToggleEnabled => {
            app.handle_event(AppEvent::SetTracksEnabled { track_ids: target_ids(app), enabled: !state.enabled });
        }
        TrackMenuItem::Delete => app.handle_event(AppEvent::DeleteTracks(target_ids(app))),
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
