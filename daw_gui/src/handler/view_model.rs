//! handler::view_model — 派生 view-model getter (track mix / inspector summary / chain 等)
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;

/// live 値を読む 1 フレーム分の文脈 (ランチャーの走行状態の表を 1 回だけ組んで配る)。
pub struct LiveParamScope {
    running: Vec<crate::launcher_time::RunningRow>,
}

impl AppData {
    // -------- Derived snapshots (毎フレーム計算; cache が必要なら view 側で持つ) -----

    /// 「カーソル相当」 = `selected_track_ids` の末尾要素。 `None` の
    /// ときは選択ゼロ (まだ何もクリックしていない / 全 track 削除直後)。
    pub fn cursor_track_id(&self) -> Option<u32> {
        self.cur.selection.selected_track_ids.last().copied()
    }

    /// カーソル track の `song.tracks` 内 index。 selection は id ベース
    /// なので、 track 並び替え後でも index は再評価される。
    pub fn cursor_track_index(&self) -> Option<usize> {
        let id = self.cursor_track_id()?;
        self.cur.song_doc.song().tracks.iter().position(|t| t.id == id)
    }

    /// A track acts as a "group" iff at least one other track points
    /// at it via `parent_group_id`. The role is purely derived — there
    /// is no `Track::kind` field. SSOT (CLAUDE.md).
    pub fn is_group_track(&self, track_id: u32) -> bool {
        crate::group_compose::is_group_track(self.cur.song_doc.song(), track_id)
    }

