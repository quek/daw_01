//! r.md #129: 組み込み DSP の golden (`src/native_dsp/golden_v38.txt`) の刺激・統計・テキスト形式。
//!
//! **記録側と比較側が同じこのコードを使う。** 記録は旧 `mixer::channel_strip` の tests にあった
//! `record_golden` (commit 76837672 の strip DSP、旧型と一緒に削除済み)、比較は新 `native_dsp` の tests
//! (`docs/plan_rack_native_devices.md` §15.3 T1)。旧 DSP の型には依存しない — 記録の後で旧 DSP は
//! 消えるので、ここが旧型を引くと比較側がビルドできなくなる。例外は `limiter.on = true` の
//! master_full で、ceiling を超えていた旧 Limiter を直した後に `native_dsp::tests::rerecord_limiter_scenarios`
//! で窓だけを取り直した (r.md #129 E)。
//!
//! - 刺激: 48 kHz・2 s のステレオ 3 種 ([`Stimulus`])。
//! - 駆動: 刺激を `block` サンプルずつ in-place で処理させる ([`run_blocks`])。
//! - 統計: 4096 サンプル窓ごとの RMS / peak / PRBS 重み付き和 / GR ([`WindowStats`])。
//! - 形式: 行単位のテキスト ([`format`] / [`parse`])。f64 は `{:?}` (最短の往復可能表記) で書くので、
//!   読み戻した値は書いた値とビット一致する。
//!
//! 比較の許容誤差 (T1: `|Δ| ≤ 1e-6 + 1e-5·|ref|`、`|Δgr| ≤ 1e-4 dB`) は比較側が持つ。

use std::f64::consts::PI;
use std::path::PathBuf;

pub const SAMPLE_RATE: u32 = 48_000;
/// 刺激の長さ (2 s)。
pub const LEN: usize = 96_000;
/// 統計の窓長。最後の窓だけ短い (`LEN % WINDOW` サンプル)。
pub const WINDOW: usize = 4_096;
/// [`Stimulus::Noise`] の段の長さ (0.25 s)。
pub const NOISE_STEP: usize = 12_000;
/// [`Stimulus::Noise`] の段ごとのピーク振幅 (dBFS)。段 `k` は `NOISE_LEVELS_DB[k % 3]`。
pub const NOISE_LEVELS_DB: [f64; 3] = [-40.0, -12.0, 0.0];
/// [`Stimulus::Tones`] の周波数と、R チャンネルだけに足す初期位相。振幅は各 0.25。
pub const TONE_HZ: [f64; 3] = [50.0, 1_000.0, 8_000.0];
pub const TONE_R_PHASE: [f64; 3] = [PI / 4.0, PI / 2.0, 3.0 * PI / 4.0];
/// [`Stimulus::Impulses`] の周期 (0.1 s)。
pub const IMPULSE_PERIOD: usize = 4_800;

const NOISE_SEED_L: u64 = 0x9E37_79B9_7F4A_7C15;
const NOISE_SEED_R: u64 = 0xD1B5_4A32_D192_ED03;
const PRBS_SEED_L: u64 = 0x243F_6A88_85A3_08D3;
const PRBS_SEED_R: u64 = 0x1319_8A2E_0370_7344;

/// golden ファイルの置き場 (crate root 相対)。
pub fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/native_dsp/golden_v38.txt")
}

/// 窓の数 (最後の短い窓を含む)。
pub fn window_count() -> usize {
    LEN.div_ceil(WINDOW)
}

/// 刺激 3 種。すべて L/R が異なる (チャンネルの取り違えが `sig_*` に出る)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stimulus {
    /// xorshift64 の一様ノイズ (L/R 非相関)。0.25 s ごとにピーク振幅が −40 / −12 / 0 dBFS を巡回する
    /// (上がる段でアタック、0 → −40 の段でリリースを見る)。
    Noise,
    /// 50 Hz + 1 kHz + 8 kHz の正弦の和 (各振幅 0.25)。R は各成分に [`TONE_R_PHASE`] の位相を足す。
    Tones,
    /// インパルス列。L は [`IMPULSE_PERIOD`] ごとに振幅 1.0 と 0.25 を交互、R は L から 1200 サンプル
    /// 遅れて −0.5。
    Impulses,
}

