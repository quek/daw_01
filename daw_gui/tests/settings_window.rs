//! r.md #156: 設定 window (左にカテゴリ、右にそのページ) を **実ポインタ**で駆動する回帰網と、
//! 窓の大きさを変えたときの配置の検査。
//!
//! 配置・重なり・文字切れは build / clippy / event 直叩きのテストを全部すり抜けるので、
//! `build_root` を合成 pointer で描いて click し、描かれた scene の中身 (矩形と文字) を
//! 窓の区画と突き合わせる。描画結果は `target/theme_shots/settings_*.png` に残す (目視用)。

use std::collections::HashSet;
use std::sync::Arc;

use common::app_dirs::AppDirs;
use common::protocol::PluginCommand;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_gui::state::SettingsCategory;
use daw_gui::view::settings;
use daw_ui_core::{FrameInput, PointerFrame, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{OffscreenRenderer, Rect, Scene};

const W: u32 = 1280;
const H: u32 = 800;
const SCREEN: PhysicalSize = PhysicalSize { width: W, height: H };
const SCREEN_RECT: Rect = Rect { x: 0.0, y: 0.0, w: W as f32, h: H as f32 };

fn build_app(app_dirs: Option<AppDirs>) -> (AppData, UnboundedReceiver<PluginCommand>) {
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
        app_dirs,
        48_000,
    );
    (app, plugin_rx)
}

fn frame(host: &mut UiHost<AppData>, app: &mut AppData, p: PointerFrame) -> Scene {
    let mut scene = Scene::new();
    let input = FrameInput { pointer: p, ..FrameInput::default() };
    host.frame(app, &mut scene, SCREEN, input, |app, ui| {
        daw_gui::view::root::build_root(app, ui, SCREEN);
    });
    scene
}

fn click(host: &mut UiHost<AppData>, app: &mut AppData, (x, y): (f32, f32)) {
    let at = |f: fn(&mut PointerFrame)| {
        let mut p = PointerFrame { pos: Some((x, y)), ..PointerFrame::default() };
        f(&mut p);
        p
    };
    frame(host, app, at(|p| {
        p.primary_just_pressed = true;
        p.primary_pressed = true;
    }));
    frame(host, app, at(|p| p.primary_just_released = true));
    // release で積んだ edit を反映させる idle frame。
    frame(host, app, at(|_| {}));
}

fn center(r: Rect) -> (f32, f32) {
    (r.x + r.w * 0.5, r.y + r.h * 0.5)
}

/// 左の一覧の `category` の行の中心 (今の window rect から求める)。
fn category_center(app: &AppData, category: SettingsCategory) -> (f32, f32) {
    let areas = settings::layout(settings::window_rect(app, SCREEN_RECT));
    let index = SettingsCategory::ALL.iter().position(|c| *c == category).unwrap();
    center(settings::category_row_rect(areas.sidebar, index))
}

/// 左のカテゴリを click するとページが替わり、選んだカテゴリが `app_config.json` に残って
/// 再起動後も同じページが開く。
#[test]
fn clicking_a_category_switches_the_page_and_persists_it() {
    let dir = tempfile::tempdir().unwrap();
    let dirs = AppDirs::under(dir.path());
    let (mut app, _rx) = build_app(Some(dirs.clone()));
    let mut host: UiHost<AppData> = UiHost::no_redraw();

    app.handle_event(AppEvent::ToggleSettings);
    assert!(app.ui_prefs.settings_open);
    assert_eq!(app.ui_prefs.settings_category, SettingsCategory::Audio, "既定は「オーディオ」");
    frame(&mut host, &mut app, PointerFrame::default());

    let theme = category_center(&app, SettingsCategory::Theme);
    click(&mut host, &mut app, theme);
    assert_eq!(app.ui_prefs.settings_category, SettingsCategory::Theme);
    assert!(
        !app.ui_ephemeral.available_themes.is_empty(),
        "テーマのページへ替えたらテーマ一覧が取れている (空のまま描かない)"
    );

    let saved = daw_gui::app_config::load(dirs.app_config());
    assert_eq!(saved.settings_category, "theme");
    assert!(saved.settings_open);

    // 再起動: 開いたまま、同じカテゴリで戻る。
    let (restarted, _rx) = build_app(Some(dirs));
    assert!(restarted.ui_prefs.settings_open);
    assert_eq!(restarted.ui_prefs.settings_category, SettingsCategory::Theme);
    assert!(!restarted.ui_ephemeral.available_themes.is_empty(), "開いたまま起動しても一覧がある");

    // 別のカテゴリへも替えられる。
    let vox = category_center(&app, SettingsCategory::Voicevox);
    click(&mut host, &mut app, vox);
    assert_eq!(app.ui_prefs.settings_category, SettingsCategory::Voicevox);
}

