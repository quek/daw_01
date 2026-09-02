//! マスターバス専用のストリップ設定 (バスコンプ + トーン EQ + リミッター)。
//!
//! 設計正本は [docs/plan_master_strip.md](../../../docs/plan_master_strip.md)。
//! 通常チャンネルの [`ChannelStrip`](super::ChannelStrip) とは**別物**で、共有するのは
//! レンジ表現 ([`ParamRange`](super::ParamRange)) と DSP の部品だけ。
//!
//! 信号順は `合算 → Comp → EQ → insert → フェーダー → リミッター` で固定
//! (Reason のマスターセクションと同じ「内蔵が先・insert が後」)。
//!
//! バスコンプの Ratio / Attack / Release が **段階式**なのは意図的 — SSL バスコンプや
//! Reason のマスターコンプと同じで、選択肢が少ないぶん速く決まる。オートメーションでは
//! **段の index** を値として載せる (`TrackBuiltin::Mute` と同じ階段扱い)。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::{COMP_KNEE_DB, EQ_SHELF_Q, ParamRange};

/// バスコンプのレシオ (3 択)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum MasterRatio {
    #[default]
    R2,
    R4,
    R10,
}

impl MasterRatio {
    pub const ALL: [Self; 3] = [Self::R2, Self::R4, Self::R10];

    #[must_use]
    pub fn value(self) -> f32 {
        match self {
            Self::R2 => 2.0,
            Self::R4 => 4.0,
            Self::R10 => 10.0,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::R2 => "2:1",
            Self::R4 => "4:1",
            Self::R10 => "10:1",
        }
    }
}

/// バスコンプのアタック (6 段、ms)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum MasterAttack {
    A01,
    A03,
    A1,
    #[default]
    A3,
    A10,
    A30,
}

impl MasterAttack {
    pub const ALL: [Self; 6] = [Self::A01, Self::A03, Self::A1, Self::A3, Self::A10, Self::A30];

    #[must_use]
    pub fn ms(self) -> f32 {
        match self {
            Self::A01 => 0.1,
            Self::A03 => 0.3,
            Self::A1 => 1.0,
            Self::A3 => 3.0,
            Self::A10 => 10.0,
            Self::A30 => 30.0,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::A01 => "0.1",
            Self::A03 => "0.3",
            Self::A1 => "1",
            Self::A3 => "3",
            Self::A10 => "10",
            Self::A30 => "30",
        }
    }
}

/// バスコンプのリリース (4 段 + Auto)。
///
/// `Auto` は program-adaptive — 長いピークの後は遅く、短いピークの後は速く戻る
/// (Reason の Master Bus Compressor と同じ挙動)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum MasterRelease {
    R100,
    #[default]
    R300,
    R600,
    R1200,
    Auto,
}

impl MasterRelease {
    pub const ALL: [Self; 5] =
        [Self::R100, Self::R300, Self::R600, Self::R1200, Self::Auto];

    /// 固定段の時定数 (ms)。`Auto` は信号追従なので `None`。
    #[must_use]
    pub fn ms(self) -> Option<f32> {
        match self {
            Self::R100 => Some(100.0),
            Self::R300 => Some(300.0),
            Self::R600 => Some(600.0),
            Self::R1200 => Some(1_200.0),
            Self::Auto => None,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::R100 => "0.1",
            Self::R300 => "0.3",
            Self::R600 => "0.6",
            Self::R1200 => "1.2",
            Self::Auto => "Auto",
        }
    }
}

/// `Auto` リリースが動く範囲 (ms)。短いピークでは下端、長いピークでは上端へ寄る。
pub const MASTER_AUTO_RELEASE_MIN_MS: f32 = 80.0;
/// [`MASTER_AUTO_RELEASE_MIN_MS`] の上端。
pub const MASTER_AUTO_RELEASE_MAX_MS: f32 = 1_500.0;
/// `Auto` リリースが「どれだけ長く潰れ続けたか」を測る時定数 (ms)。
/// この平均が深いほどリリースが遅くなる。
pub const MASTER_AUTO_RELEASE_TRACK_MS: f32 = 2_000.0;

/// トーン EQ の 3 バンド。**周波数は固定** (Mixbus のトーンコントロール流)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum MasterEqBand {
    /// 90Hz ローシェルフ。
    Low,
    /// 300Hz ワイドベル (タープサチュレーションの倍音が乗る帯域)。
    LoMid,
    /// 8kHz ハイシェルフ。
    High,
}

