//! VOICEVOX の wav 合成 / 口パク生成の進行状態を見せる **非ブロック**
//! overlay (画面上端中央)。`load_overlay` と同じ idiom (= modal でない、操作を妨げない)。
//!
//! - WAV 合成中 (= builtin VOICEVOX が busy なトラックあり) → 回転スピナー +
//!   「VOICEVOX 合成中…  残り N トラック」。
//! - 口パク生成中 (= `lipsync_inflight` 非空) → スピナー + 「口パク生成中…」。
//! - engine 未接続が確定 (= `Unreachable` が `VOICEVOX_ENGINE_WARNING` 以上継続) → static の
//!   amber 警告「VOICEVOX エンジンに接続できません / エンジンを起動してください」に切替。
//! - 内容エラー (= engine は到達できたが歌詞等を `Rejected`) → static の amber 警告
//!   「合成できない歌詞があります / <理由>」。engine 起動を促す警告とは区別する。
//!
//! HTTP は中間進捗を返さないので **percent は出さない** (indeterminate のスピナー +
//! 残件数のみ)。完了時はスピナーが静かに消えるだけ (= 反映完了の合図、grill-me 確定)。
//!
//! クリップ上スピナー (`arrangement_view`) と共通の `draw_spinner` を提供する。

use std::time::Duration;

use daw_ui_core::Ui;
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, Rect};

use crate::app::AppData;
use crate::theme::Theme;

/// カード面の透過率。load_overlay と同じ非モーダル progress カード idiom:
/// elevation-2 の `panel_raised` をこの alpha で透過させ、背後の作業画面を透かす。
/// 面そのものはテーマ従属なので、色はフレームごとに `ui.palette()` から取る
/// (module-level const にすると起動時のテーマで固定され、切替に追従しない)。
const OVERLAY_BG_ALPHA: f32 = 0.94;

/// スピナー 1 回転の周期。
pub const SPINNER_PERIOD: Duration = Duration::from_millis(900);

const FONT: f32 = 13.0;
const LINE_H: f32 = 18.0;
const PAD: f32 = 12.0;
const PANEL_W: f32 = 320.0;

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, screen: PhysicalSize) {
    // 何も生成していない idle フレーム (= 大多数) は track 走査せず即 return。
    if app.voicevox.voicevox_synth_status.is_empty() && app.voicevox.lipsync_inflight.is_empty() {
        return;
    }
    // render_frame が frame 冒頭で確定した時刻を使う (= 再描画継続判定
    // `voicevox_animating` と同じ now を読み、5s 境界での食い違いを防ぐ)。
    let now = app.ui_ephemeral.frame_now;
    let unreachable = app.voicevox_engine_unreachable(now);
    let rejected = app.voicevox_rejected_detail();
    let wav_n = app.voicevox_synth_busy_count();
    let lipsync = !app.voicevox.lipsync_inflight.is_empty();

    // engine 未接続 / 内容エラー (拒否された歌詞) が確定したら、進行中スピナーより
    // 警告を優先表示する。
    if !unreachable && rejected.is_none() && wav_n == 0 && !lipsync {
        return;
    }

    // load_overlay (上端中央) と重ならないよう、それが出ているときは下にずらす。
    let load_active =
        matches!(app.media.load_progress, Some((_, total)) if total > 0) || app.is_async_save_pending();
    let base_y = if load_active { 12.0 + 54.0 } else { 12.0 };

    let phase = spinner_phase(now.duration_since(app.ui_ephemeral.anim_epoch), SPINNER_PERIOD);

    if unreachable {
        draw_warning_panel(ui, &app.theme, screen, base_y);
        return;
    }

    // engine には到達済だが歌詞等が拒否された内容エラー。engine 起動を促す警告とは別。
    if let Some(detail) = rejected {
        draw_rejected_panel(ui, &app.theme, screen, base_y, detail);
        return;
    }

    // 進行中: スピナー + 1〜2 行のラベル。
    let mut lines: Vec<String> = Vec::with_capacity(2);
    if wav_n > 0 {
        lines.push(format!("VOICEVOX 合成中\u{2026}  残り {wav_n} トラック"));
    }
    if lipsync {
        lines.push("口パク生成中\u{2026}".to_string());
    }
    draw_progress_panel(ui, screen, base_y, phase, &lines);
}

