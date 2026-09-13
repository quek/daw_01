//! r.md #129 (`docs/plan_rack_native_devices.md` §15.7 R-10 / R-11): Rack の内蔵 device の **見た目と入力** を
//! `build_root` + 合成ポインタで確かめる。GPU 不要 (Scene の描画コマンドの座標と色を読む)。
//!
//! - 行の並び / × の有無 / Par の格子 / Par 同士が重ならない / master の末尾 (Post-Fader / Limiter)
//! - EQ カーブの点のドラッグ (Freq + Gain、数値欄が追従、undo 1 回) とホイール (Q、scroll_area が奪わない)
//! - Par の余白を掴んでも行が動かない
//! - `Q` の宛先 (Rack の Limiter 行 / Mixer の hover が先)
//! - ダーク / ライトで、スペクトラムの上の EQ 点と OFF 行の名前が背景に沈まない

use std::sync::Arc;

use common::model::{EqBand, MASTER_TRACK_ID, NativeDevice, NativeKind, NativeParamId, NativeParams, RackPanelKey};
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher};
use daw_gui::event_device::DeviceEvent;
use daw_gui::event_native::NativeEdit;
use daw_gui::handler::bypass_target::BypassTarget;
use daw_gui::view::native_device::{CurveAxes, CurveBand, EqCurveSource, curve_handles};
use daw_gui::view::root::INSPECTOR_W;
use daw_gui::view::track_inspector::native_panel::layout;
use daw_gui::widgets::select_modifier::SelectModifier;
use daw_ui_core::{FrameInput, PointerFrame, UiHost, contrast_ratio};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, Primitive, Rect, Scene};

const W: u32 = 1400;
const H: u32 = 900;
/// インスペクタの左右の余白 (`track_inspector::draw` の `pad`)。
const PAD: f32 = 12.0;
/// chain list の行高 (`chain_list::ROW_H`)。
const ROW_H: f32 = 26.0;
/// 行の名前のラベルは行の上端から 8px 下に描かれる (`native_row` / `plugin_row`)。
const NAME_TOP: f32 = 8.0;

struct Harness {
    app: AppData,
    host: UiHost<AppData>,
    _audio: UnboundedReceiver<AudioCommand>,
    _plugin: UnboundedReceiver<PluginCommand>,
}

impl Harness {
    fn new(theme: &str) -> Self {
        let (audio_tx, audio_rx) = mpsc::unbounded_channel();
        let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
        let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
        let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
        let mut app = AppData::new(audio_tx, plugin_tx, None, None, event_dispatcher, job_dispatcher, None, None, 48_000);
        app.handle_event(AppEvent::SetTheme(theme.to_string()));
        let mut host: UiHost<AppData> = UiHost::no_redraw();
        if host.set_palette(app.theme.core.clone()) {
            host.invalidate_scene_cache();
        }
        let mut h = Self { app, host, _audio: audio_rx, _plugin: plugin_rx };
        let t0 = h.track(0);
        h.select(t0);
        h
    }

    fn track(&self, idx: usize) -> u32 {
        self.app.cur.song_doc.song().tracks[idx].id
    }

    fn select(&mut self, owner: u32) {
        if owner == MASTER_TRACK_ID {
            self.app.cur.selection.selected_track_ids = vec![MASTER_TRACK_ID];
        } else {
            let visible: Vec<u32> = self.app.cur.song_doc.song().tracks.iter().map(|t| t.id).collect();
            self.app.apply_select_tracks(owner, SelectModifier::Single, &visible);
        }
    }

    fn builtin(&self, owner: u32, kind: NativeKind) -> u64 {
        self.app.cur.song_doc.song().builtin_native(owner, kind).expect("組み込み").id
    }

    fn native(&self, id: u64) -> NativeDevice {
        *self.app.cur.song_doc.song().native_by_id(id).expect("内蔵 device")
    }

    fn dev(&mut self, ev: DeviceEvent) {
        self.app.handle_event(AppEvent::Device(ev));
    }

