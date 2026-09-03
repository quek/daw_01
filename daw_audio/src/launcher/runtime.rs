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
    AutomationLane, FollowAction, FollowActionKind, LaunchMode, LaunchQuantize, RowPlayback, Song,
    Track,
};

use super::follow::{self, FollowOutcome};
use super::quantize;
use super::{MAX_ROWS, RowKey, RowPhase, RowSourceTable, RowTimeSource, is_positive};

// 列 (シーン) のフォローアクション: 走行状態 `SceneRun` と張り / 張り直し / 発火。
// 行のそれ (`RowRuntime` / `arm_timers` / `apply_follow`) と対の、独立した寿命。
mod scene_follow;
use scene_follow::{SceneRun, scene_longest};

// 行のセル列 / 発火判断用のセル (`RowCells` / `CellRef`)。
mod cells;
pub use cells::{CellRef, RowCells};

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
    /// セルを **セル内の拍 `phase_beats` から** 鳴らす (ピアノロールの `f`)。
    /// [`LaunchMode`] を見ない — Toggle の停止 / Gate の握りは起きない。
    CellFrom { key: RowKey, clip_id: u32, phase_beats: f64 },
    /// セルを鳴らしている全行を、それぞれのセル内の拍 `phase_beats` へ揃える。
    /// 停止中 / アレンジ主導の行は触らない。
    RephaseRunning { phase_beats: f64 },
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
    /// セルのどの拍から鳴らすか (セルの `start_beat` からの拍、`0` = 頭)。
    /// [`LaunchRequest::CellFrom`] だけが `0` 以外を置く。
    start_phase: f64,
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
    /// フォローアクションを解決する**対象のセル** (`0` = 予定なし)。
    ///
    /// `phase` とは **別の寿命**。ワンショットが終端で無音になっても、その
    /// セルのフォローアクション (Unlinked の時間 / 倍率 2 以上) はまだ効くので、
    /// 「鳴っている / いない」と「フォローの予定」を 1 つの値で表さない。
    follow_clip_id: u32,
    /// 次にフォローアクションを解決する song 拍 (`INFINITY` = 予定なし)。
    follow_at: f64,
    /// `follow_at` を導いたときのセルの設定。`Song` 側で変わったら ([`LauncherRuntime::resync_cells`])
    /// 走行中でも張り直す — 鳴っているセルにインスペクタで「次のセル」を付けても、
    /// 撃ち直すまで一度も発火しなかった。
    armed_follow: FollowAction,
    /// ワンショットが終わる song 拍 (`INFINITY` = ループする / 鳴っていない)。
    end_at: f64,
    /// [`LaunchMode::Gate`] / [`LaunchMode::Repeat`] で押されているセル (0 = なし)。
    held_clip_id: u32,
    /// [`LaunchMode::Repeat`] で押しっぱなし。
    repeating: bool,
    /// この buffer の song snapshot に実在した。
    alive: bool,
    /// **直近の `Song` snapshot でこの行に書かれていた主導権。**
    ///
    /// `Song` 側の値が変わったこと ([`LauncherRuntime::sync_saved_rows`]) を
    /// 検出するためだけに持つ。走行位置ではないので `phase` とは別物 —
    /// フォローアクションで移った先は `phase` にしか出ない (§1.4)。
    seeded: RowPlayback,
}

impl RowRuntime {
    fn new(key: RowKey) -> Self {
        Self {
            key,
            phase: RowPhase::Arranger,
            queued: None,
            follow_clip_id: 0,
            follow_at: f64::INFINITY,
            armed_follow: FollowAction::default(),
            end_at: f64::INFINITY,
            held_clip_id: 0,
            repeating: false,
            alive: true,
            // 新しく現れた行は「アレンジだった」ところから差分を取る (= `Song` が
            // ランチャー主導なら、その行は現れた瞬間に撃ち直される)。
            seeded: RowPlayback::Arranger,
        }
    }
}

/// **「この発火は何拍で起きるか」の解き方。** ユーザー操作と、フォローアクションの
/// 連鎖で規則が違う (計画書 §2.3) ので、分岐をここ 1 か所に閉じる。
#[derive(Debug, Clone, Copy)]
enum FireAt {
    /// ユーザーが押した — グローバル量子化 (とセル自身の量子化) に従う。
    User { start_beat: f64, global_q: LaunchQuantize, playing: bool },
    /// フォローアクションの連鎖 — グローバル量子化を**迂回**し、`fire` 以降で
    /// セル自身の量子化にだけ従う。
    Chain { fire: f64 },
}

impl FireAt {
    /// 量子化 `cell_q` のセルが実際に鳴り出す song 拍。
    fn beat(self, cell_q: LaunchQuantize, time_sig: (u8, u8)) -> f64 {
        match self {
            // 停止中は量子化しない (拍が進まないので待っても永久に来ない)。
            Self::User { playing: false, .. } => f64::NEG_INFINITY,
            Self::User { start_beat, global_q, playing: true } => {
                match quantize::resolve(cell_q, global_q, time_sig) {
                    Some(q) => quantize::next_boundary(start_beat, q),
                    None => start_beat,
                }
            }
            // **基準は buffer 先頭ではなく「予定された拍」。** buffer 先頭へ丸めると
            // 遷移位置が device buffer size 依存になり、live と書き出しで食い違う
            // (Q9「今聴こえている通りに書き出す」が成立しない)。
            Self::Chain { fire } => match quantize::resolve(cell_q, LaunchQuantize::Off, time_sig)
            {
                Some(q) => quantize::next_boundary(fire, q),
                None => fire,
            },
        }
    }

    /// 連鎖の起点拍 (`None` = ユーザー操作)。
    fn chain_beat(self) -> Option<f64> {
        match self {
            Self::Chain { fire } => Some(fire),
            Self::User { .. } => None,
        }
    }

    /// この発火の瞬間、transport が走っていたか。
    ///
    /// 走行状態 (`RowPhase::Cell`) は停止で消えない (計画書 §0) ので、`phase` だけ
    /// では「鳴っている」と「停止したまま握っている」が区別できない。Toggle の
    /// 「もう一度押したら止める」はこの区別が要る。連鎖 (フォローアクション) は
    /// 拍が進んでいる間しか起きないので常に `true`。
    fn is_playing(self) -> bool {
        match self {
            Self::User { playing, .. } => playing,
            Self::Chain { .. } => true,
        }
    }
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
    /// 直近の `Song` snapshot の `last_launched_scene_id`。行の
    /// [`RowRuntime::seeded`] と同じ役割 (列版の差分検出)。
    seeded_scene: u32,
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
            scene: SceneRun::NONE,
            seeded_scene: 0,
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

