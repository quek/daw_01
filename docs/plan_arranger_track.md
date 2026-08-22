<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan: Arranger Track（曲のパート / セクション）— FIXME #53

アレンジビューに「曲のパート（Intro / Aメロ / サビ …）」を表す **能動アレンジャー** を実装する。
帯を動かす・並べ替えると、その範囲の全トラックの中身（clip + automation + tempo + 拍子 + key）が
**実際にその場で移動**し、曲の構成そのものを組み替えられる。

このドキュメントは `/grill-me`（2026-06-13）で確定した設計と、その根拠（各 DAW 公式マニュアルの一次情報）を残す。

> 注: `docs/FIXME.md` は編集禁止。本ファイルが #53 の唯一の設計 SSoT。

---

## 1. 採用モデルと根拠（決定木）

参照 DAW を公式マニュアルで調査した結果、**Studio One の Arranger Track** が「動かすと中身も動く」モデルの
最も完全な参照実装だった（破壊的移動・全トラック縦断・境界でクリップ分割・ripple・dual-delete・naming/color）。
これを採用する。

| # | 決定 | 採用 | 却下した代替 | 根拠 |
|---|------|------|------------|------|
| 1 | パッシブ vs 能動 | **能動**（動かすと中身も動く） | パッシブ・ラベル（REAPER region / Ableton locator） | ユーザー要件「曲の構成そのものを組み替える」 |
| 2 | 破壊的移動 vs 非破壊チェーン | **破壊的（タイムライン＝曲）**（Studio One / Logic） | 非破壊 play-order チェーン（Cubase Arranger Chain + Flatten） | 「見えているタイムライン＝曲」が単純で正直。保存＝聞こえる曲。仮想チェーン + bake 経路を持たない |
| 3 | スコープ | **全トラック縦断**（プロジェクト全体・単一レーン） | 単一トラック | 全 DAW 共通。「曲のパート」概念に必須 |
| 4 | 占有ルール | **隣接配置・重複なし・隙間可**（Studio One / Logic） | 完全タイル / 自由配置・重なり可（REAPER） | 重複禁止で並べ替え順が一意。無音への命名を強制しない |
| 5 | 何が同伴するか | **フルスコープ**（clip + automation + tempo + 拍子 + key/音階） | clip のみ（Logic は tempo を明記せず） | 部分スコープは歌声とテンポ/キーを静かにデシンクさせる。Studio One は Events/Parts/Markers/tempo/automation 全部 |
| 6 | 境界をまたぐクリップ | **境界で分割**（Studio One） | クリップ丸ごと移動（Logic） | 「帯の範囲＝厳密にその時間」と一貫。既存の content+offset 参照に乗る。丸ごと移動の所有曖昧さを回避 |
| 7 | 衝突時 | **ripple-insert**（後続を押し出す/詰める、上書きしない） | overwrite | 重複なし + 全リフローと一貫 |
| 8 | 削除 | **2 種**（帯のみ温存 / 範囲ごと詰め＝破壊的・要確認） | 単一削除 | 数時間ぶんの合成歌声を誤キーで破壊しない安全策（Studio One: Backspace vs Delete Range） |

### 一次情報引用（調査 2026-06-13）

- **Studio One Arranger Track（採用モデル）**
  - PreSonus Reference Manual "Arranger Track": これらの操作は「all Tracks ... including all Events, Parts, Markers, tempo changes, and automation data」に対して行われる
    <https://s1manual.presonus.com/Content/Arranging_Topics/ArrangerTrack.htm>
  - MusicRadar: 「if you move a section in the Arranger Track, Studio One will cut through all the clips across all the tracks and take them with it」「The Arranger Track ripples intelligently to take up any slack」
    <https://www.musicradar.com/how-to/how-to-use-studio-ones-arranger-track-to-speed-up-your-workflow>
  - Sound on Sound: double-click で bar-size セクションを自動命名（"Intro"）/ rename + color + resize / Backspace は帯のみ削除・Delete Range は時間+内容削除して詰める
    <https://www.soundonsound.com/node/4923606>
