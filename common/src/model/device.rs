//! Parallel (r.md #110, `docs/plan_parallel.md`): ネスト可能な並列 device chain。
//!
//! `Track.devices` / `Song.master_fx_chain` の要素は [`Device`] = plugin か Parallel。
//! Parallel は並列 [`ParallelChain`] の列で、各 chain がまた `Vec<Device>` を持つ (無限ネスト)。
//! plugin / parallel / chain の id は **1 つの id 空間** (`Song.ids.next_device_id`) で採番し、
//! IPC / automation / 選択 / AudioTap はすべてその id でアドレスする (不変条件 1)。
//! 位置 (chain 内 index) は表示順と挿入位置にしか使わない。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::*;

/// device chain の 1 要素。
///
/// serde: `Parallel` は externally tagged (`{"Parallel": {..}}`)、`Plugin` だけが untagged の
/// **fallback** (= 旧 `.daw` の plugin 配列がそのまま読める)。 全 variant untagged の
/// 「field 集合の pairwise 非交差」 には依存しない — 判別は `Parallel` タグの有無 1 点だけで、
/// variant を足すときはタグ付きにすればよい。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum Device {
    Parallel(Parallel),
    #[serde(untagged)] // arch-lint: allow-untagged (fallback variant 1 本、判別は Parallel タグ)
    Plugin(PluginInstance),
}

/// 並列 chain の container (Live: Audio Effect Rack / Bitwig: FX Layer / Reason: Parallel)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Parallel {
    /// 安定 id (`Song::alloc_device_id`、plugin と同じ空間)。`0` = 未採番 sentinel。
    #[serde(default)]
    pub id: u64,
    #[serde(default = "default_parallel_name")]
    pub name: String,
    pub chains: Vec<ParallelChain>,
    /// r.md #105 と同じ意味: `true` の間、Parallel 全体が素通し (中の device は dispatch されない)。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bypassed: bool,
    /// 括弧行 (開始 / 終了) の色。 作った時点で周囲と別の色を自動で振る (Bitwig)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[f32; 3]>,
    /// 出力 trim (linear、`1.0` = unity、上限 [`MAX_TRACK_GAIN`])。 全 chain の和に掛ける。
    /// automation / 変調の対象 (`TrackBuiltinParam::ParallelOutGain`)。
    #[serde(default = "default_chain_gain")]
    pub out_gain: f32,
    /// gain match: 並列の和が入力より大きく (小さく) なるぶんを自動で戻す。 入力と出力の
    /// RMS (遅い窓) の比を出力に掛ける (engine `program.rs` の `update_gain_match`)。
    /// 帯域分割や Dry + Wet のように和がそのまま正しい使い方では **off** にする (既定 off)。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gain_match: bool,
    /// r.md #112: 入力を chain にどう配るか (帯域分割など)。 既定 [`Split::None`] = 全 chain に
    /// 同じ入力。 engine は `ChainBegin` で「chain k の入力 = split の k 番目の出力」を作るだけ
    /// なので、 モードが増えても op 列 / PDC / tap / mixer は変わらない。
    #[serde(default, skip_serializing_if = "Split::is_none")]
    pub split: Split,
}

/// r.md #112: Parallel の入力の配り方。 Bitwig は Multiband FX / Loudness Split / Mid-Side Split /
/// Stereo Split を別 container にしているが、 ここでは 1 つの Parallel の「配り方」の切替に
/// する (chain 側の gain / pan / M / S / SC / automation を container ごとに複製しない)。
///
/// 配り方の出力は **chain の並び順**に対応する (`Frequency3` なら chain 1 = Low、 2 = Mid、
/// 3 = High、 `MidSide` なら chain 1 = Mid、 2 = Side)。 出力数を超える chain は素通し
/// (全帯域 / 元のステレオ) の入力を受ける。 どのモードも **空 chain を出力数ぶん並べれば和は入力**。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum Split {
    /// 全 chain に同じ入力 (従来の Parallel)。
    #[default]
    None,
    /// 3 バンド周波数分割 (Linkwitz-Riley 24 dB/oct、 和は平坦)。 `low_hz` < `high_hz`
    /// (`Parallel::set_split_freq` が順序を保つ)。 値域は [`SPLIT_FREQ_RANGE`]。
    Frequency3 { low_hz: f32, high_hz: f32 },
    /// Mid / Side 分割 (Bitwig Mid-Side Split)。 Mid chain は `(M, M)`、 Side chain は `(S, -S)`
    /// (`M = (L+R)/2`、 `S = (L-R)/2`) を受け、 和は `(M+S, M-S) = (L, R)` で元に戻る。
    MidSide,
    /// r.md #114: Selector (Bitwig Instrument Selector / FX Selector)。 入力 (audio + MIDI) を
    /// **アクティブな 1 chain だけ** が受け、 他の chain は無音 (+ 新しい note 無し) を受ける。
    /// 切替は `fade_ms` のクロスフェード (両 chain の重みの和は常に 1)。 非アクティブ chain も
    /// 処理は続くのでリバーブ等の余韻は残り、 鳴っている音は note-on を受けた chain で note-off
    /// まで鳴り切る (Bitwig: "each sounding note continues until its output is silent")。
    ///
    /// `active_chain` は安定 `ParallelChain::id` (不変条件 1。 chain を並べ替えても追従する)。
    /// chain 一覧に無い id は先頭 chain と読む ([`Parallel::active_chain_index`])。
    /// automation / 変調の的は `TrackBuiltinParam::ParallelSelect` (位置 `0..=1`、 chain
    /// `k = floor(v · n)`: "full range morphs evenly thru all layers")。
    Selector { active_chain: u64, fade_ms: f32 },
}

