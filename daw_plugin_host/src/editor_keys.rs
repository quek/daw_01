//! r.md #36: プラグインエディタ窓のキーを 「**プラグインが消化しなかったものだけ**」
//! ホスト (daw_gui) へ返すための判定。
//!
//! # なぜ判定が要るのか
//!
//! エディタ窓にフォーカスがある間、 `WM_KEYDOWN` は plugin-main スレッドのキューに入り
//! そのままプラグインへ dispatch される。 ここで Space を無条件に横取りすると、
//! プラグインのプリセット名入力欄に空白が打てなくなる。 逆に何もしないと
//! 「エディタを開いている間だけ Space で再生できない」。
//!
//! # どうやって判定するか (一次情報)
//!
//! Win32 にも CLAP にも 「フォーカス中のコントロールがこのキーを欲しがっているか」 を
//! 一般に問い合わせる手段は無い (CLAP は `include/clap/ext` / `ext/draft` を全列挙しても
//! キーボード関連の拡張がゼロ)。 実際に効くのは次の 2 経路で、 **両方入れて初めて主要
//! フレームワークが揃う**。
//!
//! ## 経路 B (主): 未消化キーの親窓バブリングを拾う
//!
//! - **JUCE**: `HWNDComponentPeer::peerWindowProc` は
//!   `if (doKeyDown (wParam)) return 0; forwardMessageToParent (...)` で、
//!   未消化キーを `PostMessage(GetParent(hwnd), ...)` する。 そして
//!   `juce::TextEditor::keyStateChanged` は修飾なしの全キーで `true` を返す
//!   (JUCE 本体のコメントに "overridden to avoid forwarding key events to the parent")。
//!   ⇒ **テキスト欄にフォーカスがあるときの Space は親に来ない / 無いときだけ来る**。
//!   我々のエディタ窓 = その親なので、 [`EditorWindow`] の WNDPROC に届いた時点で
//!   「プラグインが要らないと言った」 が確定する。
//! - **iPlug2**: `IGraphicsWin::WndProc` が未処理キーを
//!   `SendMessageW(GetAncestor(hWnd, GA_ROOT), msg, ...)` で投げ返す。 同じく確定。
//!
//! ## 経路 A (副): `WM_GETDLGCODE` でフォーカス窓に問い合わせる
//!
//! - **VSTGUI** (Steinberg 系 VST3 の多く): `Win32Frame` は `WM_GETDLGCODE` を
//!   **実装していない** ので `DefWindowProc` が 0 を返す (= 「要らない」)。 一方
//!   文字編集中は `win32textedit.cpp` が本物の `"EDIT"` を `CreateWindowEx` して
//!   `SetFocus` し、 そのサブクラスが `DLGC_WANTALLKEYS` を返す。 ⇒ 完全に判別できる。
//! - **注意**: JUCE と iPlug2 のメイン窓は **フォーカス状態に関係なく無条件で
//!   `DLGC_WANTALLKEYS`** を返す。 経路 A 単独だとこの 2 つから永久にキーを取れない。
//!   だから経路 B が主で A が副。
//! - VSTGUI の EDIT サブクラスは `DLGC_WANTCHARS` を落として `DLGC_WANTALLKEYS` だけを
//!   返すので、 `WANTCHARS` 単独判定は誤り。 両方見る。
//!
//! ## 経路 A の答えを信じてよい相手 ([`trusts_dlgcode`])
//!
//! `DLGC_WANTALLKEYS` は 「テキスト入力中」 ではなく 「キーは全部自分で捌く」 の宣言で、
//! canvas 系フレームワークはフォーカス状態に関係なく無条件に返す。 それでも JUCE / iPlug2 は
//! 経路 B で未消化キーを返してくるので信じてよい。 一方 **VCV Rack 2 (GLFW の子窓、 クラス
//! `GLFW30`) は `WM_GETDLGCODE` に `DLGC_WANTALLKEYS` (0x4) を返し (実測 2026-09-12。 公開
//! GLFW は未実装なので Rack Pro のプラグイン層が上書きしている)、 かつ親へ転送しない** ので、
//! 答えを信じると Space が永久に戻ってこない。 だから経路 A の答えは
//! - Win32 の caret を持つ窓 (= 本物のテキスト入力: `EDIT` / RichEdit / VSTGUI の EDIT)、
//! - 経路 B で未消化キーを返す canvas (JUCE `JUCE_*` / iPlug2 `IPlugWndClass`)
//!
//! からだけ信じ、 それ以外の 「全キー寄越せ」 は無視して転送する (fail-open: ホストの
//! ショートカットが効く側に倒す)。
//!
//! ## 判定できないもの
//!
//! Dear ImGui / GLFW / 自前 OpenGL 系は親へ転送せず Win32 の caret も作らないので、 自前の
//! テキスト欄に入力中かどうかは **外から知る手段が無い**。 上の規則ではそこへ打った Space も
//! 転送されてしまう。 REAPER も自動交渉を諦めて FX ごとの 「Send all keyboard input to
//! plug-in」 トグルを持っている。 本実装も同じ逃げ道 ([`PluginCommand::SetEditorSendAllKeys`])
//! を用意する。
//!
//! # どこで横取りするか — `WH_GETMESSAGE` フック (plugin-main スレッド限定)
//!
//! 判定は **メッセージがキューから取り出される瞬間** に [`KeyRouter::handle`] で行い、
//! 飲み込むなら `MSG.message` を `WM_NULL` に書き換える (MSDN `GetMsgProc`: *"the hook
//! procedure can modify the message"*)。 plugin-main の `GetMessageW` ループで判定して
//! いた頃は、 **プラグイン自身が `PeekMessageW(NULL, PM_REMOVE)` で自分のスレッドの
//! キューを空にする** 実装 (GLFW の `_glfwPlatformPollEvents` = VCV Rack が毎フレーム
//! `glfwPollEvents()` を呼ぶ) では Space が一度もループへ戻らず、 「VCV の窓にフォーカスが
//! あると Space で再生 / 停止できない」 になっていた。 フックは `GetMessage` /
//! `PeekMessage` の呼び出し元を問わず (MSDN `WH_GETMESSAGE`: *"whenever an application
//! calls the GetMessage or PeekMessage function"*) 同じスレッドで走るので、 誰がポンプ
//! しても判定が 1 本に揃う。
//!
//! 状態 ([`KeyRouter`]) は `PluginHost` から切り離して thread-local に置く。 フックは
//! プラグインのコードの内側 (= `PluginHost` が `&mut` で借りられている最中) からも
//! 呼ばれるので、 `PluginHost` の `&mut` を作る借用口では成立しない。

