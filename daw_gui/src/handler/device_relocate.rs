//! r.md #71 (プラグインのコピー / 移動) の本体 — device をチェーン間で運ぶ /
//! クリップボードに載せる / チェーン行の選択を解決する。
//!
//! `handler/devices.rs` (plugin load / GUI / 配線 / 削除) から分けてあるのは
//! 不変条件 9 (サイズ budget) のため。 #71 でここへ足した約 600 実コード行を
//! devices.rs に置いたままだと 1,320 行 = ファイル budget (1,000 行) 超過になる。
//!
//! **運搬の Song 側処理は `relocate_in_song` 1 本**に閉じ込めてある (純関数)。
//! `AppData` 側 (`relocate_devices_inner`) は「plugin state の round-trip 待ちに積む /
//! 結果を受けて session 状態を再キーする / 子プロセスへ流す」だけを持つ。
use crate::app_types::*;
use crate::state::*;
use common::model::InstrumentSource;

impl AppData {
    // -------- r.md #71: device の運搬 (移動 / コピー) ----------------------

    /// 選んだ device を別のチェーンへ運ぶ (移動 / コピー) **唯一の口**。
    ///
    /// 最新の knob 値を Song に書き戻してから実行する必要があるので、 host に
    /// plugin が居るときは `RequestAllStates` の round-trip 待ちに積む
    /// (track copy/cut/duplicate と同 idiom、`app_types.rs` の `DeferredEdit` doc 参照)。
    /// - コピー: 落とし先 device の `initial_state` が「いまのツマミ」になる。
    /// - 移動: instance は作り直さないが、undo snapshot が最新 state を捕まえる。
    pub(crate) fn relocate_devices(&mut self, req: RelocateDevices) {
        if req.device_ids.is_empty() {
            return;
        }
        if !self.song_has_plugin() {
            self.relocate_devices_inner(&req);
            return;
        }
        self.enqueue_state_request(PendingStateRequest::Deferred(
            DeferredEdit::RelocateDevices(req),
        ));
    }

