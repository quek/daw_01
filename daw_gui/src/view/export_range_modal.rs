//! Export WAV / Video の前に出す「書き出し範囲」 ピッカーモーダル。
//!
//! Ardour / REAPER の time-selection export に倣い、 書き出す時間範囲を選ぶ。
//! 表示/入力は 1-based **小節.拍** (例 "3.1") だが、 内部値は song の native 単位
//! である **拍 (beat)** のまま持つ。 拍を基準にするので audio (beat→sample) と
//! video (frame→秒→拍) の両 export が同じ窓で揃い、 A/V sync が崩れない。
//!
//! `AppData::export_range_picker` が `Some` の間だけ描画する。 start / end は
//! `scrubable_number_at` (drag-to-edit + click-to-type) で編集し、 clamp /
//! validation は `AppData::handle_event` 側 (SetExportRangeStart / End) が SSoT。
//! 「Export...」 で `ConfirmExportRange` (file dialog へ)、 「キャンセル」 / ESC で
//! `CancelExportRange`。 進捗オーバーレイ (`export_overlay`) と同じく
//! `close_on_escape: false` でフラッシュを避け、 body 内で ESC を拾う。

use daw_ui_core::{Edit, ModalStyle, ScrubableNumberFormat, ScrubableNumberStyle, Ui};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, Rect};
use crate::theme;

use crate::app::{AppData, AppEvent, ExportRangeKind};

const COLOR_TEXT: Color = theme::TEXT;
const COLOR_HINT: Color = theme::TEXT_DIM;

// 「開始 / 終了」 のラベル列 (LABEL_W) をパネル内に入れ、 解像度 dropdown に
// 最長プリセット "1080 × 1080 (正方形 1:1)" (実 advance 177px + PAD 8 + ARROW 16)
// を収めるための幅。 旧 380px ではラベルがパネル左端の**外側** (暗転 backdrop の上)
// に描かれ、 解像度の縦型 / 正方形プリセットが枠と ▼ アローを突き抜けていた。
const PANEL_W: f32 = 440.0;
const PANEL_H: f32 = 210.0;
/// Mp4 は範囲に加えて解像度 / fps の dropdown 行を最下段に持つので背が高い。
/// dropdown の popup は panel 下端より下 (= 暗転 overlay、 widget 無し) に開くため、
/// ボタン行を dropdown 行の **上** に置く (popup と button の z / 入力衝突を構造的に回避)。
const PANEL_H_MP4: f32 = 220.0;
const PAD: f32 = 18.0;
const ROW_H: f32 = 26.0;
const FIELD_W: f32 = 130.0;
const LABEL_W: f32 = 60.0;
const BTN_H: f32 = 28.0;
const BTN_W: f32 = 110.0;
/// 「全曲にリセット」 は全角 7 文字 = 7 * 16 = 112px あり、 BTN_W(110) では
/// 「全曲にリセッ…」 と省略されて意味が読めなくなるので専用幅を持つ。
const RESET_BTN_W: f32 = 128.0;

const MODAL_STYLE: ModalStyle = ModalStyle {
    overlay_color: theme::BACKDROP,
    panel_bg: theme::PANEL,
    panel_radius: 6.0,
    close_on_outside_click: false,
    close_on_escape: false,
};

/// start / end 拍フィールドの scrubable_number スタイル。 sensitivity 0.25 =
/// `1 px drag で 0.25 拍`。 clamp は handler 側 SSoT なので widget range は緩く
/// 0..=大きな上限 (実 song 長は handler が clamp する)。
const SCRUB_STYLE: ScrubableNumberStyle = ScrubableNumberStyle {
    bg_color: theme::INSET_BG,
    bg_color_hovered: theme::CONTROL,
    // scrub drag 中の active feedback。 muted blue band (bright ACCENT より沈める意図) で、
    // 一致する opaque token が無いため固有色を残す (transport の dragging tinge と同方針)。
    bg_color_dragging: Color { r: 0.20, g: 0.32, b: 0.45, a: 1.0 },
    text_color: COLOR_TEXT,
    border: theme::BORDER,
    border_width: 1.0,
    radius: 3.0,
    font_size: 13.0,
    sensitivity: 0.25,
    range: None,
};

