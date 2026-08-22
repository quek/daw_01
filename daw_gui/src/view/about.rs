// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! ヘルプ > バージョン情報 (About) オーバーレイ (r.md #60)。
//!
//! GPLv3 §0 の "Appropriate Legal Notices" は、対話的 UI に
//! 「**便利かつ目立つ形で** (1) 適切な著作権表示 (2) 無保証である旨・本ライセンスの下で
//! 再頒布してよい旨・**ライセンス本文の閲覧方法** を示す機能」を求める。GPL 本文の
//! "How to Apply These Terms" は GUI ならこれを about box でやれと名指ししている。
//! さらに FFmpeg の legal.html は「LGPL の下で FFmpeg を使っていることを about box に
//! 明記せよ」と求める。この 1 画面が両方の受け皿。
//!
//! **SSoT**: 表示内容をここに書き写さない。
//! - 第三者コンポーネントの帰属 → リポジトリの `NOTICE` を `include_str!` でそのまま表示
//! - GPL 本文 → リポジトリの `LICENSE` を `include_str!` でそのまま表示
//!   (URL 参照ではなく本文を exe に埋め込む。GPL FAQ が「5 年後 10 年後に URL が
//!   生きている保証はない」として同梱を求めている)
//! - FFmpeg のバージョン / ライセンス / configure 行 → **実行中の DLL に問い合わせる**
//!   (`av_version_info` / `avutil_license` / `avutil_configuration`)。ハードコードすると
//!   差し替えたビルドと表示が食い違う
//!
//! 開閉は `ui_prefs.is_about_open` と modal の open/close を同期させる
//! (`shortcuts_help` と同じ idiom)。Esc / 画面外クリック / ✕ で閉じる。

use std::sync::LazyLock;

use daw_ui_core::{Edit, ModalStyle, Ui};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Rect, RectCommand};

use crate::app::{AppData, AppEvent};

const MODAL_ID: &str = "about";

/// 第三者コンポーネントの帰属 (Apache-2.0 §4(d) / LGPL の prominent notice の受け皿)。
const NOTICE_TEXT: &str = include_str!("../../../NOTICE");
/// GNU GPL v3 全文。ALN の「how to view a copy of this License」をオフラインで満たす。
const LICENSE_TEXT: &str = include_str!("../../../LICENSE");

const SOURCE_URL: &str = env!("CARGO_PKG_REPOSITORY");
const VERSION: &str = env!("CARGO_PKG_VERSION");

const PAD: f32 = 22.0;
const TITLE_H: f32 = 44.0;
const LINE_H: f32 = 16.0;
const BODY_FONT: f32 = 12.0;
/// 実行中の FFmpeg の configure 行など、長い 1 行を折り返す文字数の目安。
const WRAP_COLS: usize = 108;

/// 3 つのタブの本文は**一度だけ**組み立てる。中身は起動中ずっと不変 (埋め込んだ
/// `NOTICE` / `LICENSE` と、プロセス生存中は変わらない FFmpeg のバージョン情報) なのに、
/// 毎フレーム組み直すと GPL 全文だけで 674 個の `String` を 60fps で確保することになる
/// (= 描画ループのヒープ確保。CLAUDE.md のパフォーマンス方針に反する)。
static OVERVIEW_LINES: LazyLock<Vec<String>> = LazyLock::new(overview_lines);
static THIRD_PARTY_LINES: LazyLock<Vec<String>> = LazyLock::new(third_party_lines);
static LICENSE_LINES: LazyLock<Vec<String>> =
    LazyLock::new(|| LICENSE_TEXT.lines().map(str::to_string).collect());

/// 概要タブの本文。GPLv3 §0 が要求する 4 要素をすべてこの中に置く。
fn overview_lines() -> Vec<String> {
    let mut v: Vec<String> = vec![
        format!("daw_01  version {VERSION}"),
        "VOICEVOX 歌声合成を組み込んだ Rust 製 DAW".to_string(),
        String::new(),
        // (1) 著作権表示
        "Copyright (C) 2026 Tahara Yoshinori".to_string(),
        String::new(),
        // (2) 無保証 / (3) 再頒布してよい旨 — GPL 本文の interactive 版警告文をそのまま。
        "This program comes with ABSOLUTELY NO WARRANTY.".to_string(),
        "This is free software, and you are welcome to redistribute it under".to_string(),
        "the terms of the GNU General Public License, either version 3 of the".to_string(),
        "License, or (at your option) any later version.".to_string(),
        String::new(),
        "この プログラムは 完全に無保証です。GNU General Public License version 3".to_string(),
        "またはそれ以降の条件の下で、自由に再頒布・改変できます。".to_string(),
        String::new(),
        // (4) ライセンス本文の閲覧方法
        "ライセンス全文: 「ライセンス全文 (GPL-3.0)」タブ、またはリポジトリの LICENSE".to_string(),
        "第三者コンポーネントの帰属: 「第三者コンポーネント」タブ (= リポジトリの NOTICE)".to_string(),
        String::new(),
        format!("ソースコード: {SOURCE_URL}"),
        String::new(),
        "この製品は FFmpeg プロジェクトのライブラリを LGPL v3 の下で利用しています".to_string(),
        "(https://www.gnu.org/licenses/lgpl-3.0.html)。FFmpeg のソースは".to_string(),
        "https://ffmpeg.org/download.html から入手できます。".to_string(),
        String::new(),
        "VOICEVOX は別個のプログラムです。daw_01 には VOICEVOX のコード・音声モデル・".to_string(),
        "キャラクター音声は一切含まれません。生成した音声の利用は VOICEVOX と各".to_string(),
        "キャラクターの利用規約 (https://voicevox.hiroshiba.jp/term/) に従ってください。".to_string(),
    ];
    let ffmpeg = ffmpeg_runtime_lines();
    if !ffmpeg.is_empty() {
        v.push(String::new());
        v.push("--- 実行中の FFmpeg (ライブラリに問い合わせた実測値) ---".to_string());
        v.extend(ffmpeg);
    }
    v
}

