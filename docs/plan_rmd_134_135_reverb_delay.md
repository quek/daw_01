# r.md #134 / #135 内蔵 Reverb / Delay（`Device::Native` に 2 種を足す）

内蔵デバイスの土台は r.md #129 で完成している（[plan_rack_native_devices.md](plan_rack_native_devices.md)）。
本書はそこへ **Reverb / Delay の 2 種**を足すための差分設計。

- §1 種類非依存の配線（既存 4 種と同じ手順で触る全箇所）
- §2 RT 安全性（本件の設計の核）
- §3 音符値の SSoT 化
- §4 Reverb の値 / §5 Reverb の DSP
- §6 Delay の値 / §7 Delay の DSP
- §8 GUI

## 1. 新しい `NativeKind` を足すときに触る全箇所

`NativeKind` を網羅 match している箇所は 14 ファイル。追加は**すべて非 Option の match 腕**なので、
漏れれば型が捕まえる。静かに壊れるのは「表への登録漏れ」だけなので、それをここに列挙する。

### 1.1 common（model / wire）

| ファイル | 足すもの |
|---|---|
| `src/note_value.rs`（新設） | `NoteValue` / `NoteKind`（§3） |
| `src/model/native/reverb.rs`（新設） | `ReverbParam` / `ReverbSettings`（§4） |
| `src/model/native/delay.rs`（新設） | `DelayParam` / `DelaySettings` / `DelayDiv` / `DelayPattern` / `DelayMode` / `DelayDrive`（§6） |
| `src/model/native.rs` | `NativeKind::{Reverb,Delay}` / `ALL` / `label` / `undo_label` / `ports` / `picker_id` / `NativeParams::{Reverb,Delay}` / `default_of` / `get` / `set` / `sanitize` |
| `src/model/native_param.rs` | `NativeParamId::{Reverb,Delay}` / `REVERB_ALL` / `DELAY_ALL` / `kind` / `exists` / `range` / `knob_label` / `lane_label` / `all_of` / `default_plain` / `step_labels` |
| `src/plugin_db.rs` | `NATIVE_REVERB_PICKER_ID` / `NATIVE_DELAY_PICKER_ID`（`builtin://daw_01.native.{reverb,delay}`） |
| `common/build.rs` `WIRE_SOURCES` | `src/model/native/reverb.rs` / `src/model/native/delay.rs`（**不変条件 7**。漏らすと protocol 変更が fingerprint に出ない） |
| `src/model.rs` | `CURRENT_VERSION` 43 → 44 |

**組み込み（`BUILTIN_TRACK` / `BUILTIN_MASTER`）には入れない。** 2 種とも picker から足す追加分
（`NativeDevice::new_added`、既定 ON）。よって `native/chain_rules.rs` と
`project/native_migration.rs` は無変更 — 旧プロジェクトに Reverb / Delay は存在しないので
migration も要らない（`CURRENT_VERSION` の bump は「新しい種類を含む曲を古いビルドで開かせない」ため）。

`accepts_sidechain` / `accepts_listen` / `has_gain_reduction` は 2 種とも false。

### 1.2 daw_audio

| ファイル | 足すもの |
|---|---|
| `src/native_dsp/reverb.rs`（新設） | `ReverbState`（§5） |
| `src/native_dsp/delay.rs`（新設） | `DelayState`（§7） |
| `src/native_dsp/mod.rs` | `NativeDsp::{Reverb,Delay}` / `new` / `kind` / `reset` / `process` / `adopt_state_from`、`NativeBlock` に `bpm` |
| `src/graph/native.rs` | `NativeScratch::new` に `sample_rate`、`run_native` から `bpm` を渡す |
| `src/graph/program_build.rs` | `build_program` に `sample_rate` を通す |
| `src/graph/compile/mod.rs` | `build_program` 呼び出し 2 か所に `sample_rate` |

### 1.3 daw_gui

