//! 歌唱合成のレンダリング — 塊クエリ + フレーズ単位 `frame_synthesis` + 差分 mix。
//! 設計と実測は docs/plan_rmd_75_voicevox_phrase.md。
//!
//! **synth thread (非 RT) からだけ呼ばれる**。ここに RT 制約は無いのでヒープ確保・
//! ログ・HTTP すべて可。RT 側 (audio half) に足したのは quiescence epoch の
//! atomic RMW 2 回だけ。
//!
//! ## 位置計算はすべて「曲の sample 位置」1 つの空間で行う
//!
//! 塊クエリの frame 空間は「どの frame を切り出すか」を決めるためだけに使い、
//! 配置・継ぎ目・note offset には**一切使わない**。こうすると、キャッシュ hit で
//! 塊クエリを持っていないフレーズも miss のフレーズと**完全に同じ式**で置ける。

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;

use common::plugin_metadata::NoteMetadata;
use common::voicevox::{self, OUTPUT_SAMPLE_RATE, REST_FRAMES, frames_to_samples};
use common::voicevox_cache::{CacheKey, CacheKind, VoiceVoxDiskCache, key_for_sing_phrase,
    key_for_sing_query};
use common::voicevox_phrase::{
    Chunk, ChunkQuery, PHRASE_PAD_FRAMES, Phrase, build_chunk_query, group_into_chunks,
    split_into_phrases,
};

use super::voicevox::{SynthResult, TalkSynthSpec, sing_place_samples, talk_place_samples};
use super::voicevox_synth::{
    SynthError, decode_wav_to_f32, fetch_sing_frame_query, frame_query_len, frame_synthesis,
    slice_frame_query, synth_client, synthesize_talk_for_builtin,
};

/// フレーズの継ぎ目 (= 休符の中点) に入れるクロスフェードの**全長**。
/// 隣り合うフレーズは別々に合成されるので、継ぎ目に微小な不連続が残り得る。
/// 継ぎ目を中心に前後 `SEAM_XFADE_SECS / 2` が相補ランプになる。
const SEAM_XFADE_SECS: f64 = 0.005;

/// 完成 buffer を `ArcSwapOption` へ publish する最短間隔。
const PUBLISH_INTERVAL: Duration = Duration::from_millis(500);

/// 最終 publish で buffer プールの空きを待つ上限。超えたら `Vec` を 1 本新規確保する
/// (synth thread は非 RT なので確保してよい。ここで無限に待つと job が終わらず
/// `done_gen` が進まず、bounce / 書き出しが止まる)。
const FINAL_PUBLISH_WAIT: Duration = Duration::from_secs(1);

/// decode 済みフレーズ WAV の in-memory キャッシュ (synth thread が job をまたいで持つ)。
/// これが「1 ノート編集 = 未変更フレーズは decode すらしない」を成立させる。
///
/// 値は `(mono サンプル, decode 時の sample rate)`。rate も憶えるのは、engine が
/// 想定外の rate を返したとき (= 配置を再計算する経路) が **hit したときも同じ扱いに
/// なる**ようにするため (憶えないと 2 回目だけ 48 kHz として置かれ、症状が変わる)。
pub(super) type MemoryCache = HashMap<CacheKey, (Arc<Vec<f32>>, u32)>;

/// 1 フレーズの生の合成結果 (= キャッシュに入る単位。継ぎ目のトリムもフェードも
/// **掛けていない**)。トリム量は隣接フレーズの拍に依存するので、掛けてから
/// キャッシュすると隣を編集しただけで miss する。
struct RenderedPhrase {
    /// 合成 WAV を mono 化したもの。`Arc` はメモリキャッシュと共有する
    /// (= 同じ実体を 2 度持たない)。
    samples: Arc<Vec<f32>>,
    /// `samples[0]` が来る曲 sample 位置 (符号付き。曲頭より手前なら負)。
    place: i64,
    /// 継ぎ目トリム後に残す曲 sample 範囲 `[keep_start, keep_end)`。
    keep: Range<i64>,
    /// 先頭 / 末尾にフェードを掛けるか (= その側に隣接フレーズがあるか)。
    fade_in: bool,
    fade_out: bool,
}

/// フレーズ (または talk 発話) を曲 sample 空間に置くための情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Placement {
    /// WAV の sample 0 が来る曲 sample 位置 (符号付き)。
    pub place: i64,
    /// WAV の予測長 (sample)。実測長が違えば実測を採る。
    pub len: i64,
    /// 継ぎ目トリム後に残す曲 sample 範囲。
    pub keep: Range<i64>,
    pub fade_in: bool,
    pub fade_out: bool,
}

/// フレーズの配置と継ぎ目を **曲 sample 空間だけ**で決める純粋関数。
///
/// 継ぎ目は隣り合う 2 フレーズの拍だけから決める (`(prev.end_beat + next.start_beat) / 2`)
/// ので、**両隣が同じ 2 つの拍から同じ値を出す** = 敷き詰めは定義上ぴったり合う。
/// 塊をまたぐ継ぎ目も同じ扱い (塊境界だけ特別扱いすると、そこだけ二重発音 / 欠落する)。
///
/// `span_frames` は単体 query の「先頭 note の開始 〜 末尾 note の終端」frame 数。
#[must_use]
pub(super) fn phrase_window(
    start_beat: f64,
    end_beat: f64,
    span_frames: i64,
    prev_end_beat: Option<f64>,
    next_start_beat: Option<f64>,
    bpm: f32,
    sr: u32,
) -> Placement {
    let spb = f64::from(sr) * 60.0 / f64::from(bpm.max(0.001));
    // 単体 query の frame 0 が来る曲 sample (負にもなる)。
    let head = sing_place_samples(start_beat, bpm, sr);
    let rest_s = frames_to_samples(f64::from(REST_FRAMES), sr).round() as i64;
    let pad_s = frames_to_samples(PHRASE_PAD_FRAMES as f64, sr).round() as i64;
    // 合成 WAV の sample 0 = 「先頭 note の pad 手前」。塊クエリの端 rest を
    // PHRASE_PAD_FRAMES にしたので、これはどのフレーズでも成立する (クランプ不要)。
    //
    // 位置は f64 → i64 の飽和キャスト由来なので、壊れた project の巨大な `start_beat`
    // でも panic しないよう saturating で回す (旧 `mix_placed_groups` と同じ配慮)。
    let place = head.saturating_add(rest_s).saturating_sub(pad_s);
    let len = frames_to_samples(
        (span_frames.saturating_add(2 * PHRASE_PAD_FRAMES)) as f64,
        sr,
    )
    .round() as i64;
    let end = place.saturating_add(len);

    let seam = |a: f64, b: f64| (((a + b) * 0.5) * spb).round() as i64;
    let keep_start = prev_end_beat.map_or(place, |pe| seam(pe, start_beat).max(place));
    let keep_end = next_start_beat.map_or(end, |ns| seam(end_beat, ns).min(end));
    Placement {
        place,
        len,
        keep: keep_start..keep_end,
        fade_in: keep_start > place,
        fade_out: keep_end < end,
    }
}

/// r.md #87: `place` から書き始める item が、どの sample で打ち切られるか。
///
/// `bounds` は昇順のセル区間の境界 (= 各セルの中身が始まる sample)。自分の開始位置
/// より後ろにある最初の境界がそれ。境界が無ければ [`i64::MAX`] (= 無制限)。
/// **アレンジの item も同じ関数で解ける** — 開始位置が全境界より手前なので先頭の
/// 境界が返る (= セル区間の手前で止まる)。
#[must_use]
fn region_end(bounds: &[i64], place: i64) -> i64 {
    bounds.iter().copied().find(|&b| b > place).unwrap_or(i64::MAX)
}

/// クロスフェードの半幅 (sample)。継ぎ目を中心に前後この長さのランプになる。
fn xfade_half(sr: u32) -> i64 {
    (SEAM_XFADE_SECS * 0.5 * f64::from(sr)).round().max(1.0) as i64
}

/// 曲 sample 位置 `t` におけるこのフレーズのゲイン。継ぎ目を中心とする長さ `2 * xf` の
/// 線形ランプで、隣り合うフレーズは同じ窓で相補になるので和は 1。
fn seam_gain(t: i64, keep: &Range<i64>, fade_in: bool, fade_out: bool, xf: i64) -> f32 {
    let span = (2 * xf) as f32;
    let mut g = 1.0f32;
    if fade_in {
        g *= (((t - (keep.start - xf)) as f32) / span).clamp(0.0, 1.0);
    }
    if fade_out {
        g *= ((((keep.end + xf) - t) as f32) / span).clamp(0.0, 1.0);
    }
    g
}

