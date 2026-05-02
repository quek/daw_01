//! `Ui<'a, M>` — 1 フレームの間 `&'a M` を借りて UI を構築するコンテキスト。
//!
//! ユーザのアプリループ:
//! ```ignore
//! let edits = host.frame_to_edits(&model, &mut scene, &input, |m, ui| {
//!     ui.label("title", "Mixer");
//!     ui.button("mute", "Mute", || Edit::mutate(|m: &mut MixerModel| m.mute = !m.mute));
//! });
//! for e in edits { e.apply(&mut model); }
//! ```

use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::path::PathBuf;

use daw_ui_platform::{KeyEvent, PhysicalSize};
use daw_ui_renderer::{Color, GlyphArea, LineBatch, LineSegment, Rect, RectCommand, Scene};

use crate::clipboard::ClipboardProvider;
use crate::dialog::{DialogKind, DialogRequest, DialogResult, FileDialogFilter};
use crate::edit::Edit;
use crate::history::{HistoryEntry, HistoryStack};
use crate::id::WidgetId;
use crate::input::{DroppedFiles, FrameInput, ImeEvent, PointerFrame};
use crate::popup::PopupOpenState;
use crate::scenegraph::{CachedCommands, Scenegraph};
use crate::shortcut::ShortcutMap;
use crate::widgets::WidgetState;
use crate::widgets::drag_rect::{DragRect, DragRectState};

/// アプリが 1 つ持つ UI ホスト。フレーム間で UI 内部状態を保持する。
///
/// 通常は [`Self::with_window`] で構築する:
/// ```ignore
/// let window = Arc::new(winit_window);
/// let mut ui: UiHost<MyModel> = UiHost::with_window(window.clone());
/// ```
///
/// `frame()` は `&mut model` を取って **edits を内部で apply** し、Edit が出たフレーム /
/// focus が変わったフレームでは **自動で `request_redraw` を呼ぶ** ため、利用者は
/// boilerplate (`for e in edits { e.apply(...) }` や `had_edits` 判定) を書く必要が
/// 一切ない。
/// `focus_changed_in_last_frame` / `redraw_requested_in_last_frame` / M8 の
/// `transient_undo_requested` / `transient_redo_requested` がそれぞれ独立した
/// 「frame 内で 1 度だけ書かれる」フラグで意味的に正交、state machine 化のメリットなし。
#[allow(clippy::struct_excessive_bools)]
pub struct UiHost<M: ?Sized + 'static> {
    state: HashMap<WidgetId, Box<dyn WidgetState>>,
    /// キーボードフォーカスを持つウィジェット (`text_input` 等)。
    focused: Option<WidgetId>,
    /// 直前の `frame()` 呼び出しでフォーカスが変化したか。
    /// `frame()` が自動で次フレーム redraw を要求するので、利用者がこの値を query する
    /// 必要は **ない** (互換性のため公開しているだけ)。
    focus_changed_in_last_frame: bool,
    /// 直前の `frame()` で `Ui::request_ime` が呼ばれたときの cursor 領域。
    /// アプリは on_render の終わりにこの値を見て winit の `set_ime_cursor_area` /
    /// `set_ime_allowed` を呼ぶ。`None` のフレームでは IME を無効化する。
    last_ime_request: Option<Rect>,
    /// M4 Phase 10 で追加: 内部 scenegraph (per-widget input_hash の前フレーム履歴)。
    #[allow(dead_code)]
    scenegraph: Scenegraph,
    /// edits / focus 変化の検出時にライブラリが自動で呼ぶ closure。
    /// 通常は `WindowBackend::request_redraw` をラップしたもの。
    redraw_request: Box<dyn Fn() + Send + Sync>,
    /// M7 Phase 25: 現在開いている popup の集合 (menu / context_menu / dropdown 共通)。
    /// `Ui::open_popup` / `Ui::close_popup` で出し入れする。`Ui` 経由で `&mut` 借用される
    /// ため rustc から "never read" と誤判定されるが、実際には popup_layer で読まれる。
    #[allow(dead_code)]
    open_popups: HashMap<WidgetId, PopupOpenState>,
    /// M7 後の改善: widget が `Ui::request_redraw()` を呼んだ場合の累積フラグ。
    /// `frame()` の最後で `redraw_request` 呼び出し条件に含まれる。
    redraw_requested_in_last_frame: bool,
    /// M8 Phase 29: undo / redo stack。
    history: HistoryStack<M>,
    /// M8 Phase 30: shortcut 登録テーブル。
    shortcut_map: ShortcutMap,
    /// M8 Phase 31: OS clipboard provider (None なら set/get は no-op)。
    clipboard: Option<Box<dyn ClipboardProvider>>,
    /// M8 Phase 34: 前フレームに完了した dialog 結果 (次フレームで `Ui::take_dialog_result` で取り出される)。
    pending_dialog_results: HashMap<&'static str, DialogResult>,
    /// M8 Phase 30: 前フレームに登場した focusable widget の (id, rect) 一覧。
    /// Tab / arrow nav の対象決定用。
    last_focusable: Vec<(WidgetId, Rect)>,
    /// M8: `frame_to_edits` で Ui が書いた transient な request 群。`frame` の後半で読まれる。
    /// `frame_to_edits` 単独で low-level に使う場合は `take_frame_outputs()` で取り出せる。
    transient_undo_requested: bool,
    transient_redo_requested: bool,
    transient_clipboard_writes: Vec<String>,
    transient_clipboard_writes_bytes: Vec<(&'static str, Vec<u8>)>,
    transient_dialog_requests: Vec<DialogRequest>,
    transient_consumed_dialog_results: HashSet<&'static str>,
    _m: PhantomData<fn(&mut M)>,
}

