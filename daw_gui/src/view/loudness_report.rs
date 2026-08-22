//! ラウドネスレポート window (r.md #54)。
//!
//! 範囲を freewheel で走査して得た EBU R128 の測定値を出す **移動・リサイズ可能な
//! floating window**。grill-me (2026-08-16) で確定した挙動:
//!
//! - 解析を始めると窓が**先に**開き、その中に進捗バーと中止ボタンが出る。
//!   走査中は画面全体を暗転して操作を遮断する (= 測っている最中に前提が変わらない)。
//! - 走査が終わると暗転が消え、窓はそのまま残る。以後は窓の外を普通に操作でき、
//!   最大値の位置へ飛んだり、再生したり、測り直したりできる。
//! - 走査中もグラフは**左から右へ伸びていく** (途中経過が数値と曲線の両方で届く)。
//! - 目標ラウドネスはマスターパネルのラウドネスメーターと**同じ値**
//!   (`MeterSettings.loudness_target_lufs`)。プリセットを選ぶとメーターの
//!   0 LU 線も一緒に動く。
//!
//! 遮断の実装は [`Ui::reserve_floating_region`] 1 本。走査中は **画面全体**を
//! 予約して背景の pointer を落とし、窓本体だけ [`Ui::with_floating_region`] で
//! raw pointer に戻す。暗転の矩形も窓と同じ層に描くので、「暗いのに押せる」
//! 「押せるのに効かない」のどちらも起きない。

use daw_ui_core::{DragKind, Edit, LoudnessGraphStyle, Ui};
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::app::{AppData, AppEvent};
use crate::state::LoudnessPhase;
use common::loudness_report::{LOUDNESS_HISTOGRAM_MIN_LUFS, LOUDNESS_HISTOGRAM_STEP_LU};

/// メニューバー高 (root::MENU_H のミラー)。縦 clamp の基準。
const MENU_H: f32 = 24.0;
/// 初期表示位置の上端 (MENU_H + TRANSPORT_H(44) + 8)。
const PANEL_TOP: f32 = 76.0;
/// 既定 / 最小サイズ。最小は「数値表 8 行 + プリセット行 + グラフ」が
/// **どれも消えずに** 収まる高さから逆算した (走査中は進捗バーぶん 26px 増える):
/// タイトル 32 + 見出し 38 + 進捗 26 + 数値 152 + プリセット 30 + グラフ 120 + 余白 12。
/// 幅は 132 + 118 + 92 (ラベル / 値 / 位置) + 目標との差の注記が読める分。
const DEFAULT_W: f32 = 700.0;
const DEFAULT_H: f32 = 480.0;
const MIN_W: f32 = 560.0;
const MIN_H: f32 = 420.0;
const TITLE_H: f32 = 24.0;
const CLOSE_W: f32 = 26.0;
const PAD: f32 = 12.0;
const ROW_H: f32 = 19.0;
const BTN_H: f32 = 22.0;
/// 端リサイズ grab 帯の幅 (px)。
const RESIZE_MARGIN: f32 = 6.0;
/// 右下隅リサイズ grip の一辺 (px)。
const CORNER: f32 = 14.0;
/// 数値表の列幅。
const LABEL_W: f32 = 150.0;
const VALUE_W: f32 = 118.0;
const POS_W: f32 = 92.0;
/// ヒストグラムの幅 (グラフの右に縦軸を共有して並べる)。
const HIST_W: f32 = 92.0;
/// グラフ + ヒストグラムを描く最低高 (これを割るのは窓が画面より高い場合だけ。
/// 通常は `MIN_H` の逆算で 120px 以上が残る)。
const GRAPH_MIN_H: f32 = 60.0;

/// 配信ターゲットのプリセット `(名前, Integrated [LUFS], True Peak 上限 [dBTP])`。
///
/// 値は Ardour の `loudness_settings.cc` が持つ実値に合わせてある
/// (EBU R128 = -23 / -1.0、Spotify = -14 / -1.0、YouTube = -14 / -1.0、
/// Apple Music = -16 / -1.0、Amazon = -14 / -2.0)。
const TARGET_PRESETS: &[(&str, f32, f32)] = &[
    ("EBU R128", -23.0, -1.0),
    ("Spotify", -14.0, -1.0),
    ("YouTube", -14.0, -1.0),
    ("Apple Music", -16.0, -1.0),
    ("Amazon", -14.0, -2.0),
];

