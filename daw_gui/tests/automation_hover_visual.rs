//! r.md #73 の **visual regression**: Alt を押したままカーソルをオートメーションの線に
//! 置いても、曲線が消えない。
//!
//! hover 強調 (`render::draw_bend_overlays`) は cached の **外**に重ねるだけなので、
//! cached 側の base curve は 1 本も減らないはず。 ここはその不変条件を機械で止める。
//!
//! **`arr_widget.rs` (widget を直接呼ぶ) では捕まらない種類の壊れ方**があるので、
//! ここでは `view::root::build_root` を通す — 実機と同じ経路 (arrangement_view の
//! overlay 群 / popup / inline editor まで含む) でないと、widget の外で起きた上書きが
//! 見えない。
//!
//! 2 層で見る:
//! 1. **Scene** — 線分の本数。「消えた」= 減る、「強調が乗った」= 増える。
//! 2. **pixel** — オフスクリーン描画した実ピクセル。Scene が正しくても
//!    「重ねた色が背景と同化する」「scissor で切れる」で消えうるので、
//!    ユーザーが見るのと同じ層でも確かめる (GPU が無ければ skip)。
//!
//! テーマは **ダークとライトの両方**で回す。レーンの背景はテーマで明暗が反転する
//! 可変背景なので、固定の色トークンでなぞると片方の極性でだけ消えうる
//! (memory `feedback_ui_indicator_contrast_on_variable_bg`)。

use std::collections::HashMap;
use std::sync::Arc;

use common::app_dirs::AppDirs;
use common::model::{
    AutomationClip, AutomationContent, AutomationCurve, AutomationLane, AutomationPoint,
    AutomationTarget, ClipContent, TrackBuiltinParam,
};
use common::protocol::PluginCommand;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{track_with, AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_ui_core::{FrameInput, PointerFrame, UiHost};
use daw_ui_platform::{Modifiers, PhysicalSize};
use daw_ui_renderer::{LineBatch, OffscreenRenderer, Rect, Scene};

const W: u32 = 1200;
const H: u32 = 700;
const CONTENT_ID: common::model::ContentId = 4242;

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
        None::<AppDirs>,
        48_000,
    );
    (app, plugin_rx)
}

/// track 1 本 + Volume lane 1 本 (展開済) + `[0, 8)` の clip + 点 3 つ。
/// 値は plain (Volume は `0.0..=2.0`)。
///
/// **両方の区間を S 字にしてある** — uniform sampling で段数が桁違いに増えるので、
/// 下の `automation_curve_batch` が「幅 1.5 かつ 20 段以上」で一意に拾える。
fn add_automation_lane(app: &mut AppData) {
    app.edit_song(|song| {
        song.tracks.clear();
        song.clip_contents.insert(
            CONTENT_ID,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint {
                        id: 1,
                        time_beat: 0.0,
                        value: 0.2,
                        curve: AutomationCurve::Linear,
                    },
                    AutomationPoint {
                        id: 2,
                        time_beat: 2.0,
                        value: 1.8,
                        curve: AutomationCurve::Bezier { tension: 0.5 },
                    },
                    AutomationPoint {
                        id: 3,
                        time_beat: 6.0,
                        value: 0.6,
                        curve: AutomationCurve::Bezier { tension: -0.5 },
                    },
                ],
                next_point_id: 4,
            }),
        );
        song.tracks.push(track_with(|t| {
            t.id = 1;
            t.automation_lanes = vec![AutomationLane {
                id: 1,
                height_px: 80,
                clips: vec![AutomationClip {
                    id: 1,
                    name: String::new(),
                    start_beat: 0.0,
                    length_beats: 8.0,
                    content_id: CONTENT_ID,
                    content_offset_beats: 0.0,
                    color: None,
                }],
                next_clip_id: 2,
                ..AutomationLane::new(
                    AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                    1.0,
                )
            }];
        }));
    });
    app.ui_prefs.expanded_automation_tracks.insert(1);
}