/// 第三者コンポーネントタブ = 実行中 FFmpeg の実測値 + `NOTICE` 全文。
fn third_party_lines() -> Vec<String> {
    let mut v = Vec::new();
    let ffmpeg = ffmpeg_runtime_lines();
    if !ffmpeg.is_empty() {
        v.push("--- 実行中の FFmpeg (ライブラリに問い合わせた実測値) ---".to_string());
        v.extend(ffmpeg);
        v.push(String::new());
    }
    v.extend(NOTICE_TEXT.lines().map(str::to_string));
    v
}

/// リンクしている FFmpeg 共有ライブラリ自身から、版・ライセンス・configure 行を取る。
/// ハードコードしないので、ユーザーが DLL を差し替えても表示が実物と一致する
/// (FFmpeg legal.html の「どうコンパイルしたか、例えば configure 行を示せ」に対応)。
#[cfg(windows)]
fn ffmpeg_runtime_lines() -> Vec<String> {
    use std::ffi::CStr;
    use std::os::raw::c_char;

    fn text(p: *const c_char) -> String {
        if p.is_null() {
            return "(取得できません)".to_string();
        }
        // SAFETY: これらは FFmpeg が静的に持つ NUL 終端文字列へのポインタを返す
        // (avutil の av_version_info / avutil_license / avutil_configuration)。
        // 呼び出し側は解放しない。null は上で弾いている。
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }

    // SAFETY: いずれも引数なし・副作用なしの問い合わせ関数。DLL は exe の隣に置かれ、
    // rsmpeg 経由で既にロードされている。
    let (version, license, configuration) = unsafe {
        (
            text(rsmpeg::ffi::av_version_info()),
            text(rsmpeg::ffi::avutil_license()),
            text(rsmpeg::ffi::avutil_configuration()),
        )
    };

    let mut v = vec![format!("version : {version}"), format!("license : {license}")];
    let wrapped = wrap(&configuration, WRAP_COLS);
    for (i, line) in wrapped.iter().enumerate() {
        v.push(format!("{} {line}", if i == 0 { "config  :" } else { "         " }));
    }
    v
}

#[cfg(not(windows))]
fn ffmpeg_runtime_lines() -> Vec<String> {
    Vec::new()
}

/// 空白区切りで `cols` 文字を目安に折り返す。configure 行のような 1 本の長文用。
fn wrap(s: &str, cols: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > cols {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

fn modal_style(theme: &crate::theme::Theme) -> ModalStyle {
    ModalStyle {
        overlay_color: theme.core.backdrop,
        panel_bg: theme.core.panel,
        panel_radius: 8.0,
        close_on_outside_click: true,
        close_on_escape: true,
    }
}

/// 常時呼び。`is_about_open` を modal の open/close と同期させ、開いている間だけ描画する。
pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, screen: PhysicalSize) {
    if !app.ui_prefs.is_about_open {
        return;
    }
    if !ui.is_modal_open(MODAL_ID) {
        ui.open_modal(MODAL_ID);
    }

    let sw = screen.width as f32;
    let sh = screen.height as f32;
    let pw = (sw * 0.92).min(940.0);
    let ph = (sh * 0.92).min(760.0);

    ui.modal(
        MODAL_ID,
        (pw, ph),
        &modal_style(&app.theme),
        Some(Box::new(|| {
            Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::CloseAbout))
        })),
        |ui, panel| draw_body(app, ui, panel),
    );
}

