//! 変調の**制御グリッド走行体** — buffer を 64 サンプルの刻みに割り、
//! `common::mod_graph::tick` を回して刻みごとの値面を作る。
//!
//! 設計正本 `docs/plan_rmd_88_89_cross_modulation.md` §2.2 / §4-3 / §4-4。
//!
//! # なぜ 1 本なのか
//!
//! live (`engine.rs`) と書き出し (`export.rs`) は buffer 長が違う (device の
//! 実測長 vs 1024 固定)。変調を「buffer の頭で 1 回評価して buffer 定数として
//! 当てる」と、**同じ曲でも段差の位置が両者で違う** — 聴いた通りに書き出されない。
//! `render_master_buffer` を live/export で共有しているのと同じ理由 (アーキ不変条件 6)
//! で、刻みの割り方・transport の進め方・値面の作り方もここ 1 本にする。
//!
//! # 刻み境界は絶対 song サンプル位置に整列する
//!
//! 刻み `k` の境界は絶対サンプル `k * MOD_TICK_FRAMES`。buffer の切れ目ではなく
//! 曲の頭からの位置で決まるので、buffer 長が 480 だろうと 1024 だろうと **踏む刻みの
//! 列が同じ**になる。同じ sample rate なら live と WAV がビット一致する。
//!
//! # transport の進め方は `next_mark` が SSoT
//!
//! `secs` は刻み番号からの積、`beat` は累算、`bpm` は `SongTempo` カーブ + その変調。
//! [`ModPhaseTable`] の構築も `mod_graph::locate` もこの同じ漸化式を踏むので、
//! 「どこから再生しても同じ位相」が近似ではなく厳密に成立する。ここで独自に
//! `playhead_beats += frames * bpm / (60·SR)` と進めると、その一致が壊れる。
//!
//! RT 安全: 確保・ロック・I/O 無し。`ModSourceKind` を clone しない
//! (`MsegConfig.points` / `StepsConfig.values` が `Vec` なので clone は heap 確保)。

use std::sync::Arc;

use common::mod_graph::{ModPhaseTable, ModPlan, ModRuntime, PhaseMark, TickCtx};

/// 制御グリッドの刻み幅 (サンプル)。定義の SSoT は `common::mod_graph` で、
/// ここは daw_audio 側の再公開 (`crate::automation` の automation サブバッファ刻みが
/// これを引く — automation の段と変調の段は **同じ格子でなければならない**)。
pub use common::mod_graph::MOD_TICK_FRAMES;
use common::mod_plane::{ModPlane, ModTickPlane, ModTickPlaneRef};
use common::model::{AutomationTarget, MASTER_TRACK_ID, ModParam, Song};

/// 1 buffer で踏みうる刻みの上限 (`MAX_FRAMES / MOD_TICK_FRAMES` + 前後の端数 2)。
/// 行 / mark の器はこの数で事前確保して RT で伸ばさない。
pub const MAX_TICKS_PER_BUFFER: usize =
    common::process_data::MAX_FRAMES / MOD_TICK_FRAMES as usize + 2;

/// 値面に載せるソース数の上限。
///
/// **`mod_graph::build_plan` は `Song::mod_sources` を切らない** (グラフの
/// 正しさに上限は要らないので)。一方 RT 側の器はここで事前確保するので、
/// 切らずに回すと 64 を超える曲で **audio thread が再確保する**。
/// `compile_schedule` の `follower_slots` と `AudioBridge::mod_scalars` が
/// 既に同じ上限で切っているので、値面もそこに合わせる
/// (= 65 個目以降のソースは値を publish しない、という既存の契約)。
const MAX_SLOTS: usize = common::audio_bridge::MAX_MOD_SOURCES;

/// フォロワー係数のうち変調できるもの (刻みごとに引き直す)。
pub const FOLLOWER_PARAMS: [ModParam; 5] = [
    ModParam::FollowerAttack,
    ModParam::FollowerRelease,
    ModParam::FollowerGain,
    ModParam::FollowerHpHz,
    ModParam::FollowerLpHz,
];

/// 1 刻みぶんのフォロワー実効係数 (plain 単位)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FollowerEff {
    pub attack_ms: f32,
    pub release_ms: f32,
    pub gain: f32,
    pub hp_hz: f32,
    pub lp_hz: f32,
}

