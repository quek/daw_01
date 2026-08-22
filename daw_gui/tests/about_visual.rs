//! r.md #60: ヘルプ > バージョン情報 (About) の **visual regression**。
//!
//! この画面は GPLv3 §0 の Appropriate Legal Notices を満たすための法的な表示面なので、
//! 「イベントは飛ぶが何も描かれない」「タブを切り替えても中身が出ない」という壊れ方を
//! したら義務を果たせない。しかもその壊れ方は build / clippy / 通常の unit test を全部
//! すり抜ける (CLAUDE.md 「Visual regression smoke test」)。
//!
//! ここではルート view を実際に 1 フレーム組んでオフスクリーン描画し、
//!
//! 1. メニューから開くと画面中央が**実際に置き換わる** (= 描画経路に乗っている)
//! 2. パネルの中が一様塗りではない (= 文字が描かれている)
//! 3. タブを切り替えると本文が入れ替わる (= 3 つの面すべてに中身がある)
//!
//! を pixel で検証する。描画結果は `target/theme_shots/about_*.png` に残るので、
//! 目視の sign-off もアプリを起動せずにその PNG で行える。
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
use daw_ui_core::{FrameInput, PointerFrame, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{OffscreenRenderer, Rect, Scene};

const W: u32 = 960;
const H: u32 = 640;

/// `about::draw` の panel サイズ計算と同じ式。ここがずれると crop 範囲がずれるので、
/// view 側を変えたらこちらも合わせる (= 意図しないサイズ変更を落とすための二重化)。
fn panel_rect() -> Rect {
    let (sw, sh) = (W as f32, H as f32);
    let pw = (sw * 0.92).min(940.0);
    let ph = (sh * 0.92).min(760.0);
    Rect { x: ((sw - pw) * 0.5).max(0.0), y: ((sh - ph) * 0.5).max(0.0), w: pw, h: ph }
}

/// タブの pane (= 本文が描かれる領域)。`about::draw` の PAD / TITLE_H と
/// `tab_view` の TAB_BAR_H に対応する。
fn pane_rect() -> Rect {
    const PAD: f32 = 22.0;
    const TITLE_H: f32 = 44.0;
    const TAB_BAR_H: f32 = 32.0;
    let p = panel_rect();
    Rect {
        x: p.x + PAD,
        y: p.y + TITLE_H + TAB_BAR_H,
        w: p.w - PAD * 2.0,
        h: (p.h - TITLE_H - PAD - TAB_BAR_H).max(0.0),
    }
}

const TAB_LABELS: &[&str] = &["概要", "第三者コンポーネント", "ライセンス全文 (GPL-3.0)"];
/// `tab_view` の定数 (`TAB_FONT` / `TAB_PAD_X`)。
const TAB_FONT: f32 = 14.0;
const TAB_PAD_X: f32 = 16.0;

/// 各タブの中心 x を **実フォントの advance** から求める (`tab_view` と同じ式)。
/// 文字数ベースで決め打ちすると、フォントが変わった瞬間にクリック位置がずれて
/// 「タブが切り替わらない」という偽の失敗になる。
fn tab_centers(host: &mut UiHost<AppData>, app: &AppData) -> Vec<f32> {
    let measured = std::cell::RefCell::new(Vec::new());
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: W, height: H };
    host.frame_to_edits(app, &mut scene, screen, FrameInput::default(), |_app, ui| {
        let mut x = pane_rect().x;
        let mut out = Vec::new();
        for label in TAB_LABELS {
            let w = ui.measure_text(label, TAB_FONT) + TAB_PAD_X * 2.0;
            out.push(x + w * 0.5);
            x += w;
        }
        *measured.borrow_mut() = out;
    });
    measured.into_inner()
}

