# gui_01 → daw_01 統合プラン (monorepo 化)

最終更新: 2026-06-14 / 状態: **プラン確定・実行待ち (in-flight worktree 完了が前提)**

## 1. 決定と背景

- **決定**: gui_01 (daw-ui) を daw_01 に統合し、**1 リポジトリ・1 Cargo workspace・1 セッション**にする。
- **ユーザー確認 (2026-06-14)**: gui_01 を独立公開・他プロジェクトで使う予定は **なし** → 統合する。
- **解決する 2 つの問題 (同一原因)**:
  1. daw_01 が gui_01 に委譲すべき widget を **自前実装**してしまう → revert/rework が発生
     (実例 #073 level_meter dB 目盛を `daw_gui/src/view/mixer_strips.rs` に描いて全撤去)。
  2. **worktree が機能しない**。gui_01 は daw_01 の submodule でも workspace member でもないため
     worktree に同梱されず、全 daw_01 worktree が**単一の `F:/dev/gui_01` working copy を共有**。
     branch ペア (daw_01 feature ⇔ gui_01 feature) を 1 worktree に isolate できない。
- **根本原因**: 「2 リポジトリ + relative path 依存 + 2 セッション」という分割そのもの。
  gui_01 は daw_01 依存ゼロ・実利用者は daw_01 のみ・crates.io 未公開 (`publish = false`) なので、
  分割の便益 (再利用可能な独立ライブラリ) は現状ほぼ机上。コストだけが毎日かかっている。
- **重要 — 統合しても委譲規律は失わない**: crate 境界 (`daw-ui-core` ⇔ `daw_gui`) は維持する。
  widget の幾何/SSoT は引き続き `daw-ui-core` crate に置く。規律を強制していたのは「別セッションに頼め」
  という**壁**ではなく「このコードは `daw-ui-core` crate に属す」という**crate 境界**の方。
  消えるのは friction の元であるセッションの壁だけ。よってこれは trade-off ではなく strict win。

## 2. 最終形 (ディレクトリ / workspace)

gui_01 全体を `daw_01/ui/` 配下に **subtree merge (履歴保持)** する。

```
daw_01/
  common/  daw_gui/  daw_audio/  daw_plugin_host/   # 既存 4 crate
  ui/                                               # ← 旧 F:/dev/gui_01 をまるごと
    crates/platform/   (daw-ui-platform)
    crates/renderer/   (daw-ui-renderer)
    crates/ui/         (daw-ui-core)
    crates/examples/*  (27 crate: mixer/arrangement/piano_roll … + snapshot/verify ハーネス)
    docs/  (gui_01 設計ドキュメント plan.html 等)
```

- **crate 名は据え置き** (`daw-ui-platform` / `daw-ui-renderer` / `daw-ui-core`)。改名はチャーンのみで便益なし。
- **単一 `[workspace]`**: root `Cargo.toml` に `ui/crates/*` を members 追加。`ui/Cargo.toml` の `[workspace]`
  は削除 (workspace の入れ子は不可)。
- **抽出可能性は維持**: `ui/` は daw_01 への依存ゼロを保つ。将来気が変わっても `git subtree split` で
  独立 repo に戻せる。今 monorepo にするのは「分割税を毎日払うのをやめる」ためで、不可逆ではない。

## 3. 前提条件 (実行は全 in-flight worktree 完了後)

**実行前に以下を両 repo の main に land させ、両 working tree を clean にする。**

| worktree | branch | 状態 (2026-06-14) |
|---|---|---|
| `F:/dev/daw_01_video_fx` | `feature/video-fx` | video_fx 機能。未コミット多数 + 新規 `common/src/video_fx.rs`, `daw_gui/src/video_fx/` |
| `F:/dev/daw_01_mod_followups` | `feature/modulation-followups` | modulation followups。未コミット |
| `.claude/worktrees/fixme-56-modulators` | `worktree-fixme-56-modulators` | modulators。未コミット |
| `F:/dev/gui_01_video_fx` | `feature/video-fx` | gui_01 側 video-fx (0d6d17f, working tree clean) |

理由: ディレクトリ移動 + path 依存付け替えを**未完成 feature に被せると**、各 feature の land 時に
`Cargo.toml` の path-dep conflict + gui_01 側変更の二重取り込みが連発する。clean state から **1 回で**
行うのが ideal。特に video-fx は **cross-repo** (gui_01 側にも feature commit) なので、統合後の
単一 repo で land する方が圧倒的に単純 (gui_01 commit を別途 `ui/` に取り込む手間が消える)。

## 4. 実行手順 (clean state 到達後)

### Phase 1 — subtree merge (履歴保持)
1. main から統合用 worktree/branch を作る (clean tree 必須)。
2. `git remote add gui_01_local F:/dev/gui_01`
3. `git fetch gui_01_local`
4. `git subtree add --prefix=ui gui_01_local main`
   → `ui/` に gui_01 の全ツリー + 212 commit の履歴が入る (大きな merge commit が 1 つできる = 意図通り)。
   ※ gui_01 main は video-fx 等が land 済みの最新であること。

### Phase 2 — workspace 統合 (1 commit)
root `Cargo.toml`:
- `members` に `ui/crates/{platform,renderer,ui}` + `ui/crates/examples/*` (27) を追加。
- `[workspace.dependencies]` の `daw-ui-*` path を `../gui_01/crates/*` → `ui/crates/{platform,renderer,ui}`
  に変更 (この 3 行が daw_gui と ui/ 内部 crate の両方の解決元になる = 単一定義)。
- `[workspace.package]` に **`publish = false` / `rust-version` / `license` を追加**。
  → ui/ の crate は `version/edition/rust-version/license/publish` を `*.workspace = true` で継承するため
  必須 (現状 root は version/edition しか定義していない)。daw_01 既存 crate は version/edition しか参照
  しないので追加しても無影響。
- `[workspace.lints]` を gui_01 から移植 (rust: `unsafe_op_in_unsafe_fn`/`unused_must_use`、
  clippy: `all`+`pedantic` warn + 緩和群)。
  → ui/ の crate は `[lints] workspace = true` で opt-in。**daw_01 既存 crate は opt-in しないので
  pedantic は適用されず `clippy -D warnings` は壊れない**。
- `[profile.dev]` (opt-level=1) / `[profile.dev.package."*"]` (opt-level=3) / `[profile.release]`
  (lto="thin", codegen-units=1) を gui_01 から移植。profile は workspace 全体に効く (root のみ有効)。
  gui_01 と同じ wgpu スタックを共有するので採用が正。**注意: release が lto=thin になり post-commit
  release build が遅くなる**が、これは release artifact の codegen 品質として正しい挙動。

`ui/Cargo.toml`:
- `[workspace]` / `[workspace.package]` / `[workspace.lints]` / `[workspace.dependencies]` / `[profile.*]`
  を削除 (すべて root に集約済み)。ui/crates/* の `daw-ui-*.workspace = true` は root 側で解決される。

`daw_gui/Cargo.toml`:
- `daw-ui-*` 依存は `workspace = true` 経由なので **変更不要** (path 変更は root の 1 箇所だけ)。

外部 deps (wgpu/winit/glyphon/taffy/cosmic-text/raw-window-handle/windows/windows-core/windows-sys/
bytemuck/pollster/rfd/arboard) は gui_01 crate の **per-crate 宣言**なのでそのまま移動して解決する。
単一 workspace = **単一 Cargo.lock** なので wgpu/winit/windows は自動で 1 バージョンに unify され、
feature は全 consumer で union される。**旧来の手動 byte 一致同期問題は消滅する** (Phase 5 で SSoT 化)。

### Phase 3 — build green
- `cargo metadata` で 30+ crate の解決を確認 →
- `cargo build --workspace` →
- `cargo clippy --workspace -- -D warnings` →
- `cargo test --workspace` (ui/crates/examples の snapshot/verify ハーネスもここで回る)。

### Phase 4 — 重複解消 (別 commit): mirror-drift hazard 撤去
統合で上流 crate を直接編集できるようになるので、daw_gui の手写しミラーを撤去:
- `daw_gui/src/view/window.rs::DawGuiWindow` = `daw-ui-platform` の private `WinitWindow` の手写し
  (WindowBackend + Windows TSF/IME ITextStoreACP 配線)。
- `daw_gui/src/view/runner.rs::Runner` = `winit_backend::run_app` の差し替え (gui_01 のは `EventLoop<()>`
  で user-event channel が無く、daw_01 は背景スレッド用に `EventLoopProxy<AppEvent>` が要る)。

対応: `WinitWindow::new` を `pub` 化、または **user-event 対応の `run_app` を platform crate に正式 API**
として追加し、`DawGuiWindow`/`Runner` の重複を platform crate 実装へ一本化。
→ 「gui_01 が default trait method を足すと `DawGuiWindow` が無音 no-op 化」hazard が型レベルで解消。
(参照 memory: `project_dawguiwindow_mirrors_winitwindow`)

### Phase 5 — 依存 SSoT 化 (別 commit, ideal)
ui/crates/* が直書きしている外部 deps を root `[workspace.dependencies]` に集約し、各 crate を
`workspace = true` 化。wgpu/winit/windows 等のバージョンが単一 SSoT になり、旧 Cargo.toml の
「gui_01 と byte 一致で維持せよ」コメント (root Cargo.toml L51-54, renderer Cargo.toml L19-22) が不要に。

### Phase 6 — ワークフロー / ドキュメント更新
- `CLAUDE.md`: 「gui_01 は参照のみ・実装変更は gui_01 session で」「`docs/gui_01_conversation.md` 運用」
  「path 依存 `../gui_01`」節を削除/書き換え → 「UI lib は `ui/` にあり同セッションで直接編集」。
  gui_01 要望系の memory/feedback (request-before-interim, link-plan-ref, scope-review, auto-resume,
  conversation, progress-while-waiting) は「往復プロトコル」前提なので役割を見直す
  (委譲規律は残すが「別セッションに要望を出す」手順は消える)。
- `docs/gui_01_conversation.md` (+ archive) → 歴史記録としてアーカイブ (`ui/docs` 配下へ移動 or リネーム)。
- gui_01 の `.claude/skills` (implement/debug-ui/review/research-similar-impl) を daw_01 `.claude/skills`
  と統合 (重複は daw_01 を正、不足は取り込み)。
- gui_01 の AHE memory dir (`F--dev-gui-01`) を daw_01 側 (`F--dev-daw-01`) に統合検討。
- gui_01 の設計ドキュメント (plan.html / history.html 等) は `ui/docs` として保持。

### Phase 7 — 旧 repo / worktree 退役
- `F:/dev/gui_01`, `F:/dev/gui_01_video_fx`, `.claude/worktrees/gui_01` symlink を撤去。
- 旧 daw_01 worktree の `../gui_01` path 依存はもう不要。

## 5. リスク / 注意

- **subtree merge は大きな merge commit を作る** (gui_01 212 commit を内包) — 履歴保持のため意図通り。
- **release ビルドが lto=thin で遅くなる** (Phase 2) — release codegen 品質として正しい挙動。post-commit
  release build hook の所要時間が増える点だけ承知しておく。
- **他 agent / worktree との contention** (memory `feedback_concurrent_agent_file_contention`):
  実行は全 in-flight land 後の clean state でのみ。実行中は他セッションが両 repo を触らない。
- **ロールバック**: 統合は専用 branch で行い、build/clippy/test green + 実機 smoke test
  (`--smoke-test` + 手動起動) を通すまで main に merge しない。branch ごと破棄すれば原状復帰。

## 6. 検証 (実行フェーズで)

- `cargo build --workspace` / `cargo clippy --workspace -- -D warnings` / `cargo test --workspace`
- `cargo run -p daw_gui` (起動 + 子プロセス handshake をログ確認)
- `cargo run -p daw_gui -- --smoke-test daw_gui/tests/fixtures/smoke_test.mp4` (video preview regression)
- `ui/crates/examples` の snapshot/verify ハーネス run (gui_01 の視覚回帰ネット)
