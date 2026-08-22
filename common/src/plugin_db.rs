//! CLAP plugin database.
//!
//! Scans system CLAP directories, enumerates every descriptor inside each
//! `.clap` file, and caches the resulting ID → (path, descriptor index) map
//! on disk so subsequent launches avoid the scan cost.
//!
//! Plugin state files save **plugin IDs**, not paths, so projects are
//! portable across machines as long as the same plugin (any path) is
//! installed.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::plugin_format::PluginFormat;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginEntry {
    /// Stable plugin identifier. For CLAP this is
    /// `clap_plugin_descriptor.id` (e.g. `com.vcvrack.rack`). For VST3 it
    /// is the class UUID rendered as 32 hex chars (no dashes).
    pub id: String,
    /// Which backend hosts this plugin. Defaults to CLAP so pre-VST3
    /// caches upgrade cleanly.
    #[serde(default)]
    pub format: PluginFormat,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub vendor: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// Absolute path to the plugin binary: a `.clap` file or, for VST3, the
    /// `.vst3` bundle directory (or legacy single DLL).
    pub path: PathBuf,
    /// Index within the factory. A single `.clap`/`.vst3` can host multiple
    /// descriptors (e.g. VCV Rack's `rack` / `rack.fx` / `rack.generator`,
    /// or a VST3 vendor shipping several classes in one DLL).
    pub descriptor_index: u32,
    /// ポート構成（capability の **Single Source of Truth**）。
    /// `note 入力`を持つ（= MIDI/note を受け取れる）。probe で確定。
    /// 旧 cache（フィールド無し）は `#[serde(default)]` で `false` に load される
    /// (= 起動時 [`PluginDatabase::needs_port_probe`] が rescan を促す)。
    #[serde(default)]
    pub has_note_input: bool,
    /// `note 出力`を持つ = **生成器になれる**（Scaler 2 のような dual-role 判定の基準）。
    #[serde(default)]
    pub has_note_output: bool,
    /// `audio 出力`を持つ。
    #[serde(default)]
    pub has_audio_output: bool,
    /// `audio 入力`を持つ = **audio を加工できる (= エフェクト)**。v23 単一チェーン:
    /// 役割の位置導出で「audio を生成する音源 (audio_in 無し)」と「audio を加工する
    /// エフェクト (audio_in 有り)」を区別する決め手。実 plugin は MIDI 制御付き
    /// audio エフェクトが note_in を持つため、note 系だけでは音源と区別できない。
    /// 旧 cache は `#[serde(default)]` で `false`、`PORT_PROBE_VERSION` bump で再 probe。
    #[serde(default)]
    pub has_audio_input: bool,
    /// 映像 (RGBA テクスチャ) 入力ポートを持つ。内蔵映像効果
    /// (`builtin.video.*`) のみ true。外部 CLAP/VST3 は probe で常に false。
    #[serde(default)]
    pub has_video_input: bool,
    /// 映像 (RGBA テクスチャ) 出力ポートを持つ。
    #[serde(default)]
    pub has_video_output: bool,
}

/// CLAP ARA companion-API の feature タグ (ARACLAP.h の
/// `CLAP_PLUGIN_FEATURE_ARA_SUPPORTED` / `_ARA_REQUIRED`)。CLAP は descriptor の
/// features にこれを載せる。VST3 には feature list の概念が無いので、scan が ARA
/// factory を検出したとき同タグを features へ正規化して push する。これにより
/// [`PluginEntry::is_ara`] が CLAP / VST3 を区別なく `features` から派生できる。
pub const CLAP_FEATURE_ARA_SUPPORTED: &str = "ara:supported";
pub const CLAP_FEATURE_ARA_REQUIRED: &str = "ara:required";

impl PluginEntry {
    /// ARA (Audio Random Access) 対応プラグインか。`features` を SSoT に派生する
    /// ので専用フィールド・cache 変更は不要。CLAP は descriptor features の公式
    /// タグ、VST3 は scan が ARA factory 検出時に同タグを push 済み。
    pub fn is_ara(&self) -> bool {
        self.features
            .iter()
            .any(|f| f == CLAP_FEATURE_ARA_SUPPORTED || f == CLAP_FEATURE_ARA_REQUIRED)
    }