impl MasterEqBand {
    pub const ALL: [Self; 3] = [Self::Low, Self::LoMid, Self::High];

    /// このバンドの固定中心周波数 (Hz)。
    #[must_use]
    pub fn freq_hz(self) -> f32 {
        match self {
            Self::Low => 90.0,
            Self::LoMid => 300.0,
            Self::High => 8_000.0,
        }
    }

    /// ベル (ピーキング) なら `Some(Q)`、シェルビングなら `None`。
    #[must_use]
    pub fn bell_q(self) -> Option<f32> {
        match self {
            Self::LoMid => Some(EQ_SHELF_Q),
            Self::Low | Self::High => None,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "Lo",
            Self::LoMid => "LoMid",
            Self::High => "Hi",
        }
    }
}

/// トーン EQ のゲイン上下限 (dB)。**狭い**のは意図 — 最終段で大きく動かすのは事故で、
/// 狙った帯域を追い込むのは insert の EQ プラグインの仕事 (Mixbus の思想)。
pub const MASTER_EQ_LIMIT_DB: f32 = 6.0;

/// マスターストリップのパラメータ selector (オートメーション / 変調の住所)。
///
/// 段階式のもの (`CompRatio` / `CompAttack` / `CompRelease`) は **段の index** を
/// plain 値として扱う (`range()` が `0..=段数-1` を返す)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum MasterStripParam {
    CompOn,
    CompThreshold,
    CompRatio,
    CompAttack,
    CompRelease,
    CompMakeup,
    EqOn,
    EqGain(MasterEqBand),
    LimiterOn,
    LimiterCeiling,
}

impl MasterStripParam {
    /// 可動範囲。段階式は `0..=段数-1` の index ドメイン。
    #[must_use]
    pub fn range(self) -> ParamRange {
        match self {
            Self::CompOn | Self::EqOn | Self::LimiterOn => {
                ParamRange::Linear { lo: 0.0, hi: 1.0 }
            }
            Self::CompThreshold => ParamRange::Linear { lo: -30.0, hi: 0.0 },
            Self::CompRatio => ParamRange::Linear { lo: 0.0, hi: 2.0 },
            Self::CompAttack => ParamRange::Linear { lo: 0.0, hi: 5.0 },
            Self::CompRelease => ParamRange::Linear { lo: 0.0, hi: 4.0 },
            Self::CompMakeup => ParamRange::Linear { lo: -5.0, hi: 15.0 },
            Self::EqGain(_) => {
                ParamRange::Linear { lo: -MASTER_EQ_LIMIT_DB, hi: MASTER_EQ_LIMIT_DB }
            }
            Self::LimiterCeiling => ParamRange::Linear { lo: -6.0, hi: 0.0 },
        }
    }

    /// 値が段 (整数 index) か。曲線を段で描く / 値を丸める判定に使う。
    #[must_use]
    pub fn is_stepped(self) -> bool {
        matches!(
            self,
            Self::CompOn
                | Self::EqOn
                | Self::LimiterOn
                | Self::CompRatio
                | Self::CompAttack
                | Self::CompRelease
        )
    }

    /// ノブ / レーンに出す短いラベル。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::CompOn => "Comp On",
            Self::CompThreshold => "Thr",
            Self::CompRatio => "Ratio",
            Self::CompAttack => "Atk",
            Self::CompRelease => "Rel",
            Self::CompMakeup => "Gain",
            Self::EqOn => "EQ On",
            Self::EqGain(b) => b.label(),
            Self::LimiterOn => "Lim On",
            Self::LimiterCeiling => "Ceil",
        }
    }
}

/// マスターバスコンプ (Reason のマスターコンプ準拠、5 操作子)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct MasterCompSettings {
    pub on: bool,
    /// -30〜0 dB。
    pub threshold_db: f32,
    pub ratio: MasterRatio,
    pub attack: MasterAttack,
    pub release: MasterRelease,
    /// -5〜+15 dB。
    pub makeup_db: f32,
}

impl Default for MasterCompSettings {
    fn default() -> Self {
        Self {
            on: false,
            threshold_db: 0.0,
            ratio: MasterRatio::default(),
            attack: MasterAttack::default(),
            release: MasterRelease::default(),
            makeup_db: 0.0,
        }
    }
}

