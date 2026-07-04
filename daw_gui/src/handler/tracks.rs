//! handler::tracks — track lifecycle (add/delete/copy/paste/reorder/rename/ensure)
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use common::protocol::{AudioCommand, PluginCommand};

impl AppData {
    // -------- Track operations ---------------------------------------------

    /// `AppEvent::DeleteTrack` の dispatcher。 plugin が song に居る
    /// 場合は `RequestAllStates` を投げて、 受信時に最新 plugin state
    /// を Song に書き込んでから [`Self::push_undo_snapshot`] + 削除を
    /// 実行する。 これで「knob を回した状態で track 削除 → Undo」 で
    /// knob 値が復元される。 plugin 無しの song は即時実行 (= state を
    /// 取りに行く相手が居ない)。
    pub(crate) fn delete_track(&mut self, idx: u32) {
        let Some(track_id) = self.song_doc.song().tracks.get(idx as usize).map(|t| t.id) else {
            return;
        };
        if !self.song_has_plugin() {
            self.delete_track_inner(track_id);
            return;
        }
        self.enqueue_state_request(PendingStateRequest::Deferred(
            DeferredEdit::DeleteTrack { track_id },
        ));
    }

    // -------- track clipboard --------

    /// Ctrl+C (トラック面)。plugin があれば最新 state を取ってから serialize する
    /// ため deferred、無ければ即時。copy は Song 不変なので undo を積まない。
    pub fn copy_tracks(&mut self, track_ids: Vec<u32>) {
        if track_ids.is_empty() {
            return;
        }
        if !self.song_has_plugin() {
            self.copy_tracks_inner(&track_ids);
            return;
        }
        self.enqueue_state_request(PendingStateRequest::CopyToClipboard { track_ids });
    }

    /// Ctrl+X (トラック面)。copy → 削除を 1 undo step。plugin があれば deferred
    /// (削除前に最新 state 捕捉 + undo snapshot)、無ければ即時。
    pub fn cut_tracks(&mut self, track_ids: Vec<u32>) {
        if track_ids.is_empty() {
            return;
        }
        if !self.song_has_plugin() {
            self.cut_tracks_inner(&track_ids);
            return;
        }
        self.enqueue_state_request(PendingStateRequest::Deferred(DeferredEdit::CutTracks {
            track_ids,
        }));
    }

    /// copy 本体。最新 state 込みの live song から該当トラックを serialize して
    /// `pending_clipboard_write` に積む (view が次フレーム OS clipboard へ flush)。
    pub(crate) fn copy_tracks_inner(&mut self, track_ids: &[u32]) {
        if let Some((json, count)) = self.serialize_tracks_to_envelope(track_ids) {
            self.ui_ephemeral.pending_clipboard_write = Some(json);
            self.ui_ephemeral.status_message = format!("コピー: {count} トラック");
        }
    }

    /// cut 本体。serialize → `pending_clipboard_write` → 各トラック削除。呼び出し側で
    /// undo snapshot 済み (deferred 経由 or 即時 fallback)。group は subtree 一括削除。
    pub(crate) fn cut_tracks_inner(&mut self, track_ids: &[u32]) {
        if let Some((json, count)) = self.serialize_tracks_to_envelope(track_ids) {
            self.ui_ephemeral.pending_clipboard_write = Some(json);
            self.ui_ephemeral.status_message = format!("カット: {count} トラック");
        }
        for &id in track_ids {
            self.delete_track_inner(id);
        }
    }