impl Stimulus {
    pub const ALL: [Self; 3] = [Self::Noise, Self::Tones, Self::Impulses];

    pub fn name(self) -> &'static str {
        match self {
            Self::Noise => "noise",
            Self::Tones => "tones",
            Self::Impulses => "impulses",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// `input_gain_db` を掛けた刺激を作る (掛け算は f32 で 1 回、`0.0` なら素のまま)。
    pub fn generate(self, input_gain_db: f64) -> Stereo {
        let (mut l, mut r) = (vec![0.0_f32; LEN], vec![0.0_f32; LEN]);
        match self {
            Self::Noise => {
                let (mut sl, mut sr) = (NOISE_SEED_L, NOISE_SEED_R);
                for i in 0..LEN {
                    let amp = db_to_amp(NOISE_LEVELS_DB[(i / NOISE_STEP) % NOISE_LEVELS_DB.len()]);
                    l[i] = (uniform(&mut sl) * amp) as f32;
                    r[i] = (uniform(&mut sr) * amp) as f32;
                }
            }
            Self::Tones => {
                for i in 0..LEN {
                    let t = i as f64 / f64::from(SAMPLE_RATE);
                    let (mut a, mut b) = (0.0_f64, 0.0_f64);
                    for (hz, phase) in TONE_HZ.iter().zip(TONE_R_PHASE) {
                        a += 0.25 * (2.0 * PI * hz * t).sin();
                        b += 0.25 * (2.0 * PI * hz * t + phase).sin();
                    }
                    l[i] = a as f32;
                    r[i] = b as f32;
                }
            }
            Self::Impulses => {
                for i in (0..LEN).step_by(IMPULSE_PERIOD) {
                    l[i] = if (i / IMPULSE_PERIOD) % 2 == 0 { 1.0 } else { 0.25 };
                    if let Some(s) = r.get_mut(i + 1_200) {
                        *s = -0.5;
                    }
                }
            }
        }
        if input_gain_db != 0.0 {
            let g = db_to_amp(input_gain_db) as f32;
            l.iter_mut().chain(r.iter_mut()).for_each(|s| *s *= g);
        }
        Stereo { l, r }
    }
}

pub struct Stereo {
    pub l: Vec<f32>,
    pub r: Vec<f32>,
}

/// 1 ブロックの処理結果の GR (dB、0 以下)。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BlockGr {
    /// 処理対象の DSP の GR (Comp / Bus Comp。EQ 系は 0)。
    pub gr: f32,
    /// master 完全形のリミッターの GR (それ以外は 0)。
    pub lim_gr: f32,
}

/// 1 窓の統計。値はすべて処理後の信号から f64 で計算する。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowStats {
    pub start: usize,
    pub len: usize,
    pub rms_l: f64,
    pub rms_r: f64,
    pub peak_l: f64,
    pub peak_r: f64,
    /// PRBS (±1、サンプル位置の関数) で重み付けした和 ÷ `len`。符号・時間ずれ・L/R の取り違えを拾う。
    pub sig_l: f64,
    pub sig_r: f64,
    /// 窓に始点がある全ブロックの [`BlockGr::gr`] の最小値。
    pub gr: f64,
    pub lim_gr: f64,
}

