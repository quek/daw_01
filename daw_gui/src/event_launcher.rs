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
//! - **engine への送信**: [`LauncherAudioCommand`] を
//!   [`AppData::send_launcher_audio`](crate::state::AppData::send_launcher_audio) が
//!   `common::protocol::AudioCommand` へ 1:1 で写す (送信側はその 1 関数に閉じている)。
//! - **MIDI 割り当て**: 表は `Song.midi_bindings` **1 本**で、連続値のパラメータ (CC) と
//!   ランチャー操作 (ノート / CC) が同じ場所に載る (`common::model::MidiBindInput` /
//!   `BindingTarget`)。読み書きは `AppData::launcher_bindings` /
//!   `add_launcher_binding` / `clear_launcher_bindings` の 3 本だけを通す。

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
            // 行動を選んだら有効にする (EQ / Comp のノブと同じ「触ったら ON」)。「次のセル」を
            // 選んだのに別の 有効 トグルを押すまで何も起きない、が「効かない」に見えていた。
            Self::FollowA(k) => {
                f.a = k;
                if k != FollowActionKind::NoAction {
                    f.enabled = true;
                }
            }
            Self::FollowB(k) => {
                f.b = k;
                if k != FollowActionKind::NoAction {
                    f.enabled = true;
                }
            }
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

/// アレンジ側で掴んだクリップ。トラック行の MIDI / オーディオと、オートメーション
/// レーン行のクリップ — セルの 2 種 ([`LauncherCellKey`]) と 1:1 で、帯へ落とせるのは
/// どちらも同じ (行き先は同じ種別の行だけ)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrangementClipRef {
    Track(ClipKey),
    Lane(AutomationClipKey),
}

/// アレンジのクリップを**セルへ**落とした 1 件。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipToCellDrop {
    /// 掴んだアレンジのクリップ。
    pub from: ArrangementClipRef,
    pub to_row: LauncherRow,
    /// [`LauncherCellMove::to_scene_index`] と同じ理由で **表示順 index**。
    pub to_scene_index: usize,
}

/// セルを**アレンジのレーンへ**落とした 1 件。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellToArrangerDrop {
    pub from: LauncherCellKey,
    pub to_row: LauncherRow,
    /// 落とした先の開始拍 (widget が snap 済で渡す)。
    pub to_start_beat: f64,
}

/// ドロップの意味。 既存のクリップ規約そのまま (素で移動 / `Ctrl` でリンクコピー /
/// `Ctrl+Shift` で独立コピー、`docs/plan_clip_share_clone.md`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherDropMode {
    Move,
    CopyLinked,
    CopyIndependent,
}

