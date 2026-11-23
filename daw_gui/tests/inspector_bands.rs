//! r.md #158 段 1: トラックインスペクタの帯 (`docs/plan_rmd_158_inspector.md` 仕様 1・4・5・6・12・13)。
//!
//! ルート view を headless で描き (GPU は使わない)、見出しのクリック / 右端のドラッグ / 隠す、を本番の
//! 経路で通して確かめる:
//! - 見出しのクリックで開閉し、押した見出しは動かない (先頭近く / 末尾までスクロールした状態の両方)。
//! - 開閉は見方の都合: `*` が付かず undo の段数も変わらず、ViewState で保存して戻る (知らない id は捨てる)。
//! - 開閉は種類ごとに共通 (音声クリップ A → B は畳んだまま、画像クリップは開いたまま。トラックの帯は全トラック共通)。
//! - 畳んだ帯 / 隠したインスペクタは Q の宛先を残さず、見えない EQ にスペクトラムを要求せず、開いていた
//!   dropdown の一覧も残さない。隠すと右の領域がその幅ぶん広がる。
//! - 右端のドラッグで幅が 360〜640 に畳まれ、離したときに app_config へ書かれる。
//! - 3 トラックの選択で、タイトル「3 トラック」、入力は「選択中の 3 トラック」、Rack 以下は「… のみ」。
//!
//! daw_gui 本体は起動しない (AppData を直に組む)。

use std::cell::Cell;
use std::sync::Arc;

use common::app_dirs::AppDirs;
use common::model::{AudioContent, Clip, ClipContent, ClipKey, ImageContent, NativeKind, RackPanelKey};
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent, ModSourceKindTag};
use daw_gui::dispatcher::{BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher};
use daw_gui::event_device::DeviceEvent;
use daw_gui::event_inspector::InspectorEvent;
use daw_gui::handler::inspector_view::{INSPECTOR_MAX_W, INSPECTOR_MIN_W};
use daw_gui::state::inspector_view::{ClipBandKind, InspectorBand, InspectorDetail, InspectorFolds};
use daw_gui::view::track_inspector::{panel_width, vocab};
use daw_gui::widgets::select_modifier::SelectModifier;
use daw_ui_core::{FrameInput, PointerFrame, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Primitive, Scene};

const W: u32 = 1400;
const H: u32 = 900;
const SCREEN: PhysicalSize = PhysicalSize { width: W, height: H };

struct Harness {
    app: AppData,
    host: UiHost<AppData>,
    /// 直近のフレームの末尾で popup が開いていたか。
    popups_open: Cell<bool>,
    _audio: UnboundedReceiver<AudioCommand>,
    _plugin: UnboundedReceiver<PluginCommand>,
}

impl Harness {
    fn new(app_dirs: Option<AppDirs>) -> Self {
        let (audio_tx, audio_rx) = mpsc::unbounded_channel();
        let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
        let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
        let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
        let mut app =
            AppData::new(audio_tx, plugin_tx, None, None, event_dispatcher, job_dispatcher, None, app_dirs, 48_000);
        app.handle_event(AppEvent::SetTheme("dark".to_string()));
        let mut host: UiHost<AppData> = UiHost::no_redraw();
        if host.set_palette(app.theme.core.clone()) {
            host.invalidate_scene_cache();
        }
        let mut h = Self { app, host, popups_open: Cell::new(false), _audio: audio_rx, _plugin: plugin_rx };
        let t0 = h.track(0);
        h.select(t0);
        h
    }

    fn track(&self, idx: usize) -> u32 {
        self.app.cur.song_doc.song().tracks[idx].id
    }

    fn select(&mut self, id: u32) {
        let visible: Vec<u32> = self.app.cur.song_doc.song().tracks.iter().map(|t| t.id).collect();
        self.app.apply_select_tracks(id, SelectModifier::Single, &visible);
    }

    fn add_to_selection(&mut self, id: u32) {
        let visible: Vec<u32> = self.app.cur.song_doc.song().tracks.iter().map(|t| t.id).collect();
        self.app.apply_select_tracks(id, SelectModifier::Toggle, &visible);
    }

    fn inspector(&mut self, ev: InspectorEvent) {
        self.app.handle_event(AppEvent::Inspector(ev));
    }

