//! VOICEVOX の **永続コンテンツアドレスキャッシュ** (合成 WAV と query 応答 JSON)。
//!
//! 歌唱フレーズ WAV は `build_sing_query_with(phrase.notes, bpm, carry_in)` が作る
//! query JSON + singer_id + 塊の長さ の、読み上げ WAV は `(text, speaker_id,
//! TalkParams)` の、`/sing_frame_audio_query` 応答は **楽譜 JSON** の純粋関数。
//! 同じ内容なら必ず同じ結果なので、内容ハッシュをキーにディスクへ保存し、
//! 2 回目以降は HTTP を丸ごとスキップできる。
//!
//! per-user global (`%LOCALAPPDATA%\daw_01\voicevox_cache\`、
//! [`crate::app_dirs::AppDirs::voicevox_cache_dir`]) に置くので:
//!
//! - プロジェクトを開き直しても再合成しない
//! - 同じフレーズはプロジェクト跨ぎで再利用される (コンテンツアドレスの利点)
//! - **daw_gui (口パク query) と daw_plugin_host (塊 query / 合成 WAV) の両方**が
//!   同じ root を env ベースで解決して共有できる。これがこのモジュールを
//!   `daw_plugin_host` から `common` へ移した理由 (daw_gui は plugin host に依存しない)。
//!
//! 値は VOICEVOX が返した bytes を**そのまま** `<hex>.wav` / `<hex>.json` に保存する
//! (再エンコードなし、単体で再生 / 検査でき debug しやすい)。ただし
//! **`/sing_frame_audio_query` 応答は必ず [`crate::voicevox::normalize_frame_query`] を
//! 通してから put すること** — 鍵空間を 2 プロセスで共有しているので、片方が
//! `outputSamplingRate` 未 patch の生 body を置くと、もう片方がそれをスライスして
//! 24 kHz の WAV を得る (`key_for_sing_query` の doc 参照)。
//!
//! `note_offsets` は notes から決定的に再計算できるのでキャッシュしない
//! (= note_id 非依存・安定キー)。
//!
//! ## 2 プロセス同時 prune のレース
//!
//! put は tmp + rename なので安全だが、`prune` の read_dir → remove は daw_gui と
//! daw_plugin_host で並走し得る。最悪「今書いたばかりのエントリを他方が消す」= 次回
//! miss して再合成、で終わる (致命ではない)。ロックは張らない。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use crate::model::TalkParams;

/// 安定 64-bit キー。 `DefaultHasher` は固定キー (0,0) の SipHash-1-3 なので
/// **プロセス間で安定** (randomize されるのは `RandomState` の方)。 Rust の
/// toolchain 更新で稀にアルゴリズムが変わり得るが、 その場合も「キーが変わる =
/// cache miss = 再合成 1 回」 で graceful に劣化するだけ (= 破損しない)。
pub type CacheKey = u64;

/// キャッシュ値の種別 = 拡張子。**GC の予算には両方を数える**
/// (旧実装は `.wav` しか見ておらず、JSON を足すと上限の外で無制限に増えた)。
/// プロセス内の計算専用で IPC を渡らない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    /// 合成 WAV (`<hex>.wav`)。
    Wav,
    /// `/sing_frame_audio_query` / `/audio_query` の応答 JSON (`<hex>.json`)。
    Json,
}

impl CacheKind {
    fn ext(self) -> &'static str {
        match self {
            CacheKind::Wav => "wav",
            CacheKind::Json => "json",
        }
    }
}

/// ディスクキャッシュ総量の上限。 超えたら mtime 最古から削除する (bounded growth)。
/// 48kHz mono の歌唱で 1 分 ≈ 5.5 MB (PCM16 WAV)、 1 GiB で約 3 時間ぶん。
const MAX_CACHE_BYTES: u64 = 1024 * 1024 * 1024;

