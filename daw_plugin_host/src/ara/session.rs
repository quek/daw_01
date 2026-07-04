//! Per-plug-in-instance ARA session: the orchestrator that ties the document
//! controller, host model objects, and the bound plug-in extension together.
//!
//! Given a loaded ARA-capable plug-in instance, its [`ARAFactory`], and a set of
//! audio clips, [`AraSession`] builds the ARA model graph following the wiring
//! order from Celemony's `MiniHost.c`:
//!
//! 1. create document controller (injecting our host controllers)
//! 2. `beginEditing` → musical context → region sequence → per clip: audio
//!    source → audio modification → playback region → `endEditing`
//! 3. enable sample access for every source
//! 4. bind the instance with the playback-renderer role
//! 5. `addPlaybackRegion` for every region
//!
//! On drop the graph is torn down bottom-up before the document controller is
//! destroyed and ARA is uninitialised.

use std::ffi::CString;

use anyhow::{Context, Result};
use ara_sys::{
    ARAAudioModificationRef, ARAAudioSourceHostRef, ARAAudioSourceRef, ARADocumentControllerRef,
    ARAFactory, ARAMusicalContextHostRef, ARAMusicalContextRef, ARAPlaybackRegionRef,
    ARAPlaybackTransformationFlags, ARAPlugInExtensionInstance, ARARegionSequenceRef,
    kARAPlaybackTransformationTimestretch,
};
use common::protocol::{AraClipSpec, AraRegionUpdate};

use crate::ara::audio_source::AraAudioSourceHost;
use crate::ara::document::AraDocumentController;
use crate::ara::extension::AraPlugInExtension;
use crate::ara::host_controllers::AraMusicalContextHost;

// The clip/source spec is defined once in `common::protocol`
// ([`AraClipSpec`]) since it crosses the IPC boundary; this module consumes
// it directly. v29: the source is always an absolute WAV path
// (`AraClipSpec::source_wav`) — the in-memory `Pcm` variant was removed
// (`docs/plan_arch_refactor.md` §2).

/// A host model source + its plug-in-side refs, owned for the session's life.
struct OwnedAraSource {
    /// Stable id (matches `AraClipSpec::persistent_id`) so `update_regions` can
    /// find this region when only its placement changed.
    persistent_id: String,
    /// Boxed so its address is stable — it backs the `ARAAudioSourceHostRef`
    /// the plug-in hands to our AudioAccess controller in `readAudioSamples`.
    _host: Box<AraAudioSourceHost>,
    source_ref: ARAAudioSourceRef,
    modification_ref: ARAAudioModificationRef,
    region_ref: ARAPlaybackRegionRef,
}

/// A live ARA session for one plug-in instance.
///
/// Field order matters for teardown: the host source boxes and bound extension
/// drop before `controller`, whose `Drop` destroys the document controller and
/// uninitialises ARA last.
pub struct AraSession {
    /// Host-side tempo / bar signature served to the plug-in via the
    /// ContentAccess controller. Boxed so its address (the
    /// `ARAMusicalContextHostRef` the plug-in holds) stays stable; updated in
    /// place by [`Self::set_musical_context`].
    musical_context_host: Box<AraMusicalContextHost>,
    musical_context: ARAMusicalContextRef,
    region_sequence: ARARegionSequenceRef,
    sources: Vec<OwnedAraSource>,
    extension: AraPlugInExtension,
    controller: AraDocumentController,
    /// Playback transformations the plug-in advertises (`ARAFactory`). We enable
    /// time-stretch on a region only if the factory lists it (ARA requires the
    /// region's `transformationFlags` to be a subset of the supported set).
    supported_transformation_flags: ARAPlaybackTransformationFlags,
}

