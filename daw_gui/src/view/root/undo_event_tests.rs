//! キー操作 → `AppEvent` → undo 履歴の配線 (本番のキー定義で `dispatch_shortcuts` を 1 フレーム回す)。
//!
//! Song を変える操作は `handle_event` を通るので、**1 操作 = 1 undo step で操作名が付き、直前の
//! 編集の step に吸収されない**。以前は view が handler を直に呼んでいて、直前の event の undo
//! scope とラベルのまま積まれていた (範囲ミュート / 貼り付け / カット / セクション削除)。

use super::*;

use common::model::{Clip, ClipContent, LaneRef, MidiContent};
use daw_ui_core::{ClipboardProvider, FrameInput, PointerFrame, UiHost};
use daw_ui_platform::{ElementState, KeyEvent, Modifiers, PhysicalKey};
use daw_ui_renderer::Scene;

/// `Ctrl+V` の先読みに渡す clipboard。
struct FakeClipboard(Option<String>);

impl ClipboardProvider for FakeClipboard {
    fn get_text(&mut self) -> Option<String> {
        self.0.clone()
    }

    fn set_text(&mut self, text: String) {
        self.0 = Some(text);
    }
}

const TRACK_COLOR: [f32; 3] = [0.2, 0.4, 0.6];

/// トラック `1` / `2` (トラック 1 に `[0, 8)` の MIDI クリップ `10`)。
fn app_with_two_tracks() -> AppData {
    let mut app = crate::test_support::headless_app();
    app.edit_song(|song| {
        song.tracks.clear();
        let cid = song.alloc_content_id();
        song.clip_contents.insert(cid, ClipContent::Midi(MidiContent::default()));
        song.tracks.push(crate::app::track_with(|t| {
            t.id = 1;
            t.clips = vec![Clip { id: 10, content_id: cid, start_beat: 0.0, length_beats: 8.0, ..Clip::default() }];
        }));
        song.tracks.push(crate::app::track_with(|t| t.id = 2));
    });
    app
}

/// 直前の別の編集 (トラック 1 の色)。戻り値 = その後の履歴位置。
fn prior_edit(app: &mut AppData) -> usize {
    app.handle_event(AppEvent::SetTrackColor { track: 1, color: Some(TRACK_COLOR) });
    app.cur.song_doc.history_current()
}

/// `key` を 1 回押した 1 フレームを本番のキー定義で流し、push された Edit を適用する。
fn press(app: &mut AppData, key: PhysicalKey, mods: Modifiers, clipboard: Option<String>) {
    let mut host: UiHost<AppData> = UiHost::no_redraw().with_clipboard(FakeClipboard(clipboard));
    *host.shortcut_map_mut() = crate::view::shortcuts::daw_shortcut_map();
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: 1280, height: 720 };
    let bottom_rect = Rect { x: 0.0, y: 400.0, w: 1280.0, h: 320.0 };
    let text = match key {
        PhysicalKey::Char(c) if !mods.ctrl => Some(c.to_ascii_lowercase().to_string()),
        _ => None,
    };
    let input = FrameInput {
        keyboard: vec![KeyEvent { state: ElementState::Pressed, text, physical_key: key, repeat: false }],
        pointer: PointerFrame { modifiers: mods, ..Default::default() },
        ..FrameInput::default()
    };
    let edits = host.frame_to_edits(app, &mut scene, screen, input, |app, ui| {
        dispatch_shortcuts(app, ui, bottom_rect);
    });
    for e in edits {
        e.apply(app);
    }
}

/// `before` から **ちょうど 1 step** 進み、その step の名前が `label`。
fn assert_one_named_step(app: &AppData, before: usize, label: &str) {
    let doc = &app.cur.song_doc;
    assert_eq!(doc.history_current(), before + 1, "1 操作 = 1 undo step (直前の編集に吸収されない)");
    assert_eq!(doc.history_labels()[doc.history_current()], label, "履歴に操作名が付く");
}

