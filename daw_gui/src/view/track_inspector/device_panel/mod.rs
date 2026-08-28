//! インスペクタのチェーン行を **展開したとき** に出る param パネル本体。
//!
//! `../mod.rs` の `reorderable_list_expandable` に渡す expansion クロージャが
//! ここ 1 本 ([`draw_device_panel`]) を呼ぶ。
//!
//! **セクションごとに 1 ファイル**にしてあるのは不変条件 9 (サイズ budget) の
//! ため。 元は `track_inspector/mod.rs::draw` の中に埋まった 1 個のクロージャで、
//! r.md #71 でここへ出したときも 1 関数 1,122 実コード行 = 関数 budget (300 行) の
//! 3.7 倍だった。 r.md #76 が測り方を物理行から実コード行へ変えてそれが表に出たので、
//! **baseline へ逃がさずセクション軸で割った** (「超過したら分割してから足す」)。
//!
//! 各セクションの contract は `chain_sections.rs` / `modulation_rack.rs` と同じ
//! 「`(app, ui, area, pad, 起点 y) -> 次の y`」。**gate は各セクションが自分で持つ**
//! (`voicevox_param_panel_open()` 等) ので、ここは順に呼ぶだけ = 並び順が
//! そのまま画面の上下順になる。

mod clip_voice;
mod group_transform;
mod lipsync;
mod plugin_params;
mod talk;
mod text_event;
mod video_fx;

use super::*;

/// 開いたデバイスの param パネルを描き、消費後の `y` を返す。
///
/// **この順序が画面の上下順**。 セクションはそれぞれ自分の gate で「出す / 出さない」を
/// 決めるので、 出ないセクションは `y` を素通しする。
pub(super) fn draw_device_panel(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    exp_rect: Rect,
) -> f32 {
    let mut y = exp_rect.y;
    y = group_transform::draw_group_transform(app, ui, area, pad, y);
    y = video_fx::draw_video_fx_params(app, ui, area, pad, y);
    y = plugin_params::draw_plugin_params(app, ui, area, pad, y);
    y = text_event::draw_text_event(app, ui, area, pad, y);
    y = clip_voice::draw_clip_voice(app, ui, area, pad, y);
    y = talk::draw_talk(app, ui, area, pad, y);
    lipsync::draw_lipsync_target(app, ui, area, pad, y)
}
