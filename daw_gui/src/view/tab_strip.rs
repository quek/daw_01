//! プロジェクトタブ帯 (`docs/plan_project_tabs.md` §5.3)。
//!
//! menu bar と transport の間に、**タブが 2 つ以上あるときだけ** 描く (1 つなら高さ 0 =
//! 従来と同じ画面)。1 タブ = `[▶ ]名前[*]  ✕`。クリック = 切替、✕ = 閉じる、空き領域の
//! ダブルクリック = 新しいタブ、ドラッグ = 並べ替え、右クリック = メニュー。
//!
//! タブ帯は状態を持たない (タブの集合と並びは `AppData::tabs` が SSoT)。hover の
//! 経過時間 (tooltip / spring-loaded 切替) とドラッグ中の並べ替え位置だけを daw-ui の
//! retained `widget_state` に置く。
//!
//! §5.6 (タブをまたぐ D&D): クリップ / トラックを運んでいる最中にタブの上へ
//! [`SPRING_LOAD`] 留まるとそのタブへ切り替える。アレンジ内部のドラッグを payload に
//! 昇格させるのはアレンジ widget 側 (`widgets::arrangement`) で、こちらは
//! [`layout`] (純関数) を貸すだけ。

use std::time::{Duration, Instant};

use common::protocol::ProjectKey;
use daw_ui_core::widgets::drag_in_rect::DragKind;
use daw_ui_core::{Edit, Ui, WidgetId};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent};
use crate::app_types::PROJECT_XFER_DRAG_KIND;
use crate::event_tabs::TabEvent;

/// タブ帯の高さ (2 タブ以上のとき)。
pub const TAB_H: f32 = 26.0;
const PAD_X: f32 = 6.0;
/// 余裕があるときのタブ幅の下限。
const TAB_MIN_W: f32 = 80.0;
/// 潰しきらないための保険 (0 幅にして掴めなくしない)。**下限で止めない** —
/// `TAB_MIN_W` で止めると、タブが増えたぶんだけ帯が画面の右へ伸びて、後ろのタブが
/// 押し出されたまま二度と触れなくなる (閉じることもできない)。名前が潰れても、
/// 全部のタブが必ず画面の中に居るほうを取る (重ねて並べると、描画の上下と当たり判定の
/// 優先が逆転して「見えているタブとは別のタブが反応する」ので、重ねずに細くする)。
const TAB_FLOOR_W: f32 = 8.0;
const TAB_MAX_W: f32 = 200.0;
const TAB_GAP: f32 = 2.0;
const CLOSE_W: f32 = 16.0;
/// ✕ を出す最小のタブ幅 (これより狭いと ✕ だけでタブが埋まる)。
const CLOSE_MIN_TAB_W: f32 = 52.0;
const FONT: f32 = 12.0;
/// ドラッグ並べ替えとみなす移動量。
const DRAG_THRESHOLD: f32 = 4.0;
/// tooltip が出るまでの hover 時間。
const TOOLTIP_DELAY: Duration = Duration::from_millis(600);
/// D&D 中にタブの上へ留まると切り替わるまでの時間 (spring-loaded、Q10)。
pub const SPRING_LOAD: Duration = Duration::from_millis(500);

/// タブ帯の高さ (`app.tabs.len() < 2` なら 0)。root のレイアウトが呼ぶ。
#[must_use]
pub fn height(app: &AppData) -> f32 {
    if app.tabs.len() >= 2 { TAB_H } else { 0.0 }
}

