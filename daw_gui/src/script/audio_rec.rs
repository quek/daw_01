//! r.md #156: オーディオ入力の録音の script API。
//!
//! 編集と録音の開始・終了は GUI と同じ `AppEvent` を `AppData::handle_event` に通す。headless には
//! テレメトリの poller が居ないので、engine の transport を GUI に観測させる 1 手 (`daw.appTick`) を
//! 持つ (GUI では poller が 33ms ごとに同じ event を送っている)。

use std::time::{Duration, Instant};

use boa_engine::value::TryFromJs;
use boa_engine::{Context, JsArgs, JsResult, JsString, JsValue};
use common::model::{MonitorMode, TrackInput};

use super::{arg_to_string, js_native, with_app, with_host};
use crate::app::AppEvent;
use crate::event_audio_rec::AudioRecEvent;

/// `daw.setTrackInput(trackId, kind, channel)` — インスペクタの「入力」と同じ `AudioRecEvent::SetTrackInput`。
/// `kind` = `"midi"` / `"mono"` / `"stereo"`、`channel` = デバイスの入力 ch 番号 (0 始まり。ステレオは L)。
pub(super) fn daw_set_track_input(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let track_id = u32::try_from_js(args.get_or_undefined(0), ctx)?;
    let kind = arg_to_string(args, 1, ctx)?;
    let channel = u16::try_from_js(args.get_or_undefined(2), ctx).unwrap_or(0);
    let input = match kind.as_str() {
        "midi" => TrackInput::Midi,
        "mono" => TrackInput::AudioMono { channel },
        "stereo" => TrackInput::AudioStereo { left: channel },
        other => return Err(js_native(format!("setTrackInput: unknown kind '{other}'"))),
    };
    with_app(move |app, _side, _io| {
        app.handle_event(AppEvent::AudioRec(AudioRecEvent::SetTrackInput { track_ids: vec![track_id], input }));
    });
    Ok(JsValue::undefined())
}

/// `daw.setTrackMonitor(trackId, mode)` — インスペクタの「モニター」と同じ `AudioRecEvent::SetTrackMonitor`。
/// `mode` = `"off"` / `"auto"` / `"on"`。
pub(super) fn daw_set_track_monitor(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let track_id = u32::try_from_js(args.get_or_undefined(0), ctx)?;
    let mode = arg_to_string(args, 1, ctx)?;
    let monitor = match mode.as_str() {
        "off" => MonitorMode::Off,
        "auto" => MonitorMode::Auto,
        "on" => MonitorMode::On,
        other => return Err(js_native(format!("setTrackMonitor: unknown mode '{other}'"))),
    };
    with_app(move |app, _side, _io| {
        app.handle_event(AppEvent::AudioRec(AudioRecEvent::SetTrackMonitor { track_ids: vec![track_id], monitor }));
    });
    Ok(JsValue::undefined())
}

/// `daw.toggleTrackArmed(trackId)` — トラックヘッダの R ボタンと同じ `AppEvent::ToggleTrackArmed`。
pub(super) fn daw_toggle_track_armed(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let track_id = u32::try_from_js(args.get_or_undefined(0), ctx)?;
    with_app(move |app, _side, _io| app.handle_event(AppEvent::ToggleTrackArmed(track_id)));
    Ok(JsValue::undefined())
}

/// `daw.appRecord()` — Rec ボタンと同じ `AppEvent::ToggleMidiRecording` (止まっていれば録音を始め、録音中なら
/// パンチアウト)。engine へ直接 `StartRecording` を送る `daw.startRecording()` と使い分ける。
pub(super) fn daw_app_record(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    with_app(|app, _side, _io| app.handle_event(AppEvent::ToggleMidiRecording));
    Ok(JsValue::undefined())
}

/// `daw.appUndo()` — Ctrl+Z と同じ `AppEvent::Undo`。
pub(super) fn daw_app_undo(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    with_app(|app, _side, _io| app.handle_event(AppEvent::Undo));
    Ok(JsValue::undefined())
}

/// `daw.appRedo()` — Ctrl+Shift+Z と同じ `AppEvent::Redo`。
pub(super) fn daw_app_redo(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    with_app(|app, _side, _io| app.handle_event(AppEvent::Redo));
    Ok(JsValue::undefined())
}

