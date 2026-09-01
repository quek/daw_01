//! handler::modulation — modulation source / routing の追加・編集・削除
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;

impl AppData {
    // ---- docs/plan_modulation.md §9: modulation source / routing CRUD ----
    // すべて `Song` を mutate して `flush_song_sync` で締める
    // (audio engine が follower schedule を再 compile、 preview が再合成)。

    pub(crate) fn add_mod_source(&mut self, tag: ModSourceKindTag) {
        use common::model::{ModSourceKind, RandomConfig};
        // 帰属トラック = カーソルトラック (= このラックを開いているトラック)。以後
        // inspector ではこのトラックの下にだけ列挙される。
        let owner_track_id = self.cursor_track_id().unwrap_or(0);
        let _ = self
            .edit_song(move |song| {
                let id = song.alloc_mod_source_id();
                let color = common::model::ModSource::palette_color(song.mod_sources.len());
                let kind = match tag {
                    // follower の follow 先は初期 = カーソルトラック。
                    ModSourceKindTag::Follower => ModSourceKind::EnvelopeFollower {
                        tap: common::model::AudioTap::post_fader(owner_track_id),
                        follower: common::model::FollowerConfig::default(),
                    },
                    ModSourceKindTag::Lfo => ModSourceKind::Lfo(Default::default()),
                    // seed は source ごとに決定論的かつ相異にする (id から)。
                    ModSourceKindTag::Random => ModSourceKind::Random(RandomConfig {
                        seed: u64::from(id),
                        ..Default::default()
                    }),
                    ModSourceKindTag::Mseg => ModSourceKind::Mseg(Default::default()),
                    ModSourceKindTag::Steps => ModSourceKind::Steps(Default::default()),
                };
                song.mod_sources.push(common::model::ModSource {
                    id,
                    owner_track_id,
                    color,
                    kind,
                });
            })
            .is_some();
    }

    /// envelope follower の `(tap, follower)` に `f` を適用する (generator
    /// は対象外 = no-op)。 Song 編集は `edit_song` チョークポイント経由。
    /// 戻り値 = 実際に適用できたか (= 該当 id が follower だったか)。
    pub(crate) fn edit_mod_source_follower(
        &mut self,
        id: u32,
        f: impl FnOnce(&mut common::model::AudioTap, &mut common::model::FollowerConfig),
    ) -> bool {
        self.edit_song_checked(move |song| {
            let Some(m) = song.mod_sources.iter_mut().find(|m| m.id == id) else {
                return false;
            };
            if let common::model::ModSourceKind::EnvelopeFollower { tap, follower } = &mut m.kind {
                f(tap, follower);
                true
            } else {
                false
            }
        })
    }

