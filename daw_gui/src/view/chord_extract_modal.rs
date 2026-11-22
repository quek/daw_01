//! 「コードを抽出…」の設定ダイアログ (r.md #157 §3.4、Cubase の Create Chord Events に相当)。
//!
//! クリップの右クリック「コードを抽出…」で開く (`ChordToolEvent::OpenExtractDialog`)。設定は
//! `ViewState.chord_tools.detect` (保存する道具の設定) をその場で書き換え (`SetDetectOptions`)、
//! 「抽出」で `ExtractChords` (MIDI はノートから、オーディオは音源から)。閉じ方は 抽出 / キャンセル / Esc
//! (書き出し範囲の modal と同じ、`close_on_escape: false` で body が Esc を拾う)。

use common::chord_detect::DetectOptions;
use daw_ui_core::{Edit, ModalStyle, Ui};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent};
use crate::event_chord::ChordToolEvent;

const MODAL_ID: &str = "chord_extract";
const PANEL_W: f32 = 380.0;
const PANEL_H: f32 = 236.0;
const PAD: f32 = 18.0;
const ROW_H: f32 = 26.0;
const LABEL_W: f32 = 150.0;
const BTN_W: f32 = 110.0;
const BTN_H: f32 = 28.0;

/// 1 コードの長さの選択肢 (表示名, 長さ)。先頭の「自動」はノートの鳴り始めで区切る
/// (`DetectOptions::auto_length`、既定)。ほかは固定の窓 (小節は拍子から)。
const WINDOWS: &[(&str, WindowLen)] = &[
    ("自動", WindowLen::Auto),
    ("1/2 拍", WindowLen::Beats(0.5)),
    ("1 拍", WindowLen::Beats(1.0)),
    ("2 拍", WindowLen::Beats(2.0)),
    ("1 小節", WindowLen::Bars(1.0)),
    ("2 小節", WindowLen::Bars(2.0)),
];

/// 短いノートを無視するしきい値 (表示名, 拍)。
const IGNORE: &[(&str, f64)] = &[("無視しない", 0.0), ("1/32 未満", 0.125), ("1/16 未満", 0.25), ("1/8 未満", 0.5), ("1/4 未満", 1.0)];

#[derive(Clone, Copy)]
enum WindowLen {
    Auto,
    Beats(f64),
    Bars(f64),
}

impl WindowLen {
    /// 固定の窓の長さ (拍)。`Auto` は `None`。
    fn beats(self, beats_per_bar: f64) -> Option<f64> {
        match self {
            WindowLen::Auto => None,
            WindowLen::Beats(b) => Some(b),
            WindowLen::Bars(n) => Some(n * beats_per_bar),
        }
    }

    /// この選択肢を選んだ設定。
    fn apply(self, opts: DetectOptions, beats_per_bar: f64) -> DetectOptions {
        match self.beats(beats_per_bar) {
            None => DetectOptions { auto_length: true, ..opts },
            Some(window_beats) => DetectOptions { auto_length: false, window_beats, ..opts },
        }
    }

    /// 設定 `opts` がこの選択肢か。
    fn matches(self, opts: &DetectOptions, beats_per_bar: f64) -> bool {
        match self.beats(beats_per_bar) {
            None => opts.auto_length,
            Some(b) => !opts.auto_length && (b - opts.window_beats).abs() < 1e-6,
        }
    }
}

fn set(opts: DetectOptions) -> Edit<AppData> {
    Edit::mutate(move |app: &mut AppData| app.handle_event(AppEvent::ChordTool(ChordToolEvent::SetDetectOptions(opts))))
}

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, _screen: PhysicalSize) {
    let Some(clips) = app.cur.chord_tools.extract_dialog.clone() else {
        if ui.is_modal_open(MODAL_ID) {
            ui.close_modal(MODAL_ID);
        }
        return;
    };
    if !ui.is_modal_open(MODAL_ID) {
        ui.open_modal(MODAL_ID);
    }
    let opts = app.cur.chord_tools.view.detect;
    let bpb = common::timing::beats_per_bar(app.cur.song_doc.song().time_sig);
    let window_labels: Vec<&str> = WINDOWS.iter().map(|(l, _)| *l).collect();
    let window_sel = WINDOWS.iter().position(|(_, w)| w.matches(&opts, bpb)).unwrap_or(0);
    let ignore_labels: Vec<&str> = IGNORE.iter().map(|(l, _)| *l).collect();
    let ignore_sel = IGNORE.iter().position(|(_, b)| (b - opts.ignore_shorter_than_beats).abs() < 1e-6).unwrap_or(0);
    let style = ModalStyle { close_on_outside_click: false, close_on_escape: false, ..ModalStyle::from_palette(&app.theme.core) };
    ui.modal(MODAL_ID, (PANEL_W, PANEL_H), &style, None, move |ui, panel| {
        let p = ui.palette();
        ui.label_at("cex_title", "コードを抽出", panel.x + PAD, panel.y + PAD, 16.0, p.text);
        if ui.take_shortcut("escape") {
            ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::ChordTool(ChordToolEvent::CloseExtractDialog))));
        }
        let x = panel.x + PAD;
        let full_w = panel.w - PAD * 2.0;
        let field = |y: f32| Rect { x: x + LABEL_W, y, w: full_w - LABEL_W, h: ROW_H };
        // 開いた dropdown の一覧は下の行より手前に描かれ (daw-ui: popup の中の popup)、checkbox / button は
        // 一覧の下ではヒットしない (`pointer_blocked_by_modal_popup`)。
        let mut y = panel.y + PAD + 30.0;
        ui.label_at("cex_win_l", "1 コードの長さ", x, y + 6.0, 13.0, p.text);
        if let Some(i) = ui.dropdown("cex_win", field(y), &window_labels, window_sel) {
            ui.push_edit(set(WINDOWS[i].1.apply(opts, bpb)));
        }
        y += ROW_H + 8.0;
        ui.label_at("cex_ign_l", "短いノートを無視", x, y + 6.0, 13.0, p.text);
        if let Some(i) = ui.dropdown("cex_ign", field(y), &ignore_labels, ignore_sel) {
            ui.push_edit(set(DetectOptions { ignore_shorter_than_beats: IGNORE[i].1, ..opts }));
        }
        y += ROW_H + 10.0;
        let row = |y: f32| Rect { x, y, w: full_w, h: ROW_H };
        ui.checkbox_at("cex_bass", row(y), opts.include_bass, "ベース音を含める (C/E)", move |on| {
            set(DetectOptions { include_bass: on, ..opts })
        });
        y += ROW_H + 4.0;
        ui.checkbox_at("cex_tens", row(y), opts.include_tensions, "テンションを含める (9 / 11 / 13)", move |on| {
            set(DetectOptions { include_tensions: on, ..opts })
        });
        let btn_y = panel.y + panel.h - PAD - BTN_H;
        let ok_x = panel.x + panel.w - PAD - BTN_W;
        if ui.button_at_clicked("cex_ok", "抽出", Rect { x: ok_x, y: btn_y, w: BTN_W, h: BTN_H }) {
            let clips = clips.clone();
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ChordTool(ChordToolEvent::ExtractChords { clips }));
            }));
        }
        if ui.button_at_clicked("cex_cancel", "キャンセル", Rect { x: ok_x - BTN_W - 10.0, y: btn_y, w: BTN_W, h: BTN_H }) {
            ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::ChordTool(ChordToolEvent::CloseExtractDialog))));
        }
    });
}
