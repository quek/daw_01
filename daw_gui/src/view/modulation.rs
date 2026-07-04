//! Per-control modulation widget glue (docs/plan_modulation_routing_redesign.md §6).
//!
//! Bitwig 流「コントロールを音でドラッグ変調」を全 `scrubable_number_at`
//! 呼び出しで共通化するヘルパ。`to_model` / `to_display` でコントロールの
//! *表示* 値ドメインと target の *model* 値ドメイン (= `automation::plain_to_norm`
//! の入力単位) を橋渡しするので、回転 (deg↔rad)・log scale (group scale)・
//! 恒等 (0..=1) を 1 つの経路で扱える。
//!
//! `inspector_mod_data` が到達値ベースで entries / live / armed を *表示* 単位で
//! 返し、ここでは gui_01 の `Modulation` 引数 (entries slice + on_mod_change
//! closure を borrow する) を組み立てる。

use daw_ui_core::{Edit, ModEdit, ModEntry, Modulation, Ui};
use daw_ui_renderer::Color;

use crate::app::{
    AppData, AppEvent, InspectorScrubField, ModControlDomain, TextNumField,
};
use common::model::{AutomationTarget, ImageBuiltinParam, TextBuiltinParam};

/// 表示値 == model 値 (= 恒等変換)。0..=1 正規化 field / px field 用。
pub(crate) fn ident(x: f64) -> f64 {
    x
}
/// 度入力 → radians (回転コントロールの表示→model)。
pub(crate) fn deg_to_rad(d: f64) -> f64 {
    d.to_radians()
}
/// radians → 度表示 (回転コントロールの model→表示)。
pub(crate) fn rad_to_deg(r: f64) -> f64 {
    r.to_degrees()
}

/// 恒等 (表示 == model plain) の Plain ドメイン。image/group pos・text・px field 用。
pub(crate) const PLAIN_IDENT: ModControlDomain =
    ModControlDomain::Plain { to_model: ident, to_display: ident };
/// 回転 (度表示 ↔ radians model) の Plain ドメイン。
pub(crate) const PLAIN_ROTATION: ModControlDomain =
    ModControlDomain::Plain { to_model: deg_to_rad, to_display: rad_to_deg };

/// 1 コントロール分の modulation 描画データ (owned)。`Modulation` は entries
/// slice と on_mod_change closure を borrow するので、呼び出し側が本構造体を
/// `scrubable_number_at` の呼び出しまで生存させ、[`ModBuild::modulation`] で借りる。
/// arm 中の depth-drag コールバック (表示 depth → routing 更新 `Edit`)。
type OnModChange = Box<dyn Fn(f64) -> Edit<AppData>>;

pub(crate) struct ModBuild {
    entries: Vec<ModEntry>,
    live: Option<f64>,
    /// `(source_color, current display depth, on_mod_change)` — arm 中のみ。
    edit: Option<(Color, f64, OnModChange)>,
}

impl ModBuild {
    /// gui_01 `scrubable_number_at` へ渡す `Modulation` 引数を借りる。
    pub(crate) fn modulation(&self) -> Modulation<'_, AppData> {
        Modulation {
            entries: &self.entries,
            live_value: self.live,
            edit: self.edit.as_ref().map(|(color, cur, f)| ModEdit {
                source_color: *color,
                current_depth: *cur,
                depth_range: None,
                depth_sensitivity: None,
                on_mod_change: f.as_ref(),
            }),
        }
    }
}

/// `track_id` 上の `target` のコントロール (表示 `display_base`、ドメイン `domain`)
/// の per-control modulation データを組み立てる。`track_id` は routing の帰属
/// トラック (inspector = cursor track、mixer strip = その strip のトラック)。
pub(crate) fn build_mod(
    app: &AppData,
    target: AutomationTarget,
    display_base: f64,
    domain: ModControlDomain,
    track_id: u32,
) -> ModBuild {
    let d = app.inspector_mod_data(&target, display_base, domain, track_id);
    let to_color = |c: [f32; 3]| Color { r: c[0], g: c[1], b: c[2], a: 1.0 };
    let entries = d
        .entries
        .iter()
        .map(|(c, depth)| ModEntry { color: to_color(*c), depth: *depth })
        .collect();
    let edit = d.armed.map(|(c, cur, sid)| {
        let track_id = d.track_id;
        let base_norm = d.base_norm;
        let edit_target = target.clone();
        let f: OnModChange = Box::new(move |new_depth_display: f64| {
            // 表示 depth → model 正規化 depth (affine / 回転 / log すべてここで逆変換)。
            let reach_model = domain.to_model(&edit_target, display_base + new_depth_display);
            let reach_norm =
                f64::from(common::automation::plain_to_norm(&edit_target, reach_model));
            #[allow(clippy::cast_possible_truncation)]
            let norm_depth = (reach_norm - base_norm).clamp(-1.0, 1.0) as f32;
            let t = edit_target.clone();
            Edit::mutate(move |app: &mut AppData| {
                // 未割当 (target, source) への初回ドラッグで routing を作る。
                app.handle_event(AppEvent::AddModRouting {
                    track_id,
                    target: t.clone(),
                    source_id: sid,
                });
                app.handle_event(AppEvent::SetModRoutingDepth {
                    track_id,
                    target: t,
                    source_id: sid,
                    depth: norm_depth,
                });
            })
        });
        (to_color(c), cur, f)
    });
    ModBuild { entries, live: d.live_display, edit }
}

