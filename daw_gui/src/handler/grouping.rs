//! handler::grouping — track のグループ化/解除/親子付け/末尾削除
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use common::model::InstrumentSource;
use common::protocol::{AudioCommand, PluginCommand};

impl AppData {
    pub(crate) fn action_group_selected_tracks(&mut self, track_ids: &[u32]) {
        if track_ids.is_empty() {
            tracing::info!("group request ignored: empty selection");
            return;
        }
        // De-duplicate while preserving the first-appearance order.
        let mut child_ids: Vec<u32> = Vec::with_capacity(track_ids.len());
        for &id in track_ids {
            if !child_ids.contains(&id) {
                child_ids.push(id);
            }
        }
        // Validate all ids exist before mutating anything.
        if child_ids.iter().any(|id| self.song_doc.song().track_by_id(*id).is_none()) {
            tracing::warn!(?child_ids, "group request: stale track id, abort");
            return;
        }
        // selection-root rule。 選択集合のうち、
        // 親 (`parent_group_id`) が同じ選択集合に **含まれていない** トラック
        // (= 最上位) だけを新グループへ付け替える。 グループとその子を一緒に
        // 選んだ場合、 子は元のグループに残り、 内側グループの階層が平坦化しない
        // (= 元グループが解除されない)。
        let selected: std::collections::HashSet<u32> = child_ids.iter().copied().collect();
        let roots: Vec<u32> = child_ids
            .iter()
            .copied()
            .filter(|id| {
                self.song_doc.song()
                    .track_by_id(*id)
                    .and_then(|t| t.parent_group_id)
                    .is_none_or(|pid| !selected.contains(&pid))
            })
            .collect();
        if roots.is_empty() {
            return;
        }
        // 仕様 §4: 「選択トラックのうち、 index が最も小さいものの
        // 直前」 に新グループを挿入 (= 一番上の選択 track の上)。
        // Live 互換、 視覚的には「子の上にヘッダー行」。
        let top_child_idx = child_ids
            .iter()
            .filter_map(|id| self.song_doc.song().track_index_by_id(*id))
            .min()
            .unwrap_or(self.song_doc.song().tracks.len());
        // Inherit the common parent of the selection if every selected
        // track shared the same `parent_group_id` — preserves Live's
        // behaviour of grouping inside a group keeps you in the parent.
        let common_parent = {
            let first_parent = self
                .song_doc.song()
                .track_by_id(roots[0])
                .and_then(|t| t.parent_group_id);
            if roots.iter().all(|id| {
                self.song_doc.song()
                    .track_by_id(*id)
                    .and_then(|t| t.parent_group_id)
                    == first_parent
            }) {
                first_parent
            } else {
                None
            }
        };
        let Some(group_id) = self.edit_song(|song| song.alloc_track_id()) else {
            return;
        };
        let group_index = self.song_doc.song().tracks.len() + 1;
        let group_track = track_with(|t| {
            t.id = group_id;
            t.name = format!("Group {group_index}");
            // Reaper folder model: a "group" is just a track that has
            // children. No dedicated kind enum — once the children's
            // `parent_group_id` is repointed below, this track auto-
            // matically becomes a group bus to the engine.
            t.parent_group_id = common_parent;
            t.source = InstrumentSource::None;
            t.clips = Vec::new();
        });
        // Repoint every selection-root track's parent to the new group.
        // 子孫 (= 親が選択集合内のトラック) は元の親に残すことで、 内側
        // グループの入れ子が保たれる。
        for &cid in &roots {
            self.edit_song(|song| {
                if let Some(t) = song.track_by_id_mut(cid) {
                    t.parent_group_id = Some(group_id);
                }
            });
        }
        // 仕様 §4: 「一番上の選択 track の直前」 に挿入 (= 子の上に
        // ヘッダー)。 device のアドレスは安定 `device_id` 一本なので、
        // Vec::insert で既存 track の Vec position が shift しても
        // plugin の lookup は壊れない。
        let insert_at = top_child_idx.min(self.song_doc.song().tracks.len());
        self.edit_song(|song| song.tracks.insert(insert_at, group_track));
        // 新規 group track を選択状態に (Live 互換: グループ化直後は
        // 親 group が selection cursor になる)。 明示的なトラック面操作なので
        // last-wins タグも Tracks に倒す。
        self.set_track_selection(vec![group_id]);
        self.resize_track_peak_display();
        tracing::info!(group_id, ?child_ids, "grouped tracks");
    }

