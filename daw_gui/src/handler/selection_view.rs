//! handler::selection_view — clip/note 選択 + view state + arrange follow/zoom + piano-roll id 変換
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use crate::event::AppEvent;
use crate::event_launcher::LauncherEvent;
use common::model::Note;

impl AppData {
    /// copy / cut / delete / duplicate / zoom が共有する **単一 arbiter**:
    /// 「いまキーボード操作の対象になっている編集面」 を 1 つに解決する
    /// (grill-me 2026-06-11)。 view ではなく `AppData` が持つ — 選択セマンティクスは
    /// ドメインロジックであり、 headless 統合テストからも検証できる必要があるため。
    ///
    /// `is_pianoroll_active` はポインタが bottom panel 内 + Piano Roll タブ選択中か
    /// (view しか知らないので引数で受ける)。
    ///
    /// 解決順:
    /// 0. **inline リネーム中は常に `None`** — キーは編集中のテキストのものであって
    ///    編集面のものではない。 通常は text_input の typing lock が shortcut 層で
    ///    `delete` 等を止めるが、 lock は「前フレームに text_input が描かれたか」 由来
    ///    なので、 リネーム中にその行をスクロールで画面外へ送る / 親グループを畳むと
    ///    **描かれない → lock が外れて Delete がトラック削除に化ける** (r.md #43 review)。
    ///    view の描画状態に依存しないドメイン側のガードをここに置く。
    /// 1. **ポインタが乗っている面** (piano roll / automation lane) — hover 文脈が最優先。
    ///    ただし **その面に選択があるときだけ**。 空の面を掴んだままだと、 展開済み
    ///    automation lane にポインタを置いているだけで「ヘッダで選んだトラックの Delete が
    ///    無反応」 という位置依存の沈黙になる。
    /// 2. **last-wins**: `last_edit_select` が指す面がまだ非空ならそれ (#071)。
    /// 3. **非空優先順の fallback は `last_edit_select == None` のときだけ**
    ///    (= まだ一度も面を選んでいない / プロジェクトを開いた直後の復元選択)。
    ///    タグが立っているのにその面が空なら **`None`** を返す — 「直前に触っていた面が
    ///    空になった」 だけで別の面へ勝手に飛ぶと、 セクションを消した次の Delete が
    ///    残っているクリップを消す類の事故になる。 面が **消滅** したとき
    ///    (audio editor を閉じた / プロジェクトを差し替えた) は、 その処理側が
    ///    タグを降ろす責務を持つ。
    ///
    /// `Tracks` / `Sections` を fallback に入れないのも同じ理由:
    /// `selected_track_ids` はクリップ選択の追従 ([`Self::select_track`]) や削除後の
    /// 自動再選択でも非空になるので、 非空を「トラックを消したい意図」 の代理にできない。
    #[must_use]
    pub fn edit_surface(&self, is_pianoroll_active: bool) -> Option<EditSurface> {
        use EditSurface as S;
        // 0. inline リネーム中はどの面も対象にしない (上記 doc 参照)。
        //    r.md #87 の列見出し rename も 4 つ目の rename 状態として同列に扱う —
        //    列見出しの rect は可視列ぶんしか積まれないので、打っている途中に帯を
        //    横スクロールするだけで入力欄が描かれなくなり、typing lock が外れて
        //    Delete が選択セルの削除に化ける (view の描画状態に依存しない
        //    ドメイン側のガードがここに要る理由そのもの)。
        if self.ui_ephemeral.inline_rename_active() || self.launcher.scene_rename_id.is_some() {
            return None;
        }
        // 選択集合は面を跨いで共存できる (lasso は automation の点とクリップを両方拾う、
        // clip 選択は automation 選択を消さない)。
        let audio_events = self.ui_ephemeral.audio_editor_clip.is_some()
            && !self.selected_audio_event_indices().is_empty();
        // 面は範囲のレーン種別が持つ (`time_selection_surface`)。
        let time_face = self.time_selection_surface();
        let notes = time_face == Some(S::Notes) && !self.selected_note_ids().is_empty();
        let points = !self.selection.selected_automation_points.is_empty();
        let auto_clips = !self.selection.selected_automation_clips.is_empty();
        // 範囲面 (アレンジャーのトラック行 / オートメーションレーン行)。
        let time_range = time_face == Some(S::TimeRange);
        // ランチャーのセル面 (時間軸を持たない唯一のオブジェクト選択)。
        // **実在まで見る** — トラック / レーンごと消えたセルの key が残っていると、
        // 面は「生きている」のに Delete が 1 件も消せず、他の面へも落ちない
        // (`Devices` 面が `live_device_ids()` を引くのと同じ理由)。
        let cells = self.has_live_launcher_cells();
        let tracks = !self.selection.selected_track_ids.is_empty();
        let sections = !self.selection.selected_section_ids.is_empty();
        let devices = !self.live_device_ids().is_empty();
        let auto_prefer_clips = auto_clips
            && (!points || self.selection.last_edit_select == Some(S::AutomationClips));
        // 面が **まだ生きているか** (= その面の選択集合が非空か) の唯一の表。
        // last-wins も非空優先順 fallback もここを引く — 同じ条件を 2 か所に書くと、
        // 面を足したときに片方だけ更新されて静かに食い違う。
        let live = |s: S| match s {
            S::AudioEvents => audio_events,
            S::Notes => notes,
            S::AutomationPoints => points,
            S::AutomationClips => auto_clips,
            S::TimeRange => time_range,
            S::LauncherCells => cells,
            // ランチャーの列 (シーン)。セル面とは `SelectionState` 上で排他。
            S::Scenes => !self.selection.selected_scene_ids.is_empty(),
            S::Tracks => tracks,
            S::Sections => sections,
            S::Devices => devices,
        };
        // 1. **最後に選んだ面**がまだ非空ならそれ ([[feedback_selection_action_last_wins]])。
        //
        // ポインタが乗っている面より先に見る。 逆順だと、オートメーションレーン行の上に
        // マウスが乗っているだけで、その後に選び直したクリップ / 範囲を差し置いて
        // automation 面が勝ってしまう。 ポインタ位置は**タイブレーク**であって、
        // 直近の明示的な選択を上書きしてよい根拠ではない。
        if let Some(s) = self.selection.last_edit_select
            && live(s)
        {
            return Some(s);
        }
        // 2. 直近の面が空になった / まだ何も選んでいないとき、ポインタが乗っている面。
        if is_pianoroll_active {
            if self.ui_ephemeral.audio_editor_clip.is_some() {
                if audio_events {
                    return Some(S::AudioEvents);
                }
            } else if notes {
                return Some(S::Notes);
            }
        } else if self.ui_ephemeral.arrange_hovered_automation_lane.is_some() {
            // automation lane 上: last-wins で clip が勝つなら clip 面、 それ以外は点面。
            if auto_prefer_clips {
                return Some(S::AutomationClips);
            }
            if points {
                return Some(S::AutomationPoints);
            }
        }
        // 3. タグが無いときだけ 非空優先順 (従来順)。 タグがあるのにここへ来たのは
        //    「その面が空になった」 = 対象なし。
        if self.selection.last_edit_select.is_some() {
            return None;
        }
        // **明示的に選んだときだけ立つ面 (`Tracks` / `Sections` / `Scenes` /
        // `Devices`) はここに入れない** — それらの選択集合は追従や自動再選択でも
        // 非空になるので、非空を「その面を操作したい意図」の代理にできない。
        const FALLBACK: [S; 6] = [
            S::AudioEvents,
            S::AutomationPoints,
            S::Notes,
            S::TimeRange,
            S::LauncherCells,
            S::AutomationClips,
        ];
        FALLBACK.into_iter().find(|s| live(*s))
    }

    /// Delete キー / Edit メニューの「削除」: [`Self::edit_surface`] が解決した面の
    /// 選択だけを消す。
    ///
    /// **対象面の決定も dispatch もここが唯一の実装** (r.md #43 review、 SSoT)。
    /// 以前は view 私有の `delete_for_surface` が独自の非空優先順チェーンを持ち、
    /// arbiter と順序が食い違う「第二の arbiter」 になっていた上、 view 私有ゆえ
    /// headless から検証できず「2 回目の Delete でトラックが消えない」 の回帰テストが
    /// 恒真になっていた。 AppData 側に置くことで両方を同時に解消する。
    ///
    /// 対象面が無い (選択ゼロ / 直前に触っていた面が空になった) なら **no-op**。
    /// 無条件に削除イベントを撃つと空選択でも undo snapshot が積まれ redo 履歴が飛ぶ。
    pub fn delete_current_surface(&mut self, is_pianoroll_active: bool) {
        let Some(surface) = self.edit_surface(is_pianoroll_active) else {
            return;
        };
        let event = match surface {
            // section: 選択帯のみ削除 (内容温存)。 専用 handler で AppEvent を持たない。
            EditSurface::Sections => {
                self.apply_delete_selected_sections();
                return;
            }
            // トラック面: 選択中の全トラックを 1 undo step で削除 (Ableton 準拠)。
            // 確認ダイアログは出さない (Ableton / REAPER とも出さず undo で戻す)。
            EditSurface::Tracks => {
                AppEvent::DeleteTracks(self.selection.selected_track_ids.clone())
            }
            EditSurface::AudioEvents => AppEvent::DeleteAudioEditorSelection,
            EditSurface::Notes => AppEvent::DeleteSelectedNotes,
            EditSurface::AutomationPoints => AppEvent::DeleteAutomationPoints {
                points: self.selection.selected_automation_points.clone(),
            },
            EditSurface::AutomationClips => AppEvent::DeleteAutomationClips {
                keys: self.selection.selected_automation_clips.clone(),
            },
            // 範囲: 境界で分割して範囲部分だけ削除 (時間は詰めない)。
            EditSurface::TimeRange => AppEvent::DeleteTimeSelection,
            // セル面: セルを消す口は [`LauncherEvent::DeleteCells`] 1 本
            // (右クリックメニュー / `Ctrl+X` と同じ)。対象の解決だけここで済ませる
            // — 「選択中のセルを消す」専用イベントを別に持つと、undo ラベルや
            // 前処理が片方だけ育つ (実際 `DeleteSelectedClip` は「クリップ削除」と
            // 名乗っていた)。
            EditSurface::LauncherCells => {
                AppEvent::Launcher(LauncherEvent::DeleteCells(self.live_launcher_cells()))
            }
            // 列 (シーン): 選択中の列を削除する。その列のセルも一緒に消える
            // (`delete_scenes` が `normalize_session` を通す)。
            EditSurface::Scenes => AppEvent::Launcher(LauncherEvent::DeleteScenes(
                self.selection.selected_scene_ids.clone(),
            )),
            // r.md #71 (プラグインのコピー / 移動): チェーンで選んだプラグインを
            // 1 undo step で削除する。 対象 id は正規化を通す (= いま表示している
            // チェーンに実在するものだけ)。
            EditSurface::Devices => AppEvent::RemoveDevices {
                device_ids: self.live_device_ids(),
            },
        };
        self.handle_event(event);
    }

