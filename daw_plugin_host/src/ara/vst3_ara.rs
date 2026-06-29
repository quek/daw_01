//! Hand-written VST3 companion-API glue for ARA (`ARA_API/ARAVST3.h`).
//!
//! ARA's VST3 binding rides on Steinberg COM (C++ vtables), so unlike the pure-C
//! CLAP path these are real COM interfaces. They are not part of the `vst3`
//! crate's generated bindings, so we declare them here following the exact
//! `com-scrape` interface layout (a `*const Vtbl` newtype + `Unknown` +
//! `Interface` impls) so that `ComPtr::cast::<T>()` can `queryInterface` for them.
//!
//! - [`IPlugInEntryPoint`] (on the audio-effect `IComponent`) exposes the ARA
//!   factory.
//! - [`IPlugInEntryPoint2`] binds the instance to a document controller (ARA 2).
//!
//! Verbatim from `ARAVST3.h` (ARA 2.3.0.001, Apache-2.0). IIDs are built with
//! `vst3::uid` so they match the host platform's TUID byte order.

use core::ffi::c_void;

use ara_sys::{
    ARADocumentControllerRef, ARAFactory, ARAPlugInExtensionInstance, ARAPlugInInstanceRoleFlags,
};
use vst3::ComPtr;
use vst3::Steinberg::{FUnknown, FUnknownVtbl, TUID, Vst::IComponent};
use vst3::com_scrape_types::{Guid, Inherits, Interface, Unknown};
use vst3::uid;

/// VST3 class-category string identifying an `ARA::IMainFactory` class. Its
/// presence in a plug-in's factory means the binary supports ARA (used by the
/// scan to flag ARA-capable VST3 plug-ins without instantiating them).
pub const K_ARA_MAIN_FACTORY_CLASS: &[u8] = b"ARA Main Factory Class";

/// `TUID` (Steinberg `int8[16]`) → `com-scrape` `Guid` (`u8[16]`).
const fn tuid_to_guid(t: TUID) -> Guid {
    [
        t[0] as u8, t[1] as u8, t[2] as u8, t[3] as u8, t[4] as u8, t[5] as u8, t[6] as u8,
        t[7] as u8, t[8] as u8, t[9] as u8, t[10] as u8, t[11] as u8, t[12] as u8, t[13] as u8,
        t[14] as u8, t[15] as u8,
    ]
}

/// Declares a COM interface in the layout `com-scrape` expects (a newtype over
/// `*const Vtbl` plus `Unknown` / `Interface` impls forwarding to the base
/// `FUnknown` vtable). The `Vtbl` type is defined separately.
macro_rules! ara_vst3_interface {
    ($iface:ident, $vtbl:ident, $a:expr, $b:expr, $c:expr, $d:expr) => {
        #[repr(C)]
        #[derive(Copy, Clone)]
        pub struct $iface {
            pub vtbl: *const $vtbl,
        }
        unsafe impl Send for $iface {}
        unsafe impl Sync for $iface {}
        unsafe impl Inherits<FUnknown> for $iface {}
        impl Unknown for $iface {
            unsafe fn query_interface(this: *mut Self, iid: &Guid) -> Option<*mut c_void> {
                let funknown = this.cast::<FUnknown>();
                let mut obj = core::ptr::null_mut();
                // kResultOk == 0 on every platform.
                let result = unsafe {
                    ((*(*funknown).vtbl).queryInterface)(
                        funknown,
                        iid.as_ptr() as *const TUID,
                        &mut obj,
                    )
                };
                if result == 0 { Some(obj) } else { None }
            }
            unsafe fn add_ref(this: *mut Self) -> usize {
                let funknown = this.cast::<FUnknown>();
                unsafe { ((*(*funknown).vtbl).addRef)(funknown) as usize }
            }
            unsafe fn release(this: *mut Self) -> usize {
                let funknown = this.cast::<FUnknown>();
                unsafe { ((*(*funknown).vtbl).release)(funknown) as usize }
            }
        }
        unsafe impl Interface for $iface {
            type Vtbl = $vtbl;
            const IID: Guid = tuid_to_guid(uid($a, $b, $c, $d));
            fn inherits(iid: &Guid) -> bool {
                iid == &Self::IID || FUnknown::inherits(iid)
            }
        }
    };
}

