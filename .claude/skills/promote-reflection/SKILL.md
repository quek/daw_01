---
name: promote-reflection
description: |
  AHE backlog (~/.claude/projects/F--dev-daw-01/ahe_backlog.md) の OPEN 行を
  実際の artifact に昇格させ、行を終端状態に倒すワークフロー。
  SessionStart フックが「Required Action: AHE backlog の未処理パターンを triage」を
  出したとき、または「backlog を処理して」「reflection を昇格」「この検出パターンを skill/hook に」
  等のとき発動。検出された再発フリクションを guard (guards.jsonl) / skill / command / memory に変換するか、
  hook は user 承認キューに回すか、dismiss する。memory に書いて終わりにしない (= 旧ループの欠陥) 。
allowed-tools: Read, Write, Edit, Glob, Grep
---

# AHE reflection 昇格ワークフロー

session metrics から検出された再発パターン (backlog の 1 行) を、**実際にハーネスを変える
artifact** に昇格させる。旧ループは「memory に save / discard」しか終端が無く、skill/hook が
全く作られなかった。この skill は backlog 行の `target` に応じて artifact を作り、最後に
**行の status を必ず倒す** (= やった感と行クローズが同一操作)。

## backlog の場所と構造

- file: `~/.claude/projects/F--dev-daw-01/ahe_backlog.md` (per-project user dir、全 worktree 共有、git 外)
- patterns テーブル列: `id | status | sessions | target | first-seen | last-seen | last-session | pattern | notes`
- status: `open` → `done` | `dismissed` | `needs-user`
- **insert は reflect.py が所有**。status / target / notes は人間とこの skill が所有。
  `done` / `dismissed` は終端 = reflect.py は二度と再浮上させない。
- `sessions >= 3` は escalated (何度も踏んでいる)。優先的に promote する。

## 手順

1. **対象行を読む**。`id` と `pattern` と `target` を確認。`target` は reflect.py の既定提案に
   過ぎない。**実態に合わなければ正しい target に変える** (例: read-hotspot の既定は memory だが、
   巨大ファイルなら「分割」や「ナビ skill」が正解のこともある)。
2. **重複確認**。既存 skill (`.claude/skills/`) / memory (`MEMORY.md`) / command (`.claude/commands/`) に
   同等物が無いか grep。あれば dismiss (notes に既存名)。
3. **target 別に昇格** (下記)。
4. **行を終端させる** (必須)。`ahe_backlog.md` を Edit し、その行の `status` セルを
   `done` / `dismissed` / `needs-user` に書き換え、必要なら `notes` セルに 1 文。
   - テーブルを壊さない: セル区切り `|` の数を変えない。**notes に `|` を入れない**。

### target = guard  (← 機械化できる再発の主経路。settings.json 不要・承認不要)
- 「同じ機械的ミスを繰り返す」(= tool 入力の正規表現で表現できる) なら、**guard registry に 1 行追記する**
  だけで能動的強制力になる。`guard_engine.py` は settings.json に登録済なので、ルール追加に
  hook 登録編集 (classifier ブロック) は不要 = 自分で完結できる。
- file: `.claude/guards.jsonl` (1 行 1 JSON ルール、**リポジトリ追跡下**。2026-08-22 に user dir から
  移設 — 追跡外に置いていたせいでレジストリが丸ごと消えた。CLAUDE.md「なぜ追跡下なのか」参照)。
  warn→block の自動昇格状態だけが git 外の overlay (`<state>/guard_state.json`) にある。
