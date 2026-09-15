//! 発音台帳 — sequencer が note-on を出した note と、そのとき振った **voice id** (プラグインへ
//! 渡す `note_id`) / 送った鍵盤を持つ (r.md #132 / #130)。
//!
//! # なぜ台帳が id を振るのか
//!
//! note の住所は `(Clip::id, Note::id)` の組 (どちらも単調増加の u32、再利用しない) だが、
//! プラグインへ渡す `note_id` は `i32` の非負 (CLAP `clap_event_note.note_id` / VST3 `noteId`)、
//! しかも talk の high band (`TALK_EVENT_ID_BASE = 1 << 28`) より下に収める必要がある。 組を
//! 28 bit へ畳む導出 ([`common::plugin_metadata::sing_note_id`]) は、グリッド分割のように id を
//! 大量に使う編集を undo と繰り返すと 1 content の累積採番が基数 (16384) を超え、**同時に鳴って
//! いる 2 音が同じ `note_id` になる** (和音の片方の note-off がもう片方も止める / per-note
//! modulation が両方に掛かる)。 どう畳んでも入力空間の方が広いので、導出だけでは衝突を
//! 無くせない。
//!
//! そこで CLAP の想定どおり **host が note-on ごとに voice id を振る**。 既定値は
//! `sing_note_id` (普段は daw_gui の VOICEVOX メタデータの鍵と同じ値) で、**その On の時刻に
//! 使われている id と重なるときだけ次の空き id へずらす**。 「使われている」には鳴っている音に
//! 加えて、**同じ窓で止めたが Off がその On より後の frame に出る音** も入る — 台帳から外すのは
//! 走査の順 (content の並び) で、Off の時刻順ではない。 窓の後ろで終わる音の id を窓の前で始まる音に
//! 渡すと、プラグインは Off まで 2 つの voice を同じ id で持ち、Off が両方に当たる。
//!
//! # なぜ鍵盤は送った値を持つのか
//!
//! off は note の住所で台帳を引き、note-on で振った id と **送った鍵盤** (= `pitch` + 曲の移調量、
//! r.md #130) で出す。 off をその時点の `note.pitch` / 移調量から計算し直すと、鳴っている間に
//! 音程や移調が変わったとき別の鍵盤を止めにいき、旧鍵盤が停止まで残る。
//!
//! # 窓ごとの突き合わせ
//!
//! 台帳の発音は、**鳴らした note がまだ今の Song で鳴るべきか**を毎窓確かめないと止まらない。
//! off は「note の終端が窓に入った」ときにしか出ないので、鳴っている間に終端がもう過ぎた位置へ
//! 動く編集 (E / Shift+E の分割で前半の片が短くなる / ノートを短くする / 消す / ミュートする /
//! clip を trim で窓の外へ出す / 後ろへ動かす) の後は、その off が二度と来ない。
//!
//! そこで sequencer は窓の頭で [`NoteLedger::begin_window`] を呼んで印を落とし、窓の中で鳴り続ける
//! と確かめた note に印を付け ([`NoteLedger::keep`]、新しい発音は [`NoteLedger::note_on`] が付ける)、
//! 窓の終わりに印の無い発音を [`NoteLedger::release_unseen`] で止める。 Song を舐め直すのではなく、
//! 窓に掛かる note を索引で引く既存の走査のついでに印を付けるだけなので、追加の走査は台帳の中だけ。
//!
//! RT 安全: 台帳は再生前に確保した容量の中でだけ push / swap_remove / retain する (確保・解放なし)。
//! id の空き探しと住所の照合は鳴っている数 (≤ 容量) で有界。

use common::plugin_metadata::{TALK_EVENT_ID_BASE, sing_note_id};
use common::process_data::MAX_EVENTS;

/// 1 トラックで同時に追跡する note の上限。 1 buffer の MIDI バス (`MAX_EVENTS`) と同じ量で、
/// これを超える note-on は出さない (追跡できない音は止められない)。
pub const MAX_SOUNDING: usize = MAX_EVENTS;

