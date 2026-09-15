//! 発音台帳 — sequencer が note-on を出した note と、そのとき振った **voice id** (プラグインへ
//! 渡す `note_id`) / 送った鍵盤を持つ (r.md #132)。
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
//! `sing_note_id` (普段は daw_gui の VOICEVOX メタデータの鍵と同じ値) で、**いま鳴っている
//! 音と重なるときだけ次の空き id へずらす**。 off は note の住所で台帳を引き、note-on で振った
//! id と送った鍵盤で出すので、途中で note の音程が変わっても (↑↓ / 別の編集) 旧鍵盤が残らない。
//!
//! RT 安全: 台帳は再生前に確保した容量の中でだけ push / swap_remove する (確保・解放なし)。
//! id の空き探しは鳴っている数 (≤ 容量) で有界。

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
    /// note-on で送った鍵盤。
    pub key: u8,
    /// 発音元の `Clip::id`。
    pub clip_id: u32,
    /// 発音元の `Note::id`。
    pub note_id: u32,
}

/// 発音台帳 (トラックごとに 1 つ、`PerTrackState` が持つ)。 作った時点で [`MAX_SOUNDING`] 本分を
/// 確保するので、audio thread で push しても再確保しない。
#[derive(Debug)]
pub struct NoteLedger {
    sounding: Vec<SoundingNote>,
}

impl Default for NoteLedger {
    fn default() -> Self {
        Self { sounding: Vec::with_capacity(MAX_SOUNDING) }
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

    /// `(clip_id, note_id)` の note が鳴っているか。
    pub fn is_sounding(&self, clip_id: u32, note_id: u32) -> bool {
        self.sounding.iter().any(|s| s.clip_id == clip_id && s.note_id == note_id)
    }

    /// note-on を記録し、振った voice id を返す。 満杯なら記録せず `None` (呼び出し側は
    /// note-on を出さない)。 voice id は鳴っている音と重ならない。
    pub fn note_on(&mut self, clip_id: u32, note_id: u32, key: u8) -> Option<u32> {
        if self.is_full() {
            return None;
        }
        let mut voice_id = sing_note_id(clip_id, note_id);
        // 空きは鳴っている数 + 1 回以内に必ず見つかる (band は容量よりずっと広い)。
        while self.sounding.iter().any(|s| s.voice_id == voice_id) {
            voice_id = (voice_id + 1) % TALK_EVENT_ID_BASE;
        }
        self.sounding.push(SoundingNote { voice_id, key, clip_id, note_id });
        Some(voice_id)
    }

    /// `(clip_id, note_id)` の note を台帳から外して返す (note-off を出すとき)。
    pub fn take(&mut self, clip_id: u32, note_id: u32) -> Option<SoundingNote> {
        let pos = self.sounding.iter().position(|s| s.clip_id == clip_id && s.note_id == note_id)?;
        Some(self.sounding.swap_remove(pos))
    }

    /// 鳴っている全 note (区間の切れ目 / 停止で全部止めるとき)。
    pub fn iter(&self) -> impl Iterator<Item = &SoundingNote> {
        self.sounding.iter()
    }

    pub fn clear(&mut self) {
        self.sounding.clear();
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
        let low = ledger.note_on(3, 5, 60).expect("on");
        let high = ledger.note_on(3, 5 + MAX_NOTES_PER_CLIP, 64).expect("on");
        assert_eq!(sing_note_id(3, 5), sing_note_id(3, 5 + MAX_NOTES_PER_CLIP), "前提: 既定 id は衝突する");
        assert_ne!(low, high);
        assert_eq!(low, sing_note_id(3, 5), "重ならなければ既定 id のまま");

        let off = ledger.take(3, 5 + MAX_NOTES_PER_CLIP).expect("鳴っている");
        assert_eq!((off.voice_id, off.key), (high, 64));
        assert!(ledger.is_sounding(3, 5), "もう片方は鳴ったまま");
        // 鳴り終わった id は次の note-on で使える。
        assert_eq!(ledger.note_on(3, 5 + 2 * MAX_NOTES_PER_CLIP, 67), Some(high));
    }

    /// 上限を超える note-on は記録しない (確保済みの容量の中だけで動く)。
    #[test]
    fn 満杯の台帳は_note_on_を記録しない() {
        let mut ledger = NoteLedger::default();
        let cap = ledger.sounding.capacity();
        for i in 0..MAX_SOUNDING {
            assert!(ledger.note_on(1, i as u32 + 1, 60).is_some());
        }
        assert_eq!(ledger.note_on(1, 99_999, 60), None);
        assert_eq!(ledger.sounding.capacity(), cap, "再確保していない");
    }
}
