// Rust 2024 wants every UB operation inside an unsafe fn to be in an
// explicit `unsafe { }` block. These files cross FFI boundaries on every
// line; wrapping the entire fn body is the pragmatic choice.
#![allow(unsafe_op_in_unsafe_fn)]

//! Host-side VST3 COM objects exposed to plugins.
//!
//! - `Vst3HostApp` — the `IHostApplication` handed to `IComponent::initialize`
//!   / `IEditController::initialize`.
//! - `Vst3ComponentHandler` — receives `beginEdit` / `performEdit` / `endEdit`
//!   / `restartComponent` from the controller. MVP: log-and-return-OK.
//! - `Vst3PlugFrame` — receives `resizeView` from the editor's `IPlugView`
//!   so we can route plugin-initiated resize requests back to daw_gui.

use std::ffi::c_void;

use com_scrape_types::Class;
use vst3::Steinberg::{
    IPlugFrame, IPlugFrameTrait, IPlugView, TUID, ViewRect, kNotImplemented, kResultOk, tresult,
    Vst::{
        IComponentHandler, IComponentHandlerTrait, IHostApplication, IHostApplicationTrait,
        ParamID, ParamValue, String128,
    },
};

use crate::plugin_instance::HostCallbacks;

// --- IHostApplication ------------------------------------------------------

pub struct Vst3HostApp {}

impl Vst3HostApp {
    pub fn new() -> Self {
        Self {}
    }
}

impl Class for Vst3HostApp {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for Vst3HostApp {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        if name.is_null() {
            return kResultOk;
        }
        let dst = &mut *name;
        // String128 is [TChar; 128] where TChar = char16 = u16. Write our
        // host name as a null-terminated UTF-16 string.
        const HOST_NAME: &str = "daw_01";
        let mut i = 0;
        for u in HOST_NAME.encode_utf16() {
            if i + 1 >= dst.len() {
                break;
            }
            dst[i] = u;
            i += 1;
        }
        if i < dst.len() {
            dst[i] = 0;
        }
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        _cid: *mut TUID,
        _iid: *mut TUID,
        _obj: *mut *mut c_void,
    ) -> tresult {
        // MVP: we do not provide `IMessage` / `IAttributeList`. Plugins that
        // strictly need them (e.g. inter-component messaging) will get
        // kNotImplemented; most simple instruments / effects are fine.
        kNotImplemented
    }
}

// --- IComponentHandler -----------------------------------------------------

pub struct Vst3ComponentHandler {
    callbacks: HostCallbacks,
}

impl Vst3ComponentHandler {
    pub fn new(callbacks: HostCallbacks) -> Self {
        Self { callbacks }
    }
}

impl Class for Vst3ComponentHandler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for Vst3ComponentHandler {
    unsafe fn beginEdit(&self, id: ParamID) -> tresult {
        // plugin GUI で knob を触り始めた。 daw_gui の last-touched param を
        // 更新する (= `A` キーで automation lane を作る起点)。 callback は
        // main/GUI thread から呼ばれ、 evt_tx (channel) 送信のみなので
        // audio thread とは無関係 (ロックなし)。
        (self.callbacks.on_param_gesture_begin)(id);
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, value_normalized: ParamValue) -> tresult {
        // plugin GUI 内での param 値変更。 daw_gui の plugin_param_values
        // cache を更新し、 automation lane の現在値 source にする。 VST3 の
        // 値は常に normalized [0,1]。
        (self.callbacks.on_param_value)(id, value_normalized);
        kResultOk
    }

    unsafe fn endEdit(&self, id: ParamID) -> tresult {
        (self.callbacks.on_param_gesture_end)(id);
        kResultOk
    }

    unsafe fn restartComponent(&self, flags: i32) -> tresult {
        // Plugins call this when their bus/parameter/latency topology has
        // changed. MVP: accept but do not act (the user can toggle the
        // plugin to get a clean re-activate).
        tracing::info!(flags, "VST3 plugin requested restartComponent");
        kResultOk
    }
}

// --- IPlugFrame ------------------------------------------------------------

pub struct Vst3PlugFrame {
    callbacks: HostCallbacks,
}

impl Vst3PlugFrame {
    pub fn new(callbacks: HostCallbacks) -> Self {
        Self { callbacks }
    }
}

impl Class for Vst3PlugFrame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for Vst3PlugFrame {
    unsafe fn resizeView(&self, _view: *mut IPlugView, new_size: *mut ViewRect) -> tresult {
        if new_size.is_null() {
            return kResultOk;
        }
        let rect = *new_size;
        let w = (rect.right - rect.left).max(0) as u32;
        let h = (rect.bottom - rect.top).max(0) as u32;
        (self.callbacks.on_request_resize)(w, h);
        // daw_gui will reply with a `ResizeSlotGui` that ends up calling
        // `IPlugView::onSize` on this view. Returning kResultOk tells the
        // plugin the host accepted the hint.
        kResultOk
    }
}
