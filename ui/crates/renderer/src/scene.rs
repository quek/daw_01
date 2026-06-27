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
//!
//! ## M12 Phase 53: GlyphArea / LineBatch の重コンテナを `Arc` 化
//!
//! `Primitive::clone()` は `with_widget_node` の cache hit/miss 経路 (scenegraph) で
//! `cached.primitives.iter().cloned()` / `to_vec()` 経由で毎フレーム発火する。
//! 旧設計の `GlyphArea::text: String` / `LineBatch::segments: Vec<LineSegment>` だと
//! cache hit ごとに String alloc + Vec alloc が widget 数 × primitive 数だけ発火し、
//! 「cache hit が free」という前提が崩れていた (perf_review_2026-05-04 P0-1)。
//!
//! `Arc<str>` / `Arc<[LineSegment]>` 化で `Primitive::clone()` は **refcount の bump
//! のみ**になり、heavy な primitive list の clone コストが消える。`&Arc<T>` は `&T`
//! に deref できるので renderer pipeline 側 (`area.text.hash()` / `&area.text` /
//! `&batch.segments` 等) は無変更で動く。構築側は `text: "...".into()` /
//! `segments: vec![...].into()` のように `.into()` を 1 つ追加するだけ。

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

    /// 同じ RGB で alpha だけ差し替えた色。半透明 wash / overlay を **元トークンから派生**
    /// させ、値の再宣言 (SSoT 破り) を避けるためのヘルパ。
    #[must_use]
    pub const fn with_alpha(self, a: f32) -> Self {
        Self { r: self.r, g: self.g, b: self.b, a }
    }

    /// `self` と `other` を `t` (0.0..=1.0) で線形補間する (RGBA 各成分)。
    /// `t=0` で `self`、`t=1` で `other`。hover/pressed 等の派生状態を
    /// token から計算するのに使う ([`lighten`](Self::lighten) / [`darken`](Self::darken))。
    #[must_use]
    pub fn lerp(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let m = |a: f32, b: f32| a + (b - a) * t;
        Self {
            r: m(self.r, other.r),
            g: m(self.g, other.g),
            b: m(self.b, other.b),
            a: m(self.a, other.a),
        }
    }

    /// 白方向へ `t` だけ lerp (RGB のみ持ち上げ、alpha 保持)。hover 状態の標準導出。
    #[must_use]
    pub fn lighten(self, t: f32) -> Self {
        let lit = Self::WHITE.with_alpha(self.a);
        self.lerp(lit, t)
    }

    /// 黒方向へ `t` だけ lerp (RGB のみ落とし、alpha 保持)。pressed 状態の標準導出。
    #[must_use]
    pub fn darken(self, t: f32) -> Self {
        let dark = Self::BLACK.with_alpha(self.a);
        self.lerp(dark, t)
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

/// M14 Phase 122 (daw_01 #097): [`GlyphArea`] の **box 内水平アライメント**。
///
/// `box_width` が `Some(w)` のとき、 shaping した実測 advance 幅 `tw` を `[left, left+w]` の
/// 範囲内で配置する基準。 `box_width == None` または `Left` のときは `left` がそのまま描画原点
/// (= 既存挙動、 byte 完全互換)。 実測 advance を使うので半角・全角混在 (CJK) でも字幅推定の
/// ズレが出ない (caller が `font_size * 文字数 * 係数` で近似する必要がない)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HAlign {
    /// `left` を描画原点にする (box 無し時と同じ既定)。
    #[default]
    Left,
    /// box 中央寄せ: `left + (box_width - tw) * 0.5`。
    Center,
    /// box 右詰め: `left + (box_width - tw)`。
    Right,
}

/// M14 Phase 122 (daw_01 #097): [`GlyphArea`] の **box 内垂直アライメント**。
///
/// `box_height` が `Some(h)` のとき、 テキストブロック高さ `th` (単一行なら `line_height`) を
/// `[top, top+h]` の範囲内で配置する基準。 `box_height == None` または `Top` のときは `top` が
/// そのまま描画原点 (= 既存挙動、 byte 完全互換)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VAlign {
    /// `top` を描画原点にする (box 無し時と同じ既定)。
    #[default]
    Top,
    /// box 中央寄せ: `top + (box_height - th) * 0.5`。
    Center,
    /// box 下詰め: `top + (box_height - th)`。
    Bottom,
}

