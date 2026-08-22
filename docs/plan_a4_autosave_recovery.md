<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# A4: autosave + 起動時復元

## Context

`docs/plan.html` Phase 6 の A4。 既存の autosave (30s tick → 60s throttle で `<file_path>.autosave.daw` に save) は **file_path が Some のときだけ動作**、 未保存ファイルは autosave されない。 また起動時の復元 modal、 正常終了時の autosave 削除が未実装。

`maybe_autosave` (app.rs:1121-1152) と `spawn_autosave_timer` (main.rs:434-443) は既にあるので、 不足分の追加が中心。

## 実装方針

### 1. common/src/recovery.rs (新規)

```rust
pub fn recovery_dir() -> Option<PathBuf>;        // %LOCALAPPDATA%\daw_01\recovery\
pub fn ensure_recovery_dir() -> std::io::Result<PathBuf>;
pub fn scan_recovery_files() -> Vec<PathBuf>;    // recovery_dir 内の *.autosave.daw
pub fn recovery_path_for_session(id: &str) -> Option<PathBuf>;  // recovery_dir / "{id}.autosave.daw"
pub fn sidecar_for(file: &Path) -> PathBuf;      // <file>.autosave.daw
```

`uuid` crate を common に追加 (v4 random)。

### 2. AppData フィールド追加 (app.rs)

```rust
pub recovery_session_id: String,            // AppData::new で uuid::Uuid::new_v4().to_string()
pub recovery_candidates: Vec<PathBuf>,      // 起動時 scan + Open 時 sidecar 検出で push
pub show_recovery_modal: bool,              // candidates 非空で起動時 true、 全処理後 false
```

### 3. AppEvent (app.rs)

```rust
RecoveryRestore(PathBuf),  // 該当 .autosave.daw を action_open_path 同等で読み込む + recovery_candidates から remove
RecoveryDiscard(PathBuf),  // ファイル削除 + recovery_candidates から remove
RecoveryDismiss,           // modal を閉じる (candidates は残し、 次回起動時も見える)
```

### 4. maybe_autosave 改修 (app.rs:1121-1152)

```rust
fn maybe_autosave(&mut self) {
    if !self.is_dirty { return; }
    if self.last_autosave.elapsed() < Duration::from_secs(60) { return; }

    let path = match &self.file_path {
        Some(orig) => common::recovery::sidecar_for(orig),
        None => {
            let Some(p) = common::recovery::recovery_path_for_session(&self.recovery_session_id) else {
                return;
            };
            // recovery_dir 自体を作成
            let _ = common::recovery::ensure_recovery_dir();
            p
        }
    };

    match common::project::save(&path, &self.song) {
        Ok(()) => {
            tracing::info!(path = %path.display(), "autosaved");
            self.last_autosave = Instant::now();
        }
        Err(e) => tracing::warn!(error = ?e, path = %path.display(), "autosave failed"),
    }
}
```

### 5. main.rs 起動時 scan

`AppData::new` の直前 or 直後で `common::recovery::scan_recovery_files()` を呼び、 結果を AppData に渡す (フィールド初期値経由)。 `AppData::new` 末尾で `show_recovery_modal = !recovery_candidates.is_empty()` を立てる。

### 6. action_open_path で sidecar 検出 (app.rs:1086-1107)

Open ダイアログで .daw を選んだ後、 `sidecar_for(path)` の存在を確認 → 存在すれば `recovery_candidates` に push + `show_recovery_modal = true` に。 ただし開いたファイル自体はそのままロードする (ユーザーが modal で「復元」 を選んだら sidecar 内容で上書きする)。

### 7. 新規 view/recovery_modal.rs

`plugin_picker.rs` の `Ui::modal` パターンを踏襲。 candidate list を出して各行に「復元 / 破棄」 ボタン、 下部に「閉じる」 ボタン。

```rust
pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, _screen: PhysicalSize) {
    if !app.show_recovery_modal { return; }
    if !ui.is_modal_open("recovery") { ui.open_modal("recovery"); }
    ui.modal("recovery", (W, H), &MODAL_STYLE,
        Some(Box::new(|| Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::RecoveryDismiss);
        }))),
        |ui, panel| {
            // タイトル + 各候補 row (復元 / 破棄 button) + 閉じるボタン
        });
}
```