| ファイル | 足すもの |
|---|---|
| `src/view/track_inspector/native_panel/reverb.rs` / `delay.rs`（新設） | Par 1 枚（§8） |
| `.../native_panel/mod.rs` | `draw_native_panel` の腕 |
| `.../native_panel/layout.rs` | `panel_height` の腕、見出し段の定数 |
| `.../native_panel/cell.rs` | セクション見出し `section_label`（§8.1） |
| `src/view/track_inspector/native_row.rs` | `draw_mini` の腕（**2 種とも小表示なし**） |
| `src/event_native.rs` | `NativeEdit` の 2 種ぶんと `undo_label` |

Mixer 帯（`view/strip_sections.rs`）とマスターパネルは組み込み 4 種だけを match guard で拾うので無変更。

### 1.4 不変条件チェック（CLAUDE.md）

- **1 安定 id**: 住所は `device_id + NativeParamId` のみ。位置参照を新設しない。○
- **2 wire は blob-less**: `NativeParams` は `Copy` の値型のまま。遅延メモリは engine の scratch で、
  Song にも wire にも載らない。○
- **4 RT は無限待ち・確保・解放をしない**: §2 が主題。
- **6 live と export は同じ render 関数**: `run_native` 1 本。○
- **8 daw-ui core はドメイン知識を持たない**: 新 widget は作らず `native_device` の共有部品を使う。○
- **9 サイズ budget**: 新設はいずれも 300 行未満。`native.rs`(320) / `native_param.rs`(204) も
  追加後 1,000 行に届かない。○

## 2. RT 安全性（本件の設計の核）

既存 4 種の状態は固定長で、`NativeDsp` は `Copy`、`ChainProgram::natives` にインラインで並ぶ。
Reverb / Delay は**秒オーダーの遅延メモリ**を要るのでここが初めて崩れる。

### 2.1 確保は compile 時（off-RT）の 1 か所だけ

`NativeScratch` は既に `Vec` を持つ（`ScStage` / `ListenBuf`）。同じ寿命・同じ確保口に載せる。

```
compile_schedule(song, .., sample_rate, ..)            // 既にある
  → build_program(.., sample_rate)                     // ← 引数を足す
      → NativeScratch::new(nd, track_id, sample_rate)  // ← 引数を足す
          → NativeDsp::new(kind, sample_rate)          // ← ここでだけ vec![0.0; cap]
```

容量は**セッションの sample_rate と各種の最大設定**から決める。

- Delay: `cap = ceil(5.0 * sr) + 4`（`MAX_DELAY_SEC = 5.0` は Ableton Live の公表上限）
- Reverb: 全ラインを **Size 最大 2.0** で確保（Size を動かしても再確保しない）+ predelay 250 ms

sample_rate はセッション中不変（変わればデバイスを開き直す = engine ごと作り直し）なので compile 時の
値で確定してよい。防御として `process` は読み出し位置を `cap - GUARD` に clamp する。

**固定サンプル数にしない。** Surge（`1<<18`）は 192 kHz で 1.37 s しか遅らせられず、
Ardour ACE Delay（`MAX_DELAY 768000` 固定 + Time 上限 8000 ms）は 192 kHz で
`p = posz - tap; if (p<0) p += MAX_DELAY;` が**1 回しか巻き戻さず範囲外を読む**（`a-delay.c:476-477`）。

### 2.2 `NativeDsp::reset()` はヒープに触ってはいけない

`run_native` は bypass の crossfade が OFF→ON に変わる瞬間、**RT 上で** `ns.dsp.reset()` を呼ぶ
（`graph/native.rs:229`）。現在の実装は

```rust
pub fn reset(&mut self) { *self = Self::new(self.kind()); }   // ← Reverb/Delay では確保
```

なので、そのままでは**再生スレッドでヒープ確保する**。各 state に `reset(&mut self)` を持たせ、
`NativeDsp::reset` はそれへ委譲する（バッファは `fill(0.0)`、容量は保つ）。既存 4 種の意味は不変。

### 2.3 再 compile を跨ぐ引き継ぎは move

`NativeDsp: Copy` を落とし、`adopt_state_from(&mut self, old: &mut NativeDsp)` で
`std::mem::swap` する（`NativeScratch::adopt_state_from` は既に `old: &mut` を持つ）。
容量が違う場合は swap せず、新しい側をそのまま使う（残響が切れるのは SR 変更時だけ）。
`Copy` を要求している箇所は `native.rs:167` の 1 か所のみ。