/// buffer の 1 区間 (刻みに割った断片)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TickSpan {
    /// 値面の行番号 (この区間の**入口**の値)。
    pub row: usize,
    /// buffer 内の frame offset。
    pub frame: u32,
    /// この区間の frame 数。
    pub frames: u32,
}

/// 制御グリッドを走る本体。engine (`LocalState`) と export (`render_loop`) が
/// 1 つずつ持つ。
#[derive(Debug, Default)]
pub struct ModTickRunner {
    /// off-RT で作られた評価計画 (RT は読むだけ)。
    pub plan: Arc<ModPlan>,
    /// 積分 tier の位相表 (off-thread build)。無ければ閉形式シードに倒れる。
    pub table: Option<Arc<ModPhaseTable>>,
    /// 位相 / 値 / base を持つ RT 状態 (plan と対で差し替える)。
    pub rt: ModRuntime,

    /// 値面の行 0 が指す刻み番号。`i64::MIN` = 未着地 (次の buffer で張り直す)。
    first_tick: i64,
    /// `marks[i]` = 刻み `first_tick + i` の入口の transport 状態。
    /// 行と 1:1 で、行を捨てるときに同じだけ捨てる。
    marks: Vec<PhaseMark>,
    /// 次に評価する刻み (= `first_tick + marks.len()`) の transport 状態。
    next_mark: PhaseMark,

    /// 刻みごとの値面 (この buffer が参照する範囲)。
    plane: ModTickPlane,
    /// 1 行ぶんの scratch。
    row: Vec<f32>,
    /// r.md #89 Q9: 深さの 1 行ぶん scratch (列 = `plan.depth_groups`)。
    depth_row: Vec<f32>,
    /// 同上の列 id (`plane.reset` に渡す。plan 差し替え時にだけ作り直す)。
    depth_ids: Vec<u32>,
    /// フォロワー係数の刻みごとの表 (行 = 刻み、列 = `follower_cols`)。
    /// 係数が変調されているフォロワーが 1 つも無ければ空 = ゼロコスト。
    follower_eff: Vec<FollowerEff>,
    /// `follower_eff` の列に対応する plan slot。
    follower_cols: Vec<u16>,
    /// GUI / sidecar へ出す 1 点の面。
    publish: ModPlane,
    /// この buffer の区間割り。
    spans: Vec<TickSpan>,
}

impl ModTickRunner {
    #[must_use]
    pub fn new() -> Self {
        let n = common::audio_bridge::MAX_MOD_SOURCES;
        Self {
            plan: Arc::new(ModPlan::default()),
            table: None,
            rt: ModRuntime::default(),
            first_tick: i64::MIN,
            marks: Vec::with_capacity(MAX_TICKS_PER_BUFFER),
            next_mark: PhaseMark::default(),
            plane: ModTickPlane::with_capacity(n, MAX_TICKS_PER_BUFFER),
            row: Vec::with_capacity(n),
            // 深さが動く変調はソース数より多くなり得ないので同じ器で足りる。
            depth_row: Vec::with_capacity(n),
            depth_ids: Vec::with_capacity(n),
            follower_eff: Vec::with_capacity(n * MAX_TICKS_PER_BUFFER),
            follower_cols: Vec::with_capacity(n),
            publish: ModPlane::with_capacity(n),
            spans: Vec::with_capacity(MAX_TICKS_PER_BUFFER),
        }
    }

    /// 値面に載せる slot 数 (`MAX_SLOTS` で切った plan の node 数)。
    #[must_use]
    #[inline]
    fn n_slots(&self) -> usize {
        self.plan.nodes.len().min(MAX_SLOTS)
    }