/// undo 1 回で操作だけが戻り、直前の編集 (トラック 1 の色) は残る。
fn undo_keeps_prior_edit(app: &mut AppData, before: usize) {
    app.handle_event(AppEvent::Undo);
    assert_eq!(app.cur.song_doc.history_current(), before);
    let color = app.cur.song_doc.song().track_by_id(1).and_then(|t| t.color);
    assert_eq!(color, Some(TRACK_COLOR), "直前の編集は戻らない");
}

fn ctrl() -> Modifiers {
    Modifiers { ctrl: true, ..Modifiers::empty() }
}

#[test]
fn q_on_a_time_range_mutes_in_its_own_named_step() {
    let mut app = app_with_two_tracks();
    app.handle_event(AppEvent::SetTimeSelection { start_beat: 2.0, end_beat: 6.0, lanes: vec![LaneRef::Track(1)] });
    let before = prior_edit(&mut app);

    press(&mut app, PhysicalKey::Char('Q'), Modifiers::empty(), None);

    assert_one_named_step(&app, before, "範囲のミュート");
    let muted = |app: &AppData| app.cur.song_doc.song().track_by_id(1).is_some_and(|t| t.clips.iter().any(|c| c.muted));
    assert!(muted(&app), "範囲部分がミュートされる");
    undo_keeps_prior_edit(&mut app, before);
    assert!(!muted(&app));
}

#[test]
fn ctrl_v_pastes_clips_in_its_own_named_step() {
    let mut app = app_with_two_tracks();
    app.handle_event(AppEvent::SetTimeSelection { start_beat: 0.0, end_beat: 8.0, lanes: vec![LaneRef::Track(1)] });
    let (json, _) = app.copy_time_selection_clip().expect("クリップをコピーできる");
    let before = prior_edit(&mut app);
    app.cur.peph.arrange_hovered_track = Some(2);
    app.cur.peph.arrangement_hover_beat = Some(8.0);

    press(&mut app, PhysicalKey::Char('V'), ctrl(), Some(json));

    assert_one_named_step(&app, before, "クリップ貼り付け");
    let track2_clips = |app: &AppData| app.cur.song_doc.song().track_by_id(2).map_or(0, |t| t.clips.len());
    assert_eq!(track2_clips(&app), 1, "ポインタ下のトラックへ貼られる");
    assert_eq!(app.ui_ephemeral.status_message, "貼り付け: 1 クリップ");
    undo_keeps_prior_edit(&mut app, before);
    assert_eq!(track2_clips(&app), 0);
}

#[test]
fn ctrl_x_cuts_tracks_in_its_own_named_step() {
    let mut app = app_with_two_tracks();
    app.set_track_selection(vec![2]);
    let before = prior_edit(&mut app);

    press(&mut app, PhysicalKey::Char('X'), ctrl(), None);

    assert_one_named_step(&app, before, "トラックをカット");
    assert!(app.cur.song_doc.song().track_by_id(2).is_none(), "選んだトラックが消える");
    assert!(app.ui_ephemeral.pending_clipboard_write.is_some(), "clipboard へ載る");
    undo_keeps_prior_edit(&mut app, before);
    assert!(app.cur.song_doc.song().track_by_id(2).is_some());
}

#[test]
fn delete_removes_selected_sections_in_its_own_named_step() {
    let mut app = app_with_two_tracks();
    app.handle_event(AppEvent::Section(crate::event_section::SectionEvent::Create { start: 0.0, len: 8.0 }));
    let id = app.cur.song_doc.song().sections[0].id;
    app.apply_select_section(id, crate::widgets::arrangement::SelectModifier::Single);
    let before = prior_edit(&mut app);

    press(&mut app, PhysicalKey::Delete, Modifiers::empty(), None);

    assert_one_named_step(&app, before, "セクション削除");
    assert!(app.cur.song_doc.song().sections.is_empty(), "選んだ帯が消える");
    undo_keeps_prior_edit(&mut app, before);
    assert_eq!(app.cur.song_doc.song().sections.len(), 1);
}
