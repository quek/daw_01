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

/// 録音中の波形の山 1 つが覆うフレーム数 (WAV の先頭から数える)。
pub const TAKE_PEAK_FRAMES: u32 = 512;

/// 録音中の途中経過: レーンに伸びていく波形の材料。音は運ばない — 山の高さだけを、増えた分だけ小刻みに送る
/// (1 トラック 1 秒あたり 100 byte 弱)。
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct AudioTakeProgress {
    pub take_id: u64,
    /// WAV のサンプルレート。
    pub sample_rate: u32,
    /// 前の便から増えた区間 ([`AudioTakesFinished::segments`] と同じ物)。
    pub new_segments: Vec<AudioTakeSegment>,
    pub tracks: Vec<AudioTakePeaks>,
}

/// トラック 1 本ぶんの、前の便から増えた山。
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct AudioTakePeaks {
    pub track_id: u32,
    /// [`TAKE_PEAK_FRAMES`] フレームごとの、全 ch の絶対値の最大 (`0..=1` を `0..=255` に刻んだもの)。
    pub peaks: Vec<u8>,
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
