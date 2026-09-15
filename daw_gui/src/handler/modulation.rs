//! handler::modulation — modulation source / routing の追加・編集・削除
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use common::model::{
    MOD_BAND_HZ_MAX, MOD_BAND_HZ_MIN, MOD_FOLLOWER_GAIN_MAX, MOD_FOLLOWER_GAIN_MIN, ModParam,
};

/// Pulse の duty を 0..=1 に収めた shape。 duty は「値」 なので、兄弟 param
/// (phase / smooth / slew) と同じく **書き戻しのチョークポイントで** clamp する。
/// 評価側 (`lfo_shape_value`) も clamp するが、範囲外のまま保存されると欄の表示と
/// 実出力が食い違ったまま残る (欄は "1.50"、音は常時 1.0 の直線)。
fn clamped_lfo_shape(shape: common::model::LfoShape) -> common::model::LfoShape {
    match shape {
        common::model::LfoShape::Pulse { width } => {
            common::model::LfoShape::Pulse { width: width.clamp(0.0, 1.0) }
        }
        other => other,
    }
}

impl AppData {
    // ---- docs/plan_modulation.md §9: modulation source / routing CRUD ----
    // すべて `Song` を mutate して `flush_song_sync` で締める
    // (audio engine が follower schedule を再 compile、 preview が再合成)。

