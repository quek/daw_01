# r.md #48 — テーマ (ダーク / ライト / ユーザー追加)

## ゴール

- ダークテーマとライトテーマを持つ。
- **設定画面**でどちらか選択できる。
- **テーマは後から追加できる**: ユーザーが `%LOCALAPPDATA%\daw_01\themes\*.json` に
  テーマファイルを置くと、設定画面の一覧に出る。

## grill-me で確定した仕様 (2026-08-15)

| 論点 | 決定 |
|---|---|
| テーマの追加方法 | **ファイルを置くと増える** (JSON)。「ベース + 差分」形式で、書きたい色だけ書けばよい |
| 設定画面の形 | **移動・リサイズできる floating window** (`undo_history` と同型)。位置/サイズ/開閉を `app_config.json` に永続 |
| 設定画面の中身 | **テーマだけ**。View メニューの既存項目 (リソースモニター等) はそのまま残す |
| 入口 | **Edit メニュー最下部の「設定...」** (Ardour / Cubase の Edit > Preferences に倣う) |
| OS のライト/ダーク追従 | **なし** (自分で選ぶだけ) |
| ライトの明度基準 | **薄いグレー基調** (床が薄グレー、パネル/ボタンが白に近づいて浮く。Cubase Silver 系) |
| トラック/クリップ 16 色 | **テーマ非従属** (プロジェクトの中身なのでテーマで変えない) |

原則から確定した点 (質問していない):

- 切替は**即時**、全画面 (F1 ヘルプ窓・映像プレビュー窓を含む) に反映する。
- 映像プレビューの**映像の外側 (レターボックス) は両テーマとも暗いまま**。書き出し動画の黒背景と
  対であり、`--smoke-test` の判定 (backdrop の RGB sum ≈ 44 / near-black 閾値) の前提でもある。
- 設定画面は「選択」のみ。色を編集するテーマエディタは作らない (r.md は「どちらか選択できるように」)。

## 設計

### 1. 色の所有者 — flat `pub const` を捨てて runtime パレットへ

現状は `daw_ui_core::theme` の flat `pub const` を `daw_gui::theme` が glob 再輸出し、call site が
`theme::TOKEN` で読む。これを次に置き換える。

```
daw_ui_core::theme::Palette        汎用 UI トークン (面/枠/テキスト/アクセント/グリッド/メーター/波形/カーブ)
  └ UiHost が Arc<Palette> を所有 / Ui::palette() -> &'a Palette で widget に供給

daw_gui::theme::DawColors          daw_gui 固有トークン (playhead / record / solo / clip / タグ / 鍵盤 ...)
daw_gui::theme::Theme { id, name, core: Arc<Palette>, daw: DawColors }
  └ AppData.theme が SSoT。runner が毎フレーム UiHost へ core を push する
```

- **なぜグローバル (`static RwLock<Palette>`) にしないか**: `make test` はテストを並列スレッドで
  回す。プロセス共有の可変パレットにすると、テーマを切り替えるテストが他テストの色アサーションを
  壊す (arrangement 23 件 / piano_roll 2 件が色を検証している)。context 保持なら各テストが
  `Palette::dark()` を明示的に渡せて決定論的。
- **`Ui::palette()` は `&'a Palette` を返す** (`&self` ではなく host の寿命に紐づける)。
  これで `let p = ui.palette();` した後も `ui.push_rect(...)` が呼べる (借用衝突しない)。
- **`impl Default for *Style` (14 個) は廃止**し `from_palette(&Palette)` / `from_theme(&Theme)` に
  する。テーマ色を読む `Default::default()` は「隠れたグローバル依存」で `Default` の契約
  (文脈非依存の既定値) に反する。style は**毎フレーム組む**ことを doc で契約化する。

### 2. トークンは `palette!` マクロで 1 回だけ宣言する

トークンを 1 箇所で宣言し、そこから

- struct 定義
- `dark()` / `light()` の 2 コンストラクタ
- JSON ローダ用の `set_by_name(&mut self, key, color) -> bool`
- `token_names()`

を生成する。**新しいトークンを足すのはマクロ本体に 1 行**で、JSON ローダにも自動で通る
(= 「テーマは後から追加」に加えて「トークンも後から追加」が成立し、既存のテーマファイルは壊れない)。

### 3. 極性 (polarity) の分離 — ライトテーマを壊す本体はここ

現状 `pick_contrast(bg, theme::TEXT, theme::TEXT_ON_BRIGHT)` のように、**トークンを「明インク /
暗インク」として借用**している箇所が 10 箇所ある。ライトで `TEXT` を暗色にすると両引数が暗色になり、
クリップ名・波形・muted ハッチ・黒鍵行・選択リングが一斉に消える。

対処:

1. **極性固定インク軸を新設**する。`ink_on_dark` (常に明るい) / `ink_on_bright` (常に暗い)、
   波形 3 対、`scrim` / `hatch_ink` / `row_dim_ink` / `selection_ring_outer|inner`。
   これらは「その背景の上で読める」ことが意味なので、**両テーマで同値**を既定とする
   (テーマ作者は上書きできる)。
