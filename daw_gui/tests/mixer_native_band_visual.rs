//! M-1: Mixer 帯 (組み込み Comp / EQ、`docs/plan_rack_native_devices.md` §10.8 / §15.6) の描画 regression。
//!
//! レイアウトと色は build / test / clippy を全部すり抜けて壊れる (CLAUDE.md
//! 「Visual regression smoke test」)。ルート view を 1 フレーム組み、Scene の実描画で次を固定する:
//!
//! 1. 常設サムネイル帯の **EQ カーブがパラメーターに追従して曲がる**。OFF にしても形は保ち、線だけ薄くなる
//! 2. カーブが帯の背景に **沈まない** (dark / light 両方で実描画してコントラストを測る)
//! 3. セクションを開くと **既存 strip がその分だけ下がる** (帯の高さと描画位置が同じ SSoT から来ている)
//! 4. 組み込み EQ を Comp の前へ並べ替えると **セクションの上下も入れ替わる** (Q16)
//! 5. 追加の「Comp 2」は帯に出ず、GR も **組み込みの id でだけ** 引く (Q16)

use std::sync::Arc;

use common::model::{ChainRef, CompParam, EqBand, EqParam, NativeKind, NativeParamId};
use common::protocol::PluginCommand;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent, InsertAt, RelocateDevices};
use daw_gui::dispatcher::{BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher};
use daw_gui::event::StripSection;
use daw_gui::event_device::DeviceEvent;
use daw_gui::event_native::NativeEdit;
use daw_ui_core::{FrameInput, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{GlyphArea, LineBatch, OffscreenRenderer, Scene};

const W: u32 = 960;
const H: u32 = 900;
/// `mixer_strips.rs` の strip 幅 (private なのでテスト側に写す。strip 背景の目印に使う)。
const STRIP_WIDTH: f32 = 80.0;

fn build_app(theme: &str) -> AppData {
    let (audio_tx, _audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx): (_, UnboundedReceiver<PluginCommand>) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let mut app = AppData::new(audio_tx, plugin_tx, None, None, event_dispatcher, job_dispatcher, None, None, 48_000);
    std::mem::forget(plugin_rx);
    app.handle_event(AppEvent::SetTheme(theme.to_string()));
    app
}

fn first_track(app: &AppData) -> u32 {
    app.cur.song_doc.song().tracks[0].id
}

fn builtin(app: &AppData, kind: NativeKind) -> u64 {
    app.cur.song_doc.song().builtin_native(first_track(app), kind).expect("組み込み device").id
}

/// 1 トラック目の組み込み EQ を「はっきり曲がる」設定にする (HMF を +15dB / 狭い Q)。
/// 値に触れると device は自動で ON になる (`NativeEdit::apply`)。
fn app_with_curved_eq(theme: &str) -> AppData {
    let mut app = build_app(theme);
    let device_id = builtin(&app, NativeKind::Eq);
    app.handle_event(AppEvent::Device(DeviceEvent::NativeEdit {
        device_id,
        edit: NativeEdit::Params(vec![
            (NativeParamId::Eq { band: EqBand::Hmf, param: EqParam::Gain }, 15.0),
            (NativeParamId::Eq { band: EqBand::Hmf, param: EqParam::Q }, 3.0),
        ]),
    }));
    app
}

fn frame(app: &AppData, scene: &mut Scene) {
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    if host.set_palette(app.theme.core.clone()) {
        host.invalidate_scene_cache();
    }
    let screen = PhysicalSize { width: W, height: H };
    host.frame_to_edits(app, scene, screen, FrameInput::default(), |app, ui| {
        daw_gui::view::root::build_root(app, ui, screen);
    });
}

fn scene_of(app: &AppData) -> Scene {
    let mut scene = Scene::new();
    frame(app, &mut scene);
    scene
}

#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn px(rgba: &[u8], x: f32, y: f32) -> [u8; 3] {
    let (xi, yi) = (x.round().max(0.0) as u32, y.round().max(0.0) as u32);
    let i = ((yi.min(H - 1) * W + xi.min(W - 1)) * 4) as usize;
    [rgba[i], rgba[i + 1], rgba[i + 2]]
}

