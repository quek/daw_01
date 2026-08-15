//! Audio engine state + per-buffer driver (CPAL コールバック側)。
//!
//! 役割分担 (`docs/plan_arch_refactor.md` §4/§5):
//! - `SharedState` — transport / seek / metronome 等の wait-free フラグ面。
//!   IPC 受信ループが書き、audio thread が毎 buffer 読む。
//! - `EngineShared` — off-RT 読者 (export thread / notify thread) 向けの
//!   ミラー (`plugin_refs` / `worker`) と export 予約・preroll 等。**RT は
//!   これらの `ArcSwap` を load しない** — RT へは [`RtBundle`] が rtrb の
//!   forward ring で配送され、superseded bundle は recycle ring で off-thread
//!   drop される (RT で alloc / free / 最終 refcount drop が起きない)。
//! - `LocalState` — CPAL クロージャ専有の状態 (scratch / cached bundle)。
//!   [`LocalState::process_buffer`] が 1 buffer を駆動し、実 render は
//!   live/export 共通の [`crate::graph::render_master_buffer`] に委譲する。
//!
//! plugin dispatch は **有界** (`DISPATCH_TIMEOUT_MS`)。timeout した device は
//! [`PluginEntry::quarantined`]、pair は [`SyncSlot::poisoned`] で隔離され
//! (poisoning contract は `common::plugin_ref` module doc)、通知は notify
//! thread (`main.rs`) がフラグを poll して `AudioEvent` を送る。

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use arc_swap::{ArcSwap, ArcSwapOption};
use common::audio_bridge::AudioBridgeHandle;
use common::model::Song;
use common::plugin_ref::{PluginRef, WorkerSyncRef};
use common::timing::{effective_loop_bounds, song_ended};
use common::worker_bridge::WorkerBridgeHandle;

use crate::audio_clip_renderer::AudioClipRenderer;
use crate::audio_worker::AudioWorkerPool;
use crate::graph::{DelayLine, Schedule, render_master_buffer};
use crate::metronome::{ClickVoice, render_metronome};
use crate::mixer::TrackScratch;
use crate::sequencer::NoteTransition;

/// Hard cap on tracks the audio engine can render in a single buffer.
/// Picked to match `audio_bridge::MAX_TRACKS` so the per-track peak
/// meter doesn't fall off the GUI side.
pub const MAX_TRACKS: usize = 32;

/// 鍵盤プレビュー note の `note_id`。 sequencer が振る通し index (= 0.. の
/// 小さい値) と衝突しない sentinel。 CLAP/VST3 は `note_id` を無視し、 builtin
/// は key 一致で発音/停止するので、 on/off で同値であれば voice 対応が取れる。
const PREVIEW_NOTE_ID: u32 = u32::MAX;

/// IPC 受信ループから audio thread へ渡す軽量コマンド。毎 buffer 頭の
/// `pump_commands` で drain される。v29: shmem / worker pool の重い扱いは
/// recv loop 側で [`RtBundle`] に載る経路へ移設したので、残るのは
/// 鍵盤プレビューのみ。
pub enum EngineCommand {
    /// 鍵盤レーン click のプレビュー note-on (gui_01 #055)。 `track` は
    /// song.tracks の Vec index (= main.rs が `track_id` から現 song snapshot
    /// で解決済)、 `velocity` は normalized 0..=1。 `pump_commands` が該当
    /// track の `pending_preview` に積み、 `process_track_owned` が次の
    /// dispatch で frame 0 に注入する。
    PreviewNoteOn {
        track: usize,
        pitch: u8,
        velocity: f64,
    },
    /// 鍵盤プレビューの note-off (gui_01 #055)。 `track` は note-on と同じ
    /// Vec index。
    PreviewNoteOff { track: usize, pitch: u8 },
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PlaybackCommand {
    Stop = 0,
    Play = 1,
}

impl PlaybackCommand {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Play,
            _ => Self::Stop,
        }
    }
}

/// `pending_seek` の sentinel = 「seek 要求なし」。playhead はサンプル単位で、
/// この値 (u64::MAX サンプル ≈ 数百万年) に達することは現実的に無いので
/// 「要求なし」を表す番兵に使える。
pub const NO_PENDING_SEEK: u64 = u64::MAX;

/// State shared between the audio thread and the IPC receive loop /
/// future GUI commands. Every field is wait-free: the audio thread reads
/// these on every buffer; the IPC side writes them on each command.
pub struct SharedState {
    pub song: ArcSwapOption<Song>,
    pub playback: AtomicU8,
    /// 再生ループの状態 (ON/OFF + 範囲)。 ループは `Song` ではなく GUI の
    /// session state が所有するので、`LoadSong` ではなく `AudioCommand::SetLoop`
    /// だけがここを書き換える (`common::model::LoopRegion`)。 ON/OFF と範囲を
    /// 別々の atomic に割らないのは、 audio thread が 1 buffer 内で整合した
    /// スナップショットを読むため (`recording_lanes` と同じ `ArcSwap` idiom)。
    pub loop_region: arc_swap::ArcSwap<common::model::LoopRegion>,
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
        arc_swap::ArcSwap<std::collections::HashSet<(u32, common::model::AutomationTarget)>>,
    /// メトロノーム on/off。 GUI が `AudioCommand::SetMetronomeEnabled` で更新、
    /// audio thread が `render_metronome` で読む。 false なら click 生成を
    /// skip (= 無音)。 起動時 default false。
    pub metronome_enabled: AtomicBool,
    /// パニックボタンの declick トリガ。 IPC スレッドが
    /// `AudioCommand::Panic` で `true` を store、 CPAL コールバックが各 buffer 頭で
    /// `swap(false)` して master を fade-out → hold へ入れる。 panic が全 plugin を
    /// mix から外す瞬間の段差クリックを、 master を先にフェードミュートして隠す
    /// ための edge フラグ。
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
/// ロックも I/O もしない)。`playing` は engine の内部状態で、GUI の
/// `transport.is_playing` ではない (後者は Rec 単独の録音中に立たないので
/// park 判定に使えない)。
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

impl SharedState {
    pub fn new() -> Self {
        Self {
            song: ArcSwapOption::empty(),
            playback: AtomicU8::new(PlaybackCommand::Stop as u8),
            loop_region: arc_swap::ArcSwap::from_pointee(
                common::model::LoopRegion::default(),
            ),
            playhead: AtomicU64::new(0),
            pending_seek: AtomicU64::new(NO_PENDING_SEEK),
            recording_lanes: arc_swap::ArcSwap::from_pointee(
                std::collections::HashSet::new(),
            ),
            metronome_enabled: AtomicBool::new(false),
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
    /// shmem 上の `ProcessData` への参照 (device_id 込み)。
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
        handle: common::process_data::ProcessDataHandle,
    ) -> Self {
        Self {
            plugin_ref: PluginRef {
                device_id,
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
                process_data,
            },
            quarantined: AtomicBool::new(false),
            unresponsive_notified: AtomicBool::new(false),
            _handle: None,
        }
    }
}

/// device_id (安定 `PluginInstance::id`) → entry。schedule / song 側の
/// `devices[i].id` からこの map を直接引く (positional slot map は v29 で
/// 廃止)。
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
/// recv loop が `OpenWorkerPool` で off-thread 構築し、`RtBundle` で RT へ
/// 配送する。旧 rig は recycle ring 経由で off-thread drop — `AudioWorkerPool`
/// の Drop (worker join) が RT を塞がない (plan §4)。
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

/// Engine resources shared with off-RT readers: the offline-export thread
/// and the notify thread. `plugin_refs` / `worker` は recv loop が書く
/// **ミラー** — RT は `RtBundle` に載った Arc clone を使う (RT で ArcSwap
/// guard の最終 drop が起きないようにするため)。
pub struct EngineShared {
    /// device_id → `PluginEntry` のミラー (export / notify 用)。RT へは
    /// 同じ `Arc<PluginEntry>` 群が bundle で渡るので quarantine フラグは
    /// 両者で共有される。
    pub plugin_refs: ArcSwap<PluginRefs>,
    /// worker rig のミラー (export / notify 用)。
    pub worker: ArcSwapOption<WorkerRig>,
    /// Set by the export thread while it owns the audio path. CPAL
    /// callback skips its `process_buffer` and writes silence so the
    /// export render can drive `plugin.process()` exclusively.
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
    /// プロジェクト同一性の SSoT)。値が変わった瞬間が「別プロジェクトに
    /// 切り替わった」であり、Song スコープの id を key にした engine 側の
    /// 状態 (`plugin_refs` / `recording_lanes` / RT の走行状態) をまとめて
    /// 捨てる唯一の検出点。`0` = 未ロード。
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
    /// ロックしてよい (RT 側は `LocalState::stretch_pool_rx` を lock-free に drain)。
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
    /// (= count-in 開始時に GUI が `StartCountIn { samples }` で立てた値の
    /// snapshot)。 `process_buffer` で `elapsed = total - remaining` を
    /// 計算して metronome の click trigger 用 playhead として使う。 0 で
    /// count-in 中ではない。
    pub preroll_total_samples: AtomicU64,
    /// Phase 7 B4 Step C: count-in 残り samples。 audio thread が毎 buffer
    /// `frames` だけ deduct + audio_bridge mirror 経由で GUI に publish。
    /// 0 到達で通常再生に戻る (= dispatch / clip render 復帰)。
    pub preroll_remaining_samples: AtomicU64,
    /// master volume (f32 bits)。 recv loop が `SetMasterGain` で store、
    /// render (`render_master_buffer`) が load して master へ掛ける。live /
    /// export 共通 (§5 — 旧実装は CPAL interleave 段のみで export に乗らず、
    /// master gain が WAV に反映されなかった)。
    pub master_gain: AtomicU32,
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

impl EngineShared {
    /// `EngineShared` と、RT (`LocalState`) が持つべき ring の片割れを作る。
    pub fn new_with_stretch_rings() -> (
        Self,
        rtrb::Consumer<StretchPoolDelivery>,
        rtrb::Producer<StretchPoolDelivery>,
    ) {
        let (tx, rx) = rtrb::RingBuffer::new(STRETCH_POOL_RING_CAP);
        let (recycle_tx, recycle_rx) = rtrb::RingBuffer::new(STRETCH_POOL_RING_CAP);
        let mut shared = Self::new();
        shared.stretch_pool_tx = std::sync::Mutex::new(tx);
        shared.stretch_pool_recycle_rx = std::sync::Mutex::new(recycle_rx);
        (shared, rx, recycle_tx)
    }

