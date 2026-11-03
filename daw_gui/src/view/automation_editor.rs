//! Automation Editor — オートメーションクリップの編集面。
//!
//! Audio Editor と同じく下部パネルの Piano Roll タブを置き換える。セッションのセルは曲の
//! 時間軸に位置を持たずレーンに描けないので、これがセルの点に触る唯一の場所になる。
//! アレンジのクリップも同じ面で開ける (名前帯のダブルクリック)。
//!
//! 操作はレーンと同じ: ダブルクリック = 点の追加 / ドラッグ = 移動 / 点のダブルクリック =
//! 数値入力 / Alt+クリック = 削除 / Alt+線のドラッグ = 曲げる / Alt+線のダブルクリック =
//! 直線に戻す / 右クリック = カーブの形。点の編集はレーンと同じ AppEvent を出す。
//!
//! 座標は **クリップの窓ローカル拍** (窓の左端 = 0)。点の編集イベントが受ける座標と同じ。
//! 値の高さと曲線の形は `curve.rs` を通す (再生と同じ式で描く)。

use std::borrow::Cow;
use std::sync::Arc;

use common::model::{AutomationClipKey, AutomationCurve, AutomationTarget};
use common::snap::SnapConfig;
use common::time::{TimeDisplay, TimeMapping};
use daw_ui_core::{Edit, Ui, ViewportState1D, WidgetId};
use daw_ui_platform::Modifiers;
use daw_ui_renderer::{Color, LineBatch, LineSegment, Rect, RectCommand};

use crate::app::{AppData, AppEvent, AutomationPointKeyRef, MoveAutomationPointEntry};
use crate::view::arrangement_view::{CURVE_MENU_ITEMS, apply_curve_menu_choice};
use crate::view::automation_value_input;
use crate::view::clip_ruler::{self, RulerView};
use crate::view::{snap, track_color};
use crate::widgets::arrangement::curve::{self, LaneValueMap};
use crate::widgets::arrangement::ArrangementStyle;
use crate::widgets::select_modifier::{SelectModifier, range_ordered};
use crate::widgets::time_grid::{BarBeatGridStyle, GridLines, TimeGridExt};

const PAD: f32 = 12.0;
const HEADER_H: f32 = 24.0;
const RULER_H: f32 = 20.0;
const CLOSE_W: f32 = 60.0;
/// 値域の上下の余白。上端 / 下端の点の丸が切れず、掴める。
const PLOT_V_PAD: f32 = 10.0;
/// 押して離すまでの移動がこれ未満なら、ドラッグではなくクリック (レーンと同じ閾値)。
const CLICK_SLOP_PX: f32 = 4.0;
/// 点の右クリックメニューの id。点ごとに開かず 1 枚にする (当たりが重なる点で同時に開かない)。
pub const POINT_MENU_ID: &str = "automation_editor_point_menu";

/// 面の配置。テストが画面座標を出すのにも使う。
#[derive(Clone, Copy, Debug)]
pub struct EditorLayout {
    pub close: Rect,
    pub ruler: Rect,
    /// ドラッグ / ダブルクリックを受ける領域。
    pub body: Rect,
    /// 値域 (上端 = norm 1、下端 = norm 0)。`body` の内側。
    pub plot: Rect,
}

