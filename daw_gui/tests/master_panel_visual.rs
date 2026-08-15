//! r.md #50: マスターパネルの **visual regression**。
//!
//! メーターは「値は正しいのに何も描かれていない」「一様塗りに潰れている」という
//! 壊れ方をしても build / test / clippy を全部すり抜ける (CLAUDE.md
//! 「Visual regression smoke test」)。ここでは実際にルート view を 1 フレーム組んで
//! GPU でオフスクリーン描画し、
//!
//! 1. パネルを開くと**画面右端の帯が実際に置き換わる** (レイアウトが切り出されている)
//! 2. その帯が一様塗りではない (= フェーダー / メーター / 波形 / 文字が描かれている)
//! 3. スペクトラム・オシロ・ゴニオそれぞれの高さ帯にも中身がある
//!    (「MASTER だけ描けて下 3 つが空」を捕まえる)
//!
//! を pixel で検証する。描画結果は `target/theme_shots/master_panel_*.png` に残すので
//! 目視確認にも使える。
//!
//! GPU adapter が無い環境では `OffscreenRenderer::new` が `Err` を返すので graceful skip。

use std::collections::HashSet;
use std::sync::Arc;

use common::protocol::PluginCommand;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_gui::master_meter::MasterMeterSnapshot;
use daw_gui::master_meter::loudness::LoudnessReadout;
use daw_gui::master_meter::scope::SCOPE_COLUMNS;
use daw_gui::master_meter::spectrum::{SPECTRUM_BANDS, band_center_hz};
use daw_gui::master_meter::stereo::StereoReadout;
use daw_ui_core::{FrameInput, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{OffscreenRenderer, Scene};

const W: u32 = 960;
const H: u32 = 600;

fn build_app() -> (AppData, UnboundedReceiver<PluginCommand>) {
    let (audio_tx, _audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None,
        event_dispatcher,
        job_dispatcher,
        None,
        None,
        48_000,
    );
    (app, plugin_rx)
}

/// 「鳴っている」状態のスナップショット。全メーターに意味のある形を入れて、
/// どれか 1 つでも描かれなくなったら pixel 差で分かるようにする。
fn playing_snapshot() -> MasterMeterSnapshot {
    let mut s = MasterMeterSnapshot {
        vu: [0.50, 0.42],
        peak: [0.72, 0.66],
        peak_hold: [0.80, 0.75],
        peak_max_db: -1.9,
        clip_count: 3,
        loudness: LoudnessReadout {
            momentary_lufs: -12.3,
            short_term_lufs: -13.1,
            integrated_lufs: -14.5,
            max_momentary_lufs: -9.0,
            max_short_term_lufs: -10.4,
            lra_lu: 6.2,
            lra_provisional: false,
            measured_secs: 92.0,
        },
        true_peak_dbtp: -0.9,
        max_true_peak_dbtp: -0.4,
        stereo: StereoReadout {
            correlation: 0.65,
            correlation_min: 0.12,
            correlation_max: 0.93,
            width: 0.80,
            balance_db: -1.2,
        },
        ..MasterMeterSnapshot::default()
    };
    // スペクトラム: 低域が高い右下がり + 1kHz にピーク。
    s.spectrum_db = (0..SPECTRUM_BANDS)
        .map(|b| {
            let f = band_center_hz(b);
            let base = -18.0 - 12.0 * (f / 100.0).log10();
            let peak = -14.0 * ((f / 1000.0).log2()).abs();
            base.max(peak).clamp(-100.0, -3.0)
        })
        .collect();
    s.spectrum_hold_db = s.spectrum_db.iter().map(|v| v + 3.0).collect();
    // オシロ: 3 周期の正弦 (L と R を少しずらす)。
    s.scope = (0..SCOPE_COLUMNS)
        .map(|c| {
            let t = c as f32 / SCOPE_COLUMNS as f32 * std::f32::consts::TAU * 3.0;
            let l = t.sin() * 0.8;
            let r = (t + 0.7).sin() * 0.6;
            [l - 0.02, l + 0.02, r - 0.02, r + 0.02]
        })
        .collect();
    // ゴニオ: 楕円の軌跡。
    s.gonio = (0..512)
        .map(|i| {
            let a = i as f32 / 512.0 * std::f32::consts::TAU;
            [a.cos() * 0.55, a.sin() * 0.85]
        })
        .collect();
    s.sample_rate = 48_000;
    s.visual_digest = 12345;
    s
}

struct Region {
    unique_colors: usize,
    pixels: Vec<u8>,
}

/// `[x0, x1) × [y0, y1)` を切り出して統計を取る。
fn crop(rgba: &[u8], x0: u32, x1: u32, y0: u32, y1: u32) -> Region {
    let mut uniq: HashSet<u32> = HashSet::new();
    let mut pixels = Vec::new();
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            let px = &rgba[i..i + 4];
            uniq.insert(u32::from_be_bytes([px[0], px[1], px[2], 0]));
            pixels.extend_from_slice(&px[0..3]);
        }
    }
    Region { unique_colors: uniq.len(), pixels }
}

fn render(renderer: &mut OffscreenRenderer, open: bool, name: &str) -> Vec<u8> {
    render_themed(renderer, open, "dark", name)
}

