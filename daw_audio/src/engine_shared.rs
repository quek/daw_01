//! Audio engine の **共有面** (`engine.rs` から分離、`docs/plan_project_tabs.md` §3):
//! IPC 受信ループ / export thread / notify thread と CPAL コールバックが wait-free に
//! 共有する構造体群。
//!
//! - [`SharedState`] — デバイス全体のフラグ面 (panic / park / app_active) とアイドル park。
//! - [`PluginEntry`] / [`SyncSlot`] / [`WorkerRig`] — plugin dispatch の pair と隔離
//!   (poisoning contract は `common::plugin_ref`)。
//! - [`ProjectShared`] — **プロジェクト (= タブ) ごと**の transport とミラー。
//! - [`EngineShared`] — デバイス全体のミラーと開いている project の一覧。
//!
//! RT 私有側 (`ProjectRt` / `DeviceRt` / `RtBundle`) は `engine.rs`。

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use arc_swap::{ArcSwap, ArcSwapOption};
use common::model::Song;
use common::plugin_ref::{PluginRef, WorkerSyncRef};
use common::protocol::{InstanceToken, ProjectKey};
use common::worker_bridge::WorkerBridgeHandle;

use crate::audio_clip_renderer::AudioClipRenderer;
use crate::audio_worker::AudioWorkerPool;
use crate::engine::{MAX_TRACKS, PlaybackCommand};
use crate::sampler::SamplerRig;

/// `pending_seek` の sentinel = 「seek 要求なし」。playhead はサンプル単位で、
/// この値 (u64::MAX サンプル ≈ 数百万年) に達することは現実的に無いので
/// 「要求なし」を表す番兵に使える。
pub const NO_PENDING_SEEK: u64 = u64::MAX;

/// デバイス全体の wait-free フラグ面 (IPC 受信ループが書き、audio thread が
/// 毎 buffer 読む)。project ごとの transport は [`ProjectShared`]。
pub struct SharedState {
    /// パニックボタンの declick トリガ。 IPC スレッドが
    /// `AudioCommand::Panic` で `true` を store、 CPAL コールバックが各 buffer 頭で
    /// `swap(false)` して master を fade-out → hold へ入れる。 panic が全 plugin を
    /// mix から外す瞬間の段差クリックを、 master を先にフェードミュートして隠す
    /// ための edge フラグ。デバイス最終ミックス (全 project の合算) に掛かる。
    pub panic_declick: AtomicBool,
    /// declick の hold を解除して fade-in へ移すトリガ。 daw_gui が
    /// `ReinitAllPlugins` の完了 (`PluginsReinitDone`) を確認してから
    /// `AudioCommand::PanicRelease` で `true` を store する。 これで master の
    /// ミュート解除を「固定タイマー」 ではなく「reinit が実際に終わった瞬間」 に
    /// 結びつけ、 GUI メインスレッド stall や巨大 reinit でも、 plugin が mix に
    /// 残ったまま master が戻る (= クリック / reverb tail 復活) ことを防ぐ。
    pub panic_release: AtomicBool,
    /// r.md #49: daw_01 の窓 (メイン / 動画プレビュー / プラグインエディタ) のいずれかが
    /// アクティブか。daw_gui が `AudioCommand::SetAppActive` で更新する。
    ///
    /// **これは park の条件の 1 つでしかない**。park してよいかは engine が決める
    /// (§`idle_park_state`)。起動時は true — daw_gui からの最初の報告が届くまで
    /// 「非アクティブ」と誤認して park しないため。
    pub app_active: AtomicBool,
    /// park 条件が連続して成立しているサンプル数。1 つでも崩れたら 0 に戻る。
    /// CPAL コールバック単独の writer。
    pub idle_silent_samples: AtomicU64,
    /// park すべき状態に達した。CPAL コールバックが立て、notify thread が
    /// `Stream::pause()` を実行する (コールバック内から stream は触れない)。
    pub park_requested: AtomicBool,
}

/// r.md #49: 無音かつアイドルがこの秒数続いたら CPAL stream を pause する。
///
/// 「音が消えてから」数えるので、リバーブの残響や自走プラグイン (VCV Rack 等) が
/// 鳴っている間はカウンタが進まず park しない = ブツッと切れる音は構造的に出ない。
pub const IDLE_PARK_DELAY_SECS: u64 = 5;

