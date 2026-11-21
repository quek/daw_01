//! r.md #156: 設定 window (左にカテゴリ、右にそのページ) を **実ポインタ**で駆動する回帰網と、
//! 窓の大きさを変えたときの配置の検査。
//!
//! 配置・重なり・文字切れは build / clippy / event 直叩きのテストを全部すり抜けるので、
//! `build_root` を合成 pointer で描いて click し、描かれた scene の中身 (矩形と文字) を
//! 窓の区画と突き合わせる。描画結果は `target/theme_shots/settings_*.png` に残す (目視用)。

use std::collections::HashSet;
use std::sync::Arc;

use common::app_dirs::AppDirs;
use common::audio_device::{
    AudioChannelInfo, AudioDeviceEntry, AudioDeviceInfo, AudioDeviceList, AudioDeviceRef, AudioDriverKind,
    AudioInputDevice, ChannelRange,
};
use common::protocol::{AudioCommand, PluginCommand};
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
    let (app, _audio_rx, plugin_rx) = build_app_with_audio(app_dirs);
    (app, plugin_rx)
}

/// daw_audio へ送ったコマンドも受け取れる版。
fn build_app_with_audio(
    app_dirs: Option<AppDirs>,
) -> (AppData, UnboundedReceiver<AudioCommand>, UnboundedReceiver<PluginCommand>) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
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
    (app, audio_rx, plugin_rx)
}

fn frame(host: &mut UiHost<AppData>, app: &mut AppData, p: PointerFrame) -> Scene {
    let mut scene = Scene::new();
    let input = FrameInput { pointer: p, ..FrameInput::default() };
    host.frame(app, &mut scene, SCREEN, input, |app, ui| {
        daw_gui::view::root::build_root(app, ui, SCREEN);
    });
    scene
}

fn hover(pos: (f32, f32)) -> PointerFrame {
    PointerFrame { pos: Some(pos), ..PointerFrame::default() }
}