fn frame(p: PointerFrame) -> FrameInput {
    FrameInput { pointer: p, ..FrameInput::default() }
}

/// `build_root` を 1 フレーム走らせて Scene を返す (実機と同じ経路)。
fn drive_root(host: &mut UiHost<AppData>, app: &mut AppData, p: PointerFrame) -> Scene {
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: W, height: H };
    let edits = host.frame_to_edits(app, &mut scene, screen, frame(p), |app, ui| {
        daw_gui::view::root::build_root(app, ui, screen);
    });
    for e in edits {
        e.apply(app);
    }
    scene
}

/// automation の曲線の line batch。
///
/// テスト側でレイアウト式 (ruler 高さ / lane の縦 padding / beat→px) を複製すると、
/// 式がずれた瞬間に「線の上を狙ったつもりで外れ、hover が起きず、それでも緑」という
/// 空振りになる。ここでは scene に実際に積まれた線分から拾う。
///
/// 見分け方: 曲線は `automation_curve_line_width_px` (= 1.5) で、fixture を全区間 S 字に
/// してあるので段数が多い。幅 1.5 の他の batch (loop 帯 / playhead / ノブ) は 1 桁段。
/// **一意であることも assert する** — 取り違えると「線の上を狙っていないのに緑」になる。
fn automation_curve_batch(scene: &Scene) -> &LineBatch {
    let mut hits: Vec<&LineBatch> = scene
        .iter_lines()
        .filter(|b| (b.line_width_px - 1.5).abs() < 1e-3 && b.segments.len() >= 20)
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "automation 曲線の line batch が一意に決まらない (見分け方が陳腐化した): {:?}",
        hits.iter().map(|b| (b.segments.len(), b.line_width_px, b.clip_rect)).collect::<Vec<_>>()
    );
    hits.pop().expect("1 件")
}

/// 曲線上の点 (各線分の中点)。ここを狙えば必ず線の上に乗る。
fn automation_curve_points(scene: &Scene) -> Vec<(f32, f32)> {
    automation_curve_batch(scene)
        .segments
        .iter()
        .map(|s| ((s.a[0] + s.b[0]) * 0.5, (s.a[1] + s.b[1]) * 0.5))
        .collect()
}

/// scene 全体の線分の総数。
fn total_line_segments(scene: &Scene) -> usize {
    scene.iter_lines().map(|b| b.segments.len()).sum()
}

/// `band` の中で「背景でない」ピクセル数。背景 = その帯の最頻色。
/// 線が消えれば減り、強調が乗れば増える — 「見えているか」を 1 つの尺度で表す。
fn ink_pixels(rgba: &[u8], band: Rect) -> usize {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (x0, y0) = (band.x.max(0.0) as u32, band.y.max(0.0) as u32);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (x1, y1) = (((band.x + band.w) as u32).min(W), ((band.y + band.h) as u32).min(H));
    let mut hist: HashMap<[u8; 3], usize> = HashMap::new();
    let mut px: Vec<[u8; 3]> = Vec::new();
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            let c = [rgba[i], rgba[i + 1], rgba[i + 2]];
            *hist.entry(c).or_insert(0) += 1;
            px.push(c);
        }
    }
    let bg = *hist.iter().max_by_key(|(_, n)| **n).expect("帯が空でない").0;
    // 背景から目に見えて離れているピクセルだけ数える (AA の裾で数がぶれないよう閾値を置く)。
    px.iter()
        .filter(|c| {
            let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).abs();
            d(c[0], bg[0]) + d(c[1], bg[1]) + d(c[2], bg[2]) > 60
        })
        .count()
}

/// sRGB バイトから WCAG relative luminance (`theme_visual.rs` と同じ換算)。
fn luminance(c: [u8; 3]) -> f32 {
    let lin = |v: u8| daw_ui_core::theme::srgb_to_linear(f32::from(v) / 255.0);
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}

