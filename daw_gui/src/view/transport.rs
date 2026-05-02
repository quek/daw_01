//! Transport bar (画面上端): BPM 表示 / Play / Stop / Loop / VOICEVOX 合成 /
//! Add Track / Playhead 表示。
//!
//! Master fader / L-R peak meter は Mixer の MASTER ストリップで一本化したため、
//! ここでは持たない。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::app::{AppData, AppEvent};

const BG: Color = Color { r: 0.16, g: 0.16, b: 0.20, a: 1.0 };
const TEXT: Color = Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 };

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

}
