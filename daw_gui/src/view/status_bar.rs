//! 画面下端のステータスバー: ファイルパス / MIDI 入力 / status_message +
//! resource monitor (r.md #3) の常駐メーター (DSP / CPU / FPS / xrun)。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::{theme, Color, Rect};

use crate::app::{AppData, AppEvent};

const COLOR_BG: Color = theme::HEADER;
// 旧 dim グレー (0.65/0.68/0.72) はコントラスト不足だったため primary
// (= 他 view と同じ body text) に統一。MIDI/file ラベルの可読性を上げる。
const COLOR_TEXT: Color = theme::TEXT;
// status_message は成功/通知系の緑 = semantic PLAY (status success)。
const COLOR_MSG: Color = theme::PLAY;

/// DSP / CPU load (0..1) を緑→黄→赤で色分け。 閾値は `metrics_bridge` の SSoT。
fn load_color(load: f32) -> Color {
    if load >= common::metrics_bridge::LOAD_DANGER {
        theme::METER_RED
    } else if load >= common::metrics_bridge::LOAD_WARN {
        theme::METER_YELLOW
    } else {
        theme::METER_GREEN
    }
}

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    ui.panel("status_bg", area, COLOR_BG, 0.0);

    let pad = 12.0;
    let line_y = area.y + (area.h - 11.0) * 0.5;

    let left = format!(
        "MIDI: {} \u{2502} file: {}",
        if app.midi_input_label.is_empty() {
            "(none)"
        } else {
            app.midi_input_label.as_str()
        },
        app.file_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(unsaved)".to_string()),
    );
    ui.label_at("status_left", &left, area.x + pad, line_y, 11.0, COLOR_TEXT);

    // ----- 常駐リソースメーター (r.md #3) -----
    // 右端に DSP load (peak) / system CPU / FPS / xrun を色付きで常駐表示し、
    // クリックで詳細パネルを開閉する。 app_config で on/off (デフォルト on)。
    let mut left_limit = area.x + area.w - pad;
    if app.resource_monitor_enabled {
        let m = &app.metrics;
        let badge_w = 248.0;
        let badge_x = area.x + area.w - badge_w - pad;
        let badge_y = area.y + 3.0;
        let badge_h = area.h - 6.0;
        // バッジを 2 領域に分ける。 左 (DSP/CPU/FPS) = クリックで詳細パネル開閉、
        // 右 (xrun) = クリックで xrun カウンタをクリア (パネルを開かずに直接)。
        // 背景の薄いボタン面が「クリックできる」アフォーダンスを兼ねる。
        let main_w = 170.0;
        ui.button_at(
            "resmon_badge",
            "",
            Rect { x: badge_x, y: badge_y, w: main_w, h: badge_h },
            || Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::ToggleResourcePanel)),
        );
        ui.button_at(
            "resmon_xrun_clear",
            "",
            Rect { x: badge_x + main_w, y: badge_y, w: badge_w - main_w, h: badge_h },
            || {
                Edit::mutate(|app: &mut AppData| {
                    if let Some(mb) = &app.metrics_bridge {
                        mb.reset_xrun();
                    }
                    app.metrics.xrun_count = 0;
                })
            },
        );
        // button 面の上に色分けラベルを重ねる (描画順 = 後勝ちで前面)。
        let mut mx = badge_x + 8.0;
        ui.label_at(
            "resmon_dsp",
            &format!("DSP {:>3.0}%", (m.dsp_load_peak * 100.0).min(999.0)),
            mx,
            line_y,
            11.0,
            load_color(m.dsp_load_peak),
        );
        mx += 62.0;
        ui.label_at(
            "resmon_cpu",
            &format!("CPU {:>3.0}%", m.system_cpu.min(999.0)),
            mx,
            line_y,
            11.0,
            load_color(m.system_cpu / 100.0),
        );
        mx += 62.0;
        ui.label_at(
            "resmon_fps",
            &format!("{:>2.0}fps", m.fps.min(999.0)),
            mx,
            line_y,
            11.0,
            COLOR_TEXT,
        );
        mx += 46.0;
        ui.label_at(
            "resmon_xrun",
            &format!("xr {}", m.xrun_count.min(9999)),
            mx,
            line_y,
            11.0,
            if m.xrun_count > 0 {
                theme::RECORD
            } else {
                theme::TEXT_DIM
            },
        );
        left_limit = badge_x - 8.0;
    }

    if !app.status_message.is_empty() {
        // メーターと被らない位置にだけ status_message を出す。
        let mid_x = area.x + area.w * 0.55;
        if mid_x < left_limit {
            ui.label_at(
                "status_message",
                &app.status_message,
                mid_x,
                line_y,
                11.0,
                COLOR_MSG,
            );
        }
    }
}
