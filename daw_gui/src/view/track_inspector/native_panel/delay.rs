//! Delay の Par: 切り替え帯 2 段 → `TIME` (右に実効 ms) → `FEEDBACK / TONE` → `MOD / OUT`
//! (`docs/plan_rmd_134_135_reverb_delay.md` §8.1)。

use common::model::{DelayDrive, DelayMode, DelayParam, DelayPattern, NativeParamId, NativeParams, RackPanelKey};
use daw_ui_core::Ui;

use super::PanelCtx;
use super::cell::{head_labels, param_cell, param_switch, section_label, selector};
use super::layout::{BAR_H, CELL, HEAD, PAD, ROW_GAP, SECTION_H, SELECTOR_LABEL_W, SELECTOR_W, SWITCH_W};
use crate::app::AppData;

/// 上段: L / R の時間指定。
const TIME: [DelayParam; 6] = [
    DelayParam::DivL,
    DelayParam::TimeL,
    DelayParam::OffsetL,
    DelayParam::DivR,
    DelayParam::TimeR,
    DelayParam::OffsetR,
];
/// 中段: 帰還と音色。
const TONE: [DelayParam; 4] = [DelayParam::Feedback, DelayParam::Cross, DelayParam::Hp, DelayParam::Lp];
/// 下段: 揺れと出力。
const OUT: [DelayParam; 4] = [DelayParam::ModRate, DelayParam::ModDepth, DelayParam::Width, DelayParam::Mix];

/// 切り替えボタンの間隔。
const SWITCH_GAP: f32 = 2.0;
/// セレクタ列の開始 x (`[Sync][Link][Frz]` の右)。
const SELECTOR_X: f32 = 3.0 * (SWITCH_W + SWITCH_GAP) + 8.0;

pub(super) fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, ctx: &PanelCtx<'_>) {
    let NativeParams::Delay(d) = &ctx.dev.params else {
        return;
    };
    let key = RackPanelKey::Device(ctx.dev.id);
    let x = ctx.rect.x;
    let mut y = ctx.rect.y + PAD;

    // 1 段目: ON/OFF 3 つ + Pattern / Mode。
    for (k, (part, label, p)) in [
        ("sync", "Sync", DelayParam::Sync),
        ("link", "Link", DelayParam::Link),
        ("freeze", "Frz", DelayParam::Freeze),
    ]
    .into_iter()
    .enumerate()
    {
        #[allow(clippy::cast_precision_loss)]
        let bx = x + k as f32 * (SWITCH_W + SWITCH_GAP);
        param_switch(app, ui, ctx, key, part, bx, y, label, NativeParamId::Delay(p));
    }
    let sel_x = x + SELECTOR_X;
    selector(app, ui, ctx, key, "pat", sel_x, y, "Pat", NativeParamId::Delay(DelayParam::Pattern), &DelayPattern::LABELS);
    let sel2_x = sel_x + SELECTOR_LABEL_W + SELECTOR_W + 6.0;
    selector(app, ui, ctx, key, "mode", sel2_x, y, "Mode", NativeParamId::Delay(DelayParam::Mode), &DelayMode::LABELS);
    y += BAR_H;
    // 2 段目: Drive。
    selector(app, ui, ctx, key, "drv", sel_x, y, "Drv", NativeParamId::Delay(DelayParam::Drive), &DelayDrive::LABELS);
    y += BAR_H + ROW_GAP;

    // TIME の右端に実効時間を出す (BPM が遅い / 音符値が長いと上限で頭打ちになるのが見える)。
    let bpm = app.cur.song_doc.song().bpm;
    let eff = format!("実効 {:.0} / {:.0} ms", d.effective_secs(false, bpm) * 1000.0, d.effective_secs(true, bpm) * 1000.0);
    section_label(app, ui, ctx, key, "sec_time", y, "TIME", Some(&eff));
    y += SECTION_H;
    head_labels(app, ui, ctx, key, y, &TIME.map(DelayParam::label));
    y += HEAD;
    for (col, p) in TIME.into_iter().enumerate() {
        // 効かないつまみを沈める (値は壊さない = 戻せば元どおり、Comp のモード上書きと同じ規則)。
        // Sync ON なら音符値、OFF なら ms が効く。Link ON なら R 側は L に従う。
        let dimmed = (d.link && col >= 3)
            || match p {
                DelayParam::DivL | DelayParam::DivR => !d.sync,
                DelayParam::TimeL | DelayParam::TimeR => d.sync,
                _ => false,
            };
        param_cell(app, ui, ctx, NativeParamId::Delay(p), col, y, dimmed, false);
    }
    y += CELL + ROW_GAP;

    for (part, title, row) in [("sec_fb", "FEEDBACK / TONE", &TONE), ("sec_out", "MOD / OUT", &OUT)] {
        section_label(app, ui, ctx, key, part, y, title, None);
        y += SECTION_H;
        head_labels(app, ui, ctx, key, y, &(*row).map(DelayParam::label));
        y += HEAD;
        for (col, p) in row.iter().enumerate() {
            // Cross は Pattern = Stereo のときだけ効く。
            let dimmed = *p == DelayParam::Cross && d.pattern != DelayPattern::Stereo;
            param_cell(app, ui, ctx, NativeParamId::Delay(*p), col, y, dimmed, false);
        }
        y += CELL + ROW_GAP;
    }
}
