//! `scrubable_number` ウィジェット — drag-to-edit な数値入力。
//!
//! Phase 64a (daw_01 #034): BPM / TimeSig num 等の transport 数値表示で「数値そのものを
//! mouse press + 縦 drag で連続変化」 + 「single-click で text input mode」 + 「Ctrl で fine drag」
//! + 「dblclick で default reset」 という DAW 慣習を実装する widget。
//!
//! 既存 `knob_at` (= 円形 knob で drag scrub) と `text_input_at` (= keyboard 入力) を組み合わせた
//! 上位 idiom。 `text_input_at_focused` を **内部 delegate** することで IME / clipboard / 選択 /
//! Esc rollback は全部既存実装に乗せ、 scrubable 側は state machine + drag 値計算 + format parse のみ。
//!
//! 操作 binding (Phase 64a confirmed by daw_01 #034):
//! - press + 縦 drag (>= 4px) → scrub 開始 (`dragging = true`、 per-frame `on_change(new)`)
//! - Ctrl + drag → sensitivity × 0.1 (fine、 knob/fader と同 idiom)
//! - dblclick (300ms / 5px 以内) → `default_value` リセット + `on_change(default)`
//! - press → 4px 未満で release → text input mode (`editing_text = true`)、 内部 `text_input_at_focused`
//!   が IME / 選択 / Esc rollback / Enter commit を担う
//! - text input mode Enter → committed_text を `format` で parse + range clamp + `on_change(parsed)`
//! - text input mode Esc / focus loss → 静かに rollback (= 元 value 表示に戻る)

use std::hash::Hash;
use std::time::Instant;

use daw_ui_renderer::{Color, GlyphArea, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::scenegraph::hash_inputs;
use crate::ui::{Ui, hovered};

/// ダブルクリック判定の時間しきい値 (ms)。 knob/fader と統一。
const DOUBLE_CLICK_MS: u128 = 300;
/// ダブルクリック判定の位置しきい値 (px)。 knob/fader と統一。
const DOUBLE_CLICK_PX: f32 = 5.0;
/// drag → text edit 切替の閾値 (px)。 4px 未満の release は短 click 扱いで text input mode に入る。
const DRAG_THRESHOLD_PX: f32 = 4.0;
/// Ctrl + drag の fine sensitivity 倍率。 knob/fader と統一。
const FINE_DRAG_SCALE: f32 = 0.1;

/// 数値の表示書式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrubableNumberFormat {
    /// 整数表示 (例: BPM の int 部、 TimeSig num)。 `value.round() as i64` で表示 / parse。
    Integer,
    /// 小数 N 桁 (例: `Decimal(1)` で `"120.0"`、 `Decimal(3)` で `"120.345"`)。
    Decimal(u8),
}

/// `scrubable_number_at` のスタイル + sensitivity + range。
#[derive(Debug, Clone, Copy)]
pub struct ScrubableNumberStyle {
    /// 通常時の rect 塗り色。
    pub bg_color: Color,
    /// hover 時の rect 塗り色 (subtle に切替)。
    pub bg_color_hovered: Color,
    /// drag scrub 中の rect 塗り色 (= scrub 中であることを visual 強調)。
    pub bg_color_dragging: Color,
    /// 数値テキストの色。
    pub text_color: Color,
    /// rect 枠線色。
    pub border: Color,
    /// rect 枠線太さ (px)。
    pub border_width: f32,
    /// rect 角丸 (px)。
    pub radius: f32,
    /// 数値テキストの font size (px)。
    pub font_size: f32,
    /// scrub sensitivity: **`units_per_pixel`** (daw_01 #035 Q1 = (B) 確定)。
    /// 例: BPM 入力で `sensitivity = 0.5` なら `1 px drag = 0.5 BPM 変化`。 Ctrl 押下時は
    /// この値 × `FINE_DRAG_SCALE` (= 0.1) で 10 倍精細に。 `range` の有無に依存しない absolute scale。
    pub sensitivity: f32,
    /// Optional 値範囲 (clamp 用、 widget が `on_change` 呼び出し前に clamp する)。 `None` で
    /// clamp 無し (= caller 責任で on_change 受信側 / parse 時に clamp)。 daw_01 #035 Q3 = yes 確定。
    pub range: Option<(f64, f64)>,
}

