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
use crate::event_launcher::LauncherEvent;
use crate::theme::Theme;
use crate::view::param_gesture::push_param_gesture_edges;

const TS_DEN_ITEMS: &[&str] = &["2", "4", "8", "16"];

/// r.md #56: 再生位置の読み値 (ビート / タイム) のフォントサイズ。
const READOUT_FONT: f32 = 12.0;

/// ビート表記 (`小節.拍.1/100拍`) の枠幅。
///
/// **固定幅にするのが要点**。 旧実装はバー右端の Panic ボタンから逆算した「残り幅」に
/// 置かれたバー唯一の伸縮要素だったので、 桁が増えても他は何も動かなかった。 ボタン列の
/// 途中へ移した以上、 幅が文字列長で変わると右側のボタン列が毎小節横滑りする。 枠を
/// 固定幅にし、 中で **右寄せ** にすることで、 毎フレーム変わる下位桁 (1/100 拍 /
/// ミリ秒) を固定端に留め、 稀にしか変わらない小節 / 分の桁増減を左へ逃がす。
///
/// 最長表記は `9999.64.99` (小節 4 桁 + 拍 2 桁 + 1/100 拍 2 桁)。 拍が 2 桁になるのは
/// time_sig 分子が最大 32 (`scrub_style_tsig_num` の range) / 分母 2 で 1 小節 64 拍に
/// なるため。 実 measure で収まることは `readouts_fit_fixed_width` が固定する。
const BEAT_READOUT_W: f32 = 66.0;

/// タイム表記 (`分:秒.ミリ秒`) の枠幅。 最長表記は `999:59.999`。
const TIME_READOUT_W: f32 = 66.0;

/// Phase 5 Step 5.1 follow-up (gui_01 #035): BPM scrubable_number style。
/// sensitivity 0.5 = `1 px drag で 0.5 BPM 変化` (Ableton 流の感度)、
/// range は SetSongBpmFromScrub handler の clamp と同じ 1..=400。
/// drag 中の背景は `scrub_drag_bg_warm` (暖色版) — テンポは「時間軸そのもの」 を
/// 触る欄なので、 一般の数値欄 (`scrub_drag_bg`、 寒色) と一目で区別する。
///
/// hover は `inset_bg_hover` でなく `control` (= from_palette の既定より 1 段明るい)。
/// transport の欄は他のクロームより主張させたいので意図的に上書きしている。
fn scrub_style_bpm(theme: &Theme) -> ScrubableNumberStyle {
    let p = &theme.core;
    ScrubableNumberStyle {
        bg_color_hovered: p.control,
        bg_color_dragging: p.scrub_drag_bg_warm,
        radius: 3.0,
        font_size: 13.0,
        sensitivity: 0.5,
        range: Some((1.0, 400.0)),
        ..ScrubableNumberStyle::from_palette(p)
    }
}

/// Phase 5 Step 5.1 follow-up: TimeSig numerator scrubable_number style。
/// 1 px drag で 0.1 だけ変化 (= 整数値だが widget は f64 値で内部保持、 表示は
/// `Integer` で int 切り捨て)。 これで 10 px drag = 1 拍子変化、 ユーザーの
/// 慎重さが必要な操作 (= 拍子変更は楽曲の構造変化なので飛ばすと混乱)。
/// drag 中の背景は BPM と同じ暖色 (拍子も時間軸そのもの)。
fn scrub_style_tsig_num(theme: &Theme) -> ScrubableNumberStyle {
    ScrubableNumberStyle {
        sensitivity: 0.1,
        range: Some((1.0, 32.0)),
        ..scrub_style_bpm(theme)
    }
}

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