impl Split {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// 既定のクロスオーバー `(low_hz, high_hz)` (200 Hz / 2 kHz)。 非有限値の置換にも使う (SSoT)。
    pub const DEFAULT_FREQS: (f32, f32) = (200.0, 2_000.0);
    /// 既定の 3 バンド分割。
    pub const DEFAULT_FREQUENCY3: Self =
        Self::Frequency3 { low_hz: Self::DEFAULT_FREQS.0, high_hz: Self::DEFAULT_FREQS.1 };
    /// r.md #114: Selector の既定クロスフェード (ms)。 クリックが乗らない最短程度。
    pub const DEFAULT_SELECTOR_FADE_MS: f32 = 20.0;
    /// 既定の Selector (`active_chain` は未解決 = 先頭 chain。 `Parallel::normalize_selector` が
    /// 実 id へ置き換える)。
    pub const DEFAULT_SELECTOR: Self = Self::Selector { active_chain: 0, fade_ms: Self::DEFAULT_SELECTOR_FADE_MS };

    /// この配り方が成り立つ最低 chain 数 (モード切替時に足りなければ空 chain を補う)。 `None` は 0。
    /// 分割の出力数 (`Frequency3` = 3 / `MidSide` = 2) と一致するが、 `Selector` は全 chain が出力
    /// (chain 数に追従) なので A/B の 2 本。
    pub fn min_chains(&self) -> usize {
        match self {
            Self::None => 0,
            Self::Frequency3 { .. } => 3,
            Self::MidSide | Self::Selector { .. } => 2,
        }
    }

    /// [`Split::Frequency3`] の出力名 (chain の並び順)。
    const FREQUENCY3_NAMES: [&'static str; 3] = ["Low", "Mid", "High"];
    /// [`Split::MidSide`] の出力名。
    const MID_SIDE_NAMES: [&'static str; 2] = ["Mid", "Side"];

    /// `index` 番目 (0 始まり) の出力の既定名 (chain を補完 / 付け替えるときの名前)。
    pub fn output_name(&self, index: usize) -> Option<&'static str> {
        match self {
            Self::None | Self::Selector { .. } => None,
            Self::Frequency3 { .. } => Self::FREQUENCY3_NAMES.get(index).copied(),
            Self::MidSide => Self::MID_SIDE_NAMES.get(index).copied(),
        }
    }

    /// `index` 番目の chain に **この配り方が付ける既定名**: 出力があればその出力名、 無ければ
    /// `Chain N` (N = index + 1)。 モード切替時に既定名の chain をこれへ付け替える。
    pub fn default_chain_name(&self, index: usize) -> String {
        self.output_name(index).map_or_else(|| format!("Chain {}", index + 1), str::to_string)
    }

    /// `name` が **機械が付けた既定名** か (`Chain N`、 またはどのモードかの出力名)。 モード切替で
    /// 付け替えてよい名前 = これ。 ユーザーが付けた名前 (それ以外) は据え置く。
    pub fn is_generated_chain_name(name: &str) -> bool {
        if let Some(n) = name.strip_prefix("Chain ") {
            return n.parse::<u32>().is_ok();
        }
        Self::FREQUENCY3_NAMES.contains(&name) || Self::MID_SIDE_NAMES.contains(&name)
    }

    /// `index` 番目の chain が受ける出力の番号 (= その chain の index)。 出力数を超える chain は
    /// `None` (素通し入力)。 engine の `ChainBegin` / `PreFx` tap の住所。 `Selector` は全 chain が
    /// 出力 (k 番目 = 「k 番目の chain がアクティブなら入力、 でなければ無音」)。
    pub fn output_of(&self, index: usize) -> Option<u8> {
        let count = match self {
            Self::None => 0,
            Self::Frequency3 { .. } => 3,
            Self::MidSide => 2,
            Self::Selector { .. } => usize::from(u8::MAX),
        };
        (index < count).then_some(index as u8)
    }

    /// UI に param 行 (ヘッダ直下) が要るか (`Frequency3` のクロスオーバー / `Selector` の
    /// Active + Fade)。
    pub fn has_params(&self) -> bool {
        matches!(self, Self::Frequency3 { .. } | Self::Selector { .. })
    }

    /// クロスオーバー周波数を読む (`Frequency3` 以外は `None`)。
    pub fn freq(&self, edge: SplitEdge) -> Option<f32> {
        match self {
            Self::Frequency3 { low_hz, high_hz } => Some(match edge {
                SplitEdge::LowMid => *low_hz,
                SplitEdge::MidHigh => *high_hz,
            }),
            Self::None | Self::MidSide | Self::Selector { .. } => None,
        }
    }

    /// クロスオーバー `(low_hz, high_hz)`。 `Frequency3` 以外は既定値 (engine が帯域分割器を持つのは
    /// `Frequency3` のときだけなので、 ここへ来る他 variant は snapshot の一時的な不一致)。
    pub fn freqs_or_default(&self) -> (f32, f32) {
        match self {
            Self::Frequency3 { low_hz, high_hz } => (*low_hz, *high_hz),
            Self::None | Self::MidSide | Self::Selector { .. } => Self::DEFAULT_FREQS,
        }
    }

    /// r.md #114: Selector のクロスフェード時間 (ms)。 `Selector` 以外は `None`。
    pub fn selector_fade_ms(&self) -> Option<f32> {
        match self {
            Self::Selector { fade_ms, .. } => Some(*fade_ms),
            Self::None | Self::Frequency3 { .. } | Self::MidSide => None,
        }
    }

    /// r.md #114: Selector の位置 `pos` (`0..=1`) が指す chain の index (`n` = chain 数)。
    /// `k = floor(pos · n)` を `0..n` に収める。 GUI (表示) と engine (per-sample) の唯一の写像。
    pub fn select_index(pos: f32, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        let k = (pos.clamp(0.0, 1.0) * n as f32).floor();
        (k as usize).min(n - 1)
    }

    /// [`Self::select_index`] の逆: chain `k` の中央の位置 (`(k + 0.5) / n`)。 端でなく中央に置く
    /// のは、 小さな変調で隣へ飛ばないため。
    pub fn select_pos(k: usize, n: usize) -> f32 {
        if n == 0 {
            return 0.5;
        }
        (k.min(n - 1) as f32 + 0.5) / n as f32
    }

    /// 値域へ丸め、 `low_hz <= high_hz` を保つ (load / IPC 境界の正規化)。
    pub fn sanitize(&mut self) {
        match self {
            Self::Frequency3 { low_hz, high_hz } => {
                let fix = |v: f32, d: f32| if v.is_finite() { SPLIT_FREQ_RANGE.clamp(v) } else { d };
                *low_hz = fix(*low_hz, Self::DEFAULT_FREQS.0);
                *high_hz = fix(*high_hz, Self::DEFAULT_FREQS.1);
                if *low_hz > *high_hz {
                    *high_hz = *low_hz;
                }
            }
            Self::Selector { fade_ms, .. } => {
                *fade_ms = if fade_ms.is_finite() {
                    SELECTOR_FADE_RANGE.clamp(*fade_ms)
                } else {
                    Self::DEFAULT_SELECTOR_FADE_MS
                };
            }
            Self::None | Self::MidSide => {}
        }
    }
}

/// [`Split::Frequency3`] の帯域 (chain の並び順 = Low / Mid / High)。 engine の出力番号 0/1/2。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum SplitBand {
    Low,
    Mid,
    High,
}

