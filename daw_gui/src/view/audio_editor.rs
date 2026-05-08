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

use daw_ui_core::{
    ChannelLayout, DragKind, Edit, SampleSlices, TimeDisplay, TimeMapping, TimeRulerStyle, Ui,
    ViewportState1D, WaveformRenderMode, WaveformSource, WaveformStyle, WaveformView,
};
use daw_ui_renderer::{Color, LineBatch, LineSegment, Rect};

use crate::app::{AppData, AppEvent, AudioEventTrimSide};

const BG: Color = Color { r: 0.10, g: 0.11, b: 0.13, a: 1.0 };
const TEXT: Color = Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 };
const TEXT_DIM: Color = Color { r: 0.62, g: 0.65, b: 0.70, a: 1.0 };
const GHOST: Color = Color { r: 0.95, g: 0.78, b: 0.31, a: 0.85 };

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

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    ui.panel("audio_editor_bg", area, BG, 0.0);

    // 開いている clip を解決。 audio_editor_clip がセットされていない、
    // または範囲外 / 非 audio なら placeholder を出して return (= clip
    // が削除された / Undo で消えた場合の防御)。
    let target = match app.audio_editor_clip {
        Some(t) => t,
        None => {
            ui.label_at(
                "audio_editor_empty",
                "(Audio Editor: 表示する clip が選択されていません)",
                area.x + 12.0,
                area.y + 18.0,
                12.0,
                TEXT_DIM,
            );
            return;
        }
    };
    let Some(track) = app.song.tracks.get(target.track as usize) else {
        return;
    };
    let Some(clip) = track.clips.get(target.clip as usize) else {
        return;
    };
    let Some(common::model::ClipContent::Audio(audio)) =
        app.song.clip_contents.get(&clip.content_id)
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
            clip.name,
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

    // ----- Ruler (MIDI エディタ同様、 song 全体の絶対 bar 番号を表示) -
    // wf_area が clip 全体を全幅マッピングするので、 viewport も clip
    // の time range (= clip.start_beat .. clip.start_beat + length_beats
    // を sample 単位に換算) を見せる。 これで bar 番号は曲全体基準で
    // 表示される (= 例: clip が小節 5 から始まれば左端が "5")。
    let ruler_rect = Rect {
        x: area.x + pad,
        y: area.y + header_h,
        w: (area.w - pad * 2.0).max(0.0),
        h: ruler_h,
    };
    if ruler_rect.w > 0.0 && clip.length_beats > 0.0 {
        let mapping = TimeMapping {
            sample_rate: common::audio_bridge::SAMPLE_RATE as f64,
            tempo_bpm: app.song.bpm as f64,
            time_sig: app.song.time_sig,
            display: TimeDisplay::BarBeat,
        };
        let spb = mapping.samples_per_beat();
        let viewport = ViewportState1D {
            view_start: clip.start_beat * spb,
            view_len: clip.length_beats * spb,
        };
        ui.time_ruler(
            "audio_editor_ruler",
            ruler_rect,
            mapping,
            viewport,
            TimeRulerStyle::default(),
        );
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
            TEXT_DIM,
        );
        return;
    }
    let selected_idx = app.audio_editor_selected_event.unwrap_or(0);
    let clip_len_beats = clip.length_beats.max(1e-6); // 0 div 防御
    // PR-D 段階 3: rect-based hit-test 用に px → beats 換算係数を準備。
    // wf_area.w (px) = clip_len_beats (beats) なので 1 px = beats_per_px。
    let beats_per_px = (clip_len_beats / wf_area.w as f64).max(1e-9);
    let target = match app.audio_editor_clip {
        Some(t) => t,
        None => return,
    };
    let clip_id = clip.id;

    for (idx, event) in audio.events.iter().enumerate() {
        let Some(buffer) = app.audio_source_cache.get(event.source_id) else {
            // 当該 event は decode 待ち / missing source → 透けて見える
            // 範囲だけマーカー描画 (= 他 event は描く)。
            continue;
        };

        // event rect (clip 内の time range を wf_area 全幅にマップ)
        let evt_x_start = (event.event_start_in_clip_beats / clip_len_beats) as f32;
        let evt_x_end = ((event.event_start_in_clip_beats + event.event_length_beats)
            / clip_len_beats) as f32;
        let event_rect = Rect {
            x: wf_area.x + evt_x_start.clamp(0.0, 1.0) * wf_area.w,
            y: wf_area.y,
            w: ((evt_x_end - evt_x_start) * wf_area.w).max(2.0),
            h: wf_area.h,
        };

        let planes_borrowed: Vec<&[f32]> =
            buffer.samples.iter().map(Vec::as_slice).collect();
        let event_len_frames = event
            .source_end_frames
            .saturating_sub(event.source_start_frames)
            .max(1);

        let source = WaveformSource {
            samples: SampleSlices::Planar(&planes_borrowed),
            valid_len: buffer.frames as usize,
            generation: event.source_id as u64,
            sample_rate: buffer.sample_rate,
        };
        let view = WaveformView {
            start_sample: event.source_start_frames,
            len_samples: event_len_frames,
            vertical_gain: 1.0,
        };
        let is_selected = idx == selected_idx;
        let fg = if is_selected {
            Color::rgba(0.65, 0.95, 1.0, 0.95)
        } else {
            Color::rgba(0.45, 0.70, 0.85, 0.85)
        };
        let style = WaveformStyle {
            fg,
            fg_clipped: Color::rgb(0.95, 0.45, 0.40),
            fill: None,
            baseline: Some(Color::rgba(1.0, 1.0, 1.0, 0.15)),
            channel_layout: ChannelLayout::Stack,
            render_mode: WaveformRenderMode::Auto,
            line_width_px: 1.0,
        };
        let _ = ui.waveform(
            ("audio_editor_wf", clip.id, idx),
            event_rect,
            source,
            view,
            style,
        );

        // Selection border (= 選択中のみ視認できる枠)。 1 px 太い線で
        // 上下左右を marker。 push_rect で半透明帯にしても良いが、
        // border の方が波形を遮らない。
        if is_selected {
            let border_color = Color::rgba(0.95, 0.78, 0.31, 0.85);
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

        // ----- Hit-test (PR-D 段階 3 / gui_01 #026) -----
        // event_rect を [left grip, center, right grip] に分割。 left/right
        // grip は trim drag、 center は move drag。 grip 幅は 6 px、 event
        // が 18 px 未満なら grip を出さず center のみ (= 無理に trim
        // しなくて良い、 操作性優先)。
        const GRIP_W: f32 = 6.0;
        let usable_w = event_rect.w.max(0.0);
        let (lw, rw) = if usable_w >= GRIP_W * 3.0 {
            (GRIP_W, GRIP_W)
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
            } else if kind == DragKind::Released {
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
            } else if kind == DragKind::Released {
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

        // 中央 drag = 移動 (start = select 切替、 continuing = ghost
        // 表示、 released = SetAudioEventStart commit)。
        if center_band.w > 0.0
            && let Some(drag) =
                ui.take_drag_in_rect(("audio_editor_move", clip_id, idx), center_band)
        {
            let dx = drag.delta.0;
            let kind = drag.kind;
            if kind == DragKind::Started {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SelectAudioEditorEvent(Some(idx)));
                }));
            } else if kind == DragKind::Continuing && dx.abs() > 1.0 {
                push_move_ghost(ui, event_rect, wf_area, dx);
            } else if kind == DragKind::Released {
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

        // 単発 click (drag 開始しなかった場合) = select。 take_drag_in_rect
        // が press frame に consume_pointer_click を呼ぶので、 同 frame
        // の take_primary_press_in_rect は None になり二重 select されない
        // (= drag press から 1 px も動かず即 release した場合だけ反応)。
        if let Some(_press) = ui.take_primary_press_in_rect(event_rect) {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SelectAudioEditorEvent(Some(idx)));
            }));
        }

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
                        app.handle_event(AppEvent::DeleteAudioEvent {
                            clip: target,
                            event_idx: idx,
                        });
                    }
                    2 => {
                        app.action_open_audio_event_dialog(target, evt_end_beats);
                    }
                    _ => {}
                }));
            },
        );
    }

    // 空白領域 (= waveform area で event 上にない場所) への file drop
    // で、 drop 位置を `position_in_clip_beats` に変換して
    // AddAudioEventFromFile を発火。 1 path のみ採用 (= multi-drop は
    // 先頭ファイルのみ; 残りは無視、 status_message に noted せず
    // 静かにスキップ)。
    if let Some(drop) = ui.take_file_drop_in_rect(wf_area)
        && let Some(path) = drop.paths.into_iter().next()
    {
        let pos_beats =
            ((drop.position.0 - wf_area.x).max(0.0) as f64) * beats_per_px;
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
        let in_clip = ((px - wf_area.x).max(0.0) as f64) * beats_per_px;
        Some(in_clip.clamp(0.0, clip_len_beats))
    });
    if app.audio_editor_hover_beat_in_clip != hover_in_clip {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.audio_editor_hover_beat_in_clip = hover_in_clip;
        }));
    }

    // ----- Playhead 線 (Phase 2 PR7) -----
    // 再生中 / Stop 後 (= playhead_beat is Some) かつ playhead が現
    // clip の範囲内なら、 wf_area 上に縦線を重ねる。 視覚的に「曲全体の
    // どこを再生しているか」 が Audio Editor 内でも分かる。 view は
    // event.event_start_in_clip_beats から event_length_beats まで
    // (= 1 event = clip 全体) を全幅マッピングしているので、
    // x = wf_area.x + (in_clip_beats / clip.length_beats) * wf_area.w。
    if let Some(ph_beat) = app.playhead_beat {
        let ph_beat = ph_beat as f64;
        let clip_start = clip.start_beat;
        let clip_end = clip_start + clip.length_beats;
        if ph_beat >= clip_start && ph_beat < clip_end && clip.length_beats > 0.0 {
            let in_clip = ph_beat - clip_start;
            let x = wf_area.x + (in_clip / clip.length_beats) as f32 * wf_area.w;
            let color = Color::rgba(1.0, 0.55, 0.20, 0.9);
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
    let footer_event = audio.events.get(selected_idx).or(audio.events.first());
    if let Some(footer_event) = footer_event
        && let Some(audio_source) = app.song.audio_sources.get(&footer_event.source_id)
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
            TEXT_DIM,
        );
    }
}
