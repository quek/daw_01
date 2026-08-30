//! ランチャーの走行状態 — 予約 (量子化待ち) / フォローアクション / 行ごとの
//! 供給元の解決 (`docs/plan_rmd_87_clip_launcher.md` §2.2 / §2.3)。
//!
//! **`Song` には何も書き戻さない** (§1.4)。ユーザーが撃った状態
//! ([`RowPlayback`]) は GUI が `Song` に持ち、engine はそれを「再生の起点」として
//! 読むだけ。フォローアクションで移った先は走行状態にしか残らないので、
//! 停止 → 再生も書き出しも必ず同じ起点から始まる = 何度書き出しても同じ音になる。
//!
//! RT 規約: すべて事前確保。`update` の中で確保・ロック・I/O・`format!` を行わない。

use common::model::{
    AutomationLane, FollowAction, LaunchMode, LaunchQuantize, LaunchSettings, RowPlayback,
    SessionAutomationClip, SessionClip, Song, Track,
};

use super::follow::{self, FollowOutcome};
use super::quantize;
use super::{MAX_ROWS, RowKey, RowPhase, RowSourceTable, RowTimeSource, is_positive};

/// グループ判定 ([`common::model::launch_group`]) の作業領域の大きさ =
/// フォローアクションが見る列数の上限。これを超える列にあるセルは
/// 「グループの外」として扱う (鳴らないわけではない)。
const MAX_SCENES: usize = 512;

/// 量子化待ちの予約を積める上限。1 buffer に 64 回以上のローンチ操作は
/// 人間にもコントローラにも不可能なので、溢れたら捨てる (RT で再確保しない)。
const INBOX_CAP: usize = 64;

/// 列のフォローアクションの seed に混ぜる定数 (行の seed と衝突させないため)。
const SCENE_SEED_SALT: u64 = 0x5CE7_E5EE_D0F0_1234;

/// GUI から届いたローンチ操作 (`AudioCommand` の launcher 系を audio thread 用に
/// 落としたもの)。`Copy` で確保を伴わない。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LaunchRequest {
    /// セルの押下 / 離し。モードの解釈は engine 側 ([`LaunchMode`])。
    Cell { key: RowKey, clip_id: u32, pressed: bool },
    /// 列をまとめて撃つ。その列にセルを持たない行は停止する (Q11)。
    Scene { scene_id: u32, pressed: bool },
    /// 1 行を止める (アレンジへは戻さない)。
    StopRow { key: RowKey },
    StopAll,
    /// 1 行の主導権をアレンジへ返す。
    RowToArranger { key: RowKey },
    AllToArranger,
}

/// 予約の中身。行ごとに高々 1 件で、新しい発火が前の予約を置き換える。
#[derive(Debug, Clone, Copy, PartialEq)]
struct Queued {
    target: QueueTarget,
    /// 発火する song 拍 (量子化済み)。
    at_beat: f64,
    /// 前のセルの位相を引き継ぐ (Legato)。
    legato: bool,
    /// [`LaunchMode::Repeat`] の自動再予約。離したときに取り消す対象。
    from_repeat: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueTarget {
    Cell(u32),
    Stop,
    Arranger,
}

/// 1 行ぶんの走行状態。
#[derive(Debug, Clone)]
struct RowRuntime {
    key: RowKey,
    phase: RowPhase,
    queued: Option<Queued>,
    /// 次にフォローアクションを解決する song 拍 (`INFINITY` = 予定なし)。
    follow_at: f64,
    /// ワンショットが終わる song 拍 (`INFINITY` = ループする / 鳴っていない)。
    end_at: f64,
    /// [`LaunchMode::Gate`] / [`LaunchMode::Repeat`] で押されているセル (0 = なし)。
    held_clip_id: u32,
    /// [`LaunchMode::Repeat`] で押しっぱなし。
    repeating: bool,
    /// この buffer の song snapshot に実在した。
    alive: bool,
}

impl RowRuntime {
    fn new(key: RowKey) -> Self {
        Self {
            key,
            phase: RowPhase::Arranger,
            queued: None,
            follow_at: f64::INFINITY,
            end_at: f64::INFINITY,
            held_clip_id: 0,
            repeating: false,
            alive: true,
        }
    }
}

/// 列 (シーン) のフォローアクションの走行状態。**クリップのそれより優先する**
/// (Live 12 の規則) ので、行を解く前にこちらを先に発火させる。
#[derive(Debug, Clone, Copy)]
struct SceneRun {
    scene_id: u32,
    at: f64,
    /// その列でいちばん長いセルの長さ (拍)。**Linked (既定) の再武装に要る** —
    /// 0 のまま張り直すと `due_beat` が `None` を返し、以後そのシーンの
    /// フォローアクションが二度と発火しない。
    longest: f64,
}

/// この buffer の拍の範囲。
#[derive(Debug, Clone, Copy)]
pub struct BufferSpan {
    pub start_beat: f64,
    pub beats_per_frame: f64,
    pub frames: u32,
}

impl BufferSpan {
    #[must_use]
    pub fn new(start_beat: f64, current_bpm: f32, sample_rate: u32, frames: u32) -> Self {
        let bpf = if sample_rate > 0 && current_bpm > 0.0 {
            f64::from(current_bpm) / (60.0 * f64::from(sample_rate))
        } else {
            0.0
        };
        Self { start_beat, beats_per_frame: bpf, frames }
    }

    #[must_use]
    pub fn end_beat(self) -> f64 {
        self.start_beat + f64::from(self.frames) * self.beats_per_frame
    }

    /// song 拍 → buffer 内 frame (`[0, frames]` にクランプ)。
    #[must_use]
    fn frame_of(self, beat: f64) -> u32 {
        if !is_positive(self.beats_per_frame) {
            return 0;
        }
        let f = (beat - self.start_beat) / self.beats_per_frame;
        if !f.is_finite() || f <= 0.0 {
            return 0;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let f = f.floor() as u64;
        u32::try_from(f).unwrap_or(self.frames).min(self.frames)
    }
}

/// ランチャーの走行状態一式。`LocalState` (live) と `export.rs` (書き出し) が
/// **同じ型**を持つ — 片方だけ挙動が違うと Q9 の「聴こえた通りに書き出す」が壊れる。
#[derive(Debug)]
pub struct LauncherRuntime {
    rows: Vec<RowRuntime>,
    inbox: Vec<LaunchRequest>,
    table: RowSourceTable,
    /// [`common::model::launch_group`] へ渡す作業領域 (事前確保)。
    occupied: Vec<bool>,
    scene: SceneRun,
    last_project_id: u64,
    /// 次の `update` で `Song` の [`RowPlayback`] から撃ち直す。
    reseed: bool,
}

impl Default for LauncherRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl LauncherRuntime {
    #[must_use]
    pub fn new() -> Self {
        Self {
            rows: Vec::with_capacity(MAX_ROWS),
            inbox: Vec::with_capacity(INBOX_CAP),
            table: RowSourceTable::new(),
            occupied: vec![false; MAX_SCENES],
            scene: SceneRun { scene_id: 0, at: f64::INFINITY, longest: 0.0 },
            last_project_id: 0,
            reseed: true,
        }
    }

