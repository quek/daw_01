//! `Ui<'a, M>` — 1 フレームの間 `&'a M` を借りて UI を構築するコンテキスト。
//!
//! ユーザのアプリループ:
//! ```ignore
//! let edits = host.frame(&model, &mut scene, &input, |m, ui| {
//!     ui.label("title", "Mixer");
//!     ui.button("mute", "Mute", || Edit::mutate(|m: &mut MixerModel| m.mute = !m.mute));
//! });
//! for e in edits { e.apply(&mut model); }
//! ```

use std::collections::HashMap;
use std::marker::PhantomData;

use daw_ui_platform::{KeyEvent, PhysicalSize};
use daw_ui_renderer::{Color, GlyphArea, LineBatch, Rect, RectCommand, Scene};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::input::{FrameInput, ImeEvent, PointerFrame};
use crate::scenegraph::Scenegraph;
use crate::widgets::WidgetState;

/// アプリが 1 つ持つ UI ホスト。フレーム間で UI 内部状態を保持する。
pub struct UiHost<M: ?Sized + 'static> {
    state: HashMap<WidgetId, Box<dyn WidgetState>>,
    /// キーボードフォーカスを持つウィジェット (`text_input` 等)。
    /// クリックがフォーカス可能な widget でない場所に当たったら `None` にクリアされる
    /// (`frame` の終わりに次フレームへ commit)。
    focused: Option<WidgetId>,
    /// 直前の `frame()` 呼び出しでフォーカスが変化したか。
    /// アプリ側は `had_edits` と同様、これが `true` なら「次フレームで新しい
    /// focus 状態に基づいた再描画」を行うため `request_redraw` を呼ぶこと。
    focus_changed_in_last_frame: bool,
    /// 直前の `frame()` で `Ui::request_ime` が呼ばれたときの cursor 領域。
    /// アプリは on_render の終わりにこの値を見て winit の `set_ime_cursor_area` /
    /// `set_ime_allowed` を呼ぶ。`None` のフレームでは IME を無効化する。
    last_ime_request: Option<Rect>,
    /// M4 Phase 10 で追加: 内部 scenegraph (per-widget input_hash の前フレーム履歴)。
    /// Phase 11 で `Ui::with_widget_node` API から書き込まれる。Phase 10 では宣言のみ。
    #[allow(dead_code)]
    scenegraph: Scenegraph,
    _m: PhantomData<fn(&mut M)>,
}

impl<M: ?Sized + 'static> UiHost<M> {
    pub fn new() -> Self {
        Self {
            state: HashMap::new(),
            focused: None,
            focus_changed_in_last_frame: false,
            last_ime_request: None,
            scenegraph: Scenegraph::new(),
            _m: PhantomData,
        }
    }

    /// 直前の `frame()` 呼び出しでフォーカスが変化したか。
    /// アプリの on_render はこれが true のとき `request_redraw` を呼ぶことで、
    /// blur や focus 取得が画面に追いつくように。
    pub fn focus_changed_in_last_frame(&self) -> bool {
        self.focus_changed_in_last_frame
    }

    /// 直前の `frame()` で focused widget が要求した IME 候補ウィンドウ位置 (Rect)。
    /// `Some` ならアプリは `WindowBackend::set_ime_allowed(true)` +
    /// `set_ime_cursor_area(rect)` を呼ぶ。`None` なら IME を無効化する。
    pub fn ime_request(&self) -> Option<Rect> {
        self.last_ime_request
    }

    /// 1 フレーム分の UI を構築。返り値は発生したエディットのリスト。
    ///
    /// `f` は `(model, &mut Ui)` を受け取り、ウィジェットを呼び出して UI を組む。
    /// `input` の `keyboard` / `ime` イベントは focused widget が消費する想定。
    pub fn frame<F>(
        &mut self,
        model: &M,
        scene: &mut Scene,
        screen: PhysicalSize,
        input: FrameInput,
        f: F,
    ) -> Vec<Edit<M>>
    where
        F: for<'a> FnOnce(&'a M, &mut Ui<'a, M>),
    {
        let mut edits: Vec<Edit<M>> = Vec::new();
        let FrameInput { pointer, keyboard, ime } = input;
        let mut keyboard_events = keyboard;
        let mut ime_events = ime;
        let cursor = Rect::new(0.0, 0.0, screen.width as f32, screen.height as f32);
        let focused_at_start = self.focused;
        let mut ui = Ui {
            state: &mut self.state,
            scene,
            edits: &mut edits,
            pointer,
            keyboard_events: &mut keyboard_events,
            ime_events: &mut ime_events,
            cursor,
            screen,
            next_y: 0.0,
            focused: focused_at_start,
            pending_focus: focused_at_start,
            focus_changed_this_frame: false,
            ime_request: None,
            _m: PhantomData,
        };
        f(model, &mut ui);
        // フォーカスの commit:
        // - 誰かが set_focus / clear_focus を呼んでいたら pending_focus がそのまま反映。
        // - そうでないとき、このフレームでクリック (release) があったら blur 扱いで
        //   focused = None にする (= フォーカス可能でない場所がクリックされた)。
        let prev_focused = self.focused;
        if ui.focus_changed_this_frame {
            self.focused = ui.pending_focus;
        } else if pointer.primary_just_released && self.focused.is_some() {
            self.focused = None;
        }
        self.focus_changed_in_last_frame = self.focused != prev_focused;
        // IME request の commit (フレーム内に request_ime が呼ばれていれば Some)。
        self.last_ime_request = ui.ime_request;
        edits
    }

    /// 現在キーボードフォーカスを持つ widget の ID。
    pub fn focused_widget(&self) -> Option<WidgetId> {
        self.focused
    }
}

