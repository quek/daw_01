//! r.md #156: オーディオデバイスの script API (`docs/plan_audio_io.md`)。
//!
//! 切り替えは GUI と同じ `AudioDeviceEvent::SetConfig` を `AppData::handle_event` に通す (= daw_audio を
//! 起こし直す。戻ったときには新しいデバイスが開いている)。窓の無い script は隔離した設定 root で動くので、
//! ここで替えた設定はユーザーの `app_config.json` に残らない (`--script --gui` は実機の設定を書き換える)。

use std::time::Duration;

use boa_engine::{Context, JsArgs, JsResult, JsValue};
use common::audio_device::{
    AudioChannelInfo, AudioDeviceConfig, AudioDeviceEntry, AudioDeviceList, AudioDeviceRef, AudioDriverKind,
    AudioInputDevice, ChannelRange,
};
use common::protocol::AudioEvent;
use serde_json::{Value, json};

use super::{js_native, with_app, with_host};
use crate::app::AppEvent;
use crate::bootstrap::ChildEvent;
use crate::event_audio_device::AudioDeviceEvent;

/// 列挙の返事を待つ上限 (ASIO はレジストリを読むだけ、OS 標準は endpoint を数えるだけなので普通は数十 ms)。
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// 一覧のうち、名前を引く先。
type Pick = fn(&AudioDeviceList) -> &[AudioDeviceEntry];

/// `daw.audioDevices()` → `{systemOutputs, systemInputs, asioDrivers}` (どれも `[{id, name, isDefault}]`)。
/// この PC のデバイスを列挙し直して返す (設定画面の「再スキャン」と同じ口。デバイスは開かない)。
pub(super) fn daw_audio_devices(_this: &JsValue, _args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let list = list_devices()?;
    let rows = |entries: &[AudioDeviceEntry]| -> Value {
        entries
            .iter()
            .map(|e| json!({ "id": e.device.id, "name": e.device.name, "isDefault": e.is_default }))
            .collect()
    };
    let json = json!({
        "systemOutputs": rows(&list.system_outputs),
        "systemInputs": rows(&list.system_inputs),
        "asioDrivers": rows(&list.asio_drivers),
    });
    JsValue::from_json(&json, ctx)
}

fn list_devices() -> JsResult<AudioDeviceList> {
    with_app(|app, _side, _io| app.handle_event(AppEvent::AudioDevice(AudioDeviceEvent::RefreshDevices)));
    with_host(|h| {
        h.pump_until(|msg| matches!(msg, ChildEvent::Audio(AudioEvent::AudioDevices(_))), LIST_TIMEOUT)
    })
    .map_err(|e| js_native(format!("audioDevices: {e}")))?;
    with_app(|app, _side, _io| app.audio_device.devices.clone())
        .ok_or_else(|| js_native("audioDevices: デバイスの一覧が届きませんでした"))
}

/// `daw.setAudioDevice(spec)` — 開くデバイスを替える。**戻ったときには開き直しが済んでいて**、開いたデバイスの
/// 情報 (`daw.audioDeviceInfo()` と同じ形) を返す。今の設定と同じなら開き直さない。
///
/// `spec` の項目 (どれも省略できる。省略 = デバイスの既定):
/// - `driver`: `"system"` (既定) | `"asio"` | `"synthetic"`
/// - `device`: デバイスの名前。system = 出力デバイス (省略 = OS の既定) / asio = ドライバー /
///   synthetic = `"ramp"` | `"loopback"`
/// - `id`: 名前を一覧から引かずに id を直接渡す (この PC に無いデバイスを指す検査用)
/// - `input`: system の入力デバイス。`"none"` (既定) | `"default"` | 名前
/// - `sampleRate` / `bufferFrames` / `outputFirstChannel`: 数値
/// - `inputCount` (と `inputFirst`): 開く入力 ch の範囲。`inputCount: 0` = 入力を使わない
pub(super) fn daw_set_audio_device(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let spec = args.get_or_undefined(0).to_json(ctx)?.unwrap_or(Value::Null);
    let config = config_from_spec(&spec)?;
    with_app(move |app, _side, _io| {
        app.handle_event(AppEvent::AudioDevice(AudioDeviceEvent::SetConfig(config)));
    });
    daw_audio_device_info(this, &[], ctx)
}

/// 文字列の欄。無ければ `None`。文字列でない値は黙って既定へ落とさず、エラーにする。
fn text_field(spec: &Value, key: &str) -> JsResult<Option<String>> {
    match spec.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(js_native(format!("setAudioDevice: {key} = {other} は文字列ではありません"))),
    }
}

/// 数値の欄。無ければ `None`。JS の数値は整数でも小数の型で届くことがある (`Number("44100")`) ので、値が整数なら
/// 受ける。整数として読めない値 (文字列・小数・負・範囲外) は黙って既定へ落とさず、エラーにする。
fn uint_field<T: TryFrom<u64>>(spec: &Value, key: &str) -> JsResult<Option<T>> {
    let Some(value) = spec.get(key).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let whole = |f: &f64| f.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(f);
    value
        .as_u64()
        .or_else(|| value.as_f64().filter(whole).map(|f| f as u64))
        .and_then(|n| T::try_from(n).ok())
        .map(Some)
        .ok_or_else(|| js_native(format!("setAudioDevice: {key} = {value} は範囲内の整数ではありません")))
}

