<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# レポートテンプレート

調査結果は以下の形式で日本語でまとめる。

```markdown
## 調査結果: [機能名]

### 自プロジェクト前作（sing_like_coding）の実装
- ファイル: ...
- パターン: ...
- 流用可能な部分: ...
- 変更が必要な部分: ...

### 各プロジェクトの実装

#### clap / clap-host
- ファイル: ...
- パターン: ...
- CLAP インターフェース呼び出し順序: ...
- スレッド要件: ...

#### clack / nih-plug / clap-validator
- Rust 側の安全ラッパの作り方: ...
- 型変換（`*const` → `&`）の境界処理: ...

#### Meadowlark / gui_01 (daw-ui) / その他
- UI 層とオーディオ層の分離: ...
- ロックフリーキュー設計: ...
- gui_01 でのカスタム描画パターン (heavy + cached + push_*): ...

### API リファレンスからの知見
- CLAP 公式仕様で確認したセマンティクス・スレッド制約
- cpal / winit / wgpu / gui_01 の該当 API のベストプラクティス
- VOICEVOX API の該当エンドポイントの挙動
- Windows API の使用上の注意点

### clap-sys / windows crate / gui_01 の API シグネチャ
- 確認した関数とそのシグネチャ（C との差異、Rust 側 trait bound 等）

### 推奨アプローチ
- daw_01 での実装方針
- 採用する設計パターンとその理由
- `common/` / `daw_gui/` / `daw_audio/` / `daw_plugin_host/` のどこに何を置くか

### リアルタイム安全性
- ホットパスでのヒープ確保・ロック・I/O を回避する方法
- UI ↔ オーディオスレッド間のデータ受け渡し

### 注意点
- エッジケース
- スレッドセーフティ
- FFI 境界のバリデーション

### 参考コード
- 具体的なコード例（前作 / 外部 → daw_01 への変換ポイント）
```
