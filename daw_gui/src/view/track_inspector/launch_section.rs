//! インスペクタの「ローンチ」(r.md #87 / 計画書 Q7 / §3.4)。r.md #158 で、セルの設定は選択の帯
//! (クリップ) の中の小見出し「ローンチ」、列のフォローアクションは列の帯に置く。
//!
//! 選択中の **ランチャーのセル** の量子化 / ローンチモード / ループ / レガート /
//! フォローアクション一式を出す。複数選択は一括変更で、値が割れている欄は
//! dropdown なら空表示 (選択 index を範囲外にする)、トグルなら OFF 表示にして
//! 「押したら全部 ON に揃う」 側へ倒す (トグルは 3 状態を持てない)。
//!
//! ## 高さが変わるのは「ジャンプ」の行だけ
//!
//! インスペクタは 1 本の y カーソルで積む top-down フローなので、行が増えると下のコントロールが
//! 縦に逃げる。動作に「ジャンプ」を選んだときだけ、飛び先の行がフォローアクションの直下に増える
//! (押した dropdown より下だけが動く。押した dropdown 自身は動かない)。
//!
//! ## 既存 idiom の流用
//!
//! `ui.dropdown` / `ui.toggle_button_at` / `ui.scrubable_number_at` と
//! `super::{scrub_style, toggle_audio_style}` をそのまま使う。bespoke な
//! edit-buffer widget は作らない (`feedback_reuse_inspector_idiom`)。

use std::sync::LazyLock;

use daw_ui_core::{Edit, ScrubableNumberFormat, ScrubableNumberStyle, Ui};
use daw_ui_renderer::Rect;

use common::model::{FollowAction, FollowActionKind, LaunchMode, LAUNCH_QUANTIZE_CHOICES};

use crate::app::{AppData, AppEvent, ClipKey, InspectorScrubField};
use crate::event_launcher::{LaunchEdit, LauncherCellKey, LauncherEvent};
use crate::state::inspector_view::InspectorDetail;

/// 1 行の高さ (dropdown / toggle / 数値欄で共通)。
const ROW_H: f32 = 22.0;
/// 行間。
const ROW_GAP: f32 = 4.0;
/// フォローアクション行の dropdown の本体フォント。3 分割の狭い列に「クリップ終端」
/// 「次のセル」のような語が入る (既定の `CONTROL_FONT_PX` と同じ 12px。既定が変わっても
/// この列は詰まらない大きさに固定する)。
const FOLLOW_FONT: f32 = 12.0;

/// ローンチモードの表示順 (enum の並びと一致させる)。
const MODE_CHOICES: &[(LaunchMode, &str)] = &[
    (LaunchMode::Trigger, "Trigger"),
    (LaunchMode::Gate, "Gate"),
    (LaunchMode::Toggle, "Toggle"),
    (LaunchMode::Repeat, "Repeat"),
];

/// フォローアクション 10 種のラベル (index = [`follow_index`] の返り値)。
const FOLLOW_LABELS: &[&str] = &[
    "なし",
    "停止",
    "もう一度",
    "前のセル",
    "次のセル",
    "先頭",
    "末尾",
    "ランダム",
    "別のセル",
    "ジャンプ",
];

/// Linked / Unlinked の 2 択。
const LINK_LABELS: &[&str] = &["クリップ終端", "指定時間"];

/// 量子化 dropdown のラベル列 (`LAUNCH_QUANTIZE_CHOICES` が SSoT)。
static QUANTIZE_LABELS: LazyLock<Vec<&'static str>> =
    LazyLock::new(|| LAUNCH_QUANTIZE_CHOICES.iter().map(|(_, l)| *l).collect());

/// ローンチモード dropdown のラベル列。
static MODE_LABELS: LazyLock<Vec<&'static str>> =
    LazyLock::new(|| MODE_CHOICES.iter().map(|(_, l)| *l).collect());

