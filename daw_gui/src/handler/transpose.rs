//! handler::transpose — r.md #130 グローバルトランスポーズの GUI 側の口 (`docs/plan_rmd_130_transpose.md`)。
//!
//! 移調量の評価と「追従するか」は `common::transpose` が SSoT で、ここはそれを GUI の状態 (playhead /
//! engine が publish した変調の値面 / plugin DB) につなぐだけ:
//!
//! - 基準値の編集 ([`AppData::set_song_transpose`]) と トラックの追従の切替 ([`AppData::set_tracks_follow_transpose`])
//! - 演奏プレビュー (MIDI 入力モニター / 仮想鍵盤 / 鍵盤レーン / ナッジ試聴 / MIDI Capture の試聴) が鳴らす鍵盤
//!   ([`AppData::preview_key`])。**台帳には送った鍵盤を持つ** — 生の pitch を持って消音時に移調し直すと、
//!   押している間に移調が変わったとき別の鍵盤を止めにいく。
//! - ヘッダの「追従しない」印 ([`AppData::track_shows_no_transpose_mark`])

use common::model::Track;
use common::protocol::AudioCommand;

use crate::app_types::{PREVIEW_VELOCITY, PreviewAction, diff_preview};
use crate::state::{AppData, HeldPreview, StreamGesture};

impl AppData {
    /// 移調の基準値 (`Song::transpose`) を `semitones` にする (±24 に丸める)。transport の欄のドラッグ / 入力と
    /// MIDI Learn の CC が通る唯一の口。連続する値は 1 undo step に畳み、値が変わったときだけ編集して engine へ
    /// 軽量 IPC で即時に届ける (LoadSong はフレーム末の同期がまとめて送る = BPM と同じ形)。
    pub(crate) fn set_song_transpose(&mut self, semitones: i8) {
        let max = common::transpose::TRANSPOSE_MAX_SEMITONES;
        let next = semitones.clamp(-max, max);
        self.cur.song_doc.use_stream_scope(StreamGesture::TransposeScrub);
        let changed = self.edit_song_checked(|song| {
            let changed = song.transpose != next;
            song.transpose = next;
            changed
        });
        if changed {
            self.send_audio(AudioCommand::SetSongTranspose { project: self.pk(), semitones: next });
        }
    }

    /// トラック `track_ids` の「移調に追従」を `follow` にする (1 undo step、変化が無ければ何もしない)。
    /// engine / VOICEVOX / 印はフレーム末の同期と描画が Song から読み直す。
    pub(crate) fn set_tracks_follow_transpose(&mut self, track_ids: &[u32], follow: bool) {
        self.edit_song_checked(|song| {
            let mut changed = false;
            for t in song.tracks.iter_mut().filter(|t| track_ids.contains(&t.id)) {
                changed |= t.follow_transpose != follow;
                t.follow_transpose = follow;
            }
            changed
        });
    }

    /// トラック `track_id` で `pitch` を弾いたとき **いま鳴らす鍵盤** (r.md #130 確定仕様 Q2: 移調楽器として
    /// 扱う)。移調量は playhead の位置のレーン値 + engine が publish した変調 — 再生と同じ
    /// `common::transpose::transpose_in_clip` を通る。追従しないトラックは弾いた鍵盤のまま、範囲外は `None`。
    pub(crate) fn preview_key(&self, track_id: u32, pitch: u8) -> Option<u8> {
        let song = self.cur.song_doc.song();
        if !song.track_follows_transpose(track_id) {
            return Some(pitch);
        }
        let beat = self.cur.transport.playhead_beat.map_or(0.0, f64::from).max(0.0);
        let plane = self.cur.transport.mod_plane.as_ref();
        let semitones = common::transpose::transpose_at(song, beat, |id| plane.scalar_opt(id), |r| {
            plane.depth(r.id).unwrap_or(r.depth)
        });
        common::transpose::sounding_key(pitch, semitones)
    }

