//! トラックの「入力」と「モニター」 (r.md #156)。
//!
//! トラックは入力を 1 つ持つ: MIDI (既定) か、オーディオデバイスの入力 ch (モノ / ステレオ)。
//! 録音待機 (`Track::armed`) で録音すると、MIDI のトラックは MIDI クリップ、オーディオ入力の
//! トラックはオーディオクリップができる。
//!
//! **ch はデバイスの入力 ch 番号 (0 始まり) で持つ** — 一覧の並び順ではない。デバイスを替えて
//! その ch が無くなっても値は変えず、「無い入力」として表示し無音を読む (黙って別の ch に
//! 付け替えない)。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// トラックの入力。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, Encode, Decode)]
pub enum TrackInput {
    /// MIDI 入力 (今までどおり)。
    #[default]
    Midi,
    /// オーディオ入力 1 ch (L / R の両方へ同じ音)。
    AudioMono { channel: u16 },
    /// オーディオ入力の隣り合う 2 ch (`left` が L、`left + 1` が R)。
    AudioStereo { left: u16 },
}

impl TrackInput {
    /// `skip_serializing_if` 用 (既定の MIDI は書かない = 旧ファイルと同じ形)。
    #[must_use]
    pub fn is_midi(&self) -> bool {
        matches!(self, Self::Midi)
    }

    /// オーディオ入力なら `(L の ch, R の ch)`。モノは両方同じ ch。
    #[must_use]
    pub fn audio_channels(self) -> Option<(u16, u16)> {
        match self {
            Self::Midi => None,
            Self::AudioMono { channel } => Some((channel, channel)),
            Self::AudioStereo { left } => Some((left, left.saturating_add(1))),
        }
    }

    /// 録音したファイルの ch 数 (オーディオ入力でなければ 0)。
    #[must_use]
    pub fn file_channels(self) -> u16 {
        match self {
            Self::Midi => 0,
            Self::AudioMono { .. } => 1,
            Self::AudioStereo { .. } => 2,
        }
    }

    /// 表示名 (「入力 3」「入力 1+2」)。デバイスが ch に名前を付けていれば呼び側が添える。
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Midi => "MIDI".to_string(),
            Self::AudioMono { channel } => format!("入力 {}", u32::from(channel) + 1),
            Self::AudioStereo { left } => format!("入力 {}+{}", u32::from(left) + 1, u32::from(left) + 2),
        }
    }
}

/// 入力モニター (入力音をそのトラックのデバイスチェーンへ流して聞くか)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, Encode, Decode)]
pub enum MonitorMode {
    /// 聞こえない。
    Off,
    /// 録音待機 (R) の間だけ聞こえる (既定)。
    #[default]
    Auto,
    /// 常に聞こえる。
    On,
}

impl MonitorMode {
    pub const ALL: [MonitorMode; 3] = [MonitorMode::Off, MonitorMode::Auto, MonitorMode::On];

    /// `skip_serializing_if` 用 (既定の自動は書かない)。
    #[must_use]
    pub fn is_auto(&self) -> bool {
        matches!(self, Self::Auto)
    }

    /// いま聞こえるか (`armed` = 録音待機)。
    #[must_use]
    pub fn active(self, armed: bool) -> bool {
        match self {
            Self::Off => false,
            Self::Auto => armed,
            Self::On => true,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "オフ",
            Self::Auto => "自動",
            Self::On => "オン",
        }
    }
}
