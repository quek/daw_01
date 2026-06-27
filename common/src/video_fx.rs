//! 内蔵 GPU 映像効果フレームワーク (docs/plan_video_fx.md §1)。
//!
//! 各効果 = **WGSL fragment パス列 + 宣言的パラメータ表** (ISF/OBS 流)。本モジュールは
//! その **宣言的 Single Source of Truth** で、以下が参照する:
//! - [`crate::plugin_db::builtin_descriptors`] — プラグインピッカに `builtin.video.*`
//!   を列挙する (id / name / category / video ports)。
//! - GUI 効果実行基盤 (`daw_gui/src/video_fx/`) — param → uniform 配線とパス実行。
//!   **GPU パス実行と preview/export 合成統合は gui_01 の効果パス API
//!   (docs/gui_01_conversation.md #111) landing 待ち**。
//! - インスペクタ UI — param の表示レンジ (min/max/unit)。
//!
//! ## パラメータ値ドメイン (重要)
//!
//! 映像効果の scalar param は **正規化 0..=1 plain ドメインで保存・自動化・変調**する
//! (image の x/y/opacity と同じ)。これにより [`crate::automation::plain_to_norm`] /
//! `norm_to_plain` は `PluginParam` に対して恒等のままで正しく動く (= 変調 depth の
//! 意味が全 target で一貫、追加の正規化配線が不要)。
//!
//! [`ParamKind::Scalar`] の `min` / `max` / `default` / `unit` は **表示・シェーダの
//! 実レンジ**を表すメタ情報:
//! - 効果実行基盤は保存値 `v` (0..=1) を `min + v*(max-min)` に展開して uniform に流す。
//! - インスペクタは `min..=max` を `unit` 付きで表示し、スクラブで 0..=1 へ逆写像する。
//!
//! ## WGSL effect-body 規約
//!
//! [`VideoFxPass::wgsl`] は **effect 関数 1 個のみ**を含む断片で、効果実行基盤が
//! 標準ハーネス (全画面三角形の頂点シェーダ + bind group + `@fragment` エントリ) で
//! 包んでモジュール化する。body が前提にできるのは:
//! - `fn effect(uv: vec2<f32>, src: vec4<f32>) -> vec4<f32>` を定義すること。
//!   `uv` は 0..=1 のテクスチャ座標、`src` は入力画像の当該ピクセル (sRGB→linear 済 RGBA)。
//! - `P.<key>`: 各 [`VideoFxParam::key`] の **実レンジ値** (f32。Bool は 0.0/1.0)。
//! - `P.resolution: vec2<f32>` (出力 px)、`P.texel: vec2<f32>` (1/resolution)、
//!   `P.time: f32` (秒)。
//! - `sample(uv2)`: 入力画像を任意座標でサンプル (近傍参照効果用)。
//! - `history(uv2)`: [`VideoFxDef::needs_history`] が true のとき、前フレーム出力をサンプル。
//!
//! ハーネス生成と GPU 実行は効果実行基盤側 (gui_01 #111 landing 後) が担う。本モジュールは
//! データ (型 + カタログ) のみを持ち、wgpu 依存を持たない。

/// 効果カテゴリ (docs/plan_video_fx.md §4 / §6 の ISF タクソノミー)。
/// プラグインピッカのカテゴリ別ブラウザと feature タグに使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoFxCategory {
    /// 色補正・グレード (明度/コントラスト/彩度/色相/LUT/ビネット…)。
    Color,
    /// ブラー・シャープ (ガウス/ボックス/方向/ブルーム…)。
    Blur,
    /// 歪み・ワープ (モザイク/ミラー/リップル/カレイドスコープ…)。
    Distort,
    /// スタイライズ (エッジ/ポスタライズ/しきい値/ハーフトーン…)。
    Stylize,
    /// キーイング (クロマ/ルマキー/スピル抑制)。
    Key,
    /// ノイズ・質感 (フィルムグレイン/VHS/光漏れ…)。
    Noise,
    /// 時間・フィードバック (エコー/残像トレイル…)。要 [`VideoFxDef::needs_history`]。
    Time,
    /// 座標変換 (Transform 一本化、docs/plan_video_fx.md §5)。
    Transform,
}

impl VideoFxCategory {
    /// インスペクタ / ピッカ表示用ラベル。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Color => "Color",
            Self::Blur => "Blur / Sharpen",
            Self::Distort => "Distort",
            Self::Stylize => "Stylize",
            Self::Key => "Keying",
            Self::Noise => "Noise / Texture",
            Self::Time => "Time / Feedback",
            Self::Transform => "Transform",
        }
    }

    /// `PluginEntry::features` に載せる安定タグ (ピッカのカテゴリ分類に使う)。
    /// `video-effect` 共通タグに加えてカテゴリ別タグを 1 つ付ける。
    #[must_use]
    pub fn feature_tag(self) -> &'static str {
        match self {
            Self::Color => "video-color",
            Self::Blur => "video-blur",
            Self::Distort => "video-distort",
            Self::Stylize => "video-stylize",
            Self::Key => "video-key",
            Self::Noise => "video-noise",
            Self::Time => "video-time",
            Self::Transform => "video-transform",
        }
    }
}

