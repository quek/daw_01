//! `fader` ウィジェット — 垂直スライダ。
//!
//! `scale = None` のとき値範囲は `0.0..=1.0` (従来どおり)。
//! `scale = Some(s)` のとき値は dB 値で渡し、widget が `MeterScale` カーブで内部変換する。
//! これにより `level_meter_stereo` と同一 `MeterScale` を共有するとカーブが必ず一致する (SSoT)。
//!
//! 設計の要点:
//! - 値は **アプリ側 Model が所有**。ライブラリは「現在値を借りて描き、ドラッグで
//!   新値を計算して `Edit<M>` を発行する」だけ。
//! - ドラッグ状態は `WidgetId` キーで `state` HashMap に持つ (`FaderState`)。
//!   no-Clone 制約を維持するため Model 側に状態を持たせない。
//! - 縦方向ドラッグ: 上 = 値増加、下 = 値減少。1 widget 高さ全部使う = 0 → 1 (fraction 空間)。
//! - **DAW 標準挙動**:
//!   - ダブルクリックで `default_value` に戻る (~300ms × 5px 以内の 2 回目 press)
//!   - Ctrl + ドラッグで感度 1/10 (高精度)。Mid-drag toggle で値が jump しないよう再 anchor

use std::hash::Hash;
use std::time::Instant;

use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::scenegraph::hash_inputs;
use crate::ui::{Ui, hovered};
use crate::widgets::level_meter::MeterScale;
use crate::widgets::scrubable_number::{ModEntry, Modulation};

const TRACK_PAD: f32 = 8.0;
const THUMB_W: f32 = 28.0;
const THUMB_H: f32 = 10.0;

/// ダブルクリック判定の時間しきい値 (ms)。
const DOUBLE_CLICK_MS: u128 = 300;
/// ダブルクリック判定の位置しきい値 (px)。
const DOUBLE_CLICK_PX: f32 = 5.0;
/// Ctrl + ドラッグ時の感度倍率 (1/10)。
const FINE_DRAG_SCALE: f32 = 0.1;
/// depth-edit gesture を「実 drag」 と見なす最小縦移動量 (px)。 これ未満の press→release は micro-jitter
/// として depth Edit を発火させず `mod_dragging` も立てない (knob #109 / scrubable #107 と同義)。
const DRAG_THRESHOLD_PX: f32 = 4.0;

/// fader の永続状態 (フレーム間で保持)。
#[derive(Debug, Default)]
pub(crate) struct FaderState {
    drag_anchor: Option<DragAnchor>,
    /// 直近のクリック (ダブルクリック判定用)。
    last_click: Option<ClickRecord>,
    /// M8 Phase 29: drag 開始時の値 (release frame で undoable Edit の inverse に使う)。
    drag_initial_value: Option<f32>,
    /// #110: press からの最大縦移動量 (px)。 depth gesture の `mod_dragging` / release 確定発火を
    /// `>= DRAG_THRESHOLD_PX` で gate して micro-jitter の depth Edit を防ぐ (knob と同 idiom)。
    /// base scrub には影響しない (後方互換)。
    drag_distance: f32,
}

#[derive(Debug, Clone, Copy)]
struct DragAnchor {
    /// 押下/再 anchor 時のマウス y。
    pointer_y: f32,
    /// 押下/再 anchor 時の基準値。 base gesture は press 時の frac (0..=1)、 depth-edit gesture は press
    /// 時の depth (= `ModEdit::current_depth`、 frac ドメインの符号付き量)。
    value: f64,
    /// 押下/再 anchor 時の Ctrl 状態。mid-drag toggle で再 anchor するための判定用。
    ctrl: bool,
    /// この gesture が depth-edit (= `Modulation::edit` Some) で始まったか。 true なら drag は base(音量)
    /// でなく depth を変化させ base 移動を抑止する (= 非破壊)。 gesture 途中で固定 (arm 変化に不追従)。
    depth_drag: bool,
}

#[derive(Debug, Clone, Copy)]
struct ClickRecord {
    when: Instant,
    pos: (f32, f32),
}

