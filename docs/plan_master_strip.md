# マスターストリップ (バスコンプ + トーン EQ + リミッター) 設計

マスターバスは他チャンネルと**別物**の処理を持つ。Reason も Mixbus もそうしていて、
理由も同じ — マスターに要るのは「潰して整える」ではなく **仕上げ** (グルー / トーンの
微調整 / 出力を絶対に超えさせない) だから。

通常チャンネルの内蔵ストリップは [docs/plan_channel_strip.md](plan_channel_strip.md)。
そちらとは**別実装・別 UI** で、共有するのは DSP の部品 (バイクワッド / 圧縮カーブ) だけ。

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
全 track の合算 → Comp → EQ → insert プラグイン → マスターフェーダー → リミッター → 出力
```

**固定順で、並べ替えもトグルも持たない。** 内蔵が先・insert が後なのは Reason の既定と
同じ理由 — マキシマイザー / マスタリング用プラグインを挿したとき、それが (リミッターを
除いて) 最後に来る。

**リミッターだけはフェーダーの後**。ここが「最終出力を絶対に超えさせない」唯一の場所で、
フェーダーの前に置くとフェーダーを上げた瞬間に破れる。

通常トラックは `devices → Comp → EQ → Pan → Fader` のまま (§7 参照)。

実行場所は `render_master_buffer` (live と書き出しが共有する唯一の描画関数、不変条件 6)。

## 2. レイテンシー

リミッターが **ON のときだけ**、ルックアヘッドでマスター出力に 5ms の遅延が乗る
(Mixbus と同じ割り切り)。ミックス全体に一様に掛かるのでトラック間のズレは生まれないが、
**出力全体が曲位置より 5ms 遅れる**ので、この量は `Schedule::master_latency_samples`
(既存の PDC 会計) に足す — 書き出しはその分だけ窓をずらして時間軸を揃え、
メトロノームのクリックはその分だけ前へ出す。サンプル数への換算は
`common::model::limiter_lookahead_samples` の 1 式だけ (DSP の遅延線長と会計が
1 サンプルも食い違わないように)。コンプと EQ は先読み無しの 0 サンプル。

リミッターを **OFF にすると遅延も無くなる** (素通し)。切り替えの瞬間に出力が 5ms
飛ぶ / 詰まるので再生中に切り替えるとプツッと鳴るが、「使っていないのに常に 5ms
遅れる」より切り替え時の一瞬を取る (grill で確定)。ON/OFF は `edit_song` を通るので
LoadSong → schedule 再 compile が同じフレームで走り、会計も即追従する。

## 3. UI

マスターパネル (`view/master_panel.rs`) の **MASTER セクション内**に置く。LU バーは
全高のまま、**数値欄の列だけを上下に割って**上にストリップを積む。フェーダー / メーターは
左に全高で残るので、**コンプの GR とフェーダーが必ず並んで見える**。

```
+-----+----+------------------------+
|     |    | COMP    ( 針メーター )  |   ← 上から信号順
| fdr | LU | Thr Rat Atk Rel Gain   |
|  +  | bar|------------------------|
| mtr |    | EQ    ~~ curve ~~      |
|     |    | Lo   LoMid   Hi        |
|     |    |------------------------|
|     |    | LIM  ########   -1.0   |
|     |    |------------------------|
|     |    |  M  -14.2              |   ← 既存のラウドネス数値
|     |    |  S  -13.8   TP -0.8    |
+-----+----+------------------------+
```

- **常時表示**。折り畳みは持たない (マスターは 1 本しかないので、全 ch 一括で畳む
  通常 ch の事情が無い)。パネルが狭い / 低いときは、既存のラウドネス表示と同じ作法で
  **内側から要素を落とす** (数値 → カーブ → ノブの順に諦める)。
- **ON/OFF は `Q` キー**。カーソルが Comp / EQ / LIM のどのブロックに乗っているかで
  対象が決まる (通常 ch の内蔵ストリップと同じ作法)。専用の ON ボタンは置かない。
- **セクションの中身を触ったら自動で ON** — 通常 ch と同じ。

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

**外部サイドチェーンと検出フィルタは持たない** (Reason のミキサー側コンプも持たない)。
ニーは通常 ch と同じソフトニー (`COMP_KNEE_DB`)。

### 4.2 EQ (Mixbus のトーンコントロール準拠)

| バンド | 形 | 周波数 | レンジ |
|--------|-----|--------|--------|
| Low | ローシェルフ | 90 Hz 固定 | ±6 dB |
| Low-Mid | ワイドベル (Q 0.7) | 300 Hz 固定 | ±6 dB |
| High | ハイシェルフ | 8 kHz 固定 | ±6 dB |

**周波数は動かせない。** 「最終段で大きく動かすのは事故」という Mixbus の思想どおり、
よくある問題だけに絞る。狙った帯域を追い込むのは insert の EQ プラグインの仕事。
カーブ表示は通常 ch と同じ `eq_magnitude_db` を流用する。

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

```rust
// common/src/model/automation.rs
pub enum AutomationTarget {
    …,
    /// マスターストリップ (コンプ / トーン EQ / リミッター) のパラメータ。
    MasterStrip(MasterStripParam),
}
```

段階式のパラメータ (Ratio / Attack / Release) は **段の index** を正規化して載せる
(`TrackBuiltin::Mute` と同じ「階段」扱い = 曲線は段になる)。

**マスターゲイン (フェーダー) は対象外のまま**。今回の範囲を広げない。

## 6. データモデル / 永続 / テレメトリ

- `Song` に `master_strip: MasterStrip` を追加 (`#[serde(default)]`、bincode
  `Encode/Decode`)。型は `common/src/model/track/channel_strip.rs` に同居させる
  (レンジの SSoT である `ParamRange` と DSP の語彙を共有するため)。
