//! `AppData` からランチャー帯の 1 フレーム分のビュー ([`LauncherView`]) を組む。
//!
//! `view_build::build` の末尾から呼ばれ、既に組み終わった widget 側のトラック /
//! マスターレーンを受け取る (色・深さ・アーム状態を 2 度導出しないため)。

use super::*;

use crate::event_launcher::LauncherRow;
use crate::handler::launcher_cells::row_accepts_cells;
use crate::view::track_color;

/// `view_build::build` の末尾で呼ぶ。
///
/// `tracks` / `master_lanes` は **既に組んだ widget 側のビュー** (色 / 深さ /
/// アーム状態の SSoT)。ここでは model 側の `session_clips` / `launcher` を
/// 行キーに紐付け直すだけで、色や深さを別式で作り直さない。
pub(crate) fn build(
    app: &AppData,
    tracks: &[ArrangementTrack],
    master_lanes: &[ArrangementAutomationLane],
    // content ごとの参照数 (`view_build` が 1 度だけ作る表)。
    refcounts: &HashMap<common::model::ContentId, usize>,
    // r.md #91: 連動ハイライトの対象 content (`view_build::active_share_groups`、アレンジと同じ集合)。
    active_groups: &HashSet<common::model::ContentId>,
) -> LauncherView {
    let song = app.song_doc.song();
    let scenes: Vec<LauncherSceneView> = song
        .scenes
        .iter()
        .enumerate()
        .map(|(i, s)| LauncherSceneView {
            id: s.id,
            name: Arc::from(s.display_name(i)),
            color: s
                .color
                .map_or_else(|| app.theme.core.accent, track_color::to_renderer),
            follow: s.follow.enabled,
            selected: app.selection.selected_scene_ids.contains(&s.id),
        })
        .collect();

    let mut rows: HashMap<ArrangementRowKey, LauncherRowView> = HashMap::new();

    // マスター行そのものはクリップを持たない (`song_lanes` = オートメーションのみ)。
    // 行としては並ぶので空の行ビューを置く (格子の行が欠けると縦位置がズレて見える)。
    rows.insert(
        ArrangementRowKey::Track(MASTER_TRACK_ID),
        LauncherRowView {
            playback: RowPlayback::Arranger,
            armed: false,
            group: false,
            // マスターのトラック行はセルも主導権も持たない。
            launchable: false,
            takes_cells: false,
            cells: HashMap::new(),
        },
    );
    for lane in master_lanes {
        let Some(ml) = song.song_lanes.iter().find(|l| l.id == lane.id) else {
            continue;
        };
        let key = common::model::AutomationLaneKey { track: MASTER_TRACK_ID, lane: lane.id };
        rows.insert(ArrangementRowKey::Lane(key), lane_row(ml, lane, key, song, refcounts, active_groups));
    }

    for t in tracks {
        let Some(mt) = song.tracks.iter().find(|mt| mt.id == t.id) else {
            continue;
        };
        let group = tracks.iter().any(|o| o.parent_id == Some(t.id));
        let mut cells: HashMap<u32, LauncherCellView> = HashMap::new();
        for sc in &mt.session_clips {
            cells.insert(
                sc.scene_id,
                LauncherCellView {
                    clip_id: sc.clip.id,
                    name: crate::widgets::arrangement::view_build::clip_display_label(
                        &sc.clip, song,
                    ),
                    color: track_color::to_renderer(track_color::effective_clip_color(
                        mt, &sc.clip,
                    )),
                    muted: sc.clip.muted,
                    linked: is_shared(refcounts, sc.clip.content_id),
                    in_active_group: active_groups.contains(&sc.clip.content_id),
                    follow: sc.launch.follow.enabled,
                    content_offset_beats: sc.clip.content_offset_beats,
                    len_beats: sc.clip.length_beats,
                    looping: sc.launch.looping,
                    curve: Vec::new(),
                    // r.md #94: アレンジのクリップと同じ resolver (供給元は project 共有の
                    // texture cache なので、セッションだけに居るクリップにも同じ絵が出る)。
                    thumbnail: crate::widgets::arrangement::view_build::clip_thumbnail(
                        app,
                        song.clip_contents.get(&sc.clip.content_id),
                    ),
                },
            );
        }
        rows.insert(
            ArrangementRowKey::Track(t.id),
            LauncherRowView {
                playback: mt.launcher,
                armed: t.armed,
                group,
                launchable: true,
                takes_cells: row_accepts_cells(song, LauncherRow::Track(t.id)),
                cells,
            },
        );
        for lane in &t.automation_lanes {
            let Some(ml) = mt.automation_lanes.iter().find(|l| l.id == lane.id) else {
                continue;
            };
            let key = common::model::AutomationLaneKey { track: t.id, lane: lane.id };
            rows.insert(ArrangementRowKey::Lane(key), lane_row(ml, lane, key, song, refcounts, active_groups));
        }
    }

    // engine の走行状態を最後に被せる (`Song` は「撃った起点」しか持たないので、
    // フォローアクションで移った先はこちらにしか出ない)。
    let (progress, queued) = apply_running(app, &mut rows);

    LauncherView {
        scenes,
        rows,
        layout: app.ui_prefs.launcher_layout,
        width: app.ui_prefs.launcher_width,
        col_w: app.ui_prefs.launcher_scene_col_w,
        scroll_scene: app.ui_prefs.launcher_scroll_scene,
        progress,
        queued,
        selected: app.selection.selected_launcher_cells.clone(),
    }
}