/// フォローアクションの編集先。セルと列で同じ 3 行を描くので 1 本にまとめる。
#[derive(Clone, Copy, PartialEq, Eq)]
enum FollowTarget {
    /// 選択中のセルの [`LaunchSettings::follow`](common::model::LaunchSettings::follow)。
    Cells,
    /// 選択中のセルが乗っている列の [`Scene::follow`](common::model::Scene::follow)。
    /// **走行中のクリップのフォローアクションより優先する** (Live 12 の規則)。
    Scenes,
}

/// 選択の帯 (クリップ / セル) の中の「ローンチ」: 選んでいるセルのローンチ設定と、MIDI 割り当て
/// (詳細、最初は畳む)。セルを選んでいなければ何も描かず `y` をそのまま返す (r.md #158)。
pub(super) fn draw_cell_launch(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect, pad: f32, y: f32) -> f32 {
    // 表示は **セル選択が生きているか** で決める (`shown_pianoroll_clips` と同じ判定)。
    // last-wins タグ付きの `selected_launcher_cells()` は Delete / Cut の宛先用なので、
    // セルのピアノロールでノートを選ぶと `Notes` へ倒れ、**セルを編集している最中に
    // ローンチ設定だけが消える**。
    let cells = app.live_launcher_cells();
    if cells.is_empty() {
        return y;
    }
    // セル側の行は **セルを選んでいるときだけ**出す。列見出しだけを選んだときに
    // セルの量子化 / 長さ / ループを出すと、触れる対象が無いのに操作できてしまう。
    let scene_ids = scenes_of(app, &cells);
    let y = super::band::sub_heading(
        app,
        ui,
        "inspector_launch_head",
        area.x + pad,
        area.w - pad * 2.0,
        y,
        super::vocab::SUB_LAUNCH,
        None,
    );
    let y = draw_cell_rows(app, ui, area, pad, y, &cells);
    draw_midi_rows(app, ui, area, pad, y, &cells, &scene_ids)
}

/// 列の帯の中身: 選んでいる列 (シーン) のフォローアクション。**走行中のクリップのそれより優先する**
/// (Live 12 の規則)。列見出しを選んだときだけ出る (r.md #153: セルを選んだだけで「セルが乗っている列」の
/// 設定まで並ぶと、セルのフォローアクションと紛れる)。セルを選んでいなければ MIDI 割り当てもここに出す
/// (セルがあればクリップの帯に出す — 1 つの選択に 1 か所)。どの列かは帯の対象 (列名) が示す。
pub(super) fn draw_scene_launch(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect, pad: f32, y: f32) -> f32 {
    let cells = app.live_launcher_cells();
    // `scenes_of` は明示的な列選択があればそれを返すので、表示と書き込み先は食い違わない。
    let scene_ids = scenes_of(app, &cells);
    if scene_ids.is_empty() {
        return y;
    }
    let scene_follow = fold_scene_follow(app, &scene_ids).unwrap_or_default();
    let y = follow_heading(app, ui, area, pad, y, "inspector_scene_follow_head");
    let y = draw_follow_rows(app, ui, area, pad, y, &scene_follow, FollowTarget::Scenes, &cells);
    if cells.is_empty() { draw_midi_rows(app, ui, area, pad, y, &cells, &scene_ids) } else { y }
}