/// video export の出力解像度プリセット (label, width, height)。 すべて
/// 偶数 (H.264 yuv420p は幅・高さが偶数必須)。 dropdown の closed 表示と選択肢の
/// 両方に使う。 既定 (= プロジェクト現在値 1920x1080) は `RES_DEFAULT_IDX`。
const RES_PRESETS: &[(&str, u32, u32)] = &[
    ("3840 × 2160 (4K UHD)", 3840, 2160),
    ("2560 × 1440 (QHD)", 2560, 1440),
    ("1920 × 1080 (FHD)", 1920, 1080),
    ("1280 × 720 (HD)", 1280, 720),
    ("854 × 480 (SD)", 854, 480),
    ("1080 × 1920 (縦型 9:16)", 1080, 1920),
    ("1080 × 1080 (正方形 1:1)", 1080, 1080),
];
/// FHD の `RES_PRESETS` index。 picker 値が一覧に無い (理論上のみ) ときの fallback。
const RES_DEFAULT_IDX: usize = 2;

/// video export の出力フレームレートプリセット (label, fps)。 整数のみ。
const FPS_PRESETS: &[(&str, f32)] = &[
    ("24", 24.0),
    ("25", 25.0),
    ("30", 30.0),
    ("50", 50.0),
    ("60", 60.0),
];
/// 30 fps の `FPS_PRESETS` index。 picker 値が一覧に無いときの fallback。
const FPS_DEFAULT_IDX: usize = 2;

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, _screen: PhysicalSize) {
    let Some(picker) = app.ui_ephemeral.export_range_picker else {
        if ui.is_modal_open("export_range") {
            ui.close_modal("export_range");
        }
        return;
    };
    if !ui.is_modal_open("export_range") {
        ui.open_modal("export_range");
    }

    let time_sig = app.song_doc.song().time_sig;
    let kind = picker.kind;
    let start_beat = picker.start_beat;
    let end_beat = picker.end_beat;
    // dblclick リセット時の default: 開始=曲頭(0)、 終了=曲末(length_beats)。
    let song_len = app.song_doc.song().length_beats;
    // Mp4 のときだけ出力解像度 / fps の dropdown を出す。 値は picker が
    // 保持する per-export override (open 時に Song から seed 済み)。
    let is_mp4 = matches!(kind, ExportRangeKind::Mp4);
    let resolution = picker.resolution;
    let framerate = picker.framerate;
    let panel_size = if is_mp4 { (PANEL_W, PANEL_H_MP4) } else { (PANEL_W, PANEL_H) };

    ui.modal(
        "export_range",
        panel_size,
        &MODAL_STYLE,
        None,
        move |ui, panel| {
            let title = match kind {
                ExportRangeKind::Wav => "WAV 書き出し範囲",
                ExportRangeKind::Mp4 => "Video 書き出し設定",
            };
            ui.label_at(
                "exr_title",
                title,
                panel.x + PAD,
                panel.y + PAD,
                16.0,
                COLOR_TEXT,
            );

            // ESC = キャンセル (close_on_escape: false なので body で拾う → フラッシュ回避)。
            if ui.take_shortcut("escape") {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::CancelExportRange)
                }));
            }

            // ---- 開始拍 ----
            let row0_y = panel.y + PAD + 34.0;
            field_row(
                ui,
                "exr_start",
                "開始",
                // field_row はラベルを `field_rect.x - LABEL_W` に描くので、
                // field 自体をラベル幅ぶん右に置かないとラベルがパネル外に出る。
                Rect { x: panel.x + PAD + LABEL_W, y: row0_y, w: FIELD_W, h: ROW_H },
                start_beat,
                0.0,
                time_sig,
                |v| {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetExportRangeStart(v))
                    })
                },
            );

            // ---- 終了拍 ----
            let row1_y = row0_y + ROW_H + 12.0;
            field_row(
                ui,
                "exr_end",
                "終了",
                Rect { x: panel.x + PAD + LABEL_W, y: row1_y, w: FIELD_W, h: ROW_H },
                end_beat,
                song_len,
                time_sig,
                |v| {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetExportRangeEnd(v))
                    })
                },
            );

            // 「全曲」 リセットリンク (右上寄り)。
            let reset_rect = Rect {
                x: panel.x + panel.w - PAD - RESET_BTN_W,
                y: row0_y,
                w: RESET_BTN_W,
                h: ROW_H,
            };
            if ui.button_at_clicked("exr_reset", "全曲にリセット", reset_rect) {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::ResetExportRange)
                }));
            }

            // ---- ボタン行 (右: Export / 左: キャンセル) ----
            // Mp4 は dropdown 行を最下段に置く (popup が panel 下端より下の
            // 暗転領域に開けるよう、 button より下に widget を置かない)。 ボタンは終了行
            // の直下に上げる。 Wav は従来どおり panel 最下部。
            let btn_y = if is_mp4 {
                row1_y + ROW_H + 16.0
            } else {
                panel.y + panel.h - PAD - BTN_H
            };
            let export_x = panel.x + panel.w - PAD - BTN_W;
            if ui.button_at_clicked(
                "exr_confirm",
                "書き出す...",
                Rect { x: export_x, y: btn_y, w: BTN_W, h: BTN_H },
            ) {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::ConfirmExportRange)
                }));
            }
            let cancel_x = export_x - BTN_W - 10.0;
            if ui.button_at_clicked(
                "exr_cancel",
                "キャンセル",
                Rect { x: cancel_x, y: btn_y, w: BTN_W, h: BTN_H },
            ) {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::CancelExportRange)
                }));
            }

            // ---- 出力解像度 / fps (Mp4 のみ、 最下段に side-by-side) ----
            // dropdown を最後に描く + panel 下端 (widget 無し) に popup を開かせるので、
            // popup と他 widget の z / 入力衝突が起きない。 popup は modal の z 上で出る
            // (modal body と同じ popup_buffer の後方 = 最前面)。
            if is_mp4 {
                let dd_y = btn_y + BTN_H + 16.0;
                // 解像度
                ui.label_at(
                    "exr_res_label",
                    "解像度",
                    panel.x + PAD,
                    dd_y + 6.0,
                    13.0,
                    COLOR_TEXT,
                );
                let res_labels: Vec<&str> =
                    RES_PRESETS.iter().map(|(l, _, _)| *l).collect();
                let res_sel = RES_PRESETS
                    .iter()
                    .position(|(_, w, h)| *w == resolution.0 && *h == resolution.1)
                    .unwrap_or(RES_DEFAULT_IDX);
                // 最長プリセット "1080 × 1080 (正方形 1:1)" = 177.2px。 dropdown の
                // 文字領域は w - PAD_X(8) - ARROW_W(16) なので 202px 以上必要。
                let res_rect = Rect { x: panel.x + PAD + 56.0, y: dd_y, w: 204.0, h: ROW_H };
                if let Some(idx) = ui.dropdown("exr_res_dd", res_rect, &res_labels, res_sel) {
                    let (_, w, h) = RES_PRESETS[idx];
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetExportResolution(w, h))
                    }));
                }
                // fps
                let fps_label_x = res_rect.x + res_rect.w + 16.0;
                ui.label_at(
                    "exr_fps_label",
                    "fps",
                    fps_label_x,
                    dd_y + 6.0,
                    13.0,
                    COLOR_TEXT,
                );
                let fps_labels: Vec<&str> = FPS_PRESETS.iter().map(|(l, _)| *l).collect();
                let fps_sel = FPS_PRESETS
                    .iter()
                    .position(|(_, f)| (*f - framerate).abs() < 0.001)
                    .unwrap_or(FPS_DEFAULT_IDX);
                let fps_rect = Rect { x: fps_label_x + 30.0, y: dd_y, w: 58.0, h: ROW_H };
                if let Some(idx) = ui.dropdown("exr_fps_dd", fps_rect, &fps_labels, fps_sel) {
                    let (_, f) = FPS_PRESETS[idx];
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetExportFramerate(f))
                    }));
                }
            }
        },
    );
}

