//! プロジェクトに同梱する GUI 表示状態 ([`ViewState`])。`model.rs` の肥大化を避けて
//! 分離 (r.md #96 で下部パネルの開閉を加えた際に分割)。**serde 専用** で IPC を渡らない
//! ので `common/build.rs` の `WIRE_SOURCES` 対象外。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{
    AudioEditorViewState, AutomationLaneKey, ClipKey, EditorWindowGeometry, FollowMode,
    LauncherLayout, LoopRegion, PianoRollViewState,
};

/// プロジェクトに同梱する GUI 表示状態のスナップショット。
/// `AppData` (live SSoT) から save 時に `snapshot_view_state` で作り、load 時に
/// `restore_view_state` で流し込む。**serde 専用** (bincode derive 無し) ＝ IPC を渡らない。
/// `ClipKey` は struct で JSON の map key にできないため、per-clip view は
/// `Vec<(ClipKey, _)>` で持つ。各フィールドは `#[serde(default)]` で前方互換。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ViewState {
    // ---- Arrangement (タイムラインは曲に 1 つ = グローバル) ----
    #[serde(default)]
    pub arrange_zoom_x: f32,
    #[serde(default)]
    pub arrange_scroll_beat: f32,
    #[serde(default)]
    pub arrange_track_top: f32,
    #[serde(default)]
    pub arrange_track_row_h: f32,
    #[serde(default)]
    pub arrange_header_w: f32,
    /// per-track の行高 override (track_id → px)。
    #[serde(default)]
    pub track_row_overrides: HashMap<u32, u16>,
    /// automation lane を展開中の track_id (順序非依存、save 時に sort)。
    #[serde(default)]
    pub expanded_automation_tracks: Vec<u32>,
    /// オートメーションレーンの表示行高 (`track_row_overrides` のレーン版)。
    ///
    /// **model の `AutomationLane.height_px` とは別の面**で、こちらは「見方の都合」
    /// (Fit / `Z` 縦ズームの結果) を持つ。ここが保存されないと、Fit した直後に
    /// 保存しても開き直しで model の `height_px` が復活する — 過去に `Z` で
    /// 画面いっぱいへ拡大したレーンがあると、**そのレーン 1 本がビューポートを
    /// 占有して全トラックが画面外へ押し出される**。トラック行高
    /// (`arrange_track_row_h`) は保存されるので、片側だけ Fit が効いた状態になる。
    #[serde(default)]
    pub automation_lane_row_overrides: Vec<(AutomationLaneKey, u16)>,
    #[serde(default)]
    pub master_row_automation_expanded: bool,
    /// 再生中プレイヘッド追従スクロールの方式 (Alt+F で循環)。旧 .daw は
    /// フィールド欠落 → `FollowMode::default()` (= Page) で読まれる。
    #[serde(default)]
    pub arrange_follow: FollowMode,
    /// 再生ループ (ON/OFF + 範囲)。 ズーム / スクロールと同じ「聴き方の都合」 として
    /// ここに永続化する (`Song` には無い = 変えても `*` が付かない)。 旧 `.daw` は
    /// `Song.loop_start_beat` / `loop_end_beat` を持つので、`crate::project::load_project`
    /// が読み出して [`crate::project::LoadedProject::loop_region`] へ移す。
    #[serde(default)]
    pub loop_region: LoopRegion,
    // ---- Snap / grid / piano roll モード ----
    #[serde(default)]
    pub arrange_snap_enabled: bool,
    #[serde(default)]
    pub arrange_snap_choice: u8,
    #[serde(default)]
    pub pianoroll_snap_enabled: bool,
    #[serde(default)]
    pub pianoroll_snap_choice: u8,
    #[serde(default)]
    pub piano_roll_fold: bool,
    #[serde(default)]
    pub snap_on_draw: bool,
    #[serde(default)]
    pub snap_live_input: bool,
    // ---- 下部パネル ----
    /// `None` = 閉じている / `Some(0)` = Mixer / `Some(1)` = Piano Roll (r.md #96)。
    /// 旧ファイルは `u8` を保存していたので `Some(n)` として読める。フィールド自体が
    /// 無い (さらに古い) ファイルは従来どおり Mixer を開いた状態にする。
    #[serde(default = "default_bottom_panel")]
    pub bottom_panel: Option<u8>,
    // ---- per-clip view (Ableton Live / Bitwig 流) ----
    #[serde(default)]
    pub piano_roll_views: Vec<(ClipKey, PianoRollViewState)>,
    #[serde(default)]
    pub audio_editor_views: Vec<(ClipKey, AudioEditorViewState)>,
    // ---- プラグインエディタ窓のジオメトリ (r.md #65) ----
    /// device_id → 最後に観測したエディタ窓の位置 / client サイズ。
    /// save 時に **現存する device の分だけ**を device_id 昇順で書き出す
    /// (per-clip view と同じ orphan GC + 決定的順序の idiom)。
    #[serde(default)]
    pub plugin_editor_windows: Vec<(u64, EditorWindowGeometry)>,
    // ---- クリップランチャー (r.md #87) ----
    // どれも「聴き方 / 見方の都合」なので **`*` は立てない**が保存はする
    // (ズーム / スクロールと同じ扱い、`docs/plan_rmd_87_clip_launcher.md` §1.3)。
    // 曲の一部 (列・セル・主導権) は `Song` 側に居る。
    /// ランチャー帯とアレンジのレーンをどう見せるか。
    /// アレンジと下部パネル (ピアノロール / ミキサー / オーディオエディタ) の
    /// 境界比率 (上の取り分、`0.05..=0.95`)。`0.0` = 未設定でアプリ既定に倒す。
    ///
    /// **これが無いと、境界をどこへ動かして保存してもアプリを起動し直すたびに
    /// 既定位置へ戻る** — 比率は widget の一時状態にしか無かった (縦に見える
    /// 範囲が毎回変わるので、行高を保存していても「一部しか映らない」)。
    #[serde(default)]
    pub arrangement_split_ratio: f32,
    #[serde(default)]
    pub launcher_layout: LauncherLayout,
    /// [`LauncherLayout::Both`] のときのランチャー帯の幅 (px)。`Tab` で
    /// レイアウトを一巡して戻ってきたとき、手で決めたこの幅に復帰する。
    /// `0` 以下 = 未設定 (表示側の既定幅)。
    #[serde(default)]
    pub launcher_width: f32,
    /// シーン 1 列の幅 (px)。全列共通。`0` 以下 = 未設定 (表示側の既定幅)。
    #[serde(default)]
    pub launcher_scene_col_w: f32,
    /// ランチャー帯の横スクロール位置 (列数、小数可)。
    #[serde(default)]
    pub launcher_scroll_scene: f32,
}

/// `ViewState.bottom_panel` の serde 既定値 (フィールドが無い旧ファイル = Mixer を開く)。
fn default_bottom_panel() -> Option<u8> {
    Some(0)
}
