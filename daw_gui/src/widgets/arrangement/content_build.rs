//! clip の **中身** (波形 / MIDI ノート) を model + audio cache から 1 フレーム分だけ
//! 集める層。 `run.rs` から分離 (god file budget)。
//!
//! r.md #68: ここが出す座標はすべて **content-local 拍**。 窓 (`content_offset_beats`)
//! は `ClipView` が持ち、 画面 x への換算は [`geometry::content_map`] 1 本が行う。
//!
//! ドラッグゴースト用の中身 ([`build_stretch_ghost_content`]) も同じ builder を通す。
//! Shift + 端 drag (= time-stretch) では **確定処理と同じ
//! [`ClipContent::apply_time_stretch`]** を content の複製に掛けてから span を引き直すので、
//! プレビューと確定結果が Slice のスライス配置や `Raw → Stretch` 昇格まで含めて一致する。
//! 伸縮は content の拍の軸と窓の起点を同じ倍率にするので、ゴーストの窓の起点
//! ([`resize_ghost_offset`]) とサムネイルの位相 ([`ghost_thumbnail`]) もここが出す。

use std::collections::HashMap;

use common::audio_render::{TempoMap, WaveSpan, event_wave_spans};
use common::model::{Clip, ClipContent};

use crate::app::AppData;

use super::draw::{AudioEventDraw, ClipContentDraw, MidiNoteDraw};
use super::geometry::resize_preview_start_len;
use super::{
    ArrangementTrack, ClipDragAnchor, ClipDragKind, ClipDragSession, ClipKey, ClipThumbnail,
    ClipView, MASTER_TRACK_ID,
};

/// time-stretch プレビューの写像パラメータ。
#[derive(Clone, Copy, Debug)]
pub(super) struct StretchPreview {
    /// 伸縮後の clip の開始拍。
    new_start: f64,
    /// 倍率 (= 新しい長さ ÷ 元の長さ)。
    factor: f64,
}

/// この drag が time-stretch (Shift + 端 drag) なら、その倍率 (= 新しい長さ ÷ 元の長さ)。
///
/// Shift が効くのは端 drag だけ (`release.rs` は Resize の arm でしか `stretch: nd.last_shift`
/// を送らない。 Move の Shift は clone 種別の修飾)。 退化した長さでは確定処理
/// (`stretch_clip_content`) も伸縮しないので `None`。
fn stretch_factor(nd: &ClipDragSession, anchor: &ClipDragAnchor, new_len: f64) -> Option<f64> {
    (nd.last_shift
        && matches!(nd.kind, ClipDragKind::ResizeLeft | ClipDragKind::ResizeRight)
        && anchor.len_beats > 1e-9
        && new_len > 1e-9)
        .then(|| new_len / anchor.len_beats)
}

/// 端 drag のゴーストの **窓の起点** (`content_offset_beats`) を確定後と一致させる。
///
/// - トリム: `resize_clip` が `content_offset_beats += Δstart` を行う (content 原点は不動)。
/// - time-stretch: content の拍の軸ごと伸縮するので、起点も同じ倍率になる
///   (`stretch_clip_content`)。
pub(super) fn resize_ghost_offset(
    nd: &ClipDragSession,
    anchor: &ClipDragAnchor,
    src_offset: f64,
    (new_start, new_len): (f64, f64),
) -> f64 {
    match stretch_factor(nd, anchor, new_len) {
        Some(factor) => src_offset * factor,
        None => src_offset + (new_start - anchor.start_beat),
    }
}

