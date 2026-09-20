//! Send 宛先トラックピッカー (modal overlay)。`plugin_picker.rs` を踏襲した
//! `Ui::modal` + `Ui::list_view` 構成。
//!
//! root.rs から常時呼ばれる。`app.cur.peph.send_picker == Some(..)` のとき modal を
//! 開き、ESC / outside click / Close ボタンで閉じる。宛先 track を選ぶと
//! `AppEvent::AddSend { src_track_id, dest_track_id }` を発行して閉じる。
//!
//! 既存 track の候補は `AppData::send_destination_candidates` が生成する
//! (= 自分自身とルーティング閉路を作る track を除外済み)。 その先頭に
//! 「＋ 新規トラックに送る」 (`AppEvent::AddSendToNewTrack`) を置く — r.md #136 で
//! ミキサーの「＋ Return」 列を廃したので、 **送り先を新しく作る口はここ 1 つ**。

use daw_ui_core::{Edit, ListViewStyle, ModalStyle, Ui};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent};

/// 一覧 1 行が指す送り先。 既存 track か、 「これから作る 1 本」 か。
#[derive(Clone, Copy)]
enum Dest {
    NewTrack,
    Existing(u32),
}

const PANEL_W: f32 = 420.0;
const PANEL_H: f32 = 420.0;
const TITLE_H: f32 = 36.0;

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, _screen: PhysicalSize) {
    let Some(state) = app.cur.peph.send_picker else {
        return;
    };
    let src_track_id = state.src_track_id;

    if !ui.is_modal_open("send_picker") {
        ui.open_modal("send_picker");
    }

    // 候補は毎フレーム派生 (= AppData は plain struct、 Memo 不使用)。 候補数は
    // track 数オーダーなので per-frame 再計算で十分。 先頭行は「新規トラックを
    // 作ってそこへ送る」 (= 既存 track が 1 本も無くても必ず選べる)。
    let rows: Vec<(Dest, String)> = std::iter::once((
        Dest::NewTrack,
        "\u{ff0b} \u{65b0}\u{898f}\u{30c8}\u{30e9}\u{30c3}\u{30af}\u{306b}\u{9001}\u{308b}".to_string(),
    ))
    .chain(
        app.send_destination_candidates(src_track_id)
            .into_iter()
            .map(|(id, name)| (Dest::Existing(id), name)),
    )
    .collect();

    // スタイルは const にできない (runtime テーマを読めない、 r.md #48)。 パレット既定を
    // ベースに、 このピッカー固有の寸法だけ差分で上書きする。
    let p = &app.theme.core;
    let modal_style = ModalStyle::from_palette(p);
    let list_style = ListViewStyle { radius: 3.0, ..ListViewStyle::from_palette(p) };

    ui.modal(
        "send_picker",
        (PANEL_W, PANEL_H),
        &modal_style,
        Some(Box::new(|| {
            Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::CloseSendPicker))
        })),
        |ui, panel| {
            let pad = 12.0;

            ui.label_at(
                "sp_title",
                "Send to track",
                panel.x + pad,
                panel.y + pad,
                16.0,
                p.text,
            );
            let close_w = 32.0;
            let close_x = panel.x + panel.w - pad - close_w;
            // plugin_picker と同じく ✕ は close_modal を呼ぶ → modal on_close が
            // 次フレームで CloseSendPicker を 1 度発火する (二重発火回避)。
            if ui.button_at_clicked(
                "sp_close",
                "x",
                Rect { x: close_x, y: panel.y + pad - 2.0, w: close_w, h: 24.0 },
            ) {
                ui.close_modal("send_picker");
            }

            // 一覧 (タイトルの下)
            let list_y = panel.y + TITLE_H + 6.0;
            let list_rect = Rect {
                x: panel.x + pad,
                y: list_y,
                w: panel.w - pad * 2.0,
                h: panel.y + panel.h - pad - list_y,
            };

            let resp = ui.list_view(
                "sp_list",
                list_rect,
                &rows,
                None,
                &list_style,
                |ui, entry, i, row_rect, is_selected| {
                    // 行背景は list_view が塗るクローム面なので、 本文は `p.text` /
                    // 選択行は accent 塗りの上なので auto-contrast で取る。
                    let name_color = if is_selected { p.ink_on_accent() } else { p.text };
                    ui.label_at(
                        ("sp_row_name", i),
                        &entry.1,
                        row_rect.x + 10.0,
                        row_rect.y + 6.0,
                        12.0,
                        name_color,
                    );
                },
            );
            if let Some(idx) = resp.clicked
                && let Some(&(dest, _)) = rows.get(idx)
            {
                // 送り先が既存か新規かで発行するイベントだけ変える (閉じる手順は同じ)。
                let add = match dest {
                    Dest::NewTrack => AppEvent::AddSendToNewTrack { src_track_id },
                    Dest::Existing(dest_track_id) => {
                        AppEvent::AddSend { src_track_id, dest_track_id }
                    }
                };
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(add);
                    app.handle_event(AppEvent::CloseSendPicker);
                }));
                ui.close_modal("send_picker");
            }
        },
    );
}
