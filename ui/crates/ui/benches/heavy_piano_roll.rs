// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! M5 Phase 14: heavy() + cached(viewport_key) の効果を 100k notes ピアノロールで計測。
//!
//! - `cached_viewport_100k`: viewport_key 固定 → 全 cache hit、draw_fn は warm-up 後 0 回実行
//! - `no_cache_viewport_100k`: viewport_key を毎反復で変える → 全 cache miss、毎回 draw_fn 実行
//!
//! 期待: cached が no_cache の 5x+ 高速 (M4 Phase 12 の 1.9x より大きい想定。
//! heavy は visible note rect が 100-300 個オーダーで、cache hit 時の `extend_from_slice`
//! コストと miss 時の `partition_point + walk + push_rect ×N` コストの差が大きく出る)。

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

use daw_ui_core::{FrameInput, UiHost, hash_inputs};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, Rect, RectCommand, Scene};

// ----- 100k notes 生成 (piano_roll example と同じ LCG ベース) -----

#[derive(Clone, Copy)]
struct Note {
    start_beat: f32,
    len_beats: f32,
    pitch: u8,
    velocity: u8,
}

fn generate_notes(count: usize) -> Vec<Note> {
    // LCG + splitmix64 finalizer (piano_roll example と同じアルゴリズム)。
    let mut state: u64 = 0x12345678_9ABCDEF0;
    let mut next = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    };
    let total_beats: f32 = 1024.0;
    let pitch_lo: u8 = 36;
    let pitch_hi: u8 = 96;
    let mut notes: Vec<Note> = Vec::with_capacity(count);
    for _ in 0..count {
        let r1 = next();
        let r2 = next();
        let r3 = next();
        let r4 = next();
        let start_beat = (r1 as f32 / u64::MAX as f32) * total_beats;
        let len_beats = 0.125 + (r2 as f32 / u64::MAX as f32) * 1.875;
        let pitch = pitch_lo + ((r3 % u64::from(pitch_hi - pitch_lo)) as u8);
        let velocity = 32 + ((r4 % 96) as u8);
        notes.push(Note { start_beat, len_beats, pitch, velocity });
    }
    notes.sort_by(|a, b| a.start_beat.partial_cmp(&b.start_beat).unwrap_or(std::cmp::Ordering::Equal));
    notes
}

// ----- 描画関数 (piano_roll example の heavy + cached 構造を簡略再現) -----

const KEYBOARD_W: f32 = 60.0;
const HEADER_H: f32 = 56.0;
const FOOTER_H: f32 = 56.0;

