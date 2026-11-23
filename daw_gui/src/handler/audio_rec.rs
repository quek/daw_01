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
pub use common::model::RecordableInputs;
use common::model::{AudioSourcePath, InputRules, MonitorMode, RecordedWav, TrackInput};
use common::protocol::AudioCommand;

use crate::event_audio_rec::AudioRecEvent;
use crate::media_dest::MediaPool;
use crate::state::*;

/// 「入力元」で、MIDI を録れないトラックの既定 (MIDI) の表示 ([`AppData::audio_input_choices`])。
pub const INPUT_UNSET: &str = "未選択";
/// 「入力元」に今の入力が無いときの注記: デバイスに無い入力。
pub const INPUT_MISSING: &str = "無い入力";
/// 「入力元」に今の入力が無いときの注記: そのトラックで録れない入力 (`common::model::InputRules`)。
pub const INPUT_NOT_RECORDABLE: &str = "録れない";

/// インスペクタの「入力」の選択肢 1 つ。
#[derive(Debug, Clone, PartialEq)]
pub struct InputChoice {
    pub input: TrackInput,
    pub label: String,
}

/// インスペクタの「入力」の帯の対象 1 本 ([`AppData::input_targets`])。
#[derive(Debug, Clone, Copy)]
pub struct InputTarget<'s> {
    pub track: &'s common::model::Track,
    /// このトラックで録れる入力の種類 (規則は `common::model::InputRules`)。
    pub kinds: RecordableInputs,
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

    /// インスペクタの「入力 / モニター」が編集するトラック (選んだ順) と、それぞれで録れる入力の種類:
    /// 複数選択ならその全部、そうでなければ cursor track。録れる入力が 1 つも無いトラック (master /
    /// グループのバス / リターン / 画像・映像のトラック、`common::model::InputRules`) は含めない。
    ///
    /// 帯を出すか・帯の対象・選択肢・書き込み先の絞り込みが、1 フレームに 1 回だけ呼んだこの結果を共有する
    /// (規則の表を作るのも、選択中のトラックを引くのも 1 回)。
    #[must_use]
    pub fn input_targets(&self) -> Vec<InputTarget<'_>> {
        let song = self.cur.song_doc.song();
        let selected = &self.cur.selection.selected_track_ids;
        let cursor = self.cursor_track_id();
        let ids: &[u32] = if selected.len() > 1 { selected } else { cursor.as_slice() };
        if ids.is_empty() {
            return Vec::new();
        }
        let rules = InputRules::new(song);
        // 何本も選んでいれば id → トラックの表を 1 回だけ作る (選んだ順は保つ)。
        let by_id: std::collections::HashMap<u32, &common::model::Track> =
            if ids.len() > 1 { song.tracks.iter().map(|t| (t.id, t)).collect() } else { Default::default() };
        ids.iter()
            .filter_map(|id| if ids.len() > 1 { by_id.get(id).copied() } else { song.track_by_id(*id) })
            .map(|track| InputTarget { track, kinds: rules.recordable(track) })
            .filter(|t| t.kinds.any())
            .collect()
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
    ///
    /// ただし MIDI を録れないトラックの MIDI は「未選択」と出す: `TrackInput` の既定は MIDI なので、音声だけの
    /// トラック (取り込んだ音声 / 効果だけのトラック) は誰も選んでいないのに MIDI を持っている。それを
    /// 「録れない」と出すと、何も触っていない普通の状態が警告に見える。選べば書けるのは録れるトラックだけ
    /// (インスペクタの書き込み側が絞る) なので、この項目を選んでも何も変わらない。
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
                let label = if input.is_midi() && !kinds.midi {
                    INPUT_UNSET.to_string()
                } else {
                    let why = if kinds.allows(input) { INPUT_MISSING } else { INPUT_NOT_RECORDABLE };
                    format!("{} ({why})", input.label())
                };
                out.push(InputChoice { input, label });
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
    /// 録るのは、音声入力がそのトラックで **効く** ものだけ (`common::model::InputRules::effective_input`。インスペクタの
    /// 入力の帯・engine のモニターと同じ規則): 画像を置いた / グループのバスやリターンになったトラックは、音声入力を
    /// 保存したまま待機していても録らない (無音のクリップを作らない)。外したトラックは status に名前を出す。
    fn request_audio_takes(&mut self) {
        let song = self.cur.song_doc.song();
        let rules = InputRules::new(song);
        let mut wanted: Vec<(u32, String, TrackInput)> = Vec::new();
        let mut not_recordable = Vec::new();
        for t in song.tracks.iter().filter(|t| t.armed && !t.input.is_midi() && song.track_effectively_enabled(t.id)) {
            if rules.effective_input(t).is_some() {
                wanted.push((t.id, t.name.clone(), t.input));
            } else {
                not_recordable.push(song.track_display_name(t.id).into_owned());
            }
        }
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
        let mut skipped = Vec::new();
        if !missing.is_empty() {
            skipped.push(format!("入力がデバイスに無いため録音しません: {}", missing.join(" / ")));
        }
        if !not_recordable.is_empty() {
            skipped.push(format!("このトラックでは音声を録れないため録音しません: {}", not_recordable.join(" / ")));
        }
        if !skipped.is_empty() {
            self.ui_ephemeral.status_message = skipped.join(" ・ ");
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