    /// 新しい plan と、それに合わせて **off-thread で `install` 済み**の RT 状態を
    /// 差し込む (`ModRuntime::install` は `Vec::resize` するので RT では走らせない)。
    /// 走行状態は捨てて次の buffer で張り直す。
    ///
    /// 戻り値は **旧 plan と旧 RT 状態** — どちらも `Vec` を抱えるので、
    /// audio thread で drop させず呼び出し側が recycle 経路へ渡すこと
    /// (`self.plan = plan` の代入で旧 `Arc` を落とすと、最後の参照だったときに
    /// `Vec<ModNode>` / 各 `in_edges` / MSEG の `points` が RT で free される)。
    pub fn install(&mut self, plan: Arc<ModPlan>, rt: ModRuntime) -> (Arc<ModPlan>, ModRuntime) {
        self.follower_cols.clear();
        for (slot, node) in plan.nodes.iter().enumerate().take(MAX_SLOTS) {
            let Ok(s) = u16::try_from(slot) else { continue };
            // 係数が **動く** フォロワーだけ刻みごとに引き直す。動かす経路は
            // 変調の辺と automation lane の 2 つ (r.md #89 Q4)。lane を数えないと
            // 「レーンで Attack を描いたのに compile 時の係数のまま鳴る」になる。
            let by_edge = node
                .in_edges
                .iter()
                .any(|e| FOLLOWER_PARAMS.contains(&e.param));
            let by_lane = plan
                .lane_params
                .iter()
                .any(|(ls, p)| *ls == s && FOLLOWER_PARAMS.contains(p));
            if by_edge || by_lane {
                self.follower_cols.push(s);
            }
        }
        let old_plan = std::mem::replace(&mut self.plan, plan);
        // 深さが動く変調も slot と同じ `MAX_SLOTS` で切る (`depth_row` / `depth_ids` /
        // `publish` の器がその大きさ)。溢れた群は「深さが動かない」扱いに degrade する
        // (= モデルの深さのまま鳴る)。ここは RT なので、切らずに extend すると再確保になる。
        self.depth_ids.clear();
        self.depth_ids
            .extend(self.plan.depth_groups.iter().take(MAX_SLOTS).map(|g| g.routing_id));
        self.marks.clear();
        self.plane.reset(&[], &[], MOD_TICK_FRAMES);
        self.first_tick = i64::MIN;
        (old_plan, std::mem::replace(&mut self.rt, rt))
    }

    /// 位相表を差し替える (旧表を返す — 呼び出し側が recycle する)。
    ///
    /// **差し替えたら走行を張り直す。** 表は off-thread で数十万刻みを回して作るので、
    /// 曲を開いた直後や Hz を動かした直後は「表が届く前に再生が始まる」。その間
    /// [`common::mod_graph::locate`] は表無しで閉形式シードに倒れるので、表が届いても
    /// 張り直さないと **積分 tier がシークするまで別の位相で鳴り続ける**
    /// (= 聴いた音と書き出しが一致しない)。
    pub fn set_table(&mut self, table: Option<Arc<ModPhaseTable>>) -> Option<Arc<ModPhaseTable>> {
        let prev = std::mem::replace(&mut self.table, table);
        if self.plan.needs_integration() {
            self.first_tick = i64::MIN;
        }
        prev
    }

    /// **シーク / ループ折返し / 再生開始で位相と transport を張り直す。**
    ///
    /// `beat` は tempo map が逆算した `sample` 位置の拍。刻み境界へ丸めた位置で
    /// 張り直すので、以降の前進は位相表と同じ格子に乗る。
    pub fn locate(&mut self, song: &Song, sample: u64, beat: f64, sample_rate: u32) {
        let k = tick_of(sample);
        let dt = dt_secs(sample_rate);
        let rem = sample % u64::from(MOD_TICK_FRAMES);
        let bpm0 = f64::from(common::automation::evaluate_song_tempo(song, beat)).max(1.0);
        // 刻み境界の拍 (sample が境界の途中なら手前の境界へ戻す)。刻み内は
        // bpm 一定 (= `next_mark` と同じ規則) なので線形に戻せる。
        let boundary_beat =
            beat - (rem as f64 / f64::from(sample_rate.max(1))) * bpm0 / 60.0;
        common::mod_graph::locate(
            &self.plan,
            &mut self.rt,
            self.table.as_deref(),
            song,
            sample_rate,
            k,
        );
        let bpm = f64::from(common::automation::evaluate_song_tempo(song, boundary_beat)).max(1.0);
        self.first_tick = k;
        self.marks.clear();
        let n = self.n_slots();
        self.plane.reset(&self.plan.slot_ids[..n], &self.depth_ids, MOD_TICK_FRAMES);
        self.follower_eff.clear();
        self.next_mark = PhaseMark {
            beat: boundary_beat,
            secs: k as f64 * dt,
            bpm,
        };
    }

