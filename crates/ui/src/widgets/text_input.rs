//! `text_input` ウィジェット — 1 行テキスト編集 (ASCII + UTF-8 char 単位)。
//!
//! Phase 4b: ASCII / 単純な char 単位編集。IME 対応は Phase 4c。
//!
//! - クリックで focus 取得 (cursor は末尾に移動)
//! - Backspace: cursor 直前の文字を削除
//! - Arrow Left/Right: cursor を char 境界単位で移動
//! - その他のキー入力 (`KeyEvent::text`): cursor 位置に挿入 (制御文字は除外)
//! - Enter: 「commit された」と response で通知 (フォーカスは保ったまま、blur はアプリ側責務)

use std::hash::Hash;

use daw_ui_platform::{ElementState, PhysicalKey};
use daw_ui_renderer::{Color, GlyphArea, LineBatch, LineSegment, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::ui::Ui;

/// text_input の永続状態。
#[derive(Debug, Default)]
pub(crate) struct TextInputState {
    /// 直近の press がこの widget 内から始まったか (button と同じモデル)。
    press_started_inside: bool,
    /// cursor の byte 位置。char 境界に揃っていることを保証する。
    cursor_byte: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TextInputResponse {
    /// この widget が現在キーボードフォーカスを持っているか。
    pub focused: bool,
    /// このフレームで Enter キーが押されたか。
    pub committed: bool,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 矩形指定で 1 行 text_input を描画 + キー入力処理。
    /// 編集が起きたら `on_change(new_text)` を Edit 列に積む。
    pub fn text_input_at<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        text: &str,
        on_change: F,
    ) -> TextInputResponse
    where
        F: FnOnce(String) -> Edit<M>,
    {
        let wid = WidgetId::ROOT.child((b"text_input", &id));
        let pointer = self.pointer;
        let inside = pointer.pos.is_some_and(|(px, py)| rect.contains(px, py));

        // armed-state click 判定。
        let click = {
            let state: &mut TextInputState = self.widget_state(wid);
            if pointer.primary_just_pressed {
                state.press_started_inside = inside;
            }
            let click = pointer.primary_just_released && state.press_started_inside && inside;
            if pointer.primary_just_released {
                state.press_started_inside = false;
            }
            click
        };

        // Click inside → focus 取得 + cursor を末尾へ。
        if click {
            self.set_focus(wid);
            let state: &mut TextInputState = self.widget_state(wid);
            state.cursor_byte = text.len();
        }

        // Press が rect 外で発生 + 自分が focus を持っていたら自己 blur。
        // これで「外をクリック → 次の入力イベントを待たずに枠線が即座に消える」になる。
        if pointer.primary_just_pressed && !inside && self.is_focused(wid) {
            self.clear_focus_if_focused(wid);
        }

        let was_focused = self.is_focused(wid);

        // フォーカス中ならキー入力を処理。
        let mut new_text: Option<String> = None;
        let mut committed = false;
        let mut escape_pressed = false;
        if was_focused {
            let key_events = self.take_keyboard_events_if_focused(wid);
            if !key_events.is_empty() {
                let state: &mut TextInputState = self.widget_state(wid);
                let mut working = text.to_string();
                let mut cursor = state.cursor_byte.min(working.len());
                let mut changed = false;

                for ev in key_events {
                    if !matches!(ev.state, ElementState::Pressed) {
                        continue;
                    }
                    match ev.physical_key {
                        PhysicalKey::Backspace => {
                            if cursor > 0 {
                                // char 境界まで戻す。
                                let mut prev = cursor - 1;
                                while prev > 0 && !working.is_char_boundary(prev) {
                                    prev -= 1;
                                }
                                working.replace_range(prev..cursor, "");
                                cursor = prev;
                                changed = true;
                            }
                        }
                        PhysicalKey::ArrowLeft => {
                            if cursor > 0 {
                                cursor -= 1;
                                while cursor > 0 && !working.is_char_boundary(cursor) {
                                    cursor -= 1;
                                }
                            }
                        }
                        PhysicalKey::ArrowRight => {
                            if cursor < working.len() {
                                cursor += 1;
                                while cursor < working.len()
                                    && !working.is_char_boundary(cursor)
                                {
                                    cursor += 1;
                                }
                            }
                        }
                        PhysicalKey::Enter => {
                            committed = true;
                        }
                        PhysicalKey::Escape => {
                            // フォーカスを外す処理は state 借用を抜けてから行う。
                            escape_pressed = true;
                        }
                        _ => {
                            // 文字入力。`KeyEvent::text` を信用する (IME 経由含むが、
                            // Phase 4c 前なので preedit は来ない想定)。
                            if let Some(input_text) = &ev.text {
                                let filtered: String =
                                    input_text.chars().filter(|c| !c.is_control()).collect();
                                if !filtered.is_empty() {
                                    working.insert_str(cursor, &filtered);
                                    cursor += filtered.len();
                                    changed = true;
                                }
                            }
                        }
                    }
                }

                if changed {
                    new_text = Some(working.clone());
                }
                // working はキー処理で大きさが変わっている可能性があるので最新長でクランプ。
                state.cursor_byte = cursor.min(working.len());
            }
        }
        if escape_pressed {
            self.clear_focus_if_focused(wid);
        }

        // 描画用 cursor: state を改変せずに「入力 text の長さ」でクランプするだけ
        // (state に保存した cursor は editing 中の post-edit 位置なので上書きしない)。
        let cursor_byte_for_draw = {
            let state: &mut TextInputState = self.widget_state(wid);
            state.cursor_byte.min(text.len())
        };

        // 描画。
        draw_text_input(self, rect, text, was_focused, cursor_byte_for_draw);

        if let Some(t) = new_text {
            let edit = on_change(t);
            self.push_edit(edit);
        }

        TextInputResponse { focused: was_focused, committed }
    }

    /// vstack カーソル位置に 1 行 text_input を追加 (高さ 28px、幅は cursor 幅)。
    pub fn text_input<F>(
        &mut self,
        id: impl Hash,
        text: &str,
        on_change: F,
    ) -> TextInputResponse
    where
        F: FnOnce(String) -> Edit<M>,
    {
        let pad = 8.0;
        let h = 28.0;
        let rect = Rect {
            x: self.cursor.x + pad,
            y: self.cursor.y + self.next_y,
            w: self.cursor.w - pad * 2.0,
            h,
        };
        let resp = self.text_input_at(id, rect, text, on_change);
        self.next_y += h + pad;
        resp
    }
}

/// ASCII 等幅近似の文字幅 (px)。Phase 4c で glyphon の measure に置き換える予定。
const APPROX_CHAR_W: f32 = 8.0;

fn draw_text_input<M: ?Sized + 'static>(
    ui: &mut Ui<'_, M>,
    rect: Rect,
    text: &str,
    focused: bool,
    cursor_byte: usize,
) {
    // 背景。
    let bg_fill = Color::rgb(0.08, 0.09, 0.11);
    let border = if focused {
        Color::rgb(0.55, 0.78, 0.95)
    } else {
        Color::rgb(0.30, 0.33, 0.39)
    };
    ui.push_rect(RectCommand {
        rect,
        fill: bg_fill,
        border,
        border_width: 1.5,
        radius: [3.0; 4],
    });

    // テキスト。
    let pad_x = 8.0;
    let font_size = 14.0;
    let line_h = font_size * 1.2;
    let tx = rect.x + pad_x;
    let ty = rect.y + (rect.h - line_h) * 0.5;
    if !text.is_empty() {
        ui.push_text(GlyphArea {
            text: text.to_string(),
            left: tx,
            top: ty,
            font_size,
            line_height: line_h,
            color: Color::rgb(0.92, 0.92, 0.94),
        });
    }

    // カーソル (フォーカス中のみ)。ASCII 近似。
    if focused {
        // cursor_byte までの文字数を char 単位で数える (ASCII 近似だが日本語混在でも byte/char で
        // ある程度の位置感は出る)。
        let prefix = text.get(..cursor_byte).unwrap_or("");
        let chars_before = prefix.chars().count() as f32;
        let cursor_x = tx + chars_before * APPROX_CHAR_W;
        let cursor_y = rect.y + 4.0;
        let cursor_h = (rect.h - 8.0).max(1.0);
        ui.push_lines(LineBatch {
            segments: vec![LineSegment {
                a: [cursor_x, cursor_y],
                b: [cursor_x, cursor_y + cursor_h],
                color: Color::rgb(0.95, 0.97, 1.0),
            }],
            line_width_px: 1.5,
            clip_rect: Some(rect),
        });
    }
}
