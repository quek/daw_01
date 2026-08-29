# daw_01

VOICEVOX 歌声合成を組み込んだ Rust 製 DAW。Cargo workspace (Edition 2024)、実行時は
`daw_gui` / `daw_audio` / `daw_plugin_host` の 3 プロセスが協調する。設計は [DESIGN.md](DESIGN.md)、
UI ライブラリ (`ui/` = daw-ui、旧 gui_01) の使い方と罠は [ui/CLAUDE.md](ui/CLAUDE.md)。

## 大原則

理想とベストプラクティスを追求する。
そのためには実装コストは無視して大胆に破壊して作り直す。

**何を作るかの判断**に次を持ち込むことを禁止する — 実装コスト / 実装難易度 / 変更規模 /
コンパイルがとおらないこと / 作業時間。

**順序と分け方は別**。統合順・並列 worktree の切り方・大改造の着手前確認は、規模と衝突の実測を
根拠に決めてよい (むしろ決めること)。禁じているのは「安いほうを**選ぶ**」ことであって、
「安全な順に**積む**」ことではない。

## Development Workflow

**Makefile が SSoT。素の `cargo build --workspace` / `cargo test --workspace` は使わない**
(examples まで毎回フルビルドする無駄が出る)。

```bash
make build          # 実行 3 exe をビルド (debug)
make run            # daw_gui をビルド × 起動
make test           # テストを持つ package のみ (daw_gui を起動する。下記)
make test-nolaunch  # そのうち daw_gui を起動しない target だけ
make clippy         # -D warnings
make check          # 型検査のみ
make arch-lint      # アーキテクチャ不変条件の機械検査
make gates          # 常設ゲート (clippy / test / test-nolaunch の前提条件)
make license-check  # ライセンス表示 (REUSE / GPLv3 互換性)
make audit          # 依存の脆弱性・供給網攻撃 (network 要)
make fetch-ffmpeg   # third_party/ffmpeg 取得 (gitignore なので fresh machine で必須)
```

特定 crate / test だけなら `cargo check -p <crate>` / `cargo test -p <crate> --test <name>` に
絞ってよい。避けるのは `--workspace` の無条件多用。

**`gates` は `clippy` / `test` / `test-nolaunch` の前提条件**なので、意識せず必ず通る
(license-check + lockfile-guard + 「`Cargo.lock` を変えたのに `make audit` を通していない」の検出)。
`audit` 本体は advisory DB にネットワークが要るので前提条件にはしない — 代わりに
**lock が HEAD と違うのに監査済みスタンプと一致しなければ落ちる**。`cargo update` を打ったら
`make audit`。方針は `deny.toml`、`ignore` を足すときは「RUSTSEC-ID: 理由 / 見直し期限」を
コメントで必ず書く (無言の ignore は禁止)。

`worktree-rm` / `worktree-rm-merged` は `test-worktree-rm` (削除ツールの回帰テスト、約 25 秒) を
前提条件に持つ。取り返しがつかない操作なので、その直前が唯一漏れない位置。

### `make test` は daw_gui を起動する

`daw_gui/tests/` の一部は **daw_gui 本体を `--script` で subprocess 起動**し、daw_audio /
daw_plugin_host まで spawn して audio device を開く。窓を出さず single-instance gate も素通り
するので **起動したことに誰も気付けない**。実機を触っている最中に回すと、開いているプロジェクトの
再生を壊す。

- **判定基準は 1 つだけ**: `grep -l CARGO_BIN_EXE_daw_gui daw_gui/tests/*.rs`。**名前で判断しない**
  — `pdc_real_vst3` / `sidechain_real_vst3` は smoke が付かないのに起動し、`arr_widget` /
  `pr_widget` / `font_picker` は起動しない。`--test` で名指ししても基準に当たれば起動する。
- **起動を伴わない検証だけなら `make test-nolaunch`**。対象は Makefile が上の基準から毎回導出する
  ので、手で列挙しないこと。許可を得て回すときは `DAW01_ALLOW_LAUNCH=1` を頭に付ける。
- `make test` / `make run` / `make run-release` は `scripts/preflight_no_running_app.sh` が前提条件で
  止める (ユーザーが手で打っても効く)。迂回は `DAW01_SKIP_PREFLIGHT=1`。

### ビルドと検証の区別

`clippy` / `check` / `test` は実行バイナリを作らない。`target/debug/daw_gui.exe` を走らせる前に
必ずビルドする。**子プロセスの挙動を変えたら `make build` で 3 exe を揃える** — 子 exe が古いと
IPC の decode に失敗し、「再生が止まる」形で出る ([[feedback_workspace_build_for_protocol_changes]])。
起動中のプロセスのバイナリは上書きされないことがある (Windows の ERROR 5)。

