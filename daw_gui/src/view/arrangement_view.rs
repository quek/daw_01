//! Arrangement view (track headers / ruler / lanes / clip drag) を gui_01 の
//! `Ui::arrangement` widget 1 呼び出しに集約。
//!
//! AppData は引き続き track / clip を index ベースで持つので、ここで stable id
//! (Track.id / Clip.id) と index の変換層を担う。

use std::sync::Arc;

use daw_ui_core::{
    ArrangementAutomationClip, ArrangementAutomationLane, ArrangementAutomationPoint,
    ArrangementClip, ArrangementClipAudioEdit, ArrangementCurveKind, ArrangementEditRequest,
    ArrangementStyle, ArrangementTrack, ArrangementView, AutomationClipKey,
    AutomationLaneKey, ChannelLayout, ClipKey, ColorPickerStyle, Edit, FadeCurve as WidgetFadeCurve,
    FadeEdge, SampleSlices, ToggleButtonStyle, Ui, WaveformRenderMode, WaveformSource,
    WaveformStyle, WaveformView,
};
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::app::{AppData, AppEvent, ClipRef, ColorPickerTarget, FadeEdgeKind};
use crate::view::mixer_strips::{amp_to_fader, fader_to_amp};
use crate::view::track_color;
use crate::view::snap::{self, SNAP_LABELS};

/// カーソル / ドロップの Y 座標が乗っている track の `song.tracks` index を、
/// widget が返す実際の header rect (`ArrangementResponse.track_header_rects`)
/// から解決する。各 rect は縦スクロール (`arrange_track_top`) / 個別行高
/// override / master 行 (Reaper 流 at top) を反映した実描画 Y なので、naive な
/// `(y - canvas_top) / row_h` と違い下方 track でも正しく当たる。
///
/// **file drop target と Split (E) の hover clip 判定の両方が使う** (= 行 → track
/// の Y 判定の single source of truth)。別々にコピーすると一方だけ直して
/// off-by-one が残る事故が起きる (実際に発生したため共有 helper に統一)。
/// master 行 (= `song.tracks` に居ない) や、どの行にも当たらない Y は `None`。
fn track_index_at_y(
    track_header_rects: &[(u32, Rect)],
    tracks: &[common::model::Track],
    y: f32,
) -> Option<usize> {
    track_header_rects
        .iter()
        .find(|(_, r)| y >= r.y && y < r.y + r.h)
        .and_then(|(track_id, _)| tracks.iter().position(|t| t.id == *track_id))
}

/// gui_01 widget の `FadeCurve` (#025) ↔ daw_01 model `FadeCurve` の
/// 対応変換。 同 3 種を 1:1 で対応させているだけだが、 type が別 crate
/// なので変換 helper を経由する。
fn widget_curve_from_model(c: common::model::FadeCurve) -> WidgetFadeCurve {
    match c {
        common::model::FadeCurve::Linear => WidgetFadeCurve::Linear,
        common::model::FadeCurve::Exponential => WidgetFadeCurve::Exponential,
        common::model::FadeCurve::SCurve => WidgetFadeCurve::SCurve,
    }
}

fn model_curve_from_widget(c: WidgetFadeCurve) -> common::model::FadeCurve {
    match c {
        WidgetFadeCurve::Linear => common::model::FadeCurve::Linear,
        WidgetFadeCurve::Exponential => common::model::FadeCurve::Exponential,
        WidgetFadeCurve::SCurve => common::model::FadeCurve::SCurve,
    }
}

const TRACK_HEADER_W: f32 = 160.0;
const RULER_H: f32 = 20.0;
const TOOLBAR_H: f32 = 24.0;
const COLOR_TOOLBAR_BG: Color = Color { r: 0.10, g: 0.10, b: 0.12, a: 1.0 };

