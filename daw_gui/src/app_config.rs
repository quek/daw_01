//! プロジェクト (Song) 非依存の、 アプリ全体の永続化設定 —
//! `%LOCALAPPDATA%\daw_01\app_config.json`。 現状は resource monitor の常駐
//! 表示 on/off のみ。 `window_state` と同じ load/save 流儀だが、 設定は常に
//! 有効値が欲しいので load 失敗は `Option` でなく `Default` で吸収する。

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// status bar のリソースモニター常駐表示を出すか。 default = true。
    #[serde(default = "default_true")]
    pub resource_monitor_enabled: bool,
    /// **オートメーションをクリップに追従させるか** (`docs/plan_range_selection.md` §5)。
    /// プロジェクトではなく**この人の作業のしかた**なのでアプリ設定側に持つ
    /// (Cubase の *Automation Follows Events* も環境設定)。 default = true。
    #[serde(default = "default_true")]
    pub automation_follows_clips: bool,
    /// r.md #29: 編集履歴 window が開いているか (再起動を跨いで復元)。 default = false。
    #[serde(default)]
    pub undo_history_open: bool,
    /// r.md #29: 編集履歴 window の位置・サイズ `[x, y, w, h]` (px)。 `None` =
    /// 未配置 (初回は既定の右上に出す)。 drag / resize 確定時に保存する。
    #[serde(default)]
    pub undo_history_rect: Option<[f32; 4]>,
    /// r.md #48: 選択中テーマの id (`"dark"` / `"light"` / ユーザーテーマのファイル名)。
    /// **パレットの実体ではなく id を保存する** — テーマファイルを編集したら次回起動で
    /// その内容が反映されるべきで、色を焼き込むと SSoT が二重化するため。
    /// id のテーマが見つからないときは既定テーマにフォールバックする。
    #[serde(default = "default_theme")]
    pub theme: String,
    /// r.md #48: 設定 window が開いているか (再起動を跨いで復元)。
    #[serde(default)]
    pub settings_open: bool,
    /// r.md #48: 設定 window の位置・サイズ `[x, y, w, h]` (px)。 `None` = 未配置。
    #[serde(default)]
    pub settings_rect: Option<[f32; 4]>,
    /// r.md #50: 画面右端のマスターパネルを出すか。 default = true
    /// (「常に音を視覚的にも確認できるように」が要求なので既定で見えている)。
    #[serde(default = "default_true")]
    pub master_panel_open: bool,
    /// マスターパネルの幅 (px)。
    #[serde(default = "default_master_panel_w")]
    pub master_panel_w: f32,
    /// マスターパネルのセクション高さ配分 (MASTER / スペクトラム / オシロ / ゴニオ)。
    #[serde(default = "default_master_panel_sections")]
    pub master_panel_sections: [f32; 4],
    /// メーター設定 (各メーターの右クリックメニューで変える)。
    #[serde(default)]
    pub meter: crate::master_meter::settings::MeterSettings,
    /// r.md #54: ラウドネスレポート window が開いているか (再起動を跨いで復元)。
    #[serde(default)]
    pub loudness_report_open: bool,
    /// r.md #54: ラウドネスレポート window の位置・サイズ `[x, y, w, h]` (px)。
    #[serde(default)]
    pub loudness_report_rect: Option<[f32; 4]>,
    /// r.md #75: VOICEVOX 歌唱合成の「塊」(= `/sing_frame_audio_query` 1 回) の長さ (秒)。
    /// 曲の内容ではなく **合成品質のつまみ**なのでプロジェクトには保存しない。
    /// 既定 60 秒 (実測で 30 秒はばらつきが倍、120 秒は改善せずクエリだけ遅くなる)。
    /// `load` が必ず有効範囲へクランプする (壊れた / 手書きの値で engine を落とさない)。
    #[serde(default = "default_voicevox_chunk_secs")]
    pub voicevox_chunk_secs: f32,
}

fn default_voicevox_chunk_secs() -> f32 {
    common::voicevox_phrase::DEFAULT_CHUNK_SECS
}

