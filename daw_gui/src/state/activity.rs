//! r.md #49: 「daw_01 の窓がアクティブか」の SSoT と、そこから導く省電力判定。
//!
//! アクティブ判定を 1 箇所に集めるのは、材料が **3 つの別々の経路**から届くため:
//!
//! - メインウィンドウ … winit の `WindowEvent::Focused`
//! - 動画プレビュー窓 … 同上 (別 `WindowId`)
//! - プラグインエディタ窓 … **別プロセス (daw_plugin_host) からの IPC**
//!
//! 3 つ目が IPC なのは妥協ではなく必然で、エディタ窓は daw_plugin_host が所有する
//! owner 無し top-level でなければならない (daw_gui を owner にすると
//! `GetAncestor(GA_ROOTOWNER)` が daw_gui に解決し、JUCE の cascade サブメニューが
//! `isForegroundProcess()` 判定で即 dismiss される)。つまりプラグイン GUI 操作中の
//! daw_gui は非フォーカスどころか foreground プロセスですらなく、**自分の中の情報
//! だけでは原理的に「アプリはアクティブ」と判定できない**。

/// アクティブ状態の生の材料と、daw_audio へ最後に送った値。session-only
/// (保存しない / undo しない / dirty にしない)。
#[derive(Debug)]
pub struct ActivityState {
    /// 背景スレッドと共有する「今 省電力に入っていないか」
    /// (= [`should_keep_rendering`] の最新値)。`EventLoopProxy` でイベントを
    /// 送る側が自分で止まれるようにするためのもの。
    ///
    /// **イベントを送らせない**のが目的で、送られたものを捨てるのでは意味が薄い
    /// (sysinfo poller の `refresh_processes(All)` は全プロセス列挙で、
    /// 送る前の poll 自体が重い)。
    pub awake: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// メインウィンドウが focus を持っているか。
    pub main_focused: bool,
    /// 動画プレビュー窓が focus を持っているか。
    pub preview_focused: bool,
    /// daw_plugin_host が所有する窓 (= プラグインエディタ) がアクティブか。
    /// `PluginEvent::HostWindowsActive` で更新。
    pub plugin_host_active: bool,
    /// 直近に `AudioCommand::SetAppActive` で送った値。変化時だけ送るための
    /// dedup。`None` = 未送信 (起動直後に必ず 1 回送る)。
    pub last_sent_app_active: Option<bool>,
    /// 常にアクティブとみなす (自動テスト用)。
    ///
    /// `--smoke-test` は背景スレッドがプログラム的に再生を駆動し、preview 窓の
    /// client area を `PrintWindow` で pixel capture して検証する。この窓が
    /// フォーカスを得るかは**実行環境次第** (CI / 裏で走らせたとき / 他アプリが
    /// 前面を奪ったとき) なので、省電力に入ると **描画されていない窓を撮って
    /// 「真っ黒 = 回帰」と誤検出**しうる。テストの検証対象は描画結果であって
    /// 省電力ではないため、その経路だけ判定を固定する。
    pub force_active: bool,
}

impl Default for ActivityState {
    fn default() -> Self {
        Self {
            main_focused: false,
            preview_focused: false,
            plugin_host_active: false,
            last_sent_app_active: None,
            force_active: false,
            // 起動直後は起きている状態から始める (最初の判定が届くまで
            // 背景スレッドを止めない = 「起動したのに何も動かない」を避ける)。
            awake: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }
}

impl ActivityState {
    /// daw_01 の窓のいずれかがアクティブか。
    #[must_use]
    pub fn app_windows_active(&self) -> bool {
        self.force_active
            || self.main_focused
            || self.preview_focused
            || self.plugin_host_active
    }
}

/// 画面を描き続けるべきか (= 省電力に入らないか)。
///
/// - `busy` … 進捗を見せている最中 (書き出し / VOICEVOX 合成 / プラグイン検索)。
///   非アクティブでも進捗表示は動き続ける、という決定に対応する
/// - `rolling` … 再生 or 録音中。r.md #49 の条件は「**再生停止中かつ**アクティブでない」
///   なので、走っている間は非アクティブでも止めてはいけない
#[must_use]
pub fn should_keep_rendering(windows_active: bool, busy: bool, rolling: bool) -> bool {
    windows_active || busy || rolling
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_window_counts_as_active() {
        // (main, preview, plugin_host, expected)
        let cases = [
            (false, false, false, false),
            (true, false, false, true),
            (false, true, false, true),
            // プラグインのつまみを回している間 = メインも preview も非 focus だが
            // アプリとしてはアクティブ。これを取りこぼすと演奏中に音が切れる。
            (false, false, true, true),
        ];
        for (main, preview, host, expected) in cases {
            let a = ActivityState {
                main_focused: main,
                preview_focused: preview,
                plugin_host_active: host,
                ..Default::default()
            };
            assert_eq!(
                a.app_windows_active(),
                expected,
                "main={main} preview={preview} host={host}"
            );
        }
    }

    #[test]
    fn only_stopped_and_inactive_stops_rendering() {
        // (windows_active, busy, rolling, expected)
        let cases = [
            (false, false, false, false), // これだけが省電力に入る条件
            (true, false, false, true),
            // 書き出し / 合成 / 検索の進捗は裏に回しても動き続ける。
            (false, true, false, true),
            // r.md #49 の条件は「**再生停止中かつ**アクティブでない」。裏で再生 /
            // 録音している間に画面を止めると、再生中なのにフレームが 1 枚も出ない。
            (false, false, true, true),
        ];
        for (active, busy, rolling, expected) in cases {
            assert_eq!(
                should_keep_rendering(active, busy, rolling),
                expected,
                "active={active} busy={busy} rolling={rolling}"
            );
        }
    }
}