/// 選択セルのローンチ設定 (量子化 / モード / 長さ / ループ / レガート / フォロー)。
fn draw_cell_rows(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    mut y: f32,
    cells: &[LauncherCellKey],
) -> f32 {
    let row_w = area.w - pad * 2.0;
    let half = (row_w - ROW_GAP) * 0.5;

    // ---- 量子化 / モード ----
    let quantize = app.launch_fold(cells, |s| s.quantize);
    let q_idx = quantize
        .and_then(|q| LAUNCH_QUANTIZE_CHOICES.iter().position(|(c, _)| *c == q))
        // 値が割れているときは選択なし (dropdown は範囲外 index で空表示になる)。
        .unwrap_or(usize::MAX);
    if let Some(i) = ui.dropdown(
        "inspector_launch_quantize",
        Rect { x: area.x + pad, y, w: half, h: ROW_H },
        QUANTIZE_LABELS.as_slice(),
        q_idx,
    ) && let Some((q, _)) = LAUNCH_QUANTIZE_CHOICES.get(i)
    {
        push_cell_edit(ui, cells, LaunchEdit::Quantize(*q));
    }
    let mode = app.launch_fold(cells, |s| s.mode);
    let m_idx = mode
        .and_then(|m| MODE_CHOICES.iter().position(|(c, _)| *c == m))
        .unwrap_or(usize::MAX);
    if let Some(i) = ui.dropdown(
        "inspector_launch_mode",
        Rect { x: area.x + pad + half + ROW_GAP, y, w: half, h: ROW_H },
        MODE_LABELS.as_slice(),
        m_idx,
    ) && let Some((m, _)) = MODE_CHOICES.get(i)
    {
        push_cell_edit(ui, cells, LaunchEdit::Mode(*m));
    }
    y += ROW_H + ROW_GAP;

    // ---- 長さ (ループ長) ----
    // セルは格子の中の固定サイズなので、アレンジのクリップのように端を掴めない。
    // ここが**セルの長さを変える唯一の口**。値が割れているときは 0 を出す
    // (触ると全部その値に揃う)。
    ui.label_at(
        "inspector_launch_len_label",
        "長さ (拍)",
        area.x + pad,
        y + 4.0,
        11.0,
        app.theme.core.text_dim,
    );
    let len = app.launch_cell_length_fold(cells).unwrap_or(0.0);
    {
        let style = ScrubableNumberStyle {
            sensitivity: 0.05,
            range: Some((f64::from(common::model::MIN_CLIP_LEN_BEATS as f32), 4096.0)),
            ..super::scrub_style(&app.theme)
        };
        let cells_for_len = cells.to_vec();
        let resp = ui.scrubable_number_at(
            ("inspector_launch_len", "cells"),
            Rect { x: area.x + pad + half + ROW_GAP, y, w: half, h: ROW_H },
            len,
            4.0,
            ScrubableNumberFormat::Decimal(2),
            &style,
            move |v| {
                let cells = cells_for_len.clone();
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::Launcher(LauncherEvent::SetCellLength {
                        cells: cells.clone(),
                        beats: v,
                    }));
                })
            },
            None,
            None,
        );
        // 4 → 64 の 1 ドラッグで undo が数十 step 積まれないよう bracket する
        // (`super::push_scrub_bracket` が inspector 共通の 1 本)。
        super::push_scrub_bracket(
            ui,
            app,
            InspectorScrubField::Launch(("inspector_launch_len", "cells")),
            resp.dragging || resp.editing_text,
        );
    }
    y += ROW_H + ROW_GAP;

    // ---- ループ / レガート ----
    // 値が割れているときは OFF 表示にして、押すと「全部 ON」 に揃える
    // (トグルは 3 状態を持てないので、揃える方向を 1 つに決める)。
    let looping = app.launch_fold(cells, |s| s.looping).unwrap_or(false);
    toggle(
        ui,
        app,
        "inspector_launch_loop",
        "ループ",
        Rect { x: area.x + pad, y, w: half, h: ROW_H },
        looping,
        cells,
        LaunchEdit::Looping(!looping),
    );
    let legato = app.launch_fold(cells, |s| s.legato).unwrap_or(false);
    toggle(
        ui,
        app,
        "inspector_launch_legato",
        "レガート",
        Rect { x: area.x + pad + half + ROW_GAP, y, w: half, h: ROW_H },
        legato,
        cells,
        LaunchEdit::Legato(!legato),
    );
    y += ROW_H + ROW_GAP;

    // ---- フォローアクション (セル) ----
    let cell_follow = app.launch_fold(cells, |s| s.follow.clone()).unwrap_or_default();
    let y = follow_heading(app, ui, area, pad, y, "inspector_follow_head");
    draw_follow_rows(app, ui, area, pad, y, &cell_follow, FollowTarget::Cells, cells)
}

/// 小見出し「フォローアクション」(セルと列で同じ形)。
fn follow_heading(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect, pad: f32, y: f32, id: &'static str) -> f32 {
    super::band::sub_heading(app, ui, id, area.x + pad, area.w - pad * 2.0, y + 2.0, "フォローアクション", None)
}

