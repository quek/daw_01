//! r.md #87 クリップランチャー — **行ごとの実効拍とイベント源**の解決
//! (`docs/plan_rmd_87_clip_launcher.md` §2.1 / §3.6)。
//!
//! ランチャーは「もう 1 本のタイムライン」ではなく **行ごとに時間軸の供給元を
//! 切り替える機構**なので、「今この瞬間に何を映すか」を解く側は
//! *song の playhead + `track.clips`* を直に見てはいけない。行の主導権
//! ([`RowPlayback`]) を見て
//!
//! ```text
//! Arranger        : 拍 = song の playhead     / 源 = track.clips (lane.clips)
//! Launcher(cell)  : 拍 = セル内の位相         / 源 = その 1 セル
//! LauncherStopped : 何も映さない (オートメーションはレーン既定値)
//! ```
//!
//! を解く。この 1 本を [`RowTimeline::track_scan`] / [`RowTimeline::lane_scan`] /
//! [`RowTimeline::lane_value`] として出し、**映像 (`video_playback`) / 画像
//! (`image_compose`) / 字幕 (`text_compose`) / グループ変換 (`group_compose`) /
//! 映像効果 (`video_fx`) / 動画書き出し (`render_video`) が全部ここを通る**。
//! 各所へ写すと「音は次のループに入ったのに絵が前のループのまま」で出る。
//!
//! # 実効拍の式 (計画書 §2.1 と厳密に一致させること)
//!
//! ```text
//! effective_beat = launch_beat + ((playhead_beats - launch_beat) mod loop_len)
//! ```
//!
//! ここで返す [`RowScan::clip_beat`] は **`effective_beat - launch_beat`** =
//! セル内の位相。式が引き算 1 つぶんズレて見えるのは座標系の違いだけで、指す
//! 瞬間は同一 — daw_audio 側はセルを「`launch_beat` に置かれたクリップ」として
//! 描くのに対し、`SessionClip.clip.start_beat` は **常に 0** (セルは撃った瞬間が
//! 原点で song-absolute な配置を持たない、`common::model::session` の契約) なので、
//! GUI 側はセルのタイムライン (原点 0) で走査する。テンポ写像 (拍 → 秒) だけは
//! song-absolute が要るので [`RowScan::song_beat`] を使う。
//!
//! [`RowScan::clip_beat`] は正確には `セルの start_beat + 位相` で、契約どおり
//! `start_beat == 0` なら位相そのもの ([`RowTimeline::cell_scan`] の doc)。
//!
//! # 起点 (`launch_beat`) をどこから取るか
//!
//! 走行中のセル位置は `Song` に入らない (計画書 §1.4 — 保存すると「何秒鳴らして
//! から書き出したか」で出力が変わり、Q9 の再現性が壊れる)。よってここは
//! **`Song` 起点の決定的解決**を既定にする:
//!
//! - プレビュー ([`RowTimeline::preview`]) … 起点 = 曲頭 (拍 0)。
//! - 書き出し ([`RowTimeline::export`]) … 起点 = 書き出し範囲の先頭
//!   (計画書 Q9 / §2.5「範囲の先頭で今の `Track.launcher` を一斉に撃った」)。
//!
//! 束 B (daw_audio) が `common::audio_bridge` へ走行状態を publish したら、
//! [`RunningRow`] の列を [`RowTimeline::with_running`] へ渡すだけで、フォロー
//! アクションで遷移した先まで絵が追う。**未接続でも既定値でコンパイルが通る**。

use common::model::{
    AutomationClip, AutomationLane, Clip, RowPlayback, SessionAutomationClip, SessionClip, Song,
    Track,
};

/// 行の identity。行 = arrangement の 1 行と 1:1 で、トラック行と展開した
/// オートメーションレーン行の両方がある (計画書 Q4)。
///
/// マスター行のレーン (`Song.song_lanes`) は
/// `track = common::model::MASTER_TRACK_ID`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RowId {
    pub track: u32,
    /// `None` = トラック行 / `Some(lane_id)` = オートメーションレーン行。
    pub lane: Option<u32>,
}