impl<M: ?Sized + 'static> Default for UiHost<M> {
    fn default() -> Self {
        Self::new()
    }
}

/// 1 フレーム内のみ生きる UI コンテキスト。
///
/// `'a` は `&'a M` 借用と同じ寿命。`Edit<M>` は `'static` (M1) なので Ui のライフタイムから
/// 切り離せる。
pub struct Ui<'a, M: ?Sized + 'static> {
    state: &'a mut HashMap<WidgetId, Box<dyn WidgetState>>,
    scene: &'a mut Scene,
    edits: &'a mut Vec<Edit<M>>,
    pub(crate) pointer: PointerFrame,
    /// このフレーム分のキー入力イベント (フォーカスを持つ widget が消費する)。
    keyboard_events: &'a mut Vec<KeyEvent>,
    /// このフレーム分の IME イベント (フォーカスを持つ widget が消費する)。
    ime_events: &'a mut Vec<ImeEvent>,
    /// 現在の利用可能領域 (シンプルな vstack 用)。
    pub(crate) cursor: Rect,
    pub(crate) screen: PhysicalSize,
    /// vstack 内で次に積むウィジェットの y 位置。
    pub(crate) next_y: f32,
    /// このフレーム開始時点でキーボードフォーカスを持つ widget。
    focused: Option<WidgetId>,
    /// このフレーム内で widget が `set_focus` / `clear_focus` を呼んだ結果。
    /// frame 終了時に `UiHost::focused` に commit される。
    pending_focus: Option<WidgetId>,
    /// このフレーム内で誰かがフォーカスを操作したか。
    /// クリック発生時にこれが `false` なら blur (= フォーカス可能でない場所がクリックされた)。
    focus_changed_this_frame: bool,
    /// このフレーム内で `Ui::request_ime` で要求された IME 候補ウィンドウ位置 (Rect)。
    /// 同フレーム内に複数 widget が呼んだ場合は最後の呼び出しが勝つ
    /// (typical: focused widget だけが呼ぶ想定)。
    ime_request: Option<Rect>,
    _m: PhantomData<&'a M>,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    pub fn screen(&self) -> PhysicalSize {
        self.screen
    }

    pub fn pointer(&self) -> PointerFrame {
        self.pointer
    }

    /// 内部: ウィジェットが描画コマンドを Scene に積む。
    pub(crate) fn push_rect(&mut self, cmd: RectCommand) {
        self.scene.push_rect(cmd);
    }

    /// 内部: ウィジェットがテキスト描画を積む。
    pub(crate) fn push_text(&mut self, area: GlyphArea) {
        self.scene.push_text(area);
    }

    /// 内部: ウィジェットが線分バッチを積む (波形・メータ・グリッド)。
    pub(crate) fn push_lines(&mut self, batch: LineBatch) {
        self.scene.push_lines(batch);
    }

    /// 内部: ウィジェットがエディットを積む。
    pub(crate) fn push_edit(&mut self, edit: Edit<M>) {
        self.edits.push(edit);
    }

    /// `wid` が現在キーボードフォーカスを持っているか。
    ///
    /// 同フレーム内で `set_focus` が呼ばれた場合の効果も反映する (= 描画用に
    /// 「クリックと同時にフォーカス枠を出す」ような遅延の無い見た目を作る)。
    pub fn is_focused(&self, wid: WidgetId) -> bool {
        self.pending_focus == Some(wid)
    }

    /// `wid` をキーボードフォーカスに設定する。
    /// `is_focused` には即時反映され、次フレーム以降のキー入力配信もこの widget に向く。
    pub fn set_focus(&mut self, wid: WidgetId) {
        self.pending_focus = Some(wid);
        self.focus_changed_this_frame = true;
    }

    /// 自身がフォーカスを持っているならクリアする。
    /// 持っていないときは no-op (他 widget のフォーカスを誤って消さない)。
    pub fn clear_focus_if_focused(&mut self, wid: WidgetId) {
        if self.pending_focus == Some(wid) {
            self.pending_focus = None;
            self.focus_changed_this_frame = true;
        }
    }

    /// `wid` がフォーカスを持っているならフレームに溜まったキー入力を取り出す。
    ///
    /// チェック対象は **フレーム開始時の focus** (`self.focused`)。これによって、
    /// 「同フレームに click でフォーカスを取った直後に、その widget が直前まで
    /// 流れていたキー入力を遡って消費してしまう」事故を防ぐ
    /// (= 例: 何かを打鍵中に間違えて click した場合、打鍵は古い focus に流すのが正解)。
    /// 取り出すと内部 buffer は空になるので、フレーム内で 1 回だけ呼ぶこと。
    pub fn take_keyboard_events_if_focused(&mut self, wid: WidgetId) -> Vec<KeyEvent> {
        if self.focused == Some(wid) {
            std::mem::take(self.keyboard_events)
        } else {
            Vec::new()
        }
    }

    /// `wid` がフォーカスを持っているならフレームに溜まった IME イベントを取り出す。
    /// `take_keyboard_events_if_focused` と同じく、フレーム開始時 focus でチェックする。
    pub fn take_ime_events_if_focused(&mut self, wid: WidgetId) -> Vec<ImeEvent> {
        if self.focused == Some(wid) {
            std::mem::take(self.ime_events)
        } else {
            Vec::new()
        }
    }

    /// IME 候補ウィンドウを表示するべき領域 (cursor 直下) を要求する。
    /// 通常は focused text_input が自分の cursor 位置周辺の rect を渡す。
    /// アプリは `UiHost::ime_request()` でこの値を取得し、`set_ime_cursor_area` を呼ぶ。
    pub fn request_ime(&mut self, cursor_area: Rect) {
        self.ime_request = Some(cursor_area);
    }

    /// 内部: WidgetId に紐付く永続状態を取得 or 初期化。
    /// (M2 で waveform の LOD ピラミッドキャッシュに、M3 以降は fader/knob のドラッグ状態に使う)
    pub(crate) fn widget_state<S: WidgetState + Default + 'static>(
        &mut self,
        id: WidgetId,
    ) -> &mut S {
        let entry = self
            .state
            .entry(id)
            .or_insert_with(|| Box::new(S::default()));
        // `Box<dyn WidgetState>` 自体が `T: Any + Send + Sync` の blanket impl で
        // `WidgetState` を実装してしまうため、`entry.as_any_mut()` は **Box 外側** の
        // 実装を呼んでしまう (TypeId が Box<dyn WidgetState> になり downcast が必ず失敗)。
        // 明示的に `**entry` で dyn WidgetState まで deref してから vtable 経由で呼ぶ。
        let dyn_ws: &mut dyn WidgetState = &mut **entry;
        dyn_ws
            .as_any_mut()
            .downcast_mut::<S>()
            .expect("WidgetState 型不一致")
    }
}

