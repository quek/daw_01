//! 内蔵チャンネルストリップ (コンプ + EQ) の設定値。
//!
//! 設計正本は [docs/plan_channel_strip.md](../../../docs/plan_channel_strip.md)。
//! 全 track (通常 / group / return) が 1 個ずつ持ち、削除できない。信号順は
//! `inserts → Comp → EQ → Pan → Fader` で固定 (§1)。
//!
//! **レンジと plain↔正規化写像の SSoT はこのファイル**。`common::automation` の
//! `plain_to_norm` / `norm_to_plain`、GUI のノブ・数値欄、daw_audio の IPC 境界
//! クランプはすべて [`ParamRange`] を経由する (同じ式を 3 か所に書かない)。
//!
//! DSP (バイクワッド係数・コンプの利得計算) は [`crate::channel_strip_dsp`]。
//! こちらは値だけを持ち、音の作り方は知らない。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// 1 パラメータの可動範囲と、plain (実単位) ↔ 正規化 (0..=1) の写像。
///
/// ノブ / オートメーション / IPC クランプが共有する唯一の定義。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParamRange {
    /// 線形 (dB ゲイン・スレッショルド等)。
    Linear { lo: f32, hi: f32 },
    /// 対数 (周波数・時定数・レシオ)。`lo > 0` が前提。
    Log { lo: f32, hi: f32 },
    /// 対数 + 左端の `OFF` (plain `0.0`)。検出フィルタの周波数専用。
    ///
    /// 正規化 `0.0` だけが OFF で、`OFF_SPAN` より上は `lo..=hi` の対数目盛。
    /// ノブを左へ回し切ると必ず OFF に落ちる (§5.3)。
    LogWithOff { lo: f32, hi: f32 },
}

impl ParamRange {
    /// [`ParamRange::LogWithOff`] で OFF に落ちる正規化値の上限。
    pub const OFF_SPAN: f64 = 0.02;

    /// plain → 正規化 (0..=1)。範囲外は端に丸める。
    #[must_use]
    pub fn to_norm(self, plain: f64) -> f64 {
        match self {
            Self::Linear { lo, hi } => {
                let (lo, hi) = (f64::from(lo), f64::from(hi));
                ((plain - lo) / (hi - lo)).clamp(0.0, 1.0)
            }
            Self::Log { lo, hi } => log_to_norm(plain, f64::from(lo), f64::from(hi)),
            Self::LogWithOff { lo, hi } => {
                if plain < f64::from(lo) {
                    return 0.0;
                }
                let f = log_to_norm(plain, f64::from(lo), f64::from(hi));
                Self::OFF_SPAN + f * (1.0 - Self::OFF_SPAN)
            }
        }
    }

    /// 正規化 (0..=1) → plain。範囲外は端に丸める。
    #[must_use]
    pub fn from_norm(self, norm: f64) -> f64 {
        let n = norm.clamp(0.0, 1.0);
        match self {
            Self::Linear { lo, hi } => {
                let (lo, hi) = (f64::from(lo), f64::from(hi));
                lo + n * (hi - lo)
            }
            Self::Log { lo, hi } => norm_to_log(n, f64::from(lo), f64::from(hi)),
            Self::LogWithOff { lo, hi } => {
                if n <= Self::OFF_SPAN {
                    return 0.0;
                }
                let f = (n - Self::OFF_SPAN) / (1.0 - Self::OFF_SPAN);
                norm_to_log(f, f64::from(lo), f64::from(hi))
            }
        }
    }

    /// plain を可動範囲へ丸める (IPC 境界のクランプ)。
    #[must_use]
    pub fn clamp(self, plain: f32) -> f32 {
        match self {
            Self::Linear { lo, hi } | Self::Log { lo, hi } => plain.clamp(lo, hi),
            Self::LogWithOff { lo, hi } => {
                if plain < lo {
                    0.0
                } else {
                    plain.min(hi)
                }
            }
        }
    }

