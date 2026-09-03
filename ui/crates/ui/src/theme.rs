//! テーマ — 汎用 UI クローム + 汎用 widget パレットの **Single Source of Truth**。
//!
//! renderer は `Color` 型と演算のみを持ち、意味を持つ色トークンはここ (ui-core) に置く。
//! DAW 固有の意味色 (playhead / record / solo / clip / ghost) は **アプリ側**
//! (`daw_gui::theme`) が持つ (arch 不変条件 #8: daw-ui core は DAW ドメインを持たない)。
//!
//! ## 所有者 (r.md #48)
//!
//! トークンは flat `pub const` ではなく [`Palette`] の **フィールド**。
//! [`crate::UiHost`] が `Arc<Palette>` を所有し、widget は [`crate::Ui::palette`] から読む。
//! プロセスグローバルにはしない — テストが並列スレッドで走るため、可変グローバルにすると
//! テーマを差し替えるテストが他テストの色アサーションを壊す。
//!
//! ## 軸が 3 本ある
//!
//! 1. **テーマ従属 (surface / ink)** — 面・枠・テキスト・グリッド。テーマごとに値が変わる。
//! 2. **極性固定インク (polarity-locked)** — 「常に明るい」「常に暗い」ことが意味そのもの。
//!    可変背景 (ユーザー着色クリップ / 波形 / 映像) の上で読ませるための 2 択の材料。
//!    既定では **両テーマ同値**で、テーマ作者が必要なら上書きできる。
//!    呼び出し側が 2 択を間違えないよう、選択は [`Palette::ink_for`] /
//!    [`Palette::waveform_for`] に畳んである。
//! 3. **色相固定・明度可変 (semantic hue)** — メーター ramp / accent / curve。色相が意味を
//!    運ぶので反転させないが、背景の明度に合わせて明度・彩度は調整する。
//!
//! アイデンティティ色 (ユーザーが選んだトラック色・automation lane のカテゴリ色) は
//! **テーマ非従属**。薄い面の上で沈む場合は [`Palette::adapt_on`] で色相を保ったまま
//! 明度だけ寄せる (任意色に効くので、トークンを増やす方法では代替できない)。
//!
//! ## 新しいトークンを足すとき
//!
//! [`palette!`] マクロの宣言に 1 行足すだけでよい。struct フィールド・`dark()` / `light()`・
//! JSON ローダ用の [`Palette::set_by_name`] が同時に生える (= ユーザーのテーマファイルは
//! 新トークンが増えても壊れない。書かれていないトークンはベーステーマから継承される)。
//! call site でベタ書きの `Color::rgb(...)` を新設しない。

use daw_ui_renderer::Color;

use crate::color::{CONTRAST_LUMINANCE_THRESHOLD, relative_luminance};

/// **パレットに入れる値は linear**。render target が `Rgba8UnormSrgb` なので、GPU が
/// 表示時に sRGB へエンコードする (実測: linear `(0.055, 0.365, 0.780)` → 画面上 `(66,163,229)`)。
///
/// 人が「こう見えてほしい」 色 (= 画面で拾えるスクリーンカラー / hex) から書くときは
/// **必ずこれを通す**。通さないと、意図よりずっと明るい色になる (ライトテーマの本文色を
/// 黒のつもりで 0.098 と書いたら画面では中間グレーだった、という実例がある)。
/// ダークの値は歴史的に linear 直値で目視調整済みなので、そのまま残してある。
#[must_use]
pub fn srgb(r: f32, g: f32, b: f32) -> Color {
    Color::rgb(srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b))
}

/// [`srgb`] の alpha 付き版。alpha は GPU のブレンド係数なので変換しない。
#[must_use]
pub fn srgba(r: f32, g: f32, b: f32, a: f32) -> Color {
    Color::rgba(srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b), a)
}