/// 各タブの rect (表示順)。幅は均等で、余裕があるうちは 80..=200 px。入りきらなく
/// なったら [`TAB_FLOOR_W`] まで更に縮めて、**全部のタブを画面の中に収める**。
/// 純関数なのでアレンジ widget (spring-loaded 昇格) も同じ答えを得る。
#[must_use]
pub fn layout(app: &AppData, area: Rect) -> Vec<(ProjectKey, Rect)> {
    let n = app.tabs.len();
    if n == 0 || area.h <= 0.0 {
        return Vec::new();
    }
    let avail = (area.w - PAD_X * 2.0).max(0.0);
    let even = (avail - TAB_GAP * (n as f32 - 1.0)) / n as f32;
    let w = if even >= TAB_MIN_W { even.min(TAB_MAX_W) } else { even.max(TAB_FLOOR_W) };
    app.tabs
        .order
        .iter()
        .enumerate()
        .map(|(i, key)| {
            (
                *key,
                Rect { x: area.x + PAD_X + i as f32 * (w + TAB_GAP), y: area.y + 2.0, w, h: area.h - 2.0 },
            )
        })
        .collect()
}

/// hover / ドラッグ並べ替えの retained 状態。
#[derive(Default)]
struct TabStripState {
    /// いま pointer が乗っているタブと、乗り始めた時刻。
    hover: Option<(ProjectKey, Instant)>,
    /// spring-loaded 切替を済ませたタブ (同じ hover で 2 度切り替えない)。
    sprung: Option<ProjectKey>,
    /// 並べ替えドラッグ中のタブと、しきい値を超えたか。
    drag: Option<(ProjectKey, bool)>,
}

fn state_id() -> WidgetId {
    WidgetId::ROOT.child((b"tab_strip", &"state"))
}

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let core = &app.theme.core;
    ui.panel("tab_strip_bg", area, core.header, 0.0);
    let tabs = layout(app, area);
    let now = app.ui_ephemeral.frame_now;
    let pointer = ui.pointer();
    let hovered_key = pointer
        .pos
        .and_then(|(px, py)| tabs.iter().find(|(_, r)| r.contains(px, py)).map(|(k, _)| *k));
    let xfer_dragging = ui.dragging_kind() == Some(PROJECT_XFER_DRAG_KIND);

    // ---- hover の経過時間 (tooltip / spring-loaded) ----
    let (hover_since, sprung) = {
        let st: &mut TabStripState = ui.widget_state(state_id());
        match (hovered_key, st.hover) {
            (Some(k), Some((hk, since))) if hk == k => st.hover = Some((k, since)),
            (Some(k), _) => {
                st.hover = Some((k, now));
                st.sprung = None;
            }
            (None, _) => {
                st.hover = None;
                st.sprung = None;
            }
        }
        (st.hover, st.sprung)
    };
    if xfer_dragging
        && let Some((k, since)) = hover_since
        && k != app.pk()
        && sprung != Some(k)
        && now.duration_since(since) >= SPRING_LOAD
    {
        let st: &mut TabStripState = ui.widget_state(state_id());
        st.sprung = Some(k);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Tab(TabEvent::Switch(k)));
        }));
    }
    // hover の**経過で何かが変わる間だけ**描き続ける (tooltip が出るまで / spring-loaded で
    // 切り替わるまで)。出たあとも要求し続けると、タブの上にポインタを置いたままにしただけで
    // アイドル省電力 (r.md #49) が効かなくなる。
    if let Some((k, since)) = hover_since {
        let waited = now.duration_since(since);
        let tooltip_pending = waited < TOOLTIP_DELAY;
        let spring_pending = xfer_dragging && k != app.pk() && sprung != Some(k) && waited < SPRING_LOAD;
        if tooltip_pending || spring_pending {
            ui.request_redraw();
        }
    }

    // ---- 各タブ ----
    let mut drop_indicator: Option<f32> = None;
    for (i, (key, _)) in tabs.iter().enumerate() {
        draw_tab(app, ui, &tabs, i, *key, hovered_key, &mut drop_indicator);
    }

    // 並べ替え先の縦線。
    if let Some(x) = drop_indicator {
        ui.panel(
            "tab_drop_indicator",
            Rect { x: x - 1.0, y: area.y + 2.0, w: 2.0, h: area.h - 2.0 },
            core.accent,
            0.0,
        );
    }

    // 空き領域のダブルクリック = 新しいタブ。
    let used = tabs.last().map_or(area.x + PAD_X, |(_, r)| r.x + r.w + TAB_GAP);
    let empty = Rect { x: used, y: area.y, w: (area.x + area.w - used).max(0.0), h: area.h };
    if empty.w > 0.0 && ui.take_double_click_in_rect(empty).is_some() {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::Tab(TabEvent::New));
        }));
    }
}