    /// `action_ungroup_tracks` / `delete_track` で送る IPC 列を組み立てる
    /// pure function。 順序が必須仕様 (deadlock 防止) なので、 ロジックを
    /// ここに集約して unit test で検証する:
    ///
    /// 1. `audio: ClosePluginShmem(device_id)` × N — 削除対象 track が
    ///    持っていた全 device について先に audio engine に送る。 これに
    ///    より plugin_refs から stale entry が消え、
    ///    audio worker が destroyed plugin に dispatch する race を断つ。
    ///    (踏むと `pd.prepare()` が unmapped shmem を触って audio worker が
    ///    AV で silent terminate → `all_done` 永久 wait)。
    /// 2. `plugin_host: RemoveSlotPlugin(device_id)` × N — plugin_host が
    ///    `Box<Plugin>` を properly tear down (stop_processing →
    ///    deactivate → gui_destroy → drop) して、 shmem mapping を
    ///    unmap する。 (1) で audio 側はもう触らないので安全。
    ///
    /// **列挙元は Song** (帳簿 `loaded_devices` ではない) — load 応答待ちの
    /// device を取りこぼさないため。 `fx_chain_by_track_id` は **track を Song から
    /// 外す前** にしか引けないので、 呼び出し側は削除より前にこれを呼んで戻り値を
    /// 保持し、 送信は従来どおり song update / `LoadSong` の後に行う。
    ///
    /// r.md #71 (プラグインのコピー / 移動): 単位は device。 host 側に track という
    /// 概念は無い (帰属を二重所有すると device 移動で stale になる)。
    pub fn plan_track_removal_ipc(
        song: &common::model::Song,
        track_ids: &[u32],
    ) -> Vec<TrackRemovalIpc> {
        let mut plan = Vec::new();
        for &track_id in track_ids {
            let ids: Vec<u64> = song
                .fx_chain_by_track_id(track_id)
                .map(|c| c.iter().map(|d| d.id).collect())
                .unwrap_or_default();
            // (1) audio engine から先に mapping を落とす (use-after-free deadlock 防止)。
            for &device_id in &ids {
                plan.push(TrackRemovalIpc::CloseAudioShmem { device_id });
            }
            // (2) plugin_host に device 単位で teardown させる。
            for device_id in ids {
                plan.push(TrackRemovalIpc::RemoveHostDevice { device_id });
            }
        }
        plan
    }

    /// [`Self::plan_track_removal_ipc`] が組んだ列を実際に送る。 順序は plan が
    /// 決めているので、 ここは変換して流すだけ (分岐を増やさない)。
    pub(crate) fn send_track_removal_ipc(&mut self, plan: &[TrackRemovalIpc]) {
        for step in plan {
            match *step {
                TrackRemovalIpc::CloseAudioShmem { device_id } => {
                    self.send_audio(AudioCommand::ClosePluginShmem { device_id });
                }
                TrackRemovalIpc::RemoveHostDevice { device_id } => {
                    self.send_plugin(PluginCommand::RemoveSlotPlugin { device_id });
                }
            }
        }
    }

    /// track を消す 3 経路 (選択トラック削除 / ungroup / 最終 track 削除) が共通で
    /// 使う daw_gui ローカル帳簿の掃除。 対象 device は **削除前の Song から**
    /// 列挙した id 集合で渡す (帳簿から引くと load 応答待ちを取りこぼす)。
    pub(crate) fn forget_removed_track_devices(&mut self, plan: &[TrackRemovalIpc]) {
        for step in plan {
            if let TrackRemovalIpc::RemoveHostDevice { device_id } = *step {
                self.cleanup_slot_gui(device_id);
                self.forget_device_caches(device_id);
                self.ipc.pending_plugin_loads.remove(&device_id);
                self.ipc.failed_plugin_loads.remove(&device_id);
            }
        }
    }

