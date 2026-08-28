//! **一時ファイル (r.md #77 §9-A)。分割完了後にファイルごと削除する。**
//!
//! `arrangement()` の分割前後で「描かれた `Scene.primitives` の順序つきダンプ / 適用後の
//! `AppData` / 返す `ArrangementResponse` / 要求されたカーソル」が 1 byte も変わらないことを
//! 直接示すための等価性トランスクリプト。
//!
//! `daw_gui/tests/` (別 crate) ではなく **crate 内の `#[cfg(test)]`** に置いている。
//! `UiEphemeral` は `arr_label_cache` / `tempo_map_cache` / `home_toggle_at_first` /
//! `arrange_zoom_history` / `arrange_zoom_anchor` の 5 つが `pub(crate)` で、統合テストからは
//! **そもそも名前が見えない**。後ろ 2 つは arrangement のズーム履歴そのものなので、
//! 見えないまま比較すると「観測できていない state が変わった」を見逃す。
//!
//! 実行:
//! ```text
//! DAW01_ARR_TRANSCRIPT=<path> cargo test -p daw_gui --lib arrangement::equivalence
//! ```
//! `--lib` は crate 内 unit test だけを回すので `CARGO_BIN_EXE_daw_gui` を含む
//! `daw_gui/tests/*.rs` を 1 つもビルド / 実行しない (= daw_gui を起動しない)。

#![allow(clippy::field_reassign_with_default)]

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use common::model::{
    AutomationClip, AutomationContent, AutomationCurve, AutomationLane, AutomationPoint,
    AutomationTarget, Clip, ClipContent, MidiContent, Note, Section, TrackBuiltinParam,
};
use daw_ui_core::{FrameInput, PointerFrame, UiHost};
use daw_ui_platform::{CursorIcon, Modifiers, PhysicalSize};
use daw_ui_renderer::{Rect, Scene};
use tokio::sync::mpsc;

use crate::app::{track_with, AppData};
use crate::dispatcher::{BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher};
use crate::state::transport::TransportState;
use crate::state::ui_ephemeral::UiEphemeral;

use super::{arrangement, ArrangementResponse};

const WIDGET_RECT: Rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };
const ZOOM: f32 = 64.0;
const ROW_H: f32 = 50.0;

// ============================================================
// fixture (`daw_gui/tests/arr_widget.rs:35-115` の複製 + automation / group 拡張)
// ============================================================

fn build_app_with_header(header_w: f32) -> AppData {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    // receiver を leak して drop させない (drop すると send が SendError になり挙動が変わる)。
    std::mem::forget(audio_rx);
    std::mem::forget(plugin_rx);
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let mut app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None,
        event_dispatcher,
        job_dispatcher,
        None,
        None,
        48_000,
    );
    app.ui_prefs.arrange_header_w = header_w;
    app.ui_prefs.arrange_zoom_x = ZOOM;
    app.ui_prefs.arrange_scroll_beat = 0.0;
    app.ui_prefs.arrange_track_row_h = ROW_H;
    app.ui_prefs.arrange_track_top = 0.0;
    app.ui_prefs.arrange_snap_enabled = false;
    app
}

/// 分割対象の全フェーズを踏むための「盛り合わせ」プロジェクト。
///
/// - track 1 = group (子 track 2 を持つ) / track 2 = 子 / track 3 = 平の MIDI track
/// - track 3 に MIDI clip 2 本 (`[0,4)` / `[8,12)`)
/// - track 3 に automation lane 1 本 (expanded、clip `[0,8)` に point 3 つ、うち 1 つは Bezier)
/// - section 2 本 (`[0,4)` / `[8,12)`)
fn build_fixture(header_w: f32) -> AppData {
    let mut app = build_app_with_header(header_w);
    app.edit_song(|song| {
        song.tracks.clear();
        song.tracks.push(track_with(|t| {
            t.id = 1;
            t.name = "Group".to_string();
        }));
        song.tracks.push(track_with(|t| {
            t.id = 2;
            t.name = "Child".to_string();
            t.parent_group_id = Some(1);
        }));
        song.tracks.push(track_with(|t| {
            t.id = 3;
            t.name = "Lead".to_string();
        }));

        let content_a = 101_u32;
        let content_b = 102_u32;
        song.clip_contents.insert(
            content_a,
            ClipContent::Midi(MidiContent {
                notes: vec![Note {
                    pitch: 60,
                    start_beat: 0.0,
                    duration_beats: 1.0,
                    ..Note::default()
                }],
                ..MidiContent::default()
            }),
        );
        song.clip_contents.insert(
            content_b,
            ClipContent::Midi(MidiContent {
                notes: vec![Note {
                    pitch: 64,
                    start_beat: 0.0,
                    duration_beats: 2.0,
                    ..Note::default()
                }],
                ..MidiContent::default()
            }),
        );
        // automation curve の中身 (clip → content_id で参照)。
        song.clip_contents.insert(
            201,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint { id: 1, time_beat: 0.0, value: 0.2, curve: AutomationCurve::Linear },
                    AutomationPoint {
                        id: 2,
                        time_beat: 2.0,
                        value: 0.8,
                        curve: AutomationCurve::Bezier { tension: 0.5 },
                    },
                    AutomationPoint { id: 3, time_beat: 6.0, value: 0.4, curve: AutomationCurve::Linear },
                ],
                next_point_id: 4,
            }),
        );

        let lead = &mut song.tracks[2];
        lead.clips.push(Clip {
            id: 11,
            start_beat: 0.0,
            length_beats: 4.0,
            content_id: content_a,
            ..Clip::default()
        });
        lead.clips.push(Clip {
            id: 12,
            start_beat: 8.0,
            length_beats: 4.0,
            content_id: content_b,
            ..Clip::default()
        });
        lead.automation_lanes.push(AutomationLane {
            id: 1,
            target: AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
            default_value: 0.5,
            enabled: true,
            visible: true,
            height_px: 60,
            clips: vec![AutomationClip {
                id: 1,
                name: String::new(),
                start_beat: 0.0,
                length_beats: 8.0,
                content_id: 201,
                content_offset_beats: 0.0,
            }],
            next_clip_id: 2,
        });

        song.sections.push(Section {
            id: 1,
            name: "A".to_string(),
            color: [0.3, 0.4, 0.5],
            start_beat: 0.0,
            len_beats: 4.0,
        });
        song.sections.push(Section {
            id: 2,
            name: "B".to_string(),
            color: [0.5, 0.4, 0.3],
            start_beat: 8.0,
            len_beats: 4.0,
        });
    });
    app.ui_prefs.expanded_automation_tracks.insert(3);
    app
}

fn modifiers(ctrl: bool, shift: bool, alt: bool) -> Modifiers {
    Modifiers { ctrl, shift, alt, ..Modifiers::empty() }
}

fn no_mods() -> Modifiers {
    modifiers(false, false, false)
}

fn press(x: f32, y: f32, m: Modifiers) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_just_pressed: true,
        primary_pressed: true,
        modifiers: m,
        ..PointerFrame::default()
    }
}