/// 鳴っている note 1 本。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoundingNote {
    /// note-on で振った voice id (= プラグインへ渡した `note_id`)。
    pub voice_id: u32,
    /// note-on で送った鍵盤 (移調込み)。
    pub key: u8,
    /// 発音元の `Clip::id`。
    pub clip_id: u32,
    /// 発音元の `Note::id`。
    pub note_id: u32,
}

#[derive(Debug)]
struct Entry {
    note: SoundingNote,
    /// 今の窓で「まだ鳴るべき」と確かめた印 ([`NoteLedger::begin_window`] で落とす)。
    seen: bool,
}

/// 同じ窓で台帳から外した発音の `(voice id, Off の frame)`。 その frame より前に始まる On には
/// この id を振らない (module doc「なぜ台帳が id を振るのか」)。
#[derive(Debug, Clone, Copy)]
struct Released {
    voice_id: u32,
    off_time: u32,
}

/// 発音台帳 (トラックごとに 1 つ、`PerTrackState` が持つ)。 作った時点で [`MAX_SOUNDING`] 本分を
/// 確保するので、audio thread で push しても再確保しない。
#[derive(Debug)]
pub struct NoteLedger {
    sounding: Vec<Entry>,
    /// 今の窓で [`Self::take`] した発音 ([`Self::begin_window`] で空にする)。 外すたびに Off を 1 本
    /// MIDI バスへ積むので、1 窓の件数はバスの容量 (`MAX_EVENTS`) を超えない。
    released: Vec<Released>,
}

impl Default for NoteLedger {
    fn default() -> Self {
        Self { sounding: Vec::with_capacity(MAX_SOUNDING), released: Vec::with_capacity(MAX_EVENTS) }
    }
}

impl NoteLedger {
    /// これ以上 note-on を追跡できないか。
    pub fn is_full(&self) -> bool {
        self.sounding.len() >= MAX_SOUNDING
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.sounding.is_empty()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.sounding.len()
    }

    fn position(&self, clip_id: u32, note_id: u32) -> Option<usize> {
        self.sounding.iter().position(|e| e.note.clip_id == clip_id && e.note.note_id == note_id)
    }

    /// `(clip_id, note_id)` の note の発音 (鳴っていなければ `None`)。
    pub fn get(&self, clip_id: u32, note_id: u32) -> Option<SoundingNote> {
        self.position(clip_id, note_id).map(|i| self.sounding[i].note)
    }

    /// buffer 内 frame `time` の note-on を記録し、振った voice id を返す。 満杯なら記録せず `None`
    /// (呼び出し側は note-on を出さない)。 voice id は鳴っている音とも、この窓で外したが Off が `time`
    /// より後に出る音とも重ならない (同じ frame の Off は On より先に並ぶので再利用してよい)。 記録した
    /// 発音は今の窓で確かめ済みとして印を持つ。
    pub fn note_on(&mut self, clip_id: u32, note_id: u32, key: u8, time: u32) -> Option<u32> {
        if self.is_full() {
            return None;
        }
        let in_use = |v: u32| {
            self.sounding.iter().any(|e| e.note.voice_id == v)
                || self.released.iter().any(|r| r.voice_id == v && r.off_time > time)
        };
        let mut voice_id = sing_note_id(clip_id, note_id);
        // 空きは (鳴っている数 + 外した数) + 1 回以内に必ず見つかる (band は容量よりずっと広い)。
        while in_use(voice_id) {
            voice_id = (voice_id + 1) % TALK_EVENT_ID_BASE;
        }
        self.sounding.push(Entry { note: SoundingNote { voice_id, key, clip_id, note_id }, seen: true });
        Some(voice_id)
    }

    /// `(clip_id, note_id)` の note を台帳から外して返す (buffer 内 frame `off_time` に note-off を出すとき)。
    pub fn take(&mut self, clip_id: u32, note_id: u32, off_time: u32) -> Option<SoundingNote> {
        let pos = self.position(clip_id, note_id)?;
        let note = self.sounding.swap_remove(pos).note;
        // 容量は 1 窓の Off の数の上限 (`released` の doc)。 越えることは無いが、RT で再確保しないよう守る。
        if self.released.len() < self.released.capacity() {
            self.released.push(Released { voice_id: note.voice_id, off_time });
        }
        Some(note)
    }

