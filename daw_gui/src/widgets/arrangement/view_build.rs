//! arrangement widget の入力ビュー構築 (S4b: 旧 `arrangement_view.rs` の mirror
//! 構築 + label / lane 派生を widget 内へ移設)。
//!
//! `AppData` (`song_doc` / `selection` / `ui_prefs` / `ui_ephemeral`) から widget が
//! 描画・hit-test に使うビュー構造体 ([`BuiltArrangement`]) を **直接** 組み立てる。
//! 旧設計は `ui/` の汎用 widget が `common::model` を触れないため mirror 型 +
//! `make_edit` 翻訳層を挟んでいたが、 widget が `daw_gui` に移ったので model 直読。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use daw_ui_renderer::{Color, Rect};

use crate::app::AppData;
use crate::view::snap;
use crate::view::track_color;

use super::{
    ArrangementAutomationClip, ArrangementAutomationLane, ArrangementAutomationPoint,
    ArrangementMasterRow, ArrangementStyle, ArrangementTrack, ArrangementView,
    AutomationClipKey, AutomationPointKey, ClipThumbnail, ClipView, ClipViewAudioEdit, ClipKey,
    ClipEventFade, SectionView, TrackKind, pixel_snapped_scroll_beat, view_len_beats,
};

/// Arranger レーン上に確保する ruler の高さ (px)。draw() の RULER_H と一致。
pub(super) const RULER_H: f32 = 20.0;
/// Arranger レーン (曲のパート帯) の高さ (px)。ルーラー直下に確保。
const SECTION_LANE_H: f32 = 18.0;

/// `arrangement()` が受け取る、`AppData` から派生した 1 フレーム分のビュー。
/// 旧 `Ui::arrangement(tracks, sections, view, selected_*, style, master_row, make_edit)`
/// の全入力を 1 struct に束ねたもの。
pub(super) struct BuiltArrangement {
    pub tracks: Vec<ArrangementTrack>,
    pub sections: Vec<SectionView>,
    pub view: ArrangementView,
    pub style: ArrangementStyle,
    pub selected_clips: Vec<ClipKey>,
    pub selected_tracks: Vec<u32>,
    pub selected_automation_clips: Vec<AutomationClipKey>,
    pub selected_automation_points: Vec<AutomationPointKey>,
    pub master_row: ArrangementMasterRow,
}

