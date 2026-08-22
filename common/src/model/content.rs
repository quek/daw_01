// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! Clip / ClipContent とその content variant (midi/audio/video/image/text) + source/event/Note
//!
//! arch-refactor #9 (god-file budget) で model.rs から分割。pure code movement で
//! 挙動・serialize 形式は不変。sibling 型は `use super::*` 経由で参照する。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::*;

/// A clip is a free-time container of notes positioned along the song
/// timeline. `start_beat` and `length_beats` define where the clip lives;
/// the actual notes are stored in `Song.clip_contents` keyed by
/// `content_id` so multiple clips can share the same source (REAPER
/// pooled MIDI / linked clip model).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Clip {
    /// Stable id within the owning track. `0` is "未採番" sentinel —
    /// reassigned by `Track::ensure_clip_ids` when loading. Persists across
    /// move and resize.
    #[serde(default)]
    pub id: u32,
    pub start_beat: f64,
    pub length_beats: f64,
    /// Reference into `Song.clip_contents`. `0` is the "未採番" sentinel —
    /// reassigned by `Song::ensure_clip_contents` when loading. Multiple
    /// clips with the same `content_id` share notes (linked clips).
    #[serde(default)]
    pub content_id: ContentId,
    /// v32 (`docs/plan_clip_content_window.md`): clip の左端が content の
    /// **どの拍に当たるか** (content-local 拍)。REAPER item の `SOFFS` /
    /// Ableton の clip start marker と同じ量。
    ///
    /// clip は共有 content への「窓」であり、見せる範囲は
    /// `[content_offset_beats, content_offset_beats + length_beats)`。左端 trim は
    /// content を書き換えず **この値を進める**ので、`content_id` を共有する linked
    /// clip の開始・終了は完全に独立する (r.md #44)。左端を外へ伸ばすと負になり得る
    /// (= 先頭に空白が付く、content 側は無傷)。
    ///
    /// content-local 拍 ↔ song-absolute 拍の換算は [`Clip::content_origin_beat`]
    /// / [`Clip::content_to_song_beat`] / [`Clip::song_to_content_beat`] だけを通すこと
    /// (`clip.start_beat + <content-local>` を直に書くと offset を落とす)。
    /// v31 以前は 0 に forward-migrate (= 従来の挙動と同一)。
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub content_offset_beats: f64,
    /// v18 (`docs/plan_track_clip_color.md`): per-clip color override
    /// (RGB, opaque). `None` ⇒ inherit the owning track's effective color
    /// (the default; resetting to `None` is the Ableton-style "match track
    /// color"). `Some(rgb)` ⇒ explicit per-clip override. Read only by
    /// `daw_gui` (arrangement clip fill). v17 files forward-migrate to
    /// `None` (= inherit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[f32; 3]>,
    /// v21 (`docs/plan_pakupaku.md`): 口パク自動生成 clip の印。`true` ⇒ この
    /// clip は vocal の notes+歌詞+`mouth_map` から導出された派生物で、再生成時に
    /// 口 track 上の `auto_lipsync == true` clip は全削除 → 再構築される
    /// (手編集は保持しない)。ユーザが手で置いた clip は `false` のまま温存。
    /// v20 files forward-migrate to `false`。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto_lipsync: bool,
    /// この `auto_lipsync` clip を生成した **口パク配置ルールの世代**
    /// ([`crate::lipsync::PLACEMENT_GEN`])。`0` = 世代を持たない旧 file。
    ///
    /// r.md #39: 口パク event は project に永続化される派生データで、再生成は
    /// 入力 (notes / text / bpm / mouth_map …) の fingerprint が変わったときだけ走る。
    /// 配置ルール自体を変えると、入力が同じままなので **開いても再生成されず旧
    /// タイミングが残る** (talk が音声より ~100ms 遅れたまま)。合成 WAV 側が
    /// `voicevox_cache::CACHE_SCHEMA_VERSION` で世代を切っているのと対になる仕組みで、
    /// load 時に古い世代の clip を見つけたら一度だけ再生成する。
    /// `auto_lipsync == false` の手置き clip では常に 0 (意味を持たない)。
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub lipsync_gen: u32,
    /// clip 全体のミュート (= MIDI / audio / video / image / 字幕 / 歌唱
    /// すべての content type 共通の clip-level mute の SSoT)。`true` で再生・書き出しから
    /// この clip を除外し、GUI は dim + 斜線ハッチで「ミュート中」を表示する。`q`
    /// ショートカット (選択 clip / カーソル直下 clip を toggle) と各 content inspector の
    /// "Mute" トグルがここを唯一の source として読み書きする。`Track.muted` とは独立で、
    /// 再生時は `track.muted || clip.muted` で合成される。v26 以前の per-event mute は
    /// `project::migrate_per_event_mute_to_clip_mute` で本フラグへ畳み込まれる。v26 以前は
    /// `false` に forward-migrate。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub muted: bool,
    /// この clip の VOICEVOX 歌唱声 = `/frame_synthesis` の speaker
    /// (= `/singers` の歌唱 style id)。clip 単位で独立・焼き込み (前の clip の
    /// 声を後で変えても後続に波及しない)。`0` = 未採番 (= 合成時に
    /// `voicevox::DEFAULT_SINGER_ID` へフォールバック)。vocal track 上の MIDI
    /// clip でのみ意味を持つ (他 content type では未使用)。旧プロジェクトは
    /// `project::load` の migration で旧トラック声を焼き込む。
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub speaker_id: u32,
    /// 表示用キャラ名 (例: "中国うさぎ")。`/singers` 未取得でも
    /// inspector が現在の声を出せるよう焼き込む。空なら一覧取得後に
    /// `speaker_id` から逆引きして埋める。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub singer_name: String,
    /// 表示用スタイル名 (例: "ノーマル" / "へろへろ")。同上。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub style_name: String,
    /// (talk) VOICEVOX 読み上げの全体スケール (話速/音高/抑揚/音量)。
    /// `ClipContent::Text` clip が VOICEVOX デバイス付きトラックに居るときだけ意味を
    /// 持つ (`docs/plan_voicevox_talk.md`)。`None` = 全既定。声 (talk style) は
    /// `Clip::speaker_id` を流用する。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub talk: Option<TalkParams>,
}

impl Clip {
    /// content-local 拍 0 が置かれる song-absolute 拍
    /// (= `start_beat - content_offset_beats`)。
    ///
    /// **content ↔ song の換算はここを唯一の口にすること。**
    /// `clip.start_beat + <content-local 拍>` を直に書くと、左端 trim した clip で
    /// `content_offset_beats` を落として位置がずれる (r.md #44)。
    #[must_use]
    #[inline]
    pub fn content_origin_beat(&self) -> f64 {
        self.start_beat - self.content_offset_beats
    }

