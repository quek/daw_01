//! r.md #129: 内蔵 device (`Device::Native`) と master Limiter の script API
//! (`docs/plan_rack_native_devices.md` §10.19)。
//!
//! 編集は GUI と同じ `DeviceEvent` を `AppData::handle_event` に通す (自動 ON / 値 IPC / undo は
//! handler の唯一の口が持つ)。headless にはフレーム末が無いので、Song を変えたら
//! `flush_song_sync` で LoadSong を明示的に送る。GR と master の出力は engine が publish する
//! テレメトリ面 (GUI のポーラと同じ面) を直接読む。

use std::time::{Duration, Instant};

use boa_engine::value::TryFromJs;
use boa_engine::{Context, JsArgs, JsResult, JsString, JsValue};
use common::model::{CompMode, EqBand, NativeParamId, for_each_native};
use serde_json::Value;

use super::{arg_to_string, js_native, with_host};
use crate::app::AppEvent;
use crate::event_device::DeviceEvent;
use crate::event_native::NativeEdit;

/// `daw.nativeDevices(trackId)` → `[{id, kind, builtin, ordinal, bypassed, index}]` の JSON 文字列
/// (`trackId == MASTER_TRACK_ID` は master_fx_chain)。並びは信号順。
///
/// `index` はその device を含む **最上位の** device の位置 (Parallel の中の native は、それを包む
/// Parallel の位置)。`kind` は `"Comp" | "Eq" | "BusComp" | "ToneEq"`。
pub(super) fn daw_native_devices(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let track_id = u32::try_from_js(args.get_or_undefined(0), ctx)?;
    let json = with_host(|h| {
        let devices = h.app.cur.song_doc.song().fx_chain_by_track_id(track_id).unwrap_or(&[]);
        let mut rows: Vec<Value> = Vec::new();
        for (index, top) in devices.iter().enumerate() {
            for_each_native(std::slice::from_ref(top), &mut |n| {
                rows.push(serde_json::json!({
                    "id": n.id,
                    "kind": n.kind(),
                    "builtin": n.builtin,
                    "ordinal": n.ordinal,
                    "bypassed": n.bypassed,
                    "index": index,
                }));
            });
        }
        serde_json::to_string(&rows)
    })
    .map_err(|e| js_native(format!("nativeDevices: serialize: {e}")))?;
    Ok(JsString::from(json.as_str()).into())
}

/// `daw.nativeEdit(deviceId, editJson)` → `DeviceEvent::NativeEdit`。`editJson` は `NativeEdit` の
/// externally tagged 形:
///
/// - `{"Params": [[{"Comp": "Threshold"}, -30], [{"Eq": {"band": "Hmf", "param": "Gain"}}, 6]]}`
/// - `{"EqBandOn": {"band": "Hp", "on": true}}` / `{"EqBell": {"band": "Lf", "bell": true}}`
/// - `{"CompMode": "Leveler"}`
///
/// 住所 (`NativeParamId`) / バンド / モードの綴りは保存形 (serde) と同じ。
pub(super) fn daw_native_edit(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let device_id = u64::try_from_js(args.get_or_undefined(0), ctx)?;
    let json = arg_to_string(args, 1, ctx)?;
    let value: Value = serde_json::from_str(&json).map_err(|e| js_native(format!("nativeEdit: parse: {e}")))?;
    let edit = parse_native_edit(&value).map_err(|e| js_native(format!("nativeEdit: {e}")))?;
    with_host(|h| {
        h.app.handle_event(AppEvent::Device(DeviceEvent::NativeEdit { device_id, edit }));
        h.app.flush_song_sync();
    });
    Ok(JsValue::undefined())
}

fn parse_native_edit(value: &Value) -> Result<NativeEdit, String> {
    let Some((tag, body)) = value.as_object().filter(|m| m.len() == 1).and_then(|m| m.iter().next()) else {
        return Err(format!("1 キーのオブジェクトを渡す (got {value})"));
    };
    let field = |name: &str| body.get(name).cloned().ok_or_else(|| format!("{tag}: `{name}` が無い"));
    let flag = |name: &str| field(name)?.as_bool().ok_or_else(|| format!("{tag}: `{name}` は bool"));
    match tag.as_str() {
        "Params" => Ok(NativeEdit::Params(decode::<Vec<(NativeParamId, f32)>>(tag, body.clone())?)),
        "EqBandOn" => Ok(NativeEdit::EqBandOn { band: decode::<EqBand>(tag, field("band")?)?, on: flag("on")? }),
        "EqBell" => Ok(NativeEdit::EqBell { band: decode::<EqBand>(tag, field("band")?)?, bell: flag("bell")? }),
        "CompMode" => Ok(NativeEdit::CompMode(decode::<CompMode>(tag, body.clone())?)),
        other => Err(format!("未知の編集 `{other}`")),
    }
}

