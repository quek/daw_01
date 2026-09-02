//! handler::launcher_cells — r.md #87 ランチャーの **セル**の CRUD とローンチ設定。
//!
//! 発火 / 行の主導権 / 列 (シーン) の CRUD は [`crate::handler::launcher`]。
//! 分けてあるのは実コード 1,000 行 budget (不変条件 9) のため。
//!
//! ## セルは arrangement のクリップと同じ addressing
//!
//! セルの `clip.id` は `clips` と **同じ id 空間**から採るので、
//! `ClipKey { track_id, clip_id }` / `AutomationClipKey` がそのまま通る
//! (計画書 §3.3)。**1 つの key はアレンジのクリップかセルのどちらか一方**なので、
//! 削除 / 色 / 名前などは「両方の入れ物を key で引く」だけで曖昧にならない。
//!
//! ただし **選択集合は共有しない**。セルは
//! [`SelectionState::selected_launcher_cells`](crate::state::SelectionState::selected_launcher_cells)
//! 1 本 ([`LauncherCellKey`] = トラック行 / レーン行の両方) を持ち、面タグも
//! [`EditSurface::LauncherCells`](crate::app::EditSurface::LauncherCells) 1 つ。
//! 鍵の型を共有できること (= 引ける) と、集合を共有してよいこと (= 面が同じ) は別。
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
    ArrangementClipRef, CellToArrangerDrop, ClipToCellDrop, LaunchEdit, LauncherCellKey,
    LauncherCellMove, LauncherDropMode, LauncherRow,
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

/// 長さを書き換えて「実際に変わったか」を返す (変わらないなら `*` も undo step も
/// 積まない)。[`resize_cell`] のトラック行 / レーン行の 2 分岐が同じ判定を持つための 1 本。
fn set_len(slot: &mut f64, beats: f64) -> bool {
    if (*slot - beats).abs() <= f64::EPSILON {
        return false;
    }
    *slot = beats;
    true
}