impl RowId {
    #[must_use]
    pub fn track(track: u32) -> Self {
        Self { track, lane: None }
    }

    #[must_use]
    pub fn lane(track: u32, lane: u32) -> Self {
        Self { track, lane: Some(lane) }
    }
}

/// 束 B が publish する **走行状態**の 1 行分 (計画書 §1.4 — `Song` には入らない
/// 表示専用データ)。
///
/// まだ配線が無いので既定は空スライス。差すときは
/// [`RowTimeline::with_running`] へ渡す (それ以外の呼び側は 1 行も変わらない)。
///
/// **表に載っていない行は `Song` 側 (`Track.launcher` / `AutomationLane.launcher`) へ
/// フォールバックする**ので、差分 publish でも全行 publish でも同じ結果になる。
/// `state` が [`RowPlayback`] そのものなのはそのため — 「アレンジへ返した行」を
/// 表に載せても正しく解ける (`Option<clip_id>` だと `None` が停止と区別できない)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RunningRow {
    pub row: RowId,
    /// いまの主導権。走行中のフォローアクション遷移はここに出る。
    pub state: RowPlayback,
    /// [`RowPlayback::Launcher`] のとき、そのセルを撃った song-absolute 拍。
    /// 他の状態では無視される。
    pub launch_beat: f64,
}

/// 1 行を解いた結果 = 「どのクリップ列を、どの拍で走査するか」。
///
/// アレンジ行では `clips = &track.clips` / `clip_beat = song の playhead` なので
/// **従来と 1 bit も変わらない**。ランチャー行では `clips` が 1 要素 (そのセル)、
/// `clip_beat` がセル内の位相になる。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RowScan<'a, C> {
    /// 走査するクリップ列。ランチャー行では長さ 1。
    pub clips: &'a [C],
    /// クリップ / イベントの窓判定と content 座標への換算に使う拍。
    /// アレンジ行では song の playhead、ランチャー行では
    /// `セルの start_beat (契約上 0) + 位相`。
    pub clip_beat: f64,
    /// テンポ写像 (拍 → 秒) に使う song-absolute 拍。アレンジ行では
    /// `clip_beat` と同値、ランチャー行では `launch_beat + 位相`
    /// (= 計画書 §2.1 の `effective_beat`)。
    pub song_beat: f64,
}

impl<C> RowScan<'_, C> {
    /// `clips` の座標系の拍 0 が置かれる song-absolute 拍。アレンジ行では `0.0`、
    /// ランチャー行では `launch_beat - セルの start_beat` (契約上 `start_beat == 0`
    /// なので `launch_beat`)。イベントの絶対拍を出すときに使う
    /// (`song_origin() + clip.start_beat + event.event_start_in_clip_beats` が
    /// アレンジ行でもランチャー行でも正しい絶対拍になる)。
    #[must_use]
    #[inline]
    pub fn song_origin(&self) -> f64 {
        self.song_beat - self.clip_beat
    }
}

/// 行ごとの時間解決器。1 フレームに 1 つ作って、映像 / 画像 / 字幕 / 変換 /
/// 効果 / 書き出しの全部へ配る。確保なし (`Copy` な数値 + 借りたスライス)。
#[derive(Debug, Clone, Copy)]
pub struct RowTimeline<'a> {
    playhead_beat: f64,
    origin_beat: f64,
    running: &'a [RunningRow],
}

impl RowTimeline<'static> {
    /// プレビュー用。走行状態が未接続なので「曲頭で `Track.launcher` を一斉に
    /// 撃った」決定的解決になる (module doc「起点をどこから取るか」)。
    #[must_use]
    pub fn preview(playhead_beat: f64) -> Self {
        Self::with_running(0.0, playhead_beat, &[])
    }

    /// GUI の `TransportState::playhead_beat` (停止中は `None`) からの近道。
    /// `None` は拍 0 として解く。
    #[must_use]
    pub fn at_playhead(playhead_beat: Option<f32>) -> Self {
        Self::preview(playhead_beat.map(f64::from).unwrap_or(0.0))
    }

    /// 動画書き出し用。起点は **書き出し範囲の先頭** (計画書 Q9 / §2.5)。
    /// 同じプロジェクトなら何度書き出しても同じ絵になる。
    #[must_use]
    pub fn export(origin_beat: f64, playhead_beat: f64) -> Self {
        Self::with_running(origin_beat, playhead_beat, &[])
    }
}

