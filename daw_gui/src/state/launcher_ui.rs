//! r.md #87 クリップランチャーの **session-only** な UI 状態。
//!
//! `AppData` のフィールドを 1 本 (`launcher`) に束ねてあるのは、
//! `AppData::new` が実コード 300 行 budget (不変条件 9) の天井に張り付いていて
//! フィールドを 1 個ずつ増やすと初期化行だけで超えるため。 まとまりとしても
//! 「ランチャーの一時状態」 で 1 つなので、束ねる方が読みやすい。
//!
//! ここに置いてよいのは **保存しないもの**だけ:
//! - 曲の中身 (列 / セル / 主導権 / ローンチ設定) は `Song`
//! - 見方の都合 (帯の幅 / 列幅 / 横スクロール / レイアウト) は `UiPrefs` + `ViewState`
//! - 走行中の位置 (いま鳴っているセル) は engine が atomic で publish する
//!   (計画書 §1.4 — `Song` にも ここにも 持たない)

use crate::event_launcher::{LauncherBindTarget, LauncherBinding, LauncherRow};

/// キーボード操作の起点になるセル位置。 **空セルにも置ける**ので id ではなく
/// 「行 + 表示順の列 index」 で持つ (矢印で空きプレースホルダ列まで歩ける)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LauncherFocus {
    pub row: LauncherRow,
    pub scene_index: usize,
}

/// ランチャーの一時 UI 状態。
#[derive(Debug, Default)]
pub struct LauncherUiState {
    /// 矢印 / `Enter` / `Delete` / `Ctrl+D` の対象になるセル位置。
    /// セルを click したときと矢印で動かしたときに更新する。
    pub focus: Option<LauncherFocus>,
    /// ポインタが乗っているセル位置 (widget が毎フレーム更新)。
    /// **貼り付け先の解決**に使う — arrangement の
    /// `arrange_hovered_track` + `arrangement_hover_beat` と同じ役割。
    pub hover: Option<LauncherFocus>,
    /// 列見出しの inline rename 対象 (`Scene::id`)。`None` = 編集していない。
    /// トラック名 / セクション名の rename と同 idiom。
    pub scene_rename_id: Option<u32>,
    /// rename の編集中テキスト (commit するまで `Scene::name` には書かない)。
    pub scene_rename_text: String,
    /// MIDI → ランチャー操作の binding 表。
    ///
    /// **`Song.midi_bindings` へ移すまでの暫定の置き場**。`MidiBinding` が
    /// CC 専用でノートを持てないので、いまはここが SSoT。読み書きは
    /// `AppData::launcher_bindings` / `add_launcher_binding` /
    /// `clear_launcher_bindings` の 3 本だけを通すこと (移設時にそこだけ直す)。
    pub bindings: Vec<LauncherBinding>,
    /// Learn 待ち。`Some` の間、次に届いたノート / CC を `target` へ bind する。
    pub learn_target: Option<LauncherBindTarget>,
}
