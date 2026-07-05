//! handler::notes — note 編集 + scale + clip voice/talk param + lyric + note clipboard
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use crate::event::*;
use common::model::Note;

impl AppData {
    /// 選択中ノートを clipboard envelope (`ClipboardPayload::Notes`) JSON に。
    /// 何も copy できない (選択無し / クリップ未選択 / シリアライズ失敗) 場合は `None`。
    /// 戻り値は `(json, note_count)`。status_message は `&self` を保つため呼び出し側で書く。
    /// 時間は選択群の最早 start を 0 とした相対に正規化する (paste でマウス拍に置く)。
    pub fn copy_notes_clip(&self) -> Option<(String, usize)> {
        let r = self.selected_clip_ref()?;
        if self.selection.selected_notes.is_empty() {
            return None;
        }
        let track = self.song_doc.song().tracks.get(r.track as usize)?;
        let clip = track.clips.get(r.clip as usize)?;
        let notes = self.song_doc.song().clip_notes(clip);
        let mut copied: Vec<Note> = self
            .selection.selected_notes
            .iter()
            .filter_map(|i| notes.get(*i as usize).cloned())
            .collect();
        if copied.is_empty() {
            return None;
        }
        let earliest = copied
            .iter()
            .map(|n| n.start_beat)
            .fold(f64::INFINITY, f64::min);
        if earliest.is_finite() {
            for n in &mut copied {
                n.start_beat -= earliest;
            }
        }
        let count = copied.len();
        let json = crate::clipboard::ClipboardEnvelope::new(
            self.song_doc.song().project_id,
            crate::clipboard::ClipboardPayload::Notes(copied),
        )
        .to_json()?;
        Some((json, count))
    }

    /// ノート群を「編集中クリップ (`selected_clip`)」の `at_beat`
    /// (clip-local 拍) に貼る。`notes` は最早=0 正規化済み相対 → 各 `start_beat += at_beat`。
    /// 値域は呼び出し側で sanitize 済み。貼った note 群を新選択にする。戻り値は挿入数。
    pub fn paste_notes_at(&mut self, mut notes: Vec<Note>, at_beat: f64) -> usize {
        if notes.is_empty() {
            return 0;
        }
        let Some(r) = self.selected_clip_ref() else {
            self.ui_ephemeral.status_message = "貼り付け先のクリップが選択されていません".to_string();
            return 0;
        };
        let anchor = at_beat.max(0.0);
        let count = notes.len();
        // 貼り付け先 clip が実在しなければ (edit 前に判定して) spurious な
        // undo snapshot を積まない。
        let Some(local_sel) = self.edit_song(|song| {
            let dest = midi_content_in_clip_mut(song, r.track as usize, r.clip as usize)?;
            let mut new_indices = Vec::with_capacity(notes.len());
            for src in &mut notes {
                src.start_beat += anchor;
                // clipboard の Note は元 content の id を持つ。 貼り付け先 content で
                // per-content 一意 id 不変条件 (invariant #1、 piano_roll が note.id で
                // addressing) を守るため再採番する (M4 sibling)。
                src.id = dest.alloc_note_id();
                new_indices.push(dest.notes.len() as u32);
                dest.notes.push(src.clone());
            }
            // 貼り付けた note を勝者として重なり解消。選択は remap で追従。
            let remap = resolve_note_overlaps(&mut dest.notes, &new_indices);
            Some(remap_indices(&remap, &new_indices))
        }).flatten() else {
            return 0;
        };
        // selected_notes は packed note id。貼り付け先 (anchor) clip の slot で pack。
        self.selection.selected_notes = self.pack_clip_selection(r, &local_sel);
        count
    }

    pub(crate) fn set_note_velocity(&mut self, note_idx: u32, velocity: u8) {
        let Some(r) = self.selected_clip_ref() else {
            return;
        };
        let _ = self.edit_song(|song| {
            let Some(notes) = song.notes_in_clip_mut(r.track as usize, r.clip as usize) else {
                return false;
            };
            let Some(note) = notes.get_mut(note_idx as usize) else {
                return false;
            };
            note.velocity = velocity;
            true
        });
    }

