//! `popup_layer` — modal な popup / menu / dropdown / context_menu の共通基盤 (M7 Phase 25)。
//!
//! 設計:
//! - popup の open / close 状態は `UiHost` に `HashMap<WidgetId, PopupOpenState>` で保持
//! - `Ui::popup_layer(id, |ui| ...)` で「open ならば描画」、closure 内の primitive は
//!   **deferred buffer** に積まれ、frame 末尾で base scene に append (z-order = 最前面)
//! - `Ui::open_popup(id, anchor, modal)` で popup を開く (例: `menu_bar` が File menu の click で呼ぶ)
//! - `Ui::close_popup(id)` で popup を閉じる
//! - 外クリック検出は popup_layer 内で実装、自動 close
//! - 縦リスト popup (menu / dropdown / context menu / cascade) は
//!   `list_popup_rect_*` で **画面に収まる高さへ打ち切って** 配置し、body 側で
//!   `Ui::popup_scroll` を呼んでホイール / scrollbar でスクロールさせる
//!   (打ち切らないと末尾 item が画面外に描かれ、hit-test はスクリーン座標なので
//!   原理的にクリックできない)
//! - modal: M7 では popup の anchor 外クリックを popup_layer 自身が消費する形 (他 widget は
//!   `pointer.primary_just_*` がそのまま見えるので、利用者が popup_layer を user closure の
//!   早い段階に置くこと。この前提は `feedback_pursue_best_practice` の妥協ポイントとして
//!   `docs/history.html` に記録)

use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Rect, RectCommand};

use crate::id::WidgetId;
use crate::ui::Ui;
use crate::widgets::scroll_area::{SCROLLBAR_W, thumb_rect_vertical, vertical_scrollbar_rect};

/// popup が現在 open している間 `UiHost` に保持される情報。
// modal / capture_input / capture_keyboard / dismiss_on_outside_click は、 それぞれ
// 独立した popup 挙動フラグ (close 抑制 / pointer mask / keyboard mask / 外クリック
// close)。 `Ui` struct (ui.rs) の transient bool 群と同じく、 state machine 化は
// オーバーヘッド過大なので clippy::struct_excessive_bools を allow する。
#[allow(clippy::struct_excessive_bools)]
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
    /// resource monitor (r.md #3): `capture_input == true` のうち keyboard / shortcut も
    /// 遮断するか。 `true` = 真のモーダル (dialog / color_picker)、 `false` = pointer だけ
    /// masking して keyboard は background に通す overlay panel (= Performance パネル。
    /// 再生継続のため Space 等を効かせる)。 `capture_input == false` のときは無関係。
    pub capture_keyboard: bool,
    /// M14 Phase 95 (daw_01 #066): panel (anchor) 外 click で popup を auto-close するか。
    /// `true` (default) は menu / dropdown / 通常 modal の従来挙動 (外 click で閉じる)。
    /// `false` の間は外 click で閉じず、 modal なら click を consume するだけ (= Cancel ボタン等で
    /// しか閉じない blocking modal)。`Ui::modal` が毎フレーム `ModalStyle::close_on_outside_click`
    /// から同期する (`open_popup` / `open_modal` の初期値は `true`)。
    pub dismiss_on_outside_click: bool,
    /// popup body の縦スクロール量 (px、 `0.0` = 先頭)。 画面に入りきらない縦リスト
    /// (menu / dropdown / context menu / cascade) を [`Ui::popup_scroll`] がここに書く。
    ///
    /// **popup state に持つ** のが要点: popup を閉じれば `PopupOpenState` ごと消えるので
    /// 「開き直したら必ず先頭から」が構造的に保証される (widget_state 側に持つと popup の
    /// 寿命とズレて、閉じたはずの popup の scroll 位置が次の open に漏れる)。
    pub scroll_offset: f32,
    /// scrollbar thumb を drag 中の anchor `(押下時の pointer y, 押下時の scroll_offset)`。
    /// drag 中は item の hover / click を抑止する (thumb を掴んだまま item 上で離しても
    /// 選択されない)。
    pub scroll_drag: Option<(f32, f32)>,
}

