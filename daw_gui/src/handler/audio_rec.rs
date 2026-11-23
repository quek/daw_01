//! handler::audio_rec — オーディオ入力の録音 (r.md #156)。
//!
//! - トラックの「入力」と「モニター」の編集 (インスペクタ)。`Song` の編集なので `edit_song` を通り、
//!   engine へはフレーム末の同期 (`LoadSong`) で届く。
//! - デバイスの入力 ch の一覧 ([`AppData::audio_input_choices`]): インスペクタの「入力」の選択肢。
//! - 録音の開始と終了: 開始で録音待機のオーディオ入力のトラックごとに書き先を決めて engine に渡し
//!   ([`AppData::send_start_recording`])、engine が書き終えたら WAV を素材にしてクリップを置く
//!   ([`AppData::handle_audio_takes_finished`])。音は wire を通らない (不変条件 2)。位置の補正
//!   (入出力のレイテンシ・先読み) は engine が済ませていて、返ってきた拍へそのまま置く。

use std::path::PathBuf;

use common::audio_take::{AudioTakeProgress, AudioTakeRequest, AudioTakeSegment, AudioTakeSpec, AudioTakesFinished};
use common::model::{AudioSourcePath, MASTER_TRACK_ID, MonitorMode, RecordedWav, TrackInput};
use common::protocol::AudioCommand;

use crate::event_audio_rec::AudioRecEvent;
use crate::media_dest::MediaPool;
use crate::state::*;

/// インスペクタの「入力」の選択肢 1 つ。
#[derive(Debug, Clone, PartialEq)]
pub struct InputChoice {
    pub input: TrackInput,
    pub label: String,
}

/// r.md #158 仕様 14: トラックで録れる入力の種類 (インスペクタの「入力」の帯に何を出すか)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecordableInputs {
    pub midi: bool,
    pub audio: bool,
}

impl RecordableInputs {
    /// どちらかを録れる (入力の帯を出す)。
    #[must_use]
    pub fn any(self) -> bool {
        self.midi || self.audio
    }

    /// `input` の種類を録れるか。
    #[must_use]
    pub fn allows(self, input: TrackInput) -> bool {
        if input.is_midi() { self.midi } else { self.audio }
    }
}

/// トラックで録れる入力の種類。トラックの「種類」は持たず (`[[feedback_no_role_classification]]`)、
/// 中身から導く:
/// - **MIDI**: ノートを受ける device (Parallel の中も。port 未解決は受けるとみなす) か MIDI クリップを持つ、
///   または まだ何者でもない (子も、クリップも、組み込み以外の device も無い) トラック。
/// - **音声**: 子トラックを持たず (グループのバスは自分で音を録らない)、映像系の中身 (画像 / 映像 / テキストの
///   クリップ、映像 device) を持たないトラック。
/// - master はどちらも録れない。
#[must_use]
pub fn recordable_inputs(song: &common::model::Song, track: &common::model::Track) -> RecordableInputs {
    use common::model::ClipContent as C;
    if track.id == MASTER_TRACK_ID {
        return RecordableInputs::default();
    }
    let has_children = song.track_has_children(track.id);
    let (mut midi_clips, mut visual_clips, mut any_clip) = (false, false, false);
    for clip in track.all_clips() {
        any_clip = true;
        match song.clip_contents.get(&clip.content_id) {
            Some(C::Midi(_)) => midi_clips = true,
            Some(C::Image(_) | C::Video(_) | C::Text(_)) => visual_clips = true,
            _ => {}
        }
    }
    let note_device =
        common::model::any_plugin(&track.devices, &mut |p| p.ports.has_note_input || p.ports.is_unresolved());
    let video_device = common::model::any_plugin(&track.devices, &mut |p| p.ports.is_video());
    let added_device = track.devices.iter().any(|d| !d.is_builtin_native());
    let blank = !has_children && !any_clip && !added_device;
    RecordableInputs {
        midi: note_device || midi_clips || blank,
        audio: !has_children && !visual_clips && !video_device,
    }
}

/// engine に頼んだ録音 1 回ぶん (結果待ち)。
#[derive(Debug, Clone)]
pub struct PendingAudioTake {
    pub take_id: u64,
    pub tracks: Vec<PendingTakeTrack>,
}