    /// GUI からの操作を積む。RT から呼ばれる (`pump_commands`) ので、溢れたら捨てる。
    pub fn push_request(&mut self, req: LaunchRequest) {
        if self.inbox.len() < self.inbox.capacity() {
            self.inbox.push(req);
        }
    }

    /// 次の `update` で `Song` の [`RowPlayback`] から撃ち直す (再生開始 /
    /// プロジェクト切替 / 書き出しの起点)。
    pub fn arm_reseed(&mut self) {
        self.reseed = true;
    }

    /// 直前に解いた行の供給元テーブル。
    #[must_use]
    pub fn rows(&self) -> &RowSourceTable {
        &self.table
    }

    /// 溜まっているローンチ操作を捨てる。**`update` を呼ばずに抜ける buffer**
    /// (書き出し中 / count-in 中) で使う — 溜めたままにすると、抜けた後に
    /// 全部同時に発火し、`INBOX_CAP` を超えた分は無言で消える。
    pub fn clear_requests(&mut self) {
        self.inbox.clear();
    }

    /// トランスポートが**跳んだ** (ループ端の巻き戻し / seek / 曲頭出し) ときに、
    /// 走行状態の絶対拍を同じ量だけ平行移動する。
    ///
    /// ランチャーの `launch_beat` / 予約 / フォローアクションの発火拍は
    /// **song-absolute 拍**なので、playhead だけが跳ぶと位相が壊れる:
    /// - `launch_beat` が跳び先より後ろになると `effective_beat` が `None` を返し、
    ///   その行は**ループ 1 周ぶん丸ごと無音**になる
    /// - 予約の発火拍やフォローアクションの発火拍を跳び越すと、**二度と発火しない**
    ///
    /// 位相を保ったまま新しい時間軸へ載せ直すので、ループを跨いでもセルは
    /// 途切れずに続く (Live / Bitwig と同じ)。**wrap と seek は必ずここを通す**
    /// (`queue_all_notes_off` と同じ「1 本にまとめる」流儀)。
    ///
    /// RT 安全: 事前確保済みの `Vec` を走査して加算するだけ。
    pub fn on_transport_jump(&mut self, delta_beats: f64) {
        if !delta_beats.is_finite() || delta_beats == 0.0 {
            return;
        }
        let shift = |v: &mut f64| {
            if v.is_finite() {
                *v += delta_beats;
            }
        };
        for row in &mut self.rows {
            if let RowPhase::Cell { launch_beat, .. } = &mut row.phase {
                shift(launch_beat);
            }
            if let Some(q) = &mut row.queued {
                shift(&mut q.at_beat);
            }
            shift(&mut row.follow_at);
            shift(&mut row.end_at);
        }
        shift(&mut self.scene.at);
    }

    /// セルを鳴らしている行が 1 つでもあるか。
    ///
    /// **曲末オートストップの抑止**に使う (`reached_transport_end`)。セルは
    /// アレンジの曲末と無関係に回るので、ここが `true` の間は「曲が終わった」を
    /// 停止の理由にしない。停止 (`Silent`) やアレンジ主導の行は数えない。
    #[must_use]
    pub fn any_cell_playing(&self) -> bool {
        self.rows.iter().any(|r| matches!(r.phase, RowPhase::Cell { .. }))
    }

    /// この buffer の行の供給元を解く。返り値はそのまま dispatch へ渡す。
    ///
    /// RT 安全: 事前確保した `Vec` の `clear` / `push` / `retain` と線形走査のみ。
    pub fn update(
        &mut self,
        song: &Song,
        span: BufferSpan,
        global_q: LaunchQuantize,
        playing: bool,
    ) -> &RowSourceTable {
        if song.project_id != self.last_project_id {
            // プロジェクトが変わると track_id / clip.id は 1 から採り直されるので、
            // 走行状態は全部無効 (`refresh_bundle` の reset と同じ理由)。
            self.rows.clear();
            self.inbox.clear();
            self.scene = SceneRun { scene_id: 0, at: f64::INFINITY, longest: 0.0 };
            self.last_project_id = song.project_id;
            self.reseed = true;
        }
        self.sync_rows(song);
        self.repoint_replaced_cells(song);
        if self.reseed {
            self.seed_from_song(song, span.start_beat);
            self.reseed = false;
        }
        self.drain_inbox(song, span, global_q, playing);
        self.tick_scene_follow(song, span);
        self.build_table(song, span);
        &self.table
    }

    /// 走行状態を publish する (表示専用、`Song` には入れない)。
    pub fn publish(&self, bridge: &common::audio_bridge::AudioBridgeHandle, span: BufferSpan) {
        use common::audio_bridge as ab;
        for (slot, row) in self.rows.iter().enumerate() {
            let (state, clip_id, progress, launch_beat) =
                row_display(row.phase, span.start_beat);
            let queued = match row.queued.map(|q| q.target) {
                Some(QueueTarget::Cell(id)) => id,
                Some(QueueTarget::Stop) => ab::LAUNCHER_QUEUED_STOP,
                Some(QueueTarget::Arranger) => ab::LAUNCHER_QUEUED_ARRANGER,
                None => 0,
            };
            // **発火拍もそのまま渡す。** GUI のカウントダウンはこれを引くだけで、
            // 量子化境界を GUI 側で解き直さない (シーンのフォローアクション由来の
            // 予約はグローバル量子化を迂回するので、解き直すと必ず食い違う)。
            let queued_at = row.queued.map_or(0.0, |q| q.at_beat);
            bridge.set_launcher_row(
                slot,
                row.key.packed(),
                state,
                clip_id,
                queued,
                queued_at,
                progress,
                launch_beat,
            );
        }
        bridge.clear_launcher_rows_from(self.rows.len());
    }

    // ---- 内部 ---------------------------------------------------------------

    /// `Song` にある行と走行状態を突き合わせる (増えた行を作り、消えた行を落とす)。
    fn sync_rows(&mut self, song: &Song) {
        for row in &mut self.rows {
            row.alive = false;
        }
        for track in &song.tracks {
            self.touch(RowKey::track(track.id));
            for lane in &track.automation_lanes {
                self.touch(RowKey::lane(track.id, lane.id));
            }
        }
        // マスター行のオートメーションレーン (`song_lanes`) も**ランチャーの行**。
        // GUI はここへセルを置けるので、engine が行として持たないと
        // 「撃っても何も起きない」になる (計画書 Q4)。
        for lane in &song.song_lanes {
            self.touch(RowKey::lane(common::model::MASTER_TRACK_ID, lane.id));
        }
        self.rows.retain(|r| r.alive);
    }