    pub fn new() -> Self {
        // 呼び出し側が `new_with_stretch_rings` を使わない (テスト等) 場合は、
        // 相手のいない ring を持つ = 配送は起きないが panic もしない。
        let (tx, _rx) = rtrb::RingBuffer::new(1);
        let (_recycle_tx, recycle_rx) = rtrb::RingBuffer::new(1);
        Self {
            stretch_pool_tx: std::sync::Mutex::new(tx),
            stretch_pool_recycle_rx: std::sync::Mutex::new(recycle_rx),
            delivered_engines_per_track: std::sync::Mutex::new(Vec::new()),
            plugin_refs: ArcSwap::from_pointee(HashMap::new()),
            worker: ArcSwapOption::empty(),
            export_running: AtomicBool::new(false),
            export_cancel: AtomicBool::new(false),
            live_parked: AtomicBool::new(false),
            audio_clip_renderer: ArcSwap::from_pointee(AudioClipRenderer::empty()),
            schedule_generation: AtomicU64::new(0),
            loaded_project_id: AtomicU64::new(0),
            last_published_generation: std::sync::Mutex::new(0),
            project_dir: ArcSwapOption::empty(),
            preroll_total_samples: AtomicU64::new(0),
            preroll_remaining_samples: AtomicU64::new(0),
            master_gain: AtomicU32::new(1.0_f32.to_bits()),
            last_buffer_frames: AtomicU32::new(0),
            mmcss_join_failed: AtomicBool::new(false),
            mmcss_warned: AtomicBool::new(false),
        }
    }
}

impl Default for EngineShared {
    fn default() -> Self {
        Self::new()
    }
}

/// Off-thread で構築され、RT audio thread へ wait-free に配送される
/// snapshot 一式 (plan §4 の RtBundle)。`compile_schedule` /
/// `TempoMap::from_song` / plugin_refs map の rebuild / worker pool の
/// spawn は全部 recv loop 側で走り、RT は swap (move / Arc clone) だけを
/// 行う。superseded bundle は recycle ring で recv loop に返送され、
/// `Drop` (free / shmem unmap / worker join) も off-thread で走る。
///
/// **不変条件 (field を足すときは必ずどちらか決めること)**: forward ring は
/// 両端が「最新だけ残す」 coalescing channel なので、
/// - **snapshot** field (`song` / `tempo_map` / `plugin_refs` / `worker`) は
///   最新値が過去を包含する ⇒ そのまま最新で上書きしてよい。
/// - **delta** field (`schedule` と、それと対の `input_delay_replacements`)
///   は「無い = 変更なし」を意味する ⇒ **中間 bundle を捨てるときに
///   [`RtBundle::supersede`] で畳み込まないと変更が永久に失われる**。
///   schedule が delta なのは本質的で、schedule は RT だけが持つ走行状態
///   (PDC ring / follower env) を内包するため off-thread では snapshot を
///   作れない。
pub struct RtBundle {
    /// 現 song snapshot (`None` = song 未ロード)。
    pub song: Option<Arc<Song>>,
    pub tempo_map: common::tempo_map::TempoMap,
    /// `None` = 値のみ更新 (SetTrackVolume 等) — RT は現行 schedule を
    /// 保持する (§5 D: 値更新で `compile_schedule` を走らせない)。
    /// `Some` = topology 変更 — install 時に `adopt_state_from` で
    /// DelayLine / FollowerSlot の走行状態を旧 schedule から移送する。
    pub schedule: Option<Schedule>,
    /// **delta**: `true` = 別プロジェクトが読み込まれた (`Song::project_id` が
    /// 変わった) ので、Song スコープの id で引き継いでいる **走行状態を捨てる**。
    ///
    /// `adopt_state_from` の移送キー (`DelayKey::MixSrc{track_id}` /
    /// `ModSource::id`) も `TrackScratch` の index も Song スコープの名前なので、
    /// project を跨ぐと別物同士が一致してしまう。引き継ぐと前 project の PDC
    /// リングに残った音声や follower の envelope が新 project の頭に混ざる。
    pub reset_song_scoped_state: bool,
    /// `input_delay_per_track` が `TrackScratch::input_delay_line` の
    /// prealloc (1s) を超える病的ケース用の off-thread pre-alloc 置換 line
    /// (index = track index)。install 時に必要なら swap され、旧 line が
    /// この Vec に残って recycle で off-thread drop される。通常は全 `None`
    /// (schedule が `None` のときは常に空)。
    pub input_delay_replacements: Vec<Option<DelayLine>>,
    /// device_id → entry (Arc clone — recv loop のミラーと同一 entry)。
    pub plugin_refs: Arc<PluginRefs>,
    /// worker rig (Arc clone)。`None` = pool 未 open / close 済。
    pub worker: Option<Arc<WorkerRig>>,
}

impl RtBundle {
    /// `self` (新) が `older` (旧) を supersede するときの **畳み込み**。
    /// 「`older` を install してから `self` を install する」のと、
    /// 「畳み込んだ `self` だけを install する」のを等価にする。
    ///
    /// snapshot field は `self` の値がそのまま勝つ (最新が過去を包含する)。
    /// delta field (`schedule` = `None` は「据え置き」の意) は、`self` が
    /// 持っていなければ `older` のものを引き継ぐ。`input_delay_replacements`
    /// は採用した schedule 用に off-thread で確保された line なので、必ず
    /// schedule と同じ bundle 由来のものを連れて行く。
    ///
    /// これが無いと、topology 更新 (LoadSong) と値のみ更新
    /// (`OpenPluginShmem` / `SetTrackMuted` 等) が同一バッファ周期に積まれた
    /// とき、coalescing で **compile 済み schedule が捨てられ**、RT は前の
    /// song の schedule を使い続ける (= 曲を開いて再生すると先頭 track しか
    /// 鳴らず、値のみ更新を 1 回起こすと直る、という症状になる)。
    ///
    /// 戻り値は空にした `older` (呼び出し側が recycle ring へ返して
    /// off-thread で drop する)。RT thread から呼ばれるので、操作は move と
    /// `Vec` のポインタ swap のみ — alloc / free / lock は無い。
    pub fn supersede(&mut self, mut older: RtBundle) -> RtBundle {
        if self.schedule.is_none() {
            self.schedule = older.schedule.take();
            // self 側 (値のみ更新) は常に空 Vec だが、drop を RT で走らせない
            // ため代入ではなく swap で older に載せて返す。
            std::mem::swap(
                &mut self.input_delay_replacements,
                &mut older.input_delay_replacements,
            );
        }
        // 「捨てろ」は一度でも要求されたら畳み込み後も残す (OR)。落とすと
        // project 切替の走行状態リセットが coalescing で消える。
        self.reset_song_scoped_state |= older.reset_song_scoped_state;
        older
    }
}

/// Audio-thread-private engine state. Lives in the CPAL closure for the
/// whole stream lifetime.
pub struct LocalState {
    /// Pre-allocated scratch buffers (MAX_TRACKS entries). The audio
    /// loop indexes into this with the current Song's track index — no
    /// resize, no allocation in the RT path.
    pub scratch: Vec<TrackScratch>,
    pub master_l: Vec<f32>,
    pub master_r: Vec<f32>,
    /// Whether the transport was rolling on the previous buffer. Used to
    /// detect Play/Stop transitions and reset the playhead / queue
    /// note-offs cleanly.
    pub playing: bool,
    /// Phase 5 Step 5.2 (`docs/plan_automation.md` §10): accumulated
    /// beat-domain playhead。 audio thread が buffer 頭で
    /// `evaluate_song_tempo(song, playhead_beats)` を呼んで current_bpm を
    /// 引き、 buffer 末で `playhead_beats += frames * current_bpm / (60 * SR)`
    /// で advance する。 Play edge / SeekTo IPC では tempo map で逆算する。
    pub playhead_beats: f64,
    /// Phase 5 Step 5.2: 前 buffer 末の sample-domain playhead。 次 buffer 頭
    /// で `shared.playhead != last_known_playhead` のとき seek が発生したと
    /// 判定し、 `playhead_beats` を再初期化する。 初期値 `u64::MAX` は
    /// 「未確定」 (= 最初の buffer は必ず seek 扱いで初期化される)。
    pub last_known_playhead: u64,
    /// metronome click voice 状態 (mono single-voice)。 `Some` なら active
    /// (= まだ decay 中)。 詳細は `crate::metronome`。
    pub metronome_voice: Option<ClickVoice>,
    /// Pending preview commands from the receive loop. Drained at the top
    /// of every `process_buffer`.
    pub cmd_rx: tokio::sync::mpsc::UnboundedReceiver<EngineCommand>,
    /// Resources shared with the export / notify threads.
    pub shared: Arc<EngineShared>,
    /// SPSC carrying freshly off-thread-built [`RtBundle`]s from the receive
    /// loop to the audio thread. The audio thread `pop`s the newest
    /// (wait-free, no alloc — the value moves out of the pre-allocated ring
    /// slot) and swaps it into the cached fields below.
    pub bundle_rx: rtrb::Consumer<RtBundle>,
    /// SPSC to ship superseded bundles back to the receive loop for disposal.
    /// The audio thread `push`es the old snapshot here (wait-free, no alloc)
    /// when it swaps in a newer one; the receive loop `pop`s and drops them,
    /// so `Drop` (free / unmap / worker join) runs off the audio thread.
    pub bundle_recycle_tx: rtrb::Producer<RtBundle>,
    /// r.md #40: off-thread が確保した stretch engine の受け口。 毎 buffer
    /// (renderer snapshot を load する前に) drain して `TrackScratch` に移す。
    pub stretch_pool_rx: rtrb::Consumer<StretchPoolDelivery>,
    /// 空にした配送便を off-thread へ返す口 (RT で `Vec` を drop しないため)。
    pub stretch_pool_recycle_tx: rtrb::Producer<StretchPoolDelivery>,
    /// Cached schedule (installed from the newest bundle; 値のみ更新では
    /// 据え置き)。DelayLine / FollowerSlot の走行状態を内包する。
    pub cached_schedule: Schedule,
    /// Last installed `Arc<Song>`.
    pub cached_song: Option<Arc<Song>>,
    /// (A10 r.md #8) cached_song の SongTempo curve を積分した beat↔sample map。
    /// seek / loop-wrap で playhead を sample→beat に戻すとき、 constant-bpm 線形推定
    /// でなくこの map で tempo automation を honor する。 lookup は O(log n)・
    /// alloc/lock 無で RT 安全。
    pub tempo_map: common::tempo_map::TempoMap,
    /// RT が使う plugin_refs snapshot (bundle 由来 Arc clone)。
    pub plugin_refs: Arc<PluginRefs>,
    /// RT が使う worker rig (bundle 由来 Arc clone)。
    pub worker: Option<Arc<WorkerRig>>,
    /// docs/plan_modulation.md §5: reusable per-buffer snapshot of follower
    /// scalars (slot = `ModSource` position), filled from
    /// `cached_schedule.follower_slots` before dispatch (= the previous
    /// buffer's envelopes) and published to the audio workers so volume / pan /
    /// plugin-param lanes with `mod_routings` modulate. Reused across buffers
    /// (no per-buffer allocation once warmed).
    pub mod_scalars_snapshot: Vec<f32>,
    /// Debug-only: playhead at the last heartbeat log. Throttles
    /// `engine heartbeat` to once per second of audio time.
    #[cfg(debug_assertions)]
    pub last_heartbeat_playhead: u64,
    /// Debug-only: pre-allocated scratch for the heartbeat log so the RT
    /// path doesn't allocate when the throttle window opens. Cleared and
    /// re-extended on each emit; capacity is sized at construction.
    #[cfg(debug_assertions)]
    pub heartbeat_track_peaks: Vec<(f32, f32, bool)>,
    #[cfg(debug_assertions)]
    pub heartbeat_device_ids: Vec<u64>,
}

impl LocalState {
    pub fn new(
        max_frames: usize,
        cmd_rx: tokio::sync::mpsc::UnboundedReceiver<EngineCommand>,
        shared: Arc<EngineShared>,
        bundle_rx: rtrb::Consumer<RtBundle>,
        bundle_recycle_tx: rtrb::Producer<RtBundle>,
        stretch_pool_rx: rtrb::Consumer<StretchPoolDelivery>,
        stretch_pool_recycle_tx: rtrb::Producer<StretchPoolDelivery>,
    ) -> Self {
        let scratch = (0..MAX_TRACKS).map(|_| TrackScratch::new()).collect();
        Self {
            stretch_pool_rx,
            stretch_pool_recycle_tx,
            scratch,
            master_l: vec![0.0; max_frames],
            master_r: vec![0.0; max_frames],
            playing: false,
            playhead_beats: 0.0,
            last_known_playhead: u64::MAX,
            metronome_voice: None,
            cmd_rx,
            shared,
            bundle_rx,
            bundle_recycle_tx,
            cached_schedule: Schedule::empty(),
            cached_song: None,
            // 初期は default song (= constant 120bpm)。 seek/loop-wrap は線形に縮退。
            tempo_map: common::tempo_map::TempoMap::from_song(&Song::default()),
            plugin_refs: Arc::new(HashMap::new()),
            worker: None,
            mod_scalars_snapshot: Vec::with_capacity(common::audio_bridge::MAX_MOD_SOURCES),
            #[cfg(debug_assertions)]
            last_heartbeat_playhead: 0,
            #[cfg(debug_assertions)]
            heartbeat_track_peaks: Vec::with_capacity(MAX_TRACKS),
            // 上限は実態に合わせた hint。超えても Vec が伸びるだけだが、
            // steady-state で MAX_TRACKS * 4 device を超えるケースは稀。
            #[cfg(debug_assertions)]
            heartbeat_device_ids: Vec::with_capacity(MAX_TRACKS * 4),
        }
    }

