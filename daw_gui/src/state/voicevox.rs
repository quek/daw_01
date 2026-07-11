//! S3b-1: AppData state group (VoicevoxState)。 docs/plan_arch_refactor.md §7.5
//! の分割表に従って app.rs の AppData から機械移送したフィールド群。

use std::sync::Arc;

use crate::app::VocalSynthStatus;
use crate::dispatcher::JobDispatcher;

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
    /// で送った `(bpm, notes, talk)`。`sync_vocal_metadata` は epoch bump のたび
    /// (= あらゆる編集) に呼ばれるが、この device の歌唱/読み上げ入力が前回送信から
    /// 変わっていなければ **再送しない** (= builtin plugin が不要な再合成を走らせない。
    /// Transform 等の非 vocal 編集で VOICEVOX 合成が走る問題の修正)。差分検出で送信を
    /// 抑える `sync_ara_documents` の `ara_doc_cache` と同 idiom。device (re)load 時に
    /// 該当 entry を破棄して初回 seed 合成を保証する (`SlotPluginLoadedFromChild`)。
    pub voicevox_metadata_sent: std::collections::HashMap<
        u64,
        (
            f32,
            Vec<common::plugin_metadata::NoteMetadata>,
            Vec<common::plugin_metadata::TalkMetadata>,
        ),
    >,
}
