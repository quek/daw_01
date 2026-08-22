// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! マスター出力の計測 (r.md #50)。
//!
//! daw_audio が共有メモリのリング (`common::scope_bridge`) へ書いたマスター
//! 出力サンプルを、daw_gui のテレメトリスレッドが取りこぼしなく読み、
//! **ここ 1 か所で**すべてのメーター値を導出する。ピーク / VU / トゥルーピーク /
//! ラウドネス / スペクトラム / オシロ / ゴニオ・相関が同じサンプル列から出るので、
//! 「同じ音なのにメーターごとに値が違う」が構造的に起きない。
//!
//! 測定対象は `render_master_buffer` の出力 = **書き出す WAV と同一の音**
//! (メトロノームやパニック declick は含まない)。設計は
//! `docs/plan_master_meters.md`。

pub mod scope;
pub mod settings;
pub mod spectrum;
pub mod stereo;

// ラウドネス / トゥルーピークの測定器は `common` が持つ (r.md #54)。
// daw_audio のオフライン範囲解析が同じ型を通るので、ライブメーターと
// 解析レポートで値が食い違わない。
pub use common::loudness;
pub use common::truepeak;

use common::loudness::{LoudnessMeter, LoudnessReadout};
use common::truepeak::TruePeakMeter;
use scope::{SCOPE_COLUMNS, ScopeCapture, ScopeColumn};
use settings::{MeterControl, MeterSettings};
use spectrum::{SPECTRUM_BANDS, SpectrumAnalyzer};
use stereo::{StereoMeter, StereoReadout};

/// VU の 2 次系 (IEC 60268-17: 300ms で 99% 到達 / オーバーシュート 1.0%)。
/// 「300ms 99% + Mp 1%」から数値的に逆算した値。
const VU_ZETA: f32 = 0.8261;
const VU_OMEGA: f32 = 13.9725;
/// 2 次系を semi-implicit Euler で積分するときの最大刻み [秒]。
/// 安定限界 (この ω/ζ で ~0.14s) に十分な余裕を取る。
const VU_MAX_STEP: f32 = 0.01;

/// 表示上の最小 dB。これ未満は「無音」として扱う。
const DB_FLOOR: f32 = -144.0;

/// 無音を合成するときの 1 ティックあたり上限 [秒]。省電力から復帰した直後に
/// 何十秒ぶんも回さないためのガード。
const MAX_SILENCE_SECS: f32 = 0.5;

/// 再描画ダイジェストに混ぜるゴニオ点の固定数 ([`compute_digest`] 参照)。
const DIGEST_GONIO_POINTS: usize = 128;

/// 無音がこの回数続き、かつ表示が変化しなくなったら解析を休む
/// ([`MasterAnalyzer::tick`] の settle 判定)。2 回見るのは「最後の 1 ティックで
/// まだ動いていた」を取りこぼさないため。
const SETTLE_TICKS: u32 = 2;

fn amp_to_db(v: f32) -> f32 {
    if v <= 0.0 { f32::NEG_INFINITY } else { 20.0 * v.log10() }
}

fn db_to_amp(db: f32) -> f32 {
    if db <= DB_FLOOR { 0.0 } else { 10f32.powf(db / 20.0) }
}

/// ピーク (瞬時最大) と VU (300ms 弾道) を 1 本のバーに重ねて出すための表示値。
struct LevelBallistics {
    /// ピークバーの表示値 [dB]。
    peak_db: [f32; 2],
    /// ピーク保持線の表示値 [dB] と保持経過時間 [秒]。
    hold_db: [f32; 2],
    hold_age: [f32; 2],
    /// リセットまで落ちない最大到達ピーク [dB]。
    max_db: f32,
    /// VU 2 次系の状態 (平均二乗ドメイン)。
    vu_y: [f32; 2],
    vu_v: [f32; 2],
    /// クリップ (|x| >= 1.0) を検出したサンプル数。
    clip_count: u32,
}

impl LevelBallistics {
    fn new() -> Self {
        Self {
            peak_db: [f32::NEG_INFINITY; 2],
            hold_db: [f32::NEG_INFINITY; 2],
            hold_age: [0.0; 2],
            max_db: f32::NEG_INFINITY,
            vu_y: [0.0; 2],
            vu_v: [0.0; 2],
            clip_count: 0,
        }
    }

    fn reset_peak_hold(&mut self) {
        self.hold_db = [f32::NEG_INFINITY; 2];
        self.hold_age = [0.0; 2];
        self.max_db = f32::NEG_INFINITY;
    }

    fn reset_clip(&mut self) {
        self.clip_count = 0;
    }

