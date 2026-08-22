//! daw_gui の theme — ui-core 汎用パレット + DAW 固有トークンの合成、およびテーマの読み込み。
//!
//! 汎用 UI トークン (chrome / meter / waveform / curve / selection / grid) は
//! [`daw_ui_core::theme::Palette`] が SSoT。DAW 固有の意味色 (playhead / record / solo /
//! clip / ghost) と、ui-core が持たないアプリ固有クローム (プラグインタグ / 鍵盤 /
//! ヘルプのキー表記 / 映像プレビュー) だけを **アプリ層のここ** で定義する
//! (arch 不変条件 #8: daw-ui core は DAW ドメインを持たない)。
//!
//! ## 所有者 (r.md #48)
//!
//! - [`Theme`] が「いま有効なテーマ」。`AppData.theme` が SSoT。
//! - `Theme.core` は `Arc<Palette>` で、**`UiHost` が持つ実体と同じもの**を指す
//!   (runner が毎フレーム `UiHost::set_palette(theme.core.clone())` を呼ぶ)。複製しない。
//! - call site は `app.theme.core.<token>` (汎用) / `app.theme.daw.<token>` (DAW 固有) で読む。
//!   `ui` が手元にあるなら `ui.palette()` でも同じ実体が取れる (ui-core widget 用)。
//!
//! ## テーマは後から追加できる
//!
//! `%LOCALAPPDATA%\daw_01\themes\*.json` に置いたファイルが設定画面の一覧に出る。
//! 形式は「ベース + 差分」:
//!
//! ```json
//! { "name": "Solarized Dark", "base": "dark", "colors": { "accent": "#268bd2" } }
//! ```
//!
//! 書かなかったトークンは `base` から継承するので、**後からトークンが増えても既存の
//! テーマファイルは壊れない**。未知のキーは警告して無視する (前方互換)。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use daw_ui_core::palette;
use daw_ui_renderer::Color;
use serde::Deserialize;

pub use daw_ui_core::theme::{Palette, WaveformInk, contrast_ratio, srgb, srgba, srgb_to_linear};