    /// 「＋ Send」 ピッカーに出す宛先候補 `(track_id, display_name)`。
    /// `src_track_id` 自身と、 send を足すと依存が循環する track を除く (r.md #129 §10.13)。
    /// 循環の判定は `Song::can_add_send` と同じ依存 graph (children / サイドチェイン / send、
    /// Structural) — send 辺だけを見ると「子から親 group への send」 が候補に残り、 選ぶと
    /// `add_send` に拒否される。 graph は候補ごとに組み直さず 1 回だけ組む。
    pub fn send_destination_candidates(&self, src_track_id: u32) -> Vec<(u32, String)> {
        use common::routing_deps::{EdgeScope, TrackDeps};
        let song = self.cur.song_doc.song();
        let deps = TrackDeps::build(song, EdgeScope::Structural);
        song
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.id != src_track_id && !deps.would_cycle(t.id, src_track_id))
            .map(|(i, t)| (t.id, t.display_name(i).into_owned()))
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
            cursor = self.cur.song_doc.song().track_by_id(pid).and_then(|t| t.parent_group_id);
        }
        depth
    }

    /// live 値の 1 フレーム分の文脈 ([`LiveParamScope`]) を組む。
    pub fn live_param_scope(&self) -> LiveParamScope {
        LiveParamScope { running: self.launcher_running_rows() }
    }

    /// `(owner, target)` のコントロールが mixer / arrangement / Rack で **表示すべき値**
    /// (r.md #129 §7.5 の唯一の口)。再生中に enabled かつ現在 recording 対象でない
    /// automation lane があれば playhead 位置の curve 値 (= audio engine の read-mode 解決)、
    /// それ以外 (停止中 / lane 無し / 当該 param を書き込み中) は静的な `fallback`。 これで:
    /// - 再生中はノブ / フェーダーがオートメーションに追従して audio と一致して動く、
    /// - 停止中はコントロールをそのまま手動操作でき、
    /// - 書き込み (Touch/Latch/Write) 中の drag はマウスに追従する
    ///   (audio engine の `recording_lanes` bypass と対称)。
    ///
    /// `owner_id` は store の持ち主 (track id か `MASTER_TRACK_ID` → `song_lanes`)。
    ///
    /// 変調は各ノブの per-control modulation overlay (`view::modulation::build_mod` の
    /// live_display) が別途表示するので、 ここは **lane 値のみ**返して二重適用を避ける。
    pub(crate) fn live_param_value(
        &self,
        owner_id: u32,
        target: &common::model::AutomationTarget,
        fallback: f32,
    ) -> f32 {
        self.live_param_value_on(&self.live_param_scope(), owner_id, target, fallback)
    }

    /// 文脈を **呼び側が 1 回だけ組む**版 (トラックを並べる描画で毎回
    /// `launcher_running_rows()` を組むと行数 × トラック数の O(N²) になる)。
    pub(crate) fn live_param_value_on(
        &self,
        scope: &LiveParamScope,
        owner_id: u32,
        target: &common::model::AutomationTarget,
        fallback: f32,
    ) -> f32 {
        match crate::view::native_device::ParamOwner::resolve(self.cur.song_doc.song(), owner_id) {
            Some(owner) => self.live_lane_value(scope, owner, target, fallback),
            None => fallback,
        }
    }

    /// 本体。store を解決済みで受け取る。
    ///
    /// r.md #87: レーンの値は **行の主導権込み**で解く
    /// ([`crate::launcher_time::RowTimeline`]) — ランチャー主導のレーン行では
    /// engine が `lane.session_clips` のセルを、停止させた行ではレーン既定値 (Q11)
    /// を出すので、ここで `lane.clips` を song の playhead で読むと
    /// 「聴こえている音量 ≠ フェーダーが指す値」になる。
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn live_lane_value(
        &self,
        scope: &LiveParamScope,
        owner: crate::view::native_device::ParamOwner<'_>,
        target: &common::model::AutomationTarget,
        fallback: f32,
    ) -> f32 {
        if !self.cur.transport.is_playing || self.param_is_recording(owner.id, target) {
            return fallback;
        }
        let Some(lane) = owner.lanes.iter().find(|l| l.enabled && l.target == *target) else {
            return fallback;
        };
        self.launcher_timeline(&scope.running).lane_value(owner.id, lane, self.cur.song_doc.song()) as f32
    }

    /// 内蔵 device の 1 param の表示値 (plain)。静的な値は `dev` (On は `bypassed` の反転)。
    pub fn live_native_param(
        &self,
        scope: &LiveParamScope,
        owner: crate::view::native_device::ParamOwner<'_>,
        dev: &common::model::NativeDevice,
        p: common::model::NativeParamId,
    ) -> f32 {
        let target = common::model::AutomationTarget::NativeParam { device_id: dev.id, param: p };
        self.live_lane_value(scope, owner, &target, dev.param(p).unwrap_or(0.0))
    }

    /// 行のミニ表示 / Par のカーブ用に、レーンの値を重ねた device (engine の
    /// `resolve_native_device` の GUI 版。変調は `build_mod` が別途表示する)。
    #[allow(clippy::cast_possible_truncation)]
    pub fn live_native_device(
        &self,
        scope: &LiveParamScope,
        owner: crate::view::native_device::ParamOwner<'_>,
        dev: &common::model::NativeDevice,
    ) -> common::model::NativeDevice {
        let mut out = *dev;
        if !self.cur.transport.is_playing {
            return out;
        }
        let timeline = self.launcher_timeline(&scope.running);
        for lane in owner.lanes.iter().filter(|l| l.enabled) {
            let common::model::AutomationTarget::NativeParam { device_id, param } = lane.target else {
                continue;
            };
            if device_id != dev.id || self.param_is_recording(owner.id, &lane.target) {
                continue;
            }
            out.set_param(param, timeline.lane_value(owner.id, lane, self.cur.song_doc.song()) as f32);
        }
        out
    }

    /// `currently_recording_lanes` と同じ判定の single-key 版: 当該 param を書き込み中なら
    /// lane を読まず手動値を返す (audio thread に送る `recording_lanes` と同集合 = UI と audio が
    /// drift しない)。
    fn param_is_recording(&self, owner_id: u32, target: &common::model::AutomationTarget) -> bool {
        let rec = &self.cur.recording;
        let key = (owner_id, target.clone());
        rec.recording_mode != common::model::RecordingMode::Read
            && (rec.active_param_gestures.contains_key(&key)
                || (matches!(
                    rec.recording_mode,
                    common::model::RecordingMode::Latch | common::model::RecordingMode::Write
                ) && rec.latched_param_gestures.contains(&key)))
    }

    /// いまの playhead と engine の走行状態から組んだ行解決器。
    /// `running` は借用なので呼び側が持つ ([`Self::launcher_running_rows`] の戻り値)。
    pub(crate) fn launcher_timeline<'a>(
        &self,
        running: &'a [crate::launcher_time::RunningRow],
    ) -> crate::launcher_time::RowTimeline<'a> {
        crate::launcher_time::RowTimeline::with_running(
            0.0,
            f64::from(self.cur.transport.playhead_beat.unwrap_or(0.0)),
            running,
        )
    }

    pub fn track_mix(&self) -> Vec<TrackMixEntry> {
        // Phase 6 review perf (E10): 旧コードは各 track ごとに
        // `is_group_track(t.id)` (= O(N) all-tracks scan) +
        // `compute_track_depth(t)` (= O(depth) parent chain walk) を呼び、
        // 合計 O(N²) per frame だった。 大型 song で 60fps drop。
        // 単一 pass で is_group_set / depths を batch 計算して O(N) に。
        let n_tracks = self.cur.song_doc.song().tracks.len();
        let mut is_group_set: std::collections::HashSet<u32> =
            std::collections::HashSet::with_capacity(n_tracks);
        // リターン判定も同 pass で batch 集計 (= is_group と同 idiom)。
        // ある track に向けて 1 本でも send があれば、 その宛先はリターン。
        let mut is_return_set: std::collections::HashSet<u32> =
            std::collections::HashSet::with_capacity(n_tracks);
        let mut id_to_parent: std::collections::HashMap<u32, Option<u32>> =
            std::collections::HashMap::with_capacity(n_tracks);
        for t in &self.cur.song_doc.song().tracks {
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
        // r.md #87: 行の主導権込みでレーン値を解く表は **1 フレームに 1 回**組む
        // (トラックごとに組むと行数 × トラック数の O(N²) になる)。
        let scope = self.live_param_scope();
        let live = |t: &common::model::Track, p: common::model::TrackBuiltinParam, fallback: f32| {
            let target = common::model::AutomationTarget::TrackBuiltin(p);
            self.live_lane_value(&scope, crate::view::native_device::ParamOwner::of_track(t), &target, fallback)
        };
        self.cur.song_doc.song()
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let (l, r) = self.cur.transport.track_peak_display.get(i).copied().unwrap_or((0.0, 0.0));
                TrackMixEntry {
                    index: i as u32,
                    track_id: t.id,
                    name: t.display_name(i).into_owned(),
                    // 再生中はオートメーション lane の playhead 値を表示
                    // (= audio と一致してフェーダー / パンノブが動く)。 停止中・非
                    // automation・書き込み中は静的値。
                    volume: live(t, common::model::TrackBuiltinParam::Volume, t.volume),
                    pan: live(t, common::model::TrackBuiltinParam::Pan, t.pan),
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
        let n_selected = self.cur.selection.selected_track_ids.len();
        if n_selected > 1 {
            return format!("{n_selected} tracks selected");
        }
        let song = self.cur.song_doc.song();
        match self.cursor_track_id() {
            Some(id) if id == common::model::MASTER_TRACK_ID || song.track_by_id(id).is_some() => {
                song.track_display_name(id).into_owned()
            }
            _ => "(no track)".into(),
        }
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
            .and_then(|i| self.cur.song_doc.song().tracks.get(i))
        else {
            return Vec::new();
        };
        let track_id = track.id;
        track
            .plugins()
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
    /// は engine が publish した値面から **`ModSource::id` で** 引いた実測値
    /// (r.md #89)。
    pub fn mod_source_display(&self) -> Vec<ModSourceRow> {
        // docs/plan_modulation_routing_redesign.md §6: 帰属トラック (= カーソル
        // トラック) のソースだけ列挙する。値は id 引きなので、絞り込みで位置が
        // 詰まっても正しいソースの値が出る (旧実装は `enumerate()` の位置で
        // 引いていて、engine 側の slot 数と食い違うと別のソースの値を表示した)。
        let owner = self.cursor_track_id();
        self.cur.song_doc.song()
            .mod_sources
            .iter()
            .filter(|m| Some(m.owner_track_id) == owner)
            .map(|m| ModSourceRow {
                id: m.id,
                color: m.color,
                enabled: m.enabled,
                scalar: self.cur.transport.mod_plane.scalar(m.id),
                kind: m.kind.clone(),
            })
            .collect()
    }

    /// docs/plan_modulation.md §9: track choices for a `ModSource`'s source
    /// dropdown — `(track_id, name)` for every track (a source may tap any
    /// track, including itself: the follower is control-rate, not a feedback
    /// loop).
    /// r.md #110: 他 track に加えて **同 track の Parallel 内 chain** も選べる (Bitwig と同じ)。
    /// 自 track 自身も可 (follower は control-rate なので feedback にならない)。
    /// 先頭は SC の候補と同じ「—」(入力なし。聴いていたトラック / chain が消えた follower もここを指す)。
    pub fn mod_source_track_choices(&self) -> Vec<(Option<common::model::TapSource>, String)> {
        let song = self.cur.song_doc.song();
        let mut out: Vec<(Option<common::model::TapSource>, String)> = std::iter::once((None, "—".to_string()))
            .chain(
                song.tracks
                    .iter()
                    .enumerate()
                    .map(|(i, t)| (Some(common::model::TapSource::Track(t.id)), t.display_name(i).into_owned())),
            )
            .collect();
        if let Some(devices) = self.cursor_track_id().and_then(|id| song.fx_chain_by_track_id(id)) {
            common::model::for_each_chain(devices, &mut |parallel, c| {
                out.push((
                    Some(common::model::TapSource::Chain(c.id)),
                    format!("{} / {}", parallel.name, c.name),
                ));
            });
        }
        out
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
        let song = self.cur.song_doc.song();
        let source = song.mod_sources.iter().find(|m| m.id == source_id);
        let owner = source.map_or(0, |m| m.owner_track_id);
        let source_enabled = source.is_some_and(|m| m.enabled);
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
                    format!("{} \u{25b8} {label}", song.track_display_name(track_id))
                };
                out.push(ModRoutingRow {
                    id: r.id,
                    track_id,
                    target: r.target.clone(),
                    label,
                    depth: r.depth,
                    bipolar: matches!(r.polarity, common::model::Polarity::Bipolar),
                    enabled: r.enabled,
                    effective: r.enabled && source_enabled,
                });
            }
        }
        out
    }

    /// docs/plan_modulation_routing_redesign.md §6: a stable display color for a
    /// `ModSource` (Bitwig 流の per-source 色)。source の `mod_sources` 内位置から
    /// 固定パレットを引く (id でなく位置 = 追加順に色が回る)。
    pub fn mod_source_color(&self, source_id: u32) -> [f32; 3] {
        // 色は `ModSource.color` が SSoT (作成時に palette から割当)。
        self.cur.song_doc.song()
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
        let info = self.cur.pipc.plugin_params.info(*device_id, *param_id)?;
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

    /// **チェーン上のノード (device / chain / Parallel) で束縛する** target の、song を引いた
    /// **完全修飾** 名 (r.md #72 / #78 / #129 §7.4)。形は `"<ノード名>: <param 名>"`:
    ///
    /// | target | 名前 |
    /// |---|---|
    /// | `PluginParam` | `"<device 名>: <param 名>"` (CLAP が `module` を報告していれば `"<device 名>: <module>/<param 名>"`) |
    /// | `NativeParam` | `native_param_label(display_name, p)` = `"Comp 2: Thr"` |
    /// | `ChainGain` / `ChainPan` | `"<chain 名>: Gain"` / `"<chain 名>: Pan"` |
    /// | `ParallelOutGain` / `ParallelSplitFreq` / `ParallelSelect` | `"<Parallel 名>: Out"` / `"<Parallel 名>: Split Low\|Mid"` / `"<Parallel 名>: Active"` |
    ///
    /// ノード名を必ず付けるのが要点で、 これが無いと MPhaser の "Dry/Wet" と
    /// MSaturator の "Dry/Wet" が同一表示になる (r.md #72)。 modulation ラックの
    /// 接続行・ arrangement lane header・ status message が**同じこの 1 本**を
    /// 使うので、 名前の付け方はここだけを直せばよい。
    ///
    /// 内蔵映像 FX は host が `PluginParamList` を送らない (param 表は静的
    /// マニフェスト) ので、 そちらから引く。 ノードで束縛しない target / ノードが消えて
    /// いる / host 未送 / 空名 は `None` (caller が song 非依存の名前へ fallback)。
    ///
    /// ノードは `SongDoc` の id 構造の世代つき索引で引き、param は id の索引で引く (曲全体の木も param 表も
    /// 走査しない)。modulation ラックの接続行のように毎フレーム行ごとに呼ばれるため。
    pub fn device_param_name(&self, target: &common::model::AutomationTarget) -> Option<String> {
        use common::model::{AutomationTarget as T, TrackBuiltinParam as B};
        let doc = &self.cur.song_doc;
        let (device_id, param_id) = match target {
            T::PluginParam { device_id, param_id, .. } => (*device_id, *param_id),
            T::NativeParam { device_id, param } => {
                return Some(common::model::native_param_label(&doc.native_by_id(*device_id)?.display_name(), *param));
            }
            T::TrackBuiltin(B::ChainGain { chain_id }) => return Some(format!("{}: Gain", doc.chain_by_id(*chain_id)?.1.name)),
            T::TrackBuiltin(B::ChainPan { chain_id }) => return Some(format!("{}: Pan", doc.chain_by_id(*chain_id)?.1.name)),
            T::TrackBuiltin(B::ParallelOutGain { parallel_id }) => {
                return Some(format!("{}: Out", doc.parallel_by_id(*parallel_id)?.name));
            }
            T::TrackBuiltin(B::ParallelSplitFreq { parallel_id, edge }) => {
                let edge = crate::automation_label::split_edge_label(*edge);
                return Some(format!("{}: Split {edge}", doc.parallel_by_id(*parallel_id)?.name));
            }
            T::TrackBuiltin(B::ParallelSelect { parallel_id }) => {
                return Some(format!("{}: Active", doc.parallel_by_id(*parallel_id)?.name));
            }
            T::TrackBuiltin(B::Volume | B::Pan | B::Mute | B::SendGain { .. })
            | T::MasterLimiter(_)
            | T::SongTempo
            | T::SongTimeSigNumerator
            | T::SongTranspose
            | T::ImageBuiltin(_)
            | T::TextBuiltin(_)
            | T::GroupTransform(_)
            | T::ModSourceParam { .. }
            | T::ModRoutingDepth { .. } => return None,
        };
        let inst = doc.plugin_by_id(device_id)?;
        let device = self.device_label(inst);
        if let Some(def) = common::video_fx::def_by_id(&inst.plugin_id) {
            let param = def.param(param_id)?;
            return Some(format!("{device}: {}", param.name));
        }
        let info = self.cur.pipc.plugin_params.info(device_id, param_id)?;
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
    /// ノードで束縛する target は完全修飾名 (`device_param_name`: "Comp 2: Thr" /
    /// "Chain 1: Pan") を、 解決できなければ song 非依存の名前を返す。
    /// status_message / last touched / clip 名 / mod routing 表示用。
    pub fn automation_target_label(&self, target: &common::model::AutomationTarget) -> String {
        // r.md #89: モジュレーターは song を引かないと種別も通し番号も出せない
        // (`automation_target_display_name` は song 非依存の pure label なので
        // "変調 3 ▸ 速さ" までしか出せない)。ここが人間向け表示の SSoT。
        if let Some(name) = self.mod_target_label(target) {
            return name;
        }
        self.device_param_name(target)
            .unwrap_or_else(|| automation_target_display_name(target))
    }

    /// r.md #89: `ModSourceParam` / `ModRoutingDepth` の song 依存ラベル。
    /// 種別 + 作成順の通し番号で `"LFO 2 ▸ 速さ"` / `"LFO 2 → Volume の深さ"` を作る。
    fn mod_target_label(&self, target: &common::model::AutomationTarget) -> Option<String> {
        use common::model::AutomationTarget as T;
        let song = self.cur.song_doc.song();
        match target {
            T::ModSourceParam { source_id, param } => {
                Some(format!("{} \u{25b8} {}", self.mod_source_name(*source_id)?, param.label()))
            }
            T::ModRoutingDepth { routing_id } => {
                let r = song.all_mod_routings().find(|r| r.id == *routing_id)?;
                let src = self.mod_source_name(r.source_id)?;
                // 深さの表示は「どのソースが何を変調しているか」が読めないと意味が無い。
                let dest = self.device_param_name(&r.target).unwrap_or_else(|| {
                    self.mod_target_label(&r.target)
                        .unwrap_or_else(|| automation_target_display_name(&r.target))
                });
                Some(format!("{src} \u{2192} {dest} の深さ"))
            }
            _ => None,
        }
    }

    /// 種別ラベル + 同種内の作成順 (1 始まり)。ラック / レーン / ステータスで共有する。
    pub fn mod_source_name(&self, source_id: u32) -> Option<String> {
        let song = self.cur.song_doc.song();
        let src = song.mod_sources.iter().find(|m| m.id == source_id)?;
        let kind_label = src.kind.short_label();
        let ordinal = song
            .mod_sources
            .iter()
            .filter(|m| m.kind.short_label() == kind_label)
            .position(|m| m.id == source_id)
            .map_or(1, |i| i + 1);
        Some(format!("{kind_label} {ordinal}"))
    }

    /// `target` のコントロール 1 個ぶんの変調の表示データ。`owner` は target の lane / routing の持ち主
    /// (r.md #129 §7.7) で、**面を描き始めるときに 1 回だけ解決した** ものを渡す (つまみごとに木を
    /// 引き直さない、`ParamOwner` の doc)。描画中の Song は不変なので同じスナップショットから解決した
    /// 持ち主で足り、Edit 側 (`AddModRouting` / `SetModRoutingDepth`) は実行時の Song で引き直す。
    pub fn inspector_mod_data(
        &self,
        target: &common::model::AutomationTarget,
        display_base: f64,
        domain: ModControlDomain,
        owner: crate::view::native_device::ParamOwner<'_>,
    ) -> InspectorModData {
        let (track_id, routings) = (owner.id, owner.routings);
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
            if Some(r.source_id) == self.cur.peph.armed_mod_source {
                armed = Some((color, depth_display, r.source_id));
            }
        }
        // Armed source with no routing yet on this target → editable from depth 0
        // (first drag creates the routing).
        if armed.is_none()
            && let Some(sid) = self.cur.peph.armed_mod_source
        {
            armed = Some((self.mod_source_color(sid), 0.0, sid));
        }
        // Live tick only when this target actually has modulation (otherwise the
        // modulated value equals the base and the tick is redundant noise).
        let live_display = (!entries.is_empty()).then(|| {
            let live_model = common::automation::apply_modulation_with_plane(
                target,
                model_base,
                routings,
                self.cur.transport.mod_plane.as_ref(),
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
        let track = self.cur.song_doc.song().track_by_id(cref.track_id)?;
        let clip = track.clip_by_id(cref.clip_id)?;
        let common::model::ClipContent::Audio(audio) =
            self.cur.song_doc.song().clip_contents.get(&clip.content_id)?
        else {
            return None;
        };
        // PR-D 段階 2: audio_editor が同じ clip を開いていて event を
        // 選択中なら、 そちらの event を Inspector の target にする。
        // multi-event clip でも個別 event を編集可能。 audio_editor が
        // 閉じている / 別 clip を開いている / 選択中 event idx が範囲外
        // なら first event (= Phase 2 PR1-3 と同じ既存挙動)。
        let event_idx = if self.cur.peph.audio_editor_clip == Some(cref) {
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
        let target = self.cur.peph.audio_editor_clip?;
        let track = self.cur.song_doc.song().track_by_id(target.track_id)?;
        let clip = track.clip_by_id(target.clip_id)?;
        let common::model::ClipContent::Audio(audio) =
            self.cur.song_doc.song().clip_contents.get(&clip.content_id)?
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
        let track = self.cur.song_doc.song().track_by_id(cref.track_id)?;
        let clip = track.clip_by_id(cref.clip_id)?;
        let common::model::ClipContent::Image(image) =
            self.cur.song_doc.song().clip_contents.get(&clip.content_id)?
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
            flip_h: event.flip_h,
            flip_v: event.flip_v,
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
        if self.cur.peph.audio_editor_clip == Some(target)
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
            .cur.song_doc.song()
            .track_by_id(target.track_id)
            .and_then(|t| t.clip_by_id(target.clip_id))
            .map(|c| c.content_id)
        else {
            return false;
        };
        let n_events = match self.cur.song_doc.song().clip_contents.get(&content_id) {
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

    /// **時間写像を変える編集** (移調 / 逆再生 / 伸縮 mode) を [`Self::mutate_audio_events_in_clip`] と
    /// 同じ対象へ掛ける。 値が変わる event は先に take を見えている窓へ詰め直す
    /// ([`common::model::AudioEvent::rebase_take`]) — 分割の片の写像の起点は片の外 (分割前の頭) に
    /// あるので、詰め直さずに変えると片の頭の音が跳ぶ (逆再生なら前の片の音を逆に読む)。 詰め直すと
    /// 分割していない event に掛けたのと同じく、片自身の頭を起点に効く。
    pub(crate) fn mutate_audio_event_mapping_in_clip(
        &mut self,
        target: ClipKey,
        changes: impl Fn(&common::model::AudioEvent) -> bool,
        mut f: impl FnMut(&mut common::model::AudioEvent),
    ) -> bool {
        let song = self.cur.song_doc.song();
        let secs_per_beat = 60.0 / f64::from(song.bpm.max(1.0));
        let native_fpb: std::collections::HashMap<common::model::AudioSourceId, f64> = song
            .media
            .audio_sources
            .iter()
            .map(|(&id, s)| (id, f64::from(s.sample_rate) * secs_per_beat))
            .collect();
        self.mutate_audio_events_in_clip(target, |e| {
            if !changes(e) {
                return;
            }
            if let Some(&fpb) = native_fpb.get(&e.source_id) {
                e.rebase_take(fpb);
            }
            f(e);
        })
    }

    /// B12-manual (r.md #8): `audio_editor_clip` の `event_idx` 番目 AudioEvent の
    /// `beat_markers` に `f` を適用する (= warp marker 手動編集)。 `mutate_audio_events_in_clip`
    /// と違い選択ではなく特定 event を対象にする (marker drag/add/delete は対象 event が確定して
    /// いるため)。 適用したら plugin host へ song を sync。 戻り値 = 実際に適用したか。
    pub(crate) fn mutate_warp_markers<F>(&mut self, event_idx: usize, f: F) -> bool
    where
        F: FnOnce(&mut Vec<common::model::BeatMarker>),
    {
        let Some(target) = self.cur.peph.audio_editor_clip else {
            return false;
        };
        let Some(content_id) = self
            .cur.song_doc.song()
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
            .cur.song_doc.song()
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
    /// r.md #110: cursor track の **全 plugin** (Parallel の中も含む、信号順) の行情報。
    /// 表示ツリーは [`Self::chain_rows`]。 こちらは「読み込み失敗」 section や
    /// 選択の正規化のような「plugin の集合」 が欲しい呼び出し側用。
    pub fn inspector_chain(&self) -> Vec<ChainEntry> {
        let Some(track_id) = self.cursor_track_id() else {
            return Vec::new();
        };
        let Some(devices) = self.cur.song_doc.song().fx_chain_by_track_id(track_id) else {
            return Vec::new();
        };
        common::model::plugins(devices)
            .map(|p| self.chain_entry_for(p))
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
        self.cur.peph.color_picker_target = Some(target);
        self.cur.peph.color_picker_anchor = Some(anchor);
        // picker session 全体を 1 undo step に bracket する (`close_color_picker` で end)。
        self.cur.song_doc.begin_gesture();
    }

    /// color_picker を閉じる唯一の口 (dismiss / 対象消失の両方)。`open_color_picker` の
    /// gesture をここで閉じる — 閉じ忘れると以後の離散編集が同じ gesture id に squash され
    /// 1 undo step に潰れる (旧実装は view が target を None にするだけで End が無かった)。
    pub fn close_color_picker(&mut self) {
        self.cur.peph.color_picker_target = None;
        self.cur.peph.color_picker_anchor = None;
        self.cur.song_doc.end_gesture();
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
        let song = self.cur.song_doc.song();
        match self.tempo_map_cached().as_ref() {
            Some(m) => m.beat_to_seconds(beat),
            // lane 無し = `song_beat_to_seconds` の定数 BPM 高速経路 (table を張らない)。
            None => common::tempo_map::song_beat_to_seconds(song, beat),
        }
    }

    /// `song_epoch` 世代キャッシュの [`common::tempo_map::TempoMap`]。テンポカーブが
    /// 無い曲は `None` (= 呼び側は `song.bpm` の定数換算を使う)。
    fn tempo_map_cached(&self) -> std::cell::Ref<'_, Option<common::tempo_map::TempoMap>> {
        let song = self.cur.song_doc.song();
        let epoch = self.cur.song_doc.edit_epoch();
        {
            let mut cache = self.cur.peph.tempo_map_cache.borrow_mut();
            if !cache.built || cache.epoch != epoch {
                cache.map = common::tempo_map::has_tempo_automation(song)
                    .then(|| common::tempo_map::TempoMap::from_song(song));
                cache.epoch = epoch;
                cache.built = true;
            }
        }
        std::cell::Ref::map(self.cur.peph.tempo_map_cache.borrow(), |c| &c.map)
    }


    /// D3/D4: arrangement build 用ラベルキャッシュ ([`ArrLabelCache`])。 `song_epoch`
    /// が進んでいれば全 track 名 + content ラベルを 1 度だけ作り直し、 通常フレームは
    /// 同一 `Arc<str>` の clone (refcount bump) を返す。 `clip_display_label` は
    /// `clip.content_id` のみに依存するので content_id 単位で 1 回だけ算出する
    /// (linked clip は同一ラベルを共有)。
    /// レーンのノード名は Song に加えて host の param 表と plugin DB からも決まるので、別の鍵
    /// ([`LaneLabelsKey`]) で作り直す。
    pub(crate) fn arrangement_labels(&self) -> std::cell::Ref<'_, ArrLabelCache> {
        {
            let mut cache = self.cur.peph.arr_label_cache.borrow_mut();
            let edit_epoch = self.cur.song_doc.edit_epoch();
            if !self.host_derived_key_is_current(cache.lane_labels_key.as_ref()) {
                self.fill_lane_node_labels(&mut cache.lane_node_labels);
                cache.lane_labels_key = Some(self.host_derived_key());
            }
            if cache.epoch != edit_epoch {
                cache.track_names.clear();
                cache.content_labels.clear();
                cache.section_names.clear();
                cache.content_names.clear();
                for (i, t) in self.cur.song_doc.song().tracks.iter().enumerate() {
                    cache.track_names.insert(t.id, std::sync::Arc::from(t.display_name(i).as_ref()));
                    for c in &t.clips {
                        cache.content_labels.entry(c.content_id).or_insert_with(|| {
                            crate::widgets::arrangement::view_build::clip_display_label(
                                c,
                                self.cur.song_doc.song(),
                            )
                        });
                    }
                }
                // D4 同件: section ruler / automation clip ラベルも世代キャッシュ。
                for s in &self.cur.song_doc.song().sections {
                    cache
                        .section_names
                        .insert(s.id, std::sync::Arc::from(s.name.as_str()));
                }
                for (cid, name) in &self.cur.song_doc.song().clip_content_names {
                    cache
                        .content_names
                        .insert(*cid, std::sync::Arc::from(name.as_str()));
                }
                cache.epoch = edit_epoch;
            }
        }
        self.cur.peph.arr_label_cache.borrow()
    }

    /// Song と host の param 表と plugin DB から作る派生の、今の入力の世代 ([`HostDerivedKey`])。
    pub(crate) fn host_derived_key(&self) -> HostDerivedKey {
        HostDerivedKey {
            edit_epoch: self.cur.song_doc.edit_epoch(),
            plugin_params: self.cur.pipc.plugin_params.generation(),
            plugin_db: self.ipc.plugin_db.clone(),
        }
    }

    /// `key` (派生を作ったときの世代、`None` = まだ作っていない) が今の入力のものか。
    pub(crate) fn host_derived_key_is_current(&self, key: Option<&HostDerivedKey>) -> bool {
        key.is_some_and(|k| {
            k.is_current(self.cur.song_doc.edit_epoch(), self.cur.pipc.plugin_params.generation(), &self.ipc.plugin_db)
        })
    }

    /// [`ArrLabelCache::lane_node_labels`] を作り直す (全トラック + master のレーンのうち、ノードで束縛する target)。
    /// 名前の組み立ては [`Self::device_param_name`] 1 本。
    fn fill_lane_node_labels(
        &self,
        labels: &mut std::collections::HashMap<common::model::AutomationTarget, std::sync::Arc<str>>,
    ) {
        labels.clear();
        let song = self.cur.song_doc.song();
        let lanes = song.tracks.iter().flat_map(|t| &t.automation_lanes).chain(&song.song_lanes);
        for lane in lanes {
            if !labels.contains_key(&lane.target)
                && let Some(name) = self.device_param_name(&lane.target)
            {
                labels.insert(lane.target.clone(), std::sync::Arc::from(name));
            }
        }
    }
}

#[cfg(test)]
mod live_value_tests {
    use common::model::{
        AutomationLane, AutomationTarget, BusCompParam, ChainRef, Device, MASTER_TRACK_ID, NativeKind, NativeParamId,
        Parallel, TrackBuiltinParam,
    };

    use crate::view::native_device::ParamOwner;

    /// F-G8 (§7.5): 再生中は store (master なら song 側) のレーン値、停止中は model 値。
    #[test]
    fn live_values_follow_lanes_in_the_owner_store_only_while_playing() {
        let mut app = crate::test_support::headless_app();
        let bus = app.cur.song_doc.song().builtin_native(MASTER_TRACK_ID, NativeKind::BusComp).expect("master Bus Comp").id;
        let thr = NativeParamId::BusComp(BusCompParam::Threshold);
        let mut parallel = Parallel::new();
        parallel.id = 9_001;
        parallel.chains[0].id = 9_002;
        app.edit_song(|song| {
            song.insert_device(ChainRef::Track(MASTER_TRACK_ID), 0, Device::Parallel(parallel));
            song.push_lane(MASTER_TRACK_ID, AutomationLane::new(AutomationTarget::NativeParam { device_id: bus, param: thr }, -7.0));
            song.push_lane(
                MASTER_TRACK_ID,
                AutomationLane::new(AutomationTarget::TrackBuiltin(TrackBuiltinParam::ChainGain { chain_id: 9_002 }), 0.3),
            );
        });
        let chain_gain = AutomationTarget::TrackBuiltin(TrackBuiltinParam::ChainGain { chain_id: 9_002 });
        let dev = *app.cur.song_doc.song().native_by_id(bus).expect("bus");
        let model = dev.param(thr).expect("thr");
        assert!((model - -7.0).abs() > 1e-3, "model 値はレーン値と違う");

        let scope = app.live_param_scope();
        let owner = || ParamOwner::master(app.cur.song_doc.song());
        assert_eq!(app.live_native_param(&scope, owner(), &dev, thr), model, "停止中は model 値");
        assert_eq!(app.live_param_value(MASTER_TRACK_ID, &chain_gain, 1.0), 1.0);

        app.cur.transport.is_playing = true;
        let scope = app.live_param_scope();
        let owner = ParamOwner::master(app.cur.song_doc.song());
        assert!((app.live_native_param(&scope, owner, &dev, thr) - -7.0).abs() < 1e-6, "再生中はレーン値");
        assert!(
            (app.live_native_device(&scope, owner, &dev).param(thr).unwrap() - -7.0).abs() < 1e-6,
            "device ごと解いても同じ"
        );
        assert!((app.live_param_value(MASTER_TRACK_ID, &chain_gain, 1.0) - 0.3).abs() < 1e-6, "master の chain も追従");
    }

    /// レーン見出しの完全修飾名は世代キャッシュから引くが、ノードの名前を変える編集の後は必ず新しい名前になる
    /// (キャッシュが古い名前を返さない)。
    #[test]
    fn lane_node_labels_follow_renames_through_the_generation_cache() {
        let mut app = crate::test_support::headless_app();
        let mut parallel = Parallel::new();
        parallel.id = 9_001;
        parallel.chains[0].id = 9_002;
        let chain_gain = AutomationTarget::TrackBuiltin(TrackBuiltinParam::ChainGain { chain_id: 9_002 });
        app.edit_song(|song| {
            song.insert_device(ChainRef::Track(MASTER_TRACK_ID), 0, Device::Parallel(parallel));
            song.push_lane(MASTER_TRACK_ID, AutomationLane::new(chain_gain.clone(), 0.3));
        });
        let label = |app: &crate::state::AppData| app.arrangement_labels().lane_node_label(&chain_gain);
        let before = label(&app).expect("chain のレーンに名前が付く");
        assert_eq!(Some(before.clone()), app.device_param_name(&chain_gain).map(std::sync::Arc::from), "device_param_name と同じ名前");

        app.edit_song(|song| {
            song.chain_by_id_mut(9_002).expect("chain").name = "Bass".into();
        });
        assert_eq!(label(&app).as_deref(), Some("Bass: Gain"), "改名は次の世代で反映される (旧 {before})");
    }

    /// plugin の param のレーン名も世代キャッシュから引くが、host が param 表を送った / 捨てた、plugin DB を
    /// 差し替えた後は必ず新しい名前になる (Song の世代だけでは検知できない入力も鍵に入っている)。
    #[test]
    fn plugin_param_lane_labels_follow_param_lists_and_the_plugin_db() {
        use common::model::PluginInstance;
        use common::plugin_format::PluginFormat;
        use common::protocol::PluginParamInfo;

        let mut app = crate::test_support::headless_app();
        let mut device_id = 0;
        app.edit_song(|song| {
            let mut inst = PluginInstance::new("com.example.synth".into(), PluginFormat::Clap);
            inst.id = song.alloc_device_id();
            device_id = inst.id;
            song.master_fx_chain.push(Device::Plugin(inst));
            let target = AutomationTarget::PluginParam { device_id, param_id: 7, legacy_device_index: None };
            song.push_lane(MASTER_TRACK_ID, AutomationLane::new(target, 0.5));
        });
        let target = AutomationTarget::PluginParam { device_id, param_id: 7, legacy_device_index: None };
        let label = |app: &crate::state::AppData| app.arrangement_labels().lane_node_label(&target);
        assert_eq!(label(&app), None, "host が param 表を送る前は名前が無い (見出しは song 非依存の名前)");

        let cutoff = PluginParamInfo {
            id: 7,
            name: "Cutoff".into(),
            module: String::new(),
            min_value: 0.0,
            max_value: 1.0,
            default_value: 0.5,
            flags: 0,
        };
        app.cur.pipc.plugin_params.insert(device_id, vec![cutoff]);
        assert_eq!(label(&app).as_deref(), Some("com.example.synth: Cutoff"), "param 表が届いたら名前が付く (DB に無い plugin は id)");

        let db: common::plugin_db::PluginDatabase = serde_json::from_str(
            r#"{"entries":[{"id":"com.example.synth","name":"Synth","path":"synth.clap","descriptor_index":0}]}"#,
        )
        .expect("plugin DB");
        app.ipc.plugin_db = Some(std::sync::Arc::new(db));
        assert_eq!(label(&app).as_deref(), Some("Synth: Cutoff"), "plugin DB を差し替えたら DB の名前");
        assert_eq!(label(&app), app.device_param_name(&target).map(std::sync::Arc::from), "device_param_name と同じ名前");

        app.cur.pipc.plugin_params.remove(&device_id);
        assert_eq!(label(&app), None, "param 表が捨てられたら名前も消える");
    }
}