    /// Install the newest off-thread-built bundle, if one was published since
    /// the last buffer. すべて move / `Arc` clone / `mem::swap` — RT 上で
    /// alloc も free も起きない。schedule が載っている (= topology 変更)
    /// ときは `adopt_state_from` で DelayLine / FollowerSlot の走行状態を
    /// 旧 schedule から移送する (§5 D — off-thread では live 状態を持てない
    /// ため、install 時にポインタ swap で行う)。superseded 一式は recycle
    /// ring で off-thread drop。
    fn refresh_bundle(&mut self) {
        // Drain the forward ring down to a single bundle. Skipping straight to
        // the newest is only sound for the snapshot fields; `schedule` is a
        // delta (`None` = 据え置き) なので、飛ばす bundle は捨てる前に
        // `supersede` で畳み込む (= 全 bundle を順に install したのと等価)。
        // 畳み込み後の残骸は recycle off-thread (`Drop` を callback で
        // 走らせない)。
        let mut newest: Option<RtBundle> = None;
        while let Ok(mut bundle) = self.bundle_rx.pop() {
            if let Some(skipped) = newest.take() {
                let _ = self.bundle_recycle_tx.push(bundle.supersede(skipped));
            }
            newest = Some(bundle);
        }
        let Some(mut new) = newest else {
            return;
        };

        // ---- swap in the new snapshot, collecting the old for recycling ----
        let old_song = std::mem::replace(&mut self.cached_song, new.song.take());
        let old_tempo = std::mem::replace(&mut self.tempo_map, new.tempo_map);
        let old_refs = std::mem::replace(&mut self.plugin_refs, Arc::clone(&new.plugin_refs));
        let old_worker = std::mem::replace(&mut self.worker, new.worker.take());

        let mut old_schedule: Option<Schedule> = None;
        let mut retired_lines: Vec<Option<DelayLine>> = Vec::new();
        if let Some(mut sched) = new.schedule.take() {
            if new.reset_song_scoped_state {
                // 別 project。移送キー (track_id / ModSource::id) は Song
                // スコープの名前なので、引き継ぐと **別物同士が一致**して前
                // project の PDC リング音声 / follower envelope が新 project の
                // 頭に混ざる。新 schedule は compile 直後でゼロ初期化済みなので、
                // 移送を **やらない** ことがそのままリセットになる。
                //
                // schedule 外で生き続ける per-track の input delay line は
                // 明示的にゼロ化する。全 track を舐めると 32 × 384 KB の memset に
                // なるので、実際に補償が効く (= 遅延サンプルを読み出す) track
                // だけに絞る。alloc / free は無い。
                for (i, &d) in sched.input_delay_per_track.iter().enumerate() {
                    if d > 0 && let Some(s) = self.scratch.get_mut(i) {
                        s.input_delay_line.reset();
                    }
                }
                // r.md #40: stretch engine の走行ストリームも Song スコープ。
                // `stream_key = clip.id << 32 | audio event id` は project ごとに
                // 1 から再採番される名前なので、別 project の event が同じキーで
                // **引き当てに成功してしまう** (= 前 project のスペクトル状態を
                // 引き継いだ音が頭に混ざる)。 pool の実体は使い回すが、走行状態は
                // 捨てて必ず prime し直させる。 alloc / free 無し。
                for s in &mut self.scratch {
                    for engine in &mut s.stretch_engines {
                        engine.forget_stream();
                    }
                    // tape 位置 accumulator も同じ理由で無効化する
                    // (添字は track 内 schedule 順 = 位置キー)。
                    for slot in &mut s.repitch_accum {
                        *slot = (u64::MAX, 0.0);
                    }
                }
            } else {
                // §5 D: 走行状態 (PDC ring / follower env) を stable key で移送。
                sched.adopt_state_from(&mut self.cached_schedule);
            }
            old_schedule = Some(std::mem::replace(&mut self.cached_schedule, sched));

            // per-track input delay line: prealloc (1s) を超える補償が要る
            // track には off-thread pre-alloc された置換 line が載っている。
            // swap して旧 line を bundle 側に残す (off-thread drop)。
            for (i, repl) in new.input_delay_replacements.iter_mut().enumerate() {
                if i >= self.scratch.len() {
                    break;
                }
                if let Some(line) = repl.as_mut()
                    && self.scratch[i].input_delay_line.capacity() < line.capacity()
                {
                    std::mem::swap(&mut self.scratch[i].input_delay_line, line);
                }
            }
            retired_lines = std::mem::take(&mut new.input_delay_replacements);
        }

        // Recycle the superseded snapshot off the audio thread. The very first
        // install has no prior song and only tiny defaults (empty schedule /
        // default tempo map / empty map Arc), so this is a one-time trivial
        // drop at stream startup, not a steady-state RT free. If the recycle
        // ring is somehow full (a burst the receive loop hasn't drained — not
        // reachable with human-paced edits given the ring size), drop here as
        // a last resort rather than leak.
        let recycled = RtBundle {
            song: old_song,
            tempo_map: old_tempo,
            schedule: old_schedule,
            reset_song_scoped_state: false,
            input_delay_replacements: retired_lines,
            plugin_refs: old_refs,
            worker: old_worker,
        };
        let _ = self.bundle_recycle_tx.push(recycled);
    }