/// r.md #152: ゴーストに載せる video / image clip のサムネイル。
///
/// サムネイルは「それが表す event の content-local 開始拍」を位相に敷き詰める
/// ([`super::thumbnail::thumbnail_tiling`])。 time-stretch は event の開始拍も倍率ぶん写すので、
/// 同じ倍率を掛けて確定後の位相にする。 トリム / 移動では中身が動かないので元のまま返す。
pub(super) fn ghost_thumbnail(
    nd: &ClipDragSession,
    anchor: &ClipDragAnchor,
    src: Option<&ClipView>,
    new_len: f64,
) -> Option<ClipThumbnail> {
    let mut thumb = src?.thumbnail?;
    if let Some(factor) = stretch_factor(nd, anchor, new_len) {
        thumb.start_in_content_beats *= factor;
    }
    Some(thumb)
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
        let Some(mt) = app.cur.song_doc.song().tracks.iter().find(|mt| mt.id == t.id) else {
            continue;
        };
        for c in &t.clips {
            let Some(mc) = mt.clips.iter().find(|mc| mc.id == c.id) else {
                continue;
            };
            let key = ClipKey { track_id: t.id, clip_id: c.id };
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
/// time-stretch のときだけ、確定処理と同じ `ClipContent::apply_time_stretch` を掛けた内容を返す。
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
    let mut spans: Vec<WaveSpan> = Vec::new();
    for a in &nd.anchors {
        // ゴースト矩形 / release commit と **同じ** `resize_preview_start_len`。
        // 別々に写経すると、伸縮した中身とゴースト枠が食い違う。
        let (new_start, new_len) = resize_preview_start_len(
            a.start_beat,
            a.len_beats,
            nd.kind,
            beat_delta,
            min_len,
        );
        let Some(factor) = stretch_factor(nd, a, new_len) else {
            continue;
        };
        // **base と定義域を揃える**。 ゴーストの窓の起点は描画側が `visible_tracks` から引いた
        // `ClipView::content_offset_beats` で決まるので、 ここだけ model から直接引いて中身を
        // 出すと、 drag 中に track がスクロールで見えなくなった瞬間に「中身はあるのに起点が
        // 0 扱い」 になり、 伸縮した中身がゴースト枠に対してずれる。
        if !visible_tracks.iter().any(|t| {
            t.id == a.key.track_id && t.clips.iter().any(|c| c.id == a.key.clip_id)
        }) {
            continue;
        }
        let Some(mt) = app
            .cur.song_doc
            .song()
            .tracks
            .iter()
            .find(|mt| mt.id == a.key.track_id)
        else {
            continue;
        };
        let Some(mc) = mt.clips.iter().find(|mc| mc.id == a.key.clip_id) else {
            continue;
        };
        let stretch = StretchPreview { new_start, factor };
        if let Some(content) = build_one(app, tempo_map, mc, Some(stretch), &mut spans) {
            map.insert(a.key, content);
        }
    }
    map
}

/// clip 1 件の中身を組む。 `stretch` を渡すと time-stretch 後の内容になる。
///
/// `spans` は使い回しバッファ (毎フレーム全 event ぶん確保しないため)。
pub(super) fn build_one(
    app: &AppData,
    tempo_map: &TempoMap,
    mc: &Clip,
    stretch: Option<StretchPreview>,
    spans: &mut Vec<WaveSpan>,
) -> Option<ClipContentDraw> {
    let base = app.cur.song_doc.song().clip_contents.get(&mc.content_id)?;
    // stretch preview のときだけ、確定 (`stretch_clip_content`) と同じ 1 本を複製に掛ける
    // (take の軸の伸縮と Raw→Stretch の昇格まで揃えないと、 span の張る範囲が確定後と食い違う)。
    // base は借用のまま。
    let stretched = stretch.map(|st| {
        let mut c = base.clone();
        c.apply_time_stretch(st.factor);
        c
    });
    let content = stretched.as_ref().unwrap_or(base);
    // content 原点 (content-local 拍 0 の song 拍)。 span の tempo 評価位置はここを基準に出す。
    // trim では不変、 stretch では窓の起点が倍率ぶん動く (`resize_ghost_offset` と同じ量)。
    let content_origin = stretch.map_or_else(
        || mc.content_origin_beat(),
        |st| st.new_start - mc.content_offset_beats * st.factor,
    );
    match content {
        ClipContent::Audio(_) => {
            // r.md #41: clip 内の **全** audio event を、 engine と同じ時間写像
            // (`event_wave_spans`) が返す span 列で描く。 Slice はスライスの
            // trigger 位置と gap、 Stretch は warp 区間、 逆再生は反転が
            // そのまま span に乗るので、 描画側に mode 分岐は要らない。
            let audio_events = content.audio_events()?;
            let mut events: Vec<AudioEventDraw> = Vec::new();
            for (ev_i, ev) in audio_events.iter().enumerate() {
                let Some(buffer) = app.cur.media.audio_source_cache.get(ev.source_id) else {
                    // decode 待ち / missing source は skip (他 event は描く)。
                    continue;
                };
                event_wave_spans(
                    ev,
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
                .map(|n| MidiNoteDraw {
                    pitch: n.pitch,
                    start_beat: n.start_beat,
                    duration_beats: n.duration_beats,
                    velocity: n.velocity,
                })
                .collect();
            Some(ClipContentDraw::Midi { notes: nd })
        }
    }
}
