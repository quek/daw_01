//! handler::chord_follow_view — 「このクリップのノート 1 つが鳴る鍵盤」の読み口 (r.md #157、`docs/plan_chord_track.md` §4)。
//!
//! ピアノロールの「鳴る音の薄い枠」(Q5) と「鳴る音で確定」(`ChordFollowEvent::FreezePitch`) が同じこれを引く
//! (表示と確定で別の鍵盤を出さない)。評価は `common::chord_follow` が SSoT で、ここは素材のコードの検出を
//! クリップごとに 1 回にまとめるだけ。
//!
//! 返すのは **追従だけ** の鍵盤 (書いた音の空間)。グローバルトランスポーズ (r.md #130) はピアノロールに出さない
//! (移調楽器として扱う、#130 Q2) ので含めない。

use common::chord_follow::TrackChordFollow;
use common::model::{Clip, Note, Song, SourceChord};

use crate::state::AppData;

/// クリップ 1 つのノートが鳴らす鍵盤 (追従だけ)。[`AppData::clip_sounding_keys`] で作る。
pub struct ClipSoundingKeys<'a> {
    follow: TrackChordFollow<'a>,
    clip: &'a Clip,
    /// 平行移動 / ベースの基準 (このクリップの content の素材のコード、content-local 拍)。
    sources: Vec<SourceChord>,
}

impl<'a> ClipSoundingKeys<'a> {
    /// トラック `track_id` の **アレンジの** クリップ `clip` の読み口。ランチャーのセルは曲の位置を持たない
    /// (撃った瞬間の playhead のコードで鳴る) ので、鳴る音を先に決められない → 書いた音のまま (追従しない読み口)。
    #[must_use]
    pub fn of(song: &'a Song, track_id: u32, clip: &'a Clip) -> Self {
        let in_song = song.track_by_id(track_id).is_some_and(|t| t.clip_by_id(clip.id).is_some());
        let follow =
            if in_song { TrackChordFollow::of(song, track_id) } else { TrackChordFollow::written(song) };
        let sources = match song.clip_contents.get(&clip.content_id).and_then(|c| c.notes()) {
            Some(notes) if follow.is_active() && follow.needs_source() => {
                common::chord_follow::note_source_chords(notes)
            }
            _ => Vec::new(),
        };
        Self { follow, clip, sources }
    }

    /// 追従で鳴る音が書いた音と違いうるか (`false` なら呼び出し側は枠の描画を丸ごと飛ばせる)。
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.follow.is_active()
    }

    /// ノート `note` (このクリップの content のノート) が鳴らす鍵盤 (書いた音の空間)。0..=127 を外れて鳴らないなら
    /// `None`。コードはノートの開始位置で決まる。
    #[must_use]
    pub fn key(&self, note: &Note) -> Option<u8> {
        let source = common::chord_detect::source_chord_at(&self.sources, note.start_beat);
        self.follow.key(note.pitch, self.clip.content_to_song_beat(note.start_beat), source)
    }
}

impl AppData {
    /// トラック `track_id` のクリップ `clip_id` (アレンジ / ランチャーのセル) の鳴る鍵盤の読み口。
    /// クリップが無ければ `None`。毎フレーム呼んでよい (検出はクリップの content のノート数に比例)。
    #[must_use]
    pub fn clip_sounding_keys(&self, track_id: u32, clip_id: u32) -> Option<ClipSoundingKeys<'_>> {
        let song = self.cur.song_doc.song();
        let track = song.track_by_id(track_id)?;
        let clip = track.clip_by_id(clip_id).or_else(|| track.session_clip_by_id(clip_id).map(|c| &c.clip))?;
        Some(ClipSoundingKeys::of(song, track_id, clip))
    }
}