- ルール形:
  `{"id":<slug>,"source":<feedback メモリ slug>,"tool":["Bash"]|["Edit","Write","MultiEdit"],`
  `"field":"command"|"text"|"file_path","file_glob":<任意>,"all":[<正規表現…全 match>],`
  `"none":[<任意・どれか match で抑制>],"action":"warn"|"block","msg":<日本語の是正文>}`
  - `all` の正規表現はバックスラッシュを **JSON で 2 重** に (`\\s`)。追記後に
    `python -c "import json;[json.loads(l) for l in open(PATH,encoding='utf-8') if l.strip() and not l.startswith('#')]"`
    で全行パースを必ず確認 (printf で追記するとバックスラッシュが潰れて不正 JSON になる前科あり → Write/Edit で書く)。
  - `block` は誤検知で tool 呼び出しを取り消すので **高精度 (複数トークン / アンカー) のときだけ**。
    曖昧なら `warn`。warn は 3 つの異なる session で発火すると reflect.py が自動で block に昇格する。
  - **昇格したら困る warn には `"escalate": false` を必ず付け、理由を直前のコメント行に書く。**
    substring レベルのマッチャ (任意の散文に当たるもの / 抑止条件を満たすと別ガードに抵触するもの /
    sanctioned な正規手順まで巻き込むもの) は block にすると正当な作業を取り消す。既存 12 件の
    判断理由が `.claude/guards.jsonl` のコメントにあるので、書き方はそれに倣う。
  - 既存 guard と重複しないか確認 (同 `source` / 同パターン)。
- 検証: `echo '{"tool_name":"Bash","tool_input":{"command":"…"}}' | python scripts/guard_engine.py` で
  発火 (warn=stdout / block=exit2) を目視。
- 行 status を `done`、notes に guard id。

### target = memory
- 一般化できる learning を `~/.claude/projects/F--dev-daw-01/memory/<type>_<slug>.md` に書く
  (frontmatter は CLAUDE.md memory 規約。feedback/project なら **Why** と **How to apply**)。
- `MEMORY.md` に 1 行 index 追加。
- 行 status を `done`、notes に作成した memory 名。

### target = skill
- `.claude/skills/<name>/SKILL.md` を新設 (frontmatter: name / description / allowed-tools)。
  既存 skill (verify-app 等) の体裁を踏襲。
- 行 status を `done`、notes に skill 名。

### target = command
- `.claude/commands/<name>.md` を新設 (無ければディレクトリも作る)。繰り返し叩く Bash 列を
  1 コマンド化する用途。
- 行 status を `done`、notes に command 名。

### target = hook  (← settings.json 直編集は classifier ブロック)
- **hook 登録ファイルは自分で編集しない** (classifier がブロックする)。代わりに:
  1. hook ロジック script (`scripts/<name>.ps1` 等) は書いてよい (テスト込み)。
  2. `ahe_backlog.md` の `## hook requests (awaiting your approval)` 節に、user が貼るだけの
     **ready-to-paste 設定スニペット** (event / matcher / command) を id 付きで追記。
  3. 行 status を `needs-user`。
  4. user に「hook N 件、承認(貼り付け)お願いします」と 1 メッセージで依頼。
- user が適用したら (確認後) 行を `done` に倒す。

### dismiss
- 単発ノイズ・既存でカバー済み・誤検出なら、行 status を `dismissed`、notes に理由 1 文。
- 以後 reflect.py は同 id を再浮上させない。

## 原則
- **memory で終わらせない**。target が guard/skill/hook/command なら必ずその artifact を作る (または
  新規 hook 種別なら承認キューに載せる)。memory は target=memory のときだけ。
- **機械化できる再発 (tool 入力パターン) は guard を最優先**。「振る舞いで直す」と言って dismiss するのは
  旧ループの欠陥そのもの (例: ahe_backlog の cd-prefix 再発は guard 経路が無く dismiss され再発し続けた)。
  能動ガードにすれば物理的に再発できなくなる。
- 1 行 = 1 終端。OPEN のまま放置しない (放置すると毎 session escalated で再提示される)。
- 関連 memory: [[feedback_verify_env_var_before_use]] (hook script を書くときの破壊操作ガード)、
  CLAUDE.md「Reflection の確認」節、`docs/plan_ahe.md`。