### 2.4 BPM は block 単位で渡す

`ProgramCtx::current_bpm` が既にあるので `NativeBlock` に `bpm: f32` を足す。
block 内は一定（パラメータ解決も block-rate なので粒度が揃う）。

### 2.5 denormal

減衰の尾で必須。既存 `common/src/dsp/stereo.rs` の規則（非有限か `|y| <= 1e-25` なら 0）を
そのまま使う（新しい規則を作らない）。

## 3. 音符値は `common` に 1 本化する（SSoT）

「`1/N` → 拍」の式は既に **2 か所**にある:

- `common/src/snap.rs` `SnapMode::{Straight, Dotted, Triplet}`（`4/div`, `6/div`, `(8/3)/div` 拍）
- `common/src/model/session.rs` `LaunchQuantize::Note { div, triplet }`（`4/div`、三連は `×2/3`）

`session.rs` には既に「片方だけ直すとグリッドがズレるので両方を見ること」という**散文の申し送り**が
書かれている。これは SSoT が無いことの症状（CLAUDE.md「機械が持てるものは機械に持たせ、
原文を引用して再掲しない」）。Delay で **3 コピー目を作らない**。

```rust
// common/src/note_value.rs（新設。wire ではない = WIRE_SOURCES に載せない）
pub enum NoteKind { Straight, Dotted, Triplet }   // 係数 1 / 1.5 / (2/3)
pub struct NoteValue { pub div: u32, pub kind: NoteKind }
impl NoteValue {
    pub fn beats(self) -> f64;          // 4.0 / div * 係数
    pub fn label(self) -> String;       // "1/8" / "1/8." / "1/8T"
}
```

`SnapMode` / `LaunchQuantize` の **enum の形（= serde / wire）は変えず**、拍の算出だけ
`NoteValue::beats()` へ委譲する。式が 1 本になり、散文の申し送りは消せる。

## 4. Reverb の値（`common/src/model/native/reverb.rs`）

### 4.1 パラメータ（14、`On` 含む）

`REVERB_ALL` の並び = Par の列順。`exists()` は常に true。

| # | 住所 | knob | 単位 | ParamRange | 既定 |
|---|---|---|---|---|---|
| 1 | `On(Reverb)` | On | — | `Toggle` | ON |
| 2 | `Predelay` | Pre | ms | `LogWithOff{1.0, 250.0}` | 10.0 |
| 3 | `Size` | Size | % | `Log{25.0, 200.0}` | 100.0 |
| 4 | `Decay` | Dec | s | `Log{0.1, 20.0}` | 1.8 |
| 5 | `Damp` | Damp | % | `Linear{0.0, 100.0}` | 20.0 |
| 6 | `LfDamp` | LF | % | `Linear{0.0, 100.0}` | 20.0 |
| 7 | `Diffusion` | Diff | % | `Linear{0.0, 100.0}` | 100.0 |
| 8 | `LowCut` | LoCut | Hz | `LogWithOff{20.0, 1000.0}` | 80.0 |
| 9 | `HighCut` | HiCut | Hz | `Log{1000.0, 20000.0}` | 12000.0 |
| 10 | `ModRate` | Rate | Hz | `Log{0.1, 5.0}` | 1.0 |
| 11 | `ModDepth` | Dep | % | `Linear{0.0, 100.0}` | 50.0 |
| 12 | `Width` | Wid | % | `Linear{0.0, 100.0}` | 100.0 |
| 13 | `Mix` | Mix | % | `Linear{0.0, 100.0}` | 25.0 |
| 14 | `Freeze` | Frz | — | `Toggle` | OFF |

**意図的に入れないもの（根拠つき）**

- **Decay Diffusion 2** — Dattorro Table 1 が `= decay + 0.15`（floor 0.25 / ceiling 0.50）と
  導出式を明示しているので SSoT は `decay` 1 本。独立ノブは SSoT の二重化。
- **Density / Scale / Quality** — Ableton の Density は「品質 vs CPU」のトレードオフであって
  音作りではない。単一品質でよい。
- **Early Reflections** — Dattorro プレートには早期反射の概念がない。Hall algorithm を
  足すときに一緒に入れる（`NativeKind` をもう 1 つ足す形になる）。

