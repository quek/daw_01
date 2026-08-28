# r.md #76 — god file budget の測り方を実コード行 (ncloc) へ入れ替える

**この計画は #76 専用であり、他項目との統合順は `docs/plan_rmd_index.md` を見ること。**

**この計画書だけを読んで完走できるように書いてある。実装者は他の会話文脈を持たない前提。**

- 対象: r.md #76「god file budget (arch-lint 不変条件 9 / `scripts/arch_lint.sh:288`) の判定が
  素の `wc -l` で、テスト module と doc comment まで行数に数えています。(中略) 測り方から
  見直してください」
- **この項目が所有するのは「指標」だけ。Rust のソース (`*.rs`) は 1 行も触らない。**
  分割の実作業は r.md #77 (`daw_gui/src/widgets/arrangement/run.rs`) 以降が持つ。
- 触るのは `scripts/` / `Makefile` / `Cargo.toml` の lint 設定 / `.claude/skills/` /
  `CLAUDE.md` / `docs/`。

---

## 0. 今どうなっていて、何が壊れているか (実測済み。再調査不要)

### 0.1 現行の検査

`scripts/arch_lint.sh:284-290` (check 6):

```sh
# 6. god file budget (不変条件 9): 生成物を除く .rs は 3,000 行以内。
hits=$(find common/src daw_gui/src daw_audio/src daw_plugin_host/src ui/crates -name '*.rs' \
    -not -path '*/target/*' \
    -not -name 'binding_ffmpeg*' -not -name 'bindings.rs' 2>/dev/null \
    | xargs wc -l 2>/dev/null | awk '$1 > 3000 && $2 != "total" { print $2, $1 " 行" }')
# path を第 1 field に置く (行数は増減するので fingerprint に含めない)。
record FILE-BUDGET firstfield "3,000 行超の .rs (分割してから足す):" "$hits"
```

### 0.2 確認済みの事実 (すべて実測。この計画はこれを前提に書いてある)

1. **今日この検査は空振り**: 物理行 3,000 超は **0 件**。最大は
   `daw_audio/src/graph/compile.rs` の 2,978 行 (残り 22 行)。
2. **逆インセンティブは既に 2 回発火した**。`eefdea1`
   (`refactor(arch-refactor #9): model.rs god-file 分割 step1 — tests を model/tests.rs へ`、
   -2270/+2476、本文「挙動・serialize 形式は不変 (pure code movement)」) と `720e2c1`
   (`refactor(arch-refactor #9): ui.rs god-file 分割 — tests を ui/tests.rs へ (invariant #9
   完全達成)`、本文「これで make arch-lint = **OK (違反なし)**」)。
   構造は 1 ミリも良くなっていない。
3. **doc を書くと課金される**: `common/src/model.rs` は 2,927 行中 doc 845 行、
   `daw_gui/src/widgets/arrangement/mod.rs` は 2,413 行中 doc 922 行 (実コード比 52%)。
4. **`-not -name 'binding_ffmpeg*' -not -name 'bindings.rs'` は今日すでに死んだ除外**。
   実体 (`daw_gui/ffmpeg/binding_ffmpeg_7.1.rs` 26,381 行 / `ara-sys/src/bindings.rs`
   2,421 行) は走査 root (`*/src` と `ui/crates`) の外にあり、除外指定が無くても当たらない。
   逆に `daw_gui/tests/` `common/tests/` `ara-sys` `signalsmith-sys` は非対象という非対称。
5. **check 5/6/7/8 は canary で検証されていない**。`scripts/arch_lint.sh:44-74` の canary が
   試すのは共有正規表現 4 本 (`INFINITE_RE` / `POSKEY_RE` / `UNTAGGED_RE` / `PROTOCOL_RE`)
   = check 1/2/3/4 だけ。
6. **check 6 には行内マーカーの逃げ道が無い** (`strip_allowed` を呼んでいない)。
7. **測り方の実装が 2 か所にある**: `.claude/skills/arch-review/SKILL.md:33` が同じ
   `find … | xargs wc -l` ワンライナーを丸ごと複製している (`:11` の `allowed-tools` も
   `Bash(find *), Bash(wc *)` でそれを許可している)。
8. **行数では真の god code を捕まえられない**: `daw_gui/src/view/track_inspector/mod.rs:228`
   の `fn draw` は実コード **2,063 行**なのに、ファイルが 2,623 行なので現行検査は一度も
   報告していない。
9. **関数長ゲートが木の一部にだけ既に存在する**: `Cargo.toml:132` の
   `[workspace.lints.clippy] pedantic` を **10 crate** が `[lints] workspace = true`
   (各 `Cargo.toml:11-12`) で opt-in しており、`make clippy` (`Makefile:169` = `-D warnings`)
   の下で **閾値 100 の `clippy::too_many_lines` が今日エラーとして効いている**。
   opt-in している 10 crate の内訳 (実測):
   `ui/crates/{ui, renderer, platform}` +
   `ui/crates/examples/{automation, embedded_host, mixer, sample_editor, sample_edit_ops,
   text_input_ime, waveform_validation}`。
   **`Cargo.toml:124` のコメント「ui/crates/* のみ opt-in する」自体は正しい** — 誤りやすいのは
   「ui/crates 直下の 3 crate だけ」という読みで、`ui/crates/examples/*` も含まれる。
   抑制の実数は `#[allow(clippy::too_many_lines)]` が **33 箇所**
   (ui/crates 24 — うち examples 5 / daw_gui 9)。`grep -rn too_many_lines --include='*.rs'`
   は 34 行を返すが、1 行は `ui/crates/renderer/src/pipelines/texture.rs:102` の
   doc comment 内の言及。
10. **stale な記述**: `Makefile:216-218` のコメント「違反は列挙のみ (exit 0)」は現在の挙動
    (新規違反があれば exit 1) と食い違う。
11. **`strip_comments` が check 1/2/3/5/8 の違反を取りこぼす**。
    `scripts/arch_lint.sh:14` は `grep -vE '^[^:]+:[0-9]+:[[:space:]]*//'` = 「行頭が `//`」
    だけを見るので、**raw string の中の行頭 `//`** をコメントと誤判定して落とす。
    現在該当するのは `common/src/video_fx.rs` の WGSL シェーダ (14 行) と
    `ui/crates/renderer/tests/texture_interop.rs` (1 行)。逆に `/* … */` の中に書かれた
    違反パターンは今日 1 行も落とされない (行頭が `//` ではないので)。
    **どちらも「検査器が間違える」側の穴で、`scripts/arch_lint.sh:4-7` / `:29-30` が掲げる
    「違反ゼロの報告を無条件に信じない」と正面から衝突する。この項目で塞ぐ** (§1.6)。
12. **関数キーは「path::型名::関数名」だけでは今日すでに一意でない**。実測 (この計画の
    改訂時に §3.5-§3.7 の規則を独立実装して全 389 ファイルを走査): 関数 6,229 本に対し
    **衝突キー 17 件**。原因は 2 種類しかない:
    - **cfg 対**: 同名の item が `#[cfg(windows)]` / `#[cfg(unix)]`(または `not(windows)`)、
      `#[cfg(debug_assertions)]` / `#[cfg(not(debug_assertions))]` で 2 つ並ぶ。
      `common/src/shmem.rs` の `mod imp` 2 つ (`:32` / `:170`) が `NamedShmem::{create, open,
      len, is_empty, as_ptr, drop}` の 6 件、`daw_gui/src/single_instance.rs` の `mod platform`
      2 つ (`:24` / `:137`) が `{acquire, spawn_raise_listener}` の 2 件、
      `daw_gui/src/handler/devices.rs:1197,1216` の `AppData::cleanup_slot_gui`、
      `daw_gui/src/view/about.rs:119,152` の `ffmpeg_runtime_lines`、
      `daw_audio/src/main.rs:831,848` の `hold_released_entry_for_test`
      (**`cfg(debug_assertions)` 対なので両方 production コード**。テスト扱いで消えない)、
      `daw_gui/src/app_types.rs:2307,2324` / `daw_gui/src/smoke_test.rs:{408,550},{442,555}` /
      `ui/crates/platform/src/winit_backend.rs:514,537` / `ui/crates/ui/src/ui.rs:2748,2777` /
      `daw_gui/src/view/about.rs` も同型。
    - **同一型への複数 trait impl**: `daw_gui/src/midi_import.rs:120,128` の
      `impl From<std::io::Error> for MidiImportError` と
      `impl From<midly::Error> for MidiImportError` が両方 `fn from`。
    **`#2` `#3` の連番で潰すのは不可**。連番は出現順に依存するので、無関係な並べ替えで
    baseline の天井が入れ替わる = `scripts/arch_lint.sh:91-92` が「行番号ではなく内容ハッシュ」に
    した理由と同じ故障を、キー側に作ることになる。§3.7 で **trait 名と cfg 述語をキーに含める**
    ことにより **衝突 0 件** になることを実測確認済み (同じ独立実装で 6,246 本 / 衝突 0)。
13. **`.rs` の 28 ファイルが CRLF 改行**を含む (`ara-sys/src/bindings.rs` /
    `common/src/time.rs` / `daw_audio/src/graph/compile.rs` / `daw_gui/ffmpeg/binding_ffmpeg_7.1.rs` /
    `daw_gui/src/handler/project.rs` ほか)。単独 CR は **0 ファイル**、末尾改行の無いファイルも
    **0 ファイル**。→ §3.3 / §3.5 の読み込み規則をこれに合わせて確定させてある。
14. **`ara-sys/build.rs:47` に `.raw_line("// @generated by …")` がある** — 生成マーカーの
    文字列が **コードの中の文字列リテラル**として現れる。行数窓 (先頭 N 行) で生成物を判定すると
    N を 47 以上にした瞬間に `build.rs` が走査から丸ごと落ちる。§1.4 の判定を行数窓ではなく
    **「先頭の連続するコメント / 空行ブロックの中にあること」**にしてこの依存を断つ。

---

## 1. 確定仕様 (ユーザー承認済み。実装者が変えてよい箇所ではない)

### 1.1 何を数えるか — 実コード行 (ncloc)

SonarQube の定義と同一: **空白でもタブでもコメントの一部でもない文字を 1 つ以上含む物理行**。
doc comment (`///` `//!` `/** */` `/*! */`) は comment 側に数え、ncloc に入れない。
複数行にまたがる文字列リテラルの中身は **code**。

### 1.2 テストコードは budget の対象外 (上限を課さない)

不変条件 9 が防ぎたい故障は「1 モジュールが無関係な責務を溜め込む」ことで、独立した
`#[test] fn` のフラットな並びには原理的に当てはまらない。判定は **3 経路すべて**:

1. **`#[cfg(test)]` が付いたあらゆる item** — `mod tests { }` だけでなく `fn` / `use` /
   `const` も。実例: `daw_audio/src/graph/compile.rs:900-901` の `#[cfg(test)]` +
   `pub(crate) fn compile_schedule_for_test`。
2. **`#[cfg(test)] mod X;` の宣言から解決されるファイル全体**。repo 内の実例は 5 件で、
   いずれも `#[path]` を使っていないので解決は素直
   (`grep -rn '#\[path' --include='*.rs'` = **0 件**、実測確認済み):
   - `common/src/model.rs:2926-2927` → `common/src/model/tests.rs`
   - `daw_gui/src/lib.rs:18-19` → `daw_gui/src/app_tests.rs`
   - `daw_gui/src/lib.rs:65-66` → `daw_gui/src/test_ffmpeg.rs`
   - `daw_gui/src/widgets/arrangement/mod.rs:2412-2413` → `daw_gui/src/widgets/arrangement/tests.rs`
   - `ui/crates/ui/src/ui.rs:2793-2794` → `ui/crates/ui/src/ui/tests.rs`
3. **`tests/` / `benches/` 直下の統合テスト**。Rust の統合テストは crate 全体がテスト
   ビルド専用なので `#[cfg(test)]` を書かない = 経路 1・2 のどちらにも当たらない。
   判定は「パス成分に `tests` または `benches` があり、その **親ディレクトリに
   `Cargo.toml` がある**」= Cargo の規約そのもの。現在 **69 ファイル**が該当
   (実測: `git ls-files --cached --others --exclude-standard -- '*.rs'` を
   `/tests/` `/benches/` で数えた値)。

**テスト範囲の境界を厳密に決める** (§3.3 の保存則がここに依存する):

- 経路 1 の範囲は **「その item の最初の属性行の `#` がある行」から「item の終端行」まで**。
  属性より上にある doc comment は範囲外 = doc として数える (`compile.rs:898-899` がこれ)。
- 経路 2・3 は **ファイル全体**が範囲。

### 1.3 閾値

| 検査 | 単位 | budget | 違反条件 |
|---|---|---:|---|
| `FILE-BUDGET` | ファイルの実コード行 | **1,000** | `> 1000` |
| `FN-BUDGET` | 関数の実コード行 | **300** | `> 300` |
| `FN-NESTING` | 関数内の最大インデント段数 | **6** | `> 6` |

