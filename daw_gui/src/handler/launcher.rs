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

use common::model::{ClipKey, LaunchQuantize, RowPlayback, MASTER_TRACK_ID};

use crate::event_launcher::{
    LauncherAudioCommand, LauncherCellKey, LauncherEvent, LauncherRow,
};
use crate::state::{AppData, LauncherFocus};
use crate::widgets::select_modifier::SelectModifier;

impl AppData {
    // ------------------------------------------------------------------
    // グローバル設定 / MIDI bind 表 / engine への送信 — **差し替え点**
    // ------------------------------------------------------------------

    /// グローバルローンチ量子化 (トランスポートの dropdown、既定 = 1 小節)。
    ///
    /// **SSoT は `Song`** — セルの量子化が `Global` のとき「いつ鳴り始めるか」が
    /// これで決まり、書き出す音が変わるので曲の一部 (計画書 Q9 / Q10)。
    #[must_use]
    pub fn global_launch_quantize(&self) -> LaunchQuantize {
        self.song_doc.song().global_launch_quantize
    }

    /// [`Self::global_launch_quantize`] の setter。`Song` を書き換えて (= `*` が立つ)
    /// engine へも即座に伝える (`LoadSong` を待たずに次の発火から効かせるため)。
    pub fn set_global_launch_quantize(&mut self, q: LaunchQuantize) {
        if self.global_launch_quantize() == q {
            return;
        }
        // `Song` を書き換えれば epoch が上がり、runner の frame flush が `LoadSong` で
        // engine へ届ける。**専用コマンドを持たない** — 同じ値の経路を 2 本持つと
        // どちらが効いたか分からなくなる (SSoT)。次の発火から効けば十分な粒度。
        self.edit_song(|song| song.global_launch_quantize = q);
    }

    /// MIDI → ランチャー操作の binding。**表は `Song.midi_bindings` 1 本**で、
    /// 連続値のパラメータ (CC) と同じ場所に載る。
    #[must_use]
    pub fn launcher_bindings(&self) -> Vec<common::model::MidiBinding> {
        self.song_doc
            .song()
            .midi_bindings
            .iter()
            .filter(|b| b.target.is_launcher())
            .cloned()
            .collect()
    }

    /// 同じ `(channel, input)` の既存 binding を置き換えて 1 件足す
    /// (`handle_midi_control_change` の Learn と同じ replace 規約)。
    /// **パラメータ側の bind とも重複させない** — 同じ入力が 2 つの意味を持つと、
    /// どちらが効くかが表の順序で決まってしまう。
    pub fn add_launcher_binding(&mut self, b: common::model::MidiBinding) {
        self.edit_song(|song| {
            song.midi_bindings
                .retain(|e| !(e.channel == b.channel && e.input == b.input));
            song.midi_bindings.push(b);
        });
    }

    /// ランチャー宛の bind だけを消す (パラメータ側は残す)。
    pub fn clear_launcher_bindings(&mut self) {
        self.edit_song(|song| song.midi_bindings.retain(|b| !b.target.is_launcher()));
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
        input: common::model::MidiBindInput,
        pressed: bool,
    ) -> bool {
        if let Some(target) = self.launcher.learn_target.take() {
            // 離した側で bind すると、押した瞬間に何が起きたか分からない。
            // 押下だけを bind の合図にする。
            if !pressed {
                self.launcher.learn_target = Some(target);
                return true;
            }
            self.add_launcher_binding(common::model::MidiBinding {
                channel,
                input,
                legacy_controller: None,
                target,
            });
            self.ui_ephemeral.status_message = format!(
                "MIDI 割り当て: {input:?} (ch {channel}) → {}",
                target.launcher_label().unwrap_or("")
            );
            return true;
        }
        self.fire_launcher_bindings(channel, input, pressed)
    }

    /// CC 由来の押下を **遷移したときだけ** 通す (`true` = 今回発火してよい)。
    /// ノート由来 (on/off が既に遷移) はここを通さない。
    pub(crate) fn launcher_cc_edge(
        &mut self,
        channel: u8,
        input: common::model::MidiBindInput,
        pressed: bool,
    ) -> bool {
        let key = (channel, input);
        match self.launcher.cc_pressed.iter_mut().find(|(k, _)| *k == key) {
            Some((_, prev)) => {
                let edge = *prev != pressed;
                *prev = pressed;
                edge
            }
            None => {
                self.launcher.cc_pressed.push((key, pressed));
                // 初回は「押した」だけを発火にする (立ち上がりでの取りこぼしを防ぐ)。
                pressed
            }
        }
    }

