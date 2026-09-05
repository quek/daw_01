//! Global Sampler / MIDI Capture の GUI イベント (`docs/plan_global_sampler.md`)。
//!
//! [`AppEvent::Sampler`](crate::event::AppEvent::Sampler) が包む [`SamplerEvent`]
//! 1 本に集約する。`AppEvent` へ variant を並べないのは `LauncherEvent` と同じ理由
//! (`AppData::handle_event` の巨大 match が budget の天井に張り付いている)。
//! 処理は [`AppData::handle_sampler_event`](crate::state::AppData::handle_sampler_event)。

use crate::app_types::ImportTrackTarget;

#[derive(Debug, Clone, PartialEq)]
pub enum SamplerEvent {
    /// 下部パネルの Sampler タブ (`Some(2)`) を開閉。`ToggleMixerPanel` と同じトグル規則。
    TogglePanel,
    /// MIDI Capture タブ (`Some(3)`) を開閉。
    ToggleMidiCapturePanel,
    /// テレメトリスレッドが 33ms ごとに流す波形バケツ / セグメント。
    Tick(crate::state::sampler::SamplerTick),
    /// 録音源の切替 (リングは新世代で作り直す = 中身は消える)。
    SetSource(common::protocol::SamplerSource),
    /// 溜める長さ (秒)。`commit` で app_config へ保存 + リング再確保。
    SetSeconds { seconds: u32, commit: bool },
    TogglePaused,
    /// 選択範囲 `[start, end)` (リング絶対フレーム)。`None` で解除。
    SetSelection(Option<(u64, u64)>),
    /// 選択範囲の試聴 (再押下で停止)。
    TogglePreview,
    /// 選択範囲をアレンジ / セルへ落とした (WAV に書き出して audio clip にする)。
    Drop {
        start_frame: u64,
        end_frame: u64,
        target: ImportTrackTarget,
        target_beat: Option<f64>,
    },
    /// MIDI 入力ポートに来たノート (midir コールバックで wall-clock を付けたもの)。
    /// `velocity == None` は note-off。既存の `MidiNoteOn/Off` (演奏 / 録音 / binding)
    /// とは独立に **常に** 溜める (Q5)。
    MidiCaptured { at_ns: u64, channel: u8, pitch: u8, velocity: Option<u8> },
    /// MIDI Capture の選択範囲 `[start, end)` (wall-clock ns)。
    SetMidiSelection(Option<(u64, u64)>),
    ToggleMidiPaused,
    /// 選択ノートを cursor track のインストで試聴 (再押下で停止)。
    ToggleMidiPreview,
    /// 選択範囲をアレンジ / セルへ落とした (MIDI clip にする)。
    MidiDrop {
        start_ns: u64,
        end_ns: u64,
        target: ImportTrackTarget,
        target_beat: Option<f64>,
    },
}

impl SamplerEvent {
    /// r.md #29: この event が undo step を積んだときの履歴ラベル
    /// (`AppEvent::undo_label` から委譲)。
    #[must_use]
    pub fn undo_label(&self) -> &'static str {
        match self {
            Self::Drop { .. } => "Sampler から切り出し",
            Self::MidiDrop { .. } => "MIDI Capture から切り出し",
            _ => "編集",
        }
    }
}
