//! handler::launcher_cells — r.md #87 ランチャーの **セル**の CRUD とローンチ設定。
//!
//! 発火 / 行の主導権 / 列 (シーン) の CRUD は [`crate::handler::launcher`]。
//! 分けてあるのは実コード 1,000 行 budget (不変条件 9) のため。
//!
//! ## セルは arrangement のクリップと同じ addressing
//!
//! セルの `clip.id` は `clips` と **同じ id 空間**から採るので、
//! `ClipKey { track_id, clip_id }` / `AutomationClipKey` がそのまま通る
//! (計画書 §3.3)。選択集合 (`SelectionState::selected_clips` /
//! `selected_automation_clips`) もアレンジのクリップと共有していて、
//! **1 つの key はアレンジのクリップか セルの どちらか一方**なので、
//! 削除 / 色 / 名前などは「両方の入れ物を key で引く」だけで曖昧にならない。
//!
//! ## 列の遅延実体化
//!
//! グリッドは「実シーン + 空きプレースホルダ列」を描く。プレースホルダにセルを
//! 置いた瞬間に [`Song::ensure_scene_at`](common::model::Song::ensure_scene_at) で
//! 列を実体化する。**load 時には列を補わない** ので、開いただけでは `*` が
//! 立たない (r.md #9)。

use common::model::{
    AutomationClip, AutomationClipKey, AutomationLaneKey, Clip, ClipContent, ClipKey,
    LaunchSettings, SessionAutomationClip, SessionClip, Song,
};

use crate::event_launcher::{
    CellToArrangerDrop, ClipToCellDrop, LaunchEdit, LauncherCellKey, LauncherCellMove,
    LauncherDropMode, LauncherRow,
};
use crate::app::AppEvent;
use crate::state::AppData;
use crate::widgets::select_modifier::SelectModifier;

/// 新しい空セルの長さ (拍) を曲の拍子から求める (= 1 小節)。
/// 4/4 なら 4 拍、3/4 なら 3 拍。定数 4.0 のままだと 3/4 拍子で
/// 「1 小節と 1 拍」の半端なループになる。
fn new_cell_length_beats(song: &Song) -> f64 {
    common::model::beats_per_bar(song.time_sig)
}

impl AppData {
    // ------------------------------------------------------------------
    // 参照 / 選択
    // ------------------------------------------------------------------

    /// セルのローンチ設定 (存在しなければ `None`)。
    #[must_use]
    pub fn launch_settings_of(&self, cell: LauncherCellKey) -> Option<LaunchSettings> {
        let song = self.song_doc.song();
        match cell {
            LauncherCellKey::Track(k) => song
                .track_by_id(k.track_id)
                .and_then(|t| t.session_clip_by_id(k.clip_id))
                .map(|c| c.launch.clone()),
            LauncherCellKey::Lane(k) => song
                .automation_lane_by_key(k.track, k.lane)
                .and_then(|l| l.session_clips.iter().find(|c| c.clip.id == k.clip))
                .map(|c| c.launch.clone()),
        }
    }

    /// セルが乗っている列 (`Scene::id`)。
    #[must_use]
    pub fn scene_of_cell(&self, cell: LauncherCellKey) -> Option<u32> {
        let song = self.song_doc.song();
        match cell {
            LauncherCellKey::Track(k) => song
                .track_by_id(k.track_id)
                .and_then(|t| t.session_clip_by_id(k.clip_id))
                .map(|c| c.scene_id),
            LauncherCellKey::Lane(k) => song
                .automation_lane_by_key(k.track, k.lane)
                .and_then(|l| l.session_clips.iter().find(|c| c.clip.id == k.clip))
                .map(|c| c.scene_id),
        }
    }

    /// いま選択されている **セル** (アレンジのクリップは除く)。
    ///
    /// 選択集合はアレンジのクリップと共有なので、「セルの入れ物に居る key だけ」
    /// を残して面を切り出す。インスペクタの「ローンチ」セクションと
    /// `Delete` / `Ctrl+D` / copy の対象がこれ。
    ///
    /// トラック行のセル (`selected_clips`) とオートメーションレーン行のセル
    /// (`selected_automation_clips`) は **共存できる**ので、どちらを対象に
    /// するかは `last_edit_select` (= 直近に確定した面) で決める
    /// (`feedback_selection_action_last_wins`)。 タグが無い / 別の面を指して
    /// いるときだけ両方返す。
    #[must_use]
    pub fn selected_launcher_cells(&self) -> Vec<LauncherCellKey> {
        use crate::app::EditSurface as S;
        let song = self.song_doc.song();
        let last = self.selection.last_edit_select;
        let mut out: Vec<LauncherCellKey> = Vec::new();
        // **直近に確定した面がクリップ系のときだけ**セルを返す。
        // ここを「別の面なら両方返す」にしていると、ピアノロールでノートを選んだ
        // 状態の `Ctrl+C` / `Ctrl+X` / `D` が (セルが選択に残っているだけで)
        // ノートではなくセルに効いてしまう — `Ctrl+X` は消えるので実害が大きい
        // (`feedback_selection_action_last_wins` の "直近確定面で決める")。
        if !matches!(last, Some(S::Clips) | Some(S::AutomationClips)) {
            return out;
        }
        if last != Some(S::AutomationClips) {
            for key in &self.selection.selected_clips {
                if song
                    .track_by_id(key.track_id)
                    .is_some_and(|t| t.session_clip_by_id(key.clip_id).is_some())
                {
                    out.push(LauncherCellKey::Track(*key));
                }
            }
        }
        if last != Some(S::Clips) {
            for key in &self.selection.selected_automation_clips {
                if song
                    .automation_lane_by_key(key.track, key.lane)
                    .is_some_and(|l| l.session_clips.iter().any(|c| c.clip.id == key.clip))
                {
                    out.push(LauncherCellKey::Lane(*key));
                }
            }
        }
        out
    }

