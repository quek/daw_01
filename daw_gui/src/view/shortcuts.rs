// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! daw_01 のキーボードショートカット定義 (SSoT)。
//!
//! 全ショートカットは [`SHORTCUTS`] テーブル 1 箇所で `(name, keys, category,
//! description)` を宣言する。実際のキー登録 ([`daw_shortcut_map`]) と F1 の一覧
//! オーバーレイ (`shortcuts_help`) は **どちらもこのテーブルから派生** する。
//! 将来キーバインドを設定可能にする際もこのテーブルが入口になる。
//!
//! `Ui::take_shortcut(name)` で root 末尾 (`root.rs::dispatch_shortcuts`) から拾って
//! AppEvent に変換する。`name` の文字列はそのまま `take_shortcut` の引数になるので
//! 変更しないこと (例: `select_all` / `delete` / `cut` / `copy` / `paste` /
//! `piano_roll.edit_lyric` は `is_typing_only_shortcut` が名前で判定する)。
//!
//! NOTE: `Shortcut::parse` が受理するキー表記は alphanumeric / 特殊キー / F1-F24 /
//! 記号 11 種 (`/` `;` `,` `.` `-` `=` `[` `]` `\` `'` `` ` ``)。`+` は区切り文字。

use daw_ui_core::ShortcutMap;

/// 一覧オーバーレイのカテゴリ。表示はこの enum 単位でグルーピングする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutCategory {
    File,
    Edit,
    Transport,
    Track,
    ClipNote,
    Automation,
    GridView,
    AudioEditor,
    Help,
}

impl ShortcutCategory {
    /// 一覧の見出しに使う日本語ラベル。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::File => "ファイル",
            Self::Edit => "編集",
            Self::Transport => "再生",
            Self::Track => "トラック",
            Self::ClipNote => "クリップ・ノート",
            Self::Automation => "オートメーション",
            Self::GridView => "グリッド・表示",
            Self::AudioEditor => "オーディオエディタ",
            Self::Help => "ヘルプ",
        }
    }
}

/// キーボードショートカット 1 件の定義 (SSoT)。
pub struct ShortcutDef {
    /// `take_shortcut` で使うシンボル名。
    pub name: &'static str,
    /// 割り当てキー。先頭が主表記、以降は alias (例: redo は Ctrl+Shift+Z / Ctrl+Y)。
    pub keys: &'static [&'static str],
    pub category: ShortcutCategory,
    /// 一覧に出す日本語説明。文脈で挙動が変わるものは括弧で補足する。
    pub description: &'static str,
    /// `true` のものは一覧に出さない (開発用など)。キー登録はされる。
    pub hidden: bool,
    /// r.md #36: **プラグインエディタ窓など daw_gui の外にフォーカスがあるとき**も
    /// 効かせるショートカット。 `true` の行のキーだけが plugin-host へ転送対象として
    /// 通知され、 プラグインが消化しなかった場合に daw_gui へ返ってくる。
    ///
    /// ここが「どのキーを外部窓から拾うか」 の唯一の宣言。 plugin-host 側にも
    /// プレビュー窓側にも同じ知識を複製しない。
    ///
    /// 立ててよいのは **プラグインのテキスト入力と競合しても実害が小さく、 かつ
    /// フォーカスがどこにあっても効いてほしい** ものに限る。 素の英数字キー
    /// (P / R / F / S / D / …) はプラグイン自身のショートカットと衝突するので立てない。
    pub forward_from_external_window: bool,
}