    /// この buffer が踏む刻みを全部評価し、値面と区間割りを作る。
    ///
    /// `follower_env(plan_slot)` は envelope follower の直近値 (engine ring)。
    /// 戻り値は **buffer 頭**の transport (`beat` / `bpm`) — 以降の描画はこれを使う。
    ///
    /// RT 安全: 事前確保済みの器への書き込みのみ。
    pub fn run_buffer(
        &mut self,
        song: &Song,
        start_sample: u64,
        frames: u32,
        sample_rate: u32,
        mut follower_env: impl FnMut(u16, i64) -> f32,
    ) -> PhaseMark {
        let k0 = tick_of(start_sample);
        if self.first_tick == i64::MIN || k0 < self.first_tick {
            // 走行が途切れている (install 直後 / 巻き戻し)。次の評価から張り直す。
            self.first_tick = k0;
            self.marks.clear();
            let n = self.n_slots();
            self.plane.reset(&self.plan.slot_ids[..n], &self.depth_ids, MOD_TICK_FRAMES);
            self.follower_eff.clear();
        } else {
            // 前 buffer から持ち越した、もう参照しない行を捨てる。
            let stale = usize::try_from(k0 - self.first_tick)
                .unwrap_or(0)
                .min(self.marks.len());
            if stale > 0 {
                self.plane.drop_leading_rows(stale);
                self.marks.drain(..stale);
                self.drop_leading_eff(stale);
                self.first_tick += stale as i64;
            }
            if self.marks.is_empty() {
                self.first_tick = k0;
            }
        }

        let dt = dt_secs(sample_rate);
        // buffer 末の frame が乗る刻み **+1** まで評価する。最後の区間も両端の値が
        // 揃うので、buffer の切り方に依らず同じ補間になる (末尾だけ保持に落ちると
        // live と書き出しで音が変わる)。
        let last = u64::from(frames.saturating_sub(1));
        let k_end = tick_of(start_sample + last) + 1;
        while self.first_tick + self.marks.len() as i64 <= k_end
            && self.marks.len() < MAX_TICKS_PER_BUFFER
        {
            let k = self.first_tick + self.marks.len() as i64;
            self.eval_tick(song, k, dt, &mut follower_env);
        }

        #[allow(clippy::cast_possible_truncation)]
        let rem = (start_sample % u64::from(MOD_TICK_FRAMES)) as u32;
        self.plane.set_lead(MOD_TICK_FRAMES - rem);
        self.build_spans(frames, MOD_TICK_FRAMES - rem);

        let head = self.marks.first().copied().unwrap_or(self.next_mark);
        PhaseMark {
            beat: head.beat + f64::from(rem) / f64::from(sample_rate.max(1)) * head.bpm / 60.0,
            secs: start_sample as f64 / f64::from(sample_rate.max(1)),
            bpm: head.bpm,
        }
    }

