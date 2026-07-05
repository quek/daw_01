//! Ruler 上の playhead seek / loop drag セッションと helper 群。
//!
//! arrangement と piano_roll の双方が **完全に同じ ruler 操作 UX** (#024 / #041) を
//! 持つため、 session struct / hit-test / snap 適用 endpoint 計算を 1 箇所に集約する。
//! widget-neutral な形 (`start_beat` / `len_beats` を引数で受ける) で expose する。
//!
//! - [`PlayheadDragSession`] : plain (Shift 非保持) ruler click / drag による seek session
//! - [`LoopDragSession`] / [`LoopDragKind`] : Shift + ruler drag による loop range edit session
//! - [`LoopBandHit`] / [`loop_band_hit_kind`] : 既存 loop range の handle hit-test
//! - [`compute_loop_drag_endpoints`] : drag 中 / release 時の snap 適用済 `(start, end)` 計算

use common::snap::SnapConfig;
use daw_ui_core::widgets::heavy::HeavyCtx;
use daw_ui_renderer::{Color, Rect, RectCommand};

/// loop band の hit 種別 (start handle / end handle / 中央 / 範囲外)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopBandHit {
    Start,
    End,
    Middle,
}

/// ruler 上に既存 loop range `(start_beat, end_beat)` が描画されている前提で、
/// `px` 位置が start handle / end handle / 中央帯 / 範囲外のどれにあたるかを返す。
///
/// `start_beat` / `len_beats` は ruler の表示 view (px → beat 変換用)。 `handle_radius_px`
/// は端 handle の hit 半径 (4px が arrangement/piano_roll 共通 default)。 ruler 帯自体の
/// vertical hit (y 軸方向) もここで判定する。
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn loop_band_hit_kind(
    range: (f64, f64),
    start_beat: f64,
    len_beats: f64,
    ruler: Rect,
    px: f32,
    handle_radius_px: f32,
) -> Option<LoopBandHit> {
    if !ruler.contains(px, ruler.y + ruler.h * 0.5) {
        return None;
    }
    let beat_to_px = f64::from(ruler.w) / len_beats.max(1e-6);
    let start_x = ruler.x + ((range.0 - start_beat) * beat_to_px) as f32;
    let end_x = ruler.x + ((range.1 - start_beat) * beat_to_px) as f32;
    let edge = handle_radius_px.max(1.0);
    if (px - start_x).abs() <= edge {
        Some(LoopBandHit::Start)
    } else if (px - end_x).abs() <= edge {
        Some(LoopBandHit::End)
    } else if px > start_x && px < end_x {
        Some(LoopBandHit::Middle)
    } else {
        None
    }
}

/// loop drag の sub-mode (どの端点を動かすか)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopDragKind {
    /// 既存 loop の start handle drag (end 固定)
    Start,
    /// 既存 loop の end handle drag (start 固定)
    End,
    /// 既存 loop 中央 drag (両端を同 delta で平行移動、 duration 維持)
    Middle,
    /// 新規 loop range の作成 (press 位置から drag 位置まで)
    NewRange,
}

/// M14 Phase 63j (#024) / Phase 69 (#041): Shift + ruler drag による loop range edit session。
///
/// arrangement / piano_roll で完全同形。 press frame で `Some(...)` 化、 continuation frame で
/// `last_mouse_x` / `last_alt` を update、 release frame で `take()` して `SetLoopRange` を発行。
/// drag overlay (preview) と release commit が **同じ `compute_loop_drag_endpoints` helper を共有**
/// することで、 「release で grid に飛ぶ」 不整合を構造的に回避する。
#[derive(Clone, Copy, Debug)]
pub struct LoopDragSession {
    pub kind: LoopDragKind,
    pub anchor_loop: (f64, f64),
    pub anchor_press_beat: f64,
    /// press 時 mouse x (release frame の巻き戻し検知用、 ClipDragSession.anchor_mouse と同 idiom)。
    pub anchor_mouse_x: f32,
    /// drag 中の最終 mouse x 位置 (release frame の `pointer.pos` に頼らない保険、
    /// `ClipDragSession.last_mouse` と同じ理由)。
    pub last_mouse_x: f32,
    /// drag 中の最終 alt 状態。 `ClipDragSession.last_alt` と同じ仕組みで track
    /// (continuation で update、 release で skip)。 release frame の
    /// `pointer.modifiers.alt` が ModifiersChanged 先行で false 化する race を回避。
    pub last_alt: bool,
}

