// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! r.md #54: ラウドネスレポート window の **visual regression**。
//!
//! グラフやヒストグラムは「値は正しいのに何も描かれていない」「一様塗りに潰れて
//! いる」という壊れ方をしても build / test / clippy を全部すり抜ける
//! (CLAUDE.md 「Visual regression smoke test」)。ここでは実際にルート view を
//! 1 フレーム組んで GPU でオフスクリーン描画し、
//!
//! 1. 窓を開くと画面のその矩形が**実際に置き換わる** (= 描画経路に乗っている)
//! 2. 窓の中が一様塗りではない (= 数値 / ボタン / グラフが描かれている)
//! 3. グラフ帯に中身がある (「数値だけ描けてグラフが空」を捕まえる)
//! 4. 走査中は**背景が暗転する** (= 遮断していることが見て分かる)
//!
//! を pixel で検証する。描画結果は `target/theme_shots/loudness_report_*.png` に残す。
//!
//! GPU adapter が無い環境では `OffscreenRenderer::new` が `Err` を返すので graceful skip。

use std::collections::HashSet;
use std::sync::Arc;

use common::loudness_report::{
    LOUDNESS_CURVE_COLUMNS, LOUDNESS_HISTOGRAM_BINS, LoudnessReport,
};
use common::protocol::PluginCommand;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_gui::state::LoudnessPhase;
use daw_ui_core::{FrameInput, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{OffscreenRenderer, Rect, Scene};

const W: u32 = 960;
const H: u32 = 640;

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

/// 「測り終わった」レポート。曲線に山と谷を入れて、潰れたら pixel 差で分かるようにする。
fn finished_report() -> Box<LoudnessReport> {
    let mut short_term = [f32::NEG_INFINITY; LOUDNESS_CURVE_COLUMNS];
    let mut momentary = [f32::NEG_INFINITY; LOUDNESS_CURVE_COLUMNS];
    for i in 0..LOUDNESS_CURVE_COLUMNS {
        // 3s 窓は先頭 5% が未到達 (走査開始直後は値が出ない) — その形も検証対象。
        let t = i as f32 / LOUDNESS_CURVE_COLUMNS as f32;
        if t > 0.05 {
            short_term[i] = -16.0 + 6.0 * (t * std::f32::consts::TAU * 2.0).sin();
        }
        momentary[i] = -14.0 + 9.0 * (t * std::f32::consts::TAU * 5.0).sin();
    }
    let mut histogram = [0u32; LOUDNESS_HISTOGRAM_BINS];
    for (i, h) in histogram.iter_mut().enumerate() {
        // -70 + i LU。-20 LUFS あたりに山。
        let d = (i as f32 - 50.0).abs();
        *h = (200.0 / (1.0 + d * d)) as u32;
    }
    Box::new(LoudnessReport {
        range_start_beat: 8.0,
        range_end_beat: 72.0,
        range_start_frame: 192_000,
        sample_rate: 48_000,
        done_frames: 1_536_000,
        total_frames: 1_536_000,
        complete: true,
        integrated_lufs: -13.4,
        lra_lu: 6.8,
        lra_provisional: false,
        max_momentary_lufs: -8.2,
        max_momentary_at_secs: Some(12.5),
        max_short_term_lufs: -9.9,
        max_short_term_at_secs: Some(11.0),
        true_peak_dbtp: -0.3,
        true_peak_at_secs: Some(15.25),
        sample_peak_dbfs: -0.9,
        sample_peak_at_secs: Some(15.25),
        clipped_samples: 0,
        measured_secs: 32.0,
        short_term_curve: short_term,
        momentary_curve: momentary,
        histogram,
    })
}

struct Region {
    unique_colors: usize,
    pixels: Vec<u8>,
}

fn crop(rgba: &[u8], r: Rect) -> Region {
    let mut uniq: HashSet<u32> = HashSet::new();
    let mut pixels = Vec::new();
    let x0 = r.x.max(0.0) as u32;
    let x1 = (r.x + r.w).min(W as f32) as u32;
    let y0 = r.y.max(0.0) as u32;
    let y1 = (r.y + r.h).min(H as f32) as u32;
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

/// `open` / `busy` を指定して 1 フレーム描き、RGBA と window rect を返す。
fn render(
    renderer: &mut OffscreenRenderer,
    open: bool,
    busy: bool,
    name: &str,
) -> (Vec<u8>, Rect) {
    let (mut app, _rx) = build_app();
    app.handle_event(AppEvent::SetTheme("dark".to_string()));
    app.handle_event(AppEvent::AddInstrumentTrack);
    if app.ui_prefs.loudness_report_open != open {
        app.handle_event(AppEvent::ToggleLoudnessReport);
    }
    assert_eq!(app.ui_prefs.loudness_report_open, open);
    if open {
        app.loudness.report = Some(finished_report());
        // 「測ったときの Song」= 今の Song にしておく (揃えないと窓が
        // 「もう古い」警告状態で描かれ、通常表示の regression を見られない)。
        app.loudness.report_epoch = app.song_doc.edit_epoch();
        if busy {
            app.loudness.phase = LoudnessPhase::Running;
        }
    }

    let screen_rect = Rect { x: 0.0, y: 0.0, w: W as f32, h: H as f32 };
    let win = daw_gui::view::loudness_report::window_rect(&app, screen_rect);

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
            dir.join(format!("loudness_report_{name}.png")),
            &rgba,
            W,
            H,
            image::ColorType::Rgba8,
        );
    }
    (rgba, win)
}

#[test]
fn レポート窓は数値もグラフも描かれ走査中は背景が暗転する() {
    let Ok(mut renderer) = OffscreenRenderer::new(W, H) else {
        eprintln!("skip loudness report visual test: no GPU adapter/device");
        return;
    };

    let (open, win) = render(&mut renderer, true, false, "open");
    let (closed, _) = render(&mut renderer, false, false, "closed");
    assert!(win.w >= 560.0 && win.h >= 420.0, "窓が最小サイズを割っている: {win:?}");

    // 1. 窓を開くとその矩形が実際に置き換わる。
    let open_win = crop(&open, win);
    let closed_win = crop(&closed, win);
    assert_ne!(
        open_win.pixels, closed_win.pixels,
        "窓の開閉で見た目が変わらない = 描画経路に乗っていない"
    );

    // 2. 一様塗りに潰れていない (数値 8 行 + ボタン + プリセット + グラフが入る)。
    assert!(
        open_win.unique_colors > 150,
        "窓の中身が乏しい: unique_colors={}",
        open_win.unique_colors
    );

    // 3. グラフ帯 (窓の下半分) に中身がある = 「数値だけ描けてグラフが空」を捕まえる。
    let graph_band = Rect {
        x: win.x + 12.0,
        y: win.y + win.h * 0.6,
        w: win.w - 24.0,
        h: win.h * 0.4 - 12.0,
    };
    let band = crop(&open, graph_band);
    assert!(
        band.unique_colors > 20,
        "グラフ帯が一様: unique_colors={}",
        band.unique_colors
    );

    // 4. 走査中は背景 (窓の外) が暗転する = 遮断していることが見て分かる。
    let (busy, _) = render(&mut renderer, true, true, "busy");
    let bg = Rect { x: 8.0, y: 200.0, w: 100.0, h: 100.0 };
    assert_ne!(
        crop(&busy, bg).pixels,
        crop(&open, bg).pixels,
        "走査中なのに背景が暗転していない"
    );
}