fn hold(x: f32, y: f32, m: Modifiers) -> PointerFrame {
    PointerFrame { pos: Some((x, y)), primary_pressed: true, modifiers: m, ..PointerFrame::default() }
}

fn release(x: f32, y: f32, m: Modifiers) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_just_released: true,
        modifiers: m,
        ..PointerFrame::default()
    }
}

fn frame(p: PointerFrame) -> FrameInput {
    let mut input = FrameInput::default();
    input.pointer = p;
    input
}

/// 1 フレーム走らせ、`Scene` と `ArrangementResponse` の**両方**を返す。
///
/// `arr_widget.rs:103` の `drive_scene` は `let _ = arrangement(..)` で response を捨てて
/// いるので、`arrange_fit_layout.rs:54-58` 方式で捕捉する形に直してある。
fn drive_scene(
    host: &mut UiHost<AppData>,
    app: &mut AppData,
    p: PointerFrame,
) -> (Scene, ArrangementResponse) {
    let mut scene = Scene::new();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let screen = PhysicalSize { width: WIDGET_RECT.w as u32, height: WIDGET_RECT.h as u32 };
    let mut captured = None;
    host.frame(app, &mut scene, screen, frame(p), |app, ui| {
        captured = Some(arrangement(app, ui, WIDGET_RECT));
    });
    (scene, captured.expect("arrangement() は毎フレーム response を返す"))
}

// ============================================================
// ダンプ (§9-0 (b): `..` を使わない完全分解束縛)
// ============================================================

/// `TransportState` の全 21 フィールドを 1 行 1 フィールドで出す。
/// **`..` を書かないこと** — 書いた瞬間に「列挙し忘れ」が復活する。
fn dump_transport(out: &mut String, t: &TransportState) {
    let TransportState {
        metronome_enabled,
        is_playing,
        preroll_remaining,
        loop_region,
        playhead_beat,
        playback_origin_beat,
        panic_reinit_due: _,   // Instant: 実行ごとに変わる
        panic_release_pending,
        master_meter,
        track_peak_display,
        mod_scalars,
        pending_play,
        pending_play_record,
        export_stage,
        export_progress_at: _, // Instant: 同上
        export_cancel: _,      // Arc<AtomicBool>: Debug がポインタ値を含む
        pending_video_export,
        export_temp_wav,
        pending_video_export_range,
        pending_video_export_dims,
        pending_export,
    } = t;
    let _ = writeln!(out, "  transport.metronome_enabled = {metronome_enabled:?}");
    let _ = writeln!(out, "  transport.is_playing = {is_playing:?}");
    let _ = writeln!(out, "  transport.preroll_remaining = {preroll_remaining:?}");
    let _ = writeln!(out, "  transport.loop_region = {loop_region:?}");
    let _ = writeln!(out, "  transport.playhead_beat = {playhead_beat:?}");
    let _ = writeln!(out, "  transport.playback_origin_beat = {playback_origin_beat:?}");
    let _ = writeln!(out, "  transport.panic_release_pending = {panic_release_pending:?}");
    let _ = writeln!(out, "  transport.master_meter = {master_meter:?}");
    let _ = writeln!(out, "  transport.track_peak_display = {track_peak_display:?}");
    let _ = writeln!(out, "  transport.mod_scalars = {mod_scalars:?}");
    let _ = writeln!(out, "  transport.pending_play = {pending_play:?}");
    let _ = writeln!(out, "  transport.pending_play_record = {pending_play_record:?}");
    let _ = writeln!(out, "  transport.export_stage = {export_stage:?}");
    let _ = writeln!(out, "  transport.pending_video_export = {pending_video_export:?}");
    let _ = writeln!(out, "  transport.export_temp_wav = {export_temp_wav:?}");
    let _ = writeln!(out, "  transport.pending_video_export_range = {pending_video_export_range:?}");
    let _ = writeln!(out, "  transport.pending_video_export_dims = {pending_video_export_dims:?}");
    let _ = writeln!(out, "  transport.pending_export = {pending_export:?}");
}