/// 「無音」とみなす master ピークの閾値 (≈ -120 dBFS)。24bit の LSB より
/// 十分下なので、可聴音を無音と誤判定することはない。
pub const IDLE_SILENCE_PEAK: f32 = 1.0e-6;

/// r.md #49: 今 buffer が park 条件を満たすか。
///
/// CPAL コールバックから atomic の読み値だけで呼ぶ純関数 (RT 安全 — 確保も
/// ロックも I/O もしない)。`playing` は engine の内部状態 = 「走っているか」の
/// 唯一の所有者で、GUI 側はこれを観測している (r.md #51)。複数 project では
/// 「どれか 1 つでも走っている / count-in 中」で呼ぶ。
#[must_use]
pub fn buffer_is_idle(
    app_active: bool,
    playing: bool,
    preroll_remaining: u64,
    export_running: bool,
    peak_l: f32,
    peak_r: f32,
) -> bool {
    !app_active
        && !playing
        && preroll_remaining == 0
        && !export_running
        && peak_l.abs() < IDLE_SILENCE_PEAK
        && peak_r.abs() < IDLE_SILENCE_PEAK
}

/// r.md #51: この buffer の末尾で transport を畳む (停止 or loop wrap) べきか。
///
/// - loop が有効なら loop 終端で wrap する (録音中も同じ — ループ録音は
///   「範囲を繰り返す」意図なので、そこで曲末判定を持ち出さない)。
/// - loop が無効なら曲末で停止する。ただし **`keep_rolling` なら停止しない**。
///
/// `keep_rolling` に含まれるのは 2 つ:
/// - **録音中** — 録音は曲の後ろへ素材を継ぎ足す操作でもあり、最後のクリップの
///   末尾で勝手に止まると曲の続きを録れない (参照 DAW 5 製品とも曲末で録音を
///   止めない)。
/// - **ランチャーが鳴っている** (r.md #87) — セルはアレンジのクリップの外側で
///   自分の時間軸を回すので、「アレンジの曲末」は再生をやめる理由にならない。
///   ここを見落とすと、アレンジが空の曲でセルを撃った瞬間に曲末判定が立ち、
///   **1 小節ほど鳴ってから勝手に停止する** (= ループしないように見える。
///   トランスポートのループを ON にすると曲末判定が使われなくなるので直る、
///   という紛らわしい症状になる)。
#[must_use]
pub fn reached_transport_end(
    keep_rolling: bool,
    active_loop_end: Option<u64>,
    new_playhead: u64,
    song_has_ended: bool,
) -> bool {
    match active_loop_end {
        Some(end) => new_playhead >= end,
        None => !keep_rolling && song_has_ended,
    }
}

/// r.md #49: 連続アイドルサンプル数の更新。アイドルでない buffer が 1 つでも
/// 挟まったら 0 に戻る (= 「連続して」の意味)。加算後の値を返す。
///
/// **`load` → 加算 → `store` ではなく `fetch_add` で行う**。カウンタは
/// コールバック以外 (receive loop の `wake_stream`) も 0 へ落とすので、
/// load と store の間に入った reset を取りこぼすと、**起こした直後に古い
/// カウント値が復活して即座に park し直す** = 再生開始と同時に無音になる。
pub fn advance_idle_counter(counter: &AtomicU64, idle: bool, frames: u64) -> u64 {
    if idle {
        counter
            .fetch_add(frames, Ordering::AcqRel)
            .saturating_add(frames)
    } else {
        counter.store(0, Ordering::Release);
        0
    }
}


impl SharedState {
    pub fn new() -> Self {
        Self {
            panic_declick: AtomicBool::new(false),
            panic_release: AtomicBool::new(false),
            // 起動直後は「アクティブ」から始める。daw_gui の最初の
            // `SetAppActive` が届く前に park してしまうのを防ぐ。
            app_active: AtomicBool::new(true),
            idle_silent_samples: AtomicU64::new(0),
            park_requested: AtomicBool::new(false),
        }
    }
}

impl Default for SharedState {
    fn default() -> Self {
        Self::new()
    }
}

