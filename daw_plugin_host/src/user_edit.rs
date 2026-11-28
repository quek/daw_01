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
//! 取りこぼし (報告しない plugin / 入力から [`INPUT_WINDOW`] より後に報告が来る — 重いプリセットの
//! 読み込みなど) は、閉じるときの state の比較 (daw_gui の `handler::plugin_dirty`) が拾う。比べる device は
//! 同じ証拠で絞る ([`EditorSession::touch`])。

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// 入力からこの時間までの報告を、その入力が起こした変更とみなす。ノブのドラッグ・クリック・キー入力は
/// 数 ms で報告が来る。プリセットの読み込みは窓の中のクリックから報告まで間が空くので長めに取る。
const INPUT_WINDOW: Duration = Duration::from_secs(2);

/// 最後のマウス入力 / キー入力の時刻 ([`next_stamp`] の値。`0` = 無い)。書くのは plugin-main の
/// message hook (`editor_keys`)、読むのは報告の callback (plugin-main / RT worker)。
static LAST_MOUSE_MS: AtomicU64 = AtomicU64::new(0);
static LAST_KEY_MS: AtomicU64 = AtomicU64::new(0);

/// 最後に記録したキー: 仮想キーコード + 1 (`0` = 無い / 取り消した) と、それを記録する前の [`LAST_KEY_MS`]。
/// [`retract_key_input`] が「そのキーの分だけ」戻すのに使う。読み書きは plugin-main の message hook だけ。
static LAST_KEY_VK: AtomicU64 = AtomicU64::new(0);
static KEY_MS_BEFORE_LAST: AtomicU64 = AtomicU64::new(0);

/// plugin が使わなかったキーが戻ってくるまでの時間の上限 (同じ pump の中か、次の message)。これより前に
/// 記録したキーは、同じキーでも別の押下なので取り消さない。
const KEY_BOUNCE_WINDOW_MS: u64 = 500;

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// 今の時刻 ([`epoch`] からの ms + 1)。
fn stamp_now() -> u64 {
    u64::try_from(epoch().elapsed().as_millis()).unwrap_or(u64::MAX - 1).saturating_add(1)
}

/// 入力と「ここまで見た」の印に振る時刻。[`stamp_now`] 以上で、**呼ぶたびに必ず増える** — 同じ ms に届いた
/// 入力と印の前後が、値の大小でそのまま分かる。plugin-main からだけ呼ぶ。
fn next_stamp() -> u64 {
    static CLOCK: AtomicU64 = AtomicU64::new(0);
    let now = stamp_now();
    let mut prev = CLOCK.load(Ordering::Relaxed);
    loop {
        let next = prev.max(now - 1) + 1;
        match CLOCK.compare_exchange_weak(prev, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(seen) => prev = seen,
        }
    }
}

/// このプロセスの窓へポインタの入力 (ボタン / ホイール / ボタンを押したままの移動 / ペン・タッチ / ファイルの
/// drop) が届いた。
pub fn note_mouse_input() {
    LAST_MOUSE_MS.store(next_stamp(), Ordering::Relaxed);
}

/// キー `vk` (仮想キーコード) の押下が plugin へ渡った。
pub fn note_key_input(vk: usize) {
    KEY_MS_BEFORE_LAST.store(LAST_KEY_MS.load(Ordering::Relaxed), Ordering::Relaxed);
    LAST_KEY_MS.store(next_stamp(), Ordering::Relaxed);
    LAST_KEY_VK.store(vk as u64 + 1, Ordering::Relaxed);
}

/// キー `vk` を plugin は使わなかった (エディタ窓へ戻ってきて daw_gui のショートカットになった)。
/// 入力に数えたままにすると、plugin の窓で Space を押して再生を始めただけで、続く 2 秒間の報告
/// (内部の変調 / オートメーションへの反応) がユーザーの変更になる。
///
/// 戻すのは **直前に記録したそのキーの分だけ** — 最初から plugin へ渡さなかったキーでも呼ばれるので、
/// 無条件に消すと、その前に plugin が使ったキー (数値欄への入力) まで無かったことになる。
pub fn retract_key_input(vk: usize) {
    let last = LAST_KEY_MS.load(Ordering::Relaxed);
    let bounced = LAST_KEY_VK.load(Ordering::Relaxed) == vk as u64 + 1
        && stamp_now().saturating_sub(last) <= KEY_BOUNCE_WINDOW_MS;
    if bounced {
        LAST_KEY_MS.store(KEY_MS_BEFORE_LAST.load(Ordering::Relaxed), Ordering::Relaxed);
        LAST_KEY_VK.store(0, Ordering::Relaxed);
    }
}

/// 直前 ([`INPUT_WINDOW`] 以内) に入力があったか。確保もロックもしない (RT worker から呼べる)。
pub fn input_is_recent() -> bool {
    let now = stamp_now();
    let recent = |stamp: u64| stamp != 0 && now.saturating_sub(stamp) <= INPUT_WINDOW.as_millis() as u64;
    recent(LAST_MOUSE_MS.load(Ordering::Relaxed)) || recent(LAST_KEY_MS.load(Ordering::Relaxed))
}