/// 視覚フィードバック用 — 押下中(矩形内 & primary_pressed)なら true。
pub(crate) fn pressed_inside(rect: Rect, pointer: PointerFrame) -> bool {
    let Some((px, py)) = pointer.pos else { return false };
    pointer.primary_pressed && rect.contains(px, py)
}

/// hover 中なら true。
pub(crate) fn hovered(rect: Rect, pointer: PointerFrame) -> bool {
    let Some((px, py)) = pointer.pos else { return false };
    rect.contains(px, py)
}

/// 色のヘルパ。
pub(crate) fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `widget_state` で書き戻した値が次フレームでも同型として読み取れる
    /// (`Box<dyn WidgetState>` 自体への blanket impl が `as_any_mut` を奪わないことの回帰防止)。
    #[test]
    fn widget_state_round_trip_no_downcast_panic() {
        #[derive(Debug, Default)]
        struct MyState {
            count: u32,
        }

        struct Model;

        let mut host: UiHost<Model> = UiHost::new();
        let mut scene = Scene::new();
        let model = Model;
        let screen = PhysicalSize { width: 400, height: 300 };

        // フレーム 1: state を初期化して 1 回インクリメント。
        host.frame(&model, &mut scene, screen, FrameInput::default(), |_, ui| {
            let id = WidgetId::ROOT.child("ws-roundtrip");
            let state: &mut MyState = ui.widget_state(id);
            assert_eq!(state.count, 0);
            state.count += 1;
        });

        // フレーム 2: 同じ id で同じ型を取り直すと値が保持されている。
        host.frame(&model, &mut scene, screen, FrameInput::default(), |_, ui| {
            let id = WidgetId::ROOT.child("ws-roundtrip");
            let state: &mut MyState = ui.widget_state(id);
            assert_eq!(state.count, 1);
            state.count += 1;
        });

        host.frame(&model, &mut scene, screen, FrameInput::default(), |_, ui| {
            let id = WidgetId::ROOT.child("ws-roundtrip");
            let state: &mut MyState = ui.widget_state(id);
            assert_eq!(state.count, 2);
        });
    }

    /// 「hover フレームを描画 → 次フレームで同フレーム内 press + release」で
    /// click が 1 回目から発火することを担保する。ユーザ報告:
    /// 「ボタンにマウスを乗せた直後の最初のクリックでアクションが反応しない」
    /// の回帰防止。
    #[test]
    fn button_click_fires_on_first_hover_then_click() {
        struct Counter {
            count: u32,
        }

        let mut host: UiHost<Counter> = UiHost::new();
        let mut scene = Scene::new();
        let mut model = Counter { count: 0 };
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 32.0 };

        // Frame 1: cursor をボタン上にホバー (まだクリック無し)。
        let pointer_hover = PointerFrame {
            pos: Some((50.0, 16.0)),
            ..PointerFrame::default()
        };
        let edits = host.frame(&model, &mut scene, screen, FrameInput { pointer: pointer_hover, ..Default::default() }, |_, ui| {
            ui.button_at("test", "click me", rect, || {
                Edit::mutate(|m: &mut Counter| m.count += 1)
            });
        });
        for e in edits {
            e.apply(&mut model);
        }
        assert_eq!(model.count, 0, "hover フレームでは click は出ない");

        // Frame 2: 同フレーム内で press + release (高速クリック相当)。
        let pointer_click = PointerFrame {
            pos: Some((50.0, 16.0)),
            primary_just_pressed: true,
            primary_just_released: true,
            ..PointerFrame::default()
        };
        let edits = host.frame(&model, &mut scene, screen, FrameInput { pointer: pointer_click, ..Default::default() }, |_, ui| {
            ui.button_at("test", "click me", rect, || {
                Edit::mutate(|m: &mut Counter| m.count += 1)
            });
        });
        for e in edits {
            e.apply(&mut model);
        }
        assert_eq!(
            model.count, 1,
            "hover 直後の最初のクリックで click が発火するべき"
        );
    }

    /// press と release が別フレームに分かれて届くケース (winit が press と release で
    /// それぞれ別の redraw を発火するパターン) で、`press_started_inside` がフレーム間で
    /// ちゃんと保持されて release フレームで click 発火することを担保する。
    #[test]
    fn button_click_fires_when_press_and_release_in_separate_frames() {
        struct Counter {
            count: u32,
        }

        let mut host: UiHost<Counter> = UiHost::new();
        let mut scene = Scene::new();
        let mut model = Counter { count: 0 };
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 32.0 };
        let render = |host: &mut UiHost<Counter>,
                      scene: &mut Scene,
                      model: &Counter,
                      pointer: PointerFrame|
         -> Vec<Edit<Counter>> {
            host.frame(model, scene, screen, FrameInput { pointer, ..Default::default() }, |_, ui| {
                ui.button_at("test", "click me", rect, || {
                    Edit::mutate(|m: &mut Counter| m.count += 1)
                });
            })
        };

        // Frame 1: hover.
        let edits = render(
            &mut host,
            &mut scene,
            &model,
            PointerFrame {
                pos: Some((50.0, 16.0)),
                ..PointerFrame::default()
            },
        );
        for e in edits { e.apply(&mut model); }
        assert_eq!(model.count, 0);

        // Frame 2: press フレーム (まだ release していない、ボタン押下中)。
        let edits = render(
            &mut host,
            &mut scene,
            &model,
            PointerFrame {
                pos: Some((50.0, 16.0)),
                primary_just_pressed: true,
                primary_just_released: false,
                primary_pressed: true,
                ..PointerFrame::default()
            },
        );
        for e in edits { e.apply(&mut model); }
        assert_eq!(model.count, 0, "press フレームでは click は出ない");

        // Frame 3: release フレーム。
        let edits = render(
            &mut host,
            &mut scene,
            &model,
            PointerFrame {
                pos: Some((50.0, 16.0)),
                primary_just_pressed: false,
                primary_just_released: true,
                ..PointerFrame::default()
            },
        );
        for e in edits { e.apply(&mut model); }
        assert_eq!(
            model.count, 1,
            "release フレームで click 発火するべき (press_started_inside が保持されている)"
        );
    }

    /// `Ui::set_focus` の効果が同フレームで `is_focused` に反映されること、
    /// および次フレームでも維持されることを確認する。
    #[test]
    fn focus_set_persists_to_next_frame() {
        let mut host: UiHost<()> = UiHost::new();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let id = WidgetId::ROOT.child("focus-target");

        // Frame 1: set_focus を呼ぶと **同フレーム内で** is_focused = true になる。
        host.frame(&(), &mut scene, screen, FrameInput::default(), |_, ui| {
            assert!(!ui.is_focused(id));
            ui.set_focus(id);
            assert!(ui.is_focused(id), "set_focus 後は同フレームで is_focused = true");
        });
        assert_eq!(host.focused_widget(), Some(id));

        // Frame 2: 何もしないが focus は維持。
        host.frame(&(), &mut scene, screen, FrameInput::default(), |_, ui| {
            assert!(ui.is_focused(id));
        });
        assert_eq!(host.focused_widget(), Some(id));
    }

    /// フォーカスを取った widget の上でクリックされても、その widget が `set_focus` を
    /// 呼び続ける限りフォーカスは保たれる。クリック先が誰も `set_focus` を呼ばない
    /// (= フォーカス可能でない場所) ならフォーカスはクリアされる。
    #[test]
    fn click_outside_clears_focus_when_no_widget_claims() {
        let mut host: UiHost<()> = UiHost::new();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let id = WidgetId::ROOT.child("focus-target");

        // Frame 1: フォーカスを取る。
        host.frame(&(), &mut scene, screen, FrameInput::default(), |_, ui| {
            ui.set_focus(id);
        });
        assert_eq!(host.focused_widget(), Some(id));

        // Frame 2: クリック発生 (just_released=true) で誰も set_focus を呼ばない → blur。
        let click = PointerFrame {
            pos: Some((50.0, 50.0)),
            primary_just_released: true,
            ..PointerFrame::default()
        };
        host.frame(&(), &mut scene, screen, FrameInput { pointer: click, ..Default::default() }, |_, _ui| {
            // 誰も set_focus / clear_focus を呼ばない。
        });
        assert_eq!(
            host.focused_widget(),
            None,
            "誰もフォーカスを取り直さなかったので blur される"
        );
    }

    /// 同フレームで set_focus を呼んでいればクリックがあってもフォーカスは保たれる。
    #[test]
    fn focus_kept_when_widget_re_claims_on_click() {
        let mut host: UiHost<()> = UiHost::new();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let id = WidgetId::ROOT.child("focus-target");

        host.frame(&(), &mut scene, screen, FrameInput::default(), |_, ui| {
            ui.set_focus(id);
        });
        assert_eq!(host.focused_widget(), Some(id));

        let click = PointerFrame {
            pos: Some((50.0, 50.0)),
            primary_just_released: true,
            ..PointerFrame::default()
        };
        host.frame(&(), &mut scene, screen, FrameInput { pointer: click, ..Default::default() }, |_, ui| {
            // クリックフレームで widget が再度 set_focus を呼ぶ (text_input が再クリックされたケース)。
            ui.set_focus(id);
        });
        assert_eq!(host.focused_widget(), Some(id), "再 set_focus でフォーカス維持");
    }

    /// text_input をクリックでフォーカスを取り、キー入力で text を編集できることを担保する。
    /// click → focus → 'A' 入力 → モデルが "A" になる、という流れを通しで検証。
    #[test]
    fn text_input_click_focus_then_typing_modifies_text() {
        use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey};

        struct Doc {
            text: String,
        }

        let mut host: UiHost<Doc> = UiHost::new();
        let mut scene = Scene::new();
        let mut model = Doc { text: String::new() };
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 0.0, y: 0.0, w: 200.0, h: 28.0 };

        // Frame 1: click で focus を取る (まだ text は空)。
        let click = PointerFrame {
            pos: Some((50.0, 14.0)),
            primary_just_pressed: true,
            primary_just_released: true,
            ..PointerFrame::default()
        };
        let edits = host.frame(&model, &mut scene, screen, FrameInput { pointer: click, ..Default::default() }, |_, ui| {
            ui.text_input_at("ti", rect, "", |new| {
                Edit::mutate(|m: &mut Doc| m.text = new)
            });
        });
        for e in edits { e.apply(&mut model); }
        assert_eq!(model.text, "");

        // Frame 2: 'A' のキー入力を流す (focus されているので消費される)。
        let keys = vec![KeyEvent {
            state: ElementState::Pressed,
            text: Some("A".to_string()),
            physical_key: PhysicalKey::Other(0x41),
        }];
        let edits = host.frame(
            &model, &mut scene, screen, FrameInput { keyboard: keys, ..Default::default() },
            |m, ui| {
                ui.text_input_at("ti", rect, &m.text, |new| {
                    Edit::mutate(|m: &mut Doc| m.text = new)
                });
            },
        );
        for e in edits { e.apply(&mut model); }
        assert_eq!(model.text, "A");

        // Frame 3: Backspace で 1 文字消える。
        let keys = vec![KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Backspace,
        }];
        let edits = host.frame(
            &model, &mut scene, screen, FrameInput { keyboard: keys, ..Default::default() },
            |m, ui| {
                ui.text_input_at("ti", rect, &m.text, |new| {
                    Edit::mutate(|m: &mut Doc| m.text = new)
                });
            },
        );
        for e in edits { e.apply(&mut model); }
        assert_eq!(model.text, "");
    }

    /// IME preedit イベントは focused text_input に届き、state.preedit に反映される
    /// (model の text には反映されない)。Commit イベントは cursor 位置に挿入し
    /// preedit をクリアして Edit を発行する。
    #[test]
    fn text_input_ime_preedit_then_commit() {
        use crate::input::ImeEvent;
        use daw_ui_platform::PhysicalSize as PS;

        struct Doc {
            text: String,
        }

        let mut host: UiHost<Doc> = UiHost::new();
        let mut scene = Scene::new();
        let mut model = Doc { text: String::new() };
        let screen = PS { width: 200, height: 100 };
        let rect = Rect { x: 0.0, y: 0.0, w: 200.0, h: 28.0 };

        // Frame 1: click で focus 取得。
        let click = PointerFrame {
            pos: Some((50.0, 14.0)),
            primary_just_pressed: true,
            primary_just_released: true,
            ..PointerFrame::default()
        };
        let edits = host.frame(
            &model,
            &mut scene,
            screen,
            FrameInput { pointer: click, ..Default::default() },
            |_, ui| {
                ui.text_input_at("ti", rect, "", |new| {
                    Edit::mutate(|m: &mut Doc| m.text = new)
                });
            },
        );
        for e in edits { e.apply(&mut model); }
        assert_eq!(model.text, "");

        // Frame 2: preedit 「あ」が来る。model は変わらず、内部 state にだけ反映。
        let edits = host.frame(
            &model,
            &mut scene,
            screen,
            FrameInput {
                ime: vec![ImeEvent::Preedit { text: "あ".to_string(), cursor: None }],
                ..Default::default()
            },
            |m, ui| {
                ui.text_input_at("ti", rect, &m.text, |new| {
                    Edit::mutate(|m: &mut Doc| m.text = new)
                });
            },
        );
        for e in edits { e.apply(&mut model); }
        assert_eq!(model.text, "", "preedit 中は model に反映しない");

        // Frame 3: commit 「あ」が来る。model に確定挿入され、preedit はクリア。
        let edits = host.frame(
            &model,
            &mut scene,
            screen,
            FrameInput {
                ime: vec![ImeEvent::Commit("あ".to_string())],
                ..Default::default()
            },
            |m, ui| {
                ui.text_input_at("ti", rect, &m.text, |new| {
                    Edit::mutate(|m: &mut Doc| m.text = new)
                });
            },
        );
        for e in edits { e.apply(&mut model); }
        assert_eq!(model.text, "あ", "commit で model.text に挿入される");
    }

    /// IME イベントは focused widget にだけ届き、focused でない widget は空を受け取る。
    #[test]
    fn ime_events_delivered_only_to_focused() {
        use crate::input::ImeEvent;

        let mut host: UiHost<()> = UiHost::new();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let id_a = WidgetId::ROOT.child("a");
        let id_b = WidgetId::ROOT.child("b");

        host.frame(&(), &mut scene, screen, FrameInput::default(), |_, ui| {
            ui.set_focus(id_a);
        });

        let ime = vec![ImeEvent::Commit("z".to_string())];
        host.frame(
            &(),
            &mut scene,
            screen,
            FrameInput { ime, ..Default::default() },
            |_, ui| {
                let b_ime = ui.take_ime_events_if_focused(id_b);
                assert_eq!(b_ime.len(), 0);
                let a_ime = ui.take_ime_events_if_focused(id_a);
                assert_eq!(a_ime.len(), 1);
                let a_ime2 = ui.take_ime_events_if_focused(id_a);
                assert_eq!(a_ime2.len(), 0, "drain 後は空");
            },
        );
    }

    /// キー入力イベントは focused widget だけに届き、他の widget には空が返る。
    #[test]
    fn keyboard_events_delivered_only_to_focused() {
        use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey};

        let mut host: UiHost<()> = UiHost::new();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let id_a = WidgetId::ROOT.child("a");
        let id_b = WidgetId::ROOT.child("b");

        // a にフォーカスを置く。
        host.frame(&(), &mut scene, screen, FrameInput::default(), |_, ui| {
            ui.set_focus(id_a);
        });

        // 次フレーム: キー入力を流す。a は受け取れる、b は受け取れない。
        let keys = vec![KeyEvent {
            state: ElementState::Pressed,
            text: Some("x".to_string()),
            physical_key: PhysicalKey::Other(0),
        }];
        host.frame(&(), &mut scene, screen, FrameInput { keyboard: keys, ..Default::default() }, |_, ui| {
            // b で先に呼んでも空 (フォーカスが a)。
            let b_keys = ui.take_keyboard_events_if_focused(id_b);
            assert_eq!(b_keys.len(), 0);
            // a が呼ぶと届く。
            let a_keys = ui.take_keyboard_events_if_focused(id_a);
            assert_eq!(a_keys.len(), 1);
            assert_eq!(a_keys[0].text.as_deref(), Some("x"));
            // 二度目に a が呼んでも空 (内部 buffer が drain 済み)。
            let a_keys2 = ui.take_keyboard_events_if_focused(id_a);
            assert_eq!(a_keys2.len(), 0);
        });
    }
}
