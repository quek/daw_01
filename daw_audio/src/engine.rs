//! Audio engine state + per-buffer driver (CPAL コールバック側)。
//!
//! 役割分担 (`docs/plan_arch_refactor.md` §4/§5、複数プロジェクトは
//! `docs/plan_project_tabs.md` §3):
//! - `SharedState` — デバイス全体の wait-free フラグ面 (panic / park / app_active)。
//!   IPC 受信ループが書き、audio thread が毎 buffer 読む。
//! - [`ProjectShared`] — **プロジェクト (= タブ) ごと**の wait-free 面 + off-RT 読者
//!   (export thread / notify thread) 向けのミラー (`plugin_refs` / renderer / 表)。
//!   transport (playback / playhead / seek / loop / preroll / recording) はここ。
//! - [`EngineShared`] — デバイス全体の off-RT ミラー (`worker` / `sampler` / export 予約)
//!   と、開いている project の一覧 (`projects`)。
//! - [`ProjectRt`] — project ごとの RT 私有状態 (scratch / cached bundle / launcher /
//!   mod tick / 自分の bus)。[`RtBundle`] が rtrb の forward ring で配送され、superseded
//!   bundle は recycle ring で off-thread drop される。
//! - [`DeviceRt`] — CPAL クロージャ専有の状態: 開いている `ProjectRt` の列と
//!   デバイス最終ミックス (`master_l/r`)。[`DeviceRt::process_buffer`] が 1 buffer を
//!   駆動し、project ごとに live/export 共通の [`crate::graph::render_master_buffer`]
//!   で bus を描いて**加算**する。
//!
//! plugin dispatch は **有界** (`DISPATCH_TIMEOUT_MS`)。timeout した device は
//! [`PluginEntry::quarantined`]、pair は [`SyncSlot::poisoned`] で隔離され
//! (poisoning contract は `common::plugin_ref` module doc)、通知は notify
//! thread (`main.rs`) がフラグを poll して `AudioEvent` を送る。

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::audio_bridge::{AudioBridgeHandle, ProjectTelemetry};
use common::model::Song;
use common::protocol::{ProjectKey, SamplerSource};
use common::timing::{effective_loop_bounds, song_ended};

use crate::audio_clip_renderer::AudioClipRenderer;
use crate::graph::{DelayLine, Schedule, render_master_buffer};
use crate::metronome::{ClickVoice, render_metronome};
use crate::mixer::TrackScratch;
use crate::sampler::{SamplerRig, SamplerRt};
use crate::sequencer::NoteTransition;

pub use crate::engine_shared::*;

/// Hard cap on tracks the audio engine can render in a single buffer.
/// Picked to match `audio_bridge::MAX_TRACKS` so the per-track peak
/// meter doesn't fall off the GUI side.
pub const MAX_TRACKS: usize = 32;

/// 同時に開けるプロジェクト数 (`audio_bridge::MAX_PROJECTS` と同じ SSoT)。
/// [`DeviceRt::projects`] はこの容量で事前確保し、RT で伸びない。
pub const MAX_PROJECTS: usize = common::audio_bridge::MAX_PROJECTS;

/// 鍵盤プレビュー note の `note_id`。 sequencer が振る `sing_note_id` /
/// `talk_event_id` (= `[0, 1 << 28)` ∪ high band) のどちらとも衝突しない sentinel。
/// CLAP/VST3 は `note_id` を無視し、 builtin は key 一致で発音/停止するので、
/// on/off で同値であれば voice 対応が取れる。
pub(crate) const PREVIEW_NOTE_ID: u32 = common::process_data::NOTE_ID_NONE;

/// IPC 受信ループから audio thread へ渡す軽量コマンド。毎 buffer 頭の
/// `pump_commands` で drain される。project 宛のものは `project` で
/// [`DeviceRt::projects`] を線形走査 (≤ `MAX_PROJECTS`) して配る。
#[derive(Debug)]
pub enum EngineCommand {
    /// r.md #87: ランチャーの操作 (セル / 列の発火、停止、アレンジへ返す)。
    /// 発火の判断には `Song` (セルの [`common::model::LaunchSettings`]) が要るので、
    /// IPC スレッドでは解決せず audio thread の
    /// [`crate::launcher::LauncherRuntime`] へそのまま積む。
    Launch {
        project: ProjectKey,
        req: crate::launcher::LaunchRequest,
    },
    /// 鍵盤レーン click のプレビュー note-on (gui_01 #055)。 `track` は
    /// song.tracks の Vec index (= main.rs が `track_id` から現 song snapshot
    /// で解決済)、 `velocity` は normalized 0..=1。 `pump_commands` が該当
    /// track の `pending_preview` に積み、 `process_track_owned` が次の
    /// dispatch で frame 0 に注入する。
    PreviewNoteOn {
        project: ProjectKey,
        track: usize,
        pitch: u8,
        velocity: f64,
    },
    /// 鍵盤プレビューの note-off (gui_01 #055)。 `track` は note-on と同じ
    /// Vec index。
    PreviewNoteOff {
        project: ProjectKey,
        track: usize,
        pitch: u8,
    },
    /// Global Sampler の試聴 (`docs/plan_global_sampler.md` §3.2)。リングの
    /// `[start, end)` を master へ加算する。デバイス全体。
    SamplerPreview { start: u64, end: u64 },
    SamplerPreviewStop,
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PlaybackCommand {
    Stop = 0,
    Play = 1,
    /// r.md #118: 停止した位置から続ける再生 (Shift+Space)。 ランチャーのセルを撃ち直さず
    /// (`arm_reseed` しない)、 止まったときの位相のまま鳴らす。 それ以外は `Play` と同じ。
    PlayContinue = 2,
}

impl PlaybackCommand {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Play,
            2 => Self::PlayContinue,
            _ => Self::Stop,
        }
    }
}

/// デバイス全体の snapshot (worker rig / Global Sampler ring) を RT へ配送する便。
/// [`RtBundle`] と同じ forward / recycle ring の規約 (旧値は recycle で off-thread drop)。
/// どちらも snapshot field (最新が過去を包含する) なので畳み込みは不要。
pub struct DeviceBundle {
    pub worker: Option<Arc<WorkerRig>>,
    pub sampler: Option<Arc<SamplerRig>>,
}

/// project slot の生成 / 撤去を RT へ伝える便 (`docs/plan_project_tabs.md` §3.2)。
/// `Open` の `ProjectRt` は off-thread で丸ごと確保済み。`Close` された `ProjectRt`
/// は [`DeviceRt::project_recycle_tx`] で戻り、off-thread で drop される。
pub enum ProjectDelivery {
    Open(Box<ProjectRt>),
    Close(ProjectKey),
}

/// Off-thread で構築され、RT audio thread へ wait-free に配送される
/// snapshot 一式 (plan §4 の RtBundle)。`compile_schedule` /
/// `TempoMap::from_song` / plugin_refs map の rebuild は全部 recv loop 側で走り、
/// RT は swap (move / Arc clone) だけを行う。superseded bundle は recycle ring で
/// recv loop に返送され、`Drop` (free / shmem unmap) も off-thread で走る。
///
/// **不変条件 (field を足すときは必ずどちらか決めること)**: forward ring は
/// 両端が「最新だけ残す」 coalescing channel なので、
/// - **snapshot** field (`song` / `tempo_map` / `plugin_refs` / `preview_sequence`) は
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
    /// **delta**: `true` = このタブに別のファイルが読み込まれた (`Song::project_id` が
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
    /// **成長便**: この project の per-track scratch を `song.tracks.len()` ぶんまで
    /// 増やす置換 Vec (`None` = 据え置き)。install 時に既存の行を要素ごと swap して
    /// 移し、押し出された古い Vec をこの field に残して off-thread で drop する。
    ///
    /// `TrackScratch` は 1 本 ~450 KB (PDC リング 1 秒 ぶんが大半) なので、
    /// `MAX_TRACKS` を無条件に確保すると **タブ 1 枚につき ~14 MB** を、使うかどうかに
    /// 関わらず先に取る (`docs/plan_project_tabs.md` §3.1)。曲が要る本数だけ、song と
    /// **同じ便で** 届ける (別便にすると song だけ先に着いた buffer が無音になる)。
    pub scratch_growth: Option<Vec<TrackScratch>>,
    /// device_id → entry (Arc clone — recv loop のミラーと同一 entry)。
    pub plugin_refs: Arc<PluginRefs>,
    /// MIDI Capture の試聴シーケンス (Arc clone)。`None` = 停止。snapshot field。
    pub preview_sequence: Option<Arc<crate::sampler::PreviewSequence>>,
    /// **delta**: r.md #89 のクロス変調評価計画と、それに合わせて
    /// **off-thread で `install` 済み**の RT 状態。`None` = 据え置き。
    ///
    /// `ModRuntime::install` は `Vec::resize` するので RT では走らせない。
    /// 走行状態 (位相) は install で捨たれるが、次の buffer で `locate` が
    /// 位相表から張り直すので聴感上の段差にはならない。
    pub mod_plan: Option<(Arc<common::mod_graph::ModPlan>, common::mod_graph::ModRuntime)>,
    /// **delta**: 積分 tier の位相表 (off-thread build)。`None` = 据え置き。
    /// plan とは別便で届く — 表の構築は曲長ぶんの刻みループなので、plan の
    /// 配送を待たせない (構築中は旧表 + 閉形式シードで凌ぐ)。
    pub mod_phase_table: Option<Arc<common::mod_graph::ModPhaseTable>>,
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
        // scratch は **大きいほうが勝つ** (小さい便で縮めない)。採らなかった側は
        // `older` に載せて返す = off-thread drop。
        let (mine, theirs) = (
            self.scratch_growth.as_ref().map_or(0, Vec::len),
            older.scratch_growth.as_ref().map_or(0, Vec::len),
        );
        if theirs > mine {
            std::mem::swap(&mut self.scratch_growth, &mut older.scratch_growth);
        }
        // r.md #89: plan / 位相表も delta (`None` = 据え置き)。新しい便が
        // 持っていなければ古い便のものを引き継ぐ — 落とすと「plan を差し替えた
        // のに RT が旧 plan のまま」になり、変調が別のソースを指す。
        if self.mod_plan.is_none() {
            self.mod_plan = older.mod_plan.take();
        }
        if self.mod_phase_table.is_none() {
            self.mod_phase_table = older.mod_phase_table.take();
        }
        // 「捨てろ」は一度でも要求されたら畳み込み後も残す (OR)。落とすと
        // project 切替の走行状態リセットが coalescing で消える。
        self.reset_song_scoped_state |= older.reset_song_scoped_state;
        older
    }
}