impl MasterCompSettings {
    /// ソフトニーの幅 (dB)。通常 ch と同じ値を使う (音の作りを 2 種類にしない)。
    pub const KNEE_DB: f32 = COMP_KNEE_DB;
}

/// マスタートーン EQ (3 band 固定周波数、ゲインのみ)。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, Encode, Decode)]
pub struct MasterEqSettings {
    pub on: bool,
    pub low_db: f32,
    pub lomid_db: f32,
    pub high_db: f32,
}

impl MasterEqSettings {
    #[must_use]
    pub fn gain_db(&self, band: MasterEqBand) -> f32 {
        match band {
            MasterEqBand::Low => self.low_db,
            MasterEqBand::LoMid => self.lomid_db,
            MasterEqBand::High => self.high_db,
        }
    }

    pub fn set_gain_db(&mut self, band: MasterEqBand, value: f32) {
        let v = MasterStripParam::EqGain(band).range().clamp(value);
        match band {
            MasterEqBand::Low => self.low_db = v,
            MasterEqBand::LoMid => self.lomid_db = v,
            MasterEqBand::High => self.high_db = v,
        }
    }
}

/// マスターリミッター。操作子はシーリング 1 つだけ。
///
/// リリースは信号追従の自動、ルックアヘッドは [`MASTER_LIMITER_LOOKAHEAD_MS`] 固定、
/// アタックは実質 0 (先読みで落とすため)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct MasterLimiterSettings {
    pub on: bool,
    /// -6〜0 dBFS。既定 -1.0 (配信の規定値 -1.0 dBTP に合わせやすい位置)。
    pub ceiling_db: f32,
}

impl Default for MasterLimiterSettings {
    fn default() -> Self {
        Self { on: false, ceiling_db: -1.0 }
    }
}

/// リミッターのルックアヘッド (ms)。**ON のときだけ**この遅延が乗り、OFF は素通し
/// (遅延ゼロ)。切り替えの瞬間に出力が 5ms 飛ぶ / 詰まるので再生中にプツッと鳴るが、
/// 「使っていないのに常に 5ms 遅れる」より切り替え時の一瞬を取った (設計判断、
/// `docs/plan_master_strip.md` §2)。
pub const MASTER_LIMITER_LOOKAHEAD_MS: f32 = 5.0;

/// [`MASTER_LIMITER_LOOKAHEAD_MS`] をサンプル数に直す **唯一の式**。
///
/// DSP (遅延線の長さ) と PDC 会計 (`Schedule::master_latency_samples` = 書き出しの
/// 窓ずらし / クリックの前出し) の両方がこれを引く。片方が丸め方を変えると、
/// 書き出しが 1 サンプル欠けるか、クリックが 1 サンプルずれる。
#[must_use]
pub fn limiter_lookahead_samples(sample_rate: u32) -> u32 {
    // 5ms = 1/200 秒。整数演算で切り捨て (44.1k → 220、48k → 240、96k → 480)。
    sample_rate / 200
}
/// リミッターのリリース時定数 (ms)。ルックアヘッドで先に落とすのでアタックは 0。
pub const MASTER_LIMITER_RELEASE_MS: f32 = 50.0;
/// GR メーターの表示レンジ (dB)。針式メーターの目盛り上端。
pub const MASTER_GR_METER_RANGE_DB: f32 = 20.0;

/// マスターバスのストリップ (コンプ + トーン EQ + リミッター)。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, Encode, Decode)]
pub struct MasterStrip {
    pub comp: MasterCompSettings,
    pub eq: MasterEqSettings,
    pub limiter: MasterLimiterSettings,
}

impl MasterStrip {
    /// 音に一切影響しない状態か (= DSP を丸ごと飛ばせる。リミッターも OFF なら
    /// 遅延を含めて素通し)。
    #[must_use]
    pub fn is_bypassed(&self) -> bool {
        !self.comp.on && !self.eq.on && !self.limiter.on
    }

