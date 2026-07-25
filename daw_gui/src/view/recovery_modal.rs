//! Crash recovery modal。 起動時 (or Open 時) に検出された autosave 候補を
//! 一覧表示し、 各候補ごとに「復元」 / 「破棄」 をユーザーに選ばせる。
//!
//! `Ui::modal` + `button_at_clicked + close_modal` パターン
//! (gui_01 #015 解決後の標準形)。 Esc / outside click / ✕ ボタン全てで
//! `RecoveryDismiss` を発火。

use daw_ui_core::{Edit, ModalStyle, Ui};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, Rect};
use crate::theme;

use crate::app::{AppData, AppEvent};

const COLOR_TEXT: Color = theme::TEXT;

const PANEL_W: f32 = 720.0;
const ROW_H: f32 = 60.0;
/// テキスト表示エリアの右端 (= 右側 button 列の左端)。 row.x からの相対 px、
/// label_at が clip_rect を持たないため文字列 truncate の判断に使う。
const TEXT_AREA_END: f32 = PANEL_W - PAD * 2.0 - BTN_W * 2.0 - 16.0;
const TITLE_H: f32 = 36.0;
const FOOTER_H: f32 = 36.0;
const BTN_W: f32 = 64.0;
const BTN_H: f32 = 24.0;
const PAD: f32 = 12.0;
const ROW_GAP: f32 = 6.0;

const MODAL_STYLE: ModalStyle = ModalStyle {
    overlay_color: theme::BACKDROP,
    panel_bg: theme::PANEL,
    panel_radius: 6.0,
    close_on_outside_click: true,
    close_on_escape: true,
};

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, _screen: PhysicalSize) {
    if !app.ui_ephemeral.show_recovery_modal {
        // 「復元」/「破棄」ボタンは Edit 経由で app flag だけ落とすので、 modal の
        // close はここで同期する (export_overlay と同 idiom)。 これを怠ると、 最後の
        // 候補を処理した次フレームから `ui.modal()` が呼ばれなくなり、 open_popups に
        // 残った capturing modal が全入力 (マウス+キーボード) を永久遮断する。
        if ui.is_modal_open("recovery") {
            ui.close_modal("recovery");
        }
        return;
    }
    if !ui.is_modal_open("recovery") {
        ui.open_modal("recovery");
    }

    // 高さは候補数で動的に決まる。 上限なし (16 件超えたら scroll 入れたいが
    // 通常 1〜2 件のみのはず)
    let n = app.ui_ephemeral.recovery_candidates.len() as f32;
    let panel_h = TITLE_H + (ROW_H + ROW_GAP) * n + FOOTER_H + PAD * 2.0;

    ui.modal(
        "recovery",
        (PANEL_W, panel_h),
        &MODAL_STYLE,
        Some(Box::new(|| {
            Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::RecoveryDismiss)
            })
        })),
        |ui, panel| {
            // タイトル
            ui.label_at(
                "rec_title",
                "Crash recovery — 復元する候補があります",
                panel.x + PAD,
                panel.y + PAD,
                16.0,
                COLOR_TEXT,
            );

            // 候補 row
            let mut y = panel.y + TITLE_H + PAD;
            // clone してから iterate (closure 内で AppData mut 参照を作るため)
            let candidates = app.ui_ephemeral.recovery_candidates.clone();
            for (i, candidate) in candidates.iter().enumerate() {
                let row_rect = Rect {
                    x: panel.x + PAD,
                    y,
                    w: panel.w - PAD * 2.0,
                    h: ROW_H,
                };

                // ファイル名 (1 行目) / フルパス (2 行目)。 幅は「1 char ≒ 7px /
                // 5px」 の文字数見積りではなく rect で切る。 旧見積りは font 13 の
                // 実 advance (半角 6.85 / 全角 13.7) と合わず、 純 ASCII の長いパス
                // (108 文字許可 = 569px > 552px) でも和名でもボタン列の上に重なっていた。
                let text_w = (TEXT_AREA_END - 8.0).max(1.0);
                ui.label_at_clipped(
                    ("rec_name", i),
                    &display_name(candidate),
                    Rect { x: row_rect.x + 8.0, y: row_rect.y + 6.0, w: text_w, h: 13.0 * 1.2 },
                    13.0,
                    COLOR_TEXT,
                );
                ui.label_at_clipped(
                    ("rec_path", i),
                    &candidate.display().to_string(),
                    Rect { x: row_rect.x + 8.0, y: row_rect.y + 26.0, w: text_w, h: 10.0 * 1.2 },
                    10.0,
                    COLOR_TEXT,
                );

                // ボタン (右寄せ): 「復元」 / 「破棄」
                let btn_y = row_rect.y + (row_rect.h - BTN_H) * 0.5;
                let restore_x = row_rect.x + row_rect.w - BTN_W * 2.0 - 8.0;
                let discard_x = row_rect.x + row_rect.w - BTN_W;

                let restore_path = candidate.clone();
                ui.button_at(
                    ("rec_restore", i),
                    "復元",
                    Rect { x: restore_x, y: btn_y, w: BTN_W, h: BTN_H },
                    move || {
                        let restore_path = restore_path.clone();
                        Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::RecoveryRestore(restore_path))
                        })
                    },
                );

                let discard_path = candidate.clone();
                ui.button_at(
                    ("rec_discard", i),
                    "破棄",
                    Rect { x: discard_x, y: btn_y, w: BTN_W, h: BTN_H },
                    move || {
                        let discard_path = discard_path.clone();
                        Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::RecoveryDiscard(discard_path))
                        })
                    },
                );

                y += ROW_H + ROW_GAP;
            }

            // 下部 「閉じる」 ボタン (右寄せ)。 button_at_clicked + close_modal で
            // 確実に modal state を閉じる + on_close で RecoveryDismiss を発火。
            let close_w = 96.0;
            let close_x = panel.x + panel.w - PAD - close_w;
            let close_y = panel.y + panel.h - PAD - BTN_H;
            if ui.button_at_clicked(
                "rec_close",
                "閉じる",
                Rect { x: close_x, y: close_y, w: close_w, h: BTN_H },
            ) {
                ui.close_modal("recovery");
            }
        },
    );
}

// 旧 `truncate_display` (文字数見積りで切る) は `label_at_clipped` の実 advance
// ベースの ellipsis に置き換えたので撤去した。

/// candidate path の表示名 (ユーザー向けの短いラベル)。 sidecar なら元 .daw
/// の name + " (autosave)"、 recovery_dir なら「未保存セッション + ファイル名」 を返す。
fn display_name(p: &std::path::Path) -> String {
    if let Some(orig) = common::recovery::original_file_for_sidecar(p) {
        let name = orig
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("(unknown)");
        format!("{name} (sidecar autosave)")
    } else {
        let name = p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("(unknown)");
        format!("未保存プロジェクト: {name}")
    }
}
