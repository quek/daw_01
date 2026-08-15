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

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;

use cosmic_text::FontSystem;
use daw_ui_platform::{
    CursorIcon, ImeTextEdit, KeyEvent, PhysicalKey, PhysicalSize, RectPx, TextDocument,
};
use daw_ui_renderer::{
    GlyphArea, LineBatch, LineSegment, Primitive, Rect, RectCommand, Scene, TexturedQuad,
};

use crate::clipboard::ClipboardProvider;
use crate::dialog::{DialogKind, DialogRequest, DialogResult, FileDialogFilter};
use crate::edit::Edit;
use crate::id::WidgetId;
use crate::input::{DroppedFiles, FrameInput, ImeEvent, PointerFrame};
use crate::popup::PopupOpenState;
use crate::scenegraph::{CachedCommands, Scenegraph};
use crate::shortcut::{self, ShortcutMap};
use crate::text_metrics::TextMetrics;
use crate::theme::Palette;
use crate::widgets::WidgetState;
use crate::widgets::drag_in_rect::{DragInRectState, DragInfo, DragKind};
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
/// `focus_changed_in_last_frame` / `redraw_requested_in_last_frame` がそれぞれ独立した
/// 「frame 内で 1 度だけ書かれる」フラグで意味的に正交、state machine 化のメリットなし。
/// **(M15)** focus 中 text_input の document snapshot を OS text store に publish する callback。
type SetTextDocumentFn = Box<dyn Fn(Option<&TextDocument>) + Send + Sync>;
/// **(M15)** OS IME がこのフレームに加えた text 編集を取り出す callback。
type TakeImeEditsFn = Box<dyn Fn() -> Vec<ImeTextEdit> + Send + Sync>;

#[allow(clippy::struct_excessive_bools)]
pub struct UiHost<M: ?Sized + 'static> {
    /// r.md #48: 現在有効な色パレット。**widget が読む色の唯一の出どころ**で、
    /// `Ui::palette()` から供給される。プロセスグローバルにしないのは、テストが
    /// 並列スレッドで走るため (可変グローバルだとテーマを差し替えるテストが
    /// 他テストの色アサーションを壊す)。差し替えは [`UiHost::set_palette`]。
    palette: Arc<Palette>,
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
    /// **(M15)** 直前の `frame()` で focus 中の text_input が `Ui::publish_text_document` した
    /// snapshot。`frame()` 末尾で `set_text_document_request` 経由で OS text store (TSF) に publish
    /// する。`None` = 編集対象なし (store を空にして IME 非アクティブ化)。
    last_text_document: Option<TextDocument>,
    /// M4 Phase 10 で追加: 内部 scenegraph (per-widget input_hash の前フレーム履歴)。
    #[allow(dead_code)]
    scenegraph: Scenegraph,
    /// edits / focus 変化の検出時にライブラリが自動で呼ぶ closure。
    /// 通常は `WindowBackend::request_redraw` をラップしたもの。
    redraw_request: Box<dyn Fn() + Send + Sync>,
    /// M9 Phase 41b: cursor 形状を OS に伝える callback。`with_window` 経由で
    /// `WindowBackend::set_cursor` をラップ。`new` 直接呼び出しでは `None` (no-op)。
    /// `pub(crate)` は他 widget の `#[cfg(test)]` で cursor 検証 mock を直接 inject するため。
    pub(crate) set_cursor_request: Option<Box<dyn Fn(CursorIcon) + Send + Sync>>,
    /// cursor 位置を OS に warp させる callback。`with_window` 経由で
    /// `WindowBackend::set_cursor_position` をラップ。`new` 直接呼び出しでは `None` (no-op)。
    pub(crate) set_cursor_pos_request: Option<Box<dyn Fn(f32, f32) + Send + Sync>>,
    /// **(M15)** focus 中 text_input の document snapshot を OS text store (TSF) に publish する
    /// callback。`with_window` 経由で `WindowBackend::set_text_input_document` をラップ。
    /// `new` 直接呼び出しでは `None` (no-op)。`frame()` 末尾で `last_text_document` を flush する。
    set_text_document_request: Option<SetTextDocumentFn>,
    /// **(M15)** OS IME (TSF) がこのフレームに text store へ加えた編集を取り出す callback。
    /// `with_window` 経由で `WindowBackend::take_ime_text_edits` をラップ。`frame_to_edits` 冒頭で
    /// drain し、`ImeEvent::ReplaceRange` / `SetSelection` に変換して focused widget に流す。
    take_ime_edits_request: Option<TakeImeEditsFn>,
    /// M7 Phase 25: 現在開いている popup の集合 (menu / context_menu / dropdown 共通)。
    /// `Ui::open_popup` / `Ui::close_popup` で出し入れする。`Ui` 経由で `&mut` 借用される
    /// ため rustc から "never read" と誤判定されるが、実際には popup_layer で読まれる。
    #[allow(dead_code)]
    open_popups: HashMap<WidgetId, PopupOpenState>,
    /// M7 後の改善: widget が `Ui::request_redraw()` を呼んだ場合の累積フラグ。
    /// `frame()` の最後で `redraw_request` 呼び出し条件に含まれる。
    redraw_requested_in_last_frame: bool,
    /// 自動 `request_redraw` を抑止しているか ([`Self::set_redraw_suppressed`])。
    redraw_suppressed: bool,
    /// M8 Phase 30: shortcut 登録テーブル。
    shortcut_map: ShortcutMap,
    /// M8 Phase 31: OS clipboard provider (None なら set/get は no-op)。
    clipboard: Option<Box<dyn ClipboardProvider>>,
    /// M8 Phase 34: 前フレームに完了した dialog 結果 (次フレームで `Ui::take_dialog_result` で取り出される)。
    pending_dialog_results: HashMap<&'static str, DialogResult>,
    /// M8 Phase 30: 前フレームに登場した focusable widget の (id, rect) 一覧。
    /// Tab / arrow nav の対象決定用。
    last_focusable: Vec<(WidgetId, Rect)>,
    /// M8: `frame_to_edits` で Ui が書いた transient な request 群。`frame()` の後半で
    /// drain される (clipboard write, dialog 同期実行, cursor flush)。
    ///
    /// **`frame_to_edits` 単独で使う場合の挙動**: 各 transient フィールドは `frame_to_edits`
    /// 冒頭で `clear()` されるため、 累積 leak はない (call N+1 で N 件目の transient は捨てられる)。
    /// ただし clipboard write / dialog 同期実行 は **発火しない**。
    /// edits を audio thread に送る用途では transient は通常不要だが、 必要なら
    /// `frame()` を使う (内部で `frame_to_edits` + transient drain + 自動 request_redraw を実行)。
    transient_clipboard_writes: Vec<String>,
    transient_dialog_requests: Vec<DialogRequest>,
    transient_consumed_dialog_results: HashSet<&'static str>,
    /// M9 Phase 41b: 今フレームに `Ui::set_cursor` で要求された cursor (last call wins)。
    /// `frame()` 末尾で `set_cursor_request` callback に flush され、None にリセットされる。
    transient_cursor: Option<CursorIcon>,
    /// 今フレームに `Ui::warp_cursor` で要求された cursor 位置 (物理 px、last call wins)。
    /// `frame()` 末尾で `set_cursor_pos_request` callback に flush され、None にリセットされる。
    transient_cursor_pos: Option<(f32, f32)>,
    /// M9 Phase 43: 直近フレームの統計 (debug overlay 表示用)。`frame_to_edits` 末尾で更新。
    /// frame_ms は app 側で計測 (window backend / render pipeline により取得方法が違うため、
    /// library は frame_ms を track せず `Ui::debug_overlay(rect, frame_ms)` の引数で受ける)。
    last_frame_stats: FrameStats,
    /// M9 Phase 43: working buffer (frame_to_edits の冒頭で 0 リセット、widget 描画でインクリメント、
    /// 末尾で `last_frame_stats` に転記)。
    current_cache_hits: u32,
    current_cache_misses: u32,
    /// M9 P1-4: 直近 click (primary_just_released frame) の `(時刻, x, y)`。
    /// `take_double_click_in_rect` でダブルクリック判定に使う。
    /// is_double と判定したら次フレーム以降の連続 click 誤動作防止のため None にクリア。
    last_click: Option<(std::time::Instant, f32, f32)>,
    /// M9 P1-4: ダブルクリック判定の閾値 `(時間, 位置 px)`。default 400ms / 5px。
    double_click_threshold: (std::time::Duration, f32),
    /// daw_01 r.md #35: 右ボタン press 位置。release frame に「移動したか」を判定して
    /// **右クリック (= context menu)** と **右ドラッグ (= 矩形選択)** を区別するために保持する。
    /// press では何も起こさず release で確定するのは Windows の `WM_CONTEXTMENU` と同じ
    /// (右ボタン UP で飛ぶ)。`docs/plan_selection_modifiers.md` §4.1。
    secondary_press_pos: Option<(f32, f32)>,
    /// M14 Phase 57: 前フレームに `Ui::set_typing_focus(true)` が立ったか。立っていた場合、
    /// 今フレーム冒頭の shortcut layer は `is_typing_only_shortcut(name)` (= `select_all`
    /// `delete` `cut` `copy` `paste`) を `pending_shortcuts` に積まず `keyboard_events`
    /// に残し、focused widget が `Ui::take_typing_shortcut(name)` で拾えるようにする。
    last_typing_focus: bool,
    /// daw_01 r.md #36: **キーボードイベントを伴わずに** 次フレームで発火させる shortcut 名。
    ///
    /// OS フォーカスが別プロセスの窓 (= プラグインエディタ) にあるときは winit の
    /// `KeyboardInput` が来ないので、 通常の解決経路 (`shortcut_map.matches`) に乗せられない。
    /// かといって raw な `KeyEvent` を注入すると `typing_lock` / `bare_char_key` の調停に
    /// 掛かり、 daw_gui 側のテキスト欄にフォーカスがあると Space がそこへ入ってしまう。
    /// OS フォーカスが外にある以上 typing 判定は無意味なので、 **解決済みの shortcut 名**
    /// という一段上のレイヤに注入するのが正しい高さ。
    ///
    /// これにより consumer 側 (`take_shortcut`) は一切変わらず、 「キーの発生源」 だけが
    /// 増える (= 解決器も dispatch も 1 本のまま)。
    injected_shortcuts: Vec<&'static str>,
    /// M14 Phase 58: text shape による proportional font の実 advance 計算器。`Ui::measure_text`
    /// 経由で text_input の cursor / selection の x 位置を pixel-accurate に取得する。
    /// renderer 側の `GlyphPipeline` 内 `FontSystem` とは別 instance だが、同じ system fonts を
    /// 読むので shape 結果は一致する (キャッシュは別)。
    text_metrics: TextMetrics,
    /// 通常 (headless / example) パス用の measure FontSystem。renderer を持つ runner は
    /// `frame_with_fonts` で renderer 所有の FontSystem を注入するので、このフィールドは
    /// None のまま (= 二重ロードしない)。`frame` / `frame_to_edits` を直接使う caller の
    /// ときだけ初回 measure 時に lazy 生成する。
    owned_font_system: Option<FontSystem>,
    _m: PhantomData<fn(&mut M)>,
}

/// `Ui::debug_overlay` で表示する 1 frame 分の統計 (M9 Phase 43)。
///
/// `UiHost::last_frame_stats()` で取得できる。`frame_ms` は library が計測しないため
/// 含めない (app 側で `Instant` で測定し、`Ui::debug_overlay(rect, frame_ms)` の引数で渡す)。
#[derive(Debug, Default, Clone, Copy)]
pub struct FrameStats {
    /// 直近フレームに `with_widget_node` で cache hit した回数 (= 描画スキップ数)。
    pub cache_hits: u32,
    /// 直近フレームに `with_widget_node` で cache miss して draw_fn を実行した回数。
    pub cache_misses: u32,
    /// 直近フレームに登場した widget の数 (`seen_widgets` の末尾サイズ)。
    pub widget_count: u32,
    /// frame 末尾の scenegraph entry 数 (eviction 後)。`widget_count` と通常一致。
    pub scenegraph_size: u32,
}

impl FrameStats {
    /// cache hit 率 (`hits / (hits + misses)`、totalが 0 なら 0.0)。
    #[must_use]
    pub fn cache_hit_rate(&self) -> f32 {
        let total = self.cache_hits + self.cache_misses;
        if total == 0 {
            0.0
        } else {
            self.cache_hits as f32 / total as f32
        }
    }
}