### IPC 境界で送る型

protocol 型 (`AudioCommand` / `AudioEvent` / `PluginCommand` / `PluginEvent`) と、それが保持する
内側の型すべてに `#[derive(bincode::Encode, bincode::Decode)]` が要る。足したら `make build`。

### vendored FFmpeg

`third_party/ffmpeg` は gitignore なので fresh machine では `make fetch-ffmpeg` (idempotent、
`build` / `test` / `check` の前提条件)。取得は URL + sha256 固定 + ミラーへの fallback で、pin の
SSoT は `scripts/fetch_ffmpeg.sh`。詳細と LGPL 上の義務は [docs/ffmpeg_mirror.md](docs/ffmpeg_mirror.md)。

**git に無いので `rm` / `git worktree remove` が third_party junction を辿ると本体ごと消え、
復元できない** (2026-06-14 に実際に起きた)。Claude Code の worktree は `.worktreeinclude` で
実コピーされるのでこのハザードは無いが、手で junction を張ったら消す前に
`cmd //c rmdir <junction>` で外すこと。

## スクリプトの書き方

**PowerShell 禁止。bash を既定**とし、**JSON を構造的にパースするものだけ Python (stdlib のみ)**
で書く (Linux でも動くこと。[[feedback_no_powershell_cross_platform]])。

**hook は使わない** (`.claude/settings.json` を置かない)。強制は `make` の検査と `/review` skill が
担う。どちらも結果が出力に出るので、黙って壊れても気付ける。出力に出ない検査は、壊れていても
誰も気付けない。

## 応答・コミット

- 応答は日本語 / コミットメッセージは日本語 / 技術用語は英語のまま可

## Coding Principles

### 最終形まで実装する

**禁じているのは「途中で報告して承認を待つこと」であって、計画を段階に割ることではない。**
大規模改修を `docs/plan_*.md` で段階に割り、並列 worktree の統合順まで決めるのはむしろ推奨。
だめなのは「Phase 1 完成しました。Phase 2 に進みますか」で手を止めること
([[feedback_dont_stop_prematurely]])。実装方針 / 分割単位 / 命名 / テストの粒度、一次情報を
読めば決まること、同じ root cause の同件修正 ([[feedback_sibling_occurrence_check]]) は聞かずに進む。

**止まって聞く場面は 4 つだけ**:
- **着手前** — UI の見せ方・操作 (閉じ方 / 移動 / リサイズ / 永続範囲 / 並び / 背後操作) を確定
  させる。省くとイメージ違いで全書き直しになる ([[feedback_grill_ui_presentation_first]])。
- **着手前** — 要件が 2 通りに読め、どちらを取るかで作るものが変わるとき。**1 問ずつ、上流から、
  番号付きの選択肢で** ([[feedback_one_question_at_a_time]] / [[feedback_numbered_question_options]])。
- **commit の直前** — 実機 / 視覚の sign-off ([[feedback_confirm_before_commit]])。
- **完全に手詰まりのとき** — 権限・外部要因で先へ進めないと確定したとき。

### 妥協を選択肢に上げない

出すべき問いは 2 つだけ — どれが **理想** か? / 理想を実現するには何を破壊する必要があるか?
出してはいけない問い — どれが **実装コストが低い** か? / **影響範囲が狭い** か? /
**caller boilerplate が少ない** か? / **現実的** か?

「実装コスト」「影響範囲」「連鎖する」「許容範囲」「現実的に」「妥協」 — これらが思考に出てきた
**時点で**、理想以外の選択肢を比較対象に上げてしまっている
([[feedback_pursue_ideal_only]] に、明示指示があったのに違反した実例)。

### まず調べる

一次情報 (公式ドキュメント / spec ファイル / 参照実装ソース) を引用 URL・行番号つきで確認して
から書く。推測で書かない。ユーザーの発言は調査の方向ヒントとして扱い、最終根拠は一次情報で取る。

**引用付きで集めた時点ではまだ根拠にならない。** 反証する側を独立に立て、潰せなかった主張だけを
設計判断の根拠にする。潰すべき失敗パターン:
- **実行して確かめずに書いた主張** — 結論が使われる**その経路で**動かして確かめる。
- **測定器そのものが交絡している主張** — `env -i` 相当の対照を取る ([[feedback_diagnostics_can_lie]])。
- **範囲の誇張** — 母数と分母を必ず数える。
- **隣の項目との衝突** — その推奨が、別の未解決項目に依存していないか。

