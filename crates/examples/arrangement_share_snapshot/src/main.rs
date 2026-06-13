//! 共有クリップの描画 offscreen visual / pixel verify (daw_01 #086 / Phase 114)。
//!
//! M14 Phase 114 (daw_01 #086): clip 塗りは `clip.color` が唯一の source。 `share_group_color` は
//! リンク識別 (⇌ glyph + #068 hover 強調) 専用になり、 fill / border を一切上書きしない。 これを
//! 1 枚で確認する:
//! - Audio share clip ×2 (同 group・同 color teal、 in_active_group=true): color fill + ⇌ +
//!   neutral 強調リング (= 「同じ仲間」 を明るい中立色で示す、 #068 を hue → neutral 化)。
//! - Audio plain clip (color blue、 非 share): color fill のみ (⇌ なし)。
//! - Video Text clip (thumbnail なし共有、 color purple): color fill + ⇌ (hue fill 撤去後も share マーク)。
//! - Video 実 video clip (thumbnail あり共有): letterbox は video_clip_loading のまま + ⇌ (thumbnail 不変)。
//! - Video 非 share・非 color clip: video_clip_loading 一色 (回帰確認)。
//! - selected な video share clip (color orange): selection 黄 + ⇌ (selection 最優先 / #022)。
//!
//! 実行: `cargo run --bin arrangement_share_snapshot`
//!   → `<workspace>/target/arrangement_share_snapshot.png`。
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::error::Error;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use daw_ui_core::{
    ArrangementClip, ArrangementStyle, ArrangementTrack, ArrangementView, ClipKey, Edit,
    FrameInput, SnapConfig, TrackKind, UiHost,
};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, OffscreenRenderer, Rect, Scene, TextureHandle};

const SHARE_HUE: f32 = 0.33; // green: linked Text + video が同 group
const AUDIO_HUE: f32 = 0.60; // blue: 別 group の audio share
const TEAL: Color = Color { r: 0.20, g: 0.55, b: 0.55, a: 1.0 }; // audio share group の user color

fn clip(id: u32, start: f64, len: f64, name: &str) -> ArrangementClip {
    ArrangementClip {
        id,
        start_beat: start,
        len_beats: len,
        name: Arc::from(name),
        color: None,
        share_group_color: None,
        audio_edit: None,
        thumbnail: None,
        in_active_group: false,
    }
}

fn shared(mut c: ArrangementClip, hue: f32) -> ArrangementClip {
    c.share_group_color = Some(hue);
    c
}

fn colored(mut c: ArrangementClip, color: Color) -> ArrangementClip {
    c.color = Some(color);
    c
}

fn track(id: u32, name: &str, kind: TrackKind, clips: Vec<ArrangementClip>) -> ArrangementTrack {
    ArrangementTrack {
        id,
        name: Arc::from(name),
        muted: false,
        solo: false,
        armed: false,
        clips,
        volume: 1.0,
        parent_id: None,
        depth: 0,
        automation_lanes_collapsed: true,
        automation_lanes: Vec::new(),
        collapsed: false,
        row_h: None,
        kind,
        color: None,
    }
}

/// オレンジ寄りグラデの RGBA thumbnail (緑 hue border が thumbnail を隠さないことの確認用)。
fn make_thumbnail_rgba(w: u32, h: u32) -> Vec<u8> {
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let cx = (x * 255 / w.max(1)) as u8;
            let cy = (y * 255 / h.max(1)) as u8;
            data.extend_from_slice(&[220u8.saturating_sub(cy / 2), cx / 2 + 60, 40, 255]);
        }
    }
    data
}