/// fader の幾何計算: track (細い縦バー) と thumb (つまみ) の rect を返す。
/// track 領域を **明示指定** して track / thumb rect を計算する。
/// `[col_x, col_x+col_w]` が横の列、 `[track_top, track_top+track_h]` が thumb 中心の可動域。
/// thumb 中心は `track_top + track_h * (1.0 - value)` (= `value=1` で上端、 `value=0` で下端)。
/// `channel_fader_meter` は meter と共有する region を track として渡す (daw_01 #083)。
fn fader_track_geometry(
    col_x: f32,
    col_w: f32,
    track_top: f32,
    track_h: f32,
    value: f32,
) -> (Rect, Rect) {
    let track_w = 6.0;
    let track_x = col_x + (col_w - track_w) * 0.5;
    let track = Rect { x: track_x, y: track_top, w: track_w, h: track_h };
    let thumb_x = col_x + (col_w - THUMB_W) * 0.5;
    // value=1 → thumb_y は track 上端、value=0 → 下端付近に。
    let thumb_y_unclamped = track_top + (track_h - THUMB_H * 0.5) - track_h * value;
    let thumb_y =
        thumb_y_unclamped.clamp(track_top - THUMB_H * 0.5, track_top + track_h - THUMB_H * 0.5);
    let thumb = Rect { x: thumb_x, y: thumb_y, w: THUMB_W, h: THUMB_H };
    (track, thumb)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FaderResponse {
    /// 描画されている値 (ドラッグ中なら drag value、そうでなければ入力値と同じ)。
    pub displayed_value: f32,
    pub hovered: bool,
    /// base value (音量) の drag 中 (depth-edit gesture とは排他)。
    pub dragging: bool,
    /// modulation depth の drag 編集中 (= `Modulation::edit` Some + press 中、 `>= 4px` で立つ)。 base
    /// `dragging` とは排他。 edge 検出で caller が undo bracket を発火する (daw_01 #110、 knob と同義)。
    pub mod_dragging: bool,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 矩形指定で垂直 fader を描画 + ドラッグ + ヒットテスト。
    ///
    /// 値が変わったとき `on_change(new_value)` を呼んで `Edit<M>` を発行する。
    /// **M8 Phase 29**: drag 中の各フレームでは Mutate Edit を発行 (history に乗らない)、
    /// drag 終端でのみ Undoable Edit (start_value → end_value、label 付き) を 1 度だけ発行する。
    /// これにより undo/redo は drag 単位の意味のあるステップで巻き戻る (DAW 標準動作)。
    ///
    /// `on_change` は `Fn + Clone + Send + Sync + 'static` を要求 (= `move |v| Edit::mutate(...)` の
    /// 形で書く、capture は Copy 型のみが原則)。
    ///
    /// `scale = None` のとき `value` / `default_value` / `on_change` 引数はすべて `0.0..=1.0` fraction。
    /// `scale = Some(s)` のとき `value` / `default_value` / `on_change` 引数はすべて dB 値。
    ///   - `f32::NEG_INFINITY`（または curve 下端以下）はフェーダ最下端（無音）。
    ///   - widget が `s.curve` で dB→fraction 変換してハンドル位置を決定し、
    ///     fraction→dB 逆変換して `on_change(db)` を呼ぶ。
    ///   - `level_meter_stereo` に渡す `MeterScale` と同一インスタンスを使うとカーソルが必ず一致する。
    ///
    /// `label` は undoable history パネルでの表示文字列 ("fader" / "volume" 等)。
    ///
    /// 操作:
    /// - thumb をドラッグで値編集 (track 1 本分 = 0→1 fraction)
    /// - thumb をダブルクリック (~300ms / 5px 以内) で `default_value` に戻る
    /// - Ctrl + ドラッグで感度 1/10
    ///
    /// `modulation`: `Some` で Bitwig 流 modulation を表示・編集する (daw_01 #110、 #109 knob の fader 版)。
    ///   `None` で従来描画・従来挙動 (完全回帰)。 depth / live_value / current_depth は **フェーダーの
    ///   正規化トラック位置 0..=1** で渡す (絶対 live は 0..=1、 符号付き depth は base からの増減、 dB/log
    ///   写像でなく位置の frac、 polarity は caller が解決)。 詳細は [`Ui::channel_fader_meter`] の doc 参照。
    #[allow(clippy::too_many_arguments)]
    pub fn fader_at<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        value: f32,
        default_value: f32,
        scale: Option<MeterScale>,
        label: &'static str,
        on_change: F,
        modulation: Option<Modulation<'_, M>>,
    ) -> FaderResponse
    where
        F: Fn(f32) -> Edit<M> + Clone + Send + Sync + 'static,
    {
        // track 領域は rect から TRACK_PAD インセットで導出 (従来挙動と byte 互換)。
        let wid = WidgetId::ROOT.child((b"fader", &id));
        let track_top = rect.y + TRACK_PAD;
        let track_h = (rect.h - TRACK_PAD * 2.0).max(1.0);
        self.fader_core(
            wid, rect, track_top, track_h, value, default_value, scale, label, on_change, modulation,
        )
    }

    /// `fader_at` / `channel_fader_meter` 共有の fader コア (描画 + ドラッグ + Edit 発行)。
    ///
    /// track 領域を **明示指定** する: `col` が背景パネル + thumb の横列 rect、
    /// `[track_top, track_top+track_h]` が thumb 中心の可動域 (= dB→y region)。 thumb 中心は
    /// `track_top + track_h * (1.0 - frac)`。 `channel_fader_meter` は meter と共有する region を
    /// 渡して画素整合させる (daw_01 #083)。 `scale` の dB↔fraction 変換と undoable Edit 機構は
    /// `fader_at` と同一。
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub(crate) fn fader_core<F>(
        &mut self,
        wid: WidgetId,
        col: Rect,
        track_top: f32,
        track_h: f32,
        value: f32,
        default_value: f32,
        scale: Option<MeterScale>,
        label: &'static str,
        on_change: F,
        modulation: Option<Modulation<'_, M>>,
    ) -> FaderResponse
    where
        F: Fn(f32) -> Edit<M> + Clone + Send + Sync + 'static,
    {
        // 値空間 (caller の dB or fraction) → 内部 fraction (0..=1) への変換
        let to_frac = |v: f32| -> f32 {
            match scale {
                None => v,
                Some(s) => if v.is_finite() { s.db_to_frac(v) } else { 0.0 },
            }
        };
        // 内部 fraction → 値空間 (dB or fraction) への変換。fraction=0 は無音 (NEG_INFINITY)。
        let to_val = move |frac: f32| -> f32 {
            match scale {
                None => frac,
                Some(s) => if frac <= 0.0 { f32::NEG_INFINITY } else { s.frac_to_db(frac) },
            }
        };

        // ---- modulation 記述の展開 (None = 完全回帰、 borrow のみ、 knob #109 と同形) ----
        // fader の depth domain は **frac (0..=1 トラック位置)** = base と同じ単位 (daw_01 #110)。
        let mod_ref = modulation.as_ref();
        let mod_entries: &[ModEntry] = mod_ref.map_or(&[], |m| m.entries);
        let mod_live = mod_ref.and_then(|m| m.live_value);
        let mod_edit = mod_ref.and_then(|m| m.edit.as_ref());
        let depth_mode = mod_edit.is_some();
        let current_depth = mod_edit.map_or(0.0, |e| e.current_depth);
        let depth_range = mod_edit.and_then(|e| e.depth_range);

        let pointer = self.pointer;
        let value = to_frac(value).clamp(0.0, 1.0);
        let default_value = to_frac(default_value).clamp(0.0, 1.0);
        let (_, thumb_rect) = fader_track_geometry(col.x, col.w, track_top, track_h, value);

        // base drag 感度 = track_h 分で 0→1 (= 1/track_h units_per_pixel)。 depth も ModEdit 指定が
        // 無ければ同じ感度を流用 (frac と depth が同じ 0..=1 スパンなので自然)。
        let base_units_per_px = 1.0 / track_h.max(1.0);
        let depth_units_per_px = mod_edit
            .and_then(|e| e.depth_sensitivity)
            .unwrap_or(base_units_per_px);

        // 1. 押下処理 + 2. mid-drag ctrl toggle 再 anchor + 3. release 解除 (depth/base で分岐)
        let mut reset_fired = false;
        // depth gesture が press を掴んだフレーム (= meter reset 抑止のため consume する)。
        let mut grabbed_depth_press = false;
        // depth gesture の release frame で確定する最終 depth (pointer 最終位置から再計算)。
        let mut release_depth: Option<f64> = None;
        let (drag_anchor, release_initial_value, drag_distance) = {
            let state: &mut FaderState = self.widget_state(wid);

            // 押下: ダブルクリック判定 → リセット 又は drag 開始 (thumb 内のみ)
            if pointer.primary_just_pressed
                && let Some((px, py)) = pointer.pos
                && thumb_rect.contains(px, py)
            {
                let now = Instant::now();
                let is_double = state.last_click.is_some_and(|c| {
                    now.duration_since(c.when).as_millis() < DOUBLE_CLICK_MS
                        && (c.pos.0 - px).hypot(c.pos.1 - py) < DOUBLE_CLICK_PX
                });

                if is_double && !depth_mode {
                    // リセット。drag は始めない。3 連クリック誤動作防止のため last_click も消す。
                    // depth-edit 中は base(音量) を触らない (非破壊) ので reset を抑止し depth press 扱い。
                    state.last_click = None;
                    state.drag_anchor = None;
                    state.drag_initial_value = None;
                    state.drag_distance = 0.0;
                    reset_fired = true;
                } else {
                    state.last_click = Some(ClickRecord { when: now, pos: (px, py) });
                    // depth-edit 中は anchor の基準値を base frac でなく現 depth にする。
                    let anchor_value = if depth_mode { current_depth } else { f64::from(value) };
                    state.drag_anchor = Some(DragAnchor {
                        pointer_y: py,
                        value: anchor_value,
                        ctrl: pointer.modifiers.ctrl,
                        depth_drag: depth_mode,
                    });
                    // M8 Phase 29: drag 開始時の base frac を保存 (release frame で inverse に使う)
                    state.drag_initial_value = Some(value);
                    state.drag_distance = 0.0;
                    // depth gesture はこの press を消費し meter peak-reset の二重処理を防ぐ
                    // (base は channel_fader_meter 側の consume が担当、 #110)。
                    grabbed_depth_press = depth_mode;
                }
            }

            // mid-drag で Ctrl 状態が変わったら anchor を張り直す:
            // 再 anchor しないと cumulative-from-anchor の delta 全体に新スケールが掛かって
            // 値が jump する。`(現在 py, 現在 基準値, 現在 ctrl)` に張り直すことで
            // return-to-press-position の cumulative 性質を保ったまま境界で滑らかに切替わる。
            // depth gesture は depth 基準、 base gesture は base frac 基準で再 anchor。
            if let Some(anchor) = state.drag_anchor
                && let Some((_, py)) = pointer.pos
                && pointer.modifiers.ctrl != anchor.ctrl
            {
                let anchor_value = if anchor.depth_drag { current_depth } else { f64::from(value) };
                state.drag_anchor = Some(DragAnchor {
                    pointer_y: py,
                    value: anchor_value,
                    ctrl: pointer.modifiers.ctrl,
                    depth_drag: anchor.depth_drag,
                });
            }

            // #110: drag 距離 (縦) の最大値を計測 (depth gesture の閾値判定用)。
            if let (Some(anchor), Some((_, py))) = (state.drag_anchor, pointer.pos) {
                let d = (py - anchor.pointer_y).abs();
                if d > state.drag_distance {
                    state.drag_distance = d;
                }
            }

            // M8 Phase 29 + #110: release frame を depth / base で分岐。
            let mut release_initial_value: Option<f32> = None;
            if pointer.primary_just_released {
                let anchor_opt = state.drag_anchor;
                let init = state.drag_initial_value.take();
                let dist = state.drag_distance;
                state.drag_anchor = None;
                state.drag_distance = 0.0;
                if anchor_opt.is_some_and(|a| a.depth_drag) {
                    // depth gesture: release frame は anchor None で per-frame が fire しないため、
                    // pointer 最終位置から depth を再計算して 1 度確定発火 (knob #109 と同義)。 micro-jitter
                    // click は閾値未満で抑止 (base 移動は閾値なしで後方互換)。
                    if dist >= DRAG_THRESHOLD_PX
                        && let (Some(anchor), Some((_, py))) = (anchor_opt, pointer.pos)
                    {
                        let d =
                            fader_drag_delta(anchor.pointer_y, py, depth_units_per_px, anchor.ctrl);
                        release_depth = Some(clamp_opt(anchor.value + d, depth_range));
                    }
                } else {
                    // base scrub のみ release で undoable wrap するため初期 frac を残す。
                    release_initial_value = init;
                }
            }

            (state.drag_anchor, release_initial_value, state.drag_distance)
        };

        // base 表示値: リセット > base drag (depth gesture 中は抑止 = 非破壊) > 入力値。
        let displayed_value = if reset_fired {
            default_value
        } else if let (Some(anchor), Some((_, py))) = (drag_anchor, pointer.pos)
            && !anchor.depth_drag
        {
            let d = fader_drag_delta(anchor.pointer_y, py, base_units_per_px, anchor.ctrl);
            ((anchor.value + d) as f32).clamp(0.0, 1.0)
        } else {
            value
        };

        // depth 表示値 (= modulation 帯 + on_mod_change、 frac ドメイン): depth gesture drag 中のみ更新。
        let displayed_depth: f64 = if let (Some(anchor), Some((_, py))) = (drag_anchor, pointer.pos)
            && anchor.depth_drag
        {
            let d = fader_drag_delta(anchor.pointer_y, py, depth_units_per_px, anchor.ctrl);
            clamp_opt(anchor.value + d, depth_range)
        } else {
            current_depth
        };

        // depth gesture が press を掴んだフレームは meter reset 抑止のため click を消費する。
        if grabbed_depth_press {
            self.consume_pointer_click();
        }

        // 描画。track + thumb。M4 Phase 11: with_widget_node で input_hash キャッシュ。
        // base scrub と depth-edit は排他 (anchor.depth_drag で判定)。 depth gesture 中は thumb を
        // press 色にしない (= 非破壊の視覚化、 強調は overlay の source 色枠が担う)。
        let dragging = drag_anchor.is_some_and(|a| !a.depth_drag);
        // depth gesture は閾値超で初めて mod_dragging (= daw の undo bracket edge、 knob と同義)。
        let mod_dragging =
            drag_anchor.is_some_and(|a| a.depth_drag) && drag_distance >= DRAG_THRESHOLD_PX;
        let (_, thumb_hover_rect) =
            fader_track_geometry(col.x, col.w, track_top, track_h, displayed_value);
        let hovered_thumb = pointer.pos.is_some_and(|(px, py)| thumb_hover_rect.contains(px, py));
        let input_hash = hash_inputs((
            b"fader",
            col.x.to_bits(),
            col.y.to_bits(),
            col.w.to_bits(),
            col.h.to_bits(),
            displayed_value.to_bits(),
            dragging,
            hovered_thumb,
        ));
        self.with_widget_node(wid, input_hash, |ui| {
            draw_fader(ui, col, track_top, track_h, displayed_value, dragging, pointer);
        });

        // ---- modulation overlay (= cache node の外、 毎フレーム描画) ----
        // live_value は ~30Hz、 base/depth は drag 追従なので cache に載せず overlay 化。 track/thumb の
        // cache node は modulation 非依存のまま据え置き (None で完全回帰)。 cache HIT/MISS いずれでも毎
        // フレーム描くので HIT 取りこぼし無し ([[feedback_cache_hit_path_and_multiframe_verify]])。
        if mod_ref.is_some() {
            draw_fader_modulation_overlay(
                self,
                col,
                track_top,
                track_h,
                displayed_value,
                displayed_depth,
                mod_entries,
                mod_live,
                mod_edit.map(|e| e.source_color),
            );
        }

        // depth (modulation) 発火: per-frame (hold 中) + release-frame 確定 (knob #109 と同契約)。
        // daw は mod_dragging の edge で undo bracket するため widget 側は Undoable wrap しない。
        if let Some(edit) = mod_edit
            && drag_anchor.is_some_and(|a| a.depth_drag)
            && (displayed_depth - current_depth).abs() > f64::EPSILON
        {
            self.push_edit((edit.on_mod_change)(displayed_depth));
        }
        if let Some(edit) = mod_edit
            && let Some(final_depth) = release_depth
            && (final_depth - current_depth).abs() > f64::EPSILON
        {
            self.push_edit((edit.on_mod_change)(final_depth));
        }

        // M8 Phase 29: base drag 終端では Mutate を抑制 (Undoable Edit が forward を再実行するため
        // model 値が二重更新されないようにする)。release_initial_value が Some なら drag 終端。
        // depth gesture 中は displayed_value == value で自然に base 発火が抑止される。
        let suppress_mutate_on_release = release_initial_value.is_some();
        if !suppress_mutate_on_release && (displayed_value - value).abs() > f32::EPSILON {
            let edit = on_change(to_val(displayed_value));
            self.push_edit(edit);
        }

        // M8 Phase 29: base drag 終端で Undoable Edit を発行 (start_value → end_value)。
        // 値変化が無ければ no-op (= 押下しただけで動かさず release した場合は history を汚さない)。
        if let Some(start_frac) = release_initial_value
            && (start_frac - displayed_value).abs() > f32::EPSILON
        {
            let on_change_fwd = on_change.clone();
            let on_change_inv = on_change;
            let end_val = to_val(displayed_value);
            let start_val = to_val(start_frac);
            let edit = Edit::with_inverse(
                label,
                move |m: &mut M| on_change_fwd(end_val).apply(m),
                move |m: &mut M| on_change_inv(start_val).apply(m),
            );
            self.push_edit(edit);
        }

        FaderResponse {
            displayed_value: to_val(displayed_value),
            hovered: hovered(col, pointer),
            dragging,
            mod_dragging,
        }
    }

    /// vstack カーソル位置に固定高さで垂直 fader を追加 (高さ 120 px)。
    /// レイアウト調整が必要なら `fader_at` を直接使う。
    pub fn fader<F>(
        &mut self,
        id: impl Hash,
        value: f32,
        default_value: f32,
        scale: Option<MeterScale>,
        label: &'static str,
        on_change: F,
    ) -> FaderResponse
    where
        F: Fn(f32) -> Edit<M> + Clone + Send + Sync + 'static,
    {
        let pad = 8.0;
        let h = 120.0;
        let rect = Rect {
            x: self.cursor.x + pad,
            y: self.cursor.y + self.next_y,
            w: 32.0,
            h,
        };
        let resp = self.fader_at(id, rect, value, default_value, scale, label, on_change, None);
        self.next_y += h + pad;
        resp
    }
}