const SNAP_TOGGLE_STYLE: ToggleButtonStyle = ToggleButtonStyle {
    off_color: Color { r: 0.22, g: 0.22, b: 0.26, a: 1.0 },
    on_color: Color { r: 0.30, g: 0.50, b: 0.70, a: 1.0 },
    border: Color { r: 0.35, g: 0.38, b: 0.45, a: 1.0 },
    border_width: 1.0,
    radius: 3.0,
    font_size: 12.0,
    text_color: Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 },
    on_text_color: None,
};

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    // 上部 24 px を Snap toolbar に。残りを arrangement widget に渡す。
    let toolbar_rect = Rect { x: area.x, y: area.y, w: area.w, h: TOOLBAR_H };
    let body = Rect {
        x: area.x,
        y: area.y + TOOLBAR_H,
        w: area.w,
        h: (area.h - TOOLBAR_H).max(0.0),
    };
    draw_snap_toolbar(app, ui, toolbar_rect);
    let area = body;

    // auto-fit (X キー / Fit ボタン) のために、現フレームの canvas (lanes) サイズを記録。
    let canvas_w = (area.w - TRACK_HEADER_W).max(0.0);
    let canvas_h = (area.h - RULER_H).max(0.0);
    let canvas_size = (canvas_w, canvas_h);
    if app.last_arrange_canvas_size != canvas_size {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.last_arrange_canvas_size = canvas_size;
        }));
    }

    // Phase 6 review perf (E11): 旧コードは毎フレーム per track で
    // `compute_track_depth` (= O(depth*N) parent chain walk) と per clip で
    // `clip_content_refcount` (= O(全クリップ数)) を呼んでいて、 N=50 tracks
    // × 20 clips で N² 級の line scan。 ここで 1 度だけ batch 計算する:
    //   - `id_to_parent`: track_id → parent_id (= depth 計算 O(1) lookup)
    //   - `refcount_by_content`: ContentId → 出現回数 (= refcount O(1) lookup)
    let n_tracks = app.song.tracks.len();
    let mut id_to_parent: std::collections::HashMap<u32, Option<u32>> =
        std::collections::HashMap::with_capacity(n_tracks);
    for t in &app.song.tracks {
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
    let mut refcount_by_content: std::collections::HashMap<common::model::ContentId, usize> =
        std::collections::HashMap::new();
    for t in &app.song.tracks {
        for c in &t.clips {
            *refcount_by_content.entry(c.content_id).or_insert(0) += 1;
        }
        for lane in &t.automation_lanes {
            for c in &lane.clips {
                *refcount_by_content.entry(c.content_id).or_insert(0) += 1;
            }
        }
    }

    // gui_01 #068 連動ハイライト: アクティブな共有グループの content_id 集合
    // = {選択中 clip の content_id} ∪ {前フレーム hover clip の content_id}、
    // refcount>=2 のみ。 各 clip の `in_active_group` 判定に使う。
    let active_groups: std::collections::HashSet<common::model::ContentId> = {
        let mut set = std::collections::HashSet::new();
        let is_shared = |cid: common::model::ContentId| {
            refcount_by_content.get(&cid).copied().unwrap_or(0) >= 2
        };
        for r in &app.selected_clips {
            if let Some(c) = app
                .song
                .tracks
                .get(r.track as usize)
                .and_then(|t| t.clips.get(r.clip as usize))
                && is_shared(c.content_id)
            {
                set.insert(c.content_id);
            }
        }
        if let Some(cid) = app.arrange_hover_content
            && is_shared(cid)
        {
            set.insert(cid);
        }
        set
    };

    let tracks: Vec<ArrangementTrack> = app
        .song
        .tracks
        .iter()
        .map(|t| ArrangementTrack {
            id: t.id,
            // v16 (`docs/plan_text_overlay.md` §1.9): daw_01 側で
            // `TrackKind` を廃止し全 track unified。 widget には clip
            // 種別から推測した kind を渡す: video / image / text clip
            // がある track は Video kind 表示、 そうでなければ Audio
            // (= 既存 Audio header + waveform を表示)。 混在 track は
            // Video kind 優先 (= row 背景 / thumbnail で視認性が高い)。
            kind: if t.clips.iter().any(|c| {
                matches!(
                    app.song.clip_contents.get(&c.content_id),
                    Some(common::model::ClipContent::Video(_))
                        | Some(common::model::ClipContent::Image(_))
                        | Some(common::model::ClipContent::Text(_))
                )
            }) {
                daw_ui_core::widgets::arrangement::TrackKind::Video
            } else {
                daw_ui_core::widgets::arrangement::TrackKind::Audio
            },
            name: Arc::from(t.name.as_str()),
            muted: t.muted,
            solo: t.solo,
            armed: t.armed,
            // mixer fader と同じ dB スケール (DB_MIN..DB_MAX) で表示する。
            // 編集 callback で受け取った値は fader_to_amp で linear に戻す。
            volume: amp_to_fader(t.volume),
            clips: t
                .clips
                .iter()
                .map(|c| ArrangementClip {
                    id: c.id,
                    start_beat: c.start_beat,
                    len_beats: c.length_beats,
                    // docs/plan_text_overlay.md §4 P4: text clip は
                    // event の text 先頭 ~32 文字を clip 上に表示。
                    // 空テキスト or 非 Text variant なら共有名
                    // (`content_id` 単位 SSoT) を fallback。 同 content を
                    // 共有する linked clip は同じ名前を表示する。
                    name: text_clip_label(c, &app.song.clip_contents)
                        .map(Arc::from)
                        .unwrap_or_else(|| Arc::from(app.song.content_name(c.content_id))),
                    // v18 (`docs/plan_track_clip_color.md`): clip は effective
                    // 色 (個別上書き or トラック色継承) で塗る。共有 clip
                    // (refcount >= 2) では widget が `share_group_color` (hue) を
                    // 優先し、この `color` を無視する設計 (arrangement.rs:2621)。
                    color: Some(track_color::to_renderer(
                        track_color::effective_clip_color(t, c),
                    )),
                    // gui_01 #019 (M14 Phase 63e): refcount >= 2 (= 共有 clip)
                    // のときだけ Some(hue)。 widget が hue + style.share_group_S/L
                    // で HSL→RGB 変換してアクセント色 + link glyph を描画。
                    // hue は content_id の golden-ratio hash で `[0.0, 1.0)` に
                    // 一様分布させ、 共有グループ間で色が衝突しにくいようにする。
                    // Phase 6 review perf: 旧 `clip_content_refcount` (= 全 clip
                    // scan) を batch 計算済 `refcount_by_content` の O(1) lookup へ。
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
                    // gui_01 #068: アクティブな共有グループ member なら true。
                    // widget が selection とは別レイヤで hue glow + 太枠を描く。
                    in_active_group: active_groups.contains(&c.content_id),
                    // gui_01 #025 (M14 Phase 63k): audio clip のとき first event の
                    // 値を渡して widget に dB handle / fade 角 grip / envelope を
                    // 描かせる。 Phase 1 で 1 clip 1 event 前提なので first event
                    // を「clip 全体の field」 として表示。 widget は drag release で
                    // `SetClipGainDb` / `SetClipFade` / `SetClipFadeCurve` を発行
                    // するので make_edit 側で受けて AppEvent に変換する。
                    audio_edit: app
                        .song
                        .clip_contents
                        .get(&c.content_id)
                        .and_then(|ct| ct.audio_events())
                        .and_then(|events| events.first())
                        .map(|ev| ArrangementClipAudioEdit {
                            gain_db: ev.gain_db,
                            fade_in_beats: ev.fade_in_beats,
                            fade_out_beats: ev.fade_out_beats,
                            fade_in_curve: widget_curve_from_model(ev.fade_in_curve),
                            fade_out_curve: widget_curve_from_model(ev.fade_out_curve),
                        }),
                    // gui_01 #044 (M14 Phase 72) + docs/plan_video.md P3.6:
                    // ClipContent::Video のクリップなら最初の VideoEvent の
                    // source_id を引いて video_texture_cache から
                    // TextureHandle を、 video_sources から native
                    // (width, height) を取り出す。 widget は dimensions を
                    // aspect-fit 計算に使う (= video_clip_loading 単色描画
                    // からの差分)。 import 直後の 1 フレーム目は cache に
                    // 未登録 (= None で video_clip_loading 単色)、 P3.5 の
                    // runner drain 完了次フレームから thumbnail が出る。
                    thumbnail: {
                        // docs/plan_image_overlay.md §P4: image clips share
                        // the thumbnail slot with video clips. We probe
                        // `ClipContent::Video` first; on miss fall back to
                        // `ClipContent::Image` and look up the
                        // `image_texture_cache`. The two are mutually
                        // exclusive per clip (= one ClipContent variant per
                        // content_id), so this `or_else` chain has no
                        // ambiguity.
                        let content = app.song.clip_contents.get(&c.content_id);
                        content
                            .and_then(|ct| ct.video_events())
                            .and_then(|events| events.first())
                            .and_then(|ev| {
                                let handle =
                                    *app.video_texture_cache.get(&ev.source_id)?;
                                let src = app.song.video_sources.get(&ev.source_id)?;
                                Some((handle, src.width, src.height))
                            })
                            .or_else(|| {
                                let events = content?.image_events()?;
                                let ev = events.first()?;
                                let handle =
                                    *app.image_texture_cache.get(&ev.source_id)?;
                                let src = app.song.image_sources.get(&ev.source_id)?;
                                Some((handle, src.width, src.height))
                            })
                    },
                })
                .collect(),
            // gui_01 #016 で追加された group hierarchy fields:
            // Phase 6 review perf: depth は batch 計算済 `id_to_parent` lookup へ。
            parent_id: t.parent_group_id,
            depth: compute_depth(t.id),
            collapsed: app.collapsed_groups.contains(&t.id),
            // gui_01 #028 (M14 Phase 63n-1): automation lane 行。 daw_01 は
            // 「展開中の track id 集合」 を `expanded_automation_tracks` に持ち、
            // 含まれない track は collapsed。 default 全 collapsed (Bitwig 流)。
            // lane が空の track でも collapsed flag は設定するが、 widget は
            // 「lane 0 件 → disclosure ▶/▼ 非表示」 で扱う。
            automation_lanes_collapsed: !app.expanded_automation_tracks.contains(&t.id),
            automation_lanes: build_arrangement_automation_lanes(t, &app.song),
            // gui_01 #031 (M14 Phase 63n-6): 個別 track row 高さ override。
            // None なら global `view.track_row_h` (= Alt+wheel で動く既存値) を
            // 使う。Some(px) なら override (Alt+drag / 下端 splitter drag で
            // SetSingleTrackRowH を発火 → AppData.track_row_overrides に反映)。
            row_h: app.track_row_overrides.get(&t.id).copied(),
            // v18 (`docs/plan_track_clip_color.md`, gui_01 #059): track の
            // effective 色 (明示上書き or id 由来の導出パレット色)。常に Some を
            // 渡す (widget は header 左端に色ストライプを描く)。
            color: Some(track_color::to_renderer(track_color::effective_track_color(t))),
        })
        .collect();

    // selected_clips: ClipRef (index ベース) → ClipKey (id ベース) 変換。
    // 範囲外の参照は filter_map で取り除く。
    let selected_clips: Vec<ClipKey> = app
        .selected_clips
        .iter()
        .filter_map(|r| {
            let t = app.song.tracks.get(r.track as usize)?;
            let c = t.clips.get(r.clip as usize)?;
            Some(ClipKey { track: t.id, clip: c.id })
        })
        .collect();

    // gui_01 #016 で `selected_track: u32` → `selected_tracks: &[u32]`
    // (track id 列) に変更されたので、 そのまま渡す。
    let selected_tracks: &[u32] = &app.selected_track_ids;

    let zoom = app.arrange_zoom_x.max(1.0);
    let row_h = app.arrange_track_row_h.max(1.0);
    let lanes_w = (area.w - TRACK_HEADER_W).max(1.0);
    let loop_range = if app.song.loop_end_beat > app.song.loop_start_beat {
        Some((app.song.loop_start_beat, app.song.loop_end_beat))
    } else {
        None
    };
    // data_generation: schema 編集を反映する粗粒度 hash。track 数 + clip 総数 +
    // 各 track の name 長さ和 + track 並び順 (id × position) を含めることで
    // reorder でも bump する (selection / drag / playhead では bump しない)。
    let data_generation = (app.song.tracks.len() as u64).wrapping_mul(0x10000)
        + app
            .song
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                ((i as u64).wrapping_mul(31).wrapping_add(t.id as u64 + 1))
                    .wrapping_mul(0x100)
                    + (t.clips.len() as u64)
                    + (t.name.len() as u64)
                    // M10 Phase 47b: track volume も `data_generation` 因子に
                    // (band 表示更新のため、float→bits hash で bump する)。
                    + (t.volume.to_bits() as u64)
            })
            .sum::<u64>();

    let view = ArrangementView {
        start_beat: app.arrange_scroll_beat as f64,
        len_beats: (lanes_w / zoom) as f64,
        track_top: app.arrange_track_top,
        tracks_visible: ((area.h - RULER_H) / row_h).max(1.0),
        track_row_h: row_h,
        header_w: TRACK_HEADER_W,
        ruler_h: RULER_H,
        playhead_beat: app.playhead_beat.map(|b| b as f64),
        loop_range,
        data_generation,
        bpm: app.song.bpm,
        time_sig: app.song.time_sig,
        snap: snap::arrange_snap_config(app),
    };

    // audio_edit が Some の clip に widget が描画する dB handle line (gain_db = 0
    // で clip 中央を貫通する細い水平線) は、 視覚的には波形と被って邪魔になる。
    // hit zone (`audio_db_handle_band_h`) は色と無関係なので、 線色を完全透明に
    // して描画だけ抑制する (drag は引き続き機能する)。
    //
    // clip 名の文字色は gui_01 #060 (Phase 89) で fill 輝度由来の auto-contrast
    // (WCAG relative luminance) が widget 内で default on になったため、 share /
    // selected fill を暗めに override する旧回避策 (~2026-05-09) は撤去。
    // gui_01 default の鮮やかな色 (share L=0.55 / selected yellow) に戻し、
    // 文字色は widget が背景に応じて自動で可読化する。
    let style = ArrangementStyle {
        audio_db_handle_color: Color { r: 0.0, g: 0.0, b: 0.0, a: 0.0 },
        ..ArrangementStyle::default()
    };

    // arrangement widget へ流す Edit 生成。
    // gui_01 #010 (M14 Phase 60) 以降、DoubleClickEmpty.beat / MoveClips / ResizeClips の
    // delta は widget 内で snap 済み (Alt 一時無効化も内部処理) なので、daw_01 側で
    // post-process は不要。

    // gui_01 #028 (M14 Phase 63n-3): 選択中の automation clip を widget 型
    // にそのまま渡す (daw_01 model と field 名一致なので 1:1 cast)。
    let selected_automation_clips_widget: Vec<daw_ui_core::AutomationClipKey> = app
        .selected_automation_clips
        .iter()
        .map(|k| daw_ui_core::AutomationClipKey {
            track: k.track,
            lane: k.lane,
            clip: k.clip,
        })
        .collect();

    // gui_01 #033 (M14 Phase 63n-8): 選択中の automation point を widget 型
    // (`AutomationPointKey { clip: AutomationClipKey, point_idx }`) に変換。
    // daw_01 内部は flat な `AutomationPointKeyRef { track_id, lane_id,
    // clip_id, point_idx }`、 widget は構造化 key を持つので 1:1 写像。
    let selected_automation_points_widget: Vec<daw_ui_core::AutomationPointKey> = app
        .selected_automation_points
        .iter()
        .map(|k| daw_ui_core::AutomationPointKey {
            clip: daw_ui_core::AutomationClipKey {
                track: k.track_id,
                lane: k.lane_id,
                clip: k.clip_id,
            },
            point_idx: k.point_idx,
        })
        .collect();

    // gui_01 #034 (Phase 63n-10): master row を組み立てる。 `Song.song_lanes`
    // (= SongTempo / SongTimeSigNumerator 等) を ArrangementAutomationLane に
    // 変換、 折り畳み状態は `AppData.master_row_automation_expanded` (= UI
    // session state、 negation で widget 側 `automation_lanes_collapsed` に
    // map)。 song_lanes が空でも master_row 自体は表示するため、 常に
    // `Some(...)` を渡す idiom (= None は本機能未使用時用)。
    let master_row_lanes = build_arrangement_lanes_from_slice(&app.song.song_lanes, &app.song);
    let master_row = daw_ui_core::ArrangementMasterRow {
        automation_lanes_collapsed: !app.master_row_automation_expanded,
        automation_lanes: master_row_lanes,
        height_px_override: None,
    };

    let resp = ui.arrangement(
        "arrangement",
        area,
        &tracks,
        view,
        &selected_clips,
        selected_tracks,
        &selected_automation_clips_widget,
        &selected_automation_points_widget,
        &style,
        Some(&master_row),
        make_edit,
    );

    // gui_01 #068 連動ハイライト: 今フレームの hovered clip の content_id を
    // 次フレームの active group 計算用に保持 (変化時のみ Edit を発火、 毎フレーム
    // の無駄な mutate を避ける)。
    let hover_content = resp.hovered_clip.and_then(|k| {
        let t = app.song.tracks.iter().find(|t| t.id == k.track)?;
        t.clips.iter().find(|c| c.id == k.clip).map(|c| c.content_id)
    });
    if hover_content != app.arrange_hover_content {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.arrange_hover_content = hover_content;
        }));
    }

    // gui_01 #020 (M14 Phase 63f): clip 上の右クリックメニュー (Make Unique)。
    // widget が `clip_rects: Vec<(ClipKey, Rect)>` を返してくれるので、
    // track_header_rects と同じパターンで context_menu_for を重ねる。
    // refcount==1 の clip では `MakeClipUnique` handler が「すでに独立 clip」
    // status_message を出すだけなので、 context menu はすべての clip に
    // 同形で出す (条件分岐で項目を省くと UX が分かりにくい)。
    //
    // Phase 1 PR4: audio clip の波形描画も同 loop で重ね描き。
    // Ui::arrangement が描いた clip rect の上に `Ui::waveform` を配置し、
    // ContentId が `ClipContent::Audio` の場合だけ rect 内に波形を表示する。
    // gui_01 #023 で drop position が取れるようになったらここに resolve
    // ロジックも追加する (PR4 範囲外)。
    // Phase 2 PR5: Auto-Fade / Auto-Crossfade を context_menu に追加
    // (`docs/plan_audio_clip.md` §3.5)。 選択 clip 群に対して動くので、
    // 右クリックされた clip 自体の selection を変える/変えないは handler
    // 側に任せる (= MakeClipUnique も同 pattern)。
    // rename overlay 判定用に clip_rename (index ベース ClipRef) を 1 回だけ
    // ClipKey (id ベース) に解決する (selected_clips と同 idiom)。 track rename
    // の renaming_track_id と同パターンで、 ループ内で clip_key_to_ref を毎
    // clip 呼ぶ線形探索を避ける。
    let renaming_clip_key = app.clip_rename.and_then(|r| {
        let t = app.song.tracks.get(r.track as usize)?;
        let c = t.clips.get(r.clip as usize)?;
        Some(ClipKey { track: t.id, clip: c.id })
    });
    let lanes_x = area.x + TRACK_HEADER_W;
    for (clip_key, rect) in &resp.clip_rects {
        let key = *clip_key;
        // color_picker の anchor 用に clip rect を Copy で捕捉 (closure へ move)。
        let menu_rect = *rect;
        ui.context_menu_for(
            *rect,
            &[
                "Rename",
                "Make Unique",
                "共有を一括選択",
                "Auto-Fade",
                "Auto-Crossfade",
                "Reverse",
                "Bounce In Place",
                "Bounce (with FX)",
                "色...",
            ],
            move |idx, ui| {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    let Some(target) = clip_key_to_ref(app, key) else {
                        return;
                    };
                    match idx {
                        // 右クリック対象 clip を inline rename (track rename の
                        // clip 版、 F2 でも起動)。
                        0 => app.handle_event(AppEvent::BeginRenameClip(target)),
                        1 => app.handle_event(AppEvent::MakeClipUnique(target)),
                        // 共有を一括選択: 同 content_id の linked clip group を
                        // まとめて選択 (`docs/plan_clip_shared_name.md` §2)。
                        2 => app.handle_event(AppEvent::SelectLinkedClips(target)),
                        3 => app.handle_event(AppEvent::AutoFadeSelectedClips),
                        4 => app.handle_event(AppEvent::AutoCrossfadeSelectedClips),
                        // Reverse は右クリック対象 clip 1 つだけを toggle
                        // (Auto-Fade と違って selection 全体ではなく当該
                        // clip のみ。 Bitwig clip メニューでも同様)。
                        5 => app.handle_event(AppEvent::ToggleClipReversed(target)),
                        // Bounce In Place: Pre-FX (= plugin chain 通さず)、
                        // 当該 clip の content を 1 event の baked audio に
                        // 置換 (= 元 track 内で同 path)。 Phase 2 PR9
                        // (`docs/plan_audio_clip.md` §3.8)。
                        6 => app.handle_event(AppEvent::BounceClipInPlace(target)),
                        // Bounce (with FX): plugin chain を **通した** 結果を
                        // **新 track + 新 Clip** に書き出す (元 clip は不変)。
                        // async (= IPC freewheel render → 完了通知)。
                        // Phase 2 PR-C (`docs/plan_audio_followup.md`)。
                        7 => app.handle_event(AppEvent::BounceClipWithFx(target)),
                        // v18 (`docs/plan_track_clip_color.md`): color_picker を開く
                        // (anchor = 右クリックした clip rect)。個別 clip 色の上書き。
                        // 「トラック色に戻す」 (継承へ) は Ableton と同様に track 側
                        // context menu (= 全 clip 一括) に置く。
                        8 => app.open_color_picker(ColorPickerTarget::Clip(target), menu_rect),
                        _ => {}
                    }
                }));
            },
        );
        let is_selected = selected_clips.contains(clip_key);
        draw_audio_clip_waveform(app, ui, *clip_key, *rect, lanes_x, is_selected);
        draw_audio_clip_value_overlay(app, ui, *clip_key, *rect);
        draw_midi_clip_notes(app, ui, *clip_key, *rect, lanes_x, is_selected);

        // clip rename mode 中はこの clip rect の上端に text_input を重ね描き。
        // track rename と同 idiom (text_input_at_focused が click で focus 取得、
        // Enter commit / Esc は root の escape handler が CancelRenameClip 発行)。
        // renaming_clip_key (ループ前に 1 回解決した id ベース key) と比較。
        if Some(key) == renaming_clip_key {
            let input_rect = Rect {
                x: rect.x + 2.0,
                y: rect.y + 2.0,
                w: (rect.w - 4.0).max(0.0),
                h: 18.0,
            };
            let edit_resp = ui.text_input_at_focused(
                ("clip_rename", key.track, key.clip),
                input_rect,
                &app.clip_rename_text,
                |new| {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::RenameClipChanged(new.clone()));
                    })
                },
            );
            if edit_resp.committed {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::CommitRenameClip);
                }));
            }
        }
    }

    // gui_01 #028 (M14 Phase 63n-2): automation point 上の右クリック →
    // Hold / Linear / Bezier の curve type popup。 widget が
    // `automation_point_rects: Vec<(AutomationPointKey, Rect)>` を返すので
    // clip_rects と同 idiom で `context_menu_for` を毎 frame 重ねる。
    // popup 選択 → ArrangementCurveKind を `SetAutomationCurveType` に
    // 変換、 prev は popup open 時点の `clip.points[idx].curve` を retrieve。
    //
    // **重要 (visual feedback fix 2026-05-09)**: automation_point_rects は
    // automation_clip_rects と空間的に overlap している (= point は clip
    // 内に居る)。 `context_menu_for` は rect 内右クリックで popup を open
    // するため、 同位置に **両方の popup が同 frame で open される**
    // bug があった。 user が point の "Linear" (idx=1) を click すると
    // clip popup の "Delete" (idx=1) も同時発火 → clip 消失。
    //
    // 対策: point popup を **先に** ループで register し、 同 frame で
    // 右クリックが point rect 上で起きていたら clip popup ループを **skip**
    // する。 これで point popup だけが新規 open され、 clip popup の
    // open_popup が呼ばれない。
    // popup は daw_01 側で完結する (= widget の `ArrangementCurveKind` を
    // 介さず直接 `common::model::AutomationCurve` を構築する)。 gui_01 #033
    // Phase 63n-7 で widget に Exponential variant が追加されたので 4 種
    // 完全描画 + 評価。
    //
    // popup 選択時 default 値:
    //  - Bezier { tension: 0.5 } — 新式 SSoT で `tension=0.0` は Linear と
    //    完全に同じ直線、 「Bezier を選んだのに直線のまま」 という bug-like
    //    UX を避けるため 0.5 (= 中程度の S 字) を default に。
    //  - Exponential { bend: 0.5 } — 同様に、 `bend=0.0` は Linear 等価。
    //    +0.5 で前半遅・後半速 default (Exponential らしい形状をすぐ視認)。
    //
    // 数値を ±1.0 まで動かす UI は Phase 63n-9 (tension/bend handle) で
    // landing 予定。 それまでは popup で curve type を選んで default で固定。
    for (point_key, rect) in &resp.automation_point_rects {
        let key = *point_key;
        ui.context_menu_for(
            *rect,
            &["Hold", "Linear", "Bezier", "Exponential"],
            move |idx, ui| {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    let next = match idx {
                        0 => common::model::AutomationCurve::Hold,
                        1 => common::model::AutomationCurve::Linear,
                        2 => common::model::AutomationCurve::Bezier { tension: 0.5 },
                        3 => common::model::AutomationCurve::Exponential { bend: 0.5 },
                        _ => return,
                    };
                    // prev curve を retrieve (Undo 用)。 lookup できなかった
                    // ら no-op で抜ける (= 編集中に lane / clip が削除された
                    // race を防ぐ)。
                    let prev = app
                        .song
                        .track_by_id(key.clip.track)
                        .and_then(|t| t.lane_by_id(key.clip.lane))
                        .and_then(|l| l.clip_by_id(key.clip.clip))
                        .and_then(|c| app.song.clip_contents.get(&c.content_id))
                        .and_then(|cc| cc.automation_points())
                        .and_then(|pts| pts.get(key.point_idx as usize))
                        .map(|p| p.curve);
                    let Some(prev) = prev else { return };
                    app.handle_event(AppEvent::SetAutomationCurveType {
                        track_id: key.clip.track,
                        lane_id: key.clip.lane,
                        clip_id: key.clip.clip,
                        point_idx: key.point_idx,
                        prev,
                        next,
                    });
                }));
            },
        );
    }

    // gui_01 #028 (M14 Phase 63n-3): automation clip 上の右クリック →
    // Make Unique / Delete。 ただし上で point popup を先に register してい
    // て、 同 frame で右クリックが **point rect 上** だったら clip popup の
    // 登録を skip する (= 同位置で 2 つの popup が同時 open する bug 回避)。
    let pointer = ui.pointer();
    let suppress_clip_menu = pointer.secondary_just_pressed
        && pointer.pos.is_some_and(|(px, py)| {
            resp.automation_point_rects
                .iter()
                .any(|(_, r)| r.contains(px, py))
        });
    if !suppress_clip_menu {
        for (auto_key, rect) in &resp.automation_clip_rects {
            let widget_key = *auto_key;
            ui.context_menu_for(
                *rect,
                &["Make Unique", "Delete"],
                move |idx, ui| {
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        let model_key = common::model::AutomationClipKey {
                            track: widget_key.track,
                            lane: widget_key.lane,
                            clip: widget_key.clip,
                        };
                        match idx {
                            0 => app.handle_event(AppEvent::MakeAutomationClipUnique(
                                model_key,
                            )),
                            1 => app.handle_event(AppEvent::DeleteAutomationClips {
                                keys: vec![model_key],
                            }),
                            _ => {}
                        }
                    }));
                },
            );
        }
    }

    // track header の右クリックメニュー (Rename / Delete) を widget 外で重ねる。
    // widget は track_header_rects と BeginRenameTrack / DeleteTrack の発行までを担う。
    // rename mode 中の track には text_input を rect に重ね描きする。
    let renaming_track_id = app
        .track_rename_idx
        .and_then(|idx| app.song.tracks.get(idx as usize).map(|t| t.id));
    for (track_id, rect) in &resp.track_header_rects {
        let track_id = *track_id;
        let rect = *rect;
        ui.context_menu_for(
            rect,
            &["Rename", "色...", "クリップ色をトラックに揃える", "Delete"],
            move |idx, ui| {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    let Some(t_idx) =
                        app.song.tracks.iter().position(|t| t.id == track_id)
                    else {
                        return;
                    };
                    match idx {
                        0 => app.handle_event(AppEvent::BeginRenameTrack(t_idx as u32)),
                        // v18 (`docs/plan_track_clip_color.md`): color_picker を開く
                        // (anchor = 右クリックした track header rect)。
                        1 => app.open_color_picker(ColorPickerTarget::Track(track_id), rect),
                        // Ableton 流: track の全 clip の色上書きを外して track 色継承に戻す。
                        2 => app.handle_event(AppEvent::ResetTrackClipColors {
                            track: track_id,
                        }),
                        3 => app.handle_event(AppEvent::DeleteTrack(t_idx as u32)),
                        _ => {}
                    }
                }));
            },
        );

        if Some(track_id) == renaming_track_id {
            // text_input は track header rect の上端に被せる (M/S トグル等は隠れる)。
            // text_input widget が click で focus を取る。Enter で commit、Esc は
            // root の escape shortcut handler が CancelRenameTrack を発行する。
            let input_rect = Rect {
                x: rect.x + 2.0,
                y: rect.y + 2.0,
                w: rect.w - 4.0,
                h: 22.0,
            };
            let resp = ui.text_input_at_focused(
                ("track_rename", track_id),
                input_rect,
                &app.track_rename_text,
                |new| {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::RenameTrackChanged(new.clone()));
                    })
                },
            );
            if resp.committed {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::CommitRenameTrack);
                }));
            }
        }
    }

    // v18 (`docs/plan_track_clip_color.md`, gui_01 #058): color_picker overlay。
    // `color_picker_target` が Some の間 1 フレームごとに `ui.color_picker` を
    // 呼んで overlay 描画する。anchor は track header / clip の rect を id で
    // 引き直す (= scroll off で rect が無ければ picker を閉じる)。`picked` を
    // live で model に反映 (open 中 widget 側は current を無視するので flicker
    // しない)、`dismissed` で target を None に戻す。
    render_color_picker_overlay(app, ui);

    // gui_01 #071: 空きレーン右クリック (`SecondaryClickEmpty`) → clip 生成 context menu。
    render_clip_create_menu_overlay(app, ui);

    // file drop の hint frame は widget の上に被せる。canvas (lanes) のみ受け付け。
    let canvas_area = Rect {
        x: area.x + TRACK_HEADER_W,
        y: area.y + RULER_H,
        w: area.w - TRACK_HEADER_W,
        h: area.h - RULER_H,
    };
    if ui.is_file_hovering_in_rect(canvas_area) {
        ui.panel_with_border(
            "arr_file_drop_hint",
            canvas_area,
            Color::TRANSPARENT,
            Color::rgb(0.55, 0.85, 0.95),
            2.0,
            0.0,
        );
    }
    if let Some(drop) = ui.take_file_drop_in_rect(canvas_area) {
        // drop target 解決: drop 位置 (position.y) が乗っている track を、 widget が
        // 返す実際の header rect (`resp.track_header_rects`) で hit-test する。
        // header_rects は縦スクロール (`arrange_track_top`) / 個別行高 override /
        // master 行を反映した実描画 Y なので、 naive な `local_y / row_h` と違い
        // 下方トラックでも正しく当たる (= スクロール時や master 行ぶんのズレで
        // 「Track9 にドロップしても新規 track が作られる」バグの修正)。 lanes 側
        // drop でも各行の Y レンジは header と共通なので Y のみで判定する。 当たった
        // track_id を song.tracks の index に変換し、 master 行 (song.tracks に居ない)
        // や、 どの行にも当たらない (= track の無い下の余白) は None → 新規 track 経路。
        //
        // docs/plan_video.md P2: 同じ drop 内で audio file と video file が
        // 混在する場合は extension で partition して個別 AppEvent を発火する。
        // `import_video::looks_like_video` が `mp4 / mov / mkv / webm / m4v /
        // avi` を判定 (= P2.7 wire)。 マッチしない path は従来通り Audio
        // import パイプラインに流す (= hound の WAV 判定で再度はじかれる)。
        let drop_y = drop.position.1;
        let target_track_idx =
            track_index_at_y(&resp.track_header_rects, &app.song.tracks, drop_y)
                .map(|idx| idx as u32);
        // docs/plan_image_overlay.md P2: 3-way partition (video →
        // image → audio). Video on Windows only (= WMF dependency);
        // image is OS-neutral (image crate); audio is the fallback
        // bucket (hound rejects non-WAV inside `action_import_audio`).
        #[cfg(windows)]
        let (video_paths, non_video_paths): (Vec<_>, Vec<_>) = drop
            .paths
            .into_iter()
            .partition(|p| crate::import_video::looks_like_video(p));
        #[cfg(not(windows))]
        let (video_paths, non_video_paths): (Vec<std::path::PathBuf>, Vec<_>) =
            (Vec::new(), drop.paths);
        let (image_paths, audio_paths): (Vec<_>, Vec<_>) = non_video_paths
            .into_iter()
            .partition(|p| crate::import_image::is_supported_extension(p));

        if !audio_paths.is_empty() {
            let paths = audio_paths;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ImportAudio {
                    paths,
                    target_track_idx,
                });
            }));
        }
        if !video_paths.is_empty() {
            let paths = video_paths;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ImportVideo { paths });
            }));
        }
        if !image_paths.is_empty() {
            let paths = image_paths;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ImportImage { paths, target_track_idx });
            }));
        }
    }

    // Phase 1 PR7 (`docs/plan_audio_clip.md` §3.3): Split (E) は
    // 「マウスカーソル位置」 で分割するため、 毎フレーム mouse pos →
    // beat と (track, clip) を計算して `AppData` に push する。
    // - `arrangement_hover_beat` は snap 適用版 (E 用)
    // - `arrangement_hover_beat_raw` は snap なし版 (Alt+E 用)
    // - `arrangement_hover_clip` はマウスが乗っている clip の (track, clip)
    //   index — Split が selection 不要で動くために使う
    let raw_beat: Option<f64> = ui.pointer().pos.and_then(|(px, py)| {
        if !canvas_area.contains(px, py) {
            return None;
        }
        let beat =
            view.start_beat + ((px - canvas_area.x) as f64 / zoom as f64);
        Some(beat.max(0.0))
    });
    let snapped_beat: Option<f64> = raw_beat
        .map(|raw| view.snap.snap_beat(raw, /* alt: */ false, zoom));
    let hover_clip: Option<ClipRef> = raw_beat.and_then(|beat| {
        // カーソル Y が乗っている track を、 widget が返す実際の header rect
        // (`resp.track_header_rects`) で hit-test する (file drop target と同じ手法)。
        // naive な `(py - canvas_top) / row_h` は master 行 (Reaper 流 at top) /
        // 縦スクロール (`arrange_track_top`) / 個別行高 override ぶんズレ、 Split (E)
        // が 1 つ下の track の clip を対象にしてしまうため使わない。 lanes 側でも
        // 各行の Y レンジは header と共通なので Y のみで判定する。
        let (_, py) = ui.pointer().pos?;
        let track_idx = track_index_at_y(&resp.track_header_rects, &app.song.tracks, py)?;
        let track = app.song.tracks.get(track_idx)?;
        for (clip_idx, clip) in track.clips.iter().enumerate() {
            if beat >= clip.start_beat && beat < clip.start_beat + clip.length_beats {
                return Some(ClipRef {
                    track: track_idx as u32,
                    clip: clip_idx as u32,
                });
            }
        }
        None
    });
    if app.arrangement_hover_beat != snapped_beat
        || app.arrangement_hover_beat_raw != raw_beat
        || app.arrangement_hover_clip != hover_clip
    {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.arrangement_hover_beat = snapped_beat;
            app.arrangement_hover_beat_raw = raw_beat;
            app.arrangement_hover_clip = hover_clip;
        }));
    }
}