    fn frame(&mut self, p: PointerFrame) -> Scene {
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: W, height: H };
        self.host.frame(&mut self.app, &mut scene, screen, FrameInput { pointer: p, ..FrameInput::default() }, |app, ui| {
            daw_gui::view::root::build_root(app, ui, screen);
        });
        scene
    }

    /// idle フレームを 2 回 (インスペクタの本文高さの lag-by-one を落ち着かせる)。
    fn settle(&mut self) -> Scene {
        let _ = self.frame(PointerFrame::default());
        self.frame(PointerFrame::default())
    }

    fn click(&mut self, (x, y): (f32, f32)) -> Scene {
        let _ = self.frame(hover(x, y));
        let _ = self.frame(PointerFrame { primary_just_pressed: true, primary_pressed: true, ..hover(x, y) });
        let _ = self.frame(PointerFrame { primary_just_released: true, ..hover(x, y) });
        self.settle()
    }

    /// `from` で押して `steps` 回に分けて `to` まで動かし、離す。
    fn drag(&mut self, from: (f32, f32), to: (f32, f32), steps: usize) -> Scene {
        let _ = self.frame(hover(from.0, from.1));
        let _ = self.frame(PointerFrame { primary_just_pressed: true, primary_pressed: true, ..hover(from.0, from.1) });
        for k in 1..=steps {
            let t = k as f32 / steps as f32;
            let _ = self.frame(held(from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t));
        }
        let _ = self.frame(PointerFrame { primary_just_released: true, ..hover(to.0, to.1) });
        self.settle()
    }
}

fn hover(x: f32, y: f32) -> PointerFrame {
    PointerFrame { pos: Some((x, y)), ..PointerFrame::default() }
}

fn held(x: f32, y: f32) -> PointerFrame {
    PointerFrame { primary_pressed: true, ..hover(x, y) }
}

#[derive(Debug, Clone)]
struct Glyph {
    text: String,
    left: f32,
    top: f32,
    color: Color,
    order: usize,
}

impl Glyph {
    fn center(&self) -> (f32, f32) {
        (self.left + 6.0, self.top + 6.0)
    }
}

/// インスペクタ (左カラム) の base pass の glyph。
fn inspector_glyphs(scene: &Scene) -> Vec<Glyph> {
    scene
        .primitives
        .iter()
        .enumerate()
        .filter_map(|(order, p)| match p {
            Primitive::Glyph(g) if g.left < INSPECTOR_W => {
                Some(Glyph { text: g.text.to_string(), left: g.left, top: g.top, color: g.color, order })
            }
            _ => None,
        })
        .collect()
}

fn find(scene: &Scene, text: &str) -> Vec<Glyph> {
    let mut v: Vec<Glyph> = inspector_glyphs(scene).into_iter().filter(|g| g.text == text).collect();
    v.sort_by(|a, b| a.top.partial_cmp(&b.top).unwrap());
    v
}

fn one(scene: &Scene, text: &str) -> Glyph {
    let v = find(scene, text);
    assert_eq!(v.len(), 1, "インスペクタに `{text}` が 1 つ: {v:?}");
    v[0].clone()
}

/// `row_name` の行の `[Par]` の glyph (同じ行 = top が一番近いもの)。
fn par_of(scene: &Scene, row_name: &str) -> (f32, f32) {
    let name = one(scene, row_name);
    find(scene, "Par")
        .into_iter()
        .min_by(|a, b| (a.top - name.top).abs().partial_cmp(&(b.top - name.top).abs()).unwrap())
        .expect("[Par]")
        .center()
}

/// 行の名前の glyph から、その行の Par の矩形 (行の中身の幅、行の直下)。
fn panel_rect(name: &Glyph, kind: NativeKind) -> Rect {
    Rect { x: PAD, y: name.top - NAME_TOP + ROW_H, w: INSPECTOR_W - PAD * 2.0, h: layout::panel_height(kind) }
}

/// EQ の Par のカーブ上の点の座標 (描画と同じ写像を通す)。
fn eq_handle(dev: &NativeDevice, panel: Rect, band: EqBand) -> (f32, f32) {
    let src = EqCurveSource::from_params(&dev.params).expect("EQ");
    let curve = layout::curve_rect(panel);
    curve_handles(&src, &CurveAxes::for_source(&src), curve)
        .into_iter()
        .find(|h| h.band == CurveBand::Eq(band))
        .expect("点")
        .pos
}

fn eq_params(dev: &NativeDevice) -> common::model::EqSettings {
    match dev.params {
        NativeParams::Eq(e) => e,
        _ => panic!("EQ"),
    }
}

