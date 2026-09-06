//! r.md #112: インスペクタの Parallel ヘッダ dropdown (Split) と chain 行の M / S ボタンを
//! **実ポインタ**で駆動する回帰網。
//!
//! 実機で「3 bands を選んだあと chain 行の M / S が click に反応しない」 が出た。 event 直叩きの
//! テストは全部通るので、 ここは `build_root` を合成 pointer で描き、 描かれた glyph の位置を
//! click して Song が変わることまで見る (dropdown の popup が orphan して不可視の modal 領域が
//! 残る、 の類はこの層でしか捕まらない)。

use std::sync::Arc;

use common::model::{ChainRef, Split};
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher};
use daw_gui::widgets::select_modifier::SelectModifier;
use daw_ui_core::{FrameInput, PointerFrame, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Primitive, Scene};

const W: u32 = 1400;
const H: u32 = 900;

fn build_app() -> (AppData, UnboundedReceiver<AudioCommand>, UnboundedReceiver<PluginCommand>) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let mut app = AppData::new(audio_tx, plugin_tx, None, None, event_dispatcher, job_dispatcher, None, None, 48_000);
    let visible: Vec<u32> = app.song_doc.song().tracks.iter().map(|t| t.id).collect();
    let tid = visible[0];
    app.apply_select_tracks(tid, SelectModifier::Single, &visible);
    app.handle_event(AppEvent::AddParallel { chain: ChainRef::Track(tid), index: 0 });
    (app, audio_rx, plugin_rx)
}

fn frame(host: &mut UiHost<AppData>, app: &mut AppData, p: PointerFrame) -> Scene {
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: W, height: H };
    let input = FrameInput { pointer: p, ..FrameInput::default() };
    host.frame(app, &mut scene, screen, input, |app, ui| {
        daw_gui::view::root::build_root(app, ui, screen);
    });
    scene
}

fn press(x: f32, y: f32) -> PointerFrame {
    PointerFrame { pos: Some((x, y)), primary_just_pressed: true, primary_pressed: true, ..PointerFrame::default() }
}

fn release(x: f32, y: f32) -> PointerFrame {
    PointerFrame { pos: Some((x, y)), primary_just_released: true, ..PointerFrame::default() }
}

fn hover(x: f32, y: f32) -> PointerFrame {
    PointerFrame { pos: Some((x, y)), ..PointerFrame::default() }
}

/// press → release の 1 click (release 後に idle frame を 1 つ流して edit を反映)。
fn click(host: &mut UiHost<AppData>, app: &mut AppData, (x, y): (f32, f32)) -> Scene {
    let _ = frame(host, app, hover(x, y));
    let _ = frame(host, app, press(x, y));
    let _ = frame(host, app, release(x, y));
    frame(host, app, hover(x, y))
}

/// base + popup pass の glyph で `text` に一致するものの中心 (左から順、 y 昇順)。
fn glyph_centers(scene: &Scene, text: &str) -> Vec<(f32, f32)> {
    let mut v: Vec<(f32, f32)> = scene
        .primitives
        .iter()
        .chain(scene.popup_primitives.iter())
        .filter_map(|p| match p {
            Primitive::Glyph(g) if &*g.text == text => Some((g.left + g.font_size * 0.35, g.top + g.line_height * 0.5)),
            _ => None,
        })
        .collect();
    v.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap().then(a.0.partial_cmp(&b.0).unwrap()));
    v
}

/// inspector (画面左カラム、 幅 280) の中の glyph だけ。
fn inspector_glyphs(scene: &Scene, text: &str) -> Vec<(f32, f32)> {
    glyph_centers(scene, text).into_iter().filter(|(x, _)| *x < 300.0).collect()
}

#[test]
fn chain_mute_button_still_works_after_picking_three_bands_from_the_dropdown() {
    let (mut app, _audio_rx, _plugin_rx) = build_app();
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    let parallel_id = app.song_doc.song().tracks[0].devices[0].id();

    // 分割前: chain 行の M が効く (基準)。
    let scene = frame(&mut host, &mut app, PointerFrame::default());
    let m = inspector_glyphs(&scene, "M");
    assert_eq!(m.len(), 1, "chain 1 本 = M ボタン 1 つ: {m:?}");
    click(&mut host, &mut app, m[0]);
    assert!(app.song_doc.song().parallel_by_id(parallel_id).unwrap().chains[0].muted, "分割前の M");
    click(&mut host, &mut app, m[0]);
    assert!(!app.song_doc.song().parallel_by_id(parallel_id).unwrap().chains[0].muted);

    // dropdown を開いて「3 bands」を選ぶ。
    let body = inspector_glyphs(&scene, "No split");
    assert_eq!(body.len(), 1, "{body:?}");
    let scene = click(&mut host, &mut app, body[0]);
    let item = glyph_centers(&scene, "3 bands");
    assert!(!item.is_empty(), "dropdown の一覧に 3 bands が出る");
    let scene = click(&mut host, &mut app, item[0]);
    let r = app.song_doc.song().parallel_by_id(parallel_id).unwrap();
    assert_eq!(r.split, Split::DEFAULT_FREQUENCY3);
    assert_eq!(r.chains.len(), 3);

    // 分割後: 3 本の chain 行の M がそれぞれ効く。
    let m = inspector_glyphs(&scene, "M");
    assert_eq!(m.len(), 3, "chain 3 本 = M ボタン 3 つ: {m:?}");
    for (k, pos) in m.iter().enumerate() {
        click(&mut host, &mut app, *pos);
        let r = app.song_doc.song().parallel_by_id(parallel_id).unwrap();
        assert!(r.chains[k].muted, "3 bands 後の chain {k} の M が効かない (pos {pos:?})");
    }
    // S も。
    let s = inspector_glyphs(&scene, "S");
    assert_eq!(s.len(), 3, "{s:?}");
    click(&mut host, &mut app, s[1]);
    assert!(app.song_doc.song().parallel_by_id(parallel_id).unwrap().chains[1].solo, "3 bands 後の S");
}

