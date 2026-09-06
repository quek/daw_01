//! r.md #113: 仮想鍵盤ウィンドウの **session-only** な状態 (`docs/plan_virtual_keyboard.md`)。
//!
//! ここに置くのは保存しないものだけ。 位置 / オクターブ / ベロシティは
//! 「この人の作業のしかた」 なので `UiPrefs` (→ app_config) に持つ。

use daw_ui_platform::PhysicalKey;

#[derive(Debug, Default)]
pub struct VirtualKeyboardState {
    /// 窓が開いているか = PC キーを横取りしているか。 起動時は常に閉 (保存しない)。
    pub open: bool,
    /// いま押されている PC キーと、 押した瞬間に決めたピッチ。 離すときはここから
    /// 引く (押している間にオクターブを変えても、 鳴っている音を正しく止められる)。
    pub held: Vec<(PhysicalKey, u8)>,
    /// マウスで押している鍵のピッチ (ピアノロール左の鍵盤と同じ held-value)。
    pub mouse_pitch: Option<u8>,
    /// ベロシティの数値欄をドラッグ / 入力中か (立ち下がりで app_config へ保存する
    /// edge 検出用、 Sampler の「長さ」 欄と同 idiom)。
    pub velocity_editing: bool,
}
