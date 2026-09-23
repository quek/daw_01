---
name: implement
description: |
  機能追加・バグ修正のワークフロー。類似プロダクト調査→要件整理(→必要なら grill-me)→統合テスト→実装→実機検証→commit を一貫して行う。
  「実装して」「追加して」「修正して」「対応して」「機能を作って」「バグを直して」等、
  コード変更を伴う指示で発動。
argument-hint: "[実装したい機能の説明]"
allowed-tools: Read, Grep, Glob, Edit, Write, Bash(make build), Bash(make clippy), Bash(make arch-lint), Bash(cargo check -p *), Bash(cargo build -p *), Bash(cargo clippy -p *), Bash(cargo test -p *), Bash(python scripts/loc_budget.py *), Bash(git add *), Bash(git commit *), Bash(git status *), Bash(git diff *), Agent, Skill, Workflow
---

# 機能実装ワークフロー (daw_01)

$ARGUMENTS を実装する。

調査 → 要件整理 → (必要なら統合テスト) → 実装 → 実機検証 → commit の順で進める。
テストはリグレッション防止を目的とし、可能な限り高いレイヤーで書く。

この skill は長く、会話が compaction されると先頭の約 5,000 token しか戻らない (後半の検証・
`/review`・sign-off・commit の手順が落ちる)。compaction の後に続けるときは、この skill を同じ
引数で呼び直して全文を戻してから進む。

## 大原則 (CLAUDE.md より、 この skill の全段に優先)

- **理想とベストプラクティスを追求する。実装コストは無視して大胆に作り直す。**
  「実装コスト」「影響範囲」「現実的に」「妥協」が思考に出た時点で principle 違反。
- **最終形まで実装する。** 禁じているのは途中で報告して承認を待つこと (「Phase 1 完成、次に進みますか」) で、
  計画を段階に割ることではない (大規模改修を `docs/plan_*.md` で段階に割るのはむしろ推奨)。
  止まって聞いてよいのは CLAUDE.md「最終形まで実装する」の 4 場面だけ。それ以外はゴールまで完走する。
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
5. **直す前に同件を全件洗う**: 根 (この語 / この API / この呼び出し方 / この型) を 1 文に一般化し、リポジトリ全体を
   grep して同種箇所を全部挙げてから、同じ commit で class ごと直す (対象外にするものは理由を書く)。報告には
   「根: 1 文 / 対象箇所: N 件 (表)」を入れる (`feedback_sibling_occurrence_check`)。

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
3. ホスト側拡張 (`clap_host_gui` 等) は `Host` (`daw_plugin_host/src/clap_host.rs`) にフィールドを足して `Host::new()` で埋め、
   `get_extension` callback で id を比べて `std::ptr::from_ref(&this.clap_gui) as *const c_void` のように返す。
   callback 内では `Host::from_clap(host)` で `host_data` から `&Host` を復元する
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

### 3. 要件の整理

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
不変条件に整合する形で設計する (整合しない要求は、要求と不変条件のどちらを変えるかで作るものが変わるので、
着手前にユーザーへ 1 問で設計相談する。CLAUDE.md「止まって聞く場面」の着手前の問いに当たる):

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
- **サイズ budget に近いか?** (ファイル実コード 1,000 行 / 関数実コード 300 行 /
  インデント 6 段) → 先に分割 (不変条件 9)。現在値は `python scripts/loc_budget.py --report`、
  検査は `make arch-lint`。**物理行ではない** — テスト / doc comment / 空行は数えない

**要件一覧は書き出して見える形にする。止まってユーザーに聞くのは、CLAUDE.md「止まって聞く場面は 4 つだけ」の
着手前の 2 つ (UI の見せ方・操作 / 2 通りに読めて作るものが変わる要件) に当たる点だけ**。それ以外は聞かずに次へ進む
(未完成の段階で実機確認を求めないのも同じ — `feedback_no_redundant_verification`)。

- 設計判断が多い／分岐が深い機能は **`/grill-me`** で決定木を一問ずつ潰す。
- ユーザーへの質問は **「見える挙動」の言葉** で、 **番号付き選択肢** (推奨を 1 番)、 **最も上流から 1 問ずつ**
  (`feedback_plain_language_questions` / `feedback_numbered_question_options` / `feedback_one_question_at_a_time`)。
