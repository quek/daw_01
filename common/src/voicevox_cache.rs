//! VOICEVOX 合成結果 (WAV bytes) の **永続コンテンツアドレスキャッシュ** (FIXME #77)。
//!
//! 歌唱 wav は `build_sing_query(notes, bpm)` が作る query JSON + singer_id の、
//! 読み上げ wav は `(text, speaker_id, TalkParams)` の **純粋関数**。 同じ内容なら
//! 必ず同じ wav なので、 内容ハッシュをキーにディスクへ保存し、 2 回目以降は
//! HTTP 合成 (1 音 約数秒) を丸ごとスキップできる。
//!
//! 旧実装は in-memory・プロセス寿命のみで、 プロジェクトを開き直すたびに全曲を
//! 再合成していた。 本キャッシュは per-user global (`%LOCALAPPDATA%\daw_01\
//! voicevox_cache\`、 [`crate::app_dirs::AppDirs::voicevox_cache_dir`]) に置くので:
//!
//! - プロジェクトを開き直しても再合成しない (= #77 の本旨)
//! - 同じフレーズはプロジェクト跨ぎで再利用される (コンテンツアドレスの利点)
//! - 合成プロセス (daw_plugin_host) も同じ root を env ベースで解決できる
//!
//! 値は VOICEVOX が返した WAV bytes を**そのまま** `<hex>.wav` に保存する
//! (再エンコードなし、 単体で再生でき debug しやすい)。 `note_offsets` は notes
//! から決定的に再計算できるのでキャッシュしない (= note_id 非依存・安定キー)。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::model::TalkParams;

/// 安定 64-bit キー。 `DefaultHasher` は固定キー (0,0) の SipHash-1-3 なので
/// **プロセス間で安定** (randomize されるのは `RandomState` の方)。 Rust の
/// toolchain 更新で稀にアルゴリズムが変わり得るが、 その場合も「キーが変わる =
/// cache miss = 再合成 1 回」 で graceful に劣化するだけ (= 破損しない)。
pub type CacheKey = u64;

/// ディスクキャッシュ総量の上限。 超えたら mtime 最古から削除する (bounded growth)。
/// 48kHz mono の歌唱で 1 分 ≈ 5.5 MB (PCM16 WAV)、 1 GiB で約 3 時間ぶん。
const MAX_CACHE_BYTES: u64 = 1024 * 1024 * 1024;

/// temp ファイル名のユニーク化カウンタ (同プロセス内の並行 put 衝突回避)。
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 歌唱合成のキャッシュキー = `hash(query_json, singer_id)`。 query は
/// `build_sing_query` の出力 (= notes の pitch / frame_length / lyric + bpm が
/// 畳み込まれた決定的な文字列)。 base_beat 相対なので clip の絶対位置に依存しない。
pub fn key_for_sing(query_json: &str, singer_id: u32) -> CacheKey {
    let mut h = DefaultHasher::new();
    "sing".hash(&mut h);
    query_json.hash(&mut h);
    singer_id.hash(&mut h);
    h.finish()
}

/// 読み上げ合成のキャッシュキー = `hash(text, speaker_id, scales)`。 talk wav は
/// `/audio_query` + scales patch + `/synthesis` の決定的な結果。
pub fn key_for_talk(text: &str, speaker_id: u32, scales: &TalkParams) -> CacheKey {
    let mut h = DefaultHasher::new();
    "talk".hash(&mut h);
    text.hash(&mut h);
    speaker_id.hash(&mut h);
    // f32 は Hash を実装しないので bit pattern を hash (NaN は VOICEVOX scale に
    // 来ないが、 来ても安定キーになる)。
    scales.speed_scale.to_bits().hash(&mut h);
    scales.pitch_scale.to_bits().hash(&mut h);
    scales.intonation_scale.to_bits().hash(&mut h);
    scales.volume_scale.to_bits().hash(&mut h);
    h.finish()
}

/// VOICEVOX 合成 WAV の永続ディスクキャッシュ。 1 キー = 1 ファイル
/// (`<dir>/<hex>.wav`)。 read は lock-free (ファイルを開いて読むだけ)、 write は
/// temp + atomic rename なので、 複数 builtin instance の synth thread が同時に
/// 触っても安全 (同キー並行 write は last-writer-wins)。
#[derive(Debug, Clone)]
pub struct VoiceVoxDiskCache {
    dir: PathBuf,
}

impl VoiceVoxDiskCache {
    /// 任意ディレクトリをキャッシュ root にする (test 用 = tempdir 注入)。
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// production の per-user キャッシュ (`AppDirs::production().voicevox_cache_dir()`)。
    /// local data dir が解決できない極端な環境では `None` (= キャッシュ無効)。
    pub fn production() -> Option<Self> {
        Some(Self::at(crate::app_dirs::AppDirs::production()?.voicevox_cache_dir()))
    }

    fn path_for(&self, key: CacheKey) -> PathBuf {
        self.dir.join(format!("{key:016x}.wav"))
    }