impl AppConfig {
    /// 現在の UI 設定 (`UiPrefs`) と選択中テーマ id から保存用の設定を組む。
    ///
    /// **網羅的な struct literal** なので、field を足したらここも必ず埋まる
    /// (埋め忘れでビルドが通らない = 保存漏れが起きない)。組み立てを `AppConfig` の
    /// 定義の隣に置くのは、「何を永続するか」がこの型の関心だから (呼び出し側の
    /// `persist_app_config` は保存先の解決とエラー処理だけを持つ)。
    ///
    /// テーマは **id だけ**保存する (色を焼き込むとテーマファイルを編集しても
    /// 反映されず SSoT が二重化する。r.md #48)。
    #[must_use]
    pub fn from_prefs(prefs: &crate::state::UiPrefs, theme_id: String) -> Self {
        Self {
            resource_monitor_enabled: prefs.resource_monitor_enabled,
            automation_follows_clips: prefs.automation_follows_clips,
            undo_history_open: prefs.undo_history_open,
            undo_history_rect: prefs.undo_history_rect.map(|r| [r.x, r.y, r.w, r.h]),
            theme: theme_id,
            settings_open: prefs.settings_open,
            settings_rect: prefs.settings_rect.map(|r| [r.x, r.y, r.w, r.h]),
            // r.md #50: マスターパネルの見え方は「この人の画面の使い方」なので
            // プロジェクト (`ViewState`) ではなくアプリ設定側に持つ。
            master_panel_open: prefs.master_panel_open,
            master_panel_w: prefs.master_panel_w,
            master_panel_sections: prefs.master_panel_sections,
            meter: prefs.meter_settings,
            // r.md #54: レポート window の開閉と位置も「画面の使い方」側。
            loudness_report_open: prefs.loudness_report_open,
            loudness_report_rect: prefs.loudness_report_rect.map(|r| [r.x, r.y, r.w, r.h]),
            // r.md #75: 合成の塊の長さ (秒) も「この人の作業のしかた」側。
            voicevox_chunk_secs: prefs.voicevox_chunk_secs,
        }
    }
}

fn default_master_panel_w() -> f32 {
    300.0
}

/// 既定の高さ配分。MASTER (フェーダー + ラウドネス) を厚めに、残りを均等に。
fn default_master_panel_sections() -> [f32; 4] {
    [0.34, 0.24, 0.18, 0.24]
}

fn default_theme() -> String {
    crate::theme::DEFAULT_THEME_ID.to_string()
}

fn default_true() -> bool {
    true
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            resource_monitor_enabled: true,
            automation_follows_clips: true,
            undo_history_open: false,
            undo_history_rect: None,
            theme: default_theme(),
            settings_open: false,
            settings_rect: None,
            master_panel_open: true,
            master_panel_w: default_master_panel_w(),
            master_panel_sections: default_master_panel_sections(),
            meter: crate::master_meter::settings::MeterSettings::default(),
            loudness_report_open: false,
            loudness_report_rect: None,
            voicevox_chunk_secs: default_voicevox_chunk_secs(),
        }
    }
}

/// ファイルが無い / 読めない / parse 失敗なら `Default` を返す (常に有効な設定)。
///
/// 値域を持つ設定はここで必ずクランプする — 手書きや旧バージョンの
/// `app_config.json` に 5 秒 / 9999 秒が入っていても、VOICEVOX engine を
/// 落とさないため (r.md #75)。
pub fn load(path: impl AsRef<Path>) -> AppConfig {
    let Ok(text) = std::fs::read_to_string(path.as_ref()) else {
        return AppConfig::default();
    };
    if text.trim().is_empty() {
        return AppConfig::default();
    }
    let mut cfg: AppConfig = serde_json::from_str(&text).unwrap_or_default();
    cfg.voicevox_chunk_secs = cfg.voicevox_chunk_secs.clamp(
        common::voicevox_phrase::MIN_CHUNK_SECS,
        common::voicevox_phrase::MAX_CHUNK_SECS,
    );
    cfg
}