    /// 鳴っているセルが **id ごと置き換わった**行を追従させる。
    ///
    /// GUI がセルの上へ別のクリップを落とすと、その列のセルは新しい id で作り直され
    /// (`Track::put_session_clip`)、`Song` 側の主導権も新 id へ移る。走行状態は
    /// 古い id を指したままなので、放っておくと解決に失敗して**落とした瞬間に
    /// 音が消える**。位相 (`launch_beat`) は保ったまま id と長さだけ差し替えるので、
    /// 乗り換えは無音を挟まない。
    ///
    /// RT 安全: 線形走査のみ (確保・ロック・I/O なし)。
    fn repoint_replaced_cells(&mut self, song: &Song) {
        for row in &mut self.rows {
            let RowPhase::Cell { clip_id, launch_beat, .. } = row.phase else {
                continue;
            };
            let Some((cells, saved)) = row_of(song, row.key) else {
                continue;
            };
            if cells.find_by_clip(clip_id).is_some() {
                continue; // まだ居る = 置き換わっていない。
            }
            let RowPlayback::Launcher { clip_id: want } = saved else {
                continue; // 停止 / アレンジへ戻した行は seed 側が扱う。
            };
            let Some(cell) = cells.find_by_clip(want) else {
                continue; // 落とし先も無い = 本当に消えた (停止は normalize が決める)。
            };
            row.phase = RowPhase::Cell {
                clip_id: want,
                launch_beat,
                loop_len: cell.length_beats,
                cell_start_beat: cell.start_beat,
                looping: cell.looping,
            };
            // 差し替え前のセルを指したままの bookkeeping も直す。
            // `held_clip_id` が古いままだと `Gate` のセルが「離しても止まらない」、
            // 予約が古いままだと消えた id へ飛ぼうとして無音になる。
            if row.held_clip_id == clip_id {
                row.held_clip_id = want;
            }
            if let Some(q) = &mut row.queued
                && q.target == QueueTarget::Cell(clip_id)
            {
                q.target = QueueTarget::Cell(want);
            }
            arm_timers(row, &cells);
        }
    }

    fn touch(&mut self, key: RowKey) {
        if let Some(row) = self.rows.iter_mut().find(|r| r.key == key) {
            row.alive = true;
        } else if self.rows.len() < self.rows.capacity() {
            self.rows.push(RowRuntime::new(key));
        }
    }

    /// `Song` の [`RowPlayback`] から撃ち直す。**セルが引けない行は無音**に落とす —
    /// `Arranger` へ戻すと「ランチャーに渡した行」のアレンジのクリップが黙って
    /// 鳴り出す (`Song::normalize_session` と同じ規則)。
    fn seed_from_song(&mut self, song: &Song, now: f64) {
        for row in &mut self.rows {
            row.queued = None;
            row.held_clip_id = 0;
            row.repeating = false;
            let Some((cells, saved)) = row_of(song, row.key) else {
                row.phase = RowPhase::Arranger;
                row.follow_at = f64::INFINITY;
                row.end_at = f64::INFINITY;
                continue;
            };
            row.phase = match saved {
                RowPlayback::Arranger => RowPhase::Arranger,
                RowPlayback::LauncherStopped => RowPhase::Silent,
                RowPlayback::Launcher { clip_id } => cells
                    .find_by_clip(clip_id)
                    .map_or(RowPhase::Silent, |c| c.phase_at(now, None)),
            };
            arm_timers(row, &cells);
        }
    }

    /// 積まれた操作を予約に落とす。
    fn drain_inbox(
        &mut self,
        song: &Song,
        span: BufferSpan,
        global_q: LaunchQuantize,
        playing: bool,
    ) {
        // RT で `Vec` を確保しないよう、index で舐めてから 1 回だけ `clear`。
        for i in 0..self.inbox.len() {
            let req = self.inbox[i];
            self.apply_request(song, span, global_q, playing, req);
        }
        self.inbox.clear();
    }

    fn apply_request(
        &mut self,
        song: &Song,
        span: BufferSpan,
        global_q: LaunchQuantize,
        playing: bool,
        req: LaunchRequest,
    ) {
        let global_at = self.fire_beat(span, global_q, playing, LaunchQuantize::Global, song);
        match req {
            LaunchRequest::Cell { key, clip_id, pressed } => {
                self.press_cell(song, span, global_q, playing, key, clip_id, pressed);
            }
            LaunchRequest::Scene { scene_id, pressed } => {
                if pressed {
                    self.launch_scene(song, span, global_q, playing, scene_id);
                }
            }
            LaunchRequest::StopRow { key } => {
                self.queue(key, QueueTarget::Stop, global_at, false, false);
            }
            LaunchRequest::StopAll => self.queue_all(QueueTarget::Stop, global_at),
            LaunchRequest::RowToArranger { key } => {
                self.queue(key, QueueTarget::Arranger, global_at, false, false);
            }
            LaunchRequest::AllToArranger => self.queue_all(QueueTarget::Arranger, global_at),
        }
    }

    /// セルの押下 / 離しを [`LaunchMode`] 4 種に従って予約へ落とす。
    #[allow(clippy::too_many_arguments)]
    fn press_cell(
        &mut self,
        song: &Song,
        span: BufferSpan,
        global_q: LaunchQuantize,
        playing: bool,
        key: RowKey,
        clip_id: u32,
        pressed: bool,
    ) {
        let Some((cells, _)) = row_of(song, key) else { return };
        let Some(cell) = cells.find_by_clip(clip_id) else { return };
        let at = self.fire_beat(span, global_q, playing, cell.quantize, song);
        if !pressed {
            self.release_cell(key, clip_id, cell.mode, at);
            return;
        }
        let playing_now = self
            .rows
            .iter()
            .find(|r| r.key == key)
            .is_some_and(|r| r.phase.cell_clip_id() == Some(clip_id));
        // Toggle は「鳴っているセルをもう一度押したら止める」。他の 3 モードは発火。
        let target = if cell.mode == LaunchMode::Toggle && playing_now {
            QueueTarget::Stop
        } else {
            QueueTarget::Cell(clip_id)
        };
        self.queue(key, target, at, cell.legato, false);
        if let Some(row) = self.rows.iter_mut().find(|r| r.key == key) {
            row.held_clip_id = clip_id;
            row.repeating = cell.mode == LaunchMode::Repeat;
        }
    }

    /// 離したときの解釈。Gate は停止、Repeat は撃ち直しを止める (鳴っている
    /// セルはそのまま最後まで鳴る)。Trigger / Toggle は何もしない。
    ///
    /// `at` は **そのセルの量子化で解いた発火拍** — 停止も発火と同じ格子に乗せる
    /// (Live の Gate)。まだ発火していない予約が残っていればその拍を流用するので、
    /// 「境界の前に押して離した」は発火せずに終わる (= Live と同じ)。
    fn release_cell(&mut self, key: RowKey, clip_id: u32, mode: LaunchMode, at: f64) {
        let Some(row) = self.rows.iter_mut().find(|r| r.key == key) else { return };
        if row.held_clip_id != clip_id {
            return;
        }
        row.held_clip_id = 0;
        match mode {
            LaunchMode::Gate => {
                let at = row.queued.map_or(at, |q| q.at_beat);
                row.queued = Some(Queued {
                    target: QueueTarget::Stop,
                    at_beat: at,
                    legato: false,
                    from_repeat: false,
                });
            }
            LaunchMode::Repeat => {
                row.repeating = false;
                if row.queued.is_some_and(|q| q.from_repeat) {
                    row.queued = None;
                }
            }
            LaunchMode::Trigger | LaunchMode::Toggle => {}
        }
    }