- **Logic Pro Arrangement track（二次参照・移動は丸ごと、tempo 未明記）**
  - 「When you move or copy an arrangement marker, all of the regions in that section ... are moved or copied, including ... automation points」
    <https://support.apple.com/guide/logicpro/edit-arrangement-markers-lgcpf7c0a3d7/mac>
- **Cubase Arranger Chain（却下した非破壊モデル）**
  - 非破壊 play-order。Flatten で初めて「events and parts ... reordered, repeated, resized, moved and/or deleted」
    <https://archive.steinberg.help/cubase_pro/v12/en/cubase_nuendo/topics/tracks_about/tracks_about_arranger_track_c.html>
- **REAPER Regions（却下したパッシブモデル）** — region は内容を動かさないラベル
    <https://www.soundonsound.com/techniques/power-arranging-reaper>

---

## 2. データモデル（`common/src/model.rs`）

既存 `Song`（`model.rs:182`）は `loop_start_beat` / `loop_end_beat`（既存ループ領域）、
`song_lanes: Vec<AutomationLane>`（SongTempo / SongTimeSigNumerator）、`scale_changes: Vec<ScaleChange>`、
`next_track_id` 等の id allocator を既に持つ。同じパターンで Section を追加する。

```rust
/// 曲のパート（Intro / Aメロ / サビ …）。プロジェクト全体・全トラックを縦断する時間レンジ。
/// 位置が並び順の SSoT（別途 order index は持たない — start_beat 昇順 = 演奏順）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Section {
    pub id: u32,              // Song 内で安定。next_section_id で採番
    pub name: String,         // 既定は Intro/Aメロ/サビ… を循環、自由 rename
    pub color: [f32; 3],      // 帯の色（既存 track color の RGB 表現に合わせる）
    pub start_beat: f64,
    pub len_beats: f64,       // end = start + len。重複なし（昇順に非交差）
}
```

`Song` への追加フィールド:

```rust
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub sections: Vec<Section>,
#[serde(default)]
pub next_section_id: u32,
```

- `Section` は IPC 境界（`MainToChild::LoadSong` 経由で daw_audio へ）を越えるので
  `#[derive(bincode::Encode, bincode::Decode)]` 必須。フィールド追加後は **`cargo build --workspace`**
  （daw_audio.exe を古い protocol のまま残すと LoadSong で decode 失敗 → audio engine exit）。
- 不変条件: `sections` は `start_beat` 昇順、互いに非交差（重複なし）。隙間は許容。
  編集後に必ず正規化（ソート + 重複クランプ）するヘルパ `Song::normalize_sections()` を 1 本用意。

---

## 3. 編集セマンティクス（破壊的アルゴリズム）

すべて **1 つの undoable コマンド** として実装し、後述の「同時に動かすもの」を原子的に処理する。

### 3.1 同時に動く対象（フルスコープ）

時間レンジ `[a, b)` を「持ち上げる/詰める/挿入する」とき、全トラックにわたって以下を一括処理:

1. **clip**（audio / MIDI / 歌声 / video / image — トラック上のすべて）
2. **per-track automation**（`Track` の automation lane clips）
3. **song-level automation**（`song_lanes` の clips: SongTempo / 拍子）
4. **`scale_changes`**（歌唱キー/音階）
5. **既存ループ領域 `loop_start/end_beat`・playhead・`length_beats`** をリフロー量に合わせて整合シフト
6. 歌声クリップの **絶対時間アンカー（REST_FRAMES ≈107ms リードイン）** をシフト後も保持

### 3.2 プリミティブ: cut-time / insert-time

破壊的移動はこの 2 つの合成で表現する（Ableton "...Time" / Studio One ripple と同型）:

- **split_at(t)**: 時刻 `t` をまたぐ全クリップを分割。歌声/audio は **同一 content の別 offset を指す 2 クリップ**に分ける
  （既存の content+offset 参照パターン。再合成・キャッシュ複製はしない）。第 2 断片の再生開始は
  リードイン（REST_FRAMES）を二重適用/欠落しないよう再計算。
- **lift(a, b)**: `split_at(a)`, `split_at(b)` 後、`[a,b)` 内の全対象（3.1 の 1–4）を取り出し、時刻を `-a` 正規化して保持。
- **close(a, b)**: `[a,b)` を削除し、`>= b` の全対象を `-(b-a)` シフト（ripple 詰め）。
- **open(dest, len)**: `>= dest` の全対象を `+len` シフト（ripple 挿入）して空きを作る。
- **drop(dest, payload)**: `lift` した payload を `dest` に再配置（時刻を `+dest`）。