fn draw_body(app: &AppData, ui: &mut Ui<'_, AppData>, panel: Rect) {
    let p = &app.theme.core;
    ui.label_at("about_title", "daw_01 について", panel.x + PAD, panel.y + PAD * 0.7, 19.0, p.text);
    ui.label_at(
        "about_hint",
        "Esc / 画面外クリックで閉じる",
        (panel.x + panel.w - PAD - 200.0).max(panel.x + PAD),
        panel.y + PAD * 0.95,
        12.0,
        p.text_dim,
    );

    // ソース URL をコピー: GPLv3 の「ソースの入手方法」を実際に持ち出せるようにする。
    let copy_rect = Rect {
        x: (panel.x + panel.w - PAD - 200.0 - 190.0).max(panel.x + PAD),
        y: panel.y + PAD * 0.6,
        w: 180.0,
        h: 24.0,
    };
    if ui.button_at_clicked("about_copy_url", "ソース URL をコピー", copy_rect) {
        ui.set_clipboard_text(SOURCE_URL.to_string());
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.ui_ephemeral.status_message = format!("コピー: {SOURCE_URL}");
        }));
    }

    let content = Rect {
        x: panel.x + PAD,
        y: panel.y + TITLE_H,
        w: panel.w - PAD * 2.0,
        h: (panel.h - TITLE_H - PAD).max(0.0),
    };

    ui.tab_view("about_tabs", content, |tabs| {
        tabs.tab("概要", |ui, pane| {
            text_block(app, ui, "about_overview", pane, &OVERVIEW_LINES);
        });
        tabs.tab("第三者コンポーネント", |ui, pane| {
            text_block(app, ui, "about_third_party", pane, &THIRD_PARTY_LINES);
        });
        tabs.tab("ライセンス全文 (GPL-3.0)", |ui, pane| {
            text_block(app, ui, "about_license", pane, &LICENSE_LINES);
        });
    });
}

/// 行の配列をスクロール可能に描く。**表示中の行だけ** widget を作る (GPL 全文は 674 行、
/// NOTICE も 180 行ほどあるので、毎フレーム全行を積むと無駄が大きい)。
fn text_block(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    id: &'static str,
    rect: Rect,
    lines: &[String],
) {
    let p = &app.theme.core;
    // pane の縁を 1 段沈めて「読み物の面」であることを示す。
    ui.push_rect(RectCommand {
        rect,
        fill: p.inset_bg,
        border: p.border,
        border_width: 1.0,
        radius: [4.0; 4],
        clip_rect: None,
    });

    let inner = Rect {
        x: rect.x + 10.0,
        y: rect.y + 8.0,
        w: (rect.w - 20.0).max(0.0),
        h: (rect.h - 16.0).max(0.0),
    };
    let content_h = lines.len() as f32 * LINE_H;
    ui.scroll_area(id, inner, (inner.w, content_h), |ui, offset| {
        let first = (offset.1 / LINE_H).floor().max(0.0) as usize;
        let visible = (inner.h / LINE_H).ceil() as usize + 2;
        for (i, line) in lines.iter().enumerate().skip(first).take(visible) {
            if line.is_empty() {
                continue;
            }
            let y = inner.y + i as f32 * LINE_H - offset.1;
            ui.label_at_clipped(
                (id, i),
                line,
                Rect { x: inner.x, y, w: inner.w, h: LINE_H },
                BODY_FONT,
                p.text,
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GPLv3 §0 が対話的 UI に要求する 4 要素が概要タブに載っている
    /// (著作権表示 / 無保証 / 本ライセンスの下で再頒布してよい旨 / ライセンス本文の閲覧方法)。
    /// 文面を消しても build も clippy も通ってしまうので、ここで固定する。
    #[test]
    fn overview_carries_appropriate_legal_notices() {
        let text = OVERVIEW_LINES.join("\n");
        for needle in [
            "Copyright (C) 2026 Tahara Yoshinori",
            "ABSOLUTELY NO WARRANTY",
            "redistribute",
            "GNU General Public License",
            "ライセンス全文",
            "LGPL v3",
        ] {
            assert!(text.contains(needle), "About の概要タブに {needle:?} が無い");
        }
        assert!(text.contains(SOURCE_URL), "ソース入手先 URL が無い");
    }

    /// 埋め込んだ LICENSE / NOTICE が実物であること (パスがずれて空を埋め込む事故を防ぐ)。
    #[test]
    fn embedded_documents_are_the_real_ones() {
        assert!(LICENSE_TEXT.contains("GNU GENERAL PUBLIC LICENSE"));
        assert!(LICENSE_TEXT.contains("Version 3, 29 June 2007"));
        assert!(NOTICE_TEXT.contains("Celemony Software GmbH"));
        assert!(NOTICE_TEXT.contains("FFmpeg"));
    }

    #[test]
    fn wrap_splits_on_whitespace_without_losing_words() {
        let src = "--enable-shared --disable-static --enable-version3 --disable-libx264";
        let out = wrap(src, 40);
        assert_eq!(out.len(), 2);
        assert_eq!(out.join(" "), src);
        assert!(out.iter().all(|l| l.chars().count() <= 40));
    }
}