    /// 1 刻み評価して行を積む。
    fn eval_tick(
        &mut self,
        song: &Song,
        k: i64,
        dt: f64,
        follower_env: &mut impl FnMut(u16, i64) -> f32,
    ) {
        // envelope follower の出力を先に書く (`tick` は引数で取らない —
        // plan の slot 順と `Song::mod_sources` の位置順の取り違えを防ぐため)。
        // 上限を超えるソースにも書く — 値面には載らないが、`tick` の入力辺として
        // 上限内のソースを変調しうるので評価自体は正しく回す必要がある。
        // **遅れは刻み固定** (`FOLLOWER_LAG_TICKS`)。同じ buffer の env は原理的に
        // 使えない (値面は音を描く前に作る) ので遡って読むが、遡る量を buffer 長では
        // なく刻み数で決めることで live (可変長) と書き出し (1024 固定) が一致する。
        let env_tick = k - crate::graph::follower::FOLLOWER_LAG_TICKS;
        for slot in 0..self.plan.nodes.len() {
            let Ok(s) = u16::try_from(slot) else { continue };
            self.rt.set_follower(s, follower_env(s, env_tick));
        }
        // automation lane が base を上書きする param を解決する (r.md #89 Q4)。
        for i in 0..self.plan.lane_params.len() {
            let (slot, param) = self.plan.lane_params[i];
            let plain = lane_base(song, &self.plan, slot, param, self.next_mark.beat);
            self.rt.set_base(slot, param, plain);
        }
        // r.md #89 Q9: 深さの automation lane も同じ刻みで解決する。
        for i in 0..self.plan.depth_groups.len() {
            if !self.plan.depth_groups[i].has_lane {
                continue;
            }
            let rid = self.plan.depth_groups[i].routing_id;
            let base = self.plan.depth_groups[i].base_depth;
            let plain = depth_lane_base(song, rid, base, self.next_mark.beat);
            self.rt.set_depth_base(&self.plan, rid, plain);
        }
        let mark = self.next_mark;
        common::mod_graph::tick(
            &self.plan,
            &mut self.rt,
            self.table.as_deref(),
            TickCtx {
                beat: mark.beat,
                secs: mark.secs,
                bpm: mark.bpm,
                dt_beats: dt * mark.bpm / 60.0,
                dt_secs: dt,
                tick_index: k,
            },
        );
        self.row.clear();
        for slot in 0..self.n_slots() {
            self.row
                .push(self.rt.value(u16::try_from(slot).unwrap_or(u16::MAX)));
        }
        // r.md #89 Q9: 深さも刻みごとに動く (深さを動かしていなければ空 = ゼロコスト)。
        self.depth_row.clear();
        for g in self.plan.depth_groups.iter().take(MAX_SLOTS) {
            self.depth_row
                .push(self.rt.depth_for(&self.plan, g.routing_id).unwrap_or(g.base_depth));
        }
        self.plane.push_row(&self.row, &self.depth_row);
        for i in 0..self.follower_cols.len() {
            let s = self.follower_cols[i];
            #[allow(clippy::cast_possible_truncation)]
            self.follower_eff.push(FollowerEff {
                attack_ms: self.rt.effective(s, ModParam::FollowerAttack) as f32,
                release_ms: self.rt.effective(s, ModParam::FollowerRelease) as f32,
                gain: self.rt.effective(s, ModParam::FollowerGain) as f32,
                hp_hz: self.rt.effective(s, ModParam::FollowerHpHz) as f32,
                lp_hz: self.rt.effective(s, ModParam::FollowerLpHz) as f32,
            });
        }
        self.marks.push(mark);
        self.next_mark = common::mod_graph::next_mark(song, &self.plan, &self.rt, mark, k, dt);
    }

    fn drop_leading_eff(&mut self, rows: usize) {
        let cols = self.follower_cols.len();
        if cols == 0 || rows == 0 {
            return;
        }
        let cut = (rows * cols).min(self.follower_eff.len());
        self.follower_eff.drain(..cut);
    }

    /// buffer を刻み境界で区間に割る (先頭は端数になりうる)。
    fn build_spans(&mut self, frames: u32, lead: u32) {
        self.spans.clear();
        let mut frame = 0u32;
        let mut row = 0usize;
        while frame < frames {
            let next = lead
                .saturating_add(row as u32 * MOD_TICK_FRAMES)
                .min(frames);
            self.spans.push(TickSpan {
                row,
                frame,
                frames: next - frame,
            });
            frame = next;
            row += 1;
        }
    }