    /// 指定トラック群を `ClipboardPayload::Tracks` envelope JSON に。`order` は現在の
    /// Vec 順 (上から)。各トラックの clips / automation lanes が参照する content を
    /// inline 同梱 (別プロジェクト独立復元用)。`state` は呼び出し時点で最新化済み前提。
    pub(crate) fn serialize_tracks_to_envelope(&self, track_ids: &[u32]) -> Option<(String, usize)> {
        let mut out: Vec<crate::clipboard::TrackCopy> = Vec::new();
        for t in self.song_doc.song().tracks.iter() {
            if !track_ids.contains(&t.id) {
                continue;
            }
            let mut seen: std::collections::HashSet<common::model::ContentId> =
                std::collections::HashSet::new();
            let mut contents: Vec<crate::clipboard::ContentEntry> = Vec::new();
            let mut cids: Vec<common::model::ContentId> =
                t.clips.iter().map(|c| c.content_id).collect();
            for lane in &t.automation_lanes {
                for ac in &lane.clips {
                    cids.push(ac.content_id);
                }
            }
            for cid in cids {
                if seen.insert(cid) {
                    let content = self
                        .song_doc.song()
                        .clip_contents
                        .get(&cid)
                        .cloned()
                        .unwrap_or_default();
                    let name = self.song_doc.song().clip_content_names.get(&cid).cloned();
                    contents.push(crate::clipboard::ContentEntry {
                        content_id: cid,
                        content,
                        name,
                    });
                }
            }
            out.push(crate::clipboard::TrackCopy {
                order: out.len(),
                track: t.clone(),
                contents,
            });
        }
        if out.is_empty() {
            return None;
        }
        let count = out.len();
        let json = crate::clipboard::ClipboardEnvelope::new(
            self.song_doc.song().project_id,
            crate::clipboard::ClipboardPayload::Tracks(out),
        )
        .to_json()?;
        Some((json, count))
    }

