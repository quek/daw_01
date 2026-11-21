//! floating window (開いたまま背後を操作できる常駐の窓) への pointer の配り方。
//!
//! 窓は 2 段で使う: build の **背景 widget より前** に [`Ui::reserve_floating_region`] で自分の矩形を
//! 予約し、**背景の後** に [`Ui::with_floating_region`] の中で自分を描く。予約は背景に対して効く
//! (pointer が矩形の上にある間、背景 widget は pointer を受けない)。
//!
//! 窓どうしの重なりは **予約した順 = 下から上** で決める (描く順と同じ順で予約すること)。
//! pointer の下にある窓のうち一番上の 1 つだけが pointer を受け、その下に隠れた窓は受けない。
//! 押したボタンは、押し始めた窓が離すまで持ち続ける (タイトルバーを掴んで別の窓の上を通っても、
//! ドラッグが止まったり、通り道の窓が反応したりしない)。

use std::hash::Hash;

use daw_ui_renderer::Rect;

use crate::id::WidgetId;
use crate::input::PointerFrame;
use crate::ui::{Ui, masked_pointer};

/// このフレームに予約された floating window 1 つ。
#[derive(Debug, Clone, Copy)]
pub(crate) struct FloatingRegion {
    id: WidgetId,
    rect: Rect,
    /// 予約の順に関係なく、他のどの floating window よりも上として扱う。
    exclusive: bool,
}

/// このフレームの floating window の状態 ([`Ui`] がフレームごとに作る)。
#[derive(Debug, Default)]
pub(crate) struct FloatingFrame {
    /// 予約された窓 (予約順 = 下から上)。
    regions: Vec<FloatingRegion>,
    /// pointer の配り先。最初の [`Ui::with_floating_region`] で 1 回だけ決める
    /// (それまでに全部の窓の予約が済んでいること)。
    routing: Option<Routing>,
}

/// このフレームの pointer の配り先。
#[derive(Debug, Clone, Copy)]
struct Routing {
    /// pointer を受ける窓。`None` = どの窓のものでもない (背景の上にあって、どの窓も
    /// 押したままのボタンを持っていない) ので、全部の窓が従来どおり raw pointer を読む。
    owner: Option<WidgetId>,
    /// このフレームに release と次の press が両方入り、ボタンの持ち主が替わったときの前の持ち主。
    /// release だけを見せる (見せないと、その窓のドラッグが終わらないまま残る)。
    released: Option<WidgetId>,
}

/// 押したままのボタンを持っている窓 (フレームをまたいで残す)。押し始めた位置にあった一番上の窓で、
/// 背景で押し始めた / 押していないなら `None`。
#[derive(Debug, Default)]
struct FloatingPress(Option<WidgetId>);

