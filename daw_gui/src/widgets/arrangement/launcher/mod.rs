//! r.md #87 クリップランチャー (セッションビュー) の帯。
//!
//! アレンジのレーンの **左**に「トラック = 行 / シーン = 列」の格子を 1 本足す
//! (`docs/plan_rmd_87_clip_launcher.md` §3.1-3.3)。
//!
//! ```text
//! +----------+-+-----------------------+-+---------------------------+
//! |          |#| ▶Scene1 ▶Scene2 ...   |=| ルーラー / Arranger 帯     |
//! +----------+-+-----------------------+-+---------------------------+
//! | Kick  SM |#|  .   ▶Kick    .       |=|    [Kick#1]  [Kick#2]     |
//! +----------+-+-----------------------+-+---------------------------+
//!   ヘッダ   停止    セル格子(シーン=列)  返す      アレンジのレーン
//! ```
//!
//! **行の縦位置は `ArrangementFrame::tops` をそのまま使う。** ヘッダ / レーンと
//! 同じ 1 本を共有するので、行ズレは構造的に起きない (`for_each_visible_lane` も
//! 同じ helper を通す)。行を引くキーは [`ArrangementRowKey`] (arrangement の行識別と
//! 同一の語彙) で、[`LauncherView::rows`] は `Vec` ではなく **map** にしてある —
//! 並びを別に持つと「行の順序が 2 か所にある」状態になり、必ずどこかでズレる。
//!
//! **widget は `Song` を書かない / `AudioCommand` も送らない。** 発火・停止・セルの
//! 作成/移動/選択はすべて [`LauncherIntent`] として [`LauncherResponse::intents`] に
//! 積み、caller (束 D) が `AppEvent` へ翻訳する。ここで直接書くのは
//! **「見方の都合」の view 状態だけ** (`ui_prefs.launcher_*`。帯幅 / 列幅 /
//! 横スクロール / レイアウト。`*` は立たない = `project_dirty_flag_rule`)。

use std::collections::{HashMap, HashSet};

use common::model::{LauncherLayout, RowPlayback};

use super::*;

/// 未確定の矩形 (`Rect` は `Default` を持たない — 「原点にある 0x0」を暗黙の既定に
/// させない設計なので、必要な側が明示する)。
pub(super) const ZERO_RECT: Rect = Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 };

pub(super) mod build;
pub(super) mod draw;
pub(super) mod drag;
pub(super) mod layout;
pub(super) mod press;
pub(super) mod release;

// 帯の幾何 / 標識のコントラスト / グループ展開の検査。**帯を畳んだら戻せない**
// のような「一度踏むと直すまで使えない」欠陥を機械で止めるのが主目的。
#[cfg(test)]
mod tests;

// ============================================================
// 寸法定数
// ============================================================

/// 帯を畳みきったとき / 広げきったときに **必ず残す**つかみ代 (px)。
///
/// 左端 (= アレンジのみ) でも右端 (= ランチャーのみ) でもこの幅は残るので、
/// スプリッタが画面から消えて戻せなくなることが構造的に起きない
/// (計画書 Q5「どちらの端でも 6px のつかみ代を必ず残す」)。
///
/// **[`PANE_SPLITTER_HANDLE`] と同じ値**。スプリッタのホットゾーンは境界の
/// **左側だけ**に張るので (下記)、畳んだ帯そのものがちょうど掴み代になる。
/// ヘッダ境界のスプリッタが左から 4px 食う (`header_resize_handle_px / 2`) ので、
/// 実際に帯側だけで掴める幅は 8px 残る = 要求の 6px を満たす。
pub(super) const GRAB_W: f32 = 12.0;

/// 帯の右端スプリッタのホットゾーン幅 (px)。**境界の左側にだけ**張る。
///
/// 帯の右端は **アレンジのレーンの左端でもある**。ヘッダ境界のスプリッタのように
/// 中心対称に張ると、拍 0 のクリップ / オートメーション点がスプリッタに食われて
/// 掴めなくなる。既存のヘッダ境界が同じ罠を持っているが、それを 1 つ増やす理由は無い。
pub(super) const PANE_SPLITTER_HANDLE: f32 = 12.0;

