//! ドラッグ端オートスクロール (edge auto-scroll) の純粋ロジック。
//!
//! arrangement / piano_roll の drag 中、ポインタが表示領域の端 hot-zone に入ったとき、その frame で
//! view が動くべき content px を軸ごとに返す。速度は zone 内の近接度に線形比例 (zone 境界で 0、端で
//! `max_speed_px`)。zone を越えて領域外に出ても `max_speed_px` で頭打ち。
//!
//! widget 側はこの関数の戻り値を使って scroll を emit し、相対 delta で対象位置を決める drag は
//! 同 px ぶん anchor を逆方向に shift して「掴んでいる対象がカーソルに追従」 を実現する
//! (= tldraw / 実 DAW 共通の「delta は content space」)。絶対 px→beat で毎フレーム再解決する drag
//! (ruler の loop/playhead) や、live な行 top から再解決する drag (track 並べ替え) は anchor shift
//! 不要で自動追従する。

use daw_ui_renderer::Rect;

/// edge auto-scroll を発火させる「ドラッグ開始 (press) からの最小移動量」(px)。これ未満は
/// click-and-hold (= 単なるクリック) とみなしスクロールしない。実 DAW は「ドラッグして初めて」
/// 端スクロールする (静止クリックでは動かない) ので、端近くの clip / note をクリックしただけで
/// view が飛ぶのを防ぐ。drag の short-click 化閾値 (4px) と同値。
pub(crate) const ACTIVATE_PX: f32 = 4.0;

/// 端オートスクロールの設定。
#[derive(Clone, Copy, Debug)]
pub(crate) struct EdgeScrollCfg {
    /// 端からの hot-zone 幅 (px)。ポインタがこの帯に入ると発火する。
    pub zone_px: f32,
    /// 端 (zone 最外) での 1 frame あたり content スクロール量 (px)。zone 境界では 0。
    pub max_speed_px: f32,
}

impl Default for EdgeScrollCfg {
    fn default() -> Self {
        Self { zone_px: 28.0, max_speed_px: 18.0 }
    }
}

/// 1 軸ぶんの端スクロール量を返す。`lo` = rect 左/上端、`hi` = rect 右/下端。
/// 戻り `< 0` で lo 方向 (view を手前へ)、`> 0` で hi 方向 (view を奥へ)。zone 外は 0。
fn axis_delta(p: f32, lo: f32, hi: f32, cfg: EdgeScrollCfg) -> f32 {
    // 領域幅の半分で zone を頭打ち (狭パネルで両端 zone が重なり中央が一方向へ張り付くのを防ぐ +
    // div-by-zero 回避)。下限 1px で常に > 0。
    let zone = cfg.zone_px.clamp(1.0, ((hi - lo) * 0.5).max(1.0));
    let near_lo = lo + zone;
    let near_hi = hi - zone;
    if p < near_lo {
        // lo 端に近いほど速い (p == lo で 1.0、p == near_lo で 0)。zone より外は 1.0 で頭打ち。
        -((near_lo - p) / zone).clamp(0.0, 1.0) * cfg.max_speed_px
    } else if p > near_hi {
        ((p - near_hi) / zone).clamp(0.0, 1.0) * cfg.max_speed_px
    } else {
        0.0
    }
}

/// ポインタ `pos` が `rect` の端 hot-zone に入っているとき、その frame で view が動くべき content px を
/// 軸ごとに返す。`+dx` = 右端側 (横 view を奥 = 後ろの拍へ)、`+dy` = 下端側 (縦下方向)。
/// `pos == None` / zone 外 / 軸 disable は 0。
pub(crate) fn edge_scroll_delta(
    pos: Option<(f32, f32)>,
    rect: Rect,
    cfg: EdgeScrollCfg,
    axis_x: bool,
    axis_y: bool,
) -> (f32, f32) {
    let Some((px, py)) = pos else {
        return (0.0, 0.0);
    };
    let dx = if axis_x { axis_delta(px, rect.x, rect.x + rect.w, cfg) } else { 0.0 };
    let dy = if axis_y { axis_delta(py, rect.y, rect.y + rect.h, cfg) } else { 0.0 };
    (dx, dy)
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect { x: 100.0, y: 50.0, w: 400.0, h: 300.0 }
    }

    fn cfg() -> EdgeScrollCfg {
        EdgeScrollCfg { zone_px: 20.0, max_speed_px: 10.0 }
    }

    #[test]
    fn 中央_は_両軸ゼロ() {
        let (dx, dy) = edge_scroll_delta(Some((300.0, 200.0)), rect(), cfg(), true, true);
        assert_eq!(dx, 0.0);
        assert_eq!(dy, 0.0);
    }

    #[test]
    fn pos_none_はゼロ() {
        let (dx, dy) = edge_scroll_delta(None, rect(), cfg(), true, true);
        assert_eq!(dx, 0.0);
        assert_eq!(dy, 0.0);
    }

    #[test]
    fn 各端の符号と頭打ち() {
        let c = cfg();
        let r = rect();
        // 右端 (x=500) のさらに外 = +max。
        let (dx, _) = edge_scroll_delta(Some((600.0, 200.0)), r, c, true, true);
        assert_eq!(dx, 10.0);
        // 左端 (x=100) の外 = -max。
        let (dx, _) = edge_scroll_delta(Some((0.0, 200.0)), r, c, true, true);
        assert_eq!(dx, -10.0);
        // 下端 (y=350) の外 = +max。
        let (_, dy) = edge_scroll_delta(Some((300.0, 400.0)), r, c, true, true);
        assert_eq!(dy, 10.0);
        // 上端 (y=50) の外 = -max。
        let (_, dy) = edge_scroll_delta(Some((300.0, 0.0)), r, c, true, true);
        assert_eq!(dy, -10.0);
    }

    #[test]
    fn 近接ランプは線形() {
        let c = cfg(); // zone=20, max=10
        let r = rect(); // 右端 x=500、near_hi=480
        // px=490 → (490-480)/20 = 0.5 → 5.0
        let (dx, _) = edge_scroll_delta(Some((490.0, 200.0)), r, c, true, false);
        assert_eq!(dx, 5.0);
        // px=485 → 0.25 → 2.5
        let (dx, _) = edge_scroll_delta(Some((485.0, 200.0)), r, c, true, false);
        assert_eq!(dx, 2.5);
        // ちょうど near_hi=480 → 0 (zone 内側境界)
        let (dx, _) = edge_scroll_delta(Some((480.0, 200.0)), r, c, true, false);
        assert_eq!(dx, 0.0);
    }

    #[test]
    fn 軸ディスエーブルで片軸のみ() {
        let c = cfg();
        let r = rect();
        // 右下隅。axis_x のみ有効。
        let (dx, dy) = edge_scroll_delta(Some((600.0, 400.0)), r, c, true, false);
        assert_eq!(dx, 10.0);
        assert_eq!(dy, 0.0);
        // axis_y のみ有効。
        let (dx, dy) = edge_scroll_delta(Some((600.0, 400.0)), r, c, false, true);
        assert_eq!(dx, 0.0);
        assert_eq!(dy, 10.0);
    }

    #[test]
    fn 両端同時_右下隅は両正() {
        let c = cfg();
        let r = rect();
        let (dx, dy) = edge_scroll_delta(Some((600.0, 400.0)), r, c, true, true);
        assert_eq!(dx, 10.0);
        assert_eq!(dy, 10.0);
    }
}
