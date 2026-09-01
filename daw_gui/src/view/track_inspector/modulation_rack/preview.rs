//! クロス変調中のプレビュー窓 (r.md #89 Q8)。
//!
//! 変調を受けていないソースは 1 周期を固定表示すれば足りる (周期的なので、 カーソル
//! 位置の値が常に実値と一致する)。 **速さや幅が変調されているソースは周期的でない**
//! ので、 固定窓は「今鳴っている形」 と違うものを描いてしまう。 そこで Random と同じ
//! 形 — 再生位置中心の時間窓をスクロールさせる — に倒し、 変調前の形を薄く重ねて
//! 「どれだけ振られているか」 を見せる。

use common::mod_graph::{MOD_TICK_FRAMES, ModPlan, ModRuntime, TickCtx};
use common::model::Song;
use common::modulators::ModTime;

use crate::app::AppData;

/// プレビューの 1 系列 (x は canvas 内の比 `0..=1`、 y は unipolar 出力 `0..=1`)。
pub(super) type Series = Vec<(f32, f32)>;

/// 窓のサンプル点数 (canvas の横解像度と揃える)。
pub(super) const PREVIEW_POINTS: usize = 160;

/// 窓に収める base rate の周期数。 これより速い / 遅いソースは下の上下限で挟む。
const WINDOW_CYCLES: f64 = 4.0;
/// 窓の刻み数の上限 / 下限。 上限は 1 フレームあたりの計算量の頭打ち
/// (≒2.7 秒 @48k)、 下限は 160 点のサンプルが 1 刻みを割らないため。
const MAX_TICKS: i64 = 2048;
const MIN_TICKS: i64 = PREVIEW_POINTS as i64;

/// 窓の幾何 (刻み幅 / 刻み数 / 先頭刻み)。 **前景系列とカーソルが同じ 1 本を引く** —
/// 別々に出すと、 片方の式だけ直したときにカーソルが波形からずれる。
#[derive(Clone, Copy)]
struct Window {
    dt_secs: f64,
    ticks: i64,
    start_tick: i64,
    /// 再生位置が乗る刻み (窓の中心。 曲頭近くでは左端に寄る)。
    center_tick: i64,
}

/// 再生位置 `secs` を中心にした窓を出す。 窓長は base rate の [`WINDOW_CYCLES`] 周期を
/// 目安に [`MIN_TICKS`]..=[`MAX_TICKS`] で挟む (遅すぎるソースで 1 フレームの計算量が
/// 膨らまないように、 速すぎるソースで 160 点が 1 刻みを割らないように)。
/// 曲頭近くでは左端が 0 に張り付くので、 再生位置は窓の中央とは限らない。
fn window_of(app: &AppData, node: &common::mod_graph::ModNode, secs: f64) -> Option<Window> {
    let song = app.song_doc.song();
    let sr = app.ipc.sample_rate.max(1);
    let dt_secs = f64::from(MOD_TICK_FRAMES) / f64::from(sr);
    if dt_secs <= 0.0 {
        return None;
    }
    let base_hz = node.rate.map_or(1.0, |r| r.base_hz(f64::from(song.bpm.max(1.0))));
    let want = if base_hz > 0.0 { WINDOW_CYCLES / base_hz / dt_secs } else { 0.0 };
    #[allow(clippy::cast_possible_truncation)]
    let ticks = (want as i64).clamp(MIN_TICKS, MAX_TICKS);
    #[allow(clippy::cast_possible_truncation)]
    let center_tick = (secs / dt_secs) as i64;
    Some(Window {
        dt_secs,
        ticks,
        start_tick: (center_tick - ticks / 2).max(0),
        center_tick,
    })
}

/// 窓の先頭の song 拍。 **再生位置の拍から刻みぶんだけ後退積分**して出す
/// (`samples_to_beats` は O(再生位置) の積分なので、 毎フレーム呼ぶと再生位置が
/// 進むほど描画が重くなる。 後退積分は窓の刻み数ぶんで、 走行そのものと同じ order)。
/// テンポは各刻みでその拍のテンポを引くので、 前進積分と同じ写像を辿る。
fn beat_at_window_start(song: &Song, playhead_beat: f64, back_ticks: i64, dt_secs: f64) -> f64 {
    let mut beat = playhead_beat;
    for _ in 0..back_ticks {
        if beat <= 0.0 {
            return 0.0;
        }
        let bpm = f64::from(common::automation::evaluate_song_tempo(song, beat));
        beat -= bpm * dt_secs / 60.0;
    }
    beat.max(0.0)
}