impl<'a> RowTimeline<'a> {
    /// 束 B の走行状態を差した形。`running` に載っている行はその
    /// `launch_beat` / `clip_id` が `Song.launcher` と `origin_beat` に優先する
    /// (= フォローアクションで遷移した先まで絵が追う)。
    #[must_use]
    pub fn with_running(origin_beat: f64, playhead_beat: f64, running: &'a [RunningRow]) -> Self {
        Self { playhead_beat, origin_beat, running }
    }

    /// トラック行を解く。`None` = 何も映さない
    /// ([`RowPlayback::LauncherStopped`] / ワンショット終端 / セル消失)。
    #[must_use]
    pub fn track_scan<'t>(&self, track: &'t Track) -> Option<RowScan<'t, Clip>> {
        let row = RowId::track(track.id);
        match self.row_state(row, track.launcher) {
            RowState::Arranger => Some(self.arranger_scan(&track.clips)),
            RowState::Stopped => None,
            RowState::Cell { clip_id, launch_beat } => {
                let cell: &SessionClip =
                    track.session_clips.iter().find(|c| c.clip.id == clip_id)?;
                self.cell_scan(
                    std::slice::from_ref(&cell.clip),
                    (cell.clip.start_beat, cell.clip.length_beats),
                    cell.launch.looping,
                    launch_beat,
                )
            }
        }
    }

    /// オートメーションレーン行を解く。`None` = レーン既定値を出すべき状態
    /// (計画書 Q11: セルの無いシーンを撃つとレーン既定値へ戻る)。
    #[must_use]
    pub fn lane_scan<'t>(
        &self,
        track_id: u32,
        lane: &'t AutomationLane,
    ) -> Option<RowScan<'t, AutomationClip>> {
        let row = RowId::lane(track_id, lane.id);
        match self.row_state(row, lane.launcher) {
            RowState::Arranger => Some(self.arranger_scan(&lane.clips)),
            RowState::Stopped => None,
            RowState::Cell { clip_id, launch_beat } => {
                let cell: &SessionAutomationClip =
                    lane.session_clips.iter().find(|c| c.clip.id == clip_id)?;
                self.cell_scan(
                    std::slice::from_ref(&cell.clip),
                    (cell.clip.start_beat, cell.clip.length_beats),
                    cell.launch.looping,
                    launch_beat,
                )
            }
        }
    }

    /// レーン行の**主導権を織り込んだ**値。アレンジ行なら
    /// `common::automation::lane_value_at` と完全に同値で、ランチャー行なら
    /// 供給元がそのセル 1 つに切り替わる。判定規則の SSoT は
    /// `common::automation::lane_value_over` (アレンジもセルもそこを通る)。
    #[must_use]
    pub fn lane_value(&self, track_id: u32, lane: &AutomationLane, song: &Song) -> f64 {
        match self.lane_scan(track_id, lane) {
            Some(scan) => common::automation::lane_value_over(
                lane,
                scan.clips,
                &song.clip_contents,
                scan.clip_beat,
            ),
            None => lane.default_value,
        }
    }

    /// `Song` の主導権に走行状態を重ねた、この行の今の状態。走行状態に載っていない
    /// 行は `Song` の主導権 + `origin_beat` で解く ([`RunningRow`] の契約)。
    fn row_state(&self, row: RowId, saved: RowPlayback) -> RowState {
        let (playback, launch_beat) = match self.running.iter().find(|r| r.row == row) {
            Some(live) => (live.state, live.launch_beat),
            None => (saved, self.origin_beat),
        };
        match playback {
            RowPlayback::Arranger => RowState::Arranger,
            RowPlayback::LauncherStopped => RowState::Stopped,
            RowPlayback::Launcher { clip_id } => RowState::Cell { clip_id, launch_beat },
        }
    }

    fn arranger_scan<'t, C>(&self, clips: &'t [C]) -> RowScan<'t, C> {
        RowScan { clips, clip_beat: self.playhead_beat, song_beat: self.playhead_beat }
    }

    /// セル 1 つぶんの [`RowScan`]。`window` はそのセルの
    /// `(start_beat, length_beats)`。
    ///
    /// `clip_beat` に `start_beat` を足しているのは、下流の窓判定が
    /// `[clip.start_beat, clip.start_beat + clip.length_beats)` で行われるため。
    /// セルの `start_beat` は常に 0 という契約 (`Song::normalize_session` が正す)
    /// なので通常は足しても同じだが、こう書くと **契約が破れていても絵が消えない**
    /// (content 座標への換算 `song_to_content_beat` が `start_beat` を打ち消すので、
    /// 結果は位相 + `content_offset_beats` のまま)。
    fn cell_scan<'t, C>(
        &self,
        clips: &'t [C],
        window: (f64, f64),
        looping: bool,
        launch_beat: f64,
    ) -> Option<RowScan<'t, C>> {
        let (start_beat, length_beats) = window;
        let phase = cell_phase(launch_beat, self.playhead_beat, length_beats, looping)?;
        Some(RowScan { clips, clip_beat: start_beat + phase, song_beat: launch_beat + phase })
    }
}

