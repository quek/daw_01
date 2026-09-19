//! Reverb の Par: `[Frz]` → `ROOM` 6 セル → `TONE / MOD / OUT` 6 セル
//! (`docs/plan_rmd_134_135_reverb_delay.md` §8.1)。

use common::model::{NativeParamId, ReverbParam};
use daw_ui_core::Ui;

use super::PanelCtx;
use super::cell::{head_labels, param_cell, param_switch, section_label};
use super::layout::{BAR_H, CELL, HEAD, PAD, ROW_GAP, SECTION_H};
use crate::app::AppData;

/// 上段 (部屋の性質)。
const ROOM: [ReverbParam; 6] = [
    ReverbParam::Predelay,
    ReverbParam::Size,
    ReverbParam::Decay,
    ReverbParam::Damp,
    ReverbParam::LfDamp,
    ReverbParam::Diffusion,
];
/// 下段 (音色・揺れ・出力)。
const TONE: [ReverbParam; 6] = [
    ReverbParam::LowCut,
    ReverbParam::HighCut,
    ReverbParam::ModRate,
    ReverbParam::ModDepth,
    ReverbParam::Width,
    ReverbParam::Mix,
];

pub(super) fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, ctx: &PanelCtx<'_>) {
    let key = common::model::RackPanelKey::Device(ctx.dev.id);
    let mut y = ctx.rect.y + PAD;

    param_switch(app, ui, ctx, key, "freeze", ctx.rect.x, y, "Frz", NativeParamId::Reverb(ReverbParam::Freeze));
    y += BAR_H + ROW_GAP;

    for (part, title, row) in [("sec_room", "ROOM", &ROOM), ("sec_tone", "TONE / MOD / OUT", &TONE)] {
        section_label(app, ui, ctx, key, part, y, title, None);
        y += SECTION_H;
        head_labels(app, ui, ctx, key, y, &(*row).map(ReverbParam::label));
        y += HEAD;
        for (col, p) in row.iter().enumerate() {
            param_cell(app, ui, ctx, NativeParamId::Reverb(*p), col, y, false, false);
        }
        y += CELL + ROW_GAP;
    }
}
