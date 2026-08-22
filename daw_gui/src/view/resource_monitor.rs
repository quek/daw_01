// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! resource monitor 詳細パネル (r.md #3): status bar クリックで開く非モーダルな
//! floating overlay。 全体指標 (DSP / system CPU / FPS / メモリ / xrun / buffer) +
//! トラック別・プラグイン別の CPU 内訳。
//!
//! daw-ui の `popup_layer` + `open_popup(modal=true, capture_input=false)` で実装する。
//! これにより:
//! - panel は z-order 最前面 (deferred buffer) に描かれる
//! - **panel の裏に隠れた widget (arrangement 等) の click は抑制される** (modal=true)
//!   = panel 上クリックが背後に突き抜けない
//! - 背景の他領域 (transport の再生ボタン等) は操作可能・暗転なし (capture_input=false)
//! - panel 外 click / Esc で閉じる
//!
//! 開閉状態は `AppData.resource_panel_open` を SSoT とし、 毎フレーム popup の
//! open/close と同期する (menu_picker と同じ idiom)。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::{Color, Rect};
use crate::theme::Theme;

use crate::app::{resolve_plugin_name, AppData, AppEvent};

const ROW_H: f32 = 18.0;
const PANEL_W: f32 = 360.0;
const PANEL_TOP: f32 = 76.0; // MENU_H(24) + TRANSPORT_H(44) + 8 の下。
const PANEL_ID: &str = "resource_panel";

/// DSP / CPU load (0..1) を緑→黄→赤で色分け。 閾値は `metrics_bridge` の SSoT、
/// 色は meter ramp (色相固定・明度可変) の SSoT。
///
/// ステータスバーの常駐バッジ (`view::status_bar`) もこの 1 実装を呼ぶ
/// (旧実装は status_bar 側に 1 文字違わない複製があった)。
#[must_use]
pub fn load_color(theme: &Theme, load: f32) -> Color {
    if load >= common::metrics_bridge::LOAD_DANGER {
        theme.core.meter_red
    } else if load >= common::metrics_bridge::LOAD_WARN {
        theme.core.meter_yellow
    } else {
        theme.core.meter_green
    }
}

/// 内容 (全体指標 + track/plugin 行数) からパネル rect を決める。 右側固定。
fn panel_rect(app: &AppData, screen: Rect) -> Rect {
    let n_tracks = app.song_doc.song().tracks.len();
    let n_plugins: usize = app
        .song_doc.song()
        .tracks
        .iter()
        .map(|t| app.ipc.track_plugin_ids.get(&t.id).map_or(0, Vec::len))
        .sum();
    let header_h = 28.0;
    let overall_h = ROW_H * 4.0;
    let list_h = ROW_H * (n_tracks + n_plugins) as f32;
    let ph = (header_h + overall_h + 16.0 + list_h + 12.0).min(screen.h - PANEL_TOP - 16.0);
    Rect {
        x: screen.x + screen.w - PANEL_W - 16.0,
        y: PANEL_TOP,
        w: PANEL_W,
        h: ph,
    }
}

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, screen: Rect) {
    let rect = panel_rect(app, screen);

    // AppData.resource_panel_open (SSoT) ↔ overlay open 状態を同期。
    // `open_overlay` = pointer は masking する (panel 上クリックが背後の arrangement に
    // 突き抜けない) が、 keyboard / shortcut は background に通す (Space 再生等が有効)。
    // backdrop (暗転) も描かないので非暗転。 panel 外クリック / Esc で閉じる。
    let open_in_ui = ui.is_overlay_open(PANEL_ID);
    if app.ui_ephemeral.resource_panel_open && !open_in_ui {
        ui.open_overlay(PANEL_ID);
    } else if !app.ui_ephemeral.resource_panel_open && open_in_ui {
        ui.close_overlay(PANEL_ID);
    }
    if !ui.is_overlay_open(PANEL_ID) {
        return;
    }
    // anchor (= panel rect) を最新化 — 外クリック判定 (panel 外 = dismiss) の基準。
    ui.update_popup_anchor(("overlay", PANEL_ID), rect);

    ui.popup_layer(("overlay", PANEL_ID), |ui| {
        draw_contents(app, ui, rect);
    });

    // panel 外クリック / Esc で閉じた → SSoT を同期して閉じる。
    if app.ui_ephemeral.resource_panel_open && !ui.is_overlay_open(PANEL_ID) {
        ui.push_edit(Edit::mutate(|app: &mut AppData| app.ui_ephemeral.resource_panel_open = false));
    }
}