    /// プレビューの発音の唯一の口: `pitch` を [`Self::preview_key`] で鍵盤に直して送り、**送った鍵盤**を返す
    /// (呼び出し側の台帳はこれを持ち、消音は [`Self::send_preview_off`] にこの鍵盤を渡す)。範囲外なら送らない。
    pub(crate) fn send_preview_on(&self, track_id: u32, pitch: u8, velocity: u8) -> Option<u8> {
        let key = self.preview_key(track_id, pitch)?;
        self.send_audio(AudioCommand::PreviewNoteOn { project: self.pk(), track_id, pitch: key, velocity });
        Some(key)
    }

    /// [`Self::send_preview_on`] が送った鍵盤 `key` を止める。
    pub(crate) fn send_preview_off(&self, track_id: u32, key: u8) {
        self.send_audio(AudioCommand::PreviewNoteOff { project: self.pk(), track_id, pitch: key });
    }

    /// 鳴らしているプレビュー 1 音を止める (範囲外で送っていなければ何もしない)。
    pub(crate) fn release_held_preview(&self, held: HeldPreview) {
        if let Some(key) = held.key {
            self.send_preview_off(held.track_id, key);
        }
    }

    /// 鍵盤レーンのプレビュー (gui_01 #055): 押している `(track_id, pitch)` を前フレームの値と差分して
    /// note-off / note-on を送る。消音は前に **送った鍵盤**、発音は今の移調量で解いた鍵盤。
    pub(crate) fn set_keyboard_lane_preview(&mut self, next: Option<(u32, u8)>) {
        let prev = self.cur.recording.preview_note;
        for action in diff_preview(prev.map(|h| (h.track_id, h.pitch)), next) {
            match action {
                PreviewAction::NoteOff { .. } => {
                    if let Some(held) = prev {
                        self.release_held_preview(held);
                    }
                }
                PreviewAction::NoteOn { track_id, pitch } => {
                    let key = self.send_preview_on(track_id, pitch, PREVIEW_VELOCITY);
                    self.cur.recording.preview_note = Some(HeldPreview { track_id, pitch, key });
                }
            }
        }
        if next.is_none() {
            self.cur.recording.preview_note = None;
        }
    }

    /// ヘッダの名前の横に「移調に追従しない」印を出すか: 実効的に追従しない (自分か祖先グループで外した)、
    /// または ARA のプラグイン (Melodyne 等) を持つ — ARA はプラグインが素材を直接読むので、ホストの移調が
    /// そのトラックのオーディオに効かない (`docs/plan_rmd_130_transpose.md` の「main が決めた細部」)。
    pub fn track_shows_no_transpose_mark(&self, track: &Track) -> bool {
        if !self.cur.song_doc.song().track_follows_transpose(track.id) {
            return true;
        }
        self.ipc
            .plugin_db
            .as_deref()
            .is_some_and(|db| track.plugins().any(|d| db.find_by_id(&d.plugin_id).is_some_and(|e| e.is_ara())))
    }
}

#[cfg(test)]
mod tests {
    use common::model::{AutomationLane, AutomationTarget};
    use common::protocol::AudioCommand;

    use crate::event::AppEvent;

