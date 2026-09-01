//! Modulation routing (AudioTap / aux / envelope follower / LFO / MSEG / Steps)
//!
//! arch-refactor #9 (god-file budget) で model.rs から分割。pure code movement で
//! 挙動・serialize 形式は不変。sibling 型は `use super::*` 経由で参照する。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::*;

// =====================================================================
// Modulation routing (sidechain + envelope follower) — docs/plan_modulation.md
// =====================================================================
//
// 「音をどこから取るか」の唯一の真実 = `AudioTap`。 消費者は 2 つだけで、
// どちらも AudioTap の "使い方" であって新しいルートではない:
//   - Consumer A: `AuxInputRoute` → プラグイン aux 入力 (旧 sidechain を吸収)
//   - Consumer B: `ModSource` の envelope follower → param 変調 (`ModRouting`)

/// タップ点。 track の音をどの段で拾うか (plan §6, Q4)。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, Encode, Decode,
)]
pub enum TapPoint {
    /// device chain 適用前の素の音 (`pre_fx_l/r` snapshot として捕捉。
    /// engine.rs `resolve_tap_buf` / `any_pre_fx_tap` が消費)。
    PreFx,
    /// device chain 適用後・ fader 前 (`PreFaderScratch`)。
    PostFx,
    /// fader 後 (`TrackScratch`)。 旧 sidechain の既定。
    #[default]
    PostFader,
}

/// 「音をどこから取るか」 の SSoT。 source track + タップ点。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode,
)]
pub struct AudioTap {
    /// 音源となる `Track::id`。
    pub source_track: u32,
    /// どの段で拾うか。 旧 file は `PostFader` に forward-migrate。
    #[serde(default)]
    pub tap_point: TapPoint,
}

impl AudioTap {
    /// 旧 sidechain (= 常に post-fader) からの lift / 既定構築。
    pub fn post_fader(source_track: u32) -> Self {
        Self {
            source_track,
            tap_point: TapPoint::PostFader,
        }
    }
}

/// Consumer A: プラグイン aux 入力ルート。 旧 `sidechain_sources` を置換し、
/// 生音声を `pd.buffer_aux_in[port]` に staging する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct AuxInputRoute {
    pub tap: AudioTap,
}

impl AuxInputRoute {
    /// 旧 sidechain (常に post-fader) からの lift / 既定構築。
    pub fn post_fader(source_track: u32) -> Self {
        Self {
            tap: AudioTap::post_fader(source_track),
        }
    }
}

/// Consumer B: プラグイン aux **出力** ルート (パラアウト、`docs/plan_paraout.md`)。
/// `AuxInputRoute` の対称。プラグインの `is_main=false` な出力ポート 1 本を
/// どのトラックへ流すかを表す。`PluginInstance::aux_outputs[port_idx]` に格納し、
/// `daw_plugin_host` が `pd.buffer_aux_out[port]` に書いた音を engine の
/// `NodeOp::ParallelOutTap` が `dest_track` の入力へ加算する。サイドチェインと
/// 違いタップ点 (PreFx/PostFx/PostFader) は無い (出力は常に dest の入力へ入る)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct AuxOutputRoute {
    /// この aux 出力ポートの行き先 `Track::id`。
    pub dest_track: u32,
}

impl AuxOutputRoute {
    pub fn to_track(dest_track: u32) -> Self {
        Self { dest_track }
    }
}

/// エンベロープフォロワーの検出モード (plan §3)。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode,
)]
pub enum FollowerMode {
    /// ピーク (max|L|,|R|)。
    #[default]
    Peak,
    /// RMS (sqrt(½(L²+R²)))。
    Rms,
}

/// キック抽出用の帯域フィルタ (plan §3, Q3)。 検出前に source 音へ適用。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct BandFilter {
    /// ハイパス cutoff (Hz)。
    pub hp_hz: f32,
    /// ローパス cutoff (Hz)。
    pub lp_hz: f32,
}

/// フォロワー解析パラメータ。 状態 (env/biquad) はエンジン所有リングに置き、
/// ここには設定のみ (RT-safe・ Copy、 plan §10)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct FollowerConfig {
    pub mode: FollowerMode,
    /// 検出前に全波整流するか。
    pub rectify: bool,
    pub attack_ms: f32,
    pub release_ms: f32,
    /// 検出前ゲイン。
    pub gain: f32,
    /// キック抽出等の帯域制限。 `None` で全帯域。
    #[serde(default)]
    pub band_filter: Option<BandFilter>,
}

