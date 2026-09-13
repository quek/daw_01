//! Phase 7 B5 (`docs/plan_scale.html`): Scale & Root / スナップ / 仮想鍵盤 / ノートの script API。
//!
//! production GUI mode の Transport bar / piano_roll toggle と同じ AppEvent を
//! 発火する。 JS smoke test (`tests/scripts/scale_smoke.js`) で
//! scale_changes の編集 / snap apply / quantize / fold mode の挙動を verify。

use boa_engine::{Context, JsArgs, JsNativeError, JsResult, JsValue};

use super::with_host;
use crate::app::{AppEvent, ClipKey};

fn scale_from_name(name: &str) -> Option<common::scale::Scale> {
    use common::scale::Scale;
    match name {
        "Major" => Some(Scale::Major),
        "NaturalMinor" | "Minor" => Some(Scale::NaturalMinor),
        "Dorian" => Some(Scale::Dorian),
        "Phrygian" => Some(Scale::Phrygian),
        "Lydian" => Some(Scale::Lydian),
        "Mixolydian" => Some(Scale::Mixolydian),
        "Locrian" => Some(Scale::Locrian),
        "HarmonicMinor" => Some(Scale::HarmonicMinor),
        "MelodicMinor" => Some(Scale::MelodicMinor),
        "MajorPentatonic" => Some(Scale::MajorPentatonic),
        "MinorPentatonic" => Some(Scale::MinorPentatonic),
        "Blues" => Some(Scale::Blues),
        "WholeTone" => Some(Scale::WholeTone),
        "Diminished" => Some(Scale::Diminished),
        "HalfWholeDim" => Some(Scale::HalfWholeDim),
        "Chromatic" => Some(Scale::Chromatic),
        "HarmonicMajor" => Some(Scale::HarmonicMajor),
        "DoubleHarmonic" => Some(Scale::DoubleHarmonic),
        "LydianDominant" => Some(Scale::LydianDominant),
        "PhrygianDominant" => Some(Scale::PhrygianDominant),
        "HungarianMinor" => Some(Scale::HungarianMinor),
        "Japanese" => Some(Scale::Japanese),
        _ => None,
    }
}

pub(super) fn daw_set_scale_at_playhead(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let root = args.get_or_undefined(0).to_number(ctx)? as u8;
    let scale_name = args
        .get_or_undefined(1)
        .to_string(ctx)?
        .to_std_string()
        .map_err(|e| JsNativeError::typ().with_message(format!("scale name not utf8: {e}")))?;
    let scale = scale_from_name(&scale_name).ok_or_else(|| {
        JsNativeError::typ().with_message(format!("unknown scale name: {scale_name}"))
    })?;
    with_host(|host| {
        host.app
            .handle_event(AppEvent::SetScaleAtPlayhead { root, scale });
    });
    Ok(JsValue::undefined())
}

pub(super) fn daw_clear_scale_changes(
    _this: &JsValue,
    _args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    with_host(|host| {
        host.app.handle_event(AppEvent::ClearScaleChanges);
    });
    Ok(JsValue::undefined())
}

pub(super) fn daw_toggle_snap_on_draw(
    _this: &JsValue,
    _args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    with_host(|host| {
        host.app.handle_event(AppEvent::ToggleSnapOnDraw);
    });
    Ok(JsValue::undefined())
}

pub(super) fn daw_toggle_snap_live_input(
    _this: &JsValue,
    _args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    with_host(|host| {
        host.app.handle_event(AppEvent::ToggleSnapLiveInput);
    });
    Ok(JsValue::undefined())
}

/// r.md #113: 仮想鍵盤 window の開閉 (= `K`)。
pub(super) fn daw_toggle_virtual_keyboard(
    _this: &JsValue,
    _args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    with_host(|host| {
        host.app.handle_event(AppEvent::VirtualKeyboard(
            crate::event_virtual_keyboard::VirtualKeyboardEvent::Toggle,
        ));
    });
    Ok(JsValue::undefined())
}

