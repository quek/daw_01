//! r.md #62: mixer strip の **pan 数値がノブの右に並ぶ** ことの regression。
//!
//! レイアウトは build / test / clippy を全部すり抜けて壊れる (CLAUDE.md
//! 「Visual regression smoke test」)。 ここではルート view を 1 フレーム組んで、
//!
//! 1. pan の数値 (`"L100"` / `"C"` / `"R100"`) が **各 strip 内の、 ノブの右** に描かれる
//!    (= 旧レイアウトの「ノブの下」 に戻っていない)、
//! 2. 数値が **自分の strip の内側** に収まる (= 隣の strip へグリフが漏れない)、
//! 3. 数値が strip 上端から **ノブと同じ行の高さ** に出る (= 旧レイアウトより 22px 上、
//!    その結果フェーダー / メーターが 14px せり上がっている)、
//!
//! を Scene の実描画コマンド (glyph / rect の座標) で検証する。 GPU 不要。
//!
//! 加えて GPU adapter がある環境では offscreen 描画して
//! `target/theme_shots/mixer_pan_readout.png` に残す (目視確認用)。 adapter が無ければ
//! その部分だけ graceful skip。

use std::collections::HashSet;
use std::sync::Arc;

use common::protocol::PluginCommand;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_ui_core::{FrameInput, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{OffscreenRenderer, Scene};

const W: u32 = 960;
const H: u32 = 900;

/// `mixer_strips.rs` の layout 定数 (private なのでテスト側に写す)。 ここがズレたら
/// 下のアサーションが落ちるので、 定数の意図しない変更も検知できる。
const STRIP_WIDTH: f32 = 80.0;
const KNOB_SIZE: f32 = 32.0;
/// strip 上端から「既存 strip の中身」が始まるまでの距離 (px)。
/// docs/plan_channel_strip.md: 内蔵チャンネルストリップの **常設サムネイル帯**
/// (EQ カーブ + GR) が必ず上端に乗る。EQ / Comp セクションはこのテストでは
/// 折り畳んだまま (`UiPrefs` の既定) なので、頭は帯 1 本ぶん。
const STRIP_HEAD_H: f32 = daw_gui::view::strip_sections::THUMB_H;
/// strip 上端から pan 数値の **文字の top** までの距離 (px)。
/// 頭の帯 + `pad 6 + 名前 18 + M/S 22 + 余白 6` で pan 行 (= ノブ 32px) が始まり、
/// 数値欄 (16px) はノブと縦センタなので `+8`、 その中で 1 行 (font 10 → 12px) が
/// 縦センタなので `+2`。
const READOUT_TOP_IN_STRIP: f32 = STRIP_HEAD_H + 6.0 + 18.0 + 22.0 + 6.0 + 8.0 + 2.0;
/// strip 上端から pan 行 (= ノブの rect 上端) までの距離 (px)。
const KNOB_TOP_IN_STRIP: f32 = STRIP_HEAD_H + 6.0 + 18.0 + 22.0 + 6.0;
/// strip 左端から pan 行の左端 (= ノブの rect 左端) までの距離 (px)。
/// `(STRIP_WIDTH - PAN_ROW_W) / 2` = `(80 - 66) / 2`。
const KNOB_LEFT_IN_STRIP: f32 = 7.0;
/// ノブのリング半径 (px)。 `draw_knob` の `size * 0.5 - 2.0` (= 32 / 2 - 2)。
const KNOB_RING_RADIUS: f32 = 14.0;

fn build_app() -> (AppData, UnboundedReceiver<PluginCommand>) {
    let (audio_tx, _audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let app = AppData::new(
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
    (app, plugin_rx)
}

/// 3 トラックを作り、 pan を最左 / センタ / 最右 に振る (= 表記が `"L100"` / `"C"` /
/// `"R100"` の 3 種類すべて出る状態)。 Mixer は bottom panel の tab 0 = 既定。
fn app_with_three_pans() -> AppData {
    app_with_three_pans_themed("dark")
}

fn app_with_three_pans_themed(theme: &str) -> AppData {
    let (mut app, _rx) = build_app();
    app.handle_event(AppEvent::SetTheme(theme.to_string()));
    // 既定の song が既に 1 トラック持つので、 3 本になるまで足す。
    while app.track_mix().len() < 3 {
        app.handle_event(AppEvent::AddInstrumentTrack);
    }
    // `track_mix()` の並び = mixer が strip を左から描く順 (SSoT)。 その順に振る。
    let ids: Vec<u32> = app.track_mix().iter().map(|e| e.track_id).collect();
    assert_eq!(ids.len(), 3, "3 トラック: {ids:?}");
    for (track, pan) in ids.iter().zip([-1.0_f32, 0.0, 1.0]) {
        app.handle_event(AppEvent::SetTrackPan { track: *track, pan });
    }
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

/// RGBA バッファから 1 pixel の RGB を取る (座標は f32 → 最近傍)。
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn px(rgba: &[u8], x: f32, y: f32) -> [u8; 3] {
    let (xi, yi) = (x.round().max(0.0) as u32, y.round().max(0.0) as u32);
    let i = ((yi.min(H - 1) * W + xi.min(W - 1)) * 4) as usize;
    [rgba[i], rgba[i + 1], rgba[i + 2]]
}

/// sRGB 8bit で各チャンネル 4 以内なら同色とみなす (レンダラのディザ / 端数を許容)。
fn near(a: [u8; 3], b: [u8; 3]) -> bool {
    (0..3).all(|i| a[i].abs_diff(b[i]) <= 4)
}

/// strip 背景 panel の `(x, y)` を左から順に。 幅 = `STRIP_WIDTH` の縦長 rect で拾う。
fn strip_rects(scene: &Scene) -> Vec<(f32, f32)> {
    let mut v: Vec<(f32, f32)> = scene
        .iter_rects()
        .filter(|r| (r.rect.w - STRIP_WIDTH).abs() < 0.01 && r.rect.h > 100.0)
        .map(|r| (r.rect.x, r.rect.y))
        .collect();
    v.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    v.dedup_by(|a, b| (a.0 - b.0).abs() < 0.01);
    v
}

/// pan 表記の glyph を「描かれた順 = strip の並び順」 で取り出す。
fn pan_glyphs(scene: &Scene) -> Vec<(String, f32, f32)> {
    scene
        .iter_glyphs()
        .filter(|g| matches!(g.text.as_ref(), "L100" | "C" | "R100"))
        .map(|g| (g.text.as_ref().to_string(), g.left, g.top))
        .collect()
}

#[test]
fn pan_readout_sits_right_of_the_knob_inside_its_own_strip() {
    let app = app_with_three_pans();
    let mut scene = Scene::new();
    frame(&app, &mut scene);

    let readouts = pan_glyphs(&scene);
    assert_eq!(
        readouts.iter().map(|(t, ..)| t.as_str()).collect::<Vec<_>>(),
        ["L100", "C", "R100"],
        "3 strip 分の pan 表記が左から順に描かれる (got {readouts:?})"
    );

    let strips = strip_rects(&scene);
    assert!(
        strips.len() >= 3,
        "strip 背景 (幅 {STRIP_WIDTH}) が 3 本以上見つかる (got {strips:?})"
    );

    for (i, (text, left, top)) in readouts.iter().enumerate() {
        let strip_x = strips[i].0;
        // (1) 数値は **ノブの右**: strip 左端 + ノブ幅 より右から始まる。
        assert!(
            *left > strip_x + KNOB_SIZE,
            "'{text}' (x={left}) は strip {i} (x={strip_x}) のノブ ({KNOB_SIZE}px) より右に出る"
        );
        // (2) 数値は **自分の strip の内側**: 右端がストリップ幅を越えない。
        assert!(
            *left < strip_x + STRIP_WIDTH,
            "'{text}' (x={left}) は strip {i} の右端 ({}) を越えない",
            strip_x + STRIP_WIDTH
        );
        // (3) 3 本とも同じ y (= ノブと縦センタ揃えの 1 行)。
        assert!(
            (*top - readouts[0].2).abs() < 0.01,
            "'{text}' の y ({top}) が 1 本目 ({}) と揃う",
            readouts[0].2
        );
    }
}

/// pan 数値が strip 内の **どの高さ** に出るかを実描画で固定する。 旧レイアウト
/// (ノブの真下に数値行) では数値がここより 22px 下に出た。 この 1 本で
/// 「pan 行より上の積み上げ」 と「数値がノブと同じ行にいること」 の両方が固定され、
/// 「フェーダー上端が 14px せり上がった」 ことの裏付けになる (フェーダー上端そのもの =
/// `STRIP_FADER_TOP_OFFSET` は単体テストで y 積み上げと一致固定済)。
#[test]
fn pan_readout_sits_on_the_knob_row_not_below_it() {
    let app = app_with_three_pans();
    let mut scene = Scene::new();
    frame(&app, &mut scene);

    let (_, _, readout_top) = pan_glyphs(&scene)
        .into_iter()
        .next()
        .expect("pan 数値が描かれる");
    let strip_top = strip_rects(&scene).first().expect("strip 背景が見つかる").1;

    let offset = readout_top - strip_top;
    assert!(
        (offset - READOUT_TOP_IN_STRIP).abs() < 0.01,
        "pan 数値は strip 上端から {READOUT_TOP_IN_STRIP}px (= ノブと同じ行) に出る。 got {offset}"
    );
}

/// ノブの **可動範囲外 (下の 60°)** が strip の背景と同色に塗られ、 リングが下で
/// 切れて見えること。 旧実装ではその 60° に円本体の縁 (`control`) と 1px 枠 (`border`) が
/// そのまま見えていて、 リングが途切れず 1 周しているように読めていた
/// (ユーザー報告:「パンのノブがそこまで回転しない場合の弧の色はミキサーの背景色と同じに」)。
///
/// 6 時 (可動範囲外) のリング上 pixel が strip 背景と一致し、 3 時 (可動範囲内) の
/// リング上 pixel が背景と **一致しない** ことの両方を見る。 前者だけだと「リング全体が
/// 消えた」 バグを通してしまう。 dark / light 両方で確認する。
#[test]
fn knob_out_of_range_arc_matches_the_strip_background() {
    let Ok(mut renderer) = OffscreenRenderer::new(W, H) else {
        eprintln!("skip knob out-of-range visual test: no GPU adapter/device");
        return;
    };
    for theme in ["dark", "light"] {
        let app = app_with_three_pans_themed(theme);
        let mut scene = Scene::new();
        frame(&app, &mut scene);
        let (strip_x, strip_y) = strip_rects(&scene)[0];
        let rgba = renderer.render_to_rgba(&scene).expect("offscreen render");

        let cx = strip_x + KNOB_LEFT_IN_STRIP + KNOB_SIZE * 0.5;
        let cy = strip_y + KNOB_TOP_IN_STRIP + KNOB_SIZE * 0.5;
        // strip の素の背景 (M/S 行と pan 行の間の 6px の余白、 左右中央)。
        let bg = px(&rgba, strip_x + STRIP_WIDTH * 0.5, strip_y + KNOB_TOP_IN_STRIP - 3.0);
        // 6 時 = 可動範囲外 (300° sweep の外)。
        let bottom = px(&rgba, cx, cy + KNOB_RING_RADIUS);
        // 3 時 = 可動範囲内 (track 弧か値弧が必ず乗る)。
        let right = px(&rgba, cx + KNOB_RING_RADIUS, cy);

        assert!(
            near(bottom, bg),
            "{theme}: ノブ 6 時 (可動範囲外) は strip 背景と同色 (got {bottom:?} / bg {bg:?})"
        );
        assert!(
            !near(right, bg),
            "{theme}: ノブ 3 時 (可動範囲内) にはリングが乗っている (got {right:?} / bg {bg:?})"
        );
    }
}

/// 目視確認用の PNG を残す + 「pan 数値欄が一様塗りに潰れていない」 ことを pixel で確認。
/// **ダーク / ライト両方**を見る: 欄の面 (`inset_bg`) と文字 (`text`) はテーマで反転するので、
/// 片方だけ確認すると「もう一方のテーマで文字が背景に沈む」 を見逃す (r.md #48)。
/// GPU adapter が無い環境では skip。
#[test]
fn pan_readout_renders_visible_content_in_both_themes() {
    let Ok(mut renderer) = OffscreenRenderer::new(W, H) else {
        eprintln!("skip mixer pan readout visual test: no GPU adapter/device");
        return;
    };
    for (theme, file) in [("dark", "mixer_pan_readout.png"), ("light", "mixer_pan_readout_light.png")] {
        let app = app_with_three_pans_themed(theme);
        let mut scene = Scene::new();
        frame(&app, &mut scene);

        let (_, first_left, first_top) = pan_glyphs(&scene)
            .into_iter()
            .next()
            .expect("pan 数値が描かれる");

        let rgba = renderer.render_to_rgba(&scene).expect("offscreen render");
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/theme_shots");
        if std::fs::create_dir_all(&dir).is_ok() {
            let _ = image::save_buffer(dir.join(file), &rgba, W, H, image::ColorType::Rgba8);
        }

        // 数値欄の帯 (先頭 strip の数値 x から 22px × 行高 12px) に複数の色がある
        // = 文字が実際に描かれている (欄が空 / 文字が背景に沈んでいれば 1-2 色)。
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let (x0, y0) = (first_left.max(0.0) as u32, first_top.max(0.0) as u32);
        let mut uniq: HashSet<u32> = HashSet::new();
        for y in y0..(y0 + 12).min(H) {
            for x in x0..(x0 + 22).min(W) {
                let i = ((y * W + x) * 4) as usize;
                uniq.insert(u32::from_be_bytes([rgba[i], rgba[i + 1], rgba[i + 2], 0]));
            }
        }
        assert!(
            uniq.len() > 3,
            "{theme}: pan 数値欄に文字が描かれている (unique colors = {})",
            uniq.len()
        );
    }
}
