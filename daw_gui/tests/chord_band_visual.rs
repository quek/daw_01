//! r.md #157: コードトラックの帯とポップアップの見た目 (GPU のオフスクリーン描画)。
//!
//! ルート view を組み、コードの箱 (既定色 / 明るいユーザー色 / 暗いユーザー色) とポップアップを出した
//! 状態を `target/chord_shots/<theme>.png` に撮る (目視用)。検証するのは「箱の文字が塗りに対して
//! 読める」(ラベルのある行の中で、塗りと文字の明度が十分に離れている) こと — 色の SSoT
//! (`widgets::chord_colors`) を明暗両方のテーマ・明暗両方の塗りで通す。
//!
//! GPU adapter が無い環境では `OffscreenRenderer::new` が `Err` を返すので graceful skip。

use std::sync::Arc;

use common::model::{Chord, ChordEvent, ChordKind};
use common::protocol::PluginCommand;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher};
use daw_gui::state::chord_band::ChordPopup;
use daw_ui_core::{FrameInput, PointerFrame, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{OffscreenRenderer, Scene};

const W: u32 = 1100;
const H: u32 = 700;

fn build_app() -> (AppData, UnboundedReceiver<PluginCommand>) {
    let (audio_tx, _audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let app = AppData::new(audio_tx, plugin_tx, None, None, event_dispatcher, job_dispatcher, None, None, 48_000);
    (app, plugin_rx)
}

/// 既定色 / 明るい黄 / 暗い紺の 3 箱。
fn populate(app: &mut AppData) -> u32 {
    let mut first = 0;
    app.edit_song(|song| {
        let boxes = [
            (0.0, Chord::new(9, ChordKind::Minor7), None),
            (4.0, Chord::new(5, ChordKind::Major7), Some([0.95, 0.85, 0.2])),
            (8.0, Chord::new(7, ChordKind::Dominant7), Some([0.05, 0.07, 0.25])),
        ];
        for (i, (s, chord, color)) in boxes.into_iter().enumerate() {
            let id = song.place_chord(ChordEvent { id: 0, start_beat: s, len_beats: 4.0, chord, color }).unwrap();
            if i == 0 {
                first = id;
            }
        }
    });
    first
}

fn relative_luminance(r: u8, g: u8, b: u8) -> f32 {
    let lin = |v: u8| daw_ui_core::theme::srgb_to_linear(f32::from(v) / 255.0);
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

fn shoot(renderer: &mut OffscreenRenderer, theme: &str) -> Vec<u8> {
    let (mut app, _rx) = build_app();
    app.handle_event(AppEvent::SetTheme(theme.to_string()));
    let first = populate(&mut app);
    // 3 箱 (12 拍) が帯に収まるよう、ランチャー帯を畳んで 1 拍 = 24px にする。
    app.cur.view.launcher_layout = common::model::LauncherLayout::ArrangerOnly;
    app.cur.view.arrange_zoom_x = 24.0;
    app.cur.view.arrange_scroll_beat = 0.0;
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    if host.set_palette(app.theme.core.clone()) {
        host.invalidate_scene_cache();
    }
    let screen = PhysicalSize { width: W, height: H };
    let idle = || FrameInput { pointer: PointerFrame { pos: Some((W as f32 - 5.0, H as f32 - 5.0)), ..PointerFrame::default() }, ..FrameInput::default() };
    // 1 フレーム目で帯の位置が mirror される → ポップアップを開いて描く。
    let mut scene = Scene::new();
    host.frame(&mut app, &mut scene, screen, idle(), |app, ui| daw_gui::view::root::build_root(app, ui, screen));
    app.cur.peph.chord_band.popup = Some(ChordPopup::new(first, "Am7".to_string()));
    for _ in 0..2 {
        scene = Scene::new();
        host.frame(&mut app, &mut scene, screen, idle(), |app, ui| daw_gui::view::root::build_root(app, ui, screen));
    }
    let rgba = renderer.render_to_rgba(&scene).expect("offscreen render");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/chord_shots");
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = image::save_buffer(dir.join(format!("{theme}.png")), &rgba, W, H, image::ColorType::Rgba8);
    }
    // 箱の中 (帯の縦中央の行、箱の左端から文字が載る 60px) で、最も明るい画素と最も暗い画素の
    // コントラスト比。文字が塗りに沈んでいれば 1 に近づく。
    let lane = app.cur.peph.chord_band.lane;
    let y = (lane.rect.y + lane.rect.h * 0.5) as u32;
    for (i, start) in [0.0f64, 4.0, 8.0].into_iter().enumerate() {
        let x0 = lane.x_of(start) as u32 + 3;
        let (mut lo, mut hi) = (f32::INFINITY, 0.0f32);
        for dy in 0..4u32 {
            for x in x0..x0 + 60 {
                let p = (((y - 2 + dy) * W + x) * 4) as usize;
                let l = relative_luminance(rgba[p], rgba[p + 1], rgba[p + 2]);
                lo = lo.min(l);
                hi = hi.max(l);
            }
        }
        let ratio = (hi + 0.05) / (lo + 0.05);
        assert!(ratio >= 3.0, "{theme}: 箱 {i} の文字のコントラストが足りない ({ratio:.2}:1)");
    }
    rgba
}

#[test]
fn chord_band_and_popup_render_readably_in_both_themes() {
    let Ok(mut renderer) = OffscreenRenderer::new(W, H) else {
        eprintln!("GPU adapter が無いので skip");
        return;
    };
    let dark = shoot(&mut renderer, "dark");
    let light = shoot(&mut renderer, "light");
    assert_ne!(dark, light, "テーマで描画が変わる");
}

/// 「コードを抽出…」のダイアログで「1 コードの長さ」の一覧を開くと、一覧が下の行 (もう 1 つの dropdown /
/// checkbox / ボタン) より手前に描かれる。`target/chord_shots/extract_dialog.png` に撮る (目視用)。
/// 画面 1100×700 の中央に 380×236 のパネル (左上 360,232)。1 行目の dropdown は x∈[528,722) / y∈[280,306)。
#[test]
fn the_extract_dialog_draws_its_open_list_above_the_rows_below() {
    use daw_gui::event_chord::ChordToolEvent;
    let (mut app, _rx) = build_app();
    app.handle_event(AppEvent::ChordTool(ChordToolEvent::OpenExtractDialog { clips: vec![(1, 1)] }));
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    if host.set_palette(app.theme.core.clone()) {
        host.invalidate_scene_cache();
    }
    let screen = PhysicalSize { width: W, height: H };
    let at = |x: f32, y: f32, down: bool, up: bool| FrameInput {
        pointer: PointerFrame {
            pos: Some((x, y)),
            primary_just_pressed: down,
            primary_pressed: down,
            primary_just_released: up,
            ..PointerFrame::default()
        },
        ..FrameInput::default()
    };
    let mut scene = Scene::new();
    // 開く → dropdown の本体を click (press / release) → 一覧の上へ pointer を置いて描く。
    for input in [
        at(600.0, 293.0, false, false),
        at(600.0, 293.0, true, false),
        at(600.0, 293.0, false, true),
        at(600.0, 340.0, false, false),
    ] {
        scene = Scene::new();
        host.frame(&mut app, &mut scene, screen, input, |app, ui| daw_gui::view::root::build_root(app, ui, screen));
    }
    let rects = scene.popup_rects_vec();
    // 一覧の背景 (6 項目 × 24px) と、2 行目の dropdown の本体 (y = 280 + 26 + 8)。
    let list = rects.iter().position(|r| (r.rect.h - 144.0).abs() < 0.5 && (r.rect.y - 306.0).abs() < 2.0);
    let second = rects.iter().position(|r| (r.rect.y - 314.0).abs() < 0.5 && (r.rect.h - 26.0).abs() < 0.5);
    let (list, second) = (list.expect("一覧が開いている"), second.expect("2 行目の dropdown がある"));
    assert!(list > second, "一覧 ({list}) は 2 行目の dropdown ({second}) より後 = 手前に描かれる");
    if let Ok(mut renderer) = OffscreenRenderer::new(W, H) {
        let rgba = renderer.render_to_rgba(&scene).expect("offscreen render");
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/chord_shots");
        if std::fs::create_dir_all(&dir).is_ok() {
            let _ = image::save_buffer(dir.join("extract_dialog.png"), &rgba, W, H, image::ColorType::Rgba8);
        }
    }
}
