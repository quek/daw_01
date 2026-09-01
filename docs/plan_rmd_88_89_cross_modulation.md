# plan_rmd_88_89_cross_modulation.md — LFO の Hz 指定 (r.md #88) とクロスモジュレーション (r.md #89)

> **理想**: モジュレーターの出力は「曲の位置の関数」であり続け、rate を変調しても位相は連続で、
> 再生 / プレビュー / 書き出しの 3 経路がビット一致する。モジュレーターのあらゆるツマミ
> (速さ・位相・幅・なめらかさ・スルー・フォロワーの A/R/ゲイン/帯域、および 1 本 1 本の変調の
> 深さ) が、既存の ◉ arm + 深さドラッグという **同じ 1 つの操作** で変調先になる。Hz は参照実装と
> 同じ対数目盛で単位付きで表示され、拍 ⇄ Hz を往復しても値が失われない。
>
> **フェーズ分けせず最終形まで一気に完走する** (CLAUDE.md)。

既存の設計正本 [plan_modulation.md](plan_modulation.md) /
[plan_modulation_routing_redesign.md](plan_modulation_routing_redesign.md) /
[plan_fixme_56_modulators.md](plan_fixme_56_modulators.md) を **置き換えるのではなく上書き差分**として
読む。矛盾する箇所は本書が優先で、該当箇所には本書へのリンクを 1 行入れて原文は消す
(CLAUDE.md「規範も SSoT。原文を引用して再掲しない」)。

---

## 0. 確定した仕様 (ユーザー確定、2026-09-01)

| # | 分岐 | 確定 |
|---|---|---|
| Q1 | 音価同期の LFO の「速さ」を変調したとき | **音価から連続に外れる** (Surge / Bitwig と同じ) |
| Q2 | A↔B の輪になる接続 | **繋げる。輪の 1 箇所だけ 1 制御刻み (64 サンプル) 前の値で回る**。ラックに ⟳ バッジ |
| Q3 | 変調先にできるもの | **名前の付いたツマミ全部 + 変調 1 本の深さ**。MSEG の点 / Steps の各段は対象外 |
| Q4 | モジュレーターのツマミをオートメーションレーンにも出すか | **出す** |
| Q5 | 拍 ⇄ Hz の切替 | **両方の値を保持** (Vital 流)。dropdown 末尾は `"Free"` → `"Hz"` に改名 |
| Q6 | Hz の範囲 | **0.001 〜 128 Hz、対数目盛、単位 "Hz" 表示** (Vital 準拠) |
| Q7 | Follow が速さの鎖に入ったとき | **許す**。位置依存バッジを出す |
| Q8 | クロス変調中のプレビュー | **再生位置中心の時間窓スクロール + 変調前の形を薄く重ねる** |
| Q9 | 深さの変調の操作 | **深さ欄を普通のツマミとして扱う** (◉ 待受中にドラッグ) |

聞かずに決めた事項 (理想が一意なので上流判断を要さない):

- **変調の適用解像度を 64 サンプル刻みに上げる**。既存の automation サブバッファ刻み
  (`daw_audio/src/automation.rs` の 64 frame イベント) と同じ粒度に揃える。現在は 1 buffer 1 スカラー
  (≒46Hz) なので、Hz 上限が原理的に鳴らないうえ live (device buffer 長) と export (1024 固定) で
  ZOH の段差位置が違い **今日すでに音が一致していない**。これを閉じる。
- **Free の retrigger (⟲here) を効かせる**。現在は silent no-op なのに dirty / undo は積む
  (`plan_fixme_56_modulators.md:106-107` が要求していた `map_secs` が未実装)。
- **Hz 欄に単位 "Hz" を出す / 対数スクラブにする / widget id を安定 id にする**。
- **フォロワーの gain / mode / rectify / band_filter に直接編集の口を作る**。変調先にするのに
  素の値を編集できない非対称を残さない。
- **`draw_modulation_rack` を種別ごとの関数へ割る**。arch-lint baseline に天井 (515 実コード行 /
  ネスト 9 段) が登録済みで、太らせるとゲートが落ちる。

---

## 1. 現状の欠陥一覧 (#88 の実体)

`ModRate::Free { hz }` と rate dropdown の `"Free"` は既に存在するが、次の 7 点で使い物にならない。
本書はこれを全部閉じる。