    /// 1 ティックぶんのフレームを取り込む。
    fn process(&mut self, frames: &[[f32; 2]], sample_rate: u32, settings: &MeterSettings) {
        if frames.is_empty() {
            return;
        }
        let mut block_peak = [0.0_f32; 2];
        let mut sum_sq = [0.0_f32; 2];
        for fr in frames {
            for ch in 0..2 {
                let a = fr[ch].abs();
                if a > block_peak[ch] {
                    block_peak[ch] = a;
                }
                if a >= 1.0 {
                    self.clip_count = self.clip_count.saturating_add(1);
                }
                sum_sq[ch] += fr[ch] * fr[ch];
            }
        }
        let dt = frames.len() as f32 / sample_rate as f32;
        let fall = settings.peak_fall_db_per_s * dt;
        let hold_secs = settings.peak_hold_ms as f32 / 1000.0;
        for ch in 0..2 {
            let new_db = amp_to_db(block_peak[ch]);
            let released = self.peak_db[ch] - fall;
            self.peak_db[ch] = if new_db > released { new_db } else { released.max(DB_FLOOR) };
            if new_db > self.max_db {
                self.max_db = new_db;
            }
            if self.peak_db[ch] >= self.hold_db[ch] {
                self.hold_db[ch] = self.peak_db[ch];
                self.hold_age[ch] = 0.0;
            } else {
                self.hold_age[ch] += dt;
                if self.hold_age[ch] > hold_secs {
                    self.hold_db[ch] =
                        (self.hold_db[ch] - settings.peak_fall_db_per_s * dt).max(self.peak_db[ch]);
                }
            }
            // VU: 平均二乗を目標値にした 2 次系 (IEC 60268-17 の弾道)。
            let target = sum_sq[ch] / frames.len() as f32;
            step_second_order(&mut self.vu_y[ch], &mut self.vu_v[ch], target, dt);
        }
    }

    /// VU の表示値 [dB] (mean-square → dB。AES17 の +3.01 dB は使わず、
    /// 0 VU 基準を `vu_reference_dbfs` で与える流儀に統一する)。
    fn vu_db(&self, ch: usize) -> f32 {
        let y = self.vu_y[ch].max(0.0);
        if y <= 0.0 { f32::NEG_INFINITY } else { 10.0 * y.log10() }
    }
}

/// `y'' + 2ζω y' + ω² y = ω² u` を semi-implicit Euler で `dt` 秒進める。
/// 大きい `dt` は `VU_MAX_STEP` に分割して発散を防ぐ。
fn step_second_order(y: &mut f32, v: &mut f32, target: f32, dt: f32) {
    let mut remaining = dt;
    while remaining > 0.0 {
        let h = remaining.min(VU_MAX_STEP);
        let a = VU_OMEGA * VU_OMEGA * (target - *y) - 2.0 * VU_ZETA * VU_OMEGA * *v;
        *v += a * h;
        *y += *v * h;
        if *y < 0.0 {
            *y = 0.0;
            *v = 0.0;
        }
        remaining -= h;
    }
}

/// UI スレッドへ 1 ティックごとに渡す表示状態のスナップショット。
#[derive(Debug, Clone, PartialEq)]
pub struct MasterMeterSnapshot {
    /// バーの塗り = VU (線形振幅、L/R)。
    pub vu: [f32; 2],
    /// バーに重ねる細線 = ピーク (線形振幅、L/R)。
    pub peak: [f32; 2],
    /// ピーク保持線 (線形振幅、L/R)。
    pub peak_hold: [f32; 2],
    /// リセットまで落ちない最大到達ピーク [dBFS]。
    pub peak_max_db: f32,
    /// クリップ検出サンプル数 (0 = クリップしていない)。
    pub clip_count: u32,
    pub loudness: LoudnessReadout,
    /// 直近ブロックのトゥルーピーク [dBTP]。
    pub true_peak_dbtp: f32,
    /// リセット以降の最大トゥルーピーク [dBTP]。
    pub max_true_peak_dbtp: f32,
    pub stereo: StereoReadout,
    /// ゴニオメーターの新しい点 (正規化座標)。
    pub gonio: Vec<[f32; 2]>,
    /// スナップショットの通し番号。ゴニオ widget が「同じバッチを 2 度取り込まない」
    /// ために使う (immediate-mode なので 1 スナップショットが複数フレーム描かれる)。
    pub seq: u64,
    /// スペクトラムの表示値 / ピーク保持値 [dB]。
    pub spectrum_db: Vec<f32>,
    pub spectrum_hold_db: Vec<f32>,
    /// オシロの列 `[Lmin, Lmax, Rmin, Rmax]`。
    pub scope: Vec<ScopeColumn>,
    /// 共有リングを一周されてサンプルを取りこぼした (積算値の信頼性が落ちる)。
    pub overrun: bool,
    /// r.md #57: 測定セッションが running か (EBU Tech 3341 §2.2)。`false` =
    /// トランスポート停止で stand-by = I / LRA / 最大 M / 最大 S / 最大 TP /
    /// 相関レンジが直前の値のまま保持されている。表示側はこれを見て「保持中」を出す。
    pub loudness_running: bool,
    /// 実際に解析に使ったサンプルレート。
    pub sample_rate: u32,
    /// 再描画の要否判定に使うダイジェスト。表示が変わらない限り値も変わらない。
    pub visual_digest: u64,
}