/// フォローアクションの 2 行 (有効・発火条件 / 動作 A・確率・動作 B) と、ジャンプの
/// ときだけ出る飛び先の行を積む。セルと列で同じ形なので `target` だけを変えて 2 回呼ぶ。
#[allow(clippy::too_many_arguments)]
fn draw_follow_rows(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    mut y: f32,
    follow: &FollowAction,
    target: FollowTarget,
    cells: &[LauncherCellKey],
) -> f32 {
    let row_w = area.w - pad * 2.0;
    let third = (row_w - ROW_GAP * 2.0) / 3.0;
    let x0 = area.x + pad;
    let x1 = x0 + third + ROW_GAP;
    let x2 = x1 + third + ROW_GAP;
    let tag = match target {
        FollowTarget::Cells => "cell",
        FollowTarget::Scenes => "scene",
    };

    // 行 1: 有効 / Linked-Unlinked / 倍率 or 時間
    let enabled = follow.enabled;
    let ed = LaunchEdit::FollowEnabled(!enabled);
    ui.toggle_button_at(
        ("inspector_follow_enabled", tag),
        "有効",
        Rect { x: x0, y, w: third, h: ROW_H },
        enabled,
        &super::toggle_audio_style(&app.theme),
        follow_edit_cb(target, cells, app, ed),
    );
    let link_idx = usize::from(!follow.linked);
    if let Some(i) = ui.dropdown_with_font(
        ("inspector_follow_link", tag),
        Rect { x: x1, y, w: third, h: ROW_H },
        LINK_LABELS,
        link_idx,
        FOLLOW_FONT,
    ) {
        push_follow_edit(ui, target, cells, app, LaunchEdit::FollowLinked(i == 0));
    }
    // Linked のときはループ回数、Unlinked のときは発火間隔 (拍)。同じ枠を使い回す
    // ので、切り替えても下の行は動かない。
    if follow.linked {
        let mult = f64::from(follow.multiplier);
        num_field(
            ui,
            ("inspector_follow_mult", tag),
            Rect { x: x2, y, w: third, h: ROW_H },
            mult,
            1.0,
            ScrubableNumberFormat::Integer,
            (1.0, 64.0),
            0.05,
            app,
            target,
            cells,
            |v| LaunchEdit::FollowMultiplier(v.round().clamp(1.0, 64.0) as u8),
        );
    } else {
        num_field(
            ui,
            ("inspector_follow_time", tag),
            Rect { x: x2, y, w: third, h: ROW_H },
            follow.time_beats,
            4.0,
            ScrubableNumberFormat::Decimal(2),
            (0.0625, 512.0),
            0.02,
            app,
            target,
            cells,
            LaunchEdit::FollowTimeBeats,
        );
    }
    y += ROW_H + ROW_GAP;

    // 行 2: 動作 A / A の確率 / 動作 B を 1 行に。「ジャンプ」の飛び先は、どちらかが
    // ジャンプのときだけ行 3 に出す (普段は 2 行で収まる)。
    y = draw_follow_action_row(app, ui, area, pad, y, follow, target, cells);
    draw_follow_jump_row(app, ui, area, pad, y, follow, target, cells)
}