/// テキスト描画 1 ブロック分。glyphon の `TextArea` に近い情報を持つ。
///
/// `text` は `Arc<str>` で持つ (M12 Phase 53)。`Primitive::clone()` (scenegraph cache
/// hit 経路で毎フレーム発火) を refcount のみに圧縮するため。構築側は `&str` /
/// `String` から `.into()` で渡せる。
///
/// # M14 Phase 78 (daw_01 #049): text effects (outline / shadow / blur / rotation)
///
/// `outline_*` / `shadow_*` / `rotation_radians` を「**no-op default**」 に揃えてあるため、
/// 既存 caller は 6 field を追加するだけで挙動互換 (= byte 完全互換は `has_effects` が
/// false 化される条件で保証)。 effect ありの area は内部で offscreen RGBA texture に
/// glyphon で焼いて → blur / outline / fill を 1 composite pass で合成 → Phase 71/76 の
/// `TexturedQuad` (rotation_radians 込み) で base scene に push する 4-5 pass pipeline。
#[derive(Debug, Clone)]
pub struct GlyphArea {
    /// 表示する文字列 (UTF-8)。
    pub text: std::sync::Arc<str>,
    /// クリップ矩形の左上 (物理ピクセル)。
    pub left: f32,
    pub top: f32,
    /// フォントサイズ (論理ピクセル)。スケールは Renderer 側で適用。
    pub font_size: f32,
    /// 行高さ。font_size * 1.2 等。
    pub line_height: f32,
    pub color: Color,
    /// 描画に使うフォント family 名。 `None` (または `Some("")`) で renderer default
    /// (`DEFAULT_FONT_FAMILY`)、 `Some(name)` でその family を glyphon の `Family::Name(name)` で
    /// 解決する。 `name` は
    /// [`crate::available_font_families`] が返す名前を渡せば必ず解決でき、 解決不能な名前は
    /// glyphon の system fallback に倒れる。 M14 Phase 121 (daw_01 #096): Text クリップの
    /// フォントピッカー用 per-area フォント指定。 `None` のとき従来挙動と byte 完全互換。
    pub font_family: Option<std::sync::Arc<str>>,
    /// `Some` ならこの矩形外をクリップ (glyphon の `TextBounds` で適用)。
    /// `None` で全画面描画。`Ui::with_clip_rect` が自動設定する。
    pub clip_rect: Option<Rect>,
    /// M14 Phase 78 (daw_01 #049): アウトライン色 RGBA。 `outline_width_px == 0.0` なら
    /// アウトライン無し (= `outline_color` の値は無視)。
    pub outline_color: Color,
    /// アウトライン太さ (px、 `0.0` で無効)。 NaN / ±Infinity は renderer 側で `0.0` に
    /// 正規化 (caller 責務にしない)。 軽量 9-sample 法を採用するため実用 range は 1-4 px。
    pub outline_width_px: f32,
    /// ドロップシャドウ色 RGBA。 `shadow_color.a == 0.0` なら shadow 無し (= offset / blur
    /// が non-zero でも無視)。
    pub shadow_color: Color,
    /// シャドウオフセット (`(dx, dy)` px)。 `(0, 0)` で本体真下、 `(4, 4)` で右下 4px。 NaN /
    /// ±Infinity は renderer 側で `0.0` に正規化。
    pub shadow_offset_px: (f32, f32),
    /// シャドウぼかし半径 (px、 `0.0` で hard shadow)。 `> 0.0` で separable gaussian blur
    /// (= 2-pass)。 NaN / ±Infinity は renderer 側で `0.0` に正規化。
    pub shadow_blur_px: f32,
    /// rect 中心 (`(left + width/2, top + line_height/2)`) を旋回中心とする 2D 回転
    /// (radians、 clockwise positive in screen-down y)。 `0.0` で既存挙動と byte 完全互換。
    /// NaN / ±Infinity は renderer 側で `0.0` に正規化 (Phase 76 の `TexturedQuad.
    /// rotation_radians` と同 idiom)。
    pub rotation_radians: f32,
    /// M14 Phase 122 (daw_01 #097): 水平アライメント用 box の横幅 (物理 px、 原点は `left`)。
    /// `None` = box 無し → `left` が描画原点 (現行挙動、 `align_h` は無視)。 `Some(w)` = shaping
    /// した実 advance 幅 `tw` を測り `[left, left+w]` 内で `align_h` に従って水平配置。 `tw > w`
    /// でも **clip せず両側にはみ出す** (center は対称)。 クリップは別概念の `clip_rect` で行う。
    /// NaN / ±Infinity は renderer 側で `None` 扱い (caller 責務にしない)。
    pub box_width: Option<f32>,
    /// M14 Phase 122 (daw_01 #097): 垂直アライメント用 box の高さ (物理 px、 原点は `top`)。
    /// `None` = box 無し → `top` が描画原点 (現行挙動、 `align_v` は無視)。 `Some(h)` = ブロック
    /// 高さ `th` (単一行なら `line_height`) を `[top, top+h]` 内で `align_v` 配置。 NaN / ±Infinity は
    /// renderer 側で `None` 扱い。
    pub box_height: Option<f32>,
    /// M14 Phase 122 (daw_01 #097): 水平アライメント。 default `Left` = `left` 原点 (現行)。
    /// `box_width == None` のときは無視される。
    pub align_h: HAlign,
    /// M14 Phase 122 (daw_01 #097): 垂直アライメント。 default `Top` = `top` 原点 (現行)。
    /// `box_height == None` のときは無視される。
    pub align_v: VAlign,
}