    /// r.md #40: off-thread が確保した stretch engine を `TrackScratch` へ取り込む。
    /// **`audio_clip_renderer` の snapshot を load する前**に呼ぶこと — publish 側が
    /// 「pool を push してから schedule を store」 の順で出すので、この順序を守れば
    /// 新 schedule の `engine_slot` に対応するエンジンが必ず揃っている。
    ///
    /// RT-safe: `pop` / `push` / move のみ。 `stretch_engines` は容量予約済なので
    /// `push` は再確保しない (= 走行中のエンジンを動かさずに増やせる)。 空になった
    /// 配送便は recycle ring へ返して off-thread で drop する。
    fn refresh_stretch_pools(&mut self) {
        while let Ok(mut delivery) = self.stretch_pool_rx.pop() {
            if let Some(scratch) = self.scratch.get_mut(delivery.track_idx) {
                while scratch.stretch_engines.len() < scratch.stretch_engines.capacity() {
                    let Some(engine) = delivery.engines.pop() else {
                        break;
                    };
                    scratch.stretch_engines.push(engine);
                }
            }
            // 取り込めなかったエンジン (容量超過) も配送便に残したまま返す。
            let _ = self.stretch_pool_recycle_tx.push(delivery);
        }
    }

    /// Drain pending preview commands. Called at the top of `process_buffer`.
    fn pump_commands(&mut self) {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            match cmd {
                EngineCommand::PreviewNoteOn {
                    track,
                    pitch,
                    velocity,
                } => {
                    // 鍵盤プレビュー: 該当 track の pending_preview に積む。
                    // process_track_owned が次の dispatch で frame 0 に注入する。
                    // capacity 上限で guard し RT での realloc を避ける
                    // (push_note_on と同じ「溢れたら drop」 方針)。
                    if let Some(s) = self.scratch.get_mut(track) {
                        let pp = &mut s.state.pending_preview;
                        if pp.len() < pp.capacity() {
                            pp.push(NoteTransition::On {
                                note_id: PREVIEW_NOTE_ID,
                                key: pitch,
                                velocity,
                            });
                        }
                    }
                }
                EngineCommand::PreviewNoteOff { track, pitch } => {
                    if let Some(s) = self.scratch.get_mut(track) {
                        let pp = &mut s.state.pending_preview;
                        if pp.len() < pp.capacity() {
                            pp.push(NoteTransition::Off {
                                note_id: PREVIEW_NOTE_ID,
                                key: pitch,
                            });
                        }
                    }
                }
            }
        }
    }

