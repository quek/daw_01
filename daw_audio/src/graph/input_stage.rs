//! デバイスの入力をトラックのチェーン入口へ流すための受け皿 (r.md #156)。
//!
//! 描画の文脈 (`RenderCtx`) が指す物は project の持ち物でなければならない — stall の窓の間、呼び側が
//! 戻った後も生き残りの runner が読む (`crate::audio_worker` の module doc)。デバイスの入力は
//! コールバックごとに書き換わるので、描画の前に **project が持つこの受け皿へ写し**
//! ([`InputStage::fill`])、トラックはここから読む ([`InputStage::slot`])。
//!
//! 受け皿は「オーディオ入力のトラック」1 本につき L / R の 2 本。曲の入力の組 ([`InputPlan`]) が
//! 変わったら off-thread で作り直して便で届く (`RtBundle::input_stage`)。buffer をまたぐ状態は
//! 持たないので、差し替えで引き継ぐ物は無い。
//!
//! **書き出しには渡さない** (`NativeIo::default()` の `input` は `None`) — 入力は「聴き方」で、曲ではない。

use common::model::Song;

/// 曲から決まる入力の組: `(track index, L の ch, R の ch)` を track index の昇順に。
pub type InputPlan = Vec<(u32, u16, u16)>;

/// `song` のオーディオ入力のトラックの組 (モニターの on / off や録音待機は含めない — 切り替えるたびに
/// 受け皿を作り直さない)。
#[must_use]
pub fn input_plan(song: &Song) -> InputPlan {
    song.tracks
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let (l, r) = t.input.audio_channels()?;
            Some((u32::try_from(i).ok()?, l, r))
        })
        .collect()
}

/// トラック 1 本ぶんの受け皿。
pub struct InputSlot {
    track_idx: u32,
    left: u16,
    right: u16,
    l: Vec<f32>,
    r: Vec<f32>,
    /// この buffer、入力をチェーンへ流すか (モニターが効いている)。
    pub monitor: bool,
    /// この buffer、入力レベルをトラックのメーターへ出すか (録音待機中でモニターしていない)。
    pub metered: bool,
    pub peak_l: f32,
    pub peak_r: f32,
}

impl InputSlot {
    /// 入力を `l` / `r` へ足す (クリップの音と同じ所)。
    pub fn mix_into(&self, l: &mut [f32], r: &mut [f32]) {
        for (d, s) in l.iter_mut().zip(&self.l) {
            *d += *s;
        }
        for (d, s) in r.iter_mut().zip(&self.r) {
            *d += *s;
        }
    }
}

/// 1 project ぶんの受け皿 (track index の昇順)。
#[derive(Default)]
pub struct InputStage {
    slots: Vec<InputSlot>,
}

fn peak(s: &[f32]) -> f32 {
    s.iter().fold(0.0_f32, |a, &v| a.max(v.abs()))
}

/// `src` (無ければ無音) を `dst[..n]` へ写す。`src` が短ければ残りは無音。
fn stage(dst: &mut [f32], src: Option<&[f32]>, n: usize) {
    let n = n.min(dst.len());
    let k = src.map_or(0, |s| s.len().min(n));
    if let Some(s) = src {
        dst[..k].copy_from_slice(&s[..k]);
    }
    dst[k..n].fill(0.0);
}

impl InputStage {
    /// off-thread で確保する (`max_frames` = 1 buffer の上限)。
    #[must_use]
    pub fn new(plan: &InputPlan, max_frames: usize) -> Self {
        let slots = plan
            .iter()
            .map(|&(track_idx, left, right)| InputSlot {
                track_idx,
                left,
                right,
                l: vec![0.0; max_frames],
                r: vec![0.0; max_frames],
                monitor: false,
                metered: false,
                peak_l: 0.0,
                peak_r: 0.0,
            })
            .collect();
        Self { slots }
    }

    /// この buffer のデバイス入力を写す。`channel(ch)` はデバイスの入力 ch 番号 `ch` の音 (無ければ `None`
    /// = 無音)。モニターも入力メーターも要らないトラックは写さない。確保・ロックなし。
    pub fn fill<'a>(&mut self, song: &Song, frames: usize, channel: impl Fn(u16) -> Option<&'a [f32]>) {
        for slot in &mut self.slots {
            // 曲と受け皿は同じ便で届くが、食い違っていたら流さない (別のトラックへ入力を流さない)。
            let track = song
                .tracks
                .get(slot.track_idx as usize)
                .filter(|t| t.input.audio_channels() == Some((slot.left, slot.right)));
            slot.monitor = track.is_some_and(|t| t.monitors_audio_input());
            slot.metered = track.is_some_and(|t| t.armed) && !slot.monitor;
            if !slot.monitor && !slot.metered {
                continue;
            }
            stage(&mut slot.l, channel(slot.left), frames);
            stage(&mut slot.r, channel(slot.right), frames);
            if slot.metered {
                let n = frames.min(slot.l.len());
                slot.peak_l = peak(&slot.l[..n]);
                slot.peak_r = peak(&slot.r[..n]);
            }
        }
    }

    /// track index `track_idx` の受け皿 (オーディオ入力のトラックでなければ `None`)。
    #[must_use]
    pub fn slot(&self, track_idx: u32) -> Option<&InputSlot> {
        let i = self.slots.binary_search_by_key(&track_idx, |s| s.track_idx).ok()?;
        self.slots.get(i)
    }

    /// 誰かが入力をモニターしているか (この buffer)。
    #[must_use]
    pub fn any_monitoring(&self) -> bool {
        self.slots.iter().any(|s| s.monitor)
    }
}