/// 1 本の `RenderedPhrase` を song-absolute buffer へ加算する。
///
/// `place` が負なら曲頭より手前を切り落とす (**ずらさない**)。重なりは加算。
/// 書く範囲は `keep` を中心に、フェードのある側だけ `xf` はみ出す
/// (= 継ぎ目を挟んで相補ランプが重なる区間。ここを書かないと和が 1 にならない)。
///
/// WAV が「継ぎ目 + `xf`」まで届かない (= 休符がちょうど `2 * PHRASE_PAD` 付近) ときだけ
/// ランプが窓の途中で切れて和が 1 未満になり得る。そこは元々ほぼ無音なので可聴にならない。
fn mix_phrase(buf: &mut Vec<f32>, r: &RenderedPhrase, xf: i64) {
    let wav_len = r.samples.len() as i64;
    if wav_len == 0 {
        return;
    }
    // 位置は f64 → i64 の飽和キャスト由来なので saturating で回す (壊れた project の
    // 巨大な start_beat でも panic させない)。
    let lo = if r.fade_in {
        r.keep.start.saturating_sub(xf)
    } else {
        r.keep.start
    }
    .max(r.place)
    .max(0);
    let hi = if r.fade_out {
        r.keep.end.saturating_add(xf)
    } else {
        r.keep.end
    }
    .min(r.place.saturating_add(wav_len));
    if hi <= lo {
        return;
    }
    if buf.len() < hi as usize {
        buf.resize(hi as usize, 0.0);
    }
    for t in lo..hi {
        let src = (t - r.place) as usize;
        let g = seam_gain(t, &r.keep, r.fade_in, r.fade_out, xf);
        buf[t as usize] += r.samples[src] * g;
    }
}

/// 公開中の buffer を `None` に戻す (= 無音)。**`ArcSwapOption::store(None)` を直接
/// 呼んではいけない。**
///
/// arc-swap 1.9.1 の `store` は `drop(self.swap(val))` (`src/lib.rs`) で、`Debt::pay_all`
/// の直後に **writer 自身が旧 `Arc` を即座に drop する**。store と同時に走っていた
/// `process()` の `Guard` は debt を払われて所有者になっているので、そのまま RT が
/// 最後の所有者になり、**audio thread 上で数十 MB の解放**が起きる
/// ([`PhraseRenderState::reclaim`] の doc と同じ理由)。
///
/// ここでは `swap` で旧 `Arc` を手元に取り、RT の quiesce を確認して
/// [`Arc::try_unwrap`] が通ってから (= writer が唯一の所有者になってから) 解放する。
/// 待ちは有界 (`FINAL_PUBLISH_WAIT`)。**非 RT スレッドからのみ呼ぶこと。**
pub(super) fn clear_published(result_arc: &ArcSwapOption<SynthResult>, rt_epoch: &AtomicU64) {
    let Some(prev) = result_arc.swap(None) else {
        return;
    };
    let e = rt_epoch.load(Ordering::Acquire);
    release_when_quiesced(prev, e, rt_epoch);
}

