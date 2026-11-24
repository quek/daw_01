//! handler::follow_global — フォローアクションの全体 on/off (r.md #161、Live の Enable Follow Actions Globally)。
//!
//! off にすると、クリップと列のフォローアクションが発火しなくなり、鳴っているセルはそのまま回り続ける
//! (他のセルへ飛ばずに編集するためのスイッチ。ループしないセルは最後まで鳴って止まる)。on に戻すと engine が
//! 発火の格子の次の境界から再開する (off にする前に発火済みで量子化待ちの飛び先は、そのまま実行される)。
//!
//! **聴き方の都合** なので Song に書かない (`LauncherUiState::follow_actions_off`、SC Listen と同じ扱い:
//! 保存も undo もされず `*` も立たない。書き出しは常に on で走る)。状態はプロジェクト (タブ) ごとで、
//! プロジェクトを開くと常に on。MIDI の割り当て (`BindingTarget::ToggleFollowActions`) だけは Song に載る。

use common::protocol::AudioCommand;

use crate::state::AppData;

impl AppData {
    /// フォローアクションが全体で効いているか (既定 = 効いている)。
    #[must_use]
    pub fn follow_actions_enabled(&self) -> bool {
        !self.cur.launcher.follow_actions_off
    }

    /// トランスポートのボタン / MIDI からの切り替え。有効なフォローアクションが 1 つも無いプロジェクトでは
    /// 切り替えるものが無い (ボタンはグレー表示) ので **off にはできない**。on へ戻すほうは常に受ける —
    /// off のまま最後のフォローアクションを消すとボタンはグレーになるので、ここで断ると「見えない off」が
    /// 残り、後で足したフォローアクションが黙って発火しない。
    pub(crate) fn request_follow_actions_enabled(&mut self, enabled: bool) {
        if !enabled && !self.cur.song_doc.song().has_follow_actions() {
            self.ui_ephemeral.status_message = "フォローアクションがありません".into();
            return;
        }
        self.set_follow_actions_enabled(enabled);
        self.ui_ephemeral.status_message =
            if enabled { "フォローアクション: on" } else { "フォローアクション: off (全体)" }.into();
    }

    /// `follow_actions_off` の書き込みと `SetFollowActionsEnabled` の送信の唯一の口。
    /// respawn 後の再送 (`restore_tabs_after_respawn`) と、プロジェクトを開いたときの on への戻し
    /// (`reset_song_scoped_state`) も同じ値でここを通す。
    pub(crate) fn set_follow_actions_enabled(&mut self, enabled: bool) {
        self.cur.launcher.follow_actions_off = !enabled;
        self.send_audio(AudioCommand::SetFollowActionsEnabled { project: self.pk(), enabled });
    }
}

#[cfg(test)]
mod tests {
    /// プロジェクトを開く / 作り直す (同じタブの Song の差し替え) と常に on へ戻る (off は保存されない)。
    #[test]
    fn replacing_the_song_turns_follow_actions_back_on() {
        let mut app = crate::test_support::headless_app();
        app.set_follow_actions_enabled(false);
        assert!(!app.follow_actions_enabled());

        app.after_song_replaced();

        assert!(app.follow_actions_enabled());
    }
}
