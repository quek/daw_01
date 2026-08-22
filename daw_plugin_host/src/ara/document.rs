// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! ARA document controller lifecycle.
//!
//! Given an [`ARAFactory`] obtained from a plug-in (via the CLAP/VST3 companion
//! binding), [`AraDocumentController`] drives the host side of the ARA model:
//! it starts up ARA with a negotiated API generation, creates the plug-in's
//! document controller (passing our host controllers from
//! [`crate::ara::host_controllers`]), exposes the begin/end editing cycle, and
//! tears everything down on drop. Model-object creation (musical context,
//! audio source/modification, playback region) builds on this in the next step.
//!
//! ARA structs are 1-byte packed, so field references are never taken: the
//! plug-in's `ARADocumentControllerInterface` vtable is copied out once with
//! `read_unaligned`, and individual fn-pointer fields are read by value (`Copy`).

use core::ffi::CStr;
use core::ptr;

use anyhow::{Context, Result};
use ara_sys::{
    ARAAPIGeneration, ARAAssertFunction, ARAAudioModificationHostRef,
    ARAAudioModificationProperties, ARAAudioModificationRef, ARAAudioSourceHostRef,
    ARAAudioSourceProperties, ARAAudioSourceRef, ARABool, ARAChannelCount,
    ARADocumentControllerHostInstance, ARADocumentControllerInterface, ARADocumentControllerRef,
    ARADocumentProperties, ARAFactory,
    ARAInterfaceConfiguration, ARAMusicalContextHostRef, ARAMusicalContextProperties,
    ARAMusicalContextRef, ARAPlaybackRegionHostRef, ARAPlaybackRegionProperties,
    ARAPlaybackRegionRef, ARAPlaybackTransformationFlags, ARARegionSequenceHostRef,
    ARARegionSequenceProperties,
    ARARegionSequenceRef, ARASampleCount, ARASampleRate, ARATimeDuration, ARATimePosition,
    kARAAPIGeneration_2_0_Final, kARAAPIGeneration_2_3_Final, kARAChannelArrangementUndefined,
    kARAContentUpdateEverythingChanged, kARAPlaybackTransformationNoChanges,
    kARAPlaybackTransformationTimestretch,
};

use crate::ara::host_controllers;

/// Lowest / highest ARA API generation this host knows how to drive. The model
/// calls used here are stable across ARA 2.x; we negotiate the highest mutually
/// supported generation with the plug-in.
const HOST_MIN_API_GENERATION: ARAAPIGeneration = kARAAPIGeneration_2_0_Final.0;
const HOST_MAX_API_GENERATION: ARAAPIGeneration = kARAAPIGeneration_2_3_Final.0;

/// Host-driven, plug-in-implemented ARA document controller plus the factory it
/// came from. Owns the ARA initialize/uninitialize refcount for `factory` and
/// the controller's lifetime (both released on drop).
pub struct AraDocumentController {
    factory: *const ARAFactory,
    controller_ref: ARADocumentControllerRef,
    /// Copy of the plug-in's document controller vtable (fn pointers stay valid
    /// while the plug-in is loaded).
    interface: ARADocumentControllerInterface,
    /// The host controller table handed to `createDocumentControllerWithDocument`.
    /// ARA requires it (and everything it points to) to stay valid until the
    /// document controller is destroyed — the plug-in keeps the pointer, not a
    /// copy — so it is boxed and owned here, dropped only after
    /// `destroyDocumentController` runs (Drop body precedes field drops).
    _host_instance: Box<ARADocumentControllerHostInstance>,
}