impl<M: ?Sized + 'static> UiHost<M> {
    /// 再描画リクエストを呼ぶ closure を直接渡して構築する low-level constructor。
    /// 通常は [`Self::with_window`] を使う方が簡潔。
    pub fn new(redraw_request: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            state: HashMap::new(),
            focused: None,
            focus_changed_in_last_frame: false,
            last_ime_request: None,
            scenegraph: Scenegraph::new(),
            redraw_request: Box::new(redraw_request),
            open_popups: HashMap::new(),
            redraw_requested_in_last_frame: false,
            history: HistoryStack::default(),
            shortcut_map: ShortcutMap::with_default_bindings(),
            clipboard: None,
            pending_dialog_results: HashMap::new(),
            last_focusable: Vec::new(),
            transient_undo_requested: false,
            transient_redo_requested: false,
            transient_clipboard_writes: Vec::new(),
            transient_clipboard_writes_bytes: Vec::new(),
            transient_dialog_requests: Vec::new(),
            transient_consumed_dialog_results: HashSet::new(),
            _m: PhantomData,
        }
    }

    /// M8 Phase 29: history stack の容量を変更 (default 100 step、0 で無効化)。
    #[must_use]
    pub fn with_history_capacity(mut self, n: usize) -> Self {
        self.history.set_capacity(n);
        self
    }

    /// M8 Phase 30: shortcut map を完全に置き換える (preference の serialize/deserialize 用)。
    #[must_use]
    pub fn with_shortcut_map(mut self, map: ShortcutMap) -> Self {
        self.shortcut_map = map;
        self
    }

    /// M8 Phase 31: OS clipboard provider を設定。
    /// winit backend では `daw_ui_platform::ArboardClipboard::new()` を渡す
    /// (feature `clipboard` 有効時)。test では未設定のままで Ui 側 set/get は no-op になる。
    #[must_use]
    pub fn with_clipboard<C: ClipboardProvider + 'static>(mut self, provider: C) -> Self {
        self.clipboard = Some(Box::new(provider));
        self
    }

    /// M8 Phase 29: history stack への参照 (`undo_label` / `can_undo` 等の query 用)。
    pub fn history(&self) -> &HistoryStack<M> {
        &self.history
    }

    /// M8 Phase 29: history stack への mutable 参照 (project 切替時に clear 等)。
    pub fn history_mut(&mut self) -> &mut HistoryStack<M> {
        &mut self.history
    }

    /// M8 Phase 30: shortcut map への参照 (preference の serialize 用)。
    pub fn shortcut_map(&self) -> &ShortcutMap {
        &self.shortcut_map
    }

    /// M8 Phase 30: shortcut map への mutable 参照 (実行時 rebind 用)。
    pub fn shortcut_map_mut(&mut self) -> &mut ShortcutMap {
        &mut self.shortcut_map
    }

    /// M8 Phase 31: clipboard provider が設定されていれば true。
    pub fn clipboard_available(&self) -> bool {
        self.clipboard.is_some()
    }

    /// `WindowBackend` を持つ window から、自動的に `request_redraw` を呼ぶ UiHost を
    /// 構築する。**通常の example はこれを使う**:
    /// ```ignore
    /// let window = Arc::new(winit_window);
    /// let ui = UiHost::with_window(window.clone());
    /// ```
    pub fn with_window<W>(window: std::sync::Arc<W>) -> Self
    where
        W: daw_ui_platform::WindowBackend + Send + Sync + 'static,
    {
        Self::new(move || window.request_redraw())
    }

    /// テスト / offscreen render 用、`request_redraw` を呼ばない UiHost を構築する。
    pub fn no_redraw() -> Self {
        Self::new(|| {})
    }

    /// 直前の `frame()` 呼び出しでフォーカスが変化したか。
    /// `frame()` が自動で `redraw_request` を呼ぶので、利用者がこの値を query して
    /// 再描画する必要は **ない** (互換性のため公開しているだけ)。
    pub fn focus_changed_in_last_frame(&self) -> bool {
        self.focus_changed_in_last_frame
    }

    /// 直前の `frame()` で focused widget が要求した IME 候補ウィンドウ位置 (Rect)。
    /// `Some` ならアプリは `WindowBackend::set_ime_allowed(true)` +
    /// `set_ime_cursor_area(rect)` を呼ぶ。`None` なら IME を無効化する。
    pub fn ime_request(&self) -> Option<Rect> {
        self.last_ime_request
    }

    /// 現在キーボードフォーカスを持つ widget の ID。
    pub fn focused_widget(&self) -> Option<WidgetId> {
        self.focused
    }

    /// 1 フレーム分の UI を構築。**edits は内部で apply され、`request_redraw` も
    /// 自動で呼ばれる**。利用者の boilerplate はゼロ。
    ///
    /// `f` は `(&model, &mut Ui)` を受け取り、ウィジェットを呼び出して UI を組む。
    /// 内部動作:
    /// 1. scene を積みつつ edits を収集 (build クロージャは古い model 値で 1 度だけ実行)
    /// 2. **M8**: undo/redo 要求があれば `HistoryStack` に対して実行
    /// 3. 収集した edits を `&mut model` に apply (`Undoable` は forward + history.push)
    /// 4. **M8**: clipboard write / file dialog 同期実行 / dialog 結果クリーンアップ
    /// 5. edits / undo / redo / focus 変化があった場合は `redraw_request` を呼ぶ
    ///    → 次フレームで apply 後の値で再描画される (immediate-mode + Edit queue の必然対処)
    pub fn frame<F>(
        &mut self,
        model: &mut M,
        scene: &mut Scene,
        screen: PhysicalSize,
        input: FrameInput,
        f: F,
    ) where
        F: for<'a> FnOnce(&'a M, &mut Ui<'a, M>),
    {
        let edits = self.frame_to_edits(&*model, scene, screen, input, f);
        let had_edits = !edits.is_empty();

        // M8 Phase 29: undo / redo (edits apply の前に行う = 「undo を要求したフレームでは
        // 同フレームの edits は通常通り反映、その後の undo step で巻き戻す」のは紛らわしい
        // ので、**undo → edits apply** の順)。
        // つまり「Undo は前フレームまでに積まれた entry を巻き戻し、新規 edits は前進的に積む」。
        let undo_req = self.transient_undo_requested;
        let redo_req = self.transient_redo_requested;
        if undo_req {
            let _ = self.history.undo(model);
        }
        if redo_req {
            let _ = self.history.redo(model);
        }

        // edits apply。Undoable は forward を実行 + (forward, inverse, label) を history へ push。
        for e in edits {
            match e {
                Edit::Mutate(f) => f(model),
                Edit::Undoable { forward, inverse, label } => {
                    forward(model);
                    self.history
                        .push(HistoryEntry::new(forward, inverse, label));
                }
            }
        }

        // M8 Phase 31: clipboard write (frame 末尾で provider に書き込み)。
        if let Some(c) = self.clipboard.as_mut() {
            for s in self.transient_clipboard_writes.drain(..) {
                c.set_text(s);
            }
            for (mime, bytes) in self.transient_clipboard_writes_bytes.drain(..) {
                c.set_bytes(mime, bytes);
            }
        } else {
            // provider 無しなら捨てる
            self.transient_clipboard_writes.clear();
            self.transient_clipboard_writes_bytes.clear();
        }

        // M8 Phase 34: file dialog 同期実行。結果は次フレームで `take_dialog_result` から取り出される。
        let dialog_runs = !self.transient_dialog_requests.is_empty();
        let requests = std::mem::take(&mut self.transient_dialog_requests);
        for req in requests {
            let result = run_dialog_sync(&req);
            self.pending_dialog_results.insert(req.name, result);
        }

        // 消費済 dialog 結果は pending_dialog_results から削除。
        for name in self.transient_consumed_dialog_results.drain() {
            self.pending_dialog_results.remove(name);
        }

        // 自動 redraw の発火条件: edits / undo / redo / focus 変化 / widget からの request_redraw
        // / dialog 実行 (新結果が出た) / 残っている dialog 結果 (widget が次フレームで取り出す)。
        if had_edits
            || undo_req
            || redo_req
            || self.focus_changed_in_last_frame
            || self.redraw_requested_in_last_frame
            || dialog_runs
            || !self.pending_dialog_results.is_empty()
        {
            (self.redraw_request)();
        }
    }

    /// (Advanced) edits を返す low-level API。
    /// `Edit<M>` の apply タイミングを自前で制御したい場合 (audio thread に送る、
    /// batch apply、undo stack 等) のみ使用する。通常は [`Self::frame`] を使うこと。
    ///
    /// 注: この API では **自動 `request_redraw` は呼ばれない**。利用者が edits 検出時に
    /// 手動で `WindowBackend::request_redraw` を呼ぶ責任を負う。
    /// undo/redo / clipboard write / dialog 同期実行など `Edit` 以外の副作用は `UiHost` の
    /// transient フィールドに格納され、`take_frame_outputs()` で取り出すか、`frame()` で
    /// 自動処理される。
    #[allow(clippy::too_many_lines)]
    pub fn frame_to_edits<F>(
        &mut self,
        model: &M,
        scene: &mut Scene,
        screen: PhysicalSize,
        input: FrameInput,
        f: F,
    ) -> Vec<Edit<M>>
    where
        F: for<'a> FnOnce(&'a M, &mut Ui<'a, M>),
    {
        let mut edits: Vec<Edit<M>> = Vec::new();
        let FrameInput { pointer, keyboard, ime, file_drop, file_hover } = input;
        let mut keyboard_events = keyboard;
        let mut ime_events = ime;

        // M8 Phase 30: shortcut layer (frame 頭)。keyboard_events を `shortcut_map.matches` で
        // 走査、マッチした events を取り除いて name を `pending_shortcuts` に積む。
        // text_input が後で `take_keyboard_events_if_focused` で取るのは shortcut 後の残り。
        let modifiers = pointer.modifiers;
        let mut pending_shortcuts: Vec<&'static str> = Vec::new();
        keyboard_events.retain(|ev| {
            if let Some(name) = self.shortcut_map.matches(ev, modifiers) {
                pending_shortcuts.push(name);
                false
            } else {
                true
            }
        });

        // M8 Phase 31: clipboard paste — paste shortcut がマッチしていれば provider から read。
        // 1 フレーム内で paste と他の shortcut が同時に発生しても、paste の取り出しは 1 度限り。
        let pending_clipboard_paste: Option<String> = if pending_shortcuts.contains(&"paste") {
            self.clipboard.as_mut().and_then(|c| c.get_text())
        } else {
            None
        };

        // M8 Phase 29: history の現状をスナップショットして Ui に渡す (`can_undo` 等の query 用)。
        let history_can_undo = self.history.can_undo();
        let history_can_redo = self.history.can_redo();
        let history_undo_label = self.history.undo_label();
        let history_redo_label = self.history.redo_label();

        // transient outputs (`frame()` 後半で読まれる) を frame 頭でクリア。
        self.transient_undo_requested = false;
        self.transient_redo_requested = false;
        self.transient_clipboard_writes.clear();
        self.transient_clipboard_writes_bytes.clear();
        self.transient_dialog_requests.clear();
        self.transient_consumed_dialog_results.clear();

        let cursor = Rect::new(0.0, 0.0, screen.width as f32, screen.height as f32);
        let focused_at_start = self.focused;
        let mut seen_widgets: HashSet<WidgetId> = HashSet::new();
        let mut redraw_requested = false;
        let mut typing_focus = false;
        let mut focus_order: Vec<(WidgetId, Rect)> = Vec::new();

        let mut ui = Ui {
            state: &mut self.state,
            scene,
            edits: &mut edits,
            pointer,
            keyboard_events: &mut keyboard_events,
            ime_events: &mut ime_events,
            cursor,
            screen,
            next_y: 0.0,
            focused: focused_at_start,
            pending_focus: focused_at_start,
            focus_changed_this_frame: false,
            ime_request: None,
            scenegraph: &mut self.scenegraph,
            seen_widgets: &mut seen_widgets,
            current_clip: None,
            open_popups: &mut self.open_popups,
            popup_rects: Vec::new(),
            popup_glyphs: Vec::new(),
            popup_lines: Vec::new(),
            drawing_in_popup: false,
            redraw_requested: &mut redraw_requested,
            file_drop,
            file_hover,
            pending_shortcuts: &mut pending_shortcuts,
            typing_focus: &mut typing_focus,
            shortcut_map: &self.shortcut_map,
            pending_undo: &mut self.transient_undo_requested,
            pending_redo: &mut self.transient_redo_requested,
            history_can_undo,
            history_can_redo,
            history_undo_label,
            history_redo_label,
            pending_clipboard_paste,
            pending_clipboard_writes: &mut self.transient_clipboard_writes,
            pending_clipboard_paste_bytes: HashMap::new(),
            pending_clipboard_writes_bytes: &mut self.transient_clipboard_writes_bytes,
            pending_dialog_requests: &mut self.transient_dialog_requests,
            consumed_dialog_results: &mut self.transient_consumed_dialog_results,
            dialog_results: &self.pending_dialog_results,
            focus_order: &mut focus_order,
            _m: PhantomData,
        };
        f(model, &mut ui);

        // M8 Phase 30: Tab / arrow focus traversal。
        // pending_shortcuts に "tab_next" 等が残っていれば (= widget が consume 済でなければ)、
        // focus_order (このフレームに登録された focusable 一覧) から次の wid を選んで set_focus。
        let focus_order_snapshot: Vec<(WidgetId, Rect)> = ui.focus_order.clone();
        if !focus_order_snapshot.is_empty() {
            let tab_next = ui.take_shortcut("tab_next");
            let tab_prev = ui.take_shortcut("tab_prev");
            let focus_up = ui.take_shortcut("focus_up");
            let focus_down = ui.take_shortcut("focus_down");
            let focus_left = ui.take_shortcut("focus_left");
            let focus_right = ui.take_shortcut("focus_right");

            let current = ui.focused;
            let next_wid: Option<WidgetId> = if tab_next || tab_prev {
                tab_navigate(&focus_order_snapshot, current, tab_next)
            } else if focus_up {
                arrow_navigate(&focus_order_snapshot, current, FocusDirection::Up)
            } else if focus_down {
                arrow_navigate(&focus_order_snapshot, current, FocusDirection::Down)
            } else if focus_left {
                arrow_navigate(&focus_order_snapshot, current, FocusDirection::Left)
            } else if focus_right {
                arrow_navigate(&focus_order_snapshot, current, FocusDirection::Right)
            } else {
                None
            };
            if let Some(wid) = next_wid {
                ui.set_focus(wid);
            }
        }

        // M7 Phase 25: popup の deferred buffer を取り出して、ui の borrow が外れたあと
        // base scene 末尾に append (z-order = 最前面) する。
        let popup_rects = std::mem::take(&mut ui.popup_rects);
        let popup_glyphs = std::mem::take(&mut ui.popup_glyphs);
        let popup_lines = std::mem::take(&mut ui.popup_lines);
        // フォーカスの commit:
        // - 誰かが set_focus / clear_focus を呼んでいたら pending_focus がそのまま反映。
        // - そうでないとき、このフレームでクリック (release) があったら blur 扱いで
        //   focused = None にする (= フォーカス可能でない場所がクリックされた)。
        let prev_focused = self.focused;
        if ui.focus_changed_this_frame {
            self.focused = ui.pending_focus;
        } else if pointer.primary_just_released && self.focused.is_some() {
            self.focused = None;
        }
        self.focus_changed_in_last_frame = self.focused != prev_focused;
        // IME request の commit (フレーム内に request_ime が呼ばれていれば Some)。
        self.last_ime_request = ui.ime_request;
        drop(ui);
        // widget からの request_redraw 累積を commit (ui drop 後に local 変数を読む)。
        self.redraw_requested_in_last_frame = redraw_requested;
        // ui が drop して scene の borrow が外れた後で、popup buffer を Scene の popup pass 用
        // フィールドに移す (renderer は base pass の後に popup pass で再描画する設計、
        // pipeline 順 rect→line→glyph 起因の z-order 問題を解消)。
        scene.popup_rects.extend(popup_rects);
        scene.popup_glyph_areas.extend(popup_glyphs);
        scene.popup_line_batches.extend(popup_lines);
        // M4 Phase 11: 今フレームに登場しなかった widget を scenegraph から eviction。
        self.scenegraph.retain(&seen_widgets);
        // M8 Phase 30: 次フレーム用に focusable 一覧を保存。
        self.last_focusable = focus_order_snapshot;
        edits
    }
}