### 8. 終了時 cleanup (runner.rs)

`WindowEvent::CloseRequested` arm で `state.app.on_shutdown()` を呼んでから `event_loop.exit()`。

```rust
fn on_shutdown(&self) {
    // 自セッションの recovery file を削除 (file_path None で kill されたケース以外は不要)
    if let Some(p) = common::recovery::recovery_path_for_session(&self.recovery_session_id) {
        let _ = std::fs::remove_file(&p);
    }
    // sidecar 削除 (file_path Some で正常終了したら sidecar は不要)
    if let Some(orig) = &self.file_path {
        let _ = std::fs::remove_file(common::recovery::sidecar_for(orig));
    }
}
```

### 9. RecoveryRestore / RecoveryDiscard handler (app.rs)

```rust
fn restore_recovery(&mut self, path: PathBuf) {
    self.action_open_path(path.clone());                    // 既存パスで読み込み
    self.recovery_candidates.retain(|p| p != &path);
    if self.recovery_candidates.is_empty() { self.show_recovery_modal = false; }
    let _ = std::fs::remove_file(&path);                    // 復元成功で source は削除
}

fn discard_recovery(&mut self, path: PathBuf) {
    let _ = std::fs::remove_file(&path);
    self.recovery_candidates.retain(|p| p != &path);
    if self.recovery_candidates.is_empty() { self.show_recovery_modal = false; }
}
```

注意: `restore_recovery` で `action_open_path(.autosave.daw)` を呼ぶと `file_path = .autosave.daw` になってしまう。 これは UX 的に間違い (autosave からの復元は元ファイル名で開きたい)。 → 復元時は file_path を:
- sidecar 復元: 元の `.daw` パス (sidecar from `<x>.daw.autosave.daw` → `<x>.daw`)
- recovery_dir 復元: None (新規プロジェクト扱い、 ユーザーが Save As で名前を付ける)

実装方針: `restore_recovery` 内で path を復元用に判定 (recovery_dir 内なら新規扱い、 sidecar なら元 .daw で開く)。

## 主な変更 / 新規ファイル

### 新規
- `common/src/recovery.rs`
- `daw_gui/src/view/recovery_modal.rs`

### 編集
- `common/Cargo.toml` (uuid crate 追加)
- `common/src/lib.rs` (recovery module export)
- `daw_gui/src/app.rs` (フィールド + AppEvent + handler + maybe_autosave 改修)
- `daw_gui/src/main.rs` (起動時 scan、 AppData::new に candidates 渡す)
- `daw_gui/src/view/runner.rs` (CloseRequested で on_shutdown)
- `daw_gui/src/view/root.rs` (recovery_modal の呼び出し)

## 検証

```bash
cargo check --workspace
cargo clippy --workspace -- -D warnings
cargo build -p daw_gui
cargo run -p daw_gui
```

smoke test シナリオ:
1. **未保存プロジェクトの autosave**: 起動 → トラック追加 → 60s 待機 → `%LOCALAPPDATA%\daw_01\recovery\<uuid>.autosave.daw` が作成される
2. **kill 後の起動時復元**: 上の状態で taskkill → 再起動 → 復元 modal 表示 → 「復元」 で内容戻る
3. **「破棄」 で削除**: ファイルが消えて modal 閉じる
4. **正常終了の cleanup**: 1 と同じ状態で window 閉じる → recovery file が削除される (modal は出ない)
5. **sidecar 検出**: 保存済み .daw を Open → 編集 → 60s 待機 → kill → 再起動 → 同ファイル Open → sidecar modal 表示

## スコープ外

- 自動 conflict 解決 (sidecar と元 file の merge)
- recovery 候補の preview / metadata 表示
- 古い recovery file の自動 GC (1 週間以上前等)
- multi-instance での recovery_id 衝突対策 (uuid v4 で実用上問題なし)
