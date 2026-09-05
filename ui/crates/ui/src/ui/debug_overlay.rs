//! M9 Phase 43: debug overlay — 直近フレームの統計 (`FrameStats`) を画面右上に描く。
//! `Ui::frame` の統計は前フレームの値 (今フレームは描画中でまだ確定していない)。

use daw_ui_renderer::{GlyphArea, Rect, RectCommand};

use super::Ui;

impl<M: ?Sized + 'static> Ui<'_, M> {
    /// 直近フレームの統計を `rect` の右上に半透明 overlay として描画する。
    ///
    /// `frame_ms` は app 側で測定した frame の所要時間 (window backend / render pipeline
    /// により計測方法が違うので library は track せず引数で受ける)。`0.0` を渡せば省略。
    ///
    /// 表示項目:
    /// - frame: `{frame_ms:.2}ms` (引数 `frame_ms` < 1e-6 なら省略)
    /// - cache: `{hits} / {hits+misses}` + ヒット率 `{rate:.0}%`
    /// - widgets: `{widget_count}` (scenegraph_size と通常一致)
    ///
    /// 統計は **前フレーム** の値 (今フレームは描画中でまだ確定していない)。`Ui::take_shortcut`
    /// を組み合わせると Ctrl+F1 で toggle できる:
    /// ```ignore
    /// if ui.take_shortcut("debug_overlay_toggle") {
    ///     m.show_debug = !m.show_debug;
    /// }
    /// if m.show_debug {
    ///     ui.debug_overlay(area, last_frame_ms);
    /// }
    /// ```
    pub fn debug_overlay(&mut self, rect: Rect, frame_ms: f32) {
        let stats = self.last_frame_stats;
        let line_h = 14.0;
        let pad = 6.0;
        let font_size = 11.0;
        let lines: Vec<String> = {
            let mut v = Vec::with_capacity(5);
            if frame_ms.abs() > 1e-6 {
                v.push(format!("frame  {frame_ms:>5.2}ms"));
            }
            let total = stats.cache_hits + stats.cache_misses;
            v.push(format!(
                "cache  {} / {} ({:>3.0}%)",
                stats.cache_hits,
                total,
                stats.cache_hit_rate() * 100.0
            ));
            v.push(format!("wgts   {}", stats.widget_count));
            v.push(format!("sg     {}", stats.scenegraph_size));
            v
        };
        let lines_n = lines.len() as f32;
        let bg_w = 200.0_f32.min(rect.w);
        let bg_h = (lines_n * line_h + pad * 2.0).min(rect.h);
        let bg_rect = Rect {
            x: rect.x + rect.w - bg_w - pad,
            y: rect.y + pad,
            w: bg_w,
            h: bg_h,
        };
        // M9 Phase 44a: popup buffer (= popup pass) に push して z-order 最前面に。
        // Phase 43 で発見した「popup pass の glyph buffer 上書き」問題は Phase 44a で
        // popup_glyph: GlyphPipeline を独立インスタンスにすることで根本解決済み。
        let prev_in_popup = self.drawing_in_popup;
        self.drawing_in_popup = true;
        let p = self.palette();
        self.push_rect(RectCommand {
            rect: bg_rect,
            fill: p.debug_overlay_bg,
            border: p.debug_overlay_border,
            border_width: 1.0,
            radius: [3.0; 4],
            clip_rect: None,
        });
        for (i, text) in lines.iter().enumerate() {
            self.push_text(GlyphArea {
                text: text.as_str().into(),
                left: bg_rect.x + pad,
                top: bg_rect.y + pad + (i as f32) * line_h,
                font_size,
                line_height: line_h,
                color: p.debug_text,
                clip_rect: None,
                ..GlyphArea::default()
            });
        }
        self.drawing_in_popup = prev_in_popup;
    }
}