impl EditorLayout {
    #[must_use]
    pub fn new(area: Rect) -> Self {
        let w = (area.w - PAD * 2.0).max(0.0);
        let body_y = area.y + HEADER_H + RULER_H;
        let body = Rect { x: area.x + PAD, y: body_y, w, h: (area.h - HEADER_H - RULER_H - PAD).max(0.0) };
        Self {
            close: Rect {
                x: area.x + area.w - PAD - CLOSE_W,
                y: area.y + 2.0,
                w: CLOSE_W,
                h: (HEADER_H - 4.0).max(1.0),
            },
            ruler: Rect { x: area.x + PAD, y: area.y + HEADER_H, w, h: RULER_H },
            body,
            plot: Rect {
                x: body.x,
                y: body.y + PLOT_V_PAD,
                w: body.w,
                h: (body.h - PLOT_V_PAD * 2.0).max(2.0),
            },
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct EditorPoint {
    id: u32,
    /// 窓ローカル拍。
    beat: f64,
    norm: f32,
    plain: f64,
    curve: AutomationCurve,
    selected: bool,
}

/// 1 フレーム分の解決済みの入力。
struct Frame<'a> {
    key: AutomationClipKey,
    layout: EditorLayout,
    /// 窓の左端の song 拍 (セルは契約上 0)。スナップはレーンと同じく song 拍で掛ける。
    clip_start: f64,
    clip_len: f64,
    is_cell: bool,
    view_start: f64,
    view_len: f64,
    px_per_beat: f64,
    target: &'a AutomationTarget,
    map: LaneValueMap<'a>,
    default_norm: f32,
    points: Vec<EditorPoint>,
    snap: SnapConfig,
    style: ArrangementStyle,
    /// 曲線と点の色 (レーン面の上で読める明度に寄せた、クリップの実効色)。
    ink: Color,
}

impl Frame<'_> {
    #[allow(clippy::cast_possible_truncation)]
    fn beat_to_x(&self, beat: f64) -> f32 {
        self.layout.plot.x + ((beat - self.view_start) * self.px_per_beat) as f32
    }

    fn x_to_beat(&self, x: f32) -> f64 {
        self.view_start + f64::from(x - self.layout.plot.x) / self.px_per_beat
    }

    fn y_to_norm(&self, y: f32) -> f32 {
        (1.0 - (y - self.layout.plot.y) / self.layout.plot.h.max(1.0)).clamp(0.0, 1.0)
    }

    /// 窓ローカル拍を、窓の中へ収めて snap する。
    #[allow(clippy::cast_possible_truncation)]
    fn snapped_beat(&self, beat: f64, alt: bool) -> f64 {
        let abs = self.snap.snap_beat(self.clip_start + beat, alt, self.px_per_beat as f32);
        (abs - self.clip_start).clamp(0.0, self.clip_len.max(0.0))
    }

    fn point_key(&self, idx: usize) -> AutomationPointKeyRef {
        #[allow(clippy::cast_possible_truncation)]
        AutomationPointKeyRef {
            track_id: self.key.track,
            lane_id: self.key.lane,
            clip_id: self.key.clip,
            point_idx: idx as u32,
        }
    }

    fn point_pos(&self, p: &EditorPoint) -> (f32, f32) {
        (self.beat_to_x(p.beat), self.map.norm_to_y(p.norm))
    }

    /// 当たり円 (点の半径の 2 倍) の中で最も近い点。同じ距離なら選択中の点 (上に描くので)。
    fn point_at(&self, x: f32, y: f32) -> Option<usize> {
        let hit = self.style.automation_point_radius_px.max(2.0) * 2.0;
        self.points
            .iter()
            .enumerate()
            .filter_map(|(i, p)| {
                let (px, py) = self.point_pos(p);
                let d = (x - px).hypot(y - py);
                (d <= hit).then_some((i, d, p.selected))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1).then(b.2.cmp(&a.2)))
            .map(|(i, _, _)| i)
    }

    /// `x, y` の下の区間 (入射側の点の index と、掴んだ位置の区間内進捗)。曲げられない区間は
    /// 返さない (`curve::segment_hit_in_lane` と同じ除外: Mute レーン / 水平 / 幅 0 / 未採番)。
    fn segment_at(&self, x: f32, y: f32) -> Option<(usize, f64)> {
        if !self.map.is_bendable() {
            return None;
        }
        let hit_px = self.style.automation_curve_segment_hit_px.max(1.0);
        let mut best: Option<(f32, usize, f64)> = None;
        for (i, pair) in self.points.windows(2).enumerate() {
            let (a, b) = (&pair[0], &pair[1]);
            let (x0, x1) = (self.beat_to_x(a.beat), self.beat_to_x(b.beat));
            let bendable = b.id != 0
                && x1 - x0 > 1e-3
                && x > x0
                && x < x1
                && (self.map.plain_to_y(b.plain) - self.map.plain_to_y(a.plain)).abs() >= 1.0;
            if !bendable {
                continue;
            }
            let u = f64::from(x - x0) / f64::from(x1 - x0);
            let curve_y = self.map.norm_to_y(curve::eval_norm(self.map, a.plain, b.plain, u, b.curve));
            let d = (y - curve_y).abs();
            if d <= hit_px && best.is_none_or(|(bd, _, _)| d < bd) {
                best = Some((d, i + 1, u));
            }
        }
        best.map(|(_, i, u)| (i, u))
    }

    fn point_rect(&self, p: &EditorPoint, radius: f32) -> Rect {
        let (px, py) = self.point_pos(p);
        Rect { x: px - radius, y: py - radius, w: radius * 2.0, h: radius * 2.0 }
    }
}

fn build_frame<'a>(app: &'a AppData, key: AutomationClipKey, area: Rect) -> Option<Frame<'a>> {
    let song = app.cur.song_doc.song();
    let lane = song.automation_lane_by_key(key.track, key.lane)?;
    let clip = lane.clip_by_id(key.clip)?;
    let (view_start, view_len, clip_len) = app.automation_editor_view(key)?;
    let layout = EditorLayout::new(area);
    let range = app.plugin_param_range(&lane.target);
    let style = ArrangementStyle::from_theme(&app.theme);
    let map = LaneValueMap {
        target: &lane.target,
        plugin_range: range,
        clip_y: layout.plot.y,
        clip_h: layout.plot.h,
    };
    let mut points: Vec<EditorPoint> = song
        .clip_contents
        .get(&clip.content_id)
        .and_then(common::model::ClipContent::automation_points)
        .unwrap_or(&[])
        .iter()
        .map(|p| EditorPoint {
            id: p.id,
            beat: p.time_beat - clip.content_offset_beats,
            norm: common::automation::plain_to_norm_ranged(&lane.target, p.value, range),
            plain: p.value,
            curve: p.curve,
            selected: false,
        })
        .collect();
    for k in &app.cur.selection.selected_automation_points {
        if (k.track_id, k.lane_id, k.clip_id) == (key.track, key.lane, key.clip)
            && let Some(p) = points.get_mut(k.point_idx as usize)
        {
            p.selected = true;
        }
    }
    let base = track_color::to_renderer(track_color::effective_automation_clip_color(lane, clip));
    let ink = if lane.enabled {
        app.theme.core.adapt_on(style.automation_lane_bg, base)
    } else {
        style.automation_lane_disabled_color
    };
    Some(Frame {
        key,
        layout,
        clip_start: clip.start_beat,
        clip_len,
        is_cell: app.automation_editor_is_cell(key),
        view_start,
        view_len,
        px_per_beat: f64::from(layout.plot.w.max(1.0)) / view_len.max(1e-9),
        target: &lane.target,
        map,
        default_norm: common::automation::plain_to_norm_ranged(&lane.target, lane.default_value, range),
        points,
        snap: snap::arrange_snap_config(app),
        style,
        ink,
    })
}