/// hit したエントリの mtime を「今」に進める間隔。毎 hit で書くと 1 ジョブで数百回の
/// FS 書き込みになるので、mtime が既に十分新しいときは触らない (粗い LRU で足りる)。
const TOUCH_INTERVAL: Duration = Duration::from_secs(3600);

/// 前回 `prune` してから書いた bytes 数。これが [`PRUNE_INTERVAL_BYTES`] を超えたときだけ
/// GC する (プロセス内の合算で十分)。
static BYTES_SINCE_PRUNE: AtomicU64 = AtomicU64::new(u64::MAX);

/// GC を走らせる間隔 (前回からの書き込み量)。
///
/// `prune` は `read_dir` + 全エントリ `metadata()` の全走査なので、**put のたびに
/// 呼ぶと重い**。r.md #75 で put の粒度が「曲に 1 回」から「フレーズに 1 回」
/// (5 分の曲で 200 回超) になり、1 GiB のキャッシュでは数千ファイルの stat を
/// 毎フレーズ繰り返すことになった。上限を 64 MiB だけ超過し得るが、
/// [`MAX_CACHE_BYTES`] は 1 GiB なので実害は無い。
///
/// 初期値 `u64::MAX` なので **プロセス最初の put では必ず prune する**
/// (前回終了時に上限を超えたまま終わっていても、そこで畳まれる)。
const PRUNE_INTERVAL_BYTES: u64 = 64 * 1024 * 1024;

/// temp ファイル名のユニーク化カウンタ (同プロセス内の並行 put 衝突回避)。
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// キャッシュキーの世代。**合成結果の中身を決める定義を変えたら必ず +1 する**
/// (query 文字列や scales に現れない変更 — engine へ注入するパラメータ、WAV の
/// 後処理など — は key に自然には反映されないため)。旧 wav を黙って掴んで
/// 「直したのに変わらない」という誤診に直結する (r.md #39)。
///
/// - 1: 初期
/// - 2: r.md #39 — talk に `prePhonemeLength = 0` を注入 (先頭無音を消す)
/// - 3: r.md #75 — 塊クエリ + フレーズ単位 frame_synthesis へ分割 (合成内容の定義が変わる)
const CACHE_SCHEMA_VERSION: u32 = 3;

/// 歌唱 **フレーズ WAV** のキー
/// = `hash(schema, "sing-phrase", phrase_query_json, singer_id, chunk_secs.to_bits())`。
///
/// `phrase_query_json` は **そのフレーズ単体で**
/// `build_sing_query_with(&ph.notes, bpm, ph.carry_in)` を通した JSON = フレーズ先頭
/// 起点の相対 frame 列 + 解決後の歌詞 + pitch。よって他フレーズの編集・クリップの
/// 移動 / 分割 / 複製・トラック名変更では変わらない。
///
/// **`chunk_secs` を混ぜるのは、それが合成結果を実際に変える入力だから**
/// (30 秒の塊は全体 1 クエリ基準で 4.12 dB ばらつくのに対し 60 秒は 2.30 dB)。
/// 混ぜないと「設定つまみを動かしても全フレーズが cache hit して音が 1 サンプルも
/// 変わらない」= このモジュールの doc が禁じている「直したのに変わらない」誤診に
/// なる。混ぜた結果、設定変更は**曲全体の再合成**を意味する (それが正しい挙動。旧値の
/// エントリは別キーで残るので、戻せば即座に鳴る)。
///
/// 一方 **「その塊に他のどのフレーズが同居していたか」(塊の構成) は意図的にキーへ
/// 入れない**。入れると 1 音の編集で塊内の全フレーズが miss し、この設計の目的
/// (「1 音直す = 1 フレーズだけ再合成」) が消える。構成差で音量がどれだけ動くかは
/// pad 0.5 s のとき最大 0.67 dB / σ 0.31 dB (= 全体 1 クエリを 2 回投げ直したときの
/// ノイズ下限 0.86 dB と同オーダー)。
///
/// **エンジンが返した FrameAudioQuery のスライスをキーにしてはいけない**
/// (`/sing_frame_audio_query` は非決定的 = max|Δf0| 31.7 Hz。毎回 miss する)。
#[must_use]
pub fn key_for_sing_phrase(phrase_query_json: &str, singer_id: u32, chunk_secs: f32) -> CacheKey {
    let mut h = DefaultHasher::new();
    "sing-phrase".hash(&mut h);
    CACHE_SCHEMA_VERSION.hash(&mut h);
    phrase_query_json.hash(&mut h);
    singer_id.hash(&mut h);
    // f32 は Hash を実装しないので bit pattern を hash。
    chunk_secs.to_bits().hash(&mut h);
    h.finish()
}

