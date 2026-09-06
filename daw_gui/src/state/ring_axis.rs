//! Sampler / MIDI Capture 両タブが共有する「リング座標 → x」の写像 (**スイープ表示**)。
//!
//! 単位は呼び手が決める: Sampler はリングの frame (`head` = 書き込み位置、`capacity` =
//! リング長)、MIDI Capture は wall-clock ns (`head` = 今、`capacity` = 表示秒数)。
//! どちらも「`[head - capacity, head)` を物理位置 `head % capacity` で折り返して描く」
//! だけなので写像は 1 本で足りる。

/// 描画に使う「リング座標 → x」の写像。**スイープ表示**: 左端 = リングの物理位置 0、
/// 書き込み位置が左から右へ進み、右端に着いたら左へ戻る (オシロスコープの sweep)。
///
/// スクロールしないので、鳴っている最中でも画面上の波形 / ノートが流れず、一時停止
/// せずに範囲選択できる。書き込み位置より **左が今の周回 (新しい)、右が前の周回 (古い)**。
/// ある x に居る絶対位置は、書き込み位置がそこを通り過ぎるまで (= 1 周) 変わらない
/// ので、選択も小節線もその場に留まる。
///
/// `offset` は表示の位相 (`[0, capacity)`): 物理位置 `p` を `(p + offset) % capacity`
/// の x に描く。折り返し点をまたぐ範囲はドラッグで選べないので、位相をずらして
/// 折り返し点を動かす (ヘッダの「半周ずらす」/ 本体上のホイール)。
#[derive(Debug, Clone, Copy)]
pub struct RingAxis {
    pub x: f32,
    pub w: f32,
    /// 書き込み位置 (Sampler = `write_frames`、MIDI Capture = 今の wall-clock ns)。
    pub head: u64,
    pub capacity: u64,
    pub offset: u64,
}

impl RingAxis {
    pub fn oldest(&self) -> u64 {
        self.head.saturating_sub(self.capacity)
    }

    /// 書き込み位置のリング内物理位置 `[0, capacity)`。
    pub fn head_phys(&self) -> u64 {
        if self.capacity == 0 { 0 } else { self.head % self.capacity }
    }

    /// 物理位置 → 表示位置 (位相 `offset` を足して折り返す)。
    fn phys_to_disp(&self, phys: u64) -> u64 {
        if self.capacity == 0 { 0 } else { (phys + self.offset % self.capacity) % self.capacity }
    }

    /// 表示位置 → 物理位置。`disp == capacity` (右端ぴったり) は `capacity` のまま返す
    /// (`offset == 0` のとき「前の周回の終端」として `phys_to_frame` が解く)。
    fn disp_to_phys(&self, disp: u64) -> u64 {
        if self.capacity == 0 {
            return 0;
        }
        let off = self.offset % self.capacity;
        if off == 0 { disp.min(self.capacity) } else { (disp.min(self.capacity) + self.capacity - off) % self.capacity }
    }

    /// 今の周回の物理位置 0 に当たる絶対位置。
    fn lap_start(&self) -> u64 {
        self.head - self.head_phys()
    }

    /// 物理位置 → 絶対位置。書き込み位置より左は今の周回、右は前の周回。
    /// 前の周回がまだ無い (最初の周回で head より右) なら `None` (未書き込み)。
    /// `phys == capacity` (右端ぴったり) は前の周回の終端 = 今の周回の先頭。
    pub fn phys_to_frame(&self, phys: u64) -> Option<u64> {
        if self.capacity == 0 {
            return None;
        }
        let phys = phys.min(self.capacity);
        let head = self.head_phys();
        let lap = self.lap_start();
        if phys <= head {
            Some(lap + phys)
        } else {
            lap.checked_sub(self.capacity).map(|prev| prev + phys)
        }
    }

    /// 物理位置 → x (位相込み)。
    pub fn phys_to_x(&self, phys: u64) -> f32 {
        if self.capacity == 0 {
            return self.x;
        }
        self.x + (self.phys_to_disp(phys) as f64 / self.capacity as f64) as f32 * self.w
    }

    pub fn frame_to_x(&self, frame: u64) -> f32 {
        if self.capacity == 0 {
            return self.x;
        }
        self.phys_to_x(frame % self.capacity)
    }

