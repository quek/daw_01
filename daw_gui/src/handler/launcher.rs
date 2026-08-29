//! handler::launcher — r.md #87 クリップランチャーの **発火 / 行の主導権 / 列 (シーン) の CRUD**。
//!
//! セル側の CRUD とローンチ設定は [`crate::handler::launcher_cells`]。
//! `AppEvent::Launcher(..)` の唯一の dispatcher が [`AppData::handle_launcher_event`]。
//!
//! ## 何を `Song` に書き、何を書かないか (計画書 §1.4)
//!
//! | いつ | `Song` | dirty |
//! |---|---|---|
//! | ユーザーがセル / シーンを撃つ・行を止める・アレンジへ返す | **書き換える** | 立てる |
//! | フォローアクションで次のセルへ移る | 書き換えない (engine の走行状態) | 立てない |
//!
//! 走行位置を保存しないので、書き出しは常に「範囲の先頭で `RowPlayback` を一斉に
//! 撃った」状態から始まり、そこからフォローアクションが決定的に進む
//! (= 同じプロジェクトなら同じファイル、Q9)。
//!
//! 編集は全て [`AppData::edit_song`] チョークポイントを通す (不変条件 5)。
//! `edit_song_checked` を使うのは「同じセルをもう一度撃った」ときに **空の
//! undo step を積まない**ため (押すたびに履歴が伸びると undo が使い物にならない)。

use common::model::{LaunchQuantize, RowPlayback, MASTER_TRACK_ID};

use crate::event_launcher::{
    LauncherAudioCommand, LauncherBinding, LauncherCellKey, LauncherEvent, LauncherRow,
};
use crate::state::{AppData, LauncherFocus};

impl AppData {
    // ------------------------------------------------------------------
    // グローバル設定 / MIDI bind 表 / engine への送信 — **差し替え点**
    // ------------------------------------------------------------------

    /// グローバルローンチ量子化 (トランスポートの dropdown、既定 = 1 小節)。
    ///
    /// 置き場が `UiPrefs` なのは暫定 — モデル層に `Song.global_launch_quantize` が
    /// 入ったらこの 1 対だけを差し替える (`UiPrefs::global_launch_quantize` の doc)。
    #[must_use]
    pub fn global_launch_quantize(&self) -> LaunchQuantize {
        self.ui_prefs.global_launch_quantize
    }

    /// [`Self::global_launch_quantize`] の setter。engine へも即座に伝える。
    pub fn set_global_launch_quantize(&mut self, q: LaunchQuantize) {
        if self.ui_prefs.global_launch_quantize == q {
            return;
        }
        self.ui_prefs.global_launch_quantize = q;
        self.send_launcher_audio(LauncherAudioCommand::SetGlobalLaunchQuantize(q));
    }

    /// MIDI → ランチャー操作の binding 表。
    ///
    /// 読み書きは この 3 本 ([`Self::launcher_bindings`] /
    /// [`Self::add_launcher_binding`] / [`Self::clear_launcher_bindings`]) だけ。
    /// `MidiBinding` がノートを受けられるようになったら中身を
    /// `Song.midi_bindings` へ向け直す。
    #[must_use]
    pub fn launcher_bindings(&self) -> &[LauncherBinding] {
        &self.launcher.bindings
    }

    /// 同じ `(channel, input)` の既存 binding を置き換えて 1 件足す
    /// (`handle_midi_control_change` の Learn と同じ replace 規約)。
    pub fn add_launcher_binding(&mut self, b: LauncherBinding) {
        self.launcher
            .bindings
            .retain(|e| !(e.channel == b.channel && e.input == b.input));
        self.launcher.bindings.push(b);
    }

    /// bind 表を空にする。
    pub fn clear_launcher_bindings(&mut self) {
        self.launcher.bindings.clear();
    }