/// WCAG コントラスト比 (1.0 = 同色、21.0 = 黒白)。
fn contrast(a: [u8; 3], b: [u8; 3]) -> f32 {
    let (x, y) = (luminance(a), luminance(b));
    let (hi, lo) = if x > y { (x, y) } else { (y, x) };
    (hi + 0.05) / (lo + 0.05)
}

/// `path` (曲線上の screen 座標列) のピクセルが、その帯の背景に対して
/// どれだけ contrast を持っているか。**「線が見えるか」の尺度**。
///
/// 「背景でないピクセルが在る」だけでは足りない — 可変背景の上に固定色を重ねると
/// 「描かれてはいるが見えない」が起きる (memory
/// `feedback_ui_indicator_contrast_on_variable_bg`)。
fn path_contrast(rgba: &[u8], path: &[(f32, f32)], band: Rect) -> f32 {
    let bg = modal_color(rgba, band);
    let mut best: f32 = 1.0;
    for &(x, y) in path {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (xi, yi) = (x.max(0.0) as u32, y.max(0.0) as u32);
        if xi >= W || yi >= H {
            continue;
        }
        // 線は AA が乗るので、狙った点の上下 2px から一番コントラストの高い画素を採る。
        for dy in -2_i32..=2 {
            let yy = yi as i32 + dy;
            if yy < 0 || yy >= H as i32 {
                continue;
            }
            #[allow(clippy::cast_sign_loss)]
            let i = ((yy as u32 * W + xi) * 4) as usize;
            best = best.max(contrast([rgba[i], rgba[i + 1], rgba[i + 2]], bg));
        }
    }
    best
}

fn modal_color(rgba: &[u8], band: Rect) -> [u8; 3] {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (x0, y0) = (band.x.max(0.0) as u32, band.y.max(0.0) as u32);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (x1, y1) = (((band.x + band.w) as u32).min(W), ((band.y + band.h) as u32).min(H));
    let mut hist: HashMap<[u8; 3], usize> = HashMap::new();
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            *hist.entry([rgba[i], rgba[i + 1], rgba[i + 2]]).or_insert(0) += 1;
        }
    }
    *hist.iter().max_by_key(|(_, n)| **n).expect("帯が空でない").0
}

/// **r.md #73 の回帰**: Alt hover で automation の曲線が消えない (ダーク)。
#[test]
fn alt_hover_on_the_curve_does_not_erase_it() {
    check_alt_hover_keeps_the_curve("dark", false);
}

/// 同じことをライトテーマでも見る (レーン背景は可変で、極性が反転する)。
#[test]
fn alt_hover_on_the_curve_does_not_erase_it_in_light_theme() {
    check_alt_hover_keeps_the_curve("light", false);
}

/// **クリップを選択した状態** (= 塗りが選択色に変わり背景の明度が跳ね上がる) でも消えない。
/// 強調は固定の色トークンなので、背景が明るい側へ振れると contrast を失いうる。
#[test]
fn alt_hover_on_the_curve_does_not_erase_it_when_the_clip_is_selected() {
    check_alt_hover_keeps_the_curve("dark", true);
    check_alt_hover_keeps_the_curve("light", true);
}

