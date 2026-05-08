//! 基本 API (label / button / fader / Edit / frame) が **`Clone` も `PartialEq` も
//! `Hash` も `Default` も持たない Model 型** でコンパイルすることを確認する。
//!
//! ここでビルド失敗するなら、API シグネチャに余計な制約 (例: `M: Clone`) が
//! 紛れ込んでいるか、`Application::Message: Clone` のような型境界が露出している。

use daw_ui_core::{
    ArrangementClip, ArrangementEditRequest, ArrangementStyle, ArrangementTrack, ArrangementView,
    ClipKey, Edit, Note, NoteId, NotesEditRequest, PianoRollStyle, PianoRollView, PointerFrame,
    ReorderableListEditRequest, ReorderableListStyle, UiHost,
};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Rect, Scene};

// 意図的に derive マクロを一切付けない。
// String / Vec などの非 Copy フィールドを混ぜて、Model 全体に Copy/Clone が
// 自動派生しないことも担保する。
struct Model {
    counter: u32,
    label: String,
    history: Vec<u32>,
    volume: f32,
    mute: bool,
    title: String,
    // M9 Phase 41e: piano_roll widget 用 (non-Clone Model でも piano_roll が呼べることを担保)
    notes: Vec<Note>,
    selected_note_ids: Vec<NoteId>,
    // M9 Phase 45e: arrangement widget 用 (non-Clone Model でも arrangement が呼べることを担保)
    arr_tracks: Vec<ArrangementTrack>,
    arr_selected_clips: Vec<ClipKey>,
    arr_selected_tracks: Vec<u32>,
    // M11 Phase 51: reorderable_list widget 用 (non-Clone Model でも呼べることを担保)
    chain: Vec<String>,
}

