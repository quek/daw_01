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
//! modification the song drops leaves a partial archive of its state behind (handed
//! to the host's [`crate::ara::states::AraStates`]), and a created one starts from
//! the state the host resolved for it ([`Start`]: the saved archive, a clone of a
//! modification in this document, or a partial archive kept elsewhere).
//!
//! On drop the graph is torn down bottom-up before the document controller is
//! destroyed and ARA is uninitialised.

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use ara_sys::{
    ARAAudioModificationRef, ARAAudioSourceHostRef, ARAAudioSourceRef, ARADocumentControllerRef,
    ARAFactory, ARAMusicalContextHostRef, ARAMusicalContextRef, ARAPlaybackRegionRef,
    ARAPlaybackTransformationFlags, ARAPlugInExtensionInstance, ARARegionSequenceRef,
    kARAPlaybackTransformationTimestretch,
};
use common::ara_ids::{AraArchiveEntry, archived_id};
use common::protocol::{AraArchive, AraClipSpec, AraRegionUpdate};

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

/// The saved ARA archive handed to [`AraSession::set_clips`]: its bytes and its
/// table of contents (the objects it holds, with the ids they are written under).
#[derive(Clone, Copy)]
pub struct SavedArchive<'a> {
    pub bytes: &'a [u8],
    pub ids: &'a [AraArchiveEntry],
}

/// A partial archive kept outside this edit (another document's live state, a destroyed modification's
/// state, a closed document, the clipboard's copy), holding an object's state under `archived`.
#[derive(Debug, Clone)]
pub struct KeptArchive {
    pub bytes: Arc<[u8]>,
    pub archived: String,
}

/// Where a created audio modification's state comes from, as resolved by the host
/// ([`crate::ara::graph_plan::modification_start`], materialised by
/// [`crate::ara::states::AraStates::resolve_starts`]). A modification with no entry starts empty.
#[derive(Debug, Clone)]
pub enum Start {
    /// The saved archive, the state written under this id.
    Saved(String),
    /// `cloneAudioModification` of this modification, which was in the document before the edit and survives it.
    Clone(String),
    /// A partial archive kept outside this edit.
    Restore(KeptArchive),
}

/// How the objects an edit creates start ([`crate::ara::states::AraStates::resolve_starts`]).
#[derive(Debug, Default)]
pub struct Starts {
    /// Per created audio modification id.
    pub modifications: HashMap<String, Start>,
    /// Per created audio source id the saved archive does not hold: a partial archive holding its state (it
    /// travels with the modifications copied onto it from another document). A source with no entry is restored
    /// from the saved archive if that holds it, else starts unanalysed.
    pub sources: HashMap<String, KeptArchive>,
}

/// One partial archive of audio modifications together with their audio sources' state
/// ([`AraSession::store_modifications`]): the bytes and the `(modification id, source id)` it holds.
pub type ModificationsArchive = (Vec<u8>, Vec<(String, String)>);

/// The state of the audio modifications an edit destroyed, taken just before destroying them into one partial
/// archive (ARAInterface.h "Partial Document Persistency"), together with the state of the audio sources destroyed
/// with them (so a copy into another document still has the source's state once this document no longer holds it).
/// `modifications` = `(modification id, its source's id, whether the archive holds the source's state)`.
#[derive(Debug, Default)]
pub struct Retired {
    pub bytes: Vec<u8>,
    pub modifications: Vec<(String, String, bool)>,
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
    /// The saved archive, the state written under this id.
    Saved(String),
    /// A partial archive kept outside this edit.
    Kept(KeptArchive),
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
    /// document data, every object it lists); afterwards it is only the
    /// fallback for objects created later.
    built: bool,
    /// The archive format this document writes and the formats it can import
    /// ([`AraDocumentController::archive_formats`]).
    archive_format: String,
    importable_formats: Vec<String>,
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

