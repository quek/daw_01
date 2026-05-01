---
name: implement
description: |
  機能追加・バグ修正のワークフロー。類似実装調査 → 要件整理 → テスト → 実装 → 確認 を
  一貫して行う。「実装して」「追加して」「修正して」「対応して」「機能を作って」「バグを直して」等、
  コード変更を伴う指示で発動。
argument-hint: "[実装したい機能の説明]"
allowed-tools: Read, Grep, Glob, Edit, Write, Bash(cargo test *), Bash(cargo build *), Bash(cargo clippy *), Bash(cargo run *), Bash(cargo bench *), Bash(cargo check *), Bash(git add *), Bash(git commit *), Bash(git status *), Bash(git diff *), Agent, Skill
---

# 機能実装ワークフロー (gui_01)

$ARGUMENTS を実装する。

調査 → 要件整理 → テスト → 実装 → 確認 の順で進める。
テストはリグレッション防止が目的、可能な限り高いレイヤーで書く。

## 手順

### 1. バグ修正の場合: ログ / トレースで原因を特定する

バグ修正の場合、**推測で修正するな。** コードレビューだけで原因を断定せず、実際の動作を観測する。

1. **疑わしい箇所にトレースを仕込む**: 関数の入口・出口、条件分岐の通過、`PointerFrame` の値、`Edit` の発行有無
2. **GUI イベントは `tracing::info!` か `eprintln!` を該当層に**: クリック・キー入力・focus 変化は可視フィードバックが無いと動いたか判別不能
3. **実行して再現**: `cargo run --bin <example>` で再現させ、ログを確認
4. **原因が確定してから修正する**: 「可能性がある」で修正しない

詳細な切り分け手順は [/debug-ui](../debug-ui/SKILL.md) を参照。

バグ修正でない機能追加の場合はこのステップをスキップしてよい。

### 2. 類似実装の調査

**推測で実装するな。** まず正しい振る舞い・既存パターンを調査する。

以下のいずれかに該当する場合、`/research-similar-impl` スキルを呼んで Rust GUI 生態系
(iced / egui / druid / xilem / floem / Slint 等) や winit / wgpu / glyphon / taffy / cosmic-text の実装を調査する:

| 該当条件 | 例 |
|---|---|
| 「○○みたいに」と参考プロダクトが指定されている | 「iced のように Edit を msg にせず…」(これは本プロジェクトでは禁止だが対比として) |
| 一般的な GUI ウィジェットを作る | スクロールバー、ツリービュー、メニュー、ダイアログ |
| 入力周りの仕様が不明 | IME、focus 移動、ドラッグ & ドロップ、複数選択 |
| 描画 / レイアウト挙動が wgpu / taffy のバージョン依存 | flex の wrap、scissor 矩形、SDF テキスト |
| 波形・大量描画の最適化 | LOD ピラミッド、scenegraph、heavy() キャッシュ |

調査で以下を明らかにする:
- **類似プロダクトの実際の振る舞い**
- **エッジケースの扱い** (空入力、リサイズ中、focus 喪失、IME 中断など)
- **実装上の設計判断** (アルゴリズム・データ構造・テスト方針)

該当しない場合 (内部リファクタリング、単純なバグ修正、自明な API 追加) はスキップしてよい。

### 3. 要件の整理

調査結果と既存コード (Read / Grep で確認) をもとに要件を整理する。

| 観点 | 問いかけ |
|---|---|
| 正常系 | 基本入力で何が起きるべきか? Edit が出るか、displayed_value はどう変わるか |
| エッジケース | 空 widget、focus 無し、IME preedit 中、リサイズ直後、Alt-Tab 復帰直後、modifier 押しっぱなし |
| no-Clone 制約 | 新しい API シグネチャに `Clone` / `PartialEq` / `Hash` / `Default` の境界が紛れていないか |
| derive マクロ | Lens 等の derive を新規導入していないか |
| 差分検出 | scenegraph 入る前提なら widget ID + プリミティブ末端値の hash で完結するか |
| 既存機能との相互作用 | 他の widget の focus / IME / drag を壊さないか |
| 類似プロダクトとの一致 | 調査した振る舞いをカバーしているか |

**要件一覧をユーザーに提示して過不足を確認** → 承認を得てから次のステップ。

### 4. 統合テストの作成

承認された要件をもとに統合テストを書く。

#### テストをスキップしてよいケース

以下のすべてに該当する場合、テスト作成をスキップして実装に進んでよい:
- example の見た目調整・配置変更が主で、自動テストが困難
- 一度ビルド・実行すれば正しさが目視できる (mixer / waveform_validation で動かす類)
- 既存ロジック (state、Edit 発行条件、layout 計算) に変更が無い

#### テストのレイヤー

可能な限り高いレイヤーでテスト。上から順に検討。