impl Default for MasterMeterSnapshot {
    fn default() -> Self {
        Self {
            vu: [0.0; 2],
            peak: [0.0; 2],
            peak_hold: [0.0; 2],
            peak_max_db: f32::NEG_INFINITY,
            clip_count: 0,
            loudness: LoudnessReadout::default(),
            true_peak_dbtp: f32::NEG_INFINITY,
            max_true_peak_dbtp: f32::NEG_INFINITY,
            stereo: StereoReadout::default(),
            gonio: Vec::new(),
            seq: 0,
            spectrum_db: vec![f32::NEG_INFINITY; SPECTRUM_BANDS],
            spectrum_hold_db: vec![f32::NEG_INFINITY; SPECTRUM_BANDS],
            scope: vec![[0.0; 4]; SCOPE_COLUMNS],
            overrun: false,
            loudness_running: false,
            sample_rate: 0,
            visual_digest: 0,
        }
    }
}

/// マスター出力の全メーターを 1 本のサンプル列から導く解析器。
///
/// テレメトリスレッド (`spawn_playhead_poller`) が所有し、UI スレッドとは
/// [`MeterControl`] (設定 / リセット) と [`MasterMeterSnapshot`] (結果) だけで
/// やり取りする。
pub struct MasterAnalyzer {
    sample_rate: u32,
    settings: MeterSettings,
    level: LevelBallistics,
    loudness: LoudnessMeter,
    truepeak: TruePeakMeter,
    spectrum: SpectrumAnalyzer,
    scope: ScopeCapture,
    stereo: StereoMeter,
    /// 無音を合成するための使い回しバッファ (毎ティックの確保を避ける)。
    silence: Vec<[f32; 2]>,
    /// 非有限サンプルを含むブロックを浄化して渡すための使い回しバッファ。
    sanitized: Vec<[f32; 2]>,
    last_reset_epoch: u64,
    last_peak_reset_epoch: u64,
    /// 無音かつ表示が変化しなかった連続ティック数 (`SETTLE_TICKS` で休止)。
    quiet_ticks: u32,
    /// r.md #57: 前ティックの `rolling`。エッジ検出 (休止解除) と、スナップショットへ
    /// 載せる「保持中か」の値をここから引く。
    last_rolling: bool,
    snapshot: MasterMeterSnapshot,
}