/// scalar param の表示単位 (インスペクタの数値整形に使う)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// 単位なし (係数・比率など)。
    None,
    /// ピクセル。
    Px,
    /// 度 (角度)。
    Deg,
    /// パーセント (0..100 表示)。
    Pct,
}

/// パラメータの種別と表示レンジ。保存値は常に 0..=1 正規化 plain (モジュール doc 参照)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParamKind {
    /// 線形連続値。`min`/`max`/`default` は **表示・シェーダの実レンジ**。
    Scalar {
        min: f32,
        max: f32,
        default: f32,
        unit: Unit,
    },
    /// 対数連続値（`min`/`max` は **正の実レンジ**、正規化は log 空間）。スケール系
    /// （0.1..10 で 0.5 = 等倍）のように乗法的な param 用。Transform
    /// device の ScaleX/Y が既存 GroupTransform と同じ log 正規化を保つため
    /// （[`crate::automation`] の plain_to_norm log 写像と一致、automation curve drift なし）。
    LogScalar {
        min: f32,
        max: f32,
        default: f32,
        unit: Unit,
    },
    /// オン/オフ。シェーダには 0.0/1.0 として渡る。
    Bool { default: bool },
}

impl ParamKind {
    /// 実レンジ `(min, max)`。Bool は `(0.0, 1.0)`。効果実行基盤が
    /// 保存値 0..=1 → 実値、インスペクタが逆写像に使う。
    #[must_use]
    pub fn range(&self) -> (f32, f32) {
        match *self {
            Self::Scalar { min, max, .. } | Self::LogScalar { min, max, .. } => (min, max),
            Self::Bool { .. } => (0.0, 1.0),
        }
    }

    /// **正規化 0..=1 plain** での既定値 (automation lane の `default_value` 初期値)。
    #[must_use]
    pub fn default_norm(&self) -> f64 {
        match *self {
            Self::Scalar { min, max, default, .. } => {
                if (max - min).abs() < f32::EPSILON {
                    0.0
                } else {
                    f64::from((default - min) / (max - min)).clamp(0.0, 1.0)
                }
            }
            Self::LogScalar { min, max, default, .. } => {
                if min <= 0.0 || max <= 0.0 || (max - min).abs() < f32::EPSILON {
                    0.0
                } else {
                    (f64::from(default / min).ln() / f64::from(max / min).ln()).clamp(0.0, 1.0)
                }
            }
            Self::Bool { default } => {
                if default {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }

    /// 正規化値 `norm` (0..=1) を実レンジ値へ展開 (シェーダ uniform / Transform 値用)。
    #[must_use]
    pub fn norm_to_real(&self, norm: f64) -> f32 {
        #[allow(clippy::cast_possible_truncation)]
        let n = norm.clamp(0.0, 1.0) as f32;
        match *self {
            Self::LogScalar { min, max, .. } if min > 0.0 && max > 0.0 => {
                min * (max / min).powf(n)
            }
            _ => {
                let (min, max) = self.range();
                min + n * (max - min)
            }
        }
    }

    /// 実レンジ値 `real` を正規化 0..=1 へ逆写像 (`norm_to_real` の逆)。インスペクタの
    /// スクラブ編集が「表示の実値 → lane の保存値 (0..=1)」へ戻すのに使う。
    #[must_use]
    pub fn real_to_norm(&self, real: f32) -> f64 {
        match *self {
            Self::LogScalar { min, max, .. } if min > 0.0 && max > 0.0 && real > 0.0 => {
                (f64::from(real / min).ln() / f64::from(max / min).ln()).clamp(0.0, 1.0)
            }
            Self::Bool { .. } => {
                if real >= 0.5 {
                    1.0
                } else {
                    0.0
                }
            }
            _ => {
                let (min, max) = self.range();
                if (max - min).abs() < f32::EPSILON {
                    0.0
                } else {
                    f64::from((real - min) / (max - min)).clamp(0.0, 1.0)
                }
            }
        }
    }

    /// このパラメータが log スケール (`LogScalar`) か。modulation domain の選択に使う。
    #[must_use]
    pub fn is_log(&self) -> bool {
        matches!(self, Self::LogScalar { .. })
    }
}

/// 1 個の効果パラメータ (manifest 行)。`id` は `PluginParam.param_id` に対応。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoFxParam {
    /// 安定 param id (効果内で一意、0 起番)。`AutomationTarget::PluginParam.param_id`。
    pub id: u32,
    /// WGSL 識別子 (effect-body が `P.<key>` で参照)。snake_case・効果内一意。
    pub key: &'static str,
    /// 表示名 (インスペクタ / 自動化レーンラベル)。
    pub name: &'static str,
    /// 種別と表示レンジ。
    pub kind: ParamKind,
}

/// 1 個の WGSL パスの種別 (効果実行基盤のパイプライン選択用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassKind {
    /// 入力 1 枚 → 出力 1 枚のステートレス fragment パス。
    Simple,
    /// 分離ブラー (H または V)。bloom/glow/soft-vignette の共有プリミティブ
    /// (docs/plan_video_fx.md §1.2)。`horizontal=true` が水平パス。
    SeparableBlur { horizontal: bool },
    /// フィードバック履歴を読むパス。前フレーム出力を `history(uv)` で参照
    /// (echo/残像トレイル/VHS)。[`VideoFxDef::needs_history`] と対。
    History,
}

/// 1 個の WGSL パス。`wgsl` は effect-body 1 個 (モジュール doc の規約参照)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFxPass {
    /// effect-body WGSL (`fn effect(uv, src) -> vec4<f32>` を定義)。
    pub wgsl: &'static str,
    pub kind: PassKind,
}

