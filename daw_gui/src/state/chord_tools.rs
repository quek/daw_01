//! r.md #157 (`docs/plan_chord_track.md` §3.4): コードの試聴・voicing・検出・録音の、タブごとの状態。
//!
//! どれも **聴き方 / 道具の都合** なので Song に載せない (不変条件 6)。保存するもの (試聴トラック /
//! 「再生中も鳴らす」/ voicing / 検出の設定) は `ViewState.chord_tools` に、その場限りのもの
//! (鳴らしている音・検出の進行・録音中の和音) はここだけに持つ。

use std::collections::HashSet;
use std::sync::Arc;

use common::chord_detect::DetectOptions;
use common::model::{AudioChordSpan, AudioSourceId, ChordToolsView};

use crate::audio_source_cache::AudioSourceBuffer;
use crate::chord_audio_detect::DetectGrid;
use crate::state::HeldPreview;

/// コードの試聴で鳴らしている音 (1 コードぶん) と、消音する時刻。
#[derive(Debug, Clone, Default)]
pub struct ChordAuditionNotes {
    pub notes: Vec<HeldPreview>,
    pub due: Option<std::time::Instant>,
}

/// 発注したオーディオのコード検出 1 本 (音源 1 つ)。同時に走らせる数は絞る (`handler::chord_audio`) ので、
/// 順番待ちのものもある。
#[derive(Debug)]
pub struct AudioDetectJob {
    pub source_id: AudioSourceId,
    /// 終わったら帯にも置くクリップ (`(track id, clip id)`)。空なら保存だけ。
    pub place_on: Vec<(u32, u32)>,
    pub run: DetectRun,
}

/// 検出 1 本の進み具合。
#[derive(Debug)]
pub enum DetectRun {
    /// 順番待ち (走らせるのに要るもの一式。発注した時点の音源・格子・設定で測る)。
    Queued { buf: Arc<AudioSourceBuffer>, grid: DetectGrid, opts: DetectOptions },
    /// 走っている (終わると区間が届く)。
    Running(std::sync::mpsc::Receiver<Vec<AudioChordSpan>>),
}

#[derive(Debug, Default)]
pub struct ChordToolsState {
    /// 保存する設定 (試聴トラック / 再生中も鳴らす / voicing / 検出の設定)。
    pub view: ChordToolsView,
    /// いま鳴らしている試聴音。
    pub audition: ChordAuditionNotes,
    /// 直前に試聴で鳴らした鍵盤 (書いた音の空間)。自動の voicing が声部の跳躍を抑える基準。
    pub last_voiced: Vec<u8>,
    /// 帯の録音待機 (session-only。トラックの録音待機と同じく保存しない)。
    pub record_armed: bool,
    /// 発注したオーディオのコード検出 (走っているものと順番待ち)。
    pub detect_jobs: Vec<AudioDetectJob>,
    /// 検出を済ませたのにコードが 1 つも残らなかった音源 (ドラム / 単音 / 検出の thread が落ちた)。
    /// `Song.audio_chords` は空の結果を持たないので、ここで覚えて自動検出を走らせ直さない
    /// (session-only: 開き直したら 1 回測り直す)。
    pub detected_without_chords: HashSet<AudioSourceId>,
    /// 追従するオーディオトラックの未検出の音源を最後に探した Song の版 (`SongDoc::edit_epoch`)。
    pub auto_detect_epoch: Option<u64>,
    /// 直近 engine に送った「再生中も鳴らす」(`AudioCommand::SetChordAudition`) の値。差分送信用で、
    /// `None` = 未送信 (起動直後 / daw_audio の起こし直しの後は必ず送る)。
    pub sent_playback: Option<(Option<u32>, common::model::Voicing)>,
    /// MIDI 鍵盤で弾いている和音 (`handler::chord_capture`)。
    pub capture: ChordCapture,
    /// 直近に鍵盤で弾き終えたコードと通し番号 (Instrument Input: 帯のポップアップは番号が変わったら
    /// 編集中の箱に設定する)。
    pub played: Option<(common::model::Chord, u64)>,
    pub played_seq: u64,
    /// 「コードを抽出…」のダイアログの対象クリップ (`(track id, clip id)`)。`Some` の間開いている。
    pub extract_dialog: Option<Vec<(u32, u32)>>,
}

/// MIDI 鍵盤で弾いた和音をコードにする途中の状態。
#[derive(Debug, Clone, Default)]
pub struct ChordCapture {
    /// 押している鍵。
    pub held: Vec<u8>,
    /// いまの和音で押した鍵 (全部離したら 1 つのコードとして確定する)。
    pub chord_notes: Vec<u8>,
    /// 録音中: いまの和音の頭の拍。
    pub start_beat: Option<f64>,
    /// 録音中: 直前に置いた箱 (次の和音の頭まで伸ばす)。
    pub last_recorded: Option<u32>,
}