## 5. Reverb の DSP（`daw_audio/src/native_dsp/reverb.rs`）

**方式 = Dattorro プレートリバーブ**（Jon Dattorro, "Effect Design, Part 1", *J. Audio Eng. Soc.*
45(9), 1997, pp.660-684。Fig.1 / Table 1 / Table 2）。

選定理由: **係数が査読論文に全部載っている唯一の方式**。Freeverb の定数は「listening tests で
決めた」（`tuning.h:31`）、Surge Reverb1 の 64 個の delay_time は乱数生成の残骸
（`Reverb1.h:430-434`）、zita-rev1 は設計式のみで定数の根拠は非公開。「推測で書かない」を
満たせるのはこれだけ。加えて全遅延長が固定（Size 倍率で決まる）ので compile 時に 1 回確保して
以後は index 演算だけで済み、行列積も可変長リングも要らない。

Freeverb を却下した理由: comb ごとに T60 が違う（roomsize=0.5 で L=1116 の comb は T60=1.00 s、
L=1617 は 1.45 s）ため、「Decay Time = 秒」というノブを正直に作れない。

### 5.1 トポロジ（Fig.1）

```
in = 0.5*(L + R)
  → predelay（可変長リング）
  → LowCut HPF → bandwidth LPF(= HighCut)
  → APF(142,+id1) → APF(107,+id1) → APF(379,+id2) → APF(277,+id2)   = d

tank（図八）:
  left_in  = d + right_out
    → APF_mod(672 + excursion, -dd1) → Delay(4453) → damping → *decay
    → APF(1800, +dd2)                → Delay(3720) → *decay = left_out
  right_in = d + left_out
    → APF_mod(908 + excursion, -dd1) → Delay(4217) → damping → *decay
    → APF(2656, +dd2)                → Delay(3163) → *decay = right_out
```

APF は 2-multiplier lattice:

```rust
let vd = buf[k];          // v[n-L]
let v  = x - c * vd;
let y  = vd + c * v;
buf[k] = v;  k += 1; if k >= len { k = 0; }
```

**`decay diffusion 1` の 2 本は係数を負で使う**（Fig.1 の "note sign"、§1.3.3）。

### 5.2 遅延長（29761 Hz 基準）

| 名前 | @29761 | @48000 |
|---|---|---|
| in_apf1 | 142 | 229 |
| in_apf2 | 107 | 173 |
| in_apf3 | 379 | 611 |
| in_apf4 | 277 | 447 |
| L: apf_mod | 672 | 1084 |
| L: delay1 | 4453 | 7182 |
| L: apf2 | 1800 | 2903 |
| L: delay2 | 3720 | 6000 |
| R: apf_mod | 908 | 1464 |
| R: delay1 | 4217 | 6801 |
| R: apf2 | 2656 | 4284 |
| R: delay2 | 3163 | 5101 |

実長 = `round(base * sample_rate / 29761.0 * size)`。確保は `size = 2.0` の長さで行い、
Size を動かしたら**長さの index だけ**変える（再確保しない）。

### 5.3 出力タップ（Table 2、p.665 原文）

節点の対応: `node24_30 = L:delay1` / `node31_33 = L:apf2` / `node33_39 = L:delay2` /
`node48_54 = R:delay1` / `node55_59 = R:apf2` / `node59_63 = R:delay2`。

```
yL = 0.6*( +node48_54[266] +node48_54[2974] -node55_59[1913] +node59_63[1996]
           -node24_30[1990] -node31_33[187]  -node33_39[1066] )
yR = 0.6*( +node24_30[353] +node24_30[3627] -node31_33[1228] +node33_39[2673]
           -node48_54[2111] -node55_59[335]  -node59_63[121]  )
```

**左出力が主に右枝を読み、右出力が主に左枝を読む** — これが mono 入力から stereo 像を作る仕掛け
（§1.3.6）。タップ位置も同じ倍率でスケールし、対応するライン長未満に clamp する。

### 5.4 係数（Table 1、p.663）

