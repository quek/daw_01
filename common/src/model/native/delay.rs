//! 内蔵 Delay の値。音の作り方は daw_audio の `native_dsp::delay` が持つ。
//!
//! パラメータの分節は Bitwig の Pattern (Mono / Stereo / Ping L / Ping R) と
//! Ableton Live の時間変化モード (Repitch / Fade / Jump) に倣う。設計の正本は
//! `docs/plan_rmd_134_135_reverb_delay.md` §6 / §7。
//!
//! ON/OFF の唯一の SSoT は [`super::NativeDevice::bypassed`] なのでここには持たない。
//! 音符値 → 拍の換算は [`crate::note_value`] が唯一の持ち主 (ここには式を書かない)。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::super::ParamRange;
use crate::note_value::{NoteKind, NoteValue};

/// ディレイ時間の上限 (秒)。ring の容量はこの秒数 × セッションのサンプルレートで決める
/// (固定サンプル数にすると高サンプルレートで短くなる / 範囲外を読む)。
pub const MAX_DELAY_SEC: f32 = 5.0;

/// tempo sync の音符値 (18 段、**拍長の昇順**)。
///
/// つまみを右に回すほど必ず長くなる順に並べる (混ぜて並べると段階式のつまみが破綻する)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum DelayDiv {
    D32T,
    D32,
    D16T,
    D32D,
    D16,
    D8T,
    D16D,
    #[default]
    D8,
    D4T,
    D8D,
    D4,
    D2T,
    D4D,
    D2,
    D1T,
    D2D,
    D1,
    D1D,
}

impl DelayDiv {
    pub const ALL: [Self; 18] = [
        Self::D32T,
        Self::D32,
        Self::D16T,
        Self::D32D,
        Self::D16,
        Self::D8T,
        Self::D16D,
        Self::D8,
        Self::D4T,
        Self::D8D,
        Self::D4,
        Self::D2T,
        Self::D4D,
        Self::D2,
        Self::D1T,
        Self::D2D,
        Self::D1,
        Self::D1D,
    ];
    /// [`Self::ALL`] と同順の表示ラベル (段階式の値表示の SSoT)。
    pub const LABELS: [&'static str; 18] = [
        "1/32T", "1/32", "1/16T", "1/32.", "1/16", "1/8T", "1/16.", "1/8", "1/4T", "1/8.", "1/4", "1/2T", "1/4.",
        "1/2", "1/1T", "1/2.", "1/1", "1/1.",
    ];

    /// この段が表す音符値。
    #[must_use]
    pub fn note_value(self) -> NoteValue {
        use NoteKind::{Dotted, Straight, Triplet};
        let (div, kind) = match self {
            Self::D32T => (32, Triplet),
            Self::D32 => (32, Straight),
            Self::D16T => (16, Triplet),
            Self::D32D => (32, Dotted),
            Self::D16 => (16, Straight),
            Self::D8T => (8, Triplet),
            Self::D16D => (16, Dotted),
            Self::D8 => (8, Straight),
            Self::D4T => (4, Triplet),
            Self::D8D => (8, Dotted),
            Self::D4 => (4, Straight),
            Self::D2T => (2, Triplet),
            Self::D4D => (4, Dotted),
            Self::D2 => (2, Straight),
            Self::D1T => (1, Triplet),
            Self::D2D => (2, Dotted),
            Self::D1 => (1, Straight),
            Self::D1D => (1, Dotted),
        };
        NoteValue::new(div, kind)
    }

    /// 拍の長さ。
    #[must_use]
    pub fn beats(self) -> f64 {
        self.note_value().beats()
    }
}

/// ステレオの配り方 (Bitwig の Pattern)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum DelayPattern {
    /// L / R をそれぞれ自分のラインへ (クロスなし)。
    Mono,
    /// L / R 独立 + `Cross` でたすき掛け。
    #[default]
    Stereo,
    /// 入力をモノ化して L のラインにだけ入れ、L→R→L と往復させる。
    PingL,
    /// [`Self::PingL`] の L / R を入れ替えたもの。
    PingR,
}

impl DelayPattern {
    pub const ALL: [Self; 4] = [Self::Mono, Self::Stereo, Self::PingL, Self::PingR];
    pub const LABELS: [&'static str; 4] = ["Mono", "Stereo", "Ping L", "Ping R"];

    /// ping-pong (往路 unity / 復路のみ Feedback) か。
    #[must_use]
    pub fn is_ping_pong(self) -> bool {
        matches!(self, Self::PingL | Self::PingR)
    }
}

/// 遅延時間が変わったときの読み出し位置の動かし方 (Ableton Live の 3 モード)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum DelayMode {
    /// 目標へ滑らかに寄せる。位置が連続に動くのでピッチが変わる (テープ)。
    #[default]
    Repitch,
    /// 読みヘッド 2 本を固定時間でクロスフェードする。
    Fade,
    /// 即座に切り替える (クリックが出うる)。
    Jump,
}

