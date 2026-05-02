//! `Scene` — 1 フレーム分の描画コマンド (DisplayList)。
//!
//! 上位層は `Scene::push_rect` 等でコマンドを積み、`Renderer::render(&scene)` を呼ぶ。
//! 後の M3 で内部 scenegraph + 差分検出に置き換わるが、M1 では毎フレーム積み直しで OK。

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

/// 1 フレームの全描画コマンド。
#[derive(Debug)]
pub struct Scene {
    pub clear_color: wgpu::Color,
    pub rects: Vec<RectCommand>,
    pub glyph_areas: Vec<GlyphArea>,
    pub line_batches: Vec<LineBatch>,
}

impl Scene {
    pub fn new() -> Self {
        Self {
            clear_color: wgpu::Color { r: 0.05, g: 0.05, b: 0.06, a: 1.0 },
            rects: Vec::new(),
            glyph_areas: Vec::new(),
            line_batches: Vec::new(),
        }
    }

    pub fn clear(&mut self) {
        self.rects.clear();
        self.glyph_areas.clear();
        self.line_batches.clear();
    }

    pub fn push_rect(&mut self, cmd: RectCommand) {
        self.rects.push(cmd);
    }

    pub fn push_text(&mut self, area: GlyphArea) {
        self.glyph_areas.push(area);
    }

    pub fn push_lines(&mut self, batch: LineBatch) {
        self.line_batches.push(batch);
    }
}

impl Default for Scene {
    fn default() -> Self {
        Self::new()
    }
}
