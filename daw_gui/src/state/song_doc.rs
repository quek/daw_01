//! S3b-1: Song 文書 (SongDoc) — Song 編集の**単一チョークポイント**。
//!
//! `song` field は private。 `&mut Song` を得る手段は [`SongDoc::edit`] のみで、
//! edit() が無条件に undo snapshot / edit_epoch bump / export 中拒否を担う
//! (アーキテクチャ不変条件 5、 docs/plan_arch_refactor.md §7.5)。 これにより
//! 旧 `is_undoable` whitelist (102 variants) と手動 `push_undo_snapshot`
//! (~29 箇所) は全廃され、 「whitelist 入れ忘れ = undo 不能 / dirty 漏れ」 の
//! 故障モードが型ごと消える。
//!
//! - dirty は `edit_epoch != saved_epoch` の O(1) 派生 (毎フレームの Song
//!   全比較 `recompute_dirty` を置換)。
//! - 子プロセス sync は runner の frame flush が `sync_epoch !=
//!   last_synced_epoch` を見て pull する (handler/sync.rs)。`sync_epoch` は
//!   「Song の中身が変わった」世代で、`edit_epoch` (文書の履歴 = undo / dirty) の
//!   上位集合。差は [`SongDoc::edit_playback`] — ランチャーの再生状態
//!   (`Track.launcher` 等) は Song に住み保存もされるが、撃つ / 止めるは
//!   「聴き方」なので履歴にも `*` にも入れない (`docs/plan_rmd_87_clip_launcher.md` §1.3)。
//! - undo/redo も両 epoch を bump する (= flush が LoadSong を再送する)。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::model::Song;

/// Undo 履歴の上限 (snapshot 方式)。
const UNDO_LIMIT: usize = 200;

/// 履歴リスト先頭 (= どの編集にも遡れる起点) の表示ラベル。 New / Open /
/// Recovery の直後に確定する baseline state の名前。
pub const BASELINE_LABEL: &str = "初期状態";

/// 連続 stream 編集 (MIDI CC / BPM scrub / automation 録音等、 Begin/End
/// bracket を持たない編集源) の gesture を「時間ギャップ」 で区切る閾値。
/// 最終編集からこれ以上空いたら新しい undo step を始める。
const STREAM_GESTURE_GAP: Duration = Duration::from_secs(1);

/// [`SongDoc::edit`] の undo 粒度。
///
/// - `Discrete`: 1 呼び出し = 1 undo step (常に snapshot を積む)。
/// - `Gesture(id)`: 同一 gesture id の**連続する** edit は 1 undo step に
///   squash する (drag/scrub = 1 undo)。 別 id の edit / undo / redo /
///   replace が挟まると chain が切れ、 次の edit は snapshot を積む。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditScope {
    Discrete,
    Gesture(u64),
}

/// [`SongDoc::enter_own_gesture`] が退避した bracket 状態。
#[derive(Debug, Clone, Copy)]
pub struct GestureSave {
    gesture: Option<u64>,
    scope: EditScope,
}

/// Begin/End bracket を持たない連続編集源の識別子
/// ([`SongDoc::stream_scope`] のキー)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamGesture {
    /// Transport bar の BPM scrubable_number ドラッグ。
    BpmScrub,
    /// Transport bar の拍子分子 scrubable_number ドラッグ。
    TimeSigScrub,
    /// MIDI Learn binding 経由のハードウェア CC ストリーム。
    MidiCc,
    /// Touch/Latch/Write mode の automation 録音 (playhead 追従の point 書込)。
    AutomationRecord,
    /// r.md #67: カーソルキーによるノートの移動 / 音程変更。 押しっぱなしのキーリピートも
    /// 単発の連打も、 1 秒空くまでは 1 undo step に畳む (100 回押して 100 step は誤り)。
    NoteNudgeMove,
    /// r.md #67: カーソルキーによるノート長の伸縮 (移動とは別 step にする)。
    NoteNudgeLength,
    /// カーソルキーによる**範囲内の素材のナッジ** (`docs/plan_range_selection.md` §3.2)。
    /// ノートの nudge と同じ理由で、押しっぱなしのキーリピートを 1 step に畳む。
    RangeNudge,
}

/// undo / redo スタックの 1 要素。 過去 (または未来) の Song snapshot と、
/// **その state を生んだ編集の表示ラベル**。 ラベルは履歴リスト UI
/// ([`SongDoc::history_labels`]) がそのまま行に出す。 label は編集イベントの
/// [`crate::event::AppEvent::undo_label`] 由来の `&'static str` なので heap を
/// 持たず Copy で stack 間を移動できる。
struct HistoryEntry {
    song: Song,
    label: &'static str,
}

