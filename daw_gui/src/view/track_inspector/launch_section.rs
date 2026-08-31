//! インスペクタの「ローンチ」セクション (r.md #87 / 計画書 Q7 / §3.4)。
//!
//! 選択中の **ランチャーのセル** の量子化 / ローンチモード / ループ / レガート /
//! フォローアクション一式を出す。複数選択は一括変更で、値が割れている欄は
//! dropdown なら空表示 (選択 index を範囲外にする)、トグルなら OFF 表示にして
//! 「押したら全部 ON に揃う」 側へ倒す (トグルは 3 状態を持てない)。
//!
//! ## 行の高さは状態で変えない
//!
//! インスペクタは 1 本の y カーソルで積む top-down フローなので、
//! 「Jump を選んだときだけ行が増える」 ような作りにすると **下のコントロールが
//! 縦に逃げる**。飛び先 (Jump のシーン) は動作 dropdown と同じ行の右側に置き、
//! Jump 以外のときはそこを空けたままにして高さを固定する。
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

use crate::app::{AppData, AppEvent, InspectorScrubField};
use crate::event_launcher::{LaunchEdit, LauncherCellKey, LauncherEvent};

/// 1 行の高さ (dropdown / toggle / 数値欄で共通)。
const ROW_H: f32 = 22.0;
/// 行間。
const ROW_GAP: f32 = 4.0;
/// 見出しラベルの高さ。
const LABEL_H: f32 = 18.0;

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

/// 「ローンチ」セクションを 1 本の y カーソルに積む。
/// セルも列も選ばれていなければ何も描かず `y` をそのまま返す。
pub(super) fn draw_launch_section(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    mut y: f32,
) -> f32 {
    let cells = app.selected_launcher_cells();
    let scene_ids = scenes_of(app, &cells);
    if cells.is_empty() && scene_ids.is_empty() {
        return y;
    }
    // セル側の行は **セルを選んでいるときだけ**出す。列見出しだけを選んだときに
    // セルの量子化 / 長さ / ループを出すと、触れる対象が無いのに操作できてしまう。
    if !cells.is_empty() {
        y = draw_cell_rows(app, ui, area, pad, y, &cells);
    }
    y = draw_scene_follow_rows(app, ui, area, pad, y, &cells, &scene_ids);
    draw_midi_rows(app, ui, area, pad, y, &cells, &scene_ids)
}

/// 列 (シーン) のフォローアクション。**走行中のクリップのそれより優先する**
/// (Live 12 の規則) ので、セル側の下に並べる。
fn draw_scene_follow_rows(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    mut y: f32,
    cells: &[LauncherCellKey],
    scene_ids: &[u32],
) -> f32 {
    if scene_ids.is_empty() {
        return y;
    }
    let scene_follow = fold_scene_follow(app, scene_ids).unwrap_or_default();
    // 見出しは「どこから来た列か」で書き分ける — セル経由なら「この列」、
    // 列を直接選んでいるなら選んだ列そのもの。
    let label = if cells.is_empty() {
        "選択中の列 (シーン) のフォローアクション"
    } else {
        "この列 (シーン) のフォローアクション"
    };
    ui.label_at("inspector_scene_follow_label", label, area.x + pad, y, 12.0, app.theme.core.text);
    y += LABEL_H;
    draw_follow_rows(app, ui, area, pad, y, &scene_follow, FollowTarget::Scenes, cells)
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
    let p = &app.theme.core;
    let row_w = area.w - pad * 2.0;
    let half = (row_w - ROW_GAP) * 0.5;

    ui.label_at("inspector_launch_label", "ローンチ", area.x + pad, y, 12.0, p.text);
    y += LABEL_H;

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
    ui.label_at(
        "inspector_follow_label",
        "フォローアクション",
        area.x + pad,
        y,
        12.0,
        p.text,
    );
    y += LABEL_H;
    draw_follow_rows(app, ui, area, pad, y, &cell_follow, FollowTarget::Cells, cells)
}