/// 停止列の幅 (px)。各行の Stop Clips ボタン 1 個ぶん。
pub(super) const STOP_COL_W: f32 = 16.0;

/// 「アレンジへ返す」列の幅 (px)。Bitwig は最後のスロットの右に置く。
pub(super) const RETURN_COL_W: f32 = 16.0;

/// `ui_prefs.launcher_width` が未設定 (`<= 0`) のときの帯幅 (px)。
pub(super) const DEFAULT_PANE_W: f32 = 300.0;

/// `ui_prefs.launcher_scene_col_w` が未設定 (`<= 0`) のときの列幅 (px)。
pub(super) const DEFAULT_COL_W: f32 = 96.0;

/// 列幅の下限 / 上限 (px)。
pub(super) const MIN_COL_W: f32 = 36.0;
pub(super) const MAX_COL_W: f32 = 400.0;

/// 横スクロールで到達できる列の上限 (表示 index)。プレースホルダ列は無限に続くが、
/// engine 側のフォローアクションが見る列数の上限 (`daw_audio` の `MAX_SCENES` = 512) と
/// 揃えて、そこより先へは掴めないようにする。
pub(super) const MAX_SCROLL_SCENES: f32 = 512.0;

/// 「両方」レイアウトが成立する最小の面幅 (px)。帯側は
/// 「停止列 + 列 1 本 + 返す列 + つかみ代」で、アレンジ側にも同じ値を最低幅として要求する。
///
/// **これを下回る幅は「両方」ではなく端に吸着した状態**として扱う (`drag::emit_pane_width`)。
/// そうしないと、スプリッタを端まで引く途中の「格子が 1 列も入らない幅」が
/// `ui_prefs.launcher_width` に書き込まれ、`Tab` で「両方」へ戻したとき掴み代だけの帯が
/// 出る = 計画書 Q5-b「『両方』の比率は覚えている」が成り立たない。
/// [`layout::resolve_pane_w_raw`] も同じ下限で clamp するので、壊れた `ui_prefs` を
/// 読み込んでも「両方」は必ず格子を描ける。
pub(super) const MIN_BOTH_PANE_W: f32 =
    STOP_COL_W + MIN_COL_W + RETURN_COL_W + PANE_SPLITTER_HANDLE;

/// セル左端の ▶ (発火ボタン) の一辺 (px)。行が低いときは行高で頭打ちにする。
pub(super) const LAUNCH_BTN_W: f32 = 14.0;

/// セル / シーン見出しの角丸 (px)。
pub(super) const CELL_RADIUS: f32 = 3.0;

/// 走行中セルの進捗バーの高さ (px)。
pub(super) const PROGRESS_BAR_H: f32 = 3.0;

/// 走行中セルを横切るプレイヘッド線の太さ (px)。アレンジのプレイヘッドと同じ
/// 意味の標識なので、細すぎて見失わない太さにする。
pub(super) const PLAYHEAD_W: f32 = 2.0;

/// ドラッグを「移動」と認めるまでの移動量 (px)。これ未満は click 扱い。
pub(super) const CELL_DRAG_SLOP_PX: f32 = 4.0;

// ============================================================
// caller 向けの型
// ============================================================

/// セル 1 つの identity。
///
/// `row` は arrangement の行識別 ([`ArrangementRowKey`]) そのもの。トラック行のセルは
/// `ClipKey { track, clip }`、オートメーションレーン行のセルは
/// `AutomationClipKey { track, lane, clip }` に落とせる ([`Self::clip_key`] /
/// [`Self::automation_clip_key`])。
///
/// **`scene_index` はこのフレーム限りの表示位置**で、永続参照には使わない
/// (アーキテクチャ不変条件 1)。実在する列は `scene_id` が真の identity で、
/// `scene_id == 0` は「まだ `Song.scenes` に無いプレースホルダ列」を意味する
/// (そこにセルを置いた瞬間に `Song::ensure_scene_at(scene_index)` で実体化する)。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LauncherCellKey {
    pub row: ArrangementRowKey,
    /// 表示順の列 index (このフレーム限り)。
    pub scene_index: u32,
    /// 列 id。プレースホルダ列は `0`。
    pub scene_id: u32,
    /// セルのクリップ id。空セルは `0`。
    pub clip_id: u32,
}

