//! handler::chord_audio — オーディオ音源からのコード検出の発注と回収 (r.md #157 §3.4)。
//!
//! - 発注: [`AppData::detect_audio_clips`] (クリップ → 帯のドロップ / 「コードを抽出…」) と
//!   [`AppData::auto_detect_followed_audio`] (追従を ON にしたオーディオトラックで、未検出の音源)。
//!   どちらも同じ条件 ([`detectable`]) の event の音源を、同じ口 (`order_detection`) で積む。
//! - 検出: `crate::chord_audio_detect::detect_audio_chords` を音源ごとに 1 本の thread で (重い FFT)。
//!   同時に走らせるのは [`MAX_RUNNING_DETECTIONS`] 本まで、残りは順番待ち。
//! - 回収: `on_tick` → [`AppData::poll_chord_detection`] が終わった結果を
//!   `ChordToolEvent::SetAudioChords` で Song へ入れる。検出結果 (`Song.audio_chords`) は音源から決まる
//!   派生データなので undo に積まない (`AppData::write_derived_song`)。帯に置く箱だけが undo step になる。
//!   コードが 1 つも無かった音源 (ドラム / 単音) は `ChordToolsState::detected_without_chords` に覚え、
//!   編集のたびに測り直さない。
//!
//! 窓の格子はクリップの置き方から作る: 曲の拍 `window_beats` 刻みの境界を音源のフレームへ写す
//! ([`audio_detect_grid`])。音源は複数のクリップから使われうるが、保存は音源ごと 1 つ
//! (`Song.audio_chords[source_id]`) なので、発注したクリップの置き方で 1 回測る。

use std::collections::HashSet;
use std::sync::Arc;

use common::chord_detect::DetectOptions;
use common::model::{AudioChordSpan, AudioEvent, AudioSourceId, ChordEvent, Clip, Song};

use crate::audio_source_cache::{AudioSourceBuffer, AudioSourceCache};
use crate::chord_audio_detect::{DetectGrid, detect_audio_chords};
use crate::event::AppEvent;
use crate::event_chord::ChordToolEvent;
use crate::state::AppData;
use crate::state::chord_tools::{AudioDetectJob, DetectRun};

/// 同時に走らせる検出の数 (1 本が音源全体に FFT を掛ける。GUI と再生の CPU を残す)。
const MAX_RUNNING_DETECTIONS: usize = 2;

/// コードを検出する event か (発注と「未検出か」の判定が共有する唯一の条件)。ミュートは鳴らないので測らず、
/// 逆再生は音源のフレームと曲の拍の向きが逆になるので拍の格子を写せない。
fn detectable(ev: &AudioEvent) -> bool {
    !ev.muted && !ev.reversed
}

/// 検出 1 本の発注 (音源と、測る格子)。
struct DetectRequest {
    source_id: AudioSourceId,
    buf: Arc<AudioSourceBuffer>,
    grid: DetectGrid,
}

/// クリップ `clip` の event `ev` の置き方で音源を測る発注。音源が手元に無い (読み込み中 / 読めない) なら `None`。
fn detect_request(song: &Song, cache: &AudioSourceCache, clip: &Clip, ev: &AudioEvent, opts: &DetectOptions) -> Option<DetectRequest> {
    let buf = cache.get(ev.source_id)?;
    let grid = audio_detect_grid(song, clip, ev, buf.sample_rate, opts);
    Some(DetectRequest { source_id: ev.source_id, buf, grid })
}

impl AppData {
    /// オーディオクリップ `clips` (`(track id, clip id)`) の音源のコードを検出する (MIDI クリップは無視)。
    /// `place` なら結果を帯にも置く。同じ音源を検出中なら重ねて走らせない (置き先だけ足す)。
    pub(crate) fn detect_audio_clips(&mut self, clips: &[(u32, u32)], place: bool) {
        let opts = self.cur.chord_tools.view.detect.sanitized();
        let mut requests: Vec<(DetectRequest, Option<(u32, u32)>)> = Vec::new();
        let mut missing = 0usize;
        let song = self.cur.song_doc.song();
        for &(track_id, clip_id) in clips {
            let Some(clip) = song.track_by_id(track_id).and_then(|t| t.clip_by_id(clip_id)) else { continue };
            let Some(events) = song.clip_contents.get(&clip.content_id).and_then(|c| c.audio_events()) else {
                continue;
            };
            let mut seen: HashSet<AudioSourceId> = HashSet::new();
            for ev in events.iter().filter(|e| detectable(e) && seen.insert(e.source_id)) {
                match detect_request(song, &self.cur.media.audio_source_cache, clip, ev, &opts) {
                    Some(r) => requests.push((r, place.then_some((track_id, clip_id)))),
                    None => missing += 1,
                }
            }
        }
        let mut ordered = 0usize;
        for (r, place_on) in requests {
            ordered += usize::from(self.order_detection(r, place_on, opts));
        }
        self.start_queued_detections();
        if ordered > 0 {
            self.ui_ephemeral.status_message = format!("コードを検出中… ({ordered} 音源)");
        } else if missing > 0 {
            self.ui_ephemeral.status_message = if self.audio_decode_pending() {
                "音源の読み込みが終わってからコードを検出できます".into()
            } else {
                "音源を読み込めないのでコードを検出できません".into()
            };
        }
    }