/// 2 色の知覚差 (チャンネル差の最大値)。同色なら 0。
fn diff(a: [u8; 3], b: [u8; 3]) -> u8 {
    (0..3).map(|i| a[i].abs_diff(b[i])).max().unwrap_or(0)
}

/// 一番左の strip 背景 `(x, y)`。
fn first_strip(scene: &Scene) -> (f32, f32) {
    let mut v: Vec<(f32, f32)> = scene
        .iter_rects()
        .filter(|r| (r.rect.w - STRIP_WIDTH).abs() < 0.01 && r.rect.h > 100.0)
        .map(|r| (r.rect.x, r.rect.y))
        .collect();
    v.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    *v.first().expect("strip 背景が見つかる")
}

/// 一番左の strip の中に描かれた線分バッチ (= サムネイル帯の EQ カーブ)。
fn curve_batches(scene: &Scene) -> Vec<&LineBatch> {
    let (strip_x, _) = first_strip(scene);
    scene
        .iter_lines()
        // カーブは自分のサムネイル帯で clip して描く。その clip 矩形で見分ける
        // (strip の x 範囲だけだと、波形やプレイヘッドの線束まで拾ってしまう)。
        .filter(|b| b.clip_rect.is_some_and(|c| c.x >= strip_x && c.x <= strip_x + STRIP_WIDTH && c.w <= STRIP_WIDTH))
        .filter(|b| !b.segments.is_empty())
        .collect()
}

fn curve_points(scene: &Scene) -> Vec<[f32; 2]> {
    let mut pts = Vec::new();
    for batch in curve_batches(scene) {
        pts.push(batch.segments[0].a);
        pts.extend(batch.segments.iter().map(|s| s.b));
    }
    pts
}

fn span_y(pts: &[[f32; 2]]) -> f32 {
    let (lo, hi) = pts.iter().fold((f32::MAX, f32::MIN), |(lo, hi), p| (lo.min(p[1]), hi.max(p[1])));
    if pts.is_empty() { 0.0 } else { hi - lo }
}

/// 一番左の strip の中 (strip 上端より下) に描かれた、`text` と一致する glyph。
fn strip_glyphs<'a>(scene: &'a Scene, text: &str) -> Vec<&'a GlyphArea> {
    let (strip_x, strip_y) = first_strip(scene);
    scene
        .iter_glyphs()
        .filter(|g| g.left >= strip_x && g.left < strip_x + STRIP_WIDTH && g.top > strip_y)
        .filter(|g| g.text.as_ref() == text)
        .collect()
}

#[test]
fn eq_カーブはパラメーターに追従して曲がり_off_でも形を保つ() {
    // 既定の EQ はフラット (= すべての点が同じ y)。
    let flat = build_app("dark");
    let flat_pts = curve_points(&scene_of(&flat));
    assert!(flat_pts.len() > 10, "カーブが描かれていない ({} 点)", flat_pts.len());
    let flat_span = span_y(&flat_pts);
    assert!(flat_span < 0.5, "既定の EQ なのにカーブが曲がっている (span={flat_span})");

    // +15dB のベルを立てたら山ができる (触ったので device は ON)。
    let mut curved = app_with_curved_eq("dark");
    let scene = scene_of(&curved);
    let span = span_y(&curve_points(&scene));
    assert!(span > 4.0, "+15dB のベルでカーブが山にならない (span={span})");
    let on_alpha = curve_batches(&scene)[0].segments[0].color.a;

    // Q で OFF にしても形は変えず、線だけ薄くする (§20-8)。
    let eq = builtin(&curved, NativeKind::Eq);
    curved.handle_event(AppEvent::Device(DeviceEvent::SetDevicesBypassed { device_ids: vec![eq], bypassed: true }));
    let scene = scene_of(&curved);
    let off_span = span_y(&curve_points(&scene));
    assert!((off_span - span).abs() < 0.01, "OFF でカーブの形が変わった (on {span} / off {off_span})");
    let off_alpha = curve_batches(&scene)[0].segments[0].color.a;
    assert!(off_alpha < on_alpha, "OFF のカーブが薄くならない (on α{on_alpha} / off α{off_alpha})");
}