/// `input` を先頭から `block` サンプルずつ `process(l, r)` へ in-place で渡し (最後だけ短い)、
/// 窓統計を返す。`block` は [`WINDOW`] を割り切ること (ブロックの境界が窓の境界に揃う)。
pub fn run_blocks(
    input: &Stereo,
    block: usize,
    mut process: impl FnMut(&mut [f32], &mut [f32]) -> BlockGr,
) -> Vec<WindowStats> {
    assert!(block > 0 && WINDOW % block == 0, "block {block} は WINDOW {WINDOW} を割り切らない");
    let (mut l, mut r) = (input.l.clone(), input.r.clone());
    let mut grs = vec![BlockGr::default(); window_count()];
    for start in (0..LEN).step_by(block) {
        let end = (start + block).min(LEN);
        let g = process(&mut l[start..end], &mut r[start..end]);
        let w = &mut grs[start / WINDOW];
        w.gr = w.gr.min(g.gr);
        w.lim_gr = w.lim_gr.min(g.lim_gr);
    }
    (0..window_count())
        .map(|w| {
            let start = w * WINDOW;
            let end = (start + WINDOW).min(LEN);
            let (rms_l, peak_l, sig_l) = channel_stats(&l[start..end], start, PRBS_SEED_L);
            let (rms_r, peak_r, sig_r) = channel_stats(&r[start..end], start, PRBS_SEED_R);
            WindowStats {
                start,
                len: end - start,
                rms_l,
                rms_r,
                peak_l,
                peak_r,
                sig_l,
                sig_r,
                gr: f64::from(grs[w].gr),
                lim_gr: f64::from(grs[w].lim_gr),
            }
        })
        .collect()
}

/// `(rms, peak, sig)`。`offset` は窓の先頭のサンプル位置 (PRBS の添字)。
fn channel_stats(x: &[f32], offset: usize, prbs_seed: u64) -> (f64, f64, f64) {
    let (mut sq, mut peak, mut sig) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (i, s) in x.iter().enumerate() {
        let v = f64::from(*s);
        sq += v * v;
        peak = peak.max(v.abs());
        let w = splitmix64(prbs_seed.wrapping_add((offset + i) as u64));
        sig += if w & 1 == 1 { v } else { -v };
    }
    let n = x.len() as f64;
    ((sq / n).sqrt(), peak, sig / n)
}

fn db_to_amp(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// `[-1, 1)` の一様乱数 (xorshift64、Marsaglia 13/7/17 の上位 53 bit)。
fn uniform(state: &mut u64) -> f64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    (x >> 11) as f64 / (1_u64 << 53) as f64 * 2.0 - 1.0
}

fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// 1 シナリオ。`meta` は駆動条件 (`kind` / `driver` / `stimulus` / `block` / `input_gain_db`) と DSP の
/// 設定値で、並びは記録順 (比較側はキーで引く)。旧 DSP の記録器は旧型と一緒に消えたので、meta は
/// ファイルにある値が正本。
#[derive(Debug, Clone, PartialEq)]
pub struct Scenario {
    pub name: String,
    pub meta: Vec<(String, String)>,
    pub windows: Vec<WindowStats>,
}