/// 全キーボードショートカット。カテゴリ順 = 一覧の表示順。
///
/// ここに 1 行足すだけで「キー登録」と「F1 一覧」の両方に反映される。
pub static SHORTCUTS: &[ShortcutDef] = &[
    // ----- ファイル -----
    ShortcutDef { name: "new", keys: &["Ctrl+N"], category: ShortcutCategory::File, description: "新規プロジェクト", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "open", keys: &["Ctrl+O"], category: ShortcutCategory::File, description: "プロジェクトを開く", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "save", keys: &["Ctrl+S"], category: ShortcutCategory::File, description: "保存", hidden: false, forward_from_external_window: true },
    ShortcutDef { name: "save_as", keys: &["Ctrl+Shift+S"], category: ShortcutCategory::File, description: "名前を付けて保存", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.export_wav", keys: &["Ctrl+E"], category: ShortcutCategory::File, description: "WAV 書き出し", hidden: false, forward_from_external_window: false },
    // ----- 編集 -----
    ShortcutDef { name: "undo", keys: &["Ctrl+Z"], category: ShortcutCategory::Edit, description: "元に戻す", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "redo", keys: &["Ctrl+Shift+Z", "Ctrl+Y"], category: ShortcutCategory::Edit, description: "やり直し", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.toggle_undo_history", keys: &["Ctrl+Alt+Z"], category: ShortcutCategory::Edit, description: "編集履歴パネルを開く / 閉じる", hidden: false, forward_from_external_window: false },
    // r.md #48: 設定 (テーマ選択)。REAPER の Options > Preferences と同じ Ctrl+P。
    ShortcutDef { name: "daw.toggle_settings", keys: &["Ctrl+P"], category: ShortcutCategory::Edit, description: "設定を開く / 閉じる", hidden: false, forward_from_external_window: false },
    // r.md #54: 範囲のラウドネスをオフラインで解析する (WAV 書き出し Ctrl+E と同じ「実行系」)。
    ShortcutDef { name: "daw.analyze_loudness", keys: &["Ctrl+L"], category: ShortcutCategory::File, description: "範囲のラウドネスを解析 (EBU R128)", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "cut", keys: &["Ctrl+X"], category: ShortcutCategory::Edit, description: "カット (選択中の面)", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "copy", keys: &["Ctrl+C"], category: ShortcutCategory::Edit, description: "コピー (選択中の面)", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "paste", keys: &["Ctrl+V"], category: ShortcutCategory::Edit, description: "ペースト", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "select_all", keys: &["Ctrl+A"], category: ShortcutCategory::Edit, description: "すべて選択 (文脈依存)", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "delete", keys: &["Delete"], category: ShortcutCategory::Edit, description: "削除 (選択中の面)", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "escape", keys: &["Esc"], category: ShortcutCategory::Edit, description: "閉じる / 選択解除 / 編集をキャンセル", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "tab_next", keys: &["Tab"], category: ShortcutCategory::Edit, description: "次の入力欄へ", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "tab_prev", keys: &["Shift+Tab"], category: ShortcutCategory::Edit, description: "前の入力欄へ", hidden: false, forward_from_external_window: false },
    // ----- 再生 -----
    ShortcutDef { name: "daw.play_toggle", keys: &["Space"], category: ShortcutCategory::Transport, description: "再生 / 停止", hidden: false, forward_from_external_window: true },
    ShortcutDef { name: "daw.toggle_loop", keys: &["P"], category: ShortcutCategory::Transport, description: "ループ ON / OFF", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.loop_selected_clip", keys: &["R"], category: ShortcutCategory::Transport, description: "選択クリップの範囲をループして再生 (再押下で解除)", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.play_from_cursor", keys: &["F"], category: ShortcutCategory::Transport, description: "カーソル位置から再生 (Alt で吸着なし)", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.goto_timeline_home", keys: &["Home"], category: ShortcutCategory::Transport, description: "プレイヘッドを最後のクリップ先頭へ (再押下で 1.1.1)", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.goto_timeline_end", keys: &["End"], category: ShortcutCategory::Transport, description: "プレイヘッドを最後のクリップの後ろへ", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.cycle_arrange_follow", keys: &["Alt+F"], category: ShortcutCategory::Transport, description: "再生追従スクロール: OFF → 連続 → ページ を循環", hidden: false, forward_from_external_window: false },
    // ----- トラック -----
    ShortcutDef { name: "daw.add_track", keys: &["Ctrl+T"], category: ShortcutCategory::Track, description: "トラックを追加", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.group_tracks", keys: &["Ctrl+G"], category: ShortcutCategory::Track, description: "選択トラックをグループ化", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.ungroup_tracks", keys: &["Alt+G"], category: ShortcutCategory::Track, description: "グループを解除", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.toggle_track_solo", keys: &["S"], category: ShortcutCategory::Track, description: "カーソル直下のトラックをソロ切替", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.toggle_mute", keys: &["Q"], category: ShortcutCategory::Track, description: "選択/カーソル下のクリップ・ノートをミュート切替", hidden: false, forward_from_external_window: false },
    // ----- クリップ・ノート -----
    ShortcutDef { name: "daw.duplicate_clip_shared", keys: &["D"], category: ShortcutCategory::ClipNote, description: "クリップ/トラックを複製 (共有・リンク)。ノート選択中はノート複製", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.duplicate_clip_unique", keys: &["Alt+D"], category: ShortcutCategory::ClipNote, description: "クリップ/トラックを複製 (独立コピー)", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.split_clip_at_cursor", keys: &["E"], category: ShortcutCategory::ClipNote, description: "カーソル位置でクリップを分割", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.split_clip_at_cursor_no_snap", keys: &["Alt+E"], category: ShortcutCategory::ClipNote, description: "クリップを分割 (スナップ無効)", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.glue_selected_clips", keys: &["J"], category: ShortcutCategory::ClipNote, description: "選択した隣接クリップを 1 つに結合", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.rename_clip", keys: &["F2"], category: ShortcutCategory::ClipNote, description: "クリップ / トラック名を変更", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.select_linked_clips", keys: &["Shift+L"], category: ShortcutCategory::ClipNote, description: "同じ内容のリンククリップをまとめて選択", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.quantize_pitches_to_scale", keys: &["Shift+P"], category: ShortcutCategory::ClipNote, description: "選択ノートの音程をスケールに補正", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "add_note", keys: &["Insert"], category: ShortcutCategory::ClipNote, description: "ノートを追加 (ピアノロール)", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "piano_roll.edit_lyric", keys: &["L"], category: ShortcutCategory::ClipNote, description: "歌詞を編集 (ピアノロールでノート 1 つ選択中)", hidden: false, forward_from_external_window: false },
    // ----- オートメーション -----
    ShortcutDef { name: "daw.add_automation_from_last_touched", keys: &["A"], category: ShortcutCategory::Automation, description: "最後に触れたパラメータのレーンを追加", hidden: false, forward_from_external_window: false },
    // ----- グリッド・表示 -----
    ShortcutDef { name: "daw.toggle_snap", keys: &["G"], category: ShortcutCategory::GridView, description: "グリッドスナップ ON / OFF", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.fit_view", keys: &["X"], category: ShortcutCategory::GridView, description: "表示をフィット (直前のズームへ)", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.zoom_selected_clip", keys: &["Z"], category: ShortcutCategory::GridView, description: "選択クリップへ段階ズーム (アレンジ)", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.narrow_grid", keys: &["1"], category: ShortcutCategory::GridView, description: "グリッドを細かく", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.widen_grid", keys: &["2"], category: ShortcutCategory::GridView, description: "グリッドを粗く", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.toggle_triplet", keys: &["3"], category: ShortcutCategory::GridView, description: "三連符グリッド ON / OFF", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.toggle_preview_window", keys: &["F12"], category: ShortcutCategory::GridView, description: "ビデオプレビューウィンドウを開く / 閉じる", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.toggle_master_panel", keys: &["Ctrl+Alt+M"], category: ShortcutCategory::GridView, description: "マスターパネル (フェーダー + 各種メーター) を開く / 閉じる", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.close_all_plugin_editors", keys: &["Ctrl+Shift+W"], category: ShortcutCategory::GridView, description: "開いているプラグインのエディタ窓をまとめて閉じる", hidden: false, forward_from_external_window: true },
    // ----- オーディオエディタ -----
    ShortcutDef { name: "daw.duplicate_audio_event", keys: &["Ctrl+D"], category: ShortcutCategory::AudioEditor, description: "オーディオイベントを複製", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.next_audio_event", keys: &["Ctrl+]"], category: ShortcutCategory::AudioEditor, description: "次のオーディオイベントへ", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.prev_audio_event", keys: &["Ctrl+["], category: ShortcutCategory::AudioEditor, description: "前のオーディオイベントへ", hidden: false, forward_from_external_window: false },
    ShortcutDef { name: "daw.auto_warp_clip", keys: &["Alt+W"], category: ShortcutCategory::AudioEditor, description: "選択オーディオクリップを自動ワープ (transient を拍グリッドに整列)", hidden: false, forward_from_external_window: false },
    // ----- ヘルプ -----
    ShortcutDef { name: "daw.toggle_help", keys: &["F1"], category: ShortcutCategory::Help, description: "このショートカット一覧を開く / 閉じる", hidden: false, forward_from_external_window: false },
    // ----- 開発用 (一覧には出さない) -----
    ShortcutDef { name: "debug_overlay_toggle", keys: &["Ctrl+F1"], category: ShortcutCategory::Help, description: "デバッグオーバーレイ", hidden: true, forward_from_external_window: false },
    // NOTE: `daw.synthesize_vocal` (旧 V) は PR-V4 で builtin VOICEVOX plugin の
    // 自動 synth に置き換わり dispatch から削除された。死蔵 bind なのでこのテーブルにも
    // 載せない (= キー登録もしない)。再 synth は notes を編集すれば自動 trigger される。
];

/// [`SHORTCUTS`] から実際の `ShortcutMap` を構築する。
#[must_use]
pub fn daw_shortcut_map() -> ShortcutMap {
    let mut m = ShortcutMap::new();
    for def in SHORTCUTS {
        for &key in def.keys {
            m.bind(def.name, key);
        }
    }
    m
}

/// r.md #36: `forward_from_external_window` が立った行を
/// `(Win32 chord, shortcut 名)` に展開する。
///
/// **キー表記 → Win32 仮想キーの変換はここ 1 箇所だけ**。 plugin-host には chord
/// (数値) しか渡さないので、 向こう側に「Space = 再生」 のような意味論が漏れない。
/// 逆向き (plugin-host から返ってきた chord → shortcut 名) も同じ表から引く。
#[must_use]
pub fn forwarded_editor_chords() -> Vec<(common::protocol::KeyChord, &'static str)> {
    let mut out = Vec::new();
    for def in SHORTCUTS.iter().filter(|d| d.forward_from_external_window) {
        for &spec in def.keys {
            if let Some(chord) = chord_from_spec(spec) {
                out.push((chord, def.name));
            } else {
                // 表記を増やしたのに変換表を更新し忘れた場合は転送されないだけで
                // 済むが、 気づけないので警告を残す (テストでも固定してある)。
                tracing::warn!(spec, "forwarded shortcut: Win32 仮想キーに変換できない表記");
            }
        }
    }
    out
}

/// `"Ctrl+S"` / `"Space"` 等の shortcut 表記を Win32 chord に変換する。
/// 対応していないキー名は `None` (= 転送対象にしない)。
fn chord_from_spec(spec: &str) -> Option<common::protocol::KeyChord> {
    let mut ctrl = false;
    let mut shift = false;
    let mut alt = false;
    let mut key: Option<&str> = None;
    for tok in spec.split('+') {
        match tok.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "shift" => shift = true,
            "alt" => alt = true,
            // Logo / Super は転送対象にしない (OS 側が奪う)。
            "logo" | "super" | "win" | "cmd" => return None,
            _ => {
                if key.is_some() {
                    return None; // key トークンが 2 つ = 表記異常
                }
                key = Some(tok);
            }
        }
    }
    let vk = win32_vk(key?)?;
    Some(common::protocol::KeyChord { vk, ctrl, shift, alt })
}

/// shortcut のキー表記 → Win32 仮想キーコード。
/// <https://learn.microsoft.com/en-us/windows/win32/inputdev/virtual-key-codes>
fn win32_vk(key: &str) -> Option<u16> {
    let lower = key.to_ascii_lowercase();
    Some(match lower.as_str() {
        "space" => 0x20,
        "enter" | "return" => 0x0D,
        "esc" | "escape" => 0x1B,
        "tab" => 0x09,
        "delete" | "del" => 0x2E,
        "backspace" => 0x08,
        "insert" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        _ => {
            let mut it = lower.chars();
            let c = it.next()?;
            if it.next().is_some() {
                // "F1".."F24"
                let n: u8 = lower.strip_prefix('f')?.parse().ok()?;
                if (1..=24).contains(&n) {
                    return Some(0x70 + u16::from(n) - 1);
                }
                return None;
            }
            if c.is_ascii_alphanumeric() {
                // VK_0-9 / VK_A-Z は ASCII 大文字と同値。
                u16::from(c.to_ascii_uppercase() as u8)
            } else {
                return None;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全エントリのキー表記が parse 可能で、`daw_shortcut_map` が panic せず
    /// 全 (name, key) を登録する。`bind` 内の `Shortcut::parse` は不正表記で panic
    /// するので、この呼び出し自体が parse の網羅検証になる。
    #[test]
    fn daw_shortcut_map_binds_every_entry() {
        let m = daw_shortcut_map();
        let expected: usize = SHORTCUTS.iter().map(|d| d.keys.len()).sum();
        assert_eq!(m.iter().count(), expected, "全 (name, key) が登録される");
    }

    /// 異なる name が同じキー表記を共有していない (先勝ちで片方が死ぬのを防ぐ)。
    #[test]
    fn no_two_shortcuts_share_a_key() {
        let mut seen = std::collections::HashSet::new();
        for def in SHORTCUTS {
            for &key in def.keys {
                assert!(seen.insert(key), "キー {key:?} が重複している ({})", def.name);
            }
        }
    }

    /// 主要な既存バインドが維持されている回帰確認 (テーブル化で漏れていないか)。
    #[test]
    fn well_known_bindings_present() {
        let m = daw_shortcut_map();
        let names: std::collections::HashSet<&str> = m.iter().map(|(_, n)| *n).collect();
        for name in [
            "undo", "redo", "save", "select_all", "delete", "escape",
            "daw.play_toggle", "daw.toggle_help", "daw.duplicate_clip_shared",
            "add_note", "piano_roll.edit_lyric",
        ] {
            assert!(names.contains(name), "{name} が登録されていない");
        }
        // 死蔵 bind は登録しない。
        assert!(!names.contains("daw.synthesize_vocal"), "死蔵 V bind を復活させない");
    }

    // -------- r.md #36: プラグインエディタ窓からの転送 -----------------------

    /// 転送対象は Space (再生 / 停止)、Ctrl+S (保存)、Ctrl+Shift+W
    /// (エディタ窓を全部閉じる) の 3 つだけ。
    ///
    /// 素の英数字キー (P / R / F / S / D / …) を足すとプラグイン自身のショートカットと
    /// 衝突する。 Home / End / Delete / Ctrl+X / Ctrl+C / Ctrl+V / Ctrl+A は
    /// `is_typing_only_shortcut` が既に「テキスト入力へ譲る」 と宣言している集合なので
    /// 外部窓からも奪わない。 Esc / Tab / F1 はプラグイン自身のポップアップ閉じ /
    /// フォーカス移動 / ヘルプ。 この境界が緩むのを防ぐ。
    ///
    /// r.md #55 の Ctrl+Shift+W は「エディタ窓を操作している最中に押したい」 操作
    /// そのものなので転送必須。 Ctrl+Shift 付きなのでプラグインのテキスト入力とは
    /// 衝突しない。
    #[test]
    fn forwarded_set_is_transport_save_and_close_editors() {
        let names: Vec<&str> = forwarded_editor_chords().into_iter().map(|(_, n)| n).collect();
        assert_eq!(names, vec!["save", "daw.play_toggle", "daw.close_all_plugin_editors"]);
    }

    /// 転送 chord は Win32 仮想キーに正しく変換されている
    /// (plugin-host 側は数値比較しかしないので、 ここがずれると黙って効かなくなる)。
    #[test]
    fn forwarded_chords_map_to_win32_vk() {
        let chords = forwarded_editor_chords();
        let space = chords
            .iter()
            .find(|(_, n)| *n == "daw.play_toggle")
            .expect("play_toggle が転送対象")
            .0;
        assert_eq!(space.vk, 0x20, "VK_SPACE");
        assert!(!space.ctrl && !space.shift && !space.alt);
        let save = chords.iter().find(|(_, n)| *n == "save").expect("save が転送対象").0;
        assert_eq!(save.vk, u16::from(b'S'), "VK_S");
        assert!(save.ctrl && !save.shift && !save.alt);
    }

    /// 転送対象に挙げた行のキー表記が全て Win32 仮想キーに変換できる
    /// (変換できないと黙って転送されないだけなので、 テストで固定する)。
    #[test]
    fn every_forwarded_key_spec_converts() {
        for def in SHORTCUTS.iter().filter(|d| d.forward_from_external_window) {
            for &spec in def.keys {
                assert!(
                    chord_from_spec(spec).is_some(),
                    "{} の {spec:?} が Win32 仮想キーに変換できない",
                    def.name
                );
            }
        }
    }

    /// chord → shortcut 名の逆引きが一意 (runner が `find` で引くので衝突すると誤発火する)。
    #[test]
    fn forwarded_chords_are_unique() {
        let chords = forwarded_editor_chords();
        let mut seen = std::collections::HashSet::new();
        for (c, name) in &chords {
            assert!(seen.insert(*c), "chord {c:?} が重複 ({name})");
        }
    }
}