/// r.md #87: グローバルローンチ量子化の選択肢。
/// `LAUNCH_QUANTIZE_CHOICES` (モデル側の SSoT) から
/// [`LaunchQuantize::Global`] だけを除いたもの — グローバル設定自身が
/// 「グローバルに従う」 では意味を成さない。
static GLOBAL_QUANTIZE_CHOICES: LazyLock<Vec<(common::model::LaunchQuantize, &'static str)>> =
    LazyLock::new(|| {
        common::model::LAUNCH_QUANTIZE_CHOICES
            .iter()
            .filter(|(q, _)| *q != common::model::LaunchQuantize::Global)
            .copied()
            .collect()
    });

/// [`GLOBAL_QUANTIZE_CHOICES`] のラベル列。
static GLOBAL_QUANTIZE_LABELS: LazyLock<Vec<&'static str>> =
    LazyLock::new(|| GLOBAL_QUANTIZE_CHOICES.iter().map(|(_, l)| *l).collect());

/// transport のボタン共通形 (角丸 4 / 中立の off 面)。 各ボタンは `on_color` と
/// `font_size` だけを上書きして意味色を足す。
fn style_transport_button(theme: &Theme) -> ToggleButtonStyle {
    ToggleButtonStyle {
        radius: 4.0,
        font_size: 16.0,
        ..ToggleButtonStyle::from_palette(&theme.core)
    }
}

/// 再生追従スクロール (Follow) ボタンのスタイル。色は付けない (ユーザー指定) ＝
/// 追従中も Off も同じ中立色で、状態は label の記号だけで示す。クリックごとに
/// Off → 連続 → ページ を循環する (= `Alt+F` と同じ `CycleArrangeFollow`)。
fn style_follow(theme: &Theme) -> ToggleButtonStyle {
    ToggleButtonStyle { on_color: theme.core.control, ..style_transport_button(theme) }
}

/// 追従方式を表す記号 (Follow ボタンの label)。色なしの単色グリフ (ユーザー指定):
/// ⊘=OFF / ➡=連続スクロール / ⇥=ページめくり (arrow-to-bar = 端でページ送り)。
/// いずれもカラー絵文字でない (VS16 を付けない) ので単色描画される。
fn follow_glyph(mode: common::model::FollowMode) -> &'static str {
    use common::model::FollowMode;
    match mode {
        FollowMode::Off => "\u{2298}",    // ⊘
        FollowMode::Scroll => "\u{27A1}", // ➡ (VS16 無し = 単色)
        FollowMode::Page => "\u{21E5}",   // ⇥
    }
}

/// Phase 7 B4 Step C/D (2026-05-13): MIDI Record toggle button のスタイル。
/// active 時 record red (= 業界標準) で「録音中」 を強調。 `style_rec_mode`
/// (橙、 automation recording) と意図的に区別 (= MIDI 録音は別概念)。
/// r.md #52 で Play / Loop / Follow と同じ 36px アイコンボタン (label = ●) に
/// 揃えたので、font_size も他の transport アイコンと同じ 16 (= 既定) を使う。
fn style_record(theme: &Theme) -> ToggleButtonStyle {
    ToggleButtonStyle { on_color: theme.daw.record, ..style_transport_button(theme) }
}

/// 録音を要求したのにまだ録れていない間 (count-in 中 / 読込待ち) の Record button。
/// まだ録音していない「待機」状態なので red ではなく arm 橙で染める。 label は
/// ● の代わりに count-in の残り小節数 (2 → 1)、待ち理由が count-in でなければ `…`
/// (Bitwig / Live の count-in 表示と同 idiom)。
fn style_record_pending(theme: &Theme) -> ToggleButtonStyle {
    ToggleButtonStyle { on_color: theme.daw.record_arm, ..style_transport_button(theme) }
}

/// Phase 4: recording mode toggle の見た目。 active 時 off_color (灰)
/// → on_color (橙) + 下端の hint band で「writing」 状態を強調する。
/// Bitwig の Touch/Latch/Write ボタンに準拠 (Read 含めて 4 つすべて同 style)。
fn style_rec_mode(theme: &Theme) -> ToggleButtonStyle {
    ToggleButtonStyle {
        on_color: theme.daw.record_arm,
        font_size: 12.0,
        ..style_transport_button(theme)
    }
}

/// Loop toggle の icon ボタンスタイル。 active 時に Ableton 流の blue 系に染め、
/// off 時は灰。 record (赤) / automation (橙) と意味的に区別する。 font_size は
/// 矢印 glyph ⟳ が button (28 px) 内で視認できるよう 16 に拡張。
fn style_loop(theme: &Theme) -> ToggleButtonStyle {
    // on_color は from_palette 既定の `accent` そのまま (= Ableton 流の blue)。
    style_transport_button(theme)
}

/// Play toggle の icon ボタンスタイル。 再生中 (active) は LED 風の緑、 停止中は
/// 灰。 業界標準の transport LED idiom に従う (= Ableton / Bitwig / Reaper)。
/// label は active 時 ■ (stop)、 inactive 時 ▶ (play) で切り替え。
fn style_play(theme: &Theme) -> ToggleButtonStyle {
    ToggleButtonStyle { on_color: theme.daw.play, ..style_transport_button(theme) }
}

