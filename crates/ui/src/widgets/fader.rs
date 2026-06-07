//! `fader` ウィジェット — 垂直スライダ。
//!
//! `scale = None` のとき値範囲は `0.0..=1.0` (従来どおり)。
//! `scale = Some(s)` のとき値は dB 値で渡し、widget が `MeterScale` カーブで内部変換する。
//! これにより `level_meter_stereo` と同一 `MeterScale` を共有するとカーブが必ず一致する (SSoT)。
//!
//! 設計の要点:
//! - 値は **アプリ側 Model が所有**。ライブラリは「現在値を借りて描き、ドラッグで
//!   新値を計算して `Edit<M>` を発行する」だけ。
//! - ドラッグ状態は `WidgetId` キーで `state` HashMap に持つ (`FaderState`)。
//!   no-Clone 制約を維持するため Model 側に状態を持たせない。
//! - 縦方向ドラッグ: 上 = 値増加、下 = 値減少。1 widget 高さ全部使う = 0 → 1 (fraction 空間)。
//! - **DAW 標準挙動**:
//!   - ダブルクリックで `default_value` に戻る (~300ms × 5px 以内の 2 回目 press)
//!   - Ctrl + ドラッグで感度 1/10 (高精度)。Mid-drag toggle で値が jump しないよう再 anchor

use std::hash::Hash;
use std::time::Instant;

use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::scenegraph::hash_inputs;
use crate::ui::{Ui, hovered};
use crate::widgets::level_meter::MeterScale;

const TRACK_PAD: f32 = 8.0;
const THUMB_W: f32 = 28.0;
const THUMB_H: f32 = 10.0;

/// ダブルクリック判定の時間しきい値 (ms)。
const DOUBLE_CLICK_MS: u128 = 300;
/// ダブルクリック判定の位置しきい値 (px)。
const DOUBLE_CLICK_PX: f32 = 5.0;
/// Ctrl + ドラッグ時の感度倍率 (1/10)。
const FINE_DRAG_SCALE: f32 = 0.1;

/// fader の永続状態 (フレーム間で保持)。
#[derive(Debug, Default)]
pub(crate) struct FaderState {
    drag_anchor: Option<DragAnchor>,
    /// 直近のクリック (ダブルクリック判定用)。
    last_click: Option<ClickRecord>,
    /// M8 Phase 29: drag 開始時の値 (release frame で undoable Edit の inverse に使う)。
    drag_initial_value: Option<f32>,
}

#[derive(Debug, Clone, Copy)]
struct DragAnchor {
    /// 押下/再 anchor 時のマウス y。
    pointer_y: f32,
    /// 押下/再 anchor 時の value。
    value: f32,
    /// 押下/再 anchor 時の Ctrl 状態。mid-drag toggle で再 anchor するための判定用。
    ctrl: bool,
}

