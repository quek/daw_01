//! S3b-1: AppData state group (VoicevoxState)。 docs/plan_arch_refactor.md §7.5
//! の分割表に従って app.rs の AppData から機械移送したフィールド群。

use std::sync::Arc;

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
        }
    }
}