/// 初回 (未配置) の既定 window rect: 画面中央やや上。
fn default_rect(screen: Rect) -> Rect {
    let w = DEFAULT_W.min((screen.w - 32.0).max(MIN_W));
    let h = DEFAULT_H.min((screen.h - PANEL_TOP - 16.0).max(MIN_H));
    Rect { x: screen.x + (screen.w - w) * 0.5, y: screen.y + PANEL_TOP, w, h }
}

/// 保存 rect をサイズ最小・タイトルバー可視の範囲に clamp。
fn clamp_to_screen(r: Rect, screen: Rect) -> Rect {
    let w = r.w.clamp(MIN_W, screen.w.max(MIN_W));
    let h = r.h.clamp(MIN_H, screen.h.max(MIN_H));
    let x = r.x.clamp(screen.x + 60.0 - w, screen.x + screen.w - 60.0);
    let y = r.y.clamp(screen.y + MENU_H, (screen.y + screen.h - TITLE_H).max(screen.y + MENU_H));
    Rect { x, y, w, h }
}

/// 走査中は窓**全体**を画面内へ収める。通常の clamp は「タイトルバーが見えていれば
/// よい」なので、端に寄せたまま解析を始めると右上の「中止」ボタンが画面外に出て、
/// 逃げ道が Esc だけになる (背景は暗転していて窓を掴み直せない)。
fn clamp_fully_visible(r: Rect, screen: Rect) -> Rect {
    let w = r.w.min(screen.w);
    let h = r.h.min((screen.h - MENU_H).max(1.0));
    Rect {
        x: r.x.clamp(screen.x, (screen.x + screen.w - w).max(screen.x)),
        y: r.y.clamp(screen.y + MENU_H, (screen.y + screen.h - h).max(screen.y + MENU_H)),
        w,
        h,
    }
}

/// 現在の committed window rect (未配置なら既定)。reserve / draw / visual test が
/// **同じ**基準として使う。
pub fn window_rect(app: &AppData, screen: Rect) -> Rect {
    let r = app.ui_prefs.loudness_report_rect.unwrap_or_else(|| default_rect(screen));
    let r = clamp_to_screen(r, screen);
    if app.loudness.phase.is_busy() {
        clamp_fully_visible(r, screen)
    } else {
        r
    }
}

/// build_root の **背景 widget 描画より前** に呼ぶ。
///
/// 走査中は **画面全体** を予約して背景を丸ごと inert にする (= 暗転と入力遮断が
/// 同じ 1 つの根拠から出る)。走査していないときは窓の rect だけを予約する。
pub fn reserve(app: &AppData, ui: &mut Ui<'_, AppData>, screen: Rect) {
    if !app.ui_prefs.loudness_report_open {
        return;
    }
    if app.loudness.phase.is_busy() {
        ui.reserve_floating_region(screen);
    } else {
        ui.reserve_floating_region(window_rect(app, screen));
    }
}