    /// セルの長さ (= ループ長) を変える。選択している全セルへ一括で効く。
    ///
    /// セルの `start_beat` は常に 0 なので、変わるのは `length_beats` だけ
    /// (窓の開始 `content_offset_beats` は据え置き = 「どこから鳴らすか」は
    /// 変えずに「どこまで鳴らすか」だけを変える)。
    pub fn set_cell_length(&mut self, cells: &[LauncherCellKey], beats: f64) {
        if cells.is_empty() || !beats.is_finite() {
            return;
        }
        let beats = beats.max(common::model::MIN_CLIP_LEN_BEATS);
        let cells = cells.to_vec();
        self.edit_song_checked(|song| {
            let mut changed = false;
            for cell in &cells {
                let len = match cell {
                    LauncherCellKey::Track(k) => song
                        .track_by_id_mut(k.track_id)
                        .and_then(|t| t.session_clips.iter_mut().find(|c| c.clip.id == k.clip_id))
                        .map(|c| &mut c.clip.length_beats),
                    LauncherCellKey::Lane(k) => song
                        .automation_lane_by_key_mut(k.track, k.lane)
                        .and_then(|l| l.session_clips.iter_mut().find(|c| c.clip.id == k.clip))
                        .map(|c| &mut c.clip.length_beats),
                };
                if let Some(len) = len
                    && (*len - beats).abs() > f64::EPSILON
                {
                    *len = beats;
                    changed = true;
                }
            }
            changed
        });
    }

    /// セルのダブルクリック: 選択してから**そのセルの編集面**を開く。
    ///
    /// 到達先はアレンジのクリップのダブルクリックと同じ 1 式 — オーディオなら
    /// オーディオエディタ、それ以外 (MIDI / 歌唱) はピアノロール。セルのクリップは
    /// アレンジのクリップと同じ `ClipKey` 空間に居るので、編集面は追加実装なしで
    /// そのまま開く (`Song::clip_by_key` が `session_clips` も引く)。
    ///
    /// オートメーションレーン行のセルは編集面を持たない (曲線はレーン上で直接
    /// 編集する) ので、選択だけして終わる。
    pub fn open_cell_editor(&mut self, cell: LauncherCellKey) {
        self.select_launcher_cell(cell, SelectModifier::Single);
        let LauncherCellKey::Track(key) = cell else {
            return;
        };
        if self.is_audio_clip(key) {
            self.handle_event(AppEvent::OpenAudioEditor(key));
        } else {
            self.handle_event(AppEvent::CloseAudioEditor);
            self.handle_event(AppEvent::SelectBottomPanel(1));
        }
    }

    /// セル click の選択遷移 (無修飾 = 置換 / Ctrl = トグル / Shift = 範囲)。
    ///
    /// 範囲は「行 × 列」の長方形ブロック。 アレンジのクリップ選択と同じ
    /// [`SelectModifier`] を使うので、修飾キーの意味が面ごとに割れない。
    pub fn select_launcher_cell(&mut self, cell: LauncherCellKey, modifier: SelectModifier) {
        // 列 (シーン) 選択とは排他 (`SelectionState::selected_scene_ids` の doc)。
        self.selection.selected_scene_ids.clear();
        match cell {
            LauncherCellKey::Track(key) => {
                let items = self.launcher_range_items();
                let anchor = self.selection.clip_anchor;
                let next = modifier.resolve(&self.selection.selected_clips, key, || {
                    let a = anchor?;
                    crate::widgets::select_modifier::range_block(&items, a, key)
                });
                self.selection.selected_clip = next.last().copied();
                self.selection.selected_clips = next;
                if modifier.updates_anchor() {
                    self.selection.clip_anchor = Some(key);
                }
                if self.selection.selected_clip.is_some() {
                    self.selection.last_edit_select = Some(crate::app::EditSurface::Clips);
                }
                // アレンジのクリップ選択 (`select_clip` / `set_clip_selection`) と
                // 同じく、 anchor のトラックへカーソルを追従させる。 これが無いと
                // インスペクタの上半分 (トラック名・色・デバイスチェーン) だけが
                // 前のトラックのまま残り、 下半分 (クリップ / ローンチ設定) と
                // 混ざって見える。 `Clips` タグを立てた **直後**に呼ぶ約束も同じ。
                if let Some(r) = self.selected_clip_ref() {
                    self.select_track(r.track_id);
                }
            }
            LauncherCellKey::Lane(key) => {
                let items = self.launcher_lane_range_items();
                let anchor = self.selection.automation_clip_anchor;
                let next = modifier.resolve(
                    &self.selection.selected_automation_clips,
                    key,
                    || {
                        let a = anchor?;
                        crate::widgets::select_modifier::range_block(&items, a, key)
                    },
                );
                self.selection.selected_automation_clips = next;
                if modifier.updates_anchor() {
                    self.selection.automation_clip_anchor = Some(key);
                }
                if !self.selection.selected_automation_clips.is_empty() {
                    self.selection.last_edit_select =
                        Some(crate::app::EditSurface::AutomationClips);
                }
                // automation セルも同じ — レーンは必ずどれか 1 トラックに属するので、
                // 別トラックのレーンのセルを選んだらカーソルもそのトラックへ動く。
                if let Some(track_id) =
                    self.selection.selected_automation_clips.last().map(|k| k.track)
                {
                    self.select_track(track_id);
                }
            }
        }
        // 撃つ / 設定を出す起点をクリックしたセルへ移す。
        if let (Some(scene_id), row) = (self.scene_of_cell(cell), cell.row())
            && let Some(scene_index) = self.song_doc.song().scene_index(scene_id)
        {
            self.launcher.focus = Some(crate::state::LauncherFocus { row, scene_index });
        }
    }

