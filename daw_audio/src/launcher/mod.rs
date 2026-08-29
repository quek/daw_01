//! r.md #87 クリップランチャーの再生エンジン (`docs/plan_rmd_87_clip_launcher.md` §2)。
//!
//! ランチャーは **もう 1 本のタイムラインではなく、行ごとに時間軸の供給元を
//! 切り替える機構**。「行」は arrangement の 1 行と 1:1 で、通常トラック行
//! ([`common::model::Track`]) と展開したオートメーションレーン行
//! ([`common::model::AutomationLane`]) の両方が対象 (Q4)。
//!
//! 行の実効拍を毎 buffer こう解く:
//!
//! ```text
//! Arranger       : effective = playhead_beats、イベント源 = track.clips
//! Launcher(cell) : effective = cell.start_beat + ((playhead_beats - launch_beat) mod loop_len)
//!                  イベント源 = その 1 セル
//! LauncherStopped: 無音 (オートメーションはレーン既定値)
//! ```
//!
//! 計画書は実効拍を `launch_beat + ((playhead - launch_beat) mod loop_len)` と書くが、
//! これは「セルが `launch_beat` に置かれている」座標での表現。セルの
//! `clip.start_beat` は正規化で **常に 0** なので、実装はセル自身の座標
//! (`cell.start_beat` 起点) で解く。両者は平行移動で一致し、こちらなら
//! `collect_events_for_buffer` / `render_audio_events` / automation の
//! **既存の clip 窓の算術がそのまま通る** (窓を別式で書き直さない)。
//!
//! # なぜ別モジュールなのか
//!
//! `graph/execute.rs` は 979 / 1,000 実コード行、`sequencer.rs` /
//! `audio_clip_renderer.rs` / `engine.rs` の該当関数は
//! `scripts/arch_lint_baseline.txt` の FN-NESTING 天井ちょうどに載っている
//! (天井は登録時点の実測値で余裕なし)。不変条件 9 の「超過したら分割してから足す」
//! に従い、行ごとの分岐は **全部ここに置く**。
//!
//! # RT 規約
//!
//! 走行状態も [`RowSourceTable`] もすべて事前確保。audio thread では確保・ロック・
//! I/O・`format!` を行わない (`make test-rt` の `assert_no_alloc` が検査)。
//! 乱数はグローバル状態を持たず `f(seed, 発火拍)` の純ハッシュ
//! ([`common::modulators::random_unit`]) — 書き出しを 2 回やれば同じ結果になる (Q9)。

pub mod follow;
pub mod ipc;
pub mod quantize;
pub mod render;
pub mod runtime;

pub use runtime::{LaunchRequest, LauncherRuntime};

/// 走行状態を持てる行数の上限。`common::audio_bridge::MAX_LAUNCHER_ROWS` と
/// 揃える (publish できない行を engine だけが持っても表示できない)。
pub const MAX_ROWS: usize = common::audio_bridge::MAX_LAUNCHER_ROWS;

/// 拍数 / 長さとして使える値か (有限かつ正)。
///
/// `!(x > 0.0)` と書くと NaN も弾けるが clippy の `neg_cmp_op_on_partial_ord` に
/// 発火拍 / ループ端の丸め誤差を吸収する幅 (拍)。frame → 拍 の往復で
/// 数 ULP ずれるのと、`switch_frame` が floor である (= 発火 frame の拍は
/// 発火拍のわずか手前) の 2 つを吸収する。48 kHz / 120 BPM の 1 frame が
/// 約 8.3e-5 拍なので、その 1/8 を採る。**GUI 側 (`launcher_time`) の
/// 同名定数と意味を揃えること。**
pub const LAUNCH_EPSILON_BEATS: f64 = 1e-5;

/// 当たる。判定を 1 か所に閉じて、呼び側は `!is_positive(x)` と書く。
#[must_use]
#[inline]
pub fn is_positive(x: f64) -> bool {
    x.is_finite() && x > 0.0
}