/// dropdown で split を選んだあとの流れをいくつか辿り、 どこで M が死ぬかを見る。
fn pick(host: &mut UiHost<AppData>, app: &mut AppData, body_label: &str, item_label: &str) -> Scene {
    let scene = frame(host, app, PointerFrame::default());
    let body = inspector_glyphs(&scene, body_label);
    assert_eq!(body.len(), 1, "{body_label}: {body:?}");
    let scene = click(host, app, body[0]);
    let item = glyph_centers(&scene, item_label);
    assert!(!item.is_empty(), "{item_label} が一覧に出る");
    click(host, app, item[0])
}

fn assert_first_mute_toggles(host: &mut UiHost<AppData>, app: &mut AppData, parallel_id: u64, ctx: &str) {
    let scene = frame(host, app, PointerFrame::default());
    let m = inspector_glyphs(&scene, "M");
    assert!(!m.is_empty(), "{ctx}: M が無い");
    let before = app.song_doc.song().parallel_by_id(parallel_id).unwrap().chains[0].muted;
    click(host, app, m[0]);
    let after = app.song_doc.song().parallel_by_id(parallel_id).unwrap().chains[0].muted;
    assert_ne!(before, after, "{ctx}: chain 1 の M が効かない (pos {:?})", m[0]);
}

#[test]
fn mute_works_after_switching_between_split_modes() {
    let (mut app, _a, _p) = build_app();
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    let parallel_id = app.song_doc.song().tracks[0].devices[0].id();
    pick(&mut host, &mut app, "No split", "3 bands");
    assert_first_mute_toggles(&mut host, &mut app, parallel_id, "3 bands");
    pick(&mut host, &mut app, "3 bands", "Mid/Side");
    assert_eq!(app.song_doc.song().parallel_by_id(parallel_id).unwrap().split, Split::MidSide);
    assert_first_mute_toggles(&mut host, &mut app, parallel_id, "Mid/Side");
    pick(&mut host, &mut app, "Mid/Side", "No split");
    assert_first_mute_toggles(&mut host, &mut app, parallel_id, "No split");
    pick(&mut host, &mut app, "No split", "3 bands");
    assert_first_mute_toggles(&mut host, &mut app, parallel_id, "3 bands again");
}

#[test]
fn mute_works_after_clicking_a_crossover_field() {
    let (mut app, _a, _p) = build_app();
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    let parallel_id = app.song_doc.song().tracks[0].devices[0].id();
    let scene = pick(&mut host, &mut app, "No split", "3 bands");
    // 周波数欄 (200 Hz) を click → text 入力モード。 その後 M。
    let f = inspector_glyphs(&scene, "200");
    assert!(!f.is_empty(), "周波数欄 200 が描かれる: {:?}", glyph_centers(&scene, "200 Hz"));
    click(&mut host, &mut app, f[0]);
    assert_first_mute_toggles(&mut host, &mut app, parallel_id, "周波数欄 click 後");
}

#[test]
fn mute_works_after_a_dropdown_is_dismissed_by_clicking_elsewhere() {
    let (mut app, _a, _p) = build_app();
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    let parallel_id = app.song_doc.song().tracks[0].devices[0].id();
    pick(&mut host, &mut app, "No split", "3 bands");
    // dropdown を開いて、 一覧の外 (chain 行の M) を click → 閉じるだけ。 次の click で M が効く。
    let scene = frame(&mut host, &mut app, PointerFrame::default());
    let body = inspector_glyphs(&scene, "3 bands");
    let scene = click(&mut host, &mut app, body[0]);
    let m = inspector_glyphs(&scene, "M");
    click(&mut host, &mut app, m[2]);
    assert_first_mute_toggles(&mut host, &mut app, parallel_id, "dropdown 外 click で閉じた後");
}
