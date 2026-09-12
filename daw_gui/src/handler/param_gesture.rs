//! handler::param_gesture — パラメータ操作 (フェーダー / ノブ drag) の gesture 立ち上がり /
//! 立ち下がり (`ParamGestureBegin` / `ParamGestureEnd`)。
//!
//! `app.rs` の `handle_event` から切り出した `impl AppData` メソッド群 (挙動は元と同一、
//! 不変条件 9 のサイズ budget)。
use crate::state::*;
use crate::app_types::*;

impl AppData {
    /// `ParamGestureBegin`: built-in トラックコントロール (Volume / Pan / SendGain) の
    /// drag は gesture 先頭で 1 回だけ Song snapshot を取り、「1 drag = 1 undo step」 にする
    /// (`BeginInspectorScrub` と同 idiom)。 per-frame に発火する `SetTrackVolume` /
    /// `SetTrackPan` / `SetSendGain` 自体は非 undoable のまま (連続発火で履歴が溢れるため)。
    /// これが無いとフェーダー操作が undo スタックに積まれず、 Undo が直前のクリップ移動
    /// 等まで巻き戻してしまう。 `ParamGestureBegin` は gesture 立ち上がりで 1 度だけ発火する
    /// (`push_param_gesture_edges` の edge 検知) ので二重にならない。 `PluginParam` は値が
    /// Song snapshot に入らない (plugin 内部状態) ので除外、 `SongTempo` / `TimeSig` は
    /// transport 側の commit ベース undo に委ねる。
    pub(crate) fn begin_param_gesture(
        &mut self,
        track_id: u32,
        target: common::model::AutomationTarget,
        display_name: String,
    ) {
        // fader/knob drag 全体を 1 undo step に bracket する (最初の song 編集が snapshot を
        // 積む。 PluginParam のように song を変えない gesture では undo step は増えない)。
        self.cur.song_doc.begin_gesture();
        self.cur.recording.active_param_gestures.insert((track_id, target.clone()));
        // Phase 4 Step C: Latch / Write mode で 再生中の gesture begin は
        // latched_param_gestures にも入れる。 stop まで「触れた事実」 を保持し、 release 後も
        // curve 上書きを継続する。 Touch mode では latched は使わない (= release で recording
        // 完全停止)。
        if matches!(
            self.cur.recording.recording_mode,
            common::model::RecordingMode::Latch | common::model::RecordingMode::Write
        ) && self.cur.transport.is_playing
        {
            self.cur.recording.latched_param_gestures.insert((track_id, target.clone()));
        }
        // `TouchParam` を発火し続けるより、 gesture begin で `last_touched_param` を更新する
        // idiom に統一する (= drag 開始の瞬間が touch、 drag 中の値変化は touch を再発火しない)。
        self.cur.peph.last_touched_param = Some(TouchedParam {
            track_id,
            target,
            display_name,
            touched_at: std::time::Instant::now(),
        });
        self.sync_recording_lanes_with_audio();
    }

    /// `ParamGestureEnd`: gesture を閉じて undo の bracket を解く。
    ///
    /// BPM scrub は毎 tick edit_song で epoch を bump するので、 plugin host 側の BPM 消費者
    /// (VOICEVOX metadata / ARA placement / lipsync) は runner の frame flush
    /// (flush_song_sync) が構造的に追従する (旧 pending_host_sync 予約は epoch 一本化で不要)。
    /// Phase 4 Step C: Touch mode の場合、 release で recording 完全停止 →
    /// recording_last_beat からも該当 entry を消す (= 次の gesture begin で改めて throttle 開始)。
    /// Latch / Write は stop まで latched 継続なので last_beat も保持する (= 連続 record)。
    pub(crate) fn end_param_gesture(&mut self, track_id: u32, target: common::model::AutomationTarget) {
        self.cur.recording.active_param_gestures.remove(&(track_id, target.clone()));
        if self.cur.recording.active_param_gestures.is_empty() {
            self.cur.song_doc.end_gesture();
        }
        if self.cur.recording.recording_mode == common::model::RecordingMode::Touch {
            self.cur.recording.recording_last_beat.remove(&(track_id, target));
        }
        self.sync_recording_lanes_with_audio();
    }
}