    /// トラック群を「マウス下トラック (`above_track`)」の直上に挿入する。content は
    /// 同一プロジェクト (`src_pid == project_id`) なら流用 (リンク共有)、別なら inline
    /// payload から新採番 (独立)。plugin は state 込み clone され paste 後 host で新
    /// インスタンス化。track 内参照 (parent_group / sends / sidechain / lipsync) は copy
    /// 集合内のものを新 id へ remap、集合外は同一プロジェクトなら据え置き (実在)、別
    /// プロジェクトなら drop。挿入したトラック群を新選択にする。戻り値は挿入数。
    pub fn paste_tracks_at(
        &mut self,
        mut tracks: Vec<crate::clipboard::TrackCopy>,
        src_pid: u64,
        above_track: u32,
    ) -> usize {
        if tracks.is_empty() {
            return 0;
        }
        tracks.sort_by_key(|t| t.order);
        let Some((n, new_ids)) = self.edit_song(|song| {
        let same_project = src_pid == song.project_id;


        // 1) 新 track id を全件先に採番し old→new remap を作る (集合内参照解決用)。
        let mut track_remap: std::collections::HashMap<u32, u32> =
            std::collections::HashMap::new();
        for tc in &tracks {
            let new_id = song.alloc_track_id();
            track_remap.insert(tc.track.id, new_id);
        }

        // 2) content remap。同一プロジェクトかつ content が現存すれば流用 (リンク共有)、
        //    それ以外 (別プロジェクト / 欠落) は inline payload から新採番 (独立)。同一
        //    content_id は 1 度だけ採番して dedup する (cross-track linked / 複数選択の
        //    リンクを保ち、orphan content のリークを防ぐ)。same_project で content が現存
        //    する場合は old→old を入れておき、step 4 の一律適用が no-op になる。
        let mut content_remap: std::collections::HashMap<
            common::model::ContentId,
            common::model::ContentId,
        > = std::collections::HashMap::new();
        for tc in &tracks {
            for ce in &tc.contents {
                if content_remap.contains_key(&ce.content_id) {
                    continue;
                }
                let new_cid =
                    if same_project && song.clip_contents.contains_key(&ce.content_id) {
                        ce.content_id
                    } else {
                        song
                            .alloc_content(ce.content.clone(), ce.name.clone().unwrap_or_default())
                    };
                content_remap.insert(ce.content_id, new_cid);
            }
        }

        // 3) drop 先の親 group context と挿入 index (above_track の直上)。
        let drop_parent = song
            .track_by_id(above_track)
            .and_then(|t| t.parent_group_id);
        let insert_idx = song
            .track_index_by_id(above_track)
            .unwrap_or(song.tracks.len());

        // 4) 各 track を組み立て (参照 remap + content remap)。
        let mut built: Vec<common::model::Track> = Vec::with_capacity(tracks.len());
        for tc in &tracks {
            let mut t = tc.track.clone();
            t.id = *track_remap.get(&tc.track.id).unwrap();
            t.parent_group_id = match t.parent_group_id {
                Some(old) if track_remap.contains_key(&old) => Some(track_remap[&old]),
                Some(old) if same_project && song.track_by_id(old).is_some() => Some(old),
                _ => drop_parent,
            };
            // A5 sibling (r.md #8) / v29: send を間引いたら、 消えた send の
            // SendGain automation lane / mod routing を除去する。 生き残った
            // send は安定 id ごと clone されるので lane は自動で追従する
            // (旧 positional 版の「後続 index 詰め」= reindex_send_gain_lanes
            // は id 化で不要になり model から削除済み)。
            let mut removed_send_ids: Vec<u32> = Vec::new();
            t.sends.retain_mut(|s| {
                let survives = if let Some(&new) = track_remap.get(&s.dest_track_id) {
                    s.dest_track_id = new;
                    true
                } else {
                    same_project && song.track_by_id(s.dest_track_id).is_some()
                };
                if !survives {
                    removed_send_ids.push(s.id);
                }
                survives
            });
            if !removed_send_ids.is_empty() {
                let is_removed_send_gain = |target: &common::model::AutomationTarget| {
                    matches!(
                        target,
                        common::model::AutomationTarget::TrackBuiltin(
                            common::model::TrackBuiltinParam::SendGain { send_id, .. }
                        ) if removed_send_ids.contains(send_id)
                    )
                };
                t.automation_lanes
                    .retain(|l| !is_removed_send_gain(&l.target));
                t.mod_routings.retain(|r| !is_removed_send_gain(&r.target));
            }
            // v29: device の安定 id は Song-global unique が不変条件。 clone
            // した devices の id を再採番し、 track 内 automation lane /
            // mod routing の PluginParam 参照を新 id へ貼り替える (元 track と
            // 重複 id のままだと find_device_by_id が元 device に解決して
            // しまい、 paste 側の param 系イベントが誤配される)。
            {
                let mut device_remap: std::collections::HashMap<u64, u64> =
                    std::collections::HashMap::new();
                for dev in &mut t.devices {
                    let new_id = song.alloc_device_id();
                    if dev.id != 0 {
                        device_remap.insert(dev.id, new_id);
                    }
                    dev.id = new_id;
                }
                let remap_target = |target: &mut common::model::AutomationTarget| {
                    if let common::model::AutomationTarget::PluginParam { device_id, .. } =
                        target
                        && let Some(&nid) = device_remap.get(device_id)
                    {
                        *device_id = nid;
                    }
                };
                for lane in &mut t.automation_lanes {
                    remap_target(&mut lane.target);
                }
                for r in &mut t.mod_routings {
                    remap_target(&mut r.target);
                }
            }
            for dev in &mut t.devices {
                for slot in &mut dev.aux_inputs {
                    if let Some(route) = slot {
                        let old = route.tap.source_track;
                        if let Some(&new) = track_remap.get(&old) {
                            route.tap.source_track = new;
                        } else if !(same_project && song.track_by_id(old).is_some()) {
                            // dangling after paste: drop the route (keep tap_point
                            // intact when the source survives).
                            *slot = None;
                        }
                    }
                }
            }
            t.lipsync_target_track = match t.lipsync_target_track {
                Some(old) if track_remap.contains_key(&old) => Some(track_remap[&old]),
                Some(old) if same_project && song.track_by_id(old).is_some() => Some(old),
                _ => None,
            };
            for c in &mut t.clips {
                if let Some(&new) = content_remap.get(&c.content_id) {
                    c.content_id = new;
                }
            }
            for lane in &mut t.automation_lanes {
                for ac in &mut lane.clips {
                    if let Some(&new) = content_remap.get(&ac.content_id) {
                        ac.content_id = new;
                    }
                }
            }
            built.push(t);
        }

        // 5) above_track の直上に order 昇順を維持して連続挿入。
        let n = built.len();
        for (off, t) in built.into_iter().enumerate() {
            song
                .tracks
                .insert((insert_idx + off).min(song.tracks.len()), t);
        }
        // 6) 選択を新 track 群に + plugin host へ各 device を SetSlotPlugin で実体化
        //    (flush_song_sync = LoadSong は audio 専属で plugin host では no-op
        //    なので、 plugin の実体化には restore が別途必要。state 込みで新インスタンス化)。
            let new_ids: Vec<u32> = tracks
                .iter()
                .filter_map(|tc| track_remap.get(&tc.track.id).copied())
                .collect();
            (n, new_ids)
        }) else {
            return 0;
        };
        self.selection.selected_track_ids = new_ids.clone();
        self.restore_plugins_for_tracks(&new_ids);
        self.resize_track_peak_display();
        n
    }

