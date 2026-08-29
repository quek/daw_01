//! r.md #87 クリップランチャー (セッションビュー) のモデル。
//!
//! ランチャーは **もう 1 本のタイムラインではなく、行ごとに時間軸の供給元を切り替える機構**。
//! 「行」 は arrangement の 1 行と 1:1 で、 通常トラック行 (`Track`) と展開した
//! オートメーションレーン行 (`AutomationLane`) の両方が対象 (`docs/plan_rmd_87_clip_launcher.md` Q4)。
//!
//! - **列** = [`Scene`] (`Song.scenes`、 安定 id)。 Arranger セクション (`Song.sections`) とは
//!   完全に無関係で、 列を足しても曲の長さもクリップ位置も動かない (Q3)。
//! - **セル** = [`SessionClip`] / [`SessionAutomationClip`]。 中身は arrangement と同じ
//!   [`Clip`](crate::model::Clip) / [`AutomationClip`](crate::model::AutomationClip) をそのまま持ち、
//!   `clip.id` は **arrangement のクリップと同じ id 空間** (`Track.next_clip_id` /
//!   `AutomationLane.next_clip_id`) から採る。 これで `ClipKey { track_id, clip_id }` が
//!   そのまま通り、 選択 SSoT / undo / クリップ色 / mute / 共有 content (linked clip) /
//!   ピアノロール / オーディオエディタが **追加実装ゼロ**で効く。
//! - **主導権** = [`RowPlayback`]。 停止しても消えない持続状態で、 `.daw` に保存する
//!   (停止 → 再生で同じセルが鳴り直す / 書き出しもこの状態を反映する、 Q9 / Q10)。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::{AutomationClip, Clip};

/// ランチャーの 1 列。`Song.scenes` に `Vec` 順 = 表示順で保持する
/// (並べ替えは `Vec` 内の move、 参照は常に `id`)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Scene {
    /// Song 内で安定な id (`Song::alloc_scene_id` で採番、`0` は未採番 sentinel —
    /// `Song::ensure_scene_ids` が load 時に採番する)。
    pub id: u32,
    /// 表示名。**空 = 未命名**で、 表示側が `Scene N` を並び順から作る
    /// (自動名を焼き込まないので、 並べ替えても番号が追従する)。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// 列の色 (RGB、不透明)。`None` = パレット既定色。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[f32; 3]>,
    /// シーンのフォローアクション。**走行中のクリップのそれより優先する**
    /// (Live 12 の規則)。
    #[serde(default, skip_serializing_if = "FollowAction::is_default")]
    pub follow: FollowAction,
}

impl Scene {
    #[must_use]
    pub fn new(id: u32) -> Self {
        Self { id, name: String::new(), color: None, follow: FollowAction::default() }
    }

    /// 表示名。未命名なら並び順から `Scene N` を作る (`index` は 0 始まり)。
    #[must_use]
    pub fn display_name(&self, index: usize) -> String {
        if self.name.is_empty() {
            format!("Scene {}", index + 1)
        } else {
            self.name.clone()
        }
    }
}

/// トラック行のセル 1 つ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct SessionClip {
    /// 属する列 ([`Scene::id`])。positional index は使わない (アーキ不変条件 1)。
    pub scene_id: u32,
    /// 中身。`start_beat` は **常に 0** — セルは「撃った瞬間」を原点とするので
    /// song-absolute な配置を持たない。鳴らす窓は
    /// `[content_offset_beats, content_offset_beats + length_beats)` で、
    /// arrangement のクリップと同じ意味 (`docs/plan_clip_content_window.md`)。
    pub clip: Clip,
    #[serde(default, skip_serializing_if = "LaunchSettings::is_default")]
    pub launch: LaunchSettings,
}

/// オートメーションレーン行のセル 1 つ。トラック行のセルと完全に同形。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct SessionAutomationClip {
    pub scene_id: u32,
    /// `start_beat` は常に 0 ([`SessionClip::clip`] と同じ理由)。
    pub clip: AutomationClip,
    #[serde(default, skip_serializing_if = "LaunchSettings::is_default")]
    pub launch: LaunchSettings,
}