| 記号 | 既定 | 本件での扱い |
|---|---|---|
| decay | 0.50 | `Decay`（秒）から導出（§5.5） |
| decay diffusion 1 | 0.70 | 固定（APF は負号） |
| decay diffusion 2 | 0.50 | `= (decay + 0.15).clamp(0.25, 0.50)` |
| input diffusion 1 | 0.750 | `× Diffusion/100` |
| input diffusion 2 | 0.625 | `× Diffusion/100` |
| bandwidth | 0.9995 | `HighCut` から 1 極係数として導出 |
| damping | 0.0005 | `Damp`（%）から導出 |
| 出力タップ係数 | 0.6 | 固定 |
| EXCURSION | 16 (peak ±8) | `ModDepth`（%）で 0〜±8 sample @29761 相当。rate は `ModRate` |

### 5.5 Decay Time（秒）→ `decay`

タンク一周 = `672+4453+1800+3720 + 908+4217+2656+3163 = 21589` samples @29761
= **0.725412 s**、その間に `×decay` が **4 回**。

```rust
decay = 0.001f32.powf(loop_secs / (4.0 * t60_secs));   // loop_secs は Size 倍率込み
```

検算: `decay = 0.5` → T60 = 1.807 s。この式は Surge Reverb2（`Reverb2.h:411-412`）と
zita-rev1（`zita_reverb.cc:143` `powf(0.001f, del/tmf)`）に独立に一致する。

`Freeze` = 入力を遮断し `decay = 1.0` / damping off（Freeverb `revmodel.cpp:153-158` と同じ）。

### 5.6 変調（必須）

論文 §1.3.7: 「For signals with much high-frequency content, such as drum sets, these built-in
modulators serve to break up some pretty audible modes」。対象は**タンク前段の 2 本の変調 APF
（672 / 908）のみ**（論文も計算時間が制約ならこの 2 本を優先せよと書いている）。
L / R は quadrature（sin / cos）で相関を落とす。補間は論文推奨のオールパス補間ではなく
**Catmull-Rom 4 点 3 次**（Delay と同じ関数を使う = 実装を 2 種類にしない。線形が持ち込む
時変ローパスを避けるという論文の目的は満たす）。

### 5.7 ステレオ

**mono-in / stereo-out**。入力は `0.5*(L+R)`（論文 p.665「the stereo input is converted to a
monophonic signal at the reverberator input for this particular topology」）。
Freeverb / Surge Reverb1 / Reverb2 も同じ。`Width` は出力段の M/S（0% = mono、100% = そのまま）。

## 6. Delay の値（`common/src/model/native/delay.rs`）

### 6.1 パラメータ（21、`On` 含む）

| # | 住所 | knob | 単位 | ParamRange | 既定 |
|---|---|---|---|---|---|
| 1 | `On(Delay)` | On | — | `Toggle` | ON |
| 2 | `Sync` | Sync | — | `Toggle` | **ON** |
| 3 | `DivL` | DivL | 音符値 | `Stepped{18}` | **7 = 1/8** |
| 4 | `DivR` | DivR | 音符値 | `Stepped{18}` | 7 = 1/8 |
| 5 | `TimeL` | TimeL | ms | `Log{1.0, 5000.0}` | 350.0 |
| 6 | `TimeR` | TimeR | ms | `Log{1.0, 5000.0}` | 350.0 |
| 7 | `OffsetL` | OfsL | % | `Linear{-33.0, 33.0}` | 0.0 |
| 8 | `OffsetR` | OfsR | % | `Linear{-33.0, 33.0}` | 0.0 |
| 9 | `Link` | Link | — | `Toggle` | **ON** |
| 10 | `Feedback` | FB | % | `Linear{0.0, 100.0}` | 35.0 |
| 11 | `Cross` | Cross | % | `Linear{0.0, 100.0}` | 0.0 |
| 12 | `Pattern` | Pat | — | `Stepped{4}` | **1 = Stereo** |
| 13 | `Mode` | Mode | — | `Stepped{3}` | **0 = Repitch** |
| 14 | `Hp` | HP | Hz | `LogWithOff{20.0, 2000.0}` | OFF (0.0) |
| 15 | `Lp` | LP | Hz | `Log{200.0, 20000.0}` | 6000.0 |
| 16 | `Drive` | Drv | — | `Stepped{4}` | **1 = Soft** |
| 17 | `ModRate` | Rate | Hz | `Log{0.01, 20.0}` | 0.5 |
| 18 | `ModDepth` | Dep | % | `Linear{0.0, 100.0}` | 0.0 |
| 19 | `Width` | Wid | % | `Linear{0.0, 200.0}` | 100.0 |
| 20 | `Freeze` | Frz | — | `Toggle` | OFF |
| 21 | `Mix` | Mix | % | `Linear{0.0, 100.0}` | 50.0 |

