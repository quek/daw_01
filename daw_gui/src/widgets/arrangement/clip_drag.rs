//! アレンジのクリップ / 範囲を掴んで動かしている間の走行状態
//! (press が作り、continuation が更新し、release が確定する)。
//!
//! `mod.rs` から切り出したのはファイル budget (不変条件 9) のため。中身は移動前と
//! 同じで、兄弟モジュールから読めるようフィールドに `pub(super)` を付けただけ。

use super::{AutomationClipKey, ClipDragKind, ClipKey};

#[derive(Clone, Copy, Debug)]
pub(super) struct ClipDragAnchor {
    pub(super) key: ClipKey,
    pub(super) start_beat: f64,
    pub(super) len_beats: f64,
    pub(super) track_index: usize,
}

/// 範囲移動 (`ClipDragSession` の Move) で**一緒に動く automation クリップの断片**。
/// ゴースト専用 — commit は `move_time_range` / `copy_time_range` が範囲から自分で
/// 導く (`docs/plan_range_selection.md` §6)。 ここはその結果を先に描くための写し。
#[derive(Clone, Copy, Debug)]
pub(super) struct AutomationDragAnchor {
    pub(super) key: AutomationClipKey,
    /// 範囲で切った断片 (`shift_one_lane` が `split_at(a)` / `split_at(b)` して動かすもの)。
    pub(super) start_beat: f64,
    pub(super) len_beats: f64,
    /// `true` = トラック行の**追従** (`automation_follows_clips`) で動く断片。 追従は
    /// 「同じトラックへ置くとき」 だけ効くので、縦に動かしている間はゴーストを出さない。
    /// `false` = 範囲に**明示的に入っている lane 行**の断片 (縦移動でも横だけ動く)。
    pub(super) follow: bool,
}

#[derive(Clone, Debug)]
pub(super) struct ClipDragSession {
    pub(super) kind: ClipDragKind,
    pub(super) anchor_mouse: (f32, f32),
    /// drag 中の各 frame で更新される最終 pointer 位置。release frame の `pointer.pos` が
    /// winit の implementation によっては press 位置のままになる事があるため、release では
    /// `last_mouse` を delta 計算に使う (drag preview と一致する位置で確定する)。
    pub(super) last_mouse: (f32, f32),
    /// drag 中の最終 alt 状態。 drag overlay と release commit の **両方** がこれを真値とする
    /// (`pointer.modifiers.alt` を直接見ない)。 continuation frame で毎 frame update し、
    /// release frame では `allow_update = false` で skip することで release 直前の値を保持する。
    /// これにより OS event 順序 (ModifiersChanged が MouseInput(Released) より先に来るケース)
    /// に依存せず、 overlay と commit が必ず同一値で確定する。
    pub(super) last_alt: bool,
    /// M14 Phase 63e (#019): drag 中の最終 ctrl 状態。 `last_alt` と同じ仕組みで保持する
    /// (winit 0.30 の `ModifiersChanged` が `MouseInput(Released)` より先に届く race を回避)。
    /// release 時 dispatch で `Move + last_ctrl + !last_shift` → `CloneClipsLinked`、
    /// `Move + last_ctrl + last_shift` → `CloneClipsIndependent`、 それ以外 (ResizeLeft/Right
    /// 含む) → 既存 `MoveClips` / `ResizeClips`。 ghost overlay も `last_ctrl` を読んで色 / badge
    /// glyph を切替えるため、 commit と overlay が必ず同一値で確定する。
    pub(super) last_ctrl: bool,
    /// M14 Phase 63e (#019): drag 中の最終 shift 状態。 `last_ctrl` と組み合わせて
    /// `CloneClipsLinked` (ctrl のみ) と `CloneClipsIndependent` (ctrl + shift) を識別する。
    /// 保持仕組みは `last_alt` / `last_ctrl` と同じ (continuation で update / release で skip)。
    pub(super) last_shift: bool,
    /// Move が動かす**時間範囲** (`docs/plan_range_selection.md` §6)。 press 時に確定する
    /// — いまの選択範囲がこのクリップに掛かっていればその範囲、掛かっていなければ
    /// 掴んだクリップの占有区間。 `anchors` はこの範囲でクリップを切った断片なので、
    /// ゴーストも確定後と同じ「範囲ぶんだけ」を描く。 Resize では使わない。
    pub(super) move_range: (f64, f64),
    /// `move_range` が **時間範囲の選択** 由来か (= 範囲を掴んで動かしている)。
    /// `false` なら掴んだクリップの占有区間そのもの。タブ間 D&D はこの区別で
    /// 「範囲で切った写し」と「クリップ丸ごと」を選ぶ (`docs/plan_project_tabs.md` §5.6)。
    pub(super) from_time_selection: bool,
    pub(super) anchors: Vec<ClipDragAnchor>,
    /// Move が動かす範囲に掛かっている**トラック行** `(track_id, visible-idx)` 全部
    /// (クリップの有無を問わない)。 release の `track_map` はここから組む — anchor
    /// (= クリップ断片) から組むと、クリップの無い行が範囲から置き去りになり、
    /// automation だけの行では `track_map` が空で移動そのものが起きない。
    pub(super) track_rows: Vec<(u32, usize)>,
    /// 範囲と一緒に動く automation クリップの断片 (ゴースト用、Move のみ)。
    pub(super) automation_anchors: Vec<AutomationDragAnchor>,
    /// automation クリップの名前帯から始めた範囲移動なら、その掴んだクリップ。
    /// 短 click への格下げ先を変える (= automation クリップの選択、MIDI クリップの
    /// `SelectClip` ではなく) ためだけに持つ。
    pub(super) origin_automation: Option<AutomationClipKey>,
}

impl ClipDragSession {
    /// 時間範囲を掴んで動かしているなら、その範囲 (`docs/plan_project_tabs.md` §5.6)。
    /// クリップ本体を掴んだだけなら `None` (= 掴んだクリップを丸ごと運ぶ)。
    pub(super) fn range_move(&self) -> Option<(f64, f64)> {
        self.from_time_selection.then_some(self.move_range)
    }
}