fn draw_fader<M: ?Sized + 'static>(
    ui: &mut Ui<'_, M>,
    col: Rect,
    track_top: f32,
    track_h: f32,
    value: f32,
    dragging: bool,
    pointer: crate::input::PointerFrame,
) {
    // 背景パネル (列全体)
    ui.push_rect(RectCommand {
        rect: col,
        fill: Color::rgb(0.10, 0.11, 0.13),
        border: Color::rgb(0.25, 0.28, 0.33),
        border_width: 1.0,
        radius: [4.0; 4],
        clip_rect: None,
    });

    let (track, thumb) = fader_track_geometry(col.x, col.w, track_top, track_h, value);

    // 細い track
    ui.push_rect(RectCommand {
        rect: track,
        fill: Color::rgb(0.18, 0.20, 0.24),
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [3.0; 4],
        clip_rect: None,
    });

    // 値部分 (track の下端から上に伸びる) を強調色で塗る
    let filled_h = track.h * value;
    if filled_h > 0.0 {
        ui.push_rect(RectCommand {
            rect: Rect {
                x: track.x,
                y: track.y + (track.h - filled_h),
                w: track.w,
                h: filled_h,
            },
            fill: Color::rgb(0.32, 0.55, 0.85),
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [3.0; 4],
            clip_rect: None,
        });
    }

    // thumb: flat な水平バー (border / shadow なし、Ableton 系ミニマル)
    let base = Color::rgb(0.78, 0.82, 0.90);
    let press = Color::rgb(0.95, 0.97, 1.00);
    let thumb_fill = if dragging || hovered(thumb, pointer) {
        press
    } else {
        base
    };
    ui.push_rect(RectCommand {
        rect: thumb,
        fill: thumb_fill,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [1.0; 4],
        clip_rect: None,
    });
}