use std::cell::RefCell;
use std::collections::HashSet;

use common::protocol::{DeviceAddr, KeyChord, PluginEvent};
use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, VK_CONTROL, VK_MENU, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DLGC_WANTALLKEYS, DLGC_WANTCHARS, GUITHREADINFO, GetClassNameW,
    GetGUIThreadInfo, HC_ACTION, HHOOK, IsChild, MSG, PM_REMOVE,
    SMTO_ABORTIFHUNG, SMTO_BLOCK, SendMessageTimeoutW, SetWindowsHookExW, UnhookWindowsHookEx,
    WH_GETMESSAGE, WM_GETDLGCODE, WM_KEYDOWN, WM_KEYUP, WM_NULL, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::editor_window::{WM_EDITOR_KEY_RELAY_DOWN, WM_EDITOR_KEY_RELAY_UP};

/// キー押下 / 解放メッセージか。 `Alt` 修飾付きは `WM_SYSKEY*` で届くので両方見る。
#[must_use]
pub fn is_key_down(message: u32) -> bool {
    message == WM_KEYDOWN || message == WM_SYSKEYDOWN
}

/// 対になる key-up。 down だけ奪って up をプラグインに流すと、 押しっぱなし状態を
/// 追うプラグインが 「押されたまま」 と誤認するので、 奪うなら対で奪う。
#[must_use]
pub fn is_key_up(message: u32) -> bool {
    message == WM_KEYUP || message == WM_SYSKEYUP
}