fn config_from_spec(spec: &Value) -> JsResult<AudioDeviceConfig> {
    let text = |key: &str| text_field(spec, key);
    let driver = match text("driver")?.as_deref() {
        None | Some("system") => AudioDriverKind::System,
        Some("asio") => AudioDriverKind::Asio,
        Some("synthetic") => AudioDriverKind::Synthetic,
        Some(other) => {
            return Err(js_native(format!(
                "setAudioDevice: driver \"{other}\" は system / asio / synthetic のどれでもありません"
            )));
        }
    };
    let mut config = AudioDeviceConfig { driver, ..Default::default() };
    match driver {
        AudioDriverKind::System => {
            config.system_output = resolve(|l| &l.system_outputs, text("id")?, text("device")?)?;
            config.system_input = match text("input")?.as_deref() {
                None | Some("none") => AudioInputDevice::None,
                Some("default") => AudioInputDevice::Default,
                Some(name) => AudioInputDevice::Device(find(|l| &l.system_inputs, name)?),
            };
        }
        AudioDriverKind::Asio => {
            config.asio_driver = Some(
                resolve(|l| &l.asio_drivers, text("id")?, text("device")?)?
                    .ok_or_else(|| js_native("setAudioDevice: asio は device (ドライバーの名前) が要ります"))?,
            );
        }
        AudioDriverKind::Synthetic => config.synthetic_device = text("device")?,
    }
    config.sample_rate = uint_field(spec, "sampleRate")?;
    config.buffer_frames = uint_field(spec, "bufferFrames")?;
    config.output_first_channel = uint_field(spec, "outputFirstChannel")?;
    let input_first: Option<u16> = uint_field(spec, "inputFirst")?;
    config.input_channels = uint_field::<u16>(spec, "inputCount")?
        .map(|count| ChannelRange { first: input_first.unwrap_or(0), count });
    Ok(config)
}

/// `id` があればそのまま、無ければ `name` を一覧から引く。どちらも無ければ `None` (= デバイスの既定)。
fn resolve(pick: Pick, id: Option<String>, name: Option<String>) -> JsResult<Option<AudioDeviceRef>> {
    match (id, name) {
        (Some(id), name) => Ok(Some(AudioDeviceRef { name: name.unwrap_or_else(|| id.clone()), id })),
        (None, Some(name)) => find(pick, &name).map(Some),
        (None, None) => Ok(None),
    }
}

fn find(pick: Pick, name: &str) -> JsResult<AudioDeviceRef> {
    let list = list_devices()?;
    let entries = pick(&list);
    entries.iter().find(|e| e.device.name == name).map(|e| e.device.clone()).ok_or_else(|| {
        let names: Vec<&str> = entries.iter().map(|e| e.device.name.as_str()).collect();
        js_native(format!("setAudioDevice: デバイス「{name}」がありません (ある: {})", names.join(" / ")))
    })
}

/// `daw.audioDeviceInfo()` — いま開いているデバイス (`AppData::audio_device`。設定画面が読むのと同じ値)。
/// daw_audio と繋がっていなければ `null`。
///
/// `{driver, outputName, inputName, sampleRate, bufferFrames, sampleRates, bufferSizes, outputChannels,
/// inputChannels, outputFirstChannel, activeInputs: {first, count}, inputLatencyFrames, outputLatencyFrames,
/// hasControlPanel, fallbackReason}` (ch は `[{index, name}]`、無い値は `null`)。
pub(super) fn daw_audio_device_info(_this: &JsValue, _args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(info) = with_app(|app, _side, _io| app.audio_device.info.clone()) else {
        return Ok(JsValue::null());
    };
    let channels = |list: &[AudioChannelInfo]| -> Value {
        list.iter().map(|c| json!({ "index": c.index, "name": c.name })).collect()
    };
    let json = json!({
        "driver": match info.driver {
            AudioDriverKind::System => "system",
            AudioDriverKind::Asio => "asio",
            AudioDriverKind::Synthetic => "synthetic",
        },
        "outputName": info.output_name,
        "inputName": info.input_name,
        "sampleRate": info.sample_rate,
        "bufferFrames": info.buffer_frames,
        "sampleRates": info.sample_rates,
        "bufferSizes": info.buffer_sizes,
        "outputChannels": channels(&info.output_channels),
        "inputChannels": channels(&info.input_channels),
        "outputFirstChannel": info.output_first_channel,
        "activeInputs": { "first": info.active_inputs.first, "count": info.active_inputs.count },
        "inputLatencyFrames": info.input_latency_frames,
        "outputLatencyFrames": info.output_latency_frames,
        "hasControlPanel": info.has_control_panel,
        "fallbackReason": info.fallback_reason,
    });
    JsValue::from_json(&json, ctx)
}

/// `daw.audioInputStats()` → `{droppedFrames, filledFrames, starveCount}`。デバイスの入力を描画へ渡す途中で
/// 捨てた / 無音で埋めたフレーム数の累計と、デバイスが飢えた回数 (`MetricsBridge`。engine が起きてからの累計)。
pub(super) fn daw_audio_input_stats(_this: &JsValue, _args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let json = with_host(|h| {
        let m = &h.io.metrics;
        json!({
            "droppedFrames": m.input_dropped(),
            "filledFrames": m.input_filled(),
            "starveCount": m.starve_count(),
        })
    });
    JsValue::from_json(&json, ctx)
}
