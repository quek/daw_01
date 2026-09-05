//! Global Sampler の engine 側 (`docs/plan_global_sampler.md` §3.2)。
//!
//! - [`SamplerRig`]: recv loop が `OpenSamplerRing` で open したリングと録音源。
//!   `WorkerRig` と同じく `RtBundle` の snapshot field で RT へ届き、旧世代は
//!   recycle bundle で off-thread drop される。
//! - [`SamplerRt`]: RT 私有の走行状態 (セグメント判定 / 試聴カーソル /
//!   MIDI 試聴シーケンス)。事前確保のみ。
//!
//! 毎 buffer の仕事は `process_buffer` の scope 書き込みと同じ位置で
//! [`SamplerRt::write_block`] → [`SamplerRt::mix_preview`] の順に呼ぶ。
//! 試聴音はリングへ書いた **後** に master へ足すので再録されない。

use std::sync::Arc;

use common::model::{Song, TapPoint};
use common::protocol::{PreviewNote, SamplerSource};
use common::sampler_ring::{SEGMENT_RESYNC_FRAMES, SamplerRingHandle, SegmentInfo};

use crate::mixer::TrackScratch;
use crate::sequencer::NoteTransition;

/// recv loop が組み、bundle で RT へ渡す 1 世代ぶんのリング。
pub struct SamplerRig {
    pub ring: Arc<SamplerRingHandle>,
    pub source: SamplerSource,
}

/// 試聴のフェード長 (frames)。頭と尻のクリックを消す。
const PREVIEW_FADE_FRAMES: u64 = 240;

/// MIDI 試聴で同時に鳴らせるノート数の上限 (事前確保)。
const PREVIEW_SEQ_ACTIVE_CAP: usize = 256;

/// MIDI 試聴シーケンス。recv loop が `engine_shared.preview_sequence` のミラーへ載せ、
/// `RtBundle` の snapshot field で RT へ届く (旧 Arc は recycle で off-thread drop —
/// RT が `ArcSwap` を load すると最終 drop が RT で起きうるので使わない)。
pub struct PreviewSequence {
    /// 鳴らす track の **安定 id**。index は buffer ごとに現 song snapshot から
    /// 解く (試聴中の並べ替え / 削除で別 track に刺さらない)。
    pub track_id: u32,
    /// `offset_frames` 昇順。
    pub notes: Vec<PreviewNote>,
    /// 差し替え検出用 (recv loop が単調に振る)。
    pub generation: u64,
}

/// [`SamplerRt::write_block`] に渡す 1 buffer の transport 事実。
#[derive(Clone, Copy)]
pub struct BlockTransport {
    pub playing: bool,
    /// buffer 頭の曲位置 (samples)。seek / loop wrap の検出に使う。
    pub playhead: u64,
    /// buffer 頭の曲位置 (拍、刻みが解いた `playhead_beats`)。セグメントに記録する。
    pub playhead_beat: f64,
    pub bpm: f32,
}

/// RT 私有の走行状態。
pub struct SamplerRt {
    /// 直前 buffer の `playing` (`None` = リングを開いてから 1 度も書いていない)。
    last_playing: Option<bool>,
    /// 直前 buffer 末尾の予測 playhead (= 頭 + frames)。seek / loop wrap 検出。
    expected_playhead: u64,
    /// 直前にセグメントを押したリングフレーム。
    last_segment_frame: u64,
    /// リングの試聴 `(start, end, cursor)`。
    preview: Option<(u64, u64, u64)>,
    /// 直前 buffer で snapshot を強制した track index (次 buffer で下ろす)。
    forced_track: Option<usize>,
    /// MIDI 試聴の進行 `(generation, 開始時の frames_rendered, 次に撃つ index)`。
    seq_cursor: Option<(u64, u64, usize)>,
    /// 最後まで鳴らし終えた generation。同じ generation が bundle に残っていても
    /// 先頭からやり直さない (GUI が Stop を送るまでの間の無限ループ防止)。
    seq_done: Option<u64>,
    /// MIDI 試聴で鳴っているノート `(off の絶対 frame, track id, pitch)`。
    seq_active: Vec<(u64, u32, u8)>,
}