fn check_alt_hover_keeps_the_curve(theme_id: &str, select_clip: bool) {
    let (mut app, _rx) = build_app();
    app.handle_event(AppEvent::SetTheme(theme_id.to_string()));
    assert_eq!(app.theme.id, theme_id, "テーマが適用されている");
    add_automation_lane(&mut app);
    if select_clip {
        app.selection.selected_automation_clips =
            vec![common::model::AutomationClipKey { track: 1, lane: 1, clip: 1 }];
    }
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    if host.set_palette(app.theme.core.clone()) {
        host.invalidate_scene_cache();
    }

    // 1 フレーム目で曲線が描かれる。そこから **実際に描かれた点** を拾って狙う。
    let first = drive_root(&mut host, &mut app, PointerFrame::default());
    let curve_pts = automation_curve_points(&first);
    let base_all = total_line_segments(&first);
    let alt_mods = Modifiers { alt: true, ..Modifiers::empty() };

    // (1) Scene: 曲線の上を **端から端までなぞる**。1 点だけ見ると「たまたまそこは平気」を
    //     見逃す (点の近く / clip の端 / 区間の境目で挙動が変わりうる)。
    //     cache hit の frame と、**Alt を押したまま cached が作り直される** frame の両方
    //     (後者は「alt 押下が再構築を誘発して curve が落ちる」筋の検査。実機では
    //     再生 / 編集でいつでも起きる)。
    let mut highlighted = 0_usize;
    for (i, &(px, py)) in curve_pts.iter().enumerate() {
        for invalidate in [false, true] {
            if invalidate {
                host.invalidate_scene_cache();
            }
            let s = drive_root(
                &mut host,
                &mut app,
                PointerFrame { pos: Some((px, py)), modifiers: alt_mods, ..PointerFrame::default() },
            );
            let n = total_line_segments(&s);
            assert!(
                n >= base_all,
                "[{theme_id}] 曲線上 {i} 点目 ({px},{py}) の Alt hover \
                 (再構築={invalidate}) で線が減った: {base_all} → {n}"
            );
            if n > base_all {
                highlighted += 1;
            }
        }
    }
    assert!(
        highlighted > curve_pts.len(),
        "[{theme_id}] 強調が 1 度も乗っていない = 線の上を狙えていない (空振り): \
         {highlighted} / {}",
        curve_pts.len() * 2
    );

    // (2) pixel: 実際に描かれた結果でも線が残っているか。
    let Ok(mut renderer) = OffscreenRenderer::new(W, H) else {
        eprintln!("skip pixel check: no GPU adapter/device");
        return;
    };
    let (gx, gy) = curve_pts[curve_pts.len() / 2];
    let plain = drive_root(
        &mut host,
        &mut app,
        PointerFrame { pos: Some((gx, gy)), ..PointerFrame::default() },
    );
    let hovered = drive_root(
        &mut host,
        &mut app,
        PointerFrame { pos: Some((gx, gy)), modifiers: alt_mods, ..PointerFrame::default() },
    );
    let band = automation_curve_batch(&plain).clip_rect.expect("曲線には clip がある");
    let plain_px = renderer.render_to_rgba(&plain).expect("offscreen render");
    let alt_px = renderer.render_to_rgba(&hovered).expect("offscreen render");
    // 目視用に残す (失敗時の切り分けに使う)。
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/automation_hover");
    if std::fs::create_dir_all(&dir).is_ok() {
        let save = |name: &str, buf: &[u8]| {
            let _ = image::save_buffer(dir.join(name), buf, W, H, image::ColorType::Rgba8);
        };
        let sel = if select_clip { "_selected" } else { "" };
        save(&format!("{theme_id}{sel}_plain.png"), &plain_px);
        save(&format!("{theme_id}{sel}_alt.png"), &alt_px);
    }
    let tag = if select_clip { "選択中" } else { "非選択" };
    let plain_ink = ink_pixels(&plain_px, band);
    let alt_ink = ink_pixels(&alt_px, band);
    assert!(plain_ink > 0, "[{theme_id}/{tag}] 前提: 修飾なしで曲線のピクセルが見えている");
    assert!(
        alt_ink >= plain_ink,
        "[{theme_id}/{tag}] Alt hover で曲線が消えている (pixel): \
         修飾なし {plain_ink}px → Alt {alt_ink}px"
    );

    // **描かれているだけでなく見えているか** — 曲線上のピクセルの背景に対する contrast。
    // 可変背景 (テーマ / lane 色 / 選択) の上に固定色を重ねると
    // 「Scene には在るのに目には消えている」が起きる。
    let hovered_path: Vec<(f32, f32)> = {
        // 強調が乗った区間 = 幅 2.25 の batch。 その線分の中点を辿る。
        let b = hovered
            .iter_lines()
            .find(|b| (b.line_width_px - 2.25).abs() < 1e-3)
            .expect("Alt hover で強調 batch が出ている");
        b.segments.iter().map(|s| ((s.a[0] + s.b[0]) * 0.5, (s.a[1] + s.b[1]) * 0.5)).collect()
    };
    let plain_contrast = path_contrast(&plain_px, &hovered_path, band);
    let alt_contrast = path_contrast(&alt_px, &hovered_path, band);
    eprintln!(
        "[{theme_id}/{tag}] contrast 修飾なし {plain_contrast:.2} → Alt {alt_contrast:.2} \
         (ink {plain_ink} → {alt_ink})"
    );
    // WCAG の非テキスト最低要件 3:1 を下回ったら「見えない」と判定する。
    assert!(
        plain_contrast >= 3.0,
        "[{theme_id}/{tag}] 前提: 修飾なしの曲線が背景に対して見えている ({plain_contrast:.2}:1)"
    );
    assert!(
        alt_contrast >= 3.0,
        "[{theme_id}/{tag}] Alt hover の強調が背景に埋もれて見えない \
         ({plain_contrast:.2}:1 → {alt_contrast:.2}:1)"
    );
}