| # | 欠陥 | 根拠 |
|---|---|---|
| 1 | Hz 欄に単位表示が無い (描画されるのは `"1.00"` だけ) | `modulation_rack.rs:246-263`。兄弟欄は全部ラベル付き (`φ` `w` `Smooth` `slew` `A` `R`) |
| 2 | 線形目盛 (`sensitivity: 0.05`) で 0.01→50Hz 全域に約 1000px のドラッグが要る | `modulation_rack.rs:243`、`scrubable_number.rs:117-120` (units_per_pixel) |
| 3 | 拍 ⇄ Hz を切り替えると hz が必ず `1.0` にリセットされる | `modulation_rack.rs:55-63`。`ModRate` が enum で片方の値しか持てない |
| 4 | Free では retrigger が完全に無視される (⟲here が silent no-op、dirty/undo は積む) | `modulators.rs:36-52` の Free 分岐が `retrigger` を使わない。UI は `c.rate` を見ずにトグルを描く (`modulation_rack.rs:593/637/690/767`) |
| 5 | rate の `"Free"` と retrigger の `"Free"` がラベル衝突 (LFO では 22px 上下に並ぶ) | `modulation_rack.rs:43` と `:283` |
| 6 | 変調の適用が buffer 単位 (≒46Hz) なので Hz 上限 50 は原理的に鳴らない | `daw_audio/src/automation.rs:113-126` (mod_scalars は buffer 定数) / `:257-270` (`push_param_mod(0, ..)` は buffer 1 イベント) |
| 7 | GUI プレビューだけ秒換算を自作していて、停止中にテンポカーブ下でズレる | `modulation_rack.rs:327-328` `secs = beat*60/bpm` vs `engine.rs:1319` / `export.rs:693` `playhead/sample_rate` |

加えて `ModRate::Free` の保存往復テストが 1 本も無い (generator の save/load テストが皆無)。

---

## 2. 核心アーキテクチャ — 位相は積分でしか得られない

参照実装の一次情報が結論を出している。

- **Vital** `src/synthesis/modulators/synth_lfo.cpp` `processControlRate()`:
  `control_rate_state_.offset += frequency * time_passed;` — 全 sync_type で **無条件に積分**。
  閉形式 `frac(rate×t)` を使うのは `processTrigger()` の reset 枝だけ。
- **Surge XT** `LFOModulationSource.cpp` `attackFrom()` の `case lm_freerun:` に
  「Get our time from songpos … And so the total phase is timePassed * rate + phase0」
  — 閉形式は **locate 時のシード専用**で、以降は `process_block()` が積分する。
- **理論** (JOS III, CCRMA): 瞬時周波数は瞬時位相の時間微分。よって位相は周波数の積分でしか
  得られない。rate を変調しながら `frac(rate(t)·t)` を評価すると、rate が変わった瞬間に過去の
  全区間へ遡って新 rate が適用されるため位相が跳ぶ (FM の古典的な罠)。

daw_01 の現行 `cycle_pos` はまさに Surge がシードに使っている式そのもので、継続評価には使えない。

### 2.1 瞬時周波数への統一

Sync も Free も **瞬時周波数 (Hz)** に落として同じ積分器に入れる。

```text
f_base(t) = Sync のとき  bpm(t) / (60 · period_beats)       // period_beats = 4·num/den
            Free のとき  hz
f_eff(t)  = f_base(t) · 2^( rate_offset_norm(t) · LOG2_SPAN )   // LOG2_SPAN = log2(128/0.001) ≈ 17
φ(t)      = φ(0) + ∫ f_eff dt
```

- **未変調なら閉形式と厳密に一致する**。Sync は `∫ bpm/(60·period) dt = beat/period`、
  Free は `∫ hz dt = secs·hz`。よって **rate が変調されていないソースは今日と完全に同じ値**
  (bit 一致) を返す。これを回帰テストで固定する (§8)。
- Sync のまま rate を変調すると音価から連続に外れる (Q1 = 1)。テンポ追従は `f_base` に
  `bpm(t)` が入っているので保たれる。

### 2.2 制御グリッド

**64 サンプル刻み、絶対 song サンプル位置に整列**。既存 automation のサブバッファ刻みと同じ。

- `tick_index k` = `absolute_sample / 64`。buffer 境界に依存しない ⇒ live (device buffer 長可変) と
  export (1024 固定) で **同じ刻み列**を踏む。同じ sample rate なら live と WAV がビット一致する。
