//! テーマ — UI クローム色の **Single Source of Truth**。
//!
//! 以前は色が `daw_gui/src/view/*` の file-local `const COLOR_*` と
//! `ui/crates/ui/src/widgets/*` の `*Style::Default` に **約 250 箇所ベタ書き** され、
//! 同じ論理色 (primary text ~60 / accent blue ~30 / border ~35) が複製されていた。
//! 「背景をもっと暗く・クールに」 を 1 箇所で実現できるよう、全クローム色をここに集約する。
//!
//! ## 設計
//! - **暗く・クール・階層的**: ほぼ黒の寒色チャコールを基底に、blue チャンネルを常に最も高く
//!   (b > g ≥ r) してスレートの寒色チントを全面に通す。深さは **色相でなく輝度の段差** で表現:
//!   `WINDOW_BG < HEADER < PANEL < PANEL_RAISED < CONTROL` を各 ~+0.03 luma で積層し、
//!   全段が同一の寒色クロマを共有して「一枚の機械加工された面」に見せる (Bitwig/Studio One 系)。
//! - **アクセントは 1 色**: 断片化していた 9 種の青を `ACCENT` (electric azure, hue≈212) に統一。
//!   選択 / フォーカス / アクティブトグル / 押下 / リンク / プログレスの **中立的インタラクション
//!   すべて** がこれ。
//! - **彩度は機能色だけに予約**: `PLAYHEAD` (warm coral) / `RECORD` / `SOLO` / `PLAY` / meter ramp
//!   のみ寒色基調から外れて彩度を持つ。clip/note 選択は意図的に **warm amber** (`SELECTION_WARM`) —
//!   clip はユーザー着色 (しばしば青) なので補色の暖色で確実にコントラストを確保する。
//! - **派生状態は再宣言しない**: hover = [`Color::lighten`]、pressed = [`Color::darken`]、
//!   半透明 wash = [`Color::with_alpha`] で token から計算する。
//!
//! ## SSoT の使い方
//! 新しい色が要るときは **ここに token を 1 つ足す**。call site でベタ書きの `Color::rgb(...)`
//! を新設しない (guard 対象)。runtime のテーマ切替 (light mode 等) は現状要件に無いので、
//! 構造体を `Ui` にスレッドする boilerplate は入れず flat `pub const` に留める (KISS)。
//! 将来 runtime テーマが要るなら、この flat const 群を `Palette` struct に畳む 1 回の機械的
//! refactor で済む (全参照が `theme::*` を通っているため)。

use crate::Color;

// ===== 面 (elevation: 暗→明で奥行きを表現) =====

/// アプリの最下層 = window/surface clear。全 panel が浮いて見える真の床。
pub const WINDOW_BG: Color = Color::rgb(0.035, 0.040, 0.050);
/// 彫り込まれた窪み (text/number 入力欄・dropdown 本体・meter track・溝)。panel より一段沈む。
pub const INSET_BG: Color = Color::rgb(0.028, 0.033, 0.043);
/// クロームのバー類 (transport / snap toolbar / timeline ruler / menu bar / tab bar / track header)。
pub const HEADER: Color = Color::rgb(0.048, 0.054, 0.067);
/// elevation-1: 主要 panel / strip 本体 / modal / sidebar / menu popup。
pub const PANEL: Color = Color::rgb(0.063, 0.070, 0.086);
/// elevation-2: list-row 静止・note-grid 基底・keyboard panel・master strip。panel より +0.03 luma。
pub const PANEL_RAISED: Color = Color::rgb(0.086, 0.095, 0.115);

// ===== コントロール (ボタン / トグルの面) =====

/// button / toggle の idle (OFF) 塗り。panel_raised から明確に持ち上がる汎用インタラクション面。
pub const CONTROL: Color = Color::rgb(0.110, 0.121, 0.146);
/// button / toggle / row の hover 塗り (= `CONTROL.lighten(0.06)` 相当を token 化)。
pub const CONTROL_HOVER: Color = Color::rgb(0.145, 0.158, 0.188);
/// 非意味的トグルの ON 塗り (色付き状態は `ACCENT` / semantic token を使う)。最も明るい中立面。
pub const CONTROL_ACTIVE: Color = Color::rgb(0.175, 0.190, 0.225);

// ===== 枠線 =====

/// 汎用 1px の control / panel / field 枠。寒色寄りで低コントラストな締め。
pub const BORDER: Color = Color::rgb(0.165, 0.180, 0.215);
/// focus / open 状態の明るい枠 (accent 派生)。focused input・dropdown・split handle・drop indicator。
pub const BORDER_FOCUS: Color = Color::rgb(0.34, 0.58, 0.98);

// ===== テキスト =====

