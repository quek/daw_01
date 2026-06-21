//! M14 Phase 127 (daw_01 #105): Arranger レーン (曲のパート Section) の offscreen visual verify。
//!
//! ruler の直下・track lanes の上に `arranger_lane_h` の帯を確保し、 色付き section (Intro / Aメロ /
//! サビ) を名前ラベル付きで描く。 header 側 (`header_w` 列) に "Arranger" 見出し、 帯は ruler /
//! playhead / clips と時間軸 (x) が揃うことを 1 枚で確認する。
//!
//! 実行: `cargo run --bin arrangement_section_snapshot`
//!   → `<workspace>/target/arrangement_section_snapshot.png`。
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
    ArrangementClip, ArrangementStyle, ArrangementTrack, ArrangementView, Edit, FrameInput,
    SectionView, SnapConfig, TrackKind, UiHost,
};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, OffscreenRenderer, Rect, Scene};

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
        muted: false,
    }
}

fn track(id: u32, name: &str, clips: Vec<ArrangementClip>) -> ArrangementTrack {
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
        kind: TrackKind::Audio,
        color: None,
    }
}

fn section(id: u32, name: &str, color: [f32; 3], start: f64, len: f64, selected: bool) -> SectionView {
    SectionView { id, name: Arc::from(name), color, start_beat: start, len_beats: len, selected }
}

fn view() -> ArrangementView {
    ArrangementView {
        start_beat: 0.0,
        len_beats: 16.0,
        track_top: 0.0,
        tracks_visible: 3.0,
        track_row_h: 44.0,
        header_w: 150.0,
        ruler_h: 22.0,
        playhead_beat: Some(6.0),
        loop_range: Some((4.0, 12.0)),
        data_generation: 0,
        bpm: 120.0,
        time_sig: (4, 4),
        snap: SnapConfig::OFF,
        // Arranger レーンを 22px で確保。
        arranger_lane_h: 22.0,
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let width: u32 = 840;
    let height: u32 = 200;

    let mut renderer = OffscreenRenderer::new(width, height)?;

    let tracks = vec![
        track(0, "Vocal", vec![clip(1, 0.0, 8.0, "verse")]),
        track(1, "Drums", vec![clip(2, 0.0, 16.0, "beat")]),
        track(2, "Bass", vec![clip(3, 8.0, 4.0, "low")]),
    ];

    // 隣接 (Intro|Aメロ) + gap (サビは 1 拍空けて配置) の両方を確認する。
    // M14 Phase 128 (#106): "Aメロ" を selected にして選択ハイライト (明るい太枠) を 1 枚で確認。
    let sections = vec![
        section(0, "Intro", [0.30, 0.45, 0.65], 0.0, 4.0, false),
        section(1, "Aメロ", [0.35, 0.55, 0.40], 4.0, 4.0, true),
        section(2, "サビ", [0.70, 0.45, 0.35], 9.0, 5.0, false),
    ];

    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    scene.clear_color = Color::rgb(0.10, 0.11, 0.13).to_wgpu();
    let screen = PhysicalSize { width, height };

    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        let style = ArrangementStyle::default();
        let _ = ui.arrangement(
            "arr_section_snapshot",
            Rect { x: 0.0, y: 0.0, w: width as f32, h: height as f32 },
            &tracks,
            &sections,
            view(),
            &[],
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
    let out_path = target_dir.join("arrangement_section_snapshot.png");
    save_png(&out_path, &rgba, width, height)?;
    println!("arrangement section snapshot saved to {}", out_path.display());
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
