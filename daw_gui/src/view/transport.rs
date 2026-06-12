//! Transport bar (画面上端): BPM / time_sig 編集 / Play / Stop / Loop / VOICEVOX 合成 /
//! Add Track / Playhead 表示。
//!
//! Master fader / L-R peak meter は Mixer の MASTER ストリップで一本化したため、
//! ここでは持たない。

use std::sync::LazyLock;

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

/// scale dropdown のラベル列。 `Scale::display_name` は `const fn -> &'static str`
/// だが配列化は毎フレームの collect を避けるため一度だけ行う。
static SCALE_NAMES: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    common::scale::Scale::ALL_PRESETS
        .iter()
        .map(|s| s.display_name())
        .collect()
});

/// recording mode dropdown のラベル列 (= `RECORDING_MODES` の label 列)。
/// 毎フレームの collect を避けるため一度だけ生成する。
static REC_LABELS: LazyLock<Vec<&'static str>> =
    LazyLock::new(|| RECORDING_MODES.iter().map(|(_, l)| *l).collect());

/// Phase 7 B4 Step C/D (2026-05-13): MIDI Record toggle button のスタイル。
/// active 時 record red (= 業界標準) + hint band で「録音中」 を強調。
/// count-in 中も同 active state で描画 (label 側で「Count-in...」 表示と
/// 切り替え)。 STYLE_REC_MODE (橙、 automation recording) と意図的に
/// 区別 (= MIDI 録音は別概念)。
const STYLE_RECORD: ToggleButtonStyle = ToggleButtonStyle {
    off_color: Color { r: 0.22, g: 0.22, b: 0.26, a: 1.0 },
    on_color: Color { r: 0.85, g: 0.20, b: 0.20, a: 1.0 },
    border: Color { r: 0.45, g: 0.30, b: 0.30, a: 1.0 },
    border_width: 1.0,
    radius: 4.0,
    font_size: 12.0,
    text_color: Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 },
    on_text_color: None,
};

/// Phase 4: recording mode toggle の見た目。 active 時 off_color (灰)
/// → on_color (橙) + 下端の hint band で「writing」 状態を強調する。
/// Bitwig の Touch/Latch/Write ボタンに準拠 (Read 含めて 4 つすべて同 style)。
const STYLE_REC_MODE: ToggleButtonStyle = ToggleButtonStyle {
    off_color: Color { r: 0.22, g: 0.22, b: 0.26, a: 1.0 },
    on_color: Color { r: 0.85, g: 0.45, b: 0.18, a: 1.0 },
    border: Color { r: 0.35, g: 0.38, b: 0.45, a: 1.0 },
    border_width: 1.0,
    radius: 4.0,
    font_size: 12.0,
    text_color: Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 },
    on_text_color: None,
};

/// Loop toggle の icon ボタンスタイル。 active 時に Ableton 流の blue 系に染め、
/// off 時は灰。 record (赤) / automation (橙) と意味的に区別する。 font_size は
/// 矢印 glyph ⟳ が button (28 px) 内で視認できるよう 16 に拡張。
const STYLE_LOOP: ToggleButtonStyle = ToggleButtonStyle {
    off_color: Color { r: 0.22, g: 0.22, b: 0.26, a: 1.0 },
    on_color: Color { r: 0.25, g: 0.55, b: 0.85, a: 1.0 },
    border: Color { r: 0.35, g: 0.38, b: 0.45, a: 1.0 },
    border_width: 1.0,
    radius: 4.0,
    font_size: 16.0,
    text_color: Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 },
    on_text_color: None,
};

/// Play toggle の icon ボタンスタイル。 再生中 (active) は LED 風の緑、 停止中は
/// 灰。 業界標準の transport LED idiom に従う (= Ableton / Bitwig / Reaper)。
/// label は active 時 ■ (stop)、 inactive 時 ▶ (play) で切り替え。
const STYLE_PLAY: ToggleButtonStyle = ToggleButtonStyle {
    off_color: Color { r: 0.22, g: 0.22, b: 0.26, a: 1.0 },
    on_color: Color { r: 0.30, g: 0.70, b: 0.35, a: 1.0 },
    border: Color { r: 0.35, g: 0.38, b: 0.45, a: 1.0 },
    border_width: 1.0,
    radius: 4.0,
    font_size: 16.0,
    text_color: Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 },
    on_text_color: None,
};

