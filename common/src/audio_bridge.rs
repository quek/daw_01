use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use anyhow::Result;

use crate::protocol::ProjectKey;
use crate::shmem::NamedShmem;

pub mod plane;

pub use plane::{PlaneCapacity, TelemetryPlane, plane_id, plane_shmem_id};

/// (A1 r.md #8) フォールバック既定サンプルレート。 通常はランタイムで
/// `AudioSession.sample_rate` = デバイス実レート (daw_audio が Hello で報告) が
/// SSoT で、 この const はデバイス query 失敗時の保険値としてのみ使う。
pub const DEFAULT_SAMPLE_RATE: u32 = 48000;
/// 1 バッファの最大 frame 数。 SSoT は `process_data::MAX_FRAMES` (プラグイン
/// process shmem のバッファ次元) — audio bridge 側の u32 view として re-export。
/// 二重定義で乖離すると RT パスの `assert!(frames <= MAX_FRAMES)` が panic する。
pub const MAX_FRAMES: u32 = crate::process_data::MAX_FRAMES as u32;
pub const CHANNELS: u32 = 2;
/// `docs/plan_project_tabs.md` §0: 同時に開けるプロジェクト (= タブ) の上限。
/// engine の per-project RT 状態は開いている数だけ確保するが、telemetry 面
/// ([`AudioBridge::projects`]) は shmem なのでここで固定する。
pub const MAX_PROJECTS: usize = 32;

/// r.md #117: track ごとに GUI へ見せるボイス数の上限 (表示用。 engine のボイス表
/// `MAX_VOICES` より小さく、 溢れたぶんは見せない)。
pub const MAX_PUBLISHED_VOICES: usize = 16;

/// ランチャー行の走行状態の値 ([`LauncherRowSnapshot::state`])。**engine と GUI が共有する唯一の定義**。
pub const LAUNCHER_STATE_ARRANGER: u32 = 0;
/// ランチャーが握っていてセルが鳴っている。
pub const LAUNCHER_STATE_PLAYING: u32 = 1;
/// ランチャーが握っているが無音 (Stop Clips)。
pub const LAUNCHER_STATE_STOPPED: u32 = 2;

/// [`LauncherRowSnapshot::queued_clip_id`] が「停止の予約」を表す値。
/// `clip.id` は 1 から採番されるので実 id と衝突しない。
pub const LAUNCHER_QUEUED_STOP: u32 = u32::MAX;
/// [`LauncherRowSnapshot::queued_clip_id`] が「アレンジへ返す予約」を表す値。
pub const LAUNCHER_QUEUED_ARRANGER: u32 = u32::MAX - 1;

/// [`ProjectTelemetry::playhead_samples`] の「まだ再生位置が無い」sentinel
/// (slot を claim した直後 / まだ 1 buffer も回っていない)。
pub const PLAYHEAD_UNSET: u64 = u64::MAX;

