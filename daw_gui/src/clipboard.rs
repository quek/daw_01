//! FIXME #33: 統一クリップボード envelope。
//!
//! cut/copy/paste の全対象 (ノート / オートメーションの点 / オーディオイベント /
//! クリップ / トラック) を 1 つの型タグ付き envelope に入れ、OS クリップボードへ
//! JSON text として書く (`Ui::set_clipboard_text` / `Ui::take_clipboard_paste`)。
//! paste 側は trial-decode をやめ、`ClipboardPayload` の variant で分岐する。
//!
//! `source_project_id` は copy 元の `Song.project_id`。paste 時に現在の song と
//! 一致すれば clip/track は **リンク共有** (content_id 流用)、不一致なら inline
//! payload から **独立コピー** (新 content_id 採番) する。
//!
//! 外部 (他アプリ / 改竄) clipboard text を信用しないため、deserialize 後に
//! 値域を sanitize する (NaN/Inf/範囲外を model に入れない)。

use common::model::{
    AudioEvent, AutomationCurve, ClipContent, ContentId, Note, Track,
};
use serde::{Deserialize, Serialize};

/// daw_01 clipboard を他アプリ text と区別するマーカー兼 format version。
/// 形式を破壊変更したら version を上げる (古い clipboard は magic 不一致で no-op)。
pub const CLIPBOARD_MAGIC: &str = "daw_01.clipboard.v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardEnvelope {
    pub magic: String,
    /// copy 元の `Song.project_id`。同一なら clip/track paste はリンク共有。
    pub source_project_id: u64,
    pub payload: ClipboardPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClipboardPayload {
    Notes(Vec<Note>),
    AutomationPoints(Vec<CopiedPoint>),
    AudioEvents(Vec<AudioEvent>),
    Clips(Vec<ClipCopy>),
    Tracks(Vec<TrackCopy>),
}

/// 正規化済みオートメーション点。`value_norm` は target 非依存の 0..=1 normalized
/// (paste 先 lane の値域へ `norm_to_plain` で復元)。`time_beat` は選択群の最早を
/// 0 とした相対拍。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopiedPoint {
    pub time_beat: f64,
    pub value_norm: f32,
    pub curve: AutomationCurve,
}

/// 正規化済みクリップ。`track_offset` は選択群の最上段トラックを 0 とした相対
/// トラック index、`start_beat` は最早クリップ start を 0 とした相対拍。
/// `content` は cross-project 独立復元用に inline、`content_id` は同一プロジェクト
/// リンク共有用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipCopy {
    pub track_offset: i64,
    pub start_beat: f64,
    pub length_beats: f64,
    pub color: Option<[f32; 3]>,
    pub auto_lipsync: bool,
    /// source プロジェクトでの content_id (同一プロジェクト paste でリンク共有)。
    pub content_id: ContentId,
    /// content payload (別プロジェクト paste で独立復元)。
    pub content: ClipContent,
    /// 共有名 (`Song.clip_content_names` 由来)。
    pub name: Option<String>,
}

/// 正規化済みトラックまるごと。`track` は raw (旧 legacy field は skip_serializing で
/// 落ちる)。`order` は選択群内の相対順 (上から 0,1,2...) で、paste で相対順を保つ。
/// `contents` は track の clips / automation lanes が参照する content payload を
/// cross-project 独立復元のため同梱する。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackCopy {
    pub order: usize,
    pub track: Track,
    pub contents: Vec<ContentEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentEntry {
    pub content_id: ContentId,
    pub content: ClipContent,
    pub name: Option<String>,
}

impl ClipboardEnvelope {
    pub fn new(source_project_id: u64, payload: ClipboardPayload) -> Self {
        Self {
            magic: CLIPBOARD_MAGIC.to_string(),
            source_project_id,
            payload,
        }
    }

    /// envelope を JSON 文字列へ。OS clipboard に書く。
    pub fn to_json(&self) -> Option<String> {
        serde_json::to_string(self).ok()
    }

    /// OS clipboard から取った text を envelope として decode。magic 不一致 /
    /// decode 失敗 (= 他アプリの text) は `None` で no-op。
    pub fn from_json(text: &str) -> Option<Self> {
        let env: ClipboardEnvelope = serde_json::from_str(text).ok()?;
        if env.magic != CLIPBOARD_MAGIC {
            return None;
        }
        Some(env)
    }
}