    /// 届いた MIDI をランチャーが消費したか。`true` なら **音源へ流さない**
    /// (パッドで撃ったノートが楽器も鳴らすのを防ぐ)。
    ///
    /// 1. Learn 待ちなら bind して消費
    /// 2. 既存 binding に当たれば発火して消費
    /// 3. どちらでもなければ `false` (通常の MIDI 経路へ)
    pub(crate) fn consume_launcher_midi(
        &mut self,
        channel: u8,
        input: crate::event_launcher::LauncherBindInput,
        pressed: bool,
    ) -> bool {
        if let Some(target) = self.launcher.learn_target.take() {
            // 離した側で bind すると、押した瞬間に何が起きたか分からない。
            // 押下だけを bind の合図にする。
            if !pressed {
                self.launcher.learn_target = Some(target);
                return true;
            }
            self.add_launcher_binding(LauncherBinding { channel, input, target });
            self.ui_ephemeral.status_message =
                format!("MIDI 割り当て: {input:?} (ch {channel}) → {}", target.label());
            return true;
        }
        self.fire_launcher_bindings(channel, input, pressed)
    }

    /// binding 表を引いて当たった操作を全部撃つ。当たれば `true`。
    pub(crate) fn fire_launcher_bindings(
        &mut self,
        channel: u8,
        input: crate::event_launcher::LauncherBindInput,
        pressed: bool,
    ) -> bool {
        let targets: Vec<crate::event_launcher::LauncherBindTarget> = self
            .launcher
            .bindings
            .iter()
            .filter(|b| b.matches(channel, input))
            .map(|b| b.target)
            .collect();
        if targets.is_empty() {
            return false;
        }
        for target in targets {
            self.fire_bind_target(target, pressed);
        }
        true
    }

    /// bind 先 1 つを撃つ。行 / セルが消えていれば黙って何もしない
    /// (パッドの物理位置は残るので、消えた宛先で落ちてはいけない)。
    fn fire_bind_target(
        &mut self,
        target: crate::event_launcher::LauncherBindTarget,
        pressed: bool,
    ) {
        use crate::event_launcher::LauncherBindTarget as T;
        match target {
            T::LaunchCell { track_id, scene_id } => {
                if let Some(cell) =
                    self.cell_in_row_at_scene(LauncherRow::Track(track_id), scene_id)
                {
                    self.launch_cell(cell, pressed);
                } else if pressed {
                    // 空セルは停止 (Q11)。
                    self.stop_launcher_row(LauncherRow::Track(track_id));
                }
            }
            T::LaunchScene { scene_id } => self.launch_scene(scene_id, pressed),
            // 停止 / アレンジへ戻す は押下だけで完結する (離しても何もしない)。
            T::StopRow { track_id } if pressed => {
                self.stop_launcher_row(LauncherRow::Track(track_id));
            }
            T::StopAllRows if pressed => self.stop_all_launcher_rows(),
            T::SwitchRowToArranger { track_id } if pressed => {
                self.row_to_arranger(LauncherRow::Track(track_id));
            }
            T::SwitchAllToArranger if pressed => self.all_rows_to_arranger(),
            _ => {}
        }
    }

    /// ランチャー操作を engine へ送る **唯一の口**。
    ///
    /// `common::protocol::AudioCommand` に variant が入るまでのシム。 入ったら
    /// ここを `self.send_audio(AudioCommand::…)` の match に差し替えるだけで、
    /// 呼び出し側 (この module の handler 群) は 1 行も変わらない。
    ///
    /// **`Song` 側の更新はシムの有無に関わらず効いている** — `edit_song` が
    /// epoch を bump し、runner の frame flush が `LoadSong` を送るので、
    /// 主導権 (`RowPlayback`) は今の protocol でも engine に届く。届かないのは
    /// 「量子化境界で撃つ」のタイミング指示だけ。
    pub(crate) fn send_launcher_audio(&self, cmd: LauncherAudioCommand) {
        tracing::debug!(?cmd, "launcher command (protocol variant 待ち)");
    }