    /// Shift+click の範囲計算に渡す「行 × 列」のグリッド。
    /// `row` は表示順の行 index、時間軸には **列 index** を入れる
    /// (セルは全部 `start_beat = 0` なので拍では並ばない)。
    fn launcher_range_items(
        &self,
    ) -> Vec<crate::widgets::select_modifier::RangeItem<ClipKey>> {
        let song = self.song_doc.song();
        let mut items = Vec::new();
        for (row_i, row) in self.launcher_rows().into_iter().enumerate() {
            let LauncherRow::Track(track_id) = row else {
                continue;
            };
            let Some(track) = song.track_by_id(track_id) else {
                continue;
            };
            for cell in &track.session_clips {
                let Some(col) = song.scene_index(cell.scene_id) else {
                    continue;
                };
                items.push(crate::widgets::select_modifier::RangeItem {
                    key: ClipKey { track_id, clip_id: cell.clip.id },
                    row: row_i as i64,
                    start: col as f64,
                    end: col as f64 + 1.0,
                });
            }
        }
        items
    }

    /// [`Self::launcher_range_items`] のレーン行版。トラック行と別なのは
    /// 鍵の型が違うだけで、行 × 列の長方形という規則は同じ
    /// (**片方だけ範囲選択できない**という非対称を残さない)。
    fn launcher_lane_range_items(
        &self,
    ) -> Vec<crate::widgets::select_modifier::RangeItem<AutomationClipKey>> {
        let song = self.song_doc.song();
        let mut items = Vec::new();
        for (row_i, row) in self.all_launcher_rows().into_iter().enumerate() {
            let LauncherRow::Lane(lane_key) = row else {
                continue;
            };
            let Some(lane) = song.automation_lane_by_key(lane_key.track, lane_key.lane) else {
                continue;
            };
            for cell in &lane.session_clips {
                let Some(col) = song.scene_index(cell.scene_id) else {
                    continue;
                };
                items.push(crate::widgets::select_modifier::RangeItem {
                    key: AutomationClipKey {
                        track: lane_key.track,
                        lane: lane_key.lane,
                        clip: cell.clip.id,
                    },
                    row: row_i as i64,
                    start: col as f64,
                    end: col as f64 + 1.0,
                });
            }
        }
        items
    }

    /// 消えたセルを選択集合から落とす (列削除 / セル削除の後始末)。
    pub(crate) fn prune_launcher_selection(&mut self) {
        let song = self.song_doc.song();
        // 消えた列を指したままだと、インスペクタが存在しない列の設定を出す。
        let live_scenes: Vec<u32> = song.scenes.iter().map(|s| s.id).collect();
        self.selection.selected_scene_ids.retain(|id| live_scenes.contains(id));
        self.selection.scene_anchor =
            self.selection.scene_anchor.filter(|id| live_scenes.contains(id));
        let song = self.song_doc.song();
        let clip_alive = |k: &ClipKey| {
            song.track_by_id(k.track_id).is_some_and(|t| {
                t.clips.iter().any(|c| c.id == k.clip_id)
                    || t.session_clip_by_id(k.clip_id).is_some()
            })
        };
        let auto_alive = |k: &AutomationClipKey| {
            song.automation_lane_by_key(k.track, k.lane).is_some_and(|l| {
                l.all_clips().any(|c| c.id == k.clip)
            })
        };
        let clips: Vec<ClipKey> =
            self.selection.selected_clips.iter().copied().filter(clip_alive).collect();
        let autos: Vec<AutomationClipKey> = self
            .selection
            .selected_automation_clips
            .iter()
            .copied()
            .filter(auto_alive)
            .collect();
        self.selection.selected_clip = self.selection.selected_clip.filter(clip_alive);
        // **範囲選択の起点 (anchor) も掃除する。** 消えたセルを指したままだと
        // 次の `Shift+click` が範囲を解けず、単一選択に落ちる (「範囲選択が
        // 時々効かない」の正体)。
        self.selection.clip_anchor = self.selection.clip_anchor.filter(clip_alive);
        self.selection.automation_clip_anchor =
            self.selection.automation_clip_anchor.filter(auto_alive);
        self.selection.selected_clips = clips;
        self.selection.selected_automation_clips = autos;
    }

    // ------------------------------------------------------------------
    // セルの CRUD
    // ------------------------------------------------------------------

    /// 空セルに空クリップを作る (空セルのダブルクリック)。
    ///
    /// `scene_index` が実シーン数以上 (= 空きプレースホルダ列) なら
    /// `Song::ensure_scene_at` で列を実体化してから置く。既にセルがあれば no-op。
    pub fn create_launcher_cell(&mut self, row: LauncherRow, scene_index: usize) {
        // `edit_song_checked` を使うのは、行が存在しない / 既にセルがあるときに
        // **空の undo step を積まない**ため (ダブルクリックのたびに履歴が伸びる)。
        let mut created: Option<LauncherCellKey> = None;
        self.edit_song_checked(|song| {
            // **置けるかを先に判定してから列を実体化する。** 順序が逆だと、
            // 置けなかったときに列だけが増えたまま `changed = false` を返すことになり、
            // 「undo できない列」が残る (`edit_song_checked` は snapshot を捨てるだけで
            // 編集を巻き戻さない)。
            if !can_place_cell(song, row, scene_index) {
                return false;
            }
            let scene_id = song.ensure_scene_at(scene_index);
            created = create_cell_in(song, row, scene_id);
            created.is_some()
        });
        let Some(cell) = created else {
            return;
        };
        self.select_launcher_cell(cell, SelectModifier::Single);
    }

