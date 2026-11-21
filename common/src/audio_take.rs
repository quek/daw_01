//! オーディオ入力の録音 (r.md #156) の wire 型。
//!
//! 音そのものは wire を渡らない (不変条件 2): GUI が書き先のパスを渡し ([`AudioTakeSpec`])、
//! engine が WAV を書き、終わったら「どのトラックの・どの拍から・何フレーム書いたか」を返す
//! ([`AudioTakesFinished`])。

use std::path::PathBuf;

use bincode::{Decode, Encode};

use crate::model::TrackInput;

/// 録音するトラック 1 本ぶんの指示。
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct AudioTakeSpec {
    pub track_id: u32,
    /// 取り込む入力 (録音を始めた時点のトラックの入力。録音中に入力を替えても、この録音は替わらない)。
    pub input: TrackInput,
    /// 書き先の WAV (絶対パス。32-bit float、デバイスのサンプルレート)。engine が作る。
    pub path: PathBuf,
}

/// 録音 1 回 (同時に録るトラックの組) の指示。
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct AudioTakeRequest {
    /// GUI が振る通し番号。[`AudioTakesFinished::take_id`] で対応を取る。
    pub take_id: u64,
    pub takes: Vec<AudioTakeSpec>,
}

/// トラック 1 本ぶんの結果。
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct AudioTakeResult {
    pub track_id: u32,
    pub path: PathBuf,
    /// WAV に書けたフレーム数。`0` = 何も録れなかった (ファイルは engine が消してある)。
    pub frames: u64,
    /// 取り込みが間に合わず無音で埋めたフレーム数 (`frames` に含む。位置はずれない)。
    pub dropped_frames: u64,
    /// ファイルを作れなかった / 書けなかった理由。
    pub error: Option<String>,
}

/// 録音の中で、曲の位置が連続している 1 区間。ループの折り返しや seek で区切れる。
#[derive(Debug, Clone, Copy, PartialEq, Encode, Decode)]
pub struct AudioTakeSegment {
    /// WAV の中の開始フレーム。次の区間の `file_frame` (最後の区間は WAV の末尾) までがこの区間。
    pub file_frame: u64,
    /// その音が鳴っていた曲の位置 (拍)。入出力のレイテンシと先読みぶんは engine が補正済み。
    pub start_beat: f64,
}

/// 録音 1 回が終わった (停止 / パンチアウト / count-in の取り消し)。
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct AudioTakesFinished {
    pub take_id: u64,
    /// WAV のサンプルレート。
    pub sample_rate: u32,
    /// 区間 (WAV の先頭から順)。録音が始まる前に取り消されたら空。
    pub segments: Vec<AudioTakeSegment>,
    /// 覚え切れずに捨てた区間の数 (ループを回し続けたとき。捨てた区間の音は直前の区間に続けて入っている)。
    pub segments_dropped: u32,
    pub takes: Vec<AudioTakeResult>,
}