/// 外部 clipboard 由来の `Note` 群を sanitize (pitch 0..=127 / velocity 1..=127 /
/// start・duration が非有限 or 負/0 のものは破棄)。既存 `paste_notes_from_json` の
/// 値域チェックと同一。
pub fn sanitize_notes(notes: Vec<Note>) -> Vec<Note> {
    notes
        .into_iter()
        .filter_map(|mut n| {
            if !n.start_beat.is_finite()
                || n.start_beat < 0.0
                || !n.duration_beats.is_finite()
                || n.duration_beats <= 0.0
            {
                return None;
            }
            n.pitch = n.pitch.min(127);
            n.velocity = n.velocity.clamp(1, 127);
            Some(n)
        })
        .collect()
}

/// 外部 clipboard 由来の `AudioEvent` 群を sanitize (start/length 非有限・負を破棄、
/// source frame の前後関係・各 gain/pan/pitch を clamp)。
pub fn sanitize_audio_events(events: Vec<AudioEvent>) -> Vec<AudioEvent> {
    events
        .into_iter()
        .filter_map(|mut e| {
            if !e.event_start_in_clip_beats.is_finite()
                || e.event_start_in_clip_beats < 0.0
                || !e.event_length_beats.is_finite()
                || e.event_length_beats <= 0.0
            {
                return None;
            }
            if e.source_end_frames < e.source_start_frames {
                return None;
            }
            if !e.gain_db.is_finite() {
                e.gain_db = 0.0;
            }
            e.gain_db = e.gain_db.clamp(-60.0, 24.0);
            e.pan = if e.pan.is_finite() { e.pan.clamp(-1.0, 1.0) } else { 0.0 };
            e.pitch_semitones = if e.pitch_semitones.is_finite() {
                e.pitch_semitones.clamp(-48.0, 48.0)
            } else {
                0.0
            };
            e.formant_semitones = if e.formant_semitones.is_finite() {
                e.formant_semitones.clamp(-48.0, 48.0)
            } else {
                0.0
            };
            e.fade_in_beats = if e.fade_in_beats.is_finite() {
                e.fade_in_beats.max(0.0)
            } else {
                0.0
            };
            e.fade_out_beats = if e.fade_out_beats.is_finite() {
                e.fade_out_beats.max(0.0)
            } else {
                0.0
            };
            Some(e)
        })
        .collect()
}

/// 外部 clipboard 由来の `CopiedPoint` 群を sanitize (time 非有限・負を破棄、
/// value_norm を 0..=1 に clamp)。
pub fn sanitize_points(points: Vec<CopiedPoint>) -> Vec<CopiedPoint> {
    points
        .into_iter()
        .filter_map(|mut p| {
            if !p.time_beat.is_finite() || p.time_beat < 0.0 {
                return None;
            }
            p.value_norm = if p.value_norm.is_finite() {
                p.value_norm.clamp(0.0, 1.0)
            } else {
                return None;
            };
            Some(p)
        })
        .collect()
}

/// `ClipContent` 内部の値域 sanitize。Midi notes / Audio events / Automation points を
/// 既存 sanitizer や finite チェックで掃除する。Video/Image/Text overlay content は別系統
/// (coverage invariant が load 時に再確立) なので触らない。
pub fn sanitize_content(content: &mut ClipContent) {
    match content {
        ClipContent::Midi(m) => {
            let notes = std::mem::take(&mut m.notes);
            m.notes = sanitize_notes(notes);
        }
        ClipContent::Audio(a) => {
            let events = std::mem::take(&mut a.events);
            a.events = sanitize_audio_events(events);
        }
        ClipContent::Automation(a) => {
            // automation point は plain value (target 依存で値域不明) なので clamp せず、
            // 非有限な time / value を持つ点だけ落とす。
            a.points
                .retain(|p| p.time_beat.is_finite() && p.time_beat >= 0.0 && p.value.is_finite());
        }
        _ => {}
    }
}

/// 外部 clipboard 由来の `ClipCopy` 群を sanitize。length_beats が非有限/<=0 のものは
/// 破棄、start_beat 非有限は 0、content 内部も sanitize する。
pub fn sanitize_clips(clips: Vec<ClipCopy>) -> Vec<ClipCopy> {
    clips
        .into_iter()
        .filter_map(|mut c| {
            if !c.length_beats.is_finite() || c.length_beats <= 0.0 {
                return None;
            }
            if !c.start_beat.is_finite() {
                c.start_beat = 0.0;
            }
            sanitize_content(&mut c.content);
            Some(c)
        })
        .collect()
}

