# チャンネルストリップ (内蔵 EQ + コンプ) UI 設計

> **r.md #129 で一部を置き換えた。** 組み込み Comp / EQ はチェーン上の device (`Device::Native`) になり、
> 並べ替え・追加・オートメーション住所・テレメトリは [docs/plan_rack_native_devices.md](plan_rack_native_devices.md)
> が正本。本書に残るのは Mixer 帯の見せ方 (寸法 / 開閉 / サムネイル / パラメータの範囲) だけ。
> 置き換えた決定は各節の冒頭に 1 行で示す。

Harrison Mixbus / Reason の SSL ミキサーに倣い、**全チャンネルに最初から在って消せない
EQ とコンプ**をミキサーに持たせる。折り畳んだ状態でも EQ カーブとゲインリダクションが
見え、必要なときだけ全 ch 一括でノブを開く。

参考にした一次情報:

- [Mixbus 10 — EQ Section](https://rsrc.harrisonconsoles.com/mixbus/mixbus-live-manual/10/en/topic/eqs)
  — 4 band (外側 2 = シェルビング / 内側 2 = proportional-Q) + HP/LP フィルタ、
  Q ノブは中域のみ、シェルフ/ベル切替は小スイッチ。
- [Mixbus 10 — Channel Compressor/Limiter](https://rsrc.harrisonconsoles.com/mixbus/mixbus-live-manual/10/en/topic/channel-compressor-limiter-section)
  — モード 3 択 (Leveler / Compressor / Limiter)、Threshold / Ratio / Attack / Release /
  Emph / Gain、GR メーターは 1 LED = 2dB。
- [Reason 13 — The Main Mixer](https://docs.reasonstudios.com/reason13/the-main-mixer)
  — セクション (Input / Dynamics / EQ / Inserts / Fader) は
  **Channel Strip Navigator から全 ch 一括で表示/隠す**。隠しても処理は生きている。
- [Reason 13 — Channel Dynamics](https://docs.reasonstudios.com/reason13/channel-dynamics-compressor-amp-gate)
  — GR は LED メーター。`FILTERS TO DYN S/C` で HP/LP を検出信号へ回す。

## 1. 信号経路

**廃止 (r.md #129)**: 「固定順で並べ替えできない」「挿したプラグインは必ずコンプより前」。組み込み Comp / EQ は
チェーン上の device で D&D で並べ替えられ (Q4)、新しい device の既定の挿入位置は Q6 — [plan_rack_native_devices.md](plan_rack_native_devices.md) §2。
既存曲を開いたときの並びは今と同じ音になる `devices → Comp → EQ`。

**廃止 (r.md #129)**: 実行場所 `mixer.rs` の `apply_strip` 直前 / 状態は `TrackScratch`。チェーン上の op
`ChainOp::Native`、状態は device id で引き継ぐ `ChainProgram.natives` — [plan_rack_native_devices.md](plan_rack_native_devices.md) §8。

## 2. Mixer strip の構成

**既存の strip (名前 / M・S / Pan / Fader+Meter / Sends) は一切変えない。上に足すだけ。**

**変更 (r.md #129)**: セクションの縦の並びは Rack での組み込み Comp / EQ の前後に合わせて入れ替わる。帯に出るのは
組み込みだけで、追加分 (「Comp 2」) は Rack にだけ出る (Q16) — [plan_rack_native_devices.md](plan_rack_native_devices.md) §10.8。
下の図は既定 (Comp → EQ) の並び。

```
+----------+   Comp セクション (開いているときだけ)    132px
|[LEV|CMP|LIM]|
| Thr  Rat |
| Atk  Rel |
| SC  Gain▶|
| GR ##### |
+----------+   EQ セクション (開いているときだけ)      164px
| HP LP  o |
| F  G [S] |
| F  G  Q  |
| F  G  Q  |
| F  G [S] |
+----------+   常設サムネイル帯 (常に見える)            28px
|CP|#| EQ ~~\_|
+----------+   ここから下は既存 strip のまま
| Name     |
| M   S    |
| Pan      |
| [fader]  |
| Sends    |
+----------+
```

寸法は既存の `STRIP_WIDTH = 80` / `STRIP_PAD = 6` を据え置き、内側 68px に収める。
ノブは 20px × 3 個 + gap 4px × 2 = 68px でちょうど 1 行。1 行は
`ラベル行 12 (font 10px) + ノブ 20 + 隙間 2 = 34px`。**寸法の SSoT は
`daw_gui/src/view/strip_sections.rs` の定数**で、図の px は概数
(実測は定数から導く。ラベルは 8px では読めないという指摘で 10px)。

80px 幅にノブ 3 個ぶんの数値欄は入らないので、**各行の見出し行が hover 読み出しを
兼ねる** — 何も触っていなければ行の名前 (`HMF` / `Thr Rat`)、ノブに触れている間は
その 1 個の値 (`Freq 2500 Hz`) を出す。

## 3. 常設サムネイル帯 (28px)

折り畳んでいる間もここだけは全 ch に必ず出る。**この帯が本設計の中心**。

帯の中は左端に Comp の GR、残りが EQ カーブ (セクションの並びが入れ替わっても固定)。**バイパス専用のボタンは置かない** —
80px の strip でボタンに幅を割くより、面そのものを広く取って状態は色で読ませる。

```
 |#|   ~~\_.--~~
  8  3      57        (px)
```

- **GR バー** (8px 幅) — 左端に縦、上から下へ伸びる。レンジ 0〜-20dB。**並びが入れ替わっても左端に固定**
  (全 ch で GR の位置が揃う)。
- **EQ カーブ** (帯の全幅) — HP/LP を含む合成レスポンスを 1 本の線で描く。
  横軸 20Hz〜20kHz 対数、縦軸 ±18dB。**スペクトラム重畳はしない**
  (68px 幅では 1 オクターブ 6.8px にしかならず読めないため。Rack の Par は重ねる、Q14)。
  OFF のカーブは形を保って薄く描く (plan_rack_native_devices.md §20-8)。
- **シングルクリック** = そのセクションの開閉 (全 ch 一括)。
  GR バーなら Comp、カーブなら EQ。
- **訂正 (r.md #129)**: 「ダブルクリック = バイパス」は実装されていない (1 回目の press で開閉が先に見えるため)。
  ON/OFF は `Q` (カーソル直下) と Rack の小表示ダブルクリック / 右クリック (Q15) — [plan_rack_native_devices.md](plan_rack_native_devices.md) §10.12。
- **一般化 (r.md #129)**: 「触ったら自動で ON」は 4 種 (Comp / EQ / Bus Comp / Tone EQ) 共通で、SSoT は
  `NativeEdit::apply` — [plan_rack_native_devices.md](plan_rack_native_devices.md) §10.1。

## 4. 開閉の規則

- **全 ch 一括** (Reason の Channel Strip Navigator と同じ)。個別 ch だけ開くことはできない。
  ミキサーは全 ch のフェーダー位置が横一線に揃っていることが読み取りの前提なので、
  strip ごとに高さが変わる形は採らない。
- 隠しても **処理は生きている** (Reason と同じ)。
- 開くと strip の総高が増える → **下ペインを開いた帯の高さぶんだけ広げる**。
  strip 全体が同じ量だけ伸びるので、**フェーダー / メーター / Sends の高さは
  開閉で 1px も動かない**。実装は「保存された分割比
  (`ui_prefs.arrangement_split_ratio`) は書き換えず、描画時に差し引くだけ」
  (`view/root.rs`) — 閉じた瞬間にユーザーの比率へ自動で戻り、復元用の状態を
  別に持たずに済む。アレンジ側の下限 (15%) に届いたらそこで頭打ちになり、
  以降はフェーダーが縮む。
- 既定は EQ・Comp とも **折り畳み**。

## 5. パラメータ

### 5.1 Comp (5 行)

| 行 | 内容 | レンジ |
|----|------|--------|
| Mode | `LEV` / `CMP` / `LIM` の 3 択 | Leveler = 低レシオ (2:1) 速リリース固定 / Compressor = 全可変 / Limiter = attack 0.1ms・ratio 20:1 以上 |
| — | Threshold / Ratio | -60–0dB / 1:1–20:1 |
| — | Attack / Release | 0.1–100ms / 10–2000ms |
| — | SC Freq / Gain (makeup) + `SC Listen` | §5.3 / 0–+20dB |
| — | GR メーター (横バー + 数値) | 0〜-20dB。数値は **符号なし小数第 1 位** (減衰は常に負方向なので `-` は書かない) |

### 5.2 EQ (5 行)

| 行 | 内容 | レンジ |
|----|------|--------|
| Filters | HP Freq / LP Freq + 各 ON | HP 20–3100Hz, LP 160Hz–20kHz, ともに 12dB/oct |
| HF | Freq / Gain / `BELL` 切替 | 1.5k–20kHz, ±15dB, 既定シェルビング |
| HMF | Freq / Gain / Q | 400Hz–8kHz, ±15dB, Q 0.3–3.0 |
| LMF | Freq / Gain / Q | 60Hz–2kHz, ±15dB, Q 0.3–3.0 |
| LF | Freq / Gain / `BELL` 切替 | 20–600Hz, ±15dB, 既定シェルビング |

Mixer 帯のカーブは表示専用 (ノードのドラッグは持たない)。**廃止 (r.md #129、Rack の Par について)**: Rack の EQ Par は
カーブ上の点をドラッグで操作する (Q13) — [plan_rack_native_devices.md](plan_rack_native_devices.md) §10.7。

### 5.3 検出フィルタ (SC Freq)

Mixbus の `Emph` (高域だけ強調) を置き換えて、**検出信号をバンドパスで絞るノブ 1 個**にする。
高域だけでなく低域も狙えるようにするため。

- **左端まで回すと `OFF`** = 検出フルレンジ (既定)。BPF はどこに置いてもフラットにならないので、
  素の全帯域検出はこの位置でのみ得られる。
- Q は周波数から自動で決まる (proportional-Q): `Q(f) = 0.3 × (f / 20)^0.3444`

  | Freq | Q | 帯域幅 |
  |------|---|--------|
  | 20Hz | 0.30 | 約 4.7 oct |
  | 200Hz | 0.66 | 約 2.2 oct |
  | 2kHz | 1.46 | 約 1.0 oct |
  | 16kHz | 3.00 | 約 0.5 oct |

- 低域側に置くと「低域を外す」ではなく「**低域だけを聴いてコンプが動く**」になる
  (BPF なので上が落ちる)。低域を検出から外したいときは 1〜2kHz の緩い山にする。
  代わりに「キックにだけ反応させる」使い方が手に入る。
- `SC Listen` — 検出信号そのものをモニタに出すトグル。狙った帯域に合っているかを
  耳で確認できないと Freq は詰められないので必須。

## 6. 対象

**通常 track / group / return** の組み込みは Comp + EQ。

**master の組み込みは別物** (Bus Comp + Tone EQ + フェーダー後 Limiter)。Reason も Mixbus もマスターバスには他 ch と
別物の (バス専用の) コンプ / EQ を置いている — [docs/plan_master_strip.md](plan_master_strip.md)。
r.md #129 以降、**追加分は 4 種ともどのトラックにも足せる** (Q7)。

## 7. オートメーションと変調

全パラメータが対象。**変更 (r.md #129)**: 住所は `TrackBuiltin(Strip*)` ではなく device id で束縛する
`AutomationTarget::NativeParam { device_id, param: NativeParamId }` — [plan_rack_native_devices.md](plan_rack_native_devices.md) §5.3。
ノブの右クリックで「オートメーション」、◉ アームで変調ルート、という既存の作法がそのまま効く。

## 8. データモデルと永続

- **廃止 (r.md #129)**: `Track.strip: ChannelStrip`。値は `Device::Native` の `params`、ON/OFF の SSoT は `bypassed`
  — [plan_rack_native_devices.md](plan_rack_native_devices.md) §5。値の変更は `edit_song()` チョークポイントを通す。
- 値は `Song` に保存し `*` (dirty) を立てる = 「作った中身が変わる」側。
- **訂正 (r.md #129)**: 帯の開閉と一括トグルは `UiPrefs` ではなく `ProjectView::strip_comp_open` / `strip_eq_open`
  (dirty を立てない「見方の都合」側)。
- 新規トラックの既定は EQ・Comp とも **バイパス**、値はフラット / 無圧縮。

## 9. テレメトリ

- **廃止 (r.md #129)**: per-track の GR スロットと「スペクトラムは送らない」。GR は device id キーの面、Rack の EQ Par の
  スペクトラムは device scope — [plan_rack_native_devices.md](plan_rack_native_devices.md) §11。
- **EQ カーブ** — GUI 側でパラメータから係数を起こして描く (daw_audio と同じ `common::dsp`)。

## 10. 非対象 (意図的に持たないもの)

- 大きい EQ カーブ編集窓 (Reason の Spectrum EQ 相当)
- Mixer 帯のカーブ上のノードのドラッグ編集 (Rack の Par は持つ)
- サムネイルへのスペクトラム重畳 (Rack の Par は重ねる)
- ゲート / エキスパンダー
- プリセット、ch 間の Copy / Paste / Reset