impl Default for SamplerRt {
    fn default() -> Self {
        Self::new()
    }
}

impl SamplerRt {
    pub fn new() -> Self {
        Self {
            last_playing: None,
            expected_playhead: u64::MAX,
            last_segment_frame: 0,
            preview: None,
            forced_track: None,
            seq_cursor: None,
            seq_done: None,
            seq_active: Vec::with_capacity(PREVIEW_SEQ_ACTIVE_CAP),
        }
    }

    /// リングが差し替わった (別世代) ので走行状態を捨てる。
    pub fn reset(&mut self) {
        self.last_playing = None;
        self.expected_playhead = u64::MAX;
        self.last_segment_frame = 0;
        self.preview = None;
    }

    pub fn set_preview(&mut self, start: u64, end: u64) {
        self.preview = (start < end).then_some((start, end, start));
    }

    pub fn stop_preview(&mut self) {
        self.preview = None;
    }

    /// 録音源の track に pre-fx / pre-fader snapshot を要求する flag を立てる
    /// (`Song` に無い tap なので `any_tap_at` では拾えない)。render の前に呼ぶ。
    pub fn arm_snapshot_flags(
        &mut self,
        rig: Option<&SamplerRig>,
        song: Option<&Song>,
        scratch: &mut [TrackScratch],
    ) {
        if let Some(i) = self.forced_track.take()
            && let Some(s) = scratch.get_mut(i)
        {
            s.force_prefx_snapshot = false;
            s.force_prefader_snapshot = false;
        }
        let Some(rig) = rig else { return };
        let SamplerSource::Track(tap) = rig.source else { return };
        let Some(idx) = song.and_then(|s| track_index(s, tap.source_track)) else { return };
        let Some(s) = scratch.get_mut(idx) else { return };
        match tap.tap_point {
            TapPoint::PreFx => s.force_prefx_snapshot = true,
            TapPoint::PostFx => s.force_prefader_snapshot = true,
            TapPoint::PostFader => {}
        }
        self.forced_track = Some(idx);
    }

    /// 録音源の 1 buffer をリングへ書き、必要ならセグメントを押す。
    #[allow(clippy::too_many_arguments)]
    pub fn write_block(
        &mut self,
        rig: &SamplerRig,
        song: Option<&Song>,
        scratch: &[TrackScratch],
        master_l: &[f32],
        master_r: &[f32],
        n: usize,
        transport: BlockTransport,
    ) {
        // 一時停止中もセグメント (transport の事実) は記録する — GUI の MIDI Capture は
        // wall-clock ↔ 拍の対応をここから引くので、止めると古い「再生中」から外挿し続ける。
        // 書かないのは音声だけ (ring_frame は進まない)。
        let paused = rig.ring.paused();
        let ring_frame = rig.ring.write_frames();
        let state_changed = self.last_playing != Some(transport.playing);
        let jumped = transport.playing && transport.playhead != self.expected_playhead;
        let resync = ring_frame.saturating_sub(self.last_segment_frame) >= SEGMENT_RESYNC_FRAMES;
        if state_changed || jumped || resync {
            rig.ring.push_segment(SegmentInfo {
                ring_frame,
                wall_ns: wall_clock_ns(),
                playhead_beat: transport.playing.then_some(transport.playhead_beat),
                bpm: transport.bpm,
            });
            self.last_segment_frame = ring_frame;
        }
        self.last_playing = Some(transport.playing);
        self.expected_playhead = transport.playhead.saturating_add(n as u64);
        if paused {
            return;
        }

        match rig.source {
            SamplerSource::Master => rig.ring.write_block(&master_l[..n], &master_r[..n]),
            SamplerSource::Track(tap) => {
                let bufs = song
                    .and_then(|s| track_index(s, tap.source_track))
                    .and_then(|i| scratch.get(i))
                    .map(|s| match tap.tap_point {
                        TapPoint::PostFader => (&s.track_l[..n], &s.track_r[..n]),
                        TapPoint::PostFx => (&s.pre_fader_l[..n], &s.pre_fader_r[..n]),
                        TapPoint::PreFx => (&s.pre_fx_l[..n], &s.pre_fx_r[..n]),
                    });
                match bufs {
                    Some((l, r)) => rig.ring.write_block(l, r),
                    // track が消えた: 時間軸を保つため無音を書く (silence は
                    // master バッファを流用せず定数で)。
                    None => write_silence(&rig.ring, n),
                }
            }
        }
    }