/// セルごとのローンチ設定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct LaunchSettings {
    /// 発火の量子化。既定は [`LaunchQuantize::Global`] (= トランスポートの設定に従う)。
    #[serde(default)]
    pub quantize: LaunchQuantize,
    #[serde(default)]
    pub mode: LaunchMode,
    /// `true` = 窓の終端で先頭へ戻って鳴り続ける / `false` = 1 回で止まる (ワンショット)。
    #[serde(default = "default_true")]
    pub looping: bool,
    /// `true` = 直前にその行で鳴っていたセルの**位相を引き継ぐ**
    /// (Live の Legato Mode。同じ小節位置のまま別のループへ乗り換える)。
    #[serde(default)]
    pub legato: bool,
    #[serde(default, skip_serializing_if = "FollowAction::is_default")]
    pub follow: FollowAction,
}

fn default_true() -> bool {
    true
}

impl Default for LaunchSettings {
    fn default() -> Self {
        Self {
            quantize: LaunchQuantize::Global,
            mode: LaunchMode::Trigger,
            looping: true,
            legato: false,
            follow: FollowAction::default(),
        }
    }
}

impl LaunchSettings {
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// 発火の量子化。Live の Launch Quantization と同じ選択肢を持つ。
///
/// **`Bars` を beats へ落とすのに拍子が要る**ので、換算は
/// [`LaunchQuantize::beats`] を唯一の口にすること (`4.0` 決め打ちを散らさない)。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum LaunchQuantize {
    /// トランスポートのグローバル設定に従う (セルの既定)。
    #[default]
    Global,
    /// 量子化しない (押した瞬間に発火)。
    Off,
    /// `n` 小節。1 小節の拍数は拍子から求める。
    Bars(u8),
    /// `div` 分音符。`triplet` で 3 連符 (係数 2/3)。
    /// `div = 4` が 1 拍 (quarter note) — daw_01 全体の "1/N" 解釈
    /// (`common::snap` の module doc) と同じ。
    Note { div: u16, triplet: bool },
}

impl LaunchQuantize {
    /// 量子化の粒度を拍で返す。`Global` は「呼び側が解決すべき」なので `None`、
    /// `Off` は「量子化なし」なので `None`。**両者の区別は呼び側が行う**
    /// (`Global` は先にグローバル値へ解決してからここを呼ぶ)。
    #[must_use]
    pub fn beats(self, time_sig: (u8, u8)) -> Option<f64> {
        match self {
            Self::Global | Self::Off => None,
            Self::Bars(n) => {
                let bar = beats_per_bar(time_sig);
                let n = f64::from(n);
                (n > 0.0 && bar > 0.0).then_some(bar * n)
            }
            Self::Note { div, triplet } => {
                if div == 0 {
                    return None;
                }
                let base = 4.0 / f64::from(div);
                Some(if triplet { base * 2.0 / 3.0 } else { base })
            }
        }
    }
}

/// 1 小節の拍数 (`numerator * 4 / denominator`)。4/4 → 4、3/4 → 3、6/8 → 3。
/// `common::snap` の `Bars` と同じ式 — 片方だけ直すとグリッドがズレるので、
/// 式を書き足すときは両方を見ること。
#[must_use]
pub fn beats_per_bar(time_sig: (u8, u8)) -> f64 {
    let (num, den) = (f64::from(time_sig.0), f64::from(time_sig.1));
    if num <= 0.0 || den <= 0.0 {
        return 4.0;
    }
    num * 4.0 / den
}