pub fn save(path: impl AsRef<Path>, config: &AppConfig) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(config)?;
    std::fs::write(path, text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn save_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("app_config.json");
        let cfg = AppConfig {
            resource_monitor_enabled: false,
            automation_follows_clips: false,
            undo_history_open: true,
            undo_history_rect: Some([10.0, 20.0, 260.0, 360.0]),
            theme: "light".to_string(),
            settings_open: true,
            settings_rect: Some([1.0, 2.0, 300.0, 400.0]),
            master_panel_open: false,
            master_panel_w: 412.0,
            master_panel_sections: [0.4, 0.2, 0.2, 0.2],
            meter: crate::master_meter::settings::MeterSettings {
                loudness_target_lufs: -23.0,
                loudness_true_peak_ceiling_dbtp: -2.0,
                ..Default::default()
            },
            loudness_report_open: true,
            loudness_report_rect: Some([5.0, 6.0, 720.0, 500.0]),
            voicevox_chunk_secs: 120.0,
        };
        save(&path, &cfg).unwrap();
        let loaded = load(&path);
        assert!(!loaded.resource_monitor_enabled);
        assert!(loaded.undo_history_open);
        assert_eq!(loaded.undo_history_rect, Some([10.0, 20.0, 260.0, 360.0]));
        assert_eq!(loaded.theme, "light");
        assert!(loaded.settings_open);
        assert_eq!(loaded.settings_rect, Some([1.0, 2.0, 300.0, 400.0]));
        assert!(!loaded.master_panel_open);
        assert_eq!(loaded.master_panel_w, 412.0);
        assert_eq!(loaded.master_panel_sections, [0.4, 0.2, 0.2, 0.2]);
        assert_eq!(loaded.meter.loudness_target_lufs, -23.0);
        assert_eq!(loaded.meter.loudness_true_peak_ceiling_dbtp, -2.0);
        assert!(loaded.loudness_report_open);
        assert_eq!(loaded.loudness_report_rect, Some([5.0, 6.0, 720.0, 500.0]));
        assert_eq!(loaded.voicevox_chunk_secs, 120.0);
    }

    /// r.md #75: 壊れた / 手書きの `app_config.json` で 5 秒や 9999 秒が来ても、
    /// VOICEVOX engine を落とさない (= load 時に必ず有効範囲へ畳む)。
    #[test]
    fn load_clamps_chunk_secs_out_of_range() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("app_config.json");
        std::fs::write(&path, r#"{"voicevox_chunk_secs": 9999.0}"#).unwrap();
        assert_eq!(
            load(&path).voicevox_chunk_secs,
            common::voicevox_phrase::MAX_CHUNK_SECS
        );
        std::fs::write(&path, r#"{"voicevox_chunk_secs": 5.0}"#).unwrap();
        assert_eq!(
            load(&path).voicevox_chunk_secs,
            common::voicevox_phrase::MIN_CHUNK_SECS
        );
        // 未指定は既定 (60 秒)。
        std::fs::write(&path, "{}").unwrap();
        assert_eq!(
            load(&path).voicevox_chunk_secs,
            common::voicevox_phrase::DEFAULT_CHUNK_SECS
        );
    }

    #[test]
    fn load_returns_default_when_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nope.json");
        assert!(load(&path).resource_monitor_enabled); // default = true
    }

    #[test]
    fn load_tolerates_unknown_and_missing_fields() {
        // 将来フィールドが増えても古い JSON が読め、 欠落は default で埋まる。
        let dir = tempdir().unwrap();
        let path = dir.path().join("partial.json");
        std::fs::write(&path, "{}").unwrap();
        let loaded = load(&path);
        assert!(loaded.resource_monitor_enabled); // 欠落 → default true
        // r.md #48: theme が無い旧 config でも既定テーマで起動できる。
        assert_eq!(loaded.theme, crate::theme::DEFAULT_THEME_ID);
        assert!(!loaded.settings_open);
    }
}
