#![allow(unsafe_op_in_unsafe_fn)]

//! `IEventList` wrappers for feeding notes into the plugin and collecting
//! whatever it emits.
//!
//! The audio thread drives these on every `process()`; both sides use
//! `UnsafeCell` to keep the API simple while avoiding lock overhead. The
//! audio thread is single-threaded so there is no actual sharing.

use std::cell::UnsafeCell;

use com_scrape_types::Class;
use vst3::Steinberg::{
    kInvalidArgument, kResultOk, tresult,
    Vst::{Event, IEventList, IEventListTrait},
};

use crate::plugin_instance::TimedNoteEvent;
use crate::vst3_plugin::decode_event;

// --- Input list (host -> plugin) -------------------------------------------

/// Reusable input event list: the `Vst3AudioHalf` owns a single
/// `ComWrapper<Vst3InEventList>` for its lifetime and calls `set_events`
/// at the top of every `process()` to refill the backing buffer. Using
/// `UnsafeCell` (rather than `RefCell` or a lock) avoids runtime cost in
/// the audio thread.
pub struct Vst3InEventList {
    events: UnsafeCell<Vec<Event>>,
}

// SAFETY: `set_events` (refill) and the plugin's vtable reads both happen
// inside the same audio-half `process()` call — a single thread at a time
// under the `AudioHalf` exclusive-access contract (quiesce protocol), so
// there is no actual sharing.
unsafe impl Send for Vst3InEventList {}
unsafe impl Sync for Vst3InEventList {}

impl Vst3InEventList {
    pub fn new() -> Self {
        Self {
            events: UnsafeCell::new(Vec::with_capacity(256)),
        }
    }

    /// Copies `src` into the internal buffer, replacing its previous
    /// contents. Capacity is retained across calls so this allocates only
    /// when `src` exceeds the high-water mark.
    pub fn set_events(&self, src: &[Event]) {
        let events = unsafe { &mut *self.events.get() };
        events.clear();
        events.extend_from_slice(src);
    }
}

impl Class for Vst3InEventList {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for Vst3InEventList {
    unsafe fn getEventCount(&self) -> i32 {
        let events = &*self.events.get();
        events.len() as i32
    }

    unsafe fn getEvent(&self, index: i32, e: *mut Event) -> tresult {
        if e.is_null() || index < 0 {
            return kInvalidArgument;
        }
        let events = &*self.events.get();
        let Some(src) = events.get(index as usize) else {
            return kInvalidArgument;
        };
        *e = *src;
        kResultOk
    }

    unsafe fn addEvent(&self, _e: *mut Event) -> tresult {
        // Input list is read-only from the plugin's perspective.
        kInvalidArgument
    }
}

// --- Output list (plugin -> host) ------------------------------------------

/// Upper bound on events the plugin may emit in a single `process()`. The
/// plugin calls `addEvent` from the audio thread, so the backing `Vec` must
/// never reallocate: we pre-reserve `OUT_EVENT_CAP` and silently drop any
/// overflow rather than grow on the RT thread.
const OUT_EVENT_CAP: usize = 4096;

pub struct Vst3OutEventList {
    events: UnsafeCell<Vec<Event>>,
}

unsafe impl Send for Vst3OutEventList {}
unsafe impl Sync for Vst3OutEventList {}

impl Vst3OutEventList {
    pub fn new() -> Self {
        Self {
            events: UnsafeCell::new(Vec::with_capacity(OUT_EVENT_CAP)),
        }
    }

    /// Moves every collected event into `out`, decoding to
    /// `TimedNoteEvent`. Unknown VST3 event types are silently dropped.
    pub fn drain_into(&self, out: &mut Vec<TimedNoteEvent>) {
        let events = unsafe { &mut *self.events.get() };
        for e in events.drain(..) {
            if let Some(te) = decode_event(&e) {
                out.push(te);
            }
        }
    }
}

impl Class for Vst3OutEventList {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for Vst3OutEventList {
    unsafe fn getEventCount(&self) -> i32 {
        let events = &*self.events.get();
        events.len() as i32
    }

    unsafe fn getEvent(&self, index: i32, e: *mut Event) -> tresult {
        if e.is_null() || index < 0 {
            return kInvalidArgument;
        }
        let events = &*self.events.get();
        let Some(src) = events.get(index as usize) else {
            return kInvalidArgument;
        };
        *e = *src;
        kResultOk
    }

    unsafe fn addEvent(&self, ev: *mut Event) -> tresult {
        if ev.is_null() {
            return kInvalidArgument;
        }
        let events = &mut *self.events.get();
        // Called on the audio thread: never reallocate. Drop overflow past
        // the pre-reserved bound instead of growing the backing buffer.
        if events.len() >= OUT_EVENT_CAP {
            return kResultOk;
        }
        events.push(*ev);
        kResultOk
    }
}