/// メッセージが自動リピート由来か (lParam bit30 = 直前の押下状態)。
/// 1 押下 1 発火にするため、 リピートは転送しない (`runner.rs` の
/// プレビュー窓が `!event.repeat` を見るのと同じ規約)。
#[must_use]
pub fn is_auto_repeat(lparam: LPARAM) -> bool {
    (lparam.0 >> 30) & 1 == 1
}

/// 現在の修飾キー状態を載せた chord を組み立てる。
///
/// `GetKeyState` は **呼び出しスレッドのキュー基準**の状態を返すので、 plugin-main の
/// メッセージポンプ内で呼ぶ限り 「そのキーが押された瞬間の修飾」 と一致する。
#[must_use]
pub fn chord_of(msg: &MSG) -> KeyChord {
    let down = |vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| -> bool {
        // 最上位ビットが立っていれば押下中。
        unsafe { GetKeyState(i32::from(vk.0)) < 0 }
    };
    KeyChord {
        vk: u16::try_from(msg.wParam.0).unwrap_or(0),
        ctrl: down(VK_CONTROL),
        shift: down(VK_SHIFT),
        alt: down(VK_MENU),
    }
}

/// フォーカス窓がこのキーを欲しがっているか (経路 A)。
///
/// `hwnd` はメッセージの宛先窓 (= フォーカス窓)。 キーボードメッセージは
/// 「フォーカス窓を作ったスレッドのキュー」 に入るので、 plugin-main のポンプが
/// 取り出せた時点でその窓は同一スレッド所有であり、 `SendMessage` は WNDPROC の
/// 直接呼び出しになる (デッドロックしない)。 それでも防御的に timeout 付きを使う。
#[must_use]
pub fn window_wants_key(hwnd: HWND, msg: &MSG) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    let mut result: usize = 0;
    let ok = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_GETDLGCODE,
            WPARAM(msg.wParam.0),
            LPARAM(std::ptr::from_ref(msg) as isize),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            50,
            Some(&mut result),
        )
    };
    if ok.0 == 0 {
        // 応答なし = 判定不能。 安全側 (プラグインに渡す) へ倒す。
        return true;
    }
    #[allow(clippy::cast_possible_truncation)]
    let code = result as u32;
    // VSTGUI の EDIT サブクラスは WANTCHARS を落として WANTALLKEYS だけ返すので両方見る。
    code & (DLGC_WANTALLKEYS | DLGC_WANTCHARS) != 0
}

/// 経路 A ([`window_wants_key`]) の答えを信じてよい窓か (module doc 「経路 A の答えを
/// 信じてよい相手」)。
///
/// - Win32 の caret を持つ = 本物のテキスト入力中 (EDIT / RichEdit / VSTGUI の EDIT は
///   `WM_SETFOCUS` で `CreateCaret` する)。 `GetGUIThreadInfo` は 「呼び出しスレッドの
///   キュー」 の caret を返すので、 plugin-main のフックから呼ぶ限り同じスレッドの窓が対象。
/// - 未消化キーを親へ返す canvas: JUCE (`juce_Windowing_windows.cpp`:
///   `String windowClassName ("JUCE_")`) / iPlug2 (`IGraphicsWin.cpp`:
///   `wndClassName = L"IPlugWndClass"`)。 経路 B が効くので信じても戻ってくる。
fn trusts_dlgcode(hwnd: HWND, class: &str) -> bool {
    let mut info = GUITHREADINFO {
        cbSize: u32::try_from(std::mem::size_of::<GUITHREADINFO>()).unwrap_or(0),
        ..Default::default()
    };
    let has_caret = unsafe { GetGUIThreadInfo(0, &mut info) }.is_ok() && info.hwndCaret == hwnd;
    has_caret || class.starts_with("JUCE_") || class == "IPlugWndClass"
}

/// 診断ログ用の窓クラス名。
fn window_class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..usize::try_from(n.max(0)).unwrap_or(0)])
}

// --- 横取りの状態 + WH_GETMESSAGE フック --------------------------------------

