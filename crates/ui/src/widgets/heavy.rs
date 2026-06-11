//! `HeavyCtx` — 巨大ビュー (ピアノロール / アレンジメント / 大量クリップ波形) の
//! ViewportKey ベース粗粒度キャッシュ脱出口 (M5 Phase 13)。
//!
//! 通常 widget の per-widget input_hash キャッシュ (`Ui::with_widget_node`) と
//! 違い、heavy() は「描画ブロック全体」を 1 つの巨大 widget として扱う。
//! 実装は `with_widget_node` の薄いラッパで、新キャッシュ機構は作らない。
//!
//! # 使い方
//!
//! ```ignore
//! ui.heavy("piano_roll", |hctx| {
//!     // 描画は cached() の中。viewport_key 一致なら draw_fn は呼ばれず、
//!     // 前フレームの描画コマンドを scene に再利用 append。
//!     hctx.cached((view_start, view_len, generations), |hctx| {
//!         for note in m.visible_notes() {
//!             hctx.push_rect(...);
//!         }
//!     });
//!     // ヒットテスト・動的 overlay は cached() の外で毎フレーム実行。
//!     if let Some((px, py)) = hctx.pointer().pos {
//!         // Edit 発行など
//!     }
//! });
//! ```
//!
//! # ネスト時の挙動
//!
//! `cached()` の中で既存 widget (`hctx.button_at` / `hctx.waveform` 等) を
//! 呼ぶと、外側 `cached()` hit のときは内側 widget の `with_widget_node` も
//! 呼ばれない (二段キャッシュ)。外側 miss なら内側も呼ばれて各々
//! input_hash で個別判定される。

use std::hash::Hash;

use daw_ui_platform::{CursorIcon, PhysicalSize};
use daw_ui_renderer::{Color, GlyphArea, LineBatch, Rect, RectCommand, TextureHandle, TexturedQuad};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::input::{DroppedFiles, PointerFrame};
use crate::scenegraph::hash_inputs;
use crate::time::TimeMapping;
use crate::ui::Ui;
use crate::viewport::ViewportState1D;
use crate::widgets::drag_rect::DragRect;
use crate::widgets::time_grid::{BarBeatGridStyle, SubGridSpec, TimeRulerStyle};
use crate::widgets::waveform::{WaveformResponse, WaveformSource, WaveformStyle, WaveformView};

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 巨大ビュー用の脱出口。`HeavyCtx::cached` で ViewportKey 一致時に
    /// 前フレームの描画コマンドを再利用する。
    ///
    /// `id` は heavy ブロックを特定する識別子 (アプリ側で一意であること)。
    pub fn heavy<F>(&mut self, id: impl Hash, f: F)
    where
        F: for<'b> FnOnce(&mut HeavyCtx<'b, 'a, M>),
    {
        let root_wid = WidgetId::ROOT.child((b"heavy", &id));
        let mut hctx = HeavyCtx { ui: self, root_wid };
        f(&mut hctx);
    }
}

/// `Ui::heavy` 内で使うコンテキスト。`cached` で粗粒度キャッシュ、
/// `pointer / push_*` で毎フレームのヒットテスト・追加描画を行う。
pub struct HeavyCtx<'b, 'a, M: ?Sized + 'static> {
    ui: &'b mut Ui<'a, M>,
    root_wid: WidgetId,
}

