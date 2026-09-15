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
//! - **畳んだ document** — device を降ろす / ARA document を消す直前の、document 全体のアーカイブ (トラックの無効化・
//!   削除の後に、そこに居たクリップを別のトラックへ写す / 移す)。 プロジェクトを閉じるときは取らない (もう引かれない)。
//! - **クリップボード** — 写した時点の modification の partial archive (元のプロジェクトを閉じた後に貼る)。
//!
//! 状態は書いた plug-in の形式 (`documentArchiveID`) と一緒に持ち、読める形式の document にだけ restore する
//! (別の plug-in の状態は写さない: ARAInterface.h `documentArchiveID` "shared only amongst document controllers
//! that create the same archives")。 プロジェクトの中の id は安定 id なので同じ id は同じ take の状態で、
//! 最新だけを持つ。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use common::ara_ids::AraArchiveEntry;
use common::protocol::{AraArchive, AraClipSpec, DeviceAddr, ProjectKey};

use crate::ara::graph_plan::{self, DocumentNow, GraphPlan, KeptAt, KeptStates, ModificationStart};
use crate::ara::session::{AraSession, Retired, Start, StartTable};

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
    /// 今 document に居る audio modification。
    fn modification_ids(&self) -> Vec<&str>;
    /// audio modification `id` が今 document に居るか。
    fn has_modification(&self, id: &str) -> bool;
    /// modification `id` の partial archive。
    fn store_modification(&self, id: &str) -> Option<Vec<u8>>;
    /// modification `ids` のうち居るものの partial archive と、中の id。
    fn store_modifications(&self, ids: &HashSet<&str>) -> Option<(Vec<u8>, Vec<String>)>;
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
    fn modification_ids(&self) -> Vec<&str> {
        AraSession::modification_ids(self).collect()
    }
    fn has_modification(&self, id: &str) -> bool {
        AraSession::has_modification(self, id)
    }
    fn store_modification(&self, id: &str) -> Option<Vec<u8>> {
        AraSession::store_modification(self, id)
    }
    fn store_modifications(&self, ids: &HashSet<&str>) -> Option<(Vec<u8>, Vec<String>)> {
        AraSession::store_modifications(self, ids)
    }
    fn store_archive(&self) -> Option<AraArchive> {
        AraSession::store_archive(self)
    }
}

/// 取っておいた 1 つの modification の状態 (それを含むアーカイブと、書いた plug-in の形式)。
#[derive(Clone)]
struct KeptState {
    format: Arc<str>,
    bytes: Arc<[u8]>,
}

/// クリップボードへ写した時点の状態 (最後に写したものだけ)。
struct ClipboardStates {
    /// 写した元のプロジェクト (`Song::project_id`)。
    project_id: u64,
    states: HashMap<String, KeptState>,
}

/// plug-in host に居る document (device 順 = 引き当ての順序)。
pub type Sessions<'a, D> = [(DeviceAddr, &'a D)];

#[derive(Default)]
pub struct AraStates {
    /// プロジェクトごとの、document の外に取っておいた modification の状態 (id ごとに最新)。
    retired: HashMap<ProjectKey, HashMap<String, KeptState>>,
    clipboard: Option<ClipboardStates>,
    /// 閉じた / 閉じているプロジェクト (状態を取っておかない。 `ProjectKey` はプロセスの中で使い回さない)。
    closed: HashSet<ProjectKey>,
}

impl AraStates {
    /// `project` の document (形式 `format`) が destroy した modification の状態を取っておく。
    pub fn retire(&mut self, project: ProjectKey, format: &str, retired: Retired) {
        if retired.is_empty() || self.closed.contains(&project) {
            return;
        }
        let format: Arc<str> = Arc::from(format);
        let kept = self.retired.entry(project).or_default();
        for (id, bytes) in retired {
            kept.insert(id, KeptState { format: Arc::clone(&format), bytes: Arc::from(bytes) });
        }
    }

    /// 畳む document `doc` の状態 (document 全体のアーカイブ) を、中の modification ごとに取っておく。
    pub fn keep_document(&mut self, project: ProjectKey, doc: &impl Document) {
        if self.closed.contains(&project) {
            return;
        }
        let modifications: HashSet<String> = doc.modification_ids().into_iter().map(str::to_owned).collect();
        if modifications.is_empty() {
            return;
        }
        let Some(archive) = doc.store_archive() else {
            tracing::warn!(?project, "ARA: storing a closing document's archive failed; its edits cannot be copied later");
            return;
        };
        let state = KeptState { format: Arc::from(doc.archive_format()), bytes: Arc::from(archive.bytes) };
        let kept = self.retired.entry(project).or_default();
        for id in archive.ids.into_iter().filter(|id| modifications.contains(id)) {
            kept.insert(id, state.clone());
        }
    }