/// Song 文書: song 本体 + undo/redo + dirty/epoch + 保存先 path。
pub struct SongDoc {
    /// **private**: 変更は [`SongDoc::edit`] 経由のみ (不変条件 5)。
    song: Song,
    /// song を変更するたび (edit / normalize / undo / redo / replace) に進む
    /// カウンタ。 dirty 判定 (`!= saved_epoch`)・子プロセス sync
    /// (`!= last_synced_epoch`)・描画キャッシュ (`arr_label_cache`) の世代キー。
    /// 1 始まりで cache (Default epoch 0) と必ず不一致にし、 初回 build で
    /// 一度 regenerate させる。
    edit_epoch: u64,
    /// 最後に save / load / new した時点の `edit_epoch`。 dirty = 不一致。
    saved_epoch: u64,
    /// 「Song の中身が変わった」世代 (子プロセス sync が読む)。`edit_epoch` が進む
    /// ときは必ず進み、加えて [`SongDoc::edit_playback`] でも進む。
    sync_epoch: u64,
    /// 保存先 (.daw)。 未保存プロジェクトは `None`。
    pub file_path: Option<PathBuf>,

    undo_stack: VecDeque<HistoryEntry>,
    redo_stack: VecDeque<HistoryEntry>,
    /// 現在の live state (`song`) を生んだ編集のラベル。 履歴リストで current
    /// 行に出す。 baseline は [`BASELINE_LABEL`]。 undo/redo/jump で復元し、
    /// edit() が新 step を積むたびに `pending_label` へ更新される。
    current_label: &'static str,
    /// 現在 dispatch 中の編集イベントのラベル ([`SongDoc::begin_event`] が
    /// event 由来で設定)。 edit() が **実際に snapshot を積んだ** ときだけ
    /// `current_label` へ昇格する (= 1 undo step = 1 ラベル)。
    pending_label: &'static str,
    /// 直前の edit の gesture id。 `Gesture(id)` edit が同 id なら snapshot skip。
    last_gesture: Option<u64>,

    /// gesture id の単調 allocator (event / interaction / stream 共用)。
    next_gesture_id: u64,
    /// Begin*/End* イベントで bracket される interaction gesture (pointer
    /// drag / scrub / color picker session)。 `Some` の間、 ambient scope は
    /// この id を使い、 drag 全体が 1 undo step に squash される。
    active_gesture: Option<u64>,
    /// 現在 dispatch 中の AppEvent に割り当てた ambient scope
    /// ([`SongDoc::begin_event`] が設定)。 1 event 内の複数 edit_song 呼び出し
    /// (ループ / helper 連鎖) が 1 undo step に squash されることを保証する。
    event_scope: EditScope,
    /// 連続 stream 編集源ごとの (gesture id, 最終使用時刻)。
    /// [`STREAM_GESTURE_GAP`] 以上空いたら新 id を割り当てる。
    stream_gestures: HashMap<StreamGesture, (u64, Instant)>,

    /// export (音声 freewheel / 映像 render) 中の編集ロック。 `true` の間
    /// [`SongDoc::edit`] は編集を**拒否**する (None + status message 予約)。
    /// song 凍結の保証はこの 1 点のみ (旧 handle_event 冒頭の event 遮断
    /// allow-list は全廃 — 「新 variant の分類し忘れ = GUI 永久ロック」 事故
    /// class が型ごと消える)。
    export_lock: bool,
    /// edit() が拒否したときに予約される status message。 handle_event の
    /// 末尾が drain して `status_message` へ表示する。
    rejection: Option<&'static str>,

    /// 直近 autosave 時刻。
    pub last_autosave: Instant,
    /// Crash-recovery session id (uuid v4)。 起動時に 1 回生成、 未保存
    /// プロジェクトの autosave file 名 (`<id>.autosave.daw`) と shutdown 時の
    /// cleanup target に使う。
    pub recovery_session_id: String,
}

impl SongDoc {
    pub fn new(song: Song) -> Self {
        Self {
            song,
            edit_epoch: 1,
            saved_epoch: 1,
            sync_epoch: 1,
            file_path: None,
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
            current_label: BASELINE_LABEL,
            pending_label: "編集",
            last_gesture: None,
            next_gesture_id: 1,
            active_gesture: None,
            event_scope: EditScope::Discrete,
            stream_gestures: HashMap::new(),
            export_lock: false,
            rejection: None,
            last_autosave: Instant::now(),
            recovery_session_id: common::recovery::new_session_id(),
        }
    }