    /// 運搬の本体。 **Song の書き換えは 1 回の `edit_song` に閉じ込める**
    /// (不変条件 5、undo 1 step、epoch bump 1 回)。
    pub(crate) fn relocate_devices_inner(&mut self, req: &RelocateDevices) {
        let RelocateDevices { device_ids, dest_track, dest_index, copy } = req.clone();
        let Some(outcome) = self
            .edit_song(move |song| relocate_in_song(song, &device_ids, dest_track, dest_index, copy))
            .flatten()
        else {
            return;
        };

        // lane の再キーに伴う session 状態の写し替え (B-4)。
        for &(src_track, old_lane, dst_track, new_lane) in &outcome.lane_remap {
            let from = common::model::AutomationLaneKey { track: src_track, lane: old_lane };
            let to = common::model::AutomationLaneKey { track: dst_track, lane: new_lane };
            // 行高 override は session-only だが、 鍵 (track, lane) が両方変わるので
            // 写し替えないと「行高だけ元の位置に取り残されて別 lane に化ける」。
            if let Some(v) = self.ui_prefs.automation_lane_row_overrides.remove(&from) {
                self.ui_prefs.automation_lane_row_overrides.insert(to, v);
            }
            // Z 段階ズームの復元スナップショットにも同じ写像を掛ける
            // (掛けないと X で 1 段戻した瞬間に行高が飛ぶ)。
            for snap in &mut self.ui_ephemeral.arrange_zoom_history {
                if let Some(v) = snap.lane_row_overrides.remove(&from) {
                    snap.lane_row_overrides.insert(to, v);
                }
            }
            for k in &mut self.selection.selected_automation_clips {
                if k.lane_key() == from {
                    k.track = to.track;
                    k.lane = to.lane;
                }
            }
            if let Some(k) = self.selection.automation_clip_anchor.as_mut()
                && k.lane_key() == from
            {
                k.track = to.track;
                k.lane = to.lane;
            }
            for p in &mut self.selection.selected_automation_points {
                if (p.track_id, p.lane_id) == (from.track, from.lane) {
                    p.track_id = to.track;
                    p.lane_id = to.lane;
                }
            }
            if let Some(p) = self.selection.automation_point_anchor.as_mut()
                && (p.track_id, p.lane_id) == (from.track, from.lane)
            {
                p.track_id = to.track;
                p.lane_id = to.lane;
            }
        }
        if !outcome.lane_remap.is_empty() {
            // 「いまポインタがどこを指しているか」 の観測値は写像せず捨てる
            // (次のフレームで再計算される)。
            //
            // 開いていた undo bracket をここで畳む必要は無い — 運ばれたレーンの
            // 既定値欄は次のフレームで元の key のまま描かれなくなるので、
            // `view::scrub_gesture::sweep` が必ず閉じる (寿命は「所有者が今フレーム
            // も描かれている間」)。
            self.ui_ephemeral.arrange_hovered_automation_lane = None;
        }

        for &(src_track, dst_track, device_id) in &outcome.moved_devices {
            rekey_param_gestures(&mut self.recording, src_track, dst_track, device_id);
        }
        if !outcome.moved_devices.is_empty() {
            self.sync_recording_lanes_with_audio();
        }

        // コピーで作った device を host に実体化する。 **finalize を先に積む**
        // (load 応答が先に届いたときに取りこぼさないため)。 `OpenPluginShmem` は
        // `on_plugin_loaded_from_child` が live な `audio_tx` から送る既存経路に乗る。
        for inst in &outcome.created {
            self.ipc.pending_added_plugin_finalize.insert(inst.id, false);
        }
        for inst in outcome.created.clone() {
            self.restore_device(&inst);
        }

        // 移動は plugin_host への IPC が 1 通も要らない (device_id は不変で
        // instance も作り直さない = 音が切れない)。 daw_audio 側は `LoadSong` が
        // `Topology::Recompile` を起こして処理順を再 compile する。 GUI では
        // runner の frame flush が epoch bump を拾うが、 headless / script 経路は
        // frame loop が回らないのでここで明示的に流す (epoch 未変化なら no-op)。
        self.flush_song_sync();

        // 落とした device を選択し、 落とし先のチェーンを表示し続ける
        // (選択とタグの更新は `set_device_selection` 1 本に通す = SSoT)。
        self.selection.device_anchor = outcome.result_ids.last().copied();
        self.set_device_selection(outcome.result_ids);
        self.focus_inspector_track(dest_track);
    }

    // -------- r.md #71: device のクリップボード -----------------------------

    /// Ctrl+C (device 面)。plugin があれば最新 state を取ってから serialize する
    /// ため deferred、無ければ即時。copy は Song 不変なので undo を積まない。
    pub(crate) fn copy_devices(&mut self, device_ids: Vec<u64>) {
        if device_ids.is_empty() {
            return;
        }
        if !self.song_has_plugin() {
            self.copy_devices_inner(&device_ids);
            return;
        }
        self.enqueue_state_request(PendingStateRequest::CopyToClipboard(
            ClipboardCopyRequest::Devices(device_ids),
        ));
    }

    /// Ctrl+X (device 面)。copy → 削除を 1 undo step。
    pub(crate) fn cut_devices(&mut self, device_ids: Vec<u64>) {
        if device_ids.is_empty() {
            return;
        }
        if !self.song_has_plugin() {
            self.cut_devices_inner(&device_ids);
            return;
        }
        self.enqueue_state_request(PendingStateRequest::Deferred(DeferredEdit::CutDevices {
            device_ids,
        }));
    }

    /// copy 本体。最新 state 込みの live song から該当 device を serialize して
    /// `pending_clipboard_write` に積む (view が次フレーム OS clipboard へ flush)。
    pub(crate) fn copy_devices_inner(&mut self, device_ids: &[u64]) {
        self.write_devices_to_clipboard(device_ids, "コピー");
    }