- 1,000 は SonarQube の同種ルール **S104 "Files should not have too many lines of code"** の
  既定値。SonarSource は S104 を物理行から lines of code へ数え方を変更した経緯があり
  (sonar-dotnet#396 / MMF-571 / "Kill the Noise")、r.md #76 と同じ結論に業界の lint が先に
  到達している。
- 関数の実コード行の実測分布は p50 11 / p90 50 / p99 195 / max 2,063。300 は p99 の上。
- **ネストは brace 深さではなくインデント段数**で測る (r.md #77 が使っている指標と揃える)。
  段数 = 行頭の空白を tab=4 で展開して 4 で割った商。**絶対値**で測る (mod / impl の
  ラッパも段数に数える)。r.md #77 の公表値と一致することを実測で確認済み:
  `arrangement/run.rs::arrangement` = 最大 11 段、`track_inspector/mod.rs::draw` = 最大 8 段。
  - 注: `daw_gui/src/widgets/arrangement/run.rs` は先頭が rustfmt 標準から外れていて、
    `:6` の `#[allow(clippy::too_many_lines)]` と関数本体がまるごと 1 段ぶん余計に
    字下げされている (`:7` の `pub fn arrangement` だけ 0 段)。絶対値で測る以上これも
    段数に乗るが、r.md #77 の 11 段はこれを含んだ値なので **一致する**。
    「字下げがおかしいから測り直す」は不要。

### 1.4 走査対象は原理で決める

```
git ls-files -z --cached --others --exclude-standard -- '*.rs'
```

- **`--others --exclude-standard` を必ず付ける**。`git ls-files` だけだと `git add` 前の
  新規巨大ファイルを見逃す = 検査器が黙って空振りする経路を新設してしまう。
  (`--exclude-standard` があるので `target/` や `third_party/` は .gitignore で落ちる。)
- ここから **生成物を除く**。判定は **「ファイル先頭の連続するコメント / 空行ブロックの中に
  生成マーカーがある」**こと。マーカーは `automatically generated by` / `@generated` /
  `code generated` (大小無視)。
  - 「先頭ブロック」= `lex()` の行分類 (§3.5) で `comment` / `doc` / `blank` が続く限りの前置き。
    **最初に `code` 行が現れた時点で打ち切る**。判定に使うのは `comment` / `doc` 行だけ。
  - **行数窓 (先頭 N 行) にしない**。`ara-sys/build.rs:47` に
    `.raw_line("// @generated by \`cargo build -p ara-sys --features regen\` — do not edit by hand.")`
    があり (§0.2-14)、窓を 47 行以上に取ると `build.rs` が生成物として走査から丸ごと落ちる。
    「今日はギリギリ外れている」という状態は検査器の穴なので、行数に依存しない規則にする。
    先頭コメントブロック規則なら、この行は **コードの中の文字列リテラル**なので原理的に当たらない。
  - 現在ちょうど 2 件が外れる: `ara-sys/src/bindings.rs` と
    `daw_gui/ffmpeg/binding_ffmpeg_7.1.rs` (両方 1 行目が
    `/* automatically generated by rust-bindgen 0.71.1 */` = 先頭コメントブロック)。
    **両方とも git 追跡下**なので、この除外は現行の `-not -name` と違って実際に効く。
  - リポジトリ全体で `automatically generated by` / `@generated` / `code generated` を含む
    `.rs` 行は上記 3 か所 (`bindings.rs:1` / `bindings.rs:3` / `build.rs:47`) と
    `binding_ffmpeg_7.1.rs:1` だけであることを実測確認済み。
- **現行の `-not -name 'binding_ffmpeg*' -not -name 'bindings.rs'` は廃止する** (§0.2-4)。
- 結果: 走査 391 → 生成物 2 除外 → **389 ファイル**、うちテスト扱い 74
  (統合テスト 69 + `#[cfg(test)] mod X;` 解決 5)。

**「0 ファイル走査でも緑」を原理的に塞ぐ (これが無いと検査が丸ごと消える)**:

- `git ls-files` の **サブプロセスが非ゼロ終了、または git が起動できない**
  (`FileNotFoundError`) なら **exit 2** で落とす。空リストへ縮退させない。
- 走査できたファイル数が **`MIN_SCANNED_FILES` (= 200) を下回ったら exit 2**。
  今日は 389 なので、半分を割った時点で「走査そのものが壊れた」と見なす。
- 入れなかった場合の帰結を書いておく: `scan_files()` が空を返すと違反は 0 行になり、
  `LOC-BUDGET-OK 0 files …` は出るので `arch_lint.sh` の完走マーカー検査 (§4.4) を素通りし、
  **exit 0 の緑**になる。しかも baseline の全行が「解消 — baseline から削除してよい」として
  表示されるので、案内どおり削除すると **検査が永久に消える**。
  CLAUDE.md が記録している `guards.jsonl` 消失事故 (`if not isfile: return 0` の fail-open で
  「無い」と「該当しない」が区別できなかった) と完全に同型。

### 1.5 実装は `scripts/loc_budget.py` 1 本 (Python stdlib のみ)

- **Rust の字句解析 (lexer) が必要。構文解析 (AST) は不要。** 行ベース近似では
  `#[cfg(test)]` の範囲決定が壊れることを実測済み: `common/src/project.rs:1815` の
  `fs::write(&path, "not valid json {").unwrap();` (文字列中の対応しない `{`) で naive な
  brace カウンタが desync する。誤差が有界でないので近似は許されない。
  `common/src/video_fx.rs` の `wgsl: r#"..."#` 中の WGSL が `//` を含む問題も同時に片付く。
- **bash + grep/sed で書かない**。`scripts/arch_lint.sh:18-27` が記録している MSYS2 の
  argv バックスラッシュ消失 (make 経由で 8 検査中 6 つが無言で無効化され、違反 7 行を
  抱えたまま「OK (違反なし)」を出していた) は、grep/sed へ引数を渡すから起きる。
  Python 実装には argv の往復が無く、この故障モード自体が消滅する。
  **`arch_lint.sh` 側は Python を呼んで stdout を読むだけに留め、パターンを shell に
  持たせないこと。**
- `syn` / `proc-macro2` は使わない (lint が cargo ビルドを前提にしてしまう)。

### 1.6 `strip_comments` も同じ lexer に寄せる (この項目の内側)

`scripts/arch_lint.sh:14` の `strip_comments` は check 1/2/3/5/8 の共有フィルタで、
**行頭が `//` の行を落とす**だけの近似。§0.2-11 のとおり 2 方向に間違える:

- raw string 中の行頭 `//` を「コメント」と誤判定して落とす = **違反を取りこぼす**
- `/* … */` の中に書かれた違反パターンは落とさない = **コメント内の言及を違反に数える**

**この計画の初版は「#76 の範囲外。やらない」として `STRIP-COMMENTS-BLIND` という
件数 baseline を新設していた。この改訂で反転し、根治する。** 理由:

- 初版が根拠にしていた memory `feedback_no_defensive_overgeneralization` は
  「**正しい診断から『念のため』の禁止を書くな**」であって「診断した欠陥を放置してよい」
  ではない。ここで足すのは新しい禁止ルールではなく、既に診断済みの誤判定の修正。
- 「検査器の欠陥を見つけたが直さず、件数を baseline に登録して黙らせる」は、
  この項目が問題視している `eefdea1` / `720e2c1` (ゲートを黙らせるためだけの移動) と
  同じ形をしている。
- lexer はこの項目で手に入る。**行分類の権威が `loc_budget.py` と `arch_lint.sh` の
  2 か所に並立するのは SSoT 違反**で、片方だけ正確という状態を残す理由が無い。

置き換えの形 (詳細は §3.10 / §4.1):

```sh
# 行分類の SSoT は loc_budget.py。**パターンを shell に持たせない**。
strip_comments() { "$PY" scripts/loc_budget.py --filter-comments; }
```

**今日この置き換えで違反件数は変わらない** (実測で確認済み。ratchet が荒れない):

- 現在 baseline に載っている 4 行 (`POSITIONAL-KEY`) はすべて実コード行
  (`daw_gui/src/app_types.rs:1519` / `daw_gui/src/state/ipc.rs:57,78,182`)。
  lexer 版でも落ちないので「解消」の誤通知は出ない。
- 新たに見えるようになる 15 行 (video_fx.rs 14 + texture_interop.rs 1) に、
  check 1/2/3/5/8 のパターンは 1 つも含まれない — 両ファイルに対する
  `grep -cE "WaitForSingleObject|HashMap<[(]u32|MainToChild|ChildToMain|serde[(]untagged|ArrangementEditRequest|split_into_morae|Edit::Undoable"`
  が **どちらも 0**。さらに `texture_interop.rs` は `ui/crates/renderer/tests/` なので
  check 8 の走査 root (`ui/crates/ui/src`) の外、`video_fx.rs` は check 1 の走査 root
  (`daw_audio/src` / `common/src/plugin_ref.rs` / `daw_plugin_host/src`) の外。

---

## 2. 触るファイル一覧

| ファイル | 種別 | 何をするか |
|---|---|---|
| `scripts/loc_budget.py` | **新規** | 字句解析器 + 3 種の budget 測定 + `--check` / `--report` / `--self-test` / `--filter-comments` |
| `scripts/arch_lint.sh` | 変更 | python 検出 (不在は hard fail)、`strip_comments` を lexer 版へ、canary に self-test と配線 canary を追加、`budget` モードを ratchet に追加、check 6 を差し替え、check 番号を振り直し |
| `scripts/arch_lint_baseline.txt` | 変更 | ヘッダに budget 行の書式を追記 + 新規 baseline 164 行 |
| `Makefile` | 変更 | `arch-lint` の stale コメント修正、`PYTHON` を環境変数で渡す、python 不在時の帰結を明記 |
| `Cargo.toml` | 変更 | `[workspace.lints.clippy]` に `too_many_lines = "allow"` (関数長ゲートを一本化) |
| `CLAUDE.md` | 変更 | 不変条件 9 (`:459-460`) を書き換え、`:414` の SSoT 記述、`:419-421` の baseline 説明を補う |
| `.claude/skills/implement/SKILL.md` | 変更 | `:126` のチェック項目 |
| `.claude/skills/review/SKILL.md` | 変更 | `:77` の判定基準 |
| `.claude/skills/arch-review/SKILL.md` | 変更 | `:5` description、`:11` allowed-tools、`:30-34` の複製ワンライナー |
| `.claude/guards.jsonl` | 変更 | `:113-122` のコメントブロック。`strip_comments` が lexer になると「意味は同じ」が成り立たなくなるので、差分を明記する (§7.10) |
| `scripts/test_guards.py` | 変更 | `:478-483` と `:937-938` のコメント。上と同じ前提を書いている (§7.10)。**コードは触らない、コメントだけ** |
| `docs/plan_arch_refactor.md` | 変更 | `:466` の §11 記述、`:470` の skill 記述、`:10` / `:28` / `:32` / `:85-86` に注記 |
| `docs/plan_video_decode_unify.md` | 変更 | `:91` / `:131` の 3,000 行前提 |
| `docs/plan_rmd_77_arrangement_split.md` | 変更 | `:47-50` / `:288` / `:289` / `:1501-1502` / `:1588`。**次に着手される計画書なので、#76 着地と同時に false になる記述をここで直す** (§7.8) |
| `docs/plan_rmd_71_device_copy.md` | 変更 | `:49` / `:1317-1318` / `:1797` の 3,000 行前提 (着地済みなので注記のみ) |
| `docs/plan_rmd_73_automation_curve.md` | 変更 | `:1875` / `:1877` の 3,000 行前提 (着地済みなので注記のみ) |
| `docs/plan_rmd_74_disclosure_glyph.md` | 変更 | `:960` / `:962` / `:1049` の 3,000 行前提 (着地済みなので注記のみ) |
| `docs/plan_rmd_75_voicevox_phrase.md` | 変更 | `:1883` / `:1885` の 3,000 行前提 (着地済みなので注記のみ) |

**上の行番号は 2026-08-28 に実ファイルで照合済み。**ただし `docs/plan_rmd_77_arrangement_split.md`
は着手前まで成長し続ける (改訂を重ねている) ので、**触る直前に必ず下の grep を打ち直す**。

この一覧は次の grep の実測結果から作ってある (本計画書自身の行を除く):

```bash
grep -rnE "3,000|3000|god file budget|god-file budget|god-file|行数 budget" \
    docs/ CLAUDE.md .claude/ Makefile scripts/
```

> **確認用 grep のパターンは `3000` 単体まで広げること。** 初版は
> `"3,000 行|3000 行|god file budget|god-file budget"` で作っており、
> `docs/plan_arch_refactor.md:28` の「8 モジュール **≤3000** に分割」/ `:32` の
> 「5 モジュール **≤3000** 分割」/ `:470` の「行数 budget」を取りこぼしていた
> (この 3 か所は「当時の記録」なので書き換えないが、**検出できないこと自体が穴**である)。

実装後に上の広い grep を打ち、残っているのが本計画書と「当時の記録」の注記だけであることを確認する。

---

## 3. `scripts/loc_budget.py` (新規)

既存の `scripts/reuse_lint.py` / `scripts/lockfile_guard.py` と同じ流儀で書く:
先頭に「なぜこれが要るか」を書いた長い module docstring、`from __future__ import annotations`、
stdout/stderr の `reconfigure(encoding="utf-8", errors="replace")`、`ROOT = Path(__file__).resolve().parent.parent`、
`argparse`、`main() -> int` を `sys.exit(main())` で呼ぶ。
**`open()` / `read_text()` には必ず `encoding="utf-8"` を明示する** (Windows の既定は cp932)。
コメントは日本語、密度は既存スクリプトに合わせる (「何を」ではなく「なぜ」を書く)。

### 3.1 module docstring に書くこと

- 物理行 (`wc -l`) で測ると「テストを厚くすると分割を迫られる / doc を書くと分割を迫られる」
  逆インセンティブになること、それが `eefdea1` / `720e2c1` として実際に発火したこと。
- 数え方は SonarQube の ncloc と同一であること、閾値 1,000 は S104 の既定値であること。
- **なぜ lexer が要るか** (`common/src/project.rs:1815` の実例)。
- **なぜ bash ではなく Python か** (`scripts/arch_lint.sh:18-27` の argv バックスラッシュ消失)。
- **このスクリプトは行分類の SSoT でもあること** — `arch_lint.sh` の `strip_comments` が
  `--filter-comments` を呼ぶので、check 1/2/3/5/8 の「コメント内の言及は違反に数えない」も
  ここが決める。**write-time ガード (`.claude/guards.jsonl`) は Python の行正規表現なので、
  raw string / ブロックコメントでは判定が割れる** (割れる方向は「ガードの方が広く nudge」
  なので安全側。§7.10)。
- **走査が空になったら緑にせず落とすこと**、およびその理由 (§1.4 末尾)。
- **関数キーに trait 名と cfg 述語を含める理由** — 含めないと今日すでに 17 件衝突し、
  `#n` の連番で潰すと並べ替えで baseline の天井が入れ替わる (§0.2-12 / §3.7)。
- **生成物の判定を行数窓にしない理由** — `ara-sys/build.rs:47` の
  `.raw_line("// @generated …")` (§0.2-14 / §1.4)。
- 使い方 4 行:
  ```
  python scripts/loc_budget.py --check            # 違反を stdout へ (arch_lint.sh が読む)
  python scripts/loc_budget.py --report           # 上位ファイル / 関数の一覧 (人が読む)
  python scripts/loc_budget.py --self-test        # 判定器そのものの自己検査
  python scripts/loc_budget.py --filter-comments  # stdin の path:line:content からコメント行を落とす
  ```

### 3.2 定数

```python
FILE_NCLOC_BUDGET = 1000
FN_NCLOC_BUDGET = 300
FN_INDENT_BUDGET = 6
INDENT_WIDTH = 4                      # rustfmt の既定。tab はこの幅で展開する
GEN_MARKERS = ("automatically generated by", "@generated", "code generated")
# 生成マーカーを探す範囲は **行数窓ではなく「先頭の連続コメント/空行ブロック」** (§1.4)。
# 行数窓にすると ara-sys/build.rs:47 の .raw_line("// @generated …") に届いた瞬間、
# build.rs が生成物として走査から丸ごと落ちる。定数を持たないこと自体が仕様。
ALLOW_MARKER = "arch-lint: allow-"    # + "file-budget" / "fn-budget" / "fn-nesting"
MIN_SCANNED_FILES = 200               # 今日 389。半分を割ったら走査が壊れたと見なす (§1.4)
```

### 3.3 型

```python
@dataclass(frozen=True)
class Token:
    text: str    # 識別子 / 数値 / 記号 / "<lit>" (文字列・char) / "<lifetime>"
    line: int    # 0-based

@dataclass
class Lexed:
    rel: str
    lines: list[str]
    kind: list[str]            # 'code' | 'doc' | 'comment' | 'blank' (テスト判定前)
    indent: list[int | None]   # code 行のうち「行頭が literal/comment の継続でない」行だけ段数
    tokens: list[Token]

@dataclass
class FileMetrics:
    rel: str
    raw: int          # 物理行
    ncloc: int        # 実コード行
    doc: int
    comment: int
    blank: int
    test: int         # テスト範囲に入る行 (種別を問わず)

@dataclass
class FnMetrics:
    key: str          # "<rel>::<Scope>::<name>" (スコープが空なら "<rel>::<name>")。§3.7
    rel: str
    line: int         # 1-based。fn の宣言行
    attr_line: int    # 1-based。**その fn の最初の属性行**。無ければ line と同値。§3.8 が使う
    ncloc: int
    max_indent: int
    deep_lines: int   # indent >= FN_INDENT_BUDGET の行数。**ratchet の第 2 成分** (§3.9 / §4.3)
```

**保存則の内訳定義 (曖昧さを残さない。ここが曖昧だと初回から assert が落ちて
`make arch-lint` が全面停止する)**:

```
ncloc + doc + comment + blank + test == raw
```

**`raw` の定義 (これが無いと全ファイルで assert が落ちる)**:

- ファイルは **改行変換をせずに** 読む: `open(path, encoding="utf-8", newline="", errors="replace")`。
  `read_text()` / 既定の text mode は universal newlines なので、単独 CR (旧 Mac 改行) まで
  改行に化けて `wc -l` と食い違う。今日は単独 CR が 0 ファイルなので差は出ないが、
  **「今日は一致する」に依存しない**。
- `lines = src.split("\n")`。**末尾が改行で終わっていたら最後の空要素を 1 つだけ落とす**。
  `raw = len(lines)`。
  - 末尾改行ありのファイルでは `raw == wc -l`。末尾改行が無いファイルでは最後の不完全行も
    1 行と数える (`wc -l` より 1 多い) — これが物理行の正しい定義。今日は末尾改行の無い
    `.rs` が **0 ファイル**なので、`--report` の値は `wc -l` と完全に一致する
    (例: `daw_gui/src/view/track_inspector/mod.rs` の raw 2623 = `wc -l` の 2623)。
- **CRLF を含む `.rs` が 28 ファイルある** (§0.2-13)。`newline=""` で読むと各行の末尾に `\r` が
  残るので、**分類・インデント計測の前に各行の末尾 `\r` を 1 つだけ落とす**。
  lexer 側では `\r` は空白として読み飛ばすので、トークン列には影響しない。

- `test` = **テスト範囲 (§1.2) に入る物理行の総数**。種別 (code / doc / comment / blank) を問わない。
- `ncloc` / `doc` / `comment` / `blank` = **テスト範囲に入らない行だけ**を種別ごとに数えた数。
  4 つとも「テスト範囲を除いた数」であって、テスト行がどれかに二重計上されることは無い。
- テスト範囲が重なった場合 (入れ子の `#[cfg(test)]` 等) は行の集合として union を取る
  = 同じ行を 2 回数えない。

### 3.4 関数シグネチャ

```python
def lex(src: str) -> Lexed
def cfg_test_items(lx: Lexed) -> tuple[list[tuple[int, int]], list[tuple[str, int]], list[int]]
    # (テスト範囲 [start_line, end_line] のリスト,
    #  ("mod 名", 宣言行) のリスト,           # → submodule_candidates で解決する
    #  解決に失敗した宣言行のリスト)          # → UNRESOLVED-MOD
def fn_items(lx: Lexed) -> list[tuple[str, int, int, int, int]]
    # (qualified_key, attr_line, decl_line, body_open, body_close)  ※すべて 0-based
    # **attr_line を返すのは必須** — §3.8 の行内マーカーは「最初の属性行から本体の開き行まで」を
    # 探すので、decl_line から後方走査すると doc comment をどこまで遡るかが曖昧になる。
    # 属性が無い fn では attr_line == decl_line。
def scan_files() -> list[str]
def is_generated(lx: Lexed) -> bool   # 先頭コメント/空行ブロックに生成マーカー (§1.4)
def is_integration_test(rel: str) -> bool
def submodule_candidates(rel: str, name: str) -> list[str]

# **--check と --self-test が同じ経路を通るための分岐点**。
# 合成フィクスチャもリポジトリのファイルも、必ずこの関数を通す。
# 別実装で self-test を書くと「canary が検査対象と別物を試す」
# (scripts/arch_lint.sh:29-30 が実例つきで戒めている失敗) を再生産する。
def measure_source(rel: str, src: str, force_test: bool = False) -> FileScan
def measure(files: list[str]) -> list[FileScan]

def emit_check(scans: list[FileScan]) -> list[str]   # 出力行を **返す** (print しない)
def report(scans: list[FileScan]) -> int             # --report
def filter_comments(rows: Iterable[str]) -> Iterator[str]   # --filter-comments
def self_test() -> int
def main() -> int
```

`FileScan` は `FileMetrics` + `list[FnMetrics]` + 未解決 mod の行番号 + サブモジュール解決結果を
束ねた dataclass。**`emit_check` が文字列のリストを返す**ことが要点で、`--self-test` は
合成入力に対して同じ `emit_check` を呼び、出力行そのものを assert する (§3.12 (B))。

生成物の判定に lex が要る (§1.4) ので、順序は
**`scan_files()` (git の一覧) → 各ファイルを lex → `is_generated()` が真なら捨てる → 計測**。
`MIN_SCANNED_FILES` の下限判定は **生成物を除いた後の件数** に対して行う (今日 391 → 389)。

### 3.5 `lex()` の仕様 (この通りに実装する)

1 文字ずつ走査する状態機械。**外部ライブラリ禁止、正規表現は補助的に使ってよい。**

| 入力 | 扱い |
|---|---|
| `//` 〜 行末 | comment。ただし 3 文字目が `/` または `!` かつ `////` でないなら **doc** |
| `/* … */` | **ネストを数える** (`/* /* */ */` が 1 個の comment)。`/**` `/*!` は doc、ただし `/**/` は comment |
| `"…"` | 文字列。`\` エスケープを飛ばす。またがった行は全部 **code** |
| `b"…"` `c"…"` | 同上 |
| `r"…"` `r#"…"#` `br#"…"#` `cr#"…"#` (`rb` `rc` も) | raw 文字列。`#` の個数に対応する `"…#` まで。**中の `//` `/*` `{` `}` は一切解釈しない** |
| `'x'` `'\n'` `'\''` `'\u{1F600}'` | char リテラル |
| `'ident` | **ライフタイム** (閉じ `'` が無い)。char と取り違えない |
| 識別子 / 数値 | 1 トークン |
| 多文字記号 | `->` `=>` `::` `..` `..=` `<<` `>>` `<=` `>=` `==` `!=` `&&` は **1 トークン**として返す |
| その他の記号 | 1 文字 1 トークン |

> **`->` を 1 トークンにするのは必須**。1 文字ずつに割ると `fn f() -> bool {` の `>` が
> §3.7 の angle 深さを 1 減らし、本体の開き `{` を取り違える。
> `>>` は 1 トークンとして返したうえで、§3.7 の angle 深さでは `>` 2 個ぶんとして扱う
> (`Vec<Vec<u8>>` を正しく閉じるため)。

行分類 (相互排他。この順で決める):

```
code    … コメントに属さない非空白文字が 1 つ以上ある行 (文字列の中身を含む)
doc     … code でなく、doc comment の非空白文字がある行
comment … code でも doc でもなく、通常コメントの非空白文字がある行
blank   … 上のいずれでもない
```

`indent[i]` は **「その行の最初の非空白文字が、前の行から続く文字列リテラル / ブロック
コメントの内側ではない」code 行**にだけ入れる。これを守らないと `common/src/video_fx.rs` の
WGSL の字下げが関数のネスト段数に化ける。

### 3.6 `cfg_test_items()` の仕様

トークン列を走査し、`#` `[` … `]` (先頭 `#!` も許す) を属性として取り出す。

- 属性の中身は **トークンの text を連結**して作る。ただし `<lit>` は空文字に置換する
  (これで `#[cfg(feature = "test")]` を誤検出しない)。
- `^cfg\(` にマッチし、かつ `(?<![A-Za-z0-9_])test(?![A-Za-z0-9_])` を含むなら **テスト属性**。
  `cfg(all(test, windows))` / `cfg(any(test, …))` は意図的に拾う。
- テスト属性を見つけたら、**その属性の `#` がある行**を範囲の開始行として記録する (§1.2)。
  続く属性 (`#[...]`) をすべて読み飛ばし、item 本体の開始位置へ進む。
- **item の終端 (両義性を残さない書き方)**: 深さ 0 から始め、トークンを順に見る。
  - `(` `[` `{` … 深さを **+1** する。
  - `)` `]` `}` … 深さを **−1** する。**−1 した結果が 0 になった `}` がその item の終端**。
    その直後のトークンが `;` なら、その `;` の行までを範囲に含める
    (`use foo::{a, b};` のように `}` のあとに `;` が続く形)。
  - 深さが **0 のまま** `;` に当たったら、そこが終端 (`mod X;` / `use bar;` / `const X: u32 = 1;`)。
  > 「深さ 0 で `}` に当たったら終端」と書くと、減算**前**の深さを見る実装になり得る。
  > その場合 `mod tests { … }` は深さ 1 のまま `}` を見送ってファイル末尾まで走り、
  > **production コードが丸ごとテスト扱いになって budget が空振りする**。必ず減算後で判定する。
- item が `[pub[(…)]] mod NAME ;` の形なら `(NAME, 宣言行)` を第 2 返り値に積む。
  → `submodule_candidates(rel, NAME)` で解決する。
  - `rel` の stem が `mod` / `lib` / `main` なら `dir(rel)/NAME.rs` と `dir(rel)/NAME/mod.rs`
  - そうでなければ `dir(rel)/stem/NAME.rs` と `dir(rel)/stem/NAME/mod.rs`
  - repo に `#[path = "…"] mod` は 1 件も無い (§1.2 で実測確認済み) ので対応不要。ただし
    **将来 `#[path]` が入ったら黙って外すのではなく、第 3 返り値 (解決失敗行) に積んで
    `--check` が `UNRESOLVED-MOD` を 1 行出す** (§3.9)。
- 解決したファイルは **丸ごとテスト扱い** (`measure_source(..., force_test=True)`)。
  1 段で足りる (これらの中に更なる `mod X;` は無い)。

### 3.7 `fn_items()` の仕様

トークン列を走査しつつ brace 深さと **スコープスタック**を維持する。

- `mod NAME {` → `NAME` を push (`mod X;` は push しない)
- `trait NAME {` → `NAME` を push
- `impl` → 直後が `<` なら対応する `>` まで飛ばす (impl の generics 宣言)。その後、
  paren / bracket / angle の深さがすべて 0 の位置に `for` があれば **trait impl**、無ければ
  **固有 impl**。
  - 固有 impl → **`impl` (と generics) の直後の最初の識別子** (`dyn` / `mut` / `impl` は飛ばす)
    を型名として push
  - trait impl → **`for` の直後の最初の識別子**を型名とし、`Type[Trait]` を push。
    `Trait` は `impl` の generics 直後から `for` の直前までのトークン text を
    **空白なしで連結**したもの (generic 引数を含む)
- 対応する `}` で pop
- **関数 item の判定**: トークン `fn` の**次が識別子**であること。
  これで関数ポインタ型 `fn(u32) -> u32` を除外できる。
- **本体の開始 (両義性を残さない書き方)**: `fn NAME` の直後から走査し、3 つの深さを持つ。
  - `paren` … `(` で +1、`)` で −1
  - `bracket` … `[` で +1、`]` で −1
  - `angle` … `<` の**直前トークンが 識別子 / `::` / `>` のときだけ** +1。`>` で −1
    (`>>` は `>` 2 個ぶん)。`->` は 1 トークンなので影響しない (§3.5)。
  - **3 つとも 0 のときに現れた最初の `{` が本体の開き**。
  - 3 つとも 0 のときに `;` に当たったら本体無し (trait / extern の宣言) とみなして捨てる。
  - `bracket` を数えるので `fn f<const N: usize>() -> [u8; { N * 2 }]` の `{` を誤って
    本体と見なすことは無い。`angle` を数えるので `Foo<{ N }>` も巻き込まない。
- **本体の終端**: 開き `{` から `{` +1 / `}` −1 と数え、**−1 した結果が 0 になった `}`**。
**cfg 修飾 (キーを一意かつ安定にするための必須要素)**:

- スコープを作る item (`mod` / `trait` / `impl`) と `fn` 自身について、**その item に付いている
  `#[cfg(…)]` 属性を正規化してキーに含める**。形は `NAME[cfg(...)]` /
  `Type[Trait][cfg(...)]` / `name[cfg(...)]`。`cfg(...)` の中身は属性トークンの text を
  **空白なしで連結**したもの (`#[cfg(not(windows))]` → `cfg(not(windows))`)。
  複数の cfg 属性が付いていたら `,` で連結する。
- `cfg` 以外の属性 (`#[allow(...)]` / `#[derive(...)]` / doc 属性) は **キーに入れない**。
- **cfg が付いていない item には何も足さない** (今日のキーの大多数は素のまま)。

**キーの一意性 — `#2` の連番は使わない**:

- key = `f"{rel}::" + "::".join(scope) + f"::{name}"`。**スコープが空なら `f"{rel}::{name}"`**。
- **repo 全体で key が一意であること。衝突したら `#2` を付けて黙って潰すのではなく、
  `KEY-COLLISION` として `--check` の出力に 1 行出す** (§3.9)。
  連番は出現順に依存するので、無関係な並べ替えで baseline の天井が入れ替わる —
  `scripts/arch_lint.sh:91-92` が「行番号ではなく内容ハッシュ」にした理由と同じ故障を、
  キー側に持ち込むことになる。衝突は **測定のバグか、キー規則で表現できない構造**なので、
  `UNRESOLVED-MOD` と同じく人に見せて判断させる。
- **今日の実測**: trait / cfg の修飾が無いと衝突キー **17 件** (§0.2-12)。
  上の 2 つの修飾を入れると **0 件** (関数 6,246 本)。独立実装で確認済み。
  - `common/src/shmem.rs::imp[cfg(windows)]::NamedShmem::create` と
    `common/src/shmem.rs::imp[cfg(unix)]::NamedShmem::create`
  - `daw_gui/src/handler/devices.rs::AppData::cleanup_slot_gui[cfg(windows)]` と
    `…::cleanup_slot_gui[cfg(not(windows))]`
  - `daw_gui/src/midi_import.rs::MidiImportError[From<std::io::Error>]::from` と
    `…::MidiImportError[From<midly::Error>]::from`
- **自由関数 (impl / trait / mod の外) のスコープは空**。実コードで確認済みの実例:
  `daw_gui/src/view/track_inspector/mod.rs:228` の `pub fn draw` は同ファイルに `impl` が
  1 つも無い自由関数なので、key は `daw_gui/src/view/track_inspector/mod.rs::draw`。
  `view/audio_editor.rs:139` / `view/arrangement_view.rs:58` / `view/transport.rs:263` の
  `draw`、`view/root.rs:720` の `dispatch_shortcuts` も同じく自由関数。
  key の先頭にファイル名が入っているので、別ファイルの同名 `draw` 同士は衝突しない。
- **キーに空白を入れない** (§3.9 の分解規則)。trait 名も cfg 述語も空白なし連結なので、
  この規則は自動的に満たされる。生成したキーに空白が混ざっていたら **exit 2 で落とす**
  (パスの空白と同じ扱い)。

関数の指標:

- `ncloc` = 本体の開き行〜閉じ行のうち `kind == 'code'` かつテスト範囲に入っていない行数
- `max_indent` = 同じ範囲の `indent[i]` (None を除く) の最大
- `deep_lines` = 同じ範囲で `indent[i] >= FN_INDENT_BUDGET` の行数

> **既知の差分 (実装者向け)**: r.md #77 は `run.rs::arrangement` の「6 段以上が 562 行」と
> 書いているが、この定義 (行頭が literal/comment の継続でない code 行のみ) では **520** に
> なる。差の 42 行は継続行・文字列内の行。ゲートに使う `max_indent` は 11 で一致するので
> 判定は変わらない。**520 が出ても壊れていない**。

### 3.8 行内マーカー

`scripts/arch_lint.sh:36-42` の慣習をこの検査にも広げる (`strip_allowed` は shell 側の
grep 実装なので使えない。Python 側で落とす)。

- `FILE-BUDGET`: ファイル内のどこかの行に `arch-lint: allow-file-budget` を含む → 除外
- `FN-BUDGET` / `FN-NESTING`: **その関数の `attr_line` から `body_open` まで**のいずれかに
  `arch-lint: allow-fn-budget` / `arch-lint: allow-fn-nesting` を含む → 除外。
  `attr_line` は `fn_items()` の第 2 返り値 (§3.4)。**宣言行から後方走査して属性を探す実装に
  しないこと** — doc comment をどこまで遡るかが曖昧になり、上にある無関係な関数の
  マーカーを拾い得る。範囲は `fn_items()` が決めた 1 か所だけが権威。

baseline (= 既知の負債。直す予定がある) との違いを docstring に明記すること。

### 3.9 `--check` の出力

1 違反 1 行。**第 1 field = CHECK 名**。以降は CHECK ごとに 2 形式ある。

**budget 形式** (`FILE-BUDGET` / `FN-BUDGET` / `FN-NESTING`):
第 2 field = キー、第 3 field = **計測値 (`/` 区切りの整数ベクトル)**、以降は人間向け。
第 2・第 3 field に空白を入れてはいけない (arch_lint.sh がパラメータ展開で分解する)。
パスに空白が含まれていたら **エラーで落とす** (現在 0 件であることは確認済み)。

**grep 形式** (`UNRESOLVED-MOD` / `KEY-COLLISION`): 第 2 field 以降が `path:line:content`。
既存の `grep` モードにそのまま流せるので、新しい書式を増やさない。

```
FILE-BUDGET daw_gui/src/app.rs 1993 ncloc>1000 (raw 2424 / doc 15 / comment 282 / test 0)
FN-BUDGET daw_gui/src/app.rs::AppData::handle_event 1623 ncloc>300 @daw_gui/src/app.rs:518
FN-NESTING daw_gui/src/widgets/arrangement/run.rs::arrangement 11/520 indent>6 @daw_gui/src/widgets/arrangement/run.rs:7
UNRESOLVED-MOD daw_gui/src/lib.rs:19:#[cfg(test)] mod app_tests;
KEY-COLLISION common/src/shmem.rs:191:key collision: common/src/shmem.rs::imp::NamedShmem::create
LOC-BUDGET-OK 389 files / 74 test / 2 generated / 0 key-collision / 0 unresolved-mod
```

> **`0 key-collision` は §3.7 の trait / cfg 修飾を入れた場合の値**で、実測確認済み。
> 修飾を入れずに実装すると **17 件出る** (§0.2-12)。17 が出たら「判定器が壊れた」のではなく
> **§3.7 の修飾を実装し忘れている**。ここを取り違えないよう `KEY-COLLISION` は
> ファイル名と行番号つきで出す。

- **`FN-NESTING` の計測値は `max_indent/deep_lines` の 2 成分**。理由: 天井を `max_indent`
  だけにすると、深さ 9 の関数が「6 段以上の行」を 20 行から 500 行に増やしても
  `max_indent` は 9 のまま = 緑になり、§4.3 冒頭で塞いだはずの
  「baseline 済みが無制限に太れる」穴が FN-NESTING にだけ残る。
  `FILE-BUDGET` / `FN-BUDGET` は 1 成分 (`ncloc` だけ) でよい。
- **最終行の `LOC-BUDGET-OK …` は必須**。arch_lint.sh はこれが無ければ SELF-BROKEN で落ちる
  (「出力が空」を「違反ゼロ」と取り違えないため = このリポジトリが最も警戒する false green)。
- exit code は **違反があっても 0**。ratchet の判定は arch_lint.sh が持つ。
  内部エラーのときだけ **2** で落とす。2 で落とす条件は次の 5 つ:
  1. `git ls-files` が非ゼロ終了 / git が起動できない (§1.4)
  2. 走査ファイル数 (生成物を除いた後) が `MIN_SCANNED_FILES` 未満 (§1.4)
  3. 保存則違反 (§3.3。ファイル名と内訳を stderr に出す)
  4. パスに空白が含まれる
  5. 生成した関数キーに空白が含まれる (§3.7)

`UNRESOLVED-MOD` について: `#[cfg(test)] mod X;` が解決できないと、そのテストファイルは
**production として課金される** = 測定のバグであって「既知の負債」ではない。だから
grep 形式 (= 行内容ハッシュの fingerprint) で ratchet に載せ、**直す (= `#[path]` に対応する)
か、理由を書いて baseline に載せるか**を人に選ばせる。今日は 0 件。
CHECK 名は python 側も baseline 側も `UNRESOLVED-MOD` で統一する (別名を作らない)。

`KEY-COLLISION` について: 同じ扱い。**キーが衝突すると 2 関数が 1 つの天井を共有し、
片方の違反が黙って消える**ので、これも「既知の負債」ではなく測定のバグ。
出力は衝突した **2 件目以降**の宣言行を指す grep 形式にして、`content` に
`key collision: <key>` と書く。今日は 0 件 (§3.7)。
**exit 2 で落とさない**のは、衝突が起きても他の違反の報告は正しいままだから —
落とすと全検査が止まって、直すべき違反が見えなくなる。ratchet に載せて赤くする方が強い。

### 3.10 `--filter-comments` の仕様 (`strip_comments` の中身)

stdin から `path:line:content` の行を読み、**その行が `doc` または `comment` に分類される
なら落とし、それ以外は素通しする**フィルタ。stdout へ書く。

- `path` は relative。`path` にコロンは無い前提 (`scripts/arch_lint.sh:12-13` の既存前提)。
  先頭 2 つの `:` で分解する。
- 同じ `path` を何度も lex しないよう **path 単位でキャッシュ**する。
- **読めない `path` / 範囲外の `line` の行は「落とさずに残す」**。加えて stderr に 1 行警告。
  → 迷ったら違反を表に出す側に倒す。黙って捨てると false negative になる。
- 行が `path:line:content` の形をしていなければ、そのまま素通しする (壊さない)。
- stdin も `encoding="utf-8", errors="replace"` で読む。
- exit 0 固定 (フィルタなので、判定は呼び出し側)。

この 1 か所に寄せることで、**「コメント内の言及は違反に数えない」の定義がリポジトリで 1 つ**になる。

### 3.11 `--report` の出力 (arch-review skill が読む)

```
loc-budget: 389 ファイル (テスト扱い 74 / 生成物 2 除外 / キー衝突 0 / 未解決 mod 0)
  budget: ファイル 1,000 実コード行 / 関数 300 実コード行 / インデント 6 段

--- ファイル 実コード行 上位 20 ---
 ncloc    raw    doc   comment  blank   test  path
  2214   2623     30      286     93      0  daw_gui/src/view/track_inspector/mod.rs
  ...
--- 関数 実コード行 上位 20 ---
 ncloc  indent  path:line  key
  ...
--- 関数 最大インデント 上位 20 ---
 indent  >=6行  ncloc  path:line  key
  ...
```

exit 0 固定。ただし §1.4 の走査下限・保存則に引っかかったら `--check` と同じく exit 2。

### 3.12 `--self-test` の仕様

**合成フィクスチャだけで判定器を検証する** (リポジトリの内容に依存させない。
依存させると「repo が変わった」のか「判定器が壊れた」のか区別できなくなる)。
`scripts/lockfile_guard.py:97-127` の `self_test()` と同じ形 — 期待値と一致しなければ
stderr へ理由を書いて 1 を返す。

**フィクスチャは必ず `measure_source()` → `emit_check()` を通す** (§3.4)。
別実装で assert すると canary が検査対象と別物を試すことになる。

#### (A) 分類器の検査 (各フィクスチャで保存則 §3.3 も確認する)

| # | フィクスチャ | 期待 |
|---|---|---|
| 1 | `let s = r#"// これはコメントではない { "#;` | comment 0、code、brace 深さ不変 |
| 2 | `#[cfg(test)] mod t { fn a(){ let s = "not valid json {"; } }` の後に production コード | test 範囲が `}` で正しく閉じ、後続が code (project.rs:1815 型) |
| 3 | `/* /* ネスト */ まだコメント */ let x = 1;` | 最終行は code、途中行は comment |
| 4 | `#[cfg(test)] pub(crate) fn f() { … }` | mod でなくても test 扱い (compile.rs:900 型) |
| 5 | `#[cfg(test)]\nmod tests;` | 第 2 返り値に `("tests", 行番号)` が入る |
| 6 | `fn f<'a>(x: &'a str) -> char { '}' }` | `'a` はライフタイム、`'}'` は char。深さ不変 |
| 7 | `#[cfg(feature = "test")] fn f(){}` | **test 扱いにしない** |
| 8 | `type F = fn(u32) -> u32;` / `trait T { fn g(); }` | 関数 item として拾わない |
| 9 | `//// 4 本` / `/// doc` / `//! inner` / `/** doc */` | 1 つ目は comment、残りは doc |
| 10 | `b"…"` `c"…"` `br#"…"#` | 文字列として閉じる |
| 11 | 同名 `fn new` を 2 つの `impl` に持つ入力 | key が `A::new` / `B::new` に分かれる |
| 12 | 深さ 8 の入れ子 + 内部に raw string (行頭が字下げされた WGSL) | `max_indent` が raw string に汚染されない |
| 13 | `arch-lint: allow-fn-nesting` を付けた深い関数 | 違反として出さない |
| 14 | `#[cfg(test)] use foo::{a, b};` の後に production コード | `}` の直後の `;` まで範囲、後続は code |
| 15 | `fn f() -> bool { … }` / `fn g<T: Into<String>>(x: T) { … }` | `->` と `>>` で本体の開きを取り違えない |
| 16 | impl の外の自由関数 `pub fn draw(){}` | key が `<rel>::draw` (スコープ空) |
| 17 | `impl<'a, M: ?Sized + 'static> Ui<'a, M> { pub fn f(){} }` | key が `<rel>::Ui::f` (text_input.rs:181 型) |
| 18 | `impl From<std::io::Error> for E { fn from(){} }` と `impl From<midly::Error> for E { fn from(){} }` | key が `E[From<std::io::Error>]::from` / `E[From<midly::Error>]::from` に**分かれる** (midi_import.rs:120,128 型)。衝突 0 |
| 19 | `#[cfg(windows)] mod imp { impl T { fn f(){} } }` と `#[cfg(unix)] mod imp { impl T { fn f(){} } }` | key が `imp[cfg(windows)]::T::f` / `imp[cfg(unix)]::T::f` に分かれる (shmem.rs:32,170 型)。衝突 0 |
| 20 | `#[cfg(debug_assertions)] fn g(){}` と `#[cfg(not(debug_assertions))] fn g(){}` | key が `g[cfg(debug_assertions)]` / `g[cfg(not(debug_assertions))]` (main.rs:831,848 型)。**どちらも test 扱いにしない** |
| 21 | `#[allow(clippy::too_many_lines)] fn h(){}` | key に `[allow(...)]` が**入らない** (cfg 以外の属性はキーに入れない) |
| 22 | 同じキーになる関数を 2 本含む入力 (修飾を無効化した合成) | `KEY-COLLISION` が 1 本出る。**`#2` の連番を付けて黙らせない** |
| 23 | CRLF 改行のファイル / 末尾改行の無いファイル | `raw` が §3.3 の定義どおり (`wc -l` と一致 / 不完全行も 1 行) |
| 24 | 先頭が `/* automatically generated by … */` の入力 | 生成物として除外される |
| 25 | 先頭にコメントが無く、コード中の文字列に `// @generated by …` を含む入力 (ara-sys/build.rs:47 型) | **除外しない** (先頭コメントブロック規則が効いている証明) |

#### (B) **肯定側 canary — `emit_check` が実際に違反を出すこと**

`scripts/arch_lint.sh:44-61` の canary は肯定側 (「検査が実際に違反を捕まえる」) と
否定側 (「除外マーカーが効く」) を両方持っている。同じ強度を持たせる。
**これが無いと `emit_check` が壊れても `LOC-BUDGET-OK` 行は出るので §4.4 の完走マーカー検査を
通り、baseline 164 行が全部「解消」になって exit 0** = この計画が最も警戒する false green が
新モードに開く。

| # | 合成入力 | 期待 |
|---|---|---|
| 26 | 実コード **1,001** 行のファイル | `FILE-BUDGET` の行が **ちょうど 1 本**、計測値 `1001` |
| 27 | 実コード **1,000** 行のファイル | `FILE-BUDGET` の行が **0 本** (境界は違反にしない) |
| 28 | 実コード **301** 行の関数 | `FN-BUDGET` の行が **ちょうど 1 本**、計測値 `301` |
| 29 | 実コード **300** 行の関数 | `FN-BUDGET` の行が **0 本** |
| 30 | インデント **7 段**を含む関数 | `FN-NESTING` の行が **ちょうど 1 本**、計測値が `7/<行数>` |
| 31 | インデント **6 段**までの関数 | `FN-NESTING` の行が **0 本** |
| 32 | `#[cfg(test)] mod nonexistent;` | `UNRESOLVED-MOD` の行が **ちょうど 1 本** |
| 33 | 何も違反しない入力 | 出力が `LOC-BUDGET-OK …` の 1 行だけ |
| 34 | `arch-lint: allow-fn-budget` を **属性行**に付けた 301 行の関数 | `FN-BUDGET` の行が **0 本** (§3.8 の範囲が `attr_line` から始まっている証明) |

#### (C) `--filter-comments` の検査

| # | 入力行 | 期待 |
|---|---|---|
| 35 | raw string 中の行頭 `//` を指す行 | **残る** (現行 `strip_comments` はここを落としていた) |
| 36 | 本物の行頭 `//` コメント行を指す行 | 落ちる |
| 37 | `/* … */` の中の行を指す行 | 落ちる (現行は落とせていなかった) |
| 38 | 末尾コメント付きの実コード行 (`foo(); // x`) を指す行 | 残る |
| 39 | 存在しない path を指す行 | **残る** (迷ったら表に出す側) |
| 40 | `path:line:content` の形をしていない行 | **そのまま素通し** |

#### (D) 走査の下限

| # | 検査 | 期待 |
|---|---|---|
| 41 | `scan_files()` の結果が `MIN_SCANNED_FILES` 未満になるよう注入 | exit 2 相当の例外。緑にしない |
| 42 | `git ls-files` が非ゼロ終了するよう注入 | exit 2 相当の例外。空リストへ縮退しない |

成功時は `loc-budget: self-test ok (フィクスチャ N 件 / 保存則 N 件)` を stdout に出す。

**加えて、`--check` / `--report` の実行中は全ファイルで保存則を assert する** (壊れた lexer が
偶然この等式を満たすことはまず無いので、「違反ゼロ」の信用を毎回自己証明できる)。
違反したらファイル名と内訳を stderr に出して exit 2。

---

## 4. `scripts/arch_lint.sh` の変更

### 4.1 python の解決 + `strip_comments` の置き換え (現 :14-16 の位置)

`Makefile:6` の `PYTHON ?=` は **make 変数**なので、そのままでは script に渡らない。
自前で解決し、**見つからなければ hard fail** させる (`Makefile:203-206` が書いている
「未インストールにつき skip の緑は作らない」原則。実装は `Makefile:213` の cargo-deny)。

```sh
# scripts/loc_budget.py を起動する python。Makefile:6 の PYTHON は **make 変数**なので、
# make 経由でも直接起動でも効くよう、環境変数 PYTHON を優先しつつ自前でも探す。
# **見つからなければ落とす** — 検査だけ黙って消えるのは、このファイルが一番警戒している
# false green (「緑だが検査が効いていない」) そのもの。
# なお strip_comments も loc_budget.py に依存するので、python が無い / 壊れていると
# **checks 1-12 が全部止まる**。これは意図的 (cargo-deny と同じ扱い)。
PY="${PYTHON:-}"
[ -n "$PY" ] || PY="$(command -v python 2>/dev/null || true)"
[ -n "$PY" ] || PY="$(command -v python3 2>/dev/null || true)"
if [ -z "$PY" ]; then
    printf 'arch-lint: [SELF-BROKEN] python が見つかりません。arch-lint を実行できません。\n' >&2
    printf '  make arch-lint PYTHON=/path/to/python3 か、PATH を通してください。\n' >&2
    exit 1
fi

# grep -Hn 出力 `path:line:content` から、**その行が実際にコメント (doc 含む) である**
# 行を落とす。行頭 `//` を見るだけの近似だった頃は 2 方向に間違えていた:
#   - raw string 中の行頭 `//` をコメントと誤判定して落とす = 違反の取りこぼし
#   - `/* … */` の中の違反パターンを落とせない = コメント内の言及を違反に数える
# 行分類の SSoT は scripts/loc_budget.py の lexer 1 か所。**パターンを shell に持たせない**。
# r.md #76。
strip_comments() { "$PY" scripts/loc_budget.py --filter-comments; }
```

### 4.2 canary に配線 canary と self-test を足す

**self-test は Makefile ではなく arch_lint.sh の canary ブロックに置く。**
`.claude/skills/arch-review/SKILL.md:26` も手作業も `bash scripts/arch_lint.sh` を直接叩くので、
Makefile にだけ置くと make を経由しない全経路で自己検証が消える (= 塞いだつもりの穴が
別の入口に残る)。`make arch-lint` は arch_lint.sh を呼ぶので「本検査の前に self-test が
必ず走る」という要件はそのまま満たされ、**満たされる経路が増える**。

置き場所は **`if [ "$canary_ok" -ne 1 ] … fi` (現 :62-67) の直後**。
grep 由来の故障を先に報告させたいので、正規表現 canary の判定より後ろに置く。
**`canary_ok=0` に合流させない** — 現 :62-66 のメッセージは
「検査器の正規表現が効いていません / この環境の grep に既知のパターンが通りませんでした」で、
python の不在・破損を **grep のせい**と報告してしまう。原因の違う 2 つを 1 つの出口に
まとめると、次に踏んだ人が grep を疑って時間を溶かす。専用の出口を持たせる:

```sh
# (3) strip_comments (lexer 版) の配線 canary。**repo の内容に依存させない** —
#     読めないパスの行は「落とさずに残す」契約なので、これが消えたら配線が壊れている
#     (= 違反を黙って捨てる方向の故障)。分類そのものの正しさは loc_budget.py --self-test。
#     **grep 用の canary_ok に合流させない**: ここが落ちる原因は python 側なので、
#     「grep にパターンが通らない」というメッセージを出すと診断が別方向へ逸れる。
if ! printf 'selftest/does_not_exist.rs:1:    pool: HashMap<(u32, u32), Bogus>,\n' \
        | strip_comments 2>/dev/null | grep -q .; then
    printf 'arch-lint: [SELF-BROKEN] strip_comments (scripts/loc_budget.py --filter-comments) が\n' >&2
    printf '  読めないパスの行を素通しできていません。python 側の配線が壊れています。\n' >&2
    printf '  PY=%s\n' "$PY" >&2
    exit 1
fi
```

続けて (同じく現 :62-67 の直後、バックスラッシュ NOTE (現 :68-74) の前):

```sh
# サイズ budget の判定器 (loc_budget.py) も、上の正規表現 canary と同格で自己検証する。
# **「出力が空 = 違反ゼロ」を信じないための土台**なので、失敗したら即 exit 1。
if ! _st="$("$PY" scripts/loc_budget.py --self-test 2>&1)"; then
    printf 'arch-lint: [SELF-BROKEN] loc_budget.py の self-test が落ちました。\n' >&2
    printf '%s\n' "$_st" >&2
    exit 1
fi
```

### 4.3 `budget` モードを ratchet に足す

**なぜ新モードが要るか**: 現行の `firstfield` モードは fingerprint の content が空なので、
ハッシュは常に `SHA1('') = da39a3ee5e6b` になり **実質 path のみがキー**になる。
つまり baseline に載せたファイルは 5,000 行まで太っても緑のまま = `scripts/arch_lint.sh:88-89`
が禁じている「件数 baseline」と同じ穴が、1 ファイル内の成長方向に開く。
そこで baseline 行の第 3 field に **ハッシュではなく計測値 (天井)** を持たせ、
**計測値 > 天井なら新規違反**とする。

- 計測値は **`/` 区切りの整数ベクトル**。`FILE-BUDGET` / `FN-BUDGET` は 1 成分、
  `FN-NESTING` は `max_indent/deep_lines` の 2 成分 (§3.9)。**成分ごとに比較**し、
  1 成分でも天井を超えたら新規違反。
- 縮んだときは緑のまま (`scripts/arch_lint.sh:84-86` /「良い変更を止めない」)。
- 天井は **今日の実測値そのもの**にする (余裕を持たせない)。不変条件 9 の文言
  「超過したら分割してから足す」を字義通りに強制できる = 既知の負債は 1 行も太れない。
- 天井を上げたいときは baseline 行を人が編集して理由を書く。

#### 4.3.1 `record` のヘッダコメント (現 :111-118) に mode を追記

```
#   mode = grep       … hits が `path:line:content` 形式 (既定)
#          firstfield … hits の第 1 field が path
#          budget     … hits が `key value 人間向けの説明…`。value は `/` 区切りの整数
#                       ベクトル。baseline の第 3 field を **ハッシュではなく天井**として
#                       読み、成分ごとに比較して 1 つでも超えたら新規違反。
```

#### 4.3.2 `baseline_ceiling` と `budget_le` を足す (`contains` の隣 = 現 :130-136 の後)

```sh
# baseline_ceiling <CHECK> <key> — budget 行の天井 (整数ベクトル) を取り出す。無ければ空。
baseline_ceiling() {
    while IFS= read -r _bk; do
        case "$_bk" in "$1|$2|"*) printf '%s' "${_bk##*|}"; return 0 ;; esac
    done <<EOF
$BASEKEYS
EOF
    return 1
}

# budget_le <value> <ceiling> — `/` 区切りの整数ベクトルを成分ごとに比較する。
# 全成分が天井以下なら 0。**成分数が違う / 数字でない成分がある場合は 1** (= 新規違反)。
# 書き間違えた baseline 行を「天井無し」として黙って通さないため。
budget_le() {
    _v="$1"; _c="$2"
    while [ -n "$_v" ] || [ -n "$_c" ]; do
        [ -n "$_v" ] || return 1
        [ -n "$_c" ] || return 1
        _v1="${_v%%/*}"; _c1="${_c%%/*}"
        case "$_v1" in ''|*[!0-9]*) return 1 ;; esac
        case "$_c1" in ''|*[!0-9]*) return 1 ;; esac
        [ "$_v1" -le "$_c1" ] || return 1
        if [ "$_v" = "$_v1" ]; then _v=""; else _v="${_v#*/}"; fi
        if [ "$_c" = "$_c1" ]; then _c=""; else _c="${_c#*/}"; fi
    done
    return 0
}
```

#### 4.3.3 `classify` の分類ループ (現 :168-181) に分岐を足す

`split_row` の直後、`_key=` を作る前に:

```sh
        if [ "$ROW_MODE" = "budget" ]; then
            _bkey="${ROW_LINE%% *}"
            _brest="${ROW_LINE#* }"
            _bval="${_brest%% *}"
            _ceil="$(baseline_ceiling "$ROW_CHECK" "$_bkey")"
            _blkey="$ROW_CHECK|$_bkey|$_ceil"
            if [ -n "$_ceil" ] && budget_le "$_bval" "$_ceil"; then
                SEEN="$SEEN$_blkey$NL"
                N_BASE=$((N_BASE + 1))
            else
                # **天井を超えたときも SEEN に積む** (baseline 行が存在する場合)。
                # 積まないと下の「解消」ループ (現 :214-222) が同じ baseline 行を
                # 「削除してよい」と案内する = **太ったファイルに対して天井を消せと言う**
                # ことになり、案内どおりに消すと次回から無検査になる。
                # 「解消 (もう違反していない)」と「超過 (天井を突破した)」は別の事象なので、
                # SEEN への積み方で分離する。
                [ -z "$_ceil" ] || SEEN="$SEEN$_blkey$NL"
                NEW="$NEW$_row$NL"
                N_NEW=$((N_NEW + 1))
            fi
            continue
        fi
