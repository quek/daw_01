//! r.md #94: セッションビュー (ランチャー帯) の video / image クリップのセルに、
//! アレンジのクリップと同じサムネイルが **実際に描かれる** ことの visual regression。
//!
//! 「view にサムネイルが載っている」 だけでは足りない (載っていても描画側が捨てれば
//! 無地のまま = build / test / clippy を全部すり抜ける)。 ここでは 1 色に塗った
//! texture を image source として上げ、ルート view を 1 フレーム組んでオフスクリーン
//! 描画し、
//!
//! 1. **セッションにしか居ない** 画像クリップでも、その texture の色が画面に出る
//! 2. ランチャー帯を畳む (アレンジのみ) と同じ song でその色が **消える**
//!    (= 1 の色が帯のセルから出ていた対照)
//!
//! を pixel で検証する。描画結果は `target/launcher_shots/*.png` に残す (目視用)。
//!
//! GPU adapter が無い環境では `OffscreenRenderer::new` が `Err` を返すので graceful skip。

use std::sync::Arc;

use common::model::{
    Clip, ClipContent, ImageContent, ImageEvent, ImageSource, ImageSourcePath, LaunchSettings,
    LauncherLayout, Scene, SessionClip, Track,
};
use tokio::sync::mpsc;

use daw_gui::app::AppData;
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_ui_core::{FrameInput, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{OffscreenRenderer, Scene as RenderScene, TextureHandle};

const W: u32 = 960;
const H: u32 = 600;

/// サムネイルに使う 1 色 (マゼンタ)。 UI のどのパレットにも無い色なので、
/// 画面に出ていれば texture 由来だと断定できる。
const KEY_RGB: [u8; 3] = [255, 0, 255];
const TEX_W: u32 = 16;
const TEX_H: u32 = 9;

fn build_app() -> AppData {
    let (audio_tx, _audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, _plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None,
        event_dispatcher,
        job_dispatcher,
        None,
        None,
        48_000,
    )
}

/// 画像 source 1 つ + それを指す content 1 つ + **セッションだけ** に置いたクリップ 1 つ。
/// `with_arrangement_clip` で同じ content のクリップをアレンジにも置く (目視で並べる用)。
fn populate(app: &mut AppData, texture: TextureHandle, with_arrangement_clip: bool) {
    let source_id = app
        .edit_song(|song| {
            let source_id = song.alloc_image_source_id();
            song.media.image_sources.insert(
                source_id,
                ImageSource {
                    path: ImageSourcePath::Absolute("key.png".into()),
                    name: "key.png".into(),
                    width: TEX_W,
                    height: TEX_H,
                    format: "Png".into(),
                },
            );
            let content_id = song.alloc_content_id();
            song.clip_contents.insert(
                content_id,
                ClipContent::Image(ImageContent {
                    events: vec![ImageEvent {
                        source_id,
                        event_start_in_clip_beats: 0.0,
                        event_length_beats: 4.0,
                        ..ImageEvent::default()
                    }],
                }),
            );
            let scene_id = song.alloc_scene_id();
            song.scenes.push(Scene::new(scene_id));
            let track_id = song.alloc_track_id();
            let mut track = Track { id: track_id, name: "img".into(), ..Track::default() };
            let clip_id = track.alloc_clip_id();
            track.session_clips.push(SessionClip {
                scene_id,
                clip: Clip { id: clip_id, length_beats: 4.0, content_id, ..Clip::default() },
                launch: LaunchSettings::default(),
            });
            if with_arrangement_clip {
                let clip_id = track.alloc_clip_id();
                track.clips.push(Clip {
                    id: clip_id,
                    start_beat: 0.0,
                    length_beats: 4.0,
                    content_id,
                    ..Clip::default()
                });
            }
            song.tracks.push(track);
            source_id
        })
        .expect("編集できる");
    // decode 完了後に runner が入れるのと同じ場所 (main renderer の handle)。
    app.ui_ephemeral.image_texture_cache.insert(source_id, texture);
}

/// ルート view を 1 フレーム組んでオフスクリーン描画し、KEY_RGB の pixel 数を返す。
fn render_and_count(renderer: &mut OffscreenRenderer, app: &AppData, shot_name: &str) -> usize {
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    if host.set_palette(app.theme.core.clone()) {
        host.invalidate_scene_cache();
    }
    let mut scene = RenderScene::new();
    let screen = PhysicalSize { width: W, height: H };
    host.frame_to_edits(app, &mut scene, screen, FrameInput::default(), |app, ui| {
        daw_gui::view::root::build_root(app, ui, screen);
    });
    let rgba = renderer.render_to_rgba(&scene).expect("offscreen render");

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/launcher_shots");
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = image::save_buffer(
            dir.join(format!("{shot_name}.png")),
            &rgba,
            W,
            H,
            image::ColorType::Rgba8,
        );
    }
    // sRGB 往復 (texture Rgba8UnormSrgb → render target Srgb) で 255 / 0 は保たれるが、
    // 丸めの 1 段ぶんだけ許容する。
    rgba.as_chunks::<4>()
        .0
        .iter()
        .filter(|px| {
            px[0] >= KEY_RGB[0] - 2 && px[1] <= KEY_RGB[1] + 2 && px[2] >= KEY_RGB[2] - 2
        })
        .count()
}

#[test]
fn session_only_image_clip_cell_shows_its_thumbnail() {
    let Ok(mut renderer) = OffscreenRenderer::new(W, H) else {
        eprintln!("skip launcher thumbnail visual test: no GPU adapter/device");
        return;
    };
    let texture = renderer.create_texture(TEX_W, TEX_H);
    let mut pixels = Vec::with_capacity((TEX_W * TEX_H * 4) as usize);
    for _ in 0..TEX_W * TEX_H {
        pixels.extend_from_slice(&[KEY_RGB[0], KEY_RGB[1], KEY_RGB[2], 255]);
    }
    renderer.upload_texture_rgba(texture, &pixels);

    // 1. セッションだけに居るクリップのセルにサムネイルが出る。
    let mut app = build_app();
    populate(&mut app, texture, false);
    assert_eq!(app.ui_prefs.launcher_layout, LauncherLayout::Both, "既定は帯とレーンの両方");
    let with_pane = render_and_count(&mut renderer, &app, "session_only_both");

    // 2. 対照: 同じ song で帯を畳むと消える (= 1 の色は帯のセルから出ていた)。
    app.ui_prefs.launcher_layout = LauncherLayout::ArrangerOnly;
    let without_pane = render_and_count(&mut renderer, &app, "session_only_arranger_only");

    assert_eq!(without_pane, 0, "帯を畳めば texture 色は 1px も出ない (アレンジに同 content は無い)");
    assert!(
        with_pane > 0,
        "セッションだけの画像クリップのセルにサムネイルが描かれていない (r.md #94)"
    );

    // 目視用: アレンジにも同じ content を置いた画 (assert はしない)。
    let mut both = build_app();
    populate(&mut both, texture, true);
    let _ = render_and_count(&mut renderer, &both, "session_and_arrangement");
}
