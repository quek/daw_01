//! OS カーソルの形状要求 ([`Ui::set_cursor`]) と「打っている間は隠す」
//! ([`Ui::hide_cursor_until_pointer_moves`])。フレーム末尾に [`CursorState::flush`] が OS へ送る。

use daw_ui_platform::CursorIcon;

use super::Ui;
use crate::input::PointerFrame;

/// 隠したカーソルを戻す移動量 (物理 px)。0 にすると、打鍵の振動でマウスが 1px 動いただけで戻る。
const CURSOR_UNHIDE_DISTANCE_PX: f32 = 3.0;

/// `UiHost` が持つカーソルの状態。
#[derive(Default)]
pub(crate) struct CursorState {
    /// 今フレームに `Ui::set_cursor` で要求された形状 (last call wins)。フレーム頭で消える。
    requested: Option<CursorIcon>,
    /// 今フレームに `Ui::hide_cursor_until_pointer_moves` が呼ばれたか。フレーム頭で消える。
    hide_requested: bool,
    /// 隠している間、隠した時点のポインタ位置。`None` = 隠していない。フレームをまたいで残る。
    hidden_at: Option<(f32, f32)>,
    /// 最後に OS へ送った形状。「前回と違うときだけ送る」ための dedup (毎フレーム Win32 を
    /// 叩かないため)。`None` = 未送信。
    last_flushed: Option<CursorIcon>,
}

impl CursorState {
    pub(super) fn begin_frame(&mut self) {
        self.requested = None;
        self.hide_requested = false;
    }

    /// フレーム末尾に、今フレームの要求を `sink` (`WindowBackend::set_cursor`) へ送る。
    ///
    /// **per-frame セマンティクス** (daw_01 r.md #50): 誰も要求しなかったフレームは `Default` を
    /// 送る。winit / OS 側は state-full なので、要求が無いときに何も送らないと前フレームの形状が
    /// 残り続ける。そのため旧仕様では *全 widget* が「自分の担当矩形の中なら明示的に Default」
    /// という同じ boilerplate を書く必要があり、書き忘れた widget の上でカーソルが貼り付いた
    /// (arrangement / piano_roll は書いていた、master_panel は書き忘れた = 設計欠陥のシグナル)。
    /// 実際に OS へ送るのは **前回と違うときだけ**。
    ///
    /// 「隠す」は形状の要求より強い: 隠している間は、ポインタの下の widget が形を要求していても
    /// `Hidden` を送る (last-call-wins に乗せると、打鍵した欄より後に描かれる widget の hover に
    /// 負ける)。ポインタが隠した位置から動くか、ボタンが押されたら戻す。
    pub(super) fn flush(
        &mut self,
        pointer: &PointerFrame,
        sink: Option<&(dyn Fn(CursorIcon) + Send + Sync)>,
    ) {
        let pressed = pointer.primary_just_pressed
            || pointer.primary_pressed
            || pointer.secondary_just_pressed;
        if let Some((hx, hy)) = self.hidden_at {
            let stayed = pointer
                .pos
                .is_some_and(|(x, y)| (x - hx).hypot(y - hy) <= CURSOR_UNHIDE_DISTANCE_PX);
            if !stayed || pressed {
                self.hidden_at = None;
            }
        }
        // ボタンが押されているフレームの要求は捨てる: 変換中に欄の外を click すると、IME の確定が
        // press と同じフレームに届く。そこで隠すと、click した場所でカーソルが消えたままになる。
        if std::mem::take(&mut self.hide_requested) && !pressed {
            // 打鍵のたびに基準を取り直す (打ちながら少し動いた分で戻らない)。窓の外なら隠す相手が無い。
            self.hidden_at = pointer.pos;
        }
        let want = if self.hidden_at.is_some() {
            CursorIcon::Hidden
        } else {
            self.requested.take().unwrap_or(CursorIcon::Default)
        };
        if self.last_flushed != Some(want)
            && let Some(sink) = sink
        {
            sink(want);
            self.last_flushed = Some(want);
        }
    }
}

impl<M: ?Sized + 'static> Ui<'_, M> {
    /// このフレーム末尾に OS カーソル形状を変更要求する。同フレーム内で複数回呼ばれた
    /// 場合は **last call wins** (= 直前 widget の要求は捨てられる)。
    ///
    /// `WindowBackend::set_cursor` callback が registered されていなければ no-op
    /// (low-level constructor [`UiHost::new`](super::UiHost::new) /
    /// [`UiHost::no_redraw`](super::UiHost::no_redraw) で構築した場合)。
    /// 通常 [`UiHost::with_window`](super::UiHost::with_window) で構築すれば自動的に有効化される。
    ///
    /// **誰も `set_cursor` を呼ばなかったフレームは自動的に `Default` に戻る** (理由は
    /// [`CursorState::flush`])。したがって widget は「自分が形を変えたいときだけ呼ぶ」でよい。
    pub fn set_cursor(&mut self, cursor: CursorIcon) {
        self.cursor_state.requested = Some(cursor);
    }

    /// OS カーソルを隠し、**ポインタを動かすかボタンを押すまで** 隠したままにする
    /// (文字を打っている間、打っている場所に重なるカーソルをどける。OS の「入力中はポインターを
    /// 非表示にする」と同じ挙動)。
    ///
    /// 1 回呼べば以後のフレームは呼ばなくても隠れ続け、戻すのは `UiHost` が行う (呼び出し側は
    /// 「打鍵があったフレームに呼ぶ」だけでよい — 入力欄を閉じた後も、動かすまで出てこない)。
    /// 隠している間は [`Self::set_cursor`] の要求より優先される。
    pub fn hide_cursor_until_pointer_moves(&mut self) {
        self.cursor_state.hide_requested = true;
    }
}
