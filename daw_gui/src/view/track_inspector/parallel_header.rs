//! Parallel のヘッダ行 (`「 ▼ 名前 … [Split ▾] [out knob] [Match] [x]`) と、 r.md #112 の入力の
//! 配り方 (`Split`) の UI — ヘッダ行の dropdown と、 その直下の param 行 (`Frequency3` なら
//! `Low ◂[200 Hz]▸ Mid ◂[2.0 kHz]▸ High`)。 chain list 本体 (`chain_list.rs`) から Parallel 1 行ぶんの
//! 描画を切り出したもの (サイズ budget、 不変条件 9)。
//!
//! chain 行は共通のまま (帯域は chain の並び順)。 モードが増えたら dropdown の項目と
//! `draw_split_row` の分岐を足すだけで、 chain list 本体は変わらない。 数値欄は inspector 共通の
//! `scrubable_number` idiom (log 目盛 / 有効数字 3 桁 / undo bracket / automation gesture /
//! 変調 overlay)。

use daw_ui_core::{Edit, KnobStyle, ScrubCurve, ScrubableNumberFormat, ScrubableNumberStyle, Ui};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent, InspectorScrubField};
use crate::handler::parallel::ParallelMixerEdit;
use crate::view::modulation::{PLAIN_IDENT, build_mod, push_mod_depth_bracket};
use crate::view::param_gesture::push_param_gesture_edges;
use common::model::{AutomationTarget, SPLIT_FREQ_RANGE, Split, SplitEdge, TrackBuiltinParam};

use super::chain_list::{CHAIN_BTN_W, CHAIN_KNOB, ROW_H, draw_disclosure, draw_rename_input};
use super::{push_scrub_bracket, scrub_style, toggle_audio_style};

/// dropdown の項目 (順序 = [`split_index`] / [`split_from_index`])。
const SPLIT_LABELS: &[&str] = &["No split", "3 bands", "Mid/Side"];
/// ヘッダ行の dropdown 幅。
pub(super) const SPLIT_DROPDOWN_W: f32 = 74.0;

fn split_index(split: Split) -> usize {
    match split {
        Split::None => 0,
        Split::Frequency3 { .. } => 1,
        Split::MidSide => 2,
    }
}

/// dropdown の選択 → 新しい `Split`。 同じモードなら現在値をそのまま返す (周波数を失わない)。
fn split_from_index(idx: usize, current: Split) -> Split {
    match idx {
        1 => {
            if matches!(current, Split::Frequency3 { .. }) {
                current
            } else {
                Split::DEFAULT_FREQUENCY3
            }
        }
        2 => Split::MidSide,
        _ => Split::None,
    }
}

/// ヘッダ行の Split dropdown。 選択が変わったら `SetParallelSplit` (構造変更 = LoadSong)。
/// `popup_open` で門番しない — 選択が確定する frame は dropdown 自身の一覧が開いた popup なので、
/// 右クリックメニュー用の門番を掛けると pick が毎回捨てられる (SC パネルの dropdown と同じ)。
pub(super) fn draw_split_dropdown(
    ui: &mut Ui<'_, AppData>,
    i: usize,
    parallel_id: u64,
    split: Split,
    rect: Rect,
) {
    if let Some(idx) = ui.dropdown_with_font(("inspector_parallel_split", i), rect, SPLIT_LABELS, split_index(split), 10.0) {
        let next = split_from_index(idx, split);
        if next != split {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetParallelSplit { parallel_id, split: next });
            }));
        }
    }
}