/// ローンチ量子化のドロップダウンに出す選択肢の **SSoT** (表示順)。
/// `Global` は「トランスポートの設定に従う」なのでセル側にだけ出す
/// (グローバル設定そのものの dropdown はここから `Global` を除いたもの)。
pub const LAUNCH_QUANTIZE_CHOICES: &[(LaunchQuantize, &str)] = &[
    (LaunchQuantize::Global, "Global"),
    (LaunchQuantize::Off, "None"),
    (LaunchQuantize::Bars(8), "8 Bars"),
    (LaunchQuantize::Bars(4), "4 Bars"),
    (LaunchQuantize::Bars(2), "2 Bars"),
    (LaunchQuantize::Bars(1), "1 Bar"),
    (LaunchQuantize::Note { div: 2, triplet: false }, "1/2"),
    (LaunchQuantize::Note { div: 2, triplet: true }, "1/2T"),
    (LaunchQuantize::Note { div: 4, triplet: false }, "1/4"),
    (LaunchQuantize::Note { div: 4, triplet: true }, "1/4T"),
    (LaunchQuantize::Note { div: 8, triplet: false }, "1/8"),
    (LaunchQuantize::Note { div: 8, triplet: true }, "1/8T"),
    (LaunchQuantize::Note { div: 16, triplet: false }, "1/16"),
    (LaunchQuantize::Note { div: 16, triplet: true }, "1/16T"),
    (LaunchQuantize::Note { div: 32, triplet: false }, "1/32"),
];

/// グローバルローンチ量子化の既定 (= 1 小節)。Live / Bitwig / Studio One と同じ。
pub const DEFAULT_GLOBAL_LAUNCH_QUANTIZE: LaunchQuantize = LaunchQuantize::Bars(1);

/// セルを押した / 離したときの解釈 (Live の Launch Mode)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum LaunchMode {
    /// 押すと発火、離しても何もしない。
    #[default]
    Trigger,
    /// 押すと発火、**離すと停止**。
    Gate,
    /// 押すと発火、次に押すと停止。
    Toggle,
    /// 押している間、量子化の周期で撃ち直し続ける。
    Repeat,
}

/// フォローアクション (Live 12 相当)。`a` / `b` を `chance_a` の確率で抽選する。
///
/// 抽選と `Any` / `Other` の乱数は **`f(seed, 発火拍)` の純ハッシュ**で解く
/// (書き出しを 2 回やれば同じ結果になること = Q9 の再現性の前提)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct FollowAction {
    /// `false` = フォローアクションなし (既定)。
    #[serde(default)]
    pub enabled: bool,
    pub a: FollowActionKind,
    pub b: FollowActionKind,
    /// `a` を選ぶ確率 (%)。`b` は `100 - chance_a`。
    pub chance_a: u8,
    /// `true` (Linked、既定) = クリップ終端 (`multiplier` 回ループした後) に発火。
    /// `false` (Unlinked) = `time_beats` 拍ごとに発火。
    #[serde(default = "default_true")]
    pub linked: bool,
    /// Unlinked のときの発火間隔 (拍)。既定 4 拍 (= 4/4 の 1 小節)。
    pub time_beats: f64,
    /// Linked のときのループ回数。`1` = 1 周で発火。
    pub multiplier: u8,
}

impl Default for FollowAction {
    fn default() -> Self {
        Self {
            enabled: false,
            a: FollowActionKind::NoAction,
            b: FollowActionKind::NoAction,
            chance_a: 100,
            linked: true,
            time_beats: 4.0,
            multiplier: 1,
        }
    }
}

impl FollowAction {
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// フォローアクションの行動 10 種 (Live 12)。
///
/// `Previous` / `Next` / `First` / `Last` / `Any` / `Other` が指す範囲 (= グループ) は
/// **同じ行の中で空セルに区切られた連続した塊** ([`launch_group`] が SSoT、Q13)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum FollowActionKind {
    /// 何もしない (鳴り続ける)。
    #[default]
    NoAction,
    /// 停止する。
    Stop,
    /// 同じセルを頭から撃ち直す。
    PlayAgain,
    /// グループ内の 1 つ上のセル (先頭なら末尾へ巡回)。
    Previous,
    /// グループ内の 1 つ下のセル (末尾なら先頭へ巡回)。
    Next,
    /// グループの先頭。
    First,
    /// グループの末尾。
    Last,
    /// グループ内からランダムに 1 つ (同じセルも当たる)。
    Any,
    /// `Any` と同じだがグループが 2 つ以上なら**直前と同じセルは選ばない**。
    Other,
    /// 指定した列のセルへ飛ぶ (行は同じ)。
    Jump { scene_id: u32 },
}

