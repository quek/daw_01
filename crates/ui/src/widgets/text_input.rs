//! `text_input` ウィジェット — 1 行テキスト編集 (UTF-8 / IME / OS 標準 selection)。
//!
//! - **Focus 取得時に全選択** (click / programmatic / `text_input_at_focused` 統一、F2 rename 標準挙動)
//! - **anchor + cursor の 2 点 selection** (egui `CCursorRange` 流、anchor==cursor で no-selection)
//! - **Shift+Arrow** で anchor 固定 cursor のみ動かして範囲拡張、修飾なし矢印で collapse / 移動
//! - **Ctrl+A** で全選択、文字入力 / Backspace / Delete / IME Commit / Paste は **すべて
//!   `replace_range(min..max, new)` 1 形式** に正規化して selection を範囲削除 → insert で完結
//! - **Ctrl+C / Ctrl+V / Ctrl+X** で OS clipboard 経由の cut/copy/paste
//! - **Delete** で selection あれば範囲削除、なければ cursor 後 1 char 削除
//! - **Enter / Escape** は既存通り (Response.committed / 自己 blur)
//! - **IME preedit/commit** は selection を末尾 collapse + 範囲 replace
//!
//! shortcut layer 衝突 (piano_roll / arrangement の `take_shortcut("delete")` 等) は
//! `Ui::set_typing_focus(true)` + `take_typing_shortcut(name)` で回避する (M14 Phase 57)。

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
    /// cursor の byte 位置 (selection の primary 端、Shift+Arrow で動く方)。
    /// char 境界に揃っていることを保証する。
    cursor_byte: usize,
    /// selection の anchor 端 (Shift+Arrow で固定される方)。
    /// `anchor_byte == cursor_byte` で no-selection、不等で selection あり。
    anchor_byte: usize,
    /// IME 変換中の preedit テキスト。空文字列なら preedit 中ではない。
    /// model.text には反映されず、描画のときに cursor 位置に挿入表示する。
    preedit: String,
    /// 前フレームの focus 状態 (gained_focus 検知用)。
    /// `was_focused == true && last_focused == false` で「focus 取得」と判定し全選択する。
    last_focused: bool,
    /// **(M14 Phase 59)** focus 中の編集 buffer (uncontrolled mode の source-of-truth)。
    ///
    /// 設計理由 (CLAUDE.md「ユーザに同じ workaround を書かせる API は設計欠陥」):
    /// - 旧設計: 毎フレーム `working = text.to_string()` で reset、 caller の on_change で都度 model 書き戻す
    ///   (= controlled)。 「commit するまで model に書かない」 (= rename / lyric / dialog input / search)
    ///   UX では caller が自前 buffer を持って on_change で writeback する boilerplate が必要だった。
    /// - 新設計: `was_focused == true` の間は `state.buffer_text` を source-of-truth にし、
    ///   typing で buffer を mutate、 frame 末に書き戻す。 `gained_focus` (= `last_focused == false`
    ///   から `was_focused == true` に変わった frame) で `text` 引数の値で初期化 + 全選択。
    ///   `!was_focused` のときは text 引数をそのまま表示 (controlled、 既存挙動と完全互換)。
    ///
    /// 効果:
    /// - 既存 controlled callers (daw_01 #013 rename 等): 各 keystroke で on_change → caller が
    ///   model 更新 → 次 frame text 引数 = model 値 = buffer 値、 となり挙動完全互換
    /// - uncontrolled callers (piano_roll 歌詞 inline 編集 等): on_change を no-op にしても
    ///   buffer に typed text が蓄積、 commit (Enter) で `committed_text` が正しい final text を返す
    /// - 唯一の挙動差: focus 中に **外部から `text` 引数が変わった場合** (= caller の on_change
    ///   経由ではなく、 別経路で model が変化)、 buffer が ignore する (= ユーザの typing が勝つ)。
    ///   undo/redo 中の rename 等のレアケース、 むしろ「ユーザの typing が消えない」 方が直感的。
    buffer_text: String,
}

/// `text` の `from` 位置から左方向に直近の char 境界を返す (`from` が境界ならそのまま返す)。
fn prev_char_boundary(text: &str, from: usize) -> usize {
    let mut i = from.min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// `text` の `from` 位置から右方向に直近の char 境界を返す (`from` が境界ならそのまま返す)。
fn next_char_boundary(text: &str, from: usize) -> usize {
    let mut i = from.min(text.len());
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// 範囲削除 ([min, max)) 一発。`cursor == anchor` (no-selection) なら何もせず false を返す。
/// 削除すると `cursor = anchor = min` に collapse する。
fn delete_range(working: &mut String, cursor: &mut usize, anchor: &mut usize) -> bool {
    let lo = (*cursor).min(*anchor);
    let hi = (*cursor).max(*anchor);
    if lo == hi {
        return false;
    }
    working.replace_range(lo..hi, "");
    *cursor = lo;
    *anchor = lo;
    true
}

// `focused` / `committed` / `nav_up` / `nav_down` は **意味の異なる 1 frame edge** で、
// state machine や enum にまとめると caller の `if resp.nav_up { ... }` 等の自然な扱いが
// 損なわれる (例: ↑↓ 同フレーム push を 1 つの Option<NavKey> に潰すと丸める情報が出る)。
// この struct に限り `struct_excessive_bools` を許容する。
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default)]
pub struct TextInputResponse {
    /// この widget が現在キーボードフォーカスを持っているか。
    pub focused: bool,
    /// このフレームで Enter / NumpadEnter キーが押されたか。
    pub committed: bool,
    /// (M14 Phase 59 / daw_01 #017) commit frame でのみ Some。Enter / NumpadEnter 押下時の
    /// 最終テキスト (= 直前の編集を含む working buffer)。変更が無いまま Enter したケースは
    /// caller passed `text` の clone。通常 frame は None。
    ///
    /// `on_change` callback は per-keystroke で呼ばれ、commit 時の確定 text を取り出す手段が
    /// なかった (`piano_roll` の歌詞 inline 編集で「Enter で commit text を取り出して
    /// `split_into_morae` で分割→次 note へ分配」が必要になり追加)。
    pub committed_text: Option<String>,
    /// (M14 Phase 86 / daw_01 #057) focus 中にこのフレームで ↑ キーが押されたか。
    /// text_input は単一行で ↑↓ を内部利用しないため、 type-ahead picker / combobox 等
    /// 「検索ボックスに focus を保ったまま候補リストの cursor を上下移動したい」 caller に
    /// 委譲する。 Left/Right は cursor 移動に使うため返さない。
    pub nav_up: bool,
    /// (M14 Phase 86 / daw_01 #057) focus 中にこのフレームで ↓ キーが押されたか。 詳細は [`Self::nav_up`]。
    pub nav_down: bool,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 矩形指定で 1 行 text_input を描画 + キー入力処理。
    /// 編集が起きたら `on_change(new_text)` を Edit 列に積む。
    #[allow(clippy::too_many_lines)]
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

