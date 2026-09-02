//! r.md #87 × `docs/plan_range_selection.md` §5: アレンジのクリップをセルへ運ぶとき、
//! 「オートメーションをクリップに追従」が ON なら、そのトラックのオートメーションレーンで
//! クリップの窓 `[start, end)` に**完全に入る**オートメーションクリップも、同じ列の
//! レーン行のセルへ運ぶ (アレンジ内の移動 `move_track_automation` と同じ選別規則 —
//! 境界で割ってから、窓に完全に入るものだけ)。
//!
//! レーン行のセルは 1 つのクリップしか持てず、セルのクリップは `start_beat = 0`
//! (撃った瞬間が原点、`Song::normalize_session` が正す) なので:
//! - 窓の中のクリップが 1 つで、窓とぴったり同じ範囲 → そのまま運ぶ。中身 (content) は
//!   共有したまま = アレンジ内の移動 / リンクコピーと同じ意味。
//! - それ以外 (窓の途中から始まる / 複数) → 窓の中で聞こえていた曲線を、窓全体を覆う 1 つの
//!   content に写す ([`merge_window`])。クリップの無い区間はレーン既定値。中身は新規なので、
//!   どのモードでも独立コピーになる。

use std::collections::HashMap;

use common::automation::evaluate_clip;
use common::model::{
    AutomationClip, AutomationClipKey, AutomationContent, AutomationCurve, AutomationLaneKey,
    AutomationPoint, ClipContent, ContentId, LaunchSettings, SessionAutomationClip, Song,
};

use super::launcher_cells::row_accepts_cells;
use crate::event_launcher::{LauncherCellKey, LauncherDropMode, LauncherRow};

const EPS: f64 = 1e-9;

/// トラック `track_id` の全レーンについて、窓 `[start, end)` のオートメーションを列
/// `dest_scene` のレーン行セルへ運ぶ。戻り値は作ったセルの key。
///
/// `mode` の意味はクリップと同じ: `Move` は元のクリップをレーンから消す、`CopyLinked` は
/// 中身を共有、`CopyIndependent` は中身を fork する。
pub(crate) fn carry_track_automation_to_cells(
    song: &mut Song,
    track_id: u32,
    start: f64,
    end: f64,
    dest_scene: u32,
    mode: LauncherDropMode,
) -> Vec<LauncherCellKey> {
    let mut made = Vec::new();
    if end <= start + EPS {
        return made;
    }
    let Some(track) = song.track_by_id(track_id) else {
        return made;
    };
    let lane_ids: Vec<u32> = track.automation_lanes.iter().map(|l| l.id).collect();
    for lane_id in lane_ids {
        let row = LauncherRow::Lane(AutomationLaneKey { track: track_id, lane: lane_id });
        // セルを置けないレーン (テンポ / 拍子) は運ばない (`CreateCell` と同じ門番)。
        if !row_accepts_cells(song, row) {
            continue;
        }
        // 窓の境界でクリップを割り、窓に完全に入るものだけを対象にする。
        let Some(lane) = song.automation_lane_by_key_mut(track_id, lane_id) else {
            continue;
        };
        lane.split_at(start);
        lane.split_at(end);
        let default_value = lane.default_value;
        let inside: Vec<AutomationClip> =
            lane.clips.iter().filter(|c| inside_window(c, start, end)).cloned().collect();
        if inside.is_empty() {
            continue;
        }
        let aligned = |c: &AutomationClip| {
            (c.start_beat - start).abs() < EPS
                && (c.start_beat + c.length_beats - end).abs() < EPS
        };
        let shared = matches!(inside.as_slice(), [c] if aligned(c));
        let mut cell_clip = match inside.as_slice() {
            // 窓とぴったり同じ 1 つ: そのまま運ぶ (中身は共有)。
            [c] if shared => AutomationClip { id: 0, start_beat: 0.0, ..c.clone() },
            // それ以外: 窓の中の曲線を、窓全体を覆う 1 つの content に畳む。
            many => {
                let content = merge_window(&song.clip_contents, many, start, end, default_value);
                let cid = song.alloc_content_id();
                song.clip_contents.insert(cid, ClipContent::Automation(content));
                AutomationClip {
                    id: 0,
                    start_beat: 0.0,
                    length_beats: end - start,
                    content_id: cid,
                    ..AutomationClip::default()
                }
            }
        };
        if mode == LauncherDropMode::CopyIndependent && shared {
            cell_clip.content_id = song.fork_content(cell_clip.content_id);
        }
        let Some(lane) = song.automation_lane_by_key_mut(track_id, lane_id) else {
            continue;
        };
        let id = lane.alloc_clip_id();
        cell_clip.id = id;
        lane.put_session_clip(SessionAutomationClip {
            scene_id: dest_scene,
            clip: cell_clip,
            launch: LaunchSettings::default(),
        });
        if mode == LauncherDropMode::Move {
            lane.clips.retain(|c| !inside_window(c, start, end));
        }
        made.push(LauncherCellKey::Lane(AutomationClipKey { track: track_id, lane: lane_id, clip: id }));
    }
    made
}

