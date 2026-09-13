//! 内蔵 device のつまみ (+ 数値欄)。Rack Par / Mixer 帯 / マスターパネルが同じ関数を呼ぶ (§10.4)。
//!
//! 旧 `strip_sections::strip_knob` と `master_strip_ui::master_knob` を 1 本にしたもの。値の住所は
//! オートメーション / 変調の target (`NativeParam{device_id, param}` / `MasterLimiter(Ceiling)`) と
//! 同じなので、ノブ専用の値配管を作らず既存の口をそのまま通す:
//!
//! | 役割 | 口 |
//! |---|---|
//! | 正規化 | `common::automation::{plain_to_norm, norm_to_plain}` |
//! | 表示値 (再生中はレーン値) | `AppData::live_native_param` / `live_lane_value` |
//! | 変調の帯と深さドラッグ | `view::modulation::build_mod` / `push_mod_depth_bracket` |
//! | undo 1 step + 録音 | `view::param_gesture::push_param_gesture` (1 面・1 param につき 1 回) |
//! | 数値の書式と値域 | `automation_value::automation_value_display` |
//! | 編集 (自動 ON + 値 IPC) | `DeviceEvent::NativeEdit` / `DeviceEvent::MasterLimiterEdit` |

use std::hash::Hash;

use common::automation::{norm_to_plain, plain_to_norm, target_range};
use common::model::{
    AutomationTarget, BusCompParam, EqParam, MASTER_TRACK_ID, MasterLimiterParam, NativeDevice, NativeParamId,
    ParamRange, RackPanelKey,
};
use daw_ui_core::widgets::knob::KNOB_UNITS_PER_PX;
use daw_ui_core::{Edit, KnobStyle, ScrubCurve, ScrubableNumberStyle, Ui};
use daw_ui_renderer::{Color, Rect, RectCommand};

use super::{ParamOwner, wid};
use crate::app::{AppData, AppEvent, ModControlDomain, ParamSurface};
use crate::automation_value::automation_value_display;
use crate::event_device::DeviceEvent;
use crate::event_native::{MasterLimiterEdit, NativeEdit};
use crate::handler::view_model::LiveParamScope;
use crate::view::modulation::{build_mod, push_mod_depth_bracket};
use crate::view::param_gesture::push_param_gesture;

/// 数値欄の font size (Q12 の Par の格子 = 52×16 に合わせる)。
const VALUE_FONT: f32 = 10.0;
/// 数値欄の左余白 (52px 幅に `-12.5 dB` が収まる詰め方)。
const VALUE_PAD_X: f32 = 2.0;
/// 沈めた (dimmed) つまみ / 数値欄に被せる面の色の不透明度。
const DIM_ALPHA: f32 = 0.55;

/// [`native_knob`] / [`native_knob_with_value`] の引数。
pub struct NativeKnobSpec<'a> {
    /// どの面が描くか (widget id とジェスチャー所有者の鍵)。
    pub surface: ParamSurface,
    /// lane / routing の持ち主 (device を持つトラック、master chain なら master)。
    pub owner: ParamOwner<'a>,
    /// 描画時点の device (`Song` の値)。表示値は `owner` のレーンを重ねて決める。
    pub device: &'a NativeDevice,
    /// `device` の種類に実在する住所。`On` / 種類違い / 実在しない組は何も描かない。
    pub param: NativeParamId,
    /// つまみの矩形。
    pub rect: Rect,
    /// つまみが載っている面の色 (可動範囲外のリングをくり抜く色、沈めるときの色)。
    pub surface_bg: Color,
    /// 回しても今は音が変わらないつまみ (`CompMode::overrides` / OFF のフィルタバンド)。
    pub dimmed: bool,
    /// 同じ面で同じ param を動かす別 widget (カーブ点 / ホイール Q) のドラッグ状態。
    /// ジェスチャーはつまみ・数値欄・これの OR で 1 本にする。
    pub external_drag: bool,
    /// フレームで 1 回だけ組む live 値の文脈。
    pub scope: &'a LiveParamScope,
}

/// つまみ 1 個 (+ 数値欄) の結果。
#[derive(Debug, Clone, Copy, Default)]
pub struct NativeKnobResponse {
    /// つまみか数値欄の上にカーソルがある。
    pub hovered: bool,
    /// つまみか数値欄をドラッグ中 (`external_drag` は含まない)。
    pub dragging: bool,
    /// いま描いた値 (plain)。ドラッグ中は widget の preview (model より 1 フレーム先行)。
    /// hover 読み出し (`"Thr -12.0dB"`) はこれを `automation_value_display` で書式化する。
    pub displayed_plain: f64,
}

/// 内蔵 device のつまみ (数値欄なし。Mixer 帯 / マスターパネル)。
pub fn native_knob(app: &AppData, ui: &mut Ui<'_, AppData>, spec: &NativeKnobSpec<'_>) -> NativeKnobResponse {
    match native_core(app, spec) {
        Some(core) => draw_param_knob(app, ui, core, None),
        None => NativeKnobResponse::default(),
    }
}

