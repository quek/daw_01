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
//!
//! r.md #110 (Parallel): 運ぶ単位は [`common::model::Device`] (plugin か Parallel 丸ごと)、
//! 落とし先は [`ChainRef`] (top-level か Parallel 内 chain)。 Parallel を自分の中の chain へ
//! 落とすのは循環なので拒む。
use crate::app_types::*;
use crate::handler::device_guard::{self, DeviceOp};
use crate::state::*;
use common::model::{ChainRef, Device, InstrumentSource, OrdinalPolicy, plugins};

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
        // r.md #129 (Q5): 組み込みは普通のドラッグでは所属トラックの最上位の中でしか動かせない
        // (Ctrl のコピーはどこへでも)。 運べるものが無ければ round-trip も積まない。
        let device_ids = self.permit_or_explain(&req.device_ids, DeviceOp::Relocate { dest: req.dest, copy: req.copy });
        if device_ids.is_empty() {
            return;
        }
        let req = RelocateDevices { device_ids, ..req };
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
        let RelocateDevices { device_ids, dest, dest_index, copy } = req.clone();
        let Some(dest_track) = self.cur.song_doc.song().chain_owner_track(dest) else {
            return;
        };
        let Some(outcome) = self
            .edit_song(move |song| {
                // `Default` はここ (実行時の Song) で解決する — deferred 実行でも古くならない。
                let dest_index = u32::try_from(dest_index.resolve(song, dest)?).ok()?;
                relocate_in_song(song, &device_ids, dest, dest_index, copy)
            })
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
            if let Some(v) = self.cur.view.automation_lane_row_overrides.remove(&from) {
                self.cur.view.automation_lane_row_overrides.insert(to, v);
            }
            // Z 段階ズームの復元スナップショットにも同じ写像を掛ける
            // (掛けないと X で 1 段戻した瞬間に行高が飛ぶ)。
            for snap in &mut self.cur.peph.arrange_zoom_history {
                if let Some(v) = snap.lane_row_overrides.remove(&from) {
                    snap.lane_row_overrides.insert(to, v);
                }
            }
            for k in &mut self.cur.selection.selected_automation_clips {
                if k.lane_key() == from {
                    k.track = to.track;
                    k.lane = to.lane;
                }
            }
            if let Some(k) = self.cur.selection.automation_clip_anchor.as_mut()
                && k.lane_key() == from
            {
                k.track = to.track;
                k.lane = to.lane;
            }
            for p in &mut self.cur.selection.selected_automation_points {
                if (p.track_id, p.lane_id) == (from.track, from.lane) {
                    p.track_id = to.track;
                    p.lane_id = to.lane;
                }
            }
            if let Some(p) = self.cur.selection.automation_point_anchor.as_mut()
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
            self.cur.peph.arrange_hovered_automation_lane = None;
        }

        for &(src_track, dst_track, device_id) in &outcome.moved_devices {
            rekey_param_gestures(&mut self.cur.recording, src_track, dst_track, device_id);
        }
        if !outcome.moved_devices.is_empty() {
            self.sync_recording_lanes_with_audio();
        }
        // r.md #129 (Q18): 移動した行は Par を閉じた状態から始める (同じトラック内の並べ替えも)。
        // コピーは新 id なので最初から閉じている。
        self.close_rack_panels_of(&outcome.moved_nodes);
        self.note_dropped_cyclic_routes(outcome.dropped_routes);

        // コピーで作った device を host に実体化する。 **finalize を先に積む**
        // (load 応答が先に届いたときに取りこぼさないため)。 `OpenPluginShmem` は
        // `on_plugin_loaded_from_child` が live な `audio_tx` から送る既存経路に乗る。
        // r.md #110: Parallel を複製したら中の plugin 全部。
        let created_plugins: Vec<common::model::PluginInstance> =
            plugins(&outcome.created).cloned().collect();
        for inst in &created_plugins {
            self.cur.pipc.pending_added_plugin_finalize.insert(inst.id, false);
        }
        for inst in &created_plugins {
            self.restore_device(inst);
        }

        // 移動は plugin_host への IPC が 1 通も要らない (device_id は不変で
        // instance も作り直さない = 音が切れない)。 daw_audio 側は `LoadSong` が
        // `Topology::Recompile` を起こして処理順を再 compile する。 GUI では
        // runner の frame flush が epoch bump を拾うが、 headless / script 経路は
        // frame loop が回らないのでここで明示的に流す (epoch 未変化なら no-op)。
        self.flush_song_sync();

        // 落とした device を選択し、 落とし先のチェーンを表示し続ける
        // (選択とタグの更新は `set_device_selection` 1 本に通す = SSoT)。
        self.cur.selection.device_anchor = outcome.result_ids.last().copied();
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

    /// Ctrl+X (device 面)。copy → 削除を 1 undo step。 組み込み内蔵 device は切り取れない
    /// (クリップボードにも載せない、 Q5)。
    pub fn cut_devices(&mut self, device_ids: Vec<u64>) {
        let device_ids = self.permit_or_explain(&device_ids, DeviceOp::Cut);
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
        // deferred 実行でも実行時の Song で組み込みを落とし直す (書き込みと削除は同じ id 列)。
        let ids = device_guard::permitted_ids(self.cur.song_doc.song(), device_ids, DeviceOp::Cut);
        if ids.is_empty() {
            return;
        }
        self.write_devices_to_clipboard(&ids, "カット");
        self.remove_devices_inner(&ids);
    }

    /// copy / cut 共通: serialize して `pending_clipboard_write` に積み、
    /// status を出す。 `verb` は「コピー」/「カット」。
    fn write_devices_to_clipboard(&mut self, device_ids: &[u64], verb: &str) {
        let Some((json, count, dropped)) = self.serialize_devices_to_envelope(device_ids) else {
            return;
        };
        self.ui_ephemeral.pending_clipboard_write = Some(json);
        self.ui_ephemeral.status_message = if dropped == 0 {
            format!("{verb}: {count} デバイス")
        } else {
            // 黙って切らない — 「貼ったら音色が違う」の原因が見えなくなる。
            format!(
                "クリップボードには大きすぎるため {dropped} 件のプラグイン設定を除いて\
                 {verb}しました (ドラッグで運ぶと設定ごと移せます)"
            )
        };
    }

    /// 指定 device 群を `DeviceCopy` list に組み立てて envelope へ入れる。
    /// 戻り値は `(json, 件数, blob を落とした plugin 数)`。
    ///
    /// blob (`state` / `ara_archive`) は base64 テキストとして OS クリップボードへ
    /// 流れるので [`CLIPBOARD_BLOB_BUDGET`](crate::clipboard::CLIPBOARD_BLOB_BUDGET)
    /// を超える分は運ばない。 落とす順序は決定的に **(1) 全 plugin の
    /// `ara_archive`、(2) 全 plugin の `state`** で、(1) で収まればそこで止める。
    /// r.md #110: Parallel は中身ごと 1 件。
    fn serialize_devices_to_envelope(&self, device_ids: &[u64]) -> Option<(String, usize, usize)> {
        let song = self.cur.song_doc.song();
        // 表示順 (= チェーン順) を保つため、 呼び出し側の並びをそのまま使う。
        let mut out: Vec<crate::clipboard::DeviceCopy> = Vec::new();
        for &id in device_ids {
            let Some(source_track) = song.device_owner_track(id) else {
                continue;
            };
            let Some(dev) = song.device_by_id(id) else {
                continue;
            };
            out.push(crate::clipboard::DeviceCopy {
                order: out.len(),
                source_track,
                device: dev.clone(),
            });
        }
        if out.is_empty() {
            return None;
        }
        let blob_bytes = |ds: &[crate::clipboard::DeviceCopy]| -> usize {
            ds.iter()
                .flat_map(|d| plugins(std::slice::from_ref(&d.device)))
                .map(|p| {
                    p.state.as_ref().map_or(0, |s| s.len())
                        + p.ara_archive.as_ref().map_or(0, |s| s.len())
                })
                .sum()
        };
        let mut dropped = 0usize;
        if blob_bytes(&out) > crate::clipboard::CLIPBOARD_BLOB_BUDGET {
            for d in &mut out {
                common::model::for_each_plugin_mut(std::slice::from_mut(&mut d.device), &mut |p| {
                    if p.ara_archive.take().is_some() {
                        dropped += 1;
                    }
                });
            }
        }
        if blob_bytes(&out) > crate::clipboard::CLIPBOARD_BLOB_BUDGET {
            for d in &mut out {
                common::model::for_each_plugin_mut(std::slice::from_mut(&mut d.device), &mut |p| {
                    if p.state.take().is_some() {
                        dropped += 1;
                    }
                });
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
    /// 挿入位置は **選んでいる device の直前** (その device が居る chain へ)、選択が
    /// 無ければ top-level の既定位置 (Q6: 組み込みの手前)。 戻り値は貼り付けた件数。
    pub fn paste_devices(
        &mut self,
        devices: Vec<crate::clipboard::DeviceCopy>,
        dest_track: u32,
    ) -> usize {
        if devices.is_empty() {
            return 0;
        }
        let song = self.cur.song_doc.song();
        if song.fx_chain_by_track_id(dest_track).is_none() {
            return 0;
        }
        // 挿入位置: 選択されている device (表示順の先頭) の直前。 `live_device_ids` を
        // 通すので、 別トラックの選択が残っていても末尾に落ちる (= 画面と一致する)。
        let (dest, dest_index) = self
            .live_device_ids()
            .first()
            .and_then(|&id| song.find_device(id))
            .map_or((ChainRef::Track(dest_track), InsertAt::Default), |(chain, i)| {
                (chain, InsertAt::Index(i as u32))
            });

        let mut ordered = devices;
        ordered.sort_by_key(|d| d.order);
        let created = self.edit_song(move |song| {
            // 挿入位置は貼る前の Song で解決する (`Default` は組み込みの手前)。
            let at = dest_index.resolve(song, dest).unwrap_or(0);
            let mut created: Vec<Device> = Vec::new();
            for dc in &ordered {
                let mut dev = dc.device.clone();
                let from_other_track = dc.source_track != dest_track;
                common::model::for_each_plugin_mut(std::slice::from_mut(&mut dev), &mut |inst| {
                    // 別トラックへ運んだ ARA アーカイブは復元できない (persistent_id が
                    // 元トラックのクリップを指す) ので落として解析し直させる。
                    if from_other_track {
                        inst.ara_archive = None;
                    }
                });
                resolve_aux_refs_after_paste(song, &mut dev);
                created.push(dev);
            }
            // 新 id・内蔵は追加分・番号 (`Fresh`) は一括で 1 回 (r.md #129 §5.7)。
            song.prepare_device_copies(dest_track, &mut created);
            if let Some(chain) = song.chain_devices_mut(dest) {
                chain.splice(at..at, created.iter().cloned());
            }
            // 持ち込んだサイドチェインのうち依存が循環するものは落とす (§5.9)。
            let dropped = song.drop_cyclic_aux_routes(&node_ids_of(&created));
            apply_dest_side_effects(song, dest_track, &created);
            (created, dropped)
        });
        let Some((created, dropped)) = created else {
            return 0;
        };
        self.note_dropped_cyclic_routes(dropped);
        let created_plugins: Vec<common::model::PluginInstance> =
            plugins(&created).cloned().collect();
        for inst in &created_plugins {
            self.cur.pipc.pending_added_plugin_finalize.insert(inst.id, false);
        }
        for inst in &created_plugins {
            self.restore_device(inst);
        }
        self.flush_song_sync();
        // 貼った device を選択に倒す (更新は `set_device_selection` 1 本に通す)。
        self.cur.selection.device_anchor = created.last().map(Device::id);
        let n = created.len();
        self.set_device_selection(created.iter().map(Device::id).collect());
        n
    }

    /// 貼り付け / コピー / 運搬で持ち込んだサイドチェインのうち、依存が循環するので落とした本数を status に出す
    /// (黙って消すと、後で Comp を ON にしたときに初めて配線が無いことに気付く)。0 本なら何もしない。
    pub(crate) fn note_dropped_cyclic_routes(&mut self, dropped: usize) {
        if dropped > 0 {
            self.ui_ephemeral.status_message =
                format!("依存が循環するサイドチェイン配線を {dropped} 本外しました");
        }
    }

    // -------- r.md #71: device 選択 ----------------------------------------

    /// チェーン行 click の解決 (無修飾 = Single / Ctrl = Toggle / Shift = 範囲)。
    /// 範囲の並びは表示チェーンの行 (plugin / Parallel / chain) の id 列で、 解決自体は
    /// 全選択面共通の [`range_ordered`](crate::widgets::select_modifier::range_ordered) に任せる。
    pub(crate) fn apply_select_device(
        &mut self,
        device_id: u64,
        modifier: crate::widgets::select_modifier::SelectModifier,
    ) {
        let order: Vec<u64> = self.chain_rows().iter().filter_map(|r| r.select_id()).collect();
        // `prev` は **正規化済み** を渡す (異トラックの stale id は最初の click で落ちる)。
        let prev = self.live_device_ids();
        let next = modifier.resolve(&prev, device_id, || {
            self.cur.selection
                .device_anchor
                .and_then(|a| crate::widgets::select_modifier::range_ordered(&order, a, device_id))
        });
        self.set_device_selection(next);
        if modifier.updates_anchor() {
            self.cur.selection.device_anchor = Some(device_id);
        }
    }

    /// device 選択の setter。 ここを通るのは **明示的なチェーン操作だけ**
    /// (行 click / 運搬 / 貼り付けの結果)。 空になったら last-wins タグを降ろす
    /// — 残すと `edit_surface` が Devices を返し続け、 次の Delete が
    /// 「実在 0 件」 で空振りして他の面の削除まで殺す。
    pub fn set_device_selection(&mut self, ids: Vec<u64>) {
        self.cur.selection.selected_device_ids = ids;
        if self.cur.selection.selected_device_ids.is_empty() {
            if self.cur.selection.last_edit_select == Some(EditSurface::Devices) {
                self.cur.selection.last_edit_select = None;
            }
        } else {
            self.cur.selection.last_edit_select = Some(EditSurface::Devices);
        }
    }
}

/// [`AppData::relocate_devices_inner`] が `edit_song` の中で組み立てる結果。
struct RelocateOutcome {
    /// 挿入順に並んだ結果の device id (移動なら元 id、コピーなら新 id)。選択に使う。
    result_ids: Vec<u64>,
    /// 移送した automation lane の再キー表 `(src_track, old_lane, dest_track, new_lane)`。
    lane_remap: Vec<(u32, u32, u32, u32)>,
    /// **トラックを跨いで**移した node (plugin / native / Parallel / chain) `(src_track, dest_track, id)`。
    /// recording gesture の再キーに使う (gesture の鍵は `(track_id, target)` で、
    /// lane が無くても gesture だけ立っていることがあるので、 lane 由来ではなく
    /// device 由来で洗う)。
    moved_devices: Vec<(u32, u32, u64)>,
    /// 移動した node 全部 (同じトラック内の並べ替えを含む)。Par を閉じる対象 (Q18)。
    moved_nodes: Vec<u64>,
    /// コピーで新規に作った device (中の plugin を host へ実体化する対象)。
    created: Vec<Device>,
    /// 依存が循環するので落としたサイドチェイン配線の本数 (status に出す)。
    dropped_routes: usize,
}

/// `devices` 以下の node id 全部 (plugin / native / Parallel / chain)。
fn node_ids_of(devices: &[Device]) -> Vec<u64> {
    let mut ids = Vec::new();
    common::model::for_each_node_id(devices, &mut |id| ids.push(id));
    ids
}

/// 運搬の Song 側処理 (純関数)。 `None` = 落とし先チェーンが無い / 対象ゼロ。
fn relocate_in_song(
    song: &mut common::model::Song,
    device_ids: &[u64],
    dest: ChainRef,
    dest_index: u32,
    copy: bool,
) -> Option<RelocateOutcome> {
    let dest_track = song.chain_owner_track(dest)?;
    // 解決できない id は捨てる (削除済み device への stale 要求は正常系)。
    // 運べない id (Parallel を自分の中の chain へ / 組み込みを所属トラックの最上位の外へ) も落とす
    // — 規則は `Song::can_relocate` 1 本 (Rack の drop slot の判定と同じ)。
    let targets: Vec<(u64, u32, ChainRef, usize)> = device_ids
        .iter()
        .filter_map(|&id| {
            if !song.can_relocate(id, dest, copy) {
                return None;
            }
            let (chain, index) = song.find_device(id)?;
            let owner = song.chain_owner_track(chain)?;
            Some((id, owner, chain, index))
        })
        .collect();
    if targets.is_empty() {
        return None;
    }
    // 同一チェーン内の移動は「並べ替え」として正当なので、 無変化の早期 return は
    // しない (普通に処理する)。

    let mut outcome = RelocateOutcome {
        result_ids: Vec::new(),
        lane_remap: Vec::new(),
        moved_devices: Vec::new(),
        moved_nodes: Vec::new(),
        created: Vec::new(),
        dropped_routes: 0,
    };

    if copy {
        let mut copies: Vec<Device> = Vec::new();
        for &(id, src_track, _, _) in &targets {
            let Some(src) = song.device_by_id(id) else {
                continue;
            };
            let mut dev = src.clone();
            let cross = src_track != dest_track;
            common::model::for_each_plugin_mut(std::slice::from_mut(&mut dev), &mut |inst| {
                // `state` (= いまのツマミ) は引き継ぐ (`Arc` の clone なのでコストゼロ)。
                // ARA アーカイブはトラックを跨いだら復元できないので捨てる。
                if cross {
                    inst.ara_archive = None;
                }
            });
            retarget_self_track_aux(&mut dev, src_track, dest_track);
            copies.push(dev);
        }
        // 新 id・内蔵は追加分 (組み込みのコピーも「Comp 2」)・番号 (`Fresh`) は一括で 1 回 (§5.7)。
        song.prepare_device_copies(dest_track, &mut copies);
        let at = (dest_index as usize).min(song.chain_devices(dest)?.len());
        if let Some(chain) = song.chain_devices_mut(dest) {
            chain.splice(at..at, copies.iter().cloned());
        }
        // 持ち込んだサイドチェインのうち依存が循環するものは落とす (§5.9)。
        outcome.dropped_routes = song.drop_cyclic_aux_routes(&node_ids_of(&copies));
        // 副作用は **dest 側だけ** (src はそのまま残るので降ろさない)。
        apply_dest_side_effects(song, dest_track, &copies);
        outcome.result_ids = copies.iter().map(Device::id).collect();
        outcome.created = copies;
        return Some(outcome);
    }

    // ---- 移動 ----
    // 挿入位置の補正: dest と同じチェーンから `dest_index` より前で抜いた個数だけ
    // 引く。 忘れると同一チェーン内の移動が 1 個ずれる。
    let removed_before_dest = targets
        .iter()
        .filter(|&&(_, _, chain, i)| chain == dest && (i as u32) < dest_index)
        .count();
    let ordinals = cross_track_ordinals(song, &targets, dest_track);
    // 指定順 (= チェーン表示順) に抜く。 id で抜くので index のずれは起きない。
    let mut taken: Vec<(Device, u32)> = Vec::new();
    for &(id, src_track, _, _) in &targets {
        if let Some(dev) = song.remove_device(id) {
            taken.push((dev, src_track));
        }
    }

    let mut moved: Vec<Device> = Vec::new();
    // src track ごとに「そのトラックから出ていった plugin」 を控える
    // (副作用の判定は種類ごとなので、 出ていった種類だけを見る)。
    let mut left_by_track: std::collections::HashMap<u32, Vec<String>> =
        std::collections::HashMap::new();
    for (mut dev, src_track) in taken {
        common::model::for_each_node_id(std::slice::from_ref(&dev), &mut |id| outcome.moved_nodes.push(id));
        if src_track != dest_track {
            common::model::for_each_native_mut(std::slice::from_mut(&mut dev), &mut |n| {
                if let Some(&o) = ordinals.get(&n.id) {
                    n.ordinal = o;
                }
            });
            move_device_bindings(song, &dev, src_track, dest_track, &mut outcome);
            common::model::for_each_plugin_mut(std::slice::from_mut(&mut dev), &mut |inst| {
                left_by_track
                    .entry(src_track)
                    .or_default()
                    .push(inst.plugin_id.clone());
                // ARA アーカイブは元トラックのクリップを指す persistent_id で作られて
                // いるので、 別トラックへ持ち込むと復元できない (= 解析し直す)。
                inst.ara_archive = None;
            });
            retarget_self_track_aux(&mut dev, src_track, dest_track);
        }
        moved.push(dev);
    }

    let at = ((dest_index as usize).saturating_sub(removed_before_dest))
        .min(song.chain_devices(dest)?.len());
    outcome.result_ids = moved.iter().map(Device::id).collect();
    if let Some(chain) = song.chain_devices_mut(dest) {
        chain.splice(at..at, moved.iter().cloned());
    }
    if !outcome.moved_devices.is_empty() {
        // トラックを跨いで持ち込んだサイドチェインと、運んだ Parallel の chain を他トラックから読む配線
        // (chain の持ち主が変わって辺が変わる) のうち、依存が循環するものを落とす (§5.9)。同じトラック内の
        // 並べ替えは辺を変えないので判定しない。
        let crossed: Vec<u64> = outcome.moved_devices.iter().map(|&(_, _, id)| id).collect();
        outcome.dropped_routes = song.drop_cyclic_aux_routes(&crossed);
    }
    // 副作用の対称化: src 側は「他に残っていなければ降ろす」、 dest 側は立てる。
    for (src_track, left) in left_by_track {
        apply_src_side_effects(song, src_track, &left);
    }
    apply_dest_side_effects(song, dest_track, &moved);
    Some(outcome)
}

/// トラックを跨いで運ぶ内蔵 device の番号 (K9: 空いていれば保つ、 衝突すれば空き番号)。
///
/// 使用中の番号は **dest の木 (同じトラックから一緒に動かすものを含む)** なので、 抜く前の Song で
/// 決める (抜いた後に決めると、 並べ替え中の同トラックの番号と衝突しうる)。 戻り値は native id → 番号。
fn cross_track_ordinals(
    song: &common::model::Song,
    targets: &[(u64, u32, ChainRef, usize)],
    dest_track: u32,
) -> std::collections::HashMap<u64, u16> {
    let mut cross: Vec<Device> = targets
        .iter()
        .filter(|t| t.1 != dest_track)
        .filter_map(|t| song.device_by_id(t.0).cloned())
        .collect();
    song.assign_native_ordinals(dest_track, &mut cross, OrdinalPolicy::KeepIfFree);
    let mut out = std::collections::HashMap::new();
    common::model::for_each_native(&cross, &mut |n| {
        out.insert(n.id, n.ordinal);
    });
    out
}

/// track を跨いで運ぶ device (Parallel なら中の plugin / native / chain 全部) の automation
/// lane / mod routing を `src_track` から `dest_track` へ移す。 lane を元トラックに置いたまま
/// device だけ移すと、 その lane は永久に効かない (engine は持ち主の store から lane を引く)
/// うえに、 SongDoc の `enforce_edit_invariants` が dangling として消す。
///
/// 対象は **運ぶ device 以下の node id 全部** に `bound_node_id` で束縛された住所 (住所の種類を
/// ここで列挙しない)。 lane の置き場の分岐は `Song::param_stores_mut` / `push_lane` 1 か所。
fn move_device_bindings(
    song: &mut common::model::Song,
    dev: &Device,
    src_track: u32,
    dest_track: u32,
    outcome: &mut RelocateOutcome,
) {
    let mut node_ids: Vec<u64> = Vec::new();
    common::model::for_each_node_id(std::slice::from_ref(dev), &mut |id| node_ids.push(id));
    let (lanes, mut routings) = extract_bindings(song, src_track, |target| {
        target.bound_node_id().is_some_and(|id| node_ids.contains(&id))
    });
    move_lanes(song, lanes, src_track, dest_track, outcome);
    // r.md #89: 移した変調の **深さ** を指すレーン / 変調も一緒に運ぶ。深さの深さ (= 連鎖) も
    // あり得るので、抜き取るものが無くなるまで回す (src の routing 数は毎周必ず減るので必ず止まる)。
    while !routings.is_empty() {
        // `ModRouting.source_id` は `Song.mod_sources` の song-global id なのでそのまま生きる
        // (再キー不要)。`ModRouting.id` も Song-global なので移送で変えない
        // (`ModRoutingDepth` の参照が切れる)。
        let moved_ids: Vec<u32> = routings.iter().map(|r| r.id).filter(|&id| id != 0).collect();
        if let Some((_, dest)) = song.param_stores_mut(dest_track) {
            dest.append(&mut routings);
        }
        let (dep_lanes, dep_routings) = extract_bindings(song, src_track, |target| {
            matches!(
                target,
                common::model::AutomationTarget::ModRoutingDepth { routing_id } if moved_ids.contains(routing_id)
            )
        });
        move_lanes(song, dep_lanes, src_track, dest_track, outcome);
        routings = dep_routings;
    }
    outcome.moved_devices.extend(node_ids.into_iter().map(|id| (src_track, dest_track, id)));
}

/// 抜き取った lane を `dest_track` の store へ積み、 再キー表に記録する。 **lane id は必ず
/// 再採番する** (`push_lane`) — 据え置くと dest 側の既存 lane と衝突し、 選択や行高 override が
/// silent に別 lane へ付け替わる。
fn move_lanes(
    song: &mut common::model::Song,
    lanes: Vec<common::model::AutomationLane>,
    src_track: u32,
    dest_track: u32,
    outcome: &mut RelocateOutcome,
) {
    for lane in lanes {
        let old_id = lane.id;
        if let Some(new_id) = song.push_lane(dest_track, lane) {
            outcome.lane_remap.push((src_track, old_id, dest_track, new_id));
        }
    }
}

/// `hits` が真になる target を持つ lane / routing を `track_id` の store から抜き取る
/// (retain ではなく取り出し — 移送先へ渡すため)。
fn extract_bindings(
    song: &mut common::model::Song,
    track_id: u32,
    hits: impl Fn(&common::model::AutomationTarget) -> bool,
) -> (
    Vec<common::model::AutomationLane>,
    Vec<common::model::ModRouting>,
) {
    let Some((lanes_src, routings_src)) = song.param_stores_mut(track_id) else {
        return (Vec::new(), Vec::new());
    };
    let (lanes, kept): (Vec<_>, Vec<_>) = std::mem::take(lanes_src).into_iter().partition(|l| hits(&l.target));
    *lanes_src = kept;
    let (routings, kept): (Vec<_>, Vec<_>) =
        std::mem::take(routings_src).into_iter().partition(|r| hits(&r.target));
    *routings_src = kept;
    (lanes, routings)
}

/// 自トラックを指していた aux 参照を移動先へ貼り替える。 他トラックを指すものは
/// 触らない (= その配線はユーザーが意図して張ったもの)。 chain source (同 track の
/// Parallel 内 chain) は運搬で chain が同 track に残るとは限らないが、 id は不変なので
/// そのまま (dangling なら compile が黙って落とす)。
///
/// 対象は `dev` 以下の全 device (Parallel なら中身全部)。aux 入力は plugin と内蔵 Comp / Bus Comp
/// 共通の slot (`for_each_aux_slot_mut`)、aux 出力は plugin だけ。
fn retarget_self_track_aux(dev: &mut Device, src_track: u32, dest_track: u32) {
    common::model::for_each_aux_slot_mut(std::slice::from_mut(dev), &mut |_, _, slot| {
        if let Some(route) = slot
            && let common::model::TapSource::Track(t) = &mut route.tap.source
            && *t == src_track
        {
            *t = dest_track;
        }
    });
    common::model::for_each_plugin_mut(std::slice::from_mut(dev), &mut |inst| {
        for slot in &mut inst.aux_outputs {
            if let Some(route) = slot
                && route.dest_track == src_track
            {
                route.dest_track = dest_track;
            }
        }
    });
}

/// 貼り付け (別プロジェクト由来もありうる) の aux 参照解決。 実在しない track /
/// chain を指す route は落とす。 **`aux_outputs` も見る** — `build_pasted_tracks` は
/// `aux_inputs` しか見ていないが、それは取りこぼしなので真似しない。
/// 対象は `dev` 以下の全 device (aux 入力は内蔵 Comp / Bus Comp の SC も含む)。
fn resolve_aux_refs_after_paste(song: &common::model::Song, dev: &mut Device) {
    common::model::for_each_aux_slot_mut(std::slice::from_mut(dev), &mut |_, _, slot| {
        if let Some(route) = slot {
            let alive = match route.tap.source {
                common::model::TapSource::Track(t) => song.track_by_id(t).is_some(),
                common::model::TapSource::Chain(c) => song.chain_by_id(c).is_some(),
            };
            if !alive {
                *slot = None;
            }
        }
    });
    common::model::for_each_plugin_mut(std::slice::from_mut(dev), &mut |inst| {
        for slot in &mut inst.aux_outputs {
            if let Some(route) = slot
                && song.track_by_id(route.dest_track).is_none()
            {
                *slot = None;
            }
        }
    });
}

/// 落とし先の副作用: VOICEVOX builtin が入ったら vocal track 化、 Transform が
/// 入ったら `group_transform` を初期化する (追加側 `handler/mixer.rs` と同じ規則)。
/// master (`MASTER_TRACK_ID`) は Track ではないので副作用を持たない。
fn apply_dest_side_effects(
    song: &mut common::model::Song,
    dest_track: u32,
    placed: &[Device],
) {
    if dest_track == common::model::MASTER_TRACK_ID {
        return;
    }
    let has_voicevox = plugins(placed).any(|d| d.plugin_id == common::plugin_db::BUILTIN_ID_VOICEVOX);
    let has_transform = plugins(placed).any(|d| d.plugin_id == common::video_fx::TRANSFORM_ID);
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
    if left_voicevox && !track.plugins().any(|d| d.plugin_id == common::plugin_db::BUILTIN_ID_VOICEVOX) {
        track.source = InstrumentSource::None;
    }
    if left_transform && !track.plugins().any(|d| d.plugin_id == common::video_fx::TRANSFORM_ID) {
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
        key.0 == src_track && key.1.bound_node_id() == Some(device_id)
    };
    // 所有者の面は保ったまま鍵だけ付け替える (付け替えた面が引き続き End を出せる)。
    let hits: Vec<_> = recording.active_param_gestures.keys().filter(|k| owns(k)).cloned().collect();
    for k in hits {
        if let Some(surface) = recording.active_param_gestures.remove(&k) {
            recording.active_param_gestures.insert((dst_track, k.1), surface);
        }
    }
    let hits: Vec<_> = recording.latched_param_gestures.iter().filter(|k| owns(k)).cloned().collect();
    for k in hits {
        recording.latched_param_gestures.remove(&k);
        recording.latched_param_gestures.insert((dst_track, k.1));
    }
    let hits: Vec<_> = recording.recording_last_beat.keys().filter(|k| owns(k)).cloned().collect();
    for k in hits {
        if let Some(beat) = recording.recording_last_beat.remove(&k) {
            recording.recording_last_beat.insert((dst_track, k.1), beat);
        }
    }
}
