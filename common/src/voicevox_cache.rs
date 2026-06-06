//! VOICEVOX 合成結果の in-memory キャッシュ。
//!
//! Synth ボタン押下のたびに全 Vocal clip を再合成すると遅い (1 音 約 4 秒、
//! 10 音で 40 秒)。 Clip 内容 + singer_id を hash した key で cache し、
//! 同 key で 2 回目以降は HTTP call をスキップして即返す。
//!
//! Key は clip 内の note (start_beat / duration_beats / pitch / velocity /
//! lyric) を sort してから hash + singer_id で混ぜる。 clip 自体の id は
//! 含めない (clip を別 track にコピーしても cache hit して欲しい)。
//!
//! 永続化は別 phase。 現状は process lifetime のみ有効。

use std::collections::HashMap;
use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::model::Note;

pub type CacheKey = u64;

/// In-memory cache のエントリ数上限。 超えたら FIFO で最古を捨てる。
/// 1 clip 約 数百 KB〜数 MB なので、 無制限成長を防ぐための緩い上限。
const MAX_CACHE_ENTRIES: usize = 256;

/// 合成結果 1 clip 分。
#[derive(Debug, Clone)]
pub struct CachedClip {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

/// In-memory cache。 キー衝突確率は SipHash 64-bit で実用上無視可能。
///
/// エントリ数は `MAX_CACHE_ENTRIES` を上限とし、 超えると挿入順 (FIFO) の
/// 最古を退避して無制限成長を防ぐ。 `order` は挿入順を保つ key の列で、
/// `map` と整合を保つ。
#[derive(Debug, Default)]
pub struct VoiceVoxCache {
    map: HashMap<CacheKey, CachedClip>,
    order: VecDeque<CacheKey>,
}

impl VoiceVoxCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn get(&self, key: CacheKey) -> Option<&CachedClip> {
        self.map.get(&key)
    }

    pub fn insert(&mut self, key: CacheKey, value: CachedClip) {
        // 既存 key の更新ならエントリ数は増えないので order を触らない。
        let is_new = self.map.insert(key, value).is_none();
        if is_new {
            self.order.push_back(key);
            // 上限超過分を FIFO (front = 最古) で退避。 通常 1 件だが、
            // 万一複数溜まっても整合するよう while で回す。
            while self.order.len() > MAX_CACHE_ENTRIES {
                if let Some(oldest) = self.order.pop_front() {
                    self.map.remove(&oldest);
                }
            }
        }
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    /// `(notes, singer_id)` から安定 hash を計算。 同じ内容 (notes が
    /// 同じ start/duration/pitch/velocity/lyric) で同じ singer なら同じ key。
    /// notes の順序は start_beat で sort してから hash するので、 編集中に
    /// 順序が変わっても hash は不変。
    ///
    /// v6 の linked clip 対応で `Clip` から直接 notes を取らない。 caller が
    /// `Song.clip_contents[clip.content_id].notes` を取り出して渡す。 これで
    /// 共有 clip / 独立コピーの区別なく「同じ notes 内容」 なら同 cache key
    /// になる (独立コピーでも内容が同じなら 1 回合成で済む)。
    pub fn key_for_notes(notes: &[Note], singer_id: u32) -> CacheKey {
        let mut sorted: Vec<&Note> = notes.iter().collect();
        sorted.sort_by(|a, b| {
            a.start_beat
                .partial_cmp(&b.start_beat)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.pitch.cmp(&b.pitch))
        });
        let mut h = DefaultHasher::new();
        // 個数を先に hash (notes 0 と 1 で衝突しないよう)
        sorted.len().hash(&mut h);
        for n in sorted {
            n.start_beat.to_bits().hash(&mut h);
            n.duration_beats.to_bits().hash(&mut h);
            n.pitch.hash(&mut h);
            n.velocity.hash(&mut h);
            n.lyric.hash(&mut h);
        }
        singer_id.hash(&mut h);
        h.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(start: f64, dur: f64, pitch: u8, lyric: Option<&str>) -> Note {
        Note {
            start_beat: start,
            duration_beats: dur,
            pitch,
            velocity: 100,
            lyric: lyric.map(|s| s.to_string()),
        }
    }

    #[test]
    fn key_is_stable_for_identical_notes() {
        let a = vec![note(0.0, 1.0, 60, Some("ら"))];
        let b = vec![note(0.0, 1.0, 60, Some("ら"))];
        assert_eq!(
            VoiceVoxCache::key_for_notes(&a, 6000),
            VoiceVoxCache::key_for_notes(&b, 6000)
        );
    }

    #[test]
    fn key_differs_when_singer_changes() {
        let c = vec![note(0.0, 1.0, 60, Some("ら"))];
        assert_ne!(
            VoiceVoxCache::key_for_notes(&c, 6000),
            VoiceVoxCache::key_for_notes(&c, 3001)
        );
    }

    #[test]
    fn key_differs_when_lyric_changes() {
        let a = vec![note(0.0, 1.0, 60, Some("ら"))];
        let b = vec![note(0.0, 1.0, 60, Some("り"))];
        assert_ne!(
            VoiceVoxCache::key_for_notes(&a, 6000),
            VoiceVoxCache::key_for_notes(&b, 6000)
        );
    }

    #[test]
    fn key_stable_under_note_reorder() {
        // start_beat 順に sort される ⇒ 入力順に依存しない
        let a = vec![
            note(0.0, 1.0, 60, Some("あ")),
            note(1.0, 1.0, 62, Some("い")),
        ];
        let b = vec![
            note(1.0, 1.0, 62, Some("い")),
            note(0.0, 1.0, 60, Some("あ")),
        ];
        assert_eq!(
            VoiceVoxCache::key_for_notes(&a, 6000),
            VoiceVoxCache::key_for_notes(&b, 6000)
        );
    }

    #[test]
    fn cache_get_set_roundtrip() {
        let mut c = VoiceVoxCache::new();
        let key = 42u64;
        c.insert(
            key,
            CachedClip { samples: vec![0.5; 100], sample_rate: 48000 },
        );
        let got = c.get(key).unwrap();
        assert_eq!(got.samples.len(), 100);
        assert_eq!(got.sample_rate, 48000);
    }
}