impl<'b, 'a, M: ?Sized + 'static> HeavyCtx<'b, 'a, M> {
    /// `viewport_key` が前フレームと同じ → `draw_fn` を実行せず、前フレームの
    /// 描画コマンドを scene へ append。異なる → `draw_fn` を実行して結果を記録。
    ///
    /// ヒットテスト・動的 overlay (cursor / 選択範囲) は `cached` の **外側** で
    /// 行うこと (cached 内側の描画は viewport_key 一致時にスキップされる)。
    pub fn cached<K, F>(&mut self, viewport_key: K, draw_fn: F)
    where
        K: Hash,
        F: for<'c> FnOnce(&mut HeavyCtx<'c, 'a, M>),
    {
        let block_wid = self.root_wid.child(b"cached");
        let input_hash = hash_inputs((b"heavy_cached", viewport_key));
        let root_wid = self.root_wid;
        self.ui.with_widget_node(block_wid, input_hash, |ui_inner| {
            let mut hctx = HeavyCtx { ui: ui_inner, root_wid };
            draw_fn(&mut hctx);
        });
    }

    /// このフレームの pointer 状態 (cached の内外を問わず使える)。
    pub fn pointer(&self) -> PointerFrame {
        self.ui.pointer()
    }

    /// 画面サイズ。
    pub fn screen(&self) -> PhysicalSize {
        self.ui.screen()
    }

    // === 描画コマンド (heavy 内では公開、通常 widget は pub(crate) のまま) ===

    /// 矩形を scene に積む。`cached` の内側で呼ぶと cache 対象、外側で呼ぶと
    /// 毎フレーム描画 (overlay / 選択範囲など)。
    pub fn push_rect(&mut self, cmd: RectCommand) {
        self.ui.push_rect(cmd);
    }

    /// テキストを scene に積む。
    pub fn push_text(&mut self, area: GlyphArea) {
        self.ui.push_text(area);
    }

    /// 線分バッチを scene に積む。
    pub fn push_lines(&mut self, batch: LineBatch) {
        self.ui.push_lines(batch);
    }

    /// M14 Phase 71 (daw_01 #043): textured quad を scene に積む。
    ///
    /// UV 全域 + clip なし の convenience。 部分 UV / クリップが必要なら
    /// [`Self::push_textured_quad`] を直接呼ぶ。 video frame thumbnail (arrangement) や
    /// preview window composite で使用する。 destroy 済 handle は描画 no-op。
    pub fn push_texture(&mut self, rect: Rect, texture: TextureHandle, alpha: f32) {
        self.ui.push_textured_quad(TexturedQuad {
            rect,
            texture,
            alpha,
            uv_min: (0.0, 0.0),
            uv_max: (1.0, 1.0),
            clip_rect: None,
            rotation_radians: 0.0,
            rotation_pivot: None,
        });
    }

    /// M14 Phase 71/72: 部分 UV / クリップ rect 指定が必要な textured quad 用の delegate。
    /// arrangement の video clip thumbnail (rect 内 aspect-fit + lanes intersect clip) や
    /// preview window 内の crop 描画で使用。
    pub fn push_textured_quad(&mut self, quad: TexturedQuad) {
        self.ui.push_textured_quad(quad);
    }

    /// Edit を発行する。ヒットテストで click を検出したらここから流す。
    pub fn push_edit(&mut self, edit: Edit<M>) {
        self.ui.push_edit(edit);
    }

    /// M14 Phase 77 (daw_01 #048): heavy 内で `Ui::with_clip_rect` を使う delegate。
    ///
    /// closure 内の全 `push_*` 呼び出しが `rect` で auto-scissor される (= `merge_clip`
    /// 経由で既存 `clip_rect` と intersect)。 既存 explicit な `clip_rect: Some(...)`
    /// は idempotent に narrowing される (= 二重指定で破綻しない)。 `cached` 内で呼ぶと
    /// 生成された primitive の `clip_rect` に焼き込まれて cache に保存される (= cache
    /// 再生時にも scissor が効く)。
    ///
    /// arrangement / piano_roll 等で「描画 region (ruler / header_pane / lanes) 外への
    /// leak を構造的に防ぐ」 用途で使う。
    pub fn with_clip_rect<F>(&mut self, rect: Rect, f: F)
    where
        F: FnOnce(&mut Self),
    {
        // `Ui::with_clip_rect` は `FnOnce(&mut Ui)` 受けで HeavyCtx を貫通できないため、
        // current_clip stack を直接 push/pop する形で再実装 (= ui.rs と完全同 idiom)。
        let prev = self.ui.current_clip;
        self.ui.current_clip = Some(
            crate::ui::merge_clip(prev, Some(rect)).unwrap_or(rect),
        );
        f(self);
        self.ui.current_clip = prev;
    }

    // === 既存 widget の delegate (heavy 内でも呼べる) ===

    /// `Ui::waveform` の delegate。heavy() の中で複数クリップ波形を一括描画する用途。
    pub fn waveform<'s>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        source: WaveformSource<'s>,
        view: WaveformView,
        style: WaveformStyle,
    ) -> WaveformResponse {
        self.ui.waveform(id, rect, source, view, style)
    }

    /// `Ui::label_at` の delegate。
    pub fn label_at(
        &mut self,
        id: impl Hash,
        text: &str,
        x: f32,
        y: f32,
        font_size: f32,
        color: Color,
    ) {
        self.ui.label_at(id, text, x, y, font_size, color);
    }

    /// `Ui::button_at` の delegate。
    pub fn button_at(
        &mut self,
        id: impl Hash,
        text: &str,
        rect: Rect,
        on_click: impl FnOnce() -> Edit<M>,
    ) {
        self.ui.button_at(id, text, rect, on_click);
    }

    /// `Ui::time_ruler` の delegate (M13 Phase 55、cached layer 内で呼ぶ用)。
    pub fn time_ruler(
        &mut self,
        id: impl Hash,
        rect: Rect,
        mapping: TimeMapping,
        viewport: ViewportState1D,
        style: TimeRulerStyle,
    ) {
        self.ui.time_ruler(id, rect, mapping, viewport, style);
    }

    /// `Ui::bar_beat_grid` の delegate (M13 Phase 55、cached layer 内で呼ぶ用)。
    /// (M14 Phase 124 / daw_01 #100) `sub` で subdivision (3 段目) を追加可。
    pub fn bar_beat_grid(
        &mut self,
        id: impl Hash,
        rect: Rect,
        mapping: TimeMapping,
        viewport: ViewportState1D,
        style: BarBeatGridStyle,
        sub: Option<SubGridSpec>,
    ) {
        self.ui.bar_beat_grid(id, rect, mapping, viewport, style, sub);
    }

    // === M9 P1-3: input / popup / shortcut / clipboard / dialog / history pull API ===
    //
    // すべて `Ui` の同名メソッドへの 1 行 forward。heavy 抽象の漏れ (rect-select / 右クリック /
    // shortcut consume などが heavy 内で書けなかった) を 1 commit で塞ぐ。

    /// `Ui::take_drag_rect_in_rect` の delegate (rect multi-select 用)。
    pub fn take_drag_rect_in_rect(&mut self, wid: WidgetId, bounds: Rect) -> Option<DragRect> {
        self.ui.take_drag_rect_in_rect(wid, bounds)
    }

    /// `Ui::take_file_drop_in_rect` の delegate (heavy 内に audio file を drop)。
    pub fn take_file_drop_in_rect(&mut self, rect: Rect) -> Option<DroppedFiles> {
        self.ui.take_file_drop_in_rect(rect)
    }

    /// `Ui::is_file_hovering_in_rect` の delegate (drop target highlight 用)。
    #[must_use]
    pub fn is_file_hovering_in_rect(&self, rect: Rect) -> bool {
        self.ui.is_file_hovering_in_rect(rect)
    }

    /// `Ui::take_clipboard_paste` の delegate。
    pub fn take_clipboard_paste(&mut self) -> Option<String> {
        self.ui.take_clipboard_paste()
    }

    /// `Ui::set_clipboard_text` の delegate。
    pub fn set_clipboard_text(&mut self, s: String) {
        self.ui.set_clipboard_text(s);
    }

    /// `Ui::take_shortcut` の delegate (heavy 内で `delete` などの shortcut を consume)。
    pub fn take_shortcut(&mut self, name: &'static str) -> bool {
        self.ui.take_shortcut(name)
    }

    /// M14 Phase 57: `Ui::take_typing_shortcut` の delegate (heavy 内 text widget 用)。
    pub fn take_typing_shortcut(&mut self, name: &'static str) -> bool {
        self.ui.take_typing_shortcut(name)
    }

    /// `Ui::shortcut_for` の delegate (menu hint / overlay 表示用)。
    #[must_use]
    pub fn shortcut_for(&self, name: &'static str) -> Option<String> {
        self.ui.shortcut_for(name)
    }

    /// `Ui::take_scroll_in_rect` の delegate (heavy 内で zoom / scroll)。
    pub fn take_scroll_in_rect(&mut self, rect: Rect) -> (f32, f32) {
        self.ui.take_scroll_in_rect(rect)
    }

    /// `Ui::context_menu_for` の delegate (heavy 内で右クリック menu)。
    /// M9 P1-5 で `on_select: FnOnce(usize, &mut Ui<'_, M>)` に breaking 変更。
    pub fn context_menu_for<F>(&mut self, rect: Rect, items: &[&str], on_select: F)
    where
        F: for<'ui> FnOnce(usize, &mut Ui<'ui, M>),
    {
        self.ui.context_menu_for(rect, items, on_select);
    }

    /// `Ui::request_redraw` の delegate。
    pub fn request_redraw(&mut self) {
        self.ui.request_redraw();
    }

    /// `Ui::request_undo` の delegate (heavy 内で Ctrl+Z 検出 → undo 要求)。
    pub fn request_undo(&mut self) {
        self.ui.request_undo();
    }

    /// `Ui::request_redo` の delegate。
    pub fn request_redo(&mut self) {
        self.ui.request_redo();
    }

    /// `Ui::can_undo` の delegate (heavy 内で UI 状態の表示判断に使う)。
    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.ui.can_undo()
    }

    /// `Ui::can_redo` の delegate。
    #[must_use]
    pub fn can_redo(&self) -> bool {
        self.ui.can_redo()
    }

    /// `Ui::set_cursor` の delegate (M9 Phase 41b、heavy 内 hover で cursor 変更等)。
    pub fn set_cursor(&mut self, cursor: CursorIcon) {
        self.ui.set_cursor(cursor);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use daw_ui_platform::PhysicalSize;
    use daw_ui_renderer::{Color, Rect, RectCommand, Scene};

    use crate::edit::Edit;
    use crate::input::{FrameInput, PointerFrame};
    use crate::ui::UiHost;

    fn small_red_rect(rect: Rect) -> RectCommand {
        RectCommand {
            rect,
            fill: Color::rgb(1.0, 0.0, 0.0),
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: None,
        }
    }

    /// 同じ heavy id + 同じ viewport_key → 2 回目は draw_fn skip、ただし scene には
    /// cached 経由で同じコマンドが積まれる。
    #[test]
    fn heavy_cached_hit_skips_draw_fn() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let test_rect = Rect { x: 10.0, y: 20.0, w: 30.0, h: 40.0 };

        let calls = Cell::new(0_u32);

        // Frame 1: cache miss → draw_fn 実行、rect が積まれる。
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.heavy("h1", |hctx| {
                hctx.cached(0xAAAAu64, |hctx| {
                    calls.set(calls.get() + 1);
                    hctx.push_rect(small_red_rect(test_rect));
                });
            });
        });
        assert_eq!(calls.get(), 1);
        assert_eq!(scene.rect_count(), 1);
        assert_eq!(scene.rects_vec()[0].rect, test_rect);

        // Frame 2: 同じ viewport_key → cache hit、draw_fn skip。scene には cached 経由で
        // 同じ rect が積まれる。
        scene.clear();
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.heavy("h1", |hctx| {
                hctx.cached(0xAAAAu64, |hctx| {
                    calls.set(calls.get() + 1);
                    hctx.push_rect(small_red_rect(test_rect));
                });
            });
        });
        assert_eq!(calls.get(), 1, "cache hit で draw_fn は呼ばれない");
        assert_eq!(scene.rect_count(), 1, "cache 経由で同じ rect が積まれる");
        assert_eq!(scene.rects_vec()[0].rect, test_rect);
    }

    /// viewport_key が変わると cache miss、draw_fn が再実行される。
    #[test]
    fn heavy_cached_miss_runs_draw_fn() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let test_rect = Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 };

        let calls = Cell::new(0_u32);

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.heavy("h2", |hctx| {
                hctx.cached(0xAAAAu64, |hctx| {
                    calls.set(calls.get() + 1);
                    hctx.push_rect(small_red_rect(test_rect));
                });
            });
        });
        assert_eq!(calls.get(), 1);

        scene.clear();
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.heavy("h2", |hctx| {
                // viewport_key を変える。
                hctx.cached(0xBBBBu64, |hctx| {
                    calls.set(calls.get() + 1);
                    hctx.push_rect(small_red_rect(test_rect));
                });
            });
        });
        assert_eq!(calls.get(), 2, "viewport_key 変化で draw_fn が再実行される");
    }

    /// ヒットテスト経路: pointer / push_edit が cached の外で機能する。
    #[test]
    fn heavy_pointer_and_push_edit_outside_cached() {
        struct Counter { value: u32 }

        let mut host: UiHost<Counter> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let mut model = Counter { value: 0 };
        let screen = PhysicalSize { width: 200, height: 100 };

        let click = PointerFrame {
            pos: Some((50.0, 50.0)),
            primary_just_released: true,
            ..PointerFrame::default()
        };
        let edits = host.frame_to_edits(
            &model,
            &mut scene,
            screen,
            FrameInput { pointer: click, ..Default::default() },
            |_, ui| {
                ui.heavy("h3", |hctx| {
                    hctx.cached(1u64, |hctx| {
                        hctx.push_rect(small_red_rect(Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 }));
                    });
                    // ヒットテスト + Edit 発行 (cached の外)。
                    if hctx.pointer().primary_just_released {
                        hctx.push_edit(Edit::mutate(|m: &mut Counter| {
                            m.value += 1;
                        }));
                    }
                });
            },
        );
        for e in edits {
            e.apply(&mut model);
        }
        assert_eq!(model.value, 1);
    }

    // -------- M9 P1-3: HeavyCtx delegate (input/popup/shortcut/clipboard/history) --------

    /// `take_drag_rect_in_rect` が heavy 内で呼べる (drag 中でなければ None)。
    #[test]
    fn heavy_take_drag_rect_in_rect_returns_none_without_drag() {
        use crate::id::WidgetId;
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let bounds = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.heavy("h_drag", |hctx| {
                let wid = WidgetId::ROOT.child(b"hctx_drag_test");
                let result = hctx.take_drag_rect_in_rect(wid, bounds);
                assert!(result.is_none(), "no drag in progress → None");
            });
        });
    }

    /// shortcut consume が `Ui` と `HeavyCtx` で state 共有されている。
    /// 外側で consume → heavy 内では再 consume できない。
    #[test]
    fn heavy_take_shortcut_consumed_outside_is_unavailable_inside() {
        use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey};
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };

        // default bindings に "delete" = Delete key が含まれる
        let key = KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Delete,
        };

        let outer = Cell::new(false);
        let inner = Cell::new(false);

        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { keyboard: vec![key], ..Default::default() },
            |(), ui| {
                outer.set(ui.take_shortcut("delete"));
                ui.heavy("h_sc", |hctx| {
                    inner.set(hctx.take_shortcut("delete"));
                });
            },
        );
        assert!(outer.get(), "shortcut consumed outside heavy");
        assert!(!inner.get(), "consumed → inside heavy returns false (state shared)");
    }

    /// `take_shortcut` を heavy 内側で先に呼ぶと、外側では false。逆方向の sharing 確認。
    #[test]
    fn heavy_take_shortcut_consumed_inside_is_unavailable_outside() {
        use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey};
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let key = KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Delete,
        };

        let inner = Cell::new(false);
        let outer = Cell::new(false);

        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { keyboard: vec![key], ..Default::default() },
            |(), ui| {
                ui.heavy("h_sc2", |hctx| {
                    inner.set(hctx.take_shortcut("delete"));
                });
                outer.set(ui.take_shortcut("delete"));
            },
        );
        assert!(inner.get());
        assert!(!outer.get());
    }

    /// `context_menu_for` が heavy 内で panic なく呼べる (popup の細部は menu.rs で検証済)。
    #[test]
    fn heavy_context_menu_for_runs_inside_without_panic() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 10.0, y: 10.0, w: 50.0, h: 50.0 };

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.heavy("h_ctx", |hctx| {
                hctx.context_menu_for(rect, &["Cut", "Copy", "Delete"], |idx, hctx_ui| {
                    let _ = idx;
                    hctx_ui.push_edit(Edit::mutate(|(): &mut ()| {}));
                });
            });
        });
        // 右クリックなしのため popup は開かない、panic なしを担保するだけ
    }

    /// `is_file_hovering_in_rect` / `take_file_drop_in_rect` が heavy 内で呼べる。
    #[test]
    fn heavy_file_drop_apis_run_inside() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.heavy("h_drop", |hctx| {
                assert!(!hctx.is_file_hovering_in_rect(rect));
                assert!(hctx.take_file_drop_in_rect(rect).is_none());
            });
        });
    }

    /// heavy ブロックを 1 フレームスキップ → 次フレームで呼ぶと cache miss
    /// (eviction が効いている)。
    #[test]
    fn heavy_evicts_when_not_called_for_a_frame() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let test_rect = Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 };

        let calls = Cell::new(0_u32);

        // Frame 1: heavy ブロック登場 → cache 記録。
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.heavy("h4", |hctx| {
                hctx.cached(0u64, |hctx| {
                    calls.set(calls.get() + 1);
                    hctx.push_rect(small_red_rect(test_rect));
                });
            });
        });
        assert_eq!(calls.get(), 1);

        // Frame 2: heavy ブロックを呼ばない → eviction される。
        scene.clear();
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), _ui| {});

        // Frame 3: heavy ブロックを再び呼ぶ → eviction されているので cache miss、
        // draw_fn が再実行される。
        scene.clear();
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.heavy("h4", |hctx| {
                hctx.cached(0u64, |hctx| {
                    calls.set(calls.get() + 1);
                    hctx.push_rect(small_red_rect(test_rect));
                });
            });
        });
        assert_eq!(calls.get(), 2, "eviction で cache miss、draw_fn が再実行");
    }
}