        // Click inside → focus 取得 (cursor / anchor は下の gained_focus 検知で全選択する)。
        if click {
            self.set_focus(wid);
        }

        // Press が rect 外で発生 + 自分が focus を持っていたら自己 blur。
        // これで「外をクリック → 次の入力イベントを待たずに枠線が即座に消える」になる。
        if pointer.primary_just_pressed && !inside && self.is_focused(wid) {
            self.clear_focus_if_focused(wid);
        }

        let was_focused = self.is_focused(wid);

        // gained_focus 検知 + 全選択 + buffer_text 初期化 (M14 Phase 59)。
        // click / programmatic focus / `text_input_at_focused` を 1 箇所で処理 (F2 rename 標準挙動)。
        // `buffer_text` は focus 中の source-of-truth で、 gained_focus でのみ `text` 引数から
        // 初期化、 typing で mutate される。 `!was_focused` のとき `text` 引数表示 (controlled)。
        {
            let state: &mut TextInputState = self.widget_state(wid);
            let prev = std::mem::replace(&mut state.last_focused, was_focused);
            if was_focused && !prev {
                state.buffer_text = text.to_string();
                state.anchor_byte = 0;
                state.cursor_byte = state.buffer_text.len();
            }
        }

        // 表示 / working buffer の source-of-truth を決定 (M14 Phase 59):
        // - was_focused: state.buffer_text (uncontrolled、 typing で mutate)
        // - !was_focused: text 引数 (controlled、 caller が source-of-truth)
        // 後段の typing 処理で buffer_text が変化したら、 frame 末に再 read して描画 / IME /
        // committed_text に反映するため `mut` で持つ。
        let mut displayed_text: String = if was_focused {
            let state: &mut TextInputState = self.widget_state(wid);
            state.buffer_text.clone()
        } else {
            text.to_string()
        };

