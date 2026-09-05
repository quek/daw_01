//! handler::sampler — Global Sampler / MIDI Capture (`docs/plan_global_sampler.md`)。
//!
//! - リングの世代管理 (create → `OpenSamplerRing` → 旧世代 drop)。長さ / 録音源の
//!   変更と audio 子プロセスの再起動で世代を進める / 送り直す。
//! - 選択範囲の切り出し: リング → 32-bit float WAV (scratch) → `import_one`
//!   (samples/ へ hash 付き複製) → [`AppData::place_imported_audio`] (ファイル取り込みと
//!   同じ配置経路)。
//! - MIDI Capture の捕捉 / 切り出し ([`crate::state::midi_capture::build_clip_notes`]) /
//!   試聴 (`PreviewSequence`)。

use std::sync::Arc;

use common::protocol::{AudioCommand, PreviewNote, SamplerSource};
use common::sampler_ring::{SamplerRingHandle, sampler_shmem_id};

use crate::app_types::*;
use crate::event_sampler::SamplerEvent;
use crate::state::midi_capture::{CaptureWindow, build_clip_notes};
use crate::state::sampler::{Overview, SamplerTick, wall_clock_ns};
use crate::state::*;

impl AppData {
    /// [`AppEvent::Sampler`](crate::app::AppEvent::Sampler) の入口。
    pub fn handle_sampler_event(&mut self, ev: SamplerEvent) {
        use SamplerEvent as E;
        match ev {
            // Mixer (`ToggleMixerPanel`) と同じトグル規則でタブ 2 / 3 を開閉する。
            E::TogglePanel => self.toggle_bottom_tab(2),
            E::ToggleMidiCapturePanel => self.toggle_bottom_tab(3),
            E::Tick(tick) => self.on_sampler_tick(tick),
            E::SetSource(source) => self.set_sampler_source(source),
            E::SetSeconds { seconds, commit } => self.set_sampler_seconds(seconds, commit),
            E::TogglePaused => self.toggle_sampler_paused(),
            E::SetSelection(sel) => self.set_sampler_selection(sel),
            E::TogglePreview => self.toggle_sampler_preview(),
            E::Drop { start_frame, end_frame, target, target_beat } => {
                self.sampler_drop(start_frame, end_frame, target, target_beat);
            }
            E::MidiCaptured { at_ns, channel, pitch, velocity } => {
                self.on_midi_captured(at_ns, channel, pitch, velocity);
            }
            E::SetMidiSelection(sel) => self.set_midi_capture_selection(sel),
            E::ToggleMidiPaused => self.toggle_midi_capture_paused(),
            E::ToggleMidiPreview => self.toggle_midi_capture_preview(),
            E::MidiDrop { start_ns, end_ns, target, target_beat } => {
                self.midi_capture_drop(start_ns, end_ns, target, target_beat);
            }
        }
    }

    fn toggle_bottom_tab(&mut self, tab: u8) {
        self.ui_prefs.bottom_panel =
            if self.ui_prefs.bottom_panel == Some(tab) { None } else { Some(tab) };
    }

    /// 溜める長さ (秒)。SSoT は `app_config.json` (`UiPrefs::sampler_seconds`)。
    pub(crate) fn sampler_seconds(&self) -> u32 {
        self.ui_prefs.sampler_seconds.clamp(1, common::sampler_ring::MAX_SECONDS)
    }