```

解消の通知ループ (現 :214-222) は BASEKEYS と SEEN の比較なのでそのまま動く
(閾値未満に縮んだファイルは `--check` が出さなくなり、SEEN に入らないので「解消」として
通知される。天井を超えたファイルは SEEN に入るので「解消」には出ず、「新規違反」にだけ出る)。

#### 4.3.4 ratchet の self-test を拡張 (現 :242-249 の直後)

budget モードにも同じ強度の自己検証を置く。**これが無いと新モードだけ無検証になる**。

```sh
_budget_broken=0
_bk_save="$BASEKEYS"

BASEKEYS="SELFTEST-BUDGET|selftest/a.rs|100$NL"
classify 'SELFTEST-BUDGET budget selftest/a.rs 100 ncloc' 1
{ [ "$N_BASE" -eq 1 ] && [ "$N_NEW" -eq 0 ] && [ "$N_RESOLVED" -eq 0 ]; } || _budget_broken=1
classify 'SELFTEST-BUDGET budget selftest/a.rs 101 ncloc' 1
# 超過は「新規違反」に出て、かつ「解消」には出ない (baseline 行を消せと案内しない)
{ [ "$N_NEW" -eq 1 ] && [ "$N_RESOLVED" -eq 0 ]; } || _budget_broken=1
classify 'SELFTEST-BUDGET budget selftest/a.rs 99 ncloc' 1
{ [ "$N_NEW" -eq 0 ] && [ "$N_BASE" -eq 1 ]; } || _budget_broken=1   # 縮んだら緑
classify 'SELFTEST-BUDGET budget selftest/z.rs 1 ncloc' 1
{ [ "$N_NEW" -eq 1 ] && [ "$N_RESOLVED" -eq 1 ]; } || _budget_broken=1   # 未記録は必ず新規

