#![allow(unsafe_op_in_unsafe_fn)]

//! Host-side `IParameterChanges` / `IParamValueQueue` for feeding parameter
//! automation into a VST3 plugin's `process()` (`ProcessData.
//! inputParameterChanges`).
//!
//! Mirrors `vst3_events.rs`: the host owns reusable `ComWrapper` instances
//! for the plugin's lifetime and refills them via `set_changes` before every
//! `process()`. `UnsafeCell` keeps the audio-thread access lock-free; the
//! audio thread is single-threaded so there is no real sharing.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use com_scrape_types::Class;
use vst3::ComWrapper;
use vst3::Steinberg::{
    int32, kInvalidArgument, kResultFalse, kResultOk, tresult,
    Vst::{
        IParamValueQueue, IParamValueQueueTrait, IParameterChanges, IParameterChangesTrait,
        ParamID, ParamValue,
    },
};

use crate::plugin_instance::TimedParamEvent;

/// Max distinct parameters automated in a single buffer. Pre-allocated pool
/// size so `set_changes` never heap-allocates in the audio thread. Real
/// automation rarely touches more than a handful of params per buffer; excess
/// distinct params in one buffer are dropped (logged once via the caller).
const MAX_PARAM_QUEUES: usize = 64;

/// One parameter's automation points for the current buffer. The host fills
/// it via `reset` + `push`; the plugin reads it through the
/// `IParamValueQueue` vtable during `process()`.
pub struct Vst3ParamValueQueue {
    param_id: UnsafeCell<u32>,
    /// `(sampleOffset, normalized value)`. Pushed in event order; daw_audio
    /// emits param events in ascending time so offsets stay monotonic
    /// (VST3 spec requirement for a value queue).
    points: UnsafeCell<Vec<(i32, f64)>>,
}

// SAFETY: only the audio thread touches a queue during `process()`; the
// host fills it on the same thread immediately before. Same contract as
// `Vst3InEventList`.
unsafe impl Send for Vst3ParamValueQueue {}
unsafe impl Sync for Vst3ParamValueQueue {}

impl Vst3ParamValueQueue {
    fn new() -> Self {
        Self {
            param_id: UnsafeCell::new(0),
            points: UnsafeCell::new(Vec::with_capacity(64)),
        }
    }

    fn reset(&self, id: u32) {
        let points = unsafe { &mut *self.points.get() };
        points.clear();
        unsafe { *self.param_id.get() = id };
    }

    fn push(&self, sample_offset: i32, value: f64) {
        let points = unsafe { &mut *self.points.get() };
        points.push((sample_offset, value));
    }

    fn id_value(&self) -> u32 {
        unsafe { *self.param_id.get() }
    }
}

impl Class for Vst3ParamValueQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for Vst3ParamValueQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        *self.param_id.get()
    }

    unsafe fn getPointCount(&self) -> int32 {
        (*self.points.get()).len() as int32
    }

    unsafe fn getPoint(
        &self,
        index: int32,
        sample_offset: *mut int32,
        value: *mut ParamValue,
    ) -> tresult {
        if sample_offset.is_null() || value.is_null() || index < 0 {
            return kInvalidArgument;
        }
        let points = &*self.points.get();
        let Some(&(so, v)) = points.get(index as usize) else {
            return kInvalidArgument;
        };
        *sample_offset = so;
        *value = v;
        kResultOk
    }

    unsafe fn addPoint(
        &self,
        _sample_offset: int32,
        _value: ParamValue,
        _index: *mut int32,
    ) -> tresult {
        // Host-owned input queue: the plugin must not add points to it.
        kResultFalse
    }
}

/// Host-side `IParameterChanges` handed to the plugin via
/// `ProcessData.inputParameterChanges`. Owns a fixed pool of
/// `Vst3ParamValueQueue` (one per automated param this buffer). The audio
/// thread refills it via `set_changes` before each `process()`; the plugin
/// reads it during `process()`.
pub struct Vst3InParamChanges {
    queues: Vec<ComWrapper<Vst3ParamValueQueue>>,
    /// Number of queues active this buffer (`<= queues.len()`).
    used: UnsafeCell<usize>,
}

unsafe impl Send for Vst3InParamChanges {}
unsafe impl Sync for Vst3InParamChanges {}

impl Vst3InParamChanges {
    pub fn new() -> Self {
        let mut queues = Vec::with_capacity(MAX_PARAM_QUEUES);
        for _ in 0..MAX_PARAM_QUEUES {
            queues.push(ComWrapper::new(Vst3ParamValueQueue::new()));
        }
        Self {
            queues,
            used: UnsafeCell::new(0),
        }
    }