impl Default for FollowerConfig {
    fn default() -> Self {
        Self {
            mode: FollowerMode::Peak,
            rectify: true,
            attack_ms: 1.0,
            release_ms: 100.0,
            gain: 1.0,
            band_filter: None,
        }
    }
}

// ---------------------------------------------------------------------
// モジュレーターのツマミの plain 値範囲 (r.md #88、
// `docs/plan_rmd_88_89_cross_modulation.md` §3.3)
// ---------------------------------------------------------------------
//
// **UI のスクラブ範囲と `automation::plain_to_norm` の対数正規化が同じ端点を使う**
// ための SSoT。 片方だけ動かすと「ツマミの端」 と「変調の端」 がずれ、 深さ 1.0 の
// routing がツマミの端に届かない (または届く前に飽和する) 形で静かに壊れる。

// Free rate (Hz) の下端 / 上端は generator 節の `MOD_RATE_HZ_MIN` / `MOD_RATE_HZ_MAX`。

/// フォロワーの attack / release (ms) の下端 / 上端。 対数目盛。
pub const MOD_FOLLOWER_TIME_MS_MIN: f32 = 0.1;
pub const MOD_FOLLOWER_TIME_MS_MAX: f32 = 60_000.0;
/// フォロワーの検出前ゲインの下端 / 上端 (線形倍率)。 対数目盛。
pub const MOD_FOLLOWER_GAIN_MIN: f32 = 0.01;
pub const MOD_FOLLOWER_GAIN_MAX: f32 = 8.0;
/// フォロワーの帯域フィルタ cutoff (Hz) の下端 / 上端。 対数目盛。
pub const MOD_BAND_HZ_MIN: f32 = 20.0;
pub const MOD_BAND_HZ_MAX: f32 = 20_000.0;

/// source rack の色割当 palette (Bitwig 流、 作成順に循環)。
pub const MOD_SOURCE_PALETTE: [[f32; 3]; 8] = [
    [0.30, 0.69, 1.00], // 青
    [1.00, 0.55, 0.26], // 橙
    [0.45, 0.85, 0.45], // 緑
    [0.95, 0.45, 0.75], // 桃
    [0.80, 0.65, 1.00], // 紫
    [1.00, 0.85, 0.30], // 黄
    [0.40, 0.85, 0.85], // 水
    [0.95, 0.45, 0.45], // 赤
];

fn default_mod_color() -> [f32; 3] {
    MOD_SOURCE_PALETTE[0]
}

/// 共有モジュレーション源 (1 source → 多 params、 plan Q2)。 `Song.mod_sources`
/// が route の唯一の所有者。 `id` で `ModRouting.source_id` から参照される。
/// envelope follower 専用から **generator 種別** (`kind`) へ一般化。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ModSource {
    /// 安定 id。 `0` は "未採番" sentinel、 `ensure_ids` が採番。
    #[serde(default)]
    pub id: u32,
    /// `docs/plan_modulation_routing_redesign.md` §6: このソースが**帰属する
    /// トラック** (Bitwig 流: モジュレーターはトラックに属する)。ソースは `id` で
    /// グローバル参照され続ける (= 他トラックの param も変調できる) が、inspector
    /// では帰属トラックの下にだけ列挙する。master 帰属は `MASTER_TRACK_ID`。
    /// `0` = legacy (どこにも表示しない。未リリース機能なので該当データは無い)。
    #[serde(default)]
    pub owner_track_id: u32,
    /// source 識別色 (depth リング/rack 用)。 作成時に palette から循環割当。
    #[serde(default = "default_mod_color")]
    pub color: [f32; 3],
    /// 変調器種別 (envelope follower / LFO / Random / MSEG / Steps)。
    #[serde(default)]
    pub kind: ModSourceKind,
}

impl ModSource {
    /// 作成順 `index` に対応する palette 色。
    pub fn palette_color(index: usize) -> [f32; 3] {
        MOD_SOURCE_PALETTE[index % MOD_SOURCE_PALETTE.len()]
    }