    /// オートメーション target → 現在値 (plain)。段階式は段の index。
    ///
    /// **target とフィールドの対応はここが唯一の定義** (daw_audio の再生時解決と
    /// daw_gui のノブが同じ 1 本を引く)。
    #[must_use]
    pub fn param(&self, param: MasterStripParam) -> f32 {
        use MasterStripParam as P;
        match param {
            P::CompOn => f32::from(u8::from(self.comp.on)),
            P::CompThreshold => self.comp.threshold_db,
            P::CompRatio => index_of(&MasterRatio::ALL, self.comp.ratio),
            P::CompAttack => index_of(&MasterAttack::ALL, self.comp.attack),
            P::CompRelease => index_of(&MasterRelease::ALL, self.comp.release),
            P::CompMakeup => self.comp.makeup_db,
            P::EqOn => f32::from(u8::from(self.eq.on)),
            P::EqGain(b) => self.eq.gain_db(b),
            P::LimiterOn => f32::from(u8::from(self.limiter.on)),
            P::LimiterCeiling => self.limiter.ceiling_db,
        }
    }

    /// [`Self::param`] の書き込み側。可動範囲へクランプし、段階式は最寄りの段へ丸める。
    pub fn set_param(&mut self, param: MasterStripParam, value: f32) {
        use MasterStripParam as P;
        let v = param.range().clamp(value);
        match param {
            P::CompOn => self.comp.on = v >= 0.5,
            P::CompThreshold => self.comp.threshold_db = v,
            P::CompRatio => self.comp.ratio = nearest(&MasterRatio::ALL, v),
            P::CompAttack => self.comp.attack = nearest(&MasterAttack::ALL, v),
            P::CompRelease => self.comp.release = nearest(&MasterRelease::ALL, v),
            P::CompMakeup => self.comp.makeup_db = v,
            P::EqOn => self.eq.on = v >= 0.5,
            P::EqGain(b) => self.eq.set_gain_db(b, v),
            P::LimiterOn => self.limiter.on = v >= 0.5,
            P::LimiterCeiling => self.limiter.ceiling_db = v,
        }
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
    fn 段階式パラメータは段へ丸められて往復する() {
        let mut s = MasterStrip::default();
        // 1.4 → index 1 (4:1)、2.6 → 上端 (10:1)。
        s.set_param(MasterStripParam::CompRatio, 1.4);
        assert_eq!(s.comp.ratio, MasterRatio::R4);
        assert!((s.param(MasterStripParam::CompRatio) - 1.0).abs() < 1e-6);
        s.set_param(MasterStripParam::CompRatio, 2.6);
        assert_eq!(s.comp.ratio, MasterRatio::R10);
        // 範囲外は端で止まる (index が配列外に出ない)。
        s.set_param(MasterStripParam::CompRatio, -5.0);
        assert_eq!(s.comp.ratio, MasterRatio::R2);

        s.set_param(MasterStripParam::CompRelease, 4.0);
        assert_eq!(s.comp.release, MasterRelease::Auto);
        assert_eq!(s.comp.release.ms(), None);
    }

    #[test]
    fn トーン_eq_は上下_6db_で頭打ち() {
        let mut s = MasterStrip::default();
        s.set_param(MasterStripParam::EqGain(MasterEqBand::LoMid), 99.0);
        assert!((s.eq.lomid_db - MASTER_EQ_LIMIT_DB).abs() < 1e-6);
        s.set_param(MasterStripParam::EqGain(MasterEqBand::Low), -99.0);
        assert!((s.eq.low_db - -MASTER_EQ_LIMIT_DB).abs() < 1e-6);
        // 触っていないバンドは動かない。
        assert_eq!(s.eq.high_db, 0.0);
    }

    #[test]
    fn 既定は全バイパスでシーリングは配信の規定値() {
        let s = MasterStrip::default();
        assert!(s.is_bypassed());
        assert!((s.limiter.ceiling_db - -1.0).abs() < 1e-6);
    }

    #[test]
    fn 全パラメータが往復する() {
        let mut s = MasterStrip::default();
        let cases = [
            (MasterStripParam::CompThreshold, -18.0),
            (MasterStripParam::CompMakeup, 7.5),
            (MasterStripParam::EqGain(MasterEqBand::High), -3.5),
            (MasterStripParam::LimiterCeiling, -0.3),
            (MasterStripParam::CompOn, 1.0),
        ];
        for (p, v) in cases {
            s.set_param(p, v);
            assert!((s.param(p) - v).abs() < 1e-6, "{p:?}: {v} → {}", s.param(p));
        }
    }
}