    /// generator (LFO/Random/MSEG/Steps) 設定の編集。`scrub` は連続
    /// ドラッグ系 (per-frame の recompile を避け dirty のみ、 drag-end で sync)。
    pub(crate) fn edit_mod_source(&mut self, id: u32, edit: ModSourceEdit) {
        use common::model::ModSourceKind;
        // 存在しない id は no-op (dirty も付けない = 旧 early return と同じ)。
        if !self.song_doc.song().mod_sources.iter().any(|m| m.id == id) {
            return;
        }
        // r.md #89: 「最後に触った parameter」の記録はここ 1 箇所に集める
        // (`A` キーでオートメーションレーンを作れるのは記録された param だけ)。
        let touched = Self::edit_touched_param(&edit);
        let _ = self
            .edit_song(move |song| {
                let Some(m) = song.mod_sources.iter_mut().find(|m| m.id == id) else {
                    return false;
                };
                let mut scrub = false;
                match edit {
            ModSourceEdit::Rate(rate) => {
                if let Some(r) = m.kind.rate_mut() {
                    *r = rate;
                }
            }
            ModSourceEdit::Retrigger(rt) => {
                if let Some(r) = m.kind.retrigger_mut() {
                    *r = rt;
                }
            }
            ModSourceEdit::LfoShape(shape) => {
                if let ModSourceKind::Lfo(c) = &mut m.kind {
                    c.shape = shape;
                }
            }
            ModSourceEdit::LfoPhase(p) => {
                if let ModSourceKind::Lfo(c) = &mut m.kind {
                    c.phase = p.clamp(0.0, 1.0);
                }
                scrub = true;
            }
            ModSourceEdit::RandomSmooth(s) => {
                if let ModSourceKind::Random(c) = &mut m.kind {
                    c.smooth = s.clamp(0.0, 1.0);
                }
                scrub = true;
            }
            ModSourceEdit::RerollSeed => {
                if let ModSourceKind::Random(c) = &mut m.kind {
                    // 決定論的に別の seed へ派生 (壁時計/RNG を使わない)。
                    c.seed = common::modulators::reseed(c.seed);
                }
            }
            ModSourceEdit::MsegPlayMode(pm) => {
                if let ModSourceKind::Mseg(c) = &mut m.kind {
                    c.play_mode = pm;
                }
            }
            ModSourceEdit::MsegAddPoint { time, value } => {
                if let ModSourceKind::Mseg(c) = &mut m.kind {
                    let p = common::model::MsegPoint {
                        time: time.clamp(0.0, 1.0),
                        value: value.clamp(0.0, 1.0),
                        curve: 0.0,
                    };
                    let idx = c
                        .points
                        .partition_point(|q| q.time <= p.time)
                        .clamp(1, c.points.len()); // 両端の間にだけ挿入
                    c.points.insert(idx, p);
                }
            }
            ModSourceEdit::MsegMovePoint { index, time, value } => {
                if let ModSourceKind::Mseg(c) = &mut m.kind
                    && index < c.points.len()
                {
                    let n = c.points.len();
                    // 両端は time 固定 (0.0 / 1.0)、 中間は隣接点間に clamp で単調維持。
                    if index > 0 && index < n - 1 {
                        let lo = c.points[index - 1].time + 1e-3;
                        let hi = c.points[index + 1].time - 1e-3;
                        c.points[index].time = time.clamp(lo, hi);
                    }
                    c.points[index].value = value.clamp(0.0, 1.0);
                }
                scrub = true;
            }
            ModSourceEdit::MsegSetCurve { segment, curve } => {
                if let ModSourceKind::Mseg(c) = &mut m.kind
                    && segment < c.points.len()
                {
                    c.points[segment].curve = curve.clamp(-1.0, 1.0);
                }
                scrub = true;
            }
            ModSourceEdit::MsegRemovePoint(index) => {
                if let ModSourceKind::Mseg(c) = &mut m.kind
                    && index > 0
                    && index + 1 < c.points.len()
                {
                    // 両端 (0 と末尾) は削除しない。
                    c.points.remove(index);
                }
            }
            ModSourceEdit::StepsCount(count) => {
                if let ModSourceKind::Steps(c) = &mut m.kind {
                    let count = count.clamp(1, 64);
                    c.values.resize(count, 0.5);
                }
            }
            ModSourceEdit::StepValue { index, value } => {
                if let ModSourceKind::Steps(c) = &mut m.kind
                    && index < c.values.len()
                {
                    c.values[index] = value.clamp(0.0, 1.0);
                }
                scrub = true;
            }
            ModSourceEdit::StepsDirection(dir) => {
                if let ModSourceKind::Steps(c) = &mut m.kind {
                    c.direction = dir;
                }
            }
            ModSourceEdit::StepsSlew(slew) => {
                if let ModSourceKind::Steps(c) = &mut m.kind {
                    c.slew = slew.clamp(0.0, 1.0);
                }
                scrub = true;
            }
        }
                scrub
            })
            .unwrap_or(false);
        // generator の値は engine が schedule の `mod_kinds` から評価するので、 設定
        // 変更は recompile で engine に反映する。 連続ドラッグ系は per-frame LoadSong
        // を避け dirty のみ (= edit_song が epoch bump、 drag-end edge で sync、
        // follower の attack/release と同流儀)。
        if let Some(param) = touched {
            self.note_touched_mod_target(common::model::AutomationTarget::ModSourceParam {
                source_id: id,
                param,
            });
        }
    }

    /// r.md #89: モジュレーターのツマミの **今の値** (plain)。ラックのツマミ・
    /// オートメーションレーンの既定値・変調の base が全部ここを通る
    /// (値の SSoT は `common::mod_graph::param_plain`)。
    pub(crate) fn mod_param_plain_value(
        &self,
        source_id: u32,
        param: common::model::ModParam,
    ) -> f64 {
        let song = self.song_doc.song();
        let Some(m) = song.mod_sources.iter().find(|m| m.id == source_id) else {
            return 0.0;
        };
        common::mod_graph::param_plain(&m.kind, param, f64::from(song.bpm))
    }

