//! r.md #113: 仮想鍵盤ウィンドウ (`docs/plan_virtual_keyboard.md`)。
//!
//! 編集履歴 window と同じ **true-floating** (`Ui::reserve_floating_region` /
//! `Ui::with_floating_region`): 開いたまま背後のアレンジをマウス操作できる。
//! サイズは固定 (鍵盤の幅で決まる)、 タイトルバードラッグで移動、 位置は app_config。
//!
//! ここは「見せる + 入力を集める」 だけで、 音の意味付け (PC キー → ピッチ、 MIDI 入力と
//! 同じ入口へ流す) は `handler/virtual_keyboard.rs`。 key grab が横取りした PC キーも
//! この window が毎フレーム取り出して handler へ渡す (窓が開いている間だけ宣言されるので、
//! 取り出す側も同じ場所)。

use daw_ui_core::{DragKind, Edit, ScrubableNumberFormat, ScrubableNumberStyle, Ui};
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::app::{AppData, AppEvent};
use crate::event_virtual_keyboard::VirtualKeyboardEvent as E;
use crate::virtual_keyboard::{
    DEFAULT_VELOCITY, MAX_BASE_PITCH, SPAN_SEMITONES, key_labels, pitch_name,
};
use crate::widgets::piano_roll::is_black_key;

/// メニューバー高 (root::MENU_H のミラー)。 縦 clamp の基準。
const MENU_H: f32 = 24.0;
const TITLE_H: f32 = 24.0;
const CLOSE_W: f32 = 26.0;
const PAD: f32 = 10.0;
/// 上段 (Oct / Vel / 宛先) の行。
const ROW_Y: f32 = TITLE_H + 8.0;
const ROW_H: f32 = 22.0;
const FONT: f32 = 12.0;
/// 鍵盤の上端 (window 相対)。
const KEYS_Y: f32 = ROW_Y + ROW_H + 8.0;
const WHITE_W: f32 = 30.0;
const WHITE_H: f32 = 100.0;
const BLACK_W: f32 = 18.0;
const BLACK_H: f32 = 60.0;
/// 白鍵の本数 (C..E を 2 オクターブ + 4 度 = 17)。
const WHITE_COUNT: usize = 17;
const KEYBOARD_W: f32 = WHITE_W * WHITE_COUNT as f32;
const WINDOW_W: f32 = KEYBOARD_W + PAD * 2.0;
const WINDOW_H: f32 = KEYS_Y + WHITE_H + PAD;

/// 初回 (未配置) の既定位置: 画面中央下 (ステータスバーの上)。
fn default_pos(screen: Rect) -> (f32, f32) {
    (
        screen.x + (screen.w - WINDOW_W) * 0.5,
        screen.y + screen.h - WINDOW_H - 60.0,
    )
}

/// 保存位置をタイトルバーが画面内に残る範囲に clamp (モニタ変更でも復帰できる)。
fn clamp_to_screen(x: f32, y: f32, screen: Rect) -> Rect {
    let x = x.clamp(screen.x + 60.0 - WINDOW_W, screen.x + screen.w - 60.0);
    let y = y.clamp(screen.y + MENU_H, (screen.y + screen.h - TITLE_H).max(screen.y + MENU_H));
    Rect { x, y, w: WINDOW_W, h: WINDOW_H }
}

/// 現在の committed window rect。 reserve / draw の両方が同じ基準として使う。
fn window_rect(app: &AppData, screen: Rect) -> Rect {
    let (x, y) = app
        .ui_prefs
        .virtual_keyboard_rect
        .map_or_else(|| default_pos(screen), |r| (r.x, r.y));
    clamp_to_screen(x, y, screen)
}

/// build_root の **背景 widget 描画より前** に呼ぶ (pointer の占有予約)。
pub fn reserve(app: &AppData, ui: &mut Ui<'_, AppData>, screen: Rect) {
    if !app.virtual_keyboard.open {
        return;
    }
    ui.reserve_floating_region(window_rect(app, screen));
}

/// build_root の **末尾近く** (背景描画の後 = z-order 最前面) に呼ぶ。
pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, screen: Rect) {
    if !app.virtual_keyboard.open {
        return;
    }
    // key grab が横取りした PC キー。 窓が開いているフレームだけ宣言されているので、
    // 取り出す側もここ 1 か所 (押した順のまま handler へ)。
    let grabbed = ui.take_grabbed_keys();
    if !grabbed.is_empty() {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            for key in grabbed {
                app.handle_event(AppEvent::VirtualKeyboard(E::Key(key)));
            }
        }));
    }
    ui.with_floating_region(|ui| draw_window(app, ui, screen));
}