BASEKEYS="SELFTEST-NEST|selftest/b.rs::f|7/20$NL"
classify 'SELFTEST-NEST budget selftest/b.rs::f 7/20 indent' 1
{ [ "$N_BASE" -eq 1 ] && [ "$N_NEW" -eq 0 ]; } || _budget_broken=1
classify 'SELFTEST-NEST budget selftest/b.rs::f 7/21 indent' 1
# 深さ据え置きで「6 段以上の行」だけ増えても新規違反になること (FN-NESTING の肥大検出)
{ [ "$N_NEW" -eq 1 ] && [ "$N_RESOLVED" -eq 0 ]; } || _budget_broken=1
classify 'SELFTEST-NEST budget selftest/b.rs::f 8/20 indent' 1
{ [ "$N_NEW" -eq 1 ] && [ "$N_RESOLVED" -eq 0 ]; } || _budget_broken=1
classify 'SELFTEST-NEST budget selftest/b.rs::f 6/5 indent' 1
{ [ "$N_NEW" -eq 0 ] && [ "$N_BASE" -eq 1 ]; } || _budget_broken=1
classify 'SELFTEST-NEST budget selftest/b.rs::f 7/x indent' 1
{ [ "$N_NEW" -eq 1 ]; } || _budget_broken=1   # 数字でない成分を天井無し扱いにしない