/// R-11 前半: 行の並び / × / Par の格子 / Par 同士が重ならない。
#[test]
fn native_rows_and_panels_are_laid_out_in_the_rack() {
    let mut h = Harness::new("dark");
    let t0 = h.track(0);
    let scene = h.settle();
    let (comp, eq) = (one(&scene, "Comp"), one(&scene, "EQ"));
    assert!(comp.top < eq.top, "組み込みは Comp → EQ の順");
    let xs = find(&scene, "x").len();
    h.dev(DeviceEvent::AddNative { chain: common::model::ChainRef::Track(t0), kind: NativeKind::Comp, open_panel: false });
    let scene = h.settle();
    assert_eq!(find(&scene, "x").len(), xs + 1, "× は足した分にだけ付く");
    assert!(one(&scene, "Comp 2").top < one(&scene, "Comp").top, "足した分は組み込みの上");

    // EQ の [Par]: 見出しが同じ段に左から並び、インスペクタの内側に収まり、次の行は Par の下から。
    let scene = h.click(par_of(&scene, "EQ"));
    let heads: Vec<Glyph> = ["HP", "LF", "LMF", "HMF", "HF", "LP"].iter().map(|t| one(&scene, t)).collect();
    for w in heads.windows(2) {
        assert!((w[0].top - w[1].top).abs() < 0.5 && w[0].left < w[1].left, "{w:?}");
    }
    for g in &heads {
        assert!(g.left >= PAD && g.left < INSPECTOR_W - PAD, "{g:?}");
    }
    let eq_name = one(&scene, "EQ");
    let next = one(&scene, "+ Plugin");
    assert!(
        next.top >= eq_name.top - NAME_TOP + ROW_H + layout::panel_height(NativeKind::Eq),
        "次の行は EQ の Par の下: next={} eq={}",
        next.top,
        eq_name.top
    );

    // Comp の Par も同時に開ける。上の行の Par は下の行 (EQ) に重ならない。
    let scene = h.click(par_of(&scene, "Comp"));
    let (listen, eq_name, comp_name) = (one(&scene, "Listen"), one(&scene, "EQ"), one(&scene, "Comp"));
    assert!(comp_name.top < listen.top && listen.top + 12.0 <= eq_name.top - NAME_TOP, "Comp の Par が EQ の行に重ならない");
    assert_eq!(find(&scene, "HP").len(), 1, "EQ の Par も開いたまま");
    assert!(h.app.rack_panel_open(RackPanelKey::Device(h.builtin(t0, NativeKind::Comp))));
}

/// R-11: master は「Bus Comp」「Tone EQ」「+ FX」「Post-Fader」「Limiter」の順。Limiter 行に × は無く、掴んでも動かない。
#[test]
fn master_rack_ends_with_a_fixed_limiter_after_the_post_fader_divider() {
    let mut h = Harness::new("dark");
    h.select(MASTER_TRACK_ID);
    let scene = h.settle();
    let order: Vec<f32> = ["Bus Comp", "Tone EQ", "+ FX", "Post-Fader", "Limiter"].iter().map(|t| one(&scene, t).top).collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
    let limiter = one(&scene, "Limiter");
    assert!(!find(&scene, "x").iter().any(|g| (g.top - limiter.top).abs() < 10.0), "Limiter 行に × は無い");
    let before = h.app.cur.song_doc.song().master_fx_chain.iter().map(|d| d.id()).collect::<Vec<_>>();
    let (x, y) = limiter.center();
    let scene = h.drag((x, y), (x, y - 20.0), 4);
    let after = h.app.cur.song_doc.song().master_fx_chain.iter().map(|d| d.id()).collect::<Vec<_>>();
    assert_eq!(after, before);
    let order2: Vec<f32> = ["Bus Comp", "Tone EQ", "+ FX", "Post-Fader", "Limiter"].iter().map(|t| one(&scene, t).top).collect();
    assert_eq!(order2, order, "並びも位置も変わらない");
}