2. **呼び出し側に極性ペアを渡させる API をやめる**。`palette.ink_for(bg)` /
   `palette.waveform_for(bg, kind)` に畳む。2 引数を間違えようがなくなり、
   `audio_editor.rs` のような「auto-contrast を迂回して暗背景版を無条件選択」も起きにくくなる。
3. **単層で明るさに依存している箇所**も同時に直す (automation 選択点の白、共有グループ強調 wash、
   dB ハンドル、piano_roll の歌詞色)。

### 4. 派生 (hover / pressed) の方向をパレットが決める

`Color::lighten` は白方向固定。ライトでは hover が背景に溶ける。
`Palette::hover(base)` / `Palette::pressed(base)` を持たせ、**「面から離れる方向」** に寄せる
(ダーク = 明るく / ライト = 暗く)。任意 base 色 (toggle_button の M/S/R) にも効く。

### 5. アイデンティティ色は「変換」で読ませる

トラック 16 色・automation lane の 44 カテゴリ色・ユーザーが color_picker で選んだ任意色は
**テーマ非従属**。ただし薄い背景の上に細線/文字として描くと沈む。

トークンを増やして解決すると**ユーザーが選んだ任意色には効かない**ので、
`Palette::adapt_on(bg, identity)` (色相・彩度を保ったまま、コントラストが足りるまで明度を寄せる。
足りていれば恒等) を用意し、identity 色を chrome 面に細線/文字で描く箇所で通す。

### 6. キャッシュ無効化 / clear color

- `with_widget_node` の `input_hash` にも `HeavyCtx::cached` の `viewport_key` にも色は入っていない
  (色は描画コマンドに焼き込まれて Scenegraph に残る)。テーマ切替時に
  `UiHost::invalidate_scene_cache()` を呼ぶ。`UiHost::set_palette()` が「変わったか」を返し、
  runner が変化時だけ invalidate する (呼び忘れが起きない形)。
- ウィンドウの clear 色は `Scene::DEFAULT_CLEAR` (= `WINDOW_BG` の手書き複製) のまま daw_gui が
  一度も上書きしていない。`UiHost::frame_to_edits_with_fonts` 冒頭で
  `scene.clear_color = palette.window_bg` を設定する (renderer は色の意味を持たない原則を維持)。

### 7. テーマファイル

`%LOCALAPPDATA%\daw_01\themes\<なんでも>.json`:

```json
{
  "name": "Solarized Dark",
  "base": "dark",
  "colors": {
    "accent": "#268bd2",
    "window_bg": "#002b36",
    "playhead": "#cb4b16"
  }
}
```

- `base` は組込みテーマ id (`dark` / `light`)。省略時は `dark`。
- `colors` はフラットな名前空間 (core と daw を混ぜて書ける)。**テーマ作者にとっての SSoT**。
- 未知のキーは警告して無視 (前方互換)。書かなかったキーは `base` から継承。
- id はファイル名 (拡張子なし)。組込み id と衝突したらファイル側を `file:<name>` にして両立。

### 8. 永続化

`app_config.json` に `theme: String` (テーマ id) を足す。`ui_prefs.theme_id` が live SSoT、
書き出し口は既存の `persist_app_config()` 1 箇所。Song は dirty にしない
(memory「dirty フラグの基準」= 見方の都合はプロジェクトに書かない)。
`ViewState` (プロジェクト単位) には置かない — プロジェクトを開くたびにテーマが変わってしまう。

### 9. 設定画面

`daw_gui/src/view/settings.rs`。`undo_history.rs` と同型の floating window:

- `reserve()` を背景描画前、`draw()` を背景描画後に呼ぶ (真の非ブロッキング)。
- タイトルバー drag で移動、右端/下端/右下隅で resize。位置/サイズ/開閉を `app_config` に永続。
- 中身: 「テーマ」セクション + テーマ一覧 (組込み → ユーザー、名前順)。行 click で即時適用。
  選択中の行を accent で示す。ユーザーテーマは出所 (ファイル名) を dim で添える。
- Esc の優先順位チェーンに `settings_open` を追加 (選択解除より先に閉じる)。
- 入口: Edit メニュー最下部「設定...」。

## パレットの値は linear (実装中に実測で判明、重要)

render target は `Rgba8UnormSrgb` なので、**`Color` に入れた値は linear として扱われ、GPU が
表示時に sRGB へエンコードする**。実測: linear `(0.055, 0.365, 0.780)` → 画面上 `(66, 163, 229)`
(= sRGB エンコード後の値と完全一致)。