// ============================================================
// ドラッグの session
// ============================================================

#[derive(Clone, Copy)]
struct PointDrag {
    idx: usize,
    id: u32,
    anchor: (f32, f32),
    last: (f32, f32),
    /// 離したフレームは Modifiers が先に変わることがあるので、直前の値を使う (ui/CLAUDE.md)。
    last_alt: bool,
    mods: Modifiers,
}

#[derive(Clone, Copy)]
struct BendDrag {
    idx: usize,
    id: u32,
    grab_u: f64,
    a_plain: f64,
    b_plain: f64,
    anchor_curve: AutomationCurve,
    start_curve: AutomationCurve,
    anchor_norm: f32,
    anchor_y: f32,
    preview: AutomationCurve,
}

#[derive(Clone, Copy)]
struct LassoDrag {
    anchor: (f32, f32),
    last: (f32, f32),
    mods: Modifiers,
}

#[derive(Clone, Copy)]
enum Session {
    Point(PointDrag),
    Bend(BendDrag),
    Lasso(LassoDrag),
}

#[derive(Default)]
struct EditorState {
    session: Option<(AutomationClipKey, Session)>,
    /// 右クリックメニューを開いた点の id (index は開いている間に変わりうる)。
    menu_point: Option<u32>,
}

fn state_wid() -> WidgetId {
    WidgetId::ROOT.child(b"automation_editor_state")
}

/// 点の移動先。プレビューと確定が同じ式を通る (ドラッグ中の見た目 = 離した結果)。
fn point_moves(fr: &Frame<'_>, d: &PointDrag) -> Vec<(usize, f64, f32)> {
    let Some(grabbed) = fr.points.get(d.idx) else {
        return Vec::new();
    };
    let dx = d.last.0 - d.anchor.0;
    let dy = d.last.1 - d.anchor.1;
    if dx.abs() + dy.abs() < CLICK_SLOP_PX {
        return Vec::new();
    }
    // 絶対位置で snap して差分を全点へ (ui/CLAUDE.md「drag 系 widget の snap」)。
    let raw = grabbed.beat + f64::from(dx) / fr.px_per_beat;
    let dt = fr.snapped_beat(raw, d.last_alt) - grabbed.beat;
    let dv = -dy / fr.layout.plot.h.max(1.0);
    let moved = |p: &EditorPoint| ((p.beat + dt).clamp(0.0, fr.clip_len.max(0.0)), (p.norm + dv).clamp(0.0, 1.0));
    if grabbed.selected {
        fr.points
            .iter()
            .enumerate()
            .filter(|(_, p)| p.selected)
            .map(|(i, p)| {
                let (t, v) = moved(p);
                (i, t, v)
            })
            .collect()
    } else {
        let (t, v) = moved(grabbed);
        vec![(d.idx, t, v)]
    }
}

fn rect_of(a: (f32, f32), b: (f32, f32)) -> Rect {
    Rect { x: a.0.min(b.0), y: a.1.min(b.1), w: (a.0 - b.0).abs(), h: (a.1 - b.1).abs() }
}

/// press の振り分け。優先順はレーンと同じ: 点 (Alt なら即削除) → Alt+線 = 曲げる → 投げ縄。
fn press(ui: &mut Ui<'_, AppData>, fr: &Frame<'_>, pos: (f32, f32), mods: Modifiers) -> Option<Session> {
    let (x, y) = pos;
    if let Some(idx) = fr.point_at(x, y) {
        if mods.alt {
            let points = vec![fr.point_key(idx)];
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::DeleteAutomationPoints { points });
            }));
            return None;
        }
        return Some(Session::Point(PointDrag {
            idx,
            id: fr.points[idx].id,
            anchor: pos,
            last: pos,
            last_alt: mods.alt,
            mods,
        }));
    }
    if mods.alt
        && !mods.shift
        && !mods.ctrl
        && let Some((idx, grab_u)) = fr.segment_at(x, y)
    {
        let (a, b) = (&fr.points[idx - 1], &fr.points[idx]);
        let start_curve = match b.curve {
            AutomationCurve::Hold | AutomationCurve::Linear => AutomationCurve::Exponential { bend: 0.0 },
            other => other,
        };
        return Some(Session::Bend(BendDrag {
            idx,
            id: b.id,
            grab_u,
            a_plain: a.plain,
            b_plain: b.plain,
            anchor_curve: b.curve,
            start_curve,
            anchor_norm: curve::eval_norm(fr.map, a.plain, b.plain, grab_u, start_curve),
            anchor_y: y,
            preview: b.curve,
        }));
    }
    Some(Session::Lasso(LassoDrag { anchor: pos, last: pos, mods }))
}