    // ------------------------------------------------------------------
    // dispatcher
    // ------------------------------------------------------------------

    /// `AppEvent::Launcher(..)` の本体。
    pub fn handle_launcher_event(&mut self, ev: LauncherEvent) {
        use LauncherEvent as E;
        match ev {
            // ---- 発火 ----
            E::LaunchCell { cell, pressed } => self.launch_cell(cell, pressed),
            E::LaunchScene { scene_id, pressed } => self.launch_scene(scene_id, pressed),
            E::StopRow { row } => self.stop_launcher_row(row),
            E::StopAllRows => self.stop_all_launcher_rows(),
            E::RowToArranger { row } => self.row_to_arranger(row),
            E::AllToArranger => self.all_rows_to_arranger(),
            E::SetGlobalQuantize(q) => self.set_global_launch_quantize(q),

            // ---- 表示 ----
            E::SetLayout(l) => self.ui_prefs.launcher_layout = l,
            E::CycleLayout => {
                self.ui_prefs.launcher_layout = self.ui_prefs.launcher_layout.cycle();
            }

            // ---- 選択とフォーカス ----
            E::SelectCell { cell, modifier } => self.select_launcher_cell(cell, modifier),
            E::FocusCell { row, scene_index } => {
                self.launcher.focus = Some(LauncherFocus { row, scene_index });
            }
            E::SetHover(at) => {
                self.launcher.hover =
                    at.map(|(row, scene_index)| LauncherFocus { row, scene_index });
            }
            E::MoveFocus { dx, dy } => self.move_launcher_focus(dx, dy),
            E::LaunchFocused => self.launch_focused_cell(),

            // ---- 列 (シーン) ----
            E::AddScene => self.add_scene(),
            E::DeleteScenes(ids) => self.delete_scenes(&ids),
            E::MoveScene { scene_id, to_index } => self.move_scene(scene_id, to_index),
            E::SetSceneColor { scene_id, color } => self.set_scene_color(scene_id, color),
            E::BeginRenameScene(id) => self.begin_rename_scene(id),
            E::RenameSceneChanged(t) => self.launcher.scene_rename_text = t,
            E::CommitRenameScene => self.commit_rename_scene(),
            E::CancelRenameScene => self.cancel_rename_scene(),
            E::CaptureScene => self.capture_scene(),
            E::SetSceneFollow { scene_ids, edit } => self.set_scene_follow(&scene_ids, edit),

            // ---- セル (launcher_cells.rs) ----
            E::CreateCell { row, scene_index } => self.create_launcher_cell(row, scene_index),
            E::DeleteCells(cells) => self.delete_launcher_cells(&cells),
            E::DuplicateCells { cells, unique } => self.duplicate_launcher_cells(&cells, unique),
            E::MoveCells { moves, mode } => self.move_launcher_cells(&moves, mode),
            E::SetLaunchSettings { cells, edit } => self.set_launch_settings(&cells, edit),

            // ---- MIDI ----
            E::StartLearn(target) => {
                self.launcher.learn_target = Some(target);
                self.ui_ephemeral.status_message =
                    format!("MIDI Learn: 次のノート / CC を「{}」に割り当てます", target.label());
            }
            E::CancelLearn => self.launcher.learn_target = None,
            E::ClearBindings => {
                self.clear_launcher_bindings();
                self.ui_ephemeral.status_message = "ランチャーの MIDI 割り当てを消しました".into();
            }
        }
    }

    // ------------------------------------------------------------------
    // 行の主導権
    // ------------------------------------------------------------------

    /// 行の現在の主導権。行が実在しなければ [`RowPlayback::Arranger`]。
    #[must_use]
    pub fn row_playback(&self, row: LauncherRow) -> RowPlayback {
        let song = self.song_doc.song();
        match row {
            LauncherRow::Track(id) => song.track_by_id(id).map_or(RowPlayback::Arranger, |t| t.launcher),
            LauncherRow::Lane(k) => song
                .automation_lane_by_key(k.track, k.lane)
                .map_or(RowPlayback::Arranger, |l| l.launcher),
        }
    }

