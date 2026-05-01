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

use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, GlyphArea, LineBatch, Rect, RectCommand, Scene};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::input::PointerFrame;
use crate::widgets::WidgetState;

/// アプリが 1 つ持つ UI ホスト。フレーム間で UI 内部状態を保持する。
pub struct UiHost<M: ?Sized + 'static> {
    state: HashMap<WidgetId, Box<dyn WidgetState>>,
    _m: PhantomData<fn(&mut M)>,
}

impl<M: ?Sized + 'static> UiHost<M> {
    pub fn new() -> Self {
        Self {
            state: HashMap::new(),
            _m: PhantomData,
        }
    }

    /// 1 フレーム分の UI を構築。返り値は発生したエディットのリスト。
    ///
    /// `f` は `(model, &mut Ui)` を受け取り、ウィジェットを呼び出して UI を組む。
    pub fn frame<F>(
        &mut self,
        model: &M,
        scene: &mut Scene,
        screen: PhysicalSize,
        pointer: PointerFrame,
        f: F,
    ) -> Vec<Edit<M>>
    where
        F: for<'a> FnOnce(&'a M, &mut Ui<'a, M>),
    {
        let mut edits: Vec<Edit<M>> = Vec::new();
        let cursor = Rect::new(0.0, 0.0, screen.width as f32, screen.height as f32);
        let mut ui = Ui {
            state: &mut self.state,
            scene,
            edits: &mut edits,
            pointer,
            cursor,
            screen,
            next_y: 0.0,
            _m: PhantomData,
        };
        f(model, &mut ui);
        edits
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
    // M2 以降のドラッグ/スクロール/フォーカス状態保持で使う。
    #[allow(dead_code)]
    state: &'a mut HashMap<WidgetId, Box<dyn WidgetState>>,
    scene: &'a mut Scene,
    edits: &'a mut Vec<Edit<M>>,
    pub(crate) pointer: PointerFrame,
    /// 現在の利用可能領域 (シンプルな vstack 用)。
    pub(crate) cursor: Rect,
    pub(crate) screen: PhysicalSize,
    /// vstack 内で次に積むウィジェットの y 位置。
    pub(crate) next_y: f32,
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

/// クリック判定用ヘルパ — 矩形に対するヒットテスト + just_released なら true。
pub(crate) fn clicked(rect: Rect, pointer: PointerFrame) -> bool {
    let Some((px, py)) = pointer.pos else { return false };
    pointer.primary_just_released && rect.contains(px, py)
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
        host.frame(&model, &mut scene, screen, PointerFrame::default(), |_, ui| {
            let id = WidgetId::ROOT.child("ws-roundtrip");
            let state: &mut MyState = ui.widget_state(id);
            assert_eq!(state.count, 0);
            state.count += 1;
        });

        // フレーム 2: 同じ id で同じ型を取り直すと値が保持されている。
        host.frame(&model, &mut scene, screen, PointerFrame::default(), |_, ui| {
            let id = WidgetId::ROOT.child("ws-roundtrip");
            let state: &mut MyState = ui.widget_state(id);
            assert_eq!(state.count, 1);
            state.count += 1;
        });

        host.frame(&model, &mut scene, screen, PointerFrame::default(), |_, ui| {
            let id = WidgetId::ROOT.child("ws-roundtrip");
            let state: &mut MyState = ui.widget_state(id);
            assert_eq!(state.count, 2);
        });
    }
}