    /// plain↔正規化が **affine (直線)** か。オートメーション曲線を画面で直線と
    /// して描いてよいかの判定に使う (`common::automation::norm_mapping_is_affine`)。
    #[must_use]
    pub fn is_affine(self) -> bool {
        matches!(self, Self::Linear { .. })
    }

    /// plain↔正規化が **狭義単調 (= 逆写像を持つ)** か。
    /// `LogWithOff` は左端の OFF 帯が平らなので false。
    #[must_use]
    pub fn is_invertible(self) -> bool {
        !matches!(self, Self::LogWithOff { .. })
    }

    /// 表示レンジ (数値欄の clamp 用)。`LogWithOff` は OFF (0) を下端に含む。
    #[must_use]
    pub fn display_range(self) -> (f64, f64) {
        match self {
            Self::Linear { lo, hi } | Self::Log { lo, hi } => (f64::from(lo), f64::from(hi)),
            Self::LogWithOff { hi, .. } => (0.0, f64::from(hi)),
        }
    }
}

fn log_to_norm(plain: f64, lo: f64, hi: f64) -> f64 {
    if plain <= lo {
        return 0.0;
    }
    ((plain / lo).ln() / (hi / lo).ln()).clamp(0.0, 1.0)
}

fn norm_to_log(norm: f64, lo: f64, hi: f64) -> f64 {
    lo * (hi / lo).powf(norm.clamp(0.0, 1.0))
}

/// EQ の 6 段。**位置ではなくこの enum が住所** (不変条件 1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum EqBand {
    /// ハイパスフィルタ (12 dB/oct)。
    Hp,
    /// ローパスフィルタ (12 dB/oct)。
    Lp,
    /// 低域 (既定シェルビング、`bell` でベルへ)。
    Lf,
    /// 低中域 (常にベル、Q あり)。
    Lmf,
    /// 高中域 (常にベル、Q あり)。
    Hmf,
    /// 高域 (既定シェルビング、`bell` でベルへ)。
    Hf,
}

impl EqBand {
    /// 描画・自動化の列挙順 (低い順)。
    pub const ALL: [Self; 6] = [Self::Hp, Self::Lp, Self::Lf, Self::Lmf, Self::Hmf, Self::Hf];
    /// ゲインとベル/シェルフを持つ 4 バンド (フィルタを除く)、strip の表示順 (高→低)。
    pub const GAIN_BANDS: [Self; 4] = [Self::Hf, Self::Hmf, Self::Lmf, Self::Lf];

    /// このバンドの周波数可動範囲。Harrison 32C / SSL 9000 の帯域割りに倣う。
    #[must_use]
    pub fn freq_range(self) -> ParamRange {
        let (lo, hi) = match self {
            Self::Hp => (20.0, 3_100.0),
            Self::Lp => (160.0, 20_000.0),
            Self::Lf => (20.0, 600.0),
            Self::Lmf => (60.0, 2_000.0),
            Self::Hmf => (400.0, 8_000.0),
            Self::Hf => (1_500.0, 20_000.0),
        };
        ParamRange::Log { lo, hi }
    }

    /// Q ノブを strip に出すバンドか (Mixbus / Reason と同じく中域のみ)。
    #[must_use]
    pub fn has_q_knob(self) -> bool {
        matches!(self, Self::Lmf | Self::Hmf)
    }

    /// シェルフ / ベルを切り替えられるバンドか (両端のみ)。
    #[must_use]
    pub fn has_bell_switch(self) -> bool {
        matches!(self, Self::Lf | Self::Hf)
    }

    /// strip に出す短いラベル。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Hp => "HP",
            Self::Lp => "LP",
            Self::Lf => "LF",
            Self::Lmf => "LMF",
            Self::Hmf => "HMF",
            Self::Hf => "HF",
        }
    }
}

/// EQ 1 バンドの連続パラメータ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum EqParam {
    Freq,
    Gain,
    Q,
}

