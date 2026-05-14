//! Transport bar (画面上端): BPM / time_sig 編集 / Play / Stop / Loop / VOICEVOX 合成 /
//! Add Track / Playhead 表示。
//!
//! Master fader / L-R peak meter は Mixer の MASTER ストリップで一本化したため、
//! ここでは持たない。

use common::model::{AutomationTarget, MASTER_TRACK_ID, RecordingMode};
use daw_ui_core::{
    Edit, ScrubableNumberFormat, ScrubableNumberStyle, ToggleButtonStyle, Ui,
};
use daw_ui_renderer::{Color, Rect};

use crate::app::{AppData, AppEvent};
use crate::view::param_gesture::push_param_gesture_edges;

const BG: Color = Color { r: 0.16, g: 0.16, b: 0.20, a: 1.0 };
const TEXT: Color = Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 };

const TS_DEN_ITEMS: &[&str] = &["2", "4", "8", "16"];

/// Phase 5 Step 5.1 follow-up (gui_01 #035): BPM scrubable_number style。
/// sensitivity 0.5 = `1 px drag で 0.5 BPM 変化` (Ableton 流の感度)、
/// range は SetSongBpmFromScrub handler の clamp と同じ 1..=400。
/// drag 中の hint band 風に bg_color_dragging が transport の overall
/// オレンジ tinge に合うよう薄橙系を選ぶ。
const SCRUB_STYLE_BPM: ScrubableNumberStyle = ScrubableNumberStyle {
    bg_color: Color { r: 0.13, g: 0.13, b: 0.18, a: 1.0 },
    bg_color_hovered: Color { r: 0.18, g: 0.18, b: 0.23, a: 1.0 },
    bg_color_dragging: Color { r: 0.45, g: 0.30, b: 0.20, a: 1.0 },
    text_color: Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 },
    border: Color { r: 0.35, g: 0.38, b: 0.45, a: 1.0 },
    border_width: 1.0,
    radius: 3.0,
    font_size: 13.0,
    sensitivity: 0.5,
    range: Some((1.0, 400.0)),
};

/// Phase 5 Step 5.1 follow-up: TimeSig numerator scrubable_number style。
/// 1 px drag で 0.1 だけ変化 (= 整数値だが widget は f64 値で内部保持、 表示は
/// `Integer` で int 切り捨て)。 これで 10 px drag = 1 拍子変化、 ユーザーの
/// 慎重さが必要な操作 (= 拍子変更は楽曲の構造変化なので飛ばすと混乱)。
const SCRUB_STYLE_TSIG_NUM: ScrubableNumberStyle = ScrubableNumberStyle {
    bg_color: Color { r: 0.13, g: 0.13, b: 0.18, a: 1.0 },
    bg_color_hovered: Color { r: 0.18, g: 0.18, b: 0.23, a: 1.0 },
    bg_color_dragging: Color { r: 0.45, g: 0.30, b: 0.20, a: 1.0 },
    text_color: Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 },
    border: Color { r: 0.35, g: 0.38, b: 0.45, a: 1.0 },
    border_width: 1.0,
    radius: 3.0,
    font_size: 13.0,
    sensitivity: 0.1,
    range: Some((1.0, 32.0)),
};

/// Phase 4 (`docs/plan_automation.md` §6): transport bar の automation
/// recording mode 4 way toggle のラベル列。 enum 並びは UI 並びと一致。
const RECORDING_MODES: &[(RecordingMode, &str)] = &[
    (RecordingMode::Read, "Read"),
    (RecordingMode::Touch, "Touch"),
    (RecordingMode::Latch, "Latch"),
    (RecordingMode::Write, "Write"),
];