    /// Group `events` by `param_id` into the pre-allocated queues. Steady
    /// state allocates nothing (queue `Vec`s retain capacity). Distinct
    /// params beyond the pool size are dropped; returns `true` when that
    /// happened so the caller can warn.
    pub fn set_changes(&self, events: &[TimedParamEvent]) -> bool {
        let mut n_used: usize = 0;
        let mut overflowed = false;
        for ev in events {
            // Reuse the queue already assigned to this param_id this buffer.
            let mut qi = None;
            for i in 0..n_used {
                if self.queues[i].id_value() == ev.param_id {
                    qi = Some(i);
                    break;
                }
            }
            let idx = match qi {
                Some(i) => i,
                None => {
                    if n_used >= self.queues.len() {
                        overflowed = true;
                        continue;
                    }
                    let i = n_used;
                    self.queues[i].reset(ev.param_id);
                    n_used += 1;
                    i
                }
            };
            let so = ev.time.min(i32::MAX as u32) as i32;
            self.queues[idx].push(so, ev.value);
        }
        unsafe { *self.used.get() = n_used };
        overflowed
    }
}

impl Class for Vst3InParamChanges {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for Vst3InParamChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        *self.used.get() as int32
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        let used = *self.used.get();
        if index < 0 || index as usize >= used {
            return std::ptr::null_mut();
        }
        // Borrowed pointer per VST3 spec: the queue is owned by this changes
        // object for the duration of `process()`. The temporary `ComPtr`
        // releases its addRef on drop, but the `ComWrapper` keeps its own ref
        // so the object stays alive (same idiom as `Vst3InEventList` →
        // `inputEvents` in `vst3_plugin.rs::process`).
        match self.queues[index as usize].to_com_ptr::<IParamValueQueue>() {
            Some(p) => p.as_ptr(),
            None => std::ptr::null_mut(),
        }
    }

    unsafe fn addParameterData(
        &self,
        _id: *const ParamID,
        _index: *mut int32,
    ) -> *mut IParamValueQueue {
        // Host input is read-only from the plugin's perspective.
        std::ptr::null_mut()
    }
}

// --- GUI parameter-edit bridge (r.md #4) -----------------------------------

/// Capacity of the GUI→audio parameter-edit ring. A human turning a knob
/// emits at most ~120 `performEdit`s/sec and `process()` drains every audio
/// buffer (~10 ms), so a handful of slots would do; 512 is far beyond any
/// realistic burst between two `process()` calls.
const GUI_PARAM_EDIT_CAP: usize = 512;

/// Lock-free SPSC ring buffer carrying VST3 GUI parameter edits — the edit
/// controller's `IComponentHandler::performEdit(param_id, normalized_value)` —
/// from the controller's UI thread to the audio `process()` thread.
///
/// VST3 splits a plugin into an **edit controller** (the GUI) and an **audio
/// processor** (the DSP) that do not talk to each other; carrying a parameter
/// edit from the controller to the processor (via the next `process()`'s
/// `inputParameterChanges`) is the *host's* job. Without this bridge, turning a
/// knob updates the GUI but never reaches the DSP, so the sound does not change
/// for any parameter that isn't already driven by an automation lane — exactly
/// the r.md #4 symptom (CLAP plugins are unaffected: their GUI and DSP are one
/// instance, so the edit is internal). Automation lane values arrive on a
/// separate path (`Vst3InParamChanges::set_changes`); this queue is purely the
/// GUI→DSP bridge.
///
/// Single producer (the controller calls `performEdit` on one UI thread),
/// single consumer-at-a-time (the worker pool serializes `process()` per
/// plugin). `push` only writes `head` + slots; `drain_latest` only writes
/// `tail`; the Acquire/Release pair orders the slot writes against the reads.
/// On overflow the incoming edit is dropped and a flag is raised (logged
/// off-RT) — benign because the consumer drains far faster than a human edits,
/// so during playback the ring never approaches full and the final value of a
/// drag is always delivered.
pub struct GuiParamEditQueue {
    buf: [UnsafeCell<(u32, f64)>; GUI_PARAM_EDIT_CAP],
    /// Next slot the producer will write (only the producer stores this).
    head: AtomicUsize,
    /// Next slot the consumer will read (only the consumer stores this).
    tail: AtomicUsize,
    /// Raised by `push` when the ring was full; surfaced off-RT by
    /// `take_overflowed` (a best-effort diagnostic, `Relaxed` suffices).
    overflowed: AtomicBool,
}

// SAFETY: shared across the producer (UI thread) and consumer (audio thread)
// only through the atomic head/tail; the `UnsafeCell` slots are written by the
// producer and read by the consumer with Acquire/Release establishing the
// happens-before, and the two never touch the same slot concurrently.
unsafe impl Send for GuiParamEditQueue {}
unsafe impl Sync for GuiParamEditQueue {}

impl GuiParamEditQueue {
    pub fn new() -> Self {
        Self {
            buf: std::array::from_fn(|_| UnsafeCell::new((0, 0.0))),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            overflowed: AtomicBool::new(false),
        }
    }

    /// Producer: enqueue one `(param_id, normalized_value)` edit. Called on the
    /// controller's UI thread. Drops (and flags) the edit if the ring is full.
    pub fn push(&self, param_id: u32, value: f64) {
        let head = self.head.load(Ordering::Relaxed);
        let next = (head + 1) % GUI_PARAM_EDIT_CAP;
        if next == self.tail.load(Ordering::Acquire) {
            self.overflowed.store(true, Ordering::Relaxed);
            return;
        }
        unsafe { *self.buf[head].get() = (param_id, value) };
        self.head.store(next, Ordering::Release);
    }

