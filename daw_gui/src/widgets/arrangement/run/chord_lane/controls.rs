//! 帯の見出し側のボタン (Q3 / Q7 / 計画書 §3.1)。
//!
//! - コードの帯の見出し (`Chords` の右): 録音待機 `●` / 追従の全体スイッチ `追従` / 設定 `▾`。
//!   見出しは 1 行 (帯の高さ) しか無いので、表記・試聴トラック・「再生中も鳴らす」・voicing は `▾` の
//!   メニュー (選んでいるものに ✓) にまとめる。
//! - Arranger 帯の見出しの右端: 帯の表示 / 非表示のトグル `コード` (帯を隠しても戻せる場所)。
//!
//! Arranger 帯の見出しは `render::dispatch` が塗るので、ここは `run.rs` がその **後** に呼ぶ。
//! 状態の持ち主: 追従のスイッチは Song (`ChordFollowEvent::SetEnabled`)、表示 / 表記は
//! `ProjectView::chord_band`、試聴 / voicing / 録音待機は c157-tools の `chord_tools` (`ChordToolEvent`)。

use common::chord_name::ChordNotation;
use common::chord_voicing::Voicing;
use daw_ui_core::widgets::toggle_button::ToggleButtonStyle;
use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent};
use crate::event_chord::{ChordFollowEvent, ChordToolEvent};
use crate::widgets::arrangement::frame::ArrangementFrame;

const BTN_H: f32 = 16.0;
const FONT: f32 = 10.0;

pub(in crate::widgets::arrangement) fn draw(ui: &mut Ui<'_, AppData>, app: &AppData, f: &ArrangementFrame<'_>) {
    visibility_toggle(ui, app, f);
    if f.chord_header_rect.h > 0.0 && f.chord_header_rect.w > 0.0 {
        band_buttons(ui, app, f.chord_header_rect);
    }
}

fn style(app: &AppData) -> ToggleButtonStyle {
    ToggleButtonStyle { font_size: FONT, radius: 3.0, ..ToggleButtonStyle::from_palette(&app.theme.core) }
}

/// `r` の右端から幅 `w` のボタン矩形を切り出し、残りの右端を返す。
fn take_right(right: &mut f32, r: Rect, w: f32) -> Rect {
    *right -= w + 2.0;
    Rect { x: *right, y: r.y + (r.h - BTN_H) * 0.5, w, h: BTN_H }
}

/// Arranger 帯の見出しの右端: コードの帯の表示 / 非表示。
fn visibility_toggle(ui: &mut Ui<'_, AppData>, app: &AppData, f: &ArrangementFrame<'_>) {
    let r = f.arranger_header_rect;
    if r.h < BTN_H || r.w < 80.0 {
        return;
    }
    let mut right = r.x + r.w - 2.0;
    let b = take_right(&mut right, r, 38.0);
    let visible = app.cur.view.chord_band.visible;
    let _ = ui.toggle_button_at("chord_band_visible", "コード", b, visible, &style(app), |on| {
        Edit::mutate(move |app: &mut AppData| {
            app.cur.view.chord_band.visible = on;
            if !on {
                app.close_chord_popup();
            }
        })
    });
}

/// コードの帯の見出し: `●` 録音待機 / `追従` / `▾` 設定メニュー。
fn band_buttons(ui: &mut Ui<'_, AppData>, app: &AppData, r: Rect) {
    let st = style(app);
    let mut right = r.x + r.w - 2.0;
    let menu_b = take_right(&mut right, r, 18.0);
    let follow_b = take_right(&mut right, r, 30.0);
    let rec_b = take_right(&mut right, r, 18.0);
    if rec_b.x < r.x + 44.0 {
        // 見出しが狭すぎる (ラベルと重なる) ときは出さない。
        return;
    }
    let armed = app.cur.chord_tools.record_armed;
    let _ = ui.toggle_button_at("chord_band_rec", "●", rec_b, armed, &st, |on| {
        Edit::mutate(move |app: &mut AppData| app.handle_event(AppEvent::ChordTool(ChordToolEvent::SetRecordArmed(on))))
    });
    let enabled = app.cur.song_doc.song().chord_follow_enabled;
    let _ = ui.toggle_button_at("chord_band_follow", "追従", follow_b, enabled, &st, |on| {
        Edit::mutate(move |app: &mut AppData| app.handle_event(AppEvent::ChordFollow(ChordFollowEvent::SetEnabled(on))))
    });
    let open_at = ui.button_at_clicked_sized("chord_band_menu_btn", "▾", menu_b, FONT).then_some((menu_b.x, menu_b.y + menu_b.h));
    settings_menu(ui, app, open_at);
}

/// 設定メニューの 1 項目が何をするか。
#[derive(Clone, Copy)]
enum Item {
    Notation(ChordNotation),
    Audition(Option<u32>),
    WhilePlaying(bool),
    Voicing(Voicing),
}

fn settings_menu(ui: &mut Ui<'_, AppData>, app: &AppData, open_at: Option<(f32, f32)>) {
    let check = |on: bool| if on { "✓ " } else { "　 " };
    let mut items: Vec<(String, Item)> = Vec::new();
    let notation = app.chord_notation();
    for &n in ChordNotation::ALL {
        items.push((format!("{}表記: {}", check(n == notation), n.label()), Item::Notation(n)));
    }
    let tools = &app.cur.chord_tools.view;
    items.push((format!("{}試聴: なし", check(tools.audition_track.is_none())), Item::Audition(None)));
    for (i, t) in app.cur.song_doc.song().tracks.iter().enumerate() {
        let on = tools.audition_track == Some(t.id);
        items.push((format!("{}試聴: {}", check(on), t.display_name(i)), Item::Audition(Some(t.id))));
    }
    let wp = tools.audition_while_playing;
    items.push((format!("{}再生中も鳴らす", check(wp)), Item::WhilePlaying(!wp)));
    for &(v, label) in Voicing::ALL {
        items.push((format!("{}voicing: {label}", check(v == tools.voicing)), Item::Voicing(v)));
    }
    let labels: Vec<&str> = items.iter().map(|(s, _)| s.as_str()).collect();
    let actions: Vec<Item> = items.iter().map(|(_, a)| *a).collect();
    ui.context_menu_at("chord_band_settings", open_at, &labels, move |idx, ui| {
        let Some(&item) = actions.get(idx) else { return };
        ui.push_edit(Edit::mutate(move |app: &mut AppData| match item {
            Item::Notation(n) => app.cur.view.chord_band.notation = n,
            Item::Audition(t) => app.handle_event(AppEvent::ChordTool(ChordToolEvent::SetAuditionTrack(t))),
            Item::WhilePlaying(on) => app.handle_event(AppEvent::ChordTool(ChordToolEvent::SetAuditionWhilePlaying(on))),
            Item::Voicing(v) => app.handle_event(AppEvent::ChordTool(ChordToolEvent::SetVoicing(v))),
        }));
    });
}
