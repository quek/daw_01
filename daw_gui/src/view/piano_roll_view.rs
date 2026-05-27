//! Piano roll: gui_01 の `Ui::piano_roll` widget でノート描画 / 編集 / velocity lane / playhead
//! を行う。
//! - Shift+drag で加算 rect select (widget が自動)
//! - Insert キー / 空白上 dbl-click で AddNote (widget Insert + daw_01 エミュレート、1/16 snap)
//! - drag move / 端 drag resize / Delete キー は widget 内蔵
//! - wheel: Ctrl→ZoomY, Shift→ScrollX, plain→TopPitch (drag 中は無効)

use std::sync::Arc;

use daw_ui_core::{
    Edit, MoveDelta, Note, PianoRollEditRequest, PianoRollScale, PianoRollScaleMode,
    PianoRollStyle, PianoRollView, ResizeDelta, ToggleButtonStyle, Ui, note_hit,
};
use daw_ui_renderer::{Color, Rect};

use crate::app::{AppData, AppEvent, ClipRef};
use crate::view::snap::{self, SNAP_LABELS};

const KEYBOARD_W: f32 = 56.0;
const VEL_LANE_H: f32 = 60.0;
const RULER_H: f32 = 20.0;
const TOOLBAR_H: f32 = 24.0;

