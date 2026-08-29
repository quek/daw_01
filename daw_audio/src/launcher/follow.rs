//! フォローアクション (`docs/plan_rmd_87_clip_launcher.md` §2.3)。
//!
//! Live 12 相当の 10 種 + 確率 A/B + Linked(倍率) / Unlinked(時間)。
//!
//! # 決定論
//!
//! 抽選と `Any` / `Other` の乱数は **`f(seed, 発火拍)` の純ハッシュ**
//! ([`common::modulators::random_unit`]) で、走行中の状態を一切持たない。
//! これは Q9 (「同じプロジェクトなら何度書き出しても同じファイル」) の前提で、
//! 遷移先を `Song` に書き戻さない (§1.4) 以上、再現性はこの純粋性だけが担保する。
//! **`Instant::now` / 走行カウンタ / dispatch 順に依存させないこと** — worker pool は
//! work-stealing なのでトラックの処理順は実行ごとに変わる。
//!
//! # グループ
//!
//! `Next` / `Previous` / `First` / `Last` / `Any` / `Other` が指す範囲は
//! **同じ行の中で空セルに区切られた連続した塊** (Q13)。判定は
//! [`common::model::launch_group`] が SSoT で、ここでは再実装しない。

use common::model::{FollowAction, FollowActionKind, Scene, launch_group};
use common::modulators::random_unit;

/// フォローアクションの結果。列は `Song.scenes` の表示順 index で返す
/// (呼び側が「その列にこの行のセルがあるか」を見て、無ければ停止に倒す = Q11)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowOutcome {
    /// 何もしない — そのまま鳴り続ける。
    Keep,
    /// 停止する (ランチャーは握ったまま無音)。
    Stop,
    /// この列のセルへ移る。`PlayAgain` は「今と同じ列」として返る。
    Go(usize),
}

/// 抽選と選択で別の乱数を引くための salt。同じ `(seed, step)` から 2 つ以上の
/// 独立な値が要るときは salt を変える (同じ値を使い回すと A/B の抽選結果と
/// `Any` の選択が相関する)。
const SALT_LOTTERY: u64 = 0x9E37_79B9_7F4A_7C15;
const SALT_PICK: u64 = 0xC2B2_AE3D_27D4_EB4F;

/// 発火拍を乱数の `step` に落とす分解能 (1 拍 = 960 tick、MIDI の慣習と同じ)。
/// **`f64` の拍をそのまま `i64` へ流さない** — 拍の累算誤差で同じ発火が
/// 別の step になり、書き出しの再現性が壊れる。
const TICKS_PER_BEAT: f64 = 960.0;

/// 行 + セル + 発火拍から決まる決定論 seed の `step`。
#[must_use]
pub fn beat_step(fire_beat: f64) -> i64 {
    let t = (fire_beat * TICKS_PER_BEAT).round();
    if t.is_finite() { t as i64 } else { 0 }
}

/// 行 (`row_key.packed()`) とセル (`clip_id`) から決まる seed。
#[must_use]
pub fn row_seed(row_key: u64, clip_id: u32) -> u64 {
    row_key.rotate_left(17) ^ (u64::from(clip_id) << 3)
}

/// フォローアクションを 1 回解決する。
///
/// - `occupied[i]` = 「表示順 `i` の列にこの行のセルがあるか」
/// - `from` = 今鳴っているセルの列 index
/// - `scenes` = `Song.scenes` (`Jump { scene_id }` の解決にだけ使う)
///
/// RT 安全: 確保もロックも無い純関数 (`scenes` の線形走査のみ)。
#[must_use]
pub fn resolve(
    action: &FollowAction,
    occupied: &[bool],
    from: usize,
    scenes: &[Scene],
    seed: u64,
    fire_beat: f64,
) -> FollowOutcome {
    if !action.enabled {
        return FollowOutcome::Keep;
    }
    let step = beat_step(fire_beat);
    // `chance_a` % で a、残りで b。100 なら常に a、0 なら常に b。
    let chance = action.chance_a.min(100);
    // 端は抽選しない — `random_unit` は `[0,1)` だが f32 化で 1.0 に丸まりうるので、
    // 「100% なのに稀に b」「0% なのに稀に a」が起きる (UI の表示と食い違う)。
    if chance == 100 {
        return apply(action.a, occupied, from, scenes, seed, step);
    }
    if chance == 0 {
        return apply(action.b, occupied, from, scenes, seed, step);
    }
    let roll = random_unit(seed ^ SALT_LOTTERY, step);
    let kind = if roll * 100.0 < f32::from(chance) {
        action.a
    } else {
        action.b
    };
    apply(kind, occupied, from, scenes, seed, step)
}