    /// この buffer の刻みごとの値面 (描画経路へ渡す)。
    #[must_use]
    pub fn plane(&self) -> ModTickPlaneRef<'_> {
        self.plane.as_ref()
    }

    /// **まだ着地していない** (= `locate` を呼ばずに走らせてはいけない)。
    ///
    /// `install` 直後はこれが `true` で、`next_mark` が既定値 (bpm 0) のまま。
    /// 呼び出し側は最初の buffer で必ず [`Self::locate`] を通すこと。
    #[must_use]
    pub fn needs_locate(&self) -> bool {
        self.first_tick == i64::MIN
    }

    /// フォロワーを刻みごとに進めるための view。`col_of_slot` は engine が
    /// `Schedule::follower_keys` から解決した写像 (schedule slot → 列)。
    #[must_use]
    pub fn follower_drive<'a>(&'a self, col_of_slot: &'a [u16], first_sample: u64) -> FollowerDrive<'a> {
        FollowerDrive {
            spans: &self.spans,
            first_sample,
            col_of_slot,
            eff: &self.follower_eff,
            n_cols: self.follower_cols.len(),
        }
    }

    /// `Schedule::follower_keys` (schedule slot → `ModSource::id`) から
    /// 「plan slot → schedule の follower index」を作る。`u16::MAX` = 対応なし。
    ///
    /// `mod_graph::tick` は envelope follower の値を `ModRuntime::set_follower`
    /// 経由でしか受け取らない (slot 取り違えを型で防ぐ設計) ので、engine は毎刻み
    /// この写像で `Schedule::follower_slots[i].env` を引く。**毎刻み線形探索しない**
    /// ために plan / schedule の差し替え時に 1 度だけ作る。
    pub fn build_follower_env_map(&self, follower_keys: &[u32], out: &mut Vec<u16>) {
        out.clear();
        out.resize(self.plan.nodes.len().min(MAX_SLOTS), u16::MAX);
        for (i, id) in follower_keys.iter().enumerate() {
            if let Some(slot) = self.plan.slot_of(*id)
                && let Ok(idx) = u16::try_from(i)
                && let Some(cell) = out.get_mut(usize::from(slot))
            {
                *cell = idx;
            }
        }
    }

    /// `Schedule::follower_keys` (schedule slot → `ModSource::id`) から
    /// 「schedule slot → 係数表の列」を作る。**plan / schedule のどちらかが
    /// 変わったら engine が作り直す。** 変調されていないフォロワーは `u16::MAX`。
    pub fn build_follower_cols(&self, follower_keys: &[u32], out: &mut Vec<u16>) {
        out.clear();
        for id in follower_keys {
            let col = self
                .plan
                .slot_of(*id)
                .and_then(|s| self.follower_cols.iter().position(|c| *c == s))
                .and_then(|c| u16::try_from(c).ok())
                .unwrap_or(u16::MAX);
            out.push(col);
        }
    }

    /// GUI / sidecar へ出す 1 点の面 (buffer 頭の値)。
    pub fn publish_plane(&mut self) -> &ModPlane {
        self.publish.clear();
        let row = self.plane.as_ref().row(0);
        for (i, id) in self.plan.slot_ids.iter().enumerate().take(MAX_SLOTS) {
            self.publish
                .push(*id, row.values.get(i).copied().unwrap_or(0.0));
        }
        // r.md #89 Q9: 深さの実効値も一緒に出す — GUI の深さリング / 到達値表示が
        // 「動いている深さ」を見られないと、ラックの表示と音が食い違う。
        for (i, g) in self.plan.depth_groups.iter().enumerate().take(MAX_SLOTS) {
            self.publish.push_depth(
                g.routing_id,
                row.depths.get(i).copied().unwrap_or(g.base_depth),
            );
        }
        &self.publish
    }
}

/// フォロワーを**刻みごとに**進めるための `Copy` な view (schedule 走査へ渡す)。
///
/// `NodeOp::EnvelopeFollow` は buffer 全体を 1 回で舐めていたが、係数が変調される
/// ようになると刻みごとに引き直す必要がある。schedule 側は `Schedule` の slot 番号
/// しか持たないので、plan の slot への写像 (`col_of_slot`) をここで解決済みにして渡す。
#[derive(Debug, Clone, Copy, Default)]
pub struct FollowerDrive<'a> {
    /// buffer の区間割り。空なら「刻みに割らない」= 従来どおり 1 回で舐める。
    pub spans: &'a [TickSpan],
    /// この buffer の先頭の **絶対 song サンプル位置**。フォロワーが刻み境界の
    /// envelope を記録するのに要る (境界は絶対位置で決まる = buffer 非依存)。
    pub first_sample: u64,
    /// `Schedule` の follower slot → [`Self::eff`] の列。`u16::MAX` = 係数が
    /// 変調されていない (compile 時の値のまま)。
    pub col_of_slot: &'a [u16],
    /// 行 = 刻み、列 = [`Self::n_cols`]。
    pub eff: &'a [FollowerEff],
    pub n_cols: usize,
}

impl FollowerDrive<'_> {
    /// schedule slot `slot` の刻み `row` における実効係数。
    #[must_use]
    #[inline]
    pub fn eff_for(&self, slot: u32, row: usize) -> Option<FollowerEff> {
        let col = *self.col_of_slot.get(slot as usize)?;
        if col == u16::MAX {
            return None;
        }
        self.eff.get(row * self.n_cols + usize::from(col)).copied()
    }
}

/// 絶対サンプル位置が乗る刻み番号。
#[must_use]
#[inline]
pub fn tick_of(sample: u64) -> i64 {
    i64::try_from(sample / u64::from(MOD_TICK_FRAMES)).unwrap_or(i64::MAX)
}

/// 1 刻みの秒数。
#[must_use]
#[inline]
pub fn dt_secs(sample_rate: u32) -> f64 {
    f64::from(MOD_TICK_FRAMES) / f64::from(sample_rate.max(1))
}

