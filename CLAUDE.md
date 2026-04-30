## このリポジトリ

Rust 製・モデルを Clone しない DAW 向け GUI ライブラリ。GUI のみを扱い、audio / IPC には一切関知しない。

設計の詳細は [docs/plan.md](docs/plan.md) を参照（正本）。

## クレート構成

- `crates/platform/` (`daw-ui-platform`) — winit 抽象、`WindowBackend` trait + 中立 `AppEvent`
- `crates/renderer/` (`daw-ui-renderer`) — wgpu パイプライン (rect / glyph / 今後 line)
- `crates/ui/` (`daw-ui-core`) — `Ui<'a, M>` / `Edit<M>` / widgets
- `crates/examples/*/` — 動作確認サンプル

## 設計上の不変条件 (load-bearing)

実装中は必ず守る。緩めない。

- ユーザ Model 型に `Clone` / `PartialEq` / `Hash` / `Default` を要求しない
- メッセージ型を導入しない。`Edit<M>` は `Box<dyn FnOnce(&mut M)>` で、`Application::Message: Clone` 伝染を構造的に防ぐ
- `derive` マクロ (Lens 等) 禁止
- 差分検出は widget ID + プリミティブ末端値の hash のみ。ユーザ Model の構造体全体は触らない
- `Ui<'a>` の `'a` で借用ライフタイムを統一、GAT は使わない (stable Rust)
- ライブラリは audio / IPC / プロセス間通信に一切関知しない。`Edit` を返したところで責務を切る

## Coding Principles

### 最新の安定版を使う
- Rust Edition 2024 / 各 crate は最新版
- deprecated な API を新規コードで使わない

### KISS / DRY
- 最小限の実装で目的を達成する。不要な抽象化を作らない
- 同じロジックを複数箇所に書かない

### Single Source of Truth
- 同じデータを複数箇所に複製しない
- 「この値は誰が所有し、誰が更新するか」を明確にしてから実装する

### 外部 API の挙動を先に理解する
- 推測で実装→失敗→修正のサイクルは、調査→実装より遅い
- wgpu / winit / glyphon / taffy はバージョン間で breaking change が多い。該当バージョンのドキュメント・examples を確認してから書く
