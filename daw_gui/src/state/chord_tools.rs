//! r.md #157 (`docs/plan_chord_track.md` §3.4): コードの試聴・voicing・検出・録音の、タブごとの状態。
//!
//! どれも **聴き方 / 道具の都合** なので Song に載せない (不変条件 6)。保存するもの (試聴トラック /
//! 「再生中も鳴らす」/ voicing / 検出の設定) は `ViewState.chord_tools` に、その場限りのもの
//! (鳴らしている音・検出の進行・録音中の和音) はここだけに持つ。

use common::model::{AudioChordSpan, AudioSourceId, ChordToolsView};

use crate::state::HeldPreview;

/// コードの試聴で鳴らしている音 (1 コードぶん) と、消音する時刻。
#[derive(Debug, Clone, Default)]
pub struct ChordAuditionNotes {
    pub notes: Vec<HeldPreview>,
    pub due: Option<std::time::Instant>,
}

/// 走っているオーディオのコード検出 1 本 (音源 1 つ)。
#[derive(Debug)]
pub struct AudioDetectJob {
    pub source_id: AudioSourceId,
    /// 終わったら帯にも置くクリップ (`(track id, clip id)`)。空なら保存だけ。
    pub place_on: Vec<(u32, u32)>,
    pub rx: std::sync::mpsc::Receiver<Vec<AudioChordSpan>>,
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
    /// 走っているオーディオのコード検出。
    pub detect_jobs: Vec<AudioDetectJob>,
    /// 追従するオーディオトラックの未検出の音源を最後に探した Song の版 (`SongDoc::edit_epoch`)。
    pub auto_detect_epoch: Option<u64>,
}
