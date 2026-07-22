//! Audio Editor (Phase 2 PR6 minimal viewer)。
//!
//! audio clip ダブルクリックで bottom_panel の Piano Roll タブの代わりに
//! 表示される波形 view。 `docs/plan_audio_clip.md` §3.10 の Audio Editor。
//!
//! Phase 2 PR6 はミニマル版 (read-only viewer):
//! - clip 内の first event の波形を全幅で描画 (gui_01 `Ui::waveform`)
//! - clip 名 + length / source 情報のヘッダ
//! - 「閉じる」 ボタン (`AppEvent::CloseAudioEditor`)
//! - Esc shortcut で閉じる (= `view/shortcuts.rs` で wire)
//!
//! event 単位の trim / 移動 / 追加 / 削除、 dB handle、 fade 角 drag 等の
//! 編集機能は後続 PR (PR8 以降) で。 編集はそれまで Inspector 経由
//! (Phase 2 PR1-3)。

use std::sync::Arc;

use crate::widgets::select_modifier::{SelectModifier, range_ordered};

use daw_ui_core::{
    ChannelLayout, DragKind, Edit, SampleSlices, Ui, ViewportState1D, WaveformRenderMode,
    WaveformSource, WaveformStyle, WaveformView, WidgetId,
};
use daw_ui_renderer::{Color, LineBatch, LineSegment, Rect};
use crate::theme;

use common::time::{TimeDisplay, TimeMapping};

use crate::app::{AppData, AppEvent, AudioEventTrimSide, MIN_AUDIO_EDITOR_VIEW_LEN_BEATS};
use crate::widgets::time_grid::{TimeGridExt, TimeRulerStyle};

const BG: Color = theme::PANEL;
const TEXT: Color = theme::TEXT;
const GHOST: Color = theme::SELECTION_WARM.with_alpha(0.85);

/// common な mono / stereo source 用の borrowed-plane スタック配列サイズ。
/// channel 数がこれ以下なら event ループ内の毎フレーム `Vec` 確保を消せる。
/// これを超える source (5.1 等) は Vec フォールバックで全 plane を描く。
const MAX_WAVEFORM_CHANNELS: usize = 2;

/// PR-D 段階 3: 中央 drag 中の event ghost (rectangle outline)。 dx は
/// drag.delta.0 (px)。 描画は wf_area で clip。
fn push_move_ghost(ui: &mut Ui<'_, AppData>, event_rect: Rect, wf_area: Rect, dx: f32) {
    let g = Rect {
        x: event_rect.x + dx,
        y: event_rect.y,
        w: event_rect.w,
        h: event_rect.h,
    };
    ui.push_lines(LineBatch {
        segments: Arc::from(vec![
            LineSegment { a: [g.x, g.y], b: [g.x + g.w, g.y], color: GHOST },
            LineSegment {
                a: [g.x, g.y + g.h],
                b: [g.x + g.w, g.y + g.h],
                color: GHOST,
            },
            LineSegment { a: [g.x, g.y], b: [g.x, g.y + g.h], color: GHOST },
            LineSegment {
                a: [g.x + g.w, g.y],
                b: [g.x + g.w, g.y + g.h],
                color: GHOST,
            },
        ]),
        line_width_px: 2.0,
        clip_rect: Some(wf_area),
    });
}

/// PR-D 段階 3: 端 trim drag 中の縦線 ghost (左 = event_rect.x + dx、
/// 右 = event_rect.x + event_rect.w + dx)。
fn push_trim_ghost(
    ui: &mut Ui<'_, AppData>,
    event_rect: Rect,
    wf_area: Rect,
    dx: f32,
    is_left: bool,
) {
    let x = if is_left {
        event_rect.x + dx
    } else {
        event_rect.x + event_rect.w + dx
    };
    ui.push_lines(LineBatch {
        segments: Arc::from(vec![LineSegment {
            a: [x, event_rect.y],
            b: [x, event_rect.y + event_rect.h],
            color: GHOST,
        }]),
        line_width_px: 2.0,
        clip_rect: Some(wf_area),
    });
}