/// `UiEphemeral` の全フィールドを 1 行 1 フィールドで出す。
/// **`..` を書かないこと** (同上)。ダンプしないものは理由つきで `_` に落とす。
#[allow(clippy::too_many_lines)]
fn dump_ui_ephemeral(out: &mut String, e: &UiEphemeral) {
    let UiEphemeral {
        arr_label_cache: _,          // RefCell<ArrLabelCache>: 描画キャッシュ、観測対象でない
        tempo_map_cache: _,          // RefCell<TempoMapCache>: 同上
        loaded_project_id,
        project_generation,
        video_texture_cache: _,      // GPU TextureHandle: 実行ごとに変わる
        image_texture_cache: _,      // 同上
        pending_texture_destroys: _, // 同上
        arrangement_hover_beat,
        arrangement_hover_beat_raw,
        arrangement_hover_clip,
        arrange_hovered_track,
        mixer_hovered_track,
        master_gain_dragging,
        pianoroll_hover_beat,
        pianoroll_hover_beat_song_raw,
        pianoroll_hover_note,
        pending_clipboard_write,
        editing_automation_point,
        last_touched_param,          // Instant を内側に持つので下で分解して出す
        home_toggle_at_first,
        arrange_hover_content,
        arrange_hovered_automation_lane,
        arrange_dragging_track_volume,
        arrange_default_scrub_active,
        piano_roll_lyric_editing,
        pianoroll_viewport,
        audio_editor_clip,
        audio_editor_hover_beat_in_clip,
        arrange_zoom_history,
        arrange_zoom_anchor,
        inspector_body_h,
        inspector_device_panel_h,
        last_pianoroll_grid_size,
        pending_pianoroll_fit,
        last_arrange_lanes_size,
        last_arrange_rows,
        resource_panel_open,
        available_themes: _,         // Vec<Theme>: 起動時 1 度だけ読み込む静的リスト
        undo_history_follow_pos,
        plugin_picker_entries,
        plugin_picker_visible,
        plugin_picker_query,
        is_plugin_picker_open,
        plugin_picker_cursor,
        font_picker_families,
        font_picker_visible,
        font_picker_query,
        font_picker_cursor,
        is_font_picker_open,
        font_picker_loading,
        font_picker_target,
        font_picker_restore,
        send_picker,
        open_video_fx_params,
        open_plugin_params,
        anim_epoch: _,               // Instant: 実行ごとに変わる
        frame_now: _,                // Instant: 同上
        status_message,
        track_rename_id,
        track_rename_text,
        clip_rename,
        clip_rename_text,
        color_picker_target,
        color_picker_anchor,
        color_picker_session_dirty,
        clip_create_menu,
        clip_create_menu_open,
        section_menu,
        section_menu_open,
        section_rename_id,
        section_rename_text,
        bpm_edit_text,
        time_sig_num_edit_text,
        clip_edit_buffer_target,
        group_scrub_active,
        inspector_scrub_active,
        mod_follower_scrub_active,
        armed_mod_source,
        expanded_mod_sources,
        mod_depth_scrub_active,
        export_range_picker,
        clip_text_content_edit_text,
        clip_text_font_family_edit_text,
        recovery_candidates,
        show_recovery_modal,
        dirty_guard,
        guard_after_save,
        guard_pending_action,
        main_window_hwnd,
        export_dialog_open,
        save_as_dialog_open,
    } = e;
    let _ = writeln!(out, "  eph.loaded_project_id = {loaded_project_id:?}");
    let _ = writeln!(out, "  eph.project_generation = {project_generation:?}");
    let _ = writeln!(out, "  eph.arrangement_hover_beat = {arrangement_hover_beat:?}");
    let _ = writeln!(out, "  eph.arrangement_hover_beat_raw = {arrangement_hover_beat_raw:?}");
    let _ = writeln!(out, "  eph.arrangement_hover_clip = {arrangement_hover_clip:?}");
    let _ = writeln!(out, "  eph.arrange_hovered_track = {arrange_hovered_track:?}");
    let _ = writeln!(out, "  eph.mixer_hovered_track = {mixer_hovered_track:?}");
    let _ = writeln!(out, "  eph.master_gain_dragging = {master_gain_dragging:?}");
    let _ = writeln!(out, "  eph.pianoroll_hover_beat = {pianoroll_hover_beat:?}");
    let _ = writeln!(out, "  eph.pianoroll_hover_beat_song_raw = {pianoroll_hover_beat_song_raw:?}");
    let _ = writeln!(out, "  eph.pianoroll_hover_note = {pianoroll_hover_note:?}");
    let _ = writeln!(out, "  eph.pending_clipboard_write = {pending_clipboard_write:?}");
    let _ = writeln!(out, "  eph.editing_automation_point = {editing_automation_point:?}");
    // `TouchedParam` は Debug を導出しているが `touched_at: Instant` を含むので
    // `{:?}` を丸ごと使わず、値が決定的なフィールドだけ出す。
    match last_touched_param {
        Some(tp) => {
            let _ = writeln!(
                out,
                "  eph.last_touched_param = Some(track_id={:?}, target={:?}, display_name={:?})",
                tp.track_id, tp.target, tp.display_name
            );
        }
        None => {
            let _ = writeln!(out, "  eph.last_touched_param = None");
        }
    }
    let _ = writeln!(out, "  eph.home_toggle_at_first = {home_toggle_at_first:?}");
    let _ = writeln!(out, "  eph.arrange_hover_content = {arrange_hover_content:?}");
    let _ = writeln!(
        out,
        "  eph.arrange_hovered_automation_lane = {arrange_hovered_automation_lane:?}"
    );
    let _ = writeln!(out, "  eph.arrange_dragging_track_volume = {arrange_dragging_track_volume:?}");
    let _ = writeln!(out, "  eph.arrange_default_scrub_active = {arrange_default_scrub_active:?}");
    let _ = writeln!(out, "  eph.piano_roll_lyric_editing = {piano_roll_lyric_editing:?}");
    let _ = writeln!(out, "  eph.pianoroll_viewport = {pianoroll_viewport:?}");
    let _ = writeln!(out, "  eph.audio_editor_clip = {audio_editor_clip:?}");
    let _ = writeln!(
        out,
        "  eph.audio_editor_hover_beat_in_clip = {audio_editor_hover_beat_in_clip:?}"
    );
    // `ArrangeViewSnapshot` / `ArrangeZoomAnchor` は `Debug` を持たない (`app_types.rs`)。
    // production 型に derive を足すのは #77 のスコープ外なので、手で分解して出す。
    // **arrangement のズーム履歴そのもの**なので観測から落とさない。
    let _ = writeln!(out, "  eph.arrange_zoom_history.len = {}", arrange_zoom_history.len());
    for (i, s) in arrange_zoom_history.iter().enumerate() {
        let mut rows: Vec<(u32, u16)> = s.row_overrides.iter().map(|(k, v)| (*k, *v)).collect();
        rows.sort_unstable();
        let mut lanes: Vec<(u32, u32, u16)> =
            s.lane_row_overrides.iter().map(|(k, v)| (k.track, k.lane, *v)).collect();
        lanes.sort_unstable();
        let _ = writeln!(
            out,
            "  eph.arrange_zoom_history[{i}] = zoom_x={:?} scroll_beat={:?} row_h={:?} \
             track_top={:?} row_overrides={rows:?} lane_row_overrides={lanes:?}",
            s.zoom_x, s.scroll_beat, s.row_h, s.track_top
        );
    }
    match arrange_zoom_anchor {
        Some(a) => {
            let _ = writeln!(
                out,
                "  eph.arrange_zoom_anchor = Some(stage={:?} sig.clips={:?} sig.clip={:?} \
                 sig.automation={:?} sig.target_automation={:?} applied_view=(zoom_x={:?} \
                 scroll_beat={:?} row_h={:?} track_top={:?}))",
                a.stage,
                a.sig.clips,
                a.sig.clip,
                a.sig.automation,
                a.sig.target_automation,
                a.applied_view.zoom_x,
                a.applied_view.scroll_beat,
                a.applied_view.row_h,
                a.applied_view.track_top
            );
        }
        None => {
            let _ = writeln!(out, "  eph.arrange_zoom_anchor = None");
        }
    }
    let _ = writeln!(out, "  eph.inspector_body_h = {inspector_body_h:?}");
    let _ = writeln!(out, "  eph.inspector_device_panel_h = {inspector_device_panel_h:?}");
    let _ = writeln!(out, "  eph.last_pianoroll_grid_size = {last_pianoroll_grid_size:?}");
    let _ = writeln!(out, "  eph.pending_pianoroll_fit = {pending_pianoroll_fit:?}");
    let _ = writeln!(out, "  eph.last_arrange_lanes_size = {last_arrange_lanes_size:?}");
    let _ = writeln!(out, "  eph.last_arrange_rows = {last_arrange_rows:?}");
    let _ = writeln!(out, "  eph.resource_panel_open = {resource_panel_open:?}");
    let _ = writeln!(out, "  eph.undo_history_follow_pos = {undo_history_follow_pos:?}");
    let _ = writeln!(out, "  eph.plugin_picker_entries.len = {}", plugin_picker_entries.len());
    let _ = writeln!(out, "  eph.plugin_picker_visible.len = {}", plugin_picker_visible.len());
    let _ = writeln!(out, "  eph.plugin_picker_query = {plugin_picker_query:?}");
    let _ = writeln!(out, "  eph.is_plugin_picker_open = {is_plugin_picker_open:?}");
    let _ = writeln!(out, "  eph.plugin_picker_cursor = {plugin_picker_cursor:?}");
    let _ = writeln!(out, "  eph.font_picker_families.len = {}", font_picker_families.len());
    let _ = writeln!(out, "  eph.font_picker_visible.len = {}", font_picker_visible.len());
    let _ = writeln!(out, "  eph.font_picker_query = {font_picker_query:?}");
    let _ = writeln!(out, "  eph.font_picker_cursor = {font_picker_cursor:?}");
    let _ = writeln!(out, "  eph.is_font_picker_open = {is_font_picker_open:?}");
    let _ = writeln!(out, "  eph.font_picker_loading = {font_picker_loading:?}");
    let _ = writeln!(out, "  eph.font_picker_target = {font_picker_target:?}");
    let _ = writeln!(out, "  eph.font_picker_restore = {font_picker_restore:?}");
    let _ = writeln!(out, "  eph.send_picker = {send_picker:?}");
    let _ = writeln!(out, "  eph.open_video_fx_params = {open_video_fx_params:?}");
    let _ = writeln!(out, "  eph.open_plugin_params = {open_plugin_params:?}");
    let _ = writeln!(out, "  eph.status_message = {status_message:?}");
    let _ = writeln!(out, "  eph.track_rename_id = {track_rename_id:?}");
    let _ = writeln!(out, "  eph.track_rename_text = {track_rename_text:?}");
    let _ = writeln!(out, "  eph.clip_rename = {clip_rename:?}");
    let _ = writeln!(out, "  eph.clip_rename_text = {clip_rename_text:?}");
    let _ = writeln!(out, "  eph.color_picker_target = {color_picker_target:?}");
    let _ = writeln!(out, "  eph.color_picker_anchor = {color_picker_anchor:?}");
    let _ = writeln!(out, "  eph.color_picker_session_dirty = {color_picker_session_dirty:?}");
    let _ = writeln!(out, "  eph.clip_create_menu = {clip_create_menu:?}");
    let _ = writeln!(out, "  eph.clip_create_menu_open = {clip_create_menu_open:?}");
    let _ = writeln!(out, "  eph.section_menu = {section_menu:?}");
    let _ = writeln!(out, "  eph.section_menu_open = {section_menu_open:?}");
    let _ = writeln!(out, "  eph.section_rename_id = {section_rename_id:?}");
    let _ = writeln!(out, "  eph.section_rename_text = {section_rename_text:?}");
    let _ = writeln!(out, "  eph.bpm_edit_text = {bpm_edit_text:?}");
    let _ = writeln!(out, "  eph.time_sig_num_edit_text = {time_sig_num_edit_text:?}");
    let _ = writeln!(out, "  eph.clip_edit_buffer_target = {clip_edit_buffer_target:?}");
    let _ = writeln!(out, "  eph.group_scrub_active = {group_scrub_active:?}");
    let _ = writeln!(out, "  eph.inspector_scrub_active = {inspector_scrub_active:?}");
    let _ = writeln!(out, "  eph.mod_follower_scrub_active = {mod_follower_scrub_active:?}");
    let _ = writeln!(out, "  eph.armed_mod_source = {armed_mod_source:?}");
    let mut mod_sources: Vec<u32> = expanded_mod_sources.iter().copied().collect();
    mod_sources.sort_unstable();
    let _ = writeln!(out, "  eph.expanded_mod_sources = {mod_sources:?}");
    let _ = writeln!(out, "  eph.mod_depth_scrub_active = {mod_depth_scrub_active:?}");
    let _ = writeln!(out, "  eph.export_range_picker = {export_range_picker:?}");
    let _ = writeln!(out, "  eph.clip_text_content_edit_text = {clip_text_content_edit_text:?}");
    let _ = writeln!(
        out,
        "  eph.clip_text_font_family_edit_text = {clip_text_font_family_edit_text:?}"
    );
    let _ = writeln!(out, "  eph.recovery_candidates = {recovery_candidates:?}");
    let _ = writeln!(out, "  eph.show_recovery_modal = {show_recovery_modal:?}");
    let _ = writeln!(out, "  eph.dirty_guard = {dirty_guard:?}");
    let _ = writeln!(out, "  eph.guard_after_save = {guard_after_save:?}");
    let _ = writeln!(out, "  eph.guard_pending_action = {guard_pending_action:?}");
    let _ = writeln!(out, "  eph.main_window_hwnd = {main_window_hwnd:?}");
    let _ = writeln!(out, "  eph.export_dialog_open = {export_dialog_open:?}");
    let _ = writeln!(out, "  eph.save_as_dialog_open = {save_as_dialog_open:?}");
}

