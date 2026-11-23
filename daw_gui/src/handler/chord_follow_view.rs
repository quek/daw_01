//! handler::chord_follow_view — 「このクリップのノート 1 つが鳴る鍵盤」の読み口 (r.md #157、`docs/plan_chord_track.md` §4)。
//!
//! ピアノロールの「鳴る音の薄い枠」(Q5) と「鳴る音で確定」(`ChordFollowEvent::FreezePitch`) が同じこれを引く
//! (表示と確定で別の鍵盤を出さない)。評価は `common::chord_follow` が SSoT で、ここは自分の和音 (トラックのアレンジ /
//! セルが鳴らすノートから検出、`common::chord_follow::own`) と、クリップの記録 (r.md #159 `Clip::chord_basis`) を
//! 渡すだけ。
//!
//! 自分の和音はピアノロールが毎フレーム引くので、プロジェクトごとの世代キャッシュ ([`OwnChordsCache`]、
//! `ProjectEphemeral::own_chords_cache`) から取る: 曲が変わらないフレームは検出 0 回、linked clip も素材 1 つに
//! 1 回、変わったフレームも鳴らすノートが同じ素材は使い回す (フェーダーのドラッグ)。
//!
//! 返すのは **追従だけ** の鍵盤 (書いた音の空間)。グローバルトランスポーズ (r.md #130) はピアノロールに出さない
//! (移調楽器として扱う、#130 Q2) ので含めない。

use std::collections::HashMap;
use std::sync::Arc;

use common::chord_follow::{NoteSource, OwnChordsMemo, OwnScope, TrackChordFollow};
use common::model::{Clip, ClipKey, Note, NoteChordBasis, Song, SourceChord};

use crate::state::AppData;

/// 自分の和音の世代キャッシュ。鍵は `SongDoc::sync_epoch` (Song の中身が変わるたびに進む — 再生状態の編集
/// (`edit_playback`) も含む)。`cur` (プロジェクト) ごとに持つので、タブを替えても別のプロジェクトの世代と混ざらない。
#[derive(Default)]
pub(crate) struct OwnChordsCache {
    /// `fresh` が対応する世代。
    epoch: u64,
    built: bool,
    /// この世代で引いた素材。
    fresh: HashMap<OwnScope, Arc<[SourceChord]>>,
    /// 世代をまたいで、鳴らすノートが同じ素材の検出を使い回す。
    memo: OwnChordsMemo,
}

/// クリップ 1 つのノートが鳴らす鍵盤 (追従だけ)。[`AppData::clip_sounding_keys`] で作る。
pub struct ClipSoundingKeys<'a> {
    follow: TrackChordFollow<'a>,
    clip: &'a Clip,
    /// 基準の自分の和音 (このクリップのトラックのアレンジが鳴らすノートの、曲の拍)。
    own: Arc<[SourceChord]>,
    /// 置いた時点のコードの記録 (既定はクリップの記録。[`Self::with`] で差し替える)。
    basis: &'a [NoteChordBasis],
}

impl<'a> ClipSoundingKeys<'a> {
    /// トラック `track_id` の **アレンジの** クリップ `clip` の読み口 (自分の和音は `own(scope)` が返す — 追従する
    /// ときだけ呼ぶ。GUI は [`AppData::own_chords`]、Song だけの経路は自前の置き場)。ランチャーのセルは曲の位置を
    /// 持たない (撃った瞬間の playhead のコードで鳴る) ので、鳴る音を先に決められない → 書いた音のまま (追従しない
    /// 読み口)。
    pub fn of(
        song: &'a Song,
        track_id: u32,
        clip: &'a Clip,
        basis: &'a [NoteChordBasis],
        own: impl FnOnce(OwnScope) -> Arc<[SourceChord]>,
    ) -> Self {
        let scope = song.own_scope_of(ClipKey { track_id, clip_id: clip.id });
        let follow = match scope {
            OwnScope::Arrangement { .. } => TrackChordFollow::of(song, track_id),
            OwnScope::Cell { .. } => TrackChordFollow::written(song),
        };
        let own = if follow.is_active() { own(scope) } else { Arc::from([]) };
        Self { follow, clip, own, basis }
    }

    /// [`AppData::clip_sounding_keys`] のノートと記録を差し替えたもの (「鳴る音で確定」が、確定した後の鳴る音を
    /// 確かめる。差し替えたノートは Song に無いので、キャッシュを使わず検出する)。
    #[must_use]
    pub fn with(song: &'a Song, track_id: u32, clip: &'a Clip, notes: &[Note], basis: &'a [NoteChordBasis]) -> Self {
        Self::of(song, track_id, clip, basis, |scope| {
            common::chord_follow::own_chords_in(song, scope, Some((clip.id, notes))).into()
        })
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
        let src = NoteSource::of(&self.own, self.basis, self.clip, note);
        self.follow.key(note.pitch, self.clip.content_to_song_beat(note.start_beat), &src)
    }
}

impl AppData {
    /// 素材 `scope` の自分の和音 (世代キャッシュ、[`OwnChordsCache`])。毎フレーム呼んでよい。
    #[must_use]
    pub fn own_chords(&self, scope: OwnScope) -> Arc<[SourceChord]> {
        let epoch = self.cur.song_doc.sync_epoch();
        let mut cache = self.cur.peph.own_chords_cache.borrow_mut();
        if !cache.built || cache.epoch != epoch {
            cache.memo.sweep();
            cache.fresh.clear();
            (cache.epoch, cache.built) = (epoch, true);
        }
        if let Some(own) = cache.fresh.get(&scope) {
            return Arc::clone(own);
        }
        let own = cache.memo.own_chords(self.cur.song_doc.song(), scope);
        cache.fresh.insert(scope, Arc::clone(&own));
        own
    }

    /// トラック `track_id` のクリップ `clip_id` (アレンジ / ランチャーのセル) の鳴る鍵盤の読み口。
    /// クリップが無ければ `None`。毎フレーム呼んでよい (自分の和音は世代キャッシュから引く)。
    #[must_use]
    pub fn clip_sounding_keys(&self, track_id: u32, clip_id: u32) -> Option<ClipSoundingKeys<'_>> {
        let song = self.cur.song_doc.song();
        let clip = song.track_by_id(track_id)?.clip_by_id(clip_id)?;
        Some(ClipSoundingKeys::of(song, track_id, clip, &clip.chord_basis.notes, |scope| self.own_chords(scope)))
    }

    /// トラック `track_id` のオフラインの読み口 (VOICEVOX 歌唱 / 口パク、`common::transpose::TrackTranspose`)。
    /// 自分の和音は世代キャッシュから引く。
    #[must_use]
    pub fn track_transpose(&self, track_id: u32) -> common::transpose::TrackTranspose<'_> {
        common::transpose::TrackTranspose::with_own(self.cur.song_doc.song(), track_id, |scope| self.own_chords(scope))
    }
}
