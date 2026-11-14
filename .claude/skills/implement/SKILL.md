---
name: implement
description: |
  機能追加・バグ修正の入口。規範は CLAUDE.md が正本で、ここは場面ごとの参照先と commit 前の順序だけを持つ。
  「実装して」「追加して」「修正して」「対応して」「機能を作って」「バグを直して」等、
  コード変更を伴う指示で発動。
argument-hint: "[実装したい機能の説明]"
allowed-tools: Read, Grep, Glob, Edit, Write, Bash(make build), Bash(make clippy), Bash(make arch-lint), Bash(cargo check -p *), Bash(cargo build -p *), Bash(cargo clippy -p *), Bash(cargo test -p *), Bash(python scripts/loc_budget.py *), Bash(git add *), Bash(git commit *), Bash(git status *), Bash(git diff *), Agent, Skill, Workflow
---

# 実装 (daw_01)

$ARGUMENTS を実装する。進め方・止まって聞く場面・テストの範囲と回し方・RT / FFI / アーキテクチャ不変条件は
root の `CLAUDE.md` に従う (ここに再掲しない)。最小の動く形を先に作り、動かして確かめてから広げる。

## 場面ごとの参照先

| 場面 | 読むもの |
|---|---|
| バグ修正 | 実データ (ログ) で原因を確定してから直す。キー / ボタン / イベントは `/debug-gui`、daw-ui の widget 内部は `/debug-ui`、プラグインのエディタ窓は `/debug-plugin-gui`、フリーズは memory `reference_freeze_debugging` |
| CLAP / VST3 / MIDI の仕様に依る、「○○みたいに」と参照製品が指定された、DAW として一般的な機能、daw-ui / VOICEVOX API の使い方が不明 | `/research-similar-impl` (ホスト側に CLAP 拡張を足す手順は同 skill の `references.md`) |
| daw_gui の view を足す・変える | `daw_gui/CLAUDE.md` (配置・操作性)。着手前に UI の見せ方を確定させる (設計判断が多ければ `/grill-me`) |
| daw-ui (`ui/`) の widget が足りない | `ui/CLAUDE.md`。daw_gui 側に interim の自前 widget を作らず、同じ session で `ui/` を直す |
| 大きめの計画 | `docs/plan_<feature>.md` |

## commit まで

1. 変更に関係する test target を名指しで回し、`make clippy` / `make arch-lint` を通す
   (実機で往復している間は挟まず、修正が固まってから 1 回)。
2. 実行 exe を作り直す (子プロセスを変えたら `make build`)。video preview / texture に触れたら `--smoke-test`。
3. `/review` を回し、指摘を直す。
4. `/verify-app` で自分で起動して確かめ、ユーザーの sign-off を得る。
5. `git add <パス全列挙>` → 日本語メッセージで commit。
