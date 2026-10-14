//! transport の観測値 (再生位置 / 走行中か / count-in の残り / 録音してよいか) を GUI へ publish する口。
//!
//! 先読み (`crate::output_render`) のときは描画スレッドが [`RENDER_AHEAD_PERIODS`] 回分先を描いているので、
//! **描いた回** ではなく **今デバイスへ渡している回** の値を出す (= その回数ぶん遅らせる)。GUI の再生位置・
//! 映像プレビュー・字幕・MIDI 録音の位置合わせはどれもこの値から決まるので、聞こえている音と揃う。
//! 先読みしないとき (コールバックの中で描く) は描いた回の値をそのまま出す (= 今までと同じ)。

use common::audio_bridge::{PLAYHEAD_UNSET, ProjectTelemetry};

use crate::output_render::RENDER_AHEAD_PERIODS;

/// 1 回分の transport の観測値。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransportSnapshot {
    pub playhead: u64,
    /// `playhead` の位置の曲の拍 (r.md #147 — engine が描画に使った拍。GUI はこれを再生位置に出す)。
    pub beat: f64,
    pub playing: bool,
    pub preroll: u64,
    pub recording_live: bool,
}

impl TransportSnapshot {
    /// まだ 1 回も描いていない (GUI には「再生位置が無い」と見える)。
    const UNSET: Self =
        Self { playhead: PLAYHEAD_UNSET, beat: 0.0, playing: false, preroll: 0, recording_live: false };

    fn publish(&self, slot: &ProjectTelemetry) {
        slot.set_playing(self.playing);
        slot.set_recording_live(self.recording_live);
        slot.set_preroll_remaining(self.preroll);
        slot.set_playhead_beat(self.beat);
        slot.set_playhead_samples(self.playhead);
    }
}

/// project ごとの publish の口。描きながら [`Self::now`] を埋め、回の終わりに [`Self::publish`] する。
pub struct TransportOut {
    /// この回の値。
    pub now: TransportSnapshot,
    /// 先読みの回数ぶんの遅延線。`hist[next]` が最も古い (= 今デバイスへ渡している回)。
    hist: [TransportSnapshot; RENDER_AHEAD_PERIODS],
    next: usize,
}

impl Default for TransportOut {
    fn default() -> Self {
        Self { now: TransportSnapshot::UNSET, hist: [TransportSnapshot::UNSET; RENDER_AHEAD_PERIODS], next: 0 }
    }
}

impl TransportOut {
    /// この回の値を遅延線へ入れ、`ahead` なら [`RENDER_AHEAD_PERIODS`] 回前の値を、そうでなければこの回の値を出す。
    ///
    /// 先読みしない間も遅延線を今の値で埋めておく — 切り替え (停止中だけ) の直後に、止まる前の古い位置を出さない。
    pub fn publish(&mut self, slot: &ProjectTelemetry, ahead: bool) {
        let out = if ahead {
            let oldest = std::mem::replace(&mut self.hist[self.next], self.now);
            self.next = (self.next + 1) % RENDER_AHEAD_PERIODS;
            oldest
        } else {
            self.hist = [self.now; RENDER_AHEAD_PERIODS];
            self.now
        };
        out.publish(slot);
    }
}