    pub(crate) fn add_mod_source(&mut self, tag: ModSourceKindTag) {
        use common::model::{ModSourceKind, RandomConfig};
        // 帰属トラック = カーソルトラック (= このラックを開いているトラック)。以後
        // inspector ではこのトラックの下にだけ列挙される。
        let cursor_track = self.cursor_track_id();
        let owner_track_id = cursor_track.unwrap_or(0);
        // follower の follow 先は初期 = カーソルトラック (master は音を tap できないので入力なし)。
        let follower_tap = cursor_track
            .filter(|&t| self.cur.song_doc.song().track_by_id(t).is_some())
            .map(common::model::AudioTap::post_fader);
        let _ = self
            .edit_song(move |song| {
                let id = song.alloc_mod_source_id();
                let color = common::model::ModSource::palette_color(song.mod_sources.len());
                let kind = match tag {
                    ModSourceKindTag::Follower => ModSourceKind::EnvelopeFollower {
                        tap: follower_tap,
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
                    ModSourceKindTag::Adsr => ModSourceKind::Adsr(Default::default()),
                };
                song.mod_sources.push(common::model::ModSource {
                    id,
                    owner_track_id,
                    color,
                    kind,
                    enabled: true,
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
        f: impl FnOnce(&mut Option<common::model::AudioTap>, &mut common::model::FollowerConfig),
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
        if !self.cur.song_doc.song().mod_sources.iter().any(|m| m.id == id) {
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
                    c.shape = clamped_lfo_shape(shape);
                }
            }
            ModSourceEdit::LfoPhase(p) => {
                if let ModSourceKind::Lfo(c) = &mut m.kind {
                    c.phase = p.clamp(0.0, 1.0);
                }
                scrub = true;
            }
            // r.md #116: Shape / Jitter / Smooth は 0..=1、 Steps は上限 24、 時間は拍で上限 64。
            ModSourceEdit::LfoShapeAmt(v) => {
                if let ModSourceKind::Lfo(c) = &mut m.kind {
                    c.shape_amt = v.clamp(0.0, 1.0);
                }
                scrub = true;
            }
            ModSourceEdit::LfoJitter(v) => {
                if let ModSourceKind::Lfo(c) = &mut m.kind {
                    c.jitter = v.clamp(0.0, 1.0);
                }
                scrub = true;
            }
            ModSourceEdit::LfoSmooth(v) => {
                if let ModSourceKind::Lfo(c) = &mut m.kind {
                    c.smooth = v.clamp(0.0, 1.0);
                }
                scrub = true;
            }
            ModSourceEdit::LfoSteps(n) => {
                if let ModSourceKind::Lfo(c) = &mut m.kind {
                    c.steps = n.min(common::model::LFO_STEPS_MAX);
                }
                scrub = true;
            }
            ModSourceEdit::LfoDelay(b) => {
                if let ModSourceKind::Lfo(c) = &mut m.kind {
                    c.delay_beats = if b.is_finite() { b.clamp(0.0, common::model::LFO_TIME_BEATS_MAX) } else { 0.0 };
                }
                scrub = true;
            }
            ModSourceEdit::LfoFadeIn(b) => {
                if let ModSourceKind::Lfo(c) = &mut m.kind {
                    c.fade_in_beats = if b.is_finite() { b.clamp(0.0, common::model::LFO_TIME_BEATS_MAX) } else { 0.0 };
                }
                scrub = true;
            }
            // r.md #117: ADSR。 時定数は `ADSR_TIME_MS_MIN..=MAX`、 sustain は 0..=1。
            ModSourceEdit::AdsrAttack(v) | ModSourceEdit::AdsrDecay(v) | ModSourceEdit::AdsrRelease(v) => {
                if let ModSourceKind::Adsr(c) = &mut m.kind
                    && v.is_finite()
                {
                    let v = v.clamp(common::model::ADSR_TIME_MS_MIN, common::model::ADSR_TIME_MS_MAX);
                    match edit {
                        ModSourceEdit::AdsrAttack(_) => c.attack_ms = v,
                        ModSourceEdit::AdsrDecay(_) => c.decay_ms = v,
                        _ => c.release_ms = v,
                    }
                }
                scrub = true;
            }
            ModSourceEdit::AdsrSustain(v) => {
                if let ModSourceKind::Adsr(c) = &mut m.kind {
                    c.sustain = v.clamp(0.0, 1.0);
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
            self.note_touched_mod_param(id, param);
        }
    }

    /// r.md #89: モジュレーターのツマミの **今の値** (plain)。ラックのツマミ・
    /// オートメーションレーンの既定値・変調の base が全部ここを通る
    /// (値の SSoT は `common::mod_graph::param_plain`)。
    /// ソースが居なければ 0 (ラックのツマミは居るソースしか描かない)。
    pub(crate) fn mod_param_plain_value(
        &self,
        source_id: u32,
        param: common::model::ModParam,
    ) -> f64 {
        self.mod_param_plain(source_id, param).unwrap_or(0.0)
    }

    /// [`Self::mod_param_plain_value`] の本体。`None` = ソースが居ない
    /// (`target_plain_value` はこれを「値の出所が無い」 として扱う)。
    pub(crate) fn mod_param_plain(&self, source_id: u32, param: common::model::ModParam) -> Option<f64> {
        let song = self.cur.song_doc.song();
        let m = song.mod_sources.iter().find(|m| m.id == source_id)?;
        Some(common::mod_graph::param_plain(&m.kind, param, f64::from(song.bpm)))
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
            ModSourceEdit::LfoShapeAmt(_) => Some(ModParam::LfoShapeAmt),
            ModSourceEdit::LfoJitter(_) => Some(ModParam::LfoJitter),
            ModSourceEdit::LfoSmooth(_) => Some(ModParam::LfoSmooth),
            ModSourceEdit::AdsrAttack(_) => Some(ModParam::AdsrAttack),
            ModSourceEdit::AdsrDecay(_) => Some(ModParam::AdsrDecay),
            ModSourceEdit::AdsrSustain(_) => Some(ModParam::AdsrSustain),
            ModSourceEdit::AdsrRelease(_) => Some(ModParam::AdsrRelease),
            // Pulse の duty は shape に載っているので、Pulse を選び直したときだけ拾う。
            ModSourceEdit::LfoShape(common::model::LfoShape::Pulse { .. }) => {
                Some(ModParam::LfoPulseWidth)
            }
            ModSourceEdit::RandomSmooth(_) => Some(ModParam::RandomSmooth),
            ModSourceEdit::StepsSlew(_) => Some(ModParam::StepsSlew),
            _ => None,
        }
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
        let Some(source_id) = self.cur.peph.armed_mod_source else {
            return;
        };
        let label = self.automation_target_label(&target);
        let added = self.add_mod_routing(track_id, target, source_id);
        self.cur.peph.armed_mod_source = None;
        // 既に繋がっていた param を再ドラッグしただけのときに「割り当てました」と
        // 出すと、 何が起きたかを取り違える。 起きた事実をそのまま出す。
        self.ui_ephemeral.status_message = match added {
            Some(true) => format!("変調を割り当てました → {label}"),
            Some(false) => format!("変調の深さを更新しました → {label}"),
            None => format!("変調を割り当てられません (対象かモジュレーターが削除されました) → {label}"),
        };
    }

    /// r.md #78: 待受中 (◉) のソースの `(色, 表示名)`。 ステータスバーが
    /// 「今どのソースが待受中か」を常時出すために使う。 ラックはカーソルトラック
    /// 所有のソースしか列挙しないので、 トラックを移ると ◉ ボタン自体が画面から
    /// 消える。 待受の可視化を ◉ ボタンだけに任せられない理由がこれ。
    pub fn armed_mod_source_label(&self) -> Option<([f32; 3], String)> {
        let sid = self.cur.peph.armed_mod_source?;
        let song = self.cur.song_doc.song();
        let src = song.mod_sources.iter().find(|m| m.id == sid)?;
        let track = song.track_display_name(src.owner_track_id);
        Some((src.color, format!("{track} / {}", src.kind.short_label())))
    }

    /// モジュレーターを外す。このソースを使う変調、このソースのツマミを指すレーン / 変調、消えた変調の深さを
    /// 指す変調までの連鎖掃除は、SongDoc の編集後の不変条件 (`Song::prune_dangling_param_targets`) が同じ undo
    /// step で担い、待ち受け (◉) の解除は `reconcile_song_refs` が担う (r.md #129)。帰属トラックが消えた
    /// モジュレーターの削除も不変条件 (`Song::prune_orphan_mod_sources`) がトラックを外した編集の中で行う。
    pub(crate) fn remove_mod_source(&mut self, id: u32) {
        self.edit_song(move |song| song.mod_sources.retain(|m| m.id != id));
    }

    /// `target` への**既存の**変調 routing が載る store を `f` に渡す (解除 / 深さ / 極性。足すのは
    /// [`Self::add_mod_routing`] だけ)。`f` は `(戻り値, 実際に変えたか)` を返し、変えていなければ undo も `*` も
    /// 積まない (`edit_song_checked`)。既存の routing は enforce を通って残っている = 解決済みなので、ここでは
    /// 解決を判定しない (深さのドラッグの毎フレームで node 表を作り直さない)。
    ///
    /// r.md #129 (§7.7): store の持ち主は **実行時の Song** から
    /// [`param_owner`](crate::handler::param_value::param_owner) で引き直す — view が渡す
    /// `track_id` は target だけでは持ち主が決まらない住所 (Volume / Pan …) のためだけに使う
    /// (同じフレームで device を他トラックへ運んだ後だと、view の track id は古い)。
    /// 束縛先が居なければ Song を編集せず `None`。
    pub(crate) fn edit_mod_routings<R>(
        &mut self,
        track_id: u32,
        target: &common::model::AutomationTarget,
        f: impl FnOnce(&mut Vec<common::model::ModRouting>) -> (R, bool),
    ) -> Option<R> {
        let owner = crate::handler::param_value::param_owner(&self.cur.song_doc, target, track_id)?;
        let mut out = None;
        self.edit_song_checked(|song| {
            let Some((_, routings)) = song.param_stores_mut(owner) else {
                return false;
            };
            let (r, changed) = f(routings);
            out = Some(r);
            changed
        });
        out
    }

    /// 戻り値: `Some(true)` = 足した / `Some(false)` = 既に同じ (target, source) がある /
    /// `None` = target かモジュレーターが解決しない (Song を編集しない)。
    /// per-control の depth ドラッグは毎フレームここを通るので、 呼び出し側が
    /// 「今つないだ」 と「もう繋がっていた」 を区別できるようにしている。
    /// 載せる store は [`Self::edit_mod_routings`] と同じく target の持ち主。
    pub(crate) fn add_mod_routing(
        &mut self,
        track_id: u32,
        target: common::model::AutomationTarget,
        source_id: u32,
    ) -> Option<bool> {
        // 実際に追加したときだけ recompile (per-control depth ドラッグは毎フレーム
        // AddModRouting を呼ぶので、no-op add で sync すると LoadSong 連発になる)。
        //
        // r.md #89: id は **足すこの 1 箇所**で採番する (`AutomationTarget::ModRoutingDepth`
        // が 1 本の変調を指すので、後から `ensure_ids` 任せにすると採番前の一瞬だけ
        // 深さを変調先にできない窓ができる)。
        let song = self.cur.song_doc.song();
        let owner = crate::handler::param_value::param_owner(&self.cur.song_doc, &target, track_id)?;
        let (_, routings) = song.param_stores(owner)?;
        if routings.iter().any(|r| r.source_id == source_id && r.target == target) {
            return Some(false);
        }
        // r.md #129: 解決しない routing (消えたモジュレーター / 種類違いの住所) を積むと enforce が同じ編集の中で
        // 消し、中身の無い undo step と `*` だけが残る。規則は prune の routing の retain と同じ。
        if !song.mod_routing_resolves(&target, source_id, owner) {
            return None;
        }
        let added = self.edit_song_checked(move |song| {
            let id = song.alloc_mod_routing_id();
            let Some((_, routings)) = song.param_stores_mut(owner) else {
                return false;
            };
            routings.push(common::model::ModRouting {
                id,
                target,
                source_id,
                depth: 1.0,
                polarity: common::model::Polarity::Unipolar,
                enabled: true,
            });
            true
        });
        Some(added)
    }

    pub(crate) fn remove_mod_routing(
        &mut self,
        track_id: u32,
        target: common::model::AutomationTarget,
        source_id: u32,
    ) {
        // r.md #89: 消した変調の **深さ** を指していた変調の連鎖掃除は、SongDoc の
        // `enforce_edit_invariants` が同じ undo step で担う (r.md #129)。
        self.edit_mod_routings(track_id, &target, |routings| {
            let before = routings.len();
            routings.retain(|r| !(r.source_id == source_id && r.target == target));
            ((), routings.len() != before)
        });
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
        let touched = self.edit_mod_routings(track_id, &target, |routings| {
            let Some(r) = routings.iter_mut().find(|r| r.source_id == source_id && r.target == target) else {
                return (None, false);
            };
            let depth = depth.clamp(-1.0, 1.0);
            let changed = r.depth != depth;
            r.depth = depth;
            (Some(r.id), changed)
        });
        // r.md #89: 深さ自体も変調先 / オートメーション先なので、触ったことを記録する。
        if let Some(Some(routing_id)) = touched {
            self.note_touched_target(common::model::AutomationTarget::ModRoutingDepth { routing_id }, track_id);
        }
    }

    /// r.md #115: 1 本の変調のバイパス。 住所は安定 `ModRouting::id` (どの store に居ても引く)。
    /// 合成側 (`modulation_offset_norm_with`) と計画 (`build_plan`) が `enabled` を見る。
    pub(crate) fn set_mod_routing_enabled(&mut self, routing_id: u32, enabled: bool) {
        self.edit_song_checked(move |song| {
            let Some(r) = song.mod_routing_by_id_mut(routing_id) else {
                return false;
            };
            if r.enabled == enabled {
                return false;
            }
            r.enabled = enabled;
            true
        });
    }

    /// r.md #115: モジュレーター全体のバイパス。 `build_plan` がこの source を計画から外す
    /// (= 値面に載らず、 これを引く routing / 辺は全部無効)。 routing 側の `enabled` は据え置き。
    pub(crate) fn set_mod_source_enabled(&mut self, id: u32, enabled: bool) {
        self.edit_song_checked(move |song| {
            let Some(m) = song.mod_sources.iter_mut().find(|m| m.id == id) else {
                return false;
            };
            if m.enabled == enabled {
                return false;
            }
            m.enabled = enabled;
            true
        });
    }

    pub(crate) fn set_mod_routing_polarity(
        &mut self,
        track_id: u32,
        target: common::model::AutomationTarget,
        source_id: u32,
        bipolar: bool,
    ) {
        let polarity = if bipolar { common::model::Polarity::Bipolar } else { common::model::Polarity::Unipolar };
        self.edit_mod_routings(track_id, &target, |routings| {
            let Some(r) = routings.iter_mut().find(|r| r.source_id == source_id && r.target == target) else {
                return ((), false);
            };
            ((), std::mem::replace(&mut r.polarity, polarity) != polarity)
        });
    }

    /// r.md #110: follower の source は track か同 track の Parallel 内 chain。`None` = 入力なし。
    /// tap 点は今の配線から引き継ぐ (入力なしから配線したときは既定の Post-Fader)。
    pub(crate) fn set_mod_source_tap_source(&mut self, id: u32, source: Option<common::model::TapSource>) {
        self.edit_mod_source_follower(id, |tap, _| {
            let tap_point = tap.map(|t| t.tap_point).unwrap_or_default();
            *tap = source.map(|s| common::model::AudioTap::new(s, tap_point));
        });
    }

    pub(crate) fn set_mod_source_attack(&mut self, id: u32, ms: f32) {
        // 係数は recompile 時に bake される。 scrub ドラッグ中の per-frame
        // LoadSong を避けるため dirty マークのみ (= edit_song が epoch を bump)。
        // drag-end に sync する (track_inspector の mod_follower_scrub_active エッジ検出)。
        self.edit_mod_source_follower(id, |_, follower| follower.attack_ms = ms.max(0.0));
        self.note_touched_mod_param(id, ModParam::FollowerAttack);
    }

    pub(crate) fn set_mod_source_release(&mut self, id: u32, ms: f32) {
        self.edit_mod_source_follower(id, |_, follower| follower.release_ms = ms.max(0.0));
        self.note_touched_mod_param(id, ModParam::FollowerRelease);
    }

    /// r.md #88: 検出前ゲイン。 attack/release と同じく係数は recompile で bake される
    /// (`daw_audio/src/graph/follower.rs` の `from_config`)。
    pub(crate) fn set_mod_source_gain(&mut self, id: u32, gain: f32) {
        self.edit_mod_source_follower(id, |_, follower| {
            follower.gain = gain.clamp(MOD_FOLLOWER_GAIN_MIN, MOD_FOLLOWER_GAIN_MAX);
        });
        self.note_touched_mod_param(id, ModParam::FollowerGain);
    }

    /// r.md #88: 検出モード (Peak / RMS)。 **値ではない編集**なので「触った parameter」
    /// には記録しない (`edit_touched_param` が形 / 種別を `None` に落とすのと同じ規約)。
    pub(crate) fn set_mod_source_mode(&mut self, id: u32, mode: common::model::FollowerMode) {
        self.edit_mod_source_follower(id, |_, follower| follower.mode = mode);
    }

    /// r.md #88: 検出前の全波整流。 これも値ではないので記録しない。
    pub(crate) fn set_mod_source_rectify(&mut self, id: u32, rectify: bool) {
        self.edit_mod_source_follower(id, |_, follower| follower.rectify = rectify);
    }

    /// r.md #88: 検出前の帯域制限 (`None` で全帯域)。 `hp <= lp` に整えてから入れる —
    /// 逆転した帯域は一次フィルタ 2 段が互いを打ち消して**無音を検出し続ける**ので、
    /// 「効かない」 が値からは読めない状態になる。
    ///
    /// 「触った parameter」は **前後を突き合わせて動いた側だけ** 記録する。 両方書くと
    /// `A` キーが常に片方のレーンを作ることになるし、 on/off の切替は値ではない。
    pub(crate) fn set_mod_source_band(
        &mut self,
        id: u32,
        band: Option<common::model::BandFilter>,
    ) {
        let band = band.map(|b| common::model::BandFilter {
            hp_hz: b.hp_hz.clamp(MOD_BAND_HZ_MIN, MOD_BAND_HZ_MAX),
            lp_hz: b.lp_hz.clamp(b.hp_hz.clamp(MOD_BAND_HZ_MIN, MOD_BAND_HZ_MAX), MOD_BAND_HZ_MAX),
        });
        let prev = self
            .cur.song_doc
            .song()
            .mod_sources
            .iter()
            .find(|m| m.id == id)
            .and_then(|m| m.follower().and_then(|(_, f)| f.band_filter));
        let touched = match (prev, band) {
            (Some(a), Some(b)) if a.hp_hz != b.hp_hz => Some(ModParam::FollowerHpHz),
            (Some(a), Some(b)) if a.lp_hz != b.lp_hz => Some(ModParam::FollowerLpHz),
            _ => None,
        };
        self.edit_mod_source_follower(id, |_, follower| follower.band_filter = band);
        if let Some(param) = touched {
            self.note_touched_mod_param(id, param);
        }
    }

    /// モジュレーターのツマミを触ったことを記録する薄いラッパ。 `note_touched_target`
    /// が唯一の記録口なので、 ここは `AutomationTarget` を組み立てるだけ (ソースの id で
    /// 束縛する住所なので、持ち主はソースの帰属から決まり fallback は使われない)。
    fn note_touched_mod_param(&mut self, source_id: u32, param: ModParam) {
        self.note_touched_target(
            common::model::AutomationTarget::ModSourceParam { source_id, param },
            common::model::MASTER_TRACK_ID,
        );
    }

    pub(crate) fn set_mod_follower_scrubbing(&mut self, active: bool) {
        // Drag-end edge (was scrubbing, now not) → recompile the baked follower
        // coefficients once with the final attack/release values.
        self.cur.peph.mod_follower_scrub_active = active;
    }

    pub(crate) fn set_mod_source_tap_point(&mut self, id: u32, tap_point: common::model::TapPoint) {
        // tap は EnvelopeFollower{tap} 内に内包 (generator には無い)。
        // dbfed6c の 3 段 TapPoint (PreFx/PostFx/PostFader) をそのまま設定。入力なしには点が無い。
        self.edit_mod_source_follower(id, |tap, _| {
            if let Some(tap) = tap {
                tap.tap_point = tap_point;
            }
        });
        // tap_point は schedule の BufRef を変えるので recompile が要る。
    }

    pub(crate) fn set_aux_input_tap_point(
        &mut self,
        device_id: u64,
        port: u8,
        tap_point: common::model::TapPoint,
    ) {
        // r.md #129: plugin と内蔵 Comp / Bus Comp (port 0) 共通の slot。
        self.edit_song(|song| {
            let owner = song.device_owner_track(device_id);
            if let Some(dev) = song.device_by_id_mut(device_id)
                && let Some(route) = dev.aux_input_slot_mut(port).and_then(|o| o.as_mut())
                // 自 track を source にする route は Pre-FX 固定 (他は feedback)。
                && !(matches!(route.tap.source, common::model::TapSource::Track(t) if Some(t) == owner)
                    && tap_point != common::model::TapPoint::PreFx)
            {
                route.tap.tap_point = tap_point;
            }
        });
    }

}