        // フォーカス中の shortcut + IME + キー入力処理。selection を考慮して
        // 文字入力 / Backspace / Delete / IME Commit / Paste は全部
        // `replace_range(min..max, new)` 1 形式に正規化する。
        let mut new_text: Option<String> = None;
        let mut committed = false;
        let mut escape_pressed = false;
        let mut nav_up = false;
        let mut nav_down = false;
        if was_focused {
            // typing-only shortcut (前フレームに `set_typing_focus(true)` を出していたフレームで
            // shortcut layer から keyboard_events に残してある) を先に拾う。
            let select_all_pressed = self.take_typing_shortcut("select_all");
            let delete_pressed = self.take_typing_shortcut("delete");
            let cut_pressed = self.take_typing_shortcut("cut");
            let copy_pressed = self.take_typing_shortcut("copy");
            let paste_pressed = self.take_typing_shortcut("paste");
            let pasted_text = if paste_pressed {
                self.take_clipboard_paste()
            } else {
                None
            };

            let ime_events = self.take_ime_events_if_focused(wid);
            let key_events = self.take_keyboard_events_if_focused(wid);
            let mods = pointer.modifiers;
            let any_input = select_all_pressed
                || delete_pressed
                || cut_pressed
                || copy_pressed
                || paste_pressed
                || !ime_events.is_empty()
                || !key_events.is_empty();

            if any_input {
                let state: &mut TextInputState = self.widget_state(wid);
                // M14 Phase 59: working は state.buffer_text (focus 中の source-of-truth)
                // から開始。 旧設計の毎フレーム `text` reset を廃止し、 typing が frame 跨ぎで
                // 蓄積されるように。 frame 末で state.buffer_text に書き戻す。
                let mut working = state.buffer_text.clone();
                let mut cursor = state.cursor_byte.min(working.len());
                let mut anchor = state.anchor_byte.min(working.len());
                let mut changed = false;
                let mut clipboard_write: Option<String> = None;

                // IME 先: preedit 開始時に selection を範囲削除して collapse、
                // commit は (preedit 経由で既に collapse 済みのはずだが念のため) 再度範囲削除して insert。
                for ev in ime_events {
                    match ev {
                        ImeEvent::Preedit { text: pre, .. } => {
                            if state.preedit.is_empty()
                                && delete_range(&mut working, &mut cursor, &mut anchor)
                            {
                                changed = true;
                            }
                            state.preedit = pre;
                        }
                        ImeEvent::Commit(committed_text) => {
                            state.preedit.clear();
                            if delete_range(&mut working, &mut cursor, &mut anchor) {
                                changed = true;
                            }
                            if !committed_text.is_empty() {
                                working.insert_str(cursor, &committed_text);
                                cursor += committed_text.len();
                                anchor = cursor;
                                changed = true;
                            }
                        }
                        // M15: OS text store (TSF) 由来の任意 range 置換 (まぜ書き変換結果 /
                        // 再変換)。selection ではなく明示 range を `replace_range` で書き換える。
                        ImeEvent::ReplaceRange { start_byte, end_byte, text: rep, new_cursor } => {
                            state.preedit.clear();
                            let len = working.len();
                            let mut lo = start_byte.min(len);
                            let mut hi = end_byte.min(len);
                            if lo > hi {
                                std::mem::swap(&mut lo, &mut hi);
                            }
                            // ACP→byte 変換は char 境界を保証するが、防御的に丸める。
                            lo = prev_char_boundary(&working, lo);
                            hi = next_char_boundary(&working, hi);
                            working.replace_range(lo..hi, &rep);
                            cursor = prev_char_boundary(&working, new_cursor.min(working.len()));
                            anchor = cursor;
                            changed = true;
                        }
                        // M15: text store (TSF) からの selection 変更 (text 不変)。
                        ImeEvent::SetSelection { anchor_byte, cursor_byte } => {
                            state.preedit.clear();
                            let len = working.len();
                            anchor = prev_char_boundary(&working, anchor_byte.min(len));
                            cursor = prev_char_boundary(&working, cursor_byte.min(len));
                            // text 不変なので changed は立てない (cursor/anchor のみ更新)。
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
                            if delete_range(&mut working, &mut cursor, &mut anchor) {
                                changed = true;
                            } else if cursor > 0 {
                                let prev = prev_char_boundary(&working, cursor - 1);
                                working.replace_range(prev..cursor, "");
                                cursor = prev;
                                anchor = prev;
                                changed = true;
                            }
                        }
                        PhysicalKey::ArrowLeft => {
                            if mods.shift {
                                // selection 拡張: anchor 固定で cursor だけ左移動。
                                if cursor > 0 {
                                    cursor = prev_char_boundary(&working, cursor - 1);
                                }
                            } else if cursor != anchor {
                                // selection を min に collapse。
                                let lo = cursor.min(anchor);
                                cursor = lo;
                                anchor = lo;
                            } else if cursor > 0 {
                                cursor = prev_char_boundary(&working, cursor - 1);
                                anchor = cursor;
                            }
                        }
                        PhysicalKey::ArrowRight => {
                            if mods.shift {
                                if cursor < working.len() {
                                    cursor = next_char_boundary(&working, cursor + 1);
                                }
                            } else if cursor != anchor {
                                let hi = cursor.max(anchor);
                                cursor = hi;
                                anchor = hi;
                            } else if cursor < working.len() {
                                cursor = next_char_boundary(&working, cursor + 1);
                                anchor = cursor;
                            }
                        }
                        // M14 Phase 57 (daw_01 #016): NumpadEnter も commit 扱い。
                        // DAW の数値入力 (BPM / time_sig / 拍数 / ピッチ等) でテンキー Enter を
                        // 多用する慣習 (Cubase / REAPER / Logic 全部 numpad Enter で commit)。
                        PhysicalKey::Enter | PhysicalKey::NumpadEnter => {
                            committed = true;
                        }
                        PhysicalKey::Escape => {
                            escape_pressed = true;
                        }
                        // (M14 Phase 86 / daw_01 #057) ↑↓ は text_input 単一行では未使用なので
                        // edge を Response に積んで caller に委譲 (type-ahead picker / combobox 用)。
                        // keyboard_events から消費されるが ev.text は無いので text への影響なし。
                        PhysicalKey::ArrowUp => {
                            nav_up = true;
                        }
                        PhysicalKey::ArrowDown => {
                            nav_down = true;
                        }
                        _ => {
                            if let Some(input_text) = &ev.text {
                                let filtered: String =
                                    input_text.chars().filter(|c| !c.is_control()).collect();
                                if !filtered.is_empty() {
                                    delete_range(&mut working, &mut cursor, &mut anchor);
                                    working.insert_str(cursor, &filtered);
                                    cursor += filtered.len();
                                    anchor = cursor;
                                    changed = true;
                                }
                            }
                        }
                    }
                }

                // typing-only shortcut の処理 (key 処理後)。
                if select_all_pressed {
                    anchor = 0;
                    cursor = working.len();
                }
                if cut_pressed || copy_pressed {
                    let lo = cursor.min(anchor);
                    let hi = cursor.max(anchor);
                    if lo != hi {
                        clipboard_write = Some(working[lo..hi].to_string());
                        if cut_pressed {
                            working.replace_range(lo..hi, "");
                            cursor = lo;
                            anchor = lo;
                            changed = true;
                        }
                    }
                }
                if paste_pressed
                    && let Some(p) = pasted_text
                {
                    let filtered: String = p.chars().filter(|c| !c.is_control()).collect();
                    if delete_range(&mut working, &mut cursor, &mut anchor) {
                        changed = true;
                    }
                    if !filtered.is_empty() {
                        working.insert_str(cursor, &filtered);
                        cursor += filtered.len();
                        anchor = cursor;
                        changed = true;
                    }
                }
                if delete_pressed {
                    if delete_range(&mut working, &mut cursor, &mut anchor) {
                        changed = true;
                    } else if cursor < working.len() {
                        let next = next_char_boundary(&working, cursor + 1);
                        working.replace_range(cursor..next, "");
                        // cursor / anchor は変わらない (削除された分が後ろに詰まる)
                        anchor = cursor;
                        changed = true;
                    }
                }

                if changed {
                    new_text = Some(working.clone());
                }
                state.cursor_byte = cursor.min(working.len());
                state.anchor_byte = anchor.min(working.len());
                // M14 Phase 59: buffer_text に書き戻す (frame 跨ぎの source-of-truth)
                state.buffer_text.clone_from(&working);
                // displayed_text も typing 後の値に更新 (描画 / IME / committed_text 用)
                displayed_text = working;
                if let Some(s) = clipboard_write {
                    self.set_clipboard_text(s);
                }
            }
        }
        if escape_pressed {
            self.clear_focus_if_focused(wid);
        }

        // 描画用 cursor + anchor + preedit を state から取り出す。
        // M14 Phase 59: cursor / anchor の clamp は displayed_text (= focus 中 buffer_text、
        // 非 focus 中 text 引数) の長さに対して行う。
        let (cursor_byte_for_draw, anchor_byte_for_draw, preedit_for_draw) = {
            let state: &mut TextInputState = self.widget_state(wid);
            (
                state.cursor_byte.min(displayed_text.len()),
                state.anchor_byte.min(displayed_text.len()),
                state.preedit.clone(),
            )
        };

        // 描画。M4 Phase 11: with_widget_node で input_hash キャッシュ。
        // text / cursor / anchor / preedit / focused が同じなら描画スキップ可。
        let input_hash = hash_inputs((
            b"text_input",
            rect.x.to_bits(),
            rect.y.to_bits(),
            rect.w.to_bits(),
            rect.h.to_bits(),
            displayed_text.as_str(),
            cursor_byte_for_draw as u64,
            anchor_byte_for_draw as u64,
            preedit_for_draw.as_str(),
            was_focused,
        ));
        let preedit_str = preedit_for_draw.clone();
        let displayed_for_draw = displayed_text.clone();
        self.with_widget_node(wid, input_hash, |ui| {
            draw_text_input(
                ui,
                rect,
                &displayed_for_draw,
                was_focused,
                cursor_byte_for_draw,
                anchor_byte_for_draw,
                &preedit_str,
            );
        });