- 積分は `φ_{k+1} = φ_k + f_eff(t_k) · 64 / SR` (f64 加算)。
- 変調の **適用**も同じ刻み: daw param (volume/pan/image/text/group) は tick ごとに
  `apply_modulation` を引き直して tick 内は線形補間、plugin param は tick ごとに
  frame offset 付き `push_param_mod` を出す。

> **`ProcessData::MAX_EVENTS` (=256) の共有枯渇**: `push_param_mod` が 1 buffer 1 発から最大 16 発に
> 増える。`events_in` はノート・automation サブバッファイベントと **同じ 256 枠を共有**し、溢れは
> `if i >= MAX_EVENTS { return }` で **silent drop** (`process_data.rs:294-297` / `:358-363`)。
> よって **param modulation は `events_in` と別の専用配列 `param_mods: [ParamMod; MAX_PARAM_MODS]`
> へ分離する** (ノートを変調が押し出す事故を構造的に不可能にする)。容量は
> `MAX_PARAM_MODS = 1024` (16 tick × 64 param)。溢れたら `#[cfg(debug_assertions)]` で警告し、
> release では最後の tick を優先する (先頭を捨てる = 最新が勝つ)。

### 2.3 tier — 位置依存かどうか

| tier | 条件 | 位相の求め方 | 見え方 |
|---|---|---|---|
| **closed** | rate が変調されていない | 閉形式 (今日と同じ、O(1)) | バッジ無し |
| **integrated** | rate が変調されており、鎖に follower を含まない | **ModPhaseTable** から breakpoint を引いて ≤512 tick 前進 | バッジ無し (どこから再生しても同じ位相) |
| **audio** | rate が変調されており、鎖に follower を含む | locate 時に閉形式でシード → 以降積分 | ラックに **「位置依存」バッジ** |

### 2.4 ModPhaseTable

`common/src/tempo_map.rs` と同型 (off-thread build / lookup は alloc・lock 無し / `RtBundle` で
engine へ渡り旧 map は recycle)。

- **breakpoint は 512 tick ごと** (= 32768 サンプル ≒ 0.68 秒 @48k)。breakpoint が **必ず grid tick に
  乗る**ので、breakpoint から前進した和が曲頭からの和と **厳密に一致**する (近似ではない)。
- 各 integrated ソースについて `phase: Vec<f64>` を持つ。closed / audio tier は表を張らない。
- **ハードキャップ必須** — `tempo_map.rs:17-20` の `MAX_TABLE_BEATS` と同じ理由で、
  `MAX_TABLE_SECS = 24 * 3600.0` (24 時間) を超える曲長では表を張らず audio tier に倒す。
  破損 / 悪意ある project の巨大 `length_beats` で `Vec::with_capacity` が OOM しないこと。
- 表の再構築は off-thread。構築中は旧表 + 前進で凌ぎ、完成したら世代付きで swap。

### 2.5 依存グラフと輪

`common/src/mod_graph.rs` (新規) が **唯一の判定者**。GUI も engine も export もこの 1 本を引く
(`AutomationTarget::accepts_launcher_cells` と同じ「片側だけで弾かない」規約)。

```rust
/// off-RT で Song から作る。RT は読むだけ。
pub struct ModPlan {
    /// トポロジカル順 (輪の back-edge は切ってある)。
    pub nodes: Vec<ModNode>,
    /// slot ↔ ModSource::id。RT の値面と GUI の表示を id で結ぶ (不変条件 1)。
    pub slot_ids: Vec<u32>,
    /// `slot_ids` / 値面を跨いで読む瞬間のレースを塞ぐ世代。
    pub generation: u64,
}

pub struct ModNode {
    pub source_id: u32,
    pub kind: ModSourceKind,          // off-RT で clone (RT では clone しない)
    pub tier: ModTier,                // Closed | Integrated | Audio
    pub in_edges: Vec<ModEdge>,       // param ごとの入力辺
    pub in_cycle: bool,               // ⟳ バッジ用
}

pub struct ModEdge {
    pub param: ModParam,
    pub src_slot: u16,
    pub depth: f32,
    pub polarity: Polarity,
    /// 輪を開くために **1 制御刻み前の値**を読む辺 (DFS の back-edge)。
    pub delayed: bool,
}
```

- 輪の検出 = DFS。back-edge を `delayed: true` にして開く (Q2 = 1)。
- `in_cycle` は輪に属する全ノードに立てる (GUI の ⟳ バッジ)。
- tier 判定は「rate に入る辺の推移閉包に follower が居るか」。

