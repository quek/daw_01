//! 当たり判定の clip — 描画を切り取った領域の **外** では、中身が操作を始められないようにする。
//!
//! [`Ui::with_clip_rect`] が切るのは描画だけで、widget の当たり判定は自分の rect をそのまま使う。
//! スクロールで viewport の外へ出た widget は見えないのに押せてしまい、viewport の外にある別の
//! もの (窓のタイトルバー / 窓の外の背景) を押したつもりの click が、隠れた widget にも届く。
//! [`crate::widgets::scroll_area`] は中身を [`Ui::with_hit_clip`] で包んでこれを止める。
//!
//! pointer が領域の外にある間、中身が読む入力は次のとおり:
//!
//! - **ボタンを押していない** — pointer は「居ない」(pos 無し)。hover も新しい press もホイールも起きない。
//! - **押したままドラッグ中 / 離したフレーム** — 位置と release はそのまま通す (中で始めたドラッグは
//!   領域の外へ出ても続き、外で離して終われる)。新しい press・右 press・ホイールだけ落とす。
//! - 保留中のダブルクリック / 右クリックは、その位置が領域の外なら見せない。
//!
//! 中身が開いた popup (dropdown の一覧など) は領域の外へはみ出して描かれるので、
//! [`Ui::popup_layer`] の間だけ clip 前の入力へ戻す ([`Ui::suspend_hit_clip`])。
//!
//! 消費 (click の consume / ホイールの取り出し / 保留 click の取り出し) は clip 前の入力へ写し戻す。
//! 中身が使った入力が、領域を出たあとの widget にもう一度届くことはない。

use daw_ui_renderer::Rect;

use crate::input::PointerFrame;
use crate::ui::Ui;

/// 中身が読める入力一式 (pointer と、フレーム頭に生 pointer から決めた保留 click)。
#[derive(Debug, Clone, Copy)]
struct Inputs {
    pointer: PointerFrame,
    secondary_click: Option<(f32, f32)>,
    double_click: Option<(f32, f32)>,
    double_click_press: Option<(f32, f32)>,
}

/// 有効な当たり判定の clip (入れ子は交差させて 1 つにまとめる)。
#[derive(Debug, Clone, Copy)]
pub(crate) struct HitClip {
    rect: Rect,
    /// clip 前の入力。
    real: Inputs,
}

/// [`Ui::suspend_hit_clip`] が返す、[`Ui::resume_hit_clip`] へ渡す中断中の clip。
#[derive(Debug)]
pub(crate) struct SuspendedHitClip(Option<HitClip>);

/// `real` を `rect` で clip した、中身に見せる入力。
fn clipped(real: Inputs, rect: Rect) -> Inputs {
    let inside = |pos: (f32, f32)| rect.contains(pos.0, pos.1);
    let p = real.pointer;
    let pointer = match p.pos {
        Some(pos) if !inside(pos) => {
            let dragging = (p.primary_pressed && !p.primary_just_pressed) || p.primary_just_released;
            PointerFrame {
                pos: dragging.then_some(pos),
                primary_just_pressed: false,
                primary_just_released: dragging && p.primary_just_released,
                primary_pressed: dragging && p.primary_pressed,
                secondary_just_pressed: false,
                // 右ボタンのドラッグは「押している」を持たないので、離した合図だけは常に通す
                // (中で始めた右ドラッグが終われなくならないように。位置が無ければ click にはならない)。
                secondary_just_released: p.secondary_just_released,
                modifiers: p.modifiers,
                scroll_delta: (0.0, 0.0),
            }
        }
        _ => p,
    };
    Inputs {
        pointer,
        secondary_click: real.secondary_click.filter(|&pos| inside(pos)),
        double_click: real.double_click.filter(|&pos| inside(pos)),
        double_click_press: real.double_click_press.filter(|&pos| inside(pos)),
    }
}