    /// 読みは自由 (`&Song`)。
    pub fn song(&self) -> &Song {
        &self.song
    }

    /// `&mut Song` を得る唯一の口。 無条件副作用:
    /// 1. export 中は**拒否** (None + status message 予約)
    /// 2. undo snapshot push (`Gesture(id)` が直前と同 id なら skip = 1 drag 1 undo)
    /// 3. 編集実行
    /// 4. `edit_epoch += 1` (dirty / frame-flush sync がこれを読む)
    pub fn edit<R>(&mut self, scope: EditScope, f: impl FnOnce(&mut Song) -> R) -> Option<R> {
        self.edit_impl(scope, |song| (f(song), true)).map(|(r, _)| r)
    }

    /// [`SongDoc::edit`] の「no-op 検出」付き変種: closure が `false` を返した
    /// (= 実際には何も変わらなかった) 場合、 積んだ snapshot を破棄して
    /// undo 履歴・epoch・redo stack を**一切汚さない**。 no-op になりうる
    /// 操作 (`Song::move_section` 等、 適用可否を Song 側が判定する編集) 用。
    /// 戻り値: `None` = export 中拒否、 `Some(changed)` = 実行結果。
    pub fn edit_checked(
        &mut self,
        scope: EditScope,
        f: impl FnOnce(&mut Song) -> bool,
    ) -> Option<bool> {
        self.edit_impl(scope, |song| ((), f(song))).map(|(_, changed)| changed)
    }

    fn edit_impl<R>(
        &mut self,
        scope: EditScope,
        f: impl FnOnce(&mut Song) -> (R, bool),
    ) -> Option<(R, bool)> {
        if self.export_lock {
            self.rejection = Some("書き出し中は編集できません");
            return None;
        }
        let squash = match scope {
            EditScope::Discrete => false,
            EditScope::Gesture(id) => self.last_gesture == Some(id),
        };
        if !squash {
            // pre-edit state を「その state を生んだラベル」 (= current_label)
            // ごと退避する。 label は snapshot と一緒に stack を移動するので、
            // 履歴リストが常に「この state はどの編集の結果か」 を保てる。
            self.undo_stack.push_back(HistoryEntry {
                song: self.song.clone(),
                label: self.current_label,
            });
        }
        let (r, changed) = f(&mut self.song);
        if changed {
            // redo は「実際に編集が起きた」 ときだけ無効化する (no-op で
            // redo 履歴を消さない)。
            self.redo_stack.clear();
            // 新しい live state を生んだのは今回の編集イベント。 そのラベルを
            // current に昇格する (gesture squash 中は同 event 種なので同値)。
            self.current_label = self.pending_label;
            self.last_gesture = match scope {
                EditScope::Discrete => None,
                EditScope::Gesture(id) => Some(id),
            };
            self.bump_edit_epoch();
            // 上限適用は **編集が確定してから** (push で高々 +1 したぶんを削る)。
            // closure 前に pop_front すると、 no-op (changed=false) 時に push 分だけ
            // 戻しても最古 step の evict は戻らず、 何も起きていないのに undo 履歴の
            // 最古が失われる (旧バグ)。
            while self.undo_stack.len() > UNDO_LIMIT {
                self.undo_stack.pop_front();
            }
        } else if !squash {
            // no-op: 積んだ snapshot を破棄 (dead undo step を作らない)。
            self.undo_stack.pop_back();
        }
        Some((r, changed))
    }

    /// 派生データの正規化 / 保存後の path 書換など、 **undo 履歴に入れない**
    /// song 変更 (口パク自動再生成の適用、 save 完了時の
    /// `Absolute → ProjectRelative` migration)。 epoch は bump する (= dirty
    /// 化 + 子プロセス sync は走る)。 ユーザー編集には使わないこと —
    /// ユーザー操作は必ず [`SongDoc::edit`]。
    pub fn normalize<R>(&mut self, f: impl FnOnce(&mut Song) -> R) -> Option<R> {
        if self.export_lock {
            self.rejection = Some("書き出し中は編集できません");
            return None;
        }
        let r = f(&mut self.song);
        self.bump_edit_epoch();
        Some(r)
    }

