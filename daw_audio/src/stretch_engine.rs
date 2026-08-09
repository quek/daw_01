//! Spectral stretch / pitch / formant engine — the RT-safe Rust side of
//! [`signalsmith_sys`].
//!
//! # なぜスペクトル方式か (r.md #40)
//!
//! 旧 `StretchMode::Stretch` は固定 hop の granular OLA で、grain の **配置**が
//! 長さを、grain 内部の**読み速度**が音程を決めていた。読み速度を変えると
//! スペクトル全体 (倍音列 *と* その包絡 = フォルマント) が同率で写るので、
//! 「ピッチを上げるとフォルマントも必ず一緒に上がる」= チップマンク化が
//! アルゴリズムの定義そのものだった。パラメータでは外せない。
//!
//! フォルマントをピッチから外すには周波数軸でスペクトル包絡を別に写す必要が
//! あり (位相ボコーダ + 包絡推定)、Stretch の DSP ごと差し替えるのが唯一の道。
//! 自前実装ではなく Signalsmith Stretch (MIT、Qt 6.10 の QMediaPlayer が採用) を
//! vendoring している (`signalsmith-sys/VENDOR.md`)。
//!
//! # ストリーム契約
//!
//! エンジンは **連続ストリーム**処理器で、DAW の「任意位置から鳴らす」用途とは
//! 素直に噛み合わない。ここが吸収する:
//!
//! - `key` (= 安定 clip id + audio event id) と「次に出るべき output sample」
//!   (`next_el`) が両方一致するときだけ継続。ズレたら [`sms_output_seek`] で
//!   パイプラインを詰め直す (= seek / loop / 新規発音 / schedule 再構築)。
//! - **レイテンシは 0 に見せる**。素材は全部メモリ上にあるので、出力位置より
//!   `input_latency + output_latency` ぶん先の入力を先読みして食わせられる
//!   (ライブ入力には無い DAW ならではの利点)。読み先の規約は上流の参照実装
//!   `cmd/main.cpp` と同じ:
//!   `fed = u_of(el + output_latency) + input_latency * du`。
//!
//! # RT 契約
//!
//! [`StretchEngine::new`] だけが確保する (C++ 側の warm-up 込み) ので、
//! **off-thread でしか呼ばない**。[`StretchEngine::render`] は確保・解放・
//! ロック・I/O をしない。

use signalsmith_sys as sys;

use common::process_data::MAX_FRAMES;

/// 移調時に「この周波数より上はあまり動かさない」境界 (Hz)。上流 CLI
/// (`cmd/main.cpp` の `--tonality` 既定) と同値。素材の空気感 (シンバル等) が
/// 移調でごっそりずれるのを抑える。
const TONALITY_LIMIT_HZ: f32 = 8000.0;

/// 1 回の [`StretchEngine::render`] で食わせる入力の上限を決める係数。
/// 出力 1 buffer (`MAX_FRAMES`) に対しこの倍率までは分割せず 1 回で処理する
/// (= clip を 4 倍速に詰めた状態まで)。超える比は出力側を分割して対応するので
/// 上限を超えても音は正しい (`render` の chunk ループ)。
const INPUT_RATE_HEADROOM: usize = 4;

/// prime (`sms_output_seek`) に必要な入力長 = `input_latency + rate *
/// output_latency` の `rate` 上限。これを超える再生比では prime 入力が
/// 切り詰められ、頭が僅かにずれる (実用外の比なので許容)。
const PRIME_RATE_CAP: usize = 4;

/// 「同じ発音の続き」 と見なす `el` の前方ズレ上限 (サンプル)。
///
/// 呼び出し側 (`render_audio_events`) は event の描画範囲を beat → sample の
/// 浮動小数換算 + 切り捨てで出すので、buffer 境界で 1 サンプル取りこぼすことが
/// ある (`(511.9999999) as usize == 511`)。 これを不連続とみなすと **毎 buffer
/// パイプラインを詰め直す**ことになり、出力が痩せて時間写像も壊れる。
///
/// ズレは前後どちらにも出る (`event_start_offset_in_buf` も同じ換算で丸める
/// ため、`el_start` 自体が ±1 揺れる) が、`el_start` は毎 buffer playhead から
/// 出し直すので **累積しない**。 数サンプルで足りるところを、可変 buffer 長でも
/// 余裕を持つよう 64 サンプル取る。 微小なズレは入力量の目標が毎ブロック絶対値で
/// 計算し直されて自動で吸収され、時間ズレは残らない。 これより大きい跳びは
/// 本物の seek / loop / 別発音なので prime し直す。
const CONTINUITY_SLACK_SAMPLES: u64 = 64;