/// セル 1 つの長さを `beats` にする。実際に変わったら
/// `(content_id, 窓の末尾)` を返す (呼び側が overlay の event 長を補完する)。
fn resize_cell(
    song: &mut Song,
    cell: LauncherCellKey,
    beats: f64,
) -> Option<(common::model::ContentId, f64)> {
    match cell {
        LauncherCellKey::Track(k) => {
            let c = song
                .track_by_id_mut(k.track_id)?
                .session_clips
                .iter_mut()
                .find(|c| c.clip.id == k.clip_id)?;
            set_len(&mut c.clip.length_beats, beats)
                .then_some((c.clip.content_id, c.clip.content_offset_beats + beats))
        }
        LauncherCellKey::Lane(k) => {
            let c = song
                .automation_lane_by_key_mut(k.track, k.lane)?
                .session_clips
                .iter_mut()
                .find(|c| c.clip.id == k.clip)?;
            set_len(&mut c.clip.length_beats, beats)
                .then_some((c.clip.content_id, c.clip.content_offset_beats + beats))
        }
    }
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

    /// 選択集合に居る **実在するセル**。面タグは見ない。
    ///
    /// 呼んでよいのは「対象面が既にセル面だと分かっている」場所だけ
    /// (`delete_current_surface` が解決した後の削除経路など)。
    /// それ以外は [`Self::selected_launcher_cells`] を使う。
    #[must_use]
    pub fn live_launcher_cells(&self) -> Vec<LauncherCellKey> {
        let song = self.song_doc.song();
        self.selection
            .selected_launcher_cells
            .iter()
            .copied()
            .filter(|cell| cell_exists(song, *cell))
            .collect()
    }

    /// 選択集合に **実在するセルが 1 つでも居るか** ([`Self::live_launcher_cells`] の
    /// 非空判定を確保なしで)。`edit_surface` が毎フレーム引くのでここは短絡させる。
    #[must_use]
    pub fn has_live_launcher_cells(&self) -> bool {
        let song = self.song_doc.song();
        self.selection.selected_launcher_cells.iter().any(|c| cell_exists(song, *c))
    }

    /// いま選択されている **セル** (アレンジのクリップは除く)。
    ///
    /// セル面は行の種類に関わらず [`EditSurface::LauncherCells`](crate::app::EditSurface::LauncherCells)
    /// 1 面なので、**直近に確定した面がセル面のときだけ**返す。ここを
    /// 「別の面でも中身が非空なら返す」にすると、ピアノロールでノートを選んだ
    /// 状態の `Ctrl+C` / `Ctrl+X` / `D` が (セルが選択に残っているだけで)
    /// ノートではなくセルに効いてしまう — `Ctrl+X` は消えるので実害が大きい
    /// (`feedback_selection_action_last_wins` の "直近確定面で決める")。
    #[must_use]
    pub fn selected_launcher_cells(&self) -> Vec<LauncherCellKey> {
        if self.selection.last_edit_select != Some(crate::app::EditSurface::LauncherCells) {
            return Vec::new();
        }
        self.live_launcher_cells()
    }

    /// **オブジェクト選択の排他**: 範囲がアレンジの行 ([`LaneRef::is_arrangement_row`])
    /// を持ったら、ランチャーのセル選択を降ろす。
    ///
    /// 選択されているクリップは常に 1 つの面だけ (Live: "The Clip View always shows
    /// the currently selected clip")。 セル面とアレンジ面が同時に立てると、
    /// ピアノロールがどちらを映すかを last-wins タグ頼みで決めることになり、
    /// **セルを開いたままピアノロール内をクリックした瞬間にタグがアレンジへ倒れて
    /// エディタが空になる** (r.md #90)。
    ///
    /// 逆向き (セルを選んだら範囲を降ろす) は `select_launcher_cell` 側。
    /// 鍵盤行 / 波形行だけの範囲 (= エディタ内でノート・イベントを選んだ状態) では
    /// 降ろさない — それはアレンジの面を選ぶ操作ではない。
    ///
    /// **範囲を書き換える経路はすべてここを通すこと。** 現在値から毎回判定するので
    /// 冪等で、どこから呼んでも安全。
    pub(crate) fn drop_cell_selection_if_arrangement(&mut self) {
        let arrangement = self
            .selection
            .time
            .as_ref()
            .is_some_and(|t| t.lanes.iter().copied().any(common::model::LaneRef::is_arrangement_row));
        if arrangement {
            self.selection.selected_launcher_cells.clear();
            self.selection.launcher_cell_anchor = None;
        }
    }

    /// セルの長さ (= ループ長) を変える。選択している全セルへ一括で効く。
    ///
    /// セルの `start_beat` は常に 0 なので、変わるのは `length_beats` だけ
    /// (窓の開始 `content_offset_beats` は据え置き = 「どこから鳴らすか」は
    /// 変えずに「どこまで鳴らすか」だけを変える)。
    ///
    /// アレンジの `resize_clip` と同じく、伸ばしたあとは overlay (画像 / 映像 /
    /// 字幕) の末尾 event を窓の末尾まで伸ばす — 「クリップ長 = 表示長」が
    /// overlay の不変条件で、伸ばした分が空白になると**セルの後半だけ絵が消える**。
    /// extend-only なので共有 content を伸ばしてもリンク先は自分の窓で clamp される
    /// ([`ClipContent::ensure_event_covers_clip`](common::model::ClipContent::ensure_event_covers_clip) の契約)。
    pub fn set_cell_length(&mut self, cells: &[LauncherCellKey], beats: f64) {
        if cells.is_empty() || !beats.is_finite() {
            return;
        }
        let beats = beats.max(common::model::MIN_CLIP_LEN_BEATS);
        let cells = cells.to_vec();
        self.edit_song_checked(|song| {
            // content の可変借用がセルの可変借用と重なるので、先に
            // 「(content_id, 窓の末尾)」を集めてから content を触る。
            let covers: Vec<(common::model::ContentId, f64)> =
                cells.iter().filter_map(|c| resize_cell(song, *c, beats)).collect();
            if covers.is_empty() {
                return false;
            }
            for (content_id, window_end) in covers {
                if let Some(content) = song.clip_contents.get_mut(&content_id) {
                    content.ensure_event_covers_clip(window_end);
                }
            }
            true
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
        // アレンジの範囲選択とも排他 (`drop_cell_selection_if_arrangement` の逆向き)。
        // 選択されているクリップは常に 1 つの面だけなので、セルを選んだらアレンジの
        // 範囲は降りる。 残すとインスペクタ / ピアノロールが 2 つの面を同時に指す。
        self.set_time_selection(None);
        // **行の種類で分岐しない** — セル面は 1 面 1 集合なので、トラック行と
        // レーン行を跨ぐ Shift+click の長方形もそのまま解ける (分けていた頃は
        // 「トラック行のセルからその下のオートメーションレーン行のセルまで」が
        // 範囲選択できなかった)。
        let items = self.launcher_range_items();
        let anchor = self.selection.launcher_cell_anchor;
        let next = modifier.resolve(&self.selection.selected_launcher_cells, cell, || {
            let a = anchor?;
            crate::widgets::select_modifier::range_block(&items, a, cell)
        });
        let anchor_track = next.last().map(|k| k.row().track_id());
        self.selection.selected_launcher_cells = next;
        if modifier.updates_anchor() {
            self.selection.launcher_cell_anchor = Some(cell);
        }
        if !self.selection.selected_launcher_cells.is_empty() {
            self.selection.last_edit_select = Some(crate::app::EditSurface::LauncherCells);
        }
        // アレンジのクリップ選択 (`select_clip` / `set_clip_selection`) と
        // 同じく、 anchor のトラックへカーソルを追従させる。 これが無いと
        // インスペクタの上半分 (トラック名・色・デバイスチェーン) だけが
        // 前のトラックのまま残り、 下半分 (クリップ / ローンチ設定) と
        // 混ざって見える。 面タグを立てた **直後**に呼ぶ約束も同じ。
        // レーン行のセルも同じ — レーンは必ずどれか 1 トラックに属する。
        if let Some(track_id) = anchor_track {
            self.select_track(track_id);
        }
        // 撃つ / 設定を出す起点をクリックしたセルへ移す。
        if let (Some(scene_id), row) = (self.scene_of_cell(cell), cell.row())
            && let Some(scene_index) = self.song_doc.song().scene_index(scene_id)
        {
            self.launcher.focus = Some(crate::state::LauncherFocus { row, scene_index });
        }
        // アレンジのクリップ選択 (`apply_clip_range`) と同じく、**初めて開く**セルはピアノロールを
        // auto-fit する (per-clip view の記憶が無いときだけ。記憶があれば復元に任せ、明示的な
        // 再 fit は `X`)。単一のトラック行 MIDI セルだけ — 複数表示は共有 viewport なので
        // 選ぶたびに飛ばさない。
        if let [LauncherCellKey::Track(k)] = self.selection.selected_launcher_cells.as_slice()
            && self.is_midi_clip(*k)
            && !self.ui_prefs.piano_roll_views.contains_key(k)
        {
            self.fit_piano_roll_to_clip();
        }
    }

    /// Shift+click の範囲計算に渡す「行 × 列」のグリッド。
    /// `row` は表示順の行 index、時間軸には **列 index** を入れる
    /// (セルは全部 `start_beat = 0` なので拍では並ばない)。
    ///
    /// **トラック行のセルもレーン行のセルも同じ 1 本の表に載せる。** 種類ごとに
    /// 分けると「トラック行のセルから、その下のオートメーションレーン行のセルまで」
    /// の長方形が解けず、Shift+click が片方の種類でしか効かない。
    ///
    /// 行の数え方は [`Self::launcher_rows`] (= 画面に出ている行だけ)。
    /// `all_launcher_rows` で数えると、長方形が **畳んで見えていない行のセルまで
    /// 飲み込み**、続く Delete が見ていないセルを消す。
    fn launcher_range_items(
        &self,
    ) -> Vec<crate::widgets::select_modifier::RangeItem<LauncherCellKey>> {
        let song = self.song_doc.song();
        let mut items = Vec::new();
        for (row_i, row) in self.launcher_rows().into_iter().enumerate() {
            // 行の種別ごとの「セルの列 id と clip id」だけが違う。
            let cells: Vec<(u32, u32)> = match row {
                LauncherRow::Track(track_id) => song
                    .track_by_id(track_id)
                    .map(|t| t.session_clips.iter().map(|c| (c.scene_id, c.clip.id)).collect())
                    .unwrap_or_default(),
                LauncherRow::Lane(lane_key) => song
                    .automation_lane_by_key(lane_key.track, lane_key.lane)
                    .map(|l| l.session_clips.iter().map(|c| (c.scene_id, c.clip.id)).collect())
                    .unwrap_or_default(),
            };
            for (scene_id, clip_id) in cells {
                let Some(col) = song.scene_index(scene_id) else {
                    continue;
                };
                items.push(crate::widgets::select_modifier::RangeItem {
                    key: cell_key_in_row(row, clip_id),
                    row: row_i as i64,
                    start: col as f64,
                    end: col as f64 + 1.0,
                });
            }
        }
        items
    }

    /// 消えたセル / 列を選択集合から落とす (列削除 / セル削除の後始末)。
    ///
    /// **アレンジの automation クリップ選択 (`selected_automation_clips`) は
    /// 触らない** — セルは自分の集合を持つので、ここで他面の集合を掃除する理由が
    /// 無い (掃除していた頃は「セル面が automation 面へ相乗りしている」ことの
    /// 帳尻合わせだった)。
    pub(crate) fn prune_launcher_selection(&mut self) {
        let song = self.song_doc.song();
        // 消えた列を指したままだと、インスペクタが存在しない列の設定を出す。
        let live_scenes: Vec<u32> = song.scenes.iter().map(|s| s.id).collect();
        self.selection.selected_scene_ids.retain(|id| live_scenes.contains(id));
        self.selection.scene_anchor =
            self.selection.scene_anchor.filter(|id| live_scenes.contains(id));
        let song = self.song_doc.song();
        let alive = |k: &LauncherCellKey| cell_exists(song, *k);
        let cells: Vec<LauncherCellKey> =
            self.selection.selected_launcher_cells.iter().copied().filter(alive).collect();
        // **範囲選択の起点 (anchor) も掃除する。** 消えたセルを指したままだと
        // 次の `Shift+click` が範囲を解けず、単一選択に落ちる (「範囲選択が
        // 時々効かない」の正体)。
        self.selection.launcher_cell_anchor =
            self.selection.launcher_cell_anchor.filter(alive);
        self.selection.selected_launcher_cells = cells;
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
            //
            // 1 パス目で受理できる move を選び、その場で中身を [`CellPayload`] へ
            // 退避する (`Move` なら元セルを抜き取る)。2 パス目は退避した中身だけを
            // 置くので **`song` のセル置き場を一切読まない**。
            //
            // 1 件ずつ「読んで置く」と、移動元と移動先が重なる複数選択のドラッグ
            // (列 1・2 を選んで右へ 1 列) で壊れる —
            // [`Track::put_session_clip`](common::model::Track::put_session_clip) は
            // 落とし先の既存セルを捨てるので、先に置いた列 1→2 が **まだ動かして
            // いない列 2 のセルを消し**、その後の列 2→3 は読むものが無くなって
            // セルが 1 つ消滅する。**順序で回避しない** — 右送りを順序で直しても
            // 左送りで再発するので、「置く側が読まない」形にして構造的に潰す。
            let mut pending: Vec<(LauncherCellMove, u32, CellPayload)> = Vec::new();
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
                // 種別が合わない drop (MIDI セル → オートメーションレーン行) と、
                // そもそもセルを置けない行への drop は **列を実体化する前**に弾く
                // (後で弾くと空の列だけが残る)。抜き取りより先に弾くのも必須 —
                // 置けない先へ運ぼうとして元セルだけ消えると、セルが宙に消える。
                if !row_accepts(matches!(m.from, LauncherCellKey::Track(_)), m.to_row)
                    || !row_accepts_cells(song, m.to_row)
                {
                    continue;
                }
                let Some(payload) = clone_cell_payload(song, m.from) else {
                    continue;
                };
                if mode == LauncherDropMode::Move {
                    remove_cell(song, m.from);
                }
                let dest_scene = song.ensure_scene_at(m.to_scene_index);
                pending.push((*m, dest_scene, payload));
            }
            // **編集したかは `pending` で判定する。** 1 パス目は受理した時点で
            // 元セルを抜き取り列を実体化して *もう song を変えている* ので、
            // 2 パス目の成否 (`made`) で `false` を返すと `edit_song_checked` が
            // snapshot だけ捨て、抜き取ったセルが undo できずに消える。
            if pending.is_empty() {
                return false;
            }
            for (m, dest_scene, payload) in pending {
                // **同じ行の中の移動は id を保つ** — 採り直すと
                // `RowPlayback::Launcher { clip_id }` が消えた id を指し、
                // `normalize_session` が行を停止に落とす (= 鳴っているセルを
                // 1 列ずらしただけで音が止まる)。行を跨いだ移動とコピーは
                // 落とし先の行で採り直す (id 空間が行ごとなので)。
                let keep_id = (mode == LauncherDropMode::Move && m.from.row() == m.to_row)
                    .then(|| m.from.clip_id());
                let Some(new_id) = place_cell(song, m.to_row, dest_scene, payload, keep_id)
                else {
                    continue;
                };
                if mode == LauncherDropMode::CopyIndependent {
                    make_cell_content_unique(song, m.to_row, new_id);
                }
                made.push(cell_key_in_row(m.to_row, new_id));
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
    /// 落とし先に既にセルがあれば置き換える (`Track::put_session_clip` /
    /// `AutomationLane::put_session_clip` が担う「ドロップは置き換え」規約)。
    /// `mode` は既存のクリップ規約そのまま。
    pub fn drop_clips_to_cells(&mut self, drops: &[ClipToCellDrop], mode: LauncherDropMode) {
        if drops.is_empty() {
            return;
        }
        let drops = drops.to_vec();
        let follow = self.ui_prefs.automation_follows_clips;
        let mut made: Vec<LauncherCellKey> = Vec::new();
        self.edit_song_checked(|song| {
            for d in &drops {
                if let Some(key) = drop_one_clip_with_follow(song, d, mode, follow) {
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
        // click 経路と同じく列選択・アレンジの範囲選択とは排他。
        self.selection.selected_scene_ids.clear();
        self.set_time_selection(None);
        self.selection.selected_launcher_cells = cells.to_vec();
        self.selection.last_edit_select = Some(crate::app::EditSurface::LauncherCells);
        // click 経路 (`select_launcher_cell`) と同じくカーソルトラックを追従させる。
        // 別トラックへセルを複製 / 移動したら、 インスペクタもその行に付いていく。
        // anchor は「最後に置いたセル」 = 集合の末尾。
        if let Some(track_id) = cells.last().map(|c| c.row().track_id()) {
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

/// 行 `row` が **そもそもセルを持てるか**。列とは無関係な、行そのものの性質。
///
/// - 行が実在すること
/// - **グループトラックでないこと** — daw_01 のグループトラックは自分のクリップを
///   鳴らさない (`process_track_owned` が `track_has_children` で pass 1 を抜ける)
///   ので、置いたセルは保存はされるのに永久に鳴らない。帯はグループ行を「子の
///   まとめセル」として描く (計画書 §0 / §3.2)
/// - レーン行なら
///   [`AutomationTarget::accepts_launcher_cells`](common::model::AutomationTarget::accepts_launcher_cells)
///   が `true` であること (テンポ / 拍子レーンはランチャーが握れない — 量子化
///   グリッドが自己参照し、GUI と engine で時間軸が食い違う)
///
/// **セルを置く口はすべてこれを引く** (作成 / 移動 / drop / 貼り付け / 取り込み)。
/// widget 側が rect を出さないことに頼ると、口が 1 つ増えるたびに穴が開く。
pub(crate) fn row_accepts_cells(song: &Song, row: LauncherRow) -> bool {
    match row {
        LauncherRow::Track(id) => {
            song.track_by_id(id).is_some() && !song.track_has_children(id)
        }
        LauncherRow::Lane(k) => song
            .automation_lane_by_key(k.track, k.lane)
            .is_some_and(|l| l.target.accepts_launcher_cells()),
    }
}

/// 行 `row` の表示順 `scene_index` にセルを **置けるか**。
///
/// 「列を実体化する前に判定する」ためだけの述語。行がセルを持てて
/// ([`row_accepts_cells`])、かつその位置にまだセルが無ければ `true`
/// (プレースホルダ列 = まだ実体の無い列は常に空)。
fn can_place_cell(song: &Song, row: LauncherRow, scene_index: usize) -> bool {
    if !row_accepts_cells(song, row) {
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

/// そのセルが **いまも `session_clips` に居るか**。
///
/// 生存確認はこの 1 本だけを通す — 「アレンジのクリップにも居ればよい」 と
/// 緩めると、セルをアレンジのレーンへ運んだ (= セルは消えてクリップになった) 後も
/// 選択にセル key が残り、インスペクタが存在しないセルのローンチ設定を出す。
fn cell_exists(song: &Song, cell: LauncherCellKey) -> bool {
    scene_of(song, cell).is_some()
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

/// セル 1 つの中身。**置く側が `song` を読まないため**の退避入れ物で、
/// [`AppData::move_launcher_cells`] の 1 パス目が作り 2 パス目が消費する
/// (`put_session_clip` が落とし先の既存セルを捨てるので、置きながら読むと
/// まだ動かしていない元セルを潰す)。
enum CellPayload {
    Track(SessionClip),
    Lane(SessionAutomationClip),
}

/// セルの中身を複製して取り出す (元は消さない)。`Move` の呼び側はこの直後に
/// [`remove_cell`] を呼ぶ。
fn clone_cell_payload(song: &Song, cell: LauncherCellKey) -> Option<CellPayload> {
    match cell {
        LauncherCellKey::Track(k) => song
            .track_by_id(k.track_id)
            .and_then(|t| t.session_clip_by_id(k.clip_id))
            .map(|c| CellPayload::Track(c.clone())),
        LauncherCellKey::Lane(k) => song
            .automation_lane_by_key(k.track, k.lane)
            .and_then(|l| l.session_clips.iter().find(|c| c.clip.id == k.clip))
            .map(|c| CellPayload::Lane(c.clone())),
    }
}

/// 退避した中身を行 `to_row` の列 `dest_scene` へ置く。落とし先に既にセルが
/// あれば置き換える (ドロップは「置き換え」規約)。
///
/// `keep_id` が `Some` ならその id のまま置く — 同じ行の中の移動で id を採り直すと
/// `RowPlayback::Launcher { clip_id }` が消えた id を指し、鳴っているセルを 1 列
/// ずらしただけで音が止まる。`None` なら落とし先の行で採り直す (id 空間が行ごと)。
///
/// 中身と行の種別が食い違うときは置かない (呼び側が [`row_accepts`] で先に
/// 弾いているので、ここは防御)。
fn place_cell(
    song: &mut Song,
    to_row: LauncherRow,
    dest_scene: u32,
    payload: CellPayload,
    keep_id: Option<u32>,
) -> Option<u32> {
    match (to_row, payload) {
        (LauncherRow::Track(track_id), CellPayload::Track(mut cell)) => {
            let track = song.track_by_id_mut(track_id)?;
            let id = keep_id.unwrap_or_else(|| track.alloc_clip_id());
            cell.scene_id = dest_scene;
            cell.clip.id = id;
            cell.clip.start_beat = 0.0;
            track.put_session_clip(cell);
            Some(id)
        }
        (LauncherRow::Lane(k), CellPayload::Lane(mut cell)) => {
            let lane = song.automation_lane_by_key_mut(k.track, k.lane)?;
            let id = keep_id.unwrap_or_else(|| lane.alloc_clip_id());
            cell.scene_id = dest_scene;
            cell.clip.id = id;
            cell.clip.start_beat = 0.0;
            lane.put_session_clip(cell);
            Some(id)
        }
        // 種別が違う行への drop (MIDI セル → オートメーションレーン行 など) は
        // 中身の意味が変わるので受けない。
        _ => None,
    }
}

/// [`drop_one_clip_to_cell`] + **オートメーション追従** (`docs/plan_range_selection.md` §5)。
/// 追従は同じトラックの行へ運ぶときだけ (アレンジ内の移動と同じ規則 — 別トラックの行へは、
/// どのレーンへ移すかが一意に決まらない)。窓は Move で消える前に取る。
fn drop_one_clip_with_follow(
    song: &mut Song,
    d: &ClipToCellDrop,
    mode: LauncherDropMode,
    follow: bool,
) -> Option<LauncherCellKey> {
    let follow_window = match (follow, d.from, d.to_row) {
        (true, ArrangementClipRef::Track(from), LauncherRow::Track(dest))
            if dest == from.track_id =>
        {
            song.clip_by_key(from).map(Clip::song_window)
        }
        _ => None,
    };
    let key = drop_one_clip_to_cell(song, d, mode)?;
    if let Some((start, end)) = follow_window
        && let Some(scene) = song.scenes.get(d.to_scene_index).map(|s| s.id)
    {
        super::launcher_cells_automation::carry_track_automation_to_cells(
            song,
            d.to_row.track_id(),
            start,
            end,
            scene,
            mode,
        );
    }
    Some(key)
}

/// アレンジのクリップ 1 つをセルへ運ぶ。置けたら新しいセルの key。
fn drop_one_clip_to_cell(
    song: &mut Song,
    d: &ClipToCellDrop,
    mode: LauncherDropMode,
) -> Option<LauncherCellKey> {
    // セルを置けない行 (グループ / テンポ・拍子レーン) は **列を実体化する前**に弾く。
    if !row_accepts_cells(song, d.to_row) {
        return None;
    }
    // 種別が違う行への drop (MIDI クリップ → レーン行 / オートメーション → トラック行) は
    // 中身の意味が変わるので受けない (`place_cell_payload` と同じ規約)。
    let made = match (d.from, d.to_row) {
        (ArrangementClipRef::Track(from), LauncherRow::Track(dest_track)) => {
            let src = song.clip_by_key(from).cloned()?;
            let dest_scene = song.ensure_scene_at(d.to_scene_index);
            let track = song.track_by_id_mut(dest_track)?;
            let id = track.alloc_clip_id();
            track.put_session_clip(SessionClip {
                scene_id: dest_scene,
                // セルは「先頭から鳴らす」ので開始拍は捨てる (窓 = そのまま)。
                clip: Clip { id, start_beat: 0.0, ..src },
                launch: LaunchSettings::default(),
            });
            if mode == LauncherDropMode::Move
                && let Some(t) = song.track_by_id_mut(from.track_id)
            {
                t.remove_clip_by_id(from.clip_id);
            }
            LauncherCellKey::Track(ClipKey { track_id: dest_track, clip_id: id })
        }
        (ArrangementClipRef::Lane(from), LauncherRow::Lane(dest)) => {
            let src = song
                .automation_lane_by_key(from.track, from.lane)
                .and_then(|l| l.clips.iter().find(|c| c.id == from.clip))
                .cloned()?;
            let dest_scene = song.ensure_scene_at(d.to_scene_index);
            let lane = song.automation_lane_by_key_mut(dest.track, dest.lane)?;
            let id = lane.alloc_clip_id();
            lane.put_session_clip(common::model::SessionAutomationClip {
                scene_id: dest_scene,
                clip: AutomationClip { id, start_beat: 0.0, ..src },
                launch: LaunchSettings::default(),
            });
            if mode == LauncherDropMode::Move
                && let Some(l) = song.automation_lane_by_key_mut(from.track, from.lane)
            {
                l.clips.retain(|c| c.id != from.clip);
            }
            LauncherCellKey::Lane(AutomationClipKey { track: dest.track, lane: dest.lane, clip: id })
        }
        _ => return None,
    };
    if mode == LauncherDropMode::CopyIndependent {
        make_cell_content_unique(song, d.to_row, made.clip_id());
    }
    Some(made)
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
            let id = track.place_clip(Clip { id: 0, start_beat: start, ..src });
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
