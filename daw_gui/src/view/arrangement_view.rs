//! Arrangement view (左: track headers / 右: timeline canvas)。
//!
//! 現状の対応:
//! - 描画: 背景 / ルーラ / レーン罫線 / バー罫線 / クリップ矩形 / playhead / loop band
//! - クリック (canvas 内): クリップ HIT → SelectClip (Shift で additive)
//! - クリック (track header): SelectTrack
//! - クリック (mute/solo): ToggleTrackMute / ToggleTrackSolo
//! - ダブルクリック: クリップ → ピアノロール / 空白 → CreateClip
//! - Wheel: 横スクロール / Ctrl+Wheel: 横ズーム
//!
//! TODO (次イテレーション): drag move / resize / marquee / track rename /
//! loop band drag。

use daw_ui_core::{
    BarBeatGridStyle, Edit, TimeDisplay, TimeMapping, Ui, ViewportState1D,
};
use daw_ui_renderer::{Color, LineBatch, LineSegment, Rect, RectCommand};

use crate::app::{ARRANGE_TRACK_HEIGHT, AppData, AppEvent, ClipRef};

const TRACK_HEADER_W: f32 = 160.0;
const RULER_H: f32 = 20.0;

const COLOR_BG: Color = Color { r: 0.11, g: 0.11, b: 0.13, a: 1.0 };
const COLOR_RULER_BG: Color = Color { r: 0.14, g: 0.14, b: 0.16, a: 1.0 };
const COLOR_HEADER_BG: Color = Color { r: 0.16, g: 0.16, b: 0.18, a: 1.0 };
const COLOR_HEADER_SELECTED: Color = Color { r: 0.27, g: 0.35, b: 0.48, a: 1.0 };
const COLOR_LANE_LINE: Color = Color { r: 0.19, g: 0.19, b: 0.22, a: 1.0 };
const COLOR_CLIP: Color = Color { r: 0.27, g: 0.51, b: 0.71, a: 0.92 };
const COLOR_CLIP_SELECTED: Color = Color { r: 0.47, g: 0.71, b: 0.90, a: 0.95 };
const COLOR_CLIP_BORDER: Color = Color { r: 0.08, g: 0.08, b: 0.12, a: 1.0 };
const COLOR_CLIP_TEXT: Color = Color { r: 0.94, g: 0.94, b: 0.94, a: 1.0 };
const COLOR_HEADER_TEXT: Color = Color { r: 0.85, g: 0.88, b: 0.92, a: 1.0 };
const COLOR_PLAYHEAD: Color = Color { r: 0.90, g: 0.30, b: 0.30, a: 1.0 };
const COLOR_LOOP_BAND: Color = Color { r: 0.31, g: 0.78, b: 0.90, a: 0.30 };
const COLOR_LOOP_EDGE: Color = Color { r: 0.31, g: 0.78, b: 0.90, a: 1.0 };
const COLOR_BTN_NEUTRAL: Color = Color { r: 0.22, g: 0.22, b: 0.26, a: 1.0 };
const COLOR_BTN_MUTE: Color = Color { r: 0.78, g: 0.35, b: 0.27, a: 1.0 };
const COLOR_BTN_SOLO: Color = Color { r: 0.90, g: 0.78, b: 0.31, a: 1.0 };

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    // 全体背景
    ui.heavy("arr_bg", |hctx| {
        hctx.cached((area.w.to_bits(), area.h.to_bits()), |hctx| {
            hctx.push_rect(RectCommand {
                rect: area,
                fill: COLOR_BG,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        });
    });

    let header_area = Rect {
        x: area.x,
        y: area.y,
        w: TRACK_HEADER_W,
        h: area.h,
    };
    let canvas_area = Rect {
        x: area.x + TRACK_HEADER_W,
        y: area.y,
        w: area.w - TRACK_HEADER_W,
        h: area.h,
    };

    draw_canvas(app, ui, canvas_area);
    draw_track_headers(app, ui, header_area);

    // 入力処理: pointer.pos が canvas_area 内なら hit-test。pointer events は
    // ui.pointer() から取れるが、push_edit でモデル変更を流す。
    handle_canvas_input(app, ui, canvas_area);

    // M8 Phase 32: file drop placeholder。実機能 (audio clip 化) は別フェーズ。
    // 今は status bar に drop されたパスを表示するのみ。
    if ui.is_file_hovering_in_rect(canvas_area) {
        ui.push_rect(RectCommand {
            rect: canvas_area,
            fill: Color::TRANSPARENT,
            border: Color::rgb(0.55, 0.85, 0.95),
            border_width: 2.0,
            radius: [0.0; 4],
            clip_rect: None,
        });
    }
    if let Some(paths) = ui.take_file_drop_in_rect(canvas_area) {
        let display = paths
            .iter()
            .map(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("(unnamed)")
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join(", ");
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.status_message = format!("dropped: {display}");
        }));
    }
}