    /// プロジェクト `project` を閉じる: 取っておいた状態を捨て、以後は取っておかない。
    pub fn close_project(&mut self, project: ProjectKey) {
        self.retired.remove(&project);
        self.closed.insert(project);
    }

    /// クリップボードへ写した modification `ids` (プロジェクト `project`、`Song::project_id` は `project_id`) の今の
    /// 状態を取っておく (前の写しは捨てる)。 生きている document に居ればその partial archive、居なければ取っておいた状態。
    pub fn snapshot_clipboard<D: Document>(&mut self, docs: &Sessions<'_, D>, project: ProjectKey, project_id: u64, ids: &[String]) {
        let mut wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let mut states = HashMap::new();
        for (_, doc) in docs.iter().filter(|(addr, _)| addr.project == project) {
            let Some((bytes, held)) = doc.store_modifications(&wanted) else {
                continue;
            };
            let state = KeptState { format: Arc::from(doc.archive_format()), bytes: Arc::from(bytes) };
            for id in held {
                wanted.remove(id.as_str());
                states.insert(id, state.clone());
            }
        }
        if let Some(kept) = self.retired.get(&project) {
            for id in wanted {
                if let Some(state) = kept.get(id) {
                    states.insert(id.to_owned(), state.clone());
                }
            }
        }
        self.clipboard = Some(ClipboardStates { project_id, states });
    }

    /// `device` の document `doc` を `clips` に合わせる編集で作る modification の、始め方 ([`StartTable`])。
    /// `saved` = document の保存したアーカイブの目次 (アーカイブが無ければ空)。 決め方は
    /// [`graph_plan::modification_start`]、ほかの document の今の状態はここで partial archive にする (その document は
    /// 編集中ではない)。
    pub fn resolve_starts<D: Document>(
        &self,
        device: DeviceAddr,
        doc: &D,
        docs: &Sessions<'_, D>,
        clips: &[AraClipSpec],
        saved: &[AraArchiveEntry],
    ) -> StartTable {
        let plan = doc.plan(clips);
        let surviving: HashSet<String> = doc
            .modification_ids()
            .into_iter()
            .filter(|id| !plan.destroy_modifications.contains(*id))
            .map(str::to_owned)
            .collect();
        let now = DocumentNow { project: device.project, first_build: doc.is_first_build(), saved, surviving: &surviving };
        let lookup = Lookup { states: self, docs, device, target: doc };
        let mut live_copies: HashMap<(DeviceAddr, String), Option<Arc<[u8]>>> = HashMap::new();
        let mut table = StartTable::new();
        for spec in plan.create_modifications.iter().map(|&i| &clips[i]) {
            let start = match graph_plan::modification_start(spec, now, &lookup) {
                ModificationStart::Saved(archived) => Some(Start::Saved(archived)),
                ModificationStart::Clone(origin) => Some(Start::Clone(origin)),
                ModificationStart::Kept { at, id } => {
                    self.kept_bytes(at, &id, docs, &mut live_copies).map(|bytes| Start::Restore { bytes, archived: id })
                }
                ModificationStart::Empty => None,
            };
            if let Some(start) = start {
                table.insert(spec.modification_id.clone(), start);
            }
        }
        table
    }

    /// `at` にある modification `id` の状態を含むアーカイブ。 生きている document からは 1 回だけ partial archive に
    /// する (`live_copies`)。
    fn kept_bytes<D: Document>(
        &self,
        at: KeptAt,
        id: &str,
        docs: &Sessions<'_, D>,
        live_copies: &mut HashMap<(DeviceAddr, String), Option<Arc<[u8]>>>,
    ) -> Option<Arc<[u8]>> {
        match at {
            KeptAt::Live(addr) => live_copies
                .entry((addr, id.to_owned()))
                .or_insert_with(|| {
                    let doc = docs.iter().find(|(a, _)| *a == addr).map(|(_, d)| *d)?;
                    let copied = doc.store_modification(id).map(Arc::from);
                    if copied.is_none() {
                        tracing::warn!(?addr, %id, "ARA: storing another document's modification failed; the copy starts empty");
                    }
                    copied
                })
                .clone(),
            KeptAt::Retired(project) => self.retired.get(&project)?.get(id).map(|k| Arc::clone(&k.bytes)),
            KeptAt::Clipboard => self.clipboard.as_ref()?.states.get(id).map(|k| Arc::clone(&k.bytes)),
        }
    }
}