// ============================================================
// r.md #73: 強調 / bend preview が **1 本に見える**
// ============================================================
//
// ここは上の「消えない」と **独立した不変条件**を見る。片方だけだと、一方を他方へ
// 変換する修正が素通りする — 実際そうなった:
//
// - 最初の症状は「Alt hover で線が消える」。強調の色 (`selection_warm`) が、選択中クリップの
//   塗り (`clip_selected_fill` = **同じ `selection_warm`**) と同色で、芯が塗りに沈んでいた。
// - それを「逆極性の縁取りを敷く」で直した。線は消えなくなったが **芯は沈んだまま**なので、
//   両側にはみ出した縁だけが残り **平行 2 本線**になった (= r.md #73「曲げている最中に線が
//   2 重に見える」の実体。実ピクセルで確認)。
// - 既存の尺度は両方とも緑だった。`ink_pixels` は「背景でない画素の数」なので 2 本のほうが
//   多い = 増える = 緑。`path_contrast` は曲線上 ±2px の **最大**コントラストなので、
//   縁さえ立っていれば芯が消えても緑。
//   **「見えているか」は測れていたが「何本に見えるか」を誰も測っていなかった。**
//
// よって 2 層を別々に、それぞれ何を数えているかを明示して数える。

/// 4 点 (= 3 区間) の bend 用レーン。**中間区間**を掴むと `flatten_clip_curve` の run 分割
/// (前後 2 本に割れる枝) を通る — 点 2 つの fixture ではこの枝を一度も実行できない。
///
/// 値は帯の中央寄り (0.75〜1.3 / Volume は 0..2 なので上下の縁から 20px 以上内側) に置き、
/// レーンの既定値ガイド線は `0.0` = 帯の下端へ逃がしてある。曲線がクリップ枠やガイド線と
/// 近いと「何本か」の意味が曖昧になるため — **数える対象を取り違えないための fixture 条件**。
fn add_bend_lane(app: &mut AppData) {
    let bez = |t: f32| AutomationCurve::Bezier { tension: t };
    app.edit_song(|song| {
        song.tracks.clear();
        song.clip_contents.insert(
            CONTENT_ID,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint {
                        id: 1,
                        time_beat: 0.0,
                        value: 0.75,
                        curve: AutomationCurve::Linear,
                    },
                    AutomationPoint { id: 2, time_beat: 2.0, value: 1.30, curve: bez(0.5) },
                    AutomationPoint { id: 3, time_beat: 4.0, value: 0.80, curve: bez(-0.5) },
                    AutomationPoint { id: 4, time_beat: 6.0, value: 1.25, curve: bez(0.5) },
                ],
                next_point_id: 5,
            }),
        );
        song.tracks.push(track_with(|t| {
            t.id = 1;
            t.automation_lanes = vec![AutomationLane {
                id: 1,
                height_px: 80,
                clips: vec![AutomationClip {
                    id: 1,
                    name: String::new(),
                    start_beat: 0.0,
                    length_beats: 8.0,
                    content_id: CONTENT_ID,
                    content_offset_beats: 0.0,
                    color: None,
                }],
                next_clip_id: 2,
                ..AutomationLane::new(
                    AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                    0.0,
                )
            }];
        }));
    });
    app.ui_prefs.expanded_automation_tracks.insert(1);
}

