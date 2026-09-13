//! `xy_point` ウィジェット — 矩形の中を 2 軸でドラッグする点 (daw_01 r.md #129)。
//!
//! 値は **矩形内の座標 (px)** そのもので、widget は座標の意味 (周波数 / ゲイン等) を知らない。
//! 写像は caller が持つ (daw_01 の EQ カーブ点は `view::native_device::CurveAxes`)。
//!
//! 契約:
//! - press が当たり円 (`style.hit_radius`) の中なら [`Ui::claim_press`] で所有者を名乗る
//!   (`take_drag_in_rect` は claim しないので使わない)。親の行ドラッグ (`drag_list`) は次フレームで
//!   session を捨てる ([`crate::click`])。
//! - 掴んだ位置と点の中心のずれを保つ (中心へ飛ばない)。Ctrl で感度 1/10 ([`FINE_DRAG_SCALE`])、
//!   ドラッグ中の Ctrl 切り替えは再 anchor して値を跳ねさせない (knob と同じ)。
//! - 押したまま Esc ([`Ui::drag_cancel_requested`]) で press 時の位置へ戻す (knob と同じ契約)。
//! - 動かせない軸 ([`XyAxes`]) は入力の値のまま、動かせる軸は `bounds` に clamp する。
//! - `wheel_enabled` の点は毎フレーム [`Ui::claim_wheel_in_rect`] し、hover 中のホイールを
//!   [`XyPointResponse::wheel`] (notch 単位) で返す。最後のホイールから [`WHEEL_ACTIVE_MS`] の間は
//!   `wheel_active` (caller が「ホイールの一連」を undo 1 step に束ねる窓)。
//! - modal popup の下では press もホイールも取らない。
//! - 値が変わったフレームだけ `on_change(新しい座標)` を積む (位置は caller の model が SSoT)。

use std::hash::Hash;
use std::time::{Duration, Instant};

use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::input::LINE_HEIGHT_PX;
use crate::theme::Palette;
use crate::ui::Ui;

/// Ctrl + ドラッグの感度倍率 (knob / fader と同じ 1/10)。
pub const FINE_DRAG_SCALE: f32 = 0.1;
/// 最後のホイールから `wheel_active` が立っている時間。
pub const WHEEL_ACTIVE_MS: u64 = 400;
/// 値が「変わった」とみなす最小差 (px)。
const MOVE_EPSILON: f32 = 1e-3;

/// 動かせる軸。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XyAxes {
    pub x: bool,
    pub y: bool,
}

impl XyAxes {
    pub const BOTH: Self = Self { x: true, y: true };
    pub const HORIZONTAL: Self = Self { x: true, y: false };
    pub const VERTICAL: Self = Self { x: false, y: true };
}

/// 点の見た目。
#[derive(Clone, Copy, Debug)]
pub struct XyPointStyle {
    /// 描く円の半径 (px)。
    pub radius: f32,
    /// 当たり判定の半径 (px、`radius` より大きくしてよい)。
    pub hit_radius: f32,
    /// 静止時の塗り (`Color::TRANSPARENT` で輪だけ)。
    pub fill: Color,
    /// hover / ドラッグ中の塗り。
    pub fill_active: Color,
    pub border: Color,
    pub border_width: f32,
}

impl XyPointStyle {
    /// パレットの可動ハンドル色で組む (r.md #48、`Default` にしない理由は `ToggleButtonStyle` と同じ)。
    #[must_use]
    pub fn from_palette(p: &Palette) -> Self {
        Self {
            radius: 4.5,
            hit_radius: 8.0,
            fill: p.handle,
            fill_active: p.handle_active,
            border: p.inset_bg,
            border_width: 1.0,
        }
    }
}

/// [`Ui::xy_point_at`] の結果。
#[derive(Clone, Copy, Debug, Default)]
pub struct XyPointResponse {
    /// いま描いた位置 (ドラッグ中は model より 1 フレーム先行)。
    pub position: (f32, f32),
    /// 当たり円の上にカーソルがある。
    pub hovered: bool,
    /// この点を掴んでいる (press からの一連、動かしていなくても `true`)。
    pub dragging: bool,
    /// このフレームのホイール量 (notch 単位、上 = 正)。マウスホイールの 1 行 = 1.0 で、入力層が
    /// 行を px に換算する `input::LINE_HEIGHT_PX` で戻す (トラックパッドは小数になる)。
    pub wheel: f32,
    /// 最後のホイールから [`WHEEL_ACTIVE_MS`] 以内。
    pub wheel_active: bool,
}

