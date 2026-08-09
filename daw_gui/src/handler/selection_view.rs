//! handler::selection_view — clip/note 選択 + view state + arrange follow/zoom + piano-roll id 変換
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use crate::event::AppEvent;
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
        if self.ui_ephemeral.track_rename_id.is_some()
            || self.ui_ephemeral.section_rename_id.is_some()
            || self.ui_ephemeral.clip_rename.is_some()
        {
            return None;
        }
        // 選択集合は面を跨いで共存できる (lasso は automation の点とクリップを両方拾う、
        // clip 選択は automation 選択を消さない)。
        let audio_events = self.ui_ephemeral.audio_editor_clip.is_some()
            && !self.selection.audio_editor_selected_events.is_empty();
        let notes = !self.selection.selected_notes.is_empty();
        let points = !self.selection.selected_automation_points.is_empty();
        let auto_clips = !self.selection.selected_automation_clips.is_empty();
        // 安価な空判定 (selected_clip_refs() は Vec を確保するので避ける)。
        let clips =
            self.selection.selected_clip.is_some() || !self.selection.selected_clips.is_empty();
        let tracks = !self.selection.selected_track_ids.is_empty();
        let sections = !self.selection.selected_section_ids.is_empty();
        let auto_prefer_clips = auto_clips
            && (!points || self.selection.last_edit_select == Some(S::AutomationClips));
        // 1. ポインタが乗っている面を最優先 (選択が非空な面に限る)。
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
        // 2. 「最後に選んだ面」 がまだ非空ならそれ。
        let last_wins = match self.selection.last_edit_select {
            Some(S::AudioEvents) if audio_events => Some(S::AudioEvents),
            Some(S::Notes) if notes => Some(S::Notes),
            Some(S::AutomationPoints) if points => Some(S::AutomationPoints),
            Some(S::AutomationClips) if auto_clips => Some(S::AutomationClips),
            Some(S::Clips) if clips => Some(S::Clips),
            Some(S::Tracks) if tracks => Some(S::Tracks),
            Some(S::Sections) if sections => Some(S::Sections),
            _ => None,
        };
        if let Some(surface) = last_wins {
            return Some(surface);
        }
        // 3. タグが無いときだけ 非空優先順 (従来順)。 タグがあるのにここへ来たのは
        //    「その面が空になった」 = 対象なし。
        if self.selection.last_edit_select.is_some() {
            return None;
        }
        if audio_events {
            return Some(S::AudioEvents);
        }
        if points {
            return Some(S::AutomationPoints);
        }
        if notes {
            return Some(S::Notes);
        }
        if clips {
            return Some(S::Clips);
        }
        if auto_clips {
            return Some(S::AutomationClips);
        }
        None
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
            EditSurface::Clips => AppEvent::DeleteSelectedClip,
        };
        self.handle_event(event);
    }

    /// stable `ClipKey` (track_id + clip_id) → 現在の index ベース `ClipRef`。
    /// track / clip が見つからなければ `None` (= 削除済 / undo で消えた)。
    pub fn clip_ref_of(&self, key: common::model::ClipKey) -> Option<ClipRef> {
        let t_idx = self.song_doc.song().tracks.iter().position(|t| t.id == key.track_id)?;
        let c_idx = self.song_doc.song().tracks[t_idx]
            .clips
            .iter()
            .position(|c| c.id == key.clip_id)?;
        Some(ClipRef {
            track: t_idx as u32,
            clip: c_idx as u32,
        })
    }

    /// index ベース `ClipRef` → stable `ClipKey`。 範囲外なら `None`。
    pub fn clip_key_of(&self, r: ClipRef) -> Option<common::model::ClipKey> {
        let t = self.song_doc.song().tracks.get(r.track as usize)?;
        let c = t.clips.get(r.clip as usize)?;
        Some(common::model::ClipKey {
            track_id: t.id,
            clip_id: c.id,
        })
    }

    /// 選択 anchor (`selected_clip` = 末尾) を現在の `ClipRef` へ解決。
    pub fn selected_clip_ref(&self) -> Option<ClipRef> {
        self.selection.selected_clip.and_then(|k| self.clip_ref_of(k))
    }

    // -------- per-clip piano roll / audio editor view 状態 --------

    /// 現在ピアノロールで開いている (= 選択 anchor) クリップの表示状態。
    /// entry が無ければ `PianoRollViewState::default()` (= 64/14/84/0)。
    pub fn piano_roll_view_state(&self) -> common::model::PianoRollViewState {
        // 複数表示は共有 viewport (`multi_clip_view`、song-absolute scroll)、
        // 単一は per-clip 永続 state (clip-local scroll) を返す。
        if self.shown_pianoroll_clips().len() >= 2 {
            self.ui_prefs.multi_clip_view
        } else {
            self.selection.selected_clip
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
        let key = self.selection.selected_clip?;
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
            .and_then(|r| self.clip_key_of(r))
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

    /// 現在の表示状態 (ズーム / スクロール / 行高 / スナップ等 + per-clip view) を
    /// `ViewState` にスナップショットする。 save / autosave 時に呼ぶ。 per-clip map は
    /// 現存しないクリップの orphan entry を GC して書き出す。
    pub fn snapshot_view_state(&self) -> common::model::ViewState {
        let mut expanded: Vec<u32> = self.ui_prefs.expanded_automation_tracks.iter().copied().collect();
        expanded.sort_unstable();
        let mut piano_roll_views: Vec<(common::model::ClipKey, common::model::PianoRollViewState)> =
            self.ui_prefs.piano_roll_views
                .iter()
                .filter(|(k, _)| self.clip_ref_of(**k).is_some())
                .map(|(k, v)| (*k, *v))
                .collect();
        piano_roll_views.sort_by_key(|(k, _)| (k.track_id, k.clip_id));
        let mut audio_editor_views: Vec<(
            common::model::ClipKey,
            common::model::AudioEditorViewState,
        )> = self
            .ui_prefs.audio_editor_views
            .iter()
            .filter(|(k, _)| self.clip_ref_of(**k).is_some())
            .map(|(k, v)| (*k, *v))
            .collect();
        audio_editor_views.sort_by_key(|(k, _)| (k.track_id, k.clip_id));
        common::model::ViewState {
            arrange_zoom_x: self.ui_prefs.arrange_zoom_x,
            arrange_scroll_beat: self.ui_prefs.arrange_scroll_beat,
            arrange_follow: self.ui_prefs.arrange_follow,
            // 再生ループ (ON/OFF + 範囲) は transport が live SSoT。 ここへ書くことで
            // 「dirty は立てないが保存される」 (= ズーム / スクロールと同じ扱い)。
            loop_region: self.transport.loop_region,
            arrange_track_top: self.ui_prefs.arrange_track_top,
            arrange_track_row_h: self.ui_prefs.arrange_track_row_h,
            arrange_header_w: self.ui_prefs.arrange_header_w,
            track_row_overrides: self.ui_prefs.track_row_overrides.clone(),
            expanded_automation_tracks: expanded,
            master_row_automation_expanded: self.ui_prefs.master_row_automation_expanded,
            arrange_snap_enabled: self.ui_prefs.arrange_snap_enabled,
            arrange_snap_choice: self.ui_prefs.arrange_snap_choice,
            pianoroll_snap_enabled: self.ui_prefs.pianoroll_snap_enabled,
            pianoroll_snap_choice: self.ui_prefs.pianoroll_snap_choice,
            piano_roll_fold: self.ui_prefs.piano_roll_fold,
            snap_on_draw: self.ui_prefs.snap_on_draw,
            snap_live_input: self.recording.snap_live_input,
            bottom_panel: self.ui_prefs.bottom_panel,
            // 開いていたクリップを復元できるよう選択も保存 (現存クリップのみ)。
            selected_clip: self.selection.selected_clip.filter(|k| self.clip_ref_of(*k).is_some()),
            selected_clips: self
                .selection.selected_clips
                .iter()
                .copied()
                .filter(|k| self.clip_ref_of(*k).is_some())
                .collect(),
            piano_roll_views,
            audio_editor_views,
        }
    }

    /// load 時に `ViewState` を AppData へ流し込む。 別プロジェクトの per-clip
    /// view が漏れないよう **必ず先に map をクリア**。 `view = None` (旧ファイル /
    /// view 未保存) なら globals は現状維持 (= 従来の fit-to-content / 既定値挙動)。
    /// 全値を有効域へ clamp して壊れた / 古い保存値を吸収する。
    ///
    /// `loop_region` は `view` と**別引数**で受ける: v28 以前の `.daw` は `ViewState`
    /// を持たないのにループ範囲は持つので、 loader
    /// ([`common::project::LoadedProject::loop_region`]) が両方を解決した値を渡す。
    /// ここで `view` から取ると、 旧ファイル用に `ViewState::default()` を合成する
    /// 羽目になり globals が既定値へ潰れる。 engine への `SetLoop` 送出も込みで
    /// [`AppData::set_loop_region`] を通す (= 復元忘れの故障モードを作らない)。
    pub fn restore_view_state(
        &mut self,
        view: Option<common::model::ViewState>,
        loop_region: common::model::LoopRegion,
    ) {
        self.ui_prefs.piano_roll_views.clear();
        self.ui_prefs.audio_editor_views.clear();
        self.set_loop_region(loop_region);
        let Some(v) = view else { return };
        let max_choice = (crate::view::snap::SNAP_LABELS.len() as u8).saturating_sub(1);
        self.ui_prefs.arrange_zoom_x = v.arrange_zoom_x.clamp(2.0, 400.0);
        self.ui_prefs.arrange_scroll_beat = v.arrange_scroll_beat.max(0.0);
        self.ui_prefs.arrange_follow = v.arrange_follow;
        self.ui_prefs.arrange_track_top = v.arrange_track_top.max(0.0);
        self.ui_prefs.arrange_track_row_h = v.arrange_track_row_h.clamp(16.0, 2000.0);
        self.ui_prefs.arrange_header_w = v.arrange_header_w.clamp(80.0, 480.0);
        self.ui_prefs.track_row_overrides = v
            .track_row_overrides
            .into_iter()
            .map(|(k, h)| (k, h.max(16)))
            .collect();
        self.ui_prefs.expanded_automation_tracks = v.expanded_automation_tracks.into_iter().collect();
        self.ui_prefs.master_row_automation_expanded = v.master_row_automation_expanded;
        self.ui_prefs.arrange_snap_enabled = v.arrange_snap_enabled;
        self.ui_prefs.arrange_snap_choice = v.arrange_snap_choice.min(max_choice);
        self.ui_prefs.pianoroll_snap_enabled = v.pianoroll_snap_enabled;
        self.ui_prefs.pianoroll_snap_choice = v.pianoroll_snap_choice.min(max_choice);
        self.ui_prefs.piano_roll_fold = v.piano_roll_fold;
        self.ui_prefs.snap_on_draw = v.snap_on_draw;
        self.recording.snap_live_input = v.snap_live_input;
        self.ui_prefs.bottom_panel = v.bottom_panel;
        // 開いていたクリップ選択を復元 (現存しない stale key は除外)。 これでピアノロールが
        // 開き直し直後に「前回編集していたクリップ」をその per-clip view で表示する。
        self.selection.selected_clips = v
            .selected_clips
            .into_iter()
            .filter(|k| self.clip_ref_of(*k).is_some())
            .collect();
        self.selection.selected_clip = v.selected_clip.filter(|k| self.clip_ref_of(*k).is_some());
        for (k, mut pv) in v.piano_roll_views {
            pv.zoom_x = pv.zoom_x.clamp(8.0, 400.0);
            pv.zoom_y = pv.zoom_y.clamp(6.0, 40.0);
            pv.top_pitch = pv.top_pitch.clamp(11, 127);
            pv.scroll_beat = pv.scroll_beat.max(0.0);
            self.ui_prefs.piano_roll_views.insert(k, pv);
        }
        for (k, mut av) in v.audio_editor_views {
            av.start_beat = av.start_beat.max(0.0);
            av.len_beats = av.len_beats.max(0.0);
            self.ui_prefs.audio_editor_views.insert(k, av);
        }
    }

    /// 選択集合 (`selected_clips`) を現在の `ClipRef` 群へ解決 (解決でき
    /// ない stale key は除外)。 owned `Vec` を返す。
    pub fn selected_clip_refs(&self) -> Vec<ClipRef> {
        self.selection.selected_clips
            .iter()
            .filter_map(|k| self.clip_ref_of(*k))
            .collect()
    }

    /// ピアノロールに同時表示する MIDI クリップ群を順序付きで返す
    /// (`selected_clips` を `ClipRef` 解決 → MIDI のみ filter)。anchor (`selected_clip`) は
    /// `selected_clips` の末尾なので、末尾要素 = 新規ノートの所属先 (= 対象/target クリップ)。
    /// `selected_clips` が空の単一選択経路では `selected_clip` にフォールバックする。
    /// **この順序が packed note id の `clip_slot` の SSoT** (`decode_note_id` と必ず一致させる)。
    #[must_use]
    pub fn shown_pianoroll_clips(&self) -> Vec<ClipRef> {
        let mut out = Vec::new();
        if self.selection.selected_clips.is_empty() {
            if let Some(r) = self.selected_clip_ref()
                && self.is_midi_clip(r)
            {
                out.push(r);
            }
        } else {
            for k in &self.selection.selected_clips {
                if let Some(r) = self.clip_ref_of(*k)
                    && self.is_midi_clip(r)
                {
                    out.push(r);
                }
            }
        }
        out
    }

    /// 現在の対象 (target) クリップ = 新規ノートの所属先・凡例で強調される行。
    /// SSoT は選択 anchor (`selected_clip`)。anchor が表示 MIDI クリップ集合に含まれていれば
    /// それを、含まれなければ末尾 (= 旧挙動) を返す。表示 MIDI クリップが無いときは `None`。
    /// **target を切り替えても `shown_pianoroll_clips` の順序 (= packed id の clip_slot) は
    /// 変わらない** ので、target 変更で `selected_notes` を clear する必要はない (anchor の
    /// ポインタが動くだけ)。
    #[must_use]
    pub fn pianoroll_target_clip(&self) -> Option<ClipRef> {
        let shown = self.shown_pianoroll_clips();
        if let Some(a) = self.selected_clip_ref()
            && shown.contains(&a)
        {
            return Some(a);
        }
        shown.last().copied()
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
    pub(crate) fn remap_packed_selection_for_clip(&mut self, slot: usize, remap: &[Option<u32>]) {
        let mut out = Vec::with_capacity(self.selection.selected_notes.len());
        for &packed in &self.selection.selected_notes {
            if Self::note_id_clip_slot(packed) == slot {
                let local = Self::note_id_local_index(packed);
                if let Some(Some(new_local)) = remap.get(local) {
                    out.push(Self::pack_note_id(slot, *new_local as usize));
                }
            } else {
                out.push(packed);
            }
        }
        self.selection.selected_notes = out;
    }

    /// クリップ `slot` (= `r`) の notes に `f` を適用 (local index ベースで編集し、
    /// 重なり解決の勝者にする local index 群を返す) → `resolve_note_overlaps` で同ピッチ
    /// 重なりを解消 → packed `selected_notes` の当該クリップ部分を remap、という複数クリップ
    /// note 編集の共通基盤。snap 等の immutable 計算は呼び出し側で済ませて `f` に閉じ込める。
    pub(crate) fn edit_clip_notes(&mut self, slot: usize, r: ClipRef, f: impl FnOnce(&mut Vec<Note>) -> Vec<u32>) {
        let Some(Some(remap)) = self.edit_song(move |song| {
            let notes = song.notes_in_clip_mut(r.track as usize, r.clip as usize)?;
            let winners = f(notes);
            Some(resolve_note_overlaps(notes, &winners))
        }) else {
            return;
        };
        self.remap_packed_selection_for_clip(slot, &remap);
    }

    /// クリップの song-absolute 開始拍。clip-local note ⇄ song-absolute 変換の
    /// 唯一のオフセット (範囲外は 0)。
    #[must_use]
    pub fn clip_start_beat_of(&self, r: ClipRef) -> f64 {
        self.song_doc.song()
            .tracks
            .get(r.track as usize)
            .and_then(|t| t.clips.get(r.clip as usize))
            .map(|c| c.start_beat)
            .unwrap_or(0.0)
    }

    /// packed note id を持つ `entries` を **所属クリップ (clip_slot) ごと** に
    /// グルーピングし、各クリップで `per_clip(self, slot, ClipRef, &[(local_index, payload)])` を
    /// 呼ぶ。範囲外 slot / ロック中クリップは飛ばす (ロックは widget が hit 除外済だが二重防御)。
    /// payload は handler ごとに異なる (移動=(beat,pitch)、リサイズ=(beat,len)、velocity=u8、
    /// 削除/複製=`()` 等)。複数クリップ note 編集 handler の共通ディスパッチ。slot 昇順で適用
    /// するので、各クリップ内 index ベースの remove も安定する。
    pub(crate) fn for_each_note_clip_group<T>(
        &mut self,
        entries: impl IntoIterator<Item = (u32, T)>,
        mut per_clip: impl FnMut(&mut Self, usize, ClipRef, &[(usize, T)]),
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
            if self.is_pianoroll_clip_locked(r) {
                continue;
            }
            per_clip(self, slot, r, &items);
        }
    }

    /// packed note id を `(ClipRef, clip 内 index)` に分解。`shown` は
    /// `shown_pianoroll_clips()` の結果 (呼び出し側で 1 度作って使い回す)。`clip_slot` が
    /// 範囲外なら `None`。
    #[must_use]
    pub fn decode_note_id_in(shown: &[ClipRef], id: u32) -> Option<(ClipRef, usize)> {
        let clip_slot = (id >> 24) as usize;
        let note_index = (id & 0x00FF_FFFF) as usize;
        shown.get(clip_slot).copied().map(|r| (r, note_index))
    }

    /// 単発デコード (内部で `shown_pianoroll_clips()` を 1 度計算)。多数の id を
    /// 捌くハンドラでは `shown` を 1 度作って `decode_note_id_in` を使う (再計算を避ける)。
    #[must_use]
    pub fn decode_note_id(&self, id: u32) -> Option<(ClipRef, usize)> {
        Self::decode_note_id_in(&self.shown_pianoroll_clips(), id)
    }

    /// クリップ `r` 内の local note index 群を、現在の表示集合 (`shown_pianoroll_clips`)
    /// における **packed note id** に変換する。新規ノート (add / paste / step 入力) の結果選択を
    /// packed 化する共通基盤。`r` が表示集合に無ければ slot 0 (= 単一表示と byte 互換) に倒す。
    pub(crate) fn pack_clip_selection(&self, r: ClipRef, locals: &[u32]) -> Vec<u32> {
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

    /// そのクリップが乗っている **トラック** がピアノロールで「ロック (参照専用)」
    /// かどうか。ロックはトラック単位 (凡例がトラック単位なので)。
    pub fn is_pianoroll_clip_locked(&self, r: ClipRef) -> bool {
        self.song_doc.song()
            .tracks
            .get(r.track as usize)
            .is_some_and(|t| self.ui_prefs.locked_pr_tracks.contains(&t.id))
    }

    /// トラック id がピアノロールでロック中か (凡例のロックトグル状態表示用)。
    pub fn is_pianoroll_track_locked(&self, track_id: u32) -> bool {
        self.ui_prefs.locked_pr_tracks.contains(&track_id)
    }

    /// 凡例から対象 (target) クリップを切り替える。anchor (`selected_clip`) を
    /// `key` にするだけで、選択集合 (`selected_clips`) は変えない (= `shown_pianoroll_clips`
    /// の順序 = packed id slot 不変 → `selected_notes` 維持)。新規ノートの所属先・凡例強調が
    /// この clip になる。集合に居ない / 単一表示で anchor と異なる key は no-op。track も追従。
    pub(crate) fn set_pianoroll_target_clip(&mut self, key: common::model::ClipKey) {
        // 凡例は常に表示集合内のクリップしか出さないが、stale 入力に備えて検証する。
        let in_set = self.selection.selected_clips.contains(&key) || self.selection.selected_clip == Some(key);
        if !in_set {
            return;
        }
        self.selection.selected_clip = Some(key);
        if let Some(r) = self.clip_ref_of(key) {
            self.select_track(r.track);
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

    /// inspector の編集対象クリップ群。 複数選択 (`selected_clips`) 全体を
    /// 編集対象にする。 アンカー (`selected_clip`) は `select_clip` / `set_clip_selection`
    /// の構築上 `selected_clips` の末尾にいるので別途足す必要はない。 `selected_clips`
    /// が空 (= 単一選択経路のみ) のときだけ `selected_clip` にフォールバックする。
    /// inspector 編集対象クリップを **alloc せず** 順に渡す。 `selected_clips`
    /// 全体 (空なら `selected_clip` 単体) を走査する。 mixed 検出 (`inspector_fold`) は
    /// 毎フレーム全 field で呼ばれるので、 Vec を作らないこの基盤を使う。
    pub(crate) fn for_each_inspector_target(&self, mut f: impl FnMut(ClipRef)) {
        if self.selection.selected_clips.is_empty() {
            if let Some(r) = self.selected_clip_ref() {
                f(r);
            }
        } else {
            for k in &self.selection.selected_clips {
                if let Some(r) = self.clip_ref_of(*k) {
                    f(r);
                }
            }
        }
    }

    pub fn inspector_target_refs(&self) -> Vec<ClipRef> {
        let mut refs = Vec::new();
        self.for_each_inspector_target(|r| refs.push(r));
        refs
    }

    /// 編集対象クリップ各々に `extract` を適用し、 値が全て一致すれば
    /// `Some(値)`、 割れていれば `None` (= mixed) を返す。 `extract` が `None` を返す
    /// クリップ (= その field を持たない種別) は無視する。 表示中の section のアンカーは
    /// 必ずその種別なので、 表示中 field では `None` == mixed と解釈できる。 毎フレーム
    /// 全 field で呼ばれるので alloc しない (`for_each_inspector_target` を使う)。
    pub fn inspector_fold(&self, extract: impl Fn(&AppData, ClipRef) -> Option<f64>) -> Option<f64> {
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
        target: ClipRef,
        f: impl FnOnce(&common::model::ImageEvent) -> R,
    ) -> Option<R> {
        let content_id = self
            .song_doc.song()
            .tracks
            .get(target.track as usize)?
            .clips
            .get(target.clip as usize)?
            .content_id;
        match self.song_doc.song().clip_contents.get(&content_id)? {
            common::model::ClipContent::Image(img) => img.events.first().map(f),
            _ => None,
        }
    }

    /// `target` clip の first `TextEvent` に `f` を適用 (text clip でなければ `None`)。
    pub fn text_first_event<R>(
        &self,
        target: ClipRef,
        f: impl FnOnce(&common::model::TextEvent) -> R,
    ) -> Option<R> {
        let content_id = self
            .song_doc.song()
            .tracks
            .get(target.track as usize)?
            .clips
            .get(target.clip as usize)?
            .content_id;
        match self.song_doc.song().clip_contents.get(&content_id)? {
            common::model::ClipContent::Text(text) => text.events.first().map(f),
            _ => None,
        }
    }

    /// `target` clip の first `AudioEvent` に `f` を適用 (audio clip でなければ `None`)。
    pub fn audio_first_event<R>(
        &self,
        target: ClipRef,
        f: impl FnOnce(&common::model::AudioEvent) -> R,
    ) -> Option<R> {
        let content_id = self
            .song_doc.song()
            .tracks
            .get(target.track as usize)?
            .clips
            .get(target.clip as usize)?
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

    pub(crate) fn select_clip(&mut self, target: ClipRef, additive: bool) {
        let Some(key) = self.clip_key_of(target) else {
            return;
        };
        let mut keys = self.selection.selected_clips.clone();
        if additive {
            if let Some(pos) = keys.iter().position(|k| *k == key) {
                keys.remove(pos);
            } else {
                keys.push(key);
            }
        } else {
            keys = vec![key];
        }
        let primary = keys.last().copied();
        self.selection.selected_clips = keys;
        self.selection.selected_clip = primary;
        self.selection.selected_notes.clear();
        if primary.is_some() {
            self.selection.last_edit_select = Some(EditSurface::Clips);
        }
        self.recording.step_cursor_beat = 0.0;
        if let Some(r) = self.selected_clip_ref() {
            self.select_track(r.track);
        }
        // per-clip view を記憶するので、初めて開くクリップ (= entry 無し)
        // のときだけ auto-fit する。 既に記憶があれば draw が `piano_roll_views` を
        // 読んで前回の zoom/scroll を復元する (= 再選択で view が飛ばない)。 明示的な
        // 再 fit は `X` キー / Fit ボタン (`FitPianoRollToClip`)。
        if let Some(p) = primary
            && !self.ui_prefs.piano_roll_views.contains_key(&p)
        {
            self.fit_piano_roll_to_clip();
        }
    }

    pub(crate) fn set_clip_selection(&mut self, targets: Vec<ClipRef>) {
        let keys: Vec<common::model::ClipKey> =
            targets.iter().filter_map(|r| self.clip_key_of(*r)).collect();
        let primary = keys.last().copied();
        self.selection.selected_clips = keys;
        self.selection.selected_clip = primary;
        self.selection.selected_notes.clear();
        if primary.is_some() {
            self.selection.last_edit_select = Some(EditSurface::Clips);
        }
        self.recording.step_cursor_beat = 0.0;
        if let Some(r) = self.selected_clip_ref() {
            self.select_track(r.track);
        }
        // 初回 (entry 無し) のみ fit。 記憶があれば復元 (select_clip と同方針)。
        if let Some(p) = primary
            && !self.ui_prefs.piano_roll_views.contains_key(&p)
        {
            self.fit_piano_roll_to_clip();
        }
    }

    /// Ctrl+A (クリップ領域): 曲全体・全トラックの全クリップを選択。
    /// 全選択は一括操作なので `set_clip_selection` と違い view ジャンプ
    /// (fit_piano_roll_to_clip / select_track) を起こさない (= 表示を
    /// 飛ばさない、 grill-me 2026-06-09 決定)。 既に全選択なら冪等。
    /// anchor (末尾) は inspector 表示用に維持。 selection のみで非 undoable。
    pub(crate) fn select_all_clips(&mut self) {
        let all: Vec<common::model::ClipKey> = self
            .song_doc.song()
            .tracks
            .iter()
            .flat_map(|t| {
                t.clips
                    .iter()
                    .map(|c| common::model::ClipKey {
                        track_id: t.id,
                        clip_id: c.id,
                    })
            })
            .collect();
        if all.is_empty() {
            return;
        }
        // 冪等 early-return より前に last-wins 面だけは更新する (既に全選択でも
        // 「Ctrl+A = クリップ面を選んだ」 という意図は確定している)。
        self.selection.last_edit_select = Some(EditSurface::Clips);
        // 既に全選択なら冪等 (集合一致を順序非依存で判定)。
        if self.selection.selected_clips.len() == all.len() {
            let cur: std::collections::HashSet<common::model::ClipKey> =
                self.selection.selected_clips.iter().copied().collect();
            if all.iter().all(|k| cur.contains(k)) {
                return;
            }
        }
        self.selection.selected_clip = all.last().copied();
        self.selection.selected_clips = all;
        self.selection.selected_notes.clear();
    }

    /// 単一 clip (新規作成直後の `ClipRef`) を選択集合にする。 ClipRef→ClipKey
    /// 変換して anchor + set を更新 (view ジャンプ無し)。 create / duplicate の
    /// 結果選択用。
    pub(crate) fn set_single_clip_selection(&mut self, r: ClipRef) {
        let key = self.clip_key_of(r);
        self.selection.selected_clip = key;
        self.selection.selected_clips = key.into_iter().collect();
        if key.is_some() {
            self.selection.last_edit_select = Some(EditSurface::Clips);
        }
    }

    /// 新規 clip 群 (`ClipRef`) を選択集合にする (anchor = 末尾、 view ジャンプ
    /// 無し)。 ClipRef→ClipKey 変換。 clone / split / glue の結果選択用。
    pub(crate) fn select_new_clips(&mut self, refs: &[ClipRef]) {
        let keys: Vec<common::model::ClipKey> =
            refs.iter().filter_map(|r| self.clip_key_of(*r)).collect();
        self.selection.selected_clip = keys.last().copied();
        if self.selection.selected_clip.is_some() {
            self.selection.last_edit_select = Some(EditSurface::Clips);
        }
        self.selection.selected_clips = keys;
    }

    /// Ctrl+A (ピアノロール): **表示中の全 MIDI クリップ** の全ノートを packed note id で返す。
    /// 各 id = `pack_note_id(clip_slot, local_index)`。ロック中クリップは選択対象に
    /// しない (掴めないので除外)。表示クリップが無ければ空。
    pub fn all_shown_pianoroll_note_ids(&self) -> Vec<u32> {
        let shown = self.shown_pianoroll_clips();
        let mut out = Vec::new();
        for (slot, &r) in shown.iter().enumerate() {
            if self.is_pianoroll_clip_locked(r) {
                continue;
            }
            let Some(track) = self.song_doc.song().tracks.get(r.track as usize) else {
                continue;
            };
            let Some(clip) = track.clips.get(r.clip as usize) else {
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
        let Some(track) = self.song_doc.song().tracks.get(target.track as usize) else {
            return Vec::new();
        };
        let Some(clip) = track.clips.get(target.clip as usize) else {
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

    /// 右クリック「共有を一括選択」: `target` と同じ `content_id` を持つ
    /// main clip を全 track から集めて選択する (linked clip group)。
    /// `content_id` は payload 種別ごとに別空間なので automation clip 等と
    /// 混ざらない。 refcount==1 のときは自身 1 個の選択 (= 無害)。 clicked
    /// `target` を末尾 (= primary) に置いて piano_roll fit 対象を維持する。
    pub(crate) fn select_linked_clips(&mut self, target: ClipRef) {
        let Some(content_id) = self
            .song_doc.song()
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .map(|c| c.content_id)
        else {
            return;
        };
        let mut linked = Vec::new();
        for (t_idx, track) in self.song_doc.song().tracks.iter().enumerate() {
            for (c_idx, clip) in track.clips.iter().enumerate() {
                if clip.content_id == content_id {
                    linked.push(ClipRef {
                        track: t_idx as u32,
                        clip: c_idx as u32,
                    });
                }
            }
        }
        if linked.is_empty() {
            return;
        }
        if let Some(pos) = linked.iter().position(|r| *r == target) {
            let last = linked.len() - 1;
            linked.swap(pos, last);
        }
        let count = linked.len();
        self.set_clip_selection(linked);
        self.ui_ephemeral.status_message = if count <= 1 {
            "共有クリップはありません (この clip は単独)".to_string()
        } else {
            format!("共有クリップ {count} 個を選択しました")
        };
    }

    /// 現 selected_clip のノート bounding box が piano_roll grid 領域に
    /// 収まるよう zoom_x / zoom_y / scroll_beat / top_pitch を自動調整する。
    /// ノート無しの clip は clip 全長が見える初期 zoom にフォールバック。
    /// `last_pianoroll_grid_size` が未測定 (= 0) の場合は `pending_pianoroll_fit`
    /// を立てて return → piano_roll が初めて描画され grid_size が確定したフレームの
    /// Edit 内で再実行される (初回 fit 喪失バグの修正、 [`crate::widgets::piano_roll::piano_roll`] 参照)。
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
            let Some(track) = self.song_doc.song().tracks.get(r.track as usize) else {
                continue;
            };
            let Some(clip) = track.clips.get(r.clip as usize) else {
                continue;
            };
            let offset = if multi { clip.start_beat } else { 0.0 };
            union_start = union_start.min(offset);
            union_end = union_end.max(offset + clip.length_beats);
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
        } else if let Some(key) = self.selection.selected_clip {
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
    /// `scroll` は現在の左端拍、 `visible_beats` は可視拍数 (canvas_w / zoom)、
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

    /// 全 track の全 clip が arrangement canvas に収まるよう zoom_x / scroll_beat /
    /// track_row_h を自動調整する。clip 0 個なら song.length_beats でフォールバック。
    pub(crate) fn fit_arrange_to_content(&mut self) {
        // X (fit) は明示的な view 操作なので再生中は追従を解除する (= follow が
        // 次 tick で fit を上書きして戻すのを防ぐ)。
        self.cancel_follow_on_manual_view_change();
        let (canvas_w, canvas_h) = self.ui_ephemeral.last_arrange_canvas_size;
        if canvas_w < 16.0 || canvas_h < 16.0 {
            return;
        }
        // 行高は widget が canvas_h に描く行数で割る: 常に先頭へ prepend される
        // master 行 (arrangement_view.rs が常時 `Some(&master_row)`) + 可視 track 数。
        // collapsed group 配下の子は widget 側 `is_visible_track` で描画されないので、
        // ここでも親 chain に collapsed があれば除外して同じ可視集合を数える
        // (`compute_track_depth` と同じ parent_group_id walk、 32 hop で cycle 安全)。
        let visible_track_count = self
            .song_doc.song()
            .tracks
            .iter()
            .filter(|t| !self.is_hidden_under_collapsed_group(t.id))
            .count();
        // 展開中の automation lane も viewport を占める行なので、 行数に
        // 数え、 後で各 lane を同じ fit 行高へ scale する。 数えないと (旧挙動) lane
        // ぶんの高さが余計に積まれて content が溢れ、 かつ lane は model height_px の
        // ままなので「track だけ縮んで automation レーンが高いまま」 になる (ユーザー報告)。
        // master 行 (`MASTER_TRACK_ID`) の song_lanes も同様。 collapsed track / collapsed
        // automation / 非 visible lane は widget が描かないので除外。
        let mut fit_lane_keys: Vec<common::model::AutomationLaneKey> = Vec::new();
        for t in &self.song_doc.song().tracks {
            if self.is_hidden_under_collapsed_group(t.id)
                || !self.ui_prefs.expanded_automation_tracks.contains(&t.id)
            {
                continue;
            }
            for l in t.automation_lanes.iter().filter(|l| l.visible) {
                fit_lane_keys.push(common::model::AutomationLaneKey { track: t.id, lane: l.id });
            }
        }
        if self.ui_prefs.master_row_automation_expanded {
            for l in self.song_doc.song().song_lanes.iter().filter(|l| l.visible) {
                fit_lane_keys.push(common::model::AutomationLaneKey {
                    track: common::model::MASTER_TRACK_ID,
                    lane: l.id,
                });
            }
        }
        // +1 は master 行 (widget が visible_tracks[0] に常時 prepend する)。
        let row_count = (visible_track_count + fit_lane_keys.len() + 1).max(1);

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
        self.ui_prefs.arrange_zoom_x = (f64::from(canvas_w) / span_beats).clamp(2.0, 400.0) as f32;
        let row_h = (canvas_h / row_count as f32).clamp(16.0, 96.0);
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
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let lane_px = (row_h.round() as u16).max(1);
        for k in fit_lane_keys {
            self.ui_prefs.automation_lane_row_overrides.insert(k, lane_px);
        }
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
        let (canvas_w, _) = self.ui_ephemeral.last_arrange_canvas_size;
        if canvas_w < 16.0 {
            return false;
        }
        // fresh な横ズームは新しい zoom セッションの起点。 前セッションの lane 拡大
        // (一時 override) を破棄してから snapshot を撮る — snapshot に古い拡大を
        // 持ち越さないことで、 後で `X` / fit したとき automation レーンだけ高いまま
        // 残るのを防ぐ。 override は lane-fill 中だけ存在する一時状態。
        self.ui_prefs.automation_lane_row_overrides.clear();
        let snap = self.capture_arrange_view();
        self.ui_ephemeral.arrange_zoom_history.push(snap);
        let span = max_end - min_start;
        // clip が canvas 幅の ~92% を占めるよう左右に proportional padding
        // (短い clip でも極端に拡大しすぎないよう最小 0.5 beat)。
        let pad = (span * 0.04).max(0.5);
        self.ui_prefs.arrange_scroll_beat = (min_start - pad).max(0.0) as f32;
        self.ui_prefs.arrange_zoom_x =
            (f64::from(canvas_w) / (span + pad * 2.0)).clamp(2.0, 400.0) as f32;
        true
    }

    /// `Z` 2 段目: 縦ズーム。 automation clip 選択時は primary レーンを viewport
    /// いっぱいに拡大 (lane height override) + 上端へ scroll、 それ以外は選択 track
    /// 群を viewport に収める。 適用したら `true`、 lanes 過小 / 対象解決不能なら
    /// `false` (view 不変)。
    pub(crate) fn zoom_arrange_vertical(&mut self, automation: bool) -> bool {
        let lanes_h = self.ui_ephemeral.last_arrange_canvas_size.1;
        if lanes_h < 16.0 {
            return false;
        }
        // 対象面が automation clip なら、 そのレーンを viewport 高いっぱいに拡大する
        // (= MIDI track の縦ズームの「レーン版」)。 primary レーンの実 content-Y は
        // view が widget の実 rect から算出済 (`arrange_primary_lane_content_top`)。
        if automation
            && let Some((lane_key, content_top)) = self.ui_ephemeral.arrange_primary_lane_content_top
            && self
                .selection.selected_automation_clips
                .iter()
                .any(|k| k.lane_key() == lane_key)
        {
            let snap = self.capture_arrange_view();
            self.ui_ephemeral.arrange_zoom_history.push(snap);
            // レーン高 = viewport 高 (u16 へ saturating)。 レーンより上の行高は
            // 変わらないので content_top はそのままレーン上端の絶対 y。
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let lane_px = lanes_h.clamp(16.0, f32::from(u16::MAX)) as u16;
            self.ui_prefs.automation_lane_row_overrides.insert(lane_key, lane_px);
            self.ui_prefs.arrange_track_top = content_top.max(0.0);
            return true;
        }
        // 通常 clip 選択: 選択 track 群が viewport いっぱいになるよう uniform 行高 +
        // 先頭行へ scroll (master 行を行 0 とする統一行 index 空間)。
        let Some((r_min, r_max)) = self.selected_tracks_visible_row_span(automation) else {
            return false;
        };
        let snap = self.capture_arrange_view();
        self.ui_ephemeral.arrange_zoom_history.push(snap);
        let rows = (r_max - r_min + 1) as f32;
        let row_h = (lanes_h / rows).clamp(16.0, 2000.0);
        self.ui_prefs.track_row_overrides.clear();
        self.ui_prefs.arrange_track_row_h = row_h;
        self.ui_prefs.arrange_track_top = (r_min as f32) * row_h;
        true
    }

    /// `Z` 段階ズームの選択シグネチャ (通常 clip 群 / primary clip / automation clip 群
    /// / 対象面)。 これが変わると別対象とみなして段階 0 (横ズーム) から仕切り直す。
    pub(crate) fn current_zoom_selection_sig(&self, automation: bool) -> ZoomSelectionSig {
        ZoomSelectionSig {
            clips: self.selection.selected_clips.clone(),
            clip: self.selection.selected_clip,
            automation: self.selection.selected_automation_clips.clone(),
            target_automation: automation,
        }
    }

    /// 現在の arrangement view が `snap` と一致するか (= 直近 Z 以降ユーザーが手動で
    /// ズーム / スクロール / 行高変更をしていないか)。 段階ズームの仕切り直し判定に使う。
    pub(crate) fn arrange_view_matches(&self, snap: &ArrangeViewSnapshot) -> bool {
        self.capture_arrange_view() == *snap
    }

    /// 対象面 (`automation`) の選択素材が乗っている track 群の、 arrangement の可視行
    /// 並びにおける行 index 範囲 `(min, max)`。 行 0 は常時先頭に描かれる master 行、
    /// 行 `n` (n>=1) は可視 track の `n-1` 番目 (collapsed group 配下は除外 = widget の
    /// `is_visible_track` と一致)。 master 行に乗る automation clip
    /// (`AutomationClipKey.track == MASTER_TRACK_ID`) は行 0 に対応する。 選択無し /
    /// どれも不可視なら `None`。 縦ズーム (automation はレーン拡大不能時の fallback、
    /// clip は track 群を収める) の「収める行範囲」 算出に使う。
    pub(crate) fn selected_tracks_visible_row_span(&self, automation: bool) -> Option<(usize, usize)> {
        // 対象 track id を対象面の選択から集める。 master 行 automation clip は
        // song.tracks に無いので別フラグで持つ。
        let mut track_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let mut include_master = false;
        if automation {
            for k in &self.selection.selected_automation_clips {
                if k.track == common::model::MASTER_TRACK_ID {
                    include_master = true;
                } else {
                    track_ids.insert(k.track);
                }
            }
        } else {
            track_ids.extend(self.selection.selected_clips.iter().map(|k| k.track_id));
            if track_ids.is_empty()
                && let Some(k) = self.selection.selected_clip
            {
                track_ids.insert(k.track_id);
            }
        }
        if track_ids.is_empty() && !include_master {
            return None;
        }
        let (mut r_min, mut r_max) = (usize::MAX, 0usize);
        if include_master {
            r_min = 0;
            r_max = 0;
        }
        // 行 0 = master、 可視 track の i 番目は行 i+1。
        let mut vi = 1usize;
        for t in &self.song_doc.song().tracks {
            if self.is_hidden_under_collapsed_group(t.id) {
                continue;
            }
            if track_ids.contains(&t.id) {
                r_min = r_min.min(vi);
                r_max = r_max.max(vi);
            }
            vi += 1;
        }
        (r_min != usize::MAX).then_some((r_min, r_max))
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
            // 通常 clip: selected_clips 優先、 空なら primary selected_clip 単独。
            let mut clip_keys = self.selection.selected_clips.clone();
            if clip_keys.is_empty() {
                clip_keys.extend(self.selection.selected_clip);
            }
            for key in clip_keys {
                if let Some(clip) = self.clip_at(key) {
                    min_start = min_start.min(clip.start_beat);
                    max_end = max_end.max(clip.start_beat + clip.length_beats);
                }
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
        } else {
            self.fit_arrange_to_content();
        }
        // 1 段戻したら段階ズームのアンカーは無効 (次の Z は仕切り直し)。
        self.ui_ephemeral.arrange_zoom_anchor = None;
    }

}