    /// content-local 拍 (note / event の `start_beat` 等) → song-absolute 拍。
    #[must_use]
    #[inline]
    pub fn content_to_song_beat(&self, content_beat: f64) -> f64 {
        self.content_origin_beat() + content_beat
    }

    /// song-absolute 拍 → content-local 拍 (clip 内容の座標系)。
    #[must_use]
    #[inline]
    pub fn song_to_content_beat(&self, song_beat: f64) -> f64 {
        song_beat - self.content_origin_beat()
    }

    /// この clip が見せる content の窓 `[offset, offset + length)` (content-local 拍)。
    /// 再生・描画はこの窓の外の note / event を出さない。
    #[must_use]
    #[inline]
    pub fn content_window(&self) -> (f64, f64) {
        (
            self.content_offset_beats,
            self.content_offset_beats + self.length_beats,
        )
    }

    /// この clip の song-absolute な占有範囲 `[start, start + length)`。
    #[must_use]
    #[inline]
    pub fn song_window(&self) -> (f64, f64) {
        (self.start_beat, self.start_beat + self.length_beats)
    }
}

/// Shared content referenced by one or more `Clip`s via
/// `Clip.content_id`. Stored on `Song.clip_contents`. Carries either
/// MIDI notes (`Midi(MidiContent)`) or audio events
/// (`Audio(AudioContent)`) depending on the variant.
///
/// `#[serde(untagged)]` lets v6 `.daw` files (which serialised
/// `ClipContent` as a flat struct `{ "notes": [...] }`) deserialize
/// directly into `Midi(MidiContent { notes })` — `MidiContent.notes`
/// vs `AudioContent.events` are disjoint field sets so the dispatch
/// is unambiguous. bincode (used over IPC) ignores the serde-untagged
/// attribute and encodes the variant index as usual.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
// v30 (arch-refactor §10): 明示 `type` タグで variant を判別する (旧 `#[serde(untagged)]` は
// content 型数の 2 乗で silent-misparse リスクがあり、空 content が `{}` で型消失していた)。
// 旧 untagged ファイル (v<30) は `project.rs` の `migrate_clip_content_add_tag` が load 時に
// `type` を注入して変換する。bincode (IPC) は serde tag を無視し variant index を使うので
// wire 互換は不変。
#[serde(tag = "type")]
pub enum ClipContent {
    Midi(MidiContent),
    Audio(AudioContent),
    Automation(AutomationContent),
    /// v12 (`docs/plan_video.md` §2.2): video clip payload. Untagged
    /// disambiguation from `Audio` works because `VideoEvent` requires
    /// `source_start_micros` while `AudioEvent` requires
    /// `source_start_frames` — neither field has a serde default, so
    /// a JSON shaped for one variant fails inner deserialization for
    /// the other and serde falls through to the matching variant.
    Video(VideoContent),
    /// v13 (`docs/plan_image_overlay.md` §2.2): image overlay (PiP)
    /// clip payload. Untagged disambiguation from `Audio` / `Video`
    /// works because `ImageEvent` requires `opacity` while neither
    /// `AudioEvent` nor `VideoEvent` has that field, and all three
    /// content structs use `deny_unknown_fields` so a JSON object
    /// carrying `opacity` fails the Audio / Video inner deserialize
    /// and falls through to `Image`.
    Image(ImageContent),
    /// v16 (`docs/plan_text_overlay.md` §2.2): text overlay (title /
    /// 字幕 / credits) clip payload。 untagged disambiguation:
    /// `TextEvent.text: String` + `font_family: String` を持ち、 他
    /// variant の inner struct には `String` の required field 無し
    /// (Image は `opacity` 数値、 Video は `source_start_micros`、
    /// Audio は `source_start_frames`、 Midi は `notes`、 Automation
    /// は `points`)。 `deny_unknown_fields` で意図しない fallthrough
    /// を防止。
    Text(TextContent),
}

/// r.md #38: 1 event 分の fade 情報を content 種別に依らず表す値型。
///
/// audio / video / image / text の event が共通で持つ「clip 内のどこに、 どれだけの
/// 長さで居て、 その両端にどんな fade が掛かっているか」。 アレンジ画面の fade 描画・
/// hit-test・drag と、 fade の編集 setter がこの型 1 つを見る。
///
/// **fade は clip 長ではなく `len_beats` (= event 長) に対して掛かる**。 音側
/// (`daw_audio::audio_clip_renderer`) / 映像 (`video_playback`) / 画像
/// (`image_compose`) / 字幕 (`text_compose`) が全部 event 長基準で適用しているので、
/// 描画も clamp もここに揃える (r.md #38 以前は描画と clamp だけが clip 長基準で、
/// clip より短い event では絵・音・編集可能範囲が三者三様にずれていた)。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EventFade {
    /// clip 先頭からの event 開始位置 (拍)。
    pub start_in_clip_beats: f64,
    /// event の長さ (拍)。 fade 長の上限でもある。
    pub len_beats: f64,
    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
    pub fade_in_curve: FadeCurve,
    pub fade_out_curve: FadeCurve,
}

impl Default for ClipContent {
    fn default() -> Self {
        ClipContent::Midi(MidiContent::default())
    }
}

impl ClipContent {
    /// overlay content (image / video / text) の末尾 (`event_start`
    /// 最大) event を、 その end が `clip_length_beats` に届くよう extend する
    /// (extend-only)。 単一 event なら clip 全長を覆い「clip 長 = 表示長」を保証。
    /// 縮めはしない (linked clip / `event > clip` の無害な不整合や多 event の
    /// 前方タイルは温存)。 Audio / Midi / Automation は時間軸 gate を持たないので
    /// no-op。
    pub fn ensure_event_covers_clip(&mut self, clip_length_beats: f64) {
        macro_rules! extend_last {
            ($events:expr) => {{
                if let Some(ev) = $events.iter_mut().max_by(|a, b| {
                    a.event_start_in_clip_beats
                        .total_cmp(&b.event_start_in_clip_beats)
                }) {
                    let needed = (clip_length_beats - ev.event_start_in_clip_beats).max(0.0);
                    if ev.event_length_beats < needed {
                        ev.event_length_beats = needed;
                    }
                }
            }};
        }
        match self {
            ClipContent::Image(c) => extend_last!(c.events),
            ClipContent::Video(c) => extend_last!(c.events),
            ClipContent::Text(c) => extend_last!(c.events),
            _ => {}
        }
    }