/// `AppData` から widget 入力を組み立てる (旧 `arrangement_view::draw` L106-568)。
#[allow(clippy::too_many_lines)]
pub(super) fn build(app: &AppData, area: Rect) -> BuiltArrangement {
    // Phase 6 review perf (E11): depth / refcount を 1 度だけ batch 計算。
    let n_tracks = app.song_doc.song().tracks.len();
    let mut id_to_parent: HashMap<u32, Option<u32>> = HashMap::with_capacity(n_tracks);
    for t in &app.song_doc.song().tracks {
        id_to_parent.insert(t.id, t.parent_group_id);
    }
    let compute_depth = |track_id: u32| -> u8 {
        let mut cursor = id_to_parent.get(&track_id).copied().flatten();
        let mut depth: u8 = 0;
        let mut hops = 0u8;
        while let Some(pid) = cursor {
            depth = depth.saturating_add(1);
            hops = hops.saturating_add(1);
            if hops > 32 {
                break;
            }
            cursor = id_to_parent.get(&pid).copied().flatten();
        }
        depth
    };
    let mut refcount_by_content: HashMap<common::model::ContentId, usize> = HashMap::new();
    for t in &app.song_doc.song().tracks {
        for c in &t.clips {
            *refcount_by_content.entry(c.content_id).or_insert(0) += 1;
        }
        for lane in &t.automation_lanes {
            for c in &lane.clips {
                *refcount_by_content.entry(c.content_id).or_insert(0) += 1;
            }
        }
    }
    for lane in &app.song_doc.song().song_lanes {
        for c in &lane.clips {
            *refcount_by_content.entry(c.content_id).or_insert(0) += 1;
        }
    }

    // gui_01 #068 連動ハイライト: {選択 clip} ∪ {前フレーム hover clip} の content_id (refcount>=2)。
    let active_groups: HashSet<common::model::ContentId> = if app.selection.selected_clips.is_empty()
        && app.ui_ephemeral.arrange_hover_content.is_none()
    {
        HashSet::new()
    } else {
        let mut set = HashSet::new();
        let is_shared =
            |cid: common::model::ContentId| refcount_by_content.get(&cid).copied().unwrap_or(0) >= 2;
        for r in app.selected_clip_refs() {
            if let Some(c) = app
                .song_doc
                .song()
                .tracks
                .get(r.track as usize)
                .and_then(|t| t.clips.get(r.clip as usize))
                && is_shared(c.content_id)
            {
                set.insert(c.content_id);
            }
        }
        if let Some(cid) = app.ui_ephemeral.arrange_hover_content
            && is_shared(cid)
        {
            set.insert(cid);
        }
        set
    };

    // D3/D4: track/clip 名の `Arc<str>` キャッシュ (song_epoch 世代キー)。
    let labels = app.arrangement_labels();
    let lane_build_data = LaneBuildData {
        song: app.song_doc.song(),
        refcount_by_content: &refcount_by_content,
        lane_height_overrides: &app.ui_prefs.automation_lane_row_overrides,
        content_names: &labels.content_names,
    };
    let tracks: Vec<ArrangementTrack> = app
        .song_doc
        .song()
        .tracks
        .iter()
        .map(|t| ArrangementTrack {
            id: t.id,
            kind: if t.clips.iter().any(|c| {
                matches!(
                    app.song_doc.song().clip_contents.get(&c.content_id),
                    Some(common::model::ClipContent::Video(_))
                        | Some(common::model::ClipContent::Image(_))
                        | Some(common::model::ClipContent::Text(_))
                )
            }) {
                TrackKind::Video
            } else {
                TrackKind::Audio
            },
            name: labels
                .track_names
                .get(&t.id)
                .cloned()
                .unwrap_or_else(|| Arc::from(t.name.as_str())),
            muted: t.muted,
            solo: t.solo,
            armed: t.armed,
            // r.md #21: `ArrangementTrack.volume` は **amp ドメイン** (struct doc「1.0 で unity」、
            // master 行 synthesize_master_track も 1.0、 consumer の run.rs は `amp_to_frac(t.volume)`
            // で frac 位置に直す)。 ここで amp_to_frac すると run.rs 側と合わせて **二重変換** になり、
            // +6dB へ上げたフェーダーが release で 0dB 位置へ部分的に戻っていた (drag 中は mouse frac
            // 直描画なので露呈せず、 停止中の静的表示だけがずれる = 「スケールが違う」 症状)。 amp を素通し。
            volume: t.volume,
            clips: t
                .clips
                .iter()
                .map(|c| {
                    let content = app.song_doc.song().clip_contents.get(&c.content_id);
                    ClipView {
                        id: c.id,
                        start_beat: c.start_beat,
                        len_beats: c.length_beats,
                        // r.md #68: 中身 (波形 / MIDI / fade / thumbnail) の x 写像は
                        // `start_beat - content_offset_beats` (= content 原点) を原点に、
                        // ビューのズーム 1 本で決まる (`geometry::content_map`)。
                        content_offset_beats: c.content_offset_beats,
                        name: labels
                            .content_labels
                            .get(&c.content_id)
                            .cloned()
                            .unwrap_or_else(|| clip_display_label(c, app.song_doc.song())),
                        color: Some(track_color::to_renderer(track_color::effective_clip_color(t, c))),
                        share_group_color: if refcount_by_content
                            .get(&c.content_id)
                            .copied()
                            .unwrap_or(0)
                            >= 2
                        {
                            Some(content_id_to_hue(c.content_id))
                        } else {
                            None
                        },
                        in_active_group: active_groups.contains(&c.content_id),
                        muted: c.muted,
                        audio_edit: content
                            .and_then(|ct| ct.audio_events())
                            .and_then(|events| events.first())
                            .map(|ev| ClipViewAudioEdit { gain_db: ev.gain_db }),
                        // r.md #38: fade は content 種別に依らず全 event ぶん渡す
                        // (音声だけ / first event だけ だった旧実装を置換)。
                        // r.md #68: 座標系は **content-local のまま素通し**。 r.md #44 で
                        // 窓ローカルへ畳んでいたが、 それだと中身の原点が窓と一緒に動き、
                        // 端 drag の preview が「クリップ幅 ÷ クリップ長」 のスケールに頼る
                        // ことになる。 窓の offset は `ClipView::content_offset_beats` が持ち、
                        // 換算は `geometry::content_map` 1 本に集約した。
                        fades: content.map_or_else(Vec::new, |ct| {
                            ct.event_fades()
                                .into_iter()
                                .enumerate()
                                .map(|(i, fade)| ClipEventFade {
                                    event_index: u32::try_from(i).unwrap_or(u32::MAX),
                                    fade,
                                })
                                .collect()
                        }),
                        // r.md #68: thumbnail も「中身」 なので、 それが表す event の
                        // content-local 開始拍を添える (widget は content 原点基準で置き、
                        // clip 矩形で切り抜く = 端 drag しても絵が動かない)。
                        thumbnail: {
                            content
                                .and_then(|ct| ct.video_events())
                                .and_then(|events| events.first())
                                .and_then(|ev| {
                                    let texture =
                                        *app.ui_ephemeral.video_texture_cache.get(&ev.source_id)?;
                                    let src = app.song_doc.song().media.video_sources.get(&ev.source_id)?;
                                    Some(ClipThumbnail {
                                        texture,
                                        width: src.width,
                                        height: src.height,
                                        start_in_content_beats: ev.event_start_in_clip_beats,
                                    })
                                })
                                .or_else(|| {
                                    let events = content?.image_events()?;
                                    let ev = events.first()?;
                                    let texture =
                                        *app.ui_ephemeral.image_texture_cache.get(&ev.source_id)?;
                                    let src = app.song_doc.song().media.image_sources.get(&ev.source_id)?;
                                    Some(ClipThumbnail {
                                        texture,
                                        width: src.width,
                                        height: src.height,
                                        start_in_content_beats: ev.event_start_in_clip_beats,
                                    })
                                })
                        },
                    }
                })
                .collect(),
            parent_id: t.parent_group_id,
            depth: compute_depth(t.id),
            collapsed: app.ui_prefs.collapsed_groups.contains(&t.id),
            automation_lanes_collapsed: !app.ui_prefs.expanded_automation_tracks.contains(&t.id),
            automation_lanes: build_arrangement_automation_lanes(
                t,
                lane_build_data,
                &|tgt| app.plugin_param_range(t.id, tgt),
                &|tgt| app.plugin_param_name(tgt),
            ),
            row_h: app.ui_prefs.track_row_overrides.get(&t.id).copied(),
            color: Some(track_color::to_renderer(track_color::effective_track_color(t))),
        })
        .collect();

    let selected_clips: Vec<ClipKey> = app
        .selection
        .selected_clips
        .iter()
        .map(|k| ClipKey { track: k.track_id, clip: k.clip_id })
        .collect();

    let selected_tracks: Vec<u32> = app.selection.selected_track_ids.clone();

    let zoom = app.ui_prefs.arrange_zoom_x.max(1.0);
    let row_h = app.ui_prefs.arrange_track_row_h.max(1.0);
    let lanes_w = (area.w - app.ui_prefs.arrange_header_w).max(1.0);
    let loop_range = app.transport.loop_region.range();
    let data_generation = data_generation(&tracks);

    // r.md #53: 表示原点はデバイスピクセル境界に載せる (`clip_to_rect` が使うのと同じ
    // `beat_to_px` で丸めるので、スクロール中は全アイテムが整数 px の剛体平行移動になる)。
    let scroll_beat_raw = f64::from(app.ui_prefs.arrange_scroll_beat);
    let view = ArrangementView {
        start_beat: pixel_snapped_scroll_beat(scroll_beat_raw, lanes_w, zoom),
        scroll_beat_raw,
        len_beats: view_len_beats(lanes_w, zoom),
        track_top: app.ui_prefs.arrange_track_top,
        // r.md #63: 分母は「行が実際に描かれる高さ」 = ruler と Arranger 帯を除いた lanes 高さ。
        // ここは heavy cache キーにしか使わないが、 `area.h - RULER_H` のままだと同じ誤式の
        // 3 つ目のコピーとして残り、 次に誰かが「lanes の高さ」 としてコピーする種になる。
        tracks_visible: ((area.h - RULER_H - SECTION_LANE_H) / row_h).max(1.0),
        track_row_h: row_h,
        header_w: app.ui_prefs.arrange_header_w,
        ruler_h: RULER_H,
        playhead_beat: app.transport.playhead_beat.map(|b| b as f64),
        loop_range,
        data_generation,
        bpm: app.song_doc.song().bpm,
        time_sig: app.song_doc.song().time_sig,
        snap: snap::arrange_snap_config(app),
        arranger_lane_h: SECTION_LANE_H,
    };

    // r.md #48: style の色は **いま有効なテーマ** から組む (`const STYLE` / `Default` は
    // runtime パレットを読めない)。ここで上書きするのは色ではない寸法と、意図的に描画を
    // 止める 1 色だけ。
    let style = ArrangementStyle {
        // 完全透明で dB handle line の描画だけ抑制する (hit zone は色非依存)。
        audio_db_handle_color: Color::TRANSPARENT,
        track_text_size: 11.0,
        master_row_label_size: 11.0,
        indent_px: 8.0,
        ..ArrangementStyle::from_theme(&app.theme)
    };

    let selected_automation_clips: Vec<AutomationClipKey> = app
        .selection
        .selected_automation_clips
        .iter()
        .map(|k| AutomationClipKey { track: k.track, lane: k.lane, clip: k.clip })
        .collect();

    let selected_automation_points: Vec<AutomationPointKey> = app
        .selection
        .selected_automation_points
        .iter()
        .map(|k| AutomationPointKey {
            clip: AutomationClipKey { track: k.track_id, lane: k.lane_id, clip: k.clip_id },
            point_idx: k.point_idx,
        })
        .collect();

    // master row: Song.song_lanes → automation lane 群。
    let master_row_lanes = build_arrangement_lanes_from_slice(
        &app.song_doc.song().song_lanes,
        common::model::MASTER_TRACK_ID,
        lane_build_data,
        &|tgt| app.plugin_param_range(common::model::MASTER_TRACK_ID, tgt),
        &|tgt| app.plugin_param_name(tgt),
    );
    let master_row = ArrangementMasterRow {
        automation_lanes_collapsed: !app.ui_prefs.master_row_automation_expanded,
        automation_lanes: master_row_lanes,
        height_px_override: None,
    };

    let sections: Vec<SectionView> = app
        .song_doc
        .song()
        .sections
        .iter()
        .map(|s| SectionView {
            id: s.id,
            name: labels
                .section_names
                .get(&s.id)
                .cloned()
                .unwrap_or_else(|| Arc::from(s.name.as_str())),
            color: s.color,
            start_beat: s.start_beat,
            len_beats: s.len_beats,
            selected: app.selection.selected_section_ids.contains(&s.id),
        })
        .collect();

    BuiltArrangement {
        tracks,
        sections,
        view,
        style,
        selected_clips,
        selected_tracks,
        selected_automation_clips,
        selected_automation_points,
        master_row,
    }
}