impl AraSession {
    /// Create the ARA document controller and bind the plug-in instance for
    /// playback rendering, starting with an empty model (no audio yet). `bind`
    /// performs the companion-API-specific instance binding (CLAP / VST3).
    ///
    /// Per the ARA spec the bind must precede the instance's **first** `activate`
    /// / state load / GUI creation, so this runs at load time, before the host
    /// activates the plug-in. Audio is attached later via [`Self::set_clips`]
    /// (which only edits the model + renderer, never re-binds).
    ///
    /// # Safety
    /// `factory` must be valid and belong to the loaded, not-yet-activated
    /// plug-in that `bind` binds. The plug-in must remain loaded for the
    /// session's lifetime.
    pub unsafe fn create<F>(factory: *const ARAFactory, bind: F) -> Result<Self>
    where
        F: FnOnce(ARADocumentControllerRef) -> Option<*const ARAPlugInExtensionInstance>,
    {
        let controller = unsafe { AraDocumentController::create(factory, None) }?;
        // The plug-in advertises which playback transformations it can perform;
        // we must not enable time-stretch on a region unless it is listed here.
        // Read via the controller, which owns the factory pointer it validated.
        let supported_transformation_flags = controller.supported_playback_transformation_flags();

        // Box the host's tempo/bar model first: its address is the opaque
        // `ARAMusicalContextHostRef` the plug-in keeps and passes back to our
        // ContentAccess controller, so it must be stable and outlive the
        // musical context.
        let mut musical_context_host = Box::new(AraMusicalContextHost::default());
        let musical_context_host_ref =
            std::ptr::from_mut(musical_context_host.as_mut()) as ARAMusicalContextHostRef;

        controller.begin_editing();
        let musical_context = controller
            .create_musical_context(musical_context_host_ref, 0)
            .context("plug-in returned null musical context")?;
        let region_sequence = controller
            .create_region_sequence(std::ptr::null_mut(), 0, musical_context)
            .context("plug-in returned null region sequence")?;
        controller.end_editing();
        crate::ara::trace("session.create: model graph built; binding instance");

        // Bind while the instance is still inactive (before its first activate).
        let instance_ptr = bind(controller.controller_ref())
            .context("ARA bind_to_document_controller returned null")?;
        let extension = unsafe { AraPlugInExtension::from_instance_ptr(instance_ptr) }
            .context("null ARA plug-in extension instance")?;
        crate::ara::trace(&format!(
            "session.create: instance bound; playback_renderer={}, editor_renderer={}, isLicensed={:?}",
            extension.has_playback_renderer(),
            extension.has_editor_renderer(),
            controller.is_licensed_for_capabilities(false),
        ));

        Ok(Self {
            musical_context_host,
            musical_context,
            region_sequence,
            sources: Vec::new(),
            extension,
            controller,
            supported_transformation_flags,
        })
    }