    /// セルを削除する。**アレンジのクリップは触らない** (key が指す入れ物だけ)。
    /// 鳴っていたセルを消した行は `normalize_session` が
    /// [`RowPlayback::LauncherStopped`](common::model::RowPlayback::LauncherStopped)
    /// へ落とす (アレンジへは戻さない)。
    pub fn delete_launcher_cells(&mut self, cells: &[LauncherCellKey]) {
        if cells.is_empty() {
            return;
        }
        let cells = cells.to_vec();
        self.edit_song_checked(|song| {
            let mut changed = false;
            for cell in &cells {
                changed |= remove_cell(song, *cell);
            }
            if changed {
                song.normalize_session();
            }
            changed
        });
        self.prune_launcher_selection();
    }

    /// セルを複製する (`D` / `Ctrl+D` / メニューの「複製」)。
    ///
    /// 複製先は **同じ行の右隣で最初に空いている列**。 埋まっていれば列を 1 つ
    /// 足す (Ableton の「複製すると下に伸びる」に相当する、列方向版)。
    /// `unique = false` で content 共有 (リンク) / `true` で独立コピー。
    pub fn duplicate_launcher_cells(&mut self, cells: &[LauncherCellKey], unique: bool) {
        if cells.is_empty() {
            return;
        }
        let cells = cells.to_vec();
        let mut made: Vec<LauncherCellKey> = Vec::new();
        self.edit_song_checked(|song| {
            for cell in &cells {
                // 元セルが解決できたときだけ `ensure_scene_at` へ進む
                // (列を実体化した後で失敗すると、undo できない列が残る)。
                let Some(scene_id) = scene_of(song, *cell) else {
                    continue;
                };
                let Some(from) = song.scene_index(scene_id) else {
                    continue;
                };
                let dest = free_scene_index_after(song, cell.row(), from);
                let dest_id = song.ensure_scene_at(dest);
                let Some(new_id) =
                    crate::handler::launcher::clone_cell_into_scene(song, *cell, dest_id)
                else {
                    continue;
                };
                if unique {
                    make_cell_content_unique(song, cell.row(), new_id);
                }
                made.push(cell_key_in_row(cell.row(), new_id));
            }
            !made.is_empty()
        });
        self.set_launcher_cell_selection(&made);
    }

    /// ドラッグの release でセルを移動 / コピーする。
    ///
    /// 修飾キーの意味は既存のクリップ規約そのまま (素で移動 / `Ctrl` でリンク
    /// コピー / `Ctrl+Shift` で独立コピー)。落とし先が空きプレースホルダ列なら
    /// 列を実体化する。行を跨いだ移動は **切り貼り** (元の行から消して先の行へ
    /// 置く) で、id は落とし先の行で採り直す (id 空間が行ごとなので)。
    pub fn move_launcher_cells(&mut self, moves: &[LauncherCellMove], mode: LauncherDropMode) {
        if moves.is_empty() {
            return;
        }
        let moves = moves.to_vec();
        let mut made: Vec<LauncherCellKey> = Vec::new();
        self.edit_song_checked(|song| {
            // **2 パスで処理する。**
            // 1 パス目で受理できる move だけを選び、`Move` なら元セルを *先に全部*
            // 抜き取る。1 件ずつ「置いてから消す」と、移動元と移動先が重なる複数
            // 選択のドラッグ (列 1・2 を選んで右へ 1 列) で、先に置いたセルが次の
            // move の元セルを上書きして **選択が 1 つ減る**。
            let mut pending: Vec<(LauncherCellMove, u32)> = Vec::new();
            for m in &moves {
                let Some(from_scene) = scene_of(song, m.from) else {
                    continue; // 元セルが消えている。
                };
                // 掴んだ場所へそのまま落とした = 何も起きない。ここで弾かないと
                // 「id を採り直して置き直す」だけの編集で undo 履歴と `*` が付く。
                if m.from.row() == m.to_row
                    && song.scenes.get(m.to_scene_index).map(|s| s.id) == Some(from_scene)
                {
                    continue;
                }
                // 種別が合わない drop (MIDI セル → オートメーションレーン行) は
                // **列を実体化する前**に弾く (後で弾くと空の列だけが残る)。
                if !row_accepts(matches!(m.from, LauncherCellKey::Track(_)), m.to_row) {
                    continue;
                }
                let dest_scene = song.ensure_scene_at(m.to_scene_index);
                pending.push((*m, dest_scene));
            }
            for (m, dest_scene) in &pending {
                let same_row = m.from.row() == m.to_row;
                let new_id = if mode == LauncherDropMode::Move && same_row {
                    // **同じ行の中の移動は id を保つ** — 採り直すと
                    // `RowPlayback::Launcher { clip_id }` が消えた id を指し、
                    // `normalize_session` が行を停止に落とす (= 鳴っているセルを
                    // 1 列ずらしただけで音が止まる)。
                    match move_cell_to_scene(song, m.from, *dest_scene) {
                        Some(id) => id,
                        None => continue,
                    }
                } else {
                    let Some(id) = transplant_cell(song, m.from, m.to_row, *dest_scene) else {
                        continue;
                    };
                    if mode == LauncherDropMode::Move {
                        remove_cell(song, m.from);
                    }
                    id
                };
                if mode == LauncherDropMode::CopyIndependent {
                    make_cell_content_unique(song, m.to_row, new_id);
                }
                made.push(cell_key_in_row(m.to_row, new_id));
            }
            if made.is_empty() {
                return false;
            }
            song.normalize_session();
            true
        });
        self.set_launcher_cell_selection(&made);
        self.prune_launcher_selection();
    }