palette! {
    /// daw_gui 固有の色トークン。DAW の意味色 (playhead / record / solo / clip) と、
    /// ui-core が持たないアプリ固有クローム (プラグインタグ / 鍵盤 / ヘルプ / 映像プレビュー)。
    ///
    /// 彩度を持つのはここと ui-core の meter / waveform ramp だけで、chrome は寒色基調に統一。
    ///
    /// ダーク値は **linear 直値** (歴史的にこの空間で調整済み)、ライト値は [`srgb`] で
    /// **画面上の見た目**から書く ([`Palette`] と同じ規約)。ライトは色相を保ったまま明度を
    /// 落として彩度を上げ、明るい床の上でも同じ意味に読めるようにしてある。
    pub struct DawColors {
        // ===== 機能 (semantic) 状態色 =====

        /// 再生ヘッド線 (arrangement / piano-roll / audio editor)。寒色フィールドで唯一「叫ぶ」warm coral。
        playhead: Color::rgb(1.00, 0.34, 0.20), srgb(0.839, 0.271, 0.094);
        /// record-arm / MIDI record / mute ON。予約された alarm red。
        record: Color::rgb(0.90, 0.26, 0.30), srgb(0.757, 0.102, 0.133);
        /// record-mode arm (録音モード待機) の orange。
        record_arm: Color::rgb(0.90, 0.50, 0.22), srgb(0.753, 0.353, 0.055);
        /// solo ON / metronome ON。予約された warning yellow。
        solo: Color::rgb(0.97, 0.82, 0.30), srgb(0.910, 0.725, 0.039);
        /// play / transport 稼働 LED・status success。わずかに teal 寄りの green。
        play: Color::rgb(0.28, 0.80, 0.48), srgb(0.071, 0.502, 0.251);

        // ===== clip 既定色 / ドラッグゴースト =====

        /// track 未着色時の clip 既定塗り。
        clip_default: Color::rgb(0.22, 0.34, 0.52), srgb(0.361, 0.486, 0.659);
        /// clip 既定枠。
        clip_default_border: Color::rgb(0.30, 0.46, 0.66), srgb(0.243, 0.353, 0.502);
        /// ドラッグ複製ゴースト: リンク (元と連動) = green。
        ghost_linked: Color::rgb(0.42, 0.86, 0.58), srgb(0.094, 0.604, 0.306);
        /// ドラッグ複製ゴースト: 独立 (新規実体) = orange。
        ghost_independent: Color::rgb(1.00, 0.70, 0.34), srgb(0.784, 0.459, 0.063);

        // ===== プラグイン分類タグ (plugin picker) =====

        /// フォーマット表記 (CLAP / VST3)。
        tag_format: Color::rgb(0.55, 0.78, 0.95), srgb(0.086, 0.408, 0.722);
        /// 楽器プラグイン。
        tag_instrument: Color::rgb(0.58, 0.85, 0.55), srgb(0.118, 0.518, 0.220);
        /// エフェクトプラグイン。
        tag_fx: Color::rgb(0.55, 0.78, 0.95), srgb(0.086, 0.408, 0.722);
        /// MIDI プラグイン。
        tag_midi: Color::rgb(0.95, 0.74, 0.45), srgb(0.690, 0.416, 0.047);
        /// 映像エフェクト。
        tag_video: Color::rgb(0.80, 0.62, 0.95), srgb(0.478, 0.220, 0.690);

        // ===== automation / modulation =====

        /// automation lane ヘッダの薄い藤色 (inspector の automate トグル ON と共有)。
        automation_lane: Color::rgb(0.78, 0.55, 0.85), srgb(0.478, 0.235, 0.612);
        /// カーブエディタのノード hover (淡黄)。
        node_hover: Color::rgb(1.00, 1.00, 0.60), srgb(0.604, 0.518, 0.063);
        /// カーブエディタのノード drag (珊瑚)。
        node_drag: Color::rgb(0.95, 0.45, 0.40), srgb(0.753, 0.157, 0.118);

        // ===== ミキサー =====

        /// Return strip 本体 — 緑寄りの tint で通常 track / group bus と区別する。
        strip_return_bg: Color::rgb(0.18, 0.28, 0.22), srgb(0.863, 0.922, 0.878);
        /// returns 帯と通常帯を分ける縦 divider。
        strip_return_divider: Color::rgb(0.30, 0.40, 0.32), srgb(0.561, 0.690, 0.604);

        // ===== ピアノロール =====

        /// 白鍵 (物理ピアノ鍵盤のメタファ。色相はテーマ非依存)。
        /// ライトは鍵盤パネル (`panel_raised`) よりわずかに白く、区切り線と合わせて境界が
        /// 出る紙白。実物のピアノをライト UI に置いたときの見えかたに合わせている。
        key_white: Color::rgb(0.92, 0.93, 0.95), srgb(0.992, 0.996, 1.000);
        /// 黒鍵。
        key_black: Color::rgb(0.10, 0.11, 0.13), srgb(0.180, 0.196, 0.220);
        /// velocity ランプの下端 (velocity 0)。
        note_velocity_low: Color::rgb(0.35, 0.55, 0.85), srgb(0.451, 0.588, 0.769);
        /// velocity ランプの上端 (velocity 127)。**ライトでは強いノートほど暗くなる**
        /// (明るい note-grid の上では「濃い = 強い」 が自然な読み方)。
        note_velocity_high: Color::rgb(0.70, 0.85, 0.95), srgb(0.090, 0.255, 0.494);

        // ===== アレンジ =====

        /// スライス区切り線 (選択色の借用をやめた専用トークン)。
        slice_divider: Color::rgb(1.00, 0.72, 0.24), srgb(0.690, 0.416, 0.031);
        /// 共有グループ hover 強調のリング (identity-neutral な中立色)。ダーク値は旧実装が
        /// 借用していた `TEXT` と厳密に一致させてある (ダークのピクセルを変えないため)。
        /// ライトでは暗中立色に反転するので、明るいクリップの上でも枠が沈まない。
        highlight_ring: Color::rgb(0.880, 0.902, 0.945), srgb(0.102, 0.122, 0.157);
        /// 同 glow wash (リングと同色の低 alpha 版として使う)。
        highlight_glow: Color::rgba(0.880, 0.902, 0.945, 0.22), srgba(0.102, 0.122, 0.157, 0.20);

        // ===== ヘルプ (F1) =====

        /// キーボードのキー表記 (ティール)。
        text_keycap: Color::rgb(0.52, 0.86, 0.78), srgb(0.055, 0.420, 0.361);
        /// マウスのジェスチャ表記 (アンバー、キーボードと一目で区別)。
        text_gesture: Color::rgb(0.93, 0.78, 0.46), srgb(0.588, 0.400, 0.039);

        // ===== 映像プレビュー (極性固定: 常に暗いキャンバスの上) =====
        // 映像の外側は**両テーマとも暗いまま**。書き出し動画の黒背景と対であり、
        // `--smoke-test` の判定 (backdrop の RGB sum ≈ 44 / near-black 閾値) の前提でもある。

        /// プレビューのレターボックス背景。
        video_canvas_bg: Color::rgb(0.05, 0.05, 0.07), Color::rgb(0.05, 0.05, 0.07);
        /// 映像未読込みのプレースホルダ文字。
        video_placeholder_text: Color::rgb(0.65, 0.70, 0.80), Color::rgb(0.65, 0.70, 0.80);
        /// グループ枠 (親グループの範囲表示)。
        video_group_outline: Color::rgba(0.45, 0.82, 1.00, 0.90), Color::rgba(0.45, 0.82, 1.00, 0.90);
        /// 選択枠。
        video_selection_stroke: Color::rgba(1.00, 0.95, 0.45, 0.85), Color::rgba(1.00, 0.95, 0.45, 0.85);
        /// 選択ハンドルの塗り。
        video_handle: Color::rgba(1.00, 0.95, 0.45, 1.00), Color::rgba(1.00, 0.95, 0.45, 1.00);
        /// 選択ハンドルの縁取り (映像の上で必ず立つ純黒)。
        video_handle_border: Color::rgba(0.00, 0.00, 0.00, 0.85), Color::rgba(0.00, 0.00, 0.00, 0.85);
    }
}