    /// r.md #89: `ModSourceEdit` が動かす [`common::model::ModParam`]。
    /// 「触った parameter」の記録を **`edit_mod_source` の 1 箇所**に集めるための写像
    /// (ラックの各ツマミに記録を書かせると、足し忘れたツマミだけ `A` が効かなくなる)。
    /// 形 / 種別 / 点の追加削除など「値ではない編集」は `None`。
    fn edit_touched_param(edit: &ModSourceEdit) -> Option<common::model::ModParam> {
        use common::model::ModParam;
        match edit {
            ModSourceEdit::Rate(_) => Some(ModParam::Rate),
            ModSourceEdit::LfoPhase(_) => Some(ModParam::LfoPhase),
            // Pulse の duty は shape に載っているので、Pulse を選び直したときだけ拾う。
            ModSourceEdit::LfoShape(common::model::LfoShape::Pulse { .. }) => {
                Some(ModParam::LfoPulseWidth)
            }
            ModSourceEdit::RandomSmooth(_) => Some(ModParam::RandomSmooth),
            ModSourceEdit::StepsSlew(_) => Some(ModParam::StepsSlew),
            _ => None,
        }
    }

    /// r.md #89: モジュレーターのツマミ / 変調の深さを触ったことを記録する。
    /// `A` キーの「最後に触った parameter のオートメーションレーンを追加」が
    /// これを見るので、**ラックのツマミを動かしたら必ず呼ぶこと**
    /// (呼ばないと、そのツマミだけ `A` でレーンを作れない片手落ちになる)。
    ///
    /// `track_id` はレーン / routing の置き場 (= ソースの帰属トラック、master なら
    /// `MASTER_TRACK_ID`)。`add_automation_from_last_touched` の song-level 判定が
    /// これをそのまま使う。
    pub(crate) fn note_touched_mod_target(
        &mut self,
        target: common::model::AutomationTarget,
    ) {
        use common::model::AutomationTarget as T;
        let song = self.song_doc.song();
        let track_id = match &target {
            T::ModSourceParam { source_id, .. } => song.mod_source_owner(*source_id),
            T::ModRoutingDepth { routing_id } => song.mod_routing_owner(*routing_id),
            _ => None,
        };
        let Some(track_id) = track_id else { return };
        let display_name = self.automation_target_label(&target);
        self.ui_ephemeral.last_touched_param = Some(TouchedParam {
            track_id,
            target,
            display_name,
            touched_at: std::time::Instant::now(),
        });
    }

    /// r.md #78: **待受中 (◉) のソースを `target` に繋ぐ唯一の口**。
    ///
    /// arm は「触ったツマミ 1 個に繋ぐ」ワンショットなので、 繋いだ時点で自動
    /// 解除する (待受けたまま忘れて、 音作りでツマミをいじっただけで繋がる事故を
    /// 防ぐ)。 待受中でなければ何もしない。
    ///
    /// 呼び出し元は 2 つで、 **到達範囲が違う**:
    /// - `handler/ipc.rs` の `PluginParamTouched` … プラグイン自身の窓の中の
    ///   ツマミ (daw_gui が overlay を描けない唯一の領域)。
    /// - `view/modulation.rs` の depth ドラッグ終端 … daw_gui が描いているツマミ
    ///   (ドラッグ量がそのまま depth になるので、 ここでは解除だけ担う)。
    pub(crate) fn connect_armed_mod_source_to(
        &mut self,
        track_id: u32,
        target: common::model::AutomationTarget,
    ) {
        let Some(source_id) = self.ui_ephemeral.armed_mod_source else {
            return;
        };
        let label = self.automation_target_label(&target);
        let added = self.add_mod_routing(track_id, target, source_id);
        self.ui_ephemeral.armed_mod_source = None;
        // 既に繋がっていた param を再ドラッグしただけのときに「割り当てました」と
        // 出すと、 何が起きたかを取り違える。 起きた事実をそのまま出す。
        self.ui_ephemeral.status_message = if added {
            format!("変調を割り当てました → {label}")
        } else {
            format!("変調の深さを更新しました → {label}")
        };
    }

