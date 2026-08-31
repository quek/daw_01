//! handler::launcher_clipboard — r.md #87 ランチャーのセルの copy / cut / paste。
//!
//! アレンジのクリップと **同じ作法**に揃える (`feedback_selection_action_last_wins`):
//! - copy は選択集合を正規化して OS クリップボードへ JSON で載せる
//! - 位置は **選択群の左上を (0, 0) とした相対 (行, 列)**
//!   (アレンジが「最上段トラック / 最早拍」を 0 にするのと同型)
//! - paste 先は **ポインタが乗っているセル**。乗っていなければ貼らない
//!   (再生ヘッドや先頭列への fallback はしない — アレンジの paste と同じ規約)
//! - 同一プロジェクトなら content を共有 (リンク) / 別プロジェクトなら
//!   inline payload から独立採番。オートメーションのセルは
//!   `paste_automation_clips_at` と同じく **常に独立** (値が target 依存なので)

use common::model::{AutomationClip, Clip, ClipContent};

use crate::clipboard::{
    AutomationClipCopy, ClipCopy, ClipboardEnvelope, ClipboardPayload, LauncherCellCopy,
    LauncherCellPayload,
};
use common::model::{AutomationClipKey, ClipKey};

use crate::event_launcher::{LauncherCellKey, LauncherRow};
use crate::state::{AppData, LauncherFocus};

impl AppData {
    /// 選択中のセルを clipboard envelope (JSON) にする。
    /// 戻り値 = `(json, 件数)`。セルを 1 つも選んでいなければ `None`。
    #[must_use]
    pub fn copy_launcher_cells_clip(&self) -> Option<(String, usize)> {
        let cells = self.selected_launcher_cells();
        if cells.is_empty() {
            return None;
        }
        // **行の座標は曲の構造 (`all_launcher_rows`) で数える。** 表示順
        // (`launcher_rows`) は折りたたみで変わるので、コピーと貼り付けで
        // 畳み方が違うと別の行に貼られ、畳まれた行のセルは黙って落ちる。
        let rows = self.all_launcher_rows();
        // 相対座標の原点 = 選択群の左上。
        let mut placed: Vec<(usize, usize, LauncherCellKey)> = Vec::new();
        for cell in &cells {
            let Some(row_i) = rows.iter().position(|r| *r == cell.row()) else {
                continue;
            };
            let Some(col) = self
                .scene_of_cell(*cell)
                .and_then(|s| self.song_doc.song().scene_index(s))
            else {
                continue;
            };
            placed.push((row_i, col, *cell));
        }
        let min_row = placed.iter().map(|(r, _, _)| *r).min()?;
        let min_col = placed.iter().map(|(_, c, _)| *c).min()?;

        let mut out: Vec<LauncherCellCopy> = Vec::with_capacity(placed.len());
        for (row_i, col, cell) in placed {
            let Some(payload) = self.cell_clipboard_payload(cell) else {
                continue;
            };
            out.push(LauncherCellCopy {
                row_offset: (row_i - min_row) as i64,
                scene_offset: (col - min_col) as i64,
                cell: payload,
                launch: self.launch_settings_of(cell).unwrap_or_default(),
            });
        }
        if out.is_empty() {
            return None;
        }
        let count = out.len();
        let json = ClipboardEnvelope::new(
            self.song_doc.song().project_id,
            ClipboardPayload::LauncherCells(out),
        )
        .to_json()?;
        Some((json, count))
    }

    /// セル 1 つの中身を clipboard 表現にする。
    /// 行の種別ごとの組み立ては自由関数へ出してある (不変条件 9 のインデント 6 段)。
    fn cell_clipboard_payload(&self, cell: LauncherCellKey) -> Option<LauncherCellPayload> {
        let song = self.song_doc.song();
        match cell {
            LauncherCellKey::Track(k) => track_cell_payload(song, k),
            LauncherCellKey::Lane(k) => lane_cell_payload(song, k),
        }
    }