/// `ModPlan::lane_params` の 1 件を automation lane から解決する。
///
/// 置き場は「そのソースの帰属トラック」— `MASTER_TRACK_ID` なら `song_lanes`、
/// それ以外はそのトラックの `automation_lanes`。`AutomationTarget` だけから
/// 置き場を決める全域関数は作らない (設計正本 §3.2)。
fn lane_base(song: &Song, plan: &ModPlan, slot: u16, param: ModParam, beat: f64) -> f64 {
    let Some(&source_id) = plan.slot_ids.get(usize::from(slot)) else {
        return 0.0;
    };
    let target = AutomationTarget::ModSourceParam { source_id, param };
    let lanes = match song.mod_source_owner(source_id) {
        Some(MASTER_TRACK_ID) => song.song_lanes.as_slice(),
        Some(track_id) => match song.tracks.iter().find(|t| t.id == track_id) {
            Some(t) => t.automation_lanes.as_slice(),
            None => return fallback_base(plan, slot, param),
        },
        None => return fallback_base(plan, slot, param),
    };
    match lanes.iter().find(|l| l.enabled && l.target == target) {
        Some(lane) => common::automation::lane_value_at(lane, &song.clip_contents, beat),
        None => fallback_base(plan, slot, param),
    }
}

/// `ModRoutingDepth` の automation lane を解決する (r.md #89 Q9)。
/// 置き場は **その変調が置かれている所** (`Song::mod_routing_owner`)。
fn depth_lane_base(song: &Song, routing_id: u32, base: f32, beat: f64) -> f32 {
    let target = AutomationTarget::ModRoutingDepth { routing_id };
    let lanes = match song.mod_routing_owner(routing_id) {
        Some(MASTER_TRACK_ID) => song.song_lanes.as_slice(),
        Some(track_id) => match song.tracks.iter().find(|t| t.id == track_id) {
            Some(t) => t.automation_lanes.as_slice(),
            None => return base,
        },
        None => return base,
    };
    match lanes.iter().find(|l| l.enabled && l.target == target) {
        #[allow(clippy::cast_possible_truncation)]
        Some(lane) => common::automation::lane_value_at(lane, &song.clip_contents, beat) as f32,
        None => base,
    }
}