- 大きめプランは `docs/plan_<feature>.md` に最終形を書く (`feedback_plan_location`)。

#### daw-ui (旧 gui_01) の widget 拡張が要るとき

**`ui/` は同一 workspace なので、同じセッションで直接編集する** (`project_gui01_integrated` —
旧 sibling repo 時代の `docs/gui_01_conversation.md` 経由の往復と待ち合わせは廃止済み。
あのファイルは歴史的記録)。
- daw_gui 側に interim な自前 widget を作らず、**ライブラリ側を直す**。利用者全員が同じ
  boilerplate を書く状況は設計欠陥のシグナル (`ui/CLAUDE.md`「使う側に boilerplate を強要しない」)。
- 変更は最終形を一度に入れる。v1/v2 の段階分割をしない (`feedback_gui_01_scope_review`)。
  breaking change を入れたら全 example / test / docs を **1 commit で一括更新**する。
- 「値 X を公開する」前に、daw_01 側が既に mirror / 算出していないか grep する (SSoT)。
- daw-ui core に **DAW 固有のドメイン知識を持ち込まない** (CLAUDE.md 不変条件 8)。
  arrangement / piano_roll は `daw_gui/src/widgets/` で `common::model` 直結。

### 4. 統合テストの作成

整理した要件をもとに統合テストを書く (TDD: 失敗するテスト → 実装 → 通す)。

#### テストを書く範囲

テストは非自明なロジック (純粋関数の境界計算・写像・状態機械の分岐) と、外部で定義された真実との突き合わせ
(規格の信号・移植元の実装との一致・往復同一性などの不変条件) に書く。自明な修正 (ホワイトリストへの 1 行追加・
1:1 の dispatch 配線・既存パターンの踏襲) には書かない。本番の算術をテスト側に写して突き合わせるだけのテストも
書かない (`feedback_no_tests_for_simple_cases`)。GUI / IPC / 再生を跨ぐ確認も `daw_gui --script <js>` で自分で回し
(足りない操作は script API に足す)、「自動では確かめにくい」を理由にユーザーの実機確認へ回さない。
ユーザーに頼むのは最終 sign-off だけ (CLAUDE.md「テスト」、§9)。

視覚出力 (video preview / texture) は build/test/clippy をすり抜ける。`--smoke-test` で別途担保 (§6)。

#### テストのレイヤー

可能な限り高いレイヤーでテストする。上から順に検討し、最も高いレイヤーを選ぶ。

| レイヤー | 方法 | 例 |
|---|---|---|
| **コマンド／イベント層** | `AppData::handle_event` / `handler/*` の AppData メソッドを呼び Song/Track/Clip の変化を検証 | トラック追加、Clip 編集、プラグインロード、変調 routing CRUD |
| **モデル操作** | `Song`/`Track`/`Clip`/`Row` のメソッドや `ensure_ids`/save-load 往復を検証 | copy/paste、undo/redo、bincode round-trip、歌詞分割 |
| **純粋ロジック** | 関数に入力を与え出力を検証 | DSP、BPM/サンプル変換、`apply_modulation`、変調器の `f(beat)`、正規化 |

protocol/model 型 (bincode derive) を変えたら `make build`
(`feedback_workspace_build_for_protocol_changes`) — daw_gui だけ rebuild すると子プロセスが
古い protocol のまま decode 失敗し「再生が止まる」誤認症状になる。

#### テスト設計のガイドライン

- **1 テスト = 1 つのユーザーシナリオ**
- 期待値は `assert_eq!` で具体値を検証 (`starts_with()` / `> 0` は使わない)
- 単純な入出力はパラメタライズドテストにまとめる (`(入力, 期待値)` の配列を 1 ループで回し、失敗メッセージに入力を出す)。
  実例: `common/src/modulators.rs` の `lfo_各shapeが既知点で正しい値を返す`
