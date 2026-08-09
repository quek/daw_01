//! `AppEvent` とその周辺 marker enum (app.rs から機械分割)。
//!
//! dispatch (`AppData::handle_event`) は app.rs、各 variant の処理本体は
//! `crate::handler::*` に属する。
use crate::app_types::*;
use std::path::{PathBuf};
use common::model::SendMode;

/// 既存の event handler と一貫性を保つため、enum 全体に `#[allow(dead_code)]`
/// を付ける。
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum AppEvent {
    /// daw_audio からの protocol event (旧 1:1 bridge / *FromChild を廃した
    /// direct-wrap)。 `crate::handler::ipc::dispatch_audio_event` が処理する。
    Audio(common::protocol::AudioEvent),
    /// daw_plugin_host からの protocol event (direct-wrap)。
    /// `crate::handler::ipc::dispatch_plugin_event` が処理する。
    Plugin(common::protocol::PluginEvent),
    // -------- File / playback ---------------------------------------------
    New,
    Open,
    Save,
    SaveAs,
    /// 未保存変更ありのガードモーダルで「保存して続行」。 save を発行し、
    /// 完了後に保留中の操作 (終了 / New / Open) を実行する (plugin 有り
    /// project は非同期保存を待つ)。
    DirtyGuardSave,
    /// ガードモーダルで「保存せず続行」。 保存せず即操作を実行。
    DirtyGuardDiscard,
    /// ガードモーダルで「キャンセル」 (Esc / 外クリック / ✕ 含む)。
    /// 操作を取りやめてアプリに戻る。
    DirtyGuardCancel,
    /// 別の daw_gui を起動しようとした (single-instance)。 2 つ目の
    /// プロセスが既存インスタンスにこれを送って前面化を要求する。 window 操作
    /// なので runner の `user_event` が直接処理し、 `handle_event` には届かない。
    RaiseMainWindow,
    Play,
    Stop,
    PlayToggle,
    /// パニック — 鳴っている全ての音を即座に止める transport ボタン。
    /// 再生中なら transport stop し、全 plugin を deactivate→activate で
    /// 再初期化する（WAV 書き出しと同じ `ReinitAllPlugins` 機構を流用）。
    Panic,
    /// `f` キー。カーソル直下の拍 (song-absolute, 現在の snap 設定で吸着済)
    /// へプレイヘッドを移動して再生する。再生中は seek してシームレスに継続、停止中は
    /// その位置から再生開始。view 層 (`dispatch_shortcuts`) が snap / ルーティング /
    /// song-absolute 解決済みの beat を渡すので、handler は set-playhead + seek/play のみ。
    PlayFromCursor { beat: f64 },
    /// `Home` キー: プレイヘッドを先頭 (時間的に最初) のクリップの開始位置へ移動。
    /// 既にそこへ飛んだ直後なら 1.1.1 (song 先頭 = beat 0) へ (2 度押しで先頭)。
    /// clip が無ければ先頭へ。 r.md #10。
    GotoTimelineHome,
    /// `End` キー: プレイヘッドを最後のクリップの直後 (content 終端 beat) へ移動。
    /// clip が無ければ先頭 (beat 0)。 r.md #10。
    GotoTimelineEnd,
    ToggleLoop,
    /// `R` キー: 選択素材の bounding range を loop 範囲に設定して loop ON + 再生開始。
    /// 既に loop ON かつ範囲が一致するなら loop を OFF にする (再生は維持)。 選択が
    /// 無ければ no-op。 `automation` は対象面 (通常 clip / automation clip) を root の
    /// `edit_surface` arbiter が解決した結果 (= Del/Cut と同じ last-selection-wins)。
    LoopSelectedClipToggle { automation: bool },
    /// Transport BPM 入力欄の文字列が変わった (commit ではなく途中入力)。
    /// Undo 対象外。
    BpmEditChanged(String),
    /// BPM 入力欄で Enter (commit)。 parse + clamp(1.0..=400.0) + Song.bpm 反映 +
    /// `bpm_edit_text` を formatted な現値に書き戻す。 Undo 対象。
    CommitBpmEdit,
    /// time_sig numerator 入力欄の文字列が変わった。 Undo 対象外。
    TimeSigNumEditChanged(String),
    /// numerator 入力欄で Enter (commit)。 parse + clamp(1..=32) + 反映。 Undo 対象。
    CommitTimeSigNumEdit,
    /// time_sig denominator dropdown で選択された (2/4/8/16 のみ valid)。 Undo 対象。
    SetSongTimeSigDenominator(u8),
    /// Phase 5 Step 5.1 follow-up (gui_01 #035): transport の BPM
    /// scrubable_number drag 中に流れる連続 BPM 変化。 widget 内で 1.0..=400.0
    /// に clamp 済前提だが defensive で再 clamp。 `bpm_edit_text` も同期して
    /// text input mode の表示を追随させる。 Undo 対象外 (= 連続発火、
    /// release edge の ParamGestureEnd で 1 step Undo 化を別途検討)。
    /// 軽量 IPC `AudioCommand::SetSongBpm` で audio engine 即時反映。
    SetSongBpmFromScrub(f32),
    /// Phase 5 Step 5.1 follow-up: TimeSig numerator scrub。 1..=32 clamp、
    /// `time_sig_num_edit_text` 同期、 軽量 IPC `AudioCommand::SetSongTimeSigNumerator`。
    SetSongTimeSigNumFromScrub(u8),
    Undo,
    Redo,
    /// r.md #29: Undo 履歴パネルの開閉トグル (View メニュー / パネル ✕ / Esc)。
    /// session-only な UI 状態なので Undo 対象外。
    ToggleUndoHistory,
    /// r.md #29: 履歴リストの行 click。 `index` = [`crate::state::SongDoc::history_labels`]
    /// の 0 始まり index。 その state まで一気に Undo / Redo する
    /// ([`crate::state::SongDoc::jump_to`])。 履歴操作自体は Undo 対象外。
    JumpHistory(usize),
    QuantizeSelectedNotes(u8),
    /// 鍵盤レーン click のピッチプレビュー (gui_01 #055,
    /// `docs/plan_pianoroll_keyboard_preview.md`)。 piano_roll widget が毎フレーム
    /// `resp.keyboard_active_pitch` を `preview_note` の pitch と比較し、 変化した
    /// ときだけ発火する。 `track_idx` は描画中 clip の track (Vec index)、 `pitch`
    /// は今フレームの押下 pitch (`None` = release / 鍵盤外)。 handler が前回
    /// `preview_note` と差分して note-on/off IPC を送る。 Undo 対象外。
    PreviewPitchChanged { track_idx: u32, pitch: Option<u8> },
    SetNoteVelocity { note: u32, velocity: u8 },
    /// gui_01 #018 (M14 Phase 64): velocity lane drag で 1 batch 更新。
    /// `selected_clip` の note を `(id, velocity)` で一括書き換え。 1 drag =
    /// 1 Undo step。
    SetNoteVelocities(Vec<(u32, u8)>),
    AddInstrumentTrack,
    /// Group the selected tracks under a fresh group track. Mirrors
    /// Ableton Live's Cmd/Ctrl+G: the *selection-root* tracks become
    /// children of the new group (their `parent_group_id` is set), and
    /// the new group is inserted just *before* the highest-positioned
    /// selected track (= 一番上の選択 track の直前 / 子の上にヘッダー)。
    /// `track_ids` must be non-empty — Live forbids empty groups and so
    /// do we. only the *selection roots*
    /// (tracks whose `parent_group_id` is not itself in the selection)
    /// are re-parented, so a selected group keeps its own children and
    /// nesting is preserved (depth unbounded) instead of being flattened.
    GroupSelectedTracks {
        track_ids: Vec<u32>,
    },
    /// gui_01 #028 (M14 Phase 63n-1): track 行の disclosure ▶/▼ click。
    /// `expanded_automation_tracks` の `track_id` を反転し、 widget が
    /// 次フレームで lane 群を展開 / 折り畳む。 session-only な UI 状態
    /// なので Undo / save 対象外。
    ToggleTrackAutomationCollapsed {
        track_id: u32,
    },
    // ----------------------------------------------------------------
    // gui_01 #028 (M14 Phase 63n-2) — automation lane / point 編集
    // ----------------------------------------------------------------
    /// Lane 全体の bypass。`★`/`☆` icon click。
    SetLaneEnabled {
        track_id: u32,
        lane_id: u32,
        enabled: bool,
    },
    /// Lane の表示 / 非表示。`👁` icon click。
    SetLaneVisible {
        track_id: u32,
        lane_id: u32,
        visible: bool,
    },
    /// Lane header の default value slider drag。`prev` / `next` は
    /// 共に **normalized 0..1** (widget の slider 帯と同単位)。handler
    /// 側で `lane.target` を引いて plain 単位に逆変換してから格納する。
    /// drag 中は per-frame で発火 (live preview)、release で 1 度確定。
    SetLaneDefault {
        track_id: u32,
        lane_id: u32,
        prev_norm: f32,
        next_norm: f32,
    },
    /// Lane の `✕` icon click → `Track.automation_lanes` から該当 lane
    /// を除去。lane 内 clip の `content_id` が他 clip と共有されてい
    /// なければ `clip_contents` の該当 entry も `gc_clip_contents`
    /// 次サイクルで GC される (このイベント自体は触らない)。
    DeleteLane {
        track_id: u32,
        lane_id: u32,
    },
    /// gui_01 #030 (M14 Phase 63n-5): lane 高さ drag (Alt+drag or
    /// 下端 splitter)。`prev` / `next` は px、widget 側で
    /// `[automation_lane_min_height_px, automation_lane_max_height_px]`
    /// に clamp 済。drag 中は per-frame 発火 (live preview)、release で
    /// 1 件確定。`SetLaneDefault` と同パターン。
    SetLaneHeight {
        track_id: u32,
        lane_id: u32,
        prev_px: u16,
        next_px: u16,
    },
    /// gui_01 #031 (M14 Phase 63n-6): MIDI track row 高さの個別 override。
    /// Alt+drag or 下端 splitter drag で発火。 既存 `Alt+wheel`
    /// (`SetTrackRowH(f32)` = global default) と独立、 個別 track は
    /// override map に保存。 drag 中は per-frame 発火、 release で確定。
    SetSingleTrackRowH {
        track_id: u32,
        prev_px: u16,
        next_px: u16,
    },
    /// Lane body 内 dblclick で 1 point 追加。`time_beat` は clip-local、
    /// `value_norm` は normalized 0..1 (widget が clip rect 内 cursor
    /// 座標から計算済)。handler は norm → plain 変換 + `time_beat` 昇順
    /// 維持を担当。
    AddAutomationPoint {
        track_id: u32,
        lane_id: u32,
        clip_id: u32,
        time_beat: f64,
        value_norm: f32,
    },
    /// 1 つ以上の point の position 更新 (release 時に 1 度発火)。
    /// `MoveAutomationPointEntry` の `value_norm` は normalized、handler
    /// 側で plain 化。`point_idx` は **同 frame 内のみ valid** なので、
    /// drag session 内では gui_01 widget が prev_index を保持する前提
    /// (本 event 受信時はそのフレームの index で OK)。
    MoveAutomationPoints {
        deltas: Vec<MoveAutomationPointEntry>,
    },
    /// Alt+click on point → 即時削除 (1 件)、もしくは将来の rect select
    /// → 一括削除を batch で受ける。`Vec<AutomationPointKey>` を
    /// daw_01 内部型 (`(track_id, lane_id, clip_id, point_idx)` 4-tuple
    /// 相当) で運ぶ。
    DeleteAutomationPoints {
        points: Vec<AutomationPointKeyRef>,
    },
    /// 既存 point 上の dblclick → その point の値の **inline 数値入力**
    /// を開始する。session-only (undo 対象外)。`editing_automation_point` を
    /// セットするだけで、 描画は `arrangement_view` が `automation_point_rects`
    /// から rect を引いて `text_input_at_focused` overlay を出す。
    BeginEditAutomationPointValue {
        key: AutomationPointKeyRef,
    },
    /// inline 数値入力の確定 → 1 point の `value` を **plain 単位の
    /// 絶対値** で上書き (時間 = `time_beat` は不変、 sort 不要)。Undo step 化
    /// (構造変化系)。`MoveAutomationPoints` (norm delta) と異なり absolute plain。
    SetAutomationPointValue {
        key: AutomationPointKeyRef,
        value: f64,
    },
    /// 右クリック popup → curve type 選択 → 1 point の `curve` 更新。
    /// `prev` / `next` は Undo 構築用に両方持たせる (gui_01 §11.4 と
    /// 同 idiom、`SetTrackVolume` 等と同じ pattern)。
    SetAutomationCurveType {
        track_id: u32,
        lane_id: u32,
        clip_id: u32,
        point_idx: u32,
        prev: common::model::AutomationCurve,
        next: common::model::AutomationCurve,
    },
    /// gui_01 #033 Phase 63n-9: Bezier curve 中央 handle drag (lane 高さ
    /// 連動 sensitivity、 Alt × 0.2 微調整) の release で 1 件発火。
    /// 当該 point の `curve` を `AutomationCurve::Bezier { tension: next }`
    /// で上書きする。 widget 側で `-1.0..=1.0` clamp 済。 type が Bezier
    /// 以外だった場合 (= race) は no-op (handler 内で current curve を
    /// 確認、 異なれば skip)。
    SetAutomationCurveBezierTension {
        track_id: u32,
        lane_id: u32,
        clip_id: u32,
        point_idx: u32,
        prev: f32,
        next: f32,
    },
    /// gui_01 #033 Phase 63n-9: Exponential curve 中央 handle drag の
    /// release で 1 件発火。 当該 point の `curve` を `Exponential { bend:
    /// next }` で上書き。 値域 / race 扱いは `SetAutomationCurveBezierTension`
    /// と同。
    SetAutomationCurveExponentialBend {
        track_id: u32,
        lane_id: u32,
        clip_id: u32,
        point_idx: u32,
        prev: f32,
        next: f32,
    },
    // ----------------------------------------------------------------
    // gui_01 #028 (M14 Phase 63n-3) — automation clip drag / select
    // ----------------------------------------------------------------
    /// 修飾なし drag release → source lane から clip を remove + `to_lane`
    /// に start_beat 昇順 insert。lane 跨ぎ accept (target 不一致も OK)。
    MoveAutomationClips {
        deltas: Vec<MoveAutomationClipEntry>,
    },
    /// Ctrl+drag release → source 残置 + 同一 `ContentId` を持つ新 clip
    /// を `to_lane` に追加 (linked、curve を共有)。
    CloneAutomationClipsLinked {
        deltas: Vec<MoveAutomationClipEntry>,
    },
    /// Ctrl+Shift+drag release → source 残置 + content を deep clone (新
    /// `ContentId` 採番) した独立 clip を `to_lane` に追加。
    CloneAutomationClipsIndependent {
        deltas: Vec<MoveAutomationClipEntry>,
    },
    /// 左右 edge drag release → 各 clip の start / len 上書き。
    ResizeAutomationClips {
        deltas: Vec<ResizeAutomationClipEntry>,
    },
    /// caller-driven (右クリック menu / shortcut から発火、 widget は
    /// 提供せず) → 該当 lane から `clip_id` で除去。content の GC は次の
    /// save / `gc_clip_contents` で行う。
    DeleteAutomationClips {
        keys: Vec<common::model::AutomationClipKey>,
    },
    /// 短 click on automation clip → `selected_automation_clips` を
    /// `next` で上書き。MIDI 用 `selected_clips` は触らない (= 共存)。
    SelectAutomationClips {
        prev: Vec<common::model::AutomationClipKey>,
        next: Vec<common::model::AutomationClipKey>,
    },
    /// Phase 3 (gui_01 #033 widget 側 lasso 完了後に発火される想定):
    /// `selected_automation_points` を `next` で上書き。 `prev` は Undo 用
    /// (selection 自体は session state なので Undo 非対象だが、 `SelectClips`
    /// と同じ idiom で signature を揃える)。
    SelectAutomationPoints {
        prev: Vec<AutomationPointKeyRef>,
        next: Vec<AutomationPointKeyRef>,
    },
    /// Phase 3: `selected_automation_points` を grid (`1/div` beat) に snap。
    /// piano roll の `QuantizeSelectedNotes` と同 idiom。 同 clip 内の
    /// point は sort 維持のためまとめて sort し直し、 selection も新 idx に
    /// 再採番する。 `div = 1` で 1 beat 単位、 `4` で 1/4 beat 単位。
    QuantizeSelectedAutomationPoints(u8),
    /// 右クリック menu「Make Unique」 → 共有中 (`refcount >= 2`) の
    /// automation clip の content を deep clone (新 `ContentId`)、独立化。
    /// 既に独立 clip の場合は status_message で通知。MIDI clip 用
    /// `MakeClipUnique(ClipRef)` と同 idiom の lane 版。
    MakeAutomationClipUnique(common::model::AutomationClipKey),
    /// D shortcut: 選択中の automation clip 群をまとめて共有コピー。
    /// MIDI 用 `DuplicateClipsShared` の automation lane 版。 選択ブロック span
    /// だけ後ろにずらして複製し、 新 key 群を `selected_automation_clips` に
    /// 上書きする (D 連打で後方連鎖)。
    DuplicateAutomationClipsShared(Vec<common::model::AutomationClipKey>),
    /// Alt+D shortcut: 選択中の automation clip 群をまとめて独立コピー
    /// (content を deep clone + 新 ContentId)。 配置・選択は shared 版と同じ。
    DuplicateAutomationClipsUnique(Vec<common::model::AutomationClipKey>),
    /// gui_01 #028 §7.3: parameter touch 通知。inspector の knob drag /
    /// plugin GUI の knob 操作 (Phase 2+ で IPC 経由) で発火し、
    /// `last_touched_param` を更新。`A` キー shortcut の source になる。
    /// session-only / Undo 不要。
    TouchParam {
        track_id: u32,
        target: common::model::AutomationTarget,
        display_name: String,
    },
    /// `A` キー shortcut。`last_touched_param` の lane を該当 track に
    /// 追加。既に同 target の lane があれば visible = true で復活、なけ
    /// れば新規作成 (default = 現在の plain 値)。`expanded_automation_tracks`
    /// にも所有 track を insert して即時展開。
    AddAutomationFromLastTouched,

    /// Inspector の image event section「📈」 ボタンから発火。 選択中
    /// image clip の track に `ImageBuiltin(field)` lane を追加 (既存
    /// あれば visible / enabled 復活)。 default_value は first ImageEvent
    /// の現値 (`docs/plan_image_automation.md` §4.1)。 undoable。
    AddImageAutomationLane { field: common::model::ImageBuiltinParam },

    /// Inspector の automate toggle が ON 状態のとき、 もう一度押すと
    /// 該当 `ImageBuiltin(field)` lane を track から削除する。 削除後は
    /// ImageEvent.field がふたたび effective (= override 解除)。 lane
    /// が無ければ no-op。 undoable。
    RemoveImageAutomationLane { field: common::model::ImageBuiltinParam },

    /// docs/plan_text_overlay.md §4 P8: Inspector の text section「A」
    /// ボタンから発火。 選択中 text clip の track に `TextBuiltin(field)`
    /// lane を追加 (既存あれば visible / enabled 復活)。 default_value は
    /// `lane_default_for_target` 経由で first TextEvent の現値 (event が
    /// 無ければ TextEvent::default 相当の常識値)。 undoable。
    AddTextAutomationLane { field: common::model::TextBuiltinParam },

    /// Inspector の automate toggle が ON 状態で再押し → 該当
    /// `TextBuiltin(field)` lane を track から削除 (= override 解除、
    /// TextEvent.field が effective に戻る)。 lane が無ければ no-op。
    /// undoable。
    RemoveTextAutomationLane { field: common::model::TextBuiltinParam },

    /// v19 (`docs/plan_tachie_group_transform.md` §5.5): 選択中 visual group
    /// track に `GroupTransform(param)` lane を追加（既存あれば visible /
    /// enabled 復活）。default_value は現 group_transform の field 値。undoable。
    AddGroupAutomationLane { param: common::model::GroupTransformParam },
    /// automate toggle 再押しで `GroupTransform(param)` lane を削除。undoable。
    RemoveGroupAutomationLane { param: common::model::GroupTransformParam },
    /// preview 上の group box drag 開始（undo snapshot を 1 個取る。group lane
    /// recording は未対応）。undoable。
    BeginGroupTransformDrag,
    /// preview drag 中の live 設定（非 undoable）。`set_group_transform_field`
    /// + edit buffer resync で inspector も同期。
    SetGroupTransformField {
        track_id: u32,
        param: common::model::GroupTransformParam,
        value: f32,
    },
    /// preview drag 終了（非 undoable、begin に snapshot あり）。
    EndGroupTransformDrag,

    /// audio / image / text
    /// inspector の scrubable_number で drag / text 編集を開始した瞬間に
    /// 発火する marker。 handler 本体は no-op、 `is_undoable` に含まれる
    /// ので handle_event 冒頭の auto push_undo_snapshot だけが効き、 drag
    /// 中の `SetClip*` 連発 (= 非 undoable) を 1 undo step に集約する
    /// (= group transform の `BeginGroupTransformDrag` と同 idiom)。
    BeginInspectorScrub,
    /// scrubable_number の drag / text 編集を終了した瞬間に発火 (非
    /// undoable、 begin 側に snapshot あり)。
    EndInspectorScrub,

    /// docs/plan_text_overlay.md §4 P6: text PiP rect drag で発火する
    /// `SetClipText{X,Y,W,H,Rotation}` 群 (= image と同 idiom)。 lane が
    /// effective なら handler 側で「TextEvent.field を直接書く」 動作で、
    /// lane override が drag を隠す挙動も同様。
    SetClipTextX { target: ClipRef, value: f32 },
    SetClipTextY { target: ClipRef, value: f32 },
    SetClipTextW { target: ClipRef, value: f32 },
    SetClipTextH { target: ClipRef, value: f32 },
    SetClipTextRotation { target: ClipRef, value: f32 },

    /// preview window で PiP rect の drag 操作を始めた瞬間に発火する
    /// marker event (`docs/plan_image_overlay.md` §4 P5)。 handler 本体
    /// は no-op、 is_undoable に含まれるので handle_event 冒頭の
    /// auto push_undo_snapshot だけが効く。 drag 中の SetClipImage*
    /// 連発を 1 個の Undo step に集約する用途 (= AE / Premiere 流の
    /// 「drag 1 stroke = 1 undo」 UX)。 同時に「drag 中の lane recording」
    /// の `ParamGestureBegin` 相当として、 lane を持つ image field を
    /// active_param_gestures に登録する。
    BeginImagePiPDrag,

    /// preview window で PiP rect の drag を終了した瞬間に発火する
    /// (= MouseInput Released)。 `BeginImagePiPDrag` で active_param
    /// _gestures に登録した image field を全て remove する。 non-
    /// undoable (= drag begin 側に snapshot がある)。
    EndImagePiPDrag,

    /// docs/plan_text_overlay.md §4 P6: text PiP rect の drag を開始 /
    /// 終了する marker (`BeginImagePiPDrag` と同 idiom)。 Begin で
    /// `TextBuiltin(_)` lane を `active_param_gestures` に seed、 End で
    /// 全 remove。 Begin は undoable (= drag 1 stroke = 1 undo)、 End は
    /// non-undoable。
    BeginTextPiPDrag,
    EndTextPiPDrag,

    /// docs/plan_text_overlay.md §4 P5: text inspector の編集パス。
    /// Mute / Text content / Font family は string-shaped で個別 event、
    /// 23 numeric field + 2 fade beats は `SetClipTextNumField` 1 event で
    /// `TextNumField` discriminator dispatch する。 数値入力は
    /// scrubable_number 化され、 on_change が直接 `SetClipTextNumField` を
    /// 発火 (= 旧 buffer 経路 `ClipTextNumEditChanged` / `CommitClipTextNumEdit`
    /// は撤去)。 lane override 経由でも同様、 lane が effective なら
    /// TextEvent.field の直接書き込みは preview に反映されない。
    SetClipTextMuted { target: ClipRef, muted: bool },
    SetClipTextContent { target: ClipRef, value: String },
    SetClipTextFontFamily { target: ClipRef, value: String },
    SetClipTextAlign { target: ClipRef, value: common::model::TextAlign },
    SetClipTextFadeInCurve { target: ClipRef, curve: common::model::FadeCurve },
    SetClipTextFadeOutCurve { target: ClipRef, curve: common::model::FadeCurve },

    /// text inspector の scrubable_number on_change から発火
    /// (drag 中 per-frame / text commit)。 `set_clip_text_num_field` で
    /// `value` (= Rotation は radians) を clamp + 全 TextEvent に書く。
    /// 非 undoable (= drag stroke を `Begin/EndInspectorScrub` で bracket)。
    SetClipTextNumField { target: ClipRef, field: TextNumField, value: f32 },

    ClipTextContentEditChanged(String),
    ClipTextFontFamilyEditChanged(String),
    CommitClipTextContentEdit,
    CommitClipTextFontFamilyEdit,

    /// 選択中 text clip の `TextEvent` 現値から文字列 edit buffer
    /// (content / font_family) を再生成。 inspector が clip 切替 / Undo /
    /// Redo の効果を反映するときに呼ぶ。 25 numeric field は
    /// scrubable_number 化され現値を summary から直接読むため、 数値 buffer
    /// の再生成は不要になった。
    ResyncClipTextEditBuffers(ClipRef),
    /// Phase 4 (`docs/plan_automation.md` §6): automation recording mode の
    /// transport 4 way toggle。 session-only / Undo 対象外。
    SetRecordingMode(common::model::RecordingMode),
    /// Phase 7 B3 (2026-05-13): メトロノーム on/off。 transport bar の toggle
    /// で発火、 `AppData.metronome_enabled` を更新 + `AudioCommand::Set
    /// MetronomeEnabled(bool)` を audio に送信。 session-only / Undo 対象外。
    SetMetronomeEnabled(bool),
    /// Phase 7 B4 Step C/D (2026-05-13): MIDI 録音 toggle。 Record button
    /// click で発火。 `count_in_bars > 0` なら preroll 開始、 0 なら即時
    /// recording 開始。 既に走行中なら stop。 session-only / Undo 対象外。
    ToggleMidiRecording,
    /// Phase 7 B4 Step C (2026-05-13): count-in bars (0 / 1 / 2) 設定。
    /// transport bar dropdown で発火。 session-only / Undo 対象外。
    SetCountInBars(u8),
    /// Phase 4 Step B (`docs/plan_automation.md` §6): mixer / inspector /
    /// plugin GUI で parameter knob の drag が **開始** した瞬間に発火。
    /// `active_param_gestures` に insert + `last_touched_param` を更新
    /// (= 既存 `TouchParam` の subsume)。 audio thread は Step C で
    /// `recording_mode != Read` 時に該当 lane の curve eval を bypass する。
    /// session-only / Undo 対象外 (= mutation は全て session field)。
    ParamGestureBegin {
        track_id: u32,
        target: common::model::AutomationTarget,
        display_name: String,
    },
    /// Phase 4 Step B: parameter knob の drag が **終了** した瞬間に発火。
    /// `active_param_gestures` から remove。 Touch mode では これで該当
    /// lane の recording が止まる (Latch / Write mode は別の latched set
    /// が transport stop まで持続するので、 本イベントだけでは止まらない)。
    /// session-only / Undo 対象外。
    ParamGestureEnd {
        track_id: u32,
        target: common::model::AutomationTarget,
    },
    /// gui_01 #029 (M14 Phase 63n-4): lane body 内 clip ギャップ
    /// dblclick で発行される clip 作成イベント。MIDI clip の
    /// `DoubleClickEmpty → CreateClip` と同 idiom の lane 版。
    /// `start_beat` は widget が snap 適用済、`len_beats` は widget
    /// style の `automation_clip_default_len_beats` (default 4.0)。
    CreateAutomationClip {
        lane: common::model::AutomationLaneKey,
        start_beat: f64,
        len_beats: f64,
    },
    /// Ungroup the selected group tracks. Children are reparented to
    /// the group's own parent (master or upper group), then the group
    /// track itself is removed. The group's `fx_chain` is lost
    /// (Ableton Live convention). Non-group tracks in the selection
    /// are silently ignored.
    UngroupTracks {
        track_ids: Vec<u32>,
    },
    /// Reparent a track. `track_id` becomes a child of `parent_id` (or
    /// a top-level track when `parent_id == None`). The graph compiler
    /// rejects the edit (silently keeping the old parent) if it would
    /// produce a cycle.
    SetTrackParent {
        track_id: u32,
        parent_id: Option<u32>,
    },
    RemoveLastTrack,
    /// 選択トラック群の削除 (r.md #43)。 引数は **安定 `Track::id`** の集合
    /// (positional index ではない、 不変条件 1)。 group を含むときは subtree ごと
    /// 再帰削除 (Live 準拠、 `docs/plan_group_track.md` §6)。 1 event = 1 gesture
    /// なので N 本消しても undo は 1 ステップ。 song に居ない id (master row の
    /// `MASTER_TRACK_ID` / subtree 削除で先に消えた子) は黙って無視される。
    DeleteTracks(Vec<u32>),
    /// 選択トラック群を複製する (r.md #30)。`Shared` = クリップ中身 (MIDI ノート /
    /// オーディオ / オートメーション) を元トラックと **リンク** (同じ content_id を
    /// 共有、 片方のノート編集が両方に反映) して重ねる用、 `Unique` = deep clone +
    /// 新 ContentId で **完全独立** コピー。 どちらも device は常に新インスタンス化
    /// される (走行中の plugin instance は共有不可、 state だけコピーして再構築)。
    /// 複製は元トラック (group ならその subtree) の直下に挿入し、 新トラック群を
    /// 選択にする。 クリップ複製の D / Alt+D と同じ二本立て。 選択集合内に含まれない
    /// group child を単独複製した場合は同じ group 内に残る (parent 継承)。
    DuplicateTracksShared(Vec<u32>),
    DuplicateTracksUnique(Vec<u32>),
    MoveTrackUp(u32),
    MoveTrackDown(u32),
    /// 新順での `Track.id` 列で `song.tracks` を並び替える (drag&drop reorder)。
    /// order に含まれない track はそのまま末尾に残す。
    ReorderTracks(Vec<u32>),
    /// 引数は rename 対象 track の **安定 ID** (positional index ではない)。
    BeginRenameTrack(u32),
    RenameTrackChanged(String),
    CommitRenameTrack,
    CancelRenameTrack,
    /// Arranger セクション帯の改名 (track rename の section 版)。帯名ダブルクリック
    /// またはメニュー「改名」で開始、 帯 rect に inline text_input を重ねる。
    BeginRenameSection(u32),
    RenameSectionChanged(String),
    CommitRenameSection,
    CancelRenameSection,
    /// セクション帯の色変更 (color_picker の live drag で発火)。 SetTrackColor と
    /// 同様、 非 undoable で各 arm が snapshot_for_color_edit を呼ぶ。
    SetSectionColor { id: u32, color: [f32; 3] },
    /// clip rename (track rename の clip 版)。 右クリックメニュー "Rename"
    /// または F2 で開始、 該当 clip rect に inline text_input を重ねる。
    BeginRenameClip(ClipRef),
    RenameClipChanged(String),
    CommitRenameClip,
    CancelRenameClip,
    ToggleHelp,
    CloseHelp,
    OpenRecent(PathBuf),
    AutosaveTick,
    /// Recovery modal で「復元」 を押した。 候補 .autosave.daw を読み込み、
    /// candidates から remove + 元 file 削除。 sidecar 復元なら file_path は
    /// 元 .daw、 recovery_dir 復元なら file_path = None (新規プロジェクト扱い)。
    RecoveryRestore(PathBuf),
    /// Recovery modal で「破棄」 を押した。 該当 .autosave.daw を削除 +
    /// candidates から remove。
    RecoveryDiscard(PathBuf),
    /// Recovery modal を閉じる (候補は次回起動時にも見える)。
    RecoveryDismiss,
    MidiNoteOn { pitch: u8, velocity: u8 },
    MidiNoteOff { pitch: u8 },
    /// Phase 7 B1-M Step 1 (2026-05-13): MIDI Control Change (CC)。 MIDI Learn
    /// 経路の入力。 GUI handler で midi_learn_target Some なら新規 binding
    /// 追加、 None なら既存 binding lookup → target に値送信。
    MidiControlChange { channel: u8, controller: u8, value: u8 },
    /// Phase 7 B1-M Step 2 (2026-05-13): MIDI Learn 開始 (= 「次の CC を
    /// この target に bind」 の意思表示)。 transport bar の Learn button で
    /// 発火、 midi_learn_target = Some(target)。
    StartMidiLearn(common::model::BindingTarget),
    /// Phase 7 B1-M Step 2 (2026-05-13): Learn cancel (= midi_learn_target を
    /// None に戻す)。 user が誤って Learn を始めた場合の取り消し用。
    CancelMidiLearn,
    /// Phase 7 B1-M Step 2 (2026-05-13): 既存 MIDI binding の削除。 inspector
    /// の binding list 等から発火 (= 段階 4 で UI 拡張、 段階 2 では未使用)。
    RemoveMidiBinding(usize),
    MidiInputOpened(Option<String>),

    // -------- Bottom panel -------------------------------------------------
    SelectBottomPanel(u8),

    // -------- Arrangement / clip operations -------------------------------
    SelectClip { target: ClipRef, additive: bool },
    SetClipSelection(Vec<ClipRef>),
    /// Ctrl+A (クリップ領域): 曲全体・全トラックの全クリップを選択。
    /// 一括選択なので view ジャンプ (fit_piano_roll / select_track) は
    /// 起こさない。 既に全選択なら冪等。 selection のみ更新で非 undoable。
    SelectAllClips,
    ClearSelection,
    /// 右クリック「共有を一括選択」 — target と同じ `content_id` を持つ
    /// 全 clip (linked clip group) を選択する。 refcount==1 なら自身 1 個。
    /// 共有グループの可視化 / まとめ移動・削除に使う。
    SelectLinkedClips(ClipRef),
    /// Clip の右端 trim (= `start_beat` 同値、 `length_beats` のみ更新) と
    /// 左端 trim (= `start_beat` を進めて `length_beats` を縮める) の両方を
    /// カバー。 audio clip の場合は handler が delta_start を計算して各
    /// `AudioEvent.event_start_in_clip_beats` / `source_start_frames` /
    /// `event_length_beats` を追従させる (Bitwig spec §3.2)。 gui_01
    /// `ResizeClipDelta` の `next_start` / `next_len` 両方をそのまま流す。
    ///
    /// `stretch == false` は **trim** (= 再生範囲を変える。 audio は
    /// source 窓と event 長を lockstep、 MIDI は clip 長で note を gate)、
    /// `stretch == true` は **time-stretch** (= 内容を新長さに伸縮。 audio は
    /// source 窓固定で event 長のみ変更し render が stretch_ratio で warp、 MIDI は
    /// note の start/length を比例 scale)。 Shift + 端 drag で `true` (Ableton 流)。
    ResizeClip {
        target: ClipRef,
        start_beat: f64,
        length: f64,
        stretch: bool,
    },
    /// `(source_ref, to_track_id, next_start_beat)` のタプル列。
    /// to_track_id == source の track id なら同 track 内 move、 違えば
    /// track 跨ぎ move (clip 自体を別 track の `clips: Vec<Clip>` に移す)。
    SetClipPositions(Vec<(ClipRef, u32, f64)>),
    CreateClip { track: u32, start_beat: f64 },
    DeleteSelectedClip,
    /// 選択中の clip 群をまとめて共有コピー (linked clip) する
    /// (D shortcut / `docs/plan_clip_share_clone.md` §3.2)。 選択ブロック全体の
    /// span だけ後ろにずらして相対位置を保ったまま複製し (Ctrl+drag と同じ
    /// セマンティクス)、 複製を選択集合にする。 単一 clip では span = clip 長で
    /// 旧 `DuplicateClipShared` と完全一致。 source の `content_id` を流用。
    DuplicateClipsShared(Vec<ClipRef>),
    /// 選択中の clip 群をまとめて独立コピー (deep clone + 新 ContentId)
    /// する (Alt+D shortcut / §3.3)。 配置・選択は `DuplicateClipsShared` と同じ。
    DuplicateClipsUnique(Vec<ClipRef>),
    /// arrangement Ctrl+drag → release の結果。 各 entry は `(source ClipRef,
    /// to_track_id, drop_start_beat)` (snap 済み)、 元 clip は残し、 drop 位置に
    /// 共有コピー を to_track 上で生成。 (§3.4)
    CloneClipsLinked(Vec<(ClipRef, u32, f64)>),
    /// arrangement Ctrl+Shift+drag → release。 同上だが content は deep clone
    /// + 新 ContentId 採番で独立化。 (§3.5)
    CloneClipsIndependent(Vec<(ClipRef, u32, f64)>),
    /// 右クリック「Make Unique」 — 共有 clip を独立化。 refcount==1 の場合は
    /// no-op (§3.6)。
    MakeClipUnique(ClipRef),

    // -------- Piano roll / note operations --------------------------------
    SelectNote { note: u32, additive: bool },
    ClearNoteSelection,
    AddNote {
        track: u32,
        clip: u32,
        start_beat: f64,
        duration: f64,
        pitch: u8,
    },
    SetNotePositions(Vec<(u32, f64, u8)>),
    SetNoteSelection(Vec<u32>),
    ResizeNote {
        track: u32,
        clip: u32,
        note: u32,
        duration: f64,
    },
    ResizeNotes(Vec<(u32, f64, f64)>),
    DeleteSelectedNotes,
    /// ピアノロールで選択中ノートを複製 (D キー)。選択範囲ぶん後ろにずらして
    /// 複製し、元ノートは据え置き、複製を新しい選択にする (連打で後方へ連鎖)。
    /// selected_clip 無し / 選択空なら no-op。Undoable。
    DuplicateSelectedNotes,
    /// gui_01 #054: piano_roll widget が Ctrl+drag コピー release で発行する
    /// `PianoRollEditRequest::Copy` を変換したもの。各 `(source note id,
    /// new_start_beat, new_pitch)` で source を deep clone し新 note として追加
    /// (元は据え置き)、複製を新選択にする。Undoable。
    CopyNotes(Vec<(u32, f64, u8)>),
    /// gui_01 #017 (M14 Phase 59) で piano_roll widget が L キー → Enter
    /// commit 時に発行する歌詞分配バッチ。 各 `(note_id, lyric)` を指定
    /// `clip_ref` 内で更新。 widget が空文字列を `None` に正規化済みなので
    /// daw_01 側で `is_empty` 判定不要 (None = 歌詞削除)。 1 batch = 1 undo。
    SetNoteLyrics {
        clip_ref: ClipRef,
        lyrics: Vec<(u32, Option<String>)>,
    },
    /// 複数表示ピアノロールの凡例 (legend) で対象 (target) クリップを切り替える。
    /// `selected_clip` (anchor) をこの clip にするだけ (選択集合 `selected_clips` は不変、
    /// `shown_pianoroll_clips` の順序 = packed id slot も不変なので `selected_notes` は維持)。
    /// 新規ノートの所属先・凡例強調がこの clip になる。選択集合に居ない key なら no-op。
    SetPianoRollTargetClip(common::model::ClipKey),
    /// 凡例で **トラック** の「ロック (参照専用)」を反転。ロック中はそのトラックの
    /// 表示 note を widget が hit 除外し、編集 handler も飛ばす (淡色のまま掴めない)。非永続。
    TogglePianoRollTrackLock(u32),

    // -------- Plugin picker / chain ---------------------------------------
    OpenPluginPicker,
    ClosePluginPicker,
    SelectPluginFromDb {
        id: String,
        keep_open: bool,
        open_gui: bool,
    },
    /// プラグインピッカーの検索ボックスが 1 文字毎に発行する。 query を更新し
    /// `refresh_picker_visible` で subsequence 絞り込みを再計算する。
    SetPluginPickerQuery(String),
    /// 検索結果リスト ([`AppData::plugin_picker_visible`]) のカーソルを `delta` だけ
    /// 移動し `[0, visible.len()-1]` で clamp。 visible が空なら no-op。
    /// text_input が focus 中の ↑↓ (gui_01 #057 / Phase 86 `TextInputResponse::nav_up`
    /// / `nav_down`) で発火し、 Enter で `plugin_picker_visible.get(cursor)` を確定する。
    MovePluginPickerCursor(i32),

    // -------- Font picker (Text クリップのフォント選択) ----------
    /// inspector の Font ボタンで発火。anchor の text クリップを対象に取り、
    /// 元フォントを退避してフォントピッカー modal を開く。初回は background で
    /// システムフォントを列挙する。
    OpenFontPicker,
    /// フォントピッカーを閉じる (= cancel)。preview で変えた font を元に戻す。
    /// modal の on_close (Esc / 外クリック / ✕) から発火。
    CloseFontPicker,
    SetFontPickerQuery(String),
    /// 検索リストのカーソルを移動し、移動先フォントをキャンバスにライブ
    /// プレビュー (非 undo)。
    MoveFontPickerCursor(i32),
    /// マウスが乗った行のフォントをライブプレビュー (cursor を合わせる)。
    HoverFontInPicker(usize),
    /// 行を確定 (= click / Enter)。元→選択を 1 undo step にして font を適用し閉じる。
    CommitFontFromPicker(String),
    /// background のフォント列挙完了。
    FontFamiliesLoaded(Vec<String>),

    /// プロジェクトロードの background asset decode が 1 件完了する
    /// たびに発火。 staging を caches へ流し込み、 全件完了で gate を外す。
    AssetDecodeTick,

    /// 再スキャンの VST3 note-effect probe 進捗 (done, total)。
    /// load_overlay に「プラグイン走査中 done/total」を出す。
    RescanProgress { done: usize, total: usize },

    /// 単一デバイスチェーン: `device_index` でアドレスする (役割別 slot 区分撤廃)。
    ToggleSlotGui { index: u32 },
    /// 内蔵映像 FX の param 調整パネルから 1 param を編集。
    /// `value_real` は表示の実レンジ値 → lane の保存値 (0..=1) へ逆写像して格納。
    SetVideoFxParam { device_index: u32, param_id: u32, value_real: f32 },
    /// 埋め込み GUI を持たない plugin の「⚙」インライン param パネルで
    /// param を 1 つ編集。 `value_real` は表示の実レンジ値 → host が送った
    /// `PluginParamInfo` の min/max で lane `default_value` (0..=1) へ逆写像。
    /// scrubable の per-frame 発火なので **非 undoable** (`BeginInspectorScrub`
    /// で 1 undo step に bracket)。
    SetPluginParam { device_index: u32, param_id: u32, value_real: f64 },
    /// inspector の x ボタン: 指定 `device_index` の device を chain から削除。
    RemoveDevice { index: u32 },
    /// inspector 「読み込み失敗」 セクションの「再読込」 ボタン: ロードに
    /// 失敗した device を、 保存済み state 込みで plugin_host に load し直す。
    /// 自動リトライはしない (恒常的失敗で無限ループになる) ので、 再試行の
    /// トリガーは常にこのユーザー操作。 Song は変えない (= 非 undoable)。
    ReloadDevice { track_id: u32, device_index: u32 },
    /// PR4 sidechain: wire / unwire the sidechain source for a plugin's
    /// aux input port. `track_id` + `device_index` identifies the plugin
    /// instance; `port` selects the aux input port on that plugin
    /// (0 = first sidechain bus); `source` is `Some(track_id)` to wire
    /// from a track, or `None` to disconnect.
    SetSidechainSource {
        track_id: u32,
        device_index: u32,
        port: u8,
        source: Option<u32>,
    },
    /// r.md #36: このプラグインのエディタ窓で **キーを一切横取りしない** (= REAPER の
    /// 「Send all keyboard input to plug-in」)。 消化の有無を外に出さない自前描画 GUI
    /// (Dear ImGui / GLFW 系) 用の逃げ道。 値は project に保存される。
    SetPluginSendAllKeys {
        track_id: u32,
        device_index: u32,
        enabled: bool,
    },
    /// パラアウト (docs/plan_paraout.md): one-click "explode" — auto-create a
    /// child track per `is_main=false` output port of the plugin at
    /// `(track_id, device_index)`, group them under the source track, and wire
    /// each aux output to its new child. The source track becomes a
    /// group-with-instrument bus (its own main + the children sum through its
    /// FX/fader). Idempotent: ports already routed to a live track are kept.
    ExplodeParallelOut {
        track_id: u32,
        device_index: u32,
    },
    /// パラアウト: route a single aux output port to a destination track (or
    /// `None` = unrouted = silent). Used by the inspector's per-port dropdown
    /// for re-adjustment after (or instead of) explode.
    SetParallelOutputRoute {
        track_id: u32,
        device_index: u32,
        port: u8,
        dest: Option<u32>,
    },
    /// docs/plan_modulation.md §9: create a project-level `ModSource`
    /// of the given kind, owned by the cursor track. follower は cursor track を tap。
    AddModSource { kind: ModSourceKindTag },
    /// remove the `ModSource` with id `id` and every `ModRouting` referencing it.
    RemoveModSource { id: u32 },
    /// generator (LFO/Random/MSEG/Steps) 設定の編集 (consolidated)。
    EditModSource { id: u32, edit: ModSourceEdit },
    /// **lane 非依存** (`docs/plan_modulation_routing_redesign.md` §5): add a
    /// `ModRouting` on track `track_id` (`MASTER_TRACK_ID` → `song_mod_routings`)
    /// targeting `target`, driven by `ModSource` `source_id`. No-op if a routing
    /// for the same `(target, source_id)` already exists.
    AddModRouting {
        track_id: u32,
        target: common::model::AutomationTarget,
        source_id: u32,
    },
    RemoveModRouting {
        track_id: u32,
        target: common::model::AutomationTarget,
        source_id: u32,
    },
    /// set a routing's modulation depth (normalized-domain amount, clamped
    /// to `-1..=1`).
    SetModRoutingDepth {
        track_id: u32,
        target: common::model::AutomationTarget,
        source_id: u32,
        depth: f32,
    },
    /// toggle a routing's polarity (`true` = Bipolar, `false` = Unipolar).
    SetModRoutingPolarity {
        track_id: u32,
        target: common::model::AutomationTarget,
        source_id: u32,
        bipolar: bool,
    },
    /// docs/plan_modulation.md §9: change which track a `ModSource` follows.
    SetModSourceTrack { id: u32, source_track: u32 },
    /// docs/plan_modulation.md §3: envelope follower attack / release (ms).
    /// During a scrub drag these only mark dirty (no per-frame recompile); the
    /// engine recompiles the baked coefficients once on drag-end (see
    /// `SetModFollowerScrubbing`).
    SetModSourceAttack { id: u32, ms: f32 },
    SetModSourceRelease { id: u32, ms: f32 },
    /// docs/plan_modulation.md §3: follower attack/release scrub drag edge.
    /// `false` after `true` = drag-end → recompile follower coefficients once
    /// (`flush_song_sync`). Avoids a per-frame LoadSong storm.
    SetModFollowerScrubbing(bool),
    /// docs/plan_modulation.md §6: flip a `ModSource`'s tap point
    /// (`true` = PostFader, `false` = PostFx / pre-fader).
    SetModSourceTapPoint { id: u32, tap_point: common::model::TapPoint },
    /// docs/plan_modulation_routing_redesign.md §6: arm / disarm a `ModSource`
    /// for per-control depth assignment (Bitwig 流). `Some(id)` arms; `None`
    /// disarms. While armed, inspector param controls enter depth-drag edit mode.
    SetArmedModSource(Option<u32>),
    /// flip an aux-input route's tap point (sidechain plugin input).
    SetAuxInputTapPoint {
        track_id: u32,
        device_index: u32,
        port: u8,
        tap_point: common::model::TapPoint,
    },
    /// inspector chain (= `Track.devices` / `master_fx_chain` を一列にした list)
    /// の reorder。`order` は gui_01 契約 `new[i] = items[order[i]]`。単一デバイス
    /// チェーン化で **棄却なしの純 permutation** (役割は位置から再導出)。
    ReorderInspectorChain(Vec<usize>),
    SetMasterGain(f32),

    // -------- IPC events from plugin_host ---------------------------------
    Tick { samples: u64, peak_l: f32, peak_r: f32, preroll: u64 },
    RescanPluginDb,
    PluginDbRescanCompleted,

    // -------- Scroll / zoom -----------------------------------------------
    SetArrangeScroll(f32),
    SetArrangeZoom(f32),
    /// 再生追従スクロールの方式を `Off → Scroll → Page → Off` と循環
    /// (`Alt+F` / トランスポートの追従ボタン、クリックごとに切替)。
    CycleArrangeFollow,
    SetArrangeTrackRowH(f32),
    /// arrangement の track header 幅を更新 (gui_01 widget の右端
    /// splitter drag が発火)。 handler 側で 80..480 px に clamp。 session-only。
    SetArrangeHeaderW(f32),
    SetPianoRollScrollX(f32),
    SetPianoRollTopPitch(u8),
    SetPianoRollZoomX(f32),
    SetPianoRollZoomY(f32),
    SetLoopRange { start: f64, end: f64 },

    // -------- Grid snap ---------------------------------------------------
    SetPianoRollSnapEnabled(bool),
    SetPianoRollSnapChoice(u8),
    SetArrangeSnapEnabled(bool),
    SetArrangeSnapChoice(u8),
    TogglePianoRollSnap,
    ToggleArrangeSnap,
    /// `1` キー (Ableton Live "Narrow Grid" 互換): snap unit を 1 段細かく。
    NarrowPianoRollGrid,
    NarrowArrangeGrid,
    /// `2` キー (Widen Grid): snap unit を 1 段粗く。
    WidenPianoRollGrid,
    WidenArrangeGrid,
    /// `3` キー (Toggle Triplet): Straight ↔ Triplet (div は維持)。
    TogglePianoRollTriplet,
    ToggleArrangeTriplet,
    /// `X` キー / "Fit" ボタン / SelectClip 経由の auto-fit zoom。
    /// piano_roll は selected_clip のノート bbox に、arrangement は全 clip に fit。
    FitPianoRollToClip,
    FitArrangeToContent,
    /// `Z` キー: 選択素材への段階ズーム。 1 回目で横ズーム、 2 回目で縦ズーム
    /// (automation clip ならレーンを viewport 高いっぱいに拡大、 通常 clip なら
    /// その track 群を viewport に収める)、 3 回目以降は no-op。 選択 / view 変化で
    /// 段階を仕切り直す。 `automation` は対象面 (通常 clip / automation clip) を root
    /// の `edit_surface` arbiter が解決した結果 (= Del/Cut と同じ last-selection-wins、
    /// 「MIDI clip を選んでも残存 automation 選択へズームしてしまう」 を防ぐ)。
    ZoomArrangeToSelectedClip { automation: bool },
    /// `X` キー (arrangement): ズーム履歴を 1 段戻す。 履歴が空なら全体フィット
    /// (`fit_arrange_to_content`)。 piano roll 側の `X` は引き続き
    /// `FitPianoRollToClip`。
    ArrangeZoomBack,

    // -------- Mixer -------------------------------------------------------
    SetTrackVolume { track: u32, amp: f32 },
    SetTrackPan { track: u32, pan: f32 },
    /// v18 (`docs/plan_track_clip_color.md`): track の表示色を設定。
    /// `color == None` で id 由来の導出パレット色 (auto) に戻す。音響的な
    /// 意味はなく model field のみ更新 (= audio engine への送信不要)。Undo 対象。
    SetTrackColor { track: u32, color: Option<[f32; 3]> },
    /// v18 (`docs/plan_track_clip_color.md`): Ableton 流に、track の全 clip の
    /// 色上書き (`Clip.color`) を外して track 色継承に戻す (= 一括 reset)。
    /// track 自身の color は変えない。track header context menu から発火。Undo 対象。
    ResetTrackClipColors { track: u32 },
    ToggleTrackMute(u32),
    ToggleTrackSolo(u32),
    /// Phase 7 B4 (2026-05-13): track Record-arm を toggle。 業界標準どおり
    /// caller 側で前状態を反転、 audio engine には `AudioCommand::SetTrackArmed`
    /// で確定値を送る。 session-only / Undo 対象外 (= 業界標準は arm を Undo
    /// 履歴に積まない、 mute / solo と同 idiom)。
    ToggleTrackArmed(u32),
    TrackPeaksTick(Vec<(f32, f32)>),
    /// docs/plan_modulation.md §4.2: latest per-`ModSource` envelope follower
    /// scalars (indexed by `ModSource` position), polled ~30Hz from
    /// `AudioBridge::mod_scalars`. Drives visual modulation each frame.
    ModScalarsTick(Vec<f32>),
    /// resource monitor (r.md #3): poller が ~30Hz で読む全体メトリクス
    /// (DSP load peak/avg、 xrun 累積、 buffer 長 / sample rate)。
    MetricsTick {
        dsp_load_peak: f32,
        dsp_load_avg: f32,
        xrun_count: u64,
        buffer_frames: u32,
        sample_rate: u32,
    },
    /// resource monitor (r.md #3): sysinfo スレッドが ~1Hz で読む system 指標
    /// (daw_01 3 プロセス合計の CPU% と常駐メモリ MB)。
    SystemMetricsTick { cpu: f32, mem_mb: f32 },
    /// resource monitor (r.md #3): status bar 常駐メーターの表示 on/off を
    /// トグルし app_config.json に保存 (View メニュー / ショートカット)。
    ToggleResourceMonitor,
    /// resource monitor (r.md #3): 詳細パネルの開閉トグル (status bar クリック /
    /// Esc / ショートカット)。
    ToggleResourcePanel,

    // -------- Aux send / return ------------------------------------------
    /// master 直下 (`parent_group_id = None`) の通常 track を 1 本作り
    /// `"Return N"` と命名する (N = 既存リターン数 + 1)。 track が選択中なら
    /// その track に `Send { dest = 新リターン, gain 1.0, PostFader, enabled }`
    /// を 1 本足して即座に効果が聞こえるようにする (Ableton "Add Return")。
    /// 構造変化なので full-song resend を trigger する。
    AddReturnTrack,
    /// `src_track_id` の `sends` に `dest_track_id` 宛ての send を 1 本追加。
    /// gain 1.0 / PostFader / enabled。 構造変化 → full-song resend。
    AddSend { src_track_id: u32, dest_track_id: u32 },
    /// `track_id` の `sends[send_idx]` を削除。 構造変化 → full-song resend。
    /// (後続 send の index がずれるが、 resend で schedule が再 compile
    /// されるため問題ない。 automation lane の reindex は本タスク対象外。)
    RemoveSend { track_id: u32, send_idx: usize },
    /// `track_id` の `sends[send_idx].mode` を設定。 tap 位置 (pre/post)
    /// は routing graph に影響するので 構造変化 → full-song resend。
    SetSendMode { track_id: u32, send_idx: usize, mode: SendMode },
    /// `track_id` の `sends[send_idx].gain` を設定 (clamp 0..2) + realtime
    /// `AudioCommand::SetSendGain` を送る。 SetTrackVolume と同 idiom、
    /// full-song resend しない (= drag 中の高頻度更新)。
    SetSendGain { track_id: u32, send_idx: usize, gain: f32 },
    /// `track_id` の `sends[send_idx].enabled` を設定 + realtime
    /// `AudioCommand::SetSendEnabled` を送る。 full-song resend しない。
    SetSendEnabled { track_id: u32, send_idx: usize, enabled: bool },
    /// 宛先トラックピッカーを開く (= send 元 = `src_track_id`)。
    OpenSendPicker { src_track_id: u32 },
    /// 宛先トラックピッカーを閉じる。
    CloseSendPicker,

    // -------- VOICEVOX ----------------------------------------------------
    // PR-V4: SynthesizeVocal / VocalSynthCompleted は削除済 (builtin
    // VOICEVOX plugin 経由で自動 synth)。
    /// VOICEVOX engine `/singers` の取得結果。 起動時 background thread が
    /// 1 度発行する。 失敗時は空 Vec で送る。
    SingersLoaded(Vec<crate::voicevox_client::VoiceVoxSinger>),
    /// 口パク (lip-sync) 背景ジョブ完了。`regenerate_lipsync_for_track` が
    /// spawn したスレッドが `query_phonemes` の結果を vocal clip 単位で詰めて
    /// 発行し、handler (`apply_lipsync_generated`) が口 track へ反映する。
    /// 派生データなので Undo 対象外 (= `is_undoable` に入れない)。
    /// `generation` は spawn 時点の `lipsync_gen` snapshot。 HTTP 完了が遅延して
    /// いる間に別 project を開く (= `reset_saved_baseline` が gen を bump) と、
    /// この古い結果を別 project に適用して spurious dirty を生むため、 handler は
    /// `generation == lipsync_gen` のときだけ反映する (debounce
    /// leg `LipsyncDebounceFired` と対称)。
    /// 成功/失敗/空に関わらず **常に** 発行する。handler は generation に
    /// 関わらず `target_track_id` を `lipsync_inflight` から外し (= クリップ上スピナー /
    /// 全体オーバーレイの「口パク生成中」を解除)、その後 generation 一致 & `clips` 非空の
    /// ときだけ口 track へ反映する。`target_track_id` は spawn 時に解決済の出力先 track id。
    LipsyncGenerated {
        vocal_track_id: u32,
        target_track_id: u32,
        bpm: f32,
        clips: Vec<LipsyncClipResult>,
        generation: u64,
    },
    /// Track Inspector: vocal track の口パク出力先 (口 track id) を設定。
    /// `None` で解除。設定後に口パクを再生成する。
    SetLipsyncTarget { track: u32, target: Option<u32> },
    /// Track Inspector: 口 track の `mouth_map` の 1 slot (口形状 →
    /// ImageSourceId) を設定。`0` で解除。設定後、この口 track を出力先に
    /// している vocal track の口パクを再生成する。
    SetMouthMapSlot {
        track: u32,
        shape: common::model::MouthShape,
        source_id: common::model::ImageSourceId,
    },
    /// 口パク自動再生成 debounce timer の発火。`mark_lipsync_dirty` が
    /// 立てた timer thread が送る。`lipsync_gen` と一致するときだけ
    /// (= それ以降変更なし) 全 bound vocal track を再生成する。Undo 対象外。
    LipsyncDebounceFired(u64),
    /// Clip Inspector の 2 段 dropdown で選択された声を、 対象
    /// clip (stable `ClipKey`) に焼き込む。 builtin へ再 flush して新しい声で
    /// 再合成する。
    SetClipVoice {
        clip: common::model::ClipKey,
        speaker_id: u32,
        singer_name: String,
        style_name: String,
    },
    /// Clip Inspector の「再取得」ボタン。 VOICEVOX `/singers` を
    /// 再取得して声 dropdown を更新する (新規キャラ導入時)。
    RefetchSingers,
    /// (talk) VOICEVOX engine `/speakers` の取得結果 (`docs/plan_voicevox_talk.md` §4)。
    /// 起動時 background thread が 1 度発行。失敗時は空 Vec。
    SpeakersLoaded(Vec<crate::voicevox_client::VoiceVoxSinger>),
    /// (talk) Text clip Inspector の「再取得」ボタン。`/speakers` を再取得する。
    RefetchSpeakers,
    /// (talk) Text clip Inspector の talk スケール (話速/音高/抑揚/音量) を 1 つ
    /// 編集する。対象 clip (stable `ClipKey`) の `Clip::talk` に焼き込んで builtin へ
    /// 再 flush (= 新しいスケールで再合成)。
    SetClipTalkParam {
        clip: common::model::ClipKey,
        param: TalkParamKind,
        value: f32,
    },

    // -------- WAV export -------------------------------------------------
    /// File → Export WAV...: open the range picker (default窓 = 全曲)。
    /// 確定で `ConfirmExportRange` → file dialog → freewheel render。
    ExportWav,

    // -------- Export range picker ----------------------------
    /// レンジピッカーの開始拍を更新 (scrubable_number から)。 end 未満 / 0 以上に
    /// clamp。
    SetExportRangeStart(f64),
    /// レンジピッカーの終了拍を更新 (scrubable_number から)。 start 超 / song 長
    /// 以下に clamp。
    SetExportRangeEnd(f64),
    /// レンジピッカーを「全曲」 (start=0, end=length_beats) に戻す。
    ResetExportRange,
    /// video export の出力解像度 `(width, height)` を更新 (dropdown
    /// から)。 picker が開いている間だけ有効。 per-export override で Song /
    /// preview には反映しない。
    SetExportResolution(u32, u32),
    /// video export の出力フレームレートを更新 (dropdown から)。
    SetExportFramerate(f32),
    /// レンジピッカーを確定し、 `kind` に応じた export action (file dialog) を
    /// 起動する。 picker は閉じる。
    ConfirmExportRange,
    /// レンジピッカーを破棄して export を中止する。
    CancelExportRange,
    /// Phase 7 B4 Step E (2026-05-13): MIDI export menu trigger。 rfd で
    /// path 取得 → `midi_export::export_midi(&song, &path)` で SMF1 書き出し。
    /// 失敗時は status_message に error を出すのみ (= モーダル無し)。
    ExportMidi,

    // -------- Audio clip import (Phase 1 PR3) ----------------------------
    /// Import one or more audio files into the song. Triggered by
    /// `arrangement` drag&drop and the File → Import Audio menu (PR3).
    /// The handler decodes each file (Phase 1: synchronous + WAV-only,
    /// `docs/plan_audio_clip.md` §7), copies it into
    /// `<project_dir>/samples/<basename>_<hash>.<ext>` (or the unsaved-
    /// project import_cache as fallback), registers an `AudioSource`,
    /// stashes the decoded buffer in `audio_source_cache`, and creates
    /// an audio clip on the first track at the current playhead.
    /// Phase 2 moves decode to a background thread so large WAVs (up
    /// to 4 GB §7.2) don't block the UI.
    ImportAudio {
        paths: Vec<PathBuf>,
        /// クリップを置く track の決定方法 (arrangement drop / dialog の起点)。
        /// `Track(idx)` = drop が乗った既存 track、`NewTrackBottom` = track の
        /// 無い下の余白 drop → 一番下に新規 track (r.md #31)、`NoHint` = File
        /// menu / dialog (位置情報なし) → cursor track fallback。
        target: ImportTrackTarget,
        /// drag&drop の drop X 位置から計算した beat (snap 済み)。 生成する
        /// clip をこの beat に置く (= ドロップしたカーソル位置に貼る)。 `None`
        /// なら handler 側で playhead にフォールバック (= dialog / File menu 経由)。
        target_beat: Option<f64>,
    },

    /// File menu → "Import Audio..." entry. Opens an `rfd` file picker
    /// (multi-select, WAV filter), then forwards the chosen paths to
    /// `AppEvent::ImportAudio`. The dialog itself is `rfd`'s native
    /// modal so we don't need our own ui state. `docs/plan_audio_clip.md`
    /// §3.1 — File menu からの import 経路。
    OpenImportAudioDialog,

    /// Video file import (`docs/plan_video.md` P2). For each path:
    /// copies the video into `<project_dir>/samples/<hash>.<ext>`,
    /// extracts the audio stream to a paired `.wav` via WMF,
    /// registers a `VideoSource` and (when present) the paired
    /// `AudioSource`, and appends a new video track + paired audio
    /// track to `Song.tracks` with one clip each starting at the
    /// playhead. Runs synchronously on the GUI thread — typical MV
    /// clips finish in 1-3s, slow imports leave the user with a
    /// momentary stall instead of a complex completion dispatch.
    ImportVideo {
        paths: Vec<PathBuf>,
        /// drag&drop の drop X 位置から計算した beat (snap 済み)。 生成する
        /// video / 対 audio clip をこの beat に置く。 `None` なら playhead に
        /// フォールバック (= dialog / File menu / smoke test 経由)。
        target_beat: Option<f64>,
    },
    /// v13 (`docs/plan_image_overlay.md` §P2): import one or more
    /// image files (PNG / JPEG / WebP / static), allocating an
    /// `ImageSource` per file and a Clip holding the image as a PiP
    /// overlay. `target`: 既存 track を指していればその track に貼り付け、
    /// track の無い下の余白 drop / dialog 経由なら arrangement の
    /// 一番下に新規 track を作って貼る (r.md #31)。
    /// Image clips default to aspect-fit PiP; the user shrinks /
    /// positions them in P5 drag handle UI or P4 inspector.
    ImportImage {
        paths: Vec<PathBuf>,
        target: ImportTrackTarget,
        /// drag&drop の drop X 位置から計算した beat (snap 済み)。 生成する
        /// image clip をこの beat に置く。 `None` なら playhead / beat 0 に
        /// フォールバック (= dialog / File menu 経由)。
        target_beat: Option<f64>,
    },
    /// v13: open the platform file dialog filtered to supported image
    /// extensions and dispatch the selection as `AppEvent::ImportImage`.
    OpenImportImageDialog,

    /// docs/plan_text_clip_creation.md: 空きレーン右クリック → "Text クリップ" で
    /// `track` (track id) の `start_beat` 位置に `ClipContent::Text` clip を 1 個追加
    /// する。clip は単一 `TextEvent` を default 体裁 (= center band, 64 px white font)
    /// で持つ。text 内容 / styles は inspector、PiP rect は preview drag で編集。
    /// (旧 `AddTextClip` = File menu で新規 track を先頭に作る版は廃止。text トラックは
    /// v16 で他トラックと統一済みのため、他 clip と同じくタイムライン上で生成する。)
    AddTextClipAt { track: u32, start_beat: f64 },

    /// File menu → "Import Video..." entry. Opens an `rfd` file
    /// picker (mp4 / mov / mkv / webm filter) and forwards to
    /// `AppEvent::ImportVideo`.
    OpenImportVideoDialog,

    /// Toggle the video preview window's visibility (`docs/plan_video.md`
    /// P4). When `AppData.preview_window_visible` flips to `true` the
    /// runner creates a second `winit::Window` + `Renderer` pair; flipping
    /// back to `false` (= user closed the window or re-toggled the menu)
    /// destroys it. P4 only opens an empty placeholder window — actual
    /// video frame composite arrives in P5/P7.
    TogglePreviewWindow,

    /// File menu → "Export Video..." (`docs/plan_video.md` P8). Opens
    /// a save dialog for the output mp4 path + a second open dialog
    /// (cancelable) for an optional audio WAV to mux in. Forwards to
    /// `AppEvent::ExportMp4` once the user picks paths.
    OpenExportMp4Dialog,

    /// 別スレッドで開いた native file dialog の結果。 `kind` で振り分け、 `paths`
    /// は選択された path 群 (空 = キャンセル)。 dialog を GUI スレッドで同期に開くと
    /// preview window 等の再描画 flood で modal pump が枯れてフリーズするため、 全
    /// native file dialog をこの経路 (別スレッド + owner-modal) に統一している。
    FileDialogResult {
        kind: FileDialogKind,
        paths: Vec<PathBuf>,
    },
    /// `action_save_as` が別スレッドで解決した最終保存先 (.daw のフルパス)。 save
    /// dialog + 上書き確認 (MessageDialog) を worker thread で済ませ、 `Some(path)`
    /// で確定、 `None` でキャンセル / 上書き拒否。 GUI スレッドで `create_dir_all` +
    /// `begin_save` を行う。
    SaveAsResolved {
        path: Option<PathBuf>,
    },

    /// Background mp4 render at `output_path`, optionally muxing the
    /// PCM Float32 WAV at `audio_wav` as an AAC stream. v12
    /// (`docs/plan_video.md` P8). `range_beats` restricts the
    /// rendered window to `[start_beat, end_beat)` (`None` = whole song);
    /// the muxed `audio_wav` is already trimmed to the same window.
    /// `dims` = picker で選んだ出力解像度 `(w, h)` と fps の
    /// per-export override (`None` = プロジェクト値)。
    ExportMp4 {
        output_path: PathBuf,
        audio_wav: Option<PathBuf>,
        range_beats: Option<(f64, f64)>,
        dims: Option<((u32, u32), f32)>,
    },
    /// 映像 render thread が発火（`done` / `total` フレーム）。`export_stage` を
    /// `VideoRender` に更新して進捗オーバーレイに反映。非 undoable。
    ExportProgress { done: u64, total: u64 },
    /// 映像 render thread の完了通知（成功時は出力 path、失敗 /
    /// キャンセル時は理由）。`export_stage` / `export_cancel` をクリアして
    /// status_message に結果を出す。非 undoable。
    ExportFinished {
        result: Result<PathBuf, String>,
    },
    /// 進捗オーバーレイの Cancel ボタン → 実行中 export の `export_cancel`
    /// フラグを立てる（render loop が次フレームで中断）。非 undoable。
    CancelExport,

    // -------- Split / Glue (Phase 1 PR7) -----------------------------------
    /// Split clip(s) at the **mouse cursor** (= `AppData
    /// .arrangement_hover_beat` snapped, or `_raw` when `snap == false`
    /// for the Alt+E variant). Falls back to the playhead when the
    /// cursor is outside the arrangement canvas. Operates on the clip
    /// the cursor is hovering over; if there is no hovered clip,
    /// falls back to `selected_clips`. Works on MIDI / Audio / Vocal
    /// clips alike (`docs/plan_audio_clip.md` §3.3.1): the back half
    /// gets a freshly-allocated `ContentId` and `notes` / `events` are
    /// partitioned by the split beat. Bound to `E` (snap on) and
    /// `Alt+E` (snap off).
    SplitClipAtPlayhead { snap: bool },

    /// Glue (Consolidate) the currently selected clips into a single
    /// clip per track. All clips must be the same kind (MIDI / Audio
    /// / Vocal) — mixed-kind selections are rejected with a status
    /// message (§3.3.2). Result clip spans `min(start_beat) .. max(end
    /// _beat)` and inherits a fresh `ContentId` carrying every event /
    /// note from the source clips with offsets re-aligned to the new
    /// clip start. Gaps between clips become silent ranges. Bound to `J`.
    GlueSelectedClips,

    // -------- Audio event field edits (Phase 2 PR1) ------------------------
    /// Toggle `AudioEvent.reversed` for every event in the selected
    /// audio clip. Non-audio clips no-op. `docs/plan_audio_clip.md`
    /// §3.8: Reverse は destructive ではなく、 再生時に source を逆方向
    /// 走査する flag。
    SetClipReversed { target: ClipRef, reversed: bool },

    /// clip 全体の mute を設定 (`Clip.muted` = clip-level mute の SSoT)。
    /// 旧 (v26 以前) は per-event `AudioEvent.muted` を立てていたが、v27 で `Clip.muted` に
    /// 一本化したので audio inspector の "Mute" トグルもここを設定する。track-mute とは独立。
    SetClipMuted { target: ClipRef, muted: bool },

    /// 複数 clip の mute を一括設定 (= `q` ショートカットで選択 clip / カーソル
    /// 直下 clip を toggle した結果)。各 target の `Clip.muted` に `muted` を設定する。
    SetClipsMuted { targets: Vec<ClipRef>, muted: bool },

    /// 表示中ピアノロールの note 群 (**packed note id**) の `Note.muted` を
    /// 一括設定 (= `q` で選択 note / カーソル直下 note を toggle した結果)。packed id は
    /// 所属クリップを内包するので複数クリップに跨る選択も正しく mute できる。linked clip は
    /// content 共有なので mute も共有される。
    SetNotesMuted {
        notes: Vec<u32>,
        muted: bool,
    },

    /// v18 (`docs/plan_track_clip_color.md`): clip の表示色を設定。
    /// `color == None` でトラック色継承に戻す (Ableton "match track color")。
    /// model field のみ更新。Undo 対象。
    SetClipColor { target: ClipRef, color: Option<[f32; 3]> },

    /// Set `AudioEvent.stretch_mode` for every event in the selected
    /// audio clip. Phase 1 で再生に効くのは `Raw` / `Repitch` のみ;
    /// `Stretch` / `Slice` は §3.7 に従って Raw 同等で再生される
    /// (Phase 3+ で本実装)。
    SetClipStretchMode { target: ClipRef, mode: common::model::StretchMode },

    // ---- Audio event 数値 field 編集 (Phase 2 PR2) ----------------------
    /// audio / image inspector が `clip_edit_buffer_target` を
    /// `target` に同期するために発火する純 sync marker。 数値 field は
    /// scrubable_number 化され現値を summary から直接読むため buffer 再生成
    /// は不要だが、 text section と共有する `clip_edit_buffer_target` を
    /// 正しい clip に向けておくために残す。 `is_undoable` ではない。
    ResyncClipEditBuffers(ClipRef),

    /// scrubable_number の on_change が発火する programmatic な
    /// field 設定 (drag 中 per-frame / text commit)。 全 event に broadcast
    /// (`SetClipReversed` 等と同じ semantics)。 非 undoable (= drag stroke
    /// を `Begin/EndInspectorScrub` で 1 undo step に bracket)。
    SetClipGainDb { target: ClipRef, gain_db: f32 },
    SetClipPan { target: ClipRef, pan: f32 },
    SetClipPitchSemitones { target: ClipRef, semitones: f32 },
    /// r.md #40: スペクトル包絡 (フォルマント) の移調量。 音程とは独立に
    /// 「声質」 だけを動かす。
    SetClipFormantSemitones { target: ClipRef, semitones: f32 },

    // ---- Audio event fade 編集 (Phase 2 PR3) ----------------------------
    /// Fade length / curve の programmatic 設定。 `SetClipGainDb` 等と
    /// 同じ semantics で全 event に broadcast、 値は clip.length_beats
    /// で clamp (= fade が clip より長くならない)。 curve は spec §3.5
    /// の Linear / Exponential / SCurve から選択 (Inspector dropdown 経由)。
    /// `target` の `ClipContent` が `Audio` / `Image` のいずれであっても
    /// fade フィールドが存在するので kind-aware に書き分ける (handler
    /// 側で resolve)。
    SetClipFadeInBeats { target: ClipRef, beats: f64 },
    SetClipFadeOutBeats { target: ClipRef, beats: f64 },
    SetClipFadeInCurve { target: ClipRef, curve: common::model::FadeCurve },
    SetClipFadeOutCurve { target: ClipRef, curve: common::model::FadeCurve },

    // ---- Image event 編集 (`docs/plan_image_overlay.md` §4 P4) -----------
    /// PiP rect / opacity / rotation の programmatic 設定 (Inspector の
    /// scrubable_number on_change から / preview drag handle / JS test API
    /// 経由)。 全 ImageEvent に broadcast。 各値は仕様に従って clamp:
    /// x/y/w/h は [0.0, 1.0]、 opacity も [0.0, 1.0]、 rotation は
    /// `-π..=π` で wrap (= 360° 連続入力可)。 inspector の
    /// scrubable 化で `ClipImage*EditChanged` / `CommitClipImage*Edit` は
    /// 撤去 (drag stroke を `Begin/EndInspectorScrub` で bracket)。
    SetClipImageX { target: ClipRef, value: f32 },
    SetClipImageY { target: ClipRef, value: f32 },
    SetClipImageW { target: ClipRef, value: f32 },
    SetClipImageH { target: ClipRef, value: f32 },
    SetClipImageOpacity { target: ClipRef, value: f32 },
    /// `value` は radians 単位 (= 内部単位)。 inspector は degree で
    /// 入力するが commit で radians に変換してから発火する。
    SetClipImageRotation { target: ClipRef, value: f32 },

    // ---- Auto-Fade / Auto-Crossfade (Phase 2 PR5) -----------------------
    /// 全選択 audio clip に短 (≒4 ms 相当) fade を一括適用 (`docs
    /// /plan_audio_clip.md` §3.5)。 既存 fade 値は上書き。 fade 長は
    /// `0.004 * bpm / 60` beats = 4 ms 相当 (業界標準のクリック除去
    /// 用 short fade)。
    AutoFadeSelectedClips,

    /// 隣接 audio clip 間で重なり区間に crossfade を作成 (= 前 clip の
    /// 末尾 fade_out + 次 clip の先頭 fade_in を overlap 長で揃える、
    /// `docs/plan_audio_clip.md` §3.5)。 同 track 内の clip 群を
    /// start_beat 順に sort し、 ペアごとに `prev.start + prev.length >
    /// next.start` を判定 → overlap_beats を両 fade に設定。 隙間がある
    /// (= overlap が無い) ペアは no-op。
    AutoCrossfadeSelectedClips,

    // ---- Audio Editor (Phase 2 PR6, `docs/plan_audio_clip.md` §3.10) ---
    /// audio clip ダブルクリックで Audio Editor を開く。
    /// `audio_editor_clip = Some(target)` + bottom_panel を tab 1
    /// (Piano Roll 切替先) に切り替え。 ClipContent::Audio 以外を渡された
    /// 場合は no-op (status_message 出さず silent skip)。
    OpenAudioEditor(ClipRef),

    /// Audio Editor を閉じる (Esc shortcut / 切替操作経由)。
    /// `audio_editor_clip = None` に戻して bottom_panel は現在のタブ
    /// (Piano Roll) を維持。
    CloseAudioEditor,

    /// `target` clip の first event の `reversed` を反転 (= 右クリック
    /// メニュー「Reverse」 用 toggle、 `docs/plan_audio_clip.md` §3.8)。
    /// Inspector でも同 field は編集できるが、 メニューから 1 操作で
    /// 切り替えられる UX を提供。 内部的には現値を読んで
    /// `SetClipReversed` を呼ぶのと等価で、 全 event に broadcast。
    ToggleClipReversed(ClipRef),

    /// Bounce In Place (Pre-FX、 `docs/plan_audio_clip.md` §3.8)。
    /// `target` clip 内の全 events を offline mix して 1 つの WAV
    /// (stereo 32-bit float) に書き出し、 新 `AudioSource` を採番して
    /// Song.audio_sources に追加、 `ClipContent::Audio { events: [新
    /// 1 event] }` に置換する。 Pre-FX = plugin chain (instrument /
    /// fx_chain) を通さない、 source の events を mix しただけの
    /// snapshot。 同 ContentId を共有していた linked clip も同じ新
    /// content に置換される (= 既存 ContentId を上書き)。
    BounceClipInPlace(ClipRef),

    // ---- Bounce (with FX) — Phase 2 PR-C --------------------------------
    /// audio clip を **plugin chain 込み** で render し、 結果を **新 track**
    /// に新 audio clip として配置 (`docs/plan_audio_followup.md` PR-C)。
    /// async (= IPC freewheel render → AudioEvent::BounceClipFxComplete)。
    /// `is_undoable` には入れず、 完了通知 handler 内で
    /// `push_undo_snapshot` を明示呼び出し (= 1 完了 = 1 Undo step)。
    BounceClipWithFx(ClipRef),

    // ---- multi-clip drag batch (Phase 2 PR-B) ---------------------------
    /// gui_01 widget が multi-clip 一括 drag (= dB / fade / curve) を 1
    /// release で発行する場合、 各 delta を 1 AppEvent にまとめて 1
    /// Undo step とする。 delta 数だけ単発 AppEvent を撃つと Undo step
    /// が分散してしまう (Phase 2 PR-B、 `docs/plan_audio_followup.md` §PR-B)。
    /// 単発 `SetClipGainDb` 等は Inspector commit 経路で引き続き使用。
    SetClipGainDbBatch(Vec<(ClipRef, f32)>),
    /// `(target, edge, beats)` 列で fade length を一括設定。
    /// r.md #38: 宛先は clip ではなく **clip 内の 1 event** ([`ClipEventRef`])。
    /// content 種別 (audio / video / image / text) は handler が clip から解決する。
    SetClipFadeBeatsBatch(Vec<(ClipEventRef, FadeEdgeKind, f64)>),
    /// `(target, edge, curve)` 列で fade curve を一括設定。
    SetClipFadeCurveBatch(Vec<(ClipEventRef, FadeEdgeKind, common::model::FadeCurve)>),
    /// inspector のトグル / ドロップダウン (= discrete undoable 編集) を
    /// 複数選択クリップへ一括適用する。 単発イベントをループで撃つと is_undoable の
    /// auto-push で N スナップになるため、 これ 1 つで 1 スナップにまとめ、 handler 内で
    /// per-clip setter (variant-safe) をループする。
    BroadcastDiscreteClipEdit {
        targets: Vec<ClipRef>,
        edit: DiscreteClipEdit,
    },

    // ---- Audio Editor scroll / zoom -----------------------------------
    /// Audio Editor の `view_start_beat` を変更 (= 水平 scroll)。
    /// 0 ≤ start ≤ clip.length_beats - view_len_beats で clamp、
    /// `audio_editor_clip` が None なら no-op。 view state なので非 undoable。
    SetAudioEditorScroll(f64),
    /// Audio Editor の `view_start_beat` / `view_len_beats` を一括変更
    /// (= zoom anchor 保持のため start/len 同時更新)。 view_len は
    /// `MIN_AUDIO_EDITOR_VIEW_LEN_BEATS` 以上 + clip.length_beats 以下、
    /// view_start も clamp。 `audio_editor_clip` が None なら no-op。
    SetAudioEditorZoom { view_start_beat: f64, view_len_beats: f64 },

    // ---- Audio Editor event 単位編集 (Phase 2 PR-D 段階 1) -----------
    /// Audio Editor 内で event index を選択 (= clip 内 events Vec の
    /// index)。 `None` で選択解除。 `audio_editor_clip` が `None` の
    /// ときは no-op。 view state なので非 undoable。
    SelectAudioEditorEvent(Option<usize>),

    /// 現在 Audio Editor で開いている clip + 選択中 event を Duplicate
    /// (= 同 source の event を直後に複製)。 spec §3.10.2 の `Ctrl+D`
    /// 動作。 `audio_editor_clip` / `audio_editor_selected_event` が
    /// `Some` でないと no-op。 新 event は元 event の右隣 (= clip 内
    /// 位置 = `src.event_start_in_clip_beats + src.event_length_beats`)、
    /// 同 source + 同パラメータ。 clip.length_beats が足りなければ自動
    /// で伸ばす。 selection は新 event に移る。
    DuplicateAudioEditorEvent,
    /// B12 (r.md #8): 選択オーディオクリップを auto-warp。 transient を検出し拍
    /// グリッド (16th) に snap した warp markers を生成、 該当 event を Stretch
    /// mode に切替える。 audio 以外の選択 / 未 decode は no-op。
    AutoWarpSelectedClip,

    // ---- B12-manual (r.md #8): warp marker 手動編集 (audio editor) -----------
    /// Audio Editor で warp marker `marker_idx` の出力位置 (`locked_beat`、 event-local
    /// 拍) を `new_locked_beat` へ動かす (= ドラッグ)。 `source_frame` は据え置きで
    /// stretch。 `audio_editor_clip` の `event_idx` 番目 event を対象。 隣接 marker 間に
    /// clamp (`common::audio_render::move_warp_marker`)。 範囲外 / 非 audio は no-op。
    MoveWarpMarker {
        event_idx: usize,
        marker_idx: usize,
        new_locked_beat: f64,
    },
    /// Audio Editor で warp marker を追加 (= 波形ダブルクリック)。 `source_frame` (source 内
    /// frame) を `locked_beat` (event-local 拍) に pin。 `locked_beat` 昇順を保って挿入、
    /// 退化 (同拍) は skip。 `audio_editor_clip` の `event_idx` 番目 event を対象。
    AddWarpMarker {
        event_idx: usize,
        source_frame: u64,
        locked_beat: f64,
    },
    /// Audio Editor で warp marker `marker_idx` を削除 (= 右クリック / Alt+クリック)。
    /// 2 件未満になれば warp は uniform に degrade。 `audio_editor_clip` の `event_idx` 番目。
    DeleteWarpMarker { event_idx: usize, marker_idx: usize },

    // ---- Audio Editor event 単位編集 (Phase 2 PR-D 段階 3) -----------
    /// Audio Editor で event の clip 内 start position を変更
    /// (= 中央 drag 移動)。 `clip` の `event_idx` 番目の event の
    /// `event_start_in_clip_beats` を `new_start_beats` (clamp 0..) に
    /// 設定。 範囲外 / 非 audio clip / event_idx 範囲外なら no-op。
    /// clip.length_beats は新 event の終端を含むよう自動拡張。
    SetAudioEventStart {
        clip: ClipRef,
        event_idx: usize,
        new_start_beats: f64,
    },
    /// Audio Editor で event 端 trim (= 左右端 drag)。 `side == Left`
    /// なら `event_start_in_clip_beats` + `event_length_beats` +
    /// `source_start_frames` を delta で連動更新、 `side == Right` なら
    /// `event_length_beats` + `source_end_frames` を更新。 source は
    /// `audio_sources` から sample_rate を取って delta_beats → frames
    /// 変換。 clip.length_beats は必要に応じて拡張。
    SetAudioEventTrim {
        clip: ClipRef,
        event_idx: usize,
        side: AudioEventTrimSide,
        delta_beats: f64,
    },
    /// Audio Editor の空白領域に file system drag&drop された path を
    /// decode + import し、 既存 audio clip の content に新 event として
    /// `position_in_clip_beats` の位置に追加。 source 採番 + buffer cache
    /// 登録は `import_audio::import_one` 経由 (= top-level Import Audio
    /// と同 pipeline)。 失敗時は status_message にエラー、 selection は
    /// 新 event に移す。 clip.length_beats は必要に応じて拡張。
    AddAudioEventFromFile {
        clip: ClipRef,
        path: PathBuf,
        position_in_clip_beats: f64,
    },
    /// Audio Editor の event 選択集合を `indices` で置き換える (= 矩形
    /// 選択 / Shift+click トグル / Ctrl+A 全選択)。 index は clip 内
    /// events Vec への index。 重複は handler 側で除外。 view state なので
    /// 非 undoable。
    SetAudioEditorEventSelection(Vec<usize>),
    /// Audio Editor で選択中の全 event を削除 (= Delete key、 複数選択
    /// 対応)。 `audio_editor_clip` が開いていて選択が空でないときのみ。
    /// 削除後 selection は clear。
    DeleteAudioEditorSelection,

    // -------- Phase 7 B5 (`docs/plan_scale.html`): Scale & Root ------------
    /// 現在 playhead 位置で active な scale event を `(root, scale)` で更新。
    /// `scale_changes` が空なら beat=0 の event を新規追加 (`plan §4.1`)。
    /// 空でなければ `Song::scale_at(playhead)` で見つかる event を update。
    /// undoable (= 1 dropdown commit = 1 Undo step)。
    SetScaleAtPlayhead {
        root: u8,
        scale: common::scale::Scale,
    },
    /// 全 scale event を削除 (= Scale 機能 OFF / chromatic に戻す)。
    /// Transport bar の root dropdown で「— (No Key)」 を選んだとき発火。
    /// undoable。
    ClearScaleChanges,
    /// 既存ノートの pitch を最寄りの in-scale pitch に一括補正。
    /// 対象は `QuantizePitchTarget`。 各 note の `pitch = scale_at(note の
    /// song-global beat).snap(pitch)` で書き換え (note の start_beat 時点の
    /// scale を尊重 = 転調をまたぐ note も自然に補正される)。 1 操作 1 Undo
    /// step。 piano_roll の右クリック menu / inspector ボタン経由で発火。
    QuantizePitchesToScale(QuantizePitchTarget),
    /// Snap on Draw toggle (session-only)。 piano_roll header の toggle で
    /// 切替。 Undo 非対象 (= session 設定)。
    ToggleSnapOnDraw,
    /// Snap Live Input toggle (session-only)。 transport bar の toggle で
    /// 切替。 Undo 非対象。
    ToggleSnapLiveInput,
    /// piano_roll の Fold to Scale toggle (session-only)。 piano_roll snap
    /// toolbar の「Fold」 button で切替。 Undo 非対象。
    ToggleFoldToScale,
}