fn click(host: &mut UiHost<AppData>, app: &mut AppData, pos: (f32, f32)) {
    frame(host, app, PointerFrame { primary_just_pressed: true, primary_pressed: true, ..hover(pos) });
    frame(host, app, PointerFrame { primary_just_released: true, ..hover(pos) });
    // release で積んだ edit を反映させる idle frame。
    frame(host, app, hover(pos));
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

/// 画面に描かれている `text` (前方一致) の文字の上の点。`popup` = dropdown の一覧の中から探す。
/// 座標を定数で写さず描画結果から取るので、ページの並びを変えても壊れない。
fn text_pos(scene: &Scene, text: &str, popup: bool) -> (f32, f32) {
    let found = if popup {
        scene.iter_popup_glyphs().find(|g| g.text.starts_with(text)).map(|g| (g.left, g.top))
    } else {
        scene.iter_glyphs().find(|g| g.text.starts_with(text)).map(|g| (g.left, g.top))
    };
    let (left, top) = found.unwrap_or_else(|| panic!("{text:?} が描かれていない (popup={popup})"));
    (left + 4.0, top + 6.0)
}

fn entry(id: &str, name: &str, is_default: bool) -> AudioDeviceEntry {
    AudioDeviceEntry { device: AudioDeviceRef { id: id.into(), name: name.into() }, is_default }
}

fn device_list() -> AudioDeviceList {
    AudioDeviceList {
        system_outputs: vec![
            entry("wasapi:spk", "スピーカー (Realtek(R) Audio)", true),
            entry("wasapi:hdmi", "DELL U2720Q (NVIDIA High Definition Audio)", false),
            entry("wasapi:usb", "USB DAC", false),
        ],
        system_inputs: vec![entry("wasapi:mic", "マイク (USB Audio Device)", true)],
        asio_drivers: vec![entry("{A}", "FL Studio ASIO", false), entry("{B}", "Realtek ASIO", false)],
    }
}

fn channels(count: u16, name: &str) -> Vec<AudioChannelInfo> {
    (0..count)
        .map(|index| AudioChannelInfo {
            index,
            name: if name.is_empty() { String::new() } else { format!("{name} {}", index + 1) },
        })
        .collect()
}

/// OS 標準の既定デバイスで動いている (レートもバッファもホストからは選べない・入力なし)。
fn system_state(app: &mut AppData) {
    app.audio_device.devices = Some(device_list());
    app.audio_device.info = Some(AudioDeviceInfo {
        driver: AudioDriverKind::System,
        output_name: "スピーカー (Realtek(R) Audio)".into(),
        sample_rate: 48_000,
        buffer_frames: 480,
        output_channels: channels(2, ""),
        output_latency_frames: 1_440,
        ..Default::default()
    });
}

/// ASIO のドライバーで動いている (ch に名前があり、レートもバッファも選べる)。
fn asio_state(app: &mut AppData) {
    app.audio_device.devices = Some(device_list());
    app.audio_device.config.driver = AudioDriverKind::Asio;
    app.audio_device.config.asio_driver = Some(AudioDeviceRef { id: "{A}".into(), name: "FL Studio ASIO".into() });
    app.audio_device.config.buffer_frames = Some(256);
    app.audio_device.info = Some(AudioDeviceInfo {
        driver: AudioDriverKind::Asio,
        output_name: "FL Studio ASIO".into(),
        input_name: Some("FL Studio ASIO".into()),
        sample_rate: 48_000,
        buffer_frames: 256,
        sample_rates: vec![44_100, 48_000, 88_200, 96_000],
        buffer_sizes: vec![64, 128, 256, 512, 1_024],
        output_channels: channels(8, "Analog Output"),
        input_channels: channels(8, "Analog Input"),
        output_first_channel: 2,
        active_inputs: ChannelRange { first: 0, count: 2 },
        input_latency_frames: 300,
        output_latency_frames: 560,
        has_control_panel: true,
        fallback_reason: None,
    });
}

/// 実物の「Realtek ASIO」が報告した形 (headless で取った値): ch の名前が長く、バッファの候補は今の値 1 つだけ。
fn asio_fixed_buffer_state(app: &mut AppData) {
    asio_state(app);
    app.audio_device.config.asio_driver = Some(AudioDeviceRef { id: "{B}".into(), name: "Realtek ASIO".into() });
    app.audio_device.config.buffer_frames = None;
    if let Some(info) = &mut app.audio_device.info {
        info.output_name = "Realtek ASIO".into();
        info.input_name = Some("Realtek ASIO".into());
        info.sample_rates = vec![44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
        info.buffer_frames = 144;
        info.buffer_sizes = vec![144];
        info.output_channels = channels(2, "HD Audio output");
        info.input_channels = channels(2, "HD Audio input");
        info.output_first_channel = 0;
        info.input_latency_frames = 144;
        info.output_latency_frames = 144;
    }
}

/// コントロールパネルを押したら、ドライバーが「設定画面は無い」と答えた後 (ボタンの場所に文が出る)。
fn asio_no_panel_state(app: &mut AppData) {
    asio_fixed_buffer_state(app);
    if let Some(info) = &mut app.audio_device.info {
        info.has_control_panel = false;
    }
}

/// ASIO のドライバーを選んだが開けず、OS 標準の既定デバイスへ戻っている。
fn fallback_state(app: &mut AppData) {
    system_state(app);
    app.audio_device.config.driver = AudioDriverKind::Asio;
    app.audio_device.config.asio_driver = Some(AudioDeviceRef { id: "{Z}".into(), name: "外した機材の ASIO".into() });
    app.audio_device.config.sample_rate = Some(96_000);
    if let Some(info) = &mut app.audio_device.info {
        info.fallback_reason = Some("ASIO ドライバー「外した機材の ASIO」が見つかりません".into());
    }
}

/// 設定 window に出しうる画面 1 つ: `(名前, カテゴリ, 状態の作り方)`。
type PageCase = (&'static str, SettingsCategory, fn(&mut AppData));

/// 設定 window に出しうる画面。
fn pages() -> Vec<PageCase> {
    vec![
        ("audio_disconnected", SettingsCategory::Audio, |_| {}),
        ("audio_system", SettingsCategory::Audio, system_state),
        ("audio_asio", SettingsCategory::Audio, asio_state),
        ("audio_asio_fixed", SettingsCategory::Audio, asio_fixed_buffer_state),
        ("audio_asio_no_panel", SettingsCategory::Audio, asio_no_panel_state),
        ("audio_fallback", SettingsCategory::Audio, fallback_state),
        ("voicevox", SettingsCategory::Voicevox, |_| {}),
        ("theme", SettingsCategory::Theme, |_| {}),
    ]
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

/// 「オーディオ」のページが画面に出るたびに、デバイスの一覧を engine へ取りに行く
/// (開く / カテゴリを選び直す / 開いたまま起動する)。ほかのページでは取りに行かない。
#[test]
fn showing_the_audio_page_asks_the_engine_for_the_device_list() {
    fn asked(rx: &mut UnboundedReceiver<AudioCommand>) -> usize {
        let mut count = 0;
        while let Ok(command) = rx.try_recv() {
            if matches!(command, AudioCommand::ListAudioDevices) {
                count += 1;
            }
        }
        count
    }
    let dir = tempfile::tempdir().unwrap();
    let dirs = AppDirs::under(dir.path());
    let (mut app, mut audio_rx, _rx) = build_app_with_audio(Some(dirs.clone()));
    assert_eq!(asked(&mut audio_rx), 0, "閉じている間は列挙しない");

    app.handle_event(AppEvent::ToggleSettings);
    assert_eq!(asked(&mut audio_rx), 1, "開いた");
    assert!(app.audio_device.listing, "返事が来るまでは取得中");

    app.handle_event(AppEvent::SetSettingsCategory(SettingsCategory::Theme));
    assert_eq!(asked(&mut audio_rx), 0, "ほかのページでは列挙しない");
    app.handle_event(AppEvent::SetSettingsCategory(SettingsCategory::Audio));
    assert_eq!(asked(&mut audio_rx), 1, "カテゴリを選び直した");

    let (_restarted, mut audio_rx, _rx) = build_app_with_audio(Some(dirs));
    assert_eq!(asked(&mut audio_rx), 1, "開いたまま起動した");
}

/// 「オーディオ」ページの dropdown は、選んだ瞬間に設定が替わって `app_config.json` に残る
/// (「適用」ボタンは無い)。一覧は window の中から開いて、項目を click で選べる。
#[test]
fn choosing_from_a_dropdown_switches_the_device_setting_at_once_and_saves_it() {
    let dir = tempfile::tempdir().unwrap();
    let dirs = AppDirs::under(dir.path());
    let (mut app, _rx) = build_app(Some(dirs.clone()));
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    app.handle_event(AppEvent::ToggleSettings);
    system_state(&mut app);
    assert_eq!(app.audio_device.config.system_input, AudioInputDevice::None);

    // 入力デバイス: 「なし」→「既定のデバイス (マイク …)」。
    let scene = frame(&mut host, &mut app, PointerFrame::default());
    click(&mut host, &mut app, text_pos(&scene, "なし", false));
    let scene = frame(&mut host, &mut app, PointerFrame::default());
    let item = text_pos(&scene, "既定のデバイス (マイク", true);
    click(&mut host, &mut app, item);
    assert_eq!(app.audio_device.config.system_input, AudioInputDevice::Default);
    assert_eq!(daw_gui::app_config::load(dirs.app_config()).audio_device.system_input, AudioInputDevice::Default);

    // 出力デバイス: 一覧のデバイスを名指しで選ぶ。
    let scene = frame(&mut host, &mut app, PointerFrame::default());
    click(&mut host, &mut app, text_pos(&scene, "既定のデバイス (スピーカー", false));
    let scene = frame(&mut host, &mut app, PointerFrame::default());
    click(&mut host, &mut app, text_pos(&scene, "USB DAC", true));
    let chosen = app.audio_device.config.system_output.as_ref().map(|d| d.id.as_str());
    assert_eq!(chosen, Some("wasapi:usb"), "デバイスは表示名ではなく id で覚える");
}

/// dropdown の一覧は window の外へはみ出す。はみ出した項目も選べて、その click は背景へ漏れない
/// (一覧が開いている間、背景の widget には pointer が見えない)。
#[test]
fn a_dropdown_list_that_leaves_the_window_is_clickable_and_does_not_leak_to_the_background() {
    let (mut app, _rx) = build_app(None);
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    app.handle_event(AppEvent::ToggleSettings);
    asio_state(&mut app);
    // 最小の高さ: 出力チャンネルの一覧 (4 組) は window の下端を越える。
    app.ui_prefs.settings_rect = Some(Rect { x: 300.0, y: 120.0, w: settings::MIN_W, h: settings::MIN_H });
    let window = settings::window_rect(&app, SCREEN_RECT);

    let scene = frame(&mut host, &mut app, PointerFrame::default());
    click(&mut host, &mut app, text_pos(&scene, "3 + 4", false));
    let scene = frame(&mut host, &mut app, PointerFrame::default());
    let item = text_pos(&scene, "7 + 8", true);
    assert!(!window.contains(item.0, item.1), "前提: 項目 {item:?} は window {window:?} の外");

    // 背景から見た pointer (window の予約の後・window を描く前)。
    let mut seen_by_background = Some((0.0, 0.0));
    let mut scene = Scene::new();
    let input = FrameInput { pointer: hover(item), ..FrameInput::default() };
    host.frame(&mut app, &mut scene, SCREEN, input, |app, ui| {
        settings::reserve(app, ui, SCREEN_RECT);
        seen_by_background = ui.pointer().pos;
        settings::draw(app, ui, SCREEN_RECT);
    });
    assert_eq!(seen_by_background, None, "一覧が開いている間、window の外でも背景は pointer を見ない");

    click(&mut host, &mut app, item);
    assert_eq!(app.audio_device.config.output_first_channel, Some(6), "window の外の項目を選べる");

    // 一覧が閉じたら、window の外は元どおり背景のもの。
    let mut scene = Scene::new();
    let input = FrameInput { pointer: hover(item), ..FrameInput::default() };
    host.frame(&mut app, &mut scene, SCREEN, input, |app, ui| {
        settings::reserve(app, ui, SCREEN_RECT);
        seen_by_background = ui.pointer().pos;
        settings::draw(app, ui, SCREEN_RECT);
    });
    assert_eq!(seen_by_background, Some(item));
}

/// 設定 window だけを描いた scene (背景を混ぜずに window の中身を調べる)。
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

/// 窓を最小・既定・大きめにしても、どのページでも
/// - カテゴリの列とページは重ならず、どちらも窓の中にある
/// - 描かれた矩形は窓の外へ出ない
/// - カテゴリ名は列の幅に収まり、ページの文字は横で切れず、項目名は省略されない
#[test]
fn the_sidebar_and_the_page_never_overlap_or_leave_the_window_at_any_size() {
    let sizes = [
        (settings::MIN_W, settings::MIN_H),
        (660.0, 520.0),
        (1100.0, 680.0),
        // 保存された rect が最小より小さくても、最小へ寄せて同じ条件を満たす。
        (200.0, 100.0),
    ];
    for (w, h) in sizes {
        for (name, category, prepare) in pages() {
            let (mut app, _rx) = build_app(None);
            let mut host: UiHost<AppData> = UiHost::no_redraw();
            prepare(&mut app);
            app.ui_prefs.settings_rect = Some(Rect { x: 90.0, y: 60.0, w, h });
            app.handle_event(AppEvent::SetSettingsCategory(category));
            app.handle_event(AppEvent::ToggleSettings);
            let rect = settings::window_rect(&app, SCREEN_RECT);
            let areas = settings::layout(rect);
            let ctx = format!("{w}x{h} {name}");

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

            // 文字: (文字列, 左端, 上端, サイズ, clip) を集めてから実測幅と突き合わせる。
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

/// デバイスの行の数は、種類を替えても・開くのに失敗しても変わらない
/// (知らせが出たり消えたりしても、その下の項目が動かない)。
#[test]
fn the_rows_below_the_device_do_not_move_when_the_device_state_changes() {
    let row_top = |prepare: fn(&mut AppData)| {
        let (mut app, _rx) = build_app(None);
        let mut host: UiHost<AppData> = UiHost::no_redraw();
        prepare(&mut app);
        app.ui_prefs.settings_rect = Some(Rect { x: 90.0, y: 60.0, w: 660.0, h: 520.0 });
        app.handle_event(AppEvent::ToggleSettings);
        let scene = settings_scene(&mut host, &app);
        scene.iter_glyphs().find(|g| &*g.text == "出力チャンネル").map(|g| g.top).expect("出力チャンネルの行")
    };
    let system = row_top(system_state);
    assert_eq!(row_top(asio_state), system, "ASIO");
    assert_eq!(row_top(fallback_state), system, "開けずに戻したとき (知らせが出る)");
    assert_eq!(row_top(|_| {}), system, "engine と繋がっていないとき");
}

/// 設定どおりに開けなかったときは、理由と、実際に開いているデバイスの名前がページに出る。
/// うまく行っているときは出ない。
#[test]
fn a_failed_device_shows_the_reason_and_what_is_actually_open() {
    let page_text = |prepare: fn(&mut AppData)| {
        let (mut app, _rx) = build_app(None);
        let mut host: UiHost<AppData> = UiHost::no_redraw();
        prepare(&mut app);
        app.ui_prefs.settings_rect = Some(Rect { x: 90.0, y: 60.0, w: 660.0, h: 520.0 });
        app.handle_event(AppEvent::ToggleSettings);
        // 折り返しで行に割れるので、描かれた順に繋いで読む。
        settings_scene(&mut host, &app).iter_glyphs().map(|g| g.text.to_string()).collect::<String>()
    };
    let failed = page_text(fallback_state);
    assert!(failed.contains("ASIO ドライバー「外した機材の ASIO」が見つかりません"), "{failed}");
    assert!(failed.contains("「スピーカー (Realtek(R) Audio)」で動いています"), "{failed}");
    // 選んだデバイスは選ばれたまま見える (設定は消さない)。
    assert!(failed.contains("外した機材の ASIO (見つかりません)"), "{failed}");

    assert!(!page_text(asio_state).contains("で動いています"));
    // 候補が今の値 1 つだけのバッファは選ぶものが無い: 読むだけの値と、変える場所を出す。
    assert!(page_text(asio_fixed_buffer_state).contains("144 サンプル (パネルで変更)"));
    assert!(page_text(|_| {}).contains("オーディオエンジンと繋がっていません"));

    // どの実機も開けず、engine だけが合成のデバイスで動いている: デバイス名ではなく「音は出ません」。
    let silent = page_text(|app| {
        fallback_state(app);
        if let Some(info) = &mut app.audio_device.info {
            info.driver = AudioDriverKind::Synthetic;
            info.output_name = "Synthetic (null)".into();
        }
    });
    assert!(silent.contains("が見つかりません — 音は出ません"), "{silent}");
    assert!(!silent.contains("Synthetic (null)"), "{silent}");
}

/// 設定 window を開いたルート view を GPU で描き、窓の矩形が一様塗りでないこと (= 中身が実際に
/// 描かれている) を確かめる。各ページ × 窓の大きさの画を `target/theme_shots/` に残す。
#[test]
fn the_window_renders_each_page_at_each_size() {
    let Ok(mut renderer) = OffscreenRenderer::new(W, H) else {
        eprintln!("skip settings visual test: no GPU adapter/device");
        return;
    };
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/theme_shots");
    let _ = std::fs::create_dir_all(&dir);

    for theme in ["dark", "light"] {
        for (label, w, h) in [("min", settings::MIN_W, settings::MIN_H), ("default", 660.0, 520.0)] {
            for (name, category, prepare) in pages() {
                let (mut app, _rx) = build_app(None);
                prepare(&mut app);
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
                    dir.join(format!("settings_{theme}_{name}_{label}.png")),
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
                    "{theme} {name} {label}: 窓の中が一様塗りに近い (色数 {})",
                    uniq.len()
                );
            }
        }
    }
}

/// dropdown の一覧は、floating window の中から開いても最前面に描かれる (window の外へはみ出した
/// 部分も、背景の上に出る)。
#[test]
fn an_open_dropdown_list_is_drawn_in_front_even_outside_the_window() {
    let Ok(mut renderer) = OffscreenRenderer::new(W, H) else {
        eprintln!("skip settings visual test: no GPU adapter/device");
        return;
    };
    let (mut app, _rx) = build_app(None);
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    app.handle_event(AppEvent::ToggleSettings);
    asio_state(&mut app);
    app.ui_prefs.settings_rect = Some(Rect { x: 300.0, y: 120.0, w: settings::MIN_W, h: settings::MIN_H });
    let window = settings::window_rect(&app, SCREEN_RECT);

    frame(&mut host, &mut app, PointerFrame::default());
    let closed_scene = frame(&mut host, &mut app, PointerFrame::default());
    let closed = renderer.render_to_rgba(&closed_scene).expect("offscreen render");
    click(&mut host, &mut app, text_pos(&closed_scene, "3 + 4", false));
    let open_scene = frame(&mut host, &mut app, PointerFrame::default());
    let open = renderer.render_to_rgba(&open_scene).expect("offscreen render");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/theme_shots");
    let _ = std::fs::create_dir_all(&dir);
    let _ = image::save_buffer(dir.join("settings_dropdown_open.png"), &open, W, H, image::ColorType::Rgba8);

    // window の外にある項目 (「7 + 8」) の文字の周りが、閉じているときの画と違う = 背景の上に描かれた。
    let (x, y) = text_pos(&open_scene, "7 + 8", true);
    assert!(!window.contains(x, y), "前提: 項目は window の外");
    let mut changed = 0;
    for py in (y as u32 - 6)..(y as u32 + 10) {
        for px in (x as u32 - 4)..(x as u32 + 60) {
            let i = ((py * W + px) * 4) as usize;
            if open[i..i + 3] != closed[i..i + 3] {
                changed += 1;
            }
        }
    }
    assert!(changed > 200, "window の外の一覧が描かれていない (変わった画素 {changed})");
}
