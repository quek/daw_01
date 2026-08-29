//! 「何の色を編集しているか」 の宛先 (`color_picker` overlay の対象)。
//!
//! `app_types.rs` から分けてあるのは不変条件 9 (サイズ budget) のため
//! — `device_addr.rs` を切り出したのと同じ手当てで、r.md #87 が
//! [`ColorPickerTarget::Scene`] を足す前に器を独立させた。
//! 呼び出し側は `crate::app::*` 経由で今までどおり名前を引ける
//! (`app_types` が再輸出する)。
//!
//! 対象は全部 **安定 id** で持つ (`Clip` だけは index ベースの
//! [`ClipKey`] だが、これは overlay が開いている 1 フレームの間だけ生きる
//! 一時参照で、保持されない)。

use crate::app_types::ClipKey;

/// v18 (`docs/plan_track_clip_color.md`): color_picker overlay (gui_01 #058)
/// の編集対象。`Some` の間 arrangement_view が 1 フレームごとに
/// `ui.color_picker` を呼んで overlay を描画する。`Track` は track id、
/// `Clip` は index ベース `ClipKey`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorPickerTarget {
    Track(u32),
    Clip(ClipKey),
    /// Arranger セクション帯の色。
    Section(u32),
    /// r.md #87: ランチャーの列 (シーン) の色ストライプ (`Scene::id`)。
    /// 見出しの右クリックメニューから開く。
    Scene(u32),
}