/// gui_01 #071 (`docs/plan_text_clip_creation.md`): 空きレーン右クリック
/// (`SecondaryClickEmpty`) で stash した `(track_id, snap 済み beat, 右クリック pos)` を
/// 使い、 毎フレーム `ui.context_menu_at` で `pos` に clip 生成メニューを描画する
/// (REAPER の右クリック空きエリア → Insert new item idiom)。`open_at` は 1-shot flag で
/// 1 フレームだけ `Some(pos)` を渡す (毎フレーム `Some` だと outside-click で閉じても翌
/// フレーム再 open するため)。 項目選択で `AddTextClipAt` を発火して stash を `None` に戻す。
fn render_clip_create_menu_overlay(app: &AppData, ui: &mut Ui<'_, AppData>) {
    let Some((track, beat, pos)) = app.clip_create_menu else {
        return;
    };
    let open_at = if app.clip_create_menu_open {
        Some(pos)
    } else {
        None
    };
    if app.clip_create_menu_open {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.clip_create_menu_open = false;
        }));
    }
    ui.context_menu_at(
        "arrange_clip_create_menu",
        open_at,
        &["Text クリップ"],
        move |idx, ui| {
            if idx == 0 {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::AddTextClipAt {
                        track,
                        start_beat: beat,
                    });
                    app.clip_create_menu = None;
                }));
            }
        },
    );
}

