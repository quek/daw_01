//! r.md #87 クリップランチャーの GUI イベントと、その周辺の値型。
//!
//! [`AppEvent::Launcher`](crate::event::AppEvent::Launcher) が包む
//! [`LauncherEvent`] 1 本にランチャー操作を全部集約する。 `AppEvent` へ 25 個の
//! variant を並べないのは、 `AppData::handle_event` の巨大 match が
//! 実コード 1,000 行 budget (不変条件 9) の天井に張り付いていて **1 行も足せない**
//! から — 「1 arm = 1 サブ enum」 は既存の `AppEvent::Audio` / `AppEvent::Plugin`
//! (protocol event の direct-wrap) と同じ形なので、新しい流儀を作ってもいない。
//!
//! ここに置くのは **daw_gui が自分で持てるもの**だけ。 `Scene` / `SessionClip` /
//! `LaunchSettings` / `FollowAction` / `RowPlayback` は
//! [`common::model::session`] が SSoT で、この module はそれを **参照する**
//! (mirror 型を作らない)。
//!
//! ## 他の束との接点
//!
//! - **束 C (widget)**: `ArrangementResponse` に `pub launcher: Vec<LauncherEvent>`
//!   を足し、`arrangement_view` で
//!   [`crate::view::launcher_keys::dispatch_launcher_events`] へ流す。
//! - **束 B (engine)**: [`LauncherAudioCommand`] が `common::protocol::AudioCommand`
//!   へ足してほしい variant の形。 protocol に入ったら
//!   [`AppData::send_launcher_audio`](crate::state::AppData::send_launcher_audio)
//!   の中身だけを差し替える (送信側はその 1 関数に閉じている)。
//! - **モデル層**: [`LauncherBinding`] は `common::model::MidiBinding` を
//!   ノート対応に拡張したときにそこへ移す。読み書きは
//!   `AppData::launcher_bindings` / `add_launcher_binding` /
//!   `clear_launcher_bindings` の 3 本だけを通す。

use common::model::{
    AutomationClipKey, AutomationLaneKey, ClipKey, FollowAction, FollowActionKind, LaunchMode,
    LaunchQuantize, LaunchSettings,
};

use crate::widgets::select_modifier::SelectModifier;

/// ランチャーの **行** = 主導権 ([`RowPlayback`](common::model::RowPlayback)) を
/// 持つ単位。 arrangement の 1 行と 1:1 で、通常トラック行と展開した
/// オートメーションレーン行の両方がある (計画書 Q4)。
///
/// マスター行 (`MASTER_TRACK_ID`) は自分のクリップを持たないので
/// [`Self::Track`] にはならない。 マスターのオートメーションレーン
/// (`Song.song_lanes`) は `AutomationLaneKey { track: MASTER_TRACK_ID, .. }` で
/// [`Self::Lane`] として通る (`Song::automation_lane_by_key_mut` がそう解決する)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LauncherRow {
    /// 通常トラックの行 (`Track::id`)。
    Track(u32),
    /// 展開したオートメーションレーンの行。
    Lane(AutomationLaneKey),
}

impl LauncherRow {
    /// この行が属するトラック (レーン行なら親トラック / マスター sentinel)。
    #[must_use]
    pub fn track_id(self) -> u32 {
        match self {
            Self::Track(id) => id,
            Self::Lane(k) => k.track,
        }
    }
}

/// ランチャーの **セル** 1 つ。`clip_id` は行の中で一意
/// (`clips` と `session_clips` が同じ id 空間を共有する) なので、
/// arrangement のクリップと同じ `ClipKey` / `AutomationClipKey` で指せる
/// (計画書 §3.3)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LauncherCellKey {
    Track(ClipKey),
    Lane(AutomationClipKey),
}

impl LauncherCellKey {
    /// このセルが乗っている行。
    #[must_use]
    pub fn row(self) -> LauncherRow {
        match self {
            Self::Track(k) => LauncherRow::Track(k.track_id),
            Self::Lane(k) => LauncherRow::Lane(k.lane_key()),
        }
    }

    /// 行内で一意なクリップ id。
    #[must_use]
    pub fn clip_id(self) -> u32 {
        match self {
            Self::Track(k) => k.clip_id,
            Self::Lane(k) => k.clip,
        }
    }
}