/// daw_gui が plugin の state を取り直した回数 (`RequestAllStates`)。daw_gui の未保存の印は「最後に state を
/// 取り直してから変更があったか」だけを見るので、報告は取り直しごとに device あたり 1 回で足りる
/// (ノブのドラッグは毎秒数百回の報告になる)。
static STATE_REQUESTS: AtomicU64 = AtomicU64::new(1);

/// daw_gui が state を取り直す。**state を集める前に**呼ぶ — 集めている最中の変更 (RT worker の値) は
/// 集めた state に入っている保証が無いので、次の報告として送らせる。
pub fn note_state_requested() {
    STATE_REQUESTS.fetch_add(1, Ordering::Relaxed);
}

/// 1 つの plugin instance のエディタ窓が開いているか、報告を送ったか、窓へ入力が届いたか。窓は
/// `gui_set_parent_hwnd` で開き、`gui_destroy` が **plugin の後始末を呼び終えてから**閉じる (閉じるときに
/// 確定する編集 — 入力途中の数値欄 — を拾うため。`HostCallbacks::editor_hwnd` は後始末の前に 0 へ落ちるので
/// 使えない)。
#[derive(Clone, Default)]
pub struct EditorSession(Arc<SessionState>);

#[derive(Default)]
struct SessionState {
    open: AtomicBool,
    /// 最後に報告を送ったときの [`STATE_REQUESTS`] (`0` = まだ送っていない)。
    reported_at: AtomicU64,
    /// 窓を開いた時刻 ([`next_stamp`] の値)。
    opened_at: AtomicU64,
    /// ここまでの入力は [`Self::touch`] に入れた (窓を開いた時刻 / 最後に見た時刻。[`next_stamp`] の値)。
    input_mark: AtomicU64,
    /// 窓が開いている間に届いた最後の入力の印 ([`EditorSession::touch`]。`0` = 届いていない)。
    touch: AtomicU64,
}

/// 最後の入力の時刻 ([`next_stamp`] の値。`0` = 無い)。
fn last_input_stamp() -> u64 {
    LAST_MOUSE_MS.load(Ordering::Relaxed).max(LAST_KEY_MS.load(Ordering::Relaxed))
}

/// 新しい入力の印。同じ instance が前に出した値と重ならない。instance の作り直しやこのプロセスの起動し直しを
/// またいでも重ならないようにしてある (起動時刻の ms に通し番号を足す) が、daw_gui は載せ直した instance を
/// `0` から数え直すので、それに頼ってはいない。
fn next_touch() -> u64 {
    static BASE: OnceLock<u64> = OnceLock::new();
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let base = *BASE.get_or_init(|| {
        let since_epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        u64::try_from(since_epoch.as_millis()).unwrap_or(0)
    });
    base.saturating_add(SEQ.fetch_add(1, Ordering::Relaxed))
}

/// 窓を閉じるときに確定する入力途中の欄があり得るか: 窓を開いた (`opened_at`) 後の最後の入力がキー。
fn pending_text_may_commit(opened_at: u64, last_key: u64, last_mouse: u64) -> bool {
    last_key > opened_at && last_key > last_mouse
}

impl EditorSession {
    pub fn open(&self) {
        let now = next_stamp();
        self.0.opened_at.store(now, Ordering::Relaxed);
        self.0.input_mark.store(now, Ordering::Relaxed);
        self.0.open.store(true, Ordering::Release);
    }

    pub fn close(&self) {
        self.count_input();
        // 入力途中の欄は、窓を閉じるときに確定する。その入力は前の読み取りで数え終わっているかもしれない
        // (打ちかけて保存 → 窓を閉じる) ので、開いている間の最後の入力がキーなら、閉じたことを入力に数える。
        let (key, mouse) = (LAST_KEY_MS.load(Ordering::Relaxed), LAST_MOUSE_MS.load(Ordering::Relaxed));
        let was_open = self.0.open.swap(false, Ordering::AcqRel);
        if was_open && pending_text_may_commit(self.0.opened_at.load(Ordering::Relaxed), key, mouse) {
            tracing::info!("editor closed after key input; counting the close as input (a pending text field may commit)");
            self.0.touch.store(next_touch(), Ordering::Relaxed);
        }
    }

    /// 窓が開いている間に、前に見た後の入力があれば印を新しくする。
    fn count_input(&self) {
        let now = next_stamp();
        if self.count_input_at(last_input_stamp(), now) {
            // 「なぜ保存を聞かれたか」を後から追えるように残す (state の取り直しか窓を閉じるときに、多くて 1 行)。
            let ago = |stamp: u64| (stamp != 0).then(|| now.saturating_sub(stamp));
            tracing::info!(
                mouse_ms_ago = ?ago(LAST_MOUSE_MS.load(Ordering::Relaxed)),
                key_ms_ago = ?ago(LAST_KEY_MS.load(Ordering::Relaxed)),
                "user input reached a plugin window while this editor was open"
            );
        }
    }

