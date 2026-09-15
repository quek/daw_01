//! plug-in host 全体の **ARA の状態の置き場** — document (= ARA device 1 つ) の外に取っておく audio modification
//! の状態と、document を組むときに作る modification をどこから始めるかの解決 (r.md #132 残件、ARA のコピー)。
//!
//! Melodyne の編集は plug-in host の中の document にしか無い。 クリップを別のトラック (別の device の document) へ
//! 写す / 移す、別のタブへ貼る、元を消した後に貼る、とき、写した先の modification は **ほかの document の今の状態、
//! 取っておいた状態、クリップボードの写し** から始める (`graph_plan::modification_start` が順番を決める)。 ほかの
//! document から写すのは partial archive (ARAInterface.h "Partial Document Persistency": "copying and pasting audio
//! source and audio modification state between songs"、`ARAStoreObjectsFilter::documentData` は "kARAFalse if the
//! archive is intended for copy/paste or other means of data import/export between documents")。
//!
//! 取っておく状態:
//! - **destroy した modification** — その直前の partial archive (`AraSession::set_clips` が返す、undo / redo・移動)。
//! - **畳んだ document** — device を降ろす / 差し替える / ARA document を消す直前の、document 全体のアーカイブ
//!   (トラックの無効化・削除の後に、そこに居たクリップを別のトラックへ写す / 移す)。 プロジェクトを閉じる (タブを閉じる /
//!   同じタブへ別のプロジェクトを開く) ときは取らず、取っておいた分も捨てる (もう引かれない)。
//! - **クリップボード** — 写した時点の modification の partial archive (元のプロジェクトを閉じた後に貼る)。
//!
//! 別の document へ写す modification は、その audio source の状態と一緒に運ぶ: 写す先の document に source の状態が
//! まだ無い (この編集で作り、保存したアーカイブにも無い) なら、source の状態も restore する (ARAInterface.h
//! "Restoring an audio modification without restoring its underlying audio source may not succeed if the audio source
//! state has changed since storing the audio modification"。 `resolve_starts` の source の決め方)。
//!
//! 状態は書いた plug-in の形式 (`documentArchiveID`) と一緒に持ち、読める形式の document にだけ restore する
//! (別の plug-in の状態は写さない: ARAInterface.h `documentArchiveID` "shared only amongst document controllers
//! that create the same archives")。 プロジェクトの中の id は安定 id なので同じ id は同じ take の状態で、
//! 最新だけを持つ。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use common::ara_ids::{AraArchiveEntry, archived_id, modification_source_id};
use common::protocol::{AraArchive, AraClipSpec, DeviceAddr, ProjectKey};

use crate::ara::graph_plan::{self, DocumentNow, GraphPlan, KeptAt, KeptStates, ModificationStart};
use crate::ara::session::{AraSession, KeptArchive, ModificationsArchive, Retired, Start, Starts};

/// 状態の置き場から見た document 1 つ ([`AraSession`] が実装する。 解決の規則を plug-in 無しで確かめるための面)。
pub trait Document {
    /// この document が書くアーカイブの形式。
    fn archive_format(&self) -> &str;
    /// 形式 `format` の状態をこの document へ restore してよいか。
    fn can_import(&self, format: &str) -> bool;
    /// 次の編集が document を初めて組むか。
    fn is_first_build(&self) -> bool;
    /// `clips` に合わせる編集。
    fn plan(&self, clips: &[AraClipSpec]) -> GraphPlan;
    /// 今 document に居る audio modification の `(id, audio source の id)`。
    fn modifications(&self) -> Vec<(&str, &str)>;
    /// audio modification `id` が今 document に居るか。
    fn has_modification(&self, id: &str) -> bool;
    /// audio source `id` が今 document に居るか。
    fn has_source(&self, id: &str) -> bool;
    /// modification `id` をその audio source と一緒にした partial archive と、source の id。
    fn store_modification(&self, id: &str) -> Option<(Vec<u8>, String)>;
    /// modification `ids` のうち居るものを audio source と一緒にした partial archive と、中の `(modification, source)`。
    fn store_modifications(&self, ids: &HashSet<&str>) -> Option<ModificationsArchive>;
    /// audio source `id` だけの partial archive。
    fn store_source(&self, id: &str) -> Option<Vec<u8>>;
    /// document 全体のアーカイブと目次。
    fn store_archive(&self) -> Option<AraArchive>;
}

impl Document for AraSession {
    fn archive_format(&self) -> &str {
        AraSession::archive_format(self)
    }
    fn can_import(&self, format: &str) -> bool {
        AraSession::can_import(self, format)
    }
    fn is_first_build(&self) -> bool {
        AraSession::is_first_build(self)
    }
    fn plan(&self, clips: &[AraClipSpec]) -> GraphPlan {
        AraSession::plan(self, clips)
    }
    fn modifications(&self) -> Vec<(&str, &str)> {
        AraSession::modifications(self).collect()
    }
    fn has_modification(&self, id: &str) -> bool {
        AraSession::has_modification(self, id)
    }
    fn has_source(&self, id: &str) -> bool {
        AraSession::has_source(self, id)
    }
    fn store_modification(&self, id: &str) -> Option<(Vec<u8>, String)> {
        AraSession::store_modification(self, id)
    }
    fn store_modifications(&self, ids: &HashSet<&str>) -> Option<ModificationsArchive> {
        AraSession::store_modifications(self, ids)
    }
    fn store_source(&self, id: &str) -> Option<Vec<u8>> {
        AraSession::store_source(self, id)
    }
    fn store_archive(&self) -> Option<AraArchive> {
        AraSession::store_archive(self)
    }
}

