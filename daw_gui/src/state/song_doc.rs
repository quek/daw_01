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
//! - 子プロセス sync は runner の frame flush が `edit_epoch !=
//!   last_synced_epoch` を見て pull する (state/sync.rs)。
//! - undo/redo も epoch を bump する (= flush が LoadSong を再送する)。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::model::Song;

/// Undo 履歴の上限 (snapshot 方式)。
const UNDO_LIMIT: usize = 200;

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
    /// 保存先 (.daw)。 未保存プロジェクトは `None`。
    pub file_path: Option<PathBuf>,

    undo_stack: VecDeque<Song>,
    redo_stack: VecDeque<Song>,
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
            file_path: None,
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
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
            if self.undo_stack.len() >= UNDO_LIMIT {
                self.undo_stack.pop_front();
            }
            self.undo_stack.push_back(self.song.clone());
        }
        let (r, changed) = f(&mut self.song);
        if changed {
            // redo は「実際に編集が起きた」 ときだけ無効化する (no-op で
            // redo 履歴を消さない)。
            self.redo_stack.clear();
            self.last_gesture = match scope {
                EditScope::Discrete => None,
                EditScope::Gesture(id) => Some(id),
            };
            self.edit_epoch += 1;
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
        self.edit_epoch += 1;
        Some(r)
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

    /// dirty = 「最後に保存した epoch から編集が進んだ」 の O(1) 派生。
    pub fn is_dirty(&self) -> bool {
        self.edit_epoch != self.saved_epoch
    }

    /// save 完了時に呼ぶ: 現在の epoch を保存済みベースラインにする。
    pub fn mark_saved(&mut self) {
        self.saved_epoch = self.edit_epoch;
    }

    // -------- undo / redo ---------------------------------------------------

    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo_stack.pop_back() else {
            return false;
        };
        let current = std::mem::replace(&mut self.song, prev);
        self.redo_stack.push_back(current);
        self.after_history_jump();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo_stack.pop_back() else {
            return false;
        };
        let current = std::mem::replace(&mut self.song, next);
        self.undo_stack.push_back(current);
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

    fn after_history_jump(&mut self) {
        // undo/redo も epoch を bump する (frame flush が LoadSong を再送する)。
        // saved_epoch とは一致しなくなるので dirty になる (epoch 方式では
        // 「undo で保存時点に戻ったら clean」 は表現しない — O(1) 化の意図的
        // トレードオフ、 docs/plan_arch_refactor.md §7.5)。
        self.edit_epoch += 1;
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
        self.last_gesture = None;
        self.edit_epoch += 1;
        self.saved_epoch = self.edit_epoch;
    }

    // -------- gesture scopes -------------------------------------------------

    /// AppEvent dispatch の冒頭で呼ぶ: この event の ambient scope を確定する。
    /// interaction gesture 中はその id、 それ以外は fresh id (= 1 event 内の
    /// 複数 edit は squash、 event 間は独立)。
    pub fn begin_event(&mut self) {
        let id = match self.active_gesture {
            Some(id) => id,
            None => self.alloc_gesture(),
        };
        self.event_scope = EditScope::Gesture(id);
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