impl SplitBand {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Mid => "Mid",
            Self::High => "High",
        }
    }
}

/// [`Split::Frequency3`] のクロスオーバー (2 つ)。 automation / IPC の住所。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum SplitEdge {
    /// Low | Mid の境界 (`low_hz`)。
    LowMid,
    /// Mid | High の境界 (`high_hz`)。
    MidHigh,
}

/// クロスオーバー周波数の可動範囲 (対数)。 ノブ / automation 正規化 / IPC クランプの SSoT。
pub const SPLIT_FREQ_RANGE: super::ParamRange = super::ParamRange::Log { lo: 20.0, hi: 20_000.0 };

/// r.md #114: Selector のクロスフェード時間 (ms) の可動範囲。 0 = 即切替 (1 sample)。
pub const SELECTOR_FADE_RANGE: super::ParamRange = super::ParamRange::Linear { lo: 0.0, hi: 2_000.0 };

/// Parallel の中の 1 本の並列 chain。Live の Chain List の 1 行 = Bitwig の layer 1 段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ParallelChain {
    /// 安定 id (plugin / parallel と同じ空間)。`AudioTap` / `ChainGain` automation が指す。
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[f32; 3]>,
    /// linear amp、`1.0` = unity、上限 [`MAX_TRACK_GAIN`]。
    #[serde(default = "default_chain_gain")]
    pub gain: f32,
    /// `-1.0..=1.0` (L..R)。
    #[serde(default)]
    pub pan: f32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub muted: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub solo: bool,
    #[serde(default)]
    pub devices: Vec<Device>,
}

fn default_parallel_name() -> String {
    "Parallel".to_string()
}

fn default_chain_gain() -> f32 {
    1.0
}

impl Parallel {
    /// 新規 Parallel (chain 1 本 "Chain 1"、id は未採番 = 呼び出し側が `alloc_device_id` で埋める)。
    pub fn new() -> Self {
        Self {
            id: 0,
            name: default_parallel_name(),
            chains: vec![ParallelChain::new("Chain 1")],
            bypassed: false,
            color: None,
            out_gain: 1.0,
            gain_match: false,
            split: Split::None,
        }
    }

    /// Ungroup (Live と同じ): 全 chain の device を chain 順に直列連結した列を返す。
    pub fn flatten(self) -> Vec<Device> {
        self.chains.into_iter().flat_map(|c| c.devices).collect()
    }

    /// r.md #112: クロスオーバー周波数を 1 つ書く。 値域へ丸め、 もう片方を押して
    /// `low_hz <= high_hz` を保つ (Bitwig の分割点と同じく交差しない)。 GUI と engine
    /// (`song_values`) が同じ規則を通る唯一の口。 `Frequency3` でなければ何もしない。
    /// 戻り値 = 実際に値が変わったか。
    pub fn set_split_freq(&mut self, edge: SplitEdge, hz: f32) -> bool {
        let Split::Frequency3 { low_hz, high_hz } = &mut self.split else {
            return false;
        };
        if !hz.is_finite() {
            return false;
        }
        let hz = SPLIT_FREQ_RANGE.clamp(hz);
        let before = (*low_hz, *high_hz);
        match edge {
            SplitEdge::LowMid => {
                *low_hz = hz;
                *high_hz = high_hz.max(hz);
            }
            SplitEdge::MidHigh => {
                *high_hz = hz;
                *low_hz = low_hz.min(hz);
            }
        }
        (*low_hz, *high_hz) != before
    }

    /// r.md #114: Selector のアクティブ chain の index (chain の並び順)。 `Selector` でなければ
    /// `None`。 `active_chain` が chain 一覧に無い (消した / 未解決の 0) なら先頭 chain。
    pub fn active_chain_index(&self) -> Option<usize> {
        let Split::Selector { active_chain, .. } = self.split else {
            return None;
        };
        if self.chains.is_empty() {
            return None;
        }
        Some(self.chains.iter().position(|c| c.id == active_chain).unwrap_or(0))
    }

    /// r.md #114: automation / 変調の的 `ParallelSelect` の基準値 (アクティブ chain の中央の位置)。
    /// `Selector` でなければ中央 (`0.5`)。
    pub fn select_pos(&self) -> f32 {
        self.active_chain_index()
            .map_or(0.5, |k| Split::select_pos(k, self.chains.len()))
    }