/// 取っておいた状態を restore してよい document: 書いた plug-in のアーカイブの形式が読めるもの、形式が分からない
/// (plug-in を読み込んでいない device の保存したアーカイブ) なら同じ plug-in の device のもの。
#[derive(Clone)]
enum KeptFormat {
    Archive(Arc<str>),
    Plugin(Arc<str>),
}

/// 取っておいた 1 つの modification の状態: `bytes` がその状態を `archived` の id で持ち、`source_bytes` が modification の
/// audio source の状態を `source` の id で持つ (取った時点で手に入ったとき。 modification と同じアーカイブのことも
/// ある。 無ければ `source` は元のプロジェクトでの source の id)。
#[derive(Clone)]
struct KeptState {
    format: KeptFormat,
    bytes: Arc<[u8]>,
    archived: String,
    source: String,
    source_bytes: Option<Arc<[u8]>>,
}

impl KeptState {
    /// 今の id のまま書いた modification `id` と source `source` を 1 つのアーカイブに持つ状態。
    fn with_source(format: &Arc<str>, bytes: &Arc<[u8]>, id: &str, source: String) -> Self {
        Self {
            format: KeptFormat::Archive(Arc::clone(format)),
            bytes: Arc::clone(bytes),
            archived: id.to_owned(),
            source,
            source_bytes: Some(Arc::clone(bytes)),
        }
    }
}

/// 作る modification へ restore する、document の外の状態 ([`KeptState`])。 `project` = 元の modification が居た
/// プロジェクト (source の状態をアーカイブに持たないとき、そのプロジェクトの生きている document から引く)。
#[derive(Clone)]
struct Carried {
    state: KeptState,
    project: Option<ProjectKey>,
}

/// クリップボードへ写した時点の状態 (最後に写したものだけ)。
struct ClipboardStates {
    /// 写した元のプロジェクト (`Song::project_id`)。
    project_id: u64,
    states: HashMap<String, KeptState>,
}

/// plug-in host に document の無い device の保存したアーカイブ (`PluginCommand::KeepDormantAraArchive`)。 形式は
/// plug-in を読み込むまで分からないので、同じ `plugin_id` の document にだけ restore する。
struct DormantArchive {
    plugin_id: Arc<str>,
    bytes: Arc<[u8]>,
    ids: Vec<AraArchiveEntry>,
}

impl DormantArchive {
    /// modification `id` (今の id) の状態。 目次で書かれている id へ読み替え、source は modification の id の形から引く。
    fn state(&self, id: &str) -> Option<KeptState> {
        let archived = archived_id(&self.ids, id)?.to_owned();
        let source = modification_source_id(id).unwrap_or_default();
        let archived_source = archived_id(&self.ids, &source).map(str::to_owned);
        Some(KeptState {
            format: KeptFormat::Plugin(Arc::clone(&self.plugin_id)),
            bytes: Arc::clone(&self.bytes),
            archived,
            source_bytes: archived_source.as_ref().map(|_| Arc::clone(&self.bytes)),
            source: archived_source.unwrap_or(source),
        })
    }
}

/// plug-in host に居る document (device 順 = 引き当ての順序)。
pub type Sessions<'a, D> = [(DeviceAddr, &'a D)];

#[derive(Default)]
pub struct AraStates {
    /// プロジェクトごとの、document の外に取っておいた modification の状態 (id ごとに最新)。
    retired: HashMap<ProjectKey, HashMap<String, KeptState>>,
    clipboard: Option<ClipboardStates>,
    /// document の無い device の保存したアーカイブ (device 順)。
    dormant: std::collections::BTreeMap<DeviceAddr, DormantArchive>,
}

impl AraStates {
    /// `project` の document (形式 `format`) が destroy した modification の状態を取っておく。
    pub fn retire(&mut self, project: ProjectKey, format: &str, retired: Retired) {
        if retired.modifications.is_empty() {
            return;
        }
        let (format, bytes): (Arc<str>, Arc<[u8]>) = (Arc::from(format), Arc::from(retired.bytes));
        let kept = self.retired.entry(project).or_default();
        for (id, source, source_in_archive) in retired.modifications {
            let mut state = KeptState::with_source(&format, &bytes, &id, source);
            if !source_in_archive {
                state.source_bytes = None;
            }
            kept.insert(id, state);
        }
    }

    /// 畳む document `doc` の状態 (document 全体のアーカイブ) を、中の modification ごとに取っておく。
    pub fn keep_document(&mut self, project: ProjectKey, doc: &impl Document) {
        let sources: HashMap<String, String> =
            doc.modifications().into_iter().map(|(m, s)| (m.to_owned(), s.to_owned())).collect();
        if sources.is_empty() {
            return;
        }
        let Some(archive) = doc.store_archive() else {
            tracing::warn!(?project, "ARA: storing a closing document's archive failed; its edits cannot be copied later");
            return;
        };
        let (format, bytes): (Arc<str>, Arc<[u8]>) = (Arc::from(doc.archive_format()), Arc::from(archive.bytes));
        let kept = self.retired.entry(project).or_default();
        for id in archive.ids {
            if let Some(source) = sources.get(&id).cloned() {
                let state = KeptState::with_source(&format, &bytes, &id, source);
                kept.insert(id, state);
            }
        }
    }

    /// プロジェクト `project` を閉じる: 取っておいた状態を捨てる。 同じ `ProjectKey` のタブに別のプロジェクトを開けば、
    /// そのプロジェクトの状態をまた取っておく (クリップボードの写しは `Song::project_id` で引くので残す)。
    pub fn close_project(&mut self, project: ProjectKey) {
        self.retired.remove(&project);
        self.dormant.retain(|addr, _| addr.project != project);
    }