fn draw_window(app: &AppData, ui: &mut Ui<'_, AppData>, screen: Rect) {
    let committed = window_rect(app, screen);
    let mut rect = committed;
    let mut commit = false;

    // ---- タイトルバードラッグ = 移動 (✕ ボタン領域は除外) ----
    let title_drag = Rect {
        x: committed.x,
        y: committed.y,
        w: (committed.w - CLOSE_W).max(0.0),
        h: TITLE_H,
    };
    if let Some(d) = ui.take_drag_in_rect("vkbd_move", title_drag) {
        rect = clamp_to_screen(committed.x + d.delta.0, committed.y + d.delta.1, screen);
        commit |= matches!(d.kind, DragKind::Released);
    }

    draw_chrome(app, ui, rect);
    draw_controls(app, ui, rect);
    draw_keys(app, ui, rect);

    if commit {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::VirtualKeyboard(E::SetRect(rect)));
        }));
    }
}

fn draw_chrome(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect) {
    let p = &app.theme.core;
    ui.push_rect(RectCommand {
        rect,
        fill: p.panel,
        border: p.border,
        border_width: 1.0,
        radius: [6.0; 4],
        clip_rect: None,
    });
    ui.panel("vkbd_titlebar", Rect { x: rect.x, y: rect.y, w: rect.w, h: TITLE_H }, p.header, 6.0);
    ui.label_at("vkbd_title", "仮想鍵盤", rect.x + 12.0, rect.y + 7.0, 13.0, p.text);
    ui.button_at(
        "vkbd_close",
        "\u{2715}",
        Rect { x: rect.x + rect.w - CLOSE_W, y: rect.y + 4.0, w: 20.0, h: 18.0 },
        || Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::VirtualKeyboard(E::Toggle))),
    );
}

/// 上段: `Oct [−] C3 [+]   Vel [100]   → R: <宛先>`。
fn draw_controls(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect) {
    let p = &app.theme.core;
    let y = rect.y + ROW_Y;
    let ty = y + (ROW_H - FONT) * 0.5;
    let mut x = rect.x + PAD;

    // ---- オクターブ ----
    ui.label_at("vkbd_oct_label", "Oct", x, ty, FONT, p.text_dim);
    x += 28.0;
    let base = app.ui_prefs.virtual_keyboard_base_pitch;
    if base > 0 {
        ui.button_at("vkbd_oct_down", "\u{2212}", Rect { x, y, w: ROW_H, h: ROW_H }, || {
            Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::VirtualKeyboard(E::ShiftOctave(-1)));
            })
        });
    }
    x += ROW_H + 4.0;
    let name = pitch_name(base);
    let name_w = 30.0;
    let tw = ui.measure_text(&name, FONT);
    ui.label_at("vkbd_oct_value", &name, x + (name_w - tw) * 0.5, ty, FONT, p.text);
    x += name_w + 4.0;
    if base < MAX_BASE_PITCH {
        ui.button_at("vkbd_oct_up", "+", Rect { x, y, w: ROW_H, h: ROW_H }, || {
            Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::VirtualKeyboard(E::ShiftOctave(1)));
            })
        });
    }
    x += ROW_H + PAD * 2.0;

    // ---- ベロシティ (Sampler の「長さ」 欄と同じ scrubable_number idiom) ----
    ui.label_at("vkbd_vel_label", "Vel", x, ty, FONT, p.text_dim);
    x += 26.0;
    let field = Rect { x, y, w: 48.0, h: ROW_H };
    let pending = std::cell::Cell::new(None::<f64>);
    let resp = ui.scrubable_number_at(
        "vkbd_velocity",
        field,
        f64::from(app.ui_prefs.virtual_keyboard_velocity),
        f64::from(DEFAULT_VELOCITY),
        ScrubableNumberFormat::Integer,
        &ScrubableNumberStyle {
            font_size: FONT,
            sensitivity: 0.5,
            range: Some((1.0, 127.0)),
            ..ScrubableNumberStyle::from_palette(p)
        },
        |v| {
            pending.set(Some(v));
            Edit::mutate(|_: &mut AppData| {})
        },
        None,
        None,
    );
    let active = resp.dragging || resp.editing_text;
    if let Some(v) = pending.get() {
        let velocity = velocity_from(v);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::VirtualKeyboard(E::SetVelocity { velocity, commit: !active }));
        }));
    }
    // ドラッグ / 入力の立ち下がりで保存。
    if active != app.virtual_keyboard.velocity_editing {
        let velocity = velocity_from(resp.displayed_value);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.virtual_keyboard.velocity_editing = active;
            if !active {
                app.handle_event(AppEvent::VirtualKeyboard(E::SetVelocity { velocity, commit: true }));
            }
        }));
    }
    x += field.w + PAD * 2.0;

    // ---- 宛先 (カーソルトラック = 選択中のトラック) ----
    let target = app
        .virtual_keyboard_target_track()
        .and_then(|id| app.song_doc.song().track_by_id(id));
    let (text, color) = match target {
        Some(t) => (format!("\u{2192} {}", t.name), p.text_dim),
        None => ("トラックを選択してください".to_string(), p.text_error),
    };
    let max_w = (rect.x + rect.w - PAD - x).max(0.0);
    ui.label_at_clipped("vkbd_dest", &text, Rect { x, y: ty, w: max_w, h: FONT + 4.0 }, FONT, color);
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn velocity_from(v: f64) -> u8 {
    v.round().clamp(1.0, 127.0) as u8
}