impl Default for ScrubableNumberStyle {
    fn default() -> Self {
        Self {
            bg_color: Color::rgb(0.10, 0.11, 0.13),
            bg_color_hovered: Color::rgb(0.13, 0.14, 0.17),
            bg_color_dragging: Color::rgb(0.20, 0.30, 0.42),
            text_color: Color::rgb(0.92, 0.92, 0.94),
            border: Color::rgb(0.30, 0.33, 0.39),
            border_width: 1.0,
            radius: 3.0,
            font_size: 14.0,
            sensitivity: 0.5,
            range: None,
        }
    }
}

/// `scrubable_number_at` の戻り値。
///
/// `bool` field を 3 つ持つが、 各々 (hovered / dragging / editing_text / committed) は
/// **意味的に独立** な observability であり、 state machine 化すると caller の if 文が増えて
/// boilerplate になる (= response struct は「外部から見える観測可能 flag の bag」 という慣習)。
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default)]
pub struct ScrubableNumberResponse {
    /// 描画された値 (= drag 中の preview、 idle 時は caller value、 reset frame は default_value)。
    pub displayed_value: f64,
    /// rect 上に cursor が乗っているか。
    pub hovered: bool,
    /// drag scrub 中 (= press → 4px 以上動いた状態 → release まで true)。 edge 検出で
    /// caller が `ParamGestureBegin/End` を発火する。
    pub dragging: bool,
    /// text input mode に入っているか (= キーボード入力受付中、 cursor 表示)。
    pub editing_text: bool,
    /// 文字入力 commit (Enter or NumpadEnter) の瞬間 true、 1 frame のみ。
    /// `edit_text` を caller が parse する代わりに、 widget が format-aware で parse 済の値を
    /// `on_change(parsed)` で発火する idiom (= daw_01 #035 Q3 確定の「widget は edit_text の parse
    /// をしない」 とは別解釈: widget の `format` を SSoT として parse する方が caller boilerplate ゼロ。
    /// caller が独自 parse したい場合は `edit_text` を読んで自前で push_edit すれば良い)。
    pub committed: bool,
    /// editing_text == true のときの現在のテキストバッファ。 caller の参照用 (= widget は
    /// `format` で parse 済の `on_change(f64)` を発火するため、 通常 caller は読まなくて良い)。
    pub edit_text: Option<String>,
}

/// scrubable_number の永続状態 (フレーム間で保持)。
#[derive(Debug, Default)]
pub(crate) struct ScrubableNumberState {
    /// drag anchor (press 時の `(pointer_y, value, ctrl)`)。 `Some` で press 中 (= drag or short-click 判定待ち)。
    drag_anchor: Option<DragAnchor>,
    /// drag 累積距離 (px、 abs)。 release 時に DRAG_THRESHOLD_PX 未満なら short-click → editing。
    drag_distance_y: f32,
    /// 直近のクリック (ダブルクリック判定用)。
    last_click: Option<ClickRecord>,
    /// drag 開始時の値 (release frame で undoable Edit の inverse に使う、 knob/fader と同 idiom)。
    drag_initial_value: Option<f64>,
    /// text input mode に入っているか (= editing_text)。 release で `drag_distance_y < DRAG_THRESHOLD_PX`
    /// のとき true へ遷移、 inner text_input が focus loss / commit で false へ戻る。
    editing: bool,
}

#[derive(Debug, Clone, Copy)]
struct DragAnchor {
    pointer_y: f32,
    value: f64,
    /// 押下時の Ctrl 状態。 mid-drag で Ctrl toggle 時に再 anchor する判定用 (knob/fader と同 idiom)。
    ctrl: bool,
}

#[derive(Debug, Clone, Copy)]
struct ClickRecord {
    when: Instant,
    pos: (f32, f32),
}

/// 値を `format` に従って文字列化する。
fn format_value(value: f64, format: ScrubableNumberFormat) -> String {
    match format {
        ScrubableNumberFormat::Integer => {
            // round to nearest int (= 120.6 → 121)。 i64 cast は range 内前提だが NaN/Inf 防御も。
            if value.is_finite() {
                #[allow(clippy::cast_possible_truncation)]
                let v_i = value.round() as i64;
                v_i.to_string()
            } else {
                "0".to_string()
            }
        }
        ScrubableNumberFormat::Decimal(n) => {
            // `n` は表示桁数 (例: 1 で "120.0")。
            format!("{:.*}", usize::from(n), value)
        }
    }
}