    /// アレンジのクリップを**セルへ**運ぶ (帯とレーンを跨ぐドラッグの release)。
    ///
    /// 行き先の列が空きプレースホルダなら `Song::ensure_scene_at` で実体化する。
    /// 落とし先に既にセルがあれば置き換える (`transplant_cell` と同じ「ドロップは
    /// 置き換え」規約)。`mode` は既存のクリップ規約そのまま。
    pub fn drop_clips_to_cells(&mut self, drops: &[ClipToCellDrop], mode: LauncherDropMode) {
        if drops.is_empty() {
            return;
        }
        let drops = drops.to_vec();
        let mut made: Vec<LauncherCellKey> = Vec::new();
        self.edit_song_checked(|song| {
            for d in &drops {
                if let Some(key) = drop_one_clip_to_cell(song, d, mode) {
                    made.push(key);
                }
            }
            if made.is_empty() {
                return false;
            }
            song.normalize_session();
            true
        });
        self.set_launcher_cell_selection(&made);
        self.prune_launcher_selection();
    }

    /// セルを**アレンジのレーンへ**運ぶ。`to_start_beat` は widget が snap 済で渡す。
    ///
    /// アレンジのクリップは列を持たないので、落ちたセルは列から外れる
    /// (`Move` なら元のセルは消える = 主導権を持っていた行は `normalize_session`
    /// が停止へ落とす)。
    pub fn drop_cells_to_arranger(
        &mut self,
        drops: &[CellToArrangerDrop],
        mode: LauncherDropMode,
    ) {
        if drops.is_empty() {
            return;
        }
        let drops = drops.to_vec();
        let mut made: Vec<ClipKey> = Vec::new();
        let mut placed = 0usize;
        self.edit_song_checked(|song| {
            for d in &drops {
                let Some(placed_key) = drop_one_cell_to_arranger(song, d, mode) else {
                    continue;
                };
                if let Some(key) = placed_key {
                    made.push(key);
                }
                if mode == LauncherDropMode::Move {
                    remove_cell(song, d.from);
                }
                placed += 1;
            }
            // 1 件も置けなかったら **編集ではない** — ここで `true` を返すと
            // 種別違いの drop だけで `*` と undo step が付く (他の 5 経路と同じ規約)。
            if placed == 0 {
                return false;
            }
            song.normalize_session();
            true
        });
        if !made.is_empty() {
            self.set_clip_selection(made);
        }
        self.prune_launcher_selection();
    }

    /// 生成したセル群を選択集合にする (複製 / 移動 / 貼り付けの後)。
    pub(crate) fn set_launcher_cell_selection(&mut self, cells: &[LauncherCellKey]) {
        if cells.is_empty() {
            return;
        }
        // click 経路と同じく列選択とは排他。
        self.selection.selected_scene_ids.clear();
        let clips: Vec<ClipKey> = cells
            .iter()
            .filter_map(|c| match c {
                LauncherCellKey::Track(k) => Some(*k),
                LauncherCellKey::Lane(_) => None,
            })
            .collect();
        let autos: Vec<AutomationClipKey> = cells
            .iter()
            .filter_map(|c| match c {
                LauncherCellKey::Lane(k) => Some(*k),
                LauncherCellKey::Track(_) => None,
            })
            .collect();
        // click 経路 (`select_launcher_cell`) と同じくカーソルトラックを追従させる。
        // 別トラックへセルを複製 / 移動したら、 インスペクタもその行に付いていく。
        // anchor は「最後に置いたセル」 = 集合の末尾で、 automation が後勝ち
        // (下の `last_edit_select` の優先順と同じ)。
        let anchor_track =
            autos.last().map(|k| k.track).or_else(|| clips.last().map(|k| k.track_id));
        if !clips.is_empty() {
            self.selection.selected_clip = clips.last().copied();
            self.selection.selected_clips = clips;
            self.selection.last_edit_select = Some(crate::app::EditSurface::Clips);
        }
        if !autos.is_empty() {
            self.selection.selected_automation_clips = autos;
            self.selection.last_edit_select = Some(crate::app::EditSurface::AutomationClips);
        }
        if let Some(track_id) = anchor_track {
            self.select_track(track_id);
        }
    }

    // ------------------------------------------------------------------
    // ローンチ設定 (インスペクタ)
    // ------------------------------------------------------------------

    /// 選択セル群のローンチ設定を一括で変える。
    pub fn set_launch_settings(&mut self, cells: &[LauncherCellKey], edit: LaunchEdit) {
        if cells.is_empty() {
            return;
        }
        let cells = cells.to_vec();
        self.edit_song_checked(|song| {
            let mut changed = false;
            for cell in &cells {
                let Some(launch) = launch_settings_mut(song, *cell) else {
                    continue;
                };
                let before = launch.clone();
                edit.apply(launch);
                changed |= *launch != before;
            }
            changed
        });
    }

    /// 複数選択で値が割れているかを畳む (インスペクタの `—` 表示用)。
    /// 全部同じなら `Some(値)`、割れていれば / 選択が空なら `None`。
    #[must_use]
    pub fn launch_fold<T: PartialEq>(
        &self,
        cells: &[LauncherCellKey],
        extract: impl Fn(&LaunchSettings) -> T,
    ) -> Option<T> {
        let mut it = cells.iter().filter_map(|c| self.launch_settings_of(*c));
        let first = extract(&it.next()?);
        for s in it {
            if extract(&s) != first {
                return None;
            }
        }
        Some(first)
    }

