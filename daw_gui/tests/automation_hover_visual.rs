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
                target: AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                default_value: 1.0,
                enabled: true,
                visible: true,
                height_px: 80,
                clips: vec![AutomationClip {
                    id: 1,
                    name: String::new(),
                    start_beat: 0.0,
                    length_beats: 8.0,
                    content_id: CONTENT_ID,
                    content_offset_beats: 0.0,
                }],
                next_clip_id: 2,
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

/// **r.md #73 の回帰**: Alt hover で automation の曲線が消えない (ダーク)。
#[test]
fn alt_hover_on_the_curve_does_not_erase_it() {
    check_alt_hover_keeps_the_curve("dark");
}

/// 同じことをライトテーマでも見る (レーン背景は可変で、極性が反転する)。
#[test]
fn alt_hover_on_the_curve_does_not_erase_it_in_light_theme() {
    check_alt_hover_keeps_the_curve("light");
}

fn check_alt_hover_keeps_the_curve(theme_id: &str) {
    let (mut app, _rx) = build_app();
    app.handle_event(AppEvent::SetTheme(theme_id.to_string()));
    assert_eq!(app.theme.id, theme_id, "テーマが適用されている");
    add_automation_lane(&mut app);
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
        save(&format!("{theme_id}_plain.png"), &plain_px);
        save(&format!("{theme_id}_alt.png"), &alt_px);
    }
    let plain_ink = ink_pixels(&plain_px, band);
    let alt_ink = ink_pixels(&alt_px, band);
    assert!(plain_ink > 0, "[{theme_id}] 前提: 修飾なしで曲線のピクセルが見えている");
    assert!(
        alt_ink >= plain_ink,
        "[{theme_id}] Alt hover で曲線が消えている (pixel): \
         修飾なし {plain_ink}px → Alt {alt_ink}px"
    );
}