    /// r.md #114: アクティブ chain を書く (GUI と engine が同じ規則を通る唯一の口)。 `Selector` で
    /// なければ / その id の chain が無ければ何もしない。 戻り値 = 実際に変わったか。
    pub fn set_active_chain(&mut self, chain_id: u64) -> bool {
        if !self.chains.iter().any(|c| c.id == chain_id) {
            return false;
        }
        let Split::Selector { active_chain, .. } = &mut self.split else {
            return false;
        };
        if *active_chain == chain_id {
            return false;
        }
        *active_chain = chain_id;
        true
    }

    /// r.md #114: Selector のクロスフェード時間 (ms) を書く (値域へ丸める)。 戻り値 = 変わったか。
    pub fn set_selector_fade(&mut self, ms: f32) -> bool {
        let Split::Selector { fade_ms, .. } = &mut self.split else {
            return false;
        };
        if !ms.is_finite() {
            return false;
        }
        let ms = SELECTOR_FADE_RANGE.clamp(ms);
        if *fade_ms == ms {
            return false;
        }
        *fade_ms = ms;
        true
    }

    /// r.md #114: `active_chain` を実在する chain id に揃える (モード切替 / load 時)。 chain 一覧に
    /// 無ければ先頭 chain の id。 `Selector` 以外・chain 無しは何もしない。
    pub fn normalize_selector(&mut self) {
        let Some(k) = self.active_chain_index() else { return };
        let id = self.chains[k].id;
        if let Split::Selector { active_chain, .. } = &mut self.split {
            *active_chain = id;
        }
    }

    /// chain 追加時の既定名 ("Chain N"、N = 既存最大番号 + 1)。
    pub fn next_chain_name(&self) -> String {
        let max = self
            .chains
            .iter()
            .filter_map(|c| c.name.strip_prefix("Chain ").and_then(|n| n.parse::<u32>().ok()))
            .max()
            .unwrap_or(0);
        format!("Chain {}", max + 1)
    }
}

impl Default for Parallel {
    fn default() -> Self {
        Self::new()
    }
}

impl ParallelChain {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: 0,
            name: name.into(),
            color: None,
            gain: 1.0,
            pan: 0.0,
            muted: false,
            solo: false,
            devices: Vec::new(),
        }
    }
}

impl Device {
    /// plugin / parallel どちらでも安定 id。
    pub fn id(&self) -> u64 {
        match self {
            Device::Plugin(p) => p.id,
            Device::Parallel(r) => r.id,
        }
    }

    pub fn bypassed(&self) -> bool {
        match self {
            Device::Plugin(p) => p.bypassed,
            Device::Parallel(r) => r.bypassed,
        }
    }

    pub fn set_bypassed(&mut self, bypassed: bool) {
        match self {
            Device::Plugin(p) => p.bypassed = bypassed,
            Device::Parallel(r) => r.bypassed = bypassed,
        }
    }

    pub fn as_plugin(&self) -> Option<&PluginInstance> {
        match self {
            Device::Plugin(p) => Some(p),
            Device::Parallel(_) => None,
        }
    }

    pub fn as_plugin_mut(&mut self) -> Option<&mut PluginInstance> {
        match self {
            Device::Plugin(p) => Some(p),
            Device::Parallel(_) => None,
        }
    }

    pub fn as_parallel(&self) -> Option<&Parallel> {
        match self {
            Device::Parallel(r) => Some(r),
            Device::Plugin(_) => None,
        }
    }

    pub fn as_parallel_mut(&mut self) -> Option<&mut Parallel> {
        match self {
            Device::Parallel(r) => Some(r),
            Device::Plugin(_) => None,
        }
    }

    /// この device (Parallel なら中身全部) に routed aux 出力を持つ plugin が居るか
    /// (パラアウト `docs/plan_paraout.md` の split 判定)。
    pub fn routes_any_aux_output(&self) -> bool {
        any_plugin(std::slice::from_ref(self), &mut |p| p.aux_outputs.iter().any(Option::is_some))
    }
}

impl From<PluginInstance> for Device {
    fn from(p: PluginInstance) -> Self {
        Device::Plugin(p)
    }
}

impl From<Parallel> for Device {
    fn from(r: Parallel) -> Self {
        Device::Parallel(r)
    }
}

/// device chain 上の「挿入先」。`Track(id)` = その track の top-level chain、
/// `Chain(id)` = Parallel の中の chain。master は `Track(MASTER_TRACK_ID)`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChainRef {
    Track(u32),
    Chain(u64),
}

/// `devices` 以下に `pred` を満たす plugin が居るか (pre-order、 Parallel の中も辿る)。
/// **確保なし** (再帰で辿る) なので RT からも呼べる — `Track::is_voicevox_vocal` は sequencer が
/// 毎 buffer 呼ぶ。 iterator が要らない「居るか」判定はこちらを使う ([`plugins`] は確保する)。
pub fn any_plugin(devices: &[Device], pred: &mut impl FnMut(&PluginInstance) -> bool) -> bool {
    devices.iter().any(|d| match d {
        Device::Plugin(p) => pred(p),
        Device::Parallel(r) => r.chains.iter().any(|c| any_plugin(&c.devices, pred)),
    })
}

/// `devices` 以下の全 plugin を **pre-order (= 信号順)** で辿る iterator。Parallel の中は
/// chain 順・chain 内は device 順。**RT では使わない** (stack が `Vec` = 確保する。
/// `make test-rt` が捕まえる)。 存在判定だけなら [`any_plugin`]。
pub fn plugins(devices: &[Device]) -> PluginIter<'_> {
    PluginIter {
        stack: vec![devices.iter()],
    }
}

pub struct PluginIter<'a> {
    stack: Vec<std::slice::Iter<'a, Device>>,
}