    /// 検出を積む (走らせるのは [`Self::start_queued_detections`])。同じ音源を検出中 / 順番待ちなら重ねない
    /// (置き先だけ足す)。新しく積んだら `true`。
    fn order_detection(&mut self, r: DetectRequest, place_on: Option<(u32, u32)>, opts: DetectOptions) -> bool {
        let jobs = &mut self.cur.chord_tools.detect_jobs;
        if let Some(job) = jobs.iter_mut().find(|j| j.source_id == r.source_id) {
            job.place_on.extend(place_on);
            return false;
        }
        jobs.push(AudioDetectJob {
            source_id: r.source_id,
            place_on: place_on.into_iter().collect(),
            run: DetectRun::Queued { buf: r.buf, grid: r.grid, opts },
        });
        true
    }

    /// 順番待ちの検出を、走っている数が [`MAX_RUNNING_DETECTIONS`] になるまで積んだ順に走らせる。
    /// 書き出し / 解析の間は走らせない (CPU を取らない。終われば `poll_chord_detection` が走らせる)。
    fn start_queued_detections(&mut self) {
        if self.offline_render_busy() {
            return;
        }
        let jobs = &mut self.cur.chord_tools.detect_jobs;
        let mut running = jobs.iter().filter(|j| matches!(j.run, DetectRun::Running(_))).count();
        jobs.retain_mut(|job| {
            if running >= MAX_RUNNING_DETECTIONS {
                return true;
            }
            let DetectRun::Queued { buf, grid, opts } = &job.run else { return true };
            let (buf, grid, opts) = (Arc::clone(buf), *grid, *opts);
            let (tx, rx) = std::sync::mpsc::channel();
            let spawned = std::thread::Builder::new().name("chord-detect".into()).spawn(move || {
                // 受け手が居ない (曲を差し替えて発注を捨てた) なら結果は要らない。
                let _ = tx.send(detect_audio_chords(&buf.samples, buf.sample_rate, grid, &opts));
            });
            match spawned {
                Ok(_) => {
                    job.run = DetectRun::Running(rx);
                    running += 1;
                    true
                }
                Err(e) => {
                    tracing::error!(error = %e, source = job.source_id, "chord detection thread spawn failed");
                    false
                }
            }
        });
    }

    /// 終わった検出を Song に入れ、順番待ちを走らせる (`on_tick` から毎ティック。発注が無ければ何もしない)。
    pub(crate) fn poll_chord_detection(&mut self) {
        if self.cur.chord_tools.detect_jobs.is_empty() {
            return;
        }
        // 書き出し / 解析の間は Song に書けない (`write_derived_song` が拒否する)。終わった結果は channel に
        // 置いたまま待つ (受け取って捨てると、自動検出は済んだ扱いのまま次の編集まで測り直さない)。
        if self.offline_render_busy() {
            return;
        }
        // (音源, 区間, 帯にも置くクリップ)。
        let mut done = Vec::new();
        let mut crashed = Vec::new();
        self.cur.chord_tools.detect_jobs.retain(|job| {
            let DetectRun::Running(rx) = &job.run else { return true };
            match rx.try_recv() {
                Ok(spans) => {
                    done.push((job.source_id, spans, job.place_on.clone()));
                    false
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => true,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    tracing::error!(source = job.source_id, "chord detection thread ended without a result");
                    crashed.push(job.source_id);
                    false
                }
            }
        });
        // 落ちた音源は自動では測り直さない (編集のたびに同じところで落ちる)。手で「コードを抽出」すれば測る。
        for source_id in crashed {
            if !self.cur.song_doc.song().audio_chords.contains_key(&source_id) {
                self.cur.chord_tools.detected_without_chords.insert(source_id);
            }
        }
        self.start_queued_detections();
        for (source_id, spans, place_on) in done {
            let n = spans.len();
            self.handle_event(AppEvent::ChordTool(ChordToolEvent::SetAudioChords { source_id, spans, place_on }));
            self.ui_ephemeral.status_message = format!("コードを検出しました ({n} 区間)");
        }
    }

