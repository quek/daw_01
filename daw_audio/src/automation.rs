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
///
/// Phase 4 Step C-2: `recording_lanes` に `(track_id, lane.target)` が含まれて
/// いる lane は curve eval を **skip** し、 track.volume / track.pan の constant
/// fallback がそのまま buffer に残る。 これで GUI 側の knob 操作 (=
/// SetTrackVolume / SetTrackPan IPC) が即時に audio に反映される (Live /
/// Bitwig の Touch / Latch / Write の "you hear what you do" UX)。 set は
/// audio thread が buffer 頭で `load()` する snapshot (ArcSwap、 lock-free)。
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
    recording_lanes: &std::collections::HashSet<(u32, AutomationTarget)>,
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
    let track_id = track.id;
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
        // Phase 4 Step C-2: 現在 recording 中の lane なら curve eval skip。
        // 上で fill した track.volume / track.pan の constant が残る。
        if recording_lanes
            .iter()
            .any(|(t, tg)| *t == track_id && *tg == lane.target)
        {
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
    recording_lanes: &std::collections::HashSet<(u32, AutomationTarget)>,
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
        // Phase 4 Step C-2 (plugin param 版): recording 中 (Touch/Latch/Write)
        // の lane は curve eval を skip する。 これで plugin が自身の GUI で
        // 持っている値 (= ユーザのノブ操作) を host が curve で毎バッファ
        // 上書きするのを止め、「you hear what you do」 を成立させる。 track
        // builtin Volume/Pan の `fill_track_param_ramps` と同じ仕組みだが、
        // 旧実装は plugin param 側にこの skip が無く、 write が read のまま /
        // touch が半分しか効かないバグだった。
        if recording_lanes
            .iter()
            .any(|(t, tg)| *t == track_id && *tg == lane.target)
        {
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

    /// Phase 4 Step C-2: 既存テストは recording 中ではないので、 共通 helper
    /// で empty set を borrow する。
    fn empty_recording_lanes()
    -> std::collections::HashSet<(u32, common::model::AutomationTarget)> {
        std::collections::HashSet::new()
    }

    #[test]
    fn no_song_falls_back_to_unity_volume_zero_pan() {
        let mut vol = vec![0.0_f32; 8];
        let mut pan = vec![0.5_f32; 8];
        let empty = empty_recording_lanes();
        fill_track_param_ramps(None, 0, SR, 120.0, 0, 8, &mut vol, &mut pan, &empty);
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
        let empty = empty_recording_lanes();
        fill_track_param_ramps(
            Some(&song),
            0,
            SR,
            120.0,
            0,
            16,
            &mut vol,
            &mut pan,
            &empty,
        );
        assert!(vol.iter().all(|&v| (v - 0.7).abs() < 1e-6));
        assert!(pan.iter().all(|&p| (p - -0.25).abs() < 1e-6));
    }

    #[test]
    fn volume_lane_ramps_across_buffer() {
        let song = one_volume_lane_song();
        let mut vol = vec![0.0_f32; 16];
        let mut pan = vec![0.0_f32; 16];
        let empty = empty_recording_lanes();
        // Buffer of 16 samples starting at playhead 0 → first 16 samples
        // out of 24000 samples-per-beat × 4 = 96000 total. Volume should
        // ramp from 0.0 toward ~16/96000 ≈ 0.000167.
        fill_track_param_ramps(
            Some(&song),
            0,
            SR,
            120.0,
            0,
            16,
            &mut vol,
            &mut pan,
            &empty,
        );
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
        let empty = empty_recording_lanes();
        // Beat 2.0 = 48000 samples in. Curve linear 0→1 over beats 0..4
        // → value 0.5 at beat 2.
        fill_track_param_ramps(
            Some(&song),
            0,
            SR,
            120.0,
            48_000,
            4,
            &mut vol,
            &mut pan,
            &empty,
        );
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
        let empty = empty_recording_lanes();
        fill_track_param_ramps(
            Some(&song),
            0,
            SR,
            120.0,
            0,
            4,
            &mut vol,
            &mut pan,
            &empty,
        );
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
        let empty = empty_recording_lanes();
        fill_track_param_ramps(
            Some(&song),
            0,
            SR,
            120.0,
            240_000,
            4,
            &mut vol,
            &mut pan,
            &empty,
        );
        for &v in vol.iter() {
            assert!((v - 0.5).abs() < 1e-6, "got {}", v);
        }
    }

    /// Phase 4 Step C-2: `recording_lanes` に含まれる lane は curve eval を
    /// skip し、 track.volume の constant が残るべき (= live knob 値で audio
    /// が鳴る挙動の基盤)。 track.volume を 0.9、 curve は beat 2.0 で 0.5 に
    /// なる構成にし、 bypass で vol[i] が 0.9 になることを確認する。
    #[test]
    fn recording_lane_bypasses_curve_eval() {
        let mut song = one_volume_lane_song();
        song.tracks[0].volume = 0.9; // 区別のため curve mid (0.5) と違う値に
        let track_id = song.tracks[0].id;
        let mut vol = vec![0.0_f32; 4];
        let mut pan = vec![0.0_f32; 4];
        let mut recording = std::collections::HashSet::new();
        recording.insert((
            track_id,
            common::model::AutomationTarget::TrackBuiltin(
                common::model::TrackBuiltinParam::Volume,
            ),
        ));
        // Beat 2.0 の curve eval は 0.5 だが、 recording bypass で track.volume
        // (= 0.9) がそのまま残る。
        fill_track_param_ramps(
            Some(&song),
            0,
            SR,
            120.0,
            48_000,
            4,
            &mut vol,
            &mut pan,
            &recording,
        );
        for &v in vol.iter() {
            assert!(
                (v - 0.9).abs() < 1e-6,
                "expected track.volume bypass (0.9), got {}",
                v
            );
        }
    }

    /// Phase 4 Step C-2: recording set にない lane は通常通り curve eval する。
    /// recording bypass test と pair の sanity check (= bypass が必要なときだけ
    /// 効いて、 不要なときは無回帰)。
    #[test]
    fn non_recording_lane_still_uses_curve() {
        let mut song = one_volume_lane_song();
        song.tracks[0].volume = 0.9;
        let mut vol = vec![0.0_f32; 4];
        let mut pan = vec![0.0_f32; 4];
        let empty = empty_recording_lanes();
        // Beat 2.0 の curve eval は 0.5、 bypass されないので vol[i] = 0.5。
        fill_track_param_ramps(
            Some(&song),
            0,
            SR,
            120.0,
            48_000,
            4,
            &mut vol,
            &mut pan,
            &empty,
        );
        for &v in vol.iter() {
            assert!((v - 0.5).abs() < 0.001, "expected curve eval (0.5), got {}", v);
        }
    }

    /// One track (id 7) with a single `PluginParam` (Instrument, param 5)
    /// automation lane ramping 0.25 → 0.75 over beats 0..4.
    fn one_plugin_param_lane_song() -> Song {
        let mut song = Song {
            bpm: 120.0,
            ..Song::default()
        };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint { time_beat: 0.0, value: 0.25, curve: AutomationCurve::Linear },
                    AutomationPoint { time_beat: 4.0, value: 0.75, curve: AutomationCurve::Linear },
                ],
            }),
        );
        let target = AutomationTarget::PluginParam { slot: PluginSlot::Instrument, param_id: 5 };
        let lane = AutomationLane {
            id: 1,
            clips: vec![AutomationClip {
                id: 1,
                name: "p".into(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
            }],
            next_clip_id: 2,
            ..AutomationLane::new(target, 0.5)
        };
        song.tracks.push(Track {
            id: 7,
            name: "T".into(),
            automation_lanes: vec![lane],
            next_lane_id: 2,
            ..Track::default()
        });
        song
    }

    /// Phase 4 Step C-2 (plugin param 版) 回帰: recording 中 (Touch/Latch/Write)
    /// の plugin-param lane は curve eval を skip し、 plugin が GUI で持つ値を
    /// host が上書きしない。 旧実装は skip が無く write が read のままだった。
    #[test]
    fn fill_pd_param_events_skips_recording_lanes() {
        let song = one_plugin_param_lane_song();
        let track_id = 7;
        let target = AutomationTarget::PluginParam { slot: PluginSlot::Instrument, param_id: 5 };

        // Not recording: the curve value is pushed as a ParamValue event (read).
        let mut pd = ProcessData::empty();
        let empty = empty_recording_lanes();
        fill_pd_param_events(
            &mut pd, &song, track_id, PluginSlot::Instrument, SR, 120.0, 0, 64, &empty,
        );
        assert_eq!(pd.n_events_in, 1, "read mode must push the curve value");

        // Recording: the lane is skipped, so no curve event overwrites the
        // plugin's live GUI value.
        let mut pd2 = ProcessData::empty();
        let mut rec = std::collections::HashSet::new();
        rec.insert((track_id, target));
        fill_pd_param_events(
            &mut pd2, &song, track_id, PluginSlot::Instrument, SR, 120.0, 0, 64, &rec,
        );
        assert_eq!(pd2.n_events_in, 0, "recording lane curve must be suppressed");
    }
}