/// swap で手元に取った旧 `Arc` を、**writer スレッド上で**解放されるように手放す。
///
/// `e` は swap の**直後**に読んだ epoch。偶数なら swap の瞬間に走っていた `process()` は
/// 無いので即座に安全、奇数なら epoch が `e` から変化した時点で安全。所有権の判定自体は
/// [`Arc::try_unwrap`] が担い、失敗している間は手放さない (手放すと RT が最後の所有者に
/// なり、audio thread 上で数十 MB の解放が起きる)。
///
/// 待ちは有界 (`FINAL_PUBLISH_WAIT`)。**非 RT スレッドからのみ呼ぶこと。**
fn release_when_quiesced(mut arc: Arc<SynthResult>, e: u64, rt_epoch: &AtomicU64) {
    let deadline = Instant::now() + FINAL_PUBLISH_WAIT;
    loop {
        if e.is_multiple_of(2) || rt_epoch.load(Ordering::Acquire) != e {
            match Arc::try_unwrap(arc) {
                // writer が唯一の所有者だった = 解放はこのスレッド (非 RT) で起きる。
                Ok(_) => return,
                Err(back) => arc = back,
            }
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                "VoicevoxBuiltin: 旧 synth buffer の quiesce 待ちが上限を超えた (解放先が RT になり得る)"
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// テスト用: 配置済み wav 群を 1 本の song-absolute buffer へ加算する
/// (= [`MixBuffer`] と同じ規則。`keep` = 全域・フェード無し)。
///
/// 旧 `voicevox::mix_placed_groups` を置き換えるもので、r.md #39 の配置契約
/// (「`place` が負なら曲頭より手前を **ずらさずに** 捨てる」「重なりは加算」) の
/// 回帰テストがこれを通る。本番の mix は [`PhraseRenderState::apply_pending`]。
#[cfg(test)]
pub(super) fn mix_placed_for_test(placed: &[(i64, Vec<f32>)]) -> Vec<f32> {
    let total = placed
        .iter()
        .map(|(p, s)| p.saturating_add(s.len() as i64).max(0) as usize)
        .max()
        .unwrap_or(0);
    let mut buf = vec![0.0f32; total];
    for (place, samples) in placed {
        let len = samples.len() as i64;
        let r = RenderedPhrase {
            samples: Arc::new(samples.clone()),
            place: *place,
            keep: *place..(*place + len),
            fade_in: false,
            fade_out: false,
        };
        mix_phrase(&mut buf, &r, xfade_half(OUTPUT_SAMPLE_RATE));
    }
    buf
}

/// 曲全体の mono buffer を **差分で**組み立てる器。長さは job 開始時に確定し、
/// talk がはみ出したときだけ `resize` で伸ばす。`applied` までは加算済み。
struct MixBuffer {
    buf: Vec<f32>,
    applied: usize,
    /// publish で貸し出し中の buffer が返ってくるまでの控え。最大 2 本。
    pool: Vec<Vec<f32>>,
}

impl MixBuffer {
    /// 回収した `SynthResult` の buffer をプールへ返す (最大 2 本)。
    /// `samples` が他所と共有されていれば諦めてそのまま drop する
    /// (呼び出し側が唯一の所有者になったあとに呼ぶので、通常は起きない)。
    fn recycle(&mut self, res: SynthResult) {
        let Ok(mut v) = Arc::try_unwrap(res.samples) else {
            return;
        };
        v.clear();
        if self.pool.len() < 2 {
            self.pool.push(v);
        }
    }

    fn new(total: usize) -> Self {
        Self {
            buf: vec![0.0; total],
            applied: 0,
            // 初回 publish 用に空 Vec を 2 本積んでおく (writer 側の確保、RT には無関係)。
            pool: vec![Vec::new(), Vec::new()],
        }
    }
}

/// フレーズ 1 本が終わるたびにコールバックへ渡す「積み上がった結果」。
/// 進捗の集計 (`pending` / `total` / `pending_clips`) と mix / publish を持つ。
/// [`PhraseRenderer`] 本体 (HTTP / キャッシュ / 塊 query) とは別 struct にして、
/// コールバック中に借用が衝突しないようにする。
pub(super) struct PhraseRenderState {
    rendered: Vec<RenderedPhrase>,
    mix: MixBuffer,
    /// publish 済みで RT がまだ触っているかもしれない `Arc` と、store 直後の epoch。
    retired: Vec<(Arc<SynthResult>, u64)>,
    last_publish: Instant,
    total: u32,
    done: u32,
    /// clip id → その clip に残っている仕事 (フレーズ / talk 発話) の数。
    outstanding_clips: HashMap<u32, u32>,
    note_offsets: HashMap<u32, u64>,
    /// r.md #87: `セルの clip_id → 仮想区間の原点 (拍)`。metadata から拾った値を
    /// そのまま `SynthResult` へ渡す (再生側の読み出し写像)。
    cell_bases: HashMap<u32, f64>,
    /// r.md #87: アレンジのタイムラインが読んでよい上限 (buffer sample)。
    arrangement_limit_sample: u64,
    sample_rate: u32,
    samples_per_beat: f64,
    xf: i64,
    /// audio half と共有する RT quiescence epoch。**`Drop` でも使う**ので所有する
    /// (job が途中で捨てられても `retired` を RT に押し付けないため)。
    rt_epoch: Arc<AtomicU64>,
}

impl Drop for PhraseRenderState {
    /// job がどう終わっても (完了 / supersede / engine 未到達 / shutdown)、
    /// `retired` に残った `Arc` は **writer スレッド上で**解放されるように畳む。
    ///
    /// ここを素の `Vec` の drop に任せると、RT の Guard が debt を払われて生きている
    /// 場合に RT が最後の所有者になり、audio thread 上で数十 MB の解放が起きる。
    fn drop(&mut self) {
        for (arc, e) in std::mem::take(&mut self.retired) {
            release_when_quiesced(arc, e, &self.rt_epoch);
        }
    }
}

impl PhraseRenderState {
    /// 未完了フレーズ数 (talk の 1 発話も 1 件)。
    #[must_use]
    pub(super) fn pending(&self) -> u32 {
        self.total.saturating_sub(self.done)
    }

    /// この job の総フレーズ数。
    #[must_use]
    pub(super) fn total(&self) -> u32 {
        self.total
    }

    /// 未完了の仕事が掛かっている clip id (昇順・重複なし)。
    #[must_use]
    pub(super) fn pending_clips(&self) -> Vec<u32> {
        let set: BTreeSet<u32> = self.outstanding_clips.keys().copied().collect();
        set.into_iter().collect()
    }

    /// 何か 1 本でも描けたか (= publish する中身があるか)。
    #[must_use]
    pub(super) fn has_output(&self) -> bool {
        !self.rendered.is_empty()
    }

    fn finish_item(&mut self, clip_ids: &[u32]) {
        self.done = self.done.saturating_add(1);
        for cid in clip_ids {
            if let Some(n) = self.outstanding_clips.get_mut(cid) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    self.outstanding_clips.remove(cid);
                }
            }
        }
    }

    /// 未加算のフレーズだけを mix buffer へ足す (publish のたびに全部足し直さない)。
    fn apply_pending(&mut self) {
        let Self { rendered, mix, xf, .. } = self;
        while mix.applied < rendered.len() {
            mix_phrase(&mut mix.buf, &rendered[mix.applied], *xf);
            mix.applied += 1;
        }
    }

    /// publish 済みで RT の quiesce が確認できた `Arc` を回収し、buffer をプールへ返す。
    ///
    /// quiesce 条件 = 「store より前に始まった `process()` は全部終わった」:
    /// audio half が `process()` の入口と出口で epoch を +1 する (入口後は奇数、出口後は
    /// 偶数)。store 直後に読んだ値 `e` が **偶数なら即座に安全**、奇数なら
    /// **epoch が `e` から変化した時点で安全**。
    ///
    /// epoch は「いつ試すか」のヒントにすぎず、**所有権の判定は
    /// [`Arc::try_unwrap`] が担う**。失敗したら `Arc` を**手放さずに** `retired` へ
    /// 戻す — ここで drop すると RT が最後の所有者になり、RT スレッド上で
    /// multi-MB の解放が起きる (この機構が防ごうとしているもの)。
    /// arc-swap 1.9.1 の hybrid 戦略では、`store` した writer が `Debt::pay_all`
    /// (`src/debt/mod.rs`) で未払い debt ごとに strong count を +1 するので、
    /// store と同時に走った `process()` の `Guard` は実際に所有者になり得る。
    fn reclaim(&mut self) {
        if self.retired.is_empty() {
            return;
        }
        let mut keep: Vec<(Arc<SynthResult>, u64)> = Vec::with_capacity(self.retired.len());
        for (arc, e) in std::mem::take(&mut self.retired) {
            // 偶数 = store の瞬間に走っていた `process()` が無い = 即座に安全。
            let quiesced = e.is_multiple_of(2) || self.rt_epoch.load(Ordering::Acquire) != e;
            if !quiesced {
                keep.push((arc, e));
                continue;
            }
            match Arc::try_unwrap(arc) {
                Ok(res) => self.mix.recycle(res),
                // RT (または誰か) がまだ所有している。次の機会に再試行する。
                Err(arc) => keep.push((arc, e)),
            }
        }
        self.retired = keep;
    }

    /// `PUBLISH_INTERVAL` 以上経っていて、かつ再利用可能な buffer があれば publish する。
    /// プールが空なら**見送る** (部分結果の表示が 0.5 秒遅れるだけ)。
    pub(super) fn publish_if_due(&mut self, result_arc: &ArcSwapOption<SynthResult>) {
        if self.last_publish.elapsed() < PUBLISH_INTERVAL {
            return;
        }
        self.publish(result_arc, false);
    }

    /// job 終端の publish。プールの空きを**有界に**待ち、それでも空なら 1 本だけ確保する。
    pub(super) fn publish_final(&mut self, result_arc: &ArcSwapOption<SynthResult>) {
        if !self.has_output() {
            // 歌唱も読み上げも 1 本も描けなかった (= 全フレーズが Rejected 等) = 無音。
            // 旧 buffer は **retire 経由で**手放す (`store(None)` 直呼びは RT に解放させる)。
            clear_published(result_arc, &self.rt_epoch);
            return;
        }
        let deadline = Instant::now() + FINAL_PUBLISH_WAIT;
        loop {
            if self.publish(result_arc, false) {
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        self.publish(result_arc, true);
    }

    /// 実際の publish。`allow_alloc` でプールが空でも新規確保する。
    fn publish(&mut self, result_arc: &ArcSwapOption<SynthResult>, allow_alloc: bool) -> bool {
        self.reclaim();
        self.apply_pending();
        if self.rendered.is_empty() {
            // まだ何も描けていない。publish するものが無い (= 前回の buffer を残す)。
            return true;
        }
        let Some(mut out) = self.mix.pool.pop().or_else(|| allow_alloc.then(Vec::new)) else {
            return false;
        };
        out.clear();
        out.extend_from_slice(&self.mix.buf);
        let res = Arc::new(SynthResult {
            samples: Arc::new(out),
            sample_rate: self.sample_rate,
            samples_per_beat: self.samples_per_beat,
            note_offsets: Arc::new(self.note_offsets.clone()),
            cell_bases: Arc::new(self.cell_bases.clone()),
            arrangement_limit_sample: self.arrangement_limit_sample,
        });
        let prev = result_arc.swap(Some(res));
        // swap の **後** に読むこと (これが quiesce 判定の基準点)。
        let e = self.rt_epoch.load(Ordering::Acquire);
        if let Some(p) = prev {
            self.retired.push((p, e));
        }
        self.last_publish = Instant::now();
        true
    }
}

/// レンダリングの結末。
pub(super) enum RenderOutcome {
    /// 全フレーズ (+ talk) 完了。`rejected` は engine が拒否した最初の理由。
    Done { rejected: Option<String> },
    /// より新しい入力が来た → この job は捨てる (`done_gen` は進めない)。
    Superseded,
    /// engine に到達できない → job を戻して retry (`done_gen` は進めない)。
    Unreachable(String),
    /// shutdown 中。
    Shutdown,
}

/// フレーズ 1 本ぶんの「単体 query 由来」の確定情報 (キャッシュキー / 配置 / note offset)。
/// **HTTP には投げない** (投げるのは塊クエリのスライス)。
struct SoloPhrase {
    key: CacheKey,
    placement: Placement,
    note_offsets: Vec<(u32, u64)>,
    /// 単体 query の「先頭 note の開始 〜 末尾 note の終端」frame 数。
    span_frames: i64,
}

/// `FrameAudioQuery` の JSON を検証して `(Value, frame 数)` にする。
/// キャッシュ hit をそのまま信じないための共通の関門。
fn parse_frame_query(body: &str) -> anyhow::Result<(serde_json::Value, usize)> {
    let fq: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("FrameAudioQuery parse: {e}"))?;
    let n = frame_query_len(&fq)?;
    anyhow::ensure!(n > 0, "FrameAudioQuery の f0 が空");
    Ok((fq, n))
}

/// 塊 1 個ぶんの HTTP 取得済み状態 (同じ塊の別フレーズが再利用する)。
struct ChunkState {
    query: ChunkQuery,
    fq: serde_json::Value,
    total_frames: usize,
}

/// この job のレンダリング全体。synth thread が 1 job につき 1 つ持つ。
pub(super) struct PhraseRenderer<'a> {
    bpm: f32,
    phrases: Vec<Phrase>,
    chunks: Vec<Chunk>,
    /// phrase index → chunk index。
    chunk_of: Vec<usize>,
    solo: Vec<SoloPhrase>,
    /// 塊ごとの取得済み query (lazy)。
    chunk_state: Vec<Option<ChunkState>>,
    talk: &'a [TalkSynthSpec],
    /// 合成順序 (phrases への index)。
    order: Vec<usize>,
    /// r.md #87: 区間の境界 (昇順の buffer sample)。各セルの中身が始まる位置。
    /// 書き込みは「自分の開始位置より後ろにある最初の境界」で打ち切る。
    region_bounds: Vec<i64>,
    cache: Option<VoiceVoxDiskCache>,
    client: Option<reqwest::blocking::Client>,
    state: PhraseRenderState,
}

impl<'a> PhraseRenderer<'a> {
    /// 分割 + 単体 query + 配置までを済ませる (すべて純粋・HTTP なし)。
    pub(super) fn new(
        bpm: f32,
        chunk_secs: f32,
        entries: &[NoteMetadata],
        talk: &'a [TalkSynthSpec],
        priority_beats: Option<f64>,
        rt_epoch: Arc<AtomicU64>,
    ) -> Self {
        let sr = OUTPUT_SAMPLE_RATE;
        let phrases = split_into_phrases(entries, bpm);
        let chunks = group_into_chunks(&phrases, bpm, chunk_secs);
        let mut chunk_of = vec![0usize; phrases.len()];
        for (ci, c) in chunks.iter().enumerate() {
            for pi in c.phrases.clone() {
                chunk_of[pi] = ci;
            }
        }

        // 各フレーズの単体 query = キャッシュキー・配置・note offset の SSoT。
        // 単体 query は平行移動不変 (`pos_of` の基準が自分の先頭 note) なので、
        // キーはクリップの移動・分割・複製で変わらない。
        let mut solo: Vec<SoloPhrase> = Vec::with_capacity(phrases.len());
        for (i, ph) in phrases.iter().enumerate() {
            let q = voicevox::build_sing_query_with(&ph.notes, bpm, ph.carry_in);
            let key = key_for_sing_phrase(&q.json, ph.speaker_id, chunk_secs);
            let span_frames = q
                .notes
                .last()
                .zip(q.notes.first())
                .map_or(0, |(last, first)| last.end_frame - first.start_frame);
            // 継ぎ目は「同一 speaker で隣り合うフレーズ」同士。split_into_phrases は
            // speaker ごとに連続した列を返すので、前後の同 speaker 隣接を index で引く。
            let prev_end = (i > 0 && phrases[i - 1].speaker_id == ph.speaker_id)
                .then(|| phrases[i - 1].end_beat);
            let next_start = phrases
                .get(i + 1)
                .filter(|n| n.speaker_id == ph.speaker_id)
                .map(|n| n.start_beat);
            let placement = phrase_window(
                ph.start_beat,
                ph.end_beat,
                span_frames,
                prev_end,
                next_start,
                bpm,
                sr,
            );
            // note_offsets: 単体 query の **実 frame 位置** をそのまま曲 sample へ。
            // `- REST_FRAMES` はしない — `sing_place_samples` は `sing_head_beat` 経由で
            // 既に REST_FRAMES ぶん手前を指しており、`start_frame` は先頭 rest 込みの
            // 絶対 frame だから、そのまま足すのが正しい (引くと約 107 ms 早くなる)。
            let head = sing_place_samples(ph.start_beat, bpm, sr);
            let note_offsets: Vec<(u32, u64)> = q
                .notes
                .iter()
                .filter_map(|p| {
                    let nid = *ph.note_ids.get(p.index)?;
                    let abs = head
                        .saturating_add(frames_to_samples(p.start_frame as f64, sr).round() as i64)
                        .max(0) as u64;
                    Some((nid, abs))
                })
                .collect();
            solo.push(SoloPhrase {
                key,
                placement,
                note_offsets,
                span_frames,
            });
        }

        // 曲全体の長さは合成前に確定できる (frame 演算だけで HTTP に依らない)。
        // talk は長さが事前に分からないので、超えたぶんだけ resize で伸ばす。
        let total_samples = solo
            .iter()
            .map(|s| {
                s.placement
                    .place
                    .saturating_add(s.placement.len)
                    .max(0) as usize
            })
            .max()
            .unwrap_or(0);

        // 進捗の帰属。歌唱は Phrase::clip_ids、talk は TalkSynthSpec::clip_id
        // (talk を入れ忘れると Text クリップのスピナーが消える)。
        let mut outstanding_clips: HashMap<u32, u32> = HashMap::new();
        for ph in &phrases {
            for cid in &ph.clip_ids {
                *outstanding_clips.entry(*cid).or_insert(0) += 1;
            }
        }
        for t in talk {
            *outstanding_clips.entry(t.clip_id).or_insert(0) += 1;
        }

        // r.md #87: ランチャーのセルは曲の終端より後ろの専用区間へ置かれている
        // (`NoteMetadata::cell_base_beat`)。原点は必ず正 (= アレンジ終端 + 隙間) なので
        // `> 0.0` が「セルか」の判定になる。
        let cell_bases: HashMap<u32, f64> = entries
            .iter()
            .filter(|e| e.cell_base_beat > 0.0)
            .map(|e| (e.clip_id, e.cell_base_beat))
            .chain(
                talk.iter()
                    .filter(|t| t.cell_base_beat > 0.0)
                    .map(|t| (t.clip_id, t.cell_base_beat)),
            )
            .collect();
        // セルごとの「中身が始まる最初の sample」= **区間の境界**。実際に置いた位置から
        // 取る (原点から拍で逆算すると、歌の先頭 rest / フレーズの pad ぶん手前へ
        // はみ出す実配置とズレて、境界すれすれで歌声が漏れる)。
        let mut cell_starts: HashMap<u32, i64> = HashMap::new();
        for (sp, ph) in solo.iter().zip(&phrases) {
            for cid in ph.clip_ids.iter().filter(|c| cell_bases.contains_key(c)) {
                let e = cell_starts.entry(*cid).or_insert(i64::MAX);
                *e = (*e).min(sp.placement.place);
            }
        }
        for t in talk.iter().filter(|t| t.cell_base_beat > 0.0) {
            let place = talk_place_samples(t.start_beat, t.scales.speed_scale, bpm, sr).0;
            let e = cell_starts.entry(t.clip_id).or_insert(i64::MAX);
            *e = (*e).min(place);
        }
        let mut region_bounds: Vec<i64> = cell_starts.into_values().collect();
        region_bounds.sort_unstable();
        region_bounds.dedup();
        #[allow(clippy::cast_sign_loss)]
        let arrangement_limit_sample =
            region_bounds.first().map_or(u64::MAX, |&p| p.max(0) as u64);

        // **区間をまたぐ書き込みを構造的に潰す。** 読み上げ (talk) の WAV 長は合成して
        // みるまで分からず、歌も content が窓より長ければフレーズが伸びる。はみ出しを
        // 許すと隣の区間 (= 別のセル / アレンジ) の音に混ざり、原因の分からない
        // 「別のセルの声が乗る」になる。切るのは末尾だけなので、はみ出していない
        // 通常のフレーズは 1 sample も変わらない。
        let xf = xfade_half(sr);
        for sp in &mut solo {
            let bound = region_end(&region_bounds, sp.placement.place).saturating_sub(xf);
            if sp.placement.keep.end > bound {
                sp.placement.keep.end = bound.max(sp.placement.keep.start);
                sp.placement.fade_out = true;
            }
        }

        let total = (phrases.len() + talk.len()) as u32;
        let state = PhraseRenderState {
            rendered: Vec::with_capacity(phrases.len() + talk.len()),
            mix: MixBuffer::new(total_samples),
            retired: Vec::new(),
            last_publish: Instant::now(),
            total,
            done: 0,
            outstanding_clips,
            note_offsets: HashMap::new(),
            cell_bases,
            arrangement_limit_sample,
            sample_rate: sr,
            samples_per_beat: f64::from(sr) * 60.0 / f64::from(bpm.max(0.001)),
            xf: xfade_half(sr),
            rt_epoch,
        };

        let order = order_by_priority(&phrases, priority_beats);
        let chunk_state = (0..chunks.len()).map(|_| None).collect();

        Self {
            bpm,
            phrases,
            chunks,
            chunk_of,
            solo,
            chunk_state,
            talk,
            order,
            region_bounds,
            cache: VoiceVoxDiskCache::production(),
            client: None,
            state,
        }
    }

    #[must_use]
    pub(super) fn state_mut(&mut self) -> &mut PhraseRenderState {
        &mut self.state
    }

    /// この job が使うフレーズ WAV のキー集合 (メモリキャッシュの GC 用)。
    #[must_use]
    pub(super) fn live_keys(&self) -> std::collections::HashSet<CacheKey> {
        self.solo.iter().map(|s| s.key).collect()
    }

    /// 合成を実行する。フレーズ 1 本 (talk 1 発話) が終わるたびに `on_done` を呼ぶ。
    pub(super) fn render(
        &mut self,
        mem: &mut MemoryCache,
        shutdown: &AtomicBool,
        superseded: &dyn Fn() -> bool,
        on_done: &mut dyn FnMut(&mut PhraseRenderState),
    ) -> RenderOutcome {
        let mut rejected: Option<String> = None;

        for oi in 0..self.order.len() {
            let i = self.order[oi];
            if shutdown.load(Ordering::SeqCst) {
                return RenderOutcome::Shutdown;
            }
            if superseded() {
                return RenderOutcome::Superseded;
            }
            match self.render_one_phrase(i, mem) {
                Ok(()) => {}
                Err(SynthError::Unreachable(e)) => {
                    // 全フレーズに影響するので即中断して retry。
                    return RenderOutcome::Unreachable(format!("{e:#}"));
                }
                Err(SynthError::Rejected(d)) => {
                    // そのフレーズだけ諦めて続行 (1 歌詞で全滅させない)。
                    rejected.get_or_insert(d);
                    let clip_ids = self.phrases[i].clip_ids.clone();
                    self.state.finish_item(&clip_ids);
                }
            }
            on_done(&mut self.state);
        }

        // (talk) 読み上げ群。現行どおり 1 件ずつ合成し、同じ mix buffer へ積む。
        for ti in 0..self.talk.len() {
            if shutdown.load(Ordering::SeqCst) {
                return RenderOutcome::Shutdown;
            }
            if superseded() {
                return RenderOutcome::Superseded;
            }
            match self.render_one_talk(ti) {
                Ok(()) => {}
                Err(SynthError::Unreachable(e)) => {
                    return RenderOutcome::Unreachable(format!("{e:#}"));
                }
                Err(SynthError::Rejected(d)) => {
                    rejected.get_or_insert(d);
                    let clip_id = self.talk[ti].clip_id;
                    self.state.finish_item(&[clip_id]);
                }
            }
            on_done(&mut self.state);
        }

        RenderOutcome::Done { rejected }
    }

    /// HTTP client を lazy に作る (キャッシュ全 hit なら 1 度も作らない)。
    fn client(&mut self) -> Result<&reqwest::blocking::Client, SynthError> {
        if self.client.is_none() {
            self.client = Some(synth_client()?);
        }
        Ok(self.client.as_ref().expect("just built"))
    }

    /// フレーズ `i` を メモリ → ディスク → HTTP の順で得て、配置して積む。
    fn render_one_phrase(&mut self, i: usize, mem: &mut MemoryCache) -> Result<(), SynthError> {
        let key = self.solo[i].key;
        let speaker_id = self.phrases[i].speaker_id;
        let mut query_ms: u128 = 0;
        let mut synth_ms: u128 = 0;
        let mut hit = true;
        let mut frames: i64 = self.solo[i].span_frames + 2 * PHRASE_PAD_FRAMES;

        let (samples, sample_rate) = if let Some((s, sr)) = mem.get(&key) {
            (Arc::clone(s), *sr)
        } else if let Some((s, sr)) = self
            .cache
            .as_ref()
            .and_then(|c| c.get(key, CacheKind::Wav))
            .and_then(|bytes| match decode_wav_to_f32(&bytes) {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(error = ?e, "voicevox cache: 壊れた WAV を無視して再合成");
                    None
                }
            })
        {
            let arc = Arc::new(s);
            mem.insert(key, (Arc::clone(&arc), sr));
            (arc, sr)
        } else {
            hit = false;
            // 塊クエリを用意する (この塊で初めて miss したときだけ HTTP)。
            let ci = self.chunk_of[i];
            let t0 = Instant::now();
            let fetched = self.ensure_chunk_query(ci)?;
            if fetched {
                query_ms = t0.elapsed().as_millis();
            }
            let (a, b) = self.phrase_window_frames(ci, i)?;
            frames = (b - a) as i64;
            let sliced = {
                let cs = self.chunk_state[ci].as_ref().expect("ensured above");
                slice_frame_query(&cs.fq, a, b)
                    .map_err(|e| SynthError::Rejected(format!("{e:#}")))?
            };
            let t1 = Instant::now();
            let client = self.client()?;
            let wav = frame_synthesis(client, &sliced, speaker_id)?;
            synth_ms = t1.elapsed().as_millis();
            if let Some(c) = self.cache.as_ref() {
                c.put(key, CacheKind::Wav, &wav);
            }
            let (s, sr) =
                decode_wav_to_f32(&wav).map_err(|e| SynthError::Rejected(format!("{e:#}")))?;
            let arc = Arc::new(s);
            mem.insert(key, (Arc::clone(&arc), sr));
            (arc, sr)
        };

        tracing::info!(
            phrase = i,
            speaker = speaker_id,
            frames,
            cache = if hit { "hit" } else { "miss" },
            query_ms,
            synth_ms,
            "voicevox phrase rendered"
        );

        // decode 後の sample rate が想定と違ったら **黙って無音にしない**。
        // その rate で配置を計算し直したうえで警告を出す (24 kHz の混入を見逃さない)。
        let placement = if sample_rate == OUTPUT_SAMPLE_RATE {
            self.solo[i].placement.clone()
        } else {
            tracing::warn!(
                sample_rate,
                expected = OUTPUT_SAMPLE_RATE,
                phrase = i,
                "VOICEVOX が想定外の sample rate を返した (配置を再計算する)"
            );
            let ph = &self.phrases[i];
            let prev_end = (i > 0 && self.phrases[i - 1].speaker_id == ph.speaker_id)
                .then(|| self.phrases[i - 1].end_beat);
            let next_start = self
                .phrases
                .get(i + 1)
                .filter(|n| n.speaker_id == ph.speaker_id)
                .map(|n| n.start_beat);
            phrase_window(
                ph.start_beat,
                ph.end_beat,
                self.solo[i].span_frames,
                prev_end,
                next_start,
                self.bpm,
                sample_rate,
            )
        };

        for (nid, off) in &self.solo[i].note_offsets {
            self.state.note_offsets.insert(*nid, *off);
        }
        self.state.rendered.push(RenderedPhrase {
            samples,
            place: placement.place,
            keep: placement.keep,
            fade_in: placement.fade_in,
            fade_out: placement.fade_out,
        });
        let clip_ids = self.phrases[i].clip_ids.clone();
        self.state.finish_item(&clip_ids);
        Ok(())
    }

    /// 塊 `ci` の `FrameAudioQuery` を用意する。戻り値は「HTTP を打ったか」。
    fn ensure_chunk_query(&mut self, ci: usize) -> Result<bool, SynthError> {
        if self.chunk_state[ci].is_some() {
            return Ok(false);
        }
        let chunk = self.chunks[ci].clone();
        let query = build_chunk_query(
            &self.phrases[chunk.phrases.clone()],
            chunk.carry_in,
            self.bpm,
        );
        let qkey = key_for_sing_query(&query.json);
        // キャッシュ hit は **必ず検証してから使う**。壊れたエントリをそのまま信じると、
        // 以後その塊は永久に合成できない (次も同じ壊れた JSON を読むので HTTP へ落ちない)。
        // 検証に失敗したら miss 扱いで HTTP へ落ち、`put` が上書きして直る
        // (壊れた WAV の扱いと同じ規則)。
        let mut fetched = false;
        let cached = self
            .cache
            .as_ref()
            .and_then(|c| c.get(qkey, CacheKind::Json))
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|text| match parse_frame_query(&text) {
                Ok(parsed) => Some(parsed),
                Err(e) => {
                    tracing::warn!(error = %e, "voicevox cache: 壊れた塊 query を無視して再取得");
                    None
                }
            });
        let (fq, total_frames) = match cached {
            Some(v) => v,
            None => {
                fetched = true;
                let json = query.json.clone();
                let client = self.client()?;
                // 応答は `fetch_sing_frame_query` の中で正規化済
                // (= キャッシュに入るのは正規形だけ)。
                let body = fetch_sing_frame_query(client, &json)?;
                let parsed = parse_frame_query(&body)
                    .map_err(|e| SynthError::Rejected(format!("{e:#}")))?;
                if let Some(c) = self.cache.as_ref() {
                    c.put(qkey, CacheKind::Json, body.as_bytes());
                }
                parsed
            }
        };
        // engine が返した frame 数は**こちらの予測と一致するはず**だが、engine 側の
        // 実装が変わればずれ得る。`debug_assert!` にすると debug ビルドで synth thread
        // ごと落ちる (= engine の応答で自分のスレッドを殺す) ので、警告に留める。
        // 窓は下の `phrase_window_frames` が実測値でクランプするので、ずれても degrade
        // するだけで無音にはならない。
        if total_frames as i64 != query.total_frames {
            tracing::warn!(
                engine = total_frames,
                predicted = query.total_frames,
                chunk = ci,
                "塊 query の総 frame 数が予測と一致しない (切り出し窓を実測値でクランプする)"
            );
        }
        self.chunk_state[ci] = Some(ChunkState {
            query,
            fq,
            total_frames,
        });
        Ok(fetched)
    }

    /// フレーズ `i` の切り出し窓 `[a, b)` (塊 `ci` の frame 空間)。
    fn phrase_window_frames(&self, ci: usize, i: usize) -> Result<(usize, usize), SynthError> {
        let cs = self.chunk_state[ci].as_ref().expect("chunk query prepared");
        let local = i - self.chunks[ci].phrases.start;
        let Some(w) = cs.query.phrase_windows.get(local) else {
            return Err(SynthError::Rejected(format!(
                "phrase {i} has no window in chunk {ci}"
            )));
        };
        if w.end <= w.start {
            // この塊では歌われない (到達しないはずの防御)。無音扱い。
            tracing::warn!(phrase = i, chunk = ci, "この塊にフレーズの note が 1 件も無い");
            return Err(SynthError::Rejected(
                "phrase has no notes in its chunk query".into(),
            ));
        }
        let a = w.start - PHRASE_PAD_FRAMES;
        let b = w.end + PHRASE_PAD_FRAMES;
        // 端 rest を PHRASE_PAD_FRAMES にしてあるので常に成立する。将来 既定値を
        // 触った人が気付けるよう assert を置く。
        debug_assert!(a >= 0, "切り出し窓の下端が負 (端 rest を変えた?)");
        debug_assert!(
            b <= cs.total_frames as i64,
            "切り出し窓の上端が query を超えた (端 rest を変えた?)"
        );
        let a = a.max(0) as usize;
        let b = (b.max(0) as usize).min(cs.total_frames);
        if b <= a {
            return Err(SynthError::Rejected(
                "phrase window collapsed inside the chunk query".into(),
            ));
        }
        Ok((a, b))
    }

    /// talk 発話 `ti` を合成して同じ mix buffer へ積む。
    fn render_one_talk(&mut self, ti: usize) -> Result<(), SynthError> {
        let spec = &self.talk[ti];
        if spec.text.is_empty() {
            let clip_id = spec.clip_id;
            self.state.finish_item(&[clip_id]);
            return Ok(());
        }
        let (samples, sample_rate) =
            synthesize_talk_for_builtin(&spec.text, spec.speaker_id, &spec.scales)?;
        let (head, speech_start) = talk_place_samples(
            spec.start_beat,
            spec.scales.speed_scale,
            self.bpm,
            sample_rate,
        );
        let len = samples.len() as i64;
        self.state
            .note_offsets
            .insert(spec.event_id, speech_start.max(0) as u64);
        // r.md #87: 区間 (アレンジ / セル) をまたいで書かない。talk の WAV 長は
        // 合成してみるまで分からないので、ここが唯一の防壁 (はみ出せば隣の区間の
        // 音に混ざる)。切ったときだけ末尾にフェードを掛けて click を消す。
        let bound = region_end(&self.region_bounds, head).saturating_sub(self.state.xf);
        let full_end = head.saturating_add(len);
        let keep_end = full_end.min(bound).max(head);
        self.state.rendered.push(RenderedPhrase {
            samples: Arc::new(samples),
            place: head,
            // talk は全域を残しフェードも掛けない (歌唱の継ぎ目とは無関係)。
            keep: head..keep_end,
            fade_in: false,
            fade_out: keep_end < full_end,
        });
        let clip_id = spec.clip_id;
        self.state.finish_item(&[clip_id]);
        Ok(())
    }
}