    /// envelope follower のときだけ `(tap, follower)` を返す (generator は `None`)。
    pub fn follower(&self) -> Option<(&AudioTap, &FollowerConfig)> {
        if let ModSourceKind::EnvelopeFollower { tap, follower } = &self.kind {
            Some((tap, follower))
        } else {
            None
        }
    }

    /// envelope follower の tap を可変借用 (generator は `None`)。
    pub fn follower_tap_mut(&mut self) -> Option<&mut AudioTap> {
        if let ModSourceKind::EnvelopeFollower { tap, .. } = &mut self.kind {
            Some(tap)
        } else {
            None
        }
    }
}

/// 変調の極性 (plan Q5)。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode,
)]
pub enum Polarity {
    /// 0..=1 を `depth*s` で加算。
    #[default]
    Unipolar,
    /// -1..=1 を `depth*(2s-1)` で加算。
    Bipolar,
}

/// Consumer B: param への変調 edge (加算スタック、 plan Q5)。
/// **lane 非依存** — `Track.mod_routings` / `Song.song_mod_routings` に置かれ、
/// `target` が指す param を `source_id` のフォロワー値で変調する
/// (`docs/plan_modulation_routing_redesign.md` §2)。Bitwig と同じく automation
/// レーンの有無に関係なく変調できる。base (= 変調前の値) は当該 target に
/// automation lane があればその値、無ければモデルの現在値 (ノブ / plugin param
/// 値キャッシュ)。`AutomationTarget` を内包するため `Copy` ではない。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ModRouting {
    /// 安定 id (r.md #89)。`0` は "未採番" sentinel、`ensure_ids` が採番。
    /// [`AutomationTarget::ModRoutingDepth`] が **この 1 本の深さ**を変調先として
    /// 指すために要る (Bitwig の modulation scaling)。
    #[serde(default)]
    pub id: u32,
    /// 変調先 param。lane と独立に param を直接指す。
    pub target: AutomationTarget,
    /// → `Song.mod_sources[*].id`。
    pub source_id: u32,
    /// target の *正規化* 領域での量 (-1..=1)。
    pub depth: f32,
    #[serde(default)]
    pub polarity: Polarity,
}

// =====================================================================
// Generator modulators (LFO / Random / MSEG / Steps) — docs/plan_fixme_56_modulators.md
// =====================================================================
//
// `ModSource` を envelope follower 専用から **generator 種別** へ一般化する。
// envelope follower は audio 入力に依存し engine ring が `env` を
// 算出するが、generator (LFO/Random/MSEG/Steps) は **`song_beat` の純粋関数** で
// audio に依存しない。よって ring 不要・状態レス・全経路 (RT preview / 音声書き出し
// / video export) で同一関数 → drift ゼロ・bounce 完全再現。評価は
// `common::modulators::generator_scalar`。出力は常に unipolar 0..=1 で、極性は
// 後段の `ModRouting.polarity` が担う (SSoT、 follower と同じ契約)。

/// Free (Hz) 指定の下限。`docs/plan_rmd_88_89_cross_modulation.md` Q6 (Vital 準拠)。
pub const MOD_RATE_HZ_MIN: f32 = 0.001;
/// Free (Hz) 指定の上限。制御グリッド 750Hz (64 sample @48k) の Nyquist 375Hz を十分下回る。
pub const MOD_RATE_HZ_MAX: f32 = 128.0;

/// rate をどちらの流儀で読むか。**両方の値は常に保持される** (r.md #88 Q5)。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode,
)]
pub enum ModRateMode {
    /// 音価同期 (`numerator` / `denominator` を使う)。
    #[default]
    Sync,
    /// 絶対周波数 (`hz` を使う)。
    Free,
}

/// 全 generator 共通の rate。 tempo-synced な音価と free-running な絶対周波数の
/// **両方の値を常に保持**し、`mode` でどちらを使うかだけを切り替える (r.md #88 Q5)。
/// 行き来しても値が消えない (Vital と同じ 2 コントロール構成)。
///
/// free でも壁時計でなく **song 秒** で評価するので決定論的。どちらの mode でも
/// 評価は「瞬時周波数 [`Self::base_hz`] の積分」に統一される
/// (`docs/plan_rmd_88_89_cross_modulation.md` §2.1)。未変調なら積分は閉形式と
/// 厳密に一致するので、既存曲の音は 1 サンプルも変わらない。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Encode, Decode)]
pub struct ModRate {
    pub mode: ModRateMode,
    /// 音価同期の分子。 `period_beats = 4.0 * numerator / denominator`
    /// (1/4=(1,4)→1拍, 1bar=(1,1)→4拍, 1/8三連=(1,12), 付点1/4=(3,8))。
    pub numerator: u32,
    pub denominator: u32,
    /// Free の絶対周波数 (Hz)。[`MOD_RATE_HZ_MIN`]..=[`MOD_RATE_HZ_MAX`]。
    pub hz: f32,
}

