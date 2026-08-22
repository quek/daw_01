// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! F1 で開く「ショートカット / マウス操作 一覧」オーバーレイ。
//!
//! キーボードショートカットは `shortcuts::SHORTCUTS` (SSoT) をカテゴリ別に表示する。
//! マウス操作は判定が gui_01 widget 内のヒットテスト + state machine に埋まっていて
//! データ化できないので、本モジュールに「操作 → 説明」の一覧 [`MOUSE_GESTURES`] を
//! 持つ (= 操作ドキュメント。判定ロジックとは別物)。
//!
//! 表示のみ (将来 rebind 可能にする際は `SHORTCUTS` テーブルが入口)。`is_help_open` を
//! `modal` の open/close と同期させ、F1 / Esc / 画面外クリックで閉じる
//! (`recovery_modal` と同じ idiom)。modal capture の内側で `take_shortcut("daw.toggle_help")`
//! を拾うので、background が遮断されても F1 でトグル close できる。

use daw_ui_core::{Edit, ModalStyle, Ui};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::app::{AppData, AppEvent};
use crate::view::shortcuts::{SHORTCUTS, ShortcutCategory};

const MODAL_ID: &str = "shortcuts_help";

/// r.md #48: このヘルプ窓は以前 theme を import すらしておらず、色を全部直書きしていた
/// (= テーマを切り替えてもここだけ暗いまま残る唯一の view だった)。色は `app.theme` から取る。
fn modal_style(theme: &crate::theme::Theme) -> ModalStyle {
    ModalStyle {
        overlay_color: theme.core.backdrop,
        panel_bg: theme.core.panel,
        panel_radius: 8.0,
        close_on_outside_click: true,
        close_on_escape: true,
    }
}

const PAD: f32 = 22.0;
const TITLE_H: f32 = 44.0;
const HEADER_H: f32 = 26.0;
const ROW_H: f32 = 21.0;
const SECTION_GAP: f32 = 14.0;
const COL_GAP: f32 = 26.0;
/// キー / ジェスチャ表記の列幅 (説明はこの右から始まる)。
const KEY_COL_W: f32 = 178.0;
const N_COLS: usize = 2;

/// マウス操作のカテゴリ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseCategory {
    Select,
    Arrange,
    PianoRoll,
    Automation,
    Mixer,
    Zoom,
    Preview,
    Import,
}

impl MouseCategory {
    fn label(self) -> &'static str {
        match self {
            Self::Select => "選択・再生位置 (マウス)",
            Self::Arrange => "アレンジ (マウス)",
            Self::PianoRoll => "ピアノロール (マウス)",
            Self::Automation => "オートメーション (マウス)",
            Self::Mixer => "ミキサー・ノブ (マウス)",
            Self::Zoom => "ズーム・スクロール",
            Self::Preview => "映像・立ち絵プレビュー",
            Self::Import => "読み込み",
        }
    }
}

/// マウス操作 1 件 (操作ドキュメント)。
struct MouseGestureDef {
    category: MouseCategory,
    gesture: &'static str,
    description: &'static str,
}

