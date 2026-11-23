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

/// r.md #158 仕様 14: トラックで録れる入力の種類。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecordableInputs {
    pub midi: bool,
    pub audio: bool,
}

impl RecordableInputs {
    /// どちらかを録れる (インスペクタの入力の帯を出す)。
    #[must_use]
    pub fn any(self) -> bool {
        self.midi || self.audio
    }

    /// `input` の種類を録れるか。
    #[must_use]
    pub fn allows(self, input: TrackInput) -> bool {
        if input.is_midi() { self.midi } else { self.audio }
    }

    /// 2 つを合わせたもの (どちらかで録れる)。
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        Self { midi: self.midi || other.midi, audio: self.audio || other.audio }
    }
}

/// 「このトラックで効く入力」の規則 (r.md #158)。インスペクタの入力の帯 (選択肢と書き込み先)、録音 (MIDI の宛先と
/// 音声の録音の要求)、engine の入力の流し込み (モニターと入力メーター、`daw_audio::graph::input_stage`) が
/// 全部これを通る — 帯で隠した入力が、録音とモニターでだけ効き続けることが無い。保存してある `Track::input` は
/// 書き換えない (画像や子を外せば元の入力が戻る。黙って付け替えない)。
///
/// トラックの「種類」は持たず (`[[feedback_no_role_classification]]`)、中身から導く:
/// - **MIDI**: ノートを受ける device (Parallel の中も。port 未解決は受けるとみなす) か MIDI クリップを持つ、
///   または まだ何者でもない (子も、クリップも、組み込み以外の device も無い) トラック。
/// - **音声**: 入力を受けるバスでなく (子を持つグループ / send を受けるリターン / パラアウトの宛先 — engine は
///   これらへ入力を流さず、自分のクリップも鳴らさない。`daw_audio::graph::compile::deps` の bus 判定と同じ)、
///   映像系の中身 (画像 / 映像 / テキストのクリップ、映像 device) を持たないトラック。
/// - master はどちらも録れない。
///
/// 曲 1 枚につき 1 回作り、トラックごとに曲全体を歩き直さない (選択中の全トラック / 便ごとの全トラック)。
pub struct InputRules<'s> {
    song: &'s super::Song,
    /// 子を持つトラック (グループのバス)。
    parents: std::collections::HashSet<u32>,
    /// 入力を受けるバス (子を持つ / send を受ける / パラアウトの宛先)。
    buses: std::collections::HashSet<u32>,
}

impl<'s> InputRules<'s> {
    #[must_use]
    pub fn new(song: &'s super::Song) -> Self {
        let parents: std::collections::HashSet<u32> = song.tracks.iter().filter_map(|t| t.parent_group_id).collect();
        let send_dests = song.tracks.iter().flat_map(|t| t.sends.iter().map(|s| s.dest_track_id));
        let chains = song.tracks.iter().map(|t| t.devices.as_slice()).chain(std::iter::once(song.master_fx_chain.as_slice()));
        let paraout_dests = chains
            .flat_map(super::plugins)
            .flat_map(|p| p.aux_outputs.iter().take(crate::process_data::MAX_AUX_OUT).flatten().map(|r| r.dest_track));
        let buses = parents.iter().copied().chain(send_dests).chain(paraout_dests).collect();
        Self { song, parents, buses }
    }

    /// `track` で録れる入力の種類。
    #[must_use]
    pub fn recordable(&self, track: &super::Track) -> RecordableInputs {
        use super::ClipContent as C;
        if track.id == super::MASTER_TRACK_ID {
            return RecordableInputs::default();
        }
        let has_children = self.parents.contains(&track.id);
        let bus = self.buses.contains(&track.id);
        let note_device = super::any_plugin(&track.devices, &mut |p| p.ports.has_note_input || p.ports.is_unresolved());
        let video_device = super::any_plugin(&track.devices, &mut |p| p.ports.is_video());
        // クリップを見るのは結果が中身で変わるときだけ: 音声は映像系のクリップが 1 つも無いことを、MIDI は
        // (ノートを受ける device が無ければ) MIDI クリップか何も無いことを見る。
        let audio_open = !bus && !video_device;
        let (mut midi_clips, mut visual_clips, mut any_clip) = (false, false, false);
        if audio_open || !note_device {
            for clip in track.all_clips() {
                any_clip = true;
                match self.song.clip_contents.get(&clip.content_id) {
                    Some(C::Midi(_)) => midi_clips = true,
                    Some(C::Image(_) | C::Video(_) | C::Text(_)) => visual_clips = true,
                    _ => {}
                }
                if midi_clips && visual_clips {
                    break;
                }
            }
        }
        let added_device = track.devices.iter().any(|d| !d.is_builtin_native());
        let blank = !has_children && !any_clip && !added_device;
        RecordableInputs { midi: note_device || midi_clips || blank, audio: audio_open && !visual_clips }
    }

