//! Format 非依存の process() 骨格 (`docs/plan_arch_refactor.md` §6 B8)。
//!
//! CLAP / VST3 の `process()` はどちらも「入力 copy → aux 入力 copy →
//! channel pointer 更新 → bus 配列 assembly → transport 導出 → modulation
//! folding → FFI 呼び出し」の同型骨格で、旧実装は両 backend にほぼ字面一致
//! のコードが並んでいた。ここに format 非依存の部分を一本化し、backend は
//! 「scaffold → FFI 型への写像 + 呼び出し」だけを持つ。
//!
//! すべて RT パス (worker thread) から呼ばれる: **ヒープ確保・ロック・I/O
//! 禁止**。バッファは activate 時に確保済みのものを埋めるだけ。

use std::collections::HashMap;

use crate::plugin_instance::{AuxInputBuf, ParamEventKind, TimedParamEvent, TransportContext};

// ====================================================================
// planar buffer copy (入力 copy / aux 入力 copy / pointer refresh)
// ====================================================================

/// activate 時の planar buffer 確保 (channel 数 × max_frames)。
/// 戻り値は `(buffers, ptr_scratch)`。main-thread (quiesced window) 専用。
pub fn alloc_planar(channels: usize, max_frames: usize) -> (Vec<Vec<f32>>, Vec<*mut f32>) {
    let bufs = (0..channels).map(|_| vec![0.0f32; max_frames]).collect();
    let ptrs = vec![std::ptr::null_mut(); channels];
    (bufs, ptrs)
}

/// activate 時の per-port planar buffer 確保 (port × channel × max_frames)。
pub fn alloc_planar_ports(
    port_channels: &[u32],
    max_frames: usize,
) -> (Vec<Vec<Vec<f32>>>, Vec<Vec<*mut f32>>) {
    let bufs = port_channels
        .iter()
        .map(|&ch| (0..ch as usize).map(|_| vec![0.0f32; max_frames]).collect())
        .collect();
    let ptrs = port_channels
        .iter()
        .map(|&ch| vec![std::ptr::null_mut(); ch as usize])
        .collect();
    (bufs, ptrs)
}

/// caller 提供の入力 audio を pre-allocated planar buffer へ copy する。
/// 供給されない channel / 足りない末尾は 0 fill (= plugin に stale data を
/// 見せない)。両 backend の入力 copy (旧 clap_plugin.rs:944 ⇔
/// vst3_plugin.rs:1296) の一本化。
pub fn copy_input_planar(bufs: &mut [Vec<f32>], input_audio: &[&[f32]], n: usize) {
    for (ch, buf) in bufs.iter_mut().enumerate() {
        let cap = n.min(buf.len());
        if let Some(src) = input_audio.get(ch) {
            let copy_n = cap.min(src.len());
            buf[..copy_n].copy_from_slice(&src[..copy_n]);
            if copy_n < cap {
                buf[copy_n..cap].fill(0.0);
            }
        } else {
            buf[..cap].fill(0.0);
        }
    }
}