/// 外部 clipboard 由来の `TrackCopy` 群を sanitize。各 clip / automation lane clip の
/// length_beats を検証 (不正は破棄)、volume/pan を clamp、content payload を sanitize する。
pub fn sanitize_tracks(mut tracks: Vec<TrackCopy>) -> Vec<TrackCopy> {
    for tc in &mut tracks {
        tc.track
            .clips
            .retain(|c| c.length_beats.is_finite() && c.length_beats > 0.0);
        for c in &mut tc.track.clips {
            if !c.start_beat.is_finite() {
                c.start_beat = 0.0;
            }
        }
        for lane in &mut tc.track.automation_lanes {
            lane.clips
                .retain(|c| c.length_beats.is_finite() && c.length_beats > 0.0);
        }
        for ce in &mut tc.contents {
            sanitize_content(&mut ce.content);
        }
        tc.track.volume = if tc.track.volume.is_finite() {
            tc.track.volume.clamp(0.0, 2.0)
        } else {
            1.0
        };
        tc.track.pan = if tc.track.pan.is_finite() {
            tc.track.pan.clamp(-1.0, 1.0)
        } else {
            0.0
        };
    }
    tracks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(start: f64, dur: f64, pitch: u8, vel: u8) -> Note {
        Note {
            start_beat: start,
            duration_beats: dur,
            pitch,
            velocity: vel,
            lyric: None,
        }
    }

    #[test]
    fn envelope_roundtrip_notes() {
        let env = ClipboardEnvelope::new(
            42,
            ClipboardPayload::Notes(vec![note(0.0, 1.0, 60, 100)]),
        );
        let json = env.to_json().unwrap();
        let back = ClipboardEnvelope::from_json(&json).unwrap();
        assert_eq!(back.source_project_id, 42);
        match back.payload {
            ClipboardPayload::Notes(n) => {
                assert_eq!(n.len(), 1);
                assert_eq!(n[0].pitch, 60);
            }
            _ => panic!("wrong payload variant"),
        }
    }

    #[test]
    fn from_json_rejects_foreign_and_garbage() {
        // 他アプリの素の text。
        assert!(ClipboardEnvelope::from_json("hello world").is_none());
        // magic 違いの JSON。
        let bad = r#"{"magic":"other.app","source_project_id":1,"payload":{"Notes":[]}}"#;
        assert!(ClipboardEnvelope::from_json(bad).is_none());
    }

    #[test]
    fn sanitize_notes_clamps_and_drops() {
        let input = vec![
            note(0.0, 1.0, 200, 200),          // pitch/vel over → clamp
            note(-1.0, 1.0, 60, 100),          // negative start → drop
            note(0.0, 0.0, 60, 100),           // zero duration → drop
            note(f64::NAN, 1.0, 60, 100),      // NaN start → drop
            note(2.0, 1.0, 60, 0),             // velocity 0 → clamp to 1
        ];
        let out = sanitize_notes(input);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].pitch, 127);
        assert_eq!(out[0].velocity, 127);
        assert_eq!(out[1].velocity, 1);
    }

    #[test]
    fn sanitize_clips_drops_bad_length_and_keeps_valid() {
        use common::model::ContentId;
        let mk = |len: f64, start: f64| ClipCopy {
            track_offset: 0,
            start_beat: start,
            length_beats: len,
            color: None,
            auto_lipsync: false,
            content_id: 0 as ContentId,
            content: ClipContent::default(),
            name: None,
        };
        let out = sanitize_clips(vec![
            mk(4.0, 0.0),          // valid
            mk(f64::NAN, 0.0),     // NaN length → drop
            mk(0.0, 0.0),          // zero length → drop
            mk(-1.0, 0.0),         // negative length → drop
            mk(2.0, f64::NAN),     // NaN start → kept, start reset to 0
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].start_beat, 0.0);
    }

    #[test]
    fn sanitize_points_clamps_norm_and_drops_bad_time() {
        let input = vec![
            CopiedPoint { time_beat: 0.0, value_norm: 2.0, curve: AutomationCurve::Linear },
            CopiedPoint { time_beat: -1.0, value_norm: 0.5, curve: AutomationCurve::Linear },
            CopiedPoint { time_beat: f64::INFINITY, value_norm: 0.5, curve: AutomationCurve::Linear },
        ];
        let out = sanitize_points(input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value_norm, 1.0);
    }
}