/// 全マウス操作。カテゴリ順 = 表示順。
static MOUSE_GESTURES: &[MouseGestureDef] = &[
    // 選択・再生位置
    MouseGestureDef { category: MouseCategory::Select, gesture: "クリック", description: "選択 (上書き)" },
    MouseGestureDef { category: MouseCategory::Select, gesture: "Shift+クリック", description: "選択に追加" },
    MouseGestureDef { category: MouseCategory::Select, gesture: "Ctrl+クリック", description: "選択をトグル" },
    MouseGestureDef { category: MouseCategory::Select, gesture: "ドラッグ (空き)", description: "矩形選択" },
    MouseGestureDef { category: MouseCategory::Select, gesture: "クリック (ルーラー)", description: "再生位置を移動" },
    MouseGestureDef { category: MouseCategory::Select, gesture: "Shift+ドラッグ (ルーラー)", description: "ループ範囲を設定" },
    // アレンジ
    MouseGestureDef { category: MouseCategory::Arrange, gesture: "ドラッグ", description: "クリップを移動" },
    MouseGestureDef { category: MouseCategory::Arrange, gesture: "Ctrl+ドラッグ", description: "クリップを複製 (リンク)" },
    MouseGestureDef { category: MouseCategory::Arrange, gesture: "ドラッグ (端)", description: "クリップの長さを変更" },
    MouseGestureDef { category: MouseCategory::Arrange, gesture: "ドラッグ (トラック下端)", description: "トラックの高さを変更" },
    MouseGestureDef { category: MouseCategory::Arrange, gesture: "右クリック (曲構成帯)", description: "ループ / 削除メニュー" },
    // ピアノロール
    MouseGestureDef { category: MouseCategory::PianoRoll, gesture: "ダブルクリック+ドラッグ", description: "ノートを作成 (長さを決定)" },
    MouseGestureDef { category: MouseCategory::PianoRoll, gesture: "ドラッグ", description: "ノートを移動" },
    MouseGestureDef { category: MouseCategory::PianoRoll, gesture: "Ctrl+ドラッグ", description: "ノートを複製" },
    MouseGestureDef { category: MouseCategory::PianoRoll, gesture: "ドラッグ (端)", description: "ノートの長さを変更" },
    MouseGestureDef { category: MouseCategory::PianoRoll, gesture: "ドラッグ (下部レーン)", description: "ベロシティを変更" },
    // オートメーション
    MouseGestureDef { category: MouseCategory::Automation, gesture: "ダブルクリック (空き)", description: "ポイントを追加" },
    MouseGestureDef { category: MouseCategory::Automation, gesture: "ダブルクリック (点)", description: "値を入力" },
    MouseGestureDef { category: MouseCategory::Automation, gesture: "ドラッグ (点)", description: "ポイントを移動" },
    MouseGestureDef { category: MouseCategory::Automation, gesture: "Alt+クリック (点)", description: "ポイントを削除" },
    MouseGestureDef { category: MouseCategory::Automation, gesture: "ドラッグ (線の中央)", description: "カーブの曲率を調整" },
    MouseGestureDef { category: MouseCategory::Automation, gesture: "右クリック (点)", description: "カーブの種類を選択" },
    // ミキサー・ノブ
    MouseGestureDef { category: MouseCategory::Mixer, gesture: "ドラッグ", description: "ノブ・数値を増減" },
    MouseGestureDef { category: MouseCategory::Mixer, gesture: "ダブルクリック", description: "既定値にリセット" },
    // ズーム・スクロール
    MouseGestureDef { category: MouseCategory::Zoom, gesture: "ホイール", description: "スクロール" },
    MouseGestureDef { category: MouseCategory::Zoom, gesture: "Shift+ホイール", description: "横スクロール" },
    MouseGestureDef { category: MouseCategory::Zoom, gesture: "Ctrl+ホイール", description: "ピアノロールを縦ズーム" },
    // 映像・立ち絵プレビュー
    MouseGestureDef { category: MouseCategory::Preview, gesture: "ドラッグ (枠)", description: "移動" },
    MouseGestureDef { category: MouseCategory::Preview, gesture: "ドラッグ (四隅)", description: "拡大・縮小" },
    MouseGestureDef { category: MouseCategory::Preview, gesture: "ドラッグ (回転ハンドル)", description: "回転" },
    // 読み込み
    MouseGestureDef { category: MouseCategory::Import, gesture: "ファイルをドロップ", description: "音声・画像・動画を取り込み" },
];

const KBD_ORDER: &[ShortcutCategory] = &[
    ShortcutCategory::File,
    ShortcutCategory::Edit,
    ShortcutCategory::Transport,
    ShortcutCategory::Track,
    ShortcutCategory::ClipNote,
    ShortcutCategory::Automation,
    ShortcutCategory::GridView,
    ShortcutCategory::AudioEditor,
    ShortcutCategory::Help,
];

const MOUSE_ORDER: &[MouseCategory] = &[
    MouseCategory::Select,
    MouseCategory::Arrange,
    MouseCategory::PianoRoll,
    MouseCategory::Automation,
    MouseCategory::Mixer,
    MouseCategory::Zoom,
    MouseCategory::Preview,
    MouseCategory::Import,
];

