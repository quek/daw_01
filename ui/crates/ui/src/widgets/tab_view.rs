//! `tab_view` widget — タブで切り替え可能な複数 view (M7 Phase 26)。
//!
//! M9 P0-2: `tab_view_with_state(id, rect, &mut usize, ...)` を追加し、外部から
//! selected を制御可能にした (clip → Piano Roll タブ遷移など)。内部 state 版
//! `tab_view` も併存し、同 id なら widget_state を共有して途中切替もできる。
//!
//! builder パターンで `tabs.tab(label, |ui, pane_rect| ...)` を順に並べる。
//! 選択中のタブだけ closure を実行 (= 各 closure は `FnOnce` でも問題なし)。

use std::hash::Hash;

use daw_ui_renderer::{Color, GlyphArea, Rect, RectCommand};

use crate::id::WidgetId;
use crate::ui::Ui;

const TAB_BAR_H: f32 = 32.0;
const TAB_FONT: f32 = 14.0;
const TAB_PAD_X: f32 = 16.0;

/// `Ui::tab_view` の永続状態 (現在選択中の index)。
#[derive(Debug, Default)]
pub(crate) struct TabState {
    pub selected: usize,
}

/// `Ui::tab_view` のビルダー。`tab(label, |ui, pane_rect| ...)` で各タブを宣言する。
pub struct TabBuilder<'b, 'a, M: ?Sized + 'static> {
    ui: &'b mut Ui<'a, M>,
    bar_rect: Rect,
    pane_rect: Rect,
    next_x: f32,
    selected: usize,
    /// 0,1,2... と順に増える index counter
    next_index: usize,
    /// このフレームで click された index (label 描画フェーズで決定、フレーム末で selected に反映)
    clicked: Option<usize>,
}