/// 文字列を `format` に従って parse する (失敗で `None`)。
fn parse_value(text: &str, format: ScrubableNumberFormat) -> Option<f64> {
    let trimmed = text.trim();
    match format {
        ScrubableNumberFormat::Integer => trimmed.parse::<i64>().ok().map(|v| v as f64),
        ScrubableNumberFormat::Decimal(_) => trimmed.parse::<f64>().ok(),
    }
}

impl<M: ?Sized + 'static> Ui<'_, M> {
    /// 矩形指定で drag-to-edit な数値入力を描画 + 処理。
    ///
    /// 値変化時 (= drag scrub / dblclick reset / text commit) に `on_change(new_value)` を 1 度発火する。
    /// drag 中は per-frame 連続発火 (daw_01 #035 Q2 = (A) 確定)、 release で最終値も発火。
    ///
    /// `value`: 表示中の plain 値 (f64 で精度確保)。
    /// `default_value`: dblclick リセット時の値。 `style.range` の clamp は widget 側で実施。
    /// `format`: 表示書式 (Integer / Decimal(N))。 text input mode の parse もこれを SSoT に。
    /// `style`: 色 / sensitivity (units_per_pixel) / 任意 range など。
    /// `label`: drag scrub の history label (= Ctrl+Z で表示される undo 単位名)。 dblclick reset /
    ///   text commit も同 label を共有 (drag 中含む user 操作はすべて 1 undo step に集約)。
    /// `on_change`: 値変化時の Edit を作る closure (knob_at と同形、 `Edit::Mutate` 限定で渡すこと)。
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub fn scrubable_number_at<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        value: f64,
        default_value: f64,
        format: ScrubableNumberFormat,
        style: &ScrubableNumberStyle,
        label: &'static str,
        on_change: F,
    ) -> ScrubableNumberResponse
    where
        F: Fn(f64) -> Edit<M> + Clone + Send + Sync + 'static,
    {
        let wid = WidgetId::ROOT.child((b"scrubable_number", &id));
        // 内部 `text_input_at_focused` の inner widget id を construct するため、 outer id を
        // 1 度 hash 化して u64 seed として保持。 `id: impl Hash + Clone` の `Clone` 要求を回避
        // (= 既存 widget の `impl Hash` のみと API 統一)、 hash 衝突は WidgetId のドメインで
        // unique 化された outer id に紐つくため実用上 zero。
        let id_seed: u64 = hash_inputs((b"scrubable_number_id_seed", &id));
        let pointer = self.pointer;
        let inside = pointer.pos.is_some_and(|(px, py)| rect.contains(px, py));

        // ---- press / drag / release 処理 (knob と同 pattern + drag distance 計測) ----
        let mut reset_fired = false;
        let mut release_initial_value: Option<f64> = None;
        let mut short_click_release = false;
        let (drag_anchor, drag_distance_y, was_editing) = {
            let state: &mut ScrubableNumberState = self.widget_state(wid);

            if pointer.primary_just_pressed
                && let Some((px, py)) = pointer.pos
                && inside
            {
                let now = Instant::now();
                let is_double = state.last_click.is_some_and(|c| {
                    now.duration_since(c.when).as_millis() < DOUBLE_CLICK_MS
                        && (c.pos.0 - px).hypot(c.pos.1 - py) < DOUBLE_CLICK_PX
                });

                if is_double {
                    // dblclick → default reset (= editing も解除)。
                    state.last_click = None;
                    state.drag_anchor = None;
                    state.drag_initial_value = None;
                    state.drag_distance_y = 0.0;
                    state.editing = false;
                    reset_fired = true;
                } else {
                    state.last_click = Some(ClickRecord { when: now, pos: (px, py) });
                    state.drag_anchor = Some(DragAnchor {
                        pointer_y: py,
                        value,
                        ctrl: pointer.modifiers.ctrl,
                    });
                    state.drag_initial_value = Some(value);
                    state.drag_distance_y = 0.0;
                    // press 時点で editing なら外す (= 新規 press で text input 終了)。
                    // ただし inner text_input の focus は別経路で残るので、 ここでは editing flag のみ。
                    state.editing = false;
                }
            }

            // mid-drag で Ctrl toggle されたら anchor 再設定 (= 値 jump 回避、 knob/fader と同 idiom)。
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

            // drag 距離計測 (= drag_anchor が Some の間、 abs(py - anchor.py) の最大値を保持)。
            if let (Some(anchor), Some((_, py))) = (state.drag_anchor, pointer.pos) {
                let dist = (py - anchor.pointer_y).abs();
                if dist > state.drag_distance_y {
                    state.drag_distance_y = dist;
                }
            }

            if pointer.primary_just_released {
                release_initial_value = state.drag_initial_value.take();
                let dist = state.drag_distance_y;
                let was_pressed = state.drag_anchor.is_some();
                state.drag_anchor = None;
                state.drag_distance_y = 0.0;
                // short-click (= drag < threshold、 rect 内 release) → text input mode へ遷移。
                if was_pressed && dist < DRAG_THRESHOLD_PX && inside && !reset_fired {
                    state.editing = true;
                    short_click_release = true;
                }
            }

            (state.drag_anchor, state.drag_distance_y, state.editing)
        };

        // ---- 表示値の決定 (reset > drag > value) ----
        let displayed_value = if reset_fired {
            default_value
        } else if let (Some(anchor), Some((_, py))) = (drag_anchor, pointer.pos) {
            let scale = if anchor.ctrl { FINE_DRAG_SCALE } else { 1.0 };
            // 縦 drag: 上方向 (py < anchor_y) で値増加、 下方向で減少 (= DAW 慣習)。
            let dy_px = -(py - anchor.pointer_y);
            let raw_delta = f64::from(dy_px) * f64::from(style.sensitivity) * f64::from(scale);
            let raw = anchor.value + raw_delta;
            if let Some((min, max)) = style.range {
                raw.clamp(min, max)
            } else {
                raw
            }
        } else {
            value
        };

        // ---- on_change 発火 (drag 中 = per-frame、 reset 1 回、 release final、 commit 1 回) ----
        let mut committed = false;
        let mut edit_text: Option<String> = None;
        let dragging_now = drag_anchor.is_some() && drag_distance_y >= DRAG_THRESHOLD_PX;

        // reset: dblclick で default にリセット (1 frame、 Undoable で Ctrl+Z 戻し可)。
        if reset_fired && (default_value - value).abs() > f64::EPSILON {
            let on_change_fwd = on_change.clone();
            let on_change_inv = on_change.clone();
            let start = value;
            let end = default_value;
            self.push_edit(Edit::with_inverse(
                label,
                move |m: &mut M| on_change_fwd(end).apply(m),
                move |m: &mut M| on_change_inv(start).apply(m),
            ));
        }

        // drag 中の per-frame 発火 (= short-click release は除外、 reset は別経路で済、
        // release frame も skip — release frame は下の Undoable wrap で 1 度 commit する)。
        if !short_click_release
            && !reset_fired
            && release_initial_value.is_none()
            && (displayed_value - value).abs() > f64::EPSILON
        {
            self.push_edit(on_change.clone()(displayed_value));
        }

        // release 時の最終値 (= drag scrub 完了)。 fader/knob と同 idiom で Undoable wrap、
        // forward = on_change(end) / inverse = on_change(start) で 1 undo step。
        if let Some(start_value) = release_initial_value
            && (start_value - displayed_value).abs() > f64::EPSILON
            && !short_click_release
        {
            let on_change_fwd = on_change.clone();
            let on_change_inv = on_change.clone();
            let end = displayed_value;
            self.push_edit(Edit::with_inverse(
                label,
                move |m: &mut M| on_change_fwd(end).apply(m),
                move |m: &mut M| on_change_inv(start_value).apply(m),
            ));
        }

        // ---- text input mode (editing) の内蔵 delegate ----
        // `was_editing` が true なら inner `text_input_at_focused` を描画 (= focus 取得 + 全選択)。
        // commit (Enter) で `committed_text` を parse + clamp + `on_change` 発火、 editing 解除。
        // focus loss (Esc / outside click) で editing 解除 (= 静かに rollback)。
        if was_editing {
            let value_str = format_value(value, format);
            // inner text_input の id は outer id を hash 化した seed で unique 化 (= `Clone` 要求回避)。
            let inner_id = ("scrubable_number_inner", id_seed);
            let inner_resp = self.text_input_at_focused(
                inner_id,
                rect,
                &value_str,
                |_new: String| -> Edit<M> {
                    // typing per-frame では Edit 発火しない (= commit でまとめて発火する設計)。
                    Edit::mutate(|_: &mut M| {})
                },
            );

            // commit 時 (Enter) の確定 text を取得して parse + clamp + on_change 発火。
            if inner_resp.committed
                && let Some(text) = &inner_resp.committed_text
                && let Some(parsed) = parse_value(text, format)
            {
                let final_value = if let Some((min, max)) = style.range {
                    parsed.clamp(min, max)
                } else {
                    parsed
                };
                if (final_value - value).abs() > f64::EPSILON {
                    self.push_edit(on_change.clone()(final_value));
                }
                committed = true;
                // editing 終了 (= inner widget は次 frame で見えなくなる、 focus 自動解除は inner 側が
                // 担う想定で、 ここでは scrubable の editing flag だけ false に)。
                let state: &mut ScrubableNumberState = self.widget_state(wid);
                state.editing = false;
            }

            // focus loss 検出 (= Esc / 外 click)。 inner_resp.focused が false なら editing 終了。
            if !inner_resp.focused {
                let state: &mut ScrubableNumberState = self.widget_state(wid);
                state.editing = false;
            }

            edit_text = Some(value_str);
        } else {
            // ---- 通常描画 (= 非 editing): 背景 + 数値テキスト ----
            let bg_fill = if dragging_now {
                style.bg_color_dragging
            } else if hovered(rect, pointer) {
                style.bg_color_hovered
            } else {
                style.bg_color
            };
            // input_hash で cache: 同じ表示値 / 同じ rect / 同じ bg なら再描画 skip。
            let input_hash = hash_inputs((
                b"scrubable_number",
                rect.x.to_bits(),
                rect.y.to_bits(),
                rect.w.to_bits(),
                rect.h.to_bits(),
                displayed_value.to_bits(),
                dragging_now,
                hovered(rect, pointer),
                style.font_size.to_bits(),
            ));
            let text = format_value(displayed_value, format);
            let style_copy = *style;
            self.with_widget_node(wid, input_hash, |ui| {
                draw_scrubable_number(ui, rect, &text, bg_fill, &style_copy);
            });
        }

        ScrubableNumberResponse {
            displayed_value,
            hovered: hovered(rect, pointer),
            dragging: dragging_now,
            editing_text: was_editing,
            committed,
            edit_text,
        }
    }
}