    /// [`SongDoc::normalize`] の no-op 検出版 ([`SongDoc::edit_checked`] と対称)。
    /// closure が `false` (= 実際には何も変わらなかった) を返したら
    /// `edit_epoch` を bump しない = dirty 化も子プロセス再 sync も起こさない。
    ///
    /// 用途: 非同期の派生 re-write で、 保存ファイルと**同一**な内容を書き戻す
    /// ケース。 代表例は `SlotPluginLoaded` backfill — plugin load 完了ごとに
    /// PluginInstance を再構築するが、 現行バージョンで保存した project では
    /// 再構築結果が既存と同一。 無条件 `normalize` だと epoch が進み「開いた
    /// だけで '*'」 になる (r.md #9)。 changed 判定を closure に委ねることで、
    /// 本当に内容が変わったとき (旧 file の port 解決 / 手動 plugin 挿入) だけ
    /// dirty + sync させる。 戻り値: `None` = export 中拒否、 `Some(changed)`。
    pub fn normalize_checked(&mut self, f: impl FnOnce(&mut Song) -> bool) -> Option<bool> {
        if self.export_lock {
            self.rejection = Some("書き出し中は編集できません");
            return None;
        }
        let changed = f(&mut self.song);
        if changed {
            self.bump_edit_epoch();
        }
        Some(changed)
    }

    /// ランチャーの**再生状態** (`Track.launcher` / `AutomationLane.launcher` /
    /// `last_launched_scene_id`) の書き換え専用。Song に住み `.daw` にも保存されるが、
    /// 撃つ / 止める / アレンジへ返すは「聴き方」であって曲の中身ではない
    /// (`docs/plan_rmd_87_clip_launcher.md` §1.3) ので、**undo 履歴に積まず `*` も
    /// 立てない**。子プロセス sync だけは走らせる (`sync_epoch` を進める =
    /// 書き出しは今の再生状態を反映する、Q9)。closure が `false` (= 変化なし) なら
    /// 何も進めない。戻り値: `None` = export 中拒否、`Some(changed)`。
    /// 他の field に使わないこと — 曲の中身は必ず [`SongDoc::edit`]。
    pub fn edit_playback(&mut self, f: impl FnOnce(&mut Song) -> bool) -> Option<bool> {
        if self.export_lock {
            self.rejection = Some("書き出し中は編集できません");
            return None;
        }
        let changed = f(&mut self.song);
        if changed {
            self.sync_epoch += 1;
        }
        Some(changed)
    }

    /// 文書の履歴が進んだ: dirty / 派生キャッシュ用の `edit_epoch` と、子プロセス
    /// sync 用の `sync_epoch` を一緒に進める (後者は前者の上位集合)。
    fn bump_edit_epoch(&mut self) {
        self.edit_epoch += 1;
        self.sync_epoch += 1;
    }

    /// plugin state blob の write-back (`RequestAllStates` 応答) **専用**。
    /// blob は host が真実源で wire (LoadSong) からも構造的に除外されているため、
    /// undo / epoch / dirty / 子プロセス sync のどれにも影響しない。
    /// ユーザー編集には決して使わないこと。
    pub fn write_back_plugin_state<R>(&mut self, f: impl FnOnce(&mut Song) -> R) -> R {
        f(&mut self.song)
    }

    pub fn edit_epoch(&self) -> u64 {
        self.edit_epoch
    }

    /// 「Song の中身が変わった」世代 (再生状態の変更を含む)。子プロセス sync の鍵。
    pub fn sync_epoch(&self) -> u64 {
        self.sync_epoch
    }

    /// dirty = 「最後に保存した epoch から編集が進んだ」 の O(1) 派生。
    pub fn is_dirty(&self) -> bool {
        self.edit_epoch != self.saved_epoch
    }

    /// save 完了時に呼ぶ: 現在の epoch を保存済みベースラインにする。
    pub fn mark_saved(&mut self) {
        self.saved_epoch = self.edit_epoch;
    }

    // -------- undo / redo ---------------------------------------------------

    /// undo / redo に積まれている **live 以外**の全 Song (順不同)。 保存時の
    /// メディア掃除が「Undo / Redo で戻れる状態が参照するファイル」を残すために読む
    /// (`crate::media_bundle`)。
    pub fn history_songs(&self) -> impl Iterator<Item = &Song> {
        self.undo_stack.iter().chain(self.redo_stack.iter()).map(|e| &e.song)
    }

    /// undo / redo の全 Song を書き換える。 **epoch / dirty は動かさない** — 用途は
    /// 保存時の「未保存キャッシュ → project bundle」 path 移行を履歴にも及ぼすことだけで
    /// (ファイルは移動済みなので、 履歴側の `Absolute(cache)` を残すと Undo で音源を
    /// 見失う)、 楽曲の編集ではない。
    pub fn rewrite_history(&mut self, mut f: impl FnMut(&mut Song)) {
        for e in self.undo_stack.iter_mut().chain(self.redo_stack.iter_mut()) {
            f(&mut e.song);
        }
    }