impl Default for ModRate {
    fn default() -> Self {
        // 1/4 note + 1Hz。
        Self {
            mode: ModRateMode::Sync,
            numerator: 1,
            denominator: 4,
            hz: 1.0,
        }
    }
}

impl ModRate {
    /// 音価同期の 1 周期 (拍)。`mode` に依らず `numerator`/`denominator` から決まる。
    #[must_use]
    pub fn period_beats(&self) -> f64 {
        let den = f64::from(self.denominator.max(1));
        (4.0 * f64::from(self.numerator) / den).max(f64::MIN_POSITIVE)
    }

    /// **変調前の瞬時周波数 (Hz)**。Sync は `bpm / (60 · period_beats)` で tempo に追従し、
    /// Free は `hz` そのもの。位相はこの値の積分で求める (§2.1)。
    #[must_use]
    pub fn base_hz(&self, bpm: f64) -> f64 {
        match self.mode {
            ModRateMode::Sync => bpm / (60.0 * self.period_beats()),
            ModRateMode::Free => f64::from(self.hz.clamp(MOD_RATE_HZ_MIN, MOD_RATE_HZ_MAX)),
        }
    }

    /// 音価を差し替える (mode は変えない)。
    pub fn with_division(mut self, numerator: u32, denominator: u32) -> Self {
        self.numerator = numerator;
        self.denominator = denominator.max(1);
        self
    }
}

// ---- 旧形式 (externally-tagged enum) からの migration -------------------
//
// v37 以前の .daw は `ModRate` を enum で保存していた:
//   {"Sync":{"numerator":1,"denominator":4}} / {"Free":{"hz":1.0}}
// 新形式は struct ({"mode":"Sync","numerator":1,"denominator":4,"hz":1.0})。
// 旧 2 形式と新形式の **3 つを受ける**。旧 Sync は hz を既定 1.0 で、旧 Free は
// 音価を既定 1/4 で埋める (欠けている側の値を既定で補完するだけで、使う側の値は不変)。

#[derive(Deserialize)]
struct LegacySyncBody {
    numerator: u32,
    denominator: u32,
}

#[derive(Deserialize)]
struct LegacyFreeBody {
    hz: f32,
}

/// **`serde(untagged)` は使わない。** untagged の判別は field 集合の pairwise 非交差に
/// 依存するので、`ModRate` に field を 1 つ足しただけで旧形式が silent に misparse
/// されうる (`scripts/arch_lint.sh` の UNTAGGED チェックが禁じているのはこれ)。
/// キーを直接見る visitor なら、新旧どちらの形も **明示的に**判別できる。
struct ModRateVisitor;

impl<'de> serde::de::Visitor<'de> for ModRateVisitor {
    type Value = ModRate;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ModRate (struct 形式、または旧 enum の {\"Sync\":..} / {\"Free\":..})")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<ModRate, A::Error> {
        let mut out = ModRate::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "mode" => out.mode = map.next_value()?,
                "numerator" => out.numerator = map.next_value()?,
                "denominator" => out.denominator = map.next_value()?,
                "hz" => out.hz = map.next_value()?,
                // 旧 externally-tagged enum。欠けている側は `default()` のまま。
                "Sync" => {
                    let b: LegacySyncBody = map.next_value()?;
                    out.mode = ModRateMode::Sync;
                    out.numerator = b.numerator;
                    out.denominator = b.denominator;
                }
                "Free" => {
                    let b: LegacyFreeBody = map.next_value()?;
                    out.mode = ModRateMode::Free;
                    out.hz = b.hz;
                }
                _ => {
                    let _: serde::de::IgnoredAny = map.next_value()?;
                }
            }
        }
        out.denominator = out.denominator.max(1);
        Ok(out)
    }
}