    /// x → 物理位置 (位相を戻す)。
    pub fn x_to_phys(&self, x: f32) -> u64 {
        if self.w <= 0.0 || self.capacity == 0 {
            return 0;
        }
        let t = ((x - self.x) / self.w).clamp(0.0, 1.0) as f64;
        self.disp_to_phys((t * self.capacity as f64).round() as u64)
    }

    /// x → 絶対位置。未書き込みの位置 (最初の周回で head より右) は書き込み位置へ
    /// clamp する (選択が書き込み済みの範囲を超えない)。
    pub fn x_to_frame(&self, x: f32) -> u64 {
        self.phys_to_frame(self.x_to_phys(x)).unwrap_or(self.head).min(self.head)
    }

    /// 絶対区間 `[start, end)` を x 区間に写す。右端で折り返す区間は 2 本
    /// (`[start.., 右端]` と `[左端, ..end]`)、それ以外は 1 本。リングから押し出された
    /// 部分は切り落とす。
    pub fn x_spans(&self, start: u64, end: u64) -> [Option<(f32, f32)>; 2] {
        if self.capacity == 0 || end <= start {
            return [None, None];
        }
        let start = start.max(self.oldest());
        if end <= start {
            return [None, None];
        }
        let len = (end - start).min(self.capacity);
        let ds = self.phys_to_disp(start % self.capacity);
        let de = ds + len;
        let disp_x = |d: u64| self.x + (d as f64 / self.capacity as f64) as f32 * self.w;
        if de <= self.capacity {
            [Some((disp_x(ds), disp_x(de))), None]
        } else {
            [Some((disp_x(ds), self.x + self.w)), Some((self.x, disp_x(de - self.capacity)))]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_axis_roundtrip() {
        // スイープ表示: head=1000 / cap=500 → head は物理 0 (左端)。
        let ax = RingAxis { x: 10.0, w: 100.0, head: 1000, capacity: 500, offset: 0 };
        assert_eq!(ax.frame_to_x(500), 10.0);
        assert_eq!(ax.frame_to_x(1000), 10.0, "head は左端 (折り返した直後)");
        assert_eq!(ax.frame_to_x(750), 60.0);
        // head より右は前の周回: 物理 250 → 1000 - 500 + 250。
        assert_eq!(ax.x_to_frame(60.0), 750);
        // 右端ぴったりは前の周回の終端 (= 今の周回の先頭 = 1000)。
        assert_eq!(ax.x_to_frame(110.0), 1000);
        // head が途中 (物理 100): 左は今の周回、右は前の周回。
        let ax = RingAxis { x: 10.0, w: 100.0, head: 1100, capacity: 500, offset: 0 };
        assert_eq!(ax.x_to_frame(20.0), 1050, "head の左 = 今の周回");
        assert_eq!(ax.x_to_frame(90.0), 1000 - 500 + 400, "head の右 = 前の周回");
        // 最初の周回で head より右は未書き込み → head へ clamp。
        let ax = RingAxis { x: 10.0, w: 100.0, head: 100, capacity: 500, offset: 0 };
        assert_eq!(ax.x_to_frame(90.0), 100);
        // 折り返す区間は 2 本に割れる (`[900, 1100)` = 物理 400..500 + 0..100)。
        let ax = RingAxis { x: 10.0, w: 100.0, head: 1100, capacity: 500, offset: 0 };
        let [a, b] = ax.x_spans(900, 1100);
        assert_eq!(a, Some((90.0, 110.0)));
        assert_eq!(b, Some((10.0, 30.0)));
        assert_eq!(ax.x_spans(600, 900), [Some((30.0, 90.0)), None]);
        // 位相を半周ずらすと同じ区間が 1 本になり、x → frame も往復する。
        let ax = RingAxis { x: 10.0, w: 100.0, head: 1100, capacity: 500, offset: 250 };
        assert_eq!(ax.x_spans(900, 1100), [Some((40.0, 80.0)), None]);
        for x in [15.0, 40.0, 60.0, 80.0, 105.0] {
            let f = ax.x_to_frame(x);
            assert!((ax.frame_to_x(f) - x).abs() < 0.5, "x={x} → {f} → {}", ax.frame_to_x(f));
        }
    }
}
