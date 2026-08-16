//! ITU-R BS.1770-5 / EBU Tech 3341 / Tech 3342 準拠のラウドネス測定。
//!
//! **測定器はプロセスをまたいで 1 つ** (r.md #54)。daw_gui のライブメーター
//! (`master_meter::MasterAnalyzer`) と daw_audio のオフライン範囲解析
//! (`daw_audio::export` の freewheel) が同じ [`LoudnessMeter`] を通るので、
//! 「メーターの数値と解析レポートの数値が食い違う」が構造的に起きない。
//!
//! - K-weighting は 48kHz の表をハードコードせず**任意サンプルレートへ再設計**する。
//!   48kHz を代入すると規格表と小数 14 桁まで一致することを回帰テストで固定した
//!   (`k_weighting_at_48k_matches_the_standard_table`)。
//! - 100ms サブブロックを共通基盤にして、Momentary(400ms) / Short-term(3s) /
//!   Integrated(400ms ゲーティングブロック・ホップ 100ms) / LRA を全部そこから導く。
//! - Integrated と LRA の履歴は 0.1 dB 刻み 1000 bin のヒストグラムに積む
//!   (libebur128 と同じ)。曲長に依らずメモリ一定で、確保が増えない。

/// K-weighting 段1 (shelving) の設計パラメータ (BS.1770 の表を 48kHz で再現する値)。
const SHELF_F0: f64 = 1681.974450955533;
const SHELF_GAIN_DB: f64 = 3.999843853973347;
const SHELF_Q: f64 = 0.7071752369554196;
/// `Vb = Vh^SHELF_VB_EXP`。
const SHELF_VB_EXP: f64 = 0.4996667741545416;

/// K-weighting 段2 (RLB high-pass) の設計パラメータ。
const HP_F0: f64 = 38.13547087602444;
const HP_Q: f64 = 0.5003270373238773;

/// ラウドネス式の定数 `-0.691` (997Hz における K-weighting のゲインを打ち消す)。
/// **サンプルレートを変えても固定** (規格の要求)。
pub const LOUDNESS_OFFSET: f64 = -0.691;

/// 絶対ゲート閾値 [LKFS] (BS.1770 eq.5)。
const ABS_GATE_LUFS: f64 = -70.0;
/// 相対ゲート [LU] (BS.1770 eq.6)。
const REL_GATE_LU: f64 = -10.0;
/// LRA の相対ゲート [LU] (Tech 3342。BS.1770 の -10 とは違う)。
const LRA_REL_GATE_LU: f64 = -20.0;

/// ヒストグラムのビン数 / 分解能 / 下端。
pub const HIST_BINS: usize = 1000;
const HIST_STEP_DB: f64 = 0.1;
pub const HIST_MIN_LUFS: f64 = -70.0;

/// Momentary 窓 = 400ms = 4 サブブロック。
const MOMENTARY_BLOCKS: usize = 4;
/// Short-term 窓 = 3s = 30 サブブロック。
const SHORT_TERM_BLOCKS: usize = 30;

/// LRA が「まだ安定していない」期間 [秒] (Tech 3342)。
pub const LRA_PROVISIONAL_SECS: f32 = 60.0;

/// 双一次変換で設計した 2 次 IIR。`a0` は 1 に正規化済み。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BiquadCoeffs {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

