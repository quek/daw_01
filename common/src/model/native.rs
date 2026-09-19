//! r.md #129: チェーン上の内蔵 device `Device::Native(NativeDevice)`
//! (`docs/plan_rack_native_devices.md` §5.2)。
//!
//! daw_audio が in-process で処理する Comp / EQ / Bus Comp / Tone EQ。plugin と同じく固有 id を
//! 持ち、行 / Par / D&D / オートメーション / 変調 / MIDI Learn / 外部サイドチェインの対象になる。
//! plugin との違いは「組み込み (`builtin`) は削除できず、Parallel に入れられず、普通のドラッグで
//! 他トラックへ移せない」だけ (規則は `chain_rules.rs`)。
//!
//! **ON/OFF の唯一の SSoT は [`NativeDevice::bypassed`]** (各 Settings に `on` は無い)。
//! 値の型は種類ごとのファイル、住所は `native_param.rs`、値域は `param_range.rs`。

use std::borrow::Cow;

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::{AuxInputRoute, AutomationLane, AutomationTarget, ModRouting, NativeParamId};

mod bus_comp;
mod chain_rules;
mod comp;
mod delay;
mod eq;
mod reverb;
mod tone_eq;

pub use bus_comp::*;
pub use chain_rules::*;
pub use comp::*;
pub use delay::*;
pub use eq::*;
pub use reverb::*;
pub use tone_eq::*;

/// GR メーターの表示レンジ (dB)。Comp / Bus Comp / Limiter 共通。
pub const GR_METER_RANGE_DB: f32 = 20.0;

/// 内蔵 device の種類。variant 名は JSON (`{"On":"Comp"}`) に出る。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Encode, Decode,
)]
pub enum NativeKind {
    Comp,
    Eq,
    BusComp,
    ToneEq,
    Reverb,
    Delay,
}

impl NativeKind {
    /// picker の並び。
    pub const ALL: [Self; 6] =
        [Self::Comp, Self::Eq, Self::BusComp, Self::ToneEq, Self::Reverb, Self::Delay];
    /// 通常 / group / return トラックの組み込み (チェーン末尾にこの順で補う)。
    pub const BUILTIN_TRACK: [Self; 2] = [Self::Comp, Self::Eq];
    /// master の組み込み (チェーン先頭にこの順で補う)。
    pub const BUILTIN_MASTER: [Self; 2] = [Self::BusComp, Self::ToneEq];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Comp => "Comp",
            Self::Eq => "EQ",
            Self::BusComp => "Bus Comp",
            Self::ToneEq => "Tone EQ",
            Self::Reverb => "Reverb",
            Self::Delay => "Delay",
        }
    }

    /// 値編集の undo ラベル。
    #[must_use]
    pub fn undo_label(self) -> &'static str {
        match self {
            Self::Comp => "コンプ変更",
            Self::Eq => "EQ 変更",
            Self::BusComp => "バスコンプ変更",
            Self::ToneEq => "トーン EQ 変更",
            Self::Reverb => "リバーブ変更",
            Self::Delay => "ディレイ変更",
        }
    }

    /// 全種とも audio in / out のみ (置換型、MIDI には触れない)。engine の直結規則
    /// (「audio_in があればバスを置換」) がそのまま効く。Reverb / Delay の dry/wet は
    /// device の中で混ぜるので、外から見れば同じ「置換」。
    #[must_use]
    pub fn ports(self) -> crate::port_config::PortConfig {
        crate::port_config::PortConfig {
            has_audio_input: true,
            has_audio_output: true,
            ..Default::default()
        }
    }

    /// GR を出す種類か (Comp / Bus Comp)。
    #[must_use]
    pub fn has_gain_reduction(self) -> bool {
        matches!(self, Self::Comp | Self::BusComp)
    }

    /// 外部サイドチェインを受ける種類か (Comp / Bus Comp、Q19)。
    #[must_use]
    pub fn accepts_sidechain(self) -> bool {
        matches!(self, Self::Comp | Self::BusComp)
    }

    /// SC Listen を持つ種類か (Comp のみ、Q12)。
    #[must_use]
    pub fn accepts_listen(self) -> bool {
        matches!(self, Self::Comp)
    }

    /// plugin picker の項目 id。
    #[must_use]
    pub fn picker_id(self) -> &'static str {
        use crate::plugin_db as db;
        match self {
            Self::Comp => db::NATIVE_COMP_PICKER_ID,
            Self::Eq => db::NATIVE_EQ_PICKER_ID,
            Self::BusComp => db::NATIVE_BUS_COMP_PICKER_ID,
            Self::ToneEq => db::NATIVE_TONE_EQ_PICKER_ID,
            Self::Reverb => db::NATIVE_REVERB_PICKER_ID,
            Self::Delay => db::NATIVE_DELAY_PICKER_ID,
        }
    }

    /// [`Self::picker_id`] の逆。
    #[must_use]
    pub fn from_picker_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.picker_id() == id)
    }
}