### 2.6 RT ランタイム

```rust
/// 事前確保のみ。alloc / lock / I/O 無し。
pub struct ModRuntime {
    phase: [f64; MAX_MOD_SOURCES],
    value: [f32; MAX_MOD_SOURCES],
    prev:  [f32; MAX_MOD_SOURCES],   // delayed 辺が読む
    seeded_tick: i64,
}

/// 1 制御刻みを進める。plan のトポロジカル順に評価する。
pub fn tick(
    plan: &ModPlan,
    rt: &mut ModRuntime,
    follower_env: &[f32],   // audio tier の follower 出力 (engine ring)
    ctx: TickCtx,           // song_beat / song_secs / bpm / dt_secs / tick_index
);
```

`ModSourceKind` は `MsegConfig.points` / `StepsConfig.values` の `Vec` を持つので **RT で clone しない**。
param のオーバーライドは `Copy` な `ModOffsets` 構造体で渡す。

---

## 3. モデル (`common/`)

### 3.1 `ModRate` — 拍と Hz を両方保持する

```rust
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ModRate {
    pub mode: ModRateMode,
    /// Sync 側の音価。period_beats = 4·numerator/denominator。
    pub numerator: u32,
    pub denominator: u32,
    /// Free 側の周波数 (Hz)。0.001..=128。
    pub hz: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum ModRateMode { #[default] Sync, Free }

pub const MOD_RATE_HZ_MIN: f32 = 0.001;
pub const MOD_RATE_HZ_MAX: f32 = 128.0;
```

**旧形式からの migration**: 旧 JSON は enum の externally-tagged 表現
(`{"Sync":{"numerator":1,"denominator":4}}` / `{"Free":{"hz":1.0}}`)。
`Deserialize` を手書きし、旧 2 形式と新 struct 形式の **3 つを受ける**。旧 `Sync` は
`hz` を既定 1.0 で、旧 `Free` は音価を既定 1/4 で埋める。
bincode の wire fingerprint が変わるので **`make build` で 3 exe を揃える** (不変条件 7、
`common/build.rs:23` に `src/model/modulation.rs` は登録済み)。

### 3.2 `ModParam` と新しい変調先

```rust
/// モジュレーター自身のツマミ (r.md #89)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum ModParam {
    /// 実効周波数。log2 領域で正規化 (§2.1)。LFO / Random / MSEG / Steps 共通。
    Rate,
    /// LFO の φ (0..=1)。
    LfoPhase,
    /// LFO Pulse の width (0..=1)。shape が Pulse でないときは無視。
    LfoPulseWidth,
    /// Random のなめらかさ (0..=1)。
    RandomSmooth,
    /// Steps のスルー (0..=1)。
    StepsSlew,
    /// フォロワー attack (ms, 0..=60000, log 正規化)。
    FollowerAttack,
    /// フォロワー release (ms, 0..=60000, log 正規化)。
    FollowerRelease,
    /// フォロワー gain (0..=8, log 正規化)。
    FollowerGain,
    /// 帯域フィルタ HP / LP (Hz, 20..=20000, log 正規化)。
    FollowerHpHz,
    FollowerLpHz,
}
```

`AutomationTarget` に 2 variant を足す:

```rust
    /// r.md #89: モジュレーター自身のツマミ。`source_id` = `ModSource::id`。
    ModSourceParam { source_id: u32, param: ModParam },
    /// r.md #89: 変調 1 本の深さ (Bitwig の modulation scaling)。
    /// `routing_id` = `ModRouting::id`。
    ModRoutingDepth { routing_id: u32 },
```

- **`ModRouting` に安定 id を足す** (`pub id: u32`)。採番は `IdAllocators`
  (`common/src/model/ids.rs`) に `next_mod_routing_id` を追加、`ensure_ids` が sentinel(0) を埋める
  — `alloc_mod_source_id` と同じ形。
- **置き場**: `ModSourceParam` の routing / lane は **そのソースの `owner_track_id` のトラック**
  (`MASTER_TRACK_ID` なら song 側)。`ModRoutingDepth` は **その routing が置かれている場所と同じ**。
  `AutomationTarget::scope()` のような **全域関数を作ってはならない** — master fx chain の
  `PluginParam` は target だけからは track/song を決められない (`daw_audio/src/automation.rs:180-193`)。
  解決は「対象の ModSource / ModRouting を引いてその置き場を返す」限定述語で行う。

