//! インスペクタの「ローンチ」(r.md #87 / 計画書 Q7 / §3.4)。r.md #158 で、セルの設定は選択の帯
//! (クリップ) の中の小見出し「ローンチ」、列のフォローアクションは列の帯に置く。
//!
//! 選択中の **ランチャーのセル** の量子化 / ローンチモード / ループ / レガート /
//! フォローアクション一式を出す。複数選択は一括変更で、値が割れている欄は
//! dropdown なら空表示 (選択 index を範囲外にする)、トグルなら OFF 表示にして
//! 「押したら全部 ON に揃う」 側へ倒す (トグルは 3 状態を持てない)。どの欄も「ラベル左・値右」の行
//! (`form.rs`、r.md #158 仕様 9) — フォローアクションも MIDI 割り当ても、値が何かを欄名で言う。
//!
//! ## 高さは選んだ値で変わらない
//!
//! インスペクタは 1 本の y カーソルで積む top-down フローなので、行が増えると下のコントロールが
//! 縦に逃げる。アクションに「ジャンプ」を選んだときの飛び先は同じ行の右半分に出し (アクションの
//! dropdown はその分だけ狭まる、位置は動かない)、発動の「クリップ終端 / 指定時間」で入れ替わる
//! 回数 / 間隔も同じ行を使い回す。
//!
//! ## 既存 idiom の流用
//!
//! `ui.dropdown` / `ui.toggle_button_at` / `ui.scrubable_number_at` と
//! `track_inspector::{scrub_style, toggle_audio_style}` をそのまま使う。bespoke な
//! edit-buffer widget は作らない (`feedback_reuse_inspector_idiom`)。

use std::sync::LazyLock;

use daw_ui_core::{Edit, ScrubableNumberFormat, ScrubableNumberStyle, Ui};
use daw_ui_renderer::Rect;

use common::model::{FollowAction, FollowActionKind, LaunchMode, LAUNCH_QUANTIZE_CHOICES};

use super::super::form::{self, FormCols};
use super::super::layout::FORM_ROW_PITCH;
use super::super::vocab;
use crate::app::{AppData, AppEvent, ClipKey, InspectorScrubField};
use crate::event_launcher::{LaunchEdit, LauncherCellKey, LauncherEvent};
use crate::state::inspector_view::InspectorDetail;

/// 値の欄を 2 つに割った dropdown の本体フォント ([有効][クリップ終端 ▾] / [ジャンプ ▾][飛び先 ▾])。
/// 半分の幅に「クリップ終端」「次のセル」が入る大きさ (既定の `CONTROL_FONT_PX` と同じ 12px。既定が
/// 変わってもこの欄は詰まらない大きさに固定する)。
const HALF_FONT: f32 = 12.0;

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

/// フォローアクションの編集先。セルと列で同じ行を描くので 1 本にまとめる。
#[derive(Clone, Copy, PartialEq, Eq)]
enum FollowTarget {
    /// 選択中のセルの [`LaunchSettings::follow`](common::model::LaunchSettings::follow)。
    Cells,
    /// 選択中のセルが乗っている列の [`Scene::follow`](common::model::Scene::follow)。
    /// **走行中のクリップのフォローアクションより優先する** (Live 12 の規則)。
    Scenes,
}

impl FollowTarget {
    /// widget id の頭 (セルと列の欄を別の widget にする)。
    fn tag(self) -> &'static str {
        match self {
            Self::Cells => "cell",
            Self::Scenes => "scene",
        }
    }
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
    let c = form::cols(area.x + pad, area.w - pad * 2.0);
    let y = super::super::band::sub_heading(app, ui, "inspector_launch_head", c.label_x, area.w - pad * 2.0, y, vocab::SUB_LAUNCH, None);
    let y = draw_cell_rows(app, ui, &c, area.w - pad * 2.0, y, &cells);
    draw_midi_rows(app, ui, &c, area.w - pad * 2.0, y, &cells, &scene_ids)
}

