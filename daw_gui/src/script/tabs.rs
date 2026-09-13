//! `docs/plan_project_tabs.md` §5.4: プロジェクトタブの script API (`daw.newTab` / `closeTab` /
//! `switchTab(index)` / `tabsJson`) と engine の負荷 (`metricsJson`)。既存の API はすべて
//! アクティブなタブに効く。

use boa_engine::value::TryFromJs;
use boa_engine::{Context, JsArgs, JsResult, JsString, JsValue};

use super::{js_native, with_host};

/// `daw.newTab()` — 空の Untitled を新しいタブに開いてアクティブにする。
pub(super) fn daw_new_tab(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let ok = with_host(|h| {
        let ok = h.app.new_tab().is_some();
        h.app.flush_all_song_sync();
        ok
    });
    if !ok {
        return Err(js_native("newTab: tab limit reached"));
    }
    Ok(JsValue::undefined())
}

/// `daw.closeTab()` — アクティブなタブを閉じる (未保存でも確認せず捨てる = headless)。
pub(super) fn daw_close_tab(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    with_host(|h| {
        let key = h.app.pk();
        h.app.close_tab_now(key);
        h.app.flush_all_song_sync();
    });
    Ok(JsValue::undefined())
}

/// `daw.switchTab(index)` — 表示順 `index` のタブをアクティブにする。
pub(super) fn daw_switch_tab(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let index = usize::try_from_js(args.get_or_undefined(0), ctx)?;
    let ok = with_host(|h| {
        let Some(key) = h.app.tabs.order.get(index).copied() else {
            return false;
        };
        h.app.switch_tab(key);
        true
    });
    if !ok {
        return Err(js_native(format!("switchTab: no tab at index {index}")));
    }
    Ok(JsValue::undefined())
}

/// `daw.tabsJson()` — `[{index, key, path, dirty, playing, active}]`。
pub(super) fn daw_tabs_json(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let json = with_host(|h| {
        let rows: Vec<serde_json::Value> = h
            .app
            .tabs
            .order
            .iter()
            .enumerate()
            .filter_map(|(index, key)| {
                let ps = h.app.tab(*key)?;
                // 走行状態は engine の telemetry (GUI の Tick と同じ面) から読む。
                let playing = h
                    .bootstrap
                    .bridge
                    .find_project(*key)
                    .is_some_and(|b| b.playing());
                Some(serde_json::json!({
                    "index": index,
                    "key": key.0,
                    "path": ps.song_doc.file_path.as_ref().map(|p| p.display().to_string()),
                    "dirty": ps.song_doc.is_dirty(),
                    "playing": playing,
                    "active": *key == h.app.pk(),
                }))
            })
            .collect();
        serde_json::to_string(&rows)
    })
    .map_err(|e| js_native(format!("tabsJson: serialize: {e}")))?;
    Ok(JsString::from(json.as_str()).into())
}

/// `daw.metricsJson()` — engine の負荷 (`MetricsBridge`、resource monitor と同じ面):
/// `{dspLoadAvg, xrunCount, bufferFrames, sampleRate}`。`docs/plan_project_tabs.md` §7 の
/// 「1 タブのコストは変更前と同等」を headless で実測するための口。
pub(super) fn daw_metrics_json(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let json = with_host(|h| {
        let m = &h.bootstrap.metrics;
        let (buffer_frames, sample_rate) = m.buffer_info();
        serde_json::to_string(&serde_json::json!({
            "dspLoadAvg": m.dsp_load_avg(),
            "xrunCount": m.xrun_count(),
            "bufferFrames": buffer_frames,
            "sampleRate": sample_rate,
        }))
    })
    .map_err(|e| js_native(format!("metricsJson: serialize: {e}")))?;
    Ok(JsString::from(json.as_str()).into())
}