/// `Song` を `{:?}` で丸ごと出すと `clip_contents` / `clip_content_names` /
/// `media.*_sources` の **`HashMap` のイテレーション順が実行ごとに変わる** (決定性
/// セルフチェックがこれを捕まえた)。map はキー昇順に並べ直してから出す。
fn dump_song(out: &mut String, song: &common::model::Song) {
    let _ = writeln!(out, "  song.bpm = {:?}", song.bpm);
    let _ = writeln!(out, "  song.time_sig = {:?}", song.time_sig);
    let _ = writeln!(out, "  song.length_beats = {:?}", song.length_beats);
    let _ = writeln!(out, "  song.tracks = {:?}", song.tracks);
    let _ = writeln!(out, "  song.ids = {:?}", song.ids);
    let mut contents: Vec<_> = song.clip_contents.iter().collect();
    contents.sort_by_key(|(k, _)| **k);
    let _ = writeln!(out, "  song.clip_contents = {contents:?}");
    let mut names: Vec<_> = song.clip_content_names.iter().collect();
    names.sort_by_key(|(k, _)| **k);
    let _ = writeln!(out, "  song.clip_content_names = {names:?}");
    let mut audio: Vec<_> = song.media.audio_sources.keys().copied().collect();
    audio.sort_unstable();
    let mut video: Vec<_> = song.media.video_sources.keys().copied().collect();
    video.sort_unstable();
    let mut image: Vec<_> = song.media.image_sources.keys().copied().collect();
    image.sort_unstable();
    let _ = writeln!(out, "  song.media.audio_sources.keys = {audio:?}");
    let _ = writeln!(out, "  song.media.video_sources.keys = {video:?}");
    let _ = writeln!(out, "  song.media.image_sources.keys = {image:?}");
    let _ = writeln!(out, "  song.song_lanes = {:?}", song.song_lanes);
    let _ = writeln!(out, "  song.sections = {:?}", song.sections);
    let _ = writeln!(out, "  song.midi_bindings = {:?}", song.midi_bindings);
    let _ = writeln!(out, "  song.scale_changes = {:?}", song.scale_changes);
    let _ = writeln!(out, "  song.video_resolution = {:?}", song.video_resolution);
    let _ = writeln!(out, "  song.video_framerate = {:?}", song.video_framerate);
    let _ = writeln!(out, "  song.master_fx_chain.len = {}", song.master_fx_chain.len());
}