/// エディタ窓のキー横取りに要る状態。 plugin-main スレッドの thread-local
/// ([`ROUTER`]) が唯一の所有者で、 `PluginHost` は [`with_router`] 経由で更新するだけ。
#[derive(Default)]
pub struct KeyRouter {
    /// daw_gui から通知された 「エディタ窓で拾ってよいキー」 の一覧。
    /// **意味論 (どのキーが何をするか) は持たない**。 数値比較だけに使う。
    forwarded_keys: Vec<KeyChord>,
    /// 横取り中の key-down の chord。 対応する key-up も同じ判定で飲み込むために
    /// 覚えておく (down だけ奪うとプラグインが押しっぱなしと誤認する)。
    swallowed_keys: Vec<KeyChord>,
    /// 「キーを全部プラグインに送る」 が ON の device (逃げ道)。
    send_all_keys: HashSet<DeviceAddr>,
    /// 開いているエディタのコンテナ窓 (`device`, hwnd)。 `PluginHost` が窓を作った /
    /// 壊した瞬間に [`KeyRouter::register_editor`] / [`KeyRouter::unregister_editor`] で
    /// 更新する (フックは `PluginHost` を引けないので、 ここが hwnd → device の唯一の索引)。
    editors: Vec<(DeviceAddr, u64)>,
    /// daw_gui へ `EditorKey` を送る口。 フック設置中だけ `Some`。
    evt_tx: Option<UnboundedSender<PluginEvent>>,
    /// 張ってあるフック。 [`reinstall_hook`] で張り直す (連鎖の先頭に置き直す) ために持つ。
    hook: Option<HHOOK>,
}

thread_local! {
    static ROUTER: RefCell<KeyRouter> = RefCell::new(KeyRouter::default());
}

/// plugin-main スレッドの [`KeyRouter`] を借りて更新する。
pub fn with_router<R>(f: impl FnOnce(&mut KeyRouter) -> R) -> R {
    ROUTER.with(|r| f(&mut r.borrow_mut()))
}

impl KeyRouter {
    /// daw_gui の SHORTCUTS から導出された chord 列をそのまま保持する。
    pub fn set_forwarded_keys(&mut self, chords: Vec<KeyChord>) {
        self.forwarded_keys = chords;
        // 転送対象が変わると「down を奪ったか」の前提も変わるので残骸を捨てる。
        self.swallowed_keys.clear();
    }

    pub fn set_send_all_keys(&mut self, device: DeviceAddr, enabled: bool) {
        if enabled {
            self.send_all_keys.insert(device);
        } else {
            self.send_all_keys.remove(&device);
        }
    }

    /// コンテナ窓を作った直後に呼ぶ。
    pub fn register_editor(&mut self, device: DeviceAddr, hwnd: u64) {
        self.unregister_editor(device);
        self.editors.push((device, hwnd));
    }

    /// 窓を壊す (直前 / 直後どちらでも可) ときに呼ぶ。 対応する key-up はもう届かない
    /// ので横取り中の記録も捨てる。 残すと次に別のキーの key-up を誤って飲み込む。
    pub fn unregister_editor(&mut self, device: DeviceAddr) {
        self.editors.retain(|(id, _)| *id != device);
        self.swallowed_keys.clear();
    }

    /// `hwnd` がどの device のエディタ窓 (コンテナ本体 or その子孫) かを返す。
    fn editor_device_of(&self, hwnd: HWND) -> Option<(DeviceAddr, bool)> {
        if hwnd.is_invalid() {
            return None;
        }
        for &(device, container) in &self.editors {
            let container = HWND(container as *mut core::ffi::c_void);
            if container == hwnd {
                return Some((device, true));
            }
            if unsafe { IsChild(container, hwnd) }.as_bool() {
                return Some((device, false));
            }
        }
        None
    }