/// Direct Form I の状態。
#[derive(Debug, Clone, Copy, Default)]
struct BiquadState {
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl BiquadState {
    #[inline]
    fn process(&mut self, c: &BiquadCoeffs, x: f64) -> f64 {
        let y = c.b0 * x + c.b1 * self.x1 + c.b2 * self.x2 - c.a1 * self.y1 - c.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// K-weighting 段1 (high-shelf)。BS.1770-5 Annex 1 Table 1 を任意 fs へ再設計。
///
/// **RBJ の標準 high-shelf 式では規格の係数に一致しない**ので使ってはいけない。
pub fn design_shelving(fs: f64) -> BiquadCoeffs {
    let k = (std::f64::consts::PI * SHELF_F0 / fs).tan();
    let vh = 10f64.powf(SHELF_GAIN_DB / 20.0);
    let vb = vh.powf(SHELF_VB_EXP);
    let den = 1.0 + k / SHELF_Q + k * k;
    BiquadCoeffs {
        b0: (vh + vb * k / SHELF_Q + k * k) / den,
        b1: 2.0 * (k * k - vh) / den,
        b2: (vh - vb * k / SHELF_Q + k * k) / den,
        a1: 2.0 * (k * k - 1.0) / den,
        a2: (1.0 - k / SHELF_Q + k * k) / den,
    }
}

/// K-weighting 段2 (RLB high-pass)。BS.1770-5 Annex 1 Table 2 を任意 fs へ再設計。
pub fn design_highpass(fs: f64) -> BiquadCoeffs {
    let k = (std::f64::consts::PI * HP_F0 / fs).tan();
    let den = 1.0 + k / HP_Q + k * k;
    BiquadCoeffs {
        b0: 1.0,
        b1: -2.0,
        b2: 1.0,
        a1: 2.0 * (k * k - 1.0) / den,
        a2: (1.0 - k / HP_Q + k * k) / den,
    }
}

/// 1 チャンネルぶんの K-weighting フィルタ (shelving → high-pass の直列)。
#[derive(Debug, Clone, Copy, Default)]
struct KWeighting {
    s1: BiquadState,
    s2: BiquadState,
}

impl KWeighting {
    #[inline]
    fn process(&mut self, shelf: &BiquadCoeffs, hp: &BiquadCoeffs, x: f64) -> f64 {
        let a = self.s1.process(shelf, x);
        self.s2.process(hp, a)
    }

    fn reset(&mut self) {
        self.s1.reset();
        self.s2.reset();
    }
}

/// 0.1 dB 刻みのラウドネス分布ヒストグラム。`-70 LUFS` を下端に 1000 bin。
#[derive(Debug, Clone)]
struct LoudnessHistogram {
    counts: Vec<u64>,
    total: u64,
}

impl LoudnessHistogram {
    fn new() -> Self {
        Self { counts: vec![0; HIST_BINS], total: 0 }
    }

    fn clear(&mut self) {
        self.counts.fill(0);
        self.total = 0;
    }

    /// bin `i` の代表ラウドネス [LUFS] (ビン中央)。
    fn bin_loudness(i: usize) -> f64 {
        HIST_MIN_LUFS + (i as f64 + 0.5) * HIST_STEP_DB
    }

    /// bin `i` の代表エネルギー (= `10^((L + 0.691)/10)`)。
    fn bin_energy(i: usize) -> f64 {
        10f64.powf((Self::bin_loudness(i) - LOUDNESS_OFFSET) / 10.0)
    }

    fn add(&mut self, loudness: f64) {
        if !loudness.is_finite() || loudness <= ABS_GATE_LUFS {
            return;
        }
        let idx = ((loudness - HIST_MIN_LUFS) / HIST_STEP_DB).floor();
        if idx < 0.0 {
            return;
        }
        let i = (idx as usize).min(HIST_BINS - 1);
        self.counts[i] += 1;
        self.total += 1;
    }

    /// `threshold` を超える bin だけのエネルギー平均から求めたラウドネス。
    /// 該当が無ければ `None`。
    fn gated_loudness(&self, threshold_lufs: f64) -> Option<f64> {
        let mut sum = 0.0;
        let mut n: u64 = 0;
        for (i, &c) in self.counts.iter().enumerate() {
            if c == 0 {
                continue;
            }
            if Self::bin_loudness(i) <= threshold_lufs {
                continue;
            }
            sum += Self::bin_energy(i) * c as f64;
            n += c;
        }
        if n == 0 {
            return None;
        }
        Some(LOUDNESS_OFFSET + 10.0 * (sum / n as f64).log10())
    }

    /// `threshold` を超える bin の分布に対する `p` パーセンタイル [LUFS]。
    /// Tech 3342 の 1-based `round((n-1)*p/100 + 1)` に合わせる。
    fn gated_percentile(&self, threshold_lufs: f64, p: f64) -> Option<f64> {
        let n: u64 = self
            .counts
            .iter()
            .enumerate()
            .filter(|(i, _)| Self::bin_loudness(*i) >= threshold_lufs)
            .map(|(_, c)| *c)
            .sum();
        if n == 0 {
            return None;
        }
        // 1-based rank。
        let rank = (((n - 1) as f64) * p / 100.0 + 1.0).round().max(1.0) as u64;
        let mut seen: u64 = 0;
        for (i, &c) in self.counts.iter().enumerate() {
            if c == 0 || Self::bin_loudness(i) < threshold_lufs {
                continue;
            }
            seen += c;
            if seen >= rank {
                return Some(Self::bin_loudness(i));
            }
        }
        None
    }
}

/// ラウドネス測定器の読み出し値。到達していない値は `f32::NEG_INFINITY`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoudnessReadout {
    pub momentary_lufs: f32,
    pub short_term_lufs: f32,
    pub integrated_lufs: f32,
    pub max_momentary_lufs: f32,
    pub max_short_term_lufs: f32,
    pub lra_lu: f32,
    /// リセットから 60 秒未満 = LRA はまだ暫定 (Tech 3342)。
    pub lra_provisional: bool,
    /// リセットからの測定経過時間 [秒]。
    pub measured_secs: f32,
    /// `max_momentary_lufs` を出した 400ms 窓の**先頭**位置 [秒] (リセット起点)。
    /// 未到達なら `None`。窓の先頭を返すのは、オフライン解析のレポートから
    /// 「一番大きかったところ」へ飛んで**その頭から**聴けるようにするため。
    pub max_momentary_at_secs: Option<f32>,
    /// `max_short_term_lufs` を出した 3s 窓の**先頭**位置 [秒] (リセット起点)。
    pub max_short_term_at_secs: Option<f32>,
}

impl Default for LoudnessReadout {
    fn default() -> Self {
        Self {
            momentary_lufs: f32::NEG_INFINITY,
            short_term_lufs: f32::NEG_INFINITY,
            integrated_lufs: f32::NEG_INFINITY,
            max_momentary_lufs: f32::NEG_INFINITY,
            max_short_term_lufs: f32::NEG_INFINITY,
            lra_lu: 0.0,
            lra_provisional: true,
            measured_secs: 0.0,
            max_momentary_at_secs: None,
            max_short_term_at_secs: None,
        }
    }
}

/// ステレオ (L/R とも重み 1.0) のラウドネス測定器。
pub struct LoudnessMeter {
    sample_rate: u32,
    shelf: BiquadCoeffs,
    hp: BiquadCoeffs,
    filters: [KWeighting; 2],
    /// 100ms サブブロックの長さ [samples]。
    block_len: usize,
    /// 現在のサブブロックに積んだ二乗和 (チャンネル合計) と、そのサンプル数。
    acc: f64,
    acc_n: usize,
    /// 直近 30 サブブロックの平均二乗 (チャンネル合計)。
    subblocks: std::collections::VecDeque<f64>,
    /// Integrated 用ヒストグラム (400ms ゲーティングブロック)。
    integrated_hist: LoudnessHistogram,
    /// LRA 用ヒストグラム (3s Short-term、10Hz)。
    lra_hist: LoudnessHistogram,
    max_momentary: f64,
    max_short_term: f64,
    /// `max_momentary` / `max_short_term` を出した窓の先頭サブブロック番号。
    max_momentary_at_block: u64,
    max_short_term_at_block: u64,
    /// リセットからの完了サブブロック数 (= 経過時間 × 10)。
    elapsed_blocks: u64,
    /// r.md #57: EBU Tech 3341 §2.2 の running / stand-by。`false` の間は
    /// **測定セッションに属する量** (Integrated / LRA / 最大 M / 最大 S / 経過時間) の
    /// 積算を止めて直前の値を保持する。K-weighting フィルタと 100ms サブブロックの
    /// 積み上げは止めない — 止めるとフィルタ状態が凍って再開時に過渡が出るし、
    /// Momentary / Short-term は規格上つねにライブでなければならない。
    ///
    /// 既定は `true`。オフライン解析 (`LoudnessMeter` を直接使う経路) は
    /// `set_running` を呼ばないので、既定が `false` だと全部 -inf になる。
    running: bool,
    /// stand-by / リセットを挟まずに連続で積んだサブブロック数。
    ///
    /// ゲーティング窓 (Momentary 400ms = 4 ブロック / Short-term 3s = 30 ブロック) は
    /// `subblocks` の直近 n 個の平均なので、**stand-by 中に積んだブロックが窓に残って
    /// いる間に積算を再開すると、停止中の音がそのまま Integrated / LRA / 最大値へ
    /// 流れ込む**。停止中もプラグインの残響やモニタ音は出ている (engine は `playing` に
    /// 関係なく scope リングへ書く) ので、これは実際に起きる。窓の中身が全部 running 由来に
    /// なるまで積算対象にしないためのカウンタ。
    running_blocks: usize,
}

impl LoudnessMeter {
    pub fn new(sample_rate: u32) -> Self {
        let fs = f64::from(sample_rate.max(1));
        Self {
            sample_rate,
            shelf: design_shelving(fs),
            hp: design_highpass(fs),
            filters: [KWeighting::default(); 2],
            // 「最も近いサンプルに丸める」= (fs + 5) / 10。
            block_len: ((sample_rate as usize) + 5) / 10,
            acc: 0.0,
            acc_n: 0,
            subblocks: std::collections::VecDeque::with_capacity(SHORT_TERM_BLOCKS),
            integrated_hist: LoudnessHistogram::new(),
            lra_hist: LoudnessHistogram::new(),
            max_momentary: f64::NEG_INFINITY,
            max_short_term: f64::NEG_INFINITY,
            max_momentary_at_block: 0,
            max_short_term_at_block: 0,
            elapsed_blocks: 0,
            running: true,
            running_blocks: 0,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// r.md #57: 測定セッションを running / stand-by に切り替える
    /// (EBU Tech 3341 §2.2 の start/pause/continue)。stand-by は「保持」であって
    /// 「リセット」ではない — continue できることが規格の要件で、リセットは
    /// [`Self::reset_integrated`] という独立した操作。
    pub fn set_running(&mut self, running: bool) {
        if running == self.running {
            return;
        }
        self.running = running;
        // stand-by を跨いだら、ゲーティング窓の中身が全部 running 由来に戻るまで
        // 積算しない (停止中の残響が再開後の Integrated / LRA / 最大値を汚さない)。
        self.running_blocks = 0;
    }

    /// サンプルレートが変わったら設計し直す (フィルタ状態も履歴も捨てる)。
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        if sample_rate == self.sample_rate {
            return;
        }
        *self = Self::new(sample_rate);
    }

    /// 積算値だけをリセットする (M / S のスライディング窓は残す —
    /// 「今鳴っている音」の表示が一瞬途切れないように)。
    pub fn reset_integrated(&mut self) {
        self.integrated_hist.clear();
        self.lra_hist.clear();
        self.max_momentary = f64::NEG_INFINITY;
        self.max_short_term = f64::NEG_INFINITY;
        self.max_momentary_at_block = 0;
        self.max_short_term_at_block = 0;
        self.elapsed_blocks = 0;
        // `subblocks` (= M / S のライブ窓) は意図的に残す (リセットで M / S を
        // 一瞬 -inf に落とさない)。 ただし窓にはリセット前の音が残っているので、
        // **積算対象にするのは窓が入れ替わってから**。 再生開始時の自動リセット
        // (`reset_master_loudness`) で、直前まで鳴っていた音が新しい測定の
        // 1 個目のゲーティングブロックになるのを防ぐ。
        self.running_blocks = 0;
    }

    /// フィルタ状態を含めて完全にリセットする。
    pub fn reset_all(&mut self) {
        self.reset_integrated();
        for f in &mut self.filters {
            f.reset();
        }
        self.acc = 0.0;
        self.acc_n = 0;
        self.subblocks.clear();
    }

    /// ステレオフレーム列を流し込む。
    pub fn process(&mut self, frames: &[[f32; 2]]) {
        for fr in frames {
            let l = self.filters[0].process(&self.shelf, &self.hp, f64::from(fr[0]));
            let r = self.filters[1].process(&self.shelf, &self.hp, f64::from(fr[1]));
            self.acc += l * l + r * r;
            self.acc_n += 1;
            if self.acc_n >= self.block_len {
                self.finish_subblock();
            }
        }
    }

    fn finish_subblock(&mut self) {
        let mean_sq = self.acc / self.acc_n as f64;
        self.acc = 0.0;
        self.acc_n = 0;
        if self.subblocks.len() == SHORT_TERM_BLOCKS {
            self.subblocks.pop_front();
        }
        self.subblocks.push_back(mean_sq);

        // r.md #57: ここから下が「測定セッションに属する量」。stand-by (= トランス
        // ポート停止) の間は 1 つも進めない。上のフィルタ処理とサブブロック積み上げは
        // 止めない (Momentary / Short-term はライブ表示のまま)。
        if !self.running {
            return;
        }
        self.elapsed_blocks += 1;
        self.running_blocks = self.running_blocks.saturating_add(1);

        // **リセット地点 / 停止区間をまたぐ窓は確定させない**。`reset_integrated` は
        // 「今鳴っている音」の表示を途切れさせないために `subblocks` (直近 3 秒) を残し、
        // stand-by (r.md #57) 中もライブ表示のために積み上げ自体は続ける。よって窓の
        // 中身が **全部「リセット後」かつ「running 由来」** になるまで待たないと、
        // リセット前 / 停止中の音が新しい測定の Integrated / LRA / 最大 M / 最大 S に
        // 混入する (相対ゲートの基準まで押し上げるので、以後の静かな素材が全部ゲートで
        // 落ちて I が前回値に張り付く)。
        //
        // `running_blocks` は `reset_integrated` と stand-by 遷移の**両方**で 0 に戻り、
        // running のときしか増えないので、r.md #54 の `elapsed_blocks` 条件を包含する。
        // 連続 running なら `subblocks.len() >= n` と同時に成立するので通常再生は挙動不変。

        // Momentary (400ms) → 同時に Integrated のゲーティングブロックでもある
        // (窓 400ms / ホップ 100ms = 75% オーバーラップ)。
        if self.running_blocks >= MOMENTARY_BLOCKS
            && let Some(m) = self.window_loudness(MOMENTARY_BLOCKS)
        {
            if m > self.max_momentary {
                self.max_momentary = m;
                // 窓は直近 MOMENTARY_BLOCKS 個 = `[elapsed - n, elapsed)`。
                self.max_momentary_at_block =
                    self.elapsed_blocks.saturating_sub(MOMENTARY_BLOCKS as u64);
            }
            self.integrated_hist.add(m);
        }
        // Short-term (3s) → LRA の入力 (10Hz)。
        if self.running_blocks >= SHORT_TERM_BLOCKS
            && let Some(s) = self.window_loudness(SHORT_TERM_BLOCKS)
        {
            if s > self.max_short_term {
                self.max_short_term = s;
                self.max_short_term_at_block =
                    self.elapsed_blocks.saturating_sub(SHORT_TERM_BLOCKS as u64);
            }
            self.lra_hist.add(s);
        }
    }

    /// 直近 `n` サブブロックの平均からラウドネスを求める。溜まっていなければ `None`。
    fn window_loudness(&self, n: usize) -> Option<f64> {
        if self.subblocks.len() < n {
            return None;
        }
        let sum: f64 = self.subblocks.iter().rev().take(n).sum();
        let mean = sum / n as f64;
        if mean <= 0.0 {
            return None;
        }
        Some(LOUDNESS_OFFSET + 10.0 * mean.log10())
    }

    /// BS.1770 eq.5〜7 の 2 段ゲートで求めた Integrated。
    pub fn integrated(&self) -> Option<f64> {
        let absolute = self.integrated_hist.gated_loudness(ABS_GATE_LUFS)?;
        let relative = absolute + REL_GATE_LU;
        self.integrated_hist
            .gated_loudness(relative.max(ABS_GATE_LUFS))
    }

    /// Tech 3342 の LRA [LU]。
    pub fn lra(&self) -> Option<f64> {
        let absolute = self.lra_hist.gated_loudness(ABS_GATE_LUFS)?;
        let threshold = absolute + LRA_REL_GATE_LU;
        let low = self.lra_hist.gated_percentile(threshold, 10.0)?;
        let high = self.lra_hist.gated_percentile(threshold, 95.0)?;
        Some(high - low)
    }

    pub fn readout(&self) -> LoudnessReadout {
        let to_f32 = |v: Option<f64>| v.map_or(f32::NEG_INFINITY, |x| x as f32);
        // r.md #57: BS.1770 は -70 LUFS **以下**のブロックを全測定から除外する
        // (`LoudnessHistogram::add` と同じ閾値)。表示にも同じ床を敷く。敷かないと、
        // 停止中のプラグインのノイズフロア / dither で「-95.3 LUFS」のような数字が
        // 小刻みに動き続け、「止まっていない」ように見える。
        let gated = |v: Option<f64>| match v {
            Some(x) if x > ABS_GATE_LUFS => x as f32,
            _ => f32::NEG_INFINITY,
        };
        let secs = self.elapsed_blocks as f32 / 10.0;
        LoudnessReadout {
            momentary_lufs: gated(self.window_loudness(MOMENTARY_BLOCKS)),
            short_term_lufs: gated(self.window_loudness(SHORT_TERM_BLOCKS)),
            integrated_lufs: to_f32(self.integrated()),
            max_momentary_lufs: gated(Some(self.max_momentary)),
            max_short_term_lufs: gated(Some(self.max_short_term)),
            lra_lu: self.lra().unwrap_or(0.0) as f32,
            lra_provisional: secs < LRA_PROVISIONAL_SECS,
            measured_secs: secs,
            max_momentary_at_secs: self
                .max_momentary
                .is_finite()
                .then(|| self.max_momentary_at_block as f32 / 10.0),
            max_short_term_at_secs: self
                .max_short_term
                .is_finite()
                .then(|| self.max_short_term_at_block as f32 / 10.0),
        }
    }

    /// 現在の Momentary (400ms 窓) [LUFS]。窓が埋まっていなければ `None`。
    /// `readout()` は Integrated (ヒストグラム 2 走査) まで計算するので、
    /// オフライン解析が曲線を毎バッファ積むときはこちらを使う。
    pub fn momentary_lufs(&self) -> Option<f64> {
        self.window_loudness(MOMENTARY_BLOCKS)
    }

    /// 現在の Short-term (3s 窓) [LUFS]。窓が埋まっていなければ `None`。
    pub fn short_term_lufs(&self) -> Option<f64> {
        self.window_loudness(SHORT_TERM_BLOCKS)
    }

    /// Short-term の分布ヒストグラム (LRA の入力そのもの)。
    /// bin `i` の中心ラウドネスは [`histogram_bin_lufs`]。
    pub fn short_term_histogram(&self) -> &[u64] {
        &self.lra_hist.counts
    }
}

/// [`LoudnessMeter::short_term_histogram`] の bin `i` が表すラウドネス [LUFS] (ビン中央)。
#[must_use]
pub fn histogram_bin_lufs(i: usize) -> f64 {
    LoudnessHistogram::bin_loudness(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, eps: f64, what: &str) {
        assert!((a - b).abs() <= eps, "{what}: expected {b} ± {eps}, got {a}");
    }

    /// 指定 dBFS の 1kHz 正弦をステレオで `secs` 秒ぶん流し込む。
    fn feed(m: &mut LoudnessMeter, sample_rate: u32, dbfs: f32, secs: f32) {
        let amp = 10f32.powf(dbfs / 20.0);
        let total = (sample_rate as f32 * secs) as usize;
        let mut block = vec![[0.0f32; 2]; 1024];
        let mut n = 0usize;
        while n < total {
            let k = 1024.min(total - n);
            for (i, fr) in block.iter_mut().enumerate().take(k) {
                let t = (n + i) as f32 / sample_rate as f32;
                let v = amp * (std::f32::consts::TAU * 1000.0 * t).sin();
                *fr = [v, v];
            }
            m.process(&block[..k]);
            n += k;
        }
    }

    /// リセット地点をまたぐ窓が新しい測定を汚染しないこと。
    ///
    /// `reset_integrated` は「今鳴っている音」の表示を途切れさせないために
    /// 直近 3 秒のサブブロックを残すので、経過ブロック数で見ないとリセット前の
    /// 大きい音が Integrated / 最大 M / 最大 S に載り、しかも相対ゲートの基準を
    /// 押し上げて以後の静かな素材を全部落とす (= I が前回値に張り付く)。
    #[test]
    fn reset_後の測定にリセット前の音が混入しない() {
        let mut m = LoudnessMeter::new(48_000);
        feed(&mut m, 48_000, -10.0, 3.0);
        m.reset_integrated();
        // リセット後は静か → 少し大きい、の順。最大はリセット後の後半に出るはず。
        feed(&mut m, 48_000, -40.0, 4.0);
        feed(&mut m, 48_000, -30.0, 4.0);
        let r = m.readout();
        // 汚染していれば -13 前後に張り付く (リセット前の -10 dBFS が相対ゲートの
        // 基準を押し上げ、以後の静かなブロックを全部落とすため)。
        assert!(
            r.integrated_lufs < -25.0,
            "reset 前の音が Integrated に残っている: {}",
            r.integrated_lufs
        );
        approx(f64::from(r.max_momentary_lufs), -30.0, 0.3, "reset 後の最大 M");
        approx(f64::from(r.max_short_term_lufs), -30.0, 0.3, "reset 後の最大 S");
        // 位置はリセット起点。リセット前の音を最大と誤認して 0 に飽和しない。
        let at = r.max_momentary_at_secs.expect("最大 M の位置");
        assert!(at >= 4.0, "リセット前の音を最大として拾っている: {at}");
    }

    /// BS.1770-5 Annex 1 Table 1 / Table 2 の 48kHz 係数と一致すること。
    /// 「48kHz だけ表をハードコードする」実装に退行させないための固定。
    #[test]
    fn k_weighting_at_48k_matches_the_standard_table() {
        let s = design_shelving(48_000.0);
        approx(s.b0, 1.53512485958697, 1e-12, "shelf b0");
        approx(s.b1, -2.69169618940638, 1e-12, "shelf b1");
        approx(s.b2, 1.19839281085285, 1e-12, "shelf b2");
        approx(s.a1, -1.69065929318241, 1e-12, "shelf a1");
        approx(s.a2, 0.73248077421585, 1e-12, "shelf a2");

        let h = design_highpass(48_000.0);
        approx(h.a1, -1.99004745483398, 1e-11, "hp a1");
        approx(h.a2, 0.99007225036621, 1e-11, "hp a2");
        assert_eq!((h.b0, h.b1, h.b2), (1.0, -2.0, 1.0));
    }

    fn sine(fs: u32, freq: f64, secs: f64, amp: f64, stereo: bool) -> Vec<[f32; 2]> {
        let n = (fs as f64 * secs) as usize;
        (0..n)
            .map(|i| {
                let v = (amp
                    * (std::f64::consts::TAU * freq * i as f64 / f64::from(fs)).sin())
                    as f32;
                if stereo { [v, v] } else { [v, 0.0] }
            })
            .collect()
    }

    /// BS.1770-5 の自己検証: 0 dBFS / 997Hz を 1ch だけに入れると -3.01 LKFS。
    #[test]
    fn single_channel_full_scale_997hz_reads_minus_3_01_lkfs() {
        let fs = 48_000;
        let mut m = LoudnessMeter::new(fs);
        // Short-term は 3 秒窓なので 4 秒流す。
        m.process(&sine(fs, 997.0, 4.0, 1.0, false));
        let s = m.readout().short_term_lufs;
        assert!((s - (-3.01)).abs() < 0.1, "got {s}");
    }

    /// EBU Tech 3341 §2.9 の校正: 1kHz ステレオ同相をピーク -18 dBFS で入れると -18.0 LUFS。
    #[test]
    fn stereo_1khz_at_minus_18_dbfs_reads_minus_18_lufs() {
        let fs = 48_000;
        let amp = 10f64.powf(-18.0 / 20.0);
        let mut m = LoudnessMeter::new(fs);
        m.process(&sine(fs, 1000.0, 4.0, amp, true));
        let r = m.readout();
        assert!((r.short_term_lufs - (-18.0)).abs() < 0.1, "S = {}", r.short_term_lufs);
        assert!((r.integrated_lufs - (-18.0)).abs() < 0.1, "I = {}", r.integrated_lufs);
        assert!((r.momentary_lufs - (-18.0)).abs() < 0.1, "M = {}", r.momentary_lufs);
    }

    /// 44.1kHz でも同じ読みになる (再設計式が効いている)。
    #[test]
    fn calibration_holds_at_44100(){
        let fs = 44_100;
        let amp = 10f64.powf(-18.0 / 20.0);
        let mut m = LoudnessMeter::new(fs);
        m.process(&sine(fs, 1000.0, 4.0, amp, true));
        let s = m.readout().short_term_lufs;
        assert!((s - (-18.0)).abs() < 0.15, "got {s}");
    }

    /// EBU Tech 3341 適合テスト #1 相当: -23 LUFS 定常 → I = -23.0 ±0.1。
    #[test]
    fn integrated_of_a_steady_minus_23_lufs_tone_is_minus_23() {
        let fs = 48_000;
        // ステレオ同相正弦: L = R = A sin。K-weighting 後のパワーは 2 * A²/2 * g
        // なので、-23 LUFS になる A を -18 の校正から逆算する (= -18 - 5 dB)。
        let amp = 10f64.powf((-18.0 - 5.0) / 20.0);
        let mut m = LoudnessMeter::new(fs);
        m.process(&sine(fs, 1000.0, 6.0, amp, true));
        let i = m.readout().integrated_lufs;
        assert!((i - (-23.0)).abs() < 0.12, "got {i}");
    }

    /// 絶対ゲート: -70 LUFS 未満の区間は Integrated に寄与しない。
    #[test]
    fn absolute_gate_excludes_near_silence() {
        let fs = 48_000;
        let loud = 10f64.powf(-18.0 / 20.0);
        let quiet = 10f64.powf(-90.0 / 20.0);
        let mut m = LoudnessMeter::new(fs);
        m.process(&sine(fs, 1000.0, 3.0, loud, true));
        m.process(&sine(fs, 1000.0, 6.0, quiet, true));
        let i = m.readout().integrated_lufs;
        // 無音側が平均に入っていれば大きく下がる。ゲートが効いていれば -18 のまま。
        assert!((i - (-18.0)).abs() < 0.3, "got {i}");
    }

    /// 相対ゲート: -10 LU 以上下の区間も Integrated から落ちる。
    #[test]
    fn relative_gate_excludes_quiet_passages() {
        let fs = 48_000;
        let loud = 10f64.powf(-18.0 / 20.0);
        // -18 から 20 LU 下 = 相対ゲート (-10 LU) より下。
        let quiet = 10f64.powf(-38.0 / 20.0);
        let mut m = LoudnessMeter::new(fs);
        m.process(&sine(fs, 1000.0, 4.0, loud, true));
        m.process(&sine(fs, 1000.0, 4.0, quiet, true));
        let i = m.readout().integrated_lufs;
        assert!((i - (-18.0)).abs() < 0.4, "got {i}");
    }

    /// EBU Tech 3342 適合テスト: -20 dBFS 20s + -30 dBFS 20s → LRA = 10 ±1 LU。
    #[test]
    fn lra_of_a_10_lu_two_level_signal_is_10() {
        let fs = 48_000;
        let a = 10f64.powf(-20.0 / 20.0);
        let b = 10f64.powf(-30.0 / 20.0);
        let mut m = LoudnessMeter::new(fs);
        m.process(&sine(fs, 1000.0, 20.0, a, true));
        m.process(&sine(fs, 1000.0, 20.0, b, true));
        let lra = m.readout().lra_lu;
        assert!((lra - 10.0).abs() <= 1.0, "got {lra}");
    }

    /// EBU Tech 3342 適合テスト: -20 と -15 → LRA = 5 ±1 LU。
    #[test]
    fn lra_of_a_5_lu_two_level_signal_is_5() {
        let fs = 48_000;
        let a = 10f64.powf(-20.0 / 20.0);
        let b = 10f64.powf(-15.0 / 20.0);
        let mut m = LoudnessMeter::new(fs);
        m.process(&sine(fs, 1000.0, 20.0, a, true));
        m.process(&sine(fs, 1000.0, 20.0, b, true));
        let lra = m.readout().lra_lu;
        assert!((lra - 5.0).abs() <= 1.0, "got {lra}");
    }

    #[test]
    fn silence_reports_negative_infinity_rather_than_a_number() {
        let mut m = LoudnessMeter::new(48_000);
        m.process(&vec![[0.0, 0.0]; 48_000 * 2]);
        let r = m.readout();
        assert_eq!(r.integrated_lufs, f32::NEG_INFINITY);
        assert_eq!(r.momentary_lufs, f32::NEG_INFINITY);
    }

    #[test]
    fn reset_integrated_clears_the_accumulated_values() {
        let fs = 48_000;
        let amp = 10f64.powf(-18.0 / 20.0);
        let mut m = LoudnessMeter::new(fs);
        m.process(&sine(fs, 1000.0, 3.0, amp, true));
        assert!(m.readout().integrated_lufs.is_finite());
        m.reset_integrated();
        assert_eq!(m.readout().integrated_lufs, f32::NEG_INFINITY);
        assert_eq!(m.readout().max_momentary_lufs, f32::NEG_INFINITY);
    }

    #[test]
    fn lra_is_flagged_provisional_for_the_first_60_seconds() {
        let fs = 48_000;
        let amp = 10f64.powf(-18.0 / 20.0);
        let mut m = LoudnessMeter::new(fs);
        m.process(&sine(fs, 1000.0, 5.0, amp, true));
        assert!(m.readout().lra_provisional);
    }

    /// r.md #57: stand-by 中に積んだ 100ms サブブロックが、再開後の測定へ流れ込まない。
    ///
    /// ゲーティング窓は直近 4 個 (Momentary) / 30 個 (Short-term) のサブブロック平均
    /// なので、素朴に「running のときだけ積算する」だけだと、再開直後の窓に
    /// **停止中の音が最大 3 個 / 29 個**残ったまま Integrated と最大値へ入る。
    /// 停止中もプラグインの残響やモニタ音は鳴る (engine は `playing` に関係なく
    /// scope リングへ書く) ので、これは実際に起きる。
    #[test]
    fn audio_heard_while_stopped_does_not_leak_into_the_next_measurement() {
        let fs = 48_000;
        let mut m = LoudnessMeter::new(fs);
        // 停止中に大きな音 (-6 dBFS) が 1 秒鳴っている。
        m.set_running(false);
        m.process(&sine(fs, 1000.0, 1.0, 10f64.powf(-6.0 / 20.0), true));
        assert_eq!(m.readout().max_momentary_lufs, f32::NEG_INFINITY, "停止中は積まない");

        // 再生開始。以後は静かな曲 (-30 dBFS) だけを測る。
        m.set_running(true);
        m.process(&sine(fs, 1000.0, 2.0, 10f64.powf(-30.0 / 20.0), true));
        let r = m.readout();
        // 混入すると最初の M ブロックが (停止中 3 個 + 再開後 1 個) の平均 ≒ -7.3 LUFS に
        // なり、`M max` がそこへ張り付く。
        assert!(
            (r.max_momentary_lufs - (-30.0)).abs() < 2.0,
            "M max は再開後の音だけを見る (got {})",
            r.max_momentary_lufs
        );
        assert!(
            (r.integrated_lufs - (-30.0)).abs() < 2.0,
            "I も再開後の音だけ (got {})",
            r.integrated_lufs
        );
    }

    /// r.md #57: BS.1770 の絶対ゲート (-70 LUFS) 未満は「プログラムではない」ので
    /// 数値表示も `-inf` にする。これが無いと、停止中にプラグインのノイズフロアで
    /// `-95.3 LUFS` のような数字が小刻みに動き続けて「算出が止まっていない」ように見える。
    #[test]
    fn momentary_below_the_absolute_gate_reads_negative_infinity() {
        let fs = 48_000;
        let mut m = LoudnessMeter::new(fs);
        // -95 dBFS の正弦 = K-weighting 後もゲート下。
        m.process(&sine(fs, 1000.0, 4.0, 10f64.powf(-95.0 / 20.0), true));
        let r = m.readout();
        assert_eq!(r.momentary_lufs, f32::NEG_INFINITY, "M はゲート下で -inf");
        assert_eq!(r.short_term_lufs, f32::NEG_INFINITY, "S はゲート下で -inf");
        assert_eq!(r.max_momentary_lufs, f32::NEG_INFINITY, "M max も -inf");

        // ゲート上の音なら従来どおり実測値が出る (床が効きすぎていないことの確認)。
        let mut loud = LoudnessMeter::new(fs);
        loud.process(&sine(fs, 1000.0, 4.0, 10f64.powf(-18.0 / 20.0), true));
        assert!((loud.readout().momentary_lufs - (-18.0)).abs() < 1.0);
    }
}