    /// Alt+G: 選択中の group track の subtree を 1 階層持ち上げる。
    /// 仕様 §5: 子の `parent_group_id` を group の親 (master or 上位
    /// group) に向ける + group track 自体を削除。 group の `fx_chain`
    /// は失われる (Live 仕様)。 複数 group が選択されているときは深い
    /// (子) → 浅い (親) の順に処理してインデックスを安定させる。
    /// `AppEvent::UngroupTracks` の dispatcher。 group track を ungroup
    /// すると group の `fx_chain` が削除されるため、 [`delete_track`] と
    /// 同様 plugin の最新 state を取ってから Undo snapshot を取って実行
    /// する。
    pub(crate) fn action_ungroup_tracks(&mut self, track_ids: &[u32]) {
        if track_ids.is_empty() {
            return;
        }
        if !self.song_has_plugin() {
            self.action_ungroup_tracks_inner(track_ids);
            return;
        }
        self.enqueue_state_request(PendingStateRequest::Deferred(
            DeferredEdit::UngroupTracks {
                track_ids: track_ids.to_vec(),
            },
        ));
    }

    pub(crate) fn action_ungroup_tracks_inner(&mut self, track_ids: &[u32]) {
        if track_ids.is_empty() {
            return;
        }
        // 選択された track の中から「実際に子を持つ」ものだけ ungroup
        // 対象。 通常 track が選択に混じっていても無視。
        let mut groups_to_ungroup: Vec<u32> = track_ids
            .iter()
            .copied()
            .filter(|id| self.is_group_track(*id))
            .collect();
        if groups_to_ungroup.is_empty() {
            tracing::info!(
                ?track_ids,
                "ungroup request: no group track in selection, ignored"
            );
            return;
        }
        // 深さ降順 (子から先に処理)。 同階層なら index 大きい方から。
        groups_to_ungroup.sort_by_key(|id| {
            let depth = self
                .song_doc.song()
                .track_by_id(*id)
                .map(|t| self.compute_track_depth(t))
                .unwrap_or(0);
            (-(depth as i32), -(self.song_doc.song().track_index_by_id(*id).unwrap_or(0) as i32))
        });

        // 削除する group の device teardown IPC を **Song から外す前に** 組む
        // (`fx_chain_by_track_id` は削除前の Song からしか引けない。 後から呼ぶと
        // plan が空になり IPC が 1 通も出ない = 無言で壊れる)。
        let removal_plan =
            Self::plan_track_removal_ipc(self.song_doc.song(), &groups_to_ungroup);

        let mut new_selection: Vec<u32> = Vec::new();
        for group_id in &groups_to_ungroup {
            let Some(group_track) = self.song_doc.song().track_by_id(*group_id) else {
                continue;
            };
            let new_parent = group_track.parent_group_id;
            self.edit_song(|song| {
                for t in &mut song.tracks {
                    if t.parent_group_id == Some(*group_id) {
                        t.parent_group_id = new_parent;
                        new_selection.push(t.id);
                    }
                }
            });
            if let Some(pos) = self.song_doc.song().tracks.iter().position(|t| t.id == *group_id) {
                self.edit_song(|song| song.tracks.remove(pos));
                // 消えた group track が所有していたモジュレーターと、その変調の深さを
                // 指していたレーン / 変調の後始末 (track 削除経路と同じ 1 本)。
                self.cleanup_modulation_after_track_removal();
            }
            self.ui_prefs.collapsed_groups.remove(group_id);
        }

        // **song update + LoadSong を先に送る** → daw_audio engine が
        // 新 schedule (group が消えた状態) を即適用。 audio thread が
        // 古い schedule の ProcessGroupFx で destroyed plugin にアクセス
        // する race を回避する。

        // **重要 (deadlock 防止)**: plugin_host が `tracks.mutate` で
        // chain の Box<Plugin> を drop すると `plugin_shmems.remove(&pid)`
        // で `ProcessDataHandle` も drop され、 OS が shmem mapping を
        // unmap する。 audio worker thread がその直後に `pd.prepare()`
        // で unmapped memory を読むと **access violation で worker が
        // silently terminate** し、 master の `WaitForSingleObject(all_done,
        // INFINITE)` が永久 wait → 18 秒 audio thread 完全停止。
        //
        // 対策: teardown を plugin_host に送る **前に** daw_audio に
        // 直接 ClosePluginShmem を送って `plugin_refs` から stale entry を
        // 削除させ、 audio worker が destroyed plugin を dispatch しないように
        // する。 順序は `plan_track_removal_ipc` が持っている。
        self.send_track_removal_ipc(&removal_plan);
        self.forget_removed_track_devices(&removal_plan);
        // selection: ungroup 後は元 group の子を選択 (Live 互換)。 明示的な
        // トラック面操作なので last-wins タグも Tracks に倒す。
        if !new_selection.is_empty() {
            self.set_track_selection(new_selection);
        }
        self.resize_track_peak_display();
        tracing::info!(?groups_to_ungroup, "ungrouped tracks");
    }