impl AraDocumentController {
    /// Initialise ARA on `factory` and create its document controller, wiring in
    /// our host controllers. `document_name` is shown by some plug-ins; `None`
    /// lets the plug-in pick.
    ///
    /// # Safety
    /// `factory` must be a valid `ARAFactory` obtained from a loaded plug-in and
    /// must outlive the returned controller.
    pub unsafe fn create(
        factory: *const ARAFactory,
        document_name: Option<&CStr>,
    ) -> Result<Self> {
        let fac = unsafe { factory.as_ref() }.context("ARAFactory pointer is null")?;
        let api_generation = choose_api_generation(fac)?;

        // The assert function indirection must outlive uninitializeARA(); a
        // 'static satisfies that. Providing a real callback (rather than NULL)
        // means an ARA contract violation surfaces as a logged diagnosis instead
        // of the plug-in dereferencing a null assert handler and crashing.
        static ARA_ASSERT: ARAAssertFunction = Some(ara_assert_callback);
        let config = ARAInterfaceConfiguration {
            structSize: core::mem::size_of::<ARAInterfaceConfiguration>(),
            desiredApiGeneration: api_generation,
            assertFunctionAddress: ptr::addr_of!(ARA_ASSERT).cast_mut(),
        };
        let initialize = fac
            .initializeARAWithConfiguration
            .context("ARAFactory.initializeARAWithConfiguration is null")?;
        crate::ara::trace(&format!("document.create: initializeARA (api gen {api_generation})"));
        unsafe { initialize(&config) };

        // From here on ARA is initialised, so any early return must uninitialise
        // again to keep the refcount balanced.
        let create_controller = match fac.createDocumentControllerWithDocument {
            Some(f) => f,
            None => {
                if let Some(uninit) = fac.uninitializeARA {
                    unsafe { uninit() };
                }
                anyhow::bail!("ARAFactory.createDocumentControllerWithDocument is null");
            }
        };

        // Boxed so its address stays stable and outlives the call: ARA requires
        // the host instance (and the interfaces it points to) to remain valid
        // until the document controller is destroyed — the plug-in keeps this
        // pointer, not a copy (ARAInterface.h: "must remain valid until all
        // plug-in document controllers created with this struct have been
        // destroyed"). A stack local here dangles after `create` returns and the
        // plug-in crashes on the first model call (e.g. createMusicalContext).
        // Hand the plug-in's own factory document-archive id to the host
        // instance so our ArchivingController can return it from
        // `getDocumentArchiveID` (ARA requires a non-null, non-empty id — the
        // plug-in asserts on it during analysis).
        let host_instance = Box::new(host_controllers::host_instance(fac.documentArchiveID));
        let properties = ARADocumentProperties {
            structSize: core::mem::size_of::<ARADocumentProperties>(),
            name: document_name.map_or(ptr::null(), |n| n.as_ptr().cast()),
        };

        let instance_ptr = unsafe { create_controller(host_instance.as_ref(), &properties) };
        let Some(instance) = (unsafe { instance_ptr.as_ref() }) else {
            if let Some(uninit) = fac.uninitializeARA {
                unsafe { uninit() };
            }
            anyhow::bail!("createDocumentControllerWithDocument returned null");
        };

        let controller_ref = instance.documentControllerRef;
        anyhow::ensure!(
            !instance.documentControllerInterface.is_null(),
            "document controller instance has null interface"
        );
        let interface = unsafe { crate::ara::read_versioned(instance.documentControllerInterface) };
        crate::ara::trace("document.create: document controller ready");

        Ok(Self {
            factory,
            controller_ref,
            interface,
            _host_instance: host_instance,
        })
    }

    /// Playback transformations the plug-in's factory advertises. A region may
    /// only enable a subset of these, so the host uses it to gate time-stretch.
    /// Reads the retained factory pointer (validated non-null at `create`),
    /// defaulting to no flags if it is somehow null.
    pub fn supported_playback_transformation_flags(&self) -> ARAPlaybackTransformationFlags {
        unsafe { self.factory.as_ref() }
            .map(|f| f.supportedPlaybackTransformationFlags)
            .unwrap_or(0)
    }

    /// Opaque controller ref for model-graph calls (audio sources, regions, …).
    pub fn controller_ref(&self) -> ARADocumentControllerRef {
        self.controller_ref
    }

    /// The plug-in's document controller vtable.
    pub fn interface(&self) -> &ARADocumentControllerInterface {
        &self.interface
    }

    /// Open an editing session. All model edits must be bracketed by
    /// `begin_editing` / `end_editing`.
    pub fn begin_editing(&self) {
        if let Some(begin) = self.interface.beginEditing {
            unsafe { begin(self.controller_ref) };
        }
    }