/// ドラッグの途中経過を session へ。`release` のフレームは Alt を更新せず、位置は
/// press 位置へ巻き戻っていないときだけ受ける (`widgets/arrangement/drag.rs` と同じ)。
fn advance(fr: &Frame<'_>, s: &mut Session, pos: (f32, f32), mods: Modifiers, release: bool) {
    match s {
        Session::Point(p) => {
            if !release || pos != p.anchor {
                p.last = pos;
            }
            if !release {
                p.last_alt = mods.alt;
            }
        }
        Session::Lasso(l) => {
            if !release || pos != l.anchor {
                l.last = pos;
            }
        }
        Session::Bend(b) => {
            let y = pos.1;
            // 動かしていないフレームでは preview を触らない。触ると Alt+クリックだけで
            // 直線が `Exponential { bend: 0 }` に書き換わる。
            if (y - b.anchor_y).abs() > f32::EPSILON {
                let target =
                    (b.anchor_norm - (y - b.anchor_y) / fr.layout.plot.h.max(1.0)).clamp(0.0, 1.0);
                if let Some(next) =
                    curve::solve_bend(fr.map, b.a_plain, b.b_plain, b.grab_u, b.start_curve, target)
                {
                    b.preview = next;
                }
            }
        }
    }
}

fn commit(ui: &mut Ui<'_, AppData>, fr: &Frame<'_>, s: Session) {
    match s {
        Session::Point(d) => commit_point(ui, fr, &d),
        Session::Bend(b) => {
            if b.preview != b.anchor_curve && b.id != 0 {
                let (key, id, next) = (fr.key, b.id, b.preview);
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetAutomationCurve {
                        track_id: key.track,
                        lane_id: key.lane,
                        clip_id: key.clip,
                        point_id: id,
                        next,
                    });
                }));
            }
        }
        Session::Lasso(l) => commit_lasso(ui, fr, &l),
    }
}

fn commit_point(ui: &mut Ui<'_, AppData>, fr: &Frame<'_>, d: &PointDrag) {
    let dist = (d.last.0 - d.anchor.0).abs() + (d.last.1 - d.anchor.1).abs();
    if dist >= CLICK_SLOP_PX {
        let deltas: Vec<MoveAutomationPointEntry> = point_moves(fr, d)
            .into_iter()
            .filter_map(|(i, next_time_beat, next_value_norm)| {
                let p = fr.points.get(i)?;
                let changed = (next_time_beat - p.beat).abs() > 1e-9 || (next_value_norm - p.norm).abs() > 1e-6;
                changed.then_some(MoveAutomationPointEntry {
                    key: fr.point_key(i),
                    prev_time_beat: p.beat,
                    prev_value_norm: p.norm,
                    next_time_beat,
                    next_value_norm,
                })
            })
            .collect();
        if !deltas.is_empty() {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::MoveAutomationPoints { deltas });
            }));
        }
        return;
    }
    if d.last_alt {
        return;
    }
    // 短いクリック = 選択。修飾の意味は他の選択面と同じ。
    let pressed = fr.point_key(d.idx);
    let modifier = SelectModifier::from_modifiers(d.mods.shift, d.mods.ctrl);
    let order: Vec<AutomationPointKeyRef> = (0..fr.points.len()).map(|i| fr.point_key(i)).collect();
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        let prev = app.cur.selection.selected_automation_points.clone();
        let anchor = app.cur.selection.automation_point_anchor;
        let next = modifier.resolve(&prev, pressed, || range_ordered(&order, anchor?, pressed));
        if modifier.updates_anchor() {
            app.cur.selection.automation_point_anchor = Some(pressed);
        }
        app.handle_event(AppEvent::SelectAutomationPoints { prev, next });
    }));
}

fn commit_lasso(ui: &mut Ui<'_, AppData>, fr: &Frame<'_>, l: &LassoDrag) {
    let dist = (l.last.0 - l.anchor.0).abs() + (l.last.1 - l.anchor.1).abs();
    let (shift, ctrl) = (l.mods.shift, l.mods.ctrl);
    let inside: Vec<AutomationPointKeyRef> = if dist < CLICK_SLOP_PX {
        // 空きの短いクリック: 修飾なしなら選択解除、修飾付きなら誤クリックとして何もしない。
        if shift || ctrl {
            return;
        }
        Vec::new()
    } else {
        let r = rect_of(l.anchor, l.last);
        fr.points
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                let (px, py) = fr.point_pos(p);
                r.contains(px, py)
            })
            .map(|(i, _)| fr.point_key(i))
            .collect()
    };
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        let prev = app.cur.selection.selected_automation_points.clone();
        let next: Vec<AutomationPointKeyRef> = if shift {
            let mut out = prev.clone();
            out.extend(inside.into_iter().filter(|k| !prev.contains(k)));
            out
        } else if ctrl {
            let mut out: Vec<_> = prev.iter().copied().filter(|k| !inside.contains(k)).collect();
            out.extend(inside.into_iter().filter(|k| !prev.contains(k)));
            out
        } else {
            inside
        };
        app.handle_event(AppEvent::SelectAutomationPoints { prev, next });
    }));
}