    /// Render `frames` of master output into `master_l/r`. Transport 状態を
    /// 進め、live/export 共通の `render_master_buffer` で描画し、metronome
    /// (monitoring 専用) を重ね、meters / mod scalars を publish する。
    pub fn process_buffer(
        &mut self,
        shared: &SharedState,
        bridge: &AudioBridgeHandle,
        scope: &common::scope_bridge::ScopeBridgeHandle,
        sample_rate: u32,
        frames: usize,
    ) {
        self.pump_commands();

        // Install the newest off-thread snapshot (song / schedule /
        // plugin_refs / worker rig) before the dispatch starts.
        self.refresh_bundle();
        // r.md #40: audio clip renderer snapshot を load する前に stretch engine
        // pool を取り込む (publish 側は pool → schedule の順で出す)。
        self.refresh_stretch_pools();
        let song_snapshot = self.cached_song.clone();

        // recv loop が schedule compile の buffer_frames (leaf sidechain の
        // 1-buffer 補償量) に使う実測値。変化時のみ store (steady state では
        // load 1 回で済む)。
        if self.shared.last_buffer_frames.load(Ordering::Relaxed) != frames as u32 {
            self.shared
                .last_buffer_frames
                .store(frames as u32, Ordering::Release);
        }

        let n = frames;
        self.master_l[..n].fill(0.0);
        self.master_r[..n].fill(0.0);

        // Snapshot the transport-state atomics once for the whole buffer so
        // every step below sees a single consistent view (export gate /
        // loop wrap / metronome gate). Loading each atomic at multiple call
        // sites could otherwise observe a mid-buffer flip and produce an
        // internally inconsistent buffer.
        let export_running = self.shared.export_running.load(Ordering::Acquire);
        // ループ状態は 3 値まとめて 1 回だけ copy-out する (buffer 途中で
        // ON/OFF と範囲が食い違って見えない)。 `LoopRegion` は `Copy` なので
        // guard は即座に落とせる = RT 上で Arc を持ち回らない。
        let loop_region = **shared.loop_region.load();
        let looping = loop_region.enabled;
        let metronome_enabled = shared.metronome_enabled.load(Ordering::Acquire);

        // freewheel export: while the export thread holds the audio
        // resources, write silence and skip dispatch so the worker pool
        // and plugin instances are exclusively driven by the export
        // render loop. Publish `live_parked` so the export thread knows the
        // live callback has stopped dispatching before it starts its own (no
        // collision on the shared plugin-host worker slots).
        if export_running {
            self.shared.live_parked.store(true, Ordering::Release);
            return;
        }
        self.shared.live_parked.store(false, Ordering::Release);

        // Phase 7 B4 Step C: count-in モード — preroll > 0 なら通常 dispatch /
        // clip render を skip し、 metronome のみ render + preroll counter を
        // deduct + audio_bridge に mirror。 0 到達で通常再生に戻る。
        let preroll =
            self.shared.preroll_remaining_samples.load(Ordering::Acquire);
        if preroll > 0 {
            let total =
                self.shared.preroll_total_samples.load(Ordering::Acquire);
            let elapsed = total.saturating_sub(preroll);
            let bpm = song_snapshot
                .as_ref()
                .map(|s| s.bpm)
                .unwrap_or(120.0)
                .max(1.0);
            let tsig_num = i64::from(
                song_snapshot
                    .as_ref()
                    .map(|s| s.time_sig.0)
                    .unwrap_or(4)
                    .max(1),
            );
            if metronome_enabled {
                render_metronome(
                    &mut self.metronome_voice,
                    &mut self.master_l[..n],
                    &mut self.master_r[..n],
                    n,
                    // r.md #39: count-in の click も本再生と **同じ時間軸** に載せる。
                    // 補償しないと count-in 最終拍と曲 1 拍目の間隔だけが
                    // 「1 拍 + master_latency」に伸び、録音のダウンビートでつんのめる。
                    // 揃える相手は count-in 中の音ではなく直後に続く曲の click / 音。
                    elapsed as i64
                        - i64::from(self.cached_schedule.master_latency_samples),
                    sample_rate,
                    // count-in は曲の tempo map ではなく定テンポ (preroll 長も
                    // `bars * time_sig` 拍で決まっている)。
                    &crate::metronome::ClickGrid::Fixed { bpm },
                    tsig_num,
                );
            }
            let new_preroll = preroll.saturating_sub(n as u64);
            self.shared
                .preroll_remaining_samples
                .store(new_preroll, Ordering::Release);
            bridge.set_preroll_remaining(new_preroll);
            return;
        }

        // GUI からの seek 要求を audio thread 単独 writer として
        // `playhead` に反映する。IPC スレッドが `playhead` を直接書くと、下の
        // buffer 末 advance store と同一 atomic を別スレッドから書く race になり、
        // Stop 直後 (in-flight buffer がまだ playing で advance する瞬間) に開始
        // 位置への巻き戻しが上書きされて停止位置から再生されてしまう。`swap` で
        // 消費する (多重要求は last-wins)。
        let pending_seek = shared.pending_seek.swap(NO_PENDING_SEEK, Ordering::AcqRel);
        if pending_seek != NO_PENDING_SEEK {
            shared.playhead.store(pending_seek, Ordering::Release);
        }

        // Play / Stop edge handling. On Play, restart playhead and clear
        // active notes. On Stop, queue offs at frame 0 of the next buffer
        // so plugins drain cleanly.
        let desired = PlaybackCommand::from_u8(shared.playback.load(Ordering::Acquire));
        match (self.playing, desired) {
            (false, PlaybackCommand::Play) => {
                self.playing = true;
                // Play は **現在の playhead からそのまま再生する** (頭出しは
                // しない)。「どこから再生するか」「停止でどこへ戻すか」は GUI 側
                // が所有する (モデル A = Pro Tools / Ableton 流)。
                for s in self.scratch.iter_mut() {
                    s.state.active_notes.clear();
                    s.state.pending_offs.clear();
                }
            }
            (true, PlaybackCommand::Stop) => {
                self.playing = false;
                for s in self.scratch.iter_mut() {
                    for &k in &s.state.active_notes {
                        s.state.pending_offs.push(k);
                    }
                    s.state.active_notes.clear();
                }
            }
            _ => {}
        }
        let playing = self.playing;

        let song_ref = song_snapshot.as_deref();
        let playhead = shared.playhead.load(Ordering::Acquire);

        // Phase 4 Step C-2: 「現在 recording 中の lane」 を SharedState から
        // 1 buffer 分の lifetime で借りる。
        let recording_lanes_g = shared.recording_lanes.load();
        let recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)> =
            &recording_lanes_g;

        // Phase 5 Step 5.2: seek 検出 + playhead_beats 同期。 前 buffer 末で
        // 記録した `last_known_playhead` と current playhead を比較し、 一致
        // していなければ (= IPC SeekTo / Play edge / loop wrap / 起動直後)
        // tempo map で正確に beat を逆算する (A10 r.md #8)。
        if playhead != self.last_known_playhead {
            self.playhead_beats = self.tempo_map.samples_to_beat(playhead, sample_rate);
        }
        // 今 buffer の effective bpm を SongTempo lane から評価する。
        // song = None なら 120.0 default、 SongTempo lane 無しなら song.bpm。
        // 当該 buffer 内では tempo 定数として扱う (= sub-buffer の tempo
        // change は scope 外、 1 buffer = ~5..20ms なので user 体感には
        // 影響なし)。SongTempo lane が recording 中なら curve eval を skip し
        // `song.bpm` constant fallback を維持する (Volume / Pan と同 idiom)。
        let tempo_recording = recording_lanes.contains(&(
            common::model::MASTER_TRACK_ID,
            common::model::AutomationTarget::SongTempo,
        ));
        let current_bpm: f32 = match song_ref {
            Some(s) if tempo_recording => s.bpm,
            Some(s) => {
                let base = common::automation::evaluate_song_tempo(s, self.playhead_beats);
                // B11 (r.md #8): song-level modulation (LFO/Random/MSEG/follower →
                // `SongTempo`) を base tempo に適用。 `mod_scalars_snapshot` は前
                // buffer 値 (followers は元々 1-buffer lag)。 `SongTempo` を target に
                // する song_mod_routing が無ければ offset 0 = no-op。 RT-safe。
                common::automation::apply_modulation_with_scalars(
                    s,
                    &common::model::AutomationTarget::SongTempo,
                    f64::from(base),
                    &s.song_mod_routings,
                    &self.mod_scalars_snapshot,
                ) as f32
            }
            None => 120.0,
        };