    /// Close the editing session opened by [`Self::begin_editing`].
    pub fn end_editing(&self) {
        if let Some(end) = self.interface.endEditing {
            unsafe { end(self.controller_ref) };
        }
    }

    /// Flush pending plug-in→host model update notifications. Must be called
    /// periodically while not editing (ARA only calls our ModelUpdate
    /// controller from within this call).
    pub fn notify_model_updates(&self) {
        if let Some(notify) = self.interface.notifyModelUpdates {
            unsafe { notify(self.controller_ref) };
        }
    }

    /// Serialise the plug-in's edit state (audio modifications etc.) to an ARA
    /// archive. Call outside an editing session. `None` if the plug-in has no
    /// archiving support or the store failed.
    pub fn store_objects_to_archive(&self) -> Option<Vec<u8>> {
        let store = self.interface.storeObjectsToArchive?;
        let mut writer = host_controllers::AraArchiveWriter::default();
        let writer_ref = ptr::from_mut(&mut writer) as ara_sys::ARAArchiveWriterHostRef;
        let ok = unsafe { store(self.controller_ref, writer_ref, ptr::null()) };
        (ok != 0).then_some(writer.data)
    }

    /// Restore plug-in edit state from an ARA archive. Call inside an editing
    /// session, after the matching model objects (sources / modifications with
    /// the archived persistent ids) have been re-created.
    pub fn restore_objects_from_archive(&self, archive: &[u8]) -> bool {
        let Some(restore) = self.interface.restoreObjectsFromArchive else {
            return false;
        };
        let mut reader = host_controllers::AraArchiveReader::new(archive.to_vec());
        let reader_ref = ptr::from_mut(&mut reader) as ara_sys::ARAArchiveReaderHostRef;
        let ok = unsafe { restore(self.controller_ref, reader_ref, ptr::null()) };
        ok != 0
    }
}

impl Drop for AraDocumentController {
    fn drop(&mut self) {
        // Host must have already destroyed all model objects (sources, contexts,
        // regions) before this point — that ordering is enforced by whoever owns
        // those objects on top of this controller.
        if let Some(destroy) = self.interface.destroyDocumentController {
            unsafe { destroy(self.controller_ref) };
        }
        if let Some(fac) = unsafe { self.factory.as_ref() }
            && let Some(uninitialize) = fac.uninitializeARA
        {
            unsafe { uninitialize() };
        }
    }
}

/// ARA assert callback (`ARAAssertFunction`). The plug-in calls this on a
/// detected host programming error (invalid argument / state / thread); we log
/// the category and diagnosis crash-proof so a contract violation is visible.
unsafe extern "C" fn ara_assert_callback(
    category: ara_sys::ARAAssertCategory,
    _problematic_argument: *const core::ffi::c_void,
    diagnosis: *const core::ffi::c_char,
) {
    let detail = if diagnosis.is_null() {
        String::from("(null)")
    } else {
        unsafe { CStr::from_ptr(diagnosis) }
            .to_string_lossy()
            .into_owned()
    };
    crate::ara::trace(&format!(
        "!!! ARA ASSERT category={category} diagnosis={detail}"
    ));
}

/// Negotiates the highest ARA API generation supported by both the plug-in and
/// this host, erroring if the supported ranges do not overlap.
fn choose_api_generation(factory: &ARAFactory) -> Result<ARAAPIGeneration> {
    let plugin_low = factory.lowestSupportedApiGeneration;
    let plugin_high = factory.highestSupportedApiGeneration;
    let low = plugin_low.max(HOST_MIN_API_GENERATION);
    let high = plugin_high.min(HOST_MAX_API_GENERATION);
    anyhow::ensure!(
        low <= high,
        "no common ARA API generation: plug-in supports {plugin_low}..={plugin_high}, host supports {HOST_MIN_API_GENERATION}..={HOST_MAX_API_GENERATION}"
    );
    Ok(high)
}