    /// cut 本体。serialize → `pending_clipboard_write` → 削除。呼び出し側で
    /// undo snapshot 済み (deferred 経由 or 即時 fallback)。
    pub(crate) fn cut_devices_inner(&mut self, device_ids: &[u64]) {
        self.write_devices_to_clipboard(device_ids, "カット");
        self.remove_devices_inner(device_ids);
    }

    /// copy / cut 共通: serialize して `pending_clipboard_write` に積み、
    /// status を出す。 `verb` は「コピー」/「カット」。
    fn write_devices_to_clipboard(&mut self, device_ids: &[u64], verb: &str) {
        let Some((json, count, dropped)) = self.serialize_devices_to_envelope(device_ids) else {
            return;
        };
        self.ui_ephemeral.pending_clipboard_write = Some(json);
        self.ui_ephemeral.status_message = if dropped == 0 {
            format!("{verb}: {count} プラグイン")
        } else {
            // 黙って切らない — 「貼ったら音色が違う」の原因が見えなくなる。
            format!(
                "クリップボードには大きすぎるため {dropped} 件のプラグイン設定を除いて\
                 {verb}しました (ドラッグで運ぶと設定ごと移せます)"
            )
        };
    }

    /// 指定 device 群を `DeviceCopy` list に組み立てて envelope へ入れる。
    /// 戻り値は `(json, 件数, blob を落とした device 数)`。
    ///
    /// blob (`state` / `ara_archive`) は base64 テキストとして OS クリップボードへ
    /// 流れるので [`CLIPBOARD_BLOB_BUDGET`](crate::clipboard::CLIPBOARD_BLOB_BUDGET)
    /// を超える分は運ばない。 落とす順序は決定的に **(1) 全 device の
    /// `ara_archive`、(2) 全 device の `state`** で、(1) で収まればそこで止める。
    fn serialize_devices_to_envelope(&self, device_ids: &[u64]) -> Option<(String, usize, usize)> {
        let song = self.song_doc.song();
        // 表示順 (= チェーン順) を保つため、 呼び出し側の並びをそのまま使う。
        let mut out: Vec<crate::clipboard::DeviceCopy> = Vec::new();
        for &id in device_ids {
            let Some((source_track, index)) = find_device_by_id(song, id) else {
                continue;
            };
            let Some(inst) = device_at(song, source_track, index) else {
                continue;
            };
            out.push(crate::clipboard::DeviceCopy {
                order: out.len(),
                source_track,
                device: inst.clone(),
            });
        }
        if out.is_empty() {
            return None;
        }
        let blob_bytes = |ds: &[crate::clipboard::DeviceCopy]| -> usize {
            ds.iter()
                .map(|d| {
                    d.device.state.as_ref().map_or(0, |s| s.len())
                        + d.device.ara_archive.as_ref().map_or(0, |s| s.len())
                })
                .sum()
        };
        let mut dropped = 0usize;
        if blob_bytes(&out) > crate::clipboard::CLIPBOARD_BLOB_BUDGET {
            for d in &mut out {
                if d.device.ara_archive.take().is_some() {
                    dropped += 1;
                }
            }
        }
        if blob_bytes(&out) > crate::clipboard::CLIPBOARD_BLOB_BUDGET {
            for d in &mut out {
                if d.device.state.take().is_some() {
                    dropped += 1;
                }
            }
        }
        let count = out.len();
        let json = crate::clipboard::ClipboardEnvelope::new(
            song.project_id,
            crate::clipboard::ClipboardPayload::Devices(out),
        )
        .to_json()?;
        Some((json, count, dropped))
    }