impl LauncherCellKey {
    /// トラック行のセルなら `ClipKey`。レーン行 / 空セルは `None`。
    #[must_use]
    pub fn clip_key(self) -> Option<ClipKey> {
        match self.row {
            ArrangementRowKey::Track(track) if self.clip_id != 0 => {
                Some(ClipKey { track_id: track, clip_id: self.clip_id })
            }
            _ => None,
        }
    }

    /// レーン行のセルなら `AutomationClipKey`。トラック行 / 空セルは `None`。
    #[must_use]
    pub fn automation_clip_key(self) -> Option<AutomationClipKey> {
        match self.row {
            ArrangementRowKey::Lane(l) if self.clip_id != 0 => {
                Some(AutomationClipKey { track: l.track, lane: l.lane, clip: self.clip_id })
            }
            _ => None,
        }
    }

    /// 空セルか。
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.clip_id == 0
    }

    /// 選択集合が使う **モデル側の鍵** ([`crate::event_launcher::LauncherCellKey`])。
    /// 空セル (`clip_id == 0`) は選択できないので `None`。
    #[must_use]
    pub(super) fn model_key(self) -> Option<crate::event_launcher::LauncherCellKey> {
        use crate::event_launcher::LauncherCellKey as Model;
        if let Some(k) = self.clip_key() {
            return Some(Model::Track(k));
        }
        // widget 層の `AutomationClipKey` はモデルと別型なので既存の変換を通す。
        self.automation_clip_key().map(|k| Model::Lane(widget_to_model_clip_key(k)))
    }
}

/// クリップを運ぶときの修飾キーの意味 (`docs/plan_clip_share_clone.md` そのまま)。
/// 素で移動 / `Ctrl` でリンクコピー / `Ctrl+Shift` で独立コピー。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClipCopyMode {
    Move,
    CloneLinked,
    CloneIndependent,
}

impl ClipCopyMode {
    /// drag session が持ち回った最終 modifier から解決する
    /// (release フレームの `pointer.modifiers` 生読みは ModifiersChanged 先行 race で落ちる)。
    #[must_use]
    pub(super) fn from_modifiers(ctrl: bool, shift: bool) -> Self {
        match (ctrl, shift) {
            (true, true) => Self::CloneIndependent,
            (true, false) => Self::CloneLinked,
            _ => Self::Move,
        }
    }
}

/// セルの行き先 1 件 (ランチャー内の移動 / 複製)。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct LauncherCellMove {
    pub from: LauncherCellKey,
    /// 行き先の行。
    pub to_row: ArrangementRowKey,
    /// 行き先の列 (**表示 index**)。プレースホルダ列も指せるので id では持たない
    /// — 実体化は handler の `Song::ensure_scene_at` が行う (住所を 2 つ運ばない)。
    pub to_scene_index: u32,
}

/// アレンジ側で掴んだクリップ。トラック行の MIDI / オーディオ ([`ClipKey`]) と、
/// オートメーションレーン行のクリップ ([`AutomationClipKey`]) — セルもこの 2 種
/// ([`LauncherRow`](crate::event_launcher::LauncherRow) の Track / Lane) なので、
/// 帯へ落とせるのはどちらも同じ。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArrangementClipRef {
    Track(ClipKey),
    Lane(AutomationClipKey),
}

/// アレンジのクリップをセルへ落とした 1 件。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ClipToCellDrop {
    pub from: ArrangementClipRef,
    pub to_row: ArrangementRowKey,
    pub to_scene_index: u32,
}

/// セルをアレンジのレーンへ落とした 1 件。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct CellToClipDrop {
    pub from: LauncherCellKey,
    /// 行き先のトラック行 (レーン行への drop は `ArrangementRowKey::Lane`)。
    pub to_row: ArrangementRowKey,
    /// 行き先の開始拍 (snap 適用済)。
    pub to_start_beat: f64,
}

