//! M4 Phase 12: scenegraph cache の効果を計測するベンチ。
//!
//! 1000 ボタンの UI を 1 フレーム描画する CPU コストを、
//! - `cached`: input_hash が前フレームと一致 → cache hit、draw_fn スキップ
//! - `no_cache`: text を毎フレーム変えて hash 不一致 → cache miss、draw_fn 実行
//!   の 2 シナリオで比較する。
//!
//! 期待: cached が大幅 (10x+) に速い。これが scenegraph cache の主目的。

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

use daw_ui_core::{Edit, FrameInput, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Rect, Scene};

const N_BUTTONS: usize = 1000;
const COLS: usize = 50;

fn button_rect(i: usize) -> Rect {
    Rect {
        x: (i % COLS) as f32 * 38.0,
        y: (i / COLS) as f32 * 32.0,
        w: 36.0,
        h: 28.0,
    }
}

fn render_buttons(host: &mut UiHost<()>, scene: &mut Scene, screen: PhysicalSize, label: &str) {
    host.frame_to_edits(&(), scene, screen, FrameInput::default(), |(), ui| {
        for i in 0..N_BUTTONS {
            ui.button_at(("btn", i), label, button_rect(i), || {
                Edit::mutate(|(): &mut ()| {})
            });
        }
    });
}

fn bench_static_1000_buttons(c: &mut Criterion) {
    let screen = PhysicalSize { width: 1920, height: 1080 };

    // cached: 同じ label / rect で毎回描画 → 全 cache hit
    c.bench_function("static_1000_buttons_cached", |b| {
        b.iter_batched_ref(
            || {
                let mut host: UiHost<()> = UiHost::no_redraw();
                let mut scene = Scene::new();
                // Warm-up: cache を populate する 1 フレーム
                render_buttons(&mut host, &mut scene, screen, "x");
                scene.clear();
                (host, scene)
            },
            |(host, scene)| {
                render_buttons(host, scene, screen, "x");
                scene.clear();
            },
            BatchSize::SmallInput,
        );
    });

    // no_cache: label を毎回変えて hash 不一致 → 全 cache miss、毎フレーム draw_fn 実行
    c.bench_function("static_1000_buttons_no_cache", |b| {
        let mut counter: u64 = 0;
        b.iter_batched_ref(
            || {
                let host: UiHost<()> = UiHost::no_redraw();
                let scene = Scene::new();
                (host, scene)
            },
            |(host, scene)| {
                counter += 1;
                let label = format!("btn{counter}");
                render_buttons(host, scene, screen, &label);
                scene.clear();
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, bench_static_1000_buttons);
criterion_main!(benches);
