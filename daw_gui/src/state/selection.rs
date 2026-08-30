//! S3b-1: AppData state group (SelectionState)。 docs/plan_arch_refactor.md §7.5
//! の分割表に従って app.rs の AppData から機械移送したフィールド群。

use crate::app::{AutomationPointKeyRef, EditSurface};

#[derive(Debug)]
pub struct SelectionState {
    /// **選択の SSoT** — 「時間区間 × レーン集合」1 本
    /// (`docs/plan_range_selection.md`)。 クリップ / ノート / automation 点 /
    /// automation クリップ / audio event の選択はすべてここからの**導出**で、
    /// 面ごとの選択集合とアンカーは持たない。 session-only (保存しない)。
    ///
    /// 「レーン」 は面ごとに読み替える — アレンジャーはトラック行 /
    /// オートメーションレーン行、 ピアノロールは鍵盤行、 オーディオエディタは波形行。
    pub time: Option<common::model::TimeSelection>,
    /// Shift+クリックで範囲を伸縮するときの基点 (拍)。 無修飾 click / Ctrl+click で
    /// 更新し、 Shift+click では更新しない (= 同じ基点から繰り返し伸縮できる)。
    /// 面ごとに 9 本あったアンカーはこの 1 本に畳まれた。
    pub range_anchor: Option<f64>,
    /// ランチャー (セッション) のセル選択。 **グリッドに時間軸が無いので範囲では
    /// 表せない**唯一の「オブジェクト選択」。 末尾 = anchor。 session-only。
    pub selected_launcher_cells: Vec<common::model::ClipKey>,
    /// ランチャーセル選択のアンカー (Shift+click の基点)。
    pub launcher_cell_anchor: Option<common::model::ClipKey>,

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
    /// r.md #87: 選択中の列 (シーン) id 集合。末尾 = anchor。
    ///
    /// **セル選択とは排他**にする — 列とセルはインスペクタの同じ面 (ローンチ) を
    /// 使うので、両方を非空にすると「セルの設定」と「列の設定」が同時に出て
    /// どちらを触っているのか分からなくなる。列を選べばセルの選択は落ち、
    /// セルを選べば列の選択は落ちる (`select_scene` / `select_launcher_cell`)。
    /// session-only (保存しない)。
    pub selected_scene_ids: Vec<u32>,
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
    pub last_edit_select: Option<EditSurface>,
    /// Phase 3 (`docs/plan_automation.md` §10): 選択中の automation point。
    /// gui_01 #033 で widget 側の lasso 矩形選択が landing するまで空のまま
    /// だが、 copy / paste / quantize / delete のハンドラは selection を
    /// 入力として動くので先行実装する。 widget からは
    /// `SelectAutomationPoints` (#033) で上書き。 session-only。
    pub selected_automation_points: Vec<AutomationPointKeyRef>,
    /// r.md #71 (プラグインのコピー / 移動): インスペクタのチェーンで選択中の
    /// device (安定 `PluginInstance::id`)。 末尾 = 「最後にクリックした anchor」。
    /// session-only (保存しない)。
    ///
    /// **読む側は必ず `live_device_ids()` で正規化する** — この集合は
    /// 「カーソルトラックのチェーン」 という面の上にあるので、 cursor track が
    /// 動いた瞬間に元トラックの id が stale になる。
    pub selected_device_ids: Vec<u64>,

    // -------- Shift+click 範囲選択のアンカー (r.md #35) --------
    // `docs/plan_selection_modifiers.md` §4.3。 無修飾 click / Ctrl+click で更新し、
    // Shift+click では更新しない (= 同じ基点から繰り返し Shift+click して範囲を伸縮できる。
    // Explorer / Finder / REAPER と同じ)。 「選択集合の末尾 = アンカー」 という旧 idiom は
    // RangeFromAnchor が集合ごと書き換えるので基点に使えず、 面ごとの明示フィールドが所有する。
    // すべて session-only (保存しない)。 対象が消えたら range 解決が None に倒れて Single 相当。
    /// トラック選択のアンカー (`Track::id`)。 旧 `ArrangementState.selection_anchor` から移設。
    pub track_anchor: Option<u32>,
    /// Arranger セクション選択のアンカー (`Section::id`)。
    pub section_anchor: Option<u32>,
    /// automation point 選択のアンカー。
    pub automation_point_anchor: Option<AutomationPointKeyRef>,
    /// automation clip 選択のアンカー。
    pub automation_clip_anchor: Option<common::model::AutomationClipKey>,
    /// r.md #71 (プラグインのコピー / 移動): device 選択のアンカー
    /// (Shift+click 範囲選択の基点)。
    pub device_anchor: Option<u64>,
    /// r.md #87: 選択中の列 (シーン) 選択のアンカー。
    pub scene_anchor: Option<u32>,
}