    /// Reparent `track_id` to `parent_id` (or detach to the master bus
    /// when `parent_id` is None). Any track is allowed as a parent
    /// (the "group" role is implicit — a track that has children).
    /// Validates the new parent chain doesn't contain `track_id`
    /// itself so the schedule compiler never sees a cyclic state.
    pub(crate) fn action_set_track_parent(&mut self, track_id: u32, parent_id: Option<u32>) {
        if Some(track_id) == parent_id {
            tracing::warn!(track_id, "ignored self-parent edit");
            return;
        }
        if let Some(pid) = parent_id {
            if self.song_doc.song().track_by_id(pid).is_none() {
                tracing::warn!(track_id, parent_id = pid, "ignored: parent track not found");
                return;
            }
            // Walk the parent's chain upward looking for `track_id`. If
            // we find it, the edit would create a cycle.
            let mut cursor = Some(pid);
            let mut hops = 0u32;
            while let Some(c) = cursor {
                if c == track_id {
                    tracing::warn!(track_id, parent_id = pid, "ignored: would create a cycle");
                    return;
                }
                hops += 1;
                if hops > self.song_doc.song().tracks.len() as u32 + 1 {
                    // Existing graph already has a cycle; abort to avoid an infinite loop.
                    tracing::error!("existing parent chain is cyclic; aborting reparent");
                    return;
                }
                cursor = self
                    .song_doc.song()
                    .track_by_id(c)
                    .and_then(|t| t.parent_group_id);
            }
        }
        let found = self.edit_song_checked(|song| {
            let Some(track) = song.track_by_id_mut(track_id) else {
                return false;
            };
            track.parent_group_id = parent_id;
            true
        });
        if !found {
            tracing::warn!(track_id, "ignored: track not found");
            return;
        }
        tracing::info!(track_id, ?parent_id, "track reparented");
    }

