//! Transport bar (画面上端): BPM 表示 / Play / Stop / Loop / VOICEVOX 合成 /
//! Add Track / Master fader / L-R peak meter。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::app::{AppData, AppEvent};

const BG: Color = Color { r: 0.16, g: 0.16, b: 0.20, a: 1.0 };
const TEXT: Color = Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 };
const TEXT_DIM: Color = Color { r: 0.65, g: 0.68, b: 0.72, a: 1.0 };
const METER_BG: Color = Color { r: 0.08, g: 0.08, b: 0.10, a: 1.0 };
const METER_FILL: Color = Color { r: 0.55, g: 0.85, b: 0.55, a: 1.0 };
const METER_HOT: Color = Color { r: 0.95, g: 0.45, b: 0.40, a: 1.0 };

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    // 背景
    ui.heavy("transport_bg", |hctx| {
        hctx.cached((area.w.to_bits(), area.h.to_bits()), |hctx| {
            hctx.push_rect(RectCommand {
                rect: area,
                fill: BG,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        });
    });

    let pad = 12.0;
    let mut x = area.x + pad;
    let cy = area.y + (area.h - 28.0) * 0.5;
    let bh = 28.0;

    // BPM 表示
    ui.label_at(
        "transport_bpm",
        &format!("BPM {:.1}", app.bpm()),
        x,
        area.y + (area.h - 12.0) * 0.5,
        12.0,
        TEXT,
    );
    x += 76.0;

    // Play/Stop
    let play_w = 64.0;
    ui.button_at(
        "transport_play",
        if app.is_playing { "Stop" } else { "Play" },
        Rect { x, y: cy, w: play_w, h: bh },
        || Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::PlayToggle)),
    );
    x += play_w + 6.0;

    // Loop toggle
    let loop_w = 76.0;
    ui.button_at(
        "transport_loop",
        if app.is_looping { "Loop ON" } else { "Loop OFF" },
        Rect { x, y: cy, w: loop_w, h: bh },
        || Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::ToggleLoop)),
    );
    x += loop_w + 6.0;

    // VOICEVOX synth
    let synth_w = 88.0;
    ui.button_at(
        "transport_synth",
        "Synth (V)",
        Rect { x, y: cy, w: synth_w, h: bh },
        || Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::SynthesizeVocal)),
    );
    x += synth_w + 6.0;

    // Add Vocal Track
    let add_w = 110.0;
    ui.button_at(
        "transport_add_vocal",
        "+Vocal Track",
        Rect { x, y: cy, w: add_w, h: bh },
        || Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::AddVocalTrack)),
    );
    x += add_w + 6.0;

    // Add Instrument Track
    let inst_w = 110.0;
    ui.button_at(
        "transport_add_inst",
        "+Inst Track",
        Rect { x, y: cy, w: inst_w, h: bh },
        || Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::AddInstrumentTrack)),
    );
    x += inst_w + 18.0;

    // Playhead 位置 (text)
    let playhead = app
        .playhead_beat
        .map(|b| format!("\u{25b6} {b:7.2}"))
        .unwrap_or_else(|| "\u{25a0}   --".to_string());
    ui.label_at(
        "transport_playhead",
        &playhead,
        x,
        area.y + (area.h - 12.0) * 0.5,
        12.0,
        TEXT,
    );

    // 右寄せ: master fader + L/R meter
    let right_w = 220.0;
    let right_x = area.x + area.w - right_w - pad;
    let master_label_w = 56.0;
    let fader_w = 120.0;
    let meter_w = 22.0;

    ui.label_at(
        "transport_master_label",
        "Master",
        right_x,
        area.y + (area.h - 12.0) * 0.5,
        12.0,
        TEXT_DIM,
    );

    let fader_rect = Rect {
        x: right_x + master_label_w,
        y: cy + 2.0,
        w: fader_w,
        h: bh - 4.0,
    };
    ui.fader_at(
        "transport_master",
        fader_rect,
        app.master_gain,
        1.0,
        |v| Edit::mutate(move |app: &mut AppData| app.handle_event(AppEvent::SetMasterGain(v))),
    );

    let meters_x = right_x + master_label_w + fader_w + 6.0;
    draw_peak_meter(
        ui,
        "transport_meter_l",
        meters_x,
        area.y + 6.0,
        meter_w * 0.5 - 2.0,
        area.h - 12.0,
        app.peak_l_norm,
    );
    draw_peak_meter(
        ui,
        "transport_meter_r",
        meters_x + meter_w * 0.5 + 2.0,
        area.y + 6.0,
        meter_w * 0.5 - 2.0,
        area.h - 12.0,
        app.peak_r_norm,
    );
}

fn draw_peak_meter(
    ui: &mut Ui<'_, AppData>,
    id: &'static str,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    norm: f32,
) {
    ui.heavy(id, |hctx| {
        hctx.cached(norm.to_bits(), |hctx| {
            hctx.push_rect(RectCommand {
                rect: Rect { x, y, w, h },
                fill: METER_BG,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [2.0; 4],
                clip_rect: None,
            });
            let lvl = norm.clamp(0.0, 1.0);
            if lvl > 0.0 {
                let fill_h = h * lvl;
                let color = if lvl > 0.85 { METER_HOT } else { METER_FILL };
                hctx.push_rect(RectCommand {
                    rect: Rect {
                        x,
                        y: y + (h - fill_h),
                        w,
                        h: fill_h,
                    },
                    fill: color,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [2.0; 4],
                    clip_rect: None,
                });
            }
        });
    });
}
