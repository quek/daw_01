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

use common::model::{Device, NativeDevice, Parallel, ParallelChain, PluginInstance, Song, StructureWatch};

use super::node_index::NodeIndex;

/// Undo 履歴の上限 (snapshot 方式)。
const UNDO_LIMIT: usize = 200;

/// 履歴リスト先頭 (= どの編集にも遡れる起点) の表示ラベル。 New / Open /
/// Recovery の直後に確定する baseline state の名前。
pub const BASELINE_LABEL: &str = "初期状態";

/// 操作名を持たない編集の履歴ラベル。 [`crate::event::AppEvent::undo_label`] の既定
/// (Song を変えない event) と、 dispatch の外で走った編集 ([`SongDoc::end_event`]) が使う。
/// gesture の squash 中は、後から来た具体的な名前がこれを置き換える ([`SongDoc::edit`])。
pub const GENERIC_UNDO_LABEL: &str = "編集";

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

/// [`SongDoc::enter_own_gesture`] が退避した独立 step と scope (と、その間だけ差し替えた履歴ラベル)。
#[derive(Debug, Clone, Copy)]
pub struct GestureSave {
    own_step: Option<u64>,
    scope: EditScope,
    label: &'static str,
}

/// Begin/End bracket ([`SongDoc::begin_gesture`]) の所有者。bracket は所有者ごとに開いて閉じ、**所有者が 1 つでも
/// 残っている間は 1 つの undo step** になる。進行中の bracket の中で始まった bracket は外側の step に入り、内側の
/// End は外側を閉じない (録音 take の最中に回したツマミ / 数値欄 / 色で take が割れない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GestureOwner {
    /// 録音 take (r.md #51)。
    RecordingTake,
    /// パラメーターのジェスチャー (`RecordingState::active_param_gestures` が空でない間)。
    ParamGestures,
    /// スクラブ欄の drag / text 編集 (`BeginInspectorScrub`、`view::scrub_gesture`)。
    InspectorScrub,
    /// group transform の scrub / preview drag (`BeginGroupTransformDrag`)。
    GroupTransformDrag,
    /// preview canvas 上の image PiP drag。
    ImagePipDrag,
    /// preview canvas 上の text PiP drag。
    TextPipDrag,
    /// フォントピッカーの session (開いてから確定 / 取り消しまで)。
    FontPicker,
    /// カラーピッカーの session (開いてから閉じるまで)。
    ColorPicker,
}

/// 開いている Begin/End bracket: 1 つの undo step (`id`) と、それを開いている所有者 (開いた順)。
#[derive(Debug, Clone)]
struct GestureBracket {
    id: u64,
    owners: Vec<GestureOwner>,
}

/// 履歴の動かし方 ([`SongDoc::jump`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryJump {
    /// 1 段戻る。
    Undo,
    /// 1 段進む。
    Redo,
    /// 履歴リストの行へ一気に遡る / 進む。行は **その state の識別子** ([`SongDoc::history_state_id`]) で指す — 位置は、
    /// 発注から実行までの間 (plugin state の往復待ち) に積まれた編集で詰まって別の state を指す。
    ToState(u64),
}

/// [`SongDoc::begin_event`] が返す、[`SongDoc::end_event`] で閉じるための控え。
#[derive(Debug, Clone, Copy)]
#[must_use]
pub struct EventSave {
    /// 入れ子の event なら、外側の event の履歴ラベル (閉じたときに戻す)。外側の event が
    /// 無い (dispatch の一番外) なら `None`。
    outer_label: Option<&'static str>,
}

impl EventSave {
    /// dispatch の一番外の event か (handler の中から呼ばれた入れ子でない)。
    #[must_use]
    pub fn is_outermost(&self) -> bool {
        self.outer_label.is_none()
    }
}