    pub(crate) fn action_remove_last_track(&mut self) {
        let len = self.song_doc.song().tracks.len();
        if len == 0 {
            return;
        }
        // device teardown の IPC は **pop する前に** 組む (Song から外した後では
        // chain を列挙できず、 plan が空 = IPC が 1 通も出ない)。
        let Some(last_id) = self.song_doc.song().tracks.last().map(|t| t.id) else {
            return;
        };
        let removal_plan = Self::plan_track_removal_ipc(self.song_doc.song(), &[last_id]);
        // PR2.1: pop() の前に id を保存し、 IPC は id で送る。
        let Some(Some(removed)) = self.edit_song(|song| song.tracks.pop()) else {
            return;
        };
        let removed_id = removed.id;
        tracing::info!(
            index = (len - 1) as u32,
            id = removed_id,
            name = %removed.name,
            "removed last track"
        );
        // 消えたトラックが所有していたモジュレーターと、その変調の深さを指していた
        // レーン / 変調の後始末 (track 削除 / グループ解除と同じ 1 本)。
        self.cleanup_modulation_after_track_removal();
        // ClosePluginShmem → RemoveSlotPlugin の順序は plan が持つ。 この経路は
        // 以前 `ClosePluginShmem` を送っておらず (= 順序仕様が守られていなかった)、
        // plan 経由に統一したことで穴も塞がる。
        self.send_track_removal_ipc(&removal_plan);
        self.forget_removed_track_devices(&removal_plan);
        // selected_track_ids は id ベース。 削除対象 track id を除外
        // (Vec の index で持つ subtree とは異なり id 直接判定)。 残りが
        // 空なら最後尾にフォールバック。
        let live_ids: std::collections::HashSet<u32> =
            self.song_doc.song().tracks.iter().map(|t| t.id).collect();
        self.selection.selected_track_ids.retain(|id| live_ids.contains(id));
        if self.selection.selected_track_ids.is_empty()
            && let Some(t) = self.song_doc.song().tracks.last()
        {
            self.selection.selected_track_ids.push(t.id);
        }
        self.ui_prefs.collapsed_groups.retain(|id| live_ids.contains(id));
        // 範囲は「区間 × 行」しか持たないので、消えたトラックの行を落とすだけ。
        self.prune_selection_lanes();
        self.resize_track_peak_display();
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn track_with_devices(id: u32, device_ids: &[u64]) -> common::model::Track {
        let mut t = common::model::Track {
            id,
            ..common::model::Track::default()
        };
        for &device_id in device_ids {
            t.devices.push(common::model::PluginInstance {
                id: device_id,
                ..common::model::PluginInstance::new(
                    "test.fx".into(),
                    common::plugin_format::PluginFormat::Clap,
                )
            });
        }
        t
    }

    /// **順序が仕様** (track ごとに 全 CloseAudioShmem → 全 RemoveHostDevice)。
    /// audio 側の mapping を先に落とさないと、 plugin_host が shmem を unmap した
    /// 直後に audio worker が `pd.prepare()` で unmapped memory を踏み、 AV で
    /// silent terminate → `all_done` 永久 wait になる。
    #[test]
    fn plan_orders_close_before_teardown_per_track() {
        let mut song = common::model::Song::default();
        song.tracks.clear();
        song.tracks.push(track_with_devices(1, &[10, 11]));
        song.tracks.push(track_with_devices(2, &[20, 21]));

        let plan = AppData::plan_track_removal_ipc(&song, &[1, 2]);
        assert_eq!(
            plan,
            vec![
                TrackRemovalIpc::CloseAudioShmem { device_id: 10 },
                TrackRemovalIpc::CloseAudioShmem { device_id: 11 },
                TrackRemovalIpc::RemoveHostDevice { device_id: 10 },
                TrackRemovalIpc::RemoveHostDevice { device_id: 11 },
                TrackRemovalIpc::CloseAudioShmem { device_id: 20 },
                TrackRemovalIpc::CloseAudioShmem { device_id: 21 },
                TrackRemovalIpc::RemoveHostDevice { device_id: 20 },
                TrackRemovalIpc::RemoveHostDevice { device_id: 21 },
            ],
            "2 track × 2 device で 8 要素が期待順に並ぶ"
        );
    }

    /// device を持たない track / 存在しない track は plan に何も出さない
    /// (= 空振りの IPC を送らない)。
    #[test]
    fn plan_is_empty_for_deviceless_and_missing_tracks() {
        let mut song = common::model::Song::default();
        song.tracks.clear();
        song.tracks.push(track_with_devices(1, &[]));
        assert!(AppData::plan_track_removal_ipc(&song, &[1]).is_empty());
        assert!(AppData::plan_track_removal_ipc(&song, &[99]).is_empty());
    }
}