/// 行の主導権。行 = トラック行 (`Track.launcher`) / オートメーションレーン行
/// (`AutomationLane.launcher`)。
///
/// **停止しても消えない持続状態**で `.daw` に保存する — 停止 → 再生で同じセルが
/// 鳴り直し、書き出しもこの状態を反映する (Q9 / Q10)。「いつ撃ったか」 という
/// 実時間の情報は持たない (それは engine 側の走行状態)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum RowPlayback {
    /// アレンジのタイムラインが鳴らす (既定)。
    #[default]
    Arranger,
    /// ランチャーが握っていて、`clip_id` のセルが鳴っている。
    /// `clip_id` はその行の中で一意なので列 (`scene_id`) は持たない (SSoT)。
    Launcher { clip_id: u32 },
    /// ランチャーが握っているが無音 (Stop Clips を押した状態)。
    /// アレンジへは戻らない = アレンジのクリップも鳴らない。
    LauncherStopped,
}

impl RowPlayback {
    /// アレンジが主導権を持っている (= 既定)。`skip_serializing_if` 用に
    /// `&self` を取る。
    #[must_use]
    pub fn is_arranger(&self) -> bool {
        matches!(self, Self::Arranger)
    }

    /// ランチャーが主導権を握っているか (鳴っているかどうかは問わない)。
    #[must_use]
    pub fn is_launcher(self) -> bool {
        !matches!(self, Self::Arranger)
    }

    /// 鳴っているセルの `clip_id`。
    #[must_use]
    pub fn playing_clip_id(self) -> Option<u32> {
        match self {
            Self::Launcher { clip_id } => Some(clip_id),
            _ => None,
        }
    }
}

/// フォローアクションのグループ = **空セルに区切られた連続した塊** (Q13)。
///
/// `scene_ids` は表示順の列 id 列 (`Song.scenes` の順)、`occupied` は
/// 「その列にこの行のセルがあるか」。`from` を含む塊の
/// `[start, end)` を `scene_ids` の index で返す。`from` が空セルなら `None`。
///
/// GUI (縞模様の ▶ 表示) と engine (Next/Previous/Any/Other の解決) が
/// **同じ 1 本**を使うためにここに置く。
#[must_use]
pub fn launch_group(occupied: &[bool], from: usize) -> Option<(usize, usize)> {
    if !occupied.get(from).copied().unwrap_or(false) {
        return None;
    }
    let mut start = from;
    while start > 0 && occupied[start - 1] {
        start -= 1;
    }
    let mut end = from + 1;
    while end < occupied.len() && occupied[end] {
        end += 1;
    }
    Some((start, end))
}

// ---------------------------------------------------------------------------
// Song 側の入口。**ランチャーに関する Song のメソッドはここに置く** — model.rs は
// 実コード 1,000 行 budget (不変条件 9) を大きく超えた god file で、機能を足すたびに
// そこへ積むと分割が永久に来ない。
// ---------------------------------------------------------------------------

impl super::Song {
    /// v35 (r.md #87): 新しいランチャー列 ([`Scene`]) の安定 id を採番する。
    /// `0` は未採番 sentinel なので最低 `1` から返す。
    pub fn alloc_scene_id(&mut self) -> u32 {
        let id = self.ids.next_scene_id.max(1);
        self.ids.next_scene_id = id.saturating_add(1);
        id
    }

    /// 列 id → `scenes` 内の表示順 index。
    #[must_use]
    pub fn scene_index(&self, scene_id: u32) -> Option<usize> {
        self.scenes.iter().position(|s| s.id == scene_id)
    }