    /// 列を撃つ。その列にセルを持たない行は **停止**する (Q11)。
    fn launch_scene(
        &mut self,
        song: &Song,
        span: BufferSpan,
        global_q: LaunchQuantize,
        playing: bool,
        scene_id: u32,
    ) {
        let global_at = self.fire_beat(span, global_q, playing, LaunchQuantize::Global, song);
        let mut longest = 0.0_f64;
        let mut fire = f64::INFINITY;
        for i in 0..self.rows.len() {
            let key = self.rows[i].key;
            let Some((cells, _)) = row_of(song, key) else { continue };
            match cells.find_by_scene(scene_id) {
                Some(cell) => {
                    let at = self.fire_beat(span, global_q, playing, cell.quantize, song);
                    longest = longest.max(cell.length_beats);
                    fire = fire.min(at);
                    self.queue(key, QueueTarget::Cell(cell.clip_id), at, cell.legato, false);
                }
                None => {
                    fire = fire.min(global_at);
                    self.queue(key, QueueTarget::Stop, global_at, false, false);
                }
            }
        }
        self.arm_scene_follow(song, scene_id, fire, longest, span.start_beat);
    }

    /// 列のフォローアクションの起点を張る。Linked は「その列で最も長いセル」を
    /// 1 周とみなす — 列そのものは長さを持たないので、鳴っている中身から導く。
    fn arm_scene_follow(
        &mut self,
        song: &Song,
        scene_id: u32,
        fire: f64,
        longest: f64,
        now: f64,
    ) {
        let follow = song
            .scenes
            .iter()
            .find(|s| s.id == scene_id)
            .map(|s| s.follow.clone())
            .unwrap_or_default();
        let base = if fire.is_finite() { fire } else { now };
        self.scene = SceneRun {
            scene_id,
            at: follow::due_beat(&follow, base, longest).unwrap_or(f64::INFINITY),
            longest,
        };
    }

    /// 予約を置く (行ごとに高々 1 件、新しい発火が前を置き換える)。
    fn queue(&mut self, key: RowKey, target: QueueTarget, at: f64, legato: bool, rep: bool) {
        if let Some(row) = self.rows.iter_mut().find(|r| r.key == key) {
            row.queued = Some(Queued { target, at_beat: at, legato, from_repeat: rep });
        }
    }

    fn queue_all(&mut self, target: QueueTarget, at: f64) {
        for row in &mut self.rows {
            row.queued = Some(Queued { target, at_beat: at, legato: false, from_repeat: false });
            row.held_clip_id = 0;
            row.repeating = false;
        }
    }

    /// 押した操作が実際に発火する song 拍。停止中は量子化しない
    /// (拍が進まないので待っても永久に来ない)。
    fn fire_beat(
        &self,
        span: BufferSpan,
        global_q: LaunchQuantize,
        playing: bool,
        cell_q: LaunchQuantize,
        song: &Song,
    ) -> f64 {
        if !playing {
            return f64::NEG_INFINITY;
        }
        match quantize::resolve(cell_q, global_q, song.time_sig) {
            Some(q) => quantize::next_boundary(span.start_beat, q),
            None => span.start_beat,
        }
    }

    /// 列のフォローアクション。**クリップのそれより優先**するので、行を解く前に
    /// ここで予約を置く (行の予約は「新しい発火が前を置き換える」)。
    /// `global_q` は取らない — シーンのフォローアクションは**グローバル量子化を
    /// 迂回する** (計画書 §2.3) ので、発火拍の解決に使わない。
    fn tick_scene_follow(&mut self, song: &Song, span: BufferSpan) {
        if !self.scene.at.is_finite() || self.scene.at >= span.end_beat() {
            return;
        }
        let fire = self.scene.at.max(span.start_beat);
        let Some(pos) = song.scenes.iter().position(|s| s.id == self.scene.scene_id) else {
            self.scene.at = f64::INFINITY;
            return;
        };
        let follow = song.scenes[pos].follow.clone();
        let n = self.fill_scene_occupancy(song);
        let seed = follow::row_seed(SCENE_SEED_SALT, self.scene.scene_id);
        let outcome =
            follow::resolve(&follow, &self.occupied[..n], pos, &song.scenes, seed, fire);
        match outcome {
            FollowOutcome::Keep => {
                // 再武装は **初回と同じ長さ** (その列の最長セル) で張る。
                let longest = self.scene.longest;
                self.scene.at =
                    follow::due_beat(&follow, fire, longest).unwrap_or(f64::INFINITY);
            }
            FollowOutcome::Stop => {
                self.queue_all(QueueTarget::Stop, fire);
                self.scene.at = f64::INFINITY;
            }
            FollowOutcome::Go(idx) => match song.scenes.get(idx).map(|s| s.id) {
                // 計画書 §2.3: フォローアクションはグローバル量子化を迂回する
                // (`Off` を渡すと、`Global` のセルは待たずに発火し、独自の
                // `quantize` を持つセルはそちらに従う)。
                Some(id) => self.launch_scene(song, span, LaunchQuantize::Off, true, id),
                None => self.scene.at = f64::INFINITY,
            },
        }
    }

    /// 「その列にどれかの行のセルがあるか」を作業領域へ埋め、有効長を返す。
    fn fill_scene_occupancy(&mut self, song: &Song) -> usize {
        let n = song.scenes.len().min(MAX_SCENES);
        for (i, slot) in self.occupied[..n].iter_mut().enumerate() {
            let id = song.scenes[i].id;
            *slot = song.tracks.iter().any(|t| scene_used_by_track(t, id));
        }
        n
    }

    /// 全行の供給元を解いてテーブルへ詰める。
    fn build_table(&mut self, song: &Song, span: BufferSpan) {
        self.table.clear();
        for track in &song.tracks {
            self.table.begin_track();
            let src = self.solve(song, span, RowKey::track(track.id));
            self.table.push(src);
            for lane in &track.automation_lanes {
                let src = self.solve(song, span, RowKey::lane(track.id, lane.id));
                self.table.push(src);
            }
        }
        // マスター行 (`song_lanes`) は最後のグループへ。
        self.table.begin_master();
        for lane in &song.song_lanes {
            let src = self.solve(song, span, RowKey::lane(common::model::MASTER_TRACK_ID, lane.id));
            self.table.push(src);
        }
        // `row_at` が「次のトラックの先頭」で範囲外を判定できるよう番兵を置く。
        self.table.begin_track();
    }