これを知らずに「画面でこう見えてほしい色」をそのまま入れると、**意図よりずっと明るい色**になる。
実際、ライトの本文色を黒のつもりで `0.098` と書いたら画面では中間グレー (#585858) だった。

対処:

- `daw_ui_core::theme::srgb(r, g, b)` / `srgba(..)` を追加し、**ライトの値は「画面上の見た目」から
  書く**。ダークの値は歴史的に linear 直値で目視調整済みなので、そのまま残す (既存ピクセル不変)。
- **テーマ JSON の hex も同じ変換を通す**。テーマ作者はカラーピッカーで拾った `#268bd2` を
  書くのであって、linear 値を書くのではない。変換しないと水色になる。
- ~~`color.rs` の `relative_luminance` は値を sRGB とみなしてガンマ復号する (= linear 値には
  二重復号)。ここを直すとダークのクリップ名の色が一斉に変わるので**触らない**。~~
  **2026-08-15 に修正 (r.md #50)**。「触らない」の理由が「見た目が変わるから」という
  美観上のものだけで、正しさの根拠が無かった。実際には二重復号が輝度を最大 13 倍
  過小評価しており、**中間調の面が一律「暗い」と誤判定されて明インクが載っていた**:
  - 既定クリップ色 `clip_default` (画面上 sRGB 127,157,190 の明るいスチールブルー) の
    上のクリップ名が実効 **2.80:1** (AA 4.5 未達)。修正後は暗インクで 5.90:1。
  - ライトテーマのメーター peak readout が実効 **2.50:1** で読めない (`adapt_on` が
    3:1 を満たしたと誤認して停止していた)。これが修正の発端。
  - ピアノロールの黒鍵 root 行 (warm overlay 重畳で画面上 sRGB 166,149,110) も同様。

  `relative_luminance` は **linear 入力前提**になった (WCAG の relative luminance は
  定義上 linear-light の加重和で、ガンマ復号は sRGB 値を linear に戻す前処理でしかない)。
  閾値 `CONTRAST_LUMINANCE_THRESHOLD = 0.179` は linear 輝度上の定数なのでそのまま有効。
  唯一「入力が本当に sRGB」なのは GPU readback を渡す視覚回帰テスト 2 本
  (`theme_visual.rs` / `master_panel_visual.rs`) で、こちらは呼び出し側で
  `srgb_to_linear` を通すよう修正した。

  **未解決 (別件)**: 閾値 0.179 は「純黒と純白のコントラストが等しくなる点」なので、
  `ink_for` が返す `ink_on_bright` (画面 77,82,95) / `ink_on_dark` (画面 242,245,251) の
  ような**軟らかいインク対**では正しい分岐点ではない。この対の実際の交点は輝度 0.308。
  0.179〜0.308 の面では暗インクが選ばれるが明インクの方が読める。恒久対処は
  `ink_for` を「2 つのインクの実測コントラストが高い方を返す」に変えること (閾値定数を
  インク対の仮定ごと消せる)。クリップ / 鍵盤 / 波形の色が広範に動くので未着手。

## 実装中に確定した細部

- **ライトの `accent` は「白文字が AA (4.5:1) で載る」上限まで暗くした** (相対輝度 0.121)。
  ここを明るくすると選択行 / menu hover の文字が読めなくなるので、テーマ作者向けの
  制約として doc に書いてある。
- **ダークの `text_on_accent` は既存のまま** (azure accent の上で 2.7:1)。r.md #48 以前からの
  意図的なデザインで、ここを直すとダークの選択行・menu hover が一斉に変わるため触らない。
  代わりに、コードが実際に依存する `ink_for(accent) >= 4.5` を **両テーマで**テストに固定した。
- **`ToggleButtonStyle::from_palette` の `on_text_color` を `Some(p.text_on_accent)` にした**。
  旧 `None` (= off 用 `text_color` に fallback) はライトで「暗い accent 塗りに暗い本文色」に
  なる。ダークでは 0.880 → 0.97 の near-white 同士なので見た目は変わらない。
- **`scrubable_number` の数値色は「実際に塗った背景」から auto-contrast** で決める。
  ドラッグ中の背景は caller 任意色 (daw_gui は `scrub_drag_bg` / `_warm`) なので固定色では
  破綻する。通常時 (= `bg_color` 塗り) はテーマ本文色のままでダークの見た目は不変。
- **modulation の live マーカーを amber に統一**した。旧実装は fader / knob が amber、
  scrubable_number だけ near-white で語彙が割れていた (`modulation_live` トークンに集約)。
- `inset_bg_hover` / `border_hover` のダーク値は旧 `.lighten()` の**実計算値**に合わせてある
  (ダークのピクセルを変えないため)。

## 実装順序

1. ui-core: `palette!` マクロ + `Palette` + `dark()` / `light()` + `ink_for` / `waveform_for` /
   `hover` / `pressed` / `adapt_on`。
2. ui-core: `UiHost` がパレットを所有、`Ui::palette()`、clear color、`set_palette` の変化検出。
3. ui-core widgets: `Default` 廃止 → `from_palette`。
4. daw_gui: `DawColors` / `Theme` / JSON ローダ / テーマ一覧の探索。
5. daw_gui: `AppData.theme` / `app_config` / `AppEvent::SetTheme` / runner の push。
6. daw_gui: 設定画面 + Edit メニュー + Esc チェーン。
7. 機械的スイープ: `const` 74 件 → `theme::` 参照 → 生リテラル (chrome / semantic 分)。
8. F1 ヘルプ・映像プレビューをテーマ機構に取り込む。
9. テスト: パレット明示化、JSON 継承の往復、ライトの不変条件 (黒鍵行が白鍵行より暗い等)。
10. 実機: 両テーマで目視 + `--smoke-test`。