    /// 選択セルの長さ (拍)。値が割れていれば `None` ([`Self::launch_fold`] の長さ版
    /// — 長さは `LaunchSettings` ではなく `Clip` 側にあるので別関数)。
    #[must_use]
    pub fn launch_cell_length_fold(&self, cells: &[LauncherCellKey]) -> Option<f64> {
        let song = self.song_doc.song();
        let len_of = |c: &LauncherCellKey| -> Option<f64> {
            match c {
                LauncherCellKey::Track(k) => song
                    .track_by_id(k.track_id)
                    .and_then(|t| t.session_clip_by_id(k.clip_id))
                    .map(|s| s.clip.length_beats),
                LauncherCellKey::Lane(k) => song
                    .automation_lane_by_key(k.track, k.lane)
                    .and_then(|l| l.session_clips.iter().find(|c| c.clip.id == k.clip))
                    .map(|s| s.clip.length_beats),
            }
        };
        let mut it = cells.iter().filter_map(len_of);
        let first = it.next()?;
        for v in it {
            if (v - first).abs() > f64::EPSILON {
                return None;
            }
        }
        Some(first)
    }
}

// ----------------------------------------------------------------------
// Song 上の純粋操作 (`edit_song` の closure から呼ぶ)
// ----------------------------------------------------------------------

/// 行 `row` の表示順 `scene_index` にセルを **置けるか**。
///
/// 「列を実体化する前に判定する」ためだけの述語。行が実在し、かつその位置に
/// まだセルが無ければ `true` (プレースホルダ列 = まだ実体の無い列は常に空)。
fn can_place_cell(song: &Song, row: LauncherRow, scene_index: usize) -> bool {
    let row_exists = match row {
        LauncherRow::Track(id) => song.track_by_id(id).is_some(),
        LauncherRow::Lane(k) => song.automation_lane_by_key(k.track, k.lane).is_some(),
    };
    if !row_exists {
        return false;
    }
    let Some(scene) = song.scenes.get(scene_index) else {
        return true; // プレースホルダ列 (これから作る) は必ず空。
    };
    match row {
        LauncherRow::Track(id) => {
            song.track_by_id(id).is_some_and(|t| t.session_clip(scene.id).is_none())
        }
        LauncherRow::Lane(k) => song
            .automation_lane_by_key(k.track, k.lane)
            .is_some_and(|l| l.session_clip(scene.id).is_none()),
    }
}

/// セルと行の種別が対応しているか (トラック行のセルはトラック行にしか置けない)。
/// **列を実体化する前**に弾くために使う。
fn row_accepts(cell_is_track_row: bool, row: LauncherRow) -> bool {
    matches!(
        (cell_is_track_row, row),
        (true, LauncherRow::Track(_)) | (false, LauncherRow::Lane(_))
    )
}

/// セルが乗っている列。
fn scene_of(song: &Song, cell: LauncherCellKey) -> Option<u32> {
    match cell {
        LauncherCellKey::Track(k) => song
            .track_by_id(k.track_id)
            .and_then(|t| t.session_clip_by_id(k.clip_id))
            .map(|c| c.scene_id),
        LauncherCellKey::Lane(k) => song
            .automation_lane_by_key(k.track, k.lane)
            .and_then(|l| l.session_clips.iter().find(|c| c.clip.id == k.clip))
            .map(|c| c.scene_id),
    }
}

/// 行 + 行内の clip id → セル key。行の種別とセルの種別は必ず対応するので、
/// **この 1 本を通す**こと (2 か所で組み立てると片方だけ型を間違える)。
pub(crate) fn cell_key_in_row(row: LauncherRow, clip_id: u32) -> LauncherCellKey {
    match row {
        LauncherRow::Track(track_id) => {
            LauncherCellKey::Track(ClipKey { track_id, clip_id })
        }
        LauncherRow::Lane(k) => LauncherCellKey::Lane(AutomationClipKey {
            track: k.track,
            lane: k.lane,
            clip: clip_id,
        }),
    }
}

/// セルの [`LaunchSettings`] を可変で引く。
fn launch_settings_mut(song: &mut Song, cell: LauncherCellKey) -> Option<&mut LaunchSettings> {
    match cell {
        LauncherCellKey::Track(k) => song
            .track_by_id_mut(k.track_id)
            .and_then(|t| t.session_clips.iter_mut().find(|c| c.clip.id == k.clip_id))
            .map(|c| &mut c.launch),
        LauncherCellKey::Lane(k) => song
            .automation_lane_by_key_mut(k.track, k.lane)
            .and_then(|l| l.session_clips.iter_mut().find(|c| c.clip.id == k.clip))
            .map(|c| &mut c.launch),
    }
}

/// セルを 1 つ消す。消えたら `true`。
fn remove_cell(song: &mut Song, cell: LauncherCellKey) -> bool {
    match cell {
        LauncherCellKey::Track(k) => song.track_by_id_mut(k.track_id).is_some_and(|t| {
            let before = t.session_clips.len();
            t.session_clips.retain(|c| c.clip.id != k.clip_id);
            t.session_clips.len() != before
        }),
        LauncherCellKey::Lane(k) => song
            .automation_lane_by_key_mut(k.track, k.lane)
            .is_some_and(|l| {
                let before = l.session_clips.len();
                l.session_clips.retain(|c| c.clip.id != k.clip);
                l.session_clips.len() != before
            }),
    }
}

