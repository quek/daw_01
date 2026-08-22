// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

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

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};
use std::sync::{Arc, Mutex};

use com_scrape_types::Class;
use vst3::Steinberg::{
    FIDString, IPlugFrame, IPlugFrameTrait, IPlugView, TUID, ViewRect, kInvalidArgument,
    kNotImplemented, kResultFalse, kResultOk, tresult,
    Vst::{
        IAttributeList, IAttributeListTrait, IComponentHandler, IComponentHandlerTrait,
        IHostApplication, IHostApplicationTrait, IMessage, IMessageTrait, ParamID, ParamValue,
        String128, TChar,
    },
};
use vst3::Steinberg::Vst::IAttributeList_::AttrID;
use vst3::{ComWrapper, Interface};

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
        cid: *mut TUID,
        _iid: *mut TUID,
        obj: *mut *mut c_void,
    ) -> tresult {
        // C2 (r.md #8): processor↔controller messaging を使う VST3 のため host が
        // IMessage / IAttributeList を生成する。 plugin は
        // createInstance(IMessage::IID, IMessage::IID, &obj) で IMessage を要求し、
        // IAttributeList はその IMessage が getAttributes で内包する。 旧実装は
        // kNotImplemented で messaging plugin が状態同期不能だった。 返す pointer は
        // refcount を caller へ transfer (into_raw)。
        if cid.is_null() || obj.is_null() {
            return kInvalidArgument;
        }
        // TUID ([i8;16]) と Interface::IID ([u8;16]) は同じ 16 byte を表すので byte 比較。
        let cid_bytes: [u8; 16] = std::mem::transmute(*cid);
        if cid_bytes == IMessage::IID
            && let Some(ptr) = ComWrapper::new(Vst3Message::new()).to_com_ptr::<IMessage>()
        {
            *obj = ptr.into_raw().cast::<c_void>();
            return kResultOk;
        }
        if cid_bytes == IAttributeList::IID
            && let Some(ptr) =
                ComWrapper::new(Vst3AttributeList::default()).to_com_ptr::<IAttributeList>()
        {
            *obj = ptr.into_raw().cast::<c_void>();
            return kResultOk;
        }
        kNotImplemented
    }
}

// --- IMessage / IAttributeList (C2 / r.md #8) ------------------------------
//
// processor↔controller messaging 用に host が提供する COM オブジェクト。 messaging は
// component / controller 間の単一スレッドなので Mutex contention は無いが、 ComWrapper
// の Send/Sync 境界を満たすため interior mutability に Mutex を使う。

/// `IAttributeList` の 1 attribute。
enum AttrValue {
    Int(i64),
    Float(f64),
    /// UTF-16 (TChar)、 null 終端なしで保持。
    Str(Vec<u16>),
    Bin(Vec<u8>),
}

/// `AttrID` (C string) を所有 key 化する。
unsafe fn attr_key(id: AttrID) -> Vec<u8> {
    if id.is_null() {
        Vec::new()
    } else {
        CStr::from_ptr(id).to_bytes().to_vec()
    }
}

#[derive(Default)]
pub struct Vst3AttributeList {
    attrs: Mutex<HashMap<Vec<u8>, AttrValue>>,
}

impl Class for Vst3AttributeList {
    type Interfaces = (IAttributeList,);
}

