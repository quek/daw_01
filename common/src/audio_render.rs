//! Audio rendering の純粋関数 helper (Phase 2 PR-A)。
//!
//! `daw_audio::audio_clip_renderer` (live audio thread の per-buffer mix
//! loop) と `daw_gui::AppData::bounce_clip_in_place` (offline mix → WAV
//! 書き出し) で同一ロジックが重複していたので、 共通 crate (= common
//! crate) に切り出して DRY 化する。
//!
//! 切り出し対象:
//! - [`fade_envelope`]: fade-in / fade-out の 0..=1 envelope (Linear /
//!   Exponential / SCurve)
//! - [`sample_rate_ratio`] / [`pitch_factor`]: **時間軸** (source SR ↔ engine SR)
//!   と **ピッチ軸** (semitone) の比。 直交した 2 量として持ち、 どの mode で
//!   どちらを掛けるかは呼び出し側 (render loop) が決める。 mode 分岐をここに
//!   持たせていた旧 `pitch_ratio_for` は、 Stretch / Slice / Raw で pitch 比を
//!   捨てていたため inspector のピッチが無反応になっていた
//!
//! どれも RT path で呼ばれることを想定し allocation / panic free。
//! `Stretch` / `Slice` の time-stretch 本体は daw_audio
//! `audio_clip_renderer.rs` の `granular_sample_at` / `slice_sample_at` が担う。

use crate::model::{BeatMarker, FadeCurve, StretchMode};

/// Fade envelope at frame offset `t` (frames since fade start). Output
/// is in `0..=1`. `fade_len == 0` or `t >= fade_len` で 1.0 (= 完全
/// pass through)。 RT path で呼ばれるので branch 1 つの fast path を
/// 最初に置く。
///
/// 各 curve の数式 (`docs/plan_audio_clip.md` §3.5):
/// - `Linear`: `t / fade_len`
/// - `Exponential`: `(t / fade_len)^2`
/// - `SCurve`: `0.5 - 0.5 * cos(π * t / fade_len)` (= equal-power に
///   近い、 Auto-Crossfade で重なり時のクリップ防止に推奨)
#[inline]
pub fn fade_envelope(t: u64, fade_len: u64, curve: FadeCurve) -> f32 {
    if fade_len == 0 || t >= fade_len {
        return 1.0;
    }
    fade_curve_at((t as f32) / (fade_len as f32), curve)
}

/// Fade カーブそのもの: 正規化した進度 `progress` (0 = fade 開始 = 無音、
/// 1 = fade 終了 = フル) を 0..=1 のゲインへ写す。 **fade の形を決める唯一の式**。
///
/// 音 ([`fade_envelope`] 経由の `daw_audio::audio_clip_renderer`)、 映像
/// (`daw_gui::video_playback`)、 画像 (`daw_gui::image_compose`)、 字幕
/// (`daw_gui::text_compose`)、 そして **アレンジ画面の fade 描画**
/// (`daw_gui::widgets::arrangement`) が全部ここを呼ぶ。 r.md #38 以前は
/// 同じ 3 行 match が 4 箇所にコピーされ、 描画だけが式を持たず直線 1 本で
/// 代用していたため 「線の形が curve を反映しない」 状態だった。
///
/// 数式 (`docs/plan_audio_clip.md` §3.5):
/// - `Linear`: `x`
/// - `Exponential`: `x^2`
/// - `SCurve`: `0.5 - 0.5 * cos(π x)` (= equal-power に近い)
#[inline]
#[must_use]
pub fn fade_curve_at(progress: f32, curve: FadeCurve) -> f32 {
    let x = progress.clamp(0.0, 1.0);
    match curve {
        FadeCurve::Linear => x,
        FadeCurve::Exponential => x * x,
        FadeCurve::SCurve => 0.5 - 0.5 * (std::f32::consts::PI * x).cos(),
    }
}

/// **時間軸**の換算比: engine の 1 output frame が source の何 frame に当たるか
/// (= `source_sr / engine_sr`)。 pitch とは独立で、 4 mode すべてで「出力 sample →
/// source frame」 の写像に必ず掛かる。 `engine_sample_rate == 0` は退化入力として
/// `1.0` (= 補正なし)。
///
/// 単位: `output_frame_at_engine_sr * sample_rate_ratio = source_frame`。
/// Reverse は別経路 (caller 側で `source_len - 1 - source_pos` する)
/// なのでここでは扱わない。
#[inline]
pub fn sample_rate_ratio(source_sample_rate: u32, engine_sample_rate: u32) -> f64 {
    if engine_sample_rate == 0 {
        1.0
    } else {
        f64::from(source_sample_rate) / f64::from(engine_sample_rate)
    }
}

/// **ピッチ軸**の比: semitone → 再生比 (`2^(n/12)`)。 時間軸とは独立の量で、
/// mode ごとに合成先が変わる:
/// - `Raw` / `Repitch` (tape): source を読む速度そのものに掛かる → 長さも変わる
/// - `Stretch` (granular) / `Slice`: grain / slice の **内部読み出し速度**にだけ
///   掛かり、 grain / slice の **配置**には掛からない → 長さを変えずに移調する
///
/// 非有限 / 極端な入力は ±120 半音 (= ±10 oct) に clamp して比が inf / NaN に
/// ならないようにする (RT path で使うので panic / alloc なし)。
#[inline]
pub fn pitch_factor(semitones: f32) -> f64 {
    if !semitones.is_finite() {
        return 1.0;
    }
    2f64.powf(f64::from(semitones.clamp(-120.0, 120.0)) / 12.0)
}

/// clip 伸縮量 = source の native 再生長 (秒) /
/// event の配置長 (秒、 nominal bpm 基準) の比。 `1.0` で trim 相当 (= source を
/// そのまま native rate で再生)、 `< 1.0` で event slot の方が長い → source を
/// 引き伸ばす (slow)、 `> 1.0` で event slot が短い → 詰める (fast)。 engine SR に
/// 依らない (秒で比較するので source SR ≠ engine SR でも一意)。 退化入力
/// (SR/bpm/長さ 0) は `1.0` (= 伸縮なし) を返す defensive。 compile 時 (off-RT) に
/// 1 回だけ呼び、 render loop では結果を掛けるだけにする。
#[inline]
pub fn stretch_ratio_for(
    native_frames: u64,
    source_sample_rate: u32,
    event_length_beats: f64,
    bpm: f32,
) -> f64 {
    if source_sample_rate == 0 || bpm <= 0.0 || event_length_beats <= 0.0 || native_frames == 0 {
        return 1.0;
    }
    let native_secs = native_frames as f64 / f64::from(source_sample_rate);
    let event_secs = event_length_beats * 60.0 / f64::from(bpm);
    if event_secs <= 1e-9 {
        return 1.0;
    }
    native_secs / event_secs
}

/// 波形**描画**の 1 区間。 出力の event-local 拍区間 `[start_beat, end_beat)` に
/// source frame 範囲 `[source_start, source_end)` を **線形に** 写す。
/// `reversed` なら区間内は右→左 (= source を末尾から読む)。
///
/// 「1 event = 1 連続レンジ」 を前提にしていた旧 `audible_source_span` の置き換え
/// (r.md #41)。 Slice の onset ごとの区分配置 / gap、 warp marker の区分線形、
/// 逆再生を **1 つの型**で表せるので、 アレンジビューもオーディオエディタも
/// mode 別の描画分岐を持たずに済む。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WaveSpan {
    /// 区間開始 (event-local 拍)。
    pub start_beat: f64,
    /// 区間終了 (event-local 拍)。 次 span との隙間 = 無音 (gap)。
    pub end_beat: f64,
    /// 区間が読む source 範囲 (絶対 frame、 `source_start < source_end`)。
    pub source_start: u64,
    pub source_end: u64,
    /// 区間内を source 末尾から読むか (= 描画も左右反転する)。
    pub reversed: bool,
    /// この span が **音の立ち上がり** (= slice trigger / event 頭) か。
    /// `false` は直前 span からの連続で、 warp marker 境界や tempo curve の
    /// 区分線形化のために分割されただけ。 スライス境界の縦線を出す UI は
    /// これで間引く (分割 span ごとに線を出すと嘘になる)。
    pub head: bool,
}

/// 拍 ↔ tempo の写像。 描画が engine と同じ tempo 追従を得るための唯一の口。
///
/// engine は buffer ごとに `evaluate_song_tempo(song, playhead_beats)` で
/// `current_bpm` を評価し、 `samples_per_beat` を作り直す
/// (`daw_audio::engine` / `audio_clip_renderer::render_audio_events`)。
/// このため **native rate 再生** (`Raw` 全体と `Slice` の slice 本体) は
/// 「1 拍あたりの source 消費量」 が `current_bpm` に反比例して変わる。
/// 一方 trigger / grain の **配置** は `tempo_follow_ratio` で `current_bpm` が
/// 約分されるので `nominal_bpm` (= compile 時 `song.bpm`) だけで決まる。
///
/// スカラー bpm を渡していた旧 API はこの区別ができず、 SongTempo automation を
/// 持つ曲で「描いた波形と鳴る音がずれる」 (r.md #41 の不変条件の破れ) ため、
/// 描画側もこの型を経由する。 SongTempo lane が無ければ `Constant` 相当で
/// 従来と完全に同じ (曲線評価コストも掛からない)。
///
/// **既知の限界**: engine は automation の上に song modulation
/// (LFO / MSEG → `SongTempo`) を重ねるが、 modulator の位相は audio thread が
/// 持つので GUI からは再現できない。 変調中の tempo は automation 値で近似する。
#[derive(Debug, Clone, Copy)]
pub struct TempoMap<'a> {
    /// compile 時 (`song.bpm`) 基準の nominal bpm。 `stretch_ratio` / trigger 配置に使う。
    nominal_bpm: f64,
    /// SongTempo automation を持つ song (`None` = 定数 tempo)。
    song: Option<&'a crate::model::Song>,
}

