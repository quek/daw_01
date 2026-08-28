# r.md #71 / #73 / #74 / #75 / #76 / #77 — 統合順と分担

6 項目の実装計画は 1 項目 1 ファイルに分かれている。**各計画は自分の項目のことしか書いていない**ので、
並列作業・統合順・ファイルの奪い合いはこの索引が正本。

| 項目 | 計画書 | 内容 |
|---|---|---|
| #71 | [plan_rmd_71_device_copy.md](plan_rmd_71_device_copy.md) | プラグインのコピー / 移動 (前提: device 帳簿の安定 id 化) |
| #73 | [plan_rmd_73_automation_curve.md](plan_rmd_73_automation_curve.md) | オートメーションカーブの操作系 |
| #74 | [plan_rmd_74_disclosure_glyph.md](plan_rmd_74_disclosure_glyph.md) | 開閉マークの向きと SSoT 化 |
| #75 | [plan_rmd_75_voicevox_phrase.md](plan_rmd_75_voicevox_phrase.md) | VOICEVOX 合成の塊クエリ + フレーズ合成 |
| #76 | [plan_rmd_76_loc_budget.md](plan_rmd_76_loc_budget.md) | god file budget の測り方 |
| #77 | [plan_rmd_77_arrangement_split.md](plan_rmd_77_arrangement_split.md) | `arrangement/run.rs` の分割 |

## 統合順

**第 1 波 (並列)**: #77 / #71 / #75 / #76
**第 2 波 (第 1 波が main へ入ってから)**: #73 → #74

### なぜこの順か

- **#73 と #74 は #77 の後**。3 つとも `daw_gui/src/widgets/arrangement/` を触るが、#77 は
  `run.rs` (2,699 行 1 関数) を 9 ファイルへ割る全面改稿で、同時に走らせると機械解決できない衝突になる
  ([[feedback_agent_bigfile_no_parallel_split]])。#73 / #74 の計画は **分割後のファイル構成**を前提に書いてある。
- **#74 は #71 の後でもある**。`daw_gui/src/view/mixer_strips.rs` と
  `daw_gui/src/view/track_inspector/` を両方が触る。#74 は 3 か所のグリフ複製を 1 関数へ畳む変更なので、
  #71 が同じファイルに入れる変更の上に載せるほうが安全。
- **#76 は Rust ソースを 1 行も触らない** (`scripts/` / `Makefile` / `.claude/skills/` / `CLAUDE.md` / `docs/`)。
  どの項目とも衝突しないが、`scripts/arch_lint_baseline.txt` だけは #71 も触るので、そこは行単位マージ前提。

### 第 1 波の重なり

| ファイル | #77 | #71 | #75 | #76 |
|---|---|---|---|---|
| `common/src/protocol.rs` | — | 変更 (SetSlotPlugin / RemoveTrack) | 変更 (合成メッセージ) | — |
| `scripts/arch_lint_baseline.txt` | — | 4 行削除 | — | 全面書き直し |
| `daw_gui/src/widgets/arrangement/**` | 全面 | run.rs の drop 抑止 1 か所 | — | — |
| その他 | 重複なし | 重複なし | 重複なし | 重複なし |

`protocol.rs` は #71 と #75 が別々の enum / variant を触るので行単位マージで解ける。
`arch_lint_baseline.txt` は #76 が指標そのものを作り直すので、**#71 の baseline 削除は #76 の新しい
baseline 形式に合わせて書き直す** (統合時に #76 を先に入れる)。
`arrangement/run.rs` は #71 が 1 か所だけ触る (ドラッグ中の drop フレームでトラック選択を抑止する行)。
#77 の分割でこの行は `header.rs` へ移るので、**#71 の統合時に移動先へ入れ直す**。

## 各 worktree の作業前チェック

1. `make fetch-ffmpeg` (herdr の worktree は `.worktreeinclude` を経由しないので `third_party/` が無い)
2. `cargo build -p daw_audio -p daw_plugin_host` (子プロセスのバイナリが要る)
3. 着手前に `make arch-lint` を 1 回走らせて出発点を記録する (main 時点: baseline 4 件 / 新規 0 件)

## 全 worktree 共通の禁止事項

- **`make test` を走らせない。** `daw_gui/tests/` の一部が daw_gui 本体を `--script` で起動し、
  audio device を開いて、ユーザーが開いているプロジェクトの再生を壊す。
  `make test-nolaunch` か `cargo test -p <crate> --test <name>` を使う。
- **daw_gui を起動しない。** 実機確認が要るときはユーザーへ事前に断る。
- コマンドを `&&` / `;` で連結しない。作業ディレクトリへ `cd` を前置しない。
- `r.md` を編集しない。
- commit はユーザーの sign-off を得てから。
