<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan: メーター目盛の数字間引き (FIXME #37)

## 目的

Mixer strip の高さが小さくなると、level meter の dB 目盛の数字 (label) 同士が縦に重なって読めなくなる
(FIXME #37: 「Mixer の高さが小さくなるとメーター目盛の数字同士が重なります。まびいてください。」)。

目盛 (tick line) と数字 (label) を**一緒に間引いて**、隣接する表示要素が縦方向に重ならないようにする。

## 確定設計 (interview で決定済み、再議論しない)

1. **修正は gui_01 の `level_meter` widget 側に置く**。daw_01 は今までどおり `MeterScale::default()` を
   渡すだけで一切変更しない。よってこの plan は (1) **gui_01 への要望仕様** と (2) **daw_01 = 変更なしの確認**
   を記述する。
2. **アルゴリズム**: tick line も label も「一緒に」間引く。**0 dB を必ず残すアンカー**にし、0 dB を基点に
   上下へ向かって、隣接する表示済み要素とのピクセル間隔が「重ならない最小間隔」を満たすものだけ表示する。
   満たさないものは tick も number も両方省く。
3. **間引きは実ピクセル位置基準の貪欲法**で行う。メーターのカーブは非線形 (0 dB 付近を引き伸ばし -60 付近を
   圧縮) なので、固定 dB ステップ方式は不採用。実ピクセル間隔で判定するため、結果として**上は細かく下は粗く**
   なる。
4. **重ならない最小間隔の目安**は `line_height` 以上 (= `font_px + 2` ≒ 11 px)。
5. **メーター高さが極端に小さくても 0 dB ラベルだけは最後まで残る**。

---

## (1) gui_01 widget change spec (要望内容)

### 対象ファイル / 関数

`F:\dev\gui_01\crates\ui\src\widgets\level_meter.rs` の `draw_meter_scale` (現状 L420-473)。

この関数は

- `level_meter_stereo` → `meter_body` (L351 `self.draw_meter_scale(...)`)
- `channel_fader_meter` → `meter_body` (L121) → `draw_meter_scale` (L351)

の両経路から呼ばれる **唯一の目盛描画箇所**。Mixer strip は後者 (`channel_fader_meter`) を使うが、間引きを
`draw_meter_scale` に入れれば両経路に効く。

### 現状の挙動 (verify 済み)

`draw_meter_scale` (L432-472) は `scale.labels_db` (`DEFAULT_SCALE_DB` = `[6.0, 0.0, -6.0, -12.0, -18.0,
-24.0, -30.0, -36.0, -42.0, -48.0, -54.0, -60.0]`、12 値、L58-59) を**全件ループ**して各 dB に対し:

- y 位置: `let f = meter_frac(tick_db, style); let ty = (content.y + content.h - content.h * f).round();`
  (L433-434)。`meter_frac` は `scale.curve` (= `DEFAULT_CURVE`、非線形 top-weighted、L64-77) を通すので
  ピクセル位置は非等間隔。
- tick line: L438-445 で `Rect { y: ty - 1.0, h: 2.0, ... }` の 2px 横線 (L バー左の short tick)。
- 0 dB 横線: `is_zero` のとき L448-456 で L/R 両バーを横切る 3px 横線 (これは間引き対象外、0 dB は常に残る)。
- label: `has_label_room` (L431、**横方向のみの判定** = `(clip.x + clip.w - label_left) > SCALE_FONT_PX *
  0.8`) が true のとき L457-471 で `push_text`。label の縦位置は
  `label_top = (ty - SCALE_FONT_PX * 0.62).clamp(clip.y, clip.y + clip.h - SCALE_FONT_PX)` (L459-460)、
  `font_size: SCALE_FONT_PX` (=9.0、L45)、`line_height: SCALE_FONT_PX + 2.0` (=11.0、L466)。

**問題点**: `has_label_room` は横方向の余白しか見ておらず、**縦方向の重なりを一切判定していない**。
高さが小さいと隣接 dB の `ty` が `line_height` 未満に詰まり、数字が重なる。

### 変更内容

`draw_meter_scale` 内のループを、**「全 dB を一旦 (dB, ty) に解決 → 0 dB をアンカーに貪欲間引き → 残った
ものだけ tick + label を描画」** の 2 パスに作り替える。

#### Pass 1: 各 label dB のピクセル y を解決

`scale.labels_db` を走査し、各 `tick_db` について `f = meter_frac(tick_db, style)` →
`ty = (content.y + content.h - content.h * f).round()` を計算して `(tick_db, ty, is_zero)` の列を作る。
`labels_db` は仕様上「上 → 下」(dB 降順) なので `ty` は単調増加 (上=小さい y → 下=大きい y) になる。

#### Pass 2: 0 dB アンカー貪欲間引き

- **アンカー**: `tick_db.abs() < 1e-3` (= 0 dB) を必ず採用する。`labels_db` に 0 dB が無い退避ケースでは
  `ty` が中央に最も近い要素をアンカーにする (`DEFAULT_SCALE_DB` には 0 dB が含まれるので通常はそのまま 0 dB)。
- **最小間隔** `min_gap = SCALE_FONT_PX + 2.0` (= 11 px、= 現 label `line_height` と同値)。定数化推奨
  (例: `const SCALE_LABEL_MIN_GAP: f32 = SCALE_FONT_PX + 2.0;`)。
- アンカーの index を起点に:
  - **上方向** (index を 0 へ): 直近に採用した要素の `ty` から `|ty_candidate - ty_last| >= min_gap` を
    満たす要素だけ採用、満たさなければスキップ。採用した時点で `ty_last` を更新。
  - **下方向** (index を末尾へ): 同様に `min_gap` 判定で採用/スキップ。
- 採用集合 (= `Vec<bool>` か `HashSet<usize>`、widget は heap 確保に neutral なので問題なし) を作る。

メーターのカーブが非線形 (上を引き伸ばし下を圧縮、`DEFAULT_CURVE` L64-77) なので、ピクセル間隔は上が広く
下が狭い。`min_gap` 判定を実ピクセルで行うことで、**上は多く残り下は粗く間引かれる**結果になる (設計どおり)。

#### Pass 3: 描画

採用された要素だけ tick line (L438-445 相当) と label (L457-471 相当) を描く。

- **0 dB の横線 (L448-456) は従来どおり常に描く** (0 dB は常にアンカーで採用される)。
- tick と label は**セットで間引く** (採用されなければ両方とも描かない)。これは設計の「tick も number も
  両方省く」要件。現状 tick は無条件、label だけ `has_label_room` で出していたが、**両方を採用集合で gate** する。
- `has_label_room` (横方向判定、L431) は**そのまま残す**。横に数字の幅が無いケース (極小 `meter_w`) は
  label を出さない既存挙動を維持し、縦間引きと AND を取る (label = 採用集合 AND `has_label_room`)。
  tick は横余白に依らないので採用集合のみで gate。

### エッジケース

- **高さが極端に小さい**: 上下とも `min_gap` を満たす要素が一つも無くても、アンカー (0 dB) の tick + 0 dB
  横線 + label は必ず残る (設計の「0 dB ラベルだけは最後まで残る」)。
- **`labels_db` に 0 dB が無い**: 中央最寄り `ty` をアンカーにフォールバック (上記)。`DEFAULT_SCALE_DB` には
  0 dB があるので daw_01 の実運用では発生しない。
- **`labels_db` が空**: ループが回らず何も描かない (現状と同じ、panic しない)。
- **`emphasize_zero = false`**: アンカーは依然 0 dB のピクセル位置を使う (アンカーの定義は `is_zero` フラグでは
  なく「0 dB であること」)。0 dB 横線・明色は出ないが間引きアンカーとしては機能する。
- **`round()` で複数要素の `ty` が同値**になる degenerate (高さ極小): `min_gap` 判定 (`>= 11`) で自然に
  弾かれるので二重描画しない。

### テスト (gui_01 側、widget unit test)

既存の `#[cfg(test)] mod tests` (level_meter.rs L575-818) に追加する想定:

- **重なり無し**: 小さい `rect.h` (例 `h: 60.0`) で `scale: Some(MeterScale::default())` を描き、
  `scene.iter_glyphs()` の隣接 `top` 差が全て `>= SCALE_FONT_PX + 2.0 - ε` であること。
- **0 dB 残存**: 極端に小さい `rect.h` (例 `h: 24.0`) でも label `"0"` が必ず存在すること
  (`labels.contains(&"0")`)。
- **大きい高さでは全件**: 大きい `rect.h` (例 `h: 400.0`) では全 12 ラベルが出ること
  (現状の `scale_layout_*` テストと矛盾しない回帰確認)。
- **tick と label の対応**: 描画された tick line (h≒2.0 の rect) の本数と label の本数が一致 (両方間引きの確認)。
  ※ 0 dB は tick(2px) + 横線(3px) を持つので、テストは tick(h≒2.0) だけを数えて label 数と突き合わせる。

### gui_01_conversation.md への提出フォーマット

`F:\dev\daw_01\docs\gui_01_conversation.md` 末尾に `### daw_01 → [要望] ...` エントリで追記し、
本文に上記仕様 + `関連仕様: docs/plan_meter_scale_thinning.md` を必須で含める
(memory: gui_01 要望には plan 参照を付ける / 最終形態を伝える / v1・v2 段階分割しない)。

---

## (2) daw_01 = 変更なし (verify 済み)

### Mixer strip (`channel_fader_meter` 呼び出し)

`F:\dev\daw_01\daw_gui\src\view\mixer_strips.rs`:

- `LevelMeterStyle { scale: Some(MeterScale::default()), peak_readout: true, ..LevelMeterStyle::default() }`
  を構築 (L481-485)。
- `ui.channel_fader_meter(...)` に上記 `style` を渡す (L486-)。
- メーター高さ `fader_h` は `fader_top = y + 4.0` (L465)、
  `fader_bottom = rect.y + rect.h - pad - 12.0 - sends_band_h` (L466)、
  `fader_h = (fader_bottom - fader_top).max(20.0)` (L467) で決まる。strip rect が縮むと `fader_h` が縮み、
  それが widget の `rect.h` → `content.h` に伝播して目盛が詰まる = 本件の発生経路。
- group 幅は `group_w = FADER_W (18) + METER_GAP (2) + METER_SCALE_W (35) = 55` (L24/25/30/469)。

→ **`MeterScale::default()` を渡すだけ。daw_01 側のロジック変更は不要**。間引きは widget 内で `style.scale.curve`
   と `content` 高さから自動で行われる。daw_01 は何も足さない。

### arrangement_view (影響なし、verify 済み)

`F:\dev\daw_01\daw_gui\src\view\arrangement_view.rs` L218-222 は `MeterScale::default().db_to_frac(db)` を
**dB → 0-1 fraction の mapping にのみ**使用しており (track volume を arrangement の volume bar 用 fraction に
変換)、**ラベル描画 (`draw_meter_scale`) は一切呼んでいない**。よって本件の影響を受けない。

---

## (3) 実機検証手順

本件は **video preview ではないので `--smoke-test` の対象外**。histogram smoke test では検出できない
(unit test + 目視)。

1. gui_01 側で widget unit test を追加し `cargo test -p daw-ui` (gui_01 workspace) で green を確認。
2. gui_01 landing 後、daw_01 で `cargo build --workspace` (path 依存なので gui_01 の変更が取り込まれる)。
3. `cargo run -p daw_gui` (run_in_background、二重起動チェック必須) で起動。
4. **Mixer view を開き、ウィンドウ (またはミキサーペイン) の高さを段階的に縮める**:
   - 高さ大: 12 ラベル (6 / 0 / 6 / 12 / ... / 60) が全部出ていること。
   - 高さを縮める: 数字が**重ならず**、下から (粗い側から) 順に間引かれていくこと。上 (0 dB 付近) は最後まで
     細かく残ること。
   - 極端に縮めた状態: **0 ラベルだけは必ず残る**こと。0 dB 横線も残ること。
   - 残った label と tick line が**1:1 で対応**している (label の無い裸の tick が出ていない) こと。
5. commit 前に `/review` skill を実行 (memory: review before commit)。
6. commit 後に `cargo build --workspace --release` で green を確認し、`target/.release-build-failed` が
   無いことを確認 (CLAUDE.md 規約)。

---

## ビルド / verify note

- gui_01 の変更は path 依存 (`path = "../gui_01/crates/*"`) なので、daw_01 側は `cargo build --workspace` で
  自動的に新 widget を取り込む。protocol 型 (bincode) の変更は無いので `daw_audio.exe` / `daw_plugin_host.exe`
  の互換性問題は発生しない。
- daw_01 自体のコード変更は無い (= diff ゼロ)。よって daw_01 の本 plan は「要望提出 + landing 後の検証」が実体。
- gui_01 landing は rust-analyzer / cargo の non-exhaustive match 等では検知できない (シグネチャ不変・内部実装
  のみの変更) ため、gui_01_conversation.md 末尾の reply を Read して landing を確認してから検証に入る。