    /// document の無い device `device` の保存したアーカイブ (目次 `ids`) を預かる (同じ device の前の分は捨てる)。
    pub fn keep_dormant(&mut self, device: DeviceAddr, plugin_id: &str, bytes: Vec<u8>, ids: Vec<AraArchiveEntry>) {
        self.dormant.insert(device, DormantArchive { plugin_id: Arc::from(plugin_id), bytes: Arc::from(bytes), ids });
    }

    /// `device` の document を組むので、預かっていた保存したアーカイブを捨てる (以後は生きている document が引かれる)。
    pub fn forget_dormant(&mut self, device: DeviceAddr) {
        self.dormant.remove(&device);
    }

    /// クリップボードへ写した modification `ids` (プロジェクト `project`、`Song::project_id` は `project_id`) の今の
    /// 状態を取っておく (前の写しは捨てる)。 生きている document に居ればその partial archive (source と一緒)、居なければ
    /// 取っておいた状態、それも無ければ document の無い device の保存したアーカイブ。 取っておいた状態が source の状態を
    /// 持たなければ、source が居る生きている document からここで取る (元のプロジェクトを閉じた後は引けない)。
    pub fn snapshot_clipboard<D: Document>(&mut self, docs: &Sessions<'_, D>, project: ProjectKey, project_id: u64, ids: &[String]) {
        let mut wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let mut states = HashMap::new();
        let in_project = || docs.iter().filter(|(addr, _)| addr.project == project).map(|(_, doc)| *doc);
        for doc in in_project() {
            let Some((bytes, held)) = doc.store_modifications(&wanted) else {
                continue;
            };
            let (format, bytes): (Arc<str>, Arc<[u8]>) = (Arc::from(doc.archive_format()), Arc::from(bytes));
            for (id, source) in held {
                wanted.remove(id.as_str());
                let state = KeptState::with_source(&format, &bytes, &id, source);
                states.insert(id, state);
            }
        }
        for id in wanted {
            let retired = self.retired.get(&project).and_then(|kept| kept.get(id)).cloned();
            let dormant = || self.dormant.iter().filter(|(addr, _)| addr.project == project).find_map(|(_, d)| d.state(id));
            let Some(mut state) = retired.or_else(dormant) else {
                continue;
            };
            if state.source_bytes.is_none()
                && let KeptFormat::Archive(format) = &state.format
            {
                let holder = in_project().find(|doc| *doc.archive_format() == **format && doc.has_source(&state.source));
                state.source_bytes = holder.and_then(|doc| doc.store_source(&state.source)).map(Arc::from);
            }
            states.insert(id.to_owned(), state);
        }
        self.clipboard = Some(ClipboardStates { project_id, states });
    }

    /// `device` の document `doc` (plug-in `plugin_id`) を `clips` に合わせる編集で作る object の、始め方 ([`Starts`])。
    /// `saved` = document の保存したアーカイブの目次 (アーカイブが無ければ空)。 modification の決め方は
    /// [`graph_plan::modification_start`]、ほかの document の今の状態はここで partial archive にする (その document は
    /// 編集中ではない)。 audio source は [`Self::source_starts`]。
    pub fn resolve_starts<D: Document>(
        &self,
        device: DeviceAddr,
        plugin_id: &str,
        doc: &D,
        docs: &Sessions<'_, D>,
        clips: &[AraClipSpec],
        saved: &[AraArchiveEntry],
    ) -> Starts {
        let plan = doc.plan(clips);
        let surviving: HashSet<String> = doc
            .modifications()
            .into_iter()
            .filter(|(id, _)| !plan.destroy_modifications.contains(*id))
            .map(|(id, _)| id.to_owned())
            .collect();
        let now = DocumentNow { project: device.project, first_build: doc.is_first_build(), saved, surviving: &surviving };
        let lookup = Lookup { states: self, docs, device, plugin_id, target: doc };
        let mut live_copies: HashMap<(DeviceAddr, String), Option<Carried>> = HashMap::new();
        let mut carried: HashMap<&str, Carried> = HashMap::new();
        let mut starts = Starts::default();
        for spec in plan.create_modifications.iter().map(|&i| &clips[i]) {
            let start = match graph_plan::modification_start(spec, now, &lookup) {
                ModificationStart::Saved(archived) => Some(Start::Saved(archived)),
                ModificationStart::Clone(origin) => Some(Start::Clone(origin)),
                ModificationStart::Kept { at, id } => self.kept_state(at, &id, docs, &mut live_copies).map(|carry| {
                    let restore = KeptArchive { bytes: Arc::clone(&carry.state.bytes), archived: carry.state.archived.clone() };
                    carried.insert(spec.modification_id.as_str(), carry);
                    Start::Restore(restore)
                }),
                ModificationStart::Empty => None,
            };
            if let Some(start) = start {
                starts.modifications.insert(spec.modification_id.clone(), start);
            }
        }
        starts.sources = Self::source_starts(&lookup, &plan, clips, saved, &carried);
        starts
    }