/// 掴んだものがまだ同じ所にあるか (開いているクリップが変わった / 点が消えたら捨てる)。
fn session_is_live(fr: &Frame<'_>, key: AutomationClipKey, s: &Session) -> bool {
    key == fr.key
        && match s {
            Session::Point(d) => fr.points.get(d.idx).is_some_and(|p| p.id == d.id),
            Session::Bend(b) => fr.points.get(b.idx).is_some_and(|p| p.id == b.id),
            Session::Lasso(_) => true,
        }
}

/// press / 途中経過 / release を 1 本に。戻り値はこのフレームの描画に使う session
/// (release のフレームも返す — Edit はフレーム末に適用されるので、ここで model を描くと 1 フレーム元へ戻る)。
///
/// press は **消費しない** (`take_drag_in_rect` を使わない)。後で描く数値入力欄の「外クリックで
/// 確定」と、メニューの「外クリックで閉じる」はどちらも同じ press を見る。代わりに、popup が
/// 開いているときと入力欄の上の press はここで受けない (レーンと同じガード)。
fn run_session(ui: &mut Ui<'_, AppData>, fr: &Frame<'_>, input: Option<Rect>) -> Option<Session> {
    let wid = state_wid();
    let pointer = ui.pointer();
    let mut session = ui
        .widget_state::<EditorState>(wid)
        .session
        .filter(|(k, s)| session_is_live(fr, *k, s))
        .map(|(_, s)| s);
    if ui.drag_cancel_requested() {
        session = None;
    }
    if pointer.primary_just_pressed {
        session = None;
        if let Some(pos) = pointer.pos.filter(|&(x, y)| {
            fr.layout.body.contains(x, y)
                && !ui.has_open_popups()
                && !input.is_some_and(|r| r.contains(x, y))
        }) {
            ui.claim_press(wid);
            session = press(ui, fr, pos, pointer.modifiers);
        }
    } else if let (Some(s), Some(pos)) = (session.as_mut(), pointer.pos) {
        advance(fr, s, pos, pointer.modifiers, pointer.primary_just_released);
    }
    let shown = session;
    // 離したフレームに確定する。release を取りこぼしたら (ボタンが上がっている) 捨てる。
    if pointer.primary_just_released {
        if let Some(s) = session.take() {
            commit(ui, fr, s);
        }
    } else if !pointer.primary_pressed && !pointer.primary_just_pressed {
        session = None;
    }
    ui.widget_state::<EditorState>(wid).session = session.map(|s| (fr.key, s));
    shown
}

fn double_click(ui: &mut Ui<'_, AppData>, fr: &Frame<'_>, input: Option<Rect>) {
    if ui.has_open_popups() {
        return;
    }
    let Some((x, y)) = ui.take_double_click_in_rect(fr.layout.body) else {
        return;
    };
    if input.is_some_and(|r| r.contains(x, y)) {
        return;
    };
    if let Some(idx) = fr.point_at(x, y) {
        let key = fr.point_key(idx);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::BeginEditAutomationPointValue { key });
        }));
        return;
    }
    let alt = ui.pointer().modifiers.alt;
    // 線の上の Alt+ダブルクリック = 直線に戻す。既に直線でも消費する (点を足す経路へ落とすと
    // 「戻すつもりが点が増えた」になる)。
    if alt && let Some((idx, _)) = fr.segment_at(x, y) {
        let p = fr.points[idx];
        if p.curve != AutomationCurve::Linear && p.id != 0 {
            let key = fr.key;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetAutomationCurve {
                    track_id: key.track,
                    lane_id: key.lane,
                    clip_id: key.clip,
                    point_id: p.id,
                    next: AutomationCurve::Linear,
                });
            }));
        }
        return;
    }
    let time_beat = fr.snapped_beat(fr.x_to_beat(x), alt);
    let value_norm = fr.y_to_norm(y);
    let key = fr.key;
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::AddAutomationPoint {
            track_id: key.track,
            lane_id: key.lane,
            clip_id: key.clip,
            time_beat,
            value_norm,
        });
    }));
}

/// 素 wheel = 横スクロール / Shift = 3 倍速 / Ctrl = ポインタ位置を保ったズーム
/// (Audio Editor と同じ)。
fn wheel(ui: &mut Ui<'_, AppData>, fr: &Frame<'_>) {
    if ui.has_open_popups() {
        return;
    }
    let body = fr.layout.body;
    let (sx, sy) = ui.take_scroll_in_rect(body);
    if sx.abs() <= 0.001 && sy.abs() <= 0.001 {
        return;
    }
    let pointer = ui.pointer();
    let m = pointer.modifiers;
    let key = fr.key;
    let total = fr.clip_len.max(crate::app_types::MIN_AUDIO_EDITOR_VIEW_LEN_BEATS);
    let (start, len) = (fr.view_start, fr.view_len);
    let (new_start, new_len) = if m.ctrl {
        let factor = f64::from((sy * 0.005).exp());
        let new_len = (len / factor).clamp(crate::app_types::MIN_AUDIO_EDITOR_VIEW_LEN_BEATS, total);
        let frac = pointer
            .pos
            .map_or(0.5, |(px, _)| f64::from(((px - body.x) / body.w.max(1.0)).clamp(0.0, 1.0)));
        let anchor = start + frac * len;
        ((anchor - frac * new_len).clamp(0.0, (total - new_len).max(0.0)), new_len)
    } else if m.alt {
        return;
    } else {
        let speed = if m.shift { 3.0 } else { 1.0 };
        let dx = -f64::from(sx + sy) / fr.px_per_beat * speed;
        ((start + dx).clamp(0.0, (total - len).max(0.0)), len)
    };
    if (new_start - start).abs() > 1e-9 || (new_len - len).abs() > 1e-9 {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.set_automation_editor_view(key, new_start, new_len);
        }));
    }
}

