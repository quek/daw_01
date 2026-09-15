//! handler::history — 履歴ジャンプ (undo / redo / 履歴リストの行) の入口。
//!
//! **plugin を host から降ろすジャンプは、降ろす前の plugin state を取り寄せてから動かす** (削除 / 無効化 / 移動の
//! `PendingStateRequest::Deferred` と同じ往復)。値は必ず残り、代わりにそのジャンプだけ取り寄せの間 (普通は数十 ms、
//! 重い音源だと数百 ms) 反映が遅れる。降ろす device が無いジャンプは同期で即時。
//!
//! 取り寄せ待ちの間に来たジャンプと、往復待ちの編集の後に来たジャンプは、発注の順に待ち行列 (`pending_state_queue`)
//! で 1 つずつ処理する (取り寄せ → ジャンプ → 次)。待ちの間に来た待たない編集は、往復待ちの編集のときと同じく
//! その場で live に入る — 待っているジャンプは **自分の取り寄せを始めた時点** の行き先 (state の識別子) へ動くので、
//! その取り寄せの間に入った編集は redo 側に回る (すぐ往復を始めるジャンプは発注の時点、前の要求の後ろに並んだ
//! ジャンプは前が済んで往復を始める時点 — `pin_front_history_jump`)。往復がタイムアウト / host 切断で終わらなければ、
//! 往復待ちの編集と同じくジャンプは適用しない (`abort_state_roundtrip`)。書き出し / 解析 / bounce / 焼き込みの間は
//! 編集と同じく拒否する (`SongDoc::jump`)。
//!
//! plugin state は履歴に属さない (host が持つ最新が正で、undo でツマミは戻らない)。往復の応答は live と undo / redo の
//! 全 Song へ書き戻す ([`PluginStateWriteBack`] / `SongDoc::write_back_plugin_state`) ので、次に同じ device を載せ直す
//! どの経路 (redo / undo / 履歴ジャンプ / 再有効化) も最新の値で載る。

use std::collections::HashMap;
use std::sync::Arc;

use common::model::Song;
use common::protocol::SlotState;

use crate::app_types::*;
use crate::state::*;

impl AppData {
    pub(crate) fn undo(&mut self) {
        self.request_history_jump(HistoryJump::Undo);
    }

    pub(crate) fn redo(&mut self) {
        self.request_history_jump(HistoryJump::Redo);
    }

    /// r.md #29: 履歴リストの行 click → `index` 番目の state へ一気に遡る / 進む。行はクリックした瞬間に state の
    /// 識別子へ直す (往復待ちの間に積まれた編集で位置が詰まっても、クリックした state を指し続ける)。
    pub(crate) fn jump_history_to(&mut self, index: usize) {
        if let Some(state_id) = self.cur.song_doc.history_state_id(index) {
            self.request_history_jump(HistoryJump::ToState(state_id));
        }
    }

    /// `jump` を今動かすか、plugin state の往復の後に回すか:
    /// - 待ち行列に Song を変える要求 (往復待ちの編集 / 履歴ジャンプ) が居る — 発注の順に処理する (削除の往復を
    ///   待たずに undo すると、削除ではなくその前の操作が戻る)。行き先はそれが済むまで決まらないので相対のまま並べ、
    ///   前が済んで自分の往復を始める時点で固定する ([`Self::pin_front_history_jump`])。
    /// - このジャンプで host から降りる **読み込み済みの** device がある (行き先の Song で [`compute_slot_removals`]、
    ///   reconcile と同じ導出。読み込み中の device は失う state が無いので数えない) — 行き先は今決まっているので state の識別子で固定して並べる (往復待ちの編集が id で対象を指すのと同じく、
    ///   待つ間に入った編集で「1 段前」がずれない)。
    /// - どちらでもなければ即時。
    ///
    /// 書き出し / 解析 / bounce / 焼き込みの間 (Song の凍結中) は往復も始めず、編集と同じく拒否する (`SongDoc::jump`)。
    fn request_history_jump(&mut self, jump: HistoryJump) {
        self.sync_export_lock();
        if self.cur.song_doc.export_locked() {
            // 動かさずに拒否の status を予約するのは `SongDoc::jump` (編集の拒否と同じ 1 箇所)。
            self.execute_history_jump(jump);
            return;
        }
        let pipc = &self.cur.pipc;
        let ordered = pipc
            .pending_state_queue
            .iter()
            .any(|r| matches!(r, PendingStateRequest::Deferred { .. } | PendingStateRequest::HistoryJump(_)));
        if ordered {
            self.enqueue_state_request(PendingStateRequest::HistoryJump(jump));
            return;
        }
        let doc = &self.cur.song_doc;
        let Some(target) = doc.history_target(jump) else {
            return;
        };
        // 取り寄せが要るのは **読み込みが確定した** device を降ろすときだけ。読み込み中の device は host に
        // まだ居ない (state は載せるときに送ったもののまま) ので、降ろしても失う値が無い。
        let removes_loaded = compute_slot_removals(target, &pipc.loaded_devices, &pipc.pending_plugin_loads)
            .iter()
            .any(|id| pipc.loaded_devices.contains_key(id));
        if !removes_loaded {
            self.execute_history_jump(jump);
        } else if let Some(pinned) = doc.pin_jump(jump) {
            self.enqueue_state_request(PendingStateRequest::HistoryJump(pinned));
        }
    }