/// [`Ui::popup_scroll`] の結果。
///
/// **描画と hit-test の両方に反映すること**。片方だけに適用すると「見えている項目と
/// 押される項目がずれる」という最悪の壊れ方をする。
#[derive(Debug, Clone, Copy)]
pub struct PopupScroll {
    /// item を `y - offset` に置くための縦スクロール量 (px、 `0.0` = 先頭)。
    pub offset: f32,
    /// item を描ける幅。 scrollbar を出している間は [`SCROLLBAR_W`] 分だけ狭い
    /// (item が scrollbar の下に潜らない)。
    pub content_w: f32,
    /// scrollbar thumb を drag 中。 `true` の frame は item の hover / click を抑止する。
    pub dragging: bool,
}

/// anchor 起点の popup rect (dropdown / menu_bar / color_picker 用)。
///
/// 配置順:
/// 1. anchor 直下 (`anchor.y + anchor.h`) に popup_h 分入るならそこに置く
/// 2. 入らず anchor の上に空きがあるなら `anchor.y - popup_h` に flip
/// 3. 上下どちらも入らない極端 case は空きの大きい側に置く (xy clamp、 popup_h 据え置き)
///
/// **この関数は「置き場所」だけを決め、`popup_h` は切り詰めない** (3 の枝では popup が
/// 画面外へはみ出しうる)。 画面内に収めるのは呼び出し側の責務で、縦リスト popup
/// (menu / dropdown / context menu) は [`list_popup_rect_below_or_above`] を使う
/// (= 画面高で打ち切り + y clamp + [`Ui::popup_scroll`] でスクロール)。 色ピッカーの
/// ように中身をスクロールできない固定寸法 panel だけが本関数を直接呼ぶ (打ち切ると
/// anchor が panel より小さくなり、 panel 上の click が outside-click 扱いになるため)。
///
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

/// 縦リスト popup (menu_bar / dropdown) の rect。 anchor 直下 → 上 flip の配置に加えて、
/// **画面内に必ず全体が収まる** ように高さと y を決める:
///
/// 1. 中身が画面高を超えるなら画面高で打ち切り、 scrollbar 分の幅を右に足す
///    (打ち切られた popup は body 側で [`Ui::popup_scroll`] がスクロールさせる)
/// 2. 上下どちらにも入らない高さは、 anchor に**重ねてでも**画面内へ寄せる
///    (combobox が本体に被さるのと同じ。 [`popup_rect_below_or_above`] の 3 番目の枝は
///    下に置くと画面外へはみ出すので、 ここで y を clamp する)
///
/// 打ち切りは「中身 > 画面高」のときだけなので、 画面に載る限りは **全項目を出す**
/// (例: 600px の画面に 21 項目 × 24px = 504px の dropdown はスクロールなしで全部見える)。
#[must_use]
pub fn list_popup_rect_below_or_above(
    anchor: Rect,
    content_w: f32,
    content_h: f32,
    screen: PhysicalSize,
) -> Rect {
    let screen_h = screen.height as f32;
    let (w, h) = fit_list_size(content_w, content_h, screen_h);
    let r = popup_rect_below_or_above(anchor, w, h, screen);
    Rect { y: r.y.clamp(0.0, (screen_h - h).max(0.0)), ..r }
}

/// 任意座標起点の縦リスト popup rect (context_menu 用)。 最大高さは画面高。
#[must_use]
pub fn list_popup_rect_at(
    origin: (f32, f32),
    content_w: f32,
    content_h: f32,
    screen: PhysicalSize,
) -> Rect {
    let (w, h) = fit_list_size(content_w, content_h, screen.height as f32);
    popup_rect_clamped_at(origin, w, h, screen)
}

/// cascade サブ popup の rect (親 item の横に開く)。
///
/// - x: 既定は親 item の **右隣**。 画面右端に入らなければ item の **左** へ flip し、
///   左にも入らなければ画面内へ clamp する (Windows / macOS のカスケードと同じ)。
/// - y: 既定は親 item の上端揃え。 画面下端を超える分だけ上へ押し戻す。
/// - 高さは画面高で打ち切り、 打ち切ったら scrollbar 分の幅を足す。
#[must_use]
pub fn list_popup_rect_beside(
    item: Rect,
    content_w: f32,
    content_h: f32,
    screen: PhysicalSize,
) -> Rect {
    let screen_w = screen.width as f32;
    let screen_h = screen.height as f32;
    let (w, h) = fit_list_size(content_w, content_h, screen_h);
    let right = item.x + item.w;
    let x = if right + w <= screen_w {
        right
    } else if item.x - w >= 0.0 {
        item.x - w
    } else {
        clamp_x(right, w, screen_w)
    };
    let y = if item.y + h <= screen_h {
        item.y.max(0.0)
    } else {
        (screen_h - h).max(0.0)
    };
    Rect { x, y, w, h }
}