/// handler が engine へ送るランチャー操作の **GUI 側の形**。
///
/// [`AppData::send_launcher_audio`](crate::state::AppData::send_launcher_audio) が
/// `common::protocol::AudioCommand` へ 1:1 で写す。行を `LauncherRow` (GUI の語彙) の
/// まま持てるので、呼び出し側が `(track_id, lane_id)` へ潰す必要がない。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LauncherAudioCommand {
    /// セルを撃つ / 離す。`pressed` の解釈 (Trigger / Gate / Toggle / Repeat) は
    /// engine 側がセルの [`LaunchMode`] を見て行う。
    LaunchCell { row: LauncherRow, clip_id: u32, pressed: bool },
    /// セルを `phase_beats` (セルの `start_beat` からの拍) の位置から鳴らす。
    /// [`LaunchMode`] は見ない (押下 / 離しの対を持たない操作)。
    LaunchCellFrom { row: LauncherRow, clip_id: u32, phase_beats: f64 },
    /// セルを鳴らしている全行を、それぞれのセル内の拍 `phase_beats` へ揃える。
    RephaseRows { phase_beats: f64 },
    /// 列を丸ごと撃つ。空セルの行は停止 (計画書 Q11)。
    LaunchScene { scene_id: u32, pressed: bool },
    StopRow { row: LauncherRow },
    StopAllRows,
    SwitchRowToArranger { row: LauncherRow },
    SwitchAllToArranger,
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
    /// ピアノロールの `f` = **全体をセル内の拍 `phase_beats` から再生**。
    /// 3 つを同時に行う:
    /// - `cell` をその拍から撃つ ([`LauncherAudioCommand::LaunchCellFrom`]。`Song` 側は
    ///   [`Self::LaunchCell`] と同じく「最後に撃った状態」だけを書く。[`LaunchMode`] は
    ///   見ない — Toggle で鳴っているセルを止めたり Gate で握ったりしない)
    /// - セルを鳴らしている他の全行も同じ拍へ揃える ([`LauncherAudioCommand::RephaseRows`])
    /// - song の playhead を **相対 seek** する (鳴っている `cell` の今の周回を基準に、
    ///   その拍に当たる song 位置へ。アレンジは今いる場所の近くに留まる)。`cell` が
    ///   鳴っていなければ seek せず、停止中なら現在位置から再生を始める
    ///
    /// 量子化はセルの設定に従う。
    PlayFromCellBeat { cell: LauncherCellKey, phase_beats: f64 },
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
    /// `Tab` / View メニュー: 両方 → ランチャーのみ → アレンジのみ。
    CycleLayout,

    // ---- 選択とフォーカス ---------------------------------------------
    /// セル本体クリック = 選択 (修飾キーは既存のクリップ規約どおり)。
    SelectCell { cell: LauncherCellKey, modifier: SelectModifier },
    /// シーン見出しの本体クリック = **列 (シーン) の選択**。
    ///
    /// これが無いと列そのものを指す手段が無く、列のフォローアクションは
    /// 「その列にセルを持つ行を 1 つ選ぶ」経由でしか触れない (= セルの無い列は
    /// 設定できない)。修飾キーの意味はセル / クリップと同じ規約。
    SelectScene { scene_id: u32, modifier: SelectModifier },
    /// セルのダブルクリック = 選択してからそのセルの編集面を開く
    /// (MIDI ならピアノロール / オーディオならオーディオエディタ)。
    /// アレンジのクリップのダブルクリックと**同じ到達先**。
    OpenCellEditor(LauncherCellKey),
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
    /// 表示順 `index` の位置に列を実体化する (プレースホルダ列の右クリック)。
    /// 末尾追加 (`AddScene`) だと「列 5 を右クリックしたのに列 1 が生える」。
    AddSceneAt(usize),
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
    /// アレンジのクリップをセルへ運んだ (帯とレーンを跨ぐドラッグ)。
    DropClipsToCells { drops: Vec<ClipToCellDrop>, mode: LauncherDropMode },
    /// セルをアレンジのレーンへ運んだ。
    DropCellsToArranger { drops: Vec<CellToArrangerDrop>, mode: LauncherDropMode },

    // ---- ローンチ設定 (計画書 §3.4) ------------------------------------
    SetLaunchSettings { cells: Vec<LauncherCellKey>, edit: LaunchEdit },
    /// セルの長さ (= ループ長、拍)。アレンジのクリップは端をドラッグして変えるが、
    /// セルは格子の中の固定サイズなので**掴む端が無い**。インスペクタの数値欄が
    /// 唯一の口 (これが無いと新規セルは既定長のまま変えられず、
    /// 「4 小節書いても 1 小節でループする」になる)。
    SetCellLength { cells: Vec<LauncherCellKey>, beats: f64 },

    // ---- MIDI (計画書 §3.5) --------------------------------------------
    /// 次に届いたノート / CC をこの操作へ bind する。
    StartLearn(common::model::BindingTarget),
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
            // 再生状態だけを書く event は undo step を積まない (`edit_playback`)。
            // ラベルは match の網羅性のためだけに残す。
            E::LaunchCell { .. }
            | E::PlayFromCellBeat { .. }
            | E::LaunchScene { .. }
            | E::LaunchFocused => "セルを撃つ",
            E::StopRow { .. } | E::StopAllRows => "ランチャーを止める",
            E::RowToArranger { .. } | E::AllToArranger => "アレンジに戻す",
            E::AddScene | E::AddSceneAt(..) | E::CaptureScene => "シーン追加",
            E::DeleteScenes(..) => "シーン削除",
            E::MoveScene { .. } => "シーン並べ替え",
            E::SetSceneColor { .. } => "シーン色変更",
            E::CommitRenameScene => "シーン名変更",
            E::CreateCell { .. } => "セル作成",
            E::DeleteCells(..) => "セル削除",
            E::DuplicateCells { .. } => "セル複製",
            E::MoveCells { .. } => "セル移動",
            E::DropClipsToCells { .. } => "セルへ運ぶ",
            E::DropCellsToArranger { .. } => "アレンジへ運ぶ",
            E::SetLaunchSettings { .. } => "ローンチ設定",
            E::SetCellLength { .. } => "セルの長さ",
            E::SetSceneFollow { .. } => "フォローアクション",
            // 非編集 (表示 / 選択 / MIDI learn) は snapshot を積まないので
            // ラベルは記録されない。
            _ => "編集",
        }
    }
}