impl MasterAnalyzer {
    pub fn new(sample_rate: u32) -> Self {
        let sr = if sample_rate == 0 { 48_000 } else { sample_rate };
        let settings = MeterSettings::default();
        Self {
            sample_rate: sr,
            level: LevelBallistics::new(),
            loudness: LoudnessMeter::new(sr),
            truepeak: TruePeakMeter::new(sr),
            spectrum: SpectrumAnalyzer::new(sr, &settings),
            scope: ScopeCapture::new(sr),
            stereo: StereoMeter::new(sr),
            silence: Vec::new(),
            sanitized: Vec::new(),
            last_reset_epoch: 0,
            last_peak_reset_epoch: 0,
            quiet_ticks: 0,
            // サブメーターの既定 (`running: true`) と揃える。最初の `tick` が
            // 実際のトランスポート状態で上書きする。
            last_rolling: true,
            snapshot: MasterMeterSnapshot::default(),
            settings,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn set_sample_rate(&mut self, sample_rate: u32) {
        if sample_rate == 0 || sample_rate == self.sample_rate {
            return;
        }
        self.sample_rate = sample_rate;
        self.loudness.set_sample_rate(sample_rate);
        self.truepeak.set_sample_rate(sample_rate);
        self.scope.set_sample_rate(sample_rate);
        self.stereo.set_sample_rate(sample_rate);
    }

    /// ピーク保持 (バーの線と上端の数値) をリセットする。
    pub fn reset_peak_hold(&mut self) {
        self.level.reset_peak_hold();
    }

    /// クリップ表示をリセットする。
    pub fn reset_clip(&mut self) {
        self.level.reset_clip();
    }

    /// 積算ラウドネス一式 (I / LRA / 最大 M / 最大 S / 最大 TP) を同時リセットする
    /// (EBU Tech 3341 §2.2)。相関の観測レンジも合わせて畳む。
    pub fn reset_loudness(&mut self) {
        self.loudness.reset_integrated();
        self.truepeak.reset_max();
        self.stereo.reset_range();
    }

    /// 1 ティック分の処理。`frames` が空なら `elapsed_secs` ぶんの無音を流す
    /// (エンジンが park している / 音が出ていない状態でメーターが凍らないように)。
    ///
    /// r.md #57: `rolling` は「トランスポートが走っているか」 (count-in 中は含まない)。
    /// これが解析器に渡っていなかったのが「停止しても算出が止まらない」の根本原因で、
    /// 唯一の停止条件が「サンプルがビット完全に 0 か」しか無かった。停止中もプラグインの
    /// 残響やノイズフロアが流れ続けるので、その条件は実際にはほぼ成立しない。
    /// **`MeterControl` に入れない** — engine が所有する事実を UI スレッドの Mutex 経由で
    /// 往復させると SSoT を割るうえ、トランスポートのエッジが 1 往復ぶん遅れる。
    pub fn tick(
        &mut self,
        control: &MeterControl,
        sample_rate: u32,
        frames: &[[f32; 2]],
        elapsed_secs: f32,
        overrun: bool,
        rolling: bool,
    ) -> &MasterMeterSnapshot {
        self.set_sample_rate(sample_rate);
        self.settings = control.settings;
        self.spectrum.apply(self.sample_rate, &self.settings);
        // 「ティックごとの状態をサブメーターへ押し下げる」= `spectrum.apply` と同じ流儀。
        self.loudness.set_running(rolling);
        self.truepeak.set_running(rolling);
        self.stereo.set_running(rolling);
        if rolling != self.last_rolling {
            self.last_rolling = rolling;
            // 無音休止に入ったまま再生 / 停止すると `build_snapshot` を通らず、
            // 「保持中」表示が切り替わらないまま固まる。エッジで休止を解く。
            self.quiet_ticks = 0;
        }
        if control.loudness_reset_epoch != self.last_reset_epoch {
            self.last_reset_epoch = control.loudness_reset_epoch;
            self.reset_loudness();
            self.level.reset_peak_hold();
            self.level.reset_clip();
            self.quiet_ticks = 0;
        }
        if control.peak_reset_epoch != self.last_peak_reset_epoch {
            self.last_peak_reset_epoch = control.peak_reset_epoch;
            self.level.reset_peak_hold();
            self.level.reset_clip();
            self.quiet_ticks = 0;
        }

        // 入ってきた音が完全な無音か。無音が続いて表示も変化しなくなったら
        // 解析を丸ごと休む — そうしないと、エンジンが park して何時間経っても
        // K-weighting / トゥルーピーク FIR / FFT が実時間レートで回り続け、
        // r.md #49 の省電力を GUI 側で打ち消してしまう。
        let quiet = frames.iter().all(|f| f[0] == 0.0 && f[1] == 0.0);
        if quiet && self.quiet_ticks >= SETTLE_TICKS {
            // 表示は既に落ち切っている。スナップショットもダイジェストも据え置き。
            self.snapshot.overrun = overrun;
            return &self.snapshot;
        }

        let before = self.snapshot.visual_digest;
        if frames.is_empty() {
            let n = (elapsed_secs.clamp(0.0, MAX_SILENCE_SECS) * self.sample_rate as f32) as usize;
            if n > 0 {
                self.silence.clear();
                self.silence.resize(n, [0.0; 2]);
                let silence = std::mem::take(&mut self.silence);
                self.consume(&silence);
                self.silence = silence;
            }
        } else {
            self.consume(frames);
        }

        self.build_snapshot(overrun);
        if quiet && self.snapshot.visual_digest == before {
            self.quiet_ticks = self.quiet_ticks.saturating_add(1);
        } else {
            self.quiet_ticks = 0;
        }
        &self.snapshot
    }

    /// 非有限サンプルは 0 に潰してから解析へ渡す。
    ///
    /// マスター出力に NaN / Inf が 1 サンプルでも乗ると、フィルタ状態や
    /// 2 次系の積分が汚染されて **音が正常に戻っても永久に復帰しない**
    /// (`y.max(0.0)` は NaN を 0 として返すので、表示は無音のまま固まる)。
    /// 入口 1 か所で塞ぐのが唯一のチョークポイント。
    fn consume(&mut self, frames: &[[f32; 2]]) {
        if frames.iter().any(|f| !f[0].is_finite() || !f[1].is_finite()) {
            self.sanitized.clear();
            self.sanitized.extend(frames.iter().map(|f| {
                [
                    if f[0].is_finite() { f[0] } else { 0.0 },
                    if f[1].is_finite() { f[1] } else { 0.0 },
                ]
            }));
            let clean = std::mem::take(&mut self.sanitized);
            self.consume_clean(&clean);
            self.sanitized = clean;
            return;
        }
        self.consume_clean(frames);
    }

    fn consume_clean(&mut self, frames: &[[f32; 2]]) {
        self.level.process(frames, self.sample_rate, &self.settings);
        self.loudness.process(frames);
        self.truepeak.process(frames);
        self.spectrum.process(frames);
        self.scope.push(frames);
        self.stereo.process(frames);
    }

    fn build_snapshot(&mut self, overrun: bool) {
        self.scope
            .capture(self.settings.scope_window_ms, self.settings.scope_trigger);

        let s = &mut self.snapshot;
        s.vu = [
            db_to_amp(self.level.vu_db(0)),
            db_to_amp(self.level.vu_db(1)),
        ];
        s.peak = [
            db_to_amp(self.level.peak_db[0]),
            db_to_amp(self.level.peak_db[1]),
        ];
        s.peak_hold = [
            db_to_amp(self.level.hold_db[0]),
            db_to_amp(self.level.hold_db[1]),
        ];
        s.peak_max_db = self.level.max_db;
        s.clip_count = self.level.clip_count;
        s.loudness = self.loudness.readout();
        s.true_peak_dbtp = self.truepeak.block_dbtp();
        s.max_true_peak_dbtp = self.truepeak.max_dbtp();
        s.stereo = self.stereo.readout();
        s.gonio.clear();
        s.gonio.extend_from_slice(self.stereo.points());
        s.seq = s.seq.wrapping_add(1);
        s.spectrum_db.clear();
        s.spectrum_db.extend_from_slice(self.spectrum.display_db());
        s.spectrum_hold_db.clear();
        s.spectrum_hold_db.extend_from_slice(self.spectrum.hold_db());
        s.scope.clear();
        s.scope.extend_from_slice(self.scope.columns());
        s.overrun = overrun;
        s.loudness_running = self.last_rolling;
        s.sample_rate = self.sample_rate;
        s.visual_digest = compute_digest(s);
    }

    pub fn snapshot(&self) -> &MasterMeterSnapshot {
        &self.snapshot
    }

    /// 無音が落ち切って解析を休んでいるか (テストと診断用)。
    #[must_use]
    pub fn is_paused_on_silence(&self) -> bool {
        self.quiet_ticks >= SETTLE_TICKS
    }
}

/// 表示が変わったときだけ値が変わるダイジェスト。r.md #49 のアイドル省電力は
/// 「指紋が変わらなければ再描画しない」なので、量子化して**無音では収束する**
/// ことが要件 (生の float を混ぜると永久に変化し続けて省電力が無効になる)。
fn compute_digest(s: &MasterMeterSnapshot) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |v: i64| {
        h ^= v as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    let q = |v: f32, step: f32| -> i64 {
        if v.is_finite() { (v / step).round() as i64 } else { i64::MIN }
    };
    for ch in 0..2 {
        mix(q(s.vu[ch], 1.0 / 1024.0));
        mix(q(s.peak[ch], 1.0 / 1024.0));
        mix(q(s.peak_hold[ch], 1.0 / 1024.0));
    }
    mix(q(s.peak_max_db, 0.1));
    mix(i64::from(s.clip_count));
    let l = &s.loudness;
    for v in [
        l.momentary_lufs,
        l.short_term_lufs,
        l.integrated_lufs,
        l.max_momentary_lufs,
        l.max_short_term_lufs,
        l.lra_lu,
    ] {
        mix(q(v, 0.1));
    }
    // LRA の暫定サフィックス `*` と dim 色は `lra_provisional` で切り替わるが、値そのもの
    // (`lra_lu`) は変わらないことがある。混ぜていないと 60 秒経って確定へ変わった瞬間の
    // 再描画が r.md #49 の抑止に食われて画面が古いままになる。
    mix(i64::from(u8::from(l.lra_provisional)));
    mix(q(s.true_peak_dbtp, 0.1));
    mix(q(s.max_true_peak_dbtp, 0.1));
    let st = &s.stereo;
    for v in [
        st.correlation,
        st.correlation_min,
        st.correlation_max,
        st.width,
    ] {
        mix(q(v, 0.005));
    }
    mix(q(st.balance_db, 0.05));
    for v in &s.spectrum_db {
        mix(q(*v, 0.25));
    }
    for v in &s.spectrum_hold_db {
        mix(q(*v, 0.25));
    }
    for col in &s.scope {
        for v in col {
            mix(q(*v, 1.0 / 512.0));
        }
    }
    // ゴニオ点は**個数がティックごとに揺れる** (ポーラの sleep とオーディオ
    // コールバックの位相で 1440/1920 のように変わる) ので、点をそのまま順に
    // 混ぜると全点 (0,0) の無音でも「混ぜた回数」で値が変わり、指紋が永久に
    // 収束しない = r.md #49 の再描画抑止が効かなくなる。**固定長にリサンプル
    // してから**混ぜることで、絵が同じなら指紋も同じになる。
    for k in 0..DIGEST_GONIO_POINTS {
        match s.gonio.get(k * s.gonio.len() / DIGEST_GONIO_POINTS) {
            Some(p) => {
                mix(q(p[0], 1.0 / 512.0));
                mix(q(p[1], 1.0 / 512.0));
            }
            None => {
                mix(i64::MIN);
                mix(i64::MIN);
            }
        }
    }
    mix(i64::from(u8::from(s.overrun)));
    // r.md #57: 保持中かどうかで読み値の見た目が変わる (「保持」バッジ + ラベル色)。
    // 混ぜないと、停止した瞬間に絵が変わるのに再描画が抑止されて切り替わらない。
    mix(i64::from(u8::from(s.loudness_running)));
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control() -> MeterControl {
        MeterControl {
            settings: MeterSettings::default(),
            loudness_reset_epoch: 0,
            peak_reset_epoch: 0,
            active: true,
        }
    }

    fn sine(fs: u32, freq: f32, n: usize, amp: f32) -> Vec<[f32; 2]> {
        (0..n)
            .map(|i| {
                let v = amp * (std::f32::consts::TAU * freq * i as f32 / fs as f32).sin();
                [v, v]
            })
            .collect()
    }

    /// 「同じサンプル列から全部出す」ことの回帰: 定常正弦を流したとき、
    /// ピーク・VU・ラウドネス・トゥルーピークが互いに矛盾しない値になる。
    #[test]
    fn a_steady_tone_produces_consistent_readings_across_meters() {
        let fs = 48_000;
        let mut a = MasterAnalyzer::new(fs);
        let c = control();
        // -6 dBFS のステレオ正弦を 4 秒。
        let amp = 10f32.powf(-6.0 / 20.0);
        for _ in 0..120 {
            a.tick(&c, fs, &sine(fs, 1000.0, fs as usize / 30, amp), 1.0 / 30.0, false, true);
        }
        let s = a.snapshot();
        // ピーク = -6 dBFS
        assert!((amp_to_db(s.peak[0]) - (-6.0)).abs() < 0.5, "peak {}", amp_to_db(s.peak[0]));
        // VU = mean-square = amp²/2 → -9.03 dB
        let vu_db = amp_to_db(s.vu[0]);
        assert!((vu_db - (-9.03)).abs() < 0.5, "vu {vu_db}");
        // トゥルーピークはサンプルピーク以上。
        assert!(s.max_true_peak_dbtp >= -6.2, "tp {}", s.max_true_peak_dbtp);
        // ラウドネスは校正どおり (-18 dBFS 正弦 = -18 LUFS の 12 dB 上)。
        assert!(
            (s.loudness.integrated_lufs - (-6.0)).abs() < 0.3,
            "I {}",
            s.loudness.integrated_lufs
        );
    }

    /// 無音が続くとダイジェストが収束する = 再描画が止まる (r.md #49 と両立)。
    #[test]
    fn digest_settles_when_the_signal_goes_silent() {
        let fs = 48_000;
        let mut a = MasterAnalyzer::new(fs);
        let c = control();
        a.tick(&c, fs, &sine(fs, 1000.0, fs as usize / 4, 0.9), 0.25, false, true);
        // 30 秒ぶん無音を流し込んで弾道を落とし切る。
        for _ in 0..900 {
            a.tick(&c, fs, &[], 1.0 / 30.0, false, true);
        }
        let d1 = a.snapshot().visual_digest;
        for _ in 0..10 {
            a.tick(&c, fs, &[], 1.0 / 30.0, false, true);
        }
        assert_eq!(d1, a.snapshot().visual_digest, "digest never settled");
    }

    /// **無音のフレーム数がティックごとに揺れても**ダイジェストは収束すること。
    ///
    /// エンジンは停止中も無音を書き続けるので、実機ではポーラの sleep 精度と
    /// オーディオコールバックの位相で毎ティックのフレーム数が変わる。点列を
    /// そのまま指紋に混ぜていると「混ぜた回数」で値が変わり、無音でも 30fps で
    /// 再描画し続ける (r.md #49 の退行)。
    #[test]
    fn digest_settles_even_when_the_silent_block_length_varies() {
        let fs = 48_000;
        let mut a = MasterAnalyzer::new(fs);
        let c = control();
        a.tick(&c, fs, &sine(fs, 1000.0, fs as usize / 4, 0.9), 0.25, false, true);
        let lens = [1440_usize, 1600, 1920, 1520];
        for i in 0..900 {
            let n = lens[i % lens.len()];
            a.tick(&c, fs, &vec![[0.0_f32, 0.0]; n], n as f32 / fs as f32, false, true);
        }
        let d1 = a.snapshot().visual_digest;
        for i in 0..20 {
            let n = lens[i % lens.len()];
            a.tick(&c, fs, &vec![[0.0_f32, 0.0]; n], n as f32 / fs as f32, false, true);
        }
        assert_eq!(d1, a.snapshot().visual_digest, "無音でもブロック長で指紋が動く");
    }

    /// 無音が落ち切ったら解析を休み、音が戻ったら即座に再開する。
    #[test]
    fn analysis_pauses_on_settled_silence_and_resumes_on_sound() {
        let fs = 48_000;
        let mut a = MasterAnalyzer::new(fs);
        let c = control();
        for _ in 0..600 {
            a.tick(&c, fs, &vec![[0.0_f32, 0.0]; 1600], 1.0 / 30.0, false, true);
        }
        assert!(a.is_paused_on_silence(), "無音が続いても休止に入らない");
        // 音が戻れば次のティックで必ず再開してピークが立つ。
        a.tick(&c, fs, &sine(fs, 1000.0, 1600, 0.8), 1.0 / 30.0, false, true);
        assert!(!a.is_paused_on_silence());
        assert!(a.snapshot().peak[0] > 0.5, "peak = {}", a.snapshot().peak[0]);
    }

    /// NaN が 1 サンプル混ざっても、健全な音に戻れば全メーターが復帰する。
    /// (入口で潰さないと 2 次系とフィルタ状態が汚染されて永久に戻らない。)
    #[test]
    fn a_single_nan_sample_does_not_permanently_break_the_meters() {
        let fs = 48_000;
        let mut a = MasterAnalyzer::new(fs);
        let c = control();
        a.tick(&c, fs, &[[f32::NAN, f32::INFINITY]], 1.0 / 30.0, false, true);
        let amp = 10f32.powf(-6.0 / 20.0);
        for _ in 0..150 {
            a.tick(&c, fs, &sine(fs, 1000.0, fs as usize / 30, amp), 1.0 / 30.0, false, true);
        }
        let s = a.snapshot();
        assert!(s.peak[0] > 0.4, "peak が死んでいる: {}", s.peak[0]);
        assert!(s.vu[0] > 0.2, "VU が死んでいる: {}", s.vu[0]);
        assert!(
            s.loudness.integrated_lufs.is_finite(),
            "ラウドネスが死んでいる: {}",
            s.loudness.integrated_lufs
        );
        assert!(s.max_true_peak_dbtp.is_finite());
    }

    /// 音が鳴っている間はダイジェストが動く (= 再描画される)。
    #[test]
    fn digest_changes_while_audio_is_playing() {
        let fs = 48_000;
        let mut a = MasterAnalyzer::new(fs);
        let c = control();
        a.tick(&c, fs, &sine(fs, 440.0, 1600, 0.5), 1.0 / 30.0, false, true);
        let d1 = a.snapshot().visual_digest;
        a.tick(&c, fs, &sine(fs, 440.0, 1600, 0.1), 1.0 / 30.0, false, true);
        assert_ne!(d1, a.snapshot().visual_digest);
    }

    /// リセット世代が上がると積算値が畳まれる。
    #[test]
    fn bumping_the_reset_epoch_clears_the_integrated_readings() {
        let fs = 48_000;
        let mut a = MasterAnalyzer::new(fs);
        let mut c = control();
        for _ in 0..120 {
            a.tick(&c, fs, &sine(fs, 1000.0, fs as usize / 30, 0.5), 1.0 / 30.0, false, true);
        }
        assert!(a.snapshot().loudness.integrated_lufs.is_finite());
        assert!(a.snapshot().max_true_peak_dbtp.is_finite());
        c.loudness_reset_epoch += 1;
        a.tick(&c, fs, &[], 1.0 / 30.0, false, true);
        let s = a.snapshot();
        assert_eq!(s.loudness.integrated_lufs, f32::NEG_INFINITY);
        assert_eq!(s.max_true_peak_dbtp, f32::NEG_INFINITY);
        assert_eq!(s.peak_max_db, f32::NEG_INFINITY);
    }

    /// クリップ (|x| >= 1.0) を数える。
    #[test]
    fn clipping_samples_are_counted_and_resettable() {
        let fs = 48_000;
        let mut a = MasterAnalyzer::new(fs);
        let c = control();
        let frames = vec![[1.0_f32, 0.5], [0.2, -1.2], [0.1, 0.1]];
        a.tick(&c, fs, &frames, 1.0 / 30.0, false, true);
        assert_eq!(a.snapshot().clip_count, 2);
        a.reset_clip();
        a.tick(&c, fs, &[], 1.0 / 30.0, false, true);
        assert_eq!(a.snapshot().clip_count, 0);
    }

    /// VU は 300ms で立ち上がる (ピークより明確に遅い)。
    #[test]
    fn vu_rises_over_about_300ms_while_peak_is_instant() {
        let fs = 48_000;
        let mut a = MasterAnalyzer::new(fs);
        let c = control();
        let chunk = sine(fs, 1000.0, fs as usize / 100, 1.0); // 10ms
        a.tick(&c, fs, &chunk, 0.01, false, true);
        let after_10ms = a.snapshot().vu[0];
        let peak_10ms = a.snapshot().peak[0];
        for _ in 0..29 {
            a.tick(&c, fs, &sine(fs, 1000.0, fs as usize / 100, 1.0), 0.01, false, true);
        }
        let after_300ms = a.snapshot().vu[0];
        assert!(peak_10ms > 0.9, "peak should be instant, got {peak_10ms}");
        assert!(after_10ms < after_300ms * 0.5, "VU rose too fast: {after_10ms} -> {after_300ms}");
        // snapshot の vu は**線形振幅** (peak と同じ次元) なので、
        // 平均二乗 0.5 の 99% = sqrt(0.495) ≒ 0.7036 が期待値。
        let expected = 0.7071_f32;
        assert!(
            (after_300ms - expected).abs() / expected < 0.03,
            "VU at 300ms = {after_300ms}, expected ~{expected}"
        );
    }

    /// サンプルレートが変わっても落ちず、追従して測り直す。
    #[test]
    fn switching_sample_rate_reconfigures_every_meter() {
        let mut a = MasterAnalyzer::new(48_000);
        let c = control();
        a.tick(&c, 48_000, &sine(48_000, 1000.0, 4800, 0.5), 0.1, false, true);
        a.tick(&c, 96_000, &sine(96_000, 1000.0, 9600, 0.5), 0.1, false, true);
        assert_eq!(a.sample_rate(), 96_000);
        assert_eq!(a.snapshot().sample_rate, 96_000);
    }

    /// r.md #57: トランスポートを止めたら測定セッション側の量は 1 つも進まない。
    /// ただし「今鳴っている音」を映すメーターは止まらない — 停止中もプラグインの
    /// 残響や鍵盤プレビューで実際に音は出ているので、ここを凍らせると
    /// 「音が出ているのに振れない」嘘の表示になる。
    #[test]
    fn stopping_the_transport_freezes_the_measurement_but_not_the_live_meters() {
        let fs = 48_000;
        let mut a = MasterAnalyzer::new(fs);
        let c = control();
        let amp = 10f32.powf(-18.0 / 20.0);
        // 5 秒走らせて測定セッションを立ち上げる。
        for _ in 0..150 {
            a.tick(&c, fs, &sine(fs, 1000.0, fs as usize / 30, amp), 1.0 / 30.0, false, true);
        }
        let running = a.snapshot().clone();
        assert!(running.loudness_running, "走行中は running");
        assert!(running.loudness.integrated_lufs.is_finite(), "I が出ている");

        // **同じ音を流したまま** 停止 (= 残響が鳴り続けている状況の再現) を 10 秒。
        for _ in 0..300 {
            a.tick(&c, fs, &sine(fs, 1000.0, fs as usize / 30, amp), 1.0 / 30.0, false, false);
        }
        let held = a.snapshot().clone();
        assert!(!held.loudness_running, "停止中は stand-by");
        for (name, before, after) in [
            ("I", running.loudness.integrated_lufs, held.loudness.integrated_lufs),
            ("LRA", running.loudness.lra_lu, held.loudness.lra_lu),
            ("M max", running.loudness.max_momentary_lufs, held.loudness.max_momentary_lufs),
            ("S max", running.loudness.max_short_term_lufs, held.loudness.max_short_term_lufs),
            ("TP max", running.max_true_peak_dbtp, held.max_true_peak_dbtp),
            ("corr max", running.stereo.correlation_max, held.stereo.correlation_max),
            ("measured", running.loudness.measured_secs, held.loudness.measured_secs),
        ] {
            assert_eq!(before, after, "{name} は停止中に動かない");
        }
        // ライブ側は追従したまま。
        assert!(
            (held.loudness.momentary_lufs - (-18.0)).abs() < 1.0,
            "M は停止中も今の音を映す ({})",
            held.loudness.momentary_lufs
        );
        assert!(held.peak[0] > 0.0, "ピークバーも止まらない");
        assert!(held.true_peak_dbtp.is_finite(), "直近ブロック TP も止まらない");
        // 停止したまま 60 秒相当を跨いでも LRA の暫定表示が確定へ化けない
        // (`measured_secs` が進まないことの帰結)。
        assert!(held.loudness.lra_provisional, "停止中に LRA が確定扱いにならない");

        // 再生を再開すれば続きから積算が動き出す (stand-by は「保持」であって
        // 「終了」ではない = Tech 3341 の continue)。
        for _ in 0..150 {
            a.tick(&c, fs, &sine(fs, 1000.0, fs as usize / 30, amp), 1.0 / 30.0, false, true);
        }
        let resumed = a.snapshot();
        assert!(resumed.loudness_running, "再開で running");
        assert!(
            resumed.loudness.measured_secs > held.loudness.measured_secs,
            "再開したら経過時間が伸びる"
        );
    }
}