/// 1 個の内蔵映像効果の定義 (manifest)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoFxDef {
    /// `PluginInstance.plugin_id`。`builtin.video.<x>` 形式。
    pub id: &'static str,
    /// ピッカ / インスペクタ表示名。
    pub name: &'static str,
    pub category: VideoFxCategory,
    /// パラメータ表 (id 0 起番)。
    pub params: &'static [VideoFxParam],
    /// 1..N の WGSL パス (適用順)。
    pub passes: &'static [VideoFxPass],
    /// フィードバック履歴ターゲットを要するか (echo/残像系)。
    pub needs_history: bool,
}

impl VideoFxDef {
    /// `param_id` の param を引く。
    #[must_use]
    pub fn param(&self, param_id: u32) -> Option<&'static VideoFxParam> {
        self.params.iter().find(|p| p.id == param_id)
    }
}

// ============================================================================
// カタログ (初期波)
// ============================================================================
//
// docs/plan_video_fx.md §4 の完全目標セットのうち、まず確実に正しい色補正系から
// 着手する。残りのカテゴリ (ブラー/歪み/スタイライズ/ノイズ/キーイング/時間) は
// 効果実行基盤 (gui_01 #111 landing) と同時に、各波で visual smoke test しながら
// 追加する (plan §12 phase 6 の波状実装)。

/// 色補正・グレード (露出/明度/コントラスト/彩度/ガンマ)。代表的 NLE の
/// プライマリグレードに相当する多パラメータ 1 効果。
const COLOR_GRADE: VideoFxDef = VideoFxDef {
    id: "builtin.video.color_grade",
    name: "Color Grade",
    category: VideoFxCategory::Color,
    params: &[
        VideoFxParam {
            id: 0,
            key: "exposure",
            name: "Exposure",
            kind: ParamKind::Scalar { min: -4.0, max: 4.0, default: 0.0, unit: Unit::None },
        },
        VideoFxParam {
            id: 1,
            key: "brightness",
            name: "Brightness",
            kind: ParamKind::Scalar { min: -1.0, max: 1.0, default: 0.0, unit: Unit::None },
        },
        VideoFxParam {
            id: 2,
            key: "contrast",
            name: "Contrast",
            kind: ParamKind::Scalar { min: 0.0, max: 2.0, default: 1.0, unit: Unit::None },
        },
        VideoFxParam {
            id: 3,
            key: "saturation",
            name: "Saturation",
            kind: ParamKind::Scalar { min: 0.0, max: 2.0, default: 1.0, unit: Unit::None },
        },
        VideoFxParam {
            id: 4,
            key: "gamma",
            name: "Gamma",
            kind: ParamKind::Scalar { min: 0.1, max: 4.0, default: 1.0, unit: Unit::None },
        },
    ],
    passes: &[VideoFxPass {
        kind: PassKind::Simple,
        wgsl: r#"
fn effect(uv: vec2<f32>, src: vec4<f32>) -> vec4<f32> {
    var c = src.rgb;
    // 露出 (stops): 2^exposure 倍
    c = c * exp2(P.exposure);
    // 明度: 加算
    c = c + vec3<f32>(P.brightness);
    // コントラスト: 0.5 中心
    c = (c - vec3<f32>(0.5)) * P.contrast + vec3<f32>(0.5);
    // 彩度: 輝度方向に mix
    let luma = dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
    c = mix(vec3<f32>(luma), c, P.saturation);
    // ガンマ
    c = pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / max(P.gamma, 0.001)));
    return vec4<f32>(c, src.a);
}
"#,
    }],
    needs_history: false,
};