/// 動作 A / B の 1 行 (`[動作 A ▾][A の確率][動作 B ▾]`)。B の確率は `100 - A` なので
/// 入力欄は 1 つ。
#[allow(clippy::too_many_arguments)]
fn draw_follow_action_row(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    y: f32,
    follow: &FollowAction,
    target: FollowTarget,
    cells: &[LauncherCellKey],
) -> f32 {
    let row_w = area.w - pad * 2.0;
    let third = (row_w - ROW_GAP * 2.0) / 3.0;
    let x0 = area.x + pad;
    let x1 = x0 + third + ROW_GAP;
    let x2 = x1 + third + ROW_GAP;
    let (tag_a, tag_b) = match target {
        FollowTarget::Cells => ("cell_a", "cell_b"),
        FollowTarget::Scenes => ("scene_a", "scene_b"),
    };
    // 「ジャンプ」を選んだ直後の飛び先は先頭の列 (行 3 の dropdown で変える)。
    let first_scene = app.cur.song_doc.song().scenes.first().map_or(0, |s| s.id);
    for (is_a, x, tag) in [(true, x0, tag_a), (false, x2, tag_b)] {
        let kind = if is_a { follow.a } else { follow.b };
        if let Some(i) = ui.dropdown_with_font(
            ("inspector_follow_kind", tag),
            Rect { x, y, w: third, h: ROW_H },
            FOLLOW_LABELS,
            follow_index(kind),
            FOLLOW_FONT,
        ) {
            let next = follow_from_index(i, first_scene);
            let edit = if is_a { LaunchEdit::FollowA(next) } else { LaunchEdit::FollowB(next) };
            push_follow_edit(ui, target, cells, app, edit);
        }
    }
    num_field(
        ui,
        ("inspector_follow_chance", tag_a),
        Rect { x: x1, y, w: third, h: ROW_H },
        f64::from(follow.chance_a),
        100.0,
        ScrubableNumberFormat::Integer,
        (0.0, 100.0),
        0.3,
        app,
        target,
        cells,
        |v| LaunchEdit::FollowChanceA(v.round().clamp(0.0, 100.0) as u8),
    );
    y + ROW_H + ROW_GAP
}

/// 「ジャンプ」の飛び先の行 (`[A の飛び先 ▾][ ][B の飛び先 ▾]`)。A / B のどちらもジャンプで
/// なければ描かず高さも足さない。列が 1 つも無ければ同じく描かない。
#[allow(clippy::too_many_arguments)]
fn draw_follow_jump_row(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    y: f32,
    follow: &FollowAction,
    target: FollowTarget,
    cells: &[LauncherCellKey],
) -> f32 {
    let scenes = &app.cur.song_doc.song().scenes;
    let jump_of = |k: FollowActionKind| match k {
        FollowActionKind::Jump { scene_id } => Some(scene_id),
        _ => None,
    };
    let (jump_a, jump_b) = (jump_of(follow.a), jump_of(follow.b));
    if scenes.is_empty() || (jump_a.is_none() && jump_b.is_none()) {
        return y;
    }
    let row_w = area.w - pad * 2.0;
    let third = (row_w - ROW_GAP * 2.0) / 3.0;
    let x0 = area.x + pad;
    let x2 = x0 + (third + ROW_GAP) * 2.0;
    let (tag_a, tag_b) = match target {
        FollowTarget::Cells => ("cell_a", "cell_b"),
        FollowTarget::Scenes => ("scene_a", "scene_b"),
    };
    let names: Vec<String> = scenes.iter().enumerate().map(|(i, s)| s.display_name(i)).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    for (is_a, x, tag, jump) in [(true, x0, tag_a, jump_a), (false, x2, tag_b, jump_b)] {
        let Some(scene_id) = jump else { continue };
        let sel = scenes.iter().position(|s| s.id == scene_id).unwrap_or(usize::MAX);
        if let Some(i) = ui.dropdown_with_font(
            ("inspector_follow_jump", tag),
            Rect { x, y, w: third, h: ROW_H },
            &refs,
            sel,
            FOLLOW_FONT,
        ) && let Some(s) = scenes.get(i)
        {
            let next = FollowActionKind::Jump { scene_id: s.id };
            let edit = if is_a { LaunchEdit::FollowA(next) } else { LaunchEdit::FollowB(next) };
            push_follow_edit(ui, target, cells, app, edit);
        }
    }
    y + ROW_H + ROW_GAP
}