/// 縦リスト popup の寸法を「画面に収まる高さ」で打ち切る。 打ち切ったときだけ
/// scrollbar 分の幅を足す (項目テキストが scrollbar に食われて ellipsis になるのを防ぐ)。
fn fit_list_size(content_w: f32, content_h: f32, max_h: f32) -> (f32, f32) {
    if content_h <= max_h {
        (content_w, content_h)
    } else {
        (content_w + SCROLLBAR_W, max_h.max(0.0))
    }
}

/// 任意座標起点の popup rect (context_menu_for 用)。
///
/// 右クリック座標 `origin` を top-left として popup を下に伸ばす。 画面下端 / 右端で
/// clamp。 flip しない (右クリック位置と popup の関係を維持、 DAW 標準 UX)。
/// `popup_h` は切り詰めない ([`popup_rect_below_or_above`] と同じ契約、
/// 縦リストは [`list_popup_rect_at`] を使う)。
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

impl<M: ?Sized + 'static> Ui<'_, M> {
    /// popup body の縦スクロールを 1 フレーム分処理し、 scrollbar を描く。
    ///
    /// `viewport` は popup 自身の rect (= [`list_popup_rect_below_or_above`] 等が返した
    /// 「画面に収まる高さ」)、 `content_h` は item を全部積んだときの高さ。
    /// **`popup_layer` の closure の中で、 item を描く前に呼ぶこと** (scrollbar を item より
    /// 先に積み、 item は戻り値の `content_w` 幅で描くので互いに重ならない)。
    ///
    /// `content_h <= viewport.h` (= 全部見えている) なら wheel も消費せず scrollbar も描かず
    /// `offset = 0.0` / `content_w = viewport.w` を返す。 つまり **収まっている popup の
    /// 見た目と入力は 1px も変わらない**。
    ///
    /// scroll 量は描画中の popup の [`PopupOpenState::scroll_offset`] に持つので、 popup を
    /// 閉じれば捨てられる。 `popup_layer` の外で呼んだ場合は何もしない。
    pub fn popup_scroll(&mut self, viewport: Rect, content_h: f32) -> PopupScroll {
        let idle = PopupScroll { offset: 0.0, content_w: viewport.w, dragging: false };
        let max_offset = (content_h - viewport.h).max(0.0);
        let Some((prev_offset, prev_drag)) = self.current_popup_scroll() else {
            return idle;
        };
        if max_offset <= 0.0 {
            // 収まっているので scroll state を持たない (項目が減って収まりきった直後に
            // 古い offset が残らないよう明示リセットする)。
            self.set_current_popup_scroll(0.0, None);
            return idle;
        }

        // ---- 1. wheel (popup body なので `pointer_blocked_by_modal_popup` は false) ----
        let track = vertical_scrollbar_rect(viewport, false);
        let wheel = self.take_scroll_in_rect(viewport).1;
        let pointer = self.pointer();
        let mut offset = (prev_offset - wheel).clamp(0.0, max_offset);
        let mut thumb = thumb_rect_vertical(track, content_h, viewport.h, offset, max_offset);

        // ---- 2. thumb drag (scroll_area と同じ「現在 offset の thumb で hit-test」) ----
        let mut drag = prev_drag;
        if drag.is_none()
            && pointer.primary_just_pressed
            && let Some((px, py)) = pointer.pos
            && thumb.contains(px, py)
        {
            drag = Some((py, offset));
        }
        if let Some((anchor_py, anchor_offset)) = drag
            && let Some((_, py)) = pointer.pos
        {
            let drag_range = (track.h - thumb.h).max(1.0);
            offset = (anchor_offset + (py - anchor_py) / drag_range * max_offset)
                .clamp(0.0, max_offset);
            thumb = thumb_rect_vertical(track, content_h, viewport.h, offset, max_offset);
        }
        // release frame も「この frame の pointer は scrollbar のもの」と扱う
        // (thumb を掴んだまま item の上へ動かして離しても、 その item を選ばない)。
        let dragging = drag.is_some();
        if pointer.primary_just_released {
            drag = None;
        }
        self.set_current_popup_scroll(offset, drag);
        if (offset - prev_offset).abs() > 1e-4 {
            self.request_redraw();
        }

        // ---- 3. scrollbar 描画 (色 / 寸法は scroll_area と共通) ----
        let p = self.palette();
        self.push_rect(RectCommand::uniform_radius(track, p.scrollbar_track, 2.0));
        let hovered = pointer.pos.is_some_and(|(px, py)| thumb.contains(px, py));
        self.push_rect(RectCommand::uniform_radius(
            thumb,
            if hovered || dragging { p.scrollbar_thumb_hover } else { p.scrollbar_thumb },
            3.0,
        ));

        PopupScroll { offset, content_w: (viewport.w - SCROLLBAR_W).max(0.0), dragging }
    }
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
        // 本関数は「置き場所」だけを決める契約なので popup_h は据え置き
        // (打ち切りが要る縦リストは `list_popup_rect_below_or_above` を使う)。
        assert_eq!(r.h, 504.0, "popup_h は切り詰めない");
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