/// 半音 `s` (下段 `Z` = 0) の白鍵 index (0..WHITE_COUNT)。 黒鍵は「直前の白鍵」 を返す。
fn white_index(s: u8) -> usize {
    const WHITE_BEFORE: [usize; 12] = [0, 0, 1, 1, 2, 3, 3, 4, 4, 5, 5, 6];
    usize::from(s / 12) * 7 + WHITE_BEFORE[usize::from(s % 12)]
}

/// 白鍵 index → 半音 (`SPAN_SEMITONES` を超えるなら `None`)。
fn semitone_of_white(idx: usize) -> Option<u8> {
    const WHITE_PC: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];
    let s = u8::try_from(idx / 7).ok()? * 12 + WHITE_PC[idx % 7];
    (s < SPAN_SEMITONES).then_some(s)
}

/// 鍵盤領域 `kb` 内での半音 `s` の鍵の rect。
fn key_rect(kb: Rect, s: u8) -> Rect {
    if is_black_key(s) {
        // 黒鍵は「直前の白鍵の右端」 に中心を置く。
        let boundary = kb.x + (white_index(s) + 1) as f32 * WHITE_W;
        Rect { x: boundary - BLACK_W * 0.5, y: kb.y, w: BLACK_W, h: BLACK_H }
    } else {
        Rect { x: kb.x + white_index(s) as f32 * WHITE_W, y: kb.y, w: WHITE_W, h: WHITE_H }
    }
}