/// build_root の **末尾近く** (背景描画の後) に呼ぶ。
pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, screen: Rect) {
    if !app.ui_prefs.loudness_report_open {
        return;
    }
    ui.with_floating_region(|ui| {
        if app.loudness.phase.is_busy() {
            // 背景の暗転。入力は reserve が既に落としているので、これは
            // 「触れない」ことを見せるためだけの層。
            ui.push_rect(RectCommand {
                rect: screen,
                fill: app.theme.core.backdrop,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        }
        draw_window(app, ui, screen);
    });
}

fn draw_window(app: &AppData, ui: &mut Ui<'_, AppData>, screen: Rect) {
    let committed = window_rect(app, screen);
    // drag / resize の delta を committed に載せた「表示 rect」。release まで
    // ui_prefs は書き換えない (settings / undo_history と同じ流儀)。
    let mut rect = committed;
    let mut commit = false;
    // 走査中は動かせない (暗転で全遮断しているので、窓だけ動かせると
    // 「遮断しているのに動く」という矛盾した見え方になる)。
    let busy = app.loudness.phase.is_busy();

    if !busy {
        let title_drag = Rect {
            x: committed.x,
            y: committed.y,
            w: (committed.w - CLOSE_W).max(0.0),
            h: TITLE_H,
        };
        if let Some(d) = ui.take_drag_in_rect("loudness_move", title_drag) {
            rect.x = committed.x + d.delta.0;
            rect.y = committed.y + d.delta.1;
            commit |= matches!(d.kind, DragKind::Released);
        }
        let corner = Rect {
            x: committed.x + committed.w - CORNER,
            y: committed.y + committed.h - CORNER,
            w: CORNER,
            h: CORNER,
        };
        if let Some(d) = ui.take_drag_in_rect("loudness_size_br", corner) {
            rect.w = (committed.w + d.delta.0).max(MIN_W);
            rect.h = (committed.h + d.delta.1).max(MIN_H);
            commit |= matches!(d.kind, DragKind::Released);
        }
        let right_edge = Rect {
            x: committed.x + committed.w - RESIZE_MARGIN,
            y: committed.y + TITLE_H,
            w: RESIZE_MARGIN,
            h: (committed.h - TITLE_H - CORNER).max(0.0),
        };
        if let Some(d) = ui.take_drag_in_rect("loudness_size_r", right_edge) {
            rect.w = (committed.w + d.delta.0).max(MIN_W);
            commit |= matches!(d.kind, DragKind::Released);
        }
        let bottom_edge = Rect {
            x: committed.x,
            y: committed.y + committed.h - RESIZE_MARGIN,
            w: (committed.w - CORNER).max(0.0),
            h: RESIZE_MARGIN,
        };
        if let Some(d) = ui.take_drag_in_rect("loudness_size_b", bottom_edge) {
            rect.h = (committed.h + d.delta.1).max(MIN_H);
            commit |= matches!(d.kind, DragKind::Released);
        }
        rect = clamp_to_screen(rect, screen);
    }

    draw_chrome_and_body(app, ui, rect);

    if commit {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_prefs.loudness_report_rect = Some(rect);
            app.persist_app_config();
        }));
    }
}

fn draw_chrome_and_body(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect) {
    let p = &app.theme.core;
    let busy = app.loudness.phase.is_busy();

    ui.push_rect(RectCommand {
        rect,
        fill: p.panel,
        border: p.border,
        border_width: 1.0,
        radius: [6.0; 4],
        clip_rect: None,
    });
    ui.panel(
        "loudness_titlebar",
        Rect { x: rect.x, y: rect.y, w: rect.w, h: TITLE_H },
        p.header,
        6.0,
    );
    ui.label_at(
        "loudness_title",
        "ラウドネス解析",
        rect.x + 12.0,
        rect.y + 7.0,
        13.0,
        p.text,
    );
    // 走査中は閉じられない (測定を中断せずに窓だけ消すと、暗転だけが残る)。
    if !busy {
        ui.button_at(
            "loudness_close",
            "\u{2715}",
            Rect { x: rect.x + rect.w - CLOSE_W, y: rect.y + 4.0, w: 20.0, h: 18.0 },
            || Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::ToggleLoudnessReport)),
        );
    }

    let mut y = rect.y + TITLE_H + 8.0;
    y = draw_header_row(app, ui, rect, y);
    if busy {
        y = draw_progress(app, ui, rect, y);
    }
    y = draw_values(app, ui, rect, y);
    y = draw_presets(app, ui, rect, y);
    draw_graphs(app, ui, rect, y);

    // 右下隅のリサイズ grip。
    if !busy {
        for i in 0..3 {
            let off = 4.0 + 3.0 * i as f32;
            ui.panel(
                ("loudness_grip", i),
                Rect { x: rect.x + rect.w - off - 2.0, y: rect.y + rect.h - off - 2.0, w: 2.0, h: 2.0 },
                p.text_faint,
                0.0,
            );
        }
    }
}