fn decode<T: serde::de::DeserializeOwned>(tag: &str, v: Value) -> Result<T, String> {
    serde_json::from_value(v).map_err(|e| format!("{tag}: {e}"))
}

/// `daw.nativeGainReduction(deviceId)` → いま engine が publish している GR の **減衰量** (dB, ≥ 0)。
/// 面に居ない (処理されていない / まだ publish されていない) device は 0。
pub(super) fn daw_native_gain_reduction(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let device_id = u64::try_from_js(args.get_or_undefined(0), ctx)?;
    let amount = with_host(|h| {
        let Some(slot) = h.bootstrap.bridge.find_project(h.app.pk()) else {
            return 0.0;
        };
        let mut plane = Vec::new();
        if !slot.read_native_meters(&mut plane) {
            return 0.0;
        }
        plane.iter().find(|(id, _)| *id == device_id).map_or(0.0, |&(_, gr_db)| (-gr_db).max(0.0))
    });
    Ok(JsValue::from(f64::from(amount)))
}

/// `daw.masterLimiterGainReduction()` → master Limiter の GR の減衰量 (dB, ≥ 0)。
pub(super) fn daw_master_limiter_gain_reduction(
    _this: &JsValue,
    _args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    let amount = with_host(|h| {
        h.bootstrap.bridge.find_project(h.app.pk()).map_or(0.0, |slot| (-slot.master_limiter_gr_db()).max(0.0))
    });
    Ok(JsValue::from(f64::from(amount)))
}

/// `daw.setDevicesBypassed(idsJson, bypassed)` → `DeviceEvent::SetDevicesBypassed` (Q / 右クリックと同じ口)。
pub(super) fn daw_set_devices_bypassed(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let ids_json = arg_to_string(args, 0, ctx)?;
    let device_ids: Vec<u64> =
        serde_json::from_str(&ids_json).map_err(|e| js_native(format!("setDevicesBypassed: parse ids: {e}")))?;
    let bypassed = args.get_or_undefined(1).to_boolean();
    with_host(|h| {
        h.app.handle_event(AppEvent::Device(DeviceEvent::SetDevicesBypassed { device_ids, bypassed }));
        h.app.flush_song_sync();
    });
    Ok(JsValue::undefined())
}

/// `daw.setScListen(deviceId | null)` → `DeviceEvent::SetScListen` (Listen ▶ と同じ口。bypass 中の
/// Comp を指すと有効化してから聴く)。
pub(super) fn daw_set_sc_listen(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let arg = args.get_or_undefined(0);
    let device_id = if arg.is_null() || arg.is_undefined() { None } else { Some(u64::try_from_js(arg, ctx)?) };
    with_host(|h| {
        h.app.handle_event(AppEvent::Device(DeviceEvent::SetScListen { device_id }));
        h.app.flush_song_sync();
    });
    Ok(JsValue::undefined())
}

/// `daw.masterPeakDbfs(ms)` → これから `ms` ミリ秒の間に master が出したサンプルピーク (dBFS)。
/// 無音なら `null`。GUI のマスターメーターと同じリング (`ScopeBridge`) を読み、待つ間も IPC を
/// 捌く (`sleepMs` と同じ)。書き出しではなく **再生中の音** を測る口 (SC Listen は書き出しに乗らない)。
pub(super) fn daw_master_peak_dbfs(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let ms = u64::try_from_js(args.get_or_undefined(0), ctx).unwrap_or(500);
    let peak = with_host(|h| {
        let scope = std::sync::Arc::clone(&h.bootstrap.scope);
        let mut reader = scope.reader();
        let mut frames: Vec<[f32; 2]> = Vec::new();
        let mut peak = 0.0_f32;
        let deadline = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < deadline {
            h.drain_pending_for(Duration::from_millis(20));
            frames.clear();
            reader.read(&scope, &mut frames);
            peak = frames.iter().flatten().fold(peak, |p, s| p.max(s.abs()));
        }
        peak
    });
    Ok(if peak > 0.0 { JsValue::from(f64::from(20.0 * peak.log10())) } else { JsValue::null() })
}