    pub fn undo(&mut self) -> bool {
        if self.undo_stack.is_empty() {
            return false;
        }
        self.step_backward();
        self.after_history_jump();
        true
    }

    pub fn redo(&mut self) -> bool {
        if self.redo_stack.is_empty() {
            return false;
        }
        self.step_forward();
        self.after_history_jump();
        true
    }

    /// undo 1 段: undo_stack から 1 state を pop して live に、 元 live を
    /// current_label ごと redo_stack へ退避する。 caller が境界 (`is_empty`)
    /// を保証すること。 履歴 jump の副作用 (epoch bump 等) は含めない。
    fn step_backward(&mut self) {
        let prev = self.undo_stack.pop_back().expect("caller guarantees non-empty");
        let current = std::mem::replace(&mut self.song, prev.song);
        // 再生状態は履歴に属さない — 差し替えた Song に今の状態を持ち越す
        // (undo でセルが止まったり鳴り出したりしない)。
        self.song.carry_playback_state_from(&current);
        self.redo_stack.push_back(HistoryEntry {
            song: current,
            label: self.current_label,
        });
        self.current_label = prev.label;
    }

    /// redo 1 段: [`SongDoc::step_backward`] の対称。
    fn step_forward(&mut self) {
        let next = self.redo_stack.pop_back().expect("caller guarantees non-empty");
        let current = std::mem::replace(&mut self.song, next.song);
        self.song.carry_playback_state_from(&current);
        self.undo_stack.push_back(HistoryEntry {
            song: current,
            label: self.current_label,
        });
        self.current_label = next.label;
    }

    /// 履歴リスト click 用: `target` 番目の state (0 = baseline、
    /// [`SongDoc::history_current`] = 現在) へ一気に遡る / 進む。 undo/redo を
    /// 必要段数ぶん繰り返すのと等価だが、 中間 state の reconcile を避けて
    /// **1 回だけ** 履歴 jump 副作用を出す (caller が 1 度 reconcile する)。
    /// `target` が範囲外、 または既に current のときは `false` (no-op)。
    pub fn jump_to(&mut self, target: usize) -> bool {
        let total = self.undo_stack.len() + self.redo_stack.len();
        if target > total || target == self.undo_stack.len() {
            return false;
        }
        while self.undo_stack.len() > target {
            self.step_backward();
        }
        while self.undo_stack.len() < target {
            self.step_forward();
        }
        self.after_history_jump();
        true
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    /// 現在積まれている undo snapshot 数 (= 遡れる step 数)。 テストが undo
    /// 履歴の深さを観測するための read-only accessor。
    pub fn undo_depth(&self) -> usize {
        self.undo_stack.len()
    }

    // -------- 履歴リスト (r.md #29) -----------------------------------------

    /// 履歴リスト全 state のラベルを **古い順** (baseline → 最新) で返す。
    /// 長さ = undo 段数 + 1 (current) + redo 段数。 履歴パネルがそのまま
    /// 各行に描く。 index は [`SongDoc::jump_to`] にそのまま渡せる。
    pub fn history_labels(&self) -> Vec<&'static str> {
        let mut labels = Vec::with_capacity(self.undo_stack.len() + 1 + self.redo_stack.len());
        labels.extend(self.undo_stack.iter().map(|e| e.label));
        labels.push(self.current_label);
        // redo_stack は back=次の redo 先 なので、 古い順に並べるには rev。
        labels.extend(self.redo_stack.iter().rev().map(|e| e.label));
        labels
    }

    /// [`SongDoc::history_labels`] の中で現在の live state が占める index。
    pub fn history_current(&self) -> usize {
        self.undo_stack.len()
    }

    fn after_history_jump(&mut self) {
        // undo/redo も epoch を bump する (frame flush が LoadSong を再送する)。
        // saved_epoch とは一致しなくなるので dirty になる (epoch 方式では
        // 「undo で保存時点に戻ったら clean」 は表現しない — O(1) 化の意図的
        // トレードオフ、 docs/plan_arch_refactor.md §7.5)。
        self.bump_edit_epoch();
        // gesture squash chain は履歴 jump を跨がない (跨ぐと drag 再開時の
        // snapshot が skip され、 undo 1 回分の状態が履歴から欠落する)。
        self.last_gesture = None;
    }

