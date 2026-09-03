//! 「何の色を編集しているか」 の宛先 (`color_picker` overlay の対象)。
//!
//! `app_types.rs` から分けてあるのは不変条件 9 (サイズ budget) のため
//! — `device_addr.rs` を切り出したのと同じ手当てで、r.md #87 が
//! [`ColorPickerTarget::Scene`](crate::color_target::ColorPickerTarget::Scene) を
//! 足す前に器を独立させた。
//! 呼び出し側は `crate::app::*` 経由で今までどおり名前を引ける
//! (`app_types` が再輸出する)。
//!
//! 対象は全部 **安定 id** で持つ ([`ClipKey`](crate::app_types::ClipKey) も r.md #87 で
//! `{ track_id, clip_id }` の安定 id に統合済み)。overlay が開いたまま対象が
//! 消えても、id なので他人に化けない。

use crate::app_types::ClipKey;

/// v18 (`docs/plan_track_clip_color.md`): color_picker overlay (gui_01 #058)
/// の編集対象。`Some` の間 arrangement_view が 1 フレームごとに
/// `ui.color_picker` を呼んで overlay を描画する。`Track` は `Track::id`、
/// `Clip` は [`ClipKey`] (= `Track::id` + `Clip::id`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorPickerTarget {
    Track(u32),
    Clip(ClipKey),
    /// オートメーションクリップ (`AutomationClip::color`)。arrangement / session 両方の
    /// 右クリックメニュー「色...」から開く。
    AutomationClip(common::model::AutomationClipKey),
    /// オートメーションレーン (`AutomationLane::color`)。lane header の右クリックから開く。
    AutomationLane(common::model::AutomationLaneKey),
    /// Arranger セクション帯の色。
    Section(u32),
    /// r.md #87: ランチャーの列 (シーン) の色ストライプ (`Scene::id`)。
    /// 見出しの右クリックメニューから開く。
    Scene(u32),
}
