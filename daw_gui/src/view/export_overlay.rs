//! Export 中の進捗オーバーレイ。
//!
//! 2 種類の export を 1 つの modal で見せる:
//! - **標準 WAV export** (`FileDialogKind::ExportWav`): daw_audio が freewheel で
//!   音声を書き出す。`ExportStage::AudioRender` の determinate 進捗 + Cancel。
//! - **Video export** (`action_export_mp4`): 前段 = 音声 freewheel
//!   (`AudioRender`)、後段 = 映像フレーム render (`VideoRender`)。どちらも
//!   determinate 進捗 + Cancel。
//!
//! `export_stage` が `Some` の間だけ dismissable でない true modal として表示し、
//! 下の UI 操作（再生・編集・FX 追加）をブロックする。Esc / Cancel ボタンのみ
//! 中断要求になる（Esc / 外クリックでは閉じない）。Cancel は AudioRender なら
//! daw_audio へ IPC で、VideoRender なら in-process flag で render を中断させる。

use daw_ui_core::{Edit, ModalStyle, Ui};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, Rect};
use crate::theme;

use crate::app::{AppData, AppEvent, ExportStage};

const COLOR_TEXT: Color = theme::TEXT;
const BAR_BG: Color = theme::INSET_BG;
const BAR_FILL: Color = theme::ACCENT;

const PANEL_W: f32 = 420.0;
const PANEL_H: f32 = 150.0;
const PAD: f32 = 16.0;
const BTN_H: f32 = 28.0;
const BTN_W: f32 = 110.0;

const MODAL_STYLE: ModalStyle = ModalStyle {
    overlay_color: theme::BACKDROP,
    panel_bg: theme::PANEL,
    panel_radius: 6.0,
    close_on_outside_click: false,
    close_on_escape: false,
};

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, _screen: PhysicalSize) {
    // export 全体（音声 freewheel → 映像 render）で modal を出し、下の UI 操作を
    // ブロックする。`ui.modal` は真のモーダルで pointer 入力を遮断する
    // (gui_01 `pointer_blocked_by_modal_popup`)。
    let stage = app.transport.export_stage;
    // `pending_video_export` だけ立って stage 未設定の窓は実際には起きない
    // (`action_begin_export_mp4` が AudioRender を同時に立てる) が、防御的に
    // active 判定へ含めて取りこぼしを防ぐ。
    let active = stage.is_some() || app.transport.pending_video_export.is_some();
    if !active {
        if ui.is_modal_open("export_progress") {
            ui.close_modal("export_progress");
        }
        return;
    }
    if !ui.is_modal_open("export_progress") {
        ui.open_modal("export_progress");
    }
    // タイトル: video export (前段の音声フェーズ含む) か、標準 WAV export か。
    let is_video = app.transport.pending_video_export.is_some()
        || matches!(stage, Some(ExportStage::VideoRender { .. }));
    let title = if is_video {
        "Video export 中..."
    } else {
        "WAV 書き出し中..."
    };
    ui.modal(
        "export_progress",
        (PANEL_W, PANEL_H),
        &MODAL_STYLE,
        None,
        move |ui, panel| {
            ui.label_at(
                "exp_title",
                title,
                panel.x + PAD,
                panel.y + PAD,
                16.0,
                COLOR_TEXT,
            );
            // ESC は Cancel ボタンと同じ「キャンセル要求」にする。modal を即閉じ
            // しない（`close_on_escape: false`）ことで、close→次フレーム再 open の
            // フラッシュを避ける。CancelExport で中断要求が伝わり、完了通知で
            // `active` が false になって自然に閉じる。
            if ui.take_shortcut("escape") {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::CancelExport)
                }));
            }
            match stage {
                Some(ExportStage::VideoRender { done, total }) => {
                    let pct = fraction(done, total);
                    draw_progress(
                        ui,
                        panel,
                        &format!("{done} / {total} フレーム ({:.0}%)", pct * 100.0),
                        pct,
                    );
                }
                Some(ExportStage::AudioRender { done, total }) => {
                    let pct = fraction(done, total);
                    // total==0 (空 song 等) では割合が無意味なのでパーセント非表示。
                    let count_text = if total > 0 {
                        format!("音声をレンダリング中... ({:.0}%)", pct * 100.0)
                    } else {
                        "音声をレンダリング中...".to_string()
                    };
                    draw_progress(ui, panel, &count_text, pct);
                }
                None => {
                    // pending_video_export だけ立っている理論上の窓
                    // (= AudioRender が立つ前)。indeterminate 表示。
                    ui.label_at(
                        "exp_count",
                        "準備中...",
                        panel.x + PAD,
                        panel.y + PAD + 28.0,
                        13.0,
                        COLOR_TEXT,
                    );
                }
            }
        },
    );
}

/// 進捗テキスト + バー (bg + fill) + Cancel ボタンを描く。AudioRender /
/// VideoRender 共通レイアウト。
fn draw_progress(ui: &mut Ui<'_, AppData>, panel: Rect, count_text: &str, pct: f32) {
    ui.label_at(
        "exp_count",
        count_text,
        panel.x + PAD,
        panel.y + PAD + 28.0,
        13.0,
        COLOR_TEXT,
    );
    // 進捗バー（bg + fill）。
    let bar_y = panel.y + PAD + 54.0;
    let bar_w = panel.w - PAD * 2.0;
    let bar_h = 12.0;
    ui.panel(
        "exp_bar_bg",
        Rect { x: panel.x + PAD, y: bar_y, w: bar_w, h: bar_h },
        BAR_BG,
        3.0,
    );
    ui.panel(
        "exp_bar_fill",
        Rect { x: panel.x + PAD, y: bar_y, w: bar_w * pct, h: bar_h },
        BAR_FILL,
        3.0,
    );
    // Cancel ボタン（右下）。
    let btn_y = panel.y + panel.h - PAD - BTN_H;
    let btn_x = panel.x + panel.w - PAD - BTN_W;
    if ui.button_at_clicked(
        "exp_cancel",
        "キャンセル",
        Rect { x: btn_x, y: btn_y, w: BTN_W, h: BTN_H },
    ) {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::CancelExport)
        }));
    }
}

/// `done / total` を 0.0..=1.0 にクランプ。`total == 0` は 0.0。
fn fraction(done: u64, total: u64) -> f32 {
    if total > 0 {
        (done as f32 / total as f32).clamp(0.0, 1.0)
    } else {
        0.0
    }
}
