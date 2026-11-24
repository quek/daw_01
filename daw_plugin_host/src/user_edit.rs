//! r.md #160: plugin の「中身が変わった」という報告のうち、**ユーザーが窓の中で変えたもの**だけを
//! `PluginEvent::PluginStateChanged` として daw_gui へ送るための判定。
//!
//! plugin の state は Song の外 (plugin の中) にあり、daw_gui はこの event でしか「未保存」を知れない。
//! 一方、報告の口はユーザーの操作以外でも呼ばれる:
//!
//! - 読み込みの state 復元 (state を受け取った plugin が VST3 `restartComponent(kParamValuesChanged)` /
//!   CLAP `params.rescan`・`state.mark_dirty` で host に値の読み直しを求める)
//! - host が書いたオートメーション / 変調への反応、plugin 内部の変調や MIDI 割り当て
//!   (CLAP `ext/params.h` は MIDI CC で動いた値に `CLAP_EVENT_DONT_RECORD` を立てるよう求める — それは
//!   `ClapAudioHalf` が先に外す — が、立てない plugin もある)
//!
//! どちらも「開いただけ / 再生しただけで `*`」になるので送らない。規格にはユーザーの操作かどうかを
//! 区別する口が無い (gesture は任意で、VST3 の `performEdit` はプリセット切替でも来る) ので、
//! **ユーザーの手が届いていた証拠**で絞る:
//!
//! 1. その device のエディタ窓が開いている ([`EditorSession`])。閉じていれば窓の中は触れない。
//! 2. 直前にこのプロセスの窓へマウス / キーの入力があった ([`note_mouse_input`] / [`note_key_input`] →
//!    [`input_is_recent`])。
//!
//! 取りこぼし (窓の外から変わる / 入力から [`INPUT_WINDOW`] より後に報告が来る — 重いプリセットの
//! 読み込みなど) は、閉じるときの state の比較 (daw_gui の `handler::plugin_dirty`) が拾う。

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// 入力からこの時間までの報告を、その入力が起こした変更とみなす。ノブのドラッグ・クリック・キー入力は
/// 数 ms で報告が来る。プリセットの読み込みは窓の中のクリックから報告まで間が空くので長めに取る。
const INPUT_WINDOW: Duration = Duration::from_secs(2);

/// 最後のマウス入力 / キー入力の時刻 ([`epoch`] からの ms + 1。`0` = 無い)。書くのは plugin-main の
/// message hook (`editor_keys`)、読むのは報告の callback (plugin-main / RT worker)。
static LAST_MOUSE_MS: AtomicU64 = AtomicU64::new(0);
static LAST_KEY_MS: AtomicU64 = AtomicU64::new(0);

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn stamp_now() -> u64 {
    u64::try_from(epoch().elapsed().as_millis()).unwrap_or(u64::MAX - 1).saturating_add(1)
}

/// このプロセスの窓へマウスの入力 (ボタン / ホイール / ボタンを押したままの移動) が届いた。
pub fn note_mouse_input() {
    LAST_MOUSE_MS.store(stamp_now(), Ordering::Relaxed);
}

/// キーの押下が plugin へ渡った。
pub fn note_key_input() {
    LAST_KEY_MS.store(stamp_now(), Ordering::Relaxed);
}

/// 直前のキーを plugin は使わなかった (エディタ窓へ戻ってきて daw_gui のショートカットになった)。
/// 入力に数えたままにすると、plugin の窓で Space を押して再生を始めただけで、続く 2 秒間の報告
/// (内部の変調 / オートメーションへの反応) がユーザーの変更になる。
pub fn retract_key_input() {
    LAST_KEY_MS.store(0, Ordering::Relaxed);
}

/// 直前 ([`INPUT_WINDOW`] 以内) に入力があったか。確保もロックもしない (RT worker から呼べる)。
pub fn input_is_recent() -> bool {
    let now = stamp_now();
    let recent = |stamp: u64| stamp != 0 && now.saturating_sub(stamp) <= INPUT_WINDOW.as_millis() as u64;
    recent(LAST_MOUSE_MS.load(Ordering::Relaxed)) || recent(LAST_KEY_MS.load(Ordering::Relaxed))
}

/// 1 つの plugin instance のエディタ窓が開いているか。`gui_set_parent_hwnd` で開き、`gui_destroy` が
/// **plugin の後始末を呼び終えてから**閉じる (閉じるときに確定する編集 — 入力途中の数値欄 — を拾うため。
/// `HostCallbacks::editor_hwnd` は後始末の前に 0 へ落ちるので使えない)。
#[derive(Clone, Default)]
pub struct EditorSession(Arc<AtomicBool>);

impl EditorSession {
    pub fn open(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn close(&self) {
        self.0.store(false, Ordering::Release);
    }

    /// plugin が今「中身が変わった」と報告したら、ユーザーの手による変更とみなすか (module doc の 1 と 2)。
    pub fn user_is_editing(&self) -> bool {
        self.0.load(Ordering::Acquire) && input_is_recent()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 窓が閉じている / 入力が無い間の報告は、ユーザーの変更にしない (開いただけ・再生しただけで `*` にしない)。
    #[test]
    fn a_report_counts_only_while_the_editor_is_open_and_input_was_recent() {
        let session = EditorSession::default();
        LAST_MOUSE_MS.store(0, Ordering::Relaxed);
        LAST_KEY_MS.store(0, Ordering::Relaxed);
        session.open();
        assert!(!session.user_is_editing(), "窓は開いているが入力が無い (内部の変調 / オートメーションへの反応)");

        note_key_input();
        assert!(session.user_is_editing());
        retract_key_input();
        assert!(!session.user_is_editing(), "plugin が使わなかったキー (Space で再生) は入力に数えない");

        note_mouse_input();
        assert!(session.user_is_editing());
        session.close();
        assert!(!session.user_is_editing(), "窓が閉じている (読み込みの state 復元)");
    }
}
