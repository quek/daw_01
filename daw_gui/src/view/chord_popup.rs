//! コードのポップアップ (r.md #157 Q4、Studio One の Chord Selector 型)。箱の直下に出る。
//!
//! - 入力欄 (開いた時点で focus、全選択): `Am7` + Enter で確定して閉じる。読めない間は確定しない。
//! - 五度圏の輪 (ルート、同じ位置の再クリックで異名同音) / 種類 / テンション / ベースのボタン。
//! - 次のコードの候補 (`AppData::chord_popup_candidates`)、↑↓ で巡回。
//! - ←→ で隣の箱 (入力欄を打ち換えている間は caret 移動に譲る)、Tab で次の箱を追加、外クリック / Esc で閉じる。
//!
//! 変更は即反映して試聴を鳴らす (`handler::chord_popup`)。`open_overlay` なので開いている間も
//! 再生と shortcut は背景に通る (Space 等)、背景への click は通さない。状態は
//! `ProjectEphemeral::chord_band.popup`、Inspector には編集面を作らない (Q4)。

use std::f32::consts::PI;

use common::chord_name::{kind_suffix, note_name, parse_chord};
use common::model::{Chord, ChordEvent, ChordKind, Spelling, Tensions};
use daw_ui_core::widgets::toggle_button::ToggleButtonStyle;
use daw_ui_core::{Edit, Ui, WidgetId};
use daw_ui_platform::PhysicalKey;
use daw_ui_renderer::Rect;

use crate::app::AppData;

const OVERLAY_ID: &str = "chord_popup";
/// 幅は種類の一番長いラベル (`augMaj7`) が 6 列で省略されない幅。
const W: f32 = 500.0;
const H: f32 = 268.0;
const PAD: f32 = 8.0;
const ROW_H: f32 = 22.0;
const WHEEL: f32 = 164.0;
const FONT: f32 = 11.0;

/// `arrangement_view::draw` が arrangement widget の後に呼ぶ (帯の位置は widget の mirror を読む)。
pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>) {
    let popup = app.cur.peph.chord_band.popup.as_ref();
    let lane = app.cur.peph.chord_band.lane;
    let ev = popup.and_then(|p| app.cur.song_doc.song().chord_by_id(p.chord_id));
    // 箱が消えた (undo / 削除) / 帯を隠した → 閉じる。
    let (Some(popup), Some(ev)) = (popup, ev.filter(|_| lane.rect.h > 0.0)) else {
        if ui.is_overlay_open(OVERLAY_ID) {
            ui.close_overlay(OVERLAY_ID);
        }
        if popup.is_some() {
            ui.push_edit(Edit::mutate(|app: &mut AppData| app.close_chord_popup()));
        }
        return;
    };
    // 箱の左端の直下。窓の右端からはみ出すなら左へ寄せる (帯が狭いと帯の幅には収まらない)。
    let screen_w = ui.screen().width as f32;
    let x = lane.x_of(ev.start_beat).max(lane.rect.x).min(screen_w - W - 4.0).max(0.0);
    let rect = Rect { x, y: lane.rect.y + lane.rect.h + 2.0, w: W, h: H };
    if !ui.is_overlay_open(OVERLAY_ID) {
        ui.open_overlay(OVERLAY_ID);
    }
    ui.update_popup_anchor(("overlay", OVERLAY_ID), rect);
    let (id, rev, typed, text, candidate) = (popup.chord_id, popup.rev, popup.typed, popup.text.clone(), popup.candidate);
    let ev = ev.clone();
    ui.popup_layer(("overlay", OVERLAY_ID), |ui| {
        let p = &app.theme.core;
        ui.panel_with_border("chord_popup_bg", rect, p.panel_raised, p.border, 1.0, 6.0);
        text_row(app, ui, rect, (id, rev, typed), &text);
        let body_y = rect.y + PAD + ROW_H + PAD;
        wheel(app, ui, Rect { x: rect.x + PAD, y: body_y, w: WHEEL, h: WHEEL }, &ev);
        let right = Rect { x: rect.x + PAD * 2.0 + WHEEL, y: body_y, w: W - PAD * 3.0 - WHEEL, h: WHEEL };
        let y = kinds(ui, right, &ev.chord, p);
        let y = tensions(ui, Rect { y, ..right }, &ev.chord, p);
        bass(ui, Rect { y, ..right }, &ev, p);
        let cand_y = body_y + WHEEL + PAD;
        candidates(app, ui, Rect { x: rect.x + PAD, y: cand_y, w: W - PAD * 2.0, h: ROW_H }, candidate);
        ui.label_at("chord_popup_hint", "←→ 隣のコード  Tab 次を追加  ↑↓ 候補  Enter 確定  Esc 閉じる", rect.x + PAD, cand_y + ROW_H + 6.0, 10.0, p.text_dim);
    });
    // 外クリック / Esc で閉じた → 状態も閉じる。
    if !ui.is_overlay_open(OVERLAY_ID) {
        ui.push_edit(Edit::mutate(|app: &mut AppData| app.close_chord_popup()));
    }
}