/// 列の帯の中身: 選んでいる列 (シーン) のフォローアクション。**走行中のクリップのそれより優先する**
/// (Live 12 の規則)。列見出しを選んだときだけ出る (r.md #153: セルを選んだだけで「セルが乗っている列」の
/// 設定まで並ぶと、セルのフォローアクションと紛れる)。セルを選んでいなければ MIDI 割り当てもここに出す
/// (セルがあればクリップの帯に出す — 1 つの選択に 1 か所)。どの列かは帯の対象 (列名) が示す。
pub(in crate::view::track_inspector) fn draw_scene_launch(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect, pad: f32, y: f32) -> f32 {
    let cells = app.live_launcher_cells();
    // `scenes_of` は明示的な列選択があればそれを返すので、表示と書き込み先は食い違わない。
    let scene_ids = scenes_of(app, &cells);
    if scene_ids.is_empty() {
        return y;
    }
    let c = form::cols(area.x + pad, area.w - pad * 2.0);
    let w = area.w - pad * 2.0;
    let scene_follow = fold_scene_follow(app, &scene_ids).unwrap_or_default();
    let y = follow_heading(app, ui, &c, w, y, "inspector_scene_follow_head");
    let y = draw_follow_rows(app, ui, &c, y, &scene_follow, FollowTarget::Scenes, &cells);
    if cells.is_empty() { draw_midi_rows(app, ui, &c, w, y, &cells, &scene_ids) } else { y }
}