/// ラベル + load バー (幅 = load、 色 = `load_color`) + % 数値の 1 行。
/// バーの溝はパネル面に彫り込む窪みなので `inset_bg`、 文字は溝ではなく
/// パネル面の上に乗るのでテーマ従属の `text` / `text_dim`。
fn load_row(
    theme: &Theme,
    ui: &mut Ui<'_, AppData>,
    id: &str,
    label: &str,
    indent: f32,
    load: f32,
    row: Rect,
) {
    let p = &theme.core;
    let text_y = row.y + (row.h - 11.0) * 0.5;
    ui.label_at((id, 0u8), label, row.x + indent, text_y, 11.0, p.text);
    let bar_x = row.x + row.w * 0.46;
    let bar_w = row.w * 0.40;
    let bar_h = 8.0;
    let bar_y = row.y + (row.h - bar_h) * 0.5;
    ui.panel(
        (id, 1u8),
        Rect { x: bar_x, y: bar_y, w: bar_w, h: bar_h },
        p.inset_bg,
        2.0,
    );
    let fill_w = bar_w * load.clamp(0.0, 1.0);
    if fill_w > 0.0 {
        ui.panel(
            (id, 2u8),
            Rect { x: bar_x, y: bar_y, w: fill_w, h: bar_h },
            load_color(theme, load),
            2.0,
        );
    }
    ui.label_at(
        (id, 3u8),
        &format!("{:>3.0}%", (load * 100.0).min(999.0)),
        bar_x + bar_w + 6.0,
        text_y,
        11.0,
        p.text_dim,
    );
}

