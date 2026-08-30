//! 「content への窓」を持つクリップの共通操作 — **非重なり不変条件の唯一の実装**。
//!
//! アレンジのクリップ (`Clip`) とオートメーションクリップ (`AutomationClip`) は
//! `start_beat` / `length_beats` / `content_offset_beats` / `id` という同じ窓を持つ。
//! 上書き規則 (`docs/plan_range_selection.md` §6.2) を 2 か所に書くと片方だけ直って
//! 静かに食い違うので、ここ 1 本に集約する。

/// content への窓を持つクリップ。
pub trait ClipWindow: Clone {
    fn window_id(&self) -> u32;
    fn set_window_id(&mut self, id: u32);
    fn window_start(&self) -> f64;
    fn set_window_start(&mut self, v: f64);
    fn window_len(&self) -> f64;
    fn set_window_len(&mut self, v: f64);
    fn window_offset(&self) -> f64;
    fn set_window_offset(&mut self, v: f64);
}

/// 拍の同一視イプシロン。これ以下の長さになった断片は落とす。
const EPS: f64 = 1e-9;

/// `[start, end)` に掛かるクリップを**上書き規則**で削り取る。
///
/// | 状況 | 結果 |
/// |---|---|
/// | 完全に覆われる | 削除 |
/// | 端が食い込まれる | その分だけ縮む (trim) |
/// | 真ん中を覆われる | 2 つに分割され、覆われた部分が消える |
///
/// trim / 分割は **content を一切触らない** — 窓だけを動かす。 分割後の 2 断片は
/// 同じ content を共有した 2 つの窓になるので、linked clip の関係も壊れない。
///
/// `except_id` はその id を対象から外す (自分自身を削らないため)。
/// `alloc_id` は分割で生じる右断片に振る新しい id を返す。
/// 返り値は 1 つでも変更したか。
pub fn carve_range<T: ClipWindow>(
    clips: &mut Vec<T>,
    start: f64,
    end: f64,
    except_id: Option<u32>,
    mut alloc_id: impl FnMut() -> u32,
) -> bool {
    if end <= start + EPS {
        return false;
    }
    let mut changed = false;
    let mut split_right: Vec<T> = Vec::new();
    clips.retain_mut(|c| {
        if Some(c.window_id()) == except_id {
            return true;
        }
        let c0 = c.window_start();
        let c1 = c0 + c.window_len();
        if c1 <= start + EPS || c0 >= end - EPS {
            return true; // 交差しない
        }
        changed = true;
        let keep_left = c0 < start - EPS;
        let keep_right = c1 > end + EPS;
        match (keep_left, keep_right) {
            (false, false) => false, // 完全被覆
            (true, false) => {
                c.set_window_len(start - c0); // 右側を削る (content 不変)
                true
            }
            (false, true) => {
                // 左側を削る = 窓を右へ進める。
                let delta = end - c0;
                c.set_window_offset(c.window_offset() + delta);
                c.set_window_start(end);
                c.set_window_len(c1 - end);
                true
            }
            (true, true) => {
                // 真ん中を抜く。 自分は左断片になり、右断片を後で足す。
                let mut right = c.clone();
                let delta = end - c0;
                right.set_window_offset(right.window_offset() + delta);
                right.set_window_start(end);
                right.set_window_len(c1 - end);
                split_right.push(right);
                c.set_window_len(start - c0);
                true
            }
        }
    });
    for mut right in split_right {
        let id = alloc_id();
        right.set_window_id(id);
        clips.push(right);
    }
    changed
}

/// 既存の重なりを解消する (読み込み時の移行)。
///
/// **配列順で後ろにあるもの (= 描画で前面に来ているもの) が勝つ。**
/// **冪等** — 2 回目以降は重なりが無いので `false` を返す。
pub fn resolve_overlaps<T: ClipWindow>(
    clips: &mut Vec<T>,
    mut alloc_id: impl FnMut() -> u32,
) -> bool {
    let mut changed = false;
    let src = std::mem::take(clips);
    for clip in src {
        if clip.window_len() <= EPS {
            changed = true;
            continue;
        }
        let start = clip.window_start();
        let end = start + clip.window_len();
        if carve_range(clips, start, end, None, &mut alloc_id) {
            changed = true;
        }
        clips.push(clip);
    }
    changed
}
