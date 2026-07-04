//! S3b-1: AppData state group (SelectionState)。 docs/plan_arch_refactor.md §7.5
//! の分割表に従って app.rs の AppData から機械移送したフィールド群。

use crate::app::{AutomationPointKeyRef, EditSelectSurface};

pub struct SelectionState {

    // -------- Selection --------
    /// Track multi-selection (Ableton Live / Reaper 互換)。 末尾要素 =
    /// 「最後にクリックした anchor」 = カーソル相当。 widget 側 (gui_01
    /// arrangement) からは `selected_tracks: &[u32]` として渡す。 id
    /// ベース (Track::id) で持ち、 track 並び替えでも安定。
    pub selected_track_ids: Vec<u32>,
    /// 選択中の Arranger セクション id 集合 (`selected_track_ids` と同 idiom、
    /// 末尾 = anchor)。 gui_01 の `SelectSection` で更新、 帯のハイライト + キーボード Delete
    /// の対象。 section を選ぶと他面 (clip/note/track) の選択はクリアして Delete の曖昧さを避ける。
    pub selected_section_ids: Vec<u32>,
    /// gui_01 #028 (M14 Phase 63n-3): 選択中の automation clip。 MIDI
    /// clip 用 `selected_clips` と直交 (= 同時に両方を持てる、 他 DAW
    /// 互換)。 widget の `SelectAutomationClips` で上書き、 widget へは
    /// 毎フレーム `&[AutomationClipKey]` で渡して selected highlight を
    /// 描画させる。 session-only。
    pub selected_automation_clips: Vec<common::model::AutomationClipKey>,
    /// 直近に確定した編集面 (clip / automation 点 / automation クリップ / note /
    /// audio event)。 これらは共存選択できる (lasso は点とクリップを両方拾う、
    /// clip 選択は automation 選択を消さない) ため、 copy / cut / delete の対象面が
    /// 曖昧になる。 「最後に選んだ面を対象にする」 (last-wins、 #071) ための
    /// タイブレーカ。 `None` は初期状態。 session-only。 edit_surface (view) が
    /// この値で対象面を解決する。
    pub last_edit_select: Option<EditSelectSurface>,
    /// Phase 3 (`docs/plan_automation.md` §10): 選択中の automation point。
    /// gui_01 #033 で widget 側の lasso 矩形選択が landing するまで空のまま
    /// だが、 copy / paste / quantize / delete のハンドラは selection を
    /// 入力として動くので先行実装する。 widget からは
    /// `SelectAutomationPoints` (#033) で上書き。 session-only。
    pub selected_automation_points: Vec<AutomationPointKeyRef>,
    /// 選択 anchor (= 末尾)。 stable `ClipKey` (track_id + clip_id) 保持で
    /// 並べ替え / undo を跨いでも壊れない。 index 解決は `selected_clip_ref()`。
    pub selected_clip: Option<common::model::ClipKey>,
    /// 選択集合。 stable `ClipKey` 保持。 index 解決は `selected_clip_refs()`。
    pub selected_clips: Vec<common::model::ClipKey>,
    pub selected_notes: Vec<u32>,
    /// Audio Editor で選択中の event index 群 (`audio_editor_clip` の clip
    /// 内 events Vec への index)。 複数選択対応: click = 単一、 Shift+click =
    /// トグル、 空き領域 drag = 矩形選択、 Ctrl+A = 全選択。 空 Vec で
    /// 「未選択」。 anchor (= Inspector / footer / nav の代表) は last()
    /// (= `audio_editor_anchor_event`)。 編集 (gain/pan/fade 等) は選択集合
    /// 全体に broadcast (`audio_event_target_indices`)。 close で clear、
    /// undo でも clear (index は容易にずれるため、 ノート選択と同方針)。
    pub audio_editor_selected_events: Vec<usize>,
}