BASEKEYS="$_bk_save"
if [ "$_budget_broken" = "1" ]; then
    printf 'arch-lint: [SELF-BROKEN] budget モードの ratchet が壊れています。\n' >&2
    exit 1
fi
```

(`set -u` が効いているので `_budget_broken=0` の初期化を忘れないこと。
`classify` は毎回 `SEEN` / `NEW` / カウンタを作り直すので、最後の本番
`classify "$HITS" 0` には影響しない。)

#### 4.3.5 `EMIT_BASELINE` (現 :316-327) に budget 分岐

```sh
            if [ "$ROW_MODE" = "budget" ]; then
                _bkey="${ROW_LINE%% *}"
                _brest="${ROW_LINE#* }"
                printf '%s | %s | %s | 理由 / いつ消えるか\n' "$ROW_CHECK" "$_bkey" "${_brest%% *}"
            else
                _k="$(fingerprint "$ROW_MODE" "$ROW_LINE")"
                printf '%s | %s | %s | 理由 / いつ消えるか\n' "$ROW_CHECK" "${_k%%|*}" "${_k##*|}"
            fi
```

### 4.4 check 6 の差し替え (現 :284-290 を丸ごと置換)

```sh
# 6-10. サイズ budget (不変条件 9)。**測り方の SSoT は scripts/loc_budget.py**
#      (Rust の字句解析。実コード行だけを数え、テスト / doc / コメント / 空行は数えない)。
#      物理行 (wc -l) で測っていた頃は「テストを厚くすると分割を迫られる /
#      doc を書くと分割を迫られる」逆インセンティブになっていて、実際に tests を別ファイルへ
#      移すだけの commit が 2 件生えた (eefdea1 / 720e2c1)。r.md #76。
#      **パターンを shell に持たせない** — 冒頭に書いた argv バックスラッシュ消失の
#      地雷原に戻らないため。ここは python の stdout を読むだけ。
_budget_rc=0
budget_out="$("$PY" scripts/loc_budget.py --check 2>&1)" || _budget_rc=$?
if [ "$_budget_rc" -ne 0 ]; then
    printf 'arch-lint: [SELF-BROKEN] loc_budget.py が exit %s で落ちました。\n' "$_budget_rc" >&2
    printf '  走査が空 / 保存則違反 / git 失敗のいずれか。**緑にはしません**。\n' >&2
    printf '%s\n' "$budget_out" >&2
    exit 1
fi
case "$NL$budget_out$NL" in
    *"${NL}LOC-BUDGET-OK "*) : ;;
    *)  printf 'arch-lint: [SELF-BROKEN] loc_budget.py が完走マーカーを出しませんでした。\n' >&2
        printf '  出力が空でも「違反ゼロ」とは判定しません。\n' >&2
        printf '%s\n' "$budget_out" >&2
        exit 1 ;;
esac

# pick <CHECK> — budget_out から該当行を取り出し、先頭の CHECK 名を落とす。
pick() {
    while IFS= read -r _l; do
        case "$_l" in "$1 "*) printf '%s\n' "${_l#* }" ;; esac
    done <<EOF
$budget_out
EOF
}

record FILE-BUDGET budget "実コード 1,000 行超の .rs (テスト/doc/コメントは数えない。分割してから足す):" "$(pick FILE-BUDGET)"
record FN-BUDGET budget "実コード 300 行超の関数 (単位を切って分割する):" "$(pick FN-BUDGET)"
record FN-NESTING budget "インデント 6 段を超える関数 (早期 return / ヘルパ抽出でほどく。計測値は 最大段数/6段以上の行数):" "$(pick FN-NESTING)"
record UNRESOLVED-MOD grep "#[cfg(test)] mod の解決に失敗 (#[path] 属性か。テストが production として課金される = 測定のバグ):" "$(pick UNRESOLVED-MOD)"
record KEY-COLLISION grep "関数キーが衝突 (2 関数が 1 つの天井を共有し、片方の違反が消える = 測定のバグ):" "$(pick KEY-COLLISION)"
```

**check 番号の振り直し (計画内で 1 通りに固定する)**:
record するのは `FILE-BUDGET` / `FN-BUDGET` / `FN-NESTING` / `UNRESOLVED-MOD` /
`KEY-COLLISION` の **5 本**なのでコメントは `6-10.`、後続の `COMMON-DEPS` (現 `# 7.`) を
`11.`、`UI-DOMAIN` (現 `# 8.`) を `12.` にする。
`scripts/arch_lint.sh:18-27` のコメントにある「8 チェック中 6 つ」の「8 チェック」も
**12 チェック**に更新する (事故当時の件数を消さないよう、「当時 8 チェック」と括弧書きにする)。
§4.1 / §7.1 / §7.5 / §9-6 に出てくる「checks 1-12」もこの数に揃えてある。

### 4.5 `LOC-BUDGET-OK` を人にも見せる

判定の直前 (現 :305 の `classify "$HITS" 0` の前。:304 は `# ---- 判定` の見出し) で 1 行出す。
「検査器が実際に何を見たか」を毎回可視化するため。

```sh
while IFS= read -r _l; do
    case "$_l" in "LOC-BUDGET-OK "*) printf 'arch-lint: [size] %s\n' "${_l#* }" ;; esac
done <<EOF
$budget_out
EOF
```

---

## 5. `scripts/arch_lint_baseline.txt`

### 5.1 ヘッダに追記 (現 :3-8 のあたり)

```
# 形式:  CHECK | key | fingerprint または 天井 | 理由 / いつ消えるか
#
# 第 3 field は CHECK の mode で意味が変わる:
#   grep / firstfield … マッチした行の内容ハッシュ (先頭 12 桁)
#   budget            … **計測値の天井**。`/` 区切りの整数ベクトルで、成分ごとに比較する。
#                       FILE-BUDGET / FN-BUDGET は 1 成分 (実コード行)、
#                       FN-NESTING は 2 成分 (最大インデント段数 / 6 段以上の行数)。
#                       1 成分でも天井を超えたら新規違反。
#                       天井は登録時点の実測値そのもの (余裕を持たせない) — 不変条件 9 の
#                       「超過したら分割してから足す」を字義どおり強制するため。
#                       縮んだときはゲートを落とさない。上げたいときは人がここを書き換え、
#                       なぜ太らせるのかを書く。
```

### 5.2 新規 baseline 行 (合計 164 行 = FILE 23 + FN-BUDGET 15 + FN-NESTING 126)

生成は `ARCH_LINT_EMIT_BASELINE=1 /usr/bin/bash scripts/arch_lint.sh`。
**出力を丸ごと貼らない** (`scripts/arch_lint.sh:313-315` が禁じている)。
理由と落とし所を書いてから貼る。

> **下の数値は「答え合わせ用」で、貼り付け用ではない。** 行は必ず
> `ARCH_LINT_EMIT_BASELINE=1` の実出力から貼る (理由の第 4 field を書き足す必要があるため)。
>
> **数値は 1 行でも食い違ってはいけない。** §3.5-§3.7 の規則は決定的で、独立実装 2 本が
> 下の 23 行を ncloc まで完全一致で再現していることを確認済み (計画初版が
> 「`arrangement/mod.rs` は 1,249 と 1,251 の 2 通りの実測がある」と書いていたのは、
> 初版時の粗い測定が混ざっていたため。**正しい値は 1,249**)。
> **1 行でもずれたら、貼る前に判定器を疑う** — どの規則 (§3.5 の raw string / §3.6 の
> cfg(test) 範囲 / §3.3 の raw 定義) から外れているかを特定してから進む。

#### FILE-BUDGET — 23 行 (実測値 = 天井)

**第 4 field (理由 / いつ消えるか) の書き方**: FILE-BUDGET も FN-BUDGET も、FN-NESTING と
同じく **原因ごとのコメントブロックで束ね、各行の第 4 field は短いタグ**にする
(`scripts/arch_lint_baseline.txt:20-31` と同じ形)。23 行 / 15 行を 1 行ずつ別の文章で
埋めるのは、読む側にとってノイズにしかならない。ブロックは次の 4 つを想定する
(実出力を 1 行ずつ見て、明らかに別カテゴリなら足す):

- **A. r.md #77 系の widget 分割で消える** — `widgets/arrangement/*` /
  `widgets/piano_roll/*`。タグ例 `r.md #77 系の分割で解消`。
- **B. view / handler の巨大 draw・巨大 match** — `view/track_inspector/mod.rs` /
  `view/root.rs` / `view/runner.rs` / `handler/*` / `app.rs` / `app_types.rs`。
  **未起票**。タグ例 `巨大 draw / match。未起票`。
- **C. プロセス境界の実装 (FFI / IPC / plugin host / audio engine)** —
  `daw_plugin_host/src/*` / `daw_audio/src/main.rs` / `common/src/model.rs`。
  **未起票**。タグ例 `FFI/IPC 実装。未起票`。
- **D. daw-ui (ui/crates)** — `ui/crates/ui/src/ui.rs` / `widgets/waveform.rs`。
  **未起票**。タグ例 `daw-ui core。未起票`。

未起票のものは §5.2 末尾の手順どおり、完了後にまとめてユーザーへ報告して起票を提案する
(**r.md は編集しない**)。

```
FILE-BUDGET | daw_gui/src/view/track_inspector/mod.rs                | 2214
FILE-BUDGET | daw_gui/src/app.rs                                     | 1993
FILE-BUDGET | daw_gui/src/widgets/arrangement/run.rs                 | 1946
FILE-BUDGET | common/src/model.rs                                    | 1795
FILE-BUDGET | daw_plugin_host/src/main.rs                            | 1760
FILE-BUDGET | daw_plugin_host/src/clap_plugin.rs                     | 1638
FILE-BUDGET | daw_gui/src/widgets/piano_roll/run.rs                  | 1633
FILE-BUDGET | daw_gui/src/view/runner.rs                             | 1615
FILE-BUDGET | ui/crates/ui/src/ui.rs                                 | 1613
FILE-BUDGET | daw_audio/src/main.rs                                  | 1569
FILE-BUDGET | daw_gui/src/widgets/arrangement/draw.rs                | 1565
FILE-BUDGET | daw_plugin_host/src/vst3_plugin.rs                     | 1466
FILE-BUDGET | daw_plugin_host/src/editor_window.rs                   | 1462
FILE-BUDGET | daw_gui/src/app_types.rs                               | 1436
FILE-BUDGET | daw_gui/src/widgets/arrangement/geometry.rs            | 1435
FILE-BUDGET | daw_gui/src/handler/automation.rs                      | 1410
FILE-BUDGET | daw_gui/src/widgets/arrangement/mod.rs                 | 1249
FILE-BUDGET | daw_gui/src/script.rs                                  | 1217
FILE-BUDGET | daw_gui/src/handler/project.rs                         | 1188
FILE-BUDGET | daw_gui/src/handler/selection_view.rs                  | 1184
FILE-BUDGET | daw_gui/src/handler/automation_lanes.rs                | 1170
FILE-BUDGET | daw_gui/src/view/root.rs                               | 1055
FILE-BUDGET | ui/crates/ui/src/widgets/waveform.rs                   | 1023
```

#### FN-BUDGET — 15 行

**key のスコープは §3.7 の規則で決まる。下の 15 本は実コードを読んで確定済み**
(自由関数はスコープ空、`impl` 内は型名が入る。プレースホルダではないのでそのまま使える):

```
FN-BUDGET | daw_gui/src/view/track_inspector/mod.rs::draw                             | 2063
FN-BUDGET | daw_gui/src/widgets/arrangement/run.rs::arrangement                       | 1944
FN-BUDGET | daw_gui/src/app.rs::AppData::handle_event                                 | 1623
FN-BUDGET | daw_gui/src/widgets/piano_roll/run.rs::piano_roll                         | 1389
FN-BUDGET | daw_gui/src/widgets/arrangement/release.rs::commit_releases               |  962
FN-BUDGET | daw_gui/src/view/audio_editor.rs::draw                                    |  781
FN-BUDGET | daw_audio/src/main.rs::recv_loop                                          |  705
FN-BUDGET | daw_gui/src/widgets/arrangement/render.rs::render_arrangement_heavy       |  681
FN-BUDGET | daw_gui/src/view/arrangement_view.rs::draw                                |  576
FN-BUDGET | daw_gui/src/view/root.rs::dispatch_shortcuts                              |  525
FN-BUDGET | daw_gui/src/view/track_inspector/modulation_rack.rs::draw_modulation_rack |  515
FN-BUDGET | ui/crates/ui/src/widgets/text_input.rs::Ui::text_input_at                 |  375
FN-BUDGET | daw_audio/src/graph/compile.rs::compile_schedule                          |  362
FN-BUDGET | daw_gui/src/view/transport.rs::draw                                       |  358
FN-BUDGET | daw_gui/src/app.rs::AppData::new                                          |  338
```

スコープ確定の根拠 (すべて実コードで確認):

- `track_inspector/mod.rs:228` / `audio_editor.rs:139` / `arrangement_view.rs:58` /
  `transport.rs:263` の `draw`、`root.rs:720` の `dispatch_shortcuts`、
  `arrangement/run.rs:7` の `arrangement`、`piano_roll/run.rs:26` の `piano_roll`、
  `arrangement/release.rs:10` の `commit_releases`、`arrangement/render.rs:9` の
  `render_arrangement_heavy`、`modulation_rack.rs:295` の `draw_modulation_rack`、
  `daw_audio/src/main.rs:851` の `recv_loop`、`compile.rs:71` の `compile_schedule`
  = **すべて impl の外の自由関数** → スコープ空。
  (`track_inspector/mod.rs` は `^impl ` が 1 件も無いことを確認済み。)
- `app.rs:102 impl AppData` の中に `:107 pub fn new`、`app.rs:515 impl AppData` の中に
  `:518 pub fn handle_event` → `AppData::`。
- `ui/crates/ui/src/widgets/text_input.rs:181 impl<'a, M: ?Sized + 'static> Ui<'a, M>` の中に
  `:186 pub fn text_input_at` → generics を飛ばした直後の識別子は `Ui` なので `Ui::`。

#### FN-NESTING — 126 行

インデント段数の実測ヒストグラム (関数 3,774 本):

| 段 | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| 本数 | 6 | 300 | 1291 | 1090 | 541 | 276 | 144 | 77 | 29 | 14 | 2 | 3 | 1 |

budget 6 (= 7 段以上が違反) で **126 本**。上位はこの並び (第 3 field は
`最大段数/6段以上の行数` になるので、下の段数だけでなく行数も EMIT_BASELINE から取る):