/// v18 (`docs/plan_track_clip_color.md`, gui_01 #058): `color_picker_target` が
/// `Some` の間、保存した anchor (開いた場所 = header / clip / inspector swatch の
/// rect) に color_picker overlay を描画する。`picked` は live で
/// `SetTrackColor`/`SetClipColor` に流す (open 中 widget 側は `current` を無視
/// するので flicker しない)、`dismissed` で target を `None` に戻す。対象 track /
/// clip が削除された (= 現在色を引けない) ときは picker を閉じる。
fn render_color_picker_overlay(app: &AppData, ui: &mut Ui<'_, AppData>) {
    let (Some(target), Some(anchor)) = (app.color_picker_target, app.color_picker_anchor)
    else {
        return;
    };
    let style = ColorPickerStyle::default();
    let palette = track_color::palette_colors();

    // 対象の現在色を引く。対象が消えていれば picker を閉じる。
    let current: Option<Color> = match target {
        ColorPickerTarget::Track(track_id) => app
            .song
            .track_by_id(track_id)
            .map(|t| track_color::to_renderer(track_color::effective_track_color(t))),
        ColorPickerTarget::Clip(clip_ref) => app
            .song
            .tracks
            .get(clip_ref.track as usize)
            .and_then(|t| {
                t.clips.get(clip_ref.clip as usize).map(|c| {
                    track_color::to_renderer(track_color::effective_clip_color(t, c))
                })
            }),
    };

    let Some(current) = current else {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.color_picker_target = None;
        }));
        return;
    };

    let r = ui.color_picker(("arr_color_picker", target_id_hash(target)), anchor, current, &palette, &style);
    if let Some(c) = r.picked {
        let rgb = track_color::from_renderer(c);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| match target {
            ColorPickerTarget::Track(track) => {
                app.handle_event(AppEvent::SetTrackColor { track, color: Some(rgb) });
            }
            ColorPickerTarget::Clip(clip_ref) => {
                app.handle_event(AppEvent::SetClipColor { target: clip_ref, color: Some(rgb) });
            }
        }));
    }
    if r.dismissed {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.color_picker_target = None;
        }));
    }
}

/// color_picker の widget id 用に target を一意な数値へ畳む (track / clip で衝突
/// しないよう track は最上位 bit を立てる)。
fn target_id_hash(target: ColorPickerTarget) -> u64 {
    match target {
        ColorPickerTarget::Track(id) => (1u64 << 63) | id as u64,
        ColorPickerTarget::Clip(r) => ((r.track as u64) << 32) | r.clip as u64,
    }
}