impl<'de> Deserialize<'de> for ModRate {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_map(ModRateVisitor)
    }
}

/// タイムライン変調器の retrigger。 per-note 鍵盤文脈は無いので、 壁時計・再生
/// イベント基準は採らない (決定論を壊す)。 phase は常に song 位置の関数 (plan §0)。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum RetriggerMode {
    /// phase = f(song_beat) を連続評価。 既定 (Bitwig "Sync" 相当)。 loop 跨ぎでも一致。
    #[default]
    FreeRun,
    /// phase = f(song_beat - anchor_beat)。 clip / loop 開始等の beat 基準。 MSEG OneShot 用。
    FromBeat { anchor_beat: f64 },
}

/// LFO 波形。 phase 0..=1 → unipolar 0..=1。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum LfoShape {
    #[default]
    Sine,
    Triangle,
    /// 上昇ノコギリ (ramp)。
    SawUp,
    /// 下降ノコギリ。
    SawDown,
    /// 矩形 (duty 50%)。
    Square,
    /// 可変 duty パルス。 `width` 0..=1。
    Pulse {
        width: f32,
    },
}

/// 周期波 LFO。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct LfoConfig {
    pub shape: LfoShape,
    pub rate: ModRate,
    /// cycle 内の開始オフセット 0..=1。
    pub phase: f32,
    pub retrigger: RetriggerMode,
}

impl Default for LfoConfig {
    fn default() -> Self {
        Self {
            shape: LfoShape::Sine,
            rate: ModRate::default(),
            phase: 0.0,
            retrigger: RetriggerMode::FreeRun,
        }
    }
}

/// 乱数変調器。 `seed` を保存し `hash(seed, step)` の純関数にして **オフライン再現**
/// を保証する (Vital は mt19937 をグローバル seed で非再現、 plan §0)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct RandomConfig {
    pub rate: ModRate,
    /// Bitwig の Stepped↔Smoothed 連続モーフ。 `0.0` = 完全階段 (sample & hold)、
    /// `1.0` = 隣接 step を smoothstep 補間 (滑らかな乱数)、 中間は両者の lerp。
    pub smooth: f32,
    /// 決定論のための seed (source 作成時に採番・保存。 UI で re-roll 可)。
    pub seed: u64,
    pub retrigger: RetriggerMode,
}

impl Default for RandomConfig {
    fn default() -> Self {
        Self {
            rate: ModRate::default(),
            // 既定は完全 smoothed (旧 RandomMode::Smooth 相当)。
            smooth: 1.0,
            seed: 0,
            retrigger: RetriggerMode::FreeRun,
        }
    }
}

/// MSEG の 1 ブレークポイント。 `time`/`value` は 0..=1、 `time` は単調増加で
/// `points[0].time == 0.0`。 `curve` は次セグメントへの tension (-1..=1、 0=linear、
/// +=凸、 -=凹)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct MsegPoint {
    pub time: f32,
    pub value: f32,
    pub curve: f32,
}

/// MSEG の 1 周の再生モード (per-note sustain はタイムライン文脈で無効なので除外)。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode,
)]
pub enum MsegPlayMode {
    /// 1 周のみ (FromBeat anchor から)。
    OneShot,
    /// 連続ループ。
    #[default]
    Loop,
    /// 折り返しループ (forward → backward)。
    PingPong,
}

/// 自由描画できる多段エンベロープ (Bitwig Curves/Segments、 Vital LineGenerator 相当)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct MsegConfig {
    /// 時刻昇順の breakpoint 列 (`points[0].time == 0.0`、 不変条件)。
    pub points: Vec<MsegPoint>,
    /// 1 周の長さ。
    pub rate: ModRate,
    pub play_mode: MsegPlayMode,
    pub retrigger: RetriggerMode,
}

impl Default for MsegConfig {
    fn default() -> Self {
        // 既定 = 三角 (0,0)-(0.5,1)-(1,0)。
        Self {
            points: vec![
                MsegPoint {
                    time: 0.0,
                    value: 0.0,
                    curve: 0.0,
                },
                MsegPoint {
                    time: 0.5,
                    value: 1.0,
                    curve: 0.0,
                },
                MsegPoint {
                    time: 1.0,
                    value: 0.0,
                    curve: 0.0,
                },
            ],
            rate: ModRate::default(),
            play_mode: MsegPlayMode::Loop,
            retrigger: RetriggerMode::FreeRun,
        }
    }
}

