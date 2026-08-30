//! handler::view_model — 派生 view-model getter (track mix / inspector summary / chain 等)
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use common::model::Track;
use common::plugin_format::PluginFormat;

impl AppData {
    // -------- Derived snapshots (毎フレーム計算; cache が必要なら view 側で持つ) -----

    /// 「カーソル相当」 = `selected_track_ids` の末尾要素。 `None` の
    /// ときは選択ゼロ (まだ何もクリックしていない / 全 track 削除直後)。
    pub fn cursor_track_id(&self) -> Option<u32> {
        self.selection.selected_track_ids.last().copied()
    }

    /// カーソル track の `song.tracks` 内 index。 selection は id ベース
    /// なので、 track 並び替え後でも index は再評価される。
    pub fn cursor_track_index(&self) -> Option<usize> {
        let id = self.cursor_track_id()?;
        self.song_doc.song().tracks.iter().position(|t| t.id == id)
    }

    /// A track acts as a "group" iff at least one other track points
    /// at it via `parent_group_id`. The role is purely derived — there
    /// is no `Track::kind` field. SSOT (CLAUDE.md).
    pub fn is_group_track(&self, track_id: u32) -> bool {
        crate::group_compose::is_group_track(self.song_doc.song(), track_id)
    }

    /// A track acts as a "return" iff at least one other track has a
    /// `Send` whose `dest_track_id` points at it. Purely derived (no
    /// `Track::kind`), mirroring `is_group_track`. SSOT (CLAUDE.md).
    pub fn is_return_track(&self, track_id: u32) -> bool {
        self.song_doc.song()
            .tracks
            .iter()
            .flat_map(|t| t.sends.iter())
            .any(|s| s.dest_track_id == track_id)
    }