/// 行 `row` の列 `scene_id` に空セルを作る。既にあれば `None`。
fn create_cell_in(song: &mut Song, row: LauncherRow, scene_id: u32) -> Option<LauncherCellKey> {
    let content_id = song.alloc_content_id();
    // 可変借用の前に長さを決める (拍子は `song` から読む)。
    let length_beats = new_cell_length_beats(song);
    match row {
        LauncherRow::Track(track_id) => {
            let track = song.track_by_id_mut(track_id)?;
            if track.session_clip(scene_id).is_some() {
                return None;
            }
            // 歌唱トラックのセルは声を引き継ぐ (アレンジの新規クリップと同じ 1 本)。
            let (speaker_id, singer_name, style_name) =
                crate::handler::clips::inherited_voice(track);
            let id = track.alloc_clip_id();
            track.session_clips.push(SessionClip {
                scene_id,
                clip: Clip {
                    id,
                    start_beat: 0.0,
                    length_beats,
                    content_id,
                    speaker_id,
                    singer_name,
                    style_name,
                    ..Clip::default()
                },
                launch: LaunchSettings::default(),
            });
            song.clip_contents.insert(content_id, ClipContent::default());
            Some(LauncherCellKey::Track(ClipKey { track_id, clip_id: id }))
        }
        LauncherRow::Lane(k) => {
            let lane = song.automation_lane_by_key_mut(k.track, k.lane)?;
            if lane.session_clip(scene_id).is_some() {
                return None;
            }
            let id = lane.alloc_clip_id();
            lane.session_clips.push(SessionAutomationClip {
                scene_id,
                clip: AutomationClip {
                    id,
                    start_beat: 0.0,
                    length_beats,
                    content_id,
                    ..AutomationClip::default()
                },
                launch: LaunchSettings::default(),
            });
            song.clip_contents.insert(
                content_id,
                ClipContent::Automation(common::model::AutomationContent::default()),
            );
            Some(LauncherCellKey::Lane(AutomationClipKey {
                track: k.track,
                lane: k.lane,
                clip: id,
            }))
        }
    }
}

/// 同じ行の中でセルを列 `dest_scene` へ動かす。**id を保つ** ので、その行が
/// そのセルを鳴らしていれば鳴り続ける (`RowPlayback` を貼り替えなくてよい)。
/// 落とし先に別のセルが居れば退ける (ドロップは「置き換え」)。戻り値 = 動かした id。
fn move_cell_to_scene(song: &mut Song, cell: LauncherCellKey, dest_scene: u32) -> Option<u32> {
    match cell {
        LauncherCellKey::Track(k) => {
            let track = song.track_by_id_mut(k.track_id)?;
            let moved = track.session_clips.iter().position(|c| c.clip.id == k.clip_id)?;
            let mut payload = track.session_clips.remove(moved);
            payload.scene_id = dest_scene;
            track.put_session_clip(payload);
            Some(k.clip_id)
        }
        LauncherCellKey::Lane(k) => {
            let lane = song.automation_lane_by_key_mut(k.track, k.lane)?;
            let moved = lane.session_clips.iter().position(|c| c.clip.id == k.clip)?;
            let mut payload = lane.session_clips.remove(moved);
            payload.scene_id = dest_scene;
            lane.put_session_clip(payload);
            Some(k.clip)
        }
    }
}

/// `from` のセルを `to_row` の列 `dest_scene` へ **写す** (元は消さない)。
/// 行を跨ぐので id は落とし先の行で採り直す。落とし先に既にセルがあれば
/// 上書きする (ドロップは「置き換え」が自然)。戻り値 = 新しい clip id。
fn transplant_cell(
    song: &mut Song,
    from: LauncherCellKey,
    to_row: LauncherRow,
    dest_scene: u32,
) -> Option<u32> {
    if from.row() == to_row {
        return crate::handler::launcher::clone_cell_into_scene(song, from, dest_scene);
    }
    match (from, to_row) {
        (LauncherCellKey::Track(k), LauncherRow::Track(dest_track)) => {
            let src = song
                .track_by_id(k.track_id)
                .and_then(|t| t.session_clip_by_id(k.clip_id))?
                .clone();
            let track = song.track_by_id_mut(dest_track)?;
            let id = track.alloc_clip_id();
            track.put_session_clip(SessionClip {
                scene_id: dest_scene,
                clip: Clip { id, start_beat: 0.0, ..src.clip },
                launch: src.launch,
            });
            Some(id)
        }
        (LauncherCellKey::Lane(k), LauncherRow::Lane(dest)) => {
            let src = song
                .automation_lane_by_key(k.track, k.lane)
                .and_then(|l| l.session_clips.iter().find(|c| c.clip.id == k.clip))?
                .clone();
            let lane = song.automation_lane_by_key_mut(dest.track, dest.lane)?;
            let id = lane.alloc_clip_id();
            lane.put_session_clip(SessionAutomationClip {
                scene_id: dest_scene,
                clip: AutomationClip { id, start_beat: 0.0, ..src.clip },
                launch: src.launch,
            });
            Some(id)
        }
        // 種別が違う行への drop (MIDI セル → オートメーションレーン行 など) は
        // 中身の意味が変わるので受けない。
        _ => None,
    }
}

/// アレンジのクリップ 1 つをセルへ運ぶ。置けたら新しいセルの key。
fn drop_one_clip_to_cell(
    song: &mut Song,
    d: &ClipToCellDrop,
    mode: LauncherDropMode,
) -> Option<LauncherCellKey> {
    // アレンジの (MIDI / オーディオ) クリップが行けるのはトラック行だけ。
    let LauncherRow::Track(dest_track) = d.to_row else {
        return None;
    };
    let src = song.clip_by_key(d.from).cloned()?;
    let dest_scene = song.ensure_scene_at(d.to_scene_index);
    let track = song.track_by_id_mut(dest_track)?;
    let id = track.alloc_clip_id();
    track.put_session_clip(SessionClip {
        scene_id: dest_scene,
        // セルは「先頭から鳴らす」ので開始拍は捨てる (窓 = そのまま)。
        clip: Clip { id, start_beat: 0.0, ..src },
        launch: LaunchSettings::default(),
    });
    match mode {
        LauncherDropMode::Move => {
            if let Some(t) = song.track_by_id_mut(d.from.track_id) {
                t.remove_clip_by_id(d.from.clip_id);
            }
        }
        LauncherDropMode::CopyLinked => {}
        LauncherDropMode::CopyIndependent => {
            make_cell_content_unique(song, d.to_row, id);
        }
    }
    Some(LauncherCellKey::Track(ClipKey { track_id: dest_track, clip_id: id }))
}