/// つまみ + その下の数値欄 (Q12: 数値は常に出る。ドラッグで変更 / クリックで数値入力 /
/// ダブルクリックで既定値)。ジェスチャーはつまみと数値欄で 1 本。
pub fn native_knob_with_value(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    spec: &NativeKnobSpec<'_>,
    value_rect: Rect,
) -> NativeKnobResponse {
    match native_core(app, spec) {
        Some(core) => draw_param_knob(app, ui, core, Some(value_rect)),
        None => NativeKnobResponse::default(),
    }
}

/// master のフェーダー後 Limiter の Ceiling つまみ (`value_rect` が `Some` なら数値欄も)。
pub fn limiter_knob(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    surface: ParamSurface,
    rect: Rect,
    surface_bg: Color,
    scope: &LiveParamScope,
    value_rect: Option<Rect>,
) -> NativeKnobResponse {
    let song = app.cur.song_doc.song();
    let param = MasterLimiterParam::Ceiling;
    let target = AutomationTarget::MasterLimiter(param);
    let plain = app.live_lane_value(scope, ParamOwner::master(song), &target, song.master_limiter.param(param));
    let core = KnobCore {
        surface,
        owner_id: MASTER_TRACK_ID,
        panel: RackPanelKey::MasterLimiter,
        key: param,
        target,
        plain,
        default_plain: param.default_plain(),
        rect,
        surface_bg,
        dimmed: false,
        external_drag: false,
        event: move |v| AppEvent::Device(DeviceEvent::MasterLimiterEdit(MasterLimiterEdit::Ceiling(v))),
    };
    draw_param_knob(app, ui, core, value_rect)
}

/// つまみ 1 個分の解決済みの記述 (native と Limiter で共有する)。
struct KnobCore<K, E> {
    surface: ParamSurface,
    owner_id: u32,
    panel: RackPanelKey,
    /// widget id の部品内の鍵 (`NativeParamId` / `MasterLimiterParam`)。
    key: K,
    target: AutomationTarget,
    /// 表示する plain 値 (live)。
    plain: f32,
    default_plain: f32,
    rect: Rect,
    surface_bg: Color,
    dimmed: bool,
    external_drag: bool,
    /// plain 値 → 編集イベント。
    event: E,
}

fn native_core(
    app: &AppData,
    spec: &NativeKnobSpec<'_>,
) -> Option<KnobCore<NativeParamId, impl Fn(f32) -> AppEvent + Copy + Send + 'static>> {
    let dev = spec.device;
    let param = spec.param;
    // On (値域 Toggle) はつまみにしない。種類違い / 実在しない組 (`Eq{Hp,Gain}` 等) も描かない。
    let default_plain = param.default_plain()?;
    dev.param(param)?;
    let device_id = dev.id;
    Some(KnobCore {
        surface: spec.surface,
        owner_id: spec.owner.id,
        panel: RackPanelKey::Device(device_id),
        key: param,
        target: AutomationTarget::NativeParam { device_id, param },
        plain: app.live_native_param(spec.scope, spec.owner, dev, param),
        default_plain,
        rect: spec.rect,
        surface_bg: spec.surface_bg,
        dimmed: spec.dimmed,
        external_drag: spec.external_drag,
        event: move |v| AppEvent::Device(DeviceEvent::NativeEdit { device_id, edit: NativeEdit::param(param, v) }),
    })
}

