//! MIDI Learn の割り当て表 (`Song::midi_bindings`)。
//!
//! **表は 1 本**で、連続値のパラメータ (CC) と、押した / 離したで効くランチャー操作
//! (ノート / CC) が同じ表に載る。分けると「この CC は何に割り当てたか」を 2 か所
//! 探すことになり、同じ入力を 2 つの表に bind できてしまう。
//!
//! model.rs (実コード 1,000 行 budget を大きく超えた god file、不変条件 9) から
//! 切り出した — v35 でノート対応を足す前に分割している。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// MIDI Learn で受ける入力。**MIDI 割り当ての表は 1 本** ([`Song::midi_bindings`])
/// で、パラメータもランチャーも同じ表に載る。パッドはノートで撃つので CC だけでは
/// 足りない (v35 / r.md #87、`docs/plan_rmd_87_clip_launcher.md` §3.5)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub enum MidiBindInput {
    /// CC 番号 0..=127。連続値のパラメータはこれ。押下判定は値 `>= 64`。
    ControlChange(u8),
    /// ノート番号 0..=127。note-on が押下、note-off が離し。
    Note(u8),
}

impl MidiBindInput {
    /// v34 以前の `.daw` は CC しか持たないので、旧 `controller` から起こす。
    #[must_use]
    pub fn cc(controller: u8) -> Self {
        Self::ControlChange(controller)
    }
}

/// Phase 7 B1-M Step 2 (2026-05-13): MIDI Learn binding 1 件 (= 入力 → target)。
/// `channel = 16` は any-channel (= channel-agnostic、 全 16 channel にマッチ)。
/// 同じ `(channel, input)` の重複は許容しない (= GUI 側 handler が
/// 新規 bind 時に既存 entry を replace する)。
///
/// v35 (r.md #87): 入力が CC 固定 (`controller: u8`) から [`MidiBindInput`] になり、
/// ノートでも撃てるようになった。旧 `controller` は deserialize 専用に降格し、
/// `Song::ensure_midi_binding_inputs` が load 時に `input` へ移す。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct MidiBinding {
    /// MIDI channel 0..15、 または 16 = any-channel (= channel 無視で match)。
    pub channel: u8,
    /// 受ける入力 (CC / ノート)。
    #[serde(default = "default_bind_input")]
    pub input: MidiBindInput,
    /// v34 以前 migration 用 (deserialize 専用)。旧 save は CC 番号だけを持つ。
    #[serde(default, rename = "controller", skip_serializing)]
    pub legacy_controller: Option<u8>,
    /// 入力を受けたときに更新する parameter / 実行するランチャー操作。
    pub target: BindingTarget,
}

impl MidiBinding {
    /// 受信した MIDI がこの binding に当たるか (`channel == 16` は any-channel)。
    #[must_use]
    pub fn matches(&self, channel: u8, input: MidiBindInput) -> bool {
        (self.channel == channel || self.channel == 16) && self.input == input
    }
}

/// `input` を持たない v34 以前の file 用。`legacy_controller` が
/// [`Song::ensure_midi_binding_inputs`] で上書きするための仮値。
fn default_bind_input() -> MidiBindInput {
    MidiBindInput::ControlChange(0)
}

/// MIDI Learn の bind 先。 TrackVolume / TrackPan / SongTempo / PluginParam。
/// CC 受信時は `apply_midi_value_to_target` が各 target に値を反映する
/// (PluginParam は param range で value_real に変換し inspector knob と同じ
/// lane-default 経路で plugin host へ、 r.md #8 B2)。 transport の Learn button が
/// 「直近に触った param」 を bind する (touch + learn)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum BindingTarget {
    /// `Track.volume` (0.0..=1.0、 CC 0..127 を linear マップ)。
    TrackVolume(u32),
    /// `Track.pan` (-1.0..=1.0、 CC 0..127 を `value*2/127 - 1` で linear マップ)。
    TrackPan(u32),
    /// `Song.bpm` (60.0..=180.0、 CC 0..127 を linear マップ)。 SongTempo
    /// curve とは独立 (= curve がある場合は curve が優先、 CC は base bpm を
    /// 動かすイメージ)。
    SongTempo,
    /// plugin parameter bind (r.md #8 B2)。 安定 `device_id` で plugin instance を
    /// 特定 (`AutomationTarget::PluginParam` と同じ addressing)、 `param_id` は
    /// format ごと (CLAP `clap_id` / VST3 `ParamID`)。 CC 受信時は
    /// `apply_midi_value_to_target` が param range で value_real に変換し、
    /// inspector knob と同じ lane-default 経路で plugin host へ反映する。
    PluginParam {
        /// v29: 安定 device id (`PluginInstance::id`)。`0` は未解決 sentinel。
        #[serde(default)]
        device_id: u64,
        param_id: u32,
        /// v28 以前 migration 用 (deserialize 専用)。旧 save は chain 内 positional
        /// index、または旧 `slot: PluginSlot` (load 時 JSON 前処理
        /// `project::migrate_legacy_device_chains` が chain 長から index へ解決。
        /// r.md #8 M7: 旧 PluginParam binding が device_index 欠落で deserialize を
        /// 落としていたのを是正) を持つ。`Song::ensure_ids` の remap pass が安定 device_id へ写像。
        #[serde(default, rename = "device_index", skip_serializing)]
        legacy_device_index: Option<u32>,
        /// v33 以前 migration 用 (deserialize 専用)。`legacy_device_index` を解決する
        /// ための所属 track。実行時の解決は `find_device_by_id` なので読まれない
        /// (r.md #71 (プラグインのコピー / 移動): device 移動で stale になる
        /// フィールドを保存しない)。
        #[serde(default, rename = "track", skip_serializing)]
        legacy_track: Option<u32>,
    },
    // ---- v35 (r.md #87 クリップランチャー): パッドから撃つ操作 ----------------
    // どれも「値」ではなく「押した / 離した」で効く (連続値を持たない)。
    // 宛先は安定 id。**セルは `clip.id` ではなく `(track_id, scene_id)`** で指す —
    // パッドの物理位置は「このトラックのこの列」に対応するので、セルを差し替えても
    // 同じパッドが新しいセルを撃つのが正しい。
    /// 行 × 列 のセルを撃つ。その列にセルが無ければ行を止める (計画書 Q11)。
    LaunchCell { track_id: u32, scene_id: u32 },
    /// 列を丸ごと撃つ。
    LaunchScene { scene_id: u32 },
    /// 行の Stop Clips。
    StopLauncherRow { track_id: u32 },
    /// 全行の Stop Clips。
    StopAllLauncherRows,
    /// 行をアレンジ主導へ戻す。
    SwitchRowToArranger { track_id: u32 },
    /// 全行をアレンジ主導へ戻す。
    SwitchAllToArranger,
}