const COLOR_BG: Color = Color { r: 0.10, g: 0.10, b: 0.12, a: 1.0 };
const COLOR_HINT: Color = Color { r: 0.55, g: 0.58, b: 0.65, a: 1.0 };

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
    // 上部 24 px を Snap toolbar に。残りを widget 本体 (body) に渡す。
    let toolbar_rect = Rect { x: area.x, y: area.y, w: area.w, h: TOOLBAR_H };
    let body = Rect {
        x: area.x,
        y: area.y + TOOLBAR_H,
        w: area.w,
        h: (area.h - TOOLBAR_H).max(0.0),
    };
    draw_snap_toolbar(app, ui, toolbar_rect);

    let Some(target) = app.selected_clip else {
        // クリップ未選択時のプレースホルダ
        ui.panel("pr_bg_empty", body, COLOR_BG, 0.0);
        ui.label_at(
            "pr_no_clip",
            "(\u{30af}\u{30ea}\u{30c3}\u{30d7}\u{304c}\u{9078}\u{629e}\u{3055}\u{308c}\u{3066}\u{3044}\u{307e}\u{305b}\u{3093})",
            body.x + 12.0,
            body.y + 12.0,
            12.0,
            COLOR_HINT,
        );
        return;
    };

    // widget が ruler / velocity lane を内蔵 (M13 Phase 55 で ruler 追加)、
    // grid 部分は body から keyboard / ruler / vel lane を引いた領域。
    // note hit detection はこの grid_rect を使うので、widget 内部 layout と
    // 揃えておく (rect.y から ruler_h、その下に keyboard+grid、最下段に vel_lane)。
    let grid_h = body.h - VEL_LANE_H - RULER_H;
    let grid_rect = Rect {
        x: body.x + KEYBOARD_W,
        y: body.y + RULER_H,
        w: body.w - KEYBOARD_W,
        h: grid_h,
    };

    // auto-fit (X キー / Fit ボタン / SelectClip 経由) のために、現フレームの grid 領域
    // サイズを記録する。1 frame 遅延で OK (X キー押下の次フレームに反映される)。
    // 同フレーム内で `pending_pianoroll_fit` が立っていたら消費して fit を再実行
    // (Piano Roll タブ未表示で clip 選択 → タブを開いた初回フレームの fit 確定)。
    let grid_size = (grid_rect.w, grid_rect.h);
    if app.last_pianoroll_grid_size != grid_size || app.pending_pianoroll_fit {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.last_pianoroll_grid_size = grid_size;
            if app.pending_pianoroll_fit {
                app.pending_pianoroll_fit = false;
                app.handle_event(AppEvent::FitPianoRollToClip);
            }
        }));
    }

    let widget_notes = build_widget_notes(app, target);
    let zoom_x = app.pianoroll_zoom_x.max(4.0);
    let zoom_y = app.pianoroll_zoom_y.max(6.0);
    let loop_range = if app.song.loop_end_beat > app.song.loop_start_beat {
        Some((app.song.loop_start_beat, app.song.loop_end_beat))
    } else {
        None
    };
    // Phase 7 B5 (`docs/plan_scale.html` §4.4, gui_01 #042): 編集中 clip の
    // `start_beat` 位置の scale を採用する (= 単一 view 内で動的に scale
    // が変わらないため、 piano_roll が安定して編集できる)。 scale_changes が
    // 空 / 該当 event 無し / selected_clip None なら view.scale = None で旧
    // 挙動互換 (= 機能 OFF、 既存 .daw file の regression なし)。
    let scale_beat = app
        .song
        .tracks
        .get(target.track as usize)
        .and_then(|t| t.clips.get(target.clip as usize))
        .map(|c| c.start_beat)
        .unwrap_or(0.0);
    let scale = app.song.scale_at(scale_beat).map(|sc| PianoRollScale {
        root: sc.root,
        in_scale_mask: sc.scale.pitch_class_mask(),
        mode: if app.piano_roll_fold {
            PianoRollScaleMode::Fold
        } else {
            PianoRollScaleMode::Highlight
        },
    });

    let view = PianoRollView {
        start_beat: app.pianoroll_scroll_beat as f64,
        len_beats: (grid_rect.w / zoom_x) as f64,
        pitch_top: app.pianoroll_top_pitch as f32,
        pitch_visible: grid_h / zoom_y,
        keyboard_w: KEYBOARD_W,
        notes_generation: app.pianoroll_notes_generation,
        velocity_lane_h: VEL_LANE_H,
        playhead_beat: app.playhead_beat.map(|b| b as f64),
        ruler_h: RULER_H,
        bpm: app.song.bpm,
        time_sig: app.song.time_sig,
        snap: snap::piano_roll_snap_config(app),
        loop_range,
        scale,
        // Phase 7 B5 follow-up (gui_01 #042 Phase 70b): Highlight mode + Snap
        // on Draw で widget の drag preview pitch も最寄り in-scale に snap。
        // Fold mode / scale = None / Snap on Draw OFF では無関係。
        snap_pitch_during_drag: app.snap_on_draw,
    };
    let style = PianoRollStyle::default();
    let resize_handle_px = style.resize_handle_px;

    let make_edit = move |req: PianoRollEditRequest| -> Edit<AppData> {
        match req {
            PianoRollEditRequest::Add(notes) => {
                let Some(n) = notes.into_iter().next() else {
                    return Edit::mutate(|_| {});
                };
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::AddNote {
                        track: target.track,
                        clip: target.clip,
                        start_beat: n.start_beat,
                        duration: app.last_note_duration_beats,
                        pitch: n.pitch,
                    });
                })
            }
            PianoRollEditRequest::Delete(notes) => {
                let ids: Vec<u32> = notes.iter().map(|n| n.id).collect();
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetNoteSelection(ids.clone()));
                    app.handle_event(AppEvent::DeleteSelectedNotes);
                })
            }
            PianoRollEditRequest::Move(deltas) => {
                let entries: Vec<(u32, f64, u8)> =
                    deltas.iter().map(|d: &MoveDelta| (d.0, d.3, d.4)).collect();
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetNotePositions(entries.clone()));
                })
            }
            PianoRollEditRequest::Resize(deltas) => {
                let entries: Vec<(u32, f64, f64)> = deltas
                    .iter()
                    .map(|d: &ResizeDelta| (d.0, d.3, d.4))
                    .collect();
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ResizeNotes(entries.clone()));
                })
            }
            PianoRollEditRequest::Select { next, .. } => Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetNoteSelection(next.clone()));
            }),
            PianoRollEditRequest::SetLyrics(updates) => {
                // gui_01 #017 (M14 Phase 59): widget が L キー編集 → Enter
                // commit 時に 1 batch で発行する歌詞分配 request。 widget は
                // 編集対象 clip を context として知らないので、 piano_roll_view
                // が描画中の `target` (ClipRef) を closure に capture して渡す。
                let target_clip = target;
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetNoteLyrics {
                        clip_ref: target_clip,
                        lyrics: updates.clone(),
                    });
                })
            }
            PianoRollEditRequest::SetVelocity(updates) => {
                // gui_01 #018 (M14 Phase 64): velocity lane 内 drag の release
                // frame で 1 batch 発行される `Vec<(NoteId, u8)>`。 multi-select
                // 中はすべての selected note が同じ絶対値、 単独 hit は単一
                // note のみ含まれる。 drag<3px は widget 側で除外済。
                let entries: Vec<(u32, u8)> = updates.into_iter().collect();
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetNoteVelocities(entries.clone()));
                })
            }
            // gui_01 #041 (M14 Phase 69): ruler 上 plain click / drag の press +
            // continuation frame で逐次発火する seek 要求。 arrangement
            // `SetPlayheadBeat` と完全同形 idiom: playhead_beat 更新 + audio
            // engine への seek IPC 送信。 clip 内 clamp は意図的に行わない
            // (= song-global で自由に動かせる、 arrangement との挙動整合)。
            PianoRollEditRequest::SetPlayheadBeat(beat) => {
                Edit::mutate(move |app: &mut AppData| {
                    let beat = beat.max(0.0);
                    app.playhead_beat = Some(beat as f32);
                    let sr = common::audio_bridge::SAMPLE_RATE as f64;
                    let bpm = app.song.bpm.max(1.0) as f64;
                    let samples = (beat * 60.0 / bpm * sr).max(0.0) as u64;
                    app.send_audio(common::protocol::MainToChild::SeekTo { samples });
                })
            }
            // gui_01 #041 (M14 Phase 69): Shift + ruler drag release で 1 度
            // だけ発火する loop range commit。 既存 AppEvent::SetLoopRange
            // (arrangement / audio_editor と共通) にそのまま流す。
            PianoRollEditRequest::SetLoopRange { start, end } => {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetLoopRange { start, end });
                })
            }
        }
    };

    let resp = ui.piano_roll(
        "piano_roll",
        body,
        &widget_notes,
        view,
        &app.selected_notes,
        &style,
        make_edit,
    );

    // 空白上 dbl-click → AddNote (snap_choice / Alt 押下を尊重)。
    if let Some((px, py)) = ui.take_double_click_in_rect(grid_rect)
        && note_hit(&widget_notes, view, grid_rect, px, py, resize_handle_px).is_none()
    {
        let beat_to_px = grid_rect.w as f64 / view.len_beats.max(1e-6);
        let pitch_to_px = grid_rect.h / view.pitch_visible.max(1e-6);
        let beat_raw = view.start_beat + (px - grid_rect.x) as f64 / beat_to_px;
        let cfg = snap::piano_roll_snap_config(app);
        let alt = ui.pointer().modifiers.alt;
        let snapped_beat = cfg.snap_beat(beat_raw, alt, app.pianoroll_zoom_x).max(0.0);
        let pitch_raw = view.pitch_top - (py - grid_rect.y) / pitch_to_px;
        // ceil(): 描画式 `y = grid.y + (pitch_top - pitch) * pt` の逆関数。
        // round() だと判定領域が視覚行の半行ぶん上にずれて、行下半クリックが 1 下に化ける。
        let pitch = (pitch_raw.ceil() as i32).clamp(0, 127) as u8;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::AddNote {
                track: target.track,
                clip: target.clip,
                start_beat: snapped_beat,
                duration: app.last_note_duration_beats,
                pitch,
            });
        }));
    }

    // wheel handler — note drag 中は無効
    // 一般的な DAW (Ableton Live / Reaper) 流: Ctrl=横ズーム, Alt=縦ズーム,
    // Shift=横スクロール, plain=ピッチスクロール (上下)。
    if resp.dragging.is_none() {
        let pointer = ui.pointer();
        if let Some((px, py)) = pointer.pos
            && grid_rect.contains(px, py)
        {
            let (sx, sy) = pointer.scroll_delta;
            if sy.abs() > 0.001 || sx.abs() > 0.001 {
                let scroll_beat = app.pianoroll_scroll_beat;
                let top_pitch = app.pianoroll_top_pitch as i32;
                let modifiers = pointer.modifiers;
                if modifiers.ctrl {
                    // Ctrl+wheel: 横ズーム。マウス位置の拍を anchor として保持する
                    // (一般的な DAW の挙動)。new_scroll = anchor_beat - (px-grid.x)/new_zoom。
                    let factor = (sy * 0.005).exp();
                    let new_zoom = (zoom_x * factor).clamp(8.0, 400.0);
                    if (new_zoom - zoom_x).abs() > 1e-3 {
                        let anchor_beat =
                            f64::from(scroll_beat) + f64::from((px - grid_rect.x) / zoom_x);
                        let new_scroll =
                            (anchor_beat - f64::from((px - grid_rect.x) / new_zoom)).max(0.0)
                                as f32;
                        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::SetPianoRollZoomX(new_zoom));
                            app.handle_event(AppEvent::SetPianoRollScrollX(new_scroll));
                        }));
                    }
                } else if modifiers.alt {
                    // Alt+wheel: 縦ズーム。マウス位置のピッチを anchor として保持。
                    // top_pitch は u8 なので round 後 best-effort。
                    let factor = (sy * 0.005).exp();
                    let new_zoom = (zoom_y * factor).clamp(6.0, 40.0);
                    if (new_zoom - zoom_y).abs() > 1e-3 {
                        let anchor_pitch =
                            f32::from(top_pitch as u8) - (py - grid_rect.y) / zoom_y;
                        let new_top_f = anchor_pitch + (py - grid_rect.y) / new_zoom;
                        let new_top = new_top_f.round().clamp(11.0, 127.0) as u8;
                        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::SetPianoRollZoomY(new_zoom));
                            app.handle_event(AppEvent::SetPianoRollTopPitch(new_top));
                        }));
                    }
                } else if modifiers.shift {
                    let dx_beats = -(sx + sy) / zoom_x;
                    let new_scroll = (scroll_beat + dx_beats).max(0.0);
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetPianoRollScrollX(new_scroll));
                    }));
                } else {
                    let delta = (sy / 12.0).round() as i32;
                    if delta != 0 {
                        let new_top = (top_pitch + delta).clamp(11, 127) as u8;
                        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::SetPianoRollTopPitch(new_top));
                        }));
                    }
                }
            }
        }
    }

}

