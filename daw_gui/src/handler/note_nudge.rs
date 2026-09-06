//! handler::note_nudge — r.md #67: カーソルキーによるノート編集 (移動 / 伸縮 / 音程) と、
//! ナッジ後の追従スクロール・試聴。
//!
//! `handler/notes.rs` から切り出した `impl AppData` メソッド群 (挙動は元と同一、不変条件 9 の
//! サイズ budget)。
use crate::state::*;
use crate::app_types::*;
use crate::event::*;
use common::protocol::AudioCommand;

use super::notes::{NOTE_MIN_LEN_BEATS, NUDGE_AUDITION_LEN};

impl AppData {
    /// カーソルキー 1 押しが動かす拍数。
    ///
    /// - `Grid`: ピアノロールの現在のスナップ 1 つ分。 スナップ OFF / `Off` のときは
    ///   「グリッド」 が定義できないので `Fine` に倒す (Ableton Live / Bitwig と同じ)。
    /// - `Bar`: 1 小節 (`time_sig.0 * 4 / time_sig.1` 拍)。
    /// - `Fine`: グリッド 1 つの 1/16。 スナップ設定が `Off` でも 1/16 音符基準で決まる
    ///   (= 何も設定していなくても常に微調整できる)。
    ///
    /// 戻り値は必ず正の有限値。
    #[must_use]
    pub fn nudge_beat_unit(&self, step: NudgeStep) -> f64 {
        let cfg = crate::view::snap::piano_roll_snap_config(self);
        let zoom = self.pianoroll_zoom_x();
        // スナップ OFF でも「選ばれている分割」 は決まっているので、 有効化した設定で
        // unit を求める (トグル 1 つで微調整量まで変わってしまうのを避ける)。
        let selected_unit = crate::view::snap::SnapConfig { enabled: true, ..cfg }
            .beat_unit(zoom)
            .filter(|u| u.is_finite() && *u > 0.0)
            // `SnapMode::Off` は `beat_unit` が None。 1/16 音符 (= ピアノロール既定) を基準に。
            .unwrap_or(0.25);
        match step {
            NudgeStep::Bar => {
                let (num, den) = self.song_doc.song().time_sig;
                f64::from(num.max(1)) * 4.0 / f64::from(den.max(1))
            }
            NudgeStep::Grid if cfg.is_active(false) => selected_unit,
            // Alt (Fine) と「スナップ OFF での Grid」 は同じ微移動量。
            NudgeStep::Grid | NudgeStep::Fine => selected_unit / 16.0,
        }
    }

    /// 選択ノートのうち **いま編集できるもの** を `(packed id, start_beat, duration_beats,
    /// pitch)` で返す (start / duration は content-local 拍 = model の値そのもの)。
    ///
    /// ロック中クリップ / 解決できない stale id は除外する。 **除外を delta 決定の前に
    /// やる** のが要点で、 ロック中の音が端に居るせいで選択全体が動かない、 を防ぐ。
    fn editable_selected_notes(&self) -> Vec<(u32, f64, f64, u8)> {
        let shown = self.shown_pianoroll_clips();
        let mut out = Vec::with_capacity(self.selected_note_ids().len());
        for &id in &self.selected_note_ids() {
            let Some((r, local)) = Self::decode_note_id_in(&shown, id) else {
                continue;
            };
            if self.is_pianoroll_clip_locked_in(&shown, r) {
                continue;
            }
            let Some(clip) = self
                .song_doc
                .song()
                .track_by_id(r.track_id)
                .and_then(|t| t.clip_by_id(r.clip_id))
            else {
                continue;
            };
            let Some(n) = self.song_doc.song().clip_notes(clip).get(local) else {
                continue;
            };
            out.push((id, n.start_beat, n.duration_beats, n.pitch));
        }
        out
    }