/// 鍵盤領域 `kb` の座標 → 半音 (黒鍵が白鍵より手前)。
fn hit_test(kb: Rect, px: f32, py: f32) -> Option<u8> {
    if !kb.contains(px, py) {
        return None;
    }
    if let Some(s) = (0..SPAN_SEMITONES)
        .filter(|s| is_black_key(*s))
        .find(|s| key_rect(kb, *s).contains(px, py))
    {
        return Some(s);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let idx = ((px - kb.x) / WHITE_W).floor().max(0.0) as usize;
    semitone_of_white(idx)
}

fn draw_keys(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect) {
    let p = &app.theme.core;
    let d = &app.theme.daw;
    let kb = Rect { x: rect.x + PAD, y: rect.y + KEYS_Y, w: KEYBOARD_W, h: WHITE_H };
    let base = app.ui_prefs.virtual_keyboard_base_pitch;

    // ---- マウスで弾く (ピアノロール左の鍵盤と同じ held-value + 差分) ----
    let pointer = ui.pointer();
    let hit = pointer.pos.and_then(|(px, py)| hit_test(kb, px, py));
    let current = app.virtual_keyboard.mouse_pitch;
    let next = if !pointer.primary_pressed {
        None
    } else if current.is_some() {
        // 押したまま滑らせる (glissando)。 鍵盤の外へ出たら最後の鍵を保つ。
        hit.map_or(current, |s| Some(base.saturating_add(s).min(127)))
    } else if pointer.primary_just_pressed {
        hit.map(|s| base.saturating_add(s).min(127))
    } else {
        None
    };
    if next != current {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::VirtualKeyboard(E::MousePitch(next)));
        }));
    }

    // 押している鍵 (PC キー + マウス) を半音に戻す。
    let pressed_semitone = |pitch: u8| -> Option<u8> {
        let s = pitch.checked_sub(base)?;
        (s < SPAN_SEMITONES).then_some(s)
    };
    let mut pressed = [false; SPAN_SEMITONES as usize];
    for (_, pitch) in &app.virtual_keyboard.held {
        if let Some(s) = pressed_semitone(*pitch) {
            pressed[usize::from(s)] = true;
        }
    }
    if let Some(s) = next.and_then(pressed_semitone) {
        pressed[usize::from(s)] = true;
    }

    // ---- 白鍵 → 黒鍵の順に描く (黒鍵が手前) ----
    for black in [false, true] {
        for s in (0..SPAN_SEMITONES).filter(|s| is_black_key(*s) == black) {
            let r = key_rect(kb, s);
            let fill = if pressed[usize::from(s)] {
                p.accent
            } else if black {
                d.key_black
            } else {
                d.key_white
            };
            ui.push_rect(RectCommand {
                rect: r,
                fill,
                border: p.border,
                border_width: 1.0,
                radius: [0.0, 0.0, 3.0, 3.0],
                clip_rect: None,
            });
            draw_key_labels(ui, p, s, r, fill);
        }
    }
}

/// 鍵に PC キー名を印字 (下段を下、 両段にある鍵は上段をその上に)。 色は鍵の塗りから
/// 決める (白鍵 / 黒鍵 / 押下中の accent のどれでも読める)。
fn draw_key_labels(ui: &mut Ui<'_, AppData>, p: &daw_ui_core::Palette, s: u8, r: Rect, fill: Color) {
    let ink = p.ink_for(fill);
    let (lower, upper) = key_labels(s);
    let font = 11.0;
    let mut y = r.y + r.h - font - 5.0;
    for (i, label) in [lower, upper].into_iter().enumerate() {
        let Some(label) = label else { continue };
        let tw = ui.measure_text(label, font);
        ui.label_at(("vkbd_key", s, i), label, r.x + (r.w - tw) * 0.5, y, font, ink);
        y -= font + 3.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 白鍵 index ↔ 半音の対応が両方向で一致し、 黒鍵が隣り合う白鍵の境界に乗る。
    #[test]
    fn 鍵の幾何が半音と一対一に対応する() {
        for idx in 0..WHITE_COUNT {
            let s = semitone_of_white(idx).expect("17 本の白鍵は全部 span 内");
            assert_eq!(white_index(s), idx);
            assert!(!is_black_key(s));
        }
        assert_eq!(semitone_of_white(WHITE_COUNT), None);
        let kb = Rect { x: 0.0, y: 0.0, w: KEYBOARD_W, h: WHITE_H };
        // C# は C と D の境界 (x = 30) に中心。
        let cs = key_rect(kb, 1);
        assert!((cs.x + cs.w * 0.5 - WHITE_W).abs() < 1e-3);
        // 上段 P (= 28, E) が右端の白鍵。
        let e = key_rect(kb, 28);
        assert!((e.x + e.w - KEYBOARD_W).abs() < 1e-3);
    }

    /// 黒鍵の上では黒鍵、 その下 (白鍵だけの高さ) では白鍵、 鍵盤の外では None。
    #[test]
    fn hit_test_は黒鍵を優先し外側は_none() {
        let kb = Rect { x: 100.0, y: 50.0, w: KEYBOARD_W, h: WHITE_H };
        // C と D の境界 x=130、 黒鍵の高さ内。
        assert_eq!(hit_test(kb, 130.0, 60.0), Some(1));
        // 同じ x でも黒鍵より下は白鍵 (境界の右 = D)。
        assert_eq!(hit_test(kb, 130.0, 140.0), Some(2));
        assert_eq!(hit_test(kb, 100.0 + 15.0, 140.0), Some(0));
        assert_eq!(hit_test(kb, 99.0, 60.0), None);
        assert_eq!(hit_test(kb, 100.0 + KEYBOARD_W - 1.0, 140.0), Some(28));
    }
}