/// 行の安定 id。**`lane_id == 0` がトラック行**、それ以外はそのトラックの
/// オートメーションレーン行 ([`common::model::AutomationLane`] の `id` は 1 から採番)。
///
/// タプルではなく struct なのは意図的 — `HashMap<(u32, u32), _>` は
/// `scripts/arch_lint.sh` の POSITIONAL-KEY 検査に当たる形で、そもそも
/// 行の集合は事前確保した `Vec` を線形走査する (§2.4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RowKey {
    pub track_id: u32,
    pub lane_id: u32,
}

impl RowKey {
    #[must_use]
    pub fn track(track_id: u32) -> Self {
        Self { track_id, lane_id: 0 }
    }

    #[must_use]
    pub fn lane(track_id: u32, lane_id: u32) -> Self {
        Self { track_id, lane_id }
    }

    /// shmem へ publish するための 1 ワード表現 (`common::audio_bridge`)。
    /// `0` = 空きスロット (`track_id` は 1 から採番されるので実在の行と衝突しない)。
    #[must_use]
    pub fn packed(self) -> u64 {
        (u64::from(self.track_id) << 32) | u64::from(self.lane_id)
    }
}

/// 行の時間軸の供給元。`Copy` で、確保を伴わない。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum RowPhase {
    /// アレンジのタイムラインが鳴らす (既定)。実効拍 = `playhead_beats`。
    #[default]
    Arranger,
    /// ランチャーのセルが鳴っている。
    Cell {
        /// セルの `clip.id` (行の中で一意)。
        clip_id: u32,
        /// このセルを撃った song 拍 = 位相の原点。
        launch_beat: f64,
        /// セルの長さ (拍)。`<= 0` は退化 (無音に倒す)。
        loop_len: f64,
        /// セル自身の song 拍原点 (`clip.start_beat`)。正規化済みなら 0。
        cell_start_beat: f64,
        /// `false` = ワンショット (終端で止まる)。
        looping: bool,
    },
    /// ランチャーが握っているが無音 (Stop Clips / 空セルのシーンを撃った)。
    Silent,
}

impl RowPhase {
    /// 鳴っているセルの `clip.id` (`Arranger` / `Silent` は `None`)。
    #[must_use]
    pub fn cell_clip_id(self) -> Option<u32> {
        match self {
            Self::Cell { clip_id, .. } => Some(clip_id),
            _ => None,
        }
    }

    /// song 拍 `beat` におけるこの行の実効拍。`None` = この瞬間は無音。
    ///
    /// ワンショット (`looping == false`) は終端を越えたら `None` — セル窓の外なので
    /// そのまま渡しても無音にはなるが、行の停止判定と揃えるためここでも切る。
    #[must_use]
    pub fn effective_beat(self, beat: f64) -> Option<f64> {
        match self {
            Self::Arranger => Some(beat),
            Self::Silent => None,
            Self::Cell { launch_beat, loop_len, cell_start_beat, looping, .. } => {
                // 発火拍ちょうどの frame は、frame → 拍 の丸めで `d` が
                // **ごくわずかに負**になることがある (`switch_frame` は floor)。
                // そこで `None` を返すと、その buffer の残り全部が無音として
                // 捨てられる (`emit_phase` は最初の `None` で打ち切る) ので、
                // 1 frame ぶんの下振れは「撃った瞬間」として吸収する。
                let d = (beat - launch_beat).max(0.0);
                if !d.is_finite() || beat - launch_beat < -LAUNCH_EPSILON_BEATS
                    || !is_positive(loop_len)
                {
                    return None;
                }
                if looping {
                    Some(cell_start_beat + d.rem_euclid(loop_len))
                } else if d < loop_len {
                    Some(cell_start_beat + d)
                } else {
                    None
                }
            }
        }
    }
}