    /// キャッシュから WAV bytes を取得。 無ければ `None`。 読み取り失敗
    /// (権限 / 破損) も `None` 扱い (= miss して再合成へ graceful fallback)。
    pub fn get(&self, key: CacheKey) -> Option<Vec<u8>> {
        std::fs::read(self.path_for(key)).ok()
    }

    /// WAV bytes をキャッシュへ保存。 temp + atomic rename。 失敗は握り潰す
    /// (= キャッシュは最適化であって、 書けなくても合成自体は成功しているため)。
    /// 書き込み後に総量上限を超えていれば古いものを GC する。
    pub fn put(&self, key: CacheKey, wav: &[u8]) {
        if let Err(e) = self.put_inner(key, wav) {
            tracing::debug!(error = ?e, "voicevox cache: put 失敗 (無視)");
            return;
        }
        self.prune();
    }

    fn put_inner(&self, key: CacheKey, wav: &[u8]) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let final_path = self.path_for(key);
        // 既存ヒットを無駄に書き直さない (mtime も保つ)。
        if final_path.exists() {
            return Ok(());
        }
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let tmp = self.dir.join(format!(".{key:016x}.{pid}.{seq}.tmp"));
        std::fs::write(&tmp, wav)?;
        // atomic rename。 同キー並行 put は last-writer-wins (内容は同一なので無害)。
        match std::fs::rename(&tmp, &final_path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// 総量が [`MAX_CACHE_BYTES`] を超えていたら mtime 最古から削除して収める。
    fn prune(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        // (mtime, size, path) を収集。 .wav のみ対象 (temp は除外)。
        let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
        let mut total: u64 = 0;
        for ent in entries.flatten() {
            let path = ent.path();
            if path.extension().and_then(|e| e.to_str()) != Some("wav") {
                continue;
            }
            let Ok(meta) = ent.metadata() else { continue };
            let len = meta.len();
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            total += len;
            files.push((mtime, len, path));
        }
        if total <= MAX_CACHE_BYTES {
            return;
        }
        files.sort_by_key(|(mtime, _, _)| *mtime); // 古い順
        for (_, len, path) in files {
            if total <= MAX_CACHE_BYTES {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(len);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        // テスト隔離用の一意ディレクトリ (std::env::temp_dir 下)。 Date/random を
        // 使わず、 テスト名 + atomic seq で衝突回避。
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("vvcache_test_{pid}_{seq}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn key_sing_stable_and_singer_sensitive() {
        let q = r#"{"notes":[{"key":60,"frame_length":40,"lyric":"ら"}]}"#;
        assert_eq!(key_for_sing(q, 3061), key_for_sing(q, 3061));
        assert_ne!(key_for_sing(q, 3061), key_for_sing(q, 6000));
        let q2 = r#"{"notes":[{"key":62,"frame_length":40,"lyric":"ら"}]}"#;
        assert_ne!(key_for_sing(q, 3061), key_for_sing(q2, 3061));
    }

    #[test]
    fn key_talk_sensitive_to_text_and_scales() {
        let s = TalkParams::default();
        assert_eq!(key_for_talk("こんにちは", 3, &s), key_for_talk("こんにちは", 3, &s));
        assert_ne!(key_for_talk("こんにちは", 3, &s), key_for_talk("さようなら", 3, &s));
        let s2 = TalkParams { speed_scale: 1.5, ..s };
        assert_ne!(key_for_talk("こんにちは", 3, &s), key_for_talk("こんにちは", 3, &s2));
    }

    #[test]
    fn put_then_get_roundtrips() {
        let dir = tempdir();
        let cache = VoiceVoxDiskCache::at(&dir);
        let key = 0xdead_beefu64;
        assert!(cache.get(key).is_none());
        let wav = vec![1u8, 2, 3, 4, 5];
        cache.put(key, &wav);
        assert_eq!(cache.get(key), Some(wav));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn put_is_idempotent_and_keeps_first_bytes() {
        let dir = tempdir();
        let cache = VoiceVoxDiskCache::at(&dir);
        let key = 7u64;
        cache.put(key, &[1, 1, 1]);
        // 同キー再 put は既存を保つ (上書きしない)。
        cache.put(key, &[2, 2, 2]);
        assert_eq!(cache.get(key), Some(vec![1, 1, 1]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_keeps_total_under_limit() {
        // MAX_CACHE_BYTES は実値だと大きすぎてテストに不向きなので、 ここでは
        // prune が「上限超で古いものから消す」 ロジックを、 数件で total <= limit
        // の不変だけ確認する (limit 自体は const なので、 数 KB では prune
        // しない = 何も消えない正常系)。
        let dir = tempdir();
        let cache = VoiceVoxDiskCache::at(&dir);
        for k in 0..5u64 {
            cache.put(k, &[0u8; 1024]);
        }
        // 5 KB < 1 GiB なので全件残る。
        for k in 0..5u64 {
            assert!(cache.get(k).is_some(), "key {k} should survive");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