impl EqParam {
    /// `band` におけるこのパラメータの可動範囲。
    #[must_use]
    pub fn range(self, band: EqBand) -> ParamRange {
        match self {
            Self::Freq => band.freq_range(),
            Self::Gain => ParamRange::Linear { lo: -EQ_GAIN_LIMIT_DB, hi: EQ_GAIN_LIMIT_DB },
            Self::Q => ParamRange::Log { lo: EQ_Q_MIN, hi: EQ_Q_MAX },
        }
    }
}

/// EQ ゲインの上下限 (dB)。Harrison 32C の ±15 dB に合わせる。
pub const EQ_GAIN_LIMIT_DB: f32 = 15.0;
/// EQ の Q 可動範囲。
pub const EQ_Q_MIN: f32 = 0.3;
/// EQ の Q 可動範囲。
pub const EQ_Q_MAX: f32 = 3.0;
/// シェルビング動作時の固定 Q (ベル切替時は `EqBandSettings::q` を使う)。
pub const EQ_SHELF_Q: f32 = 0.7;

/// コンプの連続パラメータ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum CompParam {
    Threshold,
    Ratio,
    Attack,
    Release,
    /// メイクアップゲイン。
    Makeup,
    /// 検出フィルタの中心周波数 (`0.0` = OFF = フルレンジ検出)。
    ScFreq,
}

impl CompParam {
    #[must_use]
    pub fn range(self) -> ParamRange {
        match self {
            Self::Threshold => ParamRange::Linear { lo: -60.0, hi: 0.0 },
            Self::Ratio => ParamRange::Log { lo: 1.0, hi: 20.0 },
            Self::Attack => ParamRange::Log { lo: 0.1, hi: 100.0 },
            Self::Release => ParamRange::Log { lo: 10.0, hi: 2_000.0 },
            Self::Makeup => ParamRange::Linear { lo: 0.0, hi: 20.0 },
            Self::ScFreq => ParamRange::LogWithOff { lo: 20.0, hi: 16_000.0 },
        }
    }

    /// strip に出す短いラベル。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Threshold => "Thr",
            Self::Ratio => "Rat",
            Self::Attack => "Atk",
            Self::Release => "Rel",
            Self::Makeup => "Gain",
            Self::ScFreq => "SC",
        }
    }
}

/// コンプの動作モード (Mixbus の 3 択と同じ)。
///
/// モードは一部のノブを**上書き**する — 上書きされた値は音に効かないが、
/// モードを戻せばノブの値がそのまま復帰する (値を破壊しない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum CompMode {
    /// 低レシオ (2:1) + 速いリリース固定。アタックのみ可変。
    Leveler,
    /// 全パラメータ可変。
    #[default]
    Compressor,
    /// アタック 0.1ms 固定 + レシオ 20:1 下限。
    Limiter,
}

impl CompMode {
    pub const ALL: [Self; 3] = [Self::Leveler, Self::Compressor, Self::Limiter];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Leveler => "LEV",
            Self::Compressor => "CMP",
            Self::Limiter => "LIM",
        }
    }

    /// このモードが `param` のノブ値を上書きするか (= ノブを淡色にする)。
    #[must_use]
    pub fn overrides(self, param: CompParam) -> bool {
        match self {
            Self::Leveler => matches!(param, CompParam::Ratio | CompParam::Release),
            Self::Compressor => false,
            Self::Limiter => matches!(param, CompParam::Attack),
        }
    }
}

/// Leveler モードの固定レシオ。
pub const LEVELER_RATIO: f32 = 2.0;
/// Leveler モードの固定リリース (ms)。
pub const LEVELER_RELEASE_MS: f32 = 100.0;
/// Limiter モードの固定アタック (ms)。
pub const LIMITER_ATTACK_MS: f32 = 0.1;
/// Limiter モードのレシオ下限。
pub const LIMITER_MIN_RATIO: f32 = 20.0;
/// コンプのソフトニー幅 (dB)。全モード共通。
pub const COMP_KNEE_DB: f32 = 6.0;
/// GR メーターの表示レンジ (dB)。
pub const GR_METER_RANGE_DB: f32 = 20.0;

