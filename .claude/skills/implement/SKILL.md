---
name: implement
description: |
  機能追加・バグ修正のワークフロー。類似プロダクト調査→要件整理(→必要なら grill-me)→統合テスト→実装→実機検証→commit を一貫して行う。
  「実装して」「追加して」「修正して」「対応して」「機能を作って」「バグを直して」等、
  コード変更を伴う指示で発動。
argument-hint: "[実装したい機能の説明]"
allowed-tools: Read, Grep, Glob, Edit, Write, Bash(cargo test *), Bash(cargo build *), Bash(cargo clippy *), Bash(cargo run *), Bash(git add *), Bash(git commit *), Bash(git status *), Bash(git diff *), Agent, Skill, Workflow
---

# 機能実装ワークフロー (daw_01)

$ARGUMENTS を実装する。

調査 → 要件整理 → (必要なら統合テスト) → 実装 → 実機検証 → commit の順で進める。
テストはリグレッション防止を目的とし、可能な限り高いレイヤーで書く。

## 大原則 (CLAUDE.md より、 この skill の全段に優先)

- **理想とベストプラクティスを追求する。実装コストは無視して大胆に作り直す。**
  「実装コスト」「影響範囲」「現実的に」「妥協」が思考に出た時点で principle 違反
  (PreToolUse guard engine `scripts/guard_engine.py` + guards.jsonl の compromise-smell ルールが検出)。
- **最終形まで一気に完成させる。フェーズ分けをしない。**「Phase 1 完成、次に進みますか」は禁止。
  ゴールまで完走する。
- **まず調べる。推測で実装しない。** 一次情報 (DAW manual / CLAP spec / 参照実装 / gui_01 doc)
  を引用付きで確認してから書く。
- **worktree session ではファイル操作を worktree パスに向ける** (`feedback_worktree_path_discipline`)。
  メインリポジトリ絶対パスへ書かない。

## 手順

### 1. バグ修正の場合: ログで原因を特定する

バグ修正の場合、**推測で修正するな。** コードレビューだけで原因を断定せず、ログで実際の動作を検証する。

1. **疑わしい箇所にログを仕込む**: 関数の入口・出口、条件分岐の通過、CLAP 呼び出しの戻り値、IPC 送受信内容
2. **オーディオホットパスでは通常の `log` を使わない**: リングバッファに溜めて UI スレッドで吐くか、一時的なデバッグ用途に限定
3. **GUI のキーバインド/イベントは可視フィードバックが無いと判別不能**: `AppData::handle_event` 冒頭に
   `tracing::info!(?event)` を仕込む等、3 層 (キー拾えてない / emit されてない / handler 間違い) で切り分ける
   (`/debug-gui` skill)。フリーズ系は `/debug-plugin-gui` / `reference_freeze_debugging` memory。
4. **原因が確定してから修正する**: 「可能性がある」で修正しない。新機能が「動かない」報告は、
   操作ミス/環境でなく **自分の未配線を第一容疑** にする (`feedback_new_feature_bug_suspect_own_wiring`)。

特に CLAP プラグインの初期化処理 (`create` → `init` → `activate` → `start_processing`) は、`?` や `.ok()` で
エラーが握りつぶされて**初期化自体が失敗しているケース**がある。各ステップの成功を個別に検証する。

**FFI 境界 (D3D11 / wgpu / CLAP / cpal / windows API) の「対応する呼び出しが無いから dead」判定は禁止**
(`feedback_no_dead_judgment_at_ffi`)。相手側が内部消費している可能性が常にある。削除前に必ず実機 smoke test。

バグ修正ではない機能追加の場合はこのステップをスキップしてよい。

### 2. 類似プロダクトの調査

**推測で実装するな。** まず正しい振る舞いを調査してから実装する (`feedback_research_responsibility`)。

