//! `fader` ウィジェット — 垂直スライダ。値範囲は `0.0..=1.0`。
//!
//! 設計の要点:
//! - 値は **アプリ側 Model が所有**。ライブラリは「現在値を借りて描き、ドラッグで
//!   新値を計算して `Edit<M>` を発行する」だけ。
//! - ドラッグ状態は `WidgetId` キーで `state` HashMap に持つ (`FaderState`)。
//!   no-Clone 制約を維持するため Model 側に状態を持たせない。
//! - 縦方向ドラッグ: 上 = 値増加、下 = 値減少。1 widget 高さ全部使う = 0 → 1。

use std::hash::Hash;

use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::ui::{Ui, hovered, lerp_color};

const TRACK_PAD: f32 = 8.0;
const THUMB_W: f32 = 24.0;
const THUMB_H: f32 = 12.0;

/// fader の永続状態 (フレーム間で保持)。
#[derive(Debug, Default)]
pub(crate) struct FaderState {
    /// ドラッグ中のアンカー: (押下時のマウス y, 押下時の value)。
    /// `None` ならドラッグしていない。
    drag_anchor: Option<(f32, f32)>,
}

/// fader の幾何計算: track (細い縦バー) と thumb (つまみ) の rect を返す。
fn fader_geometry(rect: Rect, value: f32) -> (Rect, Rect) {
    let track_w = 6.0;
    let track_x = rect.x + (rect.w - track_w) * 0.5;
    let track_top = rect.y + TRACK_PAD;
    let track_h = (rect.h - TRACK_PAD * 2.0).max(1.0);
    let track = Rect { x: track_x, y: track_top, w: track_w, h: track_h };
    let thumb_x = rect.x + (rect.w - THUMB_W) * 0.5;
    // value=1 → thumb_y は track 上端、value=0 → 下端付近に。
    let thumb_y_unclamped = track_top + (track_h - THUMB_H * 0.5) - track_h * value;
    let thumb_y = thumb_y_unclamped
        .clamp(track_top - THUMB_H * 0.5, track_top + track_h - THUMB_H * 0.5);
    let thumb = Rect { x: thumb_x, y: thumb_y, w: THUMB_W, h: THUMB_H };
    (track, thumb)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FaderResponse {
    /// 描画されている値 (ドラッグ中なら drag value、そうでなければ入力値と同じ)。
    pub displayed_value: f32,
    pub hovered: bool,
    pub dragging: bool,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 矩形指定で垂直 fader を描画 + ドラッグ + ヒットテスト。
    /// 値が変わったときだけ `on_change(new_value)` を呼んで `Edit<M>` を Edit 列に積む。
    ///
    /// `value` は `0.0..=1.0` 想定 (範囲外は内部でクランプ)。
    pub fn fader_at<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        value: f32,
        on_change: F,
    ) -> FaderResponse
    where
        F: FnOnce(f32) -> Edit<M>,
    {
        let wid = WidgetId::ROOT.child((b"fader", &id));
        let pointer = self.pointer;
        let value = value.clamp(0.0, 1.0);
        let (_, thumb_rect) = fader_geometry(rect, value);

        // 1. ドラッグ状態の更新。**つまみを押したときだけ** ドラッグ開始。
        let drag_anchor = {
            let state: &mut FaderState = self.widget_state(wid);
            if pointer.primary_just_pressed
                && let Some((px, py)) = pointer.pos
                && thumb_rect.contains(px, py)
            {
                state.drag_anchor = Some((py, value));
            }
            if pointer.primary_just_released {
                state.drag_anchor = None;
            }
            state.drag_anchor
        };

        // 2. 表示値を計算 (ドラッグ中ならアンカーから差分)。
        // 感度: track 1 本分のドラッグで 0 → 1 (= マウス移動が thumb 移動と 1:1)。
        let displayed_value = if let (Some((anchor_y, anchor_value)), Some((_, py))) =
            (drag_anchor, pointer.pos)
        {
            let track_h = (rect.h - TRACK_PAD * 2.0).max(1.0);
            let dv = -(py - anchor_y) / track_h;
            (anchor_value + dv).clamp(0.0, 1.0)
        } else {
            value
        };

        // 3. 描画。track + thumb。
        draw_fader(self, rect, displayed_value, drag_anchor.is_some(), pointer);

        // 4. 値が変わっていれば Edit を発行。
        if (displayed_value - value).abs() > f32::EPSILON {
            let edit = on_change(displayed_value);
            self.push_edit(edit);
        }

        FaderResponse {
            displayed_value,
            hovered: hovered(rect, pointer),
            dragging: drag_anchor.is_some(),
        }
    }

    /// vstack カーソル位置に固定高さで垂直 fader を追加 (高さ 120 px)。
    /// レイアウト調整が必要なら `fader_at` を直接使う。
    pub fn fader<F>(&mut self, id: impl Hash, value: f32, on_change: F) -> FaderResponse
    where
        F: FnOnce(f32) -> Edit<M>,
    {
        let pad = 8.0;
        let h = 120.0;
        let rect = Rect {
            x: self.cursor.x + pad,
            y: self.cursor.y + self.next_y,
            w: 32.0,
            h,
        };
        let resp = self.fader_at(id, rect, value, on_change);
        self.next_y += h + pad;
        resp
    }
}

fn draw_fader<M: ?Sized + 'static>(
    ui: &mut Ui<'_, M>,
    rect: Rect,
    value: f32,
    dragging: bool,
    pointer: crate::input::PointerFrame,
) {
    // 背景パネル
    ui.push_rect(RectCommand {
        rect,
        fill: Color::rgb(0.10, 0.11, 0.13),
        border: Color::rgb(0.25, 0.28, 0.33),
        border_width: 1.0,
        radius: [4.0; 4],
    });

    let (track, thumb) = fader_geometry(rect, value);

    // 細い track
    ui.push_rect(RectCommand {
        rect: track,
        fill: Color::rgb(0.18, 0.20, 0.24),
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [3.0; 4],
    });

    // 値部分 (track の下端から上に伸びる) を強調色で塗る
    let filled_h = track.h * value;
    if filled_h > 0.0 {
        ui.push_rect(RectCommand {
            rect: Rect {
                x: track.x,
                y: track.y + (track.h - filled_h),
                w: track.w,
                h: filled_h,
            },
            fill: Color::rgb(0.32, 0.55, 0.85),
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [3.0; 4],
        });
    }

    // thumb (つまみ単独で hover/press 判定する)
    let base = Color::rgb(0.62, 0.66, 0.74);
    let hover = Color::rgb(0.78, 0.82, 0.90);
    let press = Color::rgb(0.95, 0.97, 1.00);
    let thumb_fill = if dragging {
        press
    } else if hovered(thumb, pointer) {
        lerp_color(base, hover, 0.85)
    } else {
        base
    };
    ui.push_rect(RectCommand {
        rect: thumb,
        fill: thumb_fill,
        border: Color::rgb(0.30, 0.32, 0.36),
        border_width: 1.0,
        radius: [3.0; 4],
    });
}
