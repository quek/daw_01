//! グローバルトランスポーズ (r.md #130、`docs/plan_rmd_130_transpose.md`) の評価の SSoT。
//!
//! 移調は `Song` が持つ **非破壊の曲パラメーター** ([`Song::transpose`] と、song 側の置き場の
//! `SongTranspose` のレーン / 変調) で、ノートのデータは書き換えない。音程を生む全経路 — engine のノートと
//! オーディオクリップ、GUI の演奏プレビュー、VOICEVOX 歌唱 / 口パクのメタデータ、SMF 書き出し — は
//! ここにある次の 3 つだけを引く (経路ごとに式を写さない):
//!
//! - **移調量**: [`transpose_in_clip`]。engine は置き場の索引で lane と routing を引いてから呼び
//!   (`daw_audio::automation::resolve_song_transpose`)、GUI のプレビューは [`transpose_at`]、
//!   オフライン (変調を持たない) は [`TransposeCurve`] が呼ぶ。
//! - **トラックが追従するか**: [`Song::track_follows_transpose`] (自分と祖先グループ全部)。
//! - **鳴らす鍵盤**: [`sounding_key`] (0..=127 を外れたら鳴らさない。端に丸めない)。
//!
//! wire に載らないロジックだけを持つ (`common/build.rs` の `WIRE_SOURCES` 対象外)。

use crate::automation::{apply_modulation_over, lane_value_in_clip};
use crate::model::{AutomationClip, AutomationLane, AutomationTarget, ModRouting, Song};
use crate::song_index::CoverIndex;

/// 移調の幅 (半音、`-24..=24`)。値域の SSoT — 正規化 (`automation::target_range`)、読み込みの丸め
/// (`Song::sanitize_ranges`)、IPC の境界、UI のスクラブ範囲がこれを引く。
pub const TRANSPOSE_MAX_SEMITONES: i8 = 24;

/// 連続値 (レーンのカーブ + 変調) → 実効の移調量 (半音)。**丸めてから** 幅に収める
/// (レーンを直線で引いても鍵盤は半音の段で変わる)。非有限は 0。
#[must_use]
pub fn quantize_transpose(plain: f64) -> i32 {
    if !plain.is_finite() {
        return 0;
    }
    let max = f64::from(TRANSPOSE_MAX_SEMITONES);
    #[allow(clippy::cast_possible_truncation)]
    let v = plain.round().clamp(-max, max) as i32;
    v
}

/// `pitch` を `semitones` 移調した鍵盤。0..=127 を外れたら `None` (= 鳴らさない、確定仕様 Q4)。
///
/// **端に丸めない** — 丸めると別のノートと同じ鍵盤に重なり、VOICEVOX では 0 が休符と衝突する。
#[must_use]
pub fn sounding_key(pitch: u8, semitones: i32) -> Option<u8> {
    u8::try_from(i32::from(pitch) + semitones).ok().filter(|k| *k <= 127)
}

/// **移調量の評価の本体** (唯一の式)。
///
/// base = 有効な移調レーン (`lane`、`beat` を覆う clip を引き済み) のカーブ値、無ければ [`Song::transpose`]。
/// そこへ `SongTranspose` を指す変調 (`routings`、target で絞り済み) を正規化領域で重ね、
/// [`quantize_transpose`] で半音へ丸める。`scalar` / `depth` は変調の値面の読み口 (engine は刻みの値面、
/// GUI は publish された値面、オフラインは変調なしで routing を渡さない)。
///
/// RT 安全: 確保・ロックなし。
pub fn transpose_in_clip<'r, I>(
    song: &Song,
    lane: Option<(&AutomationLane, Option<&AutomationClip>)>,
    routings: I,
    scalar: impl Fn(u32) -> Option<f32>,
    depth: impl Fn(&ModRouting) -> f32,
    beat: f64,
) -> i32
where
    I: IntoIterator<Item = &'r ModRouting>,
    I::IntoIter: Clone,
{
    let base = match lane {
        Some((lane, clip)) => lane_value_in_clip(lane, clip, &song.clip_contents, beat),
        None => f64::from(song.transpose),
    };
    quantize_transpose(apply_modulation_over(&AutomationTarget::SongTranspose, base, routings, scalar, depth))
}

/// 移調を決める lane (`song_lanes` の並び順で最初の有効な `SongTranspose`)。off-RT 用 (RT は置き場の索引で引く)。
#[must_use]
pub fn transpose_lane(song: &Song) -> Option<&AutomationLane> {
    song.song_lanes.iter().find(|l| l.enabled && l.target == AutomationTarget::SongTranspose)
}