/// anchor からの縦 drag 量を frac ドメインの delta に変換する (base / depth 共用、 knob と同 idiom)。
/// 上 (py < anchor_y) で増加。 `units_per_px` は base なら `1/track_h`、 depth なら
/// `ModEdit::depth_sensitivity` (None で base と同値)。 Ctrl で `FINE_DRAG_SCALE` 倍精細。
fn fader_drag_delta(anchor_y: f32, py: f32, units_per_px: f32, ctrl: bool) -> f64 {
    let scale = if ctrl { FINE_DRAG_SCALE } else { 1.0 };
    f64::from(-(py - anchor_y) * units_per_px * scale)
}

/// `Some(range)` かつ `min <= max` のとき clamp、 それ以外 (None / 反転 / 非有限 bound) は素通し。
/// `f64::clamp` は min>max / NaN bound で panic するため防御 (knob #109 と同形)。
/// DRY note: scrubable / knob / fader が同種 helper を持つが各 module 独立。 将来 1 箇所統合の余地あり
/// (現状は各 widget の depth 実装が局所完結する方が読みやすいので分散のまま KISS)。
fn clamp_opt(v: f64, range: Option<(f64, f64)>) -> f64 {
    match range {
        Some((min, max)) if min <= max => v.clamp(min, max),
        _ => v,
    }
}

/// modulation の色帯 (縦トラック **上** の base→base+depth セグメント) + live 水平マーク + depth-edit
/// 枠/帯強調を描く (daw_01 #110、 knob の `draw_knob_modulation_overlay` の fader 版)。
///
/// cache node の **後** に毎フレーム呼ぶ overlay。 値は frac (0..=1 トラック位置)、 y 写像は
/// `y(f) = track_top + track_h*(1-f)` (frac 0 = 下端 / 1 = 上端)。 帯は **track 上**に重畳し (要望
/// 「トラック上に色帯」)、 複数 source は track 幅を縦に分割する。 非有限値は下端に丸めて renderer に
/// NaN 座標を渡さない。 全 rect を `col` に clip して meter 列へはみ出さない (dB 目盛り / メーターと共存)。
#[allow(clippy::too_many_arguments)]
fn draw_fader_modulation_overlay<M: ?Sized + 'static>(
    ui: &mut Ui<'_, M>,
    col: Rect,
    track_top: f32,
    track_h: f32,
    base_frac: f32,
    edit_depth: f64,
    entries: &[ModEntry],
    live_value: Option<f64>,
    edit_color: Option<Color>,
) {
    let track_w = 6.0;
    let track_x = col.x + (col.w - track_w) * 0.5;
    let y_of = |f: f32| -> f32 {
        if !f.is_finite() {
            return track_top + track_h; // frac 0 = 下端
        }
        track_top + track_h * (1.0 - f.clamp(0.0, 1.0))
    };
    let base_y = y_of(base_frac);
    let clip = Some(col);

    // depth-edit 中の枠強調 (entries / live が無くても出す、 source 色で track を囲う)。
    if let Some(c) = edit_color {
        ui.push_rect(RectCommand {
            rect: Rect { x: track_x - 1.5, y: track_top - 1.5, w: track_w + 3.0, h: track_h + 3.0 },
            fill: Color::TRANSPARENT,
            border: c,
            border_width: 1.5,
            radius: [2.0; 4],
            clip_rect: clip,
        });
    }

    // 各 source の色帯 (base→base+depth) を track 上に重畳。 複数は track 幅を縦に等分 (= 帯を分割)。
    // a=0.92 で track fill / thumb の上に乗せても source 色が判別できる。
    let n = entries.len().max(1);
    let band_col_w = (track_w / n as f32).max(1.0);
    for (i, e) in entries.iter().enumerate() {
        let end_y = y_of(base_frac + e.depth as f32);
        let (y0, y1) = (base_y.min(end_y), base_y.max(end_y));
        let bx = track_x + i as f32 * band_col_w;
        ui.push_rect(RectCommand {
            rect: Rect { x: bx, y: y0, w: band_col_w.max(1.0), h: (y1 - y0).max(1.0) },
            fill: Color { a: 0.92, ..e.color },
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: clip,
        });
    }

    // depth-edit 中: 編集中 depth を source 色で track 全幅に重ね描き (drag の live feedback)。
    if let Some(c) = edit_color {
        let end_y = y_of(base_frac + edit_depth as f32);
        let (y0, y1) = (base_y.min(end_y), base_y.max(end_y));
        ui.push_rect(RectCommand {
            rect: Rect { x: track_x, y: y0, w: track_w, h: (y1 - y0).max(1.0) },
            fill: Color { a: 0.88, ..c },
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: clip,
        });
    }

    // base 位置のマーカー (細い横線、 track を少しはみ出す)。 帯 / live / edit いずれも無い空
    // modulation では描かない (= `Some` でも全内容空なら描画差分なしの contract)。
    if !entries.is_empty() || live_value.is_some() || edit_color.is_some() {
        ui.push_rect(RectCommand {
            rect: Rect { x: track_x - 2.0, y: base_y - 0.5, w: track_w + 4.0, h: 1.0 },
            fill: Color::rgba(0.70, 0.70, 0.75, 0.9),
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: clip,
        });
    }

    // live 変調値の可動水平マーク (最前面、 明るい amber 横線、 track を横断して少し外へ)。
    if let Some(lv) = live_value {
        let ly = y_of(lv as f32);
        ui.push_rect(RectCommand {
            rect: Rect { x: track_x - 3.0, y: ly - 1.0, w: track_w + 6.0, h: 2.0 },
            fill: Color::rgba(1.0, 0.85, 0.30, 0.95),
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: clip,
        });
    }
}