// ============================================================
// 描画
// ============================================================

fn draw_grid(app: &AppData, ui: &mut Ui<'_, AppData>, fr: &Frame<'_>) {
    let song = app.cur.song_doc.song();
    let mapping = TimeMapping {
        sample_rate: 48_000.0,
        tempo_bpm: f64::from(song.bpm.max(1.0)),
        time_sig: (song.time_sig.0.max(1), song.time_sig.1.max(1)),
        display: TimeDisplay::BarBeat,
    };
    let spb = mapping.samples_per_beat();
    let viewport = ViewportState1D::new((fr.clip_start + fr.view_start) * spb, fr.view_len.max(1e-6) * spb);
    #[allow(clippy::cast_possible_truncation)]
    let zoom = fr.px_per_beat as f32;
    let p = &app.theme.core;
    let lines = GridLines::from_snap(
        &fr.snap,
        zoom,
        snap::subgrid_interval_beats(fr.snap, zoom),
        p.grid_line.with_alpha(0.5),
        fr.style.beat_line_width_px,
    );
    let grid_style = BarBeatGridStyle {
        bar_color: fr.style.bar_line,
        beat_color: fr.style.beat_line,
        bar_line_width: fr.style.bar_line_width_px,
        beat_line_width: fr.style.beat_line_width_px,
        ..BarBeatGridStyle::from_palette(p)
    };
    ui.bar_beat_grid("automation_editor_grid", fr.layout.body, mapping, viewport, grid_style, lines);
}

/// 点列を折れ線にして押す (`curve::flatten_segment` 1 本で形を出す)。
fn push_curve(ui: &mut Ui<'_, AppData>, fr: &Frame<'_>, points: &[EditorPoint]) {
    let mut segments: Vec<LineSegment> = Vec::with_capacity(points.len() * 8);
    let mut run: Vec<(f32, f32)> = Vec::with_capacity(32);
    let body = fr.layout.body;
    for pair in points.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let (x0, x1) = (fr.beat_to_x(a.beat), fr.beat_to_x(b.beat));
        // ズーム中は見えない区間を折れ線にしない (曲線は 1 区間で最大 512 段)。
        if x1 < body.x || x0 > body.x + body.w {
            continue;
        }
        run.clear();
        run.push((x0, fr.map.plain_to_y(a.plain)));
        curve::flatten_segment(fr.map, (x0, a.plain), (x1, b.plain), b.curve, 2.0, &mut run);
        segments.extend(run.windows(2).map(|w| LineSegment {
            a: [w[0].0, w[0].1],
            b: [w[1].0, w[1].1],
            color: fr.ink,
        }));
    }
    if !segments.is_empty() {
        ui.push_lines(LineBatch {
            segments: Arc::from(segments),
            line_width_px: fr.style.automation_curve_line_width_px,
            clip_rect: Some(fr.layout.body),
        });
    }
}

/// 1 区間だけを強調色で重ねる (Alt で曲げられる区間 / 曲げている最中のプレビュー)。
fn push_segment_accent(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    fr: &Frame<'_>,
    a: &EditorPoint,
    b: &EditorPoint,
    shape: AutomationCurve,
    accent: Color,
) {
    let (x0, x1) = (fr.beat_to_x(a.beat), fr.beat_to_x(b.beat));
    let mut run = vec![(x0, fr.map.plain_to_y(a.plain))];
    curve::flatten_segment(fr.map, (x0, a.plain), (x1, b.plain), shape, 2.0, &mut run);
    let color = app.theme.core.adapt_on(fr.style.automation_lane_bg, accent);
    let segments: Vec<LineSegment> = run
        .windows(2)
        .map(|w| LineSegment { a: [w[0].0, w[0].1], b: [w[1].0, w[1].1], color })
        .collect();
    ui.push_lines(LineBatch {
        segments: Arc::from(segments),
        line_width_px: fr.style.automation_curve_line_width_px * 1.5,
        clip_rect: Some(fr.layout.body),
    });
}

/// model の点列にドラッグの途中経過を重ねたもの (描くのはこちら、確定は離したとき)。
fn displayed_points<'f>(fr: &'f Frame<'_>, session: Option<&Session>) -> Cow<'f, [EditorPoint]> {
    match session {
        Some(Session::Point(d)) => {
            let mut points = fr.points.clone();
            for (i, t, v) in point_moves(fr, d) {
                if let Some(p) = points.get_mut(i) {
                    p.beat = t;
                    p.norm = v;
                    p.plain = fr.map.to_plain(v);
                }
            }
            points.sort_by(|a, b| a.beat.total_cmp(&b.beat));
            Cow::Owned(points)
        }
        Some(Session::Bend(b)) => {
            let mut points = fr.points.clone();
            if let Some(p) = points.get_mut(b.idx) {
                p.curve = b.preview;
            }
            Cow::Owned(points)
        }
        _ => Cow::Borrowed(&fr.points),
    }
}

