//! `list_view` ウィジェット — `scroll_area` の上に薄く乗せた virtual list (M9 Phase 45d)。
//!
//! 設計:
//! - 内部で `scroll_area` を呼び、`offset` に応じて表示 row 範囲のみ row callback を実行
//!   (range skip による軽量 virtualization)
//! - `row` callback は `&mut Ui<'_, M>` を受ける (M9 P1-5 menu item の `&mut Ui` 採用方針と一貫)
//! - drag-reorder は **本 widget には内蔵しない** (track_inspector 等で必要になったら別 widget
//!   `Ui::reorderable_list` を新設する)
//!
//! plugin_picker (daw_01 #007) の置換用、modal の body 内で使うのが想定 use case。

use std::cell::Cell;
use std::hash::Hash;

use daw_ui_renderer::{theme, Color, Rect, RectCommand};

use crate::ui::Ui;

/// `scroll_area` 内部の scrollbar 幅 (`scroll_area::SCROLLBAR_W` のミラー、
/// row 幅から差し引くために再宣言)。
const SCROLLBAR_W: f32 = 10.0;

#[derive(Clone, Copy, Debug)]
pub struct ListViewStyle {
    pub row_height: f32,
    pub row_gap: f32,
    pub row_bg: Color,
    pub row_bg_hover: Color,
    pub row_bg_selected: Color,
    pub radius: f32,
}

impl Default for ListViewStyle {
    fn default() -> Self {
        Self {
            row_height: 26.0,
            row_gap: 2.0,
            row_bg: theme::PANEL_RAISED,
            row_bg_hover: theme::CONTROL_HOVER,
            row_bg_selected: theme::ACCENT,
            radius: 2.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ListViewResponse {
    /// hover 中の row index (任意フレーム、pointer が任意 row 上)。
    pub hovered: Option<usize>,
    /// このフレームで click された row index (`primary_just_released` 時)。
    pub clicked: Option<usize>,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// `scroll_area` 上の virtual list。各 row は `row` callback で描画する。
    ///
    /// 戻り値の `hovered` / `clicked` は library が自動判定 (row callback が独自に
    /// `button_at` 等で click 検出した場合は両方の経路で trigger され得るので、user 側で
    /// どちらを採用するかを決めること)。
    ///
    /// ※ `selected: Option<usize>` は **描画用ハイライトのみ**。selection 状態の管理は
    /// caller 責任 (本 widget は `Edit<M>` を発行しない)。
    pub fn list_view<T, F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        items: &[T],
        selected: Option<usize>,
        style: &ListViewStyle,
        mut row: F,
    ) -> ListViewResponse
    where
        F: FnMut(&mut Self, &T, usize, Rect, /* is_selected */ bool),
    {
        let row_total_h = style.row_height + style.row_gap;
        let item_count = items.len();
        let content_h = (item_count as f32) * row_total_h;
        let pointer = self.pointer;

        let needs_scrollbar = content_h > rect.h;
        let row_visible_w = if needs_scrollbar {
            (rect.w - SCROLLBAR_W).max(0.0)
        } else {
            rect.w
        };

        let hovered = Cell::new(None::<usize>);
        let clicked = Cell::new(None::<usize>);
        let style_copy = *style;

        self.scroll_area(
            (b"list_view", &id),
            rect,
            (rect.w, content_h),
            |ui, offset| {
                if item_count == 0 || row_total_h <= 0.0 {
                    return;
                }
                let visible_top = offset.1;
                let visible_bottom = offset.1 + rect.h;
                let i_start = (visible_top / row_total_h).floor().max(0.0) as usize;
                let i_end = ((visible_bottom / row_total_h).ceil() as usize).min(item_count);
                for (i, item) in items.iter().enumerate().take(i_end).skip(i_start) {
                    let row_y = rect.y - offset.1 + (i as f32) * row_total_h;
                    let row_rect = Rect {
                        x: rect.x,
                        y: row_y,
                        w: row_visible_w,
                        h: style_copy.row_height,
                    };
                    let inside =
                        pointer.pos.is_some_and(|(px, py)| row_rect.contains(px, py));
                    let is_sel = selected == Some(i);
                    let bg = if is_sel {
                        style_copy.row_bg_selected
                    } else if inside {
                        style_copy.row_bg_hover
                    } else {
                        style_copy.row_bg
                    };
                    ui.push_rect(RectCommand {
                        rect: row_rect,
                        fill: bg,
                        border: Color::TRANSPARENT,
                        border_width: 0.0,
                        radius: [style_copy.radius; 4],
                        clip_rect: None,
                    });
                    row(ui, item, i, row_rect, is_sel);

                    if inside {
                        hovered.set(Some(i));
                        if pointer.primary_just_released {
                            clicked.set(Some(i));
                        }
                    }
                }
            },
        );

        ListViewResponse { hovered: hovered.get(), clicked: clicked.get() }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use daw_ui_platform::PhysicalSize;
    use daw_ui_renderer::{Rect, Scene};

    use super::ListViewStyle;
    use crate::input::{FrameInput, PointerFrame};
    use crate::ui::UiHost;

    #[test]
    fn list_view_calls_row_for_each_visible_item() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ListViewStyle::default();
        let items: Vec<u32> = (0..5).collect();
        let calls = Cell::new(0u32);

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.list_view(
                "lv",
                Rect { x: 0.0, y: 0.0, w: 200.0, h: 200.0 },
                &items,
                None,
                &style,
                |_ui, _item, _i, _row_rect, _sel| {
                    calls.set(calls.get() + 1);
                },
            );
        });

        // row_total = 26 + 2 = 28、200 / 28 ≈ 7.14、ceil = 8 行 visible だが items は 5 個
        assert_eq!(calls.get(), 5, "全 5 row 呼ばれる (画面に収まる)");
    }

