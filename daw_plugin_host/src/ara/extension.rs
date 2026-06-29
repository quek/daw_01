//! Format-agnostic wrapper over an `ARAPlugInExtensionInstance` — the per-
//! instance binding returned when a companion (CLAP/VST3) plug-in instance is
//! attached to an ARA document controller.
//!
//! It exposes the playback-renderer role used to assign playback regions for
//! rendering during playback / export. The editor-view and editor-renderer
//! roles (preview audio, plug-in GUI) are wired in later steps.

use core::ptr;

use ara_sys::{
    ARAPlaybackRegionRef, ARAPlugInExtensionInstance, ARAPlugInInstanceRoleFlags,
    kARAEditorRendererRole, kARAEditorViewRole, kARAPlaybackRendererRole,
};

/// Roles this host can drive for an ARA instance: render song playback, render
/// editor preview, and host the editor view. We advertise and assign all three
/// so a single instance both renders ARA-edited audio and can show the plug-in's
/// editor.
pub const HOST_KNOWN_ROLES: ARAPlugInInstanceRoleFlags =
    kARAPlaybackRendererRole.0 | kARAEditorRendererRole.0 | kARAEditorViewRole.0;
pub const HOST_ASSIGNED_ROLES: ARAPlugInInstanceRoleFlags = HOST_KNOWN_ROLES;

/// A plug-in instance bound to an ARA document controller. Holds a copy of the
/// returned extension instance; the role refs / interface tables inside stay
/// valid until the companion plug-in instance is destroyed by the host.
pub struct AraPlugInExtension {
    instance: ARAPlugInExtensionInstance,
}

impl AraPlugInExtension {
    /// Copy out the extension instance returned by `bind_to_document_controller`.
    ///
    /// # Safety
    /// `instance` must be a valid `ARAPlugInExtensionInstance` for a still-living
    /// plug-in instance bound to a document controller.
    pub unsafe fn from_instance_ptr(instance: *const ARAPlugInExtensionInstance) -> Option<Self> {
        if instance.is_null() {
            return None;
        }
        Some(Self {
            instance: unsafe { ptr::read_unaligned(instance) },
        })
    }

    /// Whether the playback-renderer role was assigned to this instance.
    pub fn has_playback_renderer(&self) -> bool {
        !self.instance.playbackRendererInterface.is_null()
    }

    /// Assign a playback region to this instance's playback renderer so the
    /// region's ARA-edited audio is produced during `process()`. Per ARA, this
    /// must be called while the plug-in instance is inactive and from the
    /// document (model) thread.
    pub fn add_playback_region(&self, region: ARAPlaybackRegionRef) {
        let interface = self.instance.playbackRendererInterface;
        let renderer = self.instance.playbackRendererRef;
        if interface.is_null() {
            return;
        }
        let table = unsafe { ptr::read_unaligned(interface) };
        if let Some(add) = table.addPlaybackRegion {
            unsafe { add(renderer, region) };
        }
    }

    /// Remove a previously-added playback region (same threading rules as
    /// [`Self::add_playback_region`]).
    pub fn remove_playback_region(&self, region: ARAPlaybackRegionRef) {
        let interface = self.instance.playbackRendererInterface;
        let renderer = self.instance.playbackRendererRef;
        if interface.is_null() {
            return;
        }
        let table = unsafe { ptr::read_unaligned(interface) };
        if let Some(remove) = table.removePlaybackRegion {
            unsafe { remove(renderer, region) };
        }
    }
}