/// 録音 1 回のうちの 1 トラックぶん。
#[derive(Debug, Clone)]
pub struct PendingTakeTrack {
    pub track_id: u32,
    /// engine が書くファイル (絶対パス)。
    pub file: PathBuf,
    /// Song に記録する形 (bundle 相対。未保存なら絶対)。
    pub source_path: AudioSourcePath,
    pub channels: u16,
}

/// 録音中にレーンへ出す帯。
#[derive(Debug, Clone, PartialEq)]
pub struct LiveTakeRegion {
    pub take_id: u64,
    /// 録っているトラック。
    pub track_ids: Vec<u32>,
    /// 帯の `[始まり, 終わり)` の拍。`None` = まだ録り始めていない (count-in 中)。始まりは録音が始まった位置
    /// (ループで折り返したら折り返し先)、終わりは直近の Tick のプレイヘッド。
    pub span: Option<(f64, f64)>,
    /// 帯の中に描く波形 (engine の途中経過から積む)。
    pub wave: LiveTakeWave,
}

/// 録音中の帯に描く波形の材料 (`AudioEvent::AudioTakeProgress` を積んだもの)。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LiveTakeWave {
    /// WAV のサンプルレート (`0` = まだ何も届いていない)。
    pub sample_rate: u32,
    /// いま録っている区間 (届いた中で最後の区間): WAV のこのフレームから先が、曲のこの拍から。
    pub segment: Option<AudioTakeSegment>,
    /// トラックごとの山 (WAV の先頭から `TAKE_PEAK_FRAMES` フレームごと)。
    pub peaks: Vec<(u32, Vec<u8>)>,
}

impl AppData {
    /// [`AppEvent::AudioRec`](crate::app::AppEvent::AudioRec) の入口。
    pub fn handle_audio_rec_event(&mut self, ev: AudioRecEvent) {
        use AudioRecEvent as E;
        match ev {
            E::SetTrackInput { track_ids, input } => self.set_tracks_input(&track_ids, input),
            E::SetTrackMonitor { track_ids, monitor } => self.set_tracks_monitor(&track_ids, monitor),
        }
    }

    /// インスペクタの「入力 / モニター」が編集するトラック: 複数選択ならその全部、そうでなければ
    /// cursor track。録れる入力が 1 つも無いトラック (master / グループのバス / 画像・映像のトラック、
    /// [`recordable_inputs`]) は含めない。
    #[must_use]
    pub fn input_target_track_ids(&self) -> Vec<u32> {
        let song = self.cur.song_doc.song();
        let selected = &self.cur.selection.selected_track_ids;
        let ids: Vec<u32> = if selected.len() > 1 {
            selected.to_vec()
        } else {
            self.cursor_track_id().into_iter().collect()
        };
        ids.into_iter().filter(|&id| song.track_by_id(id).is_some_and(|t| recordable_inputs(song, t).any())).collect()
    }

    /// `track_ids` のどれかで録れる入力の種類 (インスペクタの「入力元」の選択肢)。
    #[must_use]
    pub fn recordable_inputs_of(&self, track_ids: &[u32]) -> RecordableInputs {
        let song = self.cur.song_doc.song();
        track_ids.iter().filter_map(|id| song.track_by_id(*id)).fold(RecordableInputs::default(), |acc, t| {
            let r = recordable_inputs(song, t);
            RecordableInputs { midi: acc.midi || r.midi, audio: acc.audio || r.audio }
        })
    }

    /// 今開いているデバイスで **engine に届いている** 入力 ch (`(デバイスの ch 番号, 名前)`、ch 番号順)。
    /// デバイスにあっても開いていない ch (設定の入力 ch の範囲の外) は含めない — 選んでも無音しか来ない。
    #[must_use]
    pub fn device_inputs(&self) -> Vec<(u16, &str)> {
        let Some(info) = self.audio_device.info.as_ref() else { return Vec::new() };
        info.input_channels
            .iter()
            .filter(|c| info.input_is_active(c.index))
            .map(|c| (c.index, c.name.as_str()))
            .collect()
    }

    /// 入力 `input` の ch が全部 engine に届いているか (MIDI は常に真)。
    #[must_use]
    pub fn track_input_available(&self, input: TrackInput) -> bool {
        let active = |ch: u16| self.audio_device.info.as_ref().is_some_and(|i| i.input_is_active(ch));
        input.audio_channels().is_none_or(|(l, r)| active(l) && active(r))
    }

