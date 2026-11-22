//! 帯の描画 (背景 + 見出し + 箱 + ドラッグのプレビュー)。プレビューの位置は `gesture::resolve`
//! (確定と同じ 1 本) から取る。
//!
//! 箱の文字は塗りから決めるインク (`chord_box_ink`、極性固定の明暗 2 択) なので、ユーザー着色の
//! 明るい箱でも暗い箱でも沈まない。選択は塗りに依らず見える 2 重リング (`push_selection_ring`)。

use std::sync::Arc;

use daw_ui_core::Ui;
use daw_ui_core::widgets::heavy::HeavyCtx;
use daw_ui_renderer::{GlyphArea, Rect};

use super::gesture::{Resolved, Scale};
use super::{ChordBox, Live, box_rect};
use crate::app::AppData;
use crate::widgets::arrangement::ArrangementStyle;
use crate::widgets::arrangement::draw::{push_filled_rect, push_section_border, push_selection_ring};
use crate::widgets::arrangement::frame::ArrangementFrame;

/// `clip_over` = アレンジのクリップを帯の上へ運んでいる (離すとコードを抽出する) — 帯を縁取る。
pub(super) fn draw(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>, live: &Live, clip_over: bool) {
    let s = Scale { beat_per_px: f.beat_per_px, snap: &f.view.snap, zoom: f.zoom_x_px_per_beat };
    // レーンの上へ運んでいる箱は帯では動かさない (離すとクリップになる、ゴーストは `handoff`)。
    let resolved = live.drag.as_ref().filter(|d| super::handoff::drop_track(f, d).is_none()).map(|d| d.resolve(s));
    let range = live.range.filter(|r| !r.is_click()).map(|r| r.span(s));
    ui.heavy(("arrangement_chord_lane", &f.id), |hctx| {
        let style = f.style;
        if f.chord_header_rect.w > 0.0 {
            push_filled_rect(hctx, f.chord_header_rect, style.arranger_lane_bg);
            label(hctx, f.chord_header_rect, "Chords", style.arranger_label_color, style);
        }
        push_filled_rect(hctx, f.chord_rect, style.arranger_lane_bg);
        hctx.with_clip_rect(f.chord_rect, |hctx| {
            draw_boxes(hctx, f, resolved.as_ref());
            if let Some((a, b)) = range {
                preview(hctx, box_rect(a, b - a, f), style);
            }
            if clip_over {
                push_filled_rect(hctx, f.chord_rect, style.arranger_preview_fill);
                push_section_border(hctx, f.chord_rect, super::handoff::band_drop_border(f), 2.0);
            }
        });
    });
}

/// 箱を描く。動かしている箱は base から外して最後に描く (重なる相手に食われない — Arranger 帯の 2 パスと同じ)。
/// 複製 (Ctrl) は元を残し、複製先に半透明のゴーストを出す。
fn draw_boxes(hctx: &mut HeavyCtx<'_, '_, AppData>, f: &ArrangementFrame<'_>, resolved: Option<&Resolved>) {
    let style = f.style;
    let moved: Vec<(u32, f64, f64)> = match resolved {
        Some(Resolved::Move { ids, delta, duplicate: false }) => {
            f.chord.boxes.iter().filter(|b| ids.contains(&b.id)).map(|b| (b.id, b.start + delta, b.len)).collect()
        }
        Some(Resolved::Spans(spans)) => spans.clone(),
        _ => Vec::new(),
    };
    for b in f.chord.boxes.iter().filter(|b| !moved.iter().any(|m| m.0 == b.id)) {
        draw_box(hctx, b, box_rect(b.start, b.len, f), style);
    }
    for &(id, start, len) in &moved {
        if let Some(b) = f.chord.boxes.iter().find(|b| b.id == id) {
            draw_box(hctx, b, box_rect(start, len, f), style);
        }
    }
    if let Some(Resolved::Move { ids, delta, duplicate: true }) = resolved {
        for b in f.chord.boxes.iter().filter(|b| ids.contains(&b.id)) {
            preview(hctx, box_rect(b.start + delta, b.len, f), style);
        }
    }
}

/// 半透明のゴースト (複製先 / 範囲)。
fn preview(hctx: &mut HeavyCtx<'_, '_, AppData>, r: Rect, style: &ArrangementStyle) {
    push_filled_rect(hctx, r, style.arranger_preview_fill);
    push_section_border(hctx, r, style.clip_border, 1.0);
}

fn draw_box(hctx: &mut HeavyCtx<'_, '_, AppData>, b: &ChordBox, r: Rect, style: &ArrangementStyle) {
    push_filled_rect(hctx, r, b.fill);
    if b.selected {
        push_selection_ring(hctx, r, style, 0.0, Some(r));
    } else {
        push_section_border(hctx, r, style.clip_border, 1.0);
    }
    if r.w > 8.0 {
        hctx.push_text(GlyphArea {
            // 毎フレーム描くので Arc の refcount clone (帯の名前と同じ安価な経路)。
            text: b.label.clone(),
            left: r.x + 4.0,
            top: r.y + (r.h - style.clip_text_size) * 0.5,
            font_size: style.clip_text_size,
            line_height: style.clip_text_size * 1.2,
            color: b.ink,
            clip_rect: Some(r),
            ..GlyphArea::default()
        });
    }
}

fn label(hctx: &mut HeavyCtx<'_, '_, AppData>, r: Rect, text: &str, color: daw_ui_renderer::Color, style: &ArrangementStyle) {
    if r.h <= style.clip_text_size + 2.0 {
        return;
    }
    hctx.push_text(GlyphArea {
        text: Arc::from(text),
        left: r.x + 4.0,
        top: r.y + (r.h - style.clip_text_size) * 0.5,
        font_size: style.clip_text_size,
        line_height: style.clip_text_size * 1.2,
        color,
        clip_rect: Some(r),
        ..GlyphArea::default()
    });
}