impl<M: ?Sized + 'static> Default for UiHost<M> {
    fn default() -> Self {
        Self::no_redraw()
    }
}

/// 1 フレーム内のみ生きる UI コンテキスト。
///
/// `'a` は `&'a M` 借用と同じ寿命。`Edit<M>` は `'static` (M1) なので Ui のライフタイムから
/// 切り離せる。
///
/// M8 で transient bool 群 (typing_focus / pending_undo / pending_redo / history_can_undo /
/// history_can_redo + 既存の focus_changed_this_frame / drawing_in_popup) が増えたが、
/// それぞれが「frame 内で 1 度だけ書かれる / 読まれる」フラグで意味が独立しているため、
/// `clippy::struct_excessive_bools` を allow する (state machine 化はオーバーヘッド過大)。
#[allow(clippy::struct_excessive_bools)]
pub struct Ui<'a, M: ?Sized + 'static> {
    state: &'a mut HashMap<WidgetId, Box<dyn WidgetState>>,
    scene: &'a mut Scene,
    edits: &'a mut Vec<Edit<M>>,
    pub(crate) pointer: PointerFrame,
    /// このフレーム分のキー入力イベント (フォーカスを持つ widget が消費する)。
    keyboard_events: &'a mut Vec<KeyEvent>,
    /// このフレーム分の IME イベント (フォーカスを持つ widget が消費する)。
    ime_events: &'a mut Vec<ImeEvent>,
    /// 現在の利用可能領域 (シンプルな vstack 用)。
    pub(crate) cursor: Rect,
    pub(crate) screen: PhysicalSize,
    /// vstack 内で次に積むウィジェットの y 位置。
    pub(crate) next_y: f32,
    /// このフレーム開始時点でキーボードフォーカスを持つ widget。
    focused: Option<WidgetId>,
    /// このフレーム内で widget が `set_focus` / `clear_focus` を呼んだ結果。
    /// frame 終了時に `UiHost::focused` に commit される。
    pending_focus: Option<WidgetId>,
    /// このフレーム内で誰かがフォーカスを操作したか。
    /// クリック発生時にこれが `false` なら blur (= フォーカス可能でない場所がクリックされた)。
    focus_changed_this_frame: bool,
    /// このフレーム内で `Ui::request_ime` で要求された IME 候補ウィンドウ位置 (Rect)。
    /// 同フレーム内に複数 widget が呼んだ場合は最後の呼び出しが勝つ
    /// (typical: focused widget だけが呼ぶ想定)。
    ime_request: Option<Rect>,
    /// M4 Phase 11: per-widget の描画コマンドキャッシュ (UiHost が所有、frame 越しに保持)。
    scenegraph: &'a mut Scenegraph,
    /// M4 Phase 11: このフレームで `with_widget_node` 経由で描画された widget の集合。
    /// frame 末尾で `scenegraph.retain(&seen_widgets)` を呼んで未登場 widget を eviction。
    seen_widgets: &'a mut HashSet<WidgetId>,
    /// M7 Phase 22: 現在のクリップ矩形 (`with_clip_rect` でスタック管理)。
    /// `push_rect / push_text / push_lines` で自動 inject される。`None` = 全画面。
    pub(crate) current_clip: Option<Rect>,
    /// M7 Phase 25: popup の open / close 状態 (UiHost が所有、Ui が借りる)。
    open_popups: &'a mut HashMap<WidgetId, PopupOpenState>,
    /// M7 Phase 25: popup_layer 内で push される rect の deferred buffer。
    /// frame 末尾で base scene に append → z-order 最前面。
    popup_rects: Vec<RectCommand>,
    popup_glyphs: Vec<GlyphArea>,
    popup_lines: Vec<LineBatch>,
    /// `popup_layer` 内で描画中フラグ。push_* が popup_buffer に積むかを切替える。
    drawing_in_popup: bool,
    /// M7 後の改善: widget が `Ui::request_redraw()` を呼んだら true。
    /// `UiHost::frame` の末尾で `redraw_requested_in_last_frame` に commit され、
    /// 次のフレーム redraw の発火条件に含まれる。
    redraw_requested: &'a mut bool,
    /// M8 Phase 32: このフレームに OS から drop された file 群。
    /// `Ui::take_file_drop_in_rect(rect)` で widget が consume すると None に書き換わる。
    pub(crate) file_drop: Option<DroppedFiles>,
    /// M8 Phase 32: 現在 hover 中の file 一覧 (read-only、`is_file_hovering_in_rect` で参照)。
    pub(crate) file_hover: Option<Vec<PathBuf>>,
    // ---- M8 Phase 30 shortcut ----
    /// frame 頭で `shortcut_map.matches` した name 一覧。`take_shortcut(name)` で 1 度だけ消費。
    pub(crate) pending_shortcuts: &'a mut Vec<&'static str>,
    /// text_input 等が focus 中に呼ぶ。修飾なし shortcut の判定で使う (現状は記録のみ、
    /// shortcut layer 自体は frame 頭で済ませる KISS 設計)。
    pub(crate) typing_focus: &'a mut bool,
    /// shortcut display_for などの query 用 (mutable は UiHost::shortcut_map_mut 経由)。
    pub(crate) shortcut_map: &'a ShortcutMap,
    // ---- M8 Phase 29 history ----
    pub(crate) pending_undo: &'a mut bool,
    pub(crate) pending_redo: &'a mut bool,
    pub(crate) history_can_undo: bool,
    pub(crate) history_can_redo: bool,
    pub(crate) history_undo_label: Option<&'static str>,
    pub(crate) history_redo_label: Option<&'static str>,
    // ---- M8 Phase 31 clipboard ----
    /// frame 頭に paste shortcut が match していれば provider から read 済み。`take_clipboard_paste`
    /// で widget が 1 度だけ取り出す。
    pub(crate) pending_clipboard_paste: Option<String>,
    /// `set_clipboard_text` で積まれる write リクエスト (frame 末尾に provider.set_text)。
    pub(crate) pending_clipboard_writes: &'a mut Vec<String>,
    /// MIME bytes の paste/write (M8 では skeleton 経由のみ、provider default は no-op)。
    pub(crate) pending_clipboard_paste_bytes: HashMap<&'static str, Vec<u8>>,
    pub(crate) pending_clipboard_writes_bytes: &'a mut Vec<(&'static str, Vec<u8>)>,
    // ---- M8 Phase 34 dialog ----
    pub(crate) pending_dialog_requests: &'a mut Vec<DialogRequest>,
    pub(crate) consumed_dialog_results: &'a mut HashSet<&'static str>,
    pub(crate) dialog_results: &'a HashMap<&'static str, DialogResult>,
    // ---- M8 Phase 30 focus traversal ----
    /// このフレームに `Ui::focusable(wid, rect)` で登録された一覧。
    /// frame 末尾で UiHost に保存し、Tab / arrow nav の対象にする。
    pub(crate) focus_order: &'a mut Vec<(WidgetId, Rect)>,
    _m: PhantomData<&'a M>,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    pub fn screen(&self) -> PhysicalSize {
        self.screen
    }

    pub fn pointer(&self) -> PointerFrame {
        self.pointer
    }

    /// 描画コマンドを Scene に積む (外部 widget extension で利用可能)。
    /// M7 Phase 22: `current_clip` (with_clip_rect スタック) と cmd 自身の clip_rect を交差させて
    /// renderer に渡す (cmd 自身が `Some` の場合は intersect、`None` の場合は current_clip)。
    /// M7 Phase 25: `drawing_in_popup` 中は popup_buffer に積む (frame 末尾で z-order 最前面)。
    pub fn push_rect(&mut self, mut cmd: RectCommand) {
        cmd.clip_rect = merge_clip(self.current_clip, cmd.clip_rect);
        if self.drawing_in_popup {
            self.popup_rects.push(cmd);
        } else {
            self.scene.push_rect(cmd);
        }
    }

    /// テキスト描画を Scene に積む (外部 widget extension で利用可能)。
    pub fn push_text(&mut self, mut area: GlyphArea) {
        area.clip_rect = merge_clip(self.current_clip, area.clip_rect);
        if self.drawing_in_popup {
            self.popup_glyphs.push(area);
        } else {
            self.scene.push_text(area);
        }
    }

    /// 線分バッチを Scene に積む (波形・メータ・グリッド、外部 widget extension で利用可能)。
    pub fn push_lines(&mut self, mut batch: LineBatch) {
        batch.clip_rect = merge_clip(self.current_clip, batch.clip_rect);
        if self.drawing_in_popup {
            self.popup_lines.push(batch);
        } else {
            self.scene.push_lines(batch);
        }
    }

    /// M7 Phase 22: スコープ内の描画を `rect` でクリップする。
    /// nested 呼び出しでは外側 clip と intersect。`scroll_area / popup_layer / split_view` で使用。
    /// `with_widget_node` の input_hash には自動で current_clip が混ざる (キャッシュ整合性)。
    pub fn with_clip_rect<F>(&mut self, rect: Rect, f: F)
    where
        F: FnOnce(&mut Self),
    {
        let prev = self.current_clip;
        self.current_clip = Some(merge_clip(prev, Some(rect)).unwrap_or(rect));
        f(self);
        self.current_clip = prev;
    }

    // ============================================================
    // M7 Phase 25: popup 制御
    // ============================================================

    /// popup を開く。次以降のフレームで `popup_layer(id, ..)` の closure が実行される。
    /// `anchor` は popup を開く起点の矩形 (例: menu_bar の "File" ボタン)。
    /// `modal` が true なら他 widget の click を抑制 (popup_layer の outside-click 検出で消費)。
    pub fn open_popup(&mut self, id: impl std::hash::Hash, anchor: Rect, modal: bool) {
        let wid = WidgetId::ROOT.child((b"popup", &id));
        let prev_focus = self.pending_focus;
        self.open_popups.insert(
            wid,
            PopupOpenState { anchor, modal, prev_focus },
        );
    }

    /// popup を閉じる。popup を開く前の focus を復元する。
    pub fn close_popup(&mut self, id: impl std::hash::Hash) {
        let wid = WidgetId::ROOT.child((b"popup", &id));
        if let Some(state) = self.open_popups.remove(&wid) {
            self.pending_focus = state.prev_focus;
            self.focus_changed_this_frame = true;
        }
    }

    /// popup が現在開いているか。
    pub fn is_popup_open(&self, id: impl std::hash::Hash) -> bool {
        let wid = WidgetId::ROOT.child((b"popup", &id));
        self.open_popups.contains_key(&wid)
    }

    /// `id` で開いている popup の anchor (= popup の内容領域) を返す。
    /// closure 内で popup_rect を再計算する代わりに使える (例: context_menu の動的位置を
    /// open_popup 時の pointer 位置に固定したい場合)。
    pub fn popup_anchor(&self, id: impl std::hash::Hash) -> Option<Rect> {
        let wid = WidgetId::ROOT.child((b"popup", &id));
        self.open_popups.get(&wid).map(|s| s.anchor)
    }

    /// popup の内容を描画する。popup が開いていなければ closure は呼ばれない。
    /// closure 内で push される primitive は **deferred buffer** に積まれ、frame 末尾で
    /// base scene に append (z-order = 最前面)。
    ///
    /// modal popup の click consumption ルール:
    /// - `anchor` の **外** で `primary_just_pressed` → popup close + click 消費 (closure 実行せず)
    /// - `anchor` の **内** で click → closure 内 widget で処理、popup_layer 出口で click 消費
    ///   (popup の下にある widget には click が流れない)
    ///
    /// **重要**: `anchor` は「popup として扱う rect 全体」を指す (popup を開いた起点 button
    /// だけでなく、items の rect も含めること)。さもなくば popup item 上の click が
    /// outside_click 扱いで close されてしまう。menu_bar / dropdown / context_menu_for は
    /// 内部で popup_rect を anchor として渡している。
    pub fn popup_layer<F>(&mut self, id: impl std::hash::Hash, f: F)
    where
        F: FnOnce(&mut Self),
    {
        let wid = WidgetId::ROOT.child((b"popup", &id));
        let Some(state) = self.open_popups.get(&wid).copied() else {
            return;
        };

        // outside-click 検出 (closure 実行前に判定 / 自動 close)
        let outside_click = self.pointer.primary_just_pressed
            && self
                .pointer
                .pos
                .is_some_and(|(px, py)| !state.anchor.contains(px, py));
        if outside_click {
            // popup を閉じる + クリック消費 (modal なら他 widget に流さない)
            self.open_popups.remove(&wid);
            self.pending_focus = state.prev_focus;
            self.focus_changed_this_frame = true;
            if state.modal {
                self.consume_pointer_click();
            }
            return;
        }

        // popup の内容を描画 (deferred buffer)
        let prev_in_popup = self.drawing_in_popup;
        self.drawing_in_popup = true;
        f(self);
        self.drawing_in_popup = prev_in_popup;

        // modal popup が open しているフレーム中、anchor 内 click は popup item として
        // 既に処理済 → 下層の widget に同じ click が流れないよう消費する。
        // (popup item handler が close_popup を呼んだ場合も same frame で消費)
        if state.modal
            && self
                .pointer
                .pos
                .is_some_and(|(px, py)| state.anchor.contains(px, py))
        {
            self.consume_pointer_click();
        }
    }

    /// エディットを Scene に積む (外部 widget extension で利用可能)。
    pub fn push_edit(&mut self, edit: Edit<M>) {
        self.edits.push(edit);
    }

    /// M4 Phase 11: per-widget の描画を input_hash でキャッシュ付き実行する。
    ///
    /// `input_hash` が前フレームと一致 → `draw_fn` を実行せず、前フレームに記録した
    /// 描画コマンドを scene へ append する。不一致 → `draw_fn` を実行して、scene 末尾の
    /// 差分を新規 commands として記録する。
    ///
    /// `draw_fn` 内では `push_rect / push_text / push_lines` のみ呼ぶこと
    /// (state 更新・Edit 発行は外側で完結させる)。
    pub fn with_widget_node<F>(&mut self, wid: WidgetId, input_hash: u64, draw_fn: F)
    where
        F: FnOnce(&mut Self),
    {
        // M7 Phase 22: current_clip も hash に混ぜる (scroll でクリップが動いたら cache 無効)。
        let input_hash = mix_clip_into_hash(input_hash, self.current_clip);

        // フレーム末尾の eviction で「今フレームに登場した widget」として保持する。
        self.seen_widgets.insert(wid);

        // hash 一致 → cached commands を scene に append、draw_fn は実行しない。
        if let Some(cached) = self.scenegraph.get_cached(wid, input_hash) {
            self.scene.rects.extend_from_slice(&cached.rects);
            self.scene.glyph_areas.extend(cached.glyph_areas.iter().cloned());
            self.scene.line_batches.extend(cached.line_batches.iter().cloned());
            return;
        }

        // miss → draw_fn を実行して scene 末尾の差分を新規 commands として記録。
        let r0 = self.scene.rects.len();
        let g0 = self.scene.glyph_areas.len();
        let l0 = self.scene.line_batches.len();
        draw_fn(self);
        let commands = CachedCommands {
            rects: self.scene.rects[r0..].to_vec(),
            glyph_areas: self.scene.glyph_areas[g0..].to_vec(),
            line_batches: self.scene.line_batches[l0..].to_vec(),
        };
        self.scenegraph.record(wid, input_hash, commands);
    }

    /// `wid` が現在キーボードフォーカスを持っているか。
    ///
    /// 同フレーム内で `set_focus` が呼ばれた場合の効果も反映する (= 描画用に
    /// 「クリックと同時にフォーカス枠を出す」ような遅延の無い見た目を作る)。
    pub fn is_focused(&self, wid: WidgetId) -> bool {
        self.pending_focus == Some(wid)
    }

    /// `wid` をキーボードフォーカスに設定する。
    /// `is_focused` には即時反映され、次フレーム以降のキー入力配信もこの widget に向く。
    pub fn set_focus(&mut self, wid: WidgetId) {
        self.pending_focus = Some(wid);
        self.focus_changed_this_frame = true;
    }

    /// 自身がフォーカスを持っているならクリアする。
    /// 持っていないときは no-op (他 widget のフォーカスを誤って消さない)。
    pub fn clear_focus_if_focused(&mut self, wid: WidgetId) {
        if self.pending_focus == Some(wid) {
            self.pending_focus = None;
            self.focus_changed_this_frame = true;
        }
    }

    /// 現在のフレームの primary click (左ボタン press / release transition) を消費する。
    /// popup / menu / dropdown 等が click を捌いた後、下層 widget に同じ click が流れないようにする。
    /// `pointer.primary_pressed` (現在押下中フラグ) は変えない (drag 中の継続は他 widget でも見える)。
    pub fn consume_pointer_click(&mut self) {
        self.pointer.primary_just_pressed = false;
        self.pointer.primary_just_released = false;
    }

    /// 次フレームの再描画を要求する (widget が「state が変化した」「アニメーション継続中」等で呼ぶ)。
    /// `Edit` や focus 変化と独立に redraw を起こせる。例:
    /// - `level_meter`: peak_hold が減衰中 / peak がアニメ中
    /// - `tab_view`: state.selected が前フレームから変わった
    /// - drag 中の split_view handle (drag delta は pointer event で来るのでこちらは redundant)
    ///
    /// `UiHost::frame` の末尾で `redraw_request` (= `WindowBackend::request_redraw`) を呼ぶ。
    /// 1 フレーム内で複数回呼ばれても 1 度の redraw 要求として扱う (累積 OR)。
    pub fn request_redraw(&mut self) {
        *self.redraw_requested = true;
    }

    // ============================================================
    // M8 Phase 29: history (undo / redo)
    // ============================================================

    /// frame 末尾で UiHost に「undo してください」と要求 (実体は次の `UiHost::frame` 末尾で実行)。
    /// 1 フレーム内に複数回呼ばれても 1 度として扱う (idempotent)。
    pub fn request_undo(&mut self) {
        *self.pending_undo = true;
    }

    /// frame 末尾で UiHost に「redo してください」と要求。
    pub fn request_redo(&mut self) {
        *self.pending_redo = true;
    }

    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.history_can_undo
    }

    #[must_use]
    pub fn can_redo(&self) -> bool {
        self.history_can_redo
    }

    /// menu の "Undo (fader change)" 表記用のラベル取得。`None` なら undo stack が空。
    #[must_use]
    pub fn undo_label(&self) -> Option<&'static str> {
        self.history_undo_label
    }

    #[must_use]
    pub fn redo_label(&self) -> Option<&'static str> {
        self.history_redo_label
    }

    // ============================================================
    // M8 Phase 30: shortcut + focus traversal + focus ring
    // ============================================================

    /// このフレームに `name` で登録した shortcut が triggered されていれば true (consume)。
    /// 同 name で 2 度目に呼ぶと false (= 1 度限り消費)。
    pub fn take_shortcut(&mut self, name: &'static str) -> bool {
        if let Some(idx) = self.pending_shortcuts.iter().position(|n| *n == name) {
            self.pending_shortcuts.remove(idx);
            true
        } else {
            false
        }
    }

    /// このフレームに該当 shortcut が triggered されているか (consume せず読み取りのみ)。
    #[must_use]
    pub fn has_shortcut(&self, name: &'static str) -> bool {
        self.pending_shortcuts.contains(&name)
    }

    /// text_input 等の typing widget が focus 中に呼ぶ。修飾なし shortcut (Space 等) を
    /// 抑制する目的だが、現状の実装は shortcut layer が frame 頭で済ませているため、
    /// このフラグはまだ参照されていない (M9 で typing_focus を見て修飾なし shortcut を
    /// 後から restore する path を追加予定)。
    pub fn set_typing_focus(&mut self, typing: bool) {
        *self.typing_focus = typing;
    }

    /// 自身を Tab / arrow focus traversal の対象として登録する。
    /// 登場順で Tab next / Shift+Tab prev、arrow は方向別の最近傍移動。
    pub fn focusable(&mut self, wid: WidgetId, rect: Rect) {
        self.focus_order.push((wid, rect));
    }

    /// 現在 focus を持っているなら 1px の青系 ring を `rect` 周囲に描画する。
    /// `wid` を渡し、self が focused かを内部で判定する想定だが、`is_focused` を呼んで
    /// 利用者側で判定するスタイルでも動く (描画のみ行う、判定はしない)。
    pub fn draw_focus_ring(&mut self, rect: Rect) {
        let color = Color::rgb(0.55, 0.78, 0.95);
        let segments = vec![
            LineSegment { a: [rect.x, rect.y], b: [rect.x + rect.w, rect.y], color },
            LineSegment {
                a: [rect.x + rect.w, rect.y],
                b: [rect.x + rect.w, rect.y + rect.h],
                color,
            },
            LineSegment {
                a: [rect.x + rect.w, rect.y + rect.h],
                b: [rect.x, rect.y + rect.h],
                color,
            },
            LineSegment { a: [rect.x, rect.y + rect.h], b: [rect.x, rect.y], color },
        ];
        self.push_lines(LineBatch { segments, line_width_px: 1.0, clip_rect: None });
    }

    /// `name` に登録された shortcut を表記文字列で返す ("Ctrl+Z" 等)。menu 右端の表示用。
    #[must_use]
    pub fn shortcut_for(&self, name: &'static str) -> Option<String> {
        self.shortcut_map.display_for(name)
    }

    // ============================================================
    // M8 Phase 31: clipboard
    // ============================================================

    /// このフレームに paste shortcut (Ctrl+V) が triggered されていれば OS clipboard から
    /// 読み出した text を返す。同フレームに 2 度目の呼び出しは None。
    /// clipboard provider 未設定時 / clipboard 操作失敗時は None。
    pub fn take_clipboard_paste(&mut self) -> Option<String> {
        self.pending_clipboard_paste.take()
    }

    /// 任意の文字列を OS clipboard に書き込む (frame 末尾で provider.set_text)。
    /// 同 frame 内で複数回呼ぶと最後勝ち (= 直前の write は捨てられる)。
    pub fn set_clipboard_text(&mut self, s: String) {
        // 最後勝ちの semantics を維持するため、既存を clear して push。
        self.pending_clipboard_writes.clear();
        self.pending_clipboard_writes.push(s);
    }

    /// MIME bytes の paste を取り出す (M8 では skeleton、provider default は no-op)。
    pub fn take_clipboard_paste_bytes(&mut self, mime: &'static str) -> Option<Vec<u8>> {
        self.pending_clipboard_paste_bytes.remove(mime)
    }

    /// MIME bytes を clipboard に書き込む (M8 では skeleton)。
    pub fn set_clipboard_bytes(&mut self, mime: &'static str, bytes: Vec<u8>) {
        self.pending_clipboard_writes_bytes.push((mime, bytes));
    }

    // ============================================================
    // M8 Phase 32: file drop
    // ============================================================

    /// `rect` 内に file がドロップされていれば paths を 1 度だけ取り出す。
    /// 同 frame 内で複数 widget が呼んでも先勝ち。
    pub fn take_file_drop_in_rect(&mut self, rect: Rect) -> Option<Vec<PathBuf>> {
        let drop_pos = self.file_drop.as_ref()?.position;
        if !rect.contains(drop_pos.0, drop_pos.1) {
            return None;
        }
        let drop = self.file_drop.take()?;
        Some(drop.paths)
    }

    /// `rect` 内に file が hover 中か (drop target highlight 用、consume せず)。
    /// 判定は現在の pointer position に基づく (winit が hover 中も CursorMoved を送る)。
    #[must_use]
    pub fn is_file_hovering_in_rect(&self, rect: Rect) -> bool {
        if self.file_hover.is_none() {
            return false;
        }
        let Some((px, py)) = self.pointer.pos else { return false };
        rect.contains(px, py)
    }

    /// このフレームに hover 中の file 一覧 (read-only、consume されない)。
    #[must_use]
    pub fn hovering_files(&self) -> Option<&[PathBuf]> {
        self.file_hover.as_deref()
    }

    // ============================================================
    // M8 Phase 33: multi-select (rect drag)
    // ============================================================

    /// `wid` で識別される drag-rect セッション。`bounds` 内で primary 押下 → drag 開始。
    /// drag 中は library が半透明 cyan overlay (alpha 0.20 + 1px border) を **自動描画**。
    /// release フレームで `finished=true` を 1 度だけ返してから state クリア。
    pub fn take_drag_rect_in_rect(
        &mut self,
        wid: WidgetId,
        bounds: Rect,
    ) -> Option<DragRect> {
        let pointer = self.pointer;
        let modifiers = pointer.modifiers;

        let (active, just_finished, snapshot_start, snapshot_mods) = {
            let state: &mut DragRectState = self.widget_state(wid);

            // 押下 in bounds → drag 開始
            if pointer.primary_just_pressed
                && let Some((px, py)) = pointer.pos
                && bounds.contains(px, py)
            {
                state.drag_start = Some((px, py));
                state.start_modifiers = modifiers;
            }

            let active = state.drag_start.is_some();
            let just_finished = active && pointer.primary_just_released;
            let start = state.drag_start;
            let start_mods = state.start_modifiers;

            // release で state クリア (次フレーム以降は active=false)
            if pointer.primary_just_released {
                state.drag_start = None;
            }
            (active, just_finished, start, start_mods)
        };

        if !active {
            return None;
        }
        let start = snapshot_start?;
        let end = pointer.pos.unwrap_or(start);

        let drag = DragRect {
            start,
            end,
            modifiers: snapshot_mods,
            finished: just_finished,
        };

        // drag 中は半透明 cyan overlay を自動描画 (release frame でも 1 度描画する)。
        let r = drag.rect();
        let fill = Color { r: 0.32, g: 0.78, b: 0.95, a: 0.20 };
        let border = Color { r: 0.32, g: 0.78, b: 0.95, a: 0.85 };
        self.push_rect(RectCommand {
            rect: r,
            fill,
            border,
            border_width: 1.0,
            radius: [0.0; 4],
            clip_rect: Some(bounds),
        });

        Some(drag)
    }

    // ============================================================
    // M8 Phase 34: file dialog
    // ============================================================

    /// open file dialog (single) を frame 末尾で出すよう要求。`name` は結果取得時のタグ。
    pub fn request_open_file_dialog(
        &mut self,
        name: &'static str,
        title: &str,
        filters: &[FileDialogFilter],
    ) {
        self.pending_dialog_requests.push(DialogRequest {
            name,
            kind: DialogKind::OpenFile,
            title: title.into(),
            default_name: String::new(),
            filters: filters.to_vec(),
        });
    }

    pub fn request_open_files_dialog(
        &mut self,
        name: &'static str,
        title: &str,
        filters: &[FileDialogFilter],
    ) {
        self.pending_dialog_requests.push(DialogRequest {
            name,
            kind: DialogKind::OpenFiles,
            title: title.into(),
            default_name: String::new(),
            filters: filters.to_vec(),
        });
    }

    pub fn request_save_file_dialog(
        &mut self,
        name: &'static str,
        title: &str,
        default_name: &str,
        filters: &[FileDialogFilter],
    ) {
        self.pending_dialog_requests.push(DialogRequest {
            name,
            kind: DialogKind::SaveFile,
            title: title.into(),
            default_name: default_name.into(),
            filters: filters.to_vec(),
        });
    }

    /// 直前フレームに完了した dialog 結果を取り出す (1 度 consume)。
    /// 結果は次フレーム以降に届く (request 直後の同 frame では取り出せない)。
    pub fn take_dialog_result(&mut self, name: &'static str) -> Option<DialogResult> {
        let result = self.dialog_results.get(name)?.clone();
        self.consumed_dialog_results.insert(name);
        Some(result)
    }

    /// pointer が `rect` 内にあるなら、このフレームに蓄積された scroll delta (px) を取り出して
    /// 内部 buffer を 0 に戻す。focus 不要 (scroll は pointer 位置で配信)。
    ///
    /// 戻り値は `(dx, dy)` (winit 慣行: `dy > 0` = wheel を上方向に回した = コンテンツが上に流れる)。
    /// 同フレームに複数 widget が呼んでも、最初に呼んだ widget が消費する。
    pub fn take_scroll_in_rect(&mut self, rect: Rect) -> (f32, f32) {
        let Some((px, py)) = self.pointer.pos else { return (0.0, 0.0) };
        if !rect.contains(px, py) {
            return (0.0, 0.0);
        }
        let d = self.pointer.scroll_delta;
        self.pointer.scroll_delta = (0.0, 0.0);
        d
    }

    /// `wid` がフォーカスを持っているならフレームに溜まったキー入力を取り出す。
    ///
    /// チェック対象は **フレーム開始時の focus** (`self.focused`)。これによって、
    /// 「同フレームに click でフォーカスを取った直後に、その widget が直前まで
    /// 流れていたキー入力を遡って消費してしまう」事故を防ぐ
    /// (= 例: 何かを打鍵中に間違えて click した場合、打鍵は古い focus に流すのが正解)。
    /// 取り出すと内部 buffer は空になるので、フレーム内で 1 回だけ呼ぶこと。
    pub fn take_keyboard_events_if_focused(&mut self, wid: WidgetId) -> Vec<KeyEvent> {
        if self.focused == Some(wid) {
            std::mem::take(self.keyboard_events)
        } else {
            Vec::new()
        }
    }

    /// `wid` がフォーカスを持っているならフレームに溜まった IME イベントを取り出す。
    /// `take_keyboard_events_if_focused` と同じく、フレーム開始時 focus でチェックする。
    pub fn take_ime_events_if_focused(&mut self, wid: WidgetId) -> Vec<ImeEvent> {
        if self.focused == Some(wid) {
            std::mem::take(self.ime_events)
        } else {
            Vec::new()
        }
    }

    /// IME 候補ウィンドウを表示するべき領域 (cursor 直下) を要求する。
    /// 通常は focused text_input が自分の cursor 位置周辺の rect を渡す。
    /// アプリは `UiHost::ime_request()` でこの値を取得し、`set_ime_cursor_area` を呼ぶ。
    pub fn request_ime(&mut self, cursor_area: Rect) {
        self.ime_request = Some(cursor_area);
    }

    /// 内部: WidgetId に紐付く永続状態を取得 or 初期化。
    /// (M2 で waveform の LOD ピラミッドキャッシュに、M3 以降は fader/knob のドラッグ状態に使う)
    pub(crate) fn widget_state<S: WidgetState + Default + 'static>(
        &mut self,
        id: WidgetId,
    ) -> &mut S {
        let entry = self
            .state
            .entry(id)
            .or_insert_with(|| Box::new(S::default()));
        // `Box<dyn WidgetState>` 自体が `T: Any + Send + Sync` の blanket impl で
        // `WidgetState` を実装してしまうため、`entry.as_any_mut()` は **Box 外側** の
        // 実装を呼んでしまう (TypeId が Box<dyn WidgetState> になり downcast が必ず失敗)。
        // 明示的に `**entry` で dyn WidgetState まで deref してから vtable 経由で呼ぶ。
        let dyn_ws: &mut dyn WidgetState = &mut **entry;
        dyn_ws
            .as_any_mut()
            .downcast_mut::<S>()
            .expect("WidgetState 型不一致")
    }
}

