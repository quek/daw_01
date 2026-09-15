//! r.md #129: 内蔵 device (`Device::Native`) と master Limiter の値編集
//! (`docs/plan_rack_native_devices.md` §9.1 / §10.1)。wire を渡らない GUI 内の型。
//!
//! **自動 ON の唯一の SSoT は [`NativeEdit::apply`]** — Rack の Par / Mixer 帯 / マスターパネル /
//! カーブ点 / MIDI Learn のどこから触っても、値に触れたらその device を ON にする (Q15)。
//! bypass を明示的に書くのは `DeviceEvent::SetDevicesBypassed` だけ。

use common::model::{
    CompMode, EqBand, MasterLimiterParam, MasterLimiterSettings, NativeDevice, NativeParamId, NativeParams,
};

/// 内蔵 device 1 台への値編集。
#[derive(Debug, Clone, PartialEq)]
pub enum NativeEdit {
    /// 連続 / 段階のパラメーター (plain 単位)。カーブ点のドラッグは Freq + Gain を 1 イベントで運ぶ。
    /// `On(_)` は無視する (bypass は `SetDevicesBypassed` の責務)。
    Params(Vec<(NativeParamId, f32)>),
    /// EQ バンドの ON/OFF (どのバンドでも受け付ける。UI が出すのは HP / LP だけ)。
    EqBandOn { band: EqBand, on: bool },
    /// LF / HF のシェルフ ⇄ ベル。
    EqBell { band: EqBand, bell: bool },
    /// Comp の動作モード。
    CompMode(CompMode),
}

impl NativeEdit {
    /// パラメーター 1 個の編集。
    #[must_use]
    pub fn param(p: NativeParamId, v: f32) -> Self {
        Self::Params(vec![(p, v)])
    }

    /// `dev` へ適用する。**自動 ON の唯一の SSoT** (§10.1)。戻り値 = 実際に変わったか
    /// (値 IPC と dirty の根拠)。
    pub fn apply(&self, dev: &mut NativeDevice) -> bool {
        let mut changed = false;
        let mut touched = false;
        match self {
            Self::Params(params) => {
                for &(p, v) in params {
                    if matches!(p, NativeParamId::On(_)) || dev.param(p).is_none() {
                        continue;
                    }
                    changed |= dev.set_param(p, v);
                    touched = true;
                    // OFF のバンドを触ったらそのバンドを ON (カーブ点と同じ規則をつまみにも当てる)。
                    if let (Some(band), NativeParams::Eq(eq)) = (p.eq_band(), &mut dev.params) {
                        changed |= !std::mem::replace(&mut eq.band_mut(band).on, true);
                    }
                }
            }
            Self::EqBandOn { band, on } => {
                if let NativeParams::Eq(eq) = &mut dev.params {
                    changed |= std::mem::replace(&mut eq.band_mut(*band).on, *on) != *on;
                    touched = true;
                }
            }
            Self::EqBell { band, bell } => {
                if let NativeParams::Eq(eq) = &mut dev.params
                    && band.has_bell_switch()
                {
                    let b = eq.band_mut(*band);
                    changed |= std::mem::replace(&mut b.bell, *bell) != *bell;
                    changed |= !std::mem::replace(&mut b.on, true);
                    touched = true;
                }
            }
            Self::CompMode(mode) => {
                if let NativeParams::Comp(c) = &mut dev.params {
                    changed |= std::mem::replace(&mut c.mode, *mode) != *mode;
                    touched = true;
                }
            }
        }
        // Q15: Q で OFF にした直後でも、触れば ON に戻る。
        if touched {
            changed |= std::mem::replace(&mut dev.bypassed, false);
        }
        changed
    }

    /// undo ラベル (値の中身ではなく device の種類 `NativeKind::undo_label` で決まる)。
    #[must_use]
    pub fn undo_label(&self) -> &'static str {
        let kind = match self {
            Self::Params(params) => match params.first() {
                Some((p, _)) => p.kind(),
                None => return crate::state::song_doc::GENERIC_UNDO_LABEL,
            },
            Self::EqBandOn { .. } | Self::EqBell { .. } => common::model::NativeKind::Eq,
            Self::CompMode(_) => common::model::NativeKind::Comp,
        };
        kind.undo_label()
    }
}

/// master のフェーダー後 Limiter への編集 (チェーン外)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MasterLimiterEdit {
    /// 明示的な ON/OFF (自動 ON の対象外)。
    On(bool),
    /// シーリング (dB)。触ったら ON にする。
    Ceiling(f32),
}

impl MasterLimiterEdit {
    /// `l` へ適用する。戻り値 = 実際に変わったか。
    pub fn apply(self, l: &mut MasterLimiterSettings) -> bool {
        match self {
            Self::On(on) => std::mem::replace(&mut l.on, on) != on,
            Self::Ceiling(v) => {
                let changed = l.set_param(MasterLimiterParam::Ceiling, v);
                changed | !std::mem::replace(&mut l.on, true)
            }
        }
    }

