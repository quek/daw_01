<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan: アプリアイコン (#47) + DOS 窓抑制 (#48)

`docs/FIXME.md` #47 / #48 の実装計画。FIXME.md は編集禁止のため進捗はここで追う。
方針は grill (2026-06-13) で確定。

## 確定方針

### #48 — 起動時の DOS 窓を出さない (+ ログのファイル化)

- **スコープ: アプリ全体でコンソール窓ゼロ** (起動時 3 プロセス + 裏で呼ぶ probe / ffmpeg)。
- **3 プロセス (daw_gui / daw_audio / daw_plugin_host) のバイナリ先頭に**
  `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`
  - release = 窓なし / debug = 窓あり (現行の `cargo run` + ログ grep 動線を維持)。
- **`common::logging::init_tracing` を再設計**
  - `tracing_appender::non_blocking` + 日次ローテーションで
    `AppDirs::production()/logs/<process>.log` に**常時**出力 (背景スレッド書き込み =
    呼び出し側はディスクを待たない)。
  - debug ビルドはこれに加え stdout layer も付ける。
  - 返り値の `WorkerGuard` を各 `main` が保持し続ける (drop でログ欠落するため)。
  - **オーディオ RT スレッドは元々ログ皆無** (grep 確認済: daw_audio のログは panic
    handler / MMCSS 設定の 3 箇所のみ、サンプル処理ループに無し) → 品質影響ゼロ。
- **CREATE_NO_WINDOW の付与先は `daw_gui/src/subprocess.rs` の 2 spawn site のみ**:
  `spawn_sibling` (tokio Command, inherent `creation_flags`) と `probe_plugin_ports`
  (std Command, `CommandExt` trait)。release-gated (`CHILD_CREATION_FLAGS`)。
  - **訂正 (research で確証)**: runtime の外部 `ffmpeg.exe` spawn は**存在しない**。
    動画パイプは全て in-process (MF / rsmpeg)。repo 内の `Command::new(&ffmpeg)` は
    全て `#[cfg(test)]` の fixture 生成のみ。よって ffmpeg への付与は不要。
  - VOICEVOX engine は既に `CREATE_NO_WINDOW` 付与済 (`common/src/voicevox_engine.rs`)。
  - 自前子プロセスは release で windows-subsystem 化済なので CREATE_NO_WINDOW は
    belt-and-suspenders (将来の spawn 経路追加 / cfg_attr 取りこぼしへの保険)。

### #47 — アプリアイコン (f2filer 系統だが多解像度)

- **SSoT = `daw_gui/assets/icon.svg`** (ベクター原本、作図済)。
  - 絵柄: シアンの波形 5 本 + 八分音符アクセント、濃紺黒角丸プレート。歌声 DAW を表す。
- **`daw_gui/build.rs` で build 時ラスタライズ** (resvg 0.47):
  16/32/48/256 px → `ico` crate で多解像度 `.ico` をパック → `embed-resource` で exe に埋め込み
  (Explorer / タスクバー / Alt+Tab)。
  - 併せて 256px RGBA を `OUT_DIR` に出力 → window アイコン用。
- **window 左上**: `main.rs` の `window_attrs.with_window_icon(Icon::from_rgba(...))`。
  `runner.rs:611` が `window_attrs` をそのまま `create_window` に渡すので **gui_01 改修不要**。

## 依存追加 (research workflow で版を確証してから確定)

- daw_gui `[build-dependencies]`: `resvg` 0.47 系 (+ 必要なら usvg / tiny-skia)、`ico`、`embed-resource` 3。
- common `[dependencies]`: `tracing-appender` (tracing-subscriber 0.3 互換版)。

## 進捗

- [x] 方針確定 (grill 2026-06-13)
- [x] `assets/icon.svg` 作図
- [x] research workflow (外部 API 確証) — 完了 (6 観点 + 完全性クリティック)
- [x] common: logging 再設計 (`init_tracing(name, dir)` + `init_tracing_for`) + tracing-appender + `AppDirs::logs_dir()`
- [x] 3 mains: windows_subsystem cfg_attr + `init_tracing_for` で WorkerGuard 保持
- [x] subprocess.rs: spawn_sibling / probe に CREATE_NO_WINDOW (release-gated)
- [x] daw_gui/build.rs: rasterize → ico → embed-resource + window_icon.rgba (既存 ffmpeg DLL コピーは保全)
- [x] main.rs: `window_icon()` + `with_window_icon` 配線
- [x] build (workspace + release) green + PE subsystem 検証 (release=GUI(2) / debug=CONSOLE(3))
- [x] 敵対的レビュー workflow (4 次元 15 findings → 3 confirmed)、3 件ともその場修正:
  - [x] logging: `rolling::daily` (panic する `.expect()`) → `builder().build()` で graceful degrade、返り値 `Option<WorkerGuard>` 化 (release=コンソール無しでファイル open 失敗時に無言死を回避)
  - [x] window icon 寸法二重化 → main.rs はバッファ長から edge 導出、`ICON_SIZE` const 撤去 (SSoT 化)
  - [ ] `git add daw_gui/assets/icon.svg` (untracked / include_bytes! の hard dep) — commit 時に必須
- [x] 修正後 re-build (workspace + clippy -D warnings + release) すべて green
- [x] commit (1de8027、icon.svg 含む / FIXME.md 除外) + post-commit release build hook OK
- [x] 実機 (release): 3 プロセスがファイルログ出力 + handshake 成功 + window icon 構築成功 (warning 0)
- [ ] 実機 視覚 (user): タスクバー/ウィンドウ/Explorer のアイコン表示 + コンソール窓が出ないこと
      (no-console は PE subsystem=GUI(2) で確証済、視覚は最終確認のみ)