impl GlyphArea {
    /// 必須 field (`text` / `left` / `top` / `font_size` / `line_height` / `color`) のみ指定で
    /// effect 無しの GlyphArea を作る最短コンストラクタ。 clip_rect / outline / shadow /
    /// rotation はすべて no-op default。 既存挙動 byte 完全互換。
    #[must_use]
    pub fn new(
        text: std::sync::Arc<str>,
        left: f32,
        top: f32,
        font_size: f32,
        line_height: f32,
        color: Color,
    ) -> Self {
        Self {
            text,
            left,
            top,
            font_size,
            line_height,
            color,
            font_family: None,
            clip_rect: None,
            outline_color: Color::TRANSPARENT,
            outline_width_px: 0.0,
            shadow_color: Color::TRANSPARENT,
            shadow_offset_px: (0.0, 0.0),
            shadow_blur_px: 0.0,
            rotation_radians: 0.0,
            box_width: None,
            box_height: None,
            align_h: HAlign::Left,
            align_v: VAlign::Top,
        }
    }

    /// M14 Phase 78 (daw_01 #049): effect 有効判定。 全 field が no-op default なら `false`
    /// (= 既存 glyphon 直接 path を使う)、 いずれかが有効なら `true` (= offscreen composite
    /// path を使う)。 `has_effects(area) == false` の場合は既存挙動と byte 完全互換。
    #[must_use]
    pub fn has_effects(&self) -> bool {
        self.outline_width_px > 0.0
            || self.shadow_color.a > 0.0
            || self.rotation_radians != 0.0
    }

    /// 実際に shape に使う family 名。 `font_family` が `None` のとき `DEFAULT_FONT_FAMILY`。
    /// M14 Phase 121 (daw_01 #096): cache key と attrs 構築の **双方** がこの 1 関数を読むことで
    /// 「buffer の cache identity」 と「実際に描画する font」 を常に一致させる (= 同じ text+size でも
    /// font 違いは別 buffer になり、 cache collision で別 font に化けるのを防ぐ)。
    #[must_use]
    pub fn resolved_font_family(&self) -> &str {
        // `None` だけでなく `Some("")` も default 扱い。 daw_01 は font_family を `Arc<str>` の
        // `""` = default で持つので、 空文字を `Family::Name("")` (= glyphon fallback) に流さない。
        self.font_family
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(crate::pipelines::glyph::DEFAULT_FONT_FAMILY)
    }