    /// 窓の頭: 全発音の印を落とし、前の窓で外した発音を忘れる (その Off はどれもこの窓より前の frame)。
    /// module doc「窓ごとの突き合わせ」。
    pub fn begin_window(&mut self) {
        for e in &mut self.sounding {
            e.seen = false;
        }
        self.released.clear();
    }

    /// `(clip_id, note_id)` の発音がこの窓でも鳴り続けると確かめた印を付ける。
    pub fn keep(&mut self, clip_id: u32, note_id: u32) {
        if let Some(i) = self.position(clip_id, note_id) {
            self.sounding[i].seen = true;
        }
    }

    /// 窓の終わり: 印の無い発音を `release` に渡し、`true` を返したもの (off を**窓の先頭の frame** に
    /// 出せたもの) を台帳から外す。 `false` (バスが満杯で off を出せなかった) は残す — 握りつぶして外すと
    /// stuck note になるので、次の窓 / 停止の一括消音に拾わせる。 Off は窓のどの On よりも前 (か同じ
    /// frame) なので、外した id は予約しない。
    pub fn release_unseen(&mut self, mut release: impl FnMut(SoundingNote) -> bool) {
        self.sounding.retain(|e| e.seen || !release(e.note));
    }

    /// 鳴っている全 note (区間の切れ目 / 停止で全部止めるとき)。
    pub fn iter(&self) -> impl Iterator<Item = &SoundingNote> {
        self.sounding.iter().map(|e| &e.note)
    }

    /// 全部止めた後 (区間の切れ目 / 停止 / seek)。 その Off は次の区間 / buffer のどの On よりも前か
    /// 同じ frame なので、外した id も予約しない。
    pub fn clear(&mut self) {
        self.sounding.clear();
        self.released.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::plugin_metadata::MAX_NOTES_PER_CLIP;

    /// 畳み込みで同じ既定 id になる 2 音 (`note.id` が基数ぶん離れている) が同時に鳴っても、
    /// 別々の voice id になり、off はそれぞれの id と鍵盤で出る。
    #[test]
    fn 畳み込みで同じ既定_id_になる和音でも_voice_id_は重ならない() {
        let mut ledger = NoteLedger::default();
        let low = ledger.note_on(3, 5, 60, 0).expect("on");
        let high = ledger.note_on(3, 5 + MAX_NOTES_PER_CLIP, 64, 0).expect("on");
        assert_eq!(sing_note_id(3, 5), sing_note_id(3, 5 + MAX_NOTES_PER_CLIP), "前提: 既定 id は衝突する");
        assert_ne!(low, high);
        assert_eq!(low, sing_note_id(3, 5), "重ならなければ既定 id のまま");

        let off = ledger.take(3, 5 + MAX_NOTES_PER_CLIP, 100).expect("鳴っている");
        assert_eq!((off.voice_id, off.key), (high, 64));
        assert!(ledger.get(3, 5).is_some(), "もう片方は鳴ったまま");
        // Off (frame 100) より前に始まる On には、同じ窓の中ではまだ渡さない。 同じ frame 以降なら渡す。
        assert_ne!(ledger.note_on(3, 5 + 3 * MAX_NOTES_PER_CLIP, 69, 50), Some(high));
        assert_eq!(ledger.note_on(3, 5 + 2 * MAX_NOTES_PER_CLIP, 67, 100), Some(high));
    }

    /// 上限を超える note-on は記録しない (確保済みの容量の中だけで動く)。
    #[test]
    fn 満杯の台帳は_note_on_を記録しない() {
        let mut ledger = NoteLedger::default();
        let cap = ledger.sounding.capacity();
        for i in 0..MAX_SOUNDING {
            assert!(ledger.note_on(1, i as u32 + 1, 60, 0).is_some());
        }
        assert_eq!(ledger.note_on(1, 99_999, 60, 0), None);
        assert_eq!(ledger.sounding.capacity(), cap, "再確保していない");
    }
}
