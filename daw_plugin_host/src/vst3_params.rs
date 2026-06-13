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
}
