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
    ARAFactory, ARAMusicalContextRef, ARAPlaybackRegionRef, ARAPlugInExtensionInstance,
    ARARegionSequenceRef,
};
use common::protocol::{AraClipSpec, AraSourceSpec};

use crate::ara::audio_source::AraAudioSourceHost;
use crate::ara::document::AraDocumentController;
use crate::ara::extension::AraPlugInExtension;

// The clip/source spec is defined once in `common::protocol`
// ([`AraClipSpec`] / [`AraSourceSpec`]) since it crosses the IPC boundary; this
// module consumes it directly.

/// A host model source + its plug-in-side refs, owned for the session's life.
struct OwnedAraSource {
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
    musical_context: ARAMusicalContextRef,
    region_sequence: ARARegionSequenceRef,
    sources: Vec<OwnedAraSource>,
    extension: AraPlugInExtension,
    controller: AraDocumentController,
}

impl AraSession {
    /// Build the ARA model graph for a loaded plug-in and bind it for playback
    /// rendering. `bind` performs the companion-API-specific instance binding
    /// (CLAP / VST3) given the freshly created document controller ref.
    ///
    /// # Safety
    /// `factory` must be valid and belong to the loaded, inactive ARA-capable
    /// plug-in that `bind` binds. The plug-in must remain loaded for the
    /// session's lifetime.
    pub unsafe fn setup<F>(
        factory: *const ARAFactory,
        clips: &[AraClipSpec],
        bind: F,
    ) -> Result<Self>
    where
        F: FnOnce(ARADocumentControllerRef) -> Option<*const ARAPlugInExtensionInstance>,
    {
        let controller = unsafe { AraDocumentController::create(factory, None) }?;

        controller.begin_editing();
        let result = unsafe { Self::build_graph(&controller, clips, bind) };
        controller.end_editing();

        match result {
            Ok((musical_context, region_sequence, sources, extension)) => {
                // Enable sample access after editing (MiniHost order), then hand
                // every region to the renderer so playback produces edited audio.
                for owned in &sources {
                    controller.enable_audio_source_samples_access(owned.source_ref, true);
                }
                for owned in &sources {
                    extension.add_playback_region(owned.region_ref);
                }
                Ok(Self {
                    musical_context,
                    region_sequence,
                    sources,
                    extension,
                    controller,
                })
            }
            Err(e) => Err(e),
        }
    }

    /// Inner graph construction, run inside the begin/end editing bracket.
    /// Returns the context/sequence refs, owned sources, and bound extension.
    unsafe fn build_graph<F>(
        controller: &AraDocumentController,
        clips: &[AraClipSpec],
        bind: F,
    ) -> Result<(
        ARAMusicalContextRef,
        ARARegionSequenceRef,
        Vec<OwnedAraSource>,
        AraPlugInExtension,
    )>
    where
        F: FnOnce(ARADocumentControllerRef) -> Option<*const ARAPlugInExtensionInstance>,
    {
        let musical_context = controller
            .create_musical_context(std::ptr::null_mut(), 0)
            .context("plug-in returned null musical context")?;
        let region_sequence = controller
            .create_region_sequence(std::ptr::null_mut(), 0, musical_context)
            .context("plug-in returned null region sequence")?;

        let mut sources = Vec::with_capacity(clips.len());
        for clip in clips {
            sources.push(unsafe { build_source(controller, region_sequence, clip) }?);
        }

        // Bind must happen before the plug-in is activated; the instance is still
        // inactive here. The caller's closure performs the companion-API-specific
        // bind (CLAP / VST3) with the host's known/assigned roles.
        let instance_ptr = bind(controller.controller_ref())
            .context("ARA bind_to_document_controller returned null")?;
        let extension = unsafe { AraPlugInExtension::from_instance_ptr(instance_ptr) }
            .context("null ARA plug-in extension instance")?;

        Ok((musical_context, region_sequence, sources, extension))
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
}

/// Create one source → modification → playback region chain for a clip.
unsafe fn build_source(
    controller: &AraDocumentController,
    region_sequence: ARARegionSequenceRef,
    clip: &AraClipSpec,
) -> Result<OwnedAraSource> {
    let host = Box::new(match &clip.source {
        AraSourceSpec::WavFile(path) => AraAudioSourceHost::from_wav_file(path)?,
        AraSourceSpec::Pcm {
            samples,
            sample_rate,
            channel_count,
        } => {
            AraAudioSourceHost::from_interleaved(samples.clone().into(), *sample_rate, *channel_count)
        }
    });

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
            clip.start_in_modification_seconds,
            clip.duration_in_modification_seconds,
            clip.start_in_playback_seconds,
            clip.duration_in_playback_seconds,
        )
        .context("plug-in returned null playback region")?;

    Ok(OwnedAraSource {
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