/// ランチャー帯がこのフレームに起こした「意図」。
///
/// **widget はこれを `ArrangementResponse` に載せるだけで、`Song` も engine も触らない。**
/// 実際の `AudioCommand` / `AppEvent` への翻訳は束 D (`daw_gui/src/handler/**`) の担当。
/// 発生順に並ぶ (同一フレーム内で複数出ることがある = 押下と離しは別フレーム)。
#[derive(Clone, PartialEq, Debug)]
pub enum LauncherIntent {
    /// セルの ▶ を押した / 離した。
    ///
    /// **空セル (`cell.clip_id == 0`) は「その行を止める」** (計画書 Q11)。アーム中の
    /// 行なら録音開始として解釈してよい (表示も ● になっている)。
    ///
    /// **グループ行は widget 側で子行へ展開済み** — この列挙に
    /// `row = Track(グループ id)` が来ることは無い (グループトラックは自分の
    /// クリップを鳴らさないので、まとめセルは「子を一斉に」の意味しか持たない)。
    /// `pressed` は
    /// [`LaunchMode`](common::model::LaunchMode) の 4 種 (Trigger / Gate / Toggle /
    /// Repeat) を engine が解釈するために要る。
    Launch { cell: LauncherCellKey, pressed: bool },
    /// シーン見出しの ▶ を押した / 離した (その列を一斉発火)。
    LaunchScene { scene_id: u32, pressed: bool },
    /// 行の Stop Clips (停止列)。
    StopRow(ArrangementRowKey),
    /// 全行の Stop Clips (停止列の上端)。
    StopAllRows,
    /// 行をアレンジへ返す (返す列)。
    SwitchRowToArranger(ArrangementRowKey),
    /// 全行をアレンジへ返す (返す列の上端)。
    SwitchAllToArranger,
    /// セル本体クリックによる選択。`additive` = `Ctrl`、`range` = `Shift`。
    SelectCell { cell: LauncherCellKey, additive: bool, range: bool },
    /// シーン見出しの本体を **短くクリック**した = 列 (シーン) の選択。
    /// 動かせば並べ替え (`ReorderScene`) になるので、セル本体の drag → 選択への
    /// 格下げと同じ閾値・同じ修飾キー規約を通る。
    SelectScene { scene_id: u32, additive: bool, range: bool },
    /// 空セルのダブルクリック → 空クリップを作る。
    /// `scene_index` がプレースホルダ列を指していたら `Song::ensure_scene_at` で実体化する。
    CreateCell { row: ArrangementRowKey, scene_index: u32 },
    /// クリップのあるセルのダブルクリック → ピアノロール / オーディオエディタを開く。
    OpenCellEditor(LauncherCellKey),
    /// セルを別の行 / 列へ運んだ。
    MoveCells { moves: Vec<LauncherCellMove>, mode: ClipCopyMode },
    /// アレンジのクリップをセルへ落とした。
    DropClipsToCells { drops: Vec<ClipToCellDrop>, mode: ClipCopyMode },
    /// セルをアレンジのレーンへ落とした。
    DropCellsToArranger { drops: Vec<CellToClipDrop>, mode: ClipCopyMode },
    /// シーン列を並べ替えた (`scene_id` を表示順 `to_index` へ)。
    ReorderScene { scene_id: u32, to_index: u32 },
}

