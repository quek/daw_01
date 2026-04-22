---
name: review
description: |
  変更箇所のリアルタイム安全性・パフォーマンス・セキュリティレビューを行い、問題を修正する。
  コミット前に自動で実行される想定だが、明示的に `/review` で呼び出すこともできる。
allowed-tools: Read, Grep, Glob, Edit, Bash(cargo build *), Bash(cargo clippy *), Bash(git diff *), Agent
---

# リアルタイム安全性・パフォーマンス・セキュリティレビュー

変更されたファイルを対象に、以下の観点でレビューし、問題があれば修正する。

- **リアルタイム安全性**（オーディオスレッド / CLAP `process()` 経路）
- **パフォーマンス**（UI ループ、描画）
- **セキュリティ / 整合性**（FFI 境界、外部入力、エラーハンドリング）

## 手順

### 1. 変更箇所の特定

```bash
git diff --name-only HEAD
```

変更のあった `.rs` ファイルを特定する。

### 2. リアルタイム安全性レビュー（最重要）

変更箇所が `daw_audio/src/engine.rs` / `common/src/audio_buffer.rs` / `command/` 経由で再生スレッドから呼ばれる関数 / `daw_plugin_host/` のオーディオ処理パスを含む場合、以下を厳しくチェック:

| チェック項目 | 問題パターン | 修正方針 |
|---|---|---|
| ホットパスのヒープ確保 | `Vec::new()`, `Vec::with_capacity()`, `String::new()`, `format!()`, `.to_vec()`, `.collect()`, `Box::new()` | 再生開始前に確保したバッファを再利用、`SmallVec` 等のスタックストレージ |
| ホットパスのロック | `Mutex::lock()`, `RwLock::read()/write()`, `parking_lot::Mutex::lock()` | lock-free キュー、`AtomicXxx`、共有メモリの snapshot 更新 |
| ホットパスの I/O | `println!`, `eprintln!`, `log::*`, `fs::*`, `std::io::*` | リングバッファに溜めて UI スレッドで吐く |
| ホットパスのシステムコール | `SystemTime::now()`, `thread::sleep`, `std::thread::spawn` | `Instant::now()` は許容、他は避ける |
| CLAP スレッド要件違反 | main-thread-only API をオーディオスレッドから呼ぶ／その逆 | `clap/plugin.h` のコメントでスレッド要件を確認、コールサイトで保証 |
| idle 中の無駄な処理 | Song が再生中でない時に `process()` を呼んでいる | idle 判定をスキップゲートにする |

### 3. パフォーマンスレビュー

UI スレッド（Vizia 描画ループ）や 1 秒に数十回以上呼ばれる経路をチェック:

| チェック項目 | 問題パターン | 修正方針 |
|---|---|---|
| 描画ループ内ヒープ確保 | `Vec::new()`, `format!()` を毎フレーム | 事前確保、キャッシュ、`String` 再利用 |
| 毎フレームの重い計算 | O(n) で全トラック走査 | dirty フラグ、差分更新 |
| 不要な clone | `.clone()` が回避可能 | 参照で保持、ライフタイムで表現 |
| Vizia の過剰な再描画 | 状態変化がないのにビュー再構築 | Lens / Binding で差分のみ更新 |

### 4. セキュリティ / 整合性レビュー

変更箇所を Read で読み、以下をチェック:

| チェック項目 | 問題パターン | 修正方針 |
|---|---|---|
| unsafe ポインタ操作 | `from_raw_parts`, `copy_nonoverlapping`, `*ptr.add(n)` | null チェック、配列長検証、ライフタイム確認 |
| 整数キャスト | `as i32`, `as u16`, `as u32`, `as usize` | `saturating_add/sub/mul`、`try_from`、範囲チェック |
| CLAP イベント配列 | 長さ未検証のままインデックスアクセス、時刻順ソート未確認 | `count` / `size` バリデーション、ソート保証 |
| 外部入力のバッファ | MIDI 入力、クリップボード、共有メモリ、VOICEVOX HTTP レスポンス | 上限検証、途中切断の扱い |
| FFI ハンドル寿命 | HWND / プラグインポインタのスレッド間共有 | 所有モデル明示、`Send`/`Sync` の正当性確認 |
| エラーの握りつぶし | `?` → `unwrap_or_default()` / `ok()` / `unwrap_or()` | 根本原因を調査し、そこを修正する |
| CLAP 初期化の連鎖失敗 | `create` / `init` / `activate` の戻り値を無視 | 各ステップを個別に検証、失敗時は明確にアンワインド |
| Song のデシリアライズ | 値域未検証（BPM=0、サンプルレート=0、Clip 長=負値） | 読み込み後 `sanitize()` でバリデーション |
| VOICEVOX レスポンス | JSON パースエラー、WAV 不正フォーマット | エラーハンドリング、ユーザーへの通知 |

### 5. 整合性の追加チェック

- **Single Source of Truth**: 同じデータが複数箇所に複製されていないか
- **保存と復元の対称性**: 新しい状態を追加した場合、保存・読込・undo の 3 箇所すべてを更新したか
- **VOICEVOX キャッシュの整合**: Clip 変更時にキャッシュ無効化が漏れていないか
- **設計判断の整合**: CLAUDE.md / DESIGN.md の原則に違反していないか

### 6. 問題の修正

発見した問題を重要度順に修正する。
- **High**: RT 安全性違反、FFI 未検証アクセス、エラー握りつぶし
- **Mid**: パフォーマンス、整合性
- **Low**: 軽微な重複、命名

修正後は `cargo build --workspace --release` と `cargo clippy --workspace -- -D warnings` で確認。

### 7. レポート

修正内容を箇条書きで報告する。問題がなければ「問題なし」と報告。

## 制約

- 変更のあったファイルのみを対象とする（プロジェクト全体をスキャンしない）
- 軽微な問題（UI の 1 回限り呼び出しでの `format!` 等）は無視してよい
- 修正は最小限に。既存の動作を変えない（要件にない挙動変更は禁止）