- `Pattern` = `Mono / Stereo / Ping L / Ping R`（Bitwig の分節。Live の Ping Pong トグルを包含）
- `Mode` = `Repitch / Fade / Jump`（Ableton の 3 モード。定義は §7.2）
- `Drive` = `Off / Soft / Tanh / Hard`（Surge の clipping mode、`sst/effects/Delay.h:65-74`）

`Feedback` の表示値は **1 回の繰り返しの実 gain**。Surge の `x³` 写像（`Delay.h:183-188`）は
数値が嘘になるので採らない。

**意図的に入れないもの**

- **device 内蔵の汎用 LFO 波形セット** — daw_01 は統合変調基盤を持つので波形を device に抱えると
  SSoT が割れる。#17/#18 は「テープ揺れ」専用の三角波 1 基に限定する。外部変調は block-rate
  （`MAX_FRAMES = 1024` = 21 ms @48k）なので vibrato 速度の揺れは作れない — だから内蔵する。
- **マルチタップ** — `Pattern` + L/R 独立 + 既存の `Parallel` で表現できる。device 内に tap 配列を
  作ると並べ替えが positional index になり**不変条件 1** と衝突する。

### 6.2 音符値の 18 段（`Stepped{18}`、拍長の昇順）

つまみを右に回すほど必ず長くなる順に並べる。`DelayDiv::ALL` / `LABELS`。

| idx | 表示 | beats | idx | 表示 | beats |
|---|---|---|---|---|---|
| 0 | 1/32T | 0.0833 | 9 | 1/8. | 0.75 |
| 1 | 1/32 | 0.125 | **10** | **1/4** | **1.0** |
| 2 | 1/16T | 0.1667 | 11 | 1/2T | 1.3333 |
| 3 | 1/32. | 0.1875 | 12 | 1/4. | 1.5 |
| 4 | 1/16 | 0.25 | 13 | 1/2 | 2.0 |
| 5 | 1/8T | 0.3333 | 14 | 1/1T | 2.6667 |
| 6 | 1/16. | 0.375 | 15 | 1/2. | 3.0 |
| **7** | **1/8** | **0.5** | 16 | 1/1 | 4.0 |
| 8 | 1/4T | 0.6667 | 17 | 1/1. | 6.0 |

拍は `NoteValue::beats()`（§3）から引く。Delay 独自の換算表は持たない。

## 7. Delay の DSP（`daw_audio/src/native_dsp/delay.rs`）

### 7.1 信号フロー（Pattern = Stereo）

```
in_L ──┬──────────────────────────────── ×(1-mix) ─────────────┐
       │                                                       │
       │  write_L = drive( in_L + fb·wet_L + cross·wet_R )      │
       ▼                                                       ▼
 ring_L[cap] ──read(Catmull-Rom)──▶ HP → LP ──▶ wet_L ──┬── ×width ── ×mix ── ⊕ ── out_L
                                                        └──▶ フィードバックへ
```

- **フィルタは read の直後に 1 組だけ**置き、その出力を出力段とフィードバックの両方へ配る
  （Surge `Delay.h:415-428`）。Ardour ACE Delay のように出力段だけに置くと**繰り返しが暗く
  ならない** — `a-delay.c:492` はフィルタ前の `fbstate` を書き戻している。
- `drive()` は書き込み直前に 1 回。**FB 100% で発散しない唯一の保証**
  （Vital は `hardTanh(x/8)*8` を常時通す、`delay.cpp:14-17`）。
- `Freeze` = 入力 0 / fb 1.0 / **Drive をバイパス**（tanh が効くと freeze 中に音が痩せる）。