    /// 行の主導権を書く。値が変わらなければ `Song` を触らない (= 空の undo step を
    /// 積まない)。戻り値 = 実際に書き換わったか。
    fn set_row_playback(&mut self, row: LauncherRow, next: RowPlayback) -> bool {
        self.edit_song_checked(|song| match row {
            LauncherRow::Track(id) => song.track_by_id_mut(id).is_some_and(|t| {
                let changed = t.launcher != next;
                t.launcher = next;
                changed
            }),
            LauncherRow::Lane(k) => song
                .automation_lane_by_key_mut(k.track, k.lane)
                .is_some_and(|l| {
                    let changed = l.launcher != next;
                    l.launcher = next;
                    changed
                }),
        })
    }

    /// ランチャーが主導権を握っている行が 1 つでもあるか
    /// (トランスポートの「アレンジに戻す (全行)」 の点灯条件)。
    #[must_use]
    pub fn launcher_has_active_row(&self) -> bool {
        let song = self.song_doc.song();
        song.song_lanes.iter().any(|l| l.launcher.is_launcher())
            || song.tracks.iter().any(|t| {
                t.launcher.is_launcher()
                    || t.automation_lanes.iter().any(|l| l.launcher.is_launcher())
            })
    }

    /// セルを撃つ / 離す。
    ///
    /// `Song` に書くのは **「ユーザーが最後に撃った状態」** だけ。押下の解釈は
    /// セルの [`LaunchMode`](common::model::LaunchMode) に従う:
    /// - `Trigger` / `Repeat` — 押下で発火、離しても `Song` は変えない
    ///   (`Repeat` の「押している間の撃ち直し」 は engine の仕事)
    /// - `Gate` — 離すと停止
    /// - `Toggle` — 鳴っているセルをもう一度押すと停止
    pub fn launch_cell(&mut self, cell: LauncherCellKey, pressed: bool) {
        use common::model::LaunchMode;
        let row = cell.row();
        let clip_id = cell.clip_id();
        let mode = self.launch_settings_of(cell).map_or(LaunchMode::Trigger, |s| s.mode);
        let playing_this = self.row_playback(row) == RowPlayback::Launcher { clip_id };
        let next = match (mode, pressed) {
            (LaunchMode::Gate, false) => Some(RowPlayback::LauncherStopped),
            (LaunchMode::Toggle, true) if playing_this => Some(RowPlayback::LauncherStopped),
            (_, true) => Some(RowPlayback::Launcher { clip_id }),
            (_, false) => None,
        };
        if let Some(next) = next {
            self.set_row_playback(row, next);
        }
        self.send_launcher_audio(LauncherAudioCommand::LaunchCell { row, clip_id, pressed });
    }

    /// 列 (シーン) を撃つ。
    ///
    /// - その列に **セルがある行** はそのセルを鳴らす (= ランチャーが主導権を奪う)
    /// - その列にセルが無く、**既にランチャーが握っている行**は停止する
    ///   (空セル = 停止、Q11)
    /// - その列にセルが無く、**アレンジ主導のままの行**は触らない
    ///   (主導権を奪う契機は「その行を含むシーンを撃つ」ことなので、
    ///   1 つもセルの無い行がシーン発火で黙って無音になることは無い)
    pub fn launch_scene(&mut self, scene_id: u32, pressed: bool) {
        if pressed {
            let mut plan: Vec<(LauncherRow, RowPlayback)> = Vec::new();
            for row in self.all_launcher_rows() {
                if let Some(cell) = self.cell_in_row_at_scene(row, scene_id) {
                    plan.push((row, RowPlayback::Launcher { clip_id: cell.clip_id() }));
                } else if self.row_playback(row).is_launcher() {
                    plan.push((row, RowPlayback::LauncherStopped));
                }
            }
            // 1 event = 1 undo step (`edit_song` の ambient scope が squash する)。
            for (row, next) in plan {
                self.set_row_playback(row, next);
            }
        }
        self.send_launcher_audio(LauncherAudioCommand::LaunchScene { scene_id, pressed });
    }

