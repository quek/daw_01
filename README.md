# daw-ui

Rust DAW シェル UI 向けの GUI ライブラリ (実装中)。

- Rust Edition 2024
- ユーザの Model 型に `Clone`/`PartialEq`/`Hash`/`Default` を要求しない
- Hybrid アーキテクチャ: 即時モード API + 内部 scenegraph + `heavy()` 脱出口
- winit + 自前 wgpu パイプライン + glyphon + taffy

## 構成

| crate | 役割 |
|---|---|
| `daw-ui-platform` | `WindowBackend` trait と中立 `AppEvent`、winit バックエンド |
| `daw-ui-renderer` | 自前 wgpu パイプライン (instanced rect / textured quad / line strip / SDF glyph) |
| `daw-ui-core` | Hybrid 即時モード API (`Ui<'a>`, `Edit<'a>`, scenegraph cache, `heavy()`) |

## ビルド

```sh
cargo build --workspace
cargo run -p daw-ui-example-mixer
```

設計の詳細は `C:\Users\ancient\.claude\plans\rust-clone-daw-gui-zazzy-kazoo.md` を参照。