### 3.3 操作

- **移動 / 並べ替え（レーン上で帯をドラッグ）**:
  `payload = lift(a,b)` → `close(a,b)` → `open(dest,len)` → `drop(dest,payload)`。
  Section 自身の `start_beat` も更新。`normalize_sections()`。並び順は start_beat で自動決定（順序 index 不要）。
- **複製（Alt-ドラッグ）**: `lift` の代わりに **copy**（元を残す）→ `open(dest,len)` → `drop`。新 Section を採番。
- **リサイズ（端ドラッグ）**: Section の `start/len` のみ変更（内容は動かさない＝被覆範囲の再定義）。
  隣接帯に食い込む場合はクランプ（重複禁止の不変条件を守る）。
- **削除・帯のみ（Backspace 相当）**: `sections` から当該 Section を除くだけ。内容は温存。
- **削除・範囲ごと（Delete Range 相当、破壊的・確認ダイアログ）**: `close(a,b)` で時間と内容を削除し詰める。

### 3.4 ドラッグの所有（Studio One の decouple gotcha 対策）

- **アレンジャーレーン上**のドラッグ = Section + 中身が動く（3.3）。
- **トラック領域**で clip を直接ドラッグ = その clip だけ動く（Section は据え置き、既存挙動のまま）。
- この境界を実装・ドキュメントで明示する（曖昧だと「セクションと中身がデシンク」する）。

---

## 4. 歌唱 DAW 固有の扱い

- **通常は再合成不要**: フルスコープ移動でテンポ/キーが同伴するので、移動先でも歌声は同じ音楽文脈に置かれる。
- **再合成が要る唯一のケース**: 分割断片が**異なるテンポ/キー領域下に着地**したとき。
  → 歌声合成キャッシュの **cache key に tempo + key を含める**。relocate 後にキー不一致なら自動 invalidate → 再合成。
  キャッシュ一致なら **参照のまま**（再 render しない）。
- **リードイン保持**: `split_at` で分割した歌声断片は、先頭リードイン（REST_FRAMES ≈107ms、
  `project_voicevox_leadin_offset` 参照）を片側で二重適用・他方で欠落させないよう再生開始を再計算。
- ripple（cut/insert-time）は **絶対時間アンカーを持つ歌声 offset** も整合シフトする（3.1-5/6）。

---

## 5. UI / レーン配置

現状アレンジビュー（`daw_gui/src/view/arrangement_view.rs`）: `TOOLBAR_H=24`（snap ツールバー）+ `RULER_H=20`（拍ルーラー）
の下にトラック。ここに **アレンジャーレーン**（高さ ~22px）を **ルーラー直下・トラック上** に 1 本追加する。

- 帯 = 色付き矩形 + セクション名ラベル。ジャンプ移動はクリック。「このセクションをループ」は
  **既存ループ領域（`loop_start/end_beat`）を駆動**（ループの SSoT を二重化しない）。
- 作成: レーンをダブルクリック → 既定長（1 bar）の帯を自動命名で生成、または範囲ドラッグで描画。
- 描画/ヒットテスト/ドラッグは gui_01 の `ruler_ops`（`draw_loop_band` / `loop_band_hit_kind` /
  `LoopDragSession`）を一般化して再利用する（loop band と同じ基盤）。

### 5.1 daw_01 ↔ gui_01 の分担

- **gui_01（arrangement widget）**: アレンジャーレーンの描画 + 操作。`ArrangementView` に
  `sections: &[SectionView]`（id/name/color/start/len）を追加して渡す。操作は既存の
  `ArrangementEditRequest` 列挙に追加して emit する（loop band が `SetLoopRange` を emit するのと同型）:
  `CreateSection { start, len }` / `MoveSection { id, dest_start }` / `ResizeSection { id, start, len }` /
  `RenameSection { id, name }` / `RecolorSection { id, color }` / `DeleteSection { id }` /
  `DeleteSectionRange { id }` / `LoopSection { id }`。
