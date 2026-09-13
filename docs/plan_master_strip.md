# マスターストリップ (バスコンプ + トーン EQ + リミッター) 設計

> **r.md #129 で一部を置き換えた。** Bus Comp / Tone EQ は `master_fx_chain` 上の組み込み device
> (`Device::Native`)、Limiter は `Song::master_limiter` になった。並べ替え・遅延の会計・オートメーション住所・
> テレメトリは [docs/plan_rack_native_devices.md](plan_rack_native_devices.md) が正本。本書に残るのは
> マスターパネルの見せ方とパラメータの範囲だけ。置き換えた決定は各節の冒頭に 1 行で示す。

マスターバスは他チャンネルと**別物**の処理を持つ。Reason も Mixbus もそうしていて、
理由も同じ — マスターに要るのは「潰して整える」ではなく **仕上げ** (グルー / トーンの
微調整 / 出力を絶対に超えさせない) だから。

通常チャンネルの内蔵ストリップは [docs/plan_channel_strip.md](plan_channel_strip.md)。
描画部品 (つまみ / カーブ / GR) は通常 ch の帯・Rack と共有する (`daw_gui/src/view/native_device/`)。

参考にした一次情報:

- [Reason 13 — The Main Mixer](https://docs.reasonstudios.com/reason13/the-main-mixer) /
  [The Master Section strip](http://docs.propellerheads.se/reason10/TTM%20Mixer.17.06.html)
  — マスターセクションは `Master Compressor → Insert FX → Master Fader`。**内蔵コンプが先、
  insert が後**が既定 (マキシマイザーを insert に挿すのが普通で、それは最後に居なければ
  出力でクリップするため)。`Inserts Pre Compressor` で反転可。Control Room 出力
  (モニター専用パス) は Master Out に影響しない。
- Reason の MASTER COMPRESSOR (ミキサー側、実機スクリーンショットで確認): `ON` /
  **針式 compression メーター (0〜20dB)** / `THRESHOLD` / `RATIO` (2・4・10) /
  `ATTACK` / `RELEASE` (AUTO 付き) / `MAKE-UP` / `EXTERNAL SIDE CHAIN` の `KEY`。
  Mix も Input Gain も無い (それはラック版 Master Bus Compressor の話)。
- [Reason 13 — Master Bus Compressor (ラック版)](https://docs.reasonstudios.com/reason13/master-bus-compressor)
  — Release の `Auto` は「長いピークの後は遅く、短いピークの後は速く」。
- [Mixbus 10 — Mastering Techniques](https://rsrc.harrisonconsoles.com/mixbus/mixbus-live-manual/10/en/topic/mastering-techniques)
  — マスターは `トーン (3 band、±6dB、90Hz シェルフ / 300Hz ワイドベル…) + コンプ +
  テープサチュレーション + マスターリミッター`。**マスターリミッターは ON/OFF 以外に
  操作子なし**、スレッショルド固定 -2dB、5ms ルックアヘッド、GR は 1 セグメント = 1dB、
  位置は最終段。

## 1. 信号経路

```
全 track の合算 → master_fx_chain (組み込み Bus Comp / Tone EQ を含む) → マスターフェーダー → リミッター → 出力
```

**廃止 (r.md #129)**: 「固定順で、並べ替えもトグルも持たない」。Bus Comp / Tone EQ は `master_fx_chain` 上の device で
Rack から並べ替えられる。既存曲を開いたときの並びは今と同じ音になる `Bus Comp → Tone EQ → insert`
— [plan_rack_native_devices.md](plan_rack_native_devices.md) §2 (Q3 / Q4 / Q6)。

**リミッターだけはフェーダーの後で固定 (維持)**。ここが「最終出力を絶対に超えさせない」唯一の場所で、
フェーダーの前に置くとフェーダーを上げた瞬間に破れる。Rack では末尾の動かせない行として出る (Q3)。

実行場所は `render_master_buffer` (live と書き出しが共有する唯一の描画関数、不変条件 6)。

## 2. レイテンシー

ルックアヘッドでマスター出力に 5ms の遅延が乗る (Mixbus と同じ割り切り)。ミックス全体に一様に掛かるのでトラック間の
ズレは生まれないが、**出力全体が曲位置より 5ms 遅れる**ので、この量は `Schedule::master_latency_samples`
(既存の PDC 会計) に足す。サンプル数への換算は `common::model::limiter_lookahead_samples` の 1 式だけ。
コンプと EQ は先読み無しの 0 サンプル。

**変更 (r.md #129)**: 「ON のときだけ遅延」ではなく、遅延の有無は `Song::master_limiter_latency_active()`
(静的 on、または On のレーン / 変調がある) で compile 時に決まる。OFF に解決されている区間は遅延だけを通す
— [plan_rack_native_devices.md](plan_rack_native_devices.md) §8.3.4 / §20-3。

## 3. UI

マスターパネル (`view/master_panel.rs`) の **MASTER セクション内**に置く。LU バーは
全高のまま、**数値欄の列だけを上下に割って**上にストリップを積む。フェーダー / メーターは
左に全高で残るので、**コンプの GR とフェーダーが必ず並んで見える**。

```
+-----+----+------------------------+
|     |    | COMP    ( 針メーター )  |   ← 上から信号順
| fdr | LU | Thr Ratio Atk / Rel Makeup |
|  +  | bar|------------------------|
| mtr |    | EQ    ~~ curve ~~      |
|     |    | Low  LoMid  High       |
|     |    |------------------------|
|     |    | LIM  ########   -1.0   |
|     |    |------------------------|
|     |    |  M  -14.2              |   ← 既存のラウドネス数値
|     |    |  S  -13.8   TP -0.8    |
+-----+----+------------------------+
```

- **常時表示**。折り畳みは持たない (マスターは 1 本しかないので、全 ch 一括で畳む
  通常 ch の事情が無い)。パネルが低いときは優先度の低いブロックから描かない
  (Bus Comp > Tone EQ > Limiter)。
- **変更 (r.md #129)**: Bus Comp と Tone EQ の上下は Rack (master チェーン) の前後に合わせて入れ替わり、Limiter は常に
  一番下 (Q17)。上の図は既定の並び — [plan_rack_native_devices.md](plan_rack_native_devices.md) §10.9。
- **ON/OFF は `Q` キー**。カーソルが Comp / EQ / LIM のどのブロックに乗っているかで
  対象が決まる (通常 ch の内蔵ストリップと同じ作法)。専用の ON ボタンは置かない。
- **セクションの中身を触ったら自動で ON** — 通常 ch と同じ (4 種共通の `NativeEdit::apply`)。
  つまみは再生中はオートメーション値に追従し、ジェスチャー (undo 1 step / 録音) と変調 (◉) を持つ。

### 3.1 針式 GR メーター

Reason と同じ**アナログ針式** (`0 2 4 8 12 20 dB COMPRESSION` の円弧目盛り + 針)。
通常 ch の細い GR バーと一目で別物と分かる。

**daw-ui の汎用 widget として追加する** (`needle_meter`: 値 / レンジ / 目盛りラベル /
弾道)。ドメイン知識は持たせない (不変条件 8)。針は VU 相当の減衰弾道で振れる —
数値の跳ねではなく「どれくらい、どんな速さで潰れているか」を形で読ませるため。
最小サイズ 80×50px。

## 4. パラメータ

### 4.1 Comp (Reason のマスターコンプ準拠)

| 操作子 | レンジ |
|--------|--------|
| Threshold | -30〜0 dB (連続) |
| Ratio | **3 択** 2:1 / 4:1 / 10:1 |
| Attack | **6 段** 0.1 / 0.3 / 1 / 3 / 10 / 30 ms |
| Release | **5 段** 100 / 300 / 600 / 1200 ms / `Auto` |
| Make-Up | -5〜+15 dB (連続) |

段階式なのはバスコンプの定石 (SSL バスコンプも同じ) で、選択肢が少ないぶん速く決まる。
`Auto` は program-adaptive — 長いピークの後は遅く、短いピークの後は速く戻る。

**廃止 (r.md #129)**: 「外部サイドチェーンを持たない」。Bus Comp も SC▾ を持ち、plugin と同じ手順で他トラックの音で
検出できる (Q19) — [plan_rack_native_devices.md](plan_rack_native_devices.md) §10.13。**検出フィルタは持たないまま**。
ニーは通常 ch と同じソフトニー (`COMP_KNEE_DB`)。

### 4.2 EQ (Mixbus のトーンコントロール準拠)

| バンド | 形 | 周波数 | レンジ |
|--------|-----|--------|--------|
| Low | ローシェルフ | 90 Hz 固定 | ±6 dB |
| Low-Mid | ワイドベル (Q 0.7) | 300 Hz 固定 | ±6 dB |
| High | ハイシェルフ | 8 kHz 固定 | ±6 dB |

**周波数は動かせない。** 「最終段で大きく動かすのは事故」という Mixbus の思想どおり、
よくある問題だけに絞る。狙った帯域を追い込むのは insert の EQ プラグインの仕事。
カーブ表示は daw_audio と同じ `common::dsp::tone_eq_magnitude_db` から描く。

### 4.3 リミッター

| 操作子 | レンジ |
|--------|--------|
| Ceiling | -6〜0 dBFS (既定 **-1.0**) |

リリースは信号追従の自動、ルックアヘッドは **5ms 固定**、アタックは実質 0
(ルックアヘッドで先に落とす)。GR は **1 セグメント = 1dB** の段表示 + シーリング値。

**サンプルピーク基準**。真のトゥルーピーク制限 (オーバーサンプリングして再構成波形の
ピークを見る) は扱わない — TP は既存のラウドネス表示で確認する。

## 5. オートメーションと変調

**全パラメータが対象**。master には `Track` が無いので、insert プラグインの param が既に
使っている **song-level レーン** (`MASTER_TRACK_ID`) に載せる。

**変更 (r.md #129)**: 住所は `MasterStrip(..)` ではなく、Bus Comp / Tone EQ は device id で束縛する
`AutomationTarget::NativeParam { device_id, param }`、Limiter は `AutomationTarget::MasterLimiter(On | Ceiling)`
— [plan_rack_native_devices.md](plan_rack_native_devices.md) §5.3。段階式 (Ratio / Attack / Release) は段の index を
正規化して載せ、値の表示は `4:1` / `Auto` のような段のラベル。

**マスターゲイン (フェーダー) は対象外のまま**。今回の範囲を広げない。

## 6. データモデル / 永続 / テレメトリ

- **廃止 (r.md #129)**: `Song.master_strip` / `AudioCommand::SetMasterStrip` / GR 2 本 (`master_comp_gr_db` /
  `master_limiter_gr_db` のスカラー面) / `MasterStripState`。Bus Comp / Tone EQ は native device (値 IPC は
  `SetNativeDevice`、GR は device id キーの面)、Limiter は `Song::master_limiter` (`SetMasterLimiter`、GR は
  `master_limiter_gr_db`)、RT 状態は `MasterLimiterState` — [plan_rack_native_devices.md](plan_rack_native_devices.md) §5 / §8 / §11。
- 値は `Song` に保存し `*` (dirty) を立てる。変更は `edit_song()` チョークポイント経由。
- **GR は波形からは導けない** (どれだけ下げたかは処理側しか知らない) ので、`MasterAnalyzer` ではなく処理側が面に書く
  (この方針は維持)。

## 7. 通常トラックとの違い (なぜ揃えないか)

**根拠が消滅 (r.md #129)**: 「devices を分類できないので通常トラックは内蔵が後」は、組み込みをチェーン上の device にして
ユーザーが位置を決める形で解消した。新しい device の既定の挿入位置は Q6 — [plan_rack_native_devices.md](plan_rack_native_devices.md) §5.7。

## 8. 非対象 (意図的に持たないもの)

- モニターセクション / Control Room 出力 (マスターの後段でモニター音量・DIM・MONO を
  持ち、書き出しには乗らない段)。**現状 daw_01 には無い。** (通常 ch の `SC Listen` は r.md #129 で
  聴き方の状態として Song の外 (`NativeIo`) に出したので、書き出しに乗らないことは既に保証されている。)
- テープサチュレーション (Mixbus のマスターにはある)
- 検出フィルタ (§4.1)
- `Inserts Pre Comp` トグル (並べ替えは Rack の D&D で行う)
- トゥルーピーク制限 (§4.3)
- マスターゲインのオートメーション (§5)