/// 1 プロジェクト (= タブ) ぶんの **固定の** telemetry 面: daw_audio (writer) → daw_gui (30Hz
/// polling reader)。`docs/plan_project_tabs.md` §2.5。
///
/// ここに置くのは **数が曲で決まらない値** だけ (再生位置 / transport / master Limiter の GR)。
/// トラックのピーク / 鳴っているボイス / 内蔵 device の GR / 変調値 / ランチャー行は数が曲で
/// 決まるので、容量を伸ばせる [`TelemetryPlane`] に置き、ここは今の面の id ([`Self::plane_id`])
/// だけを持つ (`docs/plan_unbounded_tracks.md` §3)。
///
/// マスター出力のメーターはここに**居ない** (r.md #50)。マスターのピーク / VU / ラウドネス /
/// スペクトラム等はすべて daw_gui 側の `MasterAnalyzer` が `scope_bridge` のサンプルリングから
/// 導くので、値を shmem に複製しない (SSoT)。
///
/// All fields are lock-free Acquire/Release atomics — readers tolerate any
/// value they happen to observe.
#[repr(C)]
pub struct ProjectTelemetry {
    /// この slot がどのプロジェクトの面か (`ProjectKey.0`)。**`0` = 空きスロット。**
    /// writer (daw_audio) が `OpenProject` で CAS claim し、`CloseProject` で 0 に戻す。
    /// 読み手は index ではなくこの値で自分の slot を引く (アーキ不変条件 1)。
    pub project_key: AtomicU64,
    /// `playhead_samples` is published by **daw_audio** at the end of every
    /// buffer so daw_gui can poll it (once per UI tick) for playhead-row
    /// highlighting.
    pub playhead_samples: AtomicU64,
    /// 今 audio thread が書いている [`TelemetryPlane`] の id ([`plane_id`])。**`0` = 面なし**。
    /// RT が新しい面へ一式 publish し終えた buffer の末尾で差し替えるので、読み手が見つけた id の面は
    /// 書かれた後の面。
    pub plane_id: AtomicU64,
    /// master のフェーダー後 Limiter の GR (dB、0 以下、`f32::to_bits`)。
    pub master_limiter_gr_db: AtomicU32,
    /// r.md #51: engine が今 transport を回しているか (0/1)。
    ///
    /// **「再生中か」の唯一の所有者は engine** で、GUI はこれを観測して
    /// `transport.is_playing` に写すだけ。GUI 側で「Play を送った記憶」を持つと、
    /// engine が自分で止まったとき (曲末 auto-stop / 書き出し) に食い違う。
    pub playing: AtomicU32,
    /// Phase 7 B4 Step C (2026-05-13): count-in 残り samples mirror (audio
    /// thread が `process_buffer` で書く、 GUI が on_tick で poll)。
    /// 0 = count-in 中ではない / 完了済。 `StartRecording` 受信時に audio
    /// thread が値を立てる。 **これ単体で「count-in が終わったか」を判定しては
    /// いけない** — 0 は「まだ始まっていない」も意味するので、録音実体の開始判定は
    /// [`Self::recording_live`] を見る (r.md #51)。
    pub preroll_remaining_samples: AtomicU64,
    /// r.md #51: 今この瞬間 MIDI ノートを記録してよいか (0/1)。
    ///
    /// `録音要求あり && 再生中 && count-in 完了` を engine が判定して publish する。
    /// GUI が preroll ミラーの 0 を見て自前で導出すると、`StartRecording` 送信直後に
    /// 届いた stale な Tick (まだ preroll が立つ前の 0) で count-in を丸ごと飛ばす。
    pub recording_live: AtomicU32,
}

/// Shared memory telemetry plane: `MAX_PROJECTS` 個の [`ProjectTelemetry`]。
/// daw_gui (親) が bootstrap で **プロセス生存中 1 度だけ** create する
/// (`plugin_ref` の命名契約)。
///
/// v29 (`docs/plan_arch_refactor.md` §2): 旧 `frames_requested` / `samples`
/// 面 (M0 時代の request/ready セマフォ往復データプレーン) は writer /
/// reader とも存在しない死んだ protocol だったため削除。音声データは
/// per-plugin の `ProcessData` shmem + `WorkerBridge` dispatch が運ぶ。
#[repr(C)]
pub struct AudioBridge {
    pub projects: [ProjectTelemetry; MAX_PROJECTS],
}

impl AudioBridge {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

/// r.md #117: GUI が読む 1 ボイスぶんの起点 (拍 / 秒) と note-off の秒。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoiceSnapshot {
    pub on_beat: f64,
    pub on_secs: f64,
    /// `None` = まだ押している。
    pub off_secs: Option<f64>,
}

/// r.md #87: [`TelemetryPlane::launcher_row`] が返す 1 行ぶんの走行状態 (表示専用)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LauncherRowSnapshot {
    /// `LAUNCHER_STATE_*`。
    pub state: u32,
    pub playing_clip_id: u32,
    pub queued_clip_id: u32,
    /// 予約が発火する song 拍 (`queued_clip_id == 0` のときは `0.0`)。
    pub queued_at_beat: f64,
    /// セル内の進捗 `0..1`。
    pub progress: f32,
    /// 鳴っているセルを撃った song 拍 (停止中は `0.0`)。
    pub launch_beat: f64,
}

impl ProjectTelemetry {
    /// この slot のプロジェクト (`ProjectKey::NONE` = 空き)。
    #[must_use]
    pub fn key(&self) -> ProjectKey {
        ProjectKey(self.project_key.load(Ordering::Acquire))
    }

    /// 面を「何も鳴っていない」状態に戻す (claim の直前 / 解放の直後)。前の
    /// プロジェクトの値が新しいプロジェクトの初回 tick に見えないようにする。
    /// off-RT (recv loop) で呼ぶ。`project_key` は触らない。
    fn reset(&self) {
        self.playhead_samples.store(PLAYHEAD_UNSET, Ordering::Release);
        self.plane_id.store(0, Ordering::Release);
        self.set_master_limiter_gr_db(0.0);
        self.preroll_remaining_samples.store(0, Ordering::Release);
        self.playing.store(0, Ordering::Release);
        self.recording_live.store(0, Ordering::Release);
    }

