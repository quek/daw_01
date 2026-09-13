//! Arranger セクション ([`Section`]) の型。
//!
//! `Song.sections` として `LoadSong` の wire を渡るので `common/build.rs` の `WIRE_SOURCES` に
//! 登録している (不変条件 7)。帯を動かす / 複製する / 詰めるロジックは wire に載らないので
//! `section_ops.rs` に置く (ロジックの変更で fingerprint を動かさない)。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// Arranger セクション (曲のパート =
/// Intro / Aメロ / サビ …)。全トラックを縦断する時間レンジ + 名前 + 色で、`Song.sections`
/// に保持する。位置 (`start_beat`) が並び順の SSoT (別途 order index は持たない)。
/// `start_beat` 昇順・互いに非交差 (重複なし、隙間は許容) を `Song::normalize_sections`
/// で保つ。帯を動かす / 並べ替えると範囲内の全 clip + automation + tempo + 拍子 + key が
/// 一緒に動く破壊的アレンジャー (Studio One モデル) の位置メタデータ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Section {
    /// Song 内で安定な id (`Song::alloc_section_id` で採番、`0` は sentinel)。
    pub id: u32,
    /// 表示名 (Intro / Aメロ / サビ …)。自動命名 + 自由 rename。
    pub name: String,
    /// 帯の塗り色 (RGB、`0.0..=1.0`)。
    pub color: [f32; 3],
    /// 開始拍 (song-absolute)。
    pub start_beat: f64,
    /// 長さ (拍)。`end = start_beat + len_beats`。
    pub len_beats: f64,
}

impl Section {
    /// 終端拍 (= `start_beat + len_beats`)。
    pub fn end_beat(&self) -> f64 {
        self.start_beat + self.len_beats
    }
}