    /// 行の Stop Clips: ランチャーが握ったまま無音にする (アレンジへは戻さない)。
    pub fn stop_launcher_row(&mut self, row: LauncherRow) {
        self.set_row_playback(row, RowPlayback::LauncherStopped);
        self.send_launcher_audio(LauncherAudioCommand::StopRow { row });
    }

    /// 全行の Stop Clips。
    pub fn stop_all_launcher_rows(&mut self) {
        for row in self.all_launcher_rows() {
            self.set_row_playback(row, RowPlayback::LauncherStopped);
        }
        self.send_launcher_audio(LauncherAudioCommand::StopAllRows);
    }

    /// 行をアレンジ主導へ戻す (Switch Playback to Arranger)。
    pub fn row_to_arranger(&mut self, row: LauncherRow) {
        self.set_row_playback(row, RowPlayback::Arranger);
        self.send_launcher_audio(LauncherAudioCommand::SwitchRowToArranger { row });
    }

    /// 全行をアレンジ主導へ戻す (トランスポートのボタン)。
    pub fn all_rows_to_arranger(&mut self) {
        for row in self.all_launcher_rows() {
            self.set_row_playback(row, RowPlayback::Arranger);
        }
        self.send_launcher_audio(LauncherAudioCommand::SwitchAllToArranger);
    }

    // ------------------------------------------------------------------
    // 行の並び (キーボード移動と一括操作の順序)
    // ------------------------------------------------------------------

    /// セルを持ちうる **全部の行** (曲の構造そのもの)。
    ///
    /// [`Self::launcher_rows`] と違って **表示の折りたたみを一切見ない**。
    /// シーン発火 / 全停止 / 全行アレンジ復帰 / Capture は「いま画面に出ている行」
    /// ではなく曲の全行に効かなければならない — グループを畳んだだけで子トラックが
    /// シーン発火から外れたり、畳んだ行がランチャーに握られたまま「アレンジに戻す」
    /// で戻らなくなったりする (ボタンは点灯し続けるのに押しても直らない)。
    #[must_use]
    pub fn all_launcher_rows(&self) -> Vec<LauncherRow> {
        let song = self.song_doc.song();
        let mut rows = Vec::new();
        for lane in &song.song_lanes {
            rows.push(LauncherRow::Lane(common::model::AutomationLaneKey {
                track: MASTER_TRACK_ID,
                lane: lane.id,
            }));
        }
        for track in &song.tracks {
            rows.push(LauncherRow::Track(track.id));
            for lane in &track.automation_lanes {
                rows.push(LauncherRow::Lane(common::model::AutomationLaneKey {
                    track: track.id,
                    lane: lane.id,
                }));
            }
        }
        rows
    }

    /// ランチャーの行を **アレンジと同じ表示順** で返す。
    ///
    /// **画面に出ている行だけ**なので、使ってよいのは「見えているものを対象に
    /// する操作」 — キーボードのフォーカス移動と、コピーの相対座標。
    /// 曲全体に効く操作は [`Self::all_launcher_rows`]。
    ///
    /// マスター行 (`song_lanes`) が先頭、以降は `song.tracks` 順に
    /// 「トラック行 → 展開中のオートメーションレーン行」。 畳んだグループの
    /// 子孫と、畳んだレーン帯は含まない (widget の `compute_visible_indices` +
    /// `visible_automation_lanes` と同じ規則)。
    ///
    /// マスターの **トラック行自体は含まない** — マスターはクリップを持たない
    /// (`Song` に `master_launcher` が無い) ので、セルを置ける行にならない。
    #[must_use]
    pub fn launcher_rows(&self) -> Vec<LauncherRow> {
        let song = self.song_doc.song();
        let mut rows = Vec::new();
        if self.ui_prefs.master_row_automation_expanded {
            for lane in song.song_lanes.iter().filter(|l| l.visible) {
                rows.push(LauncherRow::Lane(common::model::AutomationLaneKey {
                    track: MASTER_TRACK_ID,
                    lane: lane.id,
                }));
            }
        }
        for track in &song.tracks {
            if self.track_hidden_by_collapsed_group(track.id) {
                continue;
            }
            rows.push(LauncherRow::Track(track.id));
            if !self.ui_prefs.expanded_automation_tracks.contains(&track.id) {
                continue;
            }
            for lane in track.automation_lanes.iter().filter(|l| l.visible) {
                rows.push(LauncherRow::Lane(common::model::AutomationLaneKey {
                    track: track.id,
                    lane: lane.id,
                }));
            }
        }
        rows
    }