    /// Replace the document's audio sources / playback regions to match `clips`.
    /// Best-effort: a clip whose source can't be decoded is skipped (logged) so
    /// one bad source doesn't drop the rest.
    ///
    /// The caller must ensure the plug-in is **inactive** — ARA's
    /// `addPlaybackRegion` / `removePlaybackRegion` (and detaching regions before
    /// destroying them) require it.
    pub fn set_clips(&mut self, clips: &[AraClipSpec], bpm: f64, time_sig: (u16, u16)) {
        // Detach + destroy the current sources / regions, freeing their host data.
        for owned in &self.sources {
            self.extension.remove_playback_region(owned.region_ref);
        }
        self.controller.begin_editing();
        // Update the musical context to the real song tempo / time signature so
        // the plug-in's editor grid (bars/beats) aligns to the project instead of
        // the placeholder created at bind time. The content controller reads
        // these from the boxed host model, so update it then tell the plug-in to
        // re-read via updateMusicalContextContent.
        self.musical_context_host.seconds_per_quarter = 60.0 / bpm.max(1.0);
        self.musical_context_host.bar_numerator = i32::from(time_sig.0.max(1));
        self.musical_context_host.bar_denominator = i32::from(time_sig.1.max(1));
        self.controller
            .update_musical_context_content(self.musical_context);
        for owned in &self.sources {
            self.controller.destroy_playback_region(owned.region_ref);
            self.controller.destroy_audio_modification(owned.modification_ref);
            self.controller.destroy_audio_source(owned.source_ref);
        }
        self.sources = Vec::new();

        let supports_timestretch = self.supports_timestretch();
        let mut new_sources = Vec::with_capacity(clips.len());
        for clip in clips {
            let time_stretch = clip.placement.time_stretch && supports_timestretch;
            match unsafe {
                build_source(&self.controller, self.region_sequence, clip, time_stretch)
            } {
                Ok(source) => new_sources.push(source),
                Err(e) => {
                    tracing::warn!(
                        error = ?e,
                        id = %clip.persistent_id,
                        "ARA: skipping clip with unreadable source"
                    );
                }
            }
        }
        self.controller.end_editing();

        for owned in &new_sources {
            self.controller
                .enable_audio_source_samples_access(owned.source_ref, true);
            // Force analysis now; otherwise the plug-in may postpone it forever
            // (no editor / head-less), leaving nothing to render.
            self.controller
                .request_audio_source_content_analysis(owned.source_ref);
        }
        for owned in &new_sources {
            self.extension.add_playback_region(owned.region_ref);
        }
        self.sources = new_sources;

        // Note: we deliberately do NOT assign regions/sequences to the *editor*
        // renderer here. That renderer is for transient preview audio and the
        // plug-in asserts its preview-region list stays empty otherwise
        // (Melodyne: `getPlaybackRegionsForPreview()->getCount()` must be 0).
        // What populates the editor's timeline is the editor-view *selection*,
        // pushed below and re-pushed when the editor view opens.
        self.notify_editor_selection();
    }

    /// Drive the plug-in's deferred model work / analysis. ARA requires the host
    /// to call this periodically while not editing — it is the only point at
    /// which the plug-in may progress background analysis and flush pending
    /// model-update notifications. Skipping it leaves e.g. Melodyne's audio
    /// analysis unfinished, so playback rendering produces silence.
    pub fn notify_model_updates(&self) {
        self.controller.notify_model_updates();
    }

    /// Tell the plug-in's editor view which regions / sequences are selected, so
    /// its editor displays them. ARA requires this whenever the plug-in view is
    /// (re-)opened (ARAInterface.h: "the host should send an update of the
    /// selection when (re-)opening an ARA plug-in view"), so the GUI path calls
    /// this right after creating the editor view — without it Melodyne's timeline
    /// stays empty even though playback renders.
    pub fn notify_editor_selection(&self) {
        if self.sources.is_empty() {
            return;
        }
        let region_refs: Vec<ARAPlaybackRegionRef> =
            self.sources.iter().map(|o| o.region_ref).collect();
        self.extension
            .notify_selection(&region_refs, &[self.region_sequence]);
    }

    /// Serialise the plug-in's ARA edit state for project save.
    pub fn store_archive(&self) -> Option<Vec<u8>> {
        self.controller.store_objects_to_archive()
    }

    /// Restore ARA edit state (from a prior [`Self::store_archive`]) onto the
    /// already-built model graph. Bracketed in an editing session, as ARA 2
    /// requires for `restoreObjectsFromArchive`.
    pub fn restore_archive(&self, archive: &[u8]) -> bool {
        self.controller.begin_editing();
        let ok = self.controller.restore_objects_from_archive(archive);
        self.controller.end_editing();
        ok
    }