/// インスペクタ「ローンチ」セクションの 1 操作。 複数選択へ同じ編集を
/// broadcast するので、「どの field を何にするか」 だけを運ぶ
/// (値の解決は view、 適用は handler)。
///
/// `Follow*` はセルの [`LaunchSettings::follow`] にも列の
/// [`Scene::follow`](common::model::Scene::follow) にも同じ意味で当たるので、
/// 1 つの enum を両方が共有する ([`Self::apply_follow`])。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LaunchEdit {
    Quantize(LaunchQuantize),
    Mode(LaunchMode),
    Looping(bool),
    Legato(bool),
    FollowEnabled(bool),
    FollowA(FollowActionKind),
    FollowB(FollowActionKind),
    /// 0..=100 (%)。`b` の確率は `100 - chance_a`。
    FollowChanceA(u8),
    /// `true` = Linked (クリップ終端で発火) / `false` = Unlinked (時間で発火)。
    FollowLinked(bool),
    FollowTimeBeats(f64),
    FollowMultiplier(u8),
}

impl LaunchEdit {
    /// セルの [`LaunchSettings`] へ適用する。
    pub fn apply(self, s: &mut LaunchSettings) {
        match self {
            Self::Quantize(q) => s.quantize = q,
            Self::Mode(m) => s.mode = m,
            Self::Looping(v) => s.looping = v,
            Self::Legato(v) => s.legato = v,
            _ => {
                self.apply_follow(&mut s.follow);
            }
        }
    }

    /// [`FollowAction`] へ適用する。 フォローアクション以外の variant なら
    /// 何もせず `false` (列の編集はこちらだけを使うので、 誤って
    /// `Quantize` を渡しても黙って無視される)。
    pub fn apply_follow(self, f: &mut FollowAction) -> bool {
        match self {
            Self::FollowEnabled(v) => f.enabled = v,
            Self::FollowA(k) => f.a = k,
            Self::FollowB(k) => f.b = k,
            Self::FollowChanceA(v) => f.chance_a = v.min(100),
            Self::FollowLinked(v) => f.linked = v,
            // 0 拍は「間隔ゼロで無限に発火」になるので下限を切る。
            Self::FollowTimeBeats(v) => {
                f.time_beats = if v.is_finite() { v.clamp(0.0625, 512.0) } else { 4.0 };
            }
            Self::FollowMultiplier(v) => f.multiplier = v.max(1),
            _ => return false,
        }
        true
    }
}

/// セルの移動 / 複製 1 件 (drag&drop の release で確定する形)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LauncherCellMove {
    pub from: LauncherCellKey,
    pub to_row: LauncherRow,
    /// 落とした先の列を **表示順 index** で指す。 空きプレースホルダ列に落とせる
    /// ように id ではなく index で受け、handler が `Song::ensure_scene_at` で
    /// 実体化してから id へ解決する。
    pub to_scene_index: usize,
}

/// ドロップの意味。 既存のクリップ規約そのまま (素で移動 / `Ctrl` でリンクコピー /
/// `Ctrl+Shift` で独立コピー、`docs/plan_clip_share_clone.md`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherDropMode {
    Move,
    CopyLinked,
    CopyIndependent,
}

/// MIDI Learn で受ける入力の種類。 パッドはノートで撃つので **CC だけでは
/// 足りない** (計画書 §3.5)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherBindInput {
    /// ノート番号 0..=127 (note-on で発火 / note-off で離す)。
    Note(u8),
    /// CC 番号 0..=127 (値 >= 64 で押下、 < 64 で離す)。
    ControlChange(u8),
}

/// パッドから撃てるランチャー操作 (= `BindingTarget` へ足してほしい variant)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherBindTarget {
    /// 行 × 列 でセルを 1 つ指す。**セルの id ではなく `(track_id, scene_id)`**
    /// にしてあるのは、パッドの物理位置が「このトラックのこの列」に対応するから
    /// (セルを差し替えても同じパッドが新しいセルを撃つ)。
    LaunchCell { track_id: u32, scene_id: u32 },
    LaunchScene { scene_id: u32 },
    StopRow { track_id: u32 },
    StopAllRows,
    SwitchRowToArranger { track_id: u32 },
    SwitchAllToArranger,
}