/// M7 Phase 22: 2 つの clip rect を交差させる (両方 `None` なら `None`、片方なら他方、両方なら intersect)。
pub(crate) fn merge_clip(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
    match (a, b) {
        (None, None) => None,
        (Some(r), None) | (None, Some(r)) => Some(r),
        (Some(a), Some(b)) => Some(a.intersect(b)),
    }
}

/// M7 Phase 22: scroll で clip rect が変化したら scene cache を無効化するため、widget の input_hash
/// に current_clip の bits を混ぜる (`x/y/w/h` の f32 bits + presence flag)。
pub(crate) fn mix_clip_into_hash(input_hash: u64, clip: Option<Rect>) -> u64 {
    use std::hash::{Hash, Hasher};
    use std::collections::hash_map::DefaultHasher;
    let mut h = DefaultHasher::new();
    input_hash.hash(&mut h);
    match clip {
        None => 0u8.hash(&mut h),
        Some(r) => {
            1u8.hash(&mut h);
            r.x.to_bits().hash(&mut h);
            r.y.to_bits().hash(&mut h);
            r.w.to_bits().hash(&mut h);
            r.h.to_bits().hash(&mut h);
        }
    }
    h.finish()
}

/// M8 Phase 30: focus traversal の方向 (`Ui::focusable` 登録の中から最近傍を選ぶ)。
#[derive(Debug, Clone, Copy)]
pub(crate) enum FocusDirection {
    Up,
    Down,
    Left,
    Right,
}

