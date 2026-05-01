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
use crate::input::ImeEvent;
use crate::scenegraph::hash_inputs;
use crate::ui::Ui;

/// text_input の永続状態。
#[derive(Debug, Default)]
pub(crate) struct TextInputState {
    /// 直近の press がこの widget 内から始まったか (button と同じモデル)。
    press_started_inside: bool,
    /// cursor の byte 位置。char 境界に揃っていることを保証する。
    cursor_byte: usize,
    /// IME 変換中の preedit テキスト。空文字列なら preedit 中ではない。
    /// model.text には反映されず、描画のときに cursor 位置に挿入表示する。
    preedit: String,
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

        // フォーカス中の入力処理 (IME → 通常キー の順)。
        // 同じ working buffer に積んで、最後にまとめて Edit を 1 つだけ発行する。
        let mut new_text: Option<String> = None;
        let mut committed = false;
        let mut escape_pressed = false;
        if was_focused {
            let ime_events = self.take_ime_events_if_focused(wid);
            let key_events = self.take_keyboard_events_if_focused(wid);
            if !ime_events.is_empty() || !key_events.is_empty() {
                let state: &mut TextInputState = self.widget_state(wid);
                let mut working = text.to_string();
                let mut cursor = state.cursor_byte.min(working.len());
                let mut changed = false;

                // IME 先: preedit は state にだけ反映、commit は working に挿入。
                for ev in ime_events {
                    match ev {
                        ImeEvent::Preedit { text: pre, .. } => {
                            state.preedit = pre;
                        }
                        ImeEvent::Commit(committed_text) => {
                            state.preedit.clear();
                            if !committed_text.is_empty() {
                                working.insert_str(cursor, &committed_text);
                                cursor += committed_text.len();
                                changed = true;
                            }
                        }
                    }
                }

                // 通常キー (preedit 中は KeyEvent::text の文字挿入は IME に取られる想定で空が多い)。
                for ev in key_events {
                    if !matches!(ev.state, ElementState::Pressed) {
                        continue;
                    }
                    match ev.physical_key {
                        PhysicalKey::Backspace => {
                            if cursor > 0 {
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
                            escape_pressed = true;
                        }
                        _ => {
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
                state.cursor_byte = cursor.min(working.len());
            }
        }
        if escape_pressed {
            self.clear_focus_if_focused(wid);
        }

        // 描画用 cursor + preedit を state から取り出す。
        let (cursor_byte_for_draw, preedit_for_draw) = {
            let state: &mut TextInputState = self.widget_state(wid);
            (state.cursor_byte.min(text.len()), state.preedit.clone())
        };

        // 描画。M4 Phase 11: with_widget_node で input_hash キャッシュ。
        // text / cursor / preedit / focused が同じなら描画スキップ可。
        let input_hash = hash_inputs((
            b"text_input",
            rect.x.to_bits(),
            rect.y.to_bits(),
            rect.w.to_bits(),
            rect.h.to_bits(),
            text,
            cursor_byte_for_draw as u64,
            preedit_for_draw.as_str(),
            was_focused,
        ));
        let preedit_str = preedit_for_draw.clone();
        self.with_widget_node(wid, input_hash, |ui| {
            draw_text_input(
                ui,
                rect,
                text,
                was_focused,
                cursor_byte_for_draw,
                &preedit_str,
            );
        });

        // フォーカス中なら IME 候補ウィンドウ位置を要求する (cursor 直下)。
        if was_focused {
            let pad_x = 8.0;
            let prefix = text.get(..cursor_byte_for_draw).unwrap_or("");
            let cursor_x = rect.x
                + pad_x
                + approx_text_width(prefix)
                + approx_text_width(&preedit_for_draw);
            let cursor_y_top = rect.y + 4.0;
            let cursor_h = (rect.h - 8.0).max(1.0);
            self.request_ime(Rect {
                x: cursor_x,
                y: cursor_y_top,
                w: 1.0,
                h: cursor_h,
            });
        }

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

/// 等幅フォント (HackGen Console NF) の ASCII 文字幅 (= font_size / 2 = 14 / 2 = 7)。
const APPROX_CHAR_W: f32 = 7.0;
/// 等幅フォントの CJK 文字幅 (= font_size = 14、ASCII の 2 倍)。
const APPROX_CJK_W: f32 = 14.0;

/// 文字列の概算ピクセル幅。ASCII は 8px、それ以外は 16px で計算する。
/// (preedit や cursor の x 位置を求めるのに使う暫定実装)
fn approx_text_width(text: &str) -> f32 {
    text.chars()
        .map(|c| {
            if c.is_ascii() {
                APPROX_CHAR_W
            } else {
                APPROX_CJK_W
            }
        })
        .sum()
}

fn draw_text_input<M: ?Sized + 'static>(
    ui: &mut Ui<'_, M>,
    rect: Rect,
    text: &str,
    focused: bool,
    cursor_byte: usize,
    preedit: &str,
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

    // テキスト + preedit。preedit は cursor 位置に挿入して表示する。
    let pad_x = 8.0;
    let font_size = 14.0;
    let line_h = font_size * 1.2;
    let tx = rect.x + pad_x;
    let ty = rect.y + (rect.h - line_h) * 0.5;

    // テキスト全体 (prefix + preedit + suffix) を 1 つの GlyphArea で描画する。
    // glyphon の自動レイアウトに任せれば、フォントの実 advance に基づいて正確に並ぶ
    // (proportional でも monospace でも OK)。preedit の色分けは GlyphArea が単色なので
    // 諦め、代わりに下線で区別する。色分け復活には cosmic-text の Buffer::layout_runs()
    // で実 measure が必要 (ui crate が renderer の FontSystem に access する経路を整備
    // するフェーズで対応予定)。
    let prefix = text.get(..cursor_byte).unwrap_or("");
    let suffix = text.get(cursor_byte..).unwrap_or("");
    let combined = if preedit.is_empty() {
        text.to_string()
    } else {
        let mut s = String::with_capacity(text.len() + preedit.len());
        s.push_str(prefix);
        s.push_str(preedit);
        s.push_str(suffix);
        s
    };
    if !combined.is_empty() {
        ui.push_text(GlyphArea {
            text: combined,
            left: tx,
            top: ty,
            font_size,
            line_height: line_h,
            color: Color::rgb(0.92, 0.92, 0.94),
        });
    }

    // preedit の下線 (位置は概算 — HackGen Console NF の半角=7 / 全角=14 で計算)。
    let prefix_w = approx_text_width(prefix);
    let preedit_w = approx_text_width(preedit);
    if !preedit.is_empty() {
        let pre_x = tx + prefix_w;
        let underline_y = rect.y + rect.h - 4.0;
        ui.push_lines(LineBatch {
            segments: vec![LineSegment {
                a: [pre_x, underline_y],
                b: [pre_x + preedit_w, underline_y],
                color: Color::rgb(0.95, 0.85, 0.55),
            }],
            line_width_px: 1.5,
            clip_rect: Some(rect),
        });
    }

    // カーソル (フォーカス中のみ、preedit があれば末尾)。
    if focused {
        let cursor_x = tx + prefix_w + preedit_w;
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