- **daw_01（AppData）**: 上記リクエストを受け、§3 の破壊的アルゴリズム（split / ripple / フルスコープ移動 /
  歌声キャッシュ整合）を Song に適用し、`sync_song_to_plugin_host()` + audio へ再送。
  破壊的ロジックはモデル/クリップ/キャッシュに触るので **必ず daw_01 側**に置く（gui_01 は座標→リクエストのみ）。

### 5.2 gui_01 要望（別途 `docs/gui_01_conversation.md` に提出）

最終形態（v 分割しない）を記述し、`関連仕様: docs/plan_arranger_track.md` を付ける:
ルーラー直下のアレンジャーレーン widget（色付き名前付き帯の描画、ダブルクリック生成、
移動/リサイズ/複製ドラッグ、ヒットテスト、上記 `ArrangementEditRequest` の emit、loop band と共存）。

---

## 6. IPC

- `Section` は `Song` の一部として `LoadSong` で daw_audio へ渡る。bincode derive + `cargo build --workspace`。
- 破壊的移動の結果は最終的に **clip 位置の変化**として現れるので、audio engine 側は通常の
  Song 差し替えで追従する（Section 自体を audio engine が解釈する必要はない＝再生は clip 位置が真実）。
- plugin host へは `sync_song_to_plugin_host()` 経由（slot 不変なら追加対応不要）。

---

## 7. 実装フェーズ

1. **モデル**: `Section` 型 + `Song.sections` / `next_section_id` + `normalize_sections()` + bincode/serde。`cargo build --workspace`。
2. **プリミティブ**: `split_at` / `lift` / `close` / `open` / `drop`（フルスコープ: clip + 全 automation + scale_changes + loop/playhead/length 整合）。ユニットテスト（ripple 量・分割 offset・歌声リードイン）。
3. **歌声キャッシュ**: cache key に tempo+key を追加、relocate 時の invalidate/参照判定。
4. **AppData ハンドラ**: `ArrangementEditRequest` の Section 系を受けて §3 を適用。
5. **gui_01 要望提出** → landing 後にレーン widget を wire（`ArrangementView.sections`、edit request 配線）。
6. **作成/命名/色/削除 2 種/ループ駆動/ジャンプ** の UX 仕上げ。
7. **検証**（§9）。

> 待ちの間（gui_01 landing 前）も 1–4 は先行実装する（`feedback_progress_while_waiting_gui01`）。

---

## 8. 既知の落とし穴（調査由来）

- **decouple**: レーンドラッグ＝内容同伴 / クリップ直ドラッグ＝クリップのみ。混同するとデシンク（§3.4）。
- **歌声分割の offset**: 分割 2 断片が同一キャッシュの正しい offset を指し、リードインを二重/欠落させない（§4）。
- **ripple は右側全体の時間シフト**: ループ領域・playhead・歌声絶対 offset・song length を同一コマンド内で整合シフト（§3.1）。
- **tempo を置き去りにしない**: Logic はスコープに tempo を含めない。daw_01 は Studio One の全スコープを採る（§1 #5）。
- **dual-delete は安全機構**: 帯のみ削除 と 範囲ごと削除 を別コマンドに（§3.3）。後者は確認必須。
- **重複禁止を editor で強制**: 後から nesting バグを発見しないよう、編集のたび `normalize_sections()`。
- **protocol bump**: `Section` 追加は `cargo build --workspace`（`feedback_workspace_build_for_protocol_changes`）。

---

## 9. 検証

- ユニット: ripple 量、分割 offset、フルスコープ（automation/tempo/scale が一緒に動く）、ループ/playhead/length 整合。
- 実機（要 VOICEVOX server）:
  - サビをイントロ前に並べ替え → 全トラックの clip・テンポ・キー・歌声が一緒に動き、鳴り方が変わらない。
  - 境界をまたぐ歌声ロングトーンを分割移動 → 断片が正しい音/位置で鳴る（再合成は文脈変化時のみ）。
  - 帯のみ削除（内容温存）／範囲ごと削除（詰め）の区別。
  - 「セクションをループ」が既存ループ領域を駆動。
- `feedback_verify_actual_content`: 静止 1 枚でなく、移動後に**全トラックが正しく鳴る**ことを通しで確認。