/// 1 行 = ラベル + scrubable な **小節.拍** フィールド + 単位ヒント。 内部値は拍
/// (beat) のままで、 表示/入力だけ 1-based 小節.拍 (例 "3.1")。 `on_change` は
/// 新しい拍値を受けて `Edit` を作る (clamp は handler 側)。
#[allow(clippy::too_many_arguments)]
fn field_row<F>(
    ui: &mut Ui<'_, AppData>,
    id: &'static str,
    label: &'static str,
    field_rect: Rect,
    value: f64,
    default_value: f64,
    time_sig: (u8, u8),
    on_change: F,
) where
    F: Fn(f64) -> Edit<AppData> + Clone + Send + Sync + 'static,
{
    ui.label_at(
        (id, "label"),
        label,
        field_rect.x - LABEL_W,
        field_rect.y + 6.0,
        13.0,
        COLOR_TEXT,
    );
    // フィールド自体を 1-based 小節.拍 表記に (例 "3.1")。 拍↔小節換算は song の
    // time signature 由来 (ruler / transport と同じ SSoT)。
    let beats_per_bar = common::timing::beats_per_bar(time_sig);
    let _ = ui.scrubable_number_at(
        (id, "field"),
        field_rect,
        value,
        default_value,
        ScrubableNumberFormat::BarBeat { beats_per_bar },
        &SCRUB_STYLE,
        on_change,
        None,
        None,
    );
    // 単位ヒント (フィールドが 1-based 小節.拍 表記であることを示す)。
    ui.label_at(
        (id, "hint"),
        "小節.拍",
        field_rect.x + field_rect.w + 12.0,
        field_rect.y + 6.0,
        12.0,
        COLOR_HINT,
    );
}