/// ステップシーケンサの進行方向。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode,
)]
pub enum StepsDirection {
    #[default]
    Forward,
    Backward,
    /// 端で折り返し。
    PingPong,
}

/// ステップシーケンサ (Bitwig Steps 相当)。 各 step の値 (0..=1) を順に出す。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct StepsConfig {
    /// step 値 0..=1 (最低 1 個)。
    pub values: Vec<f32>,
    /// 1 周 (全 step) の長さ。
    pub rate: ModRate,
    pub direction: StepsDirection,
    /// step 間の slew (0=階段、 >0 で隣接 step を補間)。
    pub slew: f32,
    pub retrigger: RetriggerMode,
}

impl Default for StepsConfig {
    fn default() -> Self {
        // 既定 = 8 step の上昇階段 (一目で sequencer と分かる)。
        Self {
            values: (0..8).map(|i| i as f32 / 7.0).collect(),
            rate: ModRate::default(),
            direction: StepsDirection::Forward,
            slew: 0.0,
            retrigger: RetriggerMode::FreeRun,
        }
    }
}

/// **モジュレーター自身のツマミ** (r.md #89 クロスモジュレーション)。
/// [`AutomationTarget::ModSourceParam`] が `ModSource::id` と組で指す。
///
/// 「名前の付いたツマミ」だけを対象にする (`docs/plan_rmd_88_89_cross_modulation.md` Q3)。
/// MSEG の 1 点 / Steps の 1 段は対象外 — 参照実装 (Bitwig / Vital / Surge) も同じ線引きで、
/// 対象にすると新しい永続 positional 参照が要る (不変条件 1)。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode,
)]
pub enum ModParam {
    /// 実効周波数。log2 領域で正規化する ([`crate::automation::plain_to_norm`])。
    /// LFO / Random / MSEG / Steps 共通。Sync mode でも効き、音価から**連続に**外れる (Q1)。
    Rate,
    /// LFO の開始位相 φ (0..=1)。
    LfoPhase,
    /// LFO Pulse の duty (0..=1)。shape が `Pulse` でないときは無視される。
    LfoPulseWidth,
    /// Random の Stepped↔Smoothed モーフ (0..=1)。
    RandomSmooth,
    /// Steps の slew (0..=1)。
    StepsSlew,
    /// フォロワー attack (ms)。
    FollowerAttack,
    /// フォロワー release (ms)。
    FollowerRelease,
    /// フォロワーの検出前ゲイン。
    FollowerGain,
    /// 帯域フィルタ HP cutoff (Hz)。`band_filter` が `None` のときは無視される。
    FollowerHpHz,
    /// 帯域フィルタ LP cutoff (Hz)。同上。
    FollowerLpHz,
}

impl ModParam {
    /// 全 variant (RT の固定長配列を張るための SSoT。順序 = 配列の添字)。
    pub const ALL: [ModParam; 10] = [
        Self::Rate,
        Self::LfoPhase,
        Self::LfoPulseWidth,
        Self::RandomSmooth,
        Self::StepsSlew,
        Self::FollowerAttack,
        Self::FollowerRelease,
        Self::FollowerGain,
        Self::FollowerHpHz,
        Self::FollowerLpHz,
    ];