/// 1 行目: 測った範囲 + 状態 + ボタン。
fn draw_header_row(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect, y: f32) -> f32 {
    let p = &app.theme.core;
    let busy = app.loudness.phase.is_busy();
    let time_sig = app.song_doc.song().time_sig;

    let text = match app.loudness.report.as_ref() {
        Some(r) => format!(
            "範囲 {} – {} ({:.1} 秒)",
            bar_beat(r.range_start_beat, time_sig),
            bar_beat(r.range_end_beat, time_sig),
            (r.total_frames as f64 / f64::from(r.sample_rate.max(1))),
        ),
        None if busy => "範囲を準備中...".to_string(),
        None => "まだ解析していません".to_string(),
    };
    ui.label_at("loudness_range", &text, rect.x + PAD, y + 3.0, 12.0, p.text);

    // 右端にボタン (走査中は「中止」だけ)。
    let bw = 92.0;
    let bx = rect.x + rect.w - PAD - bw;
    if busy {
        let label = if matches!(app.loudness.phase, LoudnessPhase::Cancelling) {
            "中止中..."
        } else {
            "中止"
        };
        if ui.button_at_clicked("loudness_cancel", label, Rect { x: bx, y, w: bw, h: BTN_H }) {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CancelLoudnessAnalysis)
            }));
        }
    } else {
        if ui.button_at_clicked("loudness_run", "解析...", Rect { x: bx, y, w: bw, h: BTN_H }) {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::AnalyzeLoudness)
            }));
        }
        if app.loudness.report.is_some() {
            let rx = bx - bw - 6.0;
            if ui.button_at_clicked(
                "loudness_rerun",
                "測り直す",
                Rect { x: rx, y, w: bw, h: BTN_H },
            ) {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::RerunLoudnessAnalysis)
                }));
            }
        }
    }

    // 古い / 失敗の表示 (2 行目相当、範囲テキストの下)。
    let note_y = y + BTN_H + 2.0;
    if let Some(err) = app.loudness.error.as_ref() {
        ui.label_at("loudness_err", err, rect.x + PAD, note_y, 11.0, p.text_error);
    } else if app.loudness_report_stale() {
        ui.label_at(
            "loudness_stale",
            "曲を編集したので、この値はもう古い (測り直してください)",
            rect.x + PAD,
            note_y,
            11.0,
            p.meter_yellow,
        );
    }
    note_y + 14.0
}

/// 進捗バー。
fn draw_progress(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect, y: f32) -> f32 {
    let p = &app.theme.core;
    let (frac, label) = match app.loudness.phase {
        LoudnessPhase::AwaitingReinit { .. } => (0.0, "プラグインを初期化中...".to_string()),
        _ => {
            let f = app.loudness.report.as_ref().map_or(0.0, |r| r.progress());
            (f, format!("解析中 {:.0}%", f * 100.0))
        }
    };
    let bar = Rect { x: rect.x + PAD, y, w: (rect.w - PAD * 2.0).max(1.0), h: 10.0 };
    ui.panel("loudness_bar_bg", bar, p.inset_bg, 2.0);
    if frac > 0.0 {
        ui.panel(
            "loudness_bar_fill",
            Rect { x: bar.x, y: bar.y, w: bar.w * frac, h: bar.h },
            p.accent,
            2.0,
        );
    }
    ui.label_at("loudness_bar_label", &label, bar.x, bar.y + bar.h + 2.0, 11.0, p.text_dim);
    y + bar.h + 16.0
}