    /// 「入力元」の選択肢: 録れる種類 (`kinds`) のうち、MIDI、デバイスの入力 ch (モノ)、隣り合う 2 ch
    /// (ステレオ、1+2 / 3+4 …)。`current` (いま選ばれている入力) が選択肢に無ければ、末尾に「無い入力」
    /// (デバイスに無い) / 「録れない」(そのトラックで意味が無い) として足す (黙って別の入力に付け替えない)。
    #[must_use]
    pub fn audio_input_choices(&self, current: &[TrackInput], kinds: RecordableInputs) -> Vec<InputChoice> {
        let mut out = Vec::new();
        if kinds.midi {
            out.push(InputChoice { input: TrackInput::Midi, label: TrackInput::Midi.label() });
        }
        let inputs = if kinds.audio { self.device_inputs() } else { Vec::new() };
        for &(channel, name) in &inputs {
            let input = TrackInput::AudioMono { channel };
            let label = if name.is_empty() { input.label() } else { format!("{} ({name})", input.label()) };
            out.push(InputChoice { input, label });
        }
        // ステレオの組は ch 番号の偶数始まり (1+2 / 3+4 …) で、両方が届いているものだけ。
        for &(left, _) in inputs.iter().filter(|(ch, _)| ch % 2 == 0) {
            let input = TrackInput::AudioStereo { left };
            if self.track_input_available(input) {
                out.push(InputChoice { input, label: format!("{} (ステレオ)", input.label()) });
            }
        }
        for &input in current {
            if !out.iter().any(|c| c.input == input) {
                let why = if kinds.allows(input) { "無い入力" } else { "録れない" };
                out.push(InputChoice { input, label: format!("{} ({why})", input.label()) });
            }
        }
        out
    }

    /// 録音の開始を engine へ送る。録音待機でオーディオ入力のトラックがあれば、**先に** その書き先を渡す
    /// (`StartAudioTakes` → `StartRecording` の順。engine は届いた順に消費する)。
    /// **`StartRecording` を送るのはここだけ** (パンチイン / 停止からの録音 / 読み込み待ち明けの録音)。
    pub(crate) fn send_start_recording(&mut self, preroll_samples: u64) {
        self.request_audio_takes();
        self.send_audio(AudioCommand::StartRecording { project: self.pk(), preroll_samples });
    }

    /// 録音待機でオーディオ入力のトラックごとに WAV の書き先を決め、engine に録音を頼む。
    /// 置き場は取り込んだ音声と同じ (`samples/`。未保存ならこの文書の置き場)。
    fn request_audio_takes(&mut self) {
        let song = self.cur.song_doc.song();
        let wanted: Vec<(u32, String, TrackInput)> = song
            .tracks
            .iter()
            .filter(|t| t.armed && !t.input.is_midi() && song.track_effectively_enabled(t.id))
            .map(|t| (t.id, t.name.clone(), t.input))
            .collect();
        let mut tracks = Vec::new();
        let mut takes = Vec::new();
        let mut missing = Vec::new();
        for (track_id, name, input) in wanted {
            // デバイスに無い入力は録らない (無音のクリップを作らない)。黙って別の ch へ付け替えもしない。
            if !self.track_input_available(input) {
                missing.push(format!("{name} ({})", input.label()));
                continue;
            }
            // 置き場が使えなければ `reserve_wav_path` が理由を status に出している。
            let Some((file, source_path)) = self.reserve_wav_path(MediaPool::Samples, "録音", &name, "track", "_rec")
            else {
                continue;
            };
            tracks.push(PendingTakeTrack { track_id, file: file.clone(), source_path, channels: input.file_channels() });
            takes.push(AudioTakeSpec { track_id, input, path: file });
        }
        if !missing.is_empty() {
            self.ui_ephemeral.status_message = format!("入力がデバイスに無いため録音しません: {}", missing.join(" / "));
        }
        if takes.is_empty() {
            return;
        }
        let rec = &mut self.cur.recording;
        let take_id = rec.next_audio_take_id;
        rec.next_audio_take_id = take_id.wrapping_add(1);
        rec.audio_rec_region = Some(LiveTakeRegion {
            take_id,
            track_ids: tracks.iter().map(|t| t.track_id).collect(),
            span: None,
            wave: LiveTakeWave::default(),
        });
        rec.audio_takes.push(PendingAudioTake { take_id, tracks });
        self.send_audio(AudioCommand::StartAudioTakes { project: self.pk(), request: AudioTakeRequest { take_id, takes } });
    }