// ---------------------------------------------------------------------------
// clip 表示名の導出 (SSoT)
// ---------------------------------------------------------------------------

/// widget の heavy cache 無効化キー (描画内容そのものを hash)。
/// 使われるのは `run.rs` の `viewport_key` の 1 成分としてのみ。
///
/// r.md #58 同件: **hover 由来で毎フレーム変わる値を混ぜない**。 特に
/// `ClipView::in_active_group` — これは hover したリンクグループを強調するためだけの
/// 値で、 読むのは cached **外**の `draw_active_group_overlay` だけ。 混ぜると、
/// リンククリップにマウスを乗せるたびにアレンジ全体 (グリッド + 全クリップ + 全
/// オートメーションレーン) の heavy cache が捨てられて再構築される。
/// widget 側は `fold_arrangement_clip_hash_ignores_in_active_group` で同じ契約を
/// テスト固定しているが、 caller がここで迂回していた (2026-08-16 に是正)。
pub(super) fn data_generation(tracks: &[ArrangementTrack]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tracks.len().hash(&mut h);
    for (i, t) in tracks.iter().enumerate() {
        i.hash(&mut h);
        t.id.hash(&mut h);
        t.name.hash(&mut h);
        (t.muted, t.solo, t.armed).hash(&mut h);
        t.volume.to_bits().hash(&mut h);
        t.parent_id.hash(&mut h);
        t.depth.hash(&mut h);
        (t.collapsed, t.automation_lanes_collapsed).hash(&mut h);
        t.clips.len().hash(&mut h);
        for c in &t.clips {
            c.id.hash(&mut h);
            c.name.hash(&mut h);
            c.muted.hash(&mut h);
            c.share_group_color.map(f32::to_bits).hash(&mut h);
            if let Some(col) = c.color {
                [col.r, col.g, col.b, col.a].map(f32::to_bits).hash(&mut h);
            }
        }
        for lane in &t.automation_lanes {
            lane.label.hash(&mut h);
            for ac in &lane.clips {
                ac.id.hash(&mut h);
                ac.name.hash(&mut h);
            }
        }
    }
    h.finish()
}