/// 自コントロールの modulation depth ドラッグの立ち上がり / 立ち下がり edge を
/// 検知し、終了時に schedule を再同期する (`SetModRoutingDepth` は dirty mark
/// のみで、engine が新 depth を読むには `sync_song_to_plugin_host` = LoadSong が
/// 要る)。`app.ui_ephemeral.mod_depth_scrub_active` は **(track_id, target) を key** にするので、
/// 同時表示される多数の modulatable コントロール (mixer は同一 `Pan` target を全
/// strip に描く) が共有 flag を奪い合って毎フレーム偽の recompile を撃つことがない
/// (各コントロールは自分のドラッグ edge にだけ反応)。
pub(crate) fn push_mod_drag_resync(
    ui: &mut Ui<'_, AppData>,
    app: &AppData,
    track_id: u32,
    target: &AutomationTarget,
    mod_dragging: bool,
) {
    let is_active = app
        .ui_ephemeral.mod_depth_scrub_active
        .as_ref()
        .is_some_and(|(t, tgt)| *t == track_id && tgt == target);
    if mod_dragging == is_active {
        return;
    }
    let key = (track_id, target.clone());
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        if mod_dragging {
            app.ui_ephemeral.mod_depth_scrub_active = Some(key);
        } else {
            app.ui_ephemeral.mod_depth_scrub_active = None;
            app.sync_song_to_plugin_host();
        }
    }));
}

/// `scrub_field` の `scrub_key` から modulation target と表示↔model 変換を導く。
/// `None` = 変調対象でない field (clip-level gain / pan / pitch / fade 等)。
/// per-control 対象は **`cursor_modulatable_targets` と同集合**に厳密に揃える
/// (ラックで見えない/外せない routing を作らないため)。text は X/Y/W/H/Opacity/
/// Rotation/FontSize が対象 (B10 r.md #8: text_compose の resolve_norm が W/H にも
/// modulation を適用しているので image と対称化。 色・outline・shadow は引き続き対象外)。
pub(crate) fn scrub_field_mod(
    scrub_key: InspectorScrubField,
) -> Option<(AutomationTarget, ModControlDomain)> {
    use InspectorScrubField as F;
    // (target, rotation?) — 回転だけ deg↔rad、他は恒等。
    let (target, rotation): (AutomationTarget, bool) = match scrub_key {
        F::ImageX => (AutomationTarget::ImageBuiltin(ImageBuiltinParam::X), false),
        F::ImageY => (AutomationTarget::ImageBuiltin(ImageBuiltinParam::Y), false),
        F::ImageW => (AutomationTarget::ImageBuiltin(ImageBuiltinParam::W), false),
        F::ImageH => (AutomationTarget::ImageBuiltin(ImageBuiltinParam::H), false),
        F::ImageOpacity => (AutomationTarget::ImageBuiltin(ImageBuiltinParam::Opacity), false),
        F::ImageRotation => (AutomationTarget::ImageBuiltin(ImageBuiltinParam::Rotation), true),
        F::Text(TextNumField::X) => (AutomationTarget::TextBuiltin(TextBuiltinParam::X), false),
        F::Text(TextNumField::Y) => (AutomationTarget::TextBuiltin(TextBuiltinParam::Y), false),
        F::Text(TextNumField::W) => (AutomationTarget::TextBuiltin(TextBuiltinParam::W), false),
        F::Text(TextNumField::H) => (AutomationTarget::TextBuiltin(TextBuiltinParam::H), false),
        F::Text(TextNumField::Opacity) => {
            (AutomationTarget::TextBuiltin(TextBuiltinParam::Opacity), false)
        }
        F::Text(TextNumField::Rotation) => {
            (AutomationTarget::TextBuiltin(TextBuiltinParam::Rotation), true)
        }
        F::Text(TextNumField::FontSize) => {
            (AutomationTarget::TextBuiltin(TextBuiltinParam::FontSize), false)
        }
        _ => return None,
    };
    let domain = if rotation { PLAIN_ROTATION } else { PLAIN_IDENT };
    Some((target, domain))
}
