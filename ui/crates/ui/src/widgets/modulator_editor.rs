//! モジュレーター用グラフィカルエディタ (daw_01 / Bitwig 同等)。
//!
//! 3 つの widget を提供する。すべて **model 非依存** (no-Clone 不変条件を守り、
//! 描画用のサンプル列とノード座標を受け取り、編集は `*Action` を返す closure 経由で
//! 1 要素だけ書き換える):
//!
//! - [`Ui::mseg_editor`] — 多段エンベロープ (MSEG) のカーブキャンバス。ノードを
//!   time+value でドラッグ、空白ダブルクリックで点追加、セグメント中央の縦ドラッグで
//!   tension、Alt+クリック / 右クリック / Delete で削除、ライブ位相カーソル重畳。
//! - [`Ui::step_grid`] — ステップシーケンサのバーグリッド。ドラッグで値を描画、
//!   走査中ステップをハイライト。
//! - [`Ui::signal_preview`] — 読み取り専用の波形プレビュー (LFO / Random 用)。
//!   ポリライン + 位相カーソル。
//!
//! **描画は呼び出し側が用意した `samples` (データ空間 `[0,1]×[0,1]` のポリライン) を
//! そのまま px へ写すだけ**。MSEG なら `common::modulators::mseg_sample`、LFO/Random
//! なら `generator_scalar` を 1 周期サンプルした列を渡す。これで「描画 == 評価」が
//! 構造的に保証される (SSoT、drift ゼロ)。

use std::hash::Hash;

use daw_ui_renderer::{theme, Color, LineBatch, LineSegment, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::scenegraph::hash_inputs;
use crate::ui::{Ui, hovered};

/// MSEG の 1 ブレークポイント (描画 / hit-test 用の plain コピー)。`curve` は
/// 「この点から次の点へ向かうセグメント」の tension (-1..=1)。
#[derive(Debug, Clone, Copy)]
pub struct MsegNode {
    pub time: f32,
    pub value: f32,
    pub curve: f32,
}

/// `mseg_editor` がこのフレームに検出した編集操作 (高々 1 つ)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MsegAction {
    /// ノード `index` を time/value へ移動 (両端は time 固定)。
    Move { index: usize, time: f32, value: f32 },
    /// セグメント `segment` (点 `segment`→`segment+1`) の tension を設定。
    SetCurve { segment: usize, curve: f32 },
    /// 空白ダブルクリックで time/value に点追加。
    Add { time: f32, value: f32 },
    /// 内側ノード `index` を削除。
    Delete { index: usize },
}

#[derive(Debug, Clone, Copy)]
pub struct MsegEditorStyle {
    pub bg: Color,
    pub grid: Color,
    pub line_color: Color,
    pub line_width_px: f32,
    pub node_color: Color,
    pub node_hover_color: Color,
    pub node_drag_color: Color,
    pub node_radius_px: f32,
    pub tension_color: Color,
    pub cursor_color: Color,
}