    /// 実際の削除処理。 [`Self::on_all_states_from_child`] か上の
    /// dispatcher の即時 fallback path から呼ばれる。 どちらでも呼び出し
    /// 側で `push_undo_snapshot` 済みである前提なので、 ここでは push
    /// しない。
    pub(crate) fn delete_track_inner(&mut self, track_id: u32) {
        let Some(idx) = self.song_doc.song().track_index_by_id(track_id) else {
            return;
        };
        let idx = idx as u32;
        if idx as usize >= self.song_doc.song().tracks.len() {
            return;
        }

        // When deleting a Group track, Live recursively removes its
        // entire subtree (children + nested groups) so dangling
        // `parent_group_id` references don't survive. Collect the full
        // subtree of stable ids, then resolve them to current indices
        // and remove from highest to lowest so earlier indices stay
        // valid during the loop.
        let target_id = self.song_doc.song().tracks[idx as usize].id;
        let subtree_ids = self.collect_track_subtree_ids(target_id);
        let mut subtree_idxs: Vec<u32> = subtree_ids
            .iter()
            .filter_map(|id| self.song_doc.song().track_index_by_id(*id))
            .map(|i| i as u32)
            .collect();
        subtree_idxs.sort_unstable();
        subtree_idxs.dedup();

        // PR2.1 race-fix: 順序を「song update → LoadSong → plugin
        // destroy → RemoveTrack」 に固定する。 song update を先に送ら
        // ないと、 audio thread が古い schedule (削除対象 track の
        // ProcessTrack / ProcessGroupFx を含む) で destroyed plugin に
        // dispatch して deadlock する。
        // (a) snapshot を取って順次 song.tracks.remove
        let mut snapshots: Vec<(u32, common::model::Track)> =
            Vec::with_capacity(subtree_idxs.len());
        for &i in subtree_idxs.iter().rev() {
            let removed_id = self.song_doc.song().tracks[i as usize].id;
            let snapshot = self.song_doc.song().tracks[i as usize].clone();
            #[cfg(windows)]
            {
                self.ipc.open_plugin_guis.retain(|&(t, _)| t != removed_id);
            }
            // slot cache からも削除する track 由来の entry を外す。
            // SlotPluginUnloaded event の到着待ち race を狭めて、
            // reconcile が stale entry を見ないようにする防御的 cleanup。
            self.ipc.loaded_slots.retain(|(t, _), _| *t != removed_id);
            self.edit_song(|song| song.tracks.remove(i as usize));
            snapshots.push((removed_id, snapshot));
        }
        // (b) LoadSong で audio engine を新 schedule に
        // (c) **重要 (deadlock 防止)**: RemoveTrack 送信前に daw_audio
        // に直接 ClosePluginShmem を送って plugin_refs から stale entry
        // を消す。 plugin_host の `plugin_shmems.remove` で shmem を
        // unmap した直後、 audio worker が `pd.prepare()` で unmapped
        // memory を読み AV → silent terminate → all_done 永久 wait
        // を防ぐため。
        for (removed_id, _snapshot) in snapshots {
            if let Some(device_ids) = self.ipc.track_plugin_ids.remove(&removed_id) {
                for device_id in device_ids {
                    self.send_audio(AudioCommand::ClosePluginShmem { device_id });
                }
            }
            self.send_plugin(PluginCommand::RemoveTrack { track_id: removed_id });
        }

        // selected_clip / selected_clips は stable ClipKey 保持なので、 残った
        // track の index shift には自動追従する (再マッピング不要)。 ただし
        // 削除された track を指す key は解決不能になるので、 set / anchor 双方
        // から落とす (after_undo_redo / action_remove_last_track と同方針)。
        let mut keys = std::mem::take(&mut self.selection.selected_clips);
        keys.retain(|k| self.clip_at(*k).is_some());
        self.selection.selected_clips = keys;
        if let Some(k) = self.selection.selected_clip
            && self.clip_at(k).is_none()
        {
            self.selection.selected_clip = None;
            self.selection.selected_notes.clear();
        }

        // selected_track_ids: subtree に含まれていた id を全て除外。
        // 残りが空なら直近の生存 track にフォールバック (UI 完全選択
        // ゼロを避ける)。
        let subtree_ids_set: std::collections::HashSet<u32> = subtree_ids.iter().copied().collect();
        self.selection.selected_track_ids
            .retain(|id| !subtree_ids_set.contains(id));
        if self.selection.selected_track_ids.is_empty()
            && let Some(t) = self.song_doc.song().tracks.last()
        {
            self.selection.selected_track_ids.push(t.id);
        }
        // collapsed_groups からも消えた id を除外。
        self.ui_prefs.collapsed_groups
            .retain(|id| !subtree_ids_set.contains(id));
        self.resize_track_peak_display();
    }