#[derive(Debug, Clone, Copy)]
struct ClickRecord {
    when: Instant,
    pos: (f32, f32),
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
    ///
    /// 値が変わったとき `on_change(new_value)` を呼んで `Edit<M>` を発行する。
    /// **M8 Phase 29**: drag 中の各フレームでは Mutate Edit を発行 (history に乗らない)、
    /// drag 終端でのみ Undoable Edit (start_value → end_value、label 付き) を 1 度だけ発行する。
    /// これにより undo/redo は drag 単位の意味のあるステップで巻き戻る (DAW 標準動作)。
    ///
    /// `on_change` は `Fn + Clone + Send + Sync + 'static` を要求 (= `move |v| Edit::mutate(...)` の
    /// 形で書く、capture は Copy 型のみが原則)。
    ///
    /// `scale = None` のとき `value` / `default_value` / `on_change` 引数はすべて `0.0..=1.0` fraction。
    /// `scale = Some(s)` のとき `value` / `default_value` / `on_change` 引数はすべて dB 値。
    ///   - `f32::NEG_INFINITY`（または curve 下端以下）はフェーダ最下端（無音）。
    ///   - widget が `s.curve` で dB→fraction 変換してハンドル位置を決定し、
    ///     fraction→dB 逆変換して `on_change(db)` を呼ぶ。
    ///   - `level_meter_stereo` に渡す `MeterScale` と同一インスタンスを使うとカーソルが必ず一致する。
    ///
    /// `label` は undoable history パネルでの表示文字列 ("fader" / "volume" 等)。
    ///
    /// 操作:
    /// - thumb をドラッグで値編集 (track 1 本分 = 0→1 fraction)
    /// - thumb をダブルクリック (~300ms / 5px 以内) で `default_value` に戻る
    /// - Ctrl + ドラッグで感度 1/10
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub fn fader_at<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        value: f32,
        default_value: f32,
        scale: Option<MeterScale>,
        label: &'static str,
        on_change: F,
    ) -> FaderResponse
    where
        F: Fn(f32) -> Edit<M> + Clone + Send + Sync + 'static,
    {
        // 値空間 (caller の dB or fraction) → 内部 fraction (0..=1) への変換
        let to_frac = |v: f32| -> f32 {
            match scale {
                None => v,
                Some(s) => if v.is_finite() { s.db_to_frac(v) } else { 0.0 },
            }
        };
        // 内部 fraction → 値空間 (dB or fraction) への変換。fraction=0 は無音 (NEG_INFINITY)。
        let to_val = move |frac: f32| -> f32 {
            match scale {
                None => frac,
                Some(s) => if frac <= 0.0 { f32::NEG_INFINITY } else { s.frac_to_db(frac) },
            }
        };

        let wid = WidgetId::ROOT.child((b"fader", &id));
        let pointer = self.pointer;
        let value = to_frac(value).clamp(0.0, 1.0);
        let default_value = to_frac(default_value).clamp(0.0, 1.0);
        let (_, thumb_rect) = fader_geometry(rect, value);

        // 1. 押下処理 + 2. mid-drag ctrl toggle 再 anchor + 3. release 解除
        let mut reset_fired = false;
        let (drag_anchor, release_initial_value) = {
            let state: &mut FaderState = self.widget_state(wid);

            // 押下: ダブルクリック判定 → リセット 又は drag 開始 (thumb 内のみ)
            if pointer.primary_just_pressed
                && let Some((px, py)) = pointer.pos
                && thumb_rect.contains(px, py)
            {
                let now = Instant::now();
                let is_double = state.last_click.is_some_and(|c| {
                    now.duration_since(c.when).as_millis() < DOUBLE_CLICK_MS
                        && (c.pos.0 - px).hypot(c.pos.1 - py) < DOUBLE_CLICK_PX
                });

                if is_double {
                    // リセット。drag は始めない。3 連クリック誤動作防止のため last_click も消す。
                    state.last_click = None;
                    state.drag_anchor = None;
                    state.drag_initial_value = None;
                    reset_fired = true;
                } else {
                    state.last_click = Some(ClickRecord { when: now, pos: (px, py) });
                    state.drag_anchor = Some(DragAnchor {
                        pointer_y: py,
                        value,
                        ctrl: pointer.modifiers.ctrl,
                    });
                    // M8 Phase 29: drag 開始時の値を保存 (release frame で inverse に使う)
                    state.drag_initial_value = Some(value);
                }
            }

            // mid-drag で Ctrl 状態が変わったら anchor を張り直す:
            // 再 anchor しないと cumulative-from-anchor の delta 全体に新スケールが掛かって
            // 値が jump する。`(現在 py, 現在 value, 現在 ctrl)` に張り直すことで
            // return-to-press-position の cumulative 性質を保ったまま境界で滑らかに切替わる。
            if let Some(anchor) = state.drag_anchor
                && let Some((_, py)) = pointer.pos
                && pointer.modifiers.ctrl != anchor.ctrl
            {
                state.drag_anchor = Some(DragAnchor {
                    pointer_y: py,
                    value,
                    ctrl: pointer.modifiers.ctrl,
                });
            }

            // M8 Phase 29: release frame で初期値を取り出す。
            let release_initial_value = if pointer.primary_just_released {
                state.drag_initial_value.take()
            } else {
                None
            };

            if pointer.primary_just_released {
                state.drag_anchor = None;
            }

            (state.drag_anchor, release_initial_value)
        };

        // 表示値を計算 (リセット > drag > 入力値の優先)
        let displayed_value = if reset_fired {
            default_value
        } else if let (Some(anchor), Some((_, py))) = (drag_anchor, pointer.pos) {
            let track_h = (rect.h - TRACK_PAD * 2.0).max(1.0);
            let scale = if anchor.ctrl { FINE_DRAG_SCALE } else { 1.0 };
            let raw_dv = -(py - anchor.pointer_y) / track_h;
            (anchor.value + raw_dv * scale).clamp(0.0, 1.0)
        } else {
            value
        };

        // 描画。track + thumb。M4 Phase 11: with_widget_node で input_hash キャッシュ。
        let dragging = drag_anchor.is_some();
        let (_, thumb_hover_rect) = fader_geometry(rect, displayed_value);
        let hovered_thumb = pointer.pos.is_some_and(|(px, py)| thumb_hover_rect.contains(px, py));
        let input_hash = hash_inputs((
            b"fader",
            rect.x.to_bits(),
            rect.y.to_bits(),
            rect.w.to_bits(),
            rect.h.to_bits(),
            displayed_value.to_bits(),
            dragging,
            hovered_thumb,
        ));
        self.with_widget_node(wid, input_hash, |ui| {
            draw_fader(ui, rect, displayed_value, dragging, pointer);
        });

        // M8 Phase 29: drag 終端では Mutate を抑制 (Undoable Edit が forward を再実行するため
        // model 値が二重更新されないようにする)。release_initial_value が Some なら drag 終端。
        let suppress_mutate_on_release = release_initial_value.is_some();
        if !suppress_mutate_on_release && (displayed_value - value).abs() > f32::EPSILON {
            let edit = on_change(to_val(displayed_value));
            self.push_edit(edit);
        }

        // M8 Phase 29: drag 終端で Undoable Edit を発行 (start_value → end_value)。
        // 値変化が無ければ no-op (= 押下しただけで動かさず release した場合は history を汚さない)。
        if let Some(start_frac) = release_initial_value
            && (start_frac - displayed_value).abs() > f32::EPSILON
        {
            let on_change_fwd = on_change.clone();
            let on_change_inv = on_change;
            let end_val = to_val(displayed_value);
            let start_val = to_val(start_frac);
            let edit = Edit::with_inverse(
                label,
                move |m: &mut M| on_change_fwd(end_val).apply(m),
                move |m: &mut M| on_change_inv(start_val).apply(m),
            );
            self.push_edit(edit);
        }

        FaderResponse {
            displayed_value: to_val(displayed_value),
            hovered: hovered(rect, pointer),
            dragging: drag_anchor.is_some(),
        }
    }

    /// vstack カーソル位置に固定高さで垂直 fader を追加 (高さ 120 px)。
    /// レイアウト調整が必要なら `fader_at` を直接使う。
    pub fn fader<F>(
        &mut self,
        id: impl Hash,
        value: f32,
        default_value: f32,
        scale: Option<MeterScale>,
        label: &'static str,
        on_change: F,
    ) -> FaderResponse
    where
        F: Fn(f32) -> Edit<M> + Clone + Send + Sync + 'static,
    {
        let pad = 8.0;
        let h = 120.0;
        let rect = Rect {
            x: self.cursor.x + pad,
            y: self.cursor.y + self.next_y,
            w: 32.0,
            h,
        };
        let resp = self.fader_at(id, rect, value, default_value, scale, label, on_change);
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
        clip_rect: None,
    });

    let (track, thumb) = fader_geometry(rect, value);

    // 細い track
    ui.push_rect(RectCommand {
        rect: track,
        fill: Color::rgb(0.18, 0.20, 0.24),
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [3.0; 4],
        clip_rect: None,
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
            clip_rect: None,
        });
    }

    // thumb: flat な水平バー (border / shadow なし、Ableton 系ミニマル)
    let base = Color::rgb(0.78, 0.82, 0.90);
    let press = Color::rgb(0.95, 0.97, 1.00);
    let thumb_fill = if dragging || hovered(thumb, pointer) {
        press
    } else {
        base
    };
    ui.push_rect(RectCommand {
        rect: thumb,
        fill: thumb_fill,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [1.0; 4],
        clip_rect: None,
    });
}