/// `UiPrefs` の全フィールドを 1 行 1 フィールドで出す。**`..` を書かないこと**。
///
/// whole-struct の `{:?}` は使えない — `track_row_overrides` /
/// `automation_lane_row_overrides` (どちらも widget が書く) をはじめ `HashMap` /
/// `HashSet` が 8 本あり、イテレーション順が実行ごとに変わる。
#[allow(clippy::too_many_lines)]
fn dump_ui_prefs(out: &mut String, p: &crate::state::ui_prefs::UiPrefs) {
    let crate::state::ui_prefs::UiPrefs {
        preview_window_visible,
        collapsed_groups,
        expanded_automation_tracks,
        master_row_automation_expanded,
        track_row_overrides,
        bottom_panel,
        audio_editor_vertical_gain,
        audio_editor_views,
        arrange_zoom_x,
        arrange_scroll_beat,
        arrange_follow,
        arrange_track_top,
        arrange_track_row_h,
        automation_lane_row_overrides,
        arrange_header_w,
        piano_roll_views,
        multi_clip_view,
        multi_clip_view_key,
        locked_pr_tracks,
        last_note_duration_beats,
        pianoroll_snap_enabled,
        pianoroll_snap_choice,
        arrange_snap_enabled,
        arrange_snap_choice,
        resource_monitor_enabled,
        undo_history_open,
        undo_history_rect,
        loudness_report_open,
        loudness_report_rect,
        settings_open,
        settings_rect,
        master_panel_open,
        master_panel_w,
        master_panel_sections,
        meter_settings,
        is_help_open,
        is_about_open,
        app_dirs,
        recent_files,
        recent_saved,
        recent_files_labels,
        recent_saved_labels,
        snap_on_draw,
        plugin_editor_windows,
        piano_roll_fold,
    } = p;
    let sorted_set = |s: &std::collections::HashSet<u32>| {
        let mut v: Vec<u32> = s.iter().copied().collect();
        v.sort_unstable();
        v
    };
    let _ = writeln!(out, "  prefs.preview_window_visible = {preview_window_visible:?}");
    let _ = writeln!(out, "  prefs.collapsed_groups = {:?}", sorted_set(collapsed_groups));
    let _ = writeln!(
        out,
        "  prefs.expanded_automation_tracks = {:?}",
        sorted_set(expanded_automation_tracks)
    );
    let _ =
        writeln!(out, "  prefs.master_row_automation_expanded = {master_row_automation_expanded:?}");
    let mut rows: Vec<(u32, u16)> = track_row_overrides.iter().map(|(k, v)| (*k, *v)).collect();
    rows.sort_unstable();
    let _ = writeln!(out, "  prefs.track_row_overrides = {rows:?}");
    let _ = writeln!(out, "  prefs.bottom_panel = {bottom_panel:?}");
    let _ = writeln!(out, "  prefs.audio_editor_vertical_gain = {audio_editor_vertical_gain:?}");
    let mut aev: Vec<_> = audio_editor_views.iter().collect();
    aev.sort_by_key(|(k, _)| format!("{k:?}"));
    let _ = writeln!(out, "  prefs.audio_editor_views = {aev:?}");
    let _ = writeln!(out, "  prefs.arrange_zoom_x = {arrange_zoom_x:?}");
    let _ = writeln!(out, "  prefs.arrange_scroll_beat = {arrange_scroll_beat:?}");
    let _ = writeln!(out, "  prefs.arrange_follow = {arrange_follow:?}");
    let _ = writeln!(out, "  prefs.arrange_track_top = {arrange_track_top:?}");
    let _ = writeln!(out, "  prefs.arrange_track_row_h = {arrange_track_row_h:?}");
    let mut lanes: Vec<(u32, u32, u16)> =
        automation_lane_row_overrides.iter().map(|(k, v)| (k.track, k.lane, *v)).collect();
    lanes.sort_unstable();
    let _ = writeln!(out, "  prefs.automation_lane_row_overrides = {lanes:?}");
    let _ = writeln!(out, "  prefs.arrange_header_w = {arrange_header_w:?}");
    let mut prv: Vec<_> = piano_roll_views.iter().collect();
    prv.sort_by_key(|(k, _)| format!("{k:?}"));
    let _ = writeln!(out, "  prefs.piano_roll_views = {prv:?}");
    let _ = writeln!(out, "  prefs.multi_clip_view = {multi_clip_view:?}");
    let _ = writeln!(out, "  prefs.multi_clip_view_key = {multi_clip_view_key:?}");
    let _ = writeln!(out, "  prefs.locked_pr_tracks = {:?}", sorted_set(locked_pr_tracks));
    let _ = writeln!(out, "  prefs.last_note_duration_beats = {last_note_duration_beats:?}");
    let _ = writeln!(out, "  prefs.pianoroll_snap_enabled = {pianoroll_snap_enabled:?}");
    let _ = writeln!(out, "  prefs.pianoroll_snap_choice = {pianoroll_snap_choice:?}");
    let _ = writeln!(out, "  prefs.arrange_snap_enabled = {arrange_snap_enabled:?}");
    let _ = writeln!(out, "  prefs.arrange_snap_choice = {arrange_snap_choice:?}");
    let _ = writeln!(out, "  prefs.resource_monitor_enabled = {resource_monitor_enabled:?}");
    let _ = writeln!(out, "  prefs.undo_history_open = {undo_history_open:?}");
    let _ = writeln!(out, "  prefs.undo_history_rect = {undo_history_rect:?}");
    let _ = writeln!(out, "  prefs.loudness_report_open = {loudness_report_open:?}");
    let _ = writeln!(out, "  prefs.loudness_report_rect = {loudness_report_rect:?}");
    let _ = writeln!(out, "  prefs.settings_open = {settings_open:?}");
    let _ = writeln!(out, "  prefs.settings_rect = {settings_rect:?}");
    let _ = writeln!(out, "  prefs.master_panel_open = {master_panel_open:?}");
    let _ = writeln!(out, "  prefs.master_panel_w = {master_panel_w:?}");
    let _ = writeln!(out, "  prefs.master_panel_sections = {master_panel_sections:?}");
    let _ = writeln!(out, "  prefs.meter_settings = {meter_settings:?}");
    let _ = writeln!(out, "  prefs.is_help_open = {is_help_open:?}");
    let _ = writeln!(out, "  prefs.is_about_open = {is_about_open:?}");
    let _ = writeln!(out, "  prefs.app_dirs = {app_dirs:?}");
    let _ = writeln!(out, "  prefs.recent_files = {recent_files:?}");
    let _ = writeln!(out, "  prefs.recent_saved = {recent_saved:?}");
    let _ = writeln!(out, "  prefs.recent_files_labels = {recent_files_labels:?}");
    let _ = writeln!(out, "  prefs.recent_saved_labels = {recent_saved_labels:?}");
    let _ = writeln!(out, "  prefs.snap_on_draw = {snap_on_draw:?}");
    let mut pew: Vec<_> = plugin_editor_windows.iter().collect();
    pew.sort_by_key(|(k, _)| format!("{k:?}"));
    let _ = writeln!(out, "  prefs.plugin_editor_windows = {pew:?}");
    let _ = writeln!(out, "  prefs.piano_roll_fold = {piano_roll_fold:?}");
}