    /// r.md #78: 待受中 (◉) のソースの `(色, 表示名)`。 ステータスバーが
    /// 「今どのソースが待受中か」を常時出すために使う。 ラックはカーソルトラック
    /// 所有のソースしか列挙しないので、 トラックを移ると ◉ ボタン自体が画面から
    /// 消える。 待受の可視化を ◉ ボタンだけに任せられない理由がこれ。
    pub fn armed_mod_source_label(&self) -> Option<([f32; 3], String)> {
        let sid = self.ui_ephemeral.armed_mod_source?;
        let src = self.song_doc.song().mod_sources.iter().find(|m| m.id == sid)?;
        let track = self.track_display_name(src.owner_track_id);
        Some((src.color, format!("{track} / {}", src.kind.short_label())))
    }

    pub(crate) fn remove_mod_source(&mut self, id: u32) {
        // 待受中のソースを消したら待受も解除する (削除済み id を掴んだままだと
        // 次に触ったツマミが幽霊 routing になる)。
        if self.ui_ephemeral.armed_mod_source == Some(id) {
            self.ui_ephemeral.armed_mod_source = None;
        }
        self.edit_song(move |song| {
            song.mod_sources.retain(|m| m.id != id);
            // この source を指す全 routing を掃除 (dangling は scalar 0 になるが、
            // 残すと UI に幽霊 routing が出るので明示削除)。lane 非依存なので
            // Track.mod_routings / Song.song_mod_routings を走査する。
            for t in &mut song.tracks {
                t.mod_routings.retain(|r| r.source_id != id);
            }
            song.song_mod_routings.retain(|r| r.source_id != id);
            // r.md #89: このソースの **ツマミ** を指していた変調 / レーンと、
            // 消えた変調の **深さ** を指していた変調まで連鎖して掃除する
            // (source_id だけ見ると幽霊 routing が残る)。
            song.prune_dangling_mod_targets();
        });
    }

    /// Resolve `track_id` to its mutable `mod_routings` Vec
    /// (`MASTER_TRACK_ID` → `Song.song_mod_routings`,
    /// `docs/plan_modulation_routing_redesign.md` §2).
    pub(crate) fn edit_mod_routings<R>(
        &mut self,
        track_id: u32,
        f: impl FnOnce(&mut Vec<common::model::ModRouting>) -> R,
    ) -> Option<R> {
        self.edit_song(move |song| {
            let routings = if track_id == common::model::MASTER_TRACK_ID {
                &mut song.song_mod_routings
            } else {
                &mut song.track_by_id_mut(track_id)?.mod_routings
            };
            Some(f(routings))
        })
        .flatten()
    }

    /// 戻り値は **実際に足したか** (既に同じ (target, source) があれば `false`)。
    /// per-control の depth ドラッグは毎フレームここを通るので、 呼び出し側が
    /// 「今つないだ」 と「もう繋がっていた」 を区別できるようにしている。
    pub(crate) fn add_mod_routing(
        &mut self,
        track_id: u32,
        target: common::model::AutomationTarget,
        source_id: u32,
    ) -> bool {
        // 実際に追加したときだけ recompile (per-control depth ドラッグは毎フレーム
        // AddModRouting を呼ぶので、no-op add で sync すると LoadSong 連発になる)。
        //
        // r.md #89: id は **足すこの 1 箇所**で採番する (`AutomationTarget::ModRoutingDepth`
        // が 1 本の変調を指すので、後から `ensure_ids` 任せにすると採番前の一瞬だけ
        // 深さを変調先にできない窓ができる)。
        self.edit_song(move |song| {
            let exists = if track_id == common::model::MASTER_TRACK_ID {
                &song.song_mod_routings
            } else {
                match song.track_by_id(track_id) {
                    Some(t) => &t.mod_routings,
                    None => return false,
                }
            }
            .iter()
            .any(|r| r.source_id == source_id && r.target == target);
            if exists {
                return false;
            }
            let id = song.alloc_mod_routing_id();
            let routings = if track_id == common::model::MASTER_TRACK_ID {
                &mut song.song_mod_routings
            } else {
                match song.track_by_id_mut(track_id) {
                    Some(t) => &mut t.mod_routings,
                    None => return false,
                }
            };
            routings.push(common::model::ModRouting {
                id,
                target,
                source_id,
                depth: 1.0,
                polarity: common::model::Polarity::Unipolar,
            });
            true
        })
        .unwrap_or(false)
    }