    /// M14 Phase 122 (daw_01 #097): box + align が描画原点を `left`/`top` から動かすか。
    /// `false` のとき renderer は measure を skip して既存挙動 (byte 完全互換)。 非有限な
    /// box 寸法は `None` 扱いなので alignment を要求しない。
    #[must_use]
    pub fn needs_alignment(&self) -> bool {
        (self.box_width.is_some_and(f32::is_finite) && self.align_h != HAlign::Left)
            || (self.box_height.is_some_and(f32::is_finite) && self.align_v != VAlign::Top)
    }

    /// M14 Phase 122 (daw_01 #097): 実測テキスト寸法 (`text_w` = 最大行 advance、 `text_h` =
    /// ブロック高さ) を与えると box + align に従った描画原点 `(x, y)` を返す。 box 未指定軸 /
    /// `Left`・`Top` / 非有限 box 寸法は `left`/`top` をそのまま返す。 `text_w > box_width` でも
    /// clip せず両側にはみ出す (center は対称)。 glyph path (非 effect) と text_effect path の
    /// **双方がこの 1 関数を読む**ことで「描画原点の計算」 を SSoT 化し、 plain / offscreen で
    /// 配置が乖離しないことを保証する。
    #[must_use]
    pub fn aligned_origin(&self, text_w: f32, text_h: f32) -> (f32, f32) {
        let x = match (self.box_width.filter(|w| w.is_finite()), self.align_h) {
            (Some(w), HAlign::Center) => self.left + (w - text_w) * 0.5,
            (Some(w), HAlign::Right) => self.left + (w - text_w),
            _ => self.left,
        };
        let y = match (self.box_height.filter(|h| h.is_finite()), self.align_v) {
            (Some(h), VAlign::Center) => self.top + (h - text_h) * 0.5,
            (Some(h), VAlign::Bottom) => self.top + (h - text_h),
            _ => self.top,
        };
        (x, y)
    }
}

impl Default for GlyphArea {
    /// M14 Phase 78 (daw_01 #049): `..GlyphArea::default()` で effect 系 field の no-op 補完
    /// を可能にする。 既存 47 caller (clip_rect / 必須 fields は explicit に設定) の 6 field
    /// 追加を 1 行 (`..Default::default()`) で済ませるための idiom。
    ///
    /// `text` は `Arc::from("")` の空文字列、 numeric 0、 colors TRANSPARENT。 `Arc<str>` の
    /// `Default` impl は stable Rust に存在しないため手書き。
    fn default() -> Self {
        Self {
            text: std::sync::Arc::from(""),
            left: 0.0,
            top: 0.0,
            font_size: 0.0,
            line_height: 0.0,
            color: Color::TRANSPARENT,
            font_family: None,
            clip_rect: None,
            outline_color: Color::TRANSPARENT,
            outline_width_px: 0.0,
            shadow_color: Color::TRANSPARENT,
            shadow_offset_px: (0.0, 0.0),
            shadow_blur_px: 0.0,
            rotation_radians: 0.0,
            box_width: None,
            box_height: None,
            align_h: HAlign::Left,
            align_v: VAlign::Top,
        }
    }
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
///
/// `segments` は `Arc<[LineSegment]>` で持つ (M12 Phase 53)。`Primitive::clone()`
/// を refcount のみに圧縮するため。構築側は `Vec<LineSegment>` から `.into()` で
/// 渡せる (`segments.push(...)` で組み立てた後に 1 度だけ Arc::from で確定する)。
#[derive(Debug, Clone, Default)]
pub struct LineBatch {
    pub segments: std::sync::Arc<[LineSegment]>,
    pub line_width_px: f32,
    /// `Some` ならこの矩形外を scissor で切り捨てる。
    pub clip_rect: Option<Rect>,
}

/// Renderer-local な texture identifier (M14 Phase 71 / daw_01 #043)。
///
/// [`Renderer::create_texture`](crate::Renderer::create_texture) /
/// [`OffscreenRenderer::create_texture`](crate::OffscreenRenderer::create_texture) で発行され、
/// 同 Renderer instance 内でのみ valid (= 別 Renderer / 別 window 間で共有してはならない)。
/// destroy 済 handle に対する push は描画 no-op (panic しない)。
///
/// `Copy + Eq + Hash` なので daw_01 側で `HashMap<ClipId, TextureHandle>` 等を持ちやすい。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextureHandle(std::num::NonZeroU32);

impl TextureHandle {
    /// `TextureStore` 内部用コンストラクタ (renderer crate のみが呼ぶ)。
    #[doc(hidden)]
    #[must_use]
    pub fn from_raw(id: std::num::NonZeroU32) -> Self {
        Self(id)
    }

