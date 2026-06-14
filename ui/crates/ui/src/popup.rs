//! `popup_layer` — modal な popup / menu / dropdown / context_menu の共通基盤 (M7 Phase 25)。
//!
//! 設計:
//! - popup の open / close 状態は `UiHost` に `HashMap<WidgetId, PopupOpenState>` で保持
//! - `Ui::popup_layer(id, |ui| ...)` で「open ならば描画」、closure 内の primitive は
//!   **deferred buffer** に積まれ、frame 末尾で base scene に append (z-order = 最前面)
//! - `Ui::open_popup(id, anchor, modal)` で popup を開く (例: `menu_bar` が File menu の click で呼ぶ)
//! - `Ui::close_popup(id)` で popup を閉じる
//! - 外クリック検出は popup_layer 内で実装、自動 close
//! - modal: M7 では popup の anchor 外クリックを popup_layer 自身が消費する形 (他 widget は
//!   `pointer.primary_just_*` がそのまま見えるので、利用者が popup_layer を user closure の
//!   早い段階に置くこと。この前提は `feedback_pursue_best_practice` の妥協ポイントとして
//!   `docs/history.html` に記録)

use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::Rect;

use crate::id::WidgetId;

/// popup が現在 open している間 `UiHost` に保持される情報。
#[derive(Debug, Clone, Copy)]
pub struct PopupOpenState {
    /// popup を開く起点となった矩形 (例: menu_bar の "File" ボタン)。
    /// 外クリック判定で「この anchor 外のクリック = close」に使う場合がある (利用者次第)。
    pub anchor: Rect,
    /// modal なら他 widget の click を抑制する。M7 では popup 自身が責任。
    pub modal: bool,
    /// popup を開く前に focus を持っていた widget。close で戻す。
    pub prev_focus: Option<WidgetId>,
    /// M14 Phase 94 (daw_01 #065): 「真のモーダル」かどうか。`true` の間、panel (popup body)
    /// **以外**の全 widget への pointer / keyboard 入力を遮断する (= 背景 widget を inert に
    /// する)。`open_popup` 経由の menu / dropdown / context_menu は `false` (= 従来どおり
    /// 「panel の裏に隠れた widget だけ」を抑制)。`Ui::open_modal` 経由の dialog だけ `true`。
    pub capture_input: bool,
    /// M14 Phase 95 (daw_01 #066): panel (anchor) 外 click で popup を auto-close するか。
    /// `true` (default) は menu / dropdown / 通常 modal の従来挙動 (外 click で閉じる)。
    /// `false` の間は外 click で閉じず、 modal なら click を consume するだけ (= Cancel ボタン等で
    /// しか閉じない blocking modal)。`Ui::modal` が毎フレーム `ModalStyle::close_on_outside_click`
    /// から同期する (`open_popup` / `open_modal` の初期値は `true`)。
    pub dismiss_on_outside_click: bool,
}

/// anchor 起点の popup rect (dropdown / menu_bar 用)。
///
/// 配置順:
/// 1. anchor 直下 (`anchor.y + anchor.h`) に popup_h 分入るならそこに置く
/// 2. 入らず anchor の上に空きがあるなら `anchor.y - popup_h` に flip
/// 3. 上下どちらも入らない極端 case は空きの大きい側に置く (xy clamp、 popup_h 据え置き)
///
/// scroll は scope 外。 popup_h は切り詰めない (極端 case では末尾 item が画面外で不可視)。
/// `screen` の単位 (physical px) は anchor / popup_w / popup_h と統一されている前提
/// (gui_01 全体が physical pixel ベース、 modal.rs:93-94 の前例と同じ扱い)。
#[must_use]
pub fn popup_rect_below_or_above(
    anchor: Rect,
    popup_w: f32,
    popup_h: f32,
    screen: PhysicalSize,
) -> Rect {
    let screen_h = screen.height as f32;
    let screen_w = screen.width as f32;
    let space_below = (screen_h - (anchor.y + anchor.h)).max(0.0);
    let space_above = anchor.y.max(0.0);

    let y = if popup_h <= space_below {
        anchor.y + anchor.h
    } else if popup_h <= space_above {
        anchor.y - popup_h
    } else if space_below >= space_above {
        anchor.y + anchor.h
    } else {
        (anchor.y - popup_h).max(0.0)
    };

    Rect { x: clamp_x(anchor.x, popup_w, screen_w), y, w: popup_w, h: popup_h }
}