**Ping L**（Vital `kPingPong` の定義）:

```
mono = (in_L + in_R) / √2  →  L の ring にだけ入れる
write_L = drive( mono + fb · wet_R )      ← 復路にだけ Feedback
write_R = drive(        1.0 · wet_L )      ← 往路は unity
```

**往路を unity にするのが肝**。両方に fb を掛けると 1 往復で 2 回掛かり、「Feedback 50%」が
実際には 25% になる（Vital は `maskLoad(current_feedback, 1.0f, kRightMask)` で R 側を 1.0 に固定、
`delay.cpp:84-88`）。**Ping R は L/R を入れ替えるだけ。**

### 7.2 遅延長の決定と BPM 追従

```
Sync ON :  beats   = DelayDiv::note_value(div).beats()
           samples = beats × 60 × sample_rate × (1 + offset/100) / bpm
Sync OFF:  samples = time_ms × sample_rate / 1000
最終    :  clamp(samples, 4.0, cap - 4)
```

`Link` ON なら R は L の値を使う（Div / Time / Offset の 3 つとも）。

BPM は 1.0 まで下がれる（`common/src/model/load_normalize.rs:22`）ので tempo sync は必ず
溢れうる。clamp は必須で、かつ **Par に実効 ms を表示**して clamp が見えるようにする。

| Mode | Ableton マニュアル原文 | 実装 |
|---|---|---|
| Repitch | "produces a pitch variation when the delay time is changed, which is similar to the behavior of old tape delay units" | 目標へ 1 極ローパスで寄せる（half-life 20 ms、Vital `kDelayHalfLife = 0.02f`） |
| Fade | "creates a crossfade between the old and new delay times" | 読みヘッド 2 本、**固定 20 ms** で等電力 crossfade |
| Jump | "immediately switches to the new delay time. This can produce audible clicks" | 即代入 |

**Fade の crossfade 長は固定 ms にする。** Ardour は `xfade += 1.0f/n_samples`（`a-delay.c:481`）
なのでホストの buffer size で音が変わる。

### 7.3 補間は Catmull-Rom 4 点 3 次

JUCE の公式 doc（`juce_DelayLine.h:43-84`）が挙げる 4 方式のうち、Linear は「低域が減衰する」
副作用がフィードバックループで累積し、Thiran は "stateful so is unsuitable for applications
requiring fast delay modulation" で Repitch と両立しない。Lagrange3rd（= 4 点 3 次）が
「変調に耐え、かつ低域減衰が小さい」唯一の選択。実装は Vital `lookups/memory.h:122-131` と同形。

Reverb の変調 APF も同じ関数を使う（§5.6）。

## 8. GUI

### 8.1 Par パネル = 切り替え帯 + 見出し付き格子

既存 Comp の Par（`[LEV|CMP|LIM]` + GR → 見出し → 6 セル）の素直な拡張。
最上段に切り替えボタンの帯、その下を小見出しで段に区切って 6 列の格子。

```
┌─ Delay Par ────────────────────────────┐
│ [Sync][Link][Frz]  Pat[Stereo] Mode[Rep]│
│                    Drv[Soft]            │
│ ── TIME ───────────  実効 348 / 348 ms  │
│  DivL  TimeL  OfsL   DivR  TimeR  OfsR  │
│ ── FEEDBACK / TONE ──────────────────── │
│   FB   Cross   HP     LP                │
│ ── MOD / OUT ────────────────────────── │
│  Rate   Dep   Wid    Mix                │
└─────────────────────────────────────────┘

┌─ Reverb Par ───────────────────────────┐
│ [Frz]                                   │
│ ── ROOM ─────────────────────────────── │
│  Pre   Size   Dec   Damp   LF    Diff   │
│ ── TONE / MOD / OUT ─────────────────── │
│ LoCut HiCut  Rate   Dep    Wid   Mix    │
└─────────────────────────────────────────┘
```

- 小見出し（`section_label`）は `cell.rs` に足す。`layout.rs` に `SECTION_H` を定数で置き、
  描画と `panel_height` が**同じ定数**を読む（Par の高さを実測しない = 開いた最初のフレームから
  行が正しい高さで並ぶ、既存規約）。
