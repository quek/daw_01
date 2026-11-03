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
use crate::event::AppEvent;
use common::plugin_format::PluginFormat;

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

    /// プラグイン自身の窓で param を掴んだ (本物の `PluginParamTouched` か、補った gesture)。
    pub(crate) fn plugin_window_gesture_begin(&mut self, device_id: u64, param_id: u32) {
        let Some((track, _index)) = find_device_by_id(self.cur.song_doc.song(), device_id) else {
            return; // 削除済み device の stale event
        };
        let target =
            common::model::AutomationTarget::PluginParam { device_id, param_id, legacy_device_index: None };
        // Phase 4 Step C-3: ParamGestureBegin として同経路で active / latched に反映する (= mixer knob と同 idiom)。
        // 名前は handler が `automation_target_label` で作る (r.md #129 §7.6)。寿命は描画と無関係に plugin 窓の
        // End が閉じる (`ParamSurface::PluginWindow` は sweep されない)。
        self.handle_event(AppEvent::ParamGestureBegin {
            surface: ParamSurface::PluginWindow,
            track_id: track,
            target: target.clone(),
        });
        // r.md #78: modulation source が待受中 (◉) なら、**プラグイン自身の窓の中で触った param** をそのソースの
        // 変調先にする。daw_gui はプラグインの窓の中に overlay を描けないので、arm + ドラッグが届かないのはここだけ。
        // touch 通知がその唯一の到達手段。ユーザー操作なので操作名の付く event で繋ぐ (この IPC event の名前で積まない)。
        self.handle_event(AppEvent::ConnectArmedModSource { track_id: track, target });
    }

    /// プラグイン自身の窓で param を離した。mixer の ParamGestureEnd と同経路に流す (Phase 4 Step C-3)。
    ///
    /// 持ち主は Song でなく**開いた gesture の key** から引く。掴んでいる間に Undo で device が別トラックへ戻った /
    /// 消えたときに Song から引くと key が合わず、閉じないまま undo の bracket が開き続ける
    /// (以後の編集が全部 1 step に固まる)。
    pub(crate) fn plugin_window_gesture_end(&mut self, device_id: u64, param_id: u32) {
        let target =
            common::model::AutomationTarget::PluginParam { device_id, param_id, legacy_device_index: None };
        let tracks: Vec<u32> = self
            .cur
            .recording
            .active_param_gestures
            .iter()
            .filter(|((_, t), surface)| *t == target && **surface == ParamSurface::PluginWindow)
            .map(|((track, _), _)| *track)
            .collect();
        for track_id in tracks {
            self.handle_event(AppEvent::ParamGestureEnd {
                surface: ParamSurface::PluginWindow,
                track_id,
                target: target.clone(),
            });
        }
    }

    /// device かそのエディタ窓が消えた: 開いていた gesture の End はもう来ない。ここで閉じないと undo の bracket が
    /// 開き続ける。
    pub(crate) fn forget_plugin_window_gestures_of(&mut self, device_id: u64) {
        use common::model::AutomationTarget as T;
        self.cur.pipc.inferred_plugin_gestures.retain(|key| key.device_id != device_id);
        let before = self.cur.recording.active_param_gestures.len();
        self.cur.recording.active_param_gestures.retain(|(_, target), surface| {
            *surface != ParamSurface::PluginWindow
                || !matches!(target, T::PluginParam { device_id: d, .. } if *d == device_id)
        });
        if self.cur.recording.active_param_gestures.len() != before {
            self.sync_param_gesture_bracket();
            self.sync_recording_lanes_with_audio();
        }
    }

    /// 曲が丸ごと入れ替わる: プラグイン窓の gesture はすべて前の曲のもの。子プロセス切断の掃除と同じく engine への
    /// 同期はしない (engine の状態は続く `LoadSong` が決める)。
    pub(crate) fn forget_all_plugin_window_gestures(&mut self) {
        self.cur.pipc.inferred_plugin_gestures.clear();
        self.cur.recording.active_param_gestures.retain(|_, surface| *surface != ParamSurface::PluginWindow);
        self.sync_param_gesture_bracket();
    }

    /// プラグインの窓を押している間に値が変わった (確かめるのは呼び出し側の `handler/ipc.rs`)。gesture を出さない
    /// CLAP プラグインなら gesture を補う。これで `A` キー / MIDI Learn / ◉ / 録音が同じ経路で効く。閉じるのは
    /// [`Self::close_inferred_plugin_gestures`]。
    ///
    /// - **CLAP**: gesture は任意で (`events.h`: "This is not mandatory to wrap parameter changes with gesture
    ///   events")、VCV Rack 2.6 は `PARAM_VALUE` だけを出す (実測 2026-11-03)。`PARAM_VALUE` はユーザーの編集に限らない
    ///   (`ext/params.h`: "When the plugin changes a parameter value, it must inform the host") ので、エディタ窓が
    ///   アクティブで押している間だけを編集とみなす。読み取り専用の param (メーター) と、本物の gesture を一度でも
    ///   出した device (掴んだノブに連動して動いた別の param を触ったことにしない) は除く。
    /// - **VST3**: 補わない。文書はホイールについて host が `beginEdit` なしの `performEdit` にタイムアウトで備えるよう
    ///   書いているが ("handle the timeout itself")、同じ文書はプログラム変更で値が少ししか変わらないときにも
    ///   `performEdit` を使うとしていて、host からはホイールと窓の中のプリセットボタンを区別できない。補うと、プリセットを
    ///   替えただけで `A` の的が動き、Latch / Write では変わった全 param にレーンができる。
    pub(crate) fn infer_plugin_window_gesture(&mut self, key: DeviceParamKey) {
        let pipc = &self.cur.pipc;
        let is_clap = self.cur.song_doc.plugin_by_id(key.device_id).is_some_and(|p| p.format == PluginFormat::Clap);
        let readonly = pipc
            .plugin_params
            .info(key.device_id, key.param_id)
            .is_some_and(|p| p.flags & common::protocol::plugin_param_flags::READONLY != 0);
        if !is_clap
            || readonly
            || !self.activity.plugin_host_active
            || !pipc.open_plugin_guis.contains_key(&key.device_id)
            || pipc.plugins_with_gestures.contains(&key.device_id)
            || !self.cur.pipc.inferred_plugin_gestures.insert(key)
        {
            return;
        }
        self.plugin_window_gesture_begin(key.device_id, key.param_id);
    }

    /// 補った gesture には離した通知が来ないので、主ボタンを離したら閉じる (`on_tick`)。
    pub(crate) fn close_inferred_plugin_gestures(&mut self) {
        for key in std::mem::take(&mut self.cur.pipc.inferred_plugin_gestures) {
            self.plugin_window_gesture_end(key.device_id, key.param_id);
        }
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

#[cfg(test)]
mod tests {
    use common::model::{AutomationTarget, Device, PluginInstance, Track};
    use common::protocol::{PluginEvent, PluginParamInfo, plugin_param_flags};

    use super::*;

    const KNOB: u32 = 7;
    const METER: u32 = 8;
    const OTHER_KNOB: u32 = 9;

    fn info(id: u32, flags: u32) -> PluginParamInfo {
        PluginParamInfo {
            id,
            name: format!("P{id}"),
            module: String::new(),
            min_value: 0.0,
            max_value: 1.0,
            default_value: 0.5,
            flags,
        }
    }

    fn app_with_plugin(format: PluginFormat) -> (AppData, u32, u64) {
        let mut app = crate::test_support::headless_app();
        let (mut track_id, mut device_id) = (0, 0);
        app.edit_song(|song| {
            track_id = song.alloc_track_id();
            let mut inst = PluginInstance::new("com.example.rack".into(), format);
            inst.id = song.alloc_device_id();
            device_id = inst.id;
            song.tracks.push(Track { id: track_id, devices: vec![Device::Plugin(inst)], ..Track::default() });
        });
        let params = vec![info(KNOB, 0), info(METER, plugin_param_flags::READONLY), info(OTHER_KNOB, 0)];
        app.cur.pipc.plugin_params.insert(device_id, params);
        app.cur.pipc.open_plugin_guis.insert(device_id, String::new());
        app.activity.plugin_host_active = true;
        (app, track_id, device_id)
    }

    fn real_touch(app: &mut AppData, device: u64, param_id: u32) {
        let device = app.dev(device);
        app.handle_event(AppEvent::Plugin(PluginEvent::PluginParamTouched { device, param_id, display_name: String::new() }));
    }

    fn target(device_id: u64, param_id: u32) -> AutomationTarget {
        AutomationTarget::PluginParam { device_id, param_id, legacy_device_index: None }
    }

    fn touched(app: &AppData) -> Option<AutomationTarget> {
        app.cur.peph.last_touched_param.as_ref().map(|t| t.target.clone())
    }

    fn param(device_id: u64, param_id: u32) -> DeviceParamKey {
        DeviceParamKey { device_id, param_id }
    }

    /// VCV Rack の CLAP 版の形: 掴むと gesture が 1 つ開き、離すと閉じて、その param が `A` キーの的として残る。
    #[test]
    fn a_clap_plugin_that_only_reports_values_gets_a_gesture_while_the_knob_is_held() {
        let (mut app, track, device) = app_with_plugin(PluginFormat::Clap);
        let gesture = (track, target(device, KNOB));

        app.infer_plugin_window_gesture(param(device, KNOB));
        app.infer_plugin_window_gesture(param(device, KNOB)); // ドラッグ中の 2 つ目以降の値
        assert_eq!(app.cur.pipc.inferred_plugin_gestures.len(), 1);
        assert_eq!(app.cur.recording.active_param_gestures.get(&gesture), Some(&ParamSurface::PluginWindow));
        assert!(app.cur.song_doc.gesture_open(GestureOwner::ParamGestures));

        app.close_inferred_plugin_gestures();
        assert_eq!(app.cur.pipc.inferred_plugin_gestures.len(), 0);
        assert_eq!(app.cur.recording.active_param_gestures.len(), 0);
        assert!(!app.cur.song_doc.gesture_open(GestureOwner::ParamGestures));
        assert_eq!(touched(&app), Some(target(device, KNOB)));

        app.add_automation_from_last_touched();
        let (lanes, _) = app.cur.song_doc.song().param_stores(track).expect("store");
        assert_eq!(lanes.iter().map(|l| l.target.clone()).collect::<Vec<_>>(), vec![target(device, KNOB)]);
    }

    /// VST3 の `performEdit` はプログラム変更でも来るので補わない。
    #[test]
    fn a_vst3_perform_edit_without_begin_edit_is_not_a_touch() {
        let (mut app, _track, device) = app_with_plugin(PluginFormat::Vst3);
        app.infer_plugin_window_gesture(param(device, KNOB));
        assert_eq!(touched(&app), None);
        assert_eq!(app.cur.recording.active_param_gestures.len(), 0);
    }

    /// 補った gesture が開いている param に本物が来たら引き継ぐ。閉じるのはボタンでなく本物の End。
    #[test]
    fn a_real_gesture_takes_over_an_inferred_one() {
        let (mut app, _track, device) = app_with_plugin(PluginFormat::Clap);
        app.infer_plugin_window_gesture(param(device, KNOB));
        real_touch(&mut app, device, KNOB);
        app.close_inferred_plugin_gestures();
        assert_eq!(app.cur.recording.active_param_gestures.len(), 1);
        app.plugin_window_gesture_end(device, KNOB);
        assert_eq!(app.cur.recording.active_param_gestures.len(), 0);
    }

    /// CLAP の値はユーザーの編集とは限らない: エディタが閉じている / プラグインの窓がアクティブでない / メーター /
    /// 本物の gesture を出すプラグインで連動して動いた param は「触った」にならない。
    #[test]
    fn clap_values_that_are_not_user_edits_do_not_count_as_touches() {
        let (mut app, _track, device) = app_with_plugin(PluginFormat::Clap);
        app.infer_plugin_window_gesture(param(device, METER));
        app.activity.plugin_host_active = false;
        app.infer_plugin_window_gesture(param(device, KNOB));
        app.activity.plugin_host_active = true;
        let title = app.cur.pipc.open_plugin_guis.remove(&device).expect("開いている");
        app.infer_plugin_window_gesture(param(device, KNOB));
        app.cur.pipc.open_plugin_guis.insert(device, title);
        assert_eq!(touched(&app), None);
        assert_eq!(app.cur.pipc.inferred_plugin_gestures.len(), 0);

        real_touch(&mut app, device, KNOB);
        app.plugin_window_gesture_end(device, KNOB);
        app.infer_plugin_window_gesture(param(device, OTHER_KNOB));
        assert_eq!(touched(&app), Some(target(device, KNOB)), "本物の gesture を出すプラグインには補わない");
        assert_eq!(app.cur.pipc.inferred_plugin_gestures.len(), 0);
    }

    /// 掴んでいる間に device かエディタ窓が消える / 持ち主のトラックが変わる → undo の bracket が開き続けない
    /// (開き続けると以後の編集が全部 1 undo step に固まる)。
    #[test]
    fn gestures_close_when_the_device_or_editor_is_gone_or_the_track_changed() {
        let (mut app, track, device) = app_with_plugin(PluginFormat::Clap);
        let closed = |app: &AppData, why: &str| {
            assert_eq!(app.cur.recording.active_param_gestures.len(), 0, "{why}");
            assert!(!app.cur.song_doc.gesture_open(GestureOwner::ParamGestures), "{why}");
        };

        app.infer_plugin_window_gesture(param(device, KNOB));
        let surface = app.cur.recording.active_param_gestures.remove(&(track, target(device, KNOB))).expect("開いている");
        app.cur.recording.active_param_gestures.insert((track + 1, target(device, KNOB)), surface);
        app.close_inferred_plugin_gestures();
        closed(&app, "Undo で別トラックへ戻った");

        real_touch(&mut app, device, OTHER_KNOB);
        app.cur.recording.active_param_gestures.insert((track + 1, target(device, OTHER_KNOB)), surface);
        app.plugin_window_gesture_end(device, OTHER_KNOB);
        closed(&app, "同じ target の key が 2 つ");

        real_touch(&mut app, device, OTHER_KNOB);
        app.forget_device_caches(device);
        closed(&app, "device が消えた");

        real_touch(&mut app, device, OTHER_KNOB);
        app.on_gui_closed(device);
        closed(&app, "エディタ窓が閉じた");

        real_touch(&mut app, device, OTHER_KNOB);
        app.forget_all_plugin_window_gestures();
        closed(&app, "曲の入れ替え");
    }
}