/// 一覧 1 ブロック (= 1 カテゴリ)。キーボードとマウスを 1 軸で扱う。
#[derive(Clone, Copy)]
enum Section {
    Kbd(ShortcutCategory),
    Mouse(MouseCategory),
}

impl Section {
    fn label(self) -> &'static str {
        match self {
            Self::Kbd(c) => c.label(),
            Self::Mouse(c) => c.label(),
        }
    }

    fn is_mouse(self) -> bool {
        matches!(self, Self::Mouse(_))
    }
}

/// 全 Section を表示順 (キーボード → マウス) で返す。
fn build_sections() -> Vec<Section> {
    let mut v: Vec<Section> = KBD_ORDER.iter().map(|&c| Section::Kbd(c)).collect();
    v.extend(MOUSE_ORDER.iter().map(|&c| Section::Mouse(c)));
    v
}

/// Section に属する (キー/ジェスチャ表記, 説明) 行。
fn section_rows(section: Section) -> Vec<(String, &'static str)> {
    match section {
        Section::Kbd(cat) => SHORTCUTS
            .iter()
            .filter(|d| d.category == cat && !d.hidden)
            .map(|d| (d.keys.join(" / "), d.description))
            .collect(),
        Section::Mouse(cat) => MOUSE_GESTURES
            .iter()
            .filter(|g| g.category == cat)
            .map(|g| (g.gesture.to_string(), g.description))
            .collect(),
    }
}

/// Section を高さバランスで `n_cols` 列に貪欲配分する。返り値は各列の Section index 列。
fn greedy_columns(heights: &[f32], n_cols: usize) -> Vec<Vec<usize>> {
    let total: f32 = heights.iter().sum();
    let target = total / n_cols as f32;
    let mut cols: Vec<Vec<usize>> = vec![Vec::new(); n_cols];
    let mut ci = 0;
    let mut acc = 0.0_f32;
    for (i, &h) in heights.iter().enumerate() {
        if acc > target && ci < n_cols - 1 {
            ci += 1;
            acc = 0.0;
        }
        cols[ci].push(i);
        acc += h;
    }
    cols
}

/// 常時呼び。`is_help_open` を modal の open/close と同期させ、開いている間だけ描画する。
pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, screen: PhysicalSize) {
    if !app.ui_prefs.is_help_open {
        return;
    }
    if !ui.is_modal_open(MODAL_ID) {
        ui.open_modal(MODAL_ID);
    }

    let sw = screen.width as f32;
    let sh = screen.height as f32;
    // 説明列の実効幅 = (pw - PAD*2) / N_COLS - COL_GAP - KEY_COL_W。 旧 1180px 上限では
    // 364px しか取れず、 最長の説明 2 件 (412px) が画面がどれだけ広くても必ず末尾省略
    // されて最後まで読めなかった。 1400px 上限なら 474px 取れて全項目が省略なしで入る
    // (狭いウィンドウでは従来どおり sw*0.94 で縮み、 溢れた分は ellipsis に落ちる)。
    let pw = (sw * 0.94).min(1400.0);
    let ph = (sh * 0.92).min(920.0);

    ui.modal(
        MODAL_ID,
        (pw, ph),
        &modal_style(&app.theme),
        Some(Box::new(|| {
            Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::CloseHelp))
        })),
        |ui, panel| {
            // F1 再押下で閉じる (modal capture の内側なので background が遮断されても拾える)。
            if ui.take_shortcut("daw.toggle_help") {
                ui.close_modal(MODAL_ID);
                return;
            }
            draw_body(app, ui, panel);
        },
    );
}

