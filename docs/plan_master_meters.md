<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# マスターパネルと各種メーター (r.md #50)

Mixer 右端にあった MASTER ストリップを、画面右端にフル高で常駐する **マスターパネル** へ移設し、
VU / ピーク / ラウドネス / スペクトラム / オシロスコープ / ゴニオ・位相相関を常時表示する。

grill-me (2026-08-15) で確定した決定:

| 論点 | 決定 |
|---|---|
| パネル構成 | 4 セクションを**全部同時に**縦積み (タブ切替をしない) |
| 幅 / 消し方 | 境界ドラッグで可変 + View メニュー / `Ctrl+Alt+M` で表示 ON/OFF。幅・表示状態は `app_config.json` |
| MASTER の中身 | 現状のまま (名前 + フェーダー + L/R メーター)。ミュート / モノ等は足さない = **音の振る舞いは一切変えない** |
| 測定対象 | **曲の音だけ** (`render_master_buffer` の出力 = 書き出す WAV と同一)。メトロノーム / パニック declick は含まない |
| ラウドネス積算 | **再生開始で自動リセット** + 手動リセットボタン |
| 設定の場所 | **各メーターを右クリック**してその場のメニューで変更。値は `app_config.json` |
| VU の見せ方 | 1 本のバーに二重表示 (塗り = VU、細線 = ピーク、上端数値 = 最大ピーク) |
| ステレオ | ゴニオメーター + 位相相関バー + ステレオ幅 / 左右バランス |
| セクション高 | 自動配分 + 境界ドラッグで再配分 (最低高を割ったらパネル内を縦スクロール) |
| ラウドネス目標 | 既定 **-14 LUFS** (配信基準)、上限 -1 dBTP |

---

## 1. データの流れ (SSoT)

```
daw_audio                                   daw_gui
──────────                                  ───────
render_master_buffer()                      spawn_playhead_poller (33ms / 省電力 250ms)
  = master fx → master gain                   │
      │  master_l / master_r                  │  ScopeBridge から前回カーソル以降を drain
      ▼                                       ▼
  ScopeBridgeHandle::write_block ──shmem──▶ MasterAnalyzer::process(&[[f32;2]])
  (RT: 事前確保リングへ store のみ)              │  ├ ピーク / トゥルーピーク / VU / RMS
                                                │  ├ K-weighting → 100ms ブロック → M / S / I / LRA
      (metronome click はこの後に足すので         │  ├ FFT (Hann 4096 / hop 1024) → 対数バンド
       メーターには乗らない)                      │  ├ トリガ検出 → オシロ列 (min/max)
                                                │  └ 相関 / 幅 / バランス (EMA)
                                                ▼
                                          MasterMeterSnapshot ── AppEvent::MasterMeterTick ──▶ view
```

- **RT スレッドがやるのは「リングへ書く」だけ**。log / FFT / フィルタは一切通らない (不変条件 4)。
- 計測はすべて **1 つの `MasterAnalyzer`** が持つ。ピーク値も含めてここが唯一の出どころ
  (旧 `AudioBridge.peak_l / peak_r` の GUI 表示経路は撤去。SSoT を割らない)。
- 書き出し (`export.rs`) はリングへ書かない。書き出し中は live コールバックが無音を出すので
  メーターは自然に落ちる。

### 1.1 `AudioBridge.peak_l / peak_r` の扱い

- **削除する**。GUI の表示は `MasterAnalyzer` が持つため、shmem に置く理由が無くなる。
- アイドル park の無音判定 (`buffer_is_idle`) は CPAL コールバック内のローカル値
  (`block_peaks_stereo(data)`) をそのまま使う。こちらは「デバイスへ実際に出た音」でなければ
  ならないので、メーターの測定点 (曲の音だけ) とは**目的が違う別物**。共有しない。
- `AppEvent::Tick { peak_l, peak_r }` / `transport.peak_l_display` / `peak_r_display` /
  `peak_l_norm` / `peak_r_norm` も併せて撤去。

### 1.2 `ScopeBridge` (新 shmem 面)

`common/src/scope_bridge.rs`。`MetricsBridge` と同じく daw_gui が create、daw_audio が open。

```rust
pub const SCOPE_FRAMES: usize = 1 << 17;   // 131072 frames = 2.73s @48k / 0.68s @192k

#[repr(C)]
pub struct ScopeBridge {
    write_frames: AtomicU64,        // 累積書き込みフレーム数 (monotonic)
    sample_rate:  AtomicU32,
    _pad:         AtomicU32,
    samples:      [[AtomicU32; 2]; SCOPE_FRAMES],   // f32::to_bits の L,R
}
```