/// 1 loaded plugin instance ぶんの daw_audio 側リソース。`plugin_refs`
/// (device_id → entry) の値。map の clone は `Arc` の refcount bump なので、
/// recv loop での snapshot-copy-mutate-publish が安価。
pub struct PluginEntry {
    /// shmem 上の `ProcessData` への参照 (device_id / token 込み)。
    pub plugin_ref: PluginRef,
    /// plan §4: dispatch timeout でこの device を隔離した。以後の buffer は
    /// この device を skip (= bypass) し、**pd (shmem) にも触らない** —
    /// timeout した `process()` は plugin_host 側でまだ走っている可能性が
    /// あり、入力を書くと並行 process と race する。plugin_host respawn /
    /// SetSlotPlugin 再ロード (= 新 entry) で解除。
    pub quarantined: AtomicBool,
    /// `AudioEvent::PluginUnresponsive` を送ったか (notify thread の dedup)。
    pub unresponsive_notified: AtomicBool,
    /// daw_audio 側の shmem mapping を entry の寿命に束ねる (旧 `Box::leak`
    /// の解消 — plan §4)。entry が map から外れ、全 snapshot (RT bundle /
    /// mirror / export guard) が死ぬと off-thread で unmap される。
    /// テストは heap の `ProcessData` を直接指すので `None`。
    pub _handle: Option<common::process_data::ProcessDataHandle>,
}

impl PluginEntry {
    pub fn new(
        device_id: u64,
        token: InstanceToken,
        handle: common::process_data::ProcessDataHandle,
    ) -> Self {
        Self {
            plugin_ref: PluginRef {
                device_id,
                token,
                process_data: handle.ptr(),
            },
            quarantined: AtomicBool::new(false),
            unresponsive_notified: AtomicBool::new(false),
            _handle: Some(handle),
        }
    }

    /// テスト用: shmem を立てず heap 上の `ProcessData` を指す entry。
    #[cfg(test)]
    pub(crate) fn for_test(
        device_id: u64,
        process_data: *mut common::process_data::ProcessData,
    ) -> Self {
        Self {
            plugin_ref: PluginRef {
                device_id,
                token: InstanceToken(device_id),
                process_data,
            },
            quarantined: AtomicBool::new(false),
            unresponsive_notified: AtomicBool::new(false),
            _handle: None,
        }
    }
}

/// device_id (安定 `PluginInstance::id`、**その project 内の名前**) → entry。
/// schedule / song 側の `devices[i].id` からこの map を直接引く (positional slot
/// map は v29 で廃止)。project ごとに 1 つ ([`ProjectShared::plugin_refs`])。
pub type PluginRefs = HashMap<u64, Arc<PluginEntry>>;

/// 1 worker handshake pair (audio worker i ↔ plugin_host worker i)。
pub struct SyncSlot {
    pub sync: WorkerSyncRef,
    /// plan §4 poisoning contract: dispatch timeout を観測した pair は
    /// 以後 dispatch 禁止 (auto-reset done event に待ち手なし signal が
    /// 残留し、次 dispatch が「走行中の process と並行に入力を書く」事故に
    /// なるため)。pool 再構築 (= 新 `WorkerRig`) まで立ちっぱなし。
    pub poisoned: AtomicBool,
}

/// worker pool 一式 (plugin_host との handshake 面 + audio 側 worker threads)。
/// recv loop が `OpenWorkerPool` で off-thread 構築し、[`DeviceBundle`] で RT へ
/// 配送する。旧 rig は recycle ring 経由で off-thread drop — `AudioWorkerPool`
/// の Drop (worker join) が RT を塞がない (plan §4)。デバイス全体で 1 つ
/// (全 project が共有し、project ごとに直列に dispatch する)。
///
/// **フィールド順序が drop 順序**: `pool` (worker threads join — slots の
/// raw pointer を deref し得る) → `slots` → `bridge` (slots の
/// `worker_task` ptr の backing shmem) の順で落とすこと。
pub struct WorkerRig {
    /// `None` = `AudioWorkerPool::new` 失敗 (serial fallback で slot 0 のみ使用)。
    pub pool: Option<AudioWorkerPool>,
    pub slots: Vec<SyncSlot>,
    /// shmem mapping を保持 (`slots[*].sync.worker_task` の backing)。
    pub bridge: WorkerBridgeHandle,
    /// `AudioEvent::WorkerPoolStalled` を送ったか (notify thread の dedup)。
    pub stall_notified: AtomicBool,
}