#[cfg(test)]
mod tests {
    //! fader の双方向挙動テスト:
    //! - ダブルクリック: 同位置 / 時間しきい値内なら default_value にリセット
    //! - drag: 通常感度と Ctrl 押下時の 1/10 感度
    //! - mid-drag Ctrl toggle: 値 jump せず再 anchor される
    //!
    //! 時刻は `Instant::now()` を widget 内で直接呼ぶ実装なので、テストは `thread::sleep`
    //! でしきい値の前後を作り出す。total sleep budget は ~400ms 程度。

    use std::thread;
    use std::time::Duration;

    use daw_ui_platform::{Modifiers, PhysicalSize};
    use daw_ui_renderer::{Rect, Scene};

    use super::*;
    use crate::FrameInput;
    use crate::input::PointerFrame;
    use crate::ui::UiHost;

    /// 単純な値 1 個の Model。
    struct VolModel {
        value: f32,
    }

    /// テスト用 fader の rect。
    fn fader_rect() -> Rect {
        Rect { x: 0.0, y: 0.0, w: 32.0, h: 120.0 }
    }

    /// 与えた value での thumb 中心座標。`fader_geometry` と同じ計算。
    fn thumb_center_at(value: f32) -> (f32, f32) {
        let rect = fader_rect();
        let (_, thumb) = fader_geometry(rect, value.clamp(0.0, 1.0));
        (thumb.x + thumb.w * 0.5, thumb.y + thumb.h * 0.5)
    }

