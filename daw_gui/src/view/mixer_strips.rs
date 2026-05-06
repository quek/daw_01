//! Renoise 風 mixer strip。draw(...) を呼ぶと指定 area 内に N 本のチャンネル
//! ストリップが横並びで描画される。
//!
//! 各 strip:
//!   - トラック名
//!   - M (mute) / S (solo) toggle
//!   - Pan knob
//!   - Volume fader (縦) + L/R peak meter

use daw_ui_core::{Edit, LevelMeterStyle, MeterBallistic, ToggleButtonStyle, Ui};
use daw_ui_renderer::{Color, Rect};

use crate::app::{AppData, AppEvent};

const STRIP_WIDTH: f32 = 80.0;
const STRIP_GAP: f32 = 4.0;
const TOP_LABEL_H: f32 = 18.0;
const TOGGLE_H: f32 = 22.0;
const KNOB_SIZE: f32 = 32.0;
const FADER_W: f32 = 18.0;
const METER_W: f32 = 4.0;
const METER_GAP: f32 = 2.0;

const COLOR_BG: Color = Color { r: 0.13, g: 0.13, b: 0.15, a: 1.0 };
const COLOR_STRIP_BG: Color = Color { r: 0.18, g: 0.18, b: 0.22, a: 1.0 };
/// Group / sub-mix bus strip — slightly bluer than a regular strip and
/// closer in luminance to MASTER_BG so the eye reads it as a bus rather
/// than a track.
const COLOR_GROUP_BG: Color = Color { r: 0.18, g: 0.22, b: 0.30, a: 1.0 };
const COLOR_MASTER_BG: Color = Color { r: 0.22, g: 0.22, b: 0.28, a: 1.0 };
const COLOR_TEXT: Color = Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 };
const COLOR_TEXT_DIM: Color = Color { r: 0.65, g: 0.68, b: 0.72, a: 1.0 };
const COLOR_MUTE_HINT: Color = Color { r: 0.86, g: 0.27, b: 0.27, a: 1.0 };
const COLOR_SOLO_HINT: Color = Color { r: 0.90, g: 0.78, b: 0.31, a: 1.0 };

const TOGGLE_BUTTON_BASE: ToggleButtonStyle = ToggleButtonStyle {
    off_color: Color { r: 0.22, g: 0.22, b: 0.26, a: 1.0 },
    on_color: Color { r: 0.30, g: 0.30, b: 0.36, a: 1.0 },
    hint_band: None,
    hint_band_h: 2.0,
    border: Color { r: 0.35, g: 0.38, b: 0.45, a: 1.0 },
    border_width: 1.0,
    radius: 4.0,
    font_size: 12.0,
    text_color: Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 },
};

const STYLE_MUTE: ToggleButtonStyle = ToggleButtonStyle {
    hint_band: Some(COLOR_MUTE_HINT),
    ..TOGGLE_BUTTON_BASE
};

const STYLE_SOLO: ToggleButtonStyle = ToggleButtonStyle {
    hint_band: Some(COLOR_SOLO_HINT),
    ..TOGGLE_BUTTON_BASE
};

pub(crate) const DB_MIN: f32 = -80.0;
pub(crate) const DB_MAX: f32 = 6.0;
pub(crate) const DB_RANGE: f32 = DB_MAX - DB_MIN;

pub(crate) fn amp_to_fader(amp: f32) -> f32 {
    if amp <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * amp.log10();
    ((db - DB_MIN) / DB_RANGE).clamp(0.0, 1.0)
}