/// R-11: EQ カーブの LMF の点を右 40・上 20 へ動かすと Freq と Gain が増え、数値欄の表示も変わり、undo 1 回で両方戻る。
#[test]
fn dragging_an_eq_point_edits_freq_and_gain_in_one_undo_step() {
    let mut h = Harness::new("dark");
    let t0 = h.track(0);
    let eq = h.builtin(t0, NativeKind::Eq);
    h.dev(DeviceEvent::ToggleRackPanel(RackPanelKey::Device(eq)));
    let scene = h.settle();
    let panel = panel_rect(&one(&scene, "EQ"), NativeKind::Eq);
    let before = eq_params(&h.native(eq)).lmf;
    let pos = eq_handle(&h.native(eq), panel, EqBand::Lmf);
    // LMF の Freq の数値欄 (見出しの直下の段、3 列目)。
    let value_rect = Rect {
        x: layout::column_x(panel, 2, layout::NUM_W),
        y: panel.y + layout::PAD + layout::CURVE_H + layout::ROW_GAP + layout::HEAD + layout::KNOB + layout::KNOB_GAP,
        w: layout::NUM_W,
        h: layout::NUM_H,
    };
    let value_text = |scene: &Scene| -> String {
        inspector_glyphs(scene)
            .into_iter()
            .find(|g| g.left >= value_rect.x - 1.0 && g.left < value_rect.x + value_rect.w && g.top >= value_rect.y - 2.0 && g.top < value_rect.y + value_rect.h)
            .map(|g| g.text)
            .expect("LMF Freq の数値欄")
    };
    let text_before = value_text(&scene);
    let depth = h.app.cur.song_doc.undo_depth();

    let scene = h.drag(pos, (pos.0 + 40.0, pos.1 - 20.0), 5);
    let after = eq_params(&h.native(eq)).lmf;
    assert!(after.freq_hz > before.freq_hz && after.gain_db > before.gain_db, "{before:?} -> {after:?}");
    assert!(!h.native(eq).bypassed, "点に触ると ON");
    assert_ne!(value_text(&scene), text_before, "数値欄も同時に動く");
    assert_eq!(h.app.cur.song_doc.undo_depth(), depth + 1, "Freq と Gain のドラッグは undo 1 step");
    h.app.handle_event(AppEvent::Undo);
    let undone = eq_params(&h.native(eq)).lmf;
    assert_eq!((undone.freq_hz, undone.gain_db), (before.freq_hz, before.gain_db));
}

/// R-11: インスペクタがあふれていても、EQ の点の上のホイールは Q を変えてスクロールしない。点の外ではスクロールする。
#[test]
fn wheel_over_an_eq_point_changes_q_instead_of_scrolling() {
    let mut h = Harness::new("dark");
    let t0 = h.track(0);
    for _ in 0..12 {
        h.dev(DeviceEvent::AddNative { chain: common::model::ChainRef::Track(t0), kind: NativeKind::Eq, open_panel: false });
    }
    let added: Vec<u64> = h.app.cur.song_doc.song().tracks[0].devices.iter().take(3).map(|d| d.id()).collect();
    for id in &added {
        h.dev(DeviceEvent::ToggleRackPanel(RackPanelKey::Device(*id)));
    }
    let scene = h.settle();
    let first = added[0];
    let name = h.native(first).display_name().to_string();
    let pos = eq_handle(&h.native(first), panel_rect(&one(&scene, &name), NativeKind::Eq), EqBand::Lmf);
    let rack_top = one(&scene, "Rack").top;
    let q_before = eq_params(&h.native(first)).lmf.q;

    let _ = h.frame(hover(pos.0, pos.1)); // claim は前フレームの宣言を読む
    let _ = h.frame(PointerFrame { scroll_delta: (0.0, -40.0), ..hover(pos.0, pos.1) });
    let scene = h.settle();
    assert_ne!(eq_params(&h.native(first)).lmf.q, q_before, "ホイールで Q が変わる");
    assert_eq!(one(&scene, "Rack").top, rack_top, "点の上ではスクロールしない");

    // 点の外 (見えている行の名前の上) ではスクロールする。
    let second = h.native(added[1]).display_name().to_string();
    let (nx, ny) = one(&scene, &second).center();
    let _ = h.frame(hover(nx, ny));
    let _ = h.frame(PointerFrame { scroll_delta: (0.0, -40.0), ..hover(nx, ny) });
    let scene = h.settle();
    assert!(one(&scene, "Rack").top < rack_top, "点の外ではスクロールする");
}