/// M8 Phase 30: Tab / Shift+Tab で次/前の focusable wid を選ぶ。
/// `current=None` の場合、forward なら最初、prev なら最後を返す。
pub(crate) fn tab_navigate(
    order: &[(WidgetId, Rect)],
    current: Option<WidgetId>,
    forward: bool,
) -> Option<WidgetId> {
    if order.is_empty() {
        return None;
    }
    let cur_idx = current.and_then(|c| order.iter().position(|(w, _)| *w == c));
    let next_idx = match cur_idx {
        Some(i) if forward => (i + 1) % order.len(),
        Some(i) => (i + order.len() - 1) % order.len(),
        None if forward => 0,
        None => order.len() - 1,
    };
    Some(order[next_idx].0)
}

/// M8 Phase 30: arrow nav で `dir` 方向の最近傍 focusable を選ぶ。
/// 距離は (主軸) + (副軸 × 2) のシンプルな metric。focus 範囲外 / 反対方向は除外。
pub(crate) fn arrow_navigate(
    order: &[(WidgetId, Rect)],
    current: Option<WidgetId>,
    dir: FocusDirection,
) -> Option<WidgetId> {
    if order.is_empty() {
        return None;
    }
    let Some(current) = current else {
        return Some(order[0].0);
    };
    let Some((cur_wid, cur_rect)) =
        order.iter().find(|(w, _)| *w == current).copied()
    else {
        return Some(order[0].0);
    };
    let cx = cur_rect.x + cur_rect.w * 0.5;
    let cy = cur_rect.y + cur_rect.h * 0.5;
    let mut best: Option<(WidgetId, f32)> = None;
    for (w, r) in order {
        if *w == cur_wid {
            continue;
        }
        let rx = r.x + r.w * 0.5;
        let ry = r.y + r.h * 0.5;
        let dx = rx - cx;
        let dy = ry - cy;
        let (primary, secondary) = match dir {
            FocusDirection::Up => (-dy, dx.abs()),
            FocusDirection::Down => (dy, dx.abs()),
            FocusDirection::Left => (-dx, dy.abs()),
            FocusDirection::Right => (dx, dy.abs()),
        };
        if primary <= 0.0 {
            continue;
        }
        let metric = primary + secondary * 2.0;
        match best {
            Some((_, m)) if metric <= m => best = Some((*w, metric)),
            None => best = Some((*w, metric)),
            _ => {}
        }
    }
    best.map(|(w, _)| w)
}