impl<M: ?Sized + 'static> UiHost<M> {
    /// 再描画リクエストを呼ぶ closure を直接渡して構築する low-level constructor。
    /// 通常は [`Self::with_window`] を使う方が簡潔。
    pub fn new(redraw_request: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            palette: Arc::new(Palette::dark()),
            state: HashMap::new(),
            focused: None,
            focus_changed_in_last_frame: false,
            last_ime_request: None,
            last_text_document: None,
            scenegraph: Scenegraph::new(),
            redraw_request: Box::new(redraw_request),
            open_popups: HashMap::new(),
            redraw_requested_in_last_frame: false,
            redraw_suppressed: false,
            shortcut_map: ShortcutMap::with_default_bindings(),
            clipboard: None,
            pending_dialog_results: HashMap::new(),
            last_focusable: Vec::new(),
            transient_clipboard_writes: Vec::new(),
            transient_dialog_requests: Vec::new(),
            transient_consumed_dialog_results: HashSet::new(),
            transient_cursor: None,
            transient_cursor_pos: None,
            set_cursor_request: None,
            set_cursor_pos_request: None,
            set_text_document_request: None,
            take_ime_edits_request: None,
            last_frame_stats: FrameStats::default(),
            current_cache_hits: 0,
            current_cache_misses: 0,
            last_click: None,
            double_click_threshold: (std::time::Duration::from_millis(400), 5.0),
            secondary_press_pos: None,
            last_typing_focus: false,
            injected_shortcuts: Vec::new(),
            text_metrics: TextMetrics::new(),
            owned_font_system: None,
            _m: PhantomData,
        }
    }

    /// r.md #48: 有効な色パレットを差し替える。**変わったら `true`** を返すので、
    /// 呼び出し側は `if host.set_palette(p) { host.invalidate_scene_cache(); }` と書ける
    /// (キャッシュされた描画コマンドには旧テーマの色が焼き込まれているため、
    /// 捨てないとアレンジ / ピアノロールだけ旧色のまま残る)。
    ///
    /// 毎フレーム無条件に呼んでよい (同じ `Arc` なら `false` を返して no-op)。
    /// そうしておけば「テーマを変えたのに push し忘れた」 が構造的に起きない。
    pub fn set_palette(&mut self, palette: Arc<Palette>) -> bool {
        if Arc::ptr_eq(&self.palette, &palette) || self.palette == palette {
            return false;
        }
        self.palette = palette;
        true
    }

    /// 現在の色パレット。アプリ側が同じ実体を共有する (= 複製しない) ために `Arc` を返す。
    #[must_use]
    pub fn palette(&self) -> &Arc<Palette> {
        &self.palette
    }

    /// M9 P1-4: ダブルクリック判定の閾値を変更 (default: 400ms / 5px)。
    /// `Ui::take_double_click_in_rect` の判定に使う。
    pub fn set_double_click_threshold(&mut self, ms: u64, px: f32) {
        self.double_click_threshold = (std::time::Duration::from_millis(ms), px);
    }

    /// M9 Phase 43: 直近フレームの統計 (debug overlay 用)。
    ///
    /// `frame_to_edits` 末尾で更新されるので、frame closure 外から read できる。
    /// frame closure 内では「前 frame の stats」が見える (= `Ui::debug_overlay` で
    /// 描画する内容)。
    #[must_use]
    pub fn last_frame_stats(&self) -> FrameStats {
        self.last_frame_stats
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

    /// M8 Phase 30: shortcut map への参照 (preference の serialize 用)。
    pub fn shortcut_map(&self) -> &ShortcutMap {
        &self.shortcut_map
    }

    /// M8 Phase 30: shortcut map への mutable 参照 (実行時 rebind 用)。
    pub fn shortcut_map_mut(&mut self) -> &mut ShortcutMap {
        &mut self.shortcut_map
    }

    /// daw_01 r.md #36: 次フレームで `Ui::take_shortcut(name)` が拾えるように
    /// shortcut を 1 件注入する。
    ///
    /// OS フォーカスが別プロセスの窓 (プラグインエディタ等) にあってキーイベントが
    /// この event loop に来ないケース用。 呼び出し後は `request_redraw` すること
    /// (注入しただけでは再描画は走らない)。
    pub fn inject_shortcut(&mut self, name: &'static str) {
        self.injected_shortcuts.push(name);
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
        let win_for_redraw = std::sync::Arc::clone(&window);
        let win_for_cursor = std::sync::Arc::clone(&window);
        let win_for_cursor_pos = std::sync::Arc::clone(&window);
        let win_for_doc = std::sync::Arc::clone(&window);
        let win_for_edits = window;
        let mut host = Self::new(move || win_for_redraw.request_redraw());
        host.set_cursor_request = Some(Box::new(move |c| win_for_cursor.set_cursor(c)));
        // cursor 位置 warp (ノート作成で右端へ移動)。
        host.set_cursor_pos_request =
            Some(Box::new(move |x, y| win_for_cursor_pos.set_cursor_position(x, y)));
        // M15: TSF text store の publish / IME 編集 drain を window backend に橋渡し。
        host.set_text_document_request =
            Some(Box::new(move |doc| win_for_doc.set_text_input_document(doc)));
        host.take_ime_edits_request =
            Some(Box::new(move || win_for_edits.take_ime_text_edits()));
        host
    }

    /// テスト / offscreen render 用、`request_redraw` を呼ばない UiHost を構築する。
    pub fn no_redraw() -> Self {
        Self::new(|| {})
    }

    /// 自動 `request_redraw` を抑止する / 解除する。
    ///
    /// `frame()` 末尾の自動 redraw は、edits / focus 変化に加えて **widget が
    /// `Ui::request_redraw` で要求した継続アニメ** (レベルメーターの減衰・
    /// peak hold、scroll / tab のトランジション等) でも発火する。これは
    /// 「見えている画面を滑らかに保つ」ためのもので、利用者側が省電力等で
    /// **「今この画面は誰も見ていないので描かない」** と決めているときには
    /// 逆効果になる — widget は自分が見えているかを知らないので、要求を出し続ける。
    ///
    /// `true` の間は自動 redraw を呼ばないので、「描くかどうか」の判断を
    /// 利用者側の 1 箇所に集約できる。**解除した側が次のフレームを要求する責任を負う**
    /// (抑止中に溜まった要求は再生されない)。
    ///
    /// 入力イベント由来の再描画は利用者が直接 `WindowBackend::request_redraw` を
    /// 呼ぶ経路なので、これには影響されない。
    pub fn set_redraw_suppressed(&mut self, suppressed: bool) {
        self.redraw_suppressed = suppressed;
    }

    /// `frame()` 末尾で cursor 要求を受け取る sink を差し込む。通常は `with_window` が
    /// `WindowBackend::set_cursor` を繋ぐが、headless テストで cursor icon を捕捉したいとき
    /// (arrangement / piano_roll の drag カーソル検証等) に外部から設定する。
    pub fn set_cursor_sink(&mut self, sink: Box<dyn Fn(CursorIcon) + Send + Sync>) {
        self.set_cursor_request = Some(sink);
    }

    /// `frame()` 末尾で cursor 位置 warp 要求を受け取る sink を差し込む。`set_cursor_sink` の
    /// warp 位置版で、headless テストで `Ui::warp_cursor` の要求座標を捕捉したいとき
    /// (piano_roll のノート作成 release warp 検証等) に外部から設定する。
    pub fn set_cursor_pos_sink(&mut self, sink: Box<dyn Fn(f32, f32) + Send + Sync>) {
        self.set_cursor_pos_request = Some(sink);
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

    /// 粗粒度描画キャッシュ (`with_widget_node` / `HeavyCtx::cached`) を全部捨てる。
    ///
    /// **GPU 資産を作り直したとき (device lost からの復旧、daw_01 r.md #42) に必ず呼ぶ。**
    /// キャッシュされた描画コマンドには旧世代の `TextureHandle` が焼き込まれており、
    /// 捨てないと「input_hash が同じだから」という理由で無効ハンドル入りのコマンドが
    /// 再送され続ける (絵が欠けたまま固まる)。
    ///
    /// focus / widget state / popup は **保持**する (= 編集中の状態を壊さない)。
    /// 利用者に `viewport_key` へ世代番号を混ぜさせる (= 全 heavy widget に同じ
    /// boilerplate を強要する) 案は採らない。
    pub fn invalidate_scene_cache(&mut self) {
        self.scenegraph.clear();
    }

    /// 1 フレーム分の UI を構築。**edits は内部で apply され、`request_redraw` も
    /// 自動で呼ばれる**。利用者の boilerplate はゼロ。
    ///
    /// `f` は `(&model, &mut Ui)` を受け取り、ウィジェットを呼び出して UI を組む。
    /// 内部動作:
    /// 1. scene を積みつつ edits を収集 (build クロージャは古い model 値で 1 度だけ実行)
    /// 2. 収集した edits を `&mut model` に apply (forward mutation のみ)
    /// 3. **M8**: clipboard write / file dialog 同期実行 / dialog 結果クリーンアップ
    /// 4. edits / focus 変化があった場合は `redraw_request` を呼ぶ
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
        // 通常 (headless / example) パス: measure 用 FontSystem を UiHost が lazy 所有する。
        let mut font_system = self.owned_font_system.take().unwrap_or_else(FontSystem::new);
        self.frame_with_fonts(model, scene, screen, input, &mut font_system, f);
        self.owned_font_system = Some(font_system);
    }

    /// [`Self::frame`] と同じだが、measure 用の `FontSystem` を **呼び出し側が注入する** 版。
    /// renderer (`Renderer::font_system_mut`) 所有の FontSystem を渡すことで、ui-core が測定用に
    /// 別 FontSystem を二重ロードするのを避ける (measure と raster が同一 font DB / shape 設定を
    /// 共有する SSoT)。daw_gui runner はこちらを使う。
    pub fn frame_with_fonts<F>(
        &mut self,
        model: &mut M,
        scene: &mut Scene,
        screen: PhysicalSize,
        input: FrameInput,
        font_system: &mut FontSystem,
        f: F,
    ) where
        F: for<'a> FnOnce(&'a M, &mut Ui<'a, M>),
    {
        let edits = self.frame_to_edits_with_fonts(&*model, scene, screen, input, font_system, f);
        let had_edits = !edits.is_empty();

        // edits apply (forward mutation のみ)。undo/redo はアプリ層の責務 (S4a で lib undo 撤去)。
        for e in edits {
            e.apply(model);
        }

        // M8 Phase 31: clipboard write (frame 末尾で provider に書き込み)。
        if let Some(c) = self.clipboard.as_mut() {
            for s in self.transient_clipboard_writes.drain(..) {
                c.set_text(s);
            }
        } else {
            // provider 無しなら捨てる
            self.transient_clipboard_writes.clear();
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

        // M9 Phase 41b: cursor flush (Ui::set_cursor で要求された形状を OS に伝える)。
        if let Some(c) = self.transient_cursor.take()
            && let Some(req) = self.set_cursor_request.as_ref()
        {
            req(c);
        }

        // cursor 位置 warp flush (Ui::warp_cursor で要求された位置を OS に伝える)。
        if let Some((x, y)) = self.transient_cursor_pos.take()
            && let Some(req) = self.set_cursor_pos_request.as_ref()
        {
            req(x, y);
        }

        // M15: text store document flush。focus 中 text_input が publish した snapshot を
        // OS text store (TSF) に渡す。`None` (= 編集対象なし) も毎フレーム渡して store を
        // 空にし IME を非アクティブ化する。
        if let Some(req) = self.set_text_document_request.as_ref() {
            req(self.last_text_document.as_ref());
        }

        // 自動 redraw の発火条件: edits / focus 変化 / widget からの request_redraw
        // / dialog 実行 (新結果が出た) / 残っている dialog 結果 (widget が次フレームで取り出す)。
        if !self.redraw_suppressed
            && (had_edits
                || self.focus_changed_in_last_frame
                || self.redraw_requested_in_last_frame
                || dialog_runs
                || !self.pending_dialog_results.is_empty())
        {
            (self.redraw_request)();
        }
    }

    /// (Advanced) edits を返す low-level API。
    /// `Edit<M>` の apply タイミングを自前で制御したい場合 (audio thread に送る、
    /// batch apply、undo stack 等) のみ使用する。通常は [`Self::frame`] を使うこと。
    ///
    /// 挙動の特徴:
    /// - 戻り値の `Vec<Edit<M>>` は **apply されていない**。 caller が `apply` を呼ぶ責任を負う。
    /// - **自動 `request_redraw` は呼ばれない**。 caller が edits 検出時に手動で
    ///   `WindowBackend::request_redraw` を呼ぶ責任を負う。
    /// - undo/redo / clipboard write / dialog 同期実行 など `Edit` 以外の副作用は
    ///   **発火しない** (transient フィールドは累積せず冒頭で `clear()` されるので leak はないが、
    ///   ユーザに伝わらない)。 これらを使いたい場合は [`Self::frame`] を使うこと。
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
        // 通常パス: measure 用 FontSystem を UiHost が lazy 所有する ([`Self::frame`] 参照)。
        let mut font_system = self.owned_font_system.take().unwrap_or_else(FontSystem::new);
        let edits = self.frame_to_edits_with_fonts(model, scene, screen, input, &mut font_system, f);
        self.owned_font_system = Some(font_system);
        edits
    }

    /// [`Self::frame_to_edits`] の FontSystem 注入版 (measure 用 FontSystem を呼び出し側が渡す)。
    /// 中身は旧 `frame_to_edits` 本体そのもの。通常は [`Self::frame_to_edits`] / [`Self::frame`]
    /// / [`Self::frame_with_fonts`] を使う。
    #[allow(clippy::too_many_lines)]
    pub fn frame_to_edits_with_fonts<F>(
        &mut self,
        model: &M,
        scene: &mut Scene,
        screen: PhysicalSize,
        input: FrameInput,
        font_system: &mut FontSystem,
        f: F,
    ) -> Vec<Edit<M>>
    where
        F: for<'a> FnOnce(&'a M, &mut Ui<'a, M>),
    {
        let mut edits: Vec<Edit<M>> = Vec::new();
        let FrameInput { pointer, keyboard, ime, file_drop, file_hover } = input;
        let mut keyboard_events = keyboard;
        let mut ime_events = ime;

        // M15: OS text store (TSF) がこのフレームに加えた編集 (まぜ書き変換 / 再変換 /
        // composition 確定) を drain し、`ImeEvent` に変換して ime_events 先頭へ置く
        // (winit IMM 由来の ime より前に適用)。Windows で TSF 駆動中は winit IMM を使わない
        // ので両者は競合しない。非 Windows / TSF 不在では callback は空を返す。
        if let Some(f) = self.take_ime_edits_request.as_ref() {
            let store_edits = f();
            if !store_edits.is_empty() {
                let mut converted: Vec<ImeEvent> =
                    store_edits.into_iter().map(ime_text_edit_to_event).collect();
                converted.append(&mut ime_events);
                ime_events = converted;
            }
        }

        // M8 Phase 30 / M14 Phase 57: shortcut layer (frame 頭)。keyboard_events を
        // `shortcut_map.matches` で走査、マッチした events を取り除いて name を
        // `pending_shortcuts` に積む。**前フレームに `set_typing_focus(true)` が立って
        // いれば**、`is_typing_only_shortcut(name)` が true な name (`select_all` /
        // `delete` / `cut` / `copy` / `paste`) は global 消費を抑制し、`keyboard_events`
        // に残して focused widget が `take_typing_shortcut(name)` で拾えるようにする。
        // text_input が後で `take_keyboard_events_if_focused` で取るのは shortcut 後の残り。
        let modifiers = pointer.modifiers;
        let typing_lock = self.last_typing_focus;
        // daw_01 r.md #36: 外部窓 (プラグインエディタ) から転送された shortcut を先頭に積む。
        // 解決は済んでいるので typing 調停は通さない (OS フォーカスが daw_gui の外にある間に
        // 押されたキーなので、 こちらのテキスト欄は入力対象ではない)。
        let mut pending_shortcuts: Vec<&'static str> = std::mem::take(&mut self.injected_shortcuts);
        // typing 中に keyboard_events に残された paste shortcut があれば clipboard を read する
        // (text_input が `take_typing_shortcut("paste")` で受け取る前に provider から取り出す)。
        let mut typing_paste_pending = false;
        keyboard_events.retain(|ev| {
            // OS auto-repeat (押しっぱなし) は **global shortcut にしない**。 shortcut は
            // Delete / D / E のような離散コマンドに bind されるので、 repeat で連射されると
            // 「Delete 長押しでトラックが次々消える」 類の破壊的挙動になる (daw_01 r.md #43)。
            // event は `keyboard_events` に残すので、 focused な text_input は従来どおり
            // Backspace / 矢印の長押しリピートを受け取れる (= 抑止は shortcut 層のみ)。
            if ev.repeat {
                return true;
            }
            if let Some(name) = self.shortcut_map.matches(ev, modifiers) {
                // (daw_01 #056) typing 中は command 修飾 (Ctrl/Alt/Logo) を持たない printable
                // 文字キー (英数字 / Space、Shift だけ付きも含む) に bind された shortcut を global
                // 消費せず、text_input に文字として届ける。daw_01 は R/D/V/... を素キーに bind する
                // ため、これを消費すると文字入力が奪われる。command 修飾付き (Ctrl+S 等) や
                // F1-F24 / Escape 等の非テキストキーは従来どおり typing 中も global 発火する。
                let bare_char_key = matches!(
                    ev.physical_key,
                    PhysicalKey::Char(_) | PhysicalKey::Digit(_) | PhysicalKey::Space
                ) && !modifiers.ctrl
                    && !modifiers.alt
                    && !modifiers.logo;
                if typing_lock && (shortcut::is_typing_only_shortcut(name) || bare_char_key) {
                    if name == "paste" {
                        typing_paste_pending = true;
                    }
                    true
                } else {
                    pending_shortcuts.push(name);
                    false
                }
            } else {
                true
            }
        });

        // M8 Phase 31 / M14 Phase 57: clipboard paste — paste shortcut が global / typing どちらの
        // 経路にあっても provider から read しておく。1 フレーム内で paste と他の shortcut が
        // 同時に発生しても、paste の取り出しは 1 度限り。
        let pending_clipboard_paste: Option<String> =
            if pending_shortcuts.contains(&"paste") || typing_paste_pending {
                self.clipboard.as_mut().and_then(|c| c.get_text())
            } else {
                None
            };

        // transient outputs (`frame()` 後半で読まれる) を frame 頭でクリア。
        self.transient_clipboard_writes.clear();
        self.transient_dialog_requests.clear();
        self.transient_consumed_dialog_results.clear();
        self.transient_cursor = None;
        self.transient_cursor_pos = None;
        // M9 Phase 43: cache stats を frame 頭でリセット。with_widget_node 内で increment、
        // 末尾で `last_frame_stats` に転記。
        self.current_cache_hits = 0;
        self.current_cache_misses = 0;

        // M9 P1-4: ダブルクリック判定。primary_just_released で前回 click との時間/位置 diff を見て、
        // threshold 内なら `pending_double_click` を Some に立てる。同 frame 内で
        // `Ui::take_double_click_in_rect(rect)` が rect.contains で 1 度だけ消費する。
        // press ベースの double-click 検出。release ベース (下) と独立で、かつ
        // `last_click` を **変更しない** (read-only)。 double-click の 2 度目の press フレームで
        // Some を立て、piano_roll widget が `take_double_click_press_in_rect` で消費して
        // 「ダブルクリックのボタンを放さずに drag → note 長を決める」セッションを開始する
        // (Bitwig 流)。release を待つと drag detection が手遅れになるため press 時に取る。
        // **必ず release ベースより先に評価する**: 同一フレームに press+release が両方立つ単発
        // クリック (press_just + released_just) で、release ベースが先に `last_click` を今フレームの
        // 値に更新してしまうと、 直後の press 判定が「自分自身」を相手に double 成立してしまう。
        // press を先に評価すれば last_click は前フレーム由来の値のままなので誤検出しない。
        // last_click を触らないので、別フレームの真の double-click では press (こちら) と release (下)
        // の両方が立つが、消費者が別 (press=piano_roll / release=arrangement 等) なので衝突しない。
        // last_click のクリア (triple-click 抑制) は従来どおり release ベース側が担う。
        let mut pending_double_click_press: Option<(f32, f32)> = None;
        if pointer.primary_just_pressed
            && let Some((px, py)) = pointer.pos
        {
            let now = std::time::Instant::now();
            let (max_dur, max_px) = self.double_click_threshold;
            let is_double = self.last_click.is_some_and(|(t, lx, ly)| {
                now.duration_since(t) < max_dur && (px - lx).hypot(py - ly) < max_px
            });
            if is_double {
                pending_double_click_press = Some((px, py));
            }
        }

        let mut pending_double_click: Option<(f32, f32)> = None;
        if pointer.primary_just_released
            && let Some((px, py)) = pointer.pos
        {
            let now = std::time::Instant::now();
            let (max_dur, max_px) = self.double_click_threshold;
            let is_double = self.last_click.is_some_and(|(t, lx, ly)| {
                now.duration_since(t) < max_dur && (px - lx).hypot(py - ly) < max_px
            });
            if is_double {
                pending_double_click = Some((px, py));
                // 連続 3 click 誤動作防止のため last_click を None に
                self.last_click = None;
            } else {
                self.last_click = Some((now, px, py));
            }
        }

        // daw_01 r.md #35: 右ボタンの **click (= context menu)** と **drag (= 矩形選択)** の分離。
        // press 位置を覚えておき、release frame に移動量が double-click 閾値の px 未満なら
        // 「右クリック」 と確定して `pending_secondary_click` を立てる。 動いていれば右ドラッグ
        // だった (= `take_secondary_drag_rect_in_rect` が既に矩形を返している) ので click は
        // 立てない。 pos 不明の release も click 扱いにしない。
        if pointer.secondary_just_pressed {
            self.secondary_press_pos = pointer.pos;
        }
        let mut pending_secondary_click: Option<(f32, f32)> = None;
        if pointer.secondary_just_released {
            if let Some((sx, sy)) = self.secondary_press_pos.take()
                && let Some((px, py)) = pointer.pos
                && (px - sx).hypot(py - sy) < self.double_click_threshold.1
            {
                // anchor は press 位置。 release 位置は最大 5px ずれるので、 menu が
                // 「押した場所」 に出る方が自然 (Explorer / REAPER と同じ)。
                pending_secondary_click = Some((sx, sy));
            }
            self.secondary_press_pos = None;
        }

        let cursor = Rect::new(0.0, 0.0, screen.width as f32, screen.height as f32);
        let focused_at_start = self.focused;
        let mut seen_widgets: HashSet<WidgetId> = HashSet::new();
        let mut redraw_requested = false;
        let mut typing_focus = false;
        let mut focus_order: Vec<(WidgetId, Rect)> = Vec::new();

        // M14 Phase 94 (daw_01 #065): 「真のモーダル」。`capture_input == true` な modal popup が
        // 開いていれば、background widget (`drawing_in_popup == false`) が読む pointer を
        // masking する。`pointer_raw` は popup_layer の close 判定 / modal body 用に温存。
        let modal_capturing = self
            .open_popups
            .values()
            .any(|s| s.modal && s.capture_input);
        // resource monitor (r.md #3): keyboard 遮断は capture_keyboard も要求する
        // (= overlay panel は pointer だけ mask、 Space 等の shortcut は background に通す)。
        let modal_capturing_keyboard = self
            .open_popups
            .values()
            .any(|s| s.modal && s.capture_input && s.capture_keyboard);
        let effective_pointer = if modal_capturing { masked_pointer(pointer) } else { pointer };

        // r.md #48: ウィンドウの clear 色はパレットの床。renderer 側の `Scene::DEFAULT_CLEAR`
        // は「app が何も言わなかったときの中立値」 に留め、意味づけは ui 層が行う。
        // これを入れないと、panel の隙間・レイアウト余白だけが旧テーマの色で残る。
        scene.clear_color = self.palette.window_bg.to_wgpu();

        let mut ui = Ui {
            palette: &self.palette,
            state: &mut self.state,
            scene,
            edits: &mut edits,
            pointer: effective_pointer,
            pointer_raw: pointer,
            modal_capturing,
            modal_capturing_keyboard,
            keyboard_events: &mut keyboard_events,
            ime_events: &mut ime_events,
            cursor,
            screen,
            next_y: 0.0,
            focused: focused_at_start,
            pending_focus: focused_at_start,
            focus_changed_this_frame: false,
            ime_request: None,
            text_document: None,
            scenegraph: &mut self.scenegraph,
            seen_widgets: &mut seen_widgets,
            current_clip: None,
            open_popups: &mut self.open_popups,
            popup_primitives: Vec::new(),
            drawing_in_popup: false,
            redraw_requested: &mut redraw_requested,
            file_drop,
            file_hover,
            pending_shortcuts: &mut pending_shortcuts,
            typing_focus: &mut typing_focus,
            text_metrics: &mut self.text_metrics,
            font_system,
            shortcut_map: &self.shortcut_map,
            pending_clipboard_paste,
            pending_clipboard_writes: &mut self.transient_clipboard_writes,
            pending_dialog_requests: &mut self.transient_dialog_requests,
            consumed_dialog_results: &mut self.transient_consumed_dialog_results,
            dialog_results: &self.pending_dialog_results,
            focus_order: &mut focus_order,
            pending_cursor: &mut self.transient_cursor,
            pending_cursor_pos: &mut self.transient_cursor_pos,
            cache_hits: &mut self.current_cache_hits,
            cache_misses: &mut self.current_cache_misses,
            last_frame_stats: self.last_frame_stats,
            pending_double_click: &mut pending_double_click,
            pending_double_click_press: &mut pending_double_click_press,
            pending_secondary_click: &mut pending_secondary_click,
            _m: PhantomData,
        };
        f(model, &mut ui);

        // M8 Phase 30: Tab / arrow focus traversal。
        // pending_shortcuts に "tab_next" 等が残っていれば (= widget が consume 済でなければ)、
        // focus_order (このフレームに登録された focusable 一覧) から次の wid を選んで set_focus。
        // M14 Phase 94 (daw_01 #065): 真のモーダル中も traversal 自体は動かす (focus_order は
        // `focusable` guard で modal panel 内 widget のみに絞られている) ため、guard を通さない
        // `take_shortcut_raw` で消費する。
        let focus_order_snapshot: Vec<(WidgetId, Rect)> = ui.focus_order.clone();
        if !focus_order_snapshot.is_empty() {
            let tab_next = ui.take_shortcut_raw("tab_next");
            let tab_prev = ui.take_shortcut_raw("tab_prev");
            let focus_up = ui.take_shortcut_raw("focus_up");
            let focus_down = ui.take_shortcut_raw("focus_down");
            let focus_left = ui.take_shortcut_raw("focus_left");
            let focus_right = ui.take_shortcut_raw("focus_right");

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
        let popup_primitives = std::mem::take(&mut ui.popup_primitives);
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
        // M15: text store document の commit (focus 中 text_input が publish していれば Some)。
        self.last_text_document = ui.text_document.take();
        drop(ui);
        // widget からの request_redraw 累積を commit (ui drop 後に local 変数を読む)。
        self.redraw_requested_in_last_frame = redraw_requested;
        // M14 Phase 57: 次フレームの shortcut layer (typing-only shortcut の global 抑制)
        // のために、今フレームに `Ui::set_typing_focus(true)` が立ったかを記録。
        self.last_typing_focus = typing_focus;
        // ui が drop して scene の borrow が外れた後で、popup buffer を Scene の popup pass 用
        // フィールドに移す (renderer は base pass の後に popup pass で再描画する設計、
        // pipeline 順 rect→line→glyph 起因の z-order 問題を解消)。
        scene.popup_primitives.extend(popup_primitives);
        // M4 Phase 11: 今フレームに登場しなかった widget を scenegraph から eviction。
        self.scenegraph.retain(&seen_widgets);
        // M8 Phase 30: 次フレーム用に focusable 一覧を保存。
        self.last_focusable = focus_order_snapshot;
        // M9 Phase 43: frame stats を確定 (debug overlay は次フレームでこれを read)。
        self.last_frame_stats = FrameStats {
            cache_hits: self.current_cache_hits,
            cache_misses: self.current_cache_misses,
            widget_count: u32::try_from(seen_widgets.len()).unwrap_or(u32::MAX),
            scenegraph_size: u32::try_from(self.scenegraph.len()).unwrap_or(u32::MAX),
        };
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
/// M8 で transient bool 群 (typing_focus + 既存の focus_changed_this_frame / drawing_in_popup)
/// が増えたが、それぞれが「frame 内で 1 度だけ書かれる / 読まれる」フラグで意味が独立している
/// ため、`clippy::struct_excessive_bools` を allow する (state machine 化はオーバーヘッド過大)。
#[allow(clippy::struct_excessive_bools)]
pub struct Ui<'a, M: ?Sized + 'static> {
    /// r.md #48: このフレームで有効な色パレット (`UiHost` 所有の実体への借用)。
    /// widget は [`Ui::palette`] から読む。
    palette: &'a Palette,
    state: &'a mut HashMap<WidgetId, Box<dyn WidgetState>>,
    scene: &'a mut Scene,
    edits: &'a mut Vec<Edit<M>>,
    /// widget が読む pointer。`modal_capturing` 中の background 描画 (`drawing_in_popup ==
    /// false`) では `masked_pointer` に差し替わり (pos = None / 全 button false / scroll 0)、
    /// `popup_layer` の body 内 (`drawing_in_popup == true`) では `pointer_raw` に戻る。
    /// fader 等 `self.pointer` を直読みする widget も、この 1 箇所の差し替えで自動的に inert
    /// になる (SSoT、daw_01 #065)。
    pub(crate) pointer: PointerFrame,
    /// M14 Phase 94 (daw_01 #065): masking 前の生 pointer。`popup_layer` の outside-click /
    /// anchor 消費判定と、modal body へ raw を渡す swap で使う。`pointer` が masking されても
    /// modal 自身の close 挙動 (outside click / ESC) は不変であることを保証する。
    pub(crate) pointer_raw: PointerFrame,
    /// M14 Phase 94 (daw_01 #065): `capture_input == true` な modal popup が 1 つ以上開いて
    /// いるか (frame 頭に確定)。`true` の間、background widget への pointer / keyboard 入力を
    /// 遮断する (= 真のモーダル)。
    pub(crate) modal_capturing: bool,
    /// resource monitor (r.md #3): `modal_capturing` のうち keyboard も遮断するか
    /// (capture_keyboard==true な真のモーダルが開いている)。 overlay panel は false なので
    /// pointer は mask されても shortcut (Space 等) は background に通る。
    pub(crate) modal_capturing_keyboard: bool,
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
    /// **(M15)** このフレーム内で focus 中 text_input が `Ui::publish_text_document` した snapshot。
    /// frame 末尾で `UiHost::last_text_document` に commit され、OS text store (TSF) へ publish。
    /// `None` = 編集対象なし。
    text_document: Option<TextDocument>,
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
    /// drawing_in_popup 中の primitive 列 (`Primitive::Rect/Glyph/Line` の混在)。
    /// frame 末で `scene.popup_primitives` に move する。
    popup_primitives: Vec<Primitive>,
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
    /// M14 Phase 58: text shape による実 advance 計算器 (`Ui::measure_text` 経由でアクセス)。
    pub(crate) text_metrics: &'a mut TextMetrics,
    /// 測定用 FontSystem (frame ごとに注入される)。`Ui::measure_text` が `text_metrics` と
    /// 組で使う。renderer 所有 (runner) か UiHost lazy 所有 (headless) のいずれか。
    pub(crate) font_system: &'a mut FontSystem,
    /// shortcut display_for などの query 用 (mutable は UiHost::shortcut_map_mut 経由)。
    pub(crate) shortcut_map: &'a ShortcutMap,
    // ---- M8 Phase 31 clipboard ----
    /// frame 頭に paste shortcut が match していれば provider から read 済み。`take_clipboard_paste`
    /// で widget が 1 度だけ取り出す。
    pub(crate) pending_clipboard_paste: Option<String>,
    /// `set_clipboard_text` で積まれる write リクエスト (frame 末尾に provider.set_text)。
    pub(crate) pending_clipboard_writes: &'a mut Vec<String>,
    // ---- M8 Phase 34 dialog ----
    pub(crate) pending_dialog_requests: &'a mut Vec<DialogRequest>,
    pub(crate) consumed_dialog_results: &'a mut HashSet<&'static str>,
    pub(crate) dialog_results: &'a HashMap<&'static str, DialogResult>,
    // ---- M8 Phase 30 focus traversal ----
    /// このフレームに `Ui::focusable(wid, rect)` で登録された一覧。
    /// frame 末尾で UiHost に保存し、Tab / arrow nav の対象にする。
    pub(crate) focus_order: &'a mut Vec<(WidgetId, Rect)>,
    // ---- M9 Phase 41b cursor ----
    /// このフレーム末尾に OS に伝える cursor (last call wins)。
    /// `Ui::set_cursor` で書き、`UiHost::frame` 末尾で `set_cursor_request` callback に流す。
    pub(crate) pending_cursor: &'a mut Option<CursorIcon>,
    /// このフレーム末尾に OS に warp させる cursor 位置 (物理 px、last call wins)。
    /// `Ui::warp_cursor` で書き、`UiHost::frame` 末尾で `set_cursor_pos_request` callback に流す。
    pub(crate) pending_cursor_pos: &'a mut Option<(f32, f32)>,
    // ---- M9 Phase 43 debug stats ----
    /// このフレームで `with_widget_node` cache hit した回数 (描画スキップ数)。
    pub(crate) cache_hits: &'a mut u32,
    /// このフレームで `with_widget_node` cache miss して draw_fn を実行した回数。
    pub(crate) cache_misses: &'a mut u32,
    /// 直近フレームの統計 (debug_overlay 表示用、Copy 値)。
    pub(crate) last_frame_stats: FrameStats,
    // ---- M9 P1-4 double-click ----
    /// このフレームで double-click が判定されていれば release 位置 (primary_just_released
    /// の座標)。`take_double_click_in_rect(rect)` が rect.contains で 1 度だけ消費する。
    pub(crate) pending_double_click: &'a mut Option<(f32, f32)>,
    /// このフレームで「double-click の 2 度目の press」が判定されていれば press 位置。
    /// `take_double_click_press_in_rect(rect)` が rect.contains で 1 度だけ消費する。
    /// release ベースの `pending_double_click` と独立 (押下のまま drag を始める用)。
    pub(crate) pending_double_click_press: &'a mut Option<(f32, f32)>,
    // ---- daw_01 r.md #35: 右クリック (drag でない) ----
    /// このフレームで「右ボタンを押して動かさずに離した」が判定されていれば press 位置。
    /// `take_secondary_click_in_rect(rect)` が rect.contains で 1 度だけ消費する。
    /// 右ドラッグ (= 矩形選択) だったフレームでは立たない。
    pub(crate) pending_secondary_click: &'a mut Option<(f32, f32)>,
    _m: PhantomData<&'a M>,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    pub fn screen(&self) -> PhysicalSize {
        self.screen
    }

    /// r.md #48: このフレームの色パレット。**widget が色を得る唯一の口**。
    ///
    /// 戻りの寿命は `&self` ではなく **host の `'a`** に紐づけてある。おかげで
    /// `let p = ui.palette();` の後に `ui.push_rect(...)` を呼べる (借用衝突しない)。
    #[must_use]
    pub fn palette(&self) -> &'a Palette {
        self.palette
    }

    pub fn pointer(&self) -> PointerFrame {
        self.pointer
    }

    /// 現在の scissor (with_clip_rect スタック top) を返す。外部 widget (daw_gui の arrangement 等) が
    /// closure 形の `with_clip_rect` を使えない interleave 描画で、scissor を open-code で push/pop する
    /// ための read アクセサ (旧 `pub(crate) current_clip` フィールド直読の置換)。
    #[must_use]
    pub fn current_clip_rect(&self) -> Option<Rect> {
        self.current_clip
    }

    /// 現在の scissor を差し替える (`current_clip_rect` の対、push/pop の write 側)。呼び出し側が
    /// 直前値を保持して復元する責務を負う (`with_clip_rect` と同 idiom を open-code するとき用)。
    pub fn set_current_clip_rect(&mut self, clip: Option<Rect>) {
        self.current_clip = clip;
    }

    /// 描画コマンドを Scene に積む (外部 widget extension で利用可能)。
    /// M7 Phase 22: `current_clip` (with_clip_rect スタック) と cmd 自身の clip_rect を交差させて
    /// renderer に渡す (cmd 自身が `Some` の場合は intersect、`None` の場合は current_clip)。
    /// M7 Phase 25: `drawing_in_popup` 中は popup_buffer に積む (frame 末尾で z-order 最前面)。
    pub fn push_rect(&mut self, mut cmd: RectCommand) {
        cmd.clip_rect = merge_clip(self.current_clip, cmd.clip_rect);
        if self.drawing_in_popup {
            self.popup_primitives.push(Primitive::Rect(cmd));
        } else {
            self.scene.push_rect(cmd);
        }
    }

    /// テキスト描画を Scene に積む (外部 widget extension で利用可能)。
    pub fn push_text(&mut self, mut area: GlyphArea) {
        area.clip_rect = merge_clip(self.current_clip, area.clip_rect);
        if self.drawing_in_popup {
            self.popup_primitives.push(Primitive::Glyph(area));
        } else {
            self.scene.push_text(area);
        }
    }

    /// 線分バッチを Scene に積む (波形・メータ・グリッド、外部 widget extension で利用可能)。
    pub fn push_lines(&mut self, mut batch: LineBatch) {
        batch.clip_rect = merge_clip(self.current_clip, batch.clip_rect);
        if self.drawing_in_popup {
            self.popup_primitives.push(Primitive::Line(batch));
        } else {
            self.scene.push_lines(batch);
        }
    }

    /// M14 Phase 71 (daw_01 #043): textured quad を Scene に積む。
    /// `current_clip` (with_clip_rect スタック) と quad.clip_rect を交差させる。
    /// popup 内で呼ぶと popup_primitives に入るが、 popup pass では texture pipeline を
    /// 持たないため render されない (#043 reply: 「video preview は popup ではない」)。
    pub fn push_textured_quad(&mut self, mut quad: TexturedQuad) {
        quad.clip_rect = merge_clip(self.current_clip, quad.clip_rect);
        if self.drawing_in_popup {
            self.popup_primitives.push(Primitive::Texture(quad));
        } else {
            self.scene.push_textured_quad(quad);
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
    ///
    /// menu / dropdown / context_menu はこの経路で開き `capture_input == false` (= 従来の
    /// 「panel の裏に隠れた widget だけ抑制」)。画面全体の入力遮断 (真のモーダル) が要るのは
    /// dialog だけなので、それは [`Ui::open_modal`] (= `capture_input == true`) を使う。
    pub fn open_popup(&mut self, id: impl std::hash::Hash, anchor: Rect, modal: bool) {
        self.open_popup_inner(id, anchor, modal, false, false);
    }

    /// resource monitor (r.md #3): 非モーダルな overlay panel を開く。 pointer は
    /// masking する (panel 上の click が背後の widget に突き抜けない) が、 keyboard /
    /// shortcut は background に通す (= Space 再生等が効く)。 暗転 backdrop も描かない
    /// (描画は呼び出し側の `popup_layer` 内に委ねる)。 Performance パネル等の「再生を
    /// 止めず最前面に重ねる panel」用。 panel 外 click / Esc で閉じる挙動は menu と同じ。
    /// anchor は呼び出し側が `update_popup_anchor(("overlay", id), panel_rect)` で更新する。
    pub fn open_overlay(&mut self, id: impl std::hash::Hash) {
        self.open_popup_inner(
            ("overlay", &id),
            Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
            true,
            true,
            false,
        );
    }

    /// overlay panel を閉じる。
    pub fn close_overlay(&mut self, id: impl std::hash::Hash) {
        self.close_popup(("overlay", &id));
    }

    /// overlay panel が開いているか。
    #[must_use]
    pub fn is_overlay_open(&self, id: impl std::hash::Hash) -> bool {
        self.is_popup_open(("overlay", &id))
    }

    /// 常駐 floating panel (daw_01 の編集履歴 window 等) が、 **自分の rect 分だけ**
    /// 背後の pointer を占有するための予約。 `open_overlay` (= 開いている間、 背景
    /// 全体の pointer を mask する capture_input modal) と違い、 pointer が `rect` の
    /// **外** にある間は背景 widget を通常どおり操作できる (= 真の非ブロッキング
    /// floating window)。
    ///
    /// 使い方 (2 段): build_root の **背景 widget 描画より前** に、 panel が開いて
    /// いるとき panel rect で本メソッドを呼ぶ。 raw pointer が `rect` 内にある間だけ
    /// `self.pointer` を mask し、 以降に描く背景 widget (arrangement 等) がその
    /// click / hover / scroll に反応しないようにする (= panel 上のクリックが背後に
    /// 漏れない)。 panel 自身の描画は末尾で [`Ui::with_floating_region`] に包む。
    ///
    /// 真のモーダル (`modal_capturing`) が開いている間は何もしない (モーダルが優先し、
    /// panel はその背後で inert)。
    pub fn reserve_floating_region(&mut self, rect: Rect) {
        if !self.modal_capturing
            && self
                .pointer_raw
                .pos
                .is_some_and(|(x, y)| rect.contains(x, y))
        {
            self.pointer = masked_pointer(self.pointer_raw);
        }
    }

    /// [`Ui::reserve_floating_region`] とペア。 floating panel 自身の描画をこの
    /// closure 内で行う。 panel 内 widget は **raw pointer** を読む (背後 mask を
    /// 一時解除) ので drag / click / scroll が効く。 真のモーダル中は解除しない
    /// (panel はモーダルの背後で操作不可のまま)。
    pub fn with_floating_region<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let saved = self.pointer;
        if !self.modal_capturing {
            self.pointer = self.pointer_raw;
        }
        let r = f(self);
        self.pointer = saved;
        r
    }

    /// M14 Phase 94 (daw_01 #065): `capture_input` を指定して popup を開く内部 API。
    /// `Ui::open_modal` が `capture_input = true` で呼ぶ。 M14 Phase 114 (daw_01 #087): `color_picker`
    /// も `capture_input = true` で開く (SV/Hue drag の press を背景 widget に先取りされないため)。
    pub(crate) fn open_popup_inner(
        &mut self,
        id: impl std::hash::Hash,
        anchor: Rect,
        modal: bool,
        capture_input: bool,
        capture_keyboard: bool,
    ) {
        let wid = WidgetId::ROOT.child((b"popup", &id));
        let prev_focus = self.pending_focus;
        self.open_popups.insert(
            wid,
            // M14 Phase 95 (daw_01 #066): dismiss_on_outside_click は default true
            // (menu / dropdown / 通常 modal の従来挙動)。modal は `Ui::modal` が毎フレーム
            // `ModalStyle::close_on_outside_click` から同期して上書きする。
            PopupOpenState {
                anchor,
                modal,
                prev_focus,
                capture_input,
                capture_keyboard,
                dismiss_on_outside_click: true,
            },
        );
    }

    /// M14 Phase 95 (daw_01 #066): 開いている popup の `dismiss_on_outside_click` を更新する。
    /// `Ui::modal` が毎フレーム `ModalStyle::close_on_outside_click` を同期するために使う
    /// (popup が閉じていれば no-op)。
    pub(crate) fn set_popup_dismiss_on_outside_click(
        &mut self,
        id: impl std::hash::Hash,
        dismiss: bool,
    ) {
        let wid = WidgetId::ROOT.child((b"popup", &id));
        if let Some(state) = self.open_popups.get_mut(&wid) {
            state.dismiss_on_outside_click = dismiss;
        }
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

    /// open な popup が 1 つでもあるか (O(1)、 無 alloc)。 menu_bar の cascade orphan cleanup
    /// (M14 Phase 120、 daw_01 #095) を popup が皆無の idle frame で早期 skip して、 closed menu
    /// 毎フレームの id_path `format!` を avoid するために使う。 cascade が orphan するのは必ず
    /// `open_popups` 非空 (= orphan 自身が居る) の状態なので、 空なら cleanup は確実に no-op。
    ///
    /// `pub`: context menu (`open_popup` は `capture_input == false` で background pointer を
    /// mask しない) が開いている間、 背景 widget (arrangement 等) が menu item click を
    /// 誤って自分への click として処理し選択をクリアするのを防ぐガードに使う (daw_gui 側)。
    pub fn has_open_popups(&self) -> bool {
        !self.open_popups.is_empty()
    }

    /// `id` で開いている popup の anchor (= popup の内容領域) を返す。
    /// closure 内で popup_rect を再計算する代わりに使える (例: context_menu の動的位置を
    /// open_popup 時の pointer 位置に固定したい場合)。
    pub fn popup_anchor(&self, id: impl std::hash::Hash) -> Option<Rect> {
        let wid = WidgetId::ROOT.child((b"popup", &id));
        self.open_popups.get(&wid).map(|s| s.anchor)
    }

    /// 開いている popup の `anchor` だけを更新する (focus 復元情報 `prev_focus` は維持)。
    /// 画面サイズ変化等で popup の位置が動くケース (例: modal の中央配置を毎フレーム
    /// 再計算したい) で使う。popup が閉じていれば no-op。
    pub fn update_popup_anchor(&mut self, id: impl std::hash::Hash, anchor: Rect) {
        let wid = WidgetId::ROOT.child((b"popup", &id));
        if let Some(state) = self.open_popups.get_mut(&wid) {
            state.anchor = anchor;
        }
    }

    /// popup の内容を描画する。popup が開いていなければ closure は呼ばれない。
    /// closure 内で push される primitive は **deferred buffer** に積まれ、frame 末尾で
    /// base scene に append (z-order = 最前面)。
    ///
    /// modal popup の click consumption ルール:
    /// - `anchor` の **外** で `primary_just_pressed` → popup close + click 消費 (closure 実行せず)
    /// - `anchor` の **外** で `secondary_just_pressed` (右クリック) → popup close (**消費しない**)。
    ///   右クリックは「今のメニューを閉じて、同じ右クリックで別のコンテキストメニューを開く」
    ///   (close-old / open-new、DAW 標準) を成立させるため consume しない (M14 Phase 100、#071 review)
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

        // outside-click 検出 (closure 実行前に判定 / 自動 close)。
        // daw_01 #038 fix: 「outside」 = 自分の anchor 外 **かつ 他の open popup の anchor 外**。
        // cascade menu / context_menu_for は親 popup_layer の closure 内で子 popup_layer を
        // 呼ぶ構造で、 cascade item (= 子 anchor 内 / 親 anchor 外) の click を親が outside
        // 扱いで握りつぶし子 closure が走らない致命的 bug があった。 全 open popup の集合を
        // 1 つの「popup 領域」 として扱う形に緩める。
        //
        // M14 Phase 94 (daw_01 #065): close 判定は **生 pointer** (`popup_pointer`) で行う。
        // 真のモーダル中は `self.pointer` が masking されて pos = None になっているため、
        // ここで masked pointer を読むと outside-click close / ESC が効かなくなる。
        // M14 Phase 100 (#071 review): primary だけでなく **secondary (右) press** も outside-click
        // 扱いにする。右クリックで開いたコンテキストメニューを、別の場所の右クリックで閉じられない
        // (popup が居残る / 二重に開く) 既存 UX 制約の解消。右クリックは「outside で close」 まで行うが
        // **consume はしない** (下記、 close-old / open-new を同 press で成立させる)。
        let pp = self.popup_pointer();
        let secondary_outside = pp.secondary_just_pressed;
        let outside_click = (pp.primary_just_pressed || secondary_outside)
            && pp.pos.is_some_and(|(px, py)| {
                !self
                    .open_popups
                    .values()
                    .any(|s| s.anchor.contains(px, py))
            });
        if outside_click {
            if state.dismiss_on_outside_click {
                // popup を閉じる。primary の場合のみ click 消費 (modal なら他 widget に流さない)。
                // secondary (右) は **消費しない** — 同じ右クリックが別の context_menu を開く /
                // arrangement の SecondaryClickEmpty を発火する余地を残し、close-old / open-new を
                // 成立させる (右クリックの dismiss は「閉じる」 のみで gesture を奪わない)。
                self.open_popups.remove(&wid);
                self.pending_focus = state.prev_focus;
                self.focus_changed_this_frame = true;
                if state.modal && !secondary_outside {
                    self.consume_pointer_click();
                }
                return;
            }
            // M14 Phase 95 (daw_01 #066): close_on_outside_click == false の blocking modal は
            // 外 click で閉じない。click は consume して無視するだけ (capturing modal では背景は
            // 既に masking 済だが、 popup_layer の close 機構へ生 click が再到達しないよう両 pointer
            // を消す) で、 **early return せず body をそのまま描画**する。早期 return すると overlay /
            // panel が 1 frame 描かれず「閉じて再 open」のフラッシュになる (#066 の症状)。
            if state.modal {
                self.consume_pointer_click();
            }
        }

        // popup の内容を描画 (deferred buffer)
        // M14 Phase 63a (#014): popup overlay は z-order 最前面の modal なので、 base scene
        // の clip 制約 (= caller が `with_clip_rect(pane_rect, ..)` で囲んだ pane) から免除する。
        // 退避しないと `push_rect/push_text/push_lines` が `merge_clip(current_clip, ..)` を
        // 適用して popup primitive が pane_rect で clip され、 画面上に出ても見えなくなる
        // (piano_roll snap dropdown が tab pane 内で消える regression を起こした)。
        //
        // M14 Phase 94 (daw_01 #065): 真のモーダル中 (`modal_capturing`) は background 向けに
        // masking された `self.pointer` を body の間だけ生 pointer に戻す (= panel 内 widget は
        // 通常どおり動く)。body 終了後に再 masking する。consume が body 内で起きても
        // `consume_pointer_click` が `pointer_raw` も消すため再 mask しても消費は保たれる。
        // **この popup が capturing modal 自身のとき** だけ body を un-mask する。
        // `state.capture_input` を見ないと、capturing modal と **同時に開いている** background の
        // 非 capturing popup (menu / dropdown / context_menu = `capture_input == false`) の body まで
        // un-mask されてしまい、その popup item が hover / click に反応してしまう (真のモーダル違反、
        // M14 Phase 94 review で発覚)。
        let masked_here = self.modal_capturing && !self.drawing_in_popup && state.capture_input;
        let saved_pointer = if masked_here {
            let s = self.pointer;
            self.pointer = self.pointer_raw;
            Some(s)
        } else {
            None
        };
        let prev_in_popup = self.drawing_in_popup;
        let prev_clip = self.current_clip;
        self.drawing_in_popup = true;
        self.current_clip = None;
        f(self);
        self.drawing_in_popup = prev_in_popup;
        self.current_clip = prev_clip;
        if let Some(s) = saved_pointer {
            self.pointer = s;
        }

        // modal popup が open しているフレーム中、anchor 内 click は popup item として
        // 既に処理済 → 下層の widget に同じ click が流れないよう消費する。
        // (popup item handler が close_popup を呼んだ場合も same frame で消費)
        // 生 pointer (`popup_pointer`) で判定する (上の outside-click と同じ理由)。
        if state.modal && pp.pos.is_some_and(|(px, py)| state.anchor.contains(px, py)) {
            self.consume_pointer_click();
        }
    }

    /// M14 Phase 94 (daw_01 #065): `popup_layer` の close / 消費判定が読むべき pointer。
    /// 真のモーダル中の background 描画では `self.pointer` が masking されているので
    /// `pointer_raw` を返し、それ以外 (= non-capturing popup や、body 内で既に raw へ
    /// swap 済) では消費 (`consume_pointer_click`) を反映した `self.pointer` を返す。
    fn popup_pointer(&self) -> PointerFrame {
        if self.modal_capturing && !self.drawing_in_popup {
            self.pointer_raw
        } else {
            self.pointer
        }
    }

    /// M14 Phase 94 (daw_01 #065): 真のモーダル中で background widget (popup body の外) の
    /// keyboard / shortcut / focus 入力を遮断すべきか。`drawing_in_popup == true` (= modal の
    /// body / 内部 internal traversal は別 API) では遮断しない。
    fn keyboard_blocked_by_modal(&self) -> bool {
        self.modal_capturing_keyboard && !self.drawing_in_popup
    }

    /// pointer が **modal popup の anchor 内** にあり、現在の widget が `drawing_in_popup`
    /// でない (= popup_layer の外で動いている) とき `true`。
    ///
    /// daw_01 #015 の root cause: arrangement_view が plugin_picker (modal) より先に走り
    /// `take_scroll_in_rect(lanes)` が pointer (modal panel 内) の scroll_delta を消費 →
    /// list_view が呼ぶ頃には (0, 0)。modal の下に隠れている widget は pointer 入力を
    /// 一切消費すべきでない (overlay の意味が失われる) ため、`take_scroll_in_rect` /
    /// `take_drag_rect_in_rect` / `take_double_click_in_rect` 冒頭で早期 return する。
    ///
    /// popup_layer 内部 (= modal の body) では `drawing_in_popup == true` なので false を
    /// 返し、通常通り消費可能。
    pub(crate) fn pointer_blocked_by_modal_popup(&self) -> bool {
        if self.drawing_in_popup {
            return false;
        }
        // M14 Phase 94 (daw_01 #065): 真のモーダル中は anchor 内外を問わず全 background を遮断。
        // `self.pointer` は既に masking されているので大半の take_* は pos = None で自然に
        // 何も返さないが、`take_double_click_in_rect` (pending_double_click は生 pointer 由来) /
        // `take_file_drop_in_rect` (drop 位置で判定) はこの早期 return で確実に止める。
        if self.modal_capturing {
            return true;
        }
        let Some((px, py)) = self.pointer.pos else {
            return false;
        };
        self.open_popups
            .values()
            .any(|s| s.modal && s.anchor.contains(px, py))
    }

    /// エディットを Scene に積む (外部 widget extension で利用可能)。
    pub fn push_edit(&mut self, edit: Edit<M>) {
        self.edits.push(edit);
    }

    /// M11 Phase 52: `wid` が前フレームの描画 (= `with_widget_node` 経由) に登場していたかを返す。
    /// `text_input_at_focused` 等が「初回 show」判定に使う。
    /// frame 末尾の `scenegraph.retain(&seen)` で eviction されるため、このフレーム途中で
    /// 呼んだとき `true` ⇔ 「前フレームに登場した」。
    pub(crate) fn was_widget_visible_last_frame(&self, wid: WidgetId) -> bool {
        self.scenegraph.contains(wid)
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

        // hash 一致 → cached primitives を call order で scene 末尾に append、draw_fn は実行しない。
        if let Some(cached) = self.scenegraph.get_cached(wid, input_hash) {
            if self.drawing_in_popup {
                self.popup_primitives
                    .extend(cached.primitives.iter().cloned());
            } else {
                self.scene
                    .primitives
                    .extend(cached.primitives.iter().cloned());
            }
            *self.cache_hits += 1;
            return;
        }
        *self.cache_misses += 1;

        // miss → draw_fn を実行して scene / popup 末尾の差分を新規 commands として記録。
        let p0 = if self.drawing_in_popup {
            self.popup_primitives.len()
        } else {
            self.scene.primitives.len()
        };
        draw_fn(self);
        let new_primitives: Vec<Primitive> = if self.drawing_in_popup {
            self.popup_primitives[p0..].to_vec()
        } else {
            self.scene.primitives[p0..].to_vec()
        };
        let commands = CachedCommands { primitives: new_primitives };
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
        // M14 Phase 94 (daw_01 #065): 真のモーダル中は `self.pointer` が masking された別 copy で、
        // popup_layer の close / anchor 判定は `pointer_raw` を読む。消費を両方に反映しないと
        // 「modal body で処理した click が popup_layer 出口 / 兄弟 popup で生きたまま」になる。
        self.pointer_raw.primary_just_pressed = false;
        self.pointer_raw.primary_just_released = false;
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
    // M9 Phase 41b: cursor 形状要求
    // ============================================================

    /// このフレーム末尾に OS カーソル形状を変更要求する。同フレーム内で複数回呼ばれた
    /// 場合は **last call wins** (= 直前 widget の要求は捨てられる)。
    ///
    /// `WindowBackend::set_cursor` callback が registered されていなければ no-op
    /// (low-level constructor [`UiHost::new`] / [`UiHost::no_redraw`] で構築した場合)。
    /// 通常 [`UiHost::with_window`] で構築すれば自動的に有効化される。
    ///
    /// `set_cursor` を呼ばなかったフレームは前フレームの形状が OS 側に保持されたまま
    /// (winit は state-full)。reset したい場合は明示的に `set_cursor(CursorIcon::Default)`
    /// を呼ぶこと。
    pub fn set_cursor(&mut self, cursor: CursorIcon) {
        *self.pending_cursor = Some(cursor);
    }

    /// このフレーム末尾に OS カーソルを `(x, y)` (物理 px、ウィンドウ client 座標) へ
    /// warp 要求する。同フレーム内で複数回呼ばれた場合は **last call wins**。
    ///
    /// `WindowBackend::set_cursor_position` callback が registered されていなければ no-op
    /// ([`UiHost::new`] / [`UiHost::no_redraw`] 構築時)。通常 [`UiHost::with_window`] で有効化。
    ///
    /// 用途: ノート作成ドラッグでカーソルを既定長ノートの右端へ移動する (Ableton Live 流)。
    /// warp は OS event として非同期に反映される (次フレーム以降に `PointerMoved` が届く) ため、
    /// caller は warp 着地を検出するロジック (例: press 位置と warp 先の中点を越えたか) を持つこと。
    pub fn warp_cursor(&mut self, x: f32, y: f32) {
        *self.pending_cursor_pos = Some((x, y));
    }

    // ============================================================
    // M9 Phase 43: debug overlay
    // ============================================================

    /// 直近フレームの統計を `rect` の右上に半透明 overlay として描画する。
    ///
    /// `frame_ms` は app 側で測定した frame の所要時間 (window backend / render pipeline
    /// により計測方法が違うので library は track せず引数で受ける)。`0.0` を渡せば省略。
    ///
    /// 表示項目:
    /// - frame: `{frame_ms:.2}ms` (引数 `frame_ms` < 1e-6 なら省略)
    /// - cache: `{hits} / {hits+misses}` + ヒット率 `{rate:.0}%`
    /// - widgets: `{widget_count}` (scenegraph_size と通常一致)
    ///
    /// 統計は **前フレーム** の値 (今フレームは描画中でまだ確定していない)。`Ui::take_shortcut`
    /// を組み合わせると Ctrl+F1 で toggle できる:
    /// ```ignore
    /// if ui.take_shortcut("debug_overlay_toggle") {
    ///     m.show_debug = !m.show_debug;
    /// }
    /// if m.show_debug {
    ///     ui.debug_overlay(area, last_frame_ms);
    /// }
    /// ```
    pub fn debug_overlay(&mut self, rect: Rect, frame_ms: f32) {
        let stats = self.last_frame_stats;
        let line_h = 14.0;
        let pad = 6.0;
        let font_size = 11.0;
        let lines: Vec<String> = {
            let mut v = Vec::with_capacity(5);
            if frame_ms.abs() > 1e-6 {
                v.push(format!("frame  {frame_ms:>5.2}ms"));
            }
            let total = stats.cache_hits + stats.cache_misses;
            v.push(format!(
                "cache  {} / {} ({:>3.0}%)",
                stats.cache_hits,
                total,
                stats.cache_hit_rate() * 100.0
            ));
            v.push(format!("wgts   {}", stats.widget_count));
            v.push(format!("sg     {}", stats.scenegraph_size));
            v
        };
        let lines_n = lines.len() as f32;
        let bg_w = 200.0_f32.min(rect.w);
        let bg_h = (lines_n * line_h + pad * 2.0).min(rect.h);
        let bg_rect = Rect {
            x: rect.x + rect.w - bg_w - pad,
            y: rect.y + pad,
            w: bg_w,
            h: bg_h,
        };
        // M9 Phase 44a: popup buffer (= popup pass) に push して z-order 最前面に。
        // Phase 43 で発見した「popup pass の glyph buffer 上書き」問題は Phase 44a で
        // popup_glyph: GlyphPipeline を独立インスタンスにすることで根本解決済み。
        let prev_in_popup = self.drawing_in_popup;
        self.drawing_in_popup = true;
        let p = self.palette();
        self.push_rect(RectCommand {
            rect: bg_rect,
            fill: p.debug_overlay_bg,
            border: p.debug_overlay_border,
            border_width: 1.0,
            radius: [3.0; 4],
            clip_rect: None,
        });
        for (i, text) in lines.iter().enumerate() {
            self.push_text(GlyphArea {
                text: text.as_str().into(),
                left: bg_rect.x + pad,
                top: bg_rect.y + pad + (i as f32) * line_h,
                font_size,
                line_height: line_h,
                color: p.debug_text,
                clip_rect: None,
                ..GlyphArea::default()
            });
        }
        self.drawing_in_popup = prev_in_popup;
    }

    // ============================================================
    // M9 P1-4: double-click 判定
    // ============================================================

    /// `rect` 内で発生したダブルクリックを 1 度だけ消費する。
    ///
    /// 判定: 直前の `primary_just_released` から `UiHost::set_double_click_threshold`
    /// (default 400ms / 5px) 内に再度 release され、かつ release 位置が `rect` 内なら
    /// `Some((x, y))` を返す。同 frame 内で 2 度目の `take_double_click_in_rect` を呼んでも
    /// 同 rect の double-click は再消費されない (1 frame で 1 度だけ Some)。
    ///
    /// release ベース (drag と区別しやすい)、UiHost-level global state なので「同時に 2 つの
    /// widget で double-click 中」のようなケースは扱わない (real DAW で発生しない前提)。
    pub fn take_double_click_in_rect(&mut self, rect: Rect) -> Option<(f32, f32)> {
        // modal popup の下に隠れている widget は pointer 入力を消費しない (#015)。
        if self.pointer_blocked_by_modal_popup() {
            return None;
        }
        let (px, py) = (*self.pending_double_click)?;
        if rect.contains(px, py) {
            *self.pending_double_click = None;
            Some((px, py))
        } else {
            None
        }
    }

    /// `rect` 内で発生した「double-click の 2 度目の press」を 1 度だけ消費する。
    ///
    /// `take_double_click_in_rect` の **press ベース** 版。 直前の click から
    /// `UiHost::set_double_click_threshold` (default 400ms / 5px) 内に再度 **press** され、
    /// かつ press 位置が `rect` 内なら `Some((x, y))` を返す。 戻り座標は press 時点の pointer
    /// 位置。 release を待たず press 即時に取れるので、「ダブルクリックのボタンを放さずに
    /// drag して note 長を決める」(Bitwig 流) の起点に使う。 release ベースの
    /// `take_double_click_in_rect` とは別 state を消費するので、両者は独立に共存できる。
    pub fn take_double_click_press_in_rect(&mut self, rect: Rect) -> Option<(f32, f32)> {
        // modal popup の下に隠れている widget は pointer 入力を消費しない (#015)。
        if self.pointer_blocked_by_modal_popup() {
            return None;
        }
        let (px, py) = (*self.pending_double_click_press)?;
        if rect.contains(px, py) {
            *self.pending_double_click_press = None;
            Some((px, py))
        } else {
            None
        }
    }

    // ============================================================
    // M14 Phase 63l: caller 側 view 用 rect-based primary press 取得 (daw_01 #026)
    // ============================================================

    /// `rect` 内で primary が **press された** frame に 1 度だけ `Some((x, y))` を返す。
    ///
    /// `take_double_click_in_rect` の single-click (release ベース) 版ではなく、
    /// **press ベース** で取り出す API。 drag start の起点を取りたい場合や、 click と同時に
    /// 即座に反応する低 latency UI (= 段階 2 の event 選択) で使う。 release を待つと
    /// drag detection が手遅れになる。
    ///
    /// semantics:
    /// - rect 内で primary がこのフレームに新たに押下された (= `primary_just_pressed`) → `Some((x, y))`
    /// - rect 外 / press なし / modal popup 配下 → `None`
    /// - 同 frame 内で 2 度目以降の呼び出しは `None` (`consume_pointer_click` で消費する
    ///   ため、 他 widget の click 検出 (button 等) からも消える)
    /// - 戻り座標は press 時点の pointer 位置 (viewport 座標)
    pub fn take_primary_press_in_rect(&mut self, rect: Rect) -> Option<(f32, f32)> {
        if self.pointer_blocked_by_modal_popup() {
            return None;
        }
        if !self.pointer.primary_just_pressed {
            return None;
        }
        let (px, py) = self.pointer.pos?;
        if !rect.contains(px, py) {
            return None;
        }
        // 1 frame 内で 2 度目以降は None / 他 widget の click 検出も巻き込む。
        // 既存 `take_drag_rect_in_rect` は consume_pointer_click を呼ばないが、
        // こちらは「caller view の click を確定的に取った」 後に下層 widget に流さない
        // 意図 (= popup 外 click と同じ挙動) で揃える。
        self.consume_pointer_click();
        Some((px, py))
    }

    /// `rect` 内で secondary (右) が **press された** frame に 1 度だけ `Some((x, y))` を返す。
    ///
    /// `take_primary_press_in_rect` の secondary 版。右クリック起点の view 操作
    /// (例: arrangement の空きレーン右クリック → `SecondaryClickEmpty`) で使う。
    ///
    /// semantics:
    /// - rect 内で secondary がこのフレームに新たに押下された (= `secondary_just_pressed`) → `Some((x, y))`
    /// - rect 外 / press なし / modal popup 配下 → `None`
    /// - 戻り座標は press 時点の pointer 位置 (viewport 座標)
    ///
    /// **primary 版と違い consume はしない**: secondary press は edge bool で 1 frame しか立たず、
    /// caller (arrangement) は rect 全体で take した後に「clip / automation 上か空きか」を判定して
    /// 空きのときだけ emit する。ここで rect 全体を consume すると clip 上の右クリック (caller の
    /// clip context menu 用) まで握りつぶしてしまうため、消費は呼び出し側の判断に委ねる。
    pub fn take_secondary_press_in_rect(&mut self, rect: Rect) -> Option<(f32, f32)> {
        if self.pointer_blocked_by_modal_popup() {
            return None;
        }
        if !self.pointer.secondary_just_pressed {
            return None;
        }
        let (px, py) = self.pointer.pos?;
        if !rect.contains(px, py) {
            return None;
        }
        Some((px, py))
    }

    /// `rect` 内で secondary (右) が **click された** (= 押して動かさずに離した) frame に
    /// 1 度だけ `Some((x, y))` を返す。 座標は **press 位置**。
    ///
    /// daw_01 r.md #35。 `take_secondary_press_in_rect` との違いは 2 点:
    ///
    /// - press ではなく **release** で確定する (Windows の `WM_CONTEXTMENU` と同じ)
    /// - 右ドラッグ (press → 移動 → release) では立たない。 これにより、 同じ右ボタンで
    ///   「click = context menu」 と「drag = 矩形選択 (`take_secondary_drag_rect_in_rect`)」
    ///   を両立できる (REAPER と同じ配置)
    ///
    /// **consume はしない** (`take_secondary_press_in_rect` と同じ理由: caller は rect 全体で
    /// take した後に clip 上か空きかを判定するため)。
    pub fn take_secondary_click_in_rect(&mut self, rect: Rect) -> Option<(f32, f32)> {
        if self.pointer_blocked_by_modal_popup() {
            return None;
        }
        let (px, py) = self.pending_secondary_click_pos()?;
        if !rect.contains(px, py) {
            return None;
        }
        Some((px, py))
    }

    /// このフレームに右クリック (押して動かさずに離した) が確定していれば press 位置。
    /// `take_secondary_click_in_rect` の rect 判定なし版で、 caller が複数 rect を横断して
    /// 「どの popup を出すか」 を決めるときに使う (consume しない read-only query)。
    #[must_use]
    pub fn pending_secondary_click_pos(&self) -> Option<(f32, f32)> {
        *self.pending_secondary_click
    }

    // ============================================================
    // M8 Phase 30: shortcut + focus traversal + focus ring
    // ============================================================

    /// このフレームに `name` で登録した shortcut が triggered されていれば true (consume)。
    /// 同 name で 2 度目に呼ぶと false (= 1 度限り消費)。
    pub fn take_shortcut(&mut self, name: &'static str) -> bool {
        // M14 Phase 94 (daw_01 #065): 真のモーダル中は background widget の shortcut を遮断。
        // **consume しない** ことで、modal の body (`drawing_in_popup == true`) が後で同じ
        // shortcut (ESC 等) を `take_shortcut` で確実に拾える。library 内部の focus traversal は
        // この guard を通さない `take_shortcut_raw` を使う。
        if self.keyboard_blocked_by_modal() {
            return false;
        }
        self.take_shortcut_raw(name)
    }

    /// guard なしの shortcut 消費 (library 内部の Tab / arrow focus traversal 用)。
    /// `take_shortcut` の真のモーダル guard をバイパスする (traversal の対象 focusable は
    /// `focusable` 側で既に modal panel 内のみに絞られているため、guard は不要)。
    pub(crate) fn take_shortcut_raw(&mut self, name: &'static str) -> bool {
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
        if self.keyboard_blocked_by_modal() {
            return false;
        }
        self.pending_shortcuts.contains(&name)
    }

    /// M14 Phase 57: typing-only shortcut (前フレームの `set_typing_focus(true)` で global
    /// 消費が抑制された name) を `keyboard_events` から消費する。focused text widget が
    /// `Ctrl+A` / `Delete` / `Ctrl+X/C/V` 等を `take_shortcut` ではなくこの API で取り出す。
    ///
    /// 一致条件: `shortcut_map.matches(ev, modifiers) == Some(name)` の **最初の Pressed**
    /// event を `keyboard_events` から remove して true を返す。同じ name を続けて呼ぶと
    /// 2 度目以降は false (`take_shortcut` と同じ pull モデル)。
    ///
    /// `is_typing_only_shortcut(name)` が false な name (例: `undo`) を渡しても false を返す。
    /// これは「typing-only に分類されない shortcut は global path で `take_shortcut(name)`
    /// から取るべき」というポリシー強制のため。
    pub fn take_typing_shortcut(&mut self, name: &'static str) -> bool {
        // M14 Phase 94 (daw_01 #065): 真のモーダル中は background の text_input を遮断。
        if self.keyboard_blocked_by_modal() {
            return false;
        }
        if !shortcut::is_typing_only_shortcut(name) {
            return false;
        }
        let mods = self.pointer.modifiers;
        let pos = self.keyboard_events.iter().position(|ev| {
            matches!(ev.state, daw_ui_platform::ElementState::Pressed)
                && self.shortcut_map.matches(ev, mods) == Some(name)
        });
        if let Some(i) = pos {
            self.keyboard_events.remove(i);
            true
        } else {
            false
        }
    }

    /// M14 Phase 58: `text` を `font_size` で shape したときの **末尾までの x advance** を返す。
    /// proportional font (system default の Segoe UI / Helvetica 等) の実 advance に基づくので、
    /// text_input の cursor / selection の x 位置計算に使うと pixel-accurate。
    /// 空文字列なら 0.0。
    ///
    /// 内部の `cosmic_text::FontSystem` は renderer 側 (`GlyphPipeline`) のものとは別 instance だが、
    /// 同じ system fonts を読むので shape 結果は一致する (キャッシュは別)。
    pub fn measure_text(&mut self, text: &str, font_size: f32) -> f32 {
        self.text_metrics.measure_advance(&mut *self.font_system, text, font_size)
    }

    /// `text` を `font_size` で描画したとき幅 `max_w` を超えるなら、 末尾を ellipsis
    /// (`…`、 描画フォントに字形が無ければ ASCII `...`) で省略して `(描画文字列, 実描画幅)`
    /// を返す。 収まる場合は元の文字列 (`Cow::Borrowed`) と実幅をそのまま返すので、 短い
    /// ラベルは **byte 完全互換** (= 既存 caller の外観不変)。 省略時のみ `Cow::Owned` を返す。
    ///
    /// 「widget は自分の rect 境界に責任を持つ」 を 1 箇所で保証するための共有 helper
    /// (daw_01 #079: 長い track 名が name 領域を越えて M/S/R ボタンの隙間から覗く bug)。
    /// `button_at_clicked_sized` / `toggle_button_at` から呼ばれ、 rect 幅より広いラベルを
    /// 渡す将来の caller も自動で守られる。
    ///
    /// prefix の探索は char 境界単位の二分探索で `measure_text(prefix + ellipsis) <= max_w`
    /// を満たす最長 prefix を選ぶ (cosmic-text の実 advance ベースなので wide glyph でも正しい)。
    /// 返り値の Cow が `Owned` か否かで caller は「省略されたか」 を判定できる
    /// (省略時は左寄せ + `clip_rect` を付ける等)。
    pub(crate) fn fit_text_ellipsized<'t>(
        &mut self,
        text: &'t str,
        font_size: f32,
        max_w: f32,
    ) -> (Cow<'t, str>, f32) {
        let full_w = self.measure_text(text, font_size);
        if full_w <= max_w || text.is_empty() {
            return (Cow::Borrowed(text), full_w);
        }
        let ellipsis = self.text_metrics.ellipsis(&mut *self.font_system);
        let ellipsis_w = self.measure_text(ellipsis, font_size);
        // max_w が ellipsis すら入らないほど狭い: prefix 0 文字 = ellipsis のみ。
        // (描画側の clip_rect が最終的な overshoot を抑える。)
        if ellipsis_w >= max_w {
            return (Cow::Owned(ellipsis.to_owned()), ellipsis_w);
        }
        // char 境界の byte offset 列 (0..=len)。空 prefix (offset 0) = ellipsis のみは必ず収まる
        // (上で ellipsis_w < max_w を確認済)。
        let boundaries: Vec<usize> = text
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(text.len()))
            .collect();
        // `measure(prefix + ellipsis) <= max_w` を満たす最長 prefix を二分探索する。
        // **prefix 単独でなく合成文字列を測る**ので、 prefix 末尾と ellipsis の接合部 kerning も
        // 制約に織り込まれ、 採用した候補の描画幅は必ず max_w 以下になる (overshoot を原理的に排除、
        // daw_01 #079 review 指摘)。 prefix が長いほど合成幅も増える単調性を仮定し、 kerning 由来の
        // 微小逆転が残っても「採用は必ず収まる候補のみ」 なので最終幅は max_w を超えない。
        let mut lo = 0usize;
        let mut hi = boundaries.len() - 1;
        let mut best = 0usize;
        let mut best_w = ellipsis_w;
        while lo <= hi {
            let mid = usize::midpoint(lo, hi);
            let mut candidate = String::from(&text[..boundaries[mid]]);
            candidate.push_str(ellipsis);
            let w = self.measure_text(&candidate, font_size);
            if w <= max_w {
                best = mid;
                best_w = w;
                lo = mid + 1;
            } else if mid == 0 {
                break;
            } else {
                hi = mid - 1;
            }
        }
        let mut display = String::from(&text[..boundaries[best]]);
        display.push_str(ellipsis);
        (Cow::Owned(display), best_w)
    }

    /// text_input 等の typing widget が focus 中に呼ぶ。修飾なし shortcut (Space 等) を
    /// 抑制する目的だが、現状の実装は shortcut layer が frame 頭で済ませているため、
    /// このフラグはまだ参照されていない (M9 で typing_focus を見て修飾なし shortcut を
    /// 後から restore する path を追加予定)。
    pub fn set_typing_focus(&mut self, typing: bool) {
        // M14 Phase 94 (daw_01 #065): 真のモーダル中は background の typing 宣言を無視
        // (次フレームの shortcut layer が typing-only shortcut を誤って残さないように)。
        if self.keyboard_blocked_by_modal() {
            return;
        }
        *self.typing_focus = typing;
    }

    /// 自身を Tab / arrow focus traversal の対象として登録する。
    /// 登場順で Tab next / Shift+Tab prev、arrow は方向別の最近傍移動。
    ///
    /// M14 Phase 94 (daw_01 #065): 真のモーダル中の background widget は登録しない。
    /// これにより Tab traversal の対象が modal panel 内 (`drawing_in_popup == true`) の
    /// widget のみになり、background へ focus が漏れない。
    pub fn focusable(&mut self, wid: WidgetId, rect: Rect) {
        if self.keyboard_blocked_by_modal() {
            return;
        }
        self.focus_order.push((wid, rect));
    }

    /// 現在 focus を持っているなら 1px の青系 ring を `rect` 周囲に描画する。
    /// `wid` を渡し、self が focused かを内部で判定する想定だが、`is_focused` を呼んで
    /// 利用者側で判定するスタイルでも動く (描画のみ行う、判定はしない)。
    pub fn draw_focus_ring(&mut self, rect: Rect) {
        let color = self.palette().focus_ring;
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
        self.push_lines(LineBatch { segments: segments.into(), line_width_px: 1.0, clip_rect: None });
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
        // M14 Phase 94 (daw_01 #065): 真のモーダル中は background の paste 取得を遮断。
        if self.keyboard_blocked_by_modal() {
            return None;
        }
        self.pending_clipboard_paste.take()
    }

    /// 任意の文字列を OS clipboard に書き込む (frame 末尾で provider.set_text)。
    /// 同 frame 内で複数回呼ぶと最後勝ち (= 直前の write は捨てられる)。
    pub fn set_clipboard_text(&mut self, s: String) {
        // 最後勝ちの semantics を維持するため、既存を clear して push。
        self.pending_clipboard_writes.clear();
        self.pending_clipboard_writes.push(s);
    }

    // ============================================================
    // M8 Phase 32: file drop
    // ============================================================

    /// `rect` 内に file がドロップされていれば `DroppedFiles` を 1 度だけ取り出す。
    /// 同 frame 内で複数 widget が呼んでも先勝ち。
    /// 戻り値の `position` は drop 直前の cursor 座標 (viewport 座標)。
    /// caller は drop.paths と drop.position から (track, beat) など好きな解決を行う。
    pub fn take_file_drop_in_rect(&mut self, rect: Rect) -> Option<DroppedFiles> {
        // M14 Phase 94 (daw_01 #065): 真のモーダル中は background widget への drop を遮断
        // (modal body 内 = drawing_in_popup では `pointer_blocked_by_modal_popup` が false)。
        if self.pointer_blocked_by_modal_popup() {
            return None;
        }
        let drop_pos = self.file_drop.as_ref()?.position;
        if !rect.contains(drop_pos.0, drop_pos.1) {
            return None;
        }
        self.file_drop.take()
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
        // M14 Phase 94 (daw_01 #065): 真のモーダル中は background widget の hover query も inert に
        // する (`is_file_hovering_in_rect` は masked pointer で自然に false になるが、こちらは
        // file_hover を直読みするので明示 guard が要る)。drop 自体は `take_file_drop_in_rect` で遮断済。
        if self.pointer_blocked_by_modal_popup() {
            return None;
        }
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
        let p = self.pointer;
        self.drag_rect_with_button(wid, bounds, p.primary_just_pressed, p.primary_just_released)
    }

    /// `take_drag_rect_in_rect` の **secondary (右) ボタン** 版 (daw_01 r.md #35)。
    ///
    /// 右ドラッグを矩形選択に使うための API。 左ドラッグと違い「要素の上から始めても
    /// 移動 / リサイズと衝突しない」 ので、 クリップ / ノートの上からでも範囲選択を開始できる
    /// (REAPER の既定と同じ配置)。 動かさずに離した右ボタンは矩形選択にならず、
    /// `take_secondary_click_in_rect` (= context menu) 側が拾う。
    ///
    /// `wid` は左 drag と **別 id を渡すこと** (同一 id だと `DragRectState` を共有して
    /// 左右の session が互いを打ち消す)。
    pub fn take_secondary_drag_rect_in_rect(
        &mut self,
        wid: WidgetId,
        bounds: Rect,
    ) -> Option<DragRect> {
        let p = self.pointer;
        self.drag_rect_with_button(
            wid,
            bounds,
            p.secondary_just_pressed,
            p.secondary_just_released,
        )
    }

    /// `take_drag_rect_in_rect` / `take_secondary_drag_rect_in_rect` の共通実装。
    /// ボタン差は press / release の edge bool だけなので、 それを引数で受ける。
    fn drag_rect_with_button(
        &mut self,
        wid: WidgetId,
        bounds: Rect,
        just_pressed: bool,
        just_released: bool,
    ) -> Option<DragRect> {
        // modal popup の下に隠れている widget は pointer 入力を消費しない (#015)。
        if self.pointer_blocked_by_modal_popup() {
            // M14 Phase 94 (daw_01 #065): 真のモーダルが drag 進行中に開いた場合、release が
            // masking で届かず anchor が永久に残る (modal close 後に phantom drag が再開)。
            // capturing modal のときは進行中 session を cancel して stale anchor を断つ
            // (既存 state があるときだけ。空 widget に default state を挿入しない)。
            if self.modal_capturing && self.state.contains_key(&wid) {
                let state: &mut DragRectState = self.widget_state(wid);
                state.drag_start = None;
            }
            return None;
        }
        let pointer = self.pointer;
        let modifiers = pointer.modifiers;

        let (active, just_finished, snapshot_start, snapshot_mods) = {
            let state: &mut DragRectState = self.widget_state(wid);

            // 押下 in bounds → drag 開始
            if just_pressed
                && let Some((px, py)) = pointer.pos
                && bounds.contains(px, py)
            {
                state.drag_start = Some((px, py));
                state.start_modifiers = modifiers;
            }

            let active = state.drag_start.is_some();
            let just_finished = active && just_released;
            let start = state.drag_start;
            let start_mods = state.start_modifiers;

            // release で state クリア (次フレーム以降は active=false)
            if just_released {
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

        // drag 中は半透明の marquee overlay を自動描画 (release frame でも 1 度描画する)。
        let r = drag.rect();
        let fill = self.palette().marquee_fill;
        let border = self.palette().marquee_border;
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
    // M14 Phase 63l: caller 側 view 用 rect-anchored drag session (daw_01 #026)
    // ============================================================

    /// `rect` を anchor とする **drag session** を 1 つ追跡する low-level primitive。
    ///
    /// `take_drag_rect_in_rect` (multi-select 用、 半透明 overlay を自動描画する) と異なり、
    /// **描画は一切行わない**。 caller 側 view (= Audio Editor の event ごとの rect 上に
    /// 中央 drag = 移動 / 端 drag = trim を載せる) が anchor 付き press / release を取り
    /// 出して自前で UI を描けるよう、 純粋に pointer state を返すだけ。
    ///
    /// semantics:
    /// - `rect` 内で primary が press された frame: `Some(DragInfo { kind: Started, .. })` を
    ///   1 度返し、 内部 state に anchor を記録 (= `consume_pointer_click` も実施)
    /// - 次フレーム以降、 release されるまで毎フレーム `Some(DragInfo { kind: Continuing, .. })`
    ///   を返す (rect 外に pointer が出ても drag session は継続)
    /// - release frame: `Some(DragInfo { kind: Released, .. })` を 1 度返し、 内部 state を clear
    /// - 以降は `None`
    /// - rect 外で primary が押下された場合、 anchor を記録せず session は始まらない
    /// - modal popup 配下では session が始まらない (#015 と同じ早期 return)
    /// - 同 frame 内で複数の caller が同 rect を要求しても、 drag 開始 frame は
    ///   `consume_pointer_click` で他 caller の press 検出を消すため 1 度だけ消費される
    /// - drag 中の `pointer.primary_pressed` (現在押下中フラグ) は消費しない → 他 widget が
    ///   読みたい場合は読める (drag rect / button の armed 判定が同 frame に共存可能)
    ///
    /// `id` は drag session を識別するための任意 Hash 値 (i32 / &str / `(label, idx)` 等)。
    /// 内部で `WidgetId::ROOT.child((b"drag_in_rect", &id))` に変換する。 同じ `id` を
    /// 複数回呼ぶと同一 session 扱いだが、 1 frame 内で複数の `id` を呼んで「複数 session
    /// が同時に走る」 のは pointer 1 つしか無いので意味が無い (実質的に 1 session)。
    pub fn take_drag_in_rect(
        &mut self,
        id: impl std::hash::Hash,
        rect: Rect,
    ) -> Option<DragInfo> {
        let wid = WidgetId::ROOT.child((b"drag_in_rect", &id));
        if self.pointer_blocked_by_modal_popup() {
            // M14 Phase 94 (daw_01 #065): take_drag_rect_in_rect と同様、capturing modal が
            // drag 中に開いたら stale anchor を断つ (既存 state があるときだけ)。
            if self.modal_capturing && self.state.contains_key(&wid) {
                let state: &mut DragInRectState = self.widget_state(wid);
                state.anchor = None;
            }
            return None;
        }
        let pointer = self.pointer;
        let modifiers = pointer.modifiers;

        // press → anchor 記録 (rect 内のみ)。 already_active なら start を上書きしない。
        let just_started = {
            let state: &mut DragInRectState = self.widget_state(wid);
            let already_active = state.anchor.is_some();
            if !already_active
                && pointer.primary_just_pressed
                && let Some((px, py)) = pointer.pos
                && rect.contains(px, py)
            {
                state.anchor = Some((px, py));
                state.start_modifiers = modifiers;
                true
            } else {
                false
            }
        };
        // drag 開始 frame は他 widget に同じ click を流さない (consume)。
        if just_started {
            self.consume_pointer_click();
        }

        // anchor / start_modifiers / release 判定の snapshot。 release 時は state も clear。
        let (anchor_opt, start_modifiers, just_released) = {
            let state: &mut DragInRectState = self.widget_state(wid);
            let active = state.anchor.is_some();
            let just_released = active && pointer.primary_just_released;
            let snap = (state.anchor, state.start_modifiers, just_released);
            if just_released {
                state.anchor = None;
            }
            snap
        };
        let anchor = anchor_opt?;

        let kind = if just_started {
            DragKind::Started
        } else if just_released {
            DragKind::Released
        } else {
            DragKind::Continuing
        };
        let current = pointer.pos.unwrap_or(anchor);

        Some(DragInfo {
            anchor,
            current,
            delta: (current.0 - anchor.0, current.1 - anchor.1),
            kind,
            start_modifiers,
            modifiers,
        })
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
        // modal popup の下に隠れている widget は pointer 入力を消費しない (#015)。
        if self.pointer_blocked_by_modal_popup() {
            return (0.0, 0.0);
        }
        let Some((px, py)) = self.pointer.pos else { return (0.0, 0.0) };
        if !rect.contains(px, py) {
            return (0.0, 0.0);
        }
        let d = self.pointer.scroll_delta;
        self.pointer.scroll_delta = (0.0, 0.0);
        // M14 Phase 94 (daw_01 #065): consume を両 pointer に反映 (`consume_pointer_click` と対称)。
        // popup body は `pointer_raw` の copy を読むので、mirror しないと同 frame の別 body へ
        // 同じ scroll が二重配信されうる (multi-popup edge)。
        self.pointer_raw.scroll_delta = (0.0, 0.0);
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
        // M14 Phase 94 (daw_01 #065): 真のモーダル中は background の focused widget を遮断。
        // **drain しない** ので、modal body の text_input (drawing_in_popup) が同じ
        // keyboard_events を取得できる。
        if self.keyboard_blocked_by_modal() {
            return Vec::new();
        }
        if self.focused == Some(wid) {
            std::mem::take(self.keyboard_events)
        } else {
            Vec::new()
        }
    }

    /// `wid` がフォーカスを持っているならフレームに溜まった IME イベントを取り出す。
    /// `take_keyboard_events_if_focused` と同じく、フレーム開始時 focus でチェックする。
    pub fn take_ime_events_if_focused(&mut self, wid: WidgetId) -> Vec<ImeEvent> {
        // M14 Phase 94 (daw_01 #065): 真のモーダル中は background の focused widget を遮断。
        if self.keyboard_blocked_by_modal() {
            return Vec::new();
        }
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
        // M14 Phase 94 (daw_01 #065): 真のモーダル中は background の focused text_input が
        // IME 候補窓を出さないようにする (modal body 内 = drawing_in_popup では通常どおり)。
        if self.keyboard_blocked_by_modal() {
            return;
        }
        self.ime_request = Some(cursor_area);
    }

    /// **(M15)** focus 中の text_input が、自身の `text` + selection (anchor/cursor byte) + caret
    /// rect を OS text store (TSF) に publish する。`frame()` 末尾で
    /// `WindowBackend::set_text_input_document` に渡され、rtry のまぜ書き `GetText` / MS-IME 再変換が
    /// アプリのテキストを読めるようになる。`request_ime` と同じく focus 中 widget だけが毎フレーム
    /// 呼ぶ想定 (last-call-wins、 modal 中の background は遮断)。
    pub fn publish_text_document(
        &mut self,
        text: &str,
        anchor_byte: usize,
        cursor_byte: usize,
        caret: Rect,
        // E1 (r.md #8): 各文字境界の `(x, byte)` (caret と同座標系)。`GetACPFromPoint` の
        // 逆 hit-test 用。空なら store は layout 無し扱い。
        char_boundaries: Vec<(f32, usize)>,
    ) {
        if self.keyboard_blocked_by_modal() {
            return;
        }
        self.text_document = Some(TextDocument {
            text: text.to_string(),
            selection: (anchor_byte, cursor_byte),
            caret_rect: RectPx { x: caret.x, y: caret.y, w: caret.w, h: caret.h },
            char_boundaries,
        });
    }

    /// `WidgetId` に紐付く永続状態 (retained state) を取得 or 初期化する。
    ///
    /// immediate-mode の widget が、フレーム間で保持したい状態 (drag anchor / LOD キャッシュ /
    /// last-click 等) を id-keyed で持つための**拡張点**。lib 内の fader/knob/waveform も同じ
    /// 経路で drag 状態を持つが、これを `pub` にすることで **lib 外 (daw_gui 側) の widget も**
    /// 自前の retained 状態を持てる (S4b/c で移設する arrangement/piano_roll がこれを使う。
    /// 「stateful widget は lib 内にしか書けない」という構造制約の解消)。
    ///
    /// `S` は `WidgetState` (= `Any + Send + Sync` の任意型に blanket 実装) + `Default`。
    /// 同じ id を別の型 `S` で取り直すと downcast に失敗して panic するので、id と型は 1:1 で使う。
    pub fn widget_state<S: WidgetState + Default + 'static>(
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
pub fn merge_clip(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
    match (a, b) {
        (None, None) => None,
        (Some(r), None) | (None, Some(r)) => Some(r),
        (Some(a), Some(b)) => Some(a.intersect(b)),
    }
}

/// M14 Phase 94 (daw_01 #065): 真のモーダル中、background widget が読む pointer。
/// pos = None / 全 button false / scroll 0 で「pointer が存在しない」状態に潰す。
/// `modifiers` は keyboard 側の状態なので保持する (pos が None なので hover / drag は始まらず無害、
/// modal body は `pointer_raw` を見るので影響しない)。
fn masked_pointer(p: PointerFrame) -> PointerFrame {
    PointerFrame {
        pos: None,
        primary_just_pressed: false,
        primary_just_released: false,
        primary_pressed: false,
        secondary_just_pressed: false,
        secondary_just_released: false,
        modifiers: p.modifiers,
        scroll_delta: (0.0, 0.0),
    }
}

/// M15: platform 層から drain した [`ImeTextEdit`] を widget が処理する [`ImeEvent`] に変換する。
fn ime_text_edit_to_event(e: ImeTextEdit) -> ImeEvent {
    match e {
        ImeTextEdit::Replace { start_byte, end_byte, text, new_cursor } => {
            ImeEvent::ReplaceRange { start_byte, end_byte, text, new_cursor }
        }
        ImeTextEdit::SetSelection { anchor_byte, cursor_byte } => {
            ImeEvent::SetSelection { anchor_byte, cursor_byte }
        }
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

#[cfg(test)]
mod tests;
