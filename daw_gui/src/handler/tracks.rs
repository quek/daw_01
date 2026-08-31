//! handler::tracks — track lifecycle (add/delete/copy/paste/reorder/rename/ensure)
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;

impl AppData {
    // -------- Track operations ---------------------------------------------

    /// `AppEvent::DeleteTracks` の dispatcher (r.md #43)。 plugin が song に居る
    /// 場合は `RequestAllStates` を投げて、 受信時に最新 plugin state
    /// を Song に書き込んでから、 `edit_song` チョークポイント (= undo snapshot を
    /// 無条件に積む、 不変条件 5) を通して削除を実行する。 これで「knob を回した状態で track 削除 → Undo」 で
    /// knob 値が復元される。 plugin 無しの song は即時実行 (= state を
    /// 取りに行く相手が居ない)。
    ///
    /// **deferred は必ず 1 件にまとめる** — id ごとに enqueue すると round-trip が
    /// 分かれて undo が N ステップに割れる (Ctrl+Z 1 回で戻らなくなる)。
    ///
    /// 実在しない id (master row の `MASTER_TRACK_ID` は `song.tracks` に居ない、
    /// 重複指定) は先に落とす。 落とした結果が空なら **何もしない** —
    /// 空のまま deferred / edit_song に入ると「何も消えないのに dirty 化 +
    /// 死んだ undo step」 になる。
    pub(crate) fn delete_tracks(&mut self, track_ids: Vec<u32>) {
        let ids = self.live_track_ids(&track_ids);
        if ids.is_empty() {
            // 無言の no-op にしない: master 行を選んで Delete したときに
            // 「Delete が壊れた」 と誤認されるので理由を出す (paste_noop と同方針)。
            if track_ids.contains(&common::model::MASTER_TRACK_ID) {
                self.ui_ephemeral.status_message =
                    "マスタートラックは削除できません".to_string();
            }
            return;
        }
        if !self.song_has_plugin() {
            self.delete_tracks_inner(&ids);
            return;
        }
        self.enqueue_state_request(PendingStateRequest::Deferred(DeferredEdit::DeleteTracks {
            track_ids: ids,
        }));
    }

    /// 複数トラック削除の本体。 呼び出し側で undo snapshot 済み (deferred 経由 or
    /// 即時 fallback)。 group は [`Self::delete_track_inner`] が subtree ごと消すので、
    /// 親と子を同時に選択していても 2 周目は `track_index_by_id` が `None` を返して
    /// 自然に no-op になる (= 与えられた集合をそのまま回してよい、 順序依存を作らない)。
    pub(crate) fn delete_tracks_inner(&mut self, track_ids: &[u32]) {
        for &id in track_ids {
            self.delete_track_inner(id);
        }
    }

    // -------- track clipboard --------

    /// Ctrl+C (トラック面)。plugin があれば最新 state を取ってから serialize する
    /// ため deferred、無ければ即時。copy は Song 不変なので undo を積まない。
    pub fn copy_tracks(&mut self, track_ids: Vec<u32>) {
        // delete と同じ実在フィルタを通す (SSoT)。 master 行だけの選択で Ctrl+C を
        // 押すと、 素通しでは plugin state の全 round-trip を 1 往復空振りさせ、
        // その間 dirty guard (New / Open / 終了) まで保留される。
        let track_ids = self.live_track_ids(&track_ids);
        if track_ids.is_empty() {
            return;
        }
        if !self.song_has_plugin() {
            self.copy_tracks_inner(&track_ids);
            return;
        }
        self.enqueue_state_request(PendingStateRequest::CopyToClipboard(
            ClipboardCopyRequest::Tracks(track_ids),
        ));
    }

    /// Ctrl+X (トラック面)。copy → 削除を 1 undo step。plugin があれば deferred
    /// (削除前に最新 state 捕捉 + undo snapshot)、無ければ即時。
    pub fn cut_tracks(&mut self, track_ids: Vec<u32>) {
        // copy / delete と同じ実在フィルタ (master 行だけの選択で空振り round-trip を
        // 起こさない)。
        let track_ids = self.live_track_ids(&track_ids);
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
        self.delete_tracks_inner(track_ids);
    }