    /// 祖先のどれかが畳まれたグループなら `true` (= 行として描かれない)。
    fn track_hidden_by_collapsed_group(&self, track_id: u32) -> bool {
        let song = self.song_doc.song();
        let mut cursor = song.track_by_id(track_id).and_then(|t| t.parent_group_id);
        // 壊れた parent 連鎖 (循環) でも止まるよう hop を切る (widget と同じ 32)。
        for _ in 0..32 {
            let Some(pid) = cursor else { return false };
            if self.ui_prefs.collapsed_groups.contains(&pid) {
                return true;
            }
            cursor = song.track_by_id(pid).and_then(|t| t.parent_group_id);
        }
        false
    }

    /// 列 id の表示順リスト。
    #[must_use]
    pub fn scene_ids(&self) -> Vec<u32> {
        self.song_doc.song().scenes.iter().map(|s| s.id).collect()
    }

    /// 行 `row` の列 `scene_id` にあるセル。
    #[must_use]
    pub fn cell_in_row_at_scene(&self, row: LauncherRow, scene_id: u32) -> Option<LauncherCellKey> {
        let song = self.song_doc.song();
        let clip_id = match row {
            LauncherRow::Track(id) => song.track_by_id(id)?.session_clip(scene_id)?.clip.id,
            LauncherRow::Lane(k) => {
                song.automation_lane_by_key(k.track, k.lane)?.session_clip(scene_id)?.clip.id
            }
        };
        Some(crate::handler::launcher_cells::cell_key_in_row(row, clip_id))
    }

    // ------------------------------------------------------------------
    // キーボードのフォーカス移動
    // ------------------------------------------------------------------

    /// 矢印キー。`dx` = 列方向 / `dy` = 行方向 (下が正)。
    /// フォーカスが無ければ左上 (先頭行 × 先頭列) から始める。
    ///
    /// 列は **実シーン数 + 1** まで歩ける — 末尾の空きプレースホルダ列に
    /// セルを置ける (置いた瞬間に `Song::ensure_scene_at` で実体化する) ように
    /// するため。行が 1 つも無ければ何もしない。
    pub fn move_launcher_focus(&mut self, dx: i32, dy: i32) {
        let rows = self.launcher_rows();
        if rows.is_empty() {
            return;
        }
        let max_scene = self.song_doc.song().scenes.len(); // = 末尾プレースホルダの index
        let cur = self.launcher.focus.and_then(|f| {
            rows.iter().position(|r| *r == f.row).map(|i| (i, f.scene_index))
        });
        let (row_i, scene_i) = cur.unwrap_or((0, 0));
        let next_row = (row_i as i64 + i64::from(dy)).clamp(0, rows.len() as i64 - 1) as usize;
        let next_scene = (scene_i as i64 + i64::from(dx)).clamp(0, max_scene as i64) as usize;
        self.launcher.focus = Some(LauncherFocus {
            row: rows[next_row],
            scene_index: next_scene,
        });
    }