/// 知らないカテゴリ名 (新しい版が書いた設定ファイル) は既定のカテゴリへ倒すだけで、
/// ほかの設定を巻き込んで捨てない。
#[test]
fn an_unknown_saved_category_falls_back_without_dropping_other_settings() {
    let dir = tempfile::tempdir().unwrap();
    let dirs = AppDirs::under(dir.path());
    std::fs::create_dir_all(dir.path()).unwrap();
    std::fs::write(
        dirs.app_config(),
        r#"{"settings_category": "midi", "settings_open": true, "render_ahead": false}"#,
    )
    .unwrap();
    let (app, _rx) = build_app(Some(dirs));
    assert_eq!(app.ui_prefs.settings_category, SettingsCategory::Audio);
    assert!(app.ui_prefs.settings_open);
    assert!(!app.ui_prefs.render_ahead, "同じファイルのほかの設定は読めている");
}

/// 設定 window だけを 1 フレーム描いた scene (背景を混ぜずに window の中身を調べる)。
fn settings_scene(host: &mut UiHost<AppData>, app: &AppData) -> Scene {
    let mut scene = Scene::new();
    // ページの高さは前フレームの実測なので、2 フレーム描いて落ち着かせる。
    for _ in 0..2 {
        scene.clear();
        host.frame_to_edits(app, &mut scene, SCREEN, FrameInput::default(), |app, ui| {
            settings::draw(app, ui, SCREEN_RECT);
        });
    }
    scene
}

fn contains(outer: Rect, inner: Rect) -> bool {
    const EPS: f32 = 0.5;
    inner.x >= outer.x - EPS
        && inner.y >= outer.y - EPS
        && inner.x + inner.w <= outer.x + outer.w + EPS
        && inner.y + inner.h <= outer.y + outer.h + EPS
}

/// 窓を最小・既定・大きめにしても、どのカテゴリでも
/// - カテゴリの列とページは重ならず、どちらも窓の中にある
/// - 描かれた矩形は窓の外へ出ない
/// - カテゴリ名は列の幅に収まり、ページの文字は横で切れない
#[test]
fn the_sidebar_and_the_page_never_overlap_or_leave_the_window_at_any_size() {
    let (mut app, _rx) = build_app(None);
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    app.handle_event(AppEvent::ToggleSettings);

    let sizes = [
        (settings::MIN_W, settings::MIN_H),
        (660.0, 480.0),
        (1100.0, 680.0),
        // 保存された rect が最小より小さくても、最小へ寄せて同じ条件を満たす。
        (200.0, 100.0),
    ];
    for (w, h) in sizes {
        for category in SettingsCategory::ALL {
            app.ui_prefs.settings_rect = Some(Rect { x: 90.0, y: 60.0, w, h });
            app.handle_event(AppEvent::SetSettingsCategory(category));
            let rect = settings::window_rect(&app, SCREEN_RECT);
            let areas = settings::layout(rect);
            let ctx = format!("{w}x{h} {category:?}");

            assert!(contains(rect, areas.sidebar) && contains(rect, areas.page), "{ctx}: 区画は窓の中");
            assert!(
                areas.sidebar.x + areas.sidebar.w <= areas.page.x,
                "{ctx}: カテゴリの列とページは重ならない"
            );
            assert!(areas.page.w >= 300.0 && areas.page.h >= 180.0, "{ctx}: ページが潰れていない {:?}", areas.page);

            let scene = settings_scene(&mut host, &app);
            for r in scene.iter_rects() {
                let drawn = r.clip_rect.map_or(r.rect, |c| r.rect.intersect(c));
                if drawn.w <= 0.0 || drawn.h <= 0.0 {
                    continue;
                }
                assert!(contains(rect, drawn), "{ctx}: 矩形 {drawn:?} が窓 {rect:?} の外");
            }

            // 文字: (文字列, 左端, サイズ, clip) を集めてから実測幅と突き合わせる。
            let glyphs: Vec<(String, f32, f32, f32, Option<Rect>)> = scene
                .iter_glyphs()
                .map(|g| (g.text.to_string(), g.left, g.top, g.font_size, g.clip_rect))
                .collect();
            assert!(glyphs.len() >= 5, "{ctx}: 窓の中身が描かれている ({} 個の文字列)", glyphs.len());
            let mut widths = Vec::new();
            host.frame_to_edits(&app, &mut Scene::new(), SCREEN, FrameInput::default(), |_, ui| {
                widths = glyphs.iter().map(|(t, _, _, size, _)| ui.measure_text(t, *size)).collect();
            });
            for ((text, left, top, _, clip), width) in glyphs.iter().zip(widths) {
                let right = left + width;
                // 縦にスクロールで切れた行は対象外 (ページは縦に scroll する)。
                if clip.is_some_and(|c| *top < c.y || *top + 10.0 > c.y + c.h) {
                    continue;
                }
                if *left < areas.page.x {
                    // タイトルバーとカテゴリの列。
                    let limit = if *top < areas.sidebar.y { rect.x + rect.w } else { areas.sidebar.x + areas.sidebar.w };
                    assert!(right <= limit + 0.5, "{ctx}: {text:?} が列の幅を超える (right={right} limit={limit})");
                } else if *top >= areas.page.y {
                    let limit = clip.map_or(areas.page.x + areas.page.w, |c| c.x + c.w);
                    assert!(right <= limit + 0.5, "{ctx}: ページの {text:?} が横で切れる (right={right} limit={limit})");
                    // 項目名の列 (ページの左端から始まる文字) は省略記号で潰れない。
                    if *left < areas.page.x + 20.0 {
                        assert!(
                            !text.ends_with('…') && !text.ends_with("..."),
                            "{ctx}: 項目名 {text:?} が省略されている"
                        );
                    }
                }
            }
        }
    }
}