/// gui_01 #019: 共有 clip 群の識別用 hue 計算。golden ratio で `[0.0, 1.0)` 一様分布。
fn content_id_to_hue(content_id: common::model::ContentId) -> f32 {
    const GOLDEN_RATIO_CONJUGATE: f32 = 0.618_034;
    (content_id as f32 * GOLDEN_RATIO_CONJUGATE).fract()
}

/// text clip の widget display label (32 文字 cap、非 Text / 空は `None`)。
fn text_clip_label(
    clip: &common::model::Clip,
    contents: &HashMap<common::model::ContentId, common::model::ClipContent>,
) -> Option<String> {
    let events = contents.get(&clip.content_id)?.text_events()?;
    let ev = events.first()?;
    if ev.text.is_empty() {
        return None;
    }
    const MAX_CHARS: usize = 32;
    let total = ev.text.chars().count();
    if total <= MAX_CHARS {
        Some(ev.text.clone())
    } else {
        let mut head: String = ev.text.chars().take(MAX_CHARS).collect();
        head.push('…');
        Some(head)
    }
}

/// MIDI ノート歌詞を start_beat 昇順で連結 (32 文字 cap)。歌詞無し / 非 MIDI は `None`。
fn lyric_clip_label(
    clip: &common::model::Clip,
    contents: &HashMap<common::model::ContentId, common::model::ClipContent>,
) -> Option<String> {
    let notes = contents.get(&clip.content_id)?.notes()?;
    let mut with_lyric: Vec<(f64, &str)> = notes
        .iter()
        .filter_map(|n| n.lyric.as_deref().map(|l| (n.start_beat, l)))
        .filter(|(_, l)| !l.is_empty())
        .collect();
    if with_lyric.is_empty() {
        return None;
    }
    with_lyric.sort_by(|a, b| a.0.total_cmp(&b.0));
    const MAX_CHARS: usize = 32;
    let mut out = String::new();
    let mut count = 0usize;
    for (_, lyric) in with_lyric {
        for ch in lyric.chars() {
            if count >= MAX_CHARS {
                out.push('…');
                return Some(out);
            }
            out.push(ch);
            count += 1;
        }
    }
    Some(out)
}

