//! r.md #157 (`docs/plan_chord_track.md` §3.1): コードトラックの帯。Arranger 帯の直下に曲で 1 本、
//! 縦スクロールしても上に残る (`frame::build` が ruler / Arranger 帯と同じ head に積む)。
//!
//! 操作は Arranger 帯・クリップと同じ感触 (`input.rs`)、描画は `draw.rs`、ドラッグの解決は
//! `gesture.rs` (プレビューと確定が同じ 1 本を通る)。帯は他のゾーンと y が排他なので、press の
//! 優先順位で競合するのは全高に張るヘッダ幅のスプリッタだけ (`input::press` が先に見る)。
//!
//! 箱の色は `widgets::chord_colors` (ピアノロール上端の帯と共有する SSoT)、コード名は
//! `AppData::chord_label` (表記の SSoT) を通す。

use std::sync::Arc;

use daw_ui_renderer::{Color, Rect};

use crate::app::AppData;
use crate::state::chord_band::ChordLaneGeom;
use crate::widgets::chord_colors::{chord_box_fill, chord_box_ink};

mod controls;
mod draw;
mod gesture;
mod handoff;
mod input;

/// 帯の高さ (px)。Arranger 帯 (18px) よりわずかに高く、コード名を 11px で読める高さ。
const CHORD_LANE_H: f32 = 20.0;

/// 帯の高さ (隠していれば 0)。レイアウト (`frame::build` / `view_build`) の唯一の口。
#[must_use]
pub(in crate::widgets::arrangement) fn lane_h(app: &AppData) -> f32 {
    if app.cur.view.chord_band.visible { CHORD_LANE_H } else { 0.0 }
}

/// 箱 1 つの 1 フレーム分のビュー。
#[derive(Clone, Debug)]
pub(in crate::widgets::arrangement) struct ChordBox {
    pub id: u32,
    pub start: f64,
    pub len: f64,
    pub label: Arc<str>,
    pub fill: Color,
    pub ink: Color,
    pub selected: bool,
}

/// 帯の 1 フレーム分のビュー (`view_build::build` が組む)。
#[derive(Default)]
pub(in crate::widgets::arrangement) struct ChordLaneView {
    /// 位置順 (`Song.chords` の不変条件のまま)。
    pub boxes: Vec<ChordBox>,
}

#[must_use]
pub(in crate::widgets::arrangement) fn build_view(app: &AppData) -> ChordLaneView {
    let p = &app.theme.core;
    let song = app.cur.song_doc.song();
    let selected = &app.cur.selection.selected_chord_ids;
    let boxes = song
        .chords
        .iter()
        .map(|c| {
            let fill = chord_box_fill(p, c);
            ChordBox {
                id: c.id,
                start: c.start_beat,
                len: c.len_beats,
                label: Arc::from(app.chord_label(&c.chord, c.start_beat)),
                fill,
                ink: chord_box_ink(p, fill),
                selected: selected.contains(&c.id),
            }
        })
        .collect();
    ChordLaneView { boxes }
}

/// 帯の drag session (arrangement の widget state とは別の id に持つ)。
#[derive(Default)]
struct ChordLaneState {
    drag: Option<gesture::ChordDrag>,
    range: Option<gesture::RangeDrag>,
}

/// このフレームに生きている session の写し (描画とカーソルが読む)。
struct Live {
    drag: Option<gesture::ChordDrag>,
    range: Option<gesture::RangeDrag>,
}

/// 帯の 1 フレーム (press → 継続 → 描画 → release → dblclick / 右クリック → カーソル)。
/// `run.rs` が `cursor::apply` の後・`render::dispatch` の前に呼ぶ (カーソルは後勝ち、帯の描画は
/// プレイヘッド / ループ帯より奥)。
///
/// `released` はアレンジの release フレームに取り出した session — 帯の上で離したクリップの
/// ドラッグはここで取り上げる (`release::commit_releases` にクリップを動かさせない)。
pub(in crate::widgets::arrangement) fn run(
    ui: &mut daw_ui_core::Ui<'_, AppData>,
    app: &AppData,
    f: &super::super::frame::ArrangementFrame<'_>,
    released: &mut super::super::sessions::ReleasedSessions,
) {
    mirror(ui, app, f);
    if f.chord_rect.h <= 0.0 {
        return;
    }
    let clip_over = handoff::clip_over_band(ui, f);
    handoff::take_clip_drop(ui, app, f, released);
    input::escape(ui, f);
    input::press(ui, app, f);
    input::advance(ui, f);
    input::edge_scroll(ui, f);
    let (live, chord_released) = input::take(ui, f);
    draw::draw(ui, f, &live, clip_over);
    input::release(ui, app, f, chord_released);
    input::double_click(ui, app, f);
    input::secondary_click(ui, f);
    input::cursor(ui, f, &live);
}

/// `render::dispatch` の後に描くもの: 見出しのボタン (Arranger 帯の見出しの上に乗る) と、
/// 箱をレーンへ運んでいる間の落とし先のゴースト (レーンの上に乗る)。
pub(in crate::widgets::arrangement) fn after_render(
    ui: &mut daw_ui_core::Ui<'_, AppData>,
    app: &AppData,
    f: &super::super::frame::ArrangementFrame<'_>,
) {
    controls::draw(ui, app, f);
    if f.chord_rect.h <= 0.0 {
        return;
    }
    let drag = {
        let s: &mut ChordLaneState = ui.widget_state(input::state_id(f));
        s.drag.clone()
    };
    if let Some(d) = drag {
        handoff::draw_drop_ghost(ui, f, &d);
    }
}

/// 帯の位置を `ProjectEphemeral::chord_band.lane` へ写す (変わったフレームだけ)。
fn mirror(ui: &mut daw_ui_core::Ui<'_, AppData>, app: &AppData, f: &super::super::frame::ArrangementFrame<'_>) {
    let geom = ChordLaneGeom { rect: f.chord_rect, start_beat: f.view.start_beat, beat_per_px: f.beat_per_px };
    if app.cur.peph.chord_band.lane != geom {
        ui.push_edit(daw_ui_core::Edit::mutate(move |app: &mut AppData| app.cur.peph.chord_band.lane = geom));
    }
}

/// 箱の矩形 (帯の全高、x は ruler / クリップと同じ写像)。
fn box_rect(start: f64, len: f64, f: &super::super::frame::ArrangementFrame<'_>) -> Rect {
    super::super::geometry::section_rect_from(start, len, f.view, f.chord_rect)
}
