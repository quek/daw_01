//! 画面下端のステータスバー: ファイルパス / MIDI 入力 / status_message +
//! resource monitor (r.md #3) の常駐メーター (DSP / CPU / FPS / xrun)。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent};
use crate::view::resource_monitor::load_color;

/// 常駐リソースメーターのバッジ幅。 左側テキストの clip 幅を先に決めるため、
/// 描画側とここで同じ値を使う (定数 1 つを共有 = 値の二重化を作らない)。
const RESMON_BADGE_W: f32 = 248.0;

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    // ステータスバーはクロームのバー類 (= transport / menu bar と同じ層)。
    // 上に乗る文字はこの面 (パレット自身のクローム) の上なのでテーマ従属の
    // `text` / `text_dim` でよい (極性固定インクは不要)。
    let p = &app.theme.core;
    ui.panel("status_bg", area, p.header, 0.0);

    let pad = 12.0;
    let line_y = area.y + (area.h - 11.0) * 0.5;

    // 左側テキストが使える右端。 常駐メーターが出ているならその手前まで
    // (下の描画より先に決めておく — 待受表示と MIDI/file 行の両方を clip したい)。
    let meters_left = if app.ui_prefs.resource_monitor_enabled {
        area.x + area.w - RESMON_BADGE_W - pad - 8.0
    } else {
        area.x + area.w - pad
    };

    // ----- 変調ソースの待受表示 (r.md #78) -----
    // ◉ 中は「次に触ったツマミ」に繋がるので、 待受中であることが常に見えて
    // いなければ事故になる。 ラックの ◉ ボタンはカーソルトラック所有のソース
    // しか出ないため、 トラックを移ると消える = 唯一の表示にできない。
    //
    // ソース色は暗いテーマ前提のパレットなので、 **文字には使わない**。
    // 色は小さなチップ (塗り矩形) だけに載せ、 文字はテーマの `text` に固定する
    // (明/暗どちらの背景でも読める)。
    let mut left_x = area.x + pad;
    if let Some((color, name)) = app.armed_mod_source_label() {
        let chip = 10.0;
        ui.panel(
            "status_mod_arm_chip",
            Rect { x: left_x, y: area.y + (area.h - chip) * 0.5, w: chip, h: chip },
            daw_ui_renderer::Color { r: color[0], g: color[1], b: color[2], a: 1.0 },
            2.0,
        );
        left_x += chip + 6.0;
        let text = format!("{name} 待受中 \u{2014} 触ったツマミに繋がります (Esc で解除)");
        // 幅は「実測」と「メーターまでの残り」の小さい方。 素の label_at は clip も
        // 省略もしないので、 狭い窓では常駐メーターに重なって双方読めなくなる。
        let w = ui.measure_text(&text, 11.0).min((meters_left - left_x).max(0.0));
        ui.label_at_clipped(
            "status_mod_arm",
            &text,
            Rect { x: left_x, y: line_y, w, h: 11.0 * 1.2 },
            11.0,
            p.text,
        );
        // 続く MIDI / file 行を待受表示の右へずらす (重ねない)。
        left_x += w + 16.0;
    }

    let left = format!(
        "MIDI: {} \u{2502} file: {}",
        if app.recording.midi_input_label.is_empty() {
            "(none)"
        } else {
            app.recording.midi_input_label.as_str()
        },
        app.song_doc.file_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(unsaved)".to_string()),
    );
    // 旧 dim グレーはコントラスト不足だったため primary (= 他 view と同じ
    // body text) に統一。MIDI/file ラベルの可読性を上げる。
    // 待受表示が出ると開始位置が右へずれるので、 常駐メーターの手前で切る
    // (長いファイルパスがメーターに重なるのを防ぐ)。
    ui.label_at_clipped(
        "status_left",
        &left,
        Rect {
            x: left_x,
            y: line_y,
            w: (meters_left - left_x).max(0.0),
            h: 11.0 * 1.2,
        },
        11.0,
        p.text,
    );

    // ----- 常駐リソースメーター (r.md #3) -----
    // 右端に DSP load (peak) / system CPU / FPS / xrun を色付きで常駐表示し、
    // クリックで詳細パネルを開閉する。 app_config で on/off (デフォルト on)。
    // 右端は上で決めた `meters_left` と同じ (同じ値を 2 度計算しない)。
    let left_limit = meters_left;
    if app.ui_prefs.resource_monitor_enabled {
        let m = &app.ipc.metrics;
        let badge_w = RESMON_BADGE_W;
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
                    if let Some(mb) = &app.ipc.metrics_bridge {
                        mb.reset_xrun();
                    }
                    app.ipc.metrics.xrun_count = 0;
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
            load_color(&app.theme, m.dsp_load_peak),
        );
        mx += 62.0;
        ui.label_at(
            "resmon_cpu",
            &format!("CPU {:>3.0}%", m.system_cpu.min(999.0)),
            mx,
            line_y,
            11.0,
            load_color(&app.theme, m.system_cpu / 100.0),
        );
        mx += 62.0;
        ui.label_at(
            "resmon_fps",
            &format!("{:>2.0}fps", m.fps.min(999.0)),
            mx,
            line_y,
            11.0,
            p.text,
        );
        mx += 46.0;
        ui.label_at(
            "resmon_xrun",
            &format!("xr {}", m.xrun_count.min(9999)),
            mx,
            line_y,
            11.0,
            if m.xrun_count > 0 {
                app.theme.daw.record
            } else {
                p.text_dim
            },
        );
        debug_assert!(
            (left_limit - (badge_x - 8.0)).abs() < 0.01,
            "meters_left と badge 実配置がずれている (幅の定数が二重化した)"
        );
    }

    if !app.ui_ephemeral.status_message.is_empty() {
        // メーターと被らない **幅** で status_message を出す。 旧実装は開始位置
        // (mid_x < left_limit) だけを見て label_at (clip も ellipsis も無し) を撃って
        // いたため、 長い日本語メッセージ (例:「ここには貼り付けできません (…)」= 377px)
        // が DSP / CPU / fps ラベルの上に重なって双方読めなくなっていた。
        let mid_x = area.x + area.w * 0.55;
        if mid_x < left_limit {
            ui.label_at_clipped(
                "status_message",
                &app.ui_ephemeral.status_message,
                Rect {
                    x: mid_x,
                    y: line_y,
                    w: (left_limit - mid_x).max(0.0),
                    h: 11.0 * 1.2,
                },
                11.0,
                // status_message は成功 / 通知系の緑 = semantic な play
                // (status success)。
                app.theme.daw.play,
            );
        }
    }
}
