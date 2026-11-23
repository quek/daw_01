//! MIDI Learn の割り当て一覧。トランスポートの「Learn」ボタンを右クリックすると、その直下に
//! `Song::midi_bindings` を 1 行ずつ出す (入力 / チャンネル / 宛先の言葉と、その 1 件を消す ×)。
//!
//! `open_overlay` (`chord_popup` / `resource_monitor` と同じ) なので、開いている間も再生と shortcut は
//! 背景に通り、背景への click は通さない。外クリック / Esc / Learn の上のもう 1 度の右クリックで閉じる
//! ([`ListState`])。開閉は daw-ui の popup 状態だけが持つ (Song にも session にも載せない — 見方の都合)。

use daw_ui_core::{Edit, Ui, WidgetId};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent};

/// overlay の id (テストが開閉を見る)。
pub const OVERLAY_ID: &str = "midi_bindings_list";
/// 割り当てが 1 件も無いときの文言。
const EMPTY_TEXT: &str = "割り当てはありません";
const ROW_H: f32 = 22.0;
const PAD: f32 = 6.0;
const FONT: f32 = 11.0;
const REMOVE_W: f32 = 20.0;
const MIN_W: f32 = 200.0;

/// 開閉の記憶 (daw-ui の widget state)。
#[derive(Default)]
struct ListState {
    /// 開いていた一覧を、いま押されているボタンの press が外クリックとして閉じた。右クリックは press で
    /// 閉じて release で確定する (`take_secondary_click_in_rect`) ので、Learn の上の 2 度目の右クリックは
    /// これが無いと release で開き直す (閉じてすぐ開く)。ボタンを離したフレームで消す。
    closed_by_press: bool,
}

fn state_wid() -> WidgetId {
    WidgetId::ROOT.child(b"midi_bindings_list_state")
}

/// `learn` = 「Learn」ボタンの rect。ボタンを描いた直後に呼ぶ。
pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, learn: Rect) {
    let pointer = ui.pointer();
    let clicked = ui.take_secondary_click_in_rect(learn).is_some();
    let state = ui.widget_state::<ListState>(state_wid());
    let closed_by_press = state.closed_by_press;
    if pointer.secondary_just_released || pointer.primary_just_pressed || pointer.primary_just_released {
        state.closed_by_press = false;
    }
    if clicked && !closed_by_press && !ui.is_overlay_open(OVERLAY_ID) {
        ui.open_overlay(OVERLAY_ID);
    }
    if !ui.is_overlay_open(OVERLAY_ID) {
        return;
    }
    draw_open(app, ui, learn);
    if !ui.is_overlay_open(OVERLAY_ID) {
        ui.widget_state::<ListState>(state_wid()).closed_by_press = true;
        return;
    }
    if ui.take_shortcut("escape") {
        ui.close_overlay(OVERLAY_ID);
    }
}

/// 開いている一覧を描く (外クリックならここで閉じる — `popup_layer`)。
fn draw_open(app: &AppData, ui: &mut Ui<'_, AppData>, learn: Rect) {
    let rows: Vec<(String, common::model::MidiBinding)> = app
        .cur
        .song_doc
        .song()
        .midi_bindings
        .iter()
        .map(|b| (app.midi_binding_row_label(b), b.clone()))
        .collect();
    let label_w = rows.iter().fold(ui.measure_text(EMPTY_TEXT, FONT), |w, (l, _)| w.max(ui.measure_text(l, FONT)));
    let screen = ui.screen();
    #[allow(clippy::cast_precision_loss)]
    let (screen_w, screen_h) = (screen.width as f32, screen.height as f32);
    let w = (label_w + PAD * 3.0 + REMOVE_W).max(MIN_W).min(screen_w - 8.0);
    #[allow(clippy::cast_precision_loss)]
    let content_h = PAD * 2.0 + ROW_H * rows.len().max(1) as f32;
    let y = learn.y + learn.h + 2.0;
    let h = content_h.min(screen_h - y - 4.0).max(ROW_H + PAD * 2.0);
    let x = learn.x.min(screen_w - w - 4.0).max(0.0);
    let rect = Rect { x, y, w, h };
    ui.update_popup_anchor(("overlay", OVERLAY_ID), rect);
    ui.popup_layer(("overlay", OVERLAY_ID), |ui| {
        let p = &app.theme.core;
        ui.panel_with_border("midi_bindings_list_bg", rect, p.panel_raised, p.border, 1.0, 6.0);
        let scroll = ui.popup_scroll(rect, content_h);
        ui.with_clip_rect(rect, |ui| {
            let text_dy = (ROW_H - FONT) * 0.5 - 1.0;
            if rows.is_empty() {
                ui.label_at("midi_bindings_list_empty", EMPTY_TEXT, rect.x + PAD, rect.y + PAD + text_dy, FONT, p.text_dim);
            }
            for (i, (label, binding)) in rows.into_iter().enumerate() {
                #[allow(clippy::cast_precision_loss)]
                let ry = rect.y + PAD + ROW_H * i as f32 - scroll.offset;
                ui.label_at(("midi_binding_row", i), &label, rect.x + PAD, ry + text_dy, FONT, p.text);
                let remove = Rect { x: rect.x + scroll.content_w - PAD - REMOVE_W, y: ry + 1.0, w: REMOVE_W, h: ROW_H - 2.0 };
                ui.button_at(("midi_binding_remove", i), "\u{00d7}", remove, move || {
                    Edit::mutate(move |app: &mut AppData| app.handle_event(AppEvent::RemoveMidiBinding(binding)))
                });
            }
        });
    });
}
