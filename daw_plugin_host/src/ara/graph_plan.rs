//! ARA document のモデルグラフを [`AraClipSpec`] の集合へ合わせる **差分** (純関数、FFI を呼ばない)。
//!
//! ARA のモデルグラフは host が編集し続けるもの (`ara-sys/vendor/ARA_API/ARAInterface.h`: object の create /
//! update / destroy を `beginEditing` / `endEditing` で括る)。 分割で region が 1 つ増えても、audio source と
//! audio modification は persistent id が同じなら **作り直さない** — 作り直すと plug-in の中の編集 (Melodyne の
//! 音符の修正) が消え、保存したアーカイブ (最後に保存した時点) からしか戻らない。 同ヘッダ: modification は
//! "a set of musical edits that the user has made"、region は "a reference to an arbitrary time section of an audio
//! modification" で "not persistent when storing documents, instead the host re-creates them as needed"。
//!
//! 依存の順 (region → modification → source) を守る: 残す modification は source も残り、残す region は
//! modification も残る (同ヘッダ destroyAudioSource: "The host must delete all objects associated with the audio
//! source (audio modifications etc.) before deleting the audio source")。

use std::collections::HashSet;

use common::ara_ids::{AraArchiveEntry, archived_id};
use common::protocol::{AraClipSpec, DeviceAddr, ProjectKey};

/// 今の document に居る object (persistent id と、依存先)。
#[derive(Debug, Default)]
pub struct GraphNow<'a> {
    /// `(source の persistent id, 読んでいる WAV)`。
    pub sources: Vec<(&'a str, &'a std::path::Path)>,
    /// `(modification の persistent id, source の persistent id)`。
    pub modifications: Vec<(&'a str, &'a str)>,
    /// `(region のキー, modification の persistent id)`。
    pub regions: Vec<(&'a str, &'a str)>,
}

/// document を `specs` に合わせる編集。 `create_*` / `update_regions` は `specs` の index (同じ id の 2 つ目
/// 以降は作らない)。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GraphPlan {
    /// renderer から外して destroy する region のキー。
    pub remove_regions: HashSet<String>,
    pub destroy_modifications: HashSet<String>,
    pub destroy_sources: HashSet<String>,
    pub create_sources: Vec<usize>,
    pub create_modifications: Vec<usize>,
    pub create_regions: Vec<usize>,
    /// 残す region の置き方を `specs[i]` で更新する。
    pub update_regions: Vec<usize>,
}

/// `now` を `specs` に合わせる差分。
#[must_use]
pub fn plan(now: &GraphNow<'_>, specs: &[AraClipSpec]) -> GraphPlan {
    let kept_sources: HashSet<&str> = now
        .sources
        .iter()
        .filter(|(id, wav)| specs.iter().any(|s| s.source_id == *id && s.source_wav == *wav))
        .map(|(id, _)| *id)
        .collect();
    let kept_modifications: HashSet<&str> = now
        .modifications
        .iter()
        .filter(|(id, source)| {
            kept_sources.contains(source) && first_with(specs, |s| s.modification_id == *id).is_some_and(|s| s.source_id == *source)
        })
        .map(|(id, _)| *id)
        .collect();
    let kept_regions: HashSet<&str> = now
        .regions
        .iter()
        .filter(|(key, modification)| {
            kept_modifications.contains(modification)
                && first_with(specs, |s| s.region_key == *key).is_some_and(|s| s.modification_id == *modification)
        })
        .map(|(key, _)| *key)
        .collect();

    let mut out = GraphPlan {
        remove_regions: gone(now.regions.iter().map(|r| r.0), &kept_regions),
        destroy_modifications: gone(now.modifications.iter().map(|m| m.0), &kept_modifications),
        destroy_sources: gone(now.sources.iter().map(|s| s.0), &kept_sources),
        ..GraphPlan::default()
    };
    let (mut sources, mut modifications, mut regions) = (HashSet::new(), HashSet::new(), HashSet::new());
    for (i, spec) in specs.iter().enumerate() {
        if sources.insert(spec.source_id.as_str()) && !kept_sources.contains(spec.source_id.as_str()) {
            out.create_sources.push(i);
        }
        if modifications.insert(spec.modification_id.as_str()) && !kept_modifications.contains(spec.modification_id.as_str()) {
            out.create_modifications.push(i);
        }
        if regions.insert(spec.region_key.as_str()) {
            if kept_regions.contains(spec.region_key.as_str()) {
                out.update_regions.push(i);
            } else {
                out.create_regions.push(i);
            }
        }
    }
    out
}