| レイヤー | 方法 | 例 |
|---|---|---|
| **UiHost::frame 経由** | `UiHost::frame` を直接呼んで `PointerFrame` / `KeyEvent` を流し、Edit の発行と Model 状態を検証 | button click、fader drag、checkbox toggle、focus 取得 / blur |
| **widget state** | `Ui::widget_state::<S>(wid)` の振る舞いを検証 | drag_anchor の更新、IME preedit の保持 |
| **純粋ロジック** | LayoutPass の compute、LOD ピラミッド構築、ヒットテスト | flex_grow の比例配分、min/max LOD 切替判定 |

UiHost::frame テストの様式は `crates/ui/src/widgets/fader.rs::tests` (Phase 4d) や `crates/ui/src/ui.rs::tests` (button armed-state) が参考。

#### no-Clone 制約の回帰防止

新しい widget / API を追加したら `crates/ui/tests/ui/pass/basic.rs` (trybuild) に呼び出しを足し、
**`Clone` / `PartialEq` / `Hash` / `Default` を実装していない Model でコンパイルできること** を CI 固定する。

```bash
cargo test -p daw-ui-core --test no_clone_required
```

#### 性能テスト (criterion)

LOD / 大量描画 / hit-test など性能が問題になり得る箇所は `crates/ui/benches/` に追加。N=1, 8, 16, 64, 128 のスケール検証 (M2 波形ベンチ参照、`feedback: project_multi_waveform`) 。

#### テスト設計のガイドライン

- **1 テスト = 1 つのユーザシナリオ**
- 期待値は `assert_eq!` で具体値、`> 0` や `starts_with` で誤魔化さない
- 単純な入出力パターンはテーブル駆動でまとめる
- 自明な初期値テストは書かない
- ヘルパ関数 (`press_at` / `hold_at` / `release_at` 等) を使って Arrange を簡潔に

#### コンパイルを通す → テスト失敗の確認

```bash
cargo test --workspace
```

- コンパイルが通ること
- 新規テストがアサーション失敗で落ちること (意味のある検証をしている証拠)
- 既存テストが壊れていないこと

### 5. 実装

テストが通るように実装する。

ガイドライン:
- 既存コードの設計・命名規則に合わせる (`platform/` / `renderer/` / `ui/` の責務分離)
- KISS / DRY / SSoT (CLAUDE.md 参照)
- **no-Clone 制約**: 新シグネチャに `Clone` 系 trait bound を入れない
- **derive マクロ禁止**: Lens / Data 等は使わない
- **library は audio / IPC を持ち込まない**: `Edit<M>` 発行で責務終了
- 新規導入した抽象は次のタスク (or このタスク内) で **使う** (CLAUDE.md「新しく入れた抽象は次の機会に使う」)
- エラーハンドリング: `?` を `ok()` に置き換えない、wgpu / winit のエラーは握りつぶさない

### 6. 全テスト通過の確認

```bash
cargo test --workspace
cargo clippy --workspace --tests -- -D warnings
```

- **新規テスト・既存テスト全てが通ること**
- clippy 増分ゼロ (master の pre-existing は除く)

#### 実機検証前の再ビルド

example で動作確認するときは:

```bash
cargo run --bin mixer                  # mixer (8 ch)
cargo run --bin waveform_validation    # 128 widgets グリッド
```

`cargo clippy` / `cargo check` / `cargo test` だけでは exe は更新されない (or test 用)。

#### docs/plan.md の更新

マイルストーン進捗を変える実装は `docs/plan.md` の Phase 表 / 残作業マーク / 履歴を **同じ commit に含めて更新** する
([memory: feedback_docs_with_code](~/.claude/projects/F--dev-gui-01/memory/feedback_docs_with_code.md))。

### 7. リファクタリング (必要に応じて)

全テストが通った状態でコードを整理する。

OK: リネーム、関数抽出、重複排除、`clippy` 警告修正、テストヘルパ整理
NG: 新機能追加 (次のサイクル)

### 8. コミット前レビュー

`/review` スキルで変更箇所のパフォーマンス・整合性・no-Clone 不変条件をチェック。

### 9. コミット

```bash
cargo test --workspace
cargo clippy --workspace --tests -- -D warnings
git add <変更ファイル>
git commit -m "feat(M3): Phase X — ○○を実装"
```

- コミットメッセージは日本語
- テストと実装は 1 コミットにまとめる
- `docs/plan.md` 更新を同 commit に含める (進捗を変えた場合)
- コンパイル警告を残さない

## テストが間違っていると気づいた場合

実装中にテストの期待値が間違っていると判断したら:

1. 根拠を明確にする (調査結果、実際の動作、taffy / wgpu の仕様等)
2. ユーザーに報告し、テスト修正の承認を得る
3. 承認後にテストを修正、実装を続ける

## 禁止事項

- 推測で実装しない (調査してから実装)
- `#[ignore]` でテストをスキップしない
- ユーザーの承認なしにテストの期待値を変更しない
- 要件にない挙動変更 (デフォルト値・初期状態・キーバインド) を勝手に入れない
- 設計不変条件 (no-Clone / no-derive-macro / メッセージ型禁止 / audio・IPC 不混入) を緩めない