    // ===== 縦リスト popup (最大高さ打ち切り + scroll) =====

    /// 収まる項目数では **従来と 1px も変わらない** (打ち切りなし / scrollbar 幅の加算なし)。
    #[test]
    fn list_popup_identical_to_plain_placement_when_it_fits() {
        let anchor = Rect { x: 100.0, y: 50.0, w: 120.0, h: 26.0 };
        let plain = popup_rect_below_or_above(anchor, 120.0, 96.0, SCREEN);
        let list = list_popup_rect_below_or_above(anchor, 120.0, 96.0, SCREEN);
        assert_eq!((list.x, list.y, list.w, list.h), (plain.x, plain.y, plain.w, plain.h));
    }

    /// 上下どちらにも入らないが **画面には載る** 高さは、 打ち切らず anchor に重ねて
    /// 全項目を出す (bug #013 の piano_roll snap dropdown = 21 items × 24px = 504px)。
    /// 従来の「空きの広い側 + 上端 clamp」 と同じ結果 = この case は回帰ゼロ。
    #[test]
    fn list_popup_keeps_all_items_when_they_fit_on_screen() {
        let anchor = Rect { x: 100.0, y: 300.0, w: 120.0, h: 26.0 };
        let r = list_popup_rect_below_or_above(anchor, 120.0, 504.0, SCREEN);
        assert_eq!(r.h, 504.0, "画面に載る高さは打ち切らない");
        assert_eq!(r.y, 0.0, "上端 clamp (従来と同じ)");
        assert_eq!(r.w, 120.0, "scrollbar 分は足さない (スクロール不要)");
    }

    /// 中身が **画面高を超える** ときだけ打ち切り + scrollbar 分の幅を足す。
    #[test]
    fn list_popup_caps_height_and_reserves_scrollbar() {
        // 50 items × 24px = 1200px > 画面高 600px。
        let anchor = Rect { x: 100.0, y: 300.0, w: 120.0, h: 26.0 };
        let r = list_popup_rect_below_or_above(anchor, 120.0, 1200.0, SCREEN);
        assert_eq!(r.h, SCREEN.height as f32, "画面高で打ち切る");
        assert_eq!(r.y, 0.0, "打ち切った popup は上端から");
        assert_eq!(r.w, 120.0 + SCROLLBAR_W, "scrollbar 分の幅を足す");
        assert!(
            r.y + r.h <= SCREEN.height as f32,
            "popup 全体が画面内 (末尾 item が画面外に描かれない)"
        );
    }

