//! 単一デバイスチェーンの「役割の位置導出」(`docs/plan_linear_chain.md` §4)。
//!
//! トラックは 1 本の `Vec<PluginInstance>` で、各プラグインの役割
//! (生成器 / 音源 / エフェクト) は**保持しない**。チェーンを左→右に 1 回歩いて
//! 各 device の [`PortConfig`] から毎回この純粋関数で導出する。daw_gui の
//! インスペクタ見出しと daw_audio のルーティングが同じ関数を使う (SSoT)。
//!
//! 規則 (実 DAW = Ableton/Bitwig/Ardour と同じ位置依存 + CLAP/VST3 の
//! dual-capable プラグインを下流参照で吸収):
//! - 信号は MIDI で始まる。
//! - MIDI 区間の device は、note 出力を持ち **かつ** (下流に note 入力を持つ機が
//!   ある **または** 自分が音源になれない) なら MIDI を素通し (= 生成器)。
//! - そうでなく note 入力 + audio 出力を持つなら**音源** (ここで信号が audio へ)。
//! - audio 区間の device は audio 出力を持てばエフェクト、無ければ無効。
//! - それ以外 (音源前の audio FX 等) は無効 = 処理 skip。

use crate::port_config::PortConfig;

/// チェーン上の 1 device の導出役割。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceRole {
    /// MIDI を受け取り MIDI を出す (生成器 / MIDI FX)。信号は MIDI のまま。
    Generator,
    /// MIDI→audio 変換 (音源)。ここで信号が audio に切り替わる。
    Instrument,
    /// audio→audio (エフェクト)。
    AudioEffect,
    /// 入力が来ず処理されない (音源前の audio FX、出力を持たない機 等)。
    Inactive,
}

impl DeviceRole {
    /// process で実際に駆動するか (`Inactive` だけ skip)。
    #[must_use]
    pub fn is_active(self) -> bool {
        !matches!(self, DeviceRole::Inactive)
    }
}

/// `ports[i]` = チェーン位置 i の device の [`PortConfig`]。左→右の順。
/// 各 device の役割を導出して返す (長さは `ports` と同じ)。
#[must_use]
pub fn derive_roles(ports: &[PortConfig]) -> Vec<DeviceRole> {
    let n = ports.len();
    // has_note_in_after[i] = i より後ろに note 入力を持つ device があるか。
    // 後方 1 パスで前計算 (dual-capable の「下流が MIDI を欲しがるか」判定用)。
    let mut has_note_in_after = vec![false; n];
    {
        let mut acc = false;
        for i in (0..n).rev() {
            has_note_in_after[i] = acc;
            if ports[i].has_note_input {
                acc = true;
            }
        }
    }
    let mut roles = vec![DeviceRole::Inactive; n];
    let mut signal_midi = true;
    for (i, p) in ports.iter().enumerate() {
        if !signal_midi {
            // audio 区間: audio 出力を持てばエフェクト、無ければ無効。
            roles[i] = if p.has_audio_output {
                DeviceRole::AudioEffect
            } else {
                DeviceRole::Inactive
            };
            continue;
        }
        // MIDI 区間。
        let can_instrument = p.has_note_input && p.has_audio_output;
        let pass_midi = p.has_note_output && (has_note_in_after[i] || !can_instrument);
        if pass_midi {
            roles[i] = DeviceRole::Generator;
        } else if can_instrument {
            roles[i] = DeviceRole::Instrument;
            signal_midi = false;
        } else {
            roles[i] = DeviceRole::Inactive;
        }
    }
    roles
}

/// チェーンの「音源」(= 最初の `Instrument`) の位置。無ければ `None`。
/// インスペクタの境界表示等で使う。
#[must_use]
pub fn instrument_index(ports: &[PortConfig]) -> Option<usize> {
    derive_roles(ports)
        .iter()
        .position(|r| *r == DeviceRole::Instrument)
}

#[cfg(test)]
mod tests {
    use super::{derive_roles, DeviceRole};
    use crate::port_config::PortConfig;

    const fn pc(note_in: bool, note_out: bool, audio_out: bool) -> PortConfig {
        PortConfig {
            has_note_input: note_in,
            has_note_output: note_out,
            has_audio_output: audio_out,
        }
    }
    // 代表プラグイン。
    const SCALER: PortConfig = pc(true, true, true); // dual-capable (生成器にも音源にもなれる)
    const ANALOG_LAB: PortConfig = pc(true, false, true); // 純粋な音源
    const ARP: PortConfig = pc(true, true, false); // 純粋な MIDI FX
    const REVERB: PortConfig = pc(false, false, true); // audio FX

    use DeviceRole::{AudioEffect, Generator, Inactive, Instrument};

    #[test]
    fn scaler_alone_is_instrument() {
        assert_eq!(derive_roles(&[SCALER]), vec![Instrument]);
    }

    #[test]
    fn scaler_then_instrument_makes_scaler_a_generator() {
        // Scaler の下流に音源 (note 入力あり) → Scaler は生成器、AnalogLab が音源。
        assert_eq!(
            derive_roles(&[SCALER, ANALOG_LAB]),
            vec![Generator, Instrument]
        );
    }

    #[test]
    fn removing_instrument_repromotes_dual_capable() {
        // AnalogLab を消した後の [Scaler] は再び音源 (無音バグの構造的解消)。
        assert_eq!(derive_roles(&[SCALER]), vec![Instrument]);
    }

    #[test]
    fn instrument_then_fx() {
        assert_eq!(
            derive_roles(&[ANALOG_LAB, REVERB]),
            vec![Instrument, AudioEffect]
        );
    }

    #[test]
    fn audio_fx_before_instrument_is_inactive() {
        // 音源前の audio FX は入力が来ないので無効 (実 DAW と同じ)。
        assert_eq!(
            derive_roles(&[REVERB, ANALOG_LAB]),
            vec![Inactive, Instrument]
        );
    }

    #[test]
    fn pure_midi_fx_before_instrument_is_generator() {
        assert_eq!(
            derive_roles(&[ARP, ANALOG_LAB]),
            vec![Generator, Instrument]
        );
    }

    #[test]
    fn dual_capable_before_pure_midi_fx_passes_midi() {
        // [Scaler, Arp]: Scaler の下流に note 入力あり → 生成器。Arp も MIDI を出す
        // だけで音源が無いので結果的に無音 (= 予測可能。音源を足せば鳴る)。
        assert_eq!(derive_roles(&[SCALER, ARP]), vec![Generator, Generator]);
    }

    #[test]
    fn full_chain_generator_instrument_fx() {
        assert_eq!(
            derive_roles(&[SCALER, ANALOG_LAB, REVERB]),
            vec![Generator, Instrument, AudioEffect]
        );
    }

    #[test]
    fn empty_chain() {
        assert_eq!(derive_roles(&[]), Vec::<DeviceRole>::new());
    }

    #[test]
    fn second_instrument_after_first_is_audio_region() {
        // 2 つ目の音源は audio 区間に入る → audio 出力を持つのでエフェクト扱い
        // (= 実 DAW の「1 トラック 1 音源」と同じく 2 つ目は音源として働かない)。
        assert_eq!(
            derive_roles(&[ANALOG_LAB, ANALOG_LAB]),
            vec![Instrument, AudioEffect]
        );
    }
}
