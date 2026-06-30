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
use std::sync::Arc;

use com_scrape_types::Class;
use vst3::Steinberg::{
    IPlugFrame, IPlugFrameTrait, IPlugView, TUID, ViewRect, kNotImplemented, kResultOk, tresult,
    Vst::{
        IComponentHandler, IComponentHandlerTrait, IHostApplication, IHostApplicationTrait,
        ParamID, ParamValue, String128,
    },
};

use crate::plugin_instance::HostCallbacks;
use crate::vst3_params::GuiParamEditQueue;

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
    /// Bridges GUI parameter edits to the audio processor (r.md #4). The VST3
    /// edit controller (this handler's caller) and the audio processor are
    /// decoupled, so `performEdit` must hand the value to the processor via the
    /// next `process()`'s `inputParameterChanges` — this queue carries it
    /// across to the audio thread. Shared (`Arc`) with the owning `Vst3Plugin`,
    /// which drains it in `process()`.
    gui_param_edits: Arc<GuiParamEditQueue>,
}

impl Vst3ComponentHandler {
    pub fn new(callbacks: HostCallbacks, gui_param_edits: Arc<GuiParamEditQueue>) -> Self {
        Self {
            callbacks,
            gui_param_edits,
        }
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
        // plugin GUI 内での param 値変更。 VST3 の値は常に normalized [0,1]。
        // (1) daw_gui に転送して plugin_param_values cache を更新し、
        //     automation lane の現在値 source / last-touched に使う。
        (self.callbacks.on_param_value)(id, value_normalized);
        // (2) audio processor へ橋渡しする (r.md #4)。 VST3 は edit controller
        //     (この GUI) と audio processor (DSP) が分離していて互いに通信し
        //     ないので、 host がこの編集を次の process() の
        //     inputParameterChanges に載せない限り音は変わらない。 この push が
        //     audio thread (Vst3Plugin::process の drain) へ値を渡す。
        self.gui_param_edits.push(id, value_normalized);
        kResultOk
    }

    unsafe fn endEdit(&self, id: ParamID) -> tresult {
        (self.callbacks.on_param_gesture_end)(id);
        kResultOk
    }

    unsafe fn restartComponent(&self, flags: i32) -> tresult {
        // C3 (r.md #8): plugin が bus / parameter / latency topology の変更を通知。
        // raw int だけ無視していたのを named bit に分解して診断ログ化する (どの
        // topology が変わったか分かる)。 deactivate→activate / latency 再 query /
        // param 再列挙の実 reaction は既存の `ReinitAllPlugins` 経路 (host→audio
        // thread の安全な reinit coordination + re-emit) で行う follow-up。 現状の
        // clean 再 activate は user の plugin toggle (= ReinitAllPlugins) で得られる。
        use vst3::Steinberg::Vst::RestartFlags_;
        let f = flags as u32;
        let mut kinds: Vec<&str> = Vec::new();
        for (bit, name) in [
            (RestartFlags_::kReloadComponent as u32, "ReloadComponent"),
            (RestartFlags_::kIoChanged as u32, "IoChanged"),
            (RestartFlags_::kParamValuesChanged as u32, "ParamValuesChanged"),
            (RestartFlags_::kLatencyChanged as u32, "LatencyChanged"),
            (RestartFlags_::kParamTitlesChanged as u32, "ParamTitlesChanged"),
            (RestartFlags_::kIoTitlesChanged as u32, "IoTitlesChanged"),
            (RestartFlags_::kRoutingInfoChanged as u32, "RoutingInfoChanged"),
        ] {
            if bit != 0 && f & bit != 0 {
                kinds.push(name);
            }
        }
        tracing::info!(flags, ?kinds, "VST3 plugin requested restartComponent");
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
