//! handler::track_enable — r.md #131 トラックの無効化 / 有効化 (`docs/plan_rmd_131_track_disable.md`)。
//!
//! 無効は Song が持つトラックの状態 (undo 可・保存・dirty)。実行系から外す判定は
//! `Song::track_effectively_enabled` 1 本で、ここは「編集の口」と「host との同期」だけを持つ:
//! host に居るべき device の導出は `compute_slot_reconcile_actions` (= `Song::live_plugins`) 1 本で、
//! ここで device を列挙しない。
//!
//! 有効に戻したトラックが鳴り始めるのは **居るべき plugin の読み込みが確定した瞬間** で、それまで engine はそのトラックを
//! グラフに入れない (`Song::executable_mask`、読み込み中の device は `AppData::sync_loading_devices` が engine へ届ける)。
//! 再生は止めない ([`LoadPlayback::KeepPlaying`])。

use std::collections::HashSet;

use crate::app_types::*;
use crate::state::*;

/// 編集の **前** の「実行系に居たもの」([`AppData::live_before`])。編集の後の Song と比べて host を追従させ
/// ([`AppData::follow_live_devices`])、載せる読み込みが有効に戻したトラックのもの (再生を止めない) かを決める。
pub(crate) struct LiveBefore {
    /// host に居るべきだった device (`Song::live_plugins`)。
    devices: HashSet<u64>,
    /// 実効的に無効だったトラック ([`AppData::disabled_track_ids`])。
    disabled_tracks: HashSet<u32>,
}

impl AppData {
    /// `AppEvent::SetTracksEnabled` の dispatcher。master / 存在しない id は落とし、全部が既にその状態なら
    /// 何もしない (死んだ undo step を作らない)。
    ///
    /// **無効化は plugin を host から降ろす** ので、削除系と同じく `RequestAllStates` の往復で最新の state を
    /// Song に書き戻してから編集する (`PendingStateRequest::Deferred`) — 降ろした device は Song の state から
    /// 有効化 / undo で復元される。有効化は降ろさないので即時。
    pub(crate) fn set_tracks_enabled(&mut self, track_ids: Vec<u32>, enabled: bool) {
        let ids = self.live_track_ids(&track_ids);
        if ids.is_empty() {
            if track_ids.contains(&common::model::MASTER_TRACK_ID) {
                self.ui_ephemeral.status_message = "マスタートラックは無効化できません".to_string();
            }
            return;
        }
        let song = self.cur.song_doc.song();
        if ids.iter().all(|&id| song.track_by_id(id).is_some_and(|t| t.enabled == enabled)) {
            return;
        }
        if enabled || !self.song_has_plugin() {
            self.set_tracks_enabled_inner(&ids, enabled);
            return;
        }
        self.enqueue_deferred_edit(DeferredEdit::DisableTracks { track_ids: ids });
    }

    /// 無効化 / 有効化の本体 (deferred の完了か即時)。`Song::set_tracks_enabled` が録音待機とランチャーの
    /// 鳴っているセルも同じ undo step で降ろす。engine への構造 (LoadSong) と plugin host の追従は
    /// [`Self::follow_live_devices`] が音の決まる順に流す (再生は止めない — 有効に戻したトラックは読み込みが
    /// 確定した時点から鳴る)。headless でも同じ順で流れる。
    pub(crate) fn set_tracks_enabled_inner(&mut self, track_ids: &[u32], enabled: bool) {
        let before = self.live_before();
        let ids = track_ids.to_vec();
        if !self.edit_song_checked(move |song| song.set_tracks_enabled(&ids, enabled)) {
            return;
        }
        self.follow_live_devices(&before);
        if !enabled {
            self.silence_monitor_notes_on_disabled_tracks();
        }
    }

    /// 編集の前の [`LiveBefore`] (host に居るべき device と、実効的に無効なトラック)。[`Self::follow_live_devices`] に渡す。
    /// (device は選択の掃除に使う `live_device_ids` = 「Song に実在する device」とは別物。)
    pub(crate) fn live_before(&self) -> LiveBefore {
        LiveBefore {
            devices: self.cur.song_doc.song().live_plugins().map(|p| p.id).collect(),
            disabled_tracks: self.disabled_track_ids(),
        }
    }

