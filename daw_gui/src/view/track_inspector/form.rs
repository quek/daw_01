//! r.md #158 仕様 9: 帯の中の欄 (ラベル左・値右、`docs/plan_rmd_158_inspector.md` §2.3)。
//!
//! 1 行 = `[ラベル列][値][A 列]`。ラベル列の幅は 1 つ ([`FORM_LABEL_W`])、A 列 ([`FORM_AUTO_W`]) は
//! 自動化の「A」が無い欄でも空けて、値の右端を全部の欄で揃える。行の高さと送りも 1 組
//! ([`FORM_ROW_H`] / [`FORM_ROW_PITCH`])。値の部品は既存の `scrub_field` / `dropdown` / `toggle_button_at` /
//! `text_input_at` をそのまま使う (bespoke な widget は作らない)。

use daw_ui_core::{Edit, ScrubableNumberFormat, ScrubableNumberStyle, Ui};
use daw_ui_renderer::Rect;

use common::model::FadeCurve;

use super::layout::{FORM_AUTO_W, FORM_GAP, FORM_LABEL_W, FORM_ROW_H, FORM_ROW_PITCH, LABEL_FONT};
use crate::app::{AppData, AppEvent, ClipKey, DiscreteClipEdit, InspectorScrubField};

/// 欄の列の x。帯の中なら `cols(area.x + PAD, area.w - 2 * PAD)`、Rack の行の直下 (Par / SC / 出力) なら
/// [`cols_nested`]。
#[derive(Debug, Clone, Copy)]
pub(super) struct FormCols {
    pub label_x: f32,
    /// ラベルを切る幅 (ラベル列の左端から値の手前の隙間まで)。
    pub label_w: f32,
    pub value_x: f32,
    pub value_w: f32,
    pub auto_x: f32,
}

impl FormCols {
    /// 値の欄 (行 `y`)。
    pub fn value(&self, y: f32) -> Rect {
        Rect { x: self.value_x, y, w: self.value_w, h: FORM_ROW_H }
    }

    /// 値の欄を `n` 等分した `i` 番目 (トグルを横に並べる)。
    pub fn value_part(&self, y: f32, n: usize, i: usize) -> Rect {
        let n = n.max(1) as f32;
        let w = (self.value_w - FORM_GAP * (n - 1.0)) / n;
        Rect { x: self.value_x + (w + FORM_GAP) * i as f32, y, w, h: FORM_ROW_H }
    }

    /// A 列 (行 `y`)。
    pub fn auto(&self, y: f32) -> Rect {
        Rect { x: self.auto_x, y, w: FORM_AUTO_W, h: FORM_ROW_H }
    }
}

/// 左端 `left`・幅 `width` の欄の列。
pub(super) fn cols(left: f32, width: f32) -> FormCols {
    cols_nested(left, width, left)
}

/// 入れ子の欄 (Rack の行の直下に開く Par / SC / 出力) の値の欄がこれより狭くならない、ラベル列の下限。
const NESTED_LABEL_MIN_W: f32 = 48.0;

/// 帯の中に入れ子で置く欄の列 (Rack の行の直下。左端 `left` は行の名前の左端で、深さとともに右へずれる)。
/// 値の欄の左端は帯の欄と同じ x (`origin_left` = 帯の欄の左端から [`FORM_LABEL_W`]) に揃え、ラベルは
/// 入れ子の左端から始めて、そのぶん狭いラベル列で切る — インスペクタのどこでも値の列が 1 本に並ぶ。
/// 深い入れ子でラベル列が [`NESTED_LABEL_MIN_W`] を割るときだけ値の欄を右へ逃がす。右端 (A 列) は `left + width`。
pub(super) fn cols_nested(left: f32, width: f32, origin_left: f32) -> FormCols {
    let value_x = (origin_left + FORM_LABEL_W + FORM_GAP).max(left + NESTED_LABEL_MIN_W + FORM_GAP);
    let auto_x = left + width - FORM_AUTO_W;
    FormCols {
        label_x: left,
        label_w: value_x - FORM_GAP - left,
        value_x,
        value_w: (auto_x - FORM_GAP - value_x).max(1.0),
        auto_x,
    }
}

/// 欄のラベル (11px text_dim、ラベル列の幅で切る)。
pub(super) fn label(ui: &mut Ui<'_, AppData>, app: &AppData, id: impl std::hash::Hash, text: &str, c: &FormCols, y: f32) {
    let h = LABEL_FONT * 1.2;
    let rect = Rect { x: c.label_x, y: y + (FORM_ROW_H - h) * 0.5, w: c.label_w, h };
    ui.label_at_clipped(id, text, rect, LABEL_FONT, app.theme.core.text_dim);
}

/// 値の欄に置く読むだけの文字 (一覧の取得中など)。
pub(super) fn value_text(ui: &mut Ui<'_, AppData>, app: &AppData, id: impl std::hash::Hash, text: &str, c: &FormCols, y: f32) {
    let h = LABEL_FONT * 1.2;
    let rect = Rect { x: c.value_x + 2.0, y: y + (FORM_ROW_H - h) * 0.5, w: c.value_w - 2.0, h };
    ui.label_at_clipped(id, text, rect, LABEL_FONT, app.theme.core.text);
}