impl LauncherBindTarget {
    /// Learn ボタン / status 表示に出す日本語ラベル。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::LaunchCell { .. } => "セルを撃つ",
            Self::LaunchScene { .. } => "シーンを撃つ",
            Self::StopRow { .. } => "行を止める",
            Self::StopAllRows => "全行を止める",
            Self::SwitchRowToArranger { .. } => "行をアレンジへ戻す",
            Self::SwitchAllToArranger => "全行をアレンジへ戻す",
        }
    }
}

/// MIDI 入力 → ランチャー操作の binding 1 件。
///
/// **`Song.midi_bindings` へ移すまでの暫定の置き場**
/// (`MidiBinding` が CC 専用なので今は載らない)。 読み書きは
/// `AppData::launcher_bindings` / `add_launcher_binding` /
/// `clear_launcher_bindings` の 3 本だけ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LauncherBinding {
    /// MIDI channel 0..=15、`16` = any-channel (既存 `MidiBinding` と同じ規約)。
    pub channel: u8,
    pub input: LauncherBindInput,
    pub target: LauncherBindTarget,
}

impl LauncherBinding {
    /// 受信した MIDI がこの binding に当たるか。
    #[must_use]
    pub fn matches(self, channel: u8, input: LauncherBindInput) -> bool {
        (self.channel == channel || self.channel == 16) && self.input == input
    }
}

/// 束 B が `common::protocol::AudioCommand` へ足すまでの **送信の形**。
///
/// engine 側の実装はこの 7 つを受ければ足りる (計画書 §2.2)。 protocol に
/// 入ったら [`AppData::send_launcher_audio`](crate::state::AppData::send_launcher_audio)
/// の中身を `self.send_audio(AudioCommand::…)` に差し替えるだけで、
/// 呼び出し側 (handler) は 1 行も変わらない。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LauncherAudioCommand {
    /// セルを撃つ / 離す。`pressed` の解釈 (Trigger / Gate / Toggle / Repeat) は
    /// engine 側がセルの [`LaunchMode`] を見て行う。
    LaunchCell { row: LauncherRow, clip_id: u32, pressed: bool },
    /// 列を丸ごと撃つ。空セルの行は停止 (計画書 Q11)。
    LaunchScene { scene_id: u32, pressed: bool },
    StopRow { row: LauncherRow },
    StopAllRows,
    SwitchRowToArranger { row: LauncherRow },
    SwitchAllToArranger,
    SetGlobalLaunchQuantize(LaunchQuantize),
}

/// ランチャーの GUI イベント。 発生源は 4 つ — ランチャー widget (束 C) /
/// インスペクタ / トランスポート・メニュー・ショートカット / MIDI。
/// **どれも同じ入口 (`AppEvent::Launcher`) を通る**ので、
/// 「セルを撃つ」 の意味が発生源ごとに分岐しない。
#[derive(Debug, Clone, PartialEq)]
pub enum LauncherEvent {
    // ---- 発火 (計画書 §3.1 / §5) --------------------------------------
    /// セルを撃つ / 離す。`Song` 側は「ユーザーが最後に撃った状態」だけを
    /// 書き換える (走行位置は engine が持つ、計画書 §1.4)。
    LaunchCell { cell: LauncherCellKey, pressed: bool },
    /// 列 (シーン) を撃つ / 離す。
    LaunchScene { scene_id: u32, pressed: bool },
    /// 行の Stop Clips (ランチャーが握ったまま無音)。
    StopRow { row: LauncherRow },
    /// 全行の Stop Clips。
    StopAllRows,
    /// 行をアレンジ主導へ戻す (Switch Playback to Arranger)。
    RowToArranger { row: LauncherRow },
    /// 全行をアレンジ主導へ戻す (トランスポートのボタン)。
    AllToArranger,
    /// トランスポートのグローバルローンチ量子化。
    SetGlobalQuantize(LaunchQuantize),

    // ---- 表示 (計画書 Q5 / Q5-b) --------------------------------------
    SetLayout(common::model::LauncherLayout),
    /// `Ctrl+Tab` / View メニュー: 両方 → ランチャーのみ → アレンジのみ。
    CycleLayout,