/// 上部 24 px の Snap toolbar を描画。
/// 配置: [Snap toggle][snap unit dropdown][Fit] [Fold][Snap on Draw]。
/// Fold / Snap on Draw は Phase 7 B5 (Scale &amp; Root)。
fn draw_snap_toolbar(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect) {
    ui.panel("pr_toolbar_bg", rect, COLOR_BG, 0.0);

    let pad = 6.0;
    let h = 18.0;
    let y = rect.y + (rect.h - h) * 0.5;

    let toggle_w = 60.0;
    let dropdown_w = 90.0;
    let fit_w = 50.0;
    let fold_w = 50.0;
    let snap_draw_w = 100.0;

    let mut x = rect.x + pad;

    ui.toggle_button_at(
        "pr_snap_toggle",
        "Snap",
        Rect { x, y, w: toggle_w, h },
        app.pianoroll_snap_enabled,
        &SNAP_TOGGLE_STYLE,
        |new| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetPianoRollSnapEnabled(new));
            })
        },
    );
    x += toggle_w + pad;

    if let Some(idx) = ui.dropdown(
        "pr_snap_unit",
        Rect { x, y, w: dropdown_w, h },
        SNAP_LABELS,
        app.pianoroll_snap_choice as usize,
    ) {
        let new = idx as u8;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetPianoRollSnapChoice(new));
        }));
    }
    x += dropdown_w + pad;

    ui.button_at("pr_fit", "Fit", Rect { x, y, w: fit_w, h }, || {
        Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::FitPianoRollToClip);
        })
    });
    x += fit_w + pad * 2.0;

    // Phase 7 B5 (`docs/plan_scale.html` §4.4): Fold to Scale toggle。
    // ON で out-of-scale 行を非表示 (Ableton K キー Fold to Scale 相当)。
    // Song.scale_changes が空のときも toggle 自体は active 化できるが、
    // PianoRollView.scale = None なので visual には影響しない。
    ui.toggle_button_at(
        "pr_fold_to_scale",
        "Fold",
        Rect { x, y, w: fold_w, h },
        app.piano_roll_fold,
        &SNAP_TOGGLE_STYLE,
        |_| {
            Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::ToggleFoldToScale);
            })
        },
    );
    x += fold_w + pad;

    // Phase 7 B5 (`docs/plan_scale.html` §5.1): Snap on Draw toggle。
    // ON で note 追加時の pitch を Song.scale_at(beat).snap(pitch) で
    // in-scale に寄せる (Highlight mode 前提、 Fold mode は widget 側で
    // 既に in-scale)。
    ui.toggle_button_at(
        "pr_snap_on_draw",
        "Snap Draw",
        Rect { x, y, w: snap_draw_w, h },
        app.snap_on_draw,
        &SNAP_TOGGLE_STYLE,
        |_| {
            Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::ToggleSnapOnDraw);
            })
        },
    );
}

/// `daw_ui_core::Note` 形式に変換 (毎フレーム alloc、widget 内 cached で性能 OK)。
/// v6 linked clip: notes は `Song.clip_contents` 経由で lookup。 共有 clip
/// 群はすべて同じ notes を見る。
fn build_widget_notes(app: &AppData, target: ClipRef) -> Vec<Note> {
    let Some(track) = app.song.tracks.get(target.track as usize) else {
        return Vec::new();
    };
    let Some(clip) = track.clips.get(target.clip as usize) else {
        return Vec::new();
    };
    app.song
        .clip_notes(clip)
        .iter()
        .enumerate()
        .map(|(i, n)| Note {
            id: i as u32,
            start_beat: n.start_beat,
            len_beats: n.duration_beats,
            pitch: n.pitch,
            velocity: n.velocity,
            lyric: n
                .lyric
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(Arc::from),
        })
        .collect()
}