    /// [`Self::ALL`] 内の添字。固定長配列のキーに使う。
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            Self::Rate => 0,
            Self::LfoPhase => 1,
            Self::LfoPulseWidth => 2,
            Self::RandomSmooth => 3,
            Self::StepsSlew => 4,
            Self::FollowerAttack => 5,
            Self::FollowerRelease => 6,
            Self::FollowerGain => 7,
            Self::FollowerHpHz => 8,
            Self::FollowerLpHz => 9,
        }
    }

    /// UI ラベル (ラック / レーン名で共有する SSoT)。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Rate => "速さ",
            Self::LfoPhase => "位相",
            Self::LfoPulseWidth => "幅",
            Self::RandomSmooth => "なめらかさ",
            Self::StepsSlew => "スルー",
            Self::FollowerAttack => "Attack",
            Self::FollowerRelease => "Release",
            Self::FollowerGain => "Gain",
            Self::FollowerHpHz => "HP",
            Self::FollowerLpHz => "LP",
        }
    }

    /// この param が `kind` に存在するか。存在しない組み合わせの routing / lane は
    /// 評価時に無視され、UI にも出さない (掃除の対象ではない — 種別を戻せば復活する)。
    #[must_use]
    pub fn exists_on(self, kind: &ModSourceKind) -> bool {
        match self {
            Self::Rate => kind.rate().is_some(),
            Self::LfoPhase | Self::LfoPulseWidth => matches!(kind, ModSourceKind::Lfo(_)),
            Self::RandomSmooth => matches!(kind, ModSourceKind::Random(_)),
            Self::StepsSlew => matches!(kind, ModSourceKind::Steps(_)),
            Self::FollowerAttack
            | Self::FollowerRelease
            | Self::FollowerGain
            | Self::FollowerHpHz
            | Self::FollowerLpHz => matches!(kind, ModSourceKind::EnvelopeFollower { .. }),
        }
    }
}

/// `ModSource` の変調器種別 (plan §1)。 envelope follower は既存を内包し、
/// generator 4 種を追加。 `Vec` を持つため `Copy` 不可 (`ModRouting` と同じ)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum ModSourceKind {
    /// 他トラック音声のエンベロープフォロワー (既存基盤)。
    EnvelopeFollower {
        tap: AudioTap,
        follower: FollowerConfig,
    },
    Lfo(LfoConfig),
    Random(RandomConfig),
    Mseg(MsegConfig),
    Steps(StepsConfig),
}

impl Default for ModSourceKind {
    fn default() -> Self {
        ModSourceKind::EnvelopeFollower {
            tap: AudioTap::post_fader(0),
            follower: FollowerConfig::default(),
        }
    }
}

impl ModSourceKind {
    /// generator 共通の rate (follower は `None`)。
    #[must_use]
    pub fn rate(&self) -> Option<ModRate> {
        match self {
            ModSourceKind::Lfo(c) => Some(c.rate),
            ModSourceKind::Random(c) => Some(c.rate),
            ModSourceKind::Mseg(c) => Some(c.rate),
            ModSourceKind::Steps(c) => Some(c.rate),
            ModSourceKind::EnvelopeFollower { .. } => None,
        }
    }

    /// generator 共通の retrigger (follower は `None`)。
    #[must_use]
    pub fn retrigger(&self) -> Option<RetriggerMode> {
        match self {
            ModSourceKind::Lfo(c) => Some(c.retrigger),
            ModSourceKind::Random(c) => Some(c.retrigger),
            ModSourceKind::Mseg(c) => Some(c.retrigger),
            ModSourceKind::Steps(c) => Some(c.retrigger),
            ModSourceKind::EnvelopeFollower { .. } => None,
        }
    }

    /// generator 共通の rate (follower は `None`)。
    pub fn rate_mut(&mut self) -> Option<&mut ModRate> {
        match self {
            ModSourceKind::Lfo(c) => Some(&mut c.rate),
            ModSourceKind::Random(c) => Some(&mut c.rate),
            ModSourceKind::Mseg(c) => Some(&mut c.rate),
            ModSourceKind::Steps(c) => Some(&mut c.rate),
            ModSourceKind::EnvelopeFollower { .. } => None,
        }
    }

    /// generator 共通の retrigger (follower は `None`)。
    pub fn retrigger_mut(&mut self) -> Option<&mut RetriggerMode> {
        match self {
            ModSourceKind::Lfo(c) => Some(&mut c.retrigger),
            ModSourceKind::Random(c) => Some(&mut c.retrigger),
            ModSourceKind::Mseg(c) => Some(&mut c.retrigger),
            ModSourceKind::Steps(c) => Some(&mut c.retrigger),
            ModSourceKind::EnvelopeFollower { .. } => None,
        }
    }

    /// 短い種別ラベル (UI rack ヘッダ用)。
    pub fn short_label(&self) -> &'static str {
        match self {
            ModSourceKind::EnvelopeFollower { .. } => "Follow",
            ModSourceKind::Lfo(_) => "LFO",
            ModSourceKind::Random(_) => "Rand",
            ModSourceKind::Mseg(_) => "MSEG",
            ModSourceKind::Steps(_) => "Steps",
        }
    }
}
