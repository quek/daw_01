//! load 時 (と disk / IPC の信頼境界) に `Song` の不変条件を回復する正規化 — 入口の
//! [`Song::normalize_after_load`]、値域の clamp ([`Song::sanitize_ranges`])、安定 id の採番と
//! 旧 positional 参照の写像 ([`Song::ensure_ids`])。
//!
//! wire に載らないロジックだけを持つ (`common/build.rs` の `WIRE_SOURCES` 対象外)。

use super::*;

impl Song {
    /// Clamp persisted scalar fields into valid ranges. The persistence
    /// layer is the trust boundary where data crosses back in from disk /
    /// IPC, so this is the single place that defends every downstream
    /// divisor (`samples_per_beat = sr*60/bpm`, `tsig_denom`, ...) against
    /// `0` / negative / `NaN` values from a corrupt or hand-edited file.
    /// `NaN` slips past naive `x <= 0.0` guards (`NaN <= 0.0` is `false`),
    /// so every check is written as `!is_finite() || out_of_range`.
    /// Idempotent.
    pub fn sanitize_ranges(&mut self) {
        if !self.bpm.is_finite() {
            self.bpm = 120.0;
        } else {
            self.bpm = self.bpm.clamp(1.0, 1000.0);
        }
        // Numerator 1..=32, denominator must be a power-of-two beat unit.
        self.time_sig.0 = self.time_sig.0.clamp(1, 32);
        if !matches!(self.time_sig.1, 1 | 2 | 4 | 8 | 16) {
            self.time_sig.1 = 4;
        }
        if !(self.length_beats.is_finite() && self.length_beats >= 0.0) {
            self.length_beats = 0.0;
        }
        if !(self.video_framerate.is_finite() && self.video_framerate > 0.0) {
            self.video_framerate = default_video_framerate();
        }
        if self.video_resolution.0 == 0 || self.video_resolution.1 == 0 {
            self.video_resolution = default_video_resolution();
        }
        // r.md #110: Parallel chain の gain / pan は RT が snapshot からそのまま掛ける
        // (IPC の `SetChain*` は境界で clamp するが、LoadSong は素通し) ので、
        // ここで値域に収める。
        let mut fix_chain = |c: &mut ParallelChain| {
            c.gain = if c.gain.is_finite() { c.gain.clamp(0.0, MAX_TRACK_GAIN) } else { 1.0 };
            c.pan = if c.pan.is_finite() { c.pan.clamp(-1.0, 1.0) } else { 0.0 };
        };
        for t in &mut self.tracks {
            for_each_chain_mut(&mut t.devices, &mut fix_chain);
        }
        for_each_chain_mut(&mut self.master_fx_chain, &mut fix_chain);
        let mut fix_parallel = |r: &mut Parallel| {
            r.out_gain = if r.out_gain.is_finite() { r.out_gain.clamp(0.0, MAX_TRACK_GAIN) } else { 1.0 };
            // r.md #112: クロスオーバーも RT がそのまま係数に使う。
            r.split.sanitize();
            // r.md #114: Selector のアクティブ chain を実在する id に揃える。
            r.normalize_selector();
        };
        for t in &mut self.tracks {
            for_each_parallel_mut(&mut t.devices, &mut fix_parallel);
        }
        for_each_parallel_mut(&mut self.master_fx_chain, &mut fix_parallel);
        // r.md #129: 内蔵 device と master Limiter の値も RT がそのまま使う (LoadSong は素通し)。
        // **値の clamp だけ**で構造 (builtin / 番号 / 配線) には触らない — daw_audio の LoadSong でも走る。
        for t in &mut self.tracks {
            for_each_native_mut(&mut t.devices, &mut NativeDevice::sanitize);
        }
        for_each_native_mut(&mut self.master_fx_chain, &mut NativeDevice::sanitize);
        self.master_limiter.sanitize();
        // r.md #116 / #117: LFO の Shape / Jitter / Smooth / Delay / Fade In と ADSR の時定数も RT が
        // そのまま使う (GUI の編集は clamp 済だが LoadSong は素通し)。
        for m in &mut self.mod_sources {
            match &mut m.kind {
                ModSourceKind::Lfo(c) => c.sanitize(),
                ModSourceKind::Adsr(c) => c.sanitize(),
                _ => {}
            }
        }
    }