/// 色相回転 + 色温度/ティント。
const HUE_TEMP: VideoFxDef = VideoFxDef {
    id: "builtin.video.hue_temp",
    name: "Hue / Temperature",
    category: VideoFxCategory::Color,
    params: &[
        VideoFxParam {
            id: 0,
            key: "hue",
            name: "Hue",
            kind: ParamKind::Scalar { min: -180.0, max: 180.0, default: 0.0, unit: Unit::Deg },
        },
        VideoFxParam {
            id: 1,
            key: "temperature",
            name: "Temperature",
            kind: ParamKind::Scalar { min: -1.0, max: 1.0, default: 0.0, unit: Unit::None },
        },
        VideoFxParam {
            id: 2,
            key: "tint",
            name: "Tint",
            kind: ParamKind::Scalar { min: -1.0, max: 1.0, default: 0.0, unit: Unit::None },
        },
    ],
    passes: &[VideoFxPass {
        kind: PassKind::Simple,
        wgsl: r#"
fn effect(uv: vec2<f32>, src: vec4<f32>) -> vec4<f32> {
    var c = src.rgb;
    // 色温度 (赤↔青) と ティント (緑↔マゼンタ) の単純なゲイン。
    c = c + vec3<f32>(P.temperature, P.tint, -P.temperature) * 0.5;
    // 色相回転 (YIQ 空間で I/Q を回す近似)。
    let a = radians(P.hue);
    let ca = cos(a);
    let sa = sin(a);
    let y = dot(c, vec3<f32>(0.299, 0.587, 0.114));
    let i = dot(c, vec3<f32>(0.596, -0.274, -0.322));
    let q = dot(c, vec3<f32>(0.211, -0.523, 0.312));
    let i2 = i * ca - q * sa;
    let q2 = i * sa + q * ca;
    c = vec3<f32>(
        y + 0.956 * i2 + 0.621 * q2,
        y - 0.272 * i2 - 0.647 * q2,
        y - 1.106 * i2 + 1.703 * q2,
    );
    return vec4<f32>(clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)), src.a);
}
"#,
    }],
    needs_history: false,
};

/// 白黒 / セピア (脱色量 + セピア量)。
const DUOTONE: VideoFxDef = VideoFxDef {
    id: "builtin.video.duotone",
    name: "Black & White / Sepia",
    category: VideoFxCategory::Color,
    params: &[
        VideoFxParam {
            id: 0,
            key: "desaturate",
            name: "Desaturate",
            kind: ParamKind::Scalar { min: 0.0, max: 1.0, default: 1.0, unit: Unit::Pct },
        },
        VideoFxParam {
            id: 1,
            key: "sepia",
            name: "Sepia",
            kind: ParamKind::Scalar { min: 0.0, max: 1.0, default: 0.0, unit: Unit::Pct },
        },
    ],
    passes: &[VideoFxPass {
        kind: PassKind::Simple,
        wgsl: r#"
fn effect(uv: vec2<f32>, src: vec4<f32>) -> vec4<f32> {
    let luma = dot(src.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    let gray = mix(src.rgb, vec3<f32>(luma), P.desaturate);
    let sepia = vec3<f32>(luma) * vec3<f32>(1.07, 0.74, 0.43);
    let c = mix(gray, sepia, P.sepia);
    return vec4<f32>(clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)), src.a);
}
"#,
    }],
    needs_history: false,
};

/// 反転 (ネガポジ、量つき)。
const INVERT: VideoFxDef = VideoFxDef {
    id: "builtin.video.invert",
    name: "Invert",
    category: VideoFxCategory::Color,
    params: &[VideoFxParam {
        id: 0,
        key: "amount",
        name: "Amount",
        kind: ParamKind::Scalar { min: 0.0, max: 1.0, default: 1.0, unit: Unit::Pct },
    }],
    passes: &[VideoFxPass {
        kind: PassKind::Simple,
        wgsl: r#"
fn effect(uv: vec2<f32>, src: vec4<f32>) -> vec4<f32> {
    let c = mix(src.rgb, vec3<f32>(1.0) - src.rgb, P.amount);
    return vec4<f32>(c, src.a);
}
"#,
    }],
    needs_history: false,
};

/// ビネット (周辺減光)。★ 音反応の花形 (plan §4)。
const VIGNETTE: VideoFxDef = VideoFxDef {
    id: "builtin.video.vignette",
    name: "Vignette",
    category: VideoFxCategory::Color,
    params: &[
        VideoFxParam {
            id: 0,
            key: "amount",
            name: "Amount",
            kind: ParamKind::Scalar { min: 0.0, max: 1.0, default: 0.6, unit: Unit::Pct },
        },
        VideoFxParam {
            id: 1,
            key: "radius",
            name: "Radius",
            kind: ParamKind::Scalar { min: 0.0, max: 1.5, default: 0.75, unit: Unit::None },
        },
        VideoFxParam {
            id: 2,
            key: "softness",
            name: "Softness",
            kind: ParamKind::Scalar { min: 0.01, max: 1.0, default: 0.45, unit: Unit::None },
        },
    ],
    passes: &[VideoFxPass {
        kind: PassKind::Simple,
        wgsl: r#"
fn effect(uv: vec2<f32>, src: vec4<f32>) -> vec4<f32> {
    let d = distance(uv, vec2<f32>(0.5, 0.5)) * 1.41421356;
    let v = 1.0 - P.amount * smoothstep(P.radius, P.radius + P.softness, d);
    return vec4<f32>(src.rgb * v, src.a);
}
"#,
    }],
    needs_history: false,
};

