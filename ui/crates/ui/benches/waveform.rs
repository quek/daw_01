//! `Ui::waveform` の性能ベンチ。
//!
//! M2 DoD:
//! - LOD 初回構築 (1 widget): 5.76M サンプル (1 分 × 48kHz × stereo) で < 50ms
//! - LOD 再利用 (1 widget, `generation` 一致時): `Ui::waveform` 呼び出し < 100µs
//!
//! DAW は波形を **複数同時表示** するので 1 widget の数値だけでは不十分。
//! N widgets (典型 8〜64) で 1 フレーム内に収まることを確認する。

use std::hint::black_box;
use std::sync::OnceLock;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use daw_ui_core::{
    SampleSlices, UiHost, WaveformSource, WaveformStyle, WaveformView,
};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Rect, Scene};

const SAMPLE_RATE: u32 = 48_000;
const SECONDS: usize = 60;
const FRAME_COUNT: usize = SAMPLE_RATE as usize * SECONDS; // 5_760_000

const SCREEN: PhysicalSize = PhysicalSize { width: 1280, height: 1200 };
const RECT_W: f32 = 1280.0;

fn samples_l() -> &'static [f32] {
    static S: OnceLock<Vec<f32>> = OnceLock::new();
    S.get_or_init(|| {
        (0..FRAME_COUNT)
            .map(|i| (i as f32 * 0.001).sin() * 0.8)
            .collect()
    })
}

fn samples_r() -> &'static [f32] {
    static S: OnceLock<Vec<f32>> = OnceLock::new();
    S.get_or_init(|| {
        (0..FRAME_COUNT)
            .map(|i| (i as f32 * 0.0011).sin() * 0.7)
            .collect()
    })
}

/// 1 frame 内で `n` 個の波形ウィジェットを描く。各ウィジェットは異なる id を持ち、
/// 別ピラミッドキャッシュとして state HashMap に乗る。
fn render_n_waveforms(
    host: &mut UiHost<()>,
    scene: &mut Scene,
    planes: &[&[f32]],
    n: usize,
    generation: u64,
) {
    let _edits = host.frame_to_edits(&(), scene, SCREEN, daw_ui_core::FrameInput::default(), |_, ui| {
        let row_h = 80.0;
        for i in 0..n {
            let rect = Rect {
                x: 0.0,
                y: i as f32 * row_h,
                w: RECT_W,
                h: row_h - 4.0,
            };
            let resp = ui.waveform(
                ("track", i),
                rect,
                WaveformSource {
                    samples: SampleSlices::Planar(planes),
                    valid_len: planes[0].len(),
                    generation,
                    sample_rate: SAMPLE_RATE,
                },
                WaveformView {
                    start_sample: 0,
                    len_samples: planes[0].len() as u64,
                    vertical_gain: 1.0,
                },
                WaveformStyle::default(),
            );
            black_box(resp);
        }
    });
}

/// 各反復で UiHost を新調 → N 個分の LOD ピラミッドを完全再構築する。
/// 1 widget 当たりのコストの線形スケールを確認する。
fn bench_initial_build(c: &mut Criterion) {
    let l = samples_l();
    let r = samples_r();
    let planes: [&[f32]; 2] = [l, r];

    let mut group = c.benchmark_group("waveform/initial_build");
    group.sample_size(20);
    for &n in &[1usize, 8, 16] {
        group.bench_function(format!("{n} widgets × 5.76M samples × 2ch"), |b| {
            b.iter_batched(
                || (UiHost::<()>::no_redraw(), Scene::new()),
                |(mut host, mut scene)| {
                    render_n_waveforms(&mut host, &mut scene, &planes, n, 0);
                    black_box((host, scene));
                },
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

/// `generation` 一致時の呼び出し。LOD ピラミッドは再利用、毎ピクセルの min/max を
/// ピラミッドから走査して segment を構築する経路を測る。
/// DAW の実利用 (タイムライン上に N トラック並ぶ) を想定し N=1, 8, 64 で測る。
fn bench_cached_call(c: &mut Criterion) {
    let l = samples_l();
    let r = samples_r();
    let planes: [&[f32]; 2] = [l, r];

    let mut group = c.benchmark_group("waveform/cached_call");
    for &n in &[1usize, 8, 64, 128] {
        let mut host = UiHost::<()>::no_redraw();
        let mut scene = Scene::new();
        // 事前ビルド (N 個のピラミッドが state HashMap に乗る)
        render_n_waveforms(&mut host, &mut scene, &planes, n, 0);

        group.bench_function(format!("{n} widgets × 5.76M samples × 2ch"), |b| {
            b.iter(|| {
                scene.clear();
                render_n_waveforms(&mut host, &mut scene, &planes, n, 0);
                black_box(());
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_initial_build, bench_cached_call);
criterion_main!(benches);