impl AppEvent {
    /// r.md #29: この event が undo step (Song snapshot) を積んだとき、 履歴
    /// リストに出す **日本語ラベル**。 「command → 名前」 の SSoT。
    ///
    /// - 編集 (undoable) event には具体的な名前を与える。
    /// - 非編集 event (transport / 選択 / view / IPC / 高頻度 tick 等) は
    ///   snapshot を積まないのでラベルは記録されない → catch-all `"編集"` で足りる。
    ///   万一ラベル漏れの編集 event があっても catch-all が名前を保証する
    ///   (= 履歴が空欄にならない graceful degradation)。
    ///
    /// `begin_event` が **全 event** で呼ぶので安価であること (heap を持たない
    /// `&'static str` の純 match)。
    pub fn undo_label(&self) -> &'static str {
        use AppEvent as E;
        match self {
            // ---- テンポ / 拍子 ----
            E::CommitBpmEdit | E::SetSongBpmFromScrub(..) => "テンポ変更",
            E::CommitTimeSigNumEdit
            | E::SetSongTimeSigDenominator(..)
            | E::SetSongTimeSigNumFromScrub(..) => "拍子変更",

            // ---- ノート ----
            E::AddNote { .. } => "ノート追加",
            E::SetNotePositions(..) => "ノート移動",
            E::ResizeNote { .. } | E::ResizeNotes(..) => "ノート長さ変更",
            E::DeleteSelectedNotes => "ノート削除",
            E::DuplicateSelectedNotes | E::CopyNotes(..) => "ノート複製",
            E::SetNoteVelocity { .. } | E::SetNoteVelocities(..) => "ベロシティ変更",
            E::SetNotesMuted { .. } => "ノートミュート",
            E::SetNoteLyrics { .. } => "歌詞編集",
            E::QuantizeSelectedNotes(..) => "ノートをクオンタイズ",
            E::QuantizePitchesToScale(..) => "ピッチをスケールに補正",

            // ---- クリップ ----
            E::CreateClip { .. } => "クリップ作成",
            E::AddTextClipAt { .. } => "テキストクリップ追加",
            E::SetClipPositions(..) => "クリップ移動",
            E::ResizeClip { .. } => "クリップ長さ変更",
            E::DeleteSelectedClip => "クリップ削除",
            E::DuplicateClipsShared(..) | E::DuplicateClipsUnique(..) => "クリップ複製",
            E::CloneClipsLinked(..) | E::CloneClipsIndependent(..) => "クリップ複製",
            E::MakeClipUnique(..) => "クリップを独立化",
            E::CommitRenameClip => "クリップ名変更",
            E::SplitClipAtPlayhead { .. } => "クリップ分割",
            E::GlueSelectedClips => "クリップ結合",
            E::SetClipColor { .. } => "クリップ色変更",
            E::SetClipMuted { .. } | E::SetClipsMuted { .. } => "クリップミュート",
            E::SetClipReversed { .. } | E::ToggleClipReversed(..) => "クリップ逆再生",
            E::SetClipStretchMode { .. } => "ストレッチモード変更",
            E::SetClipGainDb { .. } | E::SetClipGainDbBatch(..) => "クリップゲイン変更",
            E::SetClipPan { .. } => "クリップパン変更",
            E::SetClipPitchSemitones { .. } => "クリップピッチ変更",
            E::SetClipFormantSemitones { .. } => "クリップフォルマント変更",
            E::SetClipFadeInBeats { .. }
            | E::SetClipFadeOutBeats { .. }
            | E::SetClipFadeInCurve { .. }
            | E::SetClipFadeOutCurve { .. }
            | E::SetClipFadeBeatsBatch(..)
            | E::SetClipFadeCurveBatch(..) => "フェード変更",
            E::AutoFadeSelectedClips => "オートフェード",
            E::AutoCrossfadeSelectedClips => "オートクロスフェード",
            E::BroadcastDiscreteClipEdit { .. } => "クリップ編集",
            E::BounceClipInPlace(..) | E::BounceClipWithFx(..) => "バウンス",

            // ---- テキスト / 画像 クリップ ----
            E::SetClipTextMuted { .. }
            | E::SetClipTextContent { .. }
            | E::SetClipTextFontFamily { .. }
            | E::SetClipTextAlign { .. }
            | E::SetClipTextFadeInCurve { .. }
            | E::SetClipTextFadeOutCurve { .. }
            | E::SetClipTextNumField { .. }
            | E::CommitClipTextContentEdit
            | E::CommitClipTextFontFamilyEdit
            | E::SetClipTextX { .. }
            | E::SetClipTextY { .. }
            | E::SetClipTextW { .. }
            | E::SetClipTextH { .. }
            | E::SetClipTextRotation { .. } => "テキスト編集",
            E::CommitFontFromPicker(..) => "フォント変更",
            E::SetClipImageX { .. }
            | E::SetClipImageY { .. }
            | E::SetClipImageW { .. }
            | E::SetClipImageH { .. }
            | E::SetClipImageOpacity { .. }
            | E::SetClipImageRotation { .. } => "画像変形",

            // ---- トラック ----
            E::AddInstrumentTrack => "トラック追加",
            E::AddReturnTrack => "リターントラック追加",
            E::GroupSelectedTracks { .. } => "トラックをグループ化",
            E::UngroupTracks { .. } => "グループ解除",
            E::SetTrackParent { .. } => "トラック親変更",
            E::RemoveLastTrack | E::DeleteTracks(..) => "トラック削除",
            E::DuplicateTracksShared(..) | E::DuplicateTracksUnique(..) => "トラック複製",
            E::MoveTrackUp(..) | E::MoveTrackDown(..) | E::ReorderTracks(..) => "トラック並べ替え",
            E::CommitRenameTrack => "トラック名変更",
            E::SetTrackColor { .. } => "トラック色変更",
            E::ResetTrackClipColors { .. } => "クリップ色リセット",

            // ---- セクション帯 ----
            E::CommitRenameSection => "セクション名変更",
            E::SetSectionColor { .. } => "セクション色変更",

            // ---- ミキサー / センド ----
            E::SetTrackVolume { .. } => "音量変更",
            E::SetTrackPan { .. } => "パン変更",
            E::ToggleTrackMute(..) => "ミュート切替",
            E::ToggleTrackSolo(..) => "ソロ切替",
            E::SetMasterGain(..) => "マスターゲイン変更",
            E::AddSend { .. } => "センド追加",
            E::RemoveSend { .. } => "センド削除",
            E::SetSendMode { .. } => "センドモード変更",
            E::SetSendGain { .. } => "センドゲイン変更",
            E::SetSendEnabled { .. } => "センド有効切替",

            // ---- デバイス / プラグイン ----
            E::SelectPluginFromDb { .. } => "プラグイン追加",
            E::RemoveDevice { .. } => "デバイス削除",
            E::ReorderInspectorChain(..) => "チェーン並べ替え",
            E::SetVideoFxParam { .. } => "映像FX変更",
            E::SetPluginParam { .. } => "プラグインパラメータ変更",
            E::SetSidechainSource { .. } | E::SetAuxInputTapPoint { .. } => "サイドチェイン設定",
            E::SetPluginSendAllKeys { .. } => "プラグインへのキー送出設定",
            E::ExplodeParallelOut { .. } => "パラアウト展開",
            E::SetParallelOutputRoute { .. } => "パラアウト経路変更",

            // ---- モジュレーション ----
            E::AddModSource { .. } => "モジュレーション追加",
            E::RemoveModSource { .. } => "モジュレーション削除",
            E::EditModSource { .. }
            | E::SetModSourceAttack { .. }
            | E::SetModSourceRelease { .. }
            | E::SetModSourceTapPoint { .. }
            | E::SetModSourceTrack { .. } => "モジュレーション編集",
            E::AddModRouting { .. } | E::RemoveModRouting { .. } => "モジュレーション接続",
            E::SetModRoutingDepth { .. } => "モジュレーション深度変更",
            E::SetModRoutingPolarity { .. } => "モジュレーション極性変更",

            // ---- オートメーション ----
            E::AddAutomationPoint { .. } => "ポイント追加",
            E::MoveAutomationPoints { .. } => "ポイント移動",
            E::DeleteAutomationPoints { .. } => "ポイント削除",
            E::SetAutomationPointValue { .. } => "ポイント値変更",
            E::QuantizeSelectedAutomationPoints(..) => "ポイントをクオンタイズ",
            E::SetAutomationCurveType { .. }
            | E::SetAutomationCurveBezierTension { .. }
            | E::SetAutomationCurveExponentialBend { .. } => "カーブ変更",
            E::CreateAutomationClip { .. } => "オートメーションクリップ作成",
            E::MoveAutomationClips { .. } => "オートメーションクリップ移動",
            E::ResizeAutomationClips { .. } => "オートメーションクリップ長さ変更",
            E::DeleteAutomationClips { .. } => "オートメーションクリップ削除",
            E::CloneAutomationClipsLinked { .. } | E::CloneAutomationClipsIndependent { .. } => {
                "オートメーションクリップ複製"
            }
            E::DuplicateAutomationClipsShared(..) | E::DuplicateAutomationClipsUnique(..) => {
                "オートメーションクリップ複製"
            }
            E::MakeAutomationClipUnique(..) => "オートメーションクリップを独立化",
            E::SetLaneEnabled { .. } => "オートメーション有効切替",
            E::SetLaneVisible { .. } => "レーン表示切替",
            E::SetLaneDefault { .. } => "レーン既定値変更",
            E::SetLaneHeight { .. } | E::SetSingleTrackRowH { .. } => "レーン高さ変更",
            E::DeleteLane { .. } => "レーン削除",
            E::AddImageAutomationLane { .. }
            | E::AddTextAutomationLane { .. }
            | E::AddGroupAutomationLane { .. }
            | E::AddAutomationFromLastTouched => "オートメーションレーン追加",
            E::RemoveImageAutomationLane { .. }
            | E::RemoveTextAutomationLane { .. }
            | E::RemoveGroupAutomationLane { .. } => "オートメーションレーン削除",

            // ---- 立ち絵グループ変形 ----
            E::BeginGroupTransformDrag | E::SetGroupTransformField { .. } => "グループ変形",

            // ---- スケール ----
            E::SetScaleAtPlayhead { .. } => "スケール変更",
            E::ClearScaleChanges => "スケール解除",

            // ---- VOICEVOX (歌唱 / トーク / 口パク) ----
            E::SetLipsyncTarget { .. } => "口パク出力先変更",
            E::SetMouthMapSlot { .. } => "口形状設定",
            E::SetClipVoice { .. } => "声変更",
            E::SetClipTalkParam { .. } => "トークパラメータ変更",

            // ---- Audio Editor (波形編集) ----
            E::DuplicateAudioEditorEvent => "オーディオイベント複製",
            E::DeleteAudioEditorSelection => "オーディオイベント削除",
            E::SetAudioEventStart { .. } => "オーディオイベント移動",
            E::SetAudioEventTrim { .. } => "オーディオイベントトリム",
            E::AddAudioEventFromFile { .. } => "オーディオイベント追加",
            E::AutoWarpSelectedClip => "オートワープ",
            E::MoveWarpMarker { .. } | E::AddWarpMarker { .. } | E::DeleteWarpMarker { .. } => {
                "ワープマーカー編集"
            }

            // ---- メディア読み込み ----
            E::ImportAudio { .. } => "オーディオ読み込み",
            E::ImportVideo { .. } => "動画読み込み",
            E::ImportImage { .. } => "画像読み込み",

            // 非編集 event (snapshot を積まない) はここに落ちてラベルは記録
            // されない。 編集 event のラベル漏れも "編集" で名前を保証する。
            _ => "編集",
        }
    }
}

