// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! r.md #48: テーマの **visual regression**。
//!
//! 色は build / test / clippy を全部すり抜ける (CLAUDE.md「Visual regression smoke test」)。
//! ここでは実際にルート view を 1 フレーム組んで GPU でオフスクリーン描画し、
//!
//! 1. **ウィンドウの clear 色がパレットに追従する** (旧実装は `Scene::DEFAULT_CLEAR` の
//!    ダーク固定で、daw_gui が一度も上書きしていなかった = ライトにすると panel の隙間だけ
//!    黒く残る)、
//! 2. ダークとライトで画面全体の明度が実際に反転する、
//! 3. どちらのテーマも「一様塗り」 に潰れていない (= 描画が壊れていない)
//!
//! を pixel で検証する。描画結果は `target/theme_shots/<id>.png` に残すので、
//! 目視確認にも使える。
//!
//! GPU adapter が無い環境では `OffscreenRenderer::new` が `Err` を返すので graceful skip。

use std::collections::HashSet;
use std::sync::Arc;

use common::app_dirs::AppDirs;
use common::protocol::PluginCommand;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_ui_core::{FrameInput, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{OffscreenRenderer, Scene};

const W: u32 = 960;
const H: u32 = 600;

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

/// sRGB バイトから WCAG relative luminance。
///
/// ここは **本当に sRGB エンコードされた値** を扱う唯一の経路 (GPU readback は
/// `Rgba8UnormSrgb` = エンコード済みバイト)。 `relative_luminance` はパレット同様の
/// linear 入力を前提にしているので、 呼び出し側で `srgb_to_linear` を通す。
fn luminance(r: u8, g: u8, b: u8) -> f32 {
    let lin = |v: u8| daw_ui_core::theme::srgb_to_linear(f32::from(v) / 255.0);
    daw_ui_core::color::relative_luminance(lin(r), lin(g), lin(b))
}

struct Shot {
    /// frame 末の `Scene::clear_color` (= `UiHost` がパレットから書いた window 背景)。
    clear_color: wgpu::Color,
    mean_luminance: f32,
    unique_colors: usize,
}

/// 極性の危険地帯 (ユーザー着色クリップの上の名前 / 選択リング / muted ハッチ) を
/// 画に出すための最小の song。**明るいクリップと暗いクリップの両方**を置くのが要点で、
/// 片方だけだと `ink_for` の 2 択のうち 1 つしか描かれない
/// (memory `feedback_ui_indicator_contrast_on_variable_bg`)。
fn populate(app: &mut AppData) {
    for _ in 0..3 {
        app.handle_event(AppEvent::AddInstrumentTrack);
    }
    let ids: Vec<u32> = app.song_doc.song().tracks.iter().map(|t| t.id).collect();
    for (i, id) in ids.iter().enumerate() {
        app.handle_event(AppEvent::CreateClip { track: *id, start_beat: (i as f64) * 4.0 });
    }
    app.edit_song(|song| {
        // 0 = 明るいクリップ (暗インクが乗るべき)、1 = 暗いクリップ (明インク)、2 = muted。
        let colors = [Some([0.93, 0.74, 0.28]), Some([0.16, 0.22, 0.40]), Some([0.55, 0.30, 0.62])];
        for (t, c) in song.tracks.iter_mut().zip(colors) {
            t.color = c;
            if let Some(clip) = t.clips.first_mut() {
                clip.color = c;
                clip.length_beats = 6.0;
            }
        }
        if let Some(t) = song.tracks.get_mut(2)
            && let Some(clip) = t.clips.first_mut()
        {
            clip.muted = true;
        }
    });
    // 1 本目のクリップを選択して選択リングを描かせる。
    if let Some(t) = app.song_doc.song().tracks.first()
        && let Some(c) = t.clips.first()
    {
        let key = common::model::ClipKey { track_id: t.id, clip_id: c.id };
        app.selection.selected_clips = vec![key];
    }
}

/// `theme_id` のテーマでルート view を 1 フレーム組み、オフスクリーン描画して返す。
/// `settings_open` で設定 window を出した状態も撮れる (新規 UI の目視確認用)。
fn render_theme(renderer: &mut OffscreenRenderer, theme_id: &str, settings_open: bool) -> Shot {
    let (mut app, _rx) = build_app(None);
    app.handle_event(AppEvent::SetTheme(theme_id.to_string()));
    assert_eq!(app.theme.id, theme_id, "テーマが適用されている");
    populate(&mut app);
    if settings_open {
        // イベント経由で開く (テーマ一覧のキャッシュもここで埋まる — フラグ直書きだと
        // 一覧が空のまま描かれて「window は出るが中身が無い」 のを見逃す)。
        app.handle_event(AppEvent::ToggleSettings);
        assert!(app.ui_prefs.settings_open);
    }
    let shot_name =
        if settings_open { format!("{theme_id}_settings") } else { theme_id.to_string() };

    let mut host: UiHost<AppData> = UiHost::no_redraw();
    // runner と同じ配線: パレットを host に流し込む (変化したら描画キャッシュを捨てる)。
    if host.set_palette(app.theme.core.clone()) {
        host.invalidate_scene_cache();
    }

    let mut scene = Scene::new();
    let screen = PhysicalSize { width: W, height: H };
    host.frame_to_edits(&app, &mut scene, screen, FrameInput::default(), |app, ui| {
        daw_gui::view::root::build_root(app, ui, screen);
    });

    let rgba = renderer.render_to_rgba(&scene).expect("offscreen render");
    let mut sum = 0.0_f32;
    let mut uniq: HashSet<u32> = HashSet::new();
    for px in rgba.chunks_exact(4) {
        sum += luminance(px[0], px[1], px[2]);
        uniq.insert(u32::from_be_bytes([px[0], px[1], px[2], 0]));
    }
    let n = (rgba.len() / 4) as f32;

    // 目視用に残す (失敗時の切り分けにも使う)。
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/theme_shots");
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = image::save_buffer(
            dir.join(format!("{shot_name}.png")),
            &rgba,
            W,
            H,
            image::ColorType::Rgba8,
        );
    }

    Shot { clear_color: scene.clear_color, mean_luminance: sum / n, unique_colors: uniq.len() }
}