impl<'a> TempoMap<'a> {
    /// 定数 tempo (SongTempo lane 無し / テスト用)。
    #[must_use]
    pub fn constant(bpm: f32) -> Self {
        Self { nominal_bpm: f64::from(bpm), song: None }
    }

    /// song から構築する。 有効な `SongTempo` lane がある場合だけ曲線評価を有効にする
    /// (無い曲では `evaluate_song_tempo` を 1 度も呼ばない)。
    #[must_use]
    pub fn from_song(song: &'a crate::model::Song) -> Self {
        let has_curve = song.song_lanes.iter().any(|l| {
            l.enabled && matches!(l.target, crate::model::AutomationTarget::SongTempo)
        });
        Self {
            nominal_bpm: f64::from(song.bpm),
            song: has_curve.then_some(song),
        }
    }

    /// compile 時基準の bpm (= `RenderedEvent.nominal_bpm`)。
    #[must_use]
    pub fn nominal_bpm(&self) -> f64 {
        self.nominal_bpm
    }

    /// tempo が曲線でない (= 全拍で `nominal_bpm`) か。 `true` なら描画は
    /// 区分線形化せず閉形式 1 span で済む。
    #[must_use]
    pub fn is_constant(&self) -> bool {
        self.song.is_none()
    }

    /// song 絶対拍 `beat` での実効 bpm (= engine の `current_bpm`)。
    #[must_use]
    pub fn bpm_at(&self, beat: f64) -> f64 {
        match self.song {
            Some(s) => f64::from(crate::automation::evaluate_song_tempo(s, beat)),
            None => self.nominal_bpm,
        }
    }
}

/// `(beat, pos_in_event)` の線形区間を「実際に鳴る範囲」 に切り詰めて [`WaveSpan`] へ積む。
///
/// `pos_in_event` は `source_start_frames` 起点の source 位置 (= engine の
/// `source_frame_lerp` に渡る値)。 engine は `pos < 0` / `pos >= source_len` を
/// `None` (= 無音) で返すので、 窓外を指す部分は区間ごと切り詰める (beat 側も
/// 同じ比率で縮める)。 区間の source が減少方向 (warp marker の逆行 / `reversed`)
/// なら `reversed` を立てて正規化した範囲を積む。
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::too_many_arguments)]
fn push_wave_span(
    out: &mut Vec<WaveSpan>,
    source_start: u64,
    window: u64,
    event_reversed: bool,
    b0: f64,
    b1: f64,
    p0: f64,
    p1: f64,
    head: bool,
) {
    if !b0.is_finite() || !b1.is_finite() || !p0.is_finite() || !p1.is_finite() {
        return;
    }
    if b1 <= b0 || window == 0 {
        return;
    }
    let win = window as f64;
    let d = p1 - p0;
    let (mut t_lo, mut t_hi) = (0.0_f64, 1.0_f64);
    if d.abs() < 1e-9 {
        // source 位置が動かない区間 (warp の退化 / 窓手前の clamp)。 窓内なら
        // 「1 frame を保持」 として残す (engine も同じ source を読み続ける)。
        if p0 < 0.0 || p0 >= win {
            return;
        }
    } else {
        let ta = -p0 / d;
        let tb = (win - p0) / d;
        let (lo, hi) = if ta <= tb { (ta, tb) } else { (tb, ta) };
        t_lo = t_lo.max(lo);
        t_hi = t_hi.min(hi);
        if t_hi <= t_lo {
            return;
        }
    }
    let nb0 = b0 + (b1 - b0) * t_lo;
    let nb1 = b0 + (b1 - b0) * t_hi;
    if nb1 <= nb0 {
        return;
    }
    let np0 = p0 + d * t_lo;
    let np1 = p0 + d * t_hi;
    let mut reversed = np1 < np0;
    let (lo, hi) = if reversed { (np1, np0) } else { (np0, np1) };
    // `reversed` event は窓全体を反転して読む (`source_frame_lerp`)。 slice / grain
    // 単位ではなく窓基準なので、 pos 範囲を窓の反対側へ写して向きを反転する。
    let (a, b) = if event_reversed {
        reversed = !reversed;
        (win - hi, win - lo)
    } else {
        (lo, hi)
    };
    // source 範囲は必ず窓 `[source_start, source_start + window)` の内側に収める
    // (engine は窓外を無音で返すので、 窓外を指す span を描いてはいけない)。
    let window_end = source_start.saturating_add(window);
    let s0 = source_start
        .saturating_add(a.clamp(0.0, win) as u64)
        .min(window_end - 1);
    let s1 = source_start
        .saturating_add(b.clamp(0.0, win) as u64)
        .clamp(s0 + 1, window_end);
    out.push(WaveSpan {
        start_beat: nb0,
        end_beat: nb1,
        source_start: s0,
        source_end: s1,
        reversed,
        head,
    });
}

/// **native rate** (= 実時間で source を消費する) 区間を span 列にする。
/// `Raw` の全体と `Slice` の slice 本体が使う共通経路。
///
/// engine は `event_local`(出力 sample) を **その buffer の** `samples_per_beat`
/// (= `current_bpm` 由来) で作り、 `source_pos = event_local × read_stride` に
/// 再 anchor する (`repitch_source_pos` の不連続分岐 / `slice_sample_at`)。
/// 展開すると 1 拍あたりの消費量は `source_sr × 60 / current_bpm × pitch` で、
/// **tempo automation があると拍に対して非線形**になる。 定数 tempo なら
/// 閉形式 1 span、 曲線なら拍を細かく刻んだ区分線形 span 列で表す
/// (最初の 1 本だけ `head = true`)。
///
/// - `t0` / `t_limit`: event-local の開始拍 / 上限拍 (次 trigger or event 長)
/// - `p0` / `p_limit`: 開始時の `pos_in_event` / 到達したら鳴り止む `pos_in_event`
/// - `fpb_at`: event-local 拍 `t` における「1 拍あたり消費 source frame」
#[allow(clippy::too_many_arguments)]
fn push_native_rate_spans(
    out: &mut Vec<WaveSpan>,
    source_start: u64,
    window: u64,
    event_reversed: bool,
    constant_tempo: bool,
    t0: f64,
    t_limit: f64,
    p0: f64,
    p_limit: f64,
    fpb_at: &dyn Fn(f64) -> f64,
) {
    /// tempo 曲線を区分線形化する刻み (拍)。 1/8 拍は 120 BPM で 62 ms 相当。
    const STEP_BEATS: f64 = 1.0 / 8.0;
    /// 1 区間あたりの分割上限 (退化入力でループが伸びないための防御)。
    const MAX_PIECES: usize = 512;

    if t_limit <= t0 || p_limit <= p0 {
        return;
    }
    if constant_tempo {
        let fpb = fpb_at(t0);
        if !fpb.is_finite() || fpb <= 0.0 {
            return;
        }
        let end = (t0 + (p_limit - p0) / fpb).min(t_limit);
        push_wave_span(
            out,
            source_start,
            window,
            event_reversed,
            t0,
            end,
            p0,
            p0 + (end - t0) * fpb,
            true,
        );
        return;
    }
    let mut t = t0;
    let mut p = p0;
    let mut head = true;
    for _ in 0..MAX_PIECES {
        if t >= t_limit || p >= p_limit {
            break;
        }
        let mut t_next = (t + STEP_BEATS).min(t_limit);
        let fpb = fpb_at(t_next);
        if !fpb.is_finite() || fpb <= 0.0 {
            break;
        }
        // engine は「event 頭からの拍差 × その時点の 1 拍あたり消費量」 で
        // source 位置を再 anchor する (積分ではない)。 同じ式で端点を作る。
        let mut p_next = p0 + (t_next - t0) * fpb;
        if p_next >= p_limit {
            // 鳴り止む位置を区間内で線形に解いて最後の 1 本にする。
            if p_next > p {
                t_next = t + (p_limit - p) * (t_next - t) / (p_next - p);
            }
            p_next = p_limit;
        }
        push_wave_span(
            out,
            source_start,
            window,
            event_reversed,
            t,
            t_next,
            p,
            p_next,
            head,
        );
        head = false;
        if p_next >= p_limit {
            break;
        }
        t = t_next;
        p = p_next;
    }
}