/// 中身に見せた `shown` が `now` へ減っていたら (= 中身が消費した)、その分を `real` からも消す。
fn write_back(real: &mut Inputs, shown: Inputs, now: Inputs) {
    fn edge(real: &mut bool, shown: bool, now: bool) {
        if shown && !now {
            *real = false;
        }
    }
    fn slot(real: &mut Option<(f32, f32)>, shown: Option<(f32, f32)>, now: Option<(f32, f32)>) {
        if shown.is_some() && now.is_none() {
            *real = None;
        }
    }
    let (rp, sp, np) = (&mut real.pointer, shown.pointer, now.pointer);
    edge(&mut rp.primary_just_pressed, sp.primary_just_pressed, np.primary_just_pressed);
    edge(&mut rp.primary_just_released, sp.primary_just_released, np.primary_just_released);
    edge(&mut rp.secondary_just_pressed, sp.secondary_just_pressed, np.secondary_just_pressed);
    edge(&mut rp.secondary_just_released, sp.secondary_just_released, np.secondary_just_released);
    if sp.scroll_delta.0 != 0.0 && np.scroll_delta.0 == 0.0 {
        rp.scroll_delta.0 = 0.0;
    }
    if sp.scroll_delta.1 != 0.0 && np.scroll_delta.1 == 0.0 {
        rp.scroll_delta.1 = 0.0;
    }
    slot(&mut real.secondary_click, shown.secondary_click, now.secondary_click);
    slot(&mut real.double_click, shown.double_click, now.double_click);
    slot(&mut real.double_click_press, shown.double_click_press, now.double_click_press);
}

impl<M: ?Sized + 'static> Ui<'_, M> {
    /// `f` の中の widget は、pointer が `rect` の外にある間は操作を始められない (module doc)。
    /// 描画は切らないので [`Ui::with_clip_rect`] と組で使う。入れ子にすると交差が効く。
    pub fn with_hit_clip<R>(&mut self, rect: Rect, f: impl FnOnce(&mut Self) -> R) -> R {
        let outer = self.hit_clip_sync();
        let (rect, real) = match outer {
            Some(o) => (o.rect.intersect(rect), o.real),
            None => (rect, self.visible_inputs()),
        };
        self.hit_clip = Some(HitClip { rect, real });
        self.show_inputs(clipped(real, rect));
        let r = f(self);
        // 中身の消費を clip 前の入力へ写し戻し、外側の clip (無ければ clip 無し) へ戻す。
        let real = self.hit_clip_sync().map_or(real, |c| c.real);
        self.hit_clip = outer.map(|o| HitClip { real, ..o });
        self.show_inputs(self.hit_clip.map_or(real, |c| clipped(c.real, c.rect)));
        r
    }

    /// 有効な clip を外して clip 前の入力へ戻す ([`Ui::popup_layer`] の入口)。
    pub(crate) fn suspend_hit_clip(&mut self) -> SuspendedHitClip {
        let clip = self.hit_clip_sync();
        self.hit_clip = None;
        if let Some(c) = clip {
            self.show_inputs(c.real);
        }
        SuspendedHitClip(clip)
    }

    /// [`Self::suspend_hit_clip`] で外した clip を戻す。外していた間の消費は clip 前の入力に残る。
    pub(crate) fn resume_hit_clip(&mut self, suspended: SuspendedHitClip) {
        if let Some(c) = suspended.0 {
            let real = self.visible_inputs();
            self.hit_clip = Some(HitClip { real, ..c });
            self.show_inputs(clipped(real, c.rect));
        }
    }

    /// いま widget が読める入力。
    fn visible_inputs(&self) -> Inputs {
        Inputs {
            pointer: self.pointer,
            secondary_click: *self.pending_secondary_click,
            double_click: *self.pending_double_click,
            double_click_press: *self.pending_double_click_press,
        }
    }

    fn show_inputs(&mut self, inputs: Inputs) {
        self.pointer = inputs.pointer;
        *self.pending_secondary_click = inputs.secondary_click;
        *self.pending_double_click = inputs.double_click;
        *self.pending_double_click_press = inputs.double_click_press;
    }