    /// この編集が触る Limiter の住所 (last touched / オートメーションの的)。
    #[must_use]
    pub fn param(self) -> MasterLimiterParam {
        match self {
            Self::On(_) => MasterLimiterParam::On,
            Self::Ceiling(_) => MasterLimiterParam::Ceiling,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{
        BusCompParam, CompParam, EqParam, NativeKind, ToneEqBand,
    };

    fn bypassed(kind: NativeKind) -> NativeDevice {
        NativeDevice::new_builtin(kind, 1)
    }

    fn eq_of(d: &NativeDevice) -> common::model::EqSettings {
        let NativeParams::Eq(e) = d.params else { panic!("eq") };
        e
    }

    /// F-G1: 自動 ON の規則 (§10.1)。
    #[test]
    fn native_edit_apply_turns_the_device_on_when_values_are_touched() {
        // 4 種とも、bypass 中に値を触れば ON。
        let touches = [
            (NativeKind::Comp, NativeParamId::Comp(CompParam::Threshold), -20.0),
            (NativeKind::Eq, NativeParamId::Eq { band: EqBand::Lmf, param: EqParam::Gain }, 3.0),
            (NativeKind::BusComp, NativeParamId::BusComp(BusCompParam::Ratio), 2.0),
            (NativeKind::ToneEq, NativeParamId::ToneEq(ToneEqBand::High), -2.0),
        ];
        for (kind, p, v) in touches {
            let mut d = bypassed(kind);
            assert!(NativeEdit::param(p, v).apply(&mut d), "{kind:?}");
            assert!(!d.bypassed, "{kind:?}: 触ったら ON");
            assert!(!NativeEdit::param(p, v).apply(&mut d), "{kind:?}: 同じ値の再送は変化なし");
        }
        // `On` は Params では書かない (bypass の責務は SetDevicesBypassed)。
        let mut d = bypassed(NativeKind::Comp);
        assert!(!NativeEdit::param(NativeParamId::On(NativeKind::Comp), 1.0).apply(&mut d));
        assert!(d.bypassed);
        let mut on = NativeDevice::new_added(NativeKind::Comp, 2, 1);
        assert!(!NativeEdit::param(NativeParamId::On(NativeKind::Comp), 0.0).apply(&mut on));
        assert!(!on.bypassed);

        // HP Freq → hp.on、lp.on は不変。
        let mut d = bypassed(NativeKind::Eq);
        let hp = NativeParamId::Eq { band: EqBand::Hp, param: EqParam::Freq };
        assert!(NativeEdit::param(hp, 150.0).apply(&mut d));
        assert!(eq_of(&d).hp.on && !eq_of(&d).lp.on);
        // EqBandOn{Lp,false} → lp OFF のまま、device は ON。
        let mut d = bypassed(NativeKind::Eq);
        NativeEdit::EqBandOn { band: EqBand::Lp, on: true }.apply(&mut d);
        d.bypassed = true;
        assert!(NativeEdit::EqBandOn { band: EqBand::Lp, on: false }.apply(&mut d));
        assert!(!eq_of(&d).lp.on && !d.bypassed);
        // LMF を OFF にしてから LMF Gain を触ると ON に戻る。
        let mut d = NativeDevice::new_added(NativeKind::Eq, 3, 1);
        NativeEdit::EqBandOn { band: EqBand::Lmf, on: false }.apply(&mut d);
        assert!(!eq_of(&d).lmf.on);
        NativeEdit::param(NativeParamId::Eq { band: EqBand::Lmf, param: EqParam::Gain }, 2.0).apply(&mut d);
        assert!(eq_of(&d).lmf.on);

        // 種類違い / ベルを持たないバンド / Comp 以外の CompMode は何もしない。
        let mut d = bypassed(NativeKind::Eq);
        assert!(!NativeEdit::param(NativeParamId::Comp(CompParam::Ratio), 4.0).apply(&mut d));
        assert!(!NativeEdit::EqBell { band: EqBand::Hmf, bell: true }.apply(&mut d));
        assert!(!NativeEdit::CompMode(CompMode::Limiter).apply(&mut d));
        assert!(d.bypassed, "何も書かなければ ON にもしない");

        // Limiter: Ceiling は clamp + ON、明示 OFF の後は OFF のまま。
        let mut l = MasterLimiterSettings::default();
        assert!(MasterLimiterEdit::Ceiling(5.0).apply(&mut l));
        assert!(l.on && l.ceiling_db == 0.0);
        assert!(MasterLimiterEdit::On(false).apply(&mut l));
        assert!(!l.on);
        assert!(!MasterLimiterEdit::On(false).apply(&mut l));
        assert!(!l.on);
    }
}
