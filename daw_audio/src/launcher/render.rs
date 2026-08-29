//! 行の供給元を、実際の 3 経路 (MIDI / オーディオクリップ / オートメーション) へ
//! 配る層 (`docs/plan_rmd_87_clip_launcher.md` §2.1)。
//!
//! **ループ端を跨ぐ buffer の分割はここだけが持つ。** `sequencer` /
//! `audio_clip_renderer` / `automation` の本体は「1 つの実効拍で 1 区間を描く」まま
//! 触らない — あちらに分岐を足すと `scripts/arch_lint_baseline.txt` の
//! FN-NESTING 天井 (どれも余裕ゼロ) を即座に超える。
//!
//! RT 安全: [`super::for_each_segment`] は確保なし・有界 (区間は必ず 1 frame 以上
//! 進むので反復は高々 `frames` 回)、この層も出力スライスを切り出すだけ。

use std::collections::HashMap;

use common::model::{AutomationLane, Clip, ClipContent, ContentId, SessionClip, Song};

use super::{RowPhase, RowTimeSource, for_each_segment};
use crate::audio_clip_renderer::{AudioClipRenderer, ClipRenderState, render_audio_events};
use crate::sequencer::{TimedNoteEvent, collect_events_for_buffer};

/// この行の MIDI イベントを 1 buffer 分集める。
///
/// アレンジ区間は `track.clips` を、ランチャー区間は **そのセル 1 つ**を源にする。
/// セルの `clip.start_beat` は 0 なので、区間の実効拍をそのまま渡せば既存の
/// クリップ窓の算術 (`content_window` / `content_to_song_beat`) がそのまま通る。
#[allow(clippy::too_many_arguments)]
pub fn collect_row_midi(
    song: Option<&Song>,
    track_idx: u32,
    src: RowTimeSource,
    sample_rate: u32,
    playhead_beats: f64,
    current_bpm: f32,
    frames: u32,
    out: &mut Vec<TimedNoteEvent>,
    active_notes: &mut Vec<u8>,
) {
    let Some(song) = song else { return };
    let Some(track) = song.tracks.get(track_idx as usize) else { return };
    let bpf = beats_per_frame(current_bpm, sample_rate);
    for_each_segment(src, playhead_beats, bpf, frames, |seg| {
        let clips = match seg.cell_clip_id {
            0 => track.clips.as_slice(),
            id => match cell_clip(&track.session_clips, id) {
                Some(c) => std::slice::from_ref(c),
                None => return,
            },
        };
        collect_events_for_buffer(
            Some(song),
            track_idx,
            clips,
            sample_rate,
            seg.beat,
            current_bpm,
            seg.frames(),
            seg.start_frame,
            out,
            active_notes,
        );
    });
}

/// この行のオーディオクリップを 1 buffer 分描く。
///
/// `render_audio_events` は出力へ **加算**するので、区間ごとに部分スライスを
/// 渡してよい (ゼロ化は呼び出し側が buffer 全体に 1 回だけ行う)。
#[allow(clippy::too_many_arguments)]
pub fn render_row_audio(
    renderer: &AudioClipRenderer,
    track_idx: usize,
    src: RowTimeSource,
    track_l: &mut [f32],
    track_r: &mut [f32],
    playhead_beats: f64,
    current_bpm: f32,
    sample_rate: u32,
    frames: u32,
    state: &mut ClipRenderState<'_>,
) {
    let bpf = beats_per_frame(current_bpm, sample_rate);
    let n = frames as usize;
    for_each_segment(src, playhead_beats, bpf, frames, |seg| {
        let (a, b) = (seg.start_frame as usize, (seg.end_frame as usize).min(n));
        if a >= b || b > track_l.len() || b > track_r.len() {
            return;
        }
        #[allow(clippy::cast_possible_truncation)]
        let seg_frames = (b - a) as u32;
        render_audio_events(
            renderer,
            track_idx,
            seg.cell_clip_id,
            &mut track_l[a..b],
            &mut track_r[a..b],
            seg.beat,
            current_bpm,
            sample_rate,
            seg_frames,
            state,
        );
    });
}