    /// v24: `project_id == 0` (未採番 / 旧 file / `Song::default`) なら
    /// 新規採番する。既に非 0 なら触らない (idempotent) ので、New で採番済みの song に
    /// `normalize_after_load` を再走させても上書きしない。uuid v4 の下位 64bit を使う
    /// (別起動・別マシンでも衝突しない。`0` sentinel は引き直す)。
    pub fn ensure_project_id(&mut self) {
        if self.project_id == 0 {
            self.project_id = uuid::Uuid::new_v4().as_u128() as u64;
            if self.project_id == 0 {
                self.project_id = 1;
            }
        }
    }

    /// Single entry point for all post-load normalization. Re-establishes
    /// every invariant the rest of the codebase assumes about a freshly
    /// loaded song — value-range sanity, content / source migration, stable
    /// ids, and sort order — so `project::load`'s return value is always
    /// self-consistent regardless of how the file was produced. Idempotent.
    ///
    /// 返り値は **クリップの重なりを解消したか** (= 開いた時点で `*` を立てるべきか)。
    /// それ以外の正規化はすべて冪等な no-op になるよう作られているので、
    /// 「開いただけで `*`」 が立つ理由はここ 1 つに絞られる。
    pub fn normalize_after_load(&mut self) -> bool {
        self.ensure_project_id();
        self.sanitize_ranges();
        self.ensure_clip_contents();
        self.ensure_audio_source_ids();
        self.ensure_video_source_ids();
        self.ensure_image_source_ids();
        self.ensure_ids();
        self.ensure_midi_binding_inputs();
        // r.md #89 / #129: dangling な信号経路 / lane / routing / MIDI binding を掃除する。id 採番と組み込みの
        // 正規化の後 (track id の remap / routing id / device id が確定してから解決する) でなければならない。
        self.prune_dangling_refs();
        self.normalize_session();
        self.ensure_scale_changes_sorted();
        self.ensure_automation_points_sorted();
        // 重なり解消は id 採番の後 (分割断片に id を振るため)、 overlay の
        // カバレッジ補完の前 (窓を縮めてから覆う長さを決めるため)。
        let resolved = self.resolve_clip_overlaps();
        self.ensure_overlay_event_coverage();
        resolved
    }

    /// 全トラックのクリップの重なりを上書き規則で解消する
    /// ([`Track::resolve_clip_overlaps`])。 **冪等** — 2 回目は `false` を返す。
    pub fn resolve_clip_overlaps(&mut self) -> bool {
        let mut changed = false;
        for track in &mut self.tracks {
            if track.resolve_clip_overlaps() {
                changed = true;
            }
        }
        changed
    }

    /// overlay clip (image / video / text) は「clip 長 = 表示長」が
    /// 不変条件。 単一 (または末尾) の event がその clip 長に届かないと、 clip
    /// 範囲内でも event 範囲を抜けて途中で消える (= clip を伸ばしたが event が
    /// 追従していない既存 .daw を自動修復する)。
    ///
    /// 各 content を、 それを参照する **最長** clip の長さまで届くよう
    /// extend-only で覆う ([`ClipContent::ensure_event_covers_clip`])。 linked
    /// clip でより短い clip があっても、 その clip は自分の clip 範囲 gate で
    /// clamp されるので安全。 idempotent。 Audio / Midi / Automation は no-op。
    pub fn ensure_overlay_event_coverage(&mut self) {
        // content ごとに、 それを参照する clip の **窓の末尾** (content-local) の
        // 最大値を集める (r.md #44: 左端 trim した clip は content の先の方を見せる)。
        let mut max_len: HashMap<ContentId, f64> = HashMap::new();
        for track in &self.tracks {
            // v35 (r.md #87): **`all_clips` を通す** — ランチャーのセル
            // (`session_clips`) も同じ content を指すので、セルの窓のほうが長いと
            // event が届かず「撃った直後だけ絵が出て残りは真っ暗」になる
            // (`content` を数えるものは `all_clips` を通す、`Track::all_clips` の契約)。
            for clip in track.all_clips() {
                let e = max_len.entry(clip.content_id).or_insert(0.0);
                let win_end = clip.content_offset_beats + clip.length_beats;
                if win_end > *e {
                    *e = win_end;
                }
            }
        }
        for (cid, len) in max_len {
            if let Some(content) = self.clip_contents.get_mut(&cid) {
                content.ensure_event_covers_clip(len);
            }
        }
    }

