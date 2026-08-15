//! 未保存変更がある状態で「現在のプロジェクトを破棄する操作」 (= 終了 / New /
//! Open / Open Recent) を行おうとしたときの確認モーダル。
//! ふつうの DAW (Ableton / Bitwig / Logic 等) と同じく「保存して続行 /
//! 保存せず続行 / キャンセル」 の 3 択をユーザーに選ばせる。
//!
//! 旧 `close_confirm_modal` (終了専用) を一般化し、 `AppData::dirty_guard`
//! (`Option<DirtyGuardAction>`) が `Some` の間モーダルを出す。 終了
//! (`Quit`) のときは文言が「終了」、 New / Open のときは「続行」 になる。
//!
//! `Ui::modal` + `button_at_clicked` パターン (recovery_modal と同形)。
//! Esc / 外クリック / ✕ ボタンはすべて `DirtyGuardCancel` を発火し、
//! 操作を取りやめる。

use daw_ui_core::{Edit, ModalStyle, Ui};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent, DirtyGuardAction};

const PANEL_W: f32 = 460.0;
const PANEL_H: f32 = 176.0;
const PAD: f32 = 16.0;
const TITLE_H: f32 = 28.0;
const BTN_H: f32 = 28.0;
/// 「保存して終了」「保存して続行」 は長いので幅広に。 他 2 つは同幅。
const BTN_SAVE_W: f32 = 136.0;
const BTN_W: f32 = 116.0;
const BTN_GAP: f32 = 8.0;

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, _screen: PhysicalSize) {
    let Some(action) = app.ui_ephemeral.dirty_guard.as_ref() else {
        return;
    };
    if !ui.is_modal_open("dirty_guard") {
        ui.open_modal("dirty_guard");
    }

    // 保留中の操作に応じた動詞。 終了は「終了」、 New / Open は「続行」。
    let verb = match action {
        DirtyGuardAction::Quit => "終了",
        DirtyGuardAction::New | DirtyGuardAction::Open | DirtyGuardAction::OpenPath(_) => "続行",
    };
    let question = match action {
        DirtyGuardAction::Quit => "閉じる前に保存しますか？",
        DirtyGuardAction::New | DirtyGuardAction::Open | DirtyGuardAction::OpenPath(_) => {
            "続ける前に保存しますか？"
        }
    };
    let save_label = format!("保存して{verb}");
    let discard_label = format!("保存せず{verb}");

    // 表示用プロジェクト名 (未保存新規は "Untitled")。 = いま破棄しようとしている
    // (未保存変更を持つ) プロジェクト。
    let project_name = app
        .song_doc.file_path
        .as_ref()
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .unwrap_or("Untitled")
        .to_string();

    // 暗幕 + 中央パネル + Esc/外クリックで閉じる = ui-core の既定そのものなので
    // パレットから素直に組む (r.md #48: 色を焼き込んだ const は runtime テーマを読めない)。
    let style = ModalStyle::from_palette(&app.theme.core);

    ui.modal(
        "dirty_guard",
        (PANEL_W, PANEL_H),
        &style,
        Some(Box::new(|| {
            Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::DirtyGuardCancel))
        })),
        move |ui, panel| {
            // 文字が乗るのは modal panel (= パレットのクローム面) なので本文インクは `text`。
            let p = ui.palette();

            // タイトル
            ui.label_at("dg_title", "保存の確認", panel.x + PAD, panel.y + PAD, 16.0, p.text);

            // メッセージ (2 行)。 固定文だけで 329px あり、 プロジェクト名に
            // 使える余地は 115px しかない。 素の label_at だと少し長い名前で
            // パネル右端を越えて暗転 backdrop の上まで文字が伸びるので rect で切る。
            let msg_w = (PANEL_W - PAD * 2.0).max(1.0);
            ui.label_at_clipped(
                "dg_msg1",
                &format!("プロジェクト「{project_name}」に保存していない変更があります。"),
                Rect { x: panel.x + PAD, y: panel.y + PAD + TITLE_H, w: msg_w, h: 13.0 * 1.2 },
                13.0,
                p.text,
            );
            ui.label_at_clipped(
                "dg_msg2",
                question,
                Rect {
                    x: panel.x + PAD,
                    y: panel.y + PAD + TITLE_H + 22.0,
                    w: msg_w,
                    h: 13.0 * 1.2,
                },
                13.0,
                p.text,
            );

            // ボタン行 (下部、 右寄せ)。 左から「保存して〜」「保存せず〜」
            // 「キャンセル」。 主要操作 (保存) を左端に置く。
            let btn_y = panel.y + panel.h - PAD - BTN_H;
            let cancel_x = panel.x + panel.w - PAD - BTN_W;
            let discard_x = cancel_x - BTN_GAP - BTN_W;
            let save_x = discard_x - BTN_GAP - BTN_SAVE_W;

            if ui.button_at_clicked(
                "dg_save",
                &save_label,
                Rect { x: save_x, y: btn_y, w: BTN_SAVE_W, h: BTN_H },
            ) {
                ui.close_modal("dirty_guard");
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::DirtyGuardSave)
                }));
            }

            if ui.button_at_clicked(
                "dg_discard",
                &discard_label,
                Rect { x: discard_x, y: btn_y, w: BTN_W, h: BTN_H },
            ) {
                ui.close_modal("dirty_guard");
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::DirtyGuardDiscard)
                }));
            }

            if ui.button_at_clicked(
                "dg_cancel",
                "キャンセル",
                Rect { x: cancel_x, y: btn_y, w: BTN_W, h: BTN_H },
            ) {
                ui.close_modal("dirty_guard");
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::DirtyGuardCancel)
                }));
            }
        },
    );
}