**前作 sing_like_coding はプロト品質**: RT パス (`process()` 内) での `Vec::new` / `Box::new`、`unwrap` /
`panic!` の粗さ、ハードコード定数などを鵜呑みにしない (`feedback_prioritize_best_practices`)。
構造 (プロセス分割、IPC の形、イベントの流れ) は参考にできるが、**実装は best practice で組み直す**。特に:
- RT セーフなバッファ確保 (activate 時に事前確保、process では再利用のみ)
- `Option<unsafe extern "C" fn>` は `unwrap` せず `ok_or_else` で null チェック
- CLAP / FFI エラーは `anyhow::Context` で意味のあるメッセージを付ける

**CLAP 拡張を新規ホスト側に追加するときのチェックリスト:**

1. `clap-sys` に該当する `clap_plugin_*` / `clap_host_*` struct と定数 (`CLAP_EXT_*`) がある
   ことを確認 (`~/.cargo/registry/src/.../clap-sys-*/src/ext/` を grep)
2. プラグイン側拡張 (`clap_plugin_gui` 等) は `plugin.get_extension(CLAP_EXT_*)` で取得。
   戻りが null のプラグインもあるので `Option<*const _>` で保持
3. ホスト側拡張 (`clap_host_gui` 等) は `Host::new()` で struct を埋め、`Host::get_extension`
   callback から `&host.clap_gui as *const _ as *const c_void` を返す。`host_data` から `&Host` を復元
4. CLAP spec の `gui.h` 等ヘッダの**呼び出し順序を厳守** (`create → set_scale → can_resize →
   get_size → set_parent → show` が正典)。順序変更/省略で壊れるプラグインがある
5. 各拡張メソッドの `[main-thread]` / `[audio-thread]` / `[any]` を確認。`@[main-thread]` は
   daw_plugin_host の **plugin-main std::thread** で直列化
6. プラグイン callback (`request_resize` 等) は**任意スレッド**から呼ばれうる。送信端は `Send + Sync`
7. 戻り値 bool は「`false` = エラー」とは限らない (VCV Rack は `show` が `false` でも動く)

以下に該当する場合、`/research-similar-impl` を呼ぶ。 ultracode が on なら `Workflow` で
**複数参照を並列に一次情報調査** (各 agent が web/source を読み structured で返す):

| 該当条件 | 例 |
|---|---|
| 「○○みたいに」と参考プロダクトが指定 | 「Bitwig みたいに LFO/ランダム/MSEG 変調」 |
| CLAP / VST3 仕様に関わる | プリセット読込、thread pool、latency、tail、param_mod |
| DAW として一般的な機能 | ピアノロール、ミキサー、バス、変調、オートメーション曲線 |
| 正しい振る舞いが MIDI / CLAP 仕様に依存 | ノートオフ、ピッチベンド、MPE、時刻順イベント |
| gui_01 (daw-ui) の使い方が不明 | heavy()/push_rect/text/lines、scrubable_number、dropdown、LayoutPass |
| VOICEVOX API の挙動が不明 | sing API のエッジケース、スピーカー切替 |

調査で明らかにする: 実際の振る舞い / エッジケース (SR・バッファ変更・idle・crash) /
設計判断 (アルゴリズム・データ構造・RT 安全性)。引用 URL・ソース行番号付きで記録する。

該当しない場合 (内部リファクタ、単純なバグ修正) はスキップしてよい。

### 3. 要件の整理 (ユーザー承認ゲート)

調査結果と既存コード (Read/Grep) をもとに要件を整理する。
**参照製品が当然備える操作を完全列挙し、core 操作 (命名/改名・色・削除・undo 等) を polish 扱いで後回しにしない**
(`feedback_enumerate_complete_feature_set`)。