        // フォーカス中なら IME 候補ウィンドウ位置を要求する (cursor 直下)。
        if was_focused {
            let pad_x = 8.0;
            let font_size = 14.0;
            let prefix = displayed_text.get(..cursor_byte_for_draw).unwrap_or("");
            let cursor_x = rect.x
                + pad_x
                + self.measure_text(prefix, font_size)
                + self.measure_text(&preedit_for_draw, font_size);
            let cursor_y_top = rect.y + 4.0;
            let cursor_h = (rect.h - 8.0).max(1.0);
            let caret = Rect { x: cursor_x, y: cursor_y_top, w: 1.0, h: cursor_h };
            self.request_ime(caret);
            // M15: text store (TSF) に text + selection + caret を publish。rtry のまぜ書き
            // GetText / MS-IME 再変換がアプリのテキストを読めるようにする。preedit 中は selection
            // を collapse 済みなので displayed の cursor/anchor をそのまま渡す。
            self.publish_text_document(
                &displayed_text,
                anchor_byte_for_draw,
                cursor_byte_for_draw,
                caret,
            );
            // M8 Phase 30: typing 中フラグを立てて、修飾なし shortcut の global 発動を抑制可能に。
            self.set_typing_focus(true);
        }

        // commit frame では「Enter 押下時点の working buffer の最終値」を返す。
        // M14 Phase 59: displayed_text は typing 反映後の値 (focus 中 = buffer_text、
        // 非 focus 中 = text 引数)。 commit 時はこれをそのまま返せば良い。
        let committed_text = if committed { Some(displayed_text.clone()) } else { None };

        if let Some(t) = new_text {
            let edit = on_change(t);
            self.push_edit(edit);
        }

        TextInputResponse {
            focused: was_focused,
            committed,
            committed_text,
            nav_up,
            nav_down,
        }
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

    /// M11 Phase 52 (daw_01 conversation #013): `text_input_at` と同じ挙動だが、widget が
    /// 「前フレームに登場していなかった」場合に **自動でキーボードフォーカスを取得**
    /// + 全選択する (`text_input_at` の gained_focus 検知が 1 箇所で処理する)。
    ///
    /// 用途: rename UI / inline edit の「メニュー → text_input 表示 → 即タイプで上書き」
    /// (Logic / Bitwig / Cubase / OS の F2 rename 慣習) を 1 関数で実現する。
    ///
    /// 「初回 show」判定は internal Scenegraph (前フレーム登場 widget の eviction 機構)
    /// を使う実装で、caller 側の boolean flag は不要。完全に非表示 (フレーム飛ばし) →
    /// 戻ったときも再度 focus する。
    ///
    /// M14 Phase 57 で「初回 show 時 cursor を末尾」→「初回 show 時 全選択」に変更
    /// (CLAUDE.md「破壊的 API 変更を恐れない」、OS の F2 rename 標準挙動と一致)。
    pub fn text_input_at_focused<F>(
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
        if !self.was_widget_visible_last_frame(wid) {
            // 初回 show (or 一度消えて再登場): 自動 focus 取得 + state reset。
            // **重要 (M14 Phase 59 / daw_01 #017 再表示 bug fix)**:
            // state.last_focused が前 session の終了時 (`true`) のまま残っていると、
            // 直後の `text_input_at` で `prev == true` となり gained_focus が検知されず
            // buffer / 全選択 reset が走らない (= 前回入力した text が再表示される、
            // ユーザーから見ると「既に分配済の歌詞 'abc' が再び出てしまう」 症状)。
            // ここで明示的に `last_focused = false` に戻して、 直後の gained_focus path を
            // 発火させる (buffer = text 引数で初期化 + 全選択)。
            self.set_focus(wid);
            let state: &mut TextInputState = self.widget_state(wid);
            state.last_focused = false;
        }
        self.text_input_at(id, rect, text, on_change)
    }
}

fn draw_text_input<M: ?Sized + 'static>(
    ui: &mut Ui<'_, M>,
    rect: Rect,
    text: &str,
    focused: bool,
    cursor_byte: usize,
    anchor_byte: usize,
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
        clip_rect: None,
    });

    // テキスト + preedit。preedit は cursor 位置に挿入して表示する。
    let pad_x = 8.0;
    let font_size = 14.0;
    let line_h = font_size * 1.2;
    let tx = rect.x + pad_x;
    let ty = rect.y + (rect.h - line_h) * 0.5;

    // M14 Phase 58: prefix / preedit の x advance を **glyphon と同じ shape** で実測。
    // 旧 `approx_text_width` (ASCII 7px / CJK 14px の固定概算) は proportional system font
    // の "m" (~11px) や "i" (~4px) で実 advance と大きくずれていた。
    let prefix = text.get(..cursor_byte).unwrap_or("");
    let suffix = text.get(cursor_byte..).unwrap_or("");
    let prefix_w = ui.measure_text(prefix, font_size);
    let preedit_w = ui.measure_text(preedit, font_size);

    // M14 Phase 57: 選択範囲の半透明矩形 (背景の上、テキストの下に積む)。
    // preedit 中は selection は collapse 済みなので考慮不要。focus 喪失時は描画しない。
    let sel_lo = cursor_byte.min(anchor_byte);
    let sel_hi = cursor_byte.max(anchor_byte);
    if focused && sel_lo != sel_hi && preedit.is_empty() {
        let lo_str = text.get(..sel_lo).unwrap_or("");
        let hi_str = text.get(..sel_hi).unwrap_or("");
        let sel_lo_w = ui.measure_text(lo_str, font_size);
        let sel_hi_w = ui.measure_text(hi_str, font_size);
        let sel_x = tx + sel_lo_w;
        let sel_w = sel_hi_w - sel_lo_w;
        ui.push_rect(RectCommand {
            rect: Rect { x: sel_x, y: ty, w: sel_w, h: line_h },
            fill: Color::rgba(0.30, 0.50, 0.85, 0.45),
            border: Color::rgba(0.0, 0.0, 0.0, 0.0),
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: Some(rect),
        });
    }

    // テキスト全体 (prefix + preedit + suffix) を 1 つの GlyphArea で描画する。
    // glyphon の自動レイアウトに任せれば、フォントの実 advance に基づいて正確に並ぶ
    // (proportional でも monospace でも OK)。preedit の色分けは GlyphArea が単色なので
    // 諦め、代わりに下線で区別する。
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
            text: combined.into(),
            left: tx,
            top: ty,
            font_size,
            line_height: line_h,
            color: Color::rgb(0.92, 0.92, 0.94),
            clip_rect: None,
            ..GlyphArea::default()
        });
    }

    // preedit の下線 (位置は実 measure)。
    if !preedit.is_empty() {
        let pre_x = tx + prefix_w;
        let underline_y = rect.y + rect.h - 4.0;
        ui.push_lines(LineBatch {
            segments: vec![LineSegment {
                a: [pre_x, underline_y],
                b: [pre_x + preedit_w, underline_y],
                color: Color::rgb(0.95, 0.85, 0.55),
            }]
            .into(),
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
            }]
            .into(),
            line_width_px: 1.5,
            clip_rect: Some(rect),
        });
    }
}