    /// `track` で効く入力: 保存してある入力を、そのトラックで録れるときだけ (録れなければ `None` = 何も録らず、
    /// 音声入力ならモニターもしない)。
    #[must_use]
    pub fn effective_input(&self, track: &super::Track) -> Option<TrackInput> {
        self.recordable(track).allows(track.input).then_some(track.input)
    }
}

impl super::Song {
    /// [`InputRules::recordable`] (1 本だけ見るとき。何本も見るなら [`InputRules`] を 1 回作る)。
    #[must_use]
    pub fn recordable_inputs(&self, track: &super::Track) -> RecordableInputs {
        InputRules::new(self).recordable(track)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AudioContent, AuxOutputRoute, Clip, ClipContent, Device, ImageContent, MASTER_TRACK_ID, MidiContent, NativeDevice,
        NativeKind, PluginInstance, Send, Song, TextContent, Track,
    };
    use crate::plugin_format::PluginFormat;
    use crate::port_config::PortConfig;

    fn plugin(ports: PortConfig) -> Device {
        Device::Plugin(PluginInstance { ports, ..PluginInstance::new("x".into(), PluginFormat::Builtin) })
    }

    /// 新しいトラックと同じ、組み込みの Comp / EQ だけを持つ chain (+ `extra`)。
    fn chain(extra: Vec<Device>) -> Vec<Device> {
        let mut d = vec![
            Device::Native(NativeDevice::new_builtin(NativeKind::Comp, 0)),
            Device::Native(NativeDevice::new_builtin(NativeKind::Eq, 0)),
        ];
        d.extend(extra);
        d
    }

    fn add(song: &mut Song, id: u32, devices: Vec<Device>, clip: Option<ClipContent>, parent: Option<u32>) {
        let mut t = Track { id, devices, parent_group_id: parent, ..Track::default() };
        if let Some(content) = clip {
            let content_id = song.alloc_content(content, String::new());
            t.place_clip(Clip { length_beats: 4.0, content_id, ..Clip::default() });
        }
        song.tracks.push(t);
    }