- 値は `Song` に保存し `*` (dirty) を立てる。変更は `edit_song()` チョークポイント経由、
  audio へは値のみ更新の `AudioCommand::SetMasterStrip` で送る (graph は再 compile しない)。
- **GR テレメトリ**: コンプとリミッターの 2 本を `AudioBridge` に足す
  (`master_comp_gr_db` / `master_limiter_gr_db`)。マスターのメーター類は本来
  `MasterAnalyzer` が波形から導く方針だが、**GR は波形からは導けない** (どれだけ
  下げたかは処理側しか知らない) ので、per-track の GR と同じスカラー面に載せる。
- RT 状態 (バイクワッド遅延 / 平滑ゲイン / ルックアヘッドリング) は `MasterStripState` に
  持ち、engine と書き出しがそれぞれ 1 個ずつ所有する (書き出しは毎回新品 = 決定論的)。

## 7. 通常トラックとの違い (なぜ揃えないか)

Reason は通常 ch も `Dynamics → EQ → Inserts → Fader` (内蔵が先) だが、**daw_01 の
通常トラックでは実現できない**。`Track.devices` は Reaper 流の単一チェーンで、MIDI FX /
楽器 / エフェクトを役割で区別しない (不変条件 1 / 「役割判定しない」設計)。ストリップを
チェーンの前に置くと、楽器トラックでは楽器が音を出す前に処理することになり、楽器の出力が
ストリップを素通りする。「楽器の後・エフェクトの前」に差し込むには devices を分類する
しかなく、それは意図的に禁じている。

master の `master_fx_chain` は**純粋に insert だけ**なので、この問題が無い。
だから master だけ Reason と同じ並びにできる — マスターだけ並びが違うことには
この根拠がある。

## 8. 非対象 (意図的に持たないもの)

- モニターセクション / Control Room 出力 (マスターの後段でモニター音量・DIM・MONO を
  持ち、書き出しには乗らない段)。**現状 daw_01 には無い。** 作るなら通常 ch の
  `SC Listen` もそこへ移すのが筋。
- テープサチュレーション (Mixbus のマスターにはある)
- 外部サイドチェーン / 検出フィルタ (§4.1)
- 順序の入れ替え・`Inserts Pre Comp` トグル (§1)
- トゥルーピーク制限 (§4.3)
- マスターゲインのオートメーション (§5)