    pub(crate) fn remove_mod_routing(
        &mut self,
        track_id: u32,
        target: common::model::AutomationTarget,
        source_id: u32,
    ) {
        self.edit_mod_routings(track_id, |routings| {
            routings.retain(|r| !(r.source_id == source_id && r.target == target));
        });
        // r.md #89: 消した変調の **深さ** を指していた変調も連鎖して落とす。
        self.edit_song(|song| song.prune_dangling_mod_targets());
    }

    pub(crate) fn set_mod_routing_depth(
        &mut self,
        track_id: u32,
        target: common::model::AutomationTarget,
        source_id: u32,
        depth: f32,
    ) {
        // depth は GUI compose が毎フレーム読む visual-only 値 (Phase 4)。 scrub
        // ドラッグ中の per-frame LoadSong を避け、 dirty マークだけ立てる
        // (= edit_song が epoch を bump)。
        let touched = self.edit_mod_routings(track_id, |routings| {
            let r = routings
                .iter_mut()
                .find(|r| r.source_id == source_id && r.target == target)?;
            r.depth = depth.clamp(-1.0, 1.0);
            Some(r.id)
        });
        // r.md #89: 深さ自体も変調先 / オートメーション先なので、触ったことを記録する。
        if let Some(Some(routing_id)) = touched {
            self.note_touched_mod_target(common::model::AutomationTarget::ModRoutingDepth {
                routing_id,
            });
        }
    }

    pub(crate) fn set_mod_routing_polarity(
        &mut self,
        track_id: u32,
        target: common::model::AutomationTarget,
        source_id: u32,
        bipolar: bool,
    ) {
        self.edit_mod_routings(track_id, |routings| {
            if let Some(r) = routings
                .iter_mut()
                .find(|r| r.source_id == source_id && r.target == target)
            {
                r.polarity = if bipolar {
                    common::model::Polarity::Bipolar
                } else {
                    common::model::Polarity::Unipolar
                };
            }
        });
    }

    pub(crate) fn set_mod_source_track(&mut self, id: u32, source_track: u32) {
        self.edit_mod_source_follower(id, |tap, _| tap.source_track = source_track);
    }

    pub(crate) fn set_mod_source_attack(&mut self, id: u32, ms: f32) {
        // 係数は recompile 時に bake される。 scrub ドラッグ中の per-frame
        // LoadSong を避けるため dirty マークのみ (= edit_song が epoch を bump)。
        // drag-end に sync する (track_inspector の mod_follower_scrub_active エッジ検出)。
        self.edit_mod_source_follower(id, |_, follower| follower.attack_ms = ms.max(0.0));
    }

    pub(crate) fn set_mod_source_release(&mut self, id: u32, ms: f32) {
        self.edit_mod_source_follower(id, |_, follower| follower.release_ms = ms.max(0.0));
    }

    pub(crate) fn set_mod_follower_scrubbing(&mut self, active: bool) {
        // Drag-end edge (was scrubbing, now not) → recompile the baked follower
        // coefficients once with the final attack/release values.
        self.ui_ephemeral.mod_follower_scrub_active = active;
    }

    pub(crate) fn set_mod_source_tap_point(&mut self, id: u32, tap_point: common::model::TapPoint) {
        // tap は EnvelopeFollower{tap} 内に内包 (generator には無い)。
        // dbfed6c の 3 段 TapPoint (PreFx/PostFx/PostFader) をそのまま設定。
        self.edit_mod_source_follower(id, |tap, _| tap.tap_point = tap_point);
        // tap_point は schedule の BufRef を変えるので recompile が要る。
    }

    pub(crate) fn set_aux_input_tap_point(
        &mut self,
        device_id: u64,
        port: u8,
        tap_point: common::model::TapPoint,
    ) {
        self.edit_song(|song| {
            if let Some(inst) = device_mut_by_id(song, device_id)
                && let Some(route) = inst
                    .aux_inputs
                    .get_mut(port as usize)
                    .and_then(|o| o.as_mut())
            {
                route.tap.tap_point = tap_point;
            }
        });
    }

}
