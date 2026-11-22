//! handler::chord_audio — オーディオ音源からのコード検出の発注と回収 (r.md #157 §3.4)。
//!
//! - 発注: [`AppData::detect_audio_clips`] (クリップ → 帯のドロップ / 「コードを抽出…」) と
//!   [`AppData::auto_detect_followed_audio`] (追従を ON にしたオーディオトラックで、未検出の音源)。
//! - 検出: `crate::chord_audio_detect::detect_audio_chords` を音源ごとに 1 本の thread で (重い FFT)。
//! - 回収: `on_tick` → [`AppData::poll_chord_detection`] が終わった結果を
//!   `ChordToolEvent::SetAudioChords` で Song へ入れる。検出結果 (`Song.audio_chords`) は音源から決まる
//!   派生データなので undo に積まない (`SongDoc::write_derived`)。帯に置く箱だけが undo step になる。
//!
//! 窓の格子はクリップの置き方から作る: 曲の拍 `window_beats` 刻みの境界を音源のフレームへ写す
//! ([`audio_detect_grid`])。音源は複数のクリップから使われうるが、保存は音源ごと 1 つ
//! (`Song.audio_chords[source_id]`) なので、発注したクリップの置き方で 1 回測る。

use common::chord_detect::DetectOptions;
use common::model::{AudioChordSpan, AudioEvent, AudioSourceId, ChordEvent, Clip, Song};

use crate::chord_audio_detect::{DetectGrid, detect_audio_chords};
use crate::event::AppEvent;
use crate::event_chord::ChordToolEvent;
use crate::state::AppData;
use crate::state::chord_tools::AudioDetectJob;

impl AppData {
    /// オーディオクリップ `clips` (`(track id, clip id)`) の音源のコードを検出する (MIDI クリップは無視)。
    /// `place` なら結果を帯にも置く。同じ音源を検出中なら重ねて走らせない (置き先だけ足す)。
    pub(crate) fn detect_audio_clips(&mut self, clips: &[(u32, u32)], place: bool) {
        let opts = self.cur.chord_tools.view.detect;
        let mut started = 0usize;
        let mut waiting_decode = 0usize;
        for &(track_id, clip_id) in clips {
            let song = self.cur.song_doc.song();
            let Some(clip) = song.track_by_id(track_id).and_then(|t| t.clips.iter().find(|c| c.id == clip_id)) else {
                continue;
            };
            let Some(events) = song.clip_contents.get(&clip.content_id).and_then(|c| c.audio_events()) else {
                continue;
            };
            let mut sources: Vec<(AudioSourceId, DetectGrid)> = Vec::new();
            for ev in events.iter().filter(|e| !e.muted && !e.reversed) {
                if sources.iter().any(|(id, _)| *id == ev.source_id) {
                    continue;
                }
                let Some(buf) = self.cur.media.audio_source_cache.get(ev.source_id) else {
                    waiting_decode += 1;
                    continue;
                };
                sources.push((ev.source_id, audio_detect_grid(song, clip, ev, buf.sample_rate, &opts)));
            }
            for (source_id, grid) in sources {
                let place_on = place.then_some((track_id, clip_id));
                if let Some(job) = self.cur.chord_tools.detect_jobs.iter_mut().find(|j| j.source_id == source_id) {
                    job.place_on.extend(place_on);
                    continue;
                }
                let Some(buf) = self.cur.media.audio_source_cache.get(source_id) else { continue };
                let (tx, rx) = std::sync::mpsc::channel();
                let spawned = std::thread::Builder::new().name("chord-detect".into()).spawn(move || {
                    let spans = detect_audio_chords(&buf.samples, buf.sample_rate, grid, &opts);
                    let _ = tx.send(spans);
                });
                if let Err(e) = spawned {
                    tracing::error!(error = %e, "chord detection thread spawn failed");
                    continue;
                }
                self.cur.chord_tools.detect_jobs.push(AudioDetectJob {
                    source_id,
                    place_on: place_on.into_iter().collect(),
                    rx,
                });
                started += 1;
            }
        }
        if started > 0 {
            self.ui_ephemeral.status_message = format!("コードを検出中… ({started} 音源)");
        } else if waiting_decode > 0 {
            self.ui_ephemeral.status_message = "音源の読み込みが終わってからコードを検出できます".into();
        }
    }