/// 1 buffer 分の、1 行の供給元。**供給元の切り替えは 1 buffer につき高々 1 回**
/// (量子化境界での発火 / フォローアクション / ワンショットの終端)。
///
/// より細かい遷移が同じ buffer に 2 つ以上落ちる場合 (= セルが 1 buffer より短い
/// 病的なケース) は、残りを次の buffer で解く。発火拍は毎 buffer 引き直すので
/// 取りこぼしにはならず、鎖の進みが最大 1 buffer (~5..20 ms) 粗くなるだけ。
/// **セル内のループの巻き戻しは回数に制限を設けない** ([`for_each_segment`] が
/// frame 単位で分割する)。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RowTimeSource {
    pub key: RowKey,
    /// `0..switch_frame` の供給元。
    pub head: RowPhase,
    /// `switch_frame..frames` の供給元。
    pub tail: RowPhase,
    /// 供給元が切り替わる buffer 内 frame。`0` = 最初から `tail`。
    pub switch_frame: u32,
}

impl RowTimeSource {
    /// 切り替えの無い 1 供給元の行。
    #[must_use]
    pub fn uniform(key: RowKey, phase: RowPhase) -> Self {
        Self { key, head: phase, tail: phase, switch_frame: 0 }
    }
}

/// 描画する 1 区間。`[start_frame, end_frame)` を実効拍 `beat` から描く。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RowSegment {
    pub start_frame: u32,
    pub end_frame: u32,
    /// 区間先頭の実効拍。
    pub beat: f64,
    /// イベント源。`0` = アレンジのクリップ列、それ以外 = そのセル 1 つ。
    pub cell_clip_id: u32,
}

impl RowSegment {
    #[must_use]
    pub fn frames(self) -> u32 {
        self.end_frame.saturating_sub(self.start_frame)
    }
}

/// 1 buffer 分を「同じ実効拍で連続して描ける区間」に割って `f` に渡す。
///
/// 割れる契機は 2 つだけ:
/// - 供給元の切り替え ([`RowTimeSource::switch_frame`])
/// - **セルのループ端** — 跨いだら次の区間は実効拍が `cell_start_beat` へ戻る
///
/// 無音の区間は `f` を呼ばない (呼び側は「呼ばれなかった frame = 無音」)。
///
/// RT 安全 / 有界: 各区間は必ず 1 frame 以上進むので、反復は高々 `frames` 回。
/// 確保もロックも無い。
pub fn for_each_segment(
    src: RowTimeSource,
    playhead_beats: f64,
    beats_per_frame: f64,
    frames: u32,
    mut f: impl FnMut(RowSegment),
) {
    if frames == 0 || !is_positive(beats_per_frame) {
        return;
    }
    let switch = src.switch_frame.min(frames);
    emit_phase(src.head, playhead_beats, beats_per_frame, 0, switch, &mut f);
    emit_phase(src.tail, playhead_beats, beats_per_frame, switch, frames, &mut f);
}

/// [`for_each_segment`] の 1 供給元ぶん。
fn emit_phase(
    phase: RowPhase,
    playhead_beats: f64,
    beats_per_frame: f64,
    from: u32,
    to: u32,
    f: &mut impl FnMut(RowSegment),
) {
    if from >= to {
        return;
    }
    let cell_clip_id = phase.cell_clip_id().unwrap_or(0);
    let mut cursor = from;
    while cursor < to {
        let beat_at_cursor = playhead_beats + f64::from(cursor) * beats_per_frame;
        let Some(eff) = phase.effective_beat(beat_at_cursor) else {
            // 無音 (停止 / ワンショットの終端を越えた)。以降も無音なので抜ける。
            return;
        };
        // ループの巻き戻し直後 / 撃った直後の区間は、frame 境界の切り上げのぶん
        // 実効拍が原点をわずかに **越えて** 始まる。そのままだと
        // 「content 拍 0 ちょうどの note」が毎周スキップされる (キックが 2 周目
        // 以降消える) ので、1 frame 未満のはみ出しは原点へ吸着させる。
        let eff = snap_to_cell_origin(phase, eff, beats_per_frame);
        let end = next_break(phase, eff, beats_per_frame, cursor, to);
        f(RowSegment { start_frame: cursor, end_frame: end, beat: eff, cell_clip_id });
        cursor = end.max(cursor.saturating_add(1));
    }
}