/// EQ 1 バンドの設定。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct EqBandSettings {
    /// このバンドを通すか。フィルタ (HP/LP) は既定 off、ゲインバンドは既定 on
    /// (ゲイン 0 dB なので on でも音は変わらない)。
    pub on: bool,
    pub freq_hz: f32,
    /// フィルタ (HP/LP) では未使用。
    pub gain_db: f32,
    /// シェルビング時は [`EQ_SHELF_Q`] を使うので未使用。
    pub q: f32,
    /// `true` でベル (ピーキング)。両端バンドのみ意味を持つ。
    pub bell: bool,
}

impl EqBandSettings {
    fn new(on: bool, freq_hz: f32, q: f32) -> Self {
        Self { on, freq_hz, gain_db: 0.0, q, bell: false }
    }
}

/// EQ セクション全体。バンドは**名前付きフィールド**で持つ (配列 index を
/// 住所にしない、不変条件 1)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct EqSettings {
    /// セクション全体のバイパス。`false` で係数を一切通さない。
    pub on: bool,
    pub hp: EqBandSettings,
    pub lp: EqBandSettings,
    pub lf: EqBandSettings,
    pub lmf: EqBandSettings,
    pub hmf: EqBandSettings,
    pub hf: EqBandSettings,
}

impl Default for EqSettings {
    fn default() -> Self {
        Self {
            on: false,
            hp: EqBandSettings::new(false, 80.0, EQ_SHELF_Q),
            lp: EqBandSettings::new(false, 12_000.0, EQ_SHELF_Q),
            lf: EqBandSettings::new(true, 100.0, EQ_SHELF_Q),
            lmf: EqBandSettings::new(true, 400.0, 0.7),
            hmf: EqBandSettings::new(true, 2_500.0, 0.7),
            hf: EqBandSettings::new(true, 8_000.0, EQ_SHELF_Q),
        }
    }
}

impl EqSettings {
    #[must_use]
    pub fn band(&self, band: EqBand) -> &EqBandSettings {
        match band {
            EqBand::Hp => &self.hp,
            EqBand::Lp => &self.lp,
            EqBand::Lf => &self.lf,
            EqBand::Lmf => &self.lmf,
            EqBand::Hmf => &self.hmf,
            EqBand::Hf => &self.hf,
        }
    }

    #[must_use]
    pub fn band_mut(&mut self, band: EqBand) -> &mut EqBandSettings {
        match band {
            EqBand::Hp => &mut self.hp,
            EqBand::Lp => &mut self.lp,
            EqBand::Lf => &mut self.lf,
            EqBand::Lmf => &mut self.lmf,
            EqBand::Hmf => &mut self.hmf,
            EqBand::Hf => &mut self.hf,
        }
    }

    /// バンドの連続パラメータを読む (オートメーション / GUI の base 値)。
    #[must_use]
    pub fn param(&self, band: EqBand, param: EqParam) -> f32 {
        let b = self.band(band);
        match param {
            EqParam::Freq => b.freq_hz,
            EqParam::Gain => b.gain_db,
            EqParam::Q => b.q,
        }
    }

    /// バンドの連続パラメータを書く (可動範囲へクランプする)。
    pub fn set_param(&mut self, band: EqBand, param: EqParam, value: f32) {
        let clamped = param.range(band).clamp(value);
        let b = self.band_mut(band);
        match param {
            EqParam::Freq => b.freq_hz = clamped,
            EqParam::Gain => b.gain_db = clamped,
            EqParam::Q => b.q = clamped,
        }
    }
}