/// パニックボタンのスタイル。 momentary（toggle ではない）ので
/// `toggle_button_at` に `value=false` を渡し、 常に off_color が効く。 #76 で配置を
/// 一番右へ移し、 背景を他の transport ボタンと同じ中立色 (`control`、 Play /
/// Loop / Metronome の off 時と同一) に揃えた。 旧「常時赤 + label "Panic"」 の強い
/// 強調をやめ、 ラベルも "!" に圧縮して corner に控えめに置く。 on_color は momentary
/// ゆえ実描画されないが style 自己整合のため中立の `control_active` にする。
fn style_panic(theme: &Theme) -> ToggleButtonStyle {
    ToggleButtonStyle { on_color: theme.core.control_active, ..style_transport_button(theme) }
}

/// Click (metronome) toggle の icon ボタンスタイル。 active 時は Ableton 流の
/// bright yellow 背景 + 暗インク (= `on_text_color = Some(ink_on_bright)`)、 inactive は
/// 中立面 + `text` 。 record (赤) / loop (青) / play (緑) / automation (橙) と
/// 意味的に区別。 label は ♬ (16 分音符 ×2、 細かい beat 感)。
/// gui_01 #051 (state-dependent text color) landing で実現。
///
/// on 面 (`solo`) は明るい黄なので、その上のインクは `text` (テーマ従属 = ダークでは
/// 明色) では読めない。 r.md #73: ただし**色を固定せず** `ToggleButtonStyle` の既定
/// (`on_text_color: None` = ON 塗りの輝度から auto-contrast) に委ねる — 固定すると
/// ユーザーテーマが `daw.solo` を暗くしたときに文字が消える。 アレンジの Solo /
/// ミキサーの Solo と同じ 1 つの規則。
fn style_click(theme: &Theme) -> ToggleButtonStyle {
    ToggleButtonStyle {
        on_color: theme.daw.solo,
        ..style_transport_button(theme)
    }
}

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

/// r.md #56: 再生位置を (ビート表記, タイム表記) の 2 本に分けて組み立てる。
///
/// SSoT は `app.transport.playhead_beat` 一本。 小節.拍 は time_sig から
/// (アレンジ / ピアノロールのルーラと同じ [`common::timing::beat_to_bar_beat`])、
/// 秒は [`AppData::song_beat_to_seconds`] から導出する。 これは SongTempo automation
/// lane があれば `TempoMap` を `song_epoch` 世代キャッシュに載せて引くだけの経路で、
/// 無ければ定数 BPM の高速経路に落ちる。 旧実装は常に定数 BPM 換算の
/// `timing::beat_to_seconds` だったので、 テンポカーブを引いた曲で秒表示だけが
/// 実時間とずれていた。 なお `common::tempo_map::song_beat_to_seconds` を直に呼ぶと
/// テンポカーブのある曲で毎フレーム O(曲長) の table 構築が走る (常時描画のバーなので
/// 曲長に比例して悪化する) ため、 必ずキャッシュ側を通す。
fn playhead_readout(app: &AppData) -> (String, String) {
    let Some(b) = app.transport.playhead_beat else {
        return ("--.-.--".to_string(), "--:--.---".to_string());
    };
    let beat = f64::from(b);
    let song = app.song_doc.song();
    let (bar, beat_in_bar) = common::timing::beat_to_bar_beat(beat, song.time_sig);
    let beat_int = beat_in_bar.floor().max(1.0) as u32;
    let sub = ((beat_in_bar - f64::from(beat_int)) * 100.0).floor().clamp(0.0, 99.0) as u32;
    let secs = app.song_beat_to_seconds(beat);
    let mins = (secs / 60.0).floor() as u64;
    let rem = secs - (mins as f64) * 60.0;
    let whole_s = rem.floor() as u64;
    let ms = ((rem - whole_s as f64) * 1000.0).floor().clamp(0.0, 999.0) as u64;
    (
        format!("{bar}.{beat_int}.{sub:02}"),
        format!("{mins:02}:{whole_s:02}.{ms:03}"),
    )
}