/// 内蔵 device の値 (種類ごと)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum NativeParams {
    Comp(CompSettings),
    Eq(EqSettings),
    BusComp(BusCompSettings),
    ToneEq(ToneEqSettings),
    Reverb(ReverbSettings),
    Delay(DelaySettings),
}

impl NativeParams {
    #[must_use]
    pub fn kind(&self) -> NativeKind {
        match self {
            Self::Comp(_) => NativeKind::Comp,
            Self::Eq(_) => NativeKind::Eq,
            Self::BusComp(_) => NativeKind::BusComp,
            Self::ToneEq(_) => NativeKind::ToneEq,
            Self::Reverb(_) => NativeKind::Reverb,
            Self::Delay(_) => NativeKind::Delay,
        }
    }

    /// 既定値 (組み込みも picker 追加も同じ。違うのは `bypassed` だけ)。
    #[must_use]
    pub fn default_of(kind: NativeKind) -> Self {
        match kind {
            NativeKind::Comp => Self::Comp(CompSettings::default()),
            NativeKind::Eq => Self::Eq(EqSettings::default()),
            NativeKind::BusComp => Self::BusComp(BusCompSettings::default()),
            NativeKind::ToneEq => Self::ToneEq(ToneEqSettings::default()),
            NativeKind::Reverb => Self::Reverb(ReverbSettings::default()),
            NativeKind::Delay => Self::Delay(DelaySettings::default()),
        }
    }

    /// 住所 → plain 値。`On` / 種類違い / 実在しない住所は `None`。段階式は段 index。
    #[must_use]
    pub fn get(&self, p: NativeParamId) -> Option<f32> {
        if !p.exists() {
            return None;
        }
        match (self, p) {
            (Self::Comp(s), NativeParamId::Comp(q)) => Some(s.param(q)),
            (Self::Eq(s), NativeParamId::Eq { band, param }) => Some(s.param(band, param)),
            (Self::BusComp(s), NativeParamId::BusComp(q)) => Some(s.param(q)),
            (Self::ToneEq(s), NativeParamId::ToneEq(b)) => Some(s.gain_db(b)),
            (Self::Reverb(s), NativeParamId::Reverb(q)) => Some(s.param(q)),
            (Self::Delay(s), NativeParamId::Delay(q)) => Some(s.param(q)),
            _ => None,
        }
    }

    /// 住所へ書く。`On` / 種類違い / 実在しない住所 / 非有限値は書かずに false。値域へ clamp し、
    /// 段階式は段へ丸める。戻り値 = 実際に変わったか。
    pub fn set(&mut self, p: NativeParamId, v: f32) -> bool {
        if !p.exists() || !v.is_finite() {
            return false;
        }
        let v = p.range().clamp(v);
        let slot = match (self, p) {
            (Self::Comp(s), NativeParamId::Comp(q)) => s.param_mut(q),
            (Self::Eq(s), NativeParamId::Eq { band, param }) => s.param_mut(band, param),
            (Self::BusComp(s), NativeParamId::BusComp(q)) => return s.write(q, v),
            (Self::ToneEq(s), NativeParamId::ToneEq(b)) => s.gain_mut(b),
            (Self::Reverb(s), NativeParamId::Reverb(q)) => return s.write(q, v),
            (Self::Delay(s), NativeParamId::Delay(q)) => return s.write(q, v),
            _ => return false,
        };
        let changed = *slot != v;
        *slot = v;
        changed
    }

    /// フィールド単位の値域回復 (住所の集合ではなく全フィールド)。冪等。
    pub fn sanitize(&mut self) {
        match self {
            Self::Comp(s) => s.sanitize(),
            Self::Eq(s) => s.sanitize(),
            Self::BusComp(s) => s.sanitize(),
            Self::ToneEq(s) => s.sanitize(),
            Self::Reverb(s) => s.sanitize(),
            Self::Delay(s) => s.sanitize(),
        }
    }
}