    /// 指定トラック群を [`crate::clipboard::TracksCopy`] に組み立てる (clipboard
    /// serialize と in-app duplicate の共通口)。`order` は現在の Vec 順 (上から)。
    /// 各トラックの clips / automation lanes / **ランチャーのセル** が参照する content を
    /// inline 同梱 (別プロジェクト / 独立複製の復元用)。`state` は呼び出し時点で
    /// 最新化済み前提。
    ///
    /// v35 (r.md #87): content の数え上げは `Track::all_clips` /
    /// `AutomationLane::all_clips` を通す (= arrangement + launcher)。セルを数え
    /// 落とすと、別プロジェクトへ貼ったセルが元プロジェクトの `content_id` を
    /// 保ったまま落ちて、無関係な中身を鳴らすか無音になる。
    ///
    /// `scenes` (コピー元の列の並び) も一緒に載せる — セルの着地列を決める唯一の
    /// 手がかり ([`crate::clipboard::TracksCopy`])。
    pub(crate) fn collect_track_copies(&self, track_ids: &[u32]) -> crate::clipboard::TracksCopy {
        let mut out: Vec<crate::clipboard::TrackCopy> = Vec::new();
        for t in self.song_doc.song().tracks.iter() {
            if !track_ids.contains(&t.id) {
                continue;
            }
            let mut seen: std::collections::HashSet<common::model::ContentId> =
                std::collections::HashSet::new();
            let mut contents: Vec<crate::clipboard::ContentEntry> = Vec::new();
            let mut cids: Vec<common::model::ContentId> =
                t.all_clips().map(|c| c.content_id).collect();
            for lane in &t.automation_lanes {
                for ac in lane.all_clips() {
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
        crate::clipboard::TracksCopy {
            tracks: out,
            scenes: self.song_doc.song().scenes.iter().map(|s| s.id).collect(),
        }
    }

    /// 指定トラック群を `ClipboardPayload::Tracks` envelope JSON に。中身の組み立ては
    /// [`Self::collect_track_copies`] (content の inline 同梱とコピー元の列の並び) が持つ。
    pub(crate) fn serialize_tracks_to_envelope(&self, track_ids: &[u32]) -> Option<(String, usize)> {
        let out = self.collect_track_copies(track_ids);
        if out.tracks.is_empty() {
            return None;
        }
        let count = out.tracks.len();
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
    ///
    /// v35 (r.md #87): ランチャーのセルの列 (`scene_id`) は
    /// [`Self::remap_pasted_scenes`] が `payload.scenes` (コピー元の列の並び) から
    /// 貼り先の列へ張り替える。
    pub fn paste_tracks_at(
        &mut self,
        payload: crate::clipboard::TracksCopy,
        src_pid: u64,
        above_track: u32,
    ) -> usize {
        let crate::clipboard::TracksCopy {
            mut tracks,
            scenes: src_scenes,
        } = payload;
        if tracks.is_empty() {
            return 0;
        }
        tracks.sort_by_key(|t| t.order);
        // audio editor の対象が消える編集なので、退避した key で引き直して畳む。
        let audio_editor_key = self.audio_editor_target_key();
        let Some(new_ids) = self.edit_song(|song| {
            let same_project = src_pid == song.project_id;
            // drop 先の親 group context と挿入 index (above_track の直上)。
            let drop_parent = song.track_by_id(above_track).and_then(|t| t.parent_group_id);
            let insert_idx = song
                .track_index_by_id(above_track)
                .unwrap_or(song.tracks.len());
            // paste は content 流用ポリシー (same_project で現存 content はリンク共有)。
            let mut built =
                Self::build_pasted_tracks(song, &tracks, same_project, false, drop_parent);
            Self::remap_pasted_scenes(song, &mut built, &src_scenes, same_project);
            let new_ids: Vec<u32> = built.iter().map(|(_, t)| t.id).collect();
            // above_track の直上に order 昇順を維持して連続挿入。
            for (off, (_, t)) in built.into_iter().enumerate() {
                song.tracks.insert((insert_idx + off).min(song.tracks.len()), t);
            }
            // 行の不変条件 (孤児セル / 消えたセルを指す主導権 / 死んだ列への Jump) は
            // model が持つ。貼り付けた行にも同じ規則を通す (冪等なので既存行は不変)。
            song.normalize_session();
            new_ids
        }) else {
            return 0;
        };
        // 選択を新 track 群に + plugin host へ各 device を SetSlotPlugin で実体化
        // (flush_song_sync = LoadSong は audio 専属で plugin host では no-op なので、
        //  plugin の実体化には restore が別途必要。state 込みで新インスタンス化)。
        let n = new_ids.len();
        self.set_track_selection(new_ids.clone());
        self.restore_plugins_for_tracks(&new_ids);
        self.reanchor_audio_editor(audio_editor_key);
        self.resize_track_peak_display();
        n
    }

    /// paste / duplicate 共通の remap エンジン。`tracks` (order 昇順前提) から新 id
    /// で track を組み立てて返す (`(元 track id, 組み立て済み track)` の列。 挿入は
    /// caller が行う)。`same_project` = 元と同一 project (集合外の track/content 参照を
    /// 据え置くか drop するかの判定)。`force_independent_content` = true なら content を
    /// 常に新採番 (独立複製 / Alt+D 相当)、 false なら same_project で現存する content は
    /// 流用 (リンク共有 / D 相当)。`drop_parent` = 集合内にも据え置き対象にも無い parent
    /// の落とし先 (paste は above_track の group、 duplicate は None で top-level 維持)。
    /// device は常に新 id を採番する (走行中の plugin instance は共有不可、 state だけ
    /// clone して host で新インスタンス化)。
    pub(crate) fn build_pasted_tracks(
        song: &mut common::model::Song,
        tracks: &[crate::clipboard::TrackCopy],
        same_project: bool,
        force_independent_content: bool,
        drop_parent: Option<u32>,
    ) -> Vec<(u32, common::model::Track)> {
        // 1) 新 track id を全件先に採番し old→new remap を作る (集合内参照解決用)。
        let mut track_remap: std::collections::HashMap<u32, u32> =
            std::collections::HashMap::new();
        for tc in tracks {
            let new_id = song.alloc_track_id();
            track_remap.insert(tc.track.id, new_id);
        }

        // 2) content remap。`force_independent_content` なら常に新採番 (独立)。 そうでなく
        //    同一プロジェクトかつ content が現存すれば流用 (リンク共有)、 それ以外 (別
        //    プロジェクト / 欠落) は inline payload から新採番。同一 content_id は 1 度
        //    だけ採番して dedup する (cross-track linked / 複数選択のリンクを保ち、
        //    orphan content のリークを防ぐ)。流用時は old→old を入れておき、step 3 の
        //    一律適用が no-op になる。
        let mut content_remap: std::collections::HashMap<
            common::model::ContentId,
            common::model::ContentId,
        > = std::collections::HashMap::new();
        for tc in tracks {
            for ce in &tc.contents {
                if content_remap.contains_key(&ce.content_id) {
                    continue;
                }
                let new_cid = if !force_independent_content
                    && same_project
                    && song.clip_contents.contains_key(&ce.content_id)
                {
                    ce.content_id
                } else {
                    song.alloc_content(ce.content.clone(), ce.name.clone().unwrap_or_default())
                };
                content_remap.insert(ce.content_id, new_cid);
            }
        }

        // 3) 各 track を組み立て (参照 remap + content remap)。
        let mut built: Vec<(u32, common::model::Track)> = Vec::with_capacity(tracks.len());
        for tc in tracks {
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
            // v35 (r.md #87): content の張り替えは `all_clips_mut` (= arrangement +
            // launcher のセル) を通す。セルを落とすと、独立複製 (Alt+D) でセルだけが
            // 元トラックと content を共有したまま残り、片方の編集がもう片方へ漏れる。
            for c in t.all_clips_mut() {
                if let Some(&new) = content_remap.get(&c.content_id) {
                    c.content_id = new;
                }
            }
            for lane in &mut t.automation_lanes {
                for ac in lane.all_clips_mut() {
                    if let Some(&new) = content_remap.get(&ac.content_id) {
                        ac.content_id = new;
                    }
                }
            }
            built.push((tc.track.id, t));
        }
        built
    }

    /// v35 (r.md #87): 組み立て済みトラックのランチャーのセルを、**貼り先の列**へ
    /// 張り替える。`src_scenes` はコピー元 `Song.scenes` の id を表示順に並べたもの
    /// ([`crate::clipboard::TracksCopy::scenes`])。
    ///
    /// 列 id はプロジェクトごとの id 空間 (設計正本 §1.1) なので、別プロジェクトの
    /// id をそのまま持ち込むと意味の違う列を指す。解けるのは「元で何列目だったか」
    /// だけなので、その index を [`common::model::Song::ensure_scene_at`] で実体化して
    /// 張り替える (セル単体の貼り付け `paste_launcher_cells` と同じ数え方)。
    ///
    /// **列を解けなかったセルはここで落とす。** 残すと実在しない列を指したまま
    /// 保存され、次に開いたときに `Song::normalize_session` の孤児掃除が黙って消す
    /// (dirty も立たないので、保存前に気付く機会が無い)。
    ///
    /// フォローアクションの `Jump { scene_id }` も同じ表で解く — 同じ id 空間の
    /// 参照なので、片方だけ直すと「貼ったセルが無関係な列へ飛ぶ」形で残る。
    fn remap_pasted_scenes(
        song: &mut common::model::Song,
        built: &mut [(u32, common::model::Track)],
        src_scenes: &[u32],
        same_project: bool,
    ) {
        // old → new の解は列ごとに 1 つ。行をまたいで解き直すと、同じ列のセルが
        // 行ごとに違う列へ着地し得る。
        let mut cache: std::collections::HashMap<u32, Option<u32>> =
            std::collections::HashMap::new();
        for (_, t) in built.iter_mut() {
            let cells = std::mem::take(&mut t.session_clips);
            for mut cell in cells {
                let Some(new) = Self::resolve_pasted_scene(
                    song,
                    &mut cache,
                    src_scenes,
                    same_project,
                    cell.scene_id,
                ) else {
                    continue;
                };
                cell.scene_id = new;
                Self::remap_follow_jump(
                    song,
                    &mut cache,
                    src_scenes,
                    same_project,
                    &mut cell.launch,
                );
                // 「1 行 1 列 1 セル」の維持は model の口に任せる (別々の列が同じ
                // 列へ解けたときの置き換え規約も、主導権の引き継ぎもここが持つ)。
                t.put_session_clip(cell);
            }
            for lane in &mut t.automation_lanes {
                let cells = std::mem::take(&mut lane.session_clips);
                for mut cell in cells {
                    let Some(new) = Self::resolve_pasted_scene(
                        song,
                        &mut cache,
                        src_scenes,
                        same_project,
                        cell.scene_id,
                    ) else {
                        continue;
                    };
                    cell.scene_id = new;
                    Self::remap_follow_jump(
                        song,
                        &mut cache,
                        src_scenes,
                        same_project,
                        &mut cell.launch,
                    );
                    lane.put_session_clip(cell);
                }
            }
        }
    }

    /// コピー元の列 id → 貼り先の列 id。解けなければ `None` (= そのセルは貼らない)。
    ///
    /// 同一プロジェクトで列が現存すれば **その列のまま** (列を並べ替えていても
    /// ユーザーが見ていた列に着地する)。それ以外は元の表示 index で実体化する。
    fn resolve_pasted_scene(
        song: &mut common::model::Song,
        cache: &mut std::collections::HashMap<u32, Option<u32>>,
        src_scenes: &[u32],
        same_project: bool,
        old: u32,
    ) -> Option<u32> {
        if let Some(&hit) = cache.get(&old) {
            return hit;
        }
        let resolved = if old == 0 {
            // 未採番 sentinel (= sanitize が潰した重複列 / 壊れた JSON)。
            None
        } else if same_project && song.scene_index(old).is_some() {
            Some(old)
        } else {
            // 元で何列目だったか → 貼り先の同じ index を実体化する。
            src_scenes
                .iter()
                .position(|&id| id == old)
                .map(|index| song.ensure_scene_at(index))
        };
        cache.insert(old, resolved);
        resolved
    }

    /// セルのフォローアクションが指す `Jump` の飛び先を貼り先の列へ張り替える。
    /// 解けない飛び先は `NoAction` へ倒す (`Song::normalize_session` が dangling な
    /// `scene_id` に対して行うのと同じ判断)。
    fn remap_follow_jump(
        song: &mut common::model::Song,
        cache: &mut std::collections::HashMap<u32, Option<u32>>,
        src_scenes: &[u32],
        same_project: bool,
        launch: &mut common::model::LaunchSettings,
    ) {
        use common::model::FollowActionKind;
        for kind in [&mut launch.follow.a, &mut launch.follow.b] {
            if let FollowActionKind::Jump { scene_id } = *kind {
                *kind = match Self::resolve_pasted_scene(
                    song,
                    cache,
                    src_scenes,
                    same_project,
                    scene_id,
                ) {
                    Some(new) => FollowActionKind::Jump { scene_id: new },
                    None => FollowActionKind::NoAction,
                };
            }
        }
    }

    /// トラック複製 (r.md #30) の dispatcher。plugin があれば最新 state を取ってから
    /// serialize するため deferred、無ければ即時。copy_tracks / cut_tracks と同 idiom。
    /// `linked=true` はクリップ中身を元と content_id 共有 (D 相当)、 `false` は独立
    /// コピー (Alt+D 相当)。
    pub fn duplicate_tracks(&mut self, track_ids: Vec<u32>, linked: bool) {
        // delete / copy / cut と同じ実在フィルタ (master 行だけの選択で空振り
        // round-trip を起こさない)。
        let track_ids = self.live_track_ids(&track_ids);
        if track_ids.is_empty() {
            return;
        }
        if !self.song_has_plugin() {
            self.duplicate_tracks_inner(&track_ids, linked);
            return;
        }
        self.enqueue_state_request(PendingStateRequest::Deferred(DeferredEdit::DuplicateTracks {
            track_ids,
            linked,
        }));
    }

    /// 複製本体。呼び出し側で最新 plugin state 反映済み (deferred 経由 or 即時 fallback)。
    /// 各「root」(= 選択集合内に祖先を持たない選択 track) の subtree を複製し、 元 subtree
    /// の直下へ挿入する。remap は root 跨ぎで共有し (root 間の send / sidechain / linked
    /// clip の内部リンクを保つ)、 挿入だけ root ごとに行う。挿入は下方 index がずれるので
    /// **現在 index の大きい root から** 処理する。undo snapshot は edit_song が積む。
    pub(crate) fn duplicate_tracks_inner(&mut self, track_ids: &[u32], linked: bool) {
        if track_ids.is_empty() {
            return;
        }
        // roots = 選択 id のうち、 別の選択 id を祖先に持たないもの (group とその child
        // を両方選んだら group だけを root にして二重複製を防ぐ)。存在しない id は除外。
        let sel: std::collections::HashSet<u32> = track_ids.iter().copied().collect();
        let roots: Vec<u32> = track_ids
            .iter()
            .copied()
            .filter(|&id| self.song_doc.song().track_by_id(id).is_some())
            .filter(|&id| !self.track_ancestor_in_set(id, &sel))
            .collect();
        if roots.is_empty() {
            return;
        }
        // 各 root の subtree id list (安定 id、 挿入で index がずれても不変)。
        let root_subtrees: Vec<(u32, Vec<u32>)> = roots
            .iter()
            .map(|&r| (r, self.collect_track_subtree_ids(r)))
            .collect();
        // 複製対象の全 id を song 順に (serialize と built の対応付けはこの順)。
        let full: std::collections::HashSet<u32> = root_subtrees
            .iter()
            .flat_map(|(_, s)| s.iter().copied())
            .collect();
        let full_ordered: Vec<u32> = self
            .song_doc
            .song()
            .tracks
            .iter()
            .map(|t| t.id)
            .filter(|id| full.contains(id))
            .collect();
        let copies = self.collect_track_copies(&full_ordered);
        if copies.tracks.is_empty() {
            return;
        }

        let new_ids = self.edit_song(|song| {
            // 全 track を 1 度に remap (root 跨ぎのリンクを保つため remap は global)。
            // same_project=true (元と同一 project)、 独立/リンクは force_independent で
            // 切替、 drop_parent=None で top-level は top-level のまま (group child は
            // 元 parent を継承)。
            let mut built = Self::build_pasted_tracks(song, &copies.tracks, true, !linked, None);
            // 同一プロジェクトなので列はそのまま解ける (= 実質 no-op) が、複製も
            // 貼り付けと同じ 1 本を通す — 列を消した直後に複製した場合も、規則が
            // 2 本に割れずに済む。
            Self::remap_pasted_scenes(song, &mut built, &copies.scenes, true);
            let mut built_by_src: std::collections::HashMap<u32, common::model::Track> =
                built.into_iter().collect();
            // 各 root の subtree を、 元 subtree の直下へ挿入する。 挿入で下方の index が
            // ずれるので、 現在 index が大きい root から処理する (下位 root の subtree
            // index は影響を受けない)。
            let mut order: Vec<(usize, usize)> = root_subtrees
                .iter()
                .enumerate()
                .filter_map(|(i, (r, _))| song.track_index_by_id(*r).map(|idx| (idx, i)))
                .collect();
            order.sort_by_key(|(idx, _)| std::cmp::Reverse(*idx));
            let mut new_ids: Vec<u32> = Vec::new();
            for (_, i) in order {
                let (_, subtree_ids) = &root_subtrees[i];
                // subtree を現在の song 順に並べ、 最後の index の直後へ順次 insert。
                let mut sub_ordered: Vec<(usize, u32)> = subtree_ids
                    .iter()
                    .filter_map(|id| song.track_index_by_id(*id).map(|idx| (idx, *id)))
                    .collect();
                sub_ordered.sort_by_key(|(idx, _)| *idx);
                let Some(&(last_idx, _)) = sub_ordered.last() else {
                    continue;
                };
                let mut insert_at = last_idx + 1;
                for (_, src) in sub_ordered {
                    if let Some(t) = built_by_src.remove(&src) {
                        new_ids.push(t.id);
                        song.tracks.insert(insert_at.min(song.tracks.len()), t);
                        insert_at += 1;
                    }
                }
            }
            new_ids
        });
        let Some(new_ids) = new_ids else {
            return;
        };
        if new_ids.is_empty() {
            return;
        }
        self.set_track_selection(new_ids.clone());
        self.restore_plugins_for_tracks(&new_ids);
        self.resize_track_peak_display();
        self.ui_ephemeral.status_message = format!("複製: {} トラック", new_ids.len());
    }

    /// `id` の祖先チェーン (`parent_group_id`) に `set` の要素が居るか (cycle-safe)。
    /// duplicate の root 判定に使う (選択集合内の group child を root から除外)。
    fn track_ancestor_in_set(&self, id: u32, set: &std::collections::HashSet<u32>) -> bool {
        let mut cursor = self.song_doc.song().track_by_id(id).and_then(|t| t.parent_group_id);
        let limit = self.song_doc.song().tracks.len() + 1;
        let mut hops = 0;
        while let Some(pid) = cursor {
            if set.contains(&pid) {
                return true;
            }
            hops += 1;
            if hops > limit {
                break;
            }
            cursor = self.song_doc.song().track_by_id(pid).and_then(|t| t.parent_group_id);
        }
        false
    }

    /// 実際の削除処理。 [`Self::on_all_states_from_child`] か上の
    /// dispatcher の即時 fallback path から呼ばれる。 undo snapshot は
    /// どちらの経路でも呼び出し側の `edit_song` が積むので、 ここは
    /// `song` を書き換えるだけ。
    pub(crate) fn delete_track_inner(&mut self, track_id: u32) {
        let Some(idx) = self.song_doc.song().track_index_by_id(track_id) else {
            return;
        };
        // Audio Editor が開いていたら、対象が消える / audio でなくなる場合に閉じる
        // (undo 経路 `after_undo_redo` と同じガード)。key は安定 id なので
        // 「詰まって別トラックのクリップを指す」 ことは無い。
        let audio_editor_key = self.audio_editor_target_key();
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
        // 削除で空く最小 index。 削除後にここへ繰り上がる track が「隣接」。
        let removed_min_idx = subtree_idxs.first().copied().unwrap_or(idx) as usize;

        // PR2.1 race-fix: 順序を「song update → LoadSong → plugin
        // destroy → RemoveSlotPlugin」 に固定する。 song update を先に送ら
        // ないと、 audio thread が古い schedule (削除対象 track の
        // ProcessTrack / ProcessGroupFx を含む) で destroyed plugin に
        // dispatch して deadlock する。
        // (a) device teardown の IPC 列を **Song から外す前に** 組む
        //     (`fx_chain_by_track_id` は削除前の Song からしか引けない)。
        let removal_targets: Vec<u32> = subtree_idxs
            .iter()
            .rev()
            .map(|&i| self.song_doc.song().tracks[i as usize].id)
            .collect();
        let removal_plan =
            Self::plan_track_removal_ipc(self.song_doc.song(), &removal_targets);
        for &i in subtree_idxs.iter().rev() {
            self.edit_song(|song| song.tracks.remove(i as usize));
        }
        // (b) LoadSong で audio engine を新 schedule に
        // (c) **重要 (deadlock 防止)**: RemoveSlotPlugin 送信前に daw_audio
        // に直接 ClosePluginShmem を送って plugin_refs から stale entry
        // を消す。 plugin_host の `plugin_shmems.remove` で shmem を
        // unmap した直後、 audio worker が `pd.prepare()` で unmapped
        // memory を読み AV → silent terminate → all_done 永久 wait
        // を防ぐため。 順序は `plan_track_removal_ipc` が持っている。
        self.send_track_removal_ipc(&removal_plan);
        self.forget_removed_track_devices(&removal_plan);

        // 範囲は「区間 × 行」しか持たないので、消えたトラックの行を落とすだけ。
        self.prune_selection_lanes();

        // selected_track_ids: subtree に含まれていた id を全て除外。
        // 残りが空なら **削除位置に繰り上がった隣接トラック** を選ぶ
        // (UI 完全選択ゼロを避ける)。
        //
        // r.md #43 review: 旧実装は `tracks.last()` = 曲の最下段に飛んでいた。
        // Delete がトラック面を破壊操作の対象にした今、 last-wins タグは Tracks の
        // まま残る (自動再選択はユーザー操作ではないので `set_track_selection` を
        // 通さない = タグを触らない) ため、 **次の Delete が画面外の最下段トラックを
        // 消す**。 Ableton / REAPER と同じく削除位置の直後 (無ければ直前) へ倒す。
        let subtree_ids_set: std::collections::HashSet<u32> = subtree_ids.iter().copied().collect();
        // r.md #78: 消えたトラックが所有していた変調ソースも道連れにする。
        // ソースはラックで **所有トラックの下にしか列挙されない** ので、 残すと
        // どの画面にも出ず削除できないまま、 生き残ったトラックの param を変調し
        // 続ける (LFO / Random / MSEG / Steps は song 位置の純関数なので、 所有
        // トラックが消えても値を出し続ける)。 接続行をソース側へ寄せて孤児を
        // 潰したのと同じ穴。 `remove_mod_source` が参照 routing の掃除まで担う。
        let orphan_source_ids: Vec<u32> = self
            .song_doc
            .song()
            .mod_sources
            .iter()
            .filter(|m| subtree_ids_set.contains(&m.owner_track_id))
            .map(|m| m.id)
            .collect();
        for id in orphan_source_ids {
            self.remove_mod_source(id);
        }
        // r.md #87: 消えたトラックが口パクのソースだったなら、出力先の口 track に
        // 残った生成物 (`auto_lipsync` の clip / セル) も道連れにする。 残すと
        // **歌が無いのに口だけ動く** — しかも再生成の経路 (`mark_lipsync_dirty`)
        // は binding を持つ track が居なければ何もしないので、二度と片付かない。
        // 消えたのが口 track 側だったときの dangling binding も同じ 1 本が落とす。
        self.reap_orphan_lipsync();
        self.selection.selected_track_ids
            .retain(|id| !subtree_ids_set.contains(id));
        if self.selection.selected_track_ids.is_empty()
            && let Some(id) = self.neighbor_track_id_after_removal(removed_min_idx)
        {
            self.selection.selected_track_ids.push(id);
        }
        // collapsed_groups からも消えた id を除外。
        self.ui_prefs.collapsed_groups
            .retain(|id| !subtree_ids_set.contains(id));
        // r.md #71 (プラグインのコピー / 移動): 消えた track の device を指す選択も
        // 落とす (正しさは読む側の `live_device_ids()` が担保する。 これは後始末)。
        self.prune_device_selection();
        // Audio Editor を安定 key で貼り直す (消えていれば閉じる)。
        self.reanchor_audio_editor(audio_editor_key);
        self.resize_track_peak_display();
    }

    /// トラックを消したあと「選択ゼロ」 を避けるためのフォールバック先。
    ///
    /// `removed_min_idx` は削除で空いた最小 index。 削除後の `song.tracks` で
    /// **その位置に繰り上がった track** (= 消したトラックの直後にあった行) を返し、
    /// 末尾を消したなら 1 つ上、 全部消えたなら `None`。 Ableton / REAPER と同じ
    /// 「削除位置の隣を選ぶ」 挙動で、 Delete を連打しても手元から順に消える。
    ///
    /// **これはユーザーの選択操作ではない**ので、 呼び出し側は
    /// [`Self::set_track_selection`] を通さず `selected_track_ids` に直接入れる
    /// (= last-wins タグを立て直さない)。
    fn neighbor_track_id_after_removal(&self, removed_min_idx: usize) -> Option<u32> {
        let tracks = &self.song_doc.song().tracks;
        tracks
            .get(removed_min_idx)
            .or_else(|| tracks.last())
            .map(|t| t.id)
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
        // audio editor の対象が消える編集なので、退避した key で引き直して畳む。
        let audio_editor_key = self.audio_editor_target_key();
        self.edit_song(|song| song.tracks.swap(a as usize, b as usize));
        self.reanchor_audio_editor(audio_editor_key);
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
        // audio editor の対象が消える編集なので、退避した key で引き直して畳む。
        let audio_editor_key = self.audio_editor_target_key();
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
        self.reanchor_audio_editor(audio_editor_key);
        self.resize_track_peak_display();
    }

    /// トラック面の選択集合を確定する **唯一の口** (r.md #43)。 集合を書き、
    /// last-wins タグを [`EditSurface::Tracks`] にする (= 以後の Delete / Cut /
    /// Copy / D がトラック面を向く)。
    ///
    /// ここを通るのは「ユーザーがトラック面を明示的に操作した」 結果だけ —
    /// ヘッダ / ミキサーストリップの click、 追加 / グループ化 / 解除 / 複製 /
    /// 貼り付けの結果選択。 **削除・undo 後の「選択ゼロを避ける」 自動再選択と、
    /// クリップ選択に追従する [`Self::select_track`] はここを通さない**。 通すと
    /// 「クリップを Del した直後の 2 回目の Del でトラックが消える」 事故になる
    /// (`AppData::edit_surface` はトラック面を非空優先順 fallback から外し、
    /// このタグ経由でしか選ばない)。
    ///
    /// 集合が空になったとき (Ctrl+click で最後の 1 本を外した) はタグを降ろす。
    /// 立てたままだと、 後から暗黙の追従選択が集合を埋めた瞬間に Delete が
    /// トラックを向いてしまう。
    ///
    /// **master 行 (`MASTER_TRACK_ID`) しか入っていない選択でもタグは立てない**
    /// (r.md #43 review)。 master は `song.tracks` に居ない合成行なので、 タグを立てると
    /// `edit_surface` が Tracks を返す → Delete / Cut / Copy / D が「実在 0 件」 で
    /// 空振りし、 **しかも早期分岐なので他の面 (クリップ等) の削除まで殺す**。
    /// 「マスターのインスペクタを見るためにヘッダを click しただけで Delete が
    /// 効かなくなる」 という不可視の故障になるため、 選択表示はしてもタグは立てない。
    pub(crate) fn set_track_selection(&mut self, ids: Vec<u32>) {
        self.selection.selected_track_ids = ids;
        if self.has_deletable_track_selection() {
            self.selection.last_edit_select = Some(EditSurface::Tracks);
        } else if self.selection.last_edit_select == Some(EditSurface::Tracks) {
            self.selection.last_edit_select = None;
        }
    }

    /// r.md #71 (プラグインのコピー / 移動): インスペクタの表示対象トラックだけを
    /// 動かす (= カーソルトラックの移動)。
    ///
    /// [`Self::set_track_selection`] を使わないのは、あちらが last-wins タグを
    /// [`EditSurface::Tracks`] に倒すため。 device をドラッグ中 / 落とした直後に
    /// トラック面へタグが移ると、 次の Delete がトラックを消してしまう。
    /// **選択集合は動かすがタグは触らない** のがここの責務。
    pub(crate) fn focus_inspector_track(&mut self, track_id: u32) {
        if self.cursor_track_id() == Some(track_id) {
            return;
        }
        self.selection.selected_track_ids = vec![track_id];
        self.selection.track_anchor = Some(track_id);
    }

    /// 選択中トラックに `song.tracks` の実在トラックが 1 本でもあるか。
    /// master 行 (合成 id) だけの選択は「トラック面の編集対象なし」 とみなす。
    pub(crate) fn has_deletable_track_selection(&self) -> bool {
        self.selection
            .selected_track_ids
            .iter()
            .any(|id| self.song_doc.song().track_index_by_id(*id).is_some())
    }

    /// トラック面の一括操作 (削除 / cut / copy / 複製) が受け取る id 集合を正規化する。
    /// `song.tracks` に実在する id だけを **入力順のまま** 残し、 重複を落とす。
    ///
    /// master 行の `MASTER_TRACK_ID` は合成行で `song.tracks` に居ないため必ず落ちる。
    /// 空でないことを呼び出し側が確認してから作業に入ることで、 「何も起きないのに
    /// plugin state round-trip を 1 往復させる」 「dirty 化だけする死んだ undo step」
    /// を全経路で防ぐ (r.md #43 review: 以前は delete だけがこのフィルタを持っていた)。
    pub(crate) fn live_track_ids(&self, track_ids: &[u32]) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::with_capacity(track_ids.len());
        for &id in track_ids {
            if self.song_doc.song().track_index_by_id(id).is_some() && !out.contains(&id) {
                out.push(id);
            }
        }
        out
    }

    /// トラックヘッダ / ミキサーストリップの click を解決してトラック選択を更新する
    /// (`apply_select_section` と同 idiom)。 修飾キーの意味論は全選択面共通の
    /// [`SelectModifier`](crate::widgets::select_modifier::SelectModifier) —
    /// 無修飾 = Single / Ctrl = Toggle / Shift = アンカーからの範囲。
    ///
    /// `visible_ids` は **その view の可視順** (arrangement = 折り畳み除外後の行順、
    /// mixer = normal strip 左→右 → return 帯)。 範囲解決の並びだけは view しか
    /// 知らないので引数で受け、 解決ロジック自体はここ 1 本に集約する
    /// (旧実装は arrangement と mixer に別実装が 2 本あった)。
    ///
    /// アンカーは `SelectionState::track_anchor` が所有し、 Single / Toggle で更新、
    /// Shift では据え置き (r.md #35、 `docs/plan_selection_modifiers.md` §4.3)。
    pub fn apply_select_tracks(
        &mut self,
        id: u32,
        modifier: crate::widgets::select_modifier::SelectModifier,
        visible_ids: &[u32],
    ) {
        let prev = self.selection.selected_track_ids.clone();
        let anchor = self.selection.track_anchor;
        let mut next = modifier.resolve(&prev, id, || {
            crate::widgets::select_modifier::range_ordered(visible_ids, anchor?, id)
        });
        // **click した id を末尾へ寄せる** = `cursor_track_id()` (選択順の末尾) が
        // 常に「今 click したトラック」 になる。 range_ordered は表示順の slice を
        // 返すだけなので、 下から上へ Shift+click すると click したのが先頭に来て
        // カーソルが範囲下端に固着し、 インスペクタ / デバイスチェーン /
        // プラグイン追加先が click したストリップと食い違う (r.md #43 review)。
        // 統合前の mixer 実装が持っていた挙動を全選択面へ一般化したもの。
        if let Some(pos) = next.iter().position(|&v| v == id) {
            next.remove(pos);
            next.push(id);
        }
        // タグ / 集合 / 順序の更新は `set_track_selection` 1 本に通す (SSoT)。
        // 「集合が同じなら書き込みを省く」 早期分岐は持たない — 省くと
        // `next` (= 選択順) が捨てられてカーソルが古いトラックに固着する。
        // 省けるコストは Vec 1 本の代入だけで、 `push_edit` は既に積まれている。
        self.set_track_selection(next);
        if modifier.updates_anchor() {
            self.selection.track_anchor = Some(id);
        }
    }

    /// **クリップ選択に追従する暗黙のトラック選択** (単独選択、 安定 `Track::id`)。
    /// 明示的なトラック選択は [`Self::set_track_selection`] /
    /// [`Self::apply_select_tracks`]。
    ///
    /// 引数は **id であって index ではない** (不変条件 1)。 呼び側はほぼ全部
    /// `ClipKey::track_id` を渡すので、 index を取ると `ClipKey` 側の住所を
    /// index に読み替える口がここに 1 つだけ残り、 「クリップを選ぶと隣の
    /// トラックがカーソルになる」 形で出る (id は 1 始まり / index は 0 始まりなので
    /// 常に 1 つずれ、 末尾トラックでは存在せず前の選択が残る)。
    ///
    /// last-wins タグは立てない。 むしろ立っている [`EditSurface::Tracks`] を
    /// **降ろす** — 直前のユーザー意図は「クリップを触った」 なので、 タグが
    /// Tracks のままだとクリップを消すつもりの Delete でトラックが消える。
    /// (`select_clip` / `set_clip_selection` / `select_launcher_cell` は `Clips`
    /// タグを立てた **直後**にここを呼ぶので、 そちらのタグは上書きされない。)
    pub(crate) fn select_track(&mut self, track_id: u32) {
        if self.song_doc.song().track_by_id(track_id).is_none() {
            return;
        }
        if self.selection.selected_track_ids.as_slice() != [track_id] {
            self.selection.selected_track_ids = vec![track_id];
        }
        if self.selection.last_edit_select == Some(EditSurface::Tracks) {
            self.selection.last_edit_select = None;
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
        // 同名なら no-op (r.md #12 の sibling: dirty 化させない)。
        if self
            .song_doc
            .song()
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .is_some_and(|t| t.name == new_name)
        {
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
        // 同名なら no-op (r.md #12 の sibling: dirty 化させない)。
        if self
            .song_doc
            .song()
            .sections
            .iter()
            .find(|s| s.id == id)
            .is_some_and(|s| s.name == new_name)
        {
            return;
        }
        self.edit_song(|song| {
            if let Some(s) = song.sections.iter_mut().find(|s| s.id == id) {
                s.name = new_name;
            }
        });
    }

    /// clip rename の編集バッファ seed / no-op 判定の比較基準となる現在の表示名。
    /// Text clip は先頭の非空 TextEvent 本文、 それ以外は content_name (未設定は "")。
    /// `begin_rename_clip` の pre-fill と `commit_rename_clip` の同名判定で共有する
    /// (DRY: 両者が同じ「現在名」を見ることで、 未編集 commit が確実に no-op になる)。
    fn clip_rename_current(&self, content_id: common::model::ContentId) -> String {
        self.song_doc
            .song()
            .clip_contents
            .get(&content_id)
            .and_then(|c| c.text_events())
            .and_then(|events| events.first())
            .map(|ev| ev.text.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| self.song_doc.song().content_name(content_id).to_string())
    }

    pub(crate) fn begin_rename_clip(&mut self, target: ClipKey) {
        let Some(content_id) = self
            .song_doc.song()
            .track_by_id(target.track_id)
            .and_then(|t| t.clip_by_id(target.clip_id))
            .map(|c| c.content_id)
        else {
            return;
        };
        // 表示されている名前 (= clip_display_label と同じ) を編集開始値にする。
        // Text clip は本文 (= first TextEvent.text) を、 それ以外は content_name を pre-fill。
        self.ui_ephemeral.clip_rename_text = self.clip_rename_current(content_id);
        self.ui_ephemeral.clip_rename = Some(target);
    }

    /// clip rename を確定。 clip 名は表示専用 (audio / plugin processing に無関係)
    /// なので `flush_song_sync` は呼ばない。 名前は `content_id` 単位の SSoT
    /// (`Song.clip_content_names`) に書くので、 同 content を共有する linked clip
    /// 全部が同時に rename される。
    ///
    /// r.md #12: リネーム前後で名前が同じなら **`edit_song` を一切呼ばず** 早期
    /// return する (= epoch を bump しない = dirty マークが付かない)。
    /// r.md #15: 空文字は「名前をクリア」として通す。 非 Text は共有名を削除して
    /// derived / 空表示へ戻し、 Text は本文をクリアする (旧実装は空文字を無条件
    /// 無視して元の名前に張り付いていた)。
    pub(crate) fn commit_rename_clip(&mut self) {
        let Some(target) = self.ui_ephemeral.clip_rename else {
            return;
        };
        self.ui_ephemeral.clip_rename = None;
        let new_name = self.ui_ephemeral.clip_rename_text.trim().to_string();
        self.ui_ephemeral.clip_rename_text.clear();
        let Some(content_id) = self
            .song_doc.song()
            .track_by_id(target.track_id)
            .and_then(|t| t.clip_by_id(target.clip_id))
            .map(|c| c.content_id)
        else {
            return;
        };
        // 同名なら no-op (r.md #12: dirty 化させない)。 begin と同じ「現在名」で
        // 比較するので、 未編集のまま確定した場合も必ず一致して no-op になる。
        if new_name == self.clip_rename_current(content_id) {
            return;
        }
        let is_text = matches!(
            self.song_doc.song().clip_contents.get(&content_id),
            Some(common::model::ClipContent::Text(_))
        );
        if is_text {
            // Text (字幕) clip は本文 (= 全 TextEvent.text) がそのまま表示名。 空文字
            // リネームで本文を丸ごと消すのは破壊的なので **no-op** にする (字幕を空に
            // したいときは inspector の本文編集を使う)。 r.md #15 の「空でクリア」は
            // 名前を別に持つ非 Text clip 向けの挙動。
            if new_name.is_empty() {
                return;
            }
            // set_clip_text_event_content が全 event 書換え + edit buffer resync + dirty を
            // 行う (inspector の content 編集と同経路)。
            self.set_clip_text_event_content(target, new_name);
        } else if new_name.is_empty() {
            // r.md #15: 共有名を削除 → content_name が "" になり、 clip_display_label は
            // derived (歌詞) / 空へ fallback する。
            self.edit_song(|song| song.clear_content_name(content_id));
        } else {
            self.edit_song(|song| song.set_content_name(content_id, new_name));
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

/// r.md #87: トラックの copy/paste/複製が **ランチャーのセル** を落とさないこと。
///
/// セルは `Track.session_clips` に居るので、`clips` だけを歩く走査からは静かに
/// 抜ける。抜けた結果は「貼った直後は一見正しく、次に開くと消える / 独立コピー
/// のはずが元と連動する」という **その場では見えない** 壊れ方をするので、
/// 走査経路そのものをテストで留める。
#[cfg(test)]
mod launcher_track_paste_tests {
    use crate::app::AppData;
    use crate::app_types::track_with;
    use crate::clipboard::{ContentEntry, TrackCopy, TracksCopy};
    use common::model::{
        Clip, ClipContent, ContentId, LaunchSettings, RowPlayback, SessionClip, Song,
    };

    /// 3 列あり、3 列目にセルを 1 つ持つトラックの Song。
    /// 戻り値は `(song, track id, content id, 列 id 列)`。
    fn song_with_cell() -> (Song, u32, ContentId, Vec<u32>) {
        let mut song = Song::default();
        // 先に 1 件捨てて採番をずらす — content id はプロジェクトごとの id 空間
        // なので、貼り先の採番と偶然一致すると「remap していないのに通る」
        // テストになる (この bug で壊れるのはまさに id が衝突したとき)。
        song.alloc_content(ClipContent::default(), String::new());
        let cid = song.alloc_content(ClipContent::default(), String::new());
        let scenes: Vec<u32> = (0..3).map(|_| song.push_scene()).collect();
        let tid = song.alloc_track_id();
        song.tracks.push(track_with(|t| {
            t.id = tid;
            t.name = "T".into();
            t.session_clips = vec![SessionClip {
                scene_id: scenes[2],
                clip: Clip { id: 1, content_id: cid, length_beats: 4.0, ..Clip::default() },
                launch: LaunchSettings::default(),
            }];
            t.launcher = RowPlayback::Launcher { clip_id: 1 };
        }));
        (song, tid, cid, scenes)
    }

    fn copies_of(song: &Song, tid: u32, cid: ContentId, scenes: &[u32]) -> TracksCopy {
        let content = song.clip_contents.get(&cid).cloned().unwrap_or_default();
        TracksCopy {
            tracks: vec![TrackCopy {
                order: 0,
                track: song.track_by_id(tid).unwrap().clone(),
                contents: vec![ContentEntry { content_id: cid, content, name: None }],
            }],
            scenes: scenes.to_vec(),
        }
    }

    #[test]
    fn cross_project_paste_lands_cell_on_the_same_column_index() {
        let (src, tid, cid, scenes) = song_with_cell();
        let copies = copies_of(&src, tid, cid, &scenes);
        // 貼り先は列 1 本しか無い別プロジェクト。
        let mut dst = Song::default();
        dst.push_scene();

        let mut built = AppData::build_pasted_tracks(&mut dst, &copies.tracks, false, false, None);
        AppData::remap_pasted_scenes(&mut dst, &mut built, &copies.scenes, false);

        // 元で 3 列目 (index 2) だったので、貼り先も 3 列目まで実体化して着地する。
        assert_eq!(dst.scenes.len(), 3);
        let cell = &built[0].1.session_clips[0];
        assert_eq!(cell.scene_id, dst.scenes[2].id);
        // 別プロジェクトなのでセルの content も新採番 (元 id を持ち込まない)。
        assert_ne!(cell.clip.content_id, cid);
        assert!(dst.clip_contents.contains_key(&cell.clip.content_id));
    }

    #[test]
    fn cell_with_unresolvable_column_is_dropped_at_paste() {
        let (src, tid, cid, scenes) = song_with_cell();
        let mut copies = copies_of(&src, tid, cid, &scenes);
        // コピー元の列の並びが壊れている (セルの指す列が表に無い) ケース。
        copies.scenes = vec![scenes[0]];
        let mut dst = Song::default();

        let mut built = AppData::build_pasted_tracks(&mut dst, &copies.tracks, false, false, None);
        AppData::remap_pasted_scenes(&mut dst, &mut built, &copies.scenes, false);

        assert!(built[0].1.session_clips.is_empty(), "解けない列のセルは残さない");
        assert_eq!(dst.scenes.len(), 0, "解けないセルのために列を作らない");
    }

    #[test]
    fn independent_duplicate_forks_cell_content() {
        let (mut song, tid, cid, scenes) = song_with_cell();
        let copies = copies_of(&song, tid, cid, &scenes);
        // 同一プロジェクトの独立複製 (Alt+D) = force_independent_content。
        let mut built = AppData::build_pasted_tracks(&mut song, &copies.tracks, true, true, None);
        AppData::remap_pasted_scenes(&mut song, &mut built, &copies.scenes, true);

        let cell = &built[0].1.session_clips[0];
        assert_ne!(cell.clip.content_id, cid, "独立複製はセルの content も fork する");
        assert_eq!(cell.scene_id, scenes[2], "同一プロジェクトなら列はそのまま");
        assert_eq!(song.scenes.len(), 3, "列は増えない");
    }
}