    /// r.md #71 (プラグインのコピー / 移動): device 面の一括操作 (削除 / cut /
    /// copy / 複製 / 運搬) が受け取る id 集合を正規化する。**いまインスペクタに
    /// 出ているチェーンに実在する id だけ**を入力順のまま残し、 重複を落とす。
    ///
    /// device 選択は「カーソルトラックのチェーン」 という面の上にあるので、
    /// cursor track が動いた瞬間に元トラックの id が stale になる。 cursor を動かす
    /// 経路すべてに掃除を挿す (= 貼り替え補償コード、 不変条件 1 が禁じる形) のでは
    /// なく、 **読む側で毎回正規化する**。 [`Self::live_track_ids`] と同じ流儀。
    #[must_use]
    pub fn live_device_ids(&self) -> Vec<u64> {
        let Some(track_id) = self.cursor_track_id() else {
            return Vec::new();
        };
        let Some(chain) = self.song_doc.song().fx_chain_by_track_id(track_id) else {
            return Vec::new();
        };
        let mut out: Vec<u64> = Vec::with_capacity(self.selection.selected_device_ids.len());
        for &id in &self.selection.selected_device_ids {
            if chain.iter().any(|d| d.id == id) && !out.contains(&id) {
                out.push(id);
            }
        }
        out
    }

    /// まだ生きているクリップだけを通す (削除済 / undo で消えた key は `None`)。
    /// **id ↔ index の変換は無くなった** — 住所は [`ClipKey`] 1 本で、
    /// アレンジのクリップもランチャーのセルも同じ id 空間に居る (r.md #87)。
    #[must_use]
    pub fn live_clip_key(&self, key: ClipKey) -> Option<ClipKey> {
        self.song_doc.song().clip_by_key(key).map(|_| key)
    }

    /// 対象 (代表) クリップ = **範囲の先頭に最も近い**交差クリップ。
    ///
    /// 旧「最後にクリックした anchor」 の後継。 選択の SSoT が範囲 1 本になったので
    /// 代表も範囲から導出する — クリップヘッダをクリックすれば範囲はそのクリップの
    /// 占有区間になるので、結果は「クリックしたクリップ」で一致する。
    pub fn selected_clip_ref(&self) -> Option<ClipKey> {
        let song = self.song_doc.song();
        self.selected_clip_refs()
            .into_iter()
            .min_by(|a, b| {
                let sa = song.clip_by_key(*a).map_or(f64::INFINITY, |c| c.start_beat);
                let sb = song.clip_by_key(*b).map_or(f64::INFINITY, |c| c.start_beat);
                sa.total_cmp(&sb)
            })
    }

    /// 選択範囲から**消えた行**を落とす (トラック削除 / undo / グループ解除の後始末)。
    ///
    /// 範囲はクリップ id を持たず「区間 × 行」しか持たないので、後始末は
    /// 「行がまだ在るか」 を見るだけで済む。 行が全部消えたら選択解除。
    /// **冪等** — 何度呼んでも同じ結果。
    pub(crate) fn prune_selection_lanes(&mut self) {
        let Some(sel) = self.selection.time.as_mut() else {
            return;
        };
        let song = self.song_doc.song();
        sel.lanes.retain(|lane| match *lane {
            common::model::LaneRef::Track(id) => song.track_by_id(id).is_some(),
            common::model::LaneRef::Automation(key) => {
                song.automation_lane_by_key(key.track, key.lane).is_some()
            }
            common::model::LaneRef::KeyTrack { clip, .. }
            | common::model::LaneRef::AudioLane(clip) => song.clip_by_key(clip).is_some(),
        });
        if sel.lanes.is_empty() {
            self.selection.time = None;
            self.selection.range_anchor = None;
        }
    }

    /// 選択範囲そのもの (SSoT)。
    #[must_use]
    pub fn time_selection(&self) -> Option<&common::model::TimeSelection> {
        self.selection.time.as_ref()
    }

    /// 選択範囲を差し替える**唯一の口**。
    ///
    /// 範囲が非空なら `last_edit_select` を `TimeRange` に倒し、掛かっている
    /// トラックを追従選択する (旧 `set_clip_selection` の副作用と同じ)。
    pub(crate) fn set_time_selection(&mut self, next: Option<common::model::TimeSelection>) {
        let first_track = next.as_ref().and_then(|t| t.track_ids().next());
        self.selection.time = next;
        self.drop_cell_selection_if_arrangement();
        // 範囲を張り直したら、そこに**入っていない** automation クリップ / 点の選択は
        // 落とす。 選択の SSoT は範囲 1 本なので、範囲の外に残った選択は
        // 「クリップを選んだのに Z が別のオートメーションクリップへ飛ぶ」
        // 「Delete が範囲でなく古い点を消す」 の形で顔を出す (実機で報告)。
        self.prune_automation_selection();
        if let Some(face) = self.time_selection_surface() {
            self.selection.last_edit_select = Some(face);
            if let Some(tid) = first_track {
                self.select_track(tid);
            }
        }
    }

    /// automation クリップ群を「選択」する = 範囲をその**外接区間 × それらのレーン行**へ
    /// 張り直す (クリップ選択の `apply_clip_range` と同じ規約)。
    /// 空を渡したら範囲は触らない (選択解除は空きレーン click 側の仕事)。
    pub(crate) fn select_automation_clip_range(
        &mut self,
        keys: &[common::model::AutomationClipKey],
    ) {
        if keys.is_empty() {
            return;
        }
        let (mut start, mut end) = (f64::INFINITY, f64::NEG_INFINITY);
        let mut lanes: Vec<common::model::LaneRef> = Vec::new();
        {
            let song = self.song_doc.song();
            for k in keys {
                let Some(clip) = song
                    .automation_lane_by_key(k.track, k.lane)
                    .and_then(|l| l.clip_by_id(k.clip))
                else {
                    continue;
                };
                start = start.min(clip.start_beat);
                end = end.max(clip.start_beat + clip.length_beats);
                let lane = common::model::LaneRef::Automation(k.lane_key());
                if !lanes.contains(&lane) {
                    lanes.push(lane);
                }
            }
        }
        if !start.is_finite() || end <= start {
            return;
        }
        self.set_time_selection(common::model::TimeSelection::new(start, end, lanes));
    }

    /// 範囲の外に残った automation クリップ / 点の選択を落とす。
    ///
    /// 「入っている」= そのレーン行が範囲に掛かっていて、かつ位置が範囲に重なること。
    /// 範囲そのものが無ければ全部落とす。
    fn prune_automation_selection(&mut self) {
        let Some(sel) = self.selection.time.clone() else {
            self.selection.selected_automation_clips.clear();
            self.selection.selected_automation_points.clear();
            return;
        };
        let song = self.song_doc.song();
        let on_lane = |lane: common::model::AutomationLaneKey| {
            sel.has_lane(common::model::LaneRef::Automation(lane))
        };
        let keep_clips: Vec<common::model::AutomationClipKey> = self
            .selection
            .selected_automation_clips
            .iter()
            .filter(|k| {
                song.automation_lane_by_key(k.track, k.lane)
                    .and_then(|l| l.clip_by_id(k.clip))
                    .is_some_and(|c| {
                        on_lane(k.lane_key()) && sel.intersects(c.start_beat, c.length_beats)
                    })
            })
            .copied()
            .collect();
        let keep_points: Vec<crate::app_types::AutomationPointKeyRef> = self
            .selection
            .selected_automation_points
            .iter()
            .filter(|k| {
                let lane = common::model::AutomationLaneKey { track: k.track_id, lane: k.lane_id };
                song.automation_lane_by_key(k.track_id, k.lane_id)
                    .and_then(|l| l.clip_by_id(k.clip_id))
                    .and_then(|c| {
                        let pts = song.clip_contents.get(&c.content_id)?.automation_points()?;
                        let p = pts.get(k.point_idx as usize)?;
                        Some(c.content_to_song_beat(p.time_beat))
                    })
                    // 点は幅ゼロなので端に乗るのが普通 — 両端を含む判定にする。
                    .is_some_and(|beat| on_lane(lane) && sel.contains_beat(beat))
            })
            .copied()
            .collect();
        self.selection.selected_automation_clips = keep_clips;
        self.selection.selected_automation_points = keep_points;
    }

    /// 範囲が**どの面**の上に居るかは、掛かっているレーンの種類が既に持っている
    /// (`docs/plan_range_selection.md` §2.2)。 面ごとのタイブレークは要らない。
    ///
    /// | レーン | 面 |
    /// |---|---|
    /// | 鍵盤行 ([`LaneRef::KeyTrack`]) | [`EditSurface::Notes`] |
    /// | 波形行 ([`LaneRef::AudioLane`]) | [`EditSurface::AudioEvents`] |
    /// | トラック行 / オートメーションレーン行 | [`EditSurface::TimeRange`] |
    #[must_use]
    pub fn time_selection_surface(&self) -> Option<EditSurface> {
        use common::model::LaneRef as L;
        let sel = self.selection.time.as_ref()?;
        if sel.lanes.iter().any(|l| matches!(l, L::KeyTrack { .. })) {
            return Some(EditSurface::Notes);
        }
        if sel.lanes.iter().any(|l| matches!(l, L::AudioLane(_))) {
            return Some(EditSurface::AudioEvents);
        }
        Some(EditSurface::TimeRange)
    }

    // -------- per-clip piano roll / audio editor view 状態 --------

    /// 現在ピアノロールで開いている (= 対象) クリップの表示状態。
    /// entry が無ければ `PianoRollViewState::default()` (= 64/14/84/0)。
    pub fn piano_roll_view_state(&self) -> common::model::PianoRollViewState {
        // 複数表示は共有 viewport (`multi_clip_view`、song-absolute scroll)、
        // 単一は per-clip 永続 state (clip-local scroll) を返す。
        if self.shown_pianoroll_clips().len() >= 2 {
            self.ui_prefs.multi_clip_view
        } else {
            self.pianoroll_target_clip()
                .and_then(|k| self.ui_prefs.piano_roll_views.get(&k).copied())
                .unwrap_or_default()
        }
    }

    /// piano roll view を可変で得る。複数表示中は共有 transient viewport
    /// (`multi_clip_view`、song-absolute scroll、非永続) を返し、単一表示は per-clip 永続 state
    /// (`piano_roll_views[anchor]`、無ければ default 挿入) を返す。読み出し (`piano_roll_view_state`)
    /// と同じ分岐 (`shown_pianoroll_clips().len() >= 2`) を使い、scroll/zoom/top_pitch の編集が
    /// 表示と同じ viewport に書かれることを保証する。選択クリップが無いときは `None` (no-op)。
    pub(crate) fn piano_roll_view_entry(&mut self) -> Option<&mut common::model::PianoRollViewState> {
        if self.shown_pianoroll_clips().len() >= 2 {
            return Some(&mut self.ui_prefs.multi_clip_view);
        }
        let key = self.pianoroll_target_clip()?;
        Some(self.ui_prefs.piano_roll_views.entry(key).or_default())
    }

    /// ピアノロール横ズーム (px/beat)。view 層はこの accessor 経由で読む。
    pub fn pianoroll_zoom_x(&self) -> f32 {
        self.piano_roll_view_state().zoom_x
    }

    /// ピアノロール縦ズーム (px/semitone)。
    pub fn pianoroll_zoom_y(&self) -> f32 {
        self.piano_roll_view_state().zoom_y
    }

    /// ピアノロール表示上端ピッチ (MIDI note)。
    pub fn pianoroll_top_pitch(&self) -> u8 {
        self.piano_roll_view_state().top_pitch
    }

    /// ピアノロール横スクロール (clip-local beats)。
    pub fn pianoroll_scroll_beat(&self) -> f32 {
        self.piano_roll_view_state().scroll_beat
    }