    /// 下にも上にも入らない高さは、 下側に置いて画面外へはみ出すのではなく
    /// **画面内へ y を押し戻す** (旧 `popup_rect_below_or_above` の 3 番目の枝の穴)。
    #[test]
    fn list_popup_clamps_y_instead_of_overflowing_below() {
        // anchor 上端寄り (上空き 50 / 下空き 524) に 560px の中身 → 下が広いので下に
        // 置きたいが、 76 + 560 = 636 > 600 なのではみ出す → y を 40 まで押し戻す。
        let anchor = Rect { x: 100.0, y: 50.0, w: 120.0, h: 26.0 };
        let r = list_popup_rect_below_or_above(anchor, 120.0, 560.0, SCREEN);
        assert_eq!(r.h, 560.0, "画面に載るので打ち切らない");
        assert_eq!(r.y, 600.0 - 560.0, "画面内へ押し戻す");
        assert!(r.y + r.h <= SCREEN.height as f32);
    }

    /// 画面高より高い項目数でも popup の高さは画面高を超えない (どの anchor 位置でも)。
    #[test]
    fn list_popup_never_exceeds_screen_height() {
        for anchor_y in [0.0_f32, 100.0, 300.0, 560.0] {
            let anchor = Rect { x: 10.0, y: anchor_y, w: 120.0, h: 26.0 };
            // 1000 items × 24px
            let r = list_popup_rect_below_or_above(anchor, 120.0, 24_000.0, SCREEN);
            assert!(
                r.h <= SCREEN.height as f32 && r.y >= 0.0 && r.y + r.h <= SCREEN.height as f32,
                "anchor_y={anchor_y}: popup が画面内 (rect={r:?})"
            );
        }
    }

    /// context menu も画面高で打ち切る (右クリック位置は維持したまま)。
    #[test]
    fn list_popup_at_caps_to_screen_height() {
        let r = list_popup_rect_at((100.0, 100.0), 180.0, 2400.0, SCREEN);
        assert_eq!(r.y, 0.0);
        assert_eq!(r.h, SCREEN.height as f32);
        assert_eq!(r.w, 180.0 + SCROLLBAR_W);
    }

    /// cascade: 収まるなら従来どおり **親 item の右隣・上端揃え・原寸** (回帰ゼロ)。
    #[test]
    fn cascade_keeps_right_and_top_when_it_fits() {
        let item = Rect { x: 100.0, y: 200.0, w: 180.0, h: 24.0 };
        let r = list_popup_rect_beside(item, 180.0, 96.0, SCREEN);
        assert_eq!((r.x, r.y, r.w, r.h), (280.0, 200.0, 180.0, 96.0));
    }

    /// cascade: 画面右端で右隣に入らなければ **親 item の左** へ flip する。
    #[test]
    fn cascade_flips_left_at_right_edge() {
        // item 右端 = 780、 右隣に 180px は入らない (780 + 180 > 800) → 左へ flip。
        let item = Rect { x: 600.0, y: 200.0, w: 180.0, h: 24.0 };
        let r = list_popup_rect_beside(item, 180.0, 96.0, SCREEN);
        assert_eq!(r.x, 600.0 - 180.0, "親 item の左に出る");
        assert!(r.x >= 0.0 && r.x + r.w <= SCREEN.width as f32, "画面内");
    }

    /// cascade: 左右どちらにも入らない極端な幅は画面内へ clamp する。
    #[test]
    fn cascade_clamps_when_neither_side_fits() {
        let item = Rect { x: 300.0, y: 10.0, w: 100.0, h: 24.0 };
        let r = list_popup_rect_beside(item, 700.0, 96.0, SCREEN);
        assert!(r.x >= 0.0 && r.x + r.w <= SCREEN.width as f32, "画面内へ clamp (rect={r:?})");
    }

    /// cascade: 画面下端では上へ押し戻し、 画面高を超える中身は打ち切る。
    #[test]
    fn cascade_clamps_at_screen_bottom() {
        let item = Rect { x: 100.0, y: 560.0, w: 180.0, h: 24.0 };
        let r = list_popup_rect_beside(item, 180.0, 240.0, SCREEN);
        assert_eq!(r.y, 600.0 - 240.0, "下端に収まるまで上へ押し戻す");
        assert!(r.y + r.h <= SCREEN.height as f32);

        // 画面高より高い cascade は打ち切り + scrollbar 幅。
        let tall = list_popup_rect_beside(item, 180.0, 2400.0, SCREEN);
        assert_eq!(tall.y, 0.0);
        assert_eq!(tall.h, SCREEN.height as f32);
        assert_eq!(tall.w, 180.0 + SCROLLBAR_W);
    }
}