/// r.md #113: `virtualKeyboardKey(key, pressed, shift?)` — key grab が横取りした PC キーを
/// 1 件模す (`key` は US 配列の刻印 1 文字: `"Z"` / `"2"` / `"["` 等)。 window が開いて
/// いるかは問わない (headless では runner の宣言経路が無いので handler を直接叩く)。
pub(super) fn daw_virtual_keyboard_key(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let key = args
        .get_or_undefined(0)
        .to_string(ctx)?
        .to_std_string()
        .map_err(|e| JsNativeError::typ().with_message(format!("key not utf8: {e}")))?;
    let pressed = args.get_or_undefined(1).to_boolean();
    let shift = args.get_or_undefined(2).to_boolean();
    let mut chars = key.chars();
    let physical = match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_digit() => {
            daw_ui_platform::PhysicalKey::Digit(u8::try_from(c as u32 - '0' as u32).unwrap_or(0))
        }
        (Some(c), None) => daw_ui_platform::PhysicalKey::Char(c.to_ascii_uppercase()),
        _ => {
            return Err(JsNativeError::typ()
                .with_message(format!("virtualKeyboardKey: 1 文字のキー刻印を渡す (got {key:?})"))
                .into());
        }
    };
    with_host(|host| {
        host.app.handle_event(AppEvent::VirtualKeyboard(
            crate::event_virtual_keyboard::VirtualKeyboardEvent::Key(daw_ui_core::GrabbedKey {
                key: physical,
                pressed,
                repeat: false,
                shift,
            }),
        ));
    });
    Ok(JsValue::undefined())
}

pub(super) fn daw_toggle_fold_to_scale(
    _this: &JsValue,
    _args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    with_host(|host| {
        host.app.handle_event(AppEvent::ToggleFoldToScale);
    });
    Ok(JsValue::undefined())
}

pub(super) fn daw_quantize_pitches_to_scale(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    use crate::app::QuantizePitchTarget;
    let target_name = args
        .get_or_undefined(0)
        .to_string(ctx)?
        .to_std_string()
        .map_err(|e| JsNativeError::typ().with_message(format!("target name not utf8: {e}")))?;
    let target = match target_name.as_str() {
        "selected_notes" => QuantizePitchTarget::SelectedNotes,
        "selected_clip_all_notes" => QuantizePitchTarget::SelectedClipAllNotes,
        other => {
            return Err(JsNativeError::typ()
                .with_message(format!("unknown quantize target: {other}"))
                .into());
        }
    };
    with_host(|host| {
        host.app
            .handle_event(AppEvent::QuantizePitchesToScale(target));
    });
    Ok(JsValue::undefined())
}

pub(super) fn daw_add_note(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // 住所は **安定 id** (`Track.id` / `Clip.id`)。index ではない。
    let track_id = args.get_or_undefined(0).to_number(ctx)? as u32;
    let clip_id = args.get_or_undefined(1).to_number(ctx)? as u32;
    let start_beat = args.get_or_undefined(2).to_number(ctx)?;
    let duration = args.get_or_undefined(3).to_number(ctx)?;
    let pitch = args.get_or_undefined(4).to_number(ctx)? as u8;
    with_host(|host| {
        host.app.handle_event(AppEvent::AddNote {
            key: ClipKey { track_id, clip_id },
            start_beat,
            duration,
            pitch,
        });
    });
    Ok(JsValue::undefined())
}

pub(super) fn daw_set_note_positions_json(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let json = args
        .get_or_undefined(0)
        .to_string(ctx)?
        .to_std_string()
        .map_err(|e| JsNativeError::typ().with_message(format!("entries JSON not utf8: {e}")))?;
    let entries: Vec<(u32, f64, u8)> = serde_json::from_str(&json).map_err(|e| {
        JsNativeError::typ().with_message(format!("entries JSON decode: {e}"))
    })?;
    with_host(|host| {
        host.app.handle_event(AppEvent::SetNotePositions(entries));
    });
    Ok(JsValue::undefined())
}