    /// v29: 要素 (note / audio event / automation point) の安定 id を採番
    /// する。 sentinel (0) のみ上書き、 既存非 0 id は counter を bump する
    /// だけ。 Video / Image / Text の event は単一 event 中心の運用で
    /// 選択集合を持たないため対象外。
    pub fn ensure_element_ids(&mut self) {
        fn alloc(id: &mut u32, next: &mut u32) {
            if *id == 0 {
                let new_id = (*next).max(1);
                *next = new_id + 1;
                *id = new_id;
            } else if *id >= *next {
                *next = *id + 1;
            }
        }
        match self {
            ClipContent::Midi(m) => {
                for n in &mut m.notes {
                    alloc(&mut n.id, &mut m.next_note_id);
                }
                if m.next_note_id == 0 {
                    m.next_note_id = 1;
                }
            }
            ClipContent::Audio(a) => {
                for e in &mut a.events {
                    alloc(&mut e.id, &mut a.next_event_id);
                }
                if a.next_event_id == 0 {
                    a.next_event_id = 1;
                }
            }
            ClipContent::Automation(a) => {
                for p in &mut a.points {
                    alloc(&mut p.id, &mut a.next_point_id);
                }
                if a.next_point_id == 0 {
                    a.next_point_id = 1;
                }
            }
            ClipContent::Video(_) | ClipContent::Image(_) | ClipContent::Text(_) => {}
        }
    }

    /// Borrow the notes slice if this is a `Midi` variant. `Audio` /
    /// `Automation` / `Video` variants return `None`. Used by
    /// `Song::clip_notes` and other helpers that previously read
    /// `clip.notes` directly.
    pub fn notes(&self) -> Option<&[Note]> {
        match self {
            ClipContent::Midi(m) => Some(m.notes.as_slice()),
            ClipContent::Audio(_)
            | ClipContent::Automation(_)
            | ClipContent::Video(_)
            | ClipContent::Image(_)
            | ClipContent::Text(_) => None,
        }
    }

    /// Mutably borrow the notes vec for a `Midi` variant. Other
    /// variants return `None`.
    pub fn notes_mut(&mut self) -> Option<&mut Vec<Note>> {
        match self {
            ClipContent::Midi(m) => Some(&mut m.notes),
            ClipContent::Audio(_)
            | ClipContent::Automation(_)
            | ClipContent::Video(_)
            | ClipContent::Image(_)
            | ClipContent::Text(_) => None,
        }
    }

    /// r.md #38: clip 内の各 event の fade 情報を **content 種別に依らず** 列挙する。
    ///
    /// `AudioEvent` / `VideoEvent` / `ImageEvent` / `TextEvent` は
    /// `event_start_in_clip_beats` / `event_length_beats` / `fade_in_beats` /
    /// `fade_out_beats` / `fade_in_curve` / `fade_out_curve` を同じ意味で持ち、
    /// 適用側も全部 [`crate::audio_render::fade_curve_at`] を通る。 よって
    /// 「fade をどう描き、 どう掴み、 どう編集するか」 は content に依存しない。
    /// アレンジ画面の描画 / hit-test / drag はこの 1 本を SSoT にする
    /// (種別ごとに 4 実装を持たない = DRY)。
    ///
    /// `Midi` / `Automation` は fade を持たないので空 Vec。
    #[must_use]
    pub fn event_fades(&self) -> Vec<EventFade> {
        macro_rules! collect {
            ($events:expr) => {
                $events
                    .iter()
                    .map(|e| EventFade {
                        start_in_clip_beats: e.event_start_in_clip_beats,
                        len_beats: e.event_length_beats,
                        fade_in_beats: e.fade_in_beats,
                        fade_out_beats: e.fade_out_beats,
                        fade_in_curve: e.fade_in_curve,
                        fade_out_curve: e.fade_out_curve,
                    })
                    .collect()
            };
        }
        match self {
            ClipContent::Audio(c) => collect!(c.events),
            ClipContent::Video(c) => collect!(c.events),
            ClipContent::Image(c) => collect!(c.events),
            ClipContent::Text(c) => collect!(c.events),
            ClipContent::Midi(_) | ClipContent::Automation(_) => Vec::new(),
        }
    }

    /// r.md #38: `index` 番目の event の fade フィールドを content 種別に依らず
    /// 書き換える。 `f` には現在値を渡し、 戻り値をそのまま書き戻す
    /// (clamp は caller の責務 = [`EventFade::len_beats`] を上限にする)。
    ///
    /// event が存在しない / fade を持たない content なら `false`。
    pub fn set_event_fade(
        &mut self,
        index: usize,
        f: impl FnOnce(EventFade) -> EventFade,
    ) -> bool {
        macro_rules! apply {
            ($events:expr) => {{
                let Some(e) = $events.get_mut(index) else {
                    return false;
                };
                let next = f(EventFade {
                    start_in_clip_beats: e.event_start_in_clip_beats,
                    len_beats: e.event_length_beats,
                    fade_in_beats: e.fade_in_beats,
                    fade_out_beats: e.fade_out_beats,
                    fade_in_curve: e.fade_in_curve,
                    fade_out_curve: e.fade_out_curve,
                });
                e.fade_in_beats = next.fade_in_beats;
                e.fade_out_beats = next.fade_out_beats;
                e.fade_in_curve = next.fade_in_curve;
                e.fade_out_curve = next.fade_out_curve;
                true
            }};
        }
        match self {
            ClipContent::Audio(c) => apply!(c.events),
            ClipContent::Video(c) => apply!(c.events),
            ClipContent::Image(c) => apply!(c.events),
            ClipContent::Text(c) => apply!(c.events),
            ClipContent::Midi(_) | ClipContent::Automation(_) => false,
        }
    }

    /// Borrow the audio events slice if this is an `Audio` variant.
    pub fn audio_events(&self) -> Option<&[AudioEvent]> {
        match self {
            ClipContent::Audio(a) => Some(a.events.as_slice()),
            ClipContent::Midi(_)
            | ClipContent::Automation(_)
            | ClipContent::Video(_)
            | ClipContent::Image(_)
            | ClipContent::Text(_) => None,
        }
    }

    /// Mutably borrow the events vec for an `Audio` variant.
    pub fn audio_events_mut(&mut self) -> Option<&mut Vec<AudioEvent>> {
        match self {
            ClipContent::Audio(a) => Some(&mut a.events),
            ClipContent::Midi(_)
            | ClipContent::Automation(_)
            | ClipContent::Video(_)
            | ClipContent::Image(_)
            | ClipContent::Text(_) => None,
        }
    }