fn draw_contents(app: &AppData, ui: &mut Ui<'_, AppData>, panel: Rect) {
    let p = &app.theme.core;
    let m = &app.ipc.metrics;
    let px = panel.x;
    let py = panel.y;
    let ph = panel.h;
    let cw = PANEL_W - 16.0;

    // buffer period (μs) = per-track/plugin の処理時間 → load% の分母。
    let period_us = if m.sample_rate > 0 {
        m.buffer_frames as f32 / m.sample_rate as f32 * 1_000_000.0
    } else {
        0.0
    };
    let load_of = |us: u32| if period_us > 0.0 { us as f32 / period_us } else { 0.0 };
    // L1: per-plugin 計測は device_id (u64) を **値** で保持する slot に格納される
    // (index 化しないので id が MAX_PLUGINS を超えても drop しない)。 read 前に
    // 現在 live な device 集合で stale slot (unload 済み device) を解放し、 slot
    // 枯渇を防ぐ (unload 済み device の worker は既に store しないので安全)。
    if let Some(mb) = app.ipc.metrics_bridge.as_ref() {
        let song = app.song_doc.song();
        let live: std::collections::HashSet<u64> = song
            .tracks
            .iter()
            .flat_map(|t| t.devices.iter())
            .chain(song.master_fx_chain.iter())
            .map(|d| d.id)
            .filter(|&id| id != 0)
            .collect();
        mb.reclaim_plugin_metric_slots(&live);
    }
    let plugin_us = |pid: u64| {
        app.ipc.metrics_bridge
            .as_ref()
            .map_or(0, |mb| mb.plugin_dsp_us(pid))
    };

    // パネル背景 + タイトルバー。
    ui.panel("resmon_panel_bg", panel, p.panel, 6.0);
    ui.panel(
        "resmon_titlebar",
        Rect { x: px, y: py, w: PANEL_W, h: 24.0 },
        p.header,
        6.0,
    );
    ui.label_at("resmon_title", "Performance", px + 12.0, py + 7.0, 13.0, p.text);
    // xrun カウンタをリセットするボタン (Ardour / Cubase の reset xruns に相当)。
    ui.button_at(
        "resmon_clear_xrun",
        "Clear xr",
        Rect { x: px + PANEL_W - 96.0, y: py + 4.0, w: 62.0, h: 18.0 },
        || {
            Edit::mutate(|app: &mut AppData| {
                if let Some(mb) = &app.ipc.metrics_bridge {
                    mb.reset_xrun();
                }
                // 即時反映 (次の poll tick で bridge の 0 を読み直す)。
                app.ipc.metrics.xrun_count = 0;
            })
        },
    );
    ui.button_at(
        "resmon_close",
        "\u{2715}",
        Rect { x: px + PANEL_W - 28.0, y: py + 4.0, w: 22.0, h: 18.0 },
        || Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::ToggleResourcePanel)),
    );

    let mut y = py + 28.0;
    let row = |y: f32| Rect { x: px + 8.0, y, w: cw, h: ROW_H };

    // ---- 全体指標 ----
    load_row(
        &app.theme,
        ui,
        "resmon_o_dsp",
        &format!("DSP peak (avg {:>3.0}%)", m.dsp_load_avg * 100.0),
        0.0,
        m.dsp_load_peak,
        row(y),
    );
    y += ROW_H;
    load_row(&app.theme, ui, "resmon_o_cpu", "System CPU", 0.0, m.system_cpu / 100.0, row(y));
    y += ROW_H;
    ui.label_at(
        "resmon_o_fps",
        &format!("FPS {:.0}     RAM {:.0} MB", m.fps, m.memory_mb),
        px + 12.0,
        y + 3.0,
        11.0,
        p.text,
    );
    y += ROW_H;
    let latency_ms = if m.sample_rate > 0 {
        m.buffer_frames as f32 / m.sample_rate as f32 * 1000.0
    } else {
        0.0
    };
    ui.label_at(
        "resmon_o_buf",
        &format!(
            "xrun {}   buffer {} @ {} Hz  ({latency_ms:.1} ms)",
            m.xrun_count, m.buffer_frames, m.sample_rate
        ),
        px + 12.0,
        y + 3.0,
        11.0,
        if m.xrun_count > 0 {
            app.theme.daw.record
        } else {
            p.text_dim
        },
    );
    y += ROW_H + 8.0;

    ui.panel(
        "resmon_sep",
        Rect { x: px + 8.0, y, w: cw, h: 1.0 },
        p.border,
        0.0,
    );
    y += 8.0;

    // ---- トラック別 / プラグイン別 CPU 内訳 ----
    let bottom = py + ph - ROW_H;
    'tracks: for track in &app.song_doc.song().tracks {
        let pids = app.ipc.track_plugin_ids.get(&track.id);
        let track_us: u32 = pids.map_or(0, |v| v.iter().map(|pid| plugin_us(*pid)).sum());
        load_row(
            &app.theme,
            ui,
            &format!("resmon_tr_{}", track.id),
            &track.name,
            0.0,
            load_of(track_us),
            row(y),
        );
        y += ROW_H;
        if let Some(pids) = pids {
            // device 名と host plugin_id を chain 順で対応づけ、 plugin 別 load を出す。
            for (device, pid) in track.devices.iter().zip(pids.iter()) {
                if y > bottom {
                    break 'tracks; // パネル下端を超えたら打ち切り。
                }
                let name = resolve_plugin_name(&app.ipc.plugin_db, &device.plugin_id);
                load_row(
                    &app.theme,
                    ui,
                    &format!("resmon_pl_{pid}"),
                    &name,
                    14.0,
                    load_of(plugin_us(*pid)),
                    row(y),
                );
                y += ROW_H;
            }
        }
        if y > bottom {
            break;
        }
    }
}
