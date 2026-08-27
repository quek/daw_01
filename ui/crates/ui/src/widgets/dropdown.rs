//! `dropdown` widget — combobox 風の値選択 UI (M7 Phase 25)。
//!
//! `popup_layer` + `menu::draw_items_popup` を再利用。クリックで items を popup 表示、
//! 選択で `Some(idx)` を返す (利用者が Edit を発行)。

use std::hash::Hash;

use daw_ui_renderer::{GlyphArea, LineBatch, LineSegment, Rect, RectCommand};

use crate::id::WidgetId;
use crate::ui::Ui;
use crate::widgets::menu::draw_items_popup;

const DROPDOWN_FONT: f32 = 14.0;
const DROPDOWN_PAD_X: f32 = 8.0;
const DROPDOWN_ARROW_W: f32 = 16.0;

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// dropdown / combobox widget。クリックで items の popup を開き、選択で `Some(idx)` を返す。
    /// 利用者は `Some(idx)` から `Edit::mutate(...)` で Model 側の選択 index を更新する。
    ///
    /// `selected` は現在選択中の index (0-based、`items` 範囲外なら何も表示しない)。
    pub fn dropdown(
        &mut self,
        id: impl Hash,
        rect: Rect,
        items: &[&str],
        selected: usize,
    ) -> Option<usize> {
        let pointer = self.pointer;
        // popup state は caller-id ベース (rect 座標を入れると 1px 動いて state 蒸発 / 同位置別
        // dropdown で衝突する)。
        let popup_id = ("dropdown_popup", WidgetId::ROOT.child((b"dropdown", &id)));
        let inside = pointer.pos.is_some_and(|(px, py)| rect.contains(px, py));
        let already_open = self.is_popup_open(popup_id);

        // 1. 本体描画 (現在値の表示 + 三角アロー)
        // palette の寿命は host の `'a` なので、 以後の `push_rect` / `push_text` (= `&mut self`)
        // と衝突しない。 1 度取って本体描画の全色に使う。
        let p = self.palette();
        let bg_fill = if inside { p.inset_bg_hover } else { p.inset_bg };
        let border = if already_open { p.border_focus } else { p.border };
        self.push_rect(RectCommand {
            rect,
            fill: bg_fill,
            border,
            border_width: 1.0,
            radius: [3.0; 4],
            clip_rect: None,
        });

        let label = items.get(selected).copied().unwrap_or("");
        if !label.is_empty() {
            // ラベルが使えるのは [PAD_X .. ▼ アロー左端] まで。 これを無視して素の
            // `push_text` を撃つと、 長い項目 (例: transport の "No count-in") が
            // アローの上に重なった上で rect 外へはみ出す (daw_01 #079 の
            //「widget は自分の rect 境界に責任を持つ」 が dropdown だけ未適用だった)。
            // button / toggle_button と同じ `fit_text_ellipsized` + clip で揃える。
            let text_max_w = (rect.w - DROPDOWN_PAD_X - DROPDOWN_ARROW_W).max(1.0);
            let (display, _text_w) = self.fit_text_ellipsized(label, DROPDOWN_FONT, text_max_w);
            self.push_text(GlyphArea {
                text: display.as_ref().into(),
                left: rect.x + DROPDOWN_PAD_X,
                top: rect.y + (rect.h - DROPDOWN_FONT * 1.2) * 0.5,
                font_size: DROPDOWN_FONT,
                line_height: DROPDOWN_FONT * 1.2,
                color: p.text,
                clip_rect: Some(Rect {
                    x: rect.x + DROPDOWN_PAD_X,
                    y: rect.y,
                    w: text_max_w,
                    h: rect.h,
                }),
                ..GlyphArea::default()
            });
        }

        // ▼ アロー (右端、線で三角)
        let arrow_x = rect.x + rect.w - DROPDOWN_ARROW_W * 0.5;
        let arrow_y = rect.y + rect.h * 0.5;
        let arrow_size = 4.0;
        let arrow_color = p.text_dim;
        self.push_lines(LineBatch {
            segments: vec![
                LineSegment {
                    a: [arrow_x - arrow_size, arrow_y - arrow_size * 0.5],
                    b: [arrow_x, arrow_y + arrow_size * 0.5],
                    color: arrow_color,
                },
                LineSegment {
                    a: [arrow_x, arrow_y + arrow_size * 0.5],
                    b: [arrow_x + arrow_size, arrow_y - arrow_size * 0.5],
                    color: arrow_color,
                },
            ]
            .into(),
            line_width_px: 1.5,
            clip_rect: None,
        });

        // popup_rect は list_popup_rect_below_or_above で auto-flip + clamp + **最大高さ
        // 打ち切り** 込み計算 (画面下端で popup がはみ出す場合は上に flip、 上下どちらにも
        // 入らない項目数なら空きの広い側で打ち切って `popup_scroll` でスクロールさせる。
        // 打ち切らないと末尾 item が画面外に描かれ、 スクリーン座標の hit-test では
        // 原理的にクリックできない)。 anchor は body rect + popup_rect の汎用 union で
        // flip 後でも outside_click 判定が両方を「内」 として扱える。
        //
        // 高さは `draw_items_popup` の item レイアウトと同じ SSoT (`items_popup_height`)
        // から取る (dropdown 側に別の item 高さ定数を持つと popup 枠と中身がずれる)。
        let content_h = crate::widgets::menu::items_popup_height(items.len());
        // popup は本体幅を下限に、 項目が省略なしで読める幅まで伸ばす (combobox の
        // 慣用。 本体は狭くても選択肢は全部読める)。 旧実装は本体幅固定だったため、
        // 狭い dropdown では項目が popup 枠の外へ直描きされていた。
        //
        // 実測 (`items_popup_width`) は全項目を shape するので、 popup が要る frame
        // (開いている / この frame で開く) だけ計算する。 閉じている dropdown で
        // 毎フレーム全項目を測ると、 transport / inspector に並ぶ dropdown の数だけ
        // UI ループが重くなる。
        let opening = inside && pointer.primary_just_released && !already_open;
        let content_w = if already_open || opening {
            crate::widgets::menu::items_popup_width(self, items, rect.w)
        } else {
            rect.w
        };
        let popup_rect = crate::popup::list_popup_rect_below_or_above(
            rect,
            content_w,
            content_h,
            self.screen(),
        );
        let union_left = rect.x.min(popup_rect.x);
        let union_top = rect.y.min(popup_rect.y);
        let anchor = Rect {
            x: union_left,
            y: union_top,
            w: (rect.x + rect.w).max(popup_rect.x + popup_rect.w) - union_left,
            h: (rect.y + rect.h).max(popup_rect.y + popup_rect.h) - union_top,
        };

        // 2. クリックで popup toggle (click は consume して下層に流さない)
        if inside && pointer.primary_just_released {
            if already_open {
                self.close_popup(popup_id);
            } else {
                self.open_popup(popup_id, anchor, true);
            }
            self.consume_pointer_click();
        }

        // 3. popup 描画 + 選択検出
        let mut chosen: Option<usize> = None;
        self.popup_layer(popup_id, |ui| {
            chosen = draw_items_popup(ui, items, popup_rect);
        });
        if let Some(idx) = chosen {
            self.close_popup(popup_id);
            return Some(idx);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use daw_ui_platform::PhysicalSize;
    use daw_ui_renderer::Scene;

    use super::*;
    use crate::input::{FrameInput, PointerFrame};
    use crate::ui::UiHost;
    use crate::widgets::scroll_area::SCROLLBAR_W;

    const SCREEN: PhysicalSize = PhysicalSize { width: 800, height: 600 };
    /// dropdown 本体。 下空き = 600 - 124 = 476px なので、 476 / 24 = 19.8 項目までしか
    /// 画面に入らない (= 100 項目は必ず打ち切られる)。
    const BODY: Rect = Rect { x: 100.0, y: 100.0, w: 120.0, h: 24.0 };

    fn many_items() -> Vec<String> {
        (0..100).map(|i| format!("item{i:02}")).collect()
    }

    fn pointer_frame(pos: (f32, f32)) -> FrameInput {
        FrameInput {
            pointer: PointerFrame { pos: Some(pos), ..PointerFrame::default() },
            ..FrameInput::default()
        }
    }

    /// 画面に入りきらない項目数の dropdown を開き、 **ホイールで下へスクロールしてから
    /// 最終項目を click すると、 その index が返る**。
    ///
    /// 旧実装は popup_h = items.len() * 24 をそのまま使ったので末尾 item が画面外に描かれ、
    /// hit-test はスクリーン座標なので **原理的にクリックできなかった** (1080p で 45 項目が上限)。
    #[test]
    fn wheel_scroll_makes_the_last_item_clickable() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let items = many_items();
        let refs: Vec<&str> = items.iter().map(String::as_str).collect();
        let chosen = std::cell::Cell::new(None);

        // frame 1: 本体を click して popup を開く。
        let open = FrameInput {
            pointer: PointerFrame {
                pos: Some((110.0, 110.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        host.frame_to_edits(&(), &mut scene, SCREEN, open, |(), ui| {
            ui.dropdown("dd", BODY, &refs, 0);
        });

        // frame 2: popup 上でホイールを大きく下へ (offset は max まで clamp される)。
        let wheel = FrameInput {
            pointer: PointerFrame {
                pos: Some((110.0, 400.0)),
                scroll_delta: (0.0, -5000.0),
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        scene.clear();
        host.frame_to_edits(&(), &mut scene, SCREEN, wheel, |(), ui| {
            ui.dropdown("dd", BODY, &refs, 0);
        });
        let texts: Vec<&str> = scene.iter_popup_glyphs().map(|g| g.text.as_ref()).collect();
        assert!(
            texts.contains(&"item99"),
            "スクロール後は最終項目が描かれている: {texts:?}"
        );
        assert!(
            !texts.contains(&"item00"),
            "先頭項目はスクロールアウトしている: {texts:?}"
        );

        // frame 3: popup 最下段 (= 最終項目) を click → index 99 が返る。
        let click = FrameInput {
            pointer: PointerFrame {
                pos: Some((110.0, 590.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        host.frame_to_edits(&(), &mut scene, SCREEN, click, |(), ui| {
            chosen.set(ui.dropdown("dd", BODY, &refs, 0));
        });
        assert_eq!(chosen.get(), Some(99), "スクロール後の最終項目が選択できる");
    }

    /// 打ち切った popup は **画面内に収まり**、 scrollbar (track + thumb) を描く
    /// (「まだ下に項目がある」 ことが見えないとスクロールできると気付けない)。
    #[test]
    fn oversized_popup_fits_screen_and_draws_scrollbar() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let items = many_items();
        let refs: Vec<&str> = items.iter().map(String::as_str).collect();

        let open = FrameInput {
            pointer: PointerFrame {
                pos: Some((110.0, 110.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        host.frame_to_edits(&(), &mut scene, SCREEN, open, |(), ui| {
            ui.dropdown("dd", BODY, &refs, 0);
        });
        scene.clear();
        host.frame_to_edits(&(), &mut scene, SCREEN, pointer_frame((110.0, 400.0)), |(), ui| {
            ui.dropdown("dd", BODY, &refs, 0);
        });

        let popup_rects: Vec<Rect> = scene.iter_popup_rects().map(|r| r.rect).collect();
        let panel = popup_rects[0];
        assert!(
            panel.y >= 0.0 && panel.y + panel.h <= SCREEN.height as f32,
            "popup 全体が画面内 (panel={panel:?})"
        );
        assert!(
            panel.h < 100.0 * 24.0,
            "全項目ぶんの高さから打ち切られている (panel={panel:?})"
        );
        let bars = popup_rects
            .iter()
            .filter(|r| (r.w - SCROLLBAR_W).abs() < 1e-3 && r.x + r.w >= panel.x + panel.w - 1e-3)
            .count();
        assert_eq!(bars, 2, "scrollbar の track と thumb が描かれる: {popup_rects:?}");
    }

    /// scrollbar thumb の drag でスクロールでき、 **thumb を掴んだまま item の上で離しても
    /// その item は選択されない** (drag 中の pointer は scrollbar のもの)。
    #[test]
    fn thumb_drag_scrolls_without_selecting_an_item() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let items = many_items();
        let refs: Vec<&str> = items.iter().map(String::as_str).collect();
        let chosen = std::cell::Cell::new(None);

        // frame 1: 本体を click して popup を開く (popup = x[100,230) y[124,600))。
        let open = FrameInput {
            pointer: PointerFrame {
                pos: Some((110.0, 110.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        host.frame_to_edits(&(), &mut scene, SCREEN, open, |(), ui| {
            ui.dropdown("dd", BODY, &refs, 0);
        });

        // frame 2: track 右端の thumb 上端 (offset 0 なので y=124 付近) を press。
        let press = FrameInput {
            pointer: PointerFrame {
                pos: Some((225.0, 130.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        host.frame_to_edits(&(), &mut scene, SCREEN, press, |(), ui| {
            ui.dropdown("dd", BODY, &refs, 0);
        });

        // frame 3: item の上 (x=110) へ動かしながら release。 drag は成立してスクロールし、
        // release では item を選ばない。
        scene.clear();
        let drag_release = FrameInput {
            pointer: PointerFrame {
                pos: Some((110.0, 300.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        host.frame_to_edits(&(), &mut scene, SCREEN, drag_release, |(), ui| {
            chosen.set(ui.dropdown("dd", BODY, &refs, 0));
        });
        let texts: Vec<&str> = scene.iter_popup_glyphs().map(|g| g.text.as_ref()).collect();
        assert!(!texts.contains(&"item00"), "thumb drag でスクロールした: {texts:?}");
        assert_eq!(chosen.get(), None, "thumb drag の release で item を選ばない");
    }

    /// **回帰ゼロ**: 画面に収まる項目数では popup の見た目が 1px も変わらない
    /// (打ち切りなし / scrollbar 幅の加算なし / scrollbar 描画なし)。
    #[test]
    fn small_popup_is_unchanged_no_scrollbar() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let refs = ["a", "b", "c", "d"];

        let open = FrameInput {
            pointer: PointerFrame {
                pos: Some((110.0, 110.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        host.frame_to_edits(&(), &mut scene, SCREEN, open, |(), ui| {
            ui.dropdown("dd", BODY, &refs, 0);
        });
        scene.clear();
        host.frame_to_edits(&(), &mut scene, SCREEN, pointer_frame((110.0, 130.0)), |(), ui| {
            ui.dropdown("dd", BODY, &refs, 0);
        });

        let popup_rects: Vec<Rect> = scene.iter_popup_rects().map(|r| r.rect).collect();
        let panel = popup_rects[0];
        assert!((panel.x - BODY.x).abs() < 1e-3, "本体の真下 (panel={panel:?})");
        assert!((panel.y - (BODY.y + BODY.h)).abs() < 1e-3, "本体の真下 (panel={panel:?})");
        assert!((panel.h - 4.0 * 24.0).abs() < 1e-3, "4 項目ぶんの高さ (panel={panel:?})");
        assert!(
            (panel.w - BODY.w).abs() < 1e-3,
            "短い項目なら本体幅のまま (scrollbar 分を足さない): {panel:?}"
        );
        assert!(
            !popup_rects.iter().any(|r| (r.w - SCROLLBAR_W).abs() < 1e-3),
            "scrollbar は描かれない: {popup_rects:?}"
        );
    }

    /// **スクロール領域の中**で開いた dropdown も、 popup 側がホイールを取る。
    ///
    /// これが本番の配置 (daw_01 のインスペクタは `scroll_area("inspector_body", ..)` の
    /// 中にモジュレーションラックを描き、 その中の dropdown が数十トラックを並べる)。
    /// `scroll_area` は **body を実行する前に** `take_scroll_in_rect` でホイールを
    /// 消費する (`scroll_area.rs` 冒頭) ので、 素朴に考えると popup までホイールが
    /// 届かず「popup は scroll できるのに背景がスクロールする」 になりうる。
    ///
    /// 実際には dropdown の popup が `modal = true` で開くため、 popup anchor 上では
    /// `pointer_blocked_by_modal_popup` が背景の消費を止め、 popup body
    /// (`drawing_in_popup`) だけが受け取る。 **この連鎖は実装を読むだけでは
    /// 「呼んでいる」 しか分からない** ので、 背景が動かないことと popup が動くことを
    /// 両方 assert して固定する。
    #[test]
    fn wheel_goes_to_popup_not_to_the_enclosing_scroll_area() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let items = many_items();
        let refs: Vec<&str> = items.iter().map(String::as_str).collect();
        // 画面いっぱいの scroll_area (content は viewport の 10 倍 = 必ずスクロール可能)。
        let vp = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };
        let content = (800.0, 6000.0);
        let offset = std::cell::Cell::new((0.0, 0.0));

        let mut run = |input: FrameInput, scene: &mut Scene| {
            host.frame_to_edits(&(), scene, SCREEN, input, |(), ui| {
                let off = ui.scroll_area("sa", vp, content, |ui, _off| {
                    ui.dropdown("dd", BODY, &refs, 0);
                });
                offset.set(off);
            });
        };

        // frame 1: 本体を click して popup を開く。
        run(
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((110.0, 110.0)),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..FrameInput::default()
            },
            &mut scene,
        );
        let offset_before = offset.get();

        // frame 2: popup の上でホイール。
        scene.clear();
        run(
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((110.0, 400.0)),
                    scroll_delta: (0.0, -5000.0),
                    ..PointerFrame::default()
                },
                ..FrameInput::default()
            },
            &mut scene,
        );

        let texts: Vec<&str> = scene.iter_popup_glyphs().map(|g| g.text.as_ref()).collect();
        assert!(
            texts.contains(&"item99"),
            "popup 側がホイールを取って末尾までスクロールする: {texts:?}"
        );
        assert!(
            (offset.get().1 - offset_before.1).abs() < 1e-3,
            "背景の scroll_area は動かない (before={offset_before:?} after={:?})",
            offset.get()
        );
    }
}