    /// 有効な clip の `real` へ、ここまでの消費を写し戻して返す (clip が無ければ `None`)。
    fn hit_clip_sync(&mut self) -> Option<HitClip> {
        let mut clip = self.hit_clip?;
        let shown = clipped(clip.real, clip.rect);
        write_back(&mut clip.real, shown, self.visible_inputs());
        self.hit_clip = Some(clip);
        Some(clip)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use daw_ui_platform::PhysicalSize;
    use daw_ui_renderer::{Rect, Scene};

    use crate::id::WidgetId;
    use crate::input::{FrameInput, PointerFrame};
    use crate::ui::UiHost;

    const SCREEN: PhysicalSize = PhysicalSize { width: 400, height: 400 };
    /// viewport。中身は 1000px あり、下へ 100px スクロールして使う。
    const VIEWPORT: Rect = Rect { x: 0.0, y: 100.0, w: 200.0, h: 100.0 };
    const CONTENT: (f32, f32) = (200.0, 1_000.0);

    fn pointer(pos: (f32, f32), f: impl FnOnce(&mut PointerFrame)) -> FrameInput {
        let mut p = PointerFrame { pos: Some(pos), ..PointerFrame::default() };
        f(&mut p);
        FrameInput { pointer: p, ..FrameInput::default() }
    }

    fn click(pos: (f32, f32)) -> FrameInput {
        pointer(pos, |p| {
            p.primary_just_pressed = true;
            p.primary_just_released = true;
        })
    }

    /// 中身を 100px スクロールした host (中身の y=0..100 が viewport の上へ隠れている)。
    fn scrolled_host() -> UiHost<()> {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let wheel = pointer((50.0, 150.0), |p| p.scroll_delta = (0.0, -100.0));
        host.frame_to_edits(&(), &mut scene, SCREEN, wheel, |(), ui| {
            ui.scroll_area("s", VIEWPORT, CONTENT, |_, _| {});
        });
        host
    }

    /// 中身の先頭 (content y=0..30) に click を数える行を置いた scroll_area を 1 フレーム回す。
    /// 戻り値は `(行が click されたか, scroll_area の後ろの widget が click されたか)`。
    fn run_row(host: &mut UiHost<()>, input: FrameInput) -> (bool, bool) {
        let mut scene = Scene::new();
        let row_clicked = Cell::new(false);
        let after_clicked = Cell::new(false);
        host.frame_to_edits(&(), &mut scene, SCREEN, input, |(), ui| {
            ui.scroll_area("s", VIEWPORT, CONTENT, |ui, off| {
                let row = Rect { x: 0.0, y: VIEWPORT.y - off.1, w: 200.0, h: 30.0 };
                let inside = ui.pointer().pos.is_some_and(|(x, y)| row.contains(x, y));
                if ui.primary_click(WidgetId::ROOT.child(b"row"), inside).clicked {
                    row_clicked.set(true);
                }
            });
            // scroll_area を出たあとの widget は clip 前の pointer を読む。
            let pos = ui.pointer().pos;
            let inside = pos.is_some_and(|(x, y)| y < VIEWPORT.y && x < 200.0);
            if ui.primary_click(WidgetId::ROOT.child(b"after"), inside).clicked {
                after_clicked.set(true);
            }
        });
        (row_clicked.get(), after_clicked.get())
    }

    /// スクロールで viewport の上へ隠れた行は、その位置を click しても反応しない。
    /// click は scroll_area の後ろの widget へそのまま届く。
    #[test]
    fn a_row_scrolled_out_of_the_viewport_is_not_clickable() {
        let mut host = scrolled_host();
        // 行は y=0..30 に居る (viewport は y=100 から)。
        let (row, after) = run_row(&mut host, click((50.0, 10.0)));
        assert!(!row, "viewport の外へ隠れた行は click されない");
        assert!(after, "同じ click は scroll_area の後ろの widget に届く");
    }

    /// viewport の中に見えている行は今までどおり click できる。
    #[test]
    fn a_visible_row_is_still_clickable() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let (row, _) = run_row(&mut host, click((50.0, 110.0)));
        assert!(row);
    }