/// 合成順序。`priority_beats` があれば「再生位置を含むフレーズ → 位置より後ろを昇順
/// → 前を降順」(本家 `selectPriorPhrase` と同じ)。無ければ start_beat 昇順。
fn order_by_priority(phrases: &[Phrase], priority_beats: Option<f64>) -> Vec<usize> {
    use std::cmp::Ordering as Ord2;
    let mut idx: Vec<usize> = (0..phrases.len()).collect();
    idx.sort_by(|&a, &b| {
        phrases[a]
            .start_beat
            .partial_cmp(&phrases[b].start_beat)
            .unwrap_or(Ord2::Equal)
            .then(a.cmp(&b))
    });
    let Some(p) = priority_beats else { return idx };
    // (階層, 距離) — 階層 0 = 再生位置を含む、1 = 後ろ、2 = 前。
    let rank = |i: usize| -> (u8, f64) {
        let ph = &phrases[i];
        if p >= ph.start_beat && p < ph.end_beat {
            (0, 0.0)
        } else if ph.start_beat >= p {
            (1, ph.start_beat - p)
        } else {
            (2, p - ph.start_beat)
        }
    };
    idx.sort_by(|&a, &b| {
        let (ka, da) = rank(a);
        let (kb, db) = rank(b);
        ka.cmp(&kb)
            .then(da.partial_cmp(&db).unwrap_or(Ord2::Equal))
            .then(a.cmp(&b))
    });
    idx
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::Note;

    const SR: u32 = 48_000;
    const BPM: f32 = 120.0;

    fn note(start: f64, dur: f64) -> Note {
        Note {
            id: 0,
            start_beat: start,
            duration_beats: dur,
            pitch: 60,
            velocity: 100,
            lyric: Some("ら".into()),
            muted: false,
        }
    }

    fn phrase(start: f64, dur: f64) -> Phrase {
        Phrase {
            speaker_id: 3061,
            notes: vec![note(start, dur)],
            note_ids: vec![1],
            clip_ids: vec![1],
            carry_in: None,
            start_beat: start,
            end_beat: start + dur,
        }
    }

    /// 3 フレーズ列の placement を、隣接情報つきで作る。
    fn windows(starts_durs: &[(f64, f64)]) -> Vec<Placement> {
        let phs: Vec<Phrase> = starts_durs.iter().map(|&(s, d)| phrase(s, d)).collect();
        phs.iter()
            .enumerate()
            .map(|(i, ph)| {
                let span = voicevox::build_sing_query_with(&ph.notes, BPM, None);
                let span_frames = span
                    .notes
                    .last()
                    .zip(span.notes.first())
                    .map_or(0, |(l, f)| l.end_frame - f.start_frame);
                phrase_window(
                    ph.start_beat,
                    ph.end_beat,
                    span_frames,
                    (i > 0).then(|| phs[i - 1].end_beat),
                    phs.get(i + 1).map(|n| n.start_beat),
                    BPM,
                    SR,
                )
            })
            .collect()
    }

    #[test]
    fn seams_tile_without_overlap() {
        // 休符が 2 * PHRASE_PAD (= 1 秒 = 2 拍 @120bpm) 以下なら隙間なく敷き詰まる。
        let w = windows(&[(0.0, 1.0), (2.0, 1.0), (4.0, 1.0)]);
        assert_eq!(w[0].keep.end, w[1].keep.start, "継ぎ目がぴったり合う");
        assert_eq!(w[1].keep.end, w[2].keep.start);
        // 端はフェードしない (外側に隣が無い)。
        assert!(!w[0].fade_in);
        assert!(w[0].fade_out);
        assert!(w[1].fade_in && w[1].fade_out);
        assert!(w[2].fade_in);
        assert!(!w[2].fade_out);
    }

    #[test]
    fn long_rest_leaves_intentional_silence() {
        // 休符が 2 * PHRASE_PAD を超えると、両隣とも自分の WAV 端でクランプされ、
        // 間は **意図した無音**になる (欠落ではない)。重ならず順序も保たれる。
        let w = windows(&[(0.0, 1.0), (20.0, 1.0)]);
        assert!(
            w[0].keep.end < w[1].keep.start,
            "keep_end(0)={} keep_start(1)={}",
            w[0].keep.end,
            w[1].keep.start
        );
        // WAV 端でクランプされたので、その側のフェードは立たない。
        assert!(!w[0].fade_out);
        assert!(!w[1].fade_in);
    }

    #[test]
    fn crossfade_ramps_sum_to_unity() {
        // 継ぎ目を共有する 2 フレーズの相補ランプは全域で和が 1。
        let w = windows(&[(0.0, 1.0), (2.0, 1.0)]);
        let seam = w[0].keep.end;
        assert_eq!(seam, w[1].keep.start);
        let xf = xfade_half(SR);
        for t in (seam - xf)..(seam + xf) {
            let a = seam_gain(t, &w[0].keep, w[0].fade_in, w[0].fade_out, xf);
            let b = seam_gain(t, &w[1].keep, w[1].fade_in, w[1].fade_out, xf);
            assert!((a + b - 1.0).abs() < 1e-6, "t={t} a={a} b={b}");
        }
    }

    #[test]
    fn note_offset_lands_on_the_note_beat() {
        // r.md #39 契約: 単体 query 由来の note offset が拍位置と 1 sample 以内で一致する
        // (`- REST_FRAMES` を書くとここが 10 frame ずれて落ちる)。
        let spb = f64::from(SR) * 60.0 / f64::from(BPM);
        for start_beat in [0.0, 1.0, 2.5, 17.25] {
            let ph = phrase(start_beat, 1.0);
            let q = voicevox::build_sing_query_with(&ph.notes, BPM, None);
            let head = sing_place_samples(start_beat, BPM, SR);
            let off = head
                + frames_to_samples(q.notes[0].start_frame as f64, SR).round() as i64;
            let ideal = (start_beat * spb).round() as i64;
            assert!(
                (off - ideal).abs() <= 1,
                "start_beat={start_beat} off={off} ideal={ideal}"
            );
        }
    }

    #[test]
    fn window_is_inside_the_chunk_query() {
        // 端 rest PHRASE_PAD_FRAMES で組んだ塊 query に対し、先頭 / 末尾のフレーズでも
        // `0 <= a && b <= total` (= クランプが不要)。
        let phs: Vec<Phrase> = [(0.0, 1.0), (3.0, 1.0), (7.0, 2.0)]
            .iter()
            .map(|&(s, d)| phrase(s, d))
            .collect();
        let cq = common::voicevox_phrase::build_chunk_query(&phs, None, BPM);
        for (i, w) in cq.phrase_windows.iter().enumerate() {
            let a = w.start - PHRASE_PAD_FRAMES;
            let b = w.end + PHRASE_PAD_FRAMES;
            assert!(a >= 0, "phrase {i}: a={a}");
            assert!(b <= cq.total_frames, "phrase {i}: b={b} total={}", cq.total_frames);
        }
    }

    /// テスト用の空 `PhraseRenderState` (mix / applied の bookkeeping だけを使う)。
    fn empty_state(total: usize) -> PhraseRenderState {
        PhraseRenderState {
            rendered: Vec::new(),
            mix: MixBuffer::new(total),
            retired: Vec::new(),
            last_publish: Instant::now(),
            total: 0,
            done: 0,
            outstanding_clips: HashMap::new(),
            cell_bases: HashMap::new(),
            arrangement_limit_sample: u64::MAX,
            note_offsets: HashMap::new(),
            sample_rate: SR,
            samples_per_beat: f64::from(SR) * 60.0 / f64::from(BPM),
            xf: xfade_half(SR),
            rt_epoch: Arc::new(AtomicU64::new(0)),
        }
    }

    #[test]
    fn mix_buffer_incremental_equals_full_remix() {
        // フレーズを 1 本ずつ (publish のたびに) 加算した buffer と、最後にまとめて
        // 加算した buffer が一致すること。`applied` の bookkeeping が壊れると
        // 二重加算 (音量 2 倍) か取りこぼし (無音) になる。
        let w = windows(&[(0.0, 1.0), (2.0, 1.0), (4.0, 1.0)]);
        let mk = |i: usize, p: &Placement| RenderedPhrase {
            samples: Arc::new((0..p.len).map(|k| (i as f32 + 1.0) + k as f32 * 0.001).collect()),
            place: p.place,
            keep: p.keep.clone(),
            fade_in: p.fade_in,
            fade_out: p.fade_out,
        };
        let total = w
            .iter()
            .map(|p| (p.place + p.len).max(0) as usize)
            .max()
            .unwrap();

        // (a) 1 本積むごとに apply_pending (= publish_if_due が毎フレーズ呼ぶ形)。
        // 追加で「何も足していないのにもう一度 apply しても変わらない」も見る
        // (= applied が二重加算を防いでいることの直接の検査)。
        let mut incremental = empty_state(total);
        for (i, p) in w.iter().enumerate() {
            incremental.rendered.push(mk(i, p));
            incremental.apply_pending();
            let snapshot = incremental.mix.buf.clone();
            incremental.apply_pending();
            assert_eq!(incremental.mix.buf, snapshot, "冪等: 二重加算しない");
        }

        // (b) 全部積んでから 1 回だけ apply_pending。
        let mut batched = empty_state(total);
        for (i, p) in w.iter().enumerate() {
            batched.rendered.push(mk(i, p));
        }
        batched.apply_pending();

        assert_eq!(incremental.mix.buf.len(), batched.mix.buf.len());
        for (i, (a, b)) in incremental
            .mix
            .buf
            .iter()
            .zip(batched.mix.buf.iter())
            .enumerate()
        {
            assert!((a - b).abs() < 1e-9, "sample {i}: {a} vs {b}");
        }
        // 継ぎ目の直前後で欠落 (0 の穴) が無いこと。
        let seam = w[0].keep.end as usize;
        assert!(
            incremental.mix.buf[seam - 1] != 0.0 && incremental.mix.buf[seam] != 0.0,
            "継ぎ目で音が途切れない"
        );
    }

    #[test]
    fn priority_order_starts_at_the_playhead() {
        let phs: Vec<Phrase> = [(0.0, 1.0), (4.0, 1.0), (8.0, 1.0), (12.0, 1.0)]
            .iter()
            .map(|&(s, d)| phrase(s, d))
            .collect();
        // ヒント無し = start_beat 昇順。
        assert_eq!(order_by_priority(&phs, None), vec![0, 1, 2, 3]);
        // 再生位置 8.5 拍 = phrase 2 の中 → 2 → 後ろ (3) → 前 (1, 0)。
        assert_eq!(order_by_priority(&phs, Some(8.5)), vec![2, 3, 1, 0]);
    }

    /// r.md #87: ランチャーのセルは曲の終端より後ろの仮想区間へ置かれる。
    /// **アレンジの読み出し上限がセルの実配置より手前に来る**ことを固定する
    /// (走査を落として `u64::MAX` のままになると、playhead を曲の後ろへ動かした
    /// だけで撃っていないセルが歌い出す — ビルドもテストも素通りする種類の欠陥)。
    #[test]
    fn cell_regions_bound_the_arrangement_read() {
        use common::voicevox::DEFAULT_SINGER_ID;

        let mk = |note_id: u32, clip_id: u32, start: f64, base: f64| NoteMetadata {
            note_id,
            start_beat: start,
            duration_beats: 0.5,
            pitch: 60,
            velocity: 100,
            lyric: "ら".into(),
            clip_id,
            speaker_id: DEFAULT_SINGER_ID,
            cell_base_beat: base,
        };
        // アレンジ (clip 1) は 0..1 拍、セル (clip 2) は原点 40 拍の区間に 0..1 拍。
        let entries = vec![
            mk(0, 1, 0.0, 0.0),
            mk(1, 1, 0.5, 0.0),
            mk(2, 2, 40.0, 40.0),
            mk(3, 2, 40.5, 40.0),
        ];
        let talk: Vec<TalkSynthSpec> = Vec::new();
        let r = PhraseRenderer::new(120.0, 60.0, &entries, &talk, None, Arc::new(AtomicU64::new(0)));
        let limit = r.state.arrangement_limit_sample;
        assert_ne!(limit, u64::MAX, "セルを走査し落としている");

        // アレンジのフレーズ (index 0) は上限より手前で完結し、セルのフレーズ
        // (index 1) は上限のところから始まる。
        let arrangement = &r.solo[0].placement;
        let cell = &r.solo[1].placement;
        assert!(
            arrangement.place + arrangement.len <= limit as i64,
            "アレンジのフレーズが上限を越えている: {arrangement:?} limit={limit}"
        );
        assert_eq!(limit as i64, cell.place, "上限はセルの実配置ちょうど");

        // セルが無ければ従来どおり buffer 末尾まで読める。
        let only_arrangement = vec![mk(0, 1, 0.0, 0.0)];
        let r2 = PhraseRenderer::new(
            120.0,
            60.0,
            &only_arrangement,
            &talk,
            None,
            Arc::new(AtomicU64::new(0)),
        );
        assert_eq!(r2.state.arrangement_limit_sample, u64::MAX);
    }

    // ---- 実 engine を要する統合テスト (既定では走らせない) -------------------

    /// 受け入れ条件を機械で測る: 同じ楽譜を
    ///   (a) 全体 1 クエリ + 全体 frame_synthesis
    ///   (b) 60 秒の塊クエリ + フレーズ単位 frame_synthesis
    /// で描き、フレーズごとの RMS のばらつき (dB) の **σ が 0.5 dB 以下**であること。
    /// 実測は σ 0.41 dB (docs/plan_rmd_75_voicevox_phrase.md §0 (C))。
    /// bit 一致は原理的に成立しない (`/sing_frame_audio_query` が非決定的) ので比べない。
    ///
    /// **この条件が保証するのは「一貫した塊分けで一度に描いたとき」だけ**。実運用では
    /// キャッシュが効き、**別々の塊構成で合成されたフレーズが 1 本の buffer に混ざる**
    /// (それがこの設計の狙いでもある)。その patchwork 状態の音量ばらつきは **まだ
    /// 測っていない** — 塊構成をキーに入れない以上、測れる形にするには「編集を繰り返した
    /// 曲」を再現する必要があり、それは実機 sign-off に回す。数字が無いまま閾値を作らない。
    #[test]
    #[ignore = "requires a running VOICEVOX engine at localhost:50021"]
    fn chunked_render_matches_whole_render_within_half_db_sigma() {
        use common::voicevox::DEFAULT_SINGER_ID;

        // 2 拍ごとに 4 音の小節を 30 個 = 120 音、間に休符 → 30 フレーズ。
        let mut entries: Vec<NoteMetadata> = Vec::new();
        let mut id = 0u32;
        let mut beat = 0.0;
        for _ in 0..30 {
            for k in 0..4 {
                entries.push(NoteMetadata {
                    note_id: id,
                    start_beat: beat + f64::from(k) * 0.5,
                    duration_beats: 0.5,
                    pitch: 60 + (k as u8),
                    velocity: 100,
                    lyric: "ら".into(),
                    clip_id: 1,
                    speaker_id: DEFAULT_SINGER_ID,
                    cell_base_beat: 0.0,
                });
                id += 1;
            }
            beat += 4.0;
        }
        let phrases = split_into_phrases(&entries, 120.0);
        assert!(phrases.len() >= 10, "分割できている: {}", phrases.len());

        let client = synth_client().expect("http client");

        // (a) 全体 1 クエリ + 全体合成。
        let notes: Vec<Note> = entries
            .iter()
            .map(|e| Note {
                id: 0,
                start_beat: e.start_beat,
                duration_beats: e.duration_beats,
                pitch: e.pitch,
                velocity: e.velocity,
                lyric: Some(e.lyric.clone()),
                muted: false,
            })
            .collect();
        let whole_score = voicevox::build_sing_query(&notes, 120.0);
        let whole_fq_body =
            fetch_sing_frame_query(&client, &whole_score.json).expect("whole query");
        let whole_wav =
            frame_synthesis(&client, &whole_fq_body, DEFAULT_SINGER_ID).expect("whole synth");
        let (whole, whole_sr) = decode_wav_to_f32(&whole_wav).expect("decode whole");
        assert_eq!(whole_sr, OUTPUT_SAMPLE_RATE);

        // (b) 塊クエリ + フレーズ合成。
        let chunks = group_into_chunks(&phrases, 120.0, 60.0);
        let mut ratios_db: Vec<f64> = Vec::new();
        for chunk in &chunks {
            let cq = build_chunk_query(
                &phrases[chunk.phrases.clone()],
                chunk.carry_in,
                120.0,
            );
            let body = fetch_sing_frame_query(&client, &cq.json).expect("chunk query");
            let fq: serde_json::Value = serde_json::from_str(&body).expect("fq json");
            for (local, w) in cq.phrase_windows.iter().enumerate() {
                let gi = chunk.phrases.start + local;
                let a = (w.start - PHRASE_PAD_FRAMES).max(0) as usize;
                let b = (w.end + PHRASE_PAD_FRAMES) as usize;
                let sliced = slice_frame_query(&fq, a, b).expect("slice");
                let wav = frame_synthesis(&client, &sliced, chunk.speaker_id).expect("synth");
                let (part, sr) = decode_wav_to_f32(&wav).expect("decode part");
                assert_eq!(sr, OUTPUT_SAMPLE_RATE);
                // 同じ区間を全体合成からも切り出して RMS を比べる。
                let ph = &phrases[gi];
                let head = sing_place_samples(ph.start_beat, 120.0, sr);
                let rest_s =
                    frames_to_samples(f64::from(REST_FRAMES), sr).round() as i64;
                let pad_s =
                    frames_to_samples(PHRASE_PAD_FRAMES as f64, sr).round() as i64;
                let place = head + rest_s - pad_s;
                // 全体合成 wav の frame 0 は「曲の先頭 note の REST_FRAMES 手前」。
                let whole_head = sing_place_samples(
                    voicevox::sing_base_beat(&notes).expect("has notes"),
                    120.0,
                    sr,
                );
                let lo = (place - whole_head).max(0) as usize;
                let hi = (lo + part.len()).min(whole.len());
                if hi <= lo {
                    continue;
                }
                let rms = |v: &[f32]| -> f64 {
                    if v.is_empty() {
                        return 0.0;
                    }
                    (v.iter().map(|s| f64::from(*s) * f64::from(*s)).sum::<f64>()
                        / v.len() as f64)
                        .sqrt()
                };
                let a_rms = rms(&whole[lo..hi]);
                let b_rms = rms(&part[..(hi - lo)]);
                if a_rms > 1e-6 && b_rms > 1e-6 {
                    ratios_db.push(20.0 * (b_rms / a_rms).log10());
                }
            }
        }
        assert!(ratios_db.len() >= 10, "比較できたフレーズ数: {}", ratios_db.len());
        let mean = ratios_db.iter().sum::<f64>() / ratios_db.len() as f64;
        let var = ratios_db.iter().map(|d| (d - mean).powi(2)).sum::<f64>()
            / ratios_db.len() as f64;
        let sigma = var.sqrt();
        assert!(sigma <= 0.5, "σ = {sigma:.3} dB (実測 0.41 dB)");
    }

    /// 5 分 (= 単発 frame_synthesis が HTTP 500 になる長さ) の楽譜でも、全フレーズが
    /// 合成できること。実測 211 フレーズ / 失敗 0 / 合計 30.68 s。
    #[test]
    #[ignore = "requires a running VOICEVOX engine at localhost:50021"]
    fn five_minute_song_renders_every_phrase() {
        use common::voicevox::DEFAULT_SINGER_ID;

        // 120 bpm で 600 拍 = 300 秒。2 拍ごとに 2 音 + 休符。
        let mut entries: Vec<NoteMetadata> = Vec::new();
        let mut id = 0u32;
        let mut beat = 0.0;
        while beat < 600.0 {
            for k in 0..2 {
                entries.push(NoteMetadata {
                    note_id: id,
                    start_beat: beat + f64::from(k) * 0.5,
                    duration_beats: 0.5,
                    pitch: 62,
                    velocity: 100,
                    lyric: "ら".into(),
                    clip_id: 1,
                    speaker_id: DEFAULT_SINGER_ID,
                    cell_base_beat: 0.0,
                });
                id += 1;
            }
            beat += 2.0;
        }
        let talk: Vec<TalkSynthSpec> = Vec::new();
        let mut renderer = PhraseRenderer::new(
            120.0,
            60.0,
            &entries,
            &talk,
            None,
            Arc::new(AtomicU64::new(0)),
        );
        let mut mem: MemoryCache = MemoryCache::new();
        let shutdown = AtomicBool::new(false);
        let outcome = renderer.render(&mut mem, &shutdown, &|| false, &mut |_| {});
        match outcome {
            RenderOutcome::Done { rejected } => {
                assert!(rejected.is_none(), "rejected: {rejected:?}");
            }
            _ => panic!("5 分の曲が完走しなかった"),
        }
        let state = renderer.state_mut();
        assert_eq!(state.pending(), 0);
        assert!(state.total() > 100, "フレーズ数: {}", state.total());
        assert!(state.has_output());
    }
}