impl DelayMode {
    pub const ALL: [Self; 3] = [Self::Repitch, Self::Fade, Self::Jump];
    pub const LABELS: [&'static str; 3] = ["Repitch", "Fade", "Jump"];
}

/// フィードバックループ内の飽和 (Surge の clipping mode に倣う)。
/// **Feedback 100% で発散しない唯一の保証**がここ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum DelayDrive {
    /// 飽和なし (クリーンなデジタルディレイ)。
    Off,
    /// 緩い soft clip。
    #[default]
    Soft,
    /// `tanh`。
    Tanh,
    /// ハードクリップ。
    Hard,
}

impl DelayDrive {
    pub const ALL: [Self; 4] = [Self::Off, Self::Soft, Self::Tanh, Self::Hard];
    pub const LABELS: [&'static str; 4] = ["Off", "Soft", "Tanh", "Hard"];
}

/// ディレイのパラメーター住所。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum DelayParam {
    /// テンポ同期する (ON) / ms 指定 (OFF)。
    Sync,
    DivL,
    DivR,
    TimeL,
    TimeR,
    /// 音符値からのずらし (%)。
    OffsetL,
    OffsetR,
    /// R を L に追従させる。
    Link,
    Feedback,
    /// L ↔ R のたすき掛け量 (%)。[`DelayPattern::Stereo`] のときだけ効く。
    Cross,
    Pattern,
    Mode,
    /// フィードバックループ内のハイパス (`0.0` = OFF)。
    Hp,
    /// 同、ローパス。
    Lp,
    Drive,
    ModRate,
    ModDepth,
    /// 出力の M/S 幅 (%、`100` = 素通し)。
    Width,
    /// 入力を遮断して繰り返しを保持する。
    Freeze,
    Mix,
}

impl DelayParam {
    #[must_use]
    pub fn range(self) -> ParamRange {
        match self {
            Self::Sync | Self::Link | Self::Freeze => ParamRange::Toggle,
            Self::DivL | Self::DivR => ParamRange::Stepped { count: 18 },
            Self::TimeL | Self::TimeR => ParamRange::Log { lo: 1.0, hi: 5_000.0 },
            Self::OffsetL | Self::OffsetR => ParamRange::Linear { lo: -33.0, hi: 33.0 },
            Self::Feedback | Self::Cross | Self::ModDepth | Self::Mix => ParamRange::Linear { lo: 0.0, hi: 100.0 },
            Self::Pattern => ParamRange::Stepped { count: 4 },
            Self::Mode => ParamRange::Stepped { count: 3 },
            Self::Hp => ParamRange::LogWithOff { lo: 20.0, hi: 2_000.0 },
            Self::Lp => ParamRange::Log { lo: 200.0, hi: 20_000.0 },
            Self::Drive => ParamRange::Stepped { count: 4 },
            Self::ModRate => ParamRange::Log { lo: 0.01, hi: 20.0 },
            Self::Width => ParamRange::Linear { lo: 0.0, hi: 200.0 },
        }
    }

    /// つまみの下に出す短いラベル。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Sync => "Sync",
            Self::DivL => "DivL",
            Self::DivR => "DivR",
            Self::TimeL => "TimeL",
            Self::TimeR => "TimeR",
            Self::OffsetL => "OfsL",
            Self::OffsetR => "OfsR",
            Self::Link => "Link",
            Self::Feedback => "FB",
            Self::Cross => "Cross",
            Self::Pattern => "Pat",
            Self::Mode => "Mode",
            Self::Hp => "HP",
            Self::Lp => "LP",
            Self::Drive => "Drv",
            Self::ModRate => "Rate",
            Self::ModDepth => "Dep",
            Self::Width => "Wid",
            Self::Freeze => "Frz",
            Self::Mix => "Mix",
        }
    }

    /// 段階式の表示ラベル (`Some` = 段階式)。
    #[must_use]
    pub fn step_labels(self) -> Option<&'static [&'static str]> {
        match self {
            Self::DivL | Self::DivR => Some(&DelayDiv::LABELS),
            Self::Pattern => Some(&DelayPattern::LABELS),
            Self::Mode => Some(&DelayMode::LABELS),
            Self::Drive => Some(&DelayDrive::LABELS),
            _ => None,
        }
    }
}

