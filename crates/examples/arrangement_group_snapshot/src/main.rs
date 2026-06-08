//! group track の背景 tint 撤去 (daw_01 #085 / Phase 113) の offscreen visual verify。
//!
//! header pane 付きで group track (= 子を持つ track) と通常 track を縦に並べ、 group row の
//! 背景が **他の track と同じ neutral** で塗られる (= 旧 `track_group_bg` の青 tint が出ない) こと、
//! 一方で **indent (`depth * indent_px`) と disclosure ▶/▼ の構造手掛かりは残る** ことを 1 枚で確認する。
//!
//! 構成 (上から):
//! - "Group A" (id 0, depth 0, 子あり = group) … 旧設計では行背景が青 tint だった row。 修正後 neutral。
//! - "Child 1" (id 1, parent=0, depth 1) … indent される子。
//! - "Child 2" (id 2, parent=0, depth 1)
//! - "Audio"   (id 3, depth 0, 子なし = 通常) … group row と背景が一致することの比較対象。
//!
//! 実行: `cargo run --bin arrangement_group_snapshot`
//!   → `<workspace>/target/arrangement_group_snapshot.png`。
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
    SnapConfig, TrackKind, UiHost,
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
    }
}

fn track(
    id: u32,
    name: &str,
    parent_id: Option<u32>,
    depth: u8,
    clips: Vec<ArrangementClip>,
) -> ArrangementTrack {
    ArrangementTrack {
        id,
        name: Arc::from(name),
        muted: false,
        solo: false,
        armed: false,
        clips,
        volume: 1.0,
        parent_id,
        depth,
        automation_lanes_collapsed: true,
        automation_lanes: Vec::new(),
        collapsed: false,
        row_h: None,
        kind: TrackKind::Audio,
        color: None,
    }
}

fn view() -> ArrangementView {
    ArrangementView {
        start_beat: 0.0,
        len_beats: 8.0,
        track_top: 0.0,
        tracks_visible: 4.0,
        track_row_h: 48.0,
        header_w: 168.0,
        ruler_h: 22.0,
        playhead_beat: None,
        loop_range: None,
        data_generation: 0,
        bpm: 120.0,
        time_sig: (4, 4),
        snap: SnapConfig::OFF,
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let width: u32 = 820;
    let height: u32 = 230;

    let mut renderer = OffscreenRenderer::new(width, height)?;

    let tracks = vec![
        track(0, "Group A", None, 0, vec![clip(1, 0.0, 3.0, "intro")]),
        track(1, "Child 1", Some(0), 1, vec![clip(2, 0.5, 2.0, "lead")]),
        track(2, "Child 2", Some(0), 1, vec![clip(3, 2.0, 3.5, "pad")]),
        track(3, "Audio", None, 0, vec![clip(4, 1.0, 4.0, "drums")]),
    ];

    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    scene.clear_color = Color::rgb(0.10, 0.11, 0.13).to_wgpu();
    let screen = PhysicalSize { width, height };

    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        let style = ArrangementStyle::default();
        let _ = ui.arrangement(
            "arr_group_snapshot",
            Rect { x: 0.0, y: 0.0, w: width as f32, h: height as f32 },
            &tracks,
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
    let out_path = target_dir.join("arrangement_group_snapshot.png");
    save_png(&out_path, &rgba, width, height)?;
    println!("arrangement group snapshot saved to {}", out_path.display());
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