/// MIDI Learn の 6 ボタン + 消去 (計画書 §3.5)。
/// パッドで撃てないランチャーは半分の機能しかないので、
/// **セル / シーン / 行停止 / 行→アレンジ / 全停止 / 全→アレンジ** を全部出す。
#[allow(clippy::too_many_arguments)]
fn draw_midi_rows(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    y: f32,
    cells: &[LauncherCellKey],
    scene_ids: &[u32],
) -> f32 {
    let row_w = area.w - pad * 2.0;
    // 学習の宛先は「いま選んでいるセル」 (anchor = 末尾)。トラック行のセルだけが
    // パッドの宛先になる (パッドは行 × 列の格子で、オートメーションレーンには
    // 対応する物理位置が無い)。
    let anchor = cells.iter().rev().find_map(|c| match c {
        LauncherCellKey::Track(k) => Some(*k),
        LauncherCellKey::Lane(_) => None,
    });
    // 列の宛先は **セル経由と直接選択の両方**から取る ([`scenes_of`] が同じ 1 本)。
    // セル経由だけにしていると、列見出しを選んだときに「シーン」 の Learn が
    // 押せず (宛先 `None`)、**セルを 1 つも持たない列は永久に MIDI へ割り当て
    // られない** — 列を直接選べるようにした意味が半分無くなる。
    // 行の宛先 (セル / 行を止める / 行→アレンジ) は行が要るので、列だけの選択では
    // `None` のまま (押しても何も起きないのが正しい)。
    let scene_id = anchor
        .map(LauncherCellKey::Track)
        .and_then(|c| app.scene_of_cell(c))
        .or_else(|| scene_ids.last().copied());
    let learning = app.cur.launcher.learn_target;
    let n = app.launcher_bindings().len();
    // めったに使わないので「詳細」で最初は畳む (r.md #158 仕様 7、セルと列で同じ鍵)。
    let label = format!("MIDI 割り当て ({n} 件)");
    super::band::details(app, ui, area.x + pad, row_w, y, InspectorDetail::LaunchMidi, &label, |ui, y| {
        draw_midi_buttons(app, ui, area, pad, y, anchor, scene_id, learning)
    })
}

/// MIDI 割り当ての中身: Learn の 6 ボタン + 消去。
#[allow(clippy::too_many_arguments)]
fn draw_midi_buttons(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    mut y: f32,
    anchor: Option<ClipKey>,
    scene_id: Option<u32>,
    learning: Option<common::model::BindingTarget>,
) -> f32 {
    let row_w = area.w - pad * 2.0;
    let third = (row_w - ROW_GAP * 2.0) / 3.0;

    // (widget id, ラベル, 宛先)。宛先が `None` = いま決められない
    // (セル未選択 / 列が無い) ので押しても何もしない。
    let targets: [(&str, &str, Option<common::model::BindingTarget>); 6] = [
        (
            "learn_cell",
            "セル",
            anchor.zip(scene_id).map(|(k, s)| common::model::BindingTarget::LaunchCell {
                track_id: k.track_id,
                scene_id: s,
            }),
        ),
        (
            "learn_scene",
            "シーン",
            scene_id.map(|s| common::model::BindingTarget::LaunchScene { scene_id: s }),
        ),
        (
            "learn_stop_row",
            "行を止める",
            anchor.map(|k| common::model::BindingTarget::StopLauncherRow { track_id: k.track_id }),
        ),
        (
            "learn_row_arr",
            "行→アレンジ",
            anchor.map(|k| common::model::BindingTarget::SwitchRowToArranger { track_id: k.track_id }),
        ),
        ("learn_stop_all", "全停止", Some(common::model::BindingTarget::StopAllLauncherRows)),
        ("learn_all_arr", "全→アレンジ", Some(common::model::BindingTarget::SwitchAllToArranger)),
    ];

    for (i, (id, label, target)) in targets.into_iter().enumerate() {
        let col = i % 3;
        let rect = Rect {
            x: area.x + pad + (third + ROW_GAP) * col as f32,
            y: y + (ROW_H + ROW_GAP) * (i / 3) as f32,
            w: third,
            h: ROW_H,
        };
        let active = learning == target && target.is_some();
        ui.toggle_button_at(
            ("inspector_launch_learn", id),
            label,
            rect,
            active,
            &super::toggle_audio_style(&app.theme),
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    let ev = match (active, target) {
                        (true, _) => LauncherEvent::CancelLearn,
                        (false, Some(t)) => LauncherEvent::StartLearn(t),
                        // 宛先が決まらない (セル未選択 / 列が無い) ときは何もしない。
                        (false, None) => return,
                    };
                    app.handle_event(AppEvent::Launcher(ev));
                })
            },
        );
    }
    y += (ROW_H + ROW_GAP) * 2.0;

    ui.button_at(
        "inspector_launch_clear_binds",
        "割り当てを消す",
        Rect { x: area.x + pad, y, w: row_w, h: ROW_H },
        || {
            Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::Launcher(LauncherEvent::ClearBindings));
            })
        },
    );
    y + ROW_H
}