/// **変調を含む** 移調量を曲の拍 `beat` で 1 点だけ解く (off-RT)。GUI の演奏プレビューが、engine が
/// publish した値面を `scalar` / `depth` に渡して使う (engine の再生と同じ [`transpose_in_clip`] を通る)。
#[must_use]
pub fn transpose_at(
    song: &Song,
    beat: f64,
    scalar: impl Fn(u32) -> Option<f32>,
    depth: impl Fn(&ModRouting) -> f32,
) -> i32 {
    let lane = transpose_lane(song).map(|l| (l, crate::automation::clip_covering(&l.clips, beat)));
    let routings = song.song_mod_routings.iter().filter(|r| r.target == AutomationTarget::SongTranspose);
    transpose_in_clip(song, lane, routings, scalar, depth, beat)
}

/// **変調を持たない** 経路 (VOICEVOX 歌唱 / 口パク / SMF 書き出し) の移調量の読み口。lane と clip の索引を
/// 1 度だけ作る ([`crate::automation::SongTempoCurve`] と同じ形)。
///
/// 変調は再生時の信号なので、合成済みの歌声やファイルへは焼かない (テンポの変調が拍↔秒の換算に入らないのと同じ)。
#[derive(Debug, Clone)]
pub struct TransposeCurve<'a> {
    song: &'a Song,
    lane: Option<(&'a AutomationLane, CoverIndex)>,
}

impl<'a> TransposeCurve<'a> {
    #[must_use]
    pub fn of(song: &'a Song) -> Self {
        let lane = transpose_lane(song)
            .map(|l| (l, CoverIndex::build(l.clips.iter().map(|c| (c.start_beat, c.start_beat + c.length_beats)))));
        Self { song, lane }
    }

    /// 曲の拍 `beat` の移調量。
    #[must_use]
    pub fn at(&self, beat: f64) -> i32 {
        let lane = self.lane.as_ref().map(|(l, cover)| (*l, cover.first_covering(beat).and_then(|i| l.clips.get(i))));
        transpose_in_clip(self.song, lane, std::iter::empty(), |_| None, |r| r.depth, beat)
    }

    /// **曲の位置を持たない** ノート (ランチャーのセル) の移調量 = 曲の基準値 [`Song::transpose`]。
    /// セルはいつ撃たれるか分からないので、タイムライン上のレーンからは値を選べない。
    #[must_use]
    pub fn base(&self) -> i32 {
        quantize_transpose(f64::from(self.song.transpose))
    }
}

/// トラック 1 本のノートが **オフラインで** 鳴らす鍵盤の読み口 ([`TransposeCurve`] + 追従するか)。
#[derive(Debug, Clone)]
pub struct TrackTranspose<'a> {
    curve: TransposeCurve<'a>,
    follows: bool,
}

impl<'a> TrackTranspose<'a> {
    /// `track_id` のトラックの読み口 (追従しないトラックは書いた音のまま)。
    #[must_use]
    pub fn of(song: &'a Song, track_id: u32) -> Self {
        Self { curve: TransposeCurve::of(song), follows: song.track_follows_transpose(track_id) }
    }

    /// 移調しない読み口 (焼き込み = プロジェクトの中に作るものは書いた音、確定仕様 Q8)。
    #[must_use]
    pub fn written(song: &'a Song) -> Self {
        Self { curve: TransposeCurve::of(song), follows: false }
    }

    /// 音程 `pitch` のノートが鳴らす鍵盤。`song_beat` はノートの開始の曲の拍 (`None` = 曲の位置を持たない
    /// ランチャーのセル → [`TransposeCurve::base`])。範囲外は `None`。
    #[must_use]
    pub fn key(&self, pitch: u8, song_beat: Option<f64>) -> Option<u8> {
        if !self.follows {
            return Some(pitch);
        }
        let semitones = song_beat.map_or_else(|| self.curve.base(), |b| self.curve.at(b));
        sounding_key(pitch, semitones)
    }
}

impl Song {
    /// トラック `track_id` が移調に **実効的に** 追従するか: 自分と祖先グループ全部の
    /// [`crate::model::Track::follow_transpose`] が立っている (グループで外すと子もまとめて外れる)。
    /// 無いトラックは `false`。祖先の走査は [`Song::track_visually_silenced`] と同じ形 (循環は本数で打ち切る)。
    #[must_use]
    pub fn track_follows_transpose(&self, track_id: u32) -> bool {
        let mut cur = Some(track_id);
        let mut hops = 0usize;
        while let Some(id) = cur {
            if hops > self.tracks.len() {
                break;
            }
            let Some(t) = self.track_by_id(id) else {
                return hops > 0;
            };
            if !t.follow_transpose {
                return false;
            }
            cur = t.parent_group_id;
            hops += 1;
        }
        hops > 0
    }