/// `/sing_frame_audio_query` **応答 JSON** のキー = `hash(schema, "sing-query", score_json)`。
/// speaker は常に [`crate::voicevox::QUERY_SPEAKER`] (= 6000) なので混ぜない。
///
/// **daw_gui の口パク query と daw_plugin_host の塊クエリはこの 1 本の鍵関数を共有する。**
/// ただし現状の 2 者は実際には衝突しない — 口パクは `build_sing_query`
/// (端 rest = `REST_FRAMES` = 10)、塊は `voicevox_phrase::build_chunk_query`
/// (端 rest = `PHRASE_PAD_FRAMES` = 47) なので、score JSON の `rest_start` /
/// `rest_end` が必ず違う。**これは誰も強制していない偶然の分離**なので、
/// 「当たらないから何を put してもよい」とはしない:
///
/// **値は必ず [`crate::voicevox::normalize_frame_query`] を通してから put すること。**
/// これを不変条件にしておけば、将来どちらかの端 rest が変わって鍵が一致しても、
/// 掴んだ側が 24 kHz 指定の query をスライスして 24 kHz の WAV を得る事故が起きない。
#[must_use]
pub fn key_for_sing_query(score_json: &str) -> CacheKey {
    let mut h = DefaultHasher::new();
    "sing-query".hash(&mut h);
    CACHE_SCHEMA_VERSION.hash(&mut h);
    score_json.hash(&mut h);
    h.finish()
}

/// (talk) `/audio_query` **応答 JSON** のキー = `hash(schema, "talk-query", text, speaker_id)`。
/// 応答は `(text, speaker)` の純粋関数で、speed / pitch 等は応答へ後から patch するので
/// 鍵に混ぜない (= 話速を変えても query は再取得しない)。
#[must_use]
pub fn key_for_talk_query(text: &str, speaker_id: u32) -> CacheKey {
    let mut h = DefaultHasher::new();
    "talk-query".hash(&mut h);
    CACHE_SCHEMA_VERSION.hash(&mut h);
    text.hash(&mut h);
    speaker_id.hash(&mut h);
    h.finish()
}