/// document の外に取ってある modification の状態の場所 ([`KeptStates`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeptAt {
    /// 生きている document (その device の session に居る。 この編集で消える、同じ document の modification も含む)。
    Live(DeviceAddr),
    /// plug-in host が取っておいた状態 (modification を destroy した / session を畳んだ時点、プロジェクトごと)。
    Retired(ProjectKey),
    /// クリップボードへ写した時点の状態。
    Clipboard,
    /// plug-in host に document の無い device (無効のトラック / まだ組んでいない) の保存したアーカイブ。
    Dormant(DeviceAddr),
}

/// 新しく作る modification の中身をどこから始めるか。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModificationStart {
    /// 保存したアーカイブに `.0` の id で書かれた状態。
    Saved(String),
    /// この編集の前から document に居て編集の後も残る modification `.0` を `cloneAudioModification` で写す。
    Clone(String),
    /// document の外 (`at`) に取ってある modification `id` の状態を restore する。
    Kept { at: KeptAt, id: String },
    /// 状態がどこにも無い (空で始める)。
    Empty,
}

/// modification を作る document の今。
#[derive(Debug, Clone, Copy)]
pub struct DocumentNow<'a> {
    pub project: ProjectKey,
    /// document を初めて組む (開いた直後 / 載せ直した device: 保存したアーカイブが document の状態)。
    pub first_build: bool,
    /// 保存したアーカイブの目次 (アーカイブが無ければ空)。
    pub saved: &'a [AraArchiveEntry],
    /// この編集の前から document に居て、編集の後も残る modification。
    pub surviving: &'a HashSet<String>,
}

/// document の外の状態の引き当て (plug-in host 全体を見る側が答える)。 同じ plug-in の形式で書いた状態だけを答える。
pub trait KeptStates {
    /// プロジェクト `project` の中で modification `id` の状態を持っている場所。 生きている document を先に答える。
    fn in_project(&self, project: ProjectKey, id: &str) -> Option<KeptAt>;
    /// クリップボードの写し (元のプロジェクト `project_id`) が modification `id` の状態を持っているか。
    fn in_clipboard(&self, project_id: u64, id: &str) -> bool;
    /// プロジェクト `project` の、plug-in host に document の無い device の保存したアーカイブのうち modification `id` の
    /// 状態を持つもの。
    fn in_dormant(&self, project: ProjectKey, id: &str) -> Option<KeptAt>;
}