/// `daw.appSaveAs(path)` — 「名前を付けて保存」で保存先を決めた後と同じ `AppEvent::SaveAsResolved`
/// (`path` は `.daw` のフルパス)。未保存の置き場の素材は bundle へ移る。プラグインの無い曲なら戻った時点で
/// 保存が済んでいる (ある曲は state の収集を待つので、`sleepMs` で event を捌かせる)。
pub(super) fn daw_app_save_as(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let path = std::path::PathBuf::from(arg_to_string(args, 0, ctx)?);
    with_app(move |app, _side, _io| app.handle_event(AppEvent::SaveAsResolved { path: Some(path) }));
    Ok(JsValue::undefined())
}

/// `daw.appSave()` — Ctrl+S と同じ `AppEvent::Save` (保存先が決まっているプロジェクト用)。
pub(super) fn daw_app_save(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    with_app(|app, _side, _io| app.handle_event(AppEvent::Save));
    Ok(JsValue::undefined())
}

/// `daw.appTick()` — engine が publish している transport を GUI に観測させる (`AppEvent::Tick`。GUI では
/// テレメトリの poller が 33ms ごとに送る)。これで `transport.is_playing` / `recording.live` が更新され、
/// 止まったのを見て録音が閉じる。
pub(super) fn daw_app_tick(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    with_app(|app, _side, io| {
        let project = app.pk();
        let Some(b) = io.bridge.find_project(project) else { return };
        app.handle_event(AppEvent::Tick {
            project,
            samples: b.playhead_samples(),
            beat: b.playhead_beat(),
            preroll: b.preroll_remaining(),
            playing: b.playing(),
            recording_live: b.recording_live(),
        });
    });
    Ok(JsValue::undefined())
}

/// `daw.audioRecStateJson()` → `{ requested, live, pendingTakes, region: { trackIds, span } | null }`
/// (GUI が持つ録音の状態。`span` は帯の `[始まり, 終わり]` の拍か `null`)。
pub(super) fn daw_audio_rec_state_json(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let json = with_app(|app, _side, _io| {
        let rec = &app.cur.recording;
        let region = rec.audio_rec_region.as_ref().map(|r| {
            serde_json::json!({
                "trackIds": r.track_ids,
                "span": r.span.map(|(a, b)| [a, b]),
            })
        });
        serde_json::json!({
            "requested": rec.requested,
            "live": rec.live,
            "pendingTakes": rec.audio_takes.len(),
            "region": region,
            "status": app.ui_ephemeral.status_message,
        })
        .to_string()
    });
    Ok(JsString::from(json.as_str()).into())
}

/// `daw.trackPeak(trackId, ms)` → これから `ms` ミリ秒の間にそのトラックのメーターが出した最大のピーク
/// (線形。L / R の大きい方)。GUI のトラックメーターと同じテレメトリ面を読み、待つ間も IPC を捌く。
pub(super) fn daw_track_peak(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let track_id = u32::try_from_js(args.get_or_undefined(0), ctx)?;
    let ms = u64::try_from_js(args.get_or_undefined(1), ctx).unwrap_or(300);
    let project = with_app(|app, _side, _io| app.pk());
    let peak = with_host(|h| {
        let bridge = std::sync::Arc::clone(&h.io.bridge);
        let mut plane: Option<common::audio_bridge::TelemetryPlane> = None;
        let (mut peaks, mut voices) = (Vec::new(), Vec::new());
        let mut max = 0.0_f32;
        let deadline = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < deadline {
            h.drain_pending_for(Duration::from_millis(10));
            // 面は伸びると差し替わる (poller と同じ追い方)。
            let plane_id = bridge.find_project(project).map_or(0, |b| b.plane_id());
            if plane.as_ref().map_or(0, common::audio_bridge::TelemetryPlane::id) != plane_id {
                plane = (plane_id != 0)
                    .then(|| common::audio_bridge::TelemetryPlane::open(bridge.os_id(), plane_id).ok())
                    .flatten();
            }
            let Some(p) = plane.as_ref() else { continue };
            if p.read_tracks(&mut peaks, &mut voices) {
                max = peaks.iter().filter(|(id, ..)| *id == track_id).fold(max, |m, &(_, l, r)| m.max(l).max(r));
            }
        }
        max
    });
    Ok(JsValue::from(f64::from(peak)))
}