```
12  daw_gui/src/widgets/arrangement/render.rs::render_arrangement_heavy   (ncloc 681)
11  daw_gui/src/widgets/arrangement/run.rs::arrangement                   (ncloc 1944、>=6 が 520 行)
11  daw_gui/src/widgets/arrangement/release.rs::commit_releases           (ncloc 962)
11  daw_plugin_host/src/builtin/voicevox.rs::VoicevoxBuiltin::start_synth_thread (ncloc 237、11/173)
10  daw_audio/src/main.rs::recv_loop                                      (ncloc 705)
10  daw_gui/src/widgets/arrangement/view_build.rs::build                  (ncloc 280)
 9  daw_gui/src/app.rs::AppData::handle_event / piano_roll/run.rs::piano_roll / … (計 14 本)
 8  daw_gui/src/view/track_inspector/mod.rs::draw / …                     (計 29 本)
 7  daw_gui/src/view/root.rs::dispatch_shortcuts / …                      (計 77 本)
```

**上のキーのスコープも実コードで確認済み**:

- `render.rs:9 render_arrangement_heavy` / `view_build.rs:47 build` / `run.rs:7 arrangement` /
  `release.rs:10 commit_releases` / `daw_audio/src/main.rs:851 recv_loop` は
  いずれも **impl の外の自由関数** → スコープ空 (`^impl ` が同ファイルに無いことを確認済み)。
- `daw_plugin_host/src/builtin/voicevox.rs` の `start_synth_thread` は **自由関数ではない**。
  `:449 impl VoicevoxBuiltin {` の中の `:486` なので、正しいキーは
  `daw_plugin_host/src/builtin/voicevox.rs::VoicevoxBuiltin::start_synth_thread`。
  (計画初版はここだけスコープを空で書いていた。FN-BUDGET 15 本は実コードで確認済みだったが、
  FN-NESTING の例示リストは未確認のままだった。)

**126 行は「多すぎるから閾値を上げる」理由にならない**。現行検査が 0 件 = 完全な空振りで、
その裏にこれだけの負債が積んである、というのがこの項目の発見そのもの。
理由の書き方だけ工夫する: **原因ごとにコメントブロックでまとめ、各行の第 4 field は
短いタグにする**。既存の `scripts/arch_lint_baseline.txt:20-31` (POSITIONAL-KEY 4 件が
1 つのコメントブロックを共有している) と同じ形。想定するブロック:

1. `immediate-mode の描画ループ` — `heavy` → `hctx` → `for track` → `for clip` → `if` と
   下がる widget 群 (arrangement / piano_roll / mixer / waveform)。
   いつ消えるか: r.md #77 系の分割で解消。
2. `handler の edit_song クロージャ + 多重ループ` — `edit_song(|song| { for track { for clip {`。
   いつ消えるか: 未起票 (r.md への起票をユーザーへ提案)。
3. `IPC / イベントの巨大 match` — `handle_event` / `recv_loop` / `dispatch_*_event` /
   `handle_command`。いつ消えるか: 未起票。
4. `FFI / Win32 の状態機械` — `editor_wnd_proc` / `start_synth_thread` / `build_stream`。
   いつ消えるか: 未起票。
5. `ui/crates の widget` と `ui/crates/examples` — daw-ui 側。いつ消えるか: 未起票。

**実装者は 126 行を上の 5 ブロックに割り振り、各ブロックの見出しコメントに
「理由 / いつ消えるか」を書く。1 行も未分類で残さない** (「残りはその他」ブロックを作って
まとめて放り込むのは、理由を書いたことにならない)。**行そのものは 1 行ずつ目で見て、
明らかに別カテゴリならブロックを足す。** 完了後、未起票のものは列挙してユーザーに報告し、
r.md への起票を提案する (勝手に r.md を編集しない — memory: `feedback_defer_todos_to_fixme`)。

FILE-BUDGET 23 行 / FN-BUDGET 15 行の第 4 field も同じ流儀 (§5.2 の FILE-BUDGET 節に
A〜D の 4 ブロックを定義してある)。**3 CHECK 合計 164 行すべてに、束ねられたブロックの
見出しコメントと短いタグが付く**状態にして初めて baseline を書き終えたことになる。

#### `STRIP-COMMENTS-BLIND` は作らない

この計画の初版は raw string 中の行頭 `//` の件数を baseline に登録する新 CHECK を
用意していたが、**§1.6 で根治する方針に反転したので不要**。
`common/src/video_fx.rs` の 14 行 / `ui/crates/renderer/tests/texture_interop.rs` の 1 行は
lexer 版 `strip_comments` が正しく code と判定するので、baseline に載せる対象が消える。

---

## 6. `Cargo.toml` — 関数長ゲートを 1 本に畳む

**現状の欠陥**: 関数長ゲートが木の一部 (`ui/crates/*` の 10 crate) にだけ、閾値 100 で
存在している (§0.2-9)。しかも:

- `clippy::too_many_lines` の実装は **文字列を一切扱わない** — カウントループが追うのは
  `//` `/*` `*/` だけなので、`video_fx.rs` の WGSL や `project.rs:1815` を誤分類する。
- 逃げ道が **理由なしの `#[allow]`** しかなく、恒久的に正当なのか負債なのかを区別できない
  (`scripts/arch_lint_baseline.txt:10-13` が禁じている状態そのもの)。実際 **33 箇所**ある。
- `clippy::excessive_nesting` は `excessive-nesting-threshold` の既定が 0 = 無効で、
  しかも brace 深さで測るのでインデント段数を表現できない。

同じ指標に対して 2 つの権威を持つのは SSoT 違反なので、**`scripts/loc_budget.py` に
一本化する**。`[workspace.lints.clippy]` (`Cargo.toml:130-147` のブロック内、
`module_name_repetitions = "allow"` 等が並んでいるところ) に足す:

```toml
# 関数長ゲートの SSoT は scripts/loc_budget.py (make arch-lint の FN-BUDGET / FN-NESTING)。
# clippy::too_many_lines は (a) [lints] workspace = true を opt-in している ui/crates の
# 10 crate にしか掛からない非対称、(b) 文字列を見ないので raw string 中の // を誤分類する、
# (c) 逃げ道が理由なしの #[allow] しかなく「恒久的に正当」と「既知の負債」を区別できない、
# の 3 点で劣る。全 crate 共通の閾値 (実コード 300 行) + 理由付き baseline へ寄せる。r.md #76。
too_many_lines = "allow"
```

**非対称をどう畳んだかの明示** (承認済み方針「ui/crates だけ clippy pedantic が効いている
非対称の扱いを決める」への答え。ここは決定であって、実装中に判断を保留する箇所ではない):

- 採らなかった案: 残り 4 crate (`common` / `daw_gui` / `daw_audio` / `daw_plugin_host`) にも
  `[lints] workspace = true` を足して pedantic を全木へ広げる。
  これは `too_many_lines` 以外の pedantic 全部を同時に有効化する別プロジェクトで、
  しかも **同じ指標に権威が 2 つある状態は解消しない** (clippy 100 行 と loc_budget 300 行)。
- 採った案: clippy 側の関数長ゲートを降ろし、`loc_budget.py` の **全 crate 共通
  300 実コード行 + インデント 6 段**に一本化する。これで「関数長を誰が測るか」が
  リポジトリで 1 つになり、掛かる範囲は 10 crate → **全 crate (389 ファイル)** に広がる。
- 実質の緩和は「今日 100 行以下の ui/crates の関数が 300 行まで太れる」ことだけ。
  100 行を超えている 24 箇所は今日すでに `#[allow]` でゲートを無効化しているので、
  そこは変化しない。**閾値 300 は承認済み方針**なので、その帰結として受け入れる。
- 既存の `#[allow(clippy::too_many_lines)]` 33 箇所は **無害な冗長属性になるだけ** で
  警告は出ない (`clippy::allow_attributes` は restriction グループで未有効)。
  **この項目は `.rs` を 1 行も触らないという承認済み方針があるので撤去しない** (§10)。
- 変更後 `make clippy` が green のままであることを必ず確認する (§8-6)。

---

## 7. Makefile / skill / CLAUDE.md / docs の文言

### 7.1 `Makefile:216-220`

```make
# アーキテクチャ不変条件の機械検査 (CLAUDE.md「アーキテクチャ不変条件」/
# docs/plan_arch_refactor.md §11)。**exit 0 = 「違反ゼロ、または
# scripts/arch_lint_baseline.txt に記録済みのものだけ」** — baseline に無い違反が
# 1 件でもあれば exit 1 (行単位 ratchet)。ARCH_LINT_STRICT=1 は baseline 済みの負債も落とす。
# サイズ budget (FILE-BUDGET / FN-BUDGET / FN-NESTING) と行分類 (コメント内の言及を
# 違反に数えない判定) は scripts/loc_budget.py が持つので **python が要る**。
# **python が無い / 壊れている (Windows Store のスタブ等) と arch-lint は全面停止する** —
# サイズ budget だけでなく RT-INFINITE / POSITIONAL-KEY / LEGACY-PROTOCOL / UNTAGGED /
# BLOB-IN-PROTOCOL / COMMON-DEPS / UI-DOMAIN も止まる。cargo-deny (Makefile:213) と同じ
# 「skip の緑を作らない」原則で、これは意図した挙動。
# 検出と self-test は script 側が持つ (直接 bash で叩く経路 = /arch-review skill でも同じ
# 保証が要るため)。ここは Makefile:6 で解決済みの PYTHON を渡すだけ。
arch-lint:
	PYTHON="$(PYTHON)" /usr/bin/bash scripts/arch_lint.sh
```

### 7.2 `.claude/skills/arch-review/SKILL.md`

- `:5` — `arch-lint 機械検査 + god file budget で、` →
  `arch-lint 機械検査 + サイズ budget (実コード行 / 関数長 / ネスト) で、`
- `:11` — `Bash(find *), Bash(wc *)` を **`Bash(python *), Bash(python3 *)`** に置換
  (find / wc はこの skill では `:33` のワンライナーでしか使っていない。実測確認済み)。
- `:30-34` — 置換:

````markdown
加えてサイズ budget (実コード行 / 関数長 / ネスト) の推移を測る:

```bash
python scripts/loc_budget.py --report
```

`wc -l` で測らないこと。物理行はテスト module と doc comment を課金してしまい、
「テストを厚くすると分割を迫られる」逆インセンティブになる (r.md #76)。
````

### 7.3 `.claude/skills/implement/SKILL.md:126`

```markdown
- **サイズ budget に近いか?** (ファイル実コード 1,000 行 / 関数実コード 300 行 /
  インデント 6 段) → 先に分割 (不変条件 9)。現在値は `python scripts/loc_budget.py --report`、
  検査は `make arch-lint`。**物理行ではない** — テスト / doc comment / 空行は数えない
```

### 7.4 `.claude/skills/review/SKILL.md:77`

`ドメイン知識混入 / 3,000 行超ファイルの肥大継続` →
`ドメイン知識混入 / baseline 済みのサイズ超過 (FILE-BUDGET / FN-BUDGET / FN-NESTING) を更に太らせていないか`

### 7.5 `CLAUDE.md`

不変条件 9 (`:459-460`) を置換:

```markdown
9. **サイズ budget**: **実コード行 (ncloc = 空白・コメント・doc comment を除いた物理行)** で
   1 ファイル **1,000 行** / 1 関数 **300 行** / インデント **6 段**。超過したら分割してから
   足す (app.rs 25k 行の再発防止)。**テストコードは対象外** (`#[cfg(test)]` の付いた item、
   `#[cfg(test)] mod X;` が指すファイル、`tests/` `benches/` 直下)。
   測り方の SSoT は `scripts/loc_budget.py` (Rust の字句解析。`wc -l` ではない —
   物理行で測っていた頃は「テストを厚くすると分割を迫られる / doc を書くと分割を迫られる」
   逆インセンティブになり、tests を別ファイルへ移すだけの commit が実際に 2 件生えた)。
   現在値は `python scripts/loc_budget.py --report`。r.md #76。
```

`:414` の SSoT 記述に追記:

```markdown
**「何を違反とみなすか」の SSoT は `scripts/arch_lint.sh`**、ガードはその write-time ミラー。
ただし **サイズ budget (FILE-BUDGET / FN-BUDGET / FN-NESTING) の測り方**と、
**全 check 共通の「コメント内の言及は違反に数えない」の行分類**だけは
`scripts/loc_budget.py` が持つ (Rust の字句解析が要るので shell の正規表現では表せない)。
その帰結として **python が無い / 壊れていると `make arch-lint` は全面停止する**
(cargo-deny と同じ「skip の緑を作らない」原則)。
```

`:419-421` の baseline 説明に追記:

```markdown
- 既知の負債は baseline に **理由と落とし所つきで 1 行**。fingerprint は行番号ではなく
  マッチ行の内容ハッシュ (行番号は無関係な編集でずれる)。**サイズ budget の行だけは
  第 3 field が計測値の天井** (`/` 区切りの整数ベクトル。FILE/FN-BUDGET は実コード行、
  FN-NESTING は 最大段数/6段以上の行数)。1 成分でも超えたら新規違反として表に出る
  (path だけをキーにすると、baseline 済みのファイルが無制限に太れてしまう)。
  件数 baseline にはしない (「1 件直して 1 件増やす」が素通りする)。
```

### 7.6 `docs/plan_arch_refactor.md`

- `:466` — `protocol への Vec<f32>/Arc<[u8]> 混入、file 行数 budget (>3000 warn)、` →
  `protocol への Vec<f32>/Arc<[u8]> 混入、サイズ budget (実コード 1,000 行 / 関数 300 行 /
  インデント 6 段。scripts/loc_budget.py。当初は物理行 >3000 だったが r.md #76 で置換)、`
- `:470` — `本セッションの 6 レンズ並列分析 + arch-lint + 行数 budget を` →
  `本セッションの 6 レンズ並列分析 + arch-lint + サイズ budget (実コード行 / 関数長 /
  ネスト。当初は行数 budget) を`
- `:10` / `:28` / `:32` / `:85-86` は **当時の記録なので数値を書き換えない**。
  `:85` の節末に注記を 1 行足すだけ:
  `(当時の指標 = 物理行 3,000。2026-08 の r.md #76 で実コード行 1,000 + 関数長 300 +
  インデント 6 段へ置換した。物理行では逆インセンティブになるため。**`:10` / `:28` / `:32`
  の「≤3000」「< 3,000」も同じく当時の値**)`
  - `:28` 「8 モジュール **≤3000** に分割」/ `:32` 「5 モジュール **≤3000** 分割」は
    §2 の初版 grep パターンに掛からなかった形 (§2 の枠内参照)。**書き換えないが、
    確認用 grep では検出できるようにしておく**。

### 7.7 `docs/plan_video_decode_unify.md:91,131`

3,000 行前提の記述を新指標へ書き換える。

- `:91` `MF 中核を削った後は各ファイルが god-file budget (3,000 行) に十分収まるため` →
  `MF 中核を削った後は各ファイルがサイズ budget (実コード 1,000 行) に十分収まるため`
- `:131` `**#9 god-file budget**: video/ 分割で全ファイル 3,000 行以内。現 video_playback.rs
  1,946 行は解体。` → `**#9 サイズ budget**: video/ 分割で全ファイル実コード 1,000 行以内。
  現 video_playback.rs (物理 1,946 行) は解体。` (実コード値は
  `python scripts/loc_budget.py --report` で確認して書く)

### 7.8 `docs/plan_rmd_77_arrangement_split.md` — **次に着手される計画書なので必ず直す**

#76 が着地した瞬間に false になる記述が 5 か所ある (2026-08-28 に実ファイルで照合。
当時 1,621 行)。`plan_arch_refactor.md` の `:10` / `:85` のような「当時の記録」ではなく、
**これから実装者が読んで従う手順書**なので、注記ではなく **書き換える**。

> **このファイルは #77 着手まで成長し続ける。**下の行番号は目印であって、
> 触る直前に必ず
> `grep -nE "3,000|3000|god file budget|arch_lint|arch-lint" docs/plan_rmd_77_arrangement_split.md`
> を打ち直して現在位置を取ること。引用している本文の文字列で照合すれば同定できる。

