---
name: review
description: |
  変更箇所のリアルタイム安全性・パフォーマンス・セキュリティレビューを行い、問題を修正する。
  commit の前に回す (`/implement` の「commit まで」から呼ぶ。明示的に `/review` でも呼べる)。
allowed-tools: Read, Grep, Glob, Edit, Bash(git diff *), Bash(git ls-files *), Bash(make clippy), Bash(make arch-lint), Bash(cargo check -p *), Bash(cargo clippy -p *), Bash(cargo test -p *), Agent
---

# 変更のレビュー (daw_01)

変更されたファイルを起点に、RT 安全性・パフォーマンス・FFI / 外部入力・整合性を見て、問題があれば直す。
規範の正本は CLAUDE.md (「Real-Time Audio の制約」「アーキテクチャ不変条件」「FFI 境界」「SSoT / エラー」)。
ここには、それを読んだだけでは見落としやすいこのプロジェクト固有の観点だけを書く。

## 手順

### 1. 変更箇所の特定

```bash
git diff --name-only HEAD                    # 追跡済みファイルの変更
git ls-files --others --exclude-standard     # まだ add していない新規ファイル
```

### 2. 読む

読むのは、変更を書いた文脈を持たない subagent 1 体にやらせる (書いた本人は自分の意図で読み補ってしまう)。
渡すのは変更 (diff と上の新規ファイル) と下の観点だけにし、見つけたものは件数を絞らず重要度と確信度を付けて
全部返させる。返ってきた指摘は、本番の経路で実際に起きるかを自分で確かめてから直す。

**再生スレッドに至る経路** (`daw_audio/src` の CPAL callback と worker、そこから呼ばれる `common/src` の関数、
`daw_plugin_host/src` の `process()` に至る経路) を含むなら:

- 確保・解放・ロック・I/O・ログ・無限待ち (CLAUDE.md の RT 制約と不変条件 4)。
- CLAP / VST3 のスレッド要件 (main-thread-only の API を audio thread から呼んでいないか、その逆)。
- 「再生中でない」を根拠に `process()` や出力を止めていないか。停止中も live 入力・残響の減衰・録音のために
  処理は回る。止めてよいのは daw_audio の park だけ (`buffer_is_idle` の全条件。`docs/plan_idle_power.md`)。

**UI スレッドと高頻度の経路** (`UiHost::frame` / view / `heavy()` 内 / Tick・TrackPeaksTick の handler) なら:

- 毎フレームの確保と、全トラック・全 clip の走査。変わらない層は `hctx.cached(viewport_key, ..)` に入れる。
- `viewport_key` に毎フレーム変わる値 (時刻等) を入れていないか。Edit・ヒットテスト・動的 overlay を
  `cached` の中に置いていないか。

**FFI / 外部入力**:

- CLAUDE.md「FFI 境界」の各項。CLAP のイベント配列は長さと時刻順。`create` / `init` / `activate` の戻り値を
  1 つずつ確かめているか。HWND / プラグインポインタをスレッド間で共有するなら `Send` / `Sync` の根拠。
- 外部から来るバッファ (MIDI 入力・クリップボード・共有メモリ・VOICEVOX の HTTP レスポンス) の上限と途中切断。
- Song のデシリアライズで値域を確かめていない field (BPM = 0、サンプルレート = 0、負の長さ) は、load の信頼境界
  `Song::sanitize_ranges()` (入口は `Song::normalize_after_load`、`common/src/model/load_normalize.rs`) に clamp を足す。

**整合性**:

- 新しい状態を足したら、保存・読込・undo の 3 箇所とも更新したか。load 時に作り直す派生データは冪等か
  (開くだけで dirty にならないか)。
- Clip を変えたときに VOICEVOX キャッシュの無効化が漏れていないか。
- bug fix の差分なら、同じ根の同種箇所をリポジトリ全体で洗って全件直したか (`feedback_sibling_occurrence_check`)。
- アーキテクチャ不変条件。`make arch-lint` で機械検査し、検査の無い 5 / 6 (edit_song の迂回、live と export の
  二重実装) と、baseline 済みのサイズ超過を更に太らせていないかは目で見る。

### 3. 直して確かめる

重要度順に直す (RT 安全性違反・FFI の未検証アクセス・エラーの握りつぶし > パフォーマンス・整合性 > 軽微な重複・命名)。
軽微でも捨てない。読んでいて気づいた問題は変更外でもその場で直す。1 件ごとの修正は最小限にし、要件にない
挙動変更は入れない。

確かめるのは `make clippy` / 修正に関係する test target / `make arch-lint` の 3 つを各 1 回
(回し方の規則は CLAUDE.md「Development Workflow」。並列 worktree の 1 タスクでは clippy も
`cargo clippy -p <crate> --all-targets -- -D warnings` に絞る)。release ビルドは回さない。

### 4. 報告

直した内容を箇条書きで報告する。問題がなければ「問題なし」。

## 対象外

確保・`format!` の禁止はホットパス (再生スレッド / 毎フレーム / 高頻度 tick) の話で、UI の 1 回限りの呼び出しは
問題にしない。
