//! daw_01 のキーボードショートカット定義 (SSoT)。
//!
//! 全ショートカットは [`SHORTCUTS`] テーブル 1 箇所で `(name, keys, category,
//! description)` を宣言する。実際のキー登録 ([`daw_shortcut_map`]) と F1 の一覧
//! オーバーレイ (`shortcuts_help`) は **どちらもこのテーブルから派生** する
//! (FIXME #91)。将来キーバインドを設定可能にする際もこのテーブルが入口になる。
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
}

/// 全キーボードショートカット。カテゴリ順 = 一覧の表示順。
///
/// ここに 1 行足すだけで「キー登録」と「F1 一覧」の両方に反映される。
pub static SHORTCUTS: &[ShortcutDef] = &[
    // ----- ファイル -----
    ShortcutDef { name: "new", keys: &["Ctrl+N"], category: ShortcutCategory::File, description: "新規プロジェクト", hidden: false },
    ShortcutDef { name: "open", keys: &["Ctrl+O"], category: ShortcutCategory::File, description: "プロジェクトを開く", hidden: false },
    ShortcutDef { name: "save", keys: &["Ctrl+S"], category: ShortcutCategory::File, description: "保存", hidden: false },
    ShortcutDef { name: "save_as", keys: &["Ctrl+Shift+S"], category: ShortcutCategory::File, description: "名前を付けて保存", hidden: false },
    ShortcutDef { name: "daw.export_wav", keys: &["Ctrl+E"], category: ShortcutCategory::File, description: "WAV 書き出し", hidden: false },
    // ----- 編集 -----
    ShortcutDef { name: "undo", keys: &["Ctrl+Z"], category: ShortcutCategory::Edit, description: "元に戻す", hidden: false },
    ShortcutDef { name: "redo", keys: &["Ctrl+Shift+Z", "Ctrl+Y"], category: ShortcutCategory::Edit, description: "やり直し", hidden: false },
    ShortcutDef { name: "cut", keys: &["Ctrl+X"], category: ShortcutCategory::Edit, description: "カット (選択中の面)", hidden: false },
    ShortcutDef { name: "copy", keys: &["Ctrl+C"], category: ShortcutCategory::Edit, description: "コピー (選択中の面)", hidden: false },
    ShortcutDef { name: "paste", keys: &["Ctrl+V"], category: ShortcutCategory::Edit, description: "ペースト", hidden: false },
    ShortcutDef { name: "select_all", keys: &["Ctrl+A"], category: ShortcutCategory::Edit, description: "すべて選択 (文脈依存)", hidden: false },
    ShortcutDef { name: "delete", keys: &["Delete"], category: ShortcutCategory::Edit, description: "削除 (選択中の面)", hidden: false },
    ShortcutDef { name: "escape", keys: &["Esc"], category: ShortcutCategory::Edit, description: "閉じる / 選択解除 / 編集をキャンセル", hidden: false },
    ShortcutDef { name: "tab_next", keys: &["Tab"], category: ShortcutCategory::Edit, description: "次の入力欄へ", hidden: false },
    ShortcutDef { name: "tab_prev", keys: &["Shift+Tab"], category: ShortcutCategory::Edit, description: "前の入力欄へ", hidden: false },
    // ----- 再生 -----
    ShortcutDef { name: "daw.play_toggle", keys: &["Space"], category: ShortcutCategory::Transport, description: "再生 / 停止", hidden: false },
    ShortcutDef { name: "daw.toggle_loop", keys: &["P"], category: ShortcutCategory::Transport, description: "ループ ON / OFF", hidden: false },
    ShortcutDef { name: "daw.loop_selected_clip", keys: &["R"], category: ShortcutCategory::Transport, description: "選択クリップの範囲をループして再生 (再押下で解除)", hidden: false },
    ShortcutDef { name: "daw.play_from_cursor", keys: &["F"], category: ShortcutCategory::Transport, description: "カーソル位置から再生 (Alt で吸着なし)", hidden: false },
    // ----- トラック -----
    ShortcutDef { name: "daw.add_track", keys: &["Ctrl+T"], category: ShortcutCategory::Track, description: "トラックを追加", hidden: false },
    ShortcutDef { name: "daw.group_tracks", keys: &["Ctrl+G"], category: ShortcutCategory::Track, description: "選択トラックをグループ化", hidden: false },
    ShortcutDef { name: "daw.ungroup_tracks", keys: &["Alt+G"], category: ShortcutCategory::Track, description: "グループを解除", hidden: false },
    ShortcutDef { name: "daw.toggle_track_solo", keys: &["S"], category: ShortcutCategory::Track, description: "カーソル直下のトラックをソロ切替", hidden: false },
    ShortcutDef { name: "daw.toggle_mute", keys: &["Q"], category: ShortcutCategory::Track, description: "選択/カーソル下のクリップ・ノートをミュート切替", hidden: false },
    // ----- クリップ・ノート -----
    ShortcutDef { name: "daw.duplicate_clip_shared", keys: &["D"], category: ShortcutCategory::ClipNote, description: "クリップを複製 (共有)。ノート選択中はノート複製", hidden: false },
    ShortcutDef { name: "daw.duplicate_clip_unique", keys: &["Alt+D"], category: ShortcutCategory::ClipNote, description: "クリップを複製 (独立コピー)", hidden: false },
    ShortcutDef { name: "daw.split_clip_at_cursor", keys: &["E"], category: ShortcutCategory::ClipNote, description: "カーソル位置でクリップを分割", hidden: false },
    ShortcutDef { name: "daw.split_clip_at_cursor_no_snap", keys: &["Alt+E"], category: ShortcutCategory::ClipNote, description: "クリップを分割 (スナップ無効)", hidden: false },
    ShortcutDef { name: "daw.glue_selected_clips", keys: &["J"], category: ShortcutCategory::ClipNote, description: "選択した隣接クリップを 1 つに結合", hidden: false },
    ShortcutDef { name: "daw.rename_clip", keys: &["F2"], category: ShortcutCategory::ClipNote, description: "クリップ / トラック名を変更", hidden: false },
    ShortcutDef { name: "daw.select_linked_clips", keys: &["Shift+L"], category: ShortcutCategory::ClipNote, description: "同じ内容のリンククリップをまとめて選択", hidden: false },
    ShortcutDef { name: "daw.quantize_pitches_to_scale", keys: &["Shift+P"], category: ShortcutCategory::ClipNote, description: "選択ノートの音程をスケールに補正", hidden: false },
    ShortcutDef { name: "add_note", keys: &["Insert"], category: ShortcutCategory::ClipNote, description: "ノートを追加 (ピアノロール)", hidden: false },
    ShortcutDef { name: "piano_roll.edit_lyric", keys: &["L"], category: ShortcutCategory::ClipNote, description: "歌詞を編集 (ピアノロールでノート 1 つ選択中)", hidden: false },
    // ----- オートメーション -----
    ShortcutDef { name: "daw.add_automation_from_last_touched", keys: &["A"], category: ShortcutCategory::Automation, description: "最後に触れたパラメータのレーンを追加", hidden: false },
    // ----- グリッド・表示 -----
    ShortcutDef { name: "daw.toggle_snap", keys: &["G"], category: ShortcutCategory::GridView, description: "グリッドスナップ ON / OFF", hidden: false },
    ShortcutDef { name: "daw.fit_view", keys: &["X"], category: ShortcutCategory::GridView, description: "表示をフィット (直前のズームへ)", hidden: false },
    ShortcutDef { name: "daw.zoom_selected_clip", keys: &["Z"], category: ShortcutCategory::GridView, description: "選択クリップへ段階ズーム (アレンジ)", hidden: false },
    ShortcutDef { name: "daw.narrow_grid", keys: &["1"], category: ShortcutCategory::GridView, description: "グリッドを細かく", hidden: false },
    ShortcutDef { name: "daw.widen_grid", keys: &["2"], category: ShortcutCategory::GridView, description: "グリッドを粗く", hidden: false },
    ShortcutDef { name: "daw.toggle_triplet", keys: &["3"], category: ShortcutCategory::GridView, description: "三連符グリッド ON / OFF", hidden: false },
    // ----- オーディオエディタ -----
    ShortcutDef { name: "daw.duplicate_audio_event", keys: &["Ctrl+D"], category: ShortcutCategory::AudioEditor, description: "オーディオイベントを複製", hidden: false },
    ShortcutDef { name: "daw.next_audio_event", keys: &["Ctrl+]"], category: ShortcutCategory::AudioEditor, description: "次のオーディオイベントへ", hidden: false },
    ShortcutDef { name: "daw.prev_audio_event", keys: &["Ctrl+["], category: ShortcutCategory::AudioEditor, description: "前のオーディオイベントへ", hidden: false },
    // ----- ヘルプ -----
    ShortcutDef { name: "daw.toggle_help", keys: &["F1"], category: ShortcutCategory::Help, description: "このショートカット一覧を開く / 閉じる", hidden: false },
    // ----- 開発用 (一覧には出さない) -----
    ShortcutDef { name: "debug_overlay_toggle", keys: &["Ctrl+F1"], category: ShortcutCategory::Help, description: "デバッグオーバーレイ", hidden: true },
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
}