/// [`KeptStates`] を plug-in host の今から答える。 `target` = 状態を restore する document (読める形式だけを答える)。
struct Lookup<'a, D> {
    states: &'a AraStates,
    docs: &'a Sessions<'a, D>,
    device: DeviceAddr,
    target: &'a D,
}

impl<D: Document> KeptStates for Lookup<'_, D> {
    fn in_project(&self, project: ProjectKey, id: &str) -> Option<KeptAt> {
        // 生きている document: この document (この編集で消す modification) を先に、残りは device 順。
        let own = self.docs.iter().filter(|(addr, _)| *addr == self.device);
        let others = self.docs.iter().filter(|(addr, _)| *addr != self.device);
        let live = own.chain(others).find(|(addr, doc)| {
            addr.project == project && self.target.can_import(doc.archive_format()) && doc.has_modification(id)
        });
        if let Some((addr, _)) = live {
            return Some(KeptAt::Live(*addr));
        }
        let kept = self.states.retired.get(&project)?.get(id)?;
        self.target.can_import(&kept.format).then_some(KeptAt::Retired(project))
    }

    fn in_clipboard(&self, project_id: u64, id: &str) -> bool {
        self.states
            .clipboard
            .as_ref()
            .filter(|c| c.project_id == project_id)
            .and_then(|c| c.states.get(id))
            .is_some_and(|k| self.target.can_import(&k.format))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ara::graph_plan::GraphNow;
    use common::protocol::{AraModificationOrigin, AraRegionPlacement};
    use std::path::{Path, PathBuf};

    const P: ProjectKey = ProjectKey(1);
    const Q: ProjectKey = ProjectKey(2);
    const MELODYNE: &str = "com.celemony.ara.audiocontroller.version2";

    /// plug-in の document の代役: 形式、居る modification、組んだことがあるか。 状態は「`{形式}:{id}`」の bytes。
    struct FakeDoc {
        format: &'static str,
        modifications: Vec<&'static str>,
        built: bool,
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
            let wav = Path::new("C:/s.wav");
            let now = GraphNow {
                sources: vec![("s", wav)],
                modifications: self.modifications.iter().map(|m| (*m, "s")).collect(),
                regions: Vec::new(),
            };
            graph_plan::plan(&now, clips)
        }
        fn modification_ids(&self) -> Vec<&str> {
            self.modifications.clone()
        }
        fn has_modification(&self, id: &str) -> bool {
            self.modifications.contains(&id)
        }
        fn store_modification(&self, id: &str) -> Option<Vec<u8>> {
            self.modifications.contains(&id).then(|| format!("{}:{id}", self.format).into_bytes())
        }
        fn store_modifications(&self, ids: &HashSet<&str>) -> Option<(Vec<u8>, Vec<String>)> {
            let held: Vec<String> = self.modifications.iter().filter(|m| ids.contains(*m)).map(|m| (*m).to_owned()).collect();
            (!held.is_empty()).then(|| (format!("{}:{}", self.format, held.join(",")).into_bytes(), held))
        }
        fn store_archive(&self) -> Option<AraArchive> {
            let ids = self.modifications.iter().map(|m| (*m).to_owned()).collect();
            Some(AraArchive { bytes: format!("{}:all", self.format).into_bytes(), ids })
        }
    }

    fn doc(format: &'static str, modifications: &[&'static str]) -> FakeDoc {
        FakeDoc { format, modifications: modifications.to_vec(), built: true }
    }

    fn spec(modification: &str, origins: &[(Option<ProjectKey>, u64, &str)]) -> AraClipSpec {
        AraClipSpec {
            source_wav: PathBuf::from("C:/s.wav"),
            source_id: "s".into(),
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

    /// 解決した始め方を `(作る id, 始め方の文字表現)` で。 restore は取ってきたアーカイブの中身と書かれている id。
    fn describe(table: &StartTable) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = table
            .iter()
            .map(|(id, start)| {
                let how = match start {
                    Start::Saved(archived) => format!("saved {archived}"),
                    Start::Clone(origin) => format!("clone {origin}"),
                    Start::Restore { bytes, archived } => format!("restore {archived} from {}", String::from_utf8_lossy(bytes)),
                };
                (id.clone(), how)
            })
            .collect();
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
        let table = states.resolve_starts(c, &target, &docs, &clips, &[]);
        assert_eq!(
            describe(&table),
            vec![pair("m", &format!("restore m from {MELODYNE}:m")), pair("m2", &format!("restore old from {MELODYNE}:old"))],
            "m は別の plug-in (a) を飛ばして b から、m2 の元 old はこの編集で消えるこの document から"
        );
    }

    /// destroy した / 畳んだ document の状態は取っておいて、同じ id (undo / redo・移動) や写した元として引ける。 生きている
    /// document が同じ id を持っていればそちらが先。 閉じたプロジェクトの状態は取っておかない。
    #[test]
    fn 取っておいた状態は生きている_document_が無いときに引き閉じたプロジェクトは取っておかない() {
        let mut states = AraStates::default();
        states.retire(P, MELODYNE, vec![("m".into(), b"retired-m".to_vec())]);
        states.keep_document(P, &doc(MELODYNE, &["n"]));
        let target = FakeDoc { built: false, ..doc(MELODYNE, &[]) };
        let t = addr(P, 9);
        let clips = [spec("m", &[]), spec("x", &[(Some(P), 5, "n")])];
        let alone = [(t, &target)];
        assert_eq!(
            describe(&states.resolve_starts(t, &target, &alone, &clips, &[])),
            vec![pair("m", "restore m from retired-m"), pair("x", &format!("restore n from {MELODYNE}:all"))]
        );

        let live = doc(MELODYNE, &["m"]);
        let with_live = [(addr(P, 1), &live), (t, &target)];
        assert_eq!(
            describe(&states.resolve_starts(t, &target, &with_live, &clips[..1], &[])),
            vec![pair("m", &format!("restore m from {MELODYNE}:m"))],
            "取っておいた状態より生きている document の今"
        );

        states.close_project(Q);
        states.retire(Q, MELODYNE, vec![("q".into(), b"q".to_vec())]);
        states.keep_document(Q, &doc(MELODYNE, &["r"]));
        let tq = addr(Q, 1);
        let only_q = [(tq, &target)];
        assert!(states.resolve_starts(tq, &target, &only_q, &[spec("q", &[]), spec("r", &[])], &[]).is_empty());
    }

    /// クリップボードの写しは写した時点の状態を持ち (後で元を消しても引ける)、元のプロジェクトの `project_id` が合う写した
    /// 元にだけ効く。 次に写すと前の写しは捨てる。
    #[test]
    fn クリップボードの写しは写した時点の状態で元のプロジェクトの_take_にだけ効く() {
        let mut states = AraStates::default();
        let source = doc(MELODYNE, &["m", "k"]);
        states.retire(P, MELODYNE, vec![("gone".into(), b"retired-gone".to_vec())]);
        let from_p = [(addr(P, 1), &source)];
        states.snapshot_clipboard(&from_p, P, 42, &["m".into(), "gone".into(), "missing".into()]);

        let target = doc(MELODYNE, &[]);
        let t = addr(Q, 1);
        let alone = [(t, &target)];
        let clips = [spec("x", &[(None, 42, "m")]), spec("y", &[(None, 42, "gone")]), spec("z", &[(None, 43, "m")])];
        assert_eq!(
            describe(&states.resolve_starts(t, &target, &alone, &clips, &[])),
            vec![pair("x", &format!("restore m from {MELODYNE}:m")), pair("y", "restore gone from retired-gone")],
            "project_id が違う z は引かない"
        );

        states.snapshot_clipboard(&from_p, P, 42, &["k".into()]);
        assert!(states.resolve_starts(t, &target, &alone, &clips[..1], &[]).is_empty(), "前の写しは捨てる");
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
        let table = states.resolve_starts(t, &target, &docs, &clips, &saved);
        assert_eq!(
            describe(&table),
            vec![
                pair("c1", &format!("restore m from {MELODYNE}:m")),
                pair("c2", &format!("restore m from {MELODYNE}:m")),
                pair("m", "saved m"),
            ]
        );
        let (Some(Start::Restore { bytes: b1, .. }), Some(Start::Restore { bytes: b2, .. })) = (table.get("c1"), table.get("c2")) else {
            panic!("restore");
        };
        assert!(Arc::ptr_eq(b1, b2), "同じ元は 1 回だけ archive にする");
    }
}