    /// 追従を ON にしたオーディオトラックのクリップで、まだ検出していない音源を検出する (帯には置かない)。
    /// Song が変わったときだけ見る (`on_tick` から)。検出済み (コードあり = `Song.audio_chords`、コード無し =
    /// `detected_without_chords`) と発注済みの音源は積まない。
    pub(crate) fn auto_detect_followed_audio(&mut self) {
        let epoch = self.cur.song_doc.edit_epoch();
        if self.cur.chord_tools.auto_detect_epoch == Some(epoch) {
            return;
        }
        let song = self.cur.song_doc.song();
        if !song.chord_follow_enabled {
            self.cur.chord_tools.auto_detect_epoch = Some(epoch);
            return;
        }
        let opts = self.cur.chord_tools.view.detect.sanitized();
        let tools = &self.cur.chord_tools;
        let settled = |id: AudioSourceId| {
            song.audio_chords.contains_key(&id)
                || tools.detected_without_chords.contains(&id)
                || tools.detect_jobs.iter().any(|j| j.source_id == id)
        };
        let followed_events = song
            .tracks
            .iter()
            .filter(|t| !t.follow_chords.is_off())
            .flat_map(|t| t.clips.iter())
            .filter_map(|clip| Some((clip, song.clip_contents.get(&clip.content_id)?.audio_events()?)))
            .flat_map(|(clip, events)| events.iter().filter(|e| detectable(e)).map(move |ev| (clip, ev)));
        let mut requests: Vec<DetectRequest> = Vec::new();
        let mut missing = false;
        for (clip, ev) in followed_events {
            if settled(ev.source_id) || requests.iter().any(|r| r.source_id == ev.source_id) {
                continue;
            }
            match detect_request(song, &self.cur.media.audio_source_cache, clip, ev, &opts) {
                Some(r) => requests.push(r),
                None => missing = true,
            }
        }
        // 音源の読み込み中なら、終わってからもう一度見る (epoch を確定しない)。読み込みが終わっても手元に無い
        // 音源 (ファイルが無い / decode できない) は検出できないので待たない (毎ティック全クリップをなめ続けない)。
        if !(missing && self.audio_decode_pending()) {
            self.cur.chord_tools.auto_detect_epoch = Some(epoch);
        }
        for r in requests {
            self.order_detection(r, None, opts);
        }
        self.start_queued_detections();
    }

    /// `ChordToolEvent::SetAudioChords` の本体: 値を揃えて保存し、`place_on` のクリップの位置で帯にも置く。
    pub(crate) fn set_audio_chords(&mut self, source_id: AudioSourceId, mut spans: Vec<AudioChordSpan>, place_on: &[(u32, u32)]) {
        // 外 (検出スレッド / script) から来る値なので、昇順・非交差・長さ > 0 に揃えてから置く。
        AudioChordSpan::normalize_spans(&mut spans);
        // 退化した take の配置の速さの既定 (音源のサンプルレート。未 decode なら音源の記録値)。
        let sr = self.cur.media.audio_source_cache.get(source_id).map(|b| b.sample_rate).or_else(|| {
            self.cur.song_doc.song().media.audio_sources.get(&source_id).map(|s| s.sample_rate)
        });
        // 検出結果は音源から決まる派生データ: 履歴の全 Song に書き、undo step も `*` も作らない
        // (`SongDoc::write_derived` の doc — 履歴に積むと自動検出と undo が堂々巡りする)。
        let stored = self.write_derived_song(|song| {
            if !song.media.audio_sources.contains_key(&source_id) {
                return false;
            }
            if song.audio_chords.get(&source_id).map_or(&[][..], Vec::as_slice) == spans.as_slice() {
                return false;
            }
            if spans.is_empty() {
                song.audio_chords.remove(&source_id);
            } else {
                song.audio_chords.insert(source_id, spans.clone());
            }
            true
        });
        if stored.is_none() {
            return; // 書き出し中 (理由は status に出る)
        }
        // 「検出済み」の記録: コードがあれば Song が持ち、無ければ session が覚える (自動検出を走らせ直さない)。
        let without = &mut self.cur.chord_tools.detected_without_chords;
        if spans.is_empty() {
            without.insert(source_id);
        } else {
            without.remove(&source_id);
        }
        // 帯に置く箱はユーザーの編集 (「コードを抽出」 / ドロップの 1 undo step)。
        if place_on.is_empty() {
            return;
        }
        self.edit_song_checked(|song| {
            // まとめて置く (正規化 1 回。1 つずつ置くと箱の数の 2 乗で遅くなる)。
            let found: Vec<_> = place_on
                .iter()
                .flat_map(|&(track_id, clip_id)| {
                    chords_on_clip(song, track_id, clip_id, source_id, &spans, sr.unwrap_or(48_000))
                })
                .collect();
            song.place_chords(found)
        });
    }
}