    /// 1 行を 1 buffer 分解く。遷移は高々 1 回 ([`RowTimeSource`] の doc)。
    fn solve(&mut self, song: &Song, span: BufferSpan, key: RowKey) -> RowTimeSource {
        let Some(idx) = self.rows.iter().position(|r| r.key == key) else {
            return RowTimeSource::uniform(key, RowPhase::Arranger);
        };
        let Some((cells, _)) = row_of(song, key) else {
            return RowTimeSource::uniform(key, self.rows[idx].phase);
        };
        // **鳴っているセルがまだ実在するかを毎 buffer 確かめる。**
        // 消えた id を指したまま走ると、MIDI もオーディオも「そのセルが無い」で
        // 全部 skip されて **無音のまま PLAYING を publish し続ける** (どのセルも
        // 光らず進捗も出ない)。`repoint_replaced_cells` は「同じ行で id が振り直された」
        // 場合の最適化にすぎないので、最後の砦をここに置く。
        if let RowPhase::Cell { clip_id, .. } = self.rows[idx].phase
            && cells.find_by_clip(clip_id).is_none()
        {
            self.rows[idx].phase = RowPhase::Silent;
            self.rows[idx].queued = None;
            self.rows[idx].held_clip_id = 0;
            self.rows[idx].repeating = false;
            self.rows[idx].follow_at = f64::INFINITY;
            self.rows[idx].end_at = f64::INFINITY;
        }
        let head = self.rows[idx].phase;
        let Some((at, kind)) = self.next_event(idx, span) else {
            return RowTimeSource::uniform(key, head);
        };
        let fire = at.max(span.start_beat);
        let launch_at = if at.is_finite() { at } else { fire };
        let tail = self.apply_event(song, idx, &cells, kind, launch_at, fire);
        RowTimeSource { key, head, tail, switch_frame: span.frame_of(fire) }
    }

    /// この buffer で発火する最初の出来事。**同拍ならユーザー操作が最優先**
    /// (配列の並びが優先順位、比較は厳密な `<` なので先に入れた方が残る)。
    fn next_event(&self, idx: usize, span: BufferSpan) -> Option<(f64, EventKind)> {
        let row = &self.rows[idx];
        let end = span.end_beat();
        let mut best: Option<(f64, EventKind)> = None;
        let candidates = [
            (row.queued.map_or(f64::INFINITY, |q| q.at_beat), EventKind::Queued),
            (row.follow_at, EventKind::Follow),
            (row.end_at, EventKind::OneShotEnd),
        ];
        for (at, kind) in candidates {
            if at < end && best.is_none_or(|(b, _)| at < b) {
                best = Some((at, kind));
            }
        }
        best
    }

    /// 出来事を適用して、この buffer の後半の供給元を返す。
    fn apply_event(
        &mut self,
        song: &Song,
        idx: usize,
        cells: &RowCells<'_>,
        kind: EventKind,
        at: f64,
        fire: f64,
    ) -> RowPhase {
        match kind {
            EventKind::Queued => {
                let Some(q) = self.rows[idx].queued.take() else {
                    return self.rows[idx].phase;
                };
                self.enter(idx, cells, q.target, at, q.legato)
            }
            EventKind::OneShotEnd => self.set_phase(idx, RowPhase::Silent, cells),
            EventKind::Follow => self.apply_follow(song, idx, cells, fire),
        }
    }

    /// クリップのフォローアクションを 1 回解決する。
    fn apply_follow(
        &mut self,
        song: &Song,
        idx: usize,
        cells: &RowCells<'_>,
        fire: f64,
    ) -> RowPhase {
        let phase = self.rows[idx].phase;
        let Some(clip_id) = phase.cell_clip_id() else {
            self.rows[idx].follow_at = f64::INFINITY;
            return phase;
        };
        let Some(cell) = cells.find_by_clip(clip_id) else {
            return self.set_phase(idx, RowPhase::Silent, cells);
        };
        let n = fill_row_occupancy(&mut self.occupied, song, cells);
        let from = song.scenes.iter().position(|s| s.id == cell.scene_id).unwrap_or(0);
        let seed = follow::row_seed(self.rows[idx].key.packed(), clip_id);
        let outcome =
            follow::resolve(&cell.follow, &self.occupied[..n], from, &song.scenes, seed, fire);
        match outcome {
            FollowOutcome::Keep => {
                // 鳴り続ける。次の発火だけ張り直す (位相は動かさない)。
                self.rows[idx].follow_at =
                    follow::due_beat(&cell.follow, fire, cell.length_beats)
                        .unwrap_or(f64::INFINITY);
                phase
            }
            FollowOutcome::Stop => self.set_phase(idx, RowPhase::Silent, cells),
            FollowOutcome::Go(scene_idx) => {
                let Some(scene_id) = song.scenes.get(scene_idx).map(|s| s.id) else {
                    return self.set_phase(idx, RowPhase::Silent, cells);
                };
                self.go_to_scene(idx, cells, song, scene_id, fire, phase)
            }
        }
    }

    /// フォローアクションで列 `scene_id` のセルへ移る。
    ///
    /// 計画書 §2.3: 発火は**グローバル量子化を迂回する**が、**飛び先セル自身の
    /// `quantize` には従う** (1/4 量子化のセルへ飛ぶなら拍の頭で切り替わる)。
    /// 空セルの列へ飛んだら停止 (Q11)。
    fn go_to_scene(
        &mut self,
        idx: usize,
        cells: &RowCells<'_>,
        song: &Song,
        scene_id: u32,
        fire: f64,
        phase: RowPhase,
    ) -> RowPhase {
        let Some(next) = cells.find_by_scene(scene_id) else {
            return self.set_phase(idx, RowPhase::Silent, cells);
        };
        let (id, legato) = (next.clip_id, next.legato);
        let at = cell_fire_beat(next.quantize, song.time_sig, fire);
        if at > fire {
            let key = self.rows[idx].key;
            self.queue(key, QueueTarget::Cell(id), at, legato, false);
            return phase;
        }
        self.enter(idx, cells, QueueTarget::Cell(id), fire, legato)
    }

    /// 予約 / フォローアクションの行き先へ実際に入る。
    fn enter(
        &mut self,
        idx: usize,
        cells: &RowCells<'_>,
        target: QueueTarget,
        at: f64,
        legato: bool,
    ) -> RowPhase {
        let clip_id = match target {
            QueueTarget::Arranger => return self.set_phase(idx, RowPhase::Arranger, cells),
            QueueTarget::Stop => return self.set_phase(idx, RowPhase::Silent, cells),
            QueueTarget::Cell(id) => id,
        };
        let Some(cell) = cells.find_by_clip(clip_id) else {
            return self.set_phase(idx, RowPhase::Silent, cells);
        };
        let prev = self.rows[idx].phase;
        let carry = legato
            .then(|| prev.effective_beat(at).map(|b| (prev, b)))
            .flatten();
        let phase = cell.phase_at(at, carry);
        let out = self.set_phase(idx, phase, cells);
        // Repeat: 押している間、セルの長さの周期で撃ち直す。
        if self.rows[idx].repeating
            && self.rows[idx].held_clip_id == clip_id
            && cell.length_beats > 0.0
        {
            self.rows[idx].queued = Some(Queued {
                target,
                at_beat: at + cell.length_beats,
                legato: false,
                from_repeat: true,
            });
        }
        out
    }