    /// 送られた audio コマンドを全部取り出す。
    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AudioCommand>) -> Vec<AudioCommand> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// transport の欄の連続した値は 1 undo step・値が変わったときだけ即時 IPC、±24 に丸める。
    #[test]
    fn transport_edits_are_one_undo_step_and_reach_the_engine_immediately() {
        let mut app = crate::test_support::headless_app();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        app.ipc.audio_tx = Some(tx);
        let depth = app.cur.song_doc.undo_depth();
        for v in [1, 2, 2, 99] {
            app.handle_event(AppEvent::SetSongTranspose(v));
        }
        assert_eq!(app.cur.song_doc.song().transpose, 24);
        assert_eq!(app.cur.song_doc.undo_depth(), depth + 1, "ドラッグ 1 回 = 1 undo step");
        let sent: Vec<i8> = drain(&mut rx)
            .into_iter()
            .filter_map(|c| match c {
                AudioCommand::SetSongTranspose { semitones, .. } => Some(semitones),
                _ => None,
            })
            .collect();
        assert_eq!(sent, vec![1, 2, 24], "同じ値は送らない");
        app.handle_event(AppEvent::Undo);
        assert_eq!(app.cur.song_doc.song().transpose, 0);
    }

    /// 演奏プレビューは **移調楽器**: 追従するトラックは playhead の位置の移調量 (レーンがあればレーン) で鳴らし、
    /// 追従しないトラックは弾いた鍵盤のまま、範囲外は鳴らさない。台帳は送った鍵盤を持つので、押している間に
    /// 移調が変わっても離したときに鳴らした鍵盤が止まる。
    #[test]
    fn previews_sound_the_transposed_key_and_release_the_key_they_sent() {
        let mut app = crate::test_support::headless_app();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        app.ipc.audio_tx = Some(tx);
        let t1 = app.cur.song_doc.song().tracks[0].id;
        app.handle_event(AppEvent::AddInstrumentTrack);
        let t2 = app.cur.song_doc.song().tracks.iter().map(|t| t.id).find(|&id| id != t1).expect("2 本目");
        app.handle_event(AppEvent::SetTracksFollowTranspose { track_ids: vec![t2], follow: false });
        app.handle_event(AppEvent::SetSongTranspose(2));
        assert_eq!((app.preview_key(t1, 60), app.preview_key(t2, 60)), (Some(62), Some(60)));
        assert_eq!(app.preview_key(t1, 126), None, "範囲外は鳴らさない");
        app.edit_song(|song| {
            song.song_lanes.push(AutomationLane { id: 1, ..AutomationLane::new(AutomationTarget::SongTranspose, -12.0) });
        });
        assert_eq!(app.preview_key(t1, 60), Some(48), "レーンがあればレーンの値");

        // MIDI 入力モニター: 押した瞬間の鍵盤で鳴らし、移調が変わってから離しても同じ鍵盤を止める。
        app.edit_song(|song| song.tracks.iter_mut().for_each(|t| t.armed = t.id == t1));
        drain(&mut rx);
        app.handle_event(AppEvent::MidiNoteOn { channel: 0, pitch: 60, velocity: 100 });
        app.edit_song(|song| song.song_lanes.clear());
        app.handle_event(AppEvent::MidiNoteOff { channel: 0, pitch: 60 });
        let keys: Vec<(bool, u8)> = drain(&mut rx)
            .into_iter()
            .filter_map(|c| match c {
                AudioCommand::PreviewNoteOn { pitch, .. } => Some((true, pitch)),
                AudioCommand::PreviewNoteOff { pitch, .. } => Some((false, pitch)),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec![(true, 48), (false, 48)]);
    }

    /// ヘッダの印: 自分で外したトラックと、外したグループの子に出る。
    #[test]
    fn no_transpose_mark_follows_the_group_inheritance() {
        let mut app = crate::test_support::headless_app();
        let t1 = app.cur.song_doc.song().tracks[0].id;
        app.handle_event(AppEvent::AddInstrumentTrack);
        let t2 = app.cur.song_doc.song().tracks.iter().map(|t| t.id).find(|&id| id != t1).expect("2 本目");
        app.edit_song(|song| song.track_by_id_mut(t2).expect("t2").parent_group_id = Some(t1));
        let marks = |app: &crate::state::AppData| {
            let song = app.cur.song_doc.song();
            [t1, t2].map(|id| app.track_shows_no_transpose_mark(song.track_by_id(id).expect("track")))
        };
        assert_eq!(marks(&app), [false, false]);
        app.handle_event(AppEvent::SetTracksFollowTranspose { track_ids: vec![t1], follow: false });
        assert_eq!(marks(&app), [true, true], "グループで外すと子にも出る");
    }
}
