//! daw_01 #087 verification: `color_picker` を真モーダル (#065 capture_input=true) 化したことの
//! **headless 動作 assert + 視覚 PNG** を 1 プロセスで自己検証する (実機 window を user に頼まない、
//! gui_01 方針: OffscreenRenderer + UiHost::no_redraw で自分で確認)。
//!
//! 検証 1 (FIXME #9 の核心): 背景に arrangement の clip drag と同じ primitive
//! `take_drag_rect_in_rect` を置き、 (control) picker を開かずに press すると背景は drag を掴む
//! (`Some`)、 (masked) picker を開いてから同じ press では背景は inert (`None`) で picker が drag を
//! 捕捉する (`picked == Some`)、 という対比で「picker open 中は背景の SV/Hue ドラッグ press が下の
//! clip を動かさない」 を証明する。
//!
//! 検証 2 (視覚): 開いた picker (SV 矩形 + Hue バー + swatch + preview) を offscreen で PNG 化し、
//! `target/color_picker_verify.png` に保存 (自分で目視できるように)。
//!
//! 実行: `cargo run --bin color_picker_verify`
//!   → stdout に `[PASS]` / `[FAIL]`、 PNG path を表示。
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::cell::Cell;
use std::error::Error;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use daw_ui_core::{
    ColorPickerStyle, FrameInput, PointerFrame, UiHost, WidgetId,
};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, OffscreenRenderer, Rect, Scene};

const CURRENT: Color = Color { r: 0.55, g: 0.35, b: 0.70, a: 1.0 }; // 紫: hue/SV selector を可視化

/// press (just_pressed + pressed)。
fn press(pos: (f32, f32)) -> FrameInput {
    FrameInput {
        pointer: PointerFrame {
            pos: Some(pos),
            primary_just_pressed: true,
            primary_pressed: true,
            ..PointerFrame::default()
        },
        ..FrameInput::default()
    }
}

/// drag 継続 (pressed のみ)。
fn hold(pos: (f32, f32)) -> FrameInput {
    FrameInput {
        pointer: PointerFrame {
            pos: Some(pos),
            primary_pressed: true,
            ..PointerFrame::default()
        },
        ..FrameInput::default()
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let style = ColorPickerStyle::default();
    let bg_wid = WidgetId::ROOT.child(b"bg_drag_victim");
    let screen = PhysicalSize { width: 400, height: 400 };
    // 背景 (= 下の clip 相当) は全面。 picker の SV を press したとき背景もその点を含む。
    let bg = Rect { x: 0.0, y: 0.0, w: 400.0, h: 400.0 };
    // anchor を左上付近に置くと panel は下に出る: panel = {60, 56, 180, 182}、 SV = {68, 64, 140, 140}。
    // その中心 (138, 134) を press 点にする (空 palette で layout 固定)。
    let anchor = Rect { x: 60.0, y: 40.0, w: 40.0, h: 16.0 };
    let sv_press = (138.0_f32, 134.0_f32);
    let empty: Vec<Color> = Vec::new();

    // ---- 検証 1-control: picker 無しなら背景が drag を掴む ----
    let control_grabbed = Cell::new(false);
    {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        host.frame_to_edits(&(), &mut scene, screen, press(sv_press), |(), ui| {
            control_grabbed.set(ui.take_drag_rect_in_rect(bg_wid, bg).is_some());
        });
    }

    // ---- 検証 1-masked: picker open 中は背景 inert + picker が drag を捕捉 ----
    let masked_bg_grabbed = Cell::new(false);
    let picker_captured = Cell::new(false);
    {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        // frame 1: picker を開く (press なし)。
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            let _ = ui.color_picker("verify", anchor, CURRENT, &empty, &style);
        });
        // frame 2: SV を press。 background widget が picker **より先**に走る (daw_01 と同順)。
        host.frame_to_edits(&(), &mut scene, screen, press(sv_press), |(), ui| {
            if ui.take_drag_rect_in_rect(bg_wid, bg).is_some() {
                masked_bg_grabbed.set(true);
            }
            let r = ui.color_picker("verify", anchor, CURRENT, &empty, &style);
            if r.picked.is_some() {
                picker_captured.set(true);
            }
        });
        // frame 3: drag 継続 (SV 内で移動)。 背景 inert・picker は値更新。
        host.frame_to_edits(&(), &mut scene, screen, hold((150.0, 120.0)), |(), ui| {
            if ui.take_drag_rect_in_rect(bg_wid, bg).is_some() {
                masked_bg_grabbed.set(true);
            }
            let r = ui.color_picker("verify", anchor, CURRENT, &empty, &style);
            if r.picked.is_some() {
                picker_captured.set(true);
            }
        });
    }

    // ---- 検証 2: 開いた picker を PNG 化 (swatch 付き) ----
    let palette = vec![
        Color::rgb(0.90, 0.25, 0.25),
        Color::rgb(0.25, 0.80, 0.35),
        Color::rgb(0.25, 0.45, 0.90),
        Color::rgb(0.90, 0.80, 0.25),
        Color::rgb(0.60, 0.35, 0.85),
        Color::rgb(0.20, 0.75, 0.80),
    ];
    let (pw, ph) = (260u32, 320u32);
    let mut renderer = OffscreenRenderer::new(pw, ph)?;
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    scene.clear_color = Color::rgb(0.10, 0.11, 0.13).to_wgpu();
    let png_anchor = Rect { x: 24.0, y: 16.0, w: 40.0, h: 16.0 };
    // frame 1 で open + popup_layer が同 frame に panel を描く。 SV を press して selector を動かし
    // 「body が入力を処理している」 ことも絵で示す。
    host.frame_to_edits(
        &(),
        &mut scene,
        PhysicalSize { width: pw, height: ph },
        FrameInput::default(),
        |(), ui| {
            let _ = ui.color_picker("png", png_anchor, CURRENT, &palette, &style);
        },
    );
    // 2 フレーム目: SV 内 press で selector 移動 (panel = {24,32,180,~210}, swatch 1 行で SV は下にずれる)。
    host.frame_to_edits(
        &(),
        &mut scene,
        PhysicalSize { width: pw, height: ph },
        press((110.0, 150.0)),
        |(), ui| {
            let _ = ui.color_picker("png", png_anchor, CURRENT, &palette, &style);
        },
    );
    let rgba = renderer.render_to_rgba(&scene)?;
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../target");
    fs::create_dir_all(&target_dir)?;
    let out_path = target_dir.join("color_picker_verify.png");
    save_png(&out_path, &rgba, pw, ph)?;

    // ---- 結果 ----
    let pass = control_grabbed.get() && !masked_bg_grabbed.get() && picker_captured.get();
    println!("--- #087 color_picker true-modal verification ---");
    println!(
        "  [1] control (no picker): background grabbed drag = {} (expect true)",
        control_grabbed.get()
    );
    println!(
        "  [2] masked (picker open): background grabbed drag = {} (expect false)",
        masked_bg_grabbed.get()
    );
    println!(
        "  [3] masked (picker open): picker captured drag    = {} (expect true)",
        picker_captured.get()
    );
    println!("  picker PNG saved to {}", out_path.display());
    if pass {
        println!("[PASS] capturing modal blocks background drag while picker captures it (FIXME #9 解消)");
        Ok(())
    } else {
        Err("[FAIL] #087 verification failed".into())
    }
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