- 単一 writer (RT) / 単一 reader (GUI ポーラ) の**上書きリング**。writer は
  `Relaxed` store でサンプルを埋め、最後に `write_frames` を `Release` store。
- reader は `write_frames` を `Acquire` load し、`max(cursor, w - SCOPE_FRAMES)` から読む。
  リングを一周されると古い分は失われる = **overrun**。reader は検出して
  `snapshot.overrun` に立てるだけ (積算ラウドネスの信頼性表示に使う)。
  2.7 秒ぶんあるので、ポーラが 2.7 秒止まらない限り起きない。
- 1MB。`common/build.rs` の `WIRE_SOURCES` に追加する (不変条件 7)。
  → PROTOCOL_FINGERPRINT が変わるので **`make build` でワークスペース全体を再ビルド**すること。

---

## 2. 計測仕様 (一次情報)

### 2.1 ラウドネス — ITU-R BS.1770-5 / EBU Tech 3341 / 3342

K-weighting は 48kHz の表をハードコードせず、**任意サンプルレートへ再設計**する
(48kHz を代入すると規格表と小数 14 桁まで一致することを回帰テストで固定)。

```
段1 (shelving): f0 = 1681.974450955533 Hz, G = 3.999843853973347 dB,
                Q  = 0.7071752369554196
    K = tan(pi*f0/fs); Vh = 10^(G/20); Vb = Vh^0.4996667741545416;
    den = 1 + K/Q + K^2
    b = [(Vh + Vb*K/Q + K^2)/den, 2(K^2 - Vh)/den, (Vh - Vb*K/Q + K^2)/den]
    a = [1, 2(K^2 - 1)/den, (1 - K/Q + K^2)/den]

段2 (RLB high-pass): f0 = 38.13547087602444 Hz, Q = 0.5003270373238773
    K = tan(pi*f0/fs); den = 1 + K/Q + K^2
    b = [1, -2, 1]
    a = [1, 2(K^2 - 1)/den, (1 - K/Q + K^2)/den]
```

- ラウドネス `L = -0.691 + 10*log10(Σ_i G_i * z_i)`。ステレオなので `G_L = G_R = 1.0`。
  `-0.691` は fs によらず**固定** (規格の要求)。
- **100ms サブブロック**を共通基盤にする。`samples_per_100ms = (fs + 5) / 10`。
  - Momentary (400ms) = 直近 4 サブブロックの平均
  - Short-term (3s)   = 直近 30 サブブロックの平均
  - Integrated = 400ms ゲーティングブロック (ホップ 100ms = 75% オーバーラップ) を
    絶対ゲート -70 LKFS → 相対ゲート (絶対ゲート後 -10 LU) の 2 段で平均。
    履歴は **1000 bin / 0.1 dB / 下端 -70 LUFS のヒストグラム**に積んでメモリ一定 (libebur128 と同じ)。
  - LRA = Short-term を 10Hz で別配列に積み、絶対 -70 LUFS → 相対 -20 LU の 2 段ゲート後、
    昇順ソートして 1-based `round((n-1)*p/100 + 1)` の 95% - 10%。
    リセット後 60 秒は「暫定」表示にする (Tech 3342)。
- トゥルーピーク = BS.1770 Annex 2 の **48-tap / 4-phase FIR** (fs<96k で 4x、fs<192k で 2x、
  以上はそのまま)。浮動小数処理なので 12.04 dB の減衰・復元は行わない (規格が不要と明記)。
  フィルタ充填前の過渡は最大値に含めない。

### 2.2 VU / ピーク

- VU = IEC 60268-17 の 2 次系。オーバーシュート 1.0% → `ζ = 0.8261, ωn = 13.9725 rad/s`
  (300ms で 99% 到達)。**mean-square に適用**してから `10*log10`。
- 0 VU の基準は既定 **-18 dBFS** (EBU R68 / Cubase 既定)、右クリックで -20 dBFS (SMPTE) に変更可。
  バーは **dBFS 目盛りのまま**で、基準は `LevelMeterStyle.reference_db` の**アライメント線 1 本**
  として示す (Ardour が「メーターのスケール」と「line-up level」を別設定に持つのと同じ)。
  塗り (VU) を基準ぶんシフトしてしまうと、ピークと VU が同一写像を共有するという
  二重表示の前提が壊れるため。
- ピークバー = sample peak。落下 **13.3 dB/s** (x42 / Ardour の de-facto)。
  ピーク保持線 1.5 秒。上端の数値は減衰なしの最大到達値でクリックリセット (既存 widget の機能)。
- クリップ表示 = `|x| >= 1.0` を検出したら赤点灯、クリックでリセット。

### 2.3 スペクトラムアナライザ