/// 1 project ぶんの audio-thread-private 状態 (`docs/plan_project_tabs.md` §3.1)。
/// `OpenProject` で off-thread に丸ごと確保され、[`ProjectDelivery`] で RT へ渡る。
pub struct ProjectRt {
    pub key: ProjectKey,
    /// この project の共有面 (transport / preroll / master_gain / renderer)。
    pub shared: Arc<ProjectShared>,
    /// `AudioBridge` の slot index (claim 時に確定)。
    pub telemetry_slot: usize,
    /// Pre-allocated scratch buffers (MAX_TRACKS entries). The audio
    /// loop indexes into this with the current Song's track index — no
    /// resize, no allocation in the RT path.
    pub scratch: Vec<TrackScratch>,
    /// この project の master 出力 (= `render_master_buffer` の出力 + metronome)。
    /// デバイス最終ミックスへは [`DeviceRt::process_buffer`] が**加算**する。
    pub bus_l: Vec<f32>,
    pub bus_r: Vec<f32>,
    /// マスターストリップ (バスコンプ + トーン EQ + リミッター) の状態
    /// (`docs/plan_master_strip.md`)。live 用の 1 個 — 書き出しは `export` が
    /// 別に新品を持つので、書き出しの結果が直前の再生状態に影響されない。
    pub master_strip: crate::mixer::master_strip::MasterStripState,
    /// Whether the transport was rolling on the previous buffer. Used to
    /// detect Play/Stop transitions and reset the playhead / queue
    /// note-offs cleanly.
    pub playing: bool,
    /// [`Self::begin_buffer`] が transport 要求を消費する**前**の `playing`
    /// (r.md #87: 「停止中に撃ったか」の判定に要る)。
    was_playing: bool,
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
    /// SPSC carrying freshly off-thread-built [`RtBundle`]s from the receive
    /// loop to the audio thread. The audio thread `pop`s the newest
    /// (wait-free, no alloc — the value moves out of the pre-allocated ring
    /// slot) and swaps it into the cached fields below.
    pub bundle_rx: rtrb::Consumer<RtBundle>,
    /// SPSC to ship superseded bundles back to the receive loop for disposal.
    /// The audio thread `push`es the old snapshot here (wait-free, no alloc)
    /// when it swaps in a newer one; the receive loop `pop`s and drops them,
    /// so `Drop` (free / unmap) runs off the audio thread.
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
    /// MIDI Capture の試聴シーケンス (bundle 由来 Arc clone)。
    pub preview_sequence: Option<Arc<crate::sampler::PreviewSequence>>,
    /// r.md #87: クリップランチャーの走行状態 (行ごとの予約 / フォローアクション /
    /// 供給元)。**`Song` には書き戻さない** — 詳細は
    /// [`crate::launcher::LauncherRuntime`] の doc。事前確保のみで RT で伸びない。
    pub launcher: crate::launcher::LauncherRuntime,
    /// docs/plan_modulation.md §5 / r.md #89: 使い回しの変調値面
    /// (**`ModSource::id` キー**)。dispatch の前に
    /// [`crate::mod_tick::eval_plane`] で満たし (= follower は前 buffer の
    /// envelope、generator はこの刻みの song 位置から算出)、audio worker へ
    /// 渡して volume / pan / plugin param の `mod_routings` を変調する。
    /// buffer をまたいで再利用する (warm 後は確保が起きない)。
    pub mod_tick: crate::mod_tick::ModTickRunner,
    /// r.md #89: `Schedule` の follower slot → 係数表の列。plan / schedule の
    /// どちらかが変わったら作り直す (`ModTickRunner::build_follower_cols`)。
    pub follower_cols: Vec<u16>,
    /// r.md #89: plan slot → `Schedule::follower_slots` の index
    /// (`ModTickRunner::build_follower_env_map`)。刻みごとの線形探索を避けるため
    /// plan / schedule の差し替え時に 1 度だけ作る。
    pub follower_env_of_slot: Vec<u16>,
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

/// [`ProjectRt::render_buffer`] に渡す、デバイス側が所有する資源。
pub struct DeviceCtx<'a> {
    pub worker: Option<&'a WorkerRig>,
    /// この project が Global Sampler の録音源に縛られているときだけ `Some`。
    pub sampler: Option<&'a SamplerRig>,
    pub sampler_rt: &'a mut SamplerRt,
    /// この project が scope (マスターメーター) の対象のときだけ `Some`。
    pub scope: Option<&'a common::scope_bridge::ScopeBridgeHandle>,
    /// stream 開始からの累積 render フレーム数 (MIDI 試聴シーケンスの時計)。
    pub frames_rendered: u64,
}