fn view(width: u32, height: u32) -> ArrangementView {
    let _ = (width, height);
    ArrangementView {
        start_beat: 0.0,
        len_beats: 8.0,
        track_top: 0.0,
        tracks_visible: 6.0,
        track_row_h: 56.0,
        header_w: 0.0,
        ruler_h: 0.0,
        playhead_beat: None,
        loop_range: None,
        data_generation: 0,
        bpm: 120.0,
        time_sig: (4, 4),
        snap: SnapConfig::OFF,
        arranger_lane_h: 0.0,
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let width: u32 = 900;
    let height: u32 = 220;

    let mut renderer = OffscreenRenderer::new(width, height)?;
    // 実 video clip 用の thumbnail texture を 1 枚 upload。
    let (tw, thh) = (160u32, 90u32);
    let thumb: TextureHandle = renderer.create_texture(tw, thh);
    renderer.upload_texture_rgba(thumb, &make_thumbnail_rgba(tw, thh));

    // #086: 共有 clip は color で塗られ、 ⇌ がリンクを示す。 同 group の 2 clip は同 color (teal) で、
    // in_active_group=true により neutral 強調リングが乗る (= 「同じ仲間」 を中立色で示す)。
    let audio_clips = vec![
        {
            let mut c = colored(shared(clip(1, 0.0, 2.5, "Share A1"), AUDIO_HUE), TEAL);
            c.in_active_group = true;
            c
        },
        {
            let mut c = colored(shared(clip(2, 3.0, 2.5, "Share A2"), AUDIO_HUE), TEAL);
            c.in_active_group = true;
            c
        },
        // 非 share の通常 clip → color fill のみ、 ⇌ なし。
        colored(clip(3, 6.0, 2.0, "Plain"), Color::rgb(0.30, 0.45, 0.70)),
    ];
    let video_clips = vec![
        // thumbnail なし共有 Text clip → color (purple) fill + ⇌ (hue fill 撤去後も share マーク)。
        colored(shared(clip(10, 0.0, 2.5, "Text Clip"), SHARE_HUE), Color::rgb(0.55, 0.35, 0.70)),
        // thumbnail あり共有 video clip → letterbox は video_clip_loading のまま + ⇌ のみ。
        {
            let mut c = shared(clip(11, 3.0, 3.0, "Video Clip"), SHARE_HUE);
            c.thumbnail = Some((thumb, tw, thh));
            c
        },
        // 非 share・非 color video clip → 従来どおり video_clip_loading 一色 (回帰確認)。
        clip(12, 6.5, 1.5, "Plain"),
    ];
    let selected_row_clips = vec![
        // selected な共有 video clip → selection 黄 + ⇌ (selection 最優先 / #022)。
        colored(shared(clip(20, 0.0, 3.0, "Selected Share"), SHARE_HUE), Color::rgb(0.80, 0.45, 0.20)),
    ];

    let tracks = vec![
        track(0, "Audio", TrackKind::Audio, audio_clips),
        track(1, "Video", TrackKind::Video, video_clips),
        track(2, "Video2", TrackKind::Video, selected_row_clips),
    ];
    let selected = [ClipKey { track: 2, clip: 20 }];

    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    scene.clear_color = Color::rgb(0.10, 0.11, 0.13).to_wgpu();
    let screen = PhysicalSize { width, height };

    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        let style = ArrangementStyle::default();
        let _ = ui.arrangement(
            "arr_share_snapshot",
            Rect { x: 0.0, y: 0.0, w: width as f32, h: height as f32 },
            &tracks,
            &[],
            view(width, height),
            &selected,
            &[],
            &[],
            &[],
            &style,
            None,
            |_| Edit::mutate(|()| {}),
        );
    });

    let rgba = renderer.render_to_rgba(&scene)?;
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../target");
    fs::create_dir_all(&target_dir)?;
    let out_path = target_dir.join("arrangement_share_snapshot.png");
    save_png(&out_path, &rgba, width, height)?;
    println!("arrangement share snapshot saved to {}", out_path.display());
    Ok(())
}

fn save_png(path: &Path, rgba: &[u8], width: u32, height: u32) -> Result<(), Box<dyn Error>> {
    let file = File::create(path)?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba)?;
    Ok(())
}