    /// Borrow the automation point slice if this is an `Automation`
    /// variant. Other variants return `None`.
    pub fn automation_points(&self) -> Option<&[AutomationPoint]> {
        match self {
            ClipContent::Automation(a) => Some(a.points.as_slice()),
            ClipContent::Midi(_)
            | ClipContent::Audio(_)
            | ClipContent::Video(_)
            | ClipContent::Image(_)
            | ClipContent::Text(_) => None,
        }
    }

    /// Mutably borrow the automation point vec for an `Automation`
    /// variant.
    pub fn automation_points_mut(&mut self) -> Option<&mut Vec<AutomationPoint>> {
        match self {
            ClipContent::Automation(a) => Some(&mut a.points),
            ClipContent::Midi(_)
            | ClipContent::Audio(_)
            | ClipContent::Video(_)
            | ClipContent::Image(_)
            | ClipContent::Text(_) => None,
        }
    }

    /// Borrow the video events slice if this is a `Video` variant. v12
    /// (`docs/plan_video.md` §2.2).
    pub fn video_events(&self) -> Option<&[VideoEvent]> {
        match self {
            ClipContent::Video(v) => Some(v.events.as_slice()),
            ClipContent::Midi(_)
            | ClipContent::Audio(_)
            | ClipContent::Automation(_)
            | ClipContent::Image(_)
            | ClipContent::Text(_) => None,
        }
    }

    /// Mutably borrow the events vec for a `Video` variant. v12.
    pub fn video_events_mut(&mut self) -> Option<&mut Vec<VideoEvent>> {
        match self {
            ClipContent::Video(v) => Some(&mut v.events),
            ClipContent::Midi(_)
            | ClipContent::Audio(_)
            | ClipContent::Automation(_)
            | ClipContent::Image(_)
            | ClipContent::Text(_) => None,
        }
    }

    /// Borrow the image events slice if this is an `Image` variant. v13
    /// (`docs/plan_image_overlay.md` §2.2).
    pub fn image_events(&self) -> Option<&[ImageEvent]> {
        match self {
            ClipContent::Image(i) => Some(i.events.as_slice()),
            ClipContent::Midi(_)
            | ClipContent::Audio(_)
            | ClipContent::Automation(_)
            | ClipContent::Video(_)
            | ClipContent::Text(_) => None,
        }
    }

    /// Mutably borrow the events vec for an `Image` variant. v13.
    pub fn image_events_mut(&mut self) -> Option<&mut Vec<ImageEvent>> {
        match self {
            ClipContent::Image(i) => Some(&mut i.events),
            ClipContent::Midi(_)
            | ClipContent::Audio(_)
            | ClipContent::Automation(_)
            | ClipContent::Video(_)
            | ClipContent::Text(_) => None,
        }
    }

    /// Borrow the text events slice if this is a `Text` variant.
    /// v16 (`docs/plan_text_overlay.md` §2.2).
    pub fn text_events(&self) -> Option<&[TextEvent]> {
        match self {
            ClipContent::Text(t) => Some(t.events.as_slice()),
            ClipContent::Midi(_)
            | ClipContent::Audio(_)
            | ClipContent::Automation(_)
            | ClipContent::Video(_)
            | ClipContent::Image(_) => None,
        }
    }

    /// Mutably borrow the events vec for a `Text` variant. v16.
    pub fn text_events_mut(&mut self) -> Option<&mut Vec<TextEvent>> {
        match self {
            ClipContent::Text(t) => Some(&mut t.events),
            ClipContent::Midi(_)
            | ClipContent::Audio(_)
            | ClipContent::Automation(_)
            | ClipContent::Video(_)
            | ClipContent::Image(_) => None,
        }
    }
}

/// MIDI clip content — a bag of notes positioned in clip-local beats.
///
/// `deny_unknown_fields` is required: `ClipContent` is `#[serde(untagged)]`
/// so the deserializer tries each variant in order until one succeeds.
/// Without `deny_unknown_fields`, a JSON object with only an `events`
/// or `points` key would happily deserialize into `MidiContent { notes:
/// vec![] }` (because every field has a default), making it impossible
/// to disambiguate variants. With `deny_unknown_fields`, only the
/// matching variant succeeds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct MidiContent {
    /// Notes are in arbitrary order — readers that care about time
    /// order must sort by `Note::start_beat`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Note>,
    /// v29: `Note::id` の per-content allocator。`0` は sentinel、`1` から。
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub next_note_id: u32,
}

impl MidiContent {
    /// 新規 note 用の安定 id を採番する。
    pub fn alloc_note_id(&mut self) -> u32 {
        let id = self.next_note_id.max(1);
        self.next_note_id = id.saturating_add(1);
        id
    }
}

/// Audio clip content — an ordered list of audio events that play
/// within the clip. Bitwig "Clip ⊃ Audio Events" hierarchy
/// ([docs/plan_audio_clip.md](../../docs/plan_audio_clip.md)). Events
/// can overlap (mixed) or sit side by side; clip-internal layout is
/// defined by each event's `event_start_in_clip_beats` /
/// `event_length_beats`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct AudioContent {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<AudioEvent>,
    /// v29: `AudioEvent::id` の per-content allocator。`0` は sentinel。
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub next_event_id: u32,
}

impl AudioContent {
    /// 新規 audio event 用の安定 id を採番する。
    pub fn alloc_event_id(&mut self) -> u32 {
        let id = self.next_event_id.max(1);
        self.next_event_id = id.saturating_add(1);
        id
    }
}

/// Stable id for an entry in `Song.audio_sources`. `0` is the "未採番"
/// sentinel — `Song::ensure_audio_source_ids` reassigns it on load.
pub type AudioSourceId = u32;

/// Reference to an imported audio file (WAV / FLAC / generated). Path
/// resolution is governed by `AudioSourcePath`. Sample buffers are NOT
/// stored on the model — each process (GUI / audio engine) decodes the
/// file independently from the path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct AudioSource {
    pub path: AudioSourcePath,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    /// BPM detected from WAV cue chunks / ACID metadata. Used by
    /// `StretchMode::Repitch` / `Stretch` to translate to project BPM.
    /// `None` when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_bpm: Option<f32>,
    /// MIDI key the loop was recorded at — relevant for sample-based
    /// instruments. `None` when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_key: Option<u8>,
}

/// Path resolution strategy for an `AudioSource`. Normal imports
/// produce `ProjectRelative` after copying the file into
/// `<project_dir>/samples/<basename>_<hash8>.<ext>`. `Absolute` is
/// reserved for the unsaved-project import-cache fallback (and a
/// future "link to external sample" mode). `Generated` is used by
/// VOICEVOX and other in-memory synthesised audio with no file on
/// disk; the `id` is the same one carried by
/// `MainToChild::SetGeneratedAudio`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum AudioSourcePath {
    ProjectRelative(PathBuf),
    Absolute(PathBuf),
    Generated { id: u64 },
}