#[cfg(test)]
mod tests {
    //! M11 Phase 52: `text_input_at_focused` のテスト群。
    //! 「初回 show」判定が前フレームの Scenegraph 登場有無に基づくこと、再登場時に
    //! 再 focus されること、連続 visible では caller の手動 blur を上書きしないこと
    //! を確認する。

    use daw_ui_platform::PhysicalSize;
    use daw_ui_renderer::{Rect, Scene};

    use crate::edit::Edit;
    use crate::id::WidgetId;
    use crate::input::FrameInput;
    use crate::ui::UiHost;

    /// `text_input_at_focused` が内部で計算する WidgetId と同じ式。テスト用 helper。
    fn text_input_wid(id: &str) -> WidgetId {
        WidgetId::ROOT.child((b"text_input", &id))
    }

    #[test]
    fn focused_variant_takes_focus_on_first_show() {
        // frame 1: text_input_at_focused 呼び出し → focus 取得
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.text_input_at_focused(
                "rename",
                Rect { x: 10.0, y: 10.0, w: 200.0, h: 28.0 },
                "abc",
                |_new| Edit::mutate(|()| {}),
            );
        });

        assert_eq!(
            host.focused_widget(),
            Some(text_input_wid("rename")),
            "初回 show でキーボードフォーカスを取得"
        );
    }

    #[test]
    fn focused_variant_does_not_steal_focus_when_continuously_visible() {
        // frame 1: text_input_at_focused 呼び出し → focus 取得
        // frame 2: caller 側で別の widget に focus 移動 (set_focus 直接呼び)
        //          → text_input_at_focused は連続 visible なので focus を奪い返さない
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let other_wid = WidgetId::ROOT.child(b"other_input");

        // frame 1
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.text_input_at_focused(
                "rename",
                Rect { x: 10.0, y: 10.0, w: 200.0, h: 28.0 },
                "abc",
                |_new| Edit::mutate(|()| {}),
            );
        });
        assert_eq!(host.focused_widget(), Some(text_input_wid("rename")));
        scene.clear();

        // frame 2: caller 側で別 widget に focus 移動 + text_input_at_focused 連続呼び出し
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.set_focus(other_wid);
            ui.text_input_at_focused(
                "rename",
                Rect { x: 10.0, y: 10.0, w: 200.0, h: 28.0 },
                "abc",
                |_new| Edit::mutate(|()| {}),
            );
        });
        assert_eq!(
            host.focused_widget(),
            Some(other_wid),
            "連続 visible なら focus を奪い返さない (= caller の set_focus が勝つ)"
        );
    }

    #[test]
    fn focused_variant_re_focuses_after_invisible_frame() {
        // frame 1: 表示 → focus 取得
        // frame 2: 表示せず (= 不可視 → eviction)
        // frame 3: 再表示 → 再度 focus 取得
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let other_wid = WidgetId::ROOT.child(b"other_input");

        // frame 1: text_input_at_focused 表示 → focus
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.text_input_at_focused(
                "rename",
                Rect { x: 10.0, y: 10.0, w: 200.0, h: 28.0 },
                "abc",
                |_new| Edit::mutate(|()| {}),
            );
        });
        assert_eq!(host.focused_widget(), Some(text_input_wid("rename")));
        scene.clear();

        // frame 2: 表示なし (text_input_at_focused 呼び出さない) + 別 widget に focus 移動
        // (この間に scenegraph eviction で text_input の entry は消える)
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.set_focus(other_wid);
        });
        assert_eq!(host.focused_widget(), Some(other_wid));
        scene.clear();

        // frame 3: 再表示 → 再 focus
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.text_input_at_focused(
                "rename",
                Rect { x: 10.0, y: 10.0, w: 200.0, h: 28.0 },
                "abc",
                |_new| Edit::mutate(|()| {}),
            );
        });
        assert_eq!(
            host.focused_widget(),
            Some(text_input_wid("rename")),
            "不可視 → 再表示で再度 focus 取得 (= eviction で初回 show と同じ扱い)"
        );
    }

    /// daw_01 #016: テンキー Enter (`PhysicalKey::NumpadEnter`) も commit 扱いになる。
    /// メインキー Enter (`PhysicalKey::Enter`) と同じく `TextInputResponse.committed = true`
    /// を返す。DAW 数値入力 (BPM / time_sig / 拍数 / ピッチ等) でテンキー Enter を多用する
    /// 業界慣習 (Cubase / REAPER / Logic) に合わせる。
    #[test]
    fn commit_fires_on_numpad_enter() {
        use std::cell::Cell;

        use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey};

        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        // frame 1: text_input_at_focused 表示 → 自動 focus
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.text_input_at_focused(
                "ti",
                Rect { x: 10.0, y: 10.0, w: 200.0, h: 28.0 },
                "abc",
                |_new| Edit::mutate(|()| {}),
            );
        });

        // frame 2: NumpadEnter を送って committed = true を確認
        let committed = Cell::new(false);
        let numpad_enter = KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::NumpadEnter,
        };
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { keyboard: vec![numpad_enter], ..Default::default() },
            |(), ui| {
                let resp = ui.text_input_at_focused(
                    "ti",
                    Rect { x: 10.0, y: 10.0, w: 200.0, h: 28.0 },
                    "abc",
                    |_new| Edit::mutate(|()| {}),
                );
                committed.set(resp.committed);
            },
        );
        assert!(committed.get(), "NumpadEnter で committed=true");
    }

    /// 既存挙動の回帰確認: メインキー Enter でも引き続き commit する。
    #[test]
    fn commit_still_fires_on_main_enter() {
        use std::cell::Cell;

        use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey};

        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.text_input_at_focused(
                "ti",
                Rect { x: 10.0, y: 10.0, w: 200.0, h: 28.0 },
                "abc",
                |_new| Edit::mutate(|()| {}),
            );
        });

        let committed = Cell::new(false);
        let enter = KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Enter,
        };
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { keyboard: vec![enter], ..Default::default() },
            |(), ui| {
                let resp = ui.text_input_at_focused(
                    "ti",
                    Rect { x: 10.0, y: 10.0, w: 200.0, h: 28.0 },
                    "abc",
                    |_new| Edit::mutate(|()| {}),
                );
                committed.set(resp.committed);
            },
        );
        assert!(committed.get(), "Enter でも従来通り committed=true (回帰防止)");
    }

    // ============================================================================
    // M14 Phase 57: selection / Ctrl+A / Shift+Arrow / Delete / cut/copy/paste
    // ============================================================================

    use std::sync::{Arc, Mutex};

    use daw_ui_platform::{ElementState, KeyEvent, Modifiers, PhysicalKey};

    use crate::clipboard::ClipboardProvider;
    use crate::input::PointerFrame;

    /// テスト用 clipboard provider (Arc<Mutex<...>> で内容を共有して assertion で確認できる)。
    struct MemClipboard {
        text: Arc<Mutex<Option<String>>>,
    }

    impl ClipboardProvider for MemClipboard {
        fn get_text(&mut self) -> Option<String> {
            self.text.lock().unwrap().clone()
        }
        fn set_text(&mut self, t: String) {
            *self.text.lock().unwrap() = Some(t);
        }
    }

    fn key_pressed(physical: PhysicalKey) -> KeyEvent {
        KeyEvent { state: ElementState::Pressed, text: None, physical_key: physical }
    }

    fn key_pressed_text(physical: PhysicalKey, t: &str) -> KeyEvent {
        KeyEvent {
            state: ElementState::Pressed,
            text: Some(t.into()),
            physical_key: physical,
        }
    }

    fn frame_with_keys(keys: Vec<KeyEvent>, mods: Modifiers) -> FrameInput {
        FrameInput {
            pointer: PointerFrame { modifiers: mods, ..Default::default() },
            keyboard: keys,
            ..Default::default()
        }
    }

    /// Frame 1 (focus 取得 + 全選択 + typing_focus 立て) を回す helper。
    /// `frame()` を使うので末尾で clipboard provider への flush も走る (cut/copy 検証用)。
    fn run_focus_frame(host: &mut UiHost<()>, scene: &mut Scene, screen: PhysicalSize, text: &str) {
        let mut m = ();
        host.frame(&mut m, scene, screen, FrameInput::default(), |(), ui| {
            ui.text_input_at_focused(
                "ti",
                Rect { x: 10.0, y: 10.0, w: 200.0, h: 28.0 },
                text,
                |_new| Edit::mutate(|()| {}),
            );
        });
    }

    /// Frame 2: 与えた input でキー処理を 1 回回し、on_change に渡された text を返す。
    /// `text_input_at_focused` を使うので caller は連続して同じ text を渡せばよい。
    fn run_input_frame(
        host: &mut UiHost<()>,
        scene: &mut Scene,
        screen: PhysicalSize,
        text: &str,
        input: FrameInput,
    ) -> Option<String> {
        let observed: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let observed_capt = observed.clone();
        let mut m = ();
        host.frame(&mut m, scene, screen, input, |(), ui| {
            ui.text_input_at_focused(
                "ti",
                Rect { x: 10.0, y: 10.0, w: 200.0, h: 28.0 },
                text,
                |new| {
                    *observed_capt.lock().unwrap() = Some(new);
                    Edit::mutate(|()| {})
                },
            );
        });
        observed.lock().unwrap().clone()
    }

    #[test]
    fn gained_focus_selects_all_and_typing_replaces_text() {
        // Frame 1: focus 取得 → 全選択 (anchor=0, cursor=3 for "abc")
        // Frame 2: 't' 入力 → selection 範囲削除 + 't' insert → "t" になる
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        run_focus_frame(&mut host, &mut scene, screen, "abc");
        scene.clear();

        let input = frame_with_keys(
            vec![key_pressed_text(PhysicalKey::Char('T'), "t")],
            Modifiers::default(),
        );
        let observed = run_input_frame(&mut host, &mut scene, screen, "abc", input);
        assert_eq!(observed.as_deref(), Some("t"), "全選択 → 't' で全置換");
    }

    #[test]
    fn ctrl_a_selects_all_then_delete_clears_text() {
        // Frame 1: focus 取得 (= 全選択)。
        // Frame 2: ArrowRight 2回 → selection collapse + cursor=末尾 (no-selection, cursor=3)。
        // Frame 3: Ctrl+A → 全選択 (anchor=0, cursor=3)。
        // Frame 4: Delete → 範囲削除 → "" になる。
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        run_focus_frame(&mut host, &mut scene, screen, "abc");
        scene.clear();

        // Frame 2: ArrowRight (selection を末尾に collapse、cursor=3 anchor=3)
        let _ = run_input_frame(
            &mut host,
            &mut scene,
            screen,
            "abc",
            frame_with_keys(vec![key_pressed(PhysicalKey::ArrowRight)], Modifiers::default()),
        );
        scene.clear();

        // Frame 3: Ctrl+A → 全選択 (typing-only shortcut なので keyboard_events に残り、
        // text_input が take_typing_shortcut("select_all") で受ける)。
        let ctrl = Modifiers { ctrl: true, ..Modifiers::empty() };
        let _ = run_input_frame(
            &mut host,
            &mut scene,
            screen,
            "abc",
            frame_with_keys(vec![key_pressed(PhysicalKey::Char('A'))], ctrl),
        );
        scene.clear();

        // Frame 4: Delete → 範囲削除 → ""
        let observed = run_input_frame(
            &mut host,
            &mut scene,
            screen,
            "abc",
            frame_with_keys(vec![key_pressed(PhysicalKey::Delete)], Modifiers::default()),
        );
        assert_eq!(observed.as_deref(), Some(""), "Ctrl+A → Delete で空文字列");
    }

    #[test]
    fn delete_no_selection_removes_one_char_after_cursor() {
        // Frame 1: focus + 全選択
        // Frame 2: ArrowLeft → cursor=anchor=0 (collapse to min)
        // Frame 3: Delete → cursor 後 1 char ("abc" → "bc")
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        run_focus_frame(&mut host, &mut scene, screen, "abc");
        scene.clear();
        let _ = run_input_frame(
            &mut host,
            &mut scene,
            screen,
            "abc",
            frame_with_keys(vec![key_pressed(PhysicalKey::ArrowLeft)], Modifiers::default()),
        );
        scene.clear();
        let observed = run_input_frame(
            &mut host,
            &mut scene,
            screen,
            "abc",
            frame_with_keys(vec![key_pressed(PhysicalKey::Delete)], Modifiers::default()),
        );
        assert_eq!(observed.as_deref(), Some("bc"), "selection なし Delete で 1 char 削除");
    }

    #[test]
    fn backspace_with_selection_removes_range() {
        // 全選択状態で Backspace → 範囲削除 → ""
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        run_focus_frame(&mut host, &mut scene, screen, "abc");
        scene.clear();
        let observed = run_input_frame(
            &mut host,
            &mut scene,
            screen,
            "abc",
            frame_with_keys(vec![key_pressed(PhysicalKey::Backspace)], Modifiers::default()),
        );
        assert_eq!(observed.as_deref(), Some(""), "全選択 + Backspace で範囲削除");
    }

    #[test]
    fn shift_arrow_extends_selection_then_typing_replaces_only_selection() {
        // Frame 1: focus + 全選択 (anchor=0 cursor=3)
        // Frame 2: ArrowRight → collapse cursor=3 anchor=3
        // Frame 3: Shift+ArrowLeft → cursor=2 anchor=3 (= "c" 1 char 選択)
        // Frame 4: 'X' → "ab" + "X" = "abX"
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        run_focus_frame(&mut host, &mut scene, screen, "abc");
        scene.clear();
        let _ = run_input_frame(
            &mut host,
            &mut scene,
            screen,
            "abc",
            frame_with_keys(vec![key_pressed(PhysicalKey::ArrowRight)], Modifiers::default()),
        );
        scene.clear();
        let shift = Modifiers { shift: true, ..Modifiers::empty() };
        let _ = run_input_frame(
            &mut host,
            &mut scene,
            screen,
            "abc",
            frame_with_keys(vec![key_pressed(PhysicalKey::ArrowLeft)], shift),
        );
        scene.clear();
        let observed = run_input_frame(
            &mut host,
            &mut scene,
            screen,
            "abc",
            frame_with_keys(
                vec![key_pressed_text(PhysicalKey::Char('X'), "X")],
                Modifiers::default(),
            ),
        );
        assert_eq!(
            observed.as_deref(),
            Some("abX"),
            "Shift+ArrowLeft で 1 char 選択 → 'X' で末尾 1 char を置換"
        );
    }

    #[test]
    fn copy_writes_selection_to_clipboard_without_modifying_text() {
        // 全選択 + Ctrl+C → clipboard に "abc"、text 不変
        let clip_text: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let provider = MemClipboard { text: clip_text.clone() };

        let mut host: UiHost<()> = UiHost::no_redraw().with_clipboard(provider);
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        run_focus_frame(&mut host, &mut scene, screen, "abc");
        scene.clear();
        let ctrl = Modifiers { ctrl: true, ..Modifiers::empty() };
        let observed = run_input_frame(
            &mut host,
            &mut scene,
            screen,
            "abc",
            frame_with_keys(vec![key_pressed(PhysicalKey::Char('C'))], ctrl),
        );
        // text 不変 → on_change が呼ばれず observed = None
        assert_eq!(observed, None, "Ctrl+C は text を変えない");
        assert_eq!(
            clip_text.lock().unwrap().as_deref(),
            Some("abc"),
            "selection が clipboard に書かれる"
        );
    }

    #[test]
    fn cut_writes_selection_to_clipboard_and_removes() {
        // 全選択 + Ctrl+X → clipboard に "abc"、text = ""
        let clip_text: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let provider = MemClipboard { text: clip_text.clone() };

        let mut host: UiHost<()> = UiHost::no_redraw().with_clipboard(provider);
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        run_focus_frame(&mut host, &mut scene, screen, "abc");
        scene.clear();
        let ctrl = Modifiers { ctrl: true, ..Modifiers::empty() };
        let observed = run_input_frame(
            &mut host,
            &mut scene,
            screen,
            "abc",
            frame_with_keys(vec![key_pressed(PhysicalKey::Char('X'))], ctrl),
        );
        assert_eq!(observed.as_deref(), Some(""), "Ctrl+X で範囲削除");
        assert_eq!(
            clip_text.lock().unwrap().as_deref(),
            Some("abc"),
            "selection が clipboard に書かれる"
        );
    }

    // ============================================================================
    // M14 Phase 86 (daw_01 #057): focus 中の ↑↓ を `TextInputResponse` に edge 返却
    // ============================================================================

    /// focus 中の ArrowUp → `resp.nav_up == true` / text 不変 / cursor 不変。
    /// ArrowDown も同様。 type-ahead picker (検索ボックスに focus を保ったまま
    /// 候補リストの cursor を上下移動) 用。
    #[test]
    fn arrow_up_down_reported_via_response_without_text_change() {
        use std::cell::Cell;

        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        // Frame 1: focus 取得
        run_focus_frame(&mut host, &mut scene, screen, "abc");
        scene.clear();

        // Frame 2: ArrowUp → nav_up = true、 text に影響なし
        let nav_up = Cell::new(false);
        let nav_down = Cell::new(false);
        let mut m = ();
        host.frame(
            &mut m,
            &mut scene,
            screen,
            frame_with_keys(vec![key_pressed(PhysicalKey::ArrowUp)], Modifiers::default()),
            |(), ui| {
                let resp = ui.text_input_at_focused(
                    "ti",
                    Rect { x: 10.0, y: 10.0, w: 200.0, h: 28.0 },
                    "abc",
                    |new| {
                        // 反映しない (controlled caller の最小形): on_change が呼ばれたら不正。
                        panic!("ArrowUp で on_change が呼ばれた (text={new})");
                    },
                );
                nav_up.set(resp.nav_up);
                nav_down.set(resp.nav_down);
            },
        );
        assert!(nav_up.get(), "ArrowUp で nav_up=true");
        assert!(!nav_down.get(), "ArrowUp 単独で nav_down は false のまま");
        scene.clear();

        // Frame 3: ArrowDown → nav_down = true
        let nav_up = Cell::new(false);
        let nav_down = Cell::new(false);
        host.frame(
            &mut m,
            &mut scene,
            screen,
            frame_with_keys(vec![key_pressed(PhysicalKey::ArrowDown)], Modifiers::default()),
            |(), ui| {
                let resp = ui.text_input_at_focused(
                    "ti",
                    Rect { x: 10.0, y: 10.0, w: 200.0, h: 28.0 },
                    "abc",
                    |new| panic!("ArrowDown で on_change が呼ばれた (text={new})"),
                );
                nav_up.set(resp.nav_up);
                nav_down.set(resp.nav_down);
            },
        );
        assert!(!nav_up.get(), "ArrowDown 単独で nav_up は false のまま");
        assert!(nav_down.get(), "ArrowDown で nav_down=true");
    }

    /// focus を持たない widget は ↑↓ を Response に積まない (keyboard_events に届かない)。
    /// ↑↓ は global shortcut layer を通り抜けるが、 take_keyboard_events_if_focused が
    /// focus 持ちにのみ events を渡すので、 非 focus widget の Response は false のまま。
    #[test]
    fn arrow_up_down_not_reported_when_not_focused() {
        use std::cell::Cell;

        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        // text_input_at_focused は初回 show で auto focus するが、 別 widget に focus を移してから
        // 連続 visible で text_input は focus を奪い返さない (= 非 focus 状態を作る)。
        run_focus_frame(&mut host, &mut scene, screen, "abc");
        scene.clear();

        let other_wid = WidgetId::ROOT.child(b"other_input");
        // Frame 2: 別 widget に focus 移動
        host.frame(
            &mut (),
            &mut scene,
            screen,
            FrameInput::default(),
            |(), ui| {
                ui.set_focus(other_wid);
                let _ = ui.text_input_at_focused(
                    "ti",
                    Rect { x: 10.0, y: 10.0, w: 200.0, h: 28.0 },
                    "abc",
                    |_new| Edit::mutate(|()| {}),
                );
            },
        );
        scene.clear();

        // Frame 3: ArrowUp/Down を送る → 非 focus なので Response は false のまま
        let nav_up = Cell::new(true);
        let nav_down = Cell::new(true);
        host.frame(
            &mut (),
            &mut scene,
            screen,
            frame_with_keys(
                vec![key_pressed(PhysicalKey::ArrowUp), key_pressed(PhysicalKey::ArrowDown)],
                Modifiers::default(),
            ),
            |(), ui| {
                let resp = ui.text_input_at_focused(
                    "ti",
                    Rect { x: 10.0, y: 10.0, w: 200.0, h: 28.0 },
                    "abc",
                    |_new| Edit::mutate(|()| {}),
                );
                nav_up.set(resp.nav_up);
                nav_down.set(resp.nav_down);
            },
        );
        assert!(!nav_up.get(), "非 focus widget は nav_up を返さない");
        assert!(!nav_down.get(), "非 focus widget は nav_down を返さない");
    }

    #[test]
    fn paste_replaces_selection_with_clipboard_content() {
        // 全選択 + Ctrl+V (clipboard 内容 = "XYZ") → text = "XYZ"
        let clip_text: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(Some("XYZ".to_string())));
        let provider = MemClipboard { text: clip_text };

        let mut host: UiHost<()> = UiHost::no_redraw().with_clipboard(provider);
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        run_focus_frame(&mut host, &mut scene, screen, "abc");
        scene.clear();
        let ctrl = Modifiers { ctrl: true, ..Modifiers::empty() };
        let observed = run_input_frame(
            &mut host,
            &mut scene,
            screen,
            "abc",
            frame_with_keys(vec![key_pressed(PhysicalKey::Char('V'))], ctrl),
        );
        assert_eq!(observed.as_deref(), Some("XYZ"), "Ctrl+V で範囲を clipboard 内容で置換");
    }

    // ============================================================================
    // M15: ImeEvent::ReplaceRange (TSF まぜ書き / 再変換の書き戻し)
    // ============================================================================

    /// rtry まぜ書き相当: focus 中の selection に関係なく、IME 指定の **任意 range** を変換結果で
    /// 置換する。selection (focus で全選択) ではなく `[3..12]` を狙って置換できることを確認。
    #[test]
    fn replace_range_rewrites_arbitrary_range_for_mazegaki() {
        use crate::input::ImeEvent;

        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        // focus → 全選択 (anchor=0, cursor=12)。buffer_text = "あきしゃ" (各 3 byte)。
        run_focus_frame(&mut host, &mut scene, screen, "あきしゃ");
        scene.clear();

        // [3..12) = "きしゃ" を "汽車" に置換、cursor を末尾 (3 + 6 = 9) へ。
        let input = FrameInput {
            ime: vec![ImeEvent::ReplaceRange {
                start_byte: 3,
                end_byte: 12,
                text: "汽車".to_string(),
                new_cursor: 9,
            }],
            ..Default::default()
        };
        let observed = run_input_frame(&mut host, &mut scene, screen, "あきしゃ", input);
        assert_eq!(
            observed.as_deref(),
            Some("あ汽車"),
            "selection でない range を変換結果で置換 (まぜ書き)"
        );
    }

    /// `ReplaceRange` の境界 clamp: 範囲外 byte を渡しても panic せず末尾に丸める。
    #[test]
    fn replace_range_clamps_out_of_range() {
        use crate::input::ImeEvent;

        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        run_focus_frame(&mut host, &mut scene, screen, "abc");
        scene.clear();

        // end_byte=999, new_cursor=999 → working.len()=3 に clamp。"ab" + "Z" 置換。
        let input = FrameInput {
            ime: vec![ImeEvent::ReplaceRange {
                start_byte: 2,
                end_byte: 999,
                text: "Z".to_string(),
                new_cursor: 999,
            }],
            ..Default::default()
        };
        let observed = run_input_frame(&mut host, &mut scene, screen, "abc", input);
        assert_eq!(observed.as_deref(), Some("abZ"), "範囲外 end は末尾に clamp");
    }
}