/// arrangement widget からの edit request を AppEvent に変換する。
/// id ベース → index ベース (app の事情) は Edit::mutate 内で行う (ロックフリーで
/// Edit 列をフレーム末尾 apply する gui_01 の流儀に乗る)。
fn make_edit(req: ArrangementEditRequest) -> Edit<AppData> {
    match req {
        // gui_01 #024 (resolved): ruler click / drag で発火する seek
        // 要求。 PR-V4 fix: GUI 側 playhead_beat を更新するだけでなく
        // audio engine にも `MainToChild::SeekTo { samples }` を IPC
        // 送信して再生位置を同期する (= Stop 中も Play 中も click 位置
        // に飛ぶ)。 sample 換算は engine sample rate (= 48000) と
        // song.bpm から `beat × 60 / bpm × sr`。
        ArrangementEditRequest::SetPlayheadBeat(beat) => {
            Edit::mutate(move |app: &mut AppData| {
                let beat = beat.max(0.0);
                app.playhead_beat = Some(beat as f32);
                let sr = common::audio_bridge::SAMPLE_RATE as f64;
                let bpm = app.song.bpm.max(1.0) as f64;
                let samples = (beat * 60.0 / bpm * sr).max(0.0) as u64;
                app.send_audio(common::protocol::MainToChild::SeekTo {
                    samples,
                });
            })
        }
        ArrangementEditRequest::SelectClips { next, .. } => {
            Edit::mutate(move |app: &mut AppData| {
                let next_refs: Vec<ClipRef> =
                    next.iter().filter_map(|key| clip_key_to_ref(app, *key)).collect();
                app.handle_event(AppEvent::SetClipSelection(next_refs));
            })
        }
        ArrangementEditRequest::SelectTrack { next, .. } => {
            // gui_01 #016: multi-select 対応。 widget が決定した
            // `next: Vec<u32>` (id 列、 modifier-aware で Single /
            // RangeFromAnchor / Toggle が解決済) をそのまま反映する。
            Edit::mutate(move |app: &mut AppData| {
                app.selected_track_ids = next.clone();
                // selected_clip / selected_clips も末尾の cursor track
                // 上の clip だけ残す形で同期したいが、 multi-select 中は
                // clip 選択の優先度が低いので暫定的に変更しない。
            })
        }
        // gui_01 #029 (M14 Phase 63n-4): lane body 内 clip ギャップで dblclick
        // → 新規 automation clip 作成。MIDI clip の `DoubleClickEmpty →
        // CreateClip` と同 idiom の lane 版。`start_beat` は widget が snap
        // 適用済、`len_beats` は widget の `automation_clip_default_len_beats`
        // (default 4.0) — caller 側で自前ポリシーに上書きしたければ
        // `ArrangementStyle` を変える。
        ArrangementEditRequest::CreateAutomationClip {
            lane,
            start_beat,
            len_beats,
        } => Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::CreateAutomationClip {
                lane: common::model::AutomationLaneKey {
                    track: lane.track,
                    lane: lane.lane,
                },
                start_beat,
                len_beats,
            });
        }),
        // gui_01 #028 (M14 Phase 63n-1): track 行右端の disclosure ▶/▼ click。
        // automation lane 行の表示・折り畳み toggle を AppEvent に変換。
        ArrangementEditRequest::ToggleTrackAutomationCollapsed { track } => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ToggleTrackAutomationCollapsed {
                    track_id: track,
                });
            })
        }
        // gui_01 #028 (M14 Phase 63n-2): lane header `★` click。
        ArrangementEditRequest::SetLaneEnabled { lane, enabled } => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetLaneEnabled {
                    track_id: lane.track,
                    lane_id: lane.lane,
                    enabled,
                });
            })
        }
        ArrangementEditRequest::SetLaneVisible { lane, visible } => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetLaneVisible {
                    track_id: lane.track,
                    lane_id: lane.lane,
                    visible,
                });
            })
        }
        // lane header default slider drag (live preview + release 確定)。
        // prev / next は normalized、handler 側で plain 化。
        ArrangementEditRequest::SetLaneDefault { lane, prev, next } => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetLaneDefault {
                    track_id: lane.track,
                    lane_id: lane.lane,
                    prev_norm: prev,
                    next_norm: next,
                });
            })
        }
        ArrangementEditRequest::DeleteLane(lane) => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::DeleteLane {
                    track_id: lane.track,
                    lane_id: lane.lane,
                });
            })
        }
        // gui_01 #030 (M14 Phase 63n-5): lane 高さ drag (Alt+drag or
        // 下端 splitter)。 widget 側で min/max clamp 済。
        ArrangementEditRequest::SetLaneHeight { lane, prev, next } => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetLaneHeight {
                    track_id: lane.track,
                    lane_id: lane.lane,
                    prev_px: prev,
                    next_px: next,
                });
            })
        }
        // gui_01 #031 (M14 Phase 63n-6): MIDI track row 高さの個別 drag。
        // Alt+drag or 下端 splitter drag。 既存 Alt+wheel (= SetTrackRowH
        // global) とは独立。 widget 側で min/max clamp 済。
        ArrangementEditRequest::SetSingleTrackRowH { track, prev, next } => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetSingleTrackRowH {
                    track_id: track,
                    prev_px: prev,
                    next_px: next,
                });
            })
        }
        // lane body 内 dblclick で 1 point 追加。time_beat は clip-local、
        // value_norm は normalized。
        ArrangementEditRequest::AddAutomationPoint {
            clip,
            time_beat,
            value_norm,
        } => Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::AddAutomationPoint {
                track_id: clip.track,
                lane_id: clip.lane,
                clip_id: clip.clip,
                time_beat,
                value_norm,
            });
        }),
        // point drag release。同 frame 内 valid な point_idx を gui_01
        // から受け、handler 側で sort 維持しつつ更新。
        ArrangementEditRequest::MoveAutomationPoints(widget_deltas) => {
            Edit::mutate(move |app: &mut AppData| {
                let entries: Vec<crate::app::MoveAutomationPointEntry> = widget_deltas
                    .into_iter()
                    .map(|d| crate::app::MoveAutomationPointEntry {
                        key: crate::app::AutomationPointKeyRef {
                            track_id: d.point.clip.track,
                            lane_id: d.point.clip.lane,
                            clip_id: d.point.clip.clip,
                            point_idx: d.point.point_idx,
                        },
                        prev_time_beat: d.prev_time_beat,
                        prev_value_norm: d.prev_value_norm,
                        next_time_beat: d.next_time_beat,
                        next_value_norm: d.next_value_norm,
                    })
                    .collect();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::MoveAutomationPoints { deltas: entries });
                }
            })
        }
        // Alt+click on point → 即時 1 件削除 (将来は rect select で N 件)。
        ArrangementEditRequest::DeleteAutomationPoints(keys) => {
            Edit::mutate(move |app: &mut AppData| {
                let refs: Vec<crate::app::AutomationPointKeyRef> = keys
                    .into_iter()
                    .map(|k| crate::app::AutomationPointKeyRef {
                        track_id: k.clip.track,
                        lane_id: k.clip.lane,
                        clip_id: k.clip.clip,
                        point_idx: k.point_idx,
                    })
                    .collect();
                if !refs.is_empty() {
                    app.handle_event(AppEvent::DeleteAutomationPoints { points: refs });
                }
            })
        }
        // Right-click curve type popup の選択結果。caller 側で
        // `automation_point_rects` を `context_menu_for` で表示し、
        // 選んだ index → ArrangementCurveKind を gui_01 が返してくる。
        ArrangementEditRequest::SetAutomationCurveType { point, prev, next } => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetAutomationCurveType {
                    track_id: point.clip.track,
                    lane_id: point.clip.lane,
                    clip_id: point.clip.clip,
                    point_idx: point.point_idx,
                    prev: widget_curve_to_model(prev),
                    next: widget_curve_to_model(next),
                });
            })
        }
        // gui_01 #028 (M14 Phase 63n-3): automation clip drag。修飾子の
        // 違いで Move / Linked / Independent の 3 系統。lane 跨ぎは
        // target 不一致でも widget 側は accept、daw_01 側でも
        // §5.4 の通り全 accept (reject / demote しない)。
        ArrangementEditRequest::MoveAutomationClips(widget_deltas) => {
            Edit::mutate(move |app: &mut AppData| {
                let entries = widget_deltas
                    .into_iter()
                    .map(widget_to_model_clip_delta)
                    .collect::<Vec<_>>();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::MoveAutomationClips { deltas: entries });
                }
            })
        }
        ArrangementEditRequest::CloneAutomationClipsLinked(widget_deltas) => {
            Edit::mutate(move |app: &mut AppData| {
                let entries = widget_deltas
                    .into_iter()
                    .map(widget_to_model_clip_delta)
                    .collect::<Vec<_>>();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::CloneAutomationClipsLinked {
                        deltas: entries,
                    });
                }
            })
        }
        ArrangementEditRequest::CloneAutomationClipsIndependent(widget_deltas) => {
            Edit::mutate(move |app: &mut AppData| {
                let entries = widget_deltas
                    .into_iter()
                    .map(widget_to_model_clip_delta)
                    .collect::<Vec<_>>();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::CloneAutomationClipsIndependent {
                        deltas: entries,
                    });
                }
            })
        }
        ArrangementEditRequest::ResizeAutomationClips(widget_deltas) => {
            Edit::mutate(move |app: &mut AppData| {
                let entries = widget_deltas
                    .into_iter()
                    .map(|d| crate::app::ResizeAutomationClipEntry {
                        key: widget_to_model_clip_key(d.key),
                        prev_start: d.prev_start,
                        prev_len: d.prev_len,
                        next_start: d.next_start,
                        next_len: d.next_len,
                    })
                    .collect::<Vec<_>>();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::ResizeAutomationClips { deltas: entries });
                }
            })
        }
        ArrangementEditRequest::DeleteAutomationClips(keys) => {
            Edit::mutate(move |app: &mut AppData| {
                let model_keys: Vec<common::model::AutomationClipKey> =
                    keys.into_iter().map(widget_to_model_clip_key).collect();
                if !model_keys.is_empty() {
                    app.handle_event(AppEvent::DeleteAutomationClips {
                        keys: model_keys,
                    });
                }
            })
        }
        // 短 click on automation clip → selection 上書き。
        ArrangementEditRequest::SelectAutomationClips { prev, next } => {
            Edit::mutate(move |app: &mut AppData| {
                let prev_model: Vec<common::model::AutomationClipKey> =
                    prev.into_iter().map(widget_to_model_clip_key).collect();
                let next_model: Vec<common::model::AutomationClipKey> =
                    next.into_iter().map(widget_to_model_clip_key).collect();
                app.handle_event(AppEvent::SelectAutomationClips {
                    prev: prev_model,
                    next: next_model,
                });
            })
        }
        // gui_01 #033 (M14 Phase 63n-9): Bezier / Exponential curve の中央
        // handle drag release で 1 件発火。 widget は `kind` で 2 種を
        // discriminate するが、 daw_01 側は AppEvent dispatch を簡潔化する
        // ため kind ごとに **別 AppEvent** に分けて変換 (= 既存
        // `SetLaneEnabled` / `SetLaneVisible` 等の「per-field 別 variant」
        // idiom と一致)。 widget で `-1.0..=1.0` clamp 済 (handler 側も
        // defensive で再 clamp)。
        ArrangementEditRequest::SetAutomationCurveParam {
            point,
            kind,
            prev_value,
            next_value,
        } => Edit::mutate(move |app: &mut AppData| {
            let track_id = point.clip.track;
            let lane_id = point.clip.lane;
            let clip_id = point.clip.clip;
            let point_idx = point.point_idx;
            match kind {
                daw_ui_core::SetAutomationCurveParamKind::BezierTension => {
                    app.handle_event(AppEvent::SetAutomationCurveBezierTension {
                        track_id,
                        lane_id,
                        clip_id,
                        point_idx,
                        prev: prev_value,
                        next: next_value,
                    });
                }
                daw_ui_core::SetAutomationCurveParamKind::ExponentialBend => {
                    app.handle_event(AppEvent::SetAutomationCurveExponentialBend {
                        track_id,
                        lane_id,
                        clip_id,
                        point_idx,
                        prev: prev_value,
                        next: next_value,
                    });
                }
            }
        }),
        // gui_01 #033 (M14 Phase 63n-8): lasso 矩形 / 短 click による
        // automation point selection 変更。 widget は zone 排他 lasso (空き
        // zone のみ起動) と modifier 分岐 (修飾なし=replace / Shift=union /
        // Ctrl=XOR) を内包済、 daw_01 側は `prev` / `next` をそのまま
        // `selected_automation_points` に上書きするだけ。
        ArrangementEditRequest::SelectAutomationPoints { prev, next } => {
            Edit::mutate(move |app: &mut AppData| {
                let prev_model: Vec<crate::app::AutomationPointKeyRef> = prev
                    .into_iter()
                    .map(|k| crate::app::AutomationPointKeyRef {
                        track_id: k.clip.track,
                        lane_id: k.clip.lane,
                        clip_id: k.clip.clip,
                        point_idx: k.point_idx,
                    })
                    .collect();
                let next_model: Vec<crate::app::AutomationPointKeyRef> = next
                    .into_iter()
                    .map(|k| crate::app::AutomationPointKeyRef {
                        track_id: k.clip.track,
                        lane_id: k.clip.lane,
                        clip_id: k.clip.clip,
                        point_idx: k.point_idx,
                    })
                    .collect();
                app.handle_event(AppEvent::SelectAutomationPoints {
                    prev: prev_model,
                    next: next_model,
                });
            })
        }
        ArrangementEditRequest::ToggleGroupCollapsed(track_id) => {
            Edit::mutate(move |app: &mut AppData| {
                if app.collapsed_groups.contains(&track_id) {
                    app.collapsed_groups.remove(&track_id);
                } else {
                    app.collapsed_groups.insert(track_id);
                }
            })
        }
        ArrangementEditRequest::SetTrackParent {
            tracks,
            parent,
            anchor_after,
        } => {
            // gui_01 #016 reply: drag&drop reparent + reorder の統合 Edit。
            // 3 段再構築 (a) source tracks を song.tracks から remove
            // (b) parent_group_id を新親に書き換え (c) anchor_after の
            // 直後 (None で先頭) に insert。
            Edit::mutate(move |app: &mut AppData| {
                let mut moved: Vec<common::model::Track> = tracks
                    .iter()
                    .filter_map(|id| {
                        let pos = app.song.tracks.iter().position(|t| t.id == *id)?;
                        Some(app.song.tracks.remove(pos))
                    })
                    .collect();
                if moved.is_empty() {
                    return;
                }
                for t in &mut moved {
                    t.parent_group_id = parent;
                }
                let insert_at = match anchor_after {
                    None => 0,
                    Some(after_id) => app
                        .song
                        .tracks
                        .iter()
                        .position(|t| t.id == after_id)
                        .map(|i| i + 1)
                        .unwrap_or(app.song.tracks.len()),
                };
                for (offset, t) in moved.into_iter().enumerate() {
                    app.song.tracks.insert(insert_at + offset, t);
                }
                app.sync_song_to_plugin_host();
            })
        }
        ArrangementEditRequest::DoubleClickClip(key) => {
            Edit::mutate(move |app: &mut AppData| {
                if let Some(target) = clip_key_to_ref(app, key) {
                    app.handle_event(AppEvent::SelectClip { target, additive: false });
                    // Phase 2 PR6: audio clip は Audio Editor を、 それ以外
                    // (MIDI / Vocal) は Piano Roll を開く (`docs/plan_audio
                    // _clip.md` §3.10)。 bottom_panel 切替は handler 内で
                    // 行われるので、 ここでは AppEvent を発火するだけ。
                    if app.is_audio_clip(target) {
                        app.handle_event(AppEvent::OpenAudioEditor(target));
                    } else {
                        app.handle_event(AppEvent::CloseAudioEditor);
                        app.handle_event(AppEvent::SelectBottomPanel(1));
                    }
                }
            })
        }
        ArrangementEditRequest::DoubleClickEmpty { track, beat } => {
            // gui_01 #010 (M14 Phase 60) 以降、widget 内 snap 済み (Alt 一時無効化込み)。
            let start_beat = beat.max(0.0);
            Edit::mutate(move |app: &mut AppData| {
                if let Some(t_idx) = app.song.tracks.iter().position(|t| t.id == track) {
                    app.handle_event(AppEvent::CreateClip {
                        track: t_idx as u32,
                        start_beat,
                    });
                    app.handle_event(AppEvent::SelectBottomPanel(1));
                }
            })
        }
        ArrangementEditRequest::SecondaryClickEmpty { track, beat, pos } => {
            // gui_01 #071: 空きレーン右クリック → clip 生成 context menu を pos に開く。
            // beat は widget 内で snap 済み (DoubleClickEmpty と同じ、 daw_01 後処理不要)。
            // track は track id。 実メニュー描画は render_clip_create_menu_overlay が
            // `ui.context_menu_at` で毎フレーム行う (color_picker overlay と同 idiom)。
            Edit::mutate(move |app: &mut AppData| {
                app.clip_create_menu = Some((track, beat.max(0.0), pos));
                app.clip_create_menu_open = true;
            })
        }
        ArrangementEditRequest::MoveClips(deltas) => {
            // `MoveClipDelta::to_track` を保持して track 跨ぎ move に対応。
            Edit::mutate(move |app: &mut AppData| {
                let entries: Vec<(ClipRef, u32, f64)> = deltas
                    .iter()
                    .filter_map(|d| {
                        clip_key_to_ref(app, d.from)
                            .map(|r| (r, d.to_track, d.next_start_beat))
                    })
                    .collect();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::SetClipPositions(entries));
                }
            })
        }
        ArrangementEditRequest::CloneClipsLinked(deltas) => {
            // gui_01 #019: Ctrl+drag → release。 元 clip を残し、 各 source の
            // drop 位置に共有コピーを生成。 daw_01 側で `content_id` を流用する。
            // to_track が source の track と異なれば track 跨ぎコピー。
            Edit::mutate(move |app: &mut AppData| {
                let entries: Vec<(ClipRef, u32, f64)> = deltas
                    .iter()
                    .filter_map(|d| {
                        clip_key_to_ref(app, d.from)
                            .map(|r| (r, d.to_track, d.next_start_beat))
                    })
                    .collect();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::CloneClipsLinked(entries));
                }
            })
        }
        ArrangementEditRequest::CloneClipsIndependent(deltas) => {
            // gui_01 #019: Ctrl+Shift+drag → release。 同上だが content を
            // deep clone + 新 ContentId で独立コピー。
            Edit::mutate(move |app: &mut AppData| {
                let entries: Vec<(ClipRef, u32, f64)> = deltas
                    .iter()
                    .filter_map(|d| {
                        clip_key_to_ref(app, d.from)
                            .map(|r| (r, d.to_track, d.next_start_beat))
                    })
                    .collect();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::CloneClipsIndependent(entries));
                }
            })
        }
        ArrangementEditRequest::ResizeClips(deltas) => {
            Edit::mutate(move |app: &mut AppData| {
                for d in &deltas {
                    if let Some(target) = clip_key_to_ref(app, d.key) {
                        // PR4: gui_01 widget は左端 grip drag → next_start
                        // 進、 next_len 縮の delta を、 右端 grip drag →
                        // next_start == prev_start、 next_len 変の delta を
                        // emit する。 daw_01 は両方を `ResizeClip` handler
                        // にそのまま渡し、 audio event の追従は handler 内で
                        // 計算する (`docs/plan_audio_clip.md` §3.2)。
                        app.handle_event(AppEvent::ResizeClip {
                            target,
                            start_beat: d.next_start,
                            length: d.next_len,
                        });
                    }
                }
            })
        }
        ArrangementEditRequest::DeleteClips(keys) => {
            Edit::mutate(move |app: &mut AppData| {
                let refs: Vec<ClipRef> =
                    keys.iter().filter_map(|key| clip_key_to_ref(app, *key)).collect();
                if !refs.is_empty() {
                    app.handle_event(AppEvent::SetClipSelection(refs));
                    app.handle_event(AppEvent::DeleteSelectedClip);
                }
            })
        }
        ArrangementEditRequest::ToggleTrackMute(track_id) => {
            // Phase 6 review: AppEvent / IPC は stable な track_id で識別
            // するように統一済 (= 旧コードは Vec idx を作って渡していた)。
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ToggleTrackMute(track_id));
            })
        }
        ArrangementEditRequest::ToggleTrackSolo(track_id) => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ToggleTrackSolo(track_id));
            })
        }
        ArrangementEditRequest::ToggleTrackArmed(track_id) => {
            // Phase 7 B4 / gui_01 #040 (M14 Phase 68): R button click。
            // mute / solo と完全同 idiom。 master_id への click も widget は
            // 描画するが (= synthesize_master_track で armed: false 固定なので
            // off 表示)、 click が来た場合は AppEvent 経由で track_id 検索 →
            // 一致 track 無し (master_id は song.tracks に居ない) で no-op。
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ToggleTrackArmed(track_id));
            })
        }
        ArrangementEditRequest::DeleteTrack(track_id) => {
            Edit::mutate(move |app: &mut AppData| {
                if let Some(idx) = app.song.tracks.iter().position(|t| t.id == track_id) {
                    app.handle_event(AppEvent::DeleteTrack(idx as u32));
                }
            })
        }
        ArrangementEditRequest::MoveTrackUp(track_id) => {
            Edit::mutate(move |app: &mut AppData| {
                if let Some(idx) = app.song.tracks.iter().position(|t| t.id == track_id) {
                    app.handle_event(AppEvent::MoveTrackUp(idx as u32));
                }
            })
        }
        ArrangementEditRequest::MoveTrackDown(track_id) => {
            Edit::mutate(move |app: &mut AppData| {
                if let Some(idx) = app.song.tracks.iter().position(|t| t.id == track_id) {
                    app.handle_event(AppEvent::MoveTrackDown(idx as u32));
                }
            })
        }
        ArrangementEditRequest::ReorderTracks(order) => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ReorderTracks(order.clone()));
            })
        }
        ArrangementEditRequest::SetTrackVolume { track: track_id, prev: _, next } => {
            // band は dB スケールで表示しているので、widget からは fader-scale value
            // (0..1) が来る。fader_to_amp で linear amp に戻して反映する。
            // Phase 6 review: track_id をそのまま AppEvent に通す (= 旧 idx
            // 経由は不要)。
            let amp = fader_to_amp(next.clamp(0.0, 1.0));
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetTrackVolume {
                    track: track_id,
                    amp,
                });
            })
        }
        ArrangementEditRequest::SetTrackRowH(h) => Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetArrangeTrackRowH(h));
        }),
        ArrangementEditRequest::BeginRenameTrack(track_id) => {
            Edit::mutate(move |app: &mut AppData| {
                if let Some(idx) = app.song.tracks.iter().position(|t| t.id == track_id) {
                    app.handle_event(AppEvent::BeginRenameTrack(idx as u32));
                }
            })
        }
        ArrangementEditRequest::SetLoopRange { start, end } => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetLoopRange { start, end });
            })
        }
        ArrangementEditRequest::SetZoomX(z) => Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetArrangeZoom(z));
        }),
        ArrangementEditRequest::SetScrollX(beat) => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetArrangeScroll(beat as f32));
            })
        }
        ArrangementEditRequest::SetTrackTop(top) => {
            // mouse wheel / scroll bar 経由で widget が発行する縦 scroll。
            // widget が overscroll の scissor も含めて担当 (gui_01 #048
            // で対応依頼)、 daw_01 側は受け取った値をそのまま書き戻す。
            Edit::mutate(move |app: &mut AppData| {
                app.arrange_track_top = top.max(0.0);
            })
        }
        // gui_01 #025 (M14 Phase 63k): clip 上 grip drag 経路。 widget は
        // drag release で 1 度だけ delta 群を発行する。 Phase 2 PR-B で
        // multi-clip 一括 drag を 1 Undo step にまとめるため、 batch
        // AppEvent (`SetClipGainDbBatch` / `SetClipFadeBeatsBatch` /
        // `SetClipFadeCurveBatch`) に変換して 1 度だけ発火する。 Inspector
        // commit 経路の単発 AppEvent (`SetClipGainDb` 等) は引き続き存在。
        ArrangementEditRequest::SetClipGainDb(deltas) => {
            Edit::mutate(move |app: &mut AppData| {
                let entries: Vec<(ClipRef, f32)> = deltas
                    .iter()
                    .filter_map(|d| {
                        clip_key_to_ref(app, d.key).map(|t| (t, d.next_gain_db))
                    })
                    .collect();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::SetClipGainDbBatch(entries));
                }
            })
        }
        ArrangementEditRequest::SetClipFade(deltas) => {
            Edit::mutate(move |app: &mut AppData| {
                let entries: Vec<(ClipRef, FadeEdgeKind, f64)> = deltas
                    .iter()
                    .filter_map(|d| {
                        clip_key_to_ref(app, d.key).map(|t| {
                            let edge = match d.edge {
                                FadeEdge::In => FadeEdgeKind::In,
                                FadeEdge::Out => FadeEdgeKind::Out,
                            };
                            (t, edge, d.next_beats)
                        })
                    })
                    .collect();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::SetClipFadeBeatsBatch(entries));
                }
            })
        }
        ArrangementEditRequest::SetClipFadeCurve(deltas) => {
            Edit::mutate(move |app: &mut AppData| {
                let entries: Vec<(ClipRef, FadeEdgeKind, common::model::FadeCurve)> = deltas
                    .iter()
                    .filter_map(|d| {
                        clip_key_to_ref(app, d.key).map(|t| {
                            let edge = match d.edge {
                                FadeEdge::In => FadeEdgeKind::In,
                                FadeEdge::Out => FadeEdgeKind::Out,
                            };
                            let curve = model_curve_from_widget(d.next_curve);
                            (t, edge, curve)
                        })
                    })
                    .collect();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::SetClipFadeCurveBatch(entries));
                }
            })
        }
    }
}