/// Delay の値。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(default)]
pub struct DelaySettings {
    pub sync: bool,
    pub div_l: DelayDiv,
    pub div_r: DelayDiv,
    pub time_l_ms: f32,
    pub time_r_ms: f32,
    pub offset_l_pct: f32,
    pub offset_r_pct: f32,
    pub link: bool,
    pub feedback_pct: f32,
    pub cross_pct: f32,
    pub pattern: DelayPattern,
    pub mode: DelayMode,
    /// 0.0 = OFF。
    pub hp_hz: f32,
    pub lp_hz: f32,
    pub drive: DelayDrive,
    pub mod_rate_hz: f32,
    pub mod_depth_pct: f32,
    pub width_pct: f32,
    pub freeze: bool,
    pub mix_pct: f32,
}

impl Default for DelaySettings {
    fn default() -> Self {
        Self {
            sync: true,
            div_l: DelayDiv::D8,
            div_r: DelayDiv::D8,
            time_l_ms: 350.0,
            time_r_ms: 350.0,
            offset_l_pct: 0.0,
            offset_r_pct: 0.0,
            link: true,
            feedback_pct: 35.0,
            cross_pct: 0.0,
            pattern: DelayPattern::Stereo,
            mode: DelayMode::Repitch,
            hp_hz: 0.0,
            lp_hz: 6_000.0,
            drive: DelayDrive::Soft,
            mod_rate_hz: 0.5,
            mod_depth_pct: 0.0,
            width_pct: 100.0,
            freeze: false,
            mix_pct: 50.0,
        }
    }
}

impl DelaySettings {
    /// 連続値のフィールド (`sanitize` が回す対象。bool / 段は型として常に有効)。
    const CONTINUOUS: [DelayParam; 11] = [
        DelayParam::TimeL,
        DelayParam::TimeR,
        DelayParam::OffsetL,
        DelayParam::OffsetR,
        DelayParam::Feedback,
        DelayParam::Cross,
        DelayParam::Hp,
        DelayParam::Lp,
        DelayParam::ModRate,
        DelayParam::ModDepth,
        DelayParam::Width,
    ];

    /// 住所 → plain 値 (段階式は段の index、ON/OFF は 1.0 / 0.0)。
    #[must_use]
    pub fn param(&self, p: DelayParam) -> f32 {
        match p {
            DelayParam::Sync => f32::from(u8::from(self.sync)),
            DelayParam::DivL => index_of(&DelayDiv::ALL, self.div_l),
            DelayParam::DivR => index_of(&DelayDiv::ALL, self.div_r),
            DelayParam::TimeL => self.time_l_ms,
            DelayParam::TimeR => self.time_r_ms,
            DelayParam::OffsetL => self.offset_l_pct,
            DelayParam::OffsetR => self.offset_r_pct,
            DelayParam::Link => f32::from(u8::from(self.link)),
            DelayParam::Feedback => self.feedback_pct,
            DelayParam::Cross => self.cross_pct,
            DelayParam::Pattern => index_of(&DelayPattern::ALL, self.pattern),
            DelayParam::Mode => index_of(&DelayMode::ALL, self.mode),
            DelayParam::Hp => self.hp_hz,
            DelayParam::Lp => self.lp_hz,
            DelayParam::Drive => index_of(&DelayDrive::ALL, self.drive),
            DelayParam::ModRate => self.mod_rate_hz,
            DelayParam::ModDepth => self.mod_depth_pct,
            DelayParam::Width => self.width_pct,
            DelayParam::Freeze => f32::from(u8::from(self.freeze)),
            DelayParam::Mix => self.mix_pct,
        }
    }

    /// 住所へ書く (`v` は値域へクランプ済み・有限であること)。段階式は最寄りの段へ丸める。
    /// 戻り値 = 実際に変わったか。
    pub(super) fn write(&mut self, p: DelayParam, v: f32) -> bool {
        let before = *self;
        match p {
            DelayParam::Sync => self.sync = v >= 0.5,
            DelayParam::DivL => self.div_l = nearest(&DelayDiv::ALL, v),
            DelayParam::DivR => self.div_r = nearest(&DelayDiv::ALL, v),
            DelayParam::TimeL => self.time_l_ms = v,
            DelayParam::TimeR => self.time_r_ms = v,
            DelayParam::OffsetL => self.offset_l_pct = v,
            DelayParam::OffsetR => self.offset_r_pct = v,
            DelayParam::Link => self.link = v >= 0.5,
            DelayParam::Feedback => self.feedback_pct = v,
            DelayParam::Cross => self.cross_pct = v,
            DelayParam::Pattern => self.pattern = nearest(&DelayPattern::ALL, v),
            DelayParam::Mode => self.mode = nearest(&DelayMode::ALL, v),
            DelayParam::Hp => self.hp_hz = v,
            DelayParam::Lp => self.lp_hz = v,
            DelayParam::Drive => self.drive = nearest(&DelayDrive::ALL, v),
            DelayParam::ModRate => self.mod_rate_hz = v,
            DelayParam::ModDepth => self.mod_depth_pct = v,
            DelayParam::Width => self.width_pct = v,
            DelayParam::Freeze => self.freeze = v >= 0.5,
            DelayParam::Mix => self.mix_pct = v,
        }
        *self != before
    }