/// ガウシアンブラー。分離プリミティブ ([`PassKind::SeparableBlur`]) を H/V 2 パスで適用する
/// 最初の効果 (plan_video_fx §1.2: bloom/glow/soft-vignette/unsharp の土台)。半径 (px) は
/// 第 1 param で、効果実行基盤がこれを sigma=radius/3 のガウシアンに展開する (body は engine 生成)。
const GAUSSIAN_BLUR: VideoFxDef = VideoFxDef {
    id: "builtin.video.gaussian_blur",
    name: "Gaussian Blur",
    category: VideoFxCategory::Blur,
    params: &[VideoFxParam {
        id: 0,
        key: "radius",
        name: "Radius",
        kind: ParamKind::Scalar { min: 0.0, max: 64.0, default: 8.0, unit: Unit::Px },
    }],
    passes: &[
        // SeparableBlur パスの wgsl は engine が生成するため未使用 (空文字)。
        VideoFxPass { kind: PassKind::SeparableBlur { horizontal: true }, wgsl: "" },
        VideoFxPass { kind: PassKind::SeparableBlur { horizontal: false }, wgsl: "" },
    ],
    needs_history: false,
};

/// モザイク / ピクセル化。★ 音反応の花形 (plan §4)。`cells` = 横方向のセル数、縦は
/// アスペクト比から導いて正方セルにする。Simple 1 パス (近傍量子化 + 再サンプル)。
const PIXELATE: VideoFxDef = VideoFxDef {
    id: "builtin.video.pixelate",
    name: "Pixelate / Mosaic",
    category: VideoFxCategory::Distort,
    params: &[VideoFxParam {
        id: 0,
        key: "cells",
        name: "Cells",
        kind: ParamKind::Scalar { min: 2.0, max: 240.0, default: 48.0, unit: Unit::None },
    }],
    passes: &[VideoFxPass {
        kind: PassKind::Simple,
        wgsl: r#"
fn effect(uv: vec2<f32>, src: vec4<f32>) -> vec4<f32> {
    let cx = max(P.cells, 1.0);
    // 正方セル: 縦のセル数を解像度のアスペクト比でスケール。
    let cy = max(cx * P.resolution.y / max(P.resolution.x, 1.0), 1.0);
    let snapped = vec2<f32>(
        (floor(uv.x * cx) + 0.5) / cx,
        (floor(uv.y * cy) + 0.5) / cy,
    );
    return sample(snapped);
}
"#,
    }],
    needs_history: false,
};

/// RGB スプリット（色収差）。★ 音反応の花形。R を +amount、B を −amount（px）水平にずらす。
const RGB_SPLIT: VideoFxDef = VideoFxDef {
    id: "builtin.video.rgb_split",
    name: "RGB Split",
    category: VideoFxCategory::Distort,
    params: &[VideoFxParam {
        id: 0,
        key: "amount",
        name: "Amount",
        kind: ParamKind::Scalar { min: 0.0, max: 60.0, default: 8.0, unit: Unit::Px },
    }],
    passes: &[VideoFxPass {
        kind: PassKind::Simple,
        wgsl: r#"
fn effect(uv: vec2<f32>, src: vec4<f32>) -> vec4<f32> {
    let off = vec2<f32>(P.amount * P.texel.x, 0.0);
    let r = sample(uv + off).r;
    let b = sample(uv - off).b;
    return vec4<f32>(r, src.g, b, src.a);
}
"#,
    }],
    needs_history: false,
};

/// しきい値（2 値化）。★ luma がしきい値以上なら白、未満なら黒。
const THRESHOLD: VideoFxDef = VideoFxDef {
    id: "builtin.video.threshold",
    name: "Threshold",
    category: VideoFxCategory::Stylize,
    params: &[VideoFxParam {
        id: 0,
        key: "threshold",
        name: "Threshold",
        kind: ParamKind::Scalar { min: 0.0, max: 1.0, default: 0.5, unit: Unit::None },
    }],
    passes: &[VideoFxPass {
        kind: PassKind::Simple,
        wgsl: r#"
fn effect(uv: vec2<f32>, src: vec4<f32>) -> vec4<f32> {
    let l = dot(src.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    let v = step(P.threshold, l);
    return vec4<f32>(vec3<f32>(v), src.a);
}
"#,
    }],
    needs_history: false,
};

/// ポスタライズ（階調圧縮）。★ 各チャンネルを `levels` 段に量子化。
const POSTERIZE: VideoFxDef = VideoFxDef {
    id: "builtin.video.posterize",
    name: "Posterize",
    category: VideoFxCategory::Stylize,
    params: &[VideoFxParam {
        id: 0,
        key: "levels",
        name: "Levels",
        kind: ParamKind::Scalar { min: 2.0, max: 32.0, default: 4.0, unit: Unit::None },
    }],
    passes: &[VideoFxPass {
        kind: PassKind::Simple,
        wgsl: r#"
fn effect(uv: vec2<f32>, src: vec4<f32>) -> vec4<f32> {
    let n = max(P.levels, 2.0) - 1.0;
    let c = floor(src.rgb * n + vec3<f32>(0.5)) / n;
    return vec4<f32>(clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)), src.a);
}
"#,
    }],
    needs_history: false,
};

