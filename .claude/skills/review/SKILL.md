---
name: review
description: |
  変更箇所のリアルタイム安全性・パフォーマンス・セキュリティレビューを行い、問題を修正する。
  commit の前に回す (`/implement` の §8 から呼ぶ。明示的に `/review` でも呼べる)。
allowed-tools: Read, Grep, Glob, Edit, Bash(git diff *), Bash(git ls-files *), Bash(make clippy), Bash(make arch-lint), Bash(cargo check -p *), Bash(cargo clippy -p *), Bash(cargo test -p *), Agent
---

# リアルタイム安全性・パフォーマンス・セキュリティレビュー

変更されたファイルを対象に、以下の観点でレビューし、問題があれば修正する。

- **リアルタイム安全性**（オーディオスレッド / CLAP・VST3・builtin プラグインの `process()` 経路）
- **パフォーマンス**（UI ループ、描画）
- **セキュリティ / 整合性**（FFI 境界、外部入力、エラーハンドリング）

## 手順

### 1. 変更箇所の特定

```bash
git diff --name-only HEAD                    # 追跡済みファイルの変更
git ls-files --others --exclude-standard     # まだ add していない新規ファイル
```

変更のあった `.rs` ファイルを特定する。

§2〜5 の読み取りは、変更を書いた文脈を持たない subagent にやらせる (書いた本人は自分の意図で
読み補ってしまう)。渡すのは変更 (diff と上の新規ファイル) と §2〜5 の観点だけにし、見つけたものは件数を絞らず
重要度と確信度を付けて全部返させる。修正 (§6) はこの skill を呼んだ側が行う。

### 2. リアルタイム安全性レビュー（最重要）

変更箇所が再生スレッドから呼ばれる経路 (`daw_audio/src` の CPAL callback と worker、そこから呼ばれる `common/src` の関数、`daw_plugin_host/src` の `process()` に至る経路) を含む場合、以下を厳しくチェック:

| チェック項目 | 問題パターン | 修正方針 |
|---|---|---|
| ホットパスのヒープ確保 | `Vec::new()`, `Vec::with_capacity()`, `String::new()`, `format!()`, `.to_vec()`, `.collect()`, `Box::new()` | 再生開始前に確保したバッファを再利用、`SmallVec` 等のスタックストレージ |
| ホットパスのロック | `Mutex::lock()`, `RwLock::read()/write()`, `parking_lot::Mutex::lock()` | lock-free キュー、`AtomicXxx`、共有メモリの snapshot 更新 |
| ホットパスの I/O | `println!`, `eprintln!`, `log::*`, `fs::*`, `std::io::*` | リングバッファに溜めて UI スレッドで吐く |
| ホットパスのシステムコール | `SystemTime::now()`, `thread::sleep`, `std::thread::spawn` | `Instant::now()` は許容、他は避ける |
| CLAP スレッド要件違反 | main-thread-only API をオーディオスレッドから呼ぶ／その逆 | `clap/plugin.h` のコメントでスレッド要件を確認、コールサイトで保証 |
| 停止中の処理を勝手に止める | 「再生中でない」(transport の再生フラグ等) を根拠に `process()` や出力を止めている | 停止中も live 入力・残響の減衰・録音のために処理は回る。止めてよいのは daw_audio の park だけ (`buffer_is_idle` の全条件 = 窓が非アクティブ・停止中・count-in / 書き出し中でない・出力が無音。`docs/plan_idle_power.md`) |

### 3. パフォーマンスレビュー

UI スレッド (gui_01 の `UiHost::frame` / view の build closure / heavy() 内描画) や、
1 秒に数十回以上呼ばれる経路 (Tick / TrackPeaksTick handler 等) をチェック:

| チェック項目 | 問題パターン | 修正方針 |
|---|---|---|
| 描画ループ内ヒープ確保 | `Vec::new()`, `format!()` を毎フレーム | 事前確保、キャッシュ、`String` 再利用 |
| 毎フレームの重い計算 | O(n) で全トラック走査・全 clip 走査 | `ui.heavy(id, |hctx| { hctx.cached(viewport_key, ...) })` で粗粒度キャッシュ |
| 不要な clone | `.clone()` が回避可能 | 参照で保持、ライフタイムで表現 |
| 過剰な heavy() invalidation | viewport_key に毎フレーム変わる値 (Instant 等) を含めている | viewport_key は state hash のみ。時刻は含めない |

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
| Song のデシリアライズ | 値域未検証（BPM=0、サンプルレート=0、Clip 長=負値） | load の信頼境界 `Song::sanitize_ranges()` (入口は `Song::normalize_after_load`、`common/src/model/load_normalize.rs`) に clamp を足す |
| VOICEVOX レスポンス | JSON パースエラー、WAV 不正フォーマット | エラーハンドリング、ユーザーへの通知 |