/// 固定幅の枠に読み値を右寄せで描く (枠 / 背景は描かず、 transport の地の色の上に
/// 数字だけを置く)。 `label_at` は clip も ellipsis も持たないので、 想定外に長い
/// 表記が来ても隣のボタンへ左方向にはみ出さないよう左端で clamp する。
fn draw_readout(
    ui: &mut Ui<'_, AppData>,
    id: &'static str,
    text: &str,
    x: f32,
    w: f32,
    y: f32,
    color: Color,
) {
    let tw = ui.measure_text(text, READOUT_FONT);
    let tx = (x + w - tw).max(x);
    ui.label_at(id, text, tx, y, READOUT_FONT, color);
}

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let p = &app.theme.core;
    ui.panel("transport_bg", area, p.header, 0.0);

    let pad = 12.0;
    let cy = area.y + (area.h - 28.0) * 0.5;
    let bh = 28.0;

    // 左詰め列は 4 つのまとまりに分けて、running `x` をバケツリレーする。
    // 分けてあるのは実コード 300 行 budget (不変条件 9) のため — r.md #87 の
    // ランチャー用コントロールを足す前に、既存の `draw` を責務ごとに割った。
    // **並び順はこの 4 行が SSoT**。
    let mut x = area.x + pad;
    x = draw_tempo_and_key(app, ui, area, x, cy, bh);
    x = draw_playback_buttons(app, ui, area, x, cy, bh);
    x = draw_recording_controls(app, ui, x, cy, bh);
    // 左詰め列の最後尾。 ここで `x` を進めても誰も読まないので捨てる —
    // この右に要素を足すときは、この行を `x = ...` に戻してから足すこと。
    let _ = draw_launcher_controls(app, ui, area, x, cy, bh);

    // Panic ボタンは transport バーの **一番右** に右端揃えで固定配置する
    // (running `x` を使わず area 右端から逆算するので、 左側に何ボタンが増減しても常に
    // 右端に張り付く)。 ラベルは "!"、 背景は他ボタンと同じ中立色 (`style_panic`)。
    // click で `AppEvent::Panic` を発火 (再生停止 + 全 plugin 再初期化)。
    let panic_w = 28.0;
    ui.toggle_button_at(
        "transport_panic",
        "!",
        Rect {
            x: area.x + area.w - pad - panic_w,
            y: cy,
            w: panic_w,
            h: bh,
        },
        false,
        &style_panic(&app.theme),
        |_| Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::Panic)),
    );
}

/// テンポ / 拍子 / キー (スケール)。 バー左端のまとまり。
fn draw_tempo_and_key(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    mut x: f32,
    cy: f32,
    bh: f32,
) -> f32 {
    let p = &app.theme.core;

    // BPM ラベル + 入力欄
    ui.label_at(
        "transport_bpm_label",
        "BPM",
        x,
        area.y + (area.h - 12.0) * 0.5,
        12.0,
        p.text,
    );
    x += 28.0;

    // Phase 5 Step 5.1 follow-up (gui_01 #035): BPM scrubable_number。
    // press + 縦 drag (= 0.5 BPM/px) で連続変化、 release で確定、 single-click
    // で text input mode、 dblclick で 120 BPM reset。 widget は per-frame
    // on_change を発火するので、 daw_01 が `SetSongBpmFromScrub` 経由で
    // song.bpm 更新 + 軽量 IPC で audio engine に即時伝搬する。
    let bpm_w = 64.0;
    // r.md #78: tempo (SongTempo) は per-control modulation **対象**。
    //
    // 以前ここには「engine は `song_mod_routings` を一切消費しない」というコメントと
    // ともに `None` が置かれていたが、 それは stale だった。 実際には engine
    // (`daw_audio/src/engine.rs` の `current_bpm`) も書き出し
    // (`daw_audio/src/export.rs` の `smoothed_current_bpm_freewheel`) も
    // `apply_modulation_with_scalars(SongTempo, ..., song_mod_routings, ...)` を
    // 通しており、 tempo 変調は再生でも bounce でも効いている。
    //
    // 変調先の指定を ◉ (arm) 一本にした以上、 ここが `None` のままだと
    // **tempo だけ指定手段が無い**取り残しになる。
    let bpm_target = AutomationTarget::SongTempo;
    let bpm_mod = crate::view::modulation::build_mod(
        app,
        bpm_target.clone(),
        f64::from(app.song_doc.song().bpm),
        crate::view::modulation::PLAIN_IDENT,
        MASTER_TRACK_ID,
    );
    let bpm_resp = ui.scrubable_number_at(
        "transport_bpm_input",
        Rect { x, y: cy, w: bpm_w, h: bh },
        f64::from(app.song_doc.song().bpm),
        120.0,
        ScrubableNumberFormat::Decimal(1),
        &scrub_style_bpm(&app.theme),
        move |v| {
            #[allow(clippy::cast_possible_truncation)]
            let next = v as f32;
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetSongBpmFromScrub(next))
            })
        },
        None,
        Some(bpm_mod.modulation()),
    );
    crate::view::modulation::push_mod_drag_resync(
        ui,
        app,
        MASTER_TRACK_ID,
        &bpm_target,
        bpm_resp.mod_dragging,
    );
    // Phase 4 Step B 流 ParamGesture edge 検知: drag 開始 (= dragging
    // false→true) で `ParamGestureBegin`、 終了で `ParamGestureEnd` を発火。
    // target = SongTempo、 track_id = MASTER_TRACK_ID (= master row 配下の
    // song-level lane を指す sentinel)。 mixer_strips と同 helper 使用。
    let songtempo_was_dragging = app
        .recording.active_param_gestures
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
        f64::from(app.song_doc.song().time_sig.0),
        4.0,
        ScrubableNumberFormat::Integer,
        &scrub_style_tsig_num(&app.theme),
        move |v: f64| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let next = v.round().clamp(1.0, 32.0) as u8;
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetSongTimeSigNumFromScrub(next))
            })
        },
        None,
        None,
    );
    let tsig_was_dragging = app
        .recording.active_param_gestures
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
        p.text,
    );
    x += 8.0;

    // time_sig (denominator) dropdown
    let ts_den_w = 52.0;
    let cur_den_idx = ts_den_to_index(app.song_doc.song().time_sig.1);
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
        p.text,
    );
    x += 28.0;

    let playhead_for_scale = app.transport.playhead_beat.map(f64::from).unwrap_or(0.0).max(0.0);
    let cur_scale_change = app.song_doc.song().scale_at(playhead_for_scale).copied();
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
    x + scale_w + 12.0
}