/// エッジ検出 / スケッチ。Sobel で輪郭強度を出し黒地に白線で描く。
const EDGE_DETECT: VideoFxDef = VideoFxDef {
    id: "builtin.video.edge_detect",
    name: "Edge Detect",
    category: VideoFxCategory::Stylize,
    params: &[VideoFxParam {
        id: 0,
        key: "strength",
        name: "Strength",
        kind: ParamKind::Scalar { min: 0.0, max: 4.0, default: 1.0, unit: Unit::None },
    }],
    passes: &[VideoFxPass {
        kind: PassKind::Simple,
        wgsl: r#"
fn elum(c: vec3<f32>) -> f32 { return dot(c, vec3<f32>(0.299, 0.587, 0.114)); }
fn effect(uv: vec2<f32>, src: vec4<f32>) -> vec4<f32> {
    let t = P.texel;
    let tl = elum(sample(uv + vec2<f32>(-t.x, -t.y)).rgb);
    let tc = elum(sample(uv + vec2<f32>(0.0, -t.y)).rgb);
    let tr = elum(sample(uv + vec2<f32>(t.x, -t.y)).rgb);
    let ml = elum(sample(uv + vec2<f32>(-t.x, 0.0)).rgb);
    let mr = elum(sample(uv + vec2<f32>(t.x, 0.0)).rgb);
    let bl = elum(sample(uv + vec2<f32>(-t.x, t.y)).rgb);
    let bc = elum(sample(uv + vec2<f32>(0.0, t.y)).rgb);
    let br = elum(sample(uv + vec2<f32>(t.x, t.y)).rgb);
    let gx = (tr + 2.0 * mr + br) - (tl + 2.0 * ml + bl);
    let gy = (bl + 2.0 * bc + br) - (tl + 2.0 * tc + tr);
    let g = clamp(sqrt(gx * gx + gy * gy) * P.strength, 0.0, 1.0);
    return vec4<f32>(vec3<f32>(g), src.a);
}
"#,
    }],
    needs_history: false,
};

/// ミラー（鏡像）。右半分（または下半分）を反対側の鏡像で埋める。
const MIRROR: VideoFxDef = VideoFxDef {
    id: "builtin.video.mirror",
    name: "Mirror",
    category: VideoFxCategory::Distort,
    params: &[VideoFxParam {
        id: 0,
        key: "vertical",
        name: "Vertical",
        kind: ParamKind::Bool { default: false },
    }],
    passes: &[VideoFxPass {
        kind: PassKind::Simple,
        wgsl: r#"
fn effect(uv: vec2<f32>, src: vec4<f32>) -> vec4<f32> {
    var u = uv;
    if (P.vertical > 0.5) {
        if (u.y > 0.5) { u.y = 1.0 - u.y; }
    } else {
        if (u.x > 0.5) { u.x = 1.0 - u.x; }
    }
    return sample(u);
}
"#,
    }],
    needs_history: false,
};

/// クロマキー（色抜き）。キー色との距離が小さい画素を透明にする（グリーンバック等）。
const CHROMA_KEY: VideoFxDef = VideoFxDef {
    id: "builtin.video.chroma_key",
    name: "Chroma Key",
    category: VideoFxCategory::Key,
    params: &[
        VideoFxParam {
            id: 0,
            key: "key_r",
            name: "Key R",
            kind: ParamKind::Scalar { min: 0.0, max: 1.0, default: 0.0, unit: Unit::None },
        },
        VideoFxParam {
            id: 1,
            key: "key_g",
            name: "Key G",
            kind: ParamKind::Scalar { min: 0.0, max: 1.0, default: 1.0, unit: Unit::None },
        },
        VideoFxParam {
            id: 2,
            key: "key_b",
            name: "Key B",
            kind: ParamKind::Scalar { min: 0.0, max: 1.0, default: 0.0, unit: Unit::None },
        },
        VideoFxParam {
            id: 3,
            key: "threshold",
            name: "Threshold",
            kind: ParamKind::Scalar { min: 0.0, max: 1.0, default: 0.4, unit: Unit::None },
        },
        VideoFxParam {
            id: 4,
            key: "softness",
            name: "Softness",
            kind: ParamKind::Scalar { min: 0.01, max: 1.0, default: 0.1, unit: Unit::None },
        },
    ],
    passes: &[VideoFxPass {
        kind: PassKind::Simple,
        wgsl: r#"
fn effect(uv: vec2<f32>, src: vec4<f32>) -> vec4<f32> {
    let key = vec3<f32>(P.key_r, P.key_g, P.key_b);
    let d = distance(src.rgb, key);
    let a = smoothstep(P.threshold, P.threshold + P.softness, d);
    return vec4<f32>(src.rgb, src.a * a);
}
"#,
    }],
    needs_history: false,
};