    /// binding 表を引いて当たった操作を全部撃つ。当たれば `true`。
    pub(crate) fn fire_launcher_bindings(
        &mut self,
        channel: u8,
        input: common::model::MidiBindInput,
        pressed: bool,
    ) -> bool {
        let targets: Vec<common::model::BindingTarget> = self
            .song_doc
            .song()
            .midi_bindings
            .iter()
            .filter(|b| b.target.is_launcher() && b.matches(channel, input))
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
        target: common::model::BindingTarget,
        pressed: bool,
    ) {
        use common::model::BindingTarget as T;
        match target {
            T::LaunchCell { track_id, scene_id } => {
                if let Some(cell) =
                    self.cell_in_row_at_scene(LauncherRow::Track(track_id), scene_id)
                {
                    self.launch_cell(cell, pressed);
                } else if pressed && self.song_doc.song().scene_index(scene_id).is_some() {
                    // **列が実在して、そこが空セル**のときだけ停止 (Q11)。
                    // 列ごと消えている割り当ては何もしない — 「消した列のパッドを
                    // 押したら関係ない行が止まる」のは事故でしかない。
                    self.stop_launcher_row(LauncherRow::Track(track_id));
                }
            }
            T::LaunchScene { scene_id } => self.launch_scene(scene_id, pressed),
            // 停止 / アレンジへ戻す は押下だけで完結する (離しても何もしない)。
            T::StopLauncherRow { track_id } if pressed => {
                self.stop_launcher_row(LauncherRow::Track(track_id));
            }
            T::StopAllLauncherRows if pressed => self.stop_all_launcher_rows(),
            T::SwitchRowToArranger { track_id } if pressed => {
                self.row_to_arranger(LauncherRow::Track(track_id));
            }
            T::SwitchAllToArranger if pressed => self.all_rows_to_arranger(),
            _ => {}
        }
    }

    /// ランチャー操作を engine へ送る **唯一の口**。
    ///
    /// 行 ([`LauncherRow`]) を protocol の宛先 `(track_id, lane_id)` へ直す。
    /// `lane_id == 0` がトラック行 (安定 id、アーキ不変条件 1)。
    fn launcher_row_ids(row: LauncherRow) -> (u32, u32) {
        match row {
            LauncherRow::Track(id) => (id, 0),
            LauncherRow::Lane(k) => (k.track, k.lane),
        }
    }