/// 任意座標起点の popup rect (context_menu_for 用)。
///
/// 右クリック座標 `origin` を top-left として popup を下に伸ばす。 画面下端 / 右端で
/// clamp。 flip しない (右クリック位置と popup の関係を維持、 DAW 標準 UX)。
#[must_use]
pub fn popup_rect_clamped_at(
    origin: (f32, f32),
    popup_w: f32,
    popup_h: f32,
    screen: PhysicalSize,
) -> Rect {
    let screen_w = screen.width as f32;
    let screen_h = screen.height as f32;
    let (ox, oy) = origin;
    let x = clamp_x(ox, popup_w, screen_w);
    let y = if oy + popup_h <= screen_h {
        oy
    } else {
        (screen_h - popup_h).max(0.0)
    };
    Rect { x, y, w: popup_w, h: popup_h }
}

/// X 軸 clamp (popup が画面右端を超えれば押し戻す、 popup_w > screen_w なら 0)。
fn clamp_x(origin_x: f32, popup_w: f32, screen_w: f32) -> f32 {
    if origin_x + popup_w <= screen_w {
        origin_x.max(0.0)
    } else {
        (screen_w - popup_w).max(0.0)
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // 入力 + 単純な加減算なので exact equality で OK
mod tests {
    use super::*;

    const SCREEN: PhysicalSize = PhysicalSize { width: 800, height: 600 };

    #[test]
    fn below_when_fits() {
        // 普段の dropdown (4 items × 24px = 96px、 画面上端付近)
        let anchor = Rect { x: 100.0, y: 50.0, w: 120.0, h: 26.0 };
        let r = popup_rect_below_or_above(anchor, 120.0, 96.0, SCREEN);
        assert_eq!(r.x, 100.0);
        assert_eq!(r.y, 76.0);
        assert_eq!(r.w, 120.0);
        assert_eq!(r.h, 96.0);
    }

    #[test]
    fn flip_above_when_below_overflows_piano_roll_snap_case() {
        // bug #013: 21 items × 24px = 504px、 anchor 画面中央付近
        // 下空き = 600 - (300 + 26) = 274、 上空き = 300 → 上に flip
        let anchor = Rect { x: 100.0, y: 300.0, w: 120.0, h: 26.0 };
        let r = popup_rect_below_or_above(anchor, 120.0, 504.0, SCREEN);
        assert!(r.y < anchor.y, "flip して anchor の上に出る");
        assert_eq!(r.h, 504.0, "popup_h は据え置き (scroll は別 PR)");
    }

    #[test]
    fn extreme_both_overflow_below_wider_keeps_below() {
        // 上下両方入らない、 下空きのほうが広い → 下に置いて画面外にはみ出す末尾を許容
        // anchor.y = 50 (上空き 50px)、 anchor.h = 26、 下空き = 600 - 76 = 524
        // popup_h = 700 > 524 + space_below_only > 50 → 下のほうが広い
        let anchor = Rect { x: 100.0, y: 50.0, w: 120.0, h: 26.0 };
        let r = popup_rect_below_or_above(anchor, 120.0, 700.0, SCREEN);
        assert_eq!(r.y, 76.0, "下に置く (clamp なし)");
    }

    #[test]
    fn extreme_both_overflow_above_wider_clamps_to_top() {
        // 上下両方入らない、 上空きのほうが広い → 上 flip + 上端 clamp
        // anchor.y = 500 (上空き 500px)、 anchor.h = 26、 下空き = 74
        // popup_h = 580 > 500 (上) > 74 (下) → 上のほうが広い → flip + 上端 clamp
        let anchor = Rect { x: 100.0, y: 500.0, w: 120.0, h: 26.0 };
        let r = popup_rect_below_or_above(anchor, 120.0, 580.0, SCREEN);
        assert_eq!(r.y, 0.0, "上端 clamp");
    }

    #[test]
    fn clamp_x_when_overflow_right() {
        // anchor.x + popup_w > screen_w → 右端 clamp
        let anchor = Rect { x: 750.0, y: 50.0, w: 100.0, h: 26.0 };
        let r = popup_rect_below_or_above(anchor, 120.0, 96.0, SCREEN);
        assert_eq!(r.x, 800.0 - 120.0);
    }

    #[test]
    fn context_menu_no_clamp_when_fits() {
        // 普通の右クリック (画面中央)
        let r = popup_rect_clamped_at((100.0, 100.0), 180.0, 100.0, SCREEN);
        assert_eq!(r.x, 100.0);
        assert_eq!(r.y, 100.0);
    }

    #[test]
    fn context_menu_clamp_at_screen_bottom() {
        // 画面下端で右クリック (popup_h = 100、 oy + popup_h = 680 > 600)
        let r = popup_rect_clamped_at((400.0, 580.0), 180.0, 100.0, SCREEN);
        assert_eq!(r.x, 400.0);
        assert_eq!(r.y, 500.0);
    }

    #[test]
    fn context_menu_extreme_taller_than_screen() {
        // popup_h > screen_h (極端、 items 多すぎ) → 上端 0 で clamp
        let r = popup_rect_clamped_at((100.0, 100.0), 180.0, 800.0, SCREEN);
        assert_eq!(r.y, 0.0);
    }
}