    /// 1 フレーム描く。`scroll_to_end` ならインスペクタの本文を末尾までスクロールしてから描く。
    fn frame_with(&mut self, pointer: PointerFrame, scroll_to_end: bool) -> Scene {
        let mut scene = Scene::new();
        let input = FrameInput { pointer, ..FrameInput::default() };
        let popups = &self.popups_open;
        self.host.frame(&mut self.app, &mut scene, SCREEN, input, |app, ui| {
            if scroll_to_end {
                ui.set_scroll_offset("inspector_body", (0.0, 1.0e6));
            }
            daw_gui::view::root::build_root(app, ui, SCREEN);
            popups.set(ui.has_open_popups());
        });
        scene
    }

    fn hover_at(&mut self, pos: (f32, f32)) -> Scene {
        self.frame_with(PointerFrame { pos: Some(pos), ..PointerFrame::default() }, false)
    }

    fn click(&mut self, pos: (f32, f32)) -> Scene {
        let p = PointerFrame {
            pos: Some(pos),
            primary_just_pressed: true,
            primary_just_released: true,
            ..PointerFrame::default()
        };
        self.frame_with(p, false)
    }

    /// 数フレーム回す (本文高の lag-by-one と床を落ち着かせる)。ポインタは右下の隅 (どこにも hover しない)。
    fn settle(&mut self) -> Scene {
        let mut scene = Scene::new();
        for _ in 0..4 {
            scene = self.hover_at((W as f32 - 5.0, H as f32 - 5.0));
        }
        scene
    }

    fn settle_at_end(&mut self) -> Scene {
        let idle = PointerFrame { pos: Some((W as f32 - 5.0, H as f32 - 5.0)), ..PointerFrame::default() };
        let mut scene = Scene::new();
        for _ in 0..4 {
            scene = self.frame_with(idle, true);
        }
        scene
    }
}

#[derive(Debug, Clone)]
struct Glyph {
    text: String,
    left: f32,
    top: f32,
}

/// 画面左 `max_x` より左の glyph (インスペクタの列)。
fn glyphs_left_of(scene: &Scene, max_x: f32) -> Vec<Glyph> {
    scene
        .primitives
        .iter()
        .filter_map(|p| match p {
            Primitive::Glyph(g) if g.left < max_x => Some(Glyph { text: g.text.to_string(), left: g.left, top: g.top }),
            _ => None,
        })
        .collect()
}

fn find(scene: &Scene, max_x: f32, text: &str) -> Option<Glyph> {
    glyphs_left_of(scene, max_x).into_iter().find(|g| g.text == text)
}

fn one(h: &Harness, scene: &Scene, text: &str) -> Glyph {
    let w = panel_width(&h.app).max(1.0);
    find(scene, w, text).unwrap_or_else(|| panic!("インスペクタに `{text}` が描かれている"))
}

fn center(g: &Glyph) -> (f32, f32) {
    (g.left + 8.0, g.top + 6.0)
}

/// Synth (トラック 0) の本文を縦に長くする: Reverb / Delay を足して組み込み EQ と一緒に Par を開き、
/// LFO を 1 つ足して本体を開く (末尾のモジュレーションの帯に中身がある)。
fn tall_synth(h: &mut Harness) -> u32 {
    let synth = h.track(0);
    for kind in [NativeKind::Reverb, NativeKind::Delay] {
        h.app.handle_event(AppEvent::Device(DeviceEvent::AddNative {
            chain: common::model::ChainRef::Track(synth),
            kind,
            open_panel: true,
        }));
    }
    let eq = h.app.cur.song_doc.song().builtin_native(synth, NativeKind::Eq).expect("組み込み EQ").id;
    h.app.handle_event(AppEvent::Device(DeviceEvent::ToggleRackPanel(RackPanelKey::Device(eq))));
    h.app.handle_event(AppEvent::AddModSource { kind: ModSourceKindTag::Lfo });
    let sid = h.app.mod_source_display().last().expect("足した LFO").id;
    h.app.cur.peph.expanded_mod_sources.insert(sid);
    synth
}

fn place(app: &mut AppData, track: u32, start: f64, content: ClipContent, name: &str) -> ClipKey {
    let name = name.to_string();
    let clip_id = app
        .edit_song(move |song| {
            let content_id = song.alloc_content(content, name);
            song.track_by_id_mut(track)
                .expect("track")
                .place_clip(Clip { start_beat: start, length_beats: 4.0, content_id, ..Clip::default() })
        })
        .expect("edit_song");
    ClipKey { track_id: track, clip_id }
}