/// `StretchMode::Stretch` + warp marker (>= 2 本) の区分線形 span。
/// `granular_sample_at` の warp path (`warp_source_frame` で beat → source frame)
/// と同じ写像を、 marker 境界で区切った線形区間の列として返す。
fn warp_wave_spans(
    out: &mut Vec<WaveSpan>,
    event: &crate::model::AudioEvent,
    source_start: u64,
    window: u64,
    len_beats: f64,
) {
    // engine (compile) は locked_beat 昇順 + dedup 済を前提にするので、 描画も同じ
    // 正規化を通す (未 sort データで描画と再生がずれるのを防ぐ)。
    let mut sorted_buf: Vec<BeatMarker>;
    let markers: &[BeatMarker] = if event
        .beat_markers
        .windows(2)
        .all(|w| w[1].locked_beat - w[0].locked_beat > 1e-9)
    {
        &event.beat_markers
    } else {
        sorted_buf = event.beat_markers.clone();
        sorted_buf.sort_by(|a, b| a.locked_beat.total_cmp(&b.locked_beat));
        sorted_buf.dedup_by(|a, b| (a.locked_beat - b.locked_beat).abs() < 1e-9);
        &sorted_buf
    };
    if markers.len() < 2 {
        return;
    }
    let mut beats: Vec<f64> = Vec::with_capacity(markers.len() + 2);
    beats.push(0.0);
    for m in markers {
        if m.locked_beat > 1e-9 && m.locked_beat < len_beats - 1e-9 {
            beats.push(m.locked_beat);
        }
    }
    beats.push(len_beats);
    let base = source_start as f64;
    let mut head = true;
    for w in beats.windows(2) {
        let (b0, b1) = (w[0], w[1]);
        if b1 - b0 <= 1e-9 {
            continue;
        }
        let (Some(sf0), Some(sf1)) = (
            warp_source_frame(b0, markers),
            warp_source_frame(b1, markers),
        ) else {
            continue;
        };
        // engine は grain **ごと** に `(warp_source_frame(beat) - source_start).max(0.0)`
        // を評価する (`granular_sample_at`) ので、 窓手前を指す拍区間は
        // 「先頭 frame を保持 (flat)」 → その先は本来の傾き、 という区分写像になる。
        // 端点だけ clamp してから線形補間すると別形 (圧縮された 1 本の線) に
        // なってしまうので、 source_start を跨ぐ区間はその交点で分割する。
        let (p0, p1) = (sf0 - base, sf1 - base);
        if (p0 < 0.0) != (p1 < 0.0) && (p1 - p0).abs() > 1e-9 {
            let t = (-p0) / (p1 - p0);
            let bx = b0 + (b1 - b0) * t;
            if p0 < 0.0 {
                // 前半 flat (先頭 frame 保持) → 後半が本来の傾き。
                push_wave_span(out, source_start, window, event.reversed, b0, bx, 0.0, 0.0, head);
                head = false;
                push_wave_span(out, source_start, window, event.reversed, bx, b1, 0.0, p1, head);
            } else {
                push_wave_span(out, source_start, window, event.reversed, b0, bx, p0, 0.0, head);
                head = false;
                push_wave_span(out, source_start, window, event.reversed, bx, b1, 0.0, 0.0, head);
            }
        } else {
            push_wave_span(
                out,
                source_start,
                window,
                event.reversed,
                b0,
                b1,
                p0.max(0.0),
                p1.max(0.0),
                head,
            );
        }
        head = false;
    }
}

/// `StretchMode::Slice` の span 列 (`slice_sample_at` と同じ写像)。
///
/// - trigger の event-local 拍 = `onsets[i] / place_fpb`
///   (`place_fpb = source_frames_per_beat × stretch`。 engine の
///   `onsets[i] / (tempo_ratio × time_stride)` を engine SR / current bpm が
///   約分された形で表したもの)
/// - slice 本体は **伸縮しない** ので、 鳴る拍数 = `slice の source 長 / read_fpb`
///   (`read_fpb = source_frames_per_beat × pitch`)
/// - 次 trigger / event 末尾 / 窓末尾のいずれか早い方で打ち切り (= cut)、
///   残りは無音 (= gap)
///
/// slice 本体は native rate なので tempo automation で 1 拍あたりの消費量が
/// 変わる。 `read_fpb_at` (event-local 拍 → 1 拍あたり消費 frame) 経由で
/// [`push_native_rate_spans`] に委ねる。
#[allow(clippy::too_many_arguments)]
fn slice_wave_spans(
    out: &mut Vec<WaveSpan>,
    event: &crate::model::AudioEvent,
    source_start: u64,
    window: u64,
    len_beats: f64,
    place_fpb: f64,
    read_fpb_at: &dyn Fn(f64) -> f64,
    constant_tempo: bool,
) {
    // compile (`compile_audio_schedule`) と同じ sort + dedup。 既に厳密増加なら借用のまま。
    let mut sorted_buf: Vec<u64>;
    let onsets: &[u64] = if event.onsets.windows(2).all(|w| w[0] < w[1]) {
        &event.onsets
    } else {
        sorted_buf = event.onsets.clone();
        sorted_buf.sort_unstable();
        sorted_buf.dedup();
        &sorted_buf
    };
    let win = window as f64;
    for (i, &o) in onsets.iter().enumerate() {
        let o_f = o as f64;
        let start_beat = o_f / place_fpb;
        if start_beat >= len_beats {
            // trigger が event 外 → これ以降の slice は鳴らない (直前 slice の cut
            // だけは下の next_trigger_beat 経由で既に効いている)。
            break;
        }
        let next = onsets.get(i + 1).copied();
        // engine の `slice_source_end` (= 次 onset、 無ければ窓末尾)。 窓を越える
        // onset は `source_frame_lerp` が無音を返すので窓末尾で頭打ち。
        let src_end = next.map_or(win, |n| (n as f64).min(win));
        if o_f >= win || src_end <= o_f {
            continue;
        }
        let next_trigger_beat = next.map_or(f64::INFINITY, |n| n as f64 / place_fpb);
        push_native_rate_spans(
            out,
            source_start,
            window,
            event.reversed,
            constant_tempo,
            start_beat,
            next_trigger_beat.min(len_beats),
            o_f,
            src_end,
            read_fpb_at,
        );
    }
}

/// 波形**描画**の SSoT: この audio event を「どの拍区間に source のどの範囲を描くか」
/// の列 ([`WaveSpan`]) に展開する。 engine (`render_audio_events`) と同じ時間写像を
/// engine SR / current bpm に依らない単位で表すので、 これで描けば
/// 「見えている波形 = 聞こえる音」 になる。
///
/// mode ごとの内訳 (`nominal_fpb = source_sr × 60 / nominal_bpm`、
/// `stretch =` [`stretch_ratio_for`]、 `pitch =` [`pitch_factor`]):
/// - `Raw`: 1 拍あたり `source_sr × 60 / current_bpm × pitch` frame 消費
///   (伸縮に追従しない = **実時間** で source を消費するので tempo に依存する)
/// - `Repitch`: 1 span、 `nominal_fpb × stretch × pitch` (tape 式、 tempo 不変)
/// - `Stretch`: warp marker が 2 本以上なら marker 区間ごとの区分線形、 無ければ
///   1 span で `nominal_fpb × stretch` (ピッチは長さに影響しない、 tempo 不変)
/// - `Slice`: onset ごとに 1 slice。 trigger 拍 = `onsets[i] / (nominal_fpb × stretch)`
///   で **tempo 不変**、 slice 本体は Raw と同じ native rate なので、
///   伸ばせば **gap**、 詰めれば **cut**
///
/// `tempo` が曲線 (SongTempo automation) のときは、 native rate 区間だけ拍に対して
/// 非線形になるので区分線形の span 列に分割する (2 本目以降は `head = false`)。
/// `event_start_beat` はその評価に使う **song 絶対拍** (= clip 開始拍 + event 開始拍)。
///
/// どの mode でも「窓を鳴らし切って余った」 分は span が張られない (= 無音)。
/// `reversed` event は窓全体を反転して読むので、 span の source 範囲を反対側へ
/// 写して `reversed` を立てる。 退化入力 (0 長 / 0 窓 / bpm 0 / SR 0) は
/// 「窓を event 長いっぱいに」 の 1 span。
///
/// 毎フレームの描画 path から呼ばれるので、 結果は呼び出し側の `Vec` に積む
/// (先頭で `clear` する)。
///
/// **この写像は `daw_audio::audio_clip_renderer` の `render_audio_events` /
/// `granular_sample_at` / `slice_sample_at` と一致していなければならない**
/// (= 描いた波形と鳴る音が一致する条件)。 束縛テストは daw_audio 側の
/// `wave_span_binding_tests` にある。
pub fn event_wave_spans(
    event: &crate::model::AudioEvent,
    source_sample_rate: u32,
    tempo: &TempoMap<'_>,
    event_start_beat: f64,
    out: &mut Vec<WaveSpan>,
) {
    out.clear();
    let source_start = event.source_start_frames;
    let window = event.source_end_frames.saturating_sub(source_start);
    let len_beats = event.event_length_beats.max(0.0);
    if window == 0 || len_beats <= 0.0 {
        return;
    }
    let rev = event.reversed;
    let whole = |out: &mut Vec<WaveSpan>| {
        push_wave_span(out, source_start, window, rev, 0.0, len_beats, 0.0, window as f64, true);
    };
    let nominal_bpm = tempo.nominal_bpm();
    if nominal_bpm <= 0.0 || source_sample_rate == 0 {
        whole(out);
        return;
    }
    let src_sr = f64::from(source_sample_rate);
    // 配置 (trigger / grain / tape) 側の基準。 engine では `tempo_follow_ratio` で
    // current_bpm が約分されるので nominal 固定。
    let nominal_fpb = src_sr * 60.0 / nominal_bpm;
    #[allow(clippy::cast_possible_truncation)]
    let stretch = stretch_ratio_for(window, source_sample_rate, len_beats, nominal_bpm as f32);
    let pitch = pitch_factor(event.pitch_semitones);
    let constant_tempo = tempo.is_constant();
    // native rate 側は event-local 拍 t 時点の **current_bpm** で決まる。
    let read_fpb_at = |t: f64| src_sr * 60.0 / tempo.bpm_at(event_start_beat + t) * pitch;
    // 1 拍あたり `fpb` frame 進む単一 span (窓を超える分は push_wave_span が切る)。
    let uniform = |out: &mut Vec<WaveSpan>, fpb: f64| {
        if fpb.is_finite() && fpb > 0.0 {
            push_wave_span(
                out,
                source_start,
                window,
                rev,
                0.0,
                len_beats,
                0.0,
                len_beats * fpb,
                true,
            );
        } else {
            whole(out);
        }
    };
    match event.stretch_mode {
        // Raw = 窓全体を 1 slice とした native rate 再生 (`slice_sample_at` の
        // onsets 空 early-return と同じ写像)。
        StretchMode::Raw => push_native_rate_spans(
            out,
            source_start,
            window,
            rev,
            constant_tempo,
            0.0,
            len_beats,
            0.0,
            window as f64,
            &read_fpb_at,
        ),
        StretchMode::Repitch => uniform(out, nominal_fpb * stretch * pitch),
        StretchMode::Stretch => {
            if event.beat_markers.len() >= 2 {
                warp_wave_spans(out, event, source_start, window, len_beats);
                if out.is_empty() {
                    // marker が全部退化 → granular も uniform に fallback する。
                    uniform(out, nominal_fpb * stretch);
                }
            } else {
                uniform(out, nominal_fpb * stretch);
            }
        }
        StretchMode::Slice => {
            let place_fpb = nominal_fpb * stretch;
            if event.onsets.is_empty() || !(place_fpb.is_finite() && place_fpb > 0.0) {
                // `slice_sample_at` の onsets 空 early-return = 窓全体を 1 slice。
                push_native_rate_spans(
                    out,
                    source_start,
                    window,
                    rev,
                    constant_tempo,
                    0.0,
                    len_beats,
                    0.0,
                    window as f64,
                    &read_fpb_at,
                );
            } else {
                slice_wave_spans(
                    out,
                    event,
                    source_start,
                    window,
                    len_beats,
                    place_fpb,
                    &read_fpb_at,
                    constant_tempo,
                );
            }
        }
    }
}