    /// viewport の中で始めたドラッグは、外へ出ても位置と release が届く (外で離して終われる)。
    #[test]
    fn a_drag_started_inside_continues_and_ends_outside() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let seen = Cell::new((None, false, false));
        let mut run = |host: &mut UiHost<()>, input: FrameInput| {
            host.frame_to_edits(&(), &mut scene, SCREEN, input, |(), ui| {
                ui.scroll_area("s", VIEWPORT, CONTENT, |ui, _| {
                    let p = ui.pointer();
                    seen.set((p.pos, p.primary_pressed, p.primary_just_released));
                });
            });
        };
        run(&mut host, pointer((50.0, 150.0), |p| {
            p.primary_just_pressed = true;
            p.primary_pressed = true;
        }));
        run(&mut host, pointer((50.0, 20.0), |p| p.primary_pressed = true));
        assert_eq!(seen.get(), (Some((50.0, 20.0)), true, false), "外へ出てもドラッグは続く");
        run(&mut host, pointer((50.0, 20.0), |p| p.primary_just_released = true));
        assert_eq!(seen.get(), (Some((50.0, 20.0)), false, true), "外で離したことが届く");
    }

    /// viewport の外では hover・新しい press・ホイールが中身に届かない。
    #[test]
    fn hover_press_and_wheel_outside_the_viewport_do_not_reach_the_content() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let seen = Cell::new(PointerFrame::default());
        let input = pointer((50.0, 20.0), |p| {
            p.primary_just_pressed = true;
            p.primary_pressed = true;
            p.secondary_just_pressed = true;
            p.scroll_delta = (0.0, 40.0);
        });
        let after = Cell::new(PointerFrame::default());
        host.frame_to_edits(&(), &mut scene, SCREEN, input, |(), ui| {
            ui.scroll_area("s", VIEWPORT, CONTENT, |ui, _| seen.set(ui.pointer()));
            after.set(ui.pointer());
        });
        let p = seen.get();
        assert_eq!(p.pos, None);
        assert!(!p.primary_just_pressed && !p.primary_pressed && !p.secondary_just_pressed);
        assert_eq!(p.scroll_delta, (0.0, 0.0));
        let p = after.get();
        assert!(p.primary_just_pressed && p.secondary_just_pressed, "scroll_area を出たら元の入力");
        assert_eq!(p.scroll_delta, (0.0, 40.0));
    }

    /// 中身の dropdown が開いた一覧は viewport の外へはみ出す。はみ出した項目も選べ、
    /// 一覧の外を click すれば閉じる。
    #[test]
    fn a_popup_opened_from_the_content_works_outside_the_viewport() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let items = ["a", "b", "c", "d", "e", "f"];
        // 本体は viewport の下端近く。一覧 (6 項目 × 24px) は viewport の下へはみ出す。
        let body = Rect { x: 10.0, y: 170.0, w: 120.0, h: 24.0 };
        let chosen = Cell::new(None);
        let open = Cell::new(false);
        let mut run = |host: &mut UiHost<()>, input: FrameInput| {
            host.frame_to_edits(&(), &mut scene, SCREEN, input, |(), ui| {
                ui.scroll_area("s", VIEWPORT, CONTENT, |ui, _| {
                    if let Some(i) = ui.dropdown("dd", body, &items, 0) {
                        chosen.set(Some(i));
                    }
                    open.set(ui.is_dropdown_open("dd"));
                });
            });
        };
        run(&mut host, click((20.0, 180.0)));
        assert!(open.get(), "本体の click で開く");
        // 4 項目目 (y = 194 + 3 * 24 = 266..290) は viewport (y < 200) の外。
        run(&mut host, click((20.0, 278.0)));
        assert_eq!(chosen.get(), Some(3), "viewport の外へはみ出した項目を選べる");
        assert!(!open.get());

        run(&mut host, click((20.0, 180.0)));
        assert!(open.get());
        // 一覧の外 (viewport の外でもある) を click すると閉じる。
        run(&mut host, click((300.0, 350.0)));
        assert!(!open.get(), "viewport の外の click でも一覧が閉じる");
    }
}
