//! インスペクタのチェーン行を **展開したとき** に出る param パネル本体 (plugin / 映像 FX /
//! VOICEVOX の口パク先 / Transform。内蔵 device の Par は `native_panel`)。
//!
//! r.md #158: クリップの設定 (歌声の声 / 読み上げ / 字幕の文字と書式) はクリップの帯 (`clip_band`) へ
//! 移した。Par に残すのはデバイスそのものの設定だけ (字幕デバイスは Par を持たない)。
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

mod group_transform;
mod lipsync;
mod plugin_params;
mod video_fx;

use super::*;

/// Par 1 枚を描く文脈。
#[derive(Debug, Clone, Copy)]
pub(super) struct PanelCtx {
    /// Par を開いた device。
    pub device_id: u64,
    /// Par の中身の左端 (行の名前と同じ) と幅。
    pub x: f32,
    pub w: f32,
    /// このセクションを描き始める y。
    pub y: f32,
    /// 帯の欄の左端 (値の欄の x を帯の欄と揃える、`form::cols_nested`)。
    pub form_left: f32,
}

impl PanelCtx {
    /// この Par の欄の列 (ラベルは Par の左端から、値の欄は帯の欄と同じ x)。
    pub fn cols(&self) -> form::FormCols {
        form::cols_nested(self.x, self.w, self.form_left)
    }
}

/// Par を開いた device の param パネルを描き、消費後の `y` を返す。
///
/// **この順序が画面の上下順**。 セクションはそれぞれ自分の gate で「出す / 出さない」を
/// 決めるので、 出ないセクションは `y` を素通しする。各セクションは欄 (ラベル左・値右、`form`) を
/// [`layout::FORM_ROW_PITCH`] で積むだけで、見出しは描かない (行の名前と同じ見出しを繰り返さない、
/// r.md #158 仕様 9)。上下の余白 ([`layout::PAR_PAD`]) はここが持つ。中身が無ければ高さ 0。
pub(super) fn draw_device_panel(app: &AppData, ui: &mut Ui<'_, AppData>, ctx: PanelCtx) -> f32 {
    let top = ctx.y;
    let mut ctx = PanelCtx { y: top + layout::PAR_PAD, ..ctx };
    let start = ctx.y;
    ctx.y = group_transform::draw_group_transform(app, ui, ctx);
    ctx.y = video_fx::draw_video_fx_params(app, ui, ctx);
    ctx.y = plugin_params::draw_plugin_params(app, ui, ctx);
    let end = lipsync::draw_lipsync_target(app, ui, ctx);
    if end <= start {
        return top;
    }
    // 最後の行の送りの余り (行と行の間) を下の余白に含める。
    end - (layout::FORM_ROW_PITCH - layout::FORM_ROW_H) + layout::PAR_PAD
}

/// Par を開いた device が `plugin_id` の plugin か (VOICEVOX の専用欄の gate)。
fn opened_plugin_is(app: &AppData, device_id: u64, plugin_id: &str) -> bool {
    app.cur.song_doc.plugin_by_id(device_id).is_some_and(|p| p.plugin_id == plugin_id)
}
