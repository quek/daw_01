//! `Ui::heavy` / `HeavyCtx` が non-Clone Model でコンパイル可能なことを担保する
//! (M5 Phase 13)。
//!
//! `HeavyCtx::cached` の viewport_key に Hash 制約だけを要求し、Clone / PartialEq /
//! Default は要求しないことを固定する。`hctx.push_rect` / `push_edit` / `pointer` /
//! delegate (label_at / button_at / waveform) も同様に non-Clone Model で動作する。

use daw_ui_core::{
    ChannelLayout, Edit, FrameInput, SampleSlices, UiHost, WaveformRenderMode, WaveformSource,
    WaveformStyle, WaveformView,
};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, Rect, RectCommand, Scene};

// 意図的に derive を一切付けない non-Clone Model。
struct Model {
    selected_note: Option<u32>,
    notes: Vec<u32>,
    samples: Vec<f32>,
    generation: u64,
    view_start: u64,
    view_len: u64,
}

fn main() {
    let mut host: UiHost<Model> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let mut model = Model {
        selected_note: None,
        notes: vec![1, 2, 3],
        samples: vec![0.0; 1024],
        generation: 0,
        view_start: 0,
        view_len: 1024,
    };

    let edits = host.frame_to_edits(
        &model,
        &mut scene,
        PhysicalSize { width: 800, height: 600 },
        FrameInput::default(),
        |m, ui| {
            ui.heavy("notes", |hctx| {
                // viewport_key は (Hash, Hash, ...) tuple — Clone は要求されない。
                let viewport_key = (m.notes.len(), m.selected_note);
                hctx.cached(viewport_key, |hctx| {
                    for &note in &m.notes {
                        hctx.push_rect(RectCommand {
                            rect: Rect {
                                x: f32::from(note as u16) * 10.0,
                                y: 0.0,
                                w: 8.0,
                                h: 20.0,
                            },
                            fill: Color::rgb(0.5, 0.5, 0.5),
                            border: Color::TRANSPARENT,
                            border_width: 0.0,
                            radius: [0.0; 4],
                        });
                    }
                });
                // delegate (label_at / button_at) も呼べる。
                hctx.label_at(
                    "heavy_label",
                    "heavy demo",
                    10.0,
                    100.0,
                    14.0,
                    Color::rgb(0.9, 0.9, 0.9),
                );
                hctx.button_at(
                    "heavy_btn",
                    "click",
                    Rect { x: 0.0, y: 120.0, w: 60.0, h: 24.0 },
                    || Edit::mutate(|m: &mut Model| m.selected_note = Some(0)),
                );
                // ヒットテスト経路 (cached の外)。
                if hctx.pointer().primary_just_released
                    && let Some((px, _)) = hctx.pointer().pos
                {
                    let idx = (px / 10.0) as u32;
                    hctx.push_edit(Edit::mutate(move |m: &mut Model| {
                        m.selected_note = Some(idx);
                    }));
                }
                // waveform delegate (heavy 内で複数クリップ波形を描く想定)。
                let _ = hctx.waveform(
                    "heavy_wf",
                    Rect { x: 0.0, y: 200.0, w: 200.0, h: 100.0 },
                    WaveformSource {
                        samples: SampleSlices::Mono(&m.samples),
                        valid_len: m.samples.len(),
                        generation: m.generation,
                        sample_rate: 48_000,
                    },
                    WaveformView {
                        start_sample: m.view_start,
                        len_samples: m.view_len,
                        vertical_gain: 1.0,
                    },
                    WaveformStyle {
                        channel_layout: ChannelLayout::Stack,
                        render_mode: WaveformRenderMode::PeakLines,
                        ..WaveformStyle::default()
                    },
                );
            });
        },
    );

    for e in edits {
        e.apply(&mut model);
    }
}
