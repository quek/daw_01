//! plugin-main の thread timer (`SetTimer(hwnd = NULL)`)。`WM_TIMER` がこの thread の
//! キューに入って `GetMessageW` を起こす。
//!
//! **id は OS が決める**。MSDN `SetTimer`: "If the hWnd parameter is NULL, and the nIDEvent
//! does not match an existing timer then it is ignored and a new timer ID is generated."
//! 渡した id で `WM_TIMER` の `wParam` を照合しても一致しないので、戻り値を持って照合する
//! (旧 ARA timer は定数 `1` と比べていて、通常の loop では一度も発火扱いにならなかった)。

use windows::Win32::UI::WindowsAndMessaging::{KillTimer, MSG, SetTimer, WM_TIMER};

pub struct ThreadTimer {
    interval_ms: u32,
    /// `SetTimer` の戻り値。0 = 止まっている (`SetTimer` は失敗時に 0 を返す)。
    id: usize,
    /// `SetTimer` の失敗を warn 済みか (`sync` は loop の毎周回から呼ばれ、失敗が続く間は
    /// 毎回やり直すので、ログは 1 回に絞る)。
    warned: bool,
}

impl ThreadTimer {
    pub const fn new(interval_ms: u32) -> Self {
        Self { interval_ms, id: 0, warned: false }
    }

    /// `want` に合わせて回す / 止める (既にその状態なら何もしない)。
    pub fn sync(&mut self, want: bool) {
        if want && self.id == 0 {
            self.id = unsafe { SetTimer(None, 0, self.interval_ms, None) };
            if self.id == 0 && !self.warned {
                self.warned = true;
                tracing::warn!(interval_ms = self.interval_ms, "SetTimer failed");
            }
        } else if !want && self.id != 0 {
            let _ = unsafe { KillTimer(None, self.id) };
            self.id = 0;
        }
    }

    /// `msg` がこの timer の `WM_TIMER` か。
    pub fn fired(&self, msg: &MSG) -> bool {
        self.id != 0 && msg.message == WM_TIMER && msg.hwnd.0.is_null() && msg.wParam.0 == self.id
    }
}

impl Drop for ThreadTimer {
    fn drop(&mut self) {
        self.sync(false);
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::UI::WindowsAndMessaging::GetMessageW;

    use super::*;

    /// 2 本同時に回して、それぞれの `WM_TIMER` を自分のものと見分けられる
    /// (OS が振る id で照合している) こと。
    #[test]
    fn two_timers_tell_their_own_ticks_apart() {
        let mut fast = ThreadTimer::new(10);
        let mut slow = ThreadTimer::new(25);
        fast.sync(true);
        slow.sync(true);
        // 張れていないと下の `GetMessageW` が永久に待つ。
        assert!(fast.id != 0 && slow.id != 0 && fast.id != slow.id);
        let (mut fast_ticks, mut slow_ticks) = (0, 0);
        let mut msg = MSG::default();
        while fast_ticks == 0 || slow_ticks == 0 {
            assert!(unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0);
            assert!(!(fast.fired(&msg) && slow.fired(&msg)));
            fast_ticks += u32::from(fast.fired(&msg));
            slow_ticks += u32::from(slow.fired(&msg));
        }
    }
}