impl ProjectRt {
    pub fn new(
        max_frames: usize,
        shared: Arc<ProjectShared>,
        bundle_rx: rtrb::Consumer<RtBundle>,
        bundle_recycle_tx: rtrb::Producer<RtBundle>,
        stretch_pool_rx: rtrb::Consumer<StretchPoolDelivery>,
        stretch_pool_recycle_tx: rtrb::Producer<StretchPoolDelivery>,
    ) -> Self {
        // **空から始める** — 実際に要る本数は song と同じ便 (`RtBundle::scratch_growth`)
        // で届く。容量だけ予約しておき、成長便の install が再確保しないようにする。
        let scratch = Vec::with_capacity(MAX_TRACKS);
        Self {
            key: shared.key,
            telemetry_slot: shared.telemetry_slot,
            shared,
            stretch_pool_rx,
            stretch_pool_recycle_tx,
            scratch,
            bus_l: vec![0.0; max_frames],
            bus_r: vec![0.0; max_frames],
            master_strip: crate::mixer::master_strip::MasterStripState::new(),
            playing: false,
            was_playing: false,
            playhead_beats: 0.0,
            last_known_playhead: u64::MAX,
            metronome_voice: None,
            bundle_rx,
            bundle_recycle_tx,
            cached_schedule: Schedule::empty(),
            cached_song: None,
            // 初期は default song (= constant 120bpm)。 seek/loop-wrap は線形に縮退。
            tempo_map: common::tempo_map::TempoMap::from_song(&Song::default()),
            plugin_refs: Arc::new(HashMap::new()),
            preview_sequence: None,
            launcher: crate::launcher::LauncherRuntime::new(),
            mod_tick: crate::mod_tick::ModTickRunner::new(),
            follower_cols: Vec::with_capacity(common::audio_bridge::MAX_MOD_SOURCES),
            follower_env_of_slot: Vec::with_capacity(common::audio_bridge::MAX_MOD_SOURCES),
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
    pub(crate) fn refresh_bundle(&mut self) {
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
        let old_preview_sequence =
            std::mem::replace(&mut self.preview_sequence, new.preview_sequence.take());

        // r.md #89: plan / 位相表の差し替え。旧 RT 状態と旧表は recycle bundle に
        // 載せて off-thread で drop する (`ModRuntime` は `Vec` を 6 本持つ)。
        let mut retired_plan: Option<(
            Arc<common::mod_graph::ModPlan>,
            common::mod_graph::ModRuntime,
        )> = None;
        let mut plan_or_schedule_changed = false;
        if let Some((plan, rt)) = new.mod_plan.take() {
            retired_plan = Some(self.mod_tick.install(plan, rt));
            plan_or_schedule_changed = true;
        }
        let retired_table = new
            .mod_phase_table
            .take()
            .and_then(|t| self.mod_tick.set_table(Some(t)));

        // per-track scratch の成長便。**song と同じ便で届く**ので、この install の
        // 直後に走る render は必ず足りた状態で始まる。走行状態 (PDC リング / strip の
        // フィルタ / 鳴っているノート) を保つため、既存の行は要素ごと swap で移す
        // (move だけ = RT で確保も解放もしない)。押し出した古い Vec は bundle に
        // 載せ替えて recycle ring へ (drop は off-thread)。
        if let Some(mut fresh) = new.scratch_growth.take() {
            if fresh.len() > self.scratch.len() {
                for (dst, src) in fresh.iter_mut().zip(self.scratch.iter_mut()) {
                    std::mem::swap(dst, src);
                }
                fresh = std::mem::replace(&mut self.scratch, fresh);
            }
            new.scratch_growth = Some(fresh);
        }

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
                    s.repitch_accum.fill((u64::MAX, 0.0));
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
            plan_or_schedule_changed = true;
        }
        // r.md #89: 「schedule の follower slot」と「plan の slot」の写像は
        // どちらが変わっても張り直す (刻みごとの線形探索を避けるための表)。
        if plan_or_schedule_changed {
            self.mod_tick
                .build_follower_cols(&self.cached_schedule.follower_keys, &mut self.follower_cols);
            self.mod_tick.build_follower_env_map(
                &self.cached_schedule.follower_keys,
                &mut self.follower_env_of_slot,
            );
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
            scratch_growth: new.scratch_growth.take(),
            plugin_refs: old_refs,
            preview_sequence: old_preview_sequence,
            mod_plan: retired_plan,
            mod_phase_table: retired_table,
        };
        let _ = self.bundle_recycle_tx.push(recycled);
    }

    /// r.md #89: 変調の位相と transport を `sample` 位置で張り直す
    /// (シーク / ループ折返し / 再生開始)。song が無ければ何もしない。
    ///
    /// **`self.playhead_beats` は呼ぶ前に tempo map で同期しておくこと** —
    /// 刻み境界へ丸めた拍をここで求める起点になる。
    fn locate_mod(&mut self, song: Option<&Song>, sample: u64, sample_rate: u32) {
        if let Some(s) = song {
            self.mod_tick
                .locate(s, sample, self.playhead_beats, sample_rate);
        }
    }

    /// track ごとの表示用テレメトリ (peak / GR / 鳴っているボイス) を `AudioBridge` へ publish
    /// する。 同じ走査で出す = 同じ buffer の値だと保証される。 Atomic store のみ (RT 安全)。
    ///
    /// ボイス (r.md #117、 変調ラックの per-voice カーソル用) は chain の **最初の** plugin の
    /// ボイス表 = この track の MIDI 入力 (Selector で別 chain に居ても同じ MIDI を受ける)。
    /// plugin が無ければ空。
    fn publish_track_telemetry(&self, slot: &ProjectTelemetry, n_tracks: usize) {
        for (i, tr) in self.scratch.iter().take(n_tracks).enumerate() {
            slot.set_track_peak(i, tr.peak_l, tr.peak_r);
            // 内蔵チャンネルストリップの GR (docs/plan_channel_strip.md §9)。
            slot.set_track_gr_db(i, tr.strip_gr_db);
            let voices = self
                .cached_schedule
                .track_programs
                .get(i)
                .and_then(|p| p.voices.first())
                .into_iter()
                .flat_map(|vt| vt.iter())
                .map(|v| common::audio_bridge::VoiceSnapshot {
                    on_beat: v.on_beat,
                    on_secs: v.on_secs,
                    off_secs: v.off_secs,
                });
            slot.publish_track_voices(i, voices);
        }
    }

    /// r.md #89: この buffer が踏む制御刻みを回して、値面と transport を解く。
    ///
    /// envelope follower の値は `ModRuntime::set_follower` 経由でしか `tick` に
    /// 渡らない (plan の slot 順と `Song::mod_sources` の位置順の取り違えを型で
    /// 防ぐ設計) ので、`follower_env_of_slot` の写像で引いて渡す。
    ///
    /// 戻り値は buffer 頭の transport (`beat` / `bpm`)。
    fn run_mod_ticks(
        &mut self,
        song: &Song,
        playhead: u64,
        frames: u32,
        sample_rate: u32,
    ) -> common::mod_graph::PhaseMark {
        // install 直後 (plan 差し替え / 起動直後) は着地していないので、
        // まず現在位置で位相と transport を張る。
        if self.mod_tick.needs_locate() {
            self.mod_tick
                .locate(song, playhead, self.playhead_beats, sample_rate);
        }
        let sched = &self.cached_schedule;
        let env_of = &self.follower_env_of_slot;
        let follower_env = |plan_slot: u16, tick: i64| {
            match env_of.get(usize::from(plan_slot)).copied() {
                Some(i) if i != u16::MAX => sched
                    .follower_slots
                    .get(usize::from(i))
                    .map_or(0.0, |f| f.env_at_tick(tick)),
                _ => 0.0,
            }
        };
        // r.md #117: `Note` 起点のソースの最新ノート = 帰属トラックの device chain 入力で最後に
        // 鳴った note-on (`PerTrackState::latest_note`)。 slot → source → owner track → scratch。
        // Arc の clone は参照カウントの増減だけ (確保・解放なし)。
        let plan = std::sync::Arc::clone(&self.mod_tick.plan);
        let scratch = &self.scratch;
        let note_anchor = |plan_slot: u16| -> Option<common::mod_graph::NoteAnchor> {
            let idx = plan.nodes.get(usize::from(plan_slot))?.owner_track_index?;
            scratch.get(idx as usize)?.state.latest_note
        };
        self.mod_tick
            .run_buffer(song, playhead, frames, sample_rate, follower_env, note_anchor)
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
        loop {
            // **行がまだ届いていない便は pop しない。** scratch は song と同じ便で
            // 伸びる (`RtBundle::scratch_growth`) ので、pool の配送が先に着くことが
            // ある。pop してから捨てると publish 側の「配送済み」だけが進み、その
            // track のストレッチが二度と揃わない (`delivered_engines_per_track`)。
            match self.stretch_pool_rx.peek() {
                Ok(d) if d.track_idx < self.scratch.len() => {}
                _ => break,
            }
            let Ok(mut delivery) = self.stretch_pool_rx.pop() else { break };
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

    /// GUI からの seek 要求と Play / Stop の要求を消費して `self.playing` を
    /// 更新する。`render_buffer` の**あらゆる早期 return より先に**呼ぶこと
    /// (r.md #51 — count-in 中 / 書き出し中に要求を溜めたまま return すると、
    /// 溜まった Play がずっと後になって勝手に発火する)。
    ///
    /// seek を audio thread 単独 writer として `playhead` に反映するのも
    /// ここ。IPC スレッドが `playhead` を直接書くと、buffer 末の advance store と
    /// 同一 atomic を別スレッドから書く race になり、Stop 直後 (in-flight buffer が
    /// まだ playing で advance する瞬間) に開始位置への巻き戻しが上書きされて
    /// 停止位置から再生されてしまう。`swap` で消費する (多重要求は last-wins)。
    fn consume_transport_requests(&mut self) {
        let shared = Arc::clone(&self.shared);
        let pending_seek = shared.pending_seek.swap(NO_PENDING_SEEK, Ordering::AcqRel);
        if pending_seek != NO_PENDING_SEEK {
            shared.playhead.store(pending_seek, Ordering::Release);
            // 飛び先には「今鳴っている note の Off」が無い。sequencer は Off が
            // 当該 buffer 窓に入るときだけ emit する (`collect_events_for_buffer`)
            // ので、跳び越した Off は二度と出ず note が鳴り続ける。再生中の seek
            // (`R` = 選択範囲ループ / ルーラークリック / `f` / Home / End) は全部
            // ここを通るので、Stop / loop wrap と同じ flush をこの経路でも通す。
            self.queue_all_notes_off();
        }

        // Play / Stop edge handling. On Play, restart playhead and clear
        // active notes. On Stop, queue offs at frame 0 of the next buffer
        // so plugins drain cleanly.
        let desired = PlaybackCommand::from_u8(shared.playback.load(Ordering::Acquire));
        match (self.playing, desired) {
            (false, cmd @ (PlaybackCommand::Play | PlaybackCommand::PlayContinue)) => {
                self.playing = true;
                // r.md #87 §1.4: 再生の起点は **ユーザーが最後に撃った状態**
                // (`Track.launcher` / `AutomationLane.launcher`)。フォローアクションで
                // 移った先は走行状態にしか無いので、停止 → 再生で同じセルが鳴り直す。
                // r.md #118: 「停止位置から続ける」 は撃ち直さない — 走行状態 (`launch_beat`)
                // は停止中も残っていて、seek ぶんは `on_transport_jump` が平行移動済みなので、
                // 止まったときの位相のまま続く。
                if cmd == PlaybackCommand::Play {
                    self.launcher.arm_reseed();
                }
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
                // count-in は「再生中」の一形態なので、止めたら一緒に捨てる。
                // 残すと次に Play したとき、頼んでいない count-in が鳴る。
                shared.preroll_remaining_samples.store(0, Ordering::Release);
                shared.preroll_total_samples.store(0, Ordering::Release);
                self.queue_all_notes_off();
            }
            _ => {}
        }
    }

    /// 鳴っている全 note を「次の drain (= 各 track の process 冒頭、frame 0)」で
    /// 出す NoteOff として予約し、追跡集合を空にする。
    ///
    /// **stuck note を防ぐ 3 経路 (Stop / loop wrap / seek) が共有する唯一の口。**
    /// どれか 1 つで漏らすと、跳び越された Off が二度と emit されず note が
    /// 鳴り続ける (`pending_offs` を積む処理を各所に手写しすると必ずどれかが
    /// 漏れるので、増やすときもここを呼ぶこと)。
    ///
    /// RT-safe: `pending_offs` は `process_track_owned` の冒頭で毎 buffer drain +
    /// clear され、この関数はどれも buffer 冒頭 (`consume_transport_requests`) か
    /// buffer 末 (loop wrap) で呼ばれるので push 時点では空。`active_notes` は
    /// `ACTIVE_NOTES_CAP` (= `PerTrackState::with_capacity` の確保量) でクランプ
    /// 済みなので、push で再確保しない。
    fn queue_all_notes_off(&mut self) {
        crate::mixer::queue_all_notes_off(&mut self.scratch);
    }

    /// 鍵盤プレビューを該当 track の `pending_preview` に積む。capacity 上限で
    /// guard し RT での realloc を避ける (`push_note_on` と同じ「溢れたら drop」方針)。
    fn push_preview(&mut self, track: usize, ev: NoteTransition) {
        if let Some(s) = self.scratch.get_mut(track) {
            let pp = &mut s.state.pending_preview;
            if pp.len() < pp.capacity() {
                pp.push(ev);
            }
        }
    }

    /// 「今このプロジェクトは走っているか (count-in 込み)」 — デバイスの idle park 判定用。
    #[must_use]
    pub fn is_rolling(&self) -> bool {
        self.playing || self.shared.preroll_remaining_samples.load(Ordering::Acquire) > 0
    }

    /// 1 buffer の前段: bundle の install、stretch pool の取り込み、transport 要求の
    /// 消費、走行状態の publish。**書き出し gate より前に、全 project について**呼ぶ
    /// (r.md #51 — 要求を溜めたまま return すると溜まった Play が後で勝手に発火する)。
    fn begin_buffer(&mut self, slot: &ProjectTelemetry) {
        // Install the newest off-thread snapshot (song / schedule /
        // plugin_refs) before the dispatch starts.
        self.refresh_bundle();
        // r.md #40: audio clip renderer snapshot を load する前に stretch engine
        // pool を取り込む (publish 側は pool → schedule の順で出す)。
        self.refresh_stretch_pools();
        // r.md #87: 「停止中に撃ったか」は **transport 要求を消費する前**の状態でしか
        // 分からない (`render_buffer` の doc)。
        self.was_playing = self.playing;
        self.consume_transport_requests();
        let recording_requested = self.shared.recording_requested.load(Ordering::Acquire);
        let preroll = self.shared.preroll_remaining_samples.load(Ordering::Acquire);
        // GUI はこの 3 つを観測して transport 表示と録音セッションを決める
        // (GUI 側に「Play を送った記憶」を持たせない = 状態の所有者は engine)。
        // count-in を止めたときの 0 もここで必ず伝わる (count-in ブロックの中だけで
        // mirror していると、停止で捨てた preroll が GUI 側に残る)。
        slot.set_playing(self.playing);
        slot.set_recording_live(recording_requested && self.playing && preroll == 0);
        slot.set_preroll_remaining(preroll);
    }

    /// Render `frames` of this project's master into `bus_l/r`. Transport 状態を
    /// 進め、live/export 共通の `render_master_buffer` で描画し、metronome
    /// (monitoring 専用) を重ね、meters / mod scalars を publish する。
    /// [`Self::begin_buffer`] の後に呼ぶ。
    fn render_buffer(
        &mut self,
        ctx: DeviceCtx<'_>,
        slot: &ProjectTelemetry,
        sample_rate: u32,
        frames: usize,
    ) {
        let song_snapshot = self.cached_song.clone();
        let n = frames;
        self.bus_l[..n].fill(0.0);
        self.bus_r[..n].fill(0.0);

        // Snapshot the transport-state atomics once for the whole buffer so
        // every step below sees a single consistent view (loop wrap /
        // metronome gate). Loading each atomic at multiple call sites could
        // otherwise observe a mid-buffer flip and produce an internally
        // inconsistent buffer.
        //
        // ループ状態は 3 値まとめて 1 回だけ copy-out する (buffer 途中で
        // ON/OFF と範囲が食い違って見えない)。 `LoopRegion` は `Copy` なので
        // guard は即座に落とせる = RT 上で Arc を持ち回らない。
        let loop_region = **self.shared.loop_region.load();
        let looping = loop_region.enabled;
        let metronome_enabled = self.shared.metronome_enabled.load(Ordering::Acquire);
        // r.md #87: グローバルローンチ量子化。**SSoT は `Song`** — セルの量子化が
        // `Global` のとき「いつ鳴り始めるか」がこれで決まり、書き出す音が変わるので
        // 曲の一部 (計画書 Q9 / Q10)。buffer 頭で 1 回だけ読む (行ごとに読むと、
        // 同じ buffer 内で行ごとに別の格子へ発火しうる)。
        let global_launch_quantize = song_snapshot
            .as_ref()
            .map(|s| s.global_launch_quantize)
            .unwrap_or(common::model::DEFAULT_GLOBAL_LAUNCH_QUANTIZE);
        let was_playing = self.was_playing;
        let recording_requested = self.shared.recording_requested.load(Ordering::Acquire);
        let preroll = self.shared.preroll_remaining_samples.load(Ordering::Acquire);

        // Phase 7 B4 Step C: count-in モード — preroll > 0 なら通常 dispatch /
        // clip render を skip し、 metronome のみ render + preroll counter を
        // deduct + audio_bridge に mirror。 0 到達で通常再生に戻る。
        // count-in は「再生中」の一形態なので、Stop が届いていればここには来ない
        // (`begin_buffer` の `consume_transport_requests` が playing を落とし、
        // `preroll > 0 && self.playing` で弾かれる)。
        if preroll > 0 && self.playing {
            let total = self.shared.preroll_total_samples.load(Ordering::Acquire);
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
                    &mut self.bus_l[..n],
                    &mut self.bus_r[..n],
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
            slot.set_preroll_remaining(new_preroll);
            // count-in 中のローンチ操作も溜めない (書き出し中と同じ理由)。
            self.launcher.clear_requests();
            return;
        }

        let playing = self.playing;

        let song_ref = song_snapshot.as_deref();
        let playhead = self.shared.playhead.load(Ordering::Acquire);

        // Phase 4 Step C-2: 「現在 recording 中の lane」 を ProjectShared から
        // 1 buffer 分の lifetime で借りる。
        let recording_lanes_g = self.shared.recording_lanes.load();
        let recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)> =
            &recording_lanes_g;

        // Phase 5 Step 5.2: seek 検出 + playhead_beats 同期。 前 buffer 末で
        // 記録した `last_known_playhead` と current playhead を比較し、 一致
        // していなければ (= IPC SeekTo / Play edge / loop wrap / 起動直後)
        // tempo map で正確に beat を逆算する (A10 r.md #8)。
        if playhead != self.last_known_playhead {
            // 跳びの起点は **前 buffer の末尾** (`last_known_playhead`)。`playhead_beats` は
            // 前 buffer の**頭**の拍 (song があるとき刻みが buffer 頭で解いた値のまま) なので、
            // それを起点にすると跳び量が 1 buffer ぶん多くなり、ランチャーの時計が
            // 毎回 1 buffer 遅れる (位相が ~10ms 繰り返す / フォローが 1 buffer 遅れる)。
            let prev_beats = self.tempo_map.samples_to_beat(self.last_known_playhead, sample_rate);
            self.playhead_beats = self.tempo_map.samples_to_beat(playhead, sample_rate);
            // r.md #87: 跳んだぶんだけランチャーの絶対拍も動かす (`on_transport_jump`
            // の doc — これが無いと seek の後にセルが 1 周無音になり、予約と
            // フォローアクションを跳び越して二度と発火しない)。
            self.launcher.on_transport_jump(self.playhead_beats - prev_beats);
            // r.md #89: 変調の位相も張り直す。積分 tier は位相表の breakpoint から
            // **同じ漸化式で**前進するので、曲頭から通しで再生したときと厳密に
            // 一致する (= どこから再生しても同じ位相)。
            self.locate_mod(song_ref, playhead, sample_rate);
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
        // r.md #89: この buffer の変調と transport は**制御グリッド**で解く。
        // `ModTickRunner` が刻み (64 サンプル、絶対位置に整列) ごとに
        // `mod_graph::tick` を回し、`next_mark` の規則で拍とテンポを進める。
        // **ここで `playhead_beats += frames * bpm / (60·SR)` と自前に進めては
        // いけない** — `ModPhaseTable` / `locate` が同じ漸化式で位相を張るので、
        // 進め方が 1 つでもずれると「どこから再生しても同じ位相」が壊れる。
        let head_mark = match song_ref {
            Some(s) => self.run_mod_ticks(s, playhead, n as u32, sample_rate),
            None => common::mod_graph::PhaseMark {
                beat: self.playhead_beats,
                secs: 0.0,
                bpm: 120.0,
            },
        };
        // 刻みが解いた buffer 頭の拍。ここが以降の描画の SSoT。
        if song_ref.is_some() {
            self.playhead_beats = head_mark.beat;
        }
        #[allow(clippy::cast_possible_truncation)]
        let current_bpm: f32 = match song_ref {
            // Tempo lane を録音中は curve eval を skip して constant fallback。
            Some(s) if tempo_recording => s.bpm,
            Some(_) => head_mark.bpm as f32,
            None => 120.0,
        };

        if let Some(song) = song_ref {
            let n_tracks = song.tracks.len().min(MAX_TRACKS);

            // PR6: audio clip renderer snapshot for this buffer. Guard stays
            // live until the end of the call so workers can safely deref it.
            let audio_renderer_g = self.shared.audio_clip_renderer.load();
            let audio_renderer: &AudioClipRenderer = &audio_renderer_g;

            // r.md #87: 行ごとの時間軸を **dispatch より前に**確定させる
            // (worker はこのテーブルをポインタで読む)。予約の発火 / フォロー
            // アクション / セルのループ解決はすべてここで済む。
            let span = crate::launcher::runtime::BufferSpan::new(
                self.playhead_beats,
                current_bpm,
                sample_rate,
                n as u32,
            );
            self.launcher.update(song, span, global_launch_quantize, was_playing);

            // Global Sampler: 録音源が PreFx / PostFx tap なら、その track に
            // snapshot を要求する flag を render の前に立てる (この project が
            // 録音源のときだけ `ctx.sampler` が `Some`)。
            ctx.sampler_rt.arm_snapshot_flags(ctx.sampler, Some(song), &mut self.scratch);
            // MIDI Capture の試聴: この buffer に入るノートを pending_preview へ
            // (シーケンスは bundle の snapshot field で届く = RT で ArcSwap を load しない)。
            ctx.sampler_rt.step_preview_sequence(
                self.key,
                self.preview_sequence.as_deref(),
                Some(song),
                &mut self.scratch,
                ctx.frames_rendered,
                n,
            );

            // live/export 共通の単一 render 経路 (§5): dispatch → schedule →
            // master fx → master gain。
            let master_gain = f32::from_bits(self.shared.master_gain.load(Ordering::Relaxed));
            render_master_buffer(
                song,
                &mut self.cached_schedule,
                &mut self.scratch,
                &self.plugin_refs,
                ctx.worker,
                audio_renderer,
                &mut self.bus_l[..n],
                &mut self.bus_r[..n],
                sample_rate,
                n as u32,
                playing,
                loop_region,
                recording_lanes,
                current_bpm,
                self.playhead_beats,
                self.mod_tick.plane(),
                self.mod_tick.follower_drive(&self.follower_cols, playhead),
                self.launcher.rows(),
                master_gain,
                &mut self.master_strip,
            );

            // 走行状態の GUI への publish は **transport を進めた後** (この関数の末尾)。
            // 理由は同所のコメントと [`crate::launcher::LauncherRuntime::publish`] の doc。

            // r.md #50: マスター出力サンプルを GUI のメーター解析リングへ流す
            // (アクティブなタブ = `SetScopeProject` の project だけ)。
            // **metronome click を重ねる前** に取るのがこのタップ位置の要点で、
            // これで「メーターの数値 = 書き出す WAV の数値」が構造的に一致する
            // (grill-me で確定した測定対象 = 曲の音だけ)。事前確保済み shmem への
            // store のみなので RT 安全 (確保・ロック・I/O 無し)。
            if let Some(scope) = ctx.scope {
                scope.write_block(&self.bus_l[..n], &self.bus_r[..n]);
            }

            // Global Sampler (`docs/plan_global_sampler.md` §3.2): 録音源をリングへ
            // 書き、そのあとで試聴音を bus に足す (試聴を再録しない)。scope と
            // 同じく metronome の前 = 曲の音だけ。事前確保済み shmem への store のみ。
            let transport = crate::sampler::BlockTransport {
                playing,
                playhead,
                playhead_beat: self.playhead_beats,
                bpm: current_bpm,
            };
            if let Some(rig) = ctx.sampler {
                ctx.sampler_rt.write_block(
                    rig,
                    Some(song),
                    &self.scratch,
                    &self.bus_l,
                    &self.bus_r,
                    n,
                    transport,
                );
                ctx.sampler_rt
                    .mix_preview(rig, &mut self.bus_l[..n], &mut self.bus_r[..n], n);
            }

            // metronome click を bus に重ねる (monitoring 専用 — export
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
                    &mut self.bus_l[..n],
                    &mut self.bus_r[..n],
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
            self.publish_track_telemetry(slot, n_tracks);
            // マスターストリップの GR (docs/plan_master_strip.md §6)。波形からは
            // 導けない値なので、per-track の GR と同じスカラー面で publish する。
            let (comp_gr, limiter_gr) = self.master_strip.gain_reduction_db();
            slot.set_master_gr_db(comp_gr, limiter_gr);

            // docs/plan_modulation.md §4.2 / r.md #89: 変調値面を GUI へ publish する。
            // 刻みが解いた buffer 頭の値をそのまま出す (GUI は 30Hz なので
            // 刻みの粒度は要らない)。値と id を組で書く seqlock なので、
            // GUI が「新しい id と古い値」を掴むことはない。Atomic store のみ。
            slot.publish_mod_plane(self.mod_tick.publish_plane());

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
                    let master_peak = self.bus_l[..n]
                        .iter()
                        .chain(self.bus_r[..n].iter())
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
                        project = self.key.0,
                        playing,
                        playhead,
                        master_peak,
                        track_peaks = ?self.heartbeat_track_peaks,
                        device_ids = ?self.heartbeat_device_ids,
                        n_sync_slots = ctx.worker.map(|rig| rig.slots.len()).unwrap_or(0),
                        worker_pool = ctx.worker.is_some_and(|rig| rig.pool.is_some()),
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
            // r.md #87: ランチャーが鳴っている間は曲末で止めない (上の doc)。
            let reached_end = reached_transport_end(
                recording_requested || self.launcher.any_cell_playing(),
                active_end,
                new_ph,
                song_ended(song_ref, sample_rate, new_ph),
            );
            // r.md #89: `playhead_beats` は **刻みが進める** (この buffer の頭で
            // `ModTickRunner::run_buffer` が解いた値を入れてある)。ここで
            // buffer 単位に足し込むと、位相表と実演奏の拍軸が食い違う
            // (= シークすると変調の位相が飛ぶ)。song が無いときだけ、
            // 従来どおり buffer 定数で進める。
            let sr = sample_rate as f64;
            if sr > 0.0 && song_ref.is_none() {
                self.playhead_beats += n as f64 * f64::from(current_bpm) / (60.0 * sr);
            } else if song_ref.is_some()
                && let Some(end_beat) = self.mod_tick.beat_at_sample(new_ph, sample_rate)
            {
                // buffer **末** の拍 (刻みが解いた値)。次 buffer の頭で `run_buffer` が上書き
                // するので通常は使われないが、その間に LoadSong で plan が差し替わると
                // `locate` がこれを起点に張り直す。buffer 頭の拍のままだと張り直しのたびに
                // 1 buffer ぶん拍が遅れ、再生中に変調器を編集するたびに音が遅れて累積した。
                self.playhead_beats = end_beat;
            }
            if reached_end {
                self.queue_all_notes_off();
                let wrap_to = if looping {
                    effective_loop_bounds(song_ref, loop_region, sample_rate).map(|(s, _)| s)
                } else {
                    None
                };
                if let Some(start) = wrap_to {
                    // 巻き戻しの起点は **この buffer の末尾** (`new_ph` = 進めた後の位置)。
                    // `playhead_beats` (buffer 頭の拍) を起点にすると跳び量が 1 buffer ぶん
                    // 多くなり、ランチャーの時計が周回ごとに 1 buffer 遅れて累積する。
                    let prev_beats = self.tempo_map.samples_to_beat(new_ph, sample_rate);
                    new_ph = start;
                    // A10 (r.md #8): loop start の beat も tempo map で正確に
                    // 逆算 (constant-bpm 線形推定は tempo automation 中の loop
                    // boundary でズレた)。
                    self.playhead_beats = self.tempo_map.samples_to_beat(new_ph, sample_rate);
                    // r.md #87: ループで巻き戻したぶん、ランチャーの絶対拍も戻す
                    // (セルの位相はループを跨いでも連続する)。
                    self.launcher.on_transport_jump(self.playhead_beats - prev_beats);
                    // r.md #89: 変調の位相も折返し位置で張り直す (seek と同じ扱い)。
                    self.locate_mod(song_ref, new_ph, sample_rate);
                } else {
                    self.playing = false;
                    self.shared
                        .playback
                        .store(PlaybackCommand::Stop as u8, Ordering::Release);
                }
            }
            self.shared.playhead.store(new_ph, Ordering::Release);
            self.last_known_playhead = new_ph;
        } else {
            // Stop 中は audio thread が playhead を advance しない。GUI からの
            // SeekTo は begin_buffer の pending_seek consume で (audio
            // thread 自身が) shared.playhead に反映済みなので、その値で
            // last_known_playhead を同期し、次 Play 開始時の seek 検出を
            // 誤発火させない。
            self.last_known_playhead = playhead;
        }

        // r.md #87: 走行状態 (鳴っているセル / 予約 / 進捗) を GUI へ publish。
        // `Song` には入れない (§1.4 — 遷移先を保存すると書き出しの再現性が壊れる)。
        //
        // **`shared.playhead` を store した後、同じ時間軸で publish する。**
        // GUI は `playhead` と publish 値 (`launch_beat`) を組にして `cell_phase` を
        // 解くので、2 つが別の時間軸を指した 1 フレームは位相が解けず、
        // **ランチャー行の映像 / 画像 / 字幕とピアノロールの再生線がまるごと消える**。
        // 以前は publish が buffer の中ほど、ループの巻き戻し
        // (`on_transport_jump`) と `playhead` の store が末尾にあったので、
        // ループ端の 1 buffer は必ず「playhead はループ先頭 / launch_beat は
        // 巻き戻し前の絶対拍」の組になっていた (30Hz の poll で数周に 1 回、
        // そのまま画面に出る)。
        if song_snapshot.is_some() {
            self.launcher.publish(slot, self.playhead_beats);
        }
    }
}

/// CPAL クロージャ専有の状態 (`docs/plan_project_tabs.md` §3.1)。開いている
/// [`ProjectRt`] の列とデバイス最終ミックス。project の追加 / 撤去は
/// [`ProjectDelivery`] で off-thread から届く (RT で確保しない)。
pub struct DeviceRt {
    /// デバイス最終ミックス (全 project の bus を加算したもの)。
    pub master_l: Vec<f32>,
    pub master_r: Vec<f32>,
    /// 開いている project (容量 `MAX_PROJECTS` を事前確保、`push` / `swap_remove`
    /// は再確保しない)。並びに意味は無い (ミックスは加算)。
    ///
    /// `Box` なのは意図: `ProjectRt` は scratch 込みで数 MB あり、`Vec<ProjectRt>` だと
    /// push / swap_remove / ring の move が RT で MB 級の memcpy になる。Box なら
    /// ポインタ 1 本の move。
    #[allow(clippy::vec_box)]
    pub projects: Vec<Box<ProjectRt>>,
    /// project slot の生成 / 撤去便 (recv loop → RT)。
    pub project_rx: rtrb::Consumer<ProjectDelivery>,
    /// 撤去した `ProjectRt` を off-thread で drop させる口 (RT で free しない)。
    pub project_recycle_tx: rtrb::Producer<Box<ProjectRt>>,
    /// Pending preview / launcher commands from the receive loop. Drained at the top
    /// of every `process_buffer`.
    pub cmd_rx: tokio::sync::mpsc::UnboundedReceiver<EngineCommand>,
    /// Resources shared with the export / notify threads.
    pub shared: Arc<EngineShared>,
    /// デバイス全体 snapshot (worker rig / sampler ring) の配送 ring 対。
    pub device_rx: rtrb::Consumer<DeviceBundle>,
    pub device_recycle_tx: rtrb::Producer<DeviceBundle>,
    /// RT が使う worker rig (bundle 由来 Arc clone)。
    pub worker: Option<Arc<WorkerRig>>,
    /// Global Sampler のリング (bundle 由来 Arc clone)。
    pub sampler: Option<Arc<SamplerRig>>,
    /// Global Sampler の RT 走行状態 (セグメント / 試聴)。
    pub sampler_rt: SamplerRt,
    /// stream 開始からの累積 render フレーム数 (MIDI 試聴シーケンスの時計)。
    pub frames_rendered: u64,
}

impl DeviceRt {
    pub fn new(
        max_frames: usize,
        cmd_rx: tokio::sync::mpsc::UnboundedReceiver<EngineCommand>,
        shared: Arc<EngineShared>,
        project_rx: rtrb::Consumer<ProjectDelivery>,
        project_recycle_tx: rtrb::Producer<Box<ProjectRt>>,
        device_rx: rtrb::Consumer<DeviceBundle>,
        device_recycle_tx: rtrb::Producer<DeviceBundle>,
    ) -> Self {
        Self {
            master_l: vec![0.0; max_frames],
            master_r: vec![0.0; max_frames],
            projects: Vec::with_capacity(MAX_PROJECTS),
            project_rx,
            project_recycle_tx,
            cmd_rx,
            shared,
            device_rx,
            device_recycle_tx,
            worker: None,
            sampler: None,
            sampler_rt: SamplerRt::new(),
            frames_rendered: 0,
        }
    }

    /// 「どれか 1 つでも走っている (count-in 込み)」 — idle park 判定用。
    #[must_use]
    pub fn any_rolling(&self) -> bool {
        self.projects.iter().any(|p| p.is_rolling())
    }

    /// 「どれか 1 つでも再生中」 — xrun 計数用 (停止中の cold start を除外する)。
    #[must_use]
    pub fn any_playing(&self) -> bool {
        self.projects.iter().any(|p| p.playing)
    }

    fn project_mut(&mut self, key: ProjectKey) -> Option<&mut ProjectRt> {
        self.projects.iter_mut().find(|p| p.key == key).map(|b| &mut **b)
    }

    /// project slot の生成 / 撤去便を取り込む。`Open` は容量内 push (再確保なし)、
    /// `Close` は `swap_remove` して recycle ring へ (drop は off-thread)。
    fn refresh_projects(&mut self) {
        while let Ok(delivery) = self.project_rx.pop() {
            match delivery {
                ProjectDelivery::Open(rt) => {
                    if self.projects.len() < self.projects.capacity() {
                        self.projects.push(rt);
                    } else {
                        // 容量超過 (recv loop が cap を守るので届かない)。捨てるのも
                        // off-thread — recycle へ返す。
                        let _ = self.project_recycle_tx.push(rt);
                    }
                }
                ProjectDelivery::Close(key) => {
                    // 閉じたタブが握っていた試聴の走行状態を降ろす
                    // (`docs/plan_project_tabs.md` §5.5)。
                    self.sampler_rt.forget_project(key);
                    if let Some(i) = self.projects.iter().position(|p| p.key == key) {
                        let rt = self.projects.swap_remove(i);
                        let _ = self.project_recycle_tx.push(rt);
                    }
                }
            }
        }
    }

    /// デバイス全体 snapshot (worker / sampler) の install。最新だけ残し、
    /// 旧値は recycle ring で off-thread drop。
    fn refresh_device_bundle(&mut self) {
        let mut newest: Option<DeviceBundle> = None;
        while let Ok(bundle) = self.device_rx.pop() {
            if let Some(skipped) = newest.take() {
                let _ = self.device_recycle_tx.push(skipped);
            }
            newest = Some(bundle);
        }
        let Some(new) = newest else { return };
        let old_worker = std::mem::replace(&mut self.worker, new.worker);
        // Global Sampler: 世代が変わった (別 Arc) ら走行状態を捨てる。旧リングは
        // recycle bundle で off-thread unmap。
        let sampler_changed = !match (&self.sampler, &new.sampler) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        let old_sampler = std::mem::replace(&mut self.sampler, new.sampler);
        if sampler_changed {
            self.sampler_rt.reset();
        }
        let _ = self.device_recycle_tx.push(DeviceBundle {
            worker: old_worker,
            sampler: old_sampler,
        });
    }

    /// Drain pending commands. Called at the top of `process_buffer`.
    fn pump_commands(&mut self) {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            match cmd {
                // r.md #87: ランチャーの操作。発火拍の解決は buffer 頭の
                // `launcher.update` (= song snapshot が入ってから) が行う。
                EngineCommand::Launch { project, req } => {
                    if let Some(p) = self.project_mut(project) {
                        p.launcher.push_request(req);
                    }
                }
                EngineCommand::SamplerPreview { start, end } => {
                    self.sampler_rt.set_preview(start, end);
                }
                EngineCommand::SamplerPreviewStop => self.sampler_rt.stop_preview(),
                EngineCommand::PreviewNoteOn {
                    project,
                    track,
                    pitch,
                    velocity,
                } => {
                    // 鍵盤プレビュー: 該当 track の pending_preview に積む。
                    // process_track_owned が次の dispatch で frame 0 に注入する。
                    if let Some(p) = self.project_mut(project) {
                        p.push_preview(
                            track,
                            NoteTransition::On {
                                note_id: PREVIEW_NOTE_ID,
                                key: pitch,
                                velocity,
                            },
                        );
                    }
                }
                EngineCommand::PreviewNoteOff { project, track, pitch } => {
                    if let Some(p) = self.project_mut(project) {
                        p.push_preview(
                            track,
                            NoteTransition::Off {
                                note_id: PREVIEW_NOTE_ID,
                                key: pitch,
                            },
                        );
                    }
                }
            }
        }
    }

    /// Render `frames` of device output into `master_l/r` = 全 project の bus の加算
    /// (`docs/plan_project_tabs.md` §3.2)。
    pub fn process_buffer(
        &mut self,
        shared: &SharedState,
        bridge: &AudioBridgeHandle,
        scope: &common::scope_bridge::ScopeBridgeHandle,
        sample_rate: u32,
        frames: usize,
    ) {
        let _ = shared;
        self.refresh_projects();
        self.refresh_device_bundle();
        self.pump_commands();

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

        let export_running = self.shared.export_running.load(Ordering::Acquire);
        let scope_project = self.shared.scope_project();
        // Global Sampler の録音源が縛られている project: `Master` はアクティブなタブ
        // (= scope と同じ)、`Track` はその tap の project。
        let sampler_project = match self.sampler.as_deref().map(|r| r.source) {
            Some(SamplerSource::Track { project, .. }) => project,
            Some(SamplerSource::Master) | None => scope_project,
        };

        // r.md #51: transport の要求 (Play / Stop) と seek の消費、および
        // 「今どういう状態か」の publish は **どの早期 return よりも先に、全 project
        // について**行う。
        //
        // 旧実装は count-in と書き出しの早期 return より後ろに置いていたため、
        // その間に届いた Stop が永久に消費されず、
        // - count-in を取り消したのに preroll が 0 になった瞬間に曲が鳴り出す
        // - 書き出しが終わった瞬間に (誰も押していないのに) 再生が再開する
        // という 2 つの取りこぼしが起きていた。要求は届いた順に必ず消費する。
        for p in &mut self.projects {
            p.begin_buffer(bridge.project(p.telemetry_slot));
        }

        // freewheel export: while the export thread holds the audio
        // resources, write silence and skip dispatch so the worker pool
        // and plugin instances are exclusively driven by the export
        // render loop. Publish `live_parked` so the export thread knows the
        // live callback has stopped dispatching before it starts its own (no
        // collision on the shared plugin-host worker slots). **全 project が止まる**。
        if export_running {
            self.shared.live_parked.store(true, Ordering::Release);
            for p in &mut self.projects {
                // r.md #87: 書き出し中に届いたローンチ操作は捨てる。溜めておくと
                // 書き出し明けに **全部同時に**発火し、64 件で溢れた分は無言で消える。
                p.launcher.clear_requests();
                // 停止中も現在の playhead をそのまま publish する。
                bridge
                    .project(p.telemetry_slot)
                    .set_playhead_samples(p.shared.playhead.load(Ordering::Acquire));
            }
            return;
        }
        self.shared.live_parked.store(false, Ordering::Release);

        let frames_rendered = self.frames_rendered;
        for p in &mut self.projects {
            let is_scope = p.key == scope_project;
            let is_sampler = p.key == sampler_project;
            let ctx = DeviceCtx {
                worker: self.worker.as_deref(),
                sampler: if is_sampler { self.sampler.as_deref() } else { None },
                sampler_rt: &mut self.sampler_rt,
                scope: is_scope.then_some(scope),
                frames_rendered,
            };
            let slot = bridge.project(p.telemetry_slot);
            p.render_buffer(ctx, slot, sample_rate, n);
            // A2: publish the engine's playhead to shmem so the GUI
            // can draw the cursor. 停止中も現在の playhead をそのまま
            // publish する (= ruler click で動かした位置や、 Stop 直前
            // の位置を GUI に反映、 業界標準の挙動)。
            slot.set_playhead_samples(p.shared.playhead.load(Ordering::Acquire));
            // デバイス最終ミックスへ加算 (1 project なら加算 1 回 = 従来と同じコスト)。
            for i in 0..n {
                self.master_l[i] += p.bus_l[i];
                self.master_r[i] += p.bus_r[i];
            }
        }

        self.frames_rendered = self.frames_rendered.wrapping_add(n as u64);
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

    // `Track { id, ..Default::default() }` は E0451 (private field) で書けない。
    // clippy はそれを知らないので、ここだけ抑制する。
    #[allow(clippy::field_reassign_with_default)]
    fn track(id: u32) -> Track {
        // Track の legacy migration fields は common に pub(crate) で閉じて
        // いるため、 default + mutate で構築する (E0451 回避)。
        let mut t = Track::default();
        t.id = id;
        t
    }

    /// PDC 用テスト fixture: 「latency 4 を報告する device」 を 1 個持つ track。
    /// 報告値は `Song` ではなく `DeviceLatencies` 側に居る (r.md #9) ので、
    /// track には device を、 表には (device_id → samples) を置く。
    const LATENT_DEVICE_ID: u64 = 20;
    const LATENT_SAMPLES: u32 = 4;

    fn latent_track(id: u32) -> Track {
        let mut t = track(id);
        t.devices = vec![common::model::Device::Plugin(common::model::PluginInstance {
            id: LATENT_DEVICE_ID,
            ..common::model::PluginInstance::with_ports(
                "test.latent".into(),
                common::plugin_format::PluginFormat::Clap,
                common::port_config::PortConfig {
                    has_note_input: false,
                    has_note_output: false,
                    has_audio_output: true,
                    has_audio_input: true,
                    has_video_input: false,
                    has_video_output: false,
                },
            )
        })];
        t
    }

    /// 全 bundle 共通の報告 latency 表。 `latent_track` が載せる device しか
    /// 持たないので、 その device が居ない song には何の影響も無い。
    fn test_latencies() -> crate::graph::DeviceLatencies {
        let mut lat = crate::graph::DeviceLatencies::new();
        lat.insert(LATENT_DEVICE_ID, LATENT_SAMPLES);
        lat
    }

    fn make_bundle(song: &Arc<Song>) -> RtBundle {
        make_bundle_with_reset(song, false)
    }

    /// `reset_song_scoped_state` を明示する版 (project 切替相当)。
    fn make_bundle_with_reset(song: &Arc<Song>, reset: bool) -> RtBundle {
        RtBundle {
            song: Some(Arc::clone(song)),
            tempo_map: common::tempo_map::TempoMap::from_song(song),
            schedule: Some(compile_schedule(song, &test_latencies(), 48_000, 0).unwrap()),
            reset_song_scoped_state: reset,
            input_delay_replacements: Vec::new(),
            // 本番 (`project_ctl::publish_bundle`) と同じで、song と同じ便で
            // その曲が要る本数の scratch を運ぶ。
            scratch_growth: Some(
                (0..song.tracks.len().min(MAX_TRACKS)).map(|_| TrackScratch::new()).collect(),
            ),
            plugin_refs: Arc::new(HashMap::new()),
            preview_sequence: None,
            mod_plan: None,
            mod_phase_table: None,
        }
    }

    /// 値のみ更新 (schedule 無し) の bundle。
    fn value_only(song: &Arc<Song>) -> RtBundle {
        RtBundle {
            song: Some(Arc::clone(song)),
            tempo_map: common::tempo_map::TempoMap::from_song(song),
            schedule: None,
            reset_song_scoped_state: false,
            input_delay_replacements: Vec::new(),
            scratch_growth: None,
            plugin_refs: Arc::new(HashMap::new()),
            preview_sequence: None,
            mod_plan: None,
            mod_phase_table: None,
        }
    }

    /// A `ProjectRt` plus the off-thread ends of the forward + recycle rings,
    /// so a test can publish bundles and inspect what got recycled.
    fn harness() -> (
        ProjectRt,
        rtrb::Producer<RtBundle>,
        rtrb::Consumer<RtBundle>,
    ) {
        let shared = Arc::new(ProjectShared::new(ProjectKey(1), 0));
        let (bundle_tx, bundle_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, recycle_rx) = rtrb::RingBuffer::new(8);
        let (pool_tx, pool_rx) = rtrb::RingBuffer::<StretchPoolDelivery>::new(4);
        let (pool_recycle_tx, pool_recycle_rx) = rtrb::RingBuffer::<StretchPoolDelivery>::new(4);
        drop(pool_tx);
        drop(pool_recycle_rx);
        let local = ProjectRt::new(
            common::process_data::MAX_FRAMES,
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
        bundle_tx.push(value_only(&s2)).unwrap();
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
        bundle_tx.push(value_only(&opened)).unwrap();
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
        s1.tracks.push(latent_track(2));
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
        s1.tracks.push(latent_track(2));
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
        s1.tracks.push(latent_track(2));
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
        bundle_tx.push(value_only(&s1)).unwrap();
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
        bundle_tx.push(value_only(&s2)).unwrap();

        // Steady-state install: pop the newest, adopt + swap the cached
        // fields, push the old to the recycle ring — all wait-free, no alloc,
        // no free.
        assert_no_alloc::assert_no_alloc(|| {
            local.refresh_bundle();
        });
    }
}

/// `docs/plan_project_tabs.md` §3.5: 複数 project の transport 独立性とミックス、
/// slot の生成 / 撤去。
#[cfg(test)]
mod multi_project_tests {
    use super::*;
    use common::model::{Song, Track};

    #[allow(clippy::field_reassign_with_default)]
    fn track(id: u32) -> Track {
        let mut t = Track::default();
        t.id = id;
        t
    }

    struct Rig {
        dev: DeviceRt,
        project_tx: rtrb::Producer<ProjectDelivery>,
        project_recycle_rx: rtrb::Consumer<Box<ProjectRt>>,
        _device_tx: rtrb::Producer<DeviceBundle>,
        _device_recycle_rx: rtrb::Consumer<DeviceBundle>,
        bridge: AudioBridgeHandle,
        scope: common::scope_bridge::ScopeBridgeHandle,
        shared: Arc<SharedState>,
        engine: Arc<EngineShared>,
    }

    fn rig(tag: &str) -> Rig {
        let engine = Arc::new(EngineShared::new());
        let (_cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (project_tx, project_rx) = rtrb::RingBuffer::new(8);
        let (project_recycle_tx, project_recycle_rx) = rtrb::RingBuffer::new(8);
        let (device_tx, device_rx) = rtrb::RingBuffer::new(4);
        let (device_recycle_tx, device_recycle_rx) = rtrb::RingBuffer::new(4);
        let dev = DeviceRt::new(
            common::process_data::MAX_FRAMES,
            cmd_rx,
            Arc::clone(&engine),
            project_rx,
            project_recycle_tx,
            device_rx,
            device_recycle_tx,
        );
        let pid = std::process::id();
        let bridge = AudioBridgeHandle::create(&format!("daw01_test_mp_{tag}_{pid}")).unwrap();
        let scope = common::scope_bridge::ScopeBridgeHandle::create(&format!(
            "daw01_test_mp_scope_{tag}_{pid}"
        ))
        .unwrap();
        Rig {
            dev,
            project_tx,
            project_recycle_rx,
            _device_tx: device_tx,
            _device_recycle_rx: device_recycle_rx,
            bridge,
            scope,
            shared: Arc::new(SharedState::new()),
            engine,
        }
    }

    /// off-thread 側と同じ手順で project slot を開く (recv loop の `OpenProject`)。
    fn open(r: &mut Rig, key: ProjectKey) -> (Arc<ProjectShared>, rtrb::Producer<RtBundle>) {
        let slot = r.bridge.claim_project_slot(key).unwrap();
        let (shared, pool_rx, pool_recycle_tx) = ProjectShared::new_with_stretch_rings(key, slot);
        let shared = Arc::new(shared);
        let (bundle_tx, bundle_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, _recycle_rx) = rtrb::RingBuffer::new(8);
        std::mem::forget(_recycle_rx);
        let rt = ProjectRt::new(
            common::process_data::MAX_FRAMES,
            Arc::clone(&shared),
            bundle_rx,
            recycle_tx,
            pool_rx,
            pool_recycle_tx,
        );
        r.project_tx.push(ProjectDelivery::Open(Box::new(rt))).ok().unwrap();
        let mut map = (**r.engine.projects.load()).clone();
        map.insert(key, Arc::clone(&shared));
        r.engine.projects.store(Arc::new(map));
        (shared, bundle_tx)
    }

    fn song_with_tracks(n: u32) -> Arc<Song> {
        let mut s = Song::default();
        for i in 1..=n {
            s.tracks.push(track(i));
        }
        Arc::new(s)
    }

    fn bundle(song: &Arc<Song>) -> RtBundle {
        RtBundle {
            song: Some(Arc::clone(song)),
            tempo_map: common::tempo_map::TempoMap::from_song(song),
            schedule: Some(
                crate::graph::compile_schedule(song, &HashMap::new(), 48_000, 256).unwrap(),
            ),
            reset_song_scoped_state: false,
            input_delay_replacements: Vec::new(),
            // 本番 (`project_ctl::publish_bundle`) と同じく song と同じ便で運ぶ。
            scratch_growth: Some(
                (0..song.tracks.len().min(MAX_TRACKS)).map(|_| TrackScratch::new()).collect(),
            ),
            plugin_refs: Arc::new(HashMap::new()),
            preview_sequence: None,
            mod_plan: None,
            mod_phase_table: None,
        }
    }

    fn run(r: &mut Rig, buffers: usize) {
        for _ in 0..buffers {
            r.dev.process_buffer(&r.shared, &r.bridge, &r.scope, 48_000, 256);
        }
    }

    #[test]
    fn 片方だけ_play_するともう片方の_playhead_は動かない() {
        let mut r = rig("transport");
        let (a, mut a_tx) = open(&mut r, ProjectKey(1));
        let (b, mut b_tx) = open(&mut r, ProjectKey(2));
        a_tx.push(bundle(&song_with_tracks(1))).ok().unwrap();
        b_tx.push(bundle(&song_with_tracks(1))).ok().unwrap();
        run(&mut r, 1);
        assert_eq!(r.dev.projects.len(), 2, "2 slot とも RT に届く");

        a.playback.store(PlaybackCommand::Play as u8, Ordering::Release);
        run(&mut r, 4);
        assert_eq!(a.playhead.load(Ordering::Acquire), 256 * 4, "A は進む");
        assert_eq!(b.playhead.load(Ordering::Acquire), 0, "B は止まったまま");
        assert!(r.bridge.find_project(ProjectKey(1)).unwrap().playing());
        assert!(!r.bridge.find_project(ProjectKey(2)).unwrap().playing());
        assert!(r.dev.any_playing());

        // B も再生 → 両方独立に進む。A を止めても B は続く。
        b.playback.store(PlaybackCommand::Play as u8, Ordering::Release);
        run(&mut r, 2);
        a.playback.store(PlaybackCommand::Stop as u8, Ordering::Release);
        run(&mut r, 3);
        assert_eq!(a.playhead.load(Ordering::Acquire), 256 * 6);
        assert_eq!(b.playhead.load(Ordering::Acquire), 256 * 5);
        assert!(r.dev.any_playing());
    }

    /// 計画書 §3.5: **出力はミックス**。1 デバイスの master は開いている全 project の
    /// 和で、タブを増やしても片方が置き換わったり消えたりしない。
    ///
    /// 音源にはメトロノーム (曲が空でも必ず鳴る monitoring の音) を使い、同じ曲・同じ
    /// 位置で鳴らした 1 project の出力と比べる。
    #[test]
    fn 開いている全_project_の音がデバイスの_master_で合算される() {
        // 参照: 1 project だけの rig。
        let mut solo = rig("mix_solo");
        let (c, mut c_tx) = open(&mut solo, ProjectKey(1));
        c_tx.push(bundle(&song_with_tracks(1))).ok().unwrap();
        run(&mut solo, 1);
        c.metronome_enabled.store(true, Ordering::Release);
        c.playback.store(PlaybackCommand::Play as u8, Ordering::Release);
        run(&mut solo, 1);
        let one: Vec<f32> = solo.dev.master_l[..256].to_vec();
        assert!(one.iter().any(|v| v.abs() > 0.0), "1 project でも master に音が出る");

        // 同じ曲・同じ位置を 2 project で鳴らす。
        let mut r = rig("mix_two");
        let (a, mut a_tx) = open(&mut r, ProjectKey(1));
        let (b, mut b_tx) = open(&mut r, ProjectKey(2));
        a_tx.push(bundle(&song_with_tracks(1))).ok().unwrap();
        b_tx.push(bundle(&song_with_tracks(1))).ok().unwrap();
        run(&mut r, 1);
        for p in [&a, &b] {
            p.metronome_enabled.store(true, Ordering::Release);
            p.playback.store(PlaybackCommand::Play as u8, Ordering::Release);
        }
        run(&mut r, 1);
        for (i, (&two, &one)) in r.dev.master_l[..256].iter().zip(one.iter()).enumerate() {
            assert!((two - one * 2.0).abs() < 1e-6, "frame {i}: {two} != {one} * 2");
        }
    }

    #[test]
    fn close_した_project_は_rt_から外れて_off_thread_へ戻る() {
        let mut r = rig("close");
        let (_a, _a_tx) = open(&mut r, ProjectKey(1));
        let (_b, _b_tx) = open(&mut r, ProjectKey(2));
        run(&mut r, 1);
        assert_eq!(r.dev.projects.len(), 2);
        r.project_tx.push(ProjectDelivery::Close(ProjectKey(1))).ok().unwrap();
        run(&mut r, 1);
        assert_eq!(r.dev.projects.len(), 1);
        assert_eq!(r.dev.projects[0].key, ProjectKey(2));
        let recycled = r.project_recycle_rx.pop().expect("撤去した ProjectRt が戻る");
        assert_eq!(recycled.key, ProjectKey(1));
        // 未知の key への Close は無視される。
        r.project_tx.push(ProjectDelivery::Close(ProjectKey(9))).ok().unwrap();
        run(&mut r, 1);
        assert_eq!(r.dev.projects.len(), 1);
    }

    #[test]
    fn 書き出し中は全_project_の_playhead_が止まり_解除で続く() {
        let mut r = rig("export");
        let (a, mut a_tx) = open(&mut r, ProjectKey(1));
        a_tx.push(bundle(&song_with_tracks(1))).ok().unwrap();
        run(&mut r, 1);
        a.playback.store(PlaybackCommand::Play as u8, Ordering::Release);
        run(&mut r, 2);
        assert_eq!(a.playhead.load(Ordering::Acquire), 512);
        r.engine.export_running.store(true, Ordering::Release);
        run(&mut r, 3);
        assert_eq!(a.playhead.load(Ordering::Acquire), 512, "書き出し中は進まない");
        assert!(r.engine.live_parked.load(Ordering::Acquire));
        r.engine.export_running.store(false, Ordering::Release);
        run(&mut r, 2);
        assert_eq!(a.playhead.load(Ordering::Acquire), 1024, "続きから進む");
    }
}
