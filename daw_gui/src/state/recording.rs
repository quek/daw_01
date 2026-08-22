// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

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
    /// r.md #51: ユーザーが録音したいと言っているか (= Rec ボタンの点灯状態)。
    /// **daw_gui が持つ唯一の録音状態**で、これが意思、[`Self::live`] が事実。
    /// `start_recording` で true、パンチアウト / 停止 / 書き出し / 子プロセス
    /// crash で false。 session-only / Undo 対象外。
    pub requested: bool,
    /// r.md #51: 今この瞬間ノートを記録してよいか (audio engine の
    /// `AudioBridge::recording_live` の観測ミラー、writer は `on_tick` 1 箇所)。
    ///
    /// `録音要求あり && 再生中 && count-in 完了` を engine が判定して publish する。
    /// GUI 側で導出しないのは、次の 2 つを構造的に防ぐため:
    ///
    /// - 停止 / 一時停止中に書くと凍ったプレイヘッドへノートが積み上がる。
    /// - count-in の残量 `0` は「まだ始まっていない」と「終わった」の両方を
    ///   意味するので、要求直後の stale な Tick で count-in を飛ばす。
    pub live: bool,
    /// Phase 7 B4 Step C (2026-05-13): count-in bars (0 / 1 / 2)。 transport
    /// bar の dropdown で設定、 default 0。 session-only state。
    pub count_in_bars: u8,
    /// Phase 7 B4 Step D (2026-05-13): 押している最中の録音ノート。
    /// `(track_id, key) → (start_beat, note_id)`。
    ///
    /// note_off で `start_beat` を取り出して `length_beats = playhead - start` を
    /// 確定する。 **どのノートかは `note_id` で確定する** (r.md #51、不変条件 1):
    /// 旧実装は `start_beat` と pitch の値照合で探し直していたので、同じ位置に
    /// 同じ高さのノートが 2 本あると常に 1 本目に当たり、2 本目以降が仮の長さ
    /// (0.05 拍) のまま残った。 録音セッションのクローズで、押しっぱなしのぶんも
    /// ここを見て長さを確定する。
    pub midi_recording_active_notes:
        std::collections::HashMap<(u32, u8), (f64, u32)>,
    /// r.md #51: モニターで鳴らしている `(track_id, pitch)`。
    ///
    /// 録音待機トラックは transport 状態に関わらず入力を発音する
    /// (一般的なインプットモニター) ので、note-off を取りこぼすと音が鳴り
    /// 続ける。 arm 解除 / パニック / 停止で確実に消音するための held-value。
    pub monitor_notes: std::collections::HashSet<(u32, u8)>,
    /// Phase 7 B4 Step C (2026-05-13): count-in 開始前の metronome_enabled
    /// 状態 snapshot。 count-in 中だけ強制 ON にし、録音セッションのクローズで
    /// 元の値へ戻す (= user の「click off」 設定を尊重しつつ count-in 中は
    /// guide が聞こえる)。 `None` = 強制 ON していない (count-in 無しの録音を
    /// 含む) ので復元不要。 r.md #51: 旧実装は count-in の有無に関わらず
    /// snapshot していたため、count-in Off の録音中にユーザーがメトロノームを
    /// 切り替えると録音終了時に巻き戻されていた。
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
    /// r.md #67: カーソルキー (↑/↓) で音程を変えたときに短く鳴らす試聴音
    /// `(track_id, pitch, 消音予定時刻)`。
    ///
    /// 鍵盤レーンの [`Self::preview_note`] とは **別枠**。 あちらは「押している間ずっと
    /// 鳴らす」 held-value で、 widget が毎フレーム差分して note-off を送るため、
    /// キー操作由来の発音を載せると 1 フレームで消えてしまう。 こちらは時間で自動消音する
    /// one-shot (`AppData::expire_nudge_audition` が `on_tick` から回収)。
    /// session-only / Undo 対象外。
    pub nudge_audition: Option<(u32, u8, std::time::Instant)>,
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