/// Click (metronome) toggle の icon ボタンスタイル。 active 時は Ableton 流の
/// bright yellow 背景 + 黒文字 (= `on_text_color = Some(black)`)、 inactive は
/// 灰背景 + 白文字 (= `text_color = white`)。 record (赤) / loop (青) / play (緑)
/// / automation (橙) と意味的に区別。 label は ♬ (16 分音符 ×2、 細かい beat 感)。
/// gui_01 #051 (state-dependent text color) landing で実現。
const STYLE_CLICK: ToggleButtonStyle = ToggleButtonStyle {
    off_color: Color { r: 0.22, g: 0.22, b: 0.26, a: 1.0 },
    on_color: Color { r: 0.95, g: 0.85, b: 0.25, a: 1.0 },
    border: Color { r: 0.35, g: 0.38, b: 0.45, a: 1.0 },
    border_width: 1.0,
    radius: 4.0,
    font_size: 16.0,
    text_color: Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 },
    on_text_color: Some(Color { r: 0.10, g: 0.10, b: 0.12, a: 1.0 }),
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
        None,
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
        None,
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

    // Phase 7 B5 (`docs/plan_scale.html` §4.1): Key (root + scale) dropdown。
    // playhead 位置の `Song::scale_at` を表示、 root を "—" にすると
    // `ClearScaleChanges` で機能 OFF。 root + scale 別 dropdown は Bitwig /
    // Cubase と同 idiom (root: 12 pitch class + Off、 scale: 22 内蔵)。
    ui.label_at(
        "transport_key_label",
        "Key",
        x,
        area.y + (area.h - 12.0) * 0.5,
        12.0,
        TEXT,
    );
    x += 28.0;

    let playhead_for_scale = app.playhead_beat.map(f64::from).unwrap_or(0.0).max(0.0);
    let cur_scale_change = app.song.scale_at(playhead_for_scale).copied();
    let cur_root_idx = cur_scale_change
        .map(|sc| sc.root.min(11) as usize + 1)
        .unwrap_or(0); // 0 = "—" (OFF)
    let cur_scale_idx = cur_scale_change
        .and_then(|sc| {
            common::scale::Scale::ALL_PRESETS
                .iter()
                .position(|&s| s == sc.scale)
        })
        .unwrap_or(0);

    const ROOT_ITEMS: &[&str] = &[
        "—", "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let root_w = 50.0;
    if let Some(idx) = ui.dropdown(
        "transport_key_root",
        Rect { x, y: cy, w: root_w, h: bh },
        ROOT_ITEMS,
        cur_root_idx,
    ) {
        if idx == 0 {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::ClearScaleChanges)
            }));
        } else {
            let new_root = (idx - 1) as u8;
            // 既存 scale を維持 (= 空なら Major default)
            let scale = cur_scale_change
                .map(|sc| sc.scale)
                .unwrap_or(common::scale::Scale::Major);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetScaleAtPlayhead {
                    root: new_root,
                    scale,
                })
            }));
        }
    }
    x += root_w + 4.0;

    let scale_w = 124.0;
    if let Some(idx) = ui.dropdown(
        "transport_key_scale",
        Rect { x, y: cy, w: scale_w, h: bh },
        SCALE_NAMES.as_slice(),
        cur_scale_idx,
    ) {
        let new_scale = common::scale::Scale::ALL_PRESETS
            .get(idx)
            .copied()
            .unwrap_or(common::scale::Scale::Major);
        // 既存 root を維持 (= 空なら C default)
        let root = cur_scale_change.map(|sc| sc.root).unwrap_or(0);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetScaleAtPlayhead {
                root,
                scale: new_scale,
            })
        }));
    }
    x += scale_w + 12.0;

    // Play/Stop: icon (▶ / ■) + 緑色 toggle。 再生中は緑 LED で active 強調。
    let play_w = 36.0;
    let play_active = app.is_playing;
    ui.toggle_button_at(
        "transport_play",
        if play_active { "\u{25A0}" } else { "\u{25B6}" },
        Rect { x, y: cy, w: play_w, h: bh },
        play_active,
        &STYLE_PLAY,
        |_| Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::PlayToggle)),
    );
    x += play_w + 6.0;

    // Loop toggle: icon (⟳) + 色のコンパクトボタン。 active 時 blue に染まる。
    let loop_w = 36.0;
    let loop_active = app.is_looping;
    ui.toggle_button_at(
        "transport_loop",
        "\u{27F3}",
        Rect { x, y: cy, w: loop_w, h: bh },
        loop_active,
        &STYLE_LOOP,
        |_| Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::ToggleLoop)),
    );
    x += loop_w + 12.0;

    // Phase 4 (`docs/plan_automation.md` §6): automation recording mode
    // 4 択 (Read / Touch / Latch / Write) を dropdown 化。 排他選択なので
    // dropdown が UI 的に自然 + 横幅を 1/4 以下に圧縮できる。
    let rec_mode_w = 78.0;
    let cur_rec_idx = RECORDING_MODES
        .iter()
        .position(|(m, _)| *m == app.recording_mode)
        .unwrap_or(0);
    if let Some(idx) = ui.dropdown(
        "transport_rec_mode",
        Rect { x, y: cy, w: rec_mode_w, h: bh },
        REC_LABELS.as_slice(),
        cur_rec_idx,
    ) && let Some((mode, _)) = RECORDING_MODES.get(idx)
    {
        let mode_copy = *mode;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetRecordingMode(mode_copy))
        }));
    }
    x += rec_mode_w + 12.0;

    // Phase 7 B3 (2026-05-13): メトロノーム on/off toggle。 audio thread
    // が beat 境界ごとに internal click 音 (sine, accent: downbeat 880Hz /
    // 他 440Hz, 40ms decay, peak -12 dB) を master mix に重ねる。 default
    // off、 session-only state (project save には含めない)。 icon = ♬ (16 分
    // 音符 ×2、 細かい beat 感)、 active 時は黄 LED 風で 「click 鳴動中」 を強調。
    let metro_w = 36.0;
    let metro_active = app.metronome_enabled;
    ui.toggle_button_at(
        "transport_metronome",
        "\u{266C}",
        Rect { x, y: cy, w: metro_w, h: bh },
        metro_active,
        &STYLE_CLICK,
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
    x += rec_w + 6.0;

    // Phase 7 B5 (`docs/plan_scale.html` §5.2): Snap Live Input toggle。
    // ON で MIDI 録音中の note_on pitch を Song.scale_at(playhead).snap(pitch)
    // で in-scale に寄せる。 session-only state、 step input は適用外。
    let snap_live_w = 90.0;
    let snap_live_active = app.snap_live_input;
    ui.toggle_button_at(
        "transport_snap_live",
        "Snap Live",
        Rect { x, y: cy, w: snap_live_w, h: bh },
        snap_live_active,
        &STYLE_REC_MODE,
        move |_| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ToggleSnapLiveInput)
            })
        },
    );
    x += snap_live_w + 12.0;

    // Phase 7 B1-M Step 2 (2026-05-13): MIDI Learn toggle button。 inactive
    // 時 click で「次の MIDI CC を selected_track の Volume に bind」 (=
    // 段階 2 minimum scope、 Pan / Tempo / Plugin Param target は段階 4 で
    // dropdown 化予定)。 active 時は Cancel。 selected_track が無ければ
    // no-op。 既存 STYLE_REC_MODE (橙) を再利用 (= recording mode と同じく
    // 「現在 user 操作待ちの mode」 強調)。
    let learn_w = 90.0;
    let learn_active = app.midi_learn_target.is_some();
    let learn_label = if learn_active {
        "Learning..."
    } else {
        "Learn Vol"
    };
    let armed_track_for_learn = app.selected_track_ids.first().copied();
    ui.toggle_button_at(
        "transport_midi_learn",
        learn_label,
        Rect { x, y: cy, w: learn_w, h: bh },
        learn_active,
        &STYLE_REC_MODE,
        move |_| {
            Edit::mutate(move |app: &mut AppData| {
                if app.midi_learn_target.is_some() {
                    app.handle_event(AppEvent::CancelMidiLearn);
                } else if let Some(track_id) = armed_track_for_learn {
                    app.handle_event(AppEvent::StartMidiLearn(
                        common::model::BindingTarget::TrackVolume(track_id),
                    ));
                }
            })
        },
    );
    x += learn_w + 12.0;

    // PR-V4: 旧「Synth (V)」 ボタンは削除。 builtin VOICEVOX plugin が
    // 歌詞 / notes 変更時に自動 synth する (= sync_vocal_metadata 経由)。

    // Track 追加は Ctrl+T (shortcut) に集約。 旧 "+Vocal Track" / "+Inst Track"
    // ボタンは削除した。 vocal は track の instrument に VOICEVOX を挿すと自動で
    // vocal 化される (統合モデル、 select_plugin_from_db 参照)。

    // Group track には専用ボタンを置かない。Ableton Live と同じく
    // 「選択トラックを Ctrl+G でまとめる」フローのみ提供する
    // (空のグループは意味がないため)。

    // Playhead 位置 (text)。FIXME #4: 普通の DAW と同じく音楽的位置 (bar.beat.sub)
    // と絶対時間 (分:秒.ms) を併記する。SSoT は app.playhead_beat 一本で、
    // bar.beat は time_sig、time は bpm から導出 (両表示が同じ source、かつ
    // bar 番号はアレンジ / piano-roll ルーラと一致する)。
    let playhead = match app.playhead_beat {
        Some(b) => {
            let beat = f64::from(b);
            let (bar, beat_in_bar) = common::timing::beat_to_bar_beat(beat, app.song.time_sig);
            let beat_int = beat_in_bar.floor().max(1.0) as u32;
            let sub = ((beat_in_bar - f64::from(beat_int)) * 100.0).floor().clamp(0.0, 99.0) as u32;
            let secs = common::timing::beat_to_seconds(beat, app.song.bpm);
            let mins = (secs / 60.0).floor() as u64;
            let rem = secs - (mins as f64) * 60.0;
            let whole_s = rem.floor() as u64;
            let ms = ((rem - whole_s as f64) * 1000.0).floor().clamp(0.0, 999.0) as u64;
            format!("\u{25b6} {bar}.{beat_int}.{sub:02}  |  {mins:02}:{whole_s:02}.{ms:03}")
        }
        None => "\u{25a0}  --.-.--  |  --:--.---".to_string(),
    };
    ui.label_at(
        "transport_playhead",
        &playhead,
        x,
        area.y + (area.h - 12.0) * 0.5,
        12.0,
        TEXT,
    );
}
