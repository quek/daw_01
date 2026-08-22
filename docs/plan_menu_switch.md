<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan: menu bar の top-level menu 切り替え（open 中に別 menu へ hover / click で切り替わる）

## 主訴（ユーザー報告 2026-06-03）

File メニューがドロップダウン表示されている状態で View メニューを click しても View が開かない。
一度 File を click して閉じてからでないと View を開けない。

期待挙動: 開いている menu があるとき、別の top-level メニューのラベルにマウスを移動 / click
したら、その menu に切り替わる（旧 menu を閉じ、新 menu を開く）。これは Win32 メニュー・
macOS メニューバー・GTK/Qt・各 DAW（Ardour / REAPER）共通の標準挙動。

## 根本原因（調査済み）

対象は gui_01 の menu_bar widget（`crates/ui/src/widgets/menu.rs` + `crates/ui/src/ui.rs::popup_layer`）。
daw_01 は標準的に `ui.menu_bar(rect, |mb| { mb.menu("File", ..); mb.menu("Edit", ..); mb.menu("View", ..); })`
と使うだけ（`daw_gui/src/view/root.rs:131-257`）。

各 top-level menu の popup は固定幅 `MENU_W_DEFAULT = 180px`（`menu.rs:76`）。popup_layer に渡す
anchor は `union_rect(label_rect, popup_rect)`（`menu.rs:245`）なので、**File の anchor は
x = [0, 180)** に広がる（popup_rect.x は `clamp_x(label_rect.x=0, 180, screen_w) = 0`、
`menu.rs:239-244` / `popup.rs:54-76`）。

一方 top-level ラベル幅は `chars*8 + MENU_PAD_X*2` = `chars*8 + 24`（`menu.rs:219, 74`）。
"File" / "Edit" / "View" は各 4 文字 = 56px。`next_x` 累積で配置は:

- File : x = [0, 56)
- Edit : x = [56, 112)
- View : x = [112, 168)

→ **Edit ラベルも View ラベルも、File popup の anchor [0, 180) の内側**に完全に入る。

popup_layer は body 描画後、anchor 内の click を「popup item として処理済 → 下層に流さない」
として消費する（`ui.rs:1105-1107`）:

```rust
if state.modal && pp.pos.is_some_and(|(px, py)| state.anchor.contains(px, py)) {
    self.consume_pointer_click();
}
```

menu_bar builder は File → Edit → View の順に処理する。File が open のとき View ラベルを
click すると:

1. `menu("File")`: toggle 判定（`inside && primary_just_released`、`menu.rs:248`）は File ラベル
   上の click のみ見る → click は View 上なので skip。続く `popup_layer(File)` で、View click 位置は
   File anchor [0,180) 内 → 上記 consume が走り、`primary_just_pressed` / `primary_just_released`
   が両方 false にされる（`ui.rs:1248-1256`）。
2. `menu("Edit")`: 何もしない。
3. `menu("View")`: toggle 判定の `primary_just_released` は既に consume 済 = false →
   **open_popup が呼ばれない** → View が開かない。

File を一度 click して閉じる → File anchor が消える → 次の View click は誰にも食われず通る。
これが「2 ステップ必要」の正体。

（同様に Edit を開いた状態では Edit anchor [56, 236) が View を覆うので View click も奪われる。
一般に「開いた menu の右隣に来る、popup 幅 180px 以内のラベル」がすべて入力を奪われる。）

## 望む挙動（最終形態）

標準 menu bar の挙動:

1. **全 menu が閉じている**: top-level ラベルの click で開く（hover では開かない）。← 現状維持
2. **いずれかの top-level menu が open**:
   - 別の top-level ラベルに **hover** しただけで、open 中を閉じて hover 先を開く（切り替え）。
   - 別の top-level ラベルを **click** しても同様に切り替わる（主訴の修正）。
   - open 中のラベルを再度 click すると閉じる（toggle）。← 現状維持
3. menu / popup の外を click したら全部閉じる。← 現状維持（outside_click）

sub_menu（cascade）は既に hover で開く（`menu.rs:443`）。top-level も同じ「open 中は hover 追従」
に揃えるのが標準。

## gui_01 側 source の当たり / 機構案（最終形は gui_01 にお任せ）

核心は「top-level ラベルの帯が、開いている menu の popup anchor に覆われて入力を奪われる」点。

- `menu.rs:245` の anchor から **top-level ラベル帯（bar_rect の行）を除外**し、anchor を
  popup_rect（bar の下）だけにする。これだけで、隣ラベル click の anchor-consume（`ui.rs:1105`）と
  outside_click 誤判定（`ui.rs:1038`）が止まり、隣 menu の toggle release が生きる。
  toggle は open と同じ click の release で `consume_pointer_click` 済（`menu.rs:254`）なので、
  「開いた直後の release を outside と誤判定して即閉じる」回帰は起きない見込み（要確認）。
- hover 切り替え: menu_bar が「現在 open な top-level menu_id」を 1 つ把握し
  （open_popups の "menu_bar_top" prefix、または builder 内 state）、pointer が別の top-level
  ラベル上にあれば close old + open new。これを各 popup_layer より前に行う。
- 「閉じている状態では hover で開かない」を保つため、hover 切り替えは「既にいずれか open」の
  ときだけ作動させる。

ソース:

- `crates/ui/src/widgets/menu.rs` — `MenuBarBuilder::menu`（`:213-293`）、anchor 計算
  （`:237-245`）、toggle（`:248-255`）
- `crates/ui/src/ui.rs` — `popup_layer` の anchor 内 consume（`:1105-1107`）/
  outside_click（`:1038-1044`）/ `consume_pointer_click`（`:1248-1256`）

## 影響 / SSoT

- 修正は menu_bar widget 内で完結（daw_01 側は無修正で恩恵を受ける、使い方は標準のまま）。
- daw_01 の File/Edit/View だけでなく、menu bar を使う全 caller が標準挙動になる。