    /// この編集で作る audio source のうち、保存したアーカイブに無い (状態がまだどこにも無い) source の状態の restore 元。
    /// その source に作る modification が **全部** document の外の状態から始まるときだけ (ARAInterface.h: "When
    /// restoring the state of an audio source, the state of all audio modifications associated with the audio source must
    /// be restored too"): そのうち source の状態を持つ最初のもの、無ければ元のプロジェクトで元の source が居る生きている
    /// document の source だけの partial archive (元の document に source が残って modification だけ destroy した)。
    fn source_starts<D: Document>(
        lookup: &Lookup<'_, D>,
        plan: &GraphPlan,
        clips: &[AraClipSpec],
        saved: &[AraArchiveEntry],
        carried: &HashMap<&str, Carried>,
    ) -> HashMap<String, KeptArchive> {
        let mut out = HashMap::new();
        for source in plan.create_sources.iter().map(|&i| clips[i].source_id.as_str()) {
            if archived_id(saved, source).is_some() {
                continue;
            }
            let on_source: Vec<Option<&Carried>> = plan
                .create_modifications
                .iter()
                .map(|&i| &clips[i])
                .filter(|spec| spec.source_id == source)
                .map(|spec| carried.get(spec.modification_id.as_str()))
                .collect();
            let Some(on_source) = on_source.into_iter().collect::<Option<Vec<&Carried>>>().filter(|c| !c.is_empty()) else {
                continue;
            };
            let from_copy = on_source.iter().find_map(|c| {
                let bytes = c.state.source_bytes.as_ref()?;
                Some(KeptArchive { bytes: Arc::clone(bytes), archived: c.state.source.clone() })
            });
            let from_live = || on_source.iter().find_map(|c| lookup.live_source(c.project?, &c.state.source));
            if let Some(start) = from_copy.or_else(from_live) {
                out.insert(source.to_owned(), start);
            }
        }
        out
    }

    /// `at` にある modification `id` の状態。 生きている document からは 1 回だけ partial archive にする
    /// (`live_copies`)。
    fn kept_state<D: Document>(
        &self,
        at: KeptAt,
        id: &str,
        docs: &Sessions<'_, D>,
        live_copies: &mut HashMap<(DeviceAddr, String), Option<Carried>>,
    ) -> Option<Carried> {
        let carry = |state: &KeptState, project| Carried { state: state.clone(), project };
        match at {
            KeptAt::Live(addr) => live_copies
                .entry((addr, id.to_owned()))
                .or_insert_with(|| {
                    let doc = docs.iter().find(|(a, _)| *a == addr).map(|(_, d)| *d)?;
                    let copied = doc.store_modification(id).map(|(bytes, source)| {
                        let (format, bytes): (Arc<str>, Arc<[u8]>) = (Arc::from(doc.archive_format()), Arc::from(bytes));
                        Carried { state: KeptState::with_source(&format, &bytes, id, source), project: Some(addr.project) }
                    });
                    if copied.is_none() {
                        tracing::warn!(?addr, %id, "ARA: storing another document's modification failed; the copy starts empty");
                    }
                    copied
                })
                .clone(),
            KeptAt::Retired(project) => self.retired.get(&project)?.get(id).map(|k| carry(k, Some(project))),
            KeptAt::Clipboard => self.clipboard.as_ref()?.states.get(id).map(|k| carry(k, None)),
            KeptAt::Dormant(addr) => self.dormant.get(&addr)?.state(id).map(|k| carry(&k, None)),
        }
    }
}

/// [`KeptStates`] を plug-in host の今から答える。 `target` = 状態を restore する document (読める形式だけを答える)、
/// `plugin_id` = その device の plug-in (形式の分からない保存したアーカイブは同じ plug-in のものだけを答える)。
struct Lookup<'a, D> {
    states: &'a AraStates,
    docs: &'a Sessions<'a, D>,
    device: DeviceAddr,
    plugin_id: &'a str,
    target: &'a D,
}

impl<D: Document> Lookup<'_, D> {
    /// プロジェクト `project` の生きている document のうち、読める形式で `has` を満たす最初のもの: この document (この
    /// 編集で消す object) を先に、残りは device 順。
    fn live_doc(&self, project: ProjectKey, has: impl Fn(&D) -> bool) -> Option<(DeviceAddr, &D)> {
        let own = self.docs.iter().filter(|(addr, _)| *addr == self.device);
        let others = self.docs.iter().filter(|(addr, _)| *addr != self.device);
        own.chain(others)
            .find(|(addr, doc)| addr.project == project && self.target.can_import(doc.archive_format()) && has(doc))
            .map(|(addr, doc)| (*addr, *doc))
    }

    /// プロジェクト `project` で audio source `id` が居る生きている document の、source だけの partial archive。
    fn live_source(&self, project: ProjectKey, id: &str) -> Option<KeptArchive> {
        let (addr, doc) = self.live_doc(project, |doc| doc.has_source(id))?;
        let Some(bytes) = doc.store_source(id) else {
            tracing::warn!(?addr, %id, "ARA: storing another document's audio source failed; the copy's source starts unanalysed");
            return None;
        };
        Some(KeptArchive { bytes: Arc::from(bytes), archived: id.to_owned() })
    }

    /// 取っておいた状態をこの document へ restore してよいか ([`KeptFormat`])。
    fn can_restore(&self, format: &KeptFormat) -> bool {
        match format {
            KeptFormat::Archive(format) => self.target.can_import(format),
            KeptFormat::Plugin(plugin_id) => **plugin_id == *self.plugin_id,
        }
    }
}