    /// ←/→: 選択ノートを時間軸に移動する。
    ///
    /// **クリップの端ではクランプしない**。 daw_01 の clip は共有 content への「窓」 なので
    /// (`Clip::content_origin_beat`)、 窓の外へ出たノートは鳴らない / 描かれないだけで
    /// データは無傷 — 窓を広げれば戻ってくる。 他 DAW も矢印移動でクリップを伸ばさない。
    /// 下限だけは content-local 拍 0 (= model が保持できる最小位置、 ドラッグと同じ)。
    pub(crate) fn nudge_selected_notes_time(&mut self, step: NudgeStep, steps: i32) {
        if steps == 0 {
            return;
        }
        let movable = self.editable_selected_notes();
        if movable.is_empty() {
            return;
        }
        let raw = self.nudge_beat_unit(step) * f64::from(steps);
        // 集合クランプ: 端に当たった 1 音のために全体の相対位置が潰れないようにする。
        let delta = crate::widgets::piano_roll::clamp_shared_delta(
            movable.iter().map(|&(_, start, _, _)| start),
            raw,
            0.0,
            f64::INFINITY,
        );
        if delta.abs() < 1e-9 {
            return; // 端で止まっている (spurious な undo step を積まない)
        }
        let entries: Vec<(u32, f64, u8)> = movable
            .iter()
            .map(|&(id, start, _, pitch)| (id, start + delta, pitch))
            .collect();
        // 連続したカーソル操作は 1 undo step に畳む (押しっぱなしで 100 step は誤り)。
        self.song_doc.use_stream_scope(StreamGesture::NoteNudgeMove);
        self.set_note_positions(&entries);
        self.follow_nudged_notes();
    }

    /// Ctrl+←/→: 選択ノートの長さを伸縮する。
    ///
    /// 位置と同じく **集合クランプ**。 1 音ずつ下限で止めると相対的な長さの比が壊れ、
    /// 逆キーを押しても元に戻らない (可逆でなくなる)。
    pub(crate) fn nudge_selected_notes_length(&mut self, step: NudgeStep, steps: i32) {
        if steps == 0 {
            return;
        }
        let movable = self.editable_selected_notes();
        if movable.is_empty() {
            return;
        }
        let raw = self.nudge_beat_unit(step) * f64::from(steps);
        let delta = crate::widgets::piano_roll::clamp_shared_delta(
            movable.iter().map(|&(_, _, dur, _)| dur),
            raw,
            NOTE_MIN_LEN_BEATS,
            f64::INFINITY,
        );
        if delta.abs() < 1e-9 {
            return;
        }
        let entries: Vec<(u32, f64, f64)> = movable
            .iter()
            .map(|&(id, start, dur, _)| (id, start, dur + delta))
            .collect();
        self.song_doc.use_stream_scope(StreamGesture::NoteNudgeLength);
        self.resize_notes(&entries);
        self.follow_nudged_notes();
    }

