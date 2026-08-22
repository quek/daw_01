//! examples/text_input_ime — M15 TSF (`ITextStoreACP`) 実機検証ハーネス。
//!
//! 1 つの text_input を auto-focus で表示するだけの最小サンプル。**winit IMM を一切触らない**
//! (`set_ime_allowed` を呼ばない) ので、Windows では library が driveする TSF text store のみが
//! 有効になり、rtry / MS-IME の挙動を切り分けて確認できる。
//!
//! 確認手順 (Windows + rtry を IME 有効化):
//! - ひらがなを入力 → 文字が field に出る (= rtry が TSF SetText で書き込めている)
//! - `fj` でまぜ書き変換 → rtry がカーソル前テキストを `GetText` で読み変換 (= P1 の主目的)
//! - `55` でストロークヘルプ → カーソル直前 1 文字を読む
//! - MS-IME に切替え、確定済みテキストを選択 → 再変換 (`IMR_RECONVERTSTRING` 相当)
//!
//! TSF が使えない環境 (apartment 衝突等) では自動で何もしない (text_input は通常の
//! winit キー入力で動く)。

use std::sync::Arc;

use daw_ui_core::{Edit, InputAccumulator, TextInputStyle, UiHost};
use daw_ui_platform::{AppEvent, AppHost, WindowBackend, winit_backend};
use daw_ui_renderer::{Color, Rect, Renderer, Scene};
use winit::window::WindowAttributes;

struct Model {
    /// text_input の現在値 (uncontrolled buffer の writeback 先)。
    text: String,
    /// Enter で確定した最終値。
    committed: String,
}

struct App {
    window: Arc<winit_backend::WinitWindow>,
    renderer: Renderer<winit_backend::WinitWindow>,
    ui: UiHost<Model>,
    model: Model,
    scene: Scene,
    input: InputAccumulator,
    /// IME 有効状態の差分管理 (mixer / piano_roll / daw_01 と同じ contract)。
    /// winit はデフォルト IME 無効なので、focus 中は `set_ime_allowed(true)` が必須。
    ime_enabled: bool,
}

impl App {
    fn new(window: Arc<winit_backend::WinitWindow>) -> Self {
        let renderer = Renderer::new(window.clone()).expect("Renderer::new");
        window.set_title("daw-ui text_input_ime (M15 TSF)");
        Self {
            ui: UiHost::with_window(window.clone()),
            window,
            renderer,
            model: Model { text: String::new(), committed: String::new() },
            scene: Scene::new(),
            input: InputAccumulator::new(),
            ime_enabled: false,
        }
    }

    /// `ime_request()` の Some/None に応じて OS の IME 有効化 + 候補窓位置を差分更新する。
    /// この「app が IME enable を駆動」する contract は TSF とは独立 (TSF store は library が
    /// `set_text_input_document` 経由で自動 publish する)。
    fn sync_ime(&mut self) {
        match (self.ime_enabled, self.ui.ime_request()) {
            (false, Some(area)) => {
                self.window.set_ime_allowed(true);
                self.window.set_ime_cursor_area(
                    f64::from(area.x), f64::from(area.y),
                    f64::from(area.w), f64::from(area.h),
                );
                self.ime_enabled = true;
            }
            (true, Some(area)) => {
                self.window.set_ime_cursor_area(
                    f64::from(area.x), f64::from(area.y),
                    f64::from(area.w), f64::from(area.h),
                );
            }
            (true, None) => {
                self.window.set_ime_allowed(false);
                self.ime_enabled = false;
            }
            (false, None) => {}
        }
    }

    fn build_ui(&mut self) {
        self.scene.clear();
        let screen = self.renderer.size();
        let input = self.input.take_input();

        self.ui.frame(&mut self.model, &mut self.scene, screen, input, |m, ui| {
            ui.label_at(
                "title",
                "TSF IME test — まぜ書き(fj) / ストロークヘルプ(55) / 再変換 を試す",
                20.0, 20.0, 18.0,
                Color::rgb(0.95, 0.95, 0.97),
            );

            let field = Rect { x: 20.0, y: 64.0, w: (screen.width as f32 - 40.0).max(120.0), h: 32.0 };
            let resp = ui.text_input_at_focused("field", field, &m.text, &TextInputStyle::default(), |new| {
                Edit::mutate(move |m: &mut Model| m.text = new)
            });
            if let Some(committed) = resp.committed_text {
                ui.push_edit(Edit::mutate(move |m: &mut Model| m.committed = committed));
            }

            ui.label_at(
                "echo",
                &format!("text  : {}", m.text),
                20.0, 116.0, 14.0,
                Color::rgb(0.75, 0.85, 0.95),
            );
            ui.label_at(
                "commit",
                &format!("Enter : {}", m.committed),
                20.0, 140.0, 14.0,
                Color::rgb(0.65, 0.78, 0.72),
            );
        });
    }
}

impl AppHost for App {
    fn on_event(&mut self, ev: AppEvent) {
        self.input.ingest(&ev);
        match ev {
            AppEvent::Resized(size) => {
                self.renderer.resize(size);
                self.window.request_redraw();
            }
            AppEvent::PointerMoved(_)
            | AppEvent::PointerInput { .. }
            | AppEvent::Keyboard(_)
            | AppEvent::ModifiersChanged(_) => {
                self.window.request_redraw();
            }
            _ => {}
        }
    }

    fn on_render(&mut self) -> bool {
        self.build_ui();
        // text_input が focus 中なら IME を有効化 (winit デフォルトは無効)。
        self.sync_ime();
        if let Err(e) = self.renderer.render(&self.scene) {
            eprintln!("render error: {e}");
        }
        // event-driven: TSF 由来の編集 (rtry の SetText 等) はメッセージポンプ中に非同期に
        // 積まれるが、library 側が編集時に request_redraw するので continuous redraw は不要。
        // (= daw_01 のような event-driven app と同じ条件で TSF 入力が遅延しないことを実証する)
        false
    }
}

fn main() {
    let attrs = WindowAttributes::default()
        .with_title("daw-ui text_input_ime (M15 TSF)")
        .with_inner_size(winit::dpi::LogicalSize::new(900.0, 220.0));
    if let Err(e) = winit_backend::run_app(attrs, |window| App::new(Arc::new(window))) {
        eprintln!("event loop error: {e}");
    }
}