    /// Ctrl+V (device 面)。貼り先は「いまインスペクタに出ているチェーン」で、
    /// 挿入位置は **選んでいるプラグインの直前**、選択が無ければ末尾 (Ableton 流)。
    /// 戻り値は貼り付けた件数。
    pub fn paste_devices(
        &mut self,
        devices: Vec<crate::clipboard::DeviceCopy>,
        dest_track: u32,
    ) -> usize {
        if devices.is_empty() {
            return 0;
        }
        let Some(chain_len) = self
            .song_doc
            .song()
            .fx_chain_by_track_id(dest_track)
            .map(<[_]>::len)
        else {
            return 0;
        };
        // 挿入位置: 表示チェーンの中で選択されている device の最小 index。
        // `live_device_ids` を通すので、 別トラックの選択が残っていても末尾に
        // 落ちる (= 画面と一致する)。
        let selected = self.live_device_ids();
        let dest_index = self
            .song_doc
            .song()
            .fx_chain_by_track_id(dest_track)
            .and_then(|chain| {
                chain
                    .iter()
                    .position(|d| selected.contains(&d.id))
                    .map(|i| i as u32)
            })
            .unwrap_or(chain_len as u32);

        let mut ordered = devices;
        ordered.sort_by_key(|d| d.order);
        let created = self.edit_song(move |song| {
            let mut created: Vec<common::model::PluginInstance> = Vec::new();
            for dc in &ordered {
                let mut inst = dc.device.clone();
                inst.id = song.alloc_device_id();
                // 別トラックへ運んだ ARA アーカイブは復元できない (persistent_id が
                // 元トラックのクリップを指す) ので落として解析し直させる。
                if dc.source_track != dest_track {
                    inst.ara_archive = None;
                }
                resolve_aux_refs_after_paste(song, &mut inst);
                created.push(inst);
            }
            let at = (dest_index as usize).min(
                song.fx_chain_by_track_id(dest_track)
                    .map_or(0, <[_]>::len),
            );
            if let Some(chain) = song.fx_chain_by_track_id_mut(dest_track) {
                chain.splice(at..at, created.iter().cloned());
            }
            apply_dest_side_effects(song, dest_track, &created);
            created
        });
        let Some(created) = created else {
            return 0;
        };
        for inst in &created {
            self.ipc.pending_added_plugin_finalize.insert(inst.id, false);
        }
        for inst in created.clone() {
            self.restore_device(&inst);
        }
        self.flush_song_sync();
        // 貼った device を選択に倒す (更新は `set_device_selection` 1 本に通す)。
        self.selection.device_anchor = created.last().map(|d| d.id);
        let n = created.len();
        self.set_device_selection(created.into_iter().map(|d| d.id).collect());
        n
    }

    // -------- r.md #71: device 選択 ----------------------------------------

    /// チェーン行 click の解決 (無修飾 = Single / Ctrl = Toggle / Shift = 範囲)。
    /// 範囲の並びは表示チェーンの device id 列で、 解決自体は全選択面共通の
    /// [`range_ordered`](crate::widgets::select_modifier::range_ordered) に任せる。
    pub(crate) fn apply_select_device(
        &mut self,
        device_id: u64,
        modifier: crate::widgets::select_modifier::SelectModifier,
    ) {
        let order: Vec<u64> = self.inspector_chain().iter().map(|e| e.device_id).collect();
        // `prev` は **正規化済み** を渡す (異トラックの stale id は最初の click で落ちる)。
        let prev = self.live_device_ids();
        let next = modifier.resolve(&prev, device_id, || {
            self.selection
                .device_anchor
                .and_then(|a| crate::widgets::select_modifier::range_ordered(&order, a, device_id))
        });
        self.set_device_selection(next);
        if modifier.updates_anchor() {
            self.selection.device_anchor = Some(device_id);
        }
    }

    /// device 選択の setter。 ここを通るのは **明示的なチェーン操作だけ**
    /// (行 click / 運搬 / 貼り付けの結果)。 空になったら last-wins タグを降ろす
    /// — 残すと `edit_surface` が Devices を返し続け、 次の Delete が
    /// 「実在 0 件」 で空振りして他の面の削除まで殺す。
    pub fn set_device_selection(&mut self, ids: Vec<u64>) {
        self.selection.selected_device_ids = ids;
        if self.selection.selected_device_ids.is_empty() {
            if self.selection.last_edit_select == Some(EditSurface::Devices) {
                self.selection.last_edit_select = None;
            }
        } else {
            self.selection.last_edit_select = Some(EditSurface::Devices);
        }
    }