    /// `TextureStore` の内部 lookup 用 (renderer crate のみが呼ぶ)。
    #[doc(hidden)]
    #[must_use]
    pub fn raw_id(self) -> std::num::NonZeroU32 {
        self.0
    }

    /// debug / test 用の raw 値。 `from_raw` の逆。
    #[must_use]
    pub fn raw(self) -> u32 {
        self.0.get()
    }
}

/// 1 つの textured quad (M14 Phase 71 / daw_01 #043)。 video frame thumbnail / preview
/// composite で使用。 `texture` が destroy 済なら描画 no-op (panic しない)。
#[derive(Debug, Clone, Copy)]
pub struct TexturedQuad {
    /// 物理ピクセル座標 (rect / line / glyph と同 idiom)。
    pub rect: Rect,
    /// `Renderer::create_texture` で得た handle。
    pub texture: TextureHandle,
    /// `0.0` = 完全透明、 `1.0` = 完全不透明。 standard alpha blend (OVER) で composite。
    pub alpha: f32,
    /// texture 内サンプル領域 (UV `0.0..=1.0`)。 `(0,0)-(1,1)` で全 texture。 crop 用途で
    /// 部分表示する場合に絞り込む (= thumbnail で frame の一部だけ表示)。
    pub uv_min: (f32, f32),
    pub uv_max: (f32, f32),
    /// `Some` ならこの矩形外を scissor で切り捨てる。 `Ui::with_clip_rect` が自動設定する。
    pub clip_rect: Option<Rect>,
    /// M14 Phase 76 (daw_01 #047): rect 中心を旋回中心とする 2D 回転 (radians、 clockwise
    /// positive)。 `0.0` で既存の axis-aligned 描画と byte 完全互換。 NaN / ±Infinity は
    /// renderer 側で `0.0` に正規化 (caller 責務にしない)。 回転は **pixel 空間** で実施
    /// するため non-square rect (w ≠ h) でも aspect 維持で正しく回転する。
    pub rotation_radians: f32,
    /// M14 Phase 92 (daw_01 #064): `rotation_radians` の旋回中心 (pivot)。 `rect` 左上を
    /// 原点とした **物理ピクセル相対**座標 `(px, py)`。 `None` は rect 中心 `(w/2, h/2)` を
    /// 意味し、 Phase 76 までの挙動と byte 完全互換。 立ち絵 group transform (#063) で合成済
    /// 1 枚に親 affine の任意アンカー回転をかける用途。 成分が NaN / ±Infinity の場合は
    /// renderer 側で中心 pivot に fallback (caller 責務にしない)。
    pub rotation_pivot: Option<(f32, f32)>,
}

impl TexturedQuad {
    /// UV 全域 + alpha=1.0 + clip なし + rotation 0 + 中心 pivot の最短コンストラクタ。
    #[must_use]
    pub fn new(rect: Rect, texture: TextureHandle) -> Self {
        Self {
            rect,
            texture,
            alpha: 1.0,
            uv_min: (0.0, 0.0),
            uv_max: (1.0, 1.0),
            clip_rect: None,
            rotation_radians: 0.0,
            rotation_pivot: None,
        }
    }
}

/// 描画 primitive 1 つ分。`Scene::primitives` は **call order** でこれを並べる。
#[derive(Debug, Clone)]
pub enum Primitive {
    Rect(RectCommand),
    Glyph(GlyphArea),
    Line(LineBatch),
    /// M14 Phase 71 (daw_01 #043): video frame / thumbnail 用 textured quad。
    Texture(TexturedQuad),
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
            clear_color: crate::theme::WINDOW_BG.to_wgpu(),
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

