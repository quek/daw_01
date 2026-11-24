//! r.md #161: クリップランチャーの script API。フォローアクションの全体 on/off と、engine が publish している
//! 行の走行状態 (フォローアクションで移った先はここにしか出ない — `Song` には書かれない)。

use boa_engine::{Context, JsArgs, JsResult, JsString, JsValue};

use super::{js_native, with_app};
use crate::app::AppEvent;
use crate::event_launcher::LauncherEvent;

/// `daw.appSetFollowActionsEnabled(on)` — トランスポートの `Follow` ボタンと同じ
/// `LauncherEvent::SetFollowActionsEnabled`。`on` は真偽値だけ受ける。
pub(super) fn daw_app_set_follow_actions_enabled(
    _this: &JsValue,
    args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    let on = args
        .get_or_undefined(0)
        .as_boolean()
        .ok_or_else(|| js_native("appSetFollowActionsEnabled: 引数は true / false で渡してください"))?;
    with_app(move |app, _side, _io| {
        app.handle_event(AppEvent::Launcher(LauncherEvent::SetFollowActionsEnabled(on)));
    });
    Ok(JsValue::undefined())
}

/// `daw.launcherStateJson()` → `{ followActionsEnabled, hasFollowActions, rows: [{ trackId, laneId, state,
/// playingClipId, launchBeat }] }`。`followActionsEnabled` は GUI の表示値、`rows` は engine の telemetry 面
/// (GUI の poller と同じ面。`state` は `LAUNCHER_STATE_*` = 0 アレンジ / 1 再生中 / 2 停止)。
pub(super) fn daw_launcher_state_json(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let json = with_app(|app, _side, io| {
        let mut rows = Vec::new();
        if let Some(slot) = io.bridge.find_project(app.pk())
            && let Ok(plane) = common::audio_bridge::TelemetryPlane::open(io.bridge.os_id(), slot.plane_id())
        {
            plane.launcher_row_snapshots(&mut rows);
        }
        let rows: Vec<serde_json::Value> = rows
            .iter()
            .map(|(key, snap)| {
                serde_json::json!({
                    "trackId": key >> 32,
                    "laneId": key & 0xFFFF_FFFF,
                    "state": snap.state,
                    "playingClipId": snap.playing_clip_id,
                    "launchBeat": snap.launch_beat,
                })
            })
            .collect();
        serde_json::json!({
            "followActionsEnabled": app.follow_actions_enabled(),
            "hasFollowActions": app.cur.song_doc.song().has_follow_actions(),
            "rows": rows,
        })
    });
    Ok(JsValue::from(JsString::from(json.to_string().as_str())))
}
