# チャンネルストリップ (内蔵 EQ + コンプ) UI 設計

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

```
clips / instrument → devices (inserts) → Comp → EQ → Pan → Fader → 親 (group / master)
```

**固定順で、並べ替えはできない。** Comp が先なのは、検出フィルタ (§5.3) が Comp 自身に
あって EQ に検出を頼らないため。EQ が後段なので「EQ でブーストした帯域がコンプに押し戻
される」ことが無く、音作りがそのまま残る。

帰結として **挿したプラグインは必ずコンプより前**になり、コンプの後に処理を置く手段は
send / bus しかない (Mixbus が並べ替えを許しているのはここ)。承知のうえでの固定。

DSP の実行場所は daw_audio の `mixer.rs`、`apply_strip` の直前。プラグインチェーン
(plugin_host への dispatch) が終わった後、volume/pan/mute の前に挿す。RT 規約どおり
係数は事前計算、per-track のフィルタ状態は `TrackScratch` に持つ (確保・ロック・I/O なし)。

## 2. Mixer strip の構成

**既存の strip (名前 / M・S / Pan / Fader+Meter / Sends) は一切変えない。上に足すだけ。**

**セクションの縦の並びは信号順** — 上から Comp → EQ → (既存 strip の Pan / Fader)。
上から下へ読むと実際の経路と一致する。

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
`ラベル行 10 + ノブ 20 + 隙間 2 = 32px`。**寸法の SSoT は
`daw_gui/src/view/strip_sections.rs` の定数**で、上の px はその実測値。

80px 幅にノブ 3 個ぶんの数値欄は入らないので、**各行の見出し行が hover 読み出しを
兼ねる** — 何も触っていなければ行の名前 (`HMF` / `Thr Rat`)、ノブに触れている間は
その 1 個の値 (`Freq 2500Hz`) を出す。

## 3. 常設サムネイル帯 (28px)

折り畳んでいる間もここだけは全 ch に必ず出る。**この帯が本設計の中心**。

帯の中も左から信号順に `Comp → EQ` で並べる。**バイパス専用のボタンは置かない** —
80px の strip でボタンに幅を割くより、面そのものを広く取って状態は色で読ませる。

```
 |#|   ~~\_.--~~
  8  3      57        (px)
```

- **GR バー** (8px 幅) — 左端に縦、上から下へ伸びる。レンジ 0〜-20dB。
- **EQ カーブ** (残り幅 ≒57px) — HP/LP を含む合成レスポンスを 1 本の線で描く。
  横軸 20Hz〜20kHz 対数、縦軸 ±18dB。**スペクトラム重畳はしない**
  (68px 幅では 1 オクターブ 6.8px にしかならず読めないため)。
- **シングルクリック** = そのセクションの開閉 (全 ch 一括)。
  GR バーなら Comp、カーブなら EQ。
- **ダブルクリック** = そのセクションのバイパス ON/OFF。**折り畳んだままでも切れる**。
  OFF のときはカーブと GR をグレーに落とす (= 状態表示はこの色だけ)。
  ダブルクリックの 1 回目の press でシングル (開閉) が先に発火しているので、
  実装側でその開閉を打ち消す — ダブルクリックは「バイパスだけが変わる」操作になる。
- **セクションの中身を触ったら、そのセクションは自動で ON になる**。バイパス中の
  ノブを回して何も起きないのは、操作の取りこぼしにしか見えないため。
  ダブルクリックによる明示的な ON/OFF はこの自動 ON の対象外。

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

カーブは表示専用。**カーブ上のノードをドラッグする編集は持たない**し、
Reason の Spectrum EQ に相当する大きい編集窓も**持たない**。編集はノブだけ。

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

**通常 track / group / return** に付く。

**master は対象外**。Reason も Mixbus もマスターバスには他 ch と別物の
(バス専用の) コンプ / EQ を置いており、同じチャンネルストリップを流用していない。
daw_01 も将来そちらへ進むので、ここでは master に何も足さない
(`view/master_panel.rs` は無改変)。

## 7. オートメーションと変調

全パラメータが対象。`AutomationTarget::TrackBuiltin` に足す:

```rust
// common/src/model/automation.rs
pub enum TrackBuiltinParam {
    Volume, Pan, Mute, SendGain { .. },
    StripEqOn, StripCompOn,
    StripEq  { band: EqBand,  param: EqParam },   // band/param は固定 enum
    StripComp{ param: CompParam },
}
```

band は `Hp / Lp / Lf / Lmf / Hmf / Hf` の固定 enum で、positional index を使わない
(不変条件 1)。ノブの右クリックで「オートメーション」、◉ アームで変調ルート、という
既存の作法がそのまま効く。

## 8. データモデルと永続

- `Track` に `strip: ChannelStrip` を追加 (`#[serde(default)]`、bincode `Encode/Decode`)。
  group / return も実 track なので同じ型を持つ (master は `Song` 側なので持たない)。
  値の変更は `edit_song()` チョークポイントを通す
  (undo / dirty / 子プロセス sync は既存の口が担う)。
- 値は `Song` に保存し `*` (dirty) を立てる = 「作った中身が変わる」側。
- **帯の開閉と一括トグルは `UiPrefs` に持ち、dirty を立てない** = 「見方の都合」側
  (`collapsed_groups` と同じ扱い、session-only)。
- 新規トラックの既定は EQ・Comp とも **バイパス**、値はフラット / 無圧縮。

## 9. テレメトリ

- **GR** — daw_audio が per-track の GR (dB) を `AudioBridge` に書く。
  `MAX_TRACKS = 32` の既存スロットに f32 を 1 本足すだけ (128B)。
- **EQ カーブ** — GUI 側でパラメータから係数を起こして描く。テレメトリ不要。
- スペクトラムは送らない (§3)。

## 10. 非対象 (意図的に持たないもの)

- 大きい EQ カーブ編集窓 (Reason の Spectrum EQ 相当)
- カーブ上のノードのドラッグ編集
- サムネイルへのスペクトラム重畳
- ゲート / エキスパンダー
- master バスの EQ / コンプ (§6 — 将来バス専用の別物として作る)
- プリセット、ch 間の Copy / Paste / Reset
- EQ / Comp / inserts の順序入れ替え (§1)