/// 選択セルのローンチ設定 (量子化 / モード / 長さ / ループ / レガート / フォロー)。
fn draw_cell_rows(app: &AppData, ui: &mut Ui<'_, AppData>, c: &FormCols, w: f32, mut y: f32, cells: &[LauncherCellKey]) -> f32 {
    // ---- 量子化 ----
    let quantize = app.launch_fold(cells, |s| s.quantize);
    let q_idx = quantize
        .and_then(|q| LAUNCH_QUANTIZE_CHOICES.iter().position(|(c, _)| *c == q))
        // 値が割れているときは選択なし (dropdown は範囲外 index で空表示になる)。
        .unwrap_or(usize::MAX);
    form::label(ui, app, "inspector_launch_quantize_label", vocab::FIELD_LAUNCH_QUANTIZE, c, y);
    if let Some(i) = ui.dropdown("inspector_launch_quantize", c.value(y), QUANTIZE_LABELS.as_slice(), q_idx)
        && let Some((q, _)) = LAUNCH_QUANTIZE_CHOICES.get(i)
    {
        push_cell_edit(ui, cells, LaunchEdit::Quantize(*q));
    }
    y += FORM_ROW_PITCH;

    // ---- モード ----
    let mode = app.launch_fold(cells, |s| s.mode);
    let m_idx = mode
        .and_then(|m| MODE_CHOICES.iter().position(|(c, _)| *c == m))
        .unwrap_or(usize::MAX);
    form::label(ui, app, "inspector_launch_mode_label", vocab::FIELD_LAUNCH_MODE, c, y);
    if let Some(i) = ui.dropdown("inspector_launch_mode", c.value(y), MODE_LABELS.as_slice(), m_idx)
        && let Some((m, _)) = MODE_CHOICES.get(i)
    {
        push_cell_edit(ui, cells, LaunchEdit::Mode(*m));
    }
    y += FORM_ROW_PITCH;

    // ---- 長さ (ループ長) ----
    // セルは格子の中の固定サイズなので、アレンジのクリップのように端を掴めない。
    // ここが**セルの長さを変える唯一の口**。値が割れているときは 0 を出す
    // (触ると全部その値に揃う)。
    form::label(ui, app, "inspector_launch_len_label", vocab::FIELD_LAUNCH_LENGTH, c, y);
    let len = app.launch_cell_length_fold(cells).unwrap_or(0.0);
    {
        let style = ScrubableNumberStyle {
            sensitivity: 0.05,
            range: Some((f64::from(common::model::MIN_CLIP_LEN_BEATS as f32), 4096.0)),
            ..super::super::scrub_style(&app.theme)
        };
        let cells_for_len = cells.to_vec();
        let resp = ui.scrubable_number_at(
            ("inspector_launch_len", "cells"),
            c.value(y),
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
        // (`super::super::push_scrub_bracket` が inspector 共通の 1 本)。
        super::super::push_scrub_bracket(
            ui,
            app,
            InspectorScrubField::Launch(("inspector_launch_len", "cells")),
            resp.dragging || resp.editing_text,
        );
    }
    y += FORM_ROW_PITCH;

    // ---- 動作: ループ / レガート ----
    // 値が割れているときは OFF 表示にして、押すと「全部 ON」 に揃える
    // (トグルは 3 状態を持てないので、揃える方向を 1 つに決める)。
    form::label(ui, app, "inspector_launch_play_label", vocab::FIELD_LAUNCH_BEHAVIOR, c, y);
    let looping = app.launch_fold(cells, |s| s.looping).unwrap_or(false);
    toggle(ui, app, "inspector_launch_loop", "ループ", c.value_part(y, 2, 0), looping, cells, LaunchEdit::Looping(!looping));
    let legato = app.launch_fold(cells, |s| s.legato).unwrap_or(false);
    toggle(ui, app, "inspector_launch_legato", "レガート", c.value_part(y, 2, 1), legato, cells, LaunchEdit::Legato(!legato));
    y += FORM_ROW_PITCH;

    // ---- フォローアクション (セル) ----
    let cell_follow = app.launch_fold(cells, |s| s.follow.clone()).unwrap_or_default();
    let y = follow_heading(app, ui, c, w, y, "inspector_follow_head");
    draw_follow_rows(app, ui, c, y, &cell_follow, FollowTarget::Cells, cells)
}

/// 小見出し「フォローアクション」(セルと列で同じ形)。
fn follow_heading(app: &AppData, ui: &mut Ui<'_, AppData>, c: &FormCols, w: f32, y: f32, id: &'static str) -> f32 {
    super::super::band::sub_heading(app, ui, id, c.label_x, w, y + 2.0, vocab::SUB_FOLLOW, None)
}

/// フォローアクションの 5 行: 発動 ([有効][クリップ終端 / 指定時間]) / 回数 or 間隔 / アクション A /
/// A の確率 / アクション B。セルと列で同じ形なので `target` だけを変えて 2 回呼ぶ。
fn draw_follow_rows(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    c: &FormCols,
    mut y: f32,
    follow: &FollowAction,
    target: FollowTarget,
    cells: &[LauncherCellKey],
) -> f32 {
    let tag = target.tag();

    // ---- 発動: [有効] [クリップ終端 ▾ / 指定時間 ▾] ----
    form::label(ui, app, ("inspector_follow_trigger_label", tag), vocab::FIELD_FOLLOW_TRIGGER, c, y);
    let enabled = follow.enabled;
    ui.toggle_button_at(
        ("inspector_follow_enabled", tag),
        vocab::TOGGLE_FOLLOW_ON,
        c.value_part(y, 2, 0),
        enabled,
        &super::super::toggle_audio_style(&app.theme),
        follow_edit_cb(target, cells, app, LaunchEdit::FollowEnabled(!enabled)),
    );
    let link_idx = usize::from(!follow.linked);
    if let Some(i) = ui.dropdown_with_font(("inspector_follow_link", tag), c.value_part(y, 2, 1), LINK_LABELS, link_idx, HALF_FONT) {
        push_follow_edit(ui, target, cells, app, LaunchEdit::FollowLinked(i == 0));
    }
    y += FORM_ROW_PITCH;

    // ---- 回数 (クリップ終端: ループ回数) / 間隔 (指定時間: 拍)。同じ行を使い回すので、切り替えても下は動かない ----
    if follow.linked {
        form::label(ui, app, ("inspector_follow_mult_label", tag), vocab::FIELD_FOLLOW_COUNT, c, y);
        let spec = NumSpec { value: f64::from(follow.multiplier), default: 1.0, fmt: ScrubableNumberFormat::Integer, range: (1.0, 64.0), sensitivity: 0.05 };
        num_field(ui, ("inspector_follow_mult", tag), c.value(y), &spec, app, target, cells, |v| {
            LaunchEdit::FollowMultiplier(v.round().clamp(1.0, 64.0) as u8)
        });
    } else {
        form::label(ui, app, ("inspector_follow_time_label", tag), vocab::FIELD_FOLLOW_INTERVAL, c, y);
        let spec = NumSpec { value: follow.time_beats, default: 4.0, fmt: ScrubableNumberFormat::Decimal(2), range: (0.0625, 512.0), sensitivity: 0.02 };
        num_field(ui, ("inspector_follow_time", tag), c.value(y), &spec, app, target, cells, LaunchEdit::FollowTimeBeats);
    }
    y += FORM_ROW_PITCH;

    // ---- アクション A / A の確率 / アクション B (B の確率は 100 − A なので欄は 1 つ) ----
    y = draw_follow_action_row(app, ui, c, y, follow, target, cells, true);
    form::label(ui, app, ("inspector_follow_chance_label", tag), vocab::FIELD_FOLLOW_CHANCE_A, c, y);
    let spec = NumSpec { value: f64::from(follow.chance_a), default: 100.0, fmt: ScrubableNumberFormat::Integer, range: (0.0, 100.0), sensitivity: 0.3 };
    num_field(ui, ("inspector_follow_chance", tag), c.value(y), &spec, app, target, cells, |v| {
        LaunchEdit::FollowChanceA(v.round().clamp(0.0, 100.0) as u8)
    });
    y += FORM_ROW_PITCH;
    draw_follow_action_row(app, ui, c, y, follow, target, cells, false)
}

/// アクション A / B の 1 行 (`アクション A [次のセル ▾]`)。「ジャンプ」なら同じ行の右半分に飛び先の列
/// (`[ジャンプ ▾][列 ▾]`、列が 1 つも無ければ出さない)。
#[allow(clippy::too_many_arguments)]
fn draw_follow_action_row(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    c: &FormCols,
    y: f32,
    follow: &FollowAction,
    target: FollowTarget,
    cells: &[LauncherCellKey],
    is_a: bool,
) -> f32 {
    let tag = match (target, is_a) {
        (FollowTarget::Cells, true) => "cell_a",
        (FollowTarget::Cells, false) => "cell_b",
        (FollowTarget::Scenes, true) => "scene_a",
        (FollowTarget::Scenes, false) => "scene_b",
    };
    let label = if is_a { vocab::FIELD_FOLLOW_A } else { vocab::FIELD_FOLLOW_B };
    form::label(ui, app, ("inspector_follow_kind_label", tag), label, c, y);
    let kind = if is_a { follow.a } else { follow.b };
    let scenes = &app.cur.song_doc.song().scenes;
    let jump = match kind {
        FollowActionKind::Jump { scene_id } if !scenes.is_empty() => Some(scene_id),
        _ => None,
    };
    let edit = move |next: FollowActionKind| if is_a { LaunchEdit::FollowA(next) } else { LaunchEdit::FollowB(next) };
    // 「ジャンプ」を選んだ直後の飛び先は先頭の列 (右の dropdown で変える)。
    let first_scene = scenes.first().map_or(0, |s| s.id);
    let kind_rect = if jump.is_some() { c.value_part(y, 2, 0) } else { c.value(y) };
    if let Some(i) = ui.dropdown_with_font(("inspector_follow_kind", tag), kind_rect, FOLLOW_LABELS, follow_index(kind), HALF_FONT) {
        push_follow_edit(ui, target, cells, app, edit(follow_from_index(i, first_scene)));
    }
    if let Some(scene_id) = jump {
        let names: Vec<String> = scenes.iter().enumerate().map(|(i, s)| s.display_name(i)).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let sel = scenes.iter().position(|s| s.id == scene_id).unwrap_or(usize::MAX);
        if let Some(i) = ui.dropdown_with_font(("inspector_follow_jump", tag), c.value_part(y, 2, 1), &refs, sel, HALF_FONT)
            && let Some(s) = scenes.get(i)
        {
            push_follow_edit(ui, target, cells, app, edit(FollowActionKind::Jump { scene_id: s.id }));
        }
    }
    y + FORM_ROW_PITCH
}

/// MIDI 割り当て (計画書 §3.5、詳細で最初は畳む — r.md #158 仕様 7、セルと列で同じ鍵)。
/// パッドで撃てないランチャーは半分の機能しかないので、
/// **セル / 列 / 行停止 / 行→アレンジ / 全停止 / 全→アレンジ** を全部出す。
#[allow(clippy::too_many_arguments)]
fn draw_midi_rows(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    c: &FormCols,
    w: f32,
    y: f32,
    cells: &[LauncherCellKey],
    scene_ids: &[u32],
) -> f32 {
    // 学習の宛先は「いま選んでいるセル」 (anchor = 末尾)。トラック行のセルだけが
    // パッドの宛先になる (パッドは行 × 列の格子で、オートメーションレーンには
    // 対応する物理位置が無い)。
    let anchor = cells.iter().rev().find_map(|c| match c {
        LauncherCellKey::Track(k) => Some(*k),
        LauncherCellKey::Lane(_) => None,
    });
    // 列の宛先は **セル経由と直接選択の両方**から取る ([`scenes_of`] が同じ 1 本)。
    // セル経由だけにしていると、列見出しを選んだときに「列」 の Learn が
    // 押せず (宛先 `None`)、**セルを 1 つも持たない列は永久に MIDI へ割り当て
    // られない** — 列を直接選べるようにした意味が半分無くなる。
    // 行の宛先 (セル / 行を止める / 行→アレンジ) は行が要るので、列だけの選択では
    // `None` のまま (押しても何も起きないのが正しい)。
    let scene_id = anchor
        .map(LauncherCellKey::Track)
        .and_then(|c| app.scene_of_cell(c))
        .or_else(|| scene_ids.last().copied());
    let learning = app.cur.launcher.learn_target;
    let label = vocab::launch_midi_detail(app.launcher_bindings().len(), learning.is_some());
    super::super::band::details(app, ui, c.label_x, w, y, InspectorDetail::LaunchMidi, &label, |ui, y| {
        draw_midi_buttons(app, ui, c, y, anchor, scene_id, learning)
    })
}

/// MIDI 割り当ての中身: 鳴らす [セル][列] / 止める [行][全部] / アレンジへ戻す [行][全部] / 消去。
fn draw_midi_buttons(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    c: &FormCols,
    mut y: f32,
    anchor: Option<ClipKey>,
    scene_id: Option<u32>,
    learning: Option<common::model::BindingTarget>,
) -> f32 {
    use common::model::BindingTarget as T;
    // (欄名, [(widget id, ボタン, 宛先)])。宛先が `None` = いま決められない (セル未選択 / 列が無い) ので
    // 押しても何もしない。
    type Learn<'a> = (&'a str, &'a str, Option<T>);
    let rows: [(&str, [Learn<'_>; 2]); 3] = [
        (
            vocab::FIELD_LEARN_LAUNCH,
            [
                ("learn_cell", vocab::LEARN_CELL, anchor.zip(scene_id).map(|(k, s)| T::LaunchCell { track_id: k.track_id, scene_id: s })),
                ("learn_scene", vocab::LEARN_SCENE, scene_id.map(|s| T::LaunchScene { scene_id: s })),
            ],
        ),
        (
            vocab::FIELD_LEARN_STOP,
            [
                ("learn_stop_row", vocab::LEARN_ROW, anchor.map(|k| T::StopLauncherRow { track_id: k.track_id })),
                ("learn_stop_all", vocab::LEARN_ALL, Some(T::StopAllLauncherRows)),
            ],
        ),
        (
            vocab::FIELD_LEARN_TO_ARRANGER,
            [
                ("learn_row_arr", vocab::LEARN_ROW, anchor.map(|k| T::SwitchRowToArranger { track_id: k.track_id })),
                ("learn_all_arr", vocab::LEARN_ALL, Some(T::SwitchAllToArranger)),
            ],
        ),
    ];
    for (label, buttons) in rows {
        form::label(ui, app, ("inspector_launch_learn_label", label), label, c, y);
        for (k, (id, text, target)) in buttons.into_iter().enumerate() {
            learn_toggle(app, ui, id, text, c.value_part(y, 2, k), target, learning);
        }
        y += FORM_ROW_PITCH;
    }

    form::label(ui, app, "inspector_launch_clear_label", vocab::FIELD_LEARN_CLEAR, c, y);
    ui.button_at("inspector_launch_clear_binds", vocab::CLEAR_BINDINGS, c.value(y), || {
        Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::Launcher(LauncherEvent::ClearBindings));
        })
    });
    y + FORM_ROW_PITCH
}