/// セル 1 つをアレンジのレーンへ運ぶ。
/// `None` = 置けなかった (種別違い / 行やセルが無い)。
/// `Some(None)` = 置けたが選択に載せる key は無い (オートメーションクリップ)。
fn drop_one_cell_to_arranger(
    song: &mut Song,
    d: &CellToArrangerDrop,
    mode: LauncherDropMode,
) -> Option<Option<ClipKey>> {
    let start = d.to_start_beat.max(0.0);
    match (d.from, d.to_row) {
        (LauncherCellKey::Track(from), LauncherRow::Track(dest_track)) => {
            let src = song
                .track_by_id(from.track_id)
                .and_then(|t| t.session_clip_by_id(from.clip_id))
                .map(|c| c.clip.clone())?;
            let track = song.track_by_id_mut(dest_track)?;
            let id = track.alloc_clip_id();
            track.clips.push(Clip { id, start_beat: start, ..src });
            if mode == LauncherDropMode::CopyIndependent {
                fork_clip_content(song, ClipKey { track_id: dest_track, clip_id: id });
            }
            Some(Some(ClipKey { track_id: dest_track, clip_id: id }))
        }
        (LauncherCellKey::Lane(from), LauncherRow::Lane(dest)) => {
            let src = song
                .automation_lane_by_key(from.track, from.lane)
                .and_then(|l| l.session_clips.iter().find(|c| c.clip.id == from.clip))
                .map(|c| c.clip.clone())?;
            let lane = song.automation_lane_by_key_mut(dest.track, dest.lane)?;
            let id = lane.alloc_clip_id();
            lane.clips.push(AutomationClip { id, start_beat: start, ..src });
            if mode == LauncherDropMode::CopyIndependent {
                fork_automation_clip_content(song, dest, id);
            }
            Some(None)
        }
        // 種別が違う行への drop は受けない (`row_accepts` と同じ規約)。
        _ => None,
    }
}

/// クリップの content を fork して独立させる (`Make Unique` と同じ 1 式)。
/// [`Track::clip_by_id_mut`](common::model::Track::clip_by_id_mut) がアレンジの
/// クリップとセルの両方を引くので、**どちらに落ちた先でも同じ関数**で済む。
fn fork_clip_content(song: &mut Song, key: ClipKey) {
    let Some(old) = song.clip_by_key(key).map(|c| c.content_id) else {
        return;
    };
    let new_id = song.fork_content(old);
    if let Some(clip) = song.clip_by_key_mut(key) {
        clip.content_id = new_id;
    }
}

/// オートメーションクリップの content を fork して独立させる
/// ([`fork_clip_content`] のレーン版)。
fn fork_automation_clip_content(song: &mut Song, lane_key: AutomationLaneKey, clip_id: u32) {
    let Some(old) = song
        .automation_lane_by_key(lane_key.track, lane_key.lane)
        .and_then(|l| l.clips.iter().find(|c| c.id == clip_id))
        .map(|c| c.content_id)
    else {
        return;
    };
    let new_id = song.fork_content(old);
    if let Some(clip) = song
        .automation_lane_by_key_mut(lane_key.track, lane_key.lane)
        .and_then(|l| l.clips.iter_mut().find(|c| c.id == clip_id))
    {
        clip.content_id = new_id;
    }
}

/// セルの中身を **独立コピー**にする (`Ctrl+Shift` ドロップ / `Alt+D` 複製)。
/// `content_id` を採り直して中身を複製するので、以後の編集は元と連動しない。
fn make_cell_content_unique(song: &mut Song, row: LauncherRow, clip_id: u32) {
    let Some(old) = (match row {
        LauncherRow::Track(id) => song
            .track_by_id(id)
            .and_then(|t| t.session_clip_by_id(clip_id))
            .map(|c| c.clip.content_id),
        LauncherRow::Lane(k) => song
            .automation_lane_by_key(k.track, k.lane)
            .and_then(|l| l.session_clips.iter().find(|c| c.clip.id == clip_id))
            .map(|c| c.clip.content_id),
    }) else {
        return;
    };
    let content = song.clip_contents.get(&old).cloned().unwrap_or_default();
    let name = song.clip_content_names.get(&old).cloned();
    let new_id = song.alloc_content_id();
    song.clip_contents.insert(new_id, content);
    if let Some(n) = name {
        song.clip_content_names.insert(new_id, n);
    }
    match row {
        LauncherRow::Track(id) => {
            if let Some(t) = song.track_by_id_mut(id)
                && let Some(c) = t.session_clips.iter_mut().find(|c| c.clip.id == clip_id)
            {
                c.clip.content_id = new_id;
            }
        }
        LauncherRow::Lane(k) => {
            if let Some(l) = song.automation_lane_by_key_mut(k.track, k.lane)
                && let Some(c) = l.session_clips.iter_mut().find(|c| c.clip.id == clip_id)
            {
                c.clip.content_id = new_id;
            }
        }
    }
}

/// 行 `row` で `from` より右の、最初に空いている列 index。
/// 全部埋まっていれば「実シーン数」 (= 末尾に 1 列足す位置) を返す。
fn free_scene_index_after(song: &Song, row: LauncherRow, from: usize) -> usize {
    let occupied = |scene_id: u32| -> bool {
        match row {
            LauncherRow::Track(id) => song
                .track_by_id(id)
                .is_some_and(|t| t.session_clip(scene_id).is_some()),
            LauncherRow::Lane(k) => song
                .automation_lane_by_key(k.track, k.lane)
                .is_some_and(|l| l.session_clip(scene_id).is_some()),
        }
    };
    for (i, scene) in song.scenes.iter().enumerate().skip(from + 1) {
        if !occupied(scene.id) {
            return i;
        }
    }
    song.scenes.len()
}