/// 見出しのクリックで開閉し、押した見出しは動かない (先頭近く / 末尾までスクロールした状態)。
#[test]
fn band_header_click_toggles_without_moving_the_header() {
    let mut h = Harness::new(None);
    tall_synth(&mut h);

    // 先頭近く: Rack の見出しを押すと Rack が畳まれ、見出しの位置は変わらない。
    let scene = h.settle();
    let rack = one(&h, &scene, vocab::BAND_RACK);
    let modulation_before = one(&h, &scene, vocab::BAND_MODULATION).top;
    h.click(center(&rack));
    assert!(!h.app.inspector_band_open(InspectorBand::Rack), "Rack が畳まれた");
    let scene = h.settle();
    assert!((one(&h, &scene, vocab::BAND_RACK).top - rack.top).abs() < 0.5, "押した見出しは動かない");
    assert!(one(&h, &scene, vocab::BAND_MODULATION).top < modulation_before - 100.0, "下の帯は上へ詰まる");
    assert!(find(&scene, 360.0, "Reverb").is_none(), "Rack の中身は描かれない");
    h.click(center(&one(&h, &scene, vocab::BAND_RACK)));
    assert!(h.app.inspector_band_open(InspectorBand::Rack), "もう一度押すと開く");

    // 末尾までスクロールした状態: 末尾のモジュレーションの見出しを押して畳んでも、見出しは動かない
    // (本文が縮んでも scroll_area が offset を詰めない = 床)。
    let scene = h.settle_at_end();
    let body_before = h.app.cur.peph.inspector_body_h;
    let modulation = one(&h, &scene, vocab::BAND_MODULATION);
    h.click(center(&modulation));
    assert!(!h.app.inspector_band_open(InspectorBand::Modulation), "モジュレーションが畳まれた");
    let scene = h.settle();
    assert!(h.app.cur.peph.inspector_body_h < body_before - 50.0, "本文は縮んでいる");
    let after = one(&h, &scene, vocab::BAND_MODULATION);
    assert!((after.top - modulation.top).abs() < 0.5, "末尾で畳んでも見出しは動かない: {} → {}", modulation.top, after.top);
}

/// 開閉は見方の都合: `*` が付かず undo の段数も変わらず、ViewState で保存して戻る。知らない id は捨てる。
#[test]
fn band_folds_are_saved_as_view_state_without_dirtying_the_song() {
    let mut h = Harness::new(None);
    h.settle();
    let undo = h.app.cur.song_doc.undo_depth();
    let dirty = h.app.cur.song_doc.is_dirty();
    h.inspector(InspectorEvent::ToggleBand(InspectorBand::Rack));
    h.inspector(InspectorEvent::ToggleBand(InspectorBand::Clip(ClipBandKind::Audio)));
    h.inspector(InspectorEvent::ToggleDetail(InspectorDetail::LaunchMidi));
    h.settle();
    assert_eq!(h.app.cur.song_doc.is_dirty(), dirty, "開閉で `*` が付かない");
    assert_eq!(h.app.cur.song_doc.undo_depth(), undo, "開閉は undo に積まない");

    let folds = h.app.cur.view.inspector_folds.clone();
    let mut view = h.app.snapshot_view_state();
    assert_eq!(view.inspector_collapsed, ["clip.audio", "track.rack"]);
    assert_eq!(view.inspector_open_details, ["detail.launch_midi"]);
    view.inspector_collapsed.push("clip.hologram".to_string());
    h.app.cur.view.inspector_folds = InspectorFolds::default();
    let loop_region = h.app.cur.transport.loop_region;
    h.app.restore_view_state(Some(view), loop_region, Vec::new());
    assert_eq!(h.app.cur.view.inspector_folds, folds, "保存した開閉が戻り、知らない id は捨てる");
    // ViewState を持たない旧ファイル: 帯は全部開・詳細は閉。
    h.app.restore_view_state(None, loop_region, Vec::new());
    assert_eq!(h.app.cur.view.inspector_folds, InspectorFolds::default());
}