/// 数値表。位置を持つ行は「@ 12.3s」ボタンでその位置へ飛べる。
fn draw_values(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect, y: f32) -> f32 {
    let p = &app.theme.core;
    let Some(r) = app.loudness.report.as_ref() else {
        ui.label_at(
            "loudness_empty",
            "「解析...」で範囲を選ぶと、その区間のラウドネスを全速で測ります。",
            rect.x + PAD,
            y + 4.0,
            12.0,
            p.text_dim,
        );
        return y + ROW_H * 2.0;
    };
    let target = app.ui_prefs.meter_settings.loudness_target_lufs;
    let ceiling = app.ui_prefs.meter_settings.loudness_true_peak_ceiling_dbtp;

    let mut row = y;
    let mut put = |ui: &mut Ui<'_, AppData>,
                   id: &'static str,
                   label: &str,
                   value: String,
                   value_color: Color,
                   note: Option<String>,
                   at: Option<f32>| {
        ui.label_at((id, "l"), label, rect.x + PAD, row + 3.0, 12.0, p.text_dim);
        ui.label_at(
            (id, "v"),
            &value,
            rect.x + PAD + LABEL_W,
            row + 3.0,
            12.0,
            value_color,
        );
        let mut x = rect.x + PAD + LABEL_W + VALUE_W;
        if let Some(secs) = at {
            let br = Rect { x, y: row, w: POS_W - 6.0, h: ROW_H - 2.0 };
            if ui.button_at_clicked((id, "at"), &format!("@ {secs:.2} 秒"), br) {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SeekToLoudnessPosition(secs))
                }));
            }
        }
        x += POS_W;
        if let Some(n) = note {
            // 注記は窓幅次第で入り切らない (最小幅 560px では残り 176px)。
            // `label_at` は省略しないので、窓の外へ書き出さないよう clip 版を使う。
            ui.label_at_clipped(
                (id, "n"),
                &n,
                Rect { x, y: row + 3.0, w: (rect.x + rect.w - PAD - x).max(0.0), h: ROW_H },
                11.0,
                p.text_dim,
            );
        }
        row += ROW_H;
    };

    let lufs = |v: f32| {
        if v.is_finite() { format!("{v:.1} LUFS") } else { "—".to_string() }
    };
    // Integrated: 目標との差 (= 何 dB 上げ下げすればよいか) を併記する。
    let gain_note = r.normalization_gain_db(target).map(|g| {
        format!("目標 {target:.1} LUFS に対して {:+.1} dB", g)
    });
    put(
        ui,
        "lr_i",
        "Integrated",
        lufs(r.integrated_lufs),
        p.text,
        gain_note,
        None,
    );
    put(
        ui,
        "lr_lra",
        "LRA",
        if r.lra_lu > 0.0 { format!("{:.1} LU", r.lra_lu) } else { "—".to_string() },
        p.text,
        Some(if r.lra_provisional {
            "ラウドネスレンジ (60 秒未満なので暫定値)".to_string()
        } else {
            "ラウドネスレンジ".to_string()
        }),
        None,
    );
    put(
        ui,
        "lr_m",
        "最大 Momentary",
        lufs(r.max_momentary_lufs),
        p.text,
        None,
        r.max_momentary_at_secs,
    );
    put(
        ui,
        "lr_s",
        "最大 Short-term",
        lufs(r.max_short_term_lufs),
        p.text,
        None,
        r.max_short_term_at_secs,
    );
    let tp_over = r.true_peak_dbtp.is_finite() && r.true_peak_dbtp > ceiling;
    put(
        ui,
        "lr_tp",
        "True Peak",
        if r.true_peak_dbtp.is_finite() {
            format!("{:.2} dBTP", r.true_peak_dbtp)
        } else {
            "—".to_string()
        },
        if tp_over { p.text_error } else { p.text },
        tp_over.then(|| format!("上限 {ceiling:.1} dBTP を超過")),
        r.true_peak_at_secs,
    );
    put(
        ui,
        "lr_sp",
        "Sample Peak",
        if r.sample_peak_dbfs.is_finite() {
            format!("{:.2} dBFS", r.sample_peak_dbfs)
        } else {
            "—".to_string()
        },
        p.text,
        None,
        r.sample_peak_at_secs,
    );
    put(
        ui,
        "lr_clip",
        "クリップ",
        format!("{} サンプル", r.clipped_samples),
        if r.clipped_samples > 0 { p.text_error } else { p.text },
        None,
        None,
    );
    put(
        ui,
        "lr_len",
        "測定長",
        format!("{:.2} 秒", r.measured_secs),
        p.text,
        None,
        None,
    );
    row + 6.0
}