    /// device が消える経路 (削除 / track 削除 / project 切替 / undo-redo) の後始末。
    /// 実在しない id を落とし、 空になったらタグを降ろす。
    ///
    /// **正しさの担保ではない** — それは読む側の [`Self::live_device_ids`] が持つ。
    /// ここは保持した集合が無限に育たないようにするだけ。
    pub(crate) fn prune_device_selection(&mut self) {
        let song = self.song_doc.song();
        let alive: Vec<u64> = self
            .selection
            .selected_device_ids
            .iter()
            .copied()
            .filter(|id| find_device_by_id(song, *id).is_some())
            .collect();
        if alive.len() != self.selection.selected_device_ids.len() {
            self.set_device_selection(alive);
        }
        if self
            .selection
            .device_anchor
            .is_some_and(|id| find_device_by_id(self.song_doc.song(), id).is_none())
        {
            self.selection.device_anchor = None;
        }
    }
}

/// [`AppData::relocate_devices_inner`] が `edit_song` の中で組み立てる結果。
struct RelocateOutcome {
    /// 挿入順に並んだ結果の device id (移動なら元 id、コピーなら新 id)。選択に使う。
    result_ids: Vec<u64>,
    /// 移送した automation lane の再キー表 `(src_track, old_lane, dest_track, new_lane)`。
    lane_remap: Vec<(u32, u32, u32, u32)>,
    /// **トラックを跨いで**移した device `(src_track, dest_track, device_id)`。
    /// recording gesture の再キーに使う (gesture の鍵は `(track_id, target)` で、
    /// lane が無くても gesture だけ立っていることがあるので、 lane 由来ではなく
    /// device 由来で洗う)。
    moved_devices: Vec<(u32, u32, u64)>,
    /// コピーで新規に作った device (host へ実体化する対象)。
    created: Vec<common::model::PluginInstance>,
}