/// `spec` の modification の始め方。 **自分の状態が先、写した元は自分の状態がどこにも無いときだけ**:
///
/// 1. document を初めて組むとき、保存したアーカイブの目次にあれば保存した自分の状態 (開き直した / 載せ直した
///    device。 同じ content を別のトラックでも鳴らしている (linked) とき、そちらの今の状態で上書きしない)。
/// 2. プロジェクトの中に自分の状態があればそこから: 生きている document (別のトラックへ移した / 別のトラックの linked
///    clip) を先に、無ければ plug-in host が取っておいた最新の状態 (undo / redo で戻る、無効にしたトラックから移した)。
///    生きている方を先にするのは、取っておいた状態がその後に別の document で続けた編集より古いことがあるから
///    (移動の undo)。
/// 3. 保存したアーカイブの目次にあれば保存した自分の状態 (最後に保存した時点)、無ければ document の無い device (無効の
///    トラック) の保存したアーカイブ (そこから移したクリップ)。
/// 4. 写した元 (近い順) の状態: 同じプロジェクトの元は、この document に残るなら `cloneAudioModification`、無ければ
///    プロジェクトの中 (2 と同じ順)、クリップボードの写し、保存したアーカイブの目次、document の無い device の保存した
///    アーカイブ。 別のプロジェクトの元はクリップボードの写し (写した時点 = 貼った中身と同じ時点) を先に、そのプロジェクトが
///    開いていればその中 (2 と同じ順、その後に document の無い device の保存したアーカイブ)。 別のプロジェクトの id は自分の
///    プロジェクトの id と同じ文字列になり得るので、この document とアーカイブの目次では引かない。
/// 5. どこにも無ければ空。
#[must_use]
pub fn modification_start(spec: &AraClipSpec, doc: DocumentNow<'_>, kept: &impl KeptStates) -> ModificationStart {
    let own = spec.modification_id.as_str();
    let saved = |id: &str| archived_id(doc.saved, id).map(|a| ModificationStart::Saved(a.to_owned()));
    let kept_at = |at: KeptAt, id: &str| ModificationStart::Kept { at, id: id.to_owned() };
    let in_project = |project: ProjectKey, id: &str| kept.in_project(project, id).map(|at| kept_at(at, id));
    let in_clipboard = |project_id: u64, id: &str| kept.in_clipboard(project_id, id).then(|| kept_at(KeptAt::Clipboard, id));
    let in_dormant = |project: ProjectKey, id: &str| kept.in_dormant(project, id).map(|at| kept_at(at, id));
    if let Some(start) = saved(own).filter(|_| doc.first_build) {
        return start;
    }
    if let Some(start) = in_project(doc.project, own).or_else(|| saved(own)).or_else(|| in_dormant(doc.project, own)) {
        return start;
    }
    for origin in &spec.modification_origins {
        let id = origin.modification_id.as_str();
        let found = if origin.project == Some(doc.project) {
            if doc.surviving.contains(id) {
                return ModificationStart::Clone(id.to_owned());
            }
            in_project(doc.project, id)
                .or_else(|| in_clipboard(origin.project_id, id))
                .or_else(|| saved(id))
                .or_else(|| in_dormant(doc.project, id))
        } else {
            let open = |find: &dyn Fn(ProjectKey) -> Option<ModificationStart>| origin.project.and_then(find);
            in_clipboard(origin.project_id, id).or_else(|| open(&|p| in_project(p, id))).or_else(|| open(&|p| in_dormant(p, id)))
        };
        if let Some(start) = found {
            return start;
        }
    }
    ModificationStart::Empty
}

/// 編集で作った object の状態をどのアーカイブから restore するか ([`restore_calls`] の入力)。 アーカイブは
/// `None` = document の保存したアーカイブ、`Some(i)` = この編集が document の外から持ってきた `i` 番目の partial
/// archive。 `sources` の要素は `(アーカイブ, アーカイブに書かれている id, document の id)`、`modifications` はそれに
/// document の中の audio source の id を足したもの。
#[derive(Debug, Default)]
pub struct Restores<'a> {
    pub sources: Vec<(Option<usize>, &'a str, &'a str)>,
    pub modifications: Vec<(Option<usize>, &'a str, &'a str, &'a str)>,
}

/// `restoreObjectsFromArchive` の 1 回 ([`restore_calls`])。
#[derive(Debug, PartialEq, Eq)]
pub struct RestoreCall<'a> {
    /// [`Restores`] と同じアーカイブの指し方。
    pub archive: Option<usize>,
    /// 保存したアーカイブの document data も restore する。
    pub document_data: bool,
    /// `(アーカイブに書かれている id, document の id)`。
    pub sources: Vec<(&'a str, &'a str)>,
    pub modifications: Vec<(&'a str, &'a str)>,
}