    /// ↑/↓ (`octave = false`) / Shift+↑/↓ (`octave = true`): 選択ノートの音程を変える。
    ///
    /// **スケールが効いているときは半音ではなくスケール音 1 つ単位**で動く:
    /// - Fold 表示中は行そのものが in-scale 音なので、 半音で動かすと画面の 1 行と一致しない。
    /// - Snap on Draw 中は書き込み口 (`set_note_positions`) が pitch を in-scale に吸着するので、
    ///   半音で動かすと吸着で押し戻されて **キーが効かない** ように見える。
    ///
    /// オクターブは Fold / Snap on Draw 中は「そのスケールの音数」 ぶんの degree
    /// (7 音音階なら 7 degree = 12 半音)。
    pub(crate) fn nudge_selected_notes_pitch(&mut self, octave: bool, steps: i32) {
        if steps == 0 {
            return;
        }
        let movable = self.editable_selected_notes();
        if movable.is_empty() {
            return;
        }
        let scale = self.pianoroll_scale();
        // 「スケール音 1 つ単位で動かすか」。 Fold は表示がそうなっているから、
        // Snap on Draw は書き戻しで吸着されるから (どちらも半音移動が破綻する)。
        let degree_scale = scale.filter(|sc| {
            matches!(sc.mode, crate::widgets::piano_roll::PianoRollScaleMode::Fold)
                || self.ui_prefs.snap_on_draw
        });
        let per_step = match (octave, degree_scale) {
            // スケールの 1 オクターブ = そのスケールの音数ぶんの degree。
            (true, Some(sc)) => i32::try_from(sc.in_scale_mask.count_ones()).unwrap_or(12).max(1),
            (true, None) => 12,
            (false, _) => 1,
        };
        let raw = steps.saturating_mul(per_step);
        let delta = crate::widgets::piano_roll::clamp_shared_pitch_delta(
            movable.iter().map(|&(_, _, _, pitch)| pitch),
            raw,
            degree_scale,
        );
        if delta == 0 {
            return;
        }
        let entries: Vec<(u32, f64, u8)> = movable
            .iter()
            .map(|&(id, start, _, pitch)| {
                let next = match degree_scale {
                    Some(sc) => sc.scale_degree_to_pitch(sc.pitch_to_scale_degree(pitch) + delta),
                    None => u8::try_from((i32::from(pitch) + delta).clamp(0, 127)).unwrap_or(pitch),
                };
                (id, start, next)
            })
            .collect();
        self.song_doc.use_stream_scope(StreamGesture::NoteNudgeMove);
        self.set_note_positions(&entries);
        self.follow_nudged_notes();
        self.audition_anchor_note();
    }

    /// 動かした選択が画面外へ出たらピアノロールをスクロールして追う。
    ///
    /// 表示範囲の広さは view しか知らない (grid の px サイズ ÷ zoom) ので、widget が毎フレーム
    /// mirror している `pianoroll_viewport` を読む。まだ 1 度も描かれていないとき (= 値なし)
    /// は追従しない (見えていない物を追う必要がない)。
    ///
    /// 追従の基準は **アンカー** (最後に選んだ 1 音)。 選択集合全体を入れようとすると
    /// 広い選択ではズームを変えるしかなくなり、 意図しない表示倍率変更になる。
    fn follow_nudged_notes(&mut self) {
        let Some((visible_beats, visible_pitches)) = self.ui_ephemeral.pianoroll_viewport else {
            return;
        };
        let Some((_, r, start, dur, pitch)) = self.anchor_selected_note() else {
            return;
        };
        // widget が note を描く位置は song-absolute (= content-local + content 原点)。
        let song_start = start + self.clip_start_beat_of(r);
        let song_end = song_start + dur;
        // `scroll_beat` の原点は view と同じ規則: 複数表示は song 原点、単一表示は
        // **対象クリップの窓の開始** (`clip.start_beat`。content 原点ではない — 左端を trim した
        // クリップでは両者がずれる。r.md #44)。ここを間違えると trim 済クリップで追従がずれる。
        let scroll_origin = if self.shown_pianoroll_clips().len() >= 2 {
            0.0
        } else {
            self.pianoroll_target_clip()
                .and_then(|t| {
                    self.song_doc
                        .song()
                        .track_by_id(t.track_id)
                        .and_then(|tr| tr.clip_by_id(t.clip_id))
                })
                .map_or(0.0, |c| c.start_beat)
        };
        let rows = f64::from(visible_pitches).floor().max(1.0);
        let Some(view) = self.piano_roll_view_entry() else {
            return;
        };
        #[allow(clippy::cast_possible_truncation)]
        {
            let left = f64::from(view.scroll_beat) + scroll_origin;
            let right = left + visible_beats;
            if song_start < left {
                view.scroll_beat = (song_start - scroll_origin).max(0.0) as f32;
            } else if song_end > right {
                view.scroll_beat = (song_end - visible_beats - scroll_origin).max(0.0) as f32;
            }
            // 可視 MIDI pitch 範囲は Fold / linear どちらも `[pitch_top - pitch_visible, pitch_top]`
            // (Fold は同じ範囲を in-scale 行だけで並べ直すだけ、`RowGeometry::compute`)。
            let top = i32::from(view.top_pitch);
            let bottom = top - rows as i32 + 1;
            let p = i32::from(pitch);
            if p > top {
                view.top_pitch = u8::try_from(p.clamp(0, 127)).unwrap_or(view.top_pitch);
            } else if p < bottom {
                let next = p + rows as i32 - 1;
                view.top_pitch = u8::try_from(next.clamp(0, 127)).unwrap_or(view.top_pitch);
            }
        }
    }