/// 運搬の Song 側処理 (純関数)。 `None` = 落とし先チェーンが無い / 対象ゼロ。
fn relocate_in_song(
    song: &mut common::model::Song,
    device_ids: &[u64],
    dest_track: u32,
    dest_index: u32,
    copy: bool,
) -> Option<RelocateOutcome> {
    // 解決できない id は捨てる (削除済み device への stale 要求は正常系)。
    let mut targets: Vec<(u64, u32, u32)> = device_ids
        .iter()
        .filter_map(|&id| find_device_by_id(song, id).map(|(t, i)| (id, t, i)))
        .collect();
    if targets.is_empty() {
        return None;
    }
    // 落とし先チェーンが無ければ中止 (存在確認だけで値は使わない)。
    song.fx_chain_by_track_id(dest_track)?;
    // 同一チェーン内の移動は「並べ替え」として正当なので、 無変化の早期 return は
    // しない (普通に処理する)。

    let mut outcome = RelocateOutcome {
        result_ids: Vec::new(),
        lane_remap: Vec::new(),
        moved_devices: Vec::new(),
        created: Vec::new(),
    };

    if copy {
        let mut copies: Vec<common::model::PluginInstance> = Vec::new();
        for &(_, src_track, index) in &targets {
            let Some(src) = device_at(song, src_track, index) else {
                continue;
            };
            let mut inst = src.clone();
            inst.id = song.alloc_device_id();
            // `state` (= いまのツマミ) は引き継ぐ (`Arc` の clone なのでコストゼロ)。
            // ARA アーカイブはトラックを跨いだら復元できないので捨てる。
            if src_track != dest_track {
                inst.ara_archive = None;
            }
            retarget_self_track_aux(&mut inst, src_track, dest_track);
            copies.push(inst);
        }
        let at = (dest_index as usize).min(song.fx_chain_by_track_id(dest_track)?.len());
        if let Some(chain) = song.fx_chain_by_track_id_mut(dest_track) {
            chain.splice(at..at, copies.iter().cloned());
        }
        // 副作用は **dest 側だけ** (src はそのまま残るので降ろさない)。
        apply_dest_side_effects(song, dest_track, &copies);
        outcome.result_ids = copies.iter().map(|d| d.id).collect();
        outcome.created = copies;
        return Some(outcome);
    }

    // ---- 移動 ----
    // 挿入位置の補正: dest と同じチェーンから `dest_index` より前で抜いた個数だけ
    // 引く。 忘れると同一チェーン内の移動が 1 個ずれる。
    let removed_before_dest = targets
        .iter()
        .filter(|&&(_, t, i)| t == dest_track && i < dest_index)
        .count();
    // src チェーンごとに index 降順で抜く (前から抜くと後続の index がずれる)。
    targets.sort_by_key(|t| std::cmp::Reverse(t.2));
    let mut taken: Vec<(common::model::PluginInstance, u32)> = Vec::new();
    for &(_, src_track, index) in &targets {
        let Some(chain) = song.fx_chain_by_track_id_mut(src_track) else {
            continue;
        };
        if (index as usize) >= chain.len() {
            continue;
        }
        taken.push((chain.remove(index as usize), src_track));
    }
    // 元の指定順 (= チェーン表示順) に戻す。
    taken.reverse();

    let mut moved: Vec<common::model::PluginInstance> = Vec::new();
    // src track ごとに「そのトラックから出ていった device」 を控える
    // (副作用の判定は種類ごとなので、 出ていった種類だけを見る)。
    let mut left_by_track: std::collections::HashMap<u32, Vec<String>> =
        std::collections::HashMap::new();
    for (mut inst, src_track) in taken {
        if src_track != dest_track {
            // automation lane / mod_routing を新しい所有者へ移す。 lane を元
            // トラックに置いたまま device だけ移すと、 その lane は永久に効かない
            // (`daw_audio/src/automation.rs` が track から lane を引いてから
            //  device_id で絞るため)。
            let (lanes, routings) = extract_device_bindings(song, src_track, inst.id);
            for lane in lanes {
                let old_id = lane.id;
                let new_id = push_lane_to(song, dest_track, lane);
                outcome
                    .lane_remap
                    .push((src_track, old_id, dest_track, new_id));
            }
            for routing in routings {
                // `ModRouting.source_id` は `Song.mod_sources` の song-global id
                // なのでそのまま生きる (再キー不要)。
                push_routing_to(song, dest_track, routing);
            }
            outcome.moved_devices.push((src_track, dest_track, inst.id));
            left_by_track
                .entry(src_track)
                .or_default()
                .push(inst.plugin_id.clone());
            // ARA アーカイブは元トラックのクリップを指す persistent_id で作られて
            // いるので、 別トラックへ持ち込むと復元できない (= 解析し直す)。
            inst.ara_archive = None;
            retarget_self_track_aux(&mut inst, src_track, dest_track);
        }
        moved.push(inst);
    }

    let at = ((dest_index as usize).saturating_sub(removed_before_dest))
        .min(song.fx_chain_by_track_id(dest_track)?.len());
    outcome.result_ids = moved.iter().map(|d| d.id).collect();
    if let Some(chain) = song.fx_chain_by_track_id_mut(dest_track) {
        chain.splice(at..at, moved.iter().cloned());
    }
    // 副作用の対称化: src 側は「他に残っていなければ降ろす」、 dest 側は立てる。
    for (src_track, left) in left_by_track {
        apply_src_side_effects(song, src_track, &left);
    }
    apply_dest_side_effects(song, dest_track, &moved);
    Some(outcome)
}