/// aux 入力 (sidechain) を per-port planar buffer へ copy する。inactive な
/// port / 2ch 超の channel は無音。
pub fn copy_aux_inputs_planar(
    port_bufs: &mut [Vec<Vec<f32>>],
    aux_inputs: &[AuxInputBuf<'_>],
    n: usize,
) {
    for (port_idx, bufs) in port_bufs.iter_mut().enumerate() {
        let aux = aux_inputs.get(port_idx).copied();
        for (ch, buf) in bufs.iter_mut().enumerate() {
            let cap = n.min(buf.len());
            let src: &[f32] = match (aux, ch) {
                (Some(a), 0) if a.active => a.l,
                (Some(a), 1) if a.active => a.r,
                _ => &[],
            };
            let copy_n = cap.min(src.len());
            buf[..copy_n].copy_from_slice(&src[..copy_n]);
            if copy_n < cap {
                buf[copy_n..cap].fill(0.0);
            }
        }
    }
}

/// channel pointer scratch を planar buffer の現在の base pointer で更新。
pub fn refresh_ptrs(bufs: &mut [Vec<f32>], ptrs: &mut [*mut f32]) {
    for (i, buf) in bufs.iter_mut().enumerate() {
        ptrs[i] = buf.as_mut_ptr();
    }
}

/// per-port 版 [`refresh_ptrs`]。
pub fn refresh_ptrs_ports(port_bufs: &mut [Vec<Vec<f32>>], port_ptrs: &mut [Vec<*mut f32>]) {
    for (port_idx, bufs) in port_bufs.iter_mut().enumerate() {
        for (ch, buf) in bufs.iter_mut().enumerate() {
            port_ptrs[port_idx][ch] = buf.as_mut_ptr();
        }
    }
}

// ====================================================================
// transport 導出 (bar_number / bar_start / song_pos — 非有限 sanitize 込み)
// ====================================================================

/// 非有限 (NaN / inf) を 0 に倒す。timeline 値を整数化 / FFI へ渡す前段。
#[inline]
fn sanitize_pos(v: f64) -> f64 {
    if v.is_finite() { v } else { 0.0 }
}

/// format 非依存の transport 導出結果。CLAP は `clap_event_transport` へ、
/// VST3 は `ProcessContext` へ写像する。**両 format とも非有限 sanitize 済み
/// の値だけを見る** (旧実装は CLAP のみ sanitize し VST3 は
/// `projectTimeMusic` を無検査で渡す非対称だった)。
///
/// r.md #87: ここに載る拍は **plugin が載っている行の musical time**
/// ([`TransportContext::row`]) であって曲全体の位置ではない。ランチャーが
/// 行の主導権を握っている間、その行のテンポ同期ディレイ / LFO / アルペジエータは
/// セルの拍で動く (= 聞こえているセルとグリッドが揃う)。アレンジ主導の行では
/// 両者が一致するので、ランチャーを使わない曲の挙動は従来と同一。
#[derive(Debug, Clone, Copy)]
pub struct TransportBlock {
    pub bpm: f64,
    /// この行のタイムライン上の拍位置 (sanitized)。CLAP
    /// `clap_event_transport.song_pos_beats` / VST3 `projectTimeMusic` へ入る。
    pub pos_beats: f64,
    /// 同じ位置の秒表現 (= beats × 60 / bpm、sanitized)。engine は
    /// `steady_time` を設定しないので sample 由来の秒は使えない。
    pub pos_seconds: f64,
    /// 同じ位置の sample 表現 (= seconds × sample_rate、非負)。
    pub pos_samples: i64,
    /// 現在の小節 index (floor(beats / tsig_num))。
    pub bar_number: f64,
    /// 小節頭の拍位置。
    pub bar_start_beats: f64,
    pub tsig_num: u16,
    pub tsig_denom: u16,
    pub is_playing: bool,
    /// loop toggle on かつ有効な範囲 (end > start) があるか。
    pub cycle_active: bool,
    pub loop_start_beats: f64,
    pub loop_end_beats: f64,
}

impl TransportBlock {
    /// `sample_rate` は backend の activate 時レート (Hz)。
    pub fn derive(t: &TransportContext, sample_rate: f64) -> Self {
        let bpm = f64::from(t.bpm).max(1.0);
        // r.md #87: 位置もループも「この行の時間軸」から 1 度に取る
        // (アレンジ主導の行 / ARA では曲の値と同値)。
        let tl = t.plugin_timeline();
        let pos_beats = sanitize_pos(tl.pos_beats);
        let pos_seconds = sanitize_pos(pos_beats * 60.0 / bpm);
        let pos_samples = (pos_seconds * sample_rate.max(0.0)).max(0.0) as i64;
        let tsig_num = t.tsig_num.max(1);
        let bar_number = (pos_beats / f64::from(tsig_num)).floor();
        let bar_start_beats = bar_number * f64::from(tsig_num);
        let loop_start_beats = sanitize_pos(tl.loop_start_beats);
        let loop_end_beats = sanitize_pos(tl.loop_end_beats);
        Self {
            bpm,
            pos_beats,
            pos_seconds,
            pos_samples,
            bar_number,
            bar_start_beats,
            tsig_num,
            tsig_denom: t.tsig_denom.max(1),
            is_playing: t.is_playing,
            cycle_active: tl.is_looping && loop_end_beats > loop_start_beats,
            loop_start_beats,
            loop_end_beats,
        }
    }
}

// ====================================================================
// modulation folding (base cache pre-pass + fold)
// ====================================================================

/// Pre-pass: この buffer の絶対 Value events で base-value cache を更新する。
/// 後段の Mod folding が (unstable な time sort 順に依存せず) 最新 base を
/// 見られるようにする。cache は load 時に全 param の default で seed 済み
/// なので、ここでは既存 key の更新のみ (= RT heap alloc なし)。
/// r.md #89: **1 イベントぶんだけ** base を進める。
///
/// buffer 全体を先に畳んでから Mod を畳むと、どの刻みの Mod も「buffer 末の
/// automation 値」を base にする。automation と変調を同じ param に併用すると、
/// プラグインには「その時刻の automation 値」と「buffer 末 + 変調」が刻みごとに
/// 交互に届く = 48kHz で 750Hz のジッパーになる。イベント列を時刻順に舐めながら
/// この関数で進めれば、Mod は **その時刻までの** automation 値に畳まれる。
#[inline]
pub fn advance_param_base(cache: &mut HashMap<u32, f64>, ev: &TimedParamEvent) {
    if ev.kind == ParamEventKind::Value
        && let Some(slot) = cache.get_mut(&ev.param_id)
    {
        *slot = ev.value;
    }
}

/// Mod offset を絶対値へ畳む: `clamp(base + offset_scaled, min, max)`。
/// CLAP 非 modulatable param は plain 単位 (`offset·(max−min)`)、VST3 は
/// normalized (`min=0, max=1, offset_scaled == offset`)。
#[inline]
pub fn fold_mod_offset(base: f64, offset_scaled: f64, min: f64, max: f64) -> f64 {
    (base + offset_scaled).clamp(min, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> TransportContext {
        TransportContext {
            bpm: 120.0,
            sample_rate: 48_000,
            song_pos_beats: 999.0,
            tsig_num: 4,
            tsig_denom: 4,
            is_playing: true,
            is_looping: false,
            loop_start_beats: 0.0,
            loop_end_beats: 0.0,
            // アレンジ主導の行 = 行の実効拍は song 拍と同値 (engine の写像)。
            row: common::process_data::RowTransport {
                pos_beats: 999.0,
                ..Default::default()
            },
            pin_to_song: false,
        }
    }

    #[test]
    fn transport_block_derives_seconds_and_bars_from_beats() {
        let b = TransportBlock::derive(&ctx(), 48_000.0);
        assert_eq!(b.pos_beats, 999.0);
        // 999 拍 ÷ (120bpm/60) = 499.5 秒。
        assert!((b.pos_seconds - 499.5).abs() < 1e-9);
        assert_eq!(b.pos_samples, (499.5 * 48_000.0) as i64);
        // 999 / 4 = 249.75 → bar 249、小節頭 = 996 拍。
        assert_eq!(b.bar_number, 249.0);
        assert_eq!(b.bar_start_beats, 996.0);
        assert!(!b.cycle_active);
    }

    /// r.md #87: ランチャーで撃った行の plugin は **セルの窓**で動く。位置だけ
    /// 行に切り替えて曲のループ区間を名乗ると「拍がループの外を回っている」
    /// 不整合になるので、位置とループを 1 つの timeline から取ることを固定する。
    #[test]
    fn a_launched_cell_row_uses_the_cell_window_not_the_song_loop() {
        let mut c = ctx();
        // 曲のループは 4..8 拍で on。
        c.loop_start_beats = 4.0;
        c.loop_end_beats = 8.0;
        c.is_looping = true;
        // 行は 2 拍長のセルを鳴らしていて、いまセル内 0.5 拍。
        c.row = common::process_data::RowTransport {
            pos_beats: 0.5,
            loop_start_beats: 0.0,
            loop_end_beats: 2.0,
            cell_clip_id: 7,
            ..Default::default()
        };
        let b = TransportBlock::derive(&c, 48_000.0);
        assert_eq!(b.pos_beats, 0.5);
        assert_eq!(b.loop_start_beats, 0.0);
        assert_eq!(b.loop_end_beats, 2.0);
        assert!(b.cycle_active);

        // ARA を bind した instance は曲の時間軸に固定 (playback region が song 時間)。
        c.pin_to_song = true;
        let b = TransportBlock::derive(&c, 48_000.0);
        assert_eq!(b.pos_beats, 999.0);
        assert_eq!(b.loop_start_beats, 4.0);
        assert_eq!(b.loop_end_beats, 8.0);
    }

    /// 非有限 sanitize は CLAP / VST3 共通でここ一箇所 (旧 VST3 は未検査)。
    #[test]
    fn transport_block_sanitizes_non_finite() {
        let mut c = ctx();
        c.row.pos_beats = f64::NAN;
        c.loop_start_beats = f64::INFINITY;
        c.is_looping = true;
        let b = TransportBlock::derive(&c, 48_000.0);
        assert_eq!(b.pos_beats, 0.0);
        assert_eq!(b.pos_seconds, 0.0);
        assert_eq!(b.pos_samples, 0);
        assert_eq!(b.loop_start_beats, 0.0);
        assert!(!b.cycle_active, "inf loop range must not activate cycle");
    }

    #[test]
    fn cycle_requires_toggle_and_range() {
        let mut c = ctx();
        c.is_looping = true;
        c.loop_start_beats = 4.0;
        c.loop_end_beats = 8.0;
        assert!(TransportBlock::derive(&c, 48_000.0).cycle_active);
        c.loop_end_beats = 4.0;
        assert!(!TransportBlock::derive(&c, 48_000.0).cycle_active);
    }

    #[test]
    fn base_cache_updates_existing_keys_only() {
        let mut cache = HashMap::from([(1u32, 0.5f64)]);
        let evs = [
            TimedParamEvent { time: 0, param_id: 1, value: 0.9, kind: ParamEventKind::Value },
            // 未知 param は insert しない (RT alloc 回避)。
            TimedParamEvent { time: 0, param_id: 2, value: 0.1, kind: ParamEventKind::Value },
            // Mod は base を動かさない。
            TimedParamEvent { time: 0, param_id: 1, value: -0.2, kind: ParamEventKind::Mod },
        ];
        for ev in &evs {
            advance_param_base(&mut cache, ev);
        }
        assert_eq!(cache.len(), 1);
        assert_eq!(cache[&1], 0.9);
    }

    /// r.md #89: **Mod は「その時刻までの」automation 値に畳む。**
    ///
    /// buffer 全体を先に畳んでから Mod を処理すると、どの刻みの Mod も buffer 末の
    /// automation 値を base にしてしまい、プラグインには「その時刻の automation 値」と
    /// 「buffer 末 + 変調」が刻みごとに交互に届く (= ジッパー)。
    #[test]
    fn modは同時刻までのautomation値に畳まれる() {
        let mut cache = HashMap::from([(1u32, 0.0f64)]);
        // 刻み 0 で automation 0.2、刻み 512 で 0.9。各刻みに Mod +0.05。
        let evs = [
            TimedParamEvent { time: 0, param_id: 1, value: 0.2, kind: ParamEventKind::Value },
            TimedParamEvent { time: 0, param_id: 1, value: 0.05, kind: ParamEventKind::Mod },
            TimedParamEvent { time: 512, param_id: 1, value: 0.9, kind: ParamEventKind::Value },
            TimedParamEvent { time: 512, param_id: 1, value: 0.05, kind: ParamEventKind::Mod },
        ];
        let mut folded = Vec::new();
        for ev in &evs {
            advance_param_base(&mut cache, ev);
            if ev.kind == ParamEventKind::Mod {
                folded.push(fold_mod_offset(cache[&1], ev.value, 0.0, 1.0));
            }
        }
        // 期待値: 刻み 0 は 0.2+0.05、刻み 512 は 0.9+0.05 (buffer 末の 0.9 を
        // 刻み 0 にも使っていた頃は [0.95, 0.95] になっていた)。
        assert!(
            (folded[0] - 0.25).abs() < 1e-9 && (folded[1] - 0.95).abs() < 1e-9,
            "各 Mod はその時刻の automation 値に乗る: {folded:?}"
        );
    }

    #[test]
    fn fold_clamps_into_range() {
        assert_eq!(fold_mod_offset(0.9, 0.5, 0.0, 1.0), 1.0);
        assert_eq!(fold_mod_offset(0.5, -1.0, 0.0, 1.0), 0.0);
        assert_eq!(fold_mod_offset(10.0, 5.0, 0.0, 20.0), 15.0);
    }
}