/// オートメーションレーン行の値。
///
/// - `Arranger`: 従来どおり `lane.clips` を song 拍で引く
/// - `Silent`  : レーン既定値 (Q11 — セルの無い列を撃つとここへ戻る)
/// - `Cell`    : そのセルの [`common::model::AutomationClip`] を実効拍で引く
#[must_use]
pub fn lane_value(
    lane: &AutomationLane,
    clip_contents: &HashMap<ContentId, ClipContent>,
    phase: RowPhase,
    beat: f64,
) -> f64 {
    if !lane.enabled {
        return lane.default_value;
    }
    match phase {
        RowPhase::Arranger => common::automation::lane_value_at(lane, clip_contents, beat),
        RowPhase::Silent => lane.default_value,
        RowPhase::Cell { clip_id, .. } => {
            let Some(eff) = phase.effective_beat(beat) else {
                return lane.default_value;
            };
            cell_value(lane, clip_contents, clip_id, eff)
        }
    }
}

/// 1 buffer の `frame` 番目に効いている供給元 (切り替えの前後で分かれる)。
#[must_use]
pub fn phase_at_frame(src: RowTimeSource, frame: u32) -> RowPhase {
    if frame < src.switch_frame { src.head } else { src.tail }
}

/// セル 1 つを実効拍で評価する。窓の外はレーン既定値。
fn cell_value(
    lane: &AutomationLane,
    clip_contents: &HashMap<ContentId, ClipContent>,
    clip_id: u32,
    eff: f64,
) -> f64 {
    let Some(cell) = lane.session_clips.iter().find(|c| c.clip.id == clip_id) else {
        return lane.default_value;
    };
    let clip = &cell.clip;
    if clip.length_beats <= 0.0 || eff < clip.start_beat || eff >= clip.start_beat + clip.length_beats
    {
        return lane.default_value;
    }
    let Some(ClipContent::Automation(auto)) = clip_contents.get(&clip.content_id) else {
        return lane.default_value;
    };
    if auto.points.is_empty() {
        return lane.default_value;
    }
    // r.md #44: カーブ上の位置は clip 開始ではなく **content 原点**基準。
    common::automation::evaluate_clip(auto, clip.song_to_content_beat(eff))
}

/// トラック行のセルの中身 (`clip.id` で引く)。
fn cell_clip(cells: &[SessionClip], clip_id: u32) -> Option<&Clip> {
    cells.iter().find(|c| c.clip.id == clip_id).map(|c| &c.clip)
}