    /// ARA 必須 (非 ARA ロードを受け付けない) プラグインか。`is_ara` の部分集合。
    pub fn ara_required(&self) -> bool {
        self.features.iter().any(|f| f == CLAP_FEATURE_ARA_REQUIRED)
    }
}

/// v23: port 構成 probe スキーマの現行版。 `PluginEntry` に記録する
/// port bool (note in/out・audio out/**in**) の取得方法・意味づけを変えたら上げる。
/// cache の `port_probe_version` がこれ未満なら、 起動時に再 probe (rescan) する。
/// v23: `has_audio_input` を追加したので 1 → 2 (= 既存 cache を再 probe させる)。
/// `has_video_input`/`has_video_output` を追加し probe 行を 6 キー化したので
/// 2 → 3 (= 旧 4-キー probe 結果の cache を再 probe させ、外部 plugin の video=false を確定)。
/// (r.md #5 ARA2) scan に ARA 検出 (CLAP `ara:supported` feature / VST3 `ARA Main
/// Factory Class` ペアリング) を追加したので 3 → 4 (= 旧 cache を再スキャンさせ、
/// 既存プラグインに `ara:supported` を付与する。これが無いと `is_ara()` が false の
/// ままで `sync_ara_documents` が `SetupAraDocument` を送らず ARA が無音になる)。
pub const PORT_PROBE_VERSION: u32 = 4;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginDatabase {
    pub entries: Vec<PluginEntry>,
    /// UNIX timestamp (seconds) of the last successful scan. Used for
    /// "rescan if older than X" heuristics in the future.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scanned_at: Option<u64>,
    /// `PluginEntry` の port 構成 (3 bool) を probe で埋めた版
    /// ([`PORT_PROBE_VERSION`])。 古い cache (フィールド無し) は `#[serde(default)]`
    /// で 0 に load され、 [`PluginDatabase::needs_port_probe`] が再 probe を促す。
    #[serde(default)]
    pub port_probe_version: u32,
}

impl PluginDatabase {
    /// Returns the entry with matching `id`, or `None` if absent.
    pub fn find_by_id(&self, id: &str) -> Option<&PluginEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// 起動時に port 構成の再 probe (= rescan) が要るか。 cache が旧版
    /// ([`PORT_PROBE_VERSION`] 未満) で、 かつ probe 対象 (VST3/CLAP) を 1 つ以上
    /// 持つときだけ true。 builtin のみ / 空 DB では促さない (probe する物が無い)。
    #[must_use]
    pub fn needs_port_probe(&self) -> bool {
        self.port_probe_version < PORT_PROBE_VERSION
            && self
                .entries
                .iter()
                .any(|e| matches!(e.format, PluginFormat::Vst3 | PluginFormat::Clap))
    }

    /// daw_01-bundled builtin plugin (`PluginFormat::Builtin`) を entries
    /// 先頭に注入する。builtin はコードの [`builtin_descriptors`] が
    /// **Single Source of Truth** なので、cache load / scan のどちらを経由して
    /// メモリへ載せる場合も必ずこれで保証する。既存の builtin entry
    /// (古い cache 由来や重複) は format で除外してから注入するので冪等。
    ///
    /// これが無いと、builtin 追加前に保存された古い cache を load した
    /// セッションでは VOICEVOX 等が DB から欠落し、vocal track の instrument
    /// ロードが `find_by_id → None` で失敗する (= 自動合成が走らず歌わない)。
    pub fn ensure_builtins(&mut self) {
        self.entries.retain(|e| e.format != PluginFormat::Builtin);
        let mut merged = builtin_descriptors();
        merged.append(&mut self.entries);
        self.entries = merged;
    }

