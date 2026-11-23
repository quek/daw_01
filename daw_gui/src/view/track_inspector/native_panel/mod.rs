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

/// スクロールの viewport の中にカーブを描いた EQ / Tone EQ の Par (描いた順 = Rack の上から、
/// [`MAX_DEVICE_SCOPES`](common::device_scope_bridge::MAX_DEVICE_SCOPES) 個まで)。engine にスペクトラムを計算させる
/// のは、実際に見える所に描いたものだけ (`AppData::wanted_device_scopes`): 帯を畳んだ / インスペクタを隠した /
/// スクロールで外れた Par は描かれない (外れた) ので要求しない。上限は「一度に描ける EQ の数」なので、画面の外の
/// Par が見えている Par の枠を奪うことも無い。描画は状態を書かず、集めたものを `track_inspector::draw` が
/// 1 回だけ公開する (Q の宛先と同じ)。
#[derive(Default)]
pub(super) struct EqScopes(std::cell::RefCell<Vec<u64>>);

impl EqScopes {
    /// EQ `id` のカーブを `rect` に描いた (いまの scissor = viewport と重なっていれば見えている)。
    fn note_drawn(&self, ui: &Ui<'_, AppData>, id: u64, rect: Rect) {
        let visible = ui.current_clip_rect().is_none_or(|clip| !clip.intersect(rect).is_empty());
        let mut ids = self.0.borrow_mut();
        if visible && ids.len() < common::device_scope_bridge::MAX_DEVICE_SCOPES && !ids.contains(&id) {
            ids.push(id);
        }
    }

    /// 集めた id (描き終えたとき)。
    pub(super) fn take(&self) -> Vec<u64> {
        self.0.take()
    }
}

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
    /// スペクトラムを要求する EQ を集める (EQ / Tone EQ のカーブが描いたときに積む)。
    pub eq_scopes: &'a EqScopes,
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