    /// プラグインエディタ由来のキーメッセージを処理する。
    ///
    /// 戻り値 `true` = **飲み込んだ** (フックが `WM_NULL` に書き換え、 プラグインへは
    /// 届かない)。 `false` = 従来どおりプラグインへ流す。
    fn handle(&mut self, msg: &MSG) -> bool {
        let relay_down = msg.message == WM_EDITOR_KEY_RELAY_DOWN;
        let relay_up = msg.message == WM_EDITOR_KEY_RELAY_UP;
        let is_relay = relay_down || relay_up;
        let is_down = relay_down || (!is_relay && is_key_down(msg.message));
        let is_up = relay_up || (!is_relay && is_key_up(msg.message));
        if !is_down && !is_up {
            return false;
        }
        // 診断: 転送対象の vk の key-down だけ、 判定の材料を info で残す (低頻度)。
        #[allow(clippy::cast_possible_truncation)]
        let vk = msg.wParam.0 as u16;
        let diag = is_down && self.forwarded_keys.iter().any(|c| c.vk == vk);
        let device = self.editor_device_of(msg.hwnd);
        if diag {
            tracing::info!(
                hwnd = format!("{:#x}", msg.hwnd.0 as usize),
                class = %window_class_name(msg.hwnd),
                vk,
                is_relay,
                repeat = is_auto_repeat(msg.lParam),
                ?device,
                editors = ?self.editors,
                "editor key-down reached hook"
            );
        }
        let Some((device, is_container)) = device else {
            return false;
        };
        // 逃げ道 (REAPER の「Send all keyboard input to plug-in」相当): この device では
        // 一切横取りしない。 relay は既にプラグインが 「要らない」 と言ったものなので
        // 飲み込む (再 dispatch しても行き場が無い) が、 転送はしない。
        if self.send_all_keys.contains(&device) {
            return is_relay;
        }
        let chord = chord_of(msg);
        if is_up {
            // down を奪ったキーだけ up も奪う (対で奪わないとプラグインが押しっぱなしと誤認)。
            if let Some(i) = self.swallowed_keys.iter().position(|c| c.vk == chord.vk) {
                self.swallowed_keys.remove(i);
                return true;
            }
            // relay 経由の up は既にプラグインが捨てたものなので行き場が無い。
            return is_relay;
        }
        if !self.forwarded_keys.contains(&chord) {
            // 転送対象外のキーは触らない。 relay 経由 (= プラグインが捨てたキー) は
            // 行き場が無いのでここで捨てる。
            return is_relay;
        }
        // 経路 A: プラグインの子窓宛に **まだ届いていない** キーは、 フォーカス窓に
        // 「このキー要る?」 を問い合わせてから決める。 コンテナ窓宛 (= relay 含む) は
        // 既にプラグインが未消化を宣言しているので問い合わせ不要。
        //
        // **オートリピート判定より前に問い合わせる**。 逆順にすると、 プラグインの
        // テキスト欄で Space を長押ししたときに 2 発目以降が無条件で飲まれ、
        // 「最初の 1 文字しか入らない」 になる。
        if !is_container && !is_relay {
            let class = window_class_name(msg.hwnd);
            let trusted = trusts_dlgcode(msg.hwnd, &class);
            if trusted && window_wants_key(msg.hwnd, msg) {
                if diag {
                    tracing::info!(vk, %class, "editor key: focus window wants it (WM_GETDLGCODE); passed to plugin");
                }
                return false;
            }
            if diag && !trusted {
                tracing::info!(vk, %class, "editor key: focus window is neither a text control nor a relaying canvas; WM_GETDLGCODE not consulted");
            }
        }
        if diag {
            tracing::info!(vk, ?device, "editor key: swallowed and forwarded to daw_gui");
        }
        if is_auto_repeat(msg.lParam) {
            // オートリピートは 1 押下 1 発火にするため **飲み込むが emit しない**。
            return true;
        }
        // 取りこぼした key-up の残骸を回収する (エディタを閉じた / フォーカスを
        // 奪われた等で up が来ないケース)。 物理的に押されていない vk は捨てる。
        self.swallowed_keys.retain(|c| unsafe { GetAsyncKeyState(i32::from(c.vk)) } < 0);
        self.swallowed_keys.push(chord);
        if let Some(tx) = self.evt_tx.as_ref() {
            let _ = tx.send(PluginEvent::EditorKey { device, chord });
        }
        true
    }
}

/// 呼び出しスレッドに張った `WH_GETMESSAGE` フックの寿命。 Drop で外す。
pub struct KeyHook;

