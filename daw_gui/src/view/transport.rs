//! Transport bar (画面上端): BPM / time_sig 編集 / Play / Stop / Loop / VOICEVOX 合成 /
//! Add Track / Playhead 表示。
//!
//! Master fader / L-R peak meter は Mixer の MASTER ストリップで一本化したため、
//! ここでは持たない。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::{Color, Rect};

use crate::app::{AppData, AppEvent};

const BG: Color = Color { r: 0.16, g: 0.16, b: 0.20, a: 1.0 };
const TEXT: Color = Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 };

const TS_DEN_ITEMS: &[&str] = &["2", "4", "8", "16"];

fn ts_den_to_index(den: u8) -> usize {
    match den {
        2 => 0,
        4 => 1,
        8 => 2,
        16 => 3,
        _ => 1, // 異常値は 4 表示にフォールバック
    }
}

fn ts_index_to_den(idx: usize) -> u8 {
    match idx {
        0 => 2,
        1 => 4,
        2 => 8,
        3 => 16,
        _ => 4,
    }
}

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    ui.panel("transport_bg", area, BG, 0.0);

    let pad = 12.0;
    let mut x = area.x + pad;
    let cy = area.y + (area.h - 28.0) * 0.5;
    let bh = 28.0;

    // BPM ラベル + 入力欄
    ui.label_at(
        "transport_bpm_label",
        "BPM",
        x,
        area.y + (area.h - 12.0) * 0.5,
        12.0,
        TEXT,
    );
    x += 28.0;

    let bpm_w = 64.0;
    let bpm_resp = ui.text_input_at(
        "transport_bpm_input",
        Rect { x, y: cy, w: bpm_w, h: bh },
        &app.bpm_edit_text,
        |s| Edit::mutate(move |app: &mut AppData| app.handle_event(AppEvent::BpmEditChanged(s))),
    );
    if bpm_resp.committed {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::CommitBpmEdit)
        }));
    }
    x += bpm_w + 12.0;

    // time_sig (numerator) 入力欄
    let ts_num_w = 36.0;
    let ts_num_resp = ui.text_input_at(
        "transport_time_sig_num",
        Rect { x, y: cy, w: ts_num_w, h: bh },
        &app.time_sig_num_edit_text,
        |s| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::TimeSigNumEditChanged(s))
            })
        },
    );
    if ts_num_resp.committed {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::CommitTimeSigNumEdit)
        }));
    }
    x += ts_num_w + 4.0;

    // "/" セパレータ
    ui.label_at(
        "transport_time_sig_slash",
        "/",
        x,
        area.y + (area.h - 12.0) * 0.5,
        12.0,
        TEXT,
    );
    x += 8.0;

    // time_sig (denominator) dropdown
    let ts_den_w = 52.0;
    let cur_den_idx = ts_den_to_index(app.song.time_sig.1);
    if let Some(idx) = ui.dropdown(
        "transport_time_sig_den",
        Rect { x, y: cy, w: ts_den_w, h: bh },
        TS_DEN_ITEMS,
        cur_den_idx,
    ) {
        let den = ts_index_to_den(idx);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetSongTimeSigDenominator(den))
        }));
    }
    x += ts_den_w + 12.0;

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

    // Group track には専用ボタンを置かない。Ableton Live と同じく
    // 「選択トラックを Ctrl+G でまとめる」フローのみ提供する
    // (空のグループは意味がないため)。

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
