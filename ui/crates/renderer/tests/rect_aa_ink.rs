//! r.md #53: rect パイプラインの AA が **インク保存** であることの GPU pixel verify。
//!
//! 症状は「再生でオートスクロールにするとクリップの左右がチラつく」。 root cause は
//! `rect.wgsl` のボーダー帯 AA が
//! `border_alpha = 1 - smoothstep(bw*0.5 - 1, bw*0.5, |d + bw*0.5|)` で、 `bw = 1.0`
//! (= クリップ枠 / セクション枠 / オートメーション点) では plateau に到達できず、
//! サブピクセル位相によって枠の総インク量が 0 〜 0.5 を往復していたこと
//! (位相 0.5 付近では **枠が丸ごと消える**)。 追従スクロールは位相を毎フレーム
//! 動かすので、 これが左右の縦辺だけの明滅として見えていた (上下辺は y が動かない
//! ので位相固定 = 安定、 という左右非対称性もこれで説明できる)。
//!
//! ここでは「矩形を 0.1px ずつ横にずらして描き、 縦辺の総インク量が位相によらず
//! `border_width` に一致する」 ことを readback pixel で確認する。 修正前は必ず落ちる。
//!
//! 同じバグクラスは line パイプラインで既に修正済 (`line.wgsl:51-54` / `:74-77`、
//! 「bar grid の特定 bar だけ消える」)。 rect だけ取り残されていた。
//!
//! GPU adapter が無い環境では graceful skip する (composite.rs と同じ idiom)。

use daw_ui_renderer::{Color, OffscreenRenderer, Rect, RectCommand, Scene};

const W: u32 = 256;
const H: u32 = 64;
/// 矩形の上下端から十分離れた測定行 (縦辺だけを見るため)。
const PROBE_Y: u32 = 32;
const RECT_Y: f32 = 8.0;
const RECT_H: f32 = 48.0;
const RECT_W: f32 = 100.0;
const BORDER_W: f32 = 1.0;

fn try_renderer() -> Option<OffscreenRenderer> {
    match OffscreenRenderer::new(W, H) {
        Ok(r) => Some(r),
        Err(e) => {
            eprintln!("skip rect AA GPU test: no adapter/device ({e})");
            None
        }
    }
}

/// sRGB 8bit → linear。 render target は `Rgba8UnormSrgb` なので、 被覆率 (= 面積) を
/// 足し合わせるには linear に戻す必要がある。
fn srgb_to_linear(byte: u8) -> f64 {
    let c = f64::from(byte) / 255.0;
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// 黒背景 / 黒 fill / 白ボーダーの矩形を `x` に描き、 `PROBE_Y` 行の linear 輝度列を返す。
/// 背景も fill も黒なので、 非ゼロなのはボーダーの被覆分だけ = そのままインク量になる。
fn probe_row(r: &mut OffscreenRenderer, x: f32) -> Vec<f64> {
    let mut scene = Scene::new();
    scene.clear_color = Color::BLACK.to_wgpu();
    scene.push_rect(RectCommand {
        rect: Rect::new(x, RECT_Y, RECT_W, RECT_H),
        fill: Color::BLACK,
        border: Color::WHITE,
        border_width: BORDER_W,
        radius: [0.0; 4],
        clip_rect: None,
    });
    let bytes = r.render_to_rgba(&scene).expect("render");
    (0..W)
        .map(|px| {
            let i = ((PROBE_Y * W + px) * 4) as usize;
            srgb_to_linear(bytes[i])
        })
        .collect()
}

fn ink(row: &[f64], from: u32, to: u32) -> f64 {
    row[from as usize..to as usize].iter().sum()
}

/// 縦辺の総インク量は矩形のサブピクセル位相によらず `border_width` に一致する。
///
/// 旧実装は位相 0 で 0.25 (α² 減光込み)、 位相 0.5 で **0.0** (= 枠消失) だった。
#[test]
fn vertical_border_ink_is_phase_invariant() {
    let Some(mut r) = try_renderer() else { return };

    let base_x = 100.0_f32;
    let mut left_inks = Vec::new();
    let mut right_inks = Vec::new();
    for step in 0..10 {
        let x = base_x + step as f32 * 0.1;
        let row = probe_row(&mut r, x);
        // 左辺 / 右辺それぞれの周囲 ±3px を積分する (帯は最大 2 列に分かれる)。
        left_inks.push(ink(&row, 96, 104));
        right_inks.push(ink(&row, 196, 204));
    }

    for (i, &v) in left_inks.iter().enumerate() {
        assert!(
            (v - f64::from(BORDER_W)).abs() < 0.06,
            "left border ink at phase {:.1} = {v:.4}, expected {BORDER_W} \
             (rect.wgsl の AA がインク保存でない)\nleft={left_inks:?}",
            i as f32 * 0.1
        );
    }
    for (i, &v) in right_inks.iter().enumerate() {
        assert!(
            (v - f64::from(BORDER_W)).abs() < 0.06,
            "right border ink at phase {:.1} = {v:.4}, expected {BORDER_W}\nright={right_inks:?}",
            i as f32 * 0.1
        );
    }
}

/// どの位相でも枠は必ず見える (= 位相 0.5 で消えない)。 チラつきの主観症状に一番近い assertion。
#[test]
fn vertical_border_never_disappears() {
    let Some(mut r) = try_renderer() else { return };

    for step in 0..10 {
        let x = 100.0 + step as f32 * 0.1;
        let row = probe_row(&mut r, x);
        let peak = row[96..104].iter().copied().fold(0.0_f64, f64::max);
        assert!(
            peak > 0.35,
            "phase {:.1} で左枠のピーク輝度が {peak:.4} — 枠が消えかけている",
            step as f32 * 0.1
        );
    }
}

/// fill も被覆保存であること (= エッジが 0.5px 内側に侵食されない / α² で暗くならない)。
/// 不透明 fill の矩形を描き、 左端周辺の輝度積分が「はみ出しゼロ」 になることを見る。
#[test]
fn fill_edge_is_coverage_exact() {
    let Some(mut r) = try_renderer() else { return };

    // border 無し・白 fill。 x = 100.5 (= 最悪位相) で左端 2 列が 0.5 / 1.0 になるはず。
    let mut scene = Scene::new();
    scene.clear_color = Color::BLACK.to_wgpu();
    scene.push_rect(RectCommand::uniform_radius(
        Rect::new(100.5, RECT_Y, RECT_W, RECT_H),
        Color::WHITE,
        0.0,
    ));
    let bytes = r.render_to_rgba(&scene).expect("render");
    let at = |px: u32| {
        let i = ((PROBE_Y * W + px) * 4) as usize;
        srgb_to_linear(bytes[i])
    };

    assert!(at(99) < 0.02, "矩形の外 (99) が塗られている: {}", at(99));
    assert!(
        (at(100) - 0.5).abs() < 0.06,
        "境界列 (100) の被覆が 0.5 でない: {} (旧実装は片側ランプ + α² で 0.09 程度)",
        at(100)
    );
    assert!(at(101) > 0.94, "内側 1 列目 (101) が満たない: {}", at(101));
}
