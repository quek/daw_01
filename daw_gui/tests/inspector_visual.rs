//! r.md #158 の準備: トラックインスペクタ (左カラム、`view::track_inspector`) の **今の見た目** を撮る (目視用)。
//!
//! 1 つの曲 (MIDI / VOICEVOX / オーディオ / 画像 + 映像 FX / グループ + Parallel の 6 トラック) を組み、
//! 状態ごとに選択と Par の開閉を変えてルート view を GPU でオフスクリーン描画し、
//! `target/inspector_shots/<名前>.png` に保存する。見た目の良し悪しは人が判断するので、ここで確かめるのは
//! 「PNG が書けた」ことだけ (インスペクタの文言に依存する assert を置くと、作り直すたびにこのカメラが壊れる)。
//! 各状態で期待した見出しが描かれたかは `--nocapture` の出力に出す。
//!
//! GPU adapter が無い環境では `OffscreenRenderer::new` が `Err` を返すので graceful skip。
//! daw_gui 本体は起動しない (AppData を直に組む)。

use std::sync::Arc;

use common::model::{
    AudioContent, AudioEvent, AudioSource, AudioSourcePath, ChainRef, Clip, ClipContent, ClipKey, Device,
    ImageContent, ImageEvent, ImageSource, ImageSourcePath, MASTER_TRACK_ID, MidiContent, NativeKind, Note,
    RackPanelKey, Split, StretchMode,
};
use common::plugin_db::{BUILTIN_ID_VOICEVOX, PluginDatabase, builtin_descriptors};
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent, LoadedDeviceInfo};
use daw_gui::device_addr::InsertAt;
use daw_gui::dispatcher::{BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher};
use daw_gui::event_device::DeviceEvent;
use daw_gui::view::root::INSPECTOR_W;
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
    synth_eq: u64,
    synth_reverb: u64,
    vocal: u32,
    vocal_clip: ClipKey,
    voicevox: u64,
    audio_clip: ClipKey,
    image_clip: ClipKey,
    color_grade: u64,
    group: u32,
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
    place(app, synth, 0.0, 8.0, notes(&[60, 64, 67, 72, 67, 64, 60, 55], None), "Lead");
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
    add_plugin(app, image, VIGNETTE_ID);

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
    for (chain, kind) in chains.into_iter().zip([NativeKind::Comp, NativeKind::Eq, NativeKind::Reverb]) {
        add_native(app, ChainRef::Chain(chain), kind);
    }

    Project { synth, synth_eq, synth_reverb, vocal, vocal_clip, voicevox, audio_clip, image_clip, color_grade, group }
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

/// インスペクタ (左カラム) に描かれた文字列。
fn inspector_texts(scene: &Scene) -> Vec<String> {
    scene
        .primitives
        .iter()
        .filter_map(|p| match p {
            Primitive::Glyph(g) if g.left < INSPECTOR_W => Some(g.text.to_string()),
            _ => None,
        })
        .collect()
}

fn save(renderer: &mut OffscreenRenderer, scene: &Scene, name: &str, expect: &[&str]) {
    let texts = inspector_texts(scene);
    let missing: Vec<&str> = expect.iter().copied().filter(|e| !texts.iter().any(|t| t == e)).collect();
    let rgba = renderer.render_to_rgba(scene).expect("offscreen render");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/inspector_shots");
    std::fs::create_dir_all(&dir).expect("保存先を作る");
    let path = dir.join(format!("{name}.png"));
    image::save_buffer(&path, &rgba, W, H, image::ColorType::Rgba8).expect("PNG を書く");
    assert!(path.is_file(), "{} が書けた", path.display());
    eprintln!("[inspector_visual] {} (期待した見出しで描かれていないもの: {missing:?})", path.display());
}

#[test]
fn capture_track_inspector_states() {
    let Ok(mut renderer) = OffscreenRenderer::new(W, H) else {
        eprintln!("GPU adapter が無いので skip");
        return;
    };

    // 1. MIDI トラック (チェーン: Reverb / Delay / 組み込み Comp / EQ)。Par はすべて閉じている。
    let mut h = Harness::new();
    h.select_track(h.project.synth);
    let scene = h.settle();
    save(&mut renderer, &scene, "1_midi_track", &["Synth", "Reverb", "Delay", "Comp", "EQ"]);

    // 1b. 同じ MIDI トラックで、組み込み EQ と Reverb の Par を開いた状態。
    let mut h = Harness::new();
    h.select_track(h.project.synth);
    h.open_panel(h.project.synth_eq);
    h.open_panel(h.project.synth_reverb);
    let scene = h.settle();
    save(&mut renderer, &scene, "1b_midi_track_par_open", &["HP", "LMF"]);

    // 2. オーディオトラックのクリップを選んだ状態 (Audio Event セクション)。
    let mut h = Harness::new();
    h.select_clip(h.project.audio_clip);
    let scene = h.settle();
    save(&mut renderer, &scene, "2_audio_clip", &["Audio Event"]);

    // 3. VOICEVOX トラック: 歌詞付きクリップを選び、VOICEVOX の Par (Clip Voice) を開く。
    let mut h = Harness::new();
    h.select_clip(h.project.vocal_clip);
    h.open_panel(h.project.voicevox);
    let scene = h.settle();
    assert_eq!(h.app.cursor_track_id(), Some(h.project.vocal));
    save(&mut renderer, &scene, "3_voicevox_track", &["Clip Voice"]);

    // 4. 画像トラック: 画像クリップを選び (Image Event)、Color Grade の Par (映像 FX の param) を開く。
    let mut h = Harness::new();
    h.select_clip(h.project.image_clip);
    h.open_panel(h.project.color_grade);
    let scene = h.settle();
    save(&mut renderer, &scene, "4_image_video_fx", &["Image Event", "Color Grade", "Exposure"]);

    // 5. グループ (バス) トラック: 3 帯域分割の Parallel。
    let mut h = Harness::new();
    h.select_track(h.project.group);
    let scene = h.settle();
    save(&mut renderer, &scene, "5_group_parallel", &["Bus", "Parallel", "Low", "Mid", "High"]);

    // 6. マスター。
    let mut h = Harness::new();
    h.select_track(MASTER_TRACK_ID);
    let scene = h.settle();
    save(&mut renderer, &scene, "6_master", &["Bus Comp", "Tone EQ", "Post-Fader", "Limiter"]);
}