    pub(crate) fn send_launcher_audio(&self, cmd: LauncherAudioCommand) {
        use common::protocol::AudioCommand as A;
        let a = match cmd {
            LauncherAudioCommand::LaunchCell { row, clip_id, pressed } => {
                let (track_id, lane_id) = Self::launcher_row_ids(row);
                A::LaunchCell { track_id, lane_id, clip_id, pressed }
            }
            LauncherAudioCommand::LaunchCellFrom { row, clip_id, phase_beats } => {
                let (track_id, lane_id) = Self::launcher_row_ids(row);
                A::LaunchCellFrom { track_id, lane_id, clip_id, phase_beats }
            }
            LauncherAudioCommand::RephaseRows { phase_beats } => {
                A::RephaseLauncherRows { phase_beats }
            }
            LauncherAudioCommand::LaunchScene { scene_id, pressed } => {
                A::LaunchScene { scene_id, pressed }
            }
            LauncherAudioCommand::StopRow { row } => {
                let (track_id, lane_id) = Self::launcher_row_ids(row);
                A::StopRow { track_id, lane_id }
            }
            LauncherAudioCommand::StopAllRows => A::StopAllRows,
            LauncherAudioCommand::SwitchRowToArranger { row } => {
                let (track_id, lane_id) = Self::launcher_row_ids(row);
                A::SwitchRowToArranger { track_id, lane_id }
            }
            LauncherAudioCommand::SwitchAllToArranger => A::SwitchAllToArranger,
        };
        self.send_audio(a);
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
            E::PlayFromCellBeat { cell, phase_beats } => self.play_from_cell_beat(cell, phase_beats),
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
            E::SelectScene { scene_id, modifier } => self.select_scene(scene_id, modifier),
            E::OpenCellEditor(cell) => self.open_cell_editor(cell),
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
            E::AddSceneAt(index) => self.add_scene_at(index),
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
            E::DropClipsToCells { drops, mode } => self.drop_clips_to_cells(&drops, mode),
            E::DropCellsToArranger { drops, mode } => self.drop_cells_to_arranger(&drops, mode),
            E::SetLaunchSettings { cells, edit } => self.set_launch_settings(&cells, edit),
            E::SetCellLength { cells, beats } => self.set_cell_length(&cells, beats),

            // ---- MIDI ----
            E::StartLearn(target) => {
                self.launcher.learn_target = Some(target);
                self.ui_ephemeral.status_message =
                    format!("MIDI Learn: 次のノート / CC を「{}」に割り当てます", target.launcher_label().unwrap_or(""));
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

    /// 行の**走行中**の主導権 (engine 観測値。届いていなければ `Song` の値)。
    /// 「いま何が鳴っているか」で判断すべきものはこれを読む
    /// (`Song` 側は「ユーザーが最後に撃った起点」で、走行中の遷移を含まない)。
    #[must_use]
    pub fn running_playback(&self, row: LauncherRow) -> RowPlayback {
        use common::audio_bridge::{LAUNCHER_STATE_PLAYING, LAUNCHER_STATE_STOPPED};
        let (track_id, lane_id) = Self::launcher_row_ids(row);
        match self.launcher_running_row(track_id, lane_id) {
            Some(s) if s.state == LAUNCHER_STATE_PLAYING => {
                RowPlayback::Launcher { clip_id: s.playing_clip_id }
            }
            Some(s) if s.state == LAUNCHER_STATE_STOPPED => RowPlayback::LauncherStopped,
            Some(_) => RowPlayback::Arranger,
            None => self.row_playback(row),
        }
    }

    /// 行の主導権を書く。再生状態なので undo にも `*` にも入れない
    /// (`edit_playback`、計画書 §1.3)。戻り値 = 実際に書き換わったか。
    fn set_row_playback(&mut self, row: LauncherRow, next: RowPlayback) -> bool {
        self.edit_playback(|song| match row {
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
        // Toggle の判定は **いま鳴っているセル** で行う。`Song` 側 (= 最後に撃った
        // 起点) だけで見ると、フォローアクションで別のセルへ移った後に engine と
        // 判断が食い違い、「鳴っているセルをもう一度押したのに止まらない」になる。
        //
        // **停止中は「鳴っている」ではない。** ランチャーの走行状態は停止で消えない
        // (計画書 §0) ので、Space で止めた行の publish は `Cell` のまま残る。それを
        // そのまま Toggle の「もう一度押した」と読むと、停止 → ▶ が停止予約になり
        // その行だけ 1 回鳴らない。engine の `press_cell` も同じ値
        // (`FireAt::is_playing` = transport 要求を消費する前の `playing`) で判断する
        // ので、**両側を同時に直さないと** GUI が書いた `LauncherStopped` を
        // `sync_saved_rows` が後から適用してセルが消える。
        let playing_this = self.transport.is_playing
            && self.running_playback(row) == RowPlayback::Launcher { clip_id };
        // Gate の離しは **その行がまだこのセルを起点にしているとき**だけ止める。
        // 間に別のセルを撃った行を止めてしまうと、engine (`release_cell` の
        // `held_clip_id` ガード) と `Song` が食い違い、停止 → 再生で音が変わる。
        let holds_this = self.row_playback(row) == RowPlayback::Launcher { clip_id };
        let next = match (mode, pressed) {
            (LaunchMode::Gate, false) if holds_this => Some(RowPlayback::LauncherStopped),
            (LaunchMode::Gate, false) => None,
            (LaunchMode::Toggle, true) if playing_this => Some(RowPlayback::LauncherStopped),
            (_, true) => Some(RowPlayback::Launcher { clip_id }),
            (_, false) => None,
        };
        if let Some(next) = next {
            self.set_row_playback(row, next);
        }
        if pressed {
            self.ensure_transport_rolling();
        }
        self.send_launcher_audio(LauncherAudioCommand::LaunchCell { row, clip_id, pressed });
    }

    /// ピアノロールの `f`: **全体を `phase_beats` (セルの `start_beat` からの拍) から再生**
    /// ([`LauncherEvent::PlayFromCellBeat`])。
    ///
    /// 1. song の playhead を **相対 seek** する ([`Self::cell_beat_to_song_beat`]:
    ///    `cell` が今 song 上のどこで鳴っているかを基準に、同じ周回の中でその拍に当たる
    ///    song 位置へ)。アレンジは今いる場所の近くに留まり、アレンジ主導の行はこれで
    ///    動く。`cell` が鳴っていなければ基準が無いので seek しない (停止中なら現在位置から
    ///    再生を始めるだけ)。
    /// 2. `cell` をその拍から撃つ。`Song` へは [`Self::launch_cell`] の押下と同じ
    ///    「このセルを撃った」だけを書く (走行位置は engine が持つ、計画書 §1.4)。
    ///    [`LaunchMode`](common::model::LaunchMode) は見ない — 「ここから鳴らせ」に
    ///    押下 / 離しの対は無いので、Toggle で鳴っているセルを止めたり Gate で握ったり
    ///    しない (engine の `launch_cell_from` も同じ)。
    /// 3. セルを鳴らしている他の全行も同じ拍へ揃える (engine の `rephase_running`。
    ///    停止中の行は鳴らさない)。`Song` 側の「最後に撃った状態」は変わらないので書かない。
    pub fn play_from_cell_beat(&mut self, cell: LauncherCellKey, phase_beats: f64) {
        let row = cell.row();
        let clip_id = cell.clip_id();
        self.set_row_playback(row, RowPlayback::Launcher { clip_id });
        match self.cell_beat_to_song_beat(cell, phase_beats) {
            Some(song_beat) => self.action_play_from_cursor(song_beat),
            None => self.ensure_transport_rolling(),
        }
        self.send_launcher_audio(LauncherAudioCommand::LaunchCellFrom { row, clip_id, phase_beats });
        self.send_launcher_audio(LauncherAudioCommand::RephaseRows { phase_beats });
    }

    /// 鳴っているセル `cell` の「セル内の拍 `phase_beats`」に当たる song の拍。
    ///
    /// 今の周回を基準にする: `song 拍 = 今の playhead - 今のセル内位相 + 目的の位相`
    /// (目的の位相はループ長で折り返し、ワンショットは末尾で切る — engine の
    /// `CellRef::phase_from` と同じ規則)。位相は engine の publish (`launch_beat`) から
    /// 映像 / 編集面と同じ式 ([`crate::launcher_time::cell_phase`]) で解く。
    /// セルが鳴っていない (行が publish されていない / 別のセル / 停止中) なら `None`。
    #[must_use]
    fn cell_beat_to_song_beat(&self, cell: LauncherCellKey, phase_beats: f64) -> Option<f64> {
        let (track_id, lane_id) = Self::launcher_row_ids(cell.row());
        let snap = self.launcher_running_row(track_id, lane_id)?;
        if snap.state != common::audio_bridge::LAUNCHER_STATE_PLAYING
            || snap.playing_clip_id != cell.clip_id()
        {
            return None;
        }
        let (len, looping) = self.cell_loop_of(cell)?;
        let playhead = f64::from(self.transport.playhead_beat?);
        let now = crate::launcher_time::cell_phase(snap.launch_beat, playhead, len, looping)?;
        let want = if !len.is_finite() || len <= 0.0 || !phase_beats.is_finite() {
            0.0
        } else if looping {
            phase_beats.rem_euclid(len)
        } else {
            phase_beats.clamp(0.0, len)
        };
        Some(playhead - now + want)
    }

    /// セルのループ長と looping 設定。セルが無ければ `None`。
    #[must_use]
    fn cell_loop_of(&self, cell: LauncherCellKey) -> Option<(f64, bool)> {
        let song = self.song_doc.song();
        match cell {
            LauncherCellKey::Track(k) => song
                .track_by_id(k.track_id)
                .and_then(|t| t.session_clip_by_id(k.clip_id))
                .map(|c| (c.clip.length_beats, c.launch.looping)),
            LauncherCellKey::Lane(k) => song
                .automation_lane_by_key(k.track, k.lane)
                .and_then(|l| l.session_clips.iter().find(|c| c.clip.id == k.clip))
                .map(|c| (c.clip.length_beats, c.launch.looping)),
        }
    }

    /// 列 (シーン) を撃つ。
    ///
    /// - その列に **セルがある行** はそのセルを鳴らす (= ランチャーが主導権を奪う)
    /// - その列にセルが無く、**既にランチャーが握っている行**は停止する
    ///   (空セル = 停止、Q11)
    ///
    /// **全行がランチャーへ移る** (Bitwig: シーンを撃つと全トラックが Launcher
    /// 制御になる)。列にセルが無い行が「アレンジのまま鳴り続ける」ことは無い —
    /// 撃った直後にアレンジの音とセルの音が混ざるのを避けるため。
    pub fn launch_scene(&mut self, scene_id: u32, pressed: bool) {
        if pressed {
            let mut plan: Vec<(LauncherRow, RowPlayback)> = Vec::new();
            // シーンを撃つと **全行がランチャーへ移る** (Bitwig: triggering a scene
            // shifts all tracks to Launcher control)。その列にセルが無い行は停止
            // (計画書 Q11 「空セル = 停止」)。アレンジ主導のまま残すと、シーンを撃った
            // 直後にアレンジの音とセルの音が混ざって鳴る。
            for row in self.all_launcher_rows() {
                let next = match self.cell_in_row_at_scene(row, scene_id) {
                    Some(cell) => RowPlayback::Launcher { clip_id: cell.clip_id() },
                    None => RowPlayback::LauncherStopped,
                };
                plan.push((row, next));
            }
            // 1 event = 1 undo step (`edit_song` の ambient scope が squash する)。
            for (row, next) in plan {
                self.set_row_playback(row, next);
            }
            // 列の連鎖の起点も同時に確定する (行の起点と対、§1.4)。実体の無い列
            // (`scene_id == 0` = 全行停止) は「起点なし」なのでそのまま 0 が入る。
            self.set_last_launched_scene(scene_id);
        } else {
            self.release_scene(scene_id);
        }
        // 実体の無い列 (`scene_id == 0`) を撃つのは「全行停止」なので、**これで
        // 再生を始めない** — 止めるつもりの操作が再生の開始になってしまう。
        if pressed && scene_id != 0 {
            self.ensure_transport_rolling();
        }
        self.send_launcher_audio(LauncherAudioCommand::LaunchScene { scene_id, pressed });
    }

    /// 列の **離し**。engine の `release_scene` → `release_cell` と同じ解釈を
    /// `Song` 側にも書く: その列の [`LaunchMode::Gate`](common::model::LaunchMode::Gate)
    /// のセルを起点にしている行は停止へ落ちる。書かないと「起点」だけ鳴りっぱなしの
    /// まま残り、停止 → 再生と書き出しが **離したはずのセルから始まる** (§1.4 / Q9)。
    ///
    /// 「まだその起点か」で絞るのは engine が `held_clip_id` で見ている条件と同じ —
    /// 間に別のセルを撃った行は、列の離しの対象ではない。走行位置ではなく起点
    /// (`Song`) で見るのは、フォローアクションで音が次のセルへ移っても
    /// **押しているのは最初に撃ったセル**だから (走行位置は `Song` に無い)。
    fn release_scene(&mut self, scene_id: u32) {
        let stop: Vec<LauncherRow> = self
            .all_launcher_rows()
            .into_iter()
            .filter(|row| self.scene_cell_is_held_gate(*row, scene_id))
            .collect();
        for row in stop {
            self.set_row_playback(row, RowPlayback::LauncherStopped);
        }
    }

    /// 行 `row` の列 `scene_id` のセルが **Gate** で、かつその行の起点が
    /// まだそのセルか ([`Self::release_scene`] の絞り込み条件)。
    fn scene_cell_is_held_gate(&self, row: LauncherRow, scene_id: u32) -> bool {
        let Some(cell) = self.cell_in_row_at_scene(row, scene_id) else {
            return false;
        };
        self.launch_settings_of(cell).map(|s| s.mode) == Some(common::model::LaunchMode::Gate)
            && self.row_playback(row) == (RowPlayback::Launcher { clip_id: cell.clip_id() })
    }

    /// 停止中にセル / シーンを撃ったら、その操作自体が再生の開始になる
    /// (Live / Bitwig と同じ — ▶ を押したのに何も鳴らない状態を作らない)。
    ///
    /// ランチャーの走行は transport の拍 (`playhead_beats`) で進むので、停止した
    /// ままでは engine が行を解いても音にならない。撃った瞬間に鳴り出すこと
    /// (量子化待ちに落ちないこと) は engine 側が担保する — `process_buffer` が
    /// **transport 要求を消費する前**の `playing` を発火拍の解決に渡す。
    ///
    /// 再生位置は動かさない (Play と同じ「今の playhead からそのまま」)。
    fn ensure_transport_rolling(&mut self) {
        if !self.transport.is_playing {
            self.start_transport(None);
        }
    }

    /// 行の Stop Clips: ランチャーが握ったまま無音にする (アレンジへは戻さない)。
    pub fn stop_launcher_row(&mut self, row: LauncherRow) {
        self.set_row_playback(row, RowPlayback::LauncherStopped);
        self.send_launcher_audio(LauncherAudioCommand::StopRow { row });
    }

    /// 全行の Stop Clips。
    ///
    /// 列の連鎖の起点も降ろす — 残すと、engine の `seed_from_song` が停止 → 再生 /
    /// 書き出しのたびにその列のフォローアクションを arm し直し、**全部止めたはずの
    /// 行が勝手に鳴り出す** (§1.4 / Q9 の「聴こえている通りに書き出す」が破れる)。
    pub fn stop_all_launcher_rows(&mut self) {
        for row in self.all_launcher_rows() {
            self.set_row_playback(row, RowPlayback::LauncherStopped);
        }
        self.set_last_launched_scene(0);
        self.send_launcher_audio(LauncherAudioCommand::StopAllRows);
    }

    /// 行をアレンジ主導へ戻す (Switch Playback to Arranger)。
    pub fn row_to_arranger(&mut self, row: LauncherRow) {
        self.set_row_playback(row, RowPlayback::Arranger);
        self.send_launcher_audio(LauncherAudioCommand::SwitchRowToArranger { row });
    }

    /// 全行をアレンジ主導へ戻す (トランスポートのボタン)。
    /// 列の連鎖の起点も降ろす ([`Self::stop_all_launcher_rows`] と同じ理由)。
    pub fn all_rows_to_arranger(&mut self) {
        for row in self.all_launcher_rows() {
            self.set_row_playback(row, RowPlayback::Arranger);
        }
        self.set_last_launched_scene(0);
        self.send_launcher_audio(LauncherAudioCommand::SwitchAllToArranger);
    }

    /// 列の連鎖の起点 (`Song.last_launched_scene_id`) を書く。
    ///
    /// **ユーザーが撃った列だけ**を書く (`0` = 起点なし)。フォローアクションで
    /// 移った先は engine の走行状態にしか無く、ここへ書いてはいけない — 書くと
    /// 「何秒鳴らしてから書き出したか」で出力が変わり、Q9 の再現性が壊れる
    /// (計画書 §1.4、`Song::last_launched_scene_id` の doc)。
    fn set_last_launched_scene(&mut self, scene_id: u32) -> bool {
        self.edit_playback(|song| {
            if song.last_launched_scene_id == scene_id {
                return false;
            }
            song.last_launched_scene_id = scene_id;
            true
        })
    }

    // ------------------------------------------------------------------
    // 行の並び (キーボード移動と一括操作の順序)
    // ------------------------------------------------------------------

    /// セルを持ちうる **全部の行** (曲の構造そのもの)。
    ///
    /// 集合の定義は engine の `for_each_launcher_row` と同じ — 「通常トラック行 +
    /// オートメーションレーン行 + マスター行 (`song_lanes`)」から、テンポ / 拍子
    /// レーンだけを [`AutomationTarget::accepts_launcher_cells`](common::model::AutomationTarget::accepts_launcher_cells)
    /// で外す。外さないと、シーン発火 / 全停止がそこへ `RowPlayback` を書いて
    /// `*` が立つのに engine は無視する (= 押しても音が変わらない編集が積まれる)。
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
        for lane in song.song_lanes.iter().filter(|l| l.target.accepts_launcher_cells()) {
            rows.push(LauncherRow::Lane(common::model::AutomationLaneKey {
                track: MASTER_TRACK_ID,
                lane: lane.id,
            }));
        }
        for track in &song.tracks {
            rows.push(LauncherRow::Track(track.id));
            for lane in track
                .automation_lanes
                .iter()
                .filter(|l| l.target.accepts_launcher_cells())
            {
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
    /// r.md #87: poller が 30Hz で読んだ engine の**走行状態**を取り込む。
    ///
    /// 丸ごと入れ替える (差分適用しない) — engine 側は毎 buffer 全行を publish し
    /// 直すので、部分更新にすると「消えた行」が残る。`Song` には一切書かない
    /// (計画書 §1.4 — 書くとフォローアクションのたびに `*` が立ち、書き出しの
    /// 再現性も壊れる)。
    pub(crate) fn on_launcher_rows_tick(
        &mut self,
        rows: Vec<(u64, common::audio_bridge::LauncherRowSnapshot)>,
    ) {
        self.launcher.running = rows;
    }

    /// クリップ編集面 (ピアノロール / オーディオエディタ) が出すプレイヘッド拍。
    ///
    /// **ランチャーのセルを開いているときは「セルの中のどこを鳴らしているか」**。
    /// セルは自分の時間軸で走る (`start_beat` は契約上 0) ので、song の playhead を
    /// そのまま出すと編集面の外を指してしまい **線が 1 本も見えない**。位相は映像側と
    /// 同じ [`crate::launcher_time::cell_phase`] で解く (音・絵・編集面で式が 1 本)。
    ///
    /// アレンジのクリップは従来どおり song の playhead。鳴っていないセルは `None`。
    #[must_use]
    pub fn editor_playhead_beat(&self, target: ClipKey) -> Option<f64> {
        let song = self.song_doc.song();
        let Some(track) = song.track_by_id(target.track_id) else {
            return self.transport.playhead_beat.map(f64::from);
        };
        let Some(cell) = track.session_clip_by_id(target.clip_id) else {
            // アレンジのクリップ。
            return self.transport.playhead_beat.map(f64::from);
        };
        let snap = self.launcher_running_row(target.track_id, 0)?;
        if snap.state != common::audio_bridge::LAUNCHER_STATE_PLAYING
            || snap.playing_clip_id != target.clip_id
        {
            return None;
        }
        let playhead = f64::from(self.transport.playhead_beat?);
        let phase = crate::launcher_time::cell_phase(
            snap.launch_beat,
            playhead,
            cell.clip.length_beats,
            cell.launch.looping,
        )?;
        Some(cell.clip.start_beat + phase)
    }

    /// 行の走行状態 (engine 観測値)。`None` = engine がその行を publish していない
    /// (= 停止中 / まだ届いていない) ので、呼び側は `Song.launcher` へ倒す。
    /// r.md #87: 走行状態を映像側の形 ([`crate::launcher_time::RunningRow`]) へ直す。
    ///
    /// **プレビューと動画書き出しはこれを通す** — 通さないと `Song.launcher`
    /// (= 撃った起点) だけで解くことになり、フォローアクションで音が次のセルへ
    /// 移っても絵が前のセルのままになる。engine が publish していない行は
    /// 表に載らず、`RowTimeline` 側が `Song` へ倒す。
    #[must_use]
    pub fn launcher_running_rows(&self) -> Vec<crate::launcher_time::RunningRow> {
        use common::audio_bridge::{LAUNCHER_STATE_PLAYING, LAUNCHER_STATE_STOPPED};
        use crate::launcher_time::{RowId, RunningRow};
        self.launcher
            .running
            .iter()
            .map(|(key, snap)| {
                #[allow(clippy::cast_possible_truncation)]
                let track = (*key >> 32) as u32;
                #[allow(clippy::cast_possible_truncation)]
                let lane = (*key & 0xFFFF_FFFF) as u32;
                let state = match snap.state {
                    LAUNCHER_STATE_PLAYING => {
                        common::model::RowPlayback::Launcher { clip_id: snap.playing_clip_id }
                    }
                    LAUNCHER_STATE_STOPPED => common::model::RowPlayback::LauncherStopped,
                    _ => common::model::RowPlayback::Arranger,
                };
                RunningRow {
                    row: if lane == 0 { RowId::track(track) } else { RowId::lane(track, lane) },
                    state,
                    launch_beat: snap.launch_beat,
                }
            })
            .collect()
    }

    #[must_use]
    pub fn launcher_running_row(
        &self,
        track_id: u32,
        lane_id: u32,
    ) -> Option<common::audio_bridge::LauncherRowSnapshot> {
        let want = (u64::from(track_id) << 32) | u64::from(lane_id);
        self.launcher.running.iter().find(|(k, _)| *k == want).map(|(_, s)| *s)
    }

    #[must_use]
    pub fn launcher_rows(&self) -> Vec<LauncherRow> {
        let song = self.song_doc.song();
        let mut rows = Vec::new();
        if self.ui_prefs.master_row_automation_expanded {
            for lane in song
                .song_lanes
                .iter()
                .filter(|l| l.visible && l.target.accepts_launcher_cells())
            {
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
            for lane in track
                .automation_lanes
                .iter()
                .filter(|l| l.visible && l.target.accepts_launcher_cells())
            {
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

    /// シーン見出しの click による列選択 (無修飾 = 置換 / Ctrl = トグル / Shift = 範囲)。
    ///
    /// **セルの選択は落とす。** 列とセルはインスペクタの同じ面 (ローンチ) を使うので、
    /// 両方が非空だと「セルの設定」と「列の設定」が同時に出て、どちらを触っているのか
    /// 分からなくなる (`SelectionState::selected_scene_ids` の doc)。
    ///
    /// 範囲は表示順の連続区間 (トラック / セクションと同じ 1 次元の面)。
    pub fn select_scene(&mut self, scene_id: u32, modifier: SelectModifier) {
        let order = self.scene_ids();
        if !order.contains(&scene_id) {
            return;
        }
        let anchor = self.selection.scene_anchor;
        let next = modifier.resolve(&self.selection.selected_scene_ids, scene_id, || {
            let a = anchor?;
            crate::widgets::select_modifier::range_ordered(&order, a, scene_id)
        });
        self.selection.selected_scene_ids = next;
        if modifier.updates_anchor() {
            self.selection.scene_anchor = Some(scene_id);
        }
        if self.selection.selected_scene_ids.is_empty() {
            // 空になったら、自分が立てたタグだけ降ろす (`select_track` と同じ作法)。
            // 残すと `edit_surface` が「タグはあるが面は空」で `None` を返し続け、
            // Delete が無反応になる。
            if self.selection.last_edit_select == Some(crate::app::EditSurface::Scenes) {
                self.selection.last_edit_select = None;
            }
            return;
        }
        // **直近確定面を列へ移す** ([[feedback_selection_action_last_wins]])。
        // これを書かないと、列を選んでも `last_edit_select` が前の面 (アレンジの
        // 範囲など) を指したままになり、続く Delete がそちらを消す。
        self.selection.last_edit_select = Some(crate::app::EditSurface::Scenes);
        // 列を選んだらセル側は空にする (上記の排他)。セルは行の種類に関わらず
        // この 1 本に居るので、ここで落とすのも 1 本で足りる。
        self.selection.selected_launcher_cells.clear();
        self.selection.launcher_cell_anchor = None;
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
        // 行が畳まれていて表示リストに無いときは、**列は保ったまま**先頭行へ寄せる
        // (列まで 0 に戻すと、グループを畳んだ瞬間にフォーカスが左上へ飛ぶ)。
        let (row_i, scene_i) =
            cur.unwrap_or((0, self.launcher.focus.map_or(0, |f| f.scene_index)));
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
            // 消えた列を狙う MIDI 割り当ても落とす。残すと「パッドを押したら
            // 無関係な行が止まる」 (空セル = 停止の規則に落ちる) 動きになる。
            song.midi_bindings.retain(|b| !binding_targets_scene(b.target, &ids));
            // セルの掃除と主導権の落とし込み、dangling な Jump の始末は
            // モデル側の正規化が SSoT。
            song.normalize_session();
            true
        });
        self.prune_launcher_selection();
    }

    /// 表示順 `index` の位置に列を実体化する。プレースホルダ列を右クリックして
    /// 「シーンを追加」したときの口 (押した列にそのまま生える)。
    pub fn add_scene_at(&mut self, index: usize) {
        self.edit_song_checked(|song| {
            let before = song.scenes.len();
            song.ensure_scene_at(index);
            song.scenes.len() != before
        });
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
    ///
    /// 取り込む中身は [`Self::running_playback`] (= engine の観測値) で決める。
    /// `Song` 側 (= ユーザーが最後に撃った起点) を読むと、フォローアクションで
    /// 別のセルへ移った後の Capture が **鳴っていないセル**を取り込む
    /// (§1.4: 遷移先は `Song` に書かれない)。engine が publish していない行は
    /// `running_playback` が `Song` へ倒すので、停止中の挙動は変わらない。
    pub fn capture_scene(&mut self) {
        let sources: Vec<LauncherCellKey> = self
            .all_launcher_rows()
            .into_iter()
            .filter_map(|row| {
                let clip_id = self.running_playback(row).playing_clip_id()?;
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
            track.put_session_clip(common::model::SessionClip {
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
            lane.put_session_clip(common::model::SessionAutomationClip {
                scene_id,
                clip: common::model::AutomationClip { id, start_beat: 0.0, ..src.clip },
                launch: src.launch,
            });
            Some(id)
        }
    }
}

/// この MIDI 割り当ては消える列を狙っているか (`delete_scenes` の掃除条件)。
fn binding_targets_scene(target: common::model::BindingTarget, dead: &[u32]) -> bool {
    use common::model::BindingTarget as T;
    match target {
        T::LaunchCell { scene_id, .. } | T::LaunchScene { scene_id } => dead.contains(&scene_id),
        _ => false,
    }
}