/// 組込みテーマの id と表示名 (設定画面の並び順もこの順)。
pub const BUILTIN_THEMES: &[(&str, &str)] = &[("dark", "ダーク"), ("light", "ライト")];

/// 既定テーマ id (初回起動 / 保存された id が見つからないときのフォールバック)。
pub const DEFAULT_THEME_ID: &str = "dark";

/// テーマの出どころ。設定画面が「組込み / ユーザーファイル」を出し分けるのに使う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeSource {
    /// アプリに組み込まれたテーマ。
    Builtin,
    /// `themes/` ディレクトリの JSON。
    User(PathBuf),
}

/// いま有効なテーマ (汎用パレット + DAW 固有トークン)。
#[derive(Debug, Clone)]
pub struct Theme {
    /// 安定 id。組込みは `"dark"` / `"light"`、ユーザーテーマはファイル名 (拡張子なし)。
    /// `app_config.json` に保存されるのはこれ。
    pub id: String,
    /// 設定画面に出す表示名。
    pub name: String,
    /// 出どころ。
    pub source: ThemeSource,
    /// 汎用 UI トークン。`UiHost` が持つ実体と同じ `Arc` を共有する (複製しない)。
    pub core: Arc<Palette>,
    /// DAW 固有トークン。
    pub daw: DawColors,
}