    #[test]
    fn list_view_skips_offscreen_rows_when_content_overflows() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ListViewStyle::default();
        let items: Vec<u32> = (0..1000).collect();
        let calls = Cell::new(0u32);

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.list_view(
                "lv",
                Rect { x: 0.0, y: 0.0, w: 200.0, h: 200.0 },
                &items,
                None,
                &style,
                |_ui, _item, _i, _row_rect, _sel| {
                    calls.set(calls.get() + 1);
                },
            );
        });

        // row_total = 28、200 / 28 ≈ 7.14 → 表示 8 行 + ceil/floor の境界で 1 行余裕
        // 全 1000 row のうち visible のみ呼ばれる (~10 row 以下)
        assert!(calls.get() < 20, "1000 row のうち画面外は skip ({})", calls.get());
        assert!(calls.get() > 0);
    }

    #[test]
    fn list_view_reports_hovered_and_clicked_on_release() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ListViewStyle::default();
        let items: Vec<u32> = (0..5).collect();
        let rect = Rect { x: 0.0, y: 0.0, w: 200.0, h: 200.0 };

        // row 1 (index 1) は y=28..54
        // pointer を row 1 中央に置いて release
        let click = PointerFrame {
            pos: Some((100.0, 40.0)),
            primary_just_released: true,
            ..PointerFrame::default()
        };

        let resp_hovered = Cell::new(None::<usize>);
        let resp_clicked = Cell::new(None::<usize>);

        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { pointer: click, ..Default::default() },
            |(), ui| {
                let r = ui.list_view("lv", rect, &items, None, &style, |_, _, _, _, _| {});
                resp_hovered.set(r.hovered);
                resp_clicked.set(r.clicked);
            },
        );

        assert_eq!(resp_hovered.get(), Some(1));
        assert_eq!(resp_clicked.get(), Some(1));
    }

    #[test]
    fn list_view_selected_index_renders_with_selected_bg() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ListViewStyle::default();
        let items: Vec<u32> = (0..3).collect();
        let rect = Rect { x: 0.0, y: 0.0, w: 200.0, h: 200.0 };

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.list_view("lv", rect, &items, Some(1), &style, |_, _, _, _, _| {});
        });

        // row index 1 の background が row_bg_selected で描画されている。
        // scene.rects 末尾に近い 3 行分を見て、index 1 の行が selected_bg と一致するか。
        // (scroll_area が末尾に scrollbar も push するが content_h <= rect.h なら scrollbar なし)
        let sel_bg = style.row_bg_selected;
        let found_selected = scene.iter_rects().any(|r| {
            (r.fill.r - sel_bg.r).abs() < 1e-6
                && (r.fill.g - sel_bg.g).abs() < 1e-6
                && (r.fill.b - sel_bg.b).abs() < 1e-6
        });
        assert!(found_selected, "selected index 1 の bg が出ている");
    }

    #[test]
    fn list_view_empty_items_renders_nothing() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ListViewStyle::default();
        let items: Vec<u32> = vec![];
        let calls = Cell::new(0u32);

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.list_view(
                "lv",
                Rect { x: 0.0, y: 0.0, w: 200.0, h: 200.0 },
                &items,
                None,
                &style,
                |_, _, _, _, _| {
                    calls.set(calls.get() + 1);
                },
            );
        });

        assert_eq!(calls.get(), 0, "空 list で row callback は 0 回");
    }
}