    /// リングの試聴を master に加算する。範囲を読み終える / 押し出されたら止まる。
    pub fn mix_preview(&mut self, rig: &SamplerRig, master_l: &mut [f32], master_r: &mut [f32], n: usize) {
        let Some((start, end, cursor)) = self.preview else { return };
        if cursor >= end || start < rig.ring.oldest_frame() {
            self.preview = None;
            return;
        }
        let total = end - start;
        let mut c = cursor;
        for i in 0..n {
            if c >= end {
                break;
            }
            let (l, r) = rig.ring.read_frame(c);
            let g = fade_gain(c - start, total);
            master_l[i] += l * g;
            master_r[i] += r * g;
            c += 1;
        }
        self.preview = (c < end).then_some((start, end, c));
    }

    /// MIDI 試聴シーケンスを 1 buffer 進める。`seq` は bundle で届いた最新
    /// (`None` = 停止要求)。`frames_rendered` は buffer 頭の累積フレーム。
    pub fn step_preview_sequence(
        &mut self,
        seq: Option<&PreviewSequence>,
        song: Option<&Song>,
        scratch: &mut [TrackScratch],
        frames_rendered: u64,
        n: usize,
    ) {
        let Some(seq) = seq else {
            if self.seq_cursor.is_some() || !self.seq_active.is_empty() {
                self.release_all(song, scratch);
                self.seq_cursor = None;
            }
            self.seq_done = None;
            return;
        };
        if self.seq_done == Some(seq.generation) {
            return;
        }
        let (generation, started, mut next) = match self.seq_cursor {
            Some(c) if c.0 == seq.generation => c,
            _ => {
                // 新しいシーケンス: 鳴っている前のノートは消してから始める。
                self.release_all(song, scratch);
                (seq.generation, frames_rendered, 0)
            }
        };
        let block_end = frames_rendered + n as u64;
        // まず期限の来た off を出す (同 frame の on より前に)。
        self.release_due(song, scratch, block_end);
        // track は毎 buffer 安定 id から解く。消えていれば鳴らさず終える。
        let Some(track_idx) = song.and_then(|s| track_index(s, seq.track_id)) else {
            self.release_all(song, scratch);
            self.seq_cursor = None;
            self.seq_done = Some(seq.generation);
            return;
        };
        while let Some(note) = seq.notes.get(next) {
            let at = started + note.offset_frames;
            if at >= block_end {
                break;
            }
            next += 1;
            if self.seq_active.len() >= self.seq_active.capacity() {
                continue;
            }
            let Some(s) = scratch.get_mut(track_idx) else { continue };
            let pp = &mut s.state.pending_preview;
            if pp.len() < pp.capacity() {
                pp.push(NoteTransition::On {
                    note_id: PREVIEW_SEQ_NOTE_ID,
                    key: note.pitch,
                    velocity: f64::from(note.velocity) / 127.0,
                });
                self.seq_active
                    .push((at + note.duration_frames.max(1), seq.track_id, note.pitch));
            }
        }
        let finished = next >= seq.notes.len() && self.seq_active.is_empty();
        if finished {
            self.seq_cursor = None;
            self.seq_done = Some(generation);
        } else {
            self.seq_cursor = Some((generation, started, next));
        }
    }

