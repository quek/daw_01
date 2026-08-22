// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! clip の **中身** (波形 / MIDI ノート) を model + audio cache から 1 フレーム分だけ
//! 集める層。 `run.rs` から分離 (god file budget)。
//!
//! r.md #68: ここが出す座標はすべて **content-local 拍**。 窓 (`content_offset_beats`)
//! は `ClipView` が持ち、 画面 x への換算は [`geometry::content_map`] 1 本が行う。
//!
//! ドラッグゴースト用の中身 ([`build_stretch_ghost_content`]) も同じ builder を通す。
//! Shift + 端 drag (= time-stretch) では **確定処理と同じ [`crate::app_types::stretch_remap`]**
//! で event / note を写像してから span を引き直すので、 プレビューと確定結果が
//! Slice のスライス配置や `Raw → Stretch` 昇格まで含めて一致する。

use std::borrow::Cow;
use std::collections::HashMap;

use common::audio_render::{TempoMap, WaveSpan, event_wave_spans};
use common::model::{Clip, ClipContent, StretchMode};

use crate::app::AppData;
use crate::app_types::stretch_remap;

use super::draw::{AudioEventDraw, ClipContentDraw, MidiNoteDraw};
use super::geometry::resize_preview_start_len;
use super::{ArrangementTrack, ClipDragKind, ClipDragSession, ClipKey, MASTER_TRACK_ID};

/// time-stretch プレビューの写像パラメータ (`stretch_clip_content` の引数と同じ形)。
#[derive(Clone, Copy, Debug)]
struct StretchPreview {
    prev_start: f64,
    prev_len: f64,
    new_start: f64,
    new_len: f64,
}

impl StretchPreview {
    /// clip 窓の起点 (`content_offset_beats`) の変化ぶん。 `resize_clip` が
    /// `content_offset_beats += Δstart` を行うのと同じ量。
    fn offset_delta(self) -> f64 {
        self.new_start - self.prev_start
    }

    /// content-local (start, len) → 伸縮後の content-local (start, len)。
    /// `stretch_clip_content` が model へ書き戻すのと同じ式。
    fn remap(self, prev_off: f64, start: f64, len: f64) -> (f64, f64) {
        let (s, l) = stretch_remap(
            self.prev_start,
            self.prev_len,
            self.new_start,
            self.new_len,
            start - prev_off,
            len,
        );
        (prev_off + self.offset_delta() + s, l)
    }
}

/// visible clip すべての中身 (base 描画用)。
pub(super) fn build_clip_content(
    app: &AppData,
    tempo_map: &TempoMap,
    visible_tracks: &[ArrangementTrack],
) -> HashMap<ClipKey, ClipContentDraw> {
    let mut map: HashMap<ClipKey, ClipContentDraw> = HashMap::new();
    let mut spans: Vec<WaveSpan> = Vec::new();
    for t in visible_tracks {
        if t.id == MASTER_TRACK_ID {
            continue;
        }
        let Some(mt) = app.song_doc.song().tracks.iter().find(|mt| mt.id == t.id) else {
            continue;
        };
        for c in &t.clips {
            let Some(mc) = mt.clips.iter().find(|mc| mc.id == c.id) else {
                continue;
            };
            let key = ClipKey { track: t.id, clip: c.id };
            if let Some(content) = build_one(app, tempo_map, mc, None, &mut spans) {
                map.insert(key, content);
            }
        }
    }
    map
}

/// **Shift + 端 drag (= time-stretch) のときだけ** 差し替わるゴーストの中身。
///
/// r.md #68: ゴーストは base の中身を「別のスケールで」 描くのではなく、
/// **確定後の中身そのもの** を描く。 トリム / 移動では確定後の中身は base と
/// **完全に同一** (中身は 1px も動かない) なので、ここは空を返し、描画側は
/// `clip_content` をそのまま使う (= 毎フレームの span 再計算を増やさない)。
/// time-stretch のときだけ、確定処理と同じ `stretch_remap` で写像した内容を返す。
pub(super) fn build_stretch_ghost_content(
    app: &AppData,
    tempo_map: &TempoMap,
    // base の `clip_content` と **同じ定義域** にするために必要 (下の guard 参照)。
    visible_tracks: &[ArrangementTrack],
    drag: Option<&(ClipDragSession, f64, i32)>,
    min_len: f64,
) -> HashMap<ClipKey, ClipContentDraw> {
    let mut map: HashMap<ClipKey, ClipContentDraw> = HashMap::new();
    let Some(&(ref nd, beat_delta, _)) = drag else {
        return map;
    };
    // Shift が効くのは端 drag だけ (`release.rs` は Resize の arm でしか
    // `stretch: nd.last_shift` を送らない)。 Move の Shift は clone 種別の修飾。
    if !nd.last_shift || !matches!(nd.kind, ClipDragKind::ResizeLeft | ClipDragKind::ResizeRight)
    {
        return map;
    }
    let mut spans: Vec<WaveSpan> = Vec::new();
    for a in &nd.anchors {
        // **base と定義域を揃える**。 ゴーストの content 原点は描画側が
        // `visible_tracks` から引いた `ClipView::content_offset_beats` で決まるので、
        // ここだけ model から直接引いて中身を出すと、 drag 中に track がスクロールで
        // 見えなくなった瞬間に「中身はあるのに原点が 0 扱い」 になり、 伸縮した中身が
        // ゴースト枠に対して `content_offset_beats` ぶんずれる。
        if !visible_tracks.iter().any(|t| {
            t.id == a.key.track && t.clips.iter().any(|c| c.id == a.key.clip)
        }) {
            continue;
        }
        let Some(mt) = app
            .song_doc
            .song()
            .tracks
            .iter()
            .find(|mt| mt.id == a.key.track)
        else {
            continue;
        };
        let Some(mc) = mt.clips.iter().find(|mc| mc.id == a.key.clip) else {
            continue;
        };
        // ゴースト矩形 / release commit と **同じ** `resize_preview_start_len`。
        // 別々に写経すると、伸縮した中身とゴースト枠が食い違う。
        let (new_start, new_len) = resize_preview_start_len(
            a.start_beat,
            a.len_beats,
            nd.kind,
            beat_delta,
            min_len,
        );
        let stretch = StretchPreview {
            prev_start: a.start_beat,
            prev_len: a.len_beats,
            new_start,
            new_len,
        };
        if let Some(content) = build_one(app, tempo_map, mc, Some(stretch), &mut spans) {
            map.insert(a.key, content);
        }
    }
    map
}