#[allow(clippy::many_single_char_names)]
fn render_frame(
    host: &mut UiHost<()>,
    scene: &mut Scene,
    screen: PhysicalSize,
    notes: &[Note],
    view: (f32, f32, f32, f32),
) {
    let (view_start, view_len, pitch_top, pitch_visible) = view;
    let view_end = view_start + view_len;
    let grid = Rect {
        x: KEYBOARD_W,
        y: HEADER_H,
        w: (screen.width as f32 - KEYBOARD_W).max(1.0),
        h: (screen.height as f32 - HEADER_H - FOOTER_H).max(1.0),
    };
    let viewport_key = (
        b"piano_roll_v1" as &[u8],
        view_start.to_bits(),
        view_len.to_bits(),
        pitch_top.to_bits(),
        pitch_visible.to_bits(),
        grid.w.to_bits(),
        grid.h.to_bits(),
        0u64, // notes_generation
    );
    host.frame_to_edits(&(), scene, screen, FrameInput::default(), |(), ui| {
        ui.heavy("piano_roll", |hctx| {
            let s_idx = notes.partition_point(|n| n.start_beat + n.len_beats < view_start);
            let e_idx = s_idx
                + notes[s_idx..].partition_point(|n| n.start_beat <= view_end);
            let visible: &[Note] = &notes[s_idx..e_idx];

            hctx.cached(viewport_key, |hctx| {
                // 主領域背景
                hctx.push_rect(RectCommand {
                    rect: grid,
                    fill: Color::rgb(0.12, 0.13, 0.16),
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: None,
                });
                // 拍縦線 (典型的な visible 範囲では 16 本程度、簡略形)
                let beat_to_px = grid.w / view_len;
                let first_beat = view_start.floor() as i32;
                let last_beat = view_end.ceil() as i32;
                for b in first_beat..=last_beat {
                    let x = grid.x + (b as f32 - view_start) * beat_to_px;
                    hctx.push_rect(RectCommand {
                        rect: Rect { x: x - 0.5, y: grid.y, w: 1.0, h: grid.h },
                        fill: Color::rgba(1.0, 1.0, 1.0, 0.12),
                        border: Color::TRANSPARENT,
                        border_width: 0.0,
                        radius: [0.0; 4],
                        clip_rect: None,
                    });
                }
                // notes 矩形 (visible のみ)
                let pitch_to_px = grid.h / pitch_visible;
                for note in visible {
                    let x = grid.x + (note.start_beat - view_start) * beat_to_px;
                    let w = (note.len_beats * beat_to_px).max(1.5);
                    let y = grid.y + (pitch_top - f32::from(note.pitch)) * pitch_to_px;
                    let h = (pitch_to_px - 1.0).max(2.0);
                    let t = f32::from(note.velocity) / 127.0;
                    hctx.push_rect(RectCommand {
                        rect: Rect { x, y, w, h },
                        fill: Color::rgba(0.35 + t * 0.35, 0.55 + t * 0.30, 0.85 + t * 0.10, 1.0),
                        border: Color::TRANSPARENT,
                        border_width: 0.0,
                        radius: [1.5; 4],
                        clip_rect: None,
                    });
                }
            });
        });
    });
}

// ----- bench -----

fn bench_piano_roll(c: &mut Criterion) {
    let notes = generate_notes(100_000);
    let screen = PhysicalSize { width: 1920, height: 1080 };
    let fixed_view: (f32, f32, f32, f32) = (0.0, 16.0, 84.0, 36.0);

    // sanity: visible 件数を一度確認 (description 用、bench 自体には影響しない)
    let view_start = fixed_view.0;
    let view_end = view_start + fixed_view.1;
    let s = notes.partition_point(|n| n.start_beat + n.len_beats < view_start);
    let e = s + notes[s..].partition_point(|n| n.start_beat <= view_end);
    eprintln!(
        "piano_roll bench: total={} notes, visible at fixed_view = {} notes (viewport_hash={:#x})",
        notes.len(),
        e - s,
        hash_inputs((
            b"piano_roll_v1" as &[u8],
            fixed_view.0.to_bits(),
            fixed_view.1.to_bits(),
            fixed_view.2.to_bits(),
            fixed_view.3.to_bits(),
            (screen.width as f32 - KEYBOARD_W).to_bits(),
            (screen.height as f32 - HEADER_H - FOOTER_H).to_bits(),
            0u64,
        )),
    );

    // cached: viewport_key 固定 → cache hit
    c.bench_function("piano_roll_cached_viewport_100k", |b| {
        b.iter_batched_ref(
            || {
                let mut host: UiHost<()> = UiHost::no_redraw();
                let mut scene = Scene::new();
                // warm-up: cache を populate
                render_frame(&mut host, &mut scene, screen, &notes, fixed_view);
                scene.clear();
                (host, scene)
            },
            |(host, scene)| {
                render_frame(host, scene, screen, &notes, fixed_view);
                scene.clear();
            },
            BatchSize::SmallInput,
        );
    });

    // no_cache: viewport_key を毎反復で変える → cache miss
    c.bench_function("piano_roll_no_cache_viewport_100k", |b| {
        let mut step: u64 = 0;
        b.iter_batched_ref(
            || (UiHost::<()>::no_redraw(), Scene::new()),
            |(host, scene)| {
                step += 1;
                // view_start を 0.001 拍ずつずらす → viewport_key が常に変わる
                let view = (step as f32 * 0.001, fixed_view.1, fixed_view.2, fixed_view.3);
                render_frame(host, scene, screen, &notes, view);
                scene.clear();
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, bench_piano_roll);
criterion_main!(benches);