fn draw_body(app: &AppData, ui: &mut Ui<'_, AppData>, panel: Rect) {
    let p = &app.theme.core;
    ui.label_at(
        "sc_help_title",
        "ショートカット / マウス操作",
        panel.x + PAD,
        panel.y + PAD * 0.7,
        19.0,
        p.text,
    );
    // 閉じ方ヒント (右上、概算右寄せ)。
    let hint = "F1 / Esc / 画面外クリックで閉じる";
    ui.label_at(
        "sc_help_hint",
        hint,
        (panel.x + panel.w - PAD - 232.0).max(panel.x + PAD),
        panel.y + PAD * 0.95,
        12.0,
        p.text_dim,
    );

    let content = Rect {
        x: panel.x + PAD,
        y: panel.y + TITLE_H,
        w: panel.w - PAD * 2.0,
        h: (panel.h - TITLE_H - PAD).max(0.0),
    };

    let sections = build_sections();
    let rows: Vec<Vec<(String, &'static str)>> = sections.iter().map(|s| section_rows(*s)).collect();
    let heights: Vec<f32> = rows
        .iter()
        .map(|r| HEADER_H + r.len() as f32 * ROW_H + SECTION_GAP)
        .collect();
    let cols = greedy_columns(&heights, N_COLS);
    let content_h = cols
        .iter()
        .map(|c| c.iter().map(|&i| heights[i]).sum::<f32>())
        .fold(0.0_f32, f32::max);

    let col_w = content.w / N_COLS as f32;
    let mut id_counter: u32 = 0;

    ui.scroll_area("sc_help_scroll", content, (content.w, content_h), |ui, offset| {
        for (ci, col) in cols.iter().enumerate() {
            let x = content.x + ci as f32 * col_w;
            let mut y = content.y - offset.1;
            for &si in col {
                y = draw_section(
                    app,
                    ui,
                    sections[si],
                    &rows[si],
                    x,
                    y,
                    col_w - COL_GAP,
                    &mut id_counter,
                );
            }
        }
    });
}

/// 1 ブロックを描画して次の y を返す。
#[allow(clippy::too_many_arguments)]
fn draw_section(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    section: Section,
    rows: &[(String, &'static str)],
    x: f32,
    y: f32,
    w: f32,
    counter: &mut u32,
) -> f32 {
    let p = &app.theme.core;
    // キー表記 (ティール) とマウスのジェスチャ表記 (アンバー) を一目で区別する専用トークン。
    let key_color =
        if section.is_mouse() { app.theme.daw.text_gesture } else { app.theme.daw.text_keycap };

    ui.label_at(("sc_h", *counter), section.label(), x, y, 14.5, p.accent);
    *counter += 1;
    // 見出し下線。
    ui.push_rect(RectCommand {
        rect: Rect { x, y: y + HEADER_H - 8.0, w, h: 1.0 },
        fill: p.border,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: None,
    });

    let mut yy = y + HEADER_H;
    for (key, desc) in rows {
        // キー列もクリップ (長い表記が説明列にかぶらないよう保険)。
        ui.label_at_clipped(
            ("sc_k", *counter),
            key,
            Rect { x, y: yy, w: KEY_COL_W - 8.0, h: ROW_H },
            12.0,
            key_color,
        );
        *counter += 1;
        ui.label_at_clipped(
            ("sc_d", *counter),
            desc,
            Rect { x: x + KEY_COL_W, y: yy, w: (w - KEY_COL_W).max(0.0), h: ROW_H },
            12.0,
            p.text,
        );
        *counter += 1;
        yy += ROW_H;
    }
    yy + SECTION_GAP
}

#[cfg(test)]
mod tests {
    use super::*;

    /// マウス操作テーブルの全カテゴリが `MOUSE_ORDER` に載っている (表示漏れ防止)。
    #[test]
    fn every_mouse_category_is_ordered() {
        for g in MOUSE_GESTURES {
            assert!(
                MOUSE_ORDER.contains(&g.category),
                "{:?} が MOUSE_ORDER に無い",
                g.category
            );
        }
    }

    /// 全 Section が 1 行以上を持つ (空セクションの見出しだけ出るのを防ぐ)。
    #[test]
    fn every_section_has_rows() {
        for s in build_sections() {
            assert!(!section_rows(s).is_empty(), "{} が空", s.label());
        }
    }

    /// 貪欲配分が全 Section をちょうど 1 回ずつ列に割り当てる。
    #[test]
    fn greedy_columns_partition_all_sections() {
        let heights = [30.0, 60.0, 20.0, 90.0, 40.0, 50.0];
        let cols = greedy_columns(&heights, N_COLS);
        let mut seen: Vec<usize> = cols.iter().flatten().copied().collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..heights.len()).collect::<Vec<_>>());
    }
}