fn pointer_at(x: f32, y: f32, m: Modifiers) -> PointerFrame {
    PointerFrame { pos: Some((x, y)), modifiers: m, ..PointerFrame::default() }
}

fn pointer_down(x: f32, y: f32, m: Modifiers) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_just_pressed: true,
        primary_pressed: true,
        modifiers: m,
        ..PointerFrame::default()
    }
}

fn pointer_hold(x: f32, y: f32, m: Modifiers) -> PointerFrame {
    PointerFrame { pos: Some((x, y)), primary_pressed: true, modifiers: m, ..PointerFrame::default() }
}

fn pointer_up(x: f32, y: f32, m: Modifiers) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_just_released: true,
        modifiers: m,
        ..PointerFrame::default()
    }
}

/// **Scene 層 (幾何)**: `band` の中で x = `x_at` を跨ぐ **不透明な** (alpha >= 0.5) 線分の y。
/// ±1.5px は 1 本に畳む (縁取りと芯は同じ y に乗るので 1 本と数えるのが正しい)。
/// レーンの既定値ガイド線 (alpha 0.18) は不透明フィルタで落ちる。
///
/// **base curve が preview と別の形で残っていれば 2 つになる** (2c3c668 が守る不変条件)。
fn opaque_line_ys_at(scene: &Scene, band: Rect, x_at: f32) -> Vec<f32> {
    let mut ys: Vec<f32> = scene
        .iter_lines()
        .flat_map(|b| b.segments.iter())
        .filter_map(|s| {
            if s.color.a < 0.5 {
                return None;
            }
            let (x0, x1) = (s.a[0].min(s.b[0]), s.a[0].max(s.b[0]));
            if x_at < x0 || x_at > x1 {
                return None;
            }
            let y = if (s.b[0] - s.a[0]).abs() < 1e-6 {
                s.a[1]
            } else {
                let t = (x_at - s.a[0]) / (s.b[0] - s.a[0]);
                s.a[1] + (s.b[1] - s.a[1]) * t
            };
            (y >= band.y && y <= band.y + band.h).then_some(y)
        })
        .collect();
    ys.sort_by(f32::total_cmp);
    let mut clusters: Vec<f32> = Vec::new();
    for y in ys {
        if clusters.last().is_none_or(|last| (y - last).abs() > 1.5) {
            clusters.push(y);
        }
    }
    clusters
}