    /// v29 migration: `AutomationTarget` 内の旧 positional 参照
    /// (`legacy_device_index` / `legacy_send_idx`) を安定 id (`device_id` /
    /// `send_id`) へ写像する。 新形式 (legacy = None) は no-op、 範囲外
    /// index は sentinel (0) のまま残す (= 「解決不能な参照」 として
    /// 消費側が無視できる)。
    fn remap_target_ids(target: &mut AutomationTarget, device_ids: &[u64], send_ids: &[u32]) {
        match target {
            AutomationTarget::PluginParam {
                device_id,
                legacy_device_index,
                ..
            } => {
                if let Some(idx) = legacy_device_index.take()
                    && *device_id == 0
                    && let Some(&id) = device_ids.get(idx as usize)
                {
                    *device_id = id;
                }
            }
            AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain {
                send_id,
                legacy_send_idx,
            }) => {
                if let Some(idx) = legacy_send_idx.take()
                    && *send_id == 0
                    && let Some(&id) = send_ids.get(idx as usize)
                {
                    *send_id = id;
                }
            }
            _ => {}
        }
    }

    /// Re-assign stable ids to all tracks / clips after loading an older
    /// project file (or any save predating the id schema). Idempotent:
    /// records that already have non-zero ids are left untouched, and
    /// `next_*_id` counters are bumped above the highest seen id.
    ///
    /// sentinel track の id を振り直したときの参照の張り替えは
    /// `patch_remapped_track_refs` が担う。
    pub fn ensure_ids(&mut self) {
        // 旧 3-split device chain (midi_fx_chain / instrument / fx_chain) の `devices` への
        // 平坦化、および automation lane / midi_binding の旧 `slot: PluginSlot` →
        // positional `device_index` 解決は、load 時の JSON 前処理
        // (`project::migrate_legacy_device_chains`、§10) が担う。ここでは前処理が残した
        // positional `legacy_device_index` / `legacy_send_idx` を安定 device_id / send_id へ
        // 写像する (下記 remap pass。新形式 = legacy = None は no-op)。

        // (v25): 旧 `group_transform` を持つトラックにチェーン上の
        // Transform 配置 device を補う。これで「動かす変形」がチェーンの 1 device
        // として現れ、`resolve_track_transform` の device-gate で効く（device を抜けば
        // 変換が無効）。値・automation・変調は GroupTransform 系のまま（破壊的な値
        // migration は不要）。idempotent（device 既存 / group_transform 無しは no-op）。
        for track in &mut self.tracks {
            let has_transform =
                any_plugin(&track.devices, &mut |d| d.plugin_id == crate::video_fx::TRANSFORM_ID);
            if track.group_transform.is_some() && !has_transform {
                // r.md #129 Q6: 組み込み Comp / EQ より上 (末尾に push すると EQ の後ろに入る)。
                let at = default_insert_index_in(&track.devices, false);
                track.devices.insert(at, Device::Plugin(PluginInstance::with_ports(
                    crate::video_fx::TRANSFORM_ID.to_string(),
                    crate::plugin_format::PluginFormat::Builtin,
                    crate::port_config::PortConfig {
                        has_video_input: true,
                        has_video_output: true,
                        ..Default::default()
                    },
                )));
            }
        }

        // Pass 1: assign fresh ids to sentinel tracks, recording the
        // (old_id → new_id) remap so refs can be patched in pass 2.
        let mut id_remap: std::collections::HashMap<u32, u32> =
            std::collections::HashMap::new();
        for track in &mut self.tracks {
            if track.id == 0 {
                let new_id = self.ids.next_track_id.max(1);
                self.ids.next_track_id = new_id + 1;
                id_remap.insert(0, new_id);
                track.id = new_id;
            } else if track.id >= self.ids.next_track_id {
                self.ids.next_track_id = track.id + 1;
            }
            track.ensure_clip_ids();
            track.ensure_lane_ids();
        }
        if self.ids.next_track_id == 0 {
            self.ids.next_track_id = 1;
        }

        // Phase 5: song-level lane の id も同様に採番。 sentinel (0) のみ
        // 上書き、 既存非 0 id は触らず counter を bump するだけ。
        // (review) id_remap guard より **前** に置く — sentinel track が無い通常
        // ロードでも lane / mod_source の採番・counter 正規化は必要。
        for lane in &mut self.song_lanes {
            if lane.id == 0 {
                let new_id = self.ids.next_song_lane_id.max(1);
                self.ids.next_song_lane_id = new_id + 1;
                lane.id = new_id;
            } else if lane.id >= self.ids.next_song_lane_id {
                self.ids.next_song_lane_id = lane.id + 1;
            }
            // lane 内 clip ids も担保。**`AutomationLane::ensure_clip_ids` を通す** —
            // ベタ書きすると `clips` しか見ず、ランチャーのセル (`session_clips`) が
            // 採番・重複解消・`next_clip_id` の bump から漏れる (同じ id のセルが
            // 2 つできると `RowPlayback` がどちらを指すか決まらない)。
            lane.ensure_clip_ids();
        }
        if self.ids.next_song_lane_id == 0 {
            self.ids.next_song_lane_id = 1;
        }

        // docs/plan_modulation.md §8: mod_source id も song_lanes と同様に採番。
        // sentinel (0) のみ上書き、 既存非 0 id は触らず counter を bump する。
        for ms in &mut self.mod_sources {
            if ms.id == 0 {
                let new_id = self.ids.next_mod_source_id.max(1);
                self.ids.next_mod_source_id = new_id + 1;
                ms.id = new_id;
            } else if ms.id >= self.ids.next_mod_source_id {
                self.ids.next_mod_source_id = ms.id + 1;
            }
        }
        if self.ids.next_mod_source_id == 0 {
            self.ids.next_mod_source_id = 1;
        }

        // r.md #89: ModRouting id も同 idiom で採番する。sentinel (0) と **重複 id** を
        // 上書きする — 重複を放置すると `ModRoutingDepth { routing_id }` がどちらの
        // 変調を指すか決まらない (device id と同じ理由)。
        {
            let mut next = self.ids.next_mod_routing_id;
            let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
            let mut assign = |r: &mut ModRouting| {
                if r.id == 0 || !seen.insert(r.id) {
                    let new_id = next.max(1);
                    next = new_id + 1;
                    r.id = new_id;
                    seen.insert(r.id);
                } else if r.id >= next {
                    next = r.id + 1;
                }
            };
            for track in &mut self.tracks {
                for r in &mut track.mod_routings {
                    assign(r);
                }
            }
            for r in &mut self.song_mod_routings {
                assign(r);
            }
            self.ids.next_mod_routing_id = next.max(1);
        }

        // v29: device 安定 id (`PluginInstance::id`) を採番する。 track devices
        // と master_fx_chain が Song-global の `next_device_id` を共有。
        // sentinel (0) と **重複 id** を上書きし、 それ以外は counter を bump する
        // だけ (他 allocator と同 idiom)。 r.md #110: Parallel / chain の id も同じ空間で
        // 同じ規則 (`for_each_node_id_mut` が plugin / parallel / chain を全部訪問する)。
        {
            // 0 に採番する前に「既存の最大 id + 1」まで押し上げる。counter が遅れているファイルで、
            // 先に訪問した 0 へ振った id が後ろの実在 device と衝突すると、そちらが「重複」として
            // 振り直され、その device を指す PluginParam レーンが外れる。
            let mut max_id = 0u64;
            let mut see = |id: u64| max_id = max_id.max(id);
            for track in &self.tracks {
                for_each_node_id(&track.devices, &mut see);
            }
            for_each_node_id(&self.master_fx_chain, &mut see);
            let mut next = self.ids.next_device_id.max(max_id.saturating_add(1));
            let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
            let mut alloc = |id: &mut u64| {
                // 0 (未採番) と **既出 id** は必ず新採番する。 r.md #71
                // (プラグインのコピー / 移動): 重複を放置すると plugin host の
                // dedup (同 device_id + 同 plugin_id) が 2 device を 1 instance へ
                // silent に merge する (音は出るので気付けない)。
                if *id == 0 || !seen.insert(*id) {
                    let new_id = next.max(1);
                    next = new_id + 1;
                    *id = new_id;
                    seen.insert(*id);
                } else if *id >= next {
                    next = *id + 1;
                }
            };
            for track in &mut self.tracks {
                for_each_node_id_mut(&mut track.devices, &mut alloc);
            }
            for_each_node_id_mut(&mut self.master_fx_chain, &mut alloc);
            self.ids.next_device_id = next.max(1);
        }

        // v29: send 安定 id (`Send::id`) を per-track 採番する。
        for track in &mut self.tracks {
            track.ensure_send_ids();
        }

        // v29: content 内要素 (note / audio event / automation point) の
        // 安定 id を採番する (選択・undo 後の選択復元を positional index
        // でなく id でアドレスするため)。
        for content in self.clip_contents.values_mut() {
            content.ensure_element_ids();
        }

        // v29: 旧 positional addressing (`PluginParam.device_index` /
        // `SendGain.send_idx`) を安定 id へ写像する。 device_index は
        // 「同 track の devices chain 内 index」、 song_lanes /
        // song_mod_routings の PluginParam は master_fx_chain の index。
        {
            let master_ids: Vec<u64> = plugins(&self.master_fx_chain).map(|p| p.id).collect();
            for track in &mut self.tracks {
                let dev_ids: Vec<u64> = plugins(&track.devices).map(|p| p.id).collect();
                let send_ids: Vec<u32> = track.sends.iter().map(|s| s.id).collect();
                for lane in &mut track.automation_lanes {
                    Self::remap_target_ids(&mut lane.target, &dev_ids, &send_ids);
                }
                for routing in &mut track.mod_routings {
                    Self::remap_target_ids(&mut routing.target, &dev_ids, &send_ids);
                }
            }
            for lane in &mut self.song_lanes {
                Self::remap_target_ids(&mut lane.target, &master_ids, &[]);
            }
            for routing in &mut self.song_mod_routings {
                Self::remap_target_ids(&mut routing.target, &master_ids, &[]);
            }
            // MIDI binding は任意 track の device を指せるので per-binding で
            // track を解決してから写像する。
            let track_devs: std::collections::HashMap<u32, Vec<u64>> = self
                .tracks
                .iter()
                .map(|t| (t.id, plugins(&t.devices).map(|p| p.id).collect()))
                .collect();
            for binding in &mut self.midi_bindings {
                if let BindingTarget::PluginParam {
                    device_id,
                    legacy_device_index,
                    legacy_track,
                    ..
                } = &mut binding.target
                {
                    // v33 以前の positional 参照は **必ず** 消費する (どちらも
                    // deserialize 専用。 解決に使えなかった残りかすを持ち回らない)。
                    let legacy = legacy_device_index.take().zip(legacy_track.take());
                    if *device_id == 0
                        && let Some((idx, track)) = legacy
                        && let Some(ids) = track_devs.get(&track)
                        && let Some(&id) = ids.get(idx as usize)
                    {
                        *device_id = id;
                    }
                }
            }
        }

        self.patch_remapped_track_refs(&id_remap);
        // r.md #129 (K4): 組み込み native の補充 / 降格 / 番号の修復。id 採番の後 (補う組み込みの
        // id が既存と衝突しない) に置く。script 経路 (`migrate_legacy_song` + `ensure_ids`) でも揃う。
        self.normalize_native_devices();
    }

    /// [`Self::ensure_ids`] の Pass 2。
    ///
    /// PR4.5 sidechain regression fix: when a track's id changes here,
    /// every reference to the old id (= other tracks' `parent_group_id`
    /// and per-plugin `aux_inputs` tap sources) is remapped to the new
    /// id. Without this remap, a saved project that used `id == 0` as a
    /// sentinel for the first track would, on load, lose all its sidechain
    /// wiring (the references would dangle, `compile_schedule` silently
    /// skips dangling refs, and the user sees no sidechain signal).
    fn patch_remapped_track_refs(&mut self, id_remap: &std::collections::HashMap<u32, u32>) {
        // Pass 2: patch every reference to a remapped id. Multi-sentinel
        // cases (= more than one track started with id 0) collapse to the
        // *last* remap entry inserted for key 0 above, which is fine for
        // the typical "one sentinel for the first track" case. Anything
        // else was already malformed before save.
        if id_remap.is_empty() {
            return;
        }
        for track in &mut self.tracks {
            if let Some(pid) = track.parent_group_id
                && let Some(&new_pid) = id_remap.get(&pid)
            {
                track.parent_group_id = Some(new_pid);
            }
            for send in &mut track.sends {
                if let Some(&new_dest) = id_remap.get(&send.dest_track_id) {
                    send.dest_track_id = new_dest;
                }
            }
            // v23: 役割別 3 chain は単一 `devices` に統合済み。各 device の
            // aux_inputs tap の source_track / aux_outputs の dest_track を
            // 1 ループで remap する (パラアウト dest も sentinel→新 id に追従)。
        }
        // 各 device の aux 入力 (plugin の aux_inputs / native の SC) の source track と、plugin の
        // aux_outputs の dest_track を remap する (パラアウト dest も sentinel→新 id に追従)。
        // r.md #110: Parallel の中も辿る。master fx が他 track を sidechain source / パラアウト先に
        // 取るケースも同じ経路。
        let mut remap_input = |_: u64, _: u8, slot: &mut Option<AuxInputRoute>| {
            if let Some(route) = slot
                && let TapSource::Track(src) = &mut route.tap.source
                && let Some(&new_id) = id_remap.get(src)
            {
                *src = new_id;
            }
        };
        for t in &mut self.tracks {
            for_each_aux_slot_mut(&mut t.devices, &mut remap_input);
        }
        for_each_aux_slot_mut(&mut self.master_fx_chain, &mut remap_input);
        let mut remap_outputs = |p: &mut PluginInstance| {
            for route in p.aux_outputs.iter_mut().flatten() {
                if let Some(&new_id) = id_remap.get(&route.dest_track) {
                    route.dest_track = new_id;
                }
            }
        };
        self.for_each_plugin_mut(&mut remap_outputs);

        // docs/plan_modulation.md §8: mod_source の tap も track id remap に追従する
        // (mod_source.id は track id ではないので不変、 tap の source track のみ)。
        for ms in self.mod_sources.iter_mut() {
            // generator (LFO/Random/MSEG/Steps) は tap を持たない。 follower のみ remap。
            if let Some(Some(tap)) = ms.follower_tap_mut()
                && let TapSource::Track(src) = &mut tap.source
                && let Some(&new_id) = id_remap.get(src)
            {
                *src = new_id;
            }
        }
    }
}
