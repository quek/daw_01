---
name: arch-review
description: |
  コードベース全体のアーキテクチャ監査。サブシステム別の並列分析 agent (6 レンズ) +
  arch-lint 機械検査 + サイズ budget (実コード行 / 関数長 / ネスト) で、構造問題 (SSoT 違反 / positional addressing /
  レイヤ違反 / god module / 同期経路の増殖 / RT 境界リスク) を洗い出し、優先度付き
  レポートを出す。「アーキテクチャレビュー」「構造的な問題を探して」「arch review」
  「定期監査」等で発動。大機能の landing 後・四半期ごとの実行を想定。分析のみ行い、
  コードの修正はしない (修正はレポートを受けてユーザーが指示する)。
argument-hint: "[重点サブシステム (省略可: 全体)]"
allowed-tools: Read, Grep, Glob, Bash(bash scripts/arch_lint.sh*), Bash(git log *), Bash(git diff *), Bash(python *), Bash(python3 *), Agent, Workflow
disallowed-tools: Edit, Write, NotebookEdit
---

# アーキテクチャ監査 (daw_01)

対象: $ARGUMENTS (未指定なら全体)。**分析のみ、編集禁止。**

前回の全体監査と確立された不変条件は `docs/plan_arch_refactor.md` と
CLAUDE.md「アーキテクチャ不変条件」。まず両方を読み、**既に決着した論点を再提案しない**。

## 手順

### 1. 機械検査

```bash
bash scripts/arch_lint.sh
```

baseline に無い新規違反 (exit 1) があれば、それ自体がレポートの先頭項目 (不変条件からの逸脱 = 最優先)。
baseline 済みの負債は末尾の `arch-lint: baseline N 件 (解消 K) / 新規 M 件` を推移として載せる。
加えてサイズ budget (実コード行 / 関数長 / ネスト) の推移を測る:

```bash
python scripts/loc_budget.py --report
```

`wc -l` で測らないこと。物理行はテスト module と doc comment を課金してしまい、
「テストを厚くすると分割を迫られる」逆インセンティブになる。

### 2. 並列サブシステム分析 (6 レンズ)

general-purpose agent を **並列で** 立てる (読み取り専用を明示。ultracode なら 1 本の Workflow で 6 レンズと §3 の反証役を回す)。各 agent への共通指示:
「アーキテクチャ課題 (小バグではなく構造問題) を file:line 証拠付き・
『なぜ痛むか』『理想の修正方向』・優先度・確信度付きで、確信の低いものも含めて見つけたものは全部。
絞り込みは後の段で別に行う。日本語で報告。
CLAUDE.md の不変条件と plan_arch_refactor.md の確立済み設計を前提とし、
それらへの**回帰**と**新規の構造問題**を区別して報告」。

| レンズ | 対象 | 固有の問い |
|---|---|---|
| GUI 状態 | daw_gui/src (state/ event*.rs handler/ view/) | 編集チョークポイント迂回は無いか。IPC event の direct-wrap (`AppEvent::Audio` / `AppEvent::Plugin`) と export gate の positive-default + block-list (`AppData::dispatch_app_event`) は保たれているか。state モジュール境界の腐敗 |
| モデル/protocol | common/src | Song の 3 役分離 (ドキュメント/wire/ファイル) は保たれているか。id addressing の穴。migration 層の健全性 |
| audio engine | daw_audio/src | RT 有界性。live/export の render 統一。値更新 vs topology の分離。off-thread swap idiom |
| plugin host | daw_plugin_host/src | device_id 一本化の維持。CLAP/VST3 の対称性 (片側だけの修正)。ProcessScaffold の共有維持。aliasing |
| UI ライブラリ境界 | ui/crates + daw_gui/src/widgets | core のドメイン知識ゼロ維持。mirror 型/翻訳層の再発。retained state の置き場一貫性 |
| プロセス間同期 | 横断 (protocol 送受信の全経路) | 同期経路の総数と正当性。SSoT 所有者表 (playhead/tempo/plugin state/meter)。crash/respawn/fingerprint の異常系 |

### 3. 反証

レポートに載せる指摘は、分析した agent とは別の反証役 (読み取り専用) に 1 件ずつ潰させ、潰せなかったものだけを採用する
(CLAUDE.md「まず調べる」)。反証役には file:line を読み直させ、同節の「潰すべき失敗パターン」と、既に決着した論点
(plan_arch_refactor.md・不変条件) の再提案でないかを確かめさせる。そのうえで「実バグ級」と「修正提案の根拠」になる
最重要のものは、自分でも file:line を Read して確かめてから載せる (agent 報告の鵜呑み禁止)。

### 4. レポート

- **総評** (骨格の健全性 1-2 文) → **実バグ級 (挙動に出る)** → **構造テーマ (再設計対象)** →
  **不変条件への回帰** → **健全でいじる必要のない部分** の順。
- 各項目: file:line 証拠 / なぜ痛むか (具体的な故障モード) / 理想の修正方向 / 推奨着手順。
- r.md には書かない (ユーザーが転記する)。修正はユーザー指示を待つ。