#[derive(Clone, Copy, Debug)]
struct Drag {
    /// press 時の点の位置 (Esc で戻す先)。
    start: (f32, f32),
    /// 再 anchor 時点の pointer と点の位置。表示位置 = `anchor_value + (pointer - anchor_pointer) * 感度`。
    anchor_pointer: (f32, f32),
    anchor_value: (f32, f32),
    ctrl: bool,
    /// 直近に表示した位置 (再 anchor の基準)。
    last: (f32, f32),
}

#[derive(Debug, Default)]
struct XyPointState {
    drag: Option<Drag>,
    last_wheel: Option<Instant>,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// `bounds` の中を動く点 (module doc の契約)。`pos` は model の現在位置 (px)。
    #[allow(clippy::too_many_arguments)]
    pub fn xy_point_at<F>(
        &mut self,
        id: impl Hash,
        bounds: Rect,
        pos: (f32, f32),
        axes: XyAxes,
        wheel_enabled: bool,
        style: &XyPointStyle,
        on_change: F,
    ) -> XyPointResponse
    where
        F: Fn((f32, f32)) -> Edit<M>,
    {
        let wid = WidgetId::ROOT.child((b"xy_point", &id));
        let pointer = self.pointer;
        let blocked = self.pointer_blocked_by_modal_popup();
        let r = style.hit_radius.max(style.radius);
        let hit = Rect { x: pos.0 - r, y: pos.1 - r, w: r * 2.0, h: r * 2.0 };
        let on_point = |p: (f32, f32)| (p.0 - pos.0).hypot(p.1 - pos.1) <= r;
        let drag_cancel = self.drag_cancel_requested();

        // ---- press / continue / release / Esc ----
        let mut emit: Option<(f32, f32)> = None;
        let pressed_here = !blocked && pointer.primary_just_pressed && pointer.pos.is_some_and(on_point);
        let (dragging, displayed) = {
            let state: &mut XyPointState = self.widget_state(wid);
            if pressed_here && let Some(p) = pointer.pos {
                state.drag =
                    Some(Drag { start: pos, anchor_pointer: p, anchor_value: pos, ctrl: pointer.modifiers.ctrl, last: pos });
            }
            if drag_cancel && let Some(d) = state.drag.take() {
                emit = Some(d.start);
            }
            let mut displayed = pos;
            if let Some(d) = state.drag.as_mut()
                && let Some(p) = pointer.pos
            {
                if pointer.modifiers.ctrl != d.ctrl {
                    d.anchor_pointer = p;
                    d.anchor_value = d.last;
                    d.ctrl = pointer.modifiers.ctrl;
                }
                let scale = if d.ctrl { FINE_DRAG_SCALE } else { 1.0 };
                let moved = |on: bool, anchor: f32, from: f32, to: f32, cur: f32, lo: f32, hi: f32| {
                    if on { (anchor + (to - from) * scale).clamp(lo, hi.max(lo)) } else { cur }
                };
                displayed = (
                    moved(axes.x, d.anchor_value.0, d.anchor_pointer.0, p.0, pos.0, bounds.x, bounds.x + bounds.w),
                    moved(axes.y, d.anchor_value.1, d.anchor_pointer.1, p.1, pos.1, bounds.y, bounds.y + bounds.h),
                );
                d.last = displayed;
                if (displayed.0 - pos.0).abs() > MOVE_EPSILON || (displayed.1 - pos.1).abs() > MOVE_EPSILON {
                    emit = Some(displayed);
                }
            }
            let dragging = state.drag.is_some();
            if pointer.primary_just_released {
                state.drag = None;
            }
            (dragging, displayed)
        };
        if pressed_here {
            self.claim_press(wid);
        }
        if let Some(p) = emit {
            self.push_edit(on_change(p));
        }

        // ---- wheel ----
        let mut wheel = 0.0;
        let mut wheel_active = false;
        if wheel_enabled {
            self.claim_wheel_in_rect(hit);
            if !blocked && pointer.pos.is_some_and(on_point) {
                wheel = self.take_scroll_in_rect(hit).1 / LINE_HEIGHT_PX;
            }
            let now = Instant::now();
            let state: &mut XyPointState = self.widget_state(wid);
            if wheel != 0.0 {
                state.last_wheel = Some(now);
            }
            wheel_active = state
                .last_wheel
                .is_some_and(|t| now.duration_since(t) < Duration::from_millis(WHEEL_ACTIVE_MS));
            if wheel_active {
                // 窓が閉じるフレームを起こす (入力が無いと次のフレームが来ない)。
                self.request_redraw();
            }
        }

        // ---- draw ----
        let hovered = !blocked && self.hover_pos().is_some_and(on_point);
        let fill = if dragging || hovered { style.fill_active } else { style.fill };
        let rr = style.radius;
        self.push_rect(RectCommand {
            rect: Rect { x: displayed.0 - rr, y: displayed.1 - rr, w: rr * 2.0, h: rr * 2.0 },
            fill,
            border: style.border,
            border_width: style.border_width,
            radius: [rr; 4],
            clip_rect: None,
        });

        XyPointResponse { position: displayed, hovered, dragging, wheel, wheel_active }
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use std::cell::Cell;

    use daw_ui_platform::{Modifiers, PhysicalSize};
    use daw_ui_renderer::{Rect, Scene};

    use super::{XyAxes, XyPointResponse, XyPointStyle};
    use crate::edit::Edit;
    use crate::input::{FrameInput, PointerFrame};
    use crate::theme::Palette;
    use crate::ui::UiHost;
    use crate::widgets::drag_list::{DragListRow, DragListSlot, DragListStyle};

    const BOUNDS: Rect = Rect { x: 0.0, y: 0.0, w: 200.0, h: 100.0 };
    const SCREEN: PhysicalSize = PhysicalSize { width: 400, height: 400 };

    /// model = 点の位置。on_change がそのまま書く。
    type Model = Cell<(f32, f32)>;

    fn frame(pos: (f32, f32), just_pressed: bool, pressed: bool, just_released: bool) -> FrameInput {
        FrameInput {
            pointer: PointerFrame {
                pos: Some(pos),
                primary_just_pressed: just_pressed,
                primary_pressed: pressed,
                primary_just_released: just_released,
                modifiers: Modifiers::default(),
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        }
    }

    fn run(host: &mut UiHost<Model>, model: &mut Model, input: FrameInput, axes: XyAxes) -> XyPointResponse {
        let mut scene = Scene::new();
        let out = Cell::new(XyPointResponse::default());
        host.frame(model, &mut scene, SCREEN, input, |m, ui| {
            let style = XyPointStyle::from_palette(&Palette::dark());
            out.set(ui.xy_point_at("p", BOUNDS, m.get(), axes, true, &style, |p| {
                Edit::mutate(move |m: &mut Model| m.set(p))
            }));
        });
        out.get()
    }

    #[test]
    fn drag_keeps_the_grab_offset_and_locks_axes() {
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut model = Cell::new((100.0, 50.0));
        // 中心から (3, 2) ずれた所を掴んで (+20, +10) 動かすと、点も (+20, +10) (中心へ飛ばない)。
        let r = run(&mut host, &mut model, frame((103.0, 52.0), true, true, false), XyAxes::BOTH);
        assert!(r.dragging);
        assert_eq!(model.get(), (100.0, 50.0), "押しただけでは動かない");
        run(&mut host, &mut model, frame((123.0, 62.0), false, true, false), XyAxes::BOTH);
        assert_eq!(model.get(), (120.0, 60.0));

        // 横だけの点は縦に動かない。bounds で止まる。
        let mut model = Cell::new((100.0, 50.0));
        let mut host: UiHost<Model> = UiHost::no_redraw();
        run(&mut host, &mut model, frame((100.0, 50.0), true, true, false), XyAxes::HORIZONTAL);
        run(&mut host, &mut model, frame((400.0, 90.0), false, true, false), XyAxes::HORIZONTAL);
        assert_eq!(model.get(), (200.0, 50.0), "x は bounds の右端、y は動かない");
    }

    #[test]
    fn escape_while_pressed_restores_the_press_position() {
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut model = Cell::new((100.0, 50.0));
        run(&mut host, &mut model, frame((100.0, 50.0), true, true, false), XyAxes::BOTH);
        run(&mut host, &mut model, frame((150.0, 20.0), false, true, false), XyAxes::BOTH);
        assert_eq!(model.get(), (150.0, 20.0));
        let mut esc = frame((150.0, 20.0), false, true, false);
        esc.keyboard.push(daw_ui_platform::KeyEvent {
            state: daw_ui_platform::ElementState::Pressed,
            text: None,
            physical_key: daw_ui_platform::PhysicalKey::Escape,
            repeat: false,
        });
        let r = run(&mut host, &mut model, esc, XyAxes::BOTH);
        assert_eq!(model.get(), (100.0, 50.0), "Esc で press 時の位置へ戻る");
        assert!(!r.dragging);
        // 以後の移動では動かない。
        run(&mut host, &mut model, frame((190.0, 90.0), false, true, false), XyAxes::BOTH);
        assert_eq!(model.get(), (100.0, 50.0));
    }

    #[test]
    fn claiming_the_press_makes_the_parent_drag_list_drop_its_session() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let rows = [DragListRow { height: 100.0, draggable: true, block_len: 1 }; 2];
        let slots: Vec<DragListSlot> = (0..=2).map(|i| DragListSlot { after_row: i, indent: 0.0 }).collect();
        let style = DragListStyle { row_gap: 0.0, drop_indicator_color: daw_ui_renderer::Color::WHITE, drop_indicator_h: 2.0 };
        let mut step = |input: FrameInput| {
            let mut scene = Scene::new();
            let out = Cell::new(None);
            let mut model = ();
            host.frame(&mut model, &mut scene, SCREEN, input, |(), ui| {
                let resp = ui.drag_list("rows", Rect { x: 0.0, y: 0.0, w: 200.0, h: 200.0 }, &rows, &slots, None, &style, |_, _| true, |ui, i, r, _, _| {
                    if i == 0 {
                        let s = XyPointStyle::from_palette(&Palette::dark());
                        ui.xy_point_at("p", r, (50.0, 50.0), XyAxes::BOTH, false, &s, |_| Edit::mutate(|(): &mut ()| {}));
                    }
                });
                out.set(Some((resp.dragging, resp.dropped)));
            });
            out.get().expect("frame ran")
        };
        // 行 0 の中の点を掴んで、行ドラッグの閾値を超えて下へ動かし、行 1 の下で離す。
        step(frame((50.0, 50.0), true, true, false));
        let (dragging, _) = step(frame((50.0, 180.0), false, true, false));
        assert_eq!(dragging, None, "点が press を名乗ったので行はドラッグにならない");
        let (_, dropped) = step(frame((50.0, 190.0), false, false, true));
        assert_eq!(dropped, None);
    }

    #[test]
    fn wheel_is_returned_only_over_the_point_and_stays_active() {
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut model = Cell::new((100.0, 50.0));
        let mut away = frame((10.0, 10.0), false, false, false);
        away.pointer.scroll_delta = (0.0, 40.0);
        let r = run(&mut host, &mut model, away, XyAxes::BOTH);
        assert_eq!(r.wheel, 0.0, "hover していないホイールは返さない");
        assert!(!r.wheel_active);
        let mut over = frame((101.0, 51.0), false, false, false);
        over.pointer.scroll_delta = (0.0, 80.0);
        let r = run(&mut host, &mut model, over, XyAxes::BOTH);
        assert_eq!(r.wheel, 2.0, "2 行 = 2 notch");
        assert!(r.wheel_active);
        let r = run(&mut host, &mut model, frame((101.0, 51.0), false, false, false), XyAxes::BOTH);
        assert_eq!(r.wheel, 0.0);
        assert!(r.wheel_active, "窓の間は active のまま");
    }

    #[test]
    fn nothing_is_taken_under_a_modal_popup() {
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut model = Cell::new((100.0, 50.0));
        let mut scene = Scene::new();
        host.frame(&mut model, &mut scene, SCREEN, FrameInput::default(), |_, ui| {
            ui.open_popup("modal", Rect { x: 0.0, y: 0.0, w: 400.0, h: 400.0 }, true);
        });
        let mut press = frame((100.0, 50.0), true, true, false);
        press.pointer.scroll_delta = (0.0, 40.0);
        let r = run(&mut host, &mut model, press, XyAxes::BOTH);
        assert!(!r.dragging, "modal の下では press を取らない");
        assert_eq!(r.wheel, 0.0);
        run(&mut host, &mut model, frame((150.0, 80.0), false, true, false), XyAxes::BOTH);
        assert_eq!(model.get(), (100.0, 50.0));
    }
}