    /// 新しい世代のリングを create して audio へ open させる。旧世代は
    /// GUI 側の Arc を落とす (audio 側は bundle の recycle で drop) — 名前が
    /// 違うので解放の順序は問わない (`project_shmem_name_reuse_race`)。
    ///
    /// 子プロセスが居ない経路 (test / script) では何もしない。
    pub(crate) fn reopen_sampler_ring(&mut self) {
        if self.ipc.supervisor.is_none() {
            return;
        }
        let generation = self.sampler.generation.wrapping_add(1).max(1);
        let shmem_id = sampler_shmem_id(std::process::id(), generation);
        let ring = match SamplerRingHandle::create(&shmem_id, self.sampler_seconds(), self.ipc.sample_rate) {
            Ok(r) => Arc::new(r),
            Err(e) => {
                tracing::warn!(error = ?e, %shmem_id, "failed to create sampler ring");
                self.ui_ephemeral.status_message = format!("Sampler: リングを確保できません ({e})");
                return;
            }
        };
        ring.set_paused(self.sampler.paused);
        self.sampler.generation = generation;
        self.sampler.overview = Overview::with_capacity_frames(ring.capacity());
        self.sampler.write_frames = 0;
        self.sampler.segments.clear();
        self.sampler.selection = None;
        self.sampler.preview_until = None;
        if let Ok(mut g) = self.sampler.shared.ring.lock() {
            *g = Some((generation, Arc::clone(&ring)));
        }
        self.sampler.ring = Some(ring);
        self.send_open_sampler_ring();
    }

    /// 現世代の `OpenSamplerRing` を送る (初回 / 再確保 / audio 再起動後の再送)。
    pub(crate) fn send_open_sampler_ring(&self) {
        if self.sampler.ring.is_none() {
            return;
        }
        self.send_audio(AudioCommand::OpenSamplerRing {
            shmem_id: sampler_shmem_id(std::process::id(), self.sampler.generation),
            source: self.sampler.source,
        });
    }

    /// audio 子プロセスの再起動後: レートが同じなら現世代を open し直し、変わって
    /// いれば (リングの時間軸が狂うので) 新世代で作り直す。
    pub(crate) fn resume_sampler_ring_after_respawn(&mut self) {
        let same_rate = self
            .sampler
            .ring
            .as_ref()
            .is_some_and(|r| r.sample_rate() == self.ipc.sample_rate);
        if same_rate {
            self.send_open_sampler_ring();
        } else {
            self.reopen_sampler_ring();
        }
    }

    pub(crate) fn set_sampler_source(&mut self, source: SamplerSource) {
        if self.sampler.source == source {
            return;
        }
        self.sampler.source = source;
        // 別の音になるので中身は捨てる (Q3)。
        self.reopen_sampler_ring();
    }

    pub(crate) fn set_sampler_seconds(&mut self, seconds: u32, commit: bool) {
        let next = seconds.clamp(1, common::sampler_ring::MAX_SECONDS);
        self.ui_prefs.sampler_seconds = next;
        if !commit {
            return;
        }
        self.persist_app_config();
        self.midi_capture.prune(wall_clock_ns(), next);
        if self.sampler.ring.as_ref().is_some_and(|r| r.capacity() as u64 != u64::from(next) * u64::from(r.sample_rate())) {
            self.reopen_sampler_ring();
        }
    }

    pub(crate) fn toggle_sampler_paused(&mut self) {
        self.sampler.paused = !self.sampler.paused;
        if let Some(r) = &self.sampler.ring {
            r.set_paused(self.sampler.paused);
        }
    }

    pub(crate) fn set_sampler_selection(&mut self, sel: Option<(u64, u64)>) {
        self.sampler.selection = sel.filter(|(s, e)| e > s);
        if self.sampler.selection.is_none() {
            self.stop_sampler_preview();
        }
    }

    pub(crate) fn toggle_sampler_preview(&mut self) {
        if self.sampler.preview_until.is_some() {
            self.stop_sampler_preview();
            return;
        }
        let Some((start, end)) = self.sampler.selection else { return };
        let secs = (end - start) as f64 / f64::from(self.sampler.sample_rate().max(1));
        self.sampler.preview_until =
            Some(std::time::Instant::now() + std::time::Duration::from_secs_f64(secs));
        self.send_audio(AudioCommand::SamplerPreview { start_frame: start, end_frame: end });
    }