    /// [`Self::count_input`] の中身 (`last_input` = 最後の入力の時刻、`now` = 今)。印を新しくしたら `true`。
    fn count_input_at(&self, last_input: u64, now: u64) -> bool {
        let s = &self.0;
        let reached = s.open.load(Ordering::Acquire) && last_input > s.input_mark.load(Ordering::Relaxed);
        if reached {
            s.touch.store(next_touch(), Ordering::Relaxed);
        }
        s.input_mark.store(now, Ordering::Relaxed);
        reached
    }

    /// この instance のエディタ窓が開いている間に届いた最後の入力の印 (`0` = 届いていない。値そのものに
    /// 意味は無く、**前に読んだ値と違えば、その間に窓へユーザーの手が届いた**)。daw_gui は state と一緒に
    /// 受け取り、閉じるときの state の比較をこの値が変わった device に絞る — 触っていなくても取るたびに bytes が
    /// 変わる plugin (末尾に未初期化の領域を書く / zip の時刻を含める) が多く、bytes だけでは変更と区別できない。
    /// 入力はこのプロセスの窓全体で 1 つに見るので、同時に開いていた別の窓の分も変わる (聞く側に倒れる)。
    /// state を集める直前に plugin-main から呼ぶ。
    #[must_use]
    pub fn touch(&self) -> u64 {
        self.count_input();
        self.0.touch.load(Ordering::Relaxed)
    }

    /// plugin が今「中身が変わった」と報告したら、ユーザーの手による変更とみなすか (module doc の 1 と 2)。
    fn user_is_editing(&self) -> bool {
        self.0.open.load(Ordering::Acquire) && input_is_recent()
    }

    /// plugin が「中身が変わった」と報告した。daw_gui へ送るなら `true`: ユーザーの変更で、かつ daw_gui が
    /// 最後に state を取り直してからまだ送っていない。確保もロックもしない (RT worker から呼べる)。
    pub fn should_report(&self) -> bool {
        self.should_report_at(STATE_REQUESTS.load(Ordering::Relaxed))
    }

    /// [`Self::should_report`] の中身 (`requests` = 今の [`STATE_REQUESTS`])。
    fn should_report_at(&self, requests: u64) -> bool {
        self.user_is_editing() && self.0.reported_at.swap(requests, Ordering::Relaxed) != requests
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

        const SPACE: usize = 0x20;
        const DIGIT_1: usize = 0x31;
        note_key_input(SPACE);
        assert!(session.user_is_editing());
        retract_key_input(SPACE);
        assert!(!session.user_is_editing(), "plugin が使わなかったキー (Space で再生) は入力に数えない");

        // plugin が使ったキー (数値欄への入力) は、後から来た別のキーの取り消しでは消えない
        // (最初から plugin へ渡さなかったキーでも取り消しは呼ばれる)。
        note_key_input(DIGIT_1);
        retract_key_input(SPACE);
        assert!(session.user_is_editing());
        retract_key_input(DIGIT_1);
        assert!(!session.user_is_editing());

        note_mouse_input();
        assert!(session.user_is_editing());

        // 報告は、daw_gui が state を取り直すたびに 1 回だけ送る (ドラッグ中の毎回は送らない)。
        // (入力の時刻はプロセス全体の static なので、並走する別の test に分けない。取り直しの回数も static で、
        // 他の test の `RequestAllStates` が進めるので、ここは値を渡して見る。)
        assert!(session.should_report_at(1));
        assert!(!session.should_report_at(1), "同じ取り直しの間の 2 回目");
        assert!(session.should_report_at(2), "取り直した後の変更はまた送る");

        session.close();
        assert!(!session.user_is_editing(), "窓が閉じている (読み込みの state 復元)");
        assert!(!session.should_report_at(3));
    }

    /// 入力の印は、窓が開いている間に届いた入力でだけ変わる。閉じる直前の入力は閉じた後に読んでも残る。
    #[test]
    fn the_touch_mark_changes_only_for_input_that_arrived_while_the_editor_was_open() {
        let session = EditorSession::default();
        let touch = || session.0.touch.load(Ordering::Relaxed);
        session.count_input_at(50, 60);
        assert_eq!(touch(), 0, "窓が閉じている間の入力 (別の plugin の窓)");

        session.0.input_mark.store(100, Ordering::Relaxed);
        session.0.open.store(true, Ordering::Release);
        session.count_input_at(90, 110);
        assert_eq!(touch(), 0, "窓を開く前の入力");
        session.count_input_at(120, 130);
        let first = touch();
        assert_ne!(first, 0);
        session.count_input_at(120, 140);
        assert_eq!(touch(), first, "同じ入力で 2 度変えない");

        session.count_input_at(150, 160);
        let second = touch();
        assert_ne!(second, first);
        session.0.open.store(false, Ordering::Release);
        session.count_input_at(170, 180);
        assert_eq!(touch(), second, "閉じる前の入力は残り、閉じた後の入力では変わらない");
    }
}