/// 再生位置の読み値 + Play / Rec / Loop / Follow のまとまり。
fn draw_playback_buttons(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    mut x: f32,
    cy: f32,
    bh: f32,
) -> f32 {
    let p = &app.theme.core;

    // r.md #56: 再生位置 (音楽的位置 = 小節.拍.1/100拍 と 絶対時間 = 分:秒.ミリ秒) を
    // **再生ボタンの左** に置く。 旧位置はバー右端で、 目線が「操作するボタン」 と
    // 「今どこを再生しているか」 の間を画面幅ぶん往復していた。 Ableton Live の
    // Control Bar (テンポ / スケール → Arrangement Position → トランスポート) と同じ並び。
    //
    // 2 つの独立した枠に分けているのは、 桁数の変動が互いに干渉しないようにするため
    // (小節が 3 桁になってもタイム側の数字は 1px も動かない)。 旧実装が先頭に付けていた
    // ▶ / ■ 記号は落とした — 再生ボタンの真隣に来ると ▶ が 2 つ並び、 しかも再生中は
    // ボタンが ■ / 読み値が ▶ と意味が逆になって誤読を生む。 再生状態の SSoT は
    // Play ボタン側。
    let (beat_text, time_text) = playhead_readout(app);
    let readout_y = area.y + (area.h - READOUT_FONT) * 0.5;
    draw_readout(ui, "transport_pos_beat", &beat_text, x, BEAT_READOUT_W, readout_y, p.text);
    x += BEAT_READOUT_W + 8.0;
    draw_readout(ui, "transport_pos_time", &time_text, x, TIME_READOUT_W, readout_y, p.text);
    x += TIME_READOUT_W + 12.0;

    // Play/Stop: icon (▶ / ■) + 緑色 toggle。 再生中は緑 LED で active 強調。
    let play_w = 36.0;
    let play_active = app.transport.is_playing;
    ui.toggle_button_at(
        "transport_play",
        if play_active { "\u{25A0}" } else { "\u{25B6}" },
        Rect { x, y: cy, w: play_w, h: bh },
        play_active,
        &style_play(&app.theme),
        |_| Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::PlayToggle)),
    );
    x += play_w + 6.0;

    // Phase 7 B4 Step C/D (2026-05-13): MIDI 録音 toggle。 active で armed
    // track への MIDI input が clip に書き込まれる。
    // r.md #52: 旧実装は "● Rec" / "Count-in..." のテキストラベル用に 76px を
    // 確保していて、 実ラベル (~38px) との差が左右の余白として空いていた。 Play /
    // Loop / Follow / Metronome と同じ 36px の正方形アイコンボタンに詰め、 位置も
    // 再生ボタンの右隣 (= Ableton / Bitwig / Reaper の [▶][●] 並び) へ移した。
    // count-in 中は ● の代わりに残り小節数 (2 → 1) を arm 橙で出す。
    //
    // r.md #51: 点灯 = 「録音したい」意思、ラベル = 「今どの段階か」。要求したのに
    // まだ録れていない理由は 2 つあり、区別して出す — count-in を鳴らしている最中
    // (残り小節数を数字で) と、プロジェクト / プラグインの読込待ち (`…`)。
    // 同じ表示にすると嘘になる。
    let rec_w = 36.0;
    let rec_active = app.recording.requested;
    let pending = rec_active && !app.recording.live;
    let count_in_left = pending
        .then(|| app.count_in_remaining_bars())
        .flatten()
        .map(|n| n.to_string());
    let rec_label = match (&count_in_left, pending) {
        (Some(n), _) => n.as_str(),
        (None, true) => "\u{2026}", // … = 読込待ち (count-in ではない)
        (None, false) => "\u{25CF}", // ● = 停止中 / 録音中
    };
    let rec_style = if pending {
        style_record_pending(&app.theme)
    } else {
        style_record(&app.theme)
    };
    ui.toggle_button_at(
        "transport_record",
        rec_label,
        Rect { x, y: cy, w: rec_w, h: bh },
        rec_active,
        &rec_style,
        move |_| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ToggleMidiRecording)
            })
        },
    );
    x += rec_w + 6.0;

    // Loop toggle: icon (⟳) + 色のコンパクトボタン。 active 時 blue に染まる。
    let loop_w = 36.0;
    let loop_active = app.transport.loop_region.enabled;
    ui.toggle_button_at(
        "transport_loop",
        "\u{27F3}",
        Rect { x, y: cy, w: loop_w, h: bh },
        loop_active,
        &style_loop(&app.theme),
        |_| Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::ToggleLoop)),
    );
    x += loop_w + 12.0;

    // 再生追従スクロール (Follow): クリックごとに Off → 連続 → ページ を循環する
    // (= Alt+F と同じ CycleArrangeFollow)。状態は label の単色記号だけで示し、色は
    // 付けない (ユーザー指定)。Loop の隣に置き再生のまとまりにする。再生中に手動で
    // 横スクロール / ズームすると app 側で自動的に Off へ落ちる。
    let follow_w = 36.0;
    ui.toggle_button_at(
        "transport_follow",
        follow_glyph(app.ui_prefs.arrange_follow),
        Rect { x, y: cy, w: follow_w, h: bh },
        false, // 色を付けない (ユーザー指定) ので active 強調はしない
        &style_follow(&app.theme),
        |_| Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::CycleArrangeFollow)),
    );
    x + follow_w + 12.0
}