impl IAttributeListTrait for Vst3AttributeList {
    unsafe fn setInt(&self, id: AttrID, value: i64) -> tresult {
        self.attrs.lock().unwrap().insert(attr_key(id), AttrValue::Int(value));
        kResultOk
    }
    unsafe fn getInt(&self, id: AttrID, value: *mut i64) -> tresult {
        if value.is_null() {
            return kInvalidArgument;
        }
        match self.attrs.lock().unwrap().get(&attr_key(id)) {
            Some(AttrValue::Int(v)) => {
                *value = *v;
                kResultOk
            }
            _ => kResultFalse,
        }
    }
    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        self.attrs.lock().unwrap().insert(attr_key(id), AttrValue::Float(value));
        kResultOk
    }
    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        if value.is_null() {
            return kInvalidArgument;
        }
        match self.attrs.lock().unwrap().get(&attr_key(id)) {
            Some(AttrValue::Float(v)) => {
                *value = *v;
                kResultOk
            }
            _ => kResultFalse,
        }
    }
    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        let mut v = Vec::new();
        if !string.is_null() {
            let mut p = string;
            while *p != 0 {
                v.push(*p);
                p = p.add(1);
            }
        }
        self.attrs.lock().unwrap().insert(attr_key(id), AttrValue::Str(v));
        kResultOk
    }
    unsafe fn getString(&self, id: AttrID, string: *mut TChar, size_in_bytes: u32) -> tresult {
        if string.is_null() {
            return kInvalidArgument;
        }
        let map = self.attrs.lock().unwrap();
        let Some(AttrValue::Str(v)) = map.get(&attr_key(id)) else {
            return kResultFalse;
        };
        // 末尾 null (1 TChar = 2 bytes) すら入らない buffer には書かない
        // (`*string.add(0) = 0` が 0/1-byte buffer で OOB になるのを防ぐ)。
        if (size_in_bytes as usize) < 2 {
            return kInvalidArgument;
        }
        // size_in_bytes / 2 = TChar 容量、 末尾 null 用に 1 残す。
        let cap = (size_in_bytes as usize / 2).saturating_sub(1);
        let n = v.len().min(cap);
        for (i, &ch) in v.iter().take(n).enumerate() {
            *string.add(i) = ch;
        }
        *string.add(n) = 0;
        kResultOk
    }
    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, size_in_bytes: u32) -> tresult {
        let bytes = if data.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(data.cast::<u8>(), size_in_bytes as usize).to_vec()
        };
        self.attrs.lock().unwrap().insert(attr_key(id), AttrValue::Bin(bytes));
        kResultOk
    }
    unsafe fn getBinary(
        &self,
        id: AttrID,
        data: *mut *const c_void,
        size_in_bytes: *mut u32,
    ) -> tresult {
        if data.is_null() || size_in_bytes.is_null() {
            return kInvalidArgument;
        }
        let map = self.attrs.lock().unwrap();
        let Some(AttrValue::Bin(v)) = map.get(&attr_key(id)) else {
            return kResultFalse;
        };
        // messaging は単一スレッドで caller は即座に読むので、 lock 解放後も HashMap
        // が所有する Vec への pointer は次の mutate まで有効。
        *data = v.as_ptr().cast::<c_void>();
        *size_in_bytes = v.len() as u32;
        kResultOk
    }
}

/// `IMessage` — message id + 内包する `IAttributeList`。
pub struct Vst3Message {
    id: Mutex<Vec<u8>>,
    attrs: ComWrapper<Vst3AttributeList>,
}

impl Vst3Message {
    fn new() -> Self {
        Self {
            id: Mutex::new(vec![0]),
            attrs: ComWrapper::new(Vst3AttributeList::default()),
        }
    }
}

impl Class for Vst3Message {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for Vst3Message {
    unsafe fn getMessageID(&self) -> FIDString {
        // 内部 buffer への non-owning pointer (setMessageID まで有効)。
        self.id.lock().unwrap().as_ptr().cast::<c_char>()
    }
    unsafe fn setMessageID(&self, id: FIDString) {
        let bytes = if id.is_null() {
            vec![0]
        } else {
            let mut b = CStr::from_ptr(id).to_bytes().to_vec();
            b.push(0);
            b
        };
        *self.id.lock().unwrap() = bytes;
    }
    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        // VST3 convention: non-owning (message が attr list を所有)。
        match self.attrs.as_com_ref::<IAttributeList>() {
            Some(r) => r.as_ptr(),
            None => std::ptr::null_mut(),
        }
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
        // C3 (r.md #8): re-activate / I/O / latency 変更を plugin-main loop に通知し、
        // 該当 plugin を安全に reinit + latency 再 query させる。
        (self.callbacks.on_restart_component)(flags);
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
