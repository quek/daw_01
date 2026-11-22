//! handler::chord_audition — コードの試聴 (r.md #157 Q7、`docs/plan_chord_track.md` §3.4)。
//!
//! 試聴は **既存の音源トラックを 1 本選んで鳴らす** (Cubase 型)。選んでいなければ無音。鳴らす鍵盤は
//! `common::chord_voicing::voice_chord` (ノート化と同じ関数) で決め、**書いた音のまま**送る — コードは
//! 書いた音の空間にあるので、コードへの追従は通さず、移調 (r.md #130) だけを足す
//! ([`AppData::transposed_preview_key`])。台帳は送った鍵盤を持つ (`HeldPreview`)。
//!
//! 鳴らし方は one-shot: 次のコードを鳴らすか期限 ([`CHORD_AUDITION_LEN`]) が来たら止める
//! (`on_tick` → [`AppData::expire_chord_audition`])。

use common::chord_voicing::{DEFAULT_VOICING_BASE, voice_chord};
use common::model::Chord;

use crate::app_types::PREVIEW_VELOCITY;
use crate::state::{AppData, HeldPreview};

/// 試聴 1 回の長さ。
pub(crate) const CHORD_AUDITION_LEN: std::time::Duration = std::time::Duration::from_millis(900);

impl AppData {
    /// コード `chord` を試聴トラックで短く鳴らす (帯のクリック / ポップアップの編集 / 候補の選択 / 鍵盤で
    /// 弾いたコード)。前の試聴音は止める。試聴トラックが無ければ何もしない。
    #[allow(dead_code)] // 帯 / ポップアップ (c157-band) が載るまで script からしか呼ばれない。載ったら外す。
    pub(crate) fn audition_chord(&mut self, chord: &Chord) {
        self.release_chord_audition();
        let Some(track_id) = self.chord_audition_track() else {
            return;
        };
        let tools = &self.cur.chord_tools;
        let prev = (!tools.last_voiced.is_empty()).then_some(tools.last_voiced.as_slice());
        let pitches = voice_chord(chord, tools.view.voicing, DEFAULT_VOICING_BASE, prev);
        let mut notes = Vec::with_capacity(pitches.len());
        for &pitch in &pitches {
            let key = self.transposed_preview_key(track_id, pitch);
            if let Some(k) = key {
                self.send_audio(common::protocol::AudioCommand::PreviewNoteOn {
                    project: self.pk(),
                    track_id,
                    pitch: k,
                    velocity: PREVIEW_VELOCITY,
                });
            }
            notes.push(HeldPreview { track_id, pitch, key });
        }
        let tools = &mut self.cur.chord_tools;
        tools.last_voiced = pitches;
        tools.audition.notes = notes;
        tools.audition.due = Some(std::time::Instant::now() + CHORD_AUDITION_LEN);
    }

    /// 試聴を鳴らすトラック (選ばれていて、今の Song に居るもの)。
    pub(crate) fn chord_audition_track(&self) -> Option<u32> {
        let id = self.cur.chord_tools.view.audition_track?;
        self.cur.song_doc.song().track_by_id(id).map(|t| t.id)
    }

    /// 鳴らしている試聴音を止める。
    pub(crate) fn release_chord_audition(&mut self) {
        let notes = std::mem::take(&mut self.cur.chord_tools.audition.notes);
        self.cur.chord_tools.audition.due = None;
        for held in notes {
            self.release_held_preview(held);
        }
    }

    /// `on_tick` から毎ティック: 試聴の期限、オーディオのコード検出の回収と自動発注。
    pub(crate) fn chord_tools_tick(&mut self) {
        self.expire_chord_audition(false);
        self.poll_chord_detection();
        self.auto_detect_followed_audio();
    }

    /// 曲を差し替えたとき: 試聴音を止め、旧 Song の音源 id 宛の検出を捨てる (音源 id は曲ごとに採番し直す)。
    pub(crate) fn reset_chord_tools_for_new_song(&mut self) {
        self.expire_chord_audition(true);
        let tools = &mut self.cur.chord_tools;
        tools.last_voiced.clear();
        tools.detect_jobs.clear();
        tools.auto_detect_epoch = None;
    }

    /// 期限が来た試聴音を止める。`force` は停止 / 曲差し替えの即時消音。
    pub(crate) fn expire_chord_audition(&mut self, force: bool) {
        let Some(due) = self.cur.chord_tools.audition.due else {
            return;
        };
        if force || std::time::Instant::now() >= due {
            self.release_chord_audition();
        }
    }
}

#[cfg(test)]
mod tests {
    use common::model::{Chord, ChordKind};
    use common::protocol::AudioCommand;

    use crate::event::AppEvent;
    use crate::event_chord::ChordToolEvent;

    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AudioCommand>) -> Vec<(bool, u32, u8)> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|c| match c {
                AudioCommand::PreviewNoteOn { track_id, pitch, .. } => Some((true, track_id, pitch)),
                AudioCommand::PreviewNoteOff { track_id, pitch, .. } => Some((false, track_id, pitch)),
                _ => None,
            })
            .collect()
    }

    /// 試聴トラックが無ければ無音、選べばそのトラックへ voicing どおりの鍵盤 (移調込み) を送り、
    /// 次のコードで前の音を止める。追従するトラックでも書いた音のまま鳴らす。
    #[test]
    fn audition_sounds_the_voicing_on_the_chosen_track_and_replaces_the_previous_chord() {
        let mut app = crate::test_support::headless_app();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        app.ipc.audio_tx = Some(tx);
        let t1 = app.cur.song_doc.song().tracks[0].id;
        app.audition_chord(&Chord::new(0, ChordKind::Major));
        assert!(drain(&mut rx).is_empty(), "未選択なら無音");

        app.handle_event(AppEvent::ChordTool(ChordToolEvent::SetAuditionTrack(Some(t1))));
        assert!(!app.cur.song_doc.is_dirty(), "聴き方の設定は * を立てない");
        app.handle_event(AppEvent::SetSongTranspose(2));
        app.handle_event(AppEvent::ChordFollow(crate::event_chord::ChordFollowEvent::SetEnabled(true)));
        app.handle_event(AppEvent::ChordFollow(crate::event_chord::ChordFollowEvent::SetTracks {
            track_ids: vec![t1],
            mode: common::model::ChordFollow::Narrow,
        }));
        app.audition_chord(&Chord::new(0, ChordKind::Major));
        // 基本形の C (C2 C3 E3 G3) + 移調 2。
        assert_eq!(drain(&mut rx), vec![(true, t1, 38), (true, t1, 50), (true, t1, 54), (true, t1, 57)]);
        app.audition_chord(&Chord::new(9, ChordKind::Minor));
        let sent = drain(&mut rx);
        assert_eq!(&sent[..4], &[(false, t1, 38), (false, t1, 50), (false, t1, 54), (false, t1, 57)]);
        assert_eq!(sent.len(), 8);
        app.expire_chord_audition(true);
        assert_eq!(drain(&mut rx).len(), 4, "期限 / 停止で止める");
        assert_eq!(app.snapshot_view_state().chord_tools.audition_track, Some(t1), "試聴トラックは保存する");
    }
}