/// (talk) 合成 WAV のキー = `hash(schema, text, speaker_id, scales, pre)`。 talk wav は
/// `/audio_query` + params patch + `/synthesis` の決定的な結果。patch で注入する
/// `prePhonemeLength` は text / scales に現れないので明示的に混ぜる (これが無いと
/// 先頭無音付きの旧 wav が hit して修正が効かない — r.md #39)。
#[must_use]
pub fn key_for_talk(text: &str, speaker_id: u32, scales: &TalkParams) -> CacheKey {
    let mut h = DefaultHasher::new();
    "talk".hash(&mut h);
    CACHE_SCHEMA_VERSION.hash(&mut h);
    crate::voicevox::TALK_PRE_PHONEME_LENGTH.to_bits().hash(&mut h);
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

/// VOICEVOX の永続ディスクキャッシュ。 1 (キー, 種別) = 1 ファイル
/// (`<dir>/<hex>.<ext>`)。 read は lock-free (ファイルを開いて読むだけ)、 write は
/// temp + atomic rename なので、 複数プロセス / 複数 synth thread が同時に触っても
/// 安全 (同キー並行 write は last-writer-wins)。
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
    #[must_use]
    pub fn production() -> Option<Self> {
        Some(Self::at(crate::app_dirs::AppDirs::production()?.voicevox_cache_dir()))
    }

    fn path_for(&self, key: CacheKey, kind: CacheKind) -> PathBuf {
        self.dir.join(format!("{key:016x}.{}", kind.ext()))
    }

    /// キャッシュから bytes を取得。 無ければ `None`。 読み取り失敗
    /// (権限 / 破損) も `None` 扱い (= miss して再合成へ graceful fallback)。
    ///
    /// hit したら mtime を「今」へ進める (粗い LRU。[`TOUCH_INTERVAL`] より新しければ
    /// 触らない)。これが無いと実質 FIFO になり、フレーズ単位で小さいエントリが大量に
    /// 増える設計では「Undo で戻したら即鳴る」が成立しなくなる。
    #[must_use]
    pub fn get(&self, key: CacheKey, kind: CacheKind) -> Option<Vec<u8>> {
        let path = self.path_for(key, kind);
        let bytes = std::fs::read(&path).ok()?;
        touch_if_stale(&path);
        Some(bytes)
    }

    /// bytes をキャッシュへ保存。 temp + atomic rename。 失敗は握り潰す
    /// (= キャッシュは最適化であって、 書けなくても合成自体は成功しているため)。
    /// 書き込み後に総量上限を超えていれば古いものを GC する。
    pub fn put(&self, key: CacheKey, kind: CacheKind, bytes: &[u8]) {
        if let Err(e) = self.put_inner(key, kind, bytes) {
            tracing::debug!(error = ?e, "voicevox cache: put 失敗 (無視)");
            return;
        }
        // GC は毎 put ではなく **書いた量が [`PRUNE_INTERVAL_BYTES`] を超えたとき**に
        // 走らせる (`prune` はディレクトリ全走査なので、フレーズ単位の put で毎回
        // 呼ぶと合成時間に効いてくる)。
        let written = BYTES_SINCE_PRUNE
            .fetch_add(bytes.len() as u64, Ordering::Relaxed)
            .saturating_add(bytes.len() as u64);
        if written >= PRUNE_INTERVAL_BYTES {
            BYTES_SINCE_PRUNE.store(0, Ordering::Relaxed);
            self.prune();
        }
    }

    fn put_inner(&self, key: CacheKey, kind: CacheKind, bytes: &[u8]) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let final_path = self.path_for(key, kind);
        // 既存ファイルがあっても **上書きする**。`put` が呼ばれるのは miss の後だけ
        // (= 呼び出し側が HTTP で取り直した直後) なので、そこに残っているファイルは
        // 「壊れていて decode できなかったもの」か「並行して同じ内容を書いた誰か」。
        // 旧実装はここで早期 return していたため、**壊れたエントリを二度と直せなかった**
        // (しかも `get` が読めてしまう限り mtime が touch され続けて prune の対象にも
        // ならず、そのフレーズだけ毎回 HTTP で再合成し続ける)。
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let tmp = self.dir.join(format!(".{key:016x}.{pid}.{seq}.tmp"));
        std::fs::write(&tmp, bytes)?;
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
        self.prune_to(MAX_CACHE_BYTES);
    }

    /// 予算 `budget` bytes に収まるまで mtime 最古から削除する。
    /// **`.wav` と `.json` の両方**を数える (片方だけ数えると、数えない側が上限の外で
    /// 無制限に増える)。
    pub fn prune_to(&self, budget: u64) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        // (mtime, size, path) を収集。 .wav / .json のみ対象 (temp は除外)。
        let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
        let mut total: u64 = 0;
        for ent in entries.flatten() {
            let path = ent.path();
            let ext = path.extension().and_then(|e| e.to_str());
            if !matches!(ext, Some("wav") | Some("json")) {
                continue;
            }
            let Ok(meta) = ent.metadata() else { continue };
            let len = meta.len();
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            total += len;
            files.push((mtime, len, path));
        }
        if total <= budget {
            return;
        }
        files.sort_by_key(|(mtime, _, _)| *mtime); // 古い順
        for (_, len, path) in files {
            if total <= budget {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(len);
            }
        }
    }
}

