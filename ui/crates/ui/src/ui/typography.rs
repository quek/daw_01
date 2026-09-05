//! 操作 widget (button / dropdown / toggle / tab / text_input) の共通文字サイズ
//! (daw_01 r.md #103)。
//!
//! 旧実装は dropdown 14 / button 16 / tab 14 と widget ごとに px を焼き込んでおり、
//! アプリ側の 11〜12px の読み出しと並べると 1 つだけ跳ねて見えた。 host が 1 値を
//! 持ち、 widget はそれを既定にする (個別に変えたい呼び出し側だけ `*_sized` /
//! style で上書き)。 色の [`super::Palette`] と同じく **widget が読む唯一の出どころ**。

use super::{Ui, UiHost};

/// [`UiHost::control_font_size`] の初期値 (px)。ライブラリ単体 (examples / tests) の
/// 見た目。アプリは [`UiHost::set_control_font_size`] で自分の読み出し文字に揃える。
pub const DEFAULT_CONTROL_FONT_SIZE: f32 = 14.0;

impl<M: ?Sized + 'static> UiHost<M> {
    /// 操作 widget の既定文字サイズ (px) を差し替える。変化したら `true`
    /// (= 呼び出し側は描画キャッシュを捨てる。`set_palette` と同じ契約)。
    /// 毎フレーム無条件に呼んでよい。
    pub fn set_control_font_size(&mut self, px: f32) -> bool {
        if (self.control_font_size - px).abs() < f32::EPSILON {
            return false;
        }
        self.control_font_size = px;
        true
    }

    /// 操作 widget の既定文字サイズ (px)。
    #[must_use]
    pub fn control_font_size(&self) -> f32 {
        self.control_font_size
    }
}

impl<M: ?Sized + 'static> Ui<'_, M> {
    /// 操作 widget の既定文字サイズ (px)。 アプリが [`UiHost::set_control_font_size`]
    /// で一括指定する。 widget はこの値を既定にし、 個別に変えたい呼び出し側だけ
    /// `*_sized` / style で上書きする。
    #[must_use]
    pub fn control_font_size(&self) -> f32 {
        self.control_font_size
    }
}