pub(crate) fn fader_to_amp(n: f32) -> f32 {
    let db = n * DB_RANGE + DB_MIN;
    if db <= DB_MIN {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    ui.panel("mixer_bg", area, COLOR_BG, 0.0);

    let inner_pad = 8.0;
    let strip_y = area.y + inner_pad;
    let strip_h = area.h - inner_pad * 2.0;

    // Master strip (右端固定、scroll_area の外)
    let master_x = area.x + area.w - inner_pad - STRIP_WIDTH;

    // Per-track strips: 左端 inner_pad から master_x の手前まで scroll_area で横スクロール
    let scroll_x = area.x + inner_pad;
    let scroll_w = (master_x - inner_pad - scroll_x).max(0.0);
    let scroll_rect = Rect { x: scroll_x, y: strip_y, w: scroll_w, h: strip_h };
    let mix = app.track_mix();
    let pitch = STRIP_WIDTH + STRIP_GAP;
    let content_w = (mix.len() as f32) * pitch;
    ui.scroll_area("mixer_strips", scroll_rect, (content_w, strip_h), |ui, offset| {
        for (i, entry) in mix.iter().enumerate() {
            let x = scroll_x - offset.0 + (i as f32) * pitch;
            if x + STRIP_WIDTH < scroll_x || x > scroll_x + scroll_w {
                continue;
            }
            // Group strips wear a bluer tint so the eye picks them
            // out from regular per-track strips. PR2 shows depth as a
            // small "↳" prefix on the name (real indented mixer rows
            // would need a per-row layout overhaul; deferred).
            let bg = if entry.is_group { COLOR_GROUP_BG } else { COLOR_STRIP_BG };
            let display_name = if entry.depth > 0 {
                let arrows = "↳".repeat(entry.depth.min(4) as usize);
                format!("{arrows} {}", entry.name)
            } else {
                entry.name.clone()
            };
            draw_strip(
                ui,
                entry.index as usize,
                &display_name,
                entry.volume,
                entry.pan,
                entry.muted,
                entry.solo,
                entry.peak_l_raw,
                entry.peak_r_raw,
                Rect { x, y: strip_y, w: STRIP_WIDTH, h: strip_h },
                bg,
                entry.index,
                false,
            );
        }
    });

    draw_strip(
        ui,
        usize::MAX,
        "MASTER",
        app.master_gain,
        0.0,
        false,
        false,
        app.peak_l_display,
        app.peak_r_display,
        Rect { x: master_x, y: strip_y, w: STRIP_WIDTH, h: strip_h },
        COLOR_MASTER_BG,
        u32::MAX,
        true,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_strip(
    ui: &mut Ui<'_, AppData>,
    layout_idx: usize,
    name: &str,
    volume: f32,
    pan: f32,
    muted: bool,
    solo: bool,
    peak_l_raw: f32,
    peak_r_raw: f32,
    rect: Rect,
    bg: Color,
    track_idx: u32,
    is_master: bool,
) {
    ui.panel(("mixer_strip_bg", layout_idx), rect, bg, 4.0);

    let pad = 6.0;
    let mut y = rect.y + pad;

    // 名前
    ui.label_at(
        ("mixer_strip_name", layout_idx),
        name,
        rect.x + pad,
        y,
        11.0,
        if is_master { COLOR_TEXT } else { COLOR_TEXT_DIM },
    );
    y += TOP_LABEL_H;

    if !is_master {
        let btn_w = (rect.w - pad * 2.0 - 4.0) * 0.5;
        ui.toggle_button_at(
            ("mixer_strip_mute", layout_idx),
            "M",
            Rect { x: rect.x + pad, y, w: btn_w, h: TOGGLE_H },
            muted,
            &STYLE_MUTE,
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleTrackMute(track_idx))
                })
            },
        );
        ui.toggle_button_at(
            ("mixer_strip_solo", layout_idx),
            "S",
            Rect { x: rect.x + pad + btn_w + 4.0, y, w: btn_w, h: TOGGLE_H },
            solo,
            &STYLE_SOLO,
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleTrackSolo(track_idx))
                })
            },
        );
        y += TOGGLE_H + 6.0;

        // Pan knob (-1..1 → 0..1)
        let knob_x = rect.x + (rect.w - KNOB_SIZE) * 0.5;
        let knob_value = (pan + 1.0) * 0.5;
        let track_idx_for_pan = track_idx;
        ui.knob_at(
            ("mixer_strip_pan", layout_idx),
            Rect { x: knob_x, y, w: KNOB_SIZE, h: KNOB_SIZE },
            knob_value,
            0.5,
            "Pan",
            move |v| {
                let pan = v * 2.0 - 1.0;
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetTrackPan {
                        track: track_idx_for_pan,
                        pan,
                    })
                })
            },
        );
        y += KNOB_SIZE + 4.0;
    }

    // 縦 fader + L/R peak meter
    let fader_top = y + 4.0;
    let fader_bottom = rect.y + rect.h - pad - 12.0;
    let fader_h = (fader_bottom - fader_top).max(20.0);

    let group_w = FADER_W + (METER_W * 2.0 + METER_GAP * 2.0);
    let group_x = rect.x + (rect.w - group_w) * 0.5;

    let fader_value = amp_to_fader(volume);
    let track_idx_for_vol = track_idx;
    let is_master_for_vol = is_master;
    let fader_label: &'static str = if is_master_for_vol { "Master Volume" } else { "Track Volume" };
    ui.fader_at(
        ("mixer_strip_fader", layout_idx),
        Rect { x: group_x, y: fader_top, w: FADER_W, h: fader_h },
        fader_value,
        amp_to_fader(1.0),
        fader_label,
        move |v| {
            let amp = fader_to_amp(v);
            Edit::mutate(move |app: &mut AppData| {
                if is_master_for_vol {
                    app.handle_event(AppEvent::SetMasterGain(amp));
                } else {
                    app.handle_event(AppEvent::SetTrackVolume {
                        track: track_idx_for_vol,
                        amp,
                    });
                }
            })
        },
    );

    let mx = group_x + FADER_W + METER_GAP;
    let meter_style = LevelMeterStyle::default();
    ui.level_meter(
        ("mixer_meter_l", layout_idx),
        Rect { x: mx, y: fader_top, w: METER_W, h: fader_h },
        peak_l_raw,
        MeterBallistic::Peak,
        meter_style,
    );
    ui.level_meter(
        ("mixer_meter_r", layout_idx),
        Rect {
            x: mx + METER_W + METER_GAP,
            y: fader_top,
            w: METER_W,
            h: fader_h,
        },
        peak_r_raw,
        MeterBallistic::Peak,
        meter_style,
    );
}
