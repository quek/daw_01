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

use daw_ui_platform::{KeyEvent, PhysicalSize};
use daw_ui_renderer::{Color, GlyphArea, LineBatch, Rect, RectCommand, Scene};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::input::{FrameInput, ImeEvent, PointerFrame};
use crate::popup::PopupOpenState;
use crate::scenegraph::{CachedCommands, Scenegraph};
use crate::widgets::WidgetState;

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
            _m: PhantomData,
        }
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
    /// 2. 収集した edits を `&mut model` に apply
    /// 3. edits があった / focus が変わった場合は `redraw_request` を呼ぶ
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
        for e in edits {
            e.apply(model);
        }
        // 自動 redraw の発火条件: edits / focus 変化 / widget からの request_redraw
        if had_edits
            || self.focus_changed_in_last_frame
            || self.redraw_requested_in_last_frame
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
        let FrameInput { pointer, keyboard, ime } = input;
        let mut keyboard_events = keyboard;
        let mut ime_events = ime;
        let cursor = Rect::new(0.0, 0.0, screen.width as f32, screen.height as f32);
        let focused_at_start = self.focused;
        let mut seen_widgets: HashSet<WidgetId> = HashSet::new();
        let mut redraw_requested = false;
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
            _m: PhantomData,
        };
        f(model, &mut ui);
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