/// gui_01 #019: 共有 clip 群を視覚区別するためのアクセント色 hue 計算。
/// golden ratio (0.61803...) を掛けて fract で `[0.0, 1.0)` に一様分布させる
/// (連番の content_id でも色が満遍なくバラける)。 widget 側で
/// `ArrangementStyle.share_group_saturation` / `share_group_fill_lightness` /
/// `share_group_border_lightness` と組み合わせて HSL → RGB 変換される。
fn content_id_to_hue(content_id: common::model::ContentId) -> f32 {
    const GOLDEN_RATIO_CONJUGATE: f32 = 0.618_034;
    (content_id as f32 * GOLDEN_RATIO_CONJUGATE).fract()
}

/// docs/plan_text_overlay.md §4 P4: text clip の widget display label。
/// `Text` variant のとき first event の `text` を 32 文字 cap で返す
/// (= 1 行で読める長さ)。 非 Text variant / 空文字 / event 無しは `None`。
/// 戻り値が `Some` なら caller は `clip.name` を上書きしてユーザーに
/// 「実際に何の text が出るか」 を見せる。
fn text_clip_label(
    clip: &common::model::Clip,
    contents: &std::collections::HashMap<common::model::ContentId, common::model::ClipContent>,
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

/// gui_01 #028 (M14 Phase 63n-1): `Track.automation_lanes` を widget が
/// 受け取れる `ArrangementAutomationLane` 列に変換。 各 lane の
/// `target` から label / icon_glyph / color / default_value_norm を導出
/// し、 clip ごとに `Song.clip_contents` から `AutomationContent` を
/// 解決して point 列を normalize する。
///
/// Phase 1 は track-builtin Volume / Pan / Mute のみ実装。 Plugin
/// param / Song-level (tempo / time_sig) は Phase 2+ で IPC 経由の
/// param info を受け取れるようになってから extend する。
fn build_arrangement_automation_lanes(
    track: &common::model::Track,
    song: &common::model::Song,
) -> Vec<ArrangementAutomationLane> {
    build_arrangement_lanes_from_slice(&track.automation_lanes, song)
}

/// gui_01 #034 (Phase 63n-10): `Track.automation_lanes` でも `Song.song_lanes`
/// でも共通に使える slice-based helper。 caller が track 由来か song-level
/// 由来かに関わらず、 同じ idiom で widget input を組み立てる。
fn build_arrangement_lanes_from_slice(
    lanes: &[common::model::AutomationLane],
    song: &common::model::Song,
) -> Vec<ArrangementAutomationLane> {
    lanes
        .iter()
        .map(|lane| {
            let display = lane_target_display(&lane.target);
            let default_value_norm = plain_to_norm(&lane.target, lane.default_value);
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
                            time_beat: p.time_beat,
                            value_norm: plain_to_norm(&lane.target, p.value),
                            curve: model_curve_to_widget(p.curve),
                        })
                        .collect();
                    ArrangementAutomationClip {
                        id: c.id,
                        start_beat: c.start_beat,
                        len_beats: c.length_beats,
                        name: Arc::from(song.content_name(c.content_id)),
                        points,
                        share_group_color: if song.clip_content_refcount(c.content_id) >= 2 {
                            Some(content_id_to_hue(c.content_id))
                        } else {
                            None
                        },
                    }
                })
                .collect();
            ArrangementAutomationLane {
                id: lane.id,
                label: display.label,
                icon_glyph: display.icon_glyph,
                color: display.color,
                enabled: lane.enabled,
                visible: lane.visible,
                height_px: lane.height_px,
                default_value_norm,
                clips,
            }
        })
        .collect()
}

struct LaneDisplay {
    label: Arc<str>,
    icon_glyph: char,
    color: Color,
}

/// `AutomationTarget` ごとの label / icon / 識別色。 label 文字列は
/// `Arc::from` で都度生成 (per-frame だが lane 数は片手で数える程度なので
/// allocation コストは無視できる)。
fn lane_target_display(target: &common::model::AutomationTarget) -> LaneDisplay {
    use common::model::{AutomationTarget, ImageBuiltinParam, TrackBuiltinParam};
    match target {
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume) => LaneDisplay {
            label: Arc::from("Volume"),
            icon_glyph: 'V',
            color: Color::rgb(0.42, 0.78, 0.95),
        },
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan) => LaneDisplay {
            label: Arc::from("Pan"),
            icon_glyph: 'P',
            color: Color::rgb(0.55, 0.92, 0.55),
        },
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Mute) => LaneDisplay {
            label: Arc::from("Mute"),
            icon_glyph: 'M',
            color: Color::rgb(0.92, 0.45, 0.40),
        },
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain { send_idx }) => {
            LaneDisplay {
                label: Arc::from(format!("Send {}", send_idx + 1)),
                icon_glyph: 'S',
                color: Color::rgb(0.85, 0.75, 0.40),
            }
        }
        AutomationTarget::PluginParam { param_id, .. } => LaneDisplay {
            // Phase 2 で IPC param info を受けたら "Cutoff (Serum)" 等に書き換え。
            label: Arc::from(format!("Param {}", param_id)),
            icon_glyph: 'F',
            color: Color::rgb(0.78, 0.55, 0.92),
        },
        AutomationTarget::SongTempo => LaneDisplay {
            label: Arc::from("Tempo"),
            icon_glyph: 'T',
            color: Color::rgb(0.95, 0.85, 0.55),
        },
        AutomationTarget::SongTimeSigNumerator => LaneDisplay {
            label: Arc::from("Time Sig"),
            icon_glyph: 'T',
            color: Color::rgb(0.95, 0.85, 0.55),
        },
        // Image PiP field の lane。 色は image track の clip 背景色系
        // (薄い藤色) で統一、 icon は field 名の頭文字。
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::X) => LaneDisplay {
            label: Arc::from("Image X"),
            icon_glyph: 'X',
            color: Color::rgb(0.90, 0.65, 0.85),
        },
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::Y) => LaneDisplay {
            label: Arc::from("Image Y"),
            icon_glyph: 'Y',
            color: Color::rgb(0.90, 0.65, 0.85),
        },
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::W) => LaneDisplay {
            label: Arc::from("Image W"),
            icon_glyph: 'W',
            color: Color::rgb(0.85, 0.65, 0.90),
        },
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::H) => LaneDisplay {
            label: Arc::from("Image H"),
            icon_glyph: 'H',
            color: Color::rgb(0.85, 0.65, 0.90),
        },
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::Opacity) => LaneDisplay {
            label: Arc::from("Image Opacity"),
            icon_glyph: 'O',
            color: Color::rgb(0.92, 0.78, 0.70),
        },
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::Rotation) => LaneDisplay {
            label: Arc::from("Image Rotation"),
            icon_glyph: 'R',
            color: Color::rgb(0.75, 0.92, 0.92),
        },
        AutomationTarget::TextBuiltin(p) => {
            use common::model::TextBuiltinParam as T;
            // text 系 lane 全 23 variant 用 display。 色は「位置 4 / 形 3
            // / fill 4 / outline 5 / shadow 7」 を section ごとに統一。
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
            LaneDisplay {
                label: Arc::from(label),
                icon_glyph: icon,
                color,
            }
        }
        AutomationTarget::GroupTransform(p) => {
            use common::model::GroupTransformParam as G;
            // 立ち絵グループ transform lane。 色は青緑系で統一、 icon は field 頭文字。
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
            LaneDisplay {
                label: Arc::from(label),
                icon_glyph: icon,
                color,
            }
        }
    }
}