| 観点 | 問いかけ |
|---|---|
| 正常系 | 基本入力に対して何を返す／何が起きるべきか？ |
| エッジケース | 空 Clip、0 トラック、SR 変更、バッファサイズ変更、idle、プラグイン未ロード、source 削除 |
| RT 安全性 | daw_audio 再生スレッドで new / lock / I/O / format! を増やしていないか？ |
| 既存機能との相互作用 | Undo、保存／復元 (bincode/serde)、VOICEVOX キャッシュ、Arrangement、export に影響しないか？ |
| SSoT | このデータは誰が所有し誰が更新するか。複製を作っていないか？ |
| 類似プロダクトとの一致 | 調査した振る舞いを全部カバーしているか？ |

#### アーキテクチャ影響チェック (CLAUDE.md「アーキテクチャ不変条件」)

実装前に以下を列挙し、1 つでも該当したら `docs/plan_arch_refactor.md` の該当節を読んで
不変条件に整合する形で設計する (整合しない要求はユーザーへ設計相談):

- **新しい参照/アドレスを導入するか?** → 安定 id (device_id / send_id / 要素 id) 一本。
  positional index・「削除時に貼り替える補償コード」は禁止 (不変条件 1)
- **プロセス間で新しいデータを運ぶか?** → 宛先型 enum (AudioCommand 等) に variant を足す。
  bulk (PCM / blob) は直載せしない (不変条件 2/3)。wire を渡る型を新ファイルへ置いたら
  `common/build.rs` の WIRE_SOURCES に追加 (不変条件 7)
- **Song を編集するか?** → `edit_song()` チョークポイント経由のみ (不変条件 5)
- **RT パス (CPAL callback / worker / process()) に触れるか?** → 無限待ち・確保・解放禁止、
  重い構築は off-thread + ring swap (不変条件 4)
- **export / live の両方に効く音声処理か?** → `render_master_buffer` の中に入れる (不変条件 6)
- **widget を作るか?** → DAW 固有なら daw_gui/src/widgets/ (common::model 直結)、
  汎用なら ui/crates (ドメイン知識ゼロ)。mirror 型・翻訳 enum を作らない (不変条件 8)
- **ファイルが 3,000 行に近いか?** → 先に分割 (不変条件 9)。`make arch-lint` で検査

**要件一覧をユーザーに提示し、過不足の確認を取る。承認を得てから次へ進む**
(`feedback_no_redundant_verification` — 未完成段階で実機確認を求めない)。