    /// New / Open / Recovery: song を丸ごと差し替え、 履歴を破棄して clean に
    /// する。 (save は履歴を残したいので [`SongDoc::mark_saved`] を使う。)
    pub fn replace_song(&mut self, song: Song) {
        self.song = song;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.current_label = BASELINE_LABEL;
        self.last_gesture = None;
        self.bump_edit_epoch();
        self.saved_epoch = self.edit_epoch;
    }

    /// 読み込み時の**不変条件の回復**で中身が変わったことを記録し、`*` (未保存) を立てる。
    /// クリップの重なり解消 (`docs/plan_range_selection.md` §6.4) がこれを使う。
    ///
    /// 履歴 (undo) は積まない — 「元に戻せる編集」 ではなく、不変条件を満たさない
    /// ファイルを読み込んだ結果の修復だから。 解消は冪等なので、一度保存すれば
    /// 次に開いたときは立たない。
    pub fn mark_dirty_after_load_fixup(&mut self) {
        self.bump_edit_epoch();
    }

    // -------- gesture scopes -------------------------------------------------

    /// AppEvent dispatch の冒頭で呼ぶ: この event の ambient scope を確定する。
    /// interaction gesture 中はその id、 それ以外は fresh id (= 1 event 内の
    /// 複数 edit は squash、 event 間は独立)。 `label` は この event が edit() で
    /// snapshot を積んだときの履歴リスト用ラベル (r.md #29)。
    pub fn begin_event(&mut self, label: &'static str) {
        let id = match self.active_gesture {
            Some(id) => id,
            None => self.alloc_gesture(),
        };
        self.event_scope = EditScope::Gesture(id);
        // この event が edit() で snapshot を積んだら、 この label が新 step の
        // 名前になる。 編集しない event では未使用のまま次 event で上書きされる。
        self.pending_label = label;
    }

    /// 現在 dispatch 中 event の ambient scope。
    pub fn event_scope(&self) -> EditScope {
        self.event_scope
    }

    /// Begin* (scrub / drag / picker session) ハンドラが呼ぶ: 以後 End まで
    /// の全 event の編集を 1 undo step に bracket する。
    pub fn begin_gesture(&mut self) {
        let id = self.alloc_gesture();
        self.active_gesture = Some(id);
        // Begin と同一 event 内の後続 edit も gesture に含める。
        self.event_scope = EditScope::Gesture(id);
    }

    /// End* ハンドラが呼ぶ。
    pub fn end_gesture(&mut self) {
        self.active_gesture = None;
    }

    /// interaction gesture (Begin/End bracket) が進行中か。
    pub fn gesture_active(&self) -> bool {
        self.active_gesture.is_some()
    }

    /// **進行中の Begin/End bracket を壊さずに**、以後の編集を 1 undo step へ束ねる。
    /// 戻り値を [`Self::leave_own_gesture`] へ渡して必ず閉じること。
    ///
    /// `begin_gesture` / `end_gesture` を直に使うと、**非同期の完了ハンドラ**
    /// (Glue の焼き込み適用など、ユーザー操作と無関係な時点で走るもの) が
    /// ユーザーのドラッグ中の bracket を横取りして閉じてしまい、以降のドラッグが
    /// 1 フレーム 1 undo step に割れる。ここは前の状態を退避して必ず戻す。
    #[must_use]
    pub fn enter_own_gesture(&mut self) -> GestureSave {
        let save = GestureSave { gesture: self.active_gesture, scope: self.event_scope };
        let id = self.alloc_gesture();
        self.active_gesture = Some(id);
        self.event_scope = EditScope::Gesture(id);
        save
    }

    /// [`Self::enter_own_gesture`] の対。退避しておいた bracket を戻す。
    pub fn leave_own_gesture(&mut self, save: GestureSave) {
        self.active_gesture = save.gesture;
        self.event_scope = save.scope;
    }

    /// Begin/End bracket を持たない連続編集源 (MIDI CC / BPM scrub /
    /// automation 録音) 用の scope。 同一 key の編集が [`STREAM_GESTURE_GAP`]
    /// 以内に連続する間は同じ gesture id を返す (= 1 burst = 1 undo step)。
    pub fn stream_scope(&mut self, key: StreamGesture) -> EditScope {
        let now = Instant::now();
        let fresh = !matches!(
            self.stream_gestures.get(&key),
            Some((_, last)) if now.duration_since(*last) < STREAM_GESTURE_GAP
        );
        if fresh {
            let id = self.alloc_gesture();
            self.stream_gestures.insert(key, (id, now));
        } else if let Some(entry) = self.stream_gestures.get_mut(&key) {
            entry.1 = now;
        }
        EditScope::Gesture(self.stream_gestures[&key].0)
    }