fn push_points(app: &AppData, ui: &mut Ui<'_, AppData>, fr: &Frame<'_>, points: &[EditorPoint]) {
    let p = &app.theme.core;
    let (normal_r, sel_r) = (
        fr.style.automation_point_radius_px,
        fr.style.automation_point_radius_selected_px,
    );
    let normal_edge = p.ink_for(fr.ink);
    let sel_fill = fr.style.automation_point_selected_fill;
    let sel_edge = p.ink_for(sel_fill);
    // 選択中の点は非選択の上に描く (当たり判定の同距離の優先と揃える)。
    for selected_pass in [false, true] {
        for pt in points.iter().filter(|pt| pt.selected == selected_pass) {
            let (r, fill, border) =
                if pt.selected { (sel_r, sel_fill, sel_edge) } else { (normal_r, fr.ink, normal_edge) };
            ui.push_rect(RectCommand {
                rect: fr.point_rect(pt, r),
                fill,
                border,
                border_width: 1.0,
                radius: [r; 4],
                clip_rect: Some(fr.layout.body),
            });
        }
    }
}

fn draw_body(app: &AppData, ui: &mut Ui<'_, AppData>, fr: &Frame<'_>, session: Option<&Session>) {
    let body = fr.layout.body;
    let p = &app.theme.core;
    ui.push_rect(RectCommand {
        rect: body,
        fill: fr.style.automation_lane_bg,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: Some(body),
    });
    draw_grid(app, ui, fr);
    // レーンの既定値 (クリップの外 / セルが鳴っていないときの値)。
    let default_y = fr.map.norm_to_y(fr.default_norm);
    ui.push_lines(LineBatch {
        segments: Arc::from(vec![LineSegment {
            a: [body.x, default_y],
            b: [body.x + body.w, default_y],
            color: fr.style.automation_default_line_color,
        }]),
        line_width_px: fr.style.automation_default_line_width_px,
        clip_rect: Some(body),
    });

    let points = displayed_points(fr, session);
    push_curve(ui, fr, &points);
    // 曲げている区間のプレビュー / Alt で曲げられる区間の強調。
    match session {
        Some(Session::Bend(b)) => {
            if let (Some(a), Some(n)) = (b.idx.checked_sub(1).and_then(|i| points.get(i)), points.get(b.idx)) {
                push_segment_accent(app, ui, fr, a, n, b.preview, fr.style.automation_curve_bend_preview_color);
            }
        }
        None if ui.pointer().modifiers.alt => {
            if let Some((x, y)) = ui.pointer().pos.filter(|&(x, y)| body.contains(x, y))
                && fr.point_at(x, y).is_none()
                && let Some((idx, _)) = fr.segment_at(x, y)
            {
                let (a, n) = (points[idx - 1], points[idx]);
                push_segment_accent(app, ui, fr, &a, &n, n.curve, fr.style.automation_curve_bend_hover_color);
            }
        }
        _ => {}
    }
    push_points(app, ui, fr, &points);

    if points.is_empty() {
        ui.label_at(
            "automation_editor_empty",
            "ダブルクリックで点を追加",
            body.x + 8.0,
            body.y + 8.0,
            12.0,
            p.text_dim,
        );
    }
    if let Some(Session::Lasso(l)) = session {
        ui.push_rect(RectCommand {
            rect: rect_of(l.anchor, l.last),
            fill: p.selection_warm.with_alpha(0.15),
            border: p.selection_warm.with_alpha(0.7),
            border_width: 1.0,
            radius: [0.0; 4],
            clip_rect: Some(body),
        });
    }
    if let Some(beat) = app.automation_editor_playhead_beat(fr.key) {
        let local = beat - fr.clip_start;
        if local >= fr.view_start && local <= fr.view_start + fr.view_len {
            let x = fr.beat_to_x(local);
            ui.push_lines(LineBatch {
                segments: Arc::from(vec![LineSegment {
                    a: [x, body.y],
                    b: [x, body.y + body.h],
                    color: app.theme.daw.playhead.with_alpha(0.9),
                }]),
                line_width_px: 1.5,
                clip_rect: Some(body),
            });
        }
    }
}

fn draw_header(app: &AppData, ui: &mut Ui<'_, AppData>, fr: &Frame<'_>, area: Rect) {
    let song = app.cur.song_doc.song();
    let name = song
        .automation_lane_by_key(fr.key.track, fr.key.lane)
        .and_then(|l| l.clip_by_id(fr.key.clip))
        .map_or_else(String::new, |c| song.content_name(c.content_id).to_string());
    let label = app.automation_target_label(fr.target);
    let subject = if name.is_empty() { label } else { format!("{label} / {name}") };
    ui.label_at_clipped(
        "automation_editor_title",
        &format!("Automation Editor — {subject} ({:.2} beats)", fr.clip_len),
        Rect {
            x: area.x + PAD,
            y: area.y + 6.0,
            w: (area.w - PAD * 2.0 - CLOSE_W - 8.0).max(1.0),
            h: 14.0 * 1.2,
        },
        14.0,
        app.theme.core.text,
    );
    ui.button_at("automation_editor_close", "Close", fr.layout.close, || {
        Edit::mutate(|app: &mut AppData| app.close_automation_editor())
    });
}

