//! r.md #131 トラックの無効化 (`docs/plan_rmd_131_track_disable.md`)。
//!
//! 無効なトラックは「プロジェクトには残っているが実行系からは消えている」。音声 (レンダーグラフ /
//! 素材の常駐) ・映像・プラグインの host 常駐・VOICEVOX 合成・書き出しがすべて
//! [`Song::track_effectively_enabled`] **1 本**を読んで外す。group を無効にすると子も実効的に無効
//! (子自身の `Track::enabled` は別に保つので、group を戻すと個別に無効だった子は無効のまま)。

use std::collections::HashMap;

use super::*;

/// `track_id` から `parent_group_id` を辿り、自分と祖先が全部 `enabled` か。
/// 自分が居なければ無効、途中の親が居なければそこで打ち切る (dangling な親は根と同じ)。
/// 循環は `cap` 回で打ち切る (`track_visually_silenced` の走査と同形)。
fn lineage_enabled<'a>(track_id: u32, cap: usize, lookup: impl Fn(u32) -> Option<&'a Track>) -> bool {
    let mut cur = Some(track_id);
    let mut hops = 0usize;
    while let Some(id) = cur {
        let Some(t) = lookup(id) else {
            return hops > 0;
        };
        if !t.enabled {
            return false;
        }
        if hops > cap {
            break;
        }
        cur = t.parent_group_id;
        hops += 1;
    }
    true
}

impl Song {
    /// トラックが **実効的に有効か** (= 自分と祖先 group が全部 `enabled`)。無効化の判定の唯一の口 —
    /// 音声 / 映像 / host 同期 / 表示が全部ここを読む。master (`MASTER_TRACK_ID`) は無効化できないので
    /// 常に `true`、存在しないトラックは `false`。
    #[must_use]
    pub fn track_effectively_enabled(&self, track_id: u32) -> bool {
        if track_id == MASTER_TRACK_ID {
            return true;
        }
        lineage_enabled(track_id, self.tracks.len(), |id| self.track_by_id(id))
    }

    /// song-track index 順の [`Self::track_effectively_enabled`]。compile / 索引のように全トラックを
    /// 1 回で引く口 (トラックごとに線形探索しない)。同じ id が複数あれば先頭を親として引く
    /// (`track_by_id` と同じ答え)。
    #[must_use]
    pub fn effectively_enabled_mask(&self) -> Vec<bool> {
        let mut by_id: HashMap<u32, &Track> = HashMap::with_capacity(self.tracks.len());
        for t in &self.tracks {
            by_id.entry(t.id).or_insert(t);
        }
        let cap = self.tracks.len();
        self.tracks.iter().map(|t| lineage_enabled(t.id, cap, |id| by_id.get(&id).copied())).collect()
    }

    /// **実行系に居るべき** plugin = 実効的に有効なトラックと master の全 plugin ([`Self::all_plugins`] と同じ
    /// 並びの部分列、Parallel の中も含む)。plugin host に載せる device の導出 / 合成・解析の待ち合わせはこちらを使う
    /// (無効トラックの plugin は host から降ろしてあり、依頼しても誰も答えない)。
    pub fn live_plugins(&self) -> impl Iterator<Item = &PluginInstance> {
        self.tracks
            .iter()
            .zip(self.effectively_enabled_mask())
            .filter(|&(_, on)| on)
            .flat_map(|(t, _)| plugins(&t.devices))
            .chain(plugins(&self.master_fx_chain))
    }

    /// このトラックの solo が solo の判定に数えられるか (実効的に無効なトラックの solo は他を黙らせない)。
    #[must_use]
    pub fn solo_counts(&self, track: &Track) -> bool {
        track.solo && self.track_effectively_enabled(track.id)
    }

    /// 変調ソースが評価されるか: バイパスされておらず (`ModSource::enabled`)、帰属トラックが実効的に
    /// 有効 (master 帰属 / legacy `0` は常に有効)。無効トラックが持つ変調ソースは評価しない。
    #[must_use]
    pub fn mod_source_active(&self, source: &ModSource) -> bool {
        source.enabled && (source.owner_track_id == 0 || self.track_effectively_enabled(source.owner_track_id))
    }

    /// トラックを無効 / 有効にする (master と存在しない id は無視)。戻り値 = 1 つでも変わったか。
    ///
    /// 無効にしたときは、実効的に無効になった全トラック (group の子を含む) の **実行系の状態を降ろす**:
    /// 録音待機を解除し、ランチャーで鳴っているセル (トラック行とそのレーン行) を停止へ落とす。
    /// 有効に戻しても待機 / セルは戻らない (再生中に戻したセルが勝手に鳴り出さない)。
    pub fn set_tracks_enabled(&mut self, track_ids: &[u32], enabled: bool) -> bool {
        let mut changed = false;
        for t in self.tracks.iter_mut().filter(|t| track_ids.contains(&t.id)) {
            if t.enabled != enabled {
                t.enabled = enabled;
                changed = true;
            }
        }
        if changed && !enabled {
            self.settle_disabled_tracks();
        }
        changed
    }