        if let Some(song) = song_ref {
            let n_tracks = song.tracks.len().min(MAX_TRACKS);

            // PR6: audio clip renderer snapshot for this buffer. Guard stays
            // live until the end of the call so workers can safely deref it.
            let audio_renderer_g = self.shared.audio_clip_renderer.load();
            let audio_renderer: &AudioClipRenderer = &audio_renderer_g;

            // docs/plan_modulation.md §5: snapshot the previous buffer's
            // follower envelopes (slot order = `ModSource` position) for audio-
            // param modulation, reusing the buffer (no per-buffer alloc). The
            // EnvelopeFollow nodes for THIS buffer run post-dispatch, so param
            // events see the prior buffer's env — a ~1-buffer (block-rate) lag.
            // generator (LFO/Random/MSEG/Steps) は `song_beat`/`song_secs` から
            // この buffer の値を直接算出する (状態レス・lag なし、 決定論)。
            self.mod_scalars_snapshot.clear();
            let song_secs = playhead as f64 / sample_rate as f64;
            for (fs, kind) in self
                .cached_schedule
                .follower_slots
                .iter()
                .zip(self.cached_schedule.mod_kinds.iter())
            {
                let v = common::modulators::generator_scalar(kind, self.playhead_beats, song_secs)
                    .unwrap_or(fs.env);
                self.mod_scalars_snapshot.push(v);
            }

            // live/export 共通の単一 render 経路 (§5): dispatch → schedule →
            // master fx → master gain。
            let master_gain =
                f32::from_bits(self.shared.master_gain.load(Ordering::Relaxed));
            render_master_buffer(
                song,
                &mut self.cached_schedule,
                &mut self.scratch[..MAX_TRACKS],
                &self.plugin_refs,
                self.worker.as_deref(),
                audio_renderer,
                &mut self.master_l[..n],
                &mut self.master_r[..n],
                sample_rate,
                n as u32,
                playing,
                loop_region,
                recording_lanes,
                current_bpm,
                self.playhead_beats,
                &self.mod_scalars_snapshot,
                master_gain,
            );

            // r.md #50: マスター出力サンプルを GUI のメーター解析リングへ流す。
            // **metronome click を重ねる前** に取るのがこのタップ位置の要点で、
            // これで「メーターの数値 = 書き出す WAV の数値」が構造的に一致する
            // (grill-me で確定した測定対象 = 曲の音だけ)。事前確保済み shmem への
            // store のみなので RT 安全 (確保・ロック・I/O 無し)。
            scope.write_block(&self.master_l[..n], &self.master_r[..n]);

            // metronome click を master mix に重ねる (monitoring 専用 — export
            // 経路には存在しない)。 master mix の最後に重ねる (= track の mute /
            // solo / volume / master fx の影響を受けない「常に聞こえる guide」)。
            //
            // r.md #39: click の参照位置から **master の PDC 遅延** を引く。 track の音は
            // 遅延プラグイン (linear-phase EQ 等) の分だけ遅れて master に届くのに、 click
            // だけ生の playhead で重ねると click が先行して拍の基準に使えなくなる
            // (REAPER / Ardour もメトロノームを遅延補償の対象にする)。 補償後の位置は曲頭
            // 付近で負になるので符号付きで渡す (0 クランプすると 1 拍目を毎 buffer 再 trigger
            // してしまう)。
            if playing && metronome_enabled {
                let tsig_num = i64::from(song.time_sig.0.max(1));
                let click_pos = playhead as i64
                    - i64::from(self.cached_schedule.master_latency_samples);
                render_metronome(
                    &mut self.metronome_voice,
                    &mut self.master_l[..n],
                    &mut self.master_r[..n],
                    n,
                    click_pos,
                    sample_rate,
                    // r.md #39: 拍境界は tempo map (SongTempo automation 積分済み) で
                    // 求める。瞬間 bpm × sample の等間隔グリッドだと、テンポ変更以降の
                    // click が clip / note (playhead_beats 基準) と別グリッドに載る。
                    &crate::metronome::ClickGrid::Song(&self.tempo_map),
                    tsig_num,
                );
            }

            // Publish per-track peak meters into the shared AudioBridge
            // so the GUI mixer strips animate. Atomic stores, RT-safe.
            // Tracks with effective_mute already have peak_l/r == 0.
            for (i, tr) in self.scratch.iter().take(n_tracks).enumerate() {
                bridge.set_track_peak(i, tr.peak_l, tr.peak_r);
            }

            // docs/plan_modulation.md §4.2: publish each ModSource's envelope
            // follower scalar (block-rate, `env` after this buffer) so the GUI
            // poller can apply visual/param modulation. Atomic stores, RT-safe.
            // follower は env、 generator は song 位置から直接算出して publish。
            let pub_song_secs = playhead as f64 / sample_rate as f64;
            for (slot, (fs, kind)) in self
                .cached_schedule
                .follower_slots
                .iter()
                .zip(self.cached_schedule.mod_kinds.iter())
                .enumerate()
            {
                let v = common::modulators::generator_scalar(kind, self.playhead_beats, pub_song_secs)
                    .unwrap_or(fs.env);
                bridge.set_mod_scalar(slot, v);
            }

            // Debug-only heartbeat. RT 規約上 audio thread での tracing は
            // 望ましくないが、開発時に engine 状態を可視化できる利点が
            // 大きいので debug ビルド限定で残す。release では消える。
            // pre-allocated buffer (`heartbeat_*`) を `clear()+extend()` で
            // 再利用するので heap alloc は (capacity 内なら) 発生しない。
            #[cfg(debug_assertions)]
            {
                let sr = sample_rate as u64;
                if sr > 0
                    && playhead / sr != self.last_heartbeat_playhead / sr
                {
                    self.last_heartbeat_playhead = playhead;
                    let master_peak = self.master_l[..n]
                        .iter()
                        .chain(self.master_r[..n].iter())
                        .fold(0.0_f32, |a, &b| a.max(b.abs()));
                    self.heartbeat_track_peaks.clear();
                    self.heartbeat_track_peaks.extend(
                        self.scratch
                            .iter()
                            .take(n_tracks)
                            .map(|s| (s.peak_l, s.peak_r, s.effective_mute)),
                    );
                    self.heartbeat_device_ids.clear();
                    self.heartbeat_device_ids
                        .extend(self.plugin_refs.keys().copied());
                    // r.md #16: 再生中 1 行/秒でログを埋める。 debug へ降格し
                    // (RUST_LOG=debug で復活)、 既定 (info) の dev ログには出さない。
                    tracing::debug!(
                        playing,
                        playhead,
                        master_peak,
                        track_peaks = ?self.heartbeat_track_peaks,
                        device_ids = ?self.heartbeat_device_ids,
                        n_sync_slots = self
                            .worker
                            .as_ref()
                            .map(|rig| rig.slots.len())
                            .unwrap_or(0),
                        worker_pool = self
                            .worker
                            .as_ref()
                            .is_some_and(|rig| rig.pool.is_some()),
                        audio_clip_n_events = audio_renderer.schedule.len(),
                        audio_clip_n_sources = audio_renderer.sources.len(),
                        "engine heartbeat"
                    );
                }
            }
        }

        // Playhead advance + auto-stop / loop wrap.
        if playing {
            let mut new_ph = playhead + n as u64;
            let active_end = if looping {
                effective_loop_bounds(song_ref, loop_region, sample_rate).map(|(_, e)| e)
            } else {
                None
            };
            let reached_end = if let Some(end) = active_end {
                new_ph >= end
            } else {
                song_ended(song_ref, sample_rate, new_ph)
            };
            // Phase 5 Step 5.2: playhead_beats を current_bpm で 1 buffer 分
            // advance する。 sub-buffer の tempo 変化は scope 外 (= 1 buffer
            // ~5..20ms 内 constant)。
            let sr = sample_rate as f64;
            if sr > 0.0 {
                self.playhead_beats +=
                    n as f64 * f64::from(current_bpm) / (60.0 * sr);
            }
            if reached_end {
                for s in self.scratch.iter_mut() {
                    for &k in &s.state.active_notes {
                        s.state.pending_offs.push(k);
                    }
                    s.state.active_notes.clear();
                }
                let wrap_to = if looping {
                    effective_loop_bounds(song_ref, loop_region, sample_rate).map(|(s, _)| s)
                } else {
                    None
                };
                if let Some(start) = wrap_to {
                    new_ph = start;
                    // A10 (r.md #8): loop start の beat も tempo map で正確に
                    // 逆算 (constant-bpm 線形推定は tempo automation 中の loop
                    // boundary でズレた)。
                    self.playhead_beats = self.tempo_map.samples_to_beat(new_ph, sample_rate);
                } else {
                    self.playing = false;
                    shared
                        .playback
                        .store(PlaybackCommand::Stop as u8, Ordering::Release);
                }
            }
            shared.playhead.store(new_ph, Ordering::Release);
            self.last_known_playhead = new_ph;
        } else {
            // Stop 中は audio thread が playhead を advance しない。GUI からの
            // SeekTo は process_buffer 冒頭の pending_seek consume で (audio
            // thread 自身が) shared.playhead に反映済みなので、その値で
            // last_known_playhead を同期し、次 Play 開始時の seek 検出を
            // 誤発火させない。
            self.last_known_playhead = playhead;
        }
    }
}

/// D1 / plan §4: off-thread bundle publish → wait-free RT install →
/// off-thread recycle. Verifies the audio thread picks up the newest bundle,
/// hands the superseded one back for disposal, coalesces bursts, adopts
/// schedule state, and (under `rt-assert`) performs the install with zero
/// allocation/free on the audio thread.
#[cfg(test)]
mod bundle_install_tests {
    use super::*;
    use crate::graph::compile_schedule;
    use common::model::{Song, Track};