/// sRGB 成分 (0..=1) を linear へ (IEC 61966-2-1)。
#[must_use]
pub fn srgb_to_linear(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// 色トークンを 1 度だけ宣言し、struct / `dark()` / `light()` / 名前引きセッタを生成する。
///
/// `daw_gui` 側の DAW 固有パレットも同じマクロで宣言する (`#[macro_export]` でクレート外から
/// 使える)。値の式には `$crate::Color` (= `daw_ui_renderer::Color`) が使える。
#[macro_export]
macro_rules! palette {
    (
        $(#[$sm:meta])*
        $vis:vis struct $name:ident {
            $(
                $(#[$fm:meta])*
                $field:ident : $dark:expr, $light:expr;
            )*
        }
    ) => {
        $(#[$sm])*
        #[derive(Debug, Clone, PartialEq)]
        $vis struct $name {
            $(
                $(#[$fm])*
                pub $field: $crate::Color,
            )*
        }

        impl $name {
            /// 組込みダークテーマの値。
            #[must_use]
            pub fn dark() -> Self {
                Self { $( $field: $dark, )* }
            }

            /// 組込みライトテーマの値。
            #[must_use]
            pub fn light() -> Self {
                Self { $( $field: $light, )* }
            }

            /// トークン名 (snake_case) で 1 色だけ差し替える。未知の名前なら `false`。
            /// テーマ JSON の差分適用に使う。
            pub fn set_by_name(&mut self, key: &str, color: $crate::Color) -> bool {
                match key {
                    $( stringify!($field) => { self.$field = color; true } )*
                    _ => false,
                }
            }

            /// トークン名 (snake_case) で 1 色を読む。未知の名前なら `None`。
            #[must_use]
            pub fn get_by_name(&self, key: &str) -> Option<$crate::Color> {
                match key {
                    $( stringify!($field) => Some(self.$field), )*
                    _ => None,
                }
            }

            /// 全トークン名 (宣言順)。テーマ作者向けのテンプレート出力・テストで使う。
            #[must_use]
            pub fn token_names() -> &'static [&'static str] {
                &[ $( stringify!($field), )* ]
            }
        }
    };
}

palette! {
    /// 汎用 UI トークンの集合。DAW ドメインの色は持たない (arch 不変条件 #8)。
    ///
    /// ライトは「薄いグレーの床の上に、パネルとコントロールが白に近づいて浮く」構成
    /// (Cubase Silver / Studio One のライト系)。ダークと同じく blue チャンネルを最も高く
    /// 保ち (b > g ≥ r)、両テーマが同じ寒色スレート家族に属するようにしてある。
    pub struct Palette {
        // ===== 面 (elevation) =====
        // ダーク: 暗→明で持ち上げる。ライト: 床が最も暗く、raised が最も白い (順序は逆でなく
        // 「背景から離れる = コントラストが上がる」で一貫)。
        //
        // ダークの値は **linear 直値** (歴史的にこの空間で目視調整済み)。
        // ライトの値は [`srgb`] で **画面上の見た目 (スクリーンカラー)** から書く。
        // 混在しているのは意図的で、ダークの既存ピクセルを 1 つも動かさないため。

        /// アプリの最下層 = window/surface clear。全 panel が浮いて見える真の床。
        /// **`is_dark` の判定基準**でもある (この輝度で hover/pressed の向きが決まる)。
        window_bg: Color::rgb(0.035, 0.040, 0.050), srgb(0.855, 0.867, 0.886);
        /// 彫り込まれた窪み (text/number 入力欄・dropdown 本体・meter track・溝)。panel より一段沈む。
        inset_bg: Color::rgb(0.028, 0.033, 0.043), srgb(0.788, 0.804, 0.835);
        /// 窪みの hover (ダーク値は旧 `INSET_BG.lighten(0.06)` の実計算値と一致させてある)。
        inset_bg_hover: Color::rgb(0.0863, 0.0910, 0.1004), srgb(0.824, 0.839, 0.863);
        /// クロームのバー類 (transport / snap toolbar / timeline ruler / menu bar / tab bar / track header)。
        header: Color::rgb(0.048, 0.054, 0.067), srgb(0.898, 0.906, 0.922);
        /// elevation-1: 主要 panel / strip 本体 / modal / sidebar / menu popup。
        panel: Color::rgb(0.063, 0.070, 0.086), srgb(0.941, 0.949, 0.961);
        /// elevation-2: list-row 静止・note-grid 基底・keyboard panel・master strip。
        panel_raised: Color::rgb(0.086, 0.095, 0.115), srgb(0.984, 0.988, 0.992);

        // ===== コントロール (ボタン / トグルの面) =====

        /// button / toggle の idle (OFF) 塗り。汎用インタラクション面。
        control: Color::rgb(0.110, 0.121, 0.146), srgb(0.878, 0.890, 0.910);
        /// button / toggle / row の hover 塗り (面から離れる方向 = ダークは明、ライトは暗)。
        control_hover: Color::rgb(0.145, 0.158, 0.188), srgb(0.808, 0.824, 0.855);
        /// 非意味的トグルの ON 塗り (色付き状態は `accent` / semantic token を使う)。
        control_active: Color::rgb(0.175, 0.190, 0.225), srgb(0.722, 0.745, 0.788);

        // ===== 枠線 =====

        /// 汎用 1px の control / panel / field 枠。
        border: Color::rgb(0.165, 0.180, 0.215), srgb(0.604, 0.631, 0.682);
        /// hover 中の枠 (split handle・drop 標的。ダーク値は旧 `BORDER.lighten(0.15)` と一致)。
        border_hover: Color::rgb(0.2903, 0.3030, 0.3328), srgb(0.482, 0.518, 0.580);
        /// focus / open 状態の明るい枠 (accent 派生)。
        border_focus: Color::rgb(0.34, 0.58, 0.98), srgb(0.114, 0.435, 0.816);

        // ===== テキスト =====

        /// 主要 body / label テキスト (**クローム面の上**。可変背景の上では [`Palette::ink_for`])。
        text: Color::rgb(0.880, 0.902, 0.945), srgb(0.086, 0.098, 0.122);
        /// 二次 / muted テキスト (hint・ruler ラベル・chevron・disclosure)。
        text_dim: Color::rgb(0.560, 0.600, 0.680), srgb(0.329, 0.361, 0.420);
        /// 最弱可読層 (disabled menu/label・スケール外の鍵盤ラベル)。
        text_faint: Color::rgb(0.380, 0.415, 0.490), srgb(0.522, 0.553, 0.612);
        /// 失敗 / 使用不能を伝えるテキスト (塗り用 `meter_red` は文字だと沈むので別トークン)。
        text_error: Color::rgb(1.00, 0.47, 0.44), srgb(0.702, 0.094, 0.071);
        // NOTE: 「accent 塗りの上の文字色」 のトークンは**置かない**。
        // それは `ink_for(accent)` が答える問いで、トークンを別に持つと同じ問いに 2 つの
        // 答えができる (実際、旧 `TEXT_ON_ACCENT` は near-white 固定で、ダークの明るい
        // azure accent の上で 1.7:1 しか出ていなかった)。[`Palette::ink_on_accent`] を使う。

        // ===== アクセント =====

        /// PRIMARY accent: 行/欄の選択・focus・アクティブトグル ON・押下・menu hover・リンク・
        /// progress fill・fader fill。
        /// ライト値は **白文字が AA (4.5:1) で載る**濃さにしてある (実効 5.6:1)。
        /// ここを明るくすると選択行・menu hover の文字が読めなくなる。
        accent: Color::rgb(0.26, 0.62, 1.00), srgb(0.071, 0.400, 0.780);
        /// accent の半透明 wash (テキスト選択矩形・lasso 塗り・nest ターゲット・アクティブ帯)。
        accent_wash: Color::rgba(0.26, 0.62, 1.00, 0.20), srgba(0.071, 0.400, 0.780, 0.18);

        // ===== 選択 / インタラクションアフォーダンス =====

        /// 選択された要素の塗り、automation tension ハンドル。**意図的に accent ではない** warm:
        /// ユーザー着色されうる面 (青が多い) の上で補色の暖色で確実にコントラストを確保する。
        selection_warm: Color::rgb(1.00, 0.72, 0.24), srgb(0.961, 0.651, 0.137);
        /// ループ帯・ドラッグハンドル帯・reorder ドロップ標的・ファイルドロップ標的。
        loop_band: Color::rgb(0.40, 0.80, 1.00), srgb(0.118, 0.498, 0.769);
        /// 矩形選択 (marquee / lasso) の塗り。`Ui::take_drag_rect_in_rect` が自動描画する
        /// ので、arrangement / piano_roll / audio editor が同じ 1 色を共有する。
        marquee_fill: Color::rgba(0.32, 0.78, 0.95, 0.20), srgba(0.118, 0.498, 0.769, 0.18);
        /// 同 枠線。
        marquee_border: Color::rgba(0.32, 0.78, 0.95, 0.85), srgba(0.090, 0.404, 0.624, 0.85);
        /// キーボード focus の 1px リング (`Ui::draw_focus_ring`)。`border_focus` (欄の枠) とは
        /// 別レイヤの「いまキーボードがここ」標識なので独立トークンにしてある。
        focus_ring: Color::rgb(0.55, 0.78, 0.95), srgb(0.173, 0.498, 0.839);

        // ===== グリッド / lane hairline =====

        /// beat/subdivision グリッド・lane 区切り・baseline。
        grid_line: Color::rgba(0.80, 0.86, 1.00, 0.07), srgba(0.059, 0.102, 0.180, 0.13);
        /// 小節 (bar) グリッド線 = `grid_line` の強調層。
        grid_line_strong: Color::rgba(0.80, 0.86, 1.00, 0.17), srgba(0.059, 0.102, 0.180, 0.28);
        /// 最弱グリッド段 (piano_roll の subdivision 線・scrollbar track と同じ薄さ)。
        /// automation の既定値線はこれより濃い専用 alpha (0.18) なので `grid_line` 由来で書く。
        grid_line_faint: Color::rgba(0.80, 0.86, 1.00, 0.04), srgba(0.059, 0.102, 0.180, 0.07);

        // ===== スクロールバー =====

        scrollbar_track: Color::rgba(0.80, 0.86, 1.00, 0.04), srgba(0.059, 0.102, 0.180, 0.08);
        scrollbar_thumb: Color::rgba(0.80, 0.86, 1.00, 0.55), srgba(0.165, 0.196, 0.259, 0.50);
        scrollbar_thumb_hover: Color::rgba(0.80, 0.86, 1.00, 0.80), srgba(0.102, 0.125, 0.173, 0.70);

        // ===== 可動ハンドル (fader thumb / knob 指針) =====

        /// 物理的なつまみの面。周囲の面より必ず目立つ中立色。
        handle: Color::rgb(0.78, 0.82, 0.90), srgb(0.227, 0.255, 0.314);
        /// hover / drag 中のハンドル。
        handle_active: Color::rgb(0.95, 0.97, 1.00), srgb(0.090, 0.106, 0.133);

        // ===== オーバーレイ =====

        /// modal の暗転オーバーレイ。純黒でなく寒色チントで modal も同じ世界に置く。
        backdrop: Color::rgba(0.02, 0.03, 0.05, 0.62), srgba(0.078, 0.094, 0.122, 0.38);

        // ===== 数値ドラッグ中の背景 =====

        /// scrubable_number をドラッグ中の欄背景 (既定・寒色)。
        scrub_drag_bg: Color::rgb(0.20, 0.32, 0.45), srgb(0.659, 0.769, 0.910);
        /// 同 暖色版 (transport の tempo / 拍子など「時間軸そのもの」を触る欄)。
        scrub_drag_bg_warm: Color::rgb(0.45, 0.30, 0.20), srgb(0.910, 0.788, 0.659);

        // ===== IME =====

        /// IME 変換中 (preedit) の下線。警告色でも選択色でもない composition affordance。
        ime_preedit_underline: Color::rgb(0.95, 0.85, 0.55), srgb(0.604, 0.455, 0.067);

        // ===== レベルメーター ramp (色相固定・明度可変) =====

        meter_green: Color::rgb(0.30, 0.85, 0.40), srgb(0.106, 0.541, 0.227);
        meter_yellow: Color::rgb(0.92, 0.82, 0.30), srgb(0.612, 0.490, 0.039);
        meter_orange: Color::rgb(0.95, 0.55, 0.25), srgb(0.722, 0.361, 0.063);
        meter_red: Color::rgb(0.95, 0.32, 0.30), srgb(0.722, 0.122, 0.102);

        // ===== モジュレーション / カーブエディタ =====

        /// modulation / automation カーブ線。accent と区別する cyan。
        curve: Color::rgb(0.42, 0.85, 0.95), srgb(0.055, 0.478, 0.549);
        /// 変調がかかっている param の **base 位置** マーカー (fader thumb / knob 指針 /
        /// scrubable 欄に出る中立グレーの目盛)。「変調前の値はここ」を示すだけなので中立色。
        modulation_base_marker: Color::rgba(0.70, 0.70, 0.75, 0.90), srgba(0.290, 0.314, 0.361, 0.90);
        /// 変調後の **live 値** マーカー。base と一目で区別する amber
        /// (旧実装は fader/knob が amber・scrubable_number だけ near-white で不揃いだった)。
        modulation_live: Color::rgba(1.00, 0.85, 0.30, 0.95), srgba(0.659, 0.463, 0.039, 0.95);

        // ===== 極性固定インク (既定は両テーマ同値) =====
        // 可変背景 (ユーザー着色クリップ / 波形 / 映像) の上で読ませるための材料。
        // 「明るい方 / 暗い方」であること自体が意味なので、テーマで反転させてはいけない。

        /// **暗い背景の上**に置く明インク (クリップ名・鍵盤ラベル・fade 線・選択点)。
        ink_on_dark: Color::rgb(0.880, 0.902, 0.945), Color::rgb(0.880, 0.902, 0.945);
        /// **明るい背景の上**に置く暗インク。
        /// accent 塗り (ダークの明るい azure が最も明度が高い) の上でも AA (4.5:1) が
        /// 出る濃さにしてある。ここを明るくすると選択行の文字が真っ先に読めなくなる。
        ink_on_bright: Color::rgb(0.075, 0.085, 0.115), Color::rgb(0.075, 0.085, 0.115);

        /// 波形 fg (非選択、暗い背景用)。
        waveform_on_dark: Color::rgb(0.46, 0.74, 0.95), Color::rgb(0.46, 0.74, 0.95);
        /// 波形 fg (選択中、暗い背景用)。
        waveform_sel_on_dark: Color::rgb(0.62, 0.88, 1.00), Color::rgb(0.62, 0.88, 1.00);
        /// クリップしたピーク (暗い背景用)。
        waveform_peak_on_dark: Color::rgb(0.95, 0.42, 0.40), Color::rgb(0.95, 0.42, 0.40);
        /// 波形 fg (非選択、明るい背景用)。色相 (寒色ブルー) を保ったまま明度だけ落とす。
        waveform_on_bright: Color::rgb(0.06, 0.20, 0.36), Color::rgb(0.06, 0.20, 0.36);
        /// 波形 fg (選択中、明るい背景用)。
        waveform_sel_on_bright: Color::rgb(0.02, 0.13, 0.28), Color::rgb(0.02, 0.13, 0.28);
        /// クリップしたピーク (明るい背景用)。
        waveform_peak_on_bright: Color::rgb(0.52, 0.05, 0.04), Color::rgb(0.52, 0.05, 0.04);

        /// 可変背景の上に標識を読ませるための暗い裏打ちチップ (spinner badge / 数値チップ /
        /// 波形上のラベル背景)。memory `feedback_ui_indicator_contrast_on_variable_bg` の idiom。
        scrim: Color::rgba(0.02, 0.03, 0.05, 0.62), Color::rgba(0.02, 0.03, 0.05, 0.62);
        /// muted クリップ / muted ノートの斜線ハッチ。
        hatch_ink: Color::rgba(0.02, 0.03, 0.05, 0.34), Color::rgba(0.02, 0.03, 0.05, 0.34);
        /// 行を沈める overlay (黒鍵行・スケール外行・disabled lane)。
        row_dim_ink: Color::rgba(0.02, 0.03, 0.05, 0.25), Color::rgba(0.02, 0.03, 0.05, 0.25);

        /// 選択の 2 重リング 外側 (明)。ユーザー着色面の上でも必ず見える。
        selection_ring_outer: Color::rgb(1.00, 1.00, 1.00), Color::rgb(1.00, 1.00, 1.00);
        /// 選択の 2 重リング 内側 (暗)。
        selection_ring_inner: Color::rgb(0.08, 0.09, 0.12), Color::rgb(0.08, 0.09, 0.12);

        // ===== デバッグ HUD (Ctrl+F1) — 任意の内容の上に出るので極性固定 =====

        debug_overlay_bg: Color::rgba(0.05, 0.06, 0.10, 0.85), Color::rgba(0.05, 0.06, 0.10, 0.85);
        debug_overlay_border: Color::rgba(0.55, 0.85, 0.65, 0.55), Color::rgba(0.55, 0.85, 0.65, 0.55);
        debug_text: Color::rgb(0.85, 0.95, 0.85), Color::rgb(0.85, 0.95, 0.85);
    }
}

/// [`Palette::waveform_for`] が返す波形インクの種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaveformInk {
    /// 通常 (非選択) の波形本体。
    Normal,
    /// 選択中の波形本体。
    Selected,
    /// 0 dBFS を超えたピーク。
    Peak,
}

/// 図形要素 (線 / 点 / 塗り) に WCAG が求める最小コントラスト比 (WCAG 2.2 SC 1.4.11)。
/// [`Palette::adapt_on`] がこの比を満たすまでアイデンティティ色の明度を寄せる。
const MIN_GRAPHIC_CONTRAST: f32 = 3.0;

/// 本文サイズのテキストに WCAG AA が求める最小コントラスト比 (WCAG 2.2 SC 1.4.3)。
/// 図形の 3:1 では小さい数字は読めないので、文字には [`Palette::adapt_text_on`] を使う。
const MIN_TEXT_CONTRAST: f32 = 4.5;

impl Default for Palette {
    fn default() -> Self {
        Self::dark()
    }
}

impl Palette {
    /// 組込みテーマ id (`"dark"` / `"light"`) から引く。未知の id は `None`。
    #[must_use]
    pub fn builtin(id: &str) -> Option<Self> {
        match id {
            "dark" => Some(Self::dark()),
            "light" => Some(Self::light()),
            _ => None,
        }
    }

    /// このパレットが暗いか (= 床 `window_bg` の輝度で判定)。
    /// hover / pressed / auto-contrast の**向き**がこれで決まるので、テーマ作者が
    /// `window_bg` だけ差し替えても派生が破綻しない。
    #[must_use]
    pub fn is_dark(&self) -> bool {
        relative_luminance(self.window_bg.r, self.window_bg.g, self.window_bg.b)
            <= CONTRAST_LUMINANCE_THRESHOLD
    }

    /// `bg` の上に置く**本文インク**。明るい背景なら暗インク、暗い背景なら明インク。
    /// テーマ調のインクが図形基準 3:1 に届かない中間輝度の背景 (減光したクリップ色など) では、
    /// 同じ極性の極端 (純黒 / 純白) へ寄せて **どんな `bg` でも 3:1 を保証する**。
    ///
    /// 極性は輝度の閾値 (= 純黒と純白のコントラストが等しくなる点) で決める。閾値の側の極端は
    /// 必ず 3:1 以上 (閾値上で両者 4.6:1) なので、寄せれば到達する。「今のインク同士の比」で
    /// 極性を選ぶと、極端が弱い側を選んで届かない帯が残る。極端な背景では寄せが恒等
    /// (= テーマ調のインクそのまま)。
    ///
    /// 呼び出し側に明暗 2 色を渡させる旧 `pick_contrast(bg, light, dark)` を畳んだもの。
    /// 引数を取り違えて「どちらも暗色」になる事故 (ライトテーマでクリップ名が消える) が
    /// 構造的に起きなくなる。半透明 fill は先に [`crate::color::composite_over`] で
    /// 背後と合成してから渡すこと。
    #[must_use]
    pub fn ink_for(&self, bg: Color) -> Color {
        let ink = if relative_luminance(bg.r, bg.g, bg.b) > CONTRAST_LUMINANCE_THRESHOLD {
            self.ink_on_bright
        } else {
            self.ink_on_dark
        };
        adapt_to(bg, ink, MIN_GRAPHIC_CONTRAST)
    }

    /// `accent` 塗り (選択行 / menu hover / アクティブトグル) の上に置くインク。
    ///
    /// 専用トークンを持たず `ink_for(accent)` から**導出**する。トークンにすると
    /// 「accent の上は何色か」 に 2 つの答えができ、accent だけ差し替えたテーマ
    /// (ユーザーテーマを含む) で文字が読めなくなる。実際、旧 `TEXT_ON_ACCENT` は
    /// near-white 固定で、ダークの明るい azure accent の上で 1.7:1 だった。
    #[must_use]
    pub fn ink_on_accent(&self) -> Color {
        self.ink_for(self.accent)
    }

    /// `bg` の上に描く波形インク。色相 (寒色ブルー / 警告赤) を保ったまま明暗だけ切り替える。
    #[must_use]
    pub fn waveform_for(&self, bg: Color, kind: WaveformInk) -> Color {
        let bright = relative_luminance(bg.r, bg.g, bg.b) > CONTRAST_LUMINANCE_THRESHOLD;
        match (kind, bright) {
            (WaveformInk::Normal, false) => self.waveform_on_dark,
            (WaveformInk::Normal, true) => self.waveform_on_bright,
            (WaveformInk::Selected, false) => self.waveform_sel_on_dark,
            (WaveformInk::Selected, true) => self.waveform_sel_on_bright,
            (WaveformInk::Peak, false) => self.waveform_peak_on_dark,
            (WaveformInk::Peak, true) => self.waveform_peak_on_bright,
        }
    }

    /// 任意の base 色の **hover 派生**。面から離れる方向 (ダーク = 明るく / ライト = 暗く) に寄せる。
    ///
    /// `Color::lighten` は白方向に固定なので、ライトテーマで直に使うと hover が背景に溶ける。
    /// caller 任意色 (mute / solo / rec トグルの赤・黄) にも効く。
    #[must_use]
    pub fn hover(&self, base: Color) -> Color {
        if self.is_dark() { base.lighten(0.10) } else { base.darken(0.08) }
    }

    /// 任意の base 色の **pressed 派生**。押し込みは両テーマとも暗くする (物理メタファ)。
    #[must_use]
    pub fn pressed(&self, base: Color) -> Color {
        if self.is_dark() { base.darken(0.20) } else { base.darken(0.18) }
    }

    /// アイデンティティ色 (ユーザーが選んだトラック色・automation lane のカテゴリ色) を
    /// `bg` の上で読める明度に寄せる。**色相と彩度は保つ**ので識別性は失われない。
    /// 既にコントラストが足りていれば恒等 (= ダークテーマでは実質何もしない)。
    ///
    /// トークンを増やす方法ではユーザーが color_picker で選んだ任意色に効かないので、
    /// アイデンティティ色は「変換」で読ませる。
    #[must_use]
    pub fn adapt_on(&self, bg: Color, identity: Color) -> Color {
        adapt_to(bg, identity, MIN_GRAPHIC_CONTRAST)
    }

    /// [`adapt_on`](Self::adapt_on) のテキスト版。求めるコントラストが図形の 3:1 ではなく
    /// 本文テキストの AA 4.5:1 になる。
    ///
    /// 色付きの**文字** (メーターの over 表示・警告ラベル等) はこちらを使う。3:1 のまま
    /// 文字に流用すると「規格は満たしているのに小さい数字が読めない」状態になる。
    #[must_use]
    pub fn adapt_text_on(&self, bg: Color, identity: Color) -> Color {
        adapt_to(bg, identity, MIN_TEXT_CONTRAST)
    }
}

/// `identity` の色相を保ったまま、`bg` の上で `min_ratio` を満たす明度まで寄せる。
///
/// 寄せ先が純黒 / 純白なので、分岐は `CONTRAST_LUMINANCE_THRESHOLD` (= 純黒と純白の
/// コントラストが等しくなる点) が正しい。パレット依存が無いので自由関数。
fn adapt_to(bg: Color, identity: Color, min_ratio: f32) -> Color {
    let bg_l = relative_luminance(bg.r, bg.g, bg.b);
    let target = if bg_l > CONTRAST_LUMINANCE_THRESHOLD { Color::BLACK } else { Color::WHITE };
    let mut out = identity;
    // 8% 刻みで target 方向へ寄せ、最後は target そのもの (100%)。決定論的で、テストで
    // 固定できる。純黒 / 純白はどんな bg でも 3:1 を満たす (L=0.179 で両者 4.6:1) ので、
    // 極端まで許せば図形基準は必ず到達する。
    for step in 0..=13 {
        out = identity.lerp(target.with_alpha(identity.a), (step as f32 * 0.08).min(1.0));
        if contrast_ratio(out, bg) >= min_ratio {
            break;
        }
    }
    out
}

/// WCAG 2.x のコントラスト比 (1.0..=21.0)。どちらが明るいかは問わない。
#[must_use]
pub fn contrast_ratio(a: Color, b: Color) -> f32 {
    let la = relative_luminance(a.r, a.g, a.b);
    let lb = relative_luminance(b.r, b.g, b.b);
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_themes_have_expected_polarity() {
        assert!(Palette::dark().is_dark(), "ダークの床は暗い");
        assert!(!Palette::light().is_dark(), "ライトの床は明るい");
    }

    #[test]
    fn elevation_moves_away_from_the_floor_in_both_themes() {
        // 「奥行き = 床から離れるほどコントラストが上がる」 は両テーマ共通の不変条件。
        // ダークは明るくなる方向、ライトは白へ近づく方向で、どちらも panel < panel_raised。
        for p in [Palette::dark(), Palette::light()] {
            let lum = |c: Color| relative_luminance(c.r, c.g, c.b);
            if p.is_dark() {
                assert!(lum(p.window_bg) < lum(p.panel), "dark: 床 < panel");
                assert!(lum(p.panel) < lum(p.panel_raised), "dark: panel < raised");
                assert!(lum(p.inset_bg) < lum(p.panel), "dark: 窪み < panel");
            } else {
                assert!(lum(p.window_bg) < lum(p.panel), "light: 床 < panel");
                assert!(lum(p.panel) < lum(p.panel_raised), "light: panel < raised");
                assert!(lum(p.inset_bg) < lum(p.panel), "light: 窪み < panel");
            }
        }
    }

    #[test]
    fn body_text_is_readable_on_panel_in_both_themes() {
        for p in [Palette::dark(), Palette::light()] {
            let r = contrast_ratio(p.text, p.panel);
            assert!(r >= 7.0, "本文は AAA 相当 (7:1) を満たす: got {r}");
            let dim = contrast_ratio(p.text_dim, p.panel);
            assert!(dim >= 4.5, "二次テキストは AA (4.5:1) を満たす: got {dim}");
            // 選択行 / menu hover / アクティブトグルの文字色は `ink_on_accent` が決める。
            // **どのテーマでも** AA を満たすことが、選択行の可読性の土台。
            let auto = contrast_ratio(p.ink_on_accent(), p.accent);
            assert!(auto >= 4.5, "ink_on_accent は accent 塗りの上で AA を満たす: got {auto}");
        }
        // 極性が両テーマで逆に出ることも固定する (ダークの accent は明るい azure なので
        // 暗インク、ライトの accent は濃い青なので明インク)。ここが同じ向きになったら
        // `ink_for` の閾値かどちらかの accent 値が壊れている。
        assert_eq!(Palette::dark().ink_on_accent(), Palette::dark().ink_on_bright);
        assert_eq!(Palette::light().ink_on_accent(), Palette::light().ink_on_dark);
    }

    #[test]
    fn ink_for_picks_opposite_polarity_in_both_themes() {
        // テーマを切り替えても「明るい面には暗インク / 暗い面には明インク」 は不変。
        // ここが破れるとライトでクリップ名・波形が消える (r.md #48 の最大の落とし穴)。
        // **画面で見える色**で宣言する (srgb 経由)。 linear 直値で書くと直感とずれる:
        // linear (0.22, 0.34, 0.52) は画面上では sRGB (127, 157, 190) の明るいスチール
        // ブルーで、「暗いクリップ」 ではない (2026-08-15 まで relative_luminance の
        // 二重デコードでこれが「暗い」と誤判定され、白文字が 2.8:1 で載っていた)。
        let bright_clip = srgb(0.97, 0.88, 0.57);
        let dark_clip = srgb(0.22, 0.34, 0.52);
        for p in [Palette::dark(), Palette::light()] {
            assert_eq!(p.ink_for(bright_clip), p.ink_on_bright);
            assert_eq!(p.ink_for(dark_clip), p.ink_on_dark);
            assert!(contrast_ratio(p.ink_for(bright_clip), bright_clip) >= 4.5);
            assert!(contrast_ratio(p.ink_for(dark_clip), dark_clip) >= 4.5);
        }
    }

    #[test]
    fn waveform_for_keeps_hue_and_switches_polarity() {
        let p = Palette::dark();
        let bright = Color::rgb(0.90, 0.85, 0.60);
        let dark = Color::rgb(0.10, 0.12, 0.18);
        assert_eq!(p.waveform_for(dark, WaveformInk::Normal), p.waveform_on_dark);
        assert_eq!(p.waveform_for(bright, WaveformInk::Normal), p.waveform_on_bright);
        assert_eq!(p.waveform_for(bright, WaveformInk::Selected), p.waveform_sel_on_bright);
        assert_eq!(p.waveform_for(bright, WaveformInk::Peak), p.waveform_peak_on_bright);
        // 明背景用も寒色ブルーのまま (b > r) = 「波形は青」 の識別性を保つ。
        assert!(p.waveform_on_bright.b > p.waveform_on_bright.r);
    }

    #[test]
    fn hover_moves_away_from_the_surface_in_each_theme() {
        let base = Color::rgb(0.5, 0.5, 0.5);
        let lum = |c: Color| relative_luminance(c.r, c.g, c.b);
        assert!(lum(Palette::dark().hover(base)) > lum(base), "ダークの hover は明るく");
        assert!(lum(Palette::light().hover(base)) < lum(base), "ライトの hover は暗く");
        // pressed は両テーマとも押し込み = 暗く。
        assert!(lum(Palette::dark().pressed(base)) < lum(base));
        assert!(lum(Palette::light().pressed(base)) < lum(base));
    }

    #[test]
    fn adapt_on_is_identity_when_contrast_is_already_enough() {
        let p = Palette::dark();
        // 明るいパステルを暗い lane 背景に描く = 既に十分 → そのまま。
        let pastel = Color::rgb(0.55, 0.92, 0.55);
        assert_eq!(p.adapt_on(p.window_bg, pastel), pastel);
    }

    #[test]
    fn adapt_on_darkens_identity_colors_on_a_light_surface() {
        let p = Palette::light();
        let pastel = Color::rgb(0.55, 0.92, 0.55);
        let out = p.adapt_on(p.window_bg, pastel);
        assert!(
            contrast_ratio(out, p.window_bg) >= MIN_GRAPHIC_CONTRAST,
            "ライトの床の上でも図形コントラスト 3:1 を満たす"
        );
        // 色相 (緑が最大チャンネル) は保つ = カテゴリ識別性が失われない。
        assert!(out.g > out.r && out.g > out.b, "色相を保つ: {out:?}");
        // alpha は計算せず**そのまま複製する**契約なので、厳密一致で確かめる
        // (許容誤差で比べると「わずかに変えている」実装を見逃す)。
        assert_eq!(out.a.to_bits(), pastel.a.to_bits(), "alpha は保つ");
    }

    #[test]
    fn set_by_name_covers_every_declared_token() {
        // テーマ JSON の差分適用が全トークンに届くこと (= 宣言だけして名前引きから漏れる token が無い)。
        let mut p = Palette::dark();
        let probe = Color::rgba(0.123, 0.456, 0.789, 0.5);
        for name in Palette::token_names() {
            assert!(p.set_by_name(name, probe), "未対応のトークン名: {name}");
        }
        assert!(!p.set_by_name("no_such_token", probe));
    }

    #[test]
    fn dark_and_light_differ_on_every_theme_dependent_token() {
        // 極性固定インク以外は必ずテーマで変わっていること (light の書き忘れ検出)。
        let polarity_locked = [
            "ink_on_dark",
            "ink_on_bright",
            "waveform_on_dark",
            "waveform_sel_on_dark",
            "waveform_peak_on_dark",
            "waveform_on_bright",
            "waveform_sel_on_bright",
            "waveform_peak_on_bright",
            "scrim",
            "hatch_ink",
            "row_dim_ink",
            "selection_ring_outer",
            "selection_ring_inner",
            "debug_overlay_bg",
            "debug_overlay_border",
            "debug_text",
        ];
        let dark = Palette::dark();
        let light = Palette::light();
        for name in Palette::token_names() {
            let d = dark.get_by_name(name).expect("declared token");
            let l = light.get_by_name(name).expect("declared token");
            if polarity_locked.contains(name) {
                assert_eq!(d, l, "極性固定インクは両テーマ同値であるべき: {name}");
            } else {
                assert_ne!(d, l, "テーマ従属トークンなのに dark と light が同じ: {name}");
            }
        }
    }
}