    /// r.md #158 仕様 14: 録れる入力はトラックの中身から決まる。入力を受けるバス (グループ / リターン / パラアウトの
    /// 宛先) は engine が入力を流さないので音声を録れない。
    #[test]
    fn recordable_inputs_follow_what_the_track_holds() {
        let instrument = PortConfig { has_note_input: true, has_audio_output: true, ..PortConfig::default() };
        let effect = PortConfig { has_audio_input: true, has_audio_output: true, ..PortConfig::default() };
        let video = PortConfig { has_video_input: true, has_video_output: true, ..PortConfig::default() };
        let midi = || Some(ClipContent::Midi(MidiContent::default()));
        let mut song = Song::default();
        song.tracks.clear();
        add(&mut song, 1, chain(vec![plugin(instrument)]), midi(), None); // Synth
        add(&mut song, 2, chain(vec![plugin(instrument)]), None, None); // 歌声 (VOICEVOX = ノートを受ける)
        add(&mut song, 3, chain(vec![plugin(effect)]), Some(ClipContent::Audio(AudioContent::default())), None); // Audio
        add(&mut song, 4, chain(vec![plugin(video)]), Some(ClipContent::Image(ImageContent::default())), None); // 画像 + 映像 FX
        add(&mut song, 5, chain(vec![]), None, None); // グループのバス
        add(&mut song, 6, chain(vec![]), None, Some(5));
        add(&mut song, 7, chain(vec![plugin(instrument)]), None, None); // 楽器付きグループ (パラアウト)
        add(&mut song, 8, chain(vec![]), midi(), Some(7));
        add(&mut song, 9, chain(vec![]), None, None); // 空の新規トラック
        add(&mut song, 10, chain(vec![plugin(instrument), plugin(video)]), Some(ClipContent::Text(TextContent::default())), None); // 読み上げ + 字幕
        add(&mut song, 11, chain(vec![Device::Native(NativeDevice::new_added(NativeKind::Reverb, 0, 1))]), None, None); // 効果だけ
        add(&mut song, 12, chain(vec![]), None, None); // リターン (11 の send を受ける)
        let send = Send { id: 1, dest_track_id: 12, gain: 1.0, mode: crate::model::SendMode::default(), enabled: true };
        song.track_by_id_mut(11).expect("11").sends.push(send);
        // パラアウトの宛先 (1 の楽器の出力 1 を受ける)。
        add(&mut song, 13, chain(vec![]), None, None);
        if let Some(Device::Plugin(p)) = song.track_by_id_mut(1).expect("1").devices.last_mut() {
            p.aux_outputs = vec![Some(AuxOutputRoute::to_track(13))];
        }
        let both = RecordableInputs { midi: true, audio: true };
        let midi_only = RecordableInputs { midi: true, audio: false };
        let audio_only = RecordableInputs { midi: false, audio: true };
        let none = RecordableInputs::default();
        let rules = InputRules::new(&song);
        for (id, what, want) in [
            (1, "Synth", both),
            (2, "歌声", both),
            (3, "Audio", audio_only),
            (4, "画像 + 映像 FX", none),
            (5, "グループのバス", none),
            (7, "楽器付きグループ", midi_only),
            (9, "空の新規トラック", both),
            (10, "読み上げ + 字幕", midi_only),
            (11, "効果だけ", audio_only),
            (12, "リターン", midi_only),
            (13, "パラアウトの宛先", midi_only),
        ] {
            let track = song.track_by_id(id).expect("track");
            assert_eq!(rules.recordable(track), want, "{what}");
            assert_eq!(song.recordable_inputs(track), want, "{what} (1 本だけ見る口も同じ)");
        }
        let master = Track { id: MASTER_TRACK_ID, ..Track::default() };
        assert_eq!(rules.recordable(&master), none, "マスター");
    }

    /// 効く入力: 録れない入力は保存したまま効かない (画像を置いた / グループにした / リターンにした)。
    #[test]
    fn effective_input_drops_inputs_the_track_cannot_record() {
        let audio_in = TrackInput::AudioMono { channel: 0 };
        let mut song = Song::default();
        song.tracks.clear();
        add(&mut song, 1, chain(vec![]), None, None);
        add(&mut song, 2, chain(vec![]), Some(ClipContent::Image(ImageContent::default())), None);
        add(&mut song, 3, chain(vec![]), None, None);
        add(&mut song, 4, chain(vec![]), None, Some(3));
        add(&mut song, 5, chain(vec![]), Some(ClipContent::Audio(AudioContent::default())), None);
        for id in [1, 2, 3, 5] {
            song.track_by_id_mut(id).expect("track").input = audio_in;
        }
        let rules = InputRules::new(&song);
        let effective = |id: u32| rules.effective_input(song.track_by_id(id).expect("track"));
        assert_eq!(effective(1), Some(audio_in), "空のトラックは音声を録れる");
        assert_eq!(effective(2), None, "画像のトラックの音声入力は効かない");
        assert_eq!(effective(3), None, "グループのバスの音声入力は効かない");
        assert_eq!(effective(4), Some(TrackInput::Midi), "まだ何者でもない子トラックは既定の MIDI が効く");
        assert_eq!(song.track_by_id(2).expect("2").input, audio_in, "保存してある入力は書き換えない");
        assert_eq!(effective(5), Some(audio_in));
    }
}