- 設計判断が多い／分岐が深い機能は **`/grill-me`** で決定木を一問ずつ潰す (#49-54/#56 の確立パターン)。
- ユーザーへの質問は **「見える挙動」の言葉** で、 **番号付き選択肢** (推奨を 1 番)、 **最も上流から 1 問ずつ**
  (`feedback_plain_language_questions` / `feedback_numbered_question_options` / `feedback_one_question_at_a_time`)。
- 大きめプランは `docs/plan_<feature>.md` に最終形を書く (`feedback_plan_location`)。

#### gui_01 (daw-ui) widget の拡張が要るとき

interim な自前 widget を作る前に、**まず `docs/gui_01_conversation.md` に要望を出す**
(`feedback_gui_01_conversation` / `feedback_gui_01_request_before_interim`)。
- 最終的にこう使いたい完成形を全部書く。v1/v2 の段階分割をしない (`feedback_gui_01_scope_review`)。
- `関連仕様: docs/plan_<feature>.md` を必ず添える (`feedback_gui_01_link_plan_ref`)。
- landing を待つ間も **gui_01 非依存の backend は全部進める** (`feedback_progress_while_waiting_gui01`)。
  widget が landing したら呼び出し側を wire (parked)。
- 「値 X を公開して」の前に daw_01 が既に mirror/算出してないか grep (`feedback_verify_gui01_need_before_request`)。

### 4. 統合テストの作成

承認された要件をもとに統合テストを書く (TDD: 失敗するテスト → 実装 → 通す)。

#### テストをスキップしてよいケース

以下のすべてに該当する場合のみスキップ可:
- UI 操作 (メニュー、キーバインド、ドラッグ) やプロセス起動が主で自動テストが困難
- 一度ビルド・実行すれば正しさが確認できる
- 既存ロジック (model 変換、serialize/deserialize、イベント変換、DSP) に変更がない

視覚出力 (video preview / texture) は build/test/clippy をすり抜ける。`--smoke-test` で別途担保 (§6)。

#### テストのレイヤー

可能な限り高いレイヤーでテストする。上から順に検討し、最も高いレイヤーを選ぶ。

| レイヤー | 方法 | 例 |
|---|---|---|
| **コマンド／イベント層** | `AppData::handle_event` / `command/*` を呼び Song/Track/Clip の変化を検証 | トラック追加、Clip 編集、プラグインロード、変調 routing CRUD |
| **モデル操作** | `Song`/`Track`/`Clip`/`Row` のメソッドや `ensure_ids`/save-load 往復を検証 | copy/paste、undo/redo、bincode round-trip、歌詞分割 |
| **純粋ロジック** | 関数に入力を与え出力を検証 | DSP、BPM/サンプル変換、`apply_modulation`、変調器の `f(beat)`、正規化 |

protocol/model 型 (bincode derive) を変えたら `make build`
(`feedback_workspace_build_for_protocol_changes`) — daw_gui だけ rebuild すると子プロセスが
古い protocol のまま decode 失敗し「再生が止まる」誤認症状になる。

#### テスト設計のガイドライン

- **1 テスト = 1 つのユーザーシナリオ**
- 期待値は `assert_eq!` で具体値を検証 (`starts_with()` / `> 0` は使わない)
- 単純な入出力はパラメタライズドテストにまとめる:

```rust
#[test]
fn lfo_は_beat_の純粋関数で各シェイプの値を返す() {
    // (shape, rate_beats, beat, expected_unipolar_0_1)
    let cases = [
        (LfoShape::Sine,   1.0, 0.0,  0.5),   // sine は phase0 で中央
        (LfoShape::Sine,   1.0, 0.25, 1.0),   // 1/4 で頂点
        (LfoShape::SawUp,  1.0, 0.0,  0.0),
        (LfoShape::SawUp,  1.0, 0.5,  0.5),
        (LfoShape::Square, 1.0, 0.0,  1.0),
        (LfoShape::Square, 1.0, 0.6,  0.0),
    ];
    for (shape, rate, beat, expected) in cases {
        let cfg = LfoConfig { shape, rate_beats: rate, phase: 0.0, ..Default::default() };
        let got = eval_lfo(&cfg, beat);
        assert!((got - expected).abs() < 1e-6, "shape={shape:?} beat={beat} got={got}");
    }
}
```

- 自明な初期値テスト (`assert_eq!(x.field(), 0)`) は書かない
- テストヘルパーを積極的に作り Arrange を簡潔に保つ
- 変調器の **決定論** (同じ beat → 同じ値、ランダムは `f(seed,beat)` の純ハッシュ) を必ずテストする
  (export 再現性の前提)

#### コンパイルを通す

テスト対象の関数・構造体がまだ無い場合、コンパイルが通る最小スタブ (デフォルト値を返す空実装) を足してよい。

#### テスト失敗の確認

```bash
make test
```

- コンパイルが通る / 新規テストがアサーション失敗で落ちる (意味のある検証の証拠) / 既存テストは壊れていない

### 5. 実装

テストが通るように、**最終形まで一気に**実装する。

ガイドライン:
- 既存コードの設計・命名規則・コメント密度に合わせる (`common/`/`daw_gui/`/`daw_audio/`/`daw_plugin_host/` の責務分離)
- KISS・DRY・SSoT (同じデータを複製しない、所有者を明確に)
- **RT 安全性**: daw_audio 再生スレッドで heap 確保・lock・I/O・`format!` を足さない。
  バッファは再生前に確保し使い回す
- **FFI 境界**: 整数キャストは `try_from`/`saturating_*`、ポインタ null/境界、配列長を検証
- **エラーを握りつぶさない**: `?` を安易に `ok()`/`unwrap_or_default()` にしない
- **要件にない変更を入れない**: 既存挙動を勝手に変えない。ついでのリファクタは別コミット

#### 5.5 GUI/UX の配置・操作性 (UI を足す・変える時は必読)

「動く」だけでなく**使いやすい配置・操作性**まで設計する。 機能を view に挿す前に、 描画コードを読んで
**どこに・どう出るか / 開閉で何が動くか / どこと重複するか** を必ず確認する (推測で y を動かさない)。
過去、 これを怠ってパネルを冗長/不安定な位置に出し、 2 連続で UI 手戻りになった (2026-06-20)。

- **配置 = トリガの近く**: パネル/セクションは、 それを開閉するボタンの**近く**に出す。 トリガ (例: チェーン行の
  「Par」ボタン) と表示が画面の遠く (例: インスペクタ最上部) に分かれると、 押しても効いたか分からず操作不能感。
- **トグル安定性 = 他を動かさない**: パネルの開閉で**他のコントロール (特にトリガ自身) が動いてはいけない**。
  daw_01 インスペクタは `track_inspector.rs` で **param viewport (上・縦 scroll) + chain band (下・pinned)** の
  2 分割 (`boundary_y` で分かれる)。 viewport の content 高が変わると `boundary_y` が動いて chain band (=
  「Par」ボタン) ごと下にずれ、 「押した瞬間ボタンが逃げる / 表示してすぐ非表示」 という操作不能を生む
  (2026-06-20 実例)。 → 開閉で高さが変わらないよう **viewport 高を固定** する等、 レイアウトを安定させる。
- **重複編集面を作らない**: 同じ param を**2 箇所で編集できる状態にしない** (既存の専用セクション + 新パネルの
  二重表示)。 表示面は 1 つに集約し、 もう片方は gate で隠す (SSoT を「画面」 にも適用)。 2026-06-20 に
  字幕 X/Y・talk 話速を専用欄と新パネルで二重表示して手戻り。
- **no-scroll / 固定 band の制約**: インスペクタは縦 scroll の param 領域 + 下端固定 band。 背の高いパネルは
  scroll 領域に入れる / overflow がボタンを覆わないか確認する。 widget の縦 budget (行数 cap・`*_section_h`) を読む。
- **既存 UI idiom を流用**: `scrubable_number` / `dropdown` / video_fx param パネル (`inspector_video_fx_params`) /
  clip voice picker。 bespoke な edit-buffer widget を新設しない (`feedback_reuse_inspector_idiom`)。
- **配置・操作性は build/clippy/test をすり抜ける**: §6 の自動検証では**絶対に分からない**。 必ず実機で
  目視する (できれば自分で起動)。 数値計算が合っていても描画結果がズレ/重なり/はみ出すことがある
  (`feedback_verify_actual_content`)。
- **可変背景の上に標識を描くならコントラストを保証する** (`feedback_ui_indicator_contrast_on_variable_bg`):
  クリップ色 / トラック色 / 波形の上に出すスピナー / バッジ / ドット / オーバーレイは、固定の白 (near-white)
  や黒 (near-black) 単色だと **明クリップ上で白が・暗クリップ上で黒が沈んで見えない**。 暗い半透明バッキング
  チップ + 明色標識 (idiom: `voicevox_overlay::draw_spinner_badge`) / 対比色の輪郭 / 背景輝度からの
  auto-contrast のいずれかでコントラストを保証し、 **明るいクリップと暗いクリップの両方で目視** する
  (`track_color` の明色プリセットで 1 つ着色して確認)。 color / contrast も build/clippy/test をすり抜ける。
- **「上/下/近く/見づらい/やりにくい」等の配置 feedback は、 まず描画コードの y フロー・領域分割を Read してから直す**。
  どの領域 (scroll viewport / pinned band) のどの `y` に出ているかを特定してから動かす。

### 6. 全テスト通過 + 実機ビルドの確認

```bash
make test
make clippy
```

**実機検証前の再ビルド (必須)**: clippy/check/test は実行 exe を生成しない (or test exe のみ)。
`./target/debug/daw_gui.exe` で検証する前に必ず `cargo build` を明示 (`feedback_build_after_clippy`):

```bash
cargo build -p daw_gui     # daw_gui だけ変えた場合
make build                 # 子プロセス (daw_audio / daw_plugin_host) も変えた場合は必須
```

子プロセスのコードを変えてバイナリを再生成しないと「直したのに挙動が変わらない」混乱が起きる。
`os error 5` (アクセス拒否) は対象バイナリ起動中。ユーザーに閉じてもらう (`feedback_no_kill_running_app` —
`taskkill` で勝手に止めない)。

#### 視覚出力の smoke test

video preview / texture / shared-handle に触れる変更は **commit 前に必ず**:

```bash
cargo run -p daw_gui -- --smoke-test daw_gui/tests/fixtures/smoke_test.mp4
# exit 0 = visible content / exit 1 = blank/uniform/transparent
```

「在る」でなく**動的に正しく振る舞う**を検証する (静止 1 枚でなく動き・全フレーム、perf より correctness 先行、
`feedback_verify_actual_content`)。

### 7. リファクタリング (必要に応じて)

全テストが通った状態で整理する。
OK: リネーム、関数抽出、重複排除、clippy 警告修正、テストヘルパー整理。
NG: 新機能追加 (次サイクル)。リファクタ後も全テスト通過を確認。

### 8. コミット前レビュー

`/review` を呼び、変更箇所の correctness・パフォーマンス・セキュリティ・RT 安全性をチェックして直す
(`feedback_review_before_commit` — わかっているバグは spawn_task に回さずその場で修正)。

### 9. 実機検証 → コミット

**commit はユーザーの実機/視覚 sign-off を得てから** (`feedback_confirm_before_commit` —
自動検証だけで先に commit しない)。GUI/オーディオ/プラグイン挙動は `/verify-app` で起動して確認
(`feedback_launch_app_for_verification` — 自分で起動する。ただし `feedback_no_duplicate_app_launch` —
既存起動中なら二重起動しない。`feedback_launch_no_tail_pipe` — `| tail` 越しに起動しない)。

承認を得たら:

```bash
make test
make clippy
git add <変更ファイルを全列挙>        # -A / . / ディレクトリ指定は不可 (feedback_git_add_one_file)
git commit -m "<日本語メッセージ>"
```

- コミットメッセージは日本語。テストと実装を 1 コミットにまとめる。警告を残さない
- コマンドを `&&`/`;` で連結しない (`feedback_no_command_chaining`)。作業ディレクトリに `cd`/`--manifest-path`
  を付けない (`feedback_no_cd_prefix`)
- commit を細かく割りすぎない (`feedback_dont_split_commits_too_finely`)

## テストが間違っていると気づいた場合

1. 根拠を明確にする (調査結果、実際の動作、CLAP/DAW 仕様)
2. ユーザーに報告し、テスト修正の承認を得る
3. 承認後にテストを修正し、実装を続ける

## 禁止事項

- 推測で実装しない (調査してから)
- `#[ignore]` でテストをスキップしない
- ユーザーの承認なしにテストの期待値を変更しない
- 要件にない挙動変更 (デフォルト値、初期状態、キーバインド) を勝手に入れない
- 機能を消したまま新機能に進まない (`feedback_recovery_priority` — 復旧を優先)
- やれる作業が残っているのに進捗報告だけして turn を終えない (`feedback_dont_stop_prematurely`)