/// シャープ（アンシャープマスク）。4 近傍平均との差を amount 倍して足す。
const SHARPEN: VideoFxDef = VideoFxDef {
    id: "builtin.video.sharpen",
    name: "Sharpen",
    category: VideoFxCategory::Blur,
    params: &[VideoFxParam {
        id: 0,
        key: "amount",
        name: "Amount",
        kind: ParamKind::Scalar { min: 0.0, max: 4.0, default: 1.0, unit: Unit::None },
    }],
    passes: &[VideoFxPass {
        kind: PassKind::Simple,
        wgsl: r#"
fn effect(uv: vec2<f32>, src: vec4<f32>) -> vec4<f32> {
    let t = P.texel;
    let blur = (sample(uv + vec2<f32>(t.x, 0.0)).rgb
              + sample(uv - vec2<f32>(t.x, 0.0)).rgb
              + sample(uv + vec2<f32>(0.0, t.y)).rgb
              + sample(uv - vec2<f32>(0.0, t.y)).rgb) * 0.25;
    let c = src.rgb + (src.rgb - blur) * P.amount;
    return vec4<f32>(clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)), src.a);
}
"#,
    }],
    needs_history: false,
};

/// フィルムグレイン（ノイズ）。★ ハッシュノイズを time でアニメして加算。
const FILM_GRAIN: VideoFxDef = VideoFxDef {
    id: "builtin.video.film_grain",
    name: "Film Grain",
    category: VideoFxCategory::Noise,
    params: &[VideoFxParam {
        id: 0,
        key: "amount",
        name: "Amount",
        kind: ParamKind::Scalar { min: 0.0, max: 1.0, default: 0.12, unit: Unit::Pct },
    }],
    passes: &[VideoFxPass {
        kind: PassKind::Simple,
        wgsl: r#"
fn ghash(p: vec2<f32>) -> f32 {
    return fract(sin(dot(p, vec2<f32>(12.9898, 78.233))) * 43758.5453);
}
fn effect(uv: vec2<f32>, src: vec4<f32>) -> vec4<f32> {
    let seed = uv * P.resolution + vec2<f32>(P.time * 60.0, P.time * 37.0);
    let n = ghash(seed) - 0.5;
    let c = src.rgb + vec3<f32>(n * P.amount);
    return vec4<f32>(clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)), src.a);
}
"#,
    }],
    needs_history: false,
};

/// スキャンライン（走査線）。横縞で暗くする（CRT/VHS 風）。
const SCANLINES: VideoFxDef = VideoFxDef {
    id: "builtin.video.scanlines",
    name: "Scanlines",
    category: VideoFxCategory::Stylize,
    params: &[
        VideoFxParam {
            id: 0,
            key: "count",
            name: "Count",
            kind: ParamKind::Scalar { min: 10.0, max: 1080.0, default: 240.0, unit: Unit::None },
        },
        VideoFxParam {
            id: 1,
            key: "intensity",
            name: "Intensity",
            kind: ParamKind::Scalar { min: 0.0, max: 1.0, default: 0.5, unit: Unit::Pct },
        },
    ],
    passes: &[VideoFxPass {
        kind: PassKind::Simple,
        wgsl: r#"
fn effect(uv: vec2<f32>, src: vec4<f32>) -> vec4<f32> {
    let s = sin(uv.y * P.count * 3.14159265) * 0.5 + 0.5;
    let v = mix(1.0, s, P.intensity);
    return vec4<f32>(src.rgb * v, src.a);
}
"#,
    }],
    needs_history: false,
};

/// Transform（座標変換）。plan_video_fx §5: 「動かす変形」をチェーン上の
/// 1 device として刺せるようにする。**GPU シェーダパスも video param も持たない**マーカー
/// device で、効果実行基盤は apply_chain に流さない。値（位置/スケール/回転/アンカー/不透明度）の
/// SSoT は purpose-built な [`GroupTransform`](crate::model::GroupTransform)（log スケール・AE 流
/// アンカー math・automation・変調を完備、立ち絵 group で実績）で、効果実行基盤は
/// [`crate::model::GroupTransform`] を resolve して合成画 1 枚の配置（approach X: rect + 任意
/// pivot 回転）に使う。device を刺すと当該トラックの `group_transform` が有効化され、どのトラック
/// でも（立ち絵 group も通常の動画/画像トラックも）座標変換できる。
const TRANSFORM: VideoFxDef = VideoFxDef {
    id: "builtin.video.transform",
    name: "Transform",
    category: VideoFxCategory::Transform,
    // video param 無し（値は GroupTransform に持つ。inspector は専用 Group Transform セクションで編集）。
    params: &[],
    // GPU パス無し（配置 device）。効果実行基盤が GroupTransform を resolve して合成段で消費。
    passes: &[],
    needs_history: false,
};

