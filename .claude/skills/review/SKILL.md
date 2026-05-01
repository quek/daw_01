---
name: review
description: |
  変更箇所のパフォーマンス・整合性・設計不変条件 (no-Clone / derive 禁止 / メッセージ型禁止 /
  audio・IPC 不混入) のレビューを行い、問題を修正する。
  コミット前に自動で実行される想定だが、明示的に `/review` で呼び出すこともできる。
allowed-tools: Read, Grep, Glob, Edit, Bash(cargo build *), Bash(cargo clippy *), Bash(cargo test *), Bash(git diff *), Agent
---

# パフォーマンス・整合性・設計不変条件レビュー (gui_01)

変更されたファイルを対象に、以下の観点でレビューし、問題があれば修正する。

- **設計不変条件** (gui_01 固有の load-bearing 制約)
- **パフォーマンス** (UI ループ、描画ループ、毎フレーム呼ばれる経路)
- **整合性 / セキュリティ** (FFI 境界、外部入力、エラーハンドリング、SSoT)

## 手順

### 1. 変更箇所の特定

```bash
git diff --name-only HEAD
```

変更のあった `.rs` / `.md` / `Cargo.toml` ファイルを特定する。

### 2. 設計不変条件チェック (gui_01 最重要)

CLAUDE.md「設計上の不変条件」を破っていないか厳密にチェック。

| チェック項目 | 問題パターン | 修正方針 |
|---|---|---|
| ユーザ Model に Clone 要求 | 新 widget の API シグネチャに `where M: Clone` / `M: PartialEq` / `M: Hash` / `M: Default` | trait bound を外す、必要なら値を Edit のクロージャで move キャプチャ |
| メッセージ型の導入 | 新しい `enum Message { ... }` を library 層に追加 | `Edit<M> = Box<dyn FnOnce(&mut M)>` 経由に書き換える、msg を経由しない |
| derive マクロ | `#[derive(Lens)]` / `#[derive(Data)]` / カスタム derive 追加 | 手書きクロージャでアクセサを書く、derive を消す |
| 差分検出が ID+末端値 hash 以外 | Model 全体を `clone` / `==` 比較 / `Hash` で判定 | widget ID + プリミティブ末端値 hash に置き換える、Model 構造体には触らない |
| `Ui<'a>` の borrow を超える | GAT を使う、`'static` 借用を要求する、ライフタイムを `<'a, 'b>` に分離する | `'a` 1 本に統一する、難しければ設計を見直す |
| library に audio / IPC | `crates/platform/` / `crates/renderer/` / `crates/ui/` 内で `cpal` / `bincode` / `tokio::net` 等を import | `Edit<M>` を返すところで責務を切る、audio は別 crate (本ライブラリ外) |
| `crates/ui/tests/no_clone_required.rs` の trybuild 回帰 | 新 API 追加時に trybuild の pass test を更新していない | 新 widget / API を `tests/ui/pass/basic.rs` に呼び出し追加、no-Clone Model でコンパイル確認 |

設計不変条件は CLAUDE.md / `docs/plan.md` の「設計上の不変条件」が正本。**緩めない**。

### 3. パフォーマンスレビュー

UI スレッドの描画ループ / 1 秒に 60 回以上呼ばれる経路をチェック。

| チェック項目 | 問題パターン | 修正方針 |
|---|---|---|
| 描画ループ内のヒープ確保 | `Vec::new()`, `format!()`, `String::from()`, `.collect()`, `Box::new()` を毎フレーム | 事前確保、`String` 再利用、static / const、`itoa` |
| 毎フレームの重い計算 | O(N) で全 widget 走査、O(N²) のソート、波形を毎フレーム再計算 | dirty フラグ、差分更新、`generation` キーで cache 再利用 (M2 LOD ピラミッド参照) |
| 不要な clone | `.clone()` が回避可能 | 参照で保持、ライフタイムで表現、`Cow<'_, str>` |
| 毎フレームの Vec push 連発 | `scene.push_rect` を loop で呼ぶ大量描画 | バッチ化、instanced rendering (rect / line pipeline)、heavy() (M5) でキャッシュ |
| `.contains` を含む再帰探索 | `HashMap::get` / `Vec::iter().find` が深いネストで | 平坦化、index 化 |
| `request_redraw` の過剰呼び出し | 毎フレーム request_redraw、idle 時も連続描画 | edits / focus 変化 / IME 変化のときだけ呼ぶ ([memory: project_post_edit_redraw](~/.claude/projects/F--dev-gui-01/memory/project_post_edit_redraw.md)) |