    /// clipboard のセル群を `dest` を左上として貼る。戻り値 = 実際に置けた件数。
    ///
    /// 貼り先の行が足りない (下端を越える) セルは黙って落とす。列は
    /// `Song::ensure_scene_at` で必要なだけ実体化する (プレースホルダ列に
    /// 貼れる = ユーザーが列を先に作らなくてよい)。
    pub fn paste_launcher_cells(
        &mut self,
        cells: Vec<LauncherCellCopy>,
        src_pid: u64,
        dest: LauncherFocus,
    ) -> usize {
        if cells.is_empty() {
            return 0;
        }
        // コピー側と同じ基準 (曲の構造) で数える。
        let rows = self.all_launcher_rows();
        let Some(base_row) = rows.iter().position(|r| *r == dest.row) else {
            return 0;
        };
        let same_project = src_pid == self.song_doc.song().project_id;
        let mut made: Vec<LauncherCellKey> = Vec::new();
        self.edit_song_checked(|song| {
            let mut remap: std::collections::HashMap<
                common::model::ContentId,
                common::model::ContentId,
            > = std::collections::HashMap::new();
            for cc in &cells {
                let Some(row) = rows.get(base_row + cc.row_offset as usize).copied() else {
                    continue;
                };
                // 種別が合わない (MIDI セル → オートメーションレーン行) 組み合わせは
                // **列を実体化する前**に弾く。後で弾くと、貼れなかったのに列だけが
                // 増えた状態が残る (`edit_song_checked` は snapshot を捨てるだけで
                // 編集を巻き戻さない)。
                let compatible = matches!(
                    (&cc.cell, row),
                    (LauncherCellPayload::Track(_), LauncherRow::Track(_))
                        | (LauncherCellPayload::Lane(_), LauncherRow::Lane(_))
                );
                if !compatible {
                    continue;
                }
                // **行がセルを持てるかも列を実体化する前に確かめる。** 後で失敗すると
                // 「貼れていないのに列だけ増え、undo もできない」状態が残る
                // (`edit_song_checked` は snapshot を捨てるだけで巻き戻さない)。
                if !crate::handler::launcher_cells::row_accepts_cells(song, row) {
                    continue;
                }
                let scene_id = song.ensure_scene_at(dest.scene_index + cc.scene_offset as usize);
                if let Some(key) = paste_one(song, row, scene_id, cc, same_project, &mut remap) {
                    made.push(key);
                }
            }
            !made.is_empty()
        });
        let n = made.len();
        self.set_launcher_cell_selection(&made);
        n
    }
}

/// セル 1 つを貼る。行の種別と payload の種別が食い違うときは置かない
/// (MIDI セルをオートメーションレーン行へ貼っても意味が無い)。
fn paste_one(
    song: &mut common::model::Song,
    row: LauncherRow,
    scene_id: u32,
    cc: &LauncherCellCopy,
    same_project: bool,
    remap: &mut std::collections::HashMap<common::model::ContentId, common::model::ContentId>,
) -> Option<LauncherCellKey> {
    match (&cc.cell, row) {
        (LauncherCellPayload::Track(c), LauncherRow::Track(track_id)) => {
            // 同一プロジェクトで content が現存すればリンク共有、それ以外は
            // inline payload から独立採番 (`paste_clips_at` と同じ規則)。
            let content_id = match remap.get(&c.content_id) {
                Some(&id) => id,
                None => {
                    let id = if same_project && song.clip_contents.contains_key(&c.content_id) {
                        c.content_id
                    } else {
                        song.alloc_content(c.content.clone(), c.name.clone().unwrap_or_default())
                    };
                    remap.insert(c.content_id, id);
                    id
                }
            };
            let track = song.track_by_id_mut(track_id)?;
            let id = track.alloc_clip_id();
            track.put_session_clip(common::model::SessionClip {
                scene_id,
                clip: Clip {
                    id,
                    start_beat: 0.0,
                    length_beats: c.length_beats,
                    content_id,
                    content_offset_beats: c.content_offset_beats,
                    // 新規クリップにクロスフェードの張り出しは無い。
                    xfade_lead_beats: 0.0,
                    xfade_tail_beats: 0.0,
                    color: c.color,
                    auto_lipsync: false,
                    lipsync_gen: 0,
                    muted: c.muted,
                    speaker_id: c.speaker_id,
                    singer_name: c.singer_name.clone(),
                    style_name: c.style_name.clone(),
                    talk: c.talk,
                },
                launch: cc.launch.clone(),
            });
            Some(LauncherCellKey::Track(common::model::ClipKey { track_id, clip_id: id }))
        }
        (LauncherCellPayload::Lane(a), LauncherRow::Lane(lk)) => {
            let target = song.automation_lane_by_key(lk.track, lk.lane)?.target.clone();
            // v29: 新規 content の点にも安定 id を採番する (1 始まりの連番 =
            // per-content allocator と同じ、`paste_automation_clips_at` と同型)。
            let mut points: Vec<common::model::AutomationPoint> = a
                .points
                .iter()
                .enumerate()
                .map(|(i, p)| common::model::AutomationPoint {
                    id: i as u32 + 1,
                    time_beat: p.time_beat.max(0.0),
                    value: common::automation::norm_to_plain(&target, p.value_norm),
                    curve: p.curve,
                })
                .collect();
            points.sort_by(|x, y| x.time_beat.total_cmp(&y.time_beat));
            // オートメーションは値が target 依存なので **常に独立採番**
            // (`paste_automation_clips_at` と同じ方針)。
            let content_id = song.alloc_content(
                ClipContent::Automation(common::model::AutomationContent {
                    next_point_id: points.len() as u32 + 1,
                    points,
                }),
                a.name.clone().unwrap_or_default(),
            );
            let lane = song.automation_lane_by_key_mut(lk.track, lk.lane)?;
            let id = lane.alloc_clip_id();
            lane.put_session_clip(common::model::SessionAutomationClip {
                scene_id,
                clip: AutomationClip {
                    id,
                    start_beat: 0.0,
                    length_beats: a.length_beats,
                    content_id,
                    content_offset_beats: 0.0,
                    name: String::new(),
                },
                launch: cc.launch.clone(),
            });
            Some(LauncherCellKey::Lane(common::model::AutomationClipKey {
                track: lk.track,
                lane: lk.lane,
                clip: id,
            }))
        }
        _ => None,
    }
}