fn dump_frame(
    out: &mut String,
    label: &str,
    idx: usize,
    scene: &Scene,
    response: &ArrangementResponse,
    app: &AppData,
    cursors: &[CursorIcon],
) {
    let _ = writeln!(out, "=== {label} / frame {idx} ===");
    let _ = writeln!(out, "-- primitives ({}) --", scene.primitives.len());
    for (i, p) in scene.primitives.iter().enumerate() {
        let _ = writeln!(out, "  [{i}] {p:?}");
    }
    let _ = writeln!(out, "-- song --");
    dump_song(out, app.song_doc.song());
    let _ = writeln!(out, "-- ui_prefs --");
    dump_ui_prefs(out, &app.ui_prefs);
    let _ = writeln!(out, "-- selection --");
    let _ = writeln!(out, "  {:?}", app.selection);
    let _ = writeln!(out, "-- transport --");
    dump_transport(out, &app.transport);
    let _ = writeln!(out, "-- ui_ephemeral --");
    dump_ui_ephemeral(out, &app.ui_ephemeral);
    let _ = writeln!(out, "-- response --");
    let _ = writeln!(out, "  {response:?}");
    let _ = writeln!(out, "-- cursors --");
    let _ = writeln!(out, "  {cursors:?}");
}

// ============================================================
// シナリオ
// ============================================================

/// 座標の読み替え (`view_build.rs` / `geometry.rs` から導出、snap は無効):
/// `header_w = 0`   → `lanes.x = 0`,   `lanes.w = 800`, `view.len_beats = 12.5`, beat→x = `beat*64`
/// `header_w = 160` → `lanes.x = 160`, `lanes.w = 640`, `view.len_beats = 10.0`, beat→x = `160 + beat*64`
struct Geo {
    header_w: f32,
}

impl Geo {
    /// beat → x (lanes 座標系)。
    fn bx(&self, beat: f32) -> f32 {
        self.header_w + beat * ZOOM
    }
    /// master row (visible index 0) の縦中央 y。
    fn master_y(&self) -> f32 {
        let _ = self;
        38.0 + ROW_H * 0.5
    }
    /// group track (visible index 1) の縦中央 y。
    fn group_y(&self) -> f32 {
        let _ = self;
        38.0 + ROW_H + ROW_H * 0.5
    }
    /// 子 track (visible index 2) の縦中央 y。
    fn child_y(&self) -> f32 {
        let _ = self;
        38.0 + ROW_H * 2.0 + ROW_H * 0.5
    }
    /// Lead track (visible index 3) の縦中央 y。
    fn lead_y(&self) -> f32 {
        let _ = self;
        38.0 + ROW_H * 3.0 + ROW_H * 0.5
    }
    /// Lead track 行の下端 (= automation lane の上端)。
    fn lead_bottom(&self) -> f32 {
        let _ = self;
        38.0 + ROW_H * 4.0
    }
    /// Lead の automation lane body の縦中央 y (lane 高 60px)。
    fn lane_y(&self) -> f32 {
        self.lead_bottom() + 30.0
    }
    /// Lead の automation lane の下端 (= lane 下端 splitter の位置)。
    fn lane_bottom(&self) -> f32 {
        self.lead_bottom() + 60.0
    }
}