impl<D: Document> KeptStates for Lookup<'_, D> {
    fn in_project(&self, project: ProjectKey, id: &str) -> Option<KeptAt> {
        if let Some((addr, _)) = self.live_doc(project, |doc| doc.has_modification(id)) {
            return Some(KeptAt::Live(addr));
        }
        let kept = self.states.retired.get(&project)?.get(id)?;
        self.can_restore(&kept.format).then_some(KeptAt::Retired(project))
    }

    fn in_clipboard(&self, project_id: u64, id: &str) -> bool {
        self.states
            .clipboard
            .as_ref()
            .filter(|c| c.project_id == project_id)
            .and_then(|c| c.states.get(id))
            .is_some_and(|k| self.can_restore(&k.format))
    }

    fn in_dormant(&self, project: ProjectKey, id: &str) -> Option<KeptAt> {
        self.states
            .dormant
            .iter()
            .find(|(addr, d)| {
                addr.project == project && **addr != self.device && *d.plugin_id == *self.plugin_id && archived_id(&d.ids, id).is_some()
            })
            .map(|(addr, _)| KeptAt::Dormant(*addr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ara::graph_plan::GraphNow;
    use common::protocol::{AraModificationOrigin, AraRegionPlacement};
    use std::path::PathBuf;

    const P: ProjectKey = ProjectKey(1);
    const Q: ProjectKey = ProjectKey(2);
    const MELODYNE: &str = "com.celemony.ara.audiocontroller.version2";

    /// plug-in の document の代役: 形式、居る audio source と modification (`(id, source)`)、組んだことがあるか。
    /// 状態は「`{形式}:{id,...}`」(modification は source と一緒)、source だけは「`{形式}:src:{id}`」の bytes。
    struct FakeDoc {
        format: &'static str,
        sources: Vec<&'static str>,
        modifications: Vec<(&'static str, &'static str)>,
        built: bool,
    }

    fn wav(source: &str) -> PathBuf {
        PathBuf::from(format!("C:/{source}.wav"))
    }

    impl Document for FakeDoc {
        fn archive_format(&self) -> &str {
            self.format
        }
        fn can_import(&self, format: &str) -> bool {
            format == self.format
        }
        fn is_first_build(&self) -> bool {
            !self.built
        }
        fn plan(&self, clips: &[AraClipSpec]) -> GraphPlan {
            let wavs: Vec<PathBuf> = self.sources.iter().map(|s| wav(s)).collect();
            let now = GraphNow {
                sources: self.sources.iter().zip(&wavs).map(|(s, w)| (*s, w.as_path())).collect(),
                modifications: self.modifications.clone(),
                regions: Vec::new(),
            };
            graph_plan::plan(&now, clips)
        }
        fn modifications(&self) -> Vec<(&str, &str)> {
            self.modifications.clone()
        }
        fn has_modification(&self, id: &str) -> bool {
            self.modifications.iter().any(|(m, _)| *m == id)
        }
        fn has_source(&self, id: &str) -> bool {
            self.sources.contains(&id)
        }
        fn store_modification(&self, id: &str) -> Option<(Vec<u8>, String)> {
            let (bytes, mut held) = self.store_modifications(&HashSet::from([id]))?;
            Some((bytes, held.pop()?.1))
        }
        fn store_modifications(&self, ids: &HashSet<&str>) -> Option<ModificationsArchive> {
            let held: Vec<(String, String)> =
                self.modifications.iter().filter(|(m, _)| ids.contains(m)).map(|(m, s)| ((*m).to_owned(), (*s).to_owned())).collect();
            let names = held.iter().map(|(m, _)| m.as_str()).collect::<Vec<&str>>().join(",");
            (!held.is_empty()).then(|| (format!("{}:{names}", self.format).into_bytes(), held))
        }
        fn store_source(&self, id: &str) -> Option<Vec<u8>> {
            self.sources.contains(&id).then(|| format!("{}:src:{id}", self.format).into_bytes())
        }
        fn store_archive(&self) -> Option<AraArchive> {
            let ids = self.sources.iter().copied().chain(self.modifications.iter().map(|(m, _)| *m)).map(str::to_owned).collect();
            Some(AraArchive { bytes: format!("{}:all", self.format).into_bytes(), ids })
        }
    }

    /// 素材 `s` に modification `modifications` が居る document。
    fn doc(format: &'static str, modifications: &[&'static str]) -> FakeDoc {
        FakeDoc { format, sources: vec!["s"], modifications: modifications.iter().map(|m| (*m, "s")).collect(), built: true }
    }

    /// destroy した modification `(id, source, アーカイブが source の状態も持つか)` の状態。
    fn retired(bytes: &[u8], modifications: &[(&str, &str, bool)]) -> Retired {
        Retired {
            bytes: bytes.to_vec(),
            modifications: modifications.iter().map(|&(m, s, with)| (m.to_owned(), s.to_owned(), with)).collect(),
        }
    }

    fn spec(modification: &str, origins: &[(Option<ProjectKey>, u64, &str)]) -> AraClipSpec {
        spec_on("s", modification, origins)
    }

    fn spec_on(source: &str, modification: &str, origins: &[(Option<ProjectKey>, u64, &str)]) -> AraClipSpec {
        AraClipSpec {
            source_wav: wav(source),
            source_id: source.into(),
            modification_id: modification.into(),
            modification_origins: origins
                .iter()
                .map(|&(project, project_id, id)| AraModificationOrigin { project, project_id, modification_id: id.into() })
                .collect(),
            region_key: format!("r.{modification}"),
            placement: AraRegionPlacement {
                start_in_playback_seconds: 0.0,
                duration_in_playback_seconds: 1.0,
                start_in_modification_seconds: 0.0,
                duration_in_modification_seconds: 1.0,
                time_stretch: false,
            },
        }
    }

    fn addr(project: ProjectKey, device_id: u64) -> DeviceAddr {
        DeviceAddr { project, device_id }
    }

    const MELODYNE_PLUGIN: &str = "com.celemony.melodyne";

    /// Melodyne の device `device` の document `doc` を組む編集の始め方。
    fn resolve(
        states: &AraStates,
        device: DeviceAddr,
        doc: &FakeDoc,
        docs: &Sessions<'_, FakeDoc>,
        clips: &[AraClipSpec],
        saved: &[AraArchiveEntry],
    ) -> Starts {
        states.resolve_starts(device, MELODYNE_PLUGIN, doc, docs, clips, saved)
    }

    fn restore_of(kept: &KeptArchive) -> String {
        format!("restore {} from {}", kept.archived, String::from_utf8_lossy(&kept.bytes))
    }

    /// 解決した modification の始め方を `(作る id, 始め方の文字表現)` で。 restore は取ってきたアーカイブの中身と書かれて
    /// いる id。
    fn describe(starts: &Starts) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = starts
            .modifications
            .iter()
            .map(|(id, start)| {
                let how = match start {
                    Start::Saved(archived) => format!("saved {archived}"),
                    Start::Clone(origin) => format!("clone {origin}"),
                    Start::Restore(kept) => restore_of(kept),
                };
                (id.clone(), how)
            })
            .collect();
        out.sort();
        out
    }

    /// 解決した audio source の restore 元を `(作る source の id, 文字表現)` で。
    fn describe_sources(starts: &Starts) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = starts.sources.iter().map(|(id, kept)| (id.clone(), restore_of(kept))).collect();
        out.sort();
        out
    }

    fn pair(id: &str, how: &str) -> (String, String) {
        (id.to_owned(), how.to_owned())
    }

    /// 別のトラックへ写した take は、その document の今の状態から (この編集で自分の document から消える元は、自分の
    /// document を先に)。 別の plug-in の document の状態は写さない。
    #[test]
    fn 写した先は生きている_document_の今の状態から始め別の_plug_in_の状態は写さない() {
        let (a, b, c) = (addr(P, 1), addr(P, 2), addr(P, 3));
        let other_plugin = doc("com.example.other", &["m"]);
        let melodyne_b = doc(MELODYNE, &["m"]);
        let target = doc(MELODYNE, &["old"]);
        let docs = [(a, &other_plugin), (b, &melodyne_b), (c, &target)];
        let states = AraStates::default();
        let clips = [spec("m", &[]), spec("m2", &[(Some(P), 7, "old")])];
        let table = resolve(&states,c, &target, &docs, &clips, &[]);
        assert_eq!(
            describe(&table),
            vec![pair("m", &format!("restore m from {MELODYNE}:m")), pair("m2", &format!("restore old from {MELODYNE}:old"))],
            "m は別の plug-in (a) を飛ばして b から、m2 の元 old はこの編集で消えるこの document から"
        );
    }

    /// destroy した / 畳んだ document の状態は取っておいて、同じ id (undo / redo・移動) や写した元として引ける。 生きている
    /// document が同じ id を持っていればそちらが先。 閉じたプロジェクトの状態は捨てるが、同じ key のタブへ開き直した
    /// プロジェクトの状態はまた取っておく (Open は今のタブの key を使い続ける)。
    #[test]
    fn 取っておいた状態は生きている_document_が無いときに引き閉じたプロジェクトの分は捨てる() {
        let mut states = AraStates::default();
        states.retire(P, MELODYNE, retired(b"retired-m", &[("m", "s", false)]));
        states.keep_document(P, &doc(MELODYNE, &["n"]));
        let target = FakeDoc { built: false, ..doc(MELODYNE, &[]) };
        let t = addr(P, 9);
        let clips = [spec("m", &[]), spec("x", &[(Some(P), 5, "n")])];
        let alone = [(t, &target)];
        assert_eq!(
            describe(&resolve(&states,t, &target, &alone, &clips, &[])),
            vec![pair("m", "restore m from retired-m"), pair("x", &format!("restore n from {MELODYNE}:all"))]
        );

        let live = doc(MELODYNE, &["m"]);
        let with_live = [(addr(P, 1), &live), (t, &target)];
        assert_eq!(
            describe(&resolve(&states,t, &target, &with_live, &clips[..1], &[])),
            vec![pair("m", &format!("restore m from {MELODYNE}:m"))],
            "取っておいた状態より生きている document の今"
        );

        states.retire(Q, MELODYNE, retired(b"q", &[("q", "s", false)]));
        states.close_project(Q);
        let tq = addr(Q, 1);
        let only_q = [(tq, &target)];
        assert!(describe(&resolve(&states,tq, &target, &only_q, &[spec("q", &[])], &[])).is_empty(), "閉じた分は捨てる");
        states.retire(Q, MELODYNE, retired(b"reopened-q", &[("q", "s", false)]));
        assert_eq!(
            describe(&resolve(&states,tq, &target, &only_q, &[spec("q", &[])], &[])),
            vec![pair("q", "restore q from reopened-q")],
            "同じ key に開いたプロジェクトの状態は取っておく"
        );
        assert_eq!(describe(&resolve(&states,t, &target, &alone, &clips[..1], &[])).len(), 1, "ほかのプロジェクトの分は残る");
    }

    /// クリップボードの写しは写した時点の状態を持ち (後で元を消しても引ける)、元のプロジェクトの `project_id` が合う写した
    /// 元にだけ効く。 次に写すと前の写しは捨てる。
    #[test]
    fn クリップボードの写しは写した時点の状態で元のプロジェクトの_take_にだけ効く() {
        let mut states = AraStates::default();
        let source = doc(MELODYNE, &["m", "k"]);
        states.retire(P, MELODYNE, retired(b"retired-gone", &[("gone", "s", false)]));
        let from_p = [(addr(P, 1), &source)];
        states.snapshot_clipboard(&from_p, P, 42, &["m".into(), "gone".into(), "missing".into()]);

        let target = doc(MELODYNE, &[]);
        let t = addr(Q, 1);
        let alone = [(t, &target)];
        let clips = [spec("x", &[(None, 42, "m")]), spec("y", &[(None, 42, "gone")]), spec("z", &[(None, 43, "m")])];
        assert_eq!(
            describe(&resolve(&states,t, &target, &alone, &clips, &[])),
            vec![pair("x", &format!("restore m from {MELODYNE}:m")), pair("y", "restore gone from retired-gone")],
            "project_id が違う z は引かない"
        );

        states.snapshot_clipboard(&from_p, P, 42, &["k".into()]);
        assert!(describe(&resolve(&states,t, &target, &alone, &clips[..1], &[])).is_empty(), "前の写しは捨てる");
    }

    /// 同じ元を写した take がいくつあっても、生きている document からは 1 回だけ archive にする。 保存したアーカイブの
    /// 目次にある自分の状態 (初めて組む document) は、ほかの document より先。
    #[test]
    fn 初めて組む_document_は目次にある自分の状態を先に使う() {
        let live = doc(MELODYNE, &["m"]);
        let target = FakeDoc { built: false, ..doc(MELODYNE, &[]) };
        let (a, t) = (addr(P, 1), addr(P, 2));
        let docs = [(a, &live), (t, &target)];
        let saved = [AraArchiveEntry::stored("m".into())];
        let states = AraStates::default();
        let clips = [spec("m", &[]), spec("c1", &[(Some(P), 1, "m")]), spec("c2", &[(Some(P), 1, "m")])];
        let table = resolve(&states,t, &target, &docs, &clips, &saved);
        assert_eq!(
            describe(&table),
            vec![
                pair("c1", &format!("restore m from {MELODYNE}:m")),
                pair("c2", &format!("restore m from {MELODYNE}:m")),
                pair("m", "saved m"),
            ]
        );
        let (Some(Start::Restore(k1)), Some(Start::Restore(k2))) = (table.modifications.get("c1"), table.modifications.get("c2")) else {
            panic!("restore");
        };
        assert!(Arc::ptr_eq(&k1.bytes, &k2.bytes), "同じ元は 1 回だけ archive にする");
    }

    /// 別の document へ写した modification の audio source が写す先でこの編集に初めて現れる (保存したアーカイブにも無い)
    /// なら、source の状態も運ぶ: modification と一緒に取ったアーカイブ (生きている document / クリップボード / source
    /// ごと destroy した状態) から、それが source を持たなければ元のプロジェクトで元の source が居る生きている document から。
    /// 保存したアーカイブに source があるとき、source の上に状態の無い modification も作るときは運ばない。
    #[test]
    fn 写す先に初めて現れる_source_は写した_modification_と一緒に状態を運ぶ() {
        let target = FakeDoc { format: MELODYNE, sources: Vec::new(), modifications: Vec::new(), built: true };
        let t = addr(P, 9);
        let a = FakeDoc { format: MELODYNE, sources: vec!["v"], modifications: vec![("m", "v")], built: true };
        let with_a = [(addr(P, 1), &a), (t, &target)];
        let copy = [spec_on("v", "m2", &[(Some(P), 1, "m")])];
        let mut states = AraStates::default();
        let starts = resolve(&states,t, &target, &with_a, &copy, &[]);
        assert_eq!(describe(&starts), vec![pair("m2", &format!("restore m from {MELODYNE}:m"))]);
        assert_eq!(describe_sources(&starts), vec![pair("v", &format!("restore v from {MELODYNE}:m"))], "modification と同じアーカイブから");
        let (Start::Restore(m), Some(v)) = (&starts.modifications["m2"], starts.sources.get("v")) else { panic!("restore") };
        assert!(Arc::ptr_eq(&m.bytes, &v.bytes), "1 つのアーカイブ (1 回の restore) で source が modification に先行する");

        let saved = [AraArchiveEntry::stored("v".into())];
        assert!(describe_sources(&resolve(&states,t, &target, &with_a, &copy, &saved)).is_empty(), "保存したアーカイブの source が先");
        let with_fresh_take = [copy[0].clone(), spec_on("v", "new", &[])];
        assert!(
            describe_sources(&resolve(&states,t, &target, &with_a, &with_fresh_take, &[])).is_empty(),
            "状態の無い modification が同じ source に居れば運ばない (source を restore したら全部の modification も restore する規約)"
        );

        // 元を destroy した状態が source を持たない (元の document に source が残った) なら、その document の source だけを取る。
        states.retire(P, MELODYNE, retired(b"retired-m", &[("m", "v", false)]));
        let a_without_m = FakeDoc { modifications: Vec::new(), ..a };
        let a_now = [(addr(P, 1), &a_without_m), (t, &target)];
        let starts = resolve(&states,t, &target, &a_now, &copy, &[]);
        assert_eq!(describe(&starts), vec![pair("m2", "restore m from retired-m")]);
        assert_eq!(describe_sources(&starts), vec![pair("v", &format!("restore v from {MELODYNE}:src:v"))]);
        let alone = [(t, &target)];
        assert!(describe_sources(&resolve(&states,t, &target, &alone, &copy, &[])).is_empty(), "source がどこにも無ければ運ばない");
        states.retire(P, MELODYNE, retired(b"retired-m-v", &[("m", "v", true)]));
        assert_eq!(
            describe_sources(&resolve(&states,t, &target, &alone, &copy, &[])),
            vec![pair("v", "restore v from retired-m-v")],
            "source ごと destroy した状態は source を運ぶ"
        );

        // 別のプロジェクトから: source の id は元のプロジェクトのもの。 開いているタブの document からも、閉じた後に貼る
        // クリップボードの写しからも (写した時点で source が居る document から取っておく)。
        let q_doc = FakeDoc { format: MELODYNE, sources: vec!["daw01.source.3"], modifications: Vec::new(), built: true };
        states.retire(Q, MELODYNE, retired(b"q-retired-m", &[("m", "daw01.source.3", false)]));
        let with_q = [(addr(Q, 1), &q_doc), (t, &target)];
        let from_tab = [spec_on("daw01.source.8", "m9", &[(Some(Q), 77, "m")])];
        let starts = resolve(&states,t, &target, &with_q, &from_tab, &[]);
        assert_eq!(describe(&starts), vec![pair("m9", "restore m from q-retired-m")]);
        assert_eq!(describe_sources(&starts), vec![pair("daw01.source.8", &format!("restore daw01.source.3 from {MELODYNE}:src:daw01.source.3"))]);
        states.snapshot_clipboard(&[(addr(Q, 1), &q_doc)], Q, 77, &["m".into()]);
        states.close_project(Q);
        let pasted = [spec_on("daw01.source.8", "m9", &[(None, 77, "m")])];
        assert_eq!(
            describe_sources(&resolve(&states,t, &target, &alone, &pasted, &[])),
            vec![pair("daw01.source.8", &format!("restore daw01.source.3 from {MELODYNE}:src:daw01.source.3"))]
        );
        let q_with_m = FakeDoc { format: MELODYNE, sources: vec!["daw01.source.3"], modifications: vec![("m", "daw01.source.3")], built: true };
        states.snapshot_clipboard(&[(addr(Q, 1), &q_with_m)], Q, 77, &["m".into()]);
        assert_eq!(
            describe_sources(&resolve(&states,t, &target, &alone, &pasted, &[])),
            vec![pair("daw01.source.8", &format!("restore daw01.source.3 from {MELODYNE}:m"))],
            "生きている modification の写しは source と 1 つのアーカイブ"
        );
    }

    /// document の無い device (無効のトラック) の保存したアーカイブは、同じ plug-in の document にだけ、目次で書かれている
    /// id へ読み替えて restore する (modification と source)。 生きている document があればそちらが先、その device の
    /// document を組んだら / プロジェクトを閉じたら捨てる。
    #[test]
    fn document_の無い_device_の保存したアーカイブから移したクリップの状態を引く() {
        use common::ara_ids::{source_id, take_modification_id};
        let (v, m) = (source_id(3), take_modification_id(10, 1, 3));
        let mut states = AraStates::default();
        let disabled = addr(P, 4);
        let toc = vec![
            AraArchiveEntry { current: v.clone(), archived: Some("3:10:0".into()) },
            AraArchiveEntry { current: m.clone(), archived: Some("3:10:0/mod".into()) },
        ];
        states.keep_dormant(disabled, MELODYNE_PLUGIN, b"disabled-track".to_vec(), toc.clone());
        let target = FakeDoc { format: MELODYNE, sources: Vec::new(), modifications: Vec::new(), built: true };
        let t = addr(P, 9);
        let alone = [(t, &target)];
        let moved = [spec_on(&v, &m, &[])];
        let starts = resolve(&states, t, &target, &alone, &moved, &[]);
        assert_eq!(describe(&starts), vec![pair(&m, "restore 3:10:0/mod from disabled-track")]);
        assert_eq!(describe_sources(&starts), vec![pair(&v, "restore 3:10:0 from disabled-track")]);
        let copied_from_it = [spec_on(&v, "copy", &[(Some(P), 1, &m)])];
        assert_eq!(describe(&resolve(&states, t, &target, &alone, &copied_from_it, &[])), vec![pair("copy", "restore 3:10:0/mod from disabled-track")]);

        assert!(
            describe(&states.resolve_starts(t, "com.example.other", &target, &alone, &moved, &[])).is_empty(),
            "別の plug-in の device には restore しない"
        );
        let a = FakeDoc { format: MELODYNE, sources: vec!["v"], modifications: vec![("m", "v")], built: true };
        let with_live = [(addr(P, 1), &a), (t, &target)];
        let moved_live = [spec_on("v", "m", &[])];
        states.keep_dormant(disabled, MELODYNE_PLUGIN, b"disabled-track".to_vec(), vec![AraArchiveEntry::stored("m".into())]);
        assert_eq!(describe(&resolve(&states, t, &target, &with_live, &moved_live, &[])), vec![pair("m", &format!("restore m from {MELODYNE}:m"))]);

        // 閉じた後に貼る写し: クリップボードへ写すとき、document の無い device の保存したアーカイブから取っておく。
        states.keep_dormant(disabled, MELODYNE_PLUGIN, b"disabled-track".to_vec(), toc.clone());
        states.snapshot_clipboard::<FakeDoc>(&[], P, 42, std::slice::from_ref(&m));
        states.forget_dormant(disabled);
        assert!(describe(&resolve(&states, t, &target, &alone, &moved, &[])).is_empty(), "その device の document を組んだら捨てる");
        states.close_project(P);
        let pasted = [spec_on("daw01.source.8", "pasted", &[(None, 42, &m)])];
        let starts = resolve(&states, addr(Q, 1), &target, &[(addr(Q, 1), &target)], &pasted, &[]);
        assert_eq!(describe(&starts), vec![pair("pasted", "restore 3:10:0/mod from disabled-track")]);
        assert_eq!(describe_sources(&starts), vec![pair("daw01.source.8", "restore 3:10:0 from disabled-track")]);
        assert!(
            describe(&states.resolve_starts(addr(Q, 1), "com.example.other", &target, &[(addr(Q, 1), &target)], &pasted, &[])).is_empty(),
            "クリップボードの写しも同じ plug-in にだけ"
        );
        states.keep_dormant(disabled, MELODYNE_PLUGIN, b"disabled-track".to_vec(), toc);
        states.close_project(P);
        assert!(describe(&resolve(&states, t, &target, &alone, &moved, &[])).is_empty(), "プロジェクトを閉じたら捨てる");
    }
}
