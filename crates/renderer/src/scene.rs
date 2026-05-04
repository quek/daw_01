//! `Scene` — 1 フレーム分の描画コマンド (DisplayList、call-order interleave)。
//!
//! 上位層は `Scene::push_rect` / `push_text` / `push_lines` でコマンドを積み、
//! `Renderer::render(&scene)` を呼ぶ。
//!
//! ## M9 Phase 45f: call-order primitive interleave
//!
//! 旧設計 (M7-M9 Phase 45e) は `rects` / `glyph_areas` / `line_batches` を **別 Vec**
//! に振り分け、renderer が `rect → line → glyph` の固定順で描画していた。これは
//! GPU の pipeline 切り替えコストを最小化する設計だが、副作用として **z-order が
//! type ベース** になり、後から push した rect が前に push した glyph より下に描画
//! される。 panel + text_input のような overlay で button text が透けて見える根本原因。
//!
//! 新設計: 全 primitive を **`primitives: Vec<Primitive>`** に call order で並べる。
//! renderer は同 type の連続 primitive を 1 つの "run" にまとめて batch、各 run ごとに
//! drawcall を発行する。state 切り替え数は run 数に比例 (典型的に 10-50 / frame、許容範囲)。
//! popup pass も同じ pattern (`popup_primitives: Vec<Primitive>`)。

/// RGBA、各成分 [0.0, 1.0] (sRGB)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const TRANSPARENT: Self = Self { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
    pub const BLACK: Self = Self { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
    pub const WHITE: Self = Self { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };

    pub const fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }
    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }
    pub fn to_wgpu(self) -> wgpu::Color {
        wgpu::Color {
            r: f64::from(self.r),
            g: f64::from(self.g),
            b: f64::from(self.b),
            a: f64::from(self.a),
        }
    }
}

/// 物理ピクセル単位の矩形。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
    /// 2 つの矩形の交差。重ならないときは `w/h = 0` の空矩形 (位置は左上の交差点)。
    #[must_use]
    pub fn intersect(&self, other: Rect) -> Rect {
        let l = self.x.max(other.x);
        let t = self.y.max(other.y);
        let r = (self.x + self.w).min(other.x + other.w);
        let b = (self.y + self.h).min(other.y + other.h);
        if r > l && b > t {
            Rect::new(l, t, r - l, b - t)
        } else {
            Rect::new(l, t, 0.0, 0.0)
        }
    }
    /// `w == 0 || h == 0` のとき true。
    pub fn is_empty(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }
}

/// 角丸矩形 1 つ分の描画コマンド (instanced rect の入力)。
#[derive(Debug, Clone, Copy)]
pub struct RectCommand {
    pub rect: Rect,
    pub fill: Color,
    pub border: Color,
    pub border_width: f32,
    /// 4 隅の半径 (順番: tl, tr, br, bl)。同一値で済む場合は `Self::uniform_radius` 使用。
    pub radius: [f32; 4],
    /// `Some` ならこの矩形外を scissor で切り捨てる。`None` で全画面描画。
    /// scroll_area / popup_layer / split_view が `Ui::with_clip_rect` 経由で自動設定する。
    pub clip_rect: Option<Rect>,
}

impl RectCommand {
    pub fn uniform_radius(rect: Rect, fill: Color, radius: f32) -> Self {
        Self {
            rect,
            fill,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [radius; 4],
            clip_rect: None,
        }
    }
}

/// テキスト描画 1 ブロック分。glyphon の `TextArea` に近い情報を持つ。
#[derive(Debug, Clone)]
pub struct GlyphArea {
    /// 表示する文字列 (UTF-8)。
    pub text: String,
    /// クリップ矩形の左上 (物理ピクセル)。
    pub left: f32,
    pub top: f32,
    /// フォントサイズ (論理ピクセル)。スケールは Renderer 側で適用。
    pub font_size: f32,
    /// 行高さ。font_size * 1.2 等。
    pub line_height: f32,
    pub color: Color,
    /// `Some` ならこの矩形外をクリップ (glyphon の `TextBounds` で適用)。
    /// `None` で全画面描画。`Ui::with_clip_rect` が自動設定する。
    pub clip_rect: Option<Rect>,
}

/// 線分 1 本分。物理ピクセル単位。
#[derive(Debug, Clone, Copy)]
pub struct LineSegment {
    /// 始点 (物理ピクセル)
    pub a: [f32; 2],
    /// 終点 (物理ピクセル)
    pub b: [f32; 2],
    pub color: Color,
}

/// 線分の集まり 1 バッチ分。同じ `line_width_px` と `clip_rect` を共有する。
///
/// 1 ウィジェット = 1 バッチが基本想定 (波形ウィジェットの clip rect で縁を切る)。
#[derive(Debug, Clone, Default)]
pub struct LineBatch {
    pub segments: Vec<LineSegment>,
    pub line_width_px: f32,
    /// `Some` ならこの矩形外を scissor で切り捨てる。
    pub clip_rect: Option<Rect>,
}