/// コンプセクション全体。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct CompSettings {
    /// セクション全体のバイパス。
    pub on: bool,
    pub mode: CompMode,
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_db: f32,
    /// 検出フィルタの中心周波数 (`0.0` = OFF = フルレンジ検出)。
    /// Q は周波数から導出する ([`crate::channel_strip_dsp::sc_filter_q`])。
    pub sc_freq_hz: f32,
    /// 検出信号そのものをモニタへ出す (`SC Listen`)。
    ///
    /// **プロジェクトには保存しない** (`serde(skip)`) — 試聴は「聴き方の都合」で
    /// あって曲の中身ではない。IPC (bincode) には載るので daw_audio まで届く。
    #[serde(skip)]
    pub sc_listen: bool,
}

impl Default for CompSettings {
    fn default() -> Self {
        Self {
            on: false,
            mode: CompMode::Compressor,
            threshold_db: 0.0,
            ratio: 2.0,
            attack_ms: 10.0,
            release_ms: 200.0,
            makeup_db: 0.0,
            sc_freq_hz: 0.0,
            sc_listen: false,
        }
    }
}

impl CompSettings {
    /// ノブの値を読む (モードの上書きは適用しない = ノブが指す値)。
    #[must_use]
    pub fn param(&self, param: CompParam) -> f32 {
        match param {
            CompParam::Threshold => self.threshold_db,
            CompParam::Ratio => self.ratio,
            CompParam::Attack => self.attack_ms,
            CompParam::Release => self.release_ms,
            CompParam::Makeup => self.makeup_db,
            CompParam::ScFreq => self.sc_freq_hz,
        }
    }

    /// ノブの値を書く (可動範囲へクランプする)。
    pub fn set_param(&mut self, param: CompParam, value: f32) {
        let v = param.range().clamp(value);
        match param {
            CompParam::Threshold => self.threshold_db = v,
            CompParam::Ratio => self.ratio = v,
            CompParam::Attack => self.attack_ms = v,
            CompParam::Release => self.release_ms = v,
            CompParam::Makeup => self.makeup_db = v,
            CompParam::ScFreq => self.sc_freq_hz = v,
        }
    }

    /// モードの上書きを適用した **実効** レシオ / アタック / リリース。
    /// DSP はここだけを読む (上書き規則を 2 か所に書かない)。
    #[must_use]
    pub fn effective(&self) -> (f32, f32, f32) {
        match self.mode {
            CompMode::Leveler => (LEVELER_RATIO, self.attack_ms, LEVELER_RELEASE_MS),
            CompMode::Compressor => (self.ratio, self.attack_ms, self.release_ms),
            CompMode::Limiter => {
                (self.ratio.max(LIMITER_MIN_RATIO), LIMITER_ATTACK_MS, self.release_ms)
            }
        }
    }
}

/// 1 track ぶんのチャンネルストリップ。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, Encode, Decode)]
pub struct ChannelStrip {
    pub comp: CompSettings,
    pub eq: EqSettings,
}

impl ChannelStrip {
    /// 音に一切影響しない状態か (= DSP を丸ごと飛ばせる)。
    #[must_use]
    pub fn is_bypassed(&self) -> bool {
        !self.comp.on && !self.eq.on
    }

    /// オートメーション target → 現在値 (plain)。ストリップ以外の target は `None`。
    ///
    /// **target とフィールドの対応はここが唯一の定義**。daw_audio (再生時の解決)
    /// と daw_gui (ノブの base 値 / レーン既定値) が同じ 1 本を引く。
    #[must_use]
    pub fn target_value(&self, param: &super::TrackBuiltinParam) -> Option<f32> {
        use super::TrackBuiltinParam as P;
        Some(match param {
            P::StripEqOn => f32::from(u8::from(self.eq.on)),
            P::StripCompOn => f32::from(u8::from(self.comp.on)),
            P::StripEq { band, param } => self.eq.param(*band, *param),
            P::StripComp { param } => self.comp.param(*param),
            _ => return None,
        })
    }