    /// 供給元を差し替え、タイマーを張り直す。
    fn set_phase(&mut self, idx: usize, phase: RowPhase, cells: &RowCells<'_>) -> RowPhase {
        self.rows[idx].phase = phase;
        arm_timers(&mut self.rows[idx], cells);
        phase
    }
}

/// この buffer で発火する出来事の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventKind {
    /// ユーザー操作 / 列の発火の予約。
    Queued,
    /// クリップのフォローアクション。
    Follow,
    /// ワンショットの終端。
    OneShotEnd,
}

/// publish 用に供給元を `(state, clip_id, progress)` へ落とす。
fn row_display(phase: RowPhase, beat: f64) -> (u32, u32, f32, f64) {
    use common::audio_bridge as ab;
    match phase {
        RowPhase::Arranger => (ab::LAUNCHER_STATE_ARRANGER, 0, 0.0, 0.0),
        RowPhase::Silent => (ab::LAUNCHER_STATE_STOPPED, 0, 0.0, 0.0),
        RowPhase::Cell { clip_id, loop_len, cell_start_beat, launch_beat, .. } => {
            let p = match phase.effective_beat(beat) {
                Some(b) if loop_len > 0.0 => {
                    #[allow(clippy::cast_possible_truncation)]
                    let v = ((b - cell_start_beat) / loop_len) as f32;
                    v.clamp(0.0, 1.0)
                }
                _ => 0.0,
            };
            // r.md #87: `launch_beat` も出す。進捗は 30Hz でしか届かないので、
            // 映像側はこれを起点に自分のフレーム時刻から位相を解き直す
            // (= 音と同じ式・同じ滑らかさ。計画書 §3.6)。
            (ab::LAUNCHER_STATE_PLAYING, clip_id, p, launch_beat)
        }
    }
}

/// フォローアクションの飛び先セルが実際に発火する拍。
/// **グローバル量子化は見ない** (迂回する) が、セル自身の `quantize` には従う。
#[must_use]
fn cell_fire_beat(cell_q: LaunchQuantize, time_sig: (u8, u8), fire: f64) -> f64 {
    match quantize::resolve(cell_q, LaunchQuantize::Off, time_sig) {
        Some(q) => quantize::next_boundary(fire, q),
        None => fire,
    }
}

/// その列を使っているセルがこのトラック (レーンを含む) にあるか。
fn scene_used_by_track(track: &Track, scene_id: u32) -> bool {
    track.session_clips.iter().any(|c| c.scene_id == scene_id)
        || track
            .automation_lanes
            .iter()
            .any(|l| l.session_clips.iter().any(|c| c.scene_id == scene_id))
}

/// 行のセル列と、保存されている主導権。
fn row_of(song: &Song, key: RowKey) -> Option<(RowCells<'_>, RowPlayback)> {
    if key.track_id == common::model::MASTER_TRACK_ID {
        // マスター行はトラックを持たない (`Song.song_lanes` が実体)。
        let lane = song.song_lanes.iter().find(|l| l.id == key.lane_id)?;
        return Some((RowCells::Lane(&lane.session_clips), lane.launcher));
    }
    let track: &Track = song.tracks.iter().find(|t| t.id == key.track_id)?;
    if key.lane_id == 0 {
        return Some((RowCells::Track(&track.session_clips), track.launcher));
    }
    let lane: &AutomationLane = track.automation_lanes.iter().find(|l| l.id == key.lane_id)?;
    Some((RowCells::Lane(&lane.session_clips), lane.launcher))
}

/// 「この行がその列にセルを持つか」を作業領域へ埋め、有効長を返す。
fn fill_row_occupancy(occupied: &mut [bool], song: &Song, cells: &RowCells<'_>) -> usize {
    let n = song.scenes.len().min(occupied.len());
    for (i, slot) in occupied[..n].iter_mut().enumerate() {
        *slot = cells.find_by_scene(song.scenes[i].id).is_some();
    }
    n
}

/// 供給元が変わった直後のタイマーを張り直す。
fn arm_timers(row: &mut RowRuntime, cells: &RowCells<'_>) {
    let RowPhase::Cell { clip_id, launch_beat, loop_len, looping, .. } = row.phase else {
        row.follow_at = f64::INFINITY;
        row.end_at = f64::INFINITY;
        return;
    };
    row.follow_at = cells
        .find_by_clip(clip_id)
        .and_then(|c| follow::due_beat(&c.follow, launch_beat, loop_len))
        .unwrap_or(f64::INFINITY);
    row.end_at = if looping || !is_positive(loop_len) {
        f64::INFINITY
    } else {
        launch_beat + loop_len
    };
}

/// 行のセル列。トラック行とレーン行で型が違うだけで規則は同じ (Q4) なので、
/// 判定は 1 本にまとめる。
#[derive(Debug, Clone, Copy)]
pub enum RowCells<'a> {
    Track(&'a [SessionClip]),
    Lane(&'a [SessionAutomationClip]),
}

impl RowCells<'_> {
    /// `clip.id` でセルを引く (行の中で一意)。
    #[must_use]
    pub fn find_by_clip(&self, clip_id: u32) -> Option<CellRef> {
        self.find(|id, _| id == clip_id)
    }

    /// 列 (`scene_id`) でセルを引く。
    #[must_use]
    pub fn find_by_scene(&self, scene_id: u32) -> Option<CellRef> {
        self.find(|_, sid| sid == scene_id)
    }

    fn find(&self, pred: impl Fn(u32, u32) -> bool) -> Option<CellRef> {
        match self {
            Self::Track(v) => v
                .iter()
                .find(|c| pred(c.clip.id, c.scene_id))
                .map(|c| CellRef::of(c.scene_id, &c.launch, &c.clip.id, c.clip.start_beat, c.clip.length_beats)),
            Self::Lane(v) => v
                .iter()
                .find(|c| pred(c.clip.id, c.scene_id))
                .map(|c| CellRef::of(c.scene_id, &c.launch, &c.clip.id, c.clip.start_beat, c.clip.length_beats)),
        }
    }
}

/// セル 1 つから、発火の判断に要る値だけを抜いたもの。
#[derive(Debug, Clone)]
pub struct CellRef {
    pub clip_id: u32,
    pub scene_id: u32,
    /// セル自身の拍原点。正規化済みなら 0 だが、IPC は信頼境界なので値を持ち回る。
    pub start_beat: f64,
    pub length_beats: f64,
    pub quantize: LaunchQuantize,
    pub mode: LaunchMode,
    pub looping: bool,
    pub legato: bool,
    pub follow: FollowAction,
}

impl CellRef {
    fn of(
        scene_id: u32,
        launch: &LaunchSettings,
        clip_id: &u32,
        start_beat: f64,
        length_beats: f64,
    ) -> Self {
        Self {
            clip_id: *clip_id,
            scene_id,
            start_beat,
            length_beats,
            quantize: launch.quantize,
            mode: launch.mode,
            looping: launch.looping,
            legato: launch.legato,
            follow: launch.follow.clone(),
        }
    }

