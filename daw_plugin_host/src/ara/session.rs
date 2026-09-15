//! Per-plug-in-instance ARA session: the orchestrator that ties the document
//! controller, host model objects, and the bound plug-in extension together.
//!
//! Given a loaded ARA-capable plug-in instance, its [`ARAFactory`], and a set of
//! audio clips, [`AraSession`] builds the ARA model graph following the wiring
//! order from Celemony's `MiniHost.c`:
//!
//! 1. create document controller (injecting our host controllers)
//! 2. `beginEditing` → musical context → region sequence → audio sources →
//!    audio modifications → playback regions → `endEditing`
//! 3. enable sample access for every source
//! 4. bind the instance with the playback-renderer role
//! 5. `addPlaybackRegion` for every region
//!
//! The graph is then **edited in place** to follow the song ([`AraSession::set_clips`],
//! diff in [`crate::ara::graph_plan`]): one audio source per source file, one
//! audio modification per content take (split pieces share it, so the plug-in's
//! edits continue across a split), one playback region per piece a clip window
//! shows. Objects whose persistent id survives are never re-created; an audio
//! modification the song drops leaves a partial archive of its state behind, and
//! comes back from it when the song brings it back (undo / redo).
//!
//! On drop the graph is torn down bottom-up before the document controller is
//! destroyed and ARA is uninitialised.

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::path::PathBuf;

use anyhow::{Context, Result};
use ara_sys::{
    ARAAudioModificationRef, ARAAudioSourceHostRef, ARAAudioSourceRef, ARADocumentControllerRef,
    ARAFactory, ARAMusicalContextHostRef, ARAMusicalContextRef, ARAPlaybackRegionRef,
    ARAPlaybackTransformationFlags, ARAPlugInExtensionInstance, ARARegionSequenceRef,
    kARAPlaybackTransformationTimestretch,
};
use common::ara_ids::{AraIdAlias, archived_id};
use common::protocol::{AraClipSpec, AraRegionUpdate};

use crate::ara::audio_source::AraAudioSourceHost;
use crate::ara::document::AraDocumentController;
use crate::ara::extension::AraPlugInExtension;
use crate::ara::graph_plan::{self, GraphNow};
use crate::ara::host_controllers::AraMusicalContextHost;

// The clip/source spec is defined once in `common::protocol`
// ([`AraClipSpec`]) since it crosses the IPC boundary; this module consumes
// it directly. v29: the source is always an absolute WAV path
// (`AraClipSpec::source_wav`) — the in-memory `Pcm` variant was removed
// (`docs/plan_arch_refactor.md` §2).

/// The saved ARA archive handed to [`AraSession::set_clips`]: its bytes and the
/// legacy persistent-id aliases it was written with (empty = current ids).
#[derive(Clone, Copy)]
pub struct SavedArchive<'a> {
    pub bytes: &'a [u8],
    pub ids: &'a [AraIdAlias],
}

/// A host model audio source + its plug-in-side ref, owned for the session's life.
struct OwnedSource {
    persistent_id: String,
    wav: PathBuf,
    /// Boxed so its address is stable — it backs the `ARAAudioSourceHostRef`
    /// the plug-in hands to our AudioAccess controller in `readAudioSamples`.
    _host: Box<AraAudioSourceHost>,
    source_ref: ARAAudioSourceRef,
}

/// An audio modification (the plug-in's edit layer) over one source.
struct OwnedModification {
    persistent_id: String,
    source_id: String,
    modification_ref: ARAAudioModificationRef,
}

/// Where a created audio modification's state is restored from ([`AraSession::restore_created`]).
enum RestoreFrom {
    /// The session's copy taken when the modification with this id was destroyed ([`AraSession::retired`]).
    Retired(String),
    /// The saved archive, the state written under this id (or its legacy alias).
    Saved(String),
}