    /// 現在 dispatch 中 event の ambient scope を [`Self::stream_scope`] に差し替える。
    ///
    /// `begin_event` が張った「1 event = 1 undo step」 の scope を、 連続入力用の
    /// gesture へ **上書き** する。 handler 冒頭で 1 度呼べば、 その event 内の
    /// `edit_song` の入れ子もすべて同じ gesture に入るので、 同じ key の編集が
    /// [`STREAM_GESTURE_GAP`] 以内に続く限り 1 undo step に畳まれる (r.md #67 の
    /// カーソルキー nudge)。 1 秒空けば次は新しい step。
    pub fn use_stream_scope(&mut self, key: StreamGesture) {
        self.event_scope = self.stream_scope(key);
    }

    fn alloc_gesture(&mut self) -> u64 {
        let id = self.next_gesture_id;
        self.next_gesture_id += 1;
        id
    }

    // -------- export lock ----------------------------------------------------

    /// export (freewheel / video render) の開始/終了で切り替える。 `true` の間
    /// edit()/normalize() は拒否される (song 凍結の単一保証点)。
    pub fn set_export_lock(&mut self, on: bool) {
        self.export_lock = on;
    }

    pub fn export_locked(&self) -> bool {
        self.export_lock
    }

    /// edit() 拒否時に予約された status message を drain する
    /// (handle_event 末尾 → `status_message`)。
    pub fn take_rejection(&mut self) -> Option<&'static str> {
        self.rejection.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// r.md #9 の核心: no-op な normalize は epoch を bump せず dirty 化しない
    /// (= SlotPluginLoaded backfill が保存ファイルと同一内容を書き戻しても
    /// 「開いただけで '*'」 にならない)。
    #[test]
    fn normalize_checked_noop_does_not_dirty() {
        let mut doc = SongDoc::new(Song::default());
        assert!(!doc.is_dirty(), "new は clean");
        let before = doc.edit_epoch();
        let r = doc.normalize_checked(|_song| false);
        assert_eq!(r, Some(false));
        assert_eq!(doc.edit_epoch(), before, "no-op は epoch を進めない");
        assert!(!doc.is_dirty(), "no-op normalize は dirty 化しない (r.md #9)");
    }

    /// 対の保証: 実際に変えた normalize は従来どおり epoch bump + dirty。
    #[test]
    fn normalize_checked_real_change_dirties() {
        let mut doc = SongDoc::new(Song::default());
        let before = doc.edit_epoch();
        let r = doc.normalize_checked(|song| {
            song.bpm = 140.0;
            true
        });
        assert_eq!(r, Some(true));
        assert_eq!(doc.edit_epoch(), before + 1);
        assert!(doc.is_dirty(), "実変更は dirty 化する");
    }

    /// export 中は normalize_checked も拒否される (song 凍結の単一保証点)。
    #[test]
    fn normalize_checked_rejected_during_export() {
        let mut doc = SongDoc::new(Song::default());
        doc.set_export_lock(true);
        let before = doc.edit_epoch();
        let r = doc.normalize_checked(|song| {
            song.bpm = 140.0;
            true
        });
        assert_eq!(r, None, "export 中は拒否");
        assert_eq!(doc.edit_epoch(), before, "拒否時は epoch 不変");
    }

    // -------- r.md #29: ラベル付き履歴 + jump ---------------------------------

    /// discrete edit を積むと、 各 state に begin_event で渡したラベルが付き、
    /// history_labels() が baseline → 最新の順で返す。
    #[test]
    fn history_labels_reflect_edits() {
        let mut doc = SongDoc::new(Song::default());
        assert_eq!(doc.history_labels(), vec![BASELINE_LABEL]);
        assert_eq!(doc.history_current(), 0);

        doc.begin_event("テンポ変更");
        doc.edit(EditScope::Discrete, |s| s.bpm = 140.0);
        assert_eq!(doc.history_labels(), vec![BASELINE_LABEL, "テンポ変更"]);
        assert_eq!(doc.history_current(), 1);

        doc.begin_event("音量変更");
        doc.edit(EditScope::Discrete, |s| s.bpm = 150.0);
        assert_eq!(doc.history_labels(), vec![BASELINE_LABEL, "テンポ変更", "音量変更"]);
        assert_eq!(doc.history_current(), 2);
    }