fn main() {
    let mut host: UiHost<Model> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let mut model = Model {
        counter: 0,
        label: String::from("hi"),
        history: Vec::new(),
        volume: 0.5,
        mute: false,
        title: String::from("untitled"),
        notes: Vec::new(),
        selected_note_ids: Vec::new(),
        arr_tracks: Vec::new(),
        arr_selected_clips: Vec::new(),
        arr_selected_tracks: Vec::new(),
        chain: Vec::new(),
    };

    let edits = host.frame_to_edits(
        &model,
        &mut scene,
        PhysicalSize { width: 800, height: 600 },
        daw_ui_core::FrameInput::default(),
        |m, ui| {
            ui.label("title", &m.label);
            ui.button("inc", "increment", || {
                Edit::mutate(|m: &mut Model| {
                    m.counter += 1;
                    m.history.push(m.counter);
                })
            });
            // fader (M3): 矩形指定 + vstack 版の両方が non-Clone Model でコンパイルする。
            // default_value (4 番目の引数) はダブルクリックリセット用 (M3 Phase 4d)。
            // M8 Phase 29: label (5 番目) は undoable Edit に付与される表示文字列。
            // closure は `Fn + Clone + Send + Sync + 'static` (drag 終端で再呼び出しされる)。
            let _ = ui.fader_at(
                "vol",
                Rect { x: 0.0, y: 0.0, w: 32.0, h: 120.0 },
                m.volume,
                0.0,
                "fader",
                |v| Edit::mutate(move |m: &mut Model| m.volume = v),
            );
            let _ = ui.fader("vol2", m.volume, 0.0, "fader", |v| {
                Edit::mutate(move |m: &mut Model| m.volume = v)
            });
            // knob (M3): 同様に non-Clone Model でコンパイルする。
            let _ = ui.knob_at(
                "pan",
                Rect { x: 0.0, y: 0.0, w: 64.0, h: 64.0 },
                m.volume,
                0.5,
                "knob",
                |v| Edit::mutate(move |m: &mut Model| m.volume = v),
            );
            let _ = ui.knob("pan2", m.volume, 0.5, "knob", |v| {
                Edit::mutate(move |m: &mut Model| m.volume = v)
            });
            // M8 Phase 29: Edit::with_inverse は `Fn + Clone + Send + Sync` を要求するが、
            // ユーザ Model 型に Clone を要求しない (closure 内でフィールドを set するだけ)。
            let _undoable: Edit<Model> = Edit::with_inverse(
                "set_volume",
                |m: &mut Model| m.volume = 0.8,
                |m: &mut Model| m.volume = 0.5,
            );
            // checkbox (M3): non-Clone Model でコンパイルする。
            let _ = ui.checkbox_at(
                "mute",
                Rect { x: 0.0, y: 0.0, w: 100.0, h: 24.0 },
                m.mute,
                "Mute",
                |new| Edit::mutate(move |m: &mut Model| m.mute = new),
            );
            let _ = ui.checkbox("mute2", m.mute, "Mute", |new| {
                Edit::mutate(move |m: &mut Model| m.mute = new)
            });
            // text_input (M3 Phase 4b): non-Clone Model でコンパイルする。
            let _ = ui.text_input_at(
                "title",
                Rect { x: 0.0, y: 0.0, w: 200.0, h: 28.0 },
                &m.title,
                |new| Edit::mutate(move |m: &mut Model| m.title = new),
            );
            let _ = ui.text_input("title2", &m.title, |new| {
                Edit::mutate(move |m: &mut Model| m.title = new)
            });
            // M11 Phase 52: text_input_at_focused (open 時自動 focus 版) も non-Clone Model で
            // コンパイルする。
            let _ = ui.text_input_at_focused(
                "rename",
                Rect { x: 0.0, y: 0.0, w: 200.0, h: 28.0 },
                &m.title,
                |new| Edit::mutate(move |m: &mut Model| m.title = new),
            );
            // M9 Phase 43: debug_overlay も non-Clone Model でコンパイルする。
            ui.debug_overlay(Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 }, 5.5);
            // M9 Phase 41e: piano_roll widget が non-Clone Model でコンパイルする。
            let view = PianoRollView {
                start_beat: 0.0,
                len_beats: 4.0,
                pitch_top: 72.0,
                pitch_visible: 24.0,
                keyboard_w: 60.0,
                notes_generation: 0,
                velocity_lane_h: 0.0,
                playhead_beat: None,
                // M13 Phase 55: ruler/bpm/time_sig (no-Clone Model でも構造体リテラルが
                // コンパイルできることを確認)
                ruler_h: 0.0,
                bpm: 120.0,
                time_sig: (4, 4),
                // M9 Phase 45f: SnapConfig も non-Clone Model でコンパイル可能。
                snap: daw_ui_core::SnapConfig::DEFAULT,
            };
            let style = PianoRollStyle::default();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &m.notes,
                view,
                &m.selected_note_ids,
                &style,
                |req| match req {
                    NotesEditRequest::Add(_) => Edit::mutate(|_m: &mut Model| {
                        // Note を作る場合は lyric: None も渡す (M9 Phase 44c で追加)
                        let _new = Note {
                            id: 0,
                            start_beat: 0.0_f64,
                            len_beats: 0.5_f64,
                            pitch: 60,
                            velocity: 96,
                            lyric: None,
                        };
                    }),
                    NotesEditRequest::Delete(_) => Edit::mutate(|_m: &mut Model| {}),
                    NotesEditRequest::Move(_) => Edit::mutate(|_m: &mut Model| {}),
                    NotesEditRequest::Resize(_) => Edit::mutate(|_m: &mut Model| {}),
                    NotesEditRequest::Select { .. } => Edit::mutate(|_m: &mut Model| {}),
                    NotesEditRequest::SetLyrics(_) => Edit::mutate(|_m: &mut Model| {}),
                    NotesEditRequest::SetVelocity(_) => Edit::mutate(|_m: &mut Model| {}),
                },
            );
            // M9 Phase 45e: arrangement widget が non-Clone Model でコンパイルする。
            // 全 17 variants を make_edit でハンドルすることで API exhaustive 担保。
            let arr_view = ArrangementView::default();
            let arr_style = ArrangementStyle::default();
            let _ = ui.arrangement(
                "arr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &m.arr_tracks,
                arr_view,
                &m.arr_selected_clips,
                &m.arr_selected_tracks,
                &arr_style,
                |req| match req {
                    ArrangementEditRequest::SelectClips { .. } => {
                        Edit::mutate(|_m: &mut Model| {})
                    }
                    ArrangementEditRequest::SelectTrack { .. } => {
                        Edit::mutate(|_m: &mut Model| {})
                    }
                    ArrangementEditRequest::MoveClips(_) => Edit::mutate(|_m: &mut Model| {
                        let _new_clip = ArrangementClip {
                            id: 0,
                            start_beat: 0.0,
                            len_beats: 1.0,
                            name: std::sync::Arc::from("clip"),
                            color: None,
                            share_group_color: None,
                        };
                    }),
                    ArrangementEditRequest::CloneClipsLinked(_) => {
                        Edit::mutate(|_m: &mut Model| {})
                    }
                    ArrangementEditRequest::CloneClipsIndependent(_) => {
                        Edit::mutate(|_m: &mut Model| {})
                    }
                    ArrangementEditRequest::ResizeClips(_) => Edit::mutate(|_m: &mut Model| {}),
                    ArrangementEditRequest::DeleteClips(_) => Edit::mutate(|_m: &mut Model| {}),
                    ArrangementEditRequest::DoubleClickClip(_) => {
                        Edit::mutate(|_m: &mut Model| {})
                    }
                    ArrangementEditRequest::DoubleClickEmpty { .. } => {
                        Edit::mutate(|_m: &mut Model| {})
                    }
                    ArrangementEditRequest::BeginRenameTrack(_) => {
                        Edit::mutate(|_m: &mut Model| {})
                    }
                    ArrangementEditRequest::DeleteTrack(_) => Edit::mutate(|_m: &mut Model| {}),
                    ArrangementEditRequest::MoveTrackUp(_) => Edit::mutate(|_m: &mut Model| {}),
                    ArrangementEditRequest::MoveTrackDown(_) => {
                        Edit::mutate(|_m: &mut Model| {})
                    }
                    ArrangementEditRequest::ReorderTracks(_) => {
                        Edit::mutate(|_m: &mut Model| {})
                    }
                    ArrangementEditRequest::SetTrackVolume { .. } => {
                        Edit::mutate(|_m: &mut Model| {})
                    }
                    ArrangementEditRequest::ToggleTrackMute(_) => {
                        Edit::mutate(|_m: &mut Model| {})
                    }
                    ArrangementEditRequest::ToggleTrackSolo(_) => {
                        Edit::mutate(|_m: &mut Model| {})
                    }
                    ArrangementEditRequest::SetLoopRange { .. } => {
                        Edit::mutate(|_m: &mut Model| {})
                    }
                    ArrangementEditRequest::SetPlayheadBeat(_) => {
                        Edit::mutate(|_m: &mut Model| {})
                    }
                    ArrangementEditRequest::SetZoomX(_) => Edit::mutate(|_m: &mut Model| {}),
                    ArrangementEditRequest::SetScrollX(_) => Edit::mutate(|_m: &mut Model| {}),
                    ArrangementEditRequest::SetTrackTop(_) => Edit::mutate(|_m: &mut Model| {}),
                    ArrangementEditRequest::SetTrackRowH(_) => {
                        Edit::mutate(|_m: &mut Model| {})
                    }
                    ArrangementEditRequest::ToggleGroupCollapsed(_) => {
                        Edit::mutate(|_m: &mut Model| {})
                    }
                    ArrangementEditRequest::SetTrackParent { tracks: _, parent: _, anchor_after: _ } => {
                        Edit::mutate(|_m: &mut Model| {})
                    }
                },
            );
            // M11 Phase 51: reorderable_list widget が non-Clone Model でコンパイルする。
            let rl_style = ReorderableListStyle::default();
            let _ = ui.reorderable_list(
                "rl",
                Rect { x: 0.0, y: 0.0, w: 200.0, h: 200.0 },
                &m.chain,
                None,
                &rl_style,
                |req| match req {
                    ReorderableListEditRequest::Reorder(_) => Edit::mutate(|_m: &mut Model| {}),
                },
                |_ui, _name: &String, _i, _row, _sel, _drag| {},
            );
        },
    );

    for e in edits {
        e.apply(&mut model);
    }
}