/// 入力欄の行。欄より先に ←→ / Tab の押下を取り出す (打ち換えている間の ←→ は caret 移動に譲る)。
fn text_row(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect, (id, rev, typed): (u32, u32, bool), text: &str) {
    let p = &app.theme.core;
    let input_id = ("chord_popup_text", id, rev);
    let wid = WidgetId::ROOT.child((b"text_input", &input_id));
    let keys: &[PhysicalKey] =
        if typed { &[PhysicalKey::Tab] } else { &[PhysicalKey::ArrowLeft, PhysicalKey::ArrowRight, PhysicalKey::Tab] };
    for k in ui.take_focused_key_presses(wid, keys) {
        let key = k.physical_key;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| match key {
            PhysicalKey::ArrowLeft => app.chord_popup_step(-1),
            PhysicalKey::ArrowRight => app.chord_popup_step(1),
            _ => app.chord_popup_tab(),
        }));
    }
    let input = Rect { x: rect.x + PAD, y: rect.y + PAD, w: 180.0, h: ROW_H };
    let resp = ui.text_input_at_focused(input_id, input, text, &ui.text_input_style(), |new| {
        Edit::mutate(move |app: &mut AppData| app.chord_popup_typed(new))
    });
    if resp.committed {
        ui.push_edit(Edit::mutate(|app: &mut AppData| app.chord_popup_enter()));
    }
    let nav = i32::from(resp.nav_down) - i32::from(resp.nav_up);
    if nav != 0 {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| app.chord_popup_cycle(nav)));
    }
    // 読めるかどうか (読めない間は Enter で確定しない)。
    let (status, color) = if !typed {
        ("", p.text_dim)
    } else if parse_chord(text).is_some() {
        ("Enter で確定", p.text_dim)
    } else {
        ("コード名として読めません", p.text_error)
    };
    ui.label_at("chord_popup_status", status, input.x + input.w + PAD, input.y + 5.0, FONT, color);
}

/// 五度圏の輪 (12 のルート)。同じ位置の再クリックで綴りを `#` ⇄ `b` に切り替える。
fn wheel(app: &AppData, ui: &mut Ui<'_, AppData>, r: Rect, ev: &ChordEvent) {
    let p = &app.theme.core;
    let key = app.cur.song_doc.song().scale_at(ev.start_beat);
    let style = toggle_style(p);
    let (cx, cy, radius) = (r.x + r.w * 0.5, r.y + r.h * 0.5, r.w * 0.5 - 16.0);
    let chord = ev.chord;
    for i in 0..12u8 {
        let pc = (i * 7) % 12;
        let angle = -PI * 0.5 + f32::from(i) * PI / 6.0;
        let b = Rect { x: cx + radius * angle.cos() - 16.0, y: cy + radius * angle.sin() - 10.0, w: 32.0, h: 20.0 };
        let current = pc == chord.root;
        let spelling = if current { chord.spelling } else { Spelling::Auto };
        let label = note_name(pc, spelling, key);
        let _ = ui.toggle_button_at(("chord_popup_root", pc), label, b, current, &style, move |_| {
            Edit::mutate(move |app: &mut AppData| {
                let next = if current { respelled(chord, label) } else { Chord { root: pc, spelling: Spelling::Auto, ..chord } };
                app.chord_popup_set(next);
            })
        });
    }
    // 輪の中央に今のコード名。
    let name = app.chord_label(&chord, ev.start_beat);
    let w = ui.measure_text(&name, 16.0);
    ui.label_at("chord_popup_name", &name, cx - w * 0.5, cy - 9.0, 16.0, p.text);
}

/// 今の綴りの反対 (`C#` ⇄ `Db`)。幹音 (`C` / `D` …) は切り替えない。
fn respelled(chord: Chord, shown: &str) -> Chord {
    let spelling = if shown.contains('#') {
        Spelling::Flat
    } else if shown.contains('b') {
        Spelling::Sharp
    } else {
        chord.spelling
    };
    Chord { spelling, ..chord }
}