/// `ArrangementResponse` に載せるランチャー帯の戻り値。
#[derive(Clone, Debug)]
pub struct LauncherResponse {
    /// 帯全体の実 rect (幅 0 = 非表示)。
    pub pane_rect: Rect,
    /// セル格子の実 rect (停止列 / 返す列を除いた本体)。
    pub grid_rect: Rect,
    /// 各セルの rect (描画順 = 上から下・左から右)。 caller が `context_menu_for` /
    /// inline rename overlay を重ねる用 (`clip_rects` と同 semantics)。
    /// **空セル / プレースホルダ列も含む** (右クリックで「セルを作る」を出せるように)。
    ///
    /// **格子 ([`Self::grid_rect`]) でクリップ済** — 部分表示の列 / 行の rect が
    /// 「アレンジへ返す」ボタンやシーン見出しの上まで伸びない。
    /// **セルを所有できない行 (マスター行 / グループ行) は入らない**
    /// (`layout::row_takes_cells`。落とし先の除外と同じ 1 本を通す)。
    pub cell_rects: Vec<(LauncherCellKey, Rect)>,
    /// 帯に積んだ **全部の行** の y 帯 (`(row, 格子でクリップ済 rect)`、描画順)。
    ///
    /// `cell_rects` と違い **セルを置けない行 (マスター行 / グループ行 /
    /// テンポ・拍子レーン行) も入る**。 caller (ファイル drop の落とし先解決) が
    /// 「行が 1 つも無い下の余白」と「セルを置けない行」を区別するのに要る —
    /// 潰すと、グループ行へ落としたファイルが「一番下に新トラックを作る」に化ける
    /// (`cell_rects` はセルの上下インセットで行間に 4px の隙間も空くので、
    /// 行と行の境目に落としただけで同じ事故になる)。
    pub row_bands: Vec<(ArrangementRowKey, Rect)>,
    /// シーン見出しの rect (`(scene_id, 表示順 index, rect)`)。
    /// **プレースホルダ列 (`scene_id == 0`) も含む** — 右クリックで
    /// 「ここに列を作る」を出すために index が要る。
    pub scene_rects: Vec<(u32, u32, Rect)>,
    /// 列 1 本の幅 (px) と横スクロール位置 (列数、小数可)。
    /// **caller が「セルの rect に当たらなかった点」の列を解く**のに要る
    /// (ファイル drop はセルの隙間にも、行が 1 つも無い余白にも落ちる)。
    pub col_w: f32,
    pub scroll_scene: f32,
    /// ポインタ下のセル。
    pub hovered_cell: Option<LauncherCellKey>,
    /// このフレームに発生した意図 (発生順)。
    pub intents: Vec<LauncherIntent>,
}

impl Default for LauncherResponse {
    fn default() -> Self {
        Self {
            pane_rect: ZERO_RECT,
            grid_rect: ZERO_RECT,
            cell_rects: Vec::new(),
            row_bands: Vec::new(),
            scene_rects: Vec::new(),
            col_w: DEFAULT_COL_W,
            scroll_scene: 0.0,
            hovered_cell: None,
            intents: Vec::new(),
        }
    }
}

// ============================================================
// 1 フレーム分のビュー
// ============================================================

/// 1 セル (クリップのある側) の描画情報。
#[derive(Clone, Debug)]
pub(super) struct LauncherCellView {
    /// 行内で一意なクリップ id (arrangement のクリップと同じ id 空間)。
    pub clip_id: u32,
    pub name: Arc<str>,
    /// 実塗り色 (トラック色 / クリップ色の解決済み値)。
    pub color: Color,
    pub muted: bool,
    /// 共有 (linked) セル。名前の左に `⇌` を描く。
    pub linked: bool,
    /// r.md #91: 共有グループ「連動ハイライト」 (`ArrangementClip::in_active_group` と同じ契約)。
    /// `true` かつ `linked` のとき、アレンジのクリップと同じ glow wash + 明るい枠を重ねる
    /// (= 「今 hover / 選択している共有グループの member」)。caller が
    /// `{選択 clip / セル} ∪ {hover 中の clip / セル}` の content 集合から毎フレーム立てる。
    /// hover 由来で毎フレーム変わるが、帯は cached を張らないので cache key の心配は無い。
    pub in_active_group: bool,
    /// このセルにフォローアクションが設定されている (▶ を縞にする)。
    pub follow: bool,
    /// 中身の窓 (`Clip::content_offset_beats` / `length_beats`)。ミニ表示の写像に使う。
    pub content_offset_beats: f64,
    pub len_beats: f64,
    /// ループするセルか (`LaunchSettings::looping`)。進捗の位相を解くのに要る
    /// (ワンショットは終端で止まるので、位相の折り返しがない)。
    pub looping: bool,
    /// **オートメーションレーン行のセルだけ**が持つ曲線 (content-local 拍, 正規化値)。
    /// トラック行のセルは空 (中身は波形 / MIDI で、描画時に
    /// [`content_build::build_one`](super::content_build::build_one) から組む)。
    pub curve: Vec<(f64, f32)>,
}