impl Default for Theme {
    fn default() -> Self {
        Self::builtin(DEFAULT_THEME_ID).expect("既定テーマは常に存在する")
    }
}

impl Theme {
    /// 組込みテーマ。未知の id は `None`。
    #[must_use]
    pub fn builtin(id: &str) -> Option<Self> {
        let (core, daw) = match id {
            "dark" => (Palette::dark(), DawColors::dark()),
            "light" => (Palette::light(), DawColors::light()),
            _ => return None,
        };
        let name = BUILTIN_THEMES
            .iter()
            .find(|(bid, _)| *bid == id)
            .map(|(_, n)| (*n).to_string())
            .unwrap_or_else(|| id.to_string());
        Some(Self { id: id.to_string(), name, source: ThemeSource::Builtin, core: Arc::new(core), daw })
    }
}

/// テーマファイルの中身。全フィールド省略可 (= ダークをそのまま使う)。
#[derive(Debug, Deserialize)]
struct ThemeFile {
    /// 表示名。省略時はファイル名 (拡張子なし)。
    #[serde(default)]
    name: Option<String>,
    /// 継承元の**組込み** id。省略時は `"dark"`。
    #[serde(default)]
    base: Option<String>,
    /// トークン名 → 色 (`#rgb` / `#rrggbb` / `#rrggbbaa`)。core と daw を混ぜて書ける。
    #[serde(default)]
    colors: BTreeMap<String, String>,
}

/// `#rgb` / `#rrggbb` / `#rrggbbaa` を [`Color`] に。先頭の `#` は省略可。
///
/// **hex は「画面上でその色に見える」 sRGB 値**として解釈し、[`srgb_to_linear`] で
/// パレットの保持空間 (linear) へ変換する。render target が `Rgba8UnormSrgb` で GPU が
/// 表示時に sRGB エンコードするため、変換せずに入れるとテーマ作者の意図よりずっと明るい
/// 色になる (カラーピッカーで拾った `#268bd2` が水色になる)。alpha はブレンド係数なので
/// 変換しない。
fn parse_hex_color(s: &str) -> Option<Color> {
    let h = s.trim().strip_prefix('#').unwrap_or(s.trim());
    // 以降は byte index でスライスするので、**ASCII でなければここで弾く**。
    // 多バイト文字 (例 "ああ" = 6 byte) は長さ判定を通り抜けて UTF-8 境界を割り、
    // スライスが panic する (= ユーザーのテーマファイル 1 行でアプリが落ちる)。
    if !h.is_ascii() {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok().map(|v| f32::from(v) / 255.0);
    let nibble = |i: usize| {
        u8::from_str_radix(&h[i..i + 1], 16).ok().map(|v| f32::from(v * 17) / 255.0)
    };
    match h.len() {
        3 => Some(srgb(nibble(0)?, nibble(1)?, nibble(2)?)),
        4 => Some(srgba(nibble(0)?, nibble(1)?, nibble(2)?, nibble(3)?)),
        6 => Some(srgb(byte(0)?, byte(2)?, byte(4)?)),
        8 => Some(srgba(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
        _ => None,
    }
}

/// テーマ JSON 1 本を [`Theme`] に。パース失敗は `Err`、**未知のトークン名は警告して無視**
/// (前方/後方互換: 新トークンが増えても古いテーマファイルが壊れず、古い daw_01 で
/// 新しいテーマファイルを開いても落ちない)。
///
/// # Errors
/// ファイルが読めない / JSON として壊れている / `base` が未知の組込み id のとき。
pub fn load_theme_file(path: &Path) -> anyhow::Result<Theme> {
    let text = std::fs::read_to_string(path)?;
    let file: ThemeFile = serde_json::from_str(&text)?;
    let base_id = file.base.as_deref().unwrap_or(DEFAULT_THEME_ID);
    let base = Theme::builtin(base_id).ok_or_else(|| {
        anyhow::anyhow!("未知のベーステーマ \"{base_id}\" (使えるのは dark / light)")
    })?;

    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("テーマファイル名を id にできない: {}", path.display()))?;

    let mut core = (*base.core).clone();
    let mut daw = base.daw;
    for (key, value) in &file.colors {
        let Some(color) = parse_hex_color(value) else {
            tracing::warn!(theme = %id, key, value, "色として読めない値 (#rgb / #rrggbb / #rrggbbaa)");
            continue;
        };
        if !core.set_by_name(key, color) && !daw.set_by_name(key, color) {
            tracing::warn!(theme = %id, key, "未知のトークン名 (無視)");
        }
    }

    Ok(Theme {
        name: file.name.unwrap_or_else(|| id.clone()),
        id,
        source: ThemeSource::User(path.to_path_buf()),
        core: Arc::new(core),
        daw,
    })
}

/// 選べるテーマ全部 (組込み → ユーザー、それぞれ宣言順 / 名前順)。
///
/// `themes_dir` が無い / 読めない場合は組込みのみ。ユーザーテーマの id が組込みと衝突したら
/// `file:<id>` に退避して両方出す (ユーザーが `dark.json` を置いても組込みが消えない)。
#[must_use]
pub fn available_themes(themes_dir: Option<&Path>) -> Vec<Theme> {
    let mut out: Vec<Theme> =
        BUILTIN_THEMES.iter().filter_map(|(id, _)| Theme::builtin(id)).collect();

    let Some(dir) = themes_dir else { return out };
    let Ok(entries) = std::fs::read_dir(dir) else { return out };

    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("json")))
        .collect();
    paths.sort();

    for path in paths {
        match load_theme_file(&path) {
            Ok(mut theme) => {
                if out.iter().any(|t| t.id == theme.id) {
                    theme.id = format!("file:{}", theme.id);
                }
                out.push(theme);
            }
            Err(e) => tracing::warn!(path = %path.display(), error = ?e, "テーマファイルを読めない"),
        }
    }
    out
}

