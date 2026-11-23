//! r.md #158: トラックインスペクタ (左カラム、`view::track_inspector`) の見た目を撮る (目視用)。
//!
//! 1 つの曲 (MIDI / VOICEVOX 歌声 / オーディオ / 画像 + 映像 FX / グループ + Parallel / 読み上げ + 字幕 / 映像) を
//! 組み、状態ごとに選択・帯の開閉・Par の開閉・幅・テーマを変えてルート view を GPU でオフスクリーン描画し、
//! `target/inspector_shots/<名前>.png` に保存する。今の見た目は、スクロールしない 2 行のタイトル
//! (選んでいるもの + 色見本) の下に、見出しの帯 (選択 → 列 → 入力 → Rack → 口パク → モジュレーション) を
//! 積んだ 1 本の縦スクロール。クリップの設定 (音声 / 画像 / 歌声 / 読み上げ / 字幕 / ローンチ) は全部一番上の
//! 選択の帯に「ラベル左・値右」の欄で並び、Par にはデバイスそのものの設定だけが残る。Rack の行の直下に開く
//! Par / SC / 出力は行の名前の左端から始まり (行の背景と hover は見出しだけ)、欄の値は帯の欄と同じ列。OFF の
//! 行は「オフ」の札、読み込み失敗と出力 (パラアウト) は行の中。
//! 見た目の良し悪しは人が判断するので、ここで確かめるのは
//! 「PNG が書けた」ことだけ (インスペクタの文言に依存する assert を置くと、作り直すたびにこのカメラが壊れる)。
//! 各状態で期待した見出しが描かれたかは `--nocapture` の出力に出す (文言は `track_inspector::vocab`)。
//!
//! GPU adapter が無い環境では `OffscreenRenderer::new` が `Err` を返すので graceful skip。
//! daw_gui 本体は起動しない (AppData を直に組む)。

use std::sync::Arc;