    /// 末尾に列を足して、その id を返す。**セルを置くとき以外は呼ばない**
    /// (load 時に補うと「開いただけで `*`」になる、r.md #9)。
    pub fn push_scene(&mut self) -> u32 {
        let id = self.alloc_scene_id();
        self.scenes.push(Scene::new(id));
        id
    }

    /// 表示順で `index` 番目の列を確実に存在させ、その id を返す。
    /// グリッドの「空きプレースホルダ列」にセルを置いた瞬間に呼ぶ
    /// (途中の列もまとめて実体化する)。
    pub fn ensure_scene_at(&mut self, index: usize) -> u32 {
        while self.scenes.len() <= index {
            self.push_scene();
        }
        self.scenes[index].id
    }

    /// 未採番 (`0`) / 重複した [`Scene::id`] を採番し直し、`next_scene_id` を実在 id の
    /// 最大値 + 1 まで進める。**冪等** (2 回呼んでも同じ結果 = 開いただけで `*` が
    /// 立たない、r.md #9)。旧 file の forward-migration はここが唯一の口。
    pub fn ensure_scene_ids(&mut self) {
        let mut max_id = 0u32;
        for s in &self.scenes {
            if s.id != 0 {
                max_id = max_id.max(s.id);
            }
        }
        let mut next = self.ids.next_scene_id.max(max_id.saturating_add(1)).max(1);
        let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for s in &mut self.scenes {
            if s.id == 0 || !seen.insert(s.id) {
                s.id = next;
                seen.insert(next);
                next = next.saturating_add(1);
            }
        }
        self.ids.next_scene_id = next;
    }

    /// ランチャー側の不変条件をそろえる。**冪等**
    /// (2 回呼んでも同じ結果 = 開いただけで `*` が立たない、r.md #9)。
    ///
    /// 1. [`Scene::id`] の未採番 / 重複を採り直す ([`Self::ensure_scene_ids`])
    /// 2. **実在しない列を指すセルを捨てる** (孤児セルは撃てないのに content を
    ///    生かし続けるので、GC の判断が狂う)
    /// 3. セルの `clip.start_beat` を `0` に正す (セルは「撃った瞬間」が原点で
    ///    song-absolute な配置を持たない = [`SessionClip::clip`] の契約)
    /// 4. 実在しないセルを指す主導権を [`RowPlayback::LauncherStopped`] へ落とす。
    ///    [`RowPlayback::Arranger`] へは戻さない — 戻すと「ランチャーに渡した行」の
    ///    アレンジのクリップが黙って鳴り出す
    pub fn normalize_session(&mut self) {
        self.ensure_scene_ids();
        let live_scenes: std::collections::HashSet<u32> =
            self.scenes.iter().map(|s| s.id).collect();
        for track in &mut self.tracks {
            normalize_row(
                &mut track.session_clips,
                &mut track.launcher,
                &live_scenes,
                |c| &mut c.clip.start_beat,
                |c| c.clip.id,
            );
            for lane in &mut track.automation_lanes {
                normalize_row(
                    &mut lane.session_clips,
                    &mut lane.launcher,
                    &live_scenes,
                    |c| &mut c.clip.start_beat,
                    |c| c.clip.id,
                );
            }
        }
    }
}

/// [`Song::normalize_session`] の 1 行分。トラック行とオートメーションレーン行で
/// **完全に同じ規則**なので、型だけ違う 2 つのループを 1 本にまとめる
/// (片方だけ直して規則がズレるのを防ぐ)。
fn normalize_row<C>(
    cells: &mut Vec<C>,
    launcher: &mut RowPlayback,
    live_scenes: &std::collections::HashSet<u32>,
    start_beat: impl Fn(&mut C) -> &mut f64,
    clip_id: impl Fn(&C) -> u32,
) where
    C: SessionCell,
{
    cells.retain(|c| live_scenes.contains(&c.scene_id()));
    for cell in cells.iter_mut() {
        *start_beat(cell) = 0.0;
    }
    if let RowPlayback::Launcher { clip_id: playing } = *launcher
        && !cells.iter().any(|c| clip_id(c) == playing)
    {
        *launcher = RowPlayback::LauncherStopped;
    }
}