/// Model-graph construction. Each method is a thin wrapper that fills the
/// versioned ARA properties struct and dispatches through the plug-in's vtable.
/// All must be called inside a [`AraDocumentController::begin_editing`] /
/// [`AraDocumentController::end_editing`] bracket (except sample-access toggling).
/// Opaque host refs (`ARA*HostRef`) are caller-owned pointers to host model
/// state; the returned `ARA*Ref` are plug-in-owned and must be destroyed in
/// reverse dependency order (regions → modifications → sources; regions →
/// region sequences → musical contexts) before dropping the controller.
impl AraDocumentController {
    /// Create a musical context (bar/beat grid + harmony). daw_01 uses one per
    /// document. `order_index` must increase strictly monotonically across
    /// contexts.
    pub fn create_musical_context(
        &self,
        host_ref: ARAMusicalContextHostRef,
        order_index: i32,
    ) -> Option<ARAMusicalContextRef> {
        let create = self.interface.createMusicalContext?;
        let properties = ARAMusicalContextProperties {
            structSize: core::mem::size_of::<ARAMusicalContextProperties>(),
            name: ptr::null(),
            orderIndex: order_index,
            color: ptr::null(),
        };
        Some(unsafe { create(self.controller_ref, host_ref, &properties) })
    }

    pub fn destroy_musical_context(&self, context: ARAMusicalContextRef) {
        if let Some(destroy) = self.interface.destroyMusicalContext {
            unsafe { destroy(self.controller_ref, context) };
        }
    }

    /// Tell the plug-in the musical context content (tempo / bar signatures we
    /// serve via the ContentAccess controller) changed, so it re-reads it. Used
    /// to push the real song tempo after the context was created with a
    /// placeholder. Must be called inside an editing bracket.
    pub fn update_musical_context_content(&self, context: ARAMusicalContextRef) {
        if let Some(update) = self.interface.updateMusicalContextContent {
            unsafe {
                update(
                    self.controller_ref,
                    context,
                    ptr::null(),
                    kARAContentUpdateEverythingChanged.0,
                );
            }
        }
    }

    /// Create a region sequence (≈ a host track/lane) bound to a musical
    /// context. Playback regions are grouped under sequences.
    pub fn create_region_sequence(
        &self,
        host_ref: ARARegionSequenceHostRef,
        order_index: i32,
        musical_context: ARAMusicalContextRef,
    ) -> Option<ARARegionSequenceRef> {
        let create = self.interface.createRegionSequence?;
        let properties = ARARegionSequenceProperties {
            structSize: core::mem::size_of::<ARARegionSequenceProperties>(),
            name: ptr::null(),
            orderIndex: order_index,
            musicalContextRef: musical_context,
            color: ptr::null(),
        };
        Some(unsafe { create(self.controller_ref, host_ref, &properties) })
    }

    pub fn destroy_region_sequence(&self, sequence: ARARegionSequenceRef) {
        if let Some(destroy) = self.interface.destroyRegionSequence {
            unsafe { destroy(self.controller_ref, sequence) };
        }
    }

    /// Create an audio source. `host_ref` carries our `AraAudioSourceHost`;
    /// `persistent_id` must be unique within the document and stable across
    /// save/restore. Sample access starts disabled — call
    /// [`Self::enable_audio_source_samples_access`] before the plug-in reads.
    /// `channel_arrangement` is left undefined (host default L/R), which ARA
    /// permits for mono/stereo.
    pub fn create_audio_source(
        &self,
        host_ref: ARAAudioSourceHostRef,
        persistent_id: &CStr,
        sample_count: ARASampleCount,
        sample_rate: ARASampleRate,
        channel_count: ARAChannelCount,
        merits_64bit: bool,
    ) -> Option<ARAAudioSourceRef> {
        let create = self.interface.createAudioSource?;
        let properties = ARAAudioSourceProperties {
            structSize: core::mem::size_of::<ARAAudioSourceProperties>(),
            name: ptr::null(),
            persistentID: persistent_id.as_ptr(),
            sampleCount: sample_count,
            sampleRate: sample_rate,
            channelCount: channel_count,
            merits64BitSamples: ARABool::from(merits_64bit),
            channelArrangementDataType: kARAChannelArrangementUndefined.0,
            channelArrangement: ptr::null(),
        };
        Some(unsafe { create(self.controller_ref, host_ref, &properties) })
    }