/// M14 Phase 63j (#024) / Phase 69 (#041): ruler 上の plain (= Shift 非保持) click / drag による
/// playhead seek セッション。 press frame で `Some(...)` 化、 continuation frame で毎 frame
/// `SetPlayheadBeat` を発行 (`last_emitted_beat` で同値発火を抑制)。 release frame で `take()` して
/// discard (commit-by-release 無し、 既に逐次発行済)。
#[derive(Clone, Copy, Debug)]
pub struct PlayheadDragSession {
    /// drag 中の最終 mouse x 位置 (release frame の `pointer.pos` に頼らない保険、
    /// `ClipDragSession.last_mouse` と同理由)。 release では emit しないので現状未使用だが、
    /// 他 drag session と field 構成を揃えて将来の visual debug を容易にする。
    pub last_mouse_x: f32,
    /// drag 中に最後に発火した snap 適用済 beat 値 (毎 frame 同値発火を抑制)。
    /// press frame で初期化済み (= press 即発行値)、 continuation で differ 時のみ更新 + emit。
    pub last_emitted_beat: f64,
}

/// loop band overlay の描画 (背景帯 + 左右 handle bar)。 arrangement / piano_roll 両 widget が
/// ruler 上に同 idiom で loop range を表示するための共有 helper。
///
/// - `range`: 描画する `(start_beat, end_beat)` (順序は内部で正規化)
/// - `start_beat` / `len_beats`: ruler の表示 view (px 変換用)
/// - `ruler`: ruler rect (band の x range をここに clip、 handle bar の y/h もここに揃える)
/// - `band_color`: 帯部分の塗り色 (半透明推奨、 arrangement style default は cyan ~0.20 alpha)
/// - `handle_color`: 左右 handle bar の色 (不透明 cyan 系)
/// - `handle_w`: handle bar の幅 (px、 default 6.0)
#[allow(clippy::too_many_arguments)]
pub fn draw_loop_band<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    range: (f64, f64),
    start_beat: f64,
    len_beats: f64,
    ruler: Rect,
    band_color: Color,
    handle_color: Color,
    handle_w: f32,
) {
    let (lo, hi) = (range.0.min(range.1), range.0.max(range.1));
    let beat_to_px = f64::from(ruler.w) / len_beats.max(1e-6);
    #[allow(clippy::cast_possible_truncation)]
    let x0 = ruler.x + ((lo - start_beat) * beat_to_px) as f32;
    #[allow(clippy::cast_possible_truncation)]
    let x1 = ruler.x + ((hi - start_beat) * beat_to_px) as f32;
    let band_x = x0.max(ruler.x);
    let band_w = (x1.min(ruler.x + ruler.w) - band_x).max(0.0);
    if band_w > 0.0 {
        push_filled_rect(
            hctx,
            Rect { x: band_x, y: ruler.y, w: band_w, h: ruler.h },
            band_color,
        );
    }
    let hw = handle_w * 0.5;
    if x0 >= ruler.x - hw && x0 <= ruler.x + ruler.w + hw {
        push_filled_rect(
            hctx,
            Rect { x: x0 - hw, y: ruler.y, w: handle_w, h: ruler.h },
            handle_color,
        );
    }
    if x1 >= ruler.x - hw && x1 <= ruler.x + ruler.w + hw {
        push_filled_rect(
            hctx,
            Rect { x: x1 - hw, y: ruler.y, w: handle_w, h: ruler.h },
            handle_color,
        );
    }
}

fn push_filled_rect<M: ?Sized + 'static>(hctx: &mut HeavyCtx<'_, '_, M>, r: Rect, fill: Color) {
    hctx.push_rect(RectCommand {
        rect: r,
        fill,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: None,
    });
}