/// clip 1 件の中身を組む。 `stretch` を渡すと time-stretch 後の内容になる。
///
/// `spans` は使い回しバッファ (毎フレーム全 event ぶん確保しないため)。
fn build_one(
    app: &AppData,
    tempo_map: &TempoMap,
    mc: &Clip,
    stretch: Option<StretchPreview>,
    spans: &mut Vec<WaveSpan>,
) -> Option<ClipContentDraw> {
    let content = app.song_doc.song().clip_contents.get(&mc.content_id)?;
    // content 原点は trim でも stretch でも不変 (`resize_clip` / `stretch_clip_content` が
    // start と offset を同量動かす)。 span の tempo 評価位置はここを基準に出す。
    let content_origin = mc.content_origin_beat();
    match content {
        ClipContent::Audio(_) => {
            // r.md #41: clip 内の **全** audio event を、 engine と同じ時間写像
            // (`event_wave_spans`) が返す span 列で描く。 Slice はスライスの
            // trigger 位置と gap、 Stretch は warp 区間、 逆再生は反転が
            // そのまま span に乗るので、 描画側に mode 分岐は要らない。
            let audio_events = content.audio_events()?;
            let mut events: Vec<AudioEventDraw> = Vec::new();
            for (ev_i, ev) in audio_events.iter().enumerate() {
                let Some(buffer) = app.media.audio_source_cache.get(ev.source_id) else {
                    // decode 待ち / missing source は skip (他 event は描く)。
                    continue;
                };
                // stretch preview のときだけ event を写像した複製を作る (base は借用のまま)。
                let ev: Cow<'_, _> = match stretch {
                    None => Cow::Borrowed(ev),
                    Some(st) => {
                        let mut e = ev.clone();
                        let (s, l) = st.remap(
                            mc.content_offset_beats,
                            e.event_start_in_clip_beats,
                            e.event_length_beats,
                        );
                        e.event_start_in_clip_beats = s;
                        e.event_length_beats = l;
                        // ピッチ保持を既定: Raw (= 時間操作しない定義) は Stretch へ昇格。
                        // `stretch_clip_content` と同じ規則にしないと、 span の張る範囲が
                        // 確定後と食い違う (Raw は native 長で鳴り止むため)。
                        if e.stretch_mode == StretchMode::Raw {
                            e.stretch_mode = StretchMode::Stretch;
                        }
                        Cow::Owned(e)
                    }
                };
                event_wave_spans(
                    &ev,
                    buffer.sample_rate,
                    tempo_map,
                    // event の song 位置は content 原点基準 (tempo 曲線の評価位置)。
                    content_origin + ev.event_start_in_clip_beats,
                    spans,
                );
                if spans.is_empty() {
                    continue;
                }
                events.push(AudioEventDraw {
                    buffer,
                    source_id: ev.source_id,
                    // 波形 widget の LOD state キー。 `AudioEvent.id` は
                    // 安定 id (undo / 並べ替えを跨ぐ) なので decode 完了で
                    // 詰め方が変わっても pyramid が入れ替わらない。 未採番
                    // (0 sentinel) の古い song だけ model index に degrade。
                    key: if ev.id != 0 {
                        u64::from(ev.id)
                    } else {
                        u64::MAX - ev_i as u64
                    },
                    start_in_clip_beats: ev.event_start_in_clip_beats,
                    len_beats: ev.event_length_beats,
                    stretch_mode: ev.stretch_mode,
                    spans: std::mem::take(spans),
                });
            }
            (!events.is_empty()).then_some(ClipContentDraw::Audio { events })
        }
        _ => {
            let notes = content.notes()?;
            if notes.is_empty() {
                return None;
            }
            let nd: Vec<MidiNoteDraw> = notes
                .iter()
                .map(|n| {
                    let (start_beat, duration_beats) = stretch.map_or(
                        (n.start_beat, n.duration_beats),
                        |st| st.remap(mc.content_offset_beats, n.start_beat, n.duration_beats),
                    );
                    MidiNoteDraw {
                        pitch: n.pitch,
                        start_beat,
                        duration_beats,
                        velocity: n.velocity,
                    }
                })
                .collect();
            Some(ClipContentDraw::Midi { notes: nd })
        }
    }
}