    /// 現在 Audio Editor で開いているクリップの表示状態 (`audio_editor_clip` で解決)。
    /// entry が無ければ default (`{0,0}` = 「未設定」、view 側でクリップ全長表示に倒れる)。
    pub fn audio_editor_view_state(&self) -> common::model::AudioEditorViewState {
        self.ui_ephemeral.audio_editor_clip
            .and_then(|r| self.live_clip_key(r))
            .and_then(|k| self.ui_prefs.audio_editor_views.get(&k).copied())
            .unwrap_or_default()
    }

    /// Audio Editor 表示開始位置 (clip-relative beats)。
    pub fn audio_editor_view_start_beat(&self) -> f64 {
        self.audio_editor_view_state().start_beat
    }

    /// Audio Editor 表示 span (beats、`0.0` = クリップ全体)。
    pub fn audio_editor_view_len_beats(&self) -> f64 {
        self.audio_editor_view_state().len_beats
    }

    /// 「選択されているクリップ」 (開始拍順)。 **クリップを対象にする操作
    /// (インスペクタの各節 / 改名 / preview の選択枠 / ノートの貼り付け先 / 声) は
    /// すべてここから導出する** — 別の集合を持たない。
    ///
    /// 選択の SSoT は 2 面あり排他 (`select_launcher_cell` がアレンジの範囲を降ろし、
    /// [`Self::drop_cell_selection_if_arrangement`] がセルを降ろす) なので、択一で足りる:
    /// - ランチャーのセル選択が生きていれば**そのセル** (トラック行のみ。 レーン行の
    ///   セルは `Clip` ではないので入らない)。 判定は **セル選択が生きているか** 1 つだけ
    ///   で、 last-wins タグ (`last_edit_select`) は見ない — あれは「Delete / Cut が
    ///   どの面に効くか」 の関心事で、 ピアノロール内でノートを選んだだけで `Notes` へ
    ///   倒れる (r.md #90)。
    /// - そうでなければ [`Self::arrangement_selected_clip_refs`]。
    ///
    /// 以前はアレンジ側しか導出しておらず、 セルを選ぶとインスペクタの Image / Text /
    /// Audio Event 節が出ない・preview に選択枠が出ない・F2 改名やノート貼り付けが
    /// 「クリップ未選択」 になっていた (r.md #97)。 `Track::clip_by_id` はセルも引けるので、
    /// 呼び出し側は key の出所を区別しなくてよい。
    pub fn selected_clip_refs(&self) -> Vec<ClipKey> {
        if self.has_live_launcher_cells() {
            return self
                .live_launcher_cells()
                .into_iter()
                .filter_map(|cell| match cell {
                    crate::event_launcher::LauncherCellKey::Track(k) => Some(k),
                    crate::event_launcher::LauncherCellKey::Lane(_) => None,
                })
                .collect();
        }
        self.arrangement_selected_clip_refs()
    }

    /// 選択範囲と**交差**する**アレンジの**クリップ群 (開始拍順)。 セルは含まない。
    ///
    /// 交差 (完全内包ではない) なので、範囲をかすめただけのクリップもインスペクタや
    /// 改名の対象に入る (`docs/plan_range_selection.md` §3)。 一方 Delete / `J` /
    /// ミュートなどの**範囲操作は範囲そのものに効く**ので、かすめたクリップが
    /// 丸ごと消えることはない。
    ///
    /// 直接呼んでよいのはアレンジ widget の選択ハイライト
    /// (`ArrangementFrame::selected_clips` は帯のセルを混ぜない契約 —
    /// `LauncherView::selected` の doc) だけ。 それ以外は [`Self::selected_clip_refs`]。
    pub fn arrangement_selected_clip_refs(&self) -> Vec<ClipKey> {
        let Some(sel) = self.selection.time.as_ref() else {
            return Vec::new();
        };
        let song = self.song_doc.song();
        let mut out: Vec<(f64, ClipKey)> = Vec::new();
        for lane in &sel.lanes {
            let common::model::LaneRef::Track(track_id) = lane else {
                continue;
            };
            let Some(track) = song.track_by_id(*track_id) else {
                continue;
            };
            for clip in &track.clips {
                if sel.intersects(clip.start_beat, clip.length_beats) {
                    out.push((
                        clip.start_beat,
                        ClipKey { track_id: *track_id, clip_id: clip.id },
                    ));
                }
            }
        }
        out.sort_by(|a, b| a.0.total_cmp(&b.0));
        out.into_iter().map(|(_, k)| k).collect()
    }

    /// ピアノロールに同時表示する MIDI クリップ群を開始拍順で返す
    /// ([`Self::selected_clip_refs`] → MIDI のみ filter)。 ランチャーのセルを
    /// 選んでいるときは**そのセル**が開く (セルは時間軸に居ないので範囲では
    /// 表せず、唯一のオブジェクト選択として残っている —
    /// `docs/plan_range_selection.md` §2.2。 判定の根拠は `selected_clip_refs` の doc)。
    ///
    /// 範囲が部分的にしか掛かっていないクリップも**表示する** — 範囲でノートを
    /// 編集する用途 (Live §10.8.3 の multi-clip editing) がそこで成立する。
    /// **この順序が packed note id の `clip_slot` の SSoT** (`decode_note_id` と必ず一致させる)。
    #[must_use]
    pub fn shown_pianoroll_clips(&self) -> Vec<ClipKey> {
        // トラック行 / セルから拾ったクリップに加えて、**鍵盤行が直接指しているクリップ**も
        // 表示集合に入れる。 ノート選択の範囲は鍵盤行が主役なので、これが無いと
        // 「ノートを選んだ瞬間にピアノロールが空になる」。
        let mut out = self.selected_clip_refs();
        if let Some(sel) = self.selection.time.as_ref() {
            for lane in &sel.lanes {
                if let common::model::LaneRef::KeyTrack { clip, .. } = lane
                    && !out.contains(clip)
                    && self.live_clip_key(*clip).is_some()
                {
                    out.push(*clip);
                }
            }
        }
        out.retain(|r| self.is_midi_clip(*r));
        out
    }

    /// 現在の対象 (target) クリップ = 新規ノートの所属先・凡例で強調される行。
    /// focus ヒント (`ui_ephemeral.pianoroll_focus_clip`) が表示集合に含まれていれば
    /// それを、含まれなければ**範囲の先頭に最も近い**表示クリップを返す。
    /// 表示 MIDI クリップが無いときは `None`。
    /// **target を切り替えても `shown_pianoroll_clips` の順序 (= packed id の clip_slot) は
    /// 変わらない** ので、target 変更で `selected_notes` を clear する必要はない (anchor の
    /// ポインタが動くだけ)。
    #[must_use]
    pub fn pianoroll_target_clip(&self) -> Option<ClipKey> {
        let shown = self.shown_pianoroll_clips();
        if let Some(focus) = self.ui_ephemeral.pianoroll_focus_clip
            && shown.contains(&focus)
        {
            return Some(focus);
        }
        shown.first().copied()
    }

    /// piano_roll widget へ渡す **グローバル note id**。
    /// 上位 8 bit = `clip_slot` (`shown_pianoroll_clips()` 内の位置 0..=255)、
    /// 下位 24 bit = clip 内 note index (0..=16M)。複数クリップ重畳表示で id 衝突を防ぐ。
    #[must_use]
    pub fn pack_note_id(clip_slot: usize, note_index: usize) -> u32 {
        ((clip_slot as u32) << 24) | (note_index as u32 & 0x00FF_FFFF)
    }

    /// packed note id の上位 8 bit (= clip_slot) だけを取り出す。view 層が
    /// song-absolute → clip-local 変換で「その note の所属クリップ」を引くのに使う
    /// (bit レイアウトを `pack_note_id` と 1 箇所に集約する)。
    #[must_use]
    pub fn note_id_clip_slot(id: u32) -> usize {
        (id >> 24) as usize
    }

    /// packed note id の下位 24 bit (= clip 内 note index)。
    #[must_use]
    pub fn note_id_local_index(id: u32) -> usize {
        (id & 0x00FF_FFFF) as usize
    }

    /// `resolve_note_overlaps` がクリップ `slot` に返した remap を、packed な
    /// `selected_notes` のうち当該クリップ部分にだけ適用する (他クリップは不変)。
    /// `remap[old_local] = Some(new_local)` は追従、None / 範囲外は選択から落とす。
    /// `prev` は **編集前** に読んだ選択 (packed id)。 選択は範囲からの導出なので、
    /// ノートが動いた後に読むと「動く前の範囲」で解決してしまい、移動・移調のたびに
    /// 選択が外れる。 呼び出し側が編集の前に捕まえて渡すこと。
    pub(crate) fn remap_packed_selection_for_clip(
        &mut self,
        slot: usize,
        remap: &[Option<u32>],
        prev: &[u32],
    ) {
        let mut out = Vec::with_capacity(prev.len());
        for &packed in prev {
            if Self::note_id_clip_slot(packed) == slot {
                let local = Self::note_id_local_index(packed);
                if let Some(Some(new_local)) = remap.get(local) {
                    out.push(Self::pack_note_id(slot, *new_local as usize));
                }
            } else {
                out.push(packed);
            }
        }
        self.set_note_selection(&(out));
    }

    /// クリップ `slot` (= `r`) の notes に `f` を適用 (local index ベースで編集し、
    /// 重なり解決の勝者にする local index 群を返す) → `resolve_note_overlaps` で同ピッチ
    /// 重なりを解消 → packed `selected_notes` の当該クリップ部分を remap、という複数クリップ
    /// note 編集の共通基盤。snap 等の immutable 計算は呼び出し側で済ませて `f` に閉じ込める。
    pub(crate) fn edit_clip_notes(&mut self, slot: usize, r: ClipKey, f: impl FnOnce(&mut Vec<Note>) -> Vec<u32>) {
        // **編集前**の選択を捕まえてから編集する (`remap_packed_selection_for_clip` の doc)。
        let prev = self.selected_note_ids();
        let Some(Some(remap)) = self.edit_song(move |song| {
            let notes = song.notes_in_clip_mut(r)?;
            let winners = f(notes);
            Some(resolve_note_overlaps(notes, &winners))
        }) else {
            return;
        };
        self.remap_packed_selection_for_clip(slot, &remap, &prev);
    }

    /// クリップの **content 原点** の song-absolute 拍。content-local note ⇄
    /// song-absolute 変換の唯一のオフセット (範囲外は 0)。
    ///
    /// r.md #44: clip は content への窓なので、原点は `start_beat` ではなく
    /// `start_beat - content_offset_beats` ([`Clip::content_origin_beat`])。
    /// 左端 trim した clip でも note の song 上の位置は動かない。
    #[must_use]
    pub fn clip_start_beat_of(&self, r: ClipKey) -> f64 {
        self.song_doc.song()
            .track_by_id(r.track_id)
            .and_then(|t| t.clip_by_id(r.clip_id))
            .map(common::model::Clip::content_origin_beat)
            .unwrap_or(0.0)
    }