// ----------------------------------------------------------------------
// 小物 — 「どこへ書くか」 を 1 本に集約する
// ----------------------------------------------------------------------

/// ローンチ設定の変更 1 つを `Edit` にする **唯一の builder**。
///
/// 宛先がセルか列かで発火する event が違うだけなので、dropdown / toggle /
/// 数値欄が別々に `Edit::mutate` を書かずに済むよう 1 本にまとめる
/// (書き分けると「列側だけ更新し忘れた」 が起きる)。
fn launch_edit_for(
    target: FollowTarget,
    cells: &[LauncherCellKey],
    scene_ids: &[u32],
    edit: LaunchEdit,
) -> Edit<AppData> {
    match target {
        FollowTarget::Cells => {
            let cells = cells.to_vec();
            Edit::mutate(move |app: &mut AppData| {
                let ev = LauncherEvent::SetLaunchSettings { cells: cells.clone(), edit };
                app.handle_event(AppEvent::Launcher(ev));
            })
        }
        FollowTarget::Scenes => {
            let scene_ids = scene_ids.to_vec();
            Edit::mutate(move |app: &mut AppData| {
                let ev = LauncherEvent::SetSceneFollow { scene_ids: scene_ids.clone(), edit };
                app.handle_event(AppEvent::Launcher(ev));
            })
        }
    }
}

/// セルのローンチ設定を 1 つ変える `Edit` を積む (量子化 / モード用)。
fn push_cell_edit(ui: &mut Ui<'_, AppData>, cells: &[LauncherCellKey], edit: LaunchEdit) {
    ui.push_edit(launch_edit_for(FollowTarget::Cells, cells, &[], edit));
}

/// フォローアクションを 1 つ変える `Edit` を積む (セル / 列で宛先が変わる)。
fn push_follow_edit(
    ui: &mut Ui<'_, AppData>,
    target: FollowTarget,
    cells: &[LauncherCellKey],
    app: &AppData,
    edit: LaunchEdit,
) {
    let scene_ids = scenes_of(app, cells);
    ui.push_edit(launch_edit_for(target, cells, &scene_ids, edit));
}

/// `toggle_button_at` の `on_click` に渡すクロージャ (フォローアクションの「有効」)。
fn follow_edit_cb(
    target: FollowTarget,
    cells: &[LauncherCellKey],
    app: &AppData,
    edit: LaunchEdit,
) -> impl Fn(bool) -> Edit<AppData> + use<> {
    let cells = cells.to_vec();
    let scene_ids = scenes_of(app, &cells);
    move |_| launch_edit_for(target, &cells, &scene_ids, edit)
}

/// トグル 1 つ (セル側の ループ / レガート)。
#[allow(clippy::too_many_arguments)]
fn toggle(
    ui: &mut Ui<'_, AppData>,
    app: &AppData,
    id: &'static str,
    label: &str,
    rect: Rect,
    on: bool,
    cells: &[LauncherCellKey],
    edit: LaunchEdit,
) {
    let cells = cells.to_vec();
    let style = super::toggle_audio_style(&app.theme);
    ui.toggle_button_at(id, label, rect, on, &style, move |_| {
        launch_edit_for(FollowTarget::Cells, &cells, &[], edit)
    });
}