/// r.md #40: off-thread で確保した stretch engine を RT の `TrackScratch` へ
/// 渡す配送便。 RT は `engines` を `pop` して
/// `TrackScratch::stretch_engines` へ `push` し (予約済み容量内なので再確保なし)、
/// 空になった本体を recycle ring へ返す (`Vec` の解放を off-thread に追い出す)。
pub struct StretchPoolDelivery {
    pub track_idx: usize,
    pub engines: Vec<crate::stretch_engine::StretchEngine>,
}

/// 配送 ring の深さ。1 回の publish で最大 `MAX_TRACKS` 便が積まれるので、
/// RT が 1 buffer 遅れても溢れないよう 2 倍取る。
const STRETCH_POOL_RING_CAP: usize = MAX_TRACKS * 2;

/// **プロジェクト (= タブ) ごと**の共有面 (`docs/plan_project_tabs.md` §3.1)。
///
/// transport / seek / loop / preroll / recording は audio thread が毎 buffer 読む
/// wait-free 面。`plugin_refs` / renderer / `device_latencies` / `project_dir` は
/// off-RT 読者 (export / notify / decode) 向けのミラーで、**RT はこれらの `ArcSwap` を
/// load しない** — RT へは [`RtBundle`] が配送される。
pub struct ProjectShared {
    pub key: ProjectKey,
    pub song: ArcSwapOption<Song>,
    pub playback: AtomicU8,
    /// 再生ループの状態 (ON/OFF + 範囲)。 ループは `Song` ではなく GUI の
    /// session state が所有するので、`LoadSong` ではなく `AudioCommand::SetLoop`
    /// だけがここを書き換える (`common::model::LoopRegion`)。 ON/OFF と範囲を
    /// 別々の atomic に割らないのは、 audio thread が 1 buffer 内で整合した
    /// スナップショットを読むため (`recording_lanes` と同じ `ArcSwap` idiom)。
    pub loop_region: ArcSwap<common::model::LoopRegion>,
    /// Last published playhead in samples. Mirrored to shmem for the GUI
    /// playhead cursor. **書き込みは audio thread (`process_buffer`) 単独**。
    /// IPC スレッドは seek を `pending_seek` に積むだけで、ここを直接書かない
    /// (直接書くと buffer 末の advance store と race して、Stop 直後
    /// に停止位置へ巻き戻る = 開始位置に戻らないバグになる)。
    pub playhead: AtomicU64,
    /// GUI からの `SeekTo` 要求を audio thread に渡す single-writer
    /// チャネル。IPC 受信スレッドが目標サンプル位置を `store`、audio thread が
    /// `process_buffer` 冒頭で `swap` 消費して `playhead` に反映する。これにより
    /// `playhead` の writer を audio thread 単独に保ち、停止/seek の競合を排除する。
    /// `NO_PENDING_SEEK` = 要求なし。多重要求は last-wins。
    pub pending_seek: AtomicU64,
    /// Phase 4 Step C-2 (`docs/plan_automation.md` §6): currently recording
    /// lane set (= GUI が `SetRecordingLanes` で更新)。 audio thread は
    /// 各 buffer の頭で `load()` し、 `fill_track_param_ramps` で該当 lane
    /// の curve eval を bypass する。 `(track_id, AutomationTarget)` の
    /// 2 つ組で identify (lane_id を使わないのは GUI 側で lane を削除して
    /// から audio に通知が届くまでの race を避けるため = target 一致なら
    /// bypass で済む)。 起動時は空。
    pub recording_lanes:
        ArcSwap<std::collections::HashSet<(u32, common::model::AutomationTarget)>>,
    /// メトロノーム on/off。 GUI が `AudioCommand::SetMetronomeEnabled` で更新、
    /// audio thread が `render_metronome` で読む。 false なら click 生成を
    /// skip (= 無音)。 起動時 default false。
    pub metronome_enabled: AtomicBool,
    /// device_id → `PluginEntry` のミラー (export / notify 用)。RT へは
    /// 同じ `Arc<PluginEntry>` 群が bundle で渡るので quarantine フラグは
    /// 両者で共有される。
    pub plugin_refs: ArcSwap<PluginRefs>,
    /// MIDI Capture の試聴シーケンス。recv loop が載せ、RT が buffer 頭で読む
    /// (`None` = 停止)。差し替えは `generation` で検出する。
    pub preview_sequence: ArcSwapOption<crate::sampler::PreviewSequence>,
    /// Audio clip render snapshot. Built off-thread in
    /// `compile_audio_schedule` and published via `ArcSwap`. The
    /// audio thread `load()`s once per buffer to find events that
    /// overlap the current playhead range. Empty until imports start
    /// landing.
    pub audio_clip_renderer: ArcSwap<AudioClipRenderer>,
    /// Monotonic schedule version, bumped on every `LoadSong`. The background
    /// decode worker stamps each job with the generation at dispatch and only
    /// publishes its fully-decoded renderer if this is still current, so a slow
    /// decode for a superseded song can't clobber a newer schedule
    /// (r.md #7 decode 再設計 B)。
    pub schedule_generation: AtomicU64,
    /// 直近 `LoadSong` で読み込んだ `Song::project_id` (v24 で導入された
    /// プロジェクト同一性の SSoT)。同じ slot で値が変わった瞬間が「このタブに
    /// 別のファイルが開かれた」(空の Untitled タブへ Open した) であり、Song
    /// スコープの id を key にした engine 側の状態 (`plugin_refs` /
    /// `recording_lanes` / RT の走行状態) をまとめて捨てる唯一の検出点。`0` = 未ロード。
    pub loaded_project_id: AtomicU64,
    /// Guards publication of a freshly compiled `audio_clip_renderer` so a slow
    /// background decode for an older generation can't clobber a newer one
    /// (closes the TOCTOU between the generation re-check and the `ArcSwap`
    /// store). Holds the highest generation published so far. Off the audio
    /// thread — the CPAL callback never takes this lock (r.md #7 B)。
    pub last_published_generation: std::sync::Mutex<u64>,
    /// r.md #40: stretch engine pool の off-thread → RT 配送口。
    ///
    /// `StretchEngine` は 1 個 ~1 MB を確保するので **RT では作れない**。
    /// `publish_audio_clip_schedule` が新 schedule の
    /// `AudioClipRenderer::engines_per_track` を見て不足分を作り、ここへ push する
    /// (schedule を `ArcSwap` に store する **前**に push するので、RT が新
    /// schedule を見るときには pool が届いている)。 producer が 2 つある
    /// (recv loop / decode worker) ので `Mutex` で直列化する — off-thread なので
    /// ロックしてよい (RT 側は `ProjectRt::stretch_pool_rx` を lock-free に drain)。
    pub stretch_pool_tx: std::sync::Mutex<rtrb::Producer<StretchPoolDelivery>>,
    /// RT が空にした配送便 (= `Vec` の heap 実体) を返す口。 RT で `drop` すると
    /// free になるので、ここへ push して off-thread で捨てる
    /// (`input_delay_replacements` の recycle と同じ idiom)。
    pub stretch_pool_recycle_rx: std::sync::Mutex<rtrb::Consumer<StretchPoolDelivery>>,
    /// track ごとに **配送済み**のエンジン数 (= `TrackScratch::stretch_engines` の
    /// 長さ)。 pool は grow-only: 一度作ったエンジンは走行中のストリームを壊さない
    /// よう回収しない (縮めると `Vec` 全体を差し替えることになり、無関係な発音まで
    /// prime し直しになる)。 `last_published_generation` を保持したまま触る。
    pub delivered_engines_per_track: std::sync::Mutex<Vec<u16>>,
    /// Current project directory, used to resolve
    /// `AudioSourcePath::ProjectRelative`. `None` for unsaved projects
    /// — `ProjectRelative` paths fail to resolve in that state and the
    /// caller is expected to use `Absolute` (import_cache fallback).
    /// Updated by `AudioCommand::SetProjectDir`.
    pub project_dir: ArcSwapOption<PathBuf>,
    /// Phase 7 B4 Step C (2026-05-13): count-in 用 preroll の合計 samples
    /// (= 録音開始時に GUI が `StartRecording { preroll_samples }` で立てた値の
    /// snapshot)。 `process_buffer` で `elapsed = total - remaining` を
    /// 計算して metronome の click trigger 用 playhead として使う。 0 で
    /// count-in 中ではない。
    pub preroll_total_samples: AtomicU64,
    /// Phase 7 B4 Step C: count-in 残り samples。 audio thread が毎 buffer
    /// `frames` だけ deduct + audio_bridge mirror 経由で GUI に publish。
    /// 0 到達で通常再生に戻る (= dispatch / clip render 復帰)。
    pub preroll_remaining_samples: AtomicU64,
    /// r.md #51: 録音セッションが開いているか (`StartRecording` で true、
    /// `StopRecording` で false)。 audio thread はこれを 2 つに使う:
    ///
    /// 1. **曲末 auto-stop の抑止** — 録音は曲の後ろへ継ぎ足すものなので、
    ///    最後のクリップの末尾で勝手に止まってはいけない (参照 DAW 5 製品とも
    ///    曲末で録音を止めない)。
    /// 2. `recording_live` の publish — 「今ノートを書いてよいか」は
    ///    count-in 明けを知っている engine だけが正しく言える。
    pub recording_requested: AtomicBool,
    /// master volume (f32 bits)。 recv loop が `SetMasterGain` で store、
    /// render (`render_master_buffer`) が load して master へ掛ける。live /
    /// export 共通 (§5 — 旧実装は CPAL interleave 段のみで export に乗らず、
    /// master gain が WAV に反映されなかった)。
    pub master_gain: AtomicU32,
    /// 安定 `device_id` → プラグインが報告した processing latency (samples)。
    /// `AudioCommand::SetDeviceLatency` で recv loop が差し替え、
    /// `compile_schedule` (live publish / export の両方) が PDC の入力に読む。
    ///
    /// r.md #9: 報告値は実行時の観測値なので `Song` には載せない (載せると保存され、
    /// 開き直しで host の報告と食い違って「開いただけで `*`」 になる)。 track /
    /// master の合計は compile 側が device chain から導出するので、 GUI は集計しない。
    /// 読むのは off-RT (recv loop / export thread) のみ。
    pub device_latencies: ArcSwap<crate::graph::DeviceLatencies>,
    /// `AudioBridge` の telemetry slot (claim 時に確定、RT はこの index で書く)。
    pub telemetry_slot: usize,
}