/// 1 回の編集の restore の呼び方 (アーカイブごとに 1 回)。 順番は ARA の partial persistency の規約
/// (`ARAInterface.h` "Document Persistency" / `ARAStoreObjectsFilter::documentData`):
///
/// 1. 保存したアーカイブの object。
/// 2. audio source を restore する外のアーカイブ ("each call that restores some audio source state must either
///    include or precede restoring the state of any audio modification associated with the affected audio source")。
/// 3. 残りの外のアーカイブ。 2 のアーカイブの modification でも、その source を restore するのが後の呼び出しなら、
///    ここへ回す (source の後に restore する)。
/// 4. `document_data` (保存したアーカイブから document を組む編集) なら最後に保存したアーカイブの document data
///    ("the partial archive which was saved with documentData == kARATrue is restored as last archive in the restore
///    cycle, where the graph has its final structure and all object states are available")。 呼び出しが 1 の 1 回
///    だけならそこへ畳む。 restore する object が無くても呼ぶ (document の private な状態は object と別)。
#[must_use]
pub fn restore_calls<'a>(restores: &Restores<'a>, document_data: bool) -> Vec<RestoreCall<'a>> {
    let mut order: Vec<Option<usize>> = Vec::new();
    let saved_sources = restores.sources.iter().filter(|s| s.0.is_none()).map(|s| s.0);
    let saved_modifications = restores.modifications.iter().filter(|m| m.0.is_none()).map(|m| m.0);
    let kept_sources = restores.sources.iter().filter(|s| s.0.is_some()).map(|s| s.0);
    for archive in saved_sources.chain(saved_modifications).chain(kept_sources) {
        if !order.contains(&archive) {
            order.push(archive);
        }
    }
    // 各 source を restore する呼び出しの位置 (無ければ document に既に居るか、状態を restore しない source)。
    let source_call = |source: &str| {
        restores.sources.iter().find(|s| s.2 == source).and_then(|s| order.iter().position(|&a| a == s.0))
    };
    let mut calls: Vec<RestoreCall<'a>> = order
        .iter()
        .map(|&archive| RestoreCall {
            archive,
            document_data: false,
            sources: restores.sources.iter().filter(|s| s.0 == archive).map(|s| (s.1, s.2)).collect(),
            modifications: Vec::new(),
        })
        .collect();
    let source_calls = calls.len();
    for &(archive, archived, current, source) in &restores.modifications {
        let at = order.iter().position(|&a| a == archive).filter(|&i| source_call(source).is_none_or(|s| s <= i));
        let at = at.unwrap_or_else(|| {
            calls[source_calls..].iter().position(|c| c.archive == archive).map(|i| source_calls + i).unwrap_or_else(|| {
                calls.push(RestoreCall { archive, document_data: false, sources: Vec::new(), modifications: Vec::new() });
                calls.len() - 1
            })
        });
        calls[at].modifications.push((archived, current));
    }
    if document_data {
        match calls.as_mut_slice() {
            [only] if only.archive.is_none() => only.document_data = true,
            _ => calls.push(RestoreCall { archive: None, document_data: true, sources: Vec::new(), modifications: Vec::new() }),
        }
    }
    calls
}

fn first_with(specs: &[AraClipSpec], pred: impl Fn(&AraClipSpec) -> bool) -> Option<&AraClipSpec> {
    specs.iter().find(|s| pred(s))
}