use common::model::{
    AudioContent, AudioEvent, AudioSource, AudioSourcePath, ChainRef, Clip, ClipContent, ClipKey, Device,
    ImageContent, ImageEvent, ImageSource, ImageSourcePath, MASTER_TRACK_ID, MidiContent, NativeKind, Note,
    RackPanelKey, Split, StretchMode, TextContent, TextEvent, VideoContent,
};
use common::plugin_db::{BUILTIN_ID_VOICEVOX, PluginDatabase, SUBTITLE_ID, builtin_descriptors};
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent, LoadedDeviceInfo};
use daw_gui::device_addr::InsertAt;
use daw_gui::dispatcher::{BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher};
use daw_gui::event_device::DeviceEvent;
use daw_gui::event_inspector::InspectorEvent;
use daw_gui::event_launcher::{LauncherEvent, LauncherRow};
use daw_gui::state::inspector_view::{ClipBandKind, InspectorBand, InspectorDetail};
use daw_gui::view::track_inspector::vocab;
use daw_gui::voicevox_client::{VoiceVoxSinger, VoiceVoxStyle};
use daw_gui::widgets::select_modifier::SelectModifier;
use daw_ui_core::{FrameInput, PointerFrame, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{OffscreenRenderer, Primitive, Scene};

/// 実機に近い窓の大きさ。
const W: u32 = 1600;
const H: u32 = 1000;
const COLOR_GRADE_ID: &str = "builtin.video.color_grade";
const VIGNETTE_ID: &str = "builtin.video.vignette";

/// 組んだ曲の中の、撮るときに選ぶ / 開くもの。
struct Project {
    synth: u32,
    synth_clip: ClipKey,
    synth_eq: u64,
    synth_reverb: u64,
    vocal: u32,
    vocal_clip: ClipKey,
    voicevox: u64,
    audio: u32,
    audio_clip: ClipKey,
    image_clip: ClipKey,
    color_grade: u64,
    vignette: u64,
    image: u32,
    group: u32,
    /// グループの Parallel の High の chain の Reverb。
    group_reverb: u64,
    talk_clip: ClipKey,
    talk_voicevox: u64,
    video_clip: ClipKey,
}

struct Harness {
    app: AppData,
    host: UiHost<AppData>,
    project: Project,
    _audio: UnboundedReceiver<AudioCommand>,
    _plugin: UnboundedReceiver<PluginCommand>,
}

impl Harness {
    fn new() -> Self {
        let (audio_tx, audio_rx) = mpsc::unbounded_channel();
        let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
        // 内蔵 plugin (VOICEVOX / 映像 FX) の名前と port を picker と同じ DB から引く。
        let db = Arc::new(PluginDatabase::new(builtin_descriptors(), None, 0));
        let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
        let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
        let mut app =
            AppData::new(audio_tx, plugin_tx, None, Some(db), event_dispatcher, job_dispatcher, None, None, 48_000);
        // VOICEVOX トラックを作っても engine を spawn しない (テストから外部プロセスを起こさない)。
        app.voicevox.voicevox_launch_attempted = true;
        app.handle_event(AppEvent::SetTheme("dark".to_string()));
        let project = populate(&mut app);
        let mut host: UiHost<AppData> = UiHost::no_redraw();
        if host.set_palette(app.theme.core.clone()) {
            host.invalidate_scene_cache();
        }
        Self { app, host, project, _audio: audio_rx, _plugin: plugin_rx }
    }

    fn select_track(&mut self, id: u32) {
        if id == MASTER_TRACK_ID {
            self.app.cur.selection.selected_track_ids = vec![MASTER_TRACK_ID];
        } else {
            select_track(&mut self.app, id);
        }
    }

    fn select_clip(&mut self, target: ClipKey) {
        self.app.handle_event(AppEvent::SelectClip { target, additive: false });
        assert_eq!(self.app.cursor_track_id(), Some(target.track_id), "クリップを選ぶとそのトラックがカーソル");
    }

    fn open_panel(&mut self, device_id: u64) {
        self.app.handle_event(AppEvent::Device(DeviceEvent::ToggleRackPanel(RackPanelKey::Device(device_id))));
    }

    /// 選択中のトラックに `id` を足す (Ctrl+click と同じ)。
    fn add_track_to_selection(&mut self, id: u32) {
        let visible: Vec<u32> = self.app.cur.song_doc.song().tracks.iter().map(|t| t.id).collect();
        self.app.apply_select_tracks(id, SelectModifier::Toggle, &visible);
    }

    fn inspector(&mut self, ev: InspectorEvent) {
        self.app.handle_event(AppEvent::Inspector(ev));
    }

    fn set_theme(&mut self, id: &str) {
        self.app.handle_event(AppEvent::SetTheme(id.to_string()));
        if self.host.set_palette(self.app.theme.core.clone()) {
            self.host.invalidate_scene_cache();
        }
    }

    /// 今のインスペクタの幅 (隠していれば 0)。
    fn inspector_w(&self) -> f32 {
        daw_gui::view::track_inspector::panel_width(&self.app)
    }

    /// 数フレーム回して (インスペクタの本文高さの lag-by-one / edit buffer の resync を落ち着かせる) 最後の Scene。
    fn settle(&mut self) -> Scene {
        let screen = PhysicalSize { width: W, height: H };
        // ポインタはステータスバーの右端 (どの行にも hover しない)。
        let idle = || FrameInput {
            pointer: PointerFrame { pos: Some((W as f32 - 5.0, H as f32 - 5.0)), ..PointerFrame::default() },
            ..FrameInput::default()
        };
        let mut scene = Scene::new();
        for _ in 0..4 {
            scene = Scene::new();
            self.host.frame(&mut self.app, &mut scene, screen, idle(), |app, ui| {
                daw_gui::view::root::build_root(app, ui, screen);
            });
        }
        scene
    }
}

fn select_track(app: &mut AppData, id: u32) {
    let visible: Vec<u32> = app.cur.song_doc.song().tracks.iter().map(|t| t.id).collect();
    app.apply_select_tracks(id, SelectModifier::Single, &visible);
}

fn rename(app: &mut AppData, id: u32, name: &str) {
    let name = name.to_string();
    app.edit_song(move |song| {
        if let Some(t) = song.track_by_id_mut(id) {
            t.name = name;
        }
    });
}

/// 末尾にトラックを足す (選択が無いと末尾に入る)。
fn add_track(app: &mut AppData, name: &str) -> u32 {
    app.cur.selection.selected_track_ids.clear();
    app.handle_event(AppEvent::AddInstrumentTrack);
    let id = app.cursor_track_id().expect("足したトラックがカーソルになる");
    rename(app, id, name);
    id
}

fn place(app: &mut AppData, track: u32, start: f64, len: f64, content: ClipContent, name: &str) -> ClipKey {
    let name = name.to_string();
    let clip_id = app
        .edit_song(move |song| {
            let content_id = song.alloc_content(content, name);
            song.track_by_id_mut(track)
                .expect("track")
                .place_clip(Clip { start_beat: start, length_beats: len, content_id, ..Clip::default() })
        })
        .expect("edit_song");
    ClipKey { track_id: track, clip_id }
}

fn notes(pitches: &[u8], lyrics: Option<&[&str]>) -> ClipContent {
    let notes = pitches
        .iter()
        .enumerate()
        .map(|(i, &pitch)| Note {
            id: i as u32 + 1,
            start_beat: i as f64,
            duration_beats: 0.9,
            pitch,
            velocity: 100,
            lyric: lyrics.map(|l| l[i].to_string()),
            ..Note::default()
        })
        .collect();
    ClipContent::Midi(MidiContent { notes, next_note_id: pitches.len() as u32 + 1 })
}

/// 曲全体の device id (Parallel の chain も含む)。
fn all_device_ids(app: &AppData) -> Vec<u64> {
    fn walk(devices: &[Device], out: &mut Vec<u64>) {
        for d in devices {
            out.push(d.id());
            if let Device::Parallel(p) = d {
                for c in &p.chains {
                    out.push(c.id);
                    walk(&c.devices, out);
                }
            }
        }
    }
    let song = app.cur.song_doc.song();
    let mut out = Vec::new();
    for t in &song.tracks {
        walk(&t.devices, &mut out);
    }
    walk(&song.master_fx_chain, &mut out);
    out
}

/// `f` の前後で増えた device id (1 つ)。
fn added_device(app: &mut AppData, f: impl FnOnce(&mut AppData)) -> u64 {
    let before = all_device_ids(app);
    f(app);
    let after = all_device_ids(app);
    let new: Vec<u64> = after.into_iter().filter(|id| !before.contains(id)).collect();
    assert_eq!(new.len(), 1, "device が 1 つ増える: {new:?}");
    new[0]
}

fn add_native(app: &mut AppData, chain: ChainRef, kind: NativeKind) -> u64 {
    added_device(app, |app| {
        app.handle_event(AppEvent::Device(DeviceEvent::AddNative { chain, kind, open_panel: false }));
    })
}

/// picker から内蔵 plugin を足す (本番の「+ Plugin」と同じ経路)。
fn add_plugin(app: &mut AppData, track: u32, id: &str) -> u64 {
    select_track(app, track);
    let id = id.to_string();
    added_device(app, move |app| {
        app.handle_event(AppEvent::OpenPluginPicker { chain: None });
        app.handle_event(AppEvent::SelectPluginFromDb { id, keep_open: false, open_gui: false });
    })
}

fn populate(app: &mut AppData) -> Project {
    // 1. MIDI トラック (既定の 1 本目): 組み込みの Comp / EQ に、内蔵の Reverb / Delay を足す。
    let synth = app.cur.song_doc.song().tracks[0].id;
    rename(app, synth, "Synth");
    let synth_clip = place(app, synth, 0.0, 8.0, notes(&[60, 64, 67, 72, 67, 64, 60, 55], None), "Lead");
    let synth_reverb = add_native(app, ChainRef::Track(synth), NativeKind::Reverb);
    add_native(app, ChainRef::Track(synth), NativeKind::Delay);
    let synth_eq = app.cur.song_doc.song().builtin_native(synth, NativeKind::Eq).expect("組み込み EQ").id;

    // 3. VOICEVOX (歌声) トラック: 内蔵 VOICEVOX + 歌詞付きの MIDI クリップ。
    let vocal = add_track(app, "Vocal");
    let voicevox = add_plugin(app, vocal, BUILTIN_ID_VOICEVOX);
    app.cur.pipc.loaded_devices.insert(
        voicevox,
        LoadedDeviceInfo {
            plugin_id_str: BUILTIN_ID_VOICEVOX.to_string(),
            token: common::protocol::InstanceToken(1),
        },
    );
    let vocal_clip = place(
        app,
        vocal,
        0.0,
        8.0,
        notes(&[60, 62, 64, 65, 67, 69, 71, 72], Some(&["ど", "れ", "み", "ふぁ", "そ", "ら", "し", "ど"])),
        "Verse",
    );
    let singer = |name: &str, styles: &[(u32, &str)]| VoiceVoxSinger {
        name: name.to_string(),
        styles: styles.iter().map(|&(id, n)| VoiceVoxStyle { id, name: n.to_string() }).collect(),
    };
    app.handle_event(AppEvent::SingersLoaded(vec![
        singer(common::voicevox::DEFAULT_SINGER_NAME, &[(common::voicevox::DEFAULT_SINGER_ID, "ノーマル")]),
        singer("ずんだもん", &[(3003, "ノーマル"), (3001, "あまあま")]),
    ]));

    // 2. オーディオトラック: 8 拍のオーディオクリップ (ファイルは無い = 波形は出ない)。
    let audio = add_track(app, "Audio");
    app.edit_song(|song| {
        song.media.audio_sources.insert(
            1,
            AudioSource {
                path: AudioSourcePath::Absolute("C:/inspector-shots/drums.wav".into()),
                sample_rate: 48_000,
                channels: 2,
                frames: 384_000,
                original_bpm: None,
                root_key: None,
            },
        );
    });
    let event = AudioEvent {
        id: 1,
        source_id: 1,
        event_length_beats: 8.0,
        source_start_frames: 0,
        source_end_frames: 384_000,
        stretch_mode: StretchMode::Repitch,
        ..AudioEvent::default()
    };
    let audio_clip =
        place(app, audio, 0.0, 8.0, ClipContent::Audio(AudioContent { events: vec![event], next_event_id: 2 }), "Drums");

    // 4. 画像トラック: 画像クリップ + 映像 FX (Color Grade / Vignette)。
    let image = add_track(app, "Image");
    app.edit_song(|song| {
        song.media.image_sources.insert(
            1,
            ImageSource {
                path: ImageSourcePath::Absolute("C:/inspector-shots/logo.png".into()),
                name: "logo.png".to_string(),
                width: 1920,
                height: 1080,
                format: "Png".to_string(),
            },
        );
    });
    let image_event =
        ImageEvent { source_id: 1, event_length_beats: 8.0, x: 0.6, y: 0.05, w: 0.35, h: 0.35, ..ImageEvent::default() };
    let image_clip = place(app, image, 0.0, 8.0, ClipContent::Image(ImageContent { events: vec![image_event] }), "Logo");
    let color_grade = add_plugin(app, image, COLOR_GRADE_ID);
    let vignette = add_plugin(app, image, VIGNETTE_ID);

    // 5. グループ (バス): 2 本を束ね、グループに 3 帯域分割の Parallel (Low: Comp / Mid: EQ / High: Reverb)。
    let gtr_l = add_track(app, "Gtr L");
    let gtr_r = add_track(app, "Gtr R");
    app.handle_event(AppEvent::GroupSelectedTracks { track_ids: vec![gtr_l, gtr_r] });
    let group = app.cur.song_doc.song().track_by_id(gtr_l).and_then(|t| t.parent_group_id).expect("グループができる");
    rename(app, group, "Bus");
    select_track(app, group);
    app.handle_event(AppEvent::Device(DeviceEvent::AddParallel { chain: ChainRef::Track(group), at: InsertAt::Default }));
    let parallel_id = parallel_of(app, group).id;
    for _ in 0..2 {
        app.handle_event(AppEvent::Device(DeviceEvent::AddParallelChain { parallel_id }));
    }
    app.handle_event(AppEvent::Device(DeviceEvent::SetParallelSplit { parallel_id, split: Split::DEFAULT_FREQUENCY3 }));
    let chains: Vec<u64> = parallel_of(app, group).chains.iter().map(|c| c.id).collect();
    let mut group_reverb = 0;
    for (chain, kind) in chains.into_iter().zip([NativeKind::Comp, NativeKind::Eq, NativeKind::Reverb]) {
        group_reverb = add_native(app, ChainRef::Chain(chain), kind);
    }

    // 6. 読み上げ + 字幕トラック: 内蔵 VOICEVOX + 字幕デバイス + Text クリップ (セリフ = 字幕の文字)。
    let talk = add_track(app, "Talk");
    let talk_voicevox = add_plugin(app, talk, BUILTIN_ID_VOICEVOX);
    app.cur.pipc.loaded_devices.insert(
        talk_voicevox,
        LoadedDeviceInfo {
            plugin_id_str: BUILTIN_ID_VOICEVOX.to_string(),
            token: common::protocol::InstanceToken(2),
        },
    );
    add_plugin(app, talk, SUBTITLE_ID);
    let line = TextEvent { text: "こんにちは".to_string(), event_length_beats: 4.0, ..TextEvent::default() };
    let talk_clip = place(app, talk, 0.0, 4.0, ClipContent::Text(TextContent { events: vec![line] }), "");
    app.handle_event(AppEvent::SpeakersLoaded(vec![
        singer("四国めたん", &[(2, "ノーマル"), (0, "あまあま")]),
        singer("ずんだもん", &[(3, "ノーマル"), (1, "あまあま")]),
    ]));

    // 7. 映像トラック: 映像クリップ (ファイルは無い = 中身の event も無い)。
    let video = add_track(app, "Video");
    let video_clip = place(app, video, 0.0, 8.0, ClipContent::Video(VideoContent::default()), "Intro");

    Project {
        synth,
        synth_clip,
        synth_eq,
        synth_reverb,
        vocal,
        vocal_clip,
        voicevox,
        audio,
        audio_clip,
        image_clip,
        color_grade,
        vignette,
        image,
        group,
        group_reverb,
        talk_clip,
        talk_voicevox,
        video_clip,
    }
}

fn parallel_of(app: &AppData, track: u32) -> common::model::Parallel {
    app.cur
        .song_doc
        .song()
        .track_by_id(track)
        .and_then(|t| {
            t.devices.iter().find_map(|d| match d {
                Device::Parallel(p) => Some(p.clone()),
                _ => None,
            })
        })
        .expect("Parallel")
}

/// インスペクタ (左カラム、幅 `inspector_w`) に描かれた文字列。
fn inspector_texts(scene: &Scene, inspector_w: f32) -> Vec<String> {
    scene
        .primitives
        .iter()
        .filter_map(|p| match p {
            Primitive::Glyph(g) if g.left < inspector_w => Some(g.text.to_string()),
            _ => None,
        })
        .collect()
}

/// 1 状態を撮る: 曲を組み直し、`setup` で選択 / 帯 / 幅を変え、落ち着かせてから保存する。
fn shot(renderer: &mut OffscreenRenderer, name: &str, expect: &[&str], setup: impl FnOnce(&mut Harness)) {
    let mut h = Harness::new();
    setup(&mut h);
    let scene = h.settle();
    let w = h.inspector_w();
    save(renderer, &scene, name, expect, w);
}

fn save(renderer: &mut OffscreenRenderer, scene: &Scene, name: &str, expect: &[&str], inspector_w: f32) {
    let texts = inspector_texts(scene, inspector_w);
    let missing: Vec<&str> = expect.iter().copied().filter(|e| !texts.iter().any(|t| t == e)).collect();
    let rgba = renderer.render_to_rgba(scene).expect("offscreen render");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/inspector_shots");
    std::fs::create_dir_all(&dir).expect("保存先を作る");
    let path = dir.join(format!("{name}.png"));
    image::save_buffer(&path, &rgba, W, H, image::ColorType::Rgba8).expect("PNG を書く");
    assert!(path.is_file(), "{} が書けた", path.display());
    // インスペクタの列だけを等倍で切り出した版 (細部の目視用。全体図は縮小されて文字が潰れる)。
    if inspector_w >= 1.0 {
        let full = image::RgbaImage::from_raw(W, H, rgba).expect("RGBA の大きさ");
        let crop_w = (inspector_w as u32 + 8).min(W);
        let top = 60;
        let crop = image::imageops::crop_imm(&full, 0, top, crop_w, H - top).to_image();
        crop.save(dir.join(format!("{name}_inspector.png"))).expect("切り出しを書く");
    }
    eprintln!("[inspector_visual] {} (期待した見出しで描かれていないもの: {missing:?})", path.display());
}

#[test]
fn capture_track_inspector_states() {
    let Ok(mut renderer) = OffscreenRenderer::new(W, H) else {
        eprintln!("GPU adapter が無いので skip");
        return;
    };

    let r = &mut renderer;
    let (rack, modulation, input) = (vocab::BAND_RACK, vocab::BAND_MODULATION, vocab::BAND_INPUT);
    let audio_band = vocab::clip_band_title(ClipBandKind::Audio);

    // 段 1 (r.md #158 骨格): タイトル + 帯。
    // 1. MIDI トラック (チェーン: Reverb / Delay / 組み込み Comp / EQ)。Par はすべて閉じている。
    shot(r, "s1_1_midi_track", &["Synth", input, rack, modulation, "Reverb", "Delay", "Comp", "EQ"], |h| {
        h.select_track(h.project.synth);
    });
    // 3. 3 トラックの選択: タイトル「3 トラック」、入力は「選択中の 3 トラック」、Rack 以下は「… のみ」。
    shot(r, "s1_3_multi_tracks", &["3 トラック", "選択中の 3 トラック", "Audio のみ"], |h| {
        h.select_track(h.project.synth);
        h.add_track_to_selection(h.project.vocal);
        h.add_track_to_selection(h.project.audio);
    });
    // 4. Rack とモジュレーションを畳んだ (見出しだけ残る)。
    shot(r, "s1_4_bands_collapsed", &[rack, modulation], |h| {
        h.select_track(h.project.synth);
        h.inspector(InspectorEvent::ToggleBand(InspectorBand::Rack));
        h.inspector(InspectorEvent::ToggleBand(InspectorBand::Modulation));
    });
    // 5. 幅 520 (右端のドラッグと同じ経路)。オーディオクリップと Par を開いた EQ。
    shot(r, "s1_5_width_520", &[audio_band, "HP"], |h| {
        h.select_clip(h.project.audio_clip);
        h.inspector(InspectorEvent::SetWidth { w: 520.0, commit: true });
        let eq = h.app.cur.song_doc.song().builtin_native(h.project.audio, NativeKind::Eq).expect("組み込み EQ").id;
        h.open_panel(eq);
    });
    // 6. 隠した (右の領域がインスペクタの幅ぶん広がる)。
    shot(r, "s1_6_hidden", &[], |h| {
        h.select_track(h.project.synth);
        h.inspector(InspectorEvent::ToggleVisible);
    });
    // 7. ライトテーマ (見出しの面と本文のコントラスト)。
    shot(r, "s1_7_light_theme", &[audio_band, rack], |h| {
        h.set_theme("light");
        h.select_clip(h.project.audio_clip);
    });
    // 16. ランチャーのセルで MIDI 割り当ての詳細を開いた。
    shot(r, "s1_16_launcher_cell_midi_open", &["セル", "割り当てを消す"], |h| {
        let row = LauncherRow::Track(h.project.synth);
        h.app.handle_event(AppEvent::Launcher(LauncherEvent::CreateCell { row, scene_index: 0 }));
        h.inspector(InspectorEvent::ToggleDetail(InspectorDetail::LaunchMidi));
    });

    // 段 2 (r.md #158 クリップの帯への集約): クリップの設定は全部クリップの帯、Par からは消えた。
    // 1. オーディオクリップ: 名前 / 再生 (ミュート・逆再生) / 伸縮 / ゲイン… / フェード。
    shot(
        r,
        "s2_1_audio_clip",
        &["Drums", audio_band, vocab::FIELD_NAME, vocab::TOGGLE_REVERSE, vocab::FIELD_GAIN, vocab::FIELD_FADE_OUT, input],
        |h| h.select_clip(h.project.audio_clip),
    );
    // 2. 歌声のクリップ: 小見出し「歌声」の下にキャラ / スタイル。
    shot(r, "s2_2_vocal_clip", &["Verse", vocab::SUB_VOICE, vocab::FIELD_CHARACTER, vocab::FIELD_STYLE], |h| {
        h.select_clip(h.project.vocal_clip);
    });
    // 3. 同じクリップで VOICEVOX の Par を開く: Par には口パク先だけ (声はクリップの帯)。
    shot(r, "s2_3_vocal_par_open", &[vocab::SUB_VOICE, vocab::FIELD_LIPSYNC_TARGET], |h| {
        h.select_clip(h.project.vocal_clip);
        h.open_panel(h.project.voicevox);
    });
    // 4. 読み上げ + 字幕の Text クリップ: セリフ 1 本、読み上げ (話者 / スケール)、字幕 (詳細は畳む)。
    shot(
        r,
        "s2_4_talk_clip",
        &[vocab::FIELD_LINE, vocab::SUB_TALK, vocab::SUB_SUBTITLE, "話速", vocab::DETAIL_SUBTITLE_STYLE],
        |h| {
            h.select_clip(h.project.talk_clip);
            h.open_panel(h.project.talk_voicevox);
        },
    );
    // 5. 字幕の細かい設定を開いた (塗り / 縁 / 影)。
    shot(r, "s2_5_subtitle_details_open", &[vocab::SUB_FILL, vocab::SUB_OUTLINE, vocab::SUB_SHADOW], |h| {
        h.select_clip(h.project.talk_clip);
        h.inspector(InspectorEvent::ToggleDetail(InspectorDetail::SubtitleStyle));
        h.inspector(InspectorEvent::ToggleBand(InspectorBand::Rack));
        h.inspector(InspectorEvent::ToggleBand(InspectorBand::Input));
    });
    // 6. 画像クリップ (A つきの欄、反転) + Color Grade の Par。
    shot(
        r,
        "s2_6_image_clip",
        &[vocab::clip_band_title(ClipBandKind::Image), vocab::FIELD_OPACITY, vocab::FIELD_FLIP, "Color Grade", "Exposure"],
        |h| {
            h.select_clip(h.project.image_clip);
            h.open_panel(h.project.color_grade);
        },
    );
    // 7. MIDI クリップ (Synth の Lead): 共通の欄 (名前 / 再生) だけ。
    shot(r, "s2_7_midi_clip", &["Lead", vocab::clip_band_title(ClipBandKind::Midi), vocab::FIELD_NAME, vocab::TOGGLE_MUTE], |h| {
        h.select_clip(h.project.synth_clip);
    });
    // 8. 映像クリップ: 共通の欄だけ。
    shot(r, "s2_8_video_clip", &["Intro", vocab::clip_band_title(ClipBandKind::Video), vocab::FIELD_PLAYBACK], |h| {
        h.select_clip(h.project.video_clip);
    });
    // 9. ランチャーのセル: クリップの帯に共通の欄と小見出し「ローンチ」、MIDI 割り当ては詳細で畳まれている。
    shot(r, "s2_9_launcher_cell", &[vocab::FIELD_PLAYBACK, vocab::SUB_LAUNCH, "フォローアクション"], |h| {
        let row = LauncherRow::Track(h.project.synth);
        h.app.handle_event(AppEvent::Launcher(LauncherEvent::CreateCell { row, scene_index: 0 }));
    });
    // 10. 列 (シーン) の見出しを選んだ: 列の帯 (対象 = 列名) にフォローアクション。
    shot(r, "s2_10_scene_column", &[vocab::BAND_SCENE, "フォローアクション"], |h| {
        // 既定の曲は列を持たないので 1 本足す。
        h.app.handle_event(AppEvent::Launcher(LauncherEvent::AddScene));
        let scene_id = h.app.cur.song_doc.song().scenes.first().expect("足した列").id;
        h.app.handle_event(AppEvent::Launcher(LauncherEvent::SelectScene { scene_id, modifier: SelectModifier::Single }));
    });
    // 11. 画像トラック (映像 FX): 入力の帯は出ない (録れる入力が無い。グループのバスは s3_2)。
    shot(r, "s2_11_image_track", &[rack, "Color Grade"], |h| h.select_track(h.project.image));

    // 段 3 (r.md #158 中身の揃え): Par は行の名前の左端から (背景・hover は見出しだけ)、欄の値は帯と同じ列、
    // OFF は「オフ」の札、+ チェーン / + プラグイン の主従、読み込み失敗と出力は行の中。
    // 1. MIDI トラック: EQ と Reverb の Par、組み込み Comp の SC パネル (OFF の札)。
    shot(r, "s3_1_rack_par_open", &["HP", "ROOM", &vocab::sc_port(1), vocab::OFF_TAG, vocab::ADD_PLUGIN], |h| {
        h.select_track(h.project.synth);
        h.open_panel(h.project.synth_eq);
        h.open_panel(h.project.synth_reverb);
        let comp = h.app.cur.song_doc.song().builtin_native(h.project.synth, NativeKind::Comp).expect("組み込み Comp").id;
        h.app.cur.peph.open_sidechain_panel = Some(comp);
    });
    // 2. グループ (バス) トラック: 3 帯域分割の Parallel。chain の中の「+ プラグイン」(小さく device の段) と
    //    「+ チェーン」(chain の段)。High の Reverb はバイパス (chain の中の札)。入力の帯は出ない。
    shot(r, "s3_2_group_parallel", &["Bus", "Parallel", "Low", "Mid", "High", vocab::ADD_CHAIN, vocab::OFF_TAG], |h| {
        h.select_track(h.project.group);
        let reverb = h.project.group_reverb;
        h.app.handle_event(AppEvent::Device(DeviceEvent::SetDevicesBypassed { device_ids: vec![reverb], bypassed: true }));
    });
    // 3. マスター: 「+ エフェクト」「フェーダー後」、OFF の Limiter は札。
    shot(r, "s3_3_master", &["Bus Comp", "Tone EQ", vocab::ADD_EFFECT, vocab::POST_FADER, "Limiter", vocab::OFF_TAG], |h| {
        h.select_track(MASTER_TRACK_ID);
    });
    // 4. 画像トラック: Color Grade の Par (効果の名前の見出しを繰り返さない)、Vignette はバイパス (plugin 行の札)。
    shot(r, "s3_4_image_fx_par", &["Color Grade", "Exposure", vocab::OFF_TAG], |h| {
        h.select_track(h.project.image);
        h.open_panel(h.project.color_grade);
        let vignette = h.project.vignette;
        h.app.handle_event(AppEvent::Device(DeviceEvent::SetDevicesBypassed { device_ids: vec![vignette], bypassed: true }));
    });
    // 5. 出力 (パラアウト): aux 出力 2 本の VOICEVOX を展開し、行の Out▴ で直下に開いたパネル + Par の口パク先。
    shot(r, "s3_5_aux_out_open", &[vocab::FIELD_CHILD_TRACKS, &vocab::aux_out_port(2), vocab::FIELD_LIPSYNC_TARGET], |h| {
        let vv = h.project.voicevox;
        h.app.edit_song(|song| {
            song.plugin_by_id_mut(vv).expect("VOICEVOX").aux_output_count = 2;
        });
        h.select_track(h.project.vocal);
        h.app.handle_event(AppEvent::Device(DeviceEvent::ExplodeParallelOut { device_id: vv }));
        h.select_track(h.project.vocal);
        h.app.cur.peph.open_aux_out_panel = Some(vv);
        h.open_panel(vv);
    });
    // 6. 読み込みに失敗した plugin: 行の [再読込] と直下の理由 1 行 (別セクションに出さない)。
    shot(r, "s3_6_failed_load", &[vocab::RELOAD, "shmem create failed"], |h| {
        h.select_track(h.project.image);
        h.app.cur.pipc.failed_plugin_loads.insert(h.project.vignette, "shmem create failed".to_string());
    });
    // 8. モジュレーション: LFO 2 つのうち 1 つをバイパス (メーターの位置に札、名前は text_dim)。Rack は畳む。
    shot(r, "s3_8_modulation_off", &[modulation, "LFO", vocab::OFF_TAG], |h| {
        h.select_track(h.project.synth);
        for _ in 0..2 {
            h.app.handle_event(AppEvent::AddModSource { kind: daw_gui::app::ModSourceKindTag::Lfo });
        }
        let ids: Vec<u32> = h.app.mod_source_display().iter().map(|s| s.id).collect();
        h.app.handle_event(AppEvent::SetModSourceEnabled { id: ids[1], enabled: false });
        h.app.cur.peph.expanded_mod_sources.insert(ids[0]);
        h.inspector(InspectorEvent::ToggleBand(InspectorBand::Rack));
    });
    // 7. ライトテーマ: Par の面 / 行の見出し / 札のコントラスト。
    shot(r, "s3_7_light_theme_rack", &["HP", vocab::OFF_TAG], |h| {
        h.set_theme("light");
        h.select_track(h.project.synth);
        h.open_panel(h.project.synth_eq);
    });
}