/// 走行中ストリームの同一性。
struct Stream {
    /// 発音中の audio event の安定キー。
    key: u64,
    /// 次に出力すべき event-local sample offset。
    next_el: u64,
    /// 次に engine へ食わせる中間サンプルの u 座標 (単位は呼び出し側の `du` 系)。
    cursor_u: f64,
}

/// 1 発音 (audio event) ぶんのスペクトルエンジン。
pub struct StretchEngine {
    raw: *mut sys::SmsStretch,
    sample_rate: u32,
    input_latency: u64,
    output_latency: u64,
    /// 入力読み出し scratch (planar)。`new` で確保し RT では詰め替えるだけ。
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    /// 現在エンジンに設定済みの値 (差分適用で無駄な再計算を避ける)。
    cur_transpose: f32,
    cur_formant: f32,
    cur_compensate: bool,
    stream: Option<Stream>,
}

// SAFETY: `raw` は本 struct が排他所有する C++ オブジェクトへのポインタ。
// エンジンはスレッドローカル状態も COM apartment 親和性も持たない (中身は
// std::vector と float 演算のみ) ので、所有権ごと別スレッドへ移してよい。
// `Sync` は付けない — 同時に 2 スレッドから触るのは未定義。
unsafe impl Send for StretchEngine {}

impl Drop for StretchEngine {
    fn drop(&mut self) {
        // SAFETY: `raw` は `sms_create` の戻り値で、ここでしか解放しない。
        unsafe { sys::sms_destroy(self.raw) };
    }
}

impl StretchEngine {
    /// エンジンを 1 個確保する。**off-RT 専用** (内部で確保 + noise warm-up)。
    /// `None` = 確保失敗 (OOM)。
    pub fn new(sample_rate: u32) -> Option<Self> {
        if sample_rate == 0 {
            return None;
        }
        // SAFETY: 単なる確保。戻り値の null を下でチェックする。
        let raw = unsafe { sys::sms_create(sample_rate as f32) };
        if raw.is_null() {
            return None;
        }
        // SAFETY: `raw` は非 null な有効ハンドル。
        let input_latency = unsafe { sys::sms_input_latency(raw) }.max(0) as u64;
        // SAFETY: 同上。
        let output_latency = unsafe { sys::sms_output_latency(raw) }.max(0) as u64;

        // scratch は「1 buffer ぶんの通常処理」と「prime 1 回ぶん」の両方を
        // 賄えるだけ取る (どちらも同じ scratch を使い回す)。
        let block_cap = MAX_FRAMES * INPUT_RATE_HEADROOM;
        let prime_cap = (input_latency as usize) + (output_latency as usize) * PRIME_RATE_CAP;
        let cap = block_cap.max(prime_cap) + 64;

        Some(Self {
            raw,
            sample_rate,
            input_latency,
            output_latency,
            in_l: vec![0.0; cap],
            in_r: vec![0.0; cap],
            // C++ 側 warm-up の後始末で 0 / 補正なしに戻してある。
            cur_transpose: 0.0,
            cur_formant: 0.0,
            cur_compensate: false,
            stream: None,
        })
    }

    /// パラメータを差分適用する。値が動かないフレームでは FFI も `pow` も走らない。
    fn apply_params(&mut self, transpose: f32, formant: f32, compensate: bool) {
        if self.cur_transpose != transpose {
            self.cur_transpose = transpose;
            let limit = if transpose == 0.0 {
                0.0
            } else {
                TONALITY_LIMIT_HZ / self.sample_rate as f32
            };
            // SAFETY: 有効ハンドル + スカラ引数。
            unsafe { sys::sms_set_transpose_semitones(self.raw, transpose, limit) };
        }
        if self.cur_formant != formant || self.cur_compensate != compensate {
            self.cur_formant = formant;
            self.cur_compensate = compensate;
            // SAFETY: 同上。
            unsafe {
                sys::sms_set_formant_semitones(self.raw, formant, i32::from(compensate));
            }
        }
    }