fn gone<'a>(ids: impl Iterator<Item = &'a str>, kept: &HashSet<&str>) -> HashSet<String> {
    ids.filter(|id| !kept.contains(id)).map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::protocol::AraRegionPlacement;
    use std::path::{Path, PathBuf};

    fn spec(source: &str, modification: &str, region: &str) -> AraClipSpec {
        AraClipSpec {
            source_wav: PathBuf::from(format!("C:/{source}.wav")),
            source_id: source.into(),
            modification_id: modification.into(),
            modification_origins: Vec::new(),
            region_key: region.into(),
            placement: AraRegionPlacement {
                start_in_playback_seconds: 0.0,
                duration_in_playback_seconds: 1.0,
                start_in_modification_seconds: 0.0,
                duration_in_modification_seconds: 1.0,
                time_stretch: false,
            },
        }
    }

    fn set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| (*s).to_owned()).collect()
    }

    /// 分割で region が 1 つ増えるだけなら、source と modification は残し (編集が続く)、元の region は置き方の
    /// 更新、増えた region だけを作る。
    #[test]
    fn 分割は_region_を足すだけで_modification_を作り直さない() {
        let wav = Path::new("C:/s.wav");
        let now = GraphNow { sources: vec![("s", wav)], modifications: vec![("m", "s")], regions: vec![("1.1", "m")] };
        let specs = [spec("s", "m", "1.1"), spec("s", "m", "2.9")];
        let p = plan(&now, &specs);
        assert_eq!(p, GraphPlan { create_regions: vec![1], update_regions: vec![0], ..GraphPlan::default() });
    }

    /// 対照: 旧実装の「全部作り直す」と区別できる — content を複製して take の id が変わると modification ごと
    /// 作り直し、依存する region も作り直す。 素材の WAV が変わったら source から作り直す。
    #[test]
    fn id_が変わった_object_は依存の順に作り直す() {
        let wav = Path::new("C:/s.wav");
        let now = GraphNow {
            sources: vec![("s", wav), ("old", Path::new("C:/old.wav"))],
            modifications: vec![("m", "s"), ("m-old", "old")],
            regions: vec![("1.1", "m"), ("3.1", "m-old")],
        };
        let specs = [spec("s", "m2", "1.1"), spec("t", "m3", "4.1")];
        let p = plan(&now, &specs);
        assert_eq!(p.remove_regions, set(&["1.1", "3.1"]));
        assert_eq!(p.destroy_modifications, set(&["m", "m-old"]));
        assert_eq!(p.destroy_sources, set(&["old"]), "同じ WAV の source は残す");
        assert_eq!((p.create_sources, p.create_modifications, p.create_regions), (vec![1], vec![0, 1], vec![0, 1]));

        let moved = GraphNow { sources: vec![("s", Path::new("C:/elsewhere.wav"))], modifications: vec![], regions: vec![] };
        assert_eq!(plan(&moved, &[spec("s", "m", "1.1")]).destroy_sources, set(&["s"]), "WAV が変わったら作り直す");
    }

    const HERE: ProjectKey = ProjectKey(1);
    const OTHER: ProjectKey = ProjectKey(2);
    const HERE_ID: u64 = 100;
    const OTHER_ID: u64 = 200;

    /// plug-in host 全体の状態の置き場の代わり: `(プロジェクト, id)` → 場所、クリップボードの `(project_id, id)`。
    #[derive(Default)]
    struct Kept {
        project: Vec<(ProjectKey, &'static str, KeptAt)>,
        clipboard: Vec<(u64, &'static str)>,
        /// document の無い device の保存したアーカイブにある `(device, id)`。
        dormant: Vec<(DeviceAddr, &'static str)>,
    }

    impl KeptStates for Kept {
        fn in_project(&self, project: ProjectKey, id: &str) -> Option<KeptAt> {
            self.project.iter().find(|(p, i, _)| *p == project && *i == id).map(|(_, _, at)| *at)
        }
        fn in_clipboard(&self, project_id: u64, id: &str) -> bool {
            self.clipboard.iter().any(|(p, i)| *p == project_id && *i == id)
        }
        fn in_dormant(&self, project: ProjectKey, id: &str) -> Option<KeptAt> {
            self.dormant.iter().find(|(d, i)| d.project == project && *i == id).map(|(d, _)| KeptAt::Dormant(*d))
        }
    }

    fn copied(own: &str, origins: &[(Option<ProjectKey>, u64, &str)]) -> AraClipSpec {
        let modification_origins = origins
            .iter()
            .map(|&(project, project_id, id)| common::protocol::AraModificationOrigin { project, project_id, modification_id: id.into() })
            .collect();
        AraClipSpec { modification_origins, ..spec("s", own, "2.9") }
    }

    fn doc<'a>(first_build: bool, saved: &'a [AraArchiveEntry], surviving: &'a HashSet<String>) -> DocumentNow<'a> {
        DocumentNow { project: HERE, first_build, saved, surviving }
    }

    fn kept(at: KeptAt, id: &str) -> ModificationStart {
        ModificationStart::Kept { at, id: id.into() }
    }

    const LIVE_B: KeptAt = KeptAt::Live(DeviceAddr { project: HERE, device_id: 7 });

    /// 写した take は、元がこの document に残るなら clone、別の document に居ればその今の状態、消えていれば取っておいた
    /// 状態 / クリップボードの写し / 保存したアーカイブ (近い元から順に、見つかったところで止まる)。
    #[test]
    fn 写した_modification_は元の編集から始める() {
        let none = HashSet::new();
        let unique = copied("m2", &[(Some(HERE), HERE_ID, "m")]);
        assert_eq!(modification_start(&unique, doc(false, &[], &set(&["m"])), &Kept::default()), ModificationStart::Clone("m".into()));
        let elsewhere = Kept { project: vec![(HERE, "m", LIVE_B)], clipboard: vec![(HERE_ID, "m")], ..Kept::default() };
        assert_eq!(modification_start(&unique, doc(false, &[], &none), &elsewhere), kept(LIVE_B, "m"), "別のトラックの document");
        let clipboard = Kept { clipboard: vec![(HERE_ID, "m")], ..Kept::default() };
        assert_eq!(modification_start(&unique, doc(false, &[], &none), &clipboard), kept(KeptAt::Clipboard, "m"));
        let saved = [AraArchiveEntry { current: "m".into(), archived: Some("legacy/mod".into()) }];
        assert_eq!(
            modification_start(&unique, doc(false, &saved, &none), &Kept::default()),
            ModificationStart::Saved("legacy/mod".into()),
            "どこにも居なければ保存したアーカイブ (目次の書かれている id)"
        );
        assert_eq!(modification_start(&unique, doc(false, &[], &none), &Kept::default()), ModificationStart::Empty);

        let grandchild = copied("m3", &[(Some(HERE), HERE_ID, "m2"), (Some(HERE), HERE_ID, "m")]);
        assert_eq!(
            modification_start(&grandchild, doc(false, &[], &none), &elsewhere),
            kept(LIVE_B, "m"),
            "近い元 (ARA トラックに載らなかった複製) に状態が無ければ、さらにその元"
        );
    }

    /// 自分の状態がどこかにあれば、写した元より先にそれを使う: 移した take は元の document の今の状態、undo / redo で
    /// 戻る take は取っておいた状態。 開き直した document は保存した自分の状態 (別のトラックの今の状態で上書きしない)。
    #[test]
    fn 自分の状態は写した元より先に使う() {
        let surviving = set(&["m"]);
        let unique = copied("m2", &[(Some(HERE), HERE_ID, "m")]);
        let own_live = Kept { project: vec![(HERE, "m2", LIVE_B)], ..Kept::default() };
        assert_eq!(modification_start(&unique, doc(false, &[], &surviving), &own_live), kept(LIVE_B, "m2"));
        let own_retired = Kept { project: vec![(HERE, "m2", KeptAt::Retired(HERE))], ..Kept::default() };
        assert_eq!(modification_start(&unique, doc(false, &[], &surviving), &own_retired), kept(KeptAt::Retired(HERE), "m2"));

        let saved = [AraArchiveEntry::stored("m2".into())];
        assert_eq!(
            modification_start(&unique, doc(true, &saved, &surviving), &own_live),
            ModificationStart::Saved("m2".into()),
            "開き直した document は保存した自分の状態"
        );
        assert_eq!(
            modification_start(&unique, doc(false, &saved, &surviving), &own_live),
            kept(LIVE_B, "m2"),
            "作業中は最後に保存した時点より今の状態"
        );
        assert_eq!(
            modification_start(&unique, doc(false, &saved, &surviving), &Kept::default()),
            ModificationStart::Saved("m2".into()),
            "今の状態が無ければ保存した自分の状態 (写した元より先)"
        );
        let origin_live = Kept { project: vec![(HERE, "m", LIVE_B)], ..Kept::default() };
        assert_eq!(
            modification_start(&unique, doc(true, &[AraArchiveEntry::stored("m".into())], &HashSet::new()), &origin_live),
            kept(LIVE_B, "m"),
            "初めて組む document のアーカイブに無い take (目次を移し損ねた複製など) は写した元から"
        );
    }

    /// 別のプロジェクトから写した take は、クリップボードの写し (写した時点) を先に、そのプロジェクトが開いていればその中を
    /// 引く。 id は自分のプロジェクトの id と同じ文字列になり得るので、この document / 自分のプロジェクト / 保存した
    /// アーカイブでは引かない。
    #[test]
    fn 別のプロジェクトから写した_take_はそのプロジェクトの状態から始める() {
        let same_string = set(&["m"]);
        let saved = [AraArchiveEntry::stored("m".into())];
        let here_has_m = Kept { project: vec![(HERE, "m", LIVE_B)], ..Kept::default() };
        let pasted = copied("m9", &[(Some(OTHER), OTHER_ID, "m")]);
        assert_eq!(modification_start(&pasted, doc(false, &saved, &same_string), &here_has_m), ModificationStart::Empty);

        let other_live = KeptAt::Live(DeviceAddr { project: OTHER, device_id: 3 });
        let both = Kept { project: vec![(OTHER, "m", other_live)], clipboard: vec![(OTHER_ID, "m")], ..Kept::default() };
        assert_eq!(modification_start(&pasted, doc(false, &[], &HashSet::new()), &both), kept(KeptAt::Clipboard, "m"));
        let open = Kept { project: vec![(OTHER, "m", other_live)], ..Kept::default() };
        assert_eq!(modification_start(&pasted, doc(false, &[], &HashSet::new()), &open), kept(other_live, "m"));
        let closed = copied("m9", &[(None, OTHER_ID, "m")]);
        assert_eq!(modification_start(&closed, doc(false, &[], &HashSet::new()), &open), ModificationStart::Empty, "閉じたプロジェクトはクリップボードだけ");
        let dormant_there = Kept { dormant: vec![(DeviceAddr { project: OTHER, device_id: 3 }, "m")], ..Kept::default() };
        assert_eq!(
            modification_start(&pasted, doc(false, &[], &HashSet::new()), &dormant_there),
            kept(KeptAt::Dormant(DeviceAddr { project: OTHER, device_id: 3 }), "m"),
            "開いているタブの無効のトラックから"
        );
        assert_eq!(modification_start(&closed, doc(false, &[], &HashSet::new()), &dormant_there), ModificationStart::Empty);
        let dormant_here = Kept { dormant: vec![(DeviceAddr { project: HERE, device_id: 3 }, "m")], ..Kept::default() };
        assert_eq!(modification_start(&pasted, doc(false, &[], &HashSet::new()), &dormant_here), ModificationStart::Empty, "同じ文字列の自分のプロジェクトの id は引かない");
    }

    /// document の無い device (無効のトラック) の保存したアーカイブは、プロジェクトの中の今の状態・保存した自分の状態・
    /// クリップボードより後に引く: そこから移したクリップ (自分の id) と、そこから写した take (写した元の id)。
    #[test]
    fn document_の無い_device_の保存したアーカイブは今の状態と自分の保存より後に引く() {
        let none = HashSet::new();
        let disabled = KeptAt::Dormant(DeviceAddr { project: HERE, device_id: 5 });
        let moved = spec("s", "m", "1.1");
        let dormant = Kept { dormant: vec![(DeviceAddr { project: HERE, device_id: 5 }, "m")], ..Kept::default() };
        assert_eq!(modification_start(&moved, doc(false, &[], &none), &dormant), kept(disabled, "m"), "無効のトラックから移したクリップ");
        let saved = [AraArchiveEntry::stored("m".into())];
        assert_eq!(modification_start(&moved, doc(false, &saved, &none), &dormant), ModificationStart::Saved("m".into()));
        let live_too = Kept { project: vec![(HERE, "m", LIVE_B)], ..dormant };
        assert_eq!(modification_start(&moved, doc(false, &[], &none), &live_too), kept(LIVE_B, "m"));

        let copy = copied("m2", &[(Some(HERE), HERE_ID, "m")]);
        let dormant = Kept { dormant: vec![(DeviceAddr { project: HERE, device_id: 5 }, "m")], ..Kept::default() };
        assert_eq!(modification_start(&copy, doc(false, &[], &none), &dormant), kept(disabled, "m"), "無効のトラックから写した take");
        let clipboard_too = Kept { clipboard: vec![(HERE_ID, "m")], ..dormant };
        assert_eq!(modification_start(&copy, doc(false, &[], &none), &clipboard_too), kept(KeptAt::Clipboard, "m"));
    }

    fn call<'a>(archive: Option<usize>, document_data: bool, sources: &[(&'a str, &'a str)], modifications: &[(&'a str, &'a str)]) -> RestoreCall<'a> {
        RestoreCall { archive, document_data, sources: sources.to_vec(), modifications: modifications.to_vec() }
    }

    /// 保存したアーカイブの object → audio source を運ぶ外のアーカイブ (その source の modification より先) → 残りの外の
    /// アーカイブ → 保存したアーカイブの document data の順。 同じアーカイブの object は 1 回にまとめる。
    #[test]
    fn restore_は保存したアーカイブ_source_を運ぶアーカイブ_残り_document_data_の順() {
        let restores = Restores {
            sources: vec![(None, "s", "s"), (Some(1), "a.src", "t")],
            modifications: vec![
                (Some(0), "m0", "x", "s"),
                (None, "legacy/mod", "m", "s"),
                (Some(1), "a.mod", "y", "t"),
                (Some(0), "m1", "z", "old"),
            ],
        };
        assert_eq!(
            restore_calls(&restores, true),
            vec![
                call(None, false, &[("s", "s")], &[("legacy/mod", "m")]),
                call(Some(1), false, &[("a.src", "t")], &[("a.mod", "y")]),
                call(Some(0), false, &[], &[("m0", "x"), ("m1", "z")]),
                call(None, true, &[], &[]),
            ]
        );
        assert_eq!(restore_calls(&restores, false).len(), 3, "document を組む編集でなければ document data は読まない");
    }

    /// source を運ぶアーカイブが、後の呼び出しで source を restore する別の source の modification も持つなら、その
    /// modification は source の後の呼び出しへ回す。
    #[test]
    fn source_より先に_modification_を_restore_しない() {
        let restores = Restores {
            sources: vec![(Some(0), "v", "v"), (Some(1), "u", "u")],
            modifications: vec![(Some(0), "on-v", "a", "v"), (Some(0), "on-u", "b", "u"), (Some(1), "also-u", "c", "u")],
        };
        assert_eq!(
            restore_calls(&restores, false),
            vec![
                call(Some(0), false, &[("v", "v")], &[("on-v", "a")]),
                call(Some(1), false, &[("u", "u")], &[("also-u", "c")]),
                call(Some(0), false, &[], &[("on-u", "b")]),
            ]
        );
    }

    /// document data は、保存したアーカイブの呼び出しだけならそこへ畳み、restore する object が無くても組む編集では読む
    /// (空の document で保存したアーカイブにも plug-in の private な状態がある)。
    #[test]
    fn document_data_は畳めるときは畳み_object_が無くても読む() {
        let saved_only = Restores { sources: vec![(None, "s", "s")], modifications: Vec::new() };
        assert_eq!(restore_calls(&saved_only, true), vec![call(None, true, &[("s", "s")], &[])]);
        assert_eq!(restore_calls(&Restores::default(), true), vec![call(None, true, &[], &[])]);
        assert!(restore_calls(&Restores::default(), false).is_empty());
        let kept_only = Restores { sources: Vec::new(), modifications: vec![(Some(0), "m", "m2", "s")] };
        assert_eq!(restore_calls(&kept_only, true), vec![call(Some(0), false, &[], &[("m", "m2")]), call(None, true, &[], &[])]);
    }
}