| 項目 | 既定 | 右クリックの選択肢 |
|---|---|---|
| FFT 長 | 4096 | 1024 / 2048 / 4096 / 8192 |
| 窓 | Hann | Hann / Blackman-Harris (BH92) |
| オーバーラップ | 75% (hop = N/4) | 固定 |
| 正規化 | `mag = 2*|y| / S1` (`S1 = Σw`) → 0 dBFS 正弦がビン中心で 0 dB | — |
| 周波数軸 | 20Hz–20kHz 対数 | — |
| 振幅レンジ | -100 .. 0 dBFS | 60 / 90 / 100 / 120 dB |
| 傾き | 4.5 dB/oct (1kHz 支点) | 0 / 3 / 4.5 / 6 |
| 集約 | ピクセル列内のパワー最大 | — |
| 平滑 | アタック即時、リリース 20dB / 600ms | 100 / 300 / 600 / 1500 ms |
| ピーク保持 | 2 秒保持 → 13.3 dB/s で落下 | ON / OFF |

平均・集約は必ず**パワー領域**で行い、dB 化は最終段だけ。

### 2.4 オシロスコープ

- 全幅 20ms 既定 (5 / 10 / 20 / 50 / 100 ms)。
- トリガ = Mid (L+R) の**立ち上がりゼロクロス**。隣接サンプル対を見つけてから
  放物線フィットでサブサンプル位置を求める (zita-scope)。ホールドオフ 50ms。
  トリガが見つからないフレームは直前の位相を維持する (流れない)。
- 1 px = 時間区間なので区間内 min/max を縦線で描く。L / R を 2 色で重ねる。

### 2.5 ゴニオ / 位相相関 / 幅 / バランス

- ゴニオ: `x = (R - L)/√2`, `y = (L + R)/√2` (画面 y は反転)。縦線 = モノ、横線 = 逆相。
  残光は**直近 N 点のリング**を保持して古い点ほど alpha を落とす
  (指数減衰 `persist^age` を `persist^N < 1/255` で打ち切ったもの = 見た目は連続残光と等価)。
  前点との距離² が 2px² 未満の点は捨てる (x42 と同じ間引き)。
- 相関: Fons Adriaensen `stcorrdsp` 準拠。2kHz 1 極 LPF → 時定数 0.3s の EMA で
  `zlr / sqrt(zll*zrr)`。デノーマル注入と有限性チェックを省かない。
- 幅 `W = rms(S)/rms(M)` を % 表示、バランス `10*log10(P_R/P_L)` を dB 表示 (3 秒 EMA)。

---

## 3. 画面

```
menu (24)
transport (44)
┌────────────┬──────────────────────────┬─────────────┐
│            │ arrangement              │  MASTER     │  ← 新パネル
│ inspector  ├──────────────────────────┤  spectrum   │     (幅可変、
│ (280 固定) │ bottom (mixer/pianoroll) │  scope      │      境界ドラッグ)
│            │                          │  gonio      │
└────────────┴──────────────────────────┴─────────────┘
status (24)
```

- `root.rs::build_root` の `center_bottom_rect` から右へパネル幅を切り出し、残りを
  従来どおり inspector + split(arrangement / bottom) に配る。
- パネル内は 4 セクションを縦に積み、境界 (4px) をドラッグして配分を変える。
  配分は「MASTER : spectrum : scope : gonio」の比率 4 つを `app_config.json` に保存。
  最低高 (MASTER 160 / spectrum 90 / scope 70 / gonio 120) を割ったら縦スクロール。
- MASTER セクションは左 = フェーダー + L/R メーター (幅 55)、右 = ラウドネス
  (LU バー + M / S / I / LRA / TP の数値 + リセットボタン)。

### 3.1 操作

| 操作 | 結果 |
|---|---|
| `Ctrl+Alt+M` / View メニュー | パネルの表示 ON / OFF |
| パネル左端をドラッグ | 幅変更 (最小 180 / 最大 640) |
| セクション境界をドラッグ | 高さ配分 |
| メーターのピーク数値をクリック | ピーク保持 + クリップ表示をリセット (`peak_reset_epoch`) |
| クリップ表示をクリック | 同上 |
| 各メーターを右クリック | そのメーター専用の設定メニュー |
| ラウドネスの Reset ボタン | I / LRA / 最大 M / 最大 S / 最大 TP を同時リセット (`loudness_reset_epoch`) |
| 再生開始 | 上と同じリセットが自動で走る |

積算とピークでリセット世代を分けているのは、「ピーク数値のクリック」と「ラウドネスの
Reset」が実 DAW でも別操作だから (前者はメーター単位、後者は測定セッション単位)。

