//! S3b-1: AppData state group (VoicevoxState)。 docs/plan_arch_refactor.md §7.5
//! の分割表に従って app.rs の AppData から機械移送したフィールド群。

use std::sync::Arc;

use crate::app::VocalSynthStatus;
use crate::dispatcher::JobDispatcher;

/// (r.md #61) lazy spawn した VOICEVOX engine の受け渡し口。
///
/// **spawn は background thread**、**停止は終了シーケンス** という 2 者が
/// 同じ 1 つの子プロセスを扱うので、「もう終了に入った」を slot 自身が持つ。
/// これが無いと、`is_running()` の HTTP タイムアウト (最大 1 秒) を待っている
/// 間に終了が始まり、`JobHandle::close()` の **後** に spawn が成功して
/// 「Job にも入らず kill もされない engine」が孤児化する
/// (localhost:50021 と GPU メモリを掴んだまま残り、次回起動では
/// `is_running()` が true になるので二度と回収されない)。
#[derive(Default)]
pub struct VoicevoxEngineSlot {
    /// 我々が起動した engine。終了シーケンスが take して kill する。
    pub child: Option<std::process::Child>,
    /// 終了シーケンスに入った。以後 spawn が成功しても即 kill する。
    pub shutting_down: bool,
}

pub struct VoicevoxState {
    /// VOICEVOX engine `/singers` の結果。 起動時に background thread が
    /// `AppEvent::SingersLoaded` で投入する。 engine 未起動 / fetch 失敗時は
    /// 空のまま (Clip Inspector の声 dropdown は焼き込み声名 + 「取得中…」表示)。
    /// Clip Inspector の 2 段 dropdown (キャラ→style) が直接読む。
    pub singers: Vec<crate::voicevox_client::VoiceVoxSinger>,
    /// (talk) VOICEVOX engine `/speakers` の結果 (`docs/plan_voicevox_talk.md` §4)。
    /// Text clip の talk 声 dropdown (キャラ→talk style) が直接読む。sing の `singers`
    /// (= `/singers`) とは別 id 空間。engine 起動時に background thread が
    /// `AppEvent::SpeakersLoaded` で投入。未取得なら焼き込み声名 + 「取得中…」表示。
    pub talk_speakers: Vec<crate::voicevox_client::VoiceVoxSinger>,
    /// VOICEVOX engine の auto-kill 用 Job dispatcher。
    /// production は `Win32JobDispatcher` (`JobHandle::assign_std` ラップ)、
    /// test は `NoopJobDispatcher`。 trait DI により AppData::new の
    /// 引数は OS-API 抽象だけで完結する。
    pub voicevox_job: Arc<dyn JobDispatcher>,
    /// (r.md #61) **我々が spawn した** VOICEVOX engine の子プロセス。
    /// ユーザーが自分で立ち上げていた engine (`is_running()` が true だった
    /// ケース) はここに入らない = 終了時に殺さない。
    ///
    /// 旧実装は `job.assign_std` した直後に `std::mem::forget(child)` して
    /// handle ごと捨てており、停止手段が Job Object の `CloseHandle` しか
    /// 無かった (= 終了シーケンスが engine の停止を所有できない)。background
    /// thread が spawn するので `Arc<Mutex<_>>` で受け渡す。
    pub spawned_engine: Arc<std::sync::Mutex<VoicevoxEngineSlot>>,
    /// VOICEVOX engine 起動を 1 度だけ trigger するためのフラグ。 lazy 起動:
    /// 起動時 auto-launch せず、 Vocal track 選択 / Synth ボタン押下等で初めて
    /// `ensure_voicevox_engine()` が `true` にして background spawn する。
    pub voicevox_launch_attempted: bool,
    /// 口パク (lip-sync) 自動再生成の debounce 用世代カウンタ。song 変更で
    /// bump し、`mark_lipsync_dirty` が timer thread に値を渡す。timer 発火時に
    /// 値が一致していれば (= それ以降変更なし) 再生成する (rapid 編集を coalesce)。
    pub lipsync_gen: u64,
    /// 口パク (lip-sync) 背景生成が in-flight な出力先 (口 track id) の集合。
    /// `regenerate_lipsync_for_track` の spawn 時に insert、`LipsyncGenerated` 受信で remove。
    /// クリップ上スピナー / 全体オーバーレイ「口パク生成中」の駆動に使う (派生 UI 状態、非保存)。
    pub lipsync_inflight: std::collections::HashSet<u32>,
    /// 口パク再生成の入力 fingerprint。target (口) track id → 最後に再生成を
    /// 発注した時点の入力ハッシュ (`lipsync_input_fingerprint`)。`LipsyncDebounceFired`
    /// で現在値と比較し、入力 (notes / 歌詞 / bpm / mouth_map / binding / clip 位置) が
    /// 変わった target だけ再生成する。track rename / 色 / mute / volume 等、口パク
    /// 出力に無関係な編集では fingerprint が変わらず再生成をスキップする (派生状態、非保存)。
    pub lipsync_fingerprints: std::collections::HashMap<u32, u64>,
    /// builtin VOICEVOX (歌唱/読み上げ) 合成の per-plugin 状態。key = 安定
    /// device id (v29)。
    /// `VoicevoxSynthStatus` IPC で更新。`busy` = 合成中、`failing_since = Some` は直近 HTTP が
    /// 失敗中 (= engine 未起動/起動途中)。一定時間 (= `VOICEVOX_ENGINE_WARNING`) 続いたら
    /// engine 未接続警告へ切り替える。plugin unload (`SlotPluginUnloadedFromChild`) で entry を消す。
    pub voicevox_synth_status: std::collections::HashMap<u64, VocalSynthStatus>,
    /// r.md #27: builtin VOICEVOX device ごとに、最後に `SetBuiltinPluginNoteMetadata`
    /// で送った `(bpm, chunk_secs, notes, talk)`。`sync_vocal_metadata` は epoch bump のたび
    /// (= あらゆる編集) に呼ばれるが、この device の歌唱/読み上げ入力が前回送信から
    /// 変わっていなければ **再送しない** (= builtin plugin が不要な再合成を走らせない。
    /// Transform 等の非 vocal 編集で VOICEVOX 合成が走る問題の修正)。差分検出で送信を
    /// 抑える `sync_ara_documents` の `ara_doc_cache` と同 idiom。device (re)load 時に
    /// 該当 entry を破棄して初回 seed 合成を保証する (`SlotPluginLoadedFromChild`)。
    pub voicevox_metadata_sent: std::collections::HashMap<
        u64,
        (
            f32,
            f32,
            Vec<common::plugin_metadata::NoteMetadata>,
            Vec<common::plugin_metadata::TalkMetadata>,
        ),
    >,
    /// r.md #75: builtin VOICEVOX device ごとに、最後に `SetVocalSynthPriority` で送った
    /// 再生ヘッド位置 (拍)。1 拍以上動いたときだけ再送するための記憶 (= トランスポート中
    /// でも IPC は数 Hz 以下に収まる)。**再合成はトリガしない**軽量ヒントなので、
    /// `voicevox_metadata_sent` (再送デデュープ) とは別に持つ。
    pub priority_sent: std::collections::HashMap<u64, f64>,
}

impl VoicevoxState {
    /// 起動時の初期状態。engine は lazy 起動なので、ここでは何も spawn しない
    /// (`voicevox_launch_attempted` が false のまま `ensure_voicevox_engine` を待つ)。
    ///
    /// **初期化はこの group の定義の隣に置く** — `AppData::new` の巨大な struct literal
    /// に並べると、field を 1 つ足すたびに app.rs の実コード行が増えてサイズ budget
    /// (不変条件 9) を押し上げる。`state/*` へ分けた意図どおり、group ごとに閉じる。
    #[must_use]
    pub fn new(voicevox_job: Arc<dyn JobDispatcher>) -> Self {
        Self {
            singers: Vec::new(),
            talk_speakers: Vec::new(),
            voicevox_job,
            spawned_engine: Arc::new(std::sync::Mutex::new(VoicevoxEngineSlot::default())),
            voicevox_launch_attempted: false,
            lipsync_gen: 0,
            lipsync_inflight: std::collections::HashSet::new(),
            lipsync_fingerprints: std::collections::HashMap::new(),
            voicevox_synth_status: std::collections::HashMap::new(),
            voicevox_metadata_sent: std::collections::HashMap::new(),
            priority_sent: std::collections::HashMap::new(),
        }
    }
}