    /// Tick ごと: 録音中の帯をプレイヘッドまで伸ばす。録り終えた帯は、クリップが置かれるまでそのまま残す
    /// (止めた瞬間に帯が消えてからクリップが出る、にしない)。書き出しの間預かっていた結果は、終わったらクリップにする。
    pub(crate) fn audio_rec_tick(&mut self, playhead_beat: f64) {
        if !self.cur.recording.deferred_audio_takes.is_empty() && !self.offline_render_busy() {
            for result in std::mem::take(&mut self.cur.recording.deferred_audio_takes) {
                self.handle_audio_takes_finished(result);
            }
        }
        let live = self.cur.recording.live;
        let Some(region) = self.cur.recording.audio_rec_region.as_mut() else { return };
        if !live {
            return;
        }
        region.span = Some(match region.span {
            // 進んだ。
            Some((start, end)) if playhead_beat >= end => (start, playhead_beat),
            // 戻った (ループの折り返し / seek) か、録り始め。
            _ => (playhead_beat, playhead_beat),
        });
    }

    /// `AudioEvent::AudioTakeProgress`: 録音中の波形の山と区間が増えた。帯に積む (もう別の録音に替わっていたら捨てる)。
    pub(crate) fn handle_audio_take_progress(&mut self, progress: AudioTakeProgress) {
        let region = self.cur.recording.audio_rec_region.as_mut().filter(|r| r.take_id == progress.take_id);
        let Some(wave) = region.map(|r| &mut r.wave) else { return };
        wave.sample_rate = progress.sample_rate;
        if let Some(seg) = progress.new_segments.last() {
            wave.segment = Some(*seg);
        }
        for t in progress.tracks {
            match wave.peaks.iter_mut().find(|(id, _)| *id == t.track_id) {
                Some((_, peaks)) => peaks.extend(t.peaks),
                None => wave.peaks.push((t.track_id, t.peaks)),
            }
        }
    }

    /// `AudioEvent::AudioTakesFinished`: engine が録音を書き終えた。WAV を素材にして、録った区間にクリップを置く
    /// (録った全トラックぶんで 1 undo step)。
    pub(crate) fn handle_audio_takes_finished(&mut self, result: AudioTakesFinished) {
        // 書き出し / 焼き込みの間は曲を編集できない (`edit_song` が拒否する)。捨てずに終わるまで預かる。
        if self.offline_render_busy() {
            self.cur.recording.deferred_audio_takes.push(result);
            return;
        }
        let rec = &mut self.cur.recording;
        let Some(i) = rec.audio_takes.iter().position(|t| t.take_id == result.take_id) else {
            tracing::warn!(take = result.take_id, "AudioTakesFinished with no pending take; ignoring");
            return;
        };
        let pending = rec.audio_takes.remove(i);
        if rec.audio_rec_region.as_ref().is_some_and(|r| r.take_id == result.take_id) {
            rec.audio_rec_region = None;
        }

        let song = self.cur.song_doc.song();
        let mut wavs: Vec<(u32, RecordedWav, String, PathBuf)> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        let mut dropped_frames = 0_u64;
        for take in &result.takes {
            let Some(p) = pending.tracks.iter().find(|p| p.track_id == take.track_id && p.file == take.path) else {
                continue;
            };
            if let Some(e) = &take.error {
                errors.push(e.clone());
            }
            // 何も録れなかった (count-in 中の取り消し / 書けなかった) ファイルは残さない。
            if take.frames == 0 || take.error.is_some() {
                let _ = std::fs::remove_file(&p.file);
                continue;
            }
            dropped_frames += take.dropped_frames;
            let name = song.track_by_id(p.track_id).map_or_else(String::new, |t| format!("{} 録音", t.name));
            let wav = RecordedWav {
                path: p.source_path.clone(),
                sample_rate: result.sample_rate,
                channels: p.channels,
                frames: take.frames,
            };
            wavs.push((p.track_id, wav, name, p.file.clone()));
        }

        // 録音の step は MIDI の録音 (`GestureOwner::RecordingTake`) とは別 — engine の書き終わりは録音を閉じた
        // 後に届く。進行中のドラッグの bracket にも入れない。
        let gesture = self.cur.song_doc.enter_own_gesture("オーディオを録音");
        let segments = result.segments;
        let placed = self
            .edit_song(move |song| {
                wavs.into_iter()
                    .map(|(track_id, wav, name, file)| {
                        let placed = song.place_recorded_take(track_id, wav, &segments, &name).is_some();
                        (file, placed)
                    })
                    .collect::<Vec<(PathBuf, bool)>>()
            })
            .unwrap_or_default();
        self.cur.song_doc.leave_own_gesture(gesture);
        let mut count = 0;
        for (file, ok) in &placed {
            if *ok {
                count += 1;
            } else {
                // 録音中にトラックが消えた: 置き先が無いので素材にしない。
                let _ = std::fs::remove_file(file);
            }
        }
        if count > 0 {
            // 波形は裏で読む (長い録音で GUI を止めない)。
            self.begin_asset_decode("録音を読み込み中");
        }

        if let Some(e) = errors.first() {
            self.ui_ephemeral.status_message = format!("録音の書き込みに失敗しました: {e}");
        } else if dropped_frames > 0 {
            let ms = dropped_frames * 1000 / u64::from(result.sample_rate.max(1));
            self.ui_ephemeral.status_message =
                format!("録音の一部を取りこぼしました (約 {ms} ms 無音になっています。ディスクへの書き込みが間に合っていません)");
        } else if result.segments_dropped > 0 {
            self.ui_ephemeral.status_message =
                "録音中の折り返しが多すぎて、後ろの周がクリップに分かれていません".into();
        }
    }

