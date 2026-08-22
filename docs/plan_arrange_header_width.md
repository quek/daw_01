<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan: アレンジメント track header 幅の drag リサイズ (FIXME #16)

## 目的 / 最終形態

アレンジビューの **track header 列 (トラック名 + M/S/R ボタン領域) と lanes 領域の境界**
をユーザーがドラッグして header 幅を変えられるようにする。REAPER / Bitwig / Cubase の
track panel 幅リサイズに相当する。

- header と lanes の境界 (= `header_pane.x + header_w` の縦線) 付近にカーソルを置くと
  カーソルが横リサイズ (`EResize`, ↔) に変わる。
- press → 左右 drag で header 幅が **ライブで** 追従し (lanes 領域が連動して伸縮)、
  track 名がより長く読めるようになる / 逆に詰めて lanes を広げられる。
- release で確定。幅は全 track 共通の単一値 (per-track ではない。要望は「幅」単数)。
- header 幅の SSoT は **daw_01 の `AppData.arrange_header_w`** (default 160.0、session-only)。
  widget は毎フレーム `ArrangementView.header_w` としてこの値を読むだけ (既に実装済)。

## gui_01 への依頼 (widget 側)

header/lanes 境界の splitter は **arrangement widget 内** で扱うのが理想:
hit-test の優先順 (M/S/R ボタン・track 名 click・lane drag との競合回避) と cursor feedback
を widget が一元管理しているため。daw_01 が overlay で splitter を被せると widget の
pointer 消費と二重反応する懸念がある。

依頼内容:

1. `ArrangementEditRequest` に variant 追加 (非破壊・additive):
   ```rust
   /// track header 右端 splitter の drag による header 幅編集。
   /// drag 中は per-frame、 release でも emit してよい。 値は raw（clamp は caller）。
   SetHeaderW { prev: f32, next: f32 },
   ```
2. widget 内で splitter を実装:
   - hit zone = `header_pane.x + header_w` を中心とした幅 ~8px の縦帯 (header 行高さ全域)。
   - M/S/R ボタン・track 名 click zone と重ならない右端列に置く (hit-test 順は splitter を
     ボタン群の後 = 低優先にし、ボタンを潰さない)。
   - press → drag セッション開始 + cursor `EResize`。move で `SetHeaderW { prev: 元幅,
     next: ドラッグ後幅 }` を per-frame emit。release で確定。
   - widget 内で min/max clamp は不要 (daw_01 handler が 80..480px で clamp、 widget は
     `header_w` を毎フレーム読み直すので clamp 後値が即反映される)。

## daw_01 側 (consumer、 実装済 / parked)

landing 待ちで以下は既に配線済:
- `AppData.arrange_header_w: f32` (default 160.0、session-only、save/Undo 対象外)。
- `AppEvent::SetArrangeHeaderW(f32)` + handler (`clamp(80.0, 480.0)`)。
- `arrangement_view.rs` の旧 `const TRACK_HEADER_W` 撤去 → 全箇所 `app.arrange_header_w` 化。

landing 後に `arrangement_view.rs::make_edit` へ以下の 1 arm を足すだけで完了:
```rust
ArrangementEditRequest::SetHeaderW { next, .. } => Edit::mutate(move |app: &mut AppData| {
    app.handle_event(AppEvent::SetArrangeHeaderW(next));
}),
```

## 検証 (landing 後 daw_01 側)

- 境界にカーソル → ↔ 表示。drag で header 幅が live 変化、lanes 連動。release で session 維持。
- 80px / 480px で clamp。M/S/R ボタン・track 名 click・double-click rename・file drop が
  幅変更後も正しく当たる。
- #17 (group indent 8px) と併用で深いネストでも track 名が読める。