/// Split の param 行 (ヘッダ行の直下、 `Split::has_params` のときだけ行がある)。
/// `Frequency3`: `Low [hz] Mid [hz] High`。
pub(super) fn draw_split_row(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    i: usize,
    parallel_id: u64,
    split: Split,
    content: Rect,
    popup_open: bool,
) {
    let Split::Frequency3 { low_hz, high_hz } = split else {
        return;
    };
    let p = &app.theme.core;
    let field_w = 58.0;
    let field_h = ROW_H - 8.0;
    let fy = content.y + (ROW_H - field_h) * 0.5;
    let label_y = content.y + 8.0;
    let mut x = content.x + 8.0;
    let label = |ui: &mut Ui<'_, AppData>, key: &'static str, text: &'static str, x: f32| {
        ui.label_at((key, i), text, x, label_y, 10.0, p.text_dim);
    };
    label(ui, "inspector_split_low_label", "Low", x);
    x += 26.0;
    draw_freq_field(app, ui, i, parallel_id, SplitEdge::LowMid, low_hz, Rect { x, y: fy, w: field_w, h: field_h }, popup_open);
    x += field_w + 6.0;
    label(ui, "inspector_split_mid_label", "Mid", x);
    x += 26.0;
    draw_freq_field(app, ui, i, parallel_id, SplitEdge::MidHigh, high_hz, Rect { x, y: fy, w: field_w, h: field_h }, popup_open);
    x += field_w + 6.0;
    label(ui, "inspector_split_high_label", "High", x);
}

/// クロスオーバー 1 つの数値欄。 値の住所は `TrackBuiltinParam::ParallelSplitFreq` そのもの
/// (= automation / 変調の target) なので、 knob と同じ gesture / overlay 経路がそのまま乗る。
#[allow(clippy::too_many_arguments)]
fn draw_freq_field(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    i: usize,
    parallel_id: u64,
    edge: SplitEdge,
    hz: f32,
    rect: Rect,
    popup_open: bool,
) {
    let Some(track_id) = app.cursor_track_id() else { return };
    let track = app.song_doc.song().track_by_id(track_id);
    let target = AutomationTarget::TrackBuiltin(TrackBuiltinParam::ParallelSplitFreq { parallel_id, edge });
    let live = track.map_or(hz, |t| app.live_param_value(t, &target, hz));
    let default = match edge {
        SplitEdge::LowMid => Split::DEFAULT_FREQS.0,
        SplitEdge::MidHigh => Split::DEFAULT_FREQS.1,
    };
    let style = ScrubableNumberStyle {
        range: Some(SPLIT_FREQ_RANGE.display_range()),
        curve: ScrubCurve::Log,
        sensitivity: 0.003,
        font_size: 10.0,
        ..scrub_style(&app.theme)
    };
    let m = build_mod(app, target.clone(), f64::from(live), PLAIN_IDENT, track_id);
    let was = app.recording.active_param_gestures.contains(&(track_id, target.clone()));
    let key = match edge {
        SplitEdge::LowMid => "inspector_split_low",
        SplitEdge::MidHigh => "inspector_split_high",
    };
    let resp = ui.scrubable_number_at(
        (key, i),
        rect,
        f64::from(live),
        f64::from(default),
        ScrubableNumberFormat::Significant { digits: 3 },
        &style,
        move |v| {
            Edit::mutate(move |app: &mut AppData| {
                if !popup_open {
                    app.handle_event(AppEvent::SetParallelMixer {
                        parallel_id,
                        edit: ParallelMixerEdit::SplitFreq { edge, hz: v as f32 },
                    });
                }
            })
        },
        None,
        Some(m.modulation()),
    );
    let name = match edge {
        SplitEdge::LowMid => "Split Low|Mid",
        SplitEdge::MidHigh => "Split Mid|High",
    };
    push_param_gesture_edges(ui, track_id, target.clone(), name, was, resp.dragging);
    push_scrub_bracket(
        ui,
        app,
        InspectorScrubField::ParallelSplit { parallel_id, edge },
        resp.dragging || resp.editing_text,
    );
    push_mod_depth_bracket(ui, app, track_id, &target, resp.mod_dragging);
}

/// Parallel ヘッダ行の表示情報 (`ChainRowKind::ParallelBegin` の中身)。
pub(super) struct ParallelHead<'a> {
    pub parallel_id: u64,
    pub name: &'a str,
    pub bypassed: bool,
    pub open: bool,
    pub out_gain: f32,
    pub gain_match: bool,
    pub split: Split,
}