    fn stop_sampler_preview(&mut self) {
        if self.sampler.preview_until.take().is_some() {
            self.send_audio(AudioCommand::SamplerPreviewStop);
        }
    }

    pub(crate) fn on_sampler_tick(&mut self, tick: SamplerTick) {
        if tick.generation != self.sampler.generation {
            return;
        }
        self.sampler.overview.apply(tick.first_bucket, &tick.buckets);
        self.sampler.write_frames = tick.write_frames;
        if let Some(segs) = tick.segments {
            self.sampler.segments = segs;
        }
        self.sampler.prune_selection();
    }

    /// `on_tick` から: 試聴の期限切れと MIDI Capture の古いノートの回収。
    pub(crate) fn sampler_housekeeping(&mut self) {
        let now = std::time::Instant::now();
        if self.sampler.preview_until.is_some_and(|t| t <= now) {
            self.sampler.preview_until = None;
        }
        if self.midi_capture.preview_until.is_some_and(|t| t <= now) {
            self.midi_capture.preview_until = None;
        }
        let seconds = self.sampler_seconds();
        self.midi_capture.prune(wall_clock_ns(), seconds);
    }

    /// 選択範囲を WAV に書き出し、ファイル取り込みと同じ経路で clip にする。
    pub(crate) fn sampler_drop(
        &mut self,
        start_frame: u64,
        end_frame: u64,
        target: ImportTrackTarget,
        target_beat: Option<f64>,
    ) {
        let Some(ring) = self.sampler.ring.clone() else { return };
        let mut frames: Vec<[f32; 2]> = Vec::new();
        if let Err(e) = ring.read_range(start_frame, end_frame, &mut frames) {
            self.ui_ephemeral.status_message = format!("Sampler: {e}");
            return;
        }
        let name = format!(
            "Sampler_{}_{}",
            source_label_short(self.song_doc.song(), self.sampler.source),
            chrono_stamp()
        );
        let tmp = std::env::temp_dir().join(format!("{name}.wav"));
        if let Err(e) = write_float_wav(&tmp, &frames, ring.sample_rate()) {
            self.ui_ephemeral.status_message = format!("Sampler: WAV を書けません ({e})");
            return;
        }
        let project_dir = self.project_dir();
        let imported = crate::import_audio::import_one(&tmp, project_dir.as_deref());
        let _ = std::fs::remove_file(&tmp);
        let imported = match imported {
            Ok(i) => i,
            Err(e) => {
                self.ui_ephemeral.status_message = format!("Sampler: 取り込みに失敗 ({e})");
                return;
            }
        };
        let Some(mut placement) = self.begin_audio_placement(target, target_beat, name.clone())
        else {
            return;
        };
        if self.place_imported_audio(&mut placement, imported) {
            self.ui_ephemeral.status_message = format!("Sampler: {name} を配置しました");
        }
    }

    // ---- MIDI Capture -----------------------------------------------------

    pub(crate) fn on_midi_captured(&mut self, at_ns: u64, channel: u8, pitch: u8, velocity: Option<u8>) {
        let beat = self.beat_at_wall_ns(at_ns);
        match velocity {
            Some(v) => self.midi_capture.note_on(at_ns, channel, pitch, v, beat),
            None => self.midi_capture.note_off(at_ns, channel, pitch, beat),
        }
    }