/// 数値欄 1 行 (ラベル + scrub_field)。`mod_owner` = 変調の持ち主のトラック (代表クリップの居るトラック、
/// `clip_band::ClipFields::mod_owner`)。返り値 = 次の行の y。
#[allow(clippy::too_many_arguments)]
pub(super) fn scrub_row(
    ui: &mut Ui<'_, AppData>,
    app: &AppData,
    c: &FormCols,
    y: f32,
    id: &'static str,
    text: &str,
    value: Option<f64>,
    default: f64,
    fmt: ScrubableNumberFormat,
    style: &ScrubableNumberStyle,
    key: InspectorScrubField,
    mod_owner: u32,
    make_event: impl Fn(ClipKey, f64) -> AppEvent + Clone + Send + Sync + 'static,
) -> f32 {
    label(ui, app, (id, "label"), text, c, y);
    let field = super::ScrubFieldSpec { value, default, fmt, style, key, mod_owner };
    super::scrub_field(ui, app, (id, "input"), c.value(y), &field, make_event);
    y + FORM_ROW_PITCH
}

/// 自動化の「A」トグル (A 列)。`on` = この欄のレーンがある、押すと足す / 外す。
pub(super) fn auto_toggle(
    ui: &mut Ui<'_, AppData>,
    app: &AppData,
    id: impl std::hash::Hash,
    c: &FormCols,
    y: f32,
    on: bool,
    edit: impl FnOnce(bool) -> Edit<AppData>,
) {
    ui.toggle_button_at(id, "A", c.auto(y), on, &super::toggle_automate_style(&app.theme), edit);
}

/// 選んでいるクリップ全部へ 1 つの離散編集を送る `Edit` (ミュート / 伸縮 / カーブ / 揃え…)。
pub(super) fn broadcast(edit: DiscreteClipEdit) -> Edit<AppData> {
    Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::BroadcastDiscreteClipEdit { targets: app.inspector_target_refs(), edit });
    })
}

/// フェードのカーブ (表記は 1 本。音声 / 画像 / 字幕で共通)。
pub(super) const FADE_CURVE_LABELS: &[&str] = &["Linear", "Exp", "S-Curve"];

fn fade_curve_to_index(c: FadeCurve) -> usize {
    match c {
        FadeCurve::Linear => 0,
        FadeCurve::Exponential => 1,
        FadeCurve::SCurve => 2,
    }
}

fn fade_curve_from_index(i: usize) -> FadeCurve {
    match i {
        1 => FadeCurve::Exponential,
        2 => FadeCurve::SCurve,
        _ => FadeCurve::Linear,
    }
}

/// フェード 1 行の中身 (音声 / 画像 / 字幕で同じ形)。
pub(super) struct FadeRowSpec {
    /// widget id の頭。
    pub id: &'static str,
    pub label: &'static str,
    /// 長さ (拍、割れていれば `None`)。
    pub beats: Option<f64>,
    /// 長さの上限 (ひと続きの長さ)。
    pub max_beats: f64,
    /// カーブ (割れていれば `None` = 空の dropdown)。
    pub curve: Option<FadeCurve>,
    pub key: InspectorScrubField,
    /// 長さの欄の変調の持ち主 ([`scrub_row`] と同じ)。
    pub owner: u32,
    /// 長さの編集。
    pub set_beats: fn(ClipKey, f64) -> AppEvent,
    /// カーブの編集。
    pub set_curve: fn(FadeCurve) -> DiscreteClipEdit,
}

/// カーブの dropdown の幅。
const FADE_CURVE_W: f32 = 84.0;

/// フェード 1 行: `[ラベル][長さ (拍)][カーブ ▾]`。返り値 = 次の行の y。
pub(super) fn fade_row(ui: &mut Ui<'_, AppData>, app: &AppData, c: &FormCols, y: f32, spec: &FadeRowSpec) -> f32 {
    label(ui, app, (spec.id, "label"), spec.label, c, y);
    let len_w = (c.value_w - FADE_CURVE_W - FORM_GAP).max(40.0);
    let style = ScrubableNumberStyle {
        sensitivity: 0.01,
        range: Some((0.0, spec.max_beats.max(0.0))),
        ..super::scrub_style(&app.theme)
    };
    let field = super::ScrubFieldSpec {
        value: spec.beats,
        default: 0.0,
        fmt: ScrubableNumberFormat::Decimal(3),
        style: &style,
        key: spec.key,
        mod_owner: spec.owner,
    };
    super::scrub_field(ui, app, (spec.id, "input"), Rect { w: len_w, ..c.value(y) }, &field, spec.set_beats);
    let curve_rect = Rect { x: c.value_x + len_w + FORM_GAP, w: c.value_w - len_w - FORM_GAP, ..c.value(y) };
    let selected = spec.curve.map_or(usize::MAX, fade_curve_to_index);
    if let Some(picked) = ui.dropdown((spec.id, "curve"), curve_rect, FADE_CURVE_LABELS, selected) {
        ui.push_edit(broadcast((spec.set_curve)(fade_curve_from_index(picked))));
    }
    y + FORM_ROW_PITCH
}
