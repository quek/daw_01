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

/// Reusable input event list: the host-side `Vst3Plugin` owns a single
/// `ComWrapper<Vst3InEventList>` for its lifetime and calls `set_events`
/// before every `process()` to refill the backing buffer. Using
/// `UnsafeCell` (rather than `RefCell` or a lock) avoids runtime cost in
/// the audio thread.
pub struct Vst3InEventList {
    events: UnsafeCell<Vec<Event>>,
}

// SAFETY: only the audio thread touches this during `process()`; the
// plugin-main thread only calls `set_events` before starting the audio
// thread (via the audio-thread-stop/start dance in `TracksHandle::mutate`).
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

pub struct Vst3OutEventList {
    events: UnsafeCell<Vec<Event>>,
}

unsafe impl Send for Vst3OutEventList {}
unsafe impl Sync for Vst3OutEventList {}

impl Vst3OutEventList {
    pub fn new() -> Self {
        Self {
            events: UnsafeCell::new(Vec::with_capacity(64)),
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
        events.push(*ev);
        kResultOk
    }
}
