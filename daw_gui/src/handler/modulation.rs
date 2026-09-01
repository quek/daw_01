//! handler::modulation — modulation source / routing の追加・編集・削除
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use common::model::{
    MOD_BAND_HZ_MAX, MOD_BAND_HZ_MIN, MOD_FOLLOWER_GAIN_MAX, MOD_FOLLOWER_GAIN_MIN,
};

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
        self.edit_mod_routings(track_id, |routings| {
            if routings
                .iter()
                .any(|r| r.source_id == source_id && r.target == target)
            {
                false
            } else {
                routings.push(common::model::ModRouting {
                    target,
                    source_id,
                    depth: 1.0,
                    polarity: common::model::Polarity::Unipolar,
                });
                true
            }
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
        self.edit_mod_routings(track_id, |routings| {
            if let Some(r) = routings
                .iter_mut()
                .find(|r| r.source_id == source_id && r.target == target)
            {
                r.depth = depth.clamp(-1.0, 1.0);
            }
        });
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

    /// r.md #88: 検出前ゲイン。 attack/release と同じく係数は recompile で bake される
    /// (`daw_audio/src/graph/follower.rs` の `from_config`)。
    pub(crate) fn set_mod_source_gain(&mut self, id: u32, gain: f32) {
        self.edit_mod_source_follower(id, |_, follower| {
            follower.gain = gain.clamp(MOD_FOLLOWER_GAIN_MIN, MOD_FOLLOWER_GAIN_MAX);
        });
    }

    /// r.md #88: 検出モード (Peak / RMS)。
    pub(crate) fn set_mod_source_mode(&mut self, id: u32, mode: common::model::FollowerMode) {
        self.edit_mod_source_follower(id, |_, follower| follower.mode = mode);
    }

    /// r.md #88: 検出前の全波整流。
    pub(crate) fn set_mod_source_rectify(&mut self, id: u32, rectify: bool) {
        self.edit_mod_source_follower(id, |_, follower| follower.rectify = rectify);
    }

    /// r.md #88: 検出前の帯域制限 (`None` で全帯域)。 `hp <= lp` に整えてから入れる —
    /// 逆転した帯域は一次フィルタ 2 段が互いを打ち消して**無音を検出し続ける**ので、
    /// 「効かない」 が値からは読めない状態になる。
    pub(crate) fn set_mod_source_band(
        &mut self,
        id: u32,
        band: Option<common::model::BandFilter>,
    ) {
        let band = band.map(|b| common::model::BandFilter {
            hp_hz: b.hp_hz.clamp(MOD_BAND_HZ_MIN, MOD_BAND_HZ_MAX),
            lp_hz: b.lp_hz.clamp(b.hp_hz.clamp(MOD_BAND_HZ_MIN, MOD_BAND_HZ_MAX), MOD_BAND_HZ_MAX),
        });
        self.edit_mod_source_follower(id, |_, follower| follower.band_filter = band);
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