/// plugin-main スレッドで 1 回呼ぶ。 スレッド限定フックなので DLL は要らない
/// (`hmod = None`、 `dwThreadId = GetCurrentThreadId()`)。
pub fn install_hook(evt_tx: UnboundedSender<PluginEvent>) -> KeyHook {
    with_router(|r| r.evt_tx = Some(evt_tx));
    reinstall_hook();
    KeyHook
}

/// フックを張り直して **連鎖の先頭** に置く (MSDN `SetWindowsHookEx`: 後から張った
/// フックほど先に呼ばれる)。 プラグインが自前で `WH_GETMESSAGE` / `WH_KEYBOARD` を張って
/// キーを飲むと、 先に張ってあった我々のフックには届かない。 エディタを開いた直後に
/// 呼び直して、 プラグインが GUI 生成時に張ったフックより前に出る。
pub fn reinstall_hook() {
    let prev = with_router(|r| r.hook.take());
    if let Some(prev) = prev {
        unsafe {
            let _ = UnhookWindowsHookEx(prev);
        }
    }
    let thread = unsafe { GetCurrentThreadId() };
    let hook = unsafe { SetWindowsHookExW(WH_GETMESSAGE, Some(get_message_hook), None, thread) };
    match hook {
        Ok(hook) => {
            tracing::info!(thread, "editor key hook (WH_GETMESSAGE) installed");
            with_router(|r| r.hook = Some(hook));
        }
        Err(e) => {
            tracing::error!(error = %e, "SetWindowsHookExW(WH_GETMESSAGE) failed; editor keys will not be forwarded");
        }
    }
}

impl Drop for KeyHook {
    fn drop(&mut self) {
        let hook = with_router(|r| {
            r.evt_tx = None;
            r.hook.take()
        });
        if let Some(hook) = hook {
            unsafe {
                let _ = UnhookWindowsHookEx(hook);
            }
        }
    }
}

/// `GetMsgProc`。 `wParam` は `PM_REMOVE` / `PM_NOREMOVE`、 `lParam` は `MSG*`。
/// 取り出し (`PM_REMOVE`) のときだけ判定し、 飲み込むなら `WM_NULL` へ書き換える。
/// `PM_NOREMOVE` の覗き見では触らない (同じメッセージが後で取り出されるときに判定する)。
unsafe extern "system" fn get_message_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    #[allow(clippy::cast_possible_wrap)]
    let is_action = code == HC_ACTION as i32;
    if is_action && wparam.0 == PM_REMOVE.0 as usize && lparam.0 != 0 {
        // SAFETY: WH_GETMESSAGE の lParam は呼び出し元スレッドが所有する MSG (MSDN)。
        let msg = unsafe { &mut *(lparam.0 as *mut MSG) };
        if is_key_down(msg.message) {
            // 診断: フックに届いた key-down (`RUST_LOG=daw_plugin_host::editor_keys=debug`)。
            tracing::debug!(
                vk = msg.wParam.0,
                hwnd = format!("{:#x}", msg.hwnd.0 as usize),
                class = %window_class_name(msg.hwnd),
                "hook saw key-down"
            );
        }
        // WM_GETDLGCODE の問い合わせ先 WNDPROC が更にポンプすると再入するので、
        // 借用中なら判定せず素通し (安全側 = プラグインへ渡す)。
        let swallowed = ROUTER
            .with(|r| r.try_borrow_mut().map(|mut r| r.handle(msg)).unwrap_or(false));
        if swallowed {
            msg.message = WM_NULL;
            msg.wParam = WPARAM(0);
            msg.lParam = LPARAM(0);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_repeat_reads_lparam_bit30() {
        assert!(!is_auto_repeat(LPARAM(0)));
        assert!(is_auto_repeat(LPARAM(1 << 30)));
    }

    #[test]
    fn key_message_classification() {
        assert!(is_key_down(WM_KEYDOWN));
        assert!(is_key_down(WM_SYSKEYDOWN));
        assert!(!is_key_down(WM_KEYUP));
        assert!(is_key_up(WM_KEYUP));
        assert!(is_key_up(WM_SYSKEYUP));
    }
}