/// R-11: Par の余白 (見出し) を掴んで縦に動かしても、行はドラッグにならず並びは変わらない。
#[test]
fn pressing_a_panel_background_does_not_drag_the_row() {
    let mut h = Harness::new("dark");
    let t0 = h.track(0);
    h.dev(DeviceEvent::AddNative { chain: common::model::ChainRef::Track(t0), kind: NativeKind::Comp, open_panel: true });
    let scene = h.settle();
    let before = h.app.cur.song_doc.song().tracks[0].devices.iter().map(|d| d.id()).collect::<Vec<_>>();
    let (x, y) = one(&scene, "Thr").center();
    let dragging_fill = h.app.theme.core.accent.with_alpha(0.5);
    let _ = h.frame(hover(x, y));
    let _ = h.frame(PointerFrame { primary_just_pressed: true, primary_pressed: true, ..hover(x, y) });
    let _ = h.frame(held(x, y + 10.0));
    let scene = h.frame(held(x, y + 20.0));
    let row_dragging = scene.primitives.iter().any(|p| matches!(p, Primitive::Rect(r) if r.fill == dragging_fill));
    assert!(!row_dragging, "Par の余白の press で行がドラッグ状態にならない");
    // 行ドラッグなら並べ替えになる所まで運んで離しても変わらない。
    let _ = h.frame(held(x, y + 150.0));
    let _ = h.frame(PointerFrame { primary_just_released: true, ..hover(x, y + 150.0) });
    let _ = h.settle();
    let after = h.app.cur.song_doc.song().tracks[0].devices.iter().map(|d| d.id()).collect::<Vec<_>>();
    assert_eq!(after, before);
}

/// R-10: Rack の Limiter 行の上で `Q` → master Limiter の ON/OFF。Mixer の hover (Mixer タブ + 下部パネルの上) が
/// あればそちらが先。
#[test]
fn q_toggles_the_hovered_rack_limiter_unless_the_mixer_owns_the_pointer() {
    let mut h = Harness::new("dark");
    let t0 = h.track(0);
    let comp = h.builtin(t0, NativeKind::Comp);
    let _ = h.settle();
    let on = h.app.cur.song_doc.song().master_limiter.on;
    h.app.cur.peph.inspector_hovered_row = Some(BypassTarget::MasterLimiter);
    h.host.inject_shortcut("daw.toggle_mute");
    let _ = h.frame(PointerFrame::default());
    assert_eq!(h.app.cur.song_doc.song().master_limiter.on, !on, "Rack の Limiter 行の Q");

    // Mixer タブを開いて下部パネルの上に居る: Mixer の hover が先。
    h.app.cur.view.bottom_panel = Some(0);
    let _ = h.settle();
    let bypassed = h.native(comp).bypassed;
    h.app.cur.peph.inspector_hovered_row = Some(BypassTarget::MasterLimiter);
    h.app.cur.peph.mixer_hovered_native = Some(comp);
    h.host.inject_shortcut("daw.toggle_mute");
    let _ = h.frame(hover(W as f32 * 0.5, H as f32 - 60.0));
    assert_eq!(h.native(comp).bypassed, !bypassed, "Mixer の Comp が切り替わる");
    assert_eq!(h.app.cur.song_doc.song().master_limiter.on, !on, "Rack の Limiter は触らない");
}

/// `point` を含む (base pass で `before` より前に描かれた) 不透明な **面** のうち最後のものの色。
/// 1px の目盛り線 (0 dB 線など) は面ではないので除く。
fn backdrop(scene: &Scene, point: (f32, f32), before: usize) -> Color {
    scene.primitives[..before]
        .iter()
        .rev()
        .find_map(|p| match p {
            Primitive::Rect(r) if r.fill.a >= 0.999 && r.rect.w >= 2.0 && r.rect.h >= 2.0 && r.rect.contains(point.0, point.1) => {
                Some(r.fill)
            }
            _ => None,
        })
        .expect("背景の矩形")
}

/// `over` の上に半透明の `fill` を重ねた色 (linear)。
fn composite(over: Color, fill: Color) -> Color {
    let a = fill.a;
    Color { r: over.r * (1.0 - a) + fill.r * a, g: over.g * (1.0 - a) + fill.g * a, b: over.b * (1.0 - a) + fill.b * a, a: 1.0 }
}