/// オートメーションレーン行 1 本のセル群。
fn lane_row(
    ml: &common::model::AutomationLane,
    lane: &ArrangementAutomationLane,
    key: common::model::AutomationLaneKey,
    song: &common::model::Song,
    refcounts: &HashMap<common::model::ContentId, usize>,
    active_groups: &HashSet<common::model::ContentId>,
) -> LauncherRowView {
    let mut cells: HashMap<u32, LauncherCellView> = HashMap::new();
    for sc in &ml.session_clips {
        let curve: Vec<(f64, f32)> = song
            .clip_contents
            .get(&sc.clip.content_id)
            .and_then(common::model::ClipContent::automation_points)
            .unwrap_or(&[])
            .iter()
            .map(|p| {
                (
                    p.time_beat - sc.clip.content_offset_beats,
                    common::automation::plain_to_norm_ranged(
                        &lane.target,
                        p.value,
                        lane.plugin_range,
                    ),
                )
            })
            .collect();
        cells.insert(
            sc.scene_id,
            LauncherCellView {
                clip_id: sc.clip.id,
                name: Arc::from(song.content_name(sc.clip.content_id)),
                // per-clip 上書き > レーン識別色 (アレンジの automation clip と同じ優先順)。
                color: sc.clip.color.map_or(lane.color, track_color::to_renderer),
                muted: false,
                linked: is_shared(refcounts, sc.clip.content_id),
                in_active_group: active_groups.contains(&sc.clip.content_id),
                follow: sc.launch.follow.enabled,
                content_offset_beats: sc.clip.content_offset_beats,
                len_beats: sc.clip.length_beats,
                looping: sc.launch.looping,
                curve,
                // オートメーションの content に絵は無い。
                thumbnail: None,
            },
        );
    }
    LauncherRowView {
        playback: ml.launcher,
        // レーン行は録音アームを持たない (空セルは常に停止 ■)。
        armed: false,
        group: false,
        // テンポ / 拍子レーンだけ `false` (engine の `for_each_launcher_row` と同じ 1 本)。
        launchable: ml.target.accepts_launcher_cells(),
        takes_cells: row_accepts_cells(song, LauncherRow::Lane(key)),
        cells,
    }
}

/// content を参照しているクリップの数。
///
/// **`all_clips()` を通す** — arrangement と launcher の両方を数えないと、リンク
/// 表示 (`⇌`) がランチャーを跨いだ共有で出ない (同じ数え落としが保存時の GC で
/// 「セルの中身が黙って消える」に化ける、`common/src/model/track.rs` の doc 参照)。
/// 共有マーク (`⇌`) の判定。**数え方の SSoT は `view_build` が 1 度だけ作る
/// `refcount_by_content`** で、ここはそれを引くだけ (セル 1 個ごとに全クリップを
/// 走査し直すと毎フレーム O(セル数 × クリップ数) になり、しかも数え方が 2 本に
/// 増えて片方だけ直すと表示が食い違う)。
fn is_shared(
    refcounts: &HashMap<common::model::ContentId, usize>,
    id: common::model::ContentId,
) -> bool {
    refcounts.get(&id).copied().unwrap_or(0) >= 2
}