/// id からテーマを解決する。見つからなければ既定テーマ。
/// 保存された id のテーマファイルが消えていても起動できるようにするための SSoT。
#[must_use]
pub fn resolve(themes_dir: Option<&Path>, id: &str) -> Theme {
    available_themes(themes_dir)
        .into_iter()
        .find(|t| t.id == id)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parsing_covers_every_accepted_form() {
        let cases = [
            ("#000", srgb(0.0, 0.0, 0.0)),
            ("fff", srgb(1.0, 1.0, 1.0)),
            ("#ff8000", srgb(1.0, 128.0 / 255.0, 0.0)),
            ("#ff800080", srgba(1.0, 128.0 / 255.0, 0.0, 128.0 / 255.0)),
            ("#0f08", srgba(0.0, 1.0, 0.0, 136.0 / 255.0)),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_hex_color(text), Some(expected), "input={text}");
        }
        for bad in ["", "#12345", "zzzzzz", "#12"] {
            assert_eq!(parse_hex_color(bad), None, "input={bad}");
        }
        // 多バイト文字は **byte 長で判定を通り抜けて UTF-8 境界を割る**ので、
        // ASCII ガードが無いとスライスで panic する (ユーザーのテーマファイル 1 行で落ちる)。
        for bad in ["ああ", "#ああああ", "＃ffffff", "ff００ff"] {
            assert_eq!(parse_hex_color(bad), None, "input={bad}");
        }
    }

    /// テーマ作者が書く hex は **画面色**。パレットは linear で持つので、
    /// 変換されずに入ると「カラーピッカーで拾った色より明るい」 テーマになる。
    /// alpha はブレンド係数なので素通し。
    #[test]
    fn hex_is_interpreted_as_screen_srgb_and_stored_as_linear() {
        let c = parse_hex_color("#808080").unwrap();
        let expected = srgb_to_linear(128.0 / 255.0);
        assert!((c.r - expected).abs() < 1e-6, "mid gray は linear へ: {c:?}");
        assert!(c.r < 0.5, "linear 値は sRGB 値より暗い: {}", c.r);
        // 両端は不動点。
        assert!((parse_hex_color("#000").unwrap().r - 0.0).abs() < 1e-6);
        assert!((parse_hex_color("#fff").unwrap().r - 1.0).abs() < 1e-6);
        assert!((parse_hex_color("#ffffff80").unwrap().a - 128.0 / 255.0).abs() < 1e-6);
    }

    #[test]
    fn builtin_ids_resolve_and_unknown_does_not() {
        assert_eq!(Theme::builtin("dark").map(|t| t.id), Some("dark".to_string()));
        assert_eq!(Theme::builtin("light").map(|t| t.id), Some("light".to_string()));
        assert!(Theme::builtin("nope").is_none());
        assert!(Theme::builtin("dark").unwrap().core.is_dark());
        assert!(!Theme::builtin("light").unwrap().core.is_dark());
    }

    /// 「ベース + 差分」 の要: **書かなかった色はベースから継承する**。
    /// これが成り立つから、後からトークンを足しても既存のテーマファイルが壊れない。
    #[test]
    fn user_theme_inherits_unspecified_tokens_from_its_base() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("solarized.json");
        std::fs::write(
            &path,
            r##"{ "name": "Solarized", "base": "light",
                  "colors": { "accent": "#268bd2", "playhead": "#cb4b16", "nope": "#fff" } }"##,
        )
        .unwrap();

        let theme = load_theme_file(&path).unwrap();
        assert_eq!(theme.id, "solarized");
        assert_eq!(theme.name, "Solarized");
        assert_eq!(theme.source, ThemeSource::User(path.clone()));
        // 指定した core / daw トークンは上書きされる。
        assert_eq!(theme.core.accent, parse_hex_color("#268bd2").unwrap());
        assert_eq!(theme.daw.playhead, parse_hex_color("#cb4b16").unwrap());
        // 指定しなかったトークンは base (light) のまま = 継承。
        let light = Theme::builtin("light").unwrap();
        assert_eq!(theme.core.panel, light.core.panel);
        assert_eq!(theme.daw.solo, light.daw.solo);
        // 未知キーは無視されるだけで、読み込み自体は成功する (前方互換)。
    }

    #[test]
    fn user_theme_with_colliding_id_does_not_hide_the_builtin() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dark.json"), r#"{ "colors": {} }"#).unwrap();
        let themes = available_themes(Some(dir.path()));
        let ids: Vec<&str> = themes.iter().map(|t| t.id.as_str()).collect();
        assert!(ids.contains(&"dark"), "組込みが消えない: {ids:?}");
        assert!(ids.contains(&"file:dark"), "ユーザーテーマも出る: {ids:?}");
    }

    #[test]
    fn resolve_falls_back_to_the_default_theme_when_the_id_is_gone() {
        // 保存済み id のテーマファイルを消してもアプリが起動できること。
        assert_eq!(resolve(None, "deleted-theme").id, DEFAULT_THEME_ID);
        assert_eq!(resolve(None, "light").id, "light");
    }

    #[test]
    fn daw_token_set_by_name_covers_every_declared_token() {
        let mut d = DawColors::dark();
        for name in DawColors::token_names() {
            assert!(d.set_by_name(name, Color::WHITE), "未対応のトークン名: {name}");
        }
        assert!(!d.set_by_name("no_such_token", Color::WHITE));
    }

    #[test]
    fn video_canvas_stays_dark_in_every_builtin_theme() {
        // 映像の外側は書き出し動画の黒背景と対であり、`--smoke-test` の
        // near-black 判定の前提でもある。ライトで明色に倒してはいけない。
        for id in ["dark", "light"] {
            let t = Theme::builtin(id).unwrap();
            assert_eq!(t.daw.video_canvas_bg, DawColors::dark().video_canvas_bg, "theme={id}");
        }
    }
}