fn render_themed(
    renderer: &mut OffscreenRenderer,
    open: bool,
    theme: &str,
    name: &str,
) -> Vec<u8> {
    let (mut app, _rx) = build_app();
    app.handle_event(AppEvent::SetTheme(theme.to_string()));
    if app.ui_prefs.master_panel_open != open {
        app.handle_event(AppEvent::ToggleMasterPanel);
    }
    assert_eq!(app.ui_prefs.master_panel_open, open);
    app.handle_event(AppEvent::AddInstrumentTrack);
    app.handle_event(AppEvent::MasterMeterTick(Box::new(playing_snapshot())));

    let mut host: UiHost<AppData> = UiHost::no_redraw();
    if host.set_palette(app.theme.core.clone()) {
        host.invalidate_scene_cache();
    }
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: W, height: H };
    host.frame_to_edits(&app, &mut scene, screen, FrameInput::default(), |app, ui| {
        daw_gui::view::root::build_root(app, ui, screen);
    });
    let rgba = renderer.render_to_rgba(&scene).expect("offscreen render");

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/theme_shots");
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = image::save_buffer(
            dir.join(format!("master_panel_{name}.png")),
            &rgba,
            W,
            H,
            image::ColorType::Rgba8,
        );
    }
    rgba
}

#[test]
fn the_master_panel_occupies_the_right_edge_and_draws_every_meter() {
    let Ok(mut renderer) = OffscreenRenderer::new(W, H) else {
        eprintln!("skip master panel visual test: no GPU adapter/device");
        return;
    };

    let (app, _rx) = build_app();
    let panel_w = daw_gui::view::master_panel::panel_width(&app);
    assert!(panel_w >= 180.0, "既定でパネルが出ている前提: {panel_w}");

    let open = render(&mut renderer, true, "open");
    let closed = render(&mut renderer, false, "closed");

    // パネルが占める帯 (menu/transport の下、status の上)。
    let x0 = W - panel_w as u32 + 2;
    let x1 = W - 2;
    let y0 = 24 + 44 + 2; // MENU_H + TRANSPORT_H
    let y1 = H - 24 - 2; // STATUS_H
    let open_region = crop(&open, x0, x1, y0, y1);
    let closed_region = crop(&closed, x0, x1, y0, y1);

    // 1. 開閉で右端の帯が実際に変わる (= レイアウトが切り出されている)。
    assert_ne!(
        open_region.pixels, closed_region.pixels,
        "パネルの開閉で右端の見た目が変わらない = 領域が切り出されていない"
    );

    // 2. 一様塗りに潰れていない。フェーダー / 目盛り / 数値 / 波形が入る帯なので、
    //    色数が数十しかなければ何かが描かれていない。
    assert!(
        open_region.unique_colors > 150,
        "パネルの中身が乏しい: unique_colors={}",
        open_region.unique_colors
    );

    // 3. セクションごとに中身がある (「MASTER だけ描けて下 3 つが空」を捕まえる)。
    //    既定配分 [0.34, 0.24, 0.18, 0.24] を高さ帯に割り当て、各帯を個別に見る。
    let h = (y1 - y0) as f32;
    let mut y = y0 as f32;
    for (i, (name, ratio)) in
        [("MASTER", 0.34), ("spectrum", 0.24), ("scope", 0.18), ("gonio", 0.24)]
            .into_iter()
            .enumerate()
    {
        let band_h = h * ratio;
        // 見出し行と境界を避けて内側だけ見る。
        let by0 = (y + 18.0) as u32;
        let by1 = (y + band_h - 4.0) as u32;
        if by1 > by0 + 4 {
            let r = crop(&open, x0, x1, by0, by1);
            assert!(
                r.unique_colors > 12,
                "section {i} ({name}) が空に見える: unique_colors={} (y {by0}..{by1})",
                r.unique_colors
            );
        }
        y += band_h;
    }
}

/// r.md #48 のテーマ切替に追従していること。パネルはパレットトークンだけで
/// 色を決めているので、ライトにしても「暗い帯が右端に残る」ことは無いはず。
#[test]
fn the_master_panel_follows_the_light_theme() {
    let Ok(mut renderer) = OffscreenRenderer::new(W, H) else {
        eprintln!("skip master panel light test: no GPU adapter/device");
        return;
    };
    let dark = render_themed(&mut renderer, true, "dark", "dark");
    let light = render_themed(&mut renderer, true, "light", "light");

    let (app, _rx) = build_app();
    let panel_w = daw_gui::view::master_panel::panel_width(&app) as u32;
    let x0 = W - panel_w + 2;
    let x1 = W - 2;
    let y0 = 24 + 44 + 2;
    let y1 = H - 24 - 2;

    let mean = |rgba: &[u8]| {
        let r = crop(rgba, x0, x1, y0, y1);
        let sum: f32 = r
            .pixels
            .chunks_exact(3)
            .map(|p| {
                daw_ui_core::color::relative_luminance(
                    f32::from(p[0]) / 255.0,
                    f32::from(p[1]) / 255.0,
                    f32::from(p[2]) / 255.0,
                )
            })
            .sum();
        (sum / (r.pixels.len() / 3) as f32, r.unique_colors)
    };
    let (dark_l, dark_u) = mean(&dark);
    let (light_l, light_u) = mean(&light);
    assert!(
        light_l > dark_l + 0.25,
        "ライトでパネルが明るくならない: dark={dark_l} light={light_l}"
    );
    assert!(light_u > 150, "ライトでパネルの中身が乏しい: {light_u}");
    assert!(dark_u > 150, "ダークでパネルの中身が乏しい: {dark_u}");
}
