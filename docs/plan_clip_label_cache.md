<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan_clip_label_cache — アレンジ/ピアノロールの再描画キャッシュキーを描画内容から導出する

FIXME #10。「共有クリップをリネームできません」。実態は、F2 / 右クリック「Rename」での
rename 操作自体は通り、共有リンク全部の SSoT (`Song.clip_content_names`) にも正しく
書かれるが、**アレンジ上のクリップ表示が更新されない**（再起動してプロジェクトを開き
直すと反映済みの名前が表示される、というユーザー報告）。

## 現状 (2026-06-09)

アレンジ widget は描画を粗粒度キャッシュしており、その無効化キー `data_generation` を
daw_01 が手書きハッシュで組み立てて `ArrangementView.data_generation` に渡す
([arrangement_view.rs:374-392](F:/dev/daw_01/daw_gui/src/view/arrangement_view.rs))。
ハッシュの因子は **トラック数・各トラックの (index, id, clip 数, トラック名長, volume)**
のみで、**クリップ名 (`content_name`) が因子に入っていない**。

rename は `commit_rename_clip` → `Song::set_content_name`
([app.rs:6959-6991](F:/dev/daw_01/daw_gui/src/app.rs)) でモデルを更新し `is_dirty` も
立てるが、`data_generation` が変わらないため widget の内部キャッシュは古いラベルのまま
になる。再起動時はキャッシュが空から再構築され rename 済みモデルから描かれるため、
「再起動で直る」症状になる。

同型の手書きキーがピアノロールにもある: `pianoroll_notes_generation` は **12 箇所**で
手動 `+= 1` され
([app.rs:2191,2524,10037,12546-12821](F:/dev/daw_01/daw_gui/src/app.rs))、
`PianorollView.notes_generation` に渡る
([piano_roll_view.rs:133](F:/dev/daw_01/daw_gui/src/view/piano_roll_view.rs))。新しい
ノート編集経路を足して bump を書き忘れれば同じ「変更が反映されない」バグになる
（顕在化していないだけ）。

## 確定仕様 (grill-me 2026-06-09)

**キャッシュ無効化キーを「手書きの因子 allowlist」から「描画に渡す内容そのものの
ダイジェスト」へ作り替える**。内容が変われば必ず無効化し、変わらなければキャッシュを
維持する。クリップ名・色・位置・長さ・ノート等の項目の追加忘れが原理的に起きない
correct-by-construction なキー生成にする。アレンジとピアノロール **両方**を対象とし、
この種の「変更が画面に出ない」バグを根絶する。

理由: daw_01 には単一の song-revision カウンタ（mutation チョークポイント）が存在せず、
編集サイトは各所で `is_dirty = true` を立てるだけ。グローバルカウンタ案も結局あちこちで
bump し忘れる（同じ欠陥を別の場所へ移すだけ）。**内容から導出する**形だけが、散在する
mutation サイト数に関係なく構造的に正しい。

| # | 面 | 修正 | 担当 |
|---|---|---|---|
| 1 | arrangement | `data_generation` を、widget へ渡す `tracks`（解決済み `clip_display_label` を含む）+ 描画に効く派生値のダイジェストから算出する。クリップ名・色・start/length・ノート要約など「描かれる内容」を漏れなく覆う | **daw_01**（本 plan） |
| 2 | piano roll | `pianoroll_notes_generation` の 12 箇所手動 bump を廃し、widget へ渡すノート列のダイジェストを `notes_generation` に渡す | **daw_01**（本 plan） |

実装方針:
- ダイジェストは「毎フレーム widget 入力を組み立てる既存ループ」に畳み込む（`tracks` /
  ノート列は既に毎フレーム構築している → 同じ O(描画量) で hash 可能）。
- float（位置・色・volume）は `to_bits()`、文字列は内容を `Hasher` に食わせる。既存
  `data_generation` が拾っていた構造因子（track 並び順・volume）も引き続き覆う。
- ドラッグ / 再生ヘッド / 選択など「描画 transform / 高頻度 interaction」は従来どおり
  キーに含めない（widget の view transform 側で処理。`data_generation` は内容無効化専用
  という既存の役割分担を維持し、ドラッグ中のキャッシュ温存を壊さない）。

## 補足: 歌唱クリップのリネーム表示は別件

`clip_display_label` はテキスト本文 → ノート歌詞 → `content_name` の順で解決する
([arrangement_view.rs:1811-1819](F:/dev/daw_01/daw_gui/src/view/arrangement_view.rs))。
歌詞を持つ歌唱クリップは歌詞が優先されるため、本キャッシュ修正後も rename 名は表示
されない。これは FIXME #10（キャッシュ無効化）とは別の優先順位設計の話で、**本 plan の
範囲外**（歌唱クリップにも明示 rename 名を出したくなったら別途検討）。

## 受け入れ基準

- アレンジ上でクリップを F2 / 右クリック Rename して確定すると、**再起動せずに**その場で
  新しい名前が表示される（共有リンクなら全インスタンス同時に）。
- ピアノロールでノートを編集すると、`pianoroll_notes_generation` の手動 bump 無しで即座に
  描画へ反映される（手動 bump 撤去後も stale 表示が出ない）。
- ドラッグ / スクロール / 再生中のキャッシュ温存（パフォーマンス）が従来どおり保たれる。

## 非範囲

- `clip_display_label` の優先順位（本文 / 歌詞 vs 明示名）の変更（上記補足）。
- ミキサー（キャッシュ無しの毎フレームライブ描画なので対象外）。