fn draw_scrubable_number<M: ?Sized + 'static>(
    ui: &mut Ui<'_, M>,
    rect: Rect,
    text: &str,
    bg_fill: Color,
    style: &ScrubableNumberStyle,
) {
    // 背景 (rect 全体)。
    ui.push_rect(RectCommand {
        rect,
        fill: bg_fill,
        border: style.border,
        border_width: style.border_width,
        radius: [style.radius; 4],
        clip_rect: None,
    });

    // 数値テキスト (rect 中央寄せ、 horizontal は left-padded 4px)。
    let pad_x = 4.0;
    let line_h = style.font_size * 1.2;
    let tx = rect.x + pad_x;
    let ty = rect.y + (rect.h - line_h) * 0.5;
    ui.push_text(GlyphArea {
        text: text.into(),
        left: tx,
        top: ty,
        font_size: style.font_size,
        line_height: line_h,
        color: style.text_color,
        clip_rect: Some(rect),
        ..GlyphArea::default()
    });
}

#[cfg(test)]
mod tests {
    use daw_ui_platform::{Modifiers, PhysicalSize};
    use daw_ui_renderer::{Rect, Scene};

    use super::*;
    use crate::FrameInput;
    use crate::input::PointerFrame;
    use crate::ui::UiHost;

    struct BpmModel {
        bpm: f64,
    }