impl Scenario {
    pub fn meta(&self, key: &str) -> Option<&str> {
        self.meta.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Golden {
    /// ファイル先頭の `# ` コメント行 (記録条件の説明)。`# ` は含まない。
    pub header: Vec<String>,
    pub scenarios: Vec<Scenario>,
}

/// テキスト形式:
///
/// ```text
/// # <header>                       (先頭にだけ置ける)
/// scenario <name>
/// meta <key> <value>               (value は行末まで)
/// w <index> <start> <len> <rms_l> <rms_r> <peak_l> <peak_r> <sig_l> <sig_r> <gr> <lim_gr>
/// ```
///
/// シナリオの間は空行 1 つ。
pub fn format(golden: &Golden) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for h in &golden.header {
        writeln!(out, "# {h}").unwrap();
    }
    for s in &golden.scenarios {
        writeln!(out, "\nscenario {}", s.name).unwrap();
        for (k, v) in &s.meta {
            writeln!(out, "meta {k} {v}").unwrap();
        }
        for (i, w) in s.windows.iter().enumerate() {
            writeln!(
                out,
                "w {i} {} {} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?}",
                w.start, w.len, w.rms_l, w.rms_r, w.peak_l, w.peak_r, w.sig_l, w.sig_r, w.gr, w.lim_gr
            )
            .unwrap();
        }
    }
    out
}

/// [`format`] の逆。行末の `\r` は許す。
pub fn parse(text: &str) -> Result<Golden, String> {
    let mut golden = Golden { header: Vec::new(), scenarios: Vec::new() };
    for (ln, line) in text.lines().enumerate().map(|(i, l)| (i + 1, l)) {
        let err = |m: &str| format!("golden {ln} 行目: {m}: {line:?}");
        if line.is_empty() {
            continue;
        }
        if let Some(h) = line.strip_prefix("# ") {
            if !golden.scenarios.is_empty() {
                return Err(err("コメントはファイル先頭にだけ置ける"));
            }
            golden.header.push(h.to_string());
        } else if let Some(name) = line.strip_prefix("scenario ") {
            golden.scenarios.push(Scenario { name: name.to_string(), meta: Vec::new(), windows: Vec::new() });
        } else if let Some(rest) = line.strip_prefix("meta ") {
            let s = golden.scenarios.last_mut().ok_or_else(|| err("scenario の前の meta"))?;
            let (k, v) = rest.split_once(' ').ok_or_else(|| err("meta に値が無い"))?;
            s.meta.push((k.to_string(), v.to_string()));
        } else if let Some(rest) = line.strip_prefix("w ") {
            let s = golden.scenarios.last_mut().ok_or_else(|| err("scenario の前の窓"))?;
            let f: Vec<&str> = rest.split(' ').collect();
            if f.len() != 11 || f[0].parse::<usize>().ok() != Some(s.windows.len()) {
                return Err(err("窓の列数か通し番号が合わない"));
            }
            let int = |x: &str| x.parse::<usize>().map_err(|e| err(&e.to_string()));
            let num = |x: &str| x.parse::<f64>().map_err(|e| err(&e.to_string()));
            s.windows.push(WindowStats {
                start: int(f[1])?,
                len: int(f[2])?,
                rms_l: num(f[3])?,
                rms_r: num(f[4])?,
                peak_l: num(f[5])?,
                peak_r: num(f[6])?,
                sig_l: num(f[7])?,
                sig_r: num(f[8])?,
                gr: num(f[9])?,
                lim_gr: num(f[10])?,
            });
        } else {
            return Err(err("解釈できない行"));
        }
    }
    Ok(golden)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 記録済みの golden が壊れていないこと。比較側はこのファイルを [`parse`] した値だけを信じるので、
    /// (1) 読み戻しが無損失 (再書き出しがバイト一致) で、(2) 各シナリオが比較に必要な駆動条件と
    /// 全窓を持つことをここで保証する。
    #[test]
    fn golden_v38_は無損失に読み戻せて全シナリオが駆動条件と全窓を持つ() {
        let text = std::fs::read_to_string(golden_path()).expect("golden_v38.txt を読めない");
        let golden = parse(&text).expect("golden_v38.txt を解釈できない");
        assert_eq!(format(&golden), text.replace("\r\n", "\n"), "再書き出しがバイト一致しない");
        assert!(!golden.scenarios.is_empty());

        let mut names = std::collections::HashSet::new();
        for s in &golden.scenarios {
            assert!(names.insert(s.name.as_str()), "シナリオ名が重複: {}", s.name);
            assert!(s.meta("stimulus").and_then(Stimulus::from_name).is_some(), "{}: stimulus", s.name);
            let block: usize = s.meta("block").and_then(|b| b.parse().ok()).expect("block");
            assert!(block > 0 && WINDOW % block == 0, "{}: block {block}", s.name);
            assert!(s.meta("input_gain_db").and_then(|g| g.parse::<f64>().ok()).is_some(), "{}", s.name);
            assert_eq!(s.windows.len(), window_count(), "{}: 窓の数", s.name);
            for (i, w) in s.windows.iter().enumerate() {
                assert_eq!((w.start, w.len), (i * WINDOW, WINDOW.min(LEN - i * WINDOW)), "{}", s.name);
                let v = [w.rms_l, w.rms_r, w.peak_l, w.peak_r, w.sig_l, w.sig_r, w.gr, w.lim_gr];
                assert!(v.iter().all(|x| x.is_finite()), "{} 窓 {i}: 非有限値", s.name);
            }
        }
    }
}