    /// フィールド単位の値域回復。冪等。
    pub fn sanitize(&mut self) {
        let d = Self::default();
        for p in Self::CONTINUOUS {
            let v = self.param(p);
            self.write(p, if v.is_finite() { p.range().clamp(v) } else { d.param(p) });
        }
    }

    /// `Link` を反映した実効の (div, time_ms, offset_pct)。`Link` ON なら R も L の値。
    #[must_use]
    pub fn effective(&self, right: bool) -> (DelayDiv, f32, f32) {
        if right && !self.link {
            (self.div_r, self.time_r_ms, self.offset_r_pct)
        } else {
            (self.div_l, self.time_l_ms, self.offset_l_pct)
        }
    }

    /// 片チャンネルの遅延時間 (秒)。`Sync` ON なら BPM から、OFF なら ms 指定から。
    /// **clamp はしない** — 容量への clamp は ring を持つ DSP 側の責務
    /// (ここで丸めると「実効時間の表示」まで嘘になる)。
    #[must_use]
    pub fn delay_secs(&self, right: bool, bpm: f32) -> f32 {
        let (div, time_ms, offset_pct) = self.effective(right);
        let base = if self.sync {
            #[allow(clippy::cast_possible_truncation)]
            let beats = div.beats() as f32;
            beats * 60.0 / bpm.max(0.01)
        } else {
            time_ms / 1000.0
        };
        (base * (1.0 + offset_pct / 100.0)).max(0.0)
    }

    /// 実際に鳴る遅延時間 (秒)。[`MAX_DELAY_SEC`] を超える指定 (遅い BPM × 長い音符値) は
    /// そこで頭打ちになる。**GUI の「実効 ms」表示と DSP の読み出し位置が同じ関数を読む**
    /// (黙って短くなったことが画面で見える)。
    #[must_use]
    pub fn effective_secs(&self, right: bool, bpm: f32) -> f32 {
        self.delay_secs(right, bpm).min(MAX_DELAY_SEC)
    }
}

/// 段の並びの中での位置 (= plain 値)。
#[allow(clippy::cast_precision_loss)]
fn index_of<T: PartialEq + Copy>(all: &[T], v: T) -> f32 {
    all.iter().position(|x| *x == v).unwrap_or(0) as f32
}

/// plain 値 (index) を最寄りの段へ丸める。
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn nearest<T: Copy>(all: &[T], v: f32) -> T {
    let i = (v.round().max(0.0) as usize).min(all.len() - 1);
    all[i]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 音符値の段は拍長の昇順に並んでいる() {
        let mut prev = 0.0;
        for d in DelayDiv::ALL {
            let b = d.beats();
            assert!(b > prev, "{:?} ({b}) が直前 ({prev}) 以下", d);
            prev = b;
        }
        // ラベルは音符値から導かれるものと一致する (表の写し間違いの検出)。
        for (d, label) in DelayDiv::ALL.into_iter().zip(DelayDiv::LABELS) {
            assert_eq!(d.note_value().label(), label, "{d:?}");
        }
    }

    #[test]
    fn 遅延時間は_sync_と_link_と_offset_を反映する() {
        let mut s = DelaySettings::default();
        // 1/8 @120BPM = 0.25 s。
        assert!((s.delay_secs(false, 120.0) - 0.25).abs() < 1e-6);
        // Link ON なら R も L に従う。
        s.div_r = DelayDiv::D1;
        assert!((s.delay_secs(true, 120.0) - 0.25).abs() < 1e-6);
        // Link OFF で R が自分の値になる (1/1 @120BPM = 2.0 s)。
        s.link = false;
        assert!((s.delay_secs(true, 120.0) - 2.0).abs() < 1e-6);
        // Offset は割合でずらす。
        s.offset_l_pct = 33.0;
        assert!((s.delay_secs(false, 120.0) - 0.25 * 1.33).abs() < 1e-6);
        // Sync OFF は ms 指定 (Offset は同じく効く)。
        s.sync = false;
        s.offset_l_pct = 0.0;
        assert!((s.delay_secs(false, 120.0) - 0.35).abs() < 1e-6);
        // BPM 1 まで落ちても NaN / inf にはならない (容量への clamp は DSP 側)。
        s.sync = true;
        s.div_l = DelayDiv::D1D;
        assert!(s.delay_secs(false, 1.0).is_finite());
    }
}