    fn rect_default() -> Rect {
        Rect { x: 0.0, y: 0.0, w: 80.0, h: 28.0 }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_frame(
        host: &mut UiHost<BpmModel>,
        model: &BpmModel,
        rect: Rect,
        value: f64,
        default_value: f64,
        format: ScrubableNumberFormat,
        style: &ScrubableNumberStyle,
        pointer: PointerFrame,
    ) -> Vec<Edit<BpmModel>> {
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        host.frame_to_edits(
            model,
            &mut scene,
            screen,
            FrameInput { pointer, ..Default::default() },
            |_, ui| {
                ui.scrubable_number_at(
                    "test",
                    rect,
                    value,
                    default_value,
                    format,
                    style,
                    "scrub bpm",
                    |v| Edit::mutate(move |m: &mut BpmModel| m.bpm = v),
                );
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

    #[test]
    fn format_value_integer_rounds() {
        assert_eq!(format_value(120.4, ScrubableNumberFormat::Integer), "120");
        assert_eq!(format_value(120.6, ScrubableNumberFormat::Integer), "121");
    }

    #[test]
    fn format_value_decimal_precision() {
        assert_eq!(format_value(120.456, ScrubableNumberFormat::Decimal(1)), "120.5");
        assert_eq!(format_value(120.456, ScrubableNumberFormat::Decimal(3)), "120.456");
    }

    #[test]
    fn parse_value_handles_int_and_decimal() {
        assert_eq!(parse_value("120", ScrubableNumberFormat::Integer), Some(120.0));
        assert_eq!(parse_value("120.5", ScrubableNumberFormat::Decimal(1)), Some(120.5));
        assert_eq!(parse_value("abc", ScrubableNumberFormat::Integer), None);
        assert_eq!(parse_value("  120  ", ScrubableNumberFormat::Integer), Some(120.0));
    }

    /// drag 上方向 (= dy negative) で値が増加、 sensitivity が units_per_pixel として効く。
    #[test]
    fn drag_up_increases_value_by_sensitivity() {
        let mut host: UiHost<BpmModel> = UiHost::no_redraw();
        let mut model = BpmModel { bpm: 120.0 };
        let rect = rect_default();
        let style = ScrubableNumberStyle { sensitivity: 0.5, ..ScrubableNumberStyle::default() };
        let center = (40.0_f32, 14.0_f32);

        // press
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, press_at(center, false),
        );
        for e in edits { e.apply(&mut model); }
        // drag up 20px (dy = -20) → expected 120 + (-(-20)) * 0.5 = 120 + 10 = 130
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style,
            hold_at((center.0, center.1 - 20.0), false),
        );
        for e in edits { e.apply(&mut model); }

        assert!((model.bpm - 130.0).abs() < 1e-5, "drag 上 20px × sensitivity 0.5 = +10 (got {})", model.bpm);
    }

    /// Ctrl + drag で sensitivity が 1/10 (fine) になる。
    #[test]
    fn ctrl_drag_uses_fine_sensitivity() {
        let mut host: UiHost<BpmModel> = UiHost::no_redraw();
        let mut model = BpmModel { bpm: 120.0 };
        let rect = rect_default();
        let style = ScrubableNumberStyle { sensitivity: 0.5, ..ScrubableNumberStyle::default() };
        let center = (40.0_f32, 14.0_f32);

        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, press_at(center, true),
        );
        for e in edits { e.apply(&mut model); }
        // drag up 20px Ctrl → expected 120 + 20 * 0.5 * 0.1 = 120 + 1 = 121
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style,
            hold_at((center.0, center.1 - 20.0), true),
        );
        for e in edits { e.apply(&mut model); }

        assert!((model.bpm - 121.0).abs() < 1e-5, "Ctrl+drag は 1/10 = +1.0 (got {})", model.bpm);
    }