    /// wall-clock `at_ns` に曲がどの拍を再生していたか。停止中は `None`。
    ///
    /// 第一候補は engine の走行セグメント (wall-clock ↔ playhead samples がサンプル精度で
    /// 対応する SSoT)。まだ届いていない (再生開始直後の ≤33ms) / リング無しのときだけ
    /// 30Hz tick の `transport.playhead_beat` へ落とす。
    fn beat_at_wall_ns(&self, at_ns: u64) -> Option<f64> {
        let segs = &self.sampler.segments;
        let idx = segs.iter().rposition(|s| s.wall_ns <= at_ns);
        let from_segment = idx.and_then(|i| {
            let seg = &segs[i];
            // 次のセグメントが既にあり、それが at_ns より前なら i が最新ではない
            // (rposition なので起きないが、wall_ns の単調性が崩れた場合の保険)。
            let ph = seg.playhead_samples?;
            let sr = self.sampler.sample_rate().max(1);
            let dt = (at_ns - seg.wall_ns) as u128 * u128::from(sr) / 1_000_000_000;
            let samples = ph.saturating_add(dt as u64);
            let tempo = common::tempo_map::TempoMap::from_song(self.song_doc.song());
            Some(tempo.samples_to_beat(samples, self.ipc.sample_rate.max(1)))
        });
        from_segment.or_else(|| {
            self.transport
                .is_playing
                .then(|| self.transport.playhead_beat.map(f64::from))
                .flatten()
        })
    }

    pub(crate) fn set_midi_capture_selection(&mut self, sel: Option<(u64, u64)>) {
        self.midi_capture.selection = sel.filter(|(s, e)| e > s);
        if self.midi_capture.selection.is_none() {
            self.stop_midi_capture_preview();
        }
    }

    pub(crate) fn toggle_midi_capture_paused(&mut self) {
        self.midi_capture.paused = !self.midi_capture.paused;
    }

    /// 選択ノートを cursor track のインストで鳴らす (buffer 精度、engine 側)。
    pub(crate) fn toggle_midi_capture_preview(&mut self) {
        if self.midi_capture.preview_until.is_some() {
            self.stop_midi_capture_preview();
            return;
        }
        let Some((start_ns, end_ns)) = self.midi_capture.selection else { return };
        let Some(track_id) = self.cursor_track_id() else {
            self.ui_ephemeral.status_message = "MIDI Capture: 試聴するトラックを選んでください".into();
            return;
        };
        let now = wall_clock_ns();
        let sr = f64::from(self.ipc.sample_rate.max(1));
        let ns_to_frames = |ns: u64| (ns as f64 * sr / 1e9).round() as u64;
        let mut notes: Vec<PreviewNote> = self
            .midi_capture
            .notes_in(start_ns, end_ns, now)
            .map(|n| {
                let on = n.on_ns.max(start_ns) - start_ns;
                let off = n.end_ns(now).min(end_ns).max(n.on_ns.max(start_ns)) - start_ns;
                PreviewNote {
                    offset_frames: ns_to_frames(on),
                    duration_frames: ns_to_frames(off - on).max(1),
                    pitch: n.pitch,
                    velocity: n.velocity,
                }
            })
            .collect();
        if notes.is_empty() {
            return;
        }
        notes.sort_by_key(|n| n.offset_frames);
        let secs = (end_ns - start_ns) as f64 / 1e9;
        self.midi_capture.preview_until =
            Some(std::time::Instant::now() + std::time::Duration::from_secs_f64(secs));
        self.send_audio(AudioCommand::PreviewSequence { track_id, notes });
    }

    fn stop_midi_capture_preview(&mut self) {
        if self.midi_capture.preview_until.take().is_some() {
            self.send_audio(AudioCommand::PreviewSequenceStop);
        }
    }

