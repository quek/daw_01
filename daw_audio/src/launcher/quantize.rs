//! ローンチ量子化 — 「押した」を「いつ発火するか」に変える計算
//! (`docs/plan_rmd_87_clip_launcher.md` §2.2)。
//!
//! **境界は拍で解く。** engine の `playhead_beats` は `SongTempo` automation を
//! 積分した拍 (`common::tempo_map::TempoMap` と同じ量) なので、拍で `q` の倍数を
//! 取ればそれが `metronome::ClickGrid::Song` と同じグリッドになる。瞬間 bpm と
//! sample の等間隔グリッドで解くとテンポ変更後に click / note とズレる — その
//! 失敗は r.md #39 で metronome 側が一度踏んでいる。
//!
//! 1 小節の拍数は [`common::model::LaunchQuantize::beats`] が唯一の口
//! (`4.0` 決め打ちを散らさない)。

use common::model::{DEFAULT_GLOBAL_LAUNCH_QUANTIZE, LaunchQuantize};

use super::is_positive;

/// セルの量子化設定を実際の粒度 (拍) に解決する。
///
/// - [`LaunchQuantize::Global`] は `global` へ解決してから測る。`global` 自身が
///   `Global` (= 自己参照) なら既定 (1 小節) に倒す。
/// - `None` = **量子化しない** (押した瞬間に発火)。`Off` と、退化した設定
///   (`Bars(0)` / `Note { div: 0 }`) の両方がここに来る — 0 を粒度にすると
///   `mod` が 0 除算になるので、engine へ流さずここで潰す。
#[must_use]
pub fn resolve(cell: LaunchQuantize, global: LaunchQuantize, time_sig: (u8, u8)) -> Option<f64> {
    let effective = match cell {
        LaunchQuantize::Global => match global {
            LaunchQuantize::Global => DEFAULT_GLOBAL_LAUNCH_QUANTIZE,
            other => other,
        },
        other => other,
    };
    effective.beats(time_sig).filter(|b| is_positive(*b))
}

/// `beat` 以降で最初に来る `q` の倍数。`beat` がちょうど境界なら `beat` 自身
/// (= 拍頭で押したら待たずに鳴る、Live / Bitwig と同じ)。
///
/// 拍の累算誤差で「境界の 1e-12 手前」に居ることがあるので、
/// [`GRID_EPSILON_BEATS`] だけ手前を境界とみなす。これが無いと
/// 「拍頭ちょうどで押したのに 1 小節待たされる」が実機で起きる。
#[must_use]
pub fn next_boundary(beat: f64, q: f64) -> f64 {
    if !is_positive(q) || !beat.is_finite() {
        return beat;
    }
    let k = ((beat - GRID_EPSILON_BEATS) / q).ceil();
    let at = k * q;
    if at < beat - GRID_EPSILON_BEATS { beat } else { at.max(0.0) }
}

