//! track header の長い名前 ellipsis 省略 (daw_01 #079 / Phase 107) の offscreen pixel verify。
//!
//! arrangement の track header と同じ構図 (name button + M/S/R toggle + lane disclosure 領域) を
//! `header_row_layout` と同じ寸法で手組みし、 短い名前 / 中くらい / 溢れる長い名前の 3 行を並べる。
//! OffscreenRenderer で PNG 化して「長い名前が name 領域を越えて M/S/R の隙間から覗かない」 ことを
//! 目視 + pixel で確認する。
//! 実行: `cargo run --bin track_header_snapshot` → `<workspace>/target/track_header_snapshot.png`。

use std::error::Error;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use daw_ui_core::{Edit, FrameInput, ToggleButtonStyle, Ui, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, OffscreenRenderer, Rect, Scene};

/// header_row_layout (arrangement.rs) と同じ寸法でボタン rect 群を返す。
struct HeaderLayout {
    name: Rect,
    buttons: [Rect; 3],
    lane_disc: Rect,
}

fn header_row_layout(row: Rect) -> HeaderLayout {
    let pad = 4.0_f32;
    let inner = Rect {
        x: row.x + pad,
        y: row.y + pad,
        w: (row.w - pad * 2.0).max(2.0),
        h: (row.h - pad * 2.0).max(2.0),
    };
    let btn_h = inner.h.min(20.0);
    let small = 22.0_f32;
    let gap = 2.0_f32;
    let n_btn = 3.0_f32;
    let lane_disc_size = 12.0_f32;
    let lane_disc_extra = lane_disc_size + gap;
    let total_right = small * n_btn + gap * n_btn + lane_disc_extra;
    let name_w = (inner.w - total_right).max(20.0);
    let name = Rect { x: inner.x, y: inner.y, w: name_w, h: btn_h };
    let mut x = inner.x + name_w + gap;
    let mut buttons = [Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 }; 3];
    for slot in &mut buttons {
        *slot = Rect { x, y: inner.y, w: small, h: btn_h };
        x += small + gap;
    }
    let lane_disc = Rect {
        x,
        y: inner.y + (btn_h - lane_disc_size).max(0.0) * 0.5,
        w: lane_disc_size,
        h: lane_disc_size,
    };
    HeaderLayout { name, buttons, lane_disc }
}

fn draw_row(ui: &mut Ui<'_, ()>, idx: usize, row: Rect, name: &str) {
    let lay = header_row_layout(row);
    let track_text_size = 12.0_f32;
    let mute = ToggleButtonStyle {
        off_color: Color::rgb(0.18, 0.20, 0.24),
        on_color: Color::rgb(0.55, 0.18, 0.18),
        font_size: 11.0,
        ..ToggleButtonStyle::default()
    };
    let solo = ToggleButtonStyle { font_size: 11.0, ..mute };
    let armed = ToggleButtonStyle { font_size: 11.0, ..mute };

    // name button (常に左寄せ、 長いと省略 + clip)。 arrangement と同じ left-align。
    let _ = ui.button_at_clicked_sized_aligned(
        ("name", idx),
        name,
        lay.name,
        track_text_size,
        daw_ui_core::ButtonTextAlign::Left,
    );
    // M / S / R toggle (省略対象外だが同 helper を通る)。
    ui.toggle_button_at(("m", idx), "M", lay.buttons[0], false, &mute, |_| {
        Edit::mutate(|(): &mut ()| {})
    });
    ui.toggle_button_at(("s", idx), "S", lay.buttons[1], false, &solo, |_| {
        Edit::mutate(|(): &mut ()| {})
    });
    ui.toggle_button_at(("r", idx), "R", lay.buttons[2], true, &armed, |_| {
        Edit::mutate(|(): &mut ()| {})
    });
    // lane disclosure 位置の目印 (省略名がここまで覗かないことの確認用)。
    ui.push_rect(daw_ui_renderer::RectCommand {
        rect: lay.lane_disc,
        fill: Color::rgb(0.30, 0.32, 0.38),
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [2.0; 4],
        clip_rect: None,
    });
}

fn main() -> Result<(), Box<dyn Error>> {
    let width: u32 = 200;
    let height: u32 = 150;

    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    scene.clear_color = daw_ui_renderer::theme::WINDOW_BG.to_wgpu();
    let screen = PhysicalSize { width, height };

    let header_w = 160.0_f32;
    let row_h = 36.0_f32;
    let rows = [
        "Drums",                                  // 収まる: 中央寄せ・省略なし
        "Lead Synth",                             // ほぼいっぱい
        "Very Long Track Name That Overflows",    // 溢れる: 省略 + 左寄せ
    ];

    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        for (i, name) in rows.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let y = 8.0 + i as f32 * (row_h + 6.0);
            draw_row(ui, i, Rect { x: 8.0, y, w: header_w, h: row_h }, name);
        }
    });

    let mut renderer = OffscreenRenderer::new(width, height)?;
    let rgba = renderer.render_to_rgba(&scene)?;

    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../target");
    fs::create_dir_all(&target_dir)?;
    let out_path = target_dir.join("track_header_snapshot.png");
    save_png(&out_path, &rgba, width, height)?;
    println!("track header snapshot saved to {}", out_path.display());
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