    /// Consumer: drain all pending edits into `out`, collapsing to the LAST
    /// value per `param_id` (only the final knob position matters; the
    /// intermediate values of a drag are redundant). `out` is the caller's
    /// pre-allocated scratch and must be cleared before the call; draining
    /// never allocates as long as `out` has capacity for the distinct params
    /// edited this buffer (realistically one). Called on the audio thread.
    pub fn drain_latest(&self, out: &mut Vec<(u32, f64)>) {
        let mut tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        while tail != head {
            let (id, val) = unsafe { *self.buf[tail].get() };
            if let Some(slot) = out.iter_mut().find(|(eid, _)| *eid == id) {
                slot.1 = val;
            } else if out.len() < out.capacity() {
                out.push((id, val));
            }
            tail = (tail + 1) % GUI_PARAM_EDIT_CAP;
        }
        self.tail.store(tail, Ordering::Release);
    }

    /// Take-and-clear the overflow flag for an off-RT diagnostic log.
    pub fn take_overflowed(&self) -> bool {
        self.overflowed.swap(false, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(time: u32, param_id: u32, value: f64) -> TimedParamEvent {
        TimedParamEvent {
            time,
            param_id,
            value,
            kind: crate::plugin_instance::ParamEventKind::Value,
        }
    }

    #[test]
    fn set_changes_groups_by_param_id_in_order() {
        let changes = Vst3InParamChanges::new();
        // 2 distinct params, param 10 appears twice (ascending offsets).
        let overflow = changes.set_changes(&[
            ev(0, 10, 0.5),
            ev(5, 20, 0.3),
            ev(10, 10, 0.7),
        ]);
        assert!(!overflow);
        assert_eq!(unsafe { *changes.used.get() }, 2);

        // queue 0 → param 10 with 2 points in push order.
        assert_eq!(changes.queues[0].id_value(), 10);
        let p0 = unsafe { &*changes.queues[0].points.get() };
        assert_eq!(p0.as_slice(), &[(0, 0.5), (10, 0.7)]);

        // queue 1 → param 20 with 1 point.
        assert_eq!(changes.queues[1].id_value(), 20);
        let p1 = unsafe { &*changes.queues[1].points.get() };
        assert_eq!(p1.as_slice(), &[(5, 0.3)]);

        assert_eq!(unsafe { changes.getParameterCount() }, 2);
    }

    #[test]
    fn set_changes_reuses_pool_and_clears_between_calls() {
        let changes = Vst3InParamChanges::new();
        changes.set_changes(&[ev(0, 1, 0.1), ev(1, 2, 0.2), ev(2, 3, 0.3)]);
        assert_eq!(unsafe { *changes.used.get() }, 3);
        // 2nd call with fewer params must reset used + reuse queue 0 cleanly.
        let overflow = changes.set_changes(&[ev(0, 7, 0.9)]);
        assert!(!overflow);
        assert_eq!(unsafe { *changes.used.get() }, 1);
        assert_eq!(changes.queues[0].id_value(), 7);
        let p = unsafe { &*changes.queues[0].points.get() };
        assert_eq!(p.as_slice(), &[(0, 0.9)]);
    }

    #[test]
    fn set_changes_overflow_drops_excess_params() {
        let changes = Vst3InParamChanges::new();
        // MAX_PARAM_QUEUES + 5 distinct params → overflow true, used capped.
        let events: Vec<TimedParamEvent> = (0..(MAX_PARAM_QUEUES as u32 + 5))
            .map(|i| ev(0, i, 0.0))
            .collect();
        let overflow = changes.set_changes(&events);
        assert!(overflow);
        assert_eq!(unsafe { *changes.used.get() }, MAX_PARAM_QUEUES);
    }

    #[test]
    fn gui_param_edits_drain_keeps_last_value_per_param() {
        let q = GuiParamEditQueue::new();
        q.push(10, 0.1);
        q.push(20, 0.9);
        q.push(10, 0.5); // a later edit to param 10 ...
        q.push(10, 0.7); // ... and a later one still — only the last survives.
        let mut out = Vec::with_capacity(8);
        q.drain_latest(&mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out.iter().find(|(id, _)| *id == 10).unwrap().1, 0.7);
        assert_eq!(out.iter().find(|(id, _)| *id == 20).unwrap().1, 0.9);
        // Ring is drained: a second drain into a fresh buffer adds nothing.
        out.clear();
        q.drain_latest(&mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn gui_param_edits_overflow_drops_and_flags() {
        let q = GuiParamEditQueue::new();
        // One slot is always left empty (full vs empty disambiguation), so
        // CAP-1 edits fit without overflow.
        for i in 0..(GUI_PARAM_EDIT_CAP as u32 - 1) {
            q.push(i, 0.0);
        }
        assert!(!q.take_overflowed());
        q.push(9999, 1.0); // ring full → dropped + flagged.
        assert!(q.take_overflowed());
        assert!(!q.take_overflowed()); // flag cleared by take.
    }
}