/// drag 中 / release 時の `(start, end)` 計算 (snap 適用済)。
///
/// clip drag と同じ pattern (overlay と commit が必ず同一値で確定):
/// - **Start drag**: 端点 (cur_beat) を grid に round → `min(snapped, anchor_loop.1)` で start 確定
/// - **End drag**: 端点 (cur_beat) を grid に round → `max(snapped, anchor_loop.0)` で end 確定
/// - **Middle drag**: 絶対位置 snap (Cubase / Live の Move pattern と同じ)。 `anchor_loop.0` を pivot
///   として grid に round → その差分 (delta) を両端に適用、 duration 維持。
/// - **NewRange**: `anchor_press_beat` は press 時に snap 済 (caller 側責務)、 ここは cur_beat だけ
///   snap。 両端を独立に snap してから順序正規化。 duration 0 でも問題なし (caller 側で扱う)。
///
/// `last_alt = true` で snap 一時無効 (raw 通過、 `MoveClips` と同じ alt 直交 policy)。
#[must_use]
pub fn compute_loop_drag_endpoints(
    ld: &LoopDragSession,
    cur_beat_raw: f64,
    snap: &SnapConfig,
    zoom_x_px_per_beat: f32,
) -> (f64, f64) {
    let alt = ld.last_alt;
    match ld.kind {
        LoopDragKind::Start => {
            let s = snap.snap_beat(cur_beat_raw, alt, zoom_x_px_per_beat);
            (s.min(ld.anchor_loop.1), ld.anchor_loop.1)
        }
        LoopDragKind::End => {
            let e = snap.snap_beat(cur_beat_raw, alt, zoom_x_px_per_beat);
            (ld.anchor_loop.0, e.max(ld.anchor_loop.0))
        }
        LoopDragKind::Middle => {
            let raw_delta = cur_beat_raw - ld.anchor_press_beat;
            let pivot = ld.anchor_loop.0;
            let snapped_pivot = snap.snap_beat(pivot + raw_delta, alt, zoom_x_px_per_beat);
            let delta = snapped_pivot - pivot;
            (ld.anchor_loop.0 + delta, ld.anchor_loop.1 + delta)
        }
        LoopDragKind::NewRange => {
            let other = snap.snap_beat(cur_beat_raw, alt, zoom_x_px_per_beat);
            (ld.anchor_press_beat.min(other), ld.anchor_press_beat.max(other))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::snap::SnapMode;

    fn snap_quarter_beat() -> SnapConfig {
        SnapConfig {
            mode: SnapMode::Straight { div: 4 },
            enabled: true,
            min_beat_unit: 1.0 / 128.0,
            time_sig: (4, 4),
        }
    }

    /// `loop_band_hit_kind`: start handle 上 (px 距離 < 4px) は `Start`。
    #[test]
    fn loop_band_hit_kind_start_handle() {
        let ruler = Rect { x: 0.0, y: 0.0, w: 400.0, h: 16.0 };
        // start_beat=0.0, len_beats=8.0, ruler.w=400 → 1 beat = 50 px、 range start=2.0 → x=100
        let hit = loop_band_hit_kind((2.0, 6.0), 0.0, 8.0, ruler, 100.0, 4.0);
        assert_eq!(hit, Some(LoopBandHit::Start));
    }

    /// `loop_band_hit_kind`: end handle 上 (px 距離 < 4px) は `End`。
    #[test]
    fn loop_band_hit_kind_end_handle() {
        let ruler = Rect { x: 0.0, y: 0.0, w: 400.0, h: 16.0 };
        // range end=6.0 → x=300
        let hit = loop_band_hit_kind((2.0, 6.0), 0.0, 8.0, ruler, 300.0, 4.0);
        assert_eq!(hit, Some(LoopBandHit::End));
    }

    /// `loop_band_hit_kind`: 中央帯 (start_x < px < end_x、 handle 外) は `Middle`。
    #[test]
    fn loop_band_hit_kind_middle() {
        let ruler = Rect { x: 0.0, y: 0.0, w: 400.0, h: 16.0 };
        let hit = loop_band_hit_kind((2.0, 6.0), 0.0, 8.0, ruler, 200.0, 4.0);
        assert_eq!(hit, Some(LoopBandHit::Middle));
    }

    /// `loop_band_hit_kind`: 範囲外 (end_x より右) は `None`。
    #[test]
    fn loop_band_hit_kind_outside() {
        let ruler = Rect { x: 0.0, y: 0.0, w: 400.0, h: 16.0 };
        let hit = loop_band_hit_kind((2.0, 6.0), 0.0, 8.0, ruler, 380.0, 4.0);
        assert_eq!(hit, None);
    }

    /// `compute_loop_drag_endpoints` の Start drag は moving 端点を grid に snap、 fixed 端点は不変。
    #[test]
    fn loop_endpoints_start_drag_snaps_moving_endpoint() {
        let ld = LoopDragSession {
            kind: LoopDragKind::Start,
            anchor_loop: (4.0, 12.0),
            anchor_press_beat: 4.0,
            anchor_mouse_x: 0.0,
            last_mouse_x: 0.0,
            last_alt: false,
        };
        let snap = snap_quarter_beat();
        let (s, e) = compute_loop_drag_endpoints(&ld, 1.7, &snap, 50.0);
        assert!((s - 2.0).abs() < 1e-6, "Start drag で raw 1.7 → snap 2.0: got {s}");
        assert!((e - 12.0).abs() < 1e-6, "End は不変 12.0: got {e}");
    }

    /// End drag は end 端点を grid に snap、 start 端点は不変。
    #[test]
    fn loop_endpoints_end_drag_snaps_moving_endpoint() {
        let ld = LoopDragSession {
            kind: LoopDragKind::End,
            anchor_loop: (4.0, 12.0),
            anchor_press_beat: 12.0,
            anchor_mouse_x: 0.0,
            last_mouse_x: 0.0,
            last_alt: false,
        };
        let snap = snap_quarter_beat();
        let (s, e) = compute_loop_drag_endpoints(&ld, 13.4, &snap, 50.0);
        assert!((s - 4.0).abs() < 1e-6, "Start は不変 4.0: got {s}");
        assert!((e - 13.0).abs() < 1e-6, "End drag で raw 13.4 → snap 13.0: got {e}");
    }

    /// Middle drag は両端点を同 delta で平行移動 (duration 維持)、 delta は anchor_loop.0 が
    /// grid に着地するよう計算 (Cubase Move 流の絶対位置 snap)。
    #[test]
    fn loop_endpoints_middle_drag_preserves_duration_with_snap() {
        let ld = LoopDragSession {
            kind: LoopDragKind::Middle,
            anchor_loop: (4.0, 12.0),
            anchor_press_beat: 6.0,
            anchor_mouse_x: 0.0,
            last_mouse_x: 0.0,
            last_alt: false,
        };
        let snap = snap_quarter_beat();
        let (s, e) = compute_loop_drag_endpoints(&ld, 7.7, &snap, 50.0);
        assert!((s - 6.0).abs() < 1e-6, "Middle drag で start は snap 6.0: got {s}");
        assert!((e - 14.0).abs() < 1e-6, "Middle drag で end も同 delta 移動 14.0: got {e}");
        assert!(
            ((e - s) - 8.0).abs() < 1e-6,
            "duration 8.0 維持: got {}",
            e - s
        );
    }

    /// NewRange は anchor_press_beat は press 時 snap 済前提、 helper は cur_beat だけ snap。
    /// 両端を順序 (min, max) で正規化。
    #[test]
    fn loop_endpoints_newrange_snaps_both_endpoints() {
        let ld = LoopDragSession {
            kind: LoopDragKind::NewRange,
            anchor_loop: (2.0, 2.0),
            anchor_press_beat: 2.0,
            anchor_mouse_x: 0.0,
            last_mouse_x: 0.0,
            last_alt: false,
        };
        let snap = snap_quarter_beat();
        let (s, e) = compute_loop_drag_endpoints(&ld, 9.4, &snap, 50.0);
        assert!((s - 2.0).abs() < 1e-6, "NewRange start = anchor_press 2.0: got {s}");
        assert!((e - 9.0).abs() < 1e-6, "NewRange end = snap(9.4) = 9.0: got {e}");
    }

    /// NewRange で cur_beat < anchor_press_beat の場合、 (min, max) 順序で正規化される。
    #[test]
    fn loop_endpoints_newrange_normalizes_reversed_drag() {
        let ld = LoopDragSession {
            kind: LoopDragKind::NewRange,
            anchor_loop: (8.0, 8.0),
            anchor_press_beat: 8.0,
            anchor_mouse_x: 0.0,
            last_mouse_x: 0.0,
            last_alt: false,
        };
        let snap = snap_quarter_beat();
        // cur_beat raw = 3.3 → snap → 3.0、 anchor_press = 8.0 → (min=3.0, max=8.0)
        let (s, e) = compute_loop_drag_endpoints(&ld, 3.3, &snap, 50.0);
        assert!((s - 3.0).abs() < 1e-6, "NewRange (逆 drag) で start = min(8, 3) = 3.0: got {s}");
        assert!((e - 8.0).abs() < 1e-6, "NewRange (逆 drag) で end = max(8, 3) = 8.0: got {e}");
    }

    /// Alt 押下で snap 一時無効、 raw 値が pass-through される。
    #[test]
    fn loop_endpoints_alt_disables_snap() {
        let ld = LoopDragSession {
            kind: LoopDragKind::Start,
            anchor_loop: (4.0, 12.0),
            anchor_press_beat: 4.0,
            anchor_mouse_x: 0.0,
            last_mouse_x: 0.0,
            last_alt: true,
        };
        let snap = snap_quarter_beat();
        let (s, _e) = compute_loop_drag_endpoints(&ld, 1.7, &snap, 50.0);
        assert!((s - 1.7).abs() < 1e-6, "Alt 押下で raw 1.7 が pass-through: got {s}");
    }

    /// snap OFF (caller が `SnapConfig::OFF` を渡した場合) でも raw 値が通る (既存 non-snap caller 互換)。
    #[test]
    fn loop_endpoints_snap_off_returns_raw() {
        let ld = LoopDragSession {
            kind: LoopDragKind::Start,
            anchor_loop: (4.0, 12.0),
            anchor_press_beat: 4.0,
            anchor_mouse_x: 0.0,
            last_mouse_x: 0.0,
            last_alt: false,
        };
        let (s, e) = compute_loop_drag_endpoints(&ld, 2.345, &SnapConfig::OFF, 50.0);
        assert!((s - 2.345).abs() < 1e-6, "snap OFF で raw 通過: got {s}");
        assert!((e - 12.0).abs() < 1e-6, "End は不変: got {e}");
    }
}
