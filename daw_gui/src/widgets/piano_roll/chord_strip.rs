//! r.md #157: ピアノロールのルーラー直下のコード帯 (表示専用、`docs/plan_chord_track.md` §3.3)。
//!
//! 開いているクリップの範囲にあるコードトラックの箱を、アレンジの帯と同じ色 (`chord_colors`) と
//! 同じ名前 (`AppData::chord_label`) で出す。編集はアレンジの帯とポップアップが受け持つ。

use daw_ui_core::ui::Ui;
use daw_ui_renderer::{GlyphArea, Rect, RectCommand};

use crate::app::{AppData, ClipKey};
use crate::widgets::chord_colors::{chord_box_fill, chord_box_ink};

use super::PianoRollView;

/// 箱の名前の文字サイズ (px)。
const FONT_PX: f32 = 11.0;
/// 名前を描く最小の箱の幅 (px)。これより細い箱は塗りだけ。
const MIN_LABEL_W: f32 = 12.0;

/// コード帯を出すか: 曲にコードがあり、曲の時間軸のクリップ (アレンジ) を開いているとき。
/// ランチャーのセルはセル内の時間なので出さない (コードは曲の拍に置かれている)。
pub(super) fn shown(app: &AppData, target: ClipKey) -> bool {
    let song = app.cur.song_doc.song();
    !song.chords.is_empty()
        && song.track_by_id(target.track_id).is_some_and(|t| t.session_clip_by_id(target.clip_id).is_none())
}

/// 帯を描く。`gutter_x` は鍵盤の左端 (帯の左の鍵盤幅ぶんに見出しを置く)。
#[allow(clippy::cast_possible_truncation)]
pub(super) fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, lane: Rect, gutter_x: f32, view: PianoRollView) {
    if lane.h <= 0.0 {
        return;
    }
    let p = &*app.theme.core;
    let whole = Rect { x: gutter_x, y: lane.y, w: lane.x + lane.w - gutter_x, h: lane.h };
    ui.push_rect(RectCommand::uniform_radius(whole, p.header, 0.0));
    let gutter = Rect { x: gutter_x + 4.0, y: text_top(lane), w: (lane.x - gutter_x - 6.0).max(0.0), h: lane.h };
    ui.label_at_clipped("pr_chord_lane_title", "Chords", gutter, FONT_PX, p.text_dim);

    let song = app.cur.song_doc.song();
    let beat_to_px = f64::from(lane.w) / view.len_beats.max(1e-6);
    let x_of = |beat: f64| lane.x + ((beat - view.start_beat) * beat_to_px) as f32;
    let view_end = view.start_beat + view.len_beats;
    // `chords` は昇順・非交差なので終端でも二分探索できる。
    let first = song.chords.partition_point(|c| c.end_beat() <= view.start_beat);
    for ev in song.chords[first..].iter().take_while(|c| c.start_beat < view_end) {
        let l = x_of(ev.start_beat).max(lane.x);
        let r = x_of(ev.end_beat()).min(lane.x + lane.w);
        if r - l < 1.0 {
            continue;
        }
        let rect = Rect { x: l, y: lane.y + 1.0, w: r - l, h: (lane.h - 2.0).max(1.0) };
        let fill = chord_box_fill(p, ev);
        let ink = chord_box_ink(p, fill);
        ui.push_rect(RectCommand {
            rect,
            fill,
            border: ink.with_alpha(0.45),
            border_width: 1.0,
            radius: [2.0; 4],
            clip_rect: Some(lane),
        });
        if rect.w >= MIN_LABEL_W {
            // 左端が画面外に出た箱も、見えている左端から名前を出す (いま鳴っているコードが読める)。
            ui.push_text(GlyphArea {
                text: app.chord_label(&ev.chord, ev.start_beat).into(),
                left: rect.x + 4.0,
                top: text_top(lane),
                font_size: FONT_PX,
                line_height: FONT_PX * 1.2,
                color: ink,
                clip_rect: Some(Rect { x: rect.x + 1.0, y: rect.y, w: (rect.w - 2.0).max(0.0), h: rect.h }),
                ..GlyphArea::default()
            });
        }
    }
    // playhead は grid と同じ色の線で帯も縦断する (どの箱が鳴っているかが見える)。
    if let Some(b) = view.playhead_beat
        && (view.start_beat..=view_end).contains(&b)
    {
        let x = x_of(b);
        ui.push_rect(RectCommand::uniform_radius(
            Rect { x: x - 1.0, y: lane.y, w: 2.0, h: lane.h },
            app.theme.daw.playhead,
            0.0,
        ));
    }
}

fn text_top(lane: Rect) -> f32 {
    lane.y + ((lane.h - FONT_PX * 1.2) * 0.5).max(0.0)
}