/// **pixel 層 (見え方)**: 列 `x` の `[y0, y1)` を走査し、「背景色に戻る画素で区切られた
/// インクの連続」を返す。背景はその列の最頻色。
///
/// `arr_widget.rs::line_positions_at` の「±1.5px を 1 本に畳む」とは **正反対の目的**である
/// (あちらは重ね塗りを 1 本と数えるため、こちらは中抜けを 2 本と数えるため)。
///
/// 閾値 24 は「AA の裾はまだインク、塗りそのものに戻ったら背景」の線引き。中抜けした
/// ストロークの隙間は **塗りと完全に同一の画素** (差 0) になるので閾値の取り方に依らず必ず
/// 割れる。逆に閾値を上げすぎると AA の裾で偽の分割が出る (実測で確認済)。
fn visible_strokes(rgba: &[u8], x: u32, y0: u32, y1: u32) -> Vec<(u32, u32)> {
    let mut hist: HashMap<[u8; 3], usize> = HashMap::new();
    let mut col: Vec<[u8; 3]> = Vec::new();
    for y in y0..y1 {
        let i = ((y * W + x) * 4) as usize;
        let c = [rgba[i], rgba[i + 1], rgba[i + 2]];
        *hist.entry(c).or_insert(0) += 1;
        col.push(c);
    }
    let bg = *hist.iter().max_by_key(|(_, n)| **n).expect("列が空でない").0;
    let dist = |c: [u8; 3]| {
        let f = |a: u8, b: u8| (i32::from(a) - i32::from(b)).abs();
        f(c[0], bg[0]) + f(c[1], bg[1]) + f(c[2], bg[2])
    };
    let mut runs: Vec<(u32, u32)> = Vec::new();
    let mut start: Option<u32> = None;
    for (i, c) in col.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let y = y0 + i as u32;
        if dist(*c) > 24 {
            start.get_or_insert(y);
        } else if let Some(s) = start.take() {
            runs.push((s, y));
        }
    }
    if let Some(s) = start {
        runs.push((s, y1));
    }
    runs
}

/// 2 層をまとめて「この x では線が 1 本」を主張する。
/// 失敗時は **測った値をそのまま**出す (「2 本だった」で終わらせない)。
fn assert_single_stroke(scene: &Scene, rgba: &[u8], band: Rect, x: f32, what: &str) {
    let ys = opaque_line_ys_at(scene, band, x);
    assert_eq!(ys.len(), 1, "{what}: 幾何として x={x} に線が {} 本ある: {ys:?}", ys.len());
    // 芯の周り ±8px だけを見る (レーンの既定値ガイド線 / クリップ枠を数え込まないため)。
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let xi = x as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let y0 = (ys[0] - 8.0).max(0.0) as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let y1 = (ys[0] + 8.0).min(H as f32) as u32;
    let runs = visible_strokes(rgba, xi, y0, y1);
    // 失敗時は **その列の実画素**まで出す。「2 本だった」で終わると、次に読む人が
    // また同じ推測から始めることになる。
    let column: Vec<(u32, [u8; 3])> = (y0..y1)
        .map(|y| {
            let i = ((y * W + xi) * 4) as usize;
            (y, [rgba[i], rgba[i + 1], rgba[i + 2]])
        })
        .collect();
    assert_eq!(
        runs.len(),
        1,
        "{what}: 見た目が x={x} で {} 本に割れている (芯が塗りに沈んで縁だけ残った): \
         y={:.1} 近傍の連続インク {runs:?}\n  実画素: {column:?}",
        runs.len(),
        ys[0]
    );
}

/// 中間区間の上を x 方向に等間隔で拾った標本点。曲線の **実描画**から取るので、
/// レイアウト式をテスト側に複製しない。
fn middle_segment_samples(scene: &Scene) -> Vec<(f32, f32)> {
    let b = automation_curve_batch(scene);
    let pts: Vec<(f32, f32)> = b
        .segments
        .iter()
        .map(|s| ((s.a[0] + s.b[0]) * 0.5, (s.a[1] + s.b[1]) * 0.5))
        .collect();
    let (x0, x1) = (pts[0].0, pts[pts.len() - 1].0);
    // 4 点 = 3 等分。中央の区間 (1/3 .. 2/3) の内側を 5 点。
    let (lo, hi) = (x0 + (x1 - x0) * 0.38, x0 + (x1 - x0) * 0.62);
    (0..5)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let target = lo + (hi - lo) * (i as f32 / 4.0);
            *pts.iter()
                .min_by(|a, b| (a.0 - target).abs().total_cmp(&(b.0 - target).abs()))
                .expect("曲線に点がある")
        })
        .collect()
}