impl ProjectShared {
    /// `ProjectShared` と、RT (`ProjectRt`) が持つべき stretch ring の片割れを作る。
    pub fn new_with_stretch_rings(
        key: ProjectKey,
        telemetry_slot: usize,
    ) -> (
        Self,
        rtrb::Consumer<StretchPoolDelivery>,
        rtrb::Producer<StretchPoolDelivery>,
    ) {
        let (tx, rx) = rtrb::RingBuffer::new(STRETCH_POOL_RING_CAP);
        let (recycle_tx, recycle_rx) = rtrb::RingBuffer::new(STRETCH_POOL_RING_CAP);
        let mut shared = Self::new(key, telemetry_slot);
        shared.stretch_pool_tx = std::sync::Mutex::new(tx);
        shared.stretch_pool_recycle_rx = std::sync::Mutex::new(recycle_rx);
        (shared, rx, recycle_tx)
    }

    pub fn new(key: ProjectKey, telemetry_slot: usize) -> Self {
        // 呼び出し側が `new_with_stretch_rings` を使わない (テスト等) 場合は、
        // 相手のいない ring を持つ = 配送は起きないが panic もしない。
        let (tx, _rx) = rtrb::RingBuffer::new(1);
        let (_recycle_tx, recycle_rx) = rtrb::RingBuffer::new(1);
        Self {
            key,
            song: ArcSwapOption::empty(),
            playback: AtomicU8::new(PlaybackCommand::Stop as u8),
            loop_region: ArcSwap::from_pointee(common::model::LoopRegion::default()),
            playhead: AtomicU64::new(0),
            pending_seek: AtomicU64::new(NO_PENDING_SEEK),
            recording_lanes: ArcSwap::from_pointee(std::collections::HashSet::new()),
            metronome_enabled: AtomicBool::new(false),
            plugin_refs: ArcSwap::from_pointee(HashMap::new()),
            preview_sequence: ArcSwapOption::empty(),
            audio_clip_renderer: ArcSwap::from_pointee(AudioClipRenderer::empty()),
            schedule_generation: AtomicU64::new(0),
            loaded_project_id: AtomicU64::new(0),
            last_published_generation: std::sync::Mutex::new(0),
            stretch_pool_tx: std::sync::Mutex::new(tx),
            stretch_pool_recycle_rx: std::sync::Mutex::new(recycle_rx),
            delivered_engines_per_track: std::sync::Mutex::new(Vec::new()),
            project_dir: ArcSwapOption::empty(),
            preroll_total_samples: AtomicU64::new(0),
            preroll_remaining_samples: AtomicU64::new(0),
            recording_requested: AtomicBool::new(false),
            master_gain: AtomicU32::new(1.0_f32.to_bits()),
            device_latencies: ArcSwap::from_pointee(HashMap::new()),
            telemetry_slot,
        }
    }
}