/// ランチャー帯の 1 行。
#[derive(Clone, Debug)]
pub(super) struct LauncherRowView {
    /// 主導権 (アレンジ / ランチャー)。`Song` に保存される持続状態。
    pub playback: RowPlayback,
    /// 録音アーム中か (空セルの記号を ● / ■ に分ける、Bitwig 流)。
    pub armed: bool,
    /// 子を持つトラック行 (= グループ)。まとめセルを描く。
    pub group: bool,
    /// この行を **ランチャーが握れるか** (= engine が行として登録するか)。
    ///
    /// `false` はマスターのトラック行とテンポ / 拍子レーン行
    /// ([`AutomationTarget::accepts_launcher_cells`](common::model::AutomationTarget::accepts_launcher_cells)
    /// が SSoT)。 判定を GUI 側にも持たせるのは同 doc の契約 —
    /// **GUI が描かない / 落とせない / 作れない**ようにしないと、engine が無視する
    /// セルを置けてしまい「保存はされるのに永久に鳴らない」形で残る。
    ///
    /// **グループ行は `true`** (まとめセルを押すと子行を一斉に撃つ) — 「押せるか」と
    /// 「自分のセルを持てるか」は別の問いなので、後者は [`Self::takes_cells`]。
    pub launchable: bool,
    /// この行が **自分のセルを所有できる**か (落とし先 / 作成 / 編集 / rect の対象)。
    ///
    /// 値は model 側の
    /// [`row_accepts_cells`](crate::handler::launcher_cells::row_accepts_cells) を
    /// そのまま写す。**帯が独自の式を持たない**のが肝で、持たせると
    /// 「置く口 (handler) は弾くのに、落とし先 (widget) は受け付ける」がいつか作れる。
    pub takes_cells: bool,
    /// 列 id → セル。空セルはキーが無い。
    pub cells: HashMap<u32, LauncherCellView>,
}

impl LauncherRowView {
    /// この行がランチャーに主導権を渡しているか (鳴っているかは問わない)。
    /// アレンジ側のクリップを減光する判定 (計画書 Q6) もこれ。
    #[must_use]
    pub(super) fn launcher_owns(&self) -> bool {
        self.playback.is_launcher()
    }

    /// いま鳴っているセルの clip id。
    #[must_use]
    pub(super) fn playing_clip_id(&self) -> Option<u32> {
        self.playback.playing_clip_id()
    }
}

/// 1 列 (シーン) の描画情報。
#[derive(Clone, Debug)]
pub(super) struct LauncherSceneView {
    pub id: u32,
    /// 表示名 (未命名は `Scene N`)。
    pub name: Arc<str>,
    pub color: Color,
    /// シーンにフォローアクションが設定されている (▶ を縞にする)。
    pub follow: bool,
    /// この列が選択されている (見出しに選択枠を出す)。
    pub selected: bool,
}

impl LauncherSceneView {
    /// 表示順 `index` の列がまだ `Song.scenes` に無いとき (= 空きプレースホルダ列) の
    /// 見出し。**名前の作り方は実体のある列と同じ**
    /// ([`common::model::Scene::display_name`]) — ここで別の式を書くと、セルを置いて
    /// 実体化した瞬間に見出しの名前が変わってしまう。
    ///
    /// `id == 0` が「まだ実体が無い」印。撃つと全行が停止になる (その列のセルは
    /// 定義上どれも空なので、`Q11` の「空セル = 停止」がそのまま効く)。
    pub(super) fn placeholder(index: usize, color: Color) -> Self {
        Self {
            id: 0,
            name: Arc::from(common::model::Scene::new(0).display_name(index)),
            color,
            follow: false,
            // 実体の無い列は選べない (選択は列 id で持つので `0` は指せない)。
            selected: false,
        }
    }
}