fn draw_param_knob<K, E>(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    core: KnobCore<K, E>,
    value_rect: Option<Rect>,
) -> NativeKnobResponse
where
    K: Hash + Copy,
    E: Fn(f32) -> AppEvent + Copy + Send + 'static,
{
    let target = &core.target;
    let norm = plain_to_norm(target, f64::from(core.plain));
    let default_norm = plain_to_norm(target, f64::from(core.default_plain));
    let m = build_mod(app, target.clone(), f64::from(norm), ModControlDomain::Norm, core.owner_id);
    let event = core.event;
    let knob = ui.knob_at(
        wid(core.surface, core.panel, "knob", core.key),
        core.rect,
        norm,
        default_norm,
        &knob_style(target, core.surface_bg),
        {
            let target = target.clone();
            move |v| dispatch(event(norm_to_plain(&target, v) as f32))
        },
        Some(m.modulation()),
    );
    if core.dimmed {
        dim(ui, core.rect, core.surface_bg, core.rect.w * 0.5);
    }
    // つまみの preview は model より 1 フレーム先行する (ドラッグ / ダブルクリック)。動いていなければ
    // 正規化の往復で丸めずに live 値そのものを使う。
    let knob_plain = if (knob.displayed_value - norm).abs() <= f32::EPSILON {
        f64::from(core.plain)
    } else {
        norm_to_plain(target, knob.displayed_value)
    };

    let field = value_rect.map(|r| {
        let desc = automation_value_display(target, None);
        let resp = ui.scrubable_number_at(
            wid(core.surface, core.panel, "value", core.key),
            r,
            knob_plain,
            f64::from(core.default_plain),
            desc.format,
            &value_style(app, target, desc.unit),
            // 表示レンジの SSoT (`AutomationValueDisplay`) を通してから plain へ落とす。数値入力の確定と
            // ダブルクリックは 1 イベントで完結する (それ自体が 1 undo step)。
            move |v| dispatch(event(desc.clamp_plain(v) as f32)),
            None,
            // 変調の帯と深さドラッグはつまみ 1 つに集約する (同じ変調を 2 か所で編集させない)。
            None,
        );
        if core.dimmed {
            dim(ui, r, core.surface_bg, 3.0);
        }
        resp
    });

    let field_dragging = field.as_ref().is_some_and(|f| f.dragging);
    push_param_gesture(
        ui,
        app,
        core.surface,
        core.owner_id,
        target.clone(),
        knob.dragging || field_dragging || core.external_drag,
    );
    push_mod_depth_bracket(ui, app, core.surface, core.owner_id, target, knob.mod_dragging);

    NativeKnobResponse {
        hovered: knob.hovered || field.as_ref().is_some_and(|f| f.hovered),
        dragging: knob.dragging || field_dragging,
        displayed_plain: field.as_ref().map_or(knob_plain, |f| f.displayed_value),
    }
}

/// 値弧の起点。ゲイン系 (EQ Gain / Tone EQ / Bus Comp Makeup) は 0 dB から左右へ伸ばして 0 dB に
/// 吸着させ、それ以外は左端 (7 時) から伸ばす。
fn knob_style(target: &AutomationTarget, surface_bg: Color) -> KnobStyle {
    let gain = matches!(
        target,
        AutomationTarget::NativeParam {
            param: NativeParamId::Eq { param: EqParam::Gain, .. }
                | NativeParamId::ToneEq(_)
                | NativeParamId::BusComp(BusCompParam::Makeup),
            ..
        }
    );
    // 0 dB は値域の中央とは限らない (Bus Comp Makeup は -5..+15 dB で 0.25)。
    let base = if gain {
        KnobStyle { arc_origin: plain_to_norm(target, 0.0), ..KnobStyle::BIPOLAR }
    } else {
        KnobStyle::UNIPOLAR
    };
    KnobStyle { surface: Some(surface_bg), ..base }
}

/// 数値欄の style。ドラッグの感度はつまみと同じ「値域全体を 250px」に揃える
/// (対数の値域は正規化領域で動かす)。
fn value_style(app: &AppData, target: &AutomationTarget, unit: &'static str) -> ScrubableNumberStyle {
    let p = &app.theme.core;
    let (range, curve, sensitivity) = match target_range(target, None) {
        // `LogWithOff` の OFF (0) は対数目盛に載らないので、欄のドラッグは lo..hi で動かす。
        // OFF はつまみを左へ回し切るか、ダブルクリック (既定値) で戻す。
        ParamRange::Log { lo, hi } | ParamRange::LogWithOff { lo, hi } => ((lo, hi), ScrubCurve::Log, KNOB_UNITS_PER_PX),
        r @ (ParamRange::Linear { .. } | ParamRange::Toggle | ParamRange::Stepped { .. }) => {
            let (lo, hi) = r.display_range();
            ((lo, hi), ScrubCurve::Linear, (hi - lo) as f32 * KNOB_UNITS_PER_PX)
        }
    };
    ScrubableNumberStyle {
        bg_color_hovered: p.control,
        bg_color_dragging: p.scrub_drag_bg,
        font_size: VALUE_FONT,
        pad_x: VALUE_PAD_X,
        sensitivity,
        range: Some(range),
        curve,
        unit,
        // ◉ (変調の待受) 中につまみを触ると深さのドラッグになる。数値欄は変調を持たないので、
        // 同じ操作で値そのものが動かないよう読み取り専用にする。
        read_only: app.cur.peph.armed_mod_source.is_some(),
        ..ScrubableNumberStyle::from_palette(p)
    }
}

/// 載っている面の色を半透明に被せて沈める (触れるまま = 触ると自動で ON になる入口として残す)。
fn dim(ui: &mut Ui<'_, AppData>, rect: Rect, surface_bg: Color, radius: f32) {
    ui.push_rect(RectCommand {
        rect,
        fill: surface_bg.with_alpha(DIM_ALPHA),
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [radius; 4],
        clip_rect: None,
    });
}

fn dispatch(event: AppEvent) -> Edit<AppData> {
    Edit::mutate(move |app: &mut AppData| {
        app.handle_event(event);
    })
}