    /// 同一 gesture id の連続 edit は 1 step に squash され、 ラベルも 1 つ。
    #[test]
    fn gesture_squash_is_one_labeled_step() {
        let mut doc = SongDoc::new(Song::default());
        doc.begin_event("音量変更");
        doc.edit(EditScope::Gesture(7), |s| s.bpm = 130.0);
        doc.edit(EditScope::Gesture(7), |s| s.bpm = 131.0);
        doc.edit(EditScope::Gesture(7), |s| s.bpm = 132.0);
        assert_eq!(doc.history_labels(), vec![BASELINE_LABEL, "音量変更"]);
        assert_eq!(doc.history_current(), 1);
    }

    /// undo/redo は current index とラベル対応を保ちつつ live state を戻す。
    #[test]
    fn undo_redo_preserve_labels_and_state() {
        let mut doc = SongDoc::new(Song::default());
        let base_bpm = doc.song().bpm;
        doc.begin_event("A");
        doc.edit(EditScope::Discrete, |s| s.bpm = 140.0);
        doc.begin_event("B");
        doc.edit(EditScope::Discrete, |s| s.bpm = 150.0);

        assert!(doc.undo());
        assert_eq!(doc.song().bpm, 140.0);
        assert_eq!(doc.history_current(), 1);
        // 履歴の中身 (ラベル列) は undo では変わらない。
        assert_eq!(doc.history_labels(), vec![BASELINE_LABEL, "A", "B"]);

        assert!(doc.undo());
        assert_eq!(doc.song().bpm, base_bpm);
        assert_eq!(doc.history_current(), 0);

        assert!(doc.redo());
        assert_eq!(doc.song().bpm, 140.0);
        assert_eq!(doc.history_current(), 1);
    }

    /// jump_to は 1 発で任意 index の state へ遷移する (undo/redo を必要段数
    /// 繰り返したのと同じ結果)。
    #[test]
    fn jump_to_reaches_any_index() {
        let mut doc = SongDoc::new(Song::default());
        let base_bpm = doc.song().bpm;
        for (label, bpm) in [("A", 140.0), ("B", 150.0), ("C", 160.0)] {
            doc.begin_event(label);
            doc.edit(EditScope::Discrete, |s| s.bpm = bpm);
        }
        assert_eq!(doc.history_current(), 3);

        // 一気に baseline へ。
        assert!(doc.jump_to(0));
        assert_eq!(doc.history_current(), 0);
        assert_eq!(doc.song().bpm, base_bpm);

        // 一気に途中 (A の直後) へ。
        assert!(doc.jump_to(1));
        assert_eq!(doc.history_current(), 1);
        assert_eq!(doc.song().bpm, 140.0);

        // 一気に最新へ。
        assert!(doc.jump_to(3));
        assert_eq!(doc.history_current(), 3);
        assert_eq!(doc.song().bpm, 160.0);

        // current / 範囲外 は no-op。
        assert!(!doc.jump_to(3), "current へは no-op");
        assert!(!doc.jump_to(4), "範囲外は no-op");
        assert_eq!(doc.song().bpm, 160.0);
    }

    /// jump 後に新規編集すると redo 分岐は破棄される (linear undo の一貫性)。
    #[test]
    fn edit_after_jump_truncates_future() {
        let mut doc = SongDoc::new(Song::default());
        for (label, bpm) in [("A", 140.0), ("B", 150.0)] {
            doc.begin_event(label);
            doc.edit(EditScope::Discrete, |s| s.bpm = bpm);
        }
        doc.jump_to(1); // A の直後、 B は redo 待ち。
        doc.begin_event("C");
        doc.edit(EditScope::Discrete, |s| s.bpm = 170.0);
        // B は捨てられ、 A → C の直線履歴になる。
        assert_eq!(doc.history_labels(), vec![BASELINE_LABEL, "A", "C"]);
        assert_eq!(doc.history_current(), 2);
        assert!(!doc.can_redo());
    }

    /// no-op 編集 (edit_checked が false) は履歴に step を足さない。
    #[test]
    fn noop_edit_adds_no_labeled_step() {
        let mut doc = SongDoc::new(Song::default());
        doc.begin_event("A");
        doc.edit(EditScope::Discrete, |s| s.bpm = 140.0);
        let before = doc.history_labels();
        doc.begin_event("no-op");
        doc.edit_checked(EditScope::Discrete, |_s| false);
        assert_eq!(doc.history_labels(), before, "no-op は履歴を汚さない");
        assert_eq!(doc.history_current(), 1);
    }
}