### 3.3 正規化 (`plain_to_norm` / `norm_to_plain`)

新 target の arm を **両方の関数に対で足す** (互いの厳密逆であること)。

| param | plain | norm |
|---|---|---|
| `Rate` | 実効 Hz | `(log2(hz) - log2(0.001)) / log2(128/0.001)` |
| `LfoPhase` / `LfoPulseWidth` / `RandomSmooth` / `StepsSlew` | 0..=1 | 恒等 |
| `FollowerAttack` / `FollowerRelease` | ms 0.1..=60000 | log |
| `FollowerGain` | 0.01..=8 | log |
| `FollowerHpHz` / `FollowerLpHz` | 20..=20000 | log |
| `ModRoutingDepth` | -1..=1 | `(depth+1)/2` |

### 3.4 掃除 (dangling を作らない)

`ModSource` / `ModRouting` の削除は **同件を全部** 潰す (memory `feedback_sibling_occurrence_check`)。
`remove_mod_source(id)` は現在 `r.source_id != id` の 2 行しか見ていない。追加で:

1. `target == ModSourceParam { source_id: id, .. }` の routing / lane を全 track + song から除去。
2. そのソースを指していた routing が消えることで生じる `ModRoutingDepth { routing_id }` の
   dangling を再帰的に除去 (1 パスで固定点まで回す)。