/// クリップの表示名を導出する (SSoT)。**明示名優先**で Text 本文 → content_name →
/// MIDI 歌詞 → 空 の順で解決する (詳細は旧 arrangement_view の doc 参照)。
pub(crate) fn clip_display_label(clip: &common::model::Clip, song: &common::model::Song) -> Arc<str> {
    if let Some(t) = text_clip_label(clip, &song.clip_contents) {
        return Arc::from(t);
    }
    let explicit = song.content_name(clip.content_id);
    if !explicit.is_empty() {
        return Arc::from(explicit);
    }
    if let Some(l) = lyric_clip_label(clip, &song.clip_contents) {
        return Arc::from(l);
    }
    Arc::from("")
}

// ---------------------------------------------------------------------------
// automation lane の派生
// ---------------------------------------------------------------------------

/// lane build の context (song + 各 lookup 表)。
#[derive(Clone, Copy)]
struct LaneBuildData<'a> {
    song: &'a common::model::Song,
    refcount_by_content: &'a HashMap<common::model::ContentId, usize>,
    lane_height_overrides: &'a HashMap<common::model::AutomationLaneKey, u16>,
    content_names: &'a HashMap<common::model::ContentId, Arc<str>>,
}

fn build_arrangement_automation_lanes(
    track: &common::model::Track,
    data: LaneBuildData<'_>,
    range_of: &dyn Fn(&common::model::AutomationTarget) -> Option<(f64, f64)>,
    param_name_of: &dyn Fn(&common::model::AutomationTarget) -> Option<String>,
) -> Vec<ArrangementAutomationLane> {
    build_arrangement_lanes_from_slice(&track.automation_lanes, track.id, data, range_of, param_name_of)
}