#[cfg(test)]
mod tests {
    //! fader の双方向挙動テスト:
    //! - ダブルクリック: 同位置 / 時間しきい値内なら default_value にリセット
    //! - drag: 通常感度と Ctrl 押下時の 1/10 感度
    //! - mid-drag Ctrl toggle: 値 jump せず再 anchor される
    //!
    //! 時刻は `Instant::now()` を widget 内で直接呼ぶ実装なので、テストは `thread::sleep`
    //! でしきい値の前後を作り出す。total sleep budget は ~400ms 程度。

    use std::thread;
    use std::time::Duration;

    use daw_ui_platform::{Modifiers, PhysicalSize};
    use daw_ui_renderer::{Rect, Scene};

    use super::*;
    use crate::FrameInput;
    use crate::input::PointerFrame;
    use crate::ui::UiHost;

    /// 単純な値 1 個の Model。
    struct VolModel {
        value: f32,
    }

    /// テスト用 fader の rect。
    fn fader_rect() -> Rect {
        Rect { x: 0.0, y: 0.0, w: 32.0, h: 120.0 }
    }

    /// 与えた value での thumb 中心座標 (`fader_at` と同じ TRACK_PAD インセット track 領域)。
    fn thumb_center_at(value: f32) -> (f32, f32) {
        let rect = fader_rect();
        let track_top = rect.y + TRACK_PAD;
        let track_h = (rect.h - TRACK_PAD * 2.0).max(1.0);
        let (_, thumb) = fader_track_geometry(rect.x, rect.w, track_top, track_h, value.clamp(0.0, 1.0));
        (thumb.x + thumb.w * 0.5, thumb.y + thumb.h * 0.5)
    }