/// 数値欄 1 つ (drag scrub + click で数値入力)。
///
/// **drag / テキスト編集の stroke は必ず undo 1 step に bracket する** — フォロー
/// アクションの確率 / 時間 / 倍率は per-frame に `SetLaunchSettings` (= `edit_song`)
/// を撃つので、束ねないと 1 ドラッグで `UNDO_LIMIT` (200) を溢れさせ、それ以前の
/// 実編集履歴が `pop_front` で捨てられる。
#[allow(clippy::too_many_arguments)]
fn num_field(
    ui: &mut Ui<'_, AppData>,
    id: (&'static str, &'static str),
    rect: Rect,
    value: f64,
    default: f64,
    fmt: ScrubableNumberFormat,
    range: (f64, f64),
    sensitivity: f32,
    app: &AppData,
    target: FollowTarget,
    cells: &[LauncherCellKey],
    make: impl Fn(f64) -> LaunchEdit + Clone + Send + Sync + 'static,
) {
    let style = ScrubableNumberStyle {
        sensitivity,
        range: Some(range),
        ..super::scrub_style(&app.theme)
    };
    let cells = cells.to_vec();
    let scene_ids = scenes_of(app, &cells);
    let resp = ui.scrubable_number_at(
        id,
        rect,
        value,
        default,
        fmt,
        &style,
        move |v| launch_edit_for(target, &cells, &scene_ids, make(v)),
        None,
        None,
    );
    // key は欄の widget id そのもの (`tag` に宛先が入っているので列側と衝突しない)。
    super::push_scrub_bracket(
        ui,
        app,
        InspectorScrubField::Launch(id),
        resp.dragging || resp.editing_text,
    );
}

/// 編集対象の列 (シーン) id (重複なし、表示順)。
///
/// **明示的な列選択があればそれ**、無ければ「選択セルが乗っている列」。列を直接
/// 選べるようになる前 (r.md #87 初版) は後者しか無く、セルを 1 つも持たない列の
/// フォローアクションを設定する手段が存在しなかった。
///
/// 表示 (`draw_launch_section`) と書き込み (`push_follow_edit`) が **同じこの 1 本**を
/// 通るので、「見えている値」と「書き込み先」が食い違わない。
fn scenes_of(app: &AppData, cells: &[LauncherCellKey]) -> Vec<u32> {
    if !app.cur.selection.selected_scene_ids.is_empty() {
        let mut ids = app.cur.selection.selected_scene_ids.clone();
        ids.sort_by_key(|id| app.cur.song_doc.song().scene_index(*id).unwrap_or(usize::MAX));
        return ids;
    }
    let mut ids: Vec<u32> = Vec::new();
    for c in cells {
        if let Some(s) = app.scene_of_cell(*c)
            && !ids.contains(&s)
        {
            ids.push(s);
        }
    }
    ids.sort_by_key(|id| app.cur.song_doc.song().scene_index(*id).unwrap_or(usize::MAX));
    ids
}

/// 列のフォローアクションを畳む (全部同じなら `Some`、割れていれば `None`)。
fn fold_scene_follow(app: &AppData, scene_ids: &[u32]) -> Option<FollowAction> {
    let song = app.cur.song_doc.song();
    let mut it = scene_ids
        .iter()
        .filter_map(|id| song.scenes.iter().find(|s| s.id == *id))
        .map(|s| s.follow.clone());
    let first = it.next()?;
    for f in it {
        if f != first {
            return None;
        }
    }
    Some(first)
}

/// [`FollowActionKind`] → [`FOLLOW_LABELS`] の index。
fn follow_index(k: FollowActionKind) -> usize {
    match k {
        FollowActionKind::NoAction => 0,
        FollowActionKind::Stop => 1,
        FollowActionKind::PlayAgain => 2,
        FollowActionKind::Previous => 3,
        FollowActionKind::Next => 4,
        FollowActionKind::First => 5,
        FollowActionKind::Last => 6,
        FollowActionKind::Any => 7,
        FollowActionKind::Other => 8,
        FollowActionKind::Jump { .. } => 9,
    }
}

/// [`FOLLOW_LABELS`] の index → [`FollowActionKind`]。
/// 「ジャンプ」の飛び先は `jump_scene` (呼び側が既定の列を渡す)。
fn follow_from_index(i: usize, jump_scene: u32) -> FollowActionKind {
    match i {
        1 => FollowActionKind::Stop,
        2 => FollowActionKind::PlayAgain,
        3 => FollowActionKind::Previous,
        4 => FollowActionKind::Next,
        5 => FollowActionKind::First,
        6 => FollowActionKind::Last,
        7 => FollowActionKind::Any,
        8 => FollowActionKind::Other,
        9 => FollowActionKind::Jump { scene_id: jump_scene },
        _ => FollowActionKind::NoAction,
    }
}