    /// [`Self::target_value`] の書き込み側。ストリップ以外の target は `false`。
    /// 値は可動範囲へクランプされる。
    pub fn set_target_value(&mut self, param: &super::TrackBuiltinParam, value: f32) -> bool {
        use super::TrackBuiltinParam as P;
        match param {
            P::StripEqOn => self.eq.on = value >= 0.5,
            P::StripCompOn => self.comp.on = value >= 0.5,
            P::StripEq { band, param } => self.eq.set_param(*band, *param, value),
            P::StripComp { param } => self.comp.set_param(*param, value),
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 正規化は往復する() {
        let cases: [(ParamRange, f64); 6] = [
            (EqParam::Freq.range(EqBand::Lmf), 400.0),
            (EqParam::Gain.range(EqBand::Hf), -7.5),
            (EqParam::Q.range(EqBand::Hmf), 1.4),
            (CompParam::Threshold.range(), -18.0),
            (CompParam::Release.range(), 250.0),
            (CompParam::ScFreq.range(), 6_000.0),
        ];
        for (range, plain) in cases {
            let back = range.from_norm(range.to_norm(plain));
            assert!((back - plain).abs() < plain.abs() * 1e-6 + 1e-9, "{range:?}: {plain} → {back}");
        }
    }

    #[test]
    fn 検出フィルタは左端で_off_に落ちる() {
        let r = CompParam::ScFreq.range();
        assert_eq!(r.from_norm(0.0), 0.0);
        assert_eq!(r.from_norm(ParamRange::OFF_SPAN), 0.0);
        assert_eq!(r.to_norm(0.0), 0.0);
        // OFF の一段上は下限周波数。
        assert!((r.from_norm(ParamRange::OFF_SPAN + 1e-9) - 20.0).abs() < 1e-6);
        // クランプは 20Hz 未満を OFF に落とす (中途半端な低域を作らない)。
        assert_eq!(r.clamp(5.0), 0.0);
        assert_eq!(r.clamp(20_000.0), 16_000.0);
    }

    #[test]
    fn モードは実効値を上書きするがノブ値は壊さない() {
        let mut c = CompSettings { ratio: 3.0, attack_ms: 25.0, release_ms: 800.0, ..Default::default() };
        c.mode = CompMode::Leveler;
        assert_eq!(c.effective(), (LEVELER_RATIO, 25.0, LEVELER_RELEASE_MS));
        c.mode = CompMode::Limiter;
        assert_eq!(c.effective(), (LIMITER_MIN_RATIO, LIMITER_ATTACK_MS, 800.0));
        c.mode = CompMode::Compressor;
        assert_eq!(c.effective(), (3.0, 25.0, 800.0));
    }

    #[test]
    fn 既定のストリップは音を変えない() {
        assert!(ChannelStrip::default().is_bypassed());
    }

    /// 試聴 (`SC Listen`) は「聴き方の都合」なのでプロジェクトに残さない。
    /// 残すと、開いた瞬間から検出信号が鳴っていて原因が分からない状態になる。
    #[test]
    fn 保存に載るのは曲の中身だけで試聴状態は載らない() {
        let mut strip = ChannelStrip::default();
        strip.comp.on = true;
        strip.comp.threshold_db = -18.0;
        strip.comp.sc_listen = true;
        strip.eq.on = true;
        strip.eq.hmf.gain_db = 3.5;

        let json = serde_json::to_string(&strip).expect("serialize");
        assert!(!json.contains("sc_listen"), "試聴状態が保存されている: {json}");

        let back: ChannelStrip = serde_json::from_str(&json).expect("deserialize");
        assert!(back.comp.on);
        assert!((back.comp.threshold_db - -18.0).abs() < 1e-6);
        assert!((back.eq.hmf.gain_db - 3.5).abs() < 1e-6);
        assert!(!back.comp.sc_listen, "開いた直後に試聴が点いている");
    }
}