ベンチ対象なら `crates/ui/benches/` に criterion テスト追加 (M2 波形ベンチ参照、N=1/8/16/64/128 のスケール検証)。

### 4. 整合性 / セキュリティレビュー

変更箇所を Read で読み、以下をチェック。

| チェック項目 | 問題パターン | 修正方針 |
|---|---|---|
| 整数キャスト | `as i32`, `as u32`, `as usize`, `as f32` で範囲超過 | `try_from`, `saturating_add/sub/mul`, 範囲チェック |
| unsafe / FFI | wgpu / winit / glyphon の `unsafe { ... }` ブロック新規追加 | 必然性を justify、long-lived な参照は避ける、Send/Sync の正当性を確認 |
| エラー握りつぶし | `?` → `unwrap_or_default()` / `ok()` / `unwrap_or()` | 根本原因を調査、wgpu / winit からのエラーは握りつぶさない |
| `panic!` / `unreachable!` の追加 | プラグイン / 入力検証で固いケース以外で panic | `Result` で表現、回復可能なら `eprintln!` + 続行 |
| widget state の downcast | `Box<dyn WidgetState>` から直接 downcast (M2 で踏んだバグ) | `&mut **entry` で deref → `as_any_mut()` → `downcast_mut::<S>()` の順 |
| 外部入力のバッファ | キーボード text、IME preedit、wgpu surface size | 上限検証、UTF-8 境界、リサイズ 0 ピクセル対応 |
| Single Source of Truth | 同じデータが 2 箇所以上で持たれている | 所有者を明確化、もう一方は参照 / 計算で生成 |
| 保存と復元の対称性 | 新しい state field を追加したが復元側を更新していない | save / restore / undo / Edit apply を 1 セットで更新 |

### 5. ドキュメント / メタファイルの整合

| チェック項目 | 問題パターン | 修正方針 |
|---|---|---|
| `docs/plan.md` の進捗表更新漏れ | コードを変えたが Phase 表 / 履歴が更新されていない | 同 commit で更新 ([memory: feedback_docs_with_code](~/.claude/projects/F--dev-gui-01/memory/feedback_docs_with_code.md)) |
| 設計判断の記録漏れ | 非自明な選択 (例: `Dimension::percent(1.0)` を選んだ理由) を doc コメントや plan.md に書いていない | コードに doc コメント追加、または plan.md の設計判断セクションに追記 |
| 新しく入れた抽象が使われていない | API / 型 / モジュールを新設したのに次のタスクで使っていない | 同タスク内で実用例を 1 つ用意、またはなぜ後回しかを明示 ([memory: feedback_use_new_abstractions](~/.claude/projects/F--dev-gui-01/memory/feedback_use_new_abstractions.md)) |
| skill / CLAUDE.md の整合 | 新しい知見 / 罠を memory にだけ書いて CLAUDE.md「既知の罠」に統合していない | 横断的に発火しそうなものは CLAUDE.md にも一行追加 |

### 6. clippy / test の最終確認

```bash
cargo clippy --workspace --tests -- -D warnings
cargo test --workspace
```

**増分ゼロ** であることを確認。pre-existing の warning は対象外だがレポートに記載。

### 7. 問題の修正

発見した問題を重要度順に修正。

- **High** (commit 阻止級): 設計不変条件違反、エラー握りつぶし、未検証 unsafe ポインタアクセス、no-Clone trybuild 回帰
- **Mid**: パフォーマンス劣化 (描画毎フレーム heap 等)、整合性、SSoT 違反
- **Low**: 軽微な重複、命名、コメント不足

修正後は `cargo clippy --workspace --tests -- -D warnings` と `cargo test --workspace` で再確認。

### 8. レポート

修正内容を箇条書きで報告。問題が無ければ「問題なし」。

## 制約

- 変更のあったファイルのみを対象とする (プロジェクト全体をスキャンしない)
- 軽微な問題 (1 回限りの `format!` 等) は無視してよい
- 修正は最小限。既存の動作を変えない (要件にない挙動変更は禁止)
- 設計不変条件は **緩めない**。違反を見つけたら修正必須