/// 開いている project の一覧 (recv loop が書く off-RT ミラー。notify / export が読む)。
pub type Projects = HashMap<ProjectKey, Arc<ProjectShared>>;

/// Engine resources shared with off-RT readers: the offline-export thread
/// and the notify thread. デバイス全体で 1 つ。project ごとの面は
/// [`ProjectShared`] (`projects` から引く)。
pub struct EngineShared {
    /// 開いている project (recv loop が `OpenProject` / `CloseProject` で差し替え)。
    pub projects: ArcSwap<Projects>,
    /// worker rig のミラー (export / notify 用)。
    pub worker: ArcSwapOption<WorkerRig>,
    /// Global Sampler のリングのミラー ([`DeviceBundle`] に載せる元)。
    pub sampler: ArcSwapOption<SamplerRig>,
    /// Set by the export thread while it owns the audio path. CPAL
    /// callback skips its `process_buffer` and writes silence so the
    /// export render can drive `plugin.process()` exclusively.
    /// **全 project が止まる** (オフライン描画はエンジン全体で 1 本)。
    pub export_running: AtomicBool,
    /// Cancel request for the in-flight offline render. The daw_audio
    /// receive loop resets it to `false` *before* spawning each export
    /// thread (in the `ExportWav` / `BounceClipFxOnline` handlers), so the
    /// reset is FIFO-ordered against a later `AudioCommand::CancelExport`
    /// and a stale cancel from a previous render can't abort the next one.
    /// `run_export` / the freewheel loop only **read** it (every buffer)
    /// and abort (deleting the partial WAV) when set.
    pub export_cancel: AtomicBool,
    /// set `true` by the CPAL callback once it observes
    /// `export_running` and parks (writes silence, skips dispatch); set `false`
    /// on any normal (non-parked) buffer. The export thread sets
    /// `export_running` then waits for this to go `true` before it dispatches,
    /// guaranteeing the live callback's *in-flight* buffer has fully drained —
    /// otherwise two drivers would race on the shared plugin-host worker slots
    /// ("プラグインで処理がぶつかる"). It is the single-producer (CPAL callback)
    /// flag the single-consumer (export thread) polls.
    pub live_parked: AtomicBool,
    /// 直近の CPAL callback が処理した frames (= device period)。 audio
    /// thread が毎 buffer store し、recv loop が schedule compile の
    /// `buffer_frames` (leaf 宛 sidechain tap の 1-buffer 補償量) に使う。
    /// `0` = 未計測 (compile 側は 10ms 相当へ fallback)。
    pub last_buffer_frames: AtomicU32,
    /// CPAL callback thread の MMCSS "Pro Audio" join が失敗した (E:
    /// callback 初回に自前 join する — 失敗ログは RT で出せないので notify
    /// thread が 1 回だけ warn する)。
    pub mmcss_join_failed: AtomicBool,
    /// `mmcss_join_failed` の warn を出したか (notify thread の dedup)。
    pub mmcss_warned: AtomicBool,
    /// `ScopeBridge` (マスターメーター) と Global Sampler の `Master` ソースが
    /// 書く project (`ProjectKey.0`、0 = 無し)。`AudioCommand::SetScopeProject` =
    /// アクティブなタブ。
    pub scope_project: AtomicU64,
}

