//! 立ち絵グループの 2D affine + opacity (`docs/plan_tachie_group_transform.md` §5.5)。
//!
//! `device_panel/mod.rs` が順に呼ぶセクションの 1 つ (contract は同 mod の doc)。
use super::super::*;
use super::PanelCtx;

pub(super) fn draw_group_transform(app: &AppData, ui: &mut Ui<'_, AppData>, ctx: PanelCtx) -> f32 {
    let mut y = ctx.y;
    let device_id = ctx.device_id;
    // ---- Group Transform section (`docs/plan_tachie_group_transform.md` §5.5) --
    // cursor track が visual group のとき、立ち絵全体の 2D affine + opacity を
    // 数値編集 + per-param「A」automate トグルで expose。image inspector と同
    // idiom (ラベル左・値右の欄 + A 列)。group は表示 clip を持たないので clip 選択ではなく track 選択が
    // トリガ（§5.5）。純 audio バスには出ない（§5.6 group_has_visual_content）。見出しは描かない (行の名前と同じ)。
    if let Some(summary) = app.inspector_group_transform_summary(device_id) {
        let c = ctx.cols();
        let track_id = summary.track_id;
        let gt = summary.transform;
        // 変調の持ち主 (このグループのトラック) はパネルにつき 1 回だけ解決する。
        let owner = crate::view::native_device::ParamOwner::resolve(app.cur.song_doc.song(), track_id);

        for param in crate::app::GROUP_PARAMS {
            use common::model::GroupTransformParam as G;
            let idx = crate::app::group_param_index(param);
            // param 別の (現値, default, 書式, range, sensitivity[units/px], label)。
            // Rotation は degree 表示 / 入力（model は radians）。
            #[allow(clippy::type_complexity)]
            let (value, default, fmt, range, sens, label): (
                f64,
                f64,
                ScrubableNumberFormat,
                Option<(f64, f64)>,
                f32,
                &str,
            ) = match param {
                G::X => (gt.x.into(), 0.0, ScrubableNumberFormat::Decimal(3), None, 0.004, vocab::FIELD_X),
                G::Y => (gt.y.into(), 0.0, ScrubableNumberFormat::Decimal(3), None, 0.004, vocab::FIELD_Y),
                G::Rotation => (
                    gt.rotation_radians.to_degrees().into(),
                    0.0,
                    ScrubableNumberFormat::Decimal(1),
                    // 度域 range。modulation の色帯/live tick は range が要る (gui_01 overlay
                    // は range なしだと枠強調のみ)。handler は従来どおり -π..π wrap。
                    Some((-180.0, 180.0)),
                    1.0,
                    vocab::FIELD_ROTATION,
                ),
                G::ScaleX => (
                    gt.scale_x.into(),
                    1.0,
                    ScrubableNumberFormat::Decimal(3),
                    Some((0.1, 10.0)),
                    0.01,
                    vocab::FIELD_SCALE_X,
                ),
                G::ScaleY => (
                    gt.scale_y.into(),
                    1.0,
                    ScrubableNumberFormat::Decimal(3),
                    Some((0.1, 10.0)),
                    0.01,
                    vocab::FIELD_SCALE_Y,
                ),
                G::AnchorX => (
                    gt.anchor_x.into(),
                    0.5,
                    ScrubableNumberFormat::Decimal(3),
                    Some((0.0, 1.0)),
                    0.004,
                    vocab::FIELD_ANCHOR_X,
                ),
                G::AnchorY => (
                    gt.anchor_y.into(),
                    0.5,
                    ScrubableNumberFormat::Decimal(3),
                    Some((0.0, 1.0)),
                    0.004,
                    vocab::FIELD_ANCHOR_Y,
                ),
                G::Opacity => (
                    gt.opacity.into(),
                    1.0,
                    ScrubableNumberFormat::Decimal(3),
                    Some((0.0, 1.0)),
                    0.004,
                    vocab::FIELD_OPACITY,
                ),
            };
            form::label(ui, app, (device_id, param, "group_label"), label, &c, y);
            let style =
                ScrubableNumberStyle { sensitivity: sens, range, ..scrub_style(&app.theme) };
            // per-control modulation (docs/plan_modulation_routing_redesign.md §6):
            // 立ち絵を音でドラッグ変調する Bitwig 流。全 8 param を対象 (Rotation は
            // deg↔rad、ScaleX/Y は log space を `build_mod` が到達値ベースで吸収)。
            let g_target = AutomationTarget::GroupTransform(param);
            let g_domain = if matches!(param, G::Rotation) {
                mod_widget::PLAIN_ROTATION
            } else {
                mod_widget::PLAIN_IDENT
            };
            let g_mod_build = owner.map(|o| build_mod(app, g_target.clone(), value, g_domain, o));
            let g_modulation = g_mod_build.as_ref().map(|m| m.modulation());
            let resp = ui.scrubable_number_at(
                (device_id, param, "group_scrub"),
                c.value(y),
                value,
                default,
                fmt,
                &style,
                move |v| set_group_field_edit(track_id, param, v),
                None,
                g_modulation,
            );
            // modulation depth ドラッグの falling edge で host 再同期 (audio target の
            // depth 反映用。visual group transform は compose が即読みするので視覚は即時)。
            mod_widget::push_mod_depth_bracket(
                ui,
                app,
                crate::app::ParamSurface::Rack,
                track_id,
                &g_target,
                resp.mod_dragging,
            );
            // drag / text 編集を undo 1 step に bracket
            // (`view::scrub_gesture` が寿命ごと持つ 1 本、所有者は Transform device の Par)。
            crate::view::scrub_gesture::push(
                ui,
                app,
                crate::app::ScrubGesture::GroupTransform { device_id, param },
                resp.dragging || resp.editing_text,
            );
            let auto_on = summary.automated[idx];
            form::auto_toggle(ui, app, (device_id, param, "group_auto"), &c, y, auto_on, move |_| {
                toggle_group_automation_edit(param, auto_on)
            });
            y += layout::FORM_ROW_PITCH;
        }
    }
    y
}

/// group transform の 1 param 値変更 `Edit`。
/// **Rotation だけ degree 入力 → radians** (表示単位と保存単位が違うのはここだけ)。
#[allow(clippy::cast_possible_truncation)]
fn set_group_field_edit(
    track_id: u32,
    param: common::model::GroupTransformParam,
    v: f64,
) -> Edit<AppData> {
    let value = if matches!(param, common::model::GroupTransformParam::Rotation) {
        (v as f32).to_radians()
    } else {
        v as f32
    };
    Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::SetGroupTransformField { track_id, param, value });
    })
}

/// 「A」ボタン: この param の group automation lane を足す / 外す。
fn toggle_group_automation_edit(
    param: common::model::GroupTransformParam,
    auto_on: bool,
) -> Edit<AppData> {
    Edit::mutate(move |app: &mut AppData| {
        app.handle_event(if auto_on {
            AppEvent::RemoveGroupAutomationLane { param }
        } else {
            AppEvent::AddGroupAutomationLane { param }
        });
    })
}