/// Stable id for an entry in `Song.video_sources`. `0` is the "未採番"
/// sentinel — `Song::ensure_video_source_ids` reassigns it on load.
/// v12 (`docs/plan_video.md` §2.3).
pub type VideoSourceId = u32;

/// Reference to an imported video file (mp4 / mov / mkv / webm). Path
/// resolution mirrors `AudioSource` — normal imports copy the file into
/// `<project_dir>/samples/<basename>_<hash8>.<ext>` and store
/// `ProjectRelative`. The decoded frames are NOT stored on the model:
/// each frame is decoded on demand by `daw_gui`'s video worker thread
/// from the path (= same SSoT pattern as `AudioSource`). The audio
/// stream is extracted to a sibling `.wav` at import time and exposed
/// via `audio_source_id` for the auto-generated pair audio track. v12
/// (`docs/plan_video.md` §2.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct VideoSource {
    pub path: VideoSourcePath,
    /// Native pixel width / height as reported by the decoder. Project
    /// preview scales these to `Song.video_resolution`.
    pub width: u32,
    pub height: u32,
    /// Frames per second as reported by the decoder (= source FPS, not
    /// project FPS)。 **メタデータ専用** (import 時に記録、 情報表示・診断用)。
    /// フレーム選択は frame index でなく source の microsecond timestamp
    /// (`VideoEvent.source_*_micros` → MF / libav の **time-based seek**) で行い、
    /// decoder が PTS で正しいフレームを返すため、 VFR ソースでもフレームタイミングは
    /// 正しい (= この nominal FPS には依存しない)。 出力 export の刻みは別途
    /// `Song.video_framerate` (constant output FPS) を使う。
    /// (r.md #8 A7: コードを辿ると frame timing は時間ベースで VFR-correct。 旧
    /// 「MVP assumes CFR」 コメントは誤解だったので訂正。)
    pub framerate: f32,
    /// Total duration in microseconds (= libav `AV_TIME_BASE` units).
    pub duration_micros: u64,
    /// FFmpeg codec name (`"h264"` / `"hevc"` / `"vp9"` / `"av1"` etc.).
    /// Free-form string; consumers use it only for display and
    /// diagnostics.
    pub codec: String,
    /// AudioSource holding the audio stream extracted from the video at
    /// import time. `None` when the source video had no audio stream or
    /// extraction was skipped. The audio is NOT played back through this
    /// link — `daw_audio` plays from `AudioEvent.source_id` in the
    /// auto-generated pair audio track. This back-reference exists for
    /// diagnostics and future "re-extract audio" operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_source_id: Option<AudioSourceId>,
}

/// Path resolution strategy for a `VideoSource`. Mirrors
/// `AudioSourcePath` minus the `Generated` variant — video frames are
/// always backed by an on-disk file. v12 (`docs/plan_video.md` §2.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum VideoSourcePath {
    ProjectRelative(PathBuf),
    Absolute(PathBuf),
}

/// One playable audio event inside an `AudioContent`. Maps a slice of
/// an `AudioSource` (`source_*_frames`) to a position in the clip
/// (`event_start_in_clip_beats` + `event_length_beats`) and applies
/// per-event playback parameters (gain / pan / pitch / fade / stretch).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct AudioEvent {
    /// v29: content 内で安定な event id (`AudioContent.next_event_id` 採番、
    /// `0` = 未採番 sentinel)。選択・undo 後の選択復元は positional index
    /// でなくこの id でアドレスする。
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub id: u32,
    pub source_id: AudioSourceId,
    pub event_start_in_clip_beats: f64,
    pub event_length_beats: f64,
    pub source_start_frames: u64,
    pub source_end_frames: u64,

    pub gain_db: f32,
    pub pan: f32,
    /// 移調量 (半音)。有効範囲は [`PITCH_SEMITONES_LIMIT`]。
    pub pitch_semitones: f32,
    /// **スペクトル包絡 (フォルマント) の移調量 (半音)** — 「声質」を音程と
    /// 独立に動かすつまみ。有効範囲は [`FORMANT_SEMITONES_LIMIT`]。r.md #40。
    ///
    /// mode ごとの意味 (`0` の意味が mode で違うのが肝):
    ///
    /// | mode | `0` の意味 | `F` の意味 |
    /// |---|---|---|
    /// | [`StretchMode::Stretch`] | **原音のフォルマントを保持** (= 移調しても声質が変わらない) | 原音の包絡を `F` 半音動かす |
    /// | `Raw` / `Repitch` / `Slice` | 完全バイパス (= テープ挙動そのまま、出力は 1 サンプルも変わらない) | テープ結果の包絡を `F` 半音動かす |
    ///
    /// Stretch だけ「0 = 保持」なのは、スペクトル方式の定義が音程と包絡の分離
    /// だから (Ableton Complex Pro の Formants=100%、Bitwig Elastique Pro、
    /// Cubase VariAudio、Melodyne と同じ流儀)。テープ系で「0 = 未処理」なのは、
    /// Repitch の存在意義がテープ挙動そのものだから (Ableton Re-Pitch に
    /// フォルマント制御が無いのと同じ理由)。
    ///
    /// **時間軸には一切効かない** ので、波形描画
    /// (`common::audio_render::audible_source_span`) には現れない。
    pub formant_semitones: f32,

    pub stretch_mode: StretchMode,

    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
    pub fade_in_curve: FadeCurve,
    pub fade_out_curve: FadeCurve,

    pub reversed: bool,
    pub muted: bool,

    /// Auto-detected transient frame positions (`source_start_frames` 起点、
    /// `StretchMode::Slice` の slice trigger 位置)。 Slice 切替時に daw_gui が
    /// `common::onset::detect_onsets` で検出して埋める (r.md #8 B1)。 空 = 未検出で
    /// source 全体が 1 slice (= Raw 等価)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub onsets: Vec<u64>,
    /// User-placed beat markers for `StretchMode::Stretch`. Phase 3+;
    /// empty in Phase 1.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub beat_markers: Vec<BeatMarker>,
}