/// MIDI 割り当ての学習のトグル 1 つ: 押すと `target` の学習を始め、学習中 (点灯) なら取り消す。`target` が
/// `None` (セル未選択 / 列が無い) なら押しても何もしない。
fn learn_toggle(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    id: &str,
    text: &str,
    rect: Rect,
    target: Option<common::model::BindingTarget>,
    learning: Option<common::model::BindingTarget>,
) {
    let active = learning == target && target.is_some();
    let style = super::super::toggle_audio_style(&app.theme);
    ui.toggle_button_at(("inspector_launch_learn", id), text, rect, active, &style, move |_| {
        Edit::mutate(move |app: &mut AppData| {
            let ev = match (active, target) {
                (true, _) => LauncherEvent::CancelLearn,
                (false, Some(t)) => LauncherEvent::StartLearn(t),
                (false, None) => return,
            };
            app.handle_event(AppEvent::Launcher(ev));
        })
    });
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
    let style = super::super::toggle_audio_style(&app.theme);
    ui.toggle_button_at(id, label, rect, on, &style, move |_| {
        launch_edit_for(FollowTarget::Cells, &cells, &[], edit)
    });
}

/// 数値欄 1 つの中身 (値 / dblclick で戻す値 / 書式 / 範囲 / 送り)。
struct NumSpec {
    value: f64,
    default: f64,
    fmt: ScrubableNumberFormat,
    range: (f64, f64),
    sensitivity: f32,
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
    spec: &NumSpec,
    app: &AppData,
    target: FollowTarget,
    cells: &[LauncherCellKey],
    make: impl Fn(f64) -> LaunchEdit + Clone + Send + Sync + 'static,
) {
    let style = ScrubableNumberStyle {
        sensitivity: spec.sensitivity,
        range: Some(spec.range),
        ..super::super::scrub_style(&app.theme)
    };
    let cells = cells.to_vec();
    let scene_ids = scenes_of(app, &cells);
    let resp = ui.scrubable_number_at(
        id,
        rect,
        spec.value,
        spec.default,
        spec.fmt,
        &style,
        move |v| launch_edit_for(target, &cells, &scene_ids, make(v)),
        None,
        None,
    );
    // key は欄の widget id そのもの (`tag` に宛先が入っているので列側と衝突しない)。
    super::super::push_scrub_bracket(
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
/// 表示 (`draw_cell_launch` / `draw_scene_launch`) と書き込み (`push_follow_edit`) が **同じこの 1 本**を
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