impl<'b, 'a, M: ?Sized + 'static> TabBuilder<'b, 'a, M> {
    /// 1 つのタブを宣言。`label` がバーに表示される、選択中なら `f` が呼ばれて pane に描画。
    pub fn tab<F>(&mut self, label: &str, f: F)
    where
        F: FnOnce(&mut Ui<'a, M>, Rect),
    {
        let i = self.next_index;
        self.next_index += 1;

        // タブ label の rect (幅は文字数 × 8px + padding)
        let w = (label.chars().count() as f32) * 8.0 + TAB_PAD_X * 2.0;
        let tab_rect = Rect { x: self.bar_rect.x + self.next_x, y: self.bar_rect.y, w, h: TAB_BAR_H };
        self.next_x += w;

        let pointer = self.ui.pointer();
        let inside = pointer.pos.is_some_and(|(px, py)| tab_rect.contains(px, py));
        if inside && pointer.primary_just_released {
            self.clicked = Some(i);
        }

        let is_sel = i == self.selected;
        let p = self.ui.palette();
        let fill = if is_sel {
            p.panel_raised
        } else if inside {
            p.panel
        } else {
            Color::TRANSPARENT
        };
        if fill.a > 0.0 {
            self.ui.push_rect(RectCommand {
                rect: tab_rect,
                fill,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        }
        self.ui.push_text(GlyphArea {
            text: label.into(),
            left: tab_rect.x + TAB_PAD_X,
            top: tab_rect.y + (tab_rect.h - TAB_FONT * 1.2) * 0.5,
            font_size: TAB_FONT,
            line_height: TAB_FONT * 1.2,
            color: if is_sel { p.text } else { p.text_dim },
            clip_rect: None,
            ..GlyphArea::default()
        });

        // 選択中なら pane を描画 (clip 適用)
        if is_sel {
            let pane = self.pane_rect;
            self.ui.with_clip_rect(pane, |ui| f(ui, pane));
        }
    }
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// tab_view widget (内部 state 版)。`rect` 上部にタブバー、下に選択中タブの中身。
    /// builder を使って `tabs.tab(label, |ui, pane_rect| { ... })` を並べる。
    /// 選択中タブの closure のみ実行される (非選択は呼ばれない)。
    ///
    /// 選択 index は `widget_state` に保持され、同 id 内で永続。外部から selected を
    /// 制御したい場合は `tab_view_with_state` を使うこと (同 id なら state 共有)。
    pub fn tab_view<F>(&mut self, id: impl Hash, rect: Rect, f: F)
    where
        F: FnOnce(&mut TabBuilder<'_, 'a, M>),
    {
        let wid = WidgetId::ROOT.child((b"tab_view", &id));
        let initial = {
            let state: &mut TabState = self.widget_state(wid);
            state.selected
        };
        let final_idx = self.tab_view_inner(rect, initial, f);
        if final_idx != initial {
            let state: &mut TabState = self.widget_state(wid);
            state.selected = final_idx;
            self.request_redraw();
        }
    }

    /// tab_view widget (外部 state 版)。`selected: &mut usize` を借りて selected を
    /// 外部制御可能にする。タブクリックで `*selected` が更新される。
    ///
    /// `*selected` がタブ数を超えていた場合は last-valid に clamp + 書き戻し
    /// (no-panic、dynamic な tab 増減でも safely 動く)。
    ///
    /// 同 id で `tab_view` と `tab_view_with_state` を切替えても widget_state は
    /// 共有される (内部 state を `*selected` に強制 sync するため、外部値が優先)。
    pub fn tab_view_with_state<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        selected: &mut usize,
        f: F,
    ) where
        F: FnOnce(&mut TabBuilder<'_, 'a, M>),
    {
        let wid = WidgetId::ROOT.child((b"tab_view", &id));
        // 外部 → 内部 sync (`tab_view` と混在しても外部優先)
        {
            let state: &mut TabState = self.widget_state(wid);
            state.selected = *selected;
        }
        let final_idx = self.tab_view_inner(rect, *selected, f);
        if final_idx != *selected {
            *selected = final_idx;
            self.request_redraw();
        }
        let state: &mut TabState = self.widget_state(wid);
        state.selected = final_idx;
    }

    /// タブバー描画 + builder 駆動 + click 集約 + 範囲外 clamp までを行う共通実装。
    /// `initial_selected` を受け取り、フレーム末に確定した selected (clamp 後) を返す。
    fn tab_view_inner<F>(&mut self, rect: Rect, initial_selected: usize, f: F) -> usize
    where
        F: FnOnce(&mut TabBuilder<'_, 'a, M>),
    {
        // バー背景
        let bar_rect = Rect { x: rect.x, y: rect.y, w: rect.w, h: TAB_BAR_H };
        let p = self.palette();
        self.push_rect(RectCommand {
            rect: bar_rect,
            fill: p.header,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: None,
        });

        let pane_rect = Rect {
            x: rect.x,
            y: rect.y + TAB_BAR_H,
            w: rect.w,
            h: (rect.h - TAB_BAR_H).max(0.0),
        };

        let (clicked, n) = {
            let mut builder = TabBuilder {
                ui: self,
                bar_rect,
                pane_rect,
                next_x: 0.0,
                selected: initial_selected,
                next_index: 0,
                clicked: None,
            };
            f(&mut builder);
            (builder.clicked, builder.next_index)
        };

        if n == 0 {
            return 0;
        }
        let last = n - 1;
        clicked.map_or_else(|| initial_selected.min(last), |c| c.min(last))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::Edit;
    use crate::input::{FrameInput, PointerFrame};
    use crate::ui::UiHost;
    use daw_ui_platform::PhysicalSize;
    use daw_ui_renderer::Scene;

    struct Counter {
        hits: [u32; 4],
    }
    impl Counter {
        fn new() -> Self {
            Self { hits: [0; 4] }
        }
    }

    fn screen() -> PhysicalSize {
        PhysicalSize { width: 400, height: 200 }
    }
    fn full_rect() -> Rect {
        Rect { x: 0.0, y: 0.0, w: 400.0, h: 200.0 }
    }

    #[test]
    fn tab_view_with_state_respects_external() {
        let mut host: UiHost<Counter> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let mut model = Counter::new();
        let mut external = 2usize;

        let edits = host.frame_to_edits(
            &model,
            &mut scene,
            screen(),
            FrameInput::default(),
            |_, ui| {
                ui.tab_view_with_state("test", full_rect(), &mut external, |tabs| {
                    tabs.tab("A", |ui, _| {
                        ui.push_edit(Edit::mutate(|m: &mut Counter| m.hits[0] += 1));
                    });
                    tabs.tab("B", |ui, _| {
                        ui.push_edit(Edit::mutate(|m: &mut Counter| m.hits[1] += 1));
                    });
                    tabs.tab("C", |ui, _| {
                        ui.push_edit(Edit::mutate(|m: &mut Counter| m.hits[2] += 1));
                    });
                    tabs.tab("D", |ui, _| {
                        ui.push_edit(Edit::mutate(|m: &mut Counter| m.hits[3] += 1));
                    });
                });
            },
        );
        for e in edits {
            e.apply(&mut model);
        }
        assert_eq!(
            model.hits,
            [0, 0, 1, 0],
            "external = 2 → tab C のみ closure が呼ばれる"
        );
        assert_eq!(external, 2, "click なしで external 不変");
    }

    #[test]
    fn tab_view_with_state_writes_back_on_click() {
        // tab bar layout: 各 tab 幅 = chars * 8.0 + TAB_PAD_X (16) * 2 = chars*8 + 32
        // "A" / "B" / "C" は全部 1 char → 各 40px、TAB_BAR_H = 32px
        // tab[0]: x ∈ [0, 40), tab[1]: x ∈ [40, 80), tab[2]: x ∈ [80, 120)
        let mut host: UiHost<Counter> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let model = Counter::new();
        let mut external = 0usize;

        let pointer = PointerFrame {
            pos: Some((60.0, 16.0)), // tab[1] = "B" の中央
            primary_just_released: true,
            ..PointerFrame::default()
        };
        let _edits = host.frame_to_edits(
            &model,
            &mut scene,
            screen(),
            FrameInput { pointer, ..Default::default() },
            |_, ui| {
                ui.tab_view_with_state("test", full_rect(), &mut external, |tabs| {
                    tabs.tab("A", |_, _| {});
                    tabs.tab("B", |_, _| {});
                    tabs.tab("C", |_, _| {});
                });
            },
        );
        assert_eq!(external, 1, "tab B (x=40..80) click で external = 1 に書き戻し");
    }

    #[test]
    fn tab_view_with_state_clamps_out_of_bounds() {
        let mut host: UiHost<Counter> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let model = Counter::new();
        let mut external = 99usize;

        let _edits = host.frame_to_edits(
            &model,
            &mut scene,
            screen(),
            FrameInput::default(),
            |_, ui| {
                ui.tab_view_with_state("test", full_rect(), &mut external, |tabs| {
                    tabs.tab("A", |_, _| {});
                    tabs.tab("B", |_, _| {});
                });
            },
        );
        assert_eq!(external, 1, "tab 数 2 → last-valid (1) に clamp + 書き戻し");
    }

    #[test]
    fn tab_view_with_state_clamps_to_zero_when_empty_initial_oversize() {
        // initial が大きいが tab 数 1 のケース: 0 に clamp
        let mut host: UiHost<Counter> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let model = Counter::new();
        let mut external = 5usize;

        let _edits = host.frame_to_edits(
            &model,
            &mut scene,
            screen(),
            FrameInput::default(),
            |_, ui| {
                ui.tab_view_with_state("test", full_rect(), &mut external, |tabs| {
                    tabs.tab("Only", |_, _| {});
                });
            },
        );
        assert_eq!(external, 0);
    }

    #[test]
    fn tab_view_internal_and_with_state_share_widget_state() {
        // 同 id で tab_view → tab_view_with_state と切替えると、外部値が internal を上書きする。
        let mut host: UiHost<Counter> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let mut model = Counter::new();

        // Frame 1: 内部 state 版で tab[1] を click → 内部 selected = 1 になる
        let pointer = PointerFrame {
            pos: Some((60.0, 16.0)),
            primary_just_released: true,
            ..PointerFrame::default()
        };
        let edits = host.frame_to_edits(
            &model,
            &mut scene,
            screen(),
            FrameInput { pointer, ..Default::default() },
            |_, ui| {
                ui.tab_view("shared", full_rect(), |tabs| {
                    tabs.tab("A", |ui, _| {
                        ui.push_edit(Edit::mutate(|m: &mut Counter| m.hits[0] += 1));
                    });
                    tabs.tab("B", |ui, _| {
                        ui.push_edit(Edit::mutate(|m: &mut Counter| m.hits[1] += 1));
                    });
                    tabs.tab("C", |ui, _| {
                        ui.push_edit(Edit::mutate(|m: &mut Counter| m.hits[2] += 1));
                    });
                });
            },
        );
        for e in edits {
            e.apply(&mut model);
        }
        // frame 1 は古い selected = 0 で描画 → tab A
        assert_eq!(model.hits, [1, 0, 0, 0]);

        // Frame 2: 外部 state 版で external = 0 を渡す → 外部値が internal の 1 を上書き
        let mut external = 0usize;
        let edits = host.frame_to_edits(
            &model,
            &mut scene,
            screen(),
            FrameInput::default(),
            |_, ui| {
                ui.tab_view_with_state("shared", full_rect(), &mut external, |tabs| {
                    tabs.tab("A", |ui, _| {
                        ui.push_edit(Edit::mutate(|m: &mut Counter| m.hits[0] += 1));
                    });
                    tabs.tab("B", |ui, _| {
                        ui.push_edit(Edit::mutate(|m: &mut Counter| m.hits[1] += 1));
                    });
                    tabs.tab("C", |ui, _| {
                        ui.push_edit(Edit::mutate(|m: &mut Counter| m.hits[2] += 1));
                    });
                });
            },
        );
        for e in edits {
            e.apply(&mut model);
        }
        // 外部 0 が internal 1 を上書き → tab A 描画
        assert_eq!(model.hits, [2, 0, 0, 0], "外部値が優先されて tab A");
        assert_eq!(external, 0);
    }
}