- `:47-50` — 「r.md #76 で関数長の機械検査が入った**場合は** …
  `scripts/arch_lint_baseline.txt` に 1 行登録する」。もう「場合」ではないので、
  `piano_roll/run.rs` の `FILE-BUDGET` (1633) / `FN-BUDGET` (`::piano_roll` 1389) /
  `FN-NESTING` は **#76 の baseline に既に入っている**旨へ書き換える
  (#77 側で新規登録する作業は無い)。
- `:288` `全ファイルが god file budget (scripts/arch_lint.sh:284-290 のチェック 6、3,000 行) の内側。`
  → `全ファイルがサイズ budget (実コード 1,000 行 / 関数 300 行 / インデント 6 段。
  scripts/loc_budget.py) の内側であることを、分割後に
  python scripts/loc_budget.py --report で確認する。`
  **同表の分割後サイズ見積り (render.rs ~1,060 等) は物理行なので、新指標での値は未検証**
  である旨も書く。特に `render.rs` は現在 物理 861 行 / 実コード 681 行で FILE-BUDGET の
  baseline に載っていないため、#77 で `run.rs` から 244 行 (`:1851-2094`) を受け取ると
  **新規違反になり得る**。#77 の実装者は分割後に `--report` で確認し、超えるなら
  分割単位を切り直す。
- `:289` `scripts/arch_lint_baseline.txt に arrangement 関連のエントリは 0 件なので新規追記も不要。`
  → **#76 着地後は事実として偽**。arrangement 関連は少なくとも 8 行入る:
  `FILE-BUDGET` × 4 (`run.rs` / `draw.rs` / `geometry.rs` / `mod.rs`)、
  `FN-BUDGET` × 3 (`run.rs::arrangement` / `release.rs::commit_releases` /
  `render.rs::render_arrangement_heavy`)、`FN-NESTING` は同 3 本 + `view_build.rs::build`。
  書き換え文:
  `scripts/arch_lint_baseline.txt には arrangement 関連の FILE-BUDGET / FN-BUDGET /
  FN-NESTING が登録済み (r.md #76)。分割で消えた行は「解消」として通知されるので削除し、
  残った関数の天井は実測値に更新する。新しく生えたファイル/関数が違反するなら、
  分割単位を切り直す (baseline を増やして着地させない)。`
- `:1501-1502` `make arch-lint の 8 チェックはいずれも無関係。god file budget は全ファイルが
  3,000 行以内に収まる方向にしか動かない。scripts/arch_lint_baseline.txt に arrangement
  関連は 0 件。` → `make arch-lint の 12 チェックのうち FILE-BUDGET / FN-BUDGET /
  FN-NESTING が直接関係する (r.md #76)。分割は違反を減らす方向に動くが、受け取り側
  (render.rs) が新たに閾値を超えないことを python scripts/loc_budget.py --report で確認する。`
- `:1588` `scripts/arch_lint.sh:284-290 — god file budget 3,000 行` →
  `scripts/loc_budget.py — サイズ budget (実コード 1,000 行 / 関数 300 行 / インデント 6 段)。
  arch_lint.sh の check 6-10 がこれを呼ぶ`

### 7.9 着地済み計画書への注記 (4 ファイル / 10 か所)

これらは実行済みの記録なので **数値は書き換えず、注記を 1 文足すだけ**。
同じファイル内で複数か所ヒットするものは、**最初のヒットの節末に 1 回だけ**足す
(同じ注記を 3 回書かない)。行番号は 2026-08-28 に実ファイルで照合済み。

- `docs/plan_rmd_71_device_copy.md`
  - `:49` — 表の `mod.rs の巨大 expansion closure を移す先 (god file budget)`
  - `:1317-1318` — `mod.rs は **現在 2,623 行**で、3,000 行の god file budget まで 377 行しかない`
    (D-4-0「先に mod.rs を割る」の**根拠そのもの**)
  - `:1797` — `**god file budget (不変条件 9、3,000 行)**`
  - → `:1317` の段落末に注記を足す (この 3 か所は同じ前提を共有しているため)。
    **新指標では `track_inspector/mod.rs` は実コード 2,214 行 = 1,000 行 budget の
    2 倍超**で、当時の「377 行しか余裕が無い」より更に強い根拠になる旨も 1 文で書く。
- `docs/plan_rmd_73_automation_curve.md:1875,1877` — `**god file budget (3,000 行)**` /
  `3,000 に近づくので`
- `docs/plan_rmd_74_disclosure_glyph.md:960,962,1049` —
  `god file budget に余裕あり (run.rs 2,699 / app.rs 2,424 / …)` /
  `3,000 行制限には掛からない` / `god file budget に余裕`
- `docs/plan_rmd_75_voicevox_phrase.md:1883,1885` — `god file budget` / `3,000 行に近づいたら`

注記文: `(当時の指標 = 物理行 3,000。r.md #76 で実コード行 1,000 + 関数 300 行 +
インデント 6 段へ置換済み。現在値は python scripts/loc_budget.py --report)`

### 7.10 `.claude/guards.jsonl` と `scripts/test_guards.py` の「パリティ」記述

`strip_comments` が lexer になる (§1.6) と、**write-time ガード側 (Python の行正規表現) と
arch-lint 側 (字句解析) の判定が一致しなくなる**。今日その差が出るのは
「raw string 中の行頭 `//`」と「`/* … */` の中身」の 2 つで、どちらも
`.claude/guards.jsonl` の対象 (`common/src/*` の untagged / positional key / MainToChild) には
今日 0 件だが、**「意味は同じ」と書いてあるコメントが偽になる**ので直す。

- `.claude/guards.jsonl:113-122` のコメントブロック
  (`判定が割れないよう「何を違反とみなすか」の SSoT は arch_lint.sh 側` /
  `**コメント行を違反に数えない** (arch_lint.sh の strip_comments と同じ…)` /
  `表記が違うのは意図的… **意味は同じ**`) を書き換える。**JSON のルール行は 1 行も触らない
  — コメント行だけ**:
  - 「コメント行を違反に数えない」の根拠が `scripts/loc_budget.py` の lexer になったこと
  - ガード側は行正規表現なので **raw string 中の行頭 `//` と `/* … */` の中身で判定が割れる**
    こと。**割れる方向は「ガードの方が広く nudge する」**ので、write-time nudge としては
    安全側 (`escalate: false` のまま)
  - 「意味は同じ」→「**行頭コメントについては同じ。raw string / ブロックコメントでは
    arch-lint の方が正確**」へ
- `scripts/test_guards.py:478-483` のコメント
  (`arch_lint.sh has the same problem and solves it with strip_comments; these cases pin the
  parity.`) と `:937-938` のコメント
  (`arch_lint.sh の strip_comments / arch-* ガードと同じ扱い`) にも同じ趣旨の 1 文を足す。
  **テストケースもロジックも触らない** (どちらも arch_lint.sh と実行結合が無いので、
  §1.6 の変更でテストが落ちることはない — 実測確認済み)。

---

## 8. 検証手順

**Rust のソースを 1 行も触らないので `make test` / `make test-nolaunch` は不要**
(Cargo.toml の lint 設定変更があるので `make clippy` は必須)。
`make test` は daw_gui を起動するので、この項目では絶対に走らせない。

順に実行し、すべて期待どおりであること:

1. `python scripts/loc_budget.py --self-test`
   → `loc-budget: self-test ok (…)` / exit 0
2. `python scripts/loc_budget.py --report`
   → §5.2 の上位表と **完全一致** (track_inspector/mod.rs 2214 が先頭、389 ファイル /
   テスト 74 / 生成物 2、`0 key-collision` / `0 unresolved-mod`)。
   **1 行でも食い違ったら baseline を書く前に判定器を疑う** (§5.2 の枠内)。
   よくある外し方: `key-collision` が 17 → §3.7 の trait / cfg 修飾の実装漏れ。
3. `ARCH_LINT_EMIT_BASELINE=1 /usr/bin/bash scripts/arch_lint.sh`
   → 新規 164 件 + 貼り付け用の行。exit 1 (baseline を書く前なので正しい)
4. baseline を書いた後 `make arch-lint`
   → `arch-lint: [size] 389 files / 74 test / …` と
   `arch-lint: baseline 168 件 (解消 0) / 新規 0 件` (164 + 既存 POSITIONAL-KEY 4)。exit 0
5. **わざと壊して赤になることを確認する** (ratchet が効いている証明)。
   **この項目は `.rs` を 1 行も触らないので、検証も追跡下の `.rs` を編集せずに行う** —
   触るのは (a) `scripts/arch_lint_baseline.txt` (この項目が所有するファイル)、
   (b) **未追跡の使い捨て `.rs`**、(c) 環境変数、の 3 つだけ。
   最後に **`git status --porcelain` が「この項目が意図した変更だけ」を出すことを確認**する
   (使い捨てファイルが残っていない / 追跡 `.rs` が 1 つも出ていない)。
   - baseline の `FILE-BUDGET | daw_gui/src/script.rs | 1217` の天井を **一時的に 1216 へ
     下げて** `make arch-lint` → `FILE-BUDGET` が新規違反として出て exit 1。
     **同時に「解消」には出ないこと** (§4.3.3 の二重報告防止が効いている証明)。
     確認後は 1217 に戻す。
     (**ソースを太らせるのと同値な検査**で、しかも `run.rs` = r.md #77 の対象ファイルに
     触らずに済む。天井を下げる = 計測値が天井を超える、という同じ比較経路を通る。)
   - baseline の `FN-NESTING | …/run.rs::arrangement | 11/520` を **一時的に `11/519` へ
     下げて** `make arch-lint` → `FN-NESTING` が新規違反として出る
     (= **最大段数が変わらなくても肥大を捕まえる**証明 = 第 2 成分 `deep_lines` が
     ratchet に効いている証明)。確認後は `11/520` に戻す。
   - 同じ行を `12/520` (= 天井を上げる) にして `make arch-lint` → **緑のまま**
     (= 縮む方向・据え置きでゲートを落とさない証明)。確認後は戻す。
   - `scratch_huge.rs` のような **未追跡** の .rs (実コード 1,100 行) をリポジトリ内に
     作って `make arch-lint` → 新規違反として出る (= `--others` が効いている証明)。
     確認したら `python scripts/trash.py` で消す (memory: `feedback_delete_to_recycle_bin`)
   - **未追跡**の `common/src/zz_scratch_rawstring.rs` を作り、その中の raw string
     (`const S: &str = r#" … "#;`) に行頭 `// pool: HashMap<(u32, u32), Bogus>` を 1 行入れて
     `make arch-lint` → `POSITIONAL-KEY` の新規違反として出る
     (= lexer 版 `strip_comments` が raw string を code と判定している証明。
     旧実装なら黙って落ちていた)。check 2 の走査 root は `common/src` を含み、
     `grep -rn` は未追跡ファイルも読むので、**追跡下の `video_fx.rs` を編集する必要は無い**。
     同じファイルの通常コメント行に同じパターンを書いた行も足しておき、
     **そちらは違反にならない**ことも同時に確認する (否定側)。確認したら trash.py で消す。
   - `PATH` から python を外して `make arch-lint` → SELF-BROKEN で exit 1
     (= skip の緑を作っていない証明)。**メッセージが「grep のパターンが通らない」ではなく
     python を名指ししていること**も確認する (§4.2 の出口分離が効いている証明)
   - `MIN_SCANNED_FILES` を一時的に 100000 にして `make arch-lint`
     → `loc_budget.py が exit 2` で SELF-BROKEN (= 0 件走査の fail-open を塞いだ証明)。
     `scripts/loc_budget.py` は自分が新規に作るファイルなので、この一時変更は
     「追跡 `.rs` を触らない」に抵触しない。確認後は 200 に戻す。
6. `make clippy`
   → green。`too_many_lines = "allow"` を足したことで新しい警告が出ないこと
   (既存の `#[allow(clippy::too_many_lines)]` 33 箇所は冗長になるだけで警告にならない)
7. `make check`
   → 影響なし (Rust ソース無変更)
8. `make license-check`
   → 新規ファイル `scripts/loc_budget.py` が `REUSE.toml:25` の `path = "**"` blanket に
   覆われていること (per-file SPDX ヘッダは入れない — memory: `project_gplv3_publication`)
9. `git status --porcelain`
   → **この項目が意図した変更だけ**が出ること。追跡下の `.rs` が 1 つでも出たら
   §8-5 の使い捨てを戻し忘れている。使い捨て `.rs` / `MIN_SCANNED_FILES` の一時値 /
   baseline の一時的な天井も同時に確認する。

**所要時間の目安 (実測)**: 389 ファイル / 約 21.2 万行を Python で 1 文字ずつ走査するので、
`--check` の 1 回で **2〜7 秒**かかる (この計画の検証で使った独立実装 2 本の実測は
lex のみ 2.7 秒 / 指標算出込み 6.3 秒)。現在の `make arch-lint` は 1 秒未満なので、
**体感で数倍遅くなるのは正常**。内訳:

- `--self-test` … 合成フィクスチャだけなので 0.1 秒未満
- `--check` … 1 回だけ。ここが支配的
- `--filter-comments` … check 1/2/3/5/8 の 5 回呼ばれるが、**lex するのは stdin に現れた
  path だけ** (今日は合計 10 行未満)。Python の起動コスト × 5 ≒ 0.5 秒

**遅いからといって、キャッシュファイル・走査ファイルの間引き・並列化を入れないこと。**
どれも「検査が実際に何を見たか」を不透明にする方向で、この項目が塞ごうとしている
false green の温床そのものになる。数秒はオンデマンドの lint として許容範囲。

commit 前に `/review` を通す (memory: `feedback_review_before_commit`)。

---

## 9. 実装上の落とし穴 (先に潰しておくこと)

1. **文字コード**: Windows の Python 既定は cp932。`read_text(encoding="utf-8")` を必ず明示。
   日本語を出力するので `sys.stdout.reconfigure(encoding="utf-8", errors="replace")` も
   `scripts/reuse_lint.py:57-63` と同じ形で入れる (検査結果の exit code を
   `UnicodeEncodeError` で殺さない)。`--filter-comments` は stdin も同様に設定する。
2. **`set -u`**: `arch_lint.sh` は `set -u` なので、新しい変数は使う前に初期化する
   (`_budget_broken` / `_budget_rc`)。
3. **`record` の分解規則**: `scripts/arch_lint.sh:119-128` は hits の行を
   `CHECK MODE <line>` として連結し、`split_row` が **半角スペース 1 個**で分解する。
   budget 行の第 2・第 3 field に空白を入れてはいけない。
4. **一時ファイルを使わない**: `scripts/arch_lint.sh:99-104` の注記どおり、MSYS2 bash と
   Git の coreutils で `/tmp` が別ルートに解決される。すべて shell 変数に持つ。
5. **`$(pick …)` の中で `budget_out` を読む**ので、`budget_out` は `pick` の定義より前に
   代入しておくこと。
6. **`python` が Windows Store のスタブ**に解決される環境がある。`--self-test` が落ちるので
   SELF-BROKEN として表に出る (= 黙って通らない)。ただし §4.2 の self-test は canary ブロック
   = **checks 1-12 より前**にあり、しかも `strip_comments` も python に依存するので、
   この場合 **arch-lint 全体が止まる**。これは意図した挙動 (cargo-deny と同じ
   「skip の緑を作らない」)。`Makefile:216-` と `CLAUDE.md:414` にこの帰結を書く (§7.1 / §7.5)。
7. **`scripts/arch_lint_baseline.txt` は他項目も触り得る共有ファイル**。並行作業からの
   統合時は行単位マージ前提。#76 は Rust ソースを触らないので、それ以外の衝突は起きない。
   ただし `arch_lint.sh` は 冒頭の python 解決 / `strip_comments` / canary (配線 + self-test) /
   バックスラッシュ注記の「8 チェック」表記 / `record` ヘッダ / `baseline_ceiling` + `budget_le` /
   `classify` / ratchet self-test / check 6 の差し替え / check 番号の振り直し /
   `LOC-BUDGET-OK` の表示 / `EMIT_BASELINE` の **12 か所**に手が入るので、
   `arch_lint.sh` を触る他項目があれば衝突する。
8. **r.md #77 との関係**: #77 が `run.rs` を分割して着地したとき、baseline は
   **削除だけでは足りない**。
   - 消える: `FILE-BUDGET | daw_gui/src/widgets/arrangement/run.rs` /
     `FN-BUDGET | …::arrangement` / `FN-NESTING | …::arrangement`
   - **上がる方向に動く**: `render.rs` が `run.rs` から 244 行を受け取るので
     `FN-BUDGET | …::render_arrangement_heavy` と `FN-NESTING | …::render_arrangement_heavy`
     の計測値が増え、`FILE-BUDGET | …/render.rs` が新規に生えるかもしれない。
   - よって **#77 着地後は `make arch-lint` の「新規違反」と「解消」を両方読んで baseline を
     更新する**。片方だけ見ると、新規違反で赤のまま止まるか、天井が消えて無検査になる。
   - **#77 側が `arch_lint.sh` を編集する必要は無い。**
9. **key の安定性**: 関数のキーは `path::Scope::name` (+ trait / cfg 修飾、§3.7)。
   **並べ替えでは動かない** — 連番 `#n` を使わないので、出現順に依存する成分が無い。
   動くのは **ファイル移動・型名の変更・trait の差し替え・cfg 述語の書き換え**のときだけで、
   いずれも「その関数を意図して触った」ケース。その場合は baseline 行のキーを実出力に
   合わせて書き換える (天井は据え置き)。衝突は `KEY-COLLISION` として ratchet に出るので、
   増えたら赤になって気付ける (件数は `LOC-BUDGET-OK` 行にも出る)。
10. **実行時間**: `--check` 1 回で 2〜7 秒 (§8 末尾)。速くするためのキャッシュ・間引き・
   並列化は入れない。「検査器が実際に何を見たか」が不透明になるのは、この項目が
   塞ごうとしている false green の温床そのもの。

---

## 10. この項目でやらないこと

**「後でやる」ではなく「この項目の所有物ではない」もの**を列挙する。
フェーズ分けではない — 承認済み方針が明示的に切り出している境界。

- **`.rs` の編集全般**。`#[allow(clippy::too_many_lines)]` 33 箇所の撤去 (§6) も、
  126 件の FN-NESTING / 15 件の FN-BUDGET / 23 件の FILE-BUDGET を実際に分割することも、
  すべて **「Rust のソースは 1 行も触らない」という承認済み方針**の外側。
  分割は r.md #77 以降が持つ。
- **未起票の負債の起票**。FN-NESTING 126 件のうち §5.2 のブロック 2〜5 に落ちるものは
  r.md に起票されていない。**r.md は編集しない** — 最終報告で列挙してユーザーに提案する
  (memory: `feedback_defer_todos_to_fixme`)。
- `daw_audio/src/graph/compile.rs` は新指標では実コード **552 行** (物理 2,978 行のうち
  テスト 2,078 / doc 104 / comment 205 / blank 39) で、budget に対して大きく余裕がある。
  r.md #76 が「凝集としては正しい 1 ファイル、分割は不要」と判定したとおりになる。
  これは指標が意図どおり動いていることの確認であって、作業項目ではない。

---

## 11. 裏取りの指摘に対する処置 (何を直したか)

### 11.1 第 1 次裏取りの指摘に対する処置

| 指摘 | 実コードでの確認 | 処置 |
|---|---|---|
| 0 ファイル走査でも緑になる fail-open | `--check` の契約上、空リスト → 違反 0 行 + 完走マーカーあり → exit 0 になる。baseline 全行が「解消」表示 | **採用**。§1.4 に「git 非ゼロ / 起動不可 → exit 2」「`MIN_SCANNED_FILES` (200) 未満 → exit 2」、§3.2 に定数、§3.9 に exit 2 条件、§3.12 (D) に self-test、§8-5 に検証手順を追加 |
| `--self-test` に肯定側 canary が無い | `scripts/arch_lint.sh:44-61` は肯定側 (`grep -qE` が通ること) と否定側 (`strip_allowed` が効くこと) を両方持つ。初版の 13 フィクスチャは分類器と否定側だけ | **採用**。§3.12 を (A)(B)(C)(D) に再構成し、閾値±1 の肯定/境界 canary 8 件を追加。§3.4 で `emit_check` を「行を返す」形にし、self-test と `--check` が同じ経路を通ることを強制 |
| budget モードが「新規違反」と「解消」に二重報告 | `scripts/arch_lint.sh:214-222` の解消ループは BASEKEYS のうち SEEN に無いものを出す。初版の分岐は超過時に SEEN へ積んでいなかった | **採用**。§4.3.3 で超過時も SEEN に積む。§4.3.4 の ratchet self-test に `N_RESOLVED -eq 0` の assert、§8-5 に実機確認を追加 |
| `UNRESOLVED-MOD` の行フォーマット未定義 / CHECK 名不一致 | budget モードは第 3 field を整数比較するので、非数値だと `test: integer expression expected` | **採用**。`UNRESOLVED-MOD` は budget ではなく **grep 形式 (`path:line:content`)** に変更 (§3.9)。CHECK 名も `UNRESOLVED-MOD` に統一。`cfg_test_items()` の返り値に解決失敗行を追加 (§3.4 / §3.6) |
| Cargo.toml の opt-in crate 数が事実誤認 | `grep -rn "workspace = true" --include=Cargo.toml` → `[lints] workspace = true` は **10 crate** (ui/crates 3 + examples 7、各 `Cargo.toml:11-12`)。`grep -rn too_many_lines --include='*.rs'` は 34 行、うち 1 行は `renderer/src/pipelines/texture.rs:102` の doc comment → 実数 **33 箇所** (ui/crates 24 / daw_gui 9) | **採用**。§0.2-9 / §6 / §8-6 / §10 の数字を修正。`Cargo.toml:124` のコメント自体は正しかったことも明記。§6 に「どちらの案を採り、なぜ他方を採らなかったか」を追記 (承認済み方針が求める「非対称の扱いを決める」への答え。**ユーザー承認を待つ中断点は設けない**) |
| FN-NESTING の ratchet は深さしか止めない | 初版の計測値は `max_indent` の 1 成分。`deep_lines` は「表示専用」だった | **採用**。§3.9 で計測値を `max_indent/deep_lines` の 2 成分ベクトルに変更。§4.3 の budget モードを成分ごと比較に一般化 (`budget_le`)、§4.3.4 に肥大検出の self-test、§5.1 のヘッダ説明、§7.5 の CLAUDE.md 文言も同期 |
| `docs/plan_rmd_77_arrangement_split.md` が漏れている | grep で `:36-38` `:264` `:265` `:1173-1174` `:1231` の 5 か所。`render.rs` は 物理 861 行 / 実コード 681 行で FILE-BUDGET の baseline に無い | **採用**。§2 の表と §7.8 に追加。#77 の分割後見積りが物理行であること、`render.rs` が新規違反になり得ることまで書いた。§9-8 も「3 行削除するだけ」から書き直し。着地済みの `plan_rmd_71/73/75` は §7.9 で注記のみ |
| `strip_comments` の lexer 化を「範囲外」としたのは妥協 | `scripts/arch_lint.sh:14` は行頭 `//` のみ。raw string 中の行頭 `//` = 15 行 (video_fx.rs 14 / texture_interop.rs 1)。既存 baseline 4 行はすべて実コード行 (`app_types.rs:1519` / `state/ipc.rs:57,78,182`)。15 行に check 1/2/3/5/8 のパターンは 0 件 | **採用 (計画を反転)**。§1.6 で根治する方針に変更し、`STRIP-COMMENTS-BLIND` という新 CHECK と baseline 2 行を **廃止**。`--filter-comments` を新設 (§3.10)、`strip_comments` を差し替え (§4.1)、配線 canary (§4.2) と self-test (C) を追加、§8-5 に実機確認を追加。「今日は違反件数が変わらない」ことも実測で示した |
| `--self-test` の起動場所が承認済み方針から逸脱 | `.claude/skills/arch-review/SKILL.md:26` は `bash scripts/arch_lint.sh` を直接叩く。Makefile にだけ置くとこの経路で self-test が走らない | **一部採用 / 一部反論**。配置は `arch_lint.sh` の canary ブロックのまま (§4.2)。理由: この改訂に渡された承認済み方針が求めているのは「`--self-test` を持つこと」で、起動場所は指定されていない。かつ arch_lint.sh に置けば `make arch-lint` 経由でも必ず走る = Makefile 配置の **上位互換**で、経路が減る方向の変更ではない。**ユーザー承認を待つ中断点は設けない** (フェーズ分け禁止)。代わりに、この配置の帰結 (python が壊れると arch-lint 全体が止まる) を §7.1 / §7.5 / §9-6 に明記した |
| 保存則の内訳定義が曖昧 | `FileMetrics` は ncloc にだけ「テストを除いた」と注記していた | **採用**。§3.3 に「doc / comment / blank も 4 種すべてテスト範囲を除いた数、`test` は範囲内の全行 (種別を問わず)、範囲が重なったら union」を明記。§1.2 に範囲の開始行 (最初の属性行の `#`) も明記 |
| check 番号が計画内で 3 通り | §2 表 / §4.4 末尾 / 実際の record 数が食い違っていた | **採用**。`KEY-COLLISION` 追加後は **6-10** (5 本)、`COMMON-DEPS` = 11、`UI-DOMAIN` = 12 に統一 (§4.4)。`arch_lint.sh:18-27` の「8 チェック」も 12 に更新する指示を追加 |
| §3.6 の item 終端規則が両義的 | 減算前に判定すると `mod tests { }` が閉じず、production がテスト扱いになる | **採用**。§3.6 を「`}` で −1 した **結果が 0** になったらそこが終端」「深さ 0 のままの `;` も終端」「`}` の直後の `;` は範囲に含める」に書き直し、誤実装したときの症状も併記。§3.7 の本体開始も paren/bracket/angle の 3 深さで再定義し、`->` を 1 トークンにする要件を §3.5 に追加 |
| `STRIP-COMMENTS-BLIND` の 15 と 14 が食い違い | §3.9 の例が 15、§5.2 の baseline が 14 | **解消**。この CHECK 自体を廃止したので数値の食い違いも消えた |
| `::…::` プレースホルダが自由関数に当たらない | `track_inspector/mod.rs:228` は同ファイルに `^impl ` が 0 件の自由関数。`audio_editor.rs:139` / `arrangement_view.rs:58` / `transport.rs:263` / `root.rs:720` も同様。`text_input.rs:186` は `impl<'a, M: ?Sized + 'static> Ui<'a, M>` の中 | **採用**。§5.2 の FN-BUDGET 15 行をすべて実キーに置き換え、根拠 (file:line) を併記。§3.7 に「自由関数はスコープ空」を明記し、§3.12 (A)-16/17 に self-test を追加 |
| python スタブ時に arch-lint 全体が止まる帰結が未文言化 | §4.2 の self-test は canary ブロック = checks 1-12 より前。さらに `strip_comments` も python 依存になった | **採用**。§7.1 の Makefile コメント、§7.5 の CLAUDE.md `:414`、§9-6 に明記 |

### 11.2 第 2 次裏取りの指摘に対する処置 (2026-08-28 の改訂)

裏取り側は §1-§3 の仕様どおりに lexer を独立実装し、走査 389 / 生成物 2 / 統合テスト 69 /
`cfg(test) mod` 解決 5 / FILE-BUDGET 23 行の ncloc / FN-BUDGET 15 行 / FN-NESTING 126 行 /
`run.rs::arrangement = 11/520` / `compile.rs = ncloc 552 / doc 104 / comment 205 / test 2078` /
`video_fx.rs 14 + texture_interop.rs 1` を **すべて再現**した。§4.3.2 の `budget_le` と
§4.3.3/§4.3.4 の `classify` 分岐も切り出して 13 ケース実行し期待どおり。
§1.6 の「今日は違反件数が変わらない」も check 1/2/3/5/8 の delta で確認済み。
以下は残った指摘への処置。

| 指摘 | 実コードでの確認 | 処置 |
|---|---|---|
| `LOC-BUDGET-OK` の例の `0 key-collision` が事実と違う (実測 17〜18 件) | 独立実装で 17 件を再現。内訳は cfg 対 (`shmem.rs:32/170` の `mod imp` × 6 / `single_instance.rs:24/137` × 2 / `devices.rs:1197,1216` / `about.rs:119,152` / `main.rs:831,848` ほか) と trait impl 対 (`midi_import.rs:120,128` の `impl From<…> for MidiImportError`) だけ | **採用 + 設計を変更**。数字を直すだけでは `#2` の連番が残り、並べ替えで天井が入れ替わる (`arch_lint.sh:91-92` が避けた故障の再生産)。§3.7 で **キーに trait 名と cfg 述語を含める**ことにし、**衝突 0 件**になることを実測で確認。連番は廃止し、残った衝突は `KEY-COLLISION` (grep 形式) として ratchet に出す (§3.9 / §4.4)。self-test に (A)-18〜22 を追加 |
| §5.2 FN-NESTING の `voicevox.rs::start_synth_thread` がスコープ空になっていない | `:449 impl VoicevoxBuiltin {` の中の `:486`。実測 11/173、ncloc 237 | **採用**。§5.2 のキーを `…::VoicevoxBuiltin::start_synth_thread` へ修正。併せて FN-NESTING 例示リストの他 5 本 (`render.rs:9` / `view_build.rs:47` / `run.rs:7` / `release.rs:10` / `daw_audio/src/main.rs:851`) が自由関数であることも実コードで確認して明記 |
| §3.8 の行内マーカー範囲に必要な「最初の属性行」を `fn_items()` が返さない | §3.4 の返り値は `(qualified, decl_line, body_open, body_close)` の 4 つ | **採用**。`fn_items()` の返り値を **5 つ**にし `attr_line` を追加 (§3.4)。§3.8 に「宣言行からの後方走査で代用しない」理由を明記。self-test (B)-34 を追加 |
| §4.2 の配線 canary (3) が python の故障を「grep のせい」と報告する | `canary_ok=0` は `arch_lint.sh:62-66` の「検査器の正規表現が効いていません / この環境の grep に既知のパターンが通りませんでした」に合流する | **採用**。canary (3) を `canary_ok` から切り離し、python を名指しする専用の出口にした。置き場所も `if [ canary_ok -ne 1 ] … fi` の**後ろ**へ移し、grep 由来の故障を先に報告させる (§4.2)。§8-5 にメッセージの確認手順を追加 |
| 実行時間に一言も触れていない | 独立実装の実測 6.3 秒 / この改訂の lex のみ実装で 2.7 秒 (389 ファイル・約 21.2 万行) | **採用**。§8 末尾に所要時間の目安と内訳 (`--filter-comments` は stdin の path しか lex しないので 5 回でも安い) を追記。**キャッシュ / 間引き / 並列化を入れない**ことを §9-10 に明記 |
| §8-2 の受け入れ条件が「一致」なのか「±2 許容」なのか決まらない | 独立実装 2 本が 23 行すべて完全一致。`arrangement/mod.rs` は **1,249** (初版の 1,251 は粗い測定の混入) | **採用**。§5.2 の「±2 行ずれ得る」を撤回し、**1 行でも食い違ったら判定器を疑う**へ。§8-2 に「`key-collision` が 17 なら §3.7 の実装漏れ」という具体的な外し方も書いた |
| §5.2 の FILE-BUDGET 23 行 / FN-BUDGET 15 行に第 4 field (理由) の指示が無い | baseline ヘッダと `CLAUDE.md:419-421` は理由必須。FN-NESTING だけ 5 ブロックの指示があった | **採用**。§5.2 の FILE-BUDGET 節に A〜D の 4 ブロック (r.md #77 系 / view・handler / FFI・IPC / daw-ui) を定義し、FILE-BUDGET と FN-BUDGET の第 4 field も短いタグにする方針を明記。「1 行も未分類で残さない」「164 行すべてにブロック見出しとタグが付く」ことを完了条件にした |
| §8-5 が `.rs` を一時的に編集する (「1 行も触らない」方針との緊張、`run.rs` は #77 の対象) | `script.rs` / `run.rs` / `video_fx.rs` はいずれも追跡下 | **採用 (手順を作り直した)**。§8-5 は **追跡下の `.rs` を 1 つも触らない**形へ全面改稿: FILE/FN-NESTING の ratchet は **baseline の天井を一時的に下げる**ことで同じ比較経路を検査し、`strip_comments` の raw string 検査は **未追跡の使い捨て `.rs` を `common/src` に置く** (check 2 の `grep -rn` は未追跡も読む)。天井を **上げて**緑のままであることも確認する項を追加。§8-9 に `git status --porcelain` の確認を追加 |
| `GEN_SCAN_LINES = 40` が `ara-sys/build.rs:47` の `@generated` と 7 行差 | `build.rs:47` に `.raw_line("// @generated by \`cargo build -p ara-sys --features regen\` …")`。マーカーを含む `.rs` 行はこの 1 件と `bindings.rs:1,3` / `binding_ffmpeg_7.1.rs:1` だけ | **採用**。行数窓を廃止し、**「先頭の連続コメント / 空行ブロックの中にあること」**へ (§1.4 / §3.2)。`build.rs:47` はコード中の文字列リテラルなので原理的に当たらない。結果は今日と同じ 2 件。self-test (A)-24/25 を追加 |
| §3.3 の保存則で `raw` が未定義 (`split("\n")` の末尾要素で全ファイル assert 落ち) | `.rs` 391 本のうち **末尾改行なし 0 / 単独 CR 0 / CRLF を含むもの 28** | **採用**。§3.3 に `raw` の定義を明記: `newline=""` で読み (universal newlines を使わない)、`split("\n")` の末尾空要素を 1 つだけ落とす。CRLF 28 ファイルのために各行末の `\r` を 1 つ落とす手順も明記。§0.2-13 に実測を記録。self-test (A)-23 を追加 |
| `docs/plan_rmd_77` の引用行番号が全部ずれ | `:47-50` / `:288` / `:289` / `:1501-1502` / `:1588` (現在 1,621 行) | **採用**。§2 と §7.8 の行番号を実測値へ修正。**着手前に再 grep する**指示も §2 / §7.8 に入れた |
| `plan_rmd_71` / `plan_rmd_75` の行番号ずれ | `71` は `:49` / `:1317-1318` / `:1797`、`75` は `:1883` / `:1885` | **採用**。§2 / §7.9 を修正。裏取りが「`plan_rmd_73:1283` のみ正しい」としていた点は**誤り** — 実際は `:1875` / `:1877` (`:1283` は automation bend の実装コード)。§7.9 に正しい行番号を書いた |
| `plan_rmd_74_disclosure_glyph.md` が一覧に無い | `:960` / `:962` / `:1049` の 3 か所 | **採用**。§2 の表と §7.9 に追加 |
| `plan_rmd_71:1315-1320` (D-4-0 の根拠) と `:49` が一覧に無い | 実際は `:1317-1318`。「2,623 行で 3,000 行まで 377 行しかない」が分割根拠 | **採用**。§2 / §7.9 に追加。新指標では実コード 2,214 行 = budget の 2 倍超で**根拠が強まる**ことも書いた |
| §2 の確認用 grep が `≤3000` を取りこぼす | `plan_arch_refactor.md:28` 「8 モジュール ≤3000 に分割」/ `:32` 「5 モジュール ≤3000 分割」/ `:470` 「行数 budget」 | **採用**。§2 の grep を `3,000\|3000\|god file budget\|god-file budget\|god-file\|行数 budget` へ拡張。`:470` は書き換え対象として §7.6 に追加、`:28` / `:32` は当時の記録なので注記対象に加えた |
| `.claude/guards.jsonl:113-122` / `scripts/test_guards.py:478-483,937-938` の「意味は同じ」が偽になる | ガードは Python の行正規表現、arch-lint は字句解析。差が出るのは raw string 中の行頭 `//` とブロックコメント内 | **採用**。§2 の表に両ファイルを追加し、§7.10 を新設。**ルール行 / テストケース / ロジックは触らず、コメントだけ**を直す。割れる方向が「ガードの方が広く nudge」= 安全側であることも書いた |
| §4.5 / §4.1 の引用行番号 | `classify "$HITS" 0` は `:305` (`:304` は見出し)。「skip の緑を作らない」原則の記述は `Makefile:203-206` (`:213` が cargo-deny の hard fail 行) | **採用**。§4.5 / §4.1 を修正 |