/// 配信ターゲットのプリセット行。適合しているものに ○ を付ける。
fn draw_presets(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect, y: f32) -> f32 {
    let p = &app.theme.core;
    ui.label_at("lr_preset_label", "目標", rect.x + PAD, y + 4.0, 12.0, p.text_dim);
    let target = app.ui_prefs.meter_settings.loudness_target_lufs;
    let ceiling = app.ui_prefs.meter_settings.loudness_true_peak_ceiling_dbtp;
    let report = app.loudness.report.as_ref();

    let mut x = rect.x + PAD + 40.0;
    let avail = rect.x + rect.w - PAD - x;
    let bw = (avail / TARGET_PRESETS.len() as f32 - 4.0).clamp(60.0, 116.0);
    // 同じ値のプリセットが複数ある (Spotify と YouTube はどちらも -14 / -1.0) ので、
    // 「選択中」は **最初に一致した 1 つだけ** に付ける。2 本点くと「どちらが
    // 効いているのか」が読めない。
    let selected_idx = TARGET_PRESETS
        .iter()
        .position(|&(_, l, d)| (target - l).abs() < 0.05 && (ceiling - d).abs() < 0.05);
    for (i, &(name, lufs, dbtp)) in TARGET_PRESETS.iter().enumerate() {
        let br = Rect { x, y, w: bw, h: BTN_H };
        // 測定済みなら適合を ○/× で添える (Ardour の Conformity 相当)。
        let mark = report.map_or("", |r| {
            if !r.integrated_lufs.is_finite() {
                ""
            } else if (r.integrated_lufs - lufs).abs() <= 1.0
                && (!r.true_peak_dbtp.is_finite() || r.true_peak_dbtp <= dbtp)
            {
                " ○"
            } else {
                " ×"
            }
        });
        // 選択中 (= 現在の目標値と一致) はボタン下端の accent バーで示す。
        // ボタン背景の裏に敷くと button 自身の塗りに覆われて見えない。
        if selected_idx == Some(i) {
            ui.panel(
                ("lr_preset_sel", i),
                Rect { x, y: y + BTN_H, w: bw, h: 2.0 },
                p.accent,
                0.0,
            );
        }
        if ui.button_at_clicked_sized(("lr_preset", i), &format!("{name}{mark}"), br, 12.0) {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetLoudnessTarget {
                    lufs,
                    ceiling_dbtp: dbtp,
                })
            }));
        }
        x += bw + 4.0;
    }
    y + BTN_H + 8.0
}

/// 時系列グラフ + ヒストグラム (縦軸 = LUFS を共有して横に並べる)。
fn draw_graphs(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect, y: f32) {
    let p = &app.theme.core;
    let bottom = rect.y + rect.h - PAD;
    let h = bottom - y;
    if h < GRAPH_MIN_H {
        return;
    }
    let Some(r) = app.loudness.report.as_ref() else {
        return;
    };
    let target = app.ui_prefs.meter_settings.loudness_target_lufs;
    // 表示レンジは目標を中心に上下へ取る (目標線が必ず枠内に入る)。
    let style = LoudnessGraphStyle {
        range_lufs: (target - 30.0, target + 12.0),
        ..LoudnessGraphStyle::from_palette(p)
    };
    let graph = Rect {
        x: rect.x + PAD,
        y,
        w: (rect.w - PAD * 2.0 - HIST_W - 6.0).max(40.0),
        h,
    };
    if let Some(frac) = ui.loudness_graph(
        "lr_graph",
        graph,
        &r.short_term_curve,
        &r.momentary_curve,
        Some(target),
        &style,
    ) {
        // クリック位置 (0..1) → 範囲先頭からの秒。
        let secs = frac * (r.total_frames as f32 / r.sample_rate.max(1) as f32);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SeekToLoudnessPosition(secs))
        }));
    }
    ui.loudness_histogram(
        "lr_hist",
        Rect { x: graph.x + graph.w + 6.0, y, w: HIST_W, h },
        &r.histogram,
        LOUDNESS_HISTOGRAM_MIN_LUFS,
        LOUDNESS_HISTOGRAM_STEP_LU,
        &style,
    );
}

/// 拍 → 1-based「小節.拍」表記 (ルーラー / レンジピッカーと同じ流儀)。
fn bar_beat(beat: f64, time_sig: (u8, u8)) -> String {
    let bpb = common::timing::beats_per_bar(time_sig).max(1.0);
    let bar = (beat / bpb).floor();
    let within = beat - bar * bpb;
    format!("{}.{}", bar as i64 + 1, within.floor() as i64 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 小節拍表記は_1_始まり() {
        // 4/4 で 0 拍 = 1.1、4 拍 = 2.1、5 拍 = 2.2。
        assert_eq!(bar_beat(0.0, (4, 4)), "1.1");
        assert_eq!(bar_beat(4.0, (4, 4)), "2.1");
        assert_eq!(bar_beat(5.0, (4, 4)), "2.2");
        // 3/4 は 1 小節 3 拍。
        assert_eq!(bar_beat(3.0, (3, 4)), "2.1");
    }
}
