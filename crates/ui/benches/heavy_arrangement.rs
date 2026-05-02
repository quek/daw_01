//! M5 Phase 17: heavy() + cached(viewport_key) を 500 widgets スケールで計測。
//!
//! - `arrangement_cached_heavy_500w`: viewport_key 固定 → 外側 cache hit、500 widgets
//!   の `with_widget_node` も全て skip → scene への extend_from_slice のみ
//! - `arrangement_no_cache_heavy_500w`: viewport_key を毎反復で変える → 外側 cache miss、
//!   500 widgets の per-widget input_hash 判定 + per-widget cache hit (sample 不変なら)
//!
//! 期待: cached が no_cache の **10x+** 高速 (Phase 14 piano_roll の 5.77x より大きい)。
//! 500 widgets の per-widget input_hash 判定オーバーヘッドが outer hit で完全 skip
//! される効果が支配的になる想定。

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

use daw_ui_core::{
    ChannelLayout, FrameInput, SampleSlices, UiHost, WaveformRenderMode, WaveformSource,
    WaveformStyle, WaveformView,
};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, Rect, Scene};

const SAMPLE_RATE: u32 = 48_000;
const SECONDS: f32 = 60.0;
const TRACKS: usize = 10;
const CLIPS_PER_TRACK: usize = 50;
const N_WIDGETS: usize = TRACKS * CLIPS_PER_TRACK;

fn generate_test_samples(seconds: f32, sample_rate: u32) -> Vec<Vec<f32>> {
    let frames = (seconds * sample_rate as f32) as usize;
    let mut plane: Vec<f32> = Vec::with_capacity(frames);
    for i in 0..frames {
        let t = i as f32 / sample_rate as f32;
        let env = ((t * 0.5 * std::f32::consts::TAU).sin() * 0.5 + 0.5).powi(2);
        let f = 220.0;
        let phase = (t * f * std::f32::consts::TAU).sin();
        let harm = (t * f * 2.0 * std::f32::consts::TAU).sin() * 0.3;
        let n = (i.wrapping_mul(1664525).wrapping_add(1013904223)) as u32;
        let noise = (n as f32 / u32::MAX as f32 - 0.5) * 0.1;
        plane.push((phase + harm + noise) * env * 0.85);
    }
    vec![plane]
}

fn arrangement_area(screen: PhysicalSize) -> Rect {
    let pad_x = 8.0;
    let header_h = 88.0;
    let footer_h = 56.0;
    let w = (screen.width as f32 - pad_x * 2.0).max(100.0);
    let h = (screen.height as f32 - header_h - footer_h).max(100.0);
    Rect { x: pad_x, y: header_h, w, h }
}

fn clip_rect(area: Rect, i: usize) -> Rect {
    let col = i % CLIPS_PER_TRACK;
    let row = i / CLIPS_PER_TRACK;
    let cell_w = area.w / CLIPS_PER_TRACK as f32;
    let cell_h = area.h / TRACKS as f32;
    Rect {
        x: area.x + col as f32 * cell_w,
        y: area.y + row as f32 * cell_h,
        w: (cell_w - 1.0).max(1.0),
        h: (cell_h - 1.0).max(1.0),
    }
}

#[allow(clippy::many_single_char_names)]
fn render_frame(
    host: &mut UiHost<()>,
    scene: &mut Scene,
    screen: PhysicalSize,
    samples: &[Vec<f32>],
    view: (u64, u64),
) {
    let (view_start, view_len) = view;
    let area = arrangement_area(screen);
    let viewport_key = (
        b"arrangement_v1" as &[u8],
        view_start,
        view_len,
        1.0_f32.to_bits(),
        0.0_f32.to_bits(),
        1.0_f32.to_bits(),
        area.w.to_bits(),
        area.h.to_bits(),
        0u64,
    );
    let planes: Vec<&[f32]> = samples.iter().map(Vec::as_slice).collect();
    let valid_len = samples.first().map_or(0, Vec::len);
    let total_frames = valid_len as u64;
    let source = WaveformSource {
        samples: SampleSlices::Planar(&planes),
        valid_len,
        generation: 0,
        sample_rate: SAMPLE_RATE,
    };
    let style = WaveformStyle {
        fg: Color::rgb(0.55, 0.78, 0.95),
        fg_clipped: Color::rgb(0.95, 0.45, 0.40),
        fill: None,
        baseline: Some(Color::rgba(1.0, 1.0, 1.0, 0.08)),
        channel_layout: ChannelLayout::Overlay,
        render_mode: WaveformRenderMode::Auto,
        line_width_px: 1.0,
    };

    host.frame_to_edits(&(), scene, screen, FrameInput::default(), |(), ui| {
        ui.heavy("arrangement", |hctx| {
            hctx.cached(viewport_key, |hctx| {
                let shift = view_len / CLIPS_PER_TRACK as u64;
                let max_start = total_frames.saturating_sub(view_len);
                for i in 0..N_WIDGETS {
                    let rect = clip_rect(area, i);
                    let view = WaveformView {
                        start_sample: view_start
                            .saturating_add(shift * (i as u64))
                            .min(max_start),
                        len_samples: view_len,
                        vertical_gain: 1.0,
                    };
                    let _ = hctx.waveform(("clip", i), rect, source, view, style);
                }
            });
        });
    });
}

fn bench_arrangement(c: &mut Criterion) {
    let samples = generate_test_samples(SECONDS, SAMPLE_RATE);
    let total_frames = samples.first().map_or(0, |p| p.len() as u64);
    let screen = PhysicalSize { width: 1920, height: 1080 };
    let fixed_view: (u64, u64) = (0, total_frames);

    eprintln!(
        "arrangement bench: {} widgets, total={} frames, fixed view_len={}",
        N_WIDGETS, total_frames, fixed_view.1
    );

    // cached: viewport_key 固定 → outer hit
    c.bench_function("arrangement_cached_heavy_500w", |b| {
        b.iter_batched_ref(
            || {
                let mut host: UiHost<()> = UiHost::no_redraw();
                let mut scene = Scene::new();
                render_frame(&mut host, &mut scene, screen, &samples, fixed_view); // warm-up
                scene.clear();
                (host, scene)
            },
            |(host, scene)| {
                render_frame(host, scene, screen, &samples, fixed_view);
                scene.clear();
            },
            BatchSize::SmallInput,
        );
    });

    // no_cache: viewport_key を毎反復で変える → outer miss
    c.bench_function("arrangement_no_cache_heavy_500w", |b| {
        let mut step: u64 = 0;
        b.iter_batched_ref(
            || (UiHost::<()>::new(), Scene::new()),
            |(host, scene)| {
                step += 1;
                let view = (step, fixed_view.1); // view_start を 1 sample ずつずらす
                render_frame(host, scene, screen, &samples, view);
                scene.clear();
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, bench_arrangement);
criterion_main!(benches);