/// [`event_wave_spans`] の逆写像: event-local 拍 `beat` の位置で **実際に鳴っている**
/// source frame。 span の外 (= スライス間の無音 / 鳴り終わったあと) は `None`。
///
/// 「波形上のこの位置は source のどこか」 を UI が知る唯一の口 (warp marker の
/// Alt+click 追加など)。 span 側と同じ写像なので、 描いた波形とクリック位置が
/// 一致する (旧実装は `local / event_length × 窓` の uniform 近似を直書きしており、
/// Slice / ピッチ変更したクリップでは見えている波形と違う source を指していた)。
#[must_use]
pub fn source_frame_at_beat(spans: &[WaveSpan], beat: f64) -> Option<f64> {
    let s = spans
        .iter()
        .find(|s| beat >= s.start_beat && beat < s.end_beat)?;
    let span_beats = s.end_beat - s.start_beat;
    if span_beats <= 0.0 {
        return None;
    }
    let f = (beat - s.start_beat) / span_beats;
    let len = (s.source_end - s.source_start) as f64;
    Some(if s.reversed {
        s.source_end as f64 - len * f
    } else {
        s.source_start as f64 + len * f
    })
}

/// source 進度 = clip の手動 `stretch_ratio` (= [`stretch_ratio_for`]、 nominal
/// bpm 基準の native長/配置長) × tempo 追従比 (`current_bpm / nominal_bpm`)。
/// この 2 つを掛けると、 clip は **拍数を固定したまま** tempo 変化に追従して
/// 伸縮する (= MIDI clip と同じ挙動: project tempo が変わると実時間長が変わる)。
///
/// 数式上、 `event_length_beats` が固定なら、 この戻り値は nominal_bpm の取り方に
/// **不変**:
/// `stretch_ratio * current/nominal = (native_secs * nominal / (elb*60)) *
/// current/nominal = native_secs * current / (elb*60)`。
/// よって schedule の再コンパイル (= nominal_bpm が現 song.bpm に更新され、
/// stretch_ratio も同時に再算出される) を跨いでも追従結果が一致する。
///
/// `current_bpm` は呼び出し側で、 Stretch (granular) は LP smoothed な値 (=
/// click 抑制、 grain source jump 抑制)、 Repitch / Slice は instant な値 (=
/// pitch / slice trigger の追随性優先) を渡す。 `nominal_bpm <= 0` は退化入力と
/// して `stretch_ratio` を素通し (= 追従なし) する defensive。 RT path で呼ばれる
/// ので alloc / panic free。
#[inline]
pub fn tempo_follow_ratio(stretch_ratio: f64, current_bpm: f64, nominal_bpm: f64) -> f64 {
    if nominal_bpm <= 0.0 {
        return stretch_ratio;
    }
    stretch_ratio * (current_bpm / nominal_bpm)
}

/// Warp marker (r.md #8 B12) による非一様タイムストレッチの source 写像。
/// event-local beat `event_beat` に対応する source frame を、 warp markers
/// (`locked_beat` 昇順・dedup 済を前提) の区分線形補間で返す。 marker 範囲外は
/// 端セグメントの傾きで外挿する (Ableton warp 同様)。 markers が 2 個未満 /
/// 補間に使うセグメントが退化 (同 `locked_beat`) の場合は `None` (caller は
/// uniform stretch に fallback)。 純関数 (granular render と検算で共有する SSoT)。
pub fn warp_source_frame(event_beat: f64, markers: &[BeatMarker]) -> Option<f64> {
    if markers.len() < 2 {
        return None;
    }
    // 区分線形補間 (退化セグメント = 同 locked_beat は None)。
    let lerp = |a: &BeatMarker, b: &BeatMarker| -> Option<f64> {
        let db = b.locked_beat - a.locked_beat;
        if db.abs() < 1e-12 {
            return None;
        }
        let slope = (b.source_frame as f64 - a.source_frame as f64) / db;
        Some(a.source_frame as f64 + slope * (event_beat - a.locked_beat))
    };
    let n = markers.len();
    if event_beat <= markers[0].locked_beat {
        lerp(&markers[0], &markers[1]) // 先頭セグメントの傾きで外挿
    } else if event_beat >= markers[n - 1].locked_beat {
        lerp(&markers[n - 2], &markers[n - 1]) // 末尾セグメントの傾きで外挿
    } else {
        let i = markers
            .partition_point(|m| m.locked_beat <= event_beat)
            .saturating_sub(1);
        lerp(&markers[i], &markers[i + 1])
    }
}

/// B12 (r.md #8): onset (source frame) を beat grid に snap した warp markers に
/// 変換する (auto-warp の core)。 各 onset の uniform 配置 beat
/// (`onset / source_len × length_beats`) を `1/grid` に量子化 (grid=4 で 16th note)
/// し source↔beat を pin する。 先頭 (beat 0) / 末尾 (`length_beats`) の anchor を
/// 必ず含め、 `locked_beat` が厳密増加するよう間引く (warp の monotonic 前提)。 退化
/// 入力 (source_len / grid / length 0) や transient が全て grid 上で潰れる場合は
/// anchor 2 件のみ → caller は「warp 不要 = uniform」 とみなせる。 純関数 (off-RT)。
pub fn warp_markers_from_onsets(
    onsets: &[u64],
    source_start: u64,
    source_len: u64,
    length_beats: f64,
    grid: u32,
) -> Vec<BeatMarker> {
    let end_frame = source_start.saturating_add(source_len);
    let end_beat = length_beats.max(0.0);
    let mut markers = vec![BeatMarker {
        source_frame: source_start,
        locked_beat: 0.0,
    }];
    if source_len == 0 || length_beats <= 0.0 || grid == 0 {
        markers.push(BeatMarker {
            source_frame: end_frame,
            locked_beat: end_beat,
        });
        return markers;
    }
    let g = f64::from(grid);
    let mut last_beat = 0.0_f64;
    for &onset_rel in onsets {
        if onset_rel == 0 || onset_rel >= source_len {
            continue;
        }
        let b_uniform = onset_rel as f64 / source_len as f64 * length_beats;
        let b_snapped = (b_uniform * g).round() / g;
        if b_snapped > last_beat + 1e-6 && b_snapped < end_beat - 1e-6 {
            markers.push(BeatMarker {
                source_frame: source_start + onset_rel,
                locked_beat: b_snapped,
            });
            last_beat = b_snapped;
        }
    }
    markers.push(BeatMarker {
        source_frame: end_frame,
        locked_beat: end_beat,
    });
    markers
}

/// 手動 warp marker 編集の許容下限ギャップ (locked_beat)。 `warp_source_frame` は
/// 同 `locked_beat` セグメントを退化 (None) 扱いするので、 隣接 marker は最低この差を保つ。
pub const WARP_MARKER_MIN_GAP: f64 = 1e-4;

/// B12-manual (r.md #8): warp marker `idx` の出力位置 (`locked_beat`) を手動で動かす。
/// `source_frame` は据え置き (= その source 位置を新しい output beat に pin し直す = stretch)。
/// `warp_source_frame` の前提 (locked_beat 厳密増加) を壊さないよう、 隣接 marker の間
/// (`±WARP_MARKER_MIN_GAP`) に clamp する (端 marker は外側に自由)。 純関数 (off-RT、 test 可能)。
pub fn move_warp_marker(markers: &mut [BeatMarker], idx: usize, new_locked_beat: f64) {
    if idx >= markers.len() {
        return;
    }
    let lo = if idx > 0 {
        markers[idx - 1].locked_beat + WARP_MARKER_MIN_GAP
    } else {
        f64::NEG_INFINITY
    };
    let hi = if idx + 1 < markers.len() {
        markers[idx + 1].locked_beat - WARP_MARKER_MIN_GAP
    } else {
        f64::INFINITY
    };
    // 隣接が極端に近い退化ケース (lo > hi) は中点に置いて順序を保つ (clamp の panic 回避)。
    markers[idx].locked_beat = if lo > hi {
        (lo + hi) * 0.5
    } else {
        new_locked_beat.clamp(lo, hi)
    };
}