「調べた」と「確かめた」は別。「呼んだ」と「効いた」も別 ([[feedback_called_is_not_worked]])。

### SSoT / エラー

- 同じデータを複数箇所に複製しない。「誰が所有し誰が更新するか」を決めてから実装する。
- **規範も同じ**。機械が持てるものは機械に持たせ、散文は 1 行要約 + リンク。**原文を引用して
  再掲しない** — 片側だけ更新されて静かに食い違う。
- `?` を安易に `ok()` / `unwrap_or_default()` に置き換えない。FFI・CLAP コールバック・IPC の
  エラーは根本原因を調べてから対処する。
- 最小限の実装で目的を達成する。不要な抽象化を作らない。1 関数 1 責務。

### テスト

**自動で確かめられることをユーザーに頼まない。**
- Rust の `#[test]` で書けるものは書いて自分で回す。
- GUI / IPC / 再生を跨ぐ切り分けは `daw_gui --script <js>` の headless モードで自分でやる
  (`daw_gui/tests/scripts/*.js` はそのシナリオ記述であってテストではない)。同じ実機操作を何度も
  頼まない ([[feedback_prefer_headless_verification]])。**頼むのは最終 sign-off だけ**。
- 逆に**自明な修正に回帰テストを書かない**。本番の算術をテストへ写して突き合わせるだけの
  テストは特に禁止 ([[feedback_no_tests_for_simple_cases]])。

### デバッグ

実データから始める (コードパス推論より速い)。上流→下流 (UI/コマンド → Model → IPC →
Plugin Host → プラグイン本体) の順で切り分ける。個別関数が正しくてもパイプライン全体が
壊れていれば無意味なので、フルサイクルで検証する。

## Real-Time Audio の制約（最重要）

オーディオコールバック (daw_audio の再生スレッド、および CLAP `process()` に至るパス) では
次を厳守する。違反するとドロップアウト・クラックルが起きる。

- **ヒープ確保禁止**: `Vec::new()` / `format!()` / `String` / `.collect()` / `Box::new()` を
  呼ばない。バッファは再生開始前に確保して使い回す
- **ロック禁止**: ブロッキングロックを取らない。UI ↔ 再生スレッドはロックフリーキューか Atomic
- **I/O 禁止**: ファイル I/O・ログ出力・`println!` を呼ばない
- **システムコール最小化**: `Instant::now()` は許容、`thread::sleep` は避ける

## アーキテクチャ不変条件

[docs/plan_arch_refactor.md](docs/plan_arch_refactor.md) で確立。**`make arch-lint` が機械検査**し、
`/arch-review` skill が定期監査する。**「何を違反とみなすか」の SSoT は `scripts/arch_lint.sh`**
(サイズ budget の測り方だけ `scripts/loc_budget.py`)。

**`make arch-lint` の exit 0 は「違反ゼロ、または `scripts/arch_lint_baseline.txt` に記録済みの
ものだけ」を意味する。** baseline に無い違反が 1 件でもあれば exit 1 (行単位 ratchet)。
以前は違反があっても常に exit 0 で、終了コードだけ見て「OK」と報告され続けていた。
**恒久的に正当な箇所は baseline ではなく行内マーカー** `// arch-lint: allow-<check>`
(区別しないと負債が「正当」として永久に隠れる)。baseline の書式は同ファイル冒頭が正本。

> **arch-lint のパターンにバックスラッシュを使わないこと。** make (MSYS2) 経由だと grep へ渡す
> argv のバックスラッシュが落ちる。POSIX ブラケット式 (`[(]` `[]]` `[[:space:]]`) と `grep -w` で
> 書く。これを踏んで **8 チェック中 6 つが無言で無効化されたまま「OK (違反なし)」を出していた**。

1. **安定 id addressing**: プロセス境界・イベント・永続参照に positional index を使わない。
   device = `PluginInstance.id`、send = `Send.id`、note/point/audio event = 要素 id。
   **「削除/並べ替えで参照を貼り替える補償コード」を書き始めたら設計が誤り**。
2. **wire は blob-less**: `LoadSong` の Song は `state` / `ara_archive` を構造的に除外する
   (PluginInstance の手書き bincode Encode)。protocol に `Vec<f32>` / `Arc<[u8]>` の bulk を
   直載せしない (専用 message / WAV materialize / id 参照で運ぶ。16MB wire 上限は防御であって
   「大きくして解決」しない)。
3. **宛先は型で表現**: IPC は `AudioCommand` / `AudioEvent` / `PluginCommand` / `PluginEvent`。
   単一 enum (旧 MainToChild / ChildToMain) に戻さない。「相手が無視する variant の no-op arm」が
   生えたら分割が壊れているサイン。