/// `AppData` から派生した 1 フレーム分のランチャービュー。
/// `ArrangementFrame` が借用する ([`BuiltArrangement`](super::view_build::BuiltArrangement)
/// が所有する)。
#[derive(Debug, Default)]
pub(super) struct LauncherView {
    /// 表示順の列。
    pub scenes: Vec<LauncherSceneView>,
    /// 行 (キーは arrangement の行識別)。**並びは持たない** — 並びは
    /// `ArrangementFrame::rows` / `tops` が唯一の SSoT。
    pub rows: HashMap<ArrangementRowKey, LauncherRowView>,
    /// 帯とアレンジのレーンの見せ方。
    pub layout: LauncherLayout,
    /// `LauncherLayout::Both` のときの帯幅 (px、`<= 0` で既定幅)。
    pub width: f32,
    /// 列幅 (px、全列共通)。
    pub col_w: f32,
    /// 横スクロール位置 (列数、小数可)。
    pub scroll_scene: f32,
    /// 走行中セルの進捗 (0..1)。
    ///
    /// **束 B が `common::audio_bridge` の atomic で publish するまで常に空**
    /// (計画書 §1.4: 走行位置は `Song` に入れない)。空のあいだ進捗バーは描かれず、
    /// 「どのセルを握っているか」だけが [`LauncherRowView::playback`] から出る。
    pub progress: HashMap<ArrangementRowKey, f32>,
    /// 量子化境界待ちの予約 (行 → 予約)。engine が publish するまで空。
    pub queued: HashMap<ArrangementRowKey, QueuedView>,
    /// **セル面の選択** (`SelectionState::selected_launcher_cells` の写し)。
    ///
    /// 帯の選択はアレンジのクリップ選択と **別の面**なので、`ArrangementFrame`
    /// の `selected_clips` / `selected_automation_clips` に混ぜない
    /// ([`LauncherSceneView::selected`] と同じ流儀)。混ぜると「アレンジに選択が
    /// あるか」を見ている側 (空きレーンの短 click / 四角ドラッグの `prev`) が
    /// 帯のセルを数えてしまい、四角ドラッグの Shift 追加では**セルの key が
    /// アレンジの automation クリップ選択へ流れ込む**。
    pub selected: Vec<crate::event_launcher::LauncherCellKey>,
}

/// 「押したがまだ鳴っていない」行の予約 ([`LauncherView::queued`])。
///
/// 量子化が 1 小節なら、押してから最大 1 小節ぶん**何も起きない時間**がある。
/// そこを無表示にすると「押せていないのでは」としか見えないので、予約中の
/// セルは点滅し、発火までの残り拍を数字で出す。
#[derive(Clone, Copy, Debug)]
pub(super) struct QueuedView {
    /// 予約されたセルの `clip.id`。`LAUNCHER_QUEUED_STOP` / `_ARRANGER` は
    /// セルではなく「行を止める / アレンジへ返す」の予約 (停止列 / 返す列が光る)。
    pub clip_id: u32,
    /// 発火までの残り拍。**engine が publish した発火拍から引いた値**で、
    /// GUI 側で量子化境界を解き直したものではない
    /// (`common::audio_bridge::LauncherRowState::queued_at_beat_bits` の doc)。
    pub remaining_beats: f64,
}

impl QueuedView {
    /// 予約が「行の停止」か。
    #[must_use]
    pub(super) fn is_stop(self) -> bool {
        self.clip_id == common::audio_bridge::LAUNCHER_QUEUED_STOP
    }

    /// 予約が「アレンジへ返す」か。
    #[must_use]
    pub(super) fn is_arranger(self) -> bool {
        self.clip_id == common::audio_bridge::LAUNCHER_QUEUED_ARRANGER
    }

    /// 残り拍の表示文字列 (小数 1 桁、負なら `0.0`)。
    #[must_use]
    pub(super) fn label(self) -> String {
        format!("{:.1}", self.remaining_beats.max(0.0))
    }
}

// ============================================================
// widget state (drag session)
// ============================================================

