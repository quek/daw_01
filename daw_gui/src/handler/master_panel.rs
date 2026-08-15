//! handler::master_panel — r.md #50 のマスターパネル状態と計測器の制御。
//!
//! UI スレッド (view / メニュー / ショートカット) からテレメトリスレッドの
//! `MasterAnalyzer` へ届く経路はここだけ (`AppData::meter_control` の Mutex)。
//! 逆向きは `AppEvent::MasterMeterTick` の一方通行なので、経路が交差しない。

use crate::state::*;

/// マスターパネルの幅の下限 / 上限 [px]。下限はフェーダー列 (55) + ラウドネス
/// バー + 余白が成立する最小、上限はアレンジを潰しすぎない範囲。
pub const MASTER_PANEL_MIN_W: f32 = 180.0;
pub const MASTER_PANEL_MAX_W: f32 = 640.0;

/// 各セクションの最低高 [px] (MASTER / スペクトラム / オシロ / ゴニオ)。
/// これを割り込むと配分ではなくパネル内スクロールで見せる。
pub const MASTER_SECTION_MIN_H: [f32; 4] = [160.0, 90.0, 70.0, 120.0];

impl AppData {
    /// メーター設定 / パネル可視状態をテレメトリスレッドへ反映する。
    ///
    /// Mutex が poison していたら黙って諦める — メーターは監視表示なので、
    /// ここで panic して DAW ごと落とす価値は無い (次の変更で復帰する)。
    pub(crate) fn sync_meter_control(&mut self) {
        let Ok(mut c) = self.meter_control.lock() else {
            return;
        };
        c.settings = self.ui_prefs.meter_settings;
        c.active = self.ui_prefs.master_panel_open;
    }

    /// 積算ラウドネス一式 (I / LRA / 最大 M / 最大 S / 最大 TP) とピーク保持 /
    /// クリップ表示を **同時に** リセットする (EBU Tech 3341 §2.2)。
    ///
    /// 再生開始 (`play`) からも呼ばれる = 「曲を頭から流せばその曲の値が出る」。
    pub(crate) fn reset_master_loudness(&mut self) {
        let Ok(mut c) = self.meter_control.lock() else {
            return;
        };
        c.loudness_reset_epoch = c.loudness_reset_epoch.wrapping_add(1);
    }

    /// ピーク保持線 / 数値ピーク / クリップ表示だけをリセットする
    /// (メーターのクリック)。ラウドネス積算は触らない。
    pub(crate) fn reset_master_peak_hold(&mut self) {
        let Ok(mut c) = self.meter_control.lock() else {
            return;
        };
        c.peak_reset_epoch = c.peak_reset_epoch.wrapping_add(1);
    }
}

/// セクション比率 (合計 1.0) と利用可能高から、各セクションの実高 [px] を出す。
///
/// 最低高の合計が入らないときは**比率を無視して最低高を積む** (= パネル内を
/// 縦スクロールさせる)。戻り値の合計が `avail` を超えていたら呼び出し側が
/// スクロール領域として扱う。
#[must_use]
pub fn section_heights(ratios: [f32; 4], avail: f32) -> [f32; 4] {
    let min_total: f32 = MASTER_SECTION_MIN_H.iter().sum();
    if avail <= min_total {
        return MASTER_SECTION_MIN_H;
    }
    let sum: f32 = ratios.iter().sum();
    if sum <= 0.0 {
        return MASTER_SECTION_MIN_H;
    }
    // まず最低高を確保し、残りを比率で配る。
    let extra = avail - min_total;
    let mut out = MASTER_SECTION_MIN_H;
    for i in 0..4 {
        out[i] += extra * (ratios[i] / sum);
    }
    out
}

/// [`section_heights`] の **逆写像**。ドラッグで作った実高から比率へ戻す。
///
/// 比率の意味は「最低高を引いた**余り**の配分」なので、実高をそのまま
/// 正規化すると逆関数にならず、境界がカーソルに追従しないうえ触っていない
/// セクションまで毎フレーム動く (レビューで発覚)。
#[must_use]
pub fn section_ratios(heights: [f32; 4], avail: f32) -> [f32; 4] {
    let min_total: f32 = MASTER_SECTION_MIN_H.iter().sum();
    let extra = avail - min_total;
    if extra <= 0.0 {
        return [0.25; 4];
    }
    let mut out = [0.0_f32; 4];
    for i in 0..4 {
        out[i] = ((heights[i] - MASTER_SECTION_MIN_H[i]) / extra).max(0.0);
    }
    let sum: f32 = out.iter().sum();
    if sum <= 0.0 {
        return [0.25; 4];
    }
    for v in &mut out {
        *v /= sum;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heights_never_go_below_the_minimum_even_on_a_short_screen() {
        let h = section_heights([0.9, 0.05, 0.03, 0.02], 200.0);
        assert_eq!(h, MASTER_SECTION_MIN_H);
        // 入りきらないので合計は avail を超える = 呼び出し側がスクロールさせる。
        assert!(h.iter().sum::<f32>() > 200.0);
    }

    #[test]
    fn a_lopsided_ratio_still_respects_every_minimum() {
        let h = section_heights([1.0, 0.0, 0.0, 0.0], 900.0);
        for i in 0..4 {
            assert!(h[i] >= MASTER_SECTION_MIN_H[i], "{i}: {h:?}");
        }
        assert!((h.iter().sum::<f32>() - 900.0).abs() < 0.01);
    }

    #[test]
    fn zero_ratios_fall_back_to_the_minimums() {
        assert_eq!(section_heights([0.0; 4], 900.0), MASTER_SECTION_MIN_H);
    }

    /// 比率 → 実高 → 比率 が恒等であること。
    /// これが崩れると、境界をドラッグしても掴んだ位置に止まらず、
    /// 触っていないセクションまで毎フレーム動く。
    #[test]
    fn heights_and_ratios_round_trip() {
        for avail in [600.0_f32, 900.0, 1400.0] {
            for ratios in [[0.25; 4], [0.34, 0.24, 0.18, 0.24], [0.6, 0.2, 0.1, 0.1]] {
                let h = section_heights(ratios, avail);
                let back = section_ratios(h, avail);
                for i in 0..4 {
                    assert!(
                        (back[i] - ratios[i]).abs() < 1e-4,
                        "avail={avail} i={i}: {ratios:?} -> {h:?} -> {back:?}"
                    );
                }
            }
        }
    }

    /// 1 つの境界を動かしたとき、その 2 セクションだけが変わる。
    #[test]
    fn moving_one_boundary_leaves_the_other_sections_alone() {
        let avail = 1000.0;
        let ratios = [0.34, 0.24, 0.18, 0.24];
        let h = section_heights(ratios, avail);
        // MASTER / スペクトラム境界を 40px 下へ。
        let mut moved = h;
        moved[0] += 40.0;
        moved[1] -= 40.0;
        let next = section_heights(section_ratios(moved, avail), avail);
        assert!((next[0] - moved[0]).abs() < 0.05, "掴んだ境界が追従しない: {next:?}");
        assert!((next[1] - moved[1]).abs() < 0.05, "{next:?}");
        assert!((next[2] - h[2]).abs() < 0.05, "触っていないオシロが動いた: {next:?}");
        assert!((next[3] - h[3]).abs() < 0.05, "触っていないゴニオが動いた: {next:?}");
    }
}