fn build_app() -> (AppData, UnboundedReceiver<PluginCommand>) {
    let (audio_tx, _audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let mut app = AppData::new(
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
    app.handle_event(AppEvent::SetTheme("dark".to_string()));
    (app, plugin_rx)
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

fn click_at(x: f32, y: f32) -> FrameInput {
    FrameInput {
        pointer: PointerFrame {
            pos: Some((x, y)),
            primary_just_released: true,
            ..PointerFrame::default()
        },
        ..FrameInput::default()
    }
}

/// 1 フレーム描いて RGBA を返す。`host` は呼び出し間で使い回すので、modal の開閉状態と
/// タブの選択状態がフレームを跨いで持続する (= 実機と同じ状態遷移になる)。
fn frame(
    renderer: &mut OffscreenRenderer,
    host: &mut UiHost<AppData>,
    app: &AppData,
    input: FrameInput,
    name: &str,
) -> Vec<u8> {
    if host.set_palette(app.theme.core.clone()) {
        host.invalidate_scene_cache();
    }
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: W, height: H };
    host.frame_to_edits(app, &mut scene, screen, input, |app, ui| {
        daw_gui::view::root::build_root(app, ui, screen);
    });
    let rgba = renderer.render_to_rgba(&scene).expect("offscreen render");

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/theme_shots");
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = image::save_buffer(
            dir.join(format!("about_{name}.png")),
            &rgba,
            W,
            H,
            image::ColorType::Rgba8,
        );
    }
    rgba
}

#[test]
fn バージョン情報は開くと描画され三つのタブすべてに本文がある() {
    let Ok(mut renderer) = OffscreenRenderer::new(W, H) else {
        eprintln!("skip about visual test: no GPU adapter/device");
        return;
    };

    // --- 閉じている状態 (baseline) ---
    let (app, _rx) = build_app();
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    let closed = frame(&mut renderer, &mut host, &app, FrameInput::default(), "closed");

    // --- Help > バージョン情報 で開く ---
    let (mut app, _rx2) = build_app();
    app.handle_event(AppEvent::ToggleAbout);
    assert!(app.ui_prefs.is_about_open, "ToggleAbout が状態を立てていない");
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    let overview = frame(&mut renderer, &mut host, &app, FrameInput::default(), "overview");

    let panel = panel_rect();
    let closed_panel = crop(&closed, panel);
    let overview_panel = crop(&overview, panel);
    assert_ne!(
        closed_panel.pixels, overview_panel.pixels,
        "開いても画面が変わらない = About が描画経路に乗っていない"
    );
    // 文字が乗っていればアンチエイリアスで色数が一気に増える。一様塗り (= 白紙の panel
    // だけ描かれて本文が出ていない) なら数十色しか出ない。
    assert!(
        overview_panel.unique_colors > 150,
        "概要タブが一様塗りに近い (unique_colors={})",
        overview_panel.unique_colors
    );

    // --- タブを切り替えると本文が入れ替わる ---
    let pane = pane_rect();
    let overview_pane = crop(&overview, pane);

    let centers = tab_centers(&mut host, &app);
    let ty = pane_rect().y - 16.0; // タブバーの中心 (pane の 32px 上)
    let _ = frame(&mut renderer, &mut host, &app, click_at(centers[1], ty), "third_party_click");
    let third_party = frame(&mut renderer, &mut host, &app, FrameInput::default(), "third_party");
    let third_party_pane = crop(&third_party, pane);
    assert_ne!(
        overview_pane.pixels, third_party_pane.pixels,
        "「第三者コンポーネント」タブに切り替えても本文が変わらない"
    );
    assert!(
        third_party_pane.unique_colors > 150,
        "第三者コンポーネントタブが一様塗りに近い (unique_colors={})",
        third_party_pane.unique_colors
    );

    let _ = frame(&mut renderer, &mut host, &app, click_at(centers[2], ty), "license_click");
    let license = frame(&mut renderer, &mut host, &app, FrameInput::default(), "license");
    let license_pane = crop(&license, pane);
    assert_ne!(
        third_party_pane.pixels, license_pane.pixels,
        "「ライセンス全文」タブに切り替えても本文が変わらない"
    );
    assert!(
        license_pane.unique_colors > 150,
        "ライセンス全文タブが一様塗りに近い (unique_colors={})",
        license_pane.unique_colors
    );
}