/// ループ原点をわずかに越えて始まった区間の実効拍を、原点ちょうどへ吸着する。
/// はみ出しが 1 frame 未満のときだけ (= 本当に境界の直後のときだけ) 効く。
#[must_use]
fn snap_to_cell_origin(phase: RowPhase, eff: f64, beats_per_frame: f64) -> f64 {
    let RowPhase::Cell { cell_start_beat, .. } = phase else {
        return eff;
    };
    let over = eff - cell_start_beat;
    if over > 0.0 && over < beats_per_frame { cell_start_beat } else { eff }
}

/// この区間がどこで切れるか (ループ端 or 供給元の終わり)。
fn next_break(phase: RowPhase, eff: f64, beats_per_frame: f64, cursor: u32, to: u32) -> u32 {
    let RowPhase::Cell { loop_len, cell_start_beat, looping: true, .. } = phase else {
        return to;
    };
    // ループ端までの残り拍 → frame。`eff` は `[cell_start, cell_start + loop_len)`
    // にあるので残りは必ず正 = 反復は必ず 1 frame 以上進む。
    let remain_beats = (cell_start_beat + loop_len - eff).max(0.0);
    let remain_frames = (remain_beats / beats_per_frame).ceil();
    if !remain_frames.is_finite() {
        return to;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let remain_frames = (remain_frames as u32).max(1);
    cursor.saturating_add(remain_frames).min(to)
}

/// 行の供給元テーブル。engine が dispatch 直前に埋め、worker が読む。
///
/// 添字は **その buffer 限りの並び** (`song.tracks` の順 → 各トラックの
/// `automation_lanes` の順)。プロセス境界も永続参照も跨がないので positional で
/// よい (`Schedule::input_delay_per_track` と同じ扱い)。GUI へ渡る側
/// (`common::audio_bridge`) は安定 id を使う。
#[derive(Debug, Default)]
pub struct RowSourceTable {
    sources: Vec<RowTimeSource>,
    /// `offsets[track_idx]` = そのトラックの行の先頭 (= トラック行)。
    offsets: Vec<u32>,
    /// マスター行 (`Song.song_lanes`) のグループ index。
    /// マスターはトラックではないので `offsets` の末尾に別枠で積む。
    master_group: Option<usize>,
}

impl RowSourceTable {
    /// 事前確保。容量を超えた行は `Arranger` に倒れる (= 従来の挙動)。
    #[must_use]
    pub fn new() -> Self {
        Self {
            sources: Vec::with_capacity(MAX_ROWS),
            // +2 = マスター行のグループと番兵。
            offsets: Vec::with_capacity(crate::engine::MAX_TRACKS + 2),
            master_group: None,
        }
    }

    pub fn clear(&mut self) {
        self.sources.clear();
        self.offsets.clear();
        self.master_group = None;
    }

    /// マスター行群 (`Song.song_lanes`) の開始。以降の `push` はマスターの行。
    pub fn begin_master(&mut self) {
        self.master_group = Some(self.offsets.len());
        self.begin_track();
    }

    /// マスターのレーン行。マスター行を積んでいなければ空 (= すべて `Arranger`)。
    #[must_use]
    pub fn master_rows(&self) -> TrackRows<'_> {
        match self.master_group {
            Some(i) => self.track_rows(i),
            None => TrackRows::default(),
        }
    }

    /// 新しいトラックの行群を開始する (トラック行 → レーン行の順で `push`)。
    pub fn begin_track(&mut self) {
        #[allow(clippy::cast_possible_truncation)]
        let n = self.sources.len() as u32;
        if self.offsets.len() < self.offsets.capacity() {
            self.offsets.push(n);
        }
    }

    /// 行を 1 つ足す。容量を超えた分は捨てる (RT で再確保しない)。
    pub fn push(&mut self, src: RowTimeSource) {
        if self.sources.len() < self.sources.capacity() {
            self.sources.push(src);
        }
    }

    /// 1 トラック分の行 (トラック行 + そのレーン行)。範囲外は空 =
    /// すべて `Arranger` (= 従来の挙動)。
    #[must_use]
    pub fn track_rows(&self, track_idx: usize) -> TrackRows<'_> {
        let Some(&base) = self.offsets.get(track_idx) else {
            return TrackRows::default();
        };
        #[allow(clippy::cast_possible_truncation)]
        let end = self.offsets.get(track_idx + 1).copied().unwrap_or(self.sources.len() as u32);
        let (a, b) = (base as usize, (end as usize).min(self.sources.len()));
        if a >= b {
            return TrackRows::default();
        }
        TrackRows { rows: &self.sources[a..b] }
    }

    /// トラック行の供給元 (単発参照用)。
    #[cfg(test)]
    #[must_use]
    pub fn track_row(&self, track_idx: usize) -> RowTimeSource {
        self.track_rows(track_idx).track()
    }

    /// レーン行の供給元 (単発参照用)。
    #[cfg(test)]
    #[must_use]
    pub fn lane_row(&self, track_idx: usize, lane_idx: usize) -> RowTimeSource {
        self.track_rows(track_idx).lane(lane_idx)
    }
}