/// Begin/End bracket を持たない連続編集源の識別子
/// ([`SongDoc::use_stream_scope`] のキー)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamGesture {
    /// Transport bar の BPM scrubable_number ドラッグ。
    BpmScrub,
    /// Transport bar の拍子分子 scrubable_number ドラッグ。
    TimeSigScrub,
    /// r.md #130: Transport bar の Transpose 欄のドラッグ。
    TransposeScrub,
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
    /// この state の識別子 ([`SongDoc::state_id`])。 undo / redo で live に戻すとき
    /// 一緒に戻し、 保存時点の state と同じなら clean と判定できるようにする。
    state_id: u64,
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
    /// live state の識別子。 **中身が変わるたびに新しい値**、 undo / redo は履歴に
    /// 積んであった値を **戻す** (epoch と違って単調でない)。 dirty はこれと
    /// `saved_state_id` の比較 = 「undo で保存時点に戻ったら clean」 が O(1) で出る
    /// (r.md #102)。 派生キャッシュの世代キーには使わない (同じ id が再登場するため、
    /// そちらは単調な `edit_epoch`)。
    state_id: u64,
    /// 最後に save / load / new した時点の `state_id`。 dirty = 不一致。
    saved_state_id: u64,
    /// `state_id` の allocator (単調)。
    next_state_id: u64,
    /// 「Song の中身が変わった」世代 (子プロセス sync が読む)。`edit_epoch` が進む
    /// ときは必ず進み、加えて [`SongDoc::edit_playback`] でも進む。
    sync_epoch: u64,
    /// live の **id 構造** (トラック / device node / lane / 変調 / binding の id と束縛。範囲は
    /// [`StructureWatch`] の doc) の観測。編集後の不変条件の回復は、これが変わった編集でだけ回す。
    structure: StructureWatch,
    /// id 構造が変わった世代 ([`SongDoc::structure_epoch`])。
    structure_epoch: u64,
    /// device node の id → 位置 ([`SongDoc::device_node`] / [`SongDoc::chain_node`])。`structure_epoch` が
    /// 進むたびに作り直す (構造が同じ間は位置が変わらない)。
    nodes: NodeIndex,
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
    /// Begin*/End* で bracket される interaction gesture (録音 take / pointer drag / scrub / picker session)。
    /// `Some` の間、 ambient scope はこの id を使い、所有者が全員閉じるまでの編集が 1 undo step に squash される。
    bracket: Option<GestureBracket>,
    /// 非同期の完了ハンドラが開いた独立 step ([`SongDoc::enter_own_gesture`])。`Some` の間は bracket より優先する。
    own_step: Option<u64>,
    /// 現在 dispatch 中の AppEvent に割り当てた ambient scope
    /// ([`SongDoc::begin_event`] が設定)。 1 event 内の複数 edit_song 呼び出し
    /// (ループ / helper 連鎖) が 1 undo step に squash されることを保証する。
    event_scope: EditScope,
    /// AppEvent の dispatch 中か ([`SongDoc::begin_event`] 〜 [`SongDoc::end_event`])。
    /// dispatch 中に始まった event は入れ子 (handler が中で `handle_event` を呼んだ) で、
    /// 外側の操作の scope に入る。
    in_event: bool,
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
    pub fn new(mut song: Song) -> Self {
        // r.md #129: 編集後の不変条件 (組み込みの補充 / dangling の掃除)。baseline 確定前なので
        // `*` は立たない。
        let mut structure = StructureWatch::default();
        song.enforce_edit_invariants_watched(&mut structure);
        let mut nodes = NodeIndex::default();
        nodes.rebuild(&song);
        Self {
            song,
            edit_epoch: 1,
            state_id: 1,
            saved_state_id: 1,
            next_state_id: 2,
            sync_epoch: 1,
            structure,
            structure_epoch: 1,
            nodes,
            file_path: None,
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
            current_label: BASELINE_LABEL,
            pending_label: GENERIC_UNDO_LABEL,
            last_gesture: None,
            next_gesture_id: 1,
            bracket: None,
            own_step: None,
            event_scope: EditScope::Discrete,
            in_event: false,
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
                state_id: self.state_id,
            });
        }
        let (r, mut changed) = f(&mut self.song);
        // r.md #129: 編集後の不変条件 (組み込みの正規化 + dangling な lane / routing / binding の
        // 掃除) は **同じ undo step** で回復する。handler 側に prune を書かない。
        changed |= self.enforce_invariants();
        if changed {
            self.state_id = self.alloc_state_id();
            // redo は「実際に編集が起きた」 ときだけ無効化する (no-op で
            // redo 履歴を消さない)。
            self.redo_stack.clear();
            // 新しい live state を生んだのは今回の編集イベント。 そのラベルを
            // current に昇格する。 gesture の squash 中は **最初に名前を持った event が
            // step の名前** で、後から同じ step へ書いた event は上書きしない (録音 take を
            // 停止 / tick で閉じても「MIDI 入力」 が別の名前に化けない)。
            if !squash || self.current_label == GENERIC_UNDO_LABEL {
                self.current_label = self.pending_label;
            }
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
        self.enforce_invariants();
        self.state_id = self.alloc_state_id();
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
        let changed = f(&mut self.song) | self.enforce_invariants();
        if changed {
            self.state_id = self.alloc_state_id();
            self.bump_edit_epoch();
        }
        Some(changed)
    }

    /// 編集後の不変条件を、id 構造が前回の回復から変わったときだけ回復する (値だけの編集で曲全体の
    /// node 表を作り直さない)。構造が変わっていれば `structure_epoch` を進める。戻り値 = 回復で中身が変わったか。
    fn enforce_invariants(&mut self) -> bool {
        let outcome = self.song.enforce_edit_invariants_watched(&mut self.structure);
        if outcome.structure_changed {
            self.advance_structure();
        }
        outcome.changed
    }

    /// id 構造が変わった: 世代を進め、node の索引を今の Song から作り直す (この 2 つは必ず一緒に動く)。
    fn advance_structure(&mut self) {
        self.structure_epoch += 1;
        self.nodes.rebuild(&self.song);
    }

    /// live の id 構造 (トラック / device node / lane / 変調 / MIDI binding の id と束縛、範囲は
    /// [`StructureWatch`]) が変わった世代。単調増加。編集 / 正規化 / undo / redo / 履歴ジャンプで構造が
    /// 実際に変わったときと、[`SongDoc::replace_song`] (同じ id が別の曲の物を指す) で進む。
    /// id を鍵にした session 状態の後始末を、構造が変わったときだけ回すために使う。
    /// clip / content / media source / section / scene の id と値の変化では進まない。
    pub fn structure_epoch(&self) -> u64 {
        self.structure_epoch
    }

    // -------- device node の引き (id 構造の世代つき索引) --------------------

    /// `id` の device と置き場のトラック id (`MASTER_TRACK_ID` = master)。`Song::device_by_id` と
    /// `Song::device_owner_track` と同じ答えを、id 構造の世代つき索引 ([`NodeIndex`]) で位置をたどって返す
    /// (曲全体の木を走査しない)。**描画のように毎フレーム id で node を引く読みはこちらを使う**。
    /// `edit` の closure の中 (構造を変えている最中の `&mut Song`) では索引が使えないので `Song` の方を使う。
    pub fn device_node(&self, id: u64) -> Option<(&Device, u32)> {
        let found = self.nodes.device(&self.song, id);
        debug_assert!(found.is_ok(), "node 索引が id 構造の変化を取りこぼした (device {id})");
        found.unwrap_or_else(|_| self.song.device_by_id(id).zip(self.song.device_owner_track(id)))
    }

    /// `id` の chain と親 Parallel、置き場のトラック id ([`Self::device_node`] の chain 版、`Song::chain_by_id` と
    /// `Song::chain_owner_track` と同じ答え)。
    pub fn chain_node(&self, id: u64) -> Option<(&Parallel, &ParallelChain, u32)> {
        let found = self.nodes.chain(&self.song, id);
        debug_assert!(found.is_ok(), "node 索引が id 構造の変化を取りこぼした (chain {id})");
        found.unwrap_or_else(|_| {
            let (parallel, chain) = self.song.chain_by_id(id)?;
            Some((parallel, chain, self.song.chain_owner_track(common::model::ChainRef::Chain(id))?))
        })
    }

    /// `Song::device_by_id` を索引で ([`Self::device_node`])。
    pub fn device_by_id(&self, id: u64) -> Option<&Device> {
        self.device_node(id).map(|(device, _)| device)
    }

    /// `Song::plugin_by_id` を索引で ([`Self::device_node`])。
    pub fn plugin_by_id(&self, id: u64) -> Option<&PluginInstance> {
        self.device_by_id(id)?.as_plugin()
    }

    /// `Song::native_by_id` を索引で ([`Self::device_node`])。
    pub fn native_by_id(&self, id: u64) -> Option<&NativeDevice> {
        self.device_by_id(id)?.as_native()
    }

    /// `Song::parallel_by_id` を索引で ([`Self::device_node`])。
    pub fn parallel_by_id(&self, id: u64) -> Option<&Parallel> {
        self.device_by_id(id)?.as_parallel()
    }

    /// `Song::device_owner_track` を索引で ([`Self::device_node`])。
    pub fn device_owner_track(&self, id: u64) -> Option<u32> {
        self.device_node(id).map(|(_, owner)| owner)
    }

    /// `Song::chain_by_id` を索引で ([`Self::chain_node`])。
    pub fn chain_by_id(&self, id: u64) -> Option<(&Parallel, &ParallelChain)> {
        self.chain_node(id).map(|(parallel, chain, _)| (parallel, chain))
    }

    /// `Song::chain_owner_track(ChainRef::Chain(id))` を索引で ([`Self::chain_node`])。
    pub fn chain_owner_track(&self, id: u64) -> Option<u32> {
        self.chain_node(id).map(|(_, _, owner)| owner)
    }

    /// `Song::bound_owner_track` と同じ答え。node で束縛する住所 (plugin / native / chain / Parallel、
    /// `AutomationTarget::bound_node_id`) の持ち主は索引で引き、それ以外 (変調 / song 全体 / 束縛しない住所) は
    /// `Song` の同じ関数に任せる (木を走査しない住所だけが残る)。録音の tick のように繰り返し引く口で使う。
    pub fn bound_owner_track(&self, target: &common::model::AutomationTarget) -> Option<u32> {
        use common::model::{AutomationTarget as T, TrackBuiltinParam as B};
        match (target, target.bound_node_id()) {
            (T::TrackBuiltin(B::ChainGain { .. } | B::ChainPan { .. }), Some(chain_id)) => self.chain_owner_track(chain_id),
            (_, Some(device_id)) => self.device_owner_track(device_id),
            (_, None) => self.song.bound_owner_track(target),
        }
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

    /// 新しい live state の識別子を切る (中身が変わった瞬間に呼ぶ)。
    fn alloc_state_id(&mut self) -> u64 {
        let id = self.next_state_id;
        self.next_state_id += 1;
        id
    }

    /// plugin state blob の write-back (`RequestAllStates` 応答) **専用**。live と、undo / redo に積んである **全** Song
    /// へ同じ `f` を掛ける — plugin state は履歴に属さない (host が持つ最新が正で、undo でツマミは戻らない) ので、
    /// どの経路 (undo / redo / 履歴ジャンプ / 再有効化) で device を host へ載せ直しても最新の値で載るようにする。
    /// blob は host が真実源で wire (LoadSong) からも構造的に除外されているため、
    /// undo / epoch / dirty / 子プロセス sync のどれにも影響しない。
    /// ユーザー編集には決して使わないこと。
    pub fn write_back_plugin_state(&mut self, mut f: impl FnMut(&mut Song)) {
        f(&mut self.song);
        for e in self.undo_stack.iter_mut().chain(self.redo_stack.iter_mut()) {
            f(&mut e.song);
        }
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
        self.state_id != self.saved_state_id
    }

    /// save 完了時に呼ぶ: 現在の state を保存済みベースラインにする。
    pub fn mark_saved(&mut self) {
        self.saved_state_id = self.state_id;
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
        self.jump(HistoryJump::Undo)
    }

    pub fn redo(&mut self) -> bool {
        self.jump(HistoryJump::Redo)
    }

    /// 履歴を `jump` の行き先へ動かす。undo / redo を必要段数ぶん繰り返すのと等価だが、中間 state を経由した
    /// 副作用 (epoch bump / 構造の観測) は出さず **1 回だけ** 出す (caller が 1 度 reconcile する)。行き先が無い
    /// (端 / 今の state / 履歴に居ない state) ときは `false` (no-op)。
    ///
    /// export 中は [`Self::edit`] と同じく **拒否** する (`false` + status message 予約) — 履歴ジャンプも live の
    /// Song を差し替えるので、ここを素通しにすると render 中の song が入れ替わる (song 凍結の単一保証点)。
    pub fn jump(&mut self, jump: HistoryJump) -> bool {
        if self.export_lock {
            self.rejection = Some("書き出し中は編集できません");
            return false;
        }
        let Some(depth) = self.jump_depth(jump) else {
            return false;
        };
        while self.undo_stack.len() > depth {
            self.step_backward();
        }
        while self.undo_stack.len() < depth {
            self.step_forward();
        }
        self.after_history_jump();
        true
    }

    /// `jump` の行き先の Song を、動かさずに覗く。行き先が無ければ `None`。履歴ジャンプで plugin host から降りる
    /// device を、動かす **前** に求めるために読む (降ろす前に plugin state を取り寄せる)。
    pub fn history_target(&self, jump: HistoryJump) -> Option<&Song> {
        self.jump_entry(jump).map(|e| &e.song)
    }

    /// `jump` の **今の** 行き先を state の識別子で固定した形 ([`HistoryJump::ToState`])。行き先が無ければ `None`。
    /// 発注から実行まで待つジャンプを、待つ間に入った編集で「1 段前」がずれないように固定する。
    pub fn pin_jump(&self, jump: HistoryJump) -> Option<HistoryJump> {
        self.jump_entry(jump).map(|e| HistoryJump::ToState(e.state_id))
    }

    fn jump_entry(&self, jump: HistoryJump) -> Option<&HistoryEntry> {
        let depth = self.jump_depth(jump)?;
        let undo = self.undo_stack.len();
        Some(if depth < undo { &self.undo_stack[depth] } else { &self.redo_stack[undo + self.redo_stack.len() - depth] })
    }

    /// [`Self::history_labels`] の `index` 番目の state の識別子 ([`HistoryJump::ToState`] に渡す)。範囲外は `None`。
    pub fn history_state_id(&self, index: usize) -> Option<u64> {
        let (undo, redo) = (self.undo_stack.len(), self.redo_stack.len());
        match index.cmp(&undo) {
            std::cmp::Ordering::Less => Some(self.undo_stack[index].state_id),
            std::cmp::Ordering::Equal => Some(self.state_id),
            // redo_stack は back が次の redo 先 (履歴リストでは現在行の直後)。
            std::cmp::Ordering::Greater => (index <= undo + redo).then(|| self.redo_stack[undo + redo - index].state_id),
        }
    }

    /// `jump` の行き先で undo stack が何段になるか ([`Self::history_current`] の行き先)。行き先が無ければ `None`。
    fn jump_depth(&self, jump: HistoryJump) -> Option<usize> {
        let (undo, redo) = (self.undo_stack.len(), self.redo_stack.len());
        match jump {
            HistoryJump::Undo => undo.checked_sub(1),
            HistoryJump::Redo => (redo > 0).then_some(undo + 1),
            // 今の state は stack に居ないので `None` (= no-op)。redo_stack の `q` 番目へは `redo - q` 段進む。
            HistoryJump::ToState(id) => self.undo_stack.iter().position(|e| e.state_id == id).or_else(|| {
                self.redo_stack.iter().position(|e| e.state_id == id).map(|q| undo + redo - q)
            }),
        }
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
        // 採番の状態も履歴に属さない — 戻した物の id を別の新しい物に振らない (不変条件 1)。
        self.song.carry_id_high_water_from(&current);
        self.redo_stack.push_back(HistoryEntry {
            song: current,
            label: self.current_label,
            state_id: self.state_id,
        });
        self.current_label = prev.label;
        self.state_id = prev.state_id;
    }

    /// redo 1 段: [`SongDoc::step_backward`] の対称。
    fn step_forward(&mut self) {
        let next = self.redo_stack.pop_back().expect("caller guarantees non-empty");
        let current = std::mem::replace(&mut self.song, next.song);
        self.song.carry_playback_state_from(&current);
        self.song.carry_id_high_water_from(&current);
        self.undo_stack.push_back(HistoryEntry {
            song: current,
            label: self.current_label,
            state_id: self.state_id,
        });
        self.current_label = next.label;
        self.state_id = next.state_id;
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
    /// 各行に描く。 index の行の state は [`SongDoc::history_state_id`] で引く。
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
        // undo/redo も epoch を bump する (frame flush が LoadSong を再送する、
        // 派生キャッシュも作り直す)。 dirty は epoch ではなく `state_id` で見るので、
        // 保存時点の state に戻れば clean になる (r.md #102)。
        self.bump_edit_epoch();
        // 差し替えた Song の構造を観測する。履歴の Song は回復済みのはずだが、それを前提にはしない
        // (観測しただけの構造は次の編集で必ず回復を回す、`StructureWatch::observe`)。
        if self.structure.observe(&self.song) {
            self.advance_structure();
        }
        // gesture squash chain は履歴 jump を跨がない (跨ぐと drag 再開時の
        // snapshot が skip され、 undo 1 回分の状態が履歴から欠落する)。
        self.last_gesture = None;
    }

    /// New / Open / Recovery: song を丸ごと差し替え、 履歴を破棄して clean に
    /// する。 (save は履歴を残したいので [`SongDoc::mark_saved`] を使う。)
    pub fn replace_song(&mut self, song: Song) {
        self.song = song;
        // r.md #129: baseline 確定前に不変条件を回復する (load 経路は正規化済みなので no-op、
        // script 経路はここで dangling の連鎖掃除まで揃う)。`*` は立たない。
        self.song.enforce_edit_invariants_watched(&mut self.structure);
        // 構造が同じでも、同じ id は別の曲の物を指す (世代は必ず進め、索引も必ず作り直す)。
        self.advance_structure();
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.current_label = BASELINE_LABEL;
        self.last_gesture = None;
        self.state_id = self.alloc_state_id();
        self.saved_state_id = self.state_id;
        self.bump_edit_epoch();
    }

    /// 読み込み時の**不変条件の回復**で中身が変わったことを記録し、`*` (未保存) を立てる。
    /// クリップの重なり解消 (`docs/plan_range_selection.md` §6.4) がこれを使う。
    ///
    /// 履歴 (undo) は積まない — 「元に戻せる編集」 ではなく、不変条件を満たさない
    /// ファイルを読み込んだ結果の修復だから。 解消は冪等なので、一度保存すれば
    /// 次に開いたときは立たない。
    pub fn mark_dirty_after_load_fixup(&mut self) {
        self.state_id = self.alloc_state_id();
        self.bump_edit_epoch();
    }

    // -------- gesture scopes -------------------------------------------------

    /// AppEvent dispatch の冒頭で呼ぶ: この event の ambient scope を確定する。
    /// interaction gesture 中はその id、 それ以外は fresh id (= 1 event 内の
    /// 複数 edit は squash、 event 間は独立)。 `label` は この event が edit() で
    /// snapshot を積んだときの履歴リスト用ラベル (r.md #29)。
    ///
    /// **dispatch 中に呼ばれたら入れ子** (handler が中で `handle_event` を呼んだ): ユーザーの
    /// 操作は外側の 1 回なので scope を開き直さず外側の step に入れ、名前は外側が持っていない
    /// ときだけ入れ子のものを使う。 戻り値は必ず [`Self::end_event`] へ渡す。
    pub fn begin_event(&mut self, label: &'static str) -> EventSave {
        if self.in_event {
            let save = EventSave { outer_label: Some(self.pending_label) };
            if self.pending_label == GENERIC_UNDO_LABEL {
                self.pending_label = label;
            }
            return save;
        }
        self.in_event = true;
        let id = match self.ambient_gesture() {
            Some(id) => id,
            None => self.alloc_gesture(),
        };
        self.event_scope = EditScope::Gesture(id);
        // この event が edit() で snapshot を積んだら、 この label が新 step の
        // 名前になる。 編集しない event では未使用のまま [`Self::end_event`] で汎用へ戻る。
        self.pending_label = label;
        EventSave { outer_label: None }
    }

    /// AppEvent dispatch の末尾で呼ぶ: [`Self::begin_event`] で開いた event を閉じる。
    ///
    /// 一番外の event を閉じないと、dispatch の **外** で走った編集 (view が handler を直接
    /// 呼ぶ等) が直前の event の scope とラベルを引き継ぎ、その event の undo step に黙って
    /// 吸収される (1 操作 = 1 undo が崩れ、undo 1 回で 2 つの操作が戻る)。閉じた後の
    /// 編集は独立した step ([`GENERIC_UNDO_LABEL`]) になる。Begin/End bracket の最中なら
    /// その bracket に入る (bracket は event を跨いで続くもの)。
    ///
    /// 入れ子の event は外側の名前だけ戻し、scope は触らない (外側の handler の続きの編集も
    /// 同じ step。入れ子の中で始まった / 終わった bracket はそのまま効く)。
    pub fn end_event(&mut self, save: EventSave) {
        if let Some(label) = save.outer_label {
            self.pending_label = label;
            return;
        }
        self.in_event = false;
        self.event_scope = self.ambient_gesture().map_or(EditScope::Discrete, EditScope::Gesture);
        self.pending_label = GENERIC_UNDO_LABEL;
    }

    /// 現在 dispatch 中 event の履歴ラベル。完了を待つ操作 (plugin state の往復待ちの
    /// 編集 / 焼き込みの適用) が **発注した時点で** 控え、完了時に
    /// [`Self::enter_own_gesture`] へ渡す (完了を運ぶ IPC event の名前で積まない)。
    pub fn event_label(&self) -> &'static str {
        self.pending_label
    }

    /// 現在 dispatch 中 event の ambient scope。
    pub fn event_scope(&self) -> EditScope {
        self.event_scope
    }

    /// Begin* (録音 take / scrub / drag / picker session) ハンドラが呼ぶ: `owner` が
    /// [`Self::end_gesture`] するまでの全 event の編集を 1 undo step に bracket する。
    ///
    /// **進行中の bracket の中で始まったら外側の step に入る** (入れ子の `handle_event` / `use_stream_scope` と同じ
    /// 規則)。bracket は所有者が全員閉じるまで続くので、内側の End は外側を閉じない。同じ所有者が閉じずに開き直した
    /// (ピッカーを開いたまま別の対象で開く) ときは前の session を閉じてから開く — 他に所有者が居なければ新しい step、
    /// 居ればその step のまま。
    pub fn begin_gesture(&mut self, owner: GestureOwner) {
        self.end_gesture(owner);
        match &mut self.bracket {
            Some(bracket) => bracket.owners.push(owner),
            None => {
                let id = self.alloc_gesture();
                self.bracket = Some(GestureBracket { id, owners: vec![owner] });
            }
        }
        // Begin と同一 event 内の後続 edit も gesture に含める (非同期完了の独立 step の中ならそちらが優先)。
        if let Some(id) = self.ambient_gesture() {
            self.event_scope = EditScope::Gesture(id);
        }
    }

    /// End* ハンドラが呼ぶ: `owner` の bracket を閉じる。他の所有者が残っていれば step は続く。`owner` が開いて
    /// いなければ何もしない (確定と取り消しの両方が閉じる口でも、他の所有者の bracket に触れない)。
    pub fn end_gesture(&mut self, owner: GestureOwner) {
        let Some(bracket) = &mut self.bracket else {
            return;
        };
        bracket.owners.retain(|&o| o != owner);
        if bracket.owners.is_empty() {
            self.bracket = None;
        }
    }

    /// interaction gesture (Begin/End bracket) が進行中か (所有者を問わない)。
    pub fn gesture_active(&self) -> bool {
        self.bracket.is_some()
    }

    /// `owner` の bracket が開いているか。
    pub fn gesture_open(&self, owner: GestureOwner) -> bool {
        self.bracket.as_ref().is_some_and(|b| b.owners.contains(&owner))
    }

    /// いま編集が入る gesture: 非同期完了の独立 step が最優先、次に開いている bracket。
    fn ambient_gesture(&self) -> Option<u64> {
        self.own_step.or(self.bracket.as_ref().map(|b| b.id))
    }

    /// **進行中の Begin/End bracket を壊さずに**、以後の編集を `label` の名前の
    /// 1 undo step へ束ねる。戻り値を [`Self::leave_own_gesture`] へ渡して必ず閉じること。
    ///
    /// `begin_gesture` / `end_gesture` を直に使うと、**非同期の完了ハンドラ**
    /// (Glue の焼き込み適用など、ユーザー操作と無関係な時点で走るもの) が
    /// ユーザーのドラッグ中の bracket に入ってしまう。独立 step は bracket より優先する別の層なので、
    /// 中で bracket が開閉してもそれは bracket 側に残る。
    /// `label` は発注した操作の名前 ([`Self::event_label`] で控えたもの)。
    #[must_use]
    pub fn enter_own_gesture(&mut self, label: &'static str) -> GestureSave {
        let save = GestureSave { own_step: self.own_step, scope: self.event_scope, label: self.pending_label };
        let id = self.alloc_gesture();
        self.own_step = Some(id);
        self.event_scope = EditScope::Gesture(id);
        self.pending_label = label;
        save
    }

    /// [`Self::enter_own_gesture`] の対。退避しておいた独立 step / scope とラベルを戻す。
    pub fn leave_own_gesture(&mut self, save: GestureSave) {
        self.own_step = save.own_step;
        self.event_scope = save.scope;
        self.pending_label = save.label;
    }

    /// Begin/End bracket を持たない連続編集源 (MIDI CC / BPM scrub /
    /// automation 録音) 用の scope。 同一 key の編集が [`STREAM_GESTURE_GAP`]
    /// 以内に連続する間は同じ gesture id を返す (= 1 burst = 1 undo step)。
    fn stream_scope(&mut self, key: StreamGesture) -> EditScope {
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

    /// 現在 dispatch 中 event の ambient scope を連続入力 `key` の gesture に差し替える。
    ///
    /// `begin_event` が張った「1 event = 1 undo step」 の scope を、 連続入力用の
    /// gesture へ **上書き** する。 handler 冒頭で 1 度呼べば、 その event 内の
    /// `edit_song` の入れ子もすべて同じ gesture に入るので、 同じ key の編集が
    /// [`STREAM_GESTURE_GAP`] 以内に続く限り 1 undo step に畳まれる (r.md #67 の
    /// カーソルキー nudge)。 1 秒空けば次は新しい step。 **scope は編集ごとに渡さず必ずこれで
    /// 張る** — 1 本の編集だけを stream に入れると、同じ event の helper の編集 (Raw クリップの
    /// 追従 / レーンの自動生成) と step が交互に割れる。
    ///
    /// Begin/End bracket の最中 (録音 take / ツマミのドラッグ …) は bracket に入る — bracket は
    /// event を跨いで続く 1 操作なので、途中の連続入力 (take 中に回した CC など) で割らない。
    pub fn use_stream_scope(&mut self, key: StreamGesture) {
        self.event_scope = match self.ambient_gesture() {
            Some(id) => EditScope::Gesture(id),
            None => self.stream_scope(key),
        };
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

    /// r.md #102: undo で保存時点の state に戻れば clean、 redo で先へ進めば dirty。
    /// 保存後に undo した state を再編集して redo 枝を捨てると、 保存時点には戻れない
    /// ので dirty のまま。
    #[test]
    fn undo_back_to_saved_state_is_clean() {
        let mut doc = SongDoc::new(Song::default());
        doc.edit(EditScope::Discrete, |s| s.bpm = 100.0);
        doc.mark_saved();
        assert!(!doc.is_dirty());
        doc.edit(EditScope::Discrete, |s| s.bpm = 110.0);
        assert!(doc.is_dirty(), "保存後の編集は dirty");
        assert!(doc.undo());
        assert!(!doc.is_dirty(), "undo で保存時点に戻ったら clean");
        assert!(doc.redo());
        assert!(doc.is_dirty(), "redo で先へ進めば dirty");
        assert!(doc.undo());
        assert!(doc.undo(), "保存時点より前へ");
        assert!(doc.is_dirty(), "保存時点より前も dirty");
        assert!(doc.redo());
        assert!(!doc.is_dirty(), "redo で保存時点へ戻れば clean");
        // 保存時点より前で別の編集 → redo 枝が消え、 保存時点へは戻れない。
        assert!(doc.undo());
        doc.edit(EditScope::Discrete, |s| s.bpm = 90.0);
        assert!(doc.is_dirty());
        assert!(doc.undo());
        assert!(doc.is_dirty(), "保存時点の state は履歴から消えたので dirty のまま");
        assert!(!doc.redo() || doc.is_dirty());
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

    /// `label` の 1 event の中で `f` を走らせる (dispatch の `begin_event` / `end_event` と同じ対)。
    fn in_event<R>(doc: &mut SongDoc, label: &'static str, f: impl FnOnce(&mut SongDoc) -> R) -> R {
        let event = doc.begin_event(label);
        let r = f(doc);
        doc.end_event(event);
        r
    }

    /// discrete edit を積むと、 各 state に begin_event で渡したラベルが付き、
    /// history_labels() が baseline → 最新の順で返す。
    #[test]
    fn history_labels_reflect_edits() {
        let mut doc = SongDoc::new(Song::default());
        assert_eq!(doc.history_labels(), vec![BASELINE_LABEL]);
        assert_eq!(doc.history_current(), 0);

        in_event(&mut doc, "テンポ変更", |doc| doc.edit(EditScope::Discrete, |s| s.bpm = 140.0));
        assert_eq!(doc.history_labels(), vec![BASELINE_LABEL, "テンポ変更"]);
        assert_eq!(doc.history_current(), 1);

        in_event(&mut doc, "音量変更", |doc| doc.edit(EditScope::Discrete, |s| s.bpm = 150.0));
        assert_eq!(doc.history_labels(), vec![BASELINE_LABEL, "テンポ変更", "音量変更"]);
        assert_eq!(doc.history_current(), 2);
    }

    /// 同一 gesture id の連続 edit は 1 step に squash され、 ラベルも 1 つ。
    #[test]
    fn gesture_squash_is_one_labeled_step() {
        let mut doc = SongDoc::new(Song::default());
        in_event(&mut doc, "音量変更", |doc| {
            doc.edit(EditScope::Gesture(7), |s| s.bpm = 130.0);
            doc.edit(EditScope::Gesture(7), |s| s.bpm = 131.0);
            doc.edit(EditScope::Gesture(7), |s| s.bpm = 132.0);
        });
        assert_eq!(doc.history_labels(), vec![BASELINE_LABEL, "音量変更"]);
        assert_eq!(doc.history_current(), 1);
    }

    /// event を閉じた後の編集 (dispatch の外) は、直前の event の step に吸収されない。
    /// Begin/End bracket の最中は bracket に入り、step の名前は最初に名前を持った event のまま。
    /// bracket の最中の連続入力 (録音 take 中に回した CC) も bracket を割らない。
    #[test]
    fn edit_after_end_event_is_its_own_step() {
        let mut doc = SongDoc::new(Song::default());
        in_event(&mut doc, "テンポ変更", |doc| doc.edit(doc.event_scope(), |s| s.bpm = 140.0));
        doc.edit(doc.event_scope(), |s| s.bpm = 150.0);
        assert_eq!(doc.history_labels(), vec![BASELINE_LABEL, "テンポ変更", GENERIC_UNDO_LABEL]);

        in_event(&mut doc, "MIDI 入力", |doc| {
            doc.begin_gesture(GestureOwner::RecordingTake);
            doc.edit(doc.event_scope(), |s| s.bpm = 160.0);
        });
        doc.edit(doc.event_scope(), |s| s.bpm = 161.0);
        in_event(&mut doc, "MIDI コントロール", |doc| {
            doc.use_stream_scope(StreamGesture::MidiCc);
            doc.edit(doc.event_scope(), |s| s.bpm = 162.0);
        });
        in_event(&mut doc, "オートメーション録音", |doc| {
            doc.edit(doc.event_scope(), |s| s.bpm = 163.0);
            doc.end_gesture(GestureOwner::RecordingTake);
        });
        assert_eq!(doc.history_current(), 3, "bracket の中は 1 step");
        assert_eq!(doc.history_labels()[3], "MIDI 入力");
    }

    /// 進行中の bracket の中で始まった bracket は外側の step に入り、内側の End は外側を閉じない。所有者が全員
    /// 閉じるまで 1 step (開いた順と閉じた順が交差しても)。開いていない所有者の End は何も閉じない。
    #[test]
    fn nested_bracket_joins_the_outer_step_until_every_owner_closes() {
        let mut doc = SongDoc::new(Song::default());
        in_event(&mut doc, "MIDI 入力", |doc| {
            doc.begin_gesture(GestureOwner::RecordingTake);
            doc.edit(doc.event_scope(), |s| s.bpm = 140.0);
        });
        in_event(&mut doc, "音量変更", |doc| {
            doc.begin_gesture(GestureOwner::ParamGestures);
            doc.edit(doc.event_scope(), |s| s.bpm = 141.0);
        });
        in_event(&mut doc, "音量変更", |doc| doc.end_gesture(GestureOwner::ParamGestures));
        // 確定と取り消しの両方が閉じる口 (フォントピッカー) の 2 回目は、開いていない所有者の End。
        in_event(&mut doc, "フォント", |doc| doc.end_gesture(GestureOwner::FontPicker));
        assert!(doc.gesture_open(GestureOwner::RecordingTake), "内側の End で外側は閉じない");
        in_event(&mut doc, "MIDI 入力", |doc| doc.edit(doc.event_scope(), |s| s.bpm = 142.0));
        in_event(&mut doc, "色", |doc| {
            doc.begin_gesture(GestureOwner::ColorPicker);
            doc.edit(doc.event_scope(), |s| s.bpm = 143.0);
        });
        // 外側が先に閉じても、残った内側が閉じるまでは同じ step。
        in_event(&mut doc, "停止", |doc| doc.end_gesture(GestureOwner::RecordingTake));
        in_event(&mut doc, "色", |doc| doc.edit(doc.event_scope(), |s| s.bpm = 144.0));
        in_event(&mut doc, "色", |doc| doc.end_gesture(GestureOwner::ColorPicker));
        assert!(!doc.gesture_active());
        assert_eq!(doc.history_labels(), vec![BASELINE_LABEL, "MIDI 入力"], "take の間は 1 step");

        in_event(&mut doc, "テンポ変更", |doc| doc.edit(doc.event_scope(), |s| s.bpm = 150.0));
        assert_eq!(doc.history_current(), 2, "全員閉じた後の編集は別 step");

        // 同じ所有者が閉じずに開き直すと、他に所有者が居なければ新しい step。
        in_event(&mut doc, "色", |doc| {
            doc.begin_gesture(GestureOwner::ColorPicker);
            doc.edit(doc.event_scope(), |s| s.bpm = 151.0);
        });
        in_event(&mut doc, "色", |doc| {
            doc.begin_gesture(GestureOwner::ColorPicker);
            doc.edit(doc.event_scope(), |s| s.bpm = 152.0);
        });
        assert_eq!(doc.history_current(), 4);
    }

    /// handler の中で始まった event (入れ子) は外側の event の step に入り、閉じた後の外側の
    /// 編集も同じ step。名前は外側が持っていなければ入れ子のもの。
    #[test]
    fn nested_event_joins_the_outer_step() {
        let mut doc = SongDoc::new(Song::default());
        in_event(&mut doc, "テンポ変更", |doc| {
            doc.edit(doc.event_scope(), |s| s.bpm = 140.0);
            in_event(doc, "音量変更", |doc| doc.edit(doc.event_scope(), |s| s.bpm = 141.0));
            doc.edit(doc.event_scope(), |s| s.bpm = 142.0);
        });
        assert_eq!(doc.history_labels(), vec![BASELINE_LABEL, "テンポ変更"]);

        in_event(&mut doc, GENERIC_UNDO_LABEL, |doc| {
            in_event(doc, "モジュレーション接続", |doc| doc.edit(doc.event_scope(), |s| s.bpm = 150.0));
            doc.edit(doc.event_scope(), |s| s.bpm = 151.0);
        });
        assert_eq!(doc.history_labels(), vec![BASELINE_LABEL, "テンポ変更", "モジュレーション接続"]);
        doc.edit(doc.event_scope(), |s| s.bpm = 152.0);
        assert_eq!(doc.history_current(), 3, "一番外を閉じた後は別 step");
    }

    /// undo/redo は current index とラベル対応を保ちつつ live state を戻す。
    #[test]
    fn undo_redo_preserve_labels_and_state() {
        let mut doc = SongDoc::new(Song::default());
        let base_bpm = doc.song().bpm;
        in_event(&mut doc, "A", |doc| doc.edit(EditScope::Discrete, |s| s.bpm = 140.0));
        in_event(&mut doc, "B", |doc| doc.edit(EditScope::Discrete, |s| s.bpm = 150.0));

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

    /// 履歴リストの `index` 行へ飛ぶ (view の行 click と同じ: その場で行を state の識別子に直す)。
    fn jump_to(doc: &mut SongDoc, index: usize) -> bool {
        doc.history_state_id(index).is_some_and(|id| doc.jump(HistoryJump::ToState(id)))
    }

    /// 行へのジャンプは 1 発で任意 index の state へ遷移する (undo/redo を必要段数
    /// 繰り返したのと同じ結果)。行き先の Song は動かす前に覗ける。
    #[test]
    fn jump_to_reaches_any_index() {
        let mut doc = SongDoc::new(Song::default());
        let base_bpm = doc.song().bpm;
        for (label, bpm) in [("A", 140.0), ("B", 150.0), ("C", 160.0)] {
            in_event(&mut doc, label, |doc| doc.edit(EditScope::Discrete, |s| s.bpm = bpm));
        }
        assert_eq!(doc.history_current(), 3);

        // 一気に baseline へ。
        let baseline = doc.history_state_id(0).expect("baseline");
        assert_eq!(doc.history_target(HistoryJump::ToState(baseline)).map(|s| s.bpm), Some(base_bpm));
        assert!(jump_to(&mut doc, 0));
        assert_eq!(doc.history_current(), 0);
        assert_eq!(doc.song().bpm, base_bpm);

        // 一気に途中 (A の直後) へ。redo 側の行き先も覗ける。
        assert_eq!(doc.history_target(HistoryJump::Redo).map(|s| s.bpm), Some(140.0));
        let latest = doc.history_state_id(3).expect("C");
        assert_eq!(doc.history_target(HistoryJump::ToState(latest)).map(|s| s.bpm), Some(160.0));
        assert!(jump_to(&mut doc, 1));
        assert_eq!(doc.history_current(), 1);
        assert_eq!(doc.song().bpm, 140.0);

        // 一気に最新へ。
        assert!(doc.jump(HistoryJump::ToState(latest)));
        assert_eq!(doc.history_current(), 3);
        assert_eq!(doc.song().bpm, 160.0);

        // current / 範囲外 / 端 は no-op。
        assert!(!jump_to(&mut doc, 3), "current へは no-op");
        assert!(!jump_to(&mut doc, 4), "範囲外は no-op");
        assert!(doc.history_target(HistoryJump::Redo).is_none());
        assert!(!doc.redo());
        assert_eq!(doc.song().bpm, 160.0);
    }

    /// jump 後に新規編集すると redo 分岐は破棄される (linear undo の一貫性)。捨てた state の識別子へは飛べない。
    #[test]
    fn edit_after_jump_truncates_future() {
        let mut doc = SongDoc::new(Song::default());
        for (label, bpm) in [("A", 140.0), ("B", 150.0)] {
            in_event(&mut doc, label, |doc| doc.edit(EditScope::Discrete, |s| s.bpm = bpm));
        }
        let b = doc.history_state_id(2).expect("B");
        jump_to(&mut doc, 1); // A の直後、 B は redo 待ち。
        in_event(&mut doc, "C", |doc| doc.edit(EditScope::Discrete, |s| s.bpm = 170.0));
        // B は捨てられ、 A → C の直線履歴になる。
        assert_eq!(doc.history_labels(), vec![BASELINE_LABEL, "A", "C"]);
        assert_eq!(doc.history_current(), 2);
        assert!(!doc.can_redo());
        assert!(!doc.jump(HistoryJump::ToState(b)), "捨てた state へは飛べない");
    }

    /// plugin state の書き戻しは live と undo / redo の全 Song の同じ device へ届き、履歴も epoch も dirty も動かさない。
    #[test]
    fn plugin_state_write_back_reaches_every_history_song() {
        use common::model::Track;
        let mut doc = SongDoc::new(Song::default());
        let device = doc
            .edit(EditScope::Discrete, |s| {
                let id = s.alloc_device_id();
                let plugin = PluginInstance { id, ..PluginInstance::new("p".into(), common::plugin_format::PluginFormat::Clap) };
                let track_id = s.alloc_track_id();
                s.tracks.push(Track { id: track_id, devices: vec![Device::Plugin(plugin)], ..Track::default() });
                id
            })
            .expect("edit");
        doc.edit(EditScope::Discrete, |s| s.bpm = 140.0);
        doc.edit(EditScope::Discrete, |s| s.bpm = 150.0);
        assert!(doc.undo());
        doc.mark_saved();
        let (epoch, depth) = (doc.edit_epoch(), doc.undo_depth());

        let blob: std::sync::Arc<[u8]> = std::sync::Arc::from(&[9_u8][..]);
        doc.write_back_plugin_state(|song| {
            if let Some(p) = song.plugin_by_id_mut(device) {
                p.state = Some(blob.clone());
            }
        });
        let state = |song: &Song| song.plugin_by_id(device).and_then(|p| p.state.as_deref().map(<[u8]>::to_vec));
        assert_eq!(state(doc.song()), Some(vec![9]));
        assert!(doc.history_songs().filter(|s| s.plugin_by_id(device).is_some()).all(|s| state(s) == Some(vec![9])));
        assert_eq!(doc.history_songs().filter(|s| s.plugin_by_id(device).is_some()).count(), 2, "undo 1 本 + redo 1 本");
        assert_eq!((doc.edit_epoch(), doc.undo_depth(), doc.is_dirty()), (epoch, depth, false));
    }

    /// 構造の世代: 値だけの編集では進まず、id 構造 (トラック / device / lane …) が変わった編集と、
    /// それを戻す undo / redo で進む。New / Open (replace_song) は常に進む。構造を壊した編集は同じ
    /// step の中で回復される (呼び出し側の宣言は要らない)。
    #[test]
    fn structure_epoch_tracks_id_structure_changes() {
        use common::model::{AutomationLane, AutomationTarget, Track, TrackBuiltinParam};
        let mut doc = SongDoc::new(Song::default());
        let e0 = doc.structure_epoch();
        doc.edit(EditScope::Discrete, |s| s.bpm = 133.0);
        assert_eq!(doc.structure_epoch(), e0, "値だけの編集では進まない");

        doc.edit(EditScope::Discrete, |s| {
            let id = s.alloc_track_id();
            s.tracks.push(Track { id, ..Track::default() });
        });
        let e1 = doc.structure_epoch();
        assert!(e1 > e0, "トラックを足すと進む");
        let t = doc.song().tracks.last().expect("track");
        assert_eq!(t.devices.len(), 2, "組み込みは同じ step で補われる");
        let tid = t.id;

        doc.edit(EditScope::Discrete, |s| {
            let lane = AutomationLane::new(AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume), 1.0);
            s.push_lane(tid, lane);
        });
        let e2 = doc.structure_epoch();
        assert!(e2 > e1, "lane を足すと進む");
        doc.edit(EditScope::Discrete, |s| s.tracks.last_mut().expect("track").automation_lanes[0].default_value = 0.5);
        assert_eq!(doc.structure_epoch(), e2, "lane の値では進まない");

        assert!(doc.undo(), "lane の値を戻す");
        assert_eq!(doc.structure_epoch(), e2, "値の undo では進まない");
        assert!(doc.undo(), "lane の追加を戻す");
        let e3 = doc.structure_epoch();
        assert!(e3 > e2, "構造の undo で進む");
        assert!(doc.redo());
        assert!(doc.structure_epoch() > e3, "構造の redo で進む");

        let e4 = doc.structure_epoch();
        doc.replace_song(doc.song().clone());
        assert!(doc.structure_epoch() > e4, "replace_song は同じ構造でも進む");
    }

    /// device node の引き (索引) は、id 構造を変える編集 / undo / redo / replace_song の後も、木を走査する
    /// `Song` の引きと同じ答えを返す (索引が構造の世代に遅れない)。値だけの編集 (改名) では索引を作り直さず、
    /// たどった先の新しい名前が見える。
    #[test]
    fn node_lookups_agree_with_the_tree_walk_across_structure_changes() {
        use common::model::{AutomationTarget, ChainRef, MASTER_TRACK_ID, Track, TrackBuiltinParam};

        fn collect_ids(devices: &[Device], device_ids: &mut Vec<u64>, chain_ids: &mut Vec<u64>) {
            for d in devices {
                device_ids.push(d.id());
                if let Device::Parallel(p) = d {
                    for c in &p.chains {
                        chain_ids.push(c.id);
                        collect_ids(&c.devices, device_ids, chain_ids);
                    }
                }
            }
        }
        fn assert_agrees(doc: &SongDoc, step: &str) {
            let song = doc.song();
            let (mut device_ids, mut chain_ids) = (Vec::new(), Vec::new());
            for devices in song.tracks.iter().map(|t| &t.devices).chain([&song.master_fx_chain]) {
                collect_ids(devices, &mut device_ids, &mut chain_ids);
            }
            assert!(chain_ids.len() >= 3, "{step}: 入れ子の chain を含む曲で確かめる");
            // 束縛先の持ち主 (`bound_owner_track`) も同じ答え。種類違いの住所 (chain の住所に device id) を含む。
            let targets = |id: u64| {
                [
                    AutomationTarget::PluginParam { device_id: id, param_id: 0, legacy_device_index: None },
                    AutomationTarget::TrackBuiltin(TrackBuiltinParam::ParallelOutGain { parallel_id: id }),
                    AutomationTarget::TrackBuiltin(TrackBuiltinParam::ChainGain { chain_id: id }),
                ]
            };
            let fixed = [AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume), AutomationTarget::SongTempo];
            for id in device_ids.into_iter().chain([u64::MAX]) {
                let walk = song.device_by_id(id).map(Device::id).zip(song.device_owner_track(id));
                assert_eq!(doc.device_node(id).map(|(d, owner)| (d.id(), owner)), walk, "{step}: device {id}");
                for t in targets(id) {
                    assert_eq!(doc.bound_owner_track(&t), song.bound_owner_track(&t), "{step}: {t:?}");
                }
            }
            for id in chain_ids.into_iter().chain([u64::MAX]) {
                let walk = song
                    .chain_by_id(id)
                    .map(|(p, c)| (p.id, c.id))
                    .zip(song.chain_owner_track(ChainRef::Chain(id)));
                assert_eq!(doc.chain_node(id).map(|(p, c, owner)| ((p.id, c.id), owner)), walk, "{step}: chain {id}");
                for t in targets(id) {
                    assert_eq!(doc.bound_owner_track(&t), song.bound_owner_track(&t), "{step}: {t:?}");
                }
            }
            for t in &fixed {
                assert_eq!(doc.bound_owner_track(t), song.bound_owner_track(t), "{step}: {t:?}");
            }
        }

        let mut doc = SongDoc::new(Song::default());
        let (mut t1, mut t2) = (0, 0);
        doc.edit(EditScope::Discrete, |s| {
            t1 = s.alloc_track_id();
            s.tracks.push(Track { id: t1, ..Track::default() });
            t2 = s.alloc_track_id();
            s.tracks.push(Track { id: t2, ..Track::default() });
        });
        // track 1 に Parallel (chain 2 本、2 本目の中にもう 1 段の Parallel)、master にも Parallel。
        let (mut outer, mut deep_chain) = (0, 0);
        doc.edit(EditScope::Discrete, |s| {
            let mut inner = Parallel::new();
            inner.id = s.alloc_device_id();
            inner.chains[0].id = s.alloc_device_id();
            deep_chain = inner.chains[0].id;
            let mut second = ParallelChain::new("Chain 2");
            second.id = s.alloc_device_id();
            second.devices.push(Device::Parallel(inner));
            let mut p = Parallel::new();
            p.id = s.alloc_device_id();
            p.chains[0].id = s.alloc_device_id();
            p.chains.push(second);
            outer = p.id;
            s.insert_device(ChainRef::Track(t1), 0, Device::Parallel(p));
            let mut m = Parallel::new();
            m.id = s.alloc_device_id();
            m.chains[0].id = s.alloc_device_id();
            s.insert_device(ChainRef::Track(MASTER_TRACK_ID), 0, Device::Parallel(m));
        });
        assert_agrees(&doc, "Parallel を挿した");
        doc.edit(EditScope::Discrete, |s| {
            let p = s.remove_device(outer).expect("outer");
            s.insert_device(ChainRef::Track(t2), 1, p);
        });
        assert_agrees(&doc, "トラックを跨いで運んだ");
        doc.edit(EditScope::Discrete, |s| s.tracks.reverse());
        assert_agrees(&doc, "トラックを並べ替えた");

        let epoch = doc.structure_epoch();
        doc.edit(EditScope::Discrete, |s| s.chain_by_id_mut(deep_chain).expect("chain").name = "Deep".into());
        assert_eq!(doc.structure_epoch(), epoch, "改名は id 構造を変えない");
        assert_eq!(doc.chain_by_id(deep_chain).map(|(_, c)| c.name.as_str()), Some("Deep"), "たどった先の新しい名前");

        assert!(doc.undo() && doc.undo(), "改名と並べ替えを戻す");
        assert_agrees(&doc, "undo");
        assert!(doc.redo(), "並べ替えをやり直す");
        assert_agrees(&doc, "redo");
        let mut other = doc.song().clone();
        other.tracks.reverse();
        doc.replace_song(other);
        assert_agrees(&doc, "replace_song");
    }

    /// 不変条件 1: undo で採番の状態が巻き戻っても、undo した物の id を別の新しい物に振らない。
    /// redo で戻る物は元の id のまま。undo / redo しても dirty の判定 (保存時点に戻れば clean) は変わらない。
    #[test]
    fn undo_does_not_let_ids_be_reused() {
        use common::model::{ClipContent, MidiContent, Track};
        let mut doc = SongDoc::new(Song::default());
        let content = doc
            .edit(EditScope::Discrete, |s| s.alloc_content(ClipContent::Midi(MidiContent::default()), String::new()))
            .expect("edit");
        doc.mark_saved();
        // (track id, device id, 採番した note id)
        let alloc = |doc: &mut SongDoc| {
            doc.edit(EditScope::Discrete, |s| {
                let tid = s.alloc_track_id();
                s.tracks.push(Track { id: tid, ..Track::default() });
                let device = s.alloc_device_id();
                let note = match s.clip_contents.get_mut(&content) {
                    Some(ClipContent::Midi(m)) => m.alloc_note_id(),
                    _ => panic!("content"),
                };
                (tid, device, note)
            })
            .expect("edit")
        };
        let first = alloc(&mut doc);
        let first_track = doc.song().tracks.last().cloned().expect("track");
        assert!(doc.undo());
        assert!(!doc.is_dirty(), "保存時点に戻れば clean (持ち越しは dirty にしない)");
        assert!(doc.redo());
        assert_eq!(doc.song().tracks.last(), Some(&first_track), "redo で元の物が同じ id で戻る");
        assert!(doc.undo());
        let second = alloc(&mut doc);
        assert!(second.0 != first.0 && second.1 != first.1 && second.2 != first.2, "{first:?} → {second:?}");
        let second_builtins: Vec<u64> = doc.song().tracks.last().expect("track").devices.iter().map(|d| d.id()).collect();
        let first_builtins: Vec<u64> = first_track.devices.iter().map(|d| d.id()).collect();
        assert!(second_builtins.iter().all(|id| !first_builtins.contains(id)), "{first_builtins:?} / {second_builtins:?}");
    }

    /// no-op 編集 (edit_checked が false) は履歴に step を足さない。
    #[test]
    fn noop_edit_adds_no_labeled_step() {
        let mut doc = SongDoc::new(Song::default());
        in_event(&mut doc, "A", |doc| doc.edit(EditScope::Discrete, |s| s.bpm = 140.0));
        let before = doc.history_labels();
        in_event(&mut doc, "no-op", |doc| doc.edit_checked(EditScope::Discrete, |_s| false));
        assert_eq!(doc.history_labels(), before, "no-op は履歴を汚さない");
        assert_eq!(doc.history_current(), 1);
    }
}