/// A playback region on one modification.
struct OwnedRegion {
    /// Host key (`AraClipSpec::region_key`) so `update_regions` can find it.
    key: String,
    modification_id: String,
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
    /// place by [`Self::set_clips`].
    musical_context_host: Box<AraMusicalContextHost>,
    musical_context: ARAMusicalContextRef,
    region_sequence: ARARegionSequenceRef,
    sources: Vec<OwnedSource>,
    modifications: Vec<OwnedModification>,
    regions: Vec<OwnedRegion>,
    /// The graph has been built with at least one audio modification. Until then
    /// the saved archive is the whole document's state (restored with its
    /// document data, every object from its own id); afterwards it is only the
    /// fallback for objects created later.
    built: bool,
    /// The state of each audio modification this session destroyed, taken just
    /// before destroying it (a partial archive, latest per persistent id). When
    /// the song brings the same modification back (undo / redo), its edits come
    /// back from here instead of from the saved archive (the last save) —
    /// ARAInterface.h "Partial Document Persistency".
    retired: HashMap<String, Vec<u8>>,
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
            modifications: Vec::new(),
            regions: Vec::new(),
            built: false,
            retired: HashMap::new(),
            extension,
            controller,
            supported_transformation_flags,
        })
    }

    /// Edit the document's model graph in place to match `clips`
    /// ([`graph_plan::plan`]): regions / modifications / sources whose ids are gone
    /// are removed bottom-up, new ones created top-down, surviving regions get
    /// their placement updated. Surviving modifications keep the plug-in's live
    /// edits (a split adds a region on the same modification); a destroyed one
    /// leaves its state in [`Self::retired`] first. Only **the objects created
    /// here** are restored ([`Self::restore_created`]), inside the same editing
    /// cycle, as ARA's unarchiving session prescribes. Best-effort: a clip whose
    /// source can't be decoded is skipped (logged) so one bad source doesn't drop
    /// the rest.
    ///
    /// The caller must ensure the plug-in is **inactive** — ARA's
    /// `addPlaybackRegion` / `removePlaybackRegion` (and detaching regions before
    /// destroying them) require it.
    pub fn set_clips(&mut self, clips: &[AraClipSpec], bpm: f64, time_sig: (u16, u16), archive: Option<SavedArchive<'_>>) {
        let plan = graph_plan::plan(&self.graph_now(), clips);
        let first_build = !self.built;
        // ARA stores archives only outside an editing session, so keep the state of the
        // modifications about to go before the edit begins.
        self.retire_modifications(&plan.destroy_modifications);
        for owned in self.regions.iter().filter(|r| plan.remove_regions.contains(&r.key)) {
            self.extension.remove_playback_region(owned.region_ref);
        }
        self.controller.begin_editing();
        self.update_musical_context(bpm, time_sig);
        self.destroy_planned(&plan);

        let created_sources = self.create_sources(clips, &plan.create_sources);
        let starts = self.create_modifications(clips, &plan.create_modifications, first_build);
        let created_regions = self.create_regions(clips, &plan.create_regions);
        for &i in &plan.update_regions {
            self.update_region(&clips[i].region_key, &clips[i].placement);
        }
        self.restore_created(archive, first_build, &created_sources, &starts);
        self.controller.end_editing();
        self.built |= !self.modifications.is_empty();

        for owned in self.sources.iter().filter(|s| created_sources.contains(&s.persistent_id)) {
            self.controller.enable_audio_source_samples_access(owned.source_ref, true);
            // Force analysis now; otherwise the plug-in may postpone it forever
            // (no editor / head-less), leaving nothing to render.
            self.controller.request_audio_source_content_analysis(owned.source_ref);
        }
        for owned in self.regions.iter().filter(|r| created_regions.contains(&r.key)) {
            self.extension.add_playback_region(owned.region_ref);
        }

        // Note: we deliberately do NOT assign regions/sequences to the *editor*
        // renderer here. That renderer is for transient preview audio and the
        // plug-in asserts its preview-region list stays empty otherwise
        // (Melodyne: `getPlaybackRegionsForPreview()->getCount()` must be 0).
        // What populates the editor's timeline is the editor-view *selection*,
        // pushed below and re-pushed when the editor view opens.
        self.notify_editor_selection();
    }

    /// The ids currently in the graph (input of [`graph_plan::plan`]).
    fn graph_now(&self) -> GraphNow<'_> {
        GraphNow {
            sources: self.sources.iter().map(|s| (s.persistent_id.as_str(), s.wav.as_path())).collect(),
            modifications: self.modifications.iter().map(|m| (m.persistent_id.as_str(), m.source_id.as_str())).collect(),
            regions: self.regions.iter().map(|r| (r.key.as_str(), r.modification_id.as_str())).collect(),
        }
    }

    /// Update the musical context to the real song tempo / time signature so the
    /// plug-in's editor grid (bars/beats) aligns to the project instead of the
    /// placeholder created at bind time. The content controller reads these from
    /// the boxed host model, so update it then tell the plug-in to re-read via
    /// updateMusicalContextContent. Inside an editing cycle.
    fn update_musical_context(&mut self, bpm: f64, time_sig: (u16, u16)) {
        self.musical_context_host.seconds_per_quarter = 60.0 / bpm.max(1.0);
        self.musical_context_host.bar_numerator = i32::from(time_sig.0.max(1));
        self.musical_context_host.bar_denominator = i32::from(time_sig.1.max(1));
        self.controller.update_musical_context_content(self.musical_context);
    }

    /// Destroy the planned regions → modifications → sources (dependency order). Inside an editing cycle.
    fn destroy_planned(&mut self, plan: &graph_plan::GraphPlan) {
        let controller = &self.controller;
        self.regions.retain(|r| {
            let gone = plan.remove_regions.contains(&r.key);
            if gone {
                controller.destroy_playback_region(r.region_ref);
            }
            !gone
        });
        self.modifications.retain(|m| {
            let gone = plan.destroy_modifications.contains(&m.persistent_id);
            if gone {
                controller.destroy_audio_modification(m.modification_ref);
            }
            !gone
        });
        self.sources.retain(|s| {
            let gone = plan.destroy_sources.contains(&s.persistent_id);
            if gone {
                controller.destroy_audio_source(s.source_ref);
            }
            !gone
        });
    }

    /// Create the audio sources for `clips[i]` (`indices`); returns the ids created.
    fn create_sources(&mut self, clips: &[AraClipSpec], indices: &[usize]) -> Vec<String> {
        let mut created = Vec::new();
        for clip in indices.iter().map(|&i| &clips[i]) {
            match unsafe { build_source(&self.controller, clip) } {
                Ok(source) => {
                    created.push(source.persistent_id.clone());
                    self.sources.push(source);
                }
                Err(e) => tracing::warn!(error = ?e, id = %clip.source_id, "ARA: skipping unreadable audio source"),
            }
        }
        created
    }

    /// Keep the plug-in state of the audio modifications `ids` (about to be destroyed) in
    /// [`Self::retired`]. Outside an editing cycle.
    fn retire_modifications(&mut self, ids: &HashSet<String>) {
        for m in self.modifications.iter().filter(|m| ids.contains(&m.persistent_id)) {
            if let Some(bytes) = self.controller.store_modification_to_archive(m.modification_ref) {
                self.retired.insert(m.persistent_id.clone(), bytes);
            }
        }
    }

    /// Create the audio modifications for `clips[i]` on their (existing or just created) sources, and return
    /// which of them to restore and from where: `(modification id, state to restore)`. How each one starts
    /// ([`graph_plan::modification_start`]): cloned from an origin that was in the document before this edit
    /// (content un-shared, `cloneAudioModification` — not restored), or created empty and restored.
    fn create_modifications(
        &mut self,
        clips: &[AraClipSpec],
        indices: &[usize],
        first_build: bool,
    ) -> Vec<(String, RestoreFrom)> {
        // Only a modification that was already in the document carries edits to clone: one created in this
        // same edit is still empty until `restore_created`.
        let existing: HashSet<String> = self.modifications.iter().map(|m| m.persistent_id.clone()).collect();
        let mut restore = Vec::new();
        for clip in indices.iter().map(|&i| &clips[i]) {
            let Some(source_ref) = self.sources.iter().find(|s| s.persistent_id == clip.source_id).map(|s| s.source_ref)
            else {
                continue;
            };
            let Ok(id) = CString::new(clip.modification_id.as_str()) else {
                tracing::warn!(id = %clip.modification_id, "ARA: modification id has interior NUL");
                continue;
            };
            let (modification_ref, restore_from) = self.start_modification(clip, source_ref, &id, first_build, &existing);
            let Some(modification_ref) = modification_ref.filter(|r| !r.is_null()) else {
                tracing::warn!(id = %clip.modification_id, "ARA: plug-in returned null audio modification");
                continue;
            };
            if let Some(from) = restore_from {
                restore.push((clip.modification_id.clone(), from));
            }
            self.modifications.push(OwnedModification {
                persistent_id: clip.modification_id.clone(),
                source_id: clip.source_id.clone(),
                modification_ref,
            });
        }
        restore
    }

    /// Create the audio modification `id` for `clip` the way [`graph_plan::modification_start`] decides: the
    /// plug-in's ref, and the state to restore into it (`None` = cloned from its origin's live edits). `existing`
    /// = the modifications that were in the document before this edit.
    fn start_modification(
        &self,
        clip: &AraClipSpec,
        source_ref: ARAAudioSourceRef,
        id: &CStr,
        first_build: bool,
        existing: &HashSet<String>,
    ) -> (Option<ARAAudioModificationRef>, Option<RestoreFrom>) {
        use graph_plan::ModificationStart;
        let live = |origin: &str| {
            let owned = self.modifications.iter().find(|m| m.persistent_id == origin && m.source_id == clip.source_id);
            owned.filter(|_| existing.contains(origin)).map(|m| m.modification_ref)
        };
        let retired = |id: &str| self.retired.contains_key(id);
        let origin_live = clip.modification_origin.as_deref().and_then(live).is_some();
        let from = match graph_plan::modification_start(clip, first_build, origin_live, retired) {
            ModificationStart::Clone(origin) => {
                let cloned = live(origin).and_then(|o| self.controller.clone_audio_modification(o, std::ptr::null_mut(), id));
                if cloned.is_some() {
                    return (cloned, None);
                }
                // No clone from the plug-in: start from the origin's last known state instead.
                if retired(origin) { RestoreFrom::Retired(origin.to_owned()) } else { RestoreFrom::Saved(origin.to_owned()) }
            }
            ModificationStart::Retired(from) => RestoreFrom::Retired(from.to_owned()),
            ModificationStart::Saved(from) => RestoreFrom::Saved(from.to_owned()),
        };
        (self.controller.create_audio_modification(source_ref, std::ptr::null_mut(), id), Some(from))
    }

    /// Restore the state of the objects this edit created (inside its editing cycle): first from the saved
    /// `archive` — the created audio sources and the modifications that start from it (ARA: a call restoring a
    /// source must include or precede its modifications; legacy ids mapped through the archive's aliases; the
    /// document data only on the first build) — then each modification that starts from a state this session
    /// kept when destroying it. A modification back from its own kept state owns it again (a later destroy
    /// keeps a fresh copy).
    fn restore_created(
        &mut self,
        archive: Option<SavedArchive<'_>>,
        first_build: bool,
        created_sources: &[String],
        starts: &[(String, RestoreFrom)],
    ) {
        if let Some(saved) = archive.filter(|a| !a.bytes.is_empty()) {
            let sources: Vec<(&str, &str)> =
                created_sources.iter().map(|id| (archived_id(saved.ids, id), id.as_str())).collect();
            let modifications: Vec<(&str, &str)> = starts
                .iter()
                .filter_map(|(id, from)| match from {
                    RestoreFrom::Saved(from) => Some((archived_id(saved.ids, from), id.as_str())),
                    RestoreFrom::Retired(_) => None,
                })
                .collect();
            if (!sources.is_empty() || !modifications.is_empty())
                && !self.controller.restore_objects_from_archive(saved.bytes, first_build, &sources, &modifications)
            {
                tracing::warn!(n_sources = sources.len(), n_modifications = modifications.len(), "ARA: restoring the archive failed");
            }
        }
        for (id, from) in starts {
            let RestoreFrom::Retired(from) = from else {
                continue;
            };
            let Some(bytes) = self.retired.get(from) else {
                continue;
            };
            if !self.controller.restore_objects_from_archive(bytes, false, &[], &[(from.as_str(), id.as_str())]) {
                tracing::warn!(id = %id, from = %from, "ARA: restoring a destroyed modification's state failed");
            }
        }
        for (id, from) in starts {
            if matches!(from, RestoreFrom::Retired(from) if from == id) {
                self.retired.remove(id);
            }
        }
    }

    /// Create the playback regions for `clips[i]` on their modifications; returns the keys created.
    fn create_regions(&mut self, clips: &[AraClipSpec], indices: &[usize]) -> Vec<String> {
        let supports_timestretch = self.supports_timestretch();
        let mut created = Vec::new();
        for clip in indices.iter().map(|&i| &clips[i]) {
            let Some(modification) = self.modifications.iter().find(|m| m.persistent_id == clip.modification_id) else {
                continue;
            };
            let p = &clip.placement;
            let Some(region_ref) = self.controller.create_playback_region(
                modification.modification_ref,
                std::ptr::null_mut(),
                self.region_sequence,
                p.start_in_modification_seconds,
                p.duration_in_modification_seconds,
                p.start_in_playback_seconds,
                p.duration_in_playback_seconds,
                p.time_stretch && supports_timestretch,
            )
            .filter(|r| !r.is_null()) else {
                tracing::warn!(key = %clip.region_key, "ARA: plug-in returned null playback region");
                continue;
            };
            created.push(clip.region_key.clone());
            self.regions.push(OwnedRegion {
                key: clip.region_key.clone(),
                modification_id: clip.modification_id.clone(),
                region_ref,
            });
        }
        created
    }

    /// Re-state one region's placement (inside an editing cycle). Unknown keys are ignored.
    fn update_region(&self, key: &str, p: &common::protocol::AraRegionPlacement) {
        let Some(owned) = self.regions.iter().find(|r| r.key == key) else {
            return;
        };
        self.controller.update_playback_region_properties(
            owned.region_ref,
            self.region_sequence,
            p.start_in_modification_seconds,
            p.duration_in_modification_seconds,
            p.start_in_playback_seconds,
            p.duration_in_playback_seconds,
            p.time_stretch && self.supports_timestretch(),
        );
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
        if self.regions.is_empty() {
            return;
        }
        let region_refs: Vec<ARAPlaybackRegionRef> = self.regions.iter().map(|o| o.region_ref).collect();
        self.extension.notify_selection(&region_refs, &[self.region_sequence]);
    }

    /// Serialise the plug-in's ARA edit state for project save.
    pub fn store_archive(&self) -> Option<Vec<u8>> {
        self.controller.store_objects_to_archive()
    }

    /// Update the placement / stretch of already-present regions in place,
    /// matched by `region_key`, without rebuilding the document. Safe while
    /// the plug-in renders: `updatePlaybackRegionProperties` only re-states
    /// region properties (not renderer assignment) and is bracketed in
    /// begin/endEditing, which the plug-in uses for render-thread sync. Keys not
    /// currently present are ignored (a clip-set change goes through
    /// [`Self::set_clips`] instead).
    pub fn update_regions(&self, updates: &[AraRegionUpdate]) {
        if updates.is_empty() {
            return;
        }
        self.controller.begin_editing();
        for upd in updates {
            self.update_region(&upd.region_key, &upd.placement);
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

/// Create the audio source (decoding its WAV) for a clip. Inside an editing cycle.
unsafe fn build_source(controller: &AraDocumentController, clip: &AraClipSpec) -> Result<OwnedSource> {
    let host = Box::new(AraAudioSourceHost::from_audio_file(&clip.source_wav)?);

    // The boxed host's address is the opaque host ref the plug-in passes back.
    let host_ref: ARAAudioSourceHostRef = (std::ptr::from_ref::<AraAudioSourceHost>(&host)) as _;

    let source_id = CString::new(clip.source_id.as_str()).context("source id has interior NUL")?;
    let sample_count = i64::try_from(host.frame_count).unwrap_or(i64::MAX);
    let channel_count = i32::try_from(host.channel_count).unwrap_or(0);
    let sample_rate = host.sample_rate;

    let source_ref = controller
        .create_audio_source(host_ref, source_id.as_c_str(), sample_count, sample_rate, channel_count, false)
        .filter(|r| !r.is_null())
        .context("plug-in returned null audio source")?;

    Ok(OwnedSource { persistent_id: clip.source_id.clone(), wav: clip.source_wav.clone(), _host: host, source_ref })
}

impl Drop for AraSession {
    fn drop(&mut self) {
        // Detach regions from the renderer before destroying them, then tear the
        // model graph down bottom-up. The controller (and ARA uninitialise) is
        // released afterwards when the `controller` field drops.
        for owned in &self.regions {
            self.extension.remove_playback_region(owned.region_ref);
        }
        self.controller.begin_editing();
        for owned in &self.regions {
            self.controller.destroy_playback_region(owned.region_ref);
        }
        for owned in &self.modifications {
            self.controller.destroy_audio_modification(owned.modification_ref);
        }
        for owned in &self.sources {
            self.controller.destroy_audio_source(owned.source_ref);
        }
        self.controller.destroy_region_sequence(self.region_sequence);
        self.controller.destroy_musical_context(self.musical_context);
        self.controller.end_editing();
    }
}
