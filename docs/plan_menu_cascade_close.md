<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan: menu_bar の cascade (sub_menu) item click 後に cascade popup が閉じない (重大)

## 症状 (実機確認済 `20260512.daw`)

File メニューの **「Open Recent」/「Recently Saved」(sub_menu cascade)** から項目を選んで
プロジェクトを開くと、その後 **アレンジ上部 ~1/3 のトラックを double-click で rename できなく
なる**。最上段トラックに限らず、 スクロールして上部に来たトラックすべてで再現。トラック
ヘッダを広げて名前の右側 (cascade anchor の外) を double-click すれば rename できる。

## 真因 (一次情報、 daw_01 側トレースで確定)

cascade item を click すると、 **top-level menu popup は閉じるが cascade sub-popup が閉じず、
`open_popups` に modal popup として孤立 (orphaned) する**。見えないが modal なので、 その
anchor `(0,72,360,192)` (= 画面左上、 inspector + アレンジ上部に重なる) 内の **全入力を遮断**
する。`take_double_click_in_rect` が `pointer_blocked_by_modal_popup()` で早期 return → top 1/3
のトラック double-click rename が一切発火しない。

`gui_01/crates/ui/src/widgets/menu.rs` の item action 発火箇所:

```rust
// menu_bar() の top-level menu 処理 (≒ menu.rs:576-581)
if let Some(action) = clicked_action {
    action(self);
    self.close_popup(id);   // ← top-level menu popup `id` のみ close。
                            //    cascade sub-popup (`{id_path}/{i}`、 例 "menu_bar/File/2"
                            //    = Open Recent) は閉じられず orphaned に。
}
```

`clicked_action` は cascade item でも `draw_menu_entries` 内で `return_action = sub_action`
(menu.rs:405-407) として伝播してくるが、 close されるのは `id` (top menu) だけ。cascade
sub-popup は hover で `open_popup(&sub_id, ...)` (menu.rs:386) されたまま、 親 menu が閉じると
`draw_menu_entries` が呼ばれなくなり **dismiss 経路 (sibling 排他 close / outside-click) も
走らない** → 永久に open。Esc / 外 click でも消えない (実機確認済)。

## gui_01 への依頼

**cascade (sub_menu) item の action 発火時、 top-level menu popup だけでなく開いている cascade
sub-popup もすべて閉じる**。`clicked_action` を処理する箇所で `id` に加えて、 その menu 配下の
全 cascade popup (`{id_path}/{i}`、 ネスト時は再帰的に `{id_path}/{i}/{j}...`) を close する。
あるいは「action 発火時は menu_bar の全 popup (top + 全 cascade) を close」 でもよい。

最終形態: File > Open Recent / Recently Saved から項目を選んだ後、 menu 全体 (cascade 含む) が
完全に閉じ、 画面左上に孤立 modal popup が残らない (= top 1/3 のトラック double-click rename が
正常)。通常の top-level item (New / Save 等) の close 挙動は不変。

## 検証 (landing 後)

- File > Open Recent > [project] で開く → 直後に最上段トラック名を double-click → rename 起動。
- ネストした sub-sub-menu があれば、 その item click でも全 cascade が閉じる。
- New / Open... / Save 等の通常 item は従来どおり (top menu close、 cascade 無し)。

## daw_01 側

回避策なし (cascade popup は menu_bar 内部 id で管理され daw_01 から close 不可)。gui_01 fix 待ち。
当面の手動回避: トラックヘッダを広げ、 名前の右側 (cascade anchor `x>360` / `y>264` の外) を
double-click すれば rename できる。
