//! `Ui::waveform` が `Clone`/`PartialEq`/`Hash`/`Default` 不要の Model に対して、
//! `SampleSlices` の **3 variant すべて** (Mono / Planar / Interleaved) でコンパイルする
//! ことを確認する。
//!
//! 「波形ウィジェット固有の不変条件」 (`docs/plan.md` 末尾):
//! - `WaveformSource` は借用のみ
//! - `samples: &[f32]` の Clone は禁止
//! - 再構築判定は `generation: u64` のみ
//! を CI レベルで固定する役目。

use daw_ui_core::{
    PointerFrame, SampleSlices, UiHost, WaveformSource, WaveformStyle, WaveformView,
};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Rect, Scene};

// 意図的に derive マクロを一切付けない non-Clone Model。
// 録音中の追記をシミュレートするフィールド (valid_len) も持つ。
struct Model {
    mono: Vec<f32>,
    left: Vec<f32>,
    right: Vec<f32>,
    interleaved: Vec<f32>, // L0 R0 L1 R1 ...
    valid_len: usize,
    generation: u64,
}

fn main() {
    let mut host: UiHost<Model> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let model = Model {
        mono: vec![0.0; 1024],
        left: vec![0.0; 1024],
        right: vec![0.0; 1024],
        interleaved: vec![0.0; 2048],
        valid_len: 1024,
        generation: 0,
    };
    let screen = PhysicalSize { width: 800, height: 600 };
    let rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 200.0 };

    let _edits = host.frame_to_edits(
        &model,
        &mut scene,
        screen,
        daw_ui_core::FrameInput::default(),
        |m, ui| {
            // Mono: 1ch スライスを直接借用
            let _ = ui.waveform(
                "mono",
                rect,
                WaveformSource {
                    samples: SampleSlices::Mono(&m.mono),
                    valid_len: m.valid_len,
                    generation: m.generation,
                    sample_rate: 48000,
                },
                WaveformView {
                    start_sample: 0,
                    len_samples: m.valid_len as u64,
                    vertical_gain: 1.0,
                },
                WaveformStyle::default(),
            );

            // Planar: チャンネル別スライスの参照配列
            let planes: [&[f32]; 2] = [&m.left, &m.right];
            let _ = ui.waveform(
                "planar",
                rect,
                WaveformSource {
                    samples: SampleSlices::Planar(&planes),
                    valid_len: m.valid_len,
                    generation: m.generation,
                    sample_rate: 48000,
                },
                WaveformView {
                    start_sample: 0,
                    len_samples: m.valid_len as u64,
                    vertical_gain: 1.0,
                },
                WaveformStyle::default(),
            );

            // Interleaved: フレームごとに channels 個ずつ並ぶ
            let _ = ui.waveform(
                "interleaved",
                rect,
                WaveformSource {
                    samples: SampleSlices::Interleaved {
                        data: &m.interleaved,
                        channels: 2,
                    },
                    valid_len: m.valid_len,
                    generation: m.generation,
                    sample_rate: 48000,
                },
                WaveformView {
                    start_sample: 0,
                    len_samples: m.valid_len as u64,
                    vertical_gain: 1.0,
                },
                WaveformStyle::default(),
            );
        },
    );
}
