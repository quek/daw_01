//! `docs/plan_project_tabs.md` §5.4: プロジェクトタブの script API (`daw.newTab` / `closeTab` /
//! `switchTab(index)` / `tabsJson`) と engine の負荷 (`metricsJson`)。既存の API はすべて
//! アクティブなタブに効く。

use boa_engine::value::TryFromJs;
use boa_engine::{Context, JsArgs, JsResult, JsString, JsValue};

use super::{js_native, with_app, with_host};

/// `daw.newTab()` — 空の Untitled を新しいタブに開いてアクティブにする。
pub(super) fn daw_new_tab(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let ok = with_app(move |app, _side, _io| app.new_tab().is_some());
    if !ok {
        return Err(js_native("newTab: tab limit reached"));
    }
    Ok(JsValue::undefined())
}

/// `daw.closeTab()` — アクティブなタブを閉じる (未保存でも確認せず捨てる = headless)。
pub(super) fn daw_close_tab(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    with_app(move |app, _side, _io| {
        let key = app.pk();
        app.close_tab_now(key);
    });
    Ok(JsValue::undefined())
}

/// `daw.switchTab(index)` — 表示順 `index` のタブをアクティブにする。
pub(super) fn daw_switch_tab(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let index = usize::try_from_js(args.get_or_undefined(0), ctx)?;
    let ok = with_app(move |app, _side, _io| {
        let Some(key) = app.tabs.order.get(index).copied() else {
            return false;
        };
        app.switch_tab(key);
        true
    });
    if !ok {
        return Err(js_native(format!("switchTab: no tab at index {index}")));
    }
    Ok(JsValue::undefined())
}

/// `daw.tabsJson()` — `[{index, key, path, dirty, playing, active}]`。
pub(super) fn daw_tabs_json(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let json = with_app(move |app, _side, io| {
        let rows: Vec<serde_json::Value> = app
            .tabs
            .order
            .iter()
            .enumerate()
            .filter_map(|(index, key)| {
                let ps = app.tab(*key)?;
                // 走行状態は engine の telemetry (GUI の Tick と同じ面) から読む。
                let playing = io.bridge
                    .find_project(*key)
                    .is_some_and(|b| b.playing());
                Some(serde_json::json!({
                    "index": index,
                    "key": key.0,
                    "path": ps.song_doc.file_path.as_ref().map(|p| p.display().to_string()),
                    "dirty": ps.song_doc.is_dirty(),
                    "playing": playing,
                    "active": *key == app.pk(),
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
        let m = &h.io.metrics;
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

/// `daw.perfJson()` — 負荷の計測一式を **同じ時刻で** 返す:
/// `{dspLoadAvg, xrunCount, bufferFrames, sampleRate, loadHist, graph, pluginUs}`。
///
/// - `loadHist`: buffer ごとの DSP load の分布 (累積、5% 刻み 41 bucket。差分を取って使う)。
/// - `graph`: 直近の窓のグラフ内訳 (`wallNs` / `busyNs` / `dispatchNs` / `dispatches` / `steps` /
///   `buffers`)。`busy - dispatch` がエンジン自身の仕事、`dispatch / dispatches` が plugin 1 回の
///   完了待ち (audio 側)。
/// - `pluginUs`: plugin_host が測った各 instance の直近 `process()` 時間 (μs、同じ 1 回の host 側)。
///   `dispatch / dispatches` との差が IPC の受け渡しにかかった分。
///
/// 計測を「鳴っているとき」に取っているかは呼び手が `masterPeakDbfs` で確かめること
/// (鳴っていないプラグインは軽いので、無音の比較は結論を誤らせる)。
pub(super) fn daw_perf_json(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let graph = with_host(super::ScriptHost::graph_window);
    let json = with_host(|h| {
        let m = &h.io.metrics;
        let (buffer_frames, sample_rate) = m.buffer_info();
        let mut plugin: Vec<(common::protocol::InstanceToken, u32)> = Vec::new();
        let plane_id = m.plugin_plane_id();
        if plane_id != 0
            && let Ok(plane) = common::metrics_bridge::PluginMetricsPlane::open(m.os_id(), plane_id)
        {
            plane.read(&mut plugin);
        }
        let plugin_us: Vec<u32> = plugin.iter().map(|&(_, us)| us).collect();
        serde_json::to_string(&serde_json::json!({
            "dspLoadAvg": m.dsp_load_avg(),
            "xrunCount": m.xrun_count(),
            "bufferFrames": buffer_frames,
            "sampleRate": sample_rate,
            "loadHist": m.load_hist().to_vec(),
            "graph": {
                "wallNs": graph.wall_ns,
                "busyNs": graph.busy_ns,
                "dispatchNs": graph.dispatch_ns,
                "dispatches": graph.dispatches,
                "steps": graph.steps,
                "buffers": graph.buffers,
            },
            "pluginUs": plugin_us,
        }))
    })
    .map_err(|e| js_native(format!("perfJson: serialize: {e}")))?;
    Ok(JsString::from(json.as_str()).into())
}