/// 1 タブぶんの入力 (右クリック / ✕ / クリック = 切替 / ドラッグ = 並べ替え) と描画。
/// `drop_indicator` は並べ替え中の挿入位置 (x) を返す。
fn draw_tab(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    tabs: &[(ProjectKey, Rect)],
    i: usize,
    key: ProjectKey,
    hovered_key: Option<ProjectKey>,
    drop_indicator: &mut Option<f32>,
) {
    let core = &app.theme.core;
    let pointer = ui.pointer();
    let rect = tabs[i].1;
    let Some(ps) = app.tab(key) else { return };
    let active = key == app.pk();
    let hovered = hovered_key == Some(key);
    // タブが細いときは ✕ を出さない (出すと名前が 1 文字も残らない)。閉じるのは
    // 右クリックメニュー / Ctrl+W で足りる。
    let has_close = rect.w >= CLOSE_MIN_TAB_W;
    let close_rect = Rect { x: rect.x + rect.w - CLOSE_W - 4.0, y: rect.y + (rect.h - CLOSE_W) * 0.5, w: CLOSE_W, h: CLOSE_W };
    let close_hovered = has_close && pointer.pos.is_some_and(|(px, py)| close_rect.contains(px, py));

    // 右クリックメニュー (rect 内の右クリックで開く。毎フレーム重ねる idiom)。
    ui.context_menu_for(rect, &["新しいタブ", "閉じる", "他のタブを閉じる", "すべて閉じる"], move |idx, ui| {
        let ev = match idx {
            0 => TabEvent::New,
            1 => TabEvent::Close(key),
            2 => TabEvent::CloseOthers(key),
            _ => TabEvent::CloseAll,
        };
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Tab(ev));
        }));
    });

    // ✕ (先に取る: ここで消費すれば下のドラッグ / 切替が始まらない)。
    if has_close && ui.take_primary_press_in_rect(close_rect).is_some() {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Tab(TabEvent::Close(key)));
        }));
    }

    // クリック = 切替、ドラッグ = 並べ替え (同じ press から始まる)。
    if let Some(drag) = ui.take_drag_in_rect(("tab_drag", key.0), rect) {
        match drag.kind {
            DragKind::Started => {
                let st: &mut TabStripState = ui.widget_state(state_id());
                st.drag = Some((key, false));
                if !active {
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::Tab(TabEvent::Switch(key)));
                    }));
                }
            }
            DragKind::Continuing => {
                let moved = drag.delta.0.abs() >= DRAG_THRESHOLD;
                let st: &mut TabStripState = ui.widget_state(state_id());
                if moved {
                    st.drag = Some((key, true));
                }
                if st.drag.is_some_and(|(_, m)| m) {
                    let to = insert_index(tabs, drag.current.0);
                    *drop_indicator = Some(indicator_x(tabs, to));
                }
            }
            DragKind::Released => {
                let st: &mut TabStripState = ui.widget_state(state_id());
                let moved = st.drag.take().is_some_and(|(_, m)| m);
                if moved {
                    let to = insert_index(tabs, drag.current.0);
                    // 自分より右へ落とすときは自分が抜けるぶん 1 つ詰める。
                    let to = if to > i { to - 1 } else { to };
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::Tab(TabEvent::Move { key, to }));
                    }));
                }
            }
        }
    }

    // ---- 描画 ----
    let fill = if active {
        core.panel
    } else if hovered {
        core.control_hover
    } else {
        core.control
    };
    ui.panel(("tab_bg", key.0), rect, fill, 3.0);
    if active {
        // アクティブの下線 (accent)。
        ui.panel(
            ("tab_active_line", key.0),
            Rect { x: rect.x, y: rect.y + rect.h - 2.0, w: rect.w, h: 2.0 },
            core.accent,
            0.0,
        );
    }
    let ink = if active { core.text } else { core.text_dim };
    let text_y = rect.y + (rect.h - FONT) * 0.5;
    let mut x = rect.x + 8.0;
    if ps.transport.is_playing {
        // 再生中の ▶ は**タブの面の上で**読めること (ライトテーマだと固定の緑が
        // `control` に沈む)。色相を保ったまま明度だけ寄せる。
        let play = core.adapt_on(fill, app.theme.daw.play);
        ui.label_at(("tab_play", key.0), "\u{25B6}", x, text_y, FONT, play);
        x += 14.0;
    }
    // 未保存の `*` は **名前とは別に、右端に固定で**描く。名前に足して 1 つの文字列に
    // すると、長いファイル名で省略されたときに `*` ごと消えて「保存済みに見える」。
    let dirty = ps.song_doc.is_dirty();
    let right = if has_close { close_rect.x - 4.0 } else { rect.x + rect.w - 4.0 };
    let star_w = if dirty { ui.measure_text("*", FONT) + 2.0 } else { 0.0 };
    let name_w = (right - star_w - x).max(0.0);
    ui.label_at_clipped(
        ("tab_name", key.0),
        &AppData::tab_label(ps),
        Rect { x, y: text_y, w: name_w, h: FONT * 1.3 },
        FONT,
        ink,
    );
    if dirty {
        ui.label_at(("tab_dirty", key.0), "*", right - star_w + 2.0, text_y, FONT, ink);
    }
    // ✕ は hover で浮く (常時出すと帯がうるさい)。アクティブは常に出す。
    if has_close && (active || hovered) {
        let ink = if close_hovered { core.ink_for(fill) } else { core.text_dim };
        ui.label_at(("tab_close", key.0), "\u{2715}", close_rect.x + 3.0, close_rect.y + 1.0, FONT, ink);
    }
}