    /// **全行を即座に無音へ落とし、予定を全部捨てる。**
    ///
    /// 書き出しの tail 区間 (`playhead >= write_end`) で使う。走査は減衰を測るために
    /// 曲末を越えて 10 秒まで続くが、ループするセルは曲末と無関係に鳴り続けるので、
    /// 黙らせないと **tail-silence 判定が永久に立たず WAV が必ず 10 秒伸びて末尾に
    /// ループがそのまま入る**。sink 側で `write_end` を切ると減衰まで消えるので、
    /// 「鳴らすのをやめる」側で解く (= 停止したときに live で起きることと同じ)。
    ///
    /// `queue_all(Stop)` ではなくここで直接畳むのは、予約はグローバル量子化の
    /// 境界まで待つ = tail の途中まで鳴り続けてしまうため。
    pub fn silence_all(&mut self) {
        self.inbox.clear();
        self.disarm_scene();
        for row in &mut self.rows {
            row.phase = RowPhase::Silent;
            row.queued = None;
            row.follow_clip_id = 0;
            row.follow_at = f64::INFINITY;
            row.end_at = f64::INFINITY;
            row.held_clip_id = 0;
            row.repeating = false;
        }
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
            self.disarm_scene();
            self.seeded_scene = 0;
            self.last_project_id = song.project_id;
            self.reseed = true;
        }
        self.sync_rows(song);
        self.resync_cells(song, span.start_beat);
        if self.reseed {
            self.seed_from_song(song, span.start_beat);
            self.reseed = false;
        }
        // ユーザー操作 (`inbox`) を先に予約へ畳み、**その後で** `Song` 側の主導権が変わった行を
        // 撃ち直す (undo / redo / 保存版へ戻す / recovery)。順序が要点:
        // - `resync_cells` の後 — セルの差し替え / 消失を先に畳んでおかないと、`Song` と
        //   `phase` の食い違いが「変わった」に見えて毎 buffer 撃ち直す
        // - `drain_inbox` の後 — GUI は `LaunchScene` / `LaunchCell` を撃った直後に `Song`
        //   (`LoadSong`) を書くので、compile が 1 buffer に収まると **操作とその反響が同じ
        //   `update` に届く**。反響を先に見ると `sync_saved_rows` の「予約が生きている行は
        //   撃ち直さない」契約が成立せず、量子化を待たずに即 seed → 境界でもう一度頭出し
        //   (= 撃つたびに先頭 ~半小節が 2 回鳴る)。先に予約を作ってから反響を比べれば、
        //   同じ buffer に届いたユーザー操作は差分より新しいので必ず勝つ。
        self.drain_inbox(song, span, global_q, playing);
        self.sync_saved_rows(song, span.start_beat);
        self.resync_scene_follow(song, span.start_beat);
        self.tick_scene_follow(song, span);
        self.build_table(song, span, global_q);
        &self.table
    }

    /// 走行状態を publish する (表示専用、`Song` には入れない)。
    ///
    /// `beat` は **`SharedState::playhead` と同じ瞬間の song 拍**を渡すこと。
    /// GUI は publish 値 (`launch_beat`) と `playhead` を組にして位相を解くので、
    /// 2 つが別の時間軸を指した 1 フレームは `cell_phase` が `None` を返し、
    /// **ランチャー行の映像 / 画像 / 字幕とピアノロールの再生線がまるごと消える**
    /// (ループ巻き戻し直後の buffer で実際に起きていた)。呼び側は transport の
    /// advance / 巻き戻しを済ませてからここへ来る。
    pub fn publish(&self, bridge: &common::audio_bridge::AudioBridgeHandle, beat: f64) {
        use common::audio_bridge as ab;
        for (slot, row) in self.rows.iter().enumerate() {
            let (state, clip_id, progress, launch_beat) = row_display(row.phase, beat);
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
        // 行の集合の定義は [`for_each_launcher_row`] 1 本 (マスター行を含む /
        // テンポ・拍子レーンを含まない)。ここで自前に走査を書くと、列の占有判定や
        // 長さの数え上げと静かに食い違う。
        for_each_launcher_row(song, |key, _| self.touch(key));
        self.rows.retain(|r| r.alive);
    }

    /// 鳴っているセルを **`Song` の現在値へ合わせ直す** (毎 buffer)。
    ///
    /// 走行状態が持つのは位相の原点 (`launch_beat`) だけで、セルの長さ /
    /// ループ有無 / 拍原点の SSoT は `Song`。撃った瞬間の値を持ち回ると、
    /// インスペクタで「ローンチ」の長さやループを変えた瞬間から **GUI の進捗バーと
    /// 音の周期が食い違う** (GUI は毎フレーム `Song` から解き直している)。
    ///
    /// 同じ場所で 2 つの事故も始末する:
    /// - **id ごと置き換わったセル** — GUI がセルの上へ別のクリップを落とすと、その
    ///   列のセルは新しい id で作り直され (`Track::put_session_clip`)、`Song` 側の
    ///   主導権も新 id へ移る。位相を保ったまま乗り換えるので無音を挟まない。
    /// - **消えたセル** — 指したまま走ると MIDI もオーディオも全部 skip されて
    ///   **無音のまま PLAYING を publish し続ける** (どのセルも光らず進捗も出ない)。
    ///
    /// RT 安全: 線形走査のみ (確保・ロック・I/O なし)。
    fn resync_cells(&mut self, song: &Song, now: f64) {
        for row in &mut self.rows {
            let RowPhase::Cell { clip_id, launch_beat, .. } = row.phase else {
                continue;
            };
            let Some((cells, saved)) = row_of(song, row.key) else {
                continue;
            };
            if let Some(cell) = cells.find_by_clip(clip_id) {
                let want = cell.phase_at(launch_beat, None);
                if want != row.phase {
                    // **`end_at` だけ張り直す。** 長さの変更で `follow_at` は触らない —
                    // `launch_beat` 起点で張り直すと過去の拍になり、フォローアクションが
                    // その場で暴発する。周期の変更は次の発火から効けば足りる。
                    row.phase = want;
                    row.end_at = one_shot_end(want);
                }
                // **フォローアクションの設定** が変わったときだけは張り直す (SSoT は `Song`)。
                // 鳴っているセルに後から「次のセル」を付けるのは普通の操作で、撃ち直すまで
                // 効かないのは「効かない」に見える。暴発は `next_due_beat` が `now` 以降の
                // 周目へ進めることで防ぐ。
                if cell.follow != row.armed_follow {
                    rearm_follow(row, &cell, now);
                }
                continue;
            }
            if let RowPlayback::Launcher { clip_id: want } = saved
                && let Some(cell) = cells.find_by_clip(want)
            {
                row.phase = cell.phase_at(launch_beat, None);
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
                continue;
            }
            // 本当に消えた。停止 (`Silent`) へ落とし、その行の予定も全部捨てる。
            row.phase = RowPhase::Silent;
            row.queued = None;
            row.held_clip_id = 0;
            row.repeating = false;
            arm_timers(row, &cells);
        }
    }

    /// 列のフォローアクションの走行状態を捨てる。
    ///
    /// **「全行の走行状態をリセットする操作」と同じ寿命**にする — プロジェクト
    /// 切替 / 再生開始 (reseed) / 全停止 / 全行アレンジ復帰。1 行だけの停止・
    /// アレンジ復帰では捨てない (他の行はまだその列を鳴らしているので、列の
    /// 連鎖はまだ生きている)。残したまま全停止すると、止めたはずの全行が
    /// `scene.at` に到達した瞬間に勝手に鳴り出す。
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
        // **行と列を同じ 1 か所で起点へ戻す。** 列の走行位置は `Song` に無い
        // (§1.4) ので、前の再生で armed だった `scene.at` を残すと「どこで
        // 停止したか」で次のシーンへ移る拍が変わる = 同じ起点から始まらない。
        self.disarm_scene();
        for row in &mut self.rows {
            match row_of(song, row.key) {
                Some((cells, saved)) => seed_row(row, &cells, saved, now),
                // ランチャーが握れない行 (テンポ / 拍子レーン) と、消えた行。
                None => seed_row(row, &RowCells::Track(&[]), RowPlayback::Arranger, now),
            }
        }
        // 列の連鎖の起点は `Song.last_launched_scene_id` (= ユーザーが最後に撃った列)。
        // **行の起点 (`RowPlayback`) と対**で、これを読まないと停止 → 再生や
        // 書き出しでシーンのフォローアクションが一度も動かない (§1.4 / Q9)。
        // 遷移先は決して読まない — 書く側 (GUI) が書かないので、ここも「撃った列」
        // としてしか解釈しない。
        self.seeded_scene = song.last_launched_scene_id;
        if song.last_launched_scene_id != 0 {
            let longest = scene_longest(song, song.last_launched_scene_id);
            self.arm_scene_follow(song, song.last_launched_scene_id, now, longest, now);
        }
    }

    /// **`Song` 側の主導権が変わった行だけを撃ち直す** (計画書 §1.4 の裏返し)。
    ///
    /// `Song.launcher` は「ユーザーが最後に撃った起点」の SSoT で、GUI が
    /// undo / redo / 保存版へ戻す / recovery でそれを丸ごと差し替える。プロジェクト
    /// 切替 (`project_id`) と再生の立ち上がり ([`Self::arm_reseed`]) しか見ていないと、
    /// これらは **engine に一度も届かない** (音だけ差し替え前のまま鳴り続ける)。
    ///
    /// 行単位なのが要点 — 全行を撃ち直すと「鳴っている行を触らない編集」でも
    /// 全部の位相が飛ぶ。
    ///
    /// # 順序契約 (GUI との約束)
    ///
    /// GUI は発火の瞬間に `LaunchCell` / `StopRow` / `SwitchRowToArranger` を **先に**
    /// 撃ち、`LoadSong` はその直後に届く。したがって「たった今の操作の反響」を
    /// 撃ち直しと取り違えないよう、次の 2 つは値だけ取り込んで撃ち直さない:
    ///
    /// 1. **予約が生きている行** — その予約こそが `Song` の新しい値を作った操作。
    /// 2. **現在の供給元が既に新しい値を実現している行** — 予約が発火し終えてから
    ///    `LoadSong` が届いた場合 (off-thread compile の分だけ必ず遅れる)。
    ///    ここを見ないと、撃った直後に毎回 1 回位相が飛ぶ。
    ///
    /// RT 安全: 線形走査のみ (確保・ロック・I/O なし)。
    fn sync_saved_rows(&mut self, song: &Song, now: f64) {
        for idx in 0..self.rows.len() {
            let key = self.rows[idx].key;
            let Some((cells, saved)) = row_of(song, key) else {
                self.rows[idx].seeded = RowPlayback::Arranger;
                continue;
            };
            if self.rows[idx].seeded == saved {
                continue;
            }
            self.rows[idx].seeded = saved;
            if self.rows[idx].queued.is_some() || realizes(self.rows[idx].phase, saved) {
                continue;
            }
            seed_row(&mut self.rows[idx], &cells, saved, now);
        }
        // 列の連鎖の起点も同じ規則で追う (行と対の SSoT)。既にその列を走らせて
        // いるなら「反響」なので張り直さない。
        if song.last_launched_scene_id == self.seeded_scene {
            return;
        }
        self.seeded_scene = song.last_launched_scene_id;
        if self.scene.scene_id == song.last_launched_scene_id {
            return;
        }
        match song.last_launched_scene_id {
            0 => self.disarm_scene(),
            id => {
                let longest = scene_longest(song, id);
                self.arm_scene_follow(song, id, now, longest, now);
            }
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
        let at = FireAt::User { start_beat: span.start_beat, global_q, playing };
        let global_at = at.beat(LaunchQuantize::Global, song.time_sig);
        match req {
            LaunchRequest::Cell { key, clip_id, pressed } => {
                self.press_cell(song, at, key, clip_id, pressed);
            }
            LaunchRequest::CellFrom { key, clip_id, phase_beats } => {
                self.launch_cell_from(song, at, key, clip_id, phase_beats);
            }
            LaunchRequest::RephaseRunning { phase_beats } => {
                self.rephase_running(song, at, phase_beats);
            }
            LaunchRequest::Scene { scene_id, pressed } => {
                if pressed {
                    self.launch_scene(song, at, scene_id, span.start_beat);
                } else {
                    self.release_scene(song, at, scene_id);
                }
            }
            // **1 行だけの操作は列の連鎖を解除しない** ([`Self::disarm_scene`])。
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
    fn press_cell(
        &mut self,
        song: &Song,
        fire: FireAt,
        key: RowKey,
        clip_id: u32,
        pressed: bool,
    ) {
        let Some((cells, _)) = row_of(song, key) else { return };
        let Some(cell) = cells.find_by_clip(clip_id) else { return };
        let at = fire.beat(cell.quantize, song.time_sig);
        if !pressed {
            self.release_cell(key, clip_id, cell.mode, at);
            return;
        }
        // 停止中に撃ったなら「鳴っているセル」は無い ([`FireAt::is_playing`])。
        // 停止で `phase` は `Cell` のまま残るので、これを見ないと Toggle の
        // 停止 → ▶ が停止予約になり、その行だけ 1 回鳴らない。GUI 側
        // (`AppData::launch_cell`) も同じ条件で `Song` を書く。
        let playing_now = fire.is_playing()
            && self
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
                    start_phase: 0.0,
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
    fn launch_scene(&mut self, song: &Song, fire: FireAt, scene_id: u32, now: f64) {
        let stop_at = fire.beat(LaunchQuantize::Global, song.time_sig);
        // **掴んだ記録はユーザーが押したときだけ。** 連鎖 (フォローアクション) には
        // 対になる「離し」が来ないので、`Repeat` のセルへ連鎖すると誰も止められない
        // 撃ち直しが走り続ける。
        let user_press = fire.chain_beat().is_none();
        let mut longest = 0.0_f64;
        let mut first = f64::INFINITY;
        for i in 0..self.rows.len() {
            let key = self.rows[i].key;
            let Some((cells, _)) = row_of(song, key) else { continue };
            match cells.find_by_scene(scene_id) {
                Some(cell) => {
                    let at = fire.beat(cell.quantize, song.time_sig);
                    longest = longest.max(cell.length_beats);
                    first = first.min(at);
                    self.queue(key, QueueTarget::Cell(cell.clip_id), at, cell.legato, false);
                    // **列の ▶ もセルの ▶ と同じく「押している」。** 掴んだセルを
                    // 記録しないと、離しの解釈 (`Gate` は停止 / `Repeat` は撃ち直しを
                    // 止める) が `release_cell` の held 照合で弾かれ、同じセルが
                    // 撃ち方によって挙動を変える。
                    if user_press {
                        self.rows[i].held_clip_id = cell.clip_id;
                        self.rows[i].repeating = cell.mode == LaunchMode::Repeat;
                    }
                }
                None => {
                    first = first.min(stop_at);
                    self.queue(key, QueueTarget::Stop, stop_at, false, false);
                    if user_press {
                        self.rows[i].held_clip_id = 0;
                        self.rows[i].repeating = false;
                    }
                }
            }
        }
        // 連鎖は **予定された拍そのもの**を起点にする (各行の量子化で丸めた最小値だと
        // 粒度と buffer 位置に依存してずれが積もる)。ユーザー操作では「最初に
        // 鳴り出す拍」= 列が始まった拍。
        let base = fire.chain_beat().unwrap_or(first);
        self.arm_scene_follow(song, scene_id, base, longest, now);
    }

    /// 列の ▶ を離した。押下で掴んだセルに、セルの ▶ と**同じ解釈**を掛ける。
    ///
    /// 撃ち方 (セルの ▶ / 列の ▶ / MIDI パッド) で `LaunchMode` の意味が変わらない
    /// ように、判断は [`Self::release_cell`] 1 本を共有する。
    fn release_scene(&mut self, song: &Song, fire: FireAt, scene_id: u32) {
        for i in 0..self.rows.len() {
            let (key, held) = (self.rows[i].key, self.rows[i].held_clip_id);
            if held == 0 {
                continue;
            }
            let Some((cells, _)) = row_of(song, key) else { continue };
            let Some(cell) = cells.find_by_scene(scene_id) else { continue };
            if cell.clip_id != held {
                continue; // その後に別のセルを撃った行は、列の離しの対象ではない。
            }
            let at = fire.beat(cell.quantize, song.time_sig);
            self.release_cell(key, held, cell.mode, at);
        }
    }

    /// 予約を置く (行ごとに高々 1 件、新しい発火が前を置き換える)。
    fn queue(&mut self, key: RowKey, target: QueueTarget, at: f64, legato: bool, rep: bool) {
        if let Some(row) = self.rows.iter_mut().find(|r| r.key == key) {
            row.queued =
                Some(Queued { target, at_beat: at, legato, from_repeat: rep, start_phase: 0.0 });
        }
    }

    /// [`LaunchRequest::CellFrom`]: セルを `phase_beats` の位置から鳴らす予約。
    ///
    /// [`Self::press_cell`] と違い [`LaunchMode`] を見ない (Toggle の「もう一度
    /// 押したら止める」/ Gate の握り / Repeat の撃ち直しは起きない) — 「ここから
    /// 鳴らせ」は押下 / 離しの対を持たない操作なので、鳴っているセルを止める
    /// 解釈が入る余地は無い。量子化はセル自身の設定に従う (Live のクリップビューの
    /// 頭出しと同じ)。位相の折り返しは発火時 ([`CellRef::phase_from`]) に行う。
    fn launch_cell_from(
        &mut self,
        song: &Song,
        fire: FireAt,
        key: RowKey,
        clip_id: u32,
        phase_beats: f64,
    ) {
        let Some((cells, _)) = row_of(song, key) else { return };
        let Some(cell) = cells.find_by_clip(clip_id) else { return };
        let at = fire.beat(cell.quantize, song.time_sig);
        if let Some(row) = self.rows.iter_mut().find(|r| r.key == key) {
            row.queued = Some(Queued {
                target: QueueTarget::Cell(clip_id),
                at_beat: at,
                legato: false,
                from_repeat: false,
                start_phase: if phase_beats.is_finite() { phase_beats.max(0.0) } else { 0.0 },
            });
        }
    }

    /// [`LaunchRequest::RephaseRunning`]: セルを鳴らしている全行を、それぞれのセル内の
    /// 拍 `phase_beats` へ揃える (「全体をカーソルの拍から」)。
    ///
    /// 対象は供給元がセル (`RowPhase::Cell`) の行だけ — 停止中 (Silent) と
    /// アレンジ主導の行は触らない (アレンジ側は同時に届く `SeekTo` が動かす)。
    /// 停止中の transport で `Cell` のまま残っている行も対象 (再生を始めた瞬間に
    /// その拍から鳴る)。既に予約が生きている行 (量子化待ち / 列の発火) は、その予約の
    /// 行き先と発火拍を保ったまま位相だけ載せる — ここで予約を置き換えると、直前に
    /// 撃った別のセルへの遷移が消える。
    ///
    /// RT 安全: 行の線形走査のみ。
    fn rephase_running(&mut self, song: &Song, fire: FireAt, phase_beats: f64) {
        let start_phase = if phase_beats.is_finite() { phase_beats.max(0.0) } else { 0.0 };
        for idx in 0..self.rows.len() {
            if let Some(q) = &mut self.rows[idx].queued {
                if matches!(q.target, QueueTarget::Cell(_)) {
                    q.start_phase = start_phase;
                }
                continue;
            }
            let RowPhase::Cell { clip_id, .. } = self.rows[idx].phase else { continue };
            let key = self.rows[idx].key;
            let Some((cells, _)) = row_of(song, key) else { continue };
            let Some(cell) = cells.find_by_clip(clip_id) else { continue };
            let at = fire.beat(cell.quantize, song.time_sig);
            self.rows[idx].queued = Some(Queued {
                target: QueueTarget::Cell(clip_id),
                at_beat: at,
                legato: false,
                from_repeat: false,
                start_phase,
            });
        }
    }

    /// **全行**の予約を置き換える (全停止 / 全行アレンジ復帰)。列の連鎖も一緒に
    /// 解除する — 走行状態のリセットは行と列で 1 つの操作 ([`Self::disarm_scene`])。
    fn queue_all(&mut self, target: QueueTarget, at: f64) {
        self.disarm_scene();
        for row in &mut self.rows {
            row.queued = Some(Queued {
                target,
                at_beat: at,
                legato: false,
                from_repeat: false,
                start_phase: 0.0,
            });
            row.held_clip_id = 0;
            row.repeating = false;
        }
    }

    /// 全行の供給元を解いてテーブルへ詰める。
    ///
    /// **レーン行は 1 本も飛ばさない。** 消費側 (`fill_pd_param_events` /
    /// `fill_track_param_ramps` / send gain) は `automation_lanes` / `song_lanes` を
    /// `enumerate()` した index で引くので、行にならないレーン (テンポ / 拍子) も
    /// 席だけは要る。`solve` は登録されていない行を `Arranger` に倒すので、
    /// 席は自然に埋まる。
    fn build_table(&mut self, song: &Song, span: BufferSpan, global_q: LaunchQuantize) {
        self.table.clear();
        for track in &song.tracks {
            self.table.begin_track();
            let src = self.solve(song, span, global_q, RowKey::track(track.id));
            self.table.push(src);
            for lane in &track.automation_lanes {
                let src = self.solve(song, span, global_q, RowKey::lane(track.id, lane.id));
                self.table.push(src);
            }
        }
        // マスター行 (`song_lanes`) は最後のグループへ。トラック行は積まない
        // (`RowSourceTable::track_rows` がそれを知っている唯一の場所)。
        self.table.begin_master();
        for lane in &song.song_lanes {
            let key = RowKey::lane(common::model::MASTER_TRACK_ID, lane.id);
            let src = self.solve(song, span, global_q, key);
            self.table.push(src);
        }
        // `row_at` が「次のトラックの先頭」で範囲外を判定できるよう番兵を置く。
        self.table.begin_track();
    }

    /// 1 行を 1 buffer 分解く。遷移は高々 1 回 ([`RowTimeSource`] の doc)。
    ///
    /// 走行状態と `Song` の突き合わせ (消えたセル / 差し替わったセル / 長さの変更) は
    /// [`Self::resync_cells`] が buffer の頭で済ませているので、ここは出来事の
    /// 適用だけを見る。
    fn solve(
        &mut self,
        song: &Song,
        span: BufferSpan,
        global_q: LaunchQuantize,
        key: RowKey,
    ) -> RowTimeSource {
        let Some(idx) = self.rows.iter().position(|r| r.key == key) else {
            return RowTimeSource::uniform(key, RowPhase::Arranger);
        };
        let Some((cells, _)) = row_of(song, key) else {
            return RowTimeSource::uniform(key, self.rows[idx].phase);
        };
        let head = self.rows[idx].phase;
        let Some((at, kind)) = self.next_event(idx, span) else {
            return RowTimeSource::uniform(key, head);
        };
        let fire = at.max(span.start_beat);
        let launch_at = if at.is_finite() { at } else { fire };
        let tail = self.apply_event(song, idx, &cells, kind, launch_at, fire, global_q);
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
    #[allow(clippy::too_many_arguments)]
    fn apply_event(
        &mut self,
        song: &Song,
        idx: usize,
        cells: &RowCells<'_>,
        kind: EventKind,
        at: f64,
        fire: f64,
        global_q: LaunchQuantize,
    ) -> RowPhase {
        match kind {
            EventKind::Queued => {
                let Some(q) = self.rows[idx].queued.take() else {
                    return self.rows[idx].phase;
                };
                self.enter(idx, cells, q.target, at, q.legato, q.start_phase, global_q, song.time_sig)
            }
            // **ワンショットの終端は「鳴り終わった」だけ。** フォローの予定
            // (`follow_clip_id` / `follow_at`) は別の寿命なのでそのまま残す —
            // ここで捨てると「2 拍のセルを 4 拍後に次のセルへ」(Unlinked、または
            // Linked の倍率 2 以上) が二度と発火せず、行が無音で固まる。
            EventKind::OneShotEnd => {
                self.rows[idx].phase = RowPhase::Silent;
                self.rows[idx].end_at = f64::INFINITY;
                RowPhase::Silent
            }
            EventKind::Follow => self.apply_follow(song, idx, cells, fire, global_q),
        }
    }

    /// クリップのフォローアクションを 1 回解決する。
    ///
    /// **`follow_at` の所有者はここと [`arm_timers`] だけ。** 入口で必ず捨ててから
    /// 各分岐が「必要なら張り直す」形にしてある — 予約だけ置いて抜ける経路
    /// (量子化を持つ飛び先セル) で古い値が残ると、毎 buffer 同じフォローが再発火し、
    /// 予約は常に 1 つ先の境界へ逃げ続けて**行が永久に進まない**。
    ///
    /// 追う対象は `phase` ではなく `follow_clip_id` — ワンショットが終端で無音に
    /// なった後もそのセルのフォローアクションは効く (別の寿命)。
    fn apply_follow(
        &mut self,
        song: &Song,
        idx: usize,
        cells: &RowCells<'_>,
        fire: f64,
        global_q: LaunchQuantize,
    ) -> RowPhase {
        let phase = self.rows[idx].phase;
        let clip_id = self.rows[idx].follow_clip_id;
        self.rows[idx].follow_clip_id = 0;
        self.rows[idx].follow_at = f64::INFINITY;
        let Some(cell) = cells.find_by_clip(clip_id) else {
            // 追う対象が消えた。鳴っていれば `resync_cells` が既に無音へ落としている。
            return phase;
        };
        let n = fill_row_occupancy(&mut self.occupied, song, cells);
        let from = song.scenes.iter().position(|s| s.id == cell.scene_id).unwrap_or(0);
        let seed = follow::row_seed(self.rows[idx].key.packed(), clip_id);
        let outcome =
            follow::resolve(&cell.follow, &self.occupied[..n], from, &song.scenes, seed, fire);
        match outcome {
            FollowOutcome::Keep => {
                // 鳴り続ける (位相は動かさない)。次の発火だけ張り直す。
                if let Some(at) = follow::due_beat(&cell.follow, fire, cell.length_beats) {
                    self.rows[idx].follow_clip_id = clip_id;
                    self.rows[idx].follow_at = at;
                }
                phase
            }
            FollowOutcome::Stop => self.set_phase(idx, RowPhase::Silent, cells),
            FollowOutcome::Go(scene_idx) => {
                let Some(scene_id) = song.scenes.get(scene_idx).map(|s| s.id) else {
                    return self.set_phase(idx, RowPhase::Silent, cells);
                };
                self.go_to_scene(idx, cells, song, scene_id, fire, phase, global_q)
            }
        }
    }

    /// フォローアクションで列 `scene_id` のセルへ移る。
    ///
    /// 計画書 §2.3: 発火は**グローバル量子化を迂回する**が、**飛び先セル自身の
    /// `quantize` には従う** (1/4 量子化のセルへ飛ぶなら拍の頭で切り替わる)。
    /// 空セルの列へ飛んだら停止 (Q11)。
    #[allow(clippy::too_many_arguments)]
    fn go_to_scene(
        &mut self,
        idx: usize,
        cells: &RowCells<'_>,
        song: &Song,
        scene_id: u32,
        fire: f64,
        phase: RowPhase,
        global_q: LaunchQuantize,
    ) -> RowPhase {
        let Some(next) = cells.find_by_scene(scene_id) else {
            return self.set_phase(idx, RowPhase::Silent, cells);
        };
        let (id, legato) = (next.clip_id, next.legato);
        let at = FireAt::Chain { fire }.beat(next.quantize, song.time_sig);
        if at > fire {
            // 飛び先の量子化境界まで待つ。フォロータイマーは入口で捨ててあるので、
            // この行は「予約 1 件だけを持つ」状態になる。
            let key = self.rows[idx].key;
            self.queue(key, QueueTarget::Cell(id), at, legato, false);
            return phase;
        }
        self.enter(idx, cells, QueueTarget::Cell(id), fire, legato, 0.0, global_q, song.time_sig)
    }

    /// 予約 / フォローアクションの行き先へ実際に入る。
    ///
    /// `start_phase` はセルのどの拍から鳴らすか ([`Queued::start_phase`])。`0` なら
    /// 頭 (Legato ならその位相引き継ぎ)、それ以外は [`CellRef::phase_from`] で
    /// 起点を過去へ置いて「途中から」にする。
    #[allow(clippy::too_many_arguments)]
    fn enter(
        &mut self,
        idx: usize,
        cells: &RowCells<'_>,
        target: QueueTarget,
        at: f64,
        legato: bool,
        start_phase: f64,
        global_q: LaunchQuantize,
        time_sig: (u8, u8),
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
        let phase = if start_phase > 0.0 {
            cell.phase_from(at, start_phase)
        } else {
            cell.phase_at(at, carry)
        };
        let out = self.set_phase(idx, phase, cells);
        if self.rows[idx].repeating
            && self.rows[idx].held_clip_id == clip_id
            && let Some(q) = repeat_queue(target, at, &cell, global_q, time_sig)
        {
            self.rows[idx].queued = Some(q);
        }
        out
    }

    /// 供給元を差し替え、タイマーを張り直す。**行が新しい供給元へ入る唯一の口**
    /// なので、`follow_at` / `end_at` の張り直しもここに束ねる。
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

/// **ランチャーの行を 1 本の規則で数え上げる。**
///
/// 行の登録 ([`LauncherRuntime::sync_rows`])・列の占有判定
/// ([`LauncherRuntime::fill_scene_occupancy`])・列の長さ ([`scene_longest`]) が
/// 同じ集合を見るように、走査はここだけが持つ。集合は計画書 Q4 のとおり
/// 「通常トラック行 + 展開したオートメーションレーン行 + マスター行
/// (`Song.song_lanes`)」で、テンポ / 拍子レーンだけが外れる
/// ([`common::model::AutomationTarget::accepts_launcher_cells`] が SSoT)。
///
/// RT 安全: 線形走査のみ (確保・ロック・I/O なし)。
fn for_each_launcher_row(song: &Song, mut f: impl FnMut(RowKey, RowCells<'_>)) {
    for track in &song.tracks {
        f(RowKey::track(track.id), RowCells::Track(&track.session_clips));
        for lane in &track.automation_lanes {
            if lane.target.accepts_launcher_cells() {
                f(RowKey::lane(track.id, lane.id), RowCells::Lane(&lane.session_clips));
            }
        }
    }
    for lane in &song.song_lanes {
        if lane.target.accepts_launcher_cells() {
            let key = RowKey::lane(common::model::MASTER_TRACK_ID, lane.id);
            f(key, RowCells::Lane(&lane.session_clips));
        }
    }
}

/// 行のセル列と、保存されている主導権。
///
/// **RowKey → セルの解決はここが唯一の口**なので、ランチャーが握れない行
/// (テンポ / 拍子レーン、確定済み設計判断 1) の門番もここに置く。`None` を返せば
/// 発火要求も seed も `Arranger` に倒れる。
fn row_of(song: &Song, key: RowKey) -> Option<(RowCells<'_>, RowPlayback)> {
    if key.track_id == common::model::MASTER_TRACK_ID {
        // マスター行はトラックを持たない (`Song.song_lanes` が実体)。
        let lane = song.song_lanes.iter().find(|l| l.id == key.lane_id)?;
        return lane
            .target
            .accepts_launcher_cells()
            .then(|| (RowCells::Lane(&lane.session_clips), lane.launcher));
    }
    let track: &Track = song.tracks.iter().find(|t| t.id == key.track_id)?;
    if key.lane_id == 0 {
        return Some((RowCells::Track(&track.session_clips), track.launcher));
    }
    let lane: &AutomationLane = track.automation_lanes.iter().find(|l| l.id == key.lane_id)?;
    lane.target
        .accepts_launcher_cells()
        .then(|| (RowCells::Lane(&lane.session_clips), lane.launcher))
}

/// 「この行がその列にセルを持つか」を作業領域へ埋め、有効長を返す。
fn fill_row_occupancy(occupied: &mut [bool], song: &Song, cells: &RowCells<'_>) -> usize {
    let n = song.scenes.len().min(occupied.len());
    for (i, slot) in occupied[..n].iter_mut().enumerate() {
        *slot = cells.find_by_scene(song.scenes[i].id).is_some();
    }
    n
}

/// **`Song` の主導権から行の走行状態を作り直す唯一の口。**
///
/// 再生開始 / プロジェクト切替 / 書き出しの起点 ([`LauncherRuntime::seed_from_song`]) と、
/// undo などによる行単位の差し替え ([`LauncherRuntime::sync_saved_rows`]) が
/// 共有する。2 か所に写すと「停止 → 再生」と「undo」で起点の作り方がズレる。
///
/// **セルが引けない行は無音**に落とす — `Arranger` へ戻すと「ランチャーに渡した行」の
/// アレンジのクリップが黙って鳴り出す (`Song::normalize_session` と同じ規則)。
fn seed_row(row: &mut RowRuntime, cells: &RowCells<'_>, saved: RowPlayback, now: f64) {
    row.queued = None;
    row.held_clip_id = 0;
    row.repeating = false;
    row.seeded = saved;
    row.phase = match saved {
        RowPlayback::Arranger => RowPhase::Arranger,
        RowPlayback::LauncherStopped => RowPhase::Silent,
        RowPlayback::Launcher { clip_id } => cells
            .find_by_clip(clip_id)
            .map_or(RowPhase::Silent, |c| c.phase_at(now, None)),
    };
    arm_timers(row, cells);
}

/// 走行中の供給元が、保存された主導権を**既に実現しているか**。
///
/// `true` なら [`LauncherRuntime::sync_saved_rows`] は撃ち直さない (位相を保つ)。
/// `launch_beat` は比較しない — `Song` は「いつ撃ったか」を持たないので
/// (§1.4)、比較できるのは「どのセルか」だけ。
fn realizes(phase: RowPhase, saved: RowPlayback) -> bool {
    match (phase, saved) {
        (RowPhase::Arranger, RowPlayback::Arranger)
        | (RowPhase::Silent, RowPlayback::LauncherStopped) => true,
        (RowPhase::Cell { clip_id, .. }, RowPlayback::Launcher { clip_id: want }) => {
            clip_id == want
        }
        _ => false,
    }
}

/// 行が新しい供給元へ入った直後のタイマーを張り直す。
///
/// **張る契機は「行が新しい供給元へ入った」1 つだけ**なので、`follow_at` /
/// `end_at` / `follow_clip_id` をここでまとめて所有する。逆に「鳴り終わった」
/// (ワンショットの終端) はここを通さない — `end_at` だけを畳んで、フォローの
/// 予定は残すのが正しい (`apply_event` の `OneShotEnd`)。
fn arm_timers(row: &mut RowRuntime, cells: &RowCells<'_>) {
    row.follow_clip_id = 0;
    row.follow_at = f64::INFINITY;
    row.armed_follow = FollowAction::default();
    row.end_at = one_shot_end(row.phase);
    let RowPhase::Cell { clip_id, launch_beat, loop_len, .. } = row.phase else {
        return;
    };
    let Some(c) = cells.find_by_clip(clip_id) else {
        return;
    };
    row.armed_follow = c.follow.clone();
    if let Some(at) = follow::due_beat(&c.follow, launch_beat, loop_len) {
        row.follow_clip_id = clip_id;
        row.follow_at = at;
    }
}

/// 走行中にセルのフォローアクションの設定が変わった行を張り直す (`resync_cells` から)。
/// 起点 `launch_beat` から周期を刻んで `now` 以降で最初の発火へ (過去へ張ると暴発する)。
fn rearm_follow(row: &mut RowRuntime, cell: &CellRef, now: f64) {
    row.armed_follow = cell.follow.clone();
    row.follow_clip_id = 0;
    row.follow_at = f64::INFINITY;
    let RowPhase::Cell { clip_id, launch_beat, loop_len, .. } = row.phase else {
        return;
    };
    if let Some(at) = follow::next_due_beat(&cell.follow, launch_beat, loop_len, now) {
        row.follow_clip_id = clip_id;
        row.follow_at = at;
    }
}

/// ワンショットが鳴り終わる song 拍 (`INFINITY` = ループする / 鳴っていない)。
#[must_use]
fn one_shot_end(phase: RowPhase) -> f64 {
    match phase {
        RowPhase::Cell { launch_beat, loop_len, looping: false, .. } if is_positive(loop_len) => {
            launch_beat + loop_len
        }
        _ => f64::INFINITY,
    }
}

/// [`LaunchMode::Repeat`] の自動再予約。
///
/// 周期は **量子化の粒度** ([`LaunchMode::Repeat`] の doc が SSoT: 「押している間、
/// 量子化の周期で撃ち直し続ける」)。セル長で撃ち直すと、既定の `looping: true` の
/// セルではもともとセル長で折り返しているので Repeat が Trigger と区別できない。
/// 量子化なし (`Off`) のときだけセル長へ倒す — 格子が無いと周期が決まらないため。
fn repeat_queue(
    target: QueueTarget,
    at: f64,
    cell: &CellRef,
    global_q: LaunchQuantize,
    time_sig: (u8, u8),
) -> Option<Queued> {
    let next = match quantize::resolve(cell.quantize, global_q, time_sig) {
        // `at` は既にその格子の上に居るので、そのまま解くと `at` 自身が返って
        // 同じ拍へ無限に再予約する。半周期ずらして「次の格子点」を取る。
        Some(q) => quantize::next_boundary(at + q * 0.5, q),
        None if is_positive(cell.length_beats) => at + cell.length_beats,
        None => return None,
    };
    (next > at).then_some(Queued {
        target,
        at_beat: next,
        legato: false,
        from_repeat: true,
        start_phase: 0.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{
        AutomationClip, AutomationTarget, Clip, FollowActionKind, LaunchSettings, Scene,
        SessionAutomationClip, SessionClip, Track, TrackBuiltinParam,
    };

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

    fn press_from(rt: &mut LauncherRuntime, track_id: u32, clip_id: u32, phase: f64) {
        rt.push_request(LaunchRequest::CellFrom {
            key: RowKey::track(track_id),
            clip_id,
            phase_beats: phase,
        });
    }

    fn cell_launch_beat(phase: RowPhase) -> f64 {
        let RowPhase::Cell { launch_beat, .. } = phase else { panic!("セルでない: {phase:?}") };
        launch_beat
    }

    /// **`CellFrom` はセルの途中から鳴らす** — 起点 (`launch_beat`) が位相ぶん過去に置かれ、
    /// 実効拍がその位置になる。ループ長を超える位相は折り返す。
    #[test]
    fn セルを途中から撃つと起点が位相ぶん過去に置かれる() {
        let song = two_rows();
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);

        press_from(&mut rt, 1, 10, 1.5);
        step(&mut rt, &song, 1.0);
        let tail = rt.rows().track_row(0).tail;
        assert!((cell_launch_beat(tail) - (1.0 - 1.5)).abs() < 1e-9, "{tail:?}");
        assert_eq!(tail.effective_beat(1.0), Some(1.5), "撃った瞬間にセルの 1.5 拍目");

        // 4 拍ループのセルに 5.5 拍を指せば 1.5 拍目 (折り返し)。
        press_from(&mut rt, 1, 10, 5.5);
        step(&mut rt, &song, 2.0);
        let tail = rt.rows().track_row(0).tail;
        assert!((cell_launch_beat(tail) - (2.0 - 1.5)).abs() < 1e-9, "{tail:?}");
    }

    /// **`CellFrom` は [`LaunchMode`] を見ない。** Toggle のセルが鳴っているときに
    /// 「ここから鳴らせ」と言われても止めず、位相だけ差し替える。
    #[test]
    fn 途中から撃つのは_toggle_で鳴っているセルを止めない() {
        let mut song = two_rows();
        song.tracks[0].session_clips[0].launch.mode = LaunchMode::Toggle;
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        press(&mut rt, 1, 10, true);
        step(&mut rt, &song, 0.0);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(10));

        press_from(&mut rt, 1, 10, 3.0);
        step(&mut rt, &song, 2.0);
        let tail = rt.rows().track_row(0).tail;
        assert_eq!(tail.cell_clip_id(), Some(10), "Toggle の停止として解釈した: {tail:?}");
        assert!((cell_launch_beat(tail) - (2.0 - 3.0)).abs() < 1e-9, "{tail:?}");
    }

    /// **`RephaseRunning` はセルを鳴らしている全行を同じ拍へ揃える。** 停止中 (Silent) の
    /// 行は触らず、予約が生きている行はその予約に位相を載せる (行き先は変えない)。
    #[test]
    fn 鳴っている全行を同じ拍へ揃える() {
        let song = two_rows();
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        press(&mut rt, 1, 10, true);
        press(&mut rt, 2, 20, true);
        step(&mut rt, &song, 0.0);
        // track 2 は止めておく (Silent)。
        rt.push_request(LaunchRequest::StopRow { key: RowKey::track(2) });
        step(&mut rt, &song, 1.0);
        assert_eq!(rt.rows().track_row(1).tail, RowPhase::Silent);

        rt.push_request(LaunchRequest::RephaseRunning { phase_beats: 2.5 });
        step(&mut rt, &song, 3.0);
        let t1 = rt.rows().track_row(0).tail;
        assert!((cell_launch_beat(t1) - (3.0 - 2.5)).abs() < 1e-9, "{t1:?}");
        assert_eq!(rt.rows().track_row(1).tail, RowPhase::Silent, "止めた行を鳴らした");

        // 予約が生きている行: 行き先 (11) はそのまま、位相だけ載る。
        rt.push_request(LaunchRequest::Cell { key: RowKey::track(1), clip_id: 11, pressed: true });
        rt.push_request(LaunchRequest::RephaseRunning { phase_beats: 1.0 });
        step(&mut rt, &song, 4.0);
        let t1 = rt.rows().track_row(0).tail;
        assert_eq!(t1.cell_clip_id(), Some(11), "予約の行き先が消えた: {t1:?}");
        assert!((cell_launch_beat(t1) - (4.0 - 1.0)).abs() < 1e-9, "{t1:?}");
    }

    /// 列 1 に「1 周したら次の列へ」を付け、`Song` 側は「列 1 を撃った状態」に
    /// してある曲。書き出しの起点はこれを読み直すところから始まる。
    fn chained_song() -> Song {
        let mut song = two_rows();
        song.scenes[0].follow = FollowAction {
            enabled: true,
            a: FollowActionKind::Next,
            chance_a: 100,
            ..FollowAction::default()
        };
        song.tracks[0].launcher = RowPlayback::Launcher { clip_id: 10 };
        song.tracks[1].launcher = RowPlayback::Launcher { clip_id: 20 };
        song.last_launched_scene_id = 1;
        song
    }

    /// `export.rs` の走査と同じ起点 (`LauncherRuntime::new()` + `arm_reseed()`) で
    /// 0..12 拍を刻み、各 buffer の 2 行の供給元を記録する。
    fn export_walk(song: &Song) -> Vec<(Option<u32>, Option<u32>, bool)> {
        let mut rt = LauncherRuntime::new();
        rt.arm_reseed();
        let mut out = Vec::new();
        let mut beat = 0.0_f64;
        while beat < 12.0 {
            rt.update(song, span_at(beat), LaunchQuantize::Off, true);
            let t1 = rt.rows().track_row(0).tail;
            let t2 = rt.rows().track_row(1).tail;
            out.push((t1.cell_clip_id(), t2.cell_clip_id(), t2 == RowPhase::Silent));
            beat += 0.25;
        }
        out
    }

    /// **書き出しはシーンの連鎖を再現する** (Q9 / §2.5)。`export.rs` は
    /// `LauncherRuntime` を新品で作って `arm_reseed` するだけなので、
    /// 「ユーザーが最後に撃った列」(`Song.last_launched_scene_id`) を
    /// 起点として読まないと、列のフォローアクションが**一度も動かない**
    /// (行のセルだけが延々ループした WAV になる)。
    #[test]
    fn 書き出しの起点は列の連鎖を再現する() {
        let song = chained_song();
        let walk = export_walk(&song);
        // 起点: 列 1 (track1 = 10 / track2 = 20)。
        assert_eq!(walk[0], (Some(10), Some(20), false), "範囲の先頭で一斉に撃つ");
        // 4 拍 (列 1 の最長セル 1 周) で列 2 へ。track2 は列 2 が空なので停止 (Q11)。
        let last = walk.last().copied().expect("刻んでいる");
        assert_eq!(last, (Some(11), None, true), "列 2 へ連鎖していない: {walk:?}");

        // 連鎖の起点を持たない曲 (= 誰もシーンを撃っていない) では動かない。
        let mut lone = chained_song();
        lone.last_launched_scene_id = 0;
        let lone_walk = export_walk(&lone);
        assert_eq!(
            lone_walk.last().copied(),
            Some((Some(10), Some(20), false)),
            "撃っていない列の連鎖が勝手に走っている"
        );
    }

    /// **同じ曲を 2 回書き出すと同じ音になる** (§4 の byte 一致の前提)。
    /// 乱数は `f(seed, 発火拍)` の純ハッシュで、走行位置は `Song` に残らない
    /// (§1.4) ので、起点が同じなら遷移列も同じでなければならない。
    #[test]
    fn 書き出しは何度走らせても同じ遷移列になる() {
        // `Any` = 抽選を通る種別。相関のある状態を持ってしまうと 2 回目がズレる。
        let mut song = chained_song();
        song.scenes[0].follow.a = FollowActionKind::Any;
        for cell in &mut song.tracks[0].session_clips {
            cell.launch.follow = FollowAction {
                enabled: true,
                a: FollowActionKind::Any,
                chance_a: 50,
                b: FollowActionKind::PlayAgain,
                ..FollowAction::default()
            };
        }
        let a = export_walk(&song);
        let b = export_walk(&song);
        assert_eq!(a, b, "2 回目の書き出しが 1 回目と違う");
        assert!(
            a.iter().any(|s| s.0 != a[0].0),
            "遷移が 1 度も起きていない (再現性を確かめられていない)"
        );
    }

    /// **`Song` 側の主導権の変更 (undo / redo / 保存版へ戻す / recovery) が
    /// engine へ届く。** `project_id` の変化と Play の立ち上がりしか見ていないと、
    /// これらは音に一切反映されない (差し替え前のセルが鳴り続ける)。
    ///
    /// 同時に「行単位であること」も見る — 触っていない行の位相が飛んだら、
    /// 無関係な編集のたびに全部の音が飛ぶ。
    #[test]
    fn song_の主導権の変更は行単位で届く() {
        let mut song = two_rows();
        song.tracks[0].launcher = RowPlayback::Launcher { clip_id: 10 };
        song.tracks[1].launcher = RowPlayback::Launcher { clip_id: 20 };
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(10));
        let row2 = rt.rows().track_row(1).tail;

        // undo 相当: `Song` の主導権だけ別のセルへ差し替わる (Play も project 切替も無い)。
        song.tracks[0].launcher = RowPlayback::Launcher { clip_id: 11 };
        step(&mut rt, &song, 1.0);
        assert_eq!(
            rt.rows().track_row(0).tail.cell_clip_id(),
            Some(11),
            "Song の差し替えが engine に届いていない"
        );
        assert_eq!(rt.rows().track_row(1).tail, row2, "触っていない行の位相が飛んだ");

        // アレンジへ戻す / 停止も同じ経路で届く。
        song.tracks[0].launcher = RowPlayback::Arranger;
        step(&mut rt, &song, 2.0);
        assert_eq!(rt.rows().track_row(0).tail, RowPhase::Arranger);
        song.tracks[0].launcher = RowPlayback::LauncherStopped;
        step(&mut rt, &song, 3.0);
        assert_eq!(rt.rows().track_row(0).tail, RowPhase::Silent);
    }

    /// **発火の反響 (`LaunchCell` の直後に届く `LoadSong`) で位相が飛ばない。**
    /// GUI は操作を先に撃ってから `Song` を書くので、差分だけを見て撃ち直すと
    /// セルを撃つたびに必ず 1 回頭出しし直してしまう。
    #[test]
    fn 撃った直後の_load_song_は撃ち直しにならない() {
        let mut song = two_rows();
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        press(&mut rt, 1, 10, true);
        step(&mut rt, &song, 0.5);
        let fired = rt.rows().track_row(0).tail;
        assert_eq!(fired.cell_clip_id(), Some(10), "撃てていない");

        // GUI がその操作を `Song` へ書き、off-thread compile のぶん遅れて届く。
        song.tracks[0].launcher = RowPlayback::Launcher { clip_id: 10 };
        step(&mut rt, &song, 1.0);
        assert_eq!(
            rt.rows().track_row(0).tail,
            fired,
            "反響で撃ち直しており、位相 (launch_beat) が飛んでいる"
        );
    }

    /// **操作とその反響が同じ buffer に届いても、量子化を待って 1 回だけ切り替わる。**
    /// GUI は `LaunchScene` の直後に `Song` (`LoadSong`) を書き、compile が 1 buffer に
    /// 収まると両方が同じ `update` で見える。旧実装は反響を先に見て即 seed し (量子化
    /// 無視で頭出し)、さらに予約が境界で発火してもう一度頭出ししていた
    /// (= シーン切り替えのたびに先頭 ~半小節が 2 回鳴る)。
    #[test]
    fn 同じ_buffer_に届いた操作と反響は量子化境界で_1_回だけ切り替わる() {
        let mut song = two_rows();
        let mut rt = LauncherRuntime::new();
        rt.update(&song, span_at(0.0), LaunchQuantize::Bars(1), true);

        // 拍 1.0: 押下と、その反響 (`Song` 側が既に列 2 を撃った状態) が同時に届く。
        rt.push_request(LaunchRequest::Scene { scene_id: 2, pressed: true });
        song.tracks[0].launcher = RowPlayback::Launcher { clip_id: 11 };
        song.tracks[1].launcher = RowPlayback::LauncherStopped;
        song.last_launched_scene_id = 2;
        rt.update(&song, span_at(1.0), LaunchQuantize::Bars(1), true);
        assert_eq!(
            rt.rows().track_row(0).tail,
            RowPhase::Arranger,
            "反響を先に見て量子化を待たずに撃っている"
        );

        // 拍 2.0 / 3.0 も待つ (毎 buffer 撃ち直していない)。
        rt.update(&song, span_at(2.0), LaunchQuantize::Bars(1), true);
        assert_eq!(rt.rows().track_row(0).tail, RowPhase::Arranger);
        rt.update(&song, span_at(3.0), LaunchQuantize::Bars(1), true);
        assert_eq!(rt.rows().track_row(0).tail, RowPhase::Arranger);

        // 拍 4.0 を含む buffer で切り替わり、位相の原点は境界そのもの。
        rt.update(&song, span_at(3.99), LaunchQuantize::Bars(1), true);
        let RowPhase::Cell { clip_id, launch_beat, .. } = rt.rows().track_row(0).tail else {
            panic!("境界で切り替わっていない")
        };
        assert_eq!(clip_id, 11);
        assert!((launch_beat - 4.0).abs() < 1e-9, "{launch_beat}");

        // 以降の buffer で頭出しし直さない (2 回目の再生が無い)。
        rt.update(&song, span_at(4.5), LaunchQuantize::Bars(1), true);
        rt.update(&song, span_at(5.0), LaunchQuantize::Bars(1), true);
        let RowPhase::Cell { launch_beat: later, .. } = rt.rows().track_row(0).tail else {
            panic!("セルを離した")
        };
        assert!((later - 4.0).abs() < 1e-9, "反響で撃ち直した: launch_beat {later}");
    }

    /// **鳴っているセルに後からフォローアクションを付けても効く。** 旧実装は `follow_at` を
    /// 撃った瞬間にしか張らず、走行中に「次のセル」を付けても撃ち直すまで一度も発火しなかった。
    #[test]
    fn 走行中に付けたフォローアクションは次の周期で発火する() {
        let mut song = two_rows();
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        // 列 1 のセル 10 (4 拍、フォローなし) を拍 0 で撃つ。
        press(&mut rt, 1, 10, true);
        step(&mut rt, &song, 0.0);
        step(&mut rt, &song, 1.0);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(10));

        // 拍 1.5: インスペクタで「1 周したら次のセル」を付ける (Song が更新される)。
        song.tracks[0].session_clips[0].launch.follow = FollowAction {
            enabled: true,
            a: FollowActionKind::Next,
            chance_a: 100,
            ..FollowAction::default()
        };
        step(&mut rt, &song, 1.5);
        // 周期の起点は撃った拍 0 のまま → 拍 4.0 で発火し、隣のセル 11 へ。それまでは 10。
        step(&mut rt, &song, 3.0);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(10), "早すぎる発火");
        step(&mut rt, &song, 3.99);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(11), "次のセルへ行かない");
    }

    /// 起点から見て過去の周期に張らない: 拍 5 (2 周目の途中) で付けたら発火は拍 8。
    #[test]
    fn 走行中に付けたフォローアクションは過去の拍に暴発しない() {
        let mut song = two_rows();
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        press(&mut rt, 1, 10, true);
        step(&mut rt, &song, 0.0);
        step(&mut rt, &song, 4.5);
        song.tracks[0].session_clips[0].launch.follow = FollowAction {
            enabled: true,
            a: FollowActionKind::Next,
            chance_a: 100,
            ..FollowAction::default()
        };
        step(&mut rt, &song, 5.0);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(10), "拍 4.0 (過去) に暴発");
        step(&mut rt, &song, 7.0);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(10));
        step(&mut rt, &song, 7.99);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(11), "拍 8.0 で発火しない");
    }

    /// 列 (シーン) のフォローアクションも同じ: 撃った後に付けても次の周期で列が進む。
    #[test]
    fn 走行中に付けた列のフォローアクションは次の周期で発火する() {
        let mut song = two_rows();
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        rt.push_request(LaunchRequest::Scene { scene_id: 1, pressed: true });
        step(&mut rt, &song, 0.0);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(10));
        assert_eq!(rt.rows().track_row(1).tail.cell_clip_id(), Some(20));

        // 拍 1.0: 列 1 に「1 周したら次の列」を付ける。
        song.scenes[0].follow = FollowAction {
            enabled: true,
            a: FollowActionKind::Next,
            chance_a: 100,
            ..FollowAction::default()
        };
        step(&mut rt, &song, 1.0);
        step(&mut rt, &song, 3.0);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(10), "早すぎる発火");
        // 最長セル 4 拍 → 拍 4.0 で列 2 へ (track 1 は 11、列 2 が空の track 2 は停止)。
        step(&mut rt, &song, 3.99);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(11), "次の列へ行かない");
        assert_eq!(rt.rows().track_row(1).tail, RowPhase::Silent);
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

    /// 停止中の Toggle は「鳴っているセルをもう一度押した」ではない。
    ///
    /// 走行状態は停止で消えない (計画書 §0) ので `phase` は `Cell` のまま残る。
    /// それを Toggle の押し直しと読むと停止予約になり、▶ を押しても **その行だけ
    /// 1 回鳴らない**。GUI (`AppData::launch_cell`) と対で直す必要があるので、
    /// 片側だけ戻ると静かに再発する。
    #[test]
    fn 停止中の_toggle_は止めずに撃ち直す() {
        let mut song = two_rows();
        song.tracks[1].session_clips[0].launch.mode = LaunchMode::Toggle;
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        press(&mut rt, 2, 20, true);
        step(&mut rt, &song, 0.0);
        assert_eq!(rt.rows().track_row(1).tail.cell_clip_id(), Some(20));

        // Space で停止 → 同じセルの ▶ を押す (`playing == false` で届く)。
        press(&mut rt, 2, 20, true);
        rt.update(&song, span_at(1.0), LaunchQuantize::Off, false);
        assert_eq!(rt.rows().track_row(1).tail.cell_clip_id(), Some(20));
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

    /// **飛び先セルに独自の量子化があってもフォローアクションは 1 度で進む。**
    ///
    /// 予約を置いた時点でフォロータイマーを畳まないと、次の buffer でも
    /// `follow_at` (過去の拍) が最小値として選ばれ続け、予約は毎回 1 つ先の境界へ
    /// 逃げる = 元のセルを永久にループする。計画書 §2.3 の「セル自身の quantize には
    /// 従う」経路そのものが死ぬ。
    #[test]
    fn 量子化のある飛び先へでもフォローアクションは進む() {
        let mut song = two_rows();
        // 飛び先 (列 2 のセル) だけ 1 小節量子化。
        song.tracks[0].session_clips[1].launch.quantize = LaunchQuantize::Bars(1);
        // 発火を境界からずらす (Unlinked 1.5 拍) — ここが境界に乗ると
        // `at == fire` になって待ちが発生せず、この欠陥を踏まない。
        song.tracks[0].session_clips[0].launch.follow = FollowAction {
            enabled: true,
            a: FollowActionKind::Next,
            chance_a: 100,
            linked: false,
            time_beats: 1.5,
            ..FollowAction::default()
        };
        song.tracks[0].launcher = RowPlayback::Launcher { clip_id: 10 };

        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(10));

        // 拍 1.5 でフォローが解決し、飛び先の量子化境界 (拍 4.0) へ予約される。
        step(&mut rt, &song, 1.49);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(10), "境界までは元のセル");

        // 拍 4.0 の buffer で実際に乗り換わる。
        step(&mut rt, &song, 3.99);
        let tail = rt.rows().track_row(0).tail;
        assert_eq!(tail.cell_clip_id(), Some(11), "予約が毎 buffer 先へ逃げている");
        let RowPhase::Cell { launch_beat, .. } = tail else { panic!("セルでない") };
        assert!((launch_beat - 4.0).abs() < 1e-9, "{launch_beat}");
    }

    /// **列の連鎖の発火拍は buffer の切り方に依存しない。**
    ///
    /// `Go` の分岐だけ発火拍を渡さずに解き直すと buffer 先頭へ丸められ、遷移ごとに
    /// 最大 1 buffer ぶん前へずれる。丸め量が device buffer size 依存なので、
    /// live と書き出しでシーンの切り替わる位置が食い違う (Q9 が破れる)。
    #[test]
    fn シーンの連鎖は_buffer_境界へ丸められない() {
        let mut song = two_rows();
        song.scenes[0].follow = FollowAction {
            enabled: true,
            a: FollowActionKind::Next,
            chance_a: 100,
            ..FollowAction::default()
        };
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        rt.push_request(LaunchRequest::Scene { scene_id: 1, pressed: true });
        step(&mut rt, &song, 0.0);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(10));

        // 4 拍セル / Linked ×1 → 列の遷移は拍 4.0。buffer 先頭は 3.99 なので、
        // 丸めていれば `launch_beat` が 3.99 になる。
        step(&mut rt, &song, 3.99);
        let tail = rt.rows().track_row(0).tail;
        assert_eq!(tail.cell_clip_id(), Some(11), "列 2 へ移っていない");
        let RowPhase::Cell { launch_beat, .. } = tail else { panic!("セルでない") };
        assert!((launch_beat - 4.0).abs() < 1e-9, "buffer 先頭へ丸められた: {launch_beat}");
    }

    /// **列の占有判定はマスター行 (`song_lanes`) のセルも数える。**
    ///
    /// 数えないと「マスター行にしかセルの無い列」が空列と判定され、Q13 の
    /// 「空セルに区切られた塊」がそこで途切れる → `Next` がその列を飛ばす。
    #[test]
    fn 列の占有判定はマスター行のセルを数える() {
        let mut song = two_rows();
        // 列 2 は通常トラックには無く、**マスター行のレーンにだけ**セルがある。
        song.tracks[0].session_clips.remove(1);
        let mut lane = AutomationLane::new(
            AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
            0.5,
        );
        lane.id = 1;
        lane.session_clips.push(SessionAutomationClip {
            scene_id: 2,
            clip: AutomationClip { id: 50, length_beats: 4.0, ..AutomationClip::default() },
            launch: LaunchSettings::default(),
        });
        song.song_lanes.push(lane);
        song.scenes[0].follow = FollowAction {
            enabled: true,
            a: FollowActionKind::Next,
            chance_a: 100,
            ..FollowAction::default()
        };

        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        rt.push_request(LaunchRequest::Scene { scene_id: 1, pressed: true });
        step(&mut rt, &song, 0.0);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(10));

        // 拍 4.0 で列 1 → 列 2。列 2 に居るのはマスター行のセルだけ。
        step(&mut rt, &song, 3.99);
        assert_eq!(
            rt.rows().master_rows().lane(0).tail.cell_clip_id(),
            Some(50),
            "マスター行の列が空列扱いされて Next に飛ばされた"
        );
        // 列 2 にセルを持たない通常トラック行は停止 (Q11)。
        assert_eq!(rt.rows().track_row(0).tail, RowPhase::Silent);
    }

    /// **全停止 / 全行アレンジ復帰は列の連鎖も終わらせる。**
    /// 残すと、止めたはずの全行が `scene.at` に到達した瞬間に勝手に鳴り出す。
    #[test]
    fn 全停止と全行アレンジ復帰は列の連鎖を解除する() {
        let mut song = two_rows();
        song.scenes[0].follow = FollowAction {
            enabled: true,
            a: FollowActionKind::Next,
            chance_a: 100,
            ..FollowAction::default()
        };
        for req in [LaunchRequest::StopAll, LaunchRequest::AllToArranger] {
            let mut rt = LauncherRuntime::new();
            step(&mut rt, &song, 0.0);
            rt.push_request(LaunchRequest::Scene { scene_id: 1, pressed: true });
            step(&mut rt, &song, 0.0);
            assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(10));

            rt.push_request(req);
            step(&mut rt, &song, 1.0);
            let stopped = rt.rows().track_row(0).tail;

            // 列の連鎖が生きていれば拍 4.0 で列 2 が撃たれて鳴り出す。
            step(&mut rt, &song, 3.99);
            assert_eq!(
                rt.rows().track_row(0).tail,
                stopped,
                "{req:?} の後に列のフォローアクションが勝手に鳴り出した"
            );
        }
    }

    /// **1 行だけの停止では列の連鎖を解除しない。**
    /// 他の行はまだその列を鳴らしているので、列としてはまだ走っている。
    #[test]
    fn 一行だけの停止は列の連鎖を解除しない() {
        let mut song = two_rows();
        song.scenes[0].follow = FollowAction {
            enabled: true,
            a: FollowActionKind::Next,
            chance_a: 100,
            ..FollowAction::default()
        };
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        rt.push_request(LaunchRequest::Scene { scene_id: 1, pressed: true });
        step(&mut rt, &song, 0.0);

        rt.push_request(LaunchRequest::StopRow { key: RowKey::track(2) });
        step(&mut rt, &song, 1.0);
        step(&mut rt, &song, 3.99);
        assert_eq!(
            rt.rows().track_row(0).tail.cell_clip_id(),
            Some(11),
            "track 2 を止めただけで列の連鎖まで消えた"
        );
    }

    /// **再生開始 (reseed) は列の起点も `Song` から張り直す。**
    ///
    /// 行だけ撃ち直して列の走行状態を残すと、「どこで停止したか」で
    /// 次の列へ移るまでの拍が変わる (= 同じ起点から始まらない、§1.4)。
    /// 起点の SSoT は `Song.last_launched_scene_id` (`0` = 未発火)。
    #[test]
    fn 再生し直すと列の連鎖は保存した起点から張り直す() {
        let mut song = two_rows();
        song.scenes[0].follow = FollowAction {
            enabled: true,
            a: FollowActionKind::Next,
            chance_a: 100,
            ..FollowAction::default()
        };
        song.tracks[0].launcher = RowPlayback::Launcher { clip_id: 10 };
        song.tracks[1].launcher = RowPlayback::Launcher { clip_id: 20 };

        // (a) 未発火 (`0`) なら、前の再生で armed だった連鎖は持ち越さない。
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        rt.push_request(LaunchRequest::Scene { scene_id: 1, pressed: true });
        step(&mut rt, &song, 0.0); // scene.at = 4.0
        rt.arm_reseed();
        step(&mut rt, &song, 0.0);
        step(&mut rt, &song, 3.99);
        assert_eq!(
            rt.rows().track_row(0).tail.cell_clip_id(),
            Some(10),
            "捨てたはずの列の連鎖が停止 → 再生を跨いで生き残った"
        );

        // (b) 撃った列が保存されていれば、reseed 拍を起点に同じ連鎖を辿る。
        song.last_launched_scene_id = 1;
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        step(&mut rt, &song, 3.99);
        assert_eq!(
            rt.rows().track_row(0).tail.cell_clip_id(),
            Some(11),
            "last_launched_scene_id から列の連鎖が張り直されていない"
        );
    }

    /// **ワンショットが鳴り終わってもフォローアクションは効く。**
    ///
    /// 「鳴っている / いない」と「フォローの予定」は別の寿命。終端で予定ごと
    /// 捨てると、Unlinked の時間や Linked の倍率 2 以上が二度と発火せず、
    /// 行が無音のまま固まる (計画書 §2.3 と食い違う)。
    #[test]
    fn ワンショット終端の後もフォローアクションが発火する() {
        let mut song = two_rows();
        let c = &mut song.tracks[0].session_clips[0];
        c.clip.length_beats = 2.0;
        c.launch.looping = false;
        c.launch.follow = FollowAction {
            enabled: true,
            a: FollowActionKind::Next,
            chance_a: 100,
            linked: false,
            time_beats: 4.0,
            ..FollowAction::default()
        };
        song.tracks[0].launcher = RowPlayback::Launcher { clip_id: 10 };

        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        // 拍 2.0 でワンショットが終端 → 無音。
        step(&mut rt, &song, 1.99);
        assert_eq!(rt.rows().track_row(0).tail, RowPhase::Silent);
        // 拍 4.0 のフォローアクションはまだ生きている。
        step(&mut rt, &song, 3.99);
        assert_eq!(
            rt.rows().track_row(0).tail.cell_clip_id(),
            Some(11),
            "終端で follow ごと捨てられた"
        );
    }

    /// **`LaunchMode::Repeat` は量子化の周期で撃ち直す** (型の doc が SSoT)。
    /// セル長で撃ち直していると、既定の `looping: true` のセルでは通常のループ再生と
    /// 音が完全に同じになり、Repeat を選んだことが音に現れない。
    #[test]
    fn repeat_は量子化の周期で撃ち直す() {
        let mut song = two_rows();
        let c = &mut song.tracks[0].session_clips[0];
        c.launch.mode = LaunchMode::Repeat;
        c.launch.quantize = LaunchQuantize::Note { div: 16, triplet: false }; // 0.25 拍

        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        press(&mut rt, 1, 10, true);

        let mut launches: Vec<f64> = Vec::new();
        let mut beat = 0.0;
        while beat < 1.0 {
            step(&mut rt, &song, beat);
            if let RowPhase::Cell { launch_beat, .. } = rt.rows().track_row(0).tail
                && launches.last().is_none_or(|b| (b - launch_beat).abs() > 1e-9)
            {
                launches.push(launch_beat);
            }
            beat += f64::from(FRAMES) * f64::from(BPM) / (60.0 * f64::from(SR));
        }
        // 1 拍の間に 1/16 (= 0.25 拍) 刻みで撃ち直る (拍 0 / .25 / .5 / .75 …)。
        // セル長 (4 拍) 周期に倒れていれば 1 回しか出ない。
        assert!(launches.len() >= 4, "撃ち直しの周期がセル長になっている: {launches:?}");
        for (i, b) in launches.iter().enumerate() {
            let want = 0.25 * i as f64;
            assert!((b - want).abs() < 1e-9, "{i} 回目が {b} (期待 {want})");
        }

        // 離すと撃ち直しが止まる (鳴っているセルはそのまま)。
        press(&mut rt, 1, 10, false);
        step(&mut rt, &song, beat);
        let held = rt.rows().track_row(0).tail;
        for _ in 0..40 {
            beat += f64::from(FRAMES) * f64::from(BPM) / (60.0 * f64::from(SR));
            step(&mut rt, &song, beat);
        }
        assert_eq!(rt.rows().track_row(0).tail, held, "離した後も撃ち直している");
    }

    /// **テンポ / 拍子レーンはランチャーの行にならない** (確定済み設計判断 1)。
    /// 握らせるとローンチ量子化のグリッドが自分に戻る循環になり、GUI と engine で
    /// 時間軸が食い違う。判定の SSoT は `AutomationTarget::accepts_launcher_cells`。
    #[test]
    fn テンポ拍子レーンは行として登録されない() {
        let mut song = two_rows();
        for (i, target) in
            [AutomationTarget::SongTempo, AutomationTarget::SongTimeSigNumerator].into_iter().enumerate()
        {
            let mut lane = AutomationLane::new(target, 120.0);
            lane.id = i as u32 + 1;
            lane.session_clips.push(SessionAutomationClip {
                scene_id: 1,
                clip: AutomationClip { id: 60 + i as u32, length_beats: 4.0, ..AutomationClip::default() },
                launch: LaunchSettings::default(),
            });
            lane.launcher = RowPlayback::Launcher { clip_id: 60 + i as u32 };
            song.song_lanes.push(lane);
        }
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        // 列を撃っても、保存された主導権があってもアレンジのまま。
        rt.push_request(LaunchRequest::Scene { scene_id: 1, pressed: true });
        step(&mut rt, &song, 0.5);
        for i in 0..2 {
            assert_eq!(
                rt.rows().master_rows().lane(i).tail,
                RowPhase::Arranger,
                "テンポ / 拍子レーンをランチャーが握った"
            );
        }
    }

    /// **列の ▶ を離しても `LaunchMode` の意味は変わらない** (Gate は止まる)。
    /// 押下だけ処理して離しを捨てると、同じセルが「撃ち方によって止まらない」。
    #[test]
    fn 列の離しもセルの離しと同じに解釈される() {
        let mut song = two_rows();
        song.tracks[0].session_clips[0].launch.mode = LaunchMode::Gate;
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);

        rt.push_request(LaunchRequest::Scene { scene_id: 1, pressed: true });
        step(&mut rt, &song, 0.0);
        assert_eq!(rt.rows().track_row(0).tail.cell_clip_id(), Some(10));
        // Gate でない track 2 は離しても鳴り続ける。
        assert_eq!(rt.rows().track_row(1).tail.cell_clip_id(), Some(20));

        rt.push_request(LaunchRequest::Scene { scene_id: 1, pressed: false });
        step(&mut rt, &song, 1.0);
        assert_eq!(rt.rows().track_row(0).tail, RowPhase::Silent, "Gate が離しで止まらない");
        assert_eq!(rt.rows().track_row(1).tail.cell_clip_id(), Some(20));
    }

    /// **ループ折り返し / seek でセルの位相と予定が連続する** (`on_transport_jump`)。
    ///
    /// ここが効いていないと「たまに 1 周まるごと無音」「たまにフォローアクションが
    /// 二度と発火しない」という間欠症状で出る。呼び元 (engine のループ端 / seek) は
    /// 実機でしか通らないので、契約はここで固定する。
    #[test]
    fn トランスポートが跳んでも位相と予定が連続する() {
        let mut song = two_rows();
        song.tracks[0].session_clips[0].launch.follow = FollowAction {
            enabled: true,
            a: FollowActionKind::Next,
            chance_a: 100,
            linked: false,
            time_beats: 6.0,
            ..FollowAction::default()
        };
        song.tracks[0].launcher = RowPlayback::Launcher { clip_id: 10 };
        let mut rt = LauncherRuntime::new();
        step(&mut rt, &song, 0.0);
        step(&mut rt, &song, 5.0);
        let before = rt.rows().track_row(0).tail.effective_beat(5.0).expect("鳴っている");

        // 4 拍のループが折り返した (playhead 5.0 → 1.0)。
        rt.on_transport_jump(-4.0);
        step(&mut rt, &song, 1.0);
        let after = rt.rows().track_row(0).tail.effective_beat(1.0).expect("跳んだ先で無音になった");
        assert!((after - before).abs() < 1e-9, "位相が飛んだ: {before} → {after}");

        // フォローアクションも同じ距離だけ手前へ来る (拍 6.0 → 2.0)。
        step(&mut rt, &song, 1.99);
        assert_eq!(
            rt.rows().track_row(0).tail.cell_clip_id(),
            Some(11),
            "跳び越されてフォローアクションが二度と発火しない"
        );
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