    /// このセルを `at` 拍で撃った状態。
    ///
    /// `prev` を渡すと **Legato** — 前のセルの位相 (`(前の供給元, その実効拍)`) を
    /// 引き継ぐように `launch_beat` を逆算する。同じ小節位置のまま別のループへ
    /// 乗り換えるのが Legato の定義なので、位相は移る先のセル長で折り返す。
    #[must_use]
    pub fn phase_at(&self, at: f64, prev: Option<(RowPhase, f64)>) -> RowPhase {
        let launch_beat = match prev {
            Some((RowPhase::Cell { cell_start_beat, .. }, eff)) if self.length_beats > 0.0 => {
                at - (eff - cell_start_beat).rem_euclid(self.length_beats)
            }
            _ => at,
        };
        RowPhase::Cell {
            clip_id: self.clip_id,
            launch_beat,
            loop_len: self.length_beats,
            cell_start_beat: self.start_beat,
            looping: self.looping,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{Clip, FollowActionKind, Scene, Track};

    const SR: u32 = 48_000;
    const BPM: f32 = 120.0;
    /// 512 frame @ 120 BPM / 48 kHz = 0.02133 拍。
    const FRAMES: u32 = 512;

    fn span_at(beat: f64) -> BufferSpan {
        BufferSpan::new(beat, BPM, SR, FRAMES)
    }

    fn cell(clip_id: u32, scene_id: u32, len: f64) -> SessionClip {
        SessionClip {
            scene_id,
            clip: Clip { id: clip_id, start_beat: 0.0, length_beats: len, ..Clip::default() },
            launch: LaunchSettings::default(),
        }
    }

    /// track 1 は列 1/2 にセルを持ち、track 2 は列 1 だけ。
    /// (列 2 を撃つと track 2 は「空セル = 停止」になる)
    fn two_rows() -> Song {
        let mut song = Song { bpm: BPM, project_id: 7, ..Song::default() };
        song.scenes = vec![Scene::new(1), Scene::new(2)];
        let mut t1 = Track { id: 1, next_clip_id: 100, ..Track::default() };
        t1.session_clips.push(cell(10, 1, 4.0));
        t1.session_clips.push(cell(11, 2, 4.0));
        let mut t2 = Track { id: 2, next_clip_id: 100, ..Track::default() };
        t2.session_clips.push(cell(20, 1, 4.0));
        song.tracks.push(t1);
        song.tracks.push(t2);
        song
    }

    /// 量子化なしで 1 buffer 進める。
    fn step(rt: &mut LauncherRuntime, song: &Song, beat: f64) {
        rt.update(song, span_at(beat), LaunchQuantize::Off, true);
    }

    fn press(rt: &mut LauncherRuntime, track_id: u32, clip_id: u32, pressed: bool) {
        rt.push_request(LaunchRequest::Cell { key: RowKey::track(track_id), clip_id, pressed });
    }

    #[test]
    fn 空セルの列を撃つと行は停止する() {
        let song = two_rows();
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);

        rt.push_request(LaunchRequest::Scene { scene_id: 2, pressed: true });
        step(&mut rt, &song, 0.1);

        // 列 2 にセルを持つ track 1 は鳴る。
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(11));
        // 列 2 が空の track 2 は **停止** (アレンジへは戻らない、Q11)。
        assert_eq!(rt.rows().track_row(1).tail, RowPhase::Silent);
        assert_eq!(rt.rows().track_row(1).head, RowPhase::Arranger, "切り替え前はアレンジ");
    }

    /// 撃ったセルは**何周でもループし続ける**。1 周ぶん進めても供給元が
    /// セルのまま / 位相が巻き戻ることを、実際に buffer を刻んで確かめる。
    #[test]
    fn 撃ったセルは周回してもループし続ける() {
        let song = two_rows();
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        press(&mut rt, 1, 10, true);
        step(&mut rt, &song, 0.0);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(10), "撃った直後");

        // 4 拍セルを 3 周ぶん (12 拍) 刻む。位相は毎回 [0,4) に収まる。
        let mut beat = 0.0;
        let mut seen_wrap = 0;
        let mut prev_eff = 0.0;
        while beat < 12.0 {
            beat += 0.25;
            step(&mut rt, &song, beat);
            let src = rt.rows().track_row(0);
            let phase = src.tail;
            assert_eq!(phase.cell_clip_id(), Some(10), "拍 {beat} でセルを離した");
            let eff = phase.effective_beat(beat).expect("鳴っている");
            assert!((0.0..4.0).contains(&eff), "拍 {beat} の位相が窓の外: {eff}");
            if eff < prev_eff {
                seen_wrap += 1;
            }
            prev_eff = eff;
        }
        assert!(seen_wrap >= 2, "3 周ぶん進めたのに巻き戻りが {seen_wrap} 回");
    }

    #[test]
    fn 撃つと量子化境界まで待ってから切り替わる() {
        let song = two_rows();
        let mut rt = LauncherRuntime::new();
        rt.update(&song, span_at(0.0), LaunchQuantize::Bars(1), true);

        // 拍 1.0 で押す → 1 小節量子化 (4/4 = 4 拍) なので発火は拍 4.0。
        press(&mut rt, 1, 10, true);
        rt.update(&song, span_at(1.0), LaunchQuantize::Bars(1), true);
        assert_eq!(rt.rows().track_row(0).tail, RowPhase::Arranger, "まだ鳴らない");

        // 拍 3.99 の buffer は 4.0 を含む → その buffer の途中で切り替わる。
        rt.update(&song, span_at(3.99), LaunchQuantize::Bars(1), true);
        let src = rt.rows().track_row(0);
        assert_eq!(src.head, RowPhase::Arranger);
        assert_eq!(src.tail.cell_clip_id(), Some(10));
        // 拍 4.0 = buffer 先頭から 0.01 拍 = 240 sample。
        assert!((239..=241).contains(&src.switch_frame), "{}", src.switch_frame);
        // 位相の原点は buffer 先頭ではなく **量子化境界そのもの**。
        let RowPhase::Cell { launch_beat, .. } = src.tail else {
            panic!("セルになっていない")
        };
        assert!((launch_beat - 4.0).abs() < 1e-9, "{launch_beat}");
    }

    #[test]
    fn legato_は前のセルの位相を引き継ぐ() {
        let mut song = two_rows();
        song.tracks[0].session_clips[1].launch.legato = true;
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);

        // セル 10 を拍 0 で撃つ。
        press(&mut rt, 1, 10, true);
        step(&mut rt, &song, 0.0);
        // 拍 2.5 で legato のセル 11 へ乗り換える。
        press(&mut rt, 1, 11, true);
        step(&mut rt, &song, 2.5);

        let tail = rt.rows().track_row(0).tail;
        assert_eq!(tail.cell_clip_id(), Some(11));
        // 位相が保たれる = 実効拍は乗り換え前と同じ 2.5。
        assert_eq!(tail.effective_beat(2.5), Some(2.5));