- 自明な初期値テスト (`assert_eq!(x.field(), 0)`) は書かない
- テストヘルパーを積極的に作り Arrange を簡潔に保つ
- 変調器の **決定論** (同じ beat → 同じ値、ランダムは `f(seed,beat)` の純ハッシュ) を必ずテストする
  (export 再現性の前提)

#### コンパイルを通す

テスト対象の関数・構造体がまだ無い場合、コンパイルが通る最小スタブ (デフォルト値を返す空実装) を足してよい。

#### テスト失敗の確認

```bash
cargo test -p <crate> --test <name>   # 変更に関係する target だけを名指しで回す
cargo test -p <crate> --lib <filter>
```

- コンパイルが通る / 新規テストがアサーション失敗で落ちる (意味のある検証の証拠) / 既存テストは壊れていない
- **全件 (`make test-nolaunch`) は自分の判断で回さない**。全件が要ると判断したら回す前に一言断る
  (`feedback_gates_cadence`)。`--test` で名指ししても、CLAUDE.md「`make test` は daw_gui を起動する」の
  判定基準に当たる target は daw_gui を起動する (起動に許可は要らない)
- **素の `make test` は全件なので自分の判断では回さない** (上の項目)。`daw_gui/tests/` の一部が daw_gui 本体を
  `--script` で subprocess 起動して audio device を開くが、起動そのものに許可は要らない
  (`feedback_cargo_tests_launches_app`)。ユーザーの daw_gui が動いていれば preflight が止めるので、kill せず
  閉じてもらうよう頼む (`feedback_no_kill_running_app`)。

### 5. 実装

テストが通るように、**最終形まで一気に**実装する。

ガイドライン:
- 既存コードの設計・命名規則・コメント密度に合わせる (`common/`/`daw_gui/`/`daw_audio/`/`daw_plugin_host/` の責務分離)
- KISS・DRY・SSoT (同じデータを複製しない、所有者を明確に)
- **RT 安全性**: daw_audio 再生スレッドで heap 確保・lock・I/O・`format!` を足さない。
  バッファは再生前に確保し使い回す
- **FFI 境界**: 整数キャストは `try_from`/`saturating_*`、ポインタ null/境界、配列長を検証
- **エラーを握りつぶさない**: `?` を安易に `ok()`/`unwrap_or_default()` にしない
- **要件にない挙動変更を入れない**: 既存挙動を勝手に変えない。ついでのリファクタは別コミット。ただし作業中に見つけた
  問題 (バグ・RT 経路の非効率・古いコメント等) はタスク外でもその場で直す (`feedback_fix_found_problems_no_scars`。
  r.md の backlog 項目に勝手に着手するのとは別 — `feedback_fixme_is_backlog_not_donow`)

#### 5.5 GUI/UX の配置・操作性 (UI を足す・変える時は必読)

「動く」だけでなく**使いやすい配置・操作性**まで設計する。 機能を view に挿す前に、 描画コードを読んで
**どこに・どう出るか / 開閉で何が動くか / どこと重複するか** を必ず確認する (推測で y を動かさない)。
過去、 これを怠ってパネルを冗長/不安定な位置に出し、 2 連続で UI 手戻りになった (2026-06-20)。

- **配置 = トリガの近く**: パネル/セクションは、 それを開閉するボタンの**近く**に出す。 トリガ (例: チェーン行の
  「Par」ボタン) と表示が画面の遠く (例: インスペクタ最上部) に分かれると、 押しても効いたか分からず操作不能感。
- **トグル安定性 = 他を動かさない**: パネルの開閉で**他のコントロール (特にトリガ自身) が動いてはいけない**。
  daw_01 インスペクタ (`daw_gui/src/view/track_inspector/mod.rs`) は title 下の全部が 1 本の縦 scroll viewport で、
  各セクションを `(app, ui, area, pad, y) -> f32` の関数で上から積む (y カーソル 1 本、 並び順がそのまま画面の上下順)。
  トリガより上で高さが変わると、 トリガごと下にずれて 「押した瞬間ボタンが逃げる / 表示してすぐ非表示」 という
  操作不能を生む (2026-06-20 実例)。 → 開閉するパネルはトリガの直下 (行内アコーディオン) に出し、 トリガより上の高さを変えない。