impl Default for MsegEditorStyle {
    fn default() -> Self {
        Self {
            bg: theme::INSET_BG,
            grid: theme::GRID_LINE,
            line_color: theme::CURVE,
            line_width_px: 2.0,
            node_color: theme::TEXT,
            node_hover_color: theme::SELECTION_WARM,
            node_drag_color: theme::WAVEFORM_PEAK,
            node_radius_px: 5.0,
            tension_color: theme::SELECTION_WARM.with_alpha(0.7),
            cursor_color: theme::SELECTION_WARM.with_alpha(0.85),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MsegEditorResponse {
    pub hovered: bool,
    /// ノード or tension をドラッグ中 (caller の sync 抑制エッジ用)。
    pub dragging: bool,
}

#[derive(Debug, Default)]
pub(crate) struct MsegEditorState {
    /// ドラッグ中のノード index。
    node_drag: Option<usize>,
    /// tension ドラッグ: (segment, 開始時 curve, 開始時 mouse_y)。
    curve_drag: Option<(usize, f32, f32)>,
}

/// `samples` (x 昇順のポリライン) を x で線形補間して y を返す。tension handle の
/// 縦位置をカーブ上に乗せるため。
fn sample_y_at(samples: &[(f32, f32)], x: f32) -> f32 {
    match samples {
        [] => 0.0,
        [only] => only.1,
        _ => {
            if x <= samples[0].0 {
                return samples[0].1;
            }
            let last = samples[samples.len() - 1];
            if x >= last.0 {
                return last.1;
            }
            for w in samples.windows(2) {
                if x >= w[0].0 && x <= w[1].0 {
                    let span = (w[1].0 - w[0].0).max(f32::MIN_POSITIVE);
                    let t = (x - w[0].0) / span;
                    return w[0].1 + (w[1].1 - w[0].1) * t;
                }
            }
            last.1
        }
    }
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// MSEG カーブエディタ。詳細は module doc。`on_action` はこのフレームに 1 度だけ
    /// 検出した操作に対し呼ばれ、戻り `Edit` が push される。
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn mseg_editor<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        nodes: &[MsegNode],
        samples: &[(f32, f32)],
        phase: Option<f32>,
        style: MsegEditorStyle,
        on_action: F,
    ) -> MsegEditorResponse
    where
        F: FnOnce(MsegAction) -> Edit<M>,
    {
        let wid = WidgetId::ROOT.child((b"mseg_editor", &id));
        let pointer = self.pointer;
        let n = nodes.len();
        let mut response = MsegEditorResponse {
            hovered: hovered(rect, pointer),
            dragging: false,
        };

        let to_px = |t: f32, v: f32| (rect.x + t * rect.w, rect.y + (1.0 - v) * rect.h);

        // --- hit-test: node (radius*2 で甘め) ---
        let hovered_node: Option<usize> = pointer.pos.and_then(|(px, py)| {
            let r2 = (style.node_radius_px * 2.0).powi(2);
            nodes.iter().position(|nd| {
                let (nx, ny) = to_px(nd.time, nd.value);
                (px - nx).powi(2) + (py - ny).powi(2) <= r2
            })
        });
        // --- hit-test: tension handle (各セグメント中点、ノードに当たってない時のみ) ---
        let tension_handle_px = |seg: usize| -> (f32, f32) {
            let mt = (nodes[seg].time + nodes[seg + 1].time) * 0.5;
            to_px(mt, sample_y_at(samples, mt))
        };
        let hovered_tension: Option<usize> = if hovered_node.is_some() {
            None
        } else {
            pointer.pos.and_then(|(px, py)| {
                let r2 = (style.node_radius_px * 2.2).powi(2);
                (0..n.saturating_sub(1)).find(|&seg| {
                    // 水平に潰れたセグメントは tension が見えないので除外。
                    if (nodes[seg + 1].time - nodes[seg].time).abs() < 1e-3 {
                        return false;
                    }
                    let (hx, hy) = tension_handle_px(seg);
                    (px - hx).powi(2) + (py - hy).powi(2) <= r2
                })
            })
        };

        // --- drag state 更新 + action 抽出 (scope を分けて借用を early release) ---
        let mut action: Option<MsegAction> = None;
        {
            let st: &mut MsegEditorState = self.widget_state(wid);
            // press: Alt+ノード = 削除、ノード = drag開始、tension = curve drag開始。
            if pointer.primary_just_pressed {
                if let Some(idx) = hovered_node {
                    if pointer.modifiers.alt && idx > 0 && idx + 1 < n {
                        action = Some(MsegAction::Delete { index: idx });
                    } else {
                        st.node_drag = Some(idx);
                    }
                } else if let Some(seg) = hovered_tension {
                    st.curve_drag = Some((seg, nodes[seg].curve, pointer.pos.map_or(0.0, |p| p.1)));
                }
            }
            if pointer.primary_just_released {
                st.node_drag = None;
                st.curve_drag = None;
            }
            // stale clear (caller が点を削除して len が縮んだ)。
            if let Some(idx) = st.node_drag
                && idx >= n
            {
                st.node_drag = None;
            }
            if let Some((seg, _, _)) = st.curve_drag
                && seg + 1 >= n
            {
                st.curve_drag = None;
            }

            // node drag → Move (両端 time 固定、中間は隣接間 clamp で単調維持)。
            if let Some(idx) = st.node_drag
                && idx < n
                && let Some((px, py)) = pointer.pos
            {
                response.dragging = true;
                let raw_t = ((px - rect.x) / rect.w.max(1.0)).clamp(0.0, 1.0);
                let value = (1.0 - (py - rect.y) / rect.h.max(1.0)).clamp(0.0, 1.0);
                let time = if idx == 0 {
                    0.0
                } else if idx == n - 1 {
                    1.0
                } else {
                    let lo = nodes[idx - 1].time + 1e-3;
                    let hi = nodes[idx + 1].time - 1e-3;
                    raw_t.clamp(lo.min(hi), hi.max(lo))
                };
                action = Some(MsegAction::Move { index: idx, time, value });
            }
            // tension drag → SetCurve。 `apply_tension` は補間パラメータ t を歪めるため、
            // 同じ curve 値でも **上昇セグメントは上に、 下降セグメントは下に** 膨らむ
            // (符号が逆)。 そこで segment の傾きで drag の符号を反転し、 「上げたら必ず
            // 上に膨らむ」 を上り/下りに依らず一貫させる (Bitwig と同じ)。
            if action.is_none()
                && let Some((seg, anchor_curve, anchor_y)) = st.curve_drag
                && let Some((_, py)) = pointer.pos
            {
                response.dragging = true;
                let sens = 3.0 / rect.h.max(1.0);
                let rising = nodes[seg + 1].value >= nodes[seg].value;
                let dir = if rising { 1.0 } else { -1.0 };
                let curve = (anchor_curve - (py - anchor_y) * sens * dir).clamp(-1.0, 1.0);
                action = Some(MsegAction::SetCurve { segment: seg, curve });
            }
        }

        // --- 空白ダブルクリックで点追加 (ノード上は除外) ---
        if action.is_none()
            && hovered_node.is_none()
            && let Some((px, py)) = self.take_double_click_in_rect(rect)
        {
            let t = ((px - rect.x) / rect.w.max(1.0)).clamp(0.0, 1.0);
            let v = (1.0 - (py - rect.y) / rect.h.max(1.0)).clamp(0.0, 1.0);
            action = Some(MsegAction::Add { time: t, value: v });
        }
        // --- 右クリック or Delete キーで hover 中の内側ノード削除 ---
        // (両端は不可。context_menu は `FnOnce` on_action を再利用できないため、削除は
        //  Alt+クリック / 右クリック / Delete をすべて単一の `on_action` 経路に集約する。)
        if action.is_none()
            && let Some(idx) = hovered_node
            && idx > 0
            && idx + 1 < n
            && (self.take_secondary_press_in_rect(rect).is_some() || self.take_shortcut("delete"))
        {
            action = Some(MsegAction::Delete { index: idx });
        }

        if let Some(act) = action {
            let edit = on_action(act);
            self.push_edit(edit);
        }

        // --- 描画 (cached widget node) ---
        let node_bits: Vec<u32> = nodes
            .iter()
            .flat_map(|nd| [nd.time.to_bits(), nd.value.to_bits(), nd.curve.to_bits()])
            .collect();
        let sample_bits: Vec<u32> = samples
            .iter()
            .flat_map(|&(x, y)| [x.to_bits(), y.to_bits()])
            .collect();
        let drag_tag = self
            .widget_state::<MsegEditorState>(wid)
            .node_drag
            .map_or(u32::MAX, |i| i as u32);
        let input_hash = hash_inputs((
            b"mseg_editor",
            (rect.x.to_bits(), rect.y.to_bits(), rect.w.to_bits(), rect.h.to_bits()),
            node_bits,
            sample_bits,
            (drag_tag, hovered_node.map_or(u32::MAX, |i| i as u32)),
            (hovered_tension.map_or(u32::MAX, |i| i as u32), phase.map_or(u32::MAX, f32::to_bits)),
        ));

        let nodes_owned: Vec<MsegNode> = nodes.to_vec();
        let samples_owned: Vec<(f32, f32)> = samples.to_vec();
        self.with_widget_node(wid, input_hash, move |ui| {
            // 背景 + grid (0/.25/.5/.75/1 横線、中央縦線)。
            ui.push_rect(RectCommand {
                rect,
                fill: style.bg,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [3.0; 4],
                clip_rect: None,
            });
            let mut grid: Vec<LineSegment> = Vec::new();
            for k in 0..=4 {
                let gy = rect.y + (k as f32 / 4.0) * rect.h;
                grid.push(LineSegment { a: [rect.x, gy], b: [rect.x + rect.w, gy], color: style.grid });
            }
            ui.push_lines(LineBatch { segments: grid.into(), line_width_px: 1.0, clip_rect: Some(rect) });

            // カーブ (samples を px 化)。
            if samples_owned.len() >= 2 {
                let segs: Vec<LineSegment> = samples_owned
                    .windows(2)
                    .map(|w| {
                        let (ax, ay) = (rect.x + w[0].0 * rect.w, rect.y + (1.0 - w[0].1) * rect.h);
                        let (bx, by) = (rect.x + w[1].0 * rect.w, rect.y + (1.0 - w[1].1) * rect.h);
                        LineSegment { a: [ax, ay], b: [bx, by], color: style.line_color }
                    })
                    .collect();
                ui.push_lines(LineBatch {
                    segments: segs.into(),
                    line_width_px: style.line_width_px.max(1.0),
                    clip_rect: Some(rect),
                });
            }

            // tension handle (各セグメント中点の小ダイヤ)。
            for seg in 0..nodes_owned.len().saturating_sub(1) {
                if (nodes_owned[seg + 1].time - nodes_owned[seg].time).abs() < 1e-3 {
                    continue;
                }
                let mt = (nodes_owned[seg].time + nodes_owned[seg + 1].time) * 0.5;
                let my = sample_y_at(&samples_owned, mt);
                let (hx, hy) = (rect.x + mt * rect.w, rect.y + (1.0 - my) * rect.h);
                let r = style.node_radius_px * 0.6;
                ui.push_rect(RectCommand {
                    rect: Rect { x: hx - r, y: hy - r, w: r * 2.0, h: r * 2.0 },
                    fill: style.tension_color,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [r; 4],
                    clip_rect: Some(rect),
                });
            }

            // ノード (角丸円)。
            for (i, nd) in nodes_owned.iter().enumerate() {
                let (nx, ny) = (rect.x + nd.time * rect.w, rect.y + (1.0 - nd.value) * rect.h);
                let r = style.node_radius_px;
                let fill = if Some(i as u32) == Some(drag_tag) {
                    style.node_drag_color
                } else if Some(i) == hovered_node {
                    style.node_hover_color
                } else {
                    style.node_color
                };
                ui.push_rect(RectCommand {
                    rect: Rect { x: nx - r, y: ny - r, w: r * 2.0, h: r * 2.0 },
                    fill,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [r; 4],
                    clip_rect: None,
                });
            }

            // ライブ位相カーソル (縦線)。
            if let Some(ph) = phase {
                let cx = rect.x + ph.clamp(0.0, 1.0) * rect.w;
                ui.push_lines(LineBatch {
                    segments: vec![LineSegment {
                        a: [cx, rect.y],
                        b: [cx, rect.y + rect.h],
                        color: style.cursor_color,
                    }]
                    .into(),
                    line_width_px: 1.5,
                    clip_rect: Some(rect),
                });
            }
        });

        response
    }

    /// ステップシーケンサのバーグリッド。ドラッグで各ステップ値を描画する
    /// (Bitwig 流 click-drag draw)。`current` は走査中ステップ (ライブ playhead)。
    #[allow(clippy::too_many_arguments)]
    pub fn step_grid<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        values: &[f32],
        current: Option<usize>,
        style: MsegEditorStyle,
        on_set: F,
    ) -> MsegEditorResponse
    where
        F: FnOnce(usize, f32) -> Edit<M>,
    {
        let wid = WidgetId::ROOT.child((b"step_grid", &id));
        let pointer = self.pointer;
        let n = values.len();
        let mut response = MsegEditorResponse { hovered: hovered(rect, pointer), dragging: false };
        if n == 0 {
            return response;
        }
        let cell = rect.w / n as f32;

        let mut set: Option<(usize, f32)> = None;
        {
            let st: &mut StepGridState = self.widget_state(wid);
            if pointer.primary_just_pressed
                && pointer.pos.is_some_and(|(px, py)| rect.contains(px, py))
            {
                st.painting = true;
            }
            if pointer.primary_just_released {
                st.painting = false;
            }
            if st.painting
                && let Some((px, py)) = pointer.pos
            {
                response.dragging = true;
                let idx = (((px - rect.x) / cell.max(1.0)) as usize).min(n - 1);
                let value = (1.0 - (py - rect.y) / rect.h.max(1.0)).clamp(0.0, 1.0);
                set = Some((idx, value));
            }
        }
        if let Some((idx, value)) = set {
            let edit = on_set(idx, value);
            self.push_edit(edit);
        }

        let value_bits: Vec<u32> = values.iter().map(|v| v.to_bits()).collect();
        let input_hash = hash_inputs((
            b"step_grid",
            (rect.x.to_bits(), rect.y.to_bits(), rect.w.to_bits(), rect.h.to_bits()),
            value_bits,
            current.map_or(u32::MAX, |i| i as u32),
        ));
        let values_owned: Vec<f32> = values.to_vec();
        self.with_widget_node(wid, input_hash, move |ui| {
            ui.push_rect(RectCommand {
                rect,
                fill: style.bg,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [3.0; 4],
                clip_rect: None,
            });
            for (i, &v) in values_owned.iter().enumerate() {
                let x = rect.x + i as f32 * cell;
                let bar_h = v.clamp(0.0, 1.0) * rect.h;
                // 走査中ステップは背景を明るく。
                if Some(i) == current {
                    ui.push_rect(RectCommand {
                        rect: Rect { x, y: rect.y, w: cell, h: rect.h },
                        fill: theme::SELECTION_WARM.with_alpha(0.16),
                        border: Color::TRANSPARENT,
                        border_width: 0.0,
                        radius: [0.0; 4],
                        clip_rect: Some(rect),
                    });
                }
                ui.push_rect(RectCommand {
                    rect: Rect { x: x + 1.0, y: rect.y + rect.h - bar_h, w: (cell - 2.0).max(1.0), h: bar_h },
                    fill: if Some(i) == current { style.node_hover_color } else { style.line_color },
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [1.0; 4],
                    clip_rect: Some(rect),
                });
            }
        });
        response
    }

    /// 読み取り専用の波形プレビュー (LFO / Random)。`samples` をポリラインで描き、
    /// `phase` があれば縦カーソルを重ねる。
    pub fn signal_preview(
        &mut self,
        id: impl Hash,
        rect: Rect,
        samples: &[(f32, f32)],
        phase: Option<f32>,
        style: MsegEditorStyle,
    ) {
        let wid = WidgetId::ROOT.child((b"signal_preview", &id));
        let sample_bits: Vec<u32> = samples
            .iter()
            .flat_map(|&(x, y)| [x.to_bits(), y.to_bits()])
            .collect();
        let input_hash = hash_inputs((
            b"signal_preview",
            (rect.x.to_bits(), rect.y.to_bits(), rect.w.to_bits(), rect.h.to_bits()),
            sample_bits,
            phase.map_or(u32::MAX, f32::to_bits),
        ));
        let samples_owned: Vec<(f32, f32)> = samples.to_vec();
        self.with_widget_node(wid, input_hash, move |ui| {
            ui.push_rect(RectCommand {
                rect,
                fill: style.bg,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [3.0; 4],
                clip_rect: None,
            });
            // 中央線。
            let midy = rect.y + rect.h * 0.5;
            ui.push_lines(LineBatch {
                segments: vec![LineSegment { a: [rect.x, midy], b: [rect.x + rect.w, midy], color: style.grid }]
                    .into(),
                line_width_px: 1.0,
                clip_rect: Some(rect),
            });
            if samples_owned.len() >= 2 {
                let segs: Vec<LineSegment> = samples_owned
                    .windows(2)
                    .map(|w| {
                        let (ax, ay) = (rect.x + w[0].0 * rect.w, rect.y + (1.0 - w[0].1) * rect.h);
                        let (bx, by) = (rect.x + w[1].0 * rect.w, rect.y + (1.0 - w[1].1) * rect.h);
                        LineSegment { a: [ax, ay], b: [bx, by], color: style.line_color }
                    })
                    .collect();
                ui.push_lines(LineBatch {
                    segments: segs.into(),
                    line_width_px: style.line_width_px.max(1.0),
                    clip_rect: Some(rect),
                });
            }
            if let Some(ph) = phase {
                let cx = rect.x + ph.clamp(0.0, 1.0) * rect.w;
                ui.push_lines(LineBatch {
                    segments: vec![LineSegment {
                        a: [cx, rect.y],
                        b: [cx, rect.y + rect.h],
                        color: style.cursor_color,
                    }]
                    .into(),
                    line_width_px: 1.5,
                    clip_rect: Some(rect),
                });
            }
        });
    }
}

#[derive(Debug, Default)]
pub(crate) struct StepGridState {
    painting: bool,
}
