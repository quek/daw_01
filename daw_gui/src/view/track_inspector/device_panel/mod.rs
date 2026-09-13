//! インスペクタのチェーン行を **展開したとき** に出る param パネル本体 (plugin / 映像 FX /
//! VOICEVOX / 字幕 / Transform。内蔵 device の Par は `native_panel`)。
//!
//! plugin 行の展開 (`plugin_row.rs`) がここ 1 本 ([`draw_device_panel`]) を呼ぶ。
//!
//! **セクションごとに 1 ファイル**にしてあるのは不変条件 9 (サイズ budget) の
//! ため。 元は `track_inspector/mod.rs::draw` の中に埋まった 1 個のクロージャで、
//! r.md #71 でここへ出したときも 1 関数 1,122 実コード行 = 関数 budget (300 行) の
//! 3.7 倍だった。 r.md #76 が測り方を物理行から実コード行へ変えてそれが表に出たので、
//! **baseline へ逃がさずセクション軸で割った** (「超過したら分割してから足す」)。
//!
//! 各セクションの contract は「`(app, ui, ctx: PanelCtx) -> 次の y`」(r.md #129 §10.5)。
//! `ctx` は Par を開いた device と、その行の中身と同じ x / 幅 (Parallel の中ならインデントに追従) と
//! 起点 y。**gate は各セクションが自分で持つ** (その device が VOICEVOX か等) ので、ここは順に呼ぶ
//! だけ = 並び順がそのまま画面の上下順になる。Par が開いているか / 表示中のチェーンの行かは、
//! 呼び出し側 (行が描かれて Par が開いているときだけ呼ぶ) が決めている。
//!
//! widget id と undo bracket の鍵には必ず `device_id` を入れる — 同じ種類の Par を 2 枚開いたとき、
//! 片方のドラッグがもう片方の入力状態や bracket を奪わないため (§18-AA)。

mod clip_voice;
mod group_transform;
mod lipsync;
mod plugin_params;
mod talk;
mod text_event;
mod video_fx;

use super::*;

/// Par 1 枚を描く文脈。
#[derive(Debug, Clone, Copy)]
pub(super) struct PanelCtx {
    /// Par を開いた device。
    pub device_id: u64,
    /// 行の中身の左端と幅。
    pub x: f32,
    pub w: f32,
    /// このセクションを描き始める y。
    pub y: f32,
}

/// Par を開いた device の param パネルを描き、消費後の `y` を返す。
///
/// **この順序が画面の上下順**。 セクションはそれぞれ自分の gate で「出す / 出さない」を
/// 決めるので、 出ないセクションは `y` を素通しする。
pub(super) fn draw_device_panel(app: &AppData, ui: &mut Ui<'_, AppData>, ctx: PanelCtx) -> f32 {
    let mut ctx = ctx;
    ctx.y = group_transform::draw_group_transform(app, ui, ctx);
    ctx.y = video_fx::draw_video_fx_params(app, ui, ctx);
    ctx.y = plugin_params::draw_plugin_params(app, ui, ctx);
    ctx.y = text_event::draw_text_event(app, ui, ctx);
    ctx.y = clip_voice::draw_clip_voice(app, ui, ctx);
    ctx.y = talk::draw_talk(app, ui, ctx);
    lipsync::draw_lipsync_target(app, ui, ctx)
}

/// Par を開いた device が `plugin_id` の plugin か (VOICEVOX / 字幕の専用セクションの gate)。
fn opened_plugin_is(app: &AppData, device_id: u64, plugin_id: &str) -> bool {
    app.cur.song_doc.song().plugin_by_id(device_id).is_some_and(|p| p.plugin_id == plugin_id)
}
