//! S3b-1: AppData state group (RecordingState)。 docs/plan_arch_refactor.md §7.5
//! の分割表に従って app.rs の AppData から機械移送したフィールド群。

pub struct RecordingState {
    /// Phase 4 (`docs/plan_automation.md` §6): automation recording mode。
    /// transport bar の 4 way toggle (Read / Touch / Latch / Write) で切替。
    /// session-only / Undo 対象外 (= 起動時 `Read`、 project 保存対象外)。
    /// 起動時の値は Bitwig / Ableton Live / Reaper と同じく `Read`。
    /// Phase 4 Step C+ で audio thread もこの値を読んで recording lane の
    /// curve eval をバイパスし、 GUI からの knob 値を `playhead_beat`
    /// 起点に point として書き込む。
    pub recording_mode: common::model::RecordingMode,
    /// Phase 7 B4 Step D (2026-05-13): MIDI 録音中 flag。 Record toggle ON
    /// で true、 Stop / OFF で false。 true のとき handle_midi_note_on/off
    /// は armed track の MIDI clip に書き込む (= 既存 step-input mode は
    /// midi_recording == false のときのみ動作)。 session-only / Undo 対象外。
    pub midi_recording: bool,
    /// Phase 7 B4 Step C (2026-05-13): count-in 中で「0 拍到達まで preroll
    /// 待ち」 状態。 `start_recording` で `count_in_bars > 0` なら true、
    /// audio engine の `preroll_remaining_samples` mirror が 0 に達したら
    /// `on_tick` が `midi_recording_pending → midi_recording` 遷移させる。
    /// metronome は pending 中だけ強制 ON で click guide を流す。
    pub midi_recording_pending: bool,
    /// Phase 7 B4 Step C (2026-05-13): count-in bars (0 / 1 / 2)。 transport
    /// bar の dropdown で設定、 default 0。 session-only state。
    pub count_in_bars: u8,
    /// Phase 7 B4 Step D (2026-05-13): 直近 note_on の `(track_id, key) →
    /// start_beat`。 note_off 受信時に start_beat 取り出して `length_beats =
    /// playhead - start` を確定する。 stop / midi_recording 解除で clear。
    pub midi_recording_active_notes:
        std::collections::HashMap<(u32, u8), f64>,
    /// Phase 7 B4 Step C (2026-05-13): count-in 開始前の metronome_enabled
    /// 状態 snapshot。 count-in 中だけ強制 ON にし、 `stop_recording` 時に
    /// 元の値へ戻す (= user の「click off」 設定を尊重しつつ count-in 中は
    /// guide が聞こえる)。 None なら recording 開始前 = 復元不要。
    pub metronome_enabled_pre_recording: Option<bool>,
    /// Phase 7 B1-M Step 2 (2026-05-13): MIDI Learn の bind 待ち target。
    /// `Some` なら次に来る MIDI CC をこの target に bind (= `Song.midi_bindings`
    /// に追加 + None に戻す)。 `None` (default) なら CC は既存 binding lookup
    /// で target に値を流す (= 通常モード)。 transport bar の「MIDI Learn」
    /// button で `StartMidiLearn(target)` 経由で Some 化、 `CancelMidiLearn`
    /// or 1 度の CC 受信 (= bind 確定) で None に戻る。
    pub midi_learn_target: Option<common::model::BindingTarget>,
    /// Phase 4 Step B (`docs/plan_automation.md` §6): 現在 user が触っている
    /// (= dragging) parameter の集合。 mixer / inspector / lane default knob
    /// の press で insert、 release で remove。 plugin GUI 経由の gesture も
    /// CLAP `CLAP_EVENT_PARAM_GESTURE_BEGIN/END` IPC からここに反映する
    /// (Phase 2c の `PluginParamTouchedFromChild` は begin のみ送るので
    /// end の IPC 追加は Step B follow-up)。 session-only / Undo 対象外。
    /// Step C で audio thread はこの set を読んで該当 lane の curve eval
    /// を bypass する。 `latched_param_gestures` (= Latch mode 用に保持する
    /// "1 度触れた parameter") と組み合わせて、 Read/Touch/Latch/Write の
    /// 4 mode の挙動差を audio thread 側で実現する。
    pub active_param_gestures:
        std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    /// Phase 4 Step C (`docs/plan_automation.md` §6): `Latch` / `Write` mode
    /// で「再生中に 1 度でも触れた parameter」 を transport stop まで保持する
    /// set。 `ParamGestureBegin` が `is_playing == true` 中に発火すると
    /// 即時 insert され、 `stop()` で clear される。 `Touch` mode では使われ
    /// ない (= active_param_gestures だけが「現在 recording 中」 を意味する)。
    /// audio thread への通知は active ∪ latched の和集合を毎 tick 送る (Step
    /// C-2 で IPC `SetRecordingLanes` が landing したら lock-free 化、 当面
    /// は per-tick LoadSong で済ます)。 session-only / Undo 対象外。
    pub latched_param_gestures:
        std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    /// Phase 4 Step C: parameter ごとの「直近 record した beat」 を保持する
    /// throttle 用 map。 audio bridge tick は ~60Hz、 BPM=120 で 1/64 beat
    /// は ~31ms。 同 tick 内で同じ playhead に何度も point insert しない
    /// よう、 `playhead - last_beat >= 1/64` のときだけ insert する。
    /// `stop()` で clear。 session-only / Undo 対象外。
    pub recording_last_beat:
        std::collections::HashMap<(u32, common::model::AutomationTarget), f64>,
    /// Phase 4 Step C-2: 直近 `AudioCommand::SetRecordingLanes` で audio thread
    /// に送った recording lane set のスナップショット。 GUI の currently
    /// recording set (= active ∪ latched, mode 依存) と diff を取って、 変化
    /// したときだけ IPC を送信する。 LoadSong は set が「縮んだ」 (= 1 度
    /// recording 終了した lane が出た) ときに送る (= audio thread が curve
    /// eval に戻るときに最新 points を読ませる)。 session-only / Undo 対象外。
    pub last_sent_recording_lanes:
        std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    /// 鍵盤レーン click のプレビュー発音中の `(track_id, pitch)` (gui_01 #055,
    /// `docs/plan_pianoroll_keyboard_preview.md`)。 widget の
    /// `PianoRollResponse::keyboard_active_pitch` を前フレーム値と差分して
    /// note-on/off を導出するための held-value。 押下開始した track id を pitch
    /// と一緒に持つことで、 note-off を必ず note-on と同じ track へ送る
    /// (glissando / release で stuck note を防ぐ)。 `None` で発音なし。
    /// session-only (project save には含めない)。
    pub preview_note: Option<(u32, u8)>,
    pub midi_input_label: String,

    pub step_cursor_beat: f64,
    pub step_size_beats: f64,
    /// Phase 7 B5 (`docs/plan_scale.html` §5.2): Snap Live Input toggle。 ON
    /// のとき MIDI 録音中の note_on pitch を `Song.scale_at(playhead).snap(pitch)`
    /// で in-scale に寄せる。 transport bar の toggle で切替、 session-only
    /// state。 step input (recording 停止中の MIDI input) には適用しない
    /// (= pitch を「聞いて」 決める用途、 Cubase / Bitwig も同方針)。
    pub snap_live_input: bool,
}