/// Parallel ヘッダ行: 「 ▼ 名前 … [Split ▾] [out knob] [Match] [x]。 出力 trim と gain match は
/// 終了行ではなくここ (高さを増やさない)。 Split dropdown (r.md #112) の param 行は直下。
pub(super) fn draw_parallel_begin_row(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    i: usize,
    head: &ParallelHead<'_>,
    row: Rect,
    popup_open: bool,
) {
    let ParallelHead { parallel_id, name, bypassed, open, out_gain, gain_match, split } = *head;
    let p = &app.theme.core;
    let btn_x_w = 26.0;
    let by = row.y + 2.0;
    let mut right = row.x + row.w - btn_x_w;
    ui.button_at(
        ("inspector_parallel_remove", i),
        "x",
        Rect { x: right, y: by, w: btn_x_w, h: ROW_H - 4.0 },
        move || {
            Edit::mutate(move |app: &mut AppData| {
                if !popup_open {
                    app.handle_event(AppEvent::RemoveDevices { device_ids: vec![parallel_id] });
                }
            })
        },
    );
    // Match トグル + 出力 trim knob (右から)。
    let match_w = 44.0;
    right -= match_w + 2.0;
    ui.toggle_button_at(
        ("inspector_parallel_match", i),
        "Match",
        Rect { x: right, y: row.y + (ROW_H - CHAIN_BTN_W) * 0.5, w: match_w, h: CHAIN_BTN_W },
        gain_match,
        &toggle_audio_style(&app.theme),
        move |v| {
            Edit::mutate(move |app: &mut AppData| {
                if !popup_open {
                    app.handle_event(AppEvent::SetParallelMixer { parallel_id, edit: ParallelMixerEdit::GainMatch(v) });
                }
            })
        },
    );
    if let Some(track_id) = app.cursor_track_id() {
        let track = app.song_doc.song().track_by_id(track_id);
        let target = AutomationTarget::TrackBuiltin(TrackBuiltinParam::ParallelOutGain { parallel_id });
        let live = track.map_or(out_gain, |t| app.live_param_value(t, &target, out_gain));
        right -= CHAIN_KNOB + 4.0;
        let was = app.recording.active_param_gestures.contains(&(track_id, target.clone()));
        let resp = ui.knob_at(
            ("inspector_parallel_out", i),
            Rect { x: right, y: row.y + (ROW_H - CHAIN_KNOB) * 0.5, w: CHAIN_KNOB, h: CHAIN_KNOB },
            (live * 0.5).clamp(0.0, 1.0),
            0.5,
            &KnobStyle { surface: Some(p.panel_raised), ..KnobStyle::UNIPOLAR },
            move |v| {
                let gain = v * 2.0;
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetParallelMixer { parallel_id, edit: ParallelMixerEdit::OutGain(gain) });
                })
            },
            None,
        );
        push_param_gesture_edges(ui, track_id, target, "Parallel Out", was, resp.dragging);
    }
    // r.md #112: 入力の配り方 (No split / 3 bands …)。
    right -= SPLIT_DROPDOWN_W + 4.0;
    draw_split_dropdown(
        ui,
        i,
        parallel_id,
        split,
        Rect { x: right, y: row.y + (ROW_H - CHAIN_BTN_W) * 0.5, w: SPLIT_DROPDOWN_W, h: CHAIN_BTN_W },
    );
    // 括弧 `「` は chain_list の `draw_parallel_band` (chain の帯と同じ x / 幅で終了行の `L` まで繋ぐ)。
    // ここは 開閉 disclosure + 名前。
    draw_disclosure(ui, ("inspector_parallel_disclosure", i), parallel_id, open, row.x + 14.0, row, popup_open, p);
    let name_rect = Rect { x: row.x + 28.0, y: row.y + 3.0, w: (right - 6.0 - row.x - 28.0).max(1.0), h: ROW_H - 6.0 };
    if let Some((id, buf)) = &app.ui_ephemeral.renaming_chain
        && *id == parallel_id
    {
        draw_rename_input(app, ui, ("inspector_parallel_rename", i), name_rect, buf, move |app, text| {
            app.handle_event(AppEvent::RenameParallel { parallel_id, name: text });
        });
    } else {
        ui.label_at_clipped(
            ("inspector_parallel_name", i),
            name,
            Rect { x: name_rect.x, y: row.y + 8.0, w: name_rect.w, h: 11.0 * 1.2 },
            11.0,
            if bypassed { p.text_faint } else { p.text },
        );
    }
}