    /// M14 Phase 71 (daw_01 #043): textured quad を base pass に積む。
    /// popup pass には MVP 非対応 (= popup から呼んでも safety net で skip される)。
    pub fn push_textured_quad(&mut self, quad: TexturedQuad) {
        self.primitives.push(Primitive::Texture(quad));
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

    /// M14 Phase 71: base pass の textured quad の数。
    #[must_use]
    pub fn texture_count(&self) -> usize {
        self.primitives.iter().filter(|p| matches!(p, Primitive::Texture(_))).count()
    }

    /// M14 Phase 71: base pass の textured quad を call order で iterate。
    pub fn iter_textures(&self) -> impl Iterator<Item = &TexturedQuad> {
        self.primitives.iter().filter_map(|p| match p {
            Primitive::Texture(q) => Some(q),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    fn handle(id: u32) -> TextureHandle {
        TextureHandle::from_raw(NonZeroU32::new(id).unwrap())
    }

    #[test]
    fn push_textured_quad_adds_texture_primitive() {
        let mut s = Scene::new();
        let h = handle(1);
        s.push_textured_quad(TexturedQuad::new(Rect::new(0.0, 0.0, 10.0, 10.0), h));
        assert_eq!(s.texture_count(), 1);
        assert_eq!(s.rect_count(), 0);
        assert_eq!(s.glyph_count(), 0);
        assert_eq!(s.line_count(), 0);
    }

    #[test]
    fn textured_quad_default_uv_full_and_alpha_one() {
        let q = TexturedQuad::new(Rect::new(1.0, 2.0, 3.0, 4.0), handle(7));
        // literal 1.0 / 0.0 を入れて取り出すだけなので bit-exact 比較で OK (clippy::float_cmp 不要)
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(q.alpha, 1.0);
            assert_eq!(q.uv_min, (0.0, 0.0));
            assert_eq!(q.uv_max, (1.0, 1.0));
            // M14 Phase 76 (daw_01 #047): default 0.0 で既存 axis-aligned 描画と byte 互換
            assert_eq!(q.rotation_radians, 0.0);
        }
        assert!(q.clip_rect.is_none());
        // M14 Phase 92 (daw_01 #064): default None = rect 中心 pivot (Phase 76 と byte 互換)
        assert!(q.rotation_pivot.is_none());
    }

    #[test]
    fn texture_handle_raw_round_trip() {
        let h = handle(42);
        assert_eq!(h.raw(), 42);
        assert_eq!(h.raw_id().get(), 42);
    }

    /// M14 Phase 71 (#043): primitives は call order で interleave、 type 跨ぎでも保たれる。
    #[test]
    fn texture_primitive_preserves_call_order_with_other_kinds() {
        let mut s = Scene::new();
        s.push_rect(RectCommand::uniform_radius(
            Rect::new(0.0, 0.0, 1.0, 1.0),
            Color::WHITE,
            0.0,
        ));
        s.push_textured_quad(TexturedQuad::new(Rect::new(0.0, 0.0, 1.0, 1.0), handle(1)));
        s.push_rect(RectCommand::uniform_radius(
            Rect::new(0.0, 0.0, 1.0, 1.0),
            Color::BLACK,
            0.0,
        ));
        assert!(matches!(s.primitives[0], Primitive::Rect(_)));
        assert!(matches!(s.primitives[1], Primitive::Texture(_)));
        assert!(matches!(s.primitives[2], Primitive::Rect(_)));
    }

    // ============================================================
    // M14 Phase 122 (daw_01 #097): box-based align
    // ============================================================

    /// `left`/`top` を 100/50 に置き、 box / align を後から差し替えるテスト用 area。
    fn align_area() -> GlyphArea {
        GlyphArea { left: 100.0, top: 50.0, ..GlyphArea::default() }
    }

    /// default は box 無し + Left/Top = 既存 caller と byte 完全互換。
    #[test]
    fn glyph_area_default_no_box_left_top() {
        let a = GlyphArea::default();
        assert!(a.box_width.is_none());
        assert!(a.box_height.is_none());
        assert_eq!(a.align_h, HAlign::Left);
        assert_eq!(a.align_v, VAlign::Top);
        assert!(!a.needs_alignment());
        // measure を渡しても left/top をそのまま返す (= origin 不動)。
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(a.aligned_origin(123.0, 45.0), (0.0, 0.0));
        }
    }

    /// box 指定でも Left/Top は origin 不動 + needs_alignment false (measure skip)。
    #[test]
    fn box_with_left_top_is_noop() {
        let a = GlyphArea { box_width: Some(300.0), box_height: Some(80.0), ..align_area() };
        assert!(!a.needs_alignment());
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(a.aligned_origin(60.0, 20.0), (100.0, 50.0));
        }
    }

    /// HAlign::Center は `left + (w - tw)/2`。 全角想定で tw=200, box=300 → 100 + 50 = 150。
    #[test]
    fn h_center_uses_measured_advance() {
        let a = GlyphArea { box_width: Some(300.0), align_h: HAlign::Center, ..align_area() };
        assert!(a.needs_alignment());
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(a.aligned_origin(200.0, 20.0).0, 150.0);
            // 縦は box 無し → top 不動。
            assert_eq!(a.aligned_origin(200.0, 20.0).1, 50.0);
        }
    }