/// 選ばれた 1 種を列 index へ落とす。
fn apply(
    kind: FollowActionKind,
    occupied: &[bool],
    from: usize,
    scenes: &[Scene],
    seed: u64,
    step: i64,
) -> FollowOutcome {
    match kind {
        FollowActionKind::NoAction => FollowOutcome::Keep,
        FollowActionKind::Stop => FollowOutcome::Stop,
        FollowActionKind::PlayAgain => FollowOutcome::Go(from),
        FollowActionKind::Jump { scene_id } => scenes
            .iter()
            .position(|s| s.id == scene_id)
            .map_or(FollowOutcome::Keep, FollowOutcome::Go),
        // 以下は「空セルで区切られた塊」の中で解く (Q13)。塊が引けない
        // (= 今のセルが空、通常あり得ない) なら何もしない。
        other => match launch_group(occupied, from) {
            Some((start, end)) => in_group(other, start, end, from, seed, step),
            None => FollowOutcome::Keep,
        },
    }
}

/// 塊 `[start, end)` の中での解決。`Next` は末尾で先頭へ巡回する。
fn in_group(
    kind: FollowActionKind,
    start: usize,
    end: usize,
    from: usize,
    seed: u64,
    step: i64,
) -> FollowOutcome {
    let len = end - start;
    let idx = match kind {
        FollowActionKind::Previous => {
            if from == start { end - 1 } else { from - 1 }
        }
        FollowActionKind::Next => {
            if from + 1 >= end { start } else { from + 1 }
        }
        FollowActionKind::First => start,
        FollowActionKind::Last => end - 1,
        FollowActionKind::Any => start + pick(len, seed, step),
        FollowActionKind::Other => {
            if len <= 1 {
                // 塊が 1 つしか無いなら「他」は存在しない = 同じセルを撃ち直す。
                from
            } else {
                // 自分を除いた len-1 個から選び、自分以降を 1 つずらす。
                let k = start + pick(len - 1, seed, step);
                if k >= from { k + 1 } else { k }
            }
        }
        // ここへは来ない (呼び側が先に潰している)。
        _ => return FollowOutcome::Keep,
    };
    FollowOutcome::Go(idx)
}