fn beats_per_frame(current_bpm: f32, sample_rate: u32) -> f64 {
    if sample_rate == 0 || current_bpm <= 0.0 {
        return 0.0;
    }
    f64::from(current_bpm) / (60.0 * f64::from(sample_rate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launcher::{RowKey, RowTimeSource};
    use common::model::{
        AutomationClip, AutomationContent, AutomationCurve, AutomationPoint, AutomationTarget,
        LaunchSettings, MidiContent, Note, SessionAutomationClip, Track, TrackBuiltinParam,
    };

    /// 4 拍のセル 1 つを持つ track 1 本。セルの content には拍 0 と 2 に note。
    fn song_with_cell() -> Song {
        let mut song = Song { bpm: 120.0, ..Song::default() };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Midi(MidiContent {
                notes: vec![
                    Note { id: 1, start_beat: 0.0, duration_beats: 0.5, pitch: 60, ..Note::default() },
                    Note { id: 2, start_beat: 2.0, duration_beats: 0.5, pitch: 64, ..Note::default() },
                ],
                next_note_id: 3,
            }),
        );
        let mut track = Track { id: 1, next_clip_id: 10, ..Track::default() };
        track.session_clips.push(SessionClip {
            scene_id: 1,
            clip: Clip {
                id: 5,
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                ..Clip::default()
            },
            launch: LaunchSettings::default(),
        });
        song.tracks.push(track);
        song.push_scene();
        song.tracks[0].session_clips[0].scene_id = song.scenes[0].id;
        song
    }

    fn cell_phase(launch_beat: f64) -> RowPhase {
        RowPhase::Cell {
            clip_id: 5,
            launch_beat,
            loop_len: 4.0,
            cell_start_beat: 0.0,
            looping: true,
        }
    }

    /// ループ端を跨ぐ buffer で、セル頭の note がその buffer の **途中で** 鳴ること。
    /// (「巻き戻したのに note が出ない」= 分割していない、を検出する)
    #[test]
    fn ループを跨ぐ_buffer_でセル頭の_note_が出る() {
        let song = song_with_cell();
        // 120 BPM / 48 kHz → 1 拍 = 24,000 sample。512 frame = 0.02133 拍。
        // playhead 3.99 の buffer は 3.99..4.0113 で、4 拍セルのループ端 (4.0) を跨ぐ。
        let src = RowTimeSource::uniform(RowKey::track(1), cell_phase(0.0));
        let mut out = Vec::with_capacity(256);
        let mut active = Vec::with_capacity(256);
        collect_row_midi(Some(&song), 0, src, 48_000, 3.99, 120.0, 512, &mut out, &mut active);

        let ons: Vec<u32> = out
            .iter()
            .filter(|e| matches!(e.event, crate::sequencer::NoteTransition::On { key: 60, .. }))
            .map(|e| e.time)
            .collect();
        assert_eq!(ons.len(), 1, "セル頭の note が 1 回だけ鳴る: {out:?}");
        // 拍 4.0 = buffer 先頭から 0.01 拍 = 240 sample。
        assert!((239..=241).contains(&ons[0]), "巻き戻し位置がずれている: {}", ons[0]);
        // 拍 2.0 の note (pitch 64) はこの窓に入らない。
        assert!(
            !out.iter().any(|e| matches!(
                e.event,
                crate::sequencer::NoteTransition::On { key: 64, .. }
            )),
            "窓の外の note が鳴った: {out:?}"
        );
    }

    /// アレンジ行はセルを 1 つも見ない (= 供給元の切り替えが効いている)。
    #[test]
    fn アレンジ行はセルの_note_を出さない() {
        let song = song_with_cell();
        let src = RowTimeSource::uniform(RowKey::track(1), RowPhase::Arranger);
        let mut out = Vec::with_capacity(256);
        let mut active = Vec::with_capacity(256);
        collect_row_midi(Some(&song), 0, src, 48_000, 0.0, 120.0, 512, &mut out, &mut active);
        assert!(out.is_empty(), "アレンジには clip が無いのに鳴った: {out:?}");
    }

    #[test]
    fn 停止した行は何も鳴らさない() {
        let song = song_with_cell();
        let src = RowTimeSource::uniform(RowKey::track(1), RowPhase::Silent);
        let mut out = Vec::with_capacity(256);
        let mut active = Vec::with_capacity(256);
        collect_row_midi(Some(&song), 0, src, 48_000, 0.0, 120.0, 512, &mut out, &mut active);
        assert!(out.is_empty());
    }

    /// レーン行: セルが鳴っていればそのカーブ、停止していれば既定値。
    #[test]
    fn レーン行はセルのカーブと既定値を切り替える() {
        let mut song = Song { bpm: 120.0, ..Song::default() };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                next_point_id: 3,
                points: vec![
                    AutomationPoint { id: 1, time_beat: 0.0, value: 0.0, curve: AutomationCurve::Linear },
                    AutomationPoint { id: 2, time_beat: 4.0, value: 1.0, curve: AutomationCurve::Linear },
                ],
            }),
        );
        let mut lane = AutomationLane::new(
            AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
            0.42,
        );
        lane.id = 1;
        lane.session_clips.push(SessionAutomationClip {
            scene_id: 1,
            clip: AutomationClip {
                id: 3,
                name: String::new(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                content_offset_beats: 0.0,
            },
            launch: LaunchSettings::default(),
        });
        song.tracks.push(Track { id: 1, automation_lanes: vec![lane], ..Track::default() });
        let lane = &song.tracks[0].automation_lanes[0];

        let phase = RowPhase::Cell {
            clip_id: 3,
            launch_beat: 8.0,
            loop_len: 4.0,
            cell_start_beat: 0.0,
            looping: true,
        };
        // 撃ってから 2 拍 = カーブの中央 = 0.5。
        let v = lane_value(lane, &song.clip_contents, phase, 10.0);
        assert!((v - 0.5).abs() < 1e-9, "{v}");
        // ループして 1 周した先も同じ (位相が戻る)。
        let v2 = lane_value(lane, &song.clip_contents, phase, 14.0);
        assert!((v2 - 0.5).abs() < 1e-9, "{v2}");
        // 停止した行は既定値。
        assert_eq!(lane_value(lane, &song.clip_contents, RowPhase::Silent, 10.0), 0.42);
        // アレンジ行は arrangement 側の clip (無い) → 既定値。
        assert_eq!(lane_value(lane, &song.clip_contents, RowPhase::Arranger, 10.0), 0.42);
    }

    /// Q9 / §2.5 の前提を **描いた結果**で確かめる: 同じプロジェクトを同じ起点から
    /// 2 回走らせたら、出てくる MIDI イベント列が完全に一致すること。
    ///
    /// 走らせるのは `LauncherRuntime`(予約 / フォローアクションの抽選) →
    /// 区間分割 → `collect_events_for_buffer` の **書き出しが通るのと同じ経路**。
    /// 抽選が走行状態や壁時計に依存していたらここで落ちる。
    #[test]
    fn 同じ起点から二度描くと同じイベント列になる() {
        use crate::launcher::runtime::{BufferSpan, LauncherRuntime};
        use common::model::{FollowAction, FollowActionKind, LaunchQuantize, RowPlayback};

        let mut song = song_with_cell();
        // 2 つ目の列とセルを足し、両方に「50% で Any / 50% で Next」を付ける。
        let scene2 = song.push_scene();
        let cid = song.tracks[0].session_clips[0].clip.content_id;
        song.tracks[0].session_clips.push(SessionClip {
            scene_id: scene2,
            clip: Clip {
                id: 6,
                start_beat: 0.0,
                length_beats: 2.0,
                content_id: cid,
                ..Clip::default()
            },
            launch: LaunchSettings::default(),
        });
        for c in &mut song.tracks[0].session_clips {
            c.launch.follow = FollowAction {
                enabled: true,
                a: FollowActionKind::Any,
                b: FollowActionKind::Next,
                chance_a: 50,
                ..FollowAction::default()
            };
        }
        song.tracks[0].launcher = RowPlayback::Launcher { clip_id: 5 };
        let _ = scene2;

        let render_once = || {
            let mut rt = LauncherRuntime::new();
            let mut trace: Vec<(usize, u32, u8, bool)> = Vec::new();
            let mut out = Vec::with_capacity(256);
            let mut active = Vec::with_capacity(256);
            let mut beat = 0.0_f64;
            for buf in 0..400usize {
                let span = BufferSpan::new(beat, 120.0, 48_000, 512);
                rt.update(&song, span, LaunchQuantize::Off, true);
                out.clear();
                active.clear();
                collect_row_midi(
                    Some(&song),
                    0,
                    rt.rows().track_row(0),
                    48_000,
                    beat,
                    120.0,
                    512,
                    &mut out,
                    &mut active,
                );
                for e in &out {
                    let (key, on) = match e.event {
                        crate::sequencer::NoteTransition::On { key, .. } => (key, true),
                        crate::sequencer::NoteTransition::Off { key, .. } => (key, false),
                    };
                    trace.push((buf, e.time, key, on));
                }
                beat += 512.0 * 120.0 / (60.0 * 48_000.0);
            }
            trace
        };

        let a = render_once();
        let b = render_once();
        assert_eq!(a, b, "同じ起点から 2 回描いて違う = 書き出しが再現しない");
        assert!(!a.is_empty(), "1 音も鳴っていない (テストが何も検証していない)");
    }
}