/// 境界判定の許容幅 (拍)。1/1000 拍 = 120 BPM で 0.5 ms — 拍の累算誤差
/// (f64 で 1e-12 オーダー) より十分大きく、人間が感じる先行より十分小さい。
pub const GRID_EPSILON_BEATS: f64 = 0.001;

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{
        AutomationClip, AutomationContent, AutomationCurve, AutomationLane, AutomationPoint,
        AutomationTarget, ClipContent, Song,
    };

    #[test]
    fn グローバル既定は_1_小節で拍子に従う() {
        // セルが Global、グローバル設定も未設定 (= Global) なら 1 小節。
        assert_eq!(
            resolve(LaunchQuantize::Global, LaunchQuantize::Global, (4, 4)),
            Some(4.0)
        );
        assert_eq!(
            resolve(LaunchQuantize::Global, LaunchQuantize::Global, (3, 4)),
            Some(3.0)
        );
        // グローバルを 1/16 にすればセルもそれに従う。
        assert_eq!(
            resolve(
                LaunchQuantize::Global,
                LaunchQuantize::Note { div: 16, triplet: false },
                (4, 4)
            ),
            Some(0.25)
        );
        // セル自身の設定はグローバルより優先。
        assert_eq!(
            resolve(LaunchQuantize::Bars(2), LaunchQuantize::Off, (4, 4)),
            Some(8.0)
        );
    }

    #[test]
    fn 量子化なしと壊れた設定はどちらも_none() {
        assert_eq!(resolve(LaunchQuantize::Off, LaunchQuantize::Bars(1), (4, 4)), None);
        // 0 小節 / 0 分音符は 0 除算の種なので engine へ流さない。
        assert_eq!(resolve(LaunchQuantize::Bars(0), LaunchQuantize::Bars(1), (4, 4)), None);
        assert_eq!(
            resolve(
                LaunchQuantize::Note { div: 0, triplet: false },
                LaunchQuantize::Bars(1),
                (4, 4)
            ),
            None
        );
    }

    #[test]
    fn 拍頭で押したら待たされない() {
        assert_eq!(next_boundary(4.0, 4.0), 4.0);
        assert_eq!(next_boundary(0.0, 4.0), 0.0);
        // 累算誤差で境界の直前に居ても、その境界で鳴る。
        assert_eq!(next_boundary(4.0 - 1e-9, 4.0), 4.0);
        // 境界を過ぎたら次。
        assert_eq!(next_boundary(4.01, 4.0), 8.0);
        assert_eq!(next_boundary(5.5, 4.0), 8.0);
    }

    /// r.md #39 と同じ罠の回帰: テンポカーブのある曲で、量子化境界の **実時間**が
    /// metronome の click (= `ClickGrid::Song`) と一致すること。
    ///
    /// 拍で解いているので一致するのが当然だが、「瞬間 bpm × sample の等間隔
    /// グリッド」に書き換えるとここが落ちる。
    #[test]
    fn テンポ変更後も_click_と同じグリッドに乗る() {
        // 60 → 180 BPM に 16 拍かけて上がる曲。
        let mut song = Song { bpm: 60.0, length_beats: 32.0, ..Song::default() };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                next_point_id: 3,
                points: vec![
                    AutomationPoint {
                        id: 1,
                        time_beat: 0.0,
                        value: 60.0,
                        curve: AutomationCurve::Linear,
                    },
                    AutomationPoint {
                        id: 2,
                        time_beat: 16.0,
                        value: 180.0,
                        curve: AutomationCurve::Linear,
                    },
                ],
            }),
        );
        song.song_lanes.push(AutomationLane {
            id: 1,
            clips: vec![AutomationClip {
                id: 1,
                name: String::new(),
                start_beat: 0.0,
                length_beats: 16.0,
                content_id: cid,
                content_offset_beats: 0.0,
            }],
            ..AutomationLane::new(AutomationTarget::SongTempo, 60.0)
        });
        let map = common::tempo_map::TempoMap::from_song(&song);

        // 拍 9.3 で 1 小節量子化 → 拍 12.0 (= 4 小節目の頭 = click が鳴る拍)。
        let q = resolve(LaunchQuantize::Global, LaunchQuantize::Bars(1), song.time_sig).unwrap();
        let fire = next_boundary(9.3, q);
        assert_eq!(fire, 12.0);

        // その実時間は tempo map 由来 = metronome が click を置く位置。
        // 一定 60 BPM の素朴な換算 (12 秒) とは 3 秒以上ずれる = 実際にテンポを
        // 積分していることの証拠。
        let fire_secs = map.beat_to_seconds(fire);
        let naive_secs = fire * 60.0 / 60.0;
        assert!(
            (fire_secs - naive_secs).abs() > 3.0,
            "テンポ積分が効いていない: {fire_secs} vs {naive_secs}"
        );
        // 逆写像で拍へ戻すと同じ拍に着く (= click と同じ格子点)。
        assert!((map.seconds_to_beat(fire_secs) - 12.0).abs() < 0.01);
    }

}
