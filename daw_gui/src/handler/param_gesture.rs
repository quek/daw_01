//! handler::param_gesture — パラメータ操作 (フェーダー / ノブ drag) の gesture 立ち上がり /
//! 立ち下がり (`ParamGestureBegin` / `ParamGestureEnd`)。
//!
//! r.md #129 (§7.6): 所有者は **面つき** (`RecordingState::active_param_gestures` の値 =
//! gesture を開いた [`ParamSurface`])。閉じられるのは同じ面だけ。
//!
//! undo の bracket ([`GestureOwner::ParamGestures`]) は **`active_param_gestures` が空でない間だけ開いている**。
//! 集合を書き換えた口 (ここの Begin / End、preview の PiP drag の seed、子プロセス切断の掃除) は必ず直後に
//! [`AppData::sync_param_gesture_bracket`] を通す。
use crate::state::*;
use crate::app_types::*;

impl AppData {
    /// `ParamGestureBegin`: built-in トラックコントロール (Volume / Pan / SendGain) や内蔵 device の
    /// drag は gesture 先頭で 1 回だけ Song snapshot を取り、「1 drag = 1 undo step」 にする
    /// (`BeginInspectorScrub` と同 idiom)。 `PluginParam` は値が Song snapshot に入らない
    /// (plugin 内部状態) ので undo step は増えない。
    ///
    /// **その key に所有者が既にいれば何もしない** (別の面が握っている gesture を奪わない)。
    pub(crate) fn begin_param_gesture(
        &mut self,
        surface: ParamSurface,
        track_id: u32,
        target: common::model::AutomationTarget,
    ) {
        let key = (track_id, target.clone());
        if self.cur.recording.active_param_gestures.contains_key(&key) {
            return;
        }
        // fader/knob drag 全体を 1 undo step に bracket する (最初の song 編集が snapshot を
        // 積む。 PluginParam のように song を変えない gesture では undo step は増えない)。
        self.cur.recording.active_param_gestures.insert(key.clone(), surface);
        self.sync_param_gesture_bracket();
        // 同じフレームの sweep で閉じないよう在席印を立てる (`scrub_gesture::open` と同じ)。
        self.cur.peph.param_gesture_seen.insert(key.clone());
        // Phase 4 Step C: Latch / Write mode で 再生中の gesture begin は
        // latched_param_gestures にも入れる。 stop まで「触れた事実」 を保持し、 release 後も
        // curve 上書きを継続する。 Touch mode では latched は使わない (= release で recording
        // 完全停止)。
        if matches!(
            self.cur.recording.recording_mode,
            common::model::RecordingMode::Latch | common::model::RecordingMode::Write
        ) && self.cur.transport.is_playing
        {
            self.cur.recording.latched_param_gestures.insert(key);
        }
        // drag 開始の瞬間が touch (drag 中の値変化は touch を再発火しない)。名前は
        // `automation_target_label` (song を引いて device / chain の名前を補う) が SSoT。
        self.cur.peph.last_touched_param = Some(TouchedParam {
            track_id,
            display_name: self.automation_target_label(&target),
            target,
            touched_at: std::time::Instant::now(),
        });
        self.sync_recording_lanes_with_audio();
    }

    /// `ParamGestureEnd`: gesture を閉じて undo の bracket を解く。**所有者が `surface` のときだけ**。
    ///
    /// Phase 4 Step C: Touch mode の場合、 release で recording 完全停止 →
    /// recording_last_beat からも該当 entry を消す (= 次の gesture begin で改めて throttle 開始)。
    /// Latch / Write は stop まで latched 継続なので last_beat も保持する (= 連続 record)。
    pub(crate) fn end_param_gesture(
        &mut self,
        surface: ParamSurface,
        track_id: u32,
        target: common::model::AutomationTarget,
    ) {
        let key = (track_id, target);
        if self.cur.recording.active_param_gestures.get(&key) != Some(&surface) {
            return;
        }
        self.cur.recording.active_param_gestures.remove(&key);
        self.sync_param_gesture_bracket();
        if self.cur.recording.recording_mode == common::model::RecordingMode::Touch {
            self.cur.recording.recording_last_beat.remove(&key);
        }
        self.sync_recording_lanes_with_audio();
    }

    /// undo の bracket ([`GestureOwner::ParamGestures`]) を `active_param_gestures` に合わせる: 空でなければ開き、
    /// 空なら閉じる。進行中の別の bracket (録音 take …) の中で開けばその step に入り、閉じても外側は閉じない
    /// (`SongDoc::begin_gesture`)。
    pub(crate) fn sync_param_gesture_bracket(&mut self) {
        let active = !self.cur.recording.active_param_gestures.is_empty();
        let doc = &mut self.cur.song_doc;
        match (active, doc.gesture_open(GestureOwner::ParamGestures)) {
            (true, false) => doc.begin_gesture(GestureOwner::ParamGestures),
            (false, true) => doc.end_gesture(GestureOwner::ParamGestures),
            _ => {}
        }
    }
}