impl Default for AudioEvent {
    fn default() -> Self {
        Self {
            id: 0,
            source_id: 0,
            event_start_in_clip_beats: 0.0,
            event_length_beats: 0.0,
            source_start_frames: 0,
            source_end_frames: 0,
            gain_db: 0.0,
            pan: 0.0,
            pitch_semitones: 0.0,
            formant_semitones: 0.0,
            // 新規 audio clip は既定で tempo 追従 (= MIDI clip と同じく
            // project bpm 変更に拍を固定して伸縮、 ピッチ保持の granular)。
            // ワンショット等で追従させたくない場合は inspector の
            // stretch-mode セレクタで Raw に切り替える (= Bitwig Raw /
            // Ableton Warp-off 相当)。 enum `StretchMode::#[default]` は
            // Raw のまま (= このフィールドの deserialize default には
            // 使われず、 保存済みプロジェクトの mode は格納値を維持)。
            stretch_mode: StretchMode::Stretch,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
            reversed: false,
            muted: false,
            onsets: Vec::new(),
            beat_markers: Vec::new(),
        }
    }
}

/// [`AudioEvent::pitch_semitones`] の絶対値上限 (半音)。Bitwig の audio event
/// pitch range (±96) に合わせる。
///
/// **setter / clipboard sanitize / inspector の range はここだけを見ること**。
/// r.md #40 以前は setter が ±96、貼り付け sanitize が ±48 と割れていて、
/// コピー&ペーストするだけで値が静かに切られていた。
pub const PITCH_SEMITONES_LIMIT: f32 = 96.0;

/// [`AudioEvent::formant_semitones`] の絶対値上限 (半音)。±4 oct は Melodyne の
/// フォルマントツール (cent 単位で数千 cent) の実用域を包含する。
pub const FORMANT_SEMITONES_LIMIT: f32 = 48.0;

/// 有限値かつ `±limit` に収める。非有限 (NaN / Inf) は `0.0`。
/// pitch / formant の clamp を書く場所を 1 つにするための helper。
#[must_use]
pub fn clamp_semitones(value: f32, limit: f32) -> f32 {
    if value.is_finite() {
        value.clamp(-limit, limit)
    } else {
        0.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum StretchMode {
    #[default]
    Raw,
    Repitch,
    Stretch,
    Slice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, Encode, Decode)]
pub enum FadeCurve {
    #[default]
    Linear,
    Exponential,
    SCurve,
}

impl FadeCurve {
    /// Vertical drag で次の curve に進める (`Linear → Exponential → SCurve → Linear`)。
    /// r.md #38 で arrangement widget の mirror enum を撤去し、 順送りの SSoT をここに移した。
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            FadeCurve::Linear => FadeCurve::Exponential,
            FadeCurve::Exponential => FadeCurve::SCurve,
            FadeCurve::SCurve => FadeCurve::Linear,
        }
    }

    /// ghost label / debug 表示用の英語名 (Bitwig / Reaper と整合)。
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            FadeCurve::Linear => "Linear",
            FadeCurve::Exponential => "Exponential",
            FadeCurve::SCurve => "SCurve",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct BeatMarker {
    /// Position inside the source file (sample frames).
    pub source_frame: u64,
    /// Position inside the event (event-local beats) where the source
    /// frame is locked to land.
    pub locked_beat: f64,
}

// =============================================================================
// Video (v12, docs/plan_video.md §2.2)
// =============================================================================

/// Video clip content — an ordered list of video events that play within
/// the clip. Mirrors the Bitwig-style `Clip ⊃ Event` hierarchy used by
/// `AudioContent`; each `VideoEvent` maps a slice of a `VideoSource` to a
/// position in the clip. Events on the same clip can overlap (= the
/// preview composite alpha-blends them per #043 wgpu pipeline) or sit
/// side by side (= split clip).
///
/// `#[serde(deny_unknown_fields)]` is required so the `#[serde(untagged)]`
/// dispatch on `ClipContent` distinguishes `Audio` vs `Video` when the
/// outer field name (`events`) collides — disjoint inner required fields
/// (`source_start_frames` vs `source_start_micros`) handle the actual
/// disambiguation, but denying unknowns here prevents a future field
/// addition from accidentally widening the match.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct VideoContent {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<VideoEvent>,
}

/// One playable video event inside a `VideoContent`. Maps a slice of a
/// `VideoSource` (`source_*_micros`) to a position in the clip
/// (`event_start_in_clip_beats` + `event_length_beats`) and applies
/// per-event playback parameters (mute / fade).
///
/// **Required-field invariant**: `source_start_micros` MUST stay as a
/// required (no `#[serde(default)]`) field of distinct name from any
/// required field of `AudioEvent`. The untagged `ClipContent` dispatch
/// relies on this to disambiguate Video vs Audio JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct VideoEvent {
    pub source_id: VideoSourceId,
    /// Clip-local beat at which the event starts.
    pub event_start_in_clip_beats: f64,
    /// Duration of the event in clip-local beats. The source range
    /// (`source_end_micros - source_start_micros`) maps onto this
    /// duration at project tempo; tempo changes are not interpolated
    /// for MVP (= CFR assumption).
    pub event_length_beats: f64,
    /// Source-relative start position in microseconds (libav
    /// `AV_TIME_BASE` units). Disjoint from `AudioEvent`'s
    /// `source_start_frames` so untagged `ClipContent` can dispatch
    /// unambiguously.
    pub source_start_micros: u64,
    pub source_end_micros: u64,

    /// When `true` the event renders as a solid clear color (= black
    /// frame, no `VideoSource` decode). Useful for "blank" placeholders
    /// without removing the event.
    pub muted: bool,
    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
    pub fade_in_curve: FadeCurve,
    pub fade_out_curve: FadeCurve,
}

impl Default for VideoEvent {
    fn default() -> Self {
        Self {
            source_id: 0,
            event_start_in_clip_beats: 0.0,
            event_length_beats: 0.0,
            source_start_micros: 0,
            source_end_micros: 0,
            muted: false,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        }
    }
}

// =============================================================================
// Image overlay / PiP (v13, docs/plan_image_overlay.md §2)
// =============================================================================

/// Stable id for an imported image source. `0` is the "未採番" sentinel.
/// v13 (`docs/plan_image_overlay.md` §2.1).
pub type ImageSourceId = u32;