fn inside_window(c: &AutomationClip, start: f64, end: f64) -> bool {
    c.start_beat >= start - EPS && c.start_beat + c.length_beats <= end + EPS
}

/// 窓 `[start, end)` の中で聞こえていた曲線を 1 つの content (窓の先頭 = 拍 0) に写す。
///
/// 各クリップの窓の先頭に「その時点の値」を Hold で置き、末尾にレーン既定値を Hold で置く。
/// これでクリップ間の隙間はアレンジと同じくレーン既定値、クリップの中は元の点と補間
/// (点の `curve` は「前の点からこの点へ」の形なので、そのまま写せば同じ形になる)。
/// 窓の外に出た点 (`content_offset_beats` で手前 / 奥へずれた点) は先頭 / 末尾の合成点が
/// その時点の値を持つので落としてよい。
fn merge_window(
    contents: &HashMap<ContentId, ClipContent>,
    clips: &[AutomationClip],
    start: f64,
    end: f64,
    default_value: f64,
) -> AutomationContent {
    let mut sorted: Vec<&AutomationClip> = clips.iter().collect();
    sorted.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
    let mut out = AutomationContent::default();
    let push = |out: &mut AutomationContent, t: f64, value: f64, curve: AutomationCurve| {
        let id = out.alloc_point_id();
        out.points.push(AutomationPoint { id, time_beat: t, value, curve });
    };
    // 先頭のクリップが窓の途中から始まるなら、拍 0 に既定値を置く (`evaluate_clip` は
    // 最初の点より手前をその点の値に張り付けるので、置かないと手前がクリップの値になる)。
    if sorted.first().is_some_and(|c| c.start_beat > start + EPS) {
        push(&mut out, 0.0, default_value, AutomationCurve::Hold);
    }
    for c in sorted {
        let (c_start, c_end) = (c.start_beat.max(start), (c.start_beat + c.length_beats).min(end));
        let auto = match contents.get(&c.content_id) {
            Some(ClipContent::Automation(a)) if !a.points.is_empty() => a,
            // 中身が無いクリップはアレンジでもレーン既定値。
            _ => {
                push(&mut out, c_start - start, default_value, AutomationCurve::Hold);
                continue;
            }
        };
        let v0 = evaluate_clip(auto, c.song_to_content_beat(c_start));
        push(&mut out, c_start - start, v0, AutomationCurve::Hold);
        for p in &auto.points {
            let song_t = c.content_to_song_beat(p.time_beat);
            if song_t > c_start + EPS && song_t < c_end - EPS {
                push(&mut out, song_t - start, p.value, p.curve);
            }
        }
        push(&mut out, c_end - start, default_value, AutomationCurve::Hold);
    }
    out
}