/// pointer x から「どのタブの前に挿すか」(0..=n)。
fn insert_index(tabs: &[(ProjectKey, Rect)], px: f32) -> usize {
    tabs.iter()
        .position(|(_, r)| px < r.x + r.w * 0.5)
        .unwrap_or(tabs.len())
}

fn indicator_x(tabs: &[(ProjectKey, Rect)], to: usize) -> f32 {
    match tabs.get(to) {
        Some((_, r)) => r.x - TAB_GAP * 0.5,
        None => tabs.last().map_or(0.0, |(_, r)| r.x + r.w + TAB_GAP * 0.5),
    }
}

/// tooltip (フルパス)。root の **最後** に描く (帯の直下は transport なので、帯の中で
/// 描くと上書きされる)。hover が [`TOOLTIP_DELAY`] を超えたタブについて出す。
pub fn draw_tooltip(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    if area.h <= 0.0 || ui.pointer().primary_pressed || ui.dragging_kind().is_some() {
        return;
    }
    let hover = {
        let st: &mut TabStripState = ui.widget_state(state_id());
        st.hover
    };
    let Some((key, since)) = hover else { return };
    if app.ui_ephemeral.frame_now.duration_since(since) < TOOLTIP_DELAY {
        return;
    }
    let Some(ps) = app.tab(key) else { return };
    let Some((_, rect)) = layout(app, area).into_iter().find(|(k, _)| *k == key) else {
        return;
    };
    let text = ps
        .song_doc
        .file_path
        .as_ref()
        .map_or_else(|| "未保存のプロジェクト".to_string(), |p| p.display().to_string());
    let core = &app.theme.core;
    let w = ui.measure_text(&text, 11.0) + 16.0;
    let screen_w = ui.screen().width as f32;
    let x = rect.x.min((screen_w - w - 4.0).max(0.0));
    let tip = Rect { x, y: rect.y + rect.h + 4.0, w, h: 20.0 };
    ui.panel_with_border("tab_tooltip_bg", tip, core.panel_raised, core.border, 1.0, 3.0);
    ui.label_at("tab_tooltip", &text, tip.x + 8.0, tip.y + 4.0, 11.0, core.text);
}