        // legato でなければ頭から鳴り直す。
        song.tracks[0].session_clips[1].launch.legato = false;
        let mut rt2 = LauncherRuntime::new();
        step(&mut rt2, &song, 0.0);
        press(&mut rt2, 1, 10, true);
        step(&mut rt2, &song, 0.0);
        press(&mut rt2, 1, 11, true);
        step(&mut rt2, &song, 2.5);
        assert_eq!(rt2.rows().track_row(0).tail.effective_beat(2.5), Some(0.0));
    }

    #[test]
    fn ワンショットはセル終端で止まる() {
        let mut song = two_rows();
        song.tracks[0].session_clips[0].launch.looping = false;
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        press(&mut rt, 1, 10, true);
        step(&mut rt, &song, 0.0);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(10));

        // 4 拍セルなので拍 4.0 で終わる。
        step(&mut rt, &song, 3.99);
        assert_eq!(rt.rows().track_row(0).tail, RowPhase::Silent);
        assert_eq!(rt.rows().track_row(0).head.cell_clip_id(), Some(10), "終端までは鳴る");
    }

    #[test]
    fn 停止から再生すると保存した状態から鳴り直す() {
        let mut song = two_rows();
        // ユーザーが最後に撃った状態を Song が持っている (§1.4)。
        song.tracks[0].launcher = RowPlayback::Launcher { clip_id: 11 };
        song.tracks[1].launcher = RowPlayback::LauncherStopped;

        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(11));
        assert_eq!(rt.rows().track_row(1).tail, RowPhase::Silent);

        // 走行中に別のセルへ移っても…
        press(&mut rt, 1, 10, true);
        step(&mut rt, &song, 1.0);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(10));

        // 再生し直す (= reseed) と Song の状態へ戻る。
        rt.arm_reseed();
        step(&mut rt, &song, 0.0);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(11));
    }

    #[test]
    fn 消えたセルを指す主導権は停止に落ちる() {
        let mut song = two_rows();
        // 実在しない clip_id (= GUI 側で消されたセル)。engine は信頼境界。
        song.tracks[0].launcher = RowPlayback::Launcher { clip_id: 999 };
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        assert_eq!(
            rt.rows().track_row(0).tail,
            RowPhase::Silent,
            "Arranger へ戻すとアレンジのクリップが黙って鳴り出す"
        );
    }

    #[test]
    fn アレンジへ返すと主導権が戻る() {
        let song = two_rows();
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        press(&mut rt, 1, 10, true);
        step(&mut rt, &song, 0.0);
        assert!(rt.rows().track_row(0).tail.cell_clip_id().is_some());

        rt.push_request(LaunchRequest::AllToArranger);
        step(&mut rt, &song, 1.0);
        assert_eq!(rt.rows().track_row(0).tail, RowPhase::Arranger);
        assert_eq!(rt.rows().track_row(1).tail, RowPhase::Arranger);
    }

    #[test]
    fn gate_は離すと止まり_toggle_は押し直すと止まる() {
        let mut song = two_rows();
        song.tracks[0].session_clips[0].launch.mode = LaunchMode::Gate;
        song.tracks[1].session_clips[0].launch.mode = LaunchMode::Toggle;
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);

        press(&mut rt, 1, 10, true);
        press(&mut rt, 2, 20, true);
        step(&mut rt, &song, 0.0);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(10));
        assert_eq!(rt.rows().track_row(1).tail.cell_clip_id(), Some(20));

        // Gate: 離すと止まる / Toggle: 離しても鳴り続ける。
        press(&mut rt, 1, 10, false);
        press(&mut rt, 2, 20, false);
        step(&mut rt, &song, 1.0);
        assert_eq!(rt.rows().track_row(0).tail, RowPhase::Silent);
        assert_eq!(rt.rows().track_row(1).tail.cell_clip_id(), Some(20));

        // Toggle: もう一度押すと止まる。
        press(&mut rt, 2, 20, true);
        step(&mut rt, &song, 2.0);
        assert_eq!(rt.rows().track_row(1).tail, RowPhase::Silent);
    }

    /// Q9 の前提: **同じプロジェクトから 2 回走らせたら遷移が完全に一致する**。
    /// フォローアクションの抽選と `Any` の選択が走行状態を持っていたらここで落ちる。
    #[test]
    fn 同じ状態から二度走らせると遷移が一致する() {
        let mut song = two_rows();
        // 両セルに「50% で Any / 50% で Next」のフォローアクションを付ける
        // (1 周ごとに発火 = 走査中に何度も抽選が走る)。
        for c in &mut song.tracks[0].session_clips {
            c.launch.follow = FollowAction {
                enabled: true,
                a: FollowActionKind::Any,
                b: FollowActionKind::Next,
                chance_a: 50,
                ..FollowAction::default()
            };
        }
        song.tracks[0].launcher = RowPlayback::Launcher { clip_id: 10 };

        let run = || {
            let mut rt = LauncherRuntime::new();
            let mut trace = Vec::new();
            let mut beat = 0.0;
            for _ in 0..2000 {
                rt.update(&song, span_at(beat), LaunchQuantize::Off, true);
                trace.push(rt.rows().track_row(0).tail);
                beat += f64::from(FRAMES) * f64::from(BPM) / (60.0 * f64::from(SR));
            }
            trace
        };
        let a = run();
        let b = run();
        assert_eq!(a, b, "同じ状態から 2 回走らせて遷移が違う = 書き出しが再現しない");
        // 実際にセルを行き来している (= 抽選が効いている) ことも確認する。
        let ids: std::collections::HashSet<Option<u32>> =
            a.iter().map(|p| p.cell_clip_id()).collect();
        assert!(ids.len() >= 2, "フォローアクションが 1 度も動いていない: {ids:?}");
    }

    #[test]
    fn フォローアクションで空の列へ飛ぶと停止する() {
        let mut song = two_rows();
        // track 2 は列 1 にしかセルが無い。列 2 へ Jump させる。
        song.tracks[1].session_clips[0].launch.follow = FollowAction {
            enabled: true,
            a: FollowActionKind::Jump { scene_id: 2 },
            chance_a: 100,
            ..FollowAction::default()
        };
        song.tracks[1].launcher = RowPlayback::Launcher { clip_id: 20 };
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        assert_eq!(rt.rows().track_row(1).tail.cell_clip_id(), Some(20));
        // 4 拍セルなので拍 4.0 でフォローアクションが発火する。
        step(&mut rt, &song, 3.99);
        assert_eq!(rt.rows().track_row(1).tail, RowPhase::Silent);
    }

    #[test]
    fn プロジェクトが変わると走行状態を捨てる() {
        let song = two_rows();
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        press(&mut rt, 1, 10, true);
        step(&mut rt, &song, 0.0);
        assert!(rt.rows().track_row(0).tail.cell_clip_id().is_some());

        // 別 project (track_id / clip.id は 1 から採り直される)。
        let mut other = two_rows();
        other.project_id = 99;
        step(&mut rt, &other, 0.0);
        assert_eq!(
            rt.rows().track_row(0).tail,
            RowPhase::Arranger,
            "前 project の走行状態が同じ id で継続してはいけない"
        );
    }
}