    /// 「＋ Send」 ピッカーに出す宛先候補 `(track_id, display_name)`。
    /// `src_track_id` 自身は除外し、 加えて「その宛先が send 辺で
    /// (直接 / 間接に) `src` に戻ってくる」 = ルーティング閉路を作る track
    /// も除外する。 閉路判定は send グラフ上で `dest` から `src` への
    /// 到達可能性を BFS で見る (= `dest` を起点に send を辿って `src` に
    /// 着けば、 `src -> dest` を足すと閉路になる)。 schedule compiler 側も
    /// 閉路を弾くが、 GUI で予め隠すことで誤操作を防ぐ。
    pub fn send_destination_candidates(&self, src_track_id: u32) -> Vec<(u32, String)> {
        // dest を起点に send 辺を辿って src に到達するか。 到達するなら
        // src -> dest は閉路を成すので候補から除く。
        let creates_cycle = |dest: u32| -> bool {
            if dest == src_track_id {
                return true;
            }
            let mut stack = vec![dest];
            let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
            while let Some(cur) = stack.pop() {
                if cur == src_track_id {
                    return true;
                }
                if !seen.insert(cur) {
                    continue;
                }
                if let Some(t) = self.song_doc.song().track_by_id(cur) {
                    for s in &t.sends {
                        stack.push(s.dest_track_id);
                    }
                }
            }
            false
        };
        self.song_doc.song()
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.id != src_track_id && !creates_cycle(t.id))
            .map(|(i, t)| {
                let name = if t.name.is_empty() {
                    format!("Track {}", i + 1)
                } else {
                    t.name.clone()
                };
                (t.id, name)
            })
            .collect()
    }

    /// Walk a track's `parent_group_id` chain to count how many group
    /// hops sit between it and the master bus. Saturated at 32 to keep
    /// pathological cycles (which the schedule compiler also rejects)
    /// from looping forever in the GUI's derived snapshot.
    pub fn compute_track_depth(&self, track: &common::model::Track) -> u8 {
        let mut cursor = track.parent_group_id;
        let mut depth: u8 = 0;
        let mut hops = 0;
        while let Some(pid) = cursor {
            depth = depth.saturating_add(1);
            hops += 1;
            if hops > 32 {
                break;
            }
            cursor = self.song_doc.song().track_by_id(pid).and_then(|t| t.parent_group_id);
        }
        depth
    }

    /// `(track, target)` の built-in コントロールが mixer / arrangement で
    /// **表示すべき値**を返す。 再生中に enabled かつ現在 recording 対象でない
    /// automation lane があれば playhead 位置の curve 値 (= audio engine の
    /// `fill_track_param_ramps` と同じ read-mode 解決)、 それ以外 (停止中 / lane 無し
    /// / 当該 param を書き込み中) は静的な `fallback`。 これで:
    /// - 再生中はノブ / フェーダーがオートメーションに追従して audio と一致して動く、
    /// - 停止中はコントロールをそのまま手動操作でき、
    /// - 書き込み (Touch/Latch/Write) 中の drag はマウスに追従する
    ///   (audio engine の `recording_lanes` bypass と対称)。
    ///
    /// 変調 (`Track.mod_routings`) は各ノブの per-control modulation overlay
    /// (`view::modulation::build_mod` の live_display) が別途表示するので、 ここは
    /// **lane 値のみ**返して二重適用を避ける。
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn live_param_value(
        &self,
        track: &Track,
        target: &common::model::AutomationTarget,
        fallback: f32,
    ) -> f32 {
        if !self.transport.is_playing {
            return fallback;
        }
        // `currently_recording_lanes` と同じ判定の single-key 版: 当該 param を
        // 書き込み中なら lane を読まず手動値を返す (audio thread に送る
        // `recording_lanes` と同集合 = UI と audio が drift しない)。
        let key = (track.id, target.clone());
        let recording = self.recording.recording_mode != common::model::RecordingMode::Read
            && (self.recording.active_param_gestures.contains(&key)
                || (matches!(
                    self.recording.recording_mode,
                    common::model::RecordingMode::Latch | common::model::RecordingMode::Write
                ) && self.recording.latched_param_gestures.contains(&key)));
        if recording {
            return fallback;
        }
        let Some(lane) = track
            .automation_lanes
            .iter()
            .find(|l| l.enabled && l.target == *target)
        else {
            return fallback;
        };
        let beat = f64::from(self.transport.playhead_beat.unwrap_or(0.0));
        common::automation::lane_value_at(lane, &self.song_doc.song().clip_contents, beat) as f32
    }

    pub fn track_mix(&self) -> Vec<TrackMixEntry> {
        // Phase 6 review perf (E10): 旧コードは各 track ごとに
        // `is_group_track(t.id)` (= O(N) all-tracks scan) +
        // `compute_track_depth(t)` (= O(depth) parent chain walk) を呼び、
        // 合計 O(N²) per frame だった。 大型 song で 60fps drop。
        // 単一 pass で is_group_set / depths を batch 計算して O(N) に。
        let n_tracks = self.song_doc.song().tracks.len();
        let mut is_group_set: std::collections::HashSet<u32> =
            std::collections::HashSet::with_capacity(n_tracks);
        // リターン判定も同 pass で batch 集計 (= is_group と同 idiom)。
        // ある track に向けて 1 本でも send があれば、 その宛先はリターン。
        let mut is_return_set: std::collections::HashSet<u32> =
            std::collections::HashSet::with_capacity(n_tracks);
        let mut id_to_parent: std::collections::HashMap<u32, Option<u32>> =
            std::collections::HashMap::with_capacity(n_tracks);
        for t in &self.song_doc.song().tracks {
            id_to_parent.insert(t.id, t.parent_group_id);
            if let Some(pid) = t.parent_group_id {
                is_group_set.insert(pid);
            }
            for s in &t.sends {
                is_return_set.insert(s.dest_track_id);
            }
        }
        // depth は parent chain を walk するが、 lookup を `id_to_parent`
        // HashMap で O(1) 化 (= 旧 `track_by_id` の line search O(N) を削減)。
        // 32 hops で saturate (= cycle 防御は schedule compiler 側にもある)。
        let compute_depth = |track_id: u32| -> u8 {
            let mut cursor = id_to_parent.get(&track_id).copied().flatten();
            let mut depth: u8 = 0;
            let mut hops = 0u8;
            while let Some(pid) = cursor {
                depth = depth.saturating_add(1);
                hops = hops.saturating_add(1);
                if hops > 32 {
                    break;
                }
                cursor = id_to_parent.get(&pid).copied().flatten();
            }
            depth
        };
        self.song_doc.song()
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let (l, r) = self.transport.track_peak_display.get(i).copied().unwrap_or((0.0, 0.0));
                TrackMixEntry {
                    index: i as u32,
                    track_id: t.id,
                    name: if t.name.is_empty() {
                        format!("Track {}", i + 1)
                    } else {
                        t.name.clone()
                    },
                    // 再生中はオートメーション lane の playhead 値を表示
                    // (= audio と一致してフェーダー / パンノブが動く)。 停止中・非
                    // automation・書き込み中は静的値。
                    volume: self.live_param_value(
                        t,
                        &common::model::AutomationTarget::TrackBuiltin(
                            common::model::TrackBuiltinParam::Volume,
                        ),
                        t.volume,
                    ),
                    pan: self.live_param_value(
                        t,
                        &common::model::AutomationTarget::TrackBuiltin(
                            common::model::TrackBuiltinParam::Pan,
                        ),
                        t.pan,
                    ),
                    muted: t.muted,
                    solo: t.solo,
                    peak_l_raw: l,
                    peak_r_raw: r,
                    is_group: is_group_set.contains(&t.id),
                    is_return: is_return_set.contains(&t.id),
                    depth: compute_depth(t.id),
                    color: crate::view::track_color::effective_track_color(t),
                }
            })
            .collect()
    }

    pub fn selected_track_label(&self) -> String {
        let n_selected = self.selection.selected_track_ids.len();
        if n_selected > 1 {
            return format!("{n_selected} tracks selected");
        }
        if self.cursor_track_id() == Some(common::model::MASTER_TRACK_ID) {
            return "Master".into();
        }
        match self.cursor_track_index() {
            Some(idx) => self
                .song_doc.song()
                .tracks
                .get(idx)
                .map(|t| {
                    if t.name.is_empty() {
                        format!("Track {}", idx + 1)
                    } else {
                        t.name.clone()
                    }
                })
                .unwrap_or_else(|| format!("Track {}", idx + 1)),
            None => "(no track)".into(),
        }
    }

    /// Per-plugin sidechain wiring entries shown in the inspector. One
    /// entry per chain plugin (MidiFx / Instrument / Fx); each carries
    /// the plugin's current `aux_inputs[0]` tap source (port 0; PR4
    /// only exposes the first aux input port through the inspector). The
    /// track picker UI maps `None` → "—" and `Some(track_id)` → the
    /// track's name. Self-track is filtered out by the picker because
    /// feeding a track its own output into a sidechain creates a
    /// feedback loop the schedule compiler catches with `GraphError::Cycle`.
    pub fn sidechain_entries(&self) -> Vec<SidechainEntry> {
        // 単一デバイスチェーン: master bus も通常 track も flat な device 列を
        // `device_index` でアドレスする (役割は位置から導出するので保持しない)。
        // master 選択時は track Vec ではなく Song.master_fx_chain を対象にする。
        let (track_id, devices): (u32, &[common::model::PluginInstance]) =
            if self.cursor_track_id() == Some(common::model::MASTER_TRACK_ID) {
                (common::model::MASTER_TRACK_ID, &self.song_doc.song().master_fx_chain)
            } else {
                let Some(track) = self
                    .cursor_track_index()
                    .and_then(|i| self.song_doc.song().tracks.get(i))
                else {
                    return Vec::new();
                };
                (track.id, track.devices.as_slice())
            };
        let entries: Vec<SidechainEntry> = devices
            .iter()
            .map(|p| SidechainEntry {
                track_id,
                device_id: p.id,
                plugin_name: resolve_plugin_name(&self.ipc.plugin_db, &p.plugin_id),
                current_source: p
                    .aux_inputs
                    .first()
                    .and_then(|o| o.as_ref())
                    .map(|r| r.tap.source_track),
                current_tap_point: p
                    .aux_inputs
                    .first()
                    .and_then(|o| o.as_ref())
                    .map(|r| r.tap.tap_point)
                    .unwrap_or_default(),
            })
            .collect();
        // PR4.5 diagnostic: if any chain plugin has a non-empty
        // aux_inputs, log the resolved current_source values once
        // per inspector_chain rebuild. Helps catch UI ↔ model state
        // mismatches (= dropdown shows "—" but model has Some(id)).
        let any_wired = devices.iter().any(|p| !p.aux_inputs.is_empty());
        if any_wired {
            // Dump raw model state alongside entries so we can see the
            // exact values UI is displaying. trace! to avoid frame-rate
            // spam at default log levels; enable with RUST_LOG=trace.
            let raw: Vec<(u32, String, Vec<Option<u32>>)> = devices
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    (
                        i as u32,
                        p.plugin_id.clone(),
                        p.aux_inputs
                            .iter()
                            .map(|o| o.as_ref().map(|r| r.tap.source_track))
                            .collect(),
                    )
                })
                .collect();
            tracing::trace!(
                cursor_track_id = track_id,
                ?raw,
                ?entries,
                "sidechain_entries: rebuilt for cursor track"
            );
        }
        entries
    }

    /// パラアウト (docs/plan_paraout.md): one entry per chain device on the
    /// cursor track that declares `is_main=false` audio outputs
    /// (`aux_output_count > 0`). Drives the inspector's "Parallel Out" section
    /// (explode button + per-port destination dropdowns). Master fx are
    /// skipped — the grouped explode model needs the source to be a real track
    /// (master has no `parent_group_id` children). Mirrors `sidechain_entries`.
    pub fn parallel_output_entries(&self) -> Vec<ParallelOutputEntry> {
        // Only non-master tracks can be a grouped paraout source.
        if self.cursor_track_id() == Some(common::model::MASTER_TRACK_ID) {
            return Vec::new();
        }
        let Some(track) = self
            .cursor_track_index()
            .and_then(|i| self.song_doc.song().tracks.get(i))
        else {
            return Vec::new();
        };
        let track_id = track.id;
        track
            .devices
            .iter()
            .filter(|p| p.aux_output_count > 0)
            .map(|p| {
                let count = p.aux_output_count as usize;
                // Normalize routes to length `count` (model Vec may be shorter
                // when only some ports are wired).
                let routes: Vec<Option<u32>> = (0..count)
                    .map(|port| {
                        p.aux_outputs
                            .get(port)
                            .and_then(|o| o.as_ref())
                            .map(|r| r.dest_track)
                    })
                    .collect();
                let exploded = routes.iter().any(Option::is_some);
                ParallelOutputEntry {
                    track_id,
                    device_id: p.id,
                    plugin_name: resolve_plugin_name(&self.ipc.plugin_db, &p.plugin_id),
                    aux_output_count: p.aux_output_count,
                    routes,
                    exploded,
                }
            })
            .collect()
    }

    /// Sidechain source picker choices: "—" (None) followed by every
    /// track in the song **except** the cursor track itself.
    /// docs/plan_modulation.md §9: one inspector row per `ModSource`. `scalar`
    /// is the live follower value read from `mod_scalars` at the source's slot
    /// (= position in `Song::mod_sources`).
    pub fn mod_source_display(&self) -> Vec<ModSourceRow> {
        // docs/plan_modulation_routing_redesign.md §6: 帰属トラック (= カーソル
        // トラック) のソースだけ列挙する。`enumerate()` の index はグローバル位置の
        // ままなので `mod_scalars` lookup は正しい (follower plane はグローバル順)。
        let owner = self.cursor_track_id();
        self.song_doc.song()
            .mod_sources
            .iter()
            .enumerate()
            .filter(|(_, m)| Some(m.owner_track_id) == owner)
            .map(|(i, m)| ModSourceRow {
                id: m.id,
                color: m.color,
                scalar: self.transport.mod_scalars.get(i).copied().unwrap_or(0.0),
                kind: m.kind.clone(),
            })
            .collect()
    }

    /// docs/plan_modulation.md §9: track choices for a `ModSource`'s source
    /// dropdown — `(track_id, name)` for every track (a source may tap any
    /// track, including itself: the follower is control-rate, not a feedback
    /// loop).
    pub fn mod_source_track_choices(&self) -> Vec<(u32, String)> {
        self.song_doc.song()
            .tracks
            .iter()
            .map(|t| (t.id, t.name.clone()))
            .collect()
    }

    /// r.md #78: `source_id` を参照する **全ての** routing を、 対象がどのトラックに
    /// あっても集める (ラックの接続行 = ソース側から見た宛先一覧、 Bitwig の
    /// "modulation sources" タブと同じ切り口)。
    ///
    /// 旧 `cursor_mod_routings` は「カーソルトラックの routing」しか返さず、
    /// ラック側で「カーソルトラック所有のソース」と積を取っていたため、
    /// **ソース所有トラック ≠ 対象トラック** の routing はどちらのインスペクタにも
    /// 出ず削除できなかった。 ◉ は他トラックのツマミにも効くので、 その孤児は
    /// 実際に作れてしまう。 ソース側 1 か所に全部並べればこの穴が閉じる。
    ///
    /// 対象が `owner_track_id` 以外にある行は `"<トラック名> ▸ "` を前置きする。
    pub fn mod_source_routings(&self, source_id: u32) -> Vec<ModRoutingRow> {
        let song = self.song_doc.song();
        let owner = song
            .mod_sources
            .iter()
            .find(|m| m.id == source_id)
            .map_or(0, |m| m.owner_track_id);
        // 自分の所有トラックを先頭に、 次に他トラック、 最後に song-level (master)。
        let scan = song
            .tracks
            .iter()
            .filter(|t| t.id == owner)
            .chain(song.tracks.iter().filter(|t| t.id != owner))
            .map(|t| (t.id, t.mod_routings.as_slice()))
            .chain(std::iter::once((
                common::model::MASTER_TRACK_ID,
                song.song_mod_routings.as_slice(),
            )));
        let mut out: Vec<ModRoutingRow> = Vec::new();
        for (track_id, routings) in scan {
            for r in routings.iter().filter(|r| r.source_id == source_id) {
                let label = self.automation_target_label(&r.target);
                let label = if track_id == owner {
                    label
                } else {
                    format!("{} \u{25b8} {label}", self.track_display_name(track_id))
                };
                out.push(ModRoutingRow {
                    track_id,
                    target: r.target.clone(),
                    label,
                    depth: r.depth,
                    bipolar: matches!(r.polarity, common::model::Polarity::Bipolar),
                });
            }
        }
        out
    }

    /// `track_id` の表示名 (`MASTER_TRACK_ID` → "Master")。 削除済みは id を出す
    /// (無言で空にすると「名前の無い行」になって原因が追えない)。
    pub fn track_display_name(&self, track_id: u32) -> String {
        if track_id == common::model::MASTER_TRACK_ID {
            return "Master".to_string();
        }
        self.song_doc
            .song()
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .map_or_else(|| format!("Track {track_id}"), |t| t.name.clone())
    }

    /// docs/plan_modulation_routing_redesign.md §6: a stable display color for a
    /// `ModSource` (Bitwig 流の per-source 色)。source の `mod_sources` 内位置から
    /// 固定パレットを引く (id でなく位置 = 追加順に色が回る)。
    pub fn mod_source_color(&self, source_id: u32) -> [f32; 3] {
        // 色は `ModSource.color` が SSoT (作成時に palette から割当)。
        self.song_doc.song()
            .mod_sources
            .iter()
            .find(|m| m.id == source_id)
            .map(|m| m.color)
            .unwrap_or(common::model::MOD_SOURCE_PALETTE[0])
    }

    /// docs/plan_modulation_routing_redesign.md §6: per-control modulation data
    /// for `target` on track `track_id` whose control displays `display_base` in
    /// `domain` units, used to build the gui_01 `Modulation` widget arg. Resolves
    /// that track's routings (`MASTER_TRACK_ID` → song-level), the live modulated
    /// value, and — when a source is **armed** — the depth-edit context. The
    /// caller passes the *owning* track (inspector = cursor track, mixer strip =
    /// that strip's track) so it works for any track, not just the cursor's.
    ///
    /// entries / live / armed depth are returned in the control's *display* domain,
    /// computed as the reachable display value
    /// `to_display(norm_to_plain((base_norm + depth).clamp(0,1))) − display_base`
    /// (exact for affine / rotation deg↔rad / log scale targets). `base_norm =
    /// plain_to_norm(target, to_model(display_base))`; the on-edit inverse is
    /// `plain_to_norm(to_model(display_base + d)) − base_norm` (see `build_mod`).
    /// docs/plan_modulation_followups.md §2: a `PluginParam` target's plain
    /// `(min, max)` from the `plugin_params` cache (= `PluginParamInfo` shipped
    /// by the plugin host), for range-aware display normalization. `None` for a
    /// non-plugin target, an unknown param, or a degenerate range.
    pub fn plugin_param_range(
        &self,
        target: &common::model::AutomationTarget,
    ) -> Option<(f64, f64)> {
        let common::model::AutomationTarget::PluginParam { device_id, param_id, .. } = target
        else {
            return None;
        };
        let params = self.ipc.plugin_params.get(device_id)?;
        let info = params.iter().find(|p| p.id == *param_id)?;
        (info.max_value > info.min_value).then_some((info.min_value, info.max_value))
    }

    /// device の表示名の **SSoT** (r.md #78)。 音プラグインは plugin DB
    /// (`resolve_name`)、 内蔵映像 FX は静的マニフェスト
    /// (`common::video_fx::def_by_id`) が出所で、 device 表示名の出所が 2 系統ある
    /// ことをここ 1 箇所に閉じ込める。
    fn device_label(&self, inst: &common::model::PluginInstance) -> String {
        common::video_fx::def_by_id(&inst.plugin_id)
            .map_or_else(|| self.resolve_name(&inst.plugin_id), |def| def.name.to_string())
    }

    /// `PluginParam` target の **完全修飾** param 名 (r.md #72 / #78)。
    /// 形は `"<device 名>: <param 名>"`、 CLAP が `module` (= "/" 区切りの
    /// グループパス) を報告していれば `"<device 名>: <module>/<param 名>"`。
    ///
    /// device 名を必ず付けるのが要点で、 これが無いと MPhaser の "Dry/Wet" と
    /// MSaturator の "Dry/Wet" が同一表示になる (r.md #72)。 modulation ラックの
    /// 接続行・ arrangement lane header・ status message が**同じこの 1 本**を
    /// 使うので、 名前の付け方はここだけを直せばよい。
    ///
    /// 内蔵映像 FX は host が `PluginParamList` を送らない (param 表は静的
    /// マニフェスト) ので、 そちらから引く。 非 plugin target / device が消えて
    /// いる / host 未送 / 空名 は `None` (caller が generic 名へ fallback)。
    pub fn plugin_param_name(&self, target: &common::model::AutomationTarget) -> Option<String> {
        let common::model::AutomationTarget::PluginParam { device_id, param_id, .. } = target
        else {
            return None;
        };
        let (track_id, device_index) = find_device_by_id(self.song_doc.song(), *device_id)?;
        let inst = device_at(self.song_doc.song(), track_id, device_index)?;
        let device = self.device_label(inst);
        if let Some(def) = common::video_fx::def_by_id(&inst.plugin_id) {
            let param = def.param(*param_id)?;
            return Some(format!("{device}: {}", param.name));
        }
        let info = self
            .ipc.plugin_params
            .get(device_id)?
            .iter()
            .find(|p| p.id == *param_id)?;
        if info.name.is_empty() {
            return None;
        }
        if info.module.is_empty() {
            Some(format!("{device}: {}", info.name))
        } else {
            Some(format!("{device}: {}/{}", info.module, info.name))
        }
    }

    /// `automation_target_display_name` の song-aware 版 (B6 / r.md #8)。
    /// `PluginParam` は完全修飾名 (`plugin_param_name`) を、 解決できなければ
    /// generic「Param N」を返す。 status_message / clip 名 / mod routing 表示用。
    pub fn automation_target_label(&self, target: &common::model::AutomationTarget) -> String {
        self.plugin_param_name(target)
            .unwrap_or_else(|| automation_target_display_name(target))
    }

    pub fn inspector_mod_data(
        &self,
        target: &common::model::AutomationTarget,
        display_base: f64,
        domain: ModControlDomain,
        track_id: u32,
    ) -> InspectorModData {
        let routings: &[common::model::ModRouting] =
            if track_id == common::model::MASTER_TRACK_ID {
                &self.song_doc.song().song_mod_routings
            } else {
                match self.song_doc.song().tracks.iter().find(|t| t.id == track_id) {
                    Some(t) => &t.mod_routings,
                    None => return InspectorModData::default(),
                }
            };
        let model_base = domain.to_model(target, display_base);
        // docs/plan_modulation_followups.md §2: plugin params normalize against
        // their real min/max (identity placeholder would saturate the overlay).
        let plugin_range = self.plugin_param_range(target);
        let base_norm =
            f64::from(common::automation::plain_to_norm_ranged(target, model_base, plugin_range));
        // Reachable display depth for a normalized `depth`: convert the value the
        // base would reach at full scalar back into the control's display domain.
        // Exact for affine / rotation / log targets (vs. a linear `depth*span`).
        let reach_depth = |depth: f32| -> f64 {
            let reach_norm = (base_norm + f64::from(depth)).clamp(0.0, 1.0);
            #[allow(clippy::cast_possible_truncation)]
            let reach_model =
                common::automation::norm_to_plain_ranged(target, reach_norm as f32, plugin_range);
            domain.to_display(target, reach_model) - display_base
        };
        let mut entries: Vec<([f32; 3], f64)> = Vec::new();
        let mut armed: Option<([f32; 3], f64, u32)> = None;
        // NOTE: 各 entry は `base + depth` (= scalar 1.0) 側の到達量を 1 本表示する。
        // bipolar routing は live tick (apply_modulation) が `base − depth` 側にも
        // 振れるが、帯は +depth 側のみ (shipped image/group と同挙動。両振れ表示は
        // widget が単一 depth しか持たないため将来 gui_01 拡張時に対応)。
        for r in routings.iter().filter(|r| &r.target == target) {
            let color = self.mod_source_color(r.source_id);
            let depth_display = reach_depth(r.depth);
            entries.push((color, depth_display));
            if Some(r.source_id) == self.ui_ephemeral.armed_mod_source {
                armed = Some((color, depth_display, r.source_id));
            }
        }
        // Armed source with no routing yet on this target → editable from depth 0
        // (first drag creates the routing).
        if armed.is_none()
            && let Some(sid) = self.ui_ephemeral.armed_mod_source
        {
            armed = Some((self.mod_source_color(sid), 0.0, sid));
        }
        // Live tick only when this target actually has modulation (otherwise the
        // modulated value equals the base and the tick is redundant noise).
        let live_display = (!entries.is_empty()).then(|| {
            let live_model = common::automation::apply_modulation_with_scalars(
                self.song_doc.song(),
                target,
                model_base,
                routings,
                &self.transport.mod_scalars,
            );
            domain.to_display(target, live_model)
        });
        InspectorModData { entries, live_display, armed, track_id, base_norm }
    }

    // r.md #78: `cursor_modulatable_targets` は撤去した。
    //
    // 「変調先の全候補を 1 本の Vec に平坦化して dropdown に流す」設計そのものが
    // 誤りだった。 `ui.dropdown` の popup は高さ = 件数 × 24px で切り詰めないので
    // (`ui/crates/ui/src/popup.rs`)、 画面高を超えた候補は描かれても hit-test に
    // 当たらず**原理的に選べない**。 実測で 1 プラグイン 47,137 param という例が
    // ある以上、 候補を絞る小細工では解けない。
    //
    // 置き換えは「候補を並べる」のをやめること。 変調先の指定は ◉ (arm) の
    // ワンショット 1 本に統一し、 daw_gui が描くツマミは per-control ドラッグ、
    // プラグイン自身の窓の中のツマミは `PluginParamTouched` が拾う
    // (`handler/ipc.rs`)。 どちらも `connect_armed_mod_source_to` に集まる。

    pub fn sidechain_source_choices(&self) -> Vec<SidechainSourceChoice> {
        let cursor_id = self.cursor_track_id();
        let mut choices: Vec<SidechainSourceChoice> = Vec::with_capacity(self.song_doc.song().tracks.len() + 1);
        choices.push(SidechainSourceChoice {
            label: "—".into(),
            track_id: None,
        });
        for t in &self.song_doc.song().tracks {
            if Some(t.id) == cursor_id {
                continue;
            }
            choices.push(SidechainSourceChoice {
                label: format!("{} (id {})", t.name, t.id),
                track_id: Some(t.id),
            });
        }
        choices
    }

    /// Audio event field の inspector 表示用ライト read snapshot。
    /// 選択 clip (`selected_clip`) が `ClipContent::Audio` で、 中に少なくとも
    /// 1 event ある場合に `Some` を返す。 それ以外 (no selection / MIDI clip
    /// / Vocal clip / 空 events) は `None`。 Phase 1 では 1 clip 1 event 前提
    /// なので first event の field を「clip 全体の field」 として表示する。
    /// 編集 AppEvent (`SetClipReversed` / `SetClipMuted` / `SetClipStretchMode`)
    /// は全 event に同じ値を broadcast するので、 multi-event clip でも
    /// view は first event を「代表値」 として見せれば編集後に整合が取れる。
    pub fn inspector_audio_event_summary(&self) -> Option<InspectorAudioEventSummary> {
        let cref = self.selected_clip_ref()?;
        let track = self.song_doc.song().track_by_id(cref.track_id)?;
        let clip = track.clip_by_id(cref.clip_id)?;
        let common::model::ClipContent::Audio(audio) =
            self.song_doc.song().clip_contents.get(&clip.content_id)?
        else {
            return None;
        };
        // PR-D 段階 2: audio_editor が同じ clip を開いていて event を
        // 選択中なら、 そちらの event を Inspector の target にする。
        // multi-event clip でも個別 event を編集可能。 audio_editor が
        // 閉じている / 別 clip を開いている / 選択中 event idx が範囲外
        // なら first event (= Phase 2 PR1-3 と同じ既存挙動)。
        let event_idx = if self.ui_ephemeral.audio_editor_clip == Some(cref) {
            self.audio_editor_anchor_event().unwrap_or(0)
        } else {
            0
        };
        let event = audio.events.get(event_idx).or(audio.events.first())?;
        Some(InspectorAudioEventSummary {
            target: cref,
            reversed: event.reversed,
            // "Mute" トグル状態は clip-level `Clip.muted` を表示する (SSoT)。
            muted: clip.muted,
            stretch_mode: event.stretch_mode,
            fade_in_curve: event.fade_in_curve,
            fade_out_curve: event.fade_out_curve,
            gain_db: event.gain_db,
            pan: event.pan,
            pitch_semitones: event.pitch_semitones,
            fade_in_beats: event.fade_in_beats,
            fade_out_beats: event.fade_out_beats,
            fade_max_beats: event.event_length_beats,
        })
    }

    /// PR-D 段階 2: Audio Editor の event 選択を `delta` (= +1 / -1) 分
    /// 進める / 戻す helper。 wrap-around (= 末尾 +1 で 0 に戻る、 0
    /// -1 で末尾)。 events が空 / audio_editor_clip が None のときは
    /// `None`、 1 event のときは Some(0) (= 動かない)。 root.rs から
    /// shortcut handler 経由で呼ばれて `SelectAudioEditorEvent` の
    /// 引数を組み立てる用。
    pub fn next_audio_editor_event_idx(&self, delta: i32) -> Option<usize> {
        let target = self.ui_ephemeral.audio_editor_clip?;
        let track = self.song_doc.song().track_by_id(target.track_id)?;
        let clip = track.clip_by_id(target.clip_id)?;
        let common::model::ClipContent::Audio(audio) =
            self.song_doc.song().clip_contents.get(&clip.content_id)?
        else {
            return None;
        };
        let n = audio.events.len();
        if n == 0 {
            return None;
        }
        let cur = self.audio_editor_anchor_event().unwrap_or(0).min(n - 1);
        let n_i = n as i32;
        let next = (cur as i32).wrapping_add(delta).rem_euclid(n_i);
        Some(next as usize)
    }

    /// Audio Editor の選択 anchor (= Inspector / footer / nav の代表 event
    /// index)。 選択集合の last (= 最後に選択した event)。 空なら None。
    pub fn audio_editor_anchor_event(&self) -> Option<usize> {
        self.selected_audio_event_indices().last().copied()
    }

    /// `selected_clip` が `ClipContent::Image` の clip を指していて、
    /// 中に少なくとも 1 event があれば first event を代表値として
    /// `InspectorImageEventSummary` を返す。
    /// 編集 AppEvent (`SetClipImageX` 等) は全 event に同じ値を broadcast
    /// するので、 multi-event clip でも view は first event を「代表値」
    /// として見せれば編集後に整合が取れる。 数値値 (x/y/w/h/opacity/
    /// fade_in_beats/fade_out_beats) は inspector の edit buffer (text
    /// 文字列) 側に持つので summary には含めない (= dropdown / toggle
    /// のみ snapshot に乗せる)。
    pub fn inspector_image_event_summary(&self) -> Option<InspectorImageEventSummary> {
        let cref = self.selected_clip_ref()?;
        let track = self.song_doc.song().track_by_id(cref.track_id)?;
        let clip = track.clip_by_id(cref.clip_id)?;
        let common::model::ClipContent::Image(image) =
            self.song_doc.song().clip_contents.get(&clip.content_id)?
        else {
            return None;
        };
        let event = image.events.first()?;
        let has_lane = |field: common::model::ImageBuiltinParam| {
            track.automation_lanes.iter().any(|l| {
                matches!(l.target, common::model::AutomationTarget::ImageBuiltin(p) if p == field)
            })
        };
        Some(InspectorImageEventSummary {
            target: cref,
            // "Mute" トグル状態は clip-level `Clip.muted` を表示する (SSoT)。
            muted: clip.muted,
            fade_in_curve: event.fade_in_curve,
            fade_out_curve: event.fade_out_curve,
            x_automated: has_lane(common::model::ImageBuiltinParam::X),
            y_automated: has_lane(common::model::ImageBuiltinParam::Y),
            w_automated: has_lane(common::model::ImageBuiltinParam::W),
            h_automated: has_lane(common::model::ImageBuiltinParam::H),
            opacity_automated: has_lane(common::model::ImageBuiltinParam::Opacity),
            rotation_automated: has_lane(common::model::ImageBuiltinParam::Rotation),
            x: event.x,
            y: event.y,
            w: event.w,
            h: event.h,
            opacity: event.opacity,
            rotation_radians: event.rotation_radians,
            fade_in_beats: event.fade_in_beats,
            fade_out_beats: event.fade_out_beats,
            fade_max_beats: event.event_length_beats,
        })
    }

    /// PR-D 段階 2: set_clip_audio_event_* 系 helper の broadcast 範囲を
    /// 決める。 audio_editor が `target` clip を開いていて event を
    /// 選択中なら、 当該 event 1 つだけ更新 (= multi-event clip の個別
    /// 編集)。 そうでなければ全 event に broadcast (= Phase 2 PR1-3 の
    /// 既存挙動、 1 clip 1 event 前提なので broadcast = first event 編集)。
    /// 引数 `n_events` は当該 ClipContent::Audio の events 長 (= 呼び出し
    /// 前に immutable get で取得)。
    pub(crate) fn audio_event_target_indices(&self, target: ClipKey, n_events: usize) -> Vec<usize> {
        if self.ui_ephemeral.audio_editor_clip == Some(target)
            && !self.selected_audio_event_indices().is_empty()
        {
            let mut v: Vec<usize> = self
                .selected_audio_event_indices()
                .iter()
                .copied()
                .filter(|&i| i < n_events)
                .collect();
            v.sort_unstable();
            v.dedup();
            // 選択はあるが全て範囲外 (stale) なら全 event に broadcast
            // (= 旧 `idx < n_events` else 全件 の挙動を踏襲)。
            if v.is_empty() { (0..n_events).collect() } else { v }
        } else {
            (0..n_events).collect()
        }
    }

    /// PR-D 段階 2 の集約 helper: `target` clip の `ClipContent::Audio`
    /// 内、 `audio_event_target_indices` で決まる範囲の event 群に
    /// closure `f` を適用 + sync。 audio_editor で個別 event 選択中なら
    /// その 1 つだけ、 そうでなければ全 event を更新する。 戻り値は
    /// 「実際に何らかの event を更新したか」 (= caller が edit buffer
    /// resync を呼ぶかの判断に使う)。
    pub(crate) fn mutate_audio_events_in_clip<F>(&mut self, target: ClipKey, mut f: F) -> bool
    where
        F: FnMut(&mut common::model::AudioEvent),
    {
        let Some(content_id) = self
            .song_doc.song()
            .track_by_id(target.track_id)
            .and_then(|t| t.clip_by_id(target.clip_id))
            .map(|c| c.content_id)
        else {
            return false;
        };
        let n_events = match self.song_doc.song().clip_contents.get(&content_id) {
            Some(common::model::ClipContent::Audio(a)) => a.events.len(),
            _ => return false,
        };
        let indices = self.audio_event_target_indices(target, n_events);
        if indices.is_empty() {
            return false;
        }
        self.edit_song(|song| {
            if let Some(common::model::ClipContent::Audio(audio)) =
                song.clip_contents.get_mut(&content_id)
            {
                for &i in &indices {
                    if let Some(event) = audio.events.get_mut(i) {
                        f(event);
                    }
                }
                true
            } else {
                false
            }
        }) == Some(true)
    }

    /// B12-manual (r.md #8): `audio_editor_clip` の `event_idx` 番目 AudioEvent の
    /// `beat_markers` に `f` を適用する (= warp marker 手動編集)。 `mutate_audio_events_in_clip`
    /// と違い選択ではなく特定 event を対象にする (marker drag/add/delete は対象 event が確定して
    /// いるため)。 適用したら plugin host へ song を sync。 戻り値 = 実際に適用したか。
    pub(crate) fn mutate_warp_markers<F>(&mut self, event_idx: usize, f: F) -> bool
    where
        F: FnOnce(&mut Vec<common::model::BeatMarker>),
    {
        let Some(target) = self.ui_ephemeral.audio_editor_clip else {
            return false;
        };
        let Some(content_id) = self
            .song_doc.song()
            .track_by_id(target.track_id)
            .and_then(|t| t.clip_by_id(target.clip_id))
            .map(|c| c.content_id)
        else {
            return false;
        };
        self.edit_song(|song| {
            if let Some(common::model::ClipContent::Audio(audio)) =
                song.clip_contents.get_mut(&content_id)
                && let Some(event) = audio.events.get_mut(event_idx)
            {
                f(&mut event.beat_markers);
                true
            } else {
                false
            }
        }) == Some(true)
    }

    /// `target` clip が `ClipContent::Image` の場合、 全 ImageEvent に
    /// `f` を適用する (= image clip は audio_editor のような per-event
    /// 選択 UI を持たないので broadcast 固定)。 戻り値は「実際に何らか
    /// の event を更新したか」 (= caller が edit buffer resync を呼ぶか
    /// の判断に使う)。
    pub(crate) fn mutate_image_events_in_clip<F>(&mut self, target: ClipKey, mut f: F) -> bool
    where
        F: FnMut(&mut common::model::ImageEvent),
    {
        let Some(content_id) = self
            .song_doc.song()
            .track_by_id(target.track_id)
            .and_then(|t| t.clip_by_id(target.clip_id))
            .map(|c| c.content_id)
        else {
            return false;
        };
        self.edit_song(|song| {
            if let Some(common::model::ClipContent::Image(image)) =
                song.clip_contents.get_mut(&content_id)
            {
                if image.events.is_empty() {
                    return false;
                }
                for event in &mut image.events {
                    f(event);
                }
                true
            } else {
                false
            }
        }) == Some(true)
    }

    /// 単一デバイスチェーン (`docs/plan_linear_chain.md` §5): `Track.devices`
    /// (master bus は `master_fx_chain`) を flat な行として返す。役割の判定は
    /// せず、plugin 名のみを並べる (挙動は engine の port 直結で決まる)。
    pub fn inspector_chain(&self) -> Vec<ChainEntry> {
        let Some(track_id) = self.cursor_track_id() else {
            return Vec::new();
        };
        let devices: &[common::model::PluginInstance] =
            if track_id == common::model::MASTER_TRACK_ID {
                &self.song_doc.song().master_fx_chain
            } else {
                let Some(idx) = self.cursor_track_index() else {
                    return Vec::new();
                };
                let Some(track) = self.song_doc.song().tracks.get(idx) else {
                    return Vec::new();
                };
                track.devices.as_slice()
            };
        devices
            .iter()
            .map(|p| {
                // 埋め込み GUI の有無。 builtin (VOICEVOX / Silence) は
                // 規定で持たないので format から即断 (= PluginParamList 到着前でも
                // 正しく「Par」routing)。 外部 CLAP・VST3 は host の通知
                // (`slot_has_gui`)、 未受信 (load 直後) は楽観的に true で「GUI」のまま。
                let has_embedded_gui = p.format != PluginFormat::Builtin
                    && self.ipc.slot_has_gui.get(&p.id).copied().unwrap_or(true);
                let has_params = self
                    .ipc.plugin_params
                    .get(&p.id)
                    .is_some_and(|v| !v.is_empty());
                let is_voicevox = p.format == PluginFormat::Builtin
                    && p.plugin_id == common::plugin_db::BUILTIN_ID_VOICEVOX;
                ChainEntry {
                    device_id: p.id,
                    plugin_name: resolve_plugin_name(&self.ipc.plugin_db, &p.plugin_id),
                    has_embedded_gui,
                    is_video: p.ports.is_video(),
                    is_voicevox,
                    has_params,
                    send_all_keys: p.send_all_keys_to_plugin,
                    load_error: self.ipc.failed_plugin_loads.get(&p.id).cloned(),
                }
            })
            .collect()
    }

    /// v18 (`docs/plan_track_clip_color.md`): color_picker を開く。target と
    /// anchor (popup 基準位置 = 開いた場所の rect) をセットし、session_dirty を
    /// false に戻す (= 次の色変更が session 先頭の 1 snapshot を取る)。
    /// 右クリック「色...」/ inspector スウォッチから呼ぶ。
    pub fn open_color_picker(
        &mut self,
        target: ColorPickerTarget,
        anchor: daw_ui_renderer::Rect,
    ) {
        self.ui_ephemeral.color_picker_target = Some(target);
        self.ui_ephemeral.color_picker_anchor = Some(anchor);
        // picker session 全体を 1 undo step に bracket する (dismiss で end)。
        self.song_doc.begin_gesture();
    }

    // -------- Undo/Redo ----------------------------------------------------

    /// r.md #56: song beat → 秒。 テンポカーブがある曲では [`TempoMap`] を
    /// `song_epoch` 世代キャッシュに載せ、 引きだけを毎フレーム行う。
    ///
    /// [`common::tempo_map::song_beat_to_seconds`] をそのまま毎フレーム呼ぶと、
    /// lane を 1 本引いただけで `TempoMap::from_song` が O(曲長) で走る (5 分の曲で
    /// ~9,600 breakpoint ≒ 77KB の `Vec` 確保、 30 分なら ~460KB)。 transport バーは
    /// 常時描画なので曲長に比例して悪化する。 `TempoMap` は「生成は曲の変更時に 1 回、
    /// 引きは O(log n)」 という設計 (tempo_map.rs 冒頭 doc) なので、 世代キャッシュが
    /// 本来の使い方。 lane が無い曲は table を張らず定数 BPM の高速経路に落ちる。
    pub(crate) fn song_beat_to_seconds(&self, beat: f64) -> f64 {
        let song = self.song_doc.song();
        let epoch = self.song_doc.edit_epoch();
        let mut cache = self.ui_ephemeral.tempo_map_cache.borrow_mut();
        if !cache.built || cache.epoch != epoch {
            cache.map = common::tempo_map::has_tempo_automation(song)
                .then(|| common::tempo_map::TempoMap::from_song(song));
            cache.epoch = epoch;
            cache.built = true;
        }
        match cache.map.as_ref() {
            Some(m) => m.beat_to_seconds(beat),
            // lane 無し = `song_beat_to_seconds` の定数 BPM 高速経路 (table を張らない)。
            None => common::tempo_map::song_beat_to_seconds(song, beat),
        }
    }

    /// D3/D4: arrangement build 用ラベルキャッシュ ([`ArrLabelCache`])。 `song_epoch`
    /// が進んでいれば全 track 名 + content ラベルを 1 度だけ作り直し、 通常フレームは
    /// 同一 `Arc<str>` の clone (refcount bump) を返す。 `clip_display_label` は
    /// `clip.content_id` のみに依存するので content_id 単位で 1 回だけ算出する
    /// (linked clip は同一ラベルを共有)。
    pub(crate) fn arrangement_labels(&self) -> std::cell::Ref<'_, ArrLabelCache> {
        {
            let mut cache = self.ui_ephemeral.arr_label_cache.borrow_mut();
            if cache.epoch != self.song_doc.edit_epoch() {
                cache.track_names.clear();
                cache.content_labels.clear();
                cache.section_names.clear();
                cache.content_names.clear();
                for t in &self.song_doc.song().tracks {
                    cache
                        .track_names
                        .insert(t.id, std::sync::Arc::from(t.name.as_str()));
                    for c in &t.clips {
                        cache.content_labels.entry(c.content_id).or_insert_with(|| {
                            crate::widgets::arrangement::view_build::clip_display_label(
                                c,
                                self.song_doc.song(),
                            )
                        });
                    }
                }
                // D4 同件: section ruler / automation clip ラベルも世代キャッシュ。
                for s in &self.song_doc.song().sections {
                    cache
                        .section_names
                        .insert(s.id, std::sync::Arc::from(s.name.as_str()));
                }
                for (cid, name) in &self.song_doc.song().clip_content_names {
                    cache
                        .content_names
                        .insert(*cid, std::sync::Arc::from(name.as_str()));
                }
                cache.epoch = self.song_doc.edit_epoch();
            }
        }
        self.ui_ephemeral.arr_label_cache.borrow()
    }

}