    fn track(id: u32) -> Track {
        // Track の legacy migration fields は common に pub(crate) で閉じて
        // いるため、 default + mutate で構築する (E0451 回避)。
        let mut t = Track::default();
        t.id = id;
        t
    }

    fn make_bundle(song: &Arc<Song>) -> RtBundle {
        make_bundle_with_reset(song, false)
    }

    /// `reset_song_scoped_state` を明示する版 (project 切替相当)。
    fn make_bundle_with_reset(song: &Arc<Song>, reset: bool) -> RtBundle {
        RtBundle {
            song: Some(Arc::clone(song)),
            tempo_map: common::tempo_map::TempoMap::from_song(song),
            schedule: Some(compile_schedule(song, 48_000, 0).unwrap()),
            reset_song_scoped_state: reset,
            input_delay_replacements: Vec::new(),
            plugin_refs: Arc::new(HashMap::new()),
            worker: None,
        }
    }

    /// A `LocalState` plus the off-thread ends of the forward + recycle rings,
    /// so a test can publish bundles and inspect what got recycled.
    fn harness() -> (
        LocalState,
        rtrb::Producer<RtBundle>,
        rtrb::Consumer<RtBundle>,
    ) {
        let shared = Arc::new(EngineShared::new());
        let (_cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (bundle_tx, bundle_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, recycle_rx) = rtrb::RingBuffer::new(8);
        let (pool_tx, pool_rx) = rtrb::RingBuffer::<StretchPoolDelivery>::new(4);
        let (pool_recycle_tx, pool_recycle_rx) = rtrb::RingBuffer::<StretchPoolDelivery>::new(4);
        drop(pool_tx);
        drop(pool_recycle_rx);
        let local = LocalState::new(
            common::process_data::MAX_FRAMES,
            cmd_rx,
            shared,
            bundle_rx,
            recycle_tx,
            pool_rx,
            pool_recycle_tx,
        );
        (local, bundle_tx, recycle_rx)
    }

    #[test]
    fn refresh_installs_published_bundle_and_recycles_the_old() {
        let (mut local, mut bundle_tx, mut recycle_rx) = harness();

        let mut s1 = Song::default();
        s1.tracks.push(track(1));
        s1.tracks.push(track(2));
        let s1 = Arc::new(s1);
        bundle_tx.push(make_bundle(&s1)).unwrap();

        local.refresh_bundle();
        assert!(
            local.cached_song.as_ref().is_some_and(|s| Arc::ptr_eq(s, &s1)),
            "first publish installs its song"
        );
        // First install recycles only trivial defaults (no prior song).
        let first = recycle_rx.pop().expect("first install ships back the defaults");
        assert!(first.song.is_none());

        let mut s2 = Song::default();
        s2.tracks.push(track(7));
        let s2 = Arc::new(s2);
        bundle_tx.push(make_bundle(&s2)).unwrap();

        local.refresh_bundle();
        assert!(
            local.cached_song.as_ref().is_some_and(|s| Arc::ptr_eq(s, &s2)),
            "second publish installs its song"
        );
        // The superseded snapshot (s1) is handed back for off-thread disposal.
        let recycled = recycle_rx.pop().expect("old bundle recycled off-thread");
        assert!(recycled.song.as_ref().is_some_and(|s| Arc::ptr_eq(s, &s1)));
    }

    #[test]
    fn refresh_coalesces_a_burst_to_newest() {
        let (mut local, mut bundle_tx, mut recycle_rx) = harness();
        let songs: Vec<Arc<Song>> = (0..3)
            .map(|i| {
                let mut s = Song::default();
                s.tracks.push(track(i + 1));
                Arc::new(s)
            })
            .collect();
        for s in &songs {
            bundle_tx.push(make_bundle(s)).unwrap();
        }
        // One refresh drains all three: installs the last, recycles the two it
        // skipped past + the initial defaults.
        local.refresh_bundle();
        assert!(
            local
                .cached_song
                .as_ref()
                .is_some_and(|s| Arc::ptr_eq(s, &songs[2]))
        );
        let mut recycled = 0;
        while recycle_rx.pop().is_ok() {
            recycled += 1;
        }
        assert_eq!(
            recycled, 3,
            "two skipped bundles + the initial defaults are recycled off-thread"
        );
    }

    /// §5 D: 値のみ更新 (schedule = None) は song を差し替えつつ現行 schedule
    /// (走行状態込み) を据え置く。
    #[test]
    fn value_only_bundle_keeps_current_schedule() {
        let (mut local, mut bundle_tx, _recycle_rx) = harness();

        let mut s1 = Song::default();
        s1.tracks.push(track(1));
        s1.tracks.push(track(2));
        let s1 = Arc::new(s1);
        bundle_tx.push(make_bundle(&s1)).unwrap();
        local.refresh_bundle();
        let node_count = local.cached_schedule.nodes.len();
        assert!(node_count > 0);

        // 値のみ更新: schedule を載せない。
        let mut s2 = (*s1).clone();
        s2.tracks[0].volume = 0.5;
        let s2 = Arc::new(s2);
        bundle_tx
            .push(RtBundle {
                song: Some(Arc::clone(&s2)),
                tempo_map: common::tempo_map::TempoMap::from_song(&s2),
                schedule: None,
                reset_song_scoped_state: false,
                input_delay_replacements: Vec::new(),
                plugin_refs: Arc::new(HashMap::new()),
                worker: None,
            })
            .unwrap();
        local.refresh_bundle();
        assert!(
            local.cached_song.as_ref().is_some_and(|s| Arc::ptr_eq(s, &s2)),
            "song must follow the value-only bundle"
        );
        assert_eq!(
            local.cached_schedule.nodes.len(),
            node_count,
            "schedule must be kept (not recompiled / not emptied)"
        );
    }

    /// coalescing の畳み込み規約: topology 更新 (LoadSong) の直後に値のみ
    /// 更新 (`OpenPluginShmem` / `SetTrackMuted` 等) が同一バッファ周期で
    /// 積まれても、compile 済み schedule を落とさない。
    ///
    /// 回帰元: 曲を開くと LoadSong の 1〜2ms 後に `OpenPluginShmem` が届き、
    /// RT が「最新だけ残す」coalescing で LoadSong の schedule を捨てて
    /// **起動時 default song (1 track) の schedule を使い続け**、先頭 track
    /// しか鳴らなくなっていた (値のみ更新を 1 回起こすと LoadSong が単独で
    /// 届いて直る、という紛らわしい症状)。
    #[test]
    fn coalescing_keeps_the_schedule_of_a_superseded_topology_bundle() {
        let (mut local, mut bundle_tx, mut recycle_rx) = harness();

        // 起動時 default 相当: 1 track の song で schedule を install。
        let mut boot = Song::default();
        boot.tracks.push(track(1));
        let boot = Arc::new(boot);
        bundle_tx.push(make_bundle(&boot)).unwrap();
        local.refresh_bundle();
        let boot_nodes = local.cached_schedule.nodes.len();

        // 曲を開く: LoadSong (schedule 有り) → 直後に OpenPluginShmem 相当の
        // 値のみ更新 (schedule = None)。RT は両方を 1 回の refresh で拾う。
        let mut opened = Song::default();
        for i in 1..=4 {
            opened.tracks.push(track(i));
        }
        let opened = Arc::new(opened);
        bundle_tx.push(make_bundle(&opened)).unwrap();
        bundle_tx
            .push(RtBundle {
                song: Some(Arc::clone(&opened)),
                tempo_map: common::tempo_map::TempoMap::from_song(&opened),
                schedule: None,
                reset_song_scoped_state: false,
                input_delay_replacements: Vec::new(),
                plugin_refs: Arc::new(HashMap::new()),
                worker: None,
            })
            .unwrap();
        local.refresh_bundle();

        assert!(
            local
                .cached_song
                .as_ref()
                .is_some_and(|s| Arc::ptr_eq(s, &opened)),
            "song は最新 (値のみ更新) が勝つ"
        );
        assert!(
            local.cached_schedule.nodes.len() > boot_nodes,
            "飛ばした topology bundle の schedule が畳み込まれ、開いた曲の \
             4 track 分の node が入っていること (捨てられると起動時 1 track \
             の schedule のままになる)"
        );
        assert_eq!(
            local.cached_schedule.input_delay_per_track.len(),
            4,
            "schedule と同じ bundle 由来の topology 派生データも追従する"
        );
        while recycle_rx.pop().is_ok() {}
    }

    /// 畳み込みは「無い delta を引き継ぐ」だけで、新しい方が delta を持つ
    /// ときは新しい方が勝つ (LoadSong 2 連発で古い schedule が復活しない)。
    #[test]
    fn coalescing_prefers_the_newest_schedule_when_both_carry_one() {
        let (mut local, mut bundle_tx, mut recycle_rx) = harness();

        let mut s1 = Song::default();
        s1.tracks.push(track(1));
        let s1 = Arc::new(s1);
        let mut s2 = Song::default();
        for i in 1..=3 {
            s2.tracks.push(track(i));
        }
        let s2 = Arc::new(s2);

        bundle_tx.push(make_bundle(&s1)).unwrap();
        bundle_tx.push(make_bundle(&s2)).unwrap();
        local.refresh_bundle();

        assert_eq!(
            local.cached_schedule.input_delay_per_track.len(),
            3,
            "新しい topology bundle の schedule が勝つ"
        );
        while recycle_rx.pop().is_ok() {}
    }

    /// §5 D: topology 更新 (schedule = Some) は DelayLine の走行状態を
    /// stable key で移送する。
    #[test]
    fn topology_bundle_adopts_delay_line_state() {
        let (mut local, mut bundle_tx, _recycle_rx) = harness();

        // 2 track、片方に latency → 補償 DelayLine が 1 本出る song。
        let mut s1 = Song::default();
        s1.tracks.push(track(1));
        s1.tracks.push({
            let mut t = track(2);
            t.reported_latency_samples = 4;
            t
        });
        let s1 = Arc::new(s1);
        bundle_tx.push(make_bundle(&s1)).unwrap();
        local.refresh_bundle();
        assert_eq!(local.cached_schedule.delay_lines.len(), 1);

        // 走行状態を作る: ring に非ゼロを流し込む。
        {
            let line = &mut local.cached_schedule.delay_lines[0];
            let mut l = [1.0f32, 2.0, 3.0];
            let mut r = [4.0f32, 5.0, 6.0];
            line.step_in_place(&mut l, &mut r, 4);
        }

        // 同一 topology の再 compile (LoadSong 相当) を publish。
        bundle_tx.push(make_bundle(&s1)).unwrap();
        local.refresh_bundle();

        // 新 schedule の line が旧状態を引き継いでいる: さらに 3 sample 流すと
        // 遅延 4 の ring から最初に注入した値が出てくる (リセットなら 0)。
        let line = &mut local.cached_schedule.delay_lines[0];
        let mut l = [0.0f32; 3];
        let mut r = [0.0f32; 3];
        line.step_in_place(&mut l, &mut r, 4);
        assert_eq!(l[1], 1.0, "adopted ring must carry the pre-swap history");
        assert_eq!(l[2], 2.0);
    }

    /// 別プロジェクトの読み込み (`reset_song_scoped_state`) では走行状態を
    /// **引き継がない**。移送キー (`DelayKey::MixSrc{track_id}` /
    /// `ModSource::id`) は Song スコープの名前なので、project を跨ぐと別物
    /// 同士が一致し、前 project の PDC リングに残った音声が新 project の頭に
    /// 混ざる。
    #[test]
    fn project_switch_bundle_does_not_adopt_running_state() {
        let (mut local, mut bundle_tx, _recycle_rx) = harness();

        let mut s1 = Song::default();
        s1.tracks.push(track(1));
        s1.tracks.push({
            let mut t = track(2);
            t.reported_latency_samples = 4;
            t
        });
        let s1 = Arc::new(s1);
        bundle_tx.push(make_bundle(&s1)).unwrap();
        local.refresh_bundle();
        assert_eq!(local.cached_schedule.delay_lines.len(), 1);

        // 前 project の走行状態を作る。
        {
            let line = &mut local.cached_schedule.delay_lines[0];
            let mut l = [1.0f32, 2.0, 3.0];
            let mut r = [4.0f32, 5.0, 6.0];
            line.step_in_place(&mut l, &mut r, 4);
        }
        // per-track input delay line にも痕跡を残す (schedule の外で生き続ける)。
        {
            let mut l = [7.0f32, 8.0, 9.0];
            let mut r = [7.0f32, 8.0, 9.0];
            local.scratch[1]
                .input_delay_line
                .step_in_place(&mut l, &mut r, 4);
        }

        // 別 project の LoadSong 相当。
        bundle_tx.push(make_bundle_with_reset(&s1, true)).unwrap();
        local.refresh_bundle();

        let line = &mut local.cached_schedule.delay_lines[0];
        let mut l = [0.0f32; 3];
        let mut r = [0.0f32; 3];
        line.step_in_place(&mut l, &mut r, 4);
        assert_eq!(
            [l[0], l[1], l[2]],
            [0.0, 0.0, 0.0],
            "project 切替では前 project の PDC ring を引き継がない"
        );
    }

    /// `reset_song_scoped_state` は delta なので、coalescing で捨ててはいけない
    /// (捨てると project 切替のリセット要求が消える)。
    #[test]
    fn coalescing_keeps_the_project_switch_reset_request() {
        let (mut local, mut bundle_tx, mut recycle_rx) = harness();

        let mut s1 = Song::default();
        s1.tracks.push(track(1));
        s1.tracks.push({
            let mut t = track(2);
            t.reported_latency_samples = 4;
            t
        });
        let s1 = Arc::new(s1);
        bundle_tx.push(make_bundle(&s1)).unwrap();
        local.refresh_bundle();
        {
            let line = &mut local.cached_schedule.delay_lines[0];
            let mut l = [1.0f32, 2.0, 3.0];
            let mut r = [4.0f32, 5.0, 6.0];
            line.step_in_place(&mut l, &mut r, 4);
        }

        // project 切替の LoadSong の直後に値のみ更新 (OpenPluginShmem 相当) が
        // 同一バッファ周期で積まれる — 実際に曲を開くと必ず起きる並び。
        bundle_tx.push(make_bundle_with_reset(&s1, true)).unwrap();
        bundle_tx
            .push(RtBundle {
                song: Some(Arc::clone(&s1)),
                tempo_map: common::tempo_map::TempoMap::from_song(&s1),
                schedule: None,
                reset_song_scoped_state: false,
                input_delay_replacements: Vec::new(),
                plugin_refs: Arc::new(HashMap::new()),
                worker: None,
            })
            .unwrap();
        local.refresh_bundle();

        let line = &mut local.cached_schedule.delay_lines[0];
        let mut l = [0.0f32; 3];
        let mut r = [0.0f32; 3];
        line.step_in_place(&mut l, &mut r, 4);
        assert_eq!(
            [l[0], l[1], l[2]],
            [0.0, 0.0, 0.0],
            "畳み込みでリセット要求が落ちてはいけない"
        );
        while recycle_rx.pop().is_ok() {}
    }

    /// Proof of the D1 invariant: a steady-state install allocates and frees
    /// nothing on the audio thread. Requires the `rt-assert` allocator hook.
    #[cfg(feature = "rt-assert")]
    #[test]
    fn refresh_bundle_does_not_allocate_on_the_audio_thread() {
        let (mut local, mut bundle_tx, _recycle_rx) = harness();

        let mut s1 = Song::default();
        s1.tracks.push(track(1));
        let s1 = Arc::new(s1);
        bundle_tx.push(make_bundle(&s1)).unwrap();
        local.refresh_bundle(); // warm up (first install)

        let mut s2 = Song::default();
        s2.tracks.push(track(1));
        s2.tracks.push(track(2));
        let s2 = Arc::new(s2);
        bundle_tx.push(make_bundle(&s2)).unwrap();
        // 値のみ更新を重ねて coalescing の畳み込み (`supersede`) も同じ
        // install で通す (畳み込みは move / Vec ポインタ swap のみ)。
        bundle_tx
            .push(RtBundle {
                song: Some(Arc::clone(&s2)),
                tempo_map: common::tempo_map::TempoMap::from_song(&s2),
                schedule: None,
                reset_song_scoped_state: false,
                input_delay_replacements: Vec::new(),
                plugin_refs: Arc::new(HashMap::new()),
                worker: None,
            })
            .unwrap();

        // Steady-state install: pop the newest, adopt + swap the cached
        // fields, push the old to the recycle ring — all wait-free, no alloc,
        // no free.
        assert_no_alloc::assert_no_alloc(|| {
            local.refresh_bundle();
        });
    }
}