    /// `Enter`: フォーカス中のセルを撃つ。空セルならその行を止める (= 空セルは停止)。
    ///
    /// キーボードには「離す」が無いので **押下だけ**を送る。`Gate` のセルは
    /// 離すまで鳴り続けるが、これは Live のキーボード発火と同じ扱い。
    pub fn launch_focused_cell(&mut self) {
        let Some(focus) = self.launcher.focus else {
            return;
        };
        let Some(&scene_id) = self.song_doc.song().scenes.get(focus.scene_index).map(|s| &s.id)
        else {
            // プレースホルダ列 (まだ実体が無い) は撃つものが無い。
            return;
        };
        match self.cell_in_row_at_scene(focus.row, scene_id) {
            Some(cell) => self.launch_cell(cell, true),
            None => self.stop_launcher_row(focus.row),
        }
    }

    // ------------------------------------------------------------------
    // 列 (シーン) の CRUD
    // ------------------------------------------------------------------

    /// 末尾に列を 1 つ足す。
    pub fn add_scene(&mut self) {
        self.edit_song(|song| song.push_scene());
    }

    /// 列を削除する。**その列のセルも一緒に消える** (`normalize_session` が
    /// 孤児セルを捨てる規則と同じことを、削除の場で明示的に行う)。
    /// 削除で鳴らすものが無くなった行は [`RowPlayback::LauncherStopped`] へ落ちる
    /// (アレンジへは戻さない — 戻すとアレンジのクリップが黙って鳴り出す)。
    pub fn delete_scenes(&mut self, ids: &[u32]) {
        if ids.is_empty() {
            return;
        }
        let ids = ids.to_vec();
        self.edit_song_checked(|song| {
            let before = song.scenes.len();
            song.scenes.retain(|s| !ids.contains(&s.id));
            if song.scenes.len() == before {
                return false;
            }
            // セルの掃除と主導権の落とし込みはモデル側の正規化が SSoT。
            song.normalize_session();
            true
        });
        self.prune_launcher_selection();
    }

    /// 列を表示順で `to_index` へ動かす (見出しのドラッグ)。
    pub fn move_scene(&mut self, scene_id: u32, to_index: usize) {
        self.edit_song_checked(|song| {
            let Some(from) = song.scene_index(scene_id) else {
                return false;
            };
            let to = to_index.min(song.scenes.len().saturating_sub(1));
            if from == to {
                return false;
            }
            let s = song.scenes.remove(from);
            song.scenes.insert(to, s);
            true
        });
    }

    /// 列の色。
    pub fn set_scene_color(&mut self, scene_id: u32, color: [f32; 3]) {
        self.edit_song_checked(|song| {
            let Some(i) = song.scene_index(scene_id) else {
                return false;
            };
            let next = Some(color);
            if song.scenes[i].color == next {
                return false;
            }
            song.scenes[i].color = next;
            true
        });
    }

    /// 列 (シーン) のフォローアクション編集。セル側 ([`Self::set_launch_settings`]) と
    /// 同じ [`LaunchEdit`](crate::event_launcher::LaunchEdit) を使う
    /// (シーンは `follow` しか持たないので、それ以外の variant は無視される)。
    pub fn set_scene_follow(&mut self, scene_ids: &[u32], edit: crate::event_launcher::LaunchEdit) {
        if scene_ids.is_empty() {
            return;
        }
        let ids = scene_ids.to_vec();
        self.edit_song_checked(|song| {
            let mut changed = false;
            for scene in song.scenes.iter_mut().filter(|s| ids.contains(&s.id)) {
                let before = scene.follow.clone();
                edit.apply_follow(&mut scene.follow);
                changed |= scene.follow != before;
            }
            changed
        });
    }

    /// 列名の inline rename を開始する (見出しのダブルクリック / メニュー)。
    pub fn begin_rename_scene(&mut self, scene_id: u32) {
        let Some(i) = self.song_doc.song().scene_index(scene_id) else {
            return;
        };
        // 未命名なら表示中の自動名 ("Scene N") を初期値に入れる — 空欄から
        // 打ち直させると「いま何という名前なのか」が消える。
        self.launcher.scene_rename_text = self.song_doc.song().scenes[i].display_name(i);
        self.launcher.scene_rename_id = Some(scene_id);
    }