/// 窓を 1 刻みずつ走らせ、 各刻みで `visit(k, &rt, beat, secs)` を呼ぶ。
///
/// **評価は `mod_graph::tick` そのもの**を回す — プレビュー用に別の式を書かない
/// (描画 == 評価の SSoT。 別式にすると「絵は動くのに音は動かない」 が起きる)。
///
/// GUI は engine の状態を持たないので近似が 2 つ入る。 どちらも窓の中の**形と速さ**
/// には効かず、 絶対位相だけに効く:
/// - フォロワーの env は最後に publish された値で固定する (窓の中で音は動かない)。
/// - `ModPhaseTable` は engine 側にしか無いので、 窓の先頭は閉形式でシードされる
///   (`ModTier::Integrated` の絶対位相はわずかにずれうる)。
///   engine が実位相を publish したら、 シードをそれに差し替える。
fn walk_window(
    app: &AppData,
    plan: &ModPlan,
    w: Window,
    playhead_beat: f64,
    mut visit: impl FnMut(i64, &ModRuntime, f64, f64),
) {
    let song = app.song_doc.song();
    // フォロワーは窓の間ずっと最後の値で止める (音は先読みできない)。
    let follower: Vec<f32> = plan
        .slot_ids
        .iter()
        .map(|id| common::automation::source_scalar(song, &app.transport.mod_scalars, *id))
        .collect();
    let mut rt = ModRuntime::default();
    rt.install(plan);
    #[allow(clippy::cast_precision_loss)]
    let start_secs = w.start_tick as f64 * w.dt_secs;
    let mut beat = beat_at_window_start(song, playhead_beat, w.center_tick - w.start_tick, w.dt_secs);
    for k in 0..=w.ticks {
        #[allow(clippy::cast_precision_loss)]
        let secs = start_secs + k as f64 * w.dt_secs;
        // engine と同じく bpm はその時点のテンポカーブから引いて積分する。
        let bpm = f64::from(common::automation::evaluate_song_tempo(song, beat));
        let dt_beats = bpm * w.dt_secs / 60.0;
        common::mod_graph::tick(
            plan,
            &mut rt,
            &follower,
            None,
            TickCtx { beat, secs, bpm, dt_beats, dt_secs: w.dt_secs, tick_index: w.start_tick + k },
        );
        visit(k, &rt, beat, secs);
        beat += dt_beats;
    }
}

/// 再生位置中心の窓で `(変調後, 変調前)` の 2 系列をサンプルする。
/// 前景は `mod_graph::tick` の出力、 薄く重ねる基準は同じ transport 位置での
/// `generator_scalar` (= 変調前)。
///
/// 点は **必ず `PREVIEW_POINTS + 1` 個**、 `x` は `0.0` で始まり `1.0` で終わる
/// (刻み側を `i * ticks / PREVIEW_POINTS` で引く。 剰余で間引くと窓長次第で
/// 161..320 点に振れ、 末尾が 1.0 に届かなかった)。
pub(super) fn cross_mod_window(
    app: &AppData,
    plan: &ModPlan,
    sid: u32,
    beat: f64,
    secs: f64,
) -> (Series, Series) {
    let Some(slot) = plan.slot_of(sid) else {
        return (Series::new(), Series::new());
    };
    let Some(node) = plan.nodes.get(usize::from(slot)) else {
        return (Series::new(), Series::new());
    };
    let Some(w) = window_of(app, node, secs) else {
        return (Series::new(), Series::new());
    };
    let mut out = Series::with_capacity(PREVIEW_POINTS + 1);
    let mut ghost = Series::with_capacity(PREVIEW_POINTS + 1);
    // 次に採る刻み (単調増加。 `ticks >= PREVIEW_POINTS` なので取りこぼさない)。
    let mut next_point = 0usize;
    walk_window(app, plan, w, beat, |k, rt, b, s| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        while next_point <= PREVIEW_POINTS
            && k == (next_point as i64 * w.ticks) / PREVIEW_POINTS as i64
        {
            #[allow(clippy::cast_precision_loss)]
            let x = next_point as f32 / PREVIEW_POINTS as f32;
            out.push((x, rt.value(slot)));
            let plain = common::modulators::generator_scalar(
                &node.kind,
                ModTime { beat: b, secs: s, anchor_secs: node.anchor_secs },
            )
            .unwrap_or(0.0);
            ghost.push((x, plain));
            next_point += 1;
        }
    });
    (out, ghost)
}

/// クロス変調中のソースの、 **再生位置における周期位置** (積分位相)。
///
/// MSEG のカーブエディタと Steps のグリッドは「形を編集する面」 なので時間窓へは
/// 倒せない (点をドラッグする座標が動いてしまう)。 一方で速さが変調されている間、
/// 閉形式の `cycle_pos` は engine の積分位相と食い違う — カーソルと点灯段だけが
/// 嘘になる。 そこで窓と **同じ刻み・同じ順序・同じ `tick`** で再生位置まで進めて、
/// engine が持っているのと同じ位相を読む (別式を書かない)。
pub(super) fn cross_mod_phase(app: &AppData, plan: &ModPlan, sid: u32, beat: f64, secs: f64) -> Option<f64> {
    let slot = plan.slot_of(sid)?;
    let node = plan.nodes.get(usize::from(slot))?;
    let w = window_of(app, node, secs)?;
    let mut phase = None;
    let target = w.center_tick - w.start_tick;
    walk_window(app, plan, w, beat, |k, rt, _, _| {
        if k == target {
            phase = Some(rt.phase(slot));
        }
    });
    phase
}

/// 窓の中で再生位置が乗る比 (0..=1)。 窓は再生位置中心だが、 曲頭近くでは左端が 0 に
/// 張り付くので中央とは限らない。 [`cross_mod_window`] と **同じ式**で出す。
pub(super) fn cross_mod_cursor(app: &AppData, plan: &ModPlan, sid: u32, secs: f64) -> Option<f32> {
    let slot = plan.slot_of(sid)?;
    let node = plan.nodes.get(usize::from(slot))?;
    let w = window_of(app, node, secs)?;
    #[allow(clippy::cast_precision_loss)]
    let t = (w.center_tick - w.start_tick) as f32 / w.ticks as f32;
    Some(t.clamp(0.0, 1.0))
}