/// この device を指す automation lane / mod routing を所有者から **抜き取る**
/// (retain ではなく取り出し — 移送先へ渡すため)。
fn extract_device_bindings(
    song: &mut common::model::Song,
    track_id: u32,
    device_id: u64,
) -> (
    Vec<common::model::AutomationLane>,
    Vec<common::model::ModRouting>,
) {
    let hits = |target: &common::model::AutomationTarget| {
        matches!(
            target,
            common::model::AutomationTarget::PluginParam { device_id: d, .. } if *d == device_id
        )
    };
    let (lanes_src, routings_src): (
        &mut Vec<common::model::AutomationLane>,
        &mut Vec<common::model::ModRouting>,
    ) = if track_id == common::model::MASTER_TRACK_ID {
        (&mut song.song_lanes, &mut song.song_mod_routings)
    } else {
        match song.tracks.iter_mut().find(|t| t.id == track_id) {
            Some(t) => (&mut t.automation_lanes, &mut t.mod_routings),
            None => return (Vec::new(), Vec::new()),
        }
    };
    let mut lanes = Vec::new();
    let mut i = 0;
    while i < lanes_src.len() {
        if hits(&lanes_src[i].target) {
            lanes.push(lanes_src.remove(i));
        } else {
            i += 1;
        }
    }
    let mut routings = Vec::new();
    let mut i = 0;
    while i < routings_src.len() {
        if hits(&routings_src[i].target) {
            routings.push(routings_src.remove(i));
        } else {
            i += 1;
        }
    }
    (lanes, routings)
}

/// 移送した lane を新しい所有者へ push する。 **lane id は必ず再採番する** —
/// 据え置くと dest 側の既存 lane と衝突し、 選択や行高 override が silent に
/// 別 lane へ付け替わる。 戻り値は新 lane id。
fn push_lane_to(
    song: &mut common::model::Song,
    dest_track: u32,
    mut lane: common::model::AutomationLane,
) -> u32 {
    if dest_track == common::model::MASTER_TRACK_ID {
        let id = song.alloc_song_lane_id();
        lane.id = id;
        song.song_lanes.push(lane);
        id
    } else if let Some(t) = song.tracks.iter_mut().find(|t| t.id == dest_track) {
        let id = t.alloc_lane_id();
        lane.id = id;
        t.automation_lanes.push(lane);
        id
    } else {
        lane.id
    }
}

fn push_routing_to(
    song: &mut common::model::Song,
    dest_track: u32,
    routing: common::model::ModRouting,
) {
    if dest_track == common::model::MASTER_TRACK_ID {
        song.song_mod_routings.push(routing);
    } else if let Some(t) = song.tracks.iter_mut().find(|t| t.id == dest_track) {
        t.mod_routings.push(routing);
    }
}

/// 自トラックを指していた aux 参照を移動先へ貼り替える。 他トラックを指すものは
/// 触らない (= その配線はユーザーが意図して張ったもの)。
fn retarget_self_track_aux(
    inst: &mut common::model::PluginInstance,
    src_track: u32,
    dest_track: u32,
) {
    for slot in &mut inst.aux_inputs {
        if let Some(route) = slot
            && route.tap.source_track == src_track
        {
            route.tap.source_track = dest_track;
        }
    }
    for slot in &mut inst.aux_outputs {
        if let Some(route) = slot
            && route.dest_track == src_track
        {
            route.dest_track = dest_track;
        }
    }
}

/// 貼り付け (別プロジェクト由来もありうる) の aux 参照解決。 実在しない track を
/// 指す route は落とす。 **`aux_outputs` も見る** — `build_pasted_tracks` は
/// `aux_inputs` しか見ていないが、それは取りこぼしなので真似しない。
fn resolve_aux_refs_after_paste(
    song: &common::model::Song,
    inst: &mut common::model::PluginInstance,
) {
    for slot in &mut inst.aux_inputs {
        if let Some(route) = slot
            && song.track_by_id(route.tap.source_track).is_none()
        {
            *slot = None;
        }
    }
    for slot in &mut inst.aux_outputs {
        if let Some(route) = slot
            && song.track_by_id(route.dest_track).is_none()
        {
            *slot = None;
        }
    }
}