    /// 移調量が 0 以外になりうるか (基準値 / 有効なレーン / 有効な変調のどれかがある)。engine が
    /// オーディオクリップにスペクトルエンジンを用意しておくかの判定 (compile 時。0 のままの曲は 1 基も確保しない)。
    #[must_use]
    pub fn transpose_can_be_nonzero(&self) -> bool {
        let t = AutomationTarget::SongTranspose;
        self.transpose != 0
            || self.song_lanes.iter().any(|l| l.enabled && l.target == t)
            || self.song_mod_routings.iter().any(|r| r.enabled && r.target == t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AutomationClip, AutomationContent, AutomationCurve, AutomationPoint, ClipContent, ModRouting, Polarity, Track,
    };

    /// 追従は自分と祖先グループ全部 (グループで外すと子も外れる、子で外しても親は追従したまま)。
    #[test]
    fn 追従は祖先グループまで辿って決まる() {
        let track = |id, parent| Track { id, parent_group_id: parent, ..Track::default() };
        let mut song = Song { tracks: vec![track(1, None), track(2, Some(1)), track(3, Some(2)), track(4, None)], ..Song::default() };
        assert!([1, 2, 3, 4].iter().all(|&id| song.track_follows_transpose(id)), "既定は全部追従");
        song.track_by_id_mut(1).unwrap().follow_transpose = false;
        assert_eq!([1, 2, 3, 4].map(|id| song.track_follows_transpose(id)), [false, false, false, true]);
        song.track_by_id_mut(1).unwrap().follow_transpose = true;
        song.track_by_id_mut(2).unwrap().follow_transpose = false;
        assert_eq!([1, 2, 3, 4].map(|id| song.track_follows_transpose(id)), [true, false, false, true]);
        assert!(!song.track_follows_transpose(99), "無いトラックは追従しない");
    }

    /// 移調量は レーン (曲の拍) → 変調 → 半音へ丸め → ±24。レーンが無ければ基準値。範囲外の鍵盤は鳴らさない。
    #[test]
    fn 移調量はレーンと変調を重ねて半音に丸める() {
        let mut song = Song { transpose: 3, ..Song::default() };
        assert_eq!(TransposeCurve::of(&song).at(10.0), 3, "レーンが無ければ基準値");

        // 0..4 拍で 0 → +8 の直線。2 拍目は +4、1.4 拍目は 2.8 → +3 (段で変わる)。
        let content_id = song.alloc_content_id();
        song.clip_contents.insert(
            content_id,
            ClipContent::Automation(AutomationContent {
                next_point_id: 3,
                points: vec![
                    AutomationPoint { id: 1, time_beat: 0.0, value: 0.0, curve: AutomationCurve::Linear },
                    AutomationPoint { id: 2, time_beat: 4.0, value: 8.0, curve: AutomationCurve::Linear },
                ],
            }),
        );
        let mut lane = AutomationLane::new(AutomationTarget::SongTranspose, -1.0);
        lane.id = 1;
        lane.clips.push(AutomationClip { id: 1, start_beat: 0.0, length_beats: 4.0, content_id, ..AutomationClip::default() });
        song.song_lanes.push(lane);
        let curve = TransposeCurve::of(&song);
        assert_eq!([curve.at(2.0), curve.at(1.4), curve.at(8.0)], [4, 3, -1], "clip の外はレーン既定値");
        assert_eq!(curve.base(), 3, "曲の位置を持たないノートは基準値");

        // 変調: 深さ 1.0 (正規化 = 48 半音ぶん) の Unipolar に scalar 0.5 → +24 → 丸めて上限。
        song.song_mod_routings.push(ModRouting {
            id: 1,
            target: AutomationTarget::SongTranspose,
            source_id: 7,
            depth: 1.0,
            polarity: Polarity::Unipolar,
            enabled: true,
        });
        assert_eq!(transpose_at(&song, 2.0, |id| (id == 7).then_some(0.5), |r| r.depth), 24);
        assert_eq!(transpose_at(&song, 2.0, |_| None, |r| r.depth), 4, "値面に無いソースの変調は効かない");

        assert_eq!([sounding_key(60, 2), sounding_key(126, 2), sounding_key(1, -2)], [Some(62), None, None]);
    }
}