    /// Update the placement / stretch of already-present regions in place,
    /// matched by `persistent_id`, without rebuilding the document. Safe while
    /// the plug-in renders: `updatePlaybackRegionProperties` only re-states
    /// region properties (not renderer assignment) and is bracketed in
    /// begin/endEditing, which the plug-in uses for render-thread sync. Ids not
    /// currently present are ignored (a clip-set change goes through
    /// [`Self::set_clips`] instead).
    pub fn update_regions(&self, updates: &[AraRegionUpdate]) {
        if updates.is_empty() {
            return;
        }
        let supports_timestretch = self.supports_timestretch();
        self.controller.begin_editing();
        for upd in updates {
            let Some(owned) = self
                .sources
                .iter()
                .find(|o| o.persistent_id == upd.persistent_id)
            else {
                continue;
            };
            let p = &upd.placement;
            self.controller.update_playback_region_properties(
                owned.region_ref,
                self.region_sequence,
                p.start_in_modification_seconds,
                p.duration_in_modification_seconds,
                p.start_in_playback_seconds,
                p.duration_in_playback_seconds,
                p.time_stretch && supports_timestretch,
            );
        }
        self.controller.end_editing();
    }

    /// Whether the plug-in advertises the time-stretch playback transformation.
    /// Regions only get `kARAPlaybackTransformationTimestretch` when this holds;
    /// otherwise the host must keep modification and playback durations equal.
    fn supports_timestretch(&self) -> bool {
        self.supported_transformation_flags & kARAPlaybackTransformationTimestretch.0 != 0
    }
}

/// Create one source → modification → playback region chain for a clip.
unsafe fn build_source(
    controller: &AraDocumentController,
    region_sequence: ARARegionSequenceRef,
    clip: &AraClipSpec,
    time_stretch: bool,
) -> Result<OwnedAraSource> {
    let host = Box::new(AraAudioSourceHost::from_wav_file(&clip.source_wav)?);

    // The boxed host's address is the opaque host ref the plug-in passes back.
    let host_ref: ARAAudioSourceHostRef = (std::ptr::from_ref::<AraAudioSourceHost>(&host)) as _;

    let source_id =
        CString::new(clip.persistent_id.as_str()).context("persistent_id has interior NUL")?;
    let modification_id = CString::new(format!("{}/mod", clip.persistent_id))
        .context("modification id has interior NUL")?;

    let sample_count = i64::try_from(host.frame_count).unwrap_or(i64::MAX);
    let channel_count = i32::try_from(host.channel_count).unwrap_or(0);
    let sample_rate = host.sample_rate;

    let source_ref = controller
        .create_audio_source(
            host_ref,
            source_id.as_c_str(),
            sample_count,
            sample_rate,
            channel_count,
            false,
        )
        .context("plug-in returned null audio source")?;

    let modification_ref = controller
        .create_audio_modification(source_ref, std::ptr::null_mut(), modification_id.as_c_str())
        .context("plug-in returned null audio modification")?;

    let region_ref = controller
        .create_playback_region(
            modification_ref,
            std::ptr::null_mut(),
            region_sequence,
            clip.placement.start_in_modification_seconds,
            clip.placement.duration_in_modification_seconds,
            clip.placement.start_in_playback_seconds,
            clip.placement.duration_in_playback_seconds,
            time_stretch,
        )
        .context("plug-in returned null playback region")?;

    Ok(OwnedAraSource {
        persistent_id: clip.persistent_id.clone(),
        _host: host,
        source_ref,
        modification_ref,
        region_ref,
    })
}

impl Drop for AraSession {
    fn drop(&mut self) {
        // Detach regions from the renderer before destroying them, then tear the
        // model graph down bottom-up. The controller (and ARA uninitialise) is
        // released afterwards when the `controller` field drops.
        for owned in &self.sources {
            self.extension.remove_playback_region(owned.region_ref);
        }
        self.controller.begin_editing();
        for owned in &self.sources {
            self.controller.destroy_playback_region(owned.region_ref);
            self.controller.destroy_audio_modification(owned.modification_ref);
            self.controller.destroy_audio_source(owned.source_ref);
        }
        self.controller.destroy_region_sequence(self.region_sequence);
        self.controller.destroy_musical_context(self.musical_context);
        self.controller.end_editing();
    }
}