impl BindingTarget {
    /// この bind 先が **押した / 離した** で効くランチャー操作か
    /// (`false` = CC の値で連続的に動かすパラメータ)。
    #[must_use]
    pub fn is_launcher(self) -> bool {
        matches!(
            self,
            Self::LaunchCell { .. }
                | Self::LaunchScene { .. }
                | Self::StopLauncherRow { .. }
                | Self::StopAllLauncherRows
                | Self::SwitchRowToArranger { .. }
                | Self::SwitchAllToArranger
        )
    }

    /// Learn ボタン / 一覧に出す日本語ラベル。
    #[must_use]
    pub fn launcher_label(self) -> Option<&'static str> {
        Some(match self {
            Self::LaunchCell { .. } => "セルを撃つ",
            Self::LaunchScene { .. } => "シーンを撃つ",
            Self::StopLauncherRow { .. } => "行を止める",
            Self::StopAllLauncherRows => "全行を止める",
            Self::SwitchRowToArranger { .. } => "行をアレンジへ戻す",
            Self::SwitchAllToArranger => "全行をアレンジへ戻す",
            _ => return None,
        })
    }
}

impl crate::model::Song {
    /// v35 (r.md #87): v34 以前の `.daw` が持つ CC 専用 binding
    /// (`controller: u8`) を [`MidiBindInput`] へ移す。
    ///
    /// **冪等** — 2 回呼んでも同じ結果 (`legacy_controller` を消費して `None` に
    /// するので、2 回目は何もしない)。開いただけで `*` が立たないための条件
    /// (r.md #9)。
    pub fn ensure_midi_binding_inputs(&mut self) {
        for b in &mut self.midi_bindings {
            if let Some(cc) = b.legacy_controller.take() {
                b.input = MidiBindInput::cc(cc);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Song;

    #[test]
    fn v34_の_cc_binding_は_input_へ移り再実行しても変わらない() {
        // v34 以前の save には `controller` しか無い (deserialize で
        // `legacy_controller` に入り、`input` は仮値のまま)。
        let mut song = Song {
            midi_bindings: vec![MidiBinding {
                channel: 3,
                input: MidiBindInput::ControlChange(0),
                legacy_controller: Some(74),
                target: BindingTarget::TrackVolume(1),
            }],
            ..Song::default()
        };

        song.ensure_midi_binding_inputs();
        assert_eq!(song.midi_bindings[0].input, MidiBindInput::ControlChange(74));
        assert_eq!(song.midi_bindings[0].legacy_controller, None, "消費して None にする");

        // 冪等 — 2 回目で仮値へ戻らない (戻ると開くだけで割り当てが壊れる)。
        let once = song.clone();
        song.ensure_midi_binding_inputs();
        assert_eq!(song, once);
    }

    #[test]
    fn any_channel_は_全チャンネルに当たる() {
        let b = MidiBinding {
            channel: 16,
            input: MidiBindInput::Note(36),
            legacy_controller: None,
            target: BindingTarget::LaunchScene { scene_id: 2 },
        };
        assert!(b.matches(0, MidiBindInput::Note(36)));
        assert!(b.matches(15, MidiBindInput::Note(36)));
        assert!(!b.matches(0, MidiBindInput::Note(37)), "ノート番号が違えば当たらない");
        assert!(
            !b.matches(0, MidiBindInput::ControlChange(36)),
            "同じ番号でも CC とノートは別の入力"
        );
    }
}
