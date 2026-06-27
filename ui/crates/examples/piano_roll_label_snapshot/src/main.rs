//! piano_roll 鍵盤オクターブラベルの auto-contrast (daw_01 #093 / Phase 117) の offscreen pixel verify。
//!
//! 実 `ui.piano_roll` widget を Highlight (root=C) / Fold (root=C) の 2 パネルで描き、 鍵盤左の
//! オクターブラベル (C5 等) が **行の実効背景 (key fill + root/out overlay 合成色) の輝度で dark/light
//! 自動反転** して読めることを確認する。 旧挙動 (warm-yellow 固定) では warm cream の root 行で
//! warm-on-warm に潰れていた。
//! 実行: `cargo run --bin piano_roll_label_snapshot` → `<workspace>/target/piano_roll_label_snapshot.png`。

use std::error::Error;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use daw_ui_core::{
    Edit, FrameInput, PianoRollScale, PianoRollScaleMode, PianoRollStyle, PianoRollView, SnapConfig,
    UiHost,
};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{OffscreenRenderer, Rect, Scene};

const MAJOR_MASK: u16 = 0b0000_1010_1011_0101;

fn view_with(mode: PianoRollScaleMode) -> PianoRollView {
    PianoRollView {
        start_beat: 0.0,
        len_beats: 4.0,
        // C3 (48) 〜 C5 (72) の 2 octave。 root=C なので C3/C4/C5 行に "Cn" ラベルが出る。
        pitch_top: 72.0,
        pitch_visible: 24.0,
        keyboard_w: 60.0,
        notes_generation: 0,
        velocity_lane_h: 0.0,
        playhead_beat: None,
        ruler_h: 0.0,
        bpm: 120.0,
        time_sig: (4, 4),
        snap: SnapConfig::DEFAULT,
        loop_range: None,
        scale: Some(PianoRollScale { root: 0, in_scale_mask: MAJOR_MASK, mode }),
        snap_pitch_during_drag: false,
        // scale label snapshot 用の focused fixture なので subdivision は OFF。
        sub_grid_interval_beats: None,
        // FIXME #82: snapshot 用 fixture では note 作成しないので任意 (1 拍)。
        default_note_len_beats: 1.0,
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let width: u32 = 760;
    let height: u32 = 420;

    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    scene.clear_color = daw_ui_renderer::theme::WINDOW_BG.to_wgpu();
    let screen = PhysicalSize { width, height };

    let style = PianoRollStyle::default();
    let highlight = Rect { x: 10.0, y: 10.0, w: 360.0, h: 400.0 };
    let fold = Rect { x: 390.0, y: 10.0, w: 360.0, h: 400.0 };

    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        let _ = ui.piano_roll(
            "highlight",
            highlight,
            &[],
            view_with(PianoRollScaleMode::Highlight),
            &[],
            &style,
            |_req| Edit::mutate(|(): &mut ()| {}),
        );
        let _ = ui.piano_roll(
            "fold",
            fold,
            &[],
            view_with(PianoRollScaleMode::Fold),
            &[],
            &style,
            |_req| Edit::mutate(|(): &mut ()| {}),
        );
    });

    let mut renderer = OffscreenRenderer::new(width, height)?;
    let rgba = renderer.render_to_rgba(&scene)?;

    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../target");
    fs::create_dir_all(&target_dir)?;
    let out_path = target_dir.join("piano_roll_label_snapshot.png");
    save_png(&out_path, &rgba, width, height)?;
    println!("piano roll label snapshot saved to {}", out_path.display());
    println!("  left = Highlight (root=C), right = Fold (root=C)");
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