    /// gui_01 #018 (M14 Phase 64): velocity lane drag の release frame で
    /// 1 batch 発行される `(note_id, new_velocity)` 列を一括適用。 widget
    /// から渡される id は piano_roll widget 上の `NoteId` (= clip 内 note
    /// index に同じ値域、 daw_01 でも u32)。 1 batch を 1 Undo step とする
    /// ため、 push_undo_snapshot は handle_event の auto push 経路に任せる
    /// (`is_undoable` で `SetNoteVelocities` を許可)。 host への LoadSong は
    /// edit_song の epoch bump を runner の frame flush が 1 frame 1 回で送る。
    pub(crate) fn set_note_velocities(&mut self, updates: &[(u32, u8)]) {
        // packed id を所属クリップごとに分配し velocity を書く。複数クリップに
        // 跨る選択の velocity lane drag をまとめて反映 (velocity 変更は重なりを生まない)。
        self.for_each_note_clip_group(
            updates.iter().map(|&(id, vel)| (id, vel)),
            |app, _slot, r, items| {
                app.edit_song(|song| {
                    if let Some(notes) =
                        song.notes_in_clip_mut(r.track as usize, r.clip as usize)
                    {
                        for &(local, vel) in items {
                            if let Some(note) = notes.get_mut(local) {
                                note.velocity = vel;
                            }
                        }
                    }
                });
            },
        );
    }

    pub(crate) fn quantize_selected_notes(&mut self, div: u8) {
        // 選択 (packed) を所属クリップごとに分配し、各クリップ内 clip-local start を
        // 量子化。重なりは edit_clip_notes が解消し選択 (packed) を remap で追従。
        if self.selection.selected_notes.is_empty() {
            return;
        }
        let div = div.max(1) as f64;
        let selected = self.selection.selected_notes.clone();
        self.for_each_note_clip_group(
            selected.into_iter().map(|id| (id, ())),
            |app, slot, r, items| {
                let locals: Vec<usize> = items.iter().map(|&(local, ())| local).collect();
                app.edit_clip_notes(slot, r, move |notes| {
                    let mut winners = Vec::with_capacity(locals.len());
                    for local in locals {
                        if let Some(n) = notes.get_mut(local) {
                            n.start_beat = ((n.start_beat * div).round() / div).max(0.0);
                            winners.push(local as u32);
                        }
                    }
                    winners
                });
            },
        );
    }

    pub(crate) fn resize_track_peak_display(&mut self) {
        let n = self.song_doc.song().tracks.len();
        self.transport.track_peak_display.resize(n, (0.0, 0.0));
    }

    // -------- Note operations ----------------------------------------------

    pub(crate) fn select_note(&mut self, note: u32, additive: bool) {
        if !additive {
            self.selection.selected_notes.clear();
        }
        if !self.selection.selected_notes.contains(&note) {
            self.selection.selected_notes.push(note);
        }
    }

    // -------- Phase 7 B5 (`docs/plan_scale.html`): Scale operations -------

    /// Transport bar の root / scale dropdown commit handler。
    /// `scale_changes` が空なら beat=0 で新規追加、 そうでなければ
    /// `scale_at(playhead)` で見つかる event を update。 plan §4.1 と一致。
    pub(crate) fn set_scale_at_playhead(&mut self, root: u8, scale: common::scale::Scale) {
        let playhead = self
            .transport.playhead_beat
            .map(f64::from)
            .unwrap_or(0.0)
            .max(0.0);
        let root = root.min(11);
        if self.song_doc.song().scale_changes.is_empty() {
            self.edit_song(move |song| {
                song.scale_changes.push(common::scale::ScaleChange {
                    beat: 0.0,
                    root,
                    scale,
                });
            });
            return;
        }
        // `scale_at` の semantics に合わせて「playhead 以下の最新 event」
        // を update。 playhead 未満の event が無ければ最初の event を update
        // (Cubase Transport の Chord Track edit と同じ idiom)。
        let target_idx = self
            .song_doc.song()
            .scale_changes
            .iter()
            .rposition(|c| c.beat <= playhead)
            .unwrap_or(0);
        self.edit_song(|song| {
            if let Some(ev) = song.scale_changes.get_mut(target_idx) {
                ev.root = root;
                ev.scale = scale;
            }
        });
    }