/// 設定 window を開いたルート view を GPU で描き、窓の矩形が一様塗りでないこと (= 中身が実際に
/// 描かれている) を確かめる。各カテゴリ × 窓の大きさの画を `target/theme_shots/` に残す。
#[test]
fn the_window_renders_each_category_at_each_size() {
    let Ok(mut renderer) = OffscreenRenderer::new(W, H) else {
        eprintln!("skip settings visual test: no GPU adapter/device");
        return;
    };
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/theme_shots");
    let _ = std::fs::create_dir_all(&dir);

    for theme in ["dark", "light"] {
        for (label, w, h) in [("min", settings::MIN_W, settings::MIN_H), ("default", 660.0, 480.0)] {
            for category in SettingsCategory::ALL {
                let (mut app, _rx) = build_app(None);
                app.handle_event(AppEvent::SetTheme(theme.to_string()));
                app.ui_prefs.settings_rect = Some(Rect { x: 300.0, y: 120.0, w, h });
                app.handle_event(AppEvent::SetSettingsCategory(category));
                app.handle_event(AppEvent::ToggleSettings);

                let mut host: UiHost<AppData> = UiHost::no_redraw();
                if host.set_palette(app.theme.core.clone()) {
                    host.invalidate_scene_cache();
                }
                frame(&mut host, &mut app, PointerFrame::default());
                let scene = frame(&mut host, &mut app, PointerFrame::default());
                let rgba = renderer.render_to_rgba(&scene).expect("offscreen render");
                let _ = image::save_buffer(
                    dir.join(format!("settings_{theme}_{}_{label}.png", category.id())),
                    &rgba,
                    W,
                    H,
                    image::ColorType::Rgba8,
                );

                let rect = settings::window_rect(&app, SCREEN_RECT);
                let mut uniq: HashSet<[u8; 3]> = HashSet::new();
                for y in rect.y as u32..(rect.y + rect.h) as u32 {
                    for x in rect.x as u32..(rect.x + rect.w) as u32 {
                        let i = ((y * W + x) * 4) as usize;
                        uniq.insert([rgba[i], rgba[i + 1], rgba[i + 2]]);
                    }
                }
                assert!(
                    uniq.len() > 40,
                    "{theme} {category:?} {label}: 窓の中が一様塗りに近い (色数 {})",
                    uniq.len()
                );
            }
        }
    }
}