    fn run_frame(
        host: &mut UiHost<VolModel>,
        model: &VolModel,
        rect: Rect,
        value: f32,
        default_value: f32,
        pointer: PointerFrame,
    ) -> Vec<Edit<VolModel>> {
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 200 };
        host.frame_to_edits(
            model,
            &mut scene,
            screen,
            FrameInput { pointer, ..Default::default() },
            |_, ui| {
                ui.fader_at("test", rect, value, default_value, None, "fader", |v| {
                    Edit::mutate(move |m: &mut VolModel| m.value = v)
                });
            },
        )
    }

    fn press_at(pos: (f32, f32), ctrl: bool) -> PointerFrame {
        PointerFrame {
            pos: Some(pos),
            primary_just_pressed: true,
            primary_pressed: true,
            modifiers: Modifiers { ctrl, ..Modifiers::default() },
            ..PointerFrame::default()
        }
    }

    fn hold_at(pos: (f32, f32), ctrl: bool) -> PointerFrame {
        PointerFrame {
            pos: Some(pos),
            primary_pressed: true,
            modifiers: Modifiers { ctrl, ..Modifiers::default() },
            ..PointerFrame::default()
        }
    }

    fn release_at(pos: (f32, f32)) -> PointerFrame {
        PointerFrame {
            pos: Some(pos),
            primary_just_released: true,
            ..PointerFrame::default()
        }
    }

    /// ダブルクリック (同位置 / ~50ms 内) で default_value に戻る。
    #[test]
    fn double_click_within_threshold_resets_to_default() {
        let mut host: UiHost<VolModel> = UiHost::no_redraw();
        let mut model = VolModel { value: 0.7 };
        let rect = fader_rect();
        let thumb = thumb_center_at(model.value);

        // Frame 1: 1 回目 press
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            press_at(thumb, false));
        for e in edits { e.apply(&mut model); }

        // Release
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            release_at(thumb));
        for e in edits { e.apply(&mut model); }

        thread::sleep(Duration::from_millis(50));

        // Frame 2: 2 回目 press (同位置、threshold 内) → reset で 0.25
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            press_at(thumb, false));
        for e in edits { e.apply(&mut model); }

        assert!(
            (model.value - 0.25).abs() < 1e-5,
            "ダブルクリックで default_value=0.25 にリセットされるべき (got {})",
            model.value
        );
    }

    /// しきい値超過 (~350ms 後) の 2 回目 press は drag 扱いで、リセットは起きない。
    #[test]
    fn click_after_threshold_does_not_reset() {
        let mut host: UiHost<VolModel> = UiHost::no_redraw();
        let mut model = VolModel { value: 0.7 };
        let rect = fader_rect();
        let thumb = thumb_center_at(model.value);

        // Frame 1
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            press_at(thumb, false));
        for e in edits { e.apply(&mut model); }
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            release_at(thumb));
        for e in edits { e.apply(&mut model); }

        thread::sleep(Duration::from_millis(350));

        // Frame 2
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            press_at(thumb, false));
        for e in edits { e.apply(&mut model); }

        assert!(
            (model.value - 0.7).abs() < 1e-5,
            "閾値超過の 2 回目 press はリセットを起こさない (got {})",
            model.value
        );
    }

    /// 同時間内でも 10px 以上離れていれば drag 扱いで、リセットは起きない。
    #[test]
    fn click_far_position_does_not_reset() {
        let mut host: UiHost<VolModel> = UiHost::no_redraw();
        let mut model = VolModel { value: 0.7 };
        let rect = fader_rect();
        let thumb = thumb_center_at(model.value);

        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            press_at(thumb, false));
        for e in edits { e.apply(&mut model); }
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            release_at(thumb));
        for e in edits { e.apply(&mut model); }

        thread::sleep(Duration::from_millis(50));

        // 2 回目 press は (thumb.x, thumb.y - 10) — 10px 上の thumb 内座標。
        // 注: value=0.7 なので thumb は中央より上、thumb 内で 10px 離れた位置を取れる。
        let far = (thumb.0, thumb.1 - 10.0);
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            press_at(far, false));
        for e in edits { e.apply(&mut model); }

        assert!(
            (model.value - 0.7).abs() < 1e-5,
            "10px 離れた 2 回目 press はリセットを起こさない (got {})",
            model.value
        );
    }

    /// Ctrl + drag は通常 drag の 1/10 の感度。
    #[test]
    fn ctrl_drag_uses_one_tenth_sensitivity() {
        // 通常 drag の値変化を測定
        let mut host_n: UiHost<VolModel> = UiHost::no_redraw();
        let mut model_n = VolModel { value: 0.5 };
        let rect = fader_rect();
        let thumb = thumb_center_at(model_n.value);

        let edits = run_frame(&mut host_n, &model_n, rect, model_n.value, 0.0,
            press_at(thumb, false));
        for e in edits { e.apply(&mut model_n); }
        // 20px 上にドラッグ
        let edits = run_frame(&mut host_n, &model_n, rect, model_n.value, 0.0,
            hold_at((thumb.0, thumb.1 - 20.0), false));
        for e in edits { e.apply(&mut model_n); }
        let normal_delta = model_n.value - 0.5;

        // Ctrl + drag の値変化を測定
        let mut host_c: UiHost<VolModel> = UiHost::no_redraw();
        let mut model_c = VolModel { value: 0.5 };
        let edits = run_frame(&mut host_c, &model_c, rect, model_c.value, 0.0,
            press_at(thumb, true));
        for e in edits { e.apply(&mut model_c); }
        let edits = run_frame(&mut host_c, &model_c, rect, model_c.value, 0.0,
            hold_at((thumb.0, thumb.1 - 20.0), true));
        for e in edits { e.apply(&mut model_c); }
        let fine_delta = model_c.value - 0.5;

        assert!(normal_delta > 0.0, "通常 drag は値が上がる (got {normal_delta})");
        assert!(fine_delta > 0.0, "Ctrl+drag は値が上がる (got {fine_delta})");
        // fine ≈ normal * 0.1
        let ratio = fine_delta / normal_delta;
        assert!(
            (ratio - 0.1).abs() < 1e-3,
            "Ctrl+drag は 1/10 感度 (ratio={ratio}, normal={normal_delta}, fine={fine_delta})",
        );
    }

    /// Mid-drag で Ctrl を on にしても、値が jump せずなめらかに継続する。
    /// 通常 drag で 20px 動かした後 Ctrl を押し、さらに 20px 動かす:
    /// - 再 anchor が無いと 40px 全体に 0.1 が掛かって値が縮む (jump)
    /// - 再 anchor 有りなら 20px 通常 + 20px ×0.1 で「jump 無し + 終端値が中間」になる
    #[test]
    fn mid_drag_ctrl_toggle_does_not_jump() {
        let mut host: UiHost<VolModel> = UiHost::no_redraw();
        let mut model = VolModel { value: 0.5 };
        let rect = fader_rect();
        let thumb = thumb_center_at(model.value);

        // 1. 通常 press
        let edits = run_frame(&mut host, &model, rect, model.value, 0.0,
            press_at(thumb, false));
        for e in edits { e.apply(&mut model); }

        // 2. 20px 上に drag (通常)
        let edits = run_frame(&mut host, &model, rect, model.value, 0.0,
            hold_at((thumb.0, thumb.1 - 20.0), false));
        for e in edits { e.apply(&mut model); }
        let after_normal = model.value;

        // 3. その位置で Ctrl 押下 (まだ動かない) → 再 anchor 発火、値は同じ
        let edits = run_frame(&mut host, &model, rect, model.value, 0.0,
            hold_at((thumb.0, thumb.1 - 20.0), true));
        for e in edits { e.apply(&mut model); }
        assert!(
            (model.value - after_normal).abs() < 1e-5,
            "Ctrl 押下のみ (動きゼロ) で値が変わらない (before={}, after={})",
            after_normal, model.value,
        );

        // 4. さらに 20px 上に drag (Ctrl 中) → fine スケール 0.1 で増える
        let edits = run_frame(&mut host, &model, rect, model.value, 0.0,
            hold_at((thumb.0, thumb.1 - 40.0), true));
        for e in edits { e.apply(&mut model); }
        let after_fine = model.value;

        // 期待: after_fine - after_normal ≈ (20px 分の通常 delta) * 0.1
        // 一方 after_normal - 0.5 ≈ (20px 分の通常 delta)
        // よって after_fine ≈ after_normal + (after_normal - 0.5) * 0.1
        let expected = after_normal + (after_normal - 0.5) * 0.1;
        assert!(
            (after_fine - expected).abs() < 1e-4,
            "再 anchor + 1/10 感度: expected={expected}, got={after_fine}",
        );
    }

    /// 3 連クリックの 3 回目はリセットを再度トリガーせず、新しい drag を開始する。
    /// 検証: 3 回目クリック後の hold-move で値が動く (drag が active になっている) ことを確認。
    /// もしリセットの再トリガーや last_click の不適切な保持があれば、3 回目の press で
    /// drag_anchor が None のままになり、続く move で値変化が起きない (はず)。
    #[test]
    fn triple_click_does_not_reset_again() {
        let mut host: UiHost<VolModel> = UiHost::no_redraw();
        let mut model = VolModel { value: 0.7 };
        let rect = fader_rect();
        let thumb_07 = thumb_center_at(0.7);

        // 1 回目 press + release
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            press_at(thumb_07, false));
        for e in edits { e.apply(&mut model); }
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            release_at(thumb_07));
        for e in edits { e.apply(&mut model); }

        thread::sleep(Duration::from_millis(50));

        // 2 回目 press + release: リセット発火 → value = 0.25
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            press_at(thumb_07, false));
        for e in edits { e.apply(&mut model); }
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            release_at(thumb_07));
        for e in edits { e.apply(&mut model); }
        assert!((model.value - 0.25).abs() < 1e-5, "2 回目でリセット成立");

        thread::sleep(Duration::from_millis(50));

        // 3 回目 press: thumb は value=0.25 の位置に移動済みなのでそこを press。
        // last_click は 2 回目で None にされたので、is_double=false → drag 開始。
        let thumb_025 = thumb_center_at(0.25);
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            press_at(thumb_025, false));
        for e in edits { e.apply(&mut model); }

        // 3 回目 hold-move (20px 上): drag が active なら値増加。
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            hold_at((thumb_025.0, thumb_025.1 - 20.0), false));
        for e in edits { e.apply(&mut model); }

        assert!(
            model.value > 0.25 + 1e-3,
            "3 回目 click は drag を開始する (move で値が増えるはず): value={}",
            model.value
        );
    }
}