4. **RT スレッドは無限待ち・確保・解放をしない**: 他プロセスの完了待ちは有界
   (`DISPATCH_TIMEOUT_MS`) + quarantine (`common/src/plugin_ref.rs` の poisoning contract)。
   重い作業は off-thread で構築し rtrb ring で swap。
5. **Song 編集の副作用は単一の口**: undo snapshot / dirty / epoch / 子プロセス sync 予約は
   `edit_song()` チョークポイントが無条件で担う。手動 `push_undo_snapshot`・whitelist・
   view からの song 直接可変参照を追加しない。
6. **live と export は同じ render 関数** (`render_master_buffer`): master fx / master gain を
   含む「1 buffer を描く」処理を二重実装しない。
7. **fingerprint handshake**: wire を渡る型を新ファイルへ切り出したら `common/build.rs` の
   `WIRE_SOURCES` に必ず追加する (protocol 変更の検出網に穴が開く)。
8. **daw-ui core はドメイン知識を持たない**: DAW 固有 widget (arrangement / piano_roll) は
   `daw_gui/src/widgets/` で `common::model` 直結。mirror 型・翻訳 request enum を作らない。
9. **サイズ budget**: **実コード行 (ncloc = 空白・コメント・doc comment を除いた物理行)** で
   1 ファイル **1,000 行** / 1 関数 **300 行** / インデント **6 段**。超過したら分割してから
   足す。**テストコードは対象外**。現在値は `python scripts/loc_budget.py --report`。

**不変条件 2 / 5 / 6 / 8 に対応する arch-lint チェックは無い。** この 4 件は上の本文が唯一の
強制手段なので、圧縮しないこと。

## FFI 境界

- ポインタの null / 境界チェックを必ず行う。整数キャストは `saturating_add` / `try_from` を優先。
  MIDI デバイスやプラグインが書き込むイベント配列はサイズ上限を検証する。
  `from_raw_parts` / `copy_nonoverlapping` は長さの妥当性を検証してから使う。
- **「自分の側で対応する呼び出しが見当たらないから dead」と判定して削除するな。**
  相手側 (wgpu / driver / OS) が**内部で**その protocol / state を消費していることがある。
  実例: worker 側の keyed-mutex Acquire/Release を dead と判定して削除したら、wgpu の DX12 /
  Vulkan import 側がそれを消費していて imported texture が全 pixel 透過になった (`c2ae697` →
  反転 `6b5eebd`)。削除前に一次情報で「相手側で何が起きるか」を確認し、**実機 smoke test で
  挙動が変わらないことを目視**する。不明なら削除しない ([[feedback_no_dead_judgment_at_ffi]])。

### プラグインエディタ窓 (Windows)

設計正本は [docs/plan_plugin_editor_topwindow.md](docs/plan_plugin_editor_topwindow.md)。
**エディタ窓は daw_plugin_host が作る top-level で、owner は daw_gui の本体窓。daw_gui が窓を
「作って」はいけない** — 窓が daw_gui のプロセスに属すると JUCE の `isForegroundProcess()`
(前面窓の**プロセス ID** 比較) が false になり、cascade サブメニューが即 dismiss される。
禁止されるのは「窓の所属プロセス」であって「所有関係」ではない。owner と `WS_EX_TOOLWINDOW` は
必ずセットで、owner が先。CLAP GUI の呼び出し順の落とし穴も同設計正本 §9。

## Visual regression smoke test

video preview の暗転 / 全 pixel 透過 / 一様 fill は `build` / `test` / `clippy` を全部すり抜ける。
**video preview / texture sampling / shared-handle に触れる変更は commit 前に必ず通す。**

```bash
cargo run -p daw_gui -- --smoke-test daw_gui/tests/fixtures/smoke_test.mp4
# exit 0 = 描画されている / exit 1 = blank・一様・透過
```

判定閾値と fixture の作り方は `daw_gui/src/smoke_test.rs` の module doc が正本。

## 参照プロジェクト

- `ui/` — 自作 GUI ライブラリ daw-ui。API は crate doc-comments、サンプルは
  `ui/crates/examples/`、設計正本は [ui/docs/plan.html](ui/docs/plan.html)。
- `sing_like_coding` (作者ローカル) — 前作 Rust DAW。IPC / CLAP ホスト / オーディオエンジンの参照実装。
- `%APPDATA%\REAPER\Scripts\<user>\voicevox\` (作者ローカル) — VOICEVOX API 統合の参照実装 (Lua)。
- clap-host / clap-validator / nih-plug 等の clone 先は `.claude/skills/research-similar-impl/references.md`。