/// 描画 primitive 1 つ分。`Scene::primitives` は **call order** でこれを並べる。
#[derive(Debug, Clone)]
pub enum Primitive {
    Rect(RectCommand),
    Glyph(GlyphArea),
    Line(LineBatch),
}

/// 1 フレームの全描画コマンド (call-order interleave)。
///
/// 2-pass 描画 (M7+ popup 対応):
/// - **base pass**: `primitives` を call order で walk、同 type 連続 primitive を 1 run に
///   batch、各 run ごとに drawcall。run 順 = z-order。
/// - **popup pass**: `popup_primitives` を同様に walk、base pass の上に再 render
///   (`LoadOp::Load`)。popup 内 primitive は base 内 primitive の最前面に出る。
#[derive(Debug)]
pub struct Scene {
    pub clear_color: wgpu::Color,
    /// base pass の primitive 列 (call order で並ぶ、z-order を保つ)。
    pub primitives: Vec<Primitive>,
    /// popup pass の primitive 列。
    pub popup_primitives: Vec<Primitive>,
}

impl Scene {
    pub fn new() -> Self {
        Self {
            clear_color: wgpu::Color { r: 0.05, g: 0.05, b: 0.06, a: 1.0 },
            primitives: Vec::new(),
            popup_primitives: Vec::new(),
        }
    }

    pub fn clear(&mut self) {
        self.primitives.clear();
        self.popup_primitives.clear();
    }

    pub fn push_rect(&mut self, cmd: RectCommand) {
        self.primitives.push(Primitive::Rect(cmd));
    }

    pub fn push_text(&mut self, area: GlyphArea) {
        self.primitives.push(Primitive::Glyph(area));
    }

    pub fn push_lines(&mut self, batch: LineBatch) {
        self.primitives.push(Primitive::Line(batch));
    }

    // ---- Test / debug helpers (旧 `scene.rects.len()` 等の代替) ----

    /// base pass の rect primitive の数。
    #[must_use]
    pub fn rect_count(&self) -> usize {
        self.primitives.iter().filter(|p| matches!(p, Primitive::Rect(_))).count()
    }

    /// base pass の glyph primitive の数。
    #[must_use]
    pub fn glyph_count(&self) -> usize {
        self.primitives.iter().filter(|p| matches!(p, Primitive::Glyph(_))).count()
    }

    /// base pass の line batch primitive の数。
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.primitives.iter().filter(|p| matches!(p, Primitive::Line(_))).count()
    }

    /// base pass の rect primitive を call order で iterate。
    pub fn iter_rects(&self) -> impl Iterator<Item = &RectCommand> {
        self.primitives.iter().filter_map(|p| match p {
            Primitive::Rect(c) => Some(c),
            _ => None,
        })
    }

    /// base pass の glyph primitive を call order で iterate。
    pub fn iter_glyphs(&self) -> impl Iterator<Item = &GlyphArea> {
        self.primitives.iter().filter_map(|p| match p {
            Primitive::Glyph(g) => Some(g),
            _ => None,
        })
    }

    /// base pass の line batch を call order で iterate。
    pub fn iter_lines(&self) -> impl Iterator<Item = &LineBatch> {
        self.primitives.iter().filter_map(|p| match p {
            Primitive::Line(l) => Some(l),
            _ => None,
        })
    }

    /// popup pass の rect primitive の数。
    #[must_use]
    pub fn popup_rect_count(&self) -> usize {
        self.popup_primitives.iter().filter(|p| matches!(p, Primitive::Rect(_))).count()
    }

    /// popup pass の glyph primitive の数。
    #[must_use]
    pub fn popup_glyph_count(&self) -> usize {
        self.popup_primitives.iter().filter(|p| matches!(p, Primitive::Glyph(_))).count()
    }

    /// popup pass の line batch の数。
    #[must_use]
    pub fn popup_line_count(&self) -> usize {
        self.popup_primitives.iter().filter(|p| matches!(p, Primitive::Line(_))).count()
    }

    /// popup pass の rect primitive を call order で iterate。
    pub fn iter_popup_rects(&self) -> impl Iterator<Item = &RectCommand> {
        self.popup_primitives.iter().filter_map(|p| match p {
            Primitive::Rect(c) => Some(c),
            _ => None,
        })
    }

    /// popup pass の glyph primitive を call order で iterate。
    pub fn iter_popup_glyphs(&self) -> impl Iterator<Item = &GlyphArea> {
        self.popup_primitives.iter().filter_map(|p| match p {
            Primitive::Glyph(g) => Some(g),
            _ => None,
        })
    }

    /// test で `scene.rects_vec()[0]` のような index access を可能にする helper (RectCommand を copy)。
    #[must_use]
    pub fn rects_vec(&self) -> Vec<RectCommand> {
        self.iter_rects().copied().collect()
    }

    /// test 用 helper (popup rect)。
    #[must_use]
    pub fn popup_rects_vec(&self) -> Vec<RectCommand> {
        self.iter_popup_rects().copied().collect()
    }
}

impl Default for Scene {
    fn default() -> Self {
        Self::new()
    }
}
