//! Audio-side automation playback. Walks the song's automation lanes,
//! evaluates curves at sample resolution, and fills the per-track
//! `volume_per_sample` / `pan_per_sample` ramps for the buffer.
//!
//! Phase 1: track-builtin Volume / Pan only. Plugin parameter
//! automation generates `clap_event_param_value` events through the
//! plugin host and is wired in Phase 2 (`docs/plan_automation.md`).
//!
//! RT-safe: no allocation, no I/O, no locking. Reads `Song` through a
//! shared reference (`ArcSwap<Song>` already loaded by the caller).

#![allow(dead_code)]

use common::automation::lane_value_at;
use common::model::{AutomationTarget, Song, TrackBuiltinParam};
use common::process_data::ProcessData;
use common::protocol::PluginSlot;

/// Fill `volume_per_sample` / `pan_per_sample` (each at least `frames`
/// long, but typically `MAX_FRAMES`) for the given track and buffer.
///
/// Default fill: each sample gets `track.volume` / `track.pan` as a
/// constant. Each enabled `Volume` / `Pan` lane then overwrites its
/// target buffer with the curve value sampled at every sample position.
///
/// `bpm == 0` or `sample_rate == 0` short-circuits to the constant
/// fallback (defensive — the engine starts in this state during init).
///
/// Lanes targeting plugin parameters are silently skipped: those are
/// converted into `TimedParamEvent`s elsewhere (Phase 2).
#[allow(clippy::too_many_arguments)]
pub fn fill_track_param_ramps(
    song: Option<&Song>,
    track_idx: u32,
    sample_rate: u32,
    bpm: f32,
    playhead: u64,
    frames: u32,
    volume_per_sample: &mut [f32],
    pan_per_sample: &mut [f32],
) {
    let frames = (frames as usize).min(volume_per_sample.len()).min(pan_per_sample.len());
    if frames == 0 {
        return;
    }
    let (track_volume, track_pan) = song
        .and_then(|s| s.tracks.get(track_idx as usize))
        .map(|t| (t.volume, t.pan))
        .unwrap_or((1.0, 0.0));
    for slot in volume_per_sample.iter_mut().take(frames) {
        *slot = track_volume;
    }
    for slot in pan_per_sample.iter_mut().take(frames) {
        *slot = track_pan;
    }

    let Some(song) = song else { return };
    let Some(track) = song.tracks.get(track_idx as usize) else {
        return;
    };
    if bpm <= 0.0 || sample_rate == 0 {
        return;
    }
    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(bpm);
    if samples_per_beat <= 0.0 {
        return;
    }

    for lane in &track.automation_lanes {
        if !lane.enabled {
            continue;
        }
        // Borrow the right buffer for this lane's target. Plugin
        // parameter / Mute / send / song-level lanes are handled
        // elsewhere — skip silently here.
        let buf: &mut [f32] = match lane.target {
            AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume) => {
                volume_per_sample
            }
            AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan) => pan_per_sample,
            _ => continue,
        };
        for (i, slot) in buf.iter_mut().enumerate().take(frames) {
            let sample_pos = playhead + i as u64;
            let beat = sample_pos as f64 / samples_per_beat;
            *slot = lane_value_at(lane, &song.clip_contents, beat) as f32;
        }
    }
}