`remove_mod_routing` も同様に `ModRoutingDepth { routing_id }` を掃除する。
`Song::ensure_ids` / `normalize_after_load` は **冪等** であること (r.md #9 の派生データ collapse 契約)。

---

## 4. エンジンと書き出し (`daw_audio/`)

1. `compile_schedule` が `common::mod_graph::build_plan(song)` を呼び `ModPlan` を作る。
   現在 `mod_kinds` / `follower_slots` が `Song::mod_sources` の **位置**で並んでいるのを
   `slot_ids` 経由の id 参照へ移す (不変条件 1)。
2. `AudioBridge` に `mod_slot_ids: [u32; MAX_MOD_SOURCES]` と `mod_plane_generation: AtomicU64` を
   追加。GUI は「世代を読む → id 表と値面を読む → 世代を読み直して一致を確認」で読む
   (seqlock)。`source_scalar` の `position(|m| m.id == source_id)` 線形探索 (per-sample 経路に
   乗っている) を id → slot の表引きに置き換える。
3. `process_buffer` は buffer を **64 サンプルの tick** に割り、tick ごとに `mod_graph::tick` を回す。
   - daw param: tick 境界で `apply_modulation` を引き直し、tick 内は線形補間。
   - plugin param: tick ごとに frame offset 付きで `param_mods` 配列へ push (§2.2)。
   - follower: 現行どおり buffer 末で env を更新。**audio tier の遅れは 1 tick に固定**する
     (live/export で遅れ量が違う現状を閉じる)。
4. `export.rs` は同じ tick ループを通す。`render_master_buffer` を共有する不変条件 6 と同じ理由で、
   **tick ループは engine と export で同じ関数**にする (`daw_audio/src/mod_tick.rs` に切り出す)。
5. `mod_sidecar` (動画書き出し) は slot ではなく **id** で焼く。フォーマット変更なので
   magic を `MOD2` に上げ、旧 sidecar は読めなくてよい (再生成される派生物)。
6. `ModPhaseTable` は off-thread build → `RtBundle` に載せて engine へ、旧表は recycle
   (`engine.rs:885` / `:955` の既存経路)。

---

## 5. GUI (`daw_gui/` + `ui/`)

### 5.1 daw-ui の拡張 (ライブラリ側を直す)

`ScrubableNumberStyle` に 2 つ足す。daw_01 固有の知識は持ち込まない (不変条件 8)。

```rust
pub enum ScrubCurve { Linear, /// 対数 (range の下端 > 0 が前提)
                       Log }
pub struct ScrubableNumberStyle {
    // 既存 …
    pub curve: ScrubCurve,
    /// 値の後ろに出す単位 ("Hz" / "ms" / "dB")。空なら出さない。
    pub unit: &'static str,
}
```

- `curve: Log` のとき、ドラッグ量は **正規化領域**で効く (全域が数百 px で舐められる)。
- `draw_modulation_overlay` の `value_to_x` も同じカーブを通す (深さリングが対数軸で正しく出る)。
- 変更に伴い `ui/crates/examples/` と `ui/docs/plan.html` を **同じ commit で**更新する。

### 5.2 ラック

- `draw_modulation_rack` を **種別ごとの関数へ分割** (`draw_lfo_body` / `draw_random_body` /
  `draw_mseg_body` / `draw_steps_body` / `draw_follower_body` / `draw_routing_row`)。
  arch-lint baseline の該当行を新しい実測値へ更新する (下げる方向なので ratchet 的に正しい)。
- **widget id を安定 id (`sid`) に統一**。現在 rate / Hz だけ位置 index `i` を使っており、
  削除・並べ替えでドラッグ状態が別ソースへ乗り移る (不変条件 1 の綻び)。
- **rate 行**: dropdown の末尾を `"Hz"` に改名 (retrigger の `"Free"` との衝突解消)。
  `Hz` を選ぶと Hz スクラバ (対数 0.001..=128、単位 "Hz" 表示) が出る。音価に戻しても
  `hz` は保持され、Hz に戻すと前の値が復活する。
- **すべてのツマミが変調先になる**: rate / φ / width / smooth / slew / follower A・R・gain・帯域 は
  `build_mod(app, AutomationTarget::ModSourceParam { source_id, param }, …)` を渡した
  `scrubable_number_at` にする。深さ欄は `ModRoutingDepth { routing_id }`。
  **既存の per-control idiom をそのまま流用し、bespoke な widget を新設しない**
  (memory `feedback_reuse_inspector_idiom`)。
- **フォロワーの gain / mode / rectify / band_filter に編集 UI を足す** (現在は口が無い)。
  `SetModSourceGain` / `SetModSourceMode` / `SetModSourceRectify` / `SetModSourceBand` を追加。
- **バッジ**: 輪に属するソースに `⟳`、audio tier に「位置依存」。ヘッダ行に出す
  (行を太らせない)。可変背景ではないが、暗チップ + 明色文字でコントラストを保証する。
- **プレビュー** (Q8): クロス変調が掛かっているソースは、再生位置中心の時間窓をスクロールさせ、
  変調前の形を薄く重ねる (Random が既にこの形: `modulation_rack.rs:178-184`)。カーソル位相は
  **engine が publish する実位相** (`mod_phases`) を読む — 自前計算しない。
- **プレビューの秒 SSoT** (#88-7): `beat*60/bpm` をやめ、`common::automation::beats_to_samples` を
  通した秒に寄せる。**beat と secs は必ず同時に同じ SSoT へ寄せること** — 片方だけ直すと
  再生中に成立していた偶然の一致が壊れて悪化する。

### 5.3 オートメーションレーン (Q4)

`ModSourceParam` / `ModRoutingDepth` をレーン一覧に出す。

- 表示名: `automation_target_display_name` / `lane_target_display` に arm を足す。
  `"LFO 1 ▸ 速さ"` / `"LFO 1 → Vol の深さ"` の形。ソース名は `ModSource::short_label()` + 通し番号。
- `lane_default_for_target` に既定値を足す。
- レーンは帰属トラック (`owner_track_id`) の下に出る。
- **新 variant で黙って no-op に落ちる wildcard match を全部洗う**。コンパイルエラーで気付けるのは
  5 か所だけ (`plain_to_norm_ranged` / `norm_to_plain_ranged` / `automation_target_display_name` /
  `lane_target_display` / `lane_default_for_target`)。残りは grep で全件確認する。

### 5.4 arm の到達範囲

`connect_armed_mod_source_to` は現在 2 経路 (`handler/ipc.rs:351` の `PluginParamTouched`、
`view/modulation.rs` の depth ドラッグ終端)。ラックのツマミも後者を通るので **新しい合流点は作らない**。

輪になる接続は **拒否しない** (Q2 = 1) ので、arm 中は全ツマミに枠が出る。

---

## 6. アーキテクチャ不変条件との整合

| 不変条件 | 本設計での守り方 |
|---|---|
| 1 安定 id | `ModRouting.id` を新設。`mod_scalars` の positional slot と sidecar の slot を id 表 + 世代へ。widget id も `sid` に統一 |
| 2 wire blob-less | protocol に何も足さない。`ModPlan` / `ModPhaseTable` は engine 内 (`RtBundle`)、bulk は shmem 固定長配列と sidecar |
| 3 宛先は型 | 変更なし |
| 4 RT | tick は alloc / lock / 無限待ち無し。`ModSourceKind` を RT で clone しない。表構築は off-thread + ring swap。locate の前進は ≤512 tick で有界 |
| 5 edit_song | 全編集が `edit_song` 経由。クロス変調はモデルを書き換えず **評価時 override** なので undo / dirty は無傷 |
| 6 live/export 同一関数 | tick ループを `daw_audio/src/mod_tick.rs` に切り出して engine と export が共有 |
| 7 fingerprint | wire 型を新ファイルへ移さない。`ModRate` の表現変更は `common/build.rs:23` の登録済みファイル内なので検出網は効く。`make build` 必須 |
| 8 daw-ui core | 足すのは `ScrubCurve` / `unit` のみ。DAW 固有の型は持ち込まない |
| 9 サイズ budget | `draw_modulation_rack` を分割してから足す。`mod_graph.rs` / `mod_tick.rs` は新規ファイルに置く |

---

## 7. 作業の分割 (worktree 並列)

`rmd-88-89-core` を先に完成させ、そこから 3 本を並列に生やす (型が共有なので core が上流)。

| worktree | 範囲 | 依存 |
|---|---|---|
| `rmd-88-89-core` | `common/`: `ModRate` 表現 + migration / `ModParam` + `AutomationTarget` 2 variant / `ModRouting.id` + allocator / `plain_to_norm`・`norm_to_plain` / `mod_graph.rs` (plan・輪・tier) / `modulators.rs` の積分化 / `ModPhaseTable` / 掃除 / 単体テスト | — |
| `rmd-88-89-engine` | `daw_audio/`: 64 サンプル tick ループ (`mod_tick.rs`) / `param_mods` 分離 / id 表 + 世代 / `ModPhaseTable` 配送 / export 共有 / sidecar MOD2 | core |
| `rmd-88-89-rack` | `ui/` の `ScrubCurve`・`unit` + 全 example/doc 更新、`daw_gui` のラック分割・rate UI・全ツマミ変調先化・follower 編集 UI・バッジ・プレビュー | core |
| `rmd-88-89-lanes` | `daw_gui`: レーン一覧 / 表示名 / 既定値 / wildcard match の全件洗い出し / 掃除の GUI 側 | core |

統合順: `core` → `engine` → `rack` → `lanes`。

---

## 8. テスト (高いレイヤーから)

**自明な算術をテストへ写すだけのテストは書かない** (memory `feedback_no_tests_for_simple_cases`)。
書くのは次の 6 本だけ。

1. `未変調のrateは閉形式とbit一致する` — 全 4 generator × Sync/Free × 複数 beat で、
   積分経路と `cycle_pos` 閉形式が **bit 一致**すること。これが「既存曲の音を変えない」の担保。
2. `ModRateの旧形式2種と新形式が同じ値へloadされる` — 旧 `{"Sync":…}` / `{"Free":…}` / 新 struct の
   3 形式 + bincode 往復。#88 で唯一存在しなかった保存テスト。
3. `輪はback-edgeが1刻み遅延で開き決定論的` — A↔B を張り、同じ tick 列を 2 回回して bit 一致、
   かつ `in_cycle` が両ノードに立つこと。
4. `rate変調時の位相がbreakpointからの前進と曲頭からの通しで一致する` — ModPhaseTable の
   厳密一致 (近似ではない) の担保。
5. `sourceを消すと自分を指すModSourceParam/ModRoutingDepthも消える` — 掃除の同件担保。
   固定点まで回ること (連鎖する dangling)。
6. `3経路(engine/export/GUIプレビュー)が同じ song 位置で同じ値を返す` — テンポオートメーション
   ありの曲で。#88-7 の再発防止で、設計正本が要求していたのに存在しなかったもの。

`make test-nolaunch` / `make clippy` / `make arch-lint` を通す。視覚は実機 sign-off。

---

## 9. 既存文書の更新

- `plan_fixme_56_modulators.md` — 「generator は `song_beat` の純粋関数」という核心 SSoT が
  **rate 変調時に限り成立しない**ので、該当節を本書へのリンク 1 行に置き換える。
- `plan_modulation.md` / `plan_modulation_routing_redesign.md` — `ModRouting` に id が付いたこと、
  変調先に `ModSourceParam` / `ModRoutingDepth` が加わったことを 1 行ずつ追記してリンク。
- `scripts/arch_lint_baseline.txt` — `draw_modulation_rack` の天井を分割後の実測値へ更新。