    /// Enable/disable plug-in sample reads for a source. Access is disabled by
    /// default after creation; this is a synchronous, comparatively expensive
    /// call (it tears down readers / aborts analysis), so toggle sparingly.
    pub fn enable_audio_source_samples_access(&self, source: ARAAudioSourceRef, enable: bool) {
        if let Some(set) = self.interface.enableAudioSourceSamplesAccess {
            unsafe { set(self.controller_ref, source, ARABool::from(enable)) };
        }
    }

    /// Explicitly ask the plug-in to analyse `source` for every content type it
    /// can analyse (`ARAFactory::analyzeableContentTypes`). Without this a plug-in
    /// is free to **postpone analysis indefinitely** (ARAInterface.h), which is
    /// exactly what Melodyne does when driven head-less / without its editor —
    /// leaving it nothing to render (silence). Call after sample access is
    /// enabled, on the model thread, outside an editing cycle. The plug-in reads
    /// samples on a background thread and reports completion via the
    /// ModelUpdateController during `notify_model_updates`.
    pub fn request_audio_source_content_analysis(&self, source: ARAAudioSourceRef) {
        let Some(request) = self.interface.requestAudioSourceContentAnalysis else {
            return;
        };
        let Some(fac) = (unsafe { self.factory.as_ref() }) else {
            return;
        };
        let count = fac.analyzeableContentTypesCount;
        let types = fac.analyzeableContentTypes;
        if count == 0 || types.is_null() {
            return;
        }
        unsafe { request(self.controller_ref, source, count, types) };
        crate::ara::trace(&format!(
            "ARA: requested content analysis ({count} content types)"
        ));
    }

    /// Whether the plug-in's current license permits analysing its analyzeable
    /// content types and rendering with no playback transformation — i.e. the
    /// capabilities we use. `None` if the plug-in doesn't expose the call.
    /// Returns `Some(false)` when the plug-in is loaded but not licensed for
    /// these tasks (e.g. PACE/iLok authorization unavailable), which makes it
    /// render silence. `run_dialog` asks the plug-in to pop its activation UI.
    pub fn is_licensed_for_capabilities(&self, run_dialog: bool) -> Option<bool> {
        let check = self.interface.isLicensedForCapabilities?;
        let fac = unsafe { self.factory.as_ref() }?;
        let result = unsafe {
            check(
                self.controller_ref,
                ARABool::from(run_dialog),
                fac.analyzeableContentTypesCount,
                fac.analyzeableContentTypes,
                kARAPlaybackTransformationNoChanges.0,
            )
        };
        Some(result != 0)
    }

    pub fn destroy_audio_source(&self, source: ARAAudioSourceRef) {
        if let Some(destroy) = self.interface.destroyAudioSource {
            unsafe { destroy(self.controller_ref, source) };
        }
    }

    /// Create an audio modification (the user-editable layer — pitch edits etc.)
    /// over a source. `persistent_id` must be unique within the document.
    pub fn create_audio_modification(
        &self,
        source: ARAAudioSourceRef,
        host_ref: ARAAudioModificationHostRef,
        persistent_id: &CStr,
    ) -> Option<ARAAudioModificationRef> {
        let create = self.interface.createAudioModification?;
        let properties = ARAAudioModificationProperties {
            structSize: core::mem::size_of::<ARAAudioModificationProperties>(),
            name: ptr::null(),
            persistentID: persistent_id.as_ptr(),
        };
        Some(unsafe { create(self.controller_ref, source, host_ref, &properties) })
    }

    pub fn destroy_audio_modification(&self, modification: ARAAudioModificationRef) {
        if let Some(destroy) = self.interface.destroyAudioModification {
            unsafe { destroy(self.controller_ref, modification) };
        }
    }