/// 落とし先の副作用: VOICEVOX builtin が入ったら vocal track 化、 Transform が
/// 入ったら `group_transform` を初期化する (追加側 `handler/mixer.rs` と同じ規則)。
/// master (`MASTER_TRACK_ID`) は Track ではないので副作用を持たない。
fn apply_dest_side_effects(
    song: &mut common::model::Song,
    dest_track: u32,
    placed: &[common::model::PluginInstance],
) {
    if dest_track == common::model::MASTER_TRACK_ID {
        return;
    }
    let has_voicevox = placed
        .iter()
        .any(|d| d.plugin_id == common::plugin_db::BUILTIN_ID_VOICEVOX);
    let has_transform = placed
        .iter()
        .any(|d| d.plugin_id == common::video_fx::TRANSFORM_ID);
    let Some(track) = song.tracks.iter_mut().find(|t| t.id == dest_track) else {
        return;
    };
    if has_voicevox {
        track.source = InstrumentSource::Vocal;
    }
    if has_transform && track.group_transform.is_none() {
        track.group_transform = Some(common::model::GroupTransform::default());
    }
}

/// 運び出した側の副作用: **出ていった種類**について、 同じ種類の device が
/// 1 つも残っていなければ降ろす (削除側 `remove_devices_inner` と同じ規則 —
/// 規則は 1 つで済ませる)。 `left` は そのトラックから出ていった `plugin_id` 列。
fn apply_src_side_effects(song: &mut common::model::Song, src_track: u32, left: &[String]) {
    if src_track == common::model::MASTER_TRACK_ID {
        return;
    }
    let left_voicevox = left
        .iter()
        .any(|id| id == common::plugin_db::BUILTIN_ID_VOICEVOX);
    let left_transform = left.iter().any(|id| id == common::video_fx::TRANSFORM_ID);
    let Some(track) = song.tracks.iter_mut().find(|t| t.id == src_track) else {
        return;
    };
    if left_voicevox
        && !track
            .devices
            .iter()
            .any(|d| d.plugin_id == common::plugin_db::BUILTIN_ID_VOICEVOX)
    {
        track.source = InstrumentSource::None;
    }
    if left_transform
        && !track
            .devices
            .iter()
            .any(|d| d.plugin_id == common::video_fx::TRANSFORM_ID)
    {
        track.group_transform = None;
    }
}

/// 移動した device の「録音中の param gesture」 を新しい所有者トラックへ移す。
///
/// 移送しないと `daw_audio/src/automation.rs` の skip 判定が旧 track でも新 track でも
/// 外れ、 curve eval とユーザーのノブ操作が二重に効く。
fn rekey_param_gestures(
    recording: &mut crate::state::RecordingState,
    src_track: u32,
    dst_track: u32,
    device_id: u64,
) {
    let owns = |key: &(u32, common::model::AutomationTarget)| {
        key.0 == src_track
            && matches!(
                &key.1,
                common::model::AutomationTarget::PluginParam { device_id: d, .. }
                    if *d == device_id
            )
    };
    let rekey_set =
        |set: &mut std::collections::HashSet<(u32, common::model::AutomationTarget)>| {
            let hits: Vec<_> = set.iter().filter(|k| owns(k)).cloned().collect();
            for k in hits {
                set.remove(&k);
                set.insert((dst_track, k.1));
            }
        };
    rekey_set(&mut recording.active_param_gestures);
    rekey_set(&mut recording.latched_param_gestures);
    let hits: Vec<_> = recording.recording_last_beat.keys().filter(|k| owns(k)).cloned().collect();
    for k in hits {
        if let Some(beat) = recording.recording_last_beat.remove(&k) {
            recording.recording_last_beat.insert((dst_track, k.1), beat);
        }
    }
}