/// 帯の右端スプリッタ (帯幅) の drag。`HeaderResizeDragSession` と同 idiom
/// (anchor 固定 + per-frame emit + release は take して捨てる)。
#[derive(Clone, Copy, Debug)]
pub(super) struct PaneWidthDragSession {
    /// drag 開始時の帯幅 (px)。
    pub anchor_pane_w: f32,
    pub anchor_mouse_x: f32,
    /// 最後に書き込んだ幅 (同値書き込みの抑制、0.5px 閾値)。
    pub last_emitted_w: f32,
}

/// シーン見出しの境界を掴んだ列幅 drag (全列共通)。
#[derive(Clone, Copy, Debug)]
pub(super) struct ColWidthDragSession {
    pub anchor_col_w: f32,
    pub anchor_mouse_x: f32,
    pub last_emitted_w: f32,
}

/// シーン見出しを掴んだ列の並べ替え drag。
#[derive(Clone, Copy, Debug)]
pub(super) struct SceneReorderSession {
    pub scene_id: u32,
    /// drag 開始時の表示 index。
    pub anchor_index: u32,
    pub anchor_mouse: (f32, f32),
    pub last_mouse: (f32, f32),
    /// 短クリックを**列の選択**へ格下げするときの修飾キー (セルの drag と同 idiom)。
    /// release フレームでは更新しない (`drag` module doc の race)。
    pub last_ctrl: bool,
    pub last_shift: bool,
}

/// セルを掴んだ drag (ランチャー内の移動 / 複製 / アレンジへの持ち出し)。
#[derive(Clone, Debug)]
pub(super) struct CellDragSession {
    /// 掴んだセル。
    pub primary: LauncherCellKey,
    /// 運ぶセル群 (掴んだセルが選択に含まれていれば選択全部、そうでなければ 1 つ)。
    pub cells: Vec<LauncherCellKey>,
    pub anchor_mouse: (f32, f32),
    pub last_mouse: (f32, f32),
    /// drag 中の最終 ctrl / shift (`ClipDragSession.last_ctrl` と同じ持ち回し方 —
    /// release フレームで `pointer.modifiers` を生読みすると ModifiersChanged
    /// 先行 race で修飾が落ちる)。
    pub last_ctrl: bool,
    pub last_shift: bool,
    pub last_alt: bool,
}

/// ▶ / 停止 / 返す ボタンの押下。`LaunchMode::Gate` の「離すと停止」を出すために、
/// 離しのフレームまで何を押していたかを覚えておく。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LauncherButton {
    Cell(LauncherCellKey),
    Scene(u32),
}

/// ランチャー帯の widget state。`ArrangementState` が 1 フィールドで持つ。
#[derive(Debug, Default)]
pub(crate) struct LauncherState {
    pub(super) pane_width_drag: Option<PaneWidthDragSession>,
    pub(super) col_width_drag: Option<ColWidthDragSession>,
    pub(super) scene_reorder: Option<SceneReorderSession>,
    pub(super) cell_drag: Option<CellDragSession>,
    /// 押しっぱなしのボタン (離しで `pressed: false` を出す相手)。
    pub(super) held_button: Option<LauncherButton>,
    /// press / release が積んだ意図。`release::commit` が `ArrangementResponse` へ移す。
    ///
    /// press ブロック中は `widget_state` を借りていて `push_edit` も `response` への
    /// 書き込みもできないので、いったんここへ貯める (`PressActions` と同じ理由)。
    pub(super) pending_intents: Vec<LauncherIntent>,
}

/// このフレームに生きている / 離した session (`LiveSessions` / `ReleasedSessions` と同 idiom)。
#[derive(Default)]
pub(super) struct LauncherSessions {
    pub live_cell_drag: Option<CellDragSession>,
    pub live_scene_reorder: Option<SceneReorderSession>,
    pub released_cell_drag: Option<CellDragSession>,
    pub released_scene_reorder: Option<SceneReorderSession>,
    pub released_button: Option<LauncherButton>,
    /// **いま押されたままのボタン** (押下フィードバックの描画用)。
    ///
    /// `released_button` は離した瞬間の 1 フレームしか出ないので、これが無いと
    /// 「押している間の見た目」を作れない — 量子化待ちのあいだ画面が 1px も
    /// 変わらず、押せたかどうかが分からない。
    pub live_held_button: Option<LauncherButton>,
}