/// Phase 7 B4 Step C/D (2026-05-13): MIDI Record toggle button のスタイル。
/// active 時 record red (= 業界標準) + hint band で「録音中」 を強調。
/// count-in 中も同 active state で描画 (label 側で「Count-in...」 表示と
/// 切り替え)。 STYLE_REC_MODE (橙、 automation recording) と意図的に
/// 区別 (= MIDI 録音は別概念)。
const STYLE_RECORD: ToggleButtonStyle = ToggleButtonStyle {
    off_color: Color { r: 0.22, g: 0.22, b: 0.26, a: 1.0 },
    on_color: Color { r: 0.85, g: 0.20, b: 0.20, a: 1.0 },
    hint_band: Some(Color { r: 1.0, g: 0.30, b: 0.30, a: 1.0 }),
    hint_band_h: 2.0,
    border: Color { r: 0.45, g: 0.30, b: 0.30, a: 1.0 },
    border_width: 1.0,
    radius: 4.0,
    font_size: 12.0,
    text_color: Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 },
};

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

    // Phase 5 Step 5.1 follow-up (gui_01 #035): BPM scrubable_number。
    // press + 縦 drag (= 0.5 BPM/px) で連続変化、 release で確定、 single-click
    // で text input mode、 dblclick で 120 BPM reset。 widget は per-frame
    // on_change を発火するので、 daw_01 が `SetSongBpmFromScrub` 経由で
    // song.bpm 更新 + 軽量 IPC で audio engine に即時伝搬する。
    let bpm_w = 64.0;
    let bpm_resp = ui.scrubable_number_at(
        "transport_bpm_input",
        Rect { x, y: cy, w: bpm_w, h: bh },
        f64::from(app.song.bpm),
        120.0,
        ScrubableNumberFormat::Decimal(1),
        &SCRUB_STYLE_BPM,
        "BPM",
        move |v| {
            #[allow(clippy::cast_possible_truncation)]
            let next = v as f32;
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetSongBpmFromScrub(next))
            })
        },
    );
    // Phase 4 Step B 流 ParamGesture edge 検知: drag 開始 (= dragging
    // false→true) で `ParamGestureBegin`、 終了で `ParamGestureEnd` を発火。
    // target = SongTempo、 track_id = MASTER_TRACK_ID (= master row 配下の
    // song-level lane を指す sentinel)。 mixer_strips と同 helper 使用。
    let songtempo_was_dragging = app
        .active_param_gestures
        .contains(&(MASTER_TRACK_ID, AutomationTarget::SongTempo));
    push_param_gesture_edges(
        ui,
        MASTER_TRACK_ID,
        AutomationTarget::SongTempo,
        "Tempo",
        songtempo_was_dragging,
        bpm_resp.dragging,
    );
    x += bpm_w + 12.0;

    // Phase 5 Step 5.1 follow-up: TimeSig numerator scrubable_number。
    // 整数表示 (= 4 / 5 / 7 等)、 widget は内部 f64 で scrub、 表示は Integer
    // で round して整数化。 target = SongTimeSigNumerator で master row へ
    // 同 idiom で gesture / recording 経路を通す。
    let ts_num_w = 36.0;
    let ts_resp = ui.scrubable_number_at(
        "transport_time_sig_num",
        Rect { x, y: cy, w: ts_num_w, h: bh },
        f64::from(app.song.time_sig.0),
        4.0,
        ScrubableNumberFormat::Integer,
        &SCRUB_STYLE_TSIG_NUM,
        "Time Sig Numerator",
        move |v: f64| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let next = v.round().clamp(1.0, 32.0) as u8;
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetSongTimeSigNumFromScrub(next))
            })
        },
    );
    let tsig_was_dragging = app
        .active_param_gestures
        .contains(&(MASTER_TRACK_ID, AutomationTarget::SongTimeSigNumerator));
    push_param_gesture_edges(
        ui,
        MASTER_TRACK_ID,
        AutomationTarget::SongTimeSigNumerator,
        "TimeSig Num",
        tsig_was_dragging,
        ts_resp.dragging,
    );
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

    // Phase 7 B3 (2026-05-13): メトロノーム on/off toggle。 audio thread
    // が beat 境界ごとに internal click 音 (sine, accent: downbeat 880Hz /
    // 他 440Hz, 40ms decay, peak -12 dB) を master mix に重ねる。 default
    // off、 session-only state (project save には含めない)。 既存 recording
    // mode toggle と同 STYLE で active 時 橙、 visual で「現在 click が鳴る」
    // 状態が一目で分かる。
    let metro_w = 60.0;
    let metro_active = app.metronome_enabled;
    ui.toggle_button_at(
        "transport_metronome",
        "Click",
        Rect { x, y: cy, w: metro_w, h: bh },
        metro_active,
        &STYLE_REC_MODE,
        move |_| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetMetronomeEnabled(!metro_active))
            })
        },
    );
    x += metro_w + 12.0;

    // Phase 7 B4 Step C (2026-05-13): count-in bars dropdown (Off / 1 / 2 bars)。
    // 録音 trigger 時に preroll bars 分 click のみ流して 0 拍到達で正規録音
    // 開始。 0 で count-in 無し (= 即時録音)。 transport の Record button
    // とセットで業界標準 (Bitwig / Live / Reaper)。
    let count_in_w = 90.0;
    let count_in_items: &[&str] = &["No count-in", "1 bar", "2 bars"];
    let cur_count_in_idx = (app.count_in_bars.min(2)) as usize;
    if let Some(idx) = ui.dropdown(
        "transport_count_in",
        Rect { x, y: cy, w: count_in_w, h: bh },
        count_in_items,
        cur_count_in_idx,
    ) {
        let bars = idx as u8;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetCountInBars(bars))
        }));
    }
    x += count_in_w + 8.0;

    // Phase 7 B4 Step C/D (2026-05-13): MIDI 録音 toggle。 active で armed
    // track への MIDI input が clip に書き込まれる。 count-in 中は label を
    // 「Count-in...」 に切り替えて「待機中」 を可視化。 STYLE_RECORD は
    // 業界標準どおり record red 系。
    let rec_w = 86.0;
    let rec_active = app.midi_recording || app.midi_recording_pending;
    let rec_label = if app.midi_recording_pending {
        "Count-in..."
    } else {
        "● Rec"
    };
    ui.toggle_button_at(
        "transport_record",
        rec_label,
        Rect { x, y: cy, w: rec_w, h: bh },
        rec_active,
        &STYLE_RECORD,
        move |_| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ToggleMidiRecording)
            })
        },
    );
    x += rec_w + 12.0;

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
