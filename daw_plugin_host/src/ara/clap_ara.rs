//! Hand-written CLAP companion-API glue for ARA (`ARA_API/ARACLAP.h`).
//!
//! `ARACLAP.h` `#include`s both `clap/clap.h` and `ARAInterface.h`, so it is not
//! bound by `ara-sys` (which binds the pure-C ARA core only — no CLAP headers
//! are vendored). The two companion structs are tiny, so they are transcribed
//! here against `clap-sys` + `ara-sys` types. Verbatim from `ARACLAP.h`
//! (ARA 2.3.0.001, Apache-2.0).

use core::ffi::{CStr, c_char};

use ara_sys::{
    ARADocumentControllerRef, ARAFactory, ARAPlugInExtensionInstance, ARAPlugInInstanceRoleFlags,
};
use clap_sys::entry::clap_plugin_entry;
use clap_sys::plugin::clap_plugin;

/// `CLAP_EXT_ARA_FACTORY` — id passed to `clap_plugin_entry.get_factory` to
/// obtain the [`clap_ara_factory`].
pub const CLAP_EXT_ARA_FACTORY: &CStr = c"org.ara-audio.ara.factory/2";

/// `CLAP_EXT_ARA_PLUGINEXTENSION` — id passed to `clap_plugin.get_extension` to
/// obtain the per-instance [`clap_ara_plugin_extension`].
pub const CLAP_EXT_ARA_PLUGINEXTENSION: &CStr = c"org.ara-audio.ara.pluginextension/2";

/// `clap_ara_factory_t` (`ARACLAP.h`). Entry-level factory that maps CLAP
/// plugin ids to their [`ARAFactory`], letting a host discover ARA support and
/// drive the model without instantiating the audio plug-in.
#[repr(C)]
pub struct clap_ara_factory {
    pub get_factory_count: Option<unsafe extern "C" fn(factory: *const clap_ara_factory) -> u32>,
    pub get_ara_factory: Option<
        unsafe extern "C" fn(factory: *const clap_ara_factory, index: u32) -> *const ARAFactory,
    >,
    pub get_plugin_id: Option<
        unsafe extern "C" fn(factory: *const clap_ara_factory, index: u32) -> *const c_char,
    >,
}

/// `clap_ara_plugin_extension_t` (`ARACLAP.h`). Per-instance extension obtained
/// via `clap_plugin.get_extension`; `bind_to_document_controller` attaches the
/// instance to a host-driven ARA document controller (VST3's
/// `bindToDocumentControllerWithRoles` analogue). Must be called once, before
/// `activate` / state load / GUI creation.
#[repr(C)]
pub struct clap_ara_plugin_extension {
    pub get_factory: Option<unsafe extern "C" fn(plugin: *const clap_plugin) -> *const ARAFactory>,
    pub bind_to_document_controller: Option<
        unsafe extern "C" fn(
            plugin: *const clap_plugin,
            document_controller_ref: ARADocumentControllerRef,
            known_roles: ARAPlugInInstanceRoleFlags,
            assigned_roles: ARAPlugInInstanceRoleFlags,
        ) -> *const ARAPlugInExtensionInstance,
    >,
}

/// Queries a loaded CLAP entry for its [`clap_ara_factory`]. Returns `None` when
/// the plug-in exposes no ARA factory (= not ARA-capable via CLAP).
///
/// # Safety
/// `entry` must be a valid `clap_plugin_entry` whose `init` has already
/// succeeded (the factory query is only valid on an initialised entry).
pub unsafe fn ara_factory_from_entry(entry: &clap_plugin_entry) -> Option<*const clap_ara_factory> {
    let get_factory = entry.get_factory?;
    let ptr = unsafe { get_factory(CLAP_EXT_ARA_FACTORY.as_ptr()) }.cast::<clap_ara_factory>();
    (!ptr.is_null()).then_some(ptr)
}

/// Queries a CLAP plug-in instance for its [`clap_ara_plugin_extension`], or
/// `None` if the instance exposes none.
///
/// # Safety
/// `plugin` must be a valid `clap_plugin` (created and not yet destroyed).
pub unsafe fn ara_plugin_extension(
    plugin: *const clap_plugin,
) -> Option<*const clap_ara_plugin_extension> {
    let instance = unsafe { plugin.as_ref() }?;
    let get_extension = instance.get_extension?;
    let ptr = unsafe { get_extension(plugin, CLAP_EXT_ARA_PLUGINEXTENSION.as_ptr()) }
        .cast::<clap_ara_plugin_extension>();
    (!ptr.is_null()).then_some(ptr)
}

/// Binds a CLAP plug-in instance to an ARA document controller, returning the
/// resulting `ARAPlugInExtensionInstance`. Per ARACLAP.h this must be called
/// exactly once, before `activate` / state load / GUI creation.
///
/// # Safety
/// `plugin` must be a valid `clap_plugin`; `document_controller` a valid ref
/// obtained from the same plug-in's ARA factory.
pub unsafe fn bind_to_document(
    plugin: *const clap_plugin,
    document_controller: ARADocumentControllerRef,
    known_roles: ARAPlugInInstanceRoleFlags,
    assigned_roles: ARAPlugInInstanceRoleFlags,
) -> Option<*const ARAPlugInExtensionInstance> {
    let extension = unsafe { ara_plugin_extension(plugin)?.as_ref() }?;
    let bind = extension.bind_to_document_controller?;
    let instance = unsafe { bind(plugin, document_controller, known_roles, assigned_roles) };
    (!instance.is_null()).then_some(instance)
}

/// Resolves the [`ARAFactory`] for the descriptor `plugin_id` within a loaded
/// CLAP entry's ARA factory (a `.clap` may host several descriptors). Returns
/// `None` if the entry exposes no ARA factory or none matches `plugin_id`.
///
/// # Safety
/// `entry` must be a valid, initialised `clap_plugin_entry`.
pub unsafe fn ara_factory_for_plugin(
    entry: &clap_plugin_entry,
    plugin_id: &CStr,
) -> Option<*const ARAFactory> {
    let factory_ptr = unsafe { ara_factory_from_entry(entry) }?;
    let factory = unsafe { factory_ptr.as_ref() }?;
    let count = unsafe { (factory.get_factory_count?)(factory_ptr) };
    let get_plugin_id = factory.get_plugin_id?;
    let get_ara_factory = factory.get_ara_factory?;
    for index in 0..count {
        let id_ptr = unsafe { get_plugin_id(factory_ptr, index) };
        if id_ptr.is_null() {
            continue;
        }
        if unsafe { CStr::from_ptr(id_ptr) } == plugin_id {
            let ara_factory = unsafe { get_ara_factory(factory_ptr, index) };
            return (!ara_factory.is_null()).then_some(ara_factory);
        }
    }
    None
}