impl<'a> Iterator for PluginIter<'a> {
    type Item = &'a PluginInstance;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let top = self.stack.last_mut()?;
            match top.next() {
                None => {
                    self.stack.pop();
                }
                Some(Device::Plugin(p)) => return Some(p),
                Some(Device::Parallel(r)) => {
                    // 逆順に積むと pop 順が chain 順になる。
                    for c in r.chains.iter().rev() {
                        self.stack.push(c.devices.iter());
                    }
                }
            }
        }
    }
}

/// `devices` 以下の全 plugin を可変で訪問する (pre-order)。
pub fn for_each_plugin_mut(devices: &mut [Device], f: &mut impl FnMut(&mut PluginInstance)) {
    for d in devices {
        match d {
            Device::Plugin(p) => f(p),
            Device::Parallel(r) => {
                for c in &mut r.chains {
                    for_each_plugin_mut(&mut c.devices, f);
                }
            }
        }
    }
}

/// `devices` 以下の全 chain (`ParallelChain`) を訪問する (pre-order、Parallel ごとに chain 順)。
/// `f(parallel, chain)`。
pub fn for_each_chain<'a>(devices: &'a [Device], f: &mut impl FnMut(&'a Parallel, &'a ParallelChain)) {
    for d in devices {
        if let Device::Parallel(r) = d {
            for c in &r.chains {
                f(r, c);
                for_each_chain(&c.devices, f);
            }
        }
    }
}

/// [`for_each_parallel`] の可変版。
pub fn for_each_parallel_mut(devices: &mut [Device], f: &mut impl FnMut(&mut Parallel)) {
    for d in devices {
        if let Device::Parallel(r) = d {
            f(r);
            for c in &mut r.chains {
                for_each_parallel_mut(&mut c.devices, f);
            }
        }
    }
}

/// [`for_each_chain`] の可変版 (chain だけ)。
pub fn for_each_chain_mut(devices: &mut [Device], f: &mut impl FnMut(&mut ParallelChain)) {
    for d in devices {
        if let Device::Parallel(r) = d {
            for c in &mut r.chains {
                f(c);
                for_each_chain_mut(&mut c.devices, f);
            }
        }
    }
}

/// `devices` 以下の全 Parallel を訪問する (pre-order)。
pub fn for_each_parallel<'a>(devices: &'a [Device], f: &mut impl FnMut(&'a Parallel)) {
    for d in devices {
        if let Device::Parallel(r) = d {
            f(r);
            for c in &r.chains {
                for_each_parallel(&c.devices, f);
            }
        }
    }
}

/// plugin / parallel / chain の **id を全部** 可変で訪問する (`Song::ensure_ids` の採番用)。
pub fn for_each_node_id_mut(devices: &mut [Device], f: &mut impl FnMut(&mut u64)) {
    for d in devices {
        match d {
            Device::Plugin(p) => f(&mut p.id),
            Device::Parallel(r) => {
                f(&mut r.id);
                for c in &mut r.chains {
                    f(&mut c.id);
                    for_each_node_id_mut(&mut c.devices, f);
                }
            }
        }
    }
}