    /// 選択のアンカー (= `selected_notes` の末尾、`SetNoteSelection` が anchor として扱う音) を
    /// `(packed id, 所属 ClipKey, start_beat, duration_beats, pitch)` で返す。
    fn anchor_selected_note(&self) -> Option<(u32, ClipKey, f64, f64, u8)> {
        let &id = self.selected_note_ids().last()?;
        let shown = self.shown_pianoroll_clips();
        let (r, local) = Self::decode_note_id_in(&shown, id)?;
        let clip = self
            .song_doc
            .song()
            .track_by_id(r.track_id)
            .and_then(|t| t.clip_by_id(r.clip_id))?;
        let n = self.song_doc.song().clip_notes(clip).get(local)?;
        Some((id, r, n.start_beat, n.duration_beats, n.pitch))
    }

    /// 音程が変わったとき、アンカーの 1 音を短く鳴らす (Cubase の Acoustic Feedback 相当)。
    ///
    /// 鍵盤レーンのプレビュー (`recording.preview_note`) は **押している間ずっと鳴らす**
    /// held-value なので流用できない (widget が毎フレーム差分して即 note-off を送ってしまう)。
    /// こちらは時間で自動消音する one-shot として別枠で持ち、`on_tick` が消音する。
    ///
    /// 実際に pitch が変わったときだけ発音する = 端で止まっているときや、
    /// キーリピートで同じ音に留まるときは鳴らさない (「押しっぱなしで連打」 を防ぐ)。
    fn audition_anchor_note(&mut self) {
        let Some((_, r, _, _, pitch)) = self.anchor_selected_note() else {
            return;
        };
        // 鳴らすのは **そのノート自身のトラック** の音源 (複数クリップ表示では対象クリップと
        // 別トラックのノートを掴んでいることがある)。
        let Some(track_id) = self
            .song_doc
            .song()
            .track_by_id(r.track_id)
            .map(|t| t.id)
        else {
            return;
        };
        if let Some((prev_track, prev_pitch, _)) = self.recording.nudge_audition {
            if prev_track == track_id && prev_pitch == pitch {
                return; // 同じ音のまま = 鳴らし直さない
            }
            self.send_audio(AudioCommand::PreviewNoteOff { track_id: prev_track, pitch: prev_pitch });
        }
        self.send_audio(AudioCommand::PreviewNoteOn {
            track_id,
            pitch,
            velocity: PREVIEW_VELOCITY,
        });
        self.recording.nudge_audition =
            Some((track_id, pitch, std::time::Instant::now() + NUDGE_AUDITION_LEN));
    }

    /// [`Self::audition_anchor_note`] が鳴らした試聴音を、期限が来ていれば消音する
    /// (`on_tick` から毎ティック呼ばれる)。`force` は停止 / 曲差し替え / 終了時の即時消音。
    pub(crate) fn expire_nudge_audition(&mut self, force: bool) {
        let Some((track_id, pitch, due)) = self.recording.nudge_audition else {
            return;
        };
        if !force && std::time::Instant::now() < due {
            return;
        }
        self.recording.nudge_audition = None;
        self.send_audio(AudioCommand::PreviewNoteOff { track_id, pitch });
    }
}
