//! `AppData` からランチャー帯の 1 フレーム分のビュー ([`LauncherView`]) を組む。
//!
//! `view_build::build` の末尾から呼ばれ、既に組み終わった widget 側のトラック /
//! マスターレーンを受け取る (色・深さ・アーム状態を 2 度導出しないため)。

use super::*;

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
            cells: HashMap::new(),
        },
    );
    for lane in master_lanes {
        let Some(ml) = song.song_lanes.iter().find(|l| l.id == lane.id) else {
            continue;
        };
        rows.insert(
            ArrangementRowKey::Lane(common::model::AutomationLaneKey {
                track: MASTER_TRACK_ID,
                lane: lane.id,
            }),
            lane_row(ml, lane, song),
        );
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
                    linked: content_refcount(song, sc.clip.content_id) >= 2,
                    follow: sc.launch.follow.enabled,
                    content_offset_beats: sc.clip.content_offset_beats,
                    len_beats: sc.clip.length_beats,
                    curve: Vec::new(),
                },
            );
        }
        rows.insert(
            ArrangementRowKey::Track(t.id),
            LauncherRowView {
                playback: mt.launcher,
                armed: t.armed,
                group,
                cells,
            },
        );
        for lane in &t.automation_lanes {
            let Some(ml) = mt.automation_lanes.iter().find(|l| l.id == lane.id) else {
                continue;
            };
            rows.insert(
                ArrangementRowKey::Lane(common::model::AutomationLaneKey {
                    track: t.id,
                    lane: lane.id,
                }),
                lane_row(ml, lane, song),
            );
        }
    }

    LauncherView {
        scenes,
        rows,
        layout: app.ui_prefs.launcher_layout,
        width: app.ui_prefs.launcher_width,
        col_w: app.ui_prefs.launcher_scene_col_w,
        scroll_scene: app.ui_prefs.launcher_scroll_scene,
        progress: build_progress(app),
    }
}

/// オートメーションレーン行 1 本のセル群。
fn lane_row(
    ml: &common::model::AutomationLane,
    lane: &ArrangementAutomationLane,
    song: &common::model::Song,
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
                color: lane.color,
                muted: false,
                linked: content_refcount(song, sc.clip.content_id) >= 2,
                follow: sc.launch.follow.enabled,
                content_offset_beats: sc.clip.content_offset_beats,
                len_beats: sc.clip.length_beats,
                curve,
            },
        );
    }
    LauncherRowView {
        playback: ml.launcher,
        // レーン行は録音アームを持たない (空セルは常に停止 ■)。
        armed: false,
        group: false,
        cells,
    }
}

/// content を参照しているクリップの数。
///
/// **`all_clips()` を通す** — arrangement と launcher の両方を数えないと、リンク
/// 表示 (`⇌`) がランチャーを跨いだ共有で出ない (同じ数え落としが保存時の GC で
/// 「セルの中身が黙って消える」に化ける、`common/src/model/track.rs` の doc 参照)。
fn content_refcount(song: &common::model::Song, id: common::model::ContentId) -> usize {
    let mut n = 0usize;
    for t in &song.tracks {
        n += t.all_clips().filter(|c| c.content_id == id).count();
        for lane in &t.automation_lanes {
            n += lane.all_clips().filter(|c| c.content_id == id).count();
        }
    }
    for lane in &song.song_lanes {
        n += lane.all_clips().filter(|c| c.content_id == id).count();
    }
    n
}

/// 走行中セルの進捗 (行 → `0..1`)。
///
/// **束 B が `common::audio_bridge` の atomic で publish するまで常に空**
/// (計画書 §1.4: 走行位置は `Song` に入れない = 表示専用)。配線するときは
/// **この 1 関数だけ**を差し替える — 表示側 ([`draw`](super::draw)) は
/// 既に `Some(0..1)` を受ける形で書いてある。
fn build_progress(_app: &AppData) -> HashMap<ArrangementRowKey, f32> {
    HashMap::new()
}
