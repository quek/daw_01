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

use crate::event_launcher::LauncherRow;

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
    /// CC でランチャーを撃つときの **前回の押下状態** (`(channel, input)` ごと)。
    /// CC は動かすたびに値が届くので、`value >= 64` を毎回「押下」にすると
    /// ノブを回しただけで `Toggle` のセルが再生 ⇔ 停止 を往復する。**0→1 /
    /// 1→0 の遷移だけ**を発火にするための直前値。
    pub cc_pressed: Vec<((u8, common::model::MidiBindInput), bool)>,
    /// 列見出しの inline rename 対象 (`Scene::id`)。`None` = 編集していない。
    /// トラック名 / セクション名の rename と同 idiom。
    pub scene_rename_id: Option<u32>,
    /// rename の編集中テキスト (commit するまで `Scene::name` には書かない)。
    pub scene_rename_text: String,
    /// Learn 待ち。`Some` の間、次に届いたノート / CC を `target` へ bind する。
    ///
    /// bind 表そのものは `Song.midi_bindings` (パラメータもランチャーも 1 本)。
    /// ここに置くのは「いま Learn ボタンを押している」という一時状態だけ。
    pub learn_target: Option<common::model::BindingTarget>,
    /// engine が publish した**走行状態** (`(row_key, snapshot)`、`row_key` は
    /// `(track_id << 32) | lane_id`)。poller が 30Hz で丸ごと入れ替える。
    ///
    /// 冒頭の「走行中の位置はここにも持たない」に対する **唯一の例外で、例外に
    /// なっていない**理由: これは所有ではなく **engine が持つ事実の観測ミラー**で、
    /// 書き手は `on_launcher_rows_tick` 1 か所、保存も undo もしない
    /// (`ipc.metrics` / track peaks と同じ扱い)。`Song` に書くと
    /// フォローアクションのたびに `*` が立ち、書き出しの再現性も壊れる
    /// (計画書 §1.4)。
    pub running: Vec<(u64, common::audio_bridge::LauncherRowSnapshot)>,
}