    /// packed note id を持つ `entries` を **所属クリップ (clip_slot) ごと** に
    /// グルーピングし、各クリップで `per_clip(self, slot, ClipKey, &[(local_index, payload)])` を
    /// 呼ぶ。範囲外 slot / ロック中クリップは飛ばす (ロックは widget が hit 除外済だが二重防御)。
    /// payload は handler ごとに異なる (移動=(beat,pitch)、リサイズ=(beat,len)、velocity=u8、
    /// 削除/複製=`()` 等)。複数クリップ note 編集 handler の共通ディスパッチ。slot 昇順で適用
    /// するので、各クリップ内 index ベースの remove も安定する。
    pub(crate) fn for_each_note_clip_group<T>(
        &mut self,
        entries: impl IntoIterator<Item = (u32, T)>,
        mut per_clip: impl FnMut(&mut Self, usize, ClipKey, &[(usize, T)]),
    ) {
        let shown = self.shown_pianoroll_clips();
        let mut groups: std::collections::BTreeMap<usize, Vec<(usize, T)>> =
            std::collections::BTreeMap::new();
        for (id, payload) in entries {
            groups
                .entry(Self::note_id_clip_slot(id))
                .or_default()
                .push((Self::note_id_local_index(id), payload));
        }
        for (slot, items) in groups {
            let Some(&r) = shown.get(slot) else { continue };
            if self.is_pianoroll_clip_locked_in(&shown, r) {
                continue;
            }
            per_clip(self, slot, r, &items);
        }
    }

    /// packed note id を `(ClipKey, clip 内 index)` に分解。`shown` は
    /// `shown_pianoroll_clips()` の結果 (呼び出し側で 1 度作って使い回す)。`clip_slot` が
    /// 範囲外なら `None`。
    #[must_use]
    pub fn decode_note_id_in(shown: &[ClipKey], id: u32) -> Option<(ClipKey, usize)> {
        let clip_slot = (id >> 24) as usize;
        let note_index = (id & 0x00FF_FFFF) as usize;
        shown.get(clip_slot).copied().map(|r| (r, note_index))
    }

    /// 単発デコード (内部で `shown_pianoroll_clips()` を 1 度計算)。多数の id を
    /// 捌くハンドラでは `shown` を 1 度作って `decode_note_id_in` を使う (再計算を避ける)。
    #[must_use]
    pub fn decode_note_id(&self, id: u32) -> Option<(ClipKey, usize)> {
        Self::decode_note_id_in(&self.shown_pianoroll_clips(), id)
    }

    /// クリップ `r` 内の local note index 群を、現在の表示集合 (`shown_pianoroll_clips`)
    /// における **packed note id** に変換する。新規ノート (add / paste / step 入力) の結果選択を
    /// packed 化する共通基盤。`r` が表示集合に無ければ slot 0 (= 単一表示と byte 互換) に倒す。
    pub(crate) fn pack_clip_selection(&self, r: ClipKey, locals: &[u32]) -> Vec<u32> {
        let slot = self
            .shown_pianoroll_clips()
            .iter()
            .position(|s| *s == r)
            .unwrap_or(0);
        locals
            .iter()
            .map(|&l| Self::pack_note_id(slot, l as usize))
            .collect()
    }

    /// ピアノロールが今そのスケールで動いているか (`None` = スケール未設定)。
    ///
    /// 判定基準は **対象 (target) クリップの窓の開始拍のスケール** で、view と handler の
    /// 両方がここを読む (r.md #67)。 view 側だけで導出していると、カーソルキーの
    /// ↑/↓ が「画面では Fold 表示なのに半音単位で動く」 ように食い違う。
    ///
    /// `mode` は Fold トグルに従う (`Fold` = out-of-scale 行を畳んだ表示)。
    #[must_use]
    pub fn pianoroll_scale(&self) -> Option<crate::widgets::piano_roll::PianoRollScale> {
        use crate::widgets::piano_roll::{PianoRollScale, PianoRollScaleMode};
        let target = self.pianoroll_target_clip()?;
        let clip = self
            .song_doc
            .song()
            .track_by_id(target.track_id)
            .and_then(|t| t.clip_by_id(target.clip_id))?;
        let sc = self.song_doc.song().scale_at(clip.start_beat)?;
        Some(PianoRollScale {
            root: sc.root,
            in_scale_mask: sc.scale.pitch_class_mask(),
            mode: if self.ui_prefs.piano_roll_fold {
                PianoRollScaleMode::Fold
            } else {
                PianoRollScaleMode::Highlight
            },
            prefer_flats: common::scale::prefers_flats(sc.root, sc.scale),
        })
    }

    /// そのトラックがピアノロールの凡例に **行を持つか**
    /// (`shown` = [`Self::shown_pianoroll_clips`] の結果)。
    ///
    /// **これがロックの効力範囲の SSoT** (r.md #64)。 凡例パネルは複数クリップ表示
    /// (`shown.len() >= 2`) のときだけ出るので、単一表示では空 = ロックは効かない。
    ///
    /// 旧実装は「効力 = `locked_pr_tracks` を直接読む (常時)」 と「解除 UI = 複数表示のとき
    /// だけ描く」 が別々に書かれていた。 ロックしたトラックのクリップを 1 つだけ開くと
    /// **ゴースト表示のまま掴めず、解除ボタンも画面に無い** = プロジェクトを開き直す以外に
    /// 復帰できない詰みになる。 効力を凡例行から導出すれば
    /// 「ロックが効いている ⟺ 解除ボタンが見えている」 が構造的な不変条件になる
    /// (`locked_pr_tracks` は「ユーザーの意思」 の生データとして残り、効力は毎回導出)。
    ///
    /// 「選択が変わった瞬間に `locked_pr_tracks` を prune する」 方式は採らない:
    /// `selected_clips` の書き込み点は 10 箇所以上に散っていてチョークポイントが無く、
    /// 「参照を貼り替える補償コード」 (アーキテクチャ不変条件 1 が禁じるパターン) になる。
    #[must_use]
    pub fn has_pianoroll_lock_row(shown: &[ClipKey], track: u32) -> bool {
        // 凡例パネルは複数クリップ表示のときだけ出て、行は表示クリップのトラック 1 つにつき 1 行。
        shown.len() >= 2 && shown.iter().any(|r| r.track_id == track)
    }