    /// `selected_clip` の note の pitch を最寄り in-scale に一括補正。
    /// 各 note の start_beat 時点の scale を尊重 (転調をまたぐ note は
    /// それぞれの local scale で snap される)。
    pub(crate) fn quantize_pitches_to_scale(&mut self, target: QuantizePitchTarget) {
        let Some(r) = self.selected_clip_ref() else {
            return;
        };
        if self.song_doc.song().scale_changes.is_empty() {
            self.ui_ephemeral.status_message =
                "Scale が設定されていません (Transport bar の Key dropdown で設定)".to_string();
            return;
        }
        let Some(track) = self.song_doc.song().tracks.get(r.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get(r.clip as usize) else {
            return;
        };
        let clip_start_beat = clip.start_beat;
        // immutable borrow で snap 計算を済ませてから可変借用に切り替える
        // (= borrow checker 衝突回避)。 `Song::clip_notes` は `Clip` を経由する
        // shared note 取得 helper、 mutable 版は `notes_in_clip_mut`。
        let snaps: Vec<(u32, u8)> = {
            let notes = self.song_doc.song().clip_notes(clip);
            let target_indices: Vec<u32> = match target {
                QuantizePitchTarget::SelectedNotes => self.selection.selected_notes.clone(),
                QuantizePitchTarget::SelectedClipAllNotes => {
                    (0..notes.len() as u32).collect()
                }
            };
            target_indices
                .iter()
                .filter_map(|&i| {
                    let n = notes.get(i as usize)?;
                    let global_beat = clip_start_beat + n.start_beat;
                    let new_pitch = self
                        .song_doc.song()
                        .scale_at(global_beat)
                        .map(|sc| sc.snap(n.pitch))
                        .unwrap_or(n.pitch);
                    if new_pitch != n.pitch {
                        Some((i, new_pitch))
                    } else {
                        None
                    }
                })
                .collect()
        };
        let count = snaps.len();
        if count == 0 {
            self.ui_ephemeral.status_message =
                "対象 note は既に in-scale です".to_string();
            return;
        }
        // pitch 補正で異なる pitch が同一 pitch に丸まると同一ピッチの
        // 重なりが生じ得るので、補正した note を勝者として重なり解消する。
        let winners: Vec<u32> = snaps.iter().map(|&(i, _)| i).collect();
        let remap = self
            .edit_song(move |song| {
                let notes = song.notes_in_clip_mut(r.track as usize, r.clip as usize)?;
                for (i, new_pitch) in snaps {
                    if let Some(n) = notes.get_mut(i as usize) {
                        n.pitch = new_pitch;
                    }
                }
                Some(resolve_note_overlaps(notes, &winners))
            })
            .flatten();
        if let Some(remap) = remap {
            let sel = std::mem::take(&mut self.selection.selected_notes);
            self.selection.selected_notes = remap_indices(&remap, &sel);
        }
        self.ui_ephemeral.status_message =
            format!("{count} 件の note を scale に補正しました");
    }

    pub(crate) fn add_note(
        &mut self,
        track_idx: u32,
        clip_idx: u32,
        start_beat: f64,
        duration: f64,
        pitch: u8,
    ) {
        let start_beat = start_beat.max(0.0);
        let duration = duration.max(0.0625);
        // Phase 7 B5 (`docs/plan_scale.html` §5.1): Snap on Draw。
        // scale_changes が空なら scale_at が None → unwrap_or で raw pitch
        // 維持 = 機能 OFF と同じ挙動。
        let pitch = if self.ui_prefs.snap_on_draw {
            let clip_start_beat = self
                .song_doc.song()
                .tracks
                .get(track_idx as usize)
                .and_then(|t| t.clips.get(clip_idx as usize))
                .map(|c| c.start_beat)
                .unwrap_or(0.0);
            let global_beat = clip_start_beat + start_beat;
            self.song_doc.song()
                .scale_at(global_beat)
                .map(|sc| sc.snap(pitch))
                .unwrap_or(pitch)
        } else {
            pitch
        };
        let Some(Some(selected)) = self.edit_song(|song| {
            let content =
                midi_content_in_clip_mut(song, track_idx as usize, clip_idx as usize)?;
            // v29: 新規 note は allocator で安定 id を採番する。
            let note_id = content.alloc_note_id();
            let notes = &mut content.notes;
            let new_idx = notes.len() as u32;
            notes.push(Note {
                id: note_id,
                start_beat,
                duration_beats: duration,
                pitch,
                velocity: 100,
                lyric: None,
                muted: false,
            });
            // 追加した note を勝者として同一ピッチの重なりを解消。
            let remap = resolve_note_overlaps(notes, &[new_idx]);
            Some(remap_indices(&remap, &[new_idx]))
        }) else {
            return;
        };
        let r = ClipRef {
            track: track_idx,
            clip: clip_idx,
        };
        // 新規ノートは「対象クリップ」(= anchor) へ入る。anchor をこのクリップに
        // 揃えるが、複数選択 (selected_clips) は **縮小しない** (複数同時表示を保持)。対象が
        // まだ選択集合に無ければ追加する (他クリップは残す)。
        if let Some(key) = self.clip_key_of(r) {
            self.selection.selected_clip = Some(key);
            if !self.selection.selected_clips.contains(&key) {
                self.selection.selected_clips.push(key);
            }
        }
        // 選択は packed note id。対象クリップの clip_slot (= shown 内位置) で pack。
        self.selection.selected_notes = self.pack_clip_selection(r, &selected);
        self.ui_prefs.last_note_duration_beats = duration;
    }

    pub(crate) fn set_note_positions(&mut self, entries: &[(u32, f64, u8)]) {
        // entries の `u32` は packed note id (clip_slot|local index)。所属クリップ
        // ごとに分配し、各クリップ内 local index で位置を書き換える。`beat` は view が既に
        // 各 note の所属クリップ clip-local に戻している (per-note offset)。
        let snap_on_draw = self.ui_prefs.snap_on_draw;
        self.for_each_note_clip_group(
            entries.iter().map(|&(id, beat, pitch)| (id, (beat, pitch))),
            |app, slot, r, items| {
                // Phase 7 B5 (`docs/plan_scale.html` §5.1): Snap on Draw を note 移動
                // (y-drag で pitch 変更) にも適用。snap 計算は immutable phase で済ませる。
                // Fold mode のときは widget が既に in-scale pitch を push しているので
                // idempotent。各 note の所属クリップ (`r`) の start_beat で song-absolute 化する。
                let clip_start = app.clip_start_beat_of(r);
                let snapped: Vec<(usize, f64, u8)> = items
                    .iter()
                    .map(|&(local, (beat, pitch))| {
                        let new_pitch = if snap_on_draw {
                            app.song_doc.song()
                                .scale_at(clip_start + beat.max(0.0))
                                .map(|sc| sc.snap(pitch))
                                .unwrap_or(pitch)
                        } else {
                            pitch
                        };
                        (local, beat, new_pitch)
                    })
                    .collect();
                // 移動した note を勝者として重なり解消。既存選択 (packed) を
                // edit_clip_notes が当該クリップ分だけ remap で追従させる。
                app.edit_clip_notes(slot, r, move |notes| {
                    let mut winners = Vec::with_capacity(snapped.len());
                    for (local, beat, pitch) in snapped {
                        if let Some(note) = notes.get_mut(local) {
                            note.start_beat = beat.max(0.0);
                            note.pitch = pitch;
                            winners.push(local as u32);
                        }
                    }
                    winners
                });
            },
        );
    }

    pub(crate) fn resize_notes(&mut self, entries: &[(u32, f64, f64)]) {
        // packed id を所属クリップごとに分配してリサイズ。`start` は view が
        // 各 note の所属クリップ clip-local に戻している。
        self.for_each_note_clip_group(
            entries.iter().map(|&(id, start, dur)| (id, (start, dur))),
            |app, slot, r, items| {
                let updates: Vec<(usize, f64, f64)> =
                    items.iter().map(|&(local, (s, d))| (local, s, d)).collect();
                // リサイズした note を勝者として重なり解消。既存選択 (packed) は
                // edit_clip_notes が remap で追従。
                app.edit_clip_notes(slot, r, move |notes| {
                    let mut winners = Vec::with_capacity(updates.len());
                    for (local, start, duration) in updates {
                        if let Some(note) = notes.get_mut(local) {
                            note.start_beat = start.max(0.0);
                            note.duration_beats = duration.max(0.0625);
                            winners.push(local as u32);
                        }
                    }
                    winners
                });
            },
        );
        if let Some(&(_, _, duration)) = entries.last() {
            self.ui_prefs.last_note_duration_beats = duration.max(0.0625);
        }
    }

    /// ピアノロールで選択中ノート (`selected_notes`) を複製する (D キー)。
    /// 複製は選択範囲の beat span ぶん後ろにずらし、元ノートは据え置き、
    /// 複製を新しい選択にする (連打で後方へ連鎖)。selected_clip 無し /
    /// 選択空 / clip 解決失敗なら no-op。
    pub(crate) fn duplicate_selected_notes(&mut self) {
        // 選択 (packed) を所属クリップごとに複製。各クリップ内でその選択分の
        // beat span ぶん後ろへずらす (元は据え置き)。新しい選択 = 全クリップの複製 (packed)。
        if self.selection.selected_notes.is_empty() {
            return;
        }
        let selected: Vec<u32> = self.selection.selected_notes.clone();
        let mut new_selection: Vec<u32> = Vec::new();
        self.for_each_note_clip_group(
            selected.into_iter().map(|id| (id, ())),
            |app, slot, r, items| {
                let locals: Vec<u32> = items.iter().map(|&(local, ())| local as u32).collect();
                let packed = app.edit_song(move |song| {
                    let Some(content) =
                        midi_content_in_clip_mut(song, r.track as usize, r.clip as usize)
                    else {
                        return Vec::new();
                    };
                    let new_ids = duplicate_notes_into(content, &locals);
                    if new_ids.is_empty() {
                        return Vec::new();
                    }
                    // 複製を勝者として重なり解消 (元と密接な複製は元を据え置く)。
                    let remap = resolve_note_overlaps(&mut content.notes, &new_ids);
                    remap_indices(&remap, &new_ids)
                        .into_iter()
                        .map(|nid| Self::pack_note_id(slot, nid as usize))
                        .collect::<Vec<u32>>()
                });
                if let Some(packed) = packed {
                    new_selection.extend(packed);
                }
            },
        );
        if !new_selection.is_empty() {
            self.selection.selected_notes = new_selection;
        }
    }

    /// gui_01 #054 (Ctrl+drag コピー): `entries` = [(source note index,
    /// new_start_beat, new_pitch)]。各 source を deep clone して指定位置へ配置し
    /// (元は据え置き)、複製を新選択にする。selected_clip 無し / 該当 index 無しなら no-op。
    pub(crate) fn copy_notes(&mut self, entries: &[(u32, f64, u8)]) {
        // packed id を所属クリップごとに分配し、各 source を同じクリップ内へ複製。
        // 新しい選択 = 全クリップの複製 (packed で再構成)。`beat` は view が clip-local 化済。
        let mut new_selection: Vec<u32> = Vec::new();
        self.for_each_note_clip_group(
            entries.iter().map(|&(id, beat, pitch)| (id, (beat, pitch))),
            |app, slot, r, items| {
                let local_entries: Vec<(u32, f64, u8)> = items
                    .iter()
                    .map(|&(local, (beat, pitch))| (local as u32, beat, pitch))
                    .collect();
                let packed = app.edit_song(move |song| {
                    let Some(content) =
                        midi_content_in_clip_mut(song, r.track as usize, r.clip as usize)
                    else {
                        return Vec::new();
                    };
                    let new_ids = copy_notes_into(content, &local_entries);
                    if new_ids.is_empty() {
                        return Vec::new();
                    }
                    // コピーを勝者として重なり解消。複製の local id を packed 化。
                    let remap = resolve_note_overlaps(&mut content.notes, &new_ids);
                    remap_indices(&remap, &new_ids)
                        .into_iter()
                        .map(|nid| Self::pack_note_id(slot, nid as usize))
                        .collect::<Vec<u32>>()
                });
                if let Some(packed) = packed {
                    new_selection.extend(packed);
                }
            },
        );
        if !new_selection.is_empty() {
            self.selection.selected_notes = new_selection;
        }
    }

    pub(crate) fn resize_note(
        &mut self,
        track_idx: u32,
        clip_idx: u32,
        note_idx: u32,
        new_duration: f64,
    ) {
        let new_duration = new_duration.max(0.0625);
        let remap = self
            .edit_song(|song| {
                let notes = song.notes_in_clip_mut(track_idx as usize, clip_idx as usize)?;
                let note = notes.get_mut(note_idx as usize)?;
                note.duration_beats = new_duration;
                // リサイズした note を勝者として重なり解消。既存選択は remap で追従。
                Some(resolve_note_overlaps(notes, &[note_idx]))
            })
            .flatten();
        let Some(remap) = remap else {
            return;
        };
        let sel = std::mem::take(&mut self.selection.selected_notes);
        self.selection.selected_notes = remap_indices(&remap, &sel);
    }

    pub(crate) fn delete_selected_notes(&mut self) {
        // 選択 (packed) を所属クリップごとに分配し、各クリップ内で index 降順に
        // remove (削除で後続 index がずれない)。複数クリップに跨る選択をまとめて消す。
        if self.selection.selected_notes.is_empty() {
            return;
        }
        let ids = std::mem::take(&mut self.selection.selected_notes);
        self.for_each_note_clip_group(
            ids.into_iter().map(|id| (id, ())),
            |app, _slot, r, items| {
                let mut locals: Vec<usize> = items.iter().map(|&(local, ())| local).collect();
                locals.sort_unstable_by(|a, b| b.cmp(a));
                app.edit_song(move |song| {
                    if let Some(notes) =
                        song.notes_in_clip_mut(r.track as usize, r.clip as usize)
                    {
                        for i in locals {
                            if i < notes.len() {
                                notes.remove(i);
                            }
                        }
                    }
                });
            },
        );
    }

    /// per-clip 声を設定。 Clip Inspector の 2 段 dropdown から
    /// `SetClipVoice` 経由で呼ばれる。 stable `ClipKey` で対象 clip を引き、
    /// 声 3 値を焼き込んで builtin へ再 flush (= 新しい声で再合成)。
    pub(crate) fn set_clip_voice(
        &mut self,
        key: common::model::ClipKey,
        speaker_id: u32,
        singer_name: String,
        style_name: String,
    ) {
        let Some(r) = self.clip_ref_of(key) else {
            return;
        };
        let changed = self.edit_song_checked(move |song| {
            let Some(clip) = song
                .tracks
                .get_mut(r.track as usize)
                .and_then(|t| t.clips.get_mut(r.clip as usize))
            else {
                return false;
            };
            if clip.speaker_id == speaker_id
                && clip.singer_name == singer_name
                && clip.style_name == style_name
            {
                return false;
            }
            clip.speaker_id = speaker_id;
            clip.singer_name = singer_name;
            clip.style_name = style_name;
            true
        });
        if changed {
            // 声変更を builtin に反映 (= clip 単位で再合成)。
            self.sync_vocal_metadata();
            // (talk) talk 声変更は phoneme (= 口パク) も変える (speaker で prosody が変わる)。
            // sing 声変更は phoneme 不変 (QUERY_SPEAKER 固定) なので no-op に近いが無害。
            self.mark_lipsync_dirty();
        }
    }

    /// (talk) `SetClipTalkParam` 経由。Text clip の読み上げスケール 1 項目を
    /// `Clip::talk` に焼き込み、builtin へ再 flush (= 新スケールで再合成)。全項目が
    /// 既定なら `None` に畳む (serialize しない)。値が変わらないなら no-op。
    pub(crate) fn set_clip_talk_param(
        &mut self,
        key: common::model::ClipKey,
        param: TalkParamKind,
        value: f32,
    ) {
        let Some(r) = self.clip_ref_of(key) else {
            return;
        };
        let changed = self.edit_song_checked(|song| {
            let Some(clip) = song
                .tracks
                .get_mut(r.track as usize)
                .and_then(|t| t.clips.get_mut(r.clip as usize))
            else {
                return false;
            };
            let mut talk = clip.talk.unwrap_or_default();
            // VOICEVOX `audio_query` の受理範囲にクランプ (範囲外は 422 を返す)。
            match param {
                TalkParamKind::Speed => talk.speed_scale = value.clamp(0.5, 2.0),
                TalkParamKind::Pitch => talk.pitch_scale = value.clamp(-0.15, 0.15),
                TalkParamKind::Intonation => talk.intonation_scale = value.clamp(0.0, 2.0),
                TalkParamKind::Volume => talk.volume_scale = value.clamp(0.0, 2.0),
            }
            let new_talk = if talk == common::model::TalkParams::default() {
                None
            } else {
                Some(talk)
            };
            if clip.talk == new_talk {
                return false;
            }
            clip.talk = new_talk;
            true
        });
        if changed {
            self.sync_vocal_metadata();
            // (talk) スケール変更 (特に話速) は phoneme 長 = 口パクタイミングを変える。
            self.mark_lipsync_dirty();
        }
    }

    /// VOICEVOX engine が ready になったら `/singers` を取得して
    /// `SingersLoaded` を発行する (既存の死に配線を初めて発火させる)。 engine
    /// 起動 (`ensure_voicevox_engine`) と「再取得」(`RefetchSingers`) から呼ぶ。
    /// background thread (= ready 待ち + blocking HTTP) で走らせる。
    pub(crate) fn spawn_fetch_singers(&self) {
        let proxy = self.ipc.event_proxy.clone();
        std::thread::spawn(move || {
            // engine が ready になるまで待つ (未起動なら timeout で抜ける)。
            crate::voicevox_engine::wait_until_ready();
            let singers = crate::voicevox_client::fetch_singers().unwrap_or_else(|e| {
                tracing::warn!(error = ?e, "VOICEVOX /singers fetch failed");
                Vec::new()
            });
            proxy.send(AppEvent::SingersLoaded(singers));
        });
    }

    /// (talk) VOICEVOX engine が ready になったら `/speakers` (talk 声一覧) を取得して
    /// `SpeakersLoaded` を発行する。engine 起動 (`ensure_voicevox_engine`) と「再取得」
    /// (`RefetchSpeakers`) から呼ぶ。background thread (= ready 待ち + blocking HTTP)。
    pub(crate) fn spawn_fetch_speakers(&self) {
        let proxy = self.ipc.event_proxy.clone();
        std::thread::spawn(move || {
            crate::voicevox_engine::wait_until_ready();
            let speakers = crate::voicevox_client::fetch_speakers().unwrap_or_else(|e| {
                tracing::warn!(error = ?e, "VOICEVOX /speakers fetch failed");
                Vec::new()
            });
            proxy.send(AppEvent::SpeakersLoaded(speakers));
        });
    }

    /// gui_01 #017 (M14 Phase 59): piano_roll widget が L キー → Enter
    /// commit で発行する歌詞分配 batch を、 指定 `clip_ref` 内の note に
    /// 適用。 各 entry は `(note_index, Option<String>)`、 widget 側で空文字列
    /// は `None` に正規化済み (= 歌詞削除)。 clip_ref が無効なら no-op。
    pub(crate) fn set_note_lyrics(&mut self, clip_ref: ClipRef, updates: &[(u32, Option<String>)]) {
        self.edit_song_checked(|song| {
            let Some(notes) =
                song.notes_in_clip_mut(clip_ref.track as usize, clip_ref.clip as usize)
            else {
                return false;
            };
            let mut changed = false;
            for (id, lyric) in updates {
                if let Some(n) = notes.get_mut(*id as usize) {
                    let normalised = lyric.as_ref().and_then(|s| {
                        let t = s.trim();
                        if t.is_empty() { None } else { Some(t.to_string()) }
                    });
                    if n.lyric != normalised {
                        n.lyric = normalised;
                        changed = true;
                    }
                }
            }
            changed
        });
    }

}