    /// 実効的に無効なトラックの録音待機とランチャーの鳴っているセルを降ろす (冪等)。
    fn settle_disabled_tracks(&mut self) {
        let stop = |row: &mut RowPlayback| {
            if matches!(row, RowPlayback::Launcher { .. }) {
                *row = RowPlayback::LauncherStopped;
            }
        };
        let mask = self.effectively_enabled_mask();
        for (t, on) in self.tracks.iter_mut().zip(mask) {
            if on {
                continue;
            }
            t.armed = false;
            stop(&mut t.launcher);
            for lane in &mut t.automation_lanes {
                stop(&mut lane.launcher);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(id: u32, parent: Option<u32>) -> Track {
        Track { id, parent_group_id: parent, ..Track::default() }
    }

    /// group (10) の子 11 と、11 の子 12、独立 13。
    fn song() -> Song {
        Song { tracks: vec![mk(10, None), mk(11, Some(10)), mk(12, Some(11)), mk(13, None)], ..Default::default() }
    }

    fn mask_of(song: &Song) -> Vec<bool> {
        song.tracks.iter().map(|t| song.track_effectively_enabled(t.id)).collect()
    }

    #[test]
    fn group_を無効にすると子孫も実効的に無効で戻すと個別の無効は残る() {
        let mut s = song();
        assert_eq!(mask_of(&s), vec![true, true, true, true]);
        // 子 12 を個別に無効 → 12 だけ。
        assert!(s.set_tracks_enabled(&[12], false));
        assert_eq!(mask_of(&s), vec![true, true, false, true]);
        // group 10 を無効 → 10 の子孫全部。13 は無関係。
        assert!(s.set_tracks_enabled(&[10], false));
        assert_eq!(mask_of(&s), vec![false, false, false, true]);
        assert_eq!(s.effectively_enabled_mask(), mask_of(&s), "mask と 1 本の判定は同じ答え");
        // group を戻す → 個別に無効だった 12 は無効のまま。
        assert!(s.set_tracks_enabled(&[10], true));
        assert_eq!(mask_of(&s), vec![true, true, false, true]);
        // 変化しない指定は false。master / 不在は無視。
        assert!(!s.set_tracks_enabled(&[10, MASTER_TRACK_ID, 999], true));
        assert!(s.track_effectively_enabled(MASTER_TRACK_ID));
        assert!(!s.track_effectively_enabled(999));
    }

    #[test]
    fn 無効化は録音待機とランチャーの鳴っているセルを降ろす() {
        let mut s = song();
        for t in &mut s.tracks {
            t.armed = true;
            t.launcher = RowPlayback::Launcher { clip_id: 1 };
            t.automation_lanes.push(AutomationLane {
                id: 1,
                launcher: RowPlayback::Launcher { clip_id: 2 },
                ..AutomationLane::new(AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume), 1.0)
            });
        }
        s.set_tracks_enabled(&[11], false);
        for t in &s.tracks {
            let off = matches!(t.id, 11 | 12);
            assert_eq!(t.armed, !off, "track {}", t.id);
            let want = if off { RowPlayback::LauncherStopped } else { RowPlayback::Launcher { clip_id: 1 } };
            assert_eq!(t.launcher, want, "track {}", t.id);
            let lane_want = if off { RowPlayback::LauncherStopped } else { RowPlayback::Launcher { clip_id: 2 } };
            assert_eq!(t.automation_lanes[0].launcher, lane_want, "lane of track {}", t.id);
        }
        // 戻しても待機とセルは戻らない。
        s.set_tracks_enabled(&[11], true);
        assert!(!s.track_by_id(11).unwrap().armed);
        assert_eq!(s.track_by_id(12).unwrap().launcher, RowPlayback::LauncherStopped);
    }

    #[test]
    fn 無効トラックは映像で隠れ_solo_にも数えず_変調ソースも評価しない() {
        let mut s = song();
        s.track_by_id_mut(13).unwrap().solo = true;
        assert!(s.track_visually_silenced(10), "13 の solo で他は隠れる");
        s.set_tracks_enabled(&[13], false);
        assert!(s.track_visually_silenced(13), "無効トラック自身は隠れる");
        assert!(!s.track_visually_silenced(10), "無効トラックの solo は他を隠さない");
        // 無効な子 12 の solo は祖先 group を solo 可視にしない。
        s.set_tracks_enabled(&[13], true);
        s.track_by_id_mut(13).unwrap().solo = false;
        s.track_by_id_mut(12).unwrap().solo = true;
        s.track_by_id_mut(12).unwrap().enabled = false;
        assert!(!s.track_visually_silenced(10), "数える solo が無いので誰も隠れない");

        let src = |owner: u32| ModSource { id: 1, owner_track_id: owner, color: [0.0; 3], kind: ModSourceKind::default(), enabled: true };
        assert!(!s.mod_source_active(&src(12)));
        assert!(s.mod_source_active(&src(11)));
        assert!(s.mod_source_active(&src(MASTER_TRACK_ID)));
        assert!(!s.mod_source_active(&ModSource { enabled: false, ..src(11) }));
    }

    #[test]
    fn enabled_は既定では書かず_旧ファイルは有効で読める() {
        let t = mk(5, None);
        let json = serde_json::to_value(&t).unwrap();
        assert!(json.get("enabled").is_none(), "有効 (既定) は書かない");
        let back: Track = serde_json::from_value(json).unwrap();
        assert!(back.enabled, "欠けていれば有効");
        let off = Track { enabled: false, ..mk(5, None) };
        let json = serde_json::to_value(&off).unwrap();
        assert_eq!(json.get("enabled"), Some(&serde_json::Value::Bool(false)));
        assert_eq!(serde_json::from_value::<Track>(json).unwrap(), off);
    }
}