#[test]
fn eq_カーブは帯の背景に沈まない() {
    let Ok(mut renderer) = OffscreenRenderer::new(W, H) else {
        eprintln!("skip: no GPU adapter/device");
        return;
    };
    // 明背景 / 暗背景の両方で見る (片方だけだと沈む側を見逃す)。
    for theme in ["dark", "light"] {
        let app = app_with_curved_eq(theme);
        let scene = scene_of(&app);
        let pts = curve_points(&scene);
        assert!(!pts.is_empty(), "{theme}: カーブが描かれていない");
        let rgba = renderer.render_to_rgba(&scene).expect("offscreen render");

        // 山の頂点 (= 一番上の点) と、その 6px 下 (= 帯の素の背景) を比べる。
        let top = pts.iter().min_by(|a, b| a[1].partial_cmp(&b[1]).unwrap()).copied().expect("点がある");
        let on_curve = px(&rgba, top[0], top[1]);
        let under = px(&rgba, top[0], top[1] + 6.0);
        assert!(
            diff(on_curve, under) > 20,
            "{theme}: カーブが帯の背景に沈んでいる (curve {on_curve:?} / bg {under:?})"
        );
    }
}

#[test]
fn セクションを開くと既存_strip_がその分だけ下がる() {
    let mut app = build_app("dark");
    let scene = scene_of(&app);
    let closed_head = daw_gui::view::strip_sections::head_height(&app);
    let closed_offset = ms_row_offset_in_strip(&scene);
    let closed_h = first_strip_h(&scene);

    app.handle_event(AppEvent::ToggleStripSection(StripSection::Comp));
    app.handle_event(AppEvent::ToggleStripSection(StripSection::Eq));
    let scene = scene_of(&app);
    let open_head = daw_gui::view::strip_sections::head_height(&app);
    let open_offset = ms_row_offset_in_strip(&scene);
    let open_h = first_strip_h(&scene);

    // 下ペインが自動で高くなる (= strip の絶対 y も動く) ので、**strip 上端からの
    // 相対位置**で見る。ここが帯の伸びと一致していれば、高さの SSoT
    // (`head_height`) と実描画が同じ値から来ていると言える。
    let grew = open_head - closed_head;
    assert!(grew > 100.0, "2 セクション開いたのに帯が伸びていない ({grew}px)");
    assert!(
        ((open_offset - closed_offset) - grew).abs() < 0.01,
        "既存 strip が帯の伸びと同じだけ下がる: M/S 行 {} / 帯 {grew}",
        open_offset - closed_offset
    );
    // **フェーダー等の既存の高さは開閉で変わらない**: strip 全体が帯の伸びと同じ
    // だけ高くなる = 下ペインがその分広がっている、ということ。
    assert!(
        ((open_h - closed_h) - grew).abs() < 0.01,
        "strip の高さが帯の伸びと同じだけ増える: strip {} / 帯 {grew}",
        open_h - closed_h
    );
}

#[test]
fn 組み込み_eq_を_comp_の前へ移すとセクションの上下が入れ替わる() {
    let mut app = build_app("dark");
    app.handle_event(AppEvent::ToggleStripSection(StripSection::Comp));
    app.handle_event(AppEvent::ToggleStripSection(StripSection::Eq));
    let (lev, hp) = section_glyph_ys(&scene_of(&app));
    assert!(lev < hp, "既定 (Comp → EQ) では Comp セクションが上: LEV y={lev} / HP y={hp}");

    let tid = first_track(&app);
    let (comp, eq) = (builtin(&app, NativeKind::Comp), builtin(&app, NativeKind::Eq));
    let comp_index = device_index(&app, comp);
    app.handle_event(AppEvent::Device(DeviceEvent::RelocateDevices(RelocateDevices {
        device_ids: vec![eq],
        dest: ChainRef::Track(tid),
        dest_index: InsertAt::Index(u32::try_from(comp_index).unwrap()),
        copy: false,
    })));
    assert!(device_index(&app, eq) < device_index(&app, comp), "チェーン上で EQ が Comp より前に移っている");

    let (lev, hp) = section_glyph_ys(&scene_of(&app));
    assert!(lev > hp, "EQ を前へ移したら EQ セクションが上: LEV y={lev} / HP y={hp}");
}