- Delay の「実効 ms」は TIME 見出しの右端に右寄せで出す（clamp が見える）。live 値 + 現在 BPM から求める。
- つまみ・数値欄・切り替えは既存部品（`param_cell` / `switch` / `head_labels`）をそのまま使う。
  bespoke widget を新設しない。

### 8.2 行の小表示はなし

`draw_mini` は Reverb / Delay で何も描かない（名前と ON/OFF だけ）。

### 8.3 `NativeEdit`

`Sync` / `Link` / `Freeze` / `Pattern` / `Mode` / `Drive` は **`ParamRange` を持つ住所**
（`Toggle` / `Stepped`）なので、`NativeEdit::Params` の一般経路でそのまま編集できる
（CompMode のような専用 variant は要らない）。自動 ON の規則（`NativeEdit::apply`）もそのまま効く。

## 9. テスト

- `NoteValue::beats()` と `SnapMode` / `LaunchQuantize` の一致（既存の期待値が変わらないこと）
- Dattorro の `decay` 導出: `decay = 0.5` → T60 ≈ 1.807 s（論文からの検算）
- tempo sync の換算: BPM 120 / 1/8 → 250 ms、BPM 1 / 1/1. → cap に clamp されること
- Ping L の 1 往復ゲイン = `Feedback` のノブ値そのもの（往路 unity の回帰）
- `NativeDsp::reset()` が容量を保つこと（= RT で再確保しない）
- bypass → ON → bypass の往復で `adopt_state_from` が残響を保つこと

## 10. 実装で確定した定数 (論文・参照実装に無いもの)

一次情報に数値が無く、実装時に決めた定数はここに集める (コード側の doc comment が正本、
ここは索引)。

| 定数 | 値 | 置き場 | 根拠 |
|---|---|---|---|
| `MAX_DAMPING` | 0.95 | `native_dsp/reverb.rs` | 論文の `damping` は `[0,1)` の 1 極係数。1.0 で極が閉じて音が止まるので手前で止める。ノブ 0〜100% がそのまま論文のパラメータに写る |
| `LF_DAMP_XOVER_HZ` | 200 Hz | `native_dsp/reverb.rs` | **論文には低域ダンピングが無い**。zita-rev1 / Surge Reverb2 が持つ帯域別減衰の最小形として、この周波数より下だけを `LfDamp` の割合で減らす |
| `MAX_MOD_MS` | 3.0 ms | `native_dsp/delay.rs` | `ModDepth` 100% の片側振れ幅。コーラス〜フランジャ相当 |
| `REPITCH_HALF_LIFE_S` | 0.02 s | `native_dsp/delay.rs` | Vital `kDelayHalfLife = 0.02f` と同値 |
| `FADE_SECS` | 0.02 s | `native_dsp/delay.rs` | Ardour ACE Delay の buffer size 依存 xfade を固定 ms に直したもの |
| Reverb のレーン色 | 藤 | `widgets/arrangement/view_build.rs` | 既存 (Comp 系 = 橙 / EQ 系 = 青緑) と重ならない色 |
| Delay のレーン色 | 黄緑 | 同上 | 同上 |

## 11. GUI の切り替え帯の作り (§8.1 の実装形)

`native_panel/cell.rs` に 3 つ足した。いずれも**新しい widget は作らず** `ui.toggle_button_at` /
`ui.label_at` / `ui.panel` の組み合わせ。

- `section_label(.., right)` — 小見出し + 右寄せの補助表示 + 罫線。Delay の「実効 ms」がこの `right`。
- `param_switch(..)` — `ParamRange::Toggle` の住所を反転するボタン (`Sync` / `Link` / `Frz`)。
- `selector(..)` — `ParamRange::Stepped` の住所を次の段へ送るボタン (`Pat` / `Mode` / `Drv`)。
  表示は `step_labels()` の段ラベル。

3 つとも `DeviceEvent::NativeEdit { edit: NativeEdit::param(..) }` を出すだけなので、
自動 ON (Q15)・値 IPC・last touched・undo は既存の 1 本の経路に乗る (専用 `NativeEdit` variant は要らない)。