/// 録音まわり (録音モード / メトロノーム / カウントイン / Snap Live / MIDI Learn)。
/// このまとまりは全部 running `x` に載る固定幅ボタンなので `area` を読まない。
fn draw_recording_controls(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    mut x: f32,
    cy: f32,
    bh: f32,
) -> f32 {
    // Phase 4 (`docs/plan_automation.md` §6): automation recording mode
    // 4 択 (Read / Touch / Latch / Write) を dropdown 化。 排他選択なので
    // dropdown が UI 的に自然 + 横幅を 1/4 以下に圧縮できる。
    // 各ボタン幅は「最長ラベルの実 advance + 余白」で決める (HackGen Console NF は
    // 半角 = font_size * 0.527)。 過大な固定幅は右端の再生位置表示を押し出し、
    // 既定 1280px 幅で Panic ボタンに食われる原因になっていた。
    // dropdown は本体幅から PAD_X(8) + ARROW_W(16) を引いた残りが文字領域。
    // "Touch"/"Latch"/"Write" = 5 字 * 14 * 0.527 = 36.9px <= 66 - 24 = 42px。
    let rec_mode_w = 66.0;
    let cur_rec_idx = RECORDING_MODES
        .iter()
        .position(|(m, _)| *m == app.recording.recording_mode)
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
    let metro_active = app.transport.metronome_enabled;
    ui.toggle_button_at(
        "transport_metronome",
        "\u{266C}",
        Rect { x, y: cy, w: metro_w, h: bh },
        metro_active,
        &style_click(&app.theme),
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
    // 既定表示 "No count-in" = 11 字 * 14 * 0.527 = 81.2px。 文字領域は
    // w - PAD_X(8) - ARROW_W(16) なので 110 - 24 = 86px 必要 (旧 90 では 66px しか
    // 無く、 既定ラベルが ▼ アローに重なっていた)。
    let count_in_w = 110.0;
    let count_in_items: &[&str] = &["No count-in", "1 bar", "2 bars"];
    let cur_count_in_idx = (app.recording.count_in_bars.min(2)) as usize;
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
    // Record button は r.md #52 で再生ボタンの右隣へ移した (この上には無い)。
    x += count_in_w + 12.0;

    // Phase 7 B5 (`docs/plan_scale.html` §5.2): Snap Live Input toggle。
    // ON で MIDI 録音中の note_on pitch を Song.scale_at(playhead).snap(pitch)
    // で in-scale に寄せる。 session-only state、 step input は適用外。
    // "Snap Live" = 9 字 * 12 * 0.527 = 57.0px。
    let snap_live_w = 64.0;
    let snap_live_active = app.recording.snap_live_input;
    ui.toggle_button_at(
        "transport_snap_live",
        "Snap Live",
        Rect { x, y: cy, w: snap_live_w, h: bh },
        snap_live_active,
        &style_rec_mode(&app.theme),
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
    // no-op。 既存 `style_rec_mode` (橙) を再利用 (= recording mode と同じく
    // 「現在 user 操作待ちの mode」 強調)。
    // 最長ラベル "Learn Param" / "Learning..." = 11 字 * 12 * 0.527 = 69.6px。
    let learn_w = 76.0;
    let learn_active = app.recording.midi_learn_target.is_some();
    let armed_track_for_learn = app.selection.selected_track_ids.first().copied();
    // B2 (r.md #8): touch + learn。 直近に触った param が bind 可能なら
    // (PluginParam / Volume / Pan) それを、 無ければ選択 track の Volume を learn。
    let learn_target = app.midi_learn_binding_target(armed_track_for_learn);
    let learn_label = if learn_active {
        "Learning..."
    } else {
        match learn_target {
            Some(common::model::BindingTarget::PluginParam { .. }) => "Learn Param",
            Some(common::model::BindingTarget::TrackPan(_)) => "Learn Pan",
            _ => "Learn Vol",
        }
    };
    ui.toggle_button_at(
        "transport_midi_learn",
        learn_label,
        Rect { x, y: cy, w: learn_w, h: bh },
        learn_active,
        &style_rec_mode(&app.theme),
        move |_| {
            Edit::mutate(move |app: &mut AppData| {
                if app.recording.midi_learn_target.is_some() {
                    app.handle_event(AppEvent::CancelMidiLearn);
                } else if let Some(target) = learn_target {
                    app.handle_event(AppEvent::StartMidiLearn(target));
                }
            })
        },
    );
    // PR-V4: 旧「Synth (V)」 ボタンは削除。 builtin VOICEVOX plugin が
    // 歌詞 / notes 変更時に自動 synth する (= sync_vocal_metadata 経由)。

    // Track 追加は Ctrl+T (shortcut) に集約。 旧 "+Vocal Track" / "+Inst Track"
    // ボタンは削除した。 vocal は track の instrument に VOICEVOX を挿すと自動で
    // vocal 化される (統合モデル、 select_plugin_from_db 参照)。

    // Group track には専用ボタンを置かない。Ableton Live と同じく
    // 「選択トラックを Ctrl+G でまとめる」フローのみ提供する
    // (空のグループは意味がないため)。

    // r.md #56: 再生位置表示はここ (バー右端) から Play ボタンの左へ移した。
    // 旧実装は「右端の Panic から逆算した残り幅」 に置かれたバー唯一の伸縮要素で、
    // 狭い窓では ellipsis して耐えていた。 移設に伴い固定幅 + 右寄せへ作り替えてある
    // (`BEAT_READOUT_W` / `TIME_READOUT_W` の doc 参照)。
    x + learn_w + 12.0
}

/// r.md #87 クリップランチャー (計画書 §3.5): **グローバルローンチ量子化**の
/// dropdown と「アレンジに戻す (全行)」ボタン。
///
/// - 量子化はセルの [`LaunchQuantize::Global`] が従う実効値。既定は 1 小節。
///   選択肢は `LAUNCH_QUANTIZE_CHOICES` が SSoT で、そこから `Global`
///   (= 「自分自身に従う」という無意味な値) だけを除いて出す。
/// - 「アレンジに戻す」は **ランチャーが主導権を持つ行が 1 つでもあれば点灯**。
///   押すと全行を [`RowPlayback::Arranger`](common::model::RowPlayback::Arranger)
///   に戻す (Bitwig の Switch Playback to Arranger のグローバル版)。
fn draw_launcher_controls(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    mut x: f32,
    cy: f32,
    bh: f32,
) -> f32 {
    let p = &app.theme.core;
    ui.label_at(
        "transport_launch_q_label",
        "Launch",
        x,
        area.y + (area.h - 12.0) * 0.5,
        12.0,
        p.text,
    );
    x += 46.0;

    // "8 Bars" = 6 字 * 14 * 0.527 = 44.3px。 dropdown の文字領域は
    // w - PAD_X(8) - ARROW_W(16) なので 76 - 24 = 52px で収まる。
    let q_w = 76.0;
    let cur = app.global_launch_quantize();
    let cur_idx = GLOBAL_QUANTIZE_CHOICES.iter().position(|(q, _)| *q == cur).unwrap_or(0);
    if let Some(i) = ui.dropdown(
        "transport_launch_quantize",
        Rect { x, y: cy, w: q_w, h: bh },
        GLOBAL_QUANTIZE_LABELS.as_slice(),
        cur_idx,
    ) && let Some((q, _)) = GLOBAL_QUANTIZE_CHOICES.get(i)
    {
        let q = *q;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Launcher(LauncherEvent::SetGlobalQuantize(q)));
        }));
    }
    x += q_w + 6.0;

    // 記号だけの 36px ボタン (Play / Loop / Follow / メトロノームと同じ寸法)。
    // `⇥` はランチャー帯の「返す列」と **同じ字**を使う — 格子の中とバーの上で
    // 同じ記号が同じ意味 (Switch Playback to Arranger) になるので、
    // 文字を足さなくても対応が読める。バーの左詰め列はもともと 1280px 幅で
    // ほぼ埋まっているので、ここを 4 文字ぶん広げると右端の Panic に食い込む。
    let back_w = 36.0;
    let active = app.launcher_has_active_row();
    ui.toggle_button_at(
        "transport_launcher_to_arranger",
        "\u{21E5}",
        Rect { x, y: cy, w: back_w, h: bh },
        active,
        &style_rec_mode(&app.theme),
        |_| {
            Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::Launcher(LauncherEvent::AllToArranger));
            })
        },
    );
    x + back_w + 12.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use daw_ui_core::{FrameInput, UiHost};
    use daw_ui_platform::PhysicalSize;
    use daw_ui_renderer::Scene;

    /// r.md #56: 読み値は固定幅の枠に右寄せで置く。 `label_at` は clip も ellipsis も
    /// 持たないので、 最長表記が枠を超えると隣のボタンの上へグリフが漏れる (溢れた
    /// ぶんは左端 clamp で枠外へ出る)。 実フォントの advance を測って固定する。
    #[test]
    fn readouts_fit_fixed_width() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };

        // 小節 4 桁 / 拍 2 桁 (time_sig 32/2 = 1 小節 64 拍) / 1/100 拍 2 桁、
        // 分 3 桁 / 秒 2 桁 / ミリ秒 3 桁。 いずれも実用上の上限。
        let (mut beat_w, mut time_w) = (0.0_f32, 0.0_f32);
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            beat_w = ui.measure_text("9999.64.99", READOUT_FONT);
            time_w = ui.measure_text("999:59.999", READOUT_FONT);
        });

        assert!(
            beat_w <= BEAT_READOUT_W,
            "ビート最長表記 '9999.64.99' ({beat_w}px @ {READOUT_FONT}pt) が枠 {BEAT_READOUT_W}px に収まる"
        );
        assert!(
            time_w <= TIME_READOUT_W,
            "タイム最長表記 '999:59.999' ({time_w}px @ {READOUT_FONT}pt) が枠 {TIME_READOUT_W}px に収まる"
        );
    }
}
