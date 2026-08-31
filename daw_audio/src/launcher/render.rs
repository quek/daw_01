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
    // 直前の区間 (`(実効拍, セル id)`)。区間の切れ目で鳴っている note を止めるのに使う。
    //
    // **`src.head` の状態で初期化する。** `switch_frame == 0` (= 発火拍が buffer 先頭
    // ちょうど。量子化 Off や、境界が frame 0 に落ちたとき) だと head の区間が
    // 1 つも出ない (`emit_phase` の `from >= to`) ので、`None` 始まりだと tail の
    // 最初の区間で切れ目 flush が走らず、head で鳴っていた note の Off が
    // **二度と出ない** (次の停止 / seek / ループ巻き戻しまで鳴りっぱなし)。
    // `switch_frame > 0` のときは head の最初の区間がこれと同じ値を書くので、
    // 余計な flush は起きない (比較は厳密な `<` / `!=`)。
    let mut prev: Option<(f64, u32)> = src
        .head
        .effective_beat(playhead_beats)
        .map(|b| (b, src.head.cell_clip_id().unwrap_or(0)));
    // 区間が届いた最後の frame。ここが `frames` に届かない = **その先は無音**。
    let mut covered_to: u32 = 0;
    for_each_segment(src, playhead_beats, bpf, frames, |seg| {
        // **区間の切れ目では鳴っている note を必ず止める。**
        // ループで巻き戻る / 別のセルへ乗り換える瞬間は、次の区間の窓にその note の
        // Off が入らない (セル末尾ぴったりの note は「窓の右端」= 排他境界に居るため
        // 二度と出ない)。止めないと鳴りっぱなしになり、次の周の On も同じ音として
        // 潰れて **「1 小節しか鳴らない / セル全長の note が鳴らない」** になる。
        // アレンジのループ端で `queue_all_notes_off` がやっているのと同じ始末。
        if let Some((prev_beat, prev_cell)) = prev
            && (seg.beat < prev_beat || seg.cell_clip_id != prev_cell)
        {
            flush_active(out, active_notes, seg.start_frame);
        }
        prev = Some((seg.beat, seg.cell_clip_id));
        covered_to = seg.end_frame.min(frames);
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
    // **無音へ落ちる buffer では区間が 1 つも出ない** (行の停止 / 空セルのシーン
    // 発火 / ワンショットの終端 / フォローアクションの Stop / セルが消えた)。
    // 区間が来ないと上の切れ目 flush も走らないので、鳴っている note の Off が
    // 二度と出ず**鳴りっぱなし**になる。届かなかった frame から止める。
    if covered_to < frames {
        flush_active(out, active_notes, covered_to);
    }
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

/// 鳴っている note を `at` frame で全部止める (区間の切れ目の始末)。
///
/// `note_id` は 0 (= 未指定) — builtin / プラグインとも key 一致で voice を止める
/// (`process_track_owned` の `pending_offs` と同じ約束)。
fn flush_active(out: &mut Vec<TimedNoteEvent>, active_notes: &mut Vec<u8>, at: u32) {
    for &key in active_notes.iter() {
        out.push(TimedNoteEvent {
            time: at,
            event: crate::sequencer::NoteTransition::Off { note_id: 0, key },
        });
    }
    active_notes.clear();
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

/// 行の供給元を、プロセス境界を渡る形 ([`common::process_data::RowTransport`]) へ写す。
///
/// **写像だけで、位相の式は書かない** — 実効拍は [`RowPhase::effective_beat`] が
/// 唯一の定義。値は buffer 頭のもの (plugin へ渡す transport は 1 buffer 1 値で、
/// アレンジのループ wrap も buffer 境界でしか起きない = 同じ粒度)。
///
/// 無音の行 (`Silent` / ワンショットの終端) は `silent = 1` を立てつつ拍は
/// **song のもの**を残す。行が黙ってもエフェクトのテンポ同期は走り続けるべきで、
/// 拍を 0 に落とすとディレイのグリッドが飛ぶ。
#[must_use]
pub fn row_transport(
    src: RowTimeSource,
    playhead_beats: f64,
) -> common::process_data::RowTransport {
    use common::process_data::{ARRANGER_CELL_ID, RowTransport};

    let phase = phase_at_frame(src, 0);
    let Some(pos_beats) = phase.effective_beat(playhead_beats) else {
        // 無音の行。拍は song のものを残し、ループも曲のもの (= 呼び側が
        // `pd.loop_*` を使う) に任せる。
        return RowTransport {
            pos_beats: playhead_beats,
            cell_clip_id: ARRANGER_CELL_ID,
            silent: 1,
            ..RowTransport::default()
        };
    };
    // ループするセルだけが自前のループ区間を持つ。ワンショットとアレンジは
    // `0.0 / 0.0` (= 自前のループ無し)。
    let (loop_start_beats, loop_end_beats) = match phase {
        RowPhase::Cell { cell_start_beat, loop_len, looping: true, .. }
            if super::is_positive(loop_len) =>
        {
            (cell_start_beat, cell_start_beat + loop_len)
        }
        _ => (0.0, 0.0),
    };
    RowTransport {
        pos_beats,
        loop_start_beats,
        loop_end_beats,
        cell_clip_id: phase.cell_clip_id().unwrap_or(ARRANGER_CELL_ID),
        silent: 0,
        _pad: [0; 3],
    }
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

    /// r.md #87: 行の供給元 → `ProcessData::row` の写像。plugin host (VOICEVOX の
    /// 連続再生 / CLAP・VST3 の transport) はこの 3 状態しか見ないので、ここが
    /// 崩れると「合成済みなのにセルが鳴らない」「停止した行のアレンジが鳴り止まない」
    /// という静かな壊れ方をする。
    #[test]
    fn 行の供給元は拍とセル_id_と無音フラグへ写る() {
        let key = RowKey::track(1);
        // アレンジ: song 拍そのまま / セル無し / 自前のループ無し。
        let r = row_transport(RowTimeSource::uniform(key, RowPhase::Arranger), 12.5);
        assert_eq!(r.pos_beats, 12.5);
        assert!(r.is_arrangement() && !r.is_silent() && !r.has_loop());

        // 4 拍のループするセルを拍 8 で撃ち、いま拍 10.5 → セル内 2.5 拍。
        let cell = RowPhase::Cell {
            clip_id: 7,
            launch_beat: 8.0,
            loop_len: 4.0,
            cell_start_beat: 0.0,
            looping: true,
        };
        let r = row_transport(RowTimeSource::uniform(key, cell), 10.5);
        assert!((r.pos_beats - 2.5).abs() < 1e-9, "{}", r.pos_beats);
        assert_eq!(r.cell_clip_id, 7);
        assert!(!r.is_silent());
        // ループ区間は**セルの窓**であって曲のループではない。
        assert_eq!((r.loop_start_beats, r.loop_end_beats), (0.0, 4.0));

        // ワンショットは自前のループを名乗らない。
        let one_shot = RowPhase::Cell {
            clip_id: 7,
            launch_beat: 8.0,
            loop_len: 4.0,
            cell_start_beat: 0.0,
            looping: false,
        };
        let r = row_transport(RowTimeSource::uniform(key, one_shot), 10.5);
        assert!(!r.has_loop());
        // 終端を越えたら無音 (拍は song のまま = エフェクトのグリッドを飛ばさない)。
        let r = row_transport(RowTimeSource::uniform(key, one_shot), 13.0);
        assert!(r.is_silent() && r.is_arrangement());
        assert_eq!(r.pos_beats, 13.0);

        // Stop Clips / 空セルのシーン。
        let r = row_transport(RowTimeSource::uniform(key, RowPhase::Silent), 4.0);
        assert!(r.is_silent());
    }

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

    /// **セルいっぱいの長さの note** (4 拍セルに 0..4 の note) が鳴り、
    /// ループ端でちゃんと Off → On が出る。
    #[test]
    fn セル全長の_note_が鳴りループ端で撃ち直される() {
        let mut song = song_with_cell();
        let cid = song.tracks[0].session_clips[0].clip.content_id;
        song.clip_contents.insert(
            cid,
            ClipContent::Midi(MidiContent {
                notes: vec![Note {
                    id: 1,
                    start_beat: 0.0,
                    duration_beats: 4.0,
                    pitch: 60,
                    ..Note::default()
                }],
                next_note_id: 2,
            }),
        );
        let src = RowTimeSource::uniform(RowKey::track(1), cell_phase(0.0));
        // 撃った直後の buffer: On が出る。
        let mut out = Vec::with_capacity(256);
        let mut active = Vec::with_capacity(256);
        collect_row_midi(Some(&song), 0, src, 48_000, 0.0, 120.0, 512, &mut out, &mut active);
        assert!(
            out.iter().any(|e| matches!(
                e.event,
                crate::sequencer::NoteTransition::On { key: 60, .. }
            )),
            "セル全長の note が鳴らない: {out:?}"
        );
        // ループ端を跨ぐ buffer: Off と次の周の On が両方出る。
        let mut out = Vec::with_capacity(256);
        collect_row_midi(Some(&song), 0, src, 48_000, 3.99, 120.0, 512, &mut out, &mut active);
        assert!(
            out.iter().any(|e| matches!(
                e.event,
                crate::sequencer::NoteTransition::Off { key: 60, .. }
            )),
            "ループ端で Off が出ない (= 鳴りっぱなし): {out:?}"
        );
        assert!(
            out.iter().any(|e| matches!(
                e.event,
                crate::sequencer::NoteTransition::On { key: 60, .. }
            )),
            "次の周の On が出ない: {out:?}"
        );
    }

    /// **frame グリッドに乗らない発火拍でも、セル頭の note が毎周鳴る。**
    /// 区間の開始拍は frame 境界の切り上げのぶん原点をわずかに越えるので、
    /// 吸着 (`snap_to_cell_origin`) が無いと content 拍 0 の note が
    /// 「撃った瞬間」も「2 周目以降」も丸ごと落ちる (キックが消える)。
    #[test]
    fn 発火拍が_frame_境界に乗らなくてもセル頭の_note_が毎周鳴る() {
        let song = song_with_cell();
        // 4 拍のセルを **中途半端な拍**で撃つ。
        let launch = 1.234_567;
        let src = RowTimeSource::uniform(RowKey::track(1), {
            RowPhase::Cell {
                clip_id: 5,
                launch_beat: launch,
                loop_len: 4.0,
                cell_start_beat: 0.0,
                looping: true,
            }
        });
        // 120 BPM / 48 kHz / 512 frame = 0.0213333… 拍。
        let bpf = 512.0 * 120.0 / (60.0 * 48_000.0);
        let mut active = Vec::with_capacity(256);
        let mut ons = 0usize;
        let mut beat = launch;
        // 3 周ぶん (原点は 0 / 4 / 8 拍の 3 回) 刻む。11 拍で止めるのは、
        // 12 拍ちょうどまで回すと 4 周目の原点も窓に入るから。
        while beat < launch + 11.0 {
            let mut out = Vec::with_capacity(64);
            collect_row_midi(
                Some(&song), 0, src, 48_000, beat, 120.0, 512, &mut out, &mut active,
            );
            ons += out
                .iter()
                .filter(|e| {
                    matches!(e.event, crate::sequencer::NoteTransition::On { key: 60, .. })
                })
                .count();
            beat += bpf;
        }
        assert_eq!(ons, 3, "3 周ぶん刻んだらセル頭の note は 3 回鳴る");
    }

    /// **供給元が buffer 先頭ちょうどで切り替わっても note-off が出る。**
    ///
    /// `switch_frame == 0` だと head の区間が 1 つも出ない (`emit_phase` の
    /// `from >= to`) ので、切れ目 flush の判定を「1 つ前の区間」だけに頼ると
    /// head で鳴っていた note の Off が二度と出ない (= 停止 / seek / ループ
    /// 巻き戻しまで鳴りっぱなし)。量子化 Off で行を撃つ / アレンジへ返すと、
    /// 発火拍は必ず buffer 先頭になるのでこの経路が常用される。
    #[test]
    fn buffer_先頭での供給元切替でも_note_off_が出る() {
        let mut song = song_with_cell();
        let cid = song.tracks[0].session_clips[0].clip.content_id;
        // セル全長 (4 拍) の note = buffer を跨いで鳴り続ける音。
        song.clip_contents.insert(
            cid,
            ClipContent::Midi(MidiContent {
                notes: vec![Note {
                    id: 1,
                    start_beat: 0.0,
                    duration_beats: 4.0,
                    pitch: 60,
                    ..Note::default()
                }],
                next_note_id: 2,
            }),
        );
        let mut out = Vec::with_capacity(256);
        let mut active = Vec::with_capacity(256);

        // 1 buffer 目: セルが鳴り出す。
        let playing = RowTimeSource::uniform(RowKey::track(1), cell_phase(0.0));
        collect_row_midi(Some(&song), 0, playing, 48_000, 0.0, 120.0, 512, &mut out, &mut active);
        assert_eq!(active.as_slice(), &[60], "セルの note が鳴っていない: {out:?}");

        // 2 buffer 目: 先頭ちょうどでアレンジへ返す (`switch_frame == 0`)。
        out.clear();
        let switch = RowTimeSource {
            key: RowKey::track(1),
            head: cell_phase(0.0),
            tail: RowPhase::Arranger,
            switch_frame: 0,
        };
        collect_row_midi(Some(&song), 0, switch, 48_000, 2.0, 120.0, 512, &mut out, &mut active);
        let offs: Vec<u32> = out
            .iter()
            .filter(|e| {
                matches!(e.event, crate::sequencer::NoteTransition::Off { key: 60, .. })
            })
            .map(|e| e.time)
            .collect();
        assert_eq!(offs.as_slice(), &[0], "切り替えの瞬間に Off が出ない: {out:?}");
        assert!(active.is_empty(), "鳴っている note が残った: {active:?}");
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

/// RT 無確保検査 (`make test-rt`) 用の Song ビルダー。
#[cfg(all(test, feature = "rt-assert"))]
mod tests_support {
    use common::model::{
        Clip, FollowAction, FollowActionKind, LaunchSettings, MidiContent, Note, SessionClip,
        Track,
    };

    use super::{ClipContent, Song};

    /// 1 トラック / 2 列。両セルに「クリップ終端で次の列へ」を付けてあるので、
    /// 走らせるだけでフォローアクションの遷移とループ端を何度も通る。
    #[must_use]
    pub fn rt_song() -> Song {
        let mut song = Song { bpm: 120.0, project_id: 1, ..Song::default() };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Midi(MidiContent {
                notes: vec![
                    Note { id: 1, start_beat: 0.0, duration_beats: 2.0, pitch: 60, ..Note::default() },
                    Note { id: 2, start_beat: 2.0, duration_beats: 2.0, pitch: 64, ..Note::default() },
                ],
                next_note_id: 3,
            }),
        );
        let s1 = song.push_scene();
        let s2 = song.push_scene();
        let mut track = Track { id: 1, next_clip_id: 100, ..Track::default() };
        for (id, scene_id, len) in [(10u32, s1, 4.0), (11, s2, 2.0)] {
            track.session_clips.push(SessionClip {
                scene_id,
                clip: Clip {
                    id,
                    start_beat: 0.0,
                    length_beats: len,
                    content_id: cid,
                    ..Clip::default()
                },
                launch: LaunchSettings {
                    follow: FollowAction {
                        enabled: true,
                        a: FollowActionKind::Next,
                        chance_a: 100,
                        ..FollowAction::default()
                    },
                    ..LaunchSettings::default()
                },
            });
        }
        song.tracks.push(track);
        song
    }
}

/// **ランチャーの RT 経路が 1 バイトも確保しないこと** (`make test-rt`)。
///
/// `launcher/mod.rs` の RT 規約はこの検査があって初めて主張になる。包むのは
/// audio callback がこの順で通る 4 本すべて — 走行状態の解決 ([`LauncherRuntime::update`])、
/// GUI への publish、MIDI の収集、オーディオの描画。
#[cfg(all(test, feature = "rt-assert"))]
mod rt_assert_tests {
    use super::tests_support::*;
    use super::*;
    use crate::audio_clip_renderer::{AudioClipRenderer, ClipRenderState};
    use crate::launcher::runtime::{BufferSpan, LaunchRequest, LauncherRuntime};
    use crate::launcher::{RowKey, RowSourceTable};
    use common::model::LaunchQuantize;

    #[test]
    fn ランチャーの_rt_経路は確保しない() {
        let song = rt_song();
        let mut rt = LauncherRuntime::new();
        // 事前確保は off-RT (live では publish 側 / export では walk の頭)。
        let mut out = Vec::with_capacity(4096);
        let mut active = Vec::with_capacity(1024);
        let renderer = AudioClipRenderer::empty();
        let mut accum: Vec<(u64, f64)> = Vec::with_capacity(8);
        let mut engines = Vec::new();
        let mut event_l = vec![0.0f32; common::process_data::MAX_FRAMES];
        let mut event_r = vec![0.0f32; common::process_data::MAX_FRAMES];
        let mut l = vec![0.0f32; 512];
        let mut r = vec![0.0f32; 512];
        let mut render_seq = 0u64;
        let name = format!("daw01-rt-launcher-{}", std::process::id());
        let bridge = common::audio_bridge::AudioBridgeHandle::create(&name).expect("bridge");

        // 1 buffer 目は行の生成 (`Vec::push`) を含むので検査の外で回す。
        rt.update(&song, BufferSpan::new(0.0, 120.0, 48_000, 512), LaunchQuantize::Off, true);

        // 以降が定常。セル発火 / シーン発火 / フォローアクションの遷移 /
        // ループ端を何度も跨ぐ描画を、まとめて 1 つの検査に入れる。
        assert_no_alloc::assert_no_alloc(|| {
            let mut beat = 0.0_f64;
            for i in 0..400usize {
                match i {
                    8 => rt.push_request(LaunchRequest::Cell {
                        key: RowKey::track(1),
                        clip_id: 10,
                        pressed: true,
                    }),
                    24 => rt.push_request(LaunchRequest::Scene { scene_id: 2, pressed: true }),
                    64 => rt.push_request(LaunchRequest::StopAll),
                    96 => rt.push_request(LaunchRequest::Scene { scene_id: 1, pressed: true }),
                    200 => rt.on_transport_jump(-8.0),
                    _ => {}
                }
                if i == 200 {
                    beat -= 8.0;
                }
                let span = BufferSpan::new(beat, 120.0, 48_000, 512);
                rt.update(&song, span, LaunchQuantize::Off, true);
                rt.publish(&bridge, span.start_beat);
                let src = rt.rows().track_row(0);
                out.clear();
                collect_row_midi(
                    Some(&song), 0, src, 48_000, beat, 120.0, 512, &mut out, &mut active,
                );
                render_row_audio(
                    &renderer,
                    0,
                    src,
                    &mut l,
                    &mut r,
                    beat,
                    120.0,
                    48_000,
                    512,
                    &mut ClipRenderState {
                        repitch_accum: &mut accum,
                        engines: &mut engines,
                        event_l: &mut event_l,
                        event_r: &mut event_r,
                        render_seq: &mut render_seq,
                    },
                );
                beat += 512.0 * 120.0 / (60.0 * 48_000.0);
            }
            // 検査の中で本当に走ったことを確かめる (何も鳴っていないと
            // `for_each_segment` が即 return して素通りする)。
            assert!(!active.is_empty() || !out.is_empty());
        });
        let _: &RowSourceTable = rt.rows();
    }
}