    /// Return `root_id` plus every descendant track that points at it
    /// (directly or transitively) via `parent_group_id`. Used by
    /// `delete_track` when removing a Group: the whole subtree is
    /// dropped together (Live convention) so no orphan references
    /// survive. Cycle-safe via a hop limit.
    pub(crate) fn collect_track_subtree_ids(&self, root_id: u32) -> Vec<u32> {
        let mut result = vec![root_id];
        let mut frontier = vec![root_id];
        let mut hops = 0;
        while !frontier.is_empty() {
            hops += 1;
            if hops > self.song_doc.song().tracks.len() + 1 {
                tracing::error!(
                    root_id,
                    "collect_track_subtree_ids: cycle detected, aborting BFS"
                );
                break;
            }
            let mut next = Vec::new();
            for &pid in &frontier {
                for t in &self.song_doc.song().tracks {
                    if t.parent_group_id == Some(pid) && !result.contains(&t.id) {
                        result.push(t.id);
                        next.push(t.id);
                    }
                }
            }
            frontier = next;
        }
        result
    }

    pub(crate) fn swap_tracks(&mut self, a: u32, b: u32) {
        if a == b {
            return;
        }
        let n = self.song_doc.song().tracks.len() as u32;
        if a >= n || b >= n {
            return;
        }
        self.edit_song(|song| song.tracks.swap(a as usize, b as usize));
        // PR2.1: plugin_host の chains は `Track::id` ベースなので、
        // Vec position swap は通知不要。 SwapTracks IPC は削除済。
        // selected_clip / selected_clips は stable ClipKey 保持なので、 track の
        // index swap には自動追従する (id 不変、 再マッピング不要)。 旧 index
        // ベース実装は selected_clips を取りこぼすバグがあったが、 これで解消。
        // selected_track_ids は id ベースなので track の index swap で
        // 自動的に追従する (id は変わらないため再マッピング不要)。
        self.resize_track_peak_display();
    }