/// 開閉は種類ごとに共通 (仕様 6): 音声クリップ A で畳む → B も畳まれたまま → 画像クリップは開いている。
/// トラックの帯は全トラック共通。
#[test]
fn band_folds_are_shared_per_kind() {
    let mut h = Harness::new(None);
    let t0 = h.track(0);
    let a = place(&mut h.app, t0, 0.0, ClipContent::Audio(AudioContent::default()), "A");
    let b = place(&mut h.app, t0, 8.0, ClipContent::Audio(AudioContent::default()), "B");
    let img = place(&mut h.app, t0, 16.0, ClipContent::Image(ImageContent::default()), "Img");
    let audio_band = vocab::clip_band_title(ClipBandKind::Audio);

    h.app.handle_event(AppEvent::SelectClip { target: a, additive: false });
    let scene = h.settle();
    assert_eq!(h.app.inspector_clip_band().map(|(k, _)| k), Some(ClipBandKind::Audio));
    h.click(center(&one(&h, &scene, audio_band)));
    assert!(!h.app.inspector_band_open(InspectorBand::Clip(ClipBandKind::Audio)));

    h.app.handle_event(AppEvent::SelectClip { target: b, additive: false });
    h.settle();
    assert_eq!(h.app.inspector_clip_band(), Some((ClipBandKind::Audio, "B".to_string())));
    assert!(!h.app.inspector_band_open(InspectorBand::Clip(ClipBandKind::Audio)), "別の音声クリップでも畳まれたまま");

    h.app.handle_event(AppEvent::SelectClip { target: img, additive: false });
    h.settle();
    assert_eq!(h.app.inspector_clip_band().map(|(k, _)| k), Some(ClipBandKind::Image));
    assert!(h.app.inspector_band_open(InspectorBand::Clip(ClipBandKind::Image)), "画像クリップの帯は開いている");

    // トラックの帯: 1 本目で Rack を畳む → 2 本目でも畳まれたまま (中身が描かれない)。
    h.app.handle_event(AppEvent::AddInstrumentTrack);
    let t1 = h.track(1);
    h.select(t0);
    let scene = h.settle();
    h.click(center(&one(&h, &scene, vocab::BAND_RACK)));
    h.select(t1);
    let scene = h.settle();
    assert!(!h.app.inspector_band_open(InspectorBand::Rack));
    assert!(find(&scene, 360.0, "+ Plugin").is_none(), "別のトラックでも Rack は畳まれたまま");
}

/// 畳んだ帯 / 隠したインスペクタは Q の宛先を残さず、見えない EQ にスペクトラムを要求しない。
/// 隠すと開いていた dropdown の一覧も閉じ、右の領域がインスペクタの幅ぶん広がる。
#[test]
fn hidden_or_collapsed_bands_leave_no_hover_scopes_or_popups() {
    let mut h = Harness::new(None);
    let synth = tall_synth(&mut h);
    let eq = h.app.cur.song_doc.song().builtin_native(synth, NativeKind::Eq).expect("組み込み EQ").id;
    let scene = h.settle();
    assert_eq!(h.app.wanted_device_scopes(), vec![eq], "開いた EQ の Par はスペクトラムを要求する");

    // Rack の行に hover → Q の宛先。Rack を畳むと消える。
    let reverb = one(&h, &scene, "Reverb");
    h.hover_at(center(&reverb));
    assert!(h.app.cur.peph.inspector_hovered_row.is_some(), "Rack の行が Q の宛先");
    h.inspector(InspectorEvent::ToggleBand(InspectorBand::Rack));
    h.hover_at(center(&reverb));
    assert_eq!(h.app.cur.peph.inspector_hovered_row, None, "畳んだ Rack は宛先を残さない");
    assert!(h.app.wanted_device_scopes().is_empty(), "畳んだ Rack の EQ はスペクトラムを要求しない");

    // モジュレーターに hover → 宛先。モジュレーションを畳むと消える。
    let scene = h.settle();
    let lfo = glyphs_left_of(&scene, 360.0)
        .into_iter()
        .find(|g| g.text == "LFO" && g.top > one(&h, &scene, vocab::BAND_MODULATION).top)
        .expect("LFO の行");
    h.hover_at(center(&lfo));
    assert!(h.app.cur.peph.inspector_hovered_mod.is_some(), "モジュレーターが Q の宛先");
    h.inspector(InspectorEvent::ToggleBand(InspectorBand::Modulation));
    h.hover_at(center(&lfo));
    assert_eq!(h.app.cur.peph.inspector_hovered_mod, None, "畳んだモジュレーションは宛先を残さない");

    // 入力の dropdown を開いてからインスペクタを隠す → 一覧は閉じ、右の領域が広がる。
    h.inspector(InspectorEvent::ToggleBand(InspectorBand::Rack));
    let scene = h.settle();
    let snap_before = find(&scene, W as f32, "Snap").expect("アレンジのツールバー").left;
    h.click(center(&one(&h, &scene, "MIDI")));
    h.hover_at((W as f32 - 5.0, H as f32 - 5.0));
    assert!(h.popups_open.get(), "入力の dropdown が開いた");
    h.inspector(InspectorEvent::ToggleVisible);
    let scene = h.settle();
    assert!(!h.popups_open.get(), "隠したインスペクタの dropdown の一覧は残らない");
    assert_eq!(h.app.cur.peph.inspector_hovered_row, None);
    assert!(h.app.wanted_device_scopes().is_empty(), "隠したインスペクタはスペクトラムを要求しない");
    let snap_after = find(&scene, W as f32, "Snap").expect("アレンジのツールバー").left;
    assert!((snap_before - snap_after - INSPECTOR_MIN_W).abs() < 0.5, "右の領域が 360px 広がる: {snap_before} → {snap_after}");
}