    /// Create a playback region mapping a slice of the modification's audio onto
    /// the song timeline (seconds). `time_stretch` enables
    /// `kARAPlaybackTransformationTimestretch`, letting the playback duration
    /// differ from the modification duration (pitch-preserving stretch); when
    /// false the caller must pass equal durations (Raw). The region sequence
    /// carries the musical context (the deprecated per-region context field is
    /// left null).
    #[allow(clippy::too_many_arguments)]
    pub fn create_playback_region(
        &self,
        modification: ARAAudioModificationRef,
        host_ref: ARAPlaybackRegionHostRef,
        region_sequence: ARARegionSequenceRef,
        start_in_modification: ARATimePosition,
        duration_in_modification: ARATimeDuration,
        start_in_playback: ARATimePosition,
        duration_in_playback: ARATimeDuration,
        time_stretch: bool,
    ) -> Option<ARAPlaybackRegionRef> {
        let create = self.interface.createPlaybackRegion?;
        let properties = playback_region_properties(
            region_sequence,
            start_in_modification,
            duration_in_modification,
            start_in_playback,
            duration_in_playback,
            time_stretch,
        );
        Some(unsafe { create(self.controller_ref, modification, host_ref, &properties) })
    }

    /// Update an existing playback region's placement / stretch in place
    /// (`updatePlaybackRegionProperties`). All properties are re-specified, per
    /// the ARA contract that the host always supplies the full set and the
    /// plug-in works out what changed. Unlike create/destroy or
    /// add/removePlaybackRegion (which require the instance inactive), this is
    /// safe while the plug-in renders — the caller brackets it in
    /// begin/endEditing, which the plug-in uses for render-thread
    /// synchronisation — so it is the path for live tempo / edge-drag follow
    /// without rebuilding the document.
    #[allow(clippy::too_many_arguments)]
    pub fn update_playback_region_properties(
        &self,
        region: ARAPlaybackRegionRef,
        region_sequence: ARARegionSequenceRef,
        start_in_modification: ARATimePosition,
        duration_in_modification: ARATimeDuration,
        start_in_playback: ARATimePosition,
        duration_in_playback: ARATimeDuration,
        time_stretch: bool,
    ) {
        let Some(update) = self.interface.updatePlaybackRegionProperties else {
            return;
        };
        let properties = playback_region_properties(
            region_sequence,
            start_in_modification,
            duration_in_modification,
            start_in_playback,
            duration_in_playback,
            time_stretch,
        );
        unsafe { update(self.controller_ref, region, &properties) };
    }

    pub fn destroy_playback_region(&self, region: ARAPlaybackRegionRef) {
        if let Some(destroy) = self.interface.destroyPlaybackRegion {
            unsafe { destroy(self.controller_ref, region) };
        }
    }
}

/// Build the shared ARA playback-region properties. `time_stretch` selects the
/// transformation: when set, the modification slice maps onto a (possibly
/// different) playback duration via `kARAPlaybackTransformationTimestretch`;
/// otherwise the region is left untransformed (`NoChanges`, durations must
/// match). Used by both create and update so the two never drift apart.
fn playback_region_properties(
    region_sequence: ARARegionSequenceRef,
    start_in_modification: ARATimePosition,
    duration_in_modification: ARATimeDuration,
    start_in_playback: ARATimePosition,
    duration_in_playback: ARATimeDuration,
    time_stretch: bool,
) -> ARAPlaybackRegionProperties {
    // With the time-stretch flag off, ARA requires the host to keep playback and
    // modification durations equal ("If disabled, the host must always specify
    // the same duration in modification and playback time") — the plug-in
    // ignores the distinction. Clamp here so a non-stretching region (Raw, or a
    // plug-in that does not advertise time-stretch) is always spec-correct
    // regardless of the playback duration the caller computed.
    let duration_in_playback = if time_stretch {
        duration_in_playback
    } else {
        duration_in_modification
    };
    ARAPlaybackRegionProperties {
        structSize: core::mem::size_of::<ARAPlaybackRegionProperties>(),
        transformationFlags: if time_stretch {
            kARAPlaybackTransformationTimestretch.0
        } else {
            kARAPlaybackTransformationNoChanges.0
        },
        startInModificationTime: start_in_modification,
        durationInModificationTime: duration_in_modification,
        startInPlaybackTime: start_in_playback,
        durationInPlaybackTime: duration_in_playback,
        musicalContextRef: ptr::null_mut(),
        regionSequenceRef: region_sequence,
        name: ptr::null(),
        color: ptr::null(),
    }
}