        let (archive_format, importable_formats) = controller.archive_formats();
        Ok(Self {
            musical_context_host,
            musical_context,
            region_sequence,
            sources: Vec::new(),
            modifications: Vec::new(),
            regions: Vec::new(),
            built: false,
            archive_format,
            importable_formats,
            extension,
            controller,
            supported_transformation_flags,
        })
    }

    /// The archive format this document writes (a kept state records it, [`Self::can_import`] checks it).
    pub fn archive_format(&self) -> &str {
        &self.archive_format
    }

    /// Whether a state written in archive format `format` may be restored into this document (ARAInterface.h
    /// `documentArchiveID` / `compatibleDocumentArchiveIDs`). A state from another plug-in is never restored.
    pub fn can_import(&self, format: &str) -> bool {
        !format.is_empty() && (format == self.archive_format || self.importable_formats.iter().any(|f| f == format))
    }

    /// The next [`Self::set_clips`] builds the graph for the first time (the saved archive is the document's state).
    pub fn is_first_build(&self) -> bool {
        !self.built
    }

    /// The graph edit [`Self::set_clips`] would make for `clips`.
    pub fn plan(&self, clips: &[AraClipSpec]) -> graph_plan::GraphPlan {
        graph_plan::plan(&self.graph_now(), clips)
    }

    /// Whether the audio modification `id` is in the document now.
    pub fn has_modification(&self, id: &str) -> bool {
        self.modifications.iter().any(|m| m.persistent_id == id)
    }

    /// Whether the audio source `id` is in the document now.
    pub fn has_source(&self, id: &str) -> bool {
        self.sources.iter().any(|s| s.persistent_id == id)
    }

    /// A partial archive of the audio modification `id`'s state together with its audio source's state (for a
    /// copy into another document), and the source's id. `None` if it is not in the document or the store failed.
    /// Outside an editing cycle.
    pub fn store_modification(&self, id: &str) -> Option<(Vec<u8>, String)> {
        let (bytes, mut held) = self.store_modifications(&HashSet::from([id]))?;
        Some((bytes, held.pop()?.1))
    }

    /// One partial archive of the audio modifications `ids` (the ones in the document) with their audio sources'
    /// state, and the `(modification id, source id)` it holds. `None` if none is in the document or the store
    /// failed. Outside an editing cycle.
    pub fn store_modifications(&self, ids: &HashSet<&str>) -> Option<ModificationsArchive> {
        let held: Vec<&OwnedModification> =
            self.modifications.iter().filter(|m| ids.contains(m.persistent_id.as_str())).collect();
        if held.is_empty() {
            return None;
        }
        let modification_refs: Vec<ARAAudioModificationRef> = held.iter().map(|m| m.modification_ref).collect();
        let source_refs = self.source_refs(held.iter().map(|m| m.source_id.as_str()));
        let bytes = self.controller.store_partial_archive(&source_refs, &modification_refs)?;
        Some((bytes, held.iter().map(|m| (m.persistent_id.clone(), m.source_id.clone())).collect()))
    }

    /// A partial archive of only the audio source `id`'s state. `None` if it is not in the document or the store
    /// failed. Outside an editing cycle.
    pub fn store_source(&self, id: &str) -> Option<Vec<u8>> {
        let source = self.sources.iter().find(|s| s.persistent_id == id)?;
        self.controller.store_partial_archive(&[source.source_ref], &[])
    }

    /// The refs of the audio sources `ids` in the document (each once).
    fn source_refs<'a>(&self, ids: impl Iterator<Item = &'a str>) -> Vec<ARAAudioSourceRef> {
        let ids: HashSet<&str> = ids.collect();
        self.sources.iter().filter(|s| ids.contains(s.persistent_id.as_str())).map(|s| s.source_ref).collect()
    }

    /// Edit the document's model graph in place to match `clips`
    /// ([`graph_plan::plan`]): regions / modifications / sources whose ids are gone
    /// are removed bottom-up, new ones created top-down, surviving regions get
    /// their placement updated. Surviving modifications keep the plug-in's live
    /// edits (a split adds a region on the same modification); a destroyed one
    /// leaves its state behind first (the returned [`Retired`]). Only **the objects
    /// created here** are restored ([`Self::restore_created`]), inside the same
    /// editing cycle, as ARA's unarchiving session prescribes, each created
    /// object from its start in `starts`. Best-effort: a clip whose
    /// source can't be decoded is skipped (logged) so one bad source doesn't drop
    /// the rest.
    ///
    /// The caller must ensure the plug-in is **inactive** — ARA's
    /// `addPlaybackRegion` / `removePlaybackRegion` (and detaching regions before
    /// destroying them) require it.
    pub fn set_clips(
        &mut self,
        clips: &[AraClipSpec],
        bpm: f64,
        time_sig: (u16, u16),
        archive: Option<SavedArchive<'_>>,
        starts: &Starts,
    ) -> Retired {
        let plan = graph_plan::plan(&self.graph_now(), clips);
        let first_build = !self.built;
        // ARA stores archives only outside an editing session, so keep the state of the
        // modifications about to go (and of clone origins a plug-in without
        // `cloneAudioModification` must copy through an archive) before the edit begins.
        let retired = self.retire_modifications(&plan);
        let clone_copies = self.clone_copies(&starts.modifications);
        for owned in self.regions.iter().filter(|r| plan.remove_regions.contains(&r.key)) {
            self.extension.remove_playback_region(owned.region_ref);
        }
        self.controller.begin_editing();
        self.update_musical_context(bpm, time_sig);
        self.destroy_planned(&plan);

        let created_sources = self.create_sources(clips, &plan.create_sources);
        let restores = self.create_modifications(clips, &plan.create_modifications, &starts.modifications, &clone_copies);
        let created_regions = self.create_regions(clips, &plan.create_regions);
        for &i in &plan.update_regions {
            self.update_region(&clips[i].region_key, &clips[i].placement);
        }
        // The edit that first builds the graph (with at least one modification) restores the saved archive's
        // document data; an edit that creates no modification leaves that to the next one.
        let building = first_build && !self.modifications.is_empty();
        self.restore_created(archive, building, &created_sources, &restores, &starts.sources);
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
        retired
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

    /// The plug-in state of the audio modifications `plan` destroys, in one partial archive with the audio sources it
    /// destroys along with them ([`Retired`]). Outside an editing cycle.
    fn retire_modifications(&self, plan: &graph_plan::GraphPlan) -> Retired {
        let doomed: Vec<&OwnedModification> =
            self.modifications.iter().filter(|m| plan.destroy_modifications.contains(&m.persistent_id)).collect();
        if doomed.is_empty() {
            return Retired::default();
        }
        let with_source = |m: &OwnedModification| plan.destroy_sources.contains(&m.source_id);
        let modification_refs: Vec<ARAAudioModificationRef> = doomed.iter().map(|m| m.modification_ref).collect();
        let source_refs = self.source_refs(doomed.iter().filter(|m| with_source(m)).map(|m| m.source_id.as_str()));
        let Some(bytes) = self.controller.store_partial_archive(&source_refs, &modification_refs) else {
            tracing::warn!(n = doomed.len(), "ARA: storing the destroyed modifications' state failed; undo / copies start without it");
            return Retired::default();
        };
        let modifications = doomed.iter().map(|m| (m.persistent_id.clone(), m.source_id.clone(), with_source(m))).collect();
        Retired { bytes, modifications }
    }

    /// For a plug-in without `cloneAudioModification`: a partial archive of each clone origin in `starts` (ARA:
    /// "hosts can achieve the same effect by creating an archive of the modification that should be cloned and
    /// unarchiving that state into a new modification"). The origin stays on the same audio source in this
    /// document, so only the modification is archived. Outside an editing cycle.
    fn clone_copies(&self, starts: &HashMap<String, Start>) -> HashMap<String, Arc<[u8]>> {
        if self.controller.can_clone_audio_modification() {
            return HashMap::new();
        }
        let origins: HashSet<&str> =
            starts.values().filter_map(|s| if let Start::Clone(origin) = s { Some(origin.as_str()) } else { None }).collect();
        origins
            .into_iter()
            .filter_map(|o| {
                let m = self.modifications.iter().find(|m| m.persistent_id == o)?;
                Some((o.to_owned(), Arc::from(self.controller.store_partial_archive(&[], &[m.modification_ref])?)))
            })
            .collect()
    }

    /// Create the audio modifications for `clips[i]` on their (existing or just created) sources, each from its
    /// [`Start`] (a clone of a modification that survives the edit, or created empty), and return which of them
    /// to restore and from where: `(modification id, state to restore)`. `clone_copies` = the archived clone
    /// origins for a plug-in that cannot clone ([`Self::clone_copies`]).
    fn create_modifications(
        &mut self,
        clips: &[AraClipSpec],
        indices: &[usize],
        starts: &HashMap<String, Start>,
        clone_copies: &HashMap<String, Arc<[u8]>>,
    ) -> Vec<(String, RestoreFrom)> {
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
            let (modification_ref, restore_from) = self.start_modification(clip, source_ref, &id, starts.get(&clip.modification_id), clone_copies);
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

    /// Create the audio modification `id` for `clip` from `start`: the plug-in's ref, and the state to restore into
    /// it (`None` = nothing to restore: cloned, or no state anywhere). Inside an editing cycle.
    fn start_modification(
        &self,
        clip: &AraClipSpec,
        source_ref: ARAAudioSourceRef,
        id: &CStr,
        start: Option<&Start>,
        clone_copies: &HashMap<String, Arc<[u8]>>,
    ) -> (Option<ARAAudioModificationRef>, Option<RestoreFrom>) {
        let from = match start {
            None => None,
            Some(Start::Clone(origin)) => {
                if let Some(bytes) = clone_copies.get(origin) {
                    Some(RestoreFrom::Kept(KeptArchive { bytes: Arc::clone(bytes), archived: origin.clone() }))
                } else {
                    // The origin survives the edit, on the same audio source (its id names the source).
                    let original = self.modifications.iter().find(|m| m.persistent_id == *origin && m.source_id == clip.source_id);
                    let cloned = original.and_then(|o| self.controller.clone_audio_modification(o.modification_ref, std::ptr::null_mut(), id));
                    if cloned.is_some() {
                        return (cloned, None);
                    }
                    tracing::warn!(id = %clip.modification_id, %origin, "ARA: cloning the audio modification failed; starting empty");
                    None
                }
            }
            Some(Start::Saved(archived)) => Some(RestoreFrom::Saved(archived.clone())),
            Some(Start::Restore(kept)) => Some(RestoreFrom::Kept(kept.clone())),
        };
        (self.controller.create_audio_modification(source_ref, std::ptr::null_mut(), id), from)
    }

    /// Restore the state of the objects this edit created (inside its editing cycle): the created audio sources
    /// the saved `archive` lists or `source_starts` carries, and each created modification from its
    /// [`RestoreFrom`], one call per archive in the order [`graph_plan::restore_calls`] derives from ARA's partial
    /// persistency rules. `building` = the edit that first builds the graph, which also restores the saved
    /// archive's document data (last).
    fn restore_created(
        &self,
        archive: Option<SavedArchive<'_>>,
        building: bool,
        created_sources: &[String],
        restores: &[(String, RestoreFrom)],
        source_starts: &HashMap<String, KeptArchive>,
    ) {
        /// The index of `bytes` among the edit's kept archives (the same archive once).
        fn index_of<'b>(kept: &mut Vec<&'b Arc<[u8]>>, bytes: &'b Arc<[u8]>) -> usize {
            kept.iter().position(|k| Arc::ptr_eq(k, bytes)).unwrap_or_else(|| {
                kept.push(bytes);
                kept.len() - 1
            })
        }
        let saved = archive.filter(|a| !a.bytes.is_empty());
        let mut kept: Vec<&Arc<[u8]>> = Vec::new();
        let mut plan = graph_plan::Restores::default();
        for id in created_sources {
            if let Some(archived) = saved.and_then(|s| archived_id(s.ids, id)) {
                plan.sources.push((None, archived, id));
            } else if let Some(start) = source_starts.get(id) {
                plan.sources.push((Some(index_of(&mut kept, &start.bytes)), &start.archived, id));
            }
        }
        for (id, from) in restores {
            let Some(source) = self.modifications.iter().find(|m| m.persistent_id == *id).map(|m| m.source_id.as_str()) else {
                continue;
            };
            match from {
                RestoreFrom::Saved(archived) if saved.is_some() => plan.modifications.push((None, archived, id, source)),
                RestoreFrom::Saved(_) => {}
                RestoreFrom::Kept(k) => plan.modifications.push((Some(index_of(&mut kept, &k.bytes)), &k.archived, id, source)),
            }
        }
        for call in graph_plan::restore_calls(&plan, building && saved.is_some()) {
            let bytes: &[u8] = match (call.archive, saved) {
                (Some(i), _) => &kept[i][..],
                (None, Some(saved)) => saved.bytes,
                (None, None) => continue,
            };
            if !self.controller.restore_objects_from_archive(bytes, call.document_data, &call.sources, &call.modifications) {
                tracing::warn!(
                    saved = call.archive.is_none(),
                    document_data = call.document_data,
                    n_sources = call.sources.len(),
                    n_modifications = call.modifications.len(),
                    "ARA: restoring an archive failed"
                );
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

    /// Serialise the plug-in's ARA edit state for project save, with its table of contents (the persistent ids of
    /// every audio source and audio modification in the document, all written under their current ids).
    pub fn store_archive(&self) -> Option<AraArchive> {
        let bytes = self.controller.store_objects_to_archive()?;
        let sources = self.sources.iter().map(|s| s.persistent_id.clone());
        let ids = sources.chain(self.modifications.iter().map(|m| m.persistent_id.clone())).collect();
        Some(AraArchive { bytes, ids })
    }

    /// The `(persistent id, audio source's persistent id)` of the audio modifications in the document.
    pub fn modifications(&self) -> impl Iterator<Item = (&str, &str)> {
        self.modifications.iter().map(|m| (m.persistent_id.as_str(), m.source_id.as_str()))
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