/// M8 Phase 34: rfd を **同期実行** して dialog を表示する。
///
/// feature `dialog` が無効化されている場合は常に `Cancelled` を返す (test 環境や rfd 互換性
/// 問題に対する retreat path)。
#[cfg(feature = "dialog")]
fn run_dialog_sync(req: &DialogRequest) -> DialogResult {
    let mut dialog = rfd::FileDialog::new().set_title(&req.title);
    for filter in &req.filters {
        dialog = dialog.add_filter(filter.name, filter.extensions);
    }
    match req.kind {
        DialogKind::OpenFile => match dialog.pick_file() {
            Some(p) => DialogResult::OpenFile(p),
            None => DialogResult::Cancelled,
        },
        DialogKind::OpenFiles => match dialog.pick_files() {
            Some(ps) => DialogResult::OpenFiles(ps),
            None => DialogResult::Cancelled,
        },
        DialogKind::SaveFile => {
            let dialog = if req.default_name.is_empty() {
                dialog
            } else {
                dialog.set_file_name(&req.default_name)
            };
            match dialog.save_file() {
                Some(p) => DialogResult::SaveFile(p),
                None => DialogResult::Cancelled,
            }
        }
    }
}

#[cfg(not(feature = "dialog"))]
fn run_dialog_sync(_req: &DialogRequest) -> DialogResult {
    DialogResult::Cancelled
}