/// Reference to an imported image file (PNG / JPEG / WebP / static
/// BMP / TIFF / TGA / GIF-static). Path resolution mirrors
/// `VideoSource` — normal imports copy the file into
/// `<project_dir>/images/<basename>_<hash8>.<ext>` and store
/// `ProjectRelative`. The decoded BGRA8 buffer is NOT stored on the
/// model: each image is decoded once at import time (= the `image`
/// crate returns a `RgbaImage`, daw_gui reorders to BGRA8 + uploads
/// to a GPU `TextureHandle` cached on the preview window for the
/// lifetime of the project). v13 (`docs/plan_image_overlay.md` §2.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ImageSource {
    pub path: ImageSourcePath,
    /// import 元ファイルの元の名前 (拡張子込み、 sanitize / content-hash
    /// 前)。 inspector / 口パク mapping ドロップダウン等、 image source を
    /// 直接列挙する UI の表示用 SSoT。 on-disk `path` は content addressing
    /// のため `<sanitized_stem>_<hash8>.<ext>` に変形され、 日本語名は
    /// `_` に潰れて区別不能になるので、 元名はここに別途保持する。 v21
    /// 以前の `.daw` は未保持なので `#[serde(default)]` で空文字になり、
    /// consumer は空なら `path.file_name()` に fallback する。
    #[serde(default)]
    pub name: String,
    /// Native pixel width / height as reported by the decoder. PiP
    /// rect (`ImageEvent.x/y/w/h`) is normalized so width/height are
    /// only used for aspect-fit fallback and metadata display.
    pub width: u32,
    pub height: u32,
    /// `image::ImageFormat` debug string (`"Png"` / `"Jpeg"` /
    /// `"WebP"` / etc.). Free-form, consumer uses for diagnostics
    /// only.
    pub format: String,
}

/// Path resolution strategy for an `ImageSource`. Mirrors
/// `VideoSourcePath`. v13 (`docs/plan_image_overlay.md` §2.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum ImageSourcePath {
    ProjectRelative(PathBuf),
    Absolute(PathBuf),
}

/// Image clip content — an ordered list of image events that display
/// within the clip. Mirrors `VideoContent`'s shape so the existing
/// clip / event UX (Split / Glue / drag move / trim / fade in/out)
/// applies uniformly. Multiple events on the same clip can overlap
/// (= the preview composite alpha-blends them, top-event-wins by
/// emit order) or sit side by side (= splittable PiP montage).
///
/// `#[serde(deny_unknown_fields)]` is required so `#[serde(untagged)]`
/// `ClipContent` distinguishes `Image` vs `Audio` / `Video`: the
/// disjoint required field is `ImageEvent.opacity`, absent from both
/// `AudioEvent` and `VideoEvent`. Denying unknowns prevents a future
/// field addition from widening the match unexpectedly.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ImageContent {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<ImageEvent>,
}

/// One playable image event inside an `ImageContent`. Maps an
/// `ImageSource` to a position in the clip
/// (`event_start_in_clip_beats` + `event_length_beats`) and a PiP
/// rect in normalized 0-1 preview coordinates.
///
/// **Required-field invariant**: `opacity` MUST stay as a required
/// (no `#[serde(default)]`) field of distinct name from any required
/// field of `AudioEvent` and `VideoEvent`. The untagged `ClipContent`
/// dispatch relies on this to disambiguate Image vs Audio / Video
/// JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ImageEvent {
    pub source_id: ImageSourceId,
    /// Clip-local beat at which the event starts.
    pub event_start_in_clip_beats: f64,
    /// Duration of the event in clip-local beats. Image is static so
    /// the source has no inherent duration — the user freely extends
    /// the event by drag-trim.
    pub event_length_beats: f64,

    /// PiP rect in normalized preview-window coordinates. `(0.0, 0.0)`
    /// is the top-left corner of the preview window, `(1.0, 1.0)` is
    /// the bottom-right. `(x, y)` is the top-left of the image's
    /// rect, `(w, h)` is its width / height. Example:
    /// `(0.0, 0.0, 1.0, 1.0)` fills the entire preview; `(0.7, 0.0,
    /// 0.3, 0.3)` lands a 30%×30% logo in the top-right corner.
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,

    /// Overall transparency (0.0 = fully transparent, 1.0 = fully
    /// opaque). Multiplied with the fade envelope (= the image
    /// crossfades and the user-set base opacity stack).
    ///
    /// **JSON disambiguation required field** — see struct doc.
    pub opacity: f32,

    /// v15 (`docs/plan_image_automation.md` rotation): rect 中心を旋回
    /// 中心とする 2D 回転 (radians、 clockwise positive)。 `0.0` =
    /// 軸並行 (互換)、 `±π` = 180°、 範囲は実用上 `-π..=π` で wrap。
    /// gui_01 #047 で `TexturedQuad.rotation_radians` が landing 次第
    /// preview / render passes に wire される。 lane override も同単位。
    #[serde(default)]
    pub rotation_radians: f32,

    pub muted: bool,
    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
    pub fade_in_curve: FadeCurve,
    pub fade_out_curve: FadeCurve,
}

impl Default for ImageEvent {
    fn default() -> Self {
        Self {
            source_id: 0,
            event_start_in_clip_beats: 0.0,
            event_length_beats: 0.0,
            // PiP rect defaults to "full screen" so a freshly-dropped
            // image immediately shows something visible; the user can
            // shrink/move it in the inspector or preview drag handle.
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            opacity: 1.0,
            rotation_radians: 0.0,
            muted: false,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        }
    }
}

/// 口形状クラス (lip-sync, v21、`docs/plan_pakupaku.md`)。VOICEVOX phoneme を
/// この 7 種へ畳む。母音 a/i/u/e/o、撥音 N (ん)、閉口 Closed (cl 促音 / pau
/// ポーズ / 子音で続く母音が無い場合 / 未割当時の fallback)。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode,
)]
pub enum MouthShape {
    A,
    I,
    U,
    E,
    O,
    N,
    Closed,
}

/// 口形状 → `ImageSourceId` のマッピング (lip-sync, v21)。`0` = 未割当
/// sentinel。口 track (= vocal の `lipsync_target_track` が指す image track) に
/// `Track.mouth_map` として持たせる。各 slot には通常の image import で
/// `Song.image_sources` に登録した口画像の id を割り当てる (id 参照のみを保持し、
/// 画像実体はプール 1 箇所が SSoT)。
#[derive(
    Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode,
)]
pub struct MouthMap {
    pub a: ImageSourceId,
    pub i: ImageSourceId,
    pub u: ImageSourceId,
    pub e: ImageSourceId,
    pub o: ImageSourceId,
    pub n: ImageSourceId,
    pub closed: ImageSourceId,
}

impl MouthMap {
    /// その口形状に割り当てられた id (未割当なら `0`)。
    pub fn get(&self, shape: MouthShape) -> ImageSourceId {
        match shape {
            MouthShape::A => self.a,
            MouthShape::I => self.i,
            MouthShape::U => self.u,
            MouthShape::E => self.e,
            MouthShape::O => self.o,
            MouthShape::N => self.n,
            MouthShape::Closed => self.closed,
        }
    }

    /// 描画に使う id を解決する。slot が未割当 (`0`) なら閉口へ fallback し、
    /// 閉口も未割当なら `0` (= 描画なし) を返す。
    pub fn resolve(&self, shape: MouthShape) -> ImageSourceId {
        let id = self.get(shape);
        if id != 0 { id } else { self.closed }
    }