    /// Load the cached database from disk. Missing file / invalid JSON
    /// returns `Ok(None)` rather than an error so callers can fall back
    /// to a fresh scan.
    ///
    /// The returned DB is **sanitised and ready to use**: entries with an
    /// empty `id` (corrupt cache) are dropped with a warning, and the
    /// builtin descriptors are re-injected via [`ensure_builtins`]. This
    /// makes "load 後は必ず builtin が居る" a function-level guarantee
    /// rather than an implicit convention each caller must remember.
    /// `ensure_builtins` is idempotent and `save_to_file` excludes builtins,
    /// so a save → load round-trip has no cumulative side effect on disk.
    ///
    /// [`ensure_builtins`]: PluginDatabase::ensure_builtins
    pub fn load_from_file(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut db: PluginDatabase = serde_json::from_str(&data)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        db.entries.retain(|e| {
            if e.id.is_empty() {
                tracing::warn!(
                    name = %e.name,
                    path = %e.path.display(),
                    "cache entry with empty id, dropping"
                );
                false
            } else {
                true
            }
        });
        db.ensure_builtins();
        Ok(Some(db))
    }

    /// Persist the database atomically (write to temp file + rename).
    ///
    /// builtin (`PluginFormat::Builtin`) はコードが **Single Source of Truth**
    /// なので **永続化しない**。ディスクには外部 plugin の scan 結果だけを書き、
    /// load 側は [`PluginDatabase::ensure_builtins`] で常にコードから builtin を
    /// 注入する。これで「古い cache に builtin が無い / 古い」乖離が構造的に
    /// 起きなくなる (= cache を消さずとも新 builtin が反映される)。除外は
    /// 永続化の 1 箇所に集約するので、ensure_builtins 後の DB を save しても
    /// 順序に関係なくディスクは外部 plugin のみになる。
    pub fn save_to_file(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let persisted = PluginDatabase {
            entries: self
                .entries
                .iter()
                .filter(|e| e.format != PluginFormat::Builtin)
                .cloned()
                .collect(),
            scanned_at: self.scanned_at,
            port_probe_version: self.port_probe_version,
        };
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_string_pretty(&persisted)
            .context("failed to serialize plugin database")?;
        fs::write(&tmp, data)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, path)
            .with_context(|| format!("failed to rename {} to {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

/// Standard cache location: `%LOCALAPPDATA%\daw_01\plugin_database.json`
/// on Windows, equivalent on other platforms via the `dirs` crate.
pub fn default_cache_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|p| p.join("daw_01").join("plugin_database.json"))
}

/// Stable identifiers for daw_01-bundled (`PluginFormat::Builtin`) plugins.
/// These URIs are used as both the descriptor `id` and the `path` field
/// in the cached `PluginEntry`, mirroring how external plugins use a
/// real filesystem path. The plugin host's `builtin` module dispatches
/// on the URI to construct the Rust implementation.
pub const BUILTIN_ID_SILENCE: &str = "builtin://daw_01.silence";
pub const BUILTIN_ID_VOICEVOX: &str = "builtin://daw_01.voicevox";
/// (talk) 字幕(テキスト表示)デバイス。`ClipContent::Text` clip を画面 overlay 化
/// するかの**表示ゲート marker** (`docs/plan_voicevox_talk.md` §1.2/§2)。shader 効果では
/// なく、`text_compose` が「このトラックに在るか」で表示を gate するだけ。`builtin.video.*`
/// 一族として video in/out port を立て、audio engine / plugin host から skip させる
/// (`PortConfig::is_video`)。`video_fx` の shader catalog には載せない (= FX executor が
/// `def_by_id` 未ヒットで素通り)。
pub const SUBTITLE_ID: &str = "builtin.video.subtitle";

/// Returns the canonical list of daw_01-bundled plugin descriptors.
/// `scan_system` appends these unconditionally so the picker UI sees
/// them on every fresh scan. Order here is the order the picker
/// receives them.
pub fn builtin_descriptors() -> Vec<PluginEntry> {
    let version = env!("CARGO_PKG_VERSION");
    let instrument_features = vec![
        "instrument".to_string(),
        "synthesizer".to_string(),
    ];
    let mut entries = vec![
        PluginEntry {
            id: BUILTIN_ID_SILENCE.to_string(),
            format: PluginFormat::Builtin,
            name: "Silence (builtin)".to_string(),
            vendor: "daw_01".to_string(),
            version: version.to_string(),
            // Mirror CLAP feature taxonomy so pickers that filter by
            // category route this to the instrument list.
            features: instrument_features.clone(),
            path: PathBuf::from(BUILTIN_ID_SILENCE),
            descriptor_index: 0,
            // 純粋音源: note を受け、 audio を出す。 note 出力は持たない。
            // audio を加工しない (= audio 入力なし) ので instrument 扱い。
            has_note_input: true,
            has_note_output: false,
            has_audio_output: true,
            has_audio_input: false,
            has_video_input: false,
            has_video_output: false,
        },
        PluginEntry {
            id: BUILTIN_ID_VOICEVOX.to_string(),
            format: PluginFormat::Builtin,
            name: "VOICEVOX (builtin)".to_string(),
            vendor: "daw_01".to_string(),
            version: version.to_string(),
            features: {
                let mut f = instrument_features;
                f.push("vocal".to_string());
                f
            },
            path: PathBuf::from(BUILTIN_ID_VOICEVOX),
            descriptor_index: 0,
            // 純粋音源 (歌唱合成): note を受け、 audio を出す。
            // audio を加工しない (= audio 入力なし) ので instrument 扱い。
            has_note_input: true,
            has_note_output: false,
            has_audio_output: true,
            has_audio_input: false,
            has_video_input: false,
            has_video_output: false,
        },
        // (talk) 字幕(テキスト表示)デバイス。video overlay marker。挿さっている
        // トラックの `ClipContent::Text` だけが画面に出る (`text_compose` が gate)。
        // video in/out を立てて audio engine / plugin host から skip させる。
        PluginEntry {
            id: SUBTITLE_ID.to_string(),
            format: PluginFormat::Builtin,
            name: "字幕 (builtin)".to_string(),
            vendor: "daw_01".to_string(),
            version: version.to_string(),
            features: vec![
                "video-overlay".to_string(),
                "text".to_string(),
            ],
            path: PathBuf::from(SUBTITLE_ID),
            descriptor_index: 0,
            has_note_input: false,
            has_note_output: false,
            has_audio_output: false,
            has_audio_input: false,
            has_video_input: true,
            has_video_output: true,
        },
    ];
    // docs/plan_video_fx.md §9: 内蔵映像効果 (`builtin.video.*`) を
    // SSoT (`crate::video_fx` カタログ) から列挙する。映像 device は GUI 描画パスで
    // 処理されるため audio/note port は全 false、video in/out を立てる。これにより
    // engine の `process_track_owned` は `slot_to_plugin_id` 未登録の index として
    // skip し (= 音声バス素通り)、plugin host へは load 要求が飛ばない (daw_gui 側で抑止)。
    for def in crate::video_fx::builtin_video_fx() {
        entries.push(PluginEntry {
            id: def.id.to_string(),
            format: PluginFormat::Builtin,
            name: def.name.to_string(),
            vendor: "daw_01".to_string(),
            version: version.to_string(),
            features: vec![
                "video-effect".to_string(),
                def.category.feature_tag().to_string(),
            ],
            path: PathBuf::from(def.id),
            descriptor_index: 0,
            has_note_input: false,
            has_note_output: false,
            has_audio_output: false,
            has_audio_input: false,
            has_video_input: true,
            has_video_output: true,
        });
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_by_id_hit() {
        let db = PluginDatabase {
            entries: vec![PluginEntry {
                id: "com.example.foo".into(),
                format: PluginFormat::Clap,
                name: "Foo".into(),
                vendor: "Example".into(),
                version: "1.0".into(),
                features: vec!["instrument".into()],
                path: PathBuf::from("C:\\foo.clap"),
                descriptor_index: 0,
                has_note_input: false,
                has_note_output: false,
                has_audio_output: true,
                // instrument: audio を生成するだけで加工しない。
                has_audio_input: false,
                has_video_input: false,
                has_video_output: false,
            }],
            scanned_at: Some(42),
            port_probe_version: 0,
        };
        assert_eq!(
            db.find_by_id("com.example.foo").map(|e| e.name.as_str()),
            Some("Foo")
        );
    }

    #[test]
    fn find_by_id_miss() {
        let db = PluginDatabase::default();
        assert!(db.find_by_id("missing").is_none());
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.json");
        let db = PluginDatabase {
            entries: vec![PluginEntry {
                id: "com.test.x".into(),
                format: PluginFormat::Clap,
                name: "X".into(),
                vendor: String::new(),
                version: "0.1.0".into(),
                features: vec![],
                path: PathBuf::from("/tmp/x.clap"),
                descriptor_index: 2,
                has_note_input: false,
                has_note_output: false,
                has_audio_output: true,
                // 外部 audio エフェクト (audio in/out)。
                has_audio_input: true,
                has_video_input: false,
                has_video_output: false,
            }],
            scanned_at: Some(100),
            port_probe_version: 0,
        };
        db.save_to_file(&path).unwrap();
        let loaded = PluginDatabase::load_from_file(&path).unwrap().unwrap();
        // load_from_file re-injects builtins, so compare against the
        // expected sanitised shape rather than the raw saved entries.
        let mut expected = db.clone();
        expected.ensure_builtins();
        assert_eq!(loaded.entries, expected.entries);
        assert_eq!(loaded.scanned_at, db.scanned_at);
        // The original external entry survives the round-trip.
        assert!(loaded.find_by_id("com.test.x").is_some());
    }

    #[test]
    fn load_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.json");
        assert!(PluginDatabase::load_from_file(&path).unwrap().is_none());
    }

    #[test]
    fn ensure_builtins_injects_voicevox() {
        // 空 DB (= builtin 無しの古い cache 相当) に注入すると VOICEVOX /
        // Silence が現れる。これが無いと vocal track のロードが失敗する。
        let mut db = PluginDatabase::default();
        assert!(db.find_by_id(BUILTIN_ID_VOICEVOX).is_none());
        db.ensure_builtins();
        let e = db
            .find_by_id(BUILTIN_ID_VOICEVOX)
            .expect("voicevox present after ensure_builtins");
        assert_eq!(e.format, PluginFormat::Builtin);
        assert!(db.find_by_id(BUILTIN_ID_SILENCE).is_some());
    }

    #[test]
    fn ensure_builtins_is_idempotent() {
        // 二度呼んでも builtin が重複しない (format で除外してから再注入)。
        let mut db = PluginDatabase::default();
        db.ensure_builtins();
        let n = db.entries.len();
        db.ensure_builtins();
        assert_eq!(db.entries.len(), n);
    }

    #[test]
    fn save_to_file_excludes_builtins() {
        // builtin を載せた DB を save しても、ディスクには外部 plugin だけが
        // 残る (builtin はコードが SSoT)。load は ensure_builtins で復元する。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.json");
        let mut db = PluginDatabase {
            entries: vec![PluginEntry {
                id: "com.test.x".into(),
                format: PluginFormat::Clap,
                name: "X".into(),
                vendor: String::new(),
                version: "0.1.0".into(),
                features: vec![],
                path: PathBuf::from("/tmp/x.clap"),
                descriptor_index: 0,
                has_note_input: false,
                has_note_output: false,
                has_audio_output: true,
                // 外部 audio エフェクト (audio in/out)。
                has_audio_input: true,
                has_video_input: false,
                has_video_output: false,
            }],
            scanned_at: Some(1),
            port_probe_version: 0,
        };
        db.ensure_builtins();
        db.save_to_file(&path).unwrap();
        // Inspect the raw on-disk JSON: builtins must be excluded from
        // persistence (load_from_file re-injects them, so we can't check
        // via the loaded DB).
        let on_disk: PluginDatabase =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(on_disk.find_by_id(BUILTIN_ID_VOICEVOX).is_none());
        assert!(on_disk.find_by_id(BUILTIN_ID_SILENCE).is_none());
        assert_eq!(on_disk.entries.len(), 1);
        assert_eq!(on_disk.entries[0].id, "com.test.x");
        // After load the external entry is still present alongside builtins.
        let loaded = PluginDatabase::load_from_file(&path).unwrap().unwrap();
        assert!(loaded.find_by_id("com.test.x").is_some());
        assert!(loaded.find_by_id(BUILTIN_ID_VOICEVOX).is_some());
    }
}