    /// Drag&drop reorder。`order` は新順での `Track.id` 列。order に含まれない
    /// track は末尾に残す (gui_01 daw_prototype の流儀に合わせ防御的)。
    pub(crate) fn reorder_tracks(&mut self, order: &[u32]) {
        if order.is_empty() {
            return;
        }
        // 並びが変化しない場合は no-op
        let same = order.iter().enumerate().all(|(i, id)| {
            self.song_doc.song().tracks.get(i).map(|t| t.id) == Some(*id)
        });
        if same && order.len() == self.song_doc.song().tracks.len() {
            return;
        }
        let selected_track_id = self
            .song_doc.song()
            .tracks
            .get(self.cursor_track_index().unwrap_or(0))
            .map(|t| t.id);
        // selected_clips / selected_clip は stable ClipKey 保持なので reorder
        // (track の index 変化) に自動追従する。 旧実装の id ラウンドトリップ
        // (抽出 → 並べ替え → index 逆引き) は不要になった。

        // 元順序での index 列を計算 (`order[i]` の id を持つ track の旧 index)。
        // この `index_order` で（旧設計では専用 IPC を送っていたが、現行は LoadSong 再送で）
        // plugin host 側で 1 回の `tracks.mutate` (= 1 回の audio thread stop/start)
        // で chains / params / vocal を新順序に並び替える。
        let index_order: Vec<u32> = order
            .iter()
            .filter_map(|id| {
                self.song_doc.song()
                    .tracks
                    .iter()
                    .position(|t| t.id == *id)
                    .map(|p| p as u32)
            })
            .collect();

        // song.tracks を新順序に並び替え (= 表示モデル更新)。
        self.edit_song(|song| {
            let mut new_tracks = Vec::with_capacity(song.tracks.len());
            for id in order {
                if let Some(pos) = song.tracks.iter().position(|t| t.id == *id) {
                    new_tracks.push(song.tracks.remove(pos));
                }
            }
            new_tracks.append(&mut song.tracks);
            song.tracks = new_tracks;
        });

        // selected_track_ids は id ベースなので、 reorder 後も自動的に
        // 整合 (id は変わらず、 song.tracks の Vec 内 index が変わるだけ
        // で `cursor_track_index` が再評価される)。 selected_track_id
        // 局所変数は不要。
        let _ = selected_track_id;
        // selected_clips / selected_clip は stable ClipKey 保持のため再構築不要。

        // PR2.1: plugin_host の chains は `Track::id` ベースなので、
        // Vec position の reorder は通知不要。 ReorderTracks IPC は
        // 削除済。 LoadSong (flush_song_sync) で song_store
        // のみ新順序に同期する。
        let _ = index_order;
        self.resize_track_peak_display();
    }

    /// 単独選択する (index ベース、 旧 API 互換)。 新 multi-select API
    /// (gui_01 #016) からは `SelectTrack { next, modifier, .. }` 経由で
    /// `selected_track_ids` を直接書き込む。
    pub(crate) fn select_track(&mut self, idx: u32) {
        let Some(t) = self.song_doc.song().tracks.get(idx as usize) else {
            return;
        };
        let id = t.id;
        if self.selection.selected_track_ids.as_slice() != [id] {
            self.selection.selected_track_ids = vec![id];
        }
    }

    pub(crate) fn begin_rename_track(&mut self, track_id: u32) {
        let Some(name) = self
            .song_doc.song()
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .map(|t| t.name.clone())
        else {
            return;
        };
        self.ui_ephemeral.track_rename_text = name;
        self.ui_ephemeral.track_rename_id = Some(track_id);
    }

    pub(crate) fn commit_rename_track(&mut self) {
        let Some(track_id) = self.ui_ephemeral.track_rename_id else {
            return;
        };
        self.ui_ephemeral.track_rename_id = None;
        let new_name = self.ui_ephemeral.track_rename_text.trim().to_string();
        self.ui_ephemeral.track_rename_text.clear();
        if new_name.is_empty() {
            return;
        }
        self.edit_song(|song| {
            if let Some(track) = song.tracks.iter_mut().find(|t| t.id == track_id) {
                track.name = new_name;
            }
        });
    }