/// 進行中パネル: 左に回転スピナー、右に 1〜2 行のラベル。
fn draw_progress_panel(
    ui: &mut Ui<'_, AppData>,
    screen: PhysicalSize,
    base_y: f32,
    phase: f32,
    lines: &[String],
) {
    let p = ui.palette();
    let n = lines.len().max(1) as f32;
    let h = PAD * 2.0 + n * LINE_H;
    let x = ((screen.width as f32) - PANEL_W) * 0.5;
    let y = base_y;
    ui.panel(
        "vox_overlay_bg",
        Rect { x, y, w: PANEL_W, h },
        p.panel_raised.with_alpha(OVERLAY_BG_ALPHA),
        6.0,
    );

    let spin_cx = x + PAD + 9.0;
    let spin_cy = y + h * 0.5;
    // 進捗系インタラクションは accent (= theme で progress fill に統一)。
    // 背景はパレット自身のクローム面なので、テーマ従属の accent で常にコントラストが立つ。
    draw_spinner(ui, "vox_overlay_spin", spin_cx, spin_cy, 9.0, phase, p.accent);

    let text_x = x + PAD + 28.0;
    let mut ty = y + PAD;
    for (i, line) in lines.iter().enumerate() {
        // カード面 (panel_raised) の上の本文なので極性固定インクではなく `text`。
        ui.label_at(("vox_overlay_line", i), line, text_x, ty, FONT, p.text);
        ty += LINE_H;
    }
}

/// engine 未接続の static 警告パネル。再描画は止まっているので回転なし。面は寒色 panel の
/// まま、警告は DAW の warning-yellow (`daw.solo`) のドット + 見出しで伝える
/// (彩度は機能色だけ)。`daw.solo` は core パレットに無いので `Theme` を受け取る。
fn draw_warning_panel(
    ui: &mut Ui<'_, AppData>,
    theme: &Theme,
    screen: PhysicalSize,
    base_y: f32,
) {
    let p = &theme.core;
    let h = PAD * 2.0 + 2.0 * LINE_H;
    let x = ((screen.width as f32) - PANEL_W) * 0.5;
    let y = base_y;
    ui.panel(
        "vox_overlay_bg",
        Rect { x, y, w: PANEL_W, h },
        p.panel_raised.with_alpha(OVERLAY_BG_ALPHA),
        6.0,
    );

    // 静的な警告ドット (= アイコン代わり、font glyph 非依存)。
    let dot_r = 5.0;
    ui.panel(
        "vox_overlay_warn_dot",
        Rect { x: x + PAD + 9.0 - dot_r, y: y + PAD + dot_r, w: dot_r * 2.0, h: dot_r * 2.0 },
        theme.daw.solo,
        dot_r,
    );

    let text_x = x + PAD + 28.0;
    ui.label_at(
        "vox_overlay_warn_head",
        "VOICEVOX エンジンに接続できません",
        text_x,
        y + PAD,
        FONT,
        theme.daw.solo,
    );
    ui.label_at(
        "vox_overlay_warn_sub",
        "エンジンを起動してください",
        text_x,
        y + PAD + LINE_H,
        FONT,
        p.text_dim,
    );
}

/// 歌詞などの入力が VOICEVOX に拒否された内容エラーの static 警告パネル。engine には
/// 到達できているので「起動してください」ではなく、直すべき歌詞を提示する。
fn draw_rejected_panel(
    ui: &mut Ui<'_, AppData>,
    theme: &Theme,
    screen: PhysicalSize,
    base_y: f32,
    detail: &str,
) {
    let p = &theme.core;
    let h = PAD * 2.0 + 2.0 * LINE_H;
    let x = ((screen.width as f32) - PANEL_W) * 0.5;
    let y = base_y;
    ui.panel(
        "vox_overlay_bg",
        Rect { x, y, w: PANEL_W, h },
        p.panel_raised.with_alpha(OVERLAY_BG_ALPHA),
        6.0,
    );

    // 静的な警告ドット (アイコン代わり、font glyph 非依存)。
    let dot_r = 5.0;
    ui.panel(
        "vox_overlay_reject_dot",
        Rect { x: x + PAD + 9.0 - dot_r, y: y + PAD + dot_r, w: dot_r * 2.0, h: dot_r * 2.0 },
        theme.daw.solo,
        dot_r,
    );

    let text_x = x + PAD + 28.0;
    // カード内に残る幅で切る。 旧実装の「24 文字で丸める」 は全角 1 文字 =
    // 13.7px なので 24 文字 = 329px となり、 使える 280px を全く制限して
    // いなかった (= 理由文がカード右端を越えて背後のアレンジ画面に描かれた)。
    let text_w = (x + PANEL_W - PAD - text_x).max(1.0);
    ui.label_at_clipped(
        "vox_overlay_reject_head",
        "合成できない歌詞があります",
        Rect { x: text_x, y: y + PAD, w: text_w, h: LINE_H },
        FONT,
        theme.daw.solo,
    );
    // VOICEVOX が返した理由 (例: `lyricが不正です: ー`)。パネル幅に収まるよう省略。
    ui.label_at_clipped(
        "vox_overlay_reject_sub",
        detail,
        Rect { x: text_x, y: y + PAD + LINE_H, w: text_w, h: LINE_H },
        FONT,
        p.text_dim,
    );
}