/// engine が publish した**走行状態**を各行に被せ、進捗 (行 → `0..1`) と
/// 予約 (行 → [`QueuedView`]) を返す。
///
/// `Song.launcher` が持つのは「ユーザーが最後に撃った状態」= 再生の起点だけで、
/// **フォローアクションで移った先はここにしか出ない** (計画書 §1.4)。被せないと
/// 音だけ次のセルへ進んでグリッドが前のセルを光らせ続ける。
///
/// engine が publish していない行 (= 停止中 / まだ届いていない) は `Song` 側の値を
/// そのまま使う — 起動直後や再生前でも「撃ってあるセル」が正しく光る。
fn apply_running(
    app: &AppData,
    rows: &mut HashMap<ArrangementRowKey, LauncherRowView>,
) -> (HashMap<ArrangementRowKey, f32>, HashMap<ArrangementRowKey, QueuedView>) {
    use common::audio_bridge::{LAUNCHER_STATE_PLAYING, LAUNCHER_STATE_STOPPED};
    let mut progress = HashMap::new();
    let mut queued = HashMap::new();
    for (key, row) in rows.iter_mut() {
        let (track_id, lane_id) = match key {
            ArrangementRowKey::Track(id) => (*id, 0),
            ArrangementRowKey::Lane(k) => (k.track, k.lane),
        };
        let Some(snap) = app.launcher_running_row(track_id, lane_id) else {
            continue;
        };
        // 予約 (量子化境界待ち)。**残り拍は engine の発火拍から引くだけ** —
        // GUI 側で境界を解き直すと、グローバル量子化を迂回するシーンの
        // フォローアクション由来の予約で必ず食い違う。
        if snap.queued_clip_id != 0
            && let Some(ph) = app.transport.playhead_beat
        {
            let remaining = snap.queued_at_beat - f64::from(ph);
            if remaining.is_finite() {
                queued.insert(
                    *key,
                    QueuedView { clip_id: snap.queued_clip_id, remaining_beats: remaining },
                );
            }
        }
        row.playback = match snap.state {
            LAUNCHER_STATE_PLAYING => {
                common::model::RowPlayback::Launcher { clip_id: snap.playing_clip_id }
            }
            LAUNCHER_STATE_STOPPED => common::model::RowPlayback::LauncherStopped,
            // `LAUNCHER_STATE_ARRANGER` と、未知の値 (= 世代違いの子プロセス)。
            // 光らせないほうが、無関係なセルを光らせるより誤解が小さい。
            _ => common::model::RowPlayback::Arranger,
        };
        if snap.state != LAUNCHER_STATE_PLAYING {
            continue;
        }
        // **進捗は engine の 30Hz スカラーではなく、撃った拍から毎フレーム解く。**
        // publish は 30Hz なので、そのまま描くと進捗バーがカクつく。位相の式は
        // 映像 / クリップ編集面と同じ 1 本 (`launcher_time::cell_phase`) を通す
        // ので、音・絵・帯がズレない。解けないとき (長さ 0 / まだ届いていない)
        // だけ publish 値へ倒す。
        let smooth = app.transport.playhead_beat.and_then(|ph| {
            let cell = row.cells.values().find(|c| c.clip_id == snap.playing_clip_id)?;
            let phase = crate::launcher_time::cell_phase(
                snap.launch_beat,
                f64::from(ph),
                cell.len_beats,
                cell.looping,
            )?;
            (cell.len_beats > 0.0).then(|| (phase / cell.len_beats) as f32)
        });
        let p = smooth.unwrap_or(snap.progress);
        if p.is_finite() {
            progress.insert(*key, p.clamp(0.0, 1.0));
        }
    }
    (progress, queued)
}