    pub fn set_playhead_samples(&self, n: u64) {
        self.playhead_samples.store(n, Ordering::Release);
    }

    pub fn playhead_samples(&self) -> u64 {
        self.playhead_samples.load(Ordering::Acquire)
    }

    /// audio thread が新しい [`TelemetryPlane`] へ一式 publish し終えたら呼ぶ (RT 安全: atomic store のみ)。
    pub fn set_plane_id(&self, id: u64) {
        self.plane_id.store(id, Ordering::Release);
    }

    /// 今の [`TelemetryPlane`] の id (`0` = 面なし)。読み手は変わったら開き直す。
    #[must_use]
    pub fn plane_id(&self) -> u64 {
        self.plane_id.load(Ordering::Acquire)
    }

    /// master のフェーダー後 Limiter の GR を publish する (dB、0 以下)。audio thread が毎 buffer 呼ぶ。
    pub fn set_master_limiter_gr_db(&self, db: f32) {
        self.master_limiter_gr_db.store(db.to_bits(), Ordering::Release);
    }

    /// master Limiter の GR を読む (GUI の UI tick)。
    #[must_use]
    pub fn master_limiter_gr_db(&self) -> f32 {
        f32::from_bits(self.master_limiter_gr_db.load(Ordering::Acquire))
    }

    /// Phase 7 B4 Step C: count-in 残り samples を audio thread が更新。
    /// `StartRecording` 受信時に audio thread が preroll を立て、
    /// `process_buffer` が preroll > 0 ループ内で毎 buffer 更新する。
    /// 0 到達で通常再生に戻る。
    pub fn set_preroll_remaining(&self, n: u64) {
        self.preroll_remaining_samples.store(n, Ordering::Release);
    }

    pub fn preroll_remaining(&self) -> u64 {
        self.preroll_remaining_samples.load(Ordering::Acquire)
    }

    /// r.md #51: engine の transport 走行状態を publish する (audio thread が
    /// 毎 buffer 呼ぶ)。読み手は GUI の playhead poller。
    pub fn set_playing(&self, playing: bool) {
        self.playing.store(u32::from(playing), Ordering::Release);
    }

    pub fn playing(&self) -> bool {
        self.playing.load(Ordering::Acquire) != 0
    }

    /// r.md #51: 「今ノートを記録してよいか」を publish する (audio thread が
    /// 毎 buffer 呼ぶ)。count-in 明けの立ち上がりもここが唯一の合図。
    pub fn set_recording_live(&self, live: bool) {
        self.recording_live.store(u32::from(live), Ordering::Release);
    }

    pub fn recording_live(&self) -> bool {
        self.recording_live.load(Ordering::Acquire) != 0
    }
}

/// Owning handle to the audio shared memory region.
pub struct AudioBridgeHandle {
    shmem: NamedShmem,
    /// この region の os_id。伸びる面の名前 ([`plane_shmem_id`]) の起点。
    os_id: String,
}

impl AudioBridgeHandle {
    pub fn create(os_id: &str) -> Result<Self> {
        let shmem = NamedShmem::create(os_id, AudioBridge::SIZE)?;
        // Zero-initialize so every slot starts free (`project_key == 0`) and silent.
        unsafe { std::ptr::write_bytes(shmem.as_ptr(), 0, AudioBridge::SIZE) };
        let handle = Self { shmem, os_id: os_id.to_owned() };
        for slot in &handle.bridge().projects {
            slot.reset();
        }
        Ok(handle)
    }

    pub fn open(os_id: &str) -> Result<Self> {
        let shmem = NamedShmem::open(os_id, AudioBridge::SIZE)?;
        Ok(Self { shmem, os_id: os_id.to_owned() })
    }

    /// この region の os_id ([`TelemetryPlane`] の名前の起点)。
    #[must_use]
    pub fn os_id(&self) -> &str {
        &self.os_id
    }

    fn ptr(&self) -> *mut AudioBridge {
        self.shmem.as_ptr() as *mut AudioBridge
    }

    pub fn bridge(&self) -> &AudioBridge {
        unsafe { &*self.ptr() }
    }