/// `(名前, 幅, フレーム列)`。幅は `0.0` / `160.0` / 両方。
fn scenarios(header_w: f32) -> Vec<(&'static str, Vec<PointerFrame>)> {
    let g = Geo { header_w };
    let has_header = header_w > 0.0;
    let mut out: Vec<(&'static str, Vec<PointerFrame>)> = Vec::new();

    // --- 静止 ---
    out.push(("idle", vec![PointerFrame::default()]));

    // --- lane 下端 splitter (automation lane の下端 ±handle) ---
    out.push((
        "lane_splitter",
        vec![
            press(g.bx(2.0), g.lane_bottom() - 1.0, no_mods()),
            hold(g.bx(2.0), g.lane_bottom() + 20.0, no_mods()),
            release(g.bx(2.0), g.lane_bottom() + 20.0, no_mods()),
        ],
    ));

    // --- track 行下端 splitter ---
    out.push((
        "row_splitter",
        vec![
            press(g.bx(2.0), 38.0 + ROW_H - 1.0, no_mods()),
            hold(g.bx(2.0), 38.0 + ROW_H + 15.0, no_mods()),
            release(g.bx(2.0), 38.0 + ROW_H + 15.0, no_mods()),
        ],
    ));

    // --- header/lanes 境界 splitter (header_w 依存) ---
    if has_header {
        out.push((
            "header_splitter",
            vec![
                press(header_w, g.lead_y(), no_mods()),
                hold(header_w + 40.0, g.lead_y(), no_mods()),
                release(header_w + 40.0, g.lead_y(), no_mods()),
            ],
        ));
    }

    // --- clip の Move / ResizeLeft / ResizeRight / 短クリック / Shift / Ctrl / Alt ---
    out.push((
        "clip_move",
        vec![
            press(g.bx(2.0), g.lead_y(), no_mods()),
            hold(g.bx(3.0), g.lead_y(), no_mods()),
            release(g.bx(3.0), g.lead_y(), no_mods()),
        ],
    ));
    out.push((
        "clip_resize_left",
        vec![
            press(g.bx(0.0) + 2.0, g.lead_y(), no_mods()),
            hold(g.bx(1.0), g.lead_y(), no_mods()),
            release(g.bx(1.0), g.lead_y(), no_mods()),
        ],
    ));
    out.push((
        "clip_resize_right",
        vec![
            press(g.bx(4.0) - 2.0, g.lead_y(), no_mods()),
            hold(g.bx(5.0), g.lead_y(), no_mods()),
            release(g.bx(5.0), g.lead_y(), no_mods()),
        ],
    ));
    out.push((
        "clip_short_click",
        vec![
            press(g.bx(2.0), g.lead_y(), no_mods()),
            release(g.bx(2.0), g.lead_y(), no_mods()),
        ],
    ));
    out.push((
        "clip_shift_click",
        vec![
            press(g.bx(2.0), g.lead_y(), modifiers(false, true, false)),
            release(g.bx(2.0), g.lead_y(), modifiers(false, true, false)),
        ],
    ));
    out.push((
        "clip_ctrl_click",
        vec![
            press(g.bx(2.0), g.lead_y(), modifiers(true, false, false)),
            release(g.bx(2.0), g.lead_y(), modifiers(true, false, false)),
        ],
    ));
    out.push((
        "clip_alt_drag",
        vec![
            press(g.bx(2.0), g.lead_y(), modifiers(false, false, true)),
            hold(g.bx(2.5), g.lead_y(), modifiers(false, false, true)),
            release(g.bx(2.5), g.lead_y(), modifiers(false, false, true)),
        ],
    ));

    // --- Arranger 帯: Move / Resize / 空き帯 Create / 短クリック ---
    let sec_y = 29.0;
    out.push((
        "section_move",
        vec![
            press(g.bx(2.0), sec_y, no_mods()),
            hold(g.bx(3.0), sec_y, no_mods()),
            release(g.bx(3.0), sec_y, no_mods()),
        ],
    ));
    out.push((
        "section_resize",
        vec![
            press(g.bx(4.0) - 2.0, sec_y, no_mods()),
            hold(g.bx(5.0), sec_y, no_mods()),
            release(g.bx(5.0), sec_y, no_mods()),
        ],
    ));
    out.push((
        "section_create",
        vec![
            press(g.bx(5.0), sec_y, no_mods()),
            hold(g.bx(7.0), sec_y, no_mods()),
            release(g.bx(7.0), sec_y, no_mods()),
        ],
    ));
    out.push((
        "section_short_click",
        vec![press(g.bx(2.0), sec_y, no_mods()), release(g.bx(2.0), sec_y, no_mods())],
    ));

    // --- ruler: plain seek / Shift+drag loop (NewRange → Start / End / Middle) ---
    let ruler_y = 10.0;
    out.push((
        "ruler_seek",
        vec![
            press(g.bx(3.0), ruler_y, no_mods()),
            hold(g.bx(4.0), ruler_y, no_mods()),
            release(g.bx(4.0), ruler_y, no_mods()),
        ],
    ));
    out.push((
        "ruler_loop_new_range",
        vec![
            press(g.bx(1.0), ruler_y, modifiers(false, true, false)),
            hold(g.bx(5.0), ruler_y, modifiers(false, true, false)),
            release(g.bx(5.0), ruler_y, modifiers(false, true, false)),
            // 続けて既存 loop の Start / Middle / End を掴む。
            press(g.bx(1.0), ruler_y, modifiers(false, true, false)),
            hold(g.bx(0.5), ruler_y, modifiers(false, true, false)),
            release(g.bx(0.5), ruler_y, modifiers(false, true, false)),
            press(g.bx(5.0), ruler_y, modifiers(false, true, false)),
            hold(g.bx(6.0), ruler_y, modifiers(false, true, false)),
            release(g.bx(6.0), ruler_y, modifiers(false, true, false)),
            press(g.bx(3.0), ruler_y, modifiers(false, true, false)),
            hold(g.bx(4.0), ruler_y, modifiers(false, true, false)),
            release(g.bx(4.0), ruler_y, modifiers(false, true, false)),
        ],
    ));

    // --- header pane (160 のみ) ---
    if has_header {
        // volume band は row 下端寄り (header_row_layout の band)。
        out.push((
            "header_volume_band",
            vec![
                press(60.0, g.lead_y() + ROW_H * 0.35, no_mods()),
                hold(100.0, g.lead_y() + ROW_H * 0.35, no_mods()),
                release(100.0, g.lead_y() + ROW_H * 0.35, no_mods()),
            ],
        ));
        out.push((
            "header_reorder",
            vec![
                press(80.0, g.lead_y() - ROW_H * 0.3, no_mods()),
                hold(80.0, g.child_y(), no_mods()),
                release(80.0, g.child_y(), no_mods()),
            ],
        ));
        out.push((
            "header_group_disclosure",
            vec![
                press(6.0, g.group_y() - ROW_H * 0.3, no_mods()),
                release(6.0, g.group_y() - ROW_H * 0.3, no_mods()),
            ],
        ));
        out.push((
            "header_lane_disclosure",
            vec![
                press(150.0, g.lead_y() - ROW_H * 0.3, no_mods()),
                release(150.0, g.lead_y() - ROW_H * 0.3, no_mods()),
            ],
        ));
        // M·S·R ボタン列 (`header_row_layout` の buttons)。
        for (name, x) in [("header_mute", 92.0_f32), ("header_solo", 112.0), ("header_rec", 132.0)] {
            out.push((
                name,
                vec![
                    press(x, g.lead_y() - ROW_H * 0.3, no_mods()),
                    release(x, g.lead_y() - ROW_H * 0.3, no_mods()),
                ],
            ));
        }
        // 行の catch-all 選択 (plain / Shift / Ctrl)。
        out.push((
            "header_row_select",
            vec![press(40.0, g.child_y(), no_mods()), release(40.0, g.child_y(), no_mods())],
        ));
        out.push((
            "header_row_select_shift",
            vec![
                press(40.0, g.child_y(), modifiers(false, true, false)),
                release(40.0, g.child_y(), modifiers(false, true, false)),
            ],
        ));
        out.push((
            "header_row_select_ctrl",
            vec![
                press(40.0, g.child_y(), modifiers(true, false, false)),
                release(40.0, g.child_y(), modifiers(true, false, false)),
            ],
        ));
        out.push((
            "header_master_row",
            vec![press(40.0, g.master_y(), no_mods()), release(40.0, g.master_y(), no_mods())],
        ));
        // 名前欄ダブルクリック (同座標 2 連続 click)。
        out.push((
            "header_name_double_click",
            vec![
                press(40.0, g.child_y(), no_mods()),
                release(40.0, g.child_y(), no_mods()),
                press(40.0, g.child_y(), no_mods()),
                release(40.0, g.child_y(), no_mods()),
            ],
        ));
        // lane header の ★ / 👁 / ✕ (`automation_lane_header_layout` の icon 列)。
        // 座標は probe で実 layout から引く (当て推量では届かない)。
        let (_points, icons) = probe_geometry(header_w);
        for (name, c) in
            [("lane_enabled_icon", 0_usize), ("lane_visible_icon", 1), ("lane_delete_icon", 2)]
        {
            if let Some(&(x, y)) = icons.get(c) {
                out.push((name, vec![press(x, y, no_mods()), release(x, y, no_mods())]));
            }
        }
    }

    // --- automation: point drag / Alt+click 削除 / curve handle / clip Move / lasso ---
    // point の座標は probe で `response.automation_point_rects` から引く
    // (lane 内の y は value_norm 依存なので、lane 中央を押しても点には当たらない)。
    let (points, _icons) = probe_geometry(header_w);
    if let Some(&(pxp, pyp)) = points.first() {
        out.push((
            "automation_point_drag",
            vec![
                press(pxp, pyp, no_mods()),
                hold(pxp + ZOOM, pyp - 10.0, no_mods()),
                release(pxp + ZOOM, pyp - 10.0, no_mods()),
            ],
        ));
    }
    if let Some(&(pxp, pyp)) = points.get(1) {
        out.push((
            "automation_point_alt_delete",
            vec![
                press(pxp, pyp, modifiers(false, false, true)),
                release(pxp, pyp, modifiers(false, false, true)),
            ],
        ));
    }
    out.push((
        "automation_clip_move",
        vec![
            press(g.bx(4.0), g.lane_y(), no_mods()),
            hold(g.bx(5.0), g.lane_y(), no_mods()),
            release(g.bx(5.0), g.lane_y(), no_mods()),
        ],
    ));
    // lasso は **空き lane zone** でしか起動しないので、clip の外 (beat 8.5) から
    // 掴んで点のある方 (beat 1.0) まで引き、lane 全高を覆う。
    out.push((
        "automation_lasso",
        vec![
            press(g.bx(8.5), g.lead_bottom() + 5.0, no_mods()),
            hold(g.bx(1.0), g.lane_bottom() - 5.0, no_mods()),
            release(g.bx(1.0), g.lane_bottom() - 5.0, no_mods()),
        ],
    ));

    // --- Alt+drag フォールバック (lane resize / row resize)。header 側枝は 160 のみ ---
    out.push((
        "alt_drag_lane_resize",
        vec![
            press(g.bx(8.5), g.lane_y(), modifiers(false, false, true)),
            hold(g.bx(8.5), g.lane_y() + 25.0, modifiers(false, false, true)),
            release(g.bx(8.5), g.lane_y() + 25.0, modifiers(false, false, true)),
        ],
    ));
    out.push((
        "alt_drag_row_resize",
        vec![
            press(g.bx(6.0), g.child_y(), modifiers(false, false, true)),
            hold(g.bx(6.0), g.child_y() + 20.0, modifiers(false, false, true)),
            release(g.bx(6.0), g.child_y() + 20.0, modifiers(false, false, true)),
        ],
    ));
    if has_header {
        out.push((
            "alt_drag_in_header_pane",
            vec![
                press(80.0, g.lane_y(), modifiers(false, false, true)),
                hold(80.0, g.lane_y() + 25.0, modifiers(false, false, true)),
                release(80.0, g.lane_y() + 25.0, modifiers(false, false, true)),
            ],
        ));
    }

    // --- 端オートスクロール (lanes 右端 / 下端でホールド) ---
    out.push((
        "edge_autoscroll",
        vec![
            press(g.bx(2.0), g.lead_y(), no_mods()),
            hold(795.0, g.lead_y(), no_mods()),
            hold(795.0, 595.0, no_mods()),
            hold(795.0, 595.0, no_mods()),
            release(795.0, 595.0, no_mods()),
        ],
    ));

    // --- hover のみ (press なし) の cursor 決定 ---
    out.push((
        "hover_only",
        vec![
            PointerFrame { pos: Some((g.bx(2.0), g.lead_y())), ..PointerFrame::default() },
            PointerFrame { pos: Some((g.bx(4.0) - 2.0, g.lead_y())), ..PointerFrame::default() },
            PointerFrame {
                pos: Some((g.bx(2.0), g.lane_bottom() - 1.0)),
                ..PointerFrame::default()
            },
            PointerFrame { pos: Some((g.bx(2.0), 29.0)), ..PointerFrame::default() },
            PointerFrame { pos: Some((g.bx(4.0), g.lane_y())), ..PointerFrame::default() },
        ],
    ));

    out
}

/// 座標を当て推量で書くと分岐に届かない (実際 lane icon / point の 3 シナリオが
/// 素通りしていた) ので、`arrangement()` が返す rect と本番の layout 関数から引く。
///
/// `automation_lane_rects` は lane **body** (x∈lanes) の rect なので、header 側は
/// `press_header::lane_header` と同じ式 (`header_pane.x + indent`、Lead は depth 0) で組む。
type ProbePoints = Vec<(f32, f32)>;

fn probe_geometry(header_w: f32) -> (ProbePoints, ProbePoints) {
    let mut app = build_fixture(header_w);
    let mut host = UiHost::no_redraw();
    let (_scene, response) = drive_scene(&mut host, &mut app, PointerFrame::default());
    // automation point の中心 (Alt+click 削除 / lasso の対象)。
    let points: Vec<(f32, f32)> = response
        .automation_point_rects
        .iter()
        .map(|(_, r)| (r.x + r.w * 0.5, r.y + r.h * 0.5))
        .collect();
    // lane header の ★ / 👁 / ✕ icon の中心。
    let built = super::view_build::build(&app, WIDGET_RECT);
    let style = &built.style;
    let mut icons: Vec<(f32, f32)> = Vec::new();
    if header_w > 0.0 {
        for (_key, body) in &response.automation_lane_rects {
            let header_rect = Rect { x: 0.0, y: body.y, w: header_w, h: body.h };
            if let Some(l) = super::automation_lane_header_layout(header_rect, style) {
                for r in
                    [l.enabled_icon_rect, l.visible_icon_rect, l.delete_icon_rect]
                {
                    icons.push((r.x + r.w * 0.5, r.y + r.h * 0.5));
                }
            }
        }
    }
    (points, icons)
}

/// popup が開いているフレームの header press (`ui.has_open_popups()` ゲート)。
///
/// `UiHost` に popup を開かせるため、widget 呼び出しの**前**に `Ui::open_popup` 相当を
/// 呼ぶ必要がある。`context_menu_for` は caller (arrangement_view) が呼ぶものなので、
/// ここでは同 frame 内で popup を開いてから `arrangement()` を回す。
fn run_popup_scenario(out: &mut String, header_w: f32) {
    let g = Geo { header_w };
    let mut app = build_fixture(header_w);
    let mut host = UiHost::no_redraw();
    let cursors: Arc<Mutex<Vec<CursorIcon>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let sink = Arc::clone(&cursors);
        host.set_cursor_sink(Box::new(move |c| sink.lock().expect("cursor sink").push(c)));
    }
    let seq = [
        press(40.0, g.child_y(), no_mods()),
        release(40.0, g.child_y(), no_mods()),
    ];
    for (i, p) in seq.iter().enumerate() {
        cursors.lock().expect("cursor sink").clear();
        let mut scene = Scene::new();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let screen = PhysicalSize { width: WIDGET_RECT.w as u32, height: WIDGET_RECT.h as u32 };
        let mut captured = None;
        host.frame(&mut app, &mut scene, screen, frame(*p), |app, ui| {
            // popup を 1 つ開いた状態で widget を回す (= `ui.has_open_popups() == true`)。
            // `context_menu_for` は secondary click を要求するので、 programmatic な
            // `open_popup` で直接開く (見たいのは header press 側のゲートだけ)。
            ui.open_popup(
                ("equivalence_popup", 0_u32),
                Rect { x: 400.0, y: 300.0, w: 10.0, h: 10.0 },
                false,
            );
            captured = Some(arrangement(app, ui, WIDGET_RECT));
        });
        let response = captured.expect("arrangement() は毎フレーム response を返す");
        let taken = cursors.lock().expect("cursor sink").clone();
        dump_frame(out, "popup_header_press", i, &scene, &response, &app, &taken);
    }
}