    fn run_frame(
        host: &mut UiHost<VolModel>,
        model: &VolModel,
        rect: Rect,
        value: f32,
        default_value: f32,
        pointer: PointerFrame,
    ) -> Vec<Edit<VolModel>> {
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 200 };
        host.frame_to_edits(
            model,
            &mut scene,
            screen,
            FrameInput { pointer, ..Default::default() },
            |_, ui| {
                ui.fader_at("test", rect, value, default_value, None, "fader", |v| {
                    Edit::mutate(move |m: &mut VolModel| m.value = v)
                }, None);
            },
        )
    }

    fn press_at(pos: (f32, f32), ctrl: bool) -> PointerFrame {
        PointerFrame {
            pos: Some(pos),
            primary_just_pressed: true,
            primary_pressed: true,
            modifiers: Modifiers { ctrl, ..Modifiers::default() },
            ..PointerFrame::default()
        }
    }

    fn hold_at(pos: (f32, f32), ctrl: bool) -> PointerFrame {
        PointerFrame {
            pos: Some(pos),
            primary_pressed: true,
            modifiers: Modifiers { ctrl, ..Modifiers::default() },
            ..PointerFrame::default()
        }
    }

    fn release_at(pos: (f32, f32)) -> PointerFrame {
        PointerFrame {
            pos: Some(pos),
            primary_just_released: true,
            ..PointerFrame::default()
        }
    }

    /// ダブルクリック (同位置 / ~50ms 内) で default_value に戻る。
    #[test]
    fn double_click_within_threshold_resets_to_default() {
        let mut host: UiHost<VolModel> = UiHost::no_redraw();
        let mut model = VolModel { value: 0.7 };
        let rect = fader_rect();
        let thumb = thumb_center_at(model.value);

        // Frame 1: 1 回目 press
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            press_at(thumb, false));
        for e in edits { e.apply(&mut model); }

        // Release
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            release_at(thumb));
        for e in edits { e.apply(&mut model); }

        thread::sleep(Duration::from_millis(50));

        // Frame 2: 2 回目 press (同位置、threshold 内) → reset で 0.25
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            press_at(thumb, false));
        for e in edits { e.apply(&mut model); }

        assert!(
            (model.value - 0.25).abs() < 1e-5,
            "ダブルクリックで default_value=0.25 にリセットされるべき (got {})",
            model.value
        );
    }

    /// しきい値超過 (~350ms 後) の 2 回目 press は drag 扱いで、リセットは起きない。
    #[test]
    fn click_after_threshold_does_not_reset() {
        let mut host: UiHost<VolModel> = UiHost::no_redraw();
        let mut model = VolModel { value: 0.7 };
        let rect = fader_rect();
        let thumb = thumb_center_at(model.value);

        // Frame 1
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            press_at(thumb, false));
        for e in edits { e.apply(&mut model); }
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            release_at(thumb));
        for e in edits { e.apply(&mut model); }

        thread::sleep(Duration::from_millis(350));

        // Frame 2
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            press_at(thumb, false));
        for e in edits { e.apply(&mut model); }

        assert!(
            (model.value - 0.7).abs() < 1e-5,
            "閾値超過の 2 回目 press はリセットを起こさない (got {})",
            model.value
        );
    }

    /// 同時間内でも 10px 以上離れていれば drag 扱いで、リセットは起きない。
    #[test]
    fn click_far_position_does_not_reset() {
        let mut host: UiHost<VolModel> = UiHost::no_redraw();
        let mut model = VolModel { value: 0.7 };
        let rect = fader_rect();
        let thumb = thumb_center_at(model.value);

        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            press_at(thumb, false));
        for e in edits { e.apply(&mut model); }
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            release_at(thumb));
        for e in edits { e.apply(&mut model); }

        thread::sleep(Duration::from_millis(50));

        // 2 回目 press は (thumb.x, thumb.y - 10) — 10px 上の thumb 内座標。
        // 注: value=0.7 なので thumb は中央より上、thumb 内で 10px 離れた位置を取れる。
        let far = (thumb.0, thumb.1 - 10.0);
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            press_at(far, false));
        for e in edits { e.apply(&mut model); }

        assert!(
            (model.value - 0.7).abs() < 1e-5,
            "10px 離れた 2 回目 press はリセットを起こさない (got {})",
            model.value
        );
    }

    /// Ctrl + drag は通常 drag の 1/10 の感度。
    #[test]
    fn ctrl_drag_uses_one_tenth_sensitivity() {
        // 通常 drag の値変化を測定
        let mut host_n: UiHost<VolModel> = UiHost::no_redraw();
        let mut model_n = VolModel { value: 0.5 };
        let rect = fader_rect();
        let thumb = thumb_center_at(model_n.value);

        let edits = run_frame(&mut host_n, &model_n, rect, model_n.value, 0.0,
            press_at(thumb, false));
        for e in edits { e.apply(&mut model_n); }
        // 20px 上にドラッグ
        let edits = run_frame(&mut host_n, &model_n, rect, model_n.value, 0.0,
            hold_at((thumb.0, thumb.1 - 20.0), false));
        for e in edits { e.apply(&mut model_n); }
        let normal_delta = model_n.value - 0.5;

        // Ctrl + drag の値変化を測定
        let mut host_c: UiHost<VolModel> = UiHost::no_redraw();
        let mut model_c = VolModel { value: 0.5 };
        let edits = run_frame(&mut host_c, &model_c, rect, model_c.value, 0.0,
            press_at(thumb, true));
        for e in edits { e.apply(&mut model_c); }
        let edits = run_frame(&mut host_c, &model_c, rect, model_c.value, 0.0,
            hold_at((thumb.0, thumb.1 - 20.0), true));
        for e in edits { e.apply(&mut model_c); }
        let fine_delta = model_c.value - 0.5;

        assert!(normal_delta > 0.0, "通常 drag は値が上がる (got {normal_delta})");
        assert!(fine_delta > 0.0, "Ctrl+drag は値が上がる (got {fine_delta})");
        // fine ≈ normal * 0.1
        let ratio = fine_delta / normal_delta;
        assert!(
            (ratio - 0.1).abs() < 1e-3,
            "Ctrl+drag は 1/10 感度 (ratio={ratio}, normal={normal_delta}, fine={fine_delta})",
        );
    }

    /// Mid-drag で Ctrl を on にしても、値が jump せずなめらかに継続する。
    /// 通常 drag で 20px 動かした後 Ctrl を押し、さらに 20px 動かす:
    /// - 再 anchor が無いと 40px 全体に 0.1 が掛かって値が縮む (jump)
    /// - 再 anchor 有りなら 20px 通常 + 20px ×0.1 で「jump 無し + 終端値が中間」になる
    #[test]
    fn mid_drag_ctrl_toggle_does_not_jump() {
        let mut host: UiHost<VolModel> = UiHost::no_redraw();
        let mut model = VolModel { value: 0.5 };
        let rect = fader_rect();
        let thumb = thumb_center_at(model.value);

        // 1. 通常 press
        let edits = run_frame(&mut host, &model, rect, model.value, 0.0,
            press_at(thumb, false));
        for e in edits { e.apply(&mut model); }

        // 2. 20px 上に drag (通常)
        let edits = run_frame(&mut host, &model, rect, model.value, 0.0,
            hold_at((thumb.0, thumb.1 - 20.0), false));
        for e in edits { e.apply(&mut model); }
        let after_normal = model.value;

        // 3. その位置で Ctrl 押下 (まだ動かない) → 再 anchor 発火、値は同じ
        let edits = run_frame(&mut host, &model, rect, model.value, 0.0,
            hold_at((thumb.0, thumb.1 - 20.0), true));
        for e in edits { e.apply(&mut model); }
        assert!(
            (model.value - after_normal).abs() < 1e-5,
            "Ctrl 押下のみ (動きゼロ) で値が変わらない (before={}, after={})",
            after_normal, model.value,
        );

        // 4. さらに 20px 上に drag (Ctrl 中) → fine スケール 0.1 で増える
        let edits = run_frame(&mut host, &model, rect, model.value, 0.0,
            hold_at((thumb.0, thumb.1 - 40.0), true));
        for e in edits { e.apply(&mut model); }
        let after_fine = model.value;

        // 期待: after_fine - after_normal ≈ (20px 分の通常 delta) * 0.1
        // 一方 after_normal - 0.5 ≈ (20px 分の通常 delta)
        // よって after_fine ≈ after_normal + (after_normal - 0.5) * 0.1
        let expected = after_normal + (after_normal - 0.5) * 0.1;
        assert!(
            (after_fine - expected).abs() < 1e-4,
            "再 anchor + 1/10 感度: expected={expected}, got={after_fine}",
        );
    }

    /// 3 連クリックの 3 回目はリセットを再度トリガーせず、新しい drag を開始する。
    /// 検証: 3 回目クリック後の hold-move で値が動く (drag が active になっている) ことを確認。
    /// もしリセットの再トリガーや last_click の不適切な保持があれば、3 回目の press で
    /// drag_anchor が None のままになり、続く move で値変化が起きない (はず)。
    #[test]
    fn triple_click_does_not_reset_again() {
        let mut host: UiHost<VolModel> = UiHost::no_redraw();
        let mut model = VolModel { value: 0.7 };
        let rect = fader_rect();
        let thumb_07 = thumb_center_at(0.7);

        // 1 回目 press + release
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            press_at(thumb_07, false));
        for e in edits { e.apply(&mut model); }
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            release_at(thumb_07));
        for e in edits { e.apply(&mut model); }

        thread::sleep(Duration::from_millis(50));

        // 2 回目 press + release: リセット発火 → value = 0.25
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            press_at(thumb_07, false));
        for e in edits { e.apply(&mut model); }
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            release_at(thumb_07));
        for e in edits { e.apply(&mut model); }
        assert!((model.value - 0.25).abs() < 1e-5, "2 回目でリセット成立");

        thread::sleep(Duration::from_millis(50));

        // 3 回目 press: thumb は value=0.25 の位置に移動済みなのでそこを press。
        // last_click は 2 回目で None にされたので、is_double=false → drag 開始。
        let thumb_025 = thumb_center_at(0.25);
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            press_at(thumb_025, false));
        for e in edits { e.apply(&mut model); }

        // 3 回目 hold-move (20px 上): drag が active なら値増加。
        let edits = run_frame(&mut host, &model, rect, model.value, 0.25,
            hold_at((thumb_025.0, thumb_025.1 - 20.0), false));
        for e in edits { e.apply(&mut model); }

        assert!(
            model.value > 0.25 + 1e-3,
            "3 回目 click は drag を開始する (move で値が増えるはず): value={}",
            model.value
        );
    }

    // ---- daw_01 #110: Bitwig 流 modulation (fader 版、 base も depth も frac 0..=1 トラック位置) ----

    use crate::widgets::scrubable_number::{ModEdit, ModEntry, Modulation};

    /// base frac (f32) と depth (f64) を別々に持つ test model (scale=None で value = frac)。
    struct ModModel {
        value: f32,
        depth: f64,
    }

    /// modulation 付き 1 frame を描画 + 処理し、 edits と response を返す。 fader_rect = 32×120
    /// (track_h = 104、 base 感度 = 1/104)。 press は value 位置の thumb 中心に置く。
    #[allow(clippy::too_many_arguments)]
    fn run_mod_frame(
        host: &mut UiHost<ModModel>,
        model: &ModModel,
        rect: Rect,
        pointer: PointerFrame,
        edit_mode: bool,
        entries: &[ModEntry],
        live_value: Option<f64>,
        scene: &mut Scene,
    ) -> (Vec<Edit<ModModel>>, FaderResponse) {
        let screen = PhysicalSize { width: 200, height: 200 };
        let base = model.value;
        let cur_depth = model.depth;
        let resp_cell: std::cell::RefCell<FaderResponse> =
            std::cell::RefCell::new(FaderResponse::default());
        let edits = host.frame_to_edits(
            model,
            scene,
            screen,
            FrameInput { pointer, ..Default::default() },
            |_, ui| {
                let on_mod = |d: f64| Edit::mutate(move |m: &mut ModModel| m.depth = d);
                let edit_desc = edit_mode.then_some(ModEdit {
                    source_color: Color::rgb(0.2, 0.9, 0.4),
                    current_depth: cur_depth,
                    depth_range: Some((-1.0, 1.0)),
                    depth_sensitivity: None,
                    on_mod_change: &on_mod,
                });
                let modulation = Modulation { entries, live_value, edit: edit_desc };
                let r = ui.fader_at(
                    "mtest",
                    rect,
                    base,
                    0.5,
                    None,
                    "vol",
                    |v| Edit::mutate(move |m: &mut ModModel| m.value = v),
                    Some(modulation),
                );
                *resp_cell.borrow_mut() = r;
            },
        );
        (edits, resp_cell.into_inner())
    }

    /// arm 中 (edit_mode) の press + 縦 drag は **depth** を変化させ、 base(音量) は触らない (非破壊)。
    #[test]
    fn mod_edit_drag_changes_depth_not_base() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { value: 0.5, depth: 0.0 };
        let rect = fader_rect();
        let thumb = thumb_center_at(0.5);

        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, press_at(thumb, false), true, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }
        // drag up 52px → depth = 0 + 52/104 = 0.5。 base value は不変。
        let (edits, resp) = run_mod_frame(
            &mut host, &model, rect, hold_at((thumb.0, thumb.1 - 52.0), false), true, &[], None, &mut Scene::new(),
        );
        for e in edits { e.apply(&mut model); }

        assert!((model.depth - 0.5).abs() < 1e-4, "depth scrub +0.5 (got {})", model.depth);
        assert!((model.value - 0.5).abs() < 1e-5, "base value は depth-edit 中 不変 (got {})", model.value);
        assert!(resp.mod_dragging, "depth drag 中は mod_dragging=true");
        assert!(!resp.dragging, "depth drag 中は base dragging=false (排他)");
    }

    /// 非 arm の drag は従来どおり base(音量) を scrub し、 depth は触らない。
    #[test]
    fn non_arm_drag_scrubs_base_only() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { value: 0.5, depth: 0.25 };
        let rect = fader_rect();
        let thumb = thumb_center_at(0.5);

        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, press_at(thumb, false), false, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }
        let (edits, resp) = run_mod_frame(
            &mut host, &model, rect, hold_at((thumb.0, thumb.1 - 52.0), false), false, &[], None, &mut Scene::new(),
        );
        for e in edits { e.apply(&mut model); }

        assert!(model.value > 0.5 + 1e-3, "base scrub で音量 frac が上がる (got {})", model.value);
        assert!((model.depth - 0.25).abs() < 1e-9, "非 arm では depth 不変 (got {})", model.depth);
        assert!(resp.dragging, "非 arm は base dragging=true");
        assert!(!resp.mod_dragging, "非 arm は mod_dragging=false");
    }

    /// arm 中 dblclick は base(音量) の default reset を発火しない (非破壊)。
    #[test]
    fn mod_edit_dblclick_does_not_reset_base() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { value: 0.8, depth: 0.0 };
        let rect = fader_rect();
        let thumb = thumb_center_at(0.8);

        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, press_at(thumb, false), true, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }
        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, release_at(thumb), true, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }
        thread::sleep(Duration::from_millis(50));
        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, press_at(thumb, false), true, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }

        assert!((model.value - 0.8).abs() < 1e-5, "arm 中 dblclick で base は reset されない (got {})", model.value);
    }

    /// `entries` を渡すと色帯 rect が overlay として追加され、 entry 色で描かれる。 None で出ない。
    #[test]
    fn entries_draw_bands() {
        let model = ModModel { value: 0.5, depth: 0.0 };
        let rect = fader_rect();
        let screen = PhysicalSize { width: 200, height: 200 };

        let mut host_n: UiHost<ModModel> = UiHost::no_redraw();
        let mut scene_none = Scene::new();
        host_n.frame_to_edits(&model, &mut scene_none, screen, FrameInput::default(), |_, ui| {
            ui.fader_at("mtest", rect, 0.5, 0.5, None, "vol",
                |v| Edit::mutate(move |m: &mut ModModel| m.value = v), None);
        });

        let cyan = Color::rgb(0.2, 0.8, 1.0);
        let entries = [ModEntry { color: cyan, depth: 0.30 }];
        let mut host_s: UiHost<ModModel> = UiHost::no_redraw();
        let mut scene_some = Scene::new();
        run_mod_frame(&mut host_s, &model, rect, PointerFrame::default(), false, &entries, None, &mut scene_some);

        assert!(
            scene_some.rect_count() > scene_none.rect_count(),
            "entries で band rect が増える (none={}, some={})",
            scene_none.rect_count(), scene_some.rect_count(),
        );
        assert!(
            scene_some.iter_rects().any(|r| {
                (r.fill.r - cyan.r).abs() < 1e-3
                    && (r.fill.g - cyan.g).abs() < 1e-3
                    && (r.fill.b - cyan.b).abs() < 1e-3
            }),
            "entry 色の帯 rect が描かれる",
        );
    }

    /// `live_value` を渡すと可動水平マーク rect が追加される。
    #[test]
    fn live_value_draws_mark() {
        let model = ModModel { value: 0.5, depth: 0.0 };
        let rect = fader_rect();

        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut scene_no = Scene::new();
        run_mod_frame(&mut host, &model, rect, PointerFrame::default(), false, &[], None, &mut scene_no);

        let mut host2: UiHost<ModModel> = UiHost::no_redraw();
        let mut scene_live = Scene::new();
        run_mod_frame(&mut host2, &model, rect, PointerFrame::default(), false, &[], Some(0.7), &mut scene_live);

        assert!(
            scene_live.rect_count() > scene_no.rect_count(),
            "live_value で mark rect が増える (no={}, live={})",
            scene_no.rect_count(), scene_live.rect_count(),
        );
    }

    /// depth gesture の release frame で pointer が動いた最終位置の depth が確定発火する。
    #[test]
    fn mod_edit_release_commits_final_depth() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { value: 0.5, depth: 0.0 };
        let rect = fader_rect();
        let thumb = thumb_center_at(0.5);

        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, press_at(thumb, false), true, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }
        // hold up 26px → depth 26/104 = 0.25
        let (edits, _) = run_mod_frame(
            &mut host, &model, rect, hold_at((thumb.0, thumb.1 - 26.0), false), true, &[], None, &mut Scene::new(),
        );
        for e in edits { e.apply(&mut model); }
        assert!((model.depth - 0.25).abs() < 1e-4, "hold で depth 0.25 (got {})", model.depth);

        // release は更に上 (-52px) で離す → 最終 depth 0.5 が release frame で確定。
        let (edits, _) = run_mod_frame(
            &mut host, &model, rect, release_at((thumb.0, thumb.1 - 52.0)), true, &[], None, &mut Scene::new(),
        );
        for e in edits { e.apply(&mut model); }
        assert!((model.depth - 0.5).abs() < 1e-4, "release frame で最終 depth 0.5 確定 (got {})", model.depth);
    }

    /// `depth_sensitivity: Some` は depth drag で base 感度 (1/track_h) を上書きする。
    #[test]
    fn depth_sensitivity_overrides_base() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { value: 0.5, depth: 0.0 };
        let rect = fader_rect();
        let thumb = thumb_center_at(0.5);
        let screen = PhysicalSize { width: 200, height: 200 };

        let run = |host: &mut UiHost<ModModel>, model: &ModModel, pointer: PointerFrame| -> Vec<Edit<ModModel>> {
            let cur = model.depth;
            host.frame_to_edits(model, &mut Scene::new(), screen, FrameInput { pointer, ..Default::default() }, |_, ui| {
                let on_mod = |d: f64| Edit::mutate(move |m: &mut ModModel| m.depth = d);
                let m = Modulation {
                    entries: &[],
                    live_value: None,
                    edit: Some(ModEdit {
                        source_color: Color::WHITE,
                        current_depth: cur,
                        depth_range: Some((-2.0, 2.0)),
                        depth_sensitivity: Some(0.05), // 0.05 units/px (base 1/104≈0.0096 を上書き)
                        on_mod_change: &on_mod,
                    }),
                };
                ui.fader_at("mtest", rect, model.value, 0.5, None, "vol",
                    |v| Edit::mutate(move |m: &mut ModModel| m.value = v), Some(m));
            })
        };

        for e in run(&mut host, &model, press_at(thumb, false)) { e.apply(&mut model); }
        // drag up 20px × depth_sensitivity 0.05 = 1.0 (base 感度 1/104 なら 0.19)。
        for e in run(&mut host, &model, hold_at((thumb.0, thumb.1 - 20.0), false)) { e.apply(&mut model); }
        assert!((model.depth - 1.0).abs() < 1e-4, "depth_sensitivity 0.05 で +1.0 (got {})", model.depth);
    }

    /// `Some` でも entries 空 + live None + edit None なら overlay 描画差分なし (None と同 rect 数)。
    #[test]
    fn empty_modulation_draws_no_overlay() {
        let model = ModModel { value: 0.5, depth: 0.0 };
        let rect = fader_rect();
        let screen = PhysicalSize { width: 200, height: 200 };

        let mut host_n: UiHost<ModModel> = UiHost::no_redraw();
        let mut scene_none = Scene::new();
        host_n.frame_to_edits(&model, &mut scene_none, screen, FrameInput::default(), |_, ui| {
            ui.fader_at("mtest", rect, 0.5, 0.5, None, "vol",
                |v| Edit::mutate(move |m: &mut ModModel| m.value = v), None);
        });

        let mut host_e: UiHost<ModModel> = UiHost::no_redraw();
        let mut scene_empty = Scene::new();
        run_mod_frame(&mut host_e, &model, rect, PointerFrame::default(), false, &[], None, &mut scene_empty);

        assert_eq!(
            scene_empty.rect_count(), scene_none.rect_count(),
            "empty Some は None と同じ rect 数 (overlay 描画差分なし)",
        );
    }

    /// 非有限 (NaN/Inf) な depth / live_value を渡しても scene の rect 座標に NaN/Inf を出さない。
    #[test]
    fn nonfinite_values_produce_no_nan() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let model = ModModel { value: 0.5, depth: 0.0 };
        let rect = fader_rect();
        let entries = [ModEntry { color: Color::WHITE, depth: f64::NAN }];
        let mut scene = Scene::new();
        run_mod_frame(
            &mut host, &model, rect, PointerFrame::default(), false, &entries, Some(f64::INFINITY), &mut scene,
        );
        for r in scene.iter_rects() {
            assert!(
                r.rect.x.is_finite() && r.rect.y.is_finite() && r.rect.w.is_finite() && r.rect.h.is_finite(),
                "rect 座標に NaN/Inf が出ない (got {:?})", r.rect,
            );
        }
    }

    /// 閾値未満 (< DRAG_THRESHOLD_PX) の press→release は depth Edit を発火せず mod_dragging も立てない。
    #[test]
    fn mod_edit_subthreshold_click_fires_no_depth() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { value: 0.5, depth: 0.0 };
        let rect = fader_rect();
        let thumb = thumb_center_at(0.5);

        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, press_at(thumb, false), true, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }
        // 2px だけ上に動いて release (閾値 4px 未満)。
        let (edits, resp) = run_mod_frame(
            &mut host, &model, rect, release_at((thumb.0, thumb.1 - 2.0)), true, &[], None, &mut Scene::new(),
        );
        let n = edits.len();
        for e in edits { e.apply(&mut model); }
        assert_eq!(n, 0, "閾値未満 click は depth Edit を発火しない (got {n} edits)");
        assert!((model.depth - 0.0).abs() < 1e-9, "depth は変わらない (got {})", model.depth);
        assert!(!resp.mod_dragging, "閾値未満では mod_dragging は立たない");
    }

    /// 反転した depth_range (min > max) を渡しても `clamp_opt` が panic しない (防御的素通し)。
    #[test]
    fn inverted_depth_range_does_not_panic() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { value: 0.5, depth: 0.0 };
        let rect = fader_rect();
        let thumb = thumb_center_at(0.5);
        let screen = PhysicalSize { width: 200, height: 200 };

        let run = |host: &mut UiHost<ModModel>, model: &ModModel, pointer: PointerFrame| -> Vec<Edit<ModModel>> {
            let cur = model.depth;
            host.frame_to_edits(model, &mut Scene::new(), screen, FrameInput { pointer, ..Default::default() }, |_, ui| {
                let on_mod = |d: f64| Edit::mutate(move |m: &mut ModModel| m.depth = d);
                let m = Modulation {
                    entries: &[],
                    live_value: None,
                    edit: Some(ModEdit {
                        source_color: Color::WHITE,
                        current_depth: cur,
                        depth_range: Some((1.0, -1.0)), // 反転 (caller bug) — f64::clamp なら panic
                        depth_sensitivity: None,
                        on_mod_change: &on_mod,
                    }),
                };
                ui.fader_at("mtest", rect, model.value, 0.5, None, "vol",
                    |v| Edit::mutate(move |m: &mut ModModel| m.value = v), Some(m));
            })
        };

        for e in run(&mut host, &model, press_at(thumb, false)) { e.apply(&mut model); }
        for e in run(&mut host, &model, hold_at((thumb.0, thumb.1 - 52.0), false)) { e.apply(&mut model); }
        assert!(model.depth.is_finite(), "panic せず depth は有限 (got {})", model.depth);
    }
}
