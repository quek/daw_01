//! コードトラック (r.md #157、`docs/plan_chord_track.md`) のイベント 3 族。
//!
//! - [`ChordTrackEvent`] (`AppEvent::Chord`): 帯の上の箱の編集。
//! - [`ChordFollowEvent`] (`AppEvent::ChordFollow`): トラックの追従の設定。
//! - [`ChordToolEvent`] (`AppEvent::ChordTool`): 検出 / 変換 / 取り込み。
//!
//! どれも serde で読める (headless の `daw.chordEvent(json)` が externally tagged 形のまま通す —
//! variant を足せば script からも撃てる)。適用は `handler::chord_event` / `chord_follow_event` /
//! `chord_tool_event`。

use common::model::{AudioChordSpan, AudioSourceId, Chord, ChordEvent, ChordFollow};
use serde::{Deserialize, Serialize};

/// 帯の上の箱の編集。位置と長さは widget で snap 済みの拍。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ChordTrackEvent {
    /// 箱を置く (上書き規則: かかった既存の箱は削られる)。
    Place { start: f64, len: f64, chord: Chord },
    /// 箱の列を `at` 基準で置く (貼り付け / 検出結果の取り込み。`chords` の位置は `at` からの相対、id は振り直す)。
    PlaceMany { at: f64, chords: Vec<ChordEvent> },
    /// 選んだ箱をまとめて `delta` 拍ずらす。
    Move { ids: Vec<u32>, delta: f64 },
    /// 箱 1 つの範囲を変える (端のドラッグ)。
    Resize { id: u32, start: f64, len: f64 },
    /// 箱の範囲をまとめて変える `(id, start, len)` (境界の連動ドラッグ / Alt の比例伸縮)。1 undo step。
    SetSpans { spans: Vec<(u32, f64, f64)> },
    /// 選んだ箱を `delta` 拍ずらした位置に複製する。
    Duplicate { ids: Vec<u32>, delta: f64 },
    Delete { ids: Vec<u32> },
    /// 選んだ箱のコードを差し替える。
    SetChord { ids: Vec<u32>, chord: Chord },
    /// 選んだ箱 (空なら全部) のコードを移調する。
    Transpose { ids: Vec<u32>, semitones: i32 },
    /// 選んだ箱の色 (`None` = テーマの既定色)。
    SetColor { ids: Vec<u32>, color: Option<[f32; 3]> },
    /// 箱を全部消す。
    Clear,
}

impl ChordTrackEvent {
    /// 編集履歴のラベル (`AppEvent::undo_label` から委譲される)。
    #[must_use]
    pub fn undo_label(&self) -> &'static str {
        match self {
            Self::Place { .. } => "コード作成",
            Self::PlaceMany { .. } => "コード貼り付け",
            Self::Move { .. } => "コード移動",
            Self::Resize { .. } | Self::SetSpans { .. } => "コード長さ変更",
            Self::Duplicate { .. } => "コード複製",
            Self::Delete { .. } => "コード削除",
            Self::SetChord { .. } => "コード変更",
            Self::Transpose { .. } => "コード移調",
            Self::SetColor { .. } => "コード色変更",
            Self::Clear => "コードを全部削除",
        }
    }
}

/// トラックの追従の設定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ChordFollowEvent {
    /// 曲全体のスイッチ (`Song::chord_follow_enabled`)。
    SetEnabled(bool),
    /// トラック `track_ids` の追従モード。
    SetTracks { track_ids: Vec<u32>, mode: ChordFollow },
}

impl ChordFollowEvent {
    #[must_use]
    pub fn undo_label(&self) -> &'static str {
        match self {
            Self::SetEnabled(_) => "コードへの追従を切替",
            Self::SetTracks { .. } => "コードへの追従モード変更",
        }
    }
}

/// 検出 / 変換 / 取り込み。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ChordToolEvent {
    /// オーディオ音源から検出したコードを保存する (空なら消す)。
    SetAudioChords { source_id: AudioSourceId, spans: Vec<AudioChordSpan> },
    /// コードトラックの箱 `chord_ids` (空なら全部) をトラック `track_id` の MIDI クリップにする
    /// (帯 → トラックのドラッグ / Chords to MIDI)。
    ChordsToClip { chord_ids: Vec<u32>, track_id: u32 },
    /// MIDI クリップ `clips` (`(track id, clip id)`) のノートからコードを検出して帯に置く
    /// (クリップ → 帯のドラッグ / コードを抽出)。`window_beats` は 1 コードとして見る長さ。
    ExtractFromClips { clips: Vec<(u32, u32)>, window_beats: f64 },
}

impl ChordToolEvent {
    #[must_use]
    pub fn undo_label(&self) -> &'static str {
        match self {
            Self::SetAudioChords { .. } => "オーディオのコード検出",
            Self::ChordsToClip { .. } => "コードをノートに変換",
            Self::ExtractFromClips { .. } => "コードを抽出",
        }
    }
}