/// Phase 2b (`docs/plan_automation.md` §8.3): push automation events for
/// the specified plugin slot into `pd.events_in` as `EventKind::
/// ParamValue` entries. plugin_host's `process_server` decodes them into
/// `TimedParamEvent` and forwards to `LoadedPlugin::process(..,
/// param_events, ..)` which converts them to CLAP `clap_event_param_value`
/// / VST3 `IParameterChanges`.
///
/// Phase 2 では「1 buffer = 1 update」 (frame 0 でのみ curve 値を 1 度
/// push) として簡素実装。 frame 単位 sample (= 64 frame 刻みで複数
/// push) は Phase 3+ でカーブの滑らかさが必要になったときに拡張。
///
/// RT 安全性: `push_param` は固定 capacity の `events_in` 配列に書く
/// だけ、 allocation なし。 `lane_value_at` は curve evaluator のみ
/// (allocation なし、 浮動小数演算のみ)。
#[allow(clippy::too_many_arguments)]
pub fn fill_pd_param_events(
    pd: &mut ProcessData,
    song: &Song,
    track_id: u32,
    slot: PluginSlot,
    sample_rate: u32,
    bpm: f32,
    playhead: u64,
    frames: u32,
) {
    if frames == 0 || bpm <= 0.0 || sample_rate == 0 {
        return;
    }
    let Some(track) = song.tracks.iter().find(|t| t.id == track_id) else {
        return;
    };
    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(bpm);
    if samples_per_beat <= 0.0 {
        return;
    }
    let beat = playhead as f64 / samples_per_beat;
    for lane in &track.automation_lanes {
        if !lane.enabled {
            continue;
        }
        let param_id = match &lane.target {
            AutomationTarget::PluginParam { slot: s, param_id }
                if *s == slot =>
            {
                *param_id
            }
            _ => continue,
        };
        let value = lane_value_at(lane, &song.clip_contents, beat);
        pd.push_param(0, param_id, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{
        AutomationClip, AutomationContent, AutomationCurve, AutomationLane,
        AutomationPoint, ClipContent, Song, Track,
    };

    /// Helper: build a song with one track owning a single automation
    /// lane that ramps `Volume` from 0.0 → 1.0 across beats 0..4.
    fn one_volume_lane_song() -> Song {
        let mut song = Song {
            bpm: 120.0,
            ..Song::default()
        };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint {
                        time_beat: 0.0,
                        value: 0.0,
                        curve: AutomationCurve::Linear,
                    },
                    AutomationPoint {
                        time_beat: 4.0,
                        value: 1.0,
                        curve: AutomationCurve::Linear,
                    },
                ],
            }),
        );
        let lane = AutomationLane {
            id: 1,
            clips: vec![AutomationClip {
                id: 1,
                name: "vol".into(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
            }],
            next_clip_id: 2,
            ..AutomationLane::new(
                AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                0.5,
            )
        };
        song.tracks.push(Track {
            id: 1,
            name: "T".into(),
            volume: 0.5,
            automation_lanes: vec![lane],
            next_lane_id: 2,
            ..Track::default()
        });
        song
    }

    /// 120 BPM at 48 kHz → 24000 samples/beat.
    const SR: u32 = 48000;

    #[test]
    fn no_song_falls_back_to_unity_volume_zero_pan() {
        let mut vol = vec![0.0_f32; 8];
        let mut pan = vec![0.5_f32; 8];
        fill_track_param_ramps(None, 0, SR, 120.0, 0, 8, &mut vol, &mut pan);
        assert!(vol.iter().all(|&v| (v - 1.0).abs() < 1e-6));
        assert!(pan.iter().all(|&p| p.abs() < 1e-6));
    }

    #[test]
    fn no_lanes_fills_with_track_strip_constants() {
        let mut song = Song {
            bpm: 120.0,
            ..Song::default()
        };
        song.tracks.push(Track {
            id: 1,
            name: "T".into(),
            volume: 0.7,
            pan: -0.25,
            ..Track::default()
        });
        let mut vol = vec![0.0_f32; 16];
        let mut pan = vec![0.0_f32; 16];
        fill_track_param_ramps(Some(&song), 0, SR, 120.0, 0, 16, &mut vol, &mut pan);
        assert!(vol.iter().all(|&v| (v - 0.7).abs() < 1e-6));
        assert!(pan.iter().all(|&p| (p - -0.25).abs() < 1e-6));
    }

    #[test]
    fn volume_lane_ramps_across_buffer() {
        let song = one_volume_lane_song();
        let mut vol = vec![0.0_f32; 16];
        let mut pan = vec![0.0_f32; 16];
        // Buffer of 16 samples starting at playhead 0 → first 16 samples
        // out of 24000 samples-per-beat × 4 = 96000 total. Volume should
        // ramp from 0.0 toward ~16/96000 ≈ 0.000167.
        fill_track_param_ramps(Some(&song), 0, SR, 120.0, 0, 16, &mut vol, &mut pan);
        assert!(vol[0].abs() < 1e-6);
        assert!(vol[15] > 0.0 && vol[15] < 0.001, "vol[15]={}", vol[15]);
        // Pan untouched (no Pan lane) → falls back to track.pan = 0.
        assert!(pan.iter().all(|&p| p.abs() < 1e-6));
    }

    #[test]
    fn buffer_at_clip_midpoint_returns_half() {
        let song = one_volume_lane_song();
        let mut vol = vec![0.0_f32; 4];
        let mut pan = vec![0.0_f32; 4];
        // Beat 2.0 = 48000 samples in. Curve linear 0→1 over beats 0..4
        // → value 0.5 at beat 2.
        fill_track_param_ramps(Some(&song), 0, SR, 120.0, 48_000, 4, &mut vol, &mut pan);
        for &v in vol.iter() {
            assert!((v - 0.5).abs() < 0.001, "expected ~0.5, got {}", v);
        }
    }

    #[test]
    fn disabled_lane_uses_default_value() {
        let mut song = one_volume_lane_song();
        song.tracks[0].automation_lanes[0].enabled = false;
        // default_value is 0.5 from one_volume_lane_song.
        let mut vol = vec![0.0_f32; 4];
        let mut pan = vec![0.0_f32; 4];
        fill_track_param_ramps(Some(&song), 0, SR, 120.0, 0, 4, &mut vol, &mut pan);
        // Bypass returns the lane's default — but `fill_track_param_ramps`
        // only writes the lane buffer when `enabled = true`. With the
        // lane disabled the constant fallback (track.volume = 0.5)
        // remains, which happens to equal the default in this test.
        for &v in vol.iter() {
            assert!((v - 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn outside_clip_range_uses_default_value() {
        let song = one_volume_lane_song();
        // Buffer at beat 10 (sample 240_000) — clip ends at beat 4.
        // lane_value_at returns lane.default_value = 0.5.
        let mut vol = vec![0.0_f32; 4];
        let mut pan = vec![0.0_f32; 4];
        fill_track_param_ramps(Some(&song), 0, SR, 120.0, 240_000, 4, &mut vol, &mut pan);
        for &v in vol.iter() {
            assert!((v - 0.5).abs() < 1e-6, "got {}", v);
        }
    }
}