fn draw_canvas(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let zoom = app.arrange_zoom_x.max(1.0);
    let scroll_beat = app.arrange_scroll_beat.max(0.0);
    let track_count = app.song.tracks.len() as u32;
    let playhead = app.playhead_beat;
    let loop_start = app.loop_start_beat();
    let loop_end = app.loop_end_beat();
    let selected_clips = app.selected_clips.clone();

    ui.heavy("arr_canvas", |hctx| {
        // viewport_key: 描画に影響する状態をすべて含める。
        let key = (
            area.w.to_bits(),
            area.h.to_bits(),
            zoom.to_bits(),
            scroll_beat.to_bits(),
            track_count,
            playhead.unwrap_or(f32::NAN).to_bits(),
            loop_start.to_bits(),
            loop_end.to_bits(),
            // クリップの集合 hash も含めたいが簡略化: 数とトラック数で代用。
            // 実際に再構築の cost は大きくないのでクリップ list のサマリで OK。
            app.song.tracks.iter().map(|t| t.clips.len()).sum::<usize>(),
            selected_clips.len(),
        );
        hctx.cached(key, |hctx| {
            // 背景 (canvas 部分)。
            hctx.push_rect(RectCommand {
                rect: area,
                fill: COLOR_BG,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
            // ルーラ背景。
            hctx.push_rect(RectCommand {
                rect: Rect { x: area.x, y: area.y, w: area.w, h: RULER_H },
                fill: COLOR_RULER_BG,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });

            // ループバンド (ルーラ内)。
            if loop_end > loop_start {
                let lx = area.x + (loop_start - scroll_beat) * zoom;
                let lw = (loop_end - loop_start) * zoom;
                let visible_x = lx.max(area.x);
                let visible_right = (lx + lw).min(area.x + area.w);
                let visible_w = visible_right - visible_x;
                if visible_w > 0.0 {
                    hctx.push_rect(RectCommand {
                        rect: Rect {
                            x: visible_x,
                            y: area.y,
                            w: visible_w,
                            h: RULER_H,
                        },
                        fill: COLOR_LOOP_BAND,
                        border: Color::TRANSPARENT,
                        border_width: 0.0,
                        radius: [0.0; 4],
                        clip_rect: None,
                    });
                }
            }

            // バー罫線は library の `ui.bar_beat_grid` に委譲 (heavy ブロックの外で
            // 呼び出し)。ここでは描かない。

            // レーン罫線。
            let mut lane_lines: Vec<LineSegment> = Vec::new();
            for i in 0..=track_count {
                let y = area.y + RULER_H + (i as f32) * ARRANGE_TRACK_HEIGHT;
                if y > area.y + area.h {
                    break;
                }
                lane_lines.push(LineSegment {
                    a: [area.x, y],
                    b: [area.x + area.w, y],
                    color: COLOR_LANE_LINE,
                });
            }
            if !lane_lines.is_empty() {
                hctx.push_lines(LineBatch {
                    segments: lane_lines,
                    line_width_px: 1.0,
                    clip_rect: Some(area),
                });
            }

            // クリップ矩形。
            for (t_idx, t) in app.song.tracks.iter().enumerate() {
                let y = area.y + RULER_H + (t_idx as f32) * ARRANGE_TRACK_HEIGHT;
                if y > area.y + area.h {
                    break;
                }
                let h = ARRANGE_TRACK_HEIGHT - 2.0;
                for (c_idx, c) in t.clips.iter().enumerate() {
                    let x = area.x + (c.start_beat as f32 - scroll_beat) * zoom;
                    let w = ((c.length_beats as f32) * zoom).max(2.0);
                    if x + w < area.x || x > area.x + area.w {
                        continue;
                    }
                    let r = ClipRef {
                        track: t_idx as u32,
                        clip: c_idx as u32,
                    };
                    let selected = selected_clips.contains(&r);
                    let body = if selected {
                        COLOR_CLIP_SELECTED
                    } else {
                        COLOR_CLIP
                    };
                    hctx.push_rect(RectCommand {
                        rect: Rect { x, y: y + 1.0, w, h },
                        fill: body,
                        border: COLOR_CLIP_BORDER,
                        border_width: 1.0,
                        radius: [3.0; 4],
                        clip_rect: Some(area),
                    });
                    if w > 28.0 {
                        hctx.label_at(
                            ("arr_clip_text", t_idx, c_idx),
                            &c.name,
                            x + 4.0,
                            y + h * 0.5 - 4.0,
                            11.0,
                            COLOR_CLIP_TEXT,
                        );
                    }
                }
            }

            // ループバンドの縁線 (ruler 内)。
            if loop_end > loop_start {
                let lx = area.x + (loop_start - scroll_beat) * zoom;
                let rx = area.x + (loop_end - scroll_beat) * zoom;
                let mut edges = Vec::new();
                if lx >= area.x && lx <= area.x + area.w {
                    edges.push(LineSegment {
                        a: [lx, area.y],
                        b: [lx, area.y + RULER_H],
                        color: COLOR_LOOP_EDGE,
                    });
                }
                if rx >= area.x && rx <= area.x + area.w {
                    edges.push(LineSegment {
                        a: [rx, area.y],
                        b: [rx, area.y + RULER_H],
                        color: COLOR_LOOP_EDGE,
                    });
                }
                if !edges.is_empty() {
                    hctx.push_lines(LineBatch {
                        segments: edges,
                        line_width_px: 1.5,
                        clip_rect: Some(area),
                    });
                }
            }

            // playhead。
            if let Some(beat) = playhead {
                let x = area.x + (beat - scroll_beat) * zoom;
                if x >= area.x && x <= area.x + area.w {
                    hctx.push_lines(LineBatch {
                        segments: vec![LineSegment {
                            a: [x, area.y],
                            b: [x, area.y + area.h],
                            color: COLOR_PLAYHEAD,
                        }],
                        line_width_px: 1.5,
                        clip_rect: Some(area),
                    });
                }
            }
        });
    });

    // bar / beat grid を library widget で重ねる (RULER_H 以下のキャンバス部分のみ)。
    // 半透明 overlay として clip 矩形の上から薄く乗る。
    let bpm = (app.bpm() as f64).max(1.0);
    let mapping = TimeMapping {
        sample_rate: 48_000.0,
        tempo_bpm: bpm,
        time_sig: (4, 4),
        display: TimeDisplay::BarBeat,
    };
    let spb = mapping.samples_per_beat();
    let visible_beats = (area.w / zoom).max(1.0) as f64;
    let viewport = ViewportState1D::new(scroll_beat as f64 * spb, visible_beats * spb);
    let grid_area = Rect {
        x: area.x,
        y: area.y + RULER_H,
        w: area.w,
        h: (area.h - RULER_H).max(0.0),
    };
    ui.bar_beat_grid(
        "arrange_grid",
        grid_area,
        mapping,
        viewport,
        BarBeatGridStyle::default(),
    );
}

fn draw_track_headers(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    // ルーラ部分の埋め色。
    ui.heavy("arr_header_bg", |hctx| {
        hctx.cached((area.w.to_bits(), area.h.to_bits()), |hctx| {
            hctx.push_rect(RectCommand {
                rect: Rect { x: area.x, y: area.y, w: area.w, h: RULER_H },
                fill: COLOR_RULER_BG,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        });
    });

    let rows_y = area.y + RULER_H;
    let row_h = ARRANGE_TRACK_HEIGHT;
    let pad = 4.0;

    for (i, t) in app.song.tracks.iter().enumerate() {
        let row_y = rows_y + (i as f32) * row_h;
        if row_y > area.y + area.h {
            break;
        }
        let is_selected = app.selected_track == i as u32;
        let track_idx = i as u32;

        // 背景 (track 選択でハイライト)。
        ui.heavy(("arr_header_row_bg", i), |hctx| {
            hctx.cached((i, t.muted, t.solo, is_selected), |hctx| {
                hctx.push_rect(RectCommand {
                    rect: Rect {
                        x: area.x,
                        y: row_y,
                        w: area.w,
                        h: row_h - 1.0,
                    },
                    fill: if is_selected {
                        COLOR_HEADER_SELECTED
                    } else {
                        COLOR_HEADER_BG
                    },
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: None,
                });
            });
        });

        // トラック名 (クリックで select)。Button にする (右の M/S と並べる)。
        let name = if t.name.is_empty() {
            format!("Track {}", i + 1)
        } else {
            t.name.clone()
        };
        let name_w = area.w - pad * 2.0 - 22.0 * 2.0 - 4.0;
        let name_rect = Rect {
            x: area.x + pad,
            y: row_y + pad,
            w: name_w,
            h: 20.0,
        };
        ui.button_at(
            ("arr_header_select", i),
            &name,
            name_rect,
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SelectTrack(track_idx))
                })
            },
        );
        // M8 Phase 25: track header の右クリックで Rename / Delete メニュー。
        // Move Up/Dn は下段の Up/Dn ボタンと役割重複するので menu には入れない。
        ui.context_menu_for(name_rect, &["Rename", "Delete"], move |idx| {
            Edit::mutate(move |app: &mut AppData| match idx {
                0 => app.handle_event(AppEvent::BeginRenameTrack(track_idx)),
                1 => app.handle_event(AppEvent::DeleteTrack(track_idx)),
                _ => {}
            })
        });
        // 名前テキスト (Button で隠れない位置に被せる)
        let _ = name_w;

        // M (mute) ボタン
        let m_x = area.x + area.w - pad - 22.0 * 2.0 - 2.0;
        let muted = t.muted;
        ui.button_at(
            ("arr_header_mute", i),
            "M",
            Rect { x: m_x, y: row_y + pad, w: 22.0, h: 20.0 },
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleTrackMute(track_idx))
                })
            },
        );
        // M ボタンのアクティブ色 (押された後の状態を背景帯で表現)。
        // 上記 button_at の位置と被せる帯 hint。
        let _ = muted;

        // S (solo) ボタン
        let s_x = m_x + 22.0 + 2.0;
        let solo = t.solo;
        ui.button_at(
            ("arr_header_solo", i),
            "S",
            Rect { x: s_x, y: row_y + pad, w: 22.0, h: 20.0 },
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleTrackSolo(track_idx))
                })
            },
        );
        let _ = solo;

        // 下段: ▲/▼/✕ ボタン
        let bottom_y = row_y + 28.0;
        let small_w = 22.0;
        let btn_h = 18.0;
        ui.button_at(
            ("arr_header_up", i),
            "Up",
            Rect { x: area.x + pad, y: bottom_y, w: small_w, h: btn_h },
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::MoveTrackUp(track_idx))
                })
            },
        );
        ui.button_at(
            ("arr_header_down", i),
            "Dn",
            Rect {
                x: area.x + pad + small_w + 2.0,
                y: bottom_y,
                w: small_w,
                h: btn_h,
            },
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::MoveTrackDown(track_idx))
                })
            },
        );
        ui.button_at(
            ("arr_header_del", i),
            "x",
            Rect {
                x: area.x + area.w - pad - small_w,
                y: bottom_y,
                w: small_w,
                h: btn_h,
            },
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::DeleteTrack(track_idx))
                })
            },
        );

        // 色付け hint (mute/solo を small bar で示す)。
        let mut hints: Vec<RectCommand> = Vec::new();
        if muted {
            hints.push(RectCommand {
                rect: Rect { x: m_x, y: row_y + pad + 18.0, w: 22.0, h: 2.0 },
                fill: COLOR_BTN_MUTE,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        }
        if solo {
            hints.push(RectCommand {
                rect: Rect { x: s_x, y: row_y + pad + 18.0, w: 22.0, h: 2.0 },
                fill: COLOR_BTN_SOLO,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        }
        if !hints.is_empty() {
            ui.heavy(("arr_header_hints", i), |hctx| {
                hctx.cached((muted, solo), |hctx| {
                    for h in &hints {
                        hctx.push_rect(h.clone());
                    }
                });
            });
        }

        let _ = COLOR_BTN_NEUTRAL;
        let _ = COLOR_HEADER_TEXT;
    }
}

/// canvas 領域でのマウス入力処理。Edit dispatch は HeavyCtx 経由 (`push_edit` は
/// 通常 `Ui` 上で `pub(crate)` のため)。
fn handle_canvas_input(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let pointer = ui.pointer();
    let Some((px, py)) = pointer.pos else { return };
    if !area.contains(px, py) {
        return;
    }
    let zoom = app.arrange_zoom_x.max(1.0);
    let scroll_beat = app.arrange_scroll_beat;
    let modifiers = pointer.modifiers;
    let area_x = area.x;
    let area_y = area.y;

    ui.heavy("arr_input", |hctx| {
        // wheel: Ctrl で zoom、それ以外で 横 pan。scroll_delta は 1 frame の累積 px。
        let (sx, sy) = pointer.scroll_delta;
        let scroll_signal = sy.abs() + sx.abs();
        if scroll_signal > 0.001 {
            if modifiers.ctrl {
                let factor = (sy * 0.005).exp();
                let new_zoom = (zoom * factor).clamp(2.0, 400.0);
                hctx.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetArrangeZoom(new_zoom))
                }));
            } else {
                let dx_beats = -(sy + sx) / zoom;
                let new_scroll = (scroll_beat + dx_beats).max(0.0);
                hctx.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetArrangeScroll(new_scroll))
                }));
            }
        }

        // クリック (release で判定): hit-test + ダブルクリック検出。
        if pointer.primary_just_released && py >= area_y + RULER_H {
            let additive = modifiers.shift;
            // 全ロジックを Edit::mutate 内に閉じ込め、app.last_click と
            // app.song に同時アクセスして判定する。
            hctx.push_edit(Edit::mutate(move |app: &mut AppData| {
                let now = std::time::Instant::now();
                let is_double = matches!(
                    app.last_click,
                    Some((t, lx, ly))
                        if now.duration_since(t)
                            < std::time::Duration::from_millis(400)
                            && (px - lx).abs() < 5.0
                            && (py - ly).abs() < 5.0
                );

                let zoom = app.arrange_zoom_x.max(1.0);
                let scroll_beat = app.arrange_scroll_beat;
                let beat = ((px - area_x) / zoom + scroll_beat) as f64;
                let track = ((py - area_y - RULER_H) / ARRANGE_TRACK_HEIGHT) as u32;

                if (track as usize) >= app.song.tracks.len() {
                    app.last_click = Some((now, px, py));
                    return;
                }

                let hit: Option<ClipRef> = app
                    .song
                    .tracks
                    .get(track as usize)
                    .and_then(|t| {
                        t.clips.iter().enumerate().find_map(|(c_idx, c)| {
                            if beat >= c.start_beat
                                && beat <= c.start_beat + c.length_beats
                            {
                                Some(ClipRef { track, clip: c_idx as u32 })
                            } else {
                                None
                            }
                        })
                    });

                if is_double {
                    if let Some(target) = hit {
                        // 既存クリップをダブルクリック → ピアノロールへ
                        app.handle_event(AppEvent::SelectClip { target, additive: false });
                        app.handle_event(AppEvent::SelectBottomPanel(1));
                    } else {
                        // 空白をダブルクリック → クリップ作成
                        let snapped = beat.floor().max(0.0);
                        app.handle_event(AppEvent::CreateClip { track, start_beat: snapped });
                        app.handle_event(AppEvent::SelectBottomPanel(1));
                    }
                    app.last_click = None;
                } else {
                    // 単発クリック: HIT → SelectClip / 空白 → ClearSelection (!additive)
                    if let Some(target) = hit {
                        app.handle_event(AppEvent::SelectClip { target, additive });
                    } else if !additive {
                        app.handle_event(AppEvent::ClearSelection);
                    }
                    app.last_click = Some((now, px, py));
                }
            }));
        }
    });
}
