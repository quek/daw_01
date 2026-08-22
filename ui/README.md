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
| `daw-ui-renderer` | 自前 wgpu パイプライン (instanced rect / line strip / SDF glyph、textured quad は M5+) |
| `daw-ui-core` | Hybrid 即時モード API (`Ui<'a>`, `Edit<'a>`, widgets — M5 で `heavy()` を追加) |

## ビルド

```sh
cargo build --workspace
cargo run -p daw-ui-example-mixer                    # M1 ボタン + 1 万矩形ベンチ
cargo run --release -p daw-ui-example-waveform-validation  # M2 128 widgets 波形グリッド
cargo bench -p daw-ui-core                           # waveform LOD ベンチ
```

設計の詳細は [docs/plan.html](docs/plan.html) を参照 (正本)。
不変条件と毎セッション参照すべき制約は [CLAUDE.md](CLAUDE.md)。