/// 主要 body / label テキスト。寒色オフホワイト (純白のギラつきを避ける)。
pub const TEXT: Color = Color::rgb(0.880, 0.902, 0.945);
/// 二次 / muted テキスト (hint・ruler ラベル・chevron・disclosure)。寒色スレートグレー。
pub const TEXT_DIM: Color = Color::rgb(0.560, 0.600, 0.680);
/// 最弱可読層 (disabled menu/label・スケール外の鍵盤ラベル)。
pub const TEXT_FAINT: Color = Color::rgb(0.380, 0.415, 0.490);
/// accent で選択された行の上に乗るテキスト/タグ (azure 上のクリスプ near-white)。
pub const TEXT_ON_ACCENT: Color = Color::rgb(0.97, 0.985, 1.0);
/// 明るい塗り (solo 黄 / 明 clip / 白鍵) の上の auto-contrast 暗テキスト。
pub const TEXT_ON_BRIGHT: Color = Color::rgb(0.08, 0.09, 0.12);

// ===== アクセント (中立的インタラクションの主役、1 色に統一) =====

/// PRIMARY accent: 行/欄の選択・focus・アクティブトグル ON・押下・menu hover・リンク・
/// progress fill・fader fill。electric azure (hue≈212)。
pub const ACCENT: Color = Color::rgb(0.26, 0.62, 1.00);
/// accent の低 alpha 版 (テキスト選択矩形・lasso 塗り・nest ターゲット・半透明アクティブ帯)。
pub const ACCENT_WASH: Color = ACCENT.with_alpha(0.20);

// ===== 選択 / 時間軸アフォーダンス =====

/// 選択された clip / note の塗り、automation tension ハンドル。**意図的に accent ではない** warm amber:
/// clip はユーザー着色 (青が多い) なので補色の暖色で確実にコントラストを確保する。
pub const SELECTION_WARM: Color = Color::rgb(1.00, 0.72, 0.24);
/// 再生ヘッド線 (arrangement / piano-roll / audio editor)。寒色フィールドで唯一「叫ぶ」 warm coral。
pub const PLAYHEAD: Color = Color::rgb(1.00, 0.34, 0.20);
/// loop 帯 + ドラッグハンドル・reorder ドロップ・ファイルドロップ標的。accent と同系の明るい空色。
pub const LOOP_BAND: Color = Color::rgb(0.40, 0.80, 1.00);

// ===== グリッド / lane hairline =====

/// beat/subdivision グリッド・lane 区切り・baseline・scrollbar track。寒色白の極薄線でスレート同一性を保つ。
pub const GRID_LINE: Color = Color::rgba(0.80, 0.86, 1.00, 0.07);
/// 小節 (bar) グリッド線 = `GRID_LINE` の強調層。
pub const GRID_LINE_STRONG: Color = GRID_LINE.with_alpha(0.17);

// ===== オーバーレイ =====

/// modal の暗転オーバーレイ。純黒でなく寒色チントで modal も同じスレート世界に置く。
pub const BACKDROP: Color = Color::rgba(0.02, 0.03, 0.05, 0.62);

// ===== 機能 (semantic) 状態色 — 彩度を持つのはここだけ =====

/// record-arm / MIDI record / mute ON。予約された alarm red。
pub const RECORD: Color = Color::rgb(0.90, 0.26, 0.30);
/// record-mode arm (録音モード待機) の orange。
pub const RECORD_ARM: Color = Color::rgb(0.90, 0.50, 0.22);
/// solo ON / metronome ON。予約された warning yellow。
pub const SOLO: Color = Color::rgb(0.97, 0.82, 0.30);
/// play / transport 稼働 LED・status success。わずかに teal 寄りの green で「go」も寒色家族に。
pub const PLAY: Color = Color::rgb(0.28, 0.80, 0.48);

// ===== レベルメーター ramp (green → yellow → orange → red) =====

pub const METER_GREEN: Color = Color::rgb(0.30, 0.85, 0.40);
pub const METER_YELLOW: Color = Color::rgb(0.92, 0.82, 0.30);
pub const METER_ORANGE: Color = Color::rgb(0.95, 0.55, 0.25);
pub const METER_RED: Color = Color::rgb(0.95, 0.32, 0.30);

// ===== 波形 =====

/// 波形 fg (非選択)。寒色ブルー。
pub const WAVEFORM: Color = Color::rgb(0.46, 0.74, 0.95);
/// 波形 fg (選択中、明るく)。
pub const WAVEFORM_SEL: Color = Color::rgb(0.62, 0.88, 1.00);
/// クリップしたピーク (赤)。
pub const WAVEFORM_PEAK: Color = Color::rgb(0.95, 0.42, 0.40);

// ===== モジュレーション / カーブエディタ =====

/// modulation / automation カーブ線。accent と区別する cyan。
pub const CURVE: Color = Color::rgb(0.42, 0.85, 0.95);

// ===== clip 既定色 / ドラッグゴースト =====

/// track 未着色時の clip 既定塗り。
pub const CLIP_DEFAULT: Color = Color::rgb(0.22, 0.34, 0.52);
/// clip 既定枠。
pub const CLIP_DEFAULT_BORDER: Color = Color::rgb(0.30, 0.46, 0.66);
/// ドラッグ複製ゴースト: リンク (元と連動) = green。
pub const GHOST_LINKED: Color = Color::rgb(0.42, 0.86, 0.58);
/// ドラッグ複製ゴースト: 独立 (新規実体) = orange。
pub const GHOST_INDEPENDENT: Color = Color::rgb(1.00, 0.70, 0.34);