/// lane が無い / 引けないときの base (plan が焼いた変調前の値)。
fn fallback_base(plan: &ModPlan, slot: u16, param: ModParam) -> f64 {
    plan.nodes
        .get(usize::from(slot))
        .map_or(0.0, |n| n.base[param.index()])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **live (可変 buffer 長) と書き出し (1024 固定) が同じ刻み列を踏む。**
    /// これが崩れると、同じ曲でも変調の段差の位置が再生と書き出しで違う
    /// (= 聴いた通りに書き出されない)。境界は buffer ではなく曲頭からの
    /// 絶対サンプル位置で決まる、というのがその担保。
    #[test]
    fn 刻み番号は_buffer_の切り方に依存しない() {
        // 刻み番号は **絶対サンプル位置**だけで決まる = buffer の切れ目に依存しない。
        // 「同じ写像を 2 通りに並べ替えて比べる」形だと `tick_of` が何であっても
        // 通ってしまうので、境界そのものを固定する。
        assert_eq!(MOD_TICK_FRAMES, 64, "刻み幅が変わったら下の期待値も変える");
        for (sample, want) in [(0u64, 0i64), (63, 0), (64, 1), (479, 7), (480, 7), (1023, 15)] {
            assert_eq!(tick_of(sample), want, "sample={sample}");
        }
        // buffer 480 で切っても 1024 で切っても、同じ絶対位置は同じ刻みに落ちる。
        assert_eq!(tick_of(480), tick_of(480), "境界は buffer 長に依存しない");
        assert_eq!(tick_of(1024), 16);
    }

    /// 変調の rate を変調した (= 積分 tier の) 曲で、**buffer の切り方を変えても
    /// 同じ song 位置で同じ値になる**。
    ///
    /// 設計正本 §8-6 の「3 経路が同じ値を返す」の engine 側。live は device の
    /// 実測 buffer 長 (可変)、書き出しは 1024 固定なので、刻みが buffer 相対だと
    /// ここが必ずずれる — 「聴いた通りに書き出される」の担保はこの 1 本。
    #[test]
    fn buffer_の切り方を変えても同じ位置で同じ値になる() {
        use common::model::{
            LfoConfig, LfoShape, ModRate, ModRouting, ModSource, ModSourceKind, Polarity,
            RetriggerMode, Track,
        };

        let lfo = |id: u32, hz: f32| ModSource {
            id,
            owner_track_id: 1,
            color: [0.0; 3],
            kind: ModSourceKind::Lfo(LfoConfig {
                shape: LfoShape::SawUp,
                rate: ModRate {
                    mode: common::model::ModRateMode::Free,
                    hz,
                    ..ModRate::default()
                },
                phase: 0.0,
                retrigger: RetriggerMode::FreeRun,
            }),
        };
        // 2 が 1 の「速さ」を変調する = 1 は積分 tier (閉形式では解けない)。
        let song = Song {
            tracks: vec![Track {
                id: 1,
                mod_routings: vec![ModRouting {
                    id: 1,
                    source_id: 2,
                    target: AutomationTarget::ModSourceParam {
                        source_id: 1,
                        param: ModParam::Rate,
                    },
                    depth: 0.3,
                    polarity: Polarity::Bipolar,
                }],
                ..Default::default()
            }],
            mod_sources: vec![lfo(1, 2.0), lfo(2, 0.25)],
            ..Default::default()
        };

        let sr = 48_000u32;
        let run = |chunks: &[u32]| -> Vec<(u64, f32)> {
            let plan = Arc::new(common::mod_graph::build_plan(&song, 1, |_| 0.0));
            let mut rt = ModRuntime::default();
            rt.install(&plan);
            let table = Arc::new(common::mod_graph::ModPhaseTable::build(&plan, &song, sr, 4.0));
            let mut r = ModTickRunner::new();
            r.install(plan, rt);
            r.set_table(Some(table));
            r.locate(&song, 0, 0.0, sr);
            let mut out = Vec::new();
            let mut at = 0u64;
            for &n in chunks {
                r.run_buffer(&song, at, n, sr, |_, _| 0.0);
                // buffer 内の各 frame の値を絶対サンプル位置つきで記録する。
                let plane = r.plane();
                for f in 0..n {
                    out.push((at + u64::from(f), plane.scalar_at_frame(1, f)));
                }
                at += u64::from(n);
            }
            out
        };

        // 書き出し (1024 固定) と live (可変長) が同じ区間を描く。
        let export = run(&[1024, 1024, 1024]);
        let live = run(&[480, 544, 512, 1024, 512]);
        assert_eq!(export.len(), live.len());
        for (a, b) in export.iter().zip(live.iter()) {
            assert_eq!(a.0, b.0, "同じサンプル位置を比べている");
            assert_eq!(
                a.1, b.1,
                "sample {} で値が違う (export={} live={})",
                a.0, a.1, b.1
            );
        }
        // 全部 0 の自明一致ではない (LFO が実際に動いている)。
        let lo = export.iter().fold(f32::MAX, |a, (_, v)| a.min(*v));
        let hi = export.iter().fold(f32::MIN, |a, (_, v)| a.max(*v));
        assert!(hi - lo > 1e-3, "LFO が動いていない: lo={lo} hi={hi}");
        // **rate が変調されている**ことの確認 — 未変調なら 2Hz なので 3072 サンプル
        // (64ms) で 0.128 まで進むはず。変調で実効 Hz が下がっているので届かない。
        assert!(hi < 0.1, "rate 変調が効いていない (未変調の閉形式のまま): hi={hi}");
    }

    /// 区間割りは buffer 頭が刻みの途中でも、以降の境界が絶対位置に揃う。
    #[test]
    fn 区間割りは先頭だけ端数になる() {
        let mut r = ModTickRunner::new();
        // playhead % 64 == 20 相当 (lead = 44)。
        r.build_spans(200, 44);
        let got: Vec<(usize, u32, u32)> =
            r.spans.iter().map(|s| (s.row, s.frame, s.frames)).collect();
        assert_eq!(got, vec![(0, 0, 44), (1, 44, 64), (2, 108, 64), (3, 172, 28)]);
        // 境界に乗っている buffer は素直に 64 刻み。
        r.build_spans(128, 64);
        let got: Vec<(usize, u32, u32)> =
            r.spans.iter().map(|s| (s.row, s.frame, s.frames)).collect();
        assert_eq!(got, vec![(0, 0, 64), (1, 64, 64)]);
    }
}