/// 種類のボタン (6 列)。返り値は次の行の y。
fn kinds(ui: &mut Ui<'_, AppData>, r: Rect, chord: &Chord, p: &crate::theme::Palette) -> f32 {
    const COLS: usize = 6;
    let style = toggle_style(p);
    let bw = r.w / COLS as f32;
    let chord = *chord;
    for (i, &kind) in ChordKind::ALL.iter().enumerate() {
        let b = Rect { x: r.x + bw * (i % COLS) as f32, y: r.y + ROW_H * (i / COLS) as f32, w: bw - 2.0, h: ROW_H - 2.0 };
        let label = if kind == ChordKind::Major { "maj" } else { kind_suffix(kind) };
        let _ = ui.toggle_button_at(("chord_popup_kind", i), label, b, chord.kind == kind, &style, move |_| {
            Edit::mutate(move |app: &mut AppData| app.chord_popup_set(Chord { kind, ..chord }))
        });
    }
    r.y + ROW_H * ChordKind::ALL.len().div_ceil(COLS) as f32 + 4.0
}

/// テンションのトグル (1 行)。`b5` / `#5` は 5 度を置き換えるので片方だけ。
fn tensions(ui: &mut Ui<'_, AppData>, r: Rect, chord: &Chord, p: &crate::theme::Palette) -> f32 {
    let style = toggle_style(p);
    let bw = r.w / Tensions::ALL.len() as f32;
    let chord = *chord;
    for (i, &(t, label, _)) in Tensions::ALL.iter().enumerate() {
        let b = Rect { x: r.x + bw * i as f32, y: r.y, w: bw - 2.0, h: ROW_H - 2.0 };
        let on = chord.tensions.contains(t);
        let _ = ui.toggle_button_at(("chord_popup_tension", i), label, b, on, &style, move |_| {
            Edit::mutate(move |app: &mut AppData| {
                let mut ts = chord.tensions.toggled(t);
                if !on && t == Tensions::FLAT5 {
                    ts = ts.without(Tensions::SHARP5);
                } else if !on && t == Tensions::SHARP5 {
                    ts = ts.without(Tensions::FLAT5);
                }
                app.chord_popup_set(Chord { tensions: ts, ..chord });
            })
        });
    }
    r.y + ROW_H + 4.0
}

/// ベースのボタン (なし + 12 音、2 行)。
fn bass(ui: &mut Ui<'_, AppData>, r: Rect, ev: &ChordEvent, p: &crate::theme::Palette) {
    const COLS: usize = 7;
    let style = toggle_style(p);
    let bw = r.w / COLS as f32;
    let chord = ev.chord;
    let opts: Vec<Option<u8>> = std::iter::once(None).chain((0..12u8).map(Some)).collect();
    for (i, bass) in opts.into_iter().enumerate() {
        let b = Rect { x: r.x + bw * (i % COLS) as f32, y: r.y + ROW_H * (i / COLS) as f32, w: bw - 2.0, h: ROW_H - 2.0 };
        let label = bass.map_or("/ なし", |pc| note_name(pc, chord.spelling, None));
        let on = chord.bass == bass;
        let _ = ui.toggle_button_at(("chord_popup_bass", i), label, b, on, &style, move |_| {
            Edit::mutate(move |app: &mut AppData| app.chord_popup_set(Chord { bass, ..chord }))
        });
    }
}

/// 次のコードの候補 (クリックでその候補を入れる、↑↓ の巡回と同じ並び)。
fn candidates(app: &AppData, ui: &mut Ui<'_, AppData>, r: Rect, selected: Option<usize>) {
    let p = &app.theme.core;
    let style = toggle_style(p);
    ui.label_at("chord_popup_cand_label", "候補", r.x, r.y + 5.0, FONT, p.text_dim);
    let start_beat = app.chord_popup_id().and_then(|id| app.cur.song_doc.song().chord_by_id(id)).map_or(0.0, |c| c.start_beat);
    let list = app.chord_popup_candidates();
    let x0 = r.x + 32.0;
    let bw = ((r.w - 32.0) / common::chord_suggest::MAX_SUGGESTIONS as f32).max(1.0);
    for (i, c) in list.into_iter().enumerate() {
        let b = Rect { x: x0 + bw * i as f32, y: r.y, w: bw - 2.0, h: ROW_H - 2.0 };
        let label = app.chord_label(&c, start_beat);
        let _ = ui.toggle_button_at(("chord_popup_cand", i), &label, b, selected == Some(i), &style, move |_| {
            Edit::mutate(move |app: &mut AppData| {
                app.chord_popup_set(c);
                if let Some(p) = app.cur.peph.chord_band.popup.as_mut() {
                    p.candidate = Some(i);
                }
            })
        });
    }
}

fn toggle_style(p: &crate::theme::Palette) -> ToggleButtonStyle {
    ToggleButtonStyle { font_size: FONT, radius: 3.0, ..ToggleButtonStyle::from_palette(p) }
}