    /// 列名の確定。空文字は「未命名へ戻す」 (= 自動名 "Scene N" に戻る)。
    pub fn commit_rename_scene(&mut self) {
        let Some(scene_id) = self.launcher.scene_rename_id.take() else {
            return;
        };
        let text = std::mem::take(&mut self.launcher.scene_rename_text);
        let name = text.trim().to_string();
        self.edit_song_checked(|song| {
            let Some(i) = song.scene_index(scene_id) else {
                return false;
            };
            // 自動名をそのまま確定したときは「未命名のまま」にする — 焼き込むと
            // 並べ替えても番号が追従しなくなる (`Scene::display_name` の契約)。
            let next = if name == song.scenes[i].display_name(i) { String::new() } else { name };
            if song.scenes[i].name == next {
                return false;
            }
            song.scenes[i].name = next;
            true
        });
    }

    /// 列名の編集をやめる (Esc / 外クリック)。
    pub fn cancel_rename_scene(&mut self) {
        self.launcher.scene_rename_id = None;
        self.launcher.scene_rename_text.clear();
    }

    /// Capture: **いま鳴っているセル**を新しい列として取り込む。
    ///
    /// 末尾に列を 1 つ作り、`RowPlayback::Launcher` の行のセルを **リンク複製**
    /// (同じ `content_id` を共有) してその列に置く。 再生は止めない / 主導権も
    /// 動かさない — 「今の音をシーンとして保存する」だけの操作なので、
    /// 押した瞬間に音が変わらないことを優先する。
    pub fn capture_scene(&mut self) {
        let sources: Vec<LauncherCellKey> = self
            .all_launcher_rows()
            .into_iter()
            .filter_map(|row| {
                let clip_id = self.row_playback(row).playing_clip_id()?;
                Some(crate::handler::launcher_cells::cell_key_in_row(row, clip_id))
            })
            .collect();
        if sources.is_empty() {
            self.ui_ephemeral.status_message =
                "取り込むセルがありません (鳴っているセルがない)".into();
            return;
        }
        self.edit_song(|song| {
            let scene_id = song.push_scene();
            for cell in &sources {
                clone_cell_into_scene(song, *cell, scene_id);
            }
        });
    }
}

/// セル 1 つを同じ行の別の列へ **リンク複製** する (`content_id` 共有)。
/// [`AppData::capture_scene`] と `launcher_cells` の複製が共有する 1 本。
pub(crate) fn clone_cell_into_scene(
    song: &mut common::model::Song,
    cell: LauncherCellKey,
    scene_id: u32,
) -> Option<u32> {
    match cell {
        LauncherCellKey::Track(k) => {
            let track = song.track_by_id_mut(k.track_id)?;
            let src = track.session_clip_by_id(k.clip_id)?.clone();
            let id = track.alloc_clip_id();
            track.session_clips.retain(|c| c.scene_id != scene_id);
            track.session_clips.push(common::model::SessionClip {
                scene_id,
                clip: common::model::Clip { id, start_beat: 0.0, ..src.clip },
                launch: src.launch,
            });
            Some(id)
        }
        LauncherCellKey::Lane(k) => {
            let lane = song.automation_lane_by_key_mut(k.track, k.lane)?;
            let src = lane.session_clips.iter().find(|c| c.clip.id == k.clip)?.clone();
            let id = lane.alloc_clip_id();
            lane.session_clips.retain(|c| c.scene_id != scene_id);
            lane.session_clips.push(common::model::SessionAutomationClip {
                scene_id,
                clip: common::model::AutomationClip { id, start_beat: 0.0, ..src.clip },
                launch: src.launch,
            });
            Some(id)
        }
    }
}
