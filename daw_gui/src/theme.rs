//! daw_gui の theme — ui-core 汎用パレット + DAW ドメイン意味色の合成。
//!
//! 汎用 UI トークン (chrome / meter / waveform / curve / selection / grid) は
//! [`daw_ui_core::theme`] が SSoT で、ここで glob 再輸出する。DAW 固有の意味色
//! (playhead / record / solo / clip / ghost) だけを **アプリ層のここ** で定義する
//! (arch 不変条件 #8: daw-ui core は DAW ドメインを持たない)。
//!
//! call site は `crate::theme::TOKEN` で汎用・DAW 双方を透過的に参照する。彩度を持つのは
//! この DAW semantic 色と ui-core の meter/waveform ramp だけで、chrome は寒色基調に統一。

pub use daw_ui_core::theme::*;

use daw_ui_renderer::Color;

// ===== 機能 (semantic) 状態色 — 彩度を持つ =====

/// 再生ヘッド線 (arrangement / piano-roll / audio editor)。寒色フィールドで唯一「叫ぶ」 warm coral。
pub const PLAYHEAD: Color = Color::rgb(1.00, 0.34, 0.20);
/// record-arm / MIDI record / mute ON。予約された alarm red。
pub const RECORD: Color = Color::rgb(0.90, 0.26, 0.30);
/// record-mode arm (録音モード待機) の orange。
pub const RECORD_ARM: Color = Color::rgb(0.90, 0.50, 0.22);
/// solo ON / metronome ON。予約された warning yellow。
pub const SOLO: Color = Color::rgb(0.97, 0.82, 0.30);
/// play / transport 稼働 LED・status success。わずかに teal 寄りの green で「go」も寒色家族に。
pub const PLAY: Color = Color::rgb(0.28, 0.80, 0.48);

// ===== clip 既定色 / ドラッグゴースト =====

/// track 未着色時の clip 既定塗り。
pub const CLIP_DEFAULT: Color = Color::rgb(0.22, 0.34, 0.52);
/// clip 既定枠。
pub const CLIP_DEFAULT_BORDER: Color = Color::rgb(0.30, 0.46, 0.66);
/// ドラッグ複製ゴースト: リンク (元と連動) = green。
pub const GHOST_LINKED: Color = Color::rgb(0.42, 0.86, 0.58);
/// ドラッグ複製ゴースト: 独立 (新規実体) = orange。
pub const GHOST_INDEPENDENT: Color = Color::rgb(1.00, 0.70, 0.34);