/// `[0, len)` から 1 つ決定論的に選ぶ。
fn pick(len: usize, seed: u64, step: i64) -> usize {
    if len <= 1 {
        return 0;
    }
    let r = f64::from(random_unit(seed ^ SALT_PICK, step)).clamp(0.0, 1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let k = (r * len as f64) as usize;
    k.min(len - 1)
}

/// このセルのフォローアクションが次に発火する song 拍。`None` = 発火しない。
///
/// - Linked  : `multiplier` 回ループした後 (= セル終端)
/// - Unlinked: `time_beats` ごと
#[must_use]
pub fn due_beat(action: &FollowAction, launch_beat: f64, loop_len: f64) -> Option<f64> {
    if !action.enabled {
        return None;
    }
    let span = if action.linked {
        loop_len * f64::from(action.multiplier.max(1))
    } else {
        action.time_beats
    };
    (span.is_finite() && span > 0.0).then_some(launch_beat + span)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenes(n: usize) -> Vec<Scene> {
        #[allow(clippy::cast_possible_truncation)]
        (0..n).map(|i| Scene::new(i as u32 + 1)).collect()
    }

    fn act(a: FollowActionKind) -> FollowAction {
        FollowAction { enabled: true, a, ..FollowAction::default() }
    }

    /// 5 列中 0..3 が埋まった 1 行 (3 は空、4 は埋まっている = 別の塊)。
    const OCC: [bool; 5] = [true, true, true, false, true];

    #[test]
    fn 十種の遷移表() {
        let sc = scenes(5);
        // (kind, from, expected)
        let cases: &[(FollowActionKind, usize, FollowOutcome)] = &[
            (FollowActionKind::NoAction, 1, FollowOutcome::Keep),
            (FollowActionKind::Stop, 1, FollowOutcome::Stop),
            (FollowActionKind::PlayAgain, 1, FollowOutcome::Go(1)),
            (FollowActionKind::Next, 0, FollowOutcome::Go(1)),
            // 塊の末尾 (2) から Next は塊の先頭 (0) へ巡回する — 空セル 3 は跨がない。
            (FollowActionKind::Next, 2, FollowOutcome::Go(0)),
            (FollowActionKind::Previous, 1, FollowOutcome::Go(0)),
            // 塊の先頭から Previous は末尾へ巡回。
            (FollowActionKind::Previous, 0, FollowOutcome::Go(2)),
            (FollowActionKind::First, 2, FollowOutcome::Go(0)),
            (FollowActionKind::Last, 0, FollowOutcome::Go(2)),
            // 4 は 1 つだけの塊 → どの相対指定も自分自身。
            (FollowActionKind::Next, 4, FollowOutcome::Go(4)),
            (FollowActionKind::Previous, 4, FollowOutcome::Go(4)),
            (FollowActionKind::First, 4, FollowOutcome::Go(4)),
            (FollowActionKind::Last, 4, FollowOutcome::Go(4)),
            (FollowActionKind::Other, 4, FollowOutcome::Go(4)),
            // Jump は列 id で指す (表示順ではない)。
            (FollowActionKind::Jump { scene_id: 5 }, 0, FollowOutcome::Go(4)),
            // 実在しない列への Jump は何もしない。
            (FollowActionKind::Jump { scene_id: 99 }, 0, FollowOutcome::Keep),
        ];
        for (kind, from, want) in cases {
            let got = resolve(&act(*kind), &OCC, *from, &sc, 12345, 8.0);
            assert_eq!(got, *want, "kind={kind:?} from={from}");
        }
    }

    #[test]
    fn 無効なフォローアクションは何もしない() {
        let sc = scenes(5);
        let a = FollowAction { a: FollowActionKind::Stop, ..FollowAction::default() };
        assert!(!a.enabled);
        assert_eq!(resolve(&a, &OCC, 1, &sc, 1, 4.0), FollowOutcome::Keep);
    }

    #[test]
    fn 確率_0_と_100_は必ず片側を選ぶ() {
        let sc = scenes(5);
        let a = FollowAction {
            enabled: true,
            a: FollowActionKind::Stop,
            b: FollowActionKind::PlayAgain,
            chance_a: 100,
            ..FollowAction::default()
        };
        let b = FollowAction { chance_a: 0, ..a.clone() };
        // 拍を動かしても (= 乱数が変わっても) 選ばれる側は変わらない。
        for i in 0..64 {
            let beat = f64::from(i) * 0.37;
            assert_eq!(resolve(&a, &OCC, 1, &sc, 7, beat), FollowOutcome::Stop);
            assert_eq!(resolve(&b, &OCC, 1, &sc, 7, beat), FollowOutcome::Go(1));
        }
    }

    #[test]
    fn 抽選は同じ状態から同じ結果を返す() {
        let sc = scenes(5);
        let a = FollowAction {
            enabled: true,
            a: FollowActionKind::Stop,
            b: FollowActionKind::Any,
            chance_a: 50,
            ..FollowAction::default()
        };
        // 同じ (seed, 拍) → 必ず同じ結果 (書き出しを 2 回やって一致する条件)。
        let mut seen_stop = 0;
        let mut seen_go = 0;
        for i in 0..200 {
            let beat = f64::from(i) * 1.25;
            let first = resolve(&a, &OCC, 1, &sc, 42, beat);
            assert_eq!(first, resolve(&a, &OCC, 1, &sc, 42, beat), "beat={beat}");
            match first {
                FollowOutcome::Stop => seen_stop += 1,
                FollowOutcome::Go(i) => {
                    assert!((0..=2).contains(&i), "Any は塊の外へ出た: {i}");
                    seen_go += 1;
                }
                FollowOutcome::Keep => panic!("50% の抽選で Keep は出ない"),
            }
        }
        // 50% なのでどちらも出る (= 片側に張り付いていない)。
        assert!(seen_stop > 40 && seen_go > 40, "偏りすぎ: stop={seen_stop} go={seen_go}");
        // seed が違えば別の並びになる。
        let other: Vec<_> =
            (0..200).map(|i| resolve(&a, &OCC, 1, &sc, 43, f64::from(i) * 1.25)).collect();
        let base: Vec<_> =
            (0..200).map(|i| resolve(&a, &OCC, 1, &sc, 42, f64::from(i) * 1.25)).collect();
        assert_ne!(other, base, "seed を変えても同じ並び = seed が効いていない");
    }

    #[test]
    fn other_は直前と同じセルを選ばない() {
        let sc = scenes(5);
        let a = act(FollowActionKind::Other);
        for from in 0..3 {
            for i in 0..200 {
                let got = resolve(&a, &OCC, from, &sc, 9, f64::from(i) * 0.5);
                let FollowOutcome::Go(idx) = got else {
                    panic!("Other が Go を返さない: {got:?}");
                };
                assert_ne!(idx, from, "Other が同じセルを選んだ");
                assert!(idx < 3, "Other が塊の外 ({idx}) を選んだ");
            }
        }
    }

    #[test]
    fn 発火拍は倍率と時間で決まる() {
        let linked = FollowAction { enabled: true, multiplier: 3, ..FollowAction::default() };
        // 拍 8 で撃った 4 拍のセルを 3 周 → 拍 20。
        assert_eq!(due_beat(&linked, 8.0, 4.0), Some(20.0));

        let unlinked = FollowAction {
            enabled: true,
            linked: false,
            time_beats: 1.5,
            ..FollowAction::default()
        };
        assert_eq!(due_beat(&unlinked, 8.0, 4.0), Some(9.5));

        // 無効 / 退化した値は発火しない (0 拍ごとの発火で無限ループしない)。
        assert_eq!(due_beat(&FollowAction::default(), 8.0, 4.0), None);
        let zero = FollowAction { enabled: true, linked: false, time_beats: 0.0, ..unlinked };
        assert_eq!(due_beat(&zero, 8.0, 4.0), None);
        assert_eq!(due_beat(&linked, 8.0, 0.0), None);
    }
}
