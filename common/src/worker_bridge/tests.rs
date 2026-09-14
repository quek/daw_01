use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::CreateEventA;
use windows::core::PCSTR;

use super::*;

struct Events {
    wake: HANDLE,
    done: HANDLE,
}

unsafe impl Send for Events {}
unsafe impl Sync for Events {}

impl Events {
    fn new() -> Self {
        let ev = || unsafe { CreateEventA(None, false, false, PCSTR(std::ptr::null())) }.expect("event");
        Self { wake: ev(), done: ev() }
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.wake);
            let _ = CloseHandle(self.done);
        }
    }
}

/// host の worker の真似: 自分の世代の依頼を受けて `work` を回し、token を記録して完了を書く。
fn spawn_host(
    ch: Arc<WorkerChannel>,
    ev: Arc<Events>,
    generation: u32,
    spin: Duration,
    work: Duration,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<Vec<u64>> {
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        let mut last = ch.host_start(generation);
        while let Some((r, token)) = ch.wait_request(generation, last, ev.wake, spin, &shutdown) {
            let t0 = Instant::now();
            while t0.elapsed() < work {
                std::hint::spin_loop();
            }
            seen.push(token.0);
            last = r;
            ch.complete(r, ev.done);
        }
        seen
    })
}

fn stop(host: std::thread::JoinHandle<Vec<u64>>, shutdown: &AtomicBool, ev: &Events) -> Vec<u64> {
    shutdown.store(true, Ordering::SeqCst);
    unsafe {
        let _ = windows::Win32::System::Threading::SetEvent(ev.wake);
    }
    host.join().expect("host")
}

/// 回って待つ場合も、すぐ寝る場合も、依頼は順に 1 回ずつ届いて全部完了する (相手が寝ていても起きていても)。
#[test]
fn 依頼は順に_1_回ずつ届いて完了する() {
    for (spin, gap) in [(Duration::ZERO, Duration::ZERO), (Duration::from_micros(200), Duration::ZERO), (Duration::ZERO, Duration::from_millis(2))] {
        let ch = Arc::new(WorkerChannel::new());
        let ev = Arc::new(Events::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let host = spawn_host(Arc::clone(&ch), Arc::clone(&ev), 3, spin, Duration::from_micros(50), Arc::clone(&shutdown));
        for token in 0..300u64 {
            assert_eq!(ch.dispatch(3, InstanceToken(token), ev.wake, ev.done, spin, 2_000), DispatchOutcome::Done);
            if !gap.is_zero() && token % 50 == 0 {
                std::thread::sleep(gap);
            }
        }
        assert_eq!(stop(host, &shutdown, &ev), (0..300).collect::<Vec<_>>(), "spin {spin:?} gap {gap:?}");
    }
}

/// 前の世代の依頼 (timeout したまま残ったもの) は新しい世代の host が拾わない。新しい世代の依頼は、host より先に
/// 置かれていても拾う。
#[test]
fn 前の世代の依頼は拾わず_自分の世代の依頼は起動前のものも拾う() {
    let ch = Arc::new(WorkerChannel::new());
    let ev = Arc::new(Events::new());
    assert_eq!(ch.dispatch(1, InstanceToken(11), ev.wake, ev.done, Duration::ZERO, 1), DispatchOutcome::TimedOut);

    let shutdown = Arc::new(AtomicBool::new(false));
    let host = spawn_host(Arc::clone(&ch), Arc::clone(&ev), 2, Duration::ZERO, Duration::ZERO, Arc::clone(&shutdown));
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(ch.completed.load(Ordering::SeqCst), 0, "前の世代の依頼を処理した");
    assert_eq!(ch.dispatch(2, InstanceToken(22), ev.wake, ev.done, Duration::ZERO, 2_000), DispatchOutcome::Done);
    assert_eq!(stop(host, &shutdown, &ev), vec![22]);

    // host が起きる前に置かれた依頼。
    let (ch, ev) = (Arc::new(WorkerChannel::new()), Arc::new(Events::new()));
    let early = {
        let (ch, ev) = (Arc::clone(&ch), Arc::clone(&ev));
        std::thread::spawn(move || ch.dispatch(4, InstanceToken(44), ev.wake, ev.done, Duration::ZERO, 2_000))
    };
    std::thread::sleep(Duration::from_millis(20));
    let shutdown = Arc::new(AtomicBool::new(false));
    let host = spawn_host(Arc::clone(&ch), Arc::clone(&ev), 4, Duration::ZERO, Duration::ZERO, Arc::clone(&shutdown));
    assert_eq!(early.join().expect("audio"), DispatchOutcome::Done);
    assert_eq!(stop(host, &shutdown, &ev), vec![44]);
}

/// 完了しなければ timeout で返る (無限に待たない)。
#[test]
fn 完了しなければ_timeout_で返る() {
    let ch = WorkerChannel::new();
    let ev = Events::new();
    let t0 = Instant::now();
    assert_eq!(ch.dispatch(1, InstanceToken(1), ev.wake, ev.done, Duration::from_millis(5), 30), DispatchOutcome::TimedOut);
    assert!(t0.elapsed() >= Duration::from_millis(30) && t0.elapsed() < Duration::from_millis(500));
    assert_eq!(ch.audio_parked.load(Ordering::SeqCst), 0, "寝ている印を残した");
}