impl EngineShared {
    pub fn new() -> Self {
        Self {
            projects: ArcSwap::from_pointee(HashMap::new()),
            worker: ArcSwapOption::empty(),
            sampler: ArcSwapOption::empty(),
            export_running: AtomicBool::new(false),
            export_cancel: AtomicBool::new(false),
            live_parked: AtomicBool::new(false),
            last_buffer_frames: AtomicU32::new(0),
            mmcss_join_failed: AtomicBool::new(false),
            mmcss_warned: AtomicBool::new(false),
            scope_project: AtomicU64::new(0),
        }
    }

    /// `key` の project (recv loop のミラーから)。閉じたタブ / 未 open は `None`。
    #[must_use]
    pub fn project(&self, key: ProjectKey) -> Option<Arc<ProjectShared>> {
        self.projects.load().get(&key).cloned()
    }

    #[must_use]
    pub fn scope_project(&self) -> ProjectKey {
        ProjectKey(self.scope_project.load(Ordering::Acquire))
    }
}

impl Default for EngineShared {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod idle_park_tests {
    use super::*;

    /// アクティブ / 再生 / count-in / 書き出し / 可聴音 のどれか 1 つでも
    /// あれば park しない。
    #[test]
    fn park_conditions_are_all_required() {
        // (app_active, playing, preroll, export, peak_l, peak_r, expect_idle)
        let cases = [
            (false, false, 0, false, 0.0, 0.0, true),
            (true, false, 0, false, 0.0, 0.0, false),   // アクティブ
            (false, true, 0, false, 0.0, 0.0, false),   // 再生中
            (false, false, 1, false, 0.0, 0.0, false),  // count-in
            (false, false, 0, true, 0.0, 0.0, false),   // 書き出し中
            (false, false, 0, false, 0.01, 0.0, false), // L が鳴っている
            (false, false, 0, false, 0.0, -0.01, false), // R が鳴っている (負値)
        ];
        for (active, playing, preroll, export, l, r, expect) in cases {
            assert_eq!(
                buffer_is_idle(active, playing, preroll, export, l, r),
                expect,
                "active={active} playing={playing} preroll={preroll} export={export} l={l} r={r}"
            );
        }
    }