/// 回転スピナー: `N` 個のドットを円周に並べ、phase に応じて明るさを巡回させる。
/// font glyph に依存しないので、どの環境でも確実に「処理中」を見せられる。
/// `phase` は `[0,1)` で 1 回転 (`spinner_phase` が供給)。`id` は他の widget /
/// 他クリップのスピナーと衝突しない identifier (クリップごとに stable id を渡す)。
pub fn draw_spinner(
    ui: &mut Ui<'_, AppData>,
    id: impl std::hash::Hash + Copy,
    cx: f32,
    cy: f32,
    radius: f32,
    phase: f32,
    color: Color,
) {
    const N: usize = 8;
    let dot_r = (radius * 0.30).max(1.0);
    let head = phase * N as f32;
    for i in 0..N {
        let ang = (i as f32 / N as f32) * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
        let dx = ang.cos() * radius;
        let dy = ang.sin() * radius;
        // head から後方へ離れるほど暗く (= 回転している尾を表現)。
        let behind = (head - i as f32).rem_euclid(N as f32);
        let t = 1.0 - behind / N as f32; // head=1.0 → 一番後ろ ≈ 0.0
        let a = color.a * (0.15 + 0.85 * t);
        let c = Color { a, ..color };
        ui.panel(
            (id, i),
            Rect { x: cx + dx - dot_r, y: cy + dy - dot_r, w: dot_r * 2.0, h: dot_r * 2.0 },
            c,
            dot_r,
        );
    }
}

/// 可変背景 (クリップ色・トラック色・波形) の上に出すスピナー。固定単色を直接
/// 重ねると明クリップ上で白が・暗クリップ上で黒が沈むため、**暗い半透明バッキング
/// チップを敷いてから明色スピナーを重ね**、どのクリップ色でもコントラストを保証する
/// (= over-clip 標識の idiom。新規の clip 上インジケータはこれを流用すること、
/// `feedback_ui_indicator_contrast_on_variable_bg`)。`radius` はスピナー半径、チップは
/// それより一回り大きい円。
///
/// チップ (`scrim`) とドット (`ink_on_dark`) はどちらも **極性固定インク**。ここは
/// パレット面ではなくユーザー着色クリップの上なので、テーマ従属トークンを使うと
/// ライトテーマでチップが明るくなり標識が消える。
pub fn draw_spinner_badge(
    ui: &mut Ui<'_, AppData>,
    id: impl std::hash::Hash + Copy,
    cx: f32,
    cy: f32,
    radius: f32,
    phase: f32,
) {
    let p = ui.palette();
    let chip = radius + 3.0;
    ui.panel(
        (id, "badge_chip"),
        Rect { x: cx - chip, y: cy - chip, w: chip * 2.0, h: chip * 2.0 },
        p.scrim,
        chip, // radius = 半径 → 円
    );
    // 暗チップの上なので「暗背景の上の明インク」= `ink_on_dark`。どのクリップ色でも視認。
    draw_spinner(ui, id, cx, cy, radius, phase, p.ink_on_dark);
}

/// 経過時間 → スピナー位相 `[0,1)`。`period` で 1 回転。純関数 (テスト可能)。
pub fn spinner_phase(elapsed: Duration, period: Duration) -> f32 {
    let p = period.as_secs_f32().max(0.001);
    (elapsed.as_secs_f32() / p).fract()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_phase_wraps_in_unit_interval() {
        let period = Duration::from_millis(1000);
        // (elapsed_ms, expected_phase)
        let cases = [
            (0u64, 0.0f32),
            (250, 0.25),
            (500, 0.5),
            (999, 0.999),
            (1000, 0.0), // ちょうど 1 周 → wrap して 0
            (1250, 0.25), // 2 周目も同じ位相
        ];
        for (ms, expected) in cases {
            let got = spinner_phase(Duration::from_millis(ms), period);
            assert!(
                (got - expected).abs() < 1e-3,
                "elapsed={ms}ms got={got} expected={expected}"
            );
            assert!((0.0..1.0).contains(&got), "phase in [0,1): {got}");
        }
    }
}