/// Plain (target's native unit) → normalized 0..1 で widget に渡すため
/// の変換。 Phase 1 で必要な範囲のみ実装。 plugin param は min/max を
/// 知らないと正規化できないので、 とりあえず `clamp(0, 1)` で渡す
/// (Phase 2 で `AppData.plugin_params` lookup に置換)。
fn plain_to_norm(target: &common::model::AutomationTarget, plain: f64) -> f32 {
    use common::model::{AutomationTarget, ImageBuiltinParam, TrackBuiltinParam};
    let v = match target {
        // Track.volume は通常 0.0..=2.0 で扱う (1.0 = unity、 amp_to_fader
        // を通せば dB 表現)。 widget の slider band も 0..1 範囲なので
        // 1/2 で normalize。 fader 表示としては不正確だが、 lane の
        // default_value_norm は単に slider 帯の位置決めなので OK。
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume) => plain / 2.0,
        // Pan は -1..=1 → 0..1。
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan) => (plain + 1.0) / 2.0,
        // Mute は bool 相当 (0 or 1)。
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Mute) => {
            if plain >= 0.5 {
                1.0
            } else {
                0.0
            }
        }
        // SendGain も 0..2 と仮定。
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain { .. }) => plain / 2.0,
        // PluginParam は min/max 不明 → そのまま clamp。
        AutomationTarget::PluginParam { .. } => plain,
        // Song-level: Phase 5 で実装。 適当に 0 を返す。
        AutomationTarget::SongTempo | AutomationTarget::SongTimeSigNumerator => 0.0,
        // Image PiP Rotation のみ Pan 同 idiom で normalize、 残りは恒等。
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::Rotation) => {
            (plain + std::f64::consts::PI) / (2.0 * std::f64::consts::PI)
        }
        AutomationTarget::ImageBuiltin(_) => plain,
        // Text Builtin: Rotation のみ Pan idiom、 残りは plain と norm が
        // 同単位 (= image と同 idiom)。
        AutomationTarget::TextBuiltin(common::model::TextBuiltinParam::Rotation) => {
            (plain + std::f64::consts::PI) / (2.0 * std::f64::consts::PI)
        }
        AutomationTarget::TextBuiltin(_) => plain,
        // Group transform: common::automation::plain_to_norm と厳密一致させる
        // (UI ↔ engine 正規化の SSoT。位置/アンカー/Opacity 恒等、Rotation は Pan
        // idiom、ScaleX/ScaleY は 0.1..=10 log space)。
        AutomationTarget::GroupTransform(common::model::GroupTransformParam::Rotation) => {
            (plain + std::f64::consts::PI) / (2.0 * std::f64::consts::PI)
        }
        AutomationTarget::GroupTransform(
            common::model::GroupTransformParam::ScaleX
            | common::model::GroupTransformParam::ScaleY,
        ) => (plain.clamp(0.1, 10.0) / 0.1).ln() / 100.0_f64.ln(),
        AutomationTarget::GroupTransform(_) => plain,
    };
    v.clamp(0.0, 1.0) as f32
}

/// `common::model::AutomationCurve` (incoming curve) → widget
/// `ArrangementCurveKind` の対応変換。 gui_01 #033 Phase 63n-7 で widget
/// 側に `Exponential` variant が追加されたので 4 種完全変換。
fn model_curve_to_widget(c: common::model::AutomationCurve) -> ArrangementCurveKind {
    use common::model::AutomationCurve;
    match c {
        AutomationCurve::Hold => ArrangementCurveKind::Hold,
        AutomationCurve::Linear => ArrangementCurveKind::Linear,
        AutomationCurve::Bezier { tension } => ArrangementCurveKind::Bezier { tension },
        AutomationCurve::Exponential { bend } => ArrangementCurveKind::Exponential { bend },
    }
}

/// 逆変換。 widget が popup で返してきた `ArrangementCurveKind` を
/// model の `AutomationCurve` に戻す。 4 種 1:1。
fn widget_curve_to_model(c: ArrangementCurveKind) -> common::model::AutomationCurve {
    use common::model::AutomationCurve;
    match c {
        ArrangementCurveKind::Hold => AutomationCurve::Hold,
        ArrangementCurveKind::Linear => AutomationCurve::Linear,
        ArrangementCurveKind::Bezier { tension } => AutomationCurve::Bezier { tension },
        ArrangementCurveKind::Exponential { bend } => AutomationCurve::Exponential { bend },
    }
}

/// widget の `AutomationClipKey { track, lane, clip }` → model の同名
/// 構造体。field 名が一致するので 1:1 cast。
fn widget_to_model_clip_key(k: AutomationClipKey) -> common::model::AutomationClipKey {
    common::model::AutomationClipKey {
        track: k.track,
        lane: k.lane,
        clip: k.clip,
    }
}

/// widget の `AutomationLaneKey { track, lane }` → model の同名構造体。
fn widget_to_model_lane_key(k: AutomationLaneKey) -> common::model::AutomationLaneKey {
    common::model::AutomationLaneKey {
        track: k.track,
        lane: k.lane,
    }
}

/// gui_01 #028 (Phase 63n-3): widget の `MoveAutomationClipDelta` を
/// daw_01 内部の `MoveAutomationClipEntry` に変換。 field 名はほぼ
/// 同形なので逐一 copy するだけ。
fn widget_to_model_clip_delta(
    d: daw_ui_core::MoveAutomationClipDelta,
) -> crate::app::MoveAutomationClipEntry {
    crate::app::MoveAutomationClipEntry {
        from: widget_to_model_clip_key(d.from),
        to_lane: widget_to_model_lane_key(d.to_lane),
        prev_start_beat: d.prev_start_beat,
        next_start_beat: d.next_start_beat,
    }
}

fn clip_key_to_ref(app: &AppData, key: ClipKey) -> Option<ClipRef> {
    let t_idx = app.song.tracks.iter().position(|t| t.id == key.track)?;
    let c_idx = app.song.tracks[t_idx].clips.iter().position(|c| c.id == key.clip)?;
    Some(ClipRef { track: t_idx as u32, clip: c_idx as u32 })
}

/// 上部 24 px の Snap toolbar を描画。
/// 配置: [Snap toggle 60px] [snap unit dropdown 90px] [Fit button 50px]
fn draw_snap_toolbar(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect) {
    ui.panel("arr_toolbar_bg", rect, COLOR_TOOLBAR_BG, 0.0);

    let pad = 6.0;
    let h = 18.0;
    let y = rect.y + (rect.h - h) * 0.5;

    let toggle_w = 60.0;
    let dropdown_w = 90.0;
    let fit_w = 50.0;

    let toggle_rect = Rect { x: rect.x + pad, y, w: toggle_w, h };
    let dropdown_rect = Rect {
        x: toggle_rect.x + toggle_rect.w + pad,
        y,
        w: dropdown_w,
        h,
    };
    let fit_rect = Rect {
        x: dropdown_rect.x + dropdown_rect.w + pad,
        y,
        w: fit_w,
        h,
    };

    ui.toggle_button_at(
        "arr_snap_toggle",
        "Snap",
        toggle_rect,
        app.arrange_snap_enabled,
        &SNAP_TOGGLE_STYLE,
        |new| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetArrangeSnapEnabled(new));
            })
        },
    );

    if let Some(idx) = ui.dropdown(
        "arr_snap_unit",
        dropdown_rect,
        SNAP_LABELS,
        app.arrange_snap_choice as usize,
    ) {
        let new = idx as u8;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetArrangeSnapChoice(new));
        }));
    }

    ui.button_at("arr_fit", "Fit", fit_rect, || {
        Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::FitArrangeToContent);
        })
    });
}

// ---------------------------------------------------------------------------
// Phase 1 PR4: audio clip 内の波形描画 (`Ui::waveform` を clip rect に重ねる)
// ---------------------------------------------------------------------------

/// 1 つの audio clip rect 内に波形を描く。 ContentId が `ClipContent::Audio`
/// でなければ何もしない (= MIDI / Vocal clip は通常通り描画される)。
///
/// `Ui::arrangement` は clip rect (border + name) を既に描画しているので、
/// その中に少し margin を取って `Ui::waveform` を呼ぶ。 PR4 段階では 1
/// clip = 1 event を前提 (Audio Editor で複数 event を編集できるのは
/// Phase 2 / PR8)。 source 参照が `audio_source_cache` に無い (=
/// 起動直後でまだ decode されていない / missing source) clip は無音で表示
/// される (波形なし)。
fn draw_audio_clip_waveform(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    clip_key: ClipKey,
    clip_rect: Rect,
    lanes_x: f32,
    is_selected: bool,
) {
    // Phase 6 review (debug): user 報告「波形が表示されないクリップがある」
    // の原因特定のため、 早期 return パスに warn ログを置く。 原因が分かっ
    // たら downgrade / 削除する。
    let Some(t_idx) = app.song.tracks.iter().position(|t| t.id == clip_key.track) else {
        tracing::warn!(?clip_key, "draw_audio_clip_waveform: track not found");
        return;
    };
    let Some(c_idx) = app.song.tracks[t_idx]
        .clips
        .iter()
        .position(|c| c.id == clip_key.clip)
    else {
        tracing::warn!(?clip_key, "draw_audio_clip_waveform: clip not found");
        return;
    };
    let clip = &app.song.tracks[t_idx].clips[c_idx];
    let Some(content) = app.song.clip_contents.get(&clip.content_id) else {
        tracing::warn!(
            ?clip_key,
            content_id = clip.content_id,
            "draw_audio_clip_waveform: content not found"
        );
        return;
    };
    let Some(events) = content.audio_events() else {
        // MIDI / Vocal clip は対象外 (= normal、 log しない)
        return;
    };
    let Some(event) = events.first() else {
        tracing::warn!(
            ?clip_key,
            content_id = clip.content_id,
            "draw_audio_clip_waveform: audio content has empty events"
        );
        return;
    };
    let Some(buffer) = app.audio_source_cache.get(event.source_id) else {
        tracing::warn!(
            ?clip_key,
            content_id = clip.content_id,
            source_id = event.source_id,
            "draw_audio_clip_waveform: source not in audio_source_cache (decode 未実行 / failed?)"
        );
        return;
    };

    // clip rect は widget が border + name 領域を含めて描画している。
    // 波形は内側 padding を取って描く: 上部 14 px (= name)、 左右 2 px。
    let inset_top: f32 = 14.0;
    let inset_lr: f32 = 2.0;
    let mut view_rect = Rect {
        x: clip_rect.x + inset_lr,
        y: clip_rect.y + inset_top,
        w: (clip_rect.w - inset_lr * 2.0).max(0.0),
        h: (clip_rect.h - inset_top - inset_lr).max(0.0),
    };
    // arrangement widget は viewport の左端より早く始まる clip も full rect で
    // 返してくる (部分カリング rect は culled せず caller 側で扱う仕様)。
    // そのまま `Ui::waveform` に渡すと track header 領域まで波形が伸びるため、
    // lanes_x で左端を clamp し、削った分の frame だけ start_sample をシフトする。
    let event_len_frames = event
        .source_end_frames
        .saturating_sub(event.source_start_frames);
    let mut view_start_sample = event.source_start_frames;
    let mut view_len_samples = event_len_frames.max(1);
    if view_rect.x < lanes_x {
        let cut_px = lanes_x - view_rect.x;
        if cut_px >= view_rect.w {
            tracing::debug!(
                ?clip_key,
                cut_px,
                w = view_rect.w,
                "draw_audio_clip_waveform: clip 完全に lanes 外"
            );
            return;
        }
        let frames_per_px = (event_len_frames as f64 / clip_rect.w.max(1.0) as f64).max(0.0);
        let skip_frames = (cut_px as f64 * frames_per_px) as u64;
        view_start_sample = view_start_sample.saturating_add(skip_frames);
        view_len_samples = view_len_samples.saturating_sub(skip_frames).max(1);
        view_rect.x = lanes_x;
        view_rect.w -= cut_px;
    }
    if view_rect.w <= 0.0 || view_rect.h <= 0.0 {
        tracing::warn!(
            ?clip_key,
            w = view_rect.w,
            h = view_rect.h,
            clip_w = clip_rect.w,
            clip_h = clip_rect.h,
            "draw_audio_clip_waveform: view_rect 寸法 0 以下 (= 描画スキップ)"
        );
        return;
    }

    // SampleSlices::Planar 用に &[&[f32]] スライスを作る (毎フレーム
    // alloc は許容、 RT path ではなく GUI 描画 path)。
    let planes_borrowed: Vec<&[f32]> = buffer.samples.iter().map(Vec::as_slice).collect();

    let source = WaveformSource {
        samples: SampleSlices::Planar(&planes_borrowed),
        valid_len: buffer.frames as usize,
        generation: event.source_id as u64,
        sample_rate: buffer.sample_rate,
    };
    let view = WaveformView {
        start_sample: view_start_sample,
        len_samples: view_len_samples,
        vertical_gain: 1.0,
    };
    // 選択 clip 背景は黄色 (clip_selected_fill = rgb(1.0, 0.85, 0.30)) なので、
    // 通常時の水色波形だと視認性が悪い。 選択時は濃紺に切り替える。
    let (fg, fg_clipped) = if is_selected {
        (
            Color::rgba(0.05, 0.10, 0.25, 0.95),
            Color::rgb(0.55, 0.05, 0.05),
        )
    } else {
        (
            Color::rgba(0.55, 0.85, 0.95, 0.85),
            Color::rgb(0.95, 0.45, 0.40),
        )
    };
    let style = WaveformStyle {
        fg,
        fg_clipped,
        fill: None,
        baseline: None,
        channel_layout: ChannelLayout::Overlay,
        render_mode: WaveformRenderMode::Auto,
        line_width_px: 1.0,
    };
    let _ = ui.waveform(
        ("audio_clip_wf", clip_key.track, clip_key.clip),
        view_rect,
        source,
        view,
        style,
    );
}