#[test]
fn dark_and_light_render_with_the_expected_polarity_and_clear_color() {
    let Ok(mut renderer) = OffscreenRenderer::new(W, H) else {
        eprintln!("skip theme visual test: no GPU adapter/device");
        return;
    };

    let dark = render_theme(&mut renderer, "dark", false);
    let light = render_theme(&mut renderer, "light", false);
    // 設定 window を開いた状態も撮っておく (新規 UI の目視確認用。assert はしない)。
    let _ = render_theme(&mut renderer, "dark", true);
    let _ = render_theme(&mut renderer, "light", true);

    // 1. ウィンドウの clear 色がパレットに追従する。旧実装は `Scene::DEFAULT_CLEAR`
    //    (ダーク固定) のままで daw_gui が一度も上書きしていなかったので、ライトにすると
    //    panel の隙間・レイアウト余白だけが黒く残っていた。
    for (id, shot) in [("dark", &dark), ("light", &light)] {
        let want = daw_gui::theme::Theme::builtin(id).unwrap().core.window_bg.to_wgpu();
        assert!(
            (shot.clear_color.r - want.r).abs() < 1e-6
                && (shot.clear_color.g - want.g).abs() < 1e-6
                && (shot.clear_color.b - want.b).abs() < 1e-6,
            "theme={id}: clear_color {:?} が window_bg {want:?} と一致しない",
            shot.clear_color,
        );
    }
    assert_ne!(
        (dark.clear_color.r, dark.clear_color.g, dark.clear_color.b),
        (light.clear_color.r, light.clear_color.g, light.clear_color.b),
        "テーマで clear 色が変わる",
    );

    // 2. 明度が実際に反転している。
    assert!(
        light.mean_luminance > dark.mean_luminance + 0.30,
        "ライトはダークより明確に明るい: dark={} light={}",
        dark.mean_luminance,
        light.mean_luminance,
    );
    assert!(dark.mean_luminance < 0.20, "ダークは暗い: {}", dark.mean_luminance);
    assert!(light.mean_luminance > 0.55, "ライトは明るい: {}", light.mean_luminance);

    // 3. どちらも一様塗りに潰れていない (= widget が実際に描かれている)。
    assert!(dark.unique_colors > 200, "dark unique_colors={}", dark.unique_colors);
    assert!(light.unique_colors > 200, "light unique_colors={}", light.unique_colors);
}