/// トラック行のセル → clipboard 表現。位置は [`LauncherCellCopy`] 側の (行, 列) が
/// 持つので、クリップ側の座標 (`track_offset` / `start_beat`) は使わない
/// (セルの `start_beat` は常に 0)。
fn track_cell_payload(song: &common::model::Song, k: ClipKey) -> Option<LauncherCellPayload> {
    let c = song.track_by_id(k.track_id)?.session_clip_by_id(k.clip_id)?;
    let content = song.clip_contents.get(&c.clip.content_id).cloned()?;
    Some(LauncherCellPayload::Track(ClipCopy {
        track_offset: 0,
        start_beat: 0.0,
        length_beats: c.clip.length_beats,
        color: c.clip.color,
        muted: c.clip.muted,
        content_id: c.clip.content_id,
        content_offset_beats: c.clip.content_offset_beats,
        content,
        name: song.clip_content_names.get(&c.clip.content_id).cloned(),
        speaker_id: c.clip.speaker_id,
        singer_name: c.clip.singer_name.clone(),
        style_name: c.clip.style_name.clone(),
        talk: c.clip.talk,
    }))
}

/// オートメーションレーン行のセル → clipboard 表現。点は **clip の窓ローカル +
/// target 非依存の normalized 値**で運ぶ (`copy_automation_clips_clip` と同じ規則)。
fn lane_cell_payload(
    song: &common::model::Song,
    k: AutomationClipKey,
) -> Option<LauncherCellPayload> {
    let lane = song.automation_lane_by_key(k.track, k.lane)?;
    let c = lane.session_clips.iter().find(|c| c.clip.id == k.clip)?;
    let points = automation_points_of(song, &lane.target, c.clip.content_id, c.clip.content_offset_beats);
    Some(LauncherCellPayload::Lane(AutomationClipCopy {
        start_beat: 0.0,
        length_beats: c.clip.length_beats,
        source_content_id: c.clip.content_id,
        points,
        name: song.clip_content_names.get(&c.clip.content_id).cloned(),
    }))
}

/// content の curve を clipboard の点列へ落とす。
fn automation_points_of(
    song: &common::model::Song,
    target: &common::model::AutomationTarget,
    content_id: common::model::ContentId,
    window_offset: f64,
) -> Vec<crate::clipboard::CopiedPoint> {
    let Some(ClipContent::Automation(a)) = song.clip_contents.get(&content_id) else {
        return Vec::new();
    };
    a.points
        .iter()
        .map(|p| crate::clipboard::CopiedPoint {
            time_beat: p.time_beat - window_offset,
            value_norm: common::automation::plain_to_norm(target, p.value),
            curve: p.curve,
        })
        .collect()
}