/// 視覚フィードバック用 — 押下中(矩形内 & primary_pressed)なら true。
pub(crate) fn pressed_inside(rect: Rect, pointer: PointerFrame) -> bool {
    let Some((px, py)) = pointer.pos else { return false };
    pointer.primary_pressed && rect.contains(px, py)
}

/// hover 中なら true。
pub(crate) fn hovered(rect: Rect, pointer: PointerFrame) -> bool {
    let Some((px, py)) = pointer.pos else { return false };
    rect.contains(px, py)
}

/// 色のヘルパ。
pub(crate) fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `widget_state` で書き戻した値が次フレームでも同型として読み取れる
    /// (`Box<dyn WidgetState>` 自体への blanket impl が `as_any_mut` を奪わないことの回帰防止)。
    #[test]
    fn widget_state_round_trip_no_downcast_panic() {
        #[derive(Debug, Default)]
        struct MyState {
            count: u32,
        }

        struct Model;

        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let model = Model;
        let screen = PhysicalSize { width: 400, height: 300 };

        // フレーム 1: state を初期化して 1 回インクリメント。
        host.frame_to_edits(&model, &mut scene, screen, FrameInput::default(), |_, ui| {
            let id = WidgetId::ROOT.child("ws-roundtrip");
            let state: &mut MyState = ui.widget_state(id);
            assert_eq!(state.count, 0);
            state.count += 1;
        });

        // フレーム 2: 同じ id で同じ型を取り直すと値が保持されている。
        host.frame_to_edits(&model, &mut scene, screen, FrameInput::default(), |_, ui| {
            let id = WidgetId::ROOT.child("ws-roundtrip");
            let state: &mut MyState = ui.widget_state(id);
            assert_eq!(state.count, 1);
            state.count += 1;
        });

        host.frame_to_edits(&model, &mut scene, screen, FrameInput::default(), |_, ui| {
            let id = WidgetId::ROOT.child("ws-roundtrip");
            let state: &mut MyState = ui.widget_state(id);
            assert_eq!(state.count, 2);
        });
    }

    /// 「hover フレームを描画 → 次フレームで同フレーム内 press + release」で
    /// click が 1 回目から発火することを担保する。ユーザ報告:
    /// 「ボタンにマウスを乗せた直後の最初のクリックでアクションが反応しない」
    /// の回帰防止。
    #[test]
    fn button_click_fires_on_first_hover_then_click() {
        struct Counter {
            count: u32,
        }

        let mut host: UiHost<Counter> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let mut model = Counter { count: 0 };
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 32.0 };

        // Frame 1: cursor をボタン上にホバー (まだクリック無し)。
        let pointer_hover = PointerFrame {
            pos: Some((50.0, 16.0)),
            ..PointerFrame::default()
        };
        let edits = host.frame_to_edits(&model, &mut scene, screen, FrameInput { pointer: pointer_hover, ..Default::default() }, |_, ui| {
            ui.button_at("test", "click me", rect, || {
                Edit::mutate(|m: &mut Counter| m.count += 1)
            });
        });
        for e in edits {
            e.apply(&mut model);
        }
        assert_eq!(model.count, 0, "hover フレームでは click は出ない");

        // Frame 2: 同フレーム内で press + release (高速クリック相当)。
        let pointer_click = PointerFrame {
            pos: Some((50.0, 16.0)),
            primary_just_pressed: true,
            primary_just_released: true,
            ..PointerFrame::default()
        };
        let edits = host.frame_to_edits(&model, &mut scene, screen, FrameInput { pointer: pointer_click, ..Default::default() }, |_, ui| {
            ui.button_at("test", "click me", rect, || {
                Edit::mutate(|m: &mut Counter| m.count += 1)
            });
        });
        for e in edits {
            e.apply(&mut model);
        }
        assert_eq!(
            model.count, 1,
            "hover 直後の最初のクリックで click が発火するべき"
        );
    }

    /// press と release が別フレームに分かれて届くケース (winit が press と release で
    /// それぞれ別の redraw を発火するパターン) で、`press_started_inside` がフレーム間で
    /// ちゃんと保持されて release フレームで click 発火することを担保する。
    #[test]
    fn button_click_fires_when_press_and_release_in_separate_frames() {
        struct Counter {
            count: u32,
        }

        let mut host: UiHost<Counter> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let mut model = Counter { count: 0 };
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 32.0 };
        let render = |host: &mut UiHost<Counter>,
                      scene: &mut Scene,
                      model: &Counter,
                      pointer: PointerFrame|
         -> Vec<Edit<Counter>> {
            host.frame_to_edits(model, scene, screen, FrameInput { pointer, ..Default::default() }, |_, ui| {
                ui.button_at("test", "click me", rect, || {
                    Edit::mutate(|m: &mut Counter| m.count += 1)
                });
            })
        };

        // Frame 1: hover.
        let edits = render(
            &mut host,
            &mut scene,
            &model,
            PointerFrame {
                pos: Some((50.0, 16.0)),
                ..PointerFrame::default()
            },
        );
        for e in edits { e.apply(&mut model); }
        assert_eq!(model.count, 0);

        // Frame 2: press フレーム (まだ release していない、ボタン押下中)。
        let edits = render(
            &mut host,
            &mut scene,
            &model,
            PointerFrame {
                pos: Some((50.0, 16.0)),
                primary_just_pressed: true,
                primary_just_released: false,
                primary_pressed: true,
                ..PointerFrame::default()
            },
        );
        for e in edits { e.apply(&mut model); }
        assert_eq!(model.count, 0, "press フレームでは click は出ない");

        // Frame 3: release フレーム。
        let edits = render(
            &mut host,
            &mut scene,
            &model,
            PointerFrame {
                pos: Some((50.0, 16.0)),
                primary_just_pressed: false,
                primary_just_released: true,
                ..PointerFrame::default()
            },
        );
        for e in edits { e.apply(&mut model); }
        assert_eq!(
            model.count, 1,
            "release フレームで click 発火するべき (press_started_inside が保持されている)"
        );
    }

    /// `Ui::set_focus` の効果が同フレームで `is_focused` に反映されること、
    /// および次フレームでも維持されることを確認する。
    #[test]
    fn focus_set_persists_to_next_frame() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let id = WidgetId::ROOT.child("focus-target");

        // Frame 1: set_focus を呼ぶと **同フレーム内で** is_focused = true になる。
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            assert!(!ui.is_focused(id));
            ui.set_focus(id);
            assert!(ui.is_focused(id), "set_focus 後は同フレームで is_focused = true");
        });
        assert_eq!(host.focused_widget(), Some(id));

        // Frame 2: 何もしないが focus は維持。
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            assert!(ui.is_focused(id));
        });
        assert_eq!(host.focused_widget(), Some(id));
    }

    /// フォーカスを取った widget の上でクリックされても、その widget が `set_focus` を
    /// 呼び続ける限りフォーカスは保たれる。クリック先が誰も `set_focus` を呼ばない
    /// (= フォーカス可能でない場所) ならフォーカスはクリアされる。
    #[test]
    fn click_outside_clears_focus_when_no_widget_claims() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let id = WidgetId::ROOT.child("focus-target");

        // Frame 1: フォーカスを取る。
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.set_focus(id);
        });
        assert_eq!(host.focused_widget(), Some(id));

        // Frame 2: クリック発生 (just_released=true) で誰も set_focus を呼ばない → blur。
        let click = PointerFrame {
            pos: Some((50.0, 50.0)),
            primary_just_released: true,
            ..PointerFrame::default()
        };
        host.frame_to_edits(&(), &mut scene, screen, FrameInput { pointer: click, ..Default::default() }, |(), _ui| {
            // 誰も set_focus / clear_focus を呼ばない。
        });
        assert_eq!(
            host.focused_widget(),
            None,
            "誰もフォーカスを取り直さなかったので blur される"
        );
    }

    /// 同フレームで set_focus を呼んでいればクリックがあってもフォーカスは保たれる。
    #[test]
    fn focus_kept_when_widget_re_claims_on_click() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let id = WidgetId::ROOT.child("focus-target");

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.set_focus(id);
        });
        assert_eq!(host.focused_widget(), Some(id));

        let click = PointerFrame {
            pos: Some((50.0, 50.0)),
            primary_just_released: true,
            ..PointerFrame::default()
        };
        host.frame_to_edits(&(), &mut scene, screen, FrameInput { pointer: click, ..Default::default() }, |(), ui| {
            // クリックフレームで widget が再度 set_focus を呼ぶ (text_input が再クリックされたケース)。
            ui.set_focus(id);
        });
        assert_eq!(host.focused_widget(), Some(id), "再 set_focus でフォーカス維持");
    }

    /// text_input をクリックでフォーカスを取り、キー入力で text を編集できることを担保する。
    /// click → focus → 'A' 入力 → モデルが "A" になる、という流れを通しで検証。
    #[test]
    fn text_input_click_focus_then_typing_modifies_text() {
        use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey};

        struct Doc {
            text: String,
        }

        let mut host: UiHost<Doc> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let mut model = Doc { text: String::new() };
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 0.0, y: 0.0, w: 200.0, h: 28.0 };

        // Frame 1: click で focus を取る (まだ text は空)。
        let click = PointerFrame {
            pos: Some((50.0, 14.0)),
            primary_just_pressed: true,
            primary_just_released: true,
            ..PointerFrame::default()
        };
        let edits = host.frame_to_edits(&model, &mut scene, screen, FrameInput { pointer: click, ..Default::default() }, |_, ui| {
            ui.text_input_at("ti", rect, "", |new| {
                Edit::mutate(|m: &mut Doc| m.text = new)
            });
        });
        for e in edits { e.apply(&mut model); }
        assert_eq!(model.text, "");

        // Frame 2: 'A' のキー入力を流す (focus されているので消費される)。
        let keys = vec![KeyEvent {
            state: ElementState::Pressed,
            text: Some("A".to_string()),
            physical_key: PhysicalKey::Other(0x41),
        }];
        let edits = host.frame_to_edits(
            &model, &mut scene, screen, FrameInput { keyboard: keys, ..Default::default() },
            |m, ui| {
                ui.text_input_at("ti", rect, &m.text, |new| {
                    Edit::mutate(|m: &mut Doc| m.text = new)
                });
            },
        );
        for e in edits { e.apply(&mut model); }
        assert_eq!(model.text, "A");

        // Frame 3: Backspace で 1 文字消える。
        let keys = vec![KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Backspace,
        }];
        let edits = host.frame_to_edits(
            &model, &mut scene, screen, FrameInput { keyboard: keys, ..Default::default() },
            |m, ui| {
                ui.text_input_at("ti", rect, &m.text, |new| {
                    Edit::mutate(|m: &mut Doc| m.text = new)
                });
            },
        );
        for e in edits { e.apply(&mut model); }
        assert_eq!(model.text, "");
    }

    /// IME preedit イベントは focused text_input に届き、state.preedit に反映される
    /// (model の text には反映されない)。Commit イベントは cursor 位置に挿入し
    /// preedit をクリアして Edit を発行する。
    #[test]
    fn text_input_ime_preedit_then_commit() {
        use crate::input::ImeEvent;
        use daw_ui_platform::PhysicalSize as PS;

        struct Doc {
            text: String,
        }

        let mut host: UiHost<Doc> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let mut model = Doc { text: String::new() };
        let screen = PS { width: 200, height: 100 };
        let rect = Rect { x: 0.0, y: 0.0, w: 200.0, h: 28.0 };

        // Frame 1: click で focus 取得。
        let click = PointerFrame {
            pos: Some((50.0, 14.0)),
            primary_just_pressed: true,
            primary_just_released: true,
            ..PointerFrame::default()
        };
        let edits = host.frame_to_edits(
            &model,
            &mut scene,
            screen,
            FrameInput { pointer: click, ..Default::default() },
            |_, ui| {
                ui.text_input_at("ti", rect, "", |new| {
                    Edit::mutate(|m: &mut Doc| m.text = new)
                });
            },
        );
        for e in edits { e.apply(&mut model); }
        assert_eq!(model.text, "");

        // Frame 2: preedit 「あ」が来る。model は変わらず、内部 state にだけ反映。
        let edits = host.frame_to_edits(
            &model,
            &mut scene,
            screen,
            FrameInput {
                ime: vec![ImeEvent::Preedit { text: "あ".to_string(), cursor: None }],
                ..Default::default()
            },
            |m, ui| {
                ui.text_input_at("ti", rect, &m.text, |new| {
                    Edit::mutate(|m: &mut Doc| m.text = new)
                });
            },
        );
        for e in edits { e.apply(&mut model); }
        assert_eq!(model.text, "", "preedit 中は model に反映しない");

        // Frame 3: commit 「あ」が来る。model に確定挿入され、preedit はクリア。
        let edits = host.frame_to_edits(
            &model,
            &mut scene,
            screen,
            FrameInput {
                ime: vec![ImeEvent::Commit("あ".to_string())],
                ..Default::default()
            },
            |m, ui| {
                ui.text_input_at("ti", rect, &m.text, |new| {
                    Edit::mutate(|m: &mut Doc| m.text = new)
                });
            },
        );
        for e in edits { e.apply(&mut model); }
        assert_eq!(model.text, "あ", "commit で model.text に挿入される");
    }

    /// IME イベントは focused widget にだけ届き、focused でない widget は空を受け取る。
    #[test]
    fn ime_events_delivered_only_to_focused() {
        use crate::input::ImeEvent;

        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let id_a = WidgetId::ROOT.child("a");
        let id_b = WidgetId::ROOT.child("b");

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.set_focus(id_a);
        });

        let ime = vec![ImeEvent::Commit("z".to_string())];
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { ime, ..Default::default() },
            |(), ui| {
                let b_ime = ui.take_ime_events_if_focused(id_b);
                assert_eq!(b_ime.len(), 0);
                let a_ime = ui.take_ime_events_if_focused(id_a);
                assert_eq!(a_ime.len(), 1);
                let a_ime2 = ui.take_ime_events_if_focused(id_a);
                assert_eq!(a_ime2.len(), 0, "drain 後は空");
            },
        );
    }

    /// キー入力イベントは focused widget だけに届き、他の widget には空が返る。
    #[test]
    fn keyboard_events_delivered_only_to_focused() {
        use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey};

        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let id_a = WidgetId::ROOT.child("a");
        let id_b = WidgetId::ROOT.child("b");

        // a にフォーカスを置く。
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.set_focus(id_a);
        });

        // 次フレーム: キー入力を流す。a は受け取れる、b は受け取れない。
        let keys = vec![KeyEvent {
            state: ElementState::Pressed,
            text: Some("x".to_string()),
            physical_key: PhysicalKey::Other(0),
        }];
        host.frame_to_edits(&(), &mut scene, screen, FrameInput { keyboard: keys, ..Default::default() }, |(), ui| {
            // b で先に呼んでも空 (フォーカスが a)。
            let b_keys = ui.take_keyboard_events_if_focused(id_b);
            assert_eq!(b_keys.len(), 0);
            // a が呼ぶと届く。
            let a_keys = ui.take_keyboard_events_if_focused(id_a);
            assert_eq!(a_keys.len(), 1);
            assert_eq!(a_keys[0].text.as_deref(), Some("x"));
            // 二度目に a が呼んでも空 (内部 buffer が drain 済み)。
            let a_keys2 = ui.take_keyboard_events_if_focused(id_a);
            assert_eq!(a_keys2.len(), 0);
        });
    }

    /// M4 Phase 11: 同じ wid + input_hash で 2 回呼ぶと、2 回目は draw_fn が実行されない
    /// (キャッシュ命中で前フレームの commands が scene に append される)。
    #[test]
    fn with_widget_node_hit_skips_draw_fn() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let id = WidgetId::ROOT.child("cache-test");
        let test_rect = Rect { x: 10.0, y: 20.0, w: 30.0, h: 40.0 };

        // Frame 1: cache miss → draw_fn 実行、scene に rect が積まれる。
        let calls_1 = std::cell::Cell::new(0_u32);
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.with_widget_node(id, 0xCAFE, |ui| {
                calls_1.set(calls_1.get() + 1);
                ui.push_rect(RectCommand {
                    rect: test_rect,
                    fill: Color::rgb(1.0, 0.0, 0.0),
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: None,
                });
            });
        });
        assert_eq!(calls_1.get(), 1, "1 回目は draw_fn が実行される");
        assert_eq!(scene.rects.len(), 1);

        // Frame 2: 同じ wid + 同じ hash → cache hit、draw_fn は実行されない。
        scene.clear();
        let calls_2 = std::cell::Cell::new(0_u32);
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.with_widget_node(id, 0xCAFE, |ui| {
                calls_2.set(calls_2.get() + 1);
                ui.push_rect(RectCommand {
                    rect: test_rect,
                    fill: Color::rgb(1.0, 0.0, 0.0),
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: None,
                });
            });
        });
        assert_eq!(calls_2.get(), 0, "2 回目は cache hit で draw_fn が実行されない");
        // scene には cache 経由で同じ rect が積まれている。
        assert_eq!(scene.rects.len(), 1);
        assert_eq!(scene.rects[0].rect, test_rect);
    }

    /// M4 Phase 11: hash が変わると cache miss、draw_fn が再実行される。
    #[test]
    fn with_widget_node_miss_runs_draw_fn() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let id = WidgetId::ROOT.child("miss-test");

        let calls_1 = std::cell::Cell::new(0_u32);
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.with_widget_node(id, 0xAAAA, |ui| {
                calls_1.set(calls_1.get() + 1);
                ui.push_rect(RectCommand {
                    rect: Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 },
                    fill: Color::rgb(1.0, 0.0, 0.0),
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: None,
                });
            });
        });

        // Frame 2: 異なる hash → cache miss、draw_fn が再実行される。
        scene.clear();
        let calls_2 = std::cell::Cell::new(0_u32);
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.with_widget_node(id, 0xBBBB, |ui| {
                calls_2.set(calls_2.get() + 1);
                ui.push_rect(RectCommand {
                    rect: Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 },
                    fill: Color::rgb(0.0, 1.0, 0.0),
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: None,
                });
            });
        });
        assert_eq!(calls_1.get(), 1);
        assert_eq!(calls_2.get(), 1, "hash 変化で draw_fn が再実行される");
    }

    /// M4 Phase 11: 前フレームに登場した widget が次フレームで呼ばれなければ
    /// scenegraph から eviction される。
    #[test]
    fn scenegraph_evicts_unseen_widgets() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let id_a = WidgetId::ROOT.child("evict-a");
        let id_b = WidgetId::ROOT.child("evict-b");

        // Frame 1: a と b 両方を wrap → scenegraph に 2 entry。
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.with_widget_node(id_a, 1, |_| {});
            ui.with_widget_node(id_b, 2, |_| {});
        });

        // Frame 2: a だけ wrap → b は seen に入らないので eviction、a は残る。
        // 同 hash で再呼び出し → cache hit、draw_fn 実行されない。
        let a_calls = std::cell::Cell::new(0_u32);
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.with_widget_node(id_a, 1, |_| {
                a_calls.set(a_calls.get() + 1);
            });
        });
        assert_eq!(a_calls.get(), 0, "a は cache hit で draw_fn 不実行");

        // Frame 3: b を再 wrap → 一度 eviction されているので cache miss、draw_fn が走る。
        let b_calls = std::cell::Cell::new(0_u32);
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.with_widget_node(id_b, 2, |_| {
                b_calls.set(b_calls.get() + 1);
            });
        });
        assert_eq!(b_calls.get(), 1, "b は eviction されたので cache miss、draw_fn が再実行");
    }
}