/// 1 拍あたりの音源フレーム (event の take の配置の速さ。退化していれば曲のテンポとサンプルレート)。
fn frames_per_beat(song: &Song, ev: &AudioEvent, sample_rate: u32) -> f64 {
    let native = f64::from(sample_rate) * 60.0 / f64::from(song.bpm.max(1.0));
    ev.take_frames_per_beat(native)
}

/// 曲の拍 `window_beats` 刻みの境界を、クリップ `clip` の event `ev` の置き方で音源のフレームへ写した格子。
fn audio_detect_grid(song: &Song, clip: &Clip, ev: &AudioEvent, sample_rate: u32, opts: &DetectOptions) -> DetectGrid {
    let fpb = frames_per_beat(song, ev, sample_rate);
    #[allow(clippy::cast_precision_loss)]
    let source_start = ev.source_start_frames as f64;
    // 曲の拍 0 が音源のどこか: 拍 0 → content 拍 → take の頭からの拍 → フレーム。
    let origin_frame = source_start + (clip.song_to_content_beat(0.0) - ev.take_start_in_clip_beats()) * fpb;
    DetectGrid { origin_frame, window_frames: opts.sanitized().window_beats * fpb }
}

/// 音源のコード `spans` を、クリップが鳴らしている位置 (曲の拍) の箱にする。クリップの窓と event の
/// 見えている範囲で切る。id は 0 (置くときに採番)。
fn chords_on_clip(
    song: &Song,
    track_id: u32,
    clip_id: u32,
    source_id: AudioSourceId,
    spans: &[AudioChordSpan],
    sample_rate: u32,
) -> Vec<ChordEvent> {
    let Some(clip) = song.track_by_id(track_id).and_then(|t| t.clips.iter().find(|c| c.id == clip_id)) else {
        return Vec::new();
    };
    let Some(events) = song.clip_contents.get(&clip.content_id).and_then(|c| c.audio_events()) else {
        return Vec::new();
    };
    let (win_start, win_end) = clip.song_window();
    let mut out = Vec::new();
    for ev in events.iter().filter(|e| e.source_id == source_id && detectable(e)) {
        let fpb = frames_per_beat(song, ev, sample_rate);
        let vis_start = clip.content_to_song_beat(ev.event_start_in_clip_beats).max(win_start);
        let vis_end = clip.content_to_song_beat(ev.event_start_in_clip_beats + ev.event_length_beats).min(win_end);
        #[allow(clippy::cast_precision_loss)]
        let to_beat = |frame: u64| {
            clip.content_to_song_beat(ev.take_start_in_clip_beats() + (frame as f64 - ev.source_start_frames as f64) / fpb)
        };
        for s in spans {
            let (a, b) = (to_beat(s.start_frame).max(vis_start), to_beat(s.end_frame).min(vis_end));
            if b - a > 1e-6 {
                out.push(ChordEvent { id: 0, start_beat: a, len_beats: b - a, chord: s.chord, color: None });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use common::model::{AudioChordSpan, AudioSource, AudioSourcePath, Chord, ChordKind};

    /// 検出結果は undo に積まず `*` も立てない。後の編集を undo しても消えない (消えると自動検出し直して
    /// redo が消え、手前へ戻れなくなる)。
    #[test]
    fn detected_chords_stay_out_of_the_undo_history() {
        let mut app = crate::test_support::headless_app();
        let source_id = 7;
        app.edit_song(|song| {
            song.media.audio_sources.insert(source_id, AudioSource {
                path: AudioSourcePath::Absolute("x.wav".into()),
                sample_rate: 48_000,
                channels: 1,
                frames: 96_000,
                original_bpm: None,
                root_key: None,
            });
        });
        app.cur.song_doc.mark_saved();
        let depth = app.cur.song_doc.undo_depth();
        let spans = vec![AudioChordSpan { start_frame: 0, end_frame: 48_000, chord: Chord::new(9, ChordKind::Minor) }];
        app.set_audio_chords(source_id, spans.clone(), &[]);
        assert_eq!(app.cur.song_doc.undo_depth(), depth, "undo step を作らない");
        assert!(!app.cur.song_doc.is_dirty(), "`*` を立てない");
        // 検出より後の編集を undo すると、検出より前に積んだ snapshot に戻る — そこにも検出結果が入っている。
        app.edit_song(|song| song.bpm = 90.0);
        app.handle_event(crate::event::AppEvent::Undo);
        assert_eq!(app.cur.song_doc.song().bpm, 120.0);
        assert_eq!(app.cur.song_doc.song().audio_chords.get(&source_id), Some(&spans), "undo しても残る");
    }

    /// `poll_chord_detection` を、発注が無くなるまで回す (検出の thread を待つ)。
    fn drain_detection(app: &mut crate::state::AppData) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !app.cur.chord_tools.detect_jobs.is_empty() {
            assert!(std::time::Instant::now() < deadline, "検出が終わらない");
            app.poll_chord_detection();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// コードの出ない音源 (無音) は 1 回だけ測り、編集しても測り直さない。書き出し中に終わった結果は捨てずに、
    /// 書き出しが終わってから入れる。
    #[test]
    fn sources_without_chords_are_measured_once_and_wait_for_the_export() {
        use common::model::{AudioContent, AudioEvent, ChordFollow, ClipContent, Track};
        let mut app = crate::test_support::headless_app();
        let source_id = 7;
        let frames = 96_000u64;
        app.edit_song(|song| {
            song.chord_follow_enabled = true;
            song.media.audio_sources.insert(source_id, AudioSource {
                path: AudioSourcePath::Absolute("x.wav".into()),
                sample_rate: 48_000,
                channels: 1,
                frames,
                original_bpm: None,
                root_key: None,
            });
            let event = AudioEvent {
                id: 1,
                source_id,
                event_start_in_clip_beats: 0.0,
                event_length_beats: 4.0,
                source_start_frames: 0,
                source_end_frames: frames,
                ..AudioEvent::default()
            };
            let content_id =
                song.alloc_content(ClipContent::Audio(AudioContent { events: vec![event], next_event_id: 2 }), "a".into());
            let id = song.alloc_track_id();
            song.tracks.push(Track {
                id,
                follow_chords: ChordFollow::Parallel,
                clips: vec![common::model::Clip { id: 1, start_beat: 0.0, length_beats: 4.0, content_id, ..Default::default() }],
                ..Track::default()
            });
        });
        app.cur.media.audio_source_cache.insert(
            source_id,
            std::sync::Arc::new(crate::audio_source_cache::AudioSourceBuffer {
                origin: "x.wav".into(),
                sample_rate: 48_000,
                channels: 1,
                frames,
                samples: vec![vec![0.0; frames as usize]],
            }),
        );
        app.auto_detect_followed_audio();
        assert_eq!(app.cur.chord_tools.detect_jobs.len(), 1);
        // 書き出し中は結果を受け取らない (受け取ると Song に書けず捨てることになる)。
        app.cur.transport.export_stage = Some(crate::app_types::ExportStage::AudioRender { done: 0, total: 0 });
        std::thread::sleep(std::time::Duration::from_millis(300));
        app.poll_chord_detection();
        assert_eq!(app.cur.chord_tools.detect_jobs.len(), 1, "書き出し中は待つ");
        app.cur.transport.export_stage = None;
        drain_detection(&mut app);
        assert!(app.cur.chord_tools.detected_without_chords.contains(&source_id));
        assert!(!app.cur.song_doc.song().audio_chords.contains_key(&source_id));
        // 編集しても、コードの無かった音源は測り直さない。
        app.edit_song(|song| song.bpm = 100.0);
        app.auto_detect_followed_audio();
        assert!(app.cur.chord_tools.detect_jobs.is_empty(), "測り直さない");
    }
}