/// Phase 7 B5: `QuantizePitchesToScale` の対象スコープ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantizePitchTarget {
    /// `selected_clip` + `selected_notes` の note を quantize。 piano_roll で
    /// 範囲選択した note の一括補正。
    SelectedNotes,
    /// `selected_clip` の全 note を quantize (note 選択不要)。 piano_roll header
    /// or arrangement clip 右クリック「Quantize all to Scale」 等から発火。
    SelectedClipAllNotes,
}

/// `*Batch` 系 AppEvent で fade in / out を区別するための marker。
/// `daw_ui_core::FadeEdge` は widget 側 type で daw_01 model 側 enum
/// に直接置けないので、 AppEvent module 内に再定義 (= bincode 経由は
/// 不要なので common::model に追加する必要なし)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FadeEdgeKind {
    In,
    Out,
}

/// [`AppEvent::BroadcastDiscreteClipEdit`] が運ぶ discrete inspector 編集の
/// 種別。 per-clip setter (`set_clip_*`) は対象 `ClipContent` variant 違いで no-op に
/// なる (variant-safe) ので、 broadcast 先に種別違いのクリップが混ざっても安全
/// (= その field を持つクリップにだけ適用される)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DiscreteClipEdit {
    Reversed(bool),
    Muted(bool),
    StretchMode(common::model::StretchMode),
    FadeCurve(FadeEdgeKind, common::model::FadeCurve),
    TextMuted(bool),
    TextAlign(common::model::TextAlign),
    TextFadeCurve(FadeEdgeKind, common::model::FadeCurve),
}

/// Audio Editor の event trim 側 (左端 / 右端) marker。 `SetAudioEventTrim`
/// AppEvent 用。 left = (event_start, source_start) 連動、 right =
/// (event_length, source_end) 連動。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioEventTrimSide {
    Left,
    Right,
}