    /// range が Some なら widget が clamp してから on_change 発火 (= caller boilerplate ゼロ)。
    #[test]
    fn range_clamps_drag_result() {
        let mut host: UiHost<BpmModel> = UiHost::no_redraw();
        let mut model = BpmModel { bpm: 200.0 };
        let rect = rect_default();
        let style = ScrubableNumberStyle {
            sensitivity: 10.0,                  // 1 px = 10 BPM
            range: Some((20.0, 240.0)),         // 上限 240
            ..ScrubableNumberStyle::default()
        };
        let center = (40.0_f32, 14.0_f32);

        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Integer, &style, press_at(center, false),
        );
        for e in edits { e.apply(&mut model); }
        // drag up 100px → raw = 200 + 100 * 10 = 1200、 clamp で 240。
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Integer, &style,
            hold_at((center.0, center.1 - 100.0), false),
        );
        for e in edits { e.apply(&mut model); }

        assert!((model.bpm - 240.0).abs() < 1e-5, "range upper で clamp (got {})", model.bpm);
    }

    /// dblclick で default_value に reset + on_change(default) 発火。
    #[test]
    fn double_click_resets_to_default() {
        use std::thread;
        use std::time::Duration;

        let mut host: UiHost<BpmModel> = UiHost::no_redraw();
        let mut model = BpmModel { bpm: 200.0 };
        let rect = rect_default();
        let style = ScrubableNumberStyle::default();
        let center = (40.0_f32, 14.0_f32);

        // 1 回目 click
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, press_at(center, false),
        );
        for e in edits { e.apply(&mut model); }
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, release_at(center),
        );
        for e in edits { e.apply(&mut model); }

        thread::sleep(Duration::from_millis(50));

        // 2 回目 click (= dblclick)
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, press_at(center, false),
        );
        for e in edits { e.apply(&mut model); }

        assert!((model.bpm - 120.0).abs() < 1e-5, "dblclick で default 120.0 にリセット (got {})", model.bpm);
    }
}