/// 点の右クリックメニュー (1 枚)。右クリックした位置に一番近い点に効く。
fn point_menu(ui: &mut Ui<'_, AppData>, fr: &Frame<'_>) {
    let wid = state_wid();
    let open_at = ui.take_secondary_click_in_rect(fr.layout.body).and_then(|(x, y)| {
        let idx = fr.point_at(x, y)?;
        Some(((x, y), fr.points[idx].id))
    });
    if let Some((_, id)) = open_at {
        ui.widget_state::<EditorState>(wid).menu_point = Some(id);
    }
    let point_id = ui.widget_state::<EditorState>(wid).menu_point;
    let key = point_id
        .and_then(|id| fr.points.iter().position(|p| p.id == id))
        .map(|i| crate::widgets::arrangement::point_key_from_model(fr.point_key(i)));
    ui.context_menu_at(POINT_MENU_ID, open_at.map(|(pos, _)| pos), &CURVE_MENU_ITEMS, move |idx, ui| {
        if let Some(key) = key {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                apply_curve_menu_choice(app, key, idx);
            }));
        }
    });
}

/// この面が出す数値入力の点の画面位置。ビューの外へ出たら `None` (入力を取り消す)。
fn editing_point_rect(fr: &Frame<'_>, edit_key: AutomationPointKeyRef) -> Option<Rect> {
    let p = fr.points.get(edit_key.point_idx as usize)?;
    let body = fr.layout.body;
    let x = fr.beat_to_x(p.beat);
    (x >= body.x && x <= body.x + body.w)
        .then(|| fr.point_rect(p, fr.style.automation_point_radius_px))
}

/// 編集中の点の数値入力 / ドラッグ中の現値。
fn draw_point_overlays(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    fr: &Frame<'_>,
    session: Option<&Session>,
    area: Rect,
) {
    if let Some(edit_key) = app.cur.peph.editing_automation_point
        && app.automation_editor_owns_point(edit_key)
    {
        automation_value_input::draw_value_input(app, ui, edit_key, editing_point_rect(fr, edit_key), area.y);
    }
    if let Some(Session::Point(d)) = session
        && let Some(&(_, _, v)) = point_moves(fr, d).iter().find(|(i, _, _)| *i == d.idx)
    {
        automation_value_input::draw_drag_readout(app, ui, fr.target, v, d.last, area.y);
    }
}

/// ポインタ下の拍を貼り付けの基準として publish する (本体の外では `None`)。
fn publish_hover(app: &AppData, ui: &mut Ui<'_, AppData>, fr: &Frame<'_>) {
    let body = fr.layout.body;
    let pointer = ui.pointer();
    let hover = pointer
        .pos
        .filter(|&(x, y)| body.contains(x, y))
        .map(|(x, _)| fr.snapped_beat(fr.x_to_beat(x), pointer.modifiers.alt));
    if app.cur.peph.automation_editor_hover_beat != hover {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.cur.peph.automation_editor_hover_beat = hover;
        }));
    }
}

/// `app.automation_editor_target()` が `Some` のとき下部パネルのタブ 1 から呼ばれる。
pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    ui.panel("automation_editor_bg", area, app.theme.core.panel, 0.0);
    let Some(key) = app.automation_editor_target() else {
        return;
    };
    let Some(fr) = build_frame(app, key, area) else {
        return;
    };
    draw_header(app, ui, &fr, area);
    clip_ruler::draw(
        app,
        ui,
        "automation_editor",
        fr.layout.ruler,
        RulerView { start_song: fr.clip_start + fr.view_start, len_beats: fr.view_len },
        !fr.is_cell,
    );
    let editing = app
        .cur.peph
        .editing_automation_point
        .filter(|k| app.automation_editor_owns_point(*k));
    if fr.layout.body.w <= 0.0 || fr.layout.body.h <= 0.0 {
        // 本体が描けないなら入力欄も出せない。開いたままにせず取り消す。
        if let Some(edit_key) = editing {
            automation_value_input::draw_value_input(app, ui, edit_key, None, area.y);
        }
        return;
    }
    let input = editing
        .and_then(|k| editing_point_rect(&fr, k))
        .map(|r| automation_value_input::input_rect(r, area.y));
    wheel(ui, &fr);
    let session = run_session(ui, &fr, input);
    double_click(ui, &fr, input);
    publish_hover(app, ui, &fr);
    draw_body(app, ui, &fr, session.as_ref());
    draw_point_overlays(app, ui, &fr, session.as_ref(), area);
    point_menu(ui, &fr);
}

/// 毎フレーム呼ぶ (下部パネルが閉じていても)。対象のクリップが消えていたら本当に閉じる。
/// 残しておくと、undo でクリップが戻った瞬間に、別のものを見ているタブがエディタへ変わる。
pub fn clear_stale_target(app: &AppData, ui: &mut Ui<'_, AppData>) {
    if app.cur.peph.automation_editor_clip.is_some() && app.automation_editor_target().is_none() {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.close_automation_editor();
        }));
    }
}