    /// [`Self::has_pianoroll_lock_row`] が真になるトラック index を **初出順** で列挙する
    /// (凡例パネルの行そのもの)。 行の集合が要る描画側が使う。 判定 1 件だけなら
    /// alloc しない述語版 (`has_pianoroll_lock_row`) を使うこと。
    #[must_use]
    pub fn pianoroll_lock_rows_in(shown: &[ClipKey]) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        for r in shown {
            if Self::has_pianoroll_lock_row(shown, r.track_id) && !out.contains(&r.track_id) {
                out.push(r.track_id);
            }
        }
        out
    }

    /// 単発版 (内部で `shown_pianoroll_clips()` を 1 度計算)。凡例の描画側が使う。
    #[must_use]
    pub fn pianoroll_lock_rows(&self) -> Vec<u32> {
        Self::pianoroll_lock_rows_in(&self.shown_pianoroll_clips())
    }

    /// そのクリップが乗っている **トラック** のロックが *いま効いているか*
    /// (r.md #64)。 `shown` は [`Self::shown_pianoroll_clips`] の結果
    /// (呼び出し側で 1 度作って使い回す、`decode_note_id_in` と同じイディオム)。
    ///
    /// 効力 = 「凡例に行がある」 ∧ 「そのトラックがロック集合に入っている」。
    /// 前者が [`Self::pianoroll_lock_rows_in`] = 解除 UI の描画条件そのものなので、
    /// 解除できないロックが存在しえない。
    #[must_use]
    pub fn is_pianoroll_clip_locked_in(&self, shown: &[ClipKey], r: ClipKey) -> bool {
        Self::has_pianoroll_lock_row(shown, r.track_id)
            && self
                .song_doc
                .song()
                .track_by_id(r.track_id)
                .is_some_and(|t| self.ui_prefs.locked_pr_tracks.contains(&t.id))
    }

    /// 単発版 (内部で `shown_pianoroll_clips()` を 1 度計算)。多数の clip を捌く
    /// ハンドラでは `shown` を 1 度作って `is_pianoroll_clip_locked_in` を使う。
    #[must_use]
    pub fn is_pianoroll_clip_locked(&self, r: ClipKey) -> bool {
        self.is_pianoroll_clip_locked_in(&self.shown_pianoroll_clips(), r)
    }

    /// ロック中クリップへの **書き込み** を拒否する共通ゲート (r.md #64)。
    ///
    /// ロックは既存ノートの編集経路 (`for_each_note_clip_group`) では効いていたが、
    /// **新規ノートを生む経路** (鉛筆 / Insert / 貼り付け / ステップ入力) はロックを
    /// まったく見ていなかった。 結果「既存ノートは掴めないのに新しいノートは描ける」
    /// という、生む経路と触る経路で判定が食い違う状態になっていた。
    ///
    /// 拒否したときは理由をステータスバーに出す (何も起きないと故障に見えるため)。
    /// 戻り値 `true` = 拒否した (呼び出し側は即 return)。
    pub(crate) fn reject_write_if_pianoroll_locked(&mut self, r: ClipKey) -> bool {
        if !self.is_pianoroll_clip_locked(r) {
            return false;
        }
        let name = self
            .song_doc
            .song()
            .track_by_id(r.track_id)
            .map_or_else(String::new, |t| format!("「{}」 ", t.name));
        self.ui_ephemeral.status_message =
            format!("{name}トラックはロック中です (凡例の L で解除)");
        true
    }

    /// トラック id がピアノロールでロック中か (凡例のロックトグル状態表示用)。
    /// **効力ではなくユーザーの意思**を返す — 凡例が出ている行にしか使わないので、
    /// その文脈では [`Self::is_pianoroll_clip_locked_in`] と必ず一致する。
    pub fn is_pianoroll_track_locked(&self, track_id: u32) -> bool {
        self.ui_prefs.locked_pr_tracks.contains(&track_id)
    }

    /// 凡例から対象 (target) クリップを切り替える。 **選択 (範囲) は変えない** —
    /// focus ヒント (`ui_ephemeral.pianoroll_focus_clip`) を動かすだけなので
    /// `shown_pianoroll_clips` の順序 (= packed id の clip_slot) は不変で、
    /// `selected_notes` も維持される。 表示集合に居ない key は no-op。 track も追従。
    pub(crate) fn set_pianoroll_target_clip(&mut self, key: common::model::ClipKey) {
        // 凡例は常に表示集合内のクリップしか出さないが、stale 入力に備えて検証する。
        if !self.shown_pianoroll_clips().contains(&key) {
            return;
        }
        self.ui_ephemeral.pianoroll_focus_clip = Some(key);
        if let Some(r) = self.live_clip_key(key) {
            self.select_track(r.track_id);
        }
    }

    /// 凡例から **トラック** のロック (参照専用) を反転。非永続な view 状態
    /// (`locked_pr_tracks`)。ロック中はそのトラックの表示 note を widget が hit 除外し、
    /// 編集 handler も飛ばす (`for_each_note_clip_group` / `is_pianoroll_clip_locked`)。
    pub(crate) fn toggle_pianoroll_track_lock(&mut self, track_id: u32) {
        if !self.ui_prefs.locked_pr_tracks.remove(&track_id) {
            self.ui_prefs.locked_pr_tracks.insert(track_id);
        }
    }

    /// inspector の編集対象クリップ群 = [`Self::selected_clip_refs`] そのもの
    /// (セルを選んでいればそのセル群、 アレンジなら**範囲と交差するクリップ全体** —
    /// `docs/plan_range_selection.md` 質問 27b)。 範囲をかすめただけのクリップも
    /// 対象に入る — 属性操作 (改名 / 色 / ゲイン / 声 / 画像の位置) はクリップという
    /// オブジェクトに効くので、範囲操作 (Delete / `J` / ミュート) とは対象の決め方が違う。
    pub(crate) fn for_each_inspector_target(&self, mut f: impl FnMut(ClipKey)) {
        for r in self.selected_clip_refs() {
            f(r);
        }
    }

    pub fn inspector_target_refs(&self) -> Vec<ClipKey> {
        let mut refs = Vec::new();
        self.for_each_inspector_target(|r| refs.push(r));
        refs
    }

    /// 編集対象クリップ各々に `extract` を適用し、 値が全て一致すれば
    /// `Some(値)`、 割れていれば `None` (= mixed) を返す。 `extract` が `None` を返す
    /// クリップ (= その field を持たない種別) は無視する。 表示中の section のアンカーは
    /// 必ずその種別なので、 表示中 field では `None` == mixed と解釈できる。 毎フレーム
    /// 全 field で呼ばれるので alloc しない (`for_each_inspector_target` を使う)。
    pub fn inspector_fold(&self, extract: impl Fn(&AppData, ClipKey) -> Option<f64>) -> Option<f64> {
        let mut acc: Option<f64> = None;
        let mut mixed = false;
        self.for_each_inspector_target(|t| {
            if mixed {
                return;
            }
            if let Some(v) = extract(self, t) {
                match acc {
                    None => acc = Some(v),
                    Some(a) if (a - v).abs() > 1e-6 => mixed = true,
                    _ => {}
                }
            }
        });
        if mixed { None } else { acc }
    }

    /// `target` clip の first `ImageEvent` に `f` を適用 (image clip でなければ `None`)。
    /// mixed 畳み込み (`inspector_fold`) 用 accessor。
    pub fn image_first_event<R>(
        &self,
        target: ClipKey,
        f: impl FnOnce(&common::model::ImageEvent) -> R,
    ) -> Option<R> {
        let content_id = self
            .song_doc.song()
            .track_by_id(target.track_id)?
            .clip_by_id(target.clip_id)?
            .content_id;
        match self.song_doc.song().clip_contents.get(&content_id)? {
            common::model::ClipContent::Image(img) => img.events.first().map(f),
            _ => None,
        }
    }

    /// `target` clip の first `TextEvent` に `f` を適用 (text clip でなければ `None`)。
    pub fn text_first_event<R>(
        &self,
        target: ClipKey,
        f: impl FnOnce(&common::model::TextEvent) -> R,
    ) -> Option<R> {
        let content_id = self
            .song_doc.song()
            .track_by_id(target.track_id)?
            .clip_by_id(target.clip_id)?
            .content_id;
        match self.song_doc.song().clip_contents.get(&content_id)? {
            common::model::ClipContent::Text(text) => text.events.first().map(f),
            _ => None,
        }
    }

    /// `target` clip の first `AudioEvent` に `f` を適用 (audio clip でなければ `None`)。
    pub fn audio_first_event<R>(
        &self,
        target: ClipKey,
        f: impl FnOnce(&common::model::AudioEvent) -> R,
    ) -> Option<R> {
        let content_id = self
            .song_doc.song()
            .track_by_id(target.track_id)?
            .clip_by_id(target.clip_id)?
            .content_id;
        match self.song_doc.song().clip_contents.get(&content_id)? {
            common::model::ClipContent::Audio(audio) => audio.events.first().map(f),
            _ => None,
        }
    }

    /// text num field を `inspector_target_refs` 全体で畳む (mixed 検出)。
    pub fn inspector_text_num_folded(&self, field: TextNumField) -> Option<f64> {
        self.inspector_fold(|a, t| a.text_first_event(t, |e| text_event_num_value(e, field)))
    }

    /// stable `ClipKey` → `&Clip` (track_by_id + clip_by_id)。
    pub fn clip_at(&self, key: common::model::ClipKey) -> Option<&common::model::Clip> {
        self.song_doc.song()
            .track_by_id(key.track_id)
            .and_then(|t| t.clip_by_id(key.clip_id))
    }

    /// クリップを選択する。 `additive` (Ctrl+クリック) なら**範囲を外接まで広げる** —
    /// 離れた 2 クリップを拾うと間のクリップも入る (Live 実機と同じ)。
    /// 既に範囲がそのクリップだけを覆っているところへ additive で同じクリップを
    /// 指した場合は選択解除 (トグル)。
    pub(crate) fn select_clip(&mut self, target: ClipKey, additive: bool) {
        let Some(clip) = self.clip_at(target) else {
            return;
        };
        let (start, end) = clip.song_window();
        if additive && let Some(sel) = self.selection.time.as_mut() {
            sel.extend(start, end, [common::model::LaneRef::Track(target.track_id)]);
            let first = sel.track_ids().next();
            self.selection.last_edit_select = Some(EditSurface::TimeRange);
            self.drop_cell_selection_if_arrangement();
            if let Some(tid) = first {
                self.select_track(tid);
            }
            self.recording.step_cursor_beat = 0.0;
            return;
        }
        self.apply_clip_range(&[target], true);
    }

    /// クリップ群を「選択」する = 範囲をその**外接区間 × それらのトラック**へ張り直す。
    /// 空を渡すと選択解除。
    pub(crate) fn set_clip_selection(&mut self, targets: Vec<ClipKey>) {
        self.apply_clip_range(&targets, true);
    }

    /// 新規 clip 群 (`ClipKey`) を選択にする (view ジャンプ無し)。
    /// clone / split / glue / bounce の結果選択用。
    pub(crate) fn select_new_clips(&mut self, refs: &[ClipKey]) {
        self.apply_clip_range(refs, false);
    }

    /// 単一 clip (新規作成直後) を選択にする (view ジャンプ無し)。
    pub(crate) fn set_single_clip_selection(&mut self, r: ClipKey) {
        self.apply_clip_range(&[r], false);
    }

    /// クリップ群 → 範囲への変換 (上の 4 経路の実体)。
    ///
    /// `jump_view` が `true` で、対象クリップの per-clip view をまだ記憶していない
    /// (= 初めて開く) ときだけピアノロールを auto-fit する。 記憶があれば復元に任せ、
    /// 再選択で view が飛ばないようにする (明示的な再 fit は `X` キー)。
    fn apply_clip_range(&mut self, targets: &[ClipKey], jump_view: bool) {
        let song = self.song_doc.song();
        let mut start = f64::INFINITY;
        let mut end = f64::NEG_INFINITY;
        let mut lanes: Vec<common::model::LaneRef> = Vec::new();
        for key in targets {
            let Some(clip) = song.clip_by_key(*key) else {
                continue;
            };
            let (s, e) = clip.song_window();
            start = start.min(s);
            end = end.max(e);
            let lane = common::model::LaneRef::Track(key.track_id);
            if !lanes.contains(&lane) {
                lanes.push(lane);
            }
        }
        let next = if lanes.is_empty() {
            None
        } else {
            common::model::TimeSelection::new(start, end, lanes)
        };
        self.set_time_selection(next);
        self.selection.range_anchor = self.selection.time.as_ref().map(|t| t.start_beat);
        self.recording.step_cursor_beat = 0.0;
        if jump_view
            && let Some(p) = self.selected_clip_ref()
            && !self.ui_prefs.piano_roll_views.contains_key(&p)
        {
            self.fit_piano_roll_to_clip();
        }
    }

    /// Ctrl+A (クリップ領域): 曲全体 × 全トラックを範囲にする。
    /// 全選択は一括操作なので view ジャンプ (fit / トラック追従) を起こさない。
    /// 既に全選択なら冪等。
    pub(crate) fn select_all_clips(&mut self) {
        let song = self.song_doc.song();
        let mut end = 0.0_f64;
        let lanes: Vec<common::model::LaneRef> = song
            .tracks
            .iter()
            .map(|t| {
                for c in &t.clips {
                    end = end.max(c.start_beat + c.length_beats);
                }
                common::model::LaneRef::Track(t.id)
            })
            .collect();
        if lanes.is_empty() || end <= 0.0 {
            return;
        }
        let next = common::model::TimeSelection::new(0.0, end, lanes);
        // 冪等 early-return より前に last-wins 面だけは更新する (既に全選択でも
        // 「Ctrl+A = 範囲面を選んだ」 という意図は確定している)。
        self.selection.last_edit_select = Some(EditSurface::TimeRange);
        if self.selection.time != next {
            self.selection.time = next;
            self.selection.range_anchor = Some(0.0);
        }
        // 冪等 early-return より後 (既に全選択でも「アレンジの面を選んだ」は確定)。
        self.drop_cell_selection_if_arrangement();
    }


    /// Ctrl+A (ピアノロール): **表示中の全 MIDI クリップ** の全ノートを packed note id で返す。
    /// 各 id = `pack_note_id(clip_slot, local_index)`。ロック中クリップは選択対象に
    /// しない (掴めないので除外)。表示クリップが無ければ空。
    pub fn all_shown_pianoroll_note_ids(&self) -> Vec<u32> {
        let shown = self.shown_pianoroll_clips();
        let mut out = Vec::new();
        for (slot, &r) in shown.iter().enumerate() {
            if self.is_pianoroll_clip_locked_in(&shown, r) {
                continue;
            }
            let Some(track) = self.song_doc.song().track_by_id(r.track_id) else {
                continue;
            };
            let Some(clip) = track.clip_by_id(r.clip_id) else {
                continue;
            };
            let n = self.song_doc.song().clip_notes(clip).len();
            out.extend((0..n).map(|local| Self::pack_note_id(slot, local)));
        }
        out
    }

    /// Ctrl+A (オーディオエディタ): 開いている clip の全 audio event index
    /// を返す。 audio_editor_clip が無い / 非 audio なら空。
    pub fn all_audio_event_indices(&self) -> Vec<usize> {
        let Some(target) = self.ui_ephemeral.audio_editor_clip else {
            return Vec::new();
        };
        let Some(track) = self.song_doc.song().track_by_id(target.track_id) else {
            return Vec::new();
        };
        let Some(clip) = track.clip_by_id(target.clip_id) else {
            return Vec::new();
        };
        match self.song_doc.song().clip_contents.get(&clip.content_id) {
            Some(common::model::ClipContent::Audio(audio)) => (0..audio.events.len()).collect(),
            _ => Vec::new(),
        }
    }

    /// Ctrl+A (automation lane): 指定 lane 内の全ポイントを
    /// `AutomationPointKeyRef` で列挙する。 lane.clips の各 clip の content
    /// (`ClipContent::Automation`) points を走査。 master row
    /// (`MASTER_TRACK_ID`) も `automation_lane_by_key` 経由で対応。
    /// lane が無い / ポイントが無いなら空。
    pub fn all_automation_points_in_lane(
        &self,
        lane: common::model::AutomationLaneKey,
    ) -> Vec<AutomationPointKeyRef> {
        let Some(lane_ref) = self.song_doc.song().automation_lane_by_key(lane.track, lane.lane) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for clip in &lane_ref.clips {
            let n = match self.song_doc.song().clip_contents.get(&clip.content_id) {
                Some(common::model::ClipContent::Automation(a)) => a.points.len(),
                _ => 0,
            };
            for point_idx in 0..n as u32 {
                out.push(AutomationPointKeyRef {
                    track_id: lane.track,
                    lane_id: lane.lane,
                    clip_id: clip.id,
                    point_idx,
                });
            }
        }
        out
    }

    /// Ctrl+A (automation lane / #071): 指定 lane 内の全 automation clip を
    /// `AutomationClipKey` で列挙する。 lane が無い / clip が無いなら空。
    /// `all_automation_points_in_lane` の clip 版 (= Ctrl+A 段階拡大の clip 段)。
    pub fn all_automation_clips_in_lane(
        &self,
        lane: common::model::AutomationLaneKey,
    ) -> Vec<common::model::AutomationClipKey> {
        let Some(lane_ref) = self.song_doc.song().automation_lane_by_key(lane.track, lane.lane) else {
            return Vec::new();
        };
        lane_ref
            .clips
            .iter()
            .map(|clip| common::model::AutomationClipKey {
                track: lane.track,
                lane: lane.lane,
                clip: clip.id,
            })
            .collect()
    }


    /// 選択**範囲**にピアノロールを合わせる (`docs/plan_range_selection.md` §3)。
    ///
    /// [`Self::fit_piano_roll_to_clip`] がクリップ全体を映すのに対し、こちらは
    /// **範囲そのもの**を横幅いっぱいに映す。 アレンジャーで範囲を引いたときは
    /// 「その範囲を見たい」 のであって、掛かったクリップ全体を見たいわけではない。
    /// 縦は範囲の中に居るノートで合わせ、無ければ表示クリップのノート全体に倒す。
    pub(crate) fn fit_piano_roll_to_range(&mut self) {
        let Some(sel) = self.selection.time.clone() else {
            return;
        };
        let shown = self.shown_pianoroll_clips();
        if shown.is_empty() {
            return;
        }
        let (grid_w, grid_h) = self.ui_ephemeral.last_pianoroll_grid_size;
        if grid_w < 16.0 || grid_h < 16.0 {
            self.ui_ephemeral.pending_pianoroll_fit = true;
            return;
        }
        // 横: 範囲そのもの (前後に 1 拍の余白)。 scroll の座標系は読み出し
        // (`piano_roll_view_state`) と view (`view_start_beat`) の multi/single 分岐に合わせる。
        let multi = shown.len() >= 2;
        let origin = if multi {
            0.0
        } else {
            self.clip_at(shown[0]).map_or(0.0, |c| c.start_beat - c.content_offset_beats)
        };
        let span_beats = (sel.len_beats() + 2.0).max(1.0);
        // 縦: 範囲に入っているノートの音域 (無ければ表示クリップ全体)。
        let (mut lo, mut hi) = (u8::MAX, u8::MIN);
        {
            let song = self.song_doc.song();
            for key in &shown {
                let Some(clip) = song.clip_by_key(*key) else {
                    continue;
                };
                for n in song.clip_notes(clip) {
                    let start = clip.content_to_song_beat(n.start_beat);
                    if !sel.intersects(start, n.duration_beats) {
                        continue;
                    }
                    lo = lo.min(n.pitch);
                    hi = hi.max(n.pitch);
                }
            }
            if lo > hi {
                for key in &shown {
                    let Some(clip) = song.clip_by_key(*key) else {
                        continue;
                    };
                    for n in song.clip_notes(clip) {
                        lo = lo.min(n.pitch);
                        hi = hi.max(n.pitch);
                    }
                }
            }
        }
        let (top_pitch, zoom_y) = if lo > hi {
            (84, 14.0)
        } else {
            let span_pitch = (i32::from(hi) - i32::from(lo) + 4).max(4);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
            let z = (grid_h / span_pitch as f32).clamp(6.0, 40.0);
            ((i32::from(hi) + 2).clamp(11, 127) as u8, z)
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
        let fitted = common::model::PianoRollViewState {
            scroll_beat: (sel.start_beat - 1.0 - origin).max(0.0) as f32,
            zoom_x: (f64::from(grid_w) / span_beats).clamp(8.0, 400.0) as f32,
            top_pitch,
            zoom_y,
        };
        if multi {
            self.ui_prefs.multi_clip_view = fitted;
        } else {
            self.ui_prefs.piano_roll_views.insert(shown[0], fitted);
        }
    }

    pub(crate) fn fit_piano_roll_to_clip(&mut self) {
        // 表示中の **全 MIDI クリップ** の note bbox を union して zoom/scroll/pitch を
        // 算出する。複数表示は song-absolute (note.start + clip.start_beat) で集計し共有 transient
        // viewport (`multi_clip_view`) に書く。単一表示は clip-local (= 旧挙動) で per-clip 永続
        // view に書く (regression なし)。scroll の座標系は read accessor (`piano_roll_view_state`)
        // と view (`view_start_beat`) の multi/single 分岐に一致させる。
        let shown = self.shown_pianoroll_clips();
        if shown.is_empty() {
            return;
        }
        let (grid_w, grid_h) = self.ui_ephemeral.last_pianoroll_grid_size;
        if grid_w < 16.0 || grid_h < 16.0 {
            self.ui_ephemeral.pending_pianoroll_fit = true;
            return;
        }
        let multi = shown.len() >= 2;

        let mut min_beat = f64::INFINITY;
        let mut max_beat = f64::NEG_INFINITY;
        let mut min_pitch = u8::MAX;
        let mut max_pitch = u8::MIN;
        let mut note_count = 0usize;
        // notes ゼロのとき用に clip 群の span (single = clip 長、multi = clip 群 union)。
        let mut union_start = f64::INFINITY;
        let mut union_end = f64::NEG_INFINITY;
        for &r in &shown {
            let Some(track) = self.song_doc.song().track_by_id(r.track_id) else {
                continue;
            };
            let Some(clip) = track.clip_by_id(r.clip_id) else {
                continue;
            };
            // r.md #44: note は content-local なので song 化は content 原点基準。
            // clip の帯 (窓) はそれとは別に `start_beat` / `content_offset_beats`。
            let (offset, win_start) = if multi {
                (clip.content_origin_beat(), clip.start_beat)
            } else {
                (0.0, clip.content_offset_beats)
            };
            union_start = union_start.min(win_start);
            union_end = union_end.max(win_start + clip.length_beats);
            for n in self.song_doc.song().clip_notes(clip) {
                note_count += 1;
                let s = n.start_beat + offset;
                min_beat = min_beat.min(s);
                max_beat = max_beat.max(s + n.duration_beats);
                min_pitch = min_pitch.min(n.pitch);
                max_pitch = max_pitch.max(n.pitch);
            }
        }

        let fitted = if note_count == 0 {
            let start = if union_start.is_finite() { union_start } else { 0.0 };
            let span = if union_end > union_start {
                union_end - union_start
            } else {
                1.0
            }
            .max(1.0);
            common::model::PianoRollViewState {
                scroll_beat: start.max(0.0) as f32,
                zoom_x: (f64::from(grid_w) / span).clamp(8.0, 400.0) as f32,
                top_pitch: 84,
                zoom_y: 14.0,
            }
        } else {
            let span_beats = (max_beat - min_beat + 2.0).max(1.0);
            let span_pitch = (i32::from(max_pitch) - i32::from(min_pitch) + 4).max(4);
            common::model::PianoRollViewState {
                scroll_beat: (min_beat - 1.0).max(0.0) as f32,
                zoom_x: (f64::from(grid_w) / span_beats).clamp(8.0, 400.0) as f32,
                top_pitch: (i32::from(max_pitch) + 2).clamp(11, 127) as u8,
                zoom_y: (grid_h / span_pitch as f32).clamp(6.0, 40.0),
            }
        };

        if multi {
            self.ui_prefs.multi_clip_view = fitted;
        } else if let Some(key) = self.pianoroll_target_clip() {
            self.ui_prefs.piano_roll_views.insert(key, fitted);
        }
    }

    /// 親 group chain のいずれかが `collapsed_groups` に含まれる (= 折り畳まれた
    /// group の配下で hide される) か。 arrangement widget の `is_visible_track`
    /// と同じ判定を daw_01 側で行い、 mixer の strip 折り畳み が
    /// arrangement と同じ可視集合を共有する (`collapsed_groups` が SSoT)。
    /// 32 hop で cycle 安全。
    pub fn is_hidden_under_collapsed_group(&self, track_id: u32) -> bool {
        let mut cursor = self
            .song_doc.song()
            .track_by_id(track_id)
            .and_then(|t| t.parent_group_id);
        let mut hops = 0u8;
        while let Some(pid) = cursor {
            if self.ui_prefs.collapsed_groups.contains(&pid) {
                return true;
            }
            hops += 1;
            if hops > 32 {
                break;
            }
            cursor = self.song_doc.song().track_by_id(pid).and_then(|t| t.parent_group_id);
        }
        false
    }

    /// 追従方式の status_message 用ラベル (Alt+F / ドロップダウンの可視フィードバック)。
    pub(crate) fn follow_mode_label(mode: common::model::FollowMode) -> &'static str {
        use common::model::FollowMode;
        match mode {
            FollowMode::Off => "追従スクロール: OFF",
            FollowMode::Scroll => "追従スクロール: 連続",
            FollowMode::Page => "追従スクロール: ページめくり",
        }
    }

    /// 再生追従スクロールの新しい `arrange_scroll_beat` を計算する純関数 (テスト可能)。
    /// `scroll` は現在の左端拍、 `visible_beats` は可視拍数 (lanes_w / zoom)、
    /// `playhead` は現在の再生位置 (拍)。 view を動かす必要が無ければ `None`。
    ///
    /// - `Page`: プレイヘッドが可視範囲 `[scroll, scroll+visible)` の外 (右端到達 or
    ///   逆方向シーク / ループ折返し) なら、 プレイヘッドが左端に来るようページめくり。
    ///   範囲内なら据え置き (= Ableton "Page" の「据え置き + 1 ページジャンプ」)。
    /// - `Scroll`: プレイヘッドを画面中央に固定 (`scroll = playhead - visible/2`、
    ///   曲頭付近は 0 で頭打ち)。 微小変化は無視して無駄な再描画を避ける。
    /// - `Off`: 常に `None`。
    pub(crate) fn follow_scroll_beat(
        mode: common::model::FollowMode,
        playhead: f32,
        scroll: f32,
        visible_beats: f32,
    ) -> Option<f32> {
        use common::model::FollowMode;
        if visible_beats <= 0.0 {
            return None;
        }
        match mode {
            FollowMode::Off => None,
            FollowMode::Page => {
                let right = scroll + visible_beats;
                (playhead >= right || playhead < scroll).then(|| playhead.max(0.0))
            }
            FollowMode::Scroll => {
                let target = (playhead - visible_beats * 0.5).max(0.0);
                // 1/1000 拍未満の揺れでは動かさない (毎 tick の無駄な scroll 更新を避ける)。
                ((target - scroll).abs() > 1e-3).then_some(target)
            }
        }
    }

    /// 追従方式を直接設定する (トランスポートのドロップダウン)。
    pub(crate) fn set_arrange_follow(&mut self, mode: common::model::FollowMode) {
        self.ui_prefs.arrange_follow = mode;
        self.ui_ephemeral.status_message = Self::follow_mode_label(mode).into();
    }

    /// `Alt+F`: 追従方式を `Off → Scroll → Page → Off` と循環する。
    pub(crate) fn cycle_arrange_follow(&mut self) {
        use common::model::FollowMode;
        let next = match self.ui_prefs.arrange_follow {
            FollowMode::Off => FollowMode::Scroll,
            FollowMode::Scroll => FollowMode::Page,
            FollowMode::Page => FollowMode::Off,
        };
        self.set_arrange_follow(next);
    }

    /// 再生中にユーザーが手動でアレンジビューを動かしたら追従を解除する
    /// (ユーザー選択: 手動スクロール / ズームで Follow OFF)。 停止中は no-op
    /// (追従は再生中のみ作用するので、 停止中の view 操作で状態を変える必要がない。
    /// これで「停止中にスクロール → 再生で追従再開」 が成立する)。
    pub(crate) fn cancel_follow_on_manual_view_change(&mut self) {
        if self.transport.is_playing {
            self.ui_prefs.arrange_follow = common::model::FollowMode::Off;
        }
    }

    /// 全 track の全 clip が arrangement の lanes 領域に収まるよう zoom_x / scroll_beat /
    /// track_row_h を自動調整する。clip 0 個なら song.length_beats でフォールバック。
    ///
    /// 縦は「全行の高さ合計 == lanes 高さ」 を **厳密に** 成立させる (= 最下段の行の下端が
    /// 画面下端にぴったり揃い、 余白もはみ出しも残らない)。 Ardour の Fit Selection
    /// (`Editor::fit_tracks`, gtk2_ardour/editor_ops.cc) と同じく行高に上限は設けない
    /// (下限だけ。 Ardour も `h < preset_height(HeightSmall)` のときだけ警告して最小に張り付く)。
    pub(crate) fn fit_arrange_to_content(&mut self) {
        // X (fit) は明示的な view 操作なので再生中は追従を解除する (= follow が
        // 次 tick で fit を上書きして戻すのを防ぐ)。
        self.cancel_follow_on_manual_view_change();
        let (lanes_w, lanes_h) = self.ui_ephemeral.last_arrange_lanes_size;
        if lanes_w < 16.0 || lanes_h < 16.0 {
            return;
        }
        // 収める行は widget が前フレームに実際に積んだ行そのもの (master 行 + 可視 track 行 +
        // 展開中の可視 automation lane 行)。 可視集合をモデルから再導出すると、 widget 側の
        // lane 除外条件が 1 つ増えただけで silent に fit がズレる。
        let fit_lane_keys: Vec<common::model::AutomationLaneKey> = self
            .ui_ephemeral
            .last_arrange_rows
            .iter()
            .filter_map(|r| match r.key {
                crate::widgets::arrangement::ArrangementRowKey::Lane(k) => Some(k),
                crate::widgets::arrangement::ArrangementRowKey::Track(_) => None,
            })
            .collect();
        let row_count = self.ui_ephemeral.last_arrange_rows.len();
        if row_count == 0 {
            return;
        }

        let (min_beat, max_beat) = self
            .song_doc.song()
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), c| {
                (lo.min(c.start_beat), hi.max(c.start_beat + c.length_beats))
            });
        let (min_beat, max_beat) = if min_beat.is_finite() {
            (min_beat, max_beat)
        } else {
            (0.0, self.song_doc.song().length_beats.max(16.0))
        };

        let span_beats = (max_beat - min_beat + 4.0).max(4.0);
        self.ui_prefs.arrange_scroll_beat = (min_beat - 2.0).max(0.0) as f32;
        self.ui_prefs.arrange_zoom_x = (f64::from(lanes_w) / span_beats).clamp(2.0, 400.0) as f32;
        // 全行を等高で lanes_h に敷き詰める。 automation lane の行高は u16 (整数 px) しか
        // 持てないので、 まず理想高を整数へ丸めて lane に配り、 **端数を f32 の track 行高が
        // 吸収する**。 これで `n_track * row_h + n_lane * lane_px == lanes_h` が厳密に成立し、
        // 最下段の行の下端が lanes の下端にぴったり揃う (lane 行に丸めた分を配らないと
        // 最大 `0.5 × lane 数` px はみ出す)。 行が lanes に収まりきらない (= 1 行 16px 未満に
        // なる) ときだけ下限に張り付いて溢れ、 縦スクロールで見る (Ardour fit_tracks と同じ)。
        let lane_count = fit_lane_keys.len();
        let track_row_count = row_count - lane_count;
        #[allow(clippy::cast_precision_loss)]
        let ideal_row_h = lanes_h / row_count as f32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let lane_px = (ideal_row_h.max(MIN_ARRANGE_ROW_H).round() as u16).max(1);
        #[allow(clippy::cast_precision_loss)]
        let row_h = if track_row_count == 0 {
            // 起こらない (master 行が必ず居る) が、 0 除算だけは構造的に塞ぐ。
            ideal_row_h.max(MIN_ARRANGE_ROW_H)
        } else {
            ((lanes_h - f32::from(lane_px) * lane_count as f32) / track_row_count as f32)
                .max(MIN_ARRANGE_ROW_H)
        };
        self.ui_prefs.arrange_track_row_h = row_h;
        // 「全 track / lane を上端から収める」 のが fit の定義なので:
        //   - 縦スクロールを 0 に戻す (怠ると row 高だけ縮んで track_top が残り、
        //     全 track が viewport 上方へ押し出されて見えなくなる = ユーザー報告のバグ)。
        //   - per-track 行高 override を消す (= row_h は uniform 前提で算出している。
        //     override が残ると 1 track が巨大化して他が画面外に押し出される)。
        //   - `Z` の段階ズーム履歴 / アンカーをリセット (= 明示的な fit はズーム状態の
        //     終端。 残すと fit 後の `X` が古いズームへ巻き戻って状態が食い違う)。
        //   - automation lane も同じ fit 行高へ scale する session override を
        //     張り直す (model height_px は保存対象なので汚さない)。 これで「track だけ
        //     縮んで automation レーンが高いまま」 を解消。 Z 拡大の残り override も
        //     ここで上書きされる。 splitter resize / fresh Z で個別に解除される。
        self.ui_prefs.arrange_track_top = 0.0;
        self.ui_prefs.track_row_overrides.clear();
        self.ui_ephemeral.arrange_zoom_history.clear();
        self.ui_ephemeral.arrange_zoom_anchor = None;
        self.ui_prefs.automation_lane_row_overrides.clear();
        for k in fit_lane_keys {
            self.ui_prefs.automation_lane_row_overrides.insert(k, lane_px);
        }
        // ここから先の override は fit が所有する (Z が捨ててよいものは無い)。
        self.ui_ephemeral.zoom_lane_fill = None;
    }

    /// `Z` キーの段階ズーム。 選択素材 (通常 clip + automation
    /// clip) を arrangement に framing する。
    /// - 1 回目: bounding beat span を幅いっぱいに **横ズーム**。
    /// - 2 回目: **縦ズーム**。 automation clip を選んでいればその primary レーンを
    ///   viewport の高さいっぱいに拡大 + 上端へ scroll (`automation_lane_row_overrides`
    ///   の session override)。 そうでなければ選択 clip の track 群を viewport に収める。
    /// - 3 回目以降: 何もしない (横+縦ズーム済み)。
    ///
    /// **仕切り直し**: 直近 Z 以降に選択が変わった or ユーザーが手動で
    /// ズーム / スクロールした (= `arrange_zoom_anchor.applied_view` と現在 view が
    /// 食い違う) ときは段階を 0 に戻し、 新しい選択へ横ズームし直す。 これにより
    /// 「別 clip を選んで Z → その clip にズーム」「マウスでズームを変えた後の Z →
    /// 取り直し」 が成立する。
    ///
    /// 各段で適用前の view を `arrange_zoom_history` に積み、 `X`
    /// (`arrange_zoom_back`) が 1 段ずつ巻き戻す。
    pub(crate) fn zoom_arrange_to_selected_clip(&mut self, automation: bool) {
        // Z (zoom-to-selection) は明示的な view 操作なので再生中は追従を解除する。
        self.cancel_follow_on_manual_view_change();
        // 「どこへズームするか」を決める入力を 1 行に出す。 Z は明示操作なので
        // 毎フレームは走らない。 「選んだものと違うところへ飛んだ」 類の切り分けは
        // この 1 行が無いと画面から逆算できない (実際、行高の跳ねはここで確定した)。
        tracing::info!(
            automation,
            range = ?self.selection.time.as_ref().map(|t| (t.start_beat, t.end_beat, t.lanes.len())),
            span = ?self.arrange_selection_beat_span(automation),
            auto_clips = self.selection.selected_automation_clips.len(),
            auto_points = self.selection.selected_automation_points.len(),
            last_face = ?self.selection.last_edit_select,
            hovered_lane = ?self.ui_ephemeral.arrange_hovered_automation_lane,
            lane_overrides = self.ui_prefs.automation_lane_row_overrides.len(),
            lane_fill = ?self.ui_ephemeral.zoom_lane_fill,
            "Z: zoom to selection"
        );
        let sig = self.current_zoom_selection_sig(automation);
        // 直近アンカーと同じ選択 + view が手付かずなら段階を継続、 それ以外は仕切り直し。
        let stage = match self.ui_ephemeral.arrange_zoom_anchor.take() {
            Some(a) if a.sig == sig && self.arrange_view_matches(&a.applied_view) => a.stage,
            _ => 0,
        };
        let new_stage = match stage {
            0 => self.zoom_arrange_horizontal(automation).then_some(1),
            1 => self.zoom_arrange_vertical(automation).then_some(2),
            // 既に横+縦ズーム済み: view を変えずアンカーだけ維持。
            _ => Some(stage),
        };
        if let Some(stage) = new_stage {
            self.ui_ephemeral.arrange_zoom_anchor = Some(ArrangeZoomAnchor {
                sig,
                applied_view: self.capture_arrange_view(),
                stage,
            });
        }
        // new_stage == None: 適用不能 (選択無し等)。 アンカーは None のまま (= 次も仕切り直し)。
    }

    /// `Z` 1 段目: 対象面 (`automation`) の選択素材の bounding beat span を幅いっぱいに
    /// 横ズーム。 適用したら `true`、 選択無し / canvas 過小なら `false` (view 不変)。
    pub(crate) fn zoom_arrange_horizontal(&mut self, automation: bool) -> bool {
        let Some((min_start, max_end)) = self.arrange_selection_beat_span(automation) else {
            return false;
        };
        let (lanes_w, _) = self.ui_ephemeral.last_arrange_lanes_size;
        if lanes_w < 16.0 {
            return false;
        }
        // **1 段目は横だけ。 行高には他人の分まで触らない。**
        //
        // 以前はここで `automation_lane_row_overrides` を map ごと捨てていた
        // (前セッションの lane 拡大を持ち越さないため)。 だがあの map には `Z` の
        // 縦ズームが広げた行だけでなく **fit (`X`) が縮めた行高**も同居するので、
        // 捨てると「1 回目の Z でオートメーションレーンだけ急に高くなる」
        // (= model の `height_px` へ戻る) になる (実機で報告)。 捨ててよいのは
        // **自分が広げた 1 行だけ**で、誰が書いたかは `ui_ephemeral.zoom_lane_fill`
        // が持つ。 fit は自分で clear + 張り直し、 `X` は履歴の snapshot
        // (`lane_row_overrides` 込み) を丸ごと復元するので、そちらの後始末は不要。
        if let Some(key) = self.ui_ephemeral.zoom_lane_fill.take() {
            self.ui_prefs.automation_lane_row_overrides.remove(&key);
        }
        let snap = self.capture_arrange_view();
        self.ui_ephemeral.arrange_zoom_history.push(snap);
        let span = max_end - min_start;
        // clip が canvas 幅の ~92% を占めるよう左右に proportional padding
        // (短い clip でも極端に拡大しすぎないよう最小 0.5 beat)。
        let pad = (span * 0.04).max(0.5);
        self.ui_prefs.arrange_scroll_beat = (min_start - pad).max(0.0) as f32;
        self.ui_prefs.arrange_zoom_x =
            (f64::from(lanes_w) / (span + pad * 2.0)).clamp(2.0, 400.0) as f32;
        true
    }

    /// `Z` 2 段目: 縦ズーム。 automation clip 選択時は primary レーンを viewport
    /// いっぱいに拡大 (lane height override) + 上端へ scroll、 それ以外は選択 track
    /// 群を viewport に収める。 適用したら `true`、 lanes 過小 / 対象解決不能なら
    /// `false` (view 不変)。
    pub(crate) fn zoom_arrange_vertical(&mut self, automation: bool) -> bool {
        use crate::widgets::arrangement::ArrangementRowKey;
        let lanes_h = self.ui_ephemeral.last_arrange_lanes_size.1;
        if lanes_h < 16.0 {
            return false;
        }
        // 対象面が automation clip なら、 そのレーンを viewport 高いっぱいに拡大する
        // (= MIDI track の縦ズームの「レーン版」)。 レーンの content-Y 上端は widget が
        // 積んだ行そのもの (`last_arrange_rows`) から引く。
        if automation
            && let Some(lane_key) = self
                .selection
                .selected_automation_clips
                .last()
                .map(|k| k.lane_key())
            && let Some(row) = self
                .ui_ephemeral
                .last_arrange_rows
                .iter()
                .find(|r| r.key == ArrangementRowKey::Lane(lane_key))
        {
            let content_top = row.content_top;
            let snap = self.capture_arrange_view();
            self.ui_ephemeral.arrange_zoom_history.push(snap);
            // レーン高 = viewport 高 (u16 へ saturating)。 レーンより上の行高は
            // 変わらないので content_top はそのままレーン上端の絶対 y。
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let lane_px = lanes_h.clamp(MIN_ARRANGE_ROW_H, f32::from(u16::MAX)) as u16;
            self.ui_prefs.automation_lane_row_overrides.insert(lane_key, lane_px);
            // 「この 1 行は Z が広げた」と記録する (次の fresh な Z が捨てる対象)。
            self.ui_ephemeral.zoom_lane_fill = Some(lane_key);
            self.ui_prefs.arrange_track_top = content_top.max(0.0);
            return true;
        }
        // 通常 clip 選択: 選択 track 群 (と、 その track が展開している automation lane) が
        // viewport いっぱいになるよう track 行を uniform 行高にし、 先頭行へ scroll する。
        //
        // lane 行の高さは据え置き、 **残りを track 行で割る** (Ardour `Editor::fit_tracks` の
        // `child_heights` と同じ扱い — 選択 track の子レーンぶんを viewport 高から先に引く)。
        // 行の高さも content-Y も widget が積んだ行から引くので、 選択 track の上に展開中の
        // レーンがあっても scroll 位置がズレない (一様行高の掛け算で再導出すると外れる)。
        let Some((first, last)) = self.selected_row_span(automation) else {
            return false;
        };
        let rows = &self.ui_ephemeral.last_arrange_rows;
        let span = &rows[first..=last];
        let lane_h_in_span: f32 = span
            .iter()
            .filter(|r| matches!(r.key, ArrangementRowKey::Lane(_)))
            .map(|r| r.height)
            .sum();
        let track_rows_in_span =
            span.iter().filter(|r| matches!(r.key, ArrangementRowKey::Track(_))).count();
        if track_rows_in_span == 0 {
            return false;
        }
        // fit と同じく上限は設けない (viewport 高さそのものが実質の上限)。
        #[allow(clippy::cast_precision_loss)]
        let row_h =
            ((lanes_h - lane_h_in_span) / track_rows_in_span as f32).max(MIN_ARRANGE_ROW_H);
        // 新しい行高で数え直した「先頭行より上の高さ合計」 が scroll 位置
        // (track 行は全部 row_h に、 lane 行は現在高のまま)。
        let track_top: f32 = rows[..first]
            .iter()
            .map(|r| match r.key {
                ArrangementRowKey::Track(_) => row_h,
                ArrangementRowKey::Lane(_) => r.height,
            })
            .sum();
        let snap = self.capture_arrange_view();
        self.ui_ephemeral.arrange_zoom_history.push(snap);
        self.ui_prefs.track_row_overrides.clear();
        self.ui_prefs.arrange_track_row_h = row_h;
        self.ui_prefs.arrange_track_top = track_top;
        true
    }

    /// `Z` 段階ズームの選択シグネチャ (通常 clip 群 / primary clip / automation clip 群
    /// / 対象面)。 これが変わると別対象とみなして段階 0 (横ズーム) から仕切り直す。
    pub(crate) fn current_zoom_selection_sig(&self, automation: bool) -> ZoomSelectionSig {
        ZoomSelectionSig {
            clips: self.selected_clip_refs(),
            clip: self.selected_clip_ref(),
            automation: self.selection.selected_automation_clips.clone(),
            target_automation: automation,
        }
    }

    /// 現在の arrangement view が `snap` と一致するか (= 直近 Z 以降ユーザーが手動で
    /// ズーム / スクロール / 行高変更をしていないか)。 段階ズームの仕切り直し判定に使う。
    pub(crate) fn arrange_view_matches(&self, snap: &ArrangeViewSnapshot) -> bool {
        self.capture_arrange_view() == *snap
    }

    /// r.md #63: 対象面 (`automation`) の選択素材が乗っている track 群を、 widget が積んだ行
    /// (`last_arrange_rows`) の index 範囲 `(first, last)` (両端含む) で返す。
    ///
    /// `first` は最初の選択 track の **track 行**、 `last` は最後の選択 track の行群の末尾
    /// (= その track が展開している automation lane 行まで含む。 Ardour `Editor::fit_tracks` が
    /// 選択 track の `child_heights` を viewport 高さから先に引くのと同じ範囲)。 選択と選択の
    /// 間に挟まる非選択 track の行も範囲に入る (画面上そこに居るので収める対象)。
    /// master 行に乗る automation clip (`track == MASTER_TRACK_ID`) は master 行に対応する。
    /// 選択無し / どれも可視行に居ない / widget 未描画なら `None`。
    pub(crate) fn selected_row_span(&self, automation: bool) -> Option<(usize, usize)> {
        use crate::widgets::arrangement::ArrangementRowKey;
        // 対象 track id を対象面の選択から集める (master 行は `MASTER_TRACK_ID` で表現)。
        let mut track_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
        if automation {
            track_ids.extend(self.selection.selected_automation_clips.iter().map(|k| k.track));
        } else {
            // 範囲が掛かっている行のトラック (クリップが 1 つも無い行でも数える)。
            if let Some(sel) = self.selection.time.as_ref() {
                track_ids.extend(sel.track_ids());
            }
        }
        if track_ids.is_empty() {
            return None;
        }
        let (mut first, mut last) = (None, None);
        for (i, r) in self.ui_ephemeral.last_arrange_rows.iter().enumerate() {
            let owner = match r.key {
                ArrangementRowKey::Track(id) => id,
                ArrangementRowKey::Lane(k) => k.track,
            };
            if !track_ids.contains(&owner) {
                continue;
            }
            // 起点は必ず track 行 (lane 行だけ選択されている track の途中から始めない)。
            if first.is_none() && matches!(r.key, ArrangementRowKey::Track(_)) {
                first = Some(i);
            }
            if first.is_some() {
                last = Some(i);
            }
        }
        first.zip(last)
    }

    /// 対象面 (`automation`) の選択素材の bounding beat 範囲。 通常 clip と automation
    /// clip は直交して共存選択できる (他 DAW 互換) ため、 `Z`/`R` は union ではなく
    /// root の `edit_surface` arbiter が選んだ **片面のみ** を対象にする (last-selection
    /// -wins、 = 「MIDI clip を選んだのに残存 automation 選択へズームしてしまう」 を防ぐ)。
    /// 解決不能 / 退化 (長さ 0) なら `None`。 `Z` 横ズームと `R` loop が共有する。
    pub(crate) fn arrange_selection_beat_span(&self, automation: bool) -> Option<(f64, f64)> {
        let (mut min_start, mut max_end) = (f64::INFINITY, f64::NEG_INFINITY);
        if automation {
            // automation clip: lane (track / master) を解決して span を畳み込む。
            for &k in &self.selection.selected_automation_clips {
                if let Some(clip) = self
                    .song_doc.song()
                    .automation_lane_by_key(k.track, k.lane)
                    .and_then(|lane| lane.clip_by_id(k.clip))
                {
                    min_start = min_start.min(clip.start_beat);
                    max_end = max_end.max(clip.start_beat + clip.length_beats);
                }
            }
        } else {
            // 通常面: **範囲そのもの**が span (空き領域に引いた範囲でも `R` / `Z` が効く、
            // `docs/plan_range_selection.md` 質問 26)。
            if let Some(sel) = self.selection.time.as_ref() {
                min_start = sel.start_beat;
                max_end = sel.end_beat;
            }
        }
        (min_start.is_finite() && max_end > min_start).then_some((min_start, max_end))
    }

    /// 現在の arrangement view 状態を snapshot (ズーム履歴 push 用)。
    pub(crate) fn capture_arrange_view(&self) -> ArrangeViewSnapshot {
        ArrangeViewSnapshot {
            zoom_x: self.ui_prefs.arrange_zoom_x,
            scroll_beat: self.ui_prefs.arrange_scroll_beat,
            row_h: self.ui_prefs.arrange_track_row_h,
            track_top: self.ui_prefs.arrange_track_top,
            row_overrides: self.ui_prefs.track_row_overrides.clone(),
            lane_row_overrides: self.ui_prefs.automation_lane_row_overrides.clone(),
        }
    }

    /// `X` キー (arrangement)。 ズーム履歴があれば 1 段戻し、 無ければ全体フィット
    /// (= 「前のズームに戻る、 無ければ全体フィット」)。
    pub(crate) fn arrange_zoom_back(&mut self) {
        // X (zoom back / fit) は明示的な view 操作なので再生中は追従を解除する。
        self.cancel_follow_on_manual_view_change();
        if let Some(v) = self.ui_ephemeral.arrange_zoom_history.pop() {
            self.ui_prefs.arrange_zoom_x = v.zoom_x;
            self.ui_prefs.arrange_scroll_beat = v.scroll_beat;
            self.ui_prefs.arrange_track_row_h = v.row_h;
            self.ui_prefs.arrange_track_top = v.track_top;
            self.ui_prefs.track_row_overrides = v.row_overrides;
            self.ui_prefs.automation_lane_row_overrides = v.lane_row_overrides;
            // 復元した行高は snapshot の持ち物 (Z が捨ててよいものは無い)。
            self.ui_ephemeral.zoom_lane_fill = None;
        } else {
            self.fit_arrange_to_content();
        }
        // 1 段戻したら段階ズームのアンカーは無効 (次の Z は仕切り直し)。
        self.ui_ephemeral.arrange_zoom_anchor = None;
    }

}