/// チェーン上の内蔵 device。`Copy` (値 IPC とRT の値解決で確保しないため)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct NativeDevice {
    /// 安定 id (plugin / parallel / chain と同じ空間)。`0` = 未採番 sentinel。
    #[serde(default)]
    pub id: u64,
    /// 組み込み (トラックに 1 個ずつ必ずある、削除できない) か。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub builtin: bool,
    /// 表示番号。`1` = 番号なし、`2..` = 「Comp 2」、`0` = 未採番 (正規化が振る)。
    #[serde(default)]
    pub ordinal: u16,
    /// ON/OFF の唯一の SSoT。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bypassed: bool,
    pub params: NativeParams,
    /// 外部サイドチェインの配線 (Comp / Bus Comp だけが意味を持つ。他は正規化が `None` に落とす)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aux_input: Option<AuxInputRoute>,
}

impl NativeDevice {
    /// 組み込み (既定は bypass = 旧 strip の既定 `on: false` と同じ音)。
    #[must_use]
    pub fn new_builtin(kind: NativeKind, id: u64) -> Self {
        Self {
            id,
            builtin: true,
            ordinal: 1,
            bypassed: true,
            params: NativeParams::default_of(kind),
            aux_input: None,
        }
    }

    /// picker 等で足した追加分 (既定は ON)。
    #[must_use]
    pub fn new_added(kind: NativeKind, id: u64, ordinal: u16) -> Self {
        Self {
            id,
            builtin: false,
            ordinal,
            bypassed: false,
            params: NativeParams::default_of(kind),
            aux_input: None,
        }
    }

    #[must_use]
    pub fn kind(&self) -> NativeKind {
        self.params.kind()
    }

    /// 住所 → plain 値。`On(自分の種類)` は `!bypassed` (1 / 0)。
    #[must_use]
    pub fn param(&self, p: NativeParamId) -> Option<f32> {
        match p {
            NativeParamId::On(k) => (k == self.kind()).then_some(if self.bypassed { 0.0 } else { 1.0 }),
            _ => self.params.get(p),
        }
    }

    /// 住所へ書く。`On(自分の種類)` は `bypassed = v < 0.5`。**自動 ON はしない**
    /// (GUI の `NativeEdit::apply` が担う)。戻り値 = 実際に変わったか。
    pub fn set_param(&mut self, p: NativeParamId, v: f32) -> bool {
        match p {
            NativeParamId::On(k) => {
                if k != self.kind() || !v.is_finite() {
                    return false;
                }
                let bypassed = v < 0.5;
                std::mem::replace(&mut self.bypassed, bypassed) != bypassed
            }
            _ => self.params.set(p, v),
        }
    }

    /// 値 IPC (`SetNativeDevice`) の受け口。種類違いは false。`params` を sanitize してから
    /// 置き換え、変化があれば true。
    pub fn replace_values(&mut self, bypassed: bool, mut params: NativeParams) -> bool {
        if params.kind() != self.kind() {
            return false;
        }
        params.sanitize();
        let changed = self.bypassed != bypassed || self.params != params;
        self.bypassed = bypassed;
        self.params = params;
        changed
    }

    /// 値の値域回復。構造 (builtin / ordinal / aux) には触らない。
    pub fn sanitize(&mut self) {
        self.params.sanitize();
    }

    /// 行 / レーンに出す名前。`ordinal <= 1` は種類名だけ、それ以外は `"{label} {ordinal}"`。
    #[must_use]
    pub fn display_name(&self) -> Cow<'static, str> {
        if self.ordinal <= 1 {
            Cow::Borrowed(self.kind().label())
        } else {
            Cow::Owned(format!("{} {}", self.kind().label(), self.ordinal))
        }
    }

    /// この device を**処理しうる**か: 静的に ON、または enabled な `On` レーン / 変調がある。
    /// compile / SC 会計 / tap の emit はこの 1 本で判定する。`lanes` / `routings` は
    /// device の持ち主の store (`Song::param_stores`)。
    #[must_use]
    pub fn can_activate(&self, lanes: &[AutomationLane], routings: &[ModRouting]) -> bool {
        if !self.bypassed {
            return true;
        }
        let on = AutomationTarget::NativeParam { device_id: self.id, param: NativeParamId::On(self.kind()) };
        lanes.iter().any(|l| l.enabled && l.target == on) || routings.iter().any(|r| r.enabled && r.target == on)
    }

    /// 外部サイドチェインの配線 (受ける種類のときだけ)。
    #[must_use]
    pub fn sidechain_input(&self) -> Option<&AuxInputRoute> {
        if self.kind().accepts_sidechain() { self.aux_input.as_ref() } else { None }
    }
}