    /// セクション帯の inline 改名を開始する (現在名を編集バッファに seed)。
    pub(crate) fn begin_rename_section(&mut self, id: u32) {
        let Some(name) = self.song_doc.song().sections.iter().find(|s| s.id == id).map(|s| s.name.clone())
        else {
            return;
        };
        self.ui_ephemeral.section_rename_text = name;
        self.ui_ephemeral.section_rename_id = Some(id);
    }

    /// セクション帯の改名を確定する (空名は無視)。
    pub(crate) fn commit_rename_section(&mut self) {
        let Some(id) = self.ui_ephemeral.section_rename_id else {
            return;
        };
        self.ui_ephemeral.section_rename_id = None;
        let new_name = self.ui_ephemeral.section_rename_text.trim().to_string();
        self.ui_ephemeral.section_rename_text.clear();
        if new_name.is_empty() {
            return;
        }
        self.edit_song(|song| {
            if let Some(s) = song.sections.iter_mut().find(|s| s.id == id) {
                s.name = new_name;
            }
        });
    }

    pub(crate) fn begin_rename_clip(&mut self, target: ClipRef) {
        let Some(content_id) = self
            .song_doc.song()
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .map(|c| c.content_id)
        else {
            return;
        };
        // 表示されている名前 (= clip_display_label と同じ) を編集開始値にする。
        // Text clip は本文 (= first TextEvent.text) を、 それ以外は content_name を pre-fill。
        self.ui_ephemeral.clip_rename_text = self
            .song_doc.song()
            .clip_contents
            .get(&content_id)
            .and_then(|c| c.text_events())
            .and_then(|events| events.first())
            .map(|ev| ev.text.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| self.song_doc.song().content_name(content_id).to_string());
        self.ui_ephemeral.clip_rename = Some(target);
    }

    /// clip rename を確定。 trim 後空文字なら無変更 (track rename と同じ)。
    /// clip 名は表示専用 (audio / plugin processing に無関係) なので
    /// `flush_song_sync` は呼ばない。 song の変更は autosave /
    /// undo snapshot (`is_undoable`) に乗る。 名前は `content_id` 単位の
    /// SSoT (`Song.clip_content_names`) に書くので、 同 content を共有する
    /// linked clip 全部が同時に rename される。
    pub(crate) fn commit_rename_clip(&mut self) {
        let Some(target) = self.ui_ephemeral.clip_rename else {
            return;
        };
        self.ui_ephemeral.clip_rename = None;
        let new_name = self.ui_ephemeral.clip_rename_text.trim().to_string();
        self.ui_ephemeral.clip_rename_text.clear();
        if new_name.is_empty() {
            return;
        }
        let Some(content_id) = self
            .song_doc.song()
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .map(|c| c.content_id)
        else {
            return;
        };
        // Text clip は本文 (= 全 TextEvent.text) に書く。 表示名 (clip_display_label)
        // は content-first で本文を優先するので、 content_name を書いても見えない。
        // set_clip_text_event_content が全 event 書換え + edit buffer resync + is_dirty を
        // 行う (inspector の content 編集と同経路)。 非 Text clip は従来どおり content_name。
        if matches!(
            self.song_doc.song().clip_contents.get(&content_id),
            Some(common::model::ClipContent::Text(_))
        ) {
            self.set_clip_text_event_content(target, new_name);
        } else {
            self.edit_song(|song| song.set_content_name(content_id, new_name));
            // content_name 経路は Song 側 set_content_name が dirty を持たないが、
            // CommitRenameClip は is_undoable なので #40 のチョークポイント
            // (handle_event 冒頭) が既に is_dirty を立てている (= 手動 arm 不要)。
        }
    }

    pub(crate) fn ensure_first_track(&mut self) {
        if self.song_doc.song().tracks.is_empty() {
            self.edit_song(|song| {
                let id = song.alloc_track_id();
                song.tracks.push(track_with(|t| {
                    t.id = id;
                    t.name = "Track 1".into();
                }));
            });
            self.resize_track_peak_display();
        }
    }

}
