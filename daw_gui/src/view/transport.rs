//! Transport bar (画面上端): BPM / time_sig 編集 / Play / Stop / Loop / VOICEVOX 合成 /
//! Add Track / Playhead 表示。
//!
//! Master fader / L-R peak meter は Mixer の MASTER ストリップで一本化したため、
//! ここでは持たない。

use common::model::RecordingMode;
use daw_ui_core::{Edit, ToggleButtonStyle, Ui};
use daw_ui_renderer::{Color, Rect};

use crate::app::{AppData, AppEvent};

const BG: Color = Color { r: 0.16, g: 0.16, b: 0.20, a: 1.0 };
const TEXT: Color = Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 };

const TS_DEN_ITEMS: &[&str] = &["2", "4", "8", "16"];

/// Phase 4 (`docs/plan_automation.md` §6): transport bar の automation
/// recording mode 4 way toggle のラベル列。 enum 並びは UI 並びと一致。
const RECORDING_MODES: &[(RecordingMode, &str)] = &[
    (RecordingMode::Read, "Read"),
    (RecordingMode::Touch, "Touch"),
    (RecordingMode::Latch, "Latch"),
    (RecordingMode::Write, "Write"),
];

/// Phase 4: recording mode toggle の見た目。 active 時 off_color (灰)
/// → on_color (橙) + 下端の hint band で「writing」 状態を強調する。
/// Bitwig の Touch/Latch/Write ボタンに準拠 (Read 含めて 4 つすべて同 style)。
const STYLE_REC_MODE: ToggleButtonStyle = ToggleButtonStyle {
    off_color: Color { r: 0.22, g: 0.22, b: 0.26, a: 1.0 },
    on_color: Color { r: 0.85, g: 0.45, b: 0.18, a: 1.0 },
    hint_band: Some(Color { r: 0.95, g: 0.55, b: 0.20, a: 1.0 }),
    hint_band_h: 2.0,
    border: Color { r: 0.35, g: 0.38, b: 0.45, a: 1.0 },
    border_width: 1.0,
    radius: 4.0,
    font_size: 12.0,
    text_color: Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 },
};

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
    x += loop_w + 12.0;

    // Phase 4 (`docs/plan_automation.md` §6): automation recording mode
    // 4 way toggle (Read / Touch / Latch / Write)。 排他 4 択を 4 個の
    // toggle_button で表現し、 active 1 個だけが on_color になる。 click
    // すると `SetRecordingMode` で同 mode に切り替え (active 上の click は
    // no-op だが、 排他 enum なので問題なし)。
    let rec_mode_w = 54.0;
    for (mode, label) in RECORDING_MODES {
        let id = format!("transport_rec_{label}");
        let active = app.recording_mode == *mode;
        let mode_copy = *mode;
        ui.toggle_button_at(
            id.as_str(),
            label,
            Rect { x, y: cy, w: rec_mode_w, h: bh },
            active,
            &STYLE_REC_MODE,
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetRecordingMode(mode_copy))
                })
            },
        );
        x += rec_mode_w + 4.0;
    }
    x += 8.0;

    // PR-V4: 旧「Synth (V)」 ボタンは削除。 builtin VOICEVOX plugin が
    // 歌詞 / notes 変更時に自動 synth する (= sync_vocal_metadata 経由)。

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