### 5. 整合性の追加チェック

- **Single Source of Truth**: 同じデータが複数箇所に複製されていないか
- **保存と復元の対称性**: 新しい状態を追加した場合、保存・読込・undo の 3 箇所すべてを更新したか
- **VOICEVOX キャッシュの整合**: Clip 変更時にキャッシュ無効化が漏れていないか
- **設計判断の整合**: CLAUDE.md / DESIGN.md の原則に違反していないか
- **同件チェック** (bug fix の差分なら): 同じ root cause の同種箇所をリポジトリ全体で grep して全件直したか。
  報告に「根: 1 文 / 対象箇所: N 件 (表)」があるか (`feedback_sibling_occurrence_check`)
- **アーキテクチャ不変条件** (CLAUDE.md 同名節): `make arch-lint` を実行し新規違反ゼロを確認。
  特に: positional index addressing の混入 / protocol への bulk 直載せ / 単一 enum への回帰 /
  RT の無限待ち / edit_song 迂回の song 変更 / live・export の二重実装 / daw-ui core への
  ドメイン知識混入 / baseline 済みのサイズ超過 (FILE-BUDGET / FN-BUDGET / FN-NESTING) を
  更に太らせていないか

### 6. 問題の修正

発見した問題を重要度順に修正する。
- **High**: RT 安全性違反、FFI 未検証アクセス、エラー握りつぶし
- **Mid**: パフォーマンス、整合性
- **Low**: 軽微な重複、命名

修正後の確認は **`make clippy` / 修正に関係する test target (`cargo test -p <crate> --test <name>`) / `make arch-lint` の 3 つだけ**。
各 1 回 (arch-lint は上の §5 で見た不変条件チェックと同じ 1 回。二重に回さない)。全件 (`make test-nolaunch`) は自分の判断で
回さず、要ると判断したら回す前に一言断る。並列 worktree の 1 タスクでは `make clippy` も
`cargo clippy -p <crate> --all-targets -- -D warnings` に絞る (`feedback_gates_cadence`)。名指しの `--test` でも
CLAUDE.md「`make test` は daw_gui を起動する」の判定基準に当たる target は daw_gui を起動する。

- **素の `make test` は全件なので自分の判断では回さない** (上の段落)。daw_gui 本体を subprocess
  起動する target を含むが、起動そのものに許可は要らない (`feedback_cargo_tests_launches_app`)。
  ユーザーの daw_gui が動いていれば preflight が止めるので、kill せず閉じてもらうよう頼む
  (`feedback_no_kill_running_app`)。
- **release ビルドは回さない**。opt-level / LTO が違うだけで、debug の clippy が
  通っていて新たに出るコンパイルエラーは実質無い。thin LTO の全ビルドは数分〜
  十数分かかり、その間 review が止まるだけ。
- **素の `cargo build --workspace` / `cargo test --workspace` を使わない**
  (CLAUDE.md「Makefile が SSoT」)。`make` 側の scoping が効かなくなる。
- 同じコマンドを 2 度走らせない (失敗判定と件数確認は 1 回の出力から読む)。
- 変更が 1 crate に閉じているなら `cargo check -p <crate>` /
  `cargo test -p <crate> --test <name>` まで絞ってよい。

### 7. レポート

修正内容を箇条書きで報告する。問題がなければ「問題なし」と報告。

## 制約

- レビューの起点は変更のあったファイル（プロジェクト全体を当てもなく走査しない）。ただし見つけた問題の同件は §5 のとおり全件直し、
  読んでいて気づいた問題は変更外でもその場で直す (`feedback_fix_found_problems_no_scars`)
- ヒープ確保・`format!` の禁止はホットパス (再生スレッド / 毎フレーム / 高頻度 tick) の話で、UI の 1 回限りの呼び出しでの
  `format!` 等は問題にしない。それ以外で見つけた問題は軽微でも捨てず、§6 の重要度を付けて直す
- 1 件ごとの修正は最小限にし、既存の動作を変えない（要件にない挙動変更は禁止）。直す範囲は上の 2 項目のとおり狭めない