    /// writer (daw_audio, off-RT): `key` 用の空き slot を claim する。面を reset して
    /// から `project_key` を **最後に** publish するので、読み手が key で見つけた時点で
    /// 中身は「何も鳴っていない」状態。満杯なら `None`。同じ key を 2 度 claim すると
    /// 既存 slot を返す (respawn 後の再構築で冪等)。
    pub fn claim_project_slot(&self, key: ProjectKey) -> Option<usize> {
        debug_assert!(key.is_some(), "ProjectKey::NONE は空きスロットの印なので claim 不可");
        let slots = &self.bridge().projects;
        if let Some(i) = slots.iter().position(|s| s.key() == key) {
            return Some(i);
        }
        for (i, s) in slots.iter().enumerate() {
            if s.project_key.load(Ordering::Acquire) != 0 {
                continue;
            }
            s.reset();
            if s.project_key
                .compare_exchange(0, key.0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(i);
            }
        }
        None
    }

    /// writer: slot を空きに戻す (`CloseProject`)。面も reset するので、読み手が
    /// 1 tick 遅れて覗いても前のプロジェクトの値は見えない。
    pub fn release_project_slot(&self, slot: usize) {
        if let Some(s) = self.bridge().projects.get(slot) {
            s.project_key.store(0, Ordering::Release);
            s.reset();
        }
    }

    /// slot index で引く (writer 側 — `ProjectRt` が claim 時の index を持つ)。
    #[must_use]
    pub fn project(&self, slot: usize) -> &ProjectTelemetry {
        &self.bridge().projects[slot]
    }

    /// reader (daw_gui): `key` の面を線形走査で引く (≤ `MAX_PROJECTS`、tick ごと)。
    /// まだ claim されていない (OpenProject が届く前) / 解放済みなら `None`。
    #[must_use]
    pub fn find_project(&self, key: ProjectKey) -> Option<&ProjectTelemetry> {
        self.bridge().projects.iter().find(|s| s.key() == key)
    }

    /// reader: 使用中の slot を `(key, 面)` で列挙する (タブの ▶ 表示用)。
    pub fn live_projects(&self) -> impl Iterator<Item = (ProjectKey, &ProjectTelemetry)> {
        self.bridge()
            .projects
            .iter()
            .filter_map(|s| {
                let k = s.key();
                k.is_some().then_some((k, s))
            })
    }
}

// The underlying shared memory is safe to share across threads; every
// field is a lock-free atomic and readers tolerate any observed value.
unsafe impl Send for AudioBridgeHandle {}
unsafe impl Sync for AudioBridgeHandle {}

pub fn shmem_id(parent_pid: u32) -> String {
    format!("daw_01_audio_{parent_pid}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `docs/plan_project_tabs.md` §2.5: slot は key で claim / release し、読み手は
    /// index ではなく key で引く。解放した slot は再利用でき、前の値は残らない。
    #[test]
    fn project_slot_は_key_で往復し解放後は前の値が残らない() {
        let name = format!("daw01_test_slots_{}", std::process::id());
        let h = AudioBridgeHandle::create(&name).expect("bridge");
        assert!(h.find_project(ProjectKey(7)).is_none());
        let a = h.claim_project_slot(ProjectKey(7)).unwrap();
        let b = h.claim_project_slot(ProjectKey(9)).unwrap();
        assert_ne!(a, b);
        assert_eq!(h.claim_project_slot(ProjectKey(7)), Some(a), "同じ key は冪等");
        h.project(a).set_playhead_samples(4800);
        h.project(a).set_playing(true);
        h.project(a).set_plane_id(plane_id(1, 3));
        h.project(a).set_master_limiter_gr_db(-2.5);
        assert_eq!(h.find_project(ProjectKey(7)).unwrap().playhead_samples(), 4800);
        assert_eq!(h.live_projects().count(), 2);

        h.release_project_slot(a);
        assert!(h.find_project(ProjectKey(7)).is_none());
        assert_eq!(h.live_projects().count(), 1);
        let c = h.claim_project_slot(ProjectKey(11)).unwrap();
        assert_eq!(c, a, "空いた slot が再利用される");
        let t = h.find_project(ProjectKey(11)).unwrap();
        assert_eq!(t.playhead_samples(), PLAYHEAD_UNSET, "前の playhead が残らない");
        assert!(!t.playing());
        assert_eq!(t.plane_id(), 0, "前のプロジェクトの面を指さない");
        assert_eq!(t.master_limiter_gr_db(), 0.0);
    }

    #[test]
    fn slot_が満杯なら_claim_は_none() {
        let name = format!("daw01_test_full_{}", std::process::id());
        let h = AudioBridgeHandle::create(&name).expect("bridge");
        for i in 0..MAX_PROJECTS {
            assert!(h.claim_project_slot(ProjectKey(i as u64 + 1)).is_some());
        }
        assert_eq!(h.claim_project_slot(ProjectKey(999)), None);
    }
}