    /// いずれかの slot に割当がある (= 口パクを生成する意味がある)。
    pub fn is_configured(&self) -> bool {
        [self.a, self.i, self.u, self.e, self.o, self.n, self.closed]
            .iter()
            .any(|&id| id != 0)
    }
}

/// Text clip content — `docs/plan_text_overlay.md` §2.2 (v16)。 単一行
/// の text overlay。 1 clip = 1 text、 複数行は禁止 (\n を含む `text`
/// は描画時に最初の改行で truncate するか、 model 側で reject する)。
///
/// `#[serde(deny_unknown_fields)]` は `ClipContent` の `#[serde(untagged)]`
/// dispatch のため必須。 `TextEvent.text` 等の disjoint required field で
/// Audio / Video / Image / Automation / MIDI と判別される。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct TextContent {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<TextEvent>,
}

/// `TextEvent.align` 用 enum。 horizontal alignment 3 選択
/// (`docs/plan_text_overlay.md` §1.5)。 vertical は単一行 text のため
/// 常に center (= box の縦中央 baseline) 固定。
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode,
)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// One playable text event inside a `TextContent` (`docs/plan_text_overlay.md`
/// §2.2)。 単一行 text、 PiP rect + font + color + outline + shadow + rotation
/// 等の描画属性を持つ。 image PiP の `(x, y, w, h)` は project resolution の
/// letterbox 内 normalized 0..=1 で展開される (= 画像 PiP と同 idiom、 window
/// resize で aspect 維持)。
///
/// **JSON disambiguation required field**: `text: String` と
/// `font_family: String` の同時保持で他 variant と disjoint。 ただし
/// `ImageEvent` も `String` field を間接保持していないので衝突無し。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct TextEvent {
    /// 表示する文字列 (単一行、 UTF-8、 改行禁止)。
    pub text: String,
    /// system font 名 (例 `"Yu Gothic"` / `""` で default)。 glyphon が
    /// 解決失敗時は fallback chain で代替。
    pub font_family: String,
    /// project resolution 基準 px (= 1920x1080 で 48 px なら 48.0)。
    pub font_size_px: f32,
    /// 塗り色 RGBA (0.0..=1.0)。
    pub fill_color: [f32; 4],
    /// アウトライン色 RGBA。 `outline_width_px == 0.0` ならアウトライン無効。
    pub outline_color: [f32; 4],
    /// アウトライン太さ (project resolution 基準 px、 0.0 で無効)。
    pub outline_width_px: f32,
    /// ドロップシャドウ色 RGBA。 `shadow_offset == (0, 0)` && `shadow_blur
    /// == 0.0` && color alpha == 0.0 のとき shadow 無し。
    pub shadow_color: [f32; 4],
    /// シャドウオフセット (project resolution 基準 px、 (dx, dy))。
    pub shadow_offset_px: (f32, f32),
    /// シャドウぼかし半径 (project resolution 基準 px、 0.0 で hard shadow)。
    pub shadow_blur_px: f32,
    /// horizontal alignment (vertical は単一行 text で center 固定)。
    pub align: TextAlign,
    /// Clip-local beat (image / audio event と同 idiom)。
    pub event_start_in_clip_beats: f64,
    pub event_length_beats: f64,
    /// PiP rect in normalized 0-1 letterbox coordinates (image と同 idiom)。
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// 全体 opacity (0..=1)。 fade envelope と multiply。
    pub opacity: f32,
    /// box 中心を旋回中心とする 2D 回転 (radians、 clockwise positive)。
    pub rotation_radians: f32,
    pub muted: bool,
    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
    pub fade_in_curve: FadeCurve,
    pub fade_out_curve: FadeCurve,
}

impl Default for TextEvent {
    fn default() -> Self {
        Self {
            text: String::new(),
            font_family: String::new(),
            font_size_px: 64.0,
            fill_color: [1.0, 1.0, 1.0, 1.0],
            outline_color: [0.0, 0.0, 0.0, 1.0],
            outline_width_px: 0.0,
            shadow_color: [0.0, 0.0, 0.0, 0.5],
            shadow_offset_px: (0.0, 0.0),
            shadow_blur_px: 0.0,
            align: TextAlign::Center,
            event_start_in_clip_beats: 0.0,
            event_length_beats: 0.0,
            // 既定 PiP rect = 「中央付近の横帯」 (= 標準 title 位置)。
            x: 0.0,
            y: 0.4,
            w: 1.0,
            h: 0.2,
            opacity: 1.0,
            rotation_radians: 0.0,
            muted: false,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        }
    }
}

/// A free-time note inside a clip. `start_beat` is relative to the clip
/// start; `duration_beats` is the note length. `pitch` is a MIDI key
/// (0..=127), `velocity` is 0..=127. `lyric` is attached for VOICEVOX
/// singing synthesis and is `None` for purely instrumental tracks.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Note {
    /// v29: content 内で安定な note id (`MidiContent.next_note_id` 採番、`0`
    /// = 未採番 sentinel — `ClipContent::ensure_element_ids` が load 時に
    /// 採番)。選択・undo 後の選択復元は positional index でなくこの id で
    /// アドレスする。linked clip は content を共有するので id も共有される。
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub id: u32,
    pub start_beat: f64,
    pub duration_beats: f64,
    pub pitch: u8,
    pub velocity: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lyric: Option<String>,
    /// note 単位のミュート。`true` でこの note を再生・書き出しから除外し
    /// (歌唱 note も含む)、piano roll は dim + 斜線ハッチで「ミュート中」を表示する。`q`
    /// ショートカット (選択 note / カーソル直下 note を toggle) が読み書きする。linked clip は
    /// content (= notes) を共有するので、note mute も linked clip 間で共有される。v26 以前は
    /// `false` に forward-migrate。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub muted: bool,
}

// =============================================================================
// Automation
// =============================================================================
//
// See `docs/plan_automation.md` for the full design. The summary:
//
// - Each `Track` carries `automation_lanes: Vec<AutomationLane>`. A lane has a
//   `target` (track-builtin volume / pan / mute, or a plugin parameter), a
//   `default_value` used outside any clip, and a list of `AutomationClip`.
// - `AutomationClip` is positioned along the track timeline and references a
//   `ContentId` in `Song.clip_contents` — the same shared store MIDI / Audio
//   clips use, so linked / independent copy machinery transparently applies.
// - `ClipContent::Automation(AutomationContent { points })` stores the actual
//   curve data. `#[serde(untagged)]` dispatch on `ClipContent` picks the
//   variant based on the disjoint field set (`notes` / `events` / `points`).

