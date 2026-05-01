# レポートテンプレート

調査結果は以下の形式で日本語でまとめる。

```markdown
## 調査結果: [機能名]

### 自プロジェクト (gui_01) の既存実装
- ファイル: ...
- パターン: ...
- 流用可能な部分: ...
- 変更が必要な部分: ...

### 各プロジェクトの実装

#### egui (immediate-mode の参考、最優先)
- ファイル: ...
- パターン: widget id の生成 / Response の組み立て / 描画コマンドの積み方
- 流用可能なアイデア: ...

#### iced / druid / xilem / floem (対比 / 反例)
- パターン: ...
- gui_01 の no-Clone / no-msg 設計と何が違うか: ...
- gui_01 が **採用しない** 部分の根拠: ...

#### winit / wgpu / glyphon / cosmic-text / taffy
- 関連 API: ... (関数シグネチャを引用)
- crates.io 版での実 API: ... (`~/.cargo/registry/src/...` から)
- /tmp の main 版との差異: ...
- スレッド要件・lifecycle 制約: ...

### crates.io / Cargo.lock との整合性
- 確認したバージョン: `<crate> = "X.Y.Z"`
- 確認したパス: `~/.cargo/registry/src/index.crates.io-*/<crate>-<version>/...`
- main との API 差異 (もしあれば): ...

### 推奨アプローチ
- gui_01 での実装方針
- 採用する設計パターンとその理由
- `crates/platform/` / `crates/renderer/` / `crates/ui/` のどこに何を置くか

### 設計不変条件への影響 (gui_01 固有)
- ユーザ Model に `Clone` / `PartialEq` / `Hash` / `Default` を要求していないか
- メッセージ型を導入していないか (`Edit<M>` 経由を維持)
- `derive` マクロ (Lens 等) を新規導入していないか
- 差分検出が widget ID + プリミティブ末端値の hash で完結するか
- `Ui<'a>` の borrow lifetime を保てるか (GAT を使わずに済むか)
- audio / IPC を library に持ち込んでいないか
- 既存の trybuild (`tests/no_clone_required.rs`) で回帰しないか

### パフォーマンスとメモリ
- 描画フレームの draw call 数 / instance 数の見積もり
- LOD / scenegraph (M4) / heavy (M5) との整合
- Vec / HashMap 等の確保が毎フレーム発生していないか
- ベンチを書くべきか (criterion、`crates/ui/benches/`)

### 注意点
- エッジケース (空入力、リサイズ中、focus 喪失、IME 中断、Alt-Tab 復帰、modifier 押しっぱなし)
- スレッドセーフティ (winit / wgpu の制約)
- 後方互換 (winit / wgpu / taffy のバージョン更新時に追従できるか)

### 参考コード
- 具体的なコード例 (外部 → gui_01 への変換ポイント)
- gui_01 の既存 widget で類似実装があれば参照
```