/// 2 つの矩形が交差するか (lasso 矩形と event_rect の hit-test)。
fn rects_intersect(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.w && a.x + a.w > b.x && a.y < b.y + b.h && a.y + a.h > b.y
}

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    ui.panel("audio_editor_bg", area, BG, 0.0);

    // 開いている clip を解決。 audio_editor_clip がセットされていない、
    // または範囲外 / 非 audio なら placeholder を出して return (= clip
    // が削除された / Undo で消えた場合の防御)。
    let target = match app.ui_ephemeral.audio_editor_clip {
        Some(t) => t,
        None => {
            ui.label_at(
                "audio_editor_empty",
                "(Audio Editor: 表示する clip が選択されていません)",
                area.x + 12.0,
                area.y + 18.0,
                12.0,
                TEXT,
            );
            return;
        }
    };
    let Some(track) = app.song_doc.song().tracks.get(target.track as usize) else {
        return;
    };
    let Some(clip) = track.clips.get(target.clip as usize) else {
        return;
    };
    let Some(common::model::ClipContent::Audio(audio)) =
        app.song_doc.song().clip_contents.get(&clip.content_id)
    else {
        return;
    };

    // ----- Header (clip 名 + length + close button) -------------------
    let pad = 12.0;
    let header_h = 24.0;
    let ruler_h = 20.0;
    ui.label_at(
        "audio_editor_title",
        &format!(
            "Audio Editor — {} ({:.2} beats)",
            app.song_doc.song().content_name(clip.content_id),
            clip.length_beats
        ),
        area.x + pad,
        area.y + 6.0,
        14.0,
        TEXT,
    );
    let close_w = 60.0;
    let close_rect = Rect {
        x: area.x + area.w - pad - close_w,
        y: area.y + 4.0,
        w: close_w,
        h: header_h,
    };
    ui.button_at("audio_editor_close", "Close", close_rect, || {
        Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::CloseAudioEditor))
    });

    // ----- View state (scroll / zoom) ------------------------------------
    // `audio_editor_view_start_beat` / `audio_editor_view_len_beats` は
    // `OpenAudioEditor` で 0 / clip.length_beats にセット済み (= 全体表示
    // 初期状態)。 wheel handler が以降 SetAudioEditorScroll / SetAudioEditorZoom
    // を発火する。 描画前に clamp し直して視覚的に無効値を防ぐ (= clip
    // が縮んだ等で view が clip 外に飛び出すケース)。
    let total_beats = clip.length_beats.max(MIN_AUDIO_EDITOR_VIEW_LEN_BEATS);
    let view_len_beats = if app.audio_editor_view_len_beats() > 0.0 {
        app.audio_editor_view_len_beats()
            .clamp(MIN_AUDIO_EDITOR_VIEW_LEN_BEATS, total_beats)
    } else {
        total_beats
    };
    let max_view_start = (total_beats - view_len_beats).max(0.0);
    let view_start_beat = app
        .audio_editor_view_start_beat()
        .clamp(0.0, max_view_start);

    // ----- Ruler (MIDI エディタ同様、 song 全体の絶対 bar 番号を表示) -
    // viewport は view_start_beat .. view_start_beat + view_len_beats を
    // sample 単位に換算 (clip.start_beat 加算で song 絶対座標)。 zoom
    // 中も bar 番号は曲全体基準で表示される。
    let ruler_rect = Rect {
        x: area.x + pad,
        y: area.y + header_h,
        w: (area.w - pad * 2.0).max(0.0),
        h: ruler_h,
    };
    if ruler_rect.w > 0.0 && view_len_beats > 0.0 {
        let mapping = TimeMapping {
            sample_rate: app.ipc.sample_rate as f64,
            tempo_bpm: app.song_doc.song().bpm as f64,
            time_sig: app.song_doc.song().time_sig,
            display: TimeDisplay::BarBeat,
        };
        let spb = mapping.samples_per_beat();
        let viewport = ViewportState1D {
            view_start: (clip.start_beat + view_start_beat) * spb,
            view_len: view_len_beats * spb,
        };
        ui.time_ruler(
            "audio_editor_ruler",
            ruler_rect,
            mapping,
            viewport,
            TimeRulerStyle::default(),
        );

        // ----- 既存 loop region overlay (ruler 上に半透明バンド) -----
        // arrangement view と同色の cyan、 view 範囲との交差のみ描画。
        if app.song_doc.song().loop_end_beat > app.song_doc.song().loop_start_beat {
            let view_start_song = clip.start_beat + view_start_beat;
            let view_end_song = view_start_song + view_len_beats;
            let lstart = app.song_doc.song().loop_start_beat;
            let lend = app.song_doc.song().loop_end_beat;
            let visible_start = lstart.max(view_start_song);
            let visible_end = lend.min(view_end_song);
            if visible_end > visible_start && view_len_beats > 0.0 {
                let nstart = ((visible_start - view_start_song) / view_len_beats) as f32;
                let nend = ((visible_end - view_start_song) / view_len_beats) as f32;
                let band = Rect {
                    x: ruler_rect.x + nstart * ruler_rect.w,
                    y: ruler_rect.y,
                    w: ((nend - nstart) * ruler_rect.w).max(1.0),
                    h: ruler_rect.h,
                };
                ui.push_rect(daw_ui_renderer::RectCommand {
                    rect: band,
                    fill: theme::LOOP_BAND.with_alpha(0.18),
                    border: theme::LOOP_BAND.with_alpha(0.55),
                    border_width: 1.0,
                    radius: [0.0; 4],
                    clip_rect: Some(ruler_rect),
                });
            }
        }
    }

    // ----- Ruler interaction (arrangement と同じ操作感) -----------------
    // gui_01 #024 の arrangement widget 内蔵動作を Audio Editor 用に
    // 外部実装。 plain (= Shift 非保持) press / drag = 連続 SetPlayheadBeat、
    // Shift+drag = release 時に SetLoopRange。 anchor / current の x 座標を
    // view_start_beat + view_norm * view_len_beats で song-absolute beat
    // に換算してから AppEvent に渡す。
    if ruler_rect.w > 0.0
        && view_len_beats > 0.0
        && let Some(drag) =
            ui.take_drag_in_rect(("audio_editor_ruler_drag", clip.id), ruler_rect)
    {
        let to_song_beat = |x_px: f32| -> f64 {
            let local_x = (x_px - ruler_rect.x).clamp(0.0, ruler_rect.w);
            let view_norm = (local_x / ruler_rect.w) as f64;
            (clip.start_beat + view_start_beat + view_norm * view_len_beats).max(0.0)
        };
        let cur_beat = to_song_beat(drag.current.0);
        if drag.start_modifiers.shift {
            // Shift+drag: release frame に commit (= drag 中は preview なし、
            // 軽量実装。 必要なら将来 preview 追加可能)。
            if drag.kind == DragKind::Released {
                let anchor_beat = to_song_beat(drag.anchor.0);
                let (start, end) = if anchor_beat <= cur_beat {
                    (anchor_beat, cur_beat)
                } else {
                    (cur_beat, anchor_beat)
                };
                if (end - start).abs() > 1e-6 {
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetLoopRange { start, end });
                    }));
                }
            }
        } else {
            // 素 drag / 単発 click: Started + Continuing で連続 seek
            // (= Stop 中も Play 中も即座にプレイカーソル移動 + IPC SeekTo)。
            // arrangement / piano_roll と同形で `AppData::seek_playhead_to` に集約。
            // seek_playhead_to は「停止で戻るホーム」も更新する。
            if drag.kind != DragKind::Released {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.seek_playhead_to(cur_beat);
                }));
            }
        }
    }

    // ----- Waveform area --------------------------------------------
    let wf_area = Rect {
        x: area.x + pad,
        y: area.y + header_h + ruler_h + 4.0,
        w: (area.w - pad * 2.0).max(0.0),
        h: (area.h - header_h - ruler_h - 4.0 - 24.0).max(0.0),
    };
    if wf_area.w <= 0.0 || wf_area.h <= 0.0 {
        return;
    }

    // ----- Multi-event 対応 (Phase 2 PR-D 段階 1) ---------------------
    // events.iter().enumerate() で各 event ごとに rect を分割描画。
    // event の rect = wf_area を `(event_start / clip.length_beats)` 〜
    // `((event_start + event_length) / clip.length_beats)` で割り当てる。
    // 選択中 event (= `audio_editor_selected_event`、 None なら 0 を
    // default) は border highlight。 Click 検出 / drag は段階 2 以降。
    if audio.events.is_empty() {
        ui.label_at(
            "audio_editor_no_event",
            "(空の audio content — event がありません)",
            wf_area.x + 4.0,
            wf_area.y + 8.0,
            11.0,
            TEXT,
        );
        return;
    }
    let selected_set: std::collections::HashSet<usize> =
        app.selection.audio_editor_selected_events.iter().copied().collect();
    let anchor_idx = app.audio_editor_anchor_event();
    // 矩形選択 (lasso) の hit-test 用に、 描画した event の rect を収集する。
    let mut event_rects: Vec<(usize, Rect)> = Vec::new();
    let clip_len_beats = clip.length_beats.max(1e-6); // 0 div 防御
    // view ベース px → beats 換算係数。 view_len_beats は zoom 中に変動
    // するので毎フレーム再計算。 wf_area.w (px) は view_len_beats (beats)
    // 分の幅を表示している。 1 px = beats_per_px。
    let beats_per_px = (view_len_beats / wf_area.w as f64).max(1e-9);
    // `target` は冒頭 (clip 解決時) に bind 済み。 `app` は &AppData で
    // 不変なので `audio_editor_clip` は変化せず、 再 resolve は不要。
    let clip_id = clip.id;

    // ----- Wheel scroll / zoom (Bitwig 流) -------------------------------
    // 素 wheel = 水平 scroll、 Ctrl+wheel = anchor 保持 zoom、
    // Shift+wheel = 高速 scroll (3x)、 Alt+wheel は scope 外 (no-op)。
    // drag 中 (`take_drag_in_rect` で active session) はこの handler
    // より後で event walk が走るため、 drag は通常通り進む。 ただし
    // wheel と drag が同 frame に来るケースは pointer.scroll_delta を
    // 消費して drag 計算には影響しない (gui_01 が分離管理)。
    {
        let pointer = ui.pointer();
        if let Some((px, py)) = pointer.pos
            && wf_area.contains(px, py)
            && wf_area.w > 0.0
        {
            let (sx, sy) = pointer.scroll_delta;
            if sy.abs() > 0.001 || sx.abs() > 0.001 {
                let m = pointer.modifiers;
                if m.ctrl {
                    // Ctrl+wheel: 水平 zoom。 マウス位置の beat を anchor
                    // として保持: new_view_start = anchor_beat - frac * new_len。
                    let factor = (sy * 0.005).exp() as f64;
                    let new_len = (view_len_beats / factor)
                        .clamp(MIN_AUDIO_EDITOR_VIEW_LEN_BEATS, total_beats);
                    if (new_len - view_len_beats).abs() > 1e-6 {
                        let anchor_frac = ((px - wf_area.x) / wf_area.w).clamp(0.0, 1.0) as f64;
                        let anchor_beat = view_start_beat + anchor_frac * view_len_beats;
                        let new_start = (anchor_beat - anchor_frac * new_len)
                            .clamp(0.0, (total_beats - new_len).max(0.0));
                        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::SetAudioEditorZoom {
                                view_start_beat: new_start,
                                view_len_beats: new_len,
                            });
                        }));
                    }
                } else if m.alt {
                    // B13 (r.md #8): Alt+wheel = 波形の縦 gain zoom (振幅表示の拡大率)。
                    // 描画スケールのみで model / 音声には非影響。 0.25..16x に clamp。
                    let factor = (((sx + sy) as f64) * 0.01).exp() as f32;
                    if (factor - 1.0).abs() > 1e-6 {
                        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                            app.ui_prefs.audio_editor_vertical_gain =
                                (app.ui_prefs.audio_editor_vertical_gain * factor).clamp(0.25, 16.0);
                        }));
                    }
                } else {
                    // 素 wheel / Shift+wheel: 水平 scroll。 piano_roll と
                    // 同じ符号 (wheel up = view_start 減少 = timeline 左へ)。
                    let speed = if m.shift { 3.0 } else { 1.0 };
                    let dx_beats = -((sx + sy) as f64) * beats_per_px * speed;
                    let new_start =
                        (view_start_beat + dx_beats).clamp(0.0, max_view_start);
                    if (new_start - view_start_beat).abs() > 1e-9 {
                        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::SetAudioEditorScroll(new_start));
                        }));
                    }
                }
            }
        }
    }

    // (r.md #35) Shift+click の範囲選択が使う「event の並び」。 index が時間順なので
    // 0..len がそのまま順序列 (`range_ordered` の入力)。
    let event_total = audio.events.len();
    for (idx, event) in audio.events.iter().enumerate() {
        let Some(buffer) = app.media.audio_source_cache.get(event.source_id) else {
            // 当該 event は decode 待ち / missing source → 透けて見える
            // 範囲だけマーカー描画 (= 他 event は描く)。
            continue;
        };

        // event rect (view 範囲を wf_area 全幅にマップ、 view 外 event は
        // [0,1] clamp で端に貼り付く)。 zoom 中も view_start_beat /
        // view_len_beats が更新されているので毎フレーム正しく追従。
        let evt_norm_start =
            ((event.event_start_in_clip_beats - view_start_beat) / view_len_beats) as f32;
        let evt_norm_end = ((event.event_start_in_clip_beats + event.event_length_beats
            - view_start_beat)
            / view_len_beats) as f32;
        let evt_x_start_clamped = evt_norm_start.clamp(0.0, 1.0);
        let evt_x_end_clamped = evt_norm_end.clamp(0.0, 1.0);
        if evt_x_end_clamped <= evt_x_start_clamped {
            // 完全に view 外 → skip (描画 / hit-test 不要、 cache 効率化)。
            continue;
        }
        let event_rect = Rect {
            x: wf_area.x + evt_x_start_clamped * wf_area.w,
            y: wf_area.y,
            w: ((evt_x_end_clamped - evt_x_start_clamped) * wf_area.w).max(2.0),
            h: wf_area.h,
        };
        event_rects.push((idx, event_rect));

        // SampleSlices::Planar 用の borrowed-plane。 common な mono/stereo は
        // スタック配列で event ループ内の毎フレーム heap 確保を消し、 稀な
        // >2ch source のときだけ Vec にフォールバックして全 plane を描く
        // (= channel を黙って捨てない)。
        let mut planes_buf: [&[f32]; MAX_WAVEFORM_CHANNELS] = [&[]; MAX_WAVEFORM_CHANNELS];
        let planes_fallback: Vec<&[f32]>;
        let planes_borrowed: &[&[f32]] = if buffer.samples.len() <= MAX_WAVEFORM_CHANNELS {
            for (i, plane) in buffer.samples.iter().enumerate() {
                planes_buf[i] = plane.as_slice();
            }
            &planes_buf[..buffer.samples.len()]
        } else {
            planes_fallback = buffer.samples.iter().map(Vec::as_slice).collect();
            &planes_fallback
        };
        let event_len_frames = event
            .source_end_frames
            .saturating_sub(event.source_start_frames)
            .max(1);

        let is_selected = selected_set.contains(&idx);
        let fg = if is_selected {
            theme::WAVEFORM_SEL.with_alpha(0.95)
        } else {
            theme::WAVEFORM.with_alpha(0.85)
        };
        let style = WaveformStyle {
            fg,
            fg_clipped: theme::WAVEFORM_PEAK,
            fill: None,
            baseline: Some(theme::GRID_LINE.with_alpha(0.15)),
            channel_layout: ChannelLayout::Stack,
            render_mode: WaveformRenderMode::Auto,
            line_width_px: 1.0,
        };

        let markers = event.beat_markers.as_slice();
        if markers.len() >= 2 {
            // ----- B12-manual: 非均一 warp の区分線形波形描画 -----
            // playback (`common::audio_render::warp_source_frame`) と同じ区分線形
            // 写像で各 marker 区間を個別の linear `ui.waveform` として描く。 区間内
            // は線形なので ui.waveform をそのまま流用でき、 全体で warp 形状 (= 実際
            // に再生される source 位置) を反映する。 境界点 = (clip 相対 beat, source
            // frame)。 端 (event-local 0 / event_length) が marker で覆われない場合は
            // `warp_source_frame` で外挿し source frame を [0, buffer.frames] に clamp。
            let src_max = buffer.frames as f64;
            let mut pts: Vec<(f64, f64)> = Vec::with_capacity(markers.len() + 2);
            if markers[0].locked_beat > 1e-9 {
                let sf = common::audio_render::warp_source_frame(0.0, markers)
                    .unwrap_or(event.source_start_frames as f64)
                    .clamp(0.0, src_max);
                pts.push((event.event_start_in_clip_beats, sf));
            }
            for m in markers {
                pts.push((
                    event.event_start_in_clip_beats + m.locked_beat,
                    (m.source_frame as f64).clamp(0.0, src_max),
                ));
            }
            if markers[markers.len() - 1].locked_beat < event.event_length_beats - 1e-9 {
                let sf =
                    common::audio_render::warp_source_frame(event.event_length_beats, markers)
                        .unwrap_or(event.source_end_frames as f64)
                        .clamp(0.0, src_max);
                pts.push((
                    event.event_start_in_clip_beats + event.event_length_beats,
                    sf,
                ));
            }
            let view_end_beat = view_start_beat + view_len_beats;
            for (seg_i, w) in pts.windows(2).enumerate() {
                let (b0, sf0) = w[0];
                let (b1, sf1) = w[1];
                let seg_beats = b1 - b0;
                if seg_beats <= 1e-9 {
                    continue;
                }
                // 区間 [b0, b1] (clip 相対 beat) を view にクリップ。
                let vis_start = b0.max(view_start_beat);
                let vis_end = b1.min(view_end_beat);
                if vis_end <= vis_start {
                    continue;
                }
                let nx0 = ((vis_start - view_start_beat) / view_len_beats) as f32;
                let nx1 = ((vis_end - view_start_beat) / view_len_beats) as f32;
                let seg_rect = Rect {
                    x: wf_area.x + nx0 * wf_area.w,
                    y: wf_area.y,
                    w: ((nx1 - nx0) * wf_area.w).max(1.0),
                    h: wf_area.h,
                };
                // 区間内は linear: 可視 beat 範囲を source frame に線形写像。
                let f0 = (vis_start - b0) / seg_beats;
                let f1 = (vis_end - b0) / seg_beats;
                let src_a = sf0 + (sf1 - sf0) * f0;
                let src_b = sf0 + (sf1 - sf0) * f1;
                let view = WaveformView {
                    start_sample: src_a.min(src_b).max(0.0) as u64,
                    len_samples: ((src_b - src_a).abs() as u64).max(1),
                    vertical_gain: app.ui_prefs.audio_editor_vertical_gain,
                };
                let source = WaveformSource {
                    samples: SampleSlices::Planar(planes_borrowed),
                    valid_len: buffer.frames as usize,
                    generation: event.source_id as u64,
                    sample_rate: buffer.sample_rate,
                };
                let _ = ui.waveform(
                    ("audio_editor_wf_seg", clip.id, idx, seg_i),
                    seg_rect,
                    source,
                    view,
                    style,
                );
            }
        } else {
            // uniform stretch / raw: 従来の単一 linear 波形。
            // visible-portion を source frames にマップ。 evt_x_*_clamped は
            // [0, 1] 内の view 内 ratio。 event-local 比率に直して
            // event_len_frames に掛ける。
            let event_len_beats_safe = event.event_length_beats.max(1e-9);
            let event_view_start_beat = event.event_start_in_clip_beats.max(view_start_beat);
            let event_view_end_beat = (event.event_start_in_clip_beats
                + event.event_length_beats)
                .min(view_start_beat + view_len_beats);
            let visible_start_in_event =
                (event_view_start_beat - event.event_start_in_clip_beats).max(0.0);
            let visible_len_in_event = (event_view_end_beat - event_view_start_beat).max(0.0);
            let src_visible_start_frames = event.source_start_frames
                + ((visible_start_in_event / event_len_beats_safe) * event_len_frames as f64)
                    as u64;
            let src_visible_len_frames = ((visible_len_in_event / event_len_beats_safe)
                * event_len_frames as f64) as u64;
            let source = WaveformSource {
                samples: SampleSlices::Planar(planes_borrowed),
                valid_len: buffer.frames as usize,
                generation: event.source_id as u64,
                sample_rate: buffer.sample_rate,
            };
            let view = WaveformView {
                start_sample: src_visible_start_frames,
                len_samples: src_visible_len_frames.max(1),
                vertical_gain: app.ui_prefs.audio_editor_vertical_gain,
            };
            let _ = ui.waveform(
                ("audio_editor_wf", clip.id, idx),
                event_rect,
                source,
                view,
                style,
            );
        }

        // Selection border (= 選択中のみ視認できる枠)。 1 px 太い線で
        // 上下左右を marker。 push_rect で半透明帯にしても良いが、
        // border の方が波形を遮らない。
        if is_selected {
            let border_color = theme::SELECTION_WARM.with_alpha(0.85);
            ui.push_lines(LineBatch {
                segments: Arc::from(vec![
                    // top
                    LineSegment {
                        a: [event_rect.x, event_rect.y],
                        b: [event_rect.x + event_rect.w, event_rect.y],
                        color: border_color,
                    },
                    // bottom
                    LineSegment {
                        a: [event_rect.x, event_rect.y + event_rect.h],
                        b: [event_rect.x + event_rect.w, event_rect.y + event_rect.h],
                        color: border_color,
                    },
                    // left
                    LineSegment {
                        a: [event_rect.x, event_rect.y],
                        b: [event_rect.x, event_rect.y + event_rect.h],
                        color: border_color,
                    },
                    // right
                    LineSegment {
                        a: [event_rect.x + event_rect.w, event_rect.y],
                        b: [event_rect.x + event_rect.w, event_rect.y + event_rect.h],
                        color: border_color,
                    },
                ]),
                line_width_px: 1.5,
                clip_rect: Some(wf_area),
            });
        }

        // ----- B12-manual: warp marker 描画 + 手動編集 -----
        // 各 marker を locked_beat の x に縦線で描く。 可変な波形/背景上でも
        // 視認できるよう暗い backing line (太) + 明色 (細) の 2 層
        // (`feedback_ui_indicator_contrast_on_variable_bg`)。 x は draw と
        // 下記 hit-test で共有 (DRY)。
        let marker_xs: Vec<(usize, f32)> = event
            .beat_markers
            .iter()
            .enumerate()
            .map(|(mi, m)| {
                let clip_beat = event.event_start_in_clip_beats + m.locked_beat;
                let x = wf_area.x + ((clip_beat - view_start_beat) / beats_per_px) as f32;
                (mi, x)
            })
            .filter(|&(_, x)| x >= wf_area.x - 0.5 && x <= wf_area.x + wf_area.w + 0.5)
            .collect();
        if !marker_xs.is_empty() {
            let mut backing: Vec<LineSegment> = Vec::with_capacity(marker_xs.len());
            let mut bright: Vec<LineSegment> = Vec::with_capacity(marker_xs.len());
            for &(_, x) in &marker_xs {
                backing.push(LineSegment {
                    a: [x, event_rect.y],
                    b: [x, event_rect.y + event_rect.h],
                    color: theme::WINDOW_BG.with_alpha(0.65),
                });
                bright.push(LineSegment {
                    a: [x, event_rect.y],
                    b: [x, event_rect.y + event_rect.h],
                    color: theme::LOOP_BAND.with_alpha(0.92),
                });
            }
            ui.push_lines(LineBatch {
                segments: Arc::from(backing),
                line_width_px: 3.0,
                clip_rect: Some(wf_area),
            });
            ui.push_lines(LineBatch {
                segments: Arc::from(bright),
                line_width_px: 1.5,
                clip_rect: Some(wf_area),
            });
        }

        // ----- Hit-test (PR-D 段階 3 / gui_01 #026) -----
        // event_rect を [left grip, center, right grip] に分割。 left/right
        // grip は trim drag、 center は move drag。 grip 幅は 6 px、 event
        // が 18 px 未満なら grip を出さず center のみ。 event の端が view
        // 外にクリップされている場合 (= zoom 中に event が画面端を超えた)
        // は対応 grip を無効化 (= grip rect が view 端にあって誤った beat
        // を返すのを防ぐ)。
        const GRIP_W: f32 = 6.0;
        let usable_w = event_rect.w.max(0.0);
        let left_edge_in_view = evt_norm_start >= 0.0;
        let right_edge_in_view = evt_norm_end <= 1.0;
        let (lw, rw) = if usable_w >= GRIP_W * 3.0 {
            (
                if left_edge_in_view { GRIP_W } else { 0.0 },
                if right_edge_in_view { GRIP_W } else { 0.0 },
            )
        } else {
            (0.0, 0.0)
        };
        let left_grip = Rect {
            x: event_rect.x,
            y: event_rect.y,
            w: lw,
            h: event_rect.h,
        };
        let right_grip = Rect {
            x: event_rect.x + event_rect.w - rw,
            y: event_rect.y,
            w: rw,
            h: event_rect.h,
        };
        let center_band = Rect {
            x: event_rect.x + lw,
            y: event_rect.y,
            w: (event_rect.w - lw - rw).max(0.0),
            h: event_rect.h,
        };

        // 左端 trim
        if lw > 0.0
            && let Some(drag) =
                ui.take_drag_in_rect(("audio_editor_trim_l", clip_id, idx), left_grip)
        {
            let dx = drag.delta.0;
            let kind = drag.kind;
            if kind == DragKind::Started {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SelectAudioEditorEvent(Some(idx)));
                }));
            } else if kind == DragKind::Continuing {
                push_trim_ghost(ui, event_rect, wf_area, dx, true);
            } else if kind == DragKind::Released && dx.abs() >= 1.0 {
                // 0px release (= grip を click しただけ) は no-op trim を commit
                // しない (undo 汚染 + 再同期を避ける。 実 drag は 1px から反映)。
                let dbeats = (dx as f64) * beats_per_px;
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetAudioEventTrim {
                        clip: target,
                        event_idx: idx,
                        side: AudioEventTrimSide::Left,
                        delta_beats: dbeats,
                    });
                }));
            }
        }

        // 右端 trim
        if rw > 0.0
            && let Some(drag) =
                ui.take_drag_in_rect(("audio_editor_trim_r", clip_id, idx), right_grip)
        {
            let dx = drag.delta.0;
            let kind = drag.kind;
            if kind == DragKind::Started {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SelectAudioEditorEvent(Some(idx)));
                }));
            } else if kind == DragKind::Continuing {
                push_trim_ghost(ui, event_rect, wf_area, dx, false);
            } else if kind == DragKind::Released && dx.abs() >= 1.0 {
                // 0px release (= grip を click しただけ) は no-op trim を commit
                // しない (undo 汚染 + 再同期を避ける。 実 drag は 1px から反映)。
                let dbeats = (dx as f64) * beats_per_px;
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetAudioEventTrim {
                        clip: target,
                        event_idx: idx,
                        side: AudioEventTrimSide::Right,
                        delta_beats: dbeats,
                    });
                }));
            }
        }

        // warp marker drag (移動) / Alt+click (削除)。 center_band より先に
        // 登録し、 narrow hit rect で press を先取りする (= marker 上の press は
        // marker が、 それ以外は center が取る)。 trim grip より後なので端
        // marker は trim に譲る。 release は 1 回だけなので各ジェスチャ = 1 undo。
        for &(marker_idx, mx) in &marker_xs {
            let hit = Rect {
                x: mx - 5.0,
                y: event_rect.y,
                w: 10.0,
                h: event_rect.h,
            };
            let Some(drag) = ui.take_drag_in_rect(
                ("audio_editor_warp_marker", clip_id, idx, marker_idx),
                hit,
            ) else {
                continue;
            };
            if drag.start_modifiers.alt {
                // Alt+click = この marker を削除。
                if drag.kind == DragKind::Started {
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::DeleteWarpMarker {
                            event_idx: idx,
                            marker_idx,
                        });
                    }));
                }
            } else if drag.kind == DragKind::Continuing {
                // ghost: 現在ポインタ位置に縦線。
                let gx = drag.current.0;
                ui.push_lines(LineBatch {
                    segments: Arc::from(vec![LineSegment {
                        a: [gx, event_rect.y],
                        b: [gx, event_rect.y + event_rect.h],
                        color: theme::LOOP_BAND.with_alpha(0.6),
                    }]),
                    line_width_px: 1.5,
                    clip_rect: Some(wf_area),
                });
            } else if drag.kind == DragKind::Released {
                // release x → event-local beat (clip 相対 - event 開始)。
                let clip_beat =
                    view_start_beat + (drag.current.0 - wf_area.x) as f64 * beats_per_px;
                let new_local = (clip_beat - event.event_start_in_clip_beats)
                    .clamp(0.0, event.event_length_beats);
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::MoveWarpMarker {
                        event_idx: idx,
                        marker_idx,
                        new_locked_beat: new_local,
                    });
                }));
            }
        }

        // 中央 drag。 plain = 移動 (start = 単一選択、 continuing = ghost、
        // released = SetAudioEventStart commit)。 Shift = 選択トグル
        // (move せず、 Started で 1 度だけ集合に add/remove)。 Alt = ここに
        // warp marker 追加 (marker 以外の波形上 Alt+click)。
        if center_band.w > 0.0
            && let Some(drag) =
                ui.take_drag_in_rect(("audio_editor_move", clip_id, idx), center_band)
        {
            let dx = drag.delta.0;
            let kind = drag.kind;
            if drag.start_modifiers.alt {
                // Alt+click on waveform (marker 以外) = press 位置に warp marker
                // 追加。 source frame = 現在の warp 曲線上の source (= 既存曲線に
                // pin して追加 → ドラッグで再 warp)。 marker < 2 (uniform) は線形近似。
                if kind == DragKind::Started {
                    let clip_beat =
                        view_start_beat + (drag.anchor.0 - wf_area.x) as f64 * beats_per_px;
                    let local = (clip_beat - event.event_start_in_clip_beats)
                        .clamp(0.0, event.event_length_beats);
                    let src = common::audio_render::warp_source_frame(local, &event.beat_markers)
                        .unwrap_or_else(|| {
                            let len = event
                                .source_end_frames
                                .saturating_sub(event.source_start_frames)
                                as f64;
                            event.source_start_frames as f64
                                + (local / event.event_length_beats.max(1e-9)) * len
                        });
                    let source_frame = src.max(0.0) as u64;
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::AddWarpMarker {
                            event_idx: idx,
                            source_frame,
                            locked_beat: local,
                        });
                    }));
                }
            } else if drag.start_modifiers.shift || drag.start_modifiers.ctrl {
                // r.md #35: 旧実装は Shift のみをトグルに使い、 Ctrl は素通り (= 無修飾と同じ
                // 単一選択) だった。 全選択面共通の `SelectModifier` に統一する —
                // Ctrl = Toggle / Shift = RangeFromAnchor。 event は 1 clip 内で時間順に
                // 並ぶ index なので範囲は 1 次元 (`range_ordered`)。
                if kind == DragKind::Started {
                    let modifier = SelectModifier::from_modifiers(
                        drag.start_modifiers.shift,
                        drag.start_modifiers.ctrl,
                    );
                    let order: Vec<usize> = (0..event_total).collect();
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        let prev = app.selection.audio_editor_selected_events.clone();
                        let anchor = app.selection.audio_editor_anchor;
                        let next = modifier
                            .resolve(&prev, idx, || range_ordered(&order, anchor?, idx));
                        if next != prev {
                            app.handle_event(AppEvent::SetAudioEditorEventSelection(next));
                        }
                        if modifier.updates_anchor() {
                            app.selection.audio_editor_anchor = Some(idx);
                        }
                    }));
                }
            } else if kind == DragKind::Started {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SelectAudioEditorEvent(Some(idx)));
                    app.selection.audio_editor_anchor = Some(idx);
                }));
            } else if kind == DragKind::Continuing && dx.abs() > 1.0 {
                push_move_ghost(ui, event_rect, wf_area, dx);
            } else if kind == DragKind::Released && dx.abs() >= 4.0 {
                // 4px 未満は click (選択のみ) に格下げ — delta≈0 の
                // SetAudioEventStart を commit すると選択クリックのたびに
                // undo snapshot + redo 破棄 + 不要な plugin-host 再同期が走る
                // (ui/CLAUDE.md の Move 4px jitter 閾値ガイド、 review)。
                let dbeats = (dx as f64) * beats_per_px;
                let original_start = event.event_start_in_clip_beats;
                let new_start = (original_start + dbeats).max(0.0);
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetAudioEventStart {
                        clip: target,
                        event_idx: idx,
                        new_start_beats: new_start,
                    });
                }));
            }
        }

        // 単発 click による選択は中央 drag / 端 grip の `DragKind::Started`
        // が担う (press frame に consume_pointer_click 済 + event_rect 全域を
        // center+grip でカバー)。 別途の take_primary_press_in_rect は不要。

        // 右クリック context menu。 Duplicate / Delete / Add From Source...
        let evt_end_beats = event.event_start_in_clip_beats + event.event_length_beats;
        ui.context_menu_for(
            event_rect,
            &["Duplicate", "Delete", "Add From Source..."],
            move |menu_idx, ui| {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| match menu_idx {
                    0 => {
                        app.handle_event(AppEvent::SelectAudioEditorEvent(Some(idx)));
                        app.handle_event(AppEvent::DuplicateAudioEditorEvent);
                    }
                    1 => {
                        // 右クリックした event を選択に collapse してから
                        // 選択集合 delete (Delete キーと同じ DeleteAudioEditorSelection
                        // 経路に統一)。 Duplicate と同じ select-then-act パターン。
                        app.handle_event(AppEvent::SelectAudioEditorEvent(Some(idx)));
                        app.handle_event(AppEvent::DeleteAudioEditorSelection);
                    }
                    2 => {
                        app.action_open_audio_event_dialog(target, evt_end_beats);
                    }
                    _ => {}
                }));
            },
        );
    }

    // ----- 矩形選択 (lasso) -----------------------------------------
    // 空き領域からの primary drag で複数 event をまとめて選択。 event の
    // grip/center が press を consume_pointer_click 済なので、 event 上から
    // 始まる drag はここに来ない (= 空き領域専用)。 Shift で既存選択に
    // 加算、 plain は置換 (空 drag / 空クリック = 全解除)。 cyan overlay は
    // take_drag_rect_in_rect が自動描画。
    if let Some(dr) = ui.take_drag_rect_in_rect(
        WidgetId::ROOT.child((b"audio_editor_lasso", clip_id)),
        wf_area,
    ) && dr.finished
    {
        let r = dr.rect();
        // r.md #35: 投げ縄の修飾も他面と揃える — 無修飾 = REPLACE / Shift = UNION / Ctrl = XOR
        // (旧実装は Ctrl を見ておらず REPLACE に落ちていた)。
        let (union, xor) = (dr.modifiers.shift, dr.modifiers.ctrl);
        let hit: Vec<usize> = event_rects
            .iter()
            .filter(|(_, er)| rects_intersect(r, *er))
            .map(|(i, _)| *i)
            .collect();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            let prev = app.selection.audio_editor_selected_events.clone();
            let next: Vec<usize> = if union {
                let mut out = prev.clone();
                for i in &hit {
                    if !out.contains(i) {
                        out.push(*i);
                    }
                }
                out
            } else if xor {
                let mut out: Vec<usize> =
                    prev.iter().copied().filter(|i| !hit.contains(i)).collect();
                for i in &hit {
                    if !prev.contains(i) {
                        out.push(*i);
                    }
                }
                out
            } else {
                hit
            };
            if next != prev {
                app.handle_event(AppEvent::SetAudioEditorEventSelection(next));
            }
        }));
    }

    // 空白領域 (= waveform area で event 上にない場所) への file drop
    // で、 drop 位置を `position_in_clip_beats` に変換して
    // AddAudioEventFromFile を発火。 1 path のみ採用 (= multi-drop は
    // 先頭ファイルのみ; 残りは無視、 status_message に noted せず
    // 静かにスキップ)。
    if let Some(drop) = ui.take_file_drop_in_rect(wf_area)
        && let Some(path) = drop.paths.into_iter().next()
    {
        // drop x 位置を view 内 ratio → clip-local beat に換算。
        let pos_beats = view_start_beat
            + ((drop.position.0 - wf_area.x).max(0.0) as f64) * beats_per_px;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::AddAudioEventFromFile {
                clip: target,
                path,
                position_in_clip_beats: pos_beats.max(0.0),
            });
        }));
    }

    // ----- Mouse hover → clip 内 beat (E キー split / 将来の波形操作用) -----
    // wf_area 内のマウス位置を clip-local beat (clip 始端 = 0) に変換。
    // E キー (action_split_clips_at_cursor) は audio editor 開いてる時
    // 既存の arrangement_hover ではなく **この値** を優先採用する
    // (= bottom panel にマウスがある時点で arrangement hover は更新
    // されないため、 そのままだと「マウスを arrangement に置いて」 status
    // で no-op に陥る)。 マウスが waveform 外なら None で push し、
    // E キーは fallback (playhead / selection) に流れる。
    let hover_in_clip: Option<f64> = ui.pointer().pos.and_then(|(px, py)| {
        if !wf_area.contains(px, py) {
            return None;
        }
        let in_clip = view_start_beat
            + ((px - wf_area.x).max(0.0) as f64) * beats_per_px;
        Some(in_clip.clamp(0.0, clip_len_beats))
    });
    if app.ui_ephemeral.audio_editor_hover_beat_in_clip != hover_in_clip {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_ephemeral.audio_editor_hover_beat_in_clip = hover_in_clip;
        }));
    }

    // ----- Playhead 線 (Phase 2 PR7) -----
    // 再生中 / Stop 後 (= playhead_beat is Some) かつ playhead が現
    // clip の範囲内なら、 wf_area 上に縦線を重ねる。 視覚的に「曲全体の
    // どこを再生しているか」 が Audio Editor 内でも分かる。 view は
    // event.event_start_in_clip_beats から event_length_beats まで
    // (= 1 event = clip 全体) を全幅マッピングしているので、
    // x = wf_area.x + (in_clip_beats / clip.length_beats) * wf_area.w。
    if let Some(ph_beat) = app.transport.playhead_beat {
        let ph_beat = ph_beat as f64;
        let clip_start = clip.start_beat;
        // playhead の clip 内位置 (clip 始端 = 0)。 view 範囲外 (zoom 中
        // で playhead が画面外) なら描画 skip。
        let in_clip = ph_beat - clip_start;
        let view_end_beat = view_start_beat + view_len_beats;
        if in_clip >= view_start_beat
            && in_clip < view_end_beat
            && view_len_beats > 0.0
        {
            let norm = (in_clip - view_start_beat) / view_len_beats;
            let x = wf_area.x + norm as f32 * wf_area.w;
            let color = theme::PLAYHEAD.with_alpha(0.9);
            ui.push_lines(LineBatch {
                segments: std::sync::Arc::from(vec![LineSegment {
                    a: [x, wf_area.y],
                    b: [x, wf_area.y + wf_area.h],
                    color,
                }]),
                line_width_px: 1.5,
                clip_rect: Some(wf_area),
            });
        }
    }

    // Source meta (= 選択中 event の source 情報)。 Phase 4+ で
    // Inspector の Source 行 (`docs/plan_audio_clip.md` §3.9) として
    // 出す予定だが、 当面は Audio Editor 内 footer に出して視認性を
    // 確保する。 multi-event のときは選択中 event の source を参照。
    let footer_event = anchor_idx
        .and_then(|i| audio.events.get(i))
        .or(audio.events.first());
    if let Some(footer_event) = footer_event
        && let Some(audio_source) = app.song_doc.song().media.audio_sources.get(&footer_event.source_id)
    {
        let meta = format!(
            "{}  {} Hz · {} ch · {} frames  ({} events)",
            match &audio_source.path {
                common::model::AudioSourcePath::ProjectRelative(p) => p.display().to_string(),
                common::model::AudioSourcePath::Absolute(p) => p.display().to_string(),
                common::model::AudioSourcePath::Generated { id } => format!("Generated #{id}"),
            },
            audio_source.sample_rate,
            audio_source.channels,
            audio_source.frames,
            audio.events.len(),
        );
        ui.label_at(
            "audio_editor_meta",
            &meta,
            area.x + pad,
            area.y + area.h - 18.0,
            10.0,
            TEXT,
        );
    }
}
