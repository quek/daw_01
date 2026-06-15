//! FIXME #55: Export WAV / Video の前に出す「書き出し範囲」 ピッカーモーダル。
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

use crate::app::{AppData, AppEvent, ExportRangeKind};

const COLOR_TEXT: Color = Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 };
const COLOR_HINT: Color = Color { r: 0.62, g: 0.65, b: 0.72, a: 1.0 };

const PANEL_W: f32 = 380.0;
const PANEL_H: f32 = 210.0;
const PAD: f32 = 18.0;
const ROW_H: f32 = 26.0;
const FIELD_W: f32 = 130.0;
const LABEL_W: f32 = 60.0;
const BTN_H: f32 = 28.0;
const BTN_W: f32 = 110.0;

const MODAL_STYLE: ModalStyle = ModalStyle {
    overlay_color: Color { r: 0.0, g: 0.0, b: 0.0, a: 0.6 },
    panel_bg: Color { r: 0.18, g: 0.18, b: 0.22, a: 1.0 },
    panel_radius: 6.0,
    close_on_outside_click: false,
    close_on_escape: false,
};

/// start / end 拍フィールドの scrubable_number スタイル。 sensitivity 0.25 =
/// `1 px drag で 0.25 拍`。 clamp は handler 側 SSoT なので widget range は緩く
/// 0..=大きな上限 (実 song 長は handler が clamp する)。
const SCRUB_STYLE: ScrubableNumberStyle = ScrubableNumberStyle {
    bg_color: Color { r: 0.12, g: 0.13, b: 0.17, a: 1.0 },
    bg_color_hovered: Color { r: 0.17, g: 0.18, b: 0.23, a: 1.0 },
    bg_color_dragging: Color { r: 0.20, g: 0.32, b: 0.45, a: 1.0 },
    text_color: COLOR_TEXT,
    border: Color { r: 0.32, g: 0.35, b: 0.42, a: 1.0 },
    border_width: 1.0,
    radius: 3.0,
    font_size: 13.0,
    sensitivity: 0.25,
    range: None,
};

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, _screen: PhysicalSize) {
    let Some(picker) = app.export_range_picker else {
        if ui.is_modal_open("export_range") {
            ui.close_modal("export_range");
        }
        return;
    };
    if !ui.is_modal_open("export_range") {
        ui.open_modal("export_range");
    }

    let time_sig = app.song.time_sig;
    let kind = picker.kind;
    let start_beat = picker.start_beat;
    let end_beat = picker.end_beat;
    // dblclick リセット時の default: 開始=曲頭(0)、 終了=曲末(length_beats)。
    let song_len = app.song.length_beats;

    ui.modal(
        "export_range",
        (PANEL_W, PANEL_H),
        &MODAL_STYLE,
        None,
        move |ui, panel| {
            let title = match kind {
                ExportRangeKind::Wav => "WAV 書き出し範囲",
                ExportRangeKind::Mp4 => "Video 書き出し範囲",
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
                Rect { x: panel.x + PAD, y: row0_y, w: FIELD_W, h: ROW_H },
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
                Rect { x: panel.x + PAD, y: row1_y, w: FIELD_W, h: ROW_H },
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
                x: panel.x + panel.w - PAD - BTN_W,
                y: row0_y,
                w: BTN_W,
                h: ROW_H,
            };
            if ui.button_at_clicked("exr_reset", "全曲にリセット", reset_rect) {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::ResetExportRange)
                }));
            }

            // ---- ボタン行 (右下: Export / 左: キャンセル) ----
            let btn_y = panel.y + panel.h - PAD - BTN_H;
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
        "Export range",
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
