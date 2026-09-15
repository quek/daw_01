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

use common::protocol::AraClipSpec;

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

/// 新しく作る modification の中身をどこから始めるか。
#[derive(Debug, PartialEq, Eq)]
pub enum ModificationStart<'a> {
    /// この編集の前から document に居る元の modification (content を複製した元) を `cloneAudioModification` で
    /// 写す (今の編集)。
    Clone(&'a str),
    /// 空で作り、この session が id `.0` の modification を destroy した時点の状態 (partial archive) を restore する。
    Retired(&'a str),
    /// 空で作り、保存したアーカイブに id `.0` で書かれた状態を restore する (無ければ空のまま)。
    Saved(&'a str),
}

/// `spec` の modification の始め方。
///
/// 1. この session で destroy した同じ id の modification があれば、その destroy した時点の状態 (undo / redo で
///    戻る object。 保存したアーカイブ = 最後に保存した時点より新しい。 複製元を写し直すと、共有を解いた後の
///    自分の編集が複製元の編集で上書きされる)。
/// 2. document を初めて組むとき (`first_build` = 開いた直後) は、アーカイブがこの id で書かれているので
///    保存した自分の状態。
/// 3. 途中で増えた、共有を解いた content の modification は、複製元が **この編集の前から** document に居れば
///    (`origin_live`) その今の編集を写し、居なければ複製元を destroy した時点の状態、それも無ければ複製元の
///    保存した状態。 同じ編集で作る複製元を写すと、restore の前の空の modification を写してしまう。
/// 4. それ以外は保存した自分の状態。
#[must_use]
pub fn modification_start<'a>(
    spec: &'a AraClipSpec,
    first_build: bool,
    origin_live: bool,
    retired: impl Fn(&str) -> bool,
) -> ModificationStart<'a> {
    let own = spec.modification_id.as_str();
    match spec.modification_origin.as_deref() {
        _ if retired(own) => ModificationStart::Retired(own),
        Some(origin) if !first_build && origin_live => ModificationStart::Clone(origin),
        Some(origin) if !first_build && retired(origin) => ModificationStart::Retired(origin),
        Some(origin) if !first_build => ModificationStart::Saved(origin),
        _ => ModificationStart::Saved(own),
    }
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
            modification_origin: None,
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

    /// 共有を解いた (content を複製した) modification は、作業中なら元の今の編集を写し、開いた直後は保存した
    /// 自分の状態から始める (保存したアーカイブはこの id で書かれている)。
    #[test]
    fn 複製した_modification_は元の編集から始める() {
        let none = |_: &str| false;
        let unique = AraClipSpec { modification_origin: Some("m".into()), ..spec("s", "m2", "2.9") };
        assert_eq!(modification_start(&unique, false, true, none), ModificationStart::Clone("m"));
        assert_eq!(modification_start(&unique, false, false, none), ModificationStart::Saved("m"), "元が消えていたら元の保存した状態");
        assert_eq!(
            modification_start(&unique, false, false, |id| id == "m"),
            ModificationStart::Retired("m"),
            "同じ編集で元も消えたなら、元を destroy した時点の状態"
        );
        assert_eq!(modification_start(&unique, true, true, none), ModificationStart::Saved("m2"), "開いた直後は自分の保存した状態");
        assert_eq!(modification_start(&spec("s", "m", "1.1"), false, false, none), ModificationStart::Saved("m"));
    }

    /// この session で destroy した modification を作り直す (undo / redo) ときは、destroy した時点の自分の状態から
    /// 始める — 共有を解いた content でも複製元を写し直さない (写し直すと、共有を解いた後の自分の編集が消える)。
    #[test]
    fn 作り直す_modification_は_destroy_した時点の自分の状態から始める() {
        let retired = |id: &str| id == "m2";
        let unique = AraClipSpec { modification_origin: Some("m".into()), ..spec("s", "m2", "2.9") };
        assert_eq!(modification_start(&unique, false, true, retired), ModificationStart::Retired("m2"));
        assert_eq!(modification_start(&spec("s", "m2", "1.1"), false, false, retired), ModificationStart::Retired("m2"));
    }
}