/// トラック行 / オートメーションレーン行のセルに共通する最小の形。
pub trait SessionCell {
    fn scene_id(&self) -> u32;
}

impl SessionCell for SessionClip {
    fn scene_id(&self) -> u32 {
        self.scene_id
    }
}

impl SessionCell for SessionAutomationClip {
    fn scene_id(&self) -> u32 {
        self.scene_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 量子化は拍子から拍数を出す() {
        // (quantize, time_sig, expected_beats)
        let cases: &[(LaunchQuantize, (u8, u8), Option<f64>)] = &[
            (LaunchQuantize::Global, (4, 4), None),
            (LaunchQuantize::Off, (4, 4), None),
            (LaunchQuantize::Bars(1), (4, 4), Some(4.0)),
            (LaunchQuantize::Bars(2), (3, 4), Some(6.0)),
            (LaunchQuantize::Bars(1), (6, 8), Some(3.0)),
            (LaunchQuantize::Note { div: 4, triplet: false }, (4, 4), Some(1.0)),
            (LaunchQuantize::Note { div: 8, triplet: false }, (4, 4), Some(0.5)),
            (LaunchQuantize::Note { div: 4, triplet: true }, (4, 4), Some(2.0 / 3.0)),
            (LaunchQuantize::Note { div: 32, triplet: false }, (4, 4), Some(0.125)),
            // 壊れた値は None (0 除算・無限ループを engine へ流さない)
            (LaunchQuantize::Bars(0), (4, 4), None),
            (LaunchQuantize::Note { div: 0, triplet: false }, (4, 4), None),
        ];
        for (q, ts, expected) in cases {
            let got = q.beats(*ts);
            match (got, expected) {
                (Some(g), Some(e)) => {
                    assert!((g - e).abs() < 1e-9, "q={q:?} ts={ts:?} got={g} want={e}");
                }
                (None, None) => {}
                _ => panic!("q={q:?} ts={ts:?} got={got:?} want={expected:?}"),
            }
        }
    }

    #[test]
    fn グループは空セルで区切られる() {
        //          0     1     2     3      4     5     6
        let occ = [true, true, true, false, true, true, false];
        assert_eq!(launch_group(&occ, 0), Some((0, 3)));
        assert_eq!(launch_group(&occ, 2), Some((0, 3)));
        assert_eq!(launch_group(&occ, 3), None, "空セルはどのグループにも属さない");
        assert_eq!(launch_group(&occ, 4), Some((4, 6)));
        assert_eq!(launch_group(&occ, 5), Some((4, 6)));
        assert_eq!(launch_group(&occ, 6), None);
        assert_eq!(launch_group(&occ, 99), None, "範囲外");
    }

    // ---- Song との結合 (参照数え上げ / id 空間 / 正規化) ----------------------

    use crate::model::{
        AutomationClip, AutomationLane, AutomationTarget, ClipContent, MidiContent, Song, Track,
        TrackBuiltinParam,
    };

    /// arrangement のクリップ 1 つと launcher のセル 1 つを持つ track 1 本の Song。
    /// セルは **別 content** を指す (= GC が片方だけ生かす事故を検出できる)。
    fn song_with_cell() -> Song {
        let mut song = Song::default();
        let mut track = Track { id: 1, next_clip_id: 1, ..Track::default() };
        track.clips.push(Clip { id: 1, start_beat: 0.0, length_beats: 4.0, ..Clip::default() });
        track.session_clips.push(SessionClip {
            scene_id: 0, // ensure_scene_ids / normalize_session の前は未解決
            clip: Clip { id: 2, start_beat: 0.0, length_beats: 4.0, ..Clip::default() },
            launch: LaunchSettings::default(),
        });
        song.tracks.push(track);
        song.push_scene();
        song.tracks[0].session_clips[0].scene_id = song.scenes[0].id;
        song.ensure_clip_contents();
        song
    }

    #[test]
    fn セルの中身は保存前_gc_で消えない() {
        let mut song = song_with_cell();
        let cell_content = song.tracks[0].session_clips[0].clip.content_id;
        assert_ne!(cell_content, 0, "セルにも content_id が採番される");
        assert_eq!(song.clip_content_refcount(cell_content), 1);

        song.normalize_for_save();
        assert!(
            song.clip_contents.contains_key(&cell_content),
            "launcher のセルが参照する content を GC が落としてはいけない"
        );
    }

    #[test]
    fn セルとアレンジのクリップは同じ_id_空間を共有する() {
        let mut track = Track { next_clip_id: 1, ..Track::default() };
        track.clips.push(Clip { id: 5, ..Clip::default() });
        // わざと重複 id と未採番を混ぜる。
        track.session_clips.push(SessionClip {
            scene_id: 1,
            clip: Clip { id: 5, ..Clip::default() },
            launch: LaunchSettings::default(),
        });
        track.session_clips.push(SessionClip {
            scene_id: 2,
            clip: Clip { id: 0, ..Clip::default() },
            launch: LaunchSettings::default(),
        });
        track.ensure_clip_ids();

        let ids: Vec<u32> = track.all_clips().map(|c| c.id).collect();
        assert_eq!(ids.len(), 3);
        let unique: std::collections::HashSet<u32> = ids.iter().copied().collect();
        assert_eq!(unique.len(), 3, "id は行内で一意 (arrangement と launcher で共有): {ids:?}");
        assert!(ids.iter().all(|&id| id != 0), "未採番が残っている: {ids:?}");

        // 冪等: もう一度呼んでも何も変わらない (= 開いただけで `*` が立たない)。
        let before = track.clone();
        track.ensure_clip_ids();
        assert_eq!(track, before);
    }

    #[test]
    fn 実在しない列のセルは捨てられ主導権は停止へ落ちる() {
        let mut song = song_with_cell();
        let dead_scene = 999;
        song.tracks[0].session_clips[0].scene_id = dead_scene;
        song.tracks[0].launcher =
            RowPlayback::Launcher { clip_id: song.tracks[0].session_clips[0].clip.id };

        song.normalize_session();

        assert!(song.tracks[0].session_clips.is_empty(), "孤児セルは捨てる");
        assert_eq!(
            song.tracks[0].launcher,
            RowPlayback::LauncherStopped,
            "セルが消えても Arranger へは戻さない (アレンジのクリップが黙って鳴り出す)"
        );
    }

    #[test]
    fn セルの開始拍は常に_0_に正される() {
        let mut song = song_with_cell();
        song.tracks[0].session_clips[0].clip.start_beat = 12.0;
        song.normalize_session();
        assert_eq!(song.tracks[0].session_clips[0].clip.start_beat, 0.0);
    }

    #[test]
    fn 読み込み後の正規化は冪等() {
        let mut song = song_with_cell();
        // オートメーションレーン行のセルも混ぜる。
        let mut lane =
            AutomationLane::new(AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume), 1.0);
        lane.id = 1;
        lane.session_clips.push(SessionAutomationClip {
            scene_id: song.scenes[0].id,
            clip: AutomationClip { id: 0, ..AutomationClip::default() },
            launch: LaunchSettings::default(),
        });
        song.tracks[0].automation_lanes.push(lane);
        song.tracks[0].clips[0].content_id = song.alloc_content_id();
        song.clip_contents
            .insert(song.tracks[0].clips[0].content_id, ClipContent::Midi(MidiContent::default()));

        song.normalize_after_load();
        let once = song.clone();
        song.normalize_after_load();
        assert_eq!(song, once, "2 回目の正規化で Song が変わると『開いただけで *』になる");
    }

    #[test]
    fn 未命名シーンは並び順から名前を作る() {
        let s = Scene::new(7);
        assert_eq!(s.display_name(0), "Scene 1");
        assert_eq!(s.display_name(4), "Scene 5");
        let named = Scene { name: "サビ".into(), ..Scene::new(7) };
        assert_eq!(named.display_name(0), "サビ");
    }
}