    fn release_due(&mut self, song: Option<&Song>, scratch: &mut [TrackScratch], block_end: u64) {
        let mut i = 0;
        while i < self.seq_active.len() {
            let (off_at, track_id, pitch) = self.seq_active[i];
            if off_at < block_end {
                push_off(song, scratch, track_id, pitch);
                self.seq_active.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }

    fn release_all(&mut self, song: Option<&Song>, scratch: &mut [TrackScratch]) {
        for &(_, track_id, pitch) in &self.seq_active {
            push_off(song, scratch, track_id, pitch);
        }
        self.seq_active.clear();
    }
}

/// recv loop (IPC スレッド) の sampler 系コマンド処理。RT には触らない。
///
/// - `OpenSamplerRing`: daw_gui が create したリングを open し、`engine_shared.sampler`
///   のミラーへ載せて `republish` (= bundle の snapshot field で RT へ届く、worker rig
///   と同じ経路)。open 失敗は warn して従来のリングを据え置く。
/// - `PreviewSequence` / `PreviewSequenceStop`: 同じくミラー + `republish`。track は
///   安定 id のまま運び、RT が buffer ごとに解く。
pub fn handle_command(
    cmd: common::protocol::AudioCommand,
    engine_shared: &crate::engine::EngineShared,
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<crate::engine::EngineCommand>,
    seq_generation: &mut u64,
    republish: &mut dyn FnMut(),
) {
    use common::protocol::AudioCommand as C;
    use crate::engine::EngineCommand as E;
    match cmd {
        C::OpenSamplerRing { shmem_id, source } => match SamplerRingHandle::open(&shmem_id) {
            Ok(ring) => {
                tracing::info!(%shmem_id, ?source, capacity = ring.capacity(), "sampler ring opened");
                engine_shared
                    .sampler
                    .store(Some(Arc::new(SamplerRig { ring: Arc::new(ring), source })));
                republish();
            }
            Err(e) => tracing::warn!(error = ?e, %shmem_id, "failed to open sampler ring"),
        },
        C::SamplerPreview { start_frame, end_frame } => {
            let _ = cmd_tx.send(E::SamplerPreview { start: start_frame, end: end_frame });
        }
        C::SamplerPreviewStop => {
            let _ = cmd_tx.send(E::SamplerPreviewStop);
        }
        C::PreviewSequence { track_id, notes } => {
            *seq_generation += 1;
            engine_shared.preview_sequence.store(Some(Arc::new(PreviewSequence {
                track_id,
                notes,
                generation: *seq_generation,
            })));
            republish();
        }
        C::PreviewSequenceStop => {
            engine_shared.preview_sequence.store(None);
            republish();
        }
        _ => {}
    }
}

/// MIDI 試聴ノートの `note_id`。鍵盤プレビュー (`u32::MAX`) と sequencer の
/// 採番域のどちらとも衝突しない sentinel。
const PREVIEW_SEQ_NOTE_ID: u32 = u32::MAX - 1;

fn push_off(song: Option<&Song>, scratch: &mut [TrackScratch], track_id: u32, pitch: u8) {
    let Some(idx) = song.and_then(|s| track_index(s, track_id)) else { return };
    if let Some(s) = scratch.get_mut(idx) {
        let pp = &mut s.state.pending_preview;
        if pp.len() < pp.capacity() {
            pp.push(NoteTransition::Off {
                note_id: PREVIEW_SEQ_NOTE_ID,
                key: pitch,
            });
        }
    }
}

fn track_index(song: &Song, track_id: u32) -> Option<usize> {
    song.tracks
        .iter()
        .position(|t| t.id == track_id)
        .filter(|&i| i < crate::engine::MAX_TRACKS)
}

fn write_silence(ring: &SamplerRingHandle, n: usize) {
    // 事前確保済みの定数バッファ (最大 buffer 長ぶん) から書く。
    static ZERO: [f32; crate::mixer::MAX_FRAMES] = [0.0; crate::mixer::MAX_FRAMES];
    let n = n.min(ZERO.len());
    ring.write_block(&ZERO[..n], &ZERO[..n]);
}

/// 試聴の頭 / 尻の線形フェード。
fn fade_gain(pos: u64, total: u64) -> f32 {
    let fade = PREVIEW_FADE_FRAMES.min(total / 2).max(1);
    let head = pos.min(fade) as f32 / fade as f32;
    let tail = (total - pos).min(fade) as f32 / fade as f32;
    head.min(tail)
}

/// UNIX epoch からの ns (`GetSystemTimePreciseAsFileTime` 由来、user-mode)。
/// MIDI Capture 側 (daw_gui) と同じ時計。
pub fn wall_clock_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rig(seconds: u32) -> SamplerRig {
        let id = format!("daw_01_sampler_engine_test_{}_{}", std::process::id(), wall_clock_ns());
        SamplerRig {
            ring: Arc::new(SamplerRingHandle::create(&id, seconds, 100).unwrap()),
            source: SamplerSource::Master,
        }
    }

    fn transport(playing: bool, playhead: u64) -> BlockTransport {
        BlockTransport { playing, playhead, playhead_beat: playhead as f64, bpm: 120.0 }
    }

    /// `pending_preview` の中身を取り出す (容量は残す = RT の「溢れたら drop」条件を
    /// 壊さない。`mem::take` だと capacity 0 の Vec が残って以後 push されない)。
    fn take_events(s: &mut TrackScratch) -> Vec<NoteTransition> {
        let v = s.state.pending_preview.clone();
        s.state.pending_preview.clear();
        v
    }

    #[test]
    fn segments_are_pushed_on_play_stop_and_jump_only() {
        let r = rig(10);
        let mut rt = SamplerRt::new();
        let l = [0.0f32; 10];
        let scratch: Vec<TrackScratch> = Vec::new();
        // 最初の buffer (停止) → 1 件
        rt.write_block(&r, None, &scratch, &l, &l, 10, transport(false, 0));
        // 停止のまま → 増えない
        rt.write_block(&r, None, &scratch, &l, &l, 10, transport(false, 0));
        // 再生開始 → 1 件
        rt.write_block(&r, None, &scratch, &l, &l, 10, transport(true, 100));
        // 連続再生 → 増えない
        rt.write_block(&r, None, &scratch, &l, &l, 10, transport(true, 110));
        // seek → 1 件
        rt.write_block(&r, None, &scratch, &l, &l, 10, transport(true, 500));
        // 停止 → 1 件
        rt.write_block(&r, None, &scratch, &l, &l, 10, transport(false, 510));
        let mut segs = Vec::new();
        r.ring.segments(&mut segs);
        let got: Vec<(u64, Option<f64>)> =
            segs.iter().map(|s| (s.ring_frame, s.playhead_beat)).collect();
        assert_eq!(got, vec![(0, None), (20, Some(100.0)), (40, Some(500.0)), (50, None)]);
        assert_eq!(r.ring.write_frames(), 60);
    }

    #[test]
    fn paused_ring_does_not_advance() {
        let r = rig(10);
        let mut rt = SamplerRt::new();
        let l = [0.5f32; 10];
        let scratch: Vec<TrackScratch> = Vec::new();
        r.ring.set_paused(true);
        rt.write_block(&r, None, &scratch, &l, &l, 10, transport(false, 0));
        assert_eq!(r.ring.write_frames(), 0);
        r.ring.set_paused(false);
        rt.write_block(&r, None, &scratch, &l, &l, 10, transport(false, 0));
        assert_eq!(r.ring.write_frames(), 10);
    }

    #[test]
    fn preview_mixes_ring_range_with_fades_and_stops_at_end() {
        let r = rig(10);
        let mut rt = SamplerRt::new();
        let one = [1.0f32; 100];
        let scratch: Vec<TrackScratch> = Vec::new();
        rt.write_block(&r, None, &scratch, &one, &one, 100, transport(false, 0));
        rt.set_preview(10, 30); // 20 frames、フェードは total/2 = 10
        let mut ml = [0.0f32; 16];
        let mut mr = [0.0f32; 16];
        rt.mix_preview(&r, &mut ml, &mut mr, 16);
        assert_eq!(ml[0], 0.0); // 頭はフェードイン 0
        assert!((ml[10] - 1.0).abs() < 1e-6); // 中央はフル
        let mut ml2 = [0.0f32; 16];
        rt.mix_preview(&r, &mut ml2, &mut mr, 16);
        assert!(ml2[3] > 0.0 && ml2[4] == 0.0, "残り 4 frame で終わる: {ml2:?}");
        assert!(rt.preview.is_none());
    }

    /// track id 1 だけを持つ song (試聴の id → index 解決用)。
    /// `Track` は private field を持つので default + mutate で組む (engine.rs と同じ)。
    #[allow(clippy::field_reassign_with_default)]
    fn song() -> Song {
        let mut s = Song::default();
        let mut t = common::model::Track::default();
        t.id = 1;
        s.tracks.push(t);
        s
    }

    #[test]
    fn preview_sequence_emits_on_then_off_at_offsets() {
        let song = song();
        let mut rt = SamplerRt::new();
        let mut scratch = vec![TrackScratch::new()];
        let seq = PreviewSequence {
            track_id: 1,
            notes: vec![
                PreviewNote { offset_frames: 5, duration_frames: 20, pitch: 60, velocity: 100 },
                PreviewNote { offset_frames: 40, duration_frames: 5, pitch: 62, velocity: 64 },
            ],
            generation: 1,
        };
        // buffer 0: [0, 32) → note 60 on
        rt.step_preview_sequence(Some(&seq), Some(&song), &mut scratch, 0, 32);
        let ev = take_events(&mut scratch[0]);
        assert!(matches!(ev.as_slice(), [NoteTransition::On { key: 60, .. }]));
        // buffer 1: [32, 64) → 60 off (期限 25) と 62 on
        rt.step_preview_sequence(Some(&seq), Some(&song), &mut scratch, 32, 32);
        let ev = take_events(&mut scratch[0]);
        assert!(matches!(
            ev.as_slice(),
            [NoteTransition::Off { key: 60, .. }, NoteTransition::On { key: 62, .. }]
        ));
        // buffer 2: 62 off (期限 45) → 完了
        rt.step_preview_sequence(Some(&seq), Some(&song), &mut scratch, 64, 32);
        let ev = take_events(&mut scratch[0]);
        assert!(matches!(ev.as_slice(), [NoteTransition::Off { key: 62, .. }]));
        assert!(rt.seq_cursor.is_none());
        // 同じ generation が bundle に残っていても先頭からやり直さない (無限ループ防止)
        rt.step_preview_sequence(Some(&seq), Some(&song), &mut scratch, 96, 32);
        assert!(scratch[0].state.pending_preview.is_empty());
        // 停止要求で鳴っているものが無ければ何も出ない
        rt.step_preview_sequence(None, Some(&song), &mut scratch, 128, 32);
        assert!(scratch[0].state.pending_preview.is_empty());
    }

    #[test]
    fn preview_sequence_stop_releases_active_notes() {
        let song = song();
        let mut rt = SamplerRt::new();
        let mut scratch = vec![TrackScratch::new()];
        let seq = PreviewSequence {
            track_id: 1,
            notes: vec![PreviewNote { offset_frames: 0, duration_frames: 1000, pitch: 60, velocity: 100 }],
            generation: 7,
        };
        rt.step_preview_sequence(Some(&seq), Some(&song), &mut scratch, 0, 32);
        scratch[0].state.pending_preview.clear();
        rt.step_preview_sequence(None, Some(&song), &mut scratch, 32, 32);
        let ev = take_events(&mut scratch[0]);
        assert!(matches!(ev.as_slice(), [NoteTransition::Off { key: 60, .. }]));
    }
}