/// [`RowTimeline::row_state`] の結果。`Song` 由来と走行状態由来を 1 つの形に
/// 畳んで、トラック行とレーン行が同じ分岐を通るようにする。
#[derive(Debug, Clone, Copy, PartialEq)]
enum RowState {
    Arranger,
    Stopped,
    Cell { clip_id: u32, launch_beat: f64 },
}

/// 撃った拍と playhead が「同時」とみなされる許容幅 (拍)。
///
/// **これが無いと範囲書き出しの 1 フレーム目が全ランチャー行で無地になる。**
/// `render_video` の frame 0 は `seconds_to_beat(beat_to_seconds(start_beat))` で
/// 拍へ戻すが、`TempoMap` は 1/16 拍刻みの表を線形補間するので往復は厳密でない —
/// 実測で **1/16 拍に乗らない拍の約 6% が最大 5.7e-14 拍ぶん下振れ**する。
/// 素朴に `elapsed < 0.0` で切ると、その下振れが「まだ撃たれていない」と判定される。
/// `1e-9` 拍は 120 BPM で 0.5 ns — 浮動小数の雑音より遥かに大きく、
/// 音楽的に意味のある間隔より遥かに小さい。
const LAUNCH_EPSILON_BEATS: f64 = 1e-9;

/// **実効拍の式そのもの** (計画書 §2.1)。返すのは
/// `effective_beat - launch_beat` = セル内の位相で、値域は `[0, loop_len)`。
///
/// `None` を返すのは「今このセルは鳴っていない」ケース:
/// - まだ撃たれていない (`playhead < launch_beat`、[`LAUNCH_EPSILON_BEATS`] 未満の
///   下振れは「撃った瞬間」として扱う)
/// - ワンショット (`looping == false`) が終端を過ぎた
/// - 長さ 0 / 非有限 (0 除算と無限ループを下流へ流さない)
///
/// daw_audio 側 (束 B) はプロセスが別なので実装を共有できない。**境界条件
/// (ループ跨ぎ / ワンショット終端 / 停止) をここと厳密に一致させること** —
/// ズレると「音は次のループに入ったのに絵が前のループのまま」になる。
#[must_use]
pub fn cell_phase(
    launch_beat: f64,
    playhead_beat: f64,
    loop_len: f64,
    looping: bool,
) -> Option<f64> {
    if !loop_len.is_finite()
        || loop_len <= 0.0
        || !launch_beat.is_finite()
        || !playhead_beat.is_finite()
    {
        return None;
    }
    let elapsed = playhead_beat - launch_beat;
    if elapsed < -LAUNCH_EPSILON_BEATS {
        return None;
    }
    let elapsed = elapsed.max(0.0);
    if !looping {
        return (elapsed < loop_len).then_some(elapsed);
    }
    // `elapsed >= 0` かつ `loop_len > 0` なので、fmod (`%`) は厳密に
    // `[0, loop_len)` を返す (IEEE 754 の剰余は丸め誤差を持たない)。
    Some(elapsed % loop_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{
        AutomationTarget, LaunchSettings, MASTER_TRACK_ID, TrackBuiltinParam,
    };

    // ---- 式そのもの (ループ跨ぎ / ワンショット終端 / 停止) -------------------

    #[test]
    fn ループするセルは長さで巻き戻る() {
        // 4 拍のセルを拍 8 で撃った。
        let phase = |playhead| cell_phase(8.0, playhead, 4.0, true);
        assert_eq!(phase(8.0), Some(0.0), "撃った瞬間は頭");
        assert_eq!(phase(11.0), Some(3.0), "1 周目の途中");
        assert_eq!(phase(12.0), Some(0.0), "ループ境界でちょうど頭へ戻る");
        assert_eq!(phase(13.5), Some(1.5), "2 周目の途中");
        assert_eq!(phase(7.5), None, "撃つ前は鳴っていない");
    }

    #[test]
    fn ワンショットは終端で止まる() {
        let phase = |playhead| cell_phase(8.0, playhead, 4.0, false);
        assert_eq!(phase(11.5), Some(3.5), "終端の手前は鳴っている");
        assert_eq!(phase(12.0), None, "終端に達したら鳴らない");
        assert_eq!(phase(100.0), None);
    }

    #[test]
    fn 壊れた長さは鳴らさない() {
        assert_eq!(cell_phase(0.0, 4.0, 0.0, true), None, "長さ 0 で無限ループしない");
        assert_eq!(cell_phase(0.0, 4.0, -1.0, true), None);
        assert_eq!(cell_phase(0.0, 4.0, f64::NAN, true), None);
        assert_eq!(cell_phase(f64::INFINITY, 4.0, 4.0, true), None);
    }


    // ---- 行の解決 -----------------------------------------------------------

    /// arrangement クリップ 1 つ (拍 16 から 4 拍) と launcher セル 1 つ
    /// (長さ 4 拍) を持つトラック。
    fn track_with_cell() -> Track {
        let mut track = Track { id: 7, next_clip_id: 1, ..Track::default() };
        track.clips.push(Clip {
            id: 1,
            start_beat: 16.0,
            length_beats: 4.0,
            ..Clip::default()
        });
        track.session_clips.push(SessionClip {
            scene_id: 1,
            clip: Clip { id: 2, start_beat: 0.0, length_beats: 4.0, ..Clip::default() },
            launch: LaunchSettings::default(),
        });
        track
    }

    #[test]
    fn アレンジ行は従来どおり_song_の拍で_track_clips_を見る() {
        let track = track_with_cell();
        let rows = RowTimeline::preview(17.0);
        let scan = rows.track_scan(&track).expect("アレンジ行は必ず走査対象");
        assert_eq!(scan.clips.len(), 1);
        assert_eq!(scan.clips[0].id, 1, "arrangement のクリップが源");
        assert_eq!(scan.clip_beat, 17.0);
        assert_eq!(scan.song_beat, 17.0);
        assert_eq!(scan.song_origin(), 0.0);
    }

    #[test]
    fn ランチャー行はセル_1_つを位相で見る() {
        let mut track = track_with_cell();
        track.launcher = RowPlayback::Launcher { clip_id: 2 };
        // 起点 = 曲頭。4 拍セルなので拍 17 の位相は 1.0。
        let rows = RowTimeline::preview(17.0);
        let scan = rows.track_scan(&track).expect("鳴っている");
        assert_eq!(scan.clips.len(), 1, "源はそのセル 1 つだけ");
        assert_eq!(scan.clips[0].id, 2);
        assert_eq!(scan.clip_beat, 1.0);
        assert_eq!(scan.song_beat, 1.0, "起点 0 + 位相 1.0");
        assert_eq!(scan.song_origin(), 0.0);
    }

    #[test]
    fn 書き出しは範囲の先頭で一斉に撃った状態から始まる() {
        let mut track = track_with_cell();
        track.launcher = RowPlayback::Launcher { clip_id: 2 };
        // 書き出し範囲 [10, ..) の 1 拍目 → 位相 1.0、song 拍 11.0。
        let rows = RowTimeline::export(10.0, 11.0);
        let scan = rows.track_scan(&track).expect("鳴っている");
        assert_eq!(scan.clip_beat, 1.0);
        assert_eq!(scan.song_beat, 11.0, "テンポ写像は song-absolute で行う");
        assert_eq!(scan.song_origin(), 10.0);
        // 範囲の先頭ちょうどはセルの頭。
        let head = RowTimeline::export(10.0, 10.0).track_scan(&track).unwrap();
        assert_eq!(head.clip_beat, 0.0);
    }

    #[test]
    fn 停止した行は何も映さない() {
        let mut track = track_with_cell();
        track.launcher = RowPlayback::LauncherStopped;
        assert!(RowTimeline::preview(17.0).track_scan(&track).is_none());
    }

    #[test]
    fn 実在しないセルを指す主導権は何も映さない() {
        let mut track = track_with_cell();
        track.launcher = RowPlayback::Launcher { clip_id: 999 };
        assert!(RowTimeline::preview(17.0).track_scan(&track).is_none());
    }

    #[test]
    fn 走行状態を差すと起点と鳴っているセルが上書きされる() {
        let mut track = track_with_cell();
        // Song は「セル 2 を撃った」状態だが、走行中はフォローアクションで
        // 別のセルへ移り、拍 12 で撃ち直している。
        track.launcher = RowPlayback::Launcher { clip_id: 2 };
        track.session_clips.push(SessionClip {
            scene_id: 2,
            clip: Clip { id: 3, start_beat: 0.0, length_beats: 4.0, ..Clip::default() },
            launch: LaunchSettings::default(),
        });
        let running = [RunningRow {
            row: RowId::track(7),
            state: RowPlayback::Launcher { clip_id: 3 },
            launch_beat: 12.0,
        }];
        let rows = RowTimeline::with_running(0.0, 13.0, &running);
        let scan = rows.track_scan(&track).expect("走行中のセルが鳴っている");
        assert_eq!(scan.clips[0].id, 3, "Song ではなく走行状態のセル");
        assert_eq!(scan.clip_beat, 1.0);
        assert_eq!(scan.song_beat, 13.0);

        // 走行状態が「止まっている」なら Song が Launcher でも何も映さない。
        let stopped = [RunningRow {
            row: RowId::track(7),
            state: RowPlayback::LauncherStopped,
            launch_beat: 0.0,
        }];
        assert!(RowTimeline::with_running(0.0, 13.0, &stopped).track_scan(&track).is_none());

        // 表に載っていない行は Song へフォールバックする (束 B が差分 publish しても
        // 「載せなかった行が全部消える」ことにならない)。
        let other = [RunningRow {
            row: RowId::track(999),
            state: RowPlayback::LauncherStopped,
            launch_beat: 0.0,
        }];
        let fallback = RowTimeline::with_running(0.0, 17.0, &other)
            .track_scan(&track)
            .expect("Song の Launcher が生きる");
        assert_eq!(fallback.clips[0].id, 2, "Song が指すセル");
    }

    /// 範囲書き出しの 1 フレーム目は `TempoMap` の拍↔秒往復で `start_beat` を
    /// わずかに下回ることがある (実測で 1/16 拍に乗らない拍の約 6%、最大 5.7e-14 拍)。
    /// そこで「まだ撃たれていない」と判定すると **全ランチャー行が 1 フレームだけ
    /// 無地になる**。
    #[test]
    fn 撃った拍のわずかな下振れは頭として扱う() {
        let mut track = track_with_cell();
        track.launcher = RowPlayback::Launcher { clip_id: 2 };
        let rows = RowTimeline::export(10.0, 10.0 - 5.7e-14);
        let scan = rows.track_scan(&track).expect("1 フレーム目が消えてはいけない");
        assert_eq!(scan.clip_beat, 0.0, "セルの頭に丸める");
        // 本物の「まだ撃たれていない」(量子化予約など) は従来どおり無音。
        assert!(RowTimeline::export(10.0, 9.9).track_scan(&track).is_none());
    }

    // ---- オートメーションレーン行 ------------------------------------------

    fn lane_with_cell(song: &mut Song) -> AutomationLane {
        use common::model::{AutomationContent, AutomationCurve, AutomationPoint, ClipContent};
        let mut lane =
            AutomationLane::new(AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume), 0.25);
        lane.id = 3;
        // arrangement 側: 拍 [16, 20) で 1.0 一定。
        let arr = song.alloc_content_id();
        song.clip_contents.insert(
            arr,
            ClipContent::Automation(AutomationContent {
                points: vec![AutomationPoint {
                    id: 1,
                    time_beat: 0.0,
                    value: 1.0,
                    curve: AutomationCurve::Linear,
                }],
                next_point_id: 2,
            }),
        );
        lane.clips.push(AutomationClip {
            id: 1,
            start_beat: 16.0,
            length_beats: 4.0,
            content_id: arr,
            ..AutomationClip::default()
        });
        // セル: 長さ 4 拍で 0.75 一定。
        let cell = song.alloc_content_id();
        song.clip_contents.insert(
            cell,
            ClipContent::Automation(AutomationContent {
                points: vec![AutomationPoint {
                    id: 1,
                    time_beat: 0.0,
                    value: 0.75,
                    curve: AutomationCurve::Linear,
                }],
                next_point_id: 2,
            }),
        );
        lane.session_clips.push(SessionAutomationClip {
            scene_id: 1,
            clip: AutomationClip {
                id: 2,
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cell,
                ..AutomationClip::default()
            },
            launch: LaunchSettings::default(),
        });
        lane
    }

    #[test]
    fn レーン行の値は主導権で供給元が変わる() {
        let mut song = Song::default();
        let mut lane = lane_with_cell(&mut song);

        // アレンジ行: 拍 17 は arrangement クリップの中 → 1.0。
        let rows = RowTimeline::preview(17.0);
        assert_eq!(rows.lane_value(1, &lane, &song), 1.0);
        // 窓の外は既定値。
        assert_eq!(RowTimeline::preview(4.0).lane_value(1, &lane, &song), 0.25);

        // ランチャー行: 同じ拍 17 でもセルの値が出る。
        lane.launcher = RowPlayback::Launcher { clip_id: 2 };
        assert_eq!(rows.lane_value(1, &lane, &song), 0.75);

        // 停止した行はレーン既定値 (計画書 Q11)。
        lane.launcher = RowPlayback::LauncherStopped;
        assert_eq!(rows.lane_value(1, &lane, &song), 0.25);
    }

    /// 走行状態は **行 id** で引く。マスター行のレーン
    /// (`track = MASTER_TRACK_ID`) を止めれば、そのレーンだけ既定値へ戻る。
    #[test]
    fn 走行状態はマスター行のレーンも行_id_で引ける() {
        let mut song = Song::default();
        let mut lane = lane_with_cell(&mut song);
        lane.launcher = RowPlayback::Launcher { clip_id: 2 };
        let stopped = [RunningRow {
            row: RowId::lane(MASTER_TRACK_ID, 3),
            state: RowPlayback::LauncherStopped,
            launch_beat: 0.0,
        }];
        let rows = RowTimeline::with_running(0.0, 17.0, &stopped);
        assert_eq!(rows.lane_value(MASTER_TRACK_ID, &lane, &song), 0.25);
        // 別トラックの同じ lane id は別の行なので影響を受けない。
        assert_eq!(rows.lane_value(1, &lane, &song), 0.75);
    }
}