/// mtime が [`TOUCH_INTERVAL`] より古ければ「今」へ進める (粗い LRU)。
/// 失敗は握り潰す (キャッシュは最適化)。
fn touch_if_stale(path: &std::path::Path) {
    let now = SystemTime::now();
    // mtime が読めない (= 状態不明) ときは touch を試みる側に倒す。
    let stale = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .is_none_or(|m| now.duration_since(m).is_ok_and(|age| age > TOUCH_INTERVAL));
    if !stale {
        return;
    }
    if let Ok(f) = std::fs::File::options().write(true).open(path) {
        let _ = f.set_modified(now);
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
    fn key_sing_phrase_stable_and_singer_sensitive() {
        let q = r#"{"notes":[{"key":60,"frame_length":40,"lyric":"ら"}]}"#;
        assert_eq!(
            key_for_sing_phrase(q, 3061, 60.0),
            key_for_sing_phrase(q, 3061, 60.0)
        );
        assert_ne!(
            key_for_sing_phrase(q, 3061, 60.0),
            key_for_sing_phrase(q, 6000, 60.0)
        );
        let q2 = r#"{"notes":[{"key":62,"frame_length":40,"lyric":"ら"}]}"#;
        assert_ne!(
            key_for_sing_phrase(q, 3061, 60.0),
            key_for_sing_phrase(q2, 3061, 60.0)
        );
    }

    #[test]
    fn phrase_key_changes_with_chunk_secs() {
        // 同じ楽譜・同じ声でも塊の長さが違えば別キー (= 設定つまみが効くことの回帰)。
        // 混ぜないと「設定を変えても全 hit で音が 1 サンプルも変わらない」になる。
        let q = r#"{"notes":[{"key":60,"frame_length":40,"lyric":"ら"}]}"#;
        assert_ne!(
            key_for_sing_phrase(q, 3061, 60.0),
            key_for_sing_phrase(q, 3061, 120.0)
        );
    }

    #[test]
    fn key_talk_sensitive_to_text_and_scales() {
        let s = TalkParams::default();
        assert_eq!(key_for_talk("こんにちは", 3, &s), key_for_talk("こんにちは", 3, &s));
        assert_ne!(key_for_talk("こんにちは", 3, &s), key_for_talk("さようなら", 3, &s));
        let s2 = TalkParams { speed_scale: 1.5, ..s };
        assert_ne!(key_for_talk("こんにちは", 3, &s), key_for_talk("こんにちは", 3, &s2));
        // talk query は (text, speaker) だけ = 話速を変えても再取得しない。
        assert_eq!(key_for_talk_query("こんにちは", 3), key_for_talk_query("こんにちは", 3));
        assert_ne!(key_for_talk_query("こんにちは", 3), key_for_talk_query("こんにちは", 5));
    }

    #[test]
    fn lipsync_and_chunk_queries_do_not_share_a_key() {
        // 口パク query (端 rest = REST_FRAMES) と塊 query (端 rest = PHRASE_PAD_FRAMES) は
        // **別キー**になる。§3.1(e) / §9-9b が「今は当たらない」と書いている事実を機械で
        // 固定して、将来どちらかの端 rest を変えた人がここで気付けるようにする
        // (当たるようになっても正規化があるので壊れないが、**気付かずに**当たるのは避ける)。
        let notes = vec![crate::model::Note {
            id: 0,
            start_beat: 0.0,
            duration_beats: 1.0,
            pitch: 60,
            velocity: 100,
            lyric: Some("ら".into()),
            muted: false,
        }];
        let lipsync = crate::voicevox::build_sing_query(&notes, 120.0);
        let phrase = crate::voicevox_phrase::Phrase {
            speaker_id: 3061,
            notes: notes.clone(),
            note_ids: vec![0],
            clip_ids: vec![0],
            carry_in: None,
            start_beat: 0.0,
            end_beat: 1.0,
        };
        let chunk = crate::voicevox_phrase::build_chunk_query(&[phrase], None, 120.0);
        assert_ne!(
            key_for_sing_query(&lipsync.json),
            key_for_sing_query(&chunk.json),
            "端 rest が違うので別ハッシュ"
        );
    }

    #[test]
    fn put_then_get_roundtrips() {
        let dir = tempdir();
        let cache = VoiceVoxDiskCache::at(&dir);
        let key = 0xdead_beefu64;
        assert!(cache.get(key, CacheKind::Wav).is_none());
        let wav = vec![1u8, 2, 3, 4, 5];
        cache.put(key, CacheKind::Wav, &wav);
        assert_eq!(cache.get(key, CacheKind::Wav), Some(wav));
        // 種別が違えば別ファイル (同じキーでも混ざらない)。
        assert!(cache.get(key, CacheKind::Json).is_none());
        cache.put(key, CacheKind::Json, b"{}");
        assert_eq!(cache.get(key, CacheKind::Json), Some(b"{}".to_vec()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn put_replaces_a_broken_entry() {
        // `put` は miss の直後にしか呼ばれない = そこに残っているファイルは壊れている
        // (decode できなかった) か、並行して書かれた同一内容。**必ず上書きする**。
        // 旧実装は「既存なら早期 return」だったため、一度壊れたエントリは二度と直らず、
        // そのフレーズだけ毎回 HTTP で再合成し続けていた。
        let dir = tempdir();
        let cache = VoiceVoxDiskCache::at(&dir);
        let key = 7u64;
        // 壊れた (= decode できない) エントリを模して 0 byte を置く。
        cache.put(key, CacheKind::Wav, &[]);
        assert_eq!(cache.get(key, CacheKind::Wav), Some(Vec::new()));
        // 再合成した正しい bytes で置き換わる。
        cache.put(key, CacheKind::Wav, &[2, 2, 2]);
        assert_eq!(cache.get(key, CacheKind::Wav), Some(vec![2, 2, 2]));
        // 同内容の再 put は冪等 (内容が変わらない)。
        cache.put(key, CacheKind::Wav, &[2, 2, 2]);
        assert_eq!(cache.get(key, CacheKind::Wav), Some(vec![2, 2, 2]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_counts_json_toward_budget() {
        // 旧実装は `.wav` しか収集しなかったので、JSON を足すと上限の外で無制限に
        // 増えた。予算注入 (`prune_to`) で `.json` も削除対象になることを固定する。
        let dir = tempdir();
        let cache = VoiceVoxDiskCache::at(&dir);
        for k in 0..8u64 {
            cache.put(k, CacheKind::Json, &[0u8; 1024]);
        }
        // 8 KiB 置いてから 3 KiB 予算で prune → 半分以上消える。
        cache.prune_to(3 * 1024);
        let left = (0..8u64)
            .filter(|k| cache.get(*k, CacheKind::Json).is_some())
            .count();
        assert!(left <= 3, "json も予算に数えて削除される: 残り {left}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn get_touches_mtime_for_lru() {
        // 古い mtime を人工的に付けてから get → 「今」に進む (= LRU 化)。
        // 進まないと実質 FIFO で、フレーズ単位の細かいエントリでは
        // 「Undo で戻したら即鳴る」が成立しない。
        let dir = tempdir();
        let cache = VoiceVoxDiskCache::at(&dir);
        let key = 42u64;
        cache.put(key, CacheKind::Wav, &[9u8; 16]);
        let path = dir.join(format!("{key:016x}.wav"));
        let old = SystemTime::now() - Duration::from_secs(60 * 60 * 24 * 30);
        {
            let f = std::fs::File::options().write(true).open(&path).unwrap();
            f.set_modified(old).unwrap();
        }
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(before <= old + Duration::from_secs(1));
        assert!(cache.get(key, CacheKind::Wav).is_some());
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(after > before, "get が mtime を進める: {before:?} → {after:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