/// Transform 配置 device の id（効果実行基盤 / picker / inspector が「配置 device」を識別する）。
pub const TRANSFORM_ID: &str = "builtin.video.transform";

/// 内蔵映像効果の正準リスト (ピッカ表示順)。Transform を先頭に置く（配置の基本）、
/// 以降は色補正 → ブラー/シャープ → 歪み → スタイライズ → キーイング → ノイズ の順。
static BUILTIN_VIDEO_FX: &[VideoFxDef] = &[
    TRANSFORM,
    // 色補正 / グレード
    COLOR_GRADE,
    HUE_TEMP,
    DUOTONE,
    INVERT,
    VIGNETTE,
    // ブラー / シャープ
    GAUSSIAN_BLUR,
    SHARPEN,
    // 歪み / ワープ
    PIXELATE,
    RGB_SPLIT,
    MIRROR,
    // スタイライズ
    THRESHOLD,
    POSTERIZE,
    EDGE_DETECT,
    SCANLINES,
    // キーイング
    CHROMA_KEY,
    // ノイズ / 質感
    FILM_GRAIN,
];

/// 内蔵映像効果の正準リストを返す。`plugin_db::builtin_descriptors` /
/// 効果実行基盤 / インスペクタが参照する **Single Source of Truth**。
#[must_use]
pub fn builtin_video_fx() -> &'static [VideoFxDef] {
    BUILTIN_VIDEO_FX
}

/// `plugin_id` の効果定義を引く (`builtin.video.*`)。映像 device でなければ `None`。
#[must_use]
pub fn def_by_id(plugin_id: &str) -> Option<&'static VideoFxDef> {
    BUILTIN_VIDEO_FX.iter().find(|d| d.id == plugin_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_video_prefixed() {
        let defs = builtin_video_fx();
        for d in defs {
            assert!(
                d.id.starts_with("builtin.video."),
                "{} must be builtin.video.*",
                d.id
            );
            assert_eq!(
                defs.iter().filter(|x| x.id == d.id).count(),
                1,
                "duplicate id {}",
                d.id
            );
            // Transform は配置 device で GPU パスを持たない（apply_chain 非対象）。
            assert!(
                !d.passes.is_empty() || d.category == VideoFxCategory::Transform,
                "{} has no passes",
                d.id
            );
        }
    }

    #[test]
    fn param_ids_and_keys_unique_per_def() {
        for d in builtin_video_fx() {
            for (i, p) in d.params.iter().enumerate() {
                assert_eq!(p.id, i as u32, "{}: param ids must be 0-based dense", d.id);
                assert_eq!(
                    d.params.iter().filter(|x| x.key == p.key).count(),
                    1,
                    "{}: duplicate param key {}",
                    d.id,
                    p.key
                );
                // key は WGSL 識別子として妥当か (snake_case 英数)。
                assert!(
                    p.key
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                    "{}: param key {} not a valid wgsl ident",
                    d.id,
                    p.key
                );
                assert!(d.param(p.id).is_some());
            }
        }
    }

    #[test]
    fn default_norm_round_trips_through_real() {
        // default_norm → norm_to_real が manifest の default に一致する。
        for d in builtin_video_fx() {
            for p in d.params {
                let n = p.kind.default_norm();
                assert!((0.0..=1.0).contains(&n), "{}.{} default_norm OOR", d.id, p.key);
                match p.kind {
                    ParamKind::Scalar { default, .. } | ParamKind::LogScalar { default, .. } => {
                        let real = p.kind.norm_to_real(n);
                        // LogScalar は乗法的なので相対許容、Scalar は絶対許容。
                        let tol = (default.abs() * 1e-4).max(1e-4);
                        assert!(
                            (real - default).abs() < tol,
                            "{}.{}: default {default} != round-trip {real}",
                            d.id,
                            p.key
                        );
                    }
                    ParamKind::Bool { .. } => {}
                }
            }
        }
    }

    #[test]
    fn needs_history_matches_pass_kind() {
        // History パスを持つ効果だけが needs_history。
        for d in builtin_video_fx() {
            let has_history_pass =
                d.passes.iter().any(|p| matches!(p.kind, PassKind::History));
            assert_eq!(
                has_history_pass, d.needs_history,
                "{}: needs_history must match presence of History pass",
                d.id
            );
        }
    }
}