    // ---- 選択とフォーカス ---------------------------------------------
    /// セル本体クリック = 選択 (修飾キーは既存のクリップ規約どおり)。
    SelectCell { cell: LauncherCellKey, modifier: SelectModifier },
    /// キーボード操作の起点を置く (セル click / 矢印移動の着地点)。
    /// 空セルにも置けるので id ではなく **行 + 列 index**。
    FocusCell { row: LauncherRow, scene_index: usize },
    /// ポインタが乗っているセル (widget が毎フレーム更新、`None` = 帯の外)。
    /// **貼り付け先の解決**に使う — arrangement の `arrange_hovered_track` +
    /// `arrangement_hover_beat` と同じ役割。
    SetHover(Option<(LauncherRow, usize)>),
    /// 矢印キー。`dx` = 列方向 / `dy` = 行方向 (下が正)。
    MoveFocus { dx: i32, dy: i32 },
    /// `Enter`: フォーカス中のセルを撃つ (空セルならその行を止める)。
    LaunchFocused,

    // ---- 列 (シーン) の CRUD (計画書 §6) ------------------------------
    AddScene,
    DeleteScenes(Vec<u32>),
    /// 表示順の並べ替え (見出しのドラッグ)。
    MoveScene { scene_id: u32, to_index: usize },
    SetSceneColor { scene_id: u32, color: [f32; 3] },
    BeginRenameScene(u32),
    RenameSceneChanged(String),
    CommitRenameScene,
    CancelRenameScene,
    /// 「今鳴っているセルを新しいシーンとして取り込む」 (Capture)。
    CaptureScene,
    /// 列 (シーン) のフォローアクション編集。
    SetSceneFollow { scene_ids: Vec<u32>, edit: LaunchEdit },

    // ---- セルの CRUD (計画書 §6) --------------------------------------
    /// 空セルに空クリップを作る (ダブルクリック / ペースト先の実体化)。
    /// プレースホルダ列なら `Song::ensure_scene_at` で列も実体化する。
    CreateCell { row: LauncherRow, scene_index: usize },
    DeleteCells(Vec<LauncherCellKey>),
    /// `unique = false` でリンク複製 (content 共有) / `true` で独立複製。
    /// 複製先は同じ行の **右隣の空き列**。
    DuplicateCells { cells: Vec<LauncherCellKey>, unique: bool },
    /// ドラッグの release で確定する移動 / コピー。
    MoveCells { moves: Vec<LauncherCellMove>, mode: LauncherDropMode },

    // ---- ローンチ設定 (計画書 §3.4) ------------------------------------
    SetLaunchSettings { cells: Vec<LauncherCellKey>, edit: LaunchEdit },

    // ---- MIDI (計画書 §3.5) --------------------------------------------
    /// 次に届いたノート / CC をこの操作へ bind する。
    StartLearn(LauncherBindTarget),
    CancelLearn,
    /// bind 表を空にする (インスペクタの「割り当てを消す」)。
    ClearBindings,
}

impl LauncherEvent {
    /// r.md #29: この event が undo step を積んだときの履歴ラベル。
    /// `AppEvent::undo_label` から委譲される。
    #[must_use]
    pub fn undo_label(&self) -> &'static str {
        use LauncherEvent as E;
        match self {
            E::LaunchCell { .. } | E::LaunchScene { .. } | E::LaunchFocused => "セルを撃つ",
            E::StopRow { .. } | E::StopAllRows => "ランチャーを止める",
            E::RowToArranger { .. } | E::AllToArranger => "アレンジに戻す",
            E::AddScene | E::CaptureScene => "シーン追加",
            E::DeleteScenes(..) => "シーン削除",
            E::MoveScene { .. } => "シーン並べ替え",
            E::SetSceneColor { .. } => "シーン色変更",
            E::CommitRenameScene => "シーン名変更",
            E::CreateCell { .. } => "セル作成",
            E::DeleteCells(..) => "セル削除",
            E::DuplicateCells { .. } => "セル複製",
            E::MoveCells { .. } => "セル移動",
            E::SetLaunchSettings { .. } => "ローンチ設定",
            E::SetSceneFollow { .. } => "フォローアクション",
            // 非編集 (表示 / 選択 / MIDI learn) は snapshot を積まないので
            // ラベルは記録されない。
            _ => "編集",
        }
    }
}