/// 右端のドラッグで幅が 360〜640 に畳まれ、離したときに app_config へ書かれる。
#[test]
fn edge_drag_clamps_the_width_and_persists_on_release() {
    let data = tempfile::tempdir().unwrap();
    let dirs = AppDirs::under(data.path());
    let mut h = Harness::new(Some(dirs.clone()));
    h.settle();
    let edge = (panel_width(&h.app) - 2.0, 400.0);
    let saved_w = || daw_gui::app_config::load(dirs.app_config()).inspector_w;
    let drag = |h: &mut Harness, pos: (f32, f32), jp: bool, pressed: bool, jr: bool| {
        let p = PointerFrame {
            pos: Some(pos),
            primary_pressed: pressed,
            primary_just_pressed: jp,
            primary_just_released: jr,
            ..PointerFrame::default()
        };
        h.frame_with(p, false);
    };
    drag(&mut h, edge, true, true, false);
    drag(&mut h, (1200.0, 400.0), false, true, false);
    assert_eq!(h.app.ui_prefs.inspector_w, INSPECTOR_MAX_W, "上限で止まる");
    assert_eq!(saved_w(), INSPECTOR_MIN_W, "ドラッグ中は書かない");
    drag(&mut h, (1200.0, 400.0), false, false, true);
    assert_eq!(saved_w(), INSPECTOR_MAX_W, "離したら書く");

    h.settle();
    let edge = (panel_width(&h.app) - 2.0, 400.0);
    drag(&mut h, edge, true, true, false);
    drag(&mut h, (20.0, 400.0), false, true, false);
    drag(&mut h, (20.0, 400.0), false, false, true);
    assert_eq!(h.app.ui_prefs.inspector_w, INSPECTOR_MIN_W, "下限で止まる");
    assert_eq!(saved_w(), INSPECTOR_MIN_W);

    // 隠す / 出すも app_config に書く。
    h.inspector(InspectorEvent::ToggleVisible);
    assert!(!daw_gui::app_config::load(dirs.app_config()).inspector_open);
    assert_eq!(panel_width(&h.app), 0.0);
}

/// 3 トラックの選択: タイトル「3 トラック」、入力は「選択中の 3 トラック」、Rack / モジュレーションは
/// 「<カーソルのトラック> のみ」。
#[test]
fn multi_track_selection_shows_who_each_band_applies_to() {
    let mut h = Harness::new(None);
    h.app.handle_event(AppEvent::AddInstrumentTrack);
    h.app.handle_event(AppEvent::AddInstrumentTrack);
    let (t0, t1, t2) = (h.track(0), h.track(1), h.track(2));
    h.select(t0);
    h.add_to_selection(t1);
    h.add_to_selection(t2);
    let cursor = h.app.cursor_track_id().expect("カーソル");
    let cursor_name = h.app.cur.song_doc.song().track_display_name(cursor).into_owned();
    let scene = h.settle();
    assert_eq!(h.app.inspector_title().main, vocab::n_tracks(3));
    let input_target = vocab::target_selected_tracks(3);
    let cursor_only = vocab::target_cursor_only(&cursor_name);
    for text in [vocab::n_tracks(3), input_target, cursor_only] {
        assert!(find(&scene, 360.0, &text).is_some(), "`{text}` が描かれている");
    }
}