    /// 履歴を動かし、session 状態と host を追従させる (即時か、往復の完了 `on_all_states_from_child`)。行き先が
    /// もう無い (往復待ちの間の編集で redo 側が捨てられた) なら何もしない。取り寄せを待つ間に bounce / 焼き込みが
    /// 始まっていたら動かさない (編集と同じく、ロックは動かす直前に transport から同期する)。
    pub(crate) fn execute_history_jump(&mut self, jump: HistoryJump) {
        self.sync_export_lock();
        // audio editor の対象は song を差し替えると消えている可能性があるので、**前** に key を退避して
        // `after_undo_redo` で引き直す。無効だったトラックも差し替える前に取る (有効に戻ったトラックの読み込みは
        // 再生を止めない、r.md #131)。
        let key = self.audio_editor_target_key();
        let disabled_before = self.disabled_track_ids();
        if self.cur.song_doc.jump(jump) {
            self.after_undo_redo(key, &disabled_before);
        }
    }

    /// 待ち行列の先頭が履歴ジャンプなら、その往復を始める **この瞬間** に行き先を state の識別子で固定する
    /// (`dispatch_front_state_request` が送る直前に呼ぶ)。前に並んでいた要求は済んで行き先が決まったので、ここから
    /// 応答までの間に入った編集で「1 段前」がずれない — すぐ往復を始めるジャンプを発注の時点で固定するのと同じ規則。
    /// 行き先がもう無い (端 / 待つ間に redo 側が捨てられた) なら `false` — 往復を始めずに取り除く。
    pub(crate) fn pin_front_history_jump(&mut self) -> bool {
        let doc = &self.cur.song_doc;
        let Some(PendingStateRequest::HistoryJump(jump)) = self.cur.pipc.pending_state_queue.front_mut() else {
            return true;
        };
        let Some(pinned) = doc.pin_jump(*jump) else {
            return false;
        };
        *jump = pinned;
        true
    }
}

/// `AllPluginStates` の応答を Song へ書き戻せる形にしたもの。blob は 1 度だけ `Arc` にして、live / 履歴 / 保存の
/// snapshot の全 Song で共有する (履歴の Song ごとに複製しない)。
///
/// v29: SlotState は安定 `device_id` keyed。track / master / Parallel のどこに居ても id 一致で書き戻すので、
/// deferred の間に並びが変わっていても壊れない (`docs/plan_arch_refactor.md` §1)。
pub(crate) struct PluginStateWriteBack {
    states: HashMap<u64, DeviceState>,
}

/// 1 device ぶんの書き戻し: `PluginInstance::state` と、plug-in が出したときだけの ARA archive (とその目次)。
struct DeviceState {
    state: Option<Arc<[u8]>>,
    ara_archive: Option<(Arc<[u8]>, Vec<String>)>,
}

impl PluginStateWriteBack {
    pub(crate) fn new(states: &[SlotState]) -> Self {
        let states = states
            .iter()
            .filter(|s| {
                // Phase 6 review (silent corruption fix): plugin_host が `state_save()` で `Err` を返したエントリは
                // `error` 付きで来る (`data` は None)。書くと **過去に保存した state が消える** (旧バグ: save 失敗 →
                // 次 save で空 state 確定) ので、飛ばして既存の state を保つ。
                if s.error.is_some() {
                    tracing::warn!(
                        device_id = s.device_id,
                        error = s.error.as_deref(),
                        "plugin state write-back: state save errored, preserving previous state",
                    );
                }
                s.error.is_none()
            })
            .map(|s| {
                let state = s.data.as_deref().map(Arc::from);
                let ara_archive = s.ara_archive.as_ref().map(|a| (Arc::from(a.bytes.as_slice()), a.ids.clone()));
                (s.device_id, DeviceState { state, ara_archive })
            })
            .collect();
        Self { states }
    }

    /// `song` の同じ id の plugin へ書く。
    pub(crate) fn apply(&self, song: &mut Song) {
        if self.states.is_empty() {
            return;
        }
        song.for_each_plugin_mut(&mut |p| {
            let Some(written) = self.states.get(&p.id) else {
                return;
            };
            p.state = written.state.clone();
            // (r.md #5 ARA2) Only overwrite the ARA archive when the plug-in actually produced one; a non-ARA
            // device or a not-yet-bound session reports None, and we must not wipe a previously-saved archive.
            // A fresh archive is written with the current persistent ids; its table of contents replaces the old
            // one (`set_ara_archive`).
            if let Some((archive, ids)) = &written.ara_archive {
                p.set_ara_archive(archive.clone(), ids.clone());
            }
        });
    }

    /// 書き戻す先が live の `song` に居なかった device を知らせる (応答が host の想定外の device を含んでいた)。
    pub(crate) fn warn_missing(&self, song: &Song) {
        for &device_id in self.states.keys().filter(|&&id| song.plugin_by_id(id).is_none()) {
            tracing::warn!(device_id, "plugin state write-back: device id not found");
        }
    }
}