/// **r.md #73 の回帰**: Alt hover の強調が 1 本に見える (2 本に割れない)。
#[test]
fn alt_hover_emphasis_is_a_single_stroke() {
    for theme in ["dark", "light"] {
        for selected in [false, true] {
            check_single_stroke(theme, selected, false);
        }
    }
}

/// **r.md #73 の回帰**: 曲げている最中の preview が 1 本に見える。
///
/// 「base curve が残って 2 本」は Scene 層が、「芯が塗りに沈んで縁だけ 2 本残る」は
/// pixel 層が捕まえる。
#[test]
fn bend_preview_is_a_single_stroke() {
    for theme in ["dark", "light"] {
        for selected in [false, true] {
            check_single_stroke(theme, selected, true);
        }
    }
}

fn check_single_stroke(theme_id: &str, select_clip: bool, do_drag: bool) {
    let (mut app, _rx) = build_app();
    app.handle_event(AppEvent::SetTheme(theme_id.to_string()));
    assert_eq!(app.theme.id, theme_id, "テーマが適用されている");
    add_bend_lane(&mut app);
    if select_clip {
        app.selection.selected_automation_clips =
            vec![common::model::AutomationClipKey { track: 1, lane: 1, clip: 1 }];
    }
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    if host.set_palette(app.theme.core.clone()) {
        host.invalidate_scene_cache();
    }
    let Ok(mut renderer) = OffscreenRenderer::new(W, H) else {
        eprintln!("skip pixel check: no GPU adapter/device");
        return;
    };
    let tag = format!(
        "[{theme_id}/{}/{}]",
        if select_clip { "選択中" } else { "非選択" },
        if do_drag { "ドラッグ" } else { "hover" }
    );
    let alt_mods = Modifiers { alt: true, ..Modifiers::empty() };

    let first = drive_root(&mut host, &mut app, PointerFrame::default());
    let band = automation_curve_batch(&first).clip_rect.expect("曲線には clip がある");
    let samples = middle_segment_samples(&first);

    for (i, &(gx, gy)) in samples.iter().enumerate() {
        // 前提: 何も触っていないときは 1 本。ここが崩れたら数えている対象が違う。
        let plain = drive_root(&mut host, &mut app, pointer_at(gx, gy, Modifiers::empty()));
        let plain_px = renderer.render_to_rgba(&plain).expect("offscreen render");
        assert_single_stroke(&plain, &plain_px, band, gx, &format!("{tag} 前提 (標本 {i})"));

        if do_drag {
            drive_root(&mut host, &mut app, pointer_down(gx, gy, alt_mods));
            let dragging = drive_root(&mut host, &mut app, pointer_hold(gx, gy - 15.0, alt_mods));
            let px = renderer.render_to_rgba(&dragging).expect("offscreen render");
            assert_single_stroke(&dragging, &px, band, gx, &format!("{tag} ドラッグ中 (標本 {i})"));
            // release フレームも 1 本 (ゴーストが消えて base が戻る 1 フレームの抜けが無い)。
            let released = drive_root(&mut host, &mut app, pointer_up(gx, gy - 15.0, alt_mods));
            let rpx = renderer.render_to_rgba(&released).expect("offscreen render");
            assert_single_stroke(&released, &rpx, band, gx, &format!("{tag} release (標本 {i})"));
        } else {
            let hovered = drive_root(&mut host, &mut app, pointer_at(gx, gy, alt_mods));
            let px = renderer.render_to_rgba(&hovered).expect("offscreen render");
            // 強調が本当に乗っているか (空振りだと「1 本」で緑になる)。
            assert!(
                total_line_segments(&hovered) > total_line_segments(&plain),
                "{tag} 標本 {i}: 強調が乗っていない = 線の上を狙えていない (空振り)"
            );
            assert_single_stroke(&hovered, &px, band, gx, &format!("{tag} Alt hover (標本 {i})"));
        }
    }
}