/// R-11: ダーク / ライトで、スペクトラムが最大 (0 dB) のときの EQ 点と、OFF 行の名前が背景に沈まない
/// (非テキストの UI 部品として 3:1 以上、[[feedback_ui_indicator_contrast_on_variable_bg]])。
#[test]
fn eq_points_over_the_spectrum_and_off_row_names_keep_contrast_in_both_themes() {
    const MIN_CONTRAST: f32 = 3.0;
    for theme in ["dark", "light"] {
        for device_on in [false, true] {
            let mut h = Harness::new(theme);
            let t0 = h.track(0);
            let eq = h.builtin(t0, NativeKind::Eq);
            if device_on {
                let edit = NativeEdit::param(NativeParamId::Eq { band: EqBand::Hmf, param: common::model::EqParam::Gain }, 3.0);
                h.dev(DeviceEvent::NativeEdit { device_id: eq, edit });
            }
            h.dev(DeviceEvent::ToggleRackPanel(RackPanelKey::Device(eq)));
            h.app.cur.transport.device_spectra.insert(eq, Arc::from(vec![0.0_f32; daw_gui::master_meter::spectrum::SPECTRUM_BANDS]));
            let scene = h.settle();
            let eq_name = one(&scene, "EQ");
            let panel = panel_rect(&eq_name, NativeKind::Eq);
            let dev = h.native(eq);
            for band in [EqBand::Lmf, EqBand::Hp] {
                let pos = eq_handle(&dev, panel, band);
                // 点 (中心が pos の小さな円の矩形) と、その真下のスペクトラムの塗り (1px 幅の縦 rect)。
                // 目盛り線 (幅 1) と区別するため幅で絞る。
                let (point_idx, point) = scene
                    .primitives
                    .iter()
                    .enumerate()
                    .find_map(|(i, p)| match p {
                        Primitive::Rect(r)
                            if r.rect.w > 2.0
                                && r.rect.w < 12.0
                                && (r.rect.x + r.rect.w * 0.5 - pos.0).abs() < 0.5
                                && (r.rect.y + r.rect.h * 0.5 - pos.1).abs() < 0.5 =>
                        {
                            Some((i, *r))
                        }
                        _ => None,
                    })
                    .expect("EQ の点が描かれる");
                // スペクトラムの塗りはカーブと同じ色相の半透明 1px 列 (目盛り線とは色で区別する)。
                let hue = h.app.theme.daw.strip_eq_curve;
                let spectrum = scene.primitives[..point_idx]
                    .iter()
                    .rev()
                    .find_map(|p| match p {
                        Primitive::Rect(r)
                            if r.rect.w <= 1.0
                                && (r.fill.r, r.fill.g, r.fill.b) == (hue.r, hue.g, hue.b)
                                && r.rect.contains(pos.0.floor() + 0.5, pos.1) =>
                        {
                            Some(r.fill)
                        }
                        _ => None,
                    })
                    .expect("スペクトラムの塗りが点の下にある");
                let bg = composite(backdrop(&scene, pos, point_idx), spectrum);
                let ink = if point.fill.a > 0.0 { point.fill } else { point.border };
                let ratio = contrast_ratio(ink, bg);
                assert!(ratio >= MIN_CONTRAST, "{theme} on={device_on} {band:?}: 点 {ink:?} / 背景 {bg:?} = {ratio:.2}");
            }
            if !device_on {
                // OFF (bypass) の行の名前。
                let bg = backdrop(&scene, eq_name.center(), eq_name.order);
                let ratio = contrast_ratio(eq_name.color, bg);
                assert!(ratio >= MIN_CONTRAST, "{theme}: OFF 行の名前 {:?} / 背景 {bg:?} = {ratio:.2}", eq_name.color);
            }
        }
    }
}

/// Comp の Par のモード切り替え (`[LEV|CMP|LIM]`) を押すと、モードに上書きされる列が変わる (描画が入力に届く確認)。
#[test]
fn comp_mode_buttons_switch_the_mode() {
    let mut h = Harness::new("dark");
    let t0 = h.track(0);
    let comp = h.builtin(t0, NativeKind::Comp);
    h.dev(DeviceEvent::ToggleRackPanel(RackPanelKey::Device(comp)));
    let scene = h.settle();
    let _ = h.click(one(&scene, "LIM").center());
    match h.native(comp).params {
        NativeParams::Comp(c) => assert_eq!(c.mode, common::model::CompMode::Limiter),
        _ => panic!("Comp"),
    }
    assert!(!h.native(comp).bypassed, "モード切り替えも触れば ON");
}