    /// daw_audio が居なくなった (crash / デバイスの切り替えでの起こし直し): 結果待ちの録音はもう返ってこない。
    /// 書きかけの WAV は消さない (1 秒ごとに flush しているので、そこまでは音が入っている)。
    pub(crate) fn abort_pending_audio_takes(&mut self) {
        let rec = &mut self.cur.recording;
        rec.audio_rec_region = None;
        // 結果がもう届いている録音 (書き出しの間預かっている) は失われていない。
        let deferred = &rec.deferred_audio_takes;
        let mut lost: Vec<PathBuf> = Vec::new();
        rec.audio_takes.retain(|t| {
            if deferred.iter().any(|r| r.take_id == t.take_id) {
                return true;
            }
            lost.extend(t.tracks.iter().map(|p| p.file.clone()));
            false
        });
        let Some(first) = lost.first() else { return };
        tracing::warn!(files = ?lost, "audio takes lost: daw_audio disconnected before finishing them");
        self.ui_ephemeral.status_message = format!(
            "音声エンジンが止まったため、録音中のオーディオはクリップになりませんでした (書きかけのファイル: {})",
            first.display()
        );
    }

    /// `track_ids` の入力を `input` に揃える (1 undo step、変化が無ければ何もしない)。
    fn set_tracks_input(&mut self, track_ids: &[u32], input: TrackInput) {
        self.edit_song_checked(|song| {
            let mut changed = false;
            for t in song.tracks.iter_mut().filter(|t| track_ids.contains(&t.id)) {
                changed |= t.input != input;
                t.input = input;
            }
            changed
        });
    }

    /// `track_ids` のモニターを `monitor` に揃える (1 undo step、変化が無ければ何もしない)。
    fn set_tracks_monitor(&mut self, track_ids: &[u32], monitor: MonitorMode) {
        self.edit_song_checked(|song| {
            let mut changed = false;
            for t in song.tracks.iter_mut().filter(|t| track_ids.contains(&t.id)) {
                changed |= t.monitor != monitor;
                t.monitor = monitor;
            }
            changed
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{
        AudioContent, Clip, ClipContent, Device, ImageContent, MidiContent, NativeDevice, NativeKind, PluginInstance, Song,
        TextContent, Track,
    };
    use common::plugin_format::PluginFormat;
    use common::port_config::PortConfig;

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

    /// r.md #158 仕様 14: 入力の帯に出す入力は、トラックの中身から決まる。
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
        let both = RecordableInputs { midi: true, audio: true };
        let midi_only = RecordableInputs { midi: true, audio: false };
        let audio_only = RecordableInputs { midi: false, audio: true };
        let none = RecordableInputs::default();
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
        ] {
            let track = song.track_by_id(id).expect("track");
            assert_eq!(recordable_inputs(&song, track), want, "{what}");
        }
        let master = Track { id: MASTER_TRACK_ID, ..Track::default() };
        assert_eq!(recordable_inputs(&song, &master), none, "マスター");
    }
}