    /// この event の出力を `out_l` / `out_r` に **上書き**で書く (加算ではない)。
    ///
    /// - `key`: 発音の安定キー。変わったら別発音として prime し直す。
    /// - `el_start`: `out_*[0]` に対応する event-local sample offset。
    /// - `du`: 中間ストリーム 1 サンプルあたりの `u` 増分 (spectral 経路では
    ///   source SR 比 `time_stride`、テープ経路では `1.0`)。
    /// - `u_of(el)`: output sample `el` を出すのに必要な中間位置。
    /// - `fetch(u)`: 中間位置 `u` の素材サンプル (範囲外は無音を返すこと)。
    ///
    /// RT-safe: 確保・ロック・panic なし。
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        key: u64,
        transpose_semitones: f32,
        formant_semitones: f32,
        compensate_pitch: bool,
        el_start: u64,
        du: f64,
        u_of: impl Fn(u64) -> f64,
        mut fetch: impl FnMut(f64) -> (f32, f32),
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) {
        let n_out = out_l.len().min(out_r.len());
        if n_out == 0 {
            return;
        }
        // 退化した写像 (0 / 負 / NaN) はここで打ち切る。無音を書いて
        // ストリームも捨てる (次で prime し直す)。
        if !du.is_finite() || du <= 0.0 || !u_of(el_start).is_finite() {
            out_l[..n_out].fill(0.0);
            out_r[..n_out].fill(0.0);
            self.stream = None;
            return;
        }

        self.apply_params(transpose_semitones, formant_semitones, compensate_pitch);

        let continuous = self.stream.as_ref().is_some_and(|s| {
            s.key == key && el_start.abs_diff(s.next_el) <= CONTINUITY_SLACK_SAMPLES
        });
        if !continuous {
            self.prime(key, el_start, du, &u_of, &mut fetch);
        }

        let cap = self.in_l.len();
        let mut done = 0usize;
        while done < n_out {
            // この chunk を 1 回の process で賄えるところまで縮める
            // (再生比が高いほど必要入力が増えるので、出力側を分割する)。
            let mut chunk = n_out - done;
            let n_in = loop {
                let el_end = el_start.saturating_add((done + chunk) as u64);
                let need = self.input_needed(el_end, du, &u_of);
                if need <= cap || chunk == 1 {
                    break need.min(cap);
                }
                chunk = chunk.div_ceil(2);
            };

            let Some(cursor_u) = self.stream.as_ref().map(|s| s.cursor_u) else {
                // prime が走らなかった (= ストリーム不在) ケースの防御。
                out_l[done..n_out].fill(0.0);
                out_r[done..n_out].fill(0.0);
                return;
            };
            let mut u = cursor_u;
            for i in 0..n_in {
                let (l, r) = fetch(u);
                self.in_l[i] = l;
                self.in_r[i] = r;
                u += du;
            }
            if let Some(stream) = self.stream.as_mut() {
                stream.cursor_u = cursor_u + n_in as f64 * du;
            }

            // SAFETY: `in_*` は `n_in <= cap` 個の有効な要素を持ち、`out_*` の
            // slice は `chunk` 個ぶん書ける。ハンドルは有効。
            unsafe {
                sys::sms_process(
                    self.raw,
                    self.in_l.as_ptr(),
                    self.in_r.as_ptr(),
                    n_in as i32,
                    out_l[done..done + chunk].as_mut_ptr(),
                    out_r[done..done + chunk].as_mut_ptr(),
                    chunk as i32,
                );
            }
            done += chunk;
        }

        if let Some(stream) = self.stream.as_mut() {
            stream.next_el = el_start.saturating_add(n_out as u64);
        }
    }

    /// `el_end` ぶんの出力を出し終えた時点で食わせ終えているべき入力量 -
    /// 既に食わせた量。上流参照実装 `cmd/main.cpp` の
    /// `inputIndex = inputPos + inputLatency` と同じ規約。
    fn input_needed(&self, el_end: u64, du: f64, u_of: &impl Fn(u64) -> f64) -> usize {
        let Some(stream) = self.stream.as_ref() else {
            return 0;
        };
        let target = u_of(el_end.saturating_add(self.output_latency)) + self.input_latency as f64 * du;
        if !target.is_finite() {
            return 0;
        }
        let need = (target - stream.cursor_u) / du;
        if !need.is_finite() || need <= 0.0 {
            return 0;
        }
        // 端数は次 buffer に持ち越す (`cursor_u` は実際に食わせた量で進めるので
        // 丸め誤差が累積せず、毎 buffer 目標との差から求め直される)。
        need.round() as usize
    }

    /// パイプラインを `el_start` に合わせて詰め直す。`sms_output_seek` は
    /// 内部で `reset` + 先読み分の出力を打ち消すので、この直後の
    /// [`render`](Self::render) 出力は `el_start` ちょうどから始まる
    /// (= 立ち上がりが痩せない)。
    fn prime(
        &mut self,
        key: u64,
        el_start: u64,
        du: f64,
        u_of: &impl Fn(u64) -> f64,
        fetch: &mut impl FnMut(f64) -> (f32, f32),
    ) {
        let u0 = u_of(el_start);
        // prime 中に進む中間サンプル数 = 先読み (output_latency ぶんの出力を
        // 内部で作って打ち消す) + analysis 履歴 (input_latency)。
        let surplus_u = u_of(el_start.saturating_add(self.output_latency)) - u0;
        // 退化した clip (長さ ~0 拍 = 伸縮比が爆発) だと `surplus_u / du` が
        // 桁外れになる。 float→int cast は飽和するので `usize::MAX` になり得るが、
        // その後の加算まで飽和させないと **RT スレッドで overflow panic** になる。
        let surplus = if surplus_u.is_finite() && surplus_u > 0.0 {
            (surplus_u / du).round() as usize
        } else {
            0
        };
        let n = (self.input_latency as usize)
            .saturating_add(surplus)
            .clamp(1, self.in_l.len());

        let mut u = u0;
        for i in 0..n {
            let (l, r) = fetch(u);
            self.in_l[i] = l;
            self.in_r[i] = r;
            u += du;
        }
        // SAFETY: `in_*` は `n <= len` 個の有効な要素を持つ。ハンドルは有効。
        unsafe {
            sys::sms_output_seek(self.raw, self.in_l.as_ptr(), self.in_r.as_ptr(), n as i32);
        }
        self.stream = Some(Stream {
            key,
            next_el: el_start,
            cursor_u: u0 + n as f64 * du,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    /// 素材を「event-local sample → 値」の純関数で与えて 1:1 (等速・移調なし)
    /// で回す共通ハーネス。`fetch` は `u` (= event_local) をそのまま使う。
    fn render_identity(
        engine: &mut StretchEngine,
        src: &[f32],
        n_out: usize,
        transpose: f32,
        formant: f32,
        compensate: bool,
        block: usize,
    ) -> Vec<f32> {
        let mut out = vec![0.0f32; n_out];
        let mut pos = 0usize;
        while pos < n_out {
            let len = block.min(n_out - pos);
            let mut l = vec![0.0f32; len];
            let mut r = vec![0.0f32; len];
            engine.render(
                1,
                transpose,
                formant,
                compensate,
                pos as u64,
                1.0,
                |el| el as f64,
                |u| {
                    let i = u as usize;
                    let v = src.get(i).copied().unwrap_or(0.0);
                    (v, v)
                },
                &mut l,
                &mut r,
            );
            out[pos..pos + len].copy_from_slice(&l);
            pos += len;
        }
        out
    }

    fn sine(freq: f64, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                (std::f64::consts::TAU * freq * i as f64 / f64::from(SR)).sin() as f32 * 0.5
            })
            .collect()
    }

    /// 素材の第 1 フォルマント (= 包絡の山) の位置。`f0` = 200 / 400 のどちらの
    /// 倍音格子にも乗る値を選んであるので、移調前後で同じ指標が使える。
    const VOWEL_F1_HZ: f64 = 800.0;

    /// 共振ピーク 2 本 (`VOWEL_F1_HZ` / 1600 Hz) を持つ合成母音。倍音は `f0` の
    /// 整数倍に立ち、その **振幅**が包絡 (= フォルマント) を作る。倍音位置が
    /// 「音程」、振幅の山の位置が「声質」で、この 2 つが独立に動くかを測る素材。
    fn vowel(f0: f64, n: usize) -> Vec<f32> {
        let env = |f: f64| -> f64 {
            1.0 / (1.0 + ((f - VOWEL_F1_HZ) / 110.0).powi(2))
                + 0.45 / (1.0 + ((f - 1600.0) / 160.0).powi(2))
                + 0.02
        };
        let harmonics: Vec<(f64, f64)> = (1..=30)
            .map(|k| (f0 * k as f64, env(f0 * k as f64)))
            .filter(|(f, _)| *f < f64::from(SR) / 2.0 * 0.8)
            .collect();
        let norm: f64 = harmonics.iter().map(|(_, a)| a).sum::<f64>().max(1e-9);
        (0..n)
            .map(|i| {
                let t = i as f64 / f64::from(SR);
                let v: f64 = harmonics
                    .iter()
                    .map(|(f, a)| a * (std::f64::consts::TAU * f * t).sin())
                    .sum();
                (v / norm * 0.8) as f32
            })
            .collect()
    }

    /// `f0` の倍音のうち最も強いものの周波数 = スペクトル包絡の山の位置。
    fn envelope_peak_hz(x: &[f32], f0: f64) -> f64 {
        let mut best = (0.0f64, 0.0f64);
        let mut k = 1u32;
        while (f0 * f64::from(k)) < 4000.0 {
            let f = f0 * f64::from(k);
            let m = magnitude_at(x, f);
            if m > best.1 {
                best = (f, m);
            }
            k += 1;
        }
        best.0
    }

    /// Goertzel: `freq` 成分の振幅 (窓なし、十分長い区間で使う)。
    fn magnitude_at(x: &[f32], freq: f64) -> f64 {
        let w = std::f64::consts::TAU * freq / f64::from(SR);
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &v in x {
            let s0 = f64::from(v) + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0).sqrt() / x.len() as f64
    }

    #[test]
    fn create_reports_positive_latencies() {
        let e = StretchEngine::new(SR).expect("engine");
        assert!(e.input_latency > 0, "input latency: {}", e.input_latency);
        assert!(e.output_latency > 0, "output latency: {}", e.output_latency);
    }

    /// `sms_output_seek` の規約 = 「次の process 出力が素材の先頭」。 これが
    /// ずれると clip の頭が遅れて鳴る (= vendored ヘッダ更新時の回帰検出)。
    /// 素材は 1 秒の 440 Hz サイン。移調も伸縮もしない (= 恒等) 経路で、
    /// 出力が入力と位相まで揃うことを相関で確認する。
    #[test]
    fn output_seek_aligns_output_to_stream_start() {
        let mut e = StretchEngine::new(SR).expect("engine");
        let n = SR as usize;
        let src = sine(440.0, n);
        let out = render_identity(&mut e, &src, n, 0.0, 0.0, false, 1024);

        // 立ち上がり (最初の 1 block) を含む区間で、遅延 0 の相関が最大になること。
        let probe = 4096usize;
        let corr = |lag: usize| -> f64 {
            (0..probe)
                .map(|i| f64::from(out[i + lag]) * f64::from(src[i]))
                .sum::<f64>()
        };
        let zero = corr(0);
        assert!(zero > 0.0, "恒等経路で出力が素材と逆相関/無音: {zero}");
        for lag in [64usize, 256, 1024, 4096] {
            assert!(
                corr(lag) < zero,
                "lag {lag} の相関 {} が lag 0 の {zero} 以上 = 出力が遅れている",
                corr(lag)
            );
        }
    }

    /// 移調は基本周波数を動かす。+12 半音で 440 Hz → 880 Hz。
    #[test]
    fn transpose_moves_the_fundamental() {
        let mut e = StretchEngine::new(SR).expect("engine");
        let n = SR as usize;
        let src = sine(440.0, n);
        let out = render_identity(&mut e, &src, n, 12.0, 0.0, false, 1024);
        // 過渡を避けて後半だけ測る。
        let tail = &out[n / 2..];
        let m880 = magnitude_at(tail, 880.0);
        let m440 = magnitude_at(tail, 440.0);
        assert!(
            m880 > m440 * 4.0,
            "+12 半音で 880 Hz が主成分になるべき: 880={m880} 440={m440}"
        );
    }

    /// **r.md #40 の core**: ピッチを上げても声質 (スペクトル包絡の山) が動かない。
    /// = Ableton Complex Pro の Formants=100% / Cubase VariAudio / Melodyne 流。
    /// 合成母音 (F0=200 Hz、共振 700/1200 Hz) を +12 半音移調し、
    /// 「倍音は 2 倍になるが包絡の山は 700 Hz 付近のまま」を測る。
    #[test]
    fn pitch_shift_preserves_the_spectral_envelope() {
        let n = SR as usize;
        let src = vowel(200.0, n);

        let mut plain = StretchEngine::new(SR).expect("engine");
        let dry = render_identity(&mut plain, &src, n, 0.0, 0.0, true, 1024);
        let mut up = StretchEngine::new(SR).expect("engine");
        let out = render_identity(&mut up, &src, n, 12.0, 0.0, true, 1024);

        let dry_tail = &dry[n / 2..];
        let tail = &out[n / 2..];

        // 音程は 1 オクターブ上がった (F0 200 → 400、200 Hz の倍音は消える)。
        let m200 = magnitude_at(tail, 200.0);
        let m400 = magnitude_at(tail, 400.0);
        assert!(
            m400 > m200 * 4.0,
            "+12 半音で F0 は 400 Hz になるべき: 400={m400} 200={m200}"
        );

        // 声質 (包絡の山) は動いていない。移調前の山と ±1 倍音以内で一致。
        let dry_peak = envelope_peak_hz(dry_tail, 200.0);
        let up_peak = envelope_peak_hz(tail, 400.0);
        assert!(
            (dry_peak - VOWEL_F1_HZ).abs() <= 200.0,
            "素材の包絡の山は {VOWEL_F1_HZ} Hz 付近のはず: {dry_peak}"
        );
        assert!(
            (up_peak - dry_peak).abs() <= 400.0,
            "移調しても包絡の山は動かないべき: 移調前 {dry_peak} Hz → 移調後 {up_peak} Hz"
        );
    }

    /// フォルマント指定はスペクトル包絡だけを動かし、音程 (倍音の位置) は
    /// 変えない。+12 半音で山が 700 Hz → 1400 Hz 付近へ、F0 は 200 Hz のまま。
    #[test]
    fn formant_shift_moves_the_envelope_but_not_the_pitch() {
        let n = SR as usize;
        let src = vowel(200.0, n);

        let mut plain = StretchEngine::new(SR).expect("engine");
        let dry = render_identity(&mut plain, &src, n, 0.0, 0.0, false, 1024);
        let mut shifted = StretchEngine::new(SR).expect("engine");
        let out = render_identity(&mut shifted, &src, n, 0.0, 12.0, false, 1024);

        let dry_peak = envelope_peak_hz(&dry[n / 2..], 200.0);
        let up_peak = envelope_peak_hz(&out[n / 2..], 200.0);
        assert!(
            up_peak > dry_peak * 1.5,
            "+12 半音のフォルマントで包絡の山は上がるべき: {dry_peak} Hz → {up_peak} Hz"
        );

        // 音程は不動: 倍音は 200 Hz の整数倍のまま (非倍音 300 Hz は立たない)。
        let tail = &out[n / 2..];
        let m200 = magnitude_at(tail, 200.0);
        let m300 = magnitude_at(tail, 300.0);
        assert!(
            m200 > m300 * 4.0,
            "フォルマントを動かしても倍音格子は 200 Hz 基準のまま: 200={m200} 300={m300}"
        );
    }

    /// buffer 分割の仕方を変えても出力が一致する (= ストリーム継続が
    /// buffer 境界に依存しない)。live 再生と export の一致条件でもある。
    #[test]
    fn output_is_independent_of_buffer_size() {
        let n = 24_000usize;
        let src = sine(330.0, n);
        let mut a = StretchEngine::new(SR).expect("engine");
        let mut b = StretchEngine::new(SR).expect("engine");
        let out_a = render_identity(&mut a, &src, n, 5.0, 3.0, true, 1024);
        let out_b = render_identity(&mut b, &src, n, 5.0, 3.0, true, 256);
        for (i, (x, y)) in out_a.iter().zip(out_b.iter()).enumerate() {
            assert!(
                (x - y).abs() < 1e-5,
                "buffer 分割で出力が変わった at {i}: {x} vs {y}"
            );
        }
    }
}