    /// いま実効的に無効なトラックの id。編集 / undo・redo の前に取り、後で読み込む device が「有効に戻したトラック」の
    /// ものか (= 再生を止めない) を決める。
    pub(crate) fn disabled_track_ids(&self) -> HashSet<u32> {
        let song = self.cur.song_doc.song();
        song.tracks.iter().zip(song.effectively_enabled_mask()).filter(|&(_, on)| !on).map(|(t, _)| t.id).collect()
    }

    /// 編集で **host に居るべきかが変わった device だけ** を host に追従させる (`before` = 編集前の
    /// [`Self::live_before`])。無効化 / 無効な group への移動 / 無効トラックへの device の移動で居るべきで
    /// なくなったものは降ろし (窓も閉じる)、有効化 / group 解除 / 無効トラックからの移動で居るべきになったものは
    /// Song の state 付きで載せる。engine への構造の同期もここで済む (順序は [`Self::apply_slot_reconcile_actions`]) ので、
    /// 呼び出し側は編集の後に `flush_song_sync` を先に呼ばない (先に構造が届くと、有効に戻したトラックが読み込み中を
    /// 知らないまま実行に入って素通しの音が鳴る)。
    ///
    /// 全体の reconcile を呼ばないのは、無関係な「読み込み失敗」の device を勝手に再読込しないため
    /// (再試行はユーザーの意思 — `failed_plugin_loads` の doc)。
    pub(crate) fn follow_live_devices(&mut self, before: &LiveBefore) {
        let song = self.cur.song_doc.song();
        let after: HashSet<u64> = song.live_plugins().map(|p| p.id).collect();
        // Song から消えた device は削除の経路 (`plan_track_removal_ipc` / `remove_devices_inner`) が降ろすので数えない。
        let changed: HashSet<u64> = before
            .devices
            .symmetric_difference(&after)
            .copied()
            .filter(|&id| song.plugin_by_id(id).is_some())
            .collect();
        let actions = if changed.is_empty() {
            Vec::new()
        } else {
            let all = crate::app_types::compute_slot_reconcile_actions(
                song,
                &self.cur.pipc.loaded_devices,
                &self.cur.pipc.pending_plugin_loads,
            );
            all.into_iter().filter(|a| changed.contains(&a.device_id())).collect()
        };
        self.apply_slot_reconcile_actions(actions, &before.disabled_tracks);
        // 降ろした device の「未ロード」表示は持ち越さない (有効に戻したときは読み込みをやり直す)。
        self.cur.pipc.failed_plugin_loads.retain(|id, _| after.contains(id) || !changed.contains(id));
    }

    /// 無効になったトラックで鳴らしていた入力モニター音を止める (arm を外したときと同じ理由 — 待機が
    /// 外れたので note-off がもう届かない)。無効化と、無効な group の中への移動が呼ぶ。
    pub(crate) fn silence_monitor_notes_on_disabled_tracks(&mut self) {
        let song = self.cur.song_doc.song();
        // 台帳は `(track, 弾いた pitch) → 送った鍵盤` (r.md #130)。止めるのは送った鍵盤。
        let held: Vec<((u32, u8), u8)> = self
            .cur
            .recording
            .monitor_notes
            .iter()
            .map(|(&held, &key)| (held, key))
            .filter(|&((track_id, _), _)| !song.track_effectively_enabled(track_id))
            .collect();
        for ((track_id, pitch), key) in held {
            self.cur.recording.monitor_notes.remove(&(track_id, pitch));
            self.send_preview_off(track_id, key);
        }
    }

    /// device が **host に居るべきか** (実効的に有効なトラックか master の device)。`Song::live_plugins` と同じ
    /// 定義で、編集の直後に device を実体化する口 (コピー / 貼り付け / 再読込 / 窓を開く) が無効トラックへ
    /// 載せないために使う。
    pub(crate) fn is_live_device(&self, device_id: u64) -> bool {
        self.cur.song_doc.song().live_plugins().any(|p| p.id == device_id)
    }
}
