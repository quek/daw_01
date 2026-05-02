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

use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, GlyphArea, LineBatch, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::input::PointerFrame;
use crate::scenegraph::hash_inputs;
use crate::ui::Ui;
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

    /// Edit を発行する。ヒットテストで click を検出したらここから流す。
    pub fn push_edit(&mut self, edit: Edit<M>) {
        self.ui.push_edit(edit);
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
        assert_eq!(scene.rects.len(), 1);
        assert_eq!(scene.rects[0].rect, test_rect);

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
        assert_eq!(scene.rects.len(), 1, "cache 経由で同じ rect が積まれる");
        assert_eq!(scene.rects[0].rect, test_rect);
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