fn build_arrangement_lanes_from_slice(
    lanes: &[common::model::AutomationLane],
    track_id: u32,
    data: LaneBuildData<'_>,
    range_of: &dyn Fn(&common::model::AutomationTarget) -> Option<(f64, f64)>,
    param_name_of: &dyn Fn(&common::model::AutomationTarget) -> Option<String>,
) -> Vec<ArrangementAutomationLane> {
    let LaneBuildData { song, refcount_by_content, lane_height_overrides, content_names } = data;
    lanes
        .iter()
        .map(|lane| {
            let param_name = param_name_of(&lane.target);
            let display = lane_target_display(&lane.target, param_name.as_deref());
            let range = range_of(&lane.target);
            let default_value_norm =
                common::automation::plain_to_norm_ranged(&lane.target, lane.default_value, range);
            let clips: Vec<ArrangementAutomationClip> = lane
                .clips
                .iter()
                .map(|c| {
                    let points: Vec<ArrangementAutomationPoint> = song
                        .clip_contents
                        .get(&c.content_id)
                        .and_then(|cc| cc.automation_points())
                        .unwrap_or(&[])
                        .iter()
                        .map(|p| ArrangementAutomationPoint {
                            // r.md #73: 曲線編集は安定 id で point を指す (不変条件 1)。
                            id: p.id,
                            // r.md #44: widget は clip の窓ローカル座標で描く
                            // (curve は content-local なので窓の offset を引く)。
                            time_beat: p.time_beat - c.content_offset_beats,
                            value_norm: common::automation::plain_to_norm_ranged(
                                &lane.target,
                                p.value,
                                range,
                            ),
                            // r.md #73: 曲線の真実は plain。 `value_norm` は clamp 済の
                            // 画面射影なので、窓の外の値はここからしか復元できない。
                            // **片方から片方を再変換しないこと** (同じ model 点から両方作る)。
                            value_plain: p.value,
                            curve: p.curve,
                        })
                        .collect();
                    ArrangementAutomationClip {
                        id: c.id,
                        start_beat: c.start_beat,
                        len_beats: c.length_beats,
                        name: content_names
                            .get(&c.content_id)
                            .cloned()
                            .unwrap_or_else(|| Arc::from(song.content_name(c.content_id))),
                        points,
                        share_group_color: if refcount_by_content
                            .get(&c.content_id)
                            .copied()
                            .unwrap_or(0)
                            >= 2
                        {
                            Some(content_id_to_hue(c.content_id))
                        } else {
                            None
                        },
                    }
                })
                .collect();
            ArrangementAutomationLane {
                id: lane.id,
                // r.md #73: 曲線を再生と同じ plain 空間で評価するために widget が持つ。
                // `range` は上の `range_of(&lane.target)` と同じもの (= point の
                // `value_norm` / `default_value_norm` と同じ写像を共有する)。
                target: lane.target.clone(),
                plugin_range: range,
                label: display.label,
                icon_glyph: display.icon_glyph,
                color: display.color,
                enabled: lane.enabled,
                visible: lane.visible,
                height_px: lane_height_overrides
                    .get(&common::model::AutomationLaneKey { track: track_id, lane: lane.id })
                    .copied()
                    .unwrap_or(lane.height_px),
                default_value_norm,
                clips,
            }
        })
        .collect()
}