/// B12-manual (r.md #8): warp marker を追加する。 `locked_beat` 昇順を保って挿入し、 挿入位置の
/// `BeatMarker` index を返す (`None` = 既存 marker と locked_beat が近すぎる退化で skip)。
pub fn add_warp_marker(
    markers: &mut Vec<BeatMarker>,
    source_frame: u64,
    locked_beat: f64,
) -> Option<usize> {
    if markers
        .iter()
        .any(|m| (m.locked_beat - locked_beat).abs() < WARP_MARKER_MIN_GAP)
    {
        return None;
    }
    let pos = markers.partition_point(|m| m.locked_beat < locked_beat);
    markers.insert(pos, BeatMarker { source_frame, locked_beat });
    Some(pos)
}

/// B12-manual (r.md #8): warp marker `idx` を削除する (範囲外は no-op)。 markers が 2 件未満に
/// なれば `warp_source_frame` は None を返し uniform stretch に degrade する (= warp 解除)。
pub fn delete_warp_marker(markers: &mut Vec<BeatMarker>, idx: usize) {
    if idx < markers.len() {
        markers.remove(idx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bm(source_frame: u64, locked_beat: f64) -> BeatMarker {
        BeatMarker { source_frame, locked_beat }
    }

    // ---- event_wave_spans (波形描画と再生の一致、 r.md #41) ----

    /// 48 kHz / 120 BPM → 1 拍 = 24000 source frame。 2 拍ぶん (48000 frame) の
    /// 「素直な」 event (= import / trim 直後の lockstep 状態) を作る。
    fn span_event(mode: StretchMode, window: u64, len_beats: f64, semis: f32) -> crate::model::AudioEvent {
        crate::model::AudioEvent {
            source_start_frames: 0,
            source_end_frames: window,
            event_length_beats: len_beats,
            stretch_mode: mode,
            pitch_semitones: semis,
            ..crate::model::AudioEvent::default()
        }
    }

    fn spans_of(event: &crate::model::AudioEvent) -> Vec<WaveSpan> {
        let mut out = Vec::new();
        event_wave_spans(event, 48_000, &TempoMap::constant(120.0), 0.0, &mut out);
        out
    }

    /// span 列が「拍の重なり無し・昇順」 という不変条件を満たすか (全 mode 共通)。
    fn assert_monotonic(spans: &[WaveSpan], label: &str) {
        for w in spans.windows(2) {
            assert!(
                w[1].start_beat >= w[0].end_beat - 1e-9,
                "{label}: span が重なっている {:?}",
                spans
            );
        }
        for s in spans {
            assert!(s.end_beat > s.start_beat, "{label}: 空 span {s:?}");
            assert!(s.source_end > s.source_start, "{label}: 空 source {s:?}");
        }
    }

    #[test]
    fn wave_spans_fill_clip_without_pitch() {
        // ピッチ 0 / 伸縮なしなら 4 mode すべて「窓を event 長いっぱいに」 1 span
        // (= 従来の連続波形と同じ絵。 ここが崩れると取り込み直後の clip が回帰する)。
        for mode in [
            StretchMode::Raw,
            StretchMode::Repitch,
            StretchMode::Stretch,
            StretchMode::Slice,
        ] {
            let spans = spans_of(&span_event(mode, 48_000, 2.0, 0.0));
            assert_eq!(spans.len(), 1, "{mode:?}: {spans:?}");
            let s = spans[0];
            assert!((s.start_beat - 0.0).abs() < 1e-9, "{mode:?}");
            assert!((s.end_beat - 2.0).abs() < 1e-9, "{mode:?}: got {}", s.end_beat);
            assert_eq!((s.source_start, s.source_end), (0, 48_000), "{mode:?}");
            assert!(!s.reversed);
        }
    }

    #[test]
    fn wave_spans_shrink_when_pitched_up_in_tape_modes() {
        // +1 oct = 2 倍速 → 窓は半分の拍数で鳴り終わり、 残りは無音 (span 無し)。
        for mode in [StretchMode::Raw, StretchMode::Repitch] {
            let spans = spans_of(&span_event(mode, 48_000, 2.0, 12.0));
            assert_eq!(spans.len(), 1, "{mode:?}");
            assert_eq!(spans[0].source_end, 48_000, "{mode:?}: 窓は全部鳴る");
            assert!((spans[0].end_beat - 1.0).abs() < 1e-9, "{mode:?}: {spans:?}");
        }
    }

    #[test]
    fn wave_spans_crop_when_pitched_down_in_tape_modes() {
        // -1 oct = 半分の速度 → clip に収まらず、 窓の前半だけが鳴る (= cut)。
        let spans = spans_of(&span_event(StretchMode::Raw, 48_000, 2.0, -12.0));
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].source_end, 24_000);
        assert!((spans[0].end_beat - 2.0).abs() < 1e-9, "{spans:?}");
    }

    #[test]
    fn wave_spans_ignore_pitch_for_placement_in_granular_mode() {
        // Stretch は配置が長さを決めるので、 移調しても span は clip 全長のまま。
        for semis in [12.0_f32, -12.0] {
            let spans = spans_of(&span_event(StretchMode::Stretch, 48_000, 2.0, semis));
            assert_eq!(spans.len(), 1, "{semis}");
            assert_eq!((spans[0].source_start, spans[0].source_end), (0, 48_000));
            assert!((spans[0].end_beat - 2.0).abs() < 1e-9, "{semis}: {spans:?}");
        }
    }

    #[test]
    fn wave_spans_stretched_clip_still_fills_in_stretch_mode() {
        // Shift ストレッチ済 (窓は 2 拍ぶんのまま clip は 4 拍) でも Stretch は充填。
        let spans = spans_of(&span_event(StretchMode::Stretch, 48_000, 4.0, 0.0));
        assert_eq!(spans.len(), 1);
        assert!((spans[0].end_beat - 4.0).abs() < 1e-9, "{spans:?}");
        // 同じ event を Raw にすると伸縮しないので、 窓は 2 拍で鳴り終わる。
        let spans = spans_of(&span_event(StretchMode::Raw, 48_000, 4.0, 0.0));
        assert_eq!(spans.len(), 1);
        assert!((spans[0].end_beat - 2.0).abs() < 1e-9, "{spans:?}");
    }

    // ---- Slice: onset ごとの区分配置 / gap / cut (r.md #41 の本丸) ----

    fn slice_event(window: u64, len_beats: f64, semis: f32, onsets: Vec<u64>) -> crate::model::AudioEvent {
        crate::model::AudioEvent {
            onsets,
            ..span_event(StretchMode::Slice, window, len_beats, semis)
        }
    }

    #[test]
    fn slice_spans_are_contiguous_when_not_stretched() {
        // 伸縮なし・ピッチ 0 → trigger 間隔と slice の鳴る長さが一致 → 隙間ゼロ。
        // (= 取り込み直後の Slice clip は従来の連続波形と同じ絵になる = 回帰しない)
        let spans = spans_of(&slice_event(48_000, 2.0, 0.0, vec![0, 12_000, 24_000, 36_000]));
        assert_eq!(spans.len(), 4, "{spans:?}");
        assert_monotonic(&spans, "contiguous");
        for (i, s) in spans.iter().enumerate() {
            assert!(
                (s.start_beat - i as f64 * 0.5).abs() < 1e-9,
                "slice {i} の trigger 拍: {spans:?}"
            );
            assert!(
                (s.end_beat - s.start_beat - 0.5).abs() < 1e-9,
                "slice {i} は次 trigger まで鳴り続ける (gap なし): {spans:?}"
            );
        }
        assert_eq!(spans[3].source_end, 48_000);
    }

    #[test]
    fn slice_spans_open_gaps_when_clip_is_stretched() {
        // clip を 2 倍に伸ばす (窓 2 拍ぶん / clip 4 拍 → stretch 0.5)。 trigger は
        // 2 倍に広がるが slice 本体は native rate なので、 各 slice の後ろに
        // 同じ長さの gap が空く (= Ableton Beats warp / Transient Loop Off)。
        let spans = spans_of(&slice_event(48_000, 4.0, 0.0, vec![0, 12_000, 24_000, 36_000]));
        assert_eq!(spans.len(), 4, "{spans:?}");
        assert_monotonic(&spans, "gap");
        for (i, s) in spans.iter().enumerate() {
            assert!(
                (s.start_beat - i as f64).abs() < 1e-9,
                "trigger は 1 拍ごと: {spans:?}"
            );
            assert!(
                (s.end_beat - s.start_beat - 0.5).abs() < 1e-9,
                "slice 本体は native rate の 0.5 拍: {spans:?}"
            );
        }
        // 末尾 slice の後ろ (3.5 → 4.0 拍) も無音。
        assert!(spans[3].end_beat < 4.0 - 1e-9);
    }

    #[test]
    fn slice_spans_cut_when_clip_is_compressed() {
        // clip を半分に詰める (窓 2 拍ぶん / clip 1 拍 → stretch 2.0)。 trigger が
        // 詰まるので各 slice は鳴り終わる前に次 trigger で切られる (= cut、 gap なし)。
        let spans = spans_of(&slice_event(48_000, 1.0, 0.0, vec![0, 12_000, 24_000, 36_000]));
        assert_eq!(spans.len(), 4, "{spans:?}");
        assert_monotonic(&spans, "cut");
        for (i, s) in spans.iter().enumerate() {
            assert!((s.start_beat - i as f64 * 0.25).abs() < 1e-9, "{spans:?}");
            assert!(
                (s.end_beat - s.start_beat - 0.25).abs() < 1e-9,
                "次 trigger で cut: {spans:?}"
            );
            // cut された分だけ source も短く読む (= 見えている波形が鳴る音)。
            assert_eq!(s.source_end - s.source_start, 6_000, "slice {i}: {spans:?}");
        }
    }

    #[test]
    fn slice_spans_shorten_when_pitched_up() {
        // Slice の移調は slice **本体**の読み速度だけを変える (trigger は動かない)。
        // +1 oct → 各 slice は半分の時間で鳴り終わり、 残りが gap になる。
        let spans = spans_of(&slice_event(48_000, 2.0, 12.0, vec![0, 12_000, 24_000, 36_000]));
        assert_eq!(spans.len(), 4, "{spans:?}");
        assert_monotonic(&spans, "pitched");
        for (i, s) in spans.iter().enumerate() {
            assert!((s.start_beat - i as f64 * 0.5).abs() < 1e-9, "trigger 不変: {spans:?}");
            assert!(
                (s.end_beat - s.start_beat - 0.25).abs() < 1e-9,
                "slice {i} は 2 倍速で鳴り終わる: {spans:?}"
            );
            assert_eq!(s.source_end - s.source_start, 12_000, "source は全部鳴る");
        }
    }

    #[test]
    fn slice_spans_without_onsets_degrade_to_single_slice() {
        // onsets 空 = `slice_sample_at` の early-return (窓全体を 1 slice、 native rate)。
        let spans = spans_of(&slice_event(48_000, 4.0, 0.0, Vec::new()));
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert!((spans[0].end_beat - 2.0).abs() < 1e-9, "伸縮しない: {spans:?}");
    }

    #[test]
    fn slice_spans_normalize_unsorted_onsets_like_compile() {
        // 未 sort / 重複 onsets でも compile (sort+dedup) と同じ列で描く。
        let a = spans_of(&slice_event(48_000, 4.0, 0.0, vec![24_000, 0, 12_000, 12_000]));
        let b = spans_of(&slice_event(48_000, 4.0, 0.0, vec![0, 12_000, 24_000]));
        assert_eq!(a, b, "sort+dedup 後の span 列は一致する");
    }

    #[test]
    fn slice_spans_ignore_onsets_outside_the_source_window() {
        // trigger 配置率は必ず `窓 / clip 長` なので、 窓外 (>= 窓) の onset は
        // event 末尾以降にしか trigger されない = 鳴らないし前 slice も切らない。
        // 描画も同じく無視する (窓 24000 / clip 2 拍 → onset 48000 は 4 拍地点)。
        let spans = spans_of(&slice_event(24_000, 2.0, 0.0, vec![0, 48_000]));
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert!((spans[0].start_beat).abs() < 1e-9);
        // 単一 slice は native rate (窓 24000 = 1 拍ぶん) で 1 拍鳴って残りは無音。
        assert!((spans[0].end_beat - 1.0).abs() < 1e-9, "{spans:?}");
    }

    // ---- reversed / warp ----

    #[test]
    fn wave_spans_mirror_source_range_when_reversed() {
        // 逆再生は窓全体を反転して読む (`source_frame_lerp`)。 slice 単位ではない。
        let mut ev = slice_event(48_000, 2.0, 0.0, vec![0, 12_000, 24_000, 36_000]);
        ev.reversed = true;
        let spans = spans_of(&ev);
        assert_eq!(spans.len(), 4, "{spans:?}");
        assert_monotonic(&spans, "reversed");
        // 出力の最初の slice は source の **末尾** 12000 frame を後ろから読む。
        assert_eq!((spans[0].source_start, spans[0].source_end), (36_000, 48_000));
        assert!(spans[0].reversed);
        assert_eq!((spans[3].source_start, spans[3].source_end), (0, 12_000));
    }

    #[test]
    fn wave_spans_reversed_offset_window_maps_into_window() {
        // trim 済 (source_start != 0) + reversed でも span は窓内に収まる。
        let ev = crate::model::AudioEvent {
            source_start_frames: 10_000,
            source_end_frames: 58_000,
            event_length_beats: 2.0,
            reversed: true,
            ..crate::model::AudioEvent::default()
        };
        let spans = spans_of(&ev);
        assert_eq!(spans.len(), 1);
        assert_eq!((spans[0].source_start, spans[0].source_end), (10_000, 58_000));
        assert!(spans[0].reversed);
    }

    #[test]
    fn wave_spans_follow_warp_markers_only_in_stretch_mode() {
        // 0..2 拍が source 前半 8000 frame、 2..4 拍が残り 40000 frame という非一様 warp。
        let markers = vec![bm(0, 0.0), bm(8_000, 2.0), bm(48_000, 4.0)];
        let mut ev = span_event(StretchMode::Stretch, 48_000, 4.0, 0.0);
        ev.beat_markers = markers.clone();
        let spans = spans_of(&ev);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_monotonic(&spans, "warp");
        assert_eq!((spans[0].source_start, spans[0].source_end), (0, 8_000));
        assert!((spans[0].end_beat - 2.0).abs() < 1e-9);
        assert_eq!((spans[1].source_start, spans[1].source_end), (8_000, 48_000));

        // 同じ marker が残ったまま Slice / Raw にすると、 再生は marker を無視する
        // ので描画も無視しなければならない (= 旧 audio_editor の markers 優先分岐バグ)。
        for mode in [StretchMode::Raw, StretchMode::Repitch, StretchMode::Slice] {
            ev.stretch_mode = mode;
            let spans = spans_of(&ev);
            assert_eq!(spans.len(), 1, "{mode:?} は warp 形状を描かない: {spans:?}");
        }
    }

    #[test]
    fn wave_spans_degenerate_inputs_are_safe() {
        // 0 窓 / 0 長 → span 無し。 SR 0 / bpm 0 → 窓全体を clip 幅いっぱいに 1 span。
        assert!(spans_of(&span_event(StretchMode::Raw, 0, 2.0, 0.0)).is_empty());
        assert!(spans_of(&span_event(StretchMode::Raw, 48_000, 0.0, 0.0)).is_empty());
        let mut out = Vec::new();
        for (sr, bpm) in [(0u32, 120.0f32), (48_000, 0.0)] {
            event_wave_spans(
                &span_event(StretchMode::Raw, 48_000, 2.0, 0.0),
                sr,
                &TempoMap::constant(bpm),
                0.0,
                &mut out,
            );
            assert_eq!(out.len(), 1, "sr={sr} bpm={bpm}");
            assert_eq!((out[0].source_start, out[0].source_end), (0, 48_000));
            assert!((out[0].end_beat - 2.0).abs() < 1e-9);
        }
        // NaN ピッチは pitch_factor が 1.0 に倒すので普通の 1 span。
        assert_eq!(spans_of(&span_event(StretchMode::Raw, 48_000, 2.0, f32::NAN)).len(), 1);
    }

    // ---- TempoMap (SongTempo automation 下での native rate) ----

    /// `beat 0..len` を `start_bpm → end_bpm` の直線で結ぶ SongTempo lane を持つ song。
    fn song_with_tempo_ramp(start_bpm: f64, end_bpm: f64, len_beats: f64) -> crate::model::Song {
        use crate::model::{
            AutomationClip, AutomationContent, AutomationCurve, AutomationLane, AutomationPoint,
            AutomationTarget, ClipContent, Song,
        };
        let mut song = Song { bpm: 120.0, ..Song::default() };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                next_point_id: 0,
                points: vec![
                    AutomationPoint { id: 0, time_beat: 0.0, value: start_bpm, curve: AutomationCurve::Linear },
                    AutomationPoint { id: 0, time_beat: len_beats, value: end_bpm, curve: AutomationCurve::Linear },
                ],
            }),
        );
        let lane_id = song.alloc_song_lane_id();
        let mut lane = AutomationLane::new(AutomationTarget::SongTempo, 120.0);
        lane.id = lane_id;
        lane.clips.push(AutomationClip {
            id: 1,
            name: "Tempo".into(),
            start_beat: 0.0,
            length_beats: len_beats,
            content_id: cid,
        });
        lane.next_clip_id = 2;
        song.song_lanes.push(lane);
        song
    }

    #[test]
    fn tempo_map_uses_curve_only_when_song_tempo_lane_exists() {
        let plain = crate::model::Song { bpm: 100.0, ..crate::model::Song::default() };
        let t = TempoMap::from_song(&plain);
        assert!(t.is_constant());
        assert!((t.bpm_at(3.0) - 100.0).abs() < 1e-9);

        let song = song_with_tempo_ramp(60.0, 60.0, 8.0);
        let t = TempoMap::from_song(&song);
        assert!(!t.is_constant(), "SongTempo lane があれば曲線評価");
        assert!((t.bpm_at(1.0) - 60.0).abs() < 1e-6, "got {}", t.bpm_at(1.0));
        // nominal は compile 基準 (song.bpm) のまま = 配置計算に使う。
        assert!((t.nominal_bpm() - 120.0).abs() < 1e-9);

        // lane を disable すると定数へ戻る。
        let mut song = song;
        song.song_lanes[0].enabled = false;
        assert!(TempoMap::from_song(&song).is_constant());
    }

    #[test]
    fn raw_span_follows_tempo_curve_for_native_rate() {
        // song.bpm = 120 のまま SongTempo lane で全域 60 BPM にする。 Raw は
        // 実時間で source を消費するので、 1 拍あたりの消費量が 2 倍になり
        // 「窓を鳴らし切る拍数」 が半分になる (= 波形が clip の左半分で終わる)。
        let song = song_with_tempo_ramp(60.0, 60.0, 8.0);
        let tempo = TempoMap::from_song(&song);
        let ev = span_event(StretchMode::Raw, 48_000, 4.0, 0.0);
        let mut out = Vec::new();
        event_wave_spans(&ev, 48_000, &tempo, 0.0, &mut out);
        assert!(!out.is_empty());
        assert_monotonic(&out, "raw tempo curve");
        let end = out.last().unwrap().end_beat;
        assert!((end - 1.0).abs() < 0.05, "60 BPM では 1 拍で鳴り終わる: got {end}");
        // 定数 120 BPM なら 2 拍ぶん鳴る (= 従来値)。
        let mut base = Vec::new();
        event_wave_spans(&ev, 48_000, &TempoMap::constant(120.0), 0.0, &mut base);
        assert_eq!(base.len(), 1);
        assert!((base[0].end_beat - 2.0).abs() < 1e-9);
        // 分割された span は 2 本目以降 head = false (スライス境界線を出さない)。
        assert!(out[0].head);
        assert!(out[1..].iter().all(|s| !s.head), "分割 span は head でない");
    }

    #[test]
    fn slice_body_follows_tempo_curve_but_triggers_do_not() {
        // 全域 60 BPM。 trigger 配置 (= 窓 / clip 長) は tempo 不変、 slice 本体だけが
        // 2 倍速で鳴り終わる → gap が広がる。
        let song = song_with_tempo_ramp(60.0, 60.0, 8.0);
        let tempo = TempoMap::from_song(&song);
        let ev = slice_event(48_000, 4.0, 0.0, vec![0, 12_000, 24_000, 36_000]);
        let mut out = Vec::new();
        event_wave_spans(&ev, 48_000, &tempo, 0.0, &mut out);
        assert_monotonic(&out, "slice tempo curve");
        let heads: Vec<f64> = out.iter().filter(|s| s.head).map(|s| s.start_beat).collect();
        assert_eq!(heads.len(), 4, "trigger は 4 本のまま: {out:?}");
        for (i, b) in heads.iter().enumerate() {
            assert!((b - i as f64).abs() < 1e-9, "trigger 拍は tempo 不変: {heads:?}");
        }
        // slice 0 が鳴り終わる拍 = 次の head の直前 span の end。
        let first_end = out
            .iter()
            .take_while(|s| s.start_beat < 1.0 - 1e-9)
            .last()
            .unwrap()
            .end_beat;
        assert!(
            (first_end - 0.25).abs() < 0.05,
            "60 BPM では slice 本体が 0.25 拍で終わる (120 BPM なら 0.5): got {first_end}"
        );
    }

    #[test]
    fn source_frame_at_beat_inverts_spans_and_skips_gaps() {
        // 伸ばした Slice (1 拍ごとに trigger、 0.5 拍鳴って 0.5 拍 gap)。
        let spans = spans_of(&slice_event(48_000, 4.0, 0.0, vec![0, 12_000, 24_000, 36_000]));
        // slice 1 の中央 (1.25 拍) は source 12000 + 0.25拍×24000 = 18000。
        let got = source_frame_at_beat(&spans, 1.25).expect("span 内");
        assert!((got - 18_000.0).abs() < 1.0, "got {got}");
        // gap (1.75 拍) は何も鳴っていない。
        assert!(source_frame_at_beat(&spans, 1.75).is_none());
        // event 末尾の余り (3.9 拍) も無音。
        assert!(source_frame_at_beat(&spans, 3.9).is_none());

        // reversed は span 内で右→左に読む (左端 = source_end)。
        let mut ev = slice_event(48_000, 2.0, 0.0, vec![0]);
        ev.reversed = true;
        let spans = spans_of(&ev);
        let got = source_frame_at_beat(&spans, 0.5).expect("span 内");
        assert!((got - 36_000.0).abs() < 1.0, "窓末尾から 1/4 読んだ位置: got {got}");
    }

    #[test]
    fn wave_spans_clear_out_param_before_filling() {
        let mut out = vec![WaveSpan {
            start_beat: 9.0,
            end_beat: 10.0,
            source_start: 1,
            source_end: 2,
            reversed: false,
            head: true,
        }];
        event_wave_spans(
            &span_event(StretchMode::Raw, 48_000, 2.0, 0.0),
            48_000,
            &TempoMap::constant(120.0),
            0.0,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert!((out[0].start_beat).abs() < 1e-9, "前回の内容が残っている");
    }

    #[test]
    fn warp_source_frame_piecewise_linear_with_extrapolation() {
        // <2 markers → None (uniform fallback)。
        assert_eq!(warp_source_frame(1.0, &[]), None);
        assert_eq!(warp_source_frame(1.0, &[bm(0, 0.0)]), None);

        // 非一様: 0..2 beat が steep (0→88200)、 2..4 beat が shallow (88200→132300)。
        let markers = [bm(0, 0.0), bm(88_200, 2.0), bm(132_300, 4.0)];
        let approx = |got: Option<f64>, want: f64| {
            let g = got.expect("Some");
            assert!((g - want).abs() < 1e-6, "got {g} want {want}");
        };
        approx(warp_source_frame(0.0, &markers), 0.0); // 先頭 marker
        approx(warp_source_frame(1.0, &markers), 44_100.0); // 第1セグメント中点
        approx(warp_source_frame(2.0, &markers), 88_200.0); // 中間 marker
        approx(warp_source_frame(3.0, &markers), 110_250.0); // 第2セグメント中点
        approx(warp_source_frame(4.0, &markers), 132_300.0); // 末尾 marker
        // 範囲外は端セグメント傾きで外挿 (先頭 44100/beat、 末尾 22050/beat)。
        approx(warp_source_frame(-1.0, &markers), -44_100.0);
        approx(warp_source_frame(5.0, &markers), 154_350.0);
    }

    #[test]
    fn warp_source_frame_degenerate_segment_is_none() {
        // 同 locked_beat の 2 marker (退化) を補間に使うと None → uniform fallback。
        let markers = [bm(0, 1.0), bm(100, 1.0)];
        assert_eq!(warp_source_frame(1.0, &markers), None);
    }

    #[test]
    fn move_warp_marker_clamps_between_neighbors_and_keeps_strict_order() {
        let mut m = vec![bm(0, 0.0), bm(1000, 1.0), bm(2000, 2.0)];
        // 中間 marker を 1.5 へ (隣接 0.0/2.0 内) → 許容、 source_frame 据え置き。
        move_warp_marker(&mut m, 1, 1.5);
        assert!((m[1].locked_beat - 1.5).abs() < 1e-9);
        assert_eq!(m[1].source_frame, 1000);
        // 末尾を 5.0 へ (next 無し = 自由)。
        move_warp_marker(&mut m, 2, 5.0);
        assert!((m[2].locked_beat - 5.0).abs() < 1e-9);
        // 中間を末尾超え 9.0 へ → next(5.0) 手前に clamp。
        move_warp_marker(&mut m, 1, 9.0);
        assert!(m[1].locked_beat < m[2].locked_beat);
        // 先頭を -3.0 へ (prev 無し = 自由)。
        move_warp_marker(&mut m, 0, -3.0);
        assert!((m[0].locked_beat - (-3.0)).abs() < 1e-9);
        // warp_source_frame の前提 = locked_beat 厳密増加を維持。
        for w in m.windows(2) {
            assert!(w[0].locked_beat < w[1].locked_beat, "locked_beat 厳密増加");
        }
    }

    #[test]
    fn add_warp_marker_inserts_sorted_and_skips_degenerate() {
        let mut m = vec![bm(0, 0.0), bm(2000, 2.0)];
        assert_eq!(add_warp_marker(&mut m, 1000, 1.0), Some(1));
        assert_eq!(m.len(), 3);
        assert_eq!(m[1].source_frame, 1000);
        // 既存と同 locked_beat (退化) は skip → warp_source_frame None 回避。
        assert_eq!(add_warp_marker(&mut m, 1500, 1.0), None);
        assert_eq!(m.len(), 3);
        for w in m.windows(2) {
            assert!(w[0].locked_beat < w[1].locked_beat);
        }
    }

    #[test]
    fn delete_warp_marker_removes_and_degrades_to_uniform() {
        let mut m = vec![bm(0, 0.0), bm(1000, 1.0), bm(2000, 2.0)];
        delete_warp_marker(&mut m, 1);
        assert_eq!(m.len(), 2);
        delete_warp_marker(&mut m, 5); // 範囲外 no-op。
        assert_eq!(m.len(), 2);
        delete_warp_marker(&mut m, 0);
        // 1 件 → warp_source_frame は None (uniform fallback = warp 解除)。
        assert_eq!(warp_source_frame(0.5, &m), None);
    }

    #[test]
    fn warp_markers_from_onsets_snaps_to_grid_monotonic() {
        // source_len=48000, length_beats=4, grid=4 (16th)。 onset 12000 → beat 1.0、
        // 18500 → ≈1.54 → snap 1.5、 30000 → beat 2.5。
        let onsets = [0u64, 12_000, 18_500, 30_000];
        let m = warp_markers_from_onsets(&onsets, 0, 48_000, 4.0, 4);
        let beats: Vec<f64> = m.iter().map(|x| x.locked_beat).collect();
        assert_eq!(m.first().unwrap().locked_beat, 0.0);
        assert_eq!(m.last().unwrap().locked_beat, 4.0);
        assert_eq!(m.last().unwrap().source_frame, 48_000);
        // 厳密増加 (warp の monotonic 前提)。
        assert!(beats.windows(2).all(|w| w[1] > w[0]), "monotonic: {beats:?}");
        // 量子化先 1.0/1.5/2.5、 source_frame は onset 実値を保持。
        assert!(m.iter().any(|x| (x.locked_beat - 1.0).abs() < 1e-9 && x.source_frame == 12_000));
        assert!(m.iter().any(|x| (x.locked_beat - 1.5).abs() < 1e-9 && x.source_frame == 18_500));
        assert!(m.iter().any(|x| (x.locked_beat - 2.5).abs() < 1e-9 && x.source_frame == 30_000));
    }

    #[test]
    fn warp_markers_from_onsets_degenerate_is_anchors_only() {
        // transient 無し (onset[0]=0 のみ) → anchor 2 件 (caller は uniform とみなす)。
        let m = warp_markers_from_onsets(&[0], 100, 48_000, 4.0, 4);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].source_frame, 100);
        assert_eq!(m[1].source_frame, 48_100);
        // 退化入力 (source_len 0) も anchor 2 件。
        assert_eq!(warp_markers_from_onsets(&[1, 2], 0, 0, 4.0, 4).len(), 2);
    }

    // ---- fade_envelope ----

    #[test]
    fn fade_envelope_zero_len_passes_through() {
        assert_eq!(fade_envelope(0, 0, FadeCurve::Linear), 1.0);
        assert_eq!(fade_envelope(100, 0, FadeCurve::Exponential), 1.0);
    }

    #[test]
    fn fade_envelope_t_at_or_past_len_is_unity() {
        assert_eq!(fade_envelope(100, 100, FadeCurve::Linear), 1.0);
        assert_eq!(fade_envelope(101, 100, FadeCurve::SCurve), 1.0);
    }

    #[test]
    fn fade_envelope_linear_midpoint_is_half() {
        let v = fade_envelope(50, 100, FadeCurve::Linear);
        assert!((v - 0.5).abs() < 1e-6, "got {v}");
    }

    #[test]
    fn fade_envelope_exp_is_squared_linear() {
        let lin = fade_envelope(50, 100, FadeCurve::Linear);
        let exp = fade_envelope(50, 100, FadeCurve::Exponential);
        assert!((exp - lin * lin).abs() < 1e-6);
    }

    #[test]
    fn fade_envelope_scurve_midpoint_is_half() {
        // 0.5 - 0.5 * cos(π/2) = 0.5
        let v = fade_envelope(50, 100, FadeCurve::SCurve);
        assert!((v - 0.5).abs() < 1e-6, "got {v}");
    }

    #[test]
    fn fade_envelope_scurve_endpoints_zero_and_one() {
        // SCurve at t=0 → 0、 t=1 (full) → 1.0 (early-out 経由)。
        assert!(fade_envelope(0, 100, FadeCurve::SCurve).abs() < 1e-6);
        assert!((fade_envelope(99, 100, FadeCurve::SCurve) - 1.0).abs() < 0.01);
    }

    // ---- sample_rate_ratio / pitch_factor ----

    #[test]
    fn sample_rate_ratio_is_source_over_engine() {
        let r = sample_rate_ratio(44_100, 48_000);
        assert!((r - 44100.0 / 48000.0).abs() < 1e-9);
    }

    #[test]
    fn pitch_factor_octave_up_and_down() {
        assert!((pitch_factor(12.0) - 2.0).abs() < 1e-6);
        assert!((pitch_factor(-12.0) - 0.5).abs() < 1e-6);
        assert!((pitch_factor(0.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn sr_and_pitch_compose_for_tape_modes() {
        // 24kHz source @ engine 48kHz + 12 semitones → 0.5 * 2 = 1.0 (Raw/Repitch の stride)。
        let r = sample_rate_ratio(24_000, 48_000) * pitch_factor(12.0);
        assert!((r - 1.0).abs() < 1e-6);
    }

    #[test]
    fn engine_sr_zero_and_non_finite_pitch_are_safe() {
        // Defensive: engine_sr=0 で divide-by-zero しない、 NaN semitone は 1.0。
        assert!((sample_rate_ratio(48_000, 0) - 1.0).abs() < 1e-9);
        assert!((pitch_factor(f32::NAN) - 1.0).abs() < 1e-9);
        assert!(pitch_factor(f32::MAX).is_finite());
    }

    // ---- stretch_ratio_for ----

    #[test]
    fn stretch_ratio_native_equals_event_is_unity() {
        // 1 秒の source (48k) を 120bpm で 2 拍 (= 1 秒) に置く → trim 相当、 比 1.0。
        let r = stretch_ratio_for(48_000, 48_000, 2.0, 120.0);
        assert!((r - 1.0).abs() < 1e-9, "got {r}");
    }

    #[test]
    fn stretch_ratio_longer_event_slows_source() {
        // 1 秒の source を 2 秒分 (= 4 拍 @120bpm) の slot に伸ばす → source は
        // 半速で進む → 比 0.5。
        let r = stretch_ratio_for(48_000, 48_000, 4.0, 120.0);
        assert!((r - 0.5).abs() < 1e-9, "got {r}");
    }

    #[test]
    fn stretch_ratio_shorter_event_speeds_source() {
        // 1 秒の source を 0.5 秒分 (= 1 拍 @120bpm) の slot に詰める → 倍速 → 比 2.0。
        let r = stretch_ratio_for(48_000, 48_000, 1.0, 120.0);
        assert!((r - 2.0).abs() < 1e-9, "got {r}");
    }

    #[test]
    fn stretch_ratio_independent_of_engine_sr() {
        // source SR が違っても秒で比較するので比は同じ (24k source、 1 秒 = 24000
        // frames を 1 秒 slot へ)。
        let r = stretch_ratio_for(24_000, 24_000, 2.0, 120.0);
        assert!((r - 1.0).abs() < 1e-9, "got {r}");
    }

    #[test]
    fn stretch_ratio_degenerate_is_unity() {
        assert!((stretch_ratio_for(0, 48_000, 2.0, 120.0) - 1.0).abs() < 1e-9);
        assert!((stretch_ratio_for(48_000, 0, 2.0, 120.0) - 1.0).abs() < 1e-9);
        assert!((stretch_ratio_for(48_000, 48_000, 0.0, 120.0) - 1.0).abs() < 1e-9);
        assert!((stretch_ratio_for(48_000, 48_000, 2.0, 0.0) - 1.0).abs() < 1e-9);
    }

    // ---- tempo_follow_ratio ----

    #[test]
    fn tempo_follow_ratio_cases() {
        // (stretch_ratio, current_bpm, nominal_bpm, expected)
        let cases = [
            (1.0, 120.0, 120.0, 1.0),  // current == nominal → 追従なし、 native rate
            (1.0, 240.0, 120.0, 2.0),  // 倍テンポ → source 倍速で進む (= 同じ拍に収める)
            (1.0, 60.0, 120.0, 0.5),   // 半テンポ → source 半速
            (0.5, 240.0, 120.0, 1.0),  // 手動 stretch 0.5 × 追従 2.0 = 1.0 (乗算合成)
            (2.0, 90.0, 180.0, 1.0),   // 手動 2.0 × 追従 0.5
            (1.0, 140.0, 0.0, 1.0),    // nominal=0 は退化 → stretch_ratio 素通し
        ];
        for (stretch, current, nominal, expected) in cases {
            let got = tempo_follow_ratio(stretch, current, nominal);
            assert!(
                (got - expected).abs() < 1e-9,
                "stretch={stretch} current={current} nominal={nominal} got={got} want={expected}"
            );
        }
    }

    #[test]
    fn tempo_follow_spans_fixed_beats_across_tempo() {
        // MIDI 流の不変条件: import 時に event_length_beats = native長(拍) で置いた
        // clip は、 どの current_bpm でも source 全体がちょうど beat window に収まる
        // (= advance_ratio × window_secs == native_secs)。 これが「拍数を固定して
        // tempo に追従する」 の数学的定義。 nominal の取り方 (import時 vs 再コンパイル時)
        // に依らず成立することも検証する。
        let native_frames = 96_000u64; // 2.0 s @ 48k
        let sr = 48_000u32;
        let native_secs = native_frames as f64 / f64::from(sr);
        // (nominal_bpm = clip 取り込み時テンポ, current_bpm = 再生時テンポ)
        let cases = [
            (120.0f32, 120.0f64),
            (120.0, 140.0),
            (120.0, 90.0),
            (90.0, 174.0),
            (174.0, 100.0),
        ];
        for (nominal_bpm, current_bpm) in cases {
            // import path (app.rs frames_to_beats) と同じ: 配置拍 = native秒 × bpm/60。
            let event_length_beats = native_secs * f64::from(nominal_bpm) / 60.0;
            let manual = stretch_ratio_for(native_frames, sr, event_length_beats, nominal_bpm);
            // 取り込み直後は手動 stretch なし → 比 1.0。
            assert!((manual - 1.0).abs() < 1e-9, "manual={manual}");
            let advance = tempo_follow_ratio(manual, current_bpm, f64::from(nominal_bpm));
            let window_secs = event_length_beats * 60.0 / current_bpm;
            assert!(
                (advance * window_secs - native_secs).abs() < 1e-6,
                "nominal={nominal_bpm} current={current_bpm} advance={advance} \
                 window_secs={window_secs} native_secs={native_secs}"
            );
        }
    }
}