// ---------------------------------------------------------------------------
// Phase 2 PR8: audio clip rect 上に値ラベルを重ね描き (read-only feedback)
// ---------------------------------------------------------------------------

/// audio clip 上に「Gain dB / Fade In / Fade Out」 を small font で
/// オーバーレイ表示する。 grip drag UI (gui_01 #025) が来るまでの
/// 視覚 feedback として、 ユーザーが Inspector に行かなくても値が
/// 確認できるようにする。 値が default (0 dB / 0 fade) の clip では
/// 描かない (= 視覚ノイズを抑える)。 clip rect が 60 px より狭い場合も
/// 描かない (= ラベルが入らない)。
fn draw_audio_clip_value_overlay(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    clip_key: ClipKey,
    clip_rect: Rect,
) {
    if clip_rect.w < 60.0 || clip_rect.h < 24.0 {
        return;
    }
    let Some(t_idx) = app.song.tracks.iter().position(|t| t.id == clip_key.track) else {
        return;
    };
    let Some(c_idx) = app.song.tracks[t_idx]
        .clips
        .iter()
        .position(|c| c.id == clip_key.clip)
    else {
        return;
    };
    let clip = &app.song.tracks[t_idx].clips[c_idx];
    let Some(content) = app.song.clip_contents.get(&clip.content_id) else {
        return;
    };
    let Some(events) = content.audio_events() else {
        return;
    };
    let Some(event) = events.first() else {
        return;
    };

    // Default 値は無表示 (= clip 名で混雑するのを避ける)。
    let show_gain = event.gain_db.abs() > 0.05;
    let show_fade_in = event.fade_in_beats > 0.0;
    let show_fade_out = event.fade_out_beats > 0.0;
    if !(show_gain || show_fade_in || show_fade_out) {
        return;
    }

    // 描画位置: clip rect の右下 (= name は左上、 重ならないように)。
    let text_color = Color::rgba(0.85, 0.92, 1.0, 0.85);
    let font_size = 9.0;
    let pad = 3.0;
    let mut x_right = clip_rect.x + clip_rect.w - pad;
    let y = clip_rect.y + clip_rect.h - font_size - 2.0;

    // 右から左に並べる: [Fade Out] [Fade In] [Gain]。 push_text を
    // 使うと汎用 left-anchored だが、 ここでは y_baseline と x_left
    // で十分 (右端揃えは label 幅推定で代替)。 簡易: 全部左揃えで
    // 順に出す + 適当な width 推定で右端から逆配置。
    if show_fade_out {
        let s = format!("Fo {:.2}b", event.fade_out_beats);
        let w = (s.chars().count() as f32) * (font_size * 0.55);
        x_right -= w;
        ui.label_at(
            ("audio_clip_lbl_fo", clip_key.track, clip_key.clip),
            &s,
            x_right,
            y,
            font_size,
            text_color,
        );
        x_right -= 6.0;
    }
    if show_fade_in {
        let s = format!("Fi {:.2}b", event.fade_in_beats);
        let w = (s.chars().count() as f32) * (font_size * 0.55);
        x_right -= w;
        ui.label_at(
            ("audio_clip_lbl_fi", clip_key.track, clip_key.clip),
            &s,
            x_right,
            y,
            font_size,
            text_color,
        );
        x_right -= 6.0;
    }
    if show_gain {
        let s = format!("{:+.1} dB", event.gain_db);
        let w = (s.chars().count() as f32) * (font_size * 0.55);
        x_right -= w;
        ui.label_at(
            ("audio_clip_lbl_gain", clip_key.track, clip_key.clip),
            &s,
            x_right,
            y,
            font_size,
            text_color,
        );
    }
}

// ---------------------------------------------------------------------------
// MIDI clip mini piano-roll overlay
// ---------------------------------------------------------------------------

/// MIDI clip rect 内に各ノートを小さな矩形で重ね描き (Ardour / REAPER /
/// Bitwig 等の標準的な mini piano-roll プレビュー)。 Audio waveform overlay
/// と同じ pattern で `Ui::push_rect` を per-note 呼ぶ。
///
/// - 横軸: clip-local beats → px (clip 全幅を `length_beats` で割る)
/// - 縦軸: pitch、 表示 range は clip 内 notes の min/max を 2 半音 padding
///   した auto-fit (空クリップは無描画)。 上に行くほど高音。
/// - 色: 共通の light-blue。 velocity は alpha (0.5..1.0) に反映。
/// - lanes_x で左端 clamp、 clip 右端で右端 trim。 hit-test は ArrangementView
///   側の clip rect が担当するので、 ここはピュア描画のみ。
fn draw_midi_clip_notes(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    clip_key: ClipKey,
    clip_rect: Rect,
    lanes_x: f32,
    is_selected: bool,
) {
    let Some(t_idx) = app.song.tracks.iter().position(|t| t.id == clip_key.track) else {
        return;
    };
    let Some(c_idx) = app.song.tracks[t_idx]
        .clips
        .iter()
        .position(|c| c.id == clip_key.clip)
    else {
        return;
    };
    let clip = &app.song.tracks[t_idx].clips[c_idx];
    let Some(content) = app.song.clip_contents.get(&clip.content_id) else {
        return;
    };
    let Some(notes) = content.notes() else {
        // Audio / Automation: 対象外。
        return;
    };
    if notes.is_empty() {
        return;
    }

    // clip 名 (上端) を avoid して内側 padding を取る。 audio waveform と
    // 同じ inset (top 14 / lr 2 / bottom 2) で視覚的に一致させる。
    let inset_top: f32 = 14.0;
    let inset_lr: f32 = 2.0;
    let inset_bottom: f32 = 2.0;
    let view_rect = Rect {
        x: clip_rect.x + inset_lr,
        y: clip_rect.y + inset_top,
        w: (clip_rect.w - inset_lr * 2.0).max(0.0),
        h: (clip_rect.h - inset_top - inset_bottom).max(0.0),
    };
    if view_rect.w <= 0.0 || view_rect.h <= 0.0 {
        return;
    }

    // 左端クリップ (track header 領域へのはみ出し防止)。 上下のはみ出しは
    // ArrangementView の track row で既にクリップされているので不要。
    let visible_left = lanes_x.max(view_rect.x);
    let visible_right = view_rect.x + view_rect.w;
    if visible_right <= visible_left {
        return;
    }

    // pitch auto-fit: clip 内 notes の min/max + 2 半音 padding。 全ノート
    // 同 pitch のときも row_h が 0 にならないよう span は 1 でクランプ。
    let mut min_pitch: u8 = 127;
    let mut max_pitch: u8 = 0;
    for n in notes {
        if n.pitch < min_pitch {
            min_pitch = n.pitch;
        }
        if n.pitch > max_pitch {
            max_pitch = n.pitch;
        }
    }
    let pad: u8 = 2;
    let min_p = min_pitch.saturating_sub(pad);
    let max_p = max_pitch.saturating_add(pad).min(127);
    let pitch_span = (max_p as i32 - min_p as i32).max(1) as f32;
    let row_h = (view_rect.h / pitch_span).max(1.0);

    let base_fill = if is_selected {
        Color::rgba(0.10, 0.15, 0.30, 1.0)
    } else {
        Color::rgba(0.55, 0.85, 0.95, 1.0)
    };

    let clip_len_beats = clip.length_beats.max(0.0001) as f32;
    let px_per_beat = view_rect.w / clip_len_beats;

    for n in notes {
        let nx = view_rect.x + (n.start_beat as f32) * px_per_beat;
        let nw = ((n.duration_beats as f32) * px_per_beat).max(1.0);
        let drawn_x = nx.max(visible_left);
        let drawn_x_end = (nx + nw).min(visible_right);
        if drawn_x_end <= drawn_x {
            continue;
        }
        let row_from_top = (max_p as i32 - n.pitch as i32).clamp(0, pitch_span as i32) as f32;
        let ny = view_rect.y + row_from_top * row_h;
        if ny + row_h <= view_rect.y || ny >= view_rect.y + view_rect.h {
            continue;
        }

        let mut fill = base_fill;
        // velocity 0..=127 → alpha 0.5..=1.0。 0 (rest) は可視性最小、
        // 最大は不透明。
        let v = (n.velocity as f32 / 127.0).clamp(0.0, 1.0);
        fill.a = 0.5 + v * 0.5;

        ui.push_rect(RectCommand {
            rect: Rect {
                x: drawn_x,
                y: ny,
                w: (drawn_x_end - drawn_x).max(1.0),
                h: row_h.max(1.0),
            },
            fill,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::track_index_at_y;
    use common::model::Track;
    use daw_ui_renderer::Rect;

    fn rect_at(y: f32, h: f32) -> Rect {
        Rect { x: 0.0, y, w: 100.0, h }
    }

    /// off-by-one 回帰固定: arrangement 最上段に master 行 (Reaper 流) があると、
    /// 「画面上の行番号」を index にする naive 方式では song.tracks[k] が +1 ずれて
    /// 1 つ下の track を指し、最下段は範囲外になる。実際の header rect を hit-test
    /// する `track_index_at_y` はこのズレを起こさないことを固定する。
    #[test]
    fn track_index_at_y_maps_via_actual_rects_not_visual_row() {
        // 行高 20px。master 行 (id=u32::MAX) が y=0..20 の先頭、その下に
        // song.tracks の 3 本 (id 10/11/12)。
        let rects = [
            (u32::MAX, rect_at(0.0, 20.0)), // master row at top
            (10, rect_at(20.0, 20.0)),      // song.tracks[0]
            (11, rect_at(40.0, 20.0)),      // song.tracks[1]
            (12, rect_at(60.0, 20.0)),      // song.tracks[2] (最下段)
        ];
        let tracks = [
            Track { id: 10, ..Track::default() },
            Track { id: 11, ..Track::default() },
            Track { id: 12, ..Track::default() },
        ];
        // master 行の Y → song.tracks に居ないので None (新規 track / split 対象外)。
        assert_eq!(track_index_at_y(&rects, &tracks, 10.0), None);
        // 各 track 行の Y → その track の index (+1 ズレ無し)。
        assert_eq!(track_index_at_y(&rects, &tracks, 25.0), Some(0));
        assert_eq!(track_index_at_y(&rects, &tracks, 45.0), Some(1));
        // 最下段も範囲外にならず正しく当たる (Track9=新トラック化バグの固定)。
        assert_eq!(track_index_at_y(&rects, &tracks, 65.0), Some(2));
        // 境界 (行の上端は含む / 下端は含まない half-open)。
        assert_eq!(track_index_at_y(&rects, &tracks, 20.0), Some(0));
        assert_eq!(track_index_at_y(&rects, &tracks, 40.0), Some(1));
        // どの行にも当たらない Y (全行より下) → None。
        assert_eq!(track_index_at_y(&rects, &tracks, 999.0), None);
    }
}