#[test]
fn 追加の_comp_は帯に出ず_gr_も組み込みの_id_でだけ引く() {
    let mut app = build_app("dark");
    let tid = first_track(&app);
    app.handle_event(AppEvent::Device(DeviceEvent::AddNative {
        chain: ChainRef::Track(tid),
        kind: NativeKind::Comp,
        open_panel: false,
    }));
    let comp2 = app.cur.song_doc.song().tracks[0]
        .devices
        .iter()
        .filter_map(|d| d.as_native())
        .find(|n| !n.builtin && n.kind() == NativeKind::Comp)
        .expect("追加の Comp")
        .id;
    // 組み込み Comp を ON にしておく (GR を取り違えたら濃い色で塗られる状態にする)。
    let comp = builtin(&app, NativeKind::Comp);
    app.handle_event(AppEvent::Device(DeviceEvent::NativeEdit {
        device_id: comp,
        edit: NativeEdit::param(NativeParamId::Comp(CompParam::Threshold), -20.0),
    }));
    app.handle_event(AppEvent::ToggleStripSection(StripSection::Comp));
    // GR は「Comp 2」にだけ入れる。
    app.handle_event(AppEvent::TrackPeaksTick {
        project: app.pk(),
        tracks: Vec::new(),
        native_gr: Some(vec![(comp2, -6.0)]),
        master_limiter_gr_db: 0.0,
    });
    assert!(app.cur.transport.native_gr.get(comp2) > 5.0, "Comp 2 の GR は届いている");

    let scene = scene_of(&app);
    assert_eq!(strip_glyphs(&scene, "LEV").len(), 1, "帯に出る Comp セクションは組み込みの 1 つだけ");
    let (strip_x, strip_y) = first_strip(&scene);
    let gr = app.theme.daw.strip_gr;
    let gr_fills = scene
        .iter_rects()
        .filter(|r| r.rect.x >= strip_x && r.rect.x < strip_x + STRIP_WIDTH && r.rect.y > strip_y)
        .filter(|r| r.fill.r == gr.r && r.fill.g == gr.g && r.fill.b == gr.b)
        .count();
    assert_eq!(gr_fills, 0, "組み込み Comp の GR は 0 のまま (Comp 2 の GR を組み込みの帯に描かない)");
}

/// 一番左の strip の「LEV」(Comp のモード切替) と「HP」(EQ のフィルタ行) の y。
fn section_glyph_ys(scene: &Scene) -> (f32, f32) {
    let lev = strip_glyphs(scene, "LEV");
    let hp = strip_glyphs(scene, "HP");
    assert_eq!((lev.len(), hp.len()), (1, 1), "Comp / EQ セクションが 1 つずつ描かれる");
    (lev[0].top, hp[0].top)
}

fn device_index(app: &AppData, id: u64) -> usize {
    app.cur.song_doc.song().tracks[0].devices.iter().position(|d| d.id() == id).expect("device が居る")
}

/// 一番左の strip 背景の高さ。
fn first_strip_h(scene: &Scene) -> f32 {
    let (x, y) = first_strip(scene);
    scene
        .iter_rects()
        .filter(|r| (r.rect.w - STRIP_WIDTH).abs() < 0.01 && r.rect.h > 100.0)
        .find(|r| (r.rect.x - x).abs() < 0.01 && (r.rect.y - y).abs() < 0.01)
        .map(|r| r.rect.h)
        .expect("strip 背景が見つかる")
}

/// 一番左の strip で、strip 上端から M/S トグル行までの距離。
/// トラック名は空だと描かれないので、**必ず出る** M ボタンの文字を目印にする。
fn ms_row_offset_in_strip(scene: &Scene) -> f32 {
    let (_, strip_y) = first_strip(scene);
    let top = strip_glyphs(scene, "M").iter().map(|g| g.top).fold(f32::MAX, f32::min);
    assert!(top < f32::MAX, "M ボタンが見つからない");
    top - strip_y
}