fn run_pass(out: &mut String, header_w: f32) {
    let _ = writeln!(out, "########## header_w = {header_w} ##########");
    for (name, seq) in scenarios(header_w) {
        let mut app = build_fixture(header_w);
        let mut host = UiHost::no_redraw();
        let cursors: Arc<Mutex<Vec<CursorIcon>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let sink = Arc::clone(&cursors);
            host.set_cursor_sink(Box::new(move |c| sink.lock().expect("cursor sink").push(c)));
        }
        for (i, p) in seq.iter().enumerate() {
            cursors.lock().expect("cursor sink").clear();
            let (scene, response) = drive_scene(&mut host, &mut app, *p);
            let taken = cursors.lock().expect("cursor sink").clone();
            let label = format!("{header_w}/{name}");
            dump_frame(out, &label, i, &scene, &response, &app, &taken);
        }
    }
    if header_w > 0.0 {
        run_popup_scenario(out, header_w);
    }
}

#[test]
fn arrangement_transcript() {
    let Ok(path) = std::env::var("DAW01_ARR_TRANSCRIPT") else {
        // 環境変数が無いときは何もしない (`make test-nolaunch` 等で常時走っても無害)。
        return;
    };
    let mut out = String::new();
    run_pass(&mut out, 0.0);
    run_pass(&mut out, 160.0);
    std::fs::write(&path, out).expect("トランスクリプトの書き出しに失敗");
}