    /// 選択範囲を MIDI clip にしてアレンジ / セルへ置く。
    pub(crate) fn midi_capture_drop(
        &mut self,
        start_ns: u64,
        end_ns: u64,
        target: ImportTrackTarget,
        target_beat: Option<f64>,
    ) {
        let song = self.song_doc.song();
        let win = CaptureWindow {
            start_ns,
            end_ns,
            bpm: f64::from(song.bpm),
            beats_per_bar: common::model::beats_per_bar(song.time_sig),
            now_ns: wall_clock_ns(),
        };
        let Some((notes, length_beats)) = build_clip_notes(&self.midi_capture, win) else {
            self.ui_ephemeral.status_message = "MIDI Capture: 選択範囲にノートがありません".into();
            return;
        };
        let n_tracks = song.tracks.len();
        let cell_track_idx = match target {
            ImportTrackTarget::LauncherCell { track_id, .. } => song.track_index_of(track_id),
            _ => None,
        };
        let dest = cell_track_idx.or_else(|| resolve_media_drop_target(target, n_tracks));
        let start_beat = target_beat
            .unwrap_or(self.transport.playhead_beat.unwrap_or(0.0) as f64)
            .max(0.0);
        let cell_index = crate::handler::media::import_cell_index(target, 0);
        let name = format!("Capture_{}", chrono_stamp());
        let n_notes = notes.len();
        let applied = self.edit_song(|song| {
            let content_id = song.alloc_content(
                common::model::ClipContent::Midi(common::model::MidiContent {
                    next_note_id: n_notes as u32 + 1,
                    notes,
                }),
                name.clone(),
            );
            let dest_idx = match dest {
                Some(i) => i,
                None => {
                    let track_id = song.alloc_track_id();
                    song.tracks.push(track_with(|t| {
                        t.id = track_id;
                        t.name = name.clone();
                    }));
                    song.tracks.len() - 1
                }
            };
            crate::handler::media::place_new_midi_clip(
                song,
                dest_idx,
                cell_index,
                (start_beat, length_beats, 0.0),
                content_id,
            );
            song.length_beats = song.length_beats.max(start_beat + length_beats);
        });
        if applied.is_some() {
            self.resize_track_peak_display();
            self.ui_ephemeral.status_message =
                format!("MIDI Capture: {n_notes} ノートを配置しました");
        }
    }
}

/// 録音源の短い表示名 (ファイル名に入る)。
fn source_label_short(song: &common::model::Song, source: SamplerSource) -> String {
    match source {
        SamplerSource::Master => "Master".to_string(),
        SamplerSource::Track(tap) => {
            let name = song
                .tracks
                .iter()
                .find(|t| t.id == tap.source_track)
                .map(|t| t.name.as_str())
                .unwrap_or("Track");
            let safe: String = name
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { '_' })
                .take(24)
                .collect();
            format!("{safe}_{}", tap_point_label(tap.tap_point))
        }
    }
}

pub(crate) fn tap_point_label(tp: common::model::TapPoint) -> &'static str {
    match tp {
        common::model::TapPoint::PreFx => "Pre-FX",
        common::model::TapPoint::PostFx => "Post-FX",
        common::model::TapPoint::PostFader => "Post-Fader",
    }
}

/// `HH-MM-SS` (ローカル時刻)。ファイル名に使うのでコロンは使わない。
fn chrono_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let local = secs as i64 + local_utc_offset_secs();
    let day = local.rem_euclid(86_400);
    format!("{:02}-{:02}-{:02}", day / 3600, (day % 3600) / 60, day % 60)
}

#[cfg(windows)]
fn local_utc_offset_secs() -> i64 {
    use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    let mut tz = TIME_ZONE_INFORMATION::default();
    let r = unsafe { GetTimeZoneInformation(&mut tz) };
    // 戻りは 1 = 標準時 / 2 = 夏時間 / 0 = 不明 (Bias のみ)。Bias は分、UTC = local + Bias。
    let bias = match r {
        2 => tz.Bias + tz.DaylightBias,
        1 => tz.Bias + tz.StandardBias,
        _ => tz.Bias,
    };
    -i64::from(bias) * 60
}

#[cfg(not(windows))]
fn local_utc_offset_secs() -> i64 {
    0
}

/// 32-bit float / stereo の WAV を書く (取り込み経路が読める形式)。
fn write_float_wav(path: &std::path::Path, frames: &[[f32; 2]], sample_rate: u32) -> anyhow::Result<()> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: sample_rate.max(1),
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for f in frames {
        w.write_sample(f[0])?;
        w.write_sample(f[1])?;
    }
    w.finalize()?;
    Ok(())
}