ara_vst3_interface!(
    IPlugInEntryPoint,
    IPlugInEntryPointVtbl,
    0x12814E54,
    0xA1CE4076,
    0x82B96813,
    0x16950BD6
);

ara_vst3_interface!(
    IPlugInEntryPoint2,
    IPlugInEntryPoint2Vtbl,
    0xCD9A5913,
    0xC9EB46D7,
    0x96CA53AD,
    0xD1DB89F5
);

#[repr(C)]
#[derive(Copy, Clone)]
pub struct IPlugInEntryPointVtbl {
    pub base: FUnknownVtbl,
    pub get_factory: unsafe extern "system" fn(this: *mut IPlugInEntryPoint) -> *const ARAFactory,
    /// Deprecated since ARA 2.0 (superseded by `IPlugInEntryPoint2`). Present
    /// only to keep the vtable layout correct; never called.
    pub bind_to_document_controller: unsafe extern "system" fn(
        this: *mut IPlugInEntryPoint,
        document_controller_ref: ARADocumentControllerRef,
    ) -> *const ARAPlugInExtensionInstance,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct IPlugInEntryPoint2Vtbl {
    pub base: FUnknownVtbl,
    pub bind_to_document_controller_with_roles: unsafe extern "system" fn(
        this: *mut IPlugInEntryPoint2,
        document_controller_ref: ARADocumentControllerRef,
        known_roles: ARAPlugInInstanceRoleFlags,
        assigned_roles: ARAPlugInInstanceRoleFlags,
    ) -> *const ARAPlugInExtensionInstance,
}

/// Queries a loaded VST3 component for its ARA factory, or `None` if it is not
/// ARA-capable.
///
/// # Safety
/// `component` must be a valid, initialised VST3 `IComponent`.
pub unsafe fn ara_factory_from_component(
    component: &ComPtr<IComponent>,
) -> Option<*const ARAFactory> {
    let entry = component.cast::<IPlugInEntryPoint>()?;
    tracing::info!("VST3 ARA: IPlugInEntryPoint cast ok; calling getFactory");
    let ptr = entry.as_ptr();
    let factory = unsafe { ((*(*ptr).vtbl).get_factory)(ptr) };
    tracing::info!(factory = ?factory, "VST3 ARA: getFactory returned");
    (!factory.is_null()).then_some(factory)
}

/// Binds a VST3 component instance to an ARA document controller, returning the
/// resulting extension instance. Per ARAVST3.h this must be called once, before
/// `setActive` / `setState` / `getProcessContextRequirements` / GUI creation.
///
/// # Safety
/// `component` must be a valid VST3 `IComponent`; `document_controller` a valid
/// ref obtained from the same plug-in's ARA factory.
pub unsafe fn bind(
    component: &ComPtr<IComponent>,
    document_controller: ARADocumentControllerRef,
    known_roles: ARAPlugInInstanceRoleFlags,
    assigned_roles: ARAPlugInInstanceRoleFlags,
) -> Option<*const ARAPlugInExtensionInstance> {
    let entry = component.cast::<IPlugInEntryPoint2>()?;
    tracing::info!("VST3 ARA: IPlugInEntryPoint2 cast ok; calling bindToDocumentControllerWithRoles");
    let ptr = entry.as_ptr();
    let instance = unsafe {
        ((*(*ptr).vtbl).bind_to_document_controller_with_roles)(
            ptr,
            document_controller,
            known_roles,
            assigned_roles,
        )
    };
    tracing::info!(instance = ?instance, "VST3 ARA: bindToDocumentControllerWithRoles returned");
    (!instance.is_null()).then_some(instance)
}