MASTER セクションの数値は M / **M max** / S / **S max** / I / LRA / TP の 7 行。
最大 M・最大 S は EBU Tech 3341 §2.2 が表示を要求しており、Integrated と同時にリセットされる。

---

## 4. 実装の置き場所

| 置き場所 | 中身 |
|---|---|
| `common/src/scope_bridge.rs` | shmem リング (writer / reader)。`WIRE_SOURCES` に追加 |
| `common/src/meter.rs` | 既存の dB 変換プリミティブ (据え置き) |
| `daw_gui/src/master_meter/mod.rs` | `MasterAnalyzer` / `MasterMeterSnapshot` / `MeterSettings` |
| `daw_gui/src/master_meter/loudness.rs` | K-weighting / 100ms ブロック / M・S・I・LRA |
| `daw_gui/src/master_meter/truepeak.rs` | BS.1770 48-tap 4-phase FIR |
| `daw_gui/src/master_meter/spectrum.rs` | 窓 + FFT + 対数バンド集約 + 弾道 |
| `daw_gui/src/master_meter/scope.rs` | トリガ + 列 min/max |
| `daw_gui/src/master_meter/stereo.rs` | 相関 / 幅 / バランス |
| `ui/crates/ui/src/widgets/spectrum.rs` | 汎用スペクトラム widget |
| `ui/crates/ui/src/widgets/oscilloscope.rs` | 汎用オシロ widget |
| `ui/crates/ui/src/widgets/goniometer.rs` | 汎用ゴニオ + 相関バー widget |
| `ui/crates/ui/src/widgets/loudness_meter.rs` | 汎用ラウドネス widget (LU バー + 数値) |
| `daw_gui/src/view/master_panel.rs` | パネルの組み立て・境界ドラッグ・右クリックメニュー |

daw-ui 側の widget は **オーディオのドメイン知識を持たない** (与えられた配列を描くだけ)。
`common::model` も IPC も触らない = 不変条件 8 に整合 (`level_meter` と同じ層)。

FFT は `realfft` (rustfft ラッパ) を新規依存として `[workspace.dependencies]` に追加し、
**daw_gui だけ**が使う。

---

## 5. アイドル省電力 (r.md #49) との整合

- `MasterAnalyzer` は毎ティックの表示状態を量子化した `visual_digest: u64` を出す。
  `tick_visual_fingerprint` はこの 1 値だけを混ぜる (旧 `peak_l_norm` / `peak_r_norm` の置き換え)。
  無音で弾道が落ち切れば digest は変化しなくなり、再描画も止まる。
- 前回ポーリング以降に新しいフレームが届かなかった場合 (= エンジンが park した / 音が出ていない)、
  経過時間ぶんの**無音を解析器に流し込む**。これで凍結せず自然に落ちる。
- **落ち切ったら解析を休む**: 入力が完全な無音で、かつ digest が 2 ティック続けて変化しなければ
  `MasterAnalyzer` は解析自体をスキップする (`is_paused_on_silence`)。休まないと、park して
  何時間経っても K-weighting / トゥルーピーク FIR / FFT が実時間レートで回り続け、#49 の
  省電力を GUI 側で打ち消してしまう。音が 1 サンプルでも戻れば次のティックで即再開する。
- **ゴニオ点は固定長にリサンプルしてから digest に混ぜる**。点数はティックごとに揺れる
  (ポーラの sleep とオーディオコールバックの位相) ので、点をそのまま順に混ぜると全点が
  (0,0) の無音でも「混ぜた回数」で値が変わり、指紋が永久に収束しない。
- パネルが閉じているときは解析自体を回さない (`awake` と同じ条件でスキップ)。

## 6. 壊れた入力への備え

- **非有限サンプルは解析器の入口 1 か所で 0 に潰す** (`MasterAnalyzer::consume`)。NaN / Inf が
  1 サンプル混ざるだけで 2 次系の積分と biquad の状態が汚染され、音が正常に戻っても
  **永久に復帰しない** (`y.max(0.0)` は NaN を 0 として返すので表示は無音のまま固まる)。
- ステレオの非有限ガードは相関だけでなく**幅 / バランスの EMA も同時に**畳む。
- リングの読み出し上限はサンプルレート追従 (`max_read_frames`)。固定 48000 にすると
  192kHz + 省電力の 250ms ポーリングで毎ティック取りこぼす。
- パネル幅 / セクション配分のドラッグは **release でだけ** `app_config.json` に書く
  (ドラッグ中に毎フレーム同期書き込みしない)。
- 右クリックメニューは **座標に依らない安定 id** で開く。`context_menu_for` は popup id を
  rect 座標から作るため、パネル幅やセクション高が変わると popup が `open_popups` に
  取り残され、見えないまま click を食う領域になる。パネルを閉じるときは明示的に閉じる。
