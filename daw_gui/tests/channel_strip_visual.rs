//! 内蔵チャンネルストリップ帯 (docs/plan_channel_strip.md) の描画 regression。
//!
//! レイアウトと色は build / test / clippy を全部すり抜けて壊れる (CLAUDE.md
//! 「Visual regression smoke test」)。ここで固定するのは 3 点:
//!
//! 1. 常設サムネイル帯の **EQ カーブがパラメータに追従して曲がる** (= 描いた線が
//!    フラットのままではない)
//! 2. カーブが帯の背景に **沈まない** (dark / light 両方で実描画してコントラストを測る)
//! 3. セクションを開くと **既存 strip がその分だけ下がる** (= 帯の高さと描画位置が
//!    同じ SSoT から来ている)

use std::sync::Arc;

use common::model::{EqBand, EqParam, TrackBuiltinParam};
use common::protocol::PluginCommand;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_gui::event::{StripEdit, StripSection};
use daw_ui_core::{FrameInput, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{OffscreenRenderer, Scene};

const W: u32 = 960;
const H: u32 = 900;
const STRIP_WIDTH: f32 = 80.0;

fn build_app(theme: &str) -> AppData {
    let (audio_tx, _audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx): (_, UnboundedReceiver<PluginCommand>) = mpsc::unbounded_channel();
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
    std::mem::forget(plugin_rx);
    app.handle_event(AppEvent::SetTheme(theme.to_string()));
    app
}

/// 1 トラック目の EQ を「はっきり曲がる」設定にする (HMF を +15dB / 狭い Q)。
fn app_with_curved_eq(theme: &str) -> AppData {
    let mut app = build_app(theme);
    let track = app.song_doc.song().tracks[0].id;
    app.handle_event(AppEvent::StripEdit {
        track,
        edit: StripEdit::Param { param: TrackBuiltinParam::StripEqOn, value: 1.0 },
    });
    app.handle_event(AppEvent::StripEdit {
        track,
        edit: StripEdit::Param {
            param: TrackBuiltinParam::StripEq { band: EqBand::Hmf, param: EqParam::Gain },
            value: 15.0,
        },
    });
    app.handle_event(AppEvent::StripEdit {
        track,
        edit: StripEdit::Param {
            param: TrackBuiltinParam::StripEq { band: EqBand::Hmf, param: EqParam::Q },
            value: 3.0,
        },
    });
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

/// 一番左の strip の中に描かれた線分バッチ (= EQ カーブ) の頂点列。
fn curve_points(scene: &Scene, strip_x: f32) -> Vec<[f32; 2]> {
    let mut pts = Vec::new();
    for batch in scene.iter_lines() {
        // カーブは自分のサムネイル帯で clip して描く。その clip 矩形で見分ける
        // (strip の x 範囲だけだと、波形やプレイヘッドの線束まで拾ってしまう)。
        let Some(clip) = batch.clip_rect else {
            continue;
        };
        if clip.x < strip_x || clip.x > strip_x + STRIP_WIDTH || clip.w > STRIP_WIDTH {
            continue;
        }
        let Some(first) = batch.segments.first() else {
            continue;
        };
        pts.push(first.a);
        pts.extend(batch.segments.iter().map(|s| s.b));
    }
    pts
}

#[test]
fn eq_カーブはパラメータに追従して曲がる() {
    // バイパス中はフラット (= すべての点が同じ y)。
    let flat = build_app("dark");
    let mut scene = Scene::new();
    frame(&flat, &mut scene);
    let (strip_x, _) = first_strip(&scene);
    let flat_pts = curve_points(&scene, strip_x);
    assert!(flat_pts.len() > 10, "カーブが描かれていない ({} 点)", flat_pts.len());
    let flat_span = span_y(&flat_pts);
    assert!(flat_span < 0.5, "バイパス中なのにカーブが曲がっている (span={flat_span})");

    // +15dB のベルを立てたら山ができる。
    let curved = app_with_curved_eq("dark");
    let mut scene = Scene::new();
    frame(&curved, &mut scene);
    let (strip_x, _) = first_strip(&scene);
    let pts = curve_points(&scene, strip_x);
    let span = span_y(&pts);
    assert!(span > 4.0, "+15dB のベルでカーブが山にならない (span={span})");
}

fn span_y(pts: &[[f32; 2]]) -> f32 {
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    for p in pts {
        lo = lo.min(p[1]);
        hi = hi.max(p[1]);
    }
    if pts.is_empty() { 0.0 } else { hi - lo }
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
        let mut scene = Scene::new();
        frame(&app, &mut scene);
        let (strip_x, _) = first_strip(&scene);
        let pts = curve_points(&scene, strip_x);
        assert!(!pts.is_empty(), "{theme}: カーブが描かれていない");
        let rgba = renderer.render_to_rgba(&scene).expect("offscreen render");

        // 山の頂点 (= 一番上の点) と、その 6px 下 (= 帯の素の背景) を比べる。
        let top = pts
            .iter()
            .min_by(|a, b| a[1].partial_cmp(&b[1]).unwrap())
            .copied()
            .expect("点がある");
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
    let mut scene = Scene::new();
    frame(&app, &mut scene);
    let closed_head = daw_gui::view::strip_sections::head_height(&app);
    let closed_offset = ms_row_offset_in_strip(&scene);
    let closed_h = first_strip_h(&scene);

    app.handle_event(AppEvent::ToggleStripSection(StripSection::Comp));
    app.handle_event(AppEvent::ToggleStripSection(StripSection::Eq));
    let mut scene = Scene::new();
    frame(&app, &mut scene);
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
    // だけ高くなる = 下ペインがその分広がっている、ということ。ここが崩れると
    // 開くたびにフェーダーが縮む (r.md の設計意図に反する)。
    assert!(
        ((open_h - closed_h) - grew).abs() < 0.01,
        "strip の高さが帯の伸びと同じだけ増える: strip {} / 帯 {grew}",
        open_h - closed_h
    );
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
    let (strip_x, strip_y) = first_strip(scene);
    let top = scene
        .iter_glyphs()
        // x だけで絞るとアレンジのトラックヘッダ (同じ x 帯・mixer より上) の
        // M ボタンを拾ってしまうので、strip の上端より下に限る。
        .filter(|g| g.left >= strip_x && g.left < strip_x + STRIP_WIDTH && g.top > strip_y)
        .filter(|g| g.text.as_ref() == "M")
        .map(|g| g.top)
        .fold(f32::MAX, f32::min);
    assert!(top < f32::MAX, "M ボタンが見つからない");
    top - strip_y
}