/// `devices` 以下で id が `id` の device (plugin / parallel) を探す。
/// 戻り値は `(その device が居る chain, index)`。`root` は `devices` 自身の ChainRef。
pub fn find_device_in(devices: &[Device], root: ChainRef, id: u64) -> Option<(ChainRef, usize)> {
    for (i, d) in devices.iter().enumerate() {
        if d.id() == id {
            return Some((root, i));
        }
        if let Device::Parallel(r) = d {
            for c in &r.chains {
                if let Some(found) = find_device_in(&c.devices, ChainRef::Chain(c.id), id) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// `devices` 以下で id が `chain_id` の chain の `devices` を返す。
pub fn chain_devices_in(devices: &[Device], chain_id: u64) -> Option<&Vec<Device>> {
    for d in devices {
        if let Device::Parallel(r) = d {
            for c in &r.chains {
                if c.id == chain_id {
                    return Some(&c.devices);
                }
                if let Some(found) = chain_devices_in(&c.devices, chain_id) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// [`chain_devices_in`] の可変版。
pub fn chain_devices_in_mut(devices: &mut [Device], chain_id: u64) -> Option<&mut Vec<Device>> {
    for d in devices {
        if let Device::Parallel(r) = d {
            for c in &mut r.chains {
                if c.id == chain_id {
                    return Some(&mut c.devices);
                }
                if let Some(found) = chain_devices_in_mut(&mut c.devices, chain_id) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// `devices` 以下で id が `chain_id` の chain を返す (親 Parallel と一緒に)。
pub fn chain_in(devices: &[Device], chain_id: u64) -> Option<(&Parallel, &ParallelChain)> {
    for d in devices {
        if let Device::Parallel(r) = d {
            for c in &r.chains {
                if c.id == chain_id {
                    return Some((r, c));
                }
                if let Some(found) = chain_in(&c.devices, chain_id) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// [`chain_in`] の可変版 (chain だけ)。
pub fn chain_in_mut(devices: &mut [Device], chain_id: u64) -> Option<&mut ParallelChain> {
    for d in devices {
        if let Device::Parallel(r) = d {
            for c in &mut r.chains {
                if c.id == chain_id {
                    return Some(c);
                }
                if let Some(found) = chain_in_mut(&mut c.devices, chain_id) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// `devices` 以下で id が `id` の device を返す。
pub fn device_in(devices: &[Device], id: u64) -> Option<&Device> {
    for d in devices {
        if d.id() == id {
            return Some(d);
        }
        if let Device::Parallel(r) = d {
            for c in &r.chains {
                if let Some(found) = device_in(&c.devices, id) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// [`device_in`] の可変版。
pub fn device_in_mut(devices: &mut [Device], id: u64) -> Option<&mut Device> {
    for d in devices {
        if d.id() == id {
            return Some(d);
        }
        if let Device::Parallel(r) = d {
            for c in &mut r.chains {
                if let Some(found) = device_in_mut(&mut c.devices, id) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// `devices` 以下で id が `id` の device を **抜き取る**。
pub fn remove_device_in(devices: &mut Vec<Device>, id: u64) -> Option<Device> {
    if let Some(i) = devices.iter().position(|d| d.id() == id) {
        return Some(devices.remove(i));
    }
    for d in devices {
        if let Device::Parallel(r) = d {
            for c in &mut r.chains {
                if let Some(found) = remove_device_in(&mut c.devices, id) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// `devices` 以下の chain id `chain_id` を持つ chain を **抜き取る** (親 Parallel から)。
pub fn remove_chain_in(devices: &mut [Device], chain_id: u64) -> Option<ParallelChain> {
    for d in devices {
        if let Device::Parallel(r) = d {
            if let Some(i) = r.chains.iter().position(|c| c.id == chain_id) {
                return Some(r.chains.remove(i));
            }
            for c in &mut r.chains {
                if let Some(found) = remove_chain_in(&mut c.devices, chain_id) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// 呼び出し側が「この chain は `id` の内側か」を判定するための祖先判定:
/// `devices` 以下で `chain_id` の chain が **`ancestor_id` の device (Parallel) の中** にあるか。
pub fn chain_is_inside_device(devices: &[Device], ancestor_id: u64, chain_id: u64) -> bool {
    device_in(devices, ancestor_id)
        .and_then(|d| d.as_parallel())
        .is_some_and(|r| r.chains.iter().any(|c| c.id == chain_id || chain_devices_in(&c.devices, chain_id).is_some()))
}

/// Song 全体 (全 track + master) の device 走査。
impl Song {
    /// `r` が指す chain の device 列。
    pub fn chain_devices(&self, r: ChainRef) -> Option<&Vec<Device>> {
        match r {
            ChainRef::Track(MASTER_TRACK_ID) => Some(&self.master_fx_chain),
            ChainRef::Track(tid) => self.track_by_id(tid).map(|t| &t.devices),
            ChainRef::Chain(cid) => self
                .tracks
                .iter()
                .find_map(|t| chain_devices_in(&t.devices, cid))
                .or_else(|| chain_devices_in(&self.master_fx_chain, cid)),
        }
    }

    /// [`Self::chain_devices`] の可変版。
    pub fn chain_devices_mut(&mut self, r: ChainRef) -> Option<&mut Vec<Device>> {
        match r {
            ChainRef::Track(MASTER_TRACK_ID) => Some(&mut self.master_fx_chain),
            ChainRef::Track(tid) => self.track_by_id_mut(tid).map(|t| &mut t.devices),
            ChainRef::Chain(cid) => {
                // borrowck: track 側で見つかればそれ、無ければ master。
                let in_track = self
                    .tracks
                    .iter()
                    .position(|t| chain_devices_in(&t.devices, cid).is_some());
                match in_track {
                    Some(i) => chain_devices_in_mut(&mut self.tracks[i].devices, cid),
                    None => chain_devices_in_mut(&mut self.master_fx_chain, cid),
                }
            }
        }
    }

    /// `id` の device (plugin / parallel) が居る `(chain, index)`。
    pub fn find_device(&self, id: u64) -> Option<(ChainRef, usize)> {
        self.tracks
            .iter()
            .find_map(|t| find_device_in(&t.devices, ChainRef::Track(t.id), id))
            .or_else(|| find_device_in(&self.master_fx_chain, ChainRef::Track(MASTER_TRACK_ID), id))
    }

    pub fn device_by_id(&self, id: u64) -> Option<&Device> {
        self.tracks
            .iter()
            .find_map(|t| device_in(&t.devices, id))
            .or_else(|| device_in(&self.master_fx_chain, id))
    }

    pub fn device_by_id_mut(&mut self, id: u64) -> Option<&mut Device> {
        let in_track = self.tracks.iter().position(|t| device_in(&t.devices, id).is_some());
        match in_track {
            Some(i) => device_in_mut(&mut self.tracks[i].devices, id),
            None => device_in_mut(&mut self.master_fx_chain, id),
        }
    }

    pub fn plugin_by_id(&self, id: u64) -> Option<&PluginInstance> {
        self.device_by_id(id).and_then(Device::as_plugin)
    }

    pub fn plugin_by_id_mut(&mut self, id: u64) -> Option<&mut PluginInstance> {
        self.device_by_id_mut(id).and_then(Device::as_plugin_mut)
    }

    pub fn parallel_by_id(&self, id: u64) -> Option<&Parallel> {
        self.device_by_id(id).and_then(Device::as_parallel)
    }

    pub fn parallel_by_id_mut(&mut self, id: u64) -> Option<&mut Parallel> {
        self.device_by_id_mut(id).and_then(Device::as_parallel_mut)
    }

    /// `chain_id` の chain (親 Parallel と一緒に)。
    pub fn chain_by_id(&self, chain_id: u64) -> Option<(&Parallel, &ParallelChain)> {
        self.tracks
            .iter()
            .find_map(|t| chain_in(&t.devices, chain_id))
            .or_else(|| chain_in(&self.master_fx_chain, chain_id))
    }

    pub fn chain_by_id_mut(&mut self, chain_id: u64) -> Option<&mut ParallelChain> {
        let in_track = self.tracks.iter().position(|t| chain_in(&t.devices, chain_id).is_some());
        match in_track {
            Some(i) => chain_in_mut(&mut self.tracks[i].devices, chain_id),
            None => chain_in_mut(&mut self.master_fx_chain, chain_id),
        }
    }

    /// `r` が属する track id (master は `MASTER_TRACK_ID`)。dangling は `None`。
    pub fn chain_owner_track(&self, r: ChainRef) -> Option<u32> {
        match r {
            ChainRef::Track(tid) => {
                (tid == MASTER_TRACK_ID || self.track_by_id(tid).is_some()).then_some(tid)
            }
            ChainRef::Chain(cid) => self
                .tracks
                .iter()
                .find(|t| chain_in(&t.devices, cid).is_some())
                .map(|t| t.id)
                .or_else(|| chain_in(&self.master_fx_chain, cid).map(|_| MASTER_TRACK_ID)),
        }
    }

    /// `id` の device が属する track id (master は `MASTER_TRACK_ID`)。
    pub fn device_owner_track(&self, id: u64) -> Option<u32> {
        self.tracks
            .iter()
            .find(|t| device_in(&t.devices, id).is_some())
            .map(|t| t.id)
            .or_else(|| device_in(&self.master_fx_chain, id).map(|_| MASTER_TRACK_ID))
    }

    /// `id` の device を抜き取る (どの chain に居ても)。
    pub fn remove_device(&mut self, id: u64) -> Option<Device> {
        for t in &mut self.tracks {
            if let Some(d) = remove_device_in(&mut t.devices, id) {
                return Some(d);
            }
        }
        remove_device_in(&mut self.master_fx_chain, id)
    }

    /// `chain_id` の chain を親 Parallel から抜き取る。
    pub fn remove_chain(&mut self, chain_id: u64) -> Option<ParallelChain> {
        for t in &mut self.tracks {
            if let Some(c) = remove_chain_in(&mut t.devices, chain_id) {
                return Some(c);
            }
        }
        remove_chain_in(&mut self.master_fx_chain, chain_id)
    }

    /// `r` の `index` に `device` を挿す (末尾超過は末尾)。chain が無ければ `false`。
    pub fn insert_device(&mut self, r: ChainRef, index: usize, device: Device) -> bool {
        let Some(chain) = self.chain_devices_mut(r) else {
            return false;
        };
        let at = index.min(chain.len());
        chain.insert(at, device);
        true
    }

    /// 全 track + master の全 plugin (pre-order)。
    pub fn all_plugins(&self) -> impl Iterator<Item = &PluginInstance> {
        self.tracks
            .iter()
            .flat_map(|t| plugins(&t.devices))
            .chain(plugins(&self.master_fx_chain))
    }

    /// 全 track + master の全 plugin を可変で訪問。
    pub fn for_each_plugin_mut(&mut self, f: &mut impl FnMut(&mut PluginInstance)) {
        for t in &mut self.tracks {
            for_each_plugin_mut(&mut t.devices, f);
        }
        for_each_plugin_mut(&mut self.master_fx_chain, f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_format::PluginFormat;

    fn plug(id: u64) -> Device {
        Device::Plugin(PluginInstance {
            id,
            ..PluginInstance::new(format!("p{id}"), PluginFormat::Clap)
        })
    }

    fn parallel(id: u64, chains: Vec<(u64, Vec<Device>)>) -> Device {
        Device::Parallel(Parallel {
            id,
            name: "Parallel".into(),
            color: None,
            out_gain: 1.0,
            gain_match: false,
            split: Split::None,
            chains: chains
                .into_iter()
                .map(|(cid, devices)| ParallelChain {
                    id: cid,
                    devices,
                    ..ParallelChain::new("c")
                })
                .collect(),
            bypassed: false,
        })
    }

    #[test]
    fn plugins_walks_pre_order_through_nested_parallels() {
        let devices = vec![
            plug(1),
            parallel(10, vec![(11, vec![plug(2), parallel(20, vec![(21, vec![plug(3)])])]), (12, vec![plug(4)])]),
            plug(5),
        ];
        let ids: Vec<u64> = plugins(&devices).map(|p| p.id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn find_and_remove_addresses_nested_chain_by_id() {
        let mut devices = vec![
            plug(1),
            parallel(10, vec![(11, vec![plug(2)]), (12, vec![parallel(20, vec![(21, vec![plug(3)])])])]),
        ];
        assert_eq!(find_device_in(&devices, ChainRef::Track(7), 3), Some((ChainRef::Chain(21), 0)));
        assert_eq!(find_device_in(&devices, ChainRef::Track(7), 20), Some((ChainRef::Chain(12), 0)));
        assert_eq!(find_device_in(&devices, ChainRef::Track(7), 10), Some((ChainRef::Track(7), 1)));
        assert!(chain_is_inside_device(&devices, 10, 21));
        assert!(!chain_is_inside_device(&devices, 20, 11));
        let taken = remove_device_in(&mut devices, 3).expect("nested plugin removed");
        assert_eq!(taken.id(), 3);
        assert!(find_device_in(&devices, ChainRef::Track(7), 3).is_none());
        let chain = remove_chain_in(&mut devices, 11).expect("chain removed");
        assert_eq!(chain.devices.len(), 1);
    }

    #[test]
    fn device_json_reads_legacy_plugin_array_and_parallel() {
        // 旧 .daw: plugin object の配列。
        let legacy = r#"[{"plugin_id":"x","format":"Clap"}]"#;
        let v: Vec<Device> = serde_json::from_str(legacy).unwrap();
        assert!(matches!(&v[0], Device::Plugin(p) if p.plugin_id == "x"));
        // Parallel 入りの往復。
        let devices = vec![plug(1), parallel(10, vec![(11, vec![plug(2)])])];
        let json = serde_json::to_string(&devices).unwrap();
        let back: Vec<Device> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, devices);
        // bincode 往復 (wire)。
        let cfg = bincode::config::standard();
        let bytes = bincode::encode_to_vec(&devices, cfg).unwrap();
        let (decoded, _): (Vec<Device>, usize) = bincode::decode_from_slice(&bytes, cfg).unwrap();
        assert_eq!(plugins(&decoded).map(|p| p.id).collect::<Vec<_>>(), vec![1, 2]);
    }

    /// r.md #112: 旧 JSON (split 無し) は `Split::None`、 `Frequency3` は往復し、 setter が順序を保つ。
    #[test]
    fn split_defaults_to_none_roundtrips_and_keeps_edge_order() {
        let legacy = r#"{"Parallel":{"id":1,"name":"P","chains":[]}}"#;
        let d: Device = serde_json::from_str(legacy).unwrap();
        assert_eq!(d.as_parallel().unwrap().split, Split::None);

        let mut r = Parallel::new();
        r.split = Split::DEFAULT_FREQUENCY3;
        assert!(r.set_split_freq(SplitEdge::MidHigh, 100.0));
        assert_eq!(r.split, Split::Frequency3 { low_hz: 100.0, high_hz: 100.0 }, "Mid|High が Low|Mid を押し下げる");
        assert!(!r.set_split_freq(SplitEdge::MidHigh, 100.0), "同値は変更なし");
        assert!(r.set_split_freq(SplitEdge::LowMid, 5.0));
        assert_eq!(r.split.freq(SplitEdge::LowMid), Some(20.0), "値域の下端へ");
        let json = serde_json::to_string(&Device::Parallel(r.clone())).unwrap();
        let back: Device = serde_json::from_str(&json).unwrap();
        assert_eq!(back.as_parallel().unwrap().split, r.split);
        let mut plain = Parallel::new();
        assert!(!plain.set_split_freq(SplitEdge::LowMid, 500.0), "None には効かない");

        let ms = Split::MidSide;
        assert_eq!((ms.min_chains(), ms.output_name(0), ms.output_name(1), ms.output_name(2)), (2, Some("Mid"), Some("Side"), None));
        assert_eq!((ms.default_chain_name(1), ms.default_chain_name(2)), ("Side".to_string(), "Chain 3".to_string()));
        assert!(Split::is_generated_chain_name("Chain 12") && Split::is_generated_chain_name("High"));
        assert!(!Split::is_generated_chain_name("Comp") && !Split::is_generated_chain_name("Chain x"));
        assert_eq!((ms.output_of(1), ms.output_of(2)), (Some(1), None));
        assert!(!ms.has_params());
        let json = serde_json::to_string(&ms).unwrap();
        assert_eq!(serde_json::from_str::<Split>(&json).unwrap(), ms);
    }

    /// r.md #114: Selector のアクティブ chain は安定 id で持ち、 無効な id は先頭 chain と読む。
    /// 位置 `0..=1` ↔ chain index の写像は端を含めて `0..n` に収まり、 setter は値域を守る。
    #[test]
    fn selector_tracks_the_active_chain_by_id_and_maps_position_to_chain_index() {
        let mut r = Parallel::new();
        r.chains[0].id = 11;
        r.chains.push(ParallelChain { id: 12, ..ParallelChain::new("Chain 2") });
        r.chains.push(ParallelChain { id: 13, ..ParallelChain::new("Chain 3") });
        assert_eq!(r.active_chain_index(), None, "Selector 以外は None");
        assert!(!r.set_active_chain(12), "Selector 以外には効かない");

        r.split = Split::DEFAULT_SELECTOR;
        assert_eq!(r.active_chain_index(), Some(0), "未解決 (0) は先頭 chain");
        r.normalize_selector();
        assert_eq!(r.split, Split::Selector { active_chain: 11, fade_ms: Split::DEFAULT_SELECTOR_FADE_MS });
        assert!(r.set_active_chain(13));
        assert!(!r.set_active_chain(13), "同値は変更なし");
        assert!(!r.set_active_chain(99), "無い chain は拒否");
        assert_eq!(r.active_chain_index(), Some(2));
        assert!((r.select_pos() - 2.5 / 3.0).abs() < 1e-6, "chain 3 の中央");
        // 並べ替えても id で追従する (不変条件 1)。
        r.chains.swap(0, 2);
        assert_eq!(r.active_chain_index(), Some(0));
        // 消したら先頭へ落ちる (補償コード無し)。
        r.chains.remove(0);
        assert_eq!(r.active_chain_index(), Some(0));
        assert_eq!(r.chains[0].id, 12);

        assert!(r.set_selector_fade(-5.0));
        assert_eq!(r.split.selector_fade_ms(), Some(0.0), "値域の下端へ");
        assert!(r.set_selector_fade(99_999.0));
        assert_eq!(r.split.selector_fade_ms(), Some(2_000.0));
        assert!(!r.set_selector_fade(f32::NAN));

        // 位置 ↔ index: 端を含めて 0..n、 中央の位置は同じ index に戻る。
        for n in 1..=5usize {
            for k in 0..n {
                assert_eq!(Split::select_index(Split::select_pos(k, n), n), k, "n={n} k={k}");
            }
            assert_eq!(Split::select_index(1.0, n), n - 1);
            assert_eq!(Split::select_index(0.0, n), 0);
            assert_eq!(Split::select_index(-1.0, n), 0);
        }
        assert_eq!(Split::select_index(0.5, 0), 0);
        let sel = Split::DEFAULT_SELECTOR;
        assert_eq!((sel.min_chains(), sel.output_name(0), sel.output_of(7), sel.default_chain_name(1)), (2, None, Some(7), "Chain 2".to_string()));
        assert!(sel.has_params());
        let json = serde_json::to_string(&sel).unwrap();
        assert_eq!(serde_json::from_str::<Split>(&json).unwrap(), sel);
    }

    #[test]
    fn flatten_concatenates_chains_in_order() {
        let r = Parallel {
            id: 1,
            name: "R".into(),
            chains: vec![
                ParallelChain { id: 2, devices: vec![plug(5), plug(6)], ..ParallelChain::new("a") },
                ParallelChain { id: 3, devices: vec![plug(7)], ..ParallelChain::new("b") },
            ],
            bypassed: false,
            color: None,
            out_gain: 1.0,
            gain_match: false,
            split: Split::None,
        };
        let flat: Vec<u64> = r.flatten().iter().map(Device::id).collect();
        assert_eq!(flat, vec![5, 6, 7]);
    }
}