    /// 終わった検出を Song に入れる (`on_tick` から毎ティック。検出していなければ何もしない)。
    pub(crate) fn poll_chord_detection(&mut self) {
        if self.cur.chord_tools.detect_jobs.is_empty() {
            return;
        }
        let mut done: Vec<(AudioSourceId, Vec<AudioChordSpan>, Vec<(u32, u32)>)> = Vec::new();
        self.cur.chord_tools.detect_jobs.retain(|job| match job.rx.try_recv() {
            Ok(spans) => {
                done.push((job.source_id, spans, job.place_on.clone()));
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                tracing::error!(source = job.source_id, "chord detection thread ended without a result");
                false
            }
        });
        for (source_id, spans, place_on) in done {
            let n = spans.len();
            self.handle_event(AppEvent::ChordTool(ChordToolEvent::SetAudioChords { source_id, spans, place_on }));
            self.ui_ephemeral.status_message = format!("コードを検出しました ({n} 区間)");
        }
    }

    /// 追従を ON にしたオーディオトラックのクリップで、まだ検出していない音源を検出する (帯には置かない)。
    /// Song が変わったときだけ見る (`on_tick` から)。
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
        let mut wanted: Vec<(u32, u32)> = Vec::new();
        let mut pending_decode = false;
        for t in song.tracks.iter().filter(|t| !t.follow_chords.is_off()) {
            for clip in &t.clips {
                let Some(events) = song.clip_contents.get(&clip.content_id).and_then(|c| c.audio_events()) else {
                    continue;
                };
                let missing = events.iter().any(|e| {
                    !song.audio_chords.contains_key(&e.source_id)
                        && !self.cur.chord_tools.detect_jobs.iter().any(|j| j.source_id == e.source_id)
                        && !wanted_has_source(song, &wanted, e.source_id)
                });
                if !missing {
                    continue;
                }
                if events.iter().all(|e| self.cur.media.audio_source_cache.contains(e.source_id)) {
                    wanted.push((t.id, clip.id));
                } else {
                    pending_decode = true;
                }
            }
        }
        // 読み込み待ちの音源があれば、次のティックでもう一度見る (epoch を進めない)。
        if !pending_decode {
            self.cur.chord_tools.auto_detect_epoch = Some(epoch);
        }
        if !wanted.is_empty() {
            self.detect_audio_clips(&wanted, false);
        }
    }

    /// `ChordToolEvent::SetAudioChords` の本体: 値を揃えて保存し、`place_on` のクリップの位置で帯にも置く。
    pub(crate) fn set_audio_chords(&mut self, source_id: AudioSourceId, mut spans: Vec<AudioChordSpan>, place_on: &[(u32, u32)]) {
        // 外 (検出スレッド / script) から来る値なので、昇順・非交差・長さ > 0 に揃えてから置く。
        spans.retain(|s| s.end_frame > s.start_frame);
        spans.sort_by_key(|s| s.start_frame);
        let mut prev_end = 0u64;
        for s in &mut spans {
            s.start_frame = s.start_frame.max(prev_end);
            s.chord = s.chord.sanitized();
            prev_end = prev_end.max(s.end_frame);
        }
        spans.retain(|s| s.end_frame > s.start_frame);
        // 退化した take の配置の速さの既定 (音源のサンプルレート。未 decode なら音源の記録値)。
        let sr = self.cur.media.audio_source_cache.get(source_id).map(|b| b.sample_rate).or_else(|| {
            self.cur.song_doc.song().media.audio_sources.get(&source_id).map(|s| s.sample_rate)
        });
        // 検出結果は音源から決まる派生データ: 履歴の全 Song に書き、undo step も `*` も作らない
        // (`SongDoc::write_derived` の doc — 履歴に積むと自動検出と undo が堂々巡りする)。
        let stored = self.cur.song_doc.write_derived(|song| {
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
        // 帯に置く箱はユーザーの編集 (「コードを抽出」 / ドロップの 1 undo step)。
        if place_on.is_empty() {
            return;
        }
        self.edit_song_checked(|song| {
            let mut placed = false;
            for &(track_id, clip_id) in place_on {
                for c in chords_on_clip(song, track_id, clip_id, source_id, &spans, sr.unwrap_or(48_000)) {
                    placed |= song.place_chord(c).is_some();
                }
            }
            placed
        });
    }
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
}

/// `wanted` のクリップのどれかが音源 `source_id` を使っているか (同じ音源を 2 回発注しない)。
fn wanted_has_source(song: &Song, wanted: &[(u32, u32)], source_id: AudioSourceId) -> bool {
    wanted.iter().any(|&(t, c)| {
        song.track_by_id(t)
            .and_then(|t| t.clips.iter().find(|x| x.id == c))
            .and_then(|clip| song.clip_contents.get(&clip.content_id))
            .and_then(|c| c.audio_events())
            .is_some_and(|evs| evs.iter().any(|e| e.source_id == source_id))
    })
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
    let window = if opts.window_beats.is_finite() && opts.window_beats > 0.0 { opts.window_beats } else { 2.0 };
    DetectGrid { origin_frame, window_frames: window * fpb }
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
    for ev in events.iter().filter(|e| e.source_id == source_id && !e.muted && !e.reversed) {
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
