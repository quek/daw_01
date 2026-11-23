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
//! 段 2 (仕様 3・7・9): クリップの設定はクリップの帯の 1 か所だけ。
//! - 歌声の声はクリップの帯に出て、VOICEVOX の Par には口パク先だけが残る。
//! - テキストクリップの本文の欄は 1 本、字幕デバイスは Par を持たない。帯を畳むと入力中のセリフは確定する。
//! - 名前の欄で改名しても、アレンジのクリップの上の改名欄は出ない。
//! - 欄名は全部ラベル列に収まる。
//!
//! 段 3 (仕様 2・9): 欄の値の列は帯でも Par / SC でも同じ x。読み込み失敗と aux 出力は行の中。
//!
//! 段 4 (レビューの指摘): 描いた欄名が「…」で切れない、隠れた行の読み込み失敗が見出しに出る、タイトル / 帯の対象が
//! 正しい相手を指す、複数選択の声 / 読み上げは全部へ書く、字幕の片が無いテキストクリップで resync が落ち着く、
//! 幅のドラッグで縁が跳ばない。
//!
//! daw_gui 本体は起動しない (AppData を直に組む)。

use std::cell::Cell;
use std::sync::Arc;

use common::app_dirs::AppDirs;
use common::model::{
    AudioContent, AudioEvent, Clip, ClipContent, ClipKey, ImageContent, ImageEvent, MidiContent, NativeKind, RackPanelKey,
    TextContent, TextEvent,
};
use common::plugin_db::{BUILTIN_ID_VOICEVOX, PluginDatabase, SUBTITLE_ID, builtin_descriptors};
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent, ModSourceKindTag};
use daw_gui::dispatcher::{BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher};
use daw_gui::event_device::DeviceEvent;
use daw_gui::event_inspector::InspectorEvent;
use daw_gui::handler::inspector_view::{INSPECTOR_DEFAULT_W, INSPECTOR_MAX_W, INSPECTOR_MIN_W};
use daw_gui::state::inspector_view::{ClipBandKind, InspectorBand, InspectorDetail, InspectorFolds};
use daw_gui::view::track_inspector::layout::{FORM_GAP, FORM_LABEL_W, FORM_ROW_H, LABEL_FONT, RACK_INDENT, ROW_NAME_X};
use daw_gui::view::track_inspector::{panel_width, vocab};
use daw_gui::voicevox_client::{VoiceVoxSinger, VoiceVoxStyle};
use daw_gui::widgets::select_modifier::SelectModifier;
use daw_ui_core::{FrameInput, ImeEvent, PointerFrame, UiHost};
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
        Self::build(app_dirs, None)
    }

    /// 内蔵 plugin (VOICEVOX / 字幕) を picker から足せる DB つき。VOICEVOX の engine は起こさない。
    fn with_builtins() -> Self {
        let db = Arc::new(PluginDatabase::new(builtin_descriptors(), None, 0));
        let mut h = Self::build(None, Some(db));
        h.app.voicevox.voicevox_launch_attempted = true;
        h
    }

    fn build(app_dirs: Option<AppDirs>, db: Option<Arc<PluginDatabase>>) -> Self {
        let (audio_tx, audio_rx) = mpsc::unbounded_channel();
        let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
        let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
        let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
        let mut app =
            AppData::new(audio_tx, plugin_tx, None, db, event_dispatcher, job_dispatcher, None, app_dirs, 48_000);
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

    /// picker から内蔵 plugin を `track` に足す (本番の「+ プラグイン」と同じ経路)。返り値 = 足した device。
    fn add_builtin(&mut self, track: u32, id: &str) -> u64 {
        self.select(track);
        self.app.handle_event(AppEvent::OpenPluginPicker { chain: None });
        self.app.handle_event(AppEvent::SelectPluginFromDb { id: id.to_string(), keep_open: false, open_gui: false });
        let song = self.app.cur.song_doc.song();
        song.track_by_id(track).and_then(|t| t.plugins().filter(|p| p.plugin_id == id).last()).expect("足した device").id
    }

    /// フォーカス中の入力欄へ文字を確定入力する (IME の確定と同じ経路)。ポインタはどこにも hover しない。
    fn type_text(&mut self, text: &str) -> Scene {
        let input = FrameInput {
            pointer: PointerFrame { pos: Some((W as f32 - 5.0, H as f32 - 5.0)), ..PointerFrame::default() },
            ime: vec![ImeEvent::Commit(text.to_string())],
            ..FrameInput::default()
        };
        self.frame_input(input, false)
    }

    /// 1 フレーム描く。`scroll_to_end` ならインスペクタの本文を末尾までスクロールしてから描く。
    fn frame_with(&mut self, pointer: PointerFrame, scroll_to_end: bool) -> Scene {
        self.frame_input(FrameInput { pointer, ..FrameInput::default() }, scroll_to_end)
    }

    fn frame_input(&mut self, input: FrameInput, scroll_to_end: bool) -> Scene {
        let mut scene = Scene::new();
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

/// 選択の帯の種類 (帯が無ければ `None`)。
fn band_kind(app: &AppData) -> Option<ClipBandKind> {
    app.inspector_selection().band.map(|b| b.kind)
}

/// インスペクタのタイトル (描くときと同じく、選択を 1 回だけ導いて作る)。
fn title_of(app: &AppData) -> daw_gui::handler::inspector_title::InspectorTitle {
    app.inspector_title(&app.inspector_selection())
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
    assert_eq!(band_kind(&h.app), Some(ClipBandKind::Audio));
    h.click(center(&one(&h, &scene, audio_band)));
    assert!(!h.app.inspector_band_open(InspectorBand::Clip(ClipBandKind::Audio)));

    h.app.handle_event(AppEvent::SelectClip { target: b, additive: false });
    h.settle();
    assert_eq!(band_kind(&h.app), Some(ClipBandKind::Audio));
    assert_eq!(h.app.inspector_clip_band_target(&h.app.inspector_selection()), "B");
    assert!(!h.app.inspector_band_open(InspectorBand::Clip(ClipBandKind::Audio)), "別の音声クリップでも畳まれたまま");

    h.app.handle_event(AppEvent::SelectClip { target: img, additive: false });
    h.settle();
    assert_eq!(band_kind(&h.app), Some(ClipBandKind::Image));
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
    assert!(find(&scene, 360.0, vocab::ADD_PLUGIN).is_none(), "別のトラックでも Rack は畳まれたまま");
}

/// スペクトラムを要求するのは見える所に描いた EQ / Tone EQ の Par だけ (Comp 系の Par は数えない): Par を沢山
/// 開いて EQ の Par が viewport の下に押し出されていれば要求せず、末尾までスクロールして見えたら要求する。
#[test]
fn spectra_are_requested_only_for_eq_panels_drawn_in_view() {
    let mut h = Harness::new(None);
    let synth = tall_synth(&mut h);
    for _ in 0..6 {
        h.app.handle_event(AppEvent::Device(DeviceEvent::AddNative {
            chain: common::model::ChainRef::Track(synth),
            kind: NativeKind::Reverb,
            open_panel: true,
        }));
    }
    let song = h.app.cur.song_doc.song();
    let eq = song.builtin_native(synth, NativeKind::Eq).expect("組み込み EQ").id;
    let comp = song.builtin_native(synth, NativeKind::Comp).expect("組み込み Comp").id;
    h.app.handle_event(AppEvent::Device(DeviceEvent::ToggleRackPanel(RackPanelKey::Device(comp))));
    let scene = h.settle();
    let eq_row = glyphs_left_of(&scene, 360.0).into_iter().filter(|g| g.text == "EQ").map(|g| g.top).fold(f32::MIN, f32::max);
    assert!(eq_row > H as f32, "前提: EQ の行は viewport の下 ({eq_row})");
    assert!(h.app.wanted_device_scopes().is_empty(), "viewport の外の EQ の Par は要求しない");
    h.settle_at_end();
    assert_eq!(h.app.wanted_device_scopes(), vec![eq], "スクロールして見えたら要求する (Comp の Par は数えない)");
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
    // 入力元の dropdown (効果だけのトラックなので、既定の MIDI は「未選択」と出ている — 警告にしない)。
    let input = one(&h, &scene, daw_gui::handler::audio_rec::INPUT_UNSET);
    assert!(
        !glyphs_left_of(&scene, 360.0)
            .iter()
            .any(|g| g.text.contains(daw_gui::handler::audio_rec::INPUT_NOT_RECORDABLE)),
        "何も触っていない状態に「録れない」を出さない"
    );
    h.click(center(&input));
    h.hover_at((W as f32 - 5.0, H as f32 - 5.0));
    assert!(h.popups_open.get(), "入力の dropdown が開いた");
    h.inspector(InspectorEvent::ToggleVisible);
    let scene = h.settle();
    assert!(!h.popups_open.get(), "隠したインスペクタの dropdown の一覧は残らない");
    assert_eq!(h.app.cur.peph.inspector_hovered_row, None);
    assert!(h.app.wanted_device_scopes().is_empty(), "隠したインスペクタはスペクトラムを要求しない");
    let snap_after = find(&scene, W as f32, "Snap").expect("アレンジのツールバー").left;
    assert!(
        (snap_before - snap_after - INSPECTOR_DEFAULT_W).abs() < 0.5,
        "右の領域が隠す前の幅ぶん広がる: {snap_before} → {snap_after}"
    );
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
    // 帯 (右端の 5px) のどこを掴んでも縁は跳ばない: 掴んだだけでは幅は変わらず、10px 動かすと 10px 広がる。
    let grab = (panel_width(&h.app) - 4.0, 400.0);
    drag(&mut h, grab, true, true, false);
    assert_eq!(h.app.ui_prefs.inspector_w, INSPECTOR_DEFAULT_W, "掴んだだけでは幅が跳ばない");
    drag(&mut h, (grab.0 + 10.0, 400.0), false, true, false);
    assert_eq!(h.app.ui_prefs.inspector_w, INSPECTOR_DEFAULT_W + 10.0, "動かした分だけ広がる");
    drag(&mut h, (grab.0 + 10.0, 400.0), false, false, true);
    h.settle();
    h.inspector(InspectorEvent::SetWidth { w: INSPECTOR_DEFAULT_W, commit: true });
    h.settle();

    drag(&mut h, edge, true, true, false);
    drag(&mut h, (1200.0, 400.0), false, true, false);
    assert_eq!(h.app.ui_prefs.inspector_w, INSPECTOR_MAX_W, "上限で止まる");
    assert_eq!(saved_w(), INSPECTOR_DEFAULT_W, "ドラッグ中は書かない (まだ書いていないのは掴む前の幅)");
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
    assert_eq!(title_of(&h.app).main, vocab::n_tracks(3));
    let input_target = vocab::target_selected_tracks(3);
    let cursor_only = vocab::target_cursor_only(&cursor_name);
    for text in [vocab::n_tracks(3), input_target, cursor_only] {
        assert!(find(&scene, 360.0, &text).is_some(), "`{text}` が描かれている");
    }
}

fn singer(name: &str, styles: &[(u32, &str)]) -> VoiceVoxSinger {
    VoiceVoxSinger {
        name: name.to_string(),
        styles: styles.iter().map(|&(id, n)| VoiceVoxStyle { id, name: n.to_string() }).collect(),
    }
}

/// `label` の欄の値 (同じ行で、ラベル列より右にある `text`)。
fn field_value(h: &Harness, scene: &Scene, label: &str, text: &str) -> Glyph {
    let label = one(h, scene, label);
    glyphs_left_of(scene, panel_width(&h.app))
        .into_iter()
        .find(|g| g.text == text && g.left > label.left + 50.0 && (g.top - label.top).abs() < 8.0)
        .unwrap_or_else(|| panic!("`{}` の欄に `{text}`", label.text))
}

fn count(h: &Harness, scene: &Scene, text: &str) -> usize {
    glyphs_left_of(scene, panel_width(&h.app)).iter().filter(|g| g.text == text).count()
}

/// 歌声の声はクリップの帯 (Rack より上) の 1 か所だけ。VOICEVOX の Par に残るのは口パク先。
#[test]
fn singing_voice_lives_in_the_clip_band_and_par_keeps_only_the_lipsync_target() {
    let mut h = Harness::with_builtins();
    let t0 = h.track(0);
    let vv = h.add_builtin(t0, BUILTIN_ID_VOICEVOX);
    h.app.handle_event(AppEvent::SingersLoaded(vec![singer("ずんだもん", &[(3, "ノーマル"), (1, "あまあま")])]));
    let clip = place(&mut h.app, t0, 0.0, ClipContent::Midi(MidiContent::default()), "Verse");
    h.app.handle_event(AppEvent::SelectClip { target: clip, additive: false });
    h.app.handle_event(AppEvent::Device(DeviceEvent::ToggleRackPanel(RackPanelKey::Device(vv))));
    let scene = h.settle();
    let rack = one(&h, &scene, vocab::BAND_RACK).top;
    assert!(one(&h, &scene, vocab::SUB_VOICE).top < rack, "歌声はクリップの帯 (Rack より上)");
    assert_eq!(count(&h, &scene, vocab::FIELD_CHARACTER), 1, "声の欄は 1 か所だけ (Par に無い)");
    assert!(one(&h, &scene, vocab::FIELD_LIPSYNC_TARGET).top > rack, "VOICEVOX の Par には口パク先");
}

/// テキストクリップの本文の欄は 1 本 (読み上げと字幕で共有)、字幕デバイスは Par を持たない。
/// 本文に打っている途中で帯を畳むと、見出しを押した press で入力欄が確定する。
#[test]
fn text_clip_has_one_body_field_and_collapsing_commits_it() {
    let mut h = Harness::with_builtins();
    let t0 = h.track(0);
    let vv = h.add_builtin(t0, BUILTIN_ID_VOICEVOX);
    let sub = h.add_builtin(t0, SUBTITLE_ID);
    let line = TextEvent { text: "こんにちは".to_string(), event_length_beats: 4.0, ..TextEvent::default() };
    let clip = place(&mut h.app, t0, 0.0, ClipContent::Text(TextContent { events: vec![line] }), "");
    h.app.handle_event(AppEvent::SelectClip { target: clip, additive: false });
    h.app.handle_event(AppEvent::Device(DeviceEvent::ToggleRackPanel(RackPanelKey::Device(vv))));

    // 字幕デバイス: Par のボタンが無く、GUI / Par の操作をしても何も開かない。
    let plugin = h.app.cur.song_doc.song().plugin_by_id(sub).expect("字幕デバイス").clone();
    let entry = h.app.chain_entry_for(&plugin);
    assert!(!entry.shows_param_panel() && !entry.shows_button(), "字幕デバイスは Par を持たない");
    h.app.handle_event(AppEvent::Device(DeviceEvent::ToggleSlotGui { device_id: sub }));
    assert!(!h.app.rack_panel_open(RackPanelKey::Device(sub)));

    let scene = h.settle();
    let rack = one(&h, &scene, vocab::BAND_RACK).top;
    assert_eq!(count(&h, &scene, vocab::FIELD_LINE), 1, "本文の欄は 1 本");
    assert_eq!(count(&h, &scene, vocab::FIELD_TEXT), 0);
    assert!(one(&h, &scene, vocab::SUB_TALK).top < rack, "読み上げはクリップの帯");
    assert!(one(&h, &scene, vocab::SUB_SUBTITLE).top < rack, "字幕はクリップの帯");
    assert!(find(&scene, 360.0, vocab::SUB_FILL).is_none(), "字幕の細かい設定は最初は畳まれている");

    // 本文に打ち、確定せずに帯の見出しを押す → 確定してから畳まれる。
    let body = field_value(&h, &scene, vocab::FIELD_LINE, "こんにちは");
    h.click(center(&body));
    h.type_text("さようなら");
    let band = one(&h, &scene, vocab::clip_band_title(ClipBandKind::Text));
    h.click(center(&band));
    assert!(!h.app.inspector_band_open(InspectorBand::Clip(ClipBandKind::Text)), "帯が畳まれた");
    assert_eq!(h.app.text_first_event(clip, |e| e.text.clone()).as_deref(), Some("さようなら"), "入力中のセリフが確定した");
}

/// 名前の欄で改名する: 確定 (欄の外のクリック) で名前が変わり (1 undo step)、その間アレンジのクリップの上の
/// 改名欄は出ない (同じ値の入力欄を 2 つ並べない)。
#[test]
fn name_field_renames_without_the_arrangement_rename_box() {
    let mut h = Harness::new(None);
    let t0 = h.track(0);
    let clip = place(&mut h.app, t0, 0.0, ClipContent::Midi(MidiContent::default()), "Lead");
    h.app.handle_event(AppEvent::SelectClip { target: clip, additive: false });
    let scene = h.settle();
    let undo = h.app.cur.song_doc.undo_depth();
    let name = field_value(&h, &scene, vocab::FIELD_NAME, "Lead");
    h.click(center(&name));
    h.type_text("Hook");
    assert_eq!(h.app.cur.peph.clip_rename, None, "打っている間もアレンジの改名欄は出ない");
    h.click(center(&one(&h, &scene, vocab::FIELD_NAME)));
    let song = h.app.cur.song_doc.song();
    let content_id = song.clip_by_key(clip).expect("clip").content_id;
    assert_eq!(song.content_name(content_id), "Hook", "欄の外のクリックで確定した");
    assert_eq!(h.app.cur.peph.clip_rename, None);
    assert_eq!(h.app.cur.song_doc.undo_depth(), undo + 1, "改名は 1 undo step");
}

/// 欄名は全部ラベル列に収まる (「…」で切れない)。帯の欄 (`vocab::FORM_LABELS`) は帯のラベル列、Rack の行の
/// 直下の欄 (`vocab::NESTED_FORM_LABELS` と SC / 出力の「入力 N」「出力 N」) は Parallel の深さ 2 の狭いラベル列
/// (行の名前の左端から始まり、1 段ごとにインデントするぶん狭い — `form::cols_nested`)。
#[test]
fn form_labels_fit_the_label_column() {
    let mut h = Harness::new(None);
    let band_w = FORM_LABEL_W - FORM_GAP;
    let nested_w = FORM_LABEL_W - ROW_NAME_X - 2.0 * RACK_INDENT - FORM_GAP;
    let mut nested: Vec<String> = vocab::NESTED_FORM_LABELS.iter().map(|l| l.to_string()).collect();
    nested.extend((1..=8).map(vocab::sc_port));
    nested.extend((1..=16).map(vocab::aux_out_port));
    let too_wide = Cell::new(Vec::new());
    let widest = Cell::new(0.0_f32);
    let mut scene = Scene::new();
    h.host.frame(&mut h.app, &mut scene, SCREEN, FrameInput::default(), |_, ui| {
        let band: Vec<(String, f32, f32)> =
            vocab::FORM_LABELS.iter().map(|l| (l.to_string(), ui.measure_text(l, LABEL_FONT), band_w)).collect();
        let nest: Vec<(String, f32, f32)> =
            nested.iter().map(|l| (l.clone(), ui.measure_text(l, LABEL_FONT), nested_w)).collect();
        widest.set(band.iter().map(|(_, w, _)| *w).fold(0.0, f32::max));
        too_wide.set(band.into_iter().chain(nest).filter(|(_, w, col)| w > col).collect());
    });
    // 測れていること (0 を返す測り方ならこの検査は素通りしてしまう)。
    assert!(widest.get() > FORM_LABEL_W * 0.6, "一番長い欄名の幅 {} px", widest.get());
    assert_eq!(
        too_wide.take(),
        Vec::<(String, f32, f32)>::new(),
        "ラベル列 (帯 {band_w} / 入れ子 {nested_w} px) に収まらない欄名"
    );
}

/// 描いた画面で、インスペクタの 11px の文字 (欄名・行の名前・帯の対象) が「…」で切れていない。欄名を vocab の
/// 一覧に登録し忘れても、ここが拾う (一覧を測るだけのテストは、一覧に無い欄名を素通りする)。帯を開いた状態を
/// 一通り描く: 音声 / 画像 / 読み上げ + 字幕 (詳細も) / セル (MIDI 割り当ても) と、EQ の Par / Comp の SC を
/// 開いた Rack。
#[test]
fn drawn_inspector_labels_are_never_cut() {
    let mut h = Harness::with_builtins();
    let t0 = h.track(0);
    let eq = h.app.cur.song_doc.song().builtin_native(t0, NativeKind::Eq).expect("組み込み EQ").id;
    let comp = h.app.cur.song_doc.song().builtin_native(t0, NativeKind::Comp).expect("組み込み Comp").id;
    h.app.handle_event(AppEvent::Device(DeviceEvent::ToggleRackPanel(RackPanelKey::Device(eq))));
    h.app.cur.peph.open_sidechain_panel = Some(comp);
    h.inspector(InspectorEvent::ToggleDetail(InspectorDetail::SubtitleStyle));
    h.inspector(InspectorEvent::ToggleDetail(InspectorDetail::LaunchMidi));
    let audio_content = AudioContent { events: vec![AudioEvent::default()], next_event_id: 2 };
    let audio = place(&mut h.app, t0, 0.0, ClipContent::Audio(audio_content), "A");
    let image = place(&mut h.app, t0, 8.0, ClipContent::Image(ImageContent { events: vec![ImageEvent::default()] }), "I");
    // 読み上げのトラック (足したトラックがカーソルになる。並びの位置は選択で変わるので index では引かない)。
    h.app.handle_event(AppEvent::AddInstrumentTrack);
    let talk = h.app.cursor_track_id().expect("足したトラック");
    assert_ne!(talk, t0);
    h.add_builtin(talk, BUILTIN_ID_VOICEVOX);
    h.add_builtin(talk, SUBTITLE_ID);
    let line = TextEvent { text: "こんにちは".to_string(), event_length_beats: 4.0, ..TextEvent::default() };
    let text = place(&mut h.app, talk, 0.0, ClipContent::Text(TextContent { events: vec![line] }), "");
    let mut checked = 0;
    let mut check = |h: &mut Harness, what: &str| {
        let scene = h.settle();
        let w = panel_width(&h.app);
        let small: Vec<String> = scene
            .primitives
            .iter()
            .filter_map(|p| match p {
                Primitive::Glyph(g) if g.left < w && (g.font_size - LABEL_FONT).abs() < 0.01 => Some(g.text.to_string()),
                _ => None,
            })
            .collect();
        checked += small.len();
        let cut: Vec<&String> = small.iter().filter(|t| t.ends_with('…')).collect();
        assert!(cut.is_empty(), "{what}: 「…」で切れた文字 {cut:?}");
    };
    for (what, clip) in [("音声クリップ", audio), ("画像クリップ", image), ("読み上げ + 字幕", text)] {
        h.app.handle_event(AppEvent::SelectClip { target: clip, additive: false });
        check(&mut h, what);
    }
    h.select(t0);
    let row = daw_gui::event_launcher::LauncherRow::Track(t0);
    h.app.handle_event(AppEvent::Launcher(daw_gui::event_launcher::LauncherEvent::CreateCell { row, scene_index: 0 }));
    check(&mut h, "セル");
    assert!(checked > 60, "11px の文字を描いている ({checked})");
}

/// 字幕の数値欄は欄の種類ごとに別の undo の束ね (§6-13: `InspectorScrubField::Text` から device を外しても、
/// 同時に描かれる X と Y の鍵は重ならない)。X を数フレームドラッグしても 1 undo step で、Y は動かない。
#[test]
fn subtitle_field_drag_is_one_undo_step() {
    let mut h = Harness::with_builtins();
    let t0 = h.track(0);
    h.add_builtin(t0, SUBTITLE_ID);
    let line = TextEvent { text: "字幕".to_string(), event_length_beats: 4.0, x: 0.1, y: 0.4, ..TextEvent::default() };
    let clip = place(&mut h.app, t0, 0.0, ClipContent::Text(TextContent { events: vec![line] }), "");
    h.app.handle_event(AppEvent::SelectClip { target: clip, additive: false });
    let scene = h.settle();
    let x = field_value(&h, &scene, "X", "0.100");
    let at = center(&x);
    let undo = h.app.cur.song_doc.undo_depth();
    let frame = |h: &mut Harness, pos: (f32, f32), jp: bool, pressed: bool, jr: bool| {
        let p = PointerFrame {
            pos: Some(pos),
            primary_pressed: pressed,
            primary_just_pressed: jp,
            primary_just_released: jr,
            ..PointerFrame::default()
        };
        h.frame_with(p, false);
    };
    frame(&mut h, at, true, true, false);
    for i in 1..=5 {
        frame(&mut h, (at.0 + 10.0 * i as f32, at.1), false, true, false);
    }
    frame(&mut h, (at.0 + 50.0, at.1), false, false, true);
    h.settle();
    let (x, y) = h.app.text_first_event(clip, |e| (e.x, e.y)).expect("字幕");
    assert!(x > 0.15, "右へ引いたので X が増える: {x}");
    assert!((y - 0.4).abs() < 1e-6, "Y は動かない: {y}");
    assert_eq!(h.app.cur.song_doc.undo_depth(), undo + 1, "1 ドラッグ = 1 undo step");
}

/// `label` の欄の値の部品 (数値欄 / dropdown) の左端 x: ラベルと同じ行にある、欄の高さの矩形のうち一番左のもの。
fn value_left(h: &Harness, scene: &Scene, label: &str) -> f32 {
    let label = one(h, scene, label);
    let row_y = label.top - (FORM_ROW_H - LABEL_FONT * 1.2) * 0.5;
    scene
        .primitives
        .iter()
        .filter_map(|p| match p {
            Primitive::Rect(r)
                if (r.rect.h - FORM_ROW_H).abs() < 0.5 && (r.rect.y - row_y).abs() < 1.0 && r.rect.x > label.left + 8.0 =>
            {
                Some(r.rect.x)
            }
            _ => None,
        })
        .fold(None, |m: Option<f32>, x| Some(m.map_or(x, |m| m.min(x))))
        .unwrap_or_else(|| panic!("`{}` の欄の値の部品", label.text))
}

/// r.md #158 仕様 9: 欄の値の列は、帯 (クリップ / 入力) の欄でも Rack の行の直下 (Par / SC) の欄でも同じ x に
/// 1 本で並ぶ (Par の中身は行の名前の左端から始まり、ラベル列だけが狭くなる)。
#[test]
fn field_values_line_up_in_one_column_across_bands_and_par() {
    let mut h = Harness::with_builtins();
    let t0 = h.track(0);
    let vv = h.add_builtin(t0, BUILTIN_ID_VOICEVOX);
    h.app.handle_event(AppEvent::Device(DeviceEvent::ToggleRackPanel(RackPanelKey::Device(vv))));
    let comp = h.app.cur.song_doc.song().builtin_native(t0, NativeKind::Comp).expect("組み込み Comp").id;
    h.app.cur.peph.open_sidechain_panel = Some(comp);
    let event = AudioEvent { id: 1, source_id: 1, event_length_beats: 4.0, ..AudioEvent::default() };
    let clip = place(&mut h.app, t0, 0.0, ClipContent::Audio(AudioContent { events: vec![event], next_event_id: 2 }), "A");
    h.app.handle_event(AppEvent::SelectClip { target: clip, additive: false });
    let scene = h.settle();
    let expected = daw_gui::view::track_inspector::layout::PAD + FORM_LABEL_W + FORM_GAP;
    for label in [vocab::FIELD_GAIN, vocab::FIELD_INPUT_SOURCE, vocab::FIELD_LIPSYNC_TARGET, &vocab::sc_port(1)] {
        let x = value_left(&h, &scene, label);
        assert!((x - expected).abs() < 0.5, "`{label}` の値の左端 {x} が帯の欄の列 {expected} に揃う");
    }
    let lipsync = one(&h, &scene, vocab::FIELD_LIPSYNC_TARGET);
    let rack_row_name = daw_gui::view::track_inspector::layout::PAD + daw_gui::view::track_inspector::layout::ROW_NAME_X;
    assert!((lipsync.left - rack_row_name).abs() < 0.5, "Par の欄は行の名前の左端から: {}", lipsync.left);
}

/// r.md #158 仕様 2: 読み込みに失敗した plugin は行の中 ([再読込] と直下の理由 1 行)、aux 出力は行の `Out▾` で
/// 直下に開く (チェーンの下の別セクションに出さない)。展開すると port ごとの宛先の行が同じパネルに足される。
#[test]
fn failed_load_and_aux_outputs_live_in_their_own_row() {
    let mut h = Harness::with_builtins();
    let t0 = h.track(0);
    let vv = h.add_builtin(t0, BUILTIN_ID_VOICEVOX);
    let sub = h.add_builtin(t0, SUBTITLE_ID);
    h.app.edit_song(|song| {
        song.plugin_by_id_mut(vv).expect("VOICEVOX").aux_output_count = 2;
    });
    h.app.cur.pipc.failed_plugin_loads.insert(sub, "shmem create failed".to_string());
    let scene = h.settle();
    let failed = glyphs_left_of(&scene, panel_width(&h.app))
        .into_iter()
        .find(|g| g.text.starts_with(vocab::NOT_LOADED))
        .expect("失敗した行の名前に [未ロード]");
    let reload = one(&h, &scene, vocab::RELOAD);
    assert!((reload.top - failed.top).abs() < 8.0, "[再読込] は失敗した行の中");
    let reason = one(&h, &scene, "shmem create failed");
    let row_h = daw_gui::view::track_inspector::layout::RACK_ROW_H;
    assert!(reason.top > failed.top && reason.top < failed.top + row_h + 4.0, "理由は失敗した行の直下");
    assert!(find(&scene, 360.0, "読み込み失敗").is_none() && find(&scene, 360.0, "Parallel Out").is_none());

    // Out▾ を押すと VOICEVOX の行の直下に出力のパネル。展開すると子トラックと port ごとの宛先の行。
    let out = one(&h, &scene, "Out\u{25BE}");
    h.click(center(&out));
    let scene = h.settle();
    let child = one(&h, &scene, vocab::FIELD_CHILD_TRACKS);
    assert!(child.top > out.top && child.top < out.top + row_h + 8.0, "出力のパネルは Out の行の直下");
    assert!(child.top < failed.top, "パネルは次の行 (字幕) より上");
    h.click(center(&field_value(&h, &scene, vocab::FIELD_CHILD_TRACKS, vocab::EXPLODE_OUTPUTS)));
    let scene = h.settle();
    assert!(h.app.aux_out_entry(vv).is_some_and(|e| e.exploded), "展開で配線された");
    let port2 = one(&h, &scene, &vocab::aux_out_port(2));
    assert!(port2.top > one(&h, &scene, vocab::FIELD_CHILD_TRACKS).top, "port の行は展開の行の下");
}

/// 読み込みに失敗した plugin は、行が見えていなくても気付ける: 畳んだ Parallel の見出しの行に「失敗 N」、Rack の帯の
/// 見出しに「読み込み失敗 N」(Rack を畳んでいても見える)。
#[test]
fn failed_loads_stay_visible_when_their_row_is_hidden() {
    let mut h = Harness::with_builtins();
    let t0 = h.track(0);
    h.app.handle_event(AppEvent::Device(DeviceEvent::AddParallel {
        chain: common::model::ChainRef::Track(t0),
        at: daw_gui::device_addr::InsertAt::Default,
    }));
    let song = h.app.cur.song_doc.song();
    let (parallel_id, chain_id) = song
        .track_by_id(t0)
        .and_then(|t| {
            t.devices.iter().find_map(|d| match d {
                common::model::Device::Parallel(p) => Some((p.id, p.chains[0].id)),
                _ => None,
            })
        })
        .expect("Parallel");
    // chain の中に映像 FX を足し、その読み込みが失敗した。
    h.app.handle_event(AppEvent::OpenPluginPicker { chain: Some(common::model::ChainRef::Chain(chain_id)) });
    h.app.handle_event(AppEvent::SelectPluginFromDb {
        id: "builtin.video.vignette".to_string(),
        keep_open: false,
        open_gui: false,
    });
    let fx = h.app.cur.song_doc.song().track_by_id(t0).and_then(|t| t.plugins().last().map(|p| p.id)).expect("足した FX");
    h.app.cur.pipc.failed_plugin_loads.insert(fx, "shmem create failed".to_string());
    h.app.handle_event(AppEvent::Device(DeviceEvent::ToggleParallelNodeCollapsed { id: parallel_id }));
    let scene = h.settle();
    let hidden = one(&h, &scene, &vocab::hidden_failures(1));
    let parallel = one(&h, &scene, "Parallel");
    assert!((hidden.top - parallel.top).abs() < 4.0, "畳んだ Parallel の見出しの行に「失敗 1」");
    assert!(find(&scene, 360.0, vocab::RELOAD).is_none(), "行は畳まれて見えない");
    one(&h, &scene, &vocab::failed_loads(1));

    // Rack を畳んでも、見出しの「読み込み失敗 1」は残る。
    h.inspector(InspectorEvent::ToggleBand(InspectorBand::Rack));
    let scene = h.settle();
    assert!(find(&scene, 360.0, "Parallel").is_none(), "Rack の中身は描かれない");
    let warn = one(&h, &scene, &vocab::failed_loads(1));
    assert!((warn.top - one(&h, &scene, vocab::BAND_RACK).top).abs() < 4.0, "Rack の見出しの行");
}

/// タイトルと帯の対象が、指している相手を正しく言う:
/// - クリップを選んだまま別のトラックをカーソルにしても、2 行目と色見本はクリップの居るトラック。
/// - 名前の無いセルは「(名前なし)」、2 行目に種類と「セル」。
/// - 1 本だけ選んだトラックの名前は、帯の見出しに繰り返さない (タイトルだけ)。
/// - 複数選択で録れるトラックが 1 本だけなら、入力の帯の対象はその 1 本 (カーソルではない)。
#[test]
fn title_and_band_targets_name_the_right_track() {
    let mut h = Harness::with_builtins();
    let t0 = h.track(0);
    let a_name = h.app.cur.song_doc.song().track_display_name(t0).into_owned();
    h.app.cur.selection.selected_track_ids.clear();
    h.app.handle_event(AppEvent::AddInstrumentTrack);
    let t1 = h.app.cursor_track_id().expect("足したトラック");
    let b_name = h.app.cur.song_doc.song().track_display_name(t1).into_owned();
    let clip = place(&mut h.app, t0, 0.0, ClipContent::Midi(MidiContent::default()), "Lead");

    // クリップを選んで、トラック B のヘッダを押す (クリップの選択は残る)。
    h.app.handle_event(AppEvent::SelectClip { target: clip, additive: false });
    h.select(t1);
    assert_eq!(h.app.inspector_target_refs(), vec![clip], "クリップの選択は残る");
    let title = title_of(&h.app);
    assert_eq!(title.main, "Lead");
    assert!(title.sub.ends_with(&vocab::track_label(&a_name)), "2 行目はクリップの居るトラック: {}", title.sub);
    assert_eq!(
        title.sub_swatch.map(|s| s.target),
        Some(daw_gui::color_target::ColorPickerTarget::Track(t0)),
        "色見本もクリップの居るトラック (カーソルの {b_name} ではない)"
    );

    // 名前の無いセル。
    let row = daw_gui::event_launcher::LauncherRow::Track(t0);
    h.app.handle_event(AppEvent::Launcher(daw_gui::event_launcher::LauncherEvent::CreateCell { row, scene_index: 0 }));
    let title = title_of(&h.app);
    assert_eq!(title.main, vocab::UNNAMED_CLIP);
    let kind = vocab::clip_band_title(ClipBandKind::Midi);
    let expected = [kind, vocab::PLACE_CELL, &vocab::track_label(&a_name)].join(vocab::SEP);
    assert_eq!(title.sub, expected);

    // 1 本だけ選んだトラック: 名前はタイトルだけ (帯の見出しに繰り返さない)。
    h.app.handle_event(AppEvent::ClearSelection);
    h.select(t0);
    let scene = h.settle();
    assert!(h.app.inspector_target_refs().is_empty(), "クリップを選んでいない");
    assert_eq!(count(&h, &scene, &a_name), 1, "トラック名はタイトルの 1 回だけ");

    // 複数選択のうち録れるのは t1 だけ (t2 は映像 FX だけのトラック = 何も録れない)、カーソルは t2。
    h.app.cur.selection.selected_track_ids.clear();
    h.app.handle_event(AppEvent::AddInstrumentTrack);
    let t2 = h.app.cursor_track_id().expect("足したトラック");
    h.add_builtin(t2, "builtin.video.vignette");
    h.select(t1);
    h.add_to_selection(t2);
    assert_eq!(h.app.cursor_track_id(), Some(t2));
    let targets = h.app.input_targets();
    assert_eq!(targets.iter().map(|t| t.track.id).collect::<Vec<_>>(), vec![t1]);
    assert_eq!(h.app.input_band_target(&targets), b_name, "入力の帯の対象は書き込み先の 1 本");
}

/// 歌声の声と読み上げのスケールは、選んでいる同じ種類のクリップ全部へ 1 undo step で書く。値が割れていれば欄は「—」。
#[test]
fn voice_and_talk_edits_reach_every_selected_clip() {
    let mut h = Harness::with_builtins();
    let t0 = h.track(0);
    h.add_builtin(t0, BUILTIN_ID_VOICEVOX);
    h.app.handle_event(AppEvent::SingersLoaded(vec![singer("ずんだもん", &[(3, "ノーマル"), (1, "あまあま")])]));
    let a = place(&mut h.app, t0, 0.0, ClipContent::Midi(MidiContent::default()), "A");
    let b = place(&mut h.app, t0, 8.0, ClipContent::Midi(MidiContent::default()), "B");
    h.app.handle_event(AppEvent::SelectClip { target: a, additive: false });
    h.app.handle_event(AppEvent::SelectClip { target: b, additive: true });
    let scene = h.settle();
    assert_eq!(count(&h, &scene, vocab::FIELD_CHARACTER), 1, "2 つ選んでいても声の欄がある");
    let undo = h.app.cur.song_doc.undo_depth();
    h.app.handle_event(AppEvent::SetClipVoice {
        clips: vec![a, b],
        speaker_id: 1,
        singer_name: "ずんだもん".to_string(),
        style_name: "あまあま".to_string(),
    });
    let song = h.app.cur.song_doc.song();
    assert!([a, b].iter().all(|k| song.clip_by_key(*k).is_some_and(|c| c.speaker_id == 1)), "2 つとも声が変わる");
    assert_eq!(h.app.cur.song_doc.undo_depth(), undo + 1, "1 undo step");

    // 読み上げのスケール: 割れていれば「—」、書けば全部が揃う。
    let line = || TextEvent { text: "あ".to_string(), event_length_beats: 4.0, ..TextEvent::default() };
    let x = place(&mut h.app, t0, 16.0, ClipContent::Text(TextContent { events: vec![line()] }), "");
    let y = place(&mut h.app, t0, 24.0, ClipContent::Text(TextContent { events: vec![line()] }), "");
    let speed = daw_gui::app::TalkParamKind::Speed;
    h.app.handle_event(AppEvent::SetClipTalkParam { clips: vec![x], param: speed, value: 1.5 });
    h.app.handle_event(AppEvent::SelectClip { target: x, additive: false });
    h.app.handle_event(AppEvent::SelectClip { target: y, additive: true });
    let scene = h.settle();
    field_value(&h, &scene, vocab::FIELD_TALK_SPEED, "\u{2014}");
    h.app.handle_event(AppEvent::SetClipTalkParam { clips: vec![x, y], param: speed, value: 1.25 });
    let song = h.app.cur.song_doc.song();
    let speeds: Vec<f32> =
        [x, y].iter().map(|k| song.clip_by_key(*k).and_then(|c| c.talk).map_or(1.0, |t| t.speed_scale)).collect();
    assert_eq!(speeds, vec![1.25, 1.25]);
}

/// 窓に字幕の片が 1 つも見えていないテキストクリップを選んでも、編集バッファの resync は 1 回で落ち着く
/// (毎フレーム積み続けると再描画が止まらない)。
#[test]
fn text_clip_without_visible_lines_settles_its_edit_buffers() {
    let mut h = Harness::with_builtins();
    let t0 = h.track(0);
    h.add_builtin(t0, SUBTITLE_ID);
    let clip = place(&mut h.app, t0, 0.0, ClipContent::Text(TextContent { events: Vec::new() }), "");
    h.app.handle_event(AppEvent::SelectClip { target: clip, additive: false });
    let scene = h.settle();
    assert_eq!(h.app.cur.peph.clip_edit_buffer_target, Some(clip), "対象はそのクリップに揃う");
    // 書く先の片が無いので本文の欄は出さない (打っても確定で黙って消える欄を残さない)。
    assert_eq!(count(&h, &scene, vocab::FIELD_TEXT) + count(&h, &scene, vocab::FIELD_LINE), 0, "本文の欄が無い");
    let idle = PointerFrame { pos: Some((W as f32 - 5.0, H as f32 - 5.0)), ..PointerFrame::default() };
    let mut scene = Scene::new();
    let edits = h.host.frame_to_edits(&h.app, &mut scene, SCREEN, FrameInput { pointer: idle, ..FrameInput::default() }, |app, ui| {
        daw_gui::view::root::build_root(app, ui, SCREEN);
    });
    assert!(edits.is_empty(), "落ち着いたフレームは Edit を積まない ({} 件)", edits.len());
}

/// 種類の違うクリップを一緒に選ぶと、タイトルと帯の見出しは代表の種類を名乗らず「種類の違うクリップ」、帯の対象は
/// 種類ごとの内訳、代表の種類の欄の上には「… だけに効く」が出る。種類が揃っていれば今までどおり。
#[test]
fn mixed_kind_selection_does_not_claim_the_representative_kind() {
    let mut h = Harness::new(None);
    let t0 = h.track(0);
    let audio = place(&mut h.app, t0, 0.0, ClipContent::Audio(AudioContent::default()), "A");
    let midi = place(&mut h.app, t0, 8.0, ClipContent::Midi(MidiContent::default()), "M");
    h.app.handle_event(AppEvent::SelectClip { target: audio, additive: false });
    assert!(!title_of(&h.app).sub.contains(vocab::KIND_MIXED), "1 種類なら代表の種類");
    h.app.handle_event(AppEvent::SelectClip { target: midi, additive: true });
    let sel = h.app.inspector_selection();
    let band = sel.band.expect("選択の帯");
    assert!(band.mixed() && band.kind == ClipBandKind::Audio, "代表は先に始まる音声クリップ");
    assert!(title_of(&h.app).sub.starts_with(vocab::KIND_MIXED), "タイトルの種類の語: {}", title_of(&h.app).sub);
    let breakdown = h.app.inspector_clip_band_target(&sel);
    assert_eq!(breakdown, vocab::clip_kind_counts(&band.counts));
    let scene = h.settle();
    assert_eq!(count(&h, &scene, vocab::KIND_MIXED), 1, "帯の見出しも混在の語");
    assert!(find(&scene, 360.0, &breakdown).is_some(), "帯の対象は種類ごとの内訳: {breakdown}");
    let only = vocab::only_kind(ClipBandKind::Audio, 1, 2);
    assert!(find(&scene, 360.0, &only).is_some(), "音声の欄の上に `{only}`");
}

/// 離散値の欄 (逆再生 / 伸縮) もミュートと同じく選んでいる全部を畳む: 割れていればトグルは消灯・dropdown は空、
/// トグルを押すと全部をオンに揃える。
#[test]
fn split_discrete_values_fold_like_mute() {
    let mut h = Harness::new(None);
    let t0 = h.track(0);
    let ev = |reversed: bool, stretch_mode: common::model::StretchMode| AudioEvent {
        event_length_beats: 4.0,
        reversed,
        stretch_mode,
        ..AudioEvent::default()
    };
    let content = |e: AudioEvent| ClipContent::Audio(AudioContent { events: vec![e], ..AudioContent::default() });
    let a = place(&mut h.app, t0, 0.0, content(ev(true, common::model::StretchMode::Stretch)), "A");
    let b = place(&mut h.app, t0, 8.0, content(ev(false, common::model::StretchMode::Raw)), "B");
    h.app.handle_event(AppEvent::SelectClip { target: a, additive: false });
    h.app.handle_event(AppEvent::SelectClip { target: b, additive: true });
    let scene = h.settle();
    assert!(find(&scene, 360.0, "Stretch").is_none() && find(&scene, 360.0, "Raw").is_none(), "割れた伸縮は空");
    // 逆再生のトグル (代表 A は逆再生だが、割れているので消灯) を押す → 2 本とも逆再生。
    h.click(center(&one(&h, &scene, vocab::TOGGLE_REVERSE)));
    let reversed = |k: ClipKey| h.app.audio_first_event(k, |e| e.reversed);
    assert_eq!((reversed(a), reversed(b)), (Some(true), Some(true)), "割れていれば押すと全部オン");
}

/// 読み上げの「+ 字幕デバイス」は、クリップの居るトラックに足す (クリップを選んだまま別のトラックのヘッダを押して
/// いても、カーソルのトラックには足さない)。1 undo step。
#[test]
fn add_subtitle_device_goes_to_the_clip_track() {
    let mut h = Harness::with_builtins();
    let t0 = h.track(0);
    h.add_builtin(t0, BUILTIN_ID_VOICEVOX);
    let line = TextEvent { text: "あ".to_string(), event_length_beats: 4.0, ..TextEvent::default() };
    let clip = place(&mut h.app, t0, 0.0, ClipContent::Text(TextContent { events: vec![line] }), "");
    h.app.handle_event(AppEvent::SelectClip { target: clip, additive: false });
    h.app.cur.selection.selected_track_ids.clear();
    h.app.handle_event(AppEvent::AddInstrumentTrack);
    let t1 = h.app.cursor_track_id().expect("足したトラック");
    assert_eq!(h.app.inspector_target_refs(), vec![clip], "クリップの選択は残る");
    let scene = h.settle();
    let undo = h.app.cur.song_doc.undo_depth();
    h.click(center(&one(&h, &scene, vocab::ADD_SUBTITLE_DEVICE)));
    let song = h.app.cur.song_doc.song();
    let has = |id: u32| song.track_by_id(id).is_some_and(common::model::Track::has_subtitle_device);
    assert!(has(t0), "クリップの居るトラックに足す");
    assert!(!has(t1), "カーソルのトラックには足さない");
    assert_eq!(h.app.cur.song_doc.undo_depth(), undo + 1, "1 undo step");
}

/// クリップの帯の数値欄の変調 (◉ を待ち受けて欄をドラッグ) は、値の書き込み先と同じ代表クリップの居るトラックに付く
/// (クリップを選んだまま別のトラックのヘッダを押していても、カーソルのトラックには付かない)。
#[test]
fn clip_field_modulation_belongs_to_the_clip_track_not_the_cursor() {
    let mut h = Harness::new(None);
    let t0 = h.track(0);
    h.app.handle_event(AppEvent::AddInstrumentTrack);
    let t1 = h.app.cursor_track_id().expect("足したトラック");
    assert_ne!(t0, t1);
    let image = ImageEvent { x: 0.25, event_length_beats: 4.0, ..ImageEvent::default() };
    let clip = place(&mut h.app, t0, 0.0, ClipContent::Image(ImageContent { events: vec![image] }), "Img");
    h.app.handle_event(AppEvent::SelectClip { target: clip, additive: false });
    h.select(t1);
    assert_eq!(h.app.cursor_track_id(), Some(t1));
    h.app.handle_event(AppEvent::AddModSource { kind: ModSourceKindTag::Lfo });
    let sid = h.app.mod_source_display().last().expect("足した LFO").id;
    h.app.handle_event(AppEvent::SetArmedModSource(Some(sid)));
    let scene = h.settle();
    let at = center(&field_value(&h, &scene, vocab::FIELD_X, "0.250"));
    let frame = |h: &mut Harness, pos: (f32, f32), jp: bool, pressed: bool, jr: bool| {
        let p = PointerFrame {
            pos: Some(pos),
            primary_pressed: pressed,
            primary_just_pressed: jp,
            primary_just_released: jr,
            ..PointerFrame::default()
        };
        h.frame_with(p, false);
    };
    frame(&mut h, at, true, true, false);
    for i in 1..=5 {
        frame(&mut h, (at.0 + 10.0 * i as f32, at.1), false, true, false);
    }
    frame(&mut h, (at.0 + 50.0, at.1), false, false, true);
    let song = h.app.cur.song_doc.song();
    let x = common::model::AutomationTarget::ImageBuiltin(common::model::ImageBuiltinParam::X);
    let on = |id: u32| song.track_by_id(id).is_some_and(|t| t.mod_routings.iter().any(|r| r.target == x && r.source_id == sid));
    assert!(on(t0), "変調はクリップの居るトラックに付く");
    assert!(!on(t1), "カーソルのトラックには付かない");
}