struct LaneDisplay {
    label: Arc<str>,
    icon_glyph: char,
    /// lane の **アイデンティティ色** (「どのパラメータのレーンか」 を運ぶカテゴリ色)。
    ///
    /// r.md #48: テーマ非従属 — テーマを切り替えても Volume は青、Pan は緑のままにする
    /// (ユーザーが color_picker で選ぶトラック色と同じ扱い)。薄い面の上で沈む問題は
    /// トークンを増やして解くのではなく、描画側 (`draw_automation_lane`) が
    /// `Palette::adapt_on` で **色相・彩度を保ったまま明度だけ寄せて** 解く。
    color: Color,
}

thread_local! {
    /// lane label の per-frame `Arc::from` 解消用 intern テーブル。
    static LANE_LABEL_INTERN: std::cell::RefCell<HashMap<Box<str>, Arc<str>>> =
        std::cell::RefCell::new(HashMap::new());
    /// SendGain "Send N" を安定 send id で intern。
    static SEND_LABEL_INTERN: std::cell::RefCell<HashMap<u32, Arc<str>>> =
        std::cell::RefCell::new(HashMap::new());
}

fn intern_label(s: &str) -> Arc<str> {
    LANE_LABEL_INTERN.with(|c| {
        let mut m = c.borrow_mut();
        if let Some(a) = m.get(s) {
            return a.clone();
        }
        let a: Arc<str> = Arc::from(s);
        m.insert(Box::from(s), a.clone());
        a
    })
}

fn intern_send_label(send_id: u32) -> Arc<str> {
    SEND_LABEL_INTERN.with(|c| {
        c.borrow_mut()
            .entry(send_id)
            .or_insert_with(|| Arc::from(format!("Send {send_id}")))
            .clone()
    })
}