    /// HAlign::Right は `left + (w - tw)`。 box=300, tw=120 → 100 + 180 = 280。
    #[test]
    fn h_right_flush() {
        let a = GlyphArea { box_width: Some(300.0), align_h: HAlign::Right, ..align_area() };
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(a.aligned_origin(120.0, 20.0).0, 280.0);
        }
    }

    /// tw > w でも clip せず center は対称にはみ出す (origin が left より左)。
    #[test]
    fn h_center_overflow_symmetric() {
        let a = GlyphArea { box_width: Some(100.0), align_h: HAlign::Center, ..align_area() };
        // tw=160, box=100 → 100 + (100-160)/2 = 100 - 30 = 70。
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(a.aligned_origin(160.0, 20.0).0, 70.0);
        }
    }

    /// VAlign::Center / Bottom。 box_h=80, th=20 → center top=50+30=80、 bottom top=50+60=110。
    #[test]
    fn v_center_and_bottom() {
        let c = GlyphArea { box_height: Some(80.0), align_v: VAlign::Center, ..align_area() };
        let b = GlyphArea { box_height: Some(80.0), align_v: VAlign::Bottom, ..align_area() };
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(c.aligned_origin(40.0, 20.0).1, 80.0);
            assert_eq!(b.aligned_origin(40.0, 20.0).1, 110.0);
        }
    }

    /// 水平・垂直を同時に center (daw_01 の text overlay 標準ケース)。
    #[test]
    fn h_and_v_center_combined() {
        let a = GlyphArea {
            box_width: Some(300.0),
            box_height: Some(80.0),
            align_h: HAlign::Center,
            align_v: VAlign::Center,
            ..align_area()
        };
        assert!(a.needs_alignment());
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(a.aligned_origin(200.0, 20.0), (150.0, 80.0));
        }
    }

    /// 非有限な box 寸法は `None` 扱い: needs_alignment false + origin 不動 (NaN 伝播しない)。
    #[test]
    fn non_finite_box_is_ignored() {
        let a = GlyphArea {
            box_width: Some(f32::NAN),
            box_height: Some(f32::INFINITY),
            align_h: HAlign::Center,
            align_v: VAlign::Bottom,
            ..align_area()
        };
        assert!(!a.needs_alignment());
        let (x, y) = a.aligned_origin(50.0, 20.0);
        #[allow(clippy::float_cmp)]
        {
            assert_eq!((x, y), (100.0, 50.0));
        }
        assert!(x.is_finite() && y.is_finite());
    }
}