/// 1 トラック分の行のビュー (`Copy`)。**先頭がトラック行**、以降が
/// `automation_lanes` と同じ順のレーン行。
///
/// worker へ渡すのはこの形 — トラックごとに 1 つの `Copy` 値で済むので、
/// `process_track_owned` から automation の評価まで引数 1 本で通る。
#[derive(Debug, Clone, Copy, Default)]
pub struct TrackRows<'a> {
    rows: &'a [RowTimeSource],
}

impl TrackRows<'_> {
    /// トラック行。無ければ `Arranger`。
    #[must_use]
    pub fn track(self) -> RowTimeSource {
        self.rows.first().copied().unwrap_or_default()
    }

    /// `lane_idx` 番目のオートメーションレーン行。無ければ `Arranger`。
    #[must_use]
    pub fn lane(self, lane_idx: usize) -> RowTimeSource {
        self.rows.get(lane_idx + 1).copied().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(launch_beat: f64, loop_len: f64, looping: bool) -> RowPhase {
        RowPhase::Cell { clip_id: 7, launch_beat, loop_len, cell_start_beat: 0.0, looping }
    }

    /// 120 BPM / 48 kHz → 1 frame = 0.004 拍。
    const BPF: f64 = 0.004;

    fn segments(src: RowTimeSource, playhead: f64, frames: u32) -> Vec<RowSegment> {
        let mut out = Vec::new();
        for_each_segment(src, playhead, BPF, frames, |s| out.push(s));
        out
    }

    #[test]
    fn アレンジ行は分割されない() {
        let src = RowTimeSource::uniform(RowKey::track(1), RowPhase::Arranger);
        let segs = segments(src, 12.0, 512);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].start_frame, 0);
        assert_eq!(segs[0].end_frame, 512);
        assert_eq!(segs[0].beat, 12.0);
        assert_eq!(segs[0].cell_clip_id, 0);
    }

    #[test]
    fn ループ端を跨ぐ_buffer_は_2_つに割れる() {
        // 4 拍のセルを拍 0 で撃った。playhead 3.99 の buffer は拍 4.0 (= ループ端) を跨ぐ。
        let src = RowTimeSource::uniform(RowKey::track(1), cell(0.0, 4.0, true));
        let segs = segments(src, 3.99, 512);
        assert_eq!(segs.len(), 2, "{segs:?}");
        // 前半: 実効拍 3.99 から、残り 0.01 拍 = ceil(2.5) = 3 frame。
        assert_eq!(segs[0].start_frame, 0);
        assert_eq!(segs[0].end_frame, 3);
        assert!((segs[0].beat - 3.99).abs() < 1e-9);
        // 後半: セルの頭へ戻る。
        assert_eq!(segs[1].start_frame, 3);
        assert_eq!(segs[1].end_frame, 512);
        assert!(segs[1].beat >= 0.0 && segs[1].beat < 0.02, "{}", segs[1].beat);
        assert_eq!(segs[1].cell_clip_id, 7);
    }

    #[test]
    fn ループ端がちょうど_buffer_先頭なら分割は起きない() {
        // playhead 8.0、4 拍セルを拍 0 で撃った → 実効拍ちょうど 0.0。
        let src = RowTimeSource::uniform(RowKey::track(1), cell(0.0, 4.0, true));
        let segs = segments(src, 8.0, 512);
        assert_eq!(segs.len(), 1, "{segs:?}");
        assert_eq!(segs[0].beat, 0.0);
        assert_eq!(segs[0].end_frame, 512);
    }

    #[test]
    fn ループ端がちょうど_buffer_末尾なら分割は起きない() {
        // 512 frame = 2.048 拍。playhead 1.952 で拍 4.0 が buffer の直後に来る。
        let src = RowTimeSource::uniform(RowKey::track(1), cell(0.0, 4.0, true));
        let segs = segments(src, 4.0 - 512.0 * BPF, 512);
        assert_eq!(segs.len(), 1, "{segs:?}");
        assert_eq!(segs[0].end_frame, 512);
    }

    #[test]
    fn buffer_より短いセルは何度でも巻き戻る() {
        // 0.05 拍 (= 12.5 frame) のセル。512 frame の buffer に 40 回以上入る。
        let src = RowTimeSource::uniform(RowKey::track(1), cell(0.0, 0.05, true));
        let segs = segments(src, 0.0, 512);
        assert!(segs.len() > 30, "巻き戻しが打ち切られている: {}", segs.len());
        // 隙間なく連続し、buffer を覆い切る。
        assert_eq!(segs[0].start_frame, 0);
        assert_eq!(segs[segs.len() - 1].end_frame, 512);
        for w in segs.windows(2) {
            assert_eq!(w[0].end_frame, w[1].start_frame, "{segs:?}");
        }
    }

    #[test]
    fn ワンショットは終端を越えたら描かれない() {
        // 1 拍のセルを拍 0 で撃ち、playhead 1.5 → もう終端の外。
        let src = RowTimeSource::uniform(RowKey::track(1), cell(0.0, 1.0, false));
        assert!(segments(src, 1.5, 512).is_empty());
        // 終端の手前なら 1 区間 (巻き戻さない)。
        let segs = segments(src, 0.5, 512);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].end_frame, 512);
        assert!((segs[0].beat - 0.5).abs() < 1e-9);
    }

    #[test]
    fn 供給元の切り替えは_switch_frame_で割れる() {
        let src = RowTimeSource {
            key: RowKey::track(1),
            head: RowPhase::Arranger,
            tail: cell(1.0, 4.0, true),
            switch_frame: 200,
        };
        // playhead 0.8 → frame 200 は拍 1.6 = セル内 0.6 拍。
        let segs = segments(src, 0.8, 512);
        assert_eq!(segs.len(), 2, "{segs:?}");
        assert_eq!(segs[0].cell_clip_id, 0);
        assert_eq!(segs[0].end_frame, 200);
        assert_eq!(segs[1].start_frame, 200);
        assert_eq!(segs[1].cell_clip_id, 7);
        assert!((segs[1].beat - 0.6).abs() < 1e-9, "{}", segs[1].beat);
    }

    #[test]
    fn 停止した行は一切描かれない() {
        let src = RowTimeSource::uniform(RowKey::track(1), RowPhase::Silent);
        assert!(segments(src, 4.0, 512).is_empty());
    }

    #[test]
    fn テーブルは行が無いトラックを_arranger_に倒す() {
        let mut t = RowSourceTable::new();
        t.begin_track();
        t.push(RowTimeSource::uniform(RowKey::track(1), RowPhase::Silent));
        t.push(RowTimeSource::uniform(RowKey::lane(1, 3), RowPhase::Arranger));
        t.begin_track();
        t.push(RowTimeSource::uniform(RowKey::track(2), RowPhase::Arranger));

        assert_eq!(t.track_row(0).head, RowPhase::Silent);
        assert_eq!(t.lane_row(0, 0).key, RowKey::lane(1, 3));
        // track 0 はレーンを 1 本しか持たない → 2 本目は Arranger 既定。
        assert_eq!(t.lane_row(0, 1).head, RowPhase::Arranger);
        assert_eq!(t.lane_row(0, 1).key, RowKey::default());
        assert_eq!(t.track_row(1).key, RowKey::track(2));
        // 存在しないトラックも既定。
        assert_eq!(t.track_row(9).head, RowPhase::Arranger);
    }
}
