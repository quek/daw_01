# plan: arrangement トラック名フォントを style 可変にする

## 背景 / 問題

アレンジビューの track header のトラック名は、gui_01 の汎用ボタン
`Ui::button_at_clicked`（`crates/ui/src/widgets/button.rs:114`）で描画されており、
その font_size が **`16.0` にハードコード**されている。

`arrangement.rs:8202`:
```rust
if self.button_at_clicked(id_name, &name_text, name_rect_visible) {
    clicked_track_for_select = Some(t.id);
}
```

一方 `ArrangementStyle::track_text_size`（default 12.0）は、名前からはトラック名の
サイズに見えるが、実際には **group の disclosure グリフ（▶/▼）専用**
（`arrangement.rs:8141`）でしか使われていない。

このため daw_01 側で `track_text_size` を小さくしてもトラック名は 16px のまま変わらない
（非グループ track では disclosure が無く、override は完全に不可視）。

## 理想 (最終形態)

- アレンジ track header の **トラック名フォントが `style.track_text_size` に従う**。
  `track_text_size` という名前どおりの意味になる（SSoT / 名が体を表す）。
- daw_01 は `track_text_size` を小さい値（暫定 11.0、実機で微調整）に設定するだけで
  名前が縮む。
- click→select / double-click→rename / ボタン外観（fill / border / 角丸）は **現状維持**。
  「フォントサイズだけ可変」にする（外観の作り替えはスコープ外）。
- disclosure グリフも同 `track_text_size` を共有 → 名前と disclosure が同サイズで一貫。

## 担当境界

- 汎用 `button_at_clicked` の 16px は **他の UI が依存**しているため変えない。
  arrangement のトラック名描画だけが `track_text_size` を使うようにする
  （新 sized ボタン method / arrangement 内インライン描画 等、実装は gui_01 判断）。
- daw_01 側 wire 不要。`ArrangementStyle.track_text_size` を渡すだけで反映される。

## daw_01 側の現状

`arrangement_view.rs` の `ArrangementStyle` で `track_text_size: 11.0` を設定済み
（gui_01 landing 前は disclosure glyph にのみ作用、landing 後に名前へ反映）。
最終 px は landing 後に実機で確認して微調整する。