    /// r.md #51: 録音中は曲末で止まらない。ループが有効なら録音中でも wrap する。
    #[test]
    fn recording_suppresses_song_end_stop_but_not_loop_wrap() {
        // (recording, loop_end, playhead, song_ended, expect_reached_end)
        let cases = [
            // ループ無し + 曲末: 再生は止まる / 録音中とランチャー走行中は止まらない
            // (`keep_rolling`)。
            (false, None, 1_000, true, true),
            (true, None, 1_000, true, false),
            // ループ無し + 曲の途中: どちらも止まらない。
            (false, None, 1_000, false, false),
            (true, None, 1_000, false, false),
            // ループ有効: 録音中でも終端で wrap する (曲末判定は使わない)。
            (true, Some(900), 1_000, false, true),
            (true, Some(1_200), 1_000, true, false),
        ];
        for (keep_rolling, loop_end, playhead, ended, expect) in cases {
            assert_eq!(
                reached_transport_end(keep_rolling, loop_end, playhead, ended),
                expect,
                "keep_rolling={keep_rolling} loop_end={loop_end:?} ph={playhead} ended={ended}"
            );
        }
    }

    /// 残響が鳴り止むまでカウンタは進まず、鳴り止んでから閾値まで数える。
    #[test]
    fn counter_restarts_after_audible_buffer() {
        let threshold = IDLE_PARK_DELAY_SECS * 48_000;
        let frames = 512;
        let counter = AtomicU64::new(0);
        // 無音が続いて閾値の手前まで到達。
        let mut n = 0;
        while n < threshold - frames {
            n = advance_idle_counter(&counter, true, frames);
        }
        assert!(n < threshold, "まだ park しない");
        // ここで 1 buffer だけ音が鳴る (= 残響 / 自走プラグイン) → 振り出しに戻る。
        n = advance_idle_counter(&counter, false, frames);
        assert_eq!(n, 0);
        // 鳴り止んだ後、改めて閾値ぶん数え直して park に至る。
        let mut buffers = 0;
        while n < threshold {
            n = advance_idle_counter(&counter, true, frames);
            buffers += 1;
        }
        assert_eq!(buffers, threshold.div_ceil(frames));
    }

    /// 別スレッド (receive loop の `wake_stream`) が挟んだ 0 リセットを
    /// 取りこぼさない = 起こした直後に古いカウントが復活しない。
    #[test]
    fn external_reset_is_not_clobbered() {
        let threshold = IDLE_PARK_DELAY_SECS * 48_000;
        let frames = 512;
        let counter = AtomicU64::new(threshold - frames);
        // コマンド受信でカウンタが 0 に落とされた直後に、アイドルのままの
        // buffer が 1 つ走るケース。加算は 0 からやり直す。
        counter.store(0, Ordering::Release);
        let n = advance_idle_counter(&counter, true, frames);
        assert_eq!(n, frames, "リセット前の値へ戻ってはいけない");
        assert!(n < threshold, "起こした直後に park し直してはいけない");
    }
}