fn lane_target_display(
    target: &common::model::AutomationTarget,
    plugin_param_name: Option<&str>,
) -> LaneDisplay {
    use common::model::{AutomationTarget, ImageBuiltinParam, TrackBuiltinParam};
    match target {
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume) => LaneDisplay {
            label: intern_label("Volume"),
            icon_glyph: 'V',
            color: Color::rgb(0.42, 0.78, 0.95),
        },
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan) => LaneDisplay {
            label: intern_label("Pan"),
            icon_glyph: 'P',
            color: Color::rgb(0.55, 0.92, 0.55),
        },
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Mute) => LaneDisplay {
            label: intern_label("Mute"),
            icon_glyph: 'M',
            color: Color::rgb(0.92, 0.45, 0.40),
        },
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain { send_id, .. }) => LaneDisplay {
            label: intern_send_label(*send_id),
            icon_glyph: 'S',
            color: Color::rgb(0.85, 0.75, 0.40),
        },
        AutomationTarget::PluginParam { param_id, .. } => LaneDisplay {
            label: match plugin_param_name {
                Some(name) => intern_label(name),
                None => intern_label(&format!("Param {param_id}")),
            },
            icon_glyph: 'F',
            color: Color::rgb(0.78, 0.55, 0.92),
        },
        AutomationTarget::SongTempo => LaneDisplay {
            label: intern_label("Tempo"),
            icon_glyph: 'T',
            color: Color::rgb(0.95, 0.85, 0.55),
        },
        AutomationTarget::SongTimeSigNumerator => LaneDisplay {
            label: intern_label("Time Sig"),
            icon_glyph: 'T',
            color: Color::rgb(0.95, 0.85, 0.55),
        },
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::X) => LaneDisplay {
            label: intern_label("Image X"),
            icon_glyph: 'X',
            color: Color::rgb(0.90, 0.65, 0.85),
        },
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::Y) => LaneDisplay {
            label: intern_label("Image Y"),
            icon_glyph: 'Y',
            color: Color::rgb(0.90, 0.65, 0.85),
        },
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::W) => LaneDisplay {
            label: intern_label("Image W"),
            icon_glyph: 'W',
            color: Color::rgb(0.85, 0.65, 0.90),
        },
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::H) => LaneDisplay {
            label: intern_label("Image H"),
            icon_glyph: 'H',
            color: Color::rgb(0.85, 0.65, 0.90),
        },
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::Opacity) => LaneDisplay {
            label: intern_label("Image Opacity"),
            icon_glyph: 'O',
            color: Color::rgb(0.92, 0.78, 0.70),
        },
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::Rotation) => LaneDisplay {
            label: intern_label("Image Rotation"),
            icon_glyph: 'R',
            color: Color::rgb(0.75, 0.92, 0.92),
        },
        AutomationTarget::TextBuiltin(p) => {
            use common::model::TextBuiltinParam as T;
            let (label, icon, color): (&'static str, char, Color) = match p {
                T::X => ("Text X", 'X', Color::rgb(0.85, 0.85, 0.65)),
                T::Y => ("Text Y", 'Y', Color::rgb(0.85, 0.85, 0.65)),
                T::W => ("Text W", 'W', Color::rgb(0.80, 0.80, 0.60)),
                T::H => ("Text H", 'H', Color::rgb(0.80, 0.80, 0.60)),
                T::Opacity => ("Text Opacity", 'O', Color::rgb(0.92, 0.85, 0.60)),
                T::Rotation => ("Text Rotation", 'R', Color::rgb(0.65, 0.92, 0.92)),
                T::FontSize => ("Text Size", 'S', Color::rgb(0.88, 0.78, 0.55)),
                T::FillR => ("Text Fill R", 'r', Color::rgb(0.95, 0.55, 0.55)),
                T::FillG => ("Text Fill G", 'g', Color::rgb(0.55, 0.95, 0.55)),
                T::FillB => ("Text Fill B", 'b', Color::rgb(0.55, 0.55, 0.95)),
                T::FillA => ("Text Fill A", 'a', Color::rgb(0.85, 0.85, 0.85)),
                T::OutlineR => ("Text Out R", 'r', Color::rgb(0.85, 0.45, 0.45)),
                T::OutlineG => ("Text Out G", 'g', Color::rgb(0.45, 0.85, 0.45)),
                T::OutlineB => ("Text Out B", 'b', Color::rgb(0.45, 0.45, 0.85)),
                T::OutlineA => ("Text Out A", 'a', Color::rgb(0.75, 0.75, 0.75)),
                T::OutlineWidth => ("Text Out W", 'w', Color::rgb(0.78, 0.65, 0.55)),
                T::ShadowR => ("Text Sh R", 'r', Color::rgb(0.65, 0.40, 0.40)),
                T::ShadowG => ("Text Sh G", 'g', Color::rgb(0.40, 0.65, 0.40)),
                T::ShadowB => ("Text Sh B", 'b', Color::rgb(0.40, 0.40, 0.65)),
                T::ShadowA => ("Text Sh A", 'a', Color::rgb(0.55, 0.55, 0.55)),
                T::ShadowOffsetX => ("Text Sh X", 'x', Color::rgb(0.55, 0.45, 0.45)),
                T::ShadowOffsetY => ("Text Sh Y", 'y', Color::rgb(0.55, 0.45, 0.45)),
                T::ShadowBlur => ("Text Sh Blur", 'B', Color::rgb(0.60, 0.55, 0.50)),
            };
            LaneDisplay { label: intern_label(label), icon_glyph: icon, color }
        }
        AutomationTarget::GroupTransform(p) => {
            use common::model::GroupTransformParam as G;
            let (label, icon, color): (&'static str, char, Color) = match p {
                G::X => ("Group X", 'X', Color::rgb(0.55, 0.85, 0.90)),
                G::Y => ("Group Y", 'Y', Color::rgb(0.55, 0.85, 0.90)),
                G::Rotation => ("Group Rot", 'R', Color::rgb(0.50, 0.80, 0.95)),
                G::ScaleX => ("Group ScaleX", 'x', Color::rgb(0.45, 0.82, 0.82)),
                G::ScaleY => ("Group ScaleY", 'y', Color::rgb(0.45, 0.82, 0.82)),
                G::AnchorX => ("Group AnchorX", 'a', Color::rgb(0.60, 0.78, 0.88)),
                G::AnchorY => ("Group AnchorY", 'a', Color::rgb(0.60, 0.78, 0.88)),
                G::Opacity => ("Group Opacity", 'O', Color::rgb(0.70, 0.80, 0.92)),
            };
            LaneDisplay { label: intern_label(label), icon_glyph: icon, color }
        }
    }
}