- **重複編集面を作らない**: 同じ param を**2 箇所で編集できる状態にしない** (既存の専用セクション + 新パネルの
  二重表示)。 表示面は 1 つに集約し、 もう片方は gate で隠す (SSoT を「画面」 にも適用)。 2026-06-20 に
  字幕 X/Y・talk 話速を専用欄と新パネルで二重表示して手戻り。
- **縦 scroll の制約**: インスペクタは 1 本の縦 scroll viewport で、 content 高には前フレームの測定値 (`inspector_body_h`、
  immediate-mode の lag-by-one) を使う。 背の高いパネルもこの流れの中に積む。
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
  どのセクション関数のどの `y` に出ているかを特定してから動かす。

### 6. テスト・lint 通過 + 実機ビルドの確認

```bash
cargo test -p <crate> --test <name>   # 変更に関係する target だけ (§4 と同じ。全件は回す前に一言断る)
make clippy
make arch-lint       # 新規違反ゼロ (exit 0 = 違反ゼロ or baseline 済みのみ)
```

実機で挙動を往復している間は、修正のたびに clippy / テストを挟まない (`cargo check -p <crate>` → build → 起動で回す)。
clippy / arch-lint / 関連テストは修正が固まってから 1 回にまとめる。並列 worktree の 1 タスクでは `make clippy` も回さず
`cargo clippy -p <crate> --all-targets -- -D warnings` まで、全件は統合後に main で 1 回 (`feedback_gates_cadence`)。

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

関連テストが通った状態で整理する。
OK: リネーム、関数抽出、重複排除、clippy 警告修正、テストヘルパー整理。
NG: 新機能追加 (次サイクル)。リファクタ後も関連テストの通過を確認。

### 8. コミット前レビュー

`/review` を呼び、変更箇所の correctness・パフォーマンス・セキュリティ・RT 安全性をチェックして直す
(`feedback_review_before_commit` — わかっているバグは別タスクや「残件」に回さずその場で直す)。

### 9. 実機検証 → コミット

**commit はユーザーの実機/視覚 sign-off を得てから** (`feedback_confirm_before_commit` —
自動検証だけで先に commit しない)。GUI/オーディオ/プラグイン挙動は `/verify-app` で起動して確認
(`feedback_launch_app_for_verification` — 自分で起動する。ただし `feedback_no_duplicate_app_launch` —
既存起動中なら二重起動しない。`feedback_launch_no_tail_pipe` — `| tail` 越しに起動しない)。

承認を得たら:

```bash
# 最後に回した検証 (§6 / §8 の /review) の後にコードを変えたときだけ、関連 test target と make clippy / make arch-lint を
# 1 回回し直す (同じ検証を 2 度回さない — feedback_gates_cadence)
git add <変更ファイルを全列挙>        # -A / . / ディレクトリ指定は不可 (feedback_git_add_one_file)
git commit -m "<日本語メッセージ>"
```

- コミットメッセージは日本語。テストと実装を 1 コミットにまとめる。警告を残さない
- commit を細かく割りすぎない (`feedback_dont_split_commits_too_finely`)

## テストが間違っていると気づいた場合

期待値を直してよいのは、根拠 (調査結果・実際の動作・CLAP/DAW 仕様) で期待値の誤りを示せるときだけ。
実装を通すために期待値を合わせにいかない。直した期待値と根拠は最終報告に書く。
誤りが要件の読み違いから来ていて、どちらを取るかで作るものが変わるなら、CLAUDE.md の
「止まって聞く場面」(要件が 2 通りに読めるとき) として 1 問で聞く。

## 禁止事項

- 推測で実装しない (調査してから)
- `#[ignore]` でテストをスキップしない
- 根拠なしにテストの期待値を変えない (上の「テストが間違っていると気づいた場合」)
- 要件にない挙動変更 (デフォルト値、初期状態、キーバインド) を勝手に入れない
- 機能を消したまま新機能に進まない (`feedback_recovery_priority` — 復旧を優先)
- やれる作業が残っているのに進捗報告だけして turn を終えない (`feedback_dont_stop_prematurely`)