fn region_id(id: impl Hash) -> WidgetId {
    WidgetId::ROOT.child((b"floating_region", &id))
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// floating window が、 **自分の rect 分だけ** 背後の pointer を占有するための予約 (module doc)。
    /// `open_overlay` (= 開いている間、 背景全体の pointer を mask する capture_input modal) と違い、
    /// pointer が `rect` の **外** にある間は背景 widget を通常どおり操作できる。
    ///
    /// `id` は窓ごとに固定の値で、 [`Ui::with_floating_region`] に同じものを渡す。
    ///
    /// 真のモーダル (`modal_capturing`) が開いている間は何もしない (モーダルが優先し、
    /// 窓はその背後で inert)。
    pub fn reserve_floating_region(&mut self, id: impl Hash, rect: Rect) {
        self.reserve_floating(region_id(id), rect, false);
    }

    /// [`Ui::reserve_floating_region`] の、 **他の floating window も全部遮る** 版。 窓の外へはみ出す
    /// 一覧を開いている間や、 処理中で画面全体を触らせない間に、 画面全体を `rect` にして使う
    /// (予約の順が後の窓も、 この窓より下として扱う)。 開いている一覧の有無は
    /// [`Ui::is_dropdown_open`](crate::ui::Ui::is_dropdown_open) で分かる。
    pub fn reserve_floating_region_exclusive(&mut self, id: impl Hash, rect: Rect) {
        self.reserve_floating(region_id(id), rect, true);
    }

    fn reserve_floating(&mut self, id: WidgetId, rect: Rect, exclusive: bool) {
        if self.modal_capturing {
            return;
        }
        debug_assert!(self.floating.routing.is_none(), "floating window の予約は、どの窓を描くよりも前に済ませる");
        self.floating.regions.push(FloatingRegion { id, rect, exclusive });
        if !self.pointer_raw.pos.is_some_and(|(x, y)| rect.contains(x, y)) {
            return;
        }
        self.pointer = masked_pointer(self.pointer_raw);
        // `pending_secondary_click` / `pending_double_click` は **生 pointer 由来**
        // (フレーム冒頭で `pointer_raw` から算出) なので、`self.pointer` を mask
        // しただけでは素通りする。落とさないと予約領域の上で右クリックした瞬間に
        // 背景 widget が context menu を開いてしまい、その popup は mask された
        // pointer しか読めないので **item も outside-click も効かない**
        // = 消せないメニューが居座る (daw_01 r.md #54 レビューで発覚。全画面を
        // 予約する用途で初めて表面化した)。
        let inside = |p: &Option<(f32, f32)>| p.is_some_and(|(x, y)| rect.contains(x, y));
        if inside(self.pending_secondary_click) {
            *self.pending_secondary_click = None;
        }
        if inside(self.pending_double_click) {
            *self.pending_double_click = None;
        }
        if inside(self.pending_double_click_press) {
            *self.pending_double_click_press = None;
        }
    }

    /// [`Ui::reserve_floating_region`] とペア。 floating window 自身の描画をこの closure 内で行う。
    /// 窓の中の widget は **raw pointer** を読む (背後 mask を一時解除) ので drag / click / scroll が
    /// 効く。 ただし pointer が自分より上の窓の下に隠れているとき、 別の窓が押したままのボタンを
    /// 持っているときは受けない (module doc)。 真のモーダル中は解除しない (窓はモーダルの背後で
    /// 操作不可のまま)。
    pub fn with_floating_region<R>(&mut self, id: impl Hash, f: impl FnOnce(&mut Self) -> R) -> R {
        let saved = self.pointer;
        if !self.modal_capturing {
            let wid = region_id(id);
            let routing = self.floating_routing();
            let raw = self.pointer_raw;
            self.pointer = if routing.owner.is_none_or(|owner| owner == wid) {
                raw
            } else if routing.released == Some(wid) {
                PointerFrame { primary_just_pressed: false, primary_pressed: false, ..raw }
            } else {
                masked_pointer(raw)
            };
        }
        let r = f(self);
        self.pointer = saved;
        r
    }

    /// このフレームの pointer の配り先。 フレームに 1 回だけ決める。
    fn floating_routing(&mut self) -> Routing {
        if let Some(routing) = self.floating.routing {
            return routing;
        }
        let raw = self.pointer_raw;
        let top = raw.pos.and_then(|(x, y)| {
            self.floating
                .regions
                .iter()
                .enumerate()
                .filter(|(_, region)| region.rect.contains(x, y))
                .max_by_key(|(order, region)| (region.exclusive, *order))
                .map(|(_, region)| region.id)
        });
        let FloatingPress(held) = self.widget_state(WidgetId::ROOT.child(b"floating_press"));
        let mut released = None;
        if raw.primary_just_pressed {
            // release も同じフレームに入っていて、 前の持ち主が居た = 離してすぐ押し直した。
            if raw.primary_just_released {
                released = held.filter(|prev| Some(*prev) != top);
            }
            *held = top;
        } else if !raw.primary_pressed && !raw.primary_just_released {
            *held = None;
        }
        let routing = Routing { owner: held.or(top), released };
        self.floating.routing = Some(routing);
        routing
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use daw_ui_platform::PhysicalSize;
    use daw_ui_renderer::{Rect, Scene};

    use crate::input::{FrameInput, PointerFrame};
    use crate::ui::UiHost;

    /// 下の窓 (先に予約)。
    const LOWER: Rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
    /// 上の窓 (後に予約)。`LOWER` と x = 60..100 で重なる。
    const UPPER: Rect = Rect { x: 60.0, y: 0.0, w: 100.0, h: 100.0 };

    /// 1 フレームぶん: 背景・下の窓・上の窓が、それぞれ受け取った pointer を返す。
    /// `lower_exclusive` = 下の窓を「他の窓も全部遮る」予約にする (画面全体)。
    fn frame(host: &mut UiHost<()>, pointer: PointerFrame, lower_exclusive: bool) -> [PointerFrame; 3] {
        let mut scene = Scene::new();
        let seen = Cell::new([PointerFrame::default(); 3]);
        let input = FrameInput { pointer, ..FrameInput::default() };
        let screen = PhysicalSize { width: 400, height: 400 };
        let _ = host.frame_to_edits(&(), &mut scene, screen, input, |(), ui| {
            if lower_exclusive {
                ui.reserve_floating_region_exclusive("lower", Rect { x: 0.0, y: 0.0, w: 400.0, h: 400.0 });
            } else {
                ui.reserve_floating_region("lower", LOWER);
            }
            ui.reserve_floating_region("upper", UPPER);
            let background = ui.pointer();
            let lower = ui.with_floating_region("lower", |ui| ui.pointer());
            let upper = ui.with_floating_region("upper", |ui| ui.pointer());
            seen.set([background, lower, upper]);
        });
        seen.get()
    }

    fn press_at(pos: (f32, f32)) -> PointerFrame {
        PointerFrame { pos: Some(pos), primary_just_pressed: true, primary_pressed: true, ..PointerFrame::default() }
    }

    fn held_at(pos: (f32, f32)) -> PointerFrame {
        PointerFrame { pos: Some(pos), primary_pressed: true, ..PointerFrame::default() }
    }

    fn release_at(pos: (f32, f32)) -> PointerFrame {
        PointerFrame { pos: Some(pos), primary_just_released: true, ..PointerFrame::default() }
    }

    /// 重なった所の press は上の窓だけに届く (下の窓にも背景にも届かない)。
    #[test]
    fn overlap_press_reaches_only_the_upper_window() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let [background, lower, upper] = frame(&mut host, press_at((80.0, 50.0)), false);
        assert!(!background.primary_just_pressed && background.pos.is_none());
        assert!(!lower.primary_just_pressed && lower.pos.is_none(), "下の窓には届かない");
        assert!(upper.primary_just_pressed, "上の窓には届く");
    }

    /// 重なっていない所は、それぞれの窓に届く。
    #[test]
    fn non_overlapping_parts_reach_their_own_window() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let [_, lower, upper] = frame(&mut host, press_at((20.0, 50.0)), false);
        assert!(lower.primary_just_pressed);
        assert!(!upper.primary_just_pressed);
        let mut host: UiHost<()> = UiHost::no_redraw();
        let [_, lower, upper] = frame(&mut host, press_at((140.0, 50.0)), false);
        assert!(!lower.primary_just_pressed);
        assert!(upper.primary_just_pressed);
    }

    /// 下の窓で押し始めたドラッグは、上の窓の上を通っても下の窓が持ち続け、release も下の窓に届く。
    /// 通り道の上の窓は何も受けない。離した次のフレームからは、また上の窓が受ける。
    #[test]
    fn press_stays_with_the_window_it_started_in() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let [_, lower, _] = frame(&mut host, press_at((20.0, 50.0)), false);
        assert!(lower.primary_just_pressed);

        let [_, lower, upper] = frame(&mut host, held_at((80.0, 50.0)), false);
        assert_eq!(lower.pos, Some((80.0, 50.0)), "ドラッグ中の位置は下の窓に届き続ける");
        assert!(upper.pos.is_none(), "通り道の上の窓は受けない");

        let [_, lower, upper] = frame(&mut host, release_at((80.0, 50.0)), false);
        assert!(lower.primary_just_released, "release は押し始めた窓に届く");
        assert!(!upper.primary_just_released);

        let hover = PointerFrame { pos: Some((80.0, 50.0)), ..PointerFrame::default() };
        let [_, lower, upper] = frame(&mut host, hover, false);
        assert!(lower.pos.is_none());
        assert_eq!(upper.pos, Some((80.0, 50.0)));
    }

    /// pointer が背景の上なら、背景も全部の窓も raw pointer を読む (窓の外へのホバーなどは従来どおり)。
    /// 背景で押し始めたドラッグが窓の上を通ると、背景は受けず、その位置の一番上の窓が受ける。
    #[test]
    fn pointer_over_background_reaches_everything() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let [background, lower, upper] = frame(&mut host, press_at((300.0, 300.0)), false);
        assert!(background.primary_just_pressed && lower.primary_just_pressed && upper.primary_just_pressed);

        let [background, lower, upper] = frame(&mut host, held_at((80.0, 50.0)), false);
        assert!(background.pos.is_none());
        assert!(lower.pos.is_none());
        assert_eq!(upper.pos, Some((80.0, 50.0)));
    }

    /// 離してすぐ別の窓を押し直したのが 1 フレームに入っても、前の持ち主には release が届く。
    #[test]
    fn previous_owner_sees_the_release_when_the_press_moves_on() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let _ = frame(&mut host, press_at((20.0, 50.0)), false);
        let both = PointerFrame { primary_just_released: true, ..press_at((140.0, 50.0)) };
        let [_, lower, upper] = frame(&mut host, both, false);
        assert!(lower.primary_just_released && !lower.primary_just_pressed && !lower.primary_pressed);
        assert!(upper.primary_just_pressed);
    }

    /// 「他の窓も全部遮る」予約は、予約の順が後の窓よりも上になる。
    #[test]
    fn exclusive_region_outranks_later_windows() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let [background, lower, upper] = frame(&mut host, press_at((140.0, 50.0)), true);
        assert!(background.pos.is_none());
        assert!(lower.primary_just_pressed, "全体を予約した窓が受ける");
        assert!(!upper.primary_just_pressed && upper.pos.is_none(), "後から予約した窓でも受けない");
    }
}
