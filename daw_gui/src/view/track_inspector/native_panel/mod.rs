//! Rack の内蔵 device の Par パネル (Q12 / Q13 / Q14、`docs/plan_rack_native_devices.md` §10.6 / §10.7)。
//!
//! 種類ごとに 1 ファイル (EQ / Comp / Bus Comp / Tone EQ / Reverb / Delay / master Limiter)。格子の寸法は [`layout`] が
//! 持ち、描画と行高 ([`layout::panel_height`]) が同じ定数を読む。つまみ・カーブ・GR メーターは
//! `view::native_device` の共有部品 (Mixer 帯 / マスターパネルと同じ関数) を呼ぶだけで、値の
//! 住所・live 値・変調・ジェスチャー・自動 ON はそちらと handler が持つ。

mod bus_comp;
mod cell;
mod comp;
mod delay;
mod eq;
mod eq_graph;
pub mod layout;
mod limiter;
mod reverb;
mod tone_eq;

use common::model::{NativeDevice, NativeKind};
use daw_ui_core::Ui;
use daw_ui_renderer::{Color, Rect};

use crate::app::AppData;
use crate::handler::view_model::LiveParamScope;
use crate::view::native_device::ParamOwner;

pub(super) use limiter::draw_limiter_panel;

/// Par 1 枚を描く文脈。
pub(super) struct PanelCtx<'a> {
    /// Song の値 (構造 / bypass の静的な状態)。
    pub dev: &'a NativeDevice,
    /// レーンを重ねた表示値 (カーブの形と点の位置)。
    pub live: NativeDevice,
    pub owner: ParamOwner<'a>,
    pub scope: &'a LiveParamScope,
    /// Par の矩形 (行の 26px の直下、行の中身と同じ幅)。
    pub rect: Rect,
    /// Par の面の色 (つまみのリングのくり抜き色)。
    pub bg: Color,
}

/// `ctx.dev` の種類の Par を描く。
pub(super) fn draw_native_panel(app: &AppData, ui: &mut Ui<'_, AppData>, ctx: &PanelCtx<'_>) {
    match ctx.dev.kind() {
        NativeKind::Eq => eq::draw(app, ui, ctx),
        NativeKind::Comp => comp::draw(app, ui, ctx),
        NativeKind::BusComp => bus_comp::draw(app, ui, ctx),
        NativeKind::ToneEq => tone_eq::draw(app, ui, ctx),
        NativeKind::Reverb => reverb::draw(app, ui, ctx),
        NativeKind::Delay => delay::draw(app, ui, ctx),
    }
}