/// フォローアクションの 3 行 (有効 / 発火条件 / 動作 A・B) を積む。
/// セルと列で同じ形なので `target` だけを変えて 2 回呼ぶ。
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
    if let Some(i) = ui.dropdown(
        ("inspector_follow_link", tag),
        Rect { x: x1, y, w: third, h: ROW_H },
        LINK_LABELS,
        link_idx,
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

    // 行 2 / 行 3: 動作 A (+ 確率) / 動作 B。 それぞれの右端に「ジャンプ」の
    // 飛び先 dropdown を置く (Jump 以外のときは空欄 = 高さは変わらない)。
    y = draw_follow_action_row(app, ui, area, pad, y, follow, target, cells, true);
    draw_follow_action_row(app, ui, area, pad, y, follow, target, cells, false)
}

/// 動作 A / B の 1 行 (`[動作 ▾][確率 or 空][飛び先 ▾]`)。
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
    is_a: bool,
) -> f32 {
    let row_w = area.w - pad * 2.0;
    let third = (row_w - ROW_GAP * 2.0) / 3.0;
    let x0 = area.x + pad;
    let x1 = x0 + third + ROW_GAP;
    let x2 = x1 + third + ROW_GAP;
    let tag = match (target, is_a) {
        (FollowTarget::Cells, true) => "cell_a",
        (FollowTarget::Cells, false) => "cell_b",
        (FollowTarget::Scenes, true) => "scene_a",
        (FollowTarget::Scenes, false) => "scene_b",
    };
    let kind = if is_a { follow.a } else { follow.b };

    if let Some(i) = ui.dropdown(
        ("inspector_follow_kind", tag),
        Rect { x: x0, y, w: third, h: ROW_H },
        FOLLOW_LABELS,
        follow_index(kind),
    ) {
        // 「ジャンプ」を選んだ直後の飛び先は先頭の列 (右の dropdown で変える)。
        let first_scene = app.song_doc.song().scenes.first().map_or(0, |s| s.id);
        let next = follow_from_index(i, first_scene);
        let edit = if is_a { LaunchEdit::FollowA(next) } else { LaunchEdit::FollowB(next) };
        push_follow_edit(ui, target, cells, app, edit);
    }
    // 確率は A の行だけ (B は `100 - a` なので入力欄を 2 つ置かない)。
    if is_a {
        num_field(
            ui,
            ("inspector_follow_chance", tag),
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
    }
    // 飛び先 (Jump のときだけ操作できる)。列が 1 つも無ければ描かない。
    let scenes = &app.song_doc.song().scenes;
    if let FollowActionKind::Jump { scene_id } = kind
        && !scenes.is_empty()
    {
        let names: Vec<String> =
            scenes.iter().enumerate().map(|(i, s)| s.display_name(i)).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let sel = scenes.iter().position(|s| s.id == scene_id).unwrap_or(usize::MAX);
        if let Some(i) = ui.dropdown(
            ("inspector_follow_jump", tag),
            Rect { x: x2, y, w: third, h: ROW_H },
            &refs,
            sel,
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
    mut y: f32,
    cells: &[LauncherCellKey],
    scene_ids: &[u32],
) -> f32 {
    let p = &app.theme.core;
    let row_w = area.w - pad * 2.0;
    let third = (row_w - ROW_GAP * 2.0) / 3.0;
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
    let learning = app.launcher.learn_target;
    let n = app.launcher_bindings().len();

    ui.label_at(
        "inspector_launch_midi_label",
        &format!("MIDI 割り当て ({n} 件)"),
        area.x + pad,
        y,
        12.0,
        p.text,
    );
    y += LABEL_H;

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
    y + ROW_H + 12.0
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
    if !app.selection.selected_scene_ids.is_empty() {
        let mut ids = app.selection.selected_scene_ids.clone();
        ids.sort_by_key(|id| app.song_doc.song().scene_index(*id).unwrap_or(usize::MAX));
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
    ids.sort_by_key(|id| app.song_doc.song().scene_index(*id).unwrap_or(usize::MAX));
    ids
}

/// 列のフォローアクションを畳む (全部同じなら `Some`、割れていれば `None`)。
fn fold_scene_follow(app: &AppData, scene_ids: &[u32]) -> Option<FollowAction> {
    let song = app.song_doc.song();
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
