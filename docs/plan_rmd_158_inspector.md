# r.md #158 トラックインスペクタの作り直し — 実装計画

- 日付: 2026-11-23
- 対象: 画面左のトラックインスペクタ (`daw_gui/src/view/track_inspector/`、呼び出しは `daw_gui/src/view/root.rs:141`)
- 材料: 今の構造の整理と問題点 (`wf158.json` の map / synth / refs)、今の見た目 (`target/inspector_shots/*.png`、撮影テストは `daw_gui/tests/inspector_visual.rs`)
- 行番号はすべて 2026-11-23 の main (`f73a1d60`) のもの

---

## 0. 確定仕様 (再掲)

「出所」の分け方: `wf158.json` の `synth.questions` でユーザーに選択肢として出した問いへの答えを**回答**とした。選択肢の文に無かった細部と、問いに出さなかった項目は**委任**とした。

| # | 仕様 (要約) | 出所 |
|---|---|---|
| 1 | 1 列の縦スクロールのまま、最上位のまとまりごとに見出しの帯を置き、帯のクリックで開閉する。帯には背景色・開閉マーク・名前・対象を出す。開閉で動くのは帯より下だけ | 回答 (問 1 の選択肢 1)。帯の塗り・中身と「帯は動かない」は委任 |
| 2 | 一番上に「選んでいるもの」(クリップ / セル / 列) の帯、その下にトラックの帯 (入力 → Rack → 口パク → モジュレーション)。トリガから離れているもの (Parallel Out) は行の直下へ移す | 回答 (問 2 の選択肢 1 = 上に出す)。帯の順と Parallel Out の移動は委任 |
| 3 | クリップの設定をクリップの帯に集める (音声 / 画像 / 歌声の声 / 読み上げ / 字幕)。Par からは消す。MIDI と映像のクリップにも共通の欄、セルのローンチ設定もこの帯 | 回答 (問 4 の選択肢 1)。MIDI / 映像の共通欄とローンチの置き場は委任 |
| 4 | 帯ごとに独立して開閉する | 回答 (問 5 の選択肢 1) |
| 5 | 開閉状態はプロジェクトの見方として保存し、`*` を付けない (`open_rack_panels` と同じ扱い) | 回答 (問 6 の選択肢 1) |
| 6 | 開閉状態は種類ごとに共通 (トラックの帯は全トラック共通) | 回答 (問 7 の選択肢 1) |
| 7 | めったに使わない欄は帯の中の「詳細」に入れ、最初は畳む。詳細の開閉も 5・6 と同じく覚える | 回答 (問 8 の選択肢 1)。詳細の開閉を覚えることは委任 |
| 8 | タイトルに選んでいるものを出す (クリップ名 + クリップ色、トラック名を小さく添える。複数なら「N クリップ」/「N トラック」)。トラック色を変える操作は残す | 回答 (問 9 の選択肢 1)。トラック色の移し先は委任 |
| 9 | 数値の欄は「ラベル左・値右」の行に揃える。ラベル列の幅は 1 つで、A 列は無い欄でも空ける。寸法は 1 組の定数にする。内蔵デバイスの Par の格子はそのまま。Par の中身は行名の左端から始め、小見出しは「ROOM ――」形に揃え、行名と同じ見出しを繰り返さない。行の背景と hover を Par にかけない | 回答 (問 10 の選択肢 1)。寸法の SSoT・格子の扱い・Par の見た目は委任 |
| 10 | 組み込みの Comp / EQ の行は今どおりいつも出す。オフのときに「無効な部品」に見えないようにする | 回答 (問 11 の選択肢 1)。見た目の作り方は委任 |
| 11 | 見出しと欄名は日本語にする。Par / SC / Mix / dB / A と「Rack」は残す | 回答 (問 12 の選択肢 1)。「Rack」を残すことは委任 |
| 12 | 右端のドラッグで幅を変えられる (下限・上限つき)。View メニューとショートカットで隠せる。幅と表示はアプリ全体の設定に覚える | 回答 (問 13 の選択肢 1)。下限・上限、View メニュー、保存先は委任 |
| 13 | 複数トラックのとき、タイトルは「N トラック」。全部に効く帯 (入力) と、カーソルの 1 本だけに効く帯 (Rack / 口パク / モジュレーション) を見出しの対象表示で区別する | 回答 (問 14 の選択肢 1) |
| 14 | 入力の帯: そのトラックで意味の無い欄は出さず、使わない欄のための空白も残さない | 委任 (問いに無し) |
| 15 | Parallel の中で、chain の中の「+ Plugin」と Parallel 全体の「+ chain」の主従をインデントと形の差で見せる | 委任 (問いに無し) |

この計画の中で決めたこと (委任判断の中身) は §8 に一覧にした。

---

## 1. 完成形の姿

```
┌──────────── インスペクタ (幅 360〜640、右端をドラッグ / I で隠す) ─────────────┐│
│ ■ Drums                                         (16px, text)           ││← タイトル (スクロールしない、高さは常に 2 行)
│ 音声クリップ · トラック Audio ■                  (11px, text_dim)       ││   ■ = 色見本 (クリックで色を選ぶ)
├────────────────────────────────────────────────────────────────────────┤│
│▼ 音声クリップ                                            Drums        ││← 帯 (header 色、全体がクリックで開閉)
│  名前          [Drums                               ]                 ││
│  再生          [ミュート] [逆再生]                                     ││← ラベル列 1 本 + 値 + A 列
│  伸縮          [Repitch                       ▾]                      ││
│  ゲイン dB     [ 0.0                            ]  ·                   ││   · = A 列 (A が無い欄も幅を空ける)
│  …                                                                    ││
│  フェードイン  [ 0.000             ] [Linear ▾]                        ││
│▼ 入力                                                    Audio        ││
│  入力          [In 1+2 (ステレオ)             ▾]                      ││
│  モニター      [自動                          ▾]                      ││← 音声入力のときだけ出る行
│▼ Rack                                                    Audio        ││
│  │Comp           [ オフ ]      SC▾  Par  ◎   │                       ││← オフは「オフ」の札 (灰色の小表示を描かない)
│  │EQ             ~~~~~~~~      Par  ◎        │                       ││
│  │  ┄ ROOM ───────────────────────────────  │                       ││← Par は行名の左端 (+8) から、背景は塗らない
│  [            + プラグイン                    ]                       ││
│▶ モジュレーション                              Audio        [+ ▾]     ││← 見出しの右に操作を置ける
└────────────────────────────────────────────────────────────────────────┘▌← 右端 5px が幅のドラッグ帯
```

---

## 2. 新しく作るもの (共通の部品)

### 2.1 寸法の SSoT — `daw_gui/src/view/track_inspector/layout.rs` (新規、`pub`)

散らばっている寸法を 1 か所に集める (今の値は map §3「揃っているか」の表)。テストも同じ定数を読む。

```rust
pub const PAD: f32 = 12.0;            // 左右の余白 (今 mod.rs:213 の `pad`)
pub const TITLE_H: f32 = 52.0;        // タイトル 2 行 + 余白 (今は 40 = mod.rs:254)
pub const BAND_H: f32 = 24.0;         // 帯の見出しの高さ
pub const BAND_GAP: f32 = 6.0;        // 帯と帯の間
pub const BAND_BODY_TOP: f32 = 6.0;   // 見出しから中身まで
pub const BAND_BODY_BOTTOM: f32 = 8.0;
pub const HEAD_FONT: f32 = 12.0;      // 帯の名前
pub const TARGET_FONT: f32 = 11.0;    // 帯の対象 (text_dim)
pub const FORM_LABEL_W: f32 = 96.0;   // ラベル列 (欄の幅はこの 1 つだけ。段 2 で 88 → 96: 「フォルマント st」が 88 では切れた)
pub const FORM_GAP: f32 = 4.0;
pub const FORM_AUTO_W: f32 = 22.0;    // A 列 (A が無い欄でも空ける)
pub const FORM_ROW_H: f32 = 22.0;     // 部品の高さ (今の 18/20/22/24 を 1 つに)
pub const FORM_ROW_PITCH: f32 = 26.0; // 行の送り (今の 22〜46 の 9 通りを 1 つに)
pub const LABEL_FONT: f32 = 11.0;     // 欄のラベル (text_dim に統一)
pub const SUB_HEAD_FONT: f32 = 10.0;  // 小見出し「ROOM ――」
pub const SUB_HEAD_H: f32 = 16.0;
pub const ROW_NAME_X: f32 = 8.0;      // Rack 行の名前の左端 = Par の中身の左端 (plugin_row.rs:153 / native_row.rs:99)
pub const RACK_ROW_H: f32 = 26.0;     // chain_list.rs:49 の ROW_H をここへ移す
pub const RESIZE_HANDLE_W: f32 = 5.0;
```

- `pub fn par_rect(row_content: Rect, panel_h: f32) -> Rect` は、行の直下の Par の矩形 (`x + ROW_NAME_X`) を返す。描画と `native_rack_visual.rs:185-187` / `param_gesture_surfaces.rs:160-167` のテストが同じ関数を通す。今のテストは `PAD` と `ROW_H` を手で写している (`native_rack_visual.rs:32-37`)。
- 内蔵 device の Par の格子 (`native_panel/layout.rs`) は今のまま残し、受け取る矩形だけを `par_rect` に変える。

### 2.2 帯 — `daw_gui/src/view/track_inspector/band.rs` (新規)

既存の部品だけで作れる (daw-ui core に足すものは無い)。

- 描画: `ui.panel` (見出しの面)、`ui.label_at_clipped` (名前 / 対象)
- 開閉マーク: `view::disclosure::disclosure_glyph(collapsed, RevealAxis::Block)` (`disclosure.rs:42`)
- クリック: `ui.primary_click` (`ui/crates/ui/src/click.rs:144`)

```rust
pub(super) struct BandHead<'a> {
    pub key: InspectorBand,   // 開閉状態の鍵 (§2.5)
    pub title: &'a str,       // 「音声クリップ」「Rack」…
    pub target: &'a str,      // 「Drums」「選択中の 3 トラック」「Vocal のみ」… (空なら出さない)
    pub warn: &'a str,        // 対象の左に警告色 (Rack の「読み込み失敗 N」)
    pub actions_w: f32,       // 見出しの右端に子の操作を置く幅 (モジュレーションの「+」)
}
/// 見出しを描いて開閉を取り、`BandHeadOut { open, actions, body_y, .. }` を返す。子の操作 (actions の矩形) は
/// 見出しのクリックより後に描くので、press の所有者は後勝ちで子が取る (click.rs:159-161)。
pub(super) fn head(app, ui, area: Rect, y: f32, h: &BandHead) -> BandHeadOut;
/// 中身を描き終えた y から帯の次の y (中身が空なら見出しだけで閉じる)。
pub(super) fn end(out: &BandHeadOut, body_end: f32) -> f32;
/// 普通の帯: head → (開いていれば) body → end。Q の宛先を返す中身 (Rack) は body の外の変数へ書く。
pub(super) fn band(app, ui, area, y, h: &BandHead, body: impl FnOnce(&mut Ui, f32) -> f32) -> f32;
/// 帯の中の「詳細」行 (▶ 詳細の名前)。開いていれば `body`。置くのは小まとまりの末尾だけ。
pub(super) fn details(app, ui, x, w, y, key: InspectorDetail, label: &str, body: impl FnOnce(&mut Ui, f32) -> f32) -> f32;
```

(当初の案は `band<R>(…, actions, body) -> (f32, R)` の 1 本だったが、子の操作を見出しの上に置く帯が
モジュレーションの 1 本だけで、返り値の型引数が全部の帯に要るので、`head` / `end` に分けた。)

- 見出しの面: palette の `control` (押せる面。hover は `control_hover`)、下端に 1px の `border` (§8-1)。
  - 仕様の例は `panel_raised` だが、Rack の行が既に `panel_raised` で塗られている (`chain_list.rs:406-411`)。帯と行が同じ色になると境目が消える。
  - 段 3 までは `header` だったが、`header` は地 (`panel`) より沈む色で、子の Rack の行 (`panel_raised`) の方が親の見出しより目立った (レビューの指摘、ダーク 1.15:1)。`control` は両テーマで地から離れる向きで、行より 1 段持ち上がる。
- 見出しの文字: 名前は 12px `text`、対象は 11px `text_dim` を右寄せで、幅で切る。
- 見出しのクリック判定は `!ui.has_open_popups()` のときだけ取る (`chain_list.rs:87` と同じガード)。

### 2.3 欄 (ラベル左・値右) — `daw_gui/src/view/track_inspector/form.rs` (新規)

```rust
pub(super) struct FormCols { pub label_x: f32, pub label_w: f32, pub value_x: f32, pub value_w: f32, pub auto_x: f32 }
pub(super) fn cols(left: f32, width: f32) -> FormCols;   // 帯なら (area.x+PAD, area.w-2PAD)、Par なら par_rect の x / w
pub(super) fn label(ui, app, id, text, c: &FormCols, y);  // 11px text_dim、FORM_LABEL_W-4 で切る
pub(super) fn sub_heading(ui, app, id, x, w, y, text, right: Option<&str>) -> f32; // 「ROOM ――」形
pub(super) fn fade_row(ui, app, c, y, FadeRowSpec) -> f32;  // 長さ (scrubable) + カーブ (dropdown) を 1 行に
```

- 値の部品は既存の `scrub_field` / `ui.dropdown` / `ui.toggle_button_at` / `ui.text_input_at` をそのまま使う (bespoke な widget は作らない。daw_gui/CLAUDE.md:23-24)。
- **段 3 で決めたこと: 入れ子の欄は `cols_nested(left, width, origin_left)`**。Rack の行の直下 (Par / SC / 出力) の欄は、
  ラベルを行の名前の左端 (`left`、深さとともに右へずれる) から始め、値の欄の左端は帯の欄と同じ x
  (`origin_left + FORM_LABEL_W + FORM_GAP`) に揃える — ラベル列はそのぶん狭くなる (深さ 0 で 88px)。§5 段 3 の完了条件
  「字幕・画像・音声・Par の欄の値の左端の x が全部同じ」をそのまま満たす形。ラベル列が 48px を割る深い入れ子だけ
  値の欄を右へ逃がす。`FormCols` に `label_w` を持たせ、`label` はそれで切る。
- `sub_heading` は `native_panel/cell.rs:85-113` の `section_label` を一般化したもの。`section_label` はこれを呼ぶだけにする (同じ描き方を 2 本持たない)。
- フェードの行は今、音声 (`audio_event_section.rs:292-383`)、画像 (`image_event_section.rs` の `draw_fade_rows`)、字幕 (`text_event.rs:206-266`) が別々に書いていて、形も違う。`fade_row` 1 本にまとめ、カーブの表記も `mod.rs:191` の定数 1 本 (表記は「S-Curve」) にする。今は `text_event.rs:211` に別表記の「S-Curve」がある。

### 2.4 文言 — `daw_gui/src/view/track_inspector/vocab.rs` (新規、`pub const`)

見出しと欄名の SSoT (対応表は §3-11)。テストは glyph をこの定数で探す。文言を変えても、`native_rack_visual.rs:231` の `"+ Plugin"` や `param_gesture_surfaces.rs:422` の `"Gain dB"` のようにテストが壊れることはなくなる。

### 2.5 開閉状態の型と保存先

**型** — `daw_gui/src/state/inspector_view.rs` (新規)

```rust
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ClipBandKind { Audio, Midi, Image, Video, Text, Automation }
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum InspectorBand { Clip(ClipBandKind), Scene, Input, Rack, MouthMap, Modulation }
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum InspectorDetail { LaunchMidi, SubtitleStyle }
#[derive(Debug, Default)]
pub struct InspectorFolds {
    pub collapsed: BTreeSet<InspectorBand>,     // 既定は開。閉じたものだけ持つ
    pub open_details: BTreeSet<InspectorDetail>,// 既定は閉。開いたものだけ持つ
}
impl InspectorBand { pub fn id(self) -> &'static str; pub fn from_id(&str) -> Option<Self>; }
// id は "clip.audio" "clip.midi" "clip.image" "clip.video" "clip.text" "clip.automation"
//       "scene" "track.input" "track.rack" "track.mouth" "track.modulation" /
//       "detail.launch_midi" "detail.subtitle_style"
```

鍵にトラック id やクリップ id は入れない。これで仕様 6 の「種類ごとに共通」が型の上で決まる。

**どこに置くか**

| 置き場 | 変更 | 根拠 |
|---|---|---|
| 実行中の状態 | `ProjectView` に `pub inspector_folds: InspectorFolds` を足す (`daw_gui/src/state/project.rs:229` の struct、初期化 :955 付近) | `open_rack_panels` と同じ層 (`state/project.rs:247-250`) |
| 保存 | `common/src/model/view_state.rs` の `ViewState` に `#[serde(default)] pub inspector_collapsed: Vec<String>` と `#[serde(default)] pub inspector_open_details: Vec<String>` を足す (:136-139 の `open_rack_panels` の後) | serde 専用で IPC を渡らない。`WIRE_SOURCES` は対象外 (view_state.rs:1-3) |
| 書く / 読む | `handler/view_state.rs` の `snapshot_view_state` で id を sort して書く (:98-112 の隣)。`restore_view_state` は最初にクリアし (:182-183 の隣)、`from_id` で読んで知らない id は捨てる (:219-227 の隣) | 書く側と読む側が隣り合う規約 (view_state.rs:5-6) |
| 新規 / 旧ファイル | `handler/project.rs:134` (`open_rack_panels.clear()` の隣) と `restore_view_state(None)` の経路でクリアする | 前のプロジェクトの状態を持ち越さない |

- 保存を enum ではなく文字列 id にする理由: 新しい版が足した種類 (例: 新しいクリップの種類) を古い版が読んだとき、enum だと ViewState 全体の decode が落ちる。`#[serde(other)]` は externally tagged な enum には使えない。既存の `SettingsCategory::id` / `from_id` (`state/ui_prefs.rs:28-42`、`app_config.rs:40-44` の「知らない名前は既定へ倒す」) と同じ形にする。
- 開閉は `edit_song()` を通さず `cur.view` を直接書く。undo snapshot も dirty も epoch も動かない (不変条件 5 の口を通らない = Song 編集ではない)。`rack_view.rs:19-25` の `toggle_rack_panel` と同じ扱い。

**イベント** — `daw_gui/src/event_inspector.rs` (新規)

`InspectorEvent { ToggleBand(InspectorBand), ToggleDetail(InspectorDetail), ToggleVisible, SetWidth { w: f32, commit: bool } }` を作る。`AppEvent::Inspector(InspectorEvent)` の委譲 arm を 1 行だけ足し、`app.rs:1017` の `VirtualKeyboard(ev)` と同じ形にする。本体は `daw_gui/src/handler/inspector_view.rs` (新規) に置く。`app.rs::dispatch_app_event` は baseline の天井 1605 に対して今 1496 なので (`scripts/arch_lint_baseline.txt:150`)、1 行足しても収まる。

### 2.6 幅と表示 (アプリ全体の設定)

マスターパネルの幅と開閉と同じ作りにする (`state/ui_prefs.rs:81-85`、`app.rs:591-610`、`app_config.rs:46-51`)。

| 変更先 | 中身 |
|---|---|
| `state/ui_prefs.rs` | `pub inspector_open: bool` と `pub inspector_w: f32` を足す |
| `app_config.rs` | `#[serde(default = "default_true")] inspector_open` と `#[serde(default = "default_inspector_w")] inspector_w` を足す。`from_prefs` (:124-155)、`Default` (:182-)、`load` のクランプ、round-trip テスト (:259-295) も直す |
| `app.rs:190-205` | 起動時に prefs へ流す |
| `handler/inspector_view.rs` | `INSPECTOR_DEFAULT_W = 360.0`、`INSPECTOR_MIN_W = 360.0`、`INSPECTOR_MAX_W = 640.0`。`toggle_visible` (反転 + `persist_app_config`)、`set_width` (クランプ、ドラッグ中は値だけ、`commit` でディスクへ — `app.rs:597-610` と同じ流儀) |
| `view/track_inspector/mod.rs` | `pub fn panel_width(app) -> f32` (閉じていれば 0)。`master_panel::panel_width` (`master_panel.rs:74-82`) と同じ形 |
| `view/root.rs:30-31` | `INSPECTOR_W` を `INSPECTOR_DEFAULT_W` に改名する (テストが窓幅の計算に使うため `pub` で残す)。:119-137 のレイアウトは `track_inspector::panel_width(app)` から組む (master_w の上限式 `(center.w - inspector_w)` も同じ値を使う) |
| `view/shortcuts.rs` (:202 の隣) | `ShortcutDef { name: "daw.toggle_inspector", keys: &["I"], category: GridView, description: "インスペクタを開く / 閉じる" … }`。素の文字キーなので、入力中は `bare_char_key` で自動的に入力欄へ譲る (ui/CLAUDE.md:205-206)。I は今どこにも割り当たっていない (使っているのは `Ctrl+I` だけ) |
| `view/root.rs:746-751` の隣 | `take_shortcut("daw.toggle_inspector")`。`dispatch_shortcuts` は天井 530 に対して今 450 |
| `view/menu_bar.rs:281-282` の隣 | `event_item(m, "インスペクタ", "daw.toggle_inspector", AppEvent::Inspector(InspectorEvent::ToggleVisible))` |

- キー I は Logic Pro の既定に合わせた (View > Show Inspector、[Logic Pro inspector interface](https://support.apple.com/guide/logicpro/inspector-interface-lgcpe9cc3b1d/mac))。仮想鍵盤を開いている間は I も鍵盤の音のキーで (`virtual_keyboard.rs:47`)、B (Mixer) と同じく鍵盤が先に取る。
- 下限 360 は今の固定幅。理由は §6-10 (内蔵 device の Par の格子が 6 列 × 数値欄 52px を前提にしている)。

### 2.7 daw-ui core に足すもの (ドメイン知識なし)

**(a) 縁のリサイズ帯** — `ui/crates/ui/src/widgets/resize_handle.rs` (新規。`ui.rs` は 1559 ncloc で budget を超えているので別ファイルにする)

```rust
pub struct ResizeDrag { pub pointer: (f32, f32), pub released: bool }
/// パネルの縁のリサイズ帯。popup が開いている間は掴まない。掴んだ press は claim_press で名乗る。
/// hover 中とドラッグ中は axis に応じて EwResize / NsResize カーソルにする。値への写像は caller が持つ。
pub fn resize_handle(&mut self, id: impl Hash, rect: Rect, axis: Orientation) -> Option<ResizeDrag>;
```

- 今、同じ手順 (popup ガード、`take_drag_in_rect`、ドラッグ中と hover 中のカーソル) を `master_panel.rs:128-150` が手で書いている。インスペクタで 2 か所目になるので core に寄せ、マスターパネルも同じ変更で置き換える。根拠は ui/CLAUDE.md:112-115「使う側に boilerplate を強要しない」。
- `take_drag_in_rect` は press を `claim_press` しない (`ui.rs:2351-2389`)。新しい部品は ui/CLAUDE.md:156-168 の作法どおり名乗る。

**(b) 持ち主が描かれなかった dropdown の popup を閉じる** — `ui/crates/ui/src/popup.rs:46-49, 290-293`

- 今、自動で閉じるのは context menu だけ (`PopupOpenState::context_menu` と `close_stale_context_menus`)。dropdown (`widgets/dropdown.rs:185-192`) は持ち主の dropdown が描かれなくなっても開いたまま残る。
- その popup は見えないまま `has_open_popups()` を true にし続け、背後の入力を止める (memory の「位置依存の入力デッドゾーン」と同じ型)。
- #158 では帯を畳むこと、インスペクタを I で隠すこと、選択を変えて帯が消えることで、この状態が日常的に起きうる。
- 直し方: フラグを `owner_draws_each_frame` に改名し、dropdown も立てる。class として潰す (同件チェック: 持ち主が毎フレーム `popup_layer` を呼ぶ popup = context menu と dropdown)。core の単体テストで確かめる: 開いた dropdown の持ち主を描かないフレームを 1 つ挟むと閉じる。

### 2.8 既存の部品で足りるもの

- 帯の見出し、詳細の行、ラベル左・値右の行、小見出し、`+ プラグイン` / `+ チェーン` の形の差: core の `panel` / `label_at_clipped` / `primary_click` / `button_at_sized` / `measure_text` の組み合わせで足りる。
- `disclosure_glyph` は daw_gui に置いたまま使う。

---

## 3. 仕様ごとの変更

### 仕様 1 帯で開閉する

- `track_inspector/mod.rs:209-329` の `draw` を作り直し、上から「タイトル (§仕様 8) → scroll_area の中で帯を順に積む」にする。
  - 各帯は `band::band` で包む。中身の関数は今の `(app, ui, area, pad, y) -> f32` 契約のまま (daw_gui/CLAUDE.md:14-17)。
  - 包む対象: `input_section::draw_input_section`、`chain_list::draw_chain_list`、`mouth_map_section::draw_mouth_map_section`、`modulation_rack::draw_modulation_rack`。
- 見出しの文言を中身の関数から外す。
  - `chain_list.rs:108-112` の「Rack」、`modulation_rack/mod.rs:177` の「Modulation」、`mouth_map_section.rs:56-60` の見出し、`launch_section.rs:170` / :522-529、`audio_event_section.rs:67-75`、`image_event_section.rs:58-66`。
- モジュレーションの「+」の dropdown (`modulation_rack/mod.rs:178-200`) は `band` の `actions` に移す (見出しの右端、帯を畳んでいても見える)。
  - 追加を選んだときに帯が閉じていれば、同じ Edit の中で開く (新しいソースが見出しの直下に出る = トリガの近く)。
- 帯自身が動かないことの保証は §6-1 (scroll の床)。

### 仕様 2 並び

- `mod.rs` の積む順を次に固定する。
  1. 選択の帯 (クリップ / セル、続いて列)
  2. 入力
  3. Rack
  4. 口パク
  5. モジュレーション
- 今の `y += 10.0` (`mod.rs:317`) は `BAND_GAP` に置き換える。
- Parallel Out と読み込み失敗を行の中へ移すのは段 3 (仕様 9・15 の作業と一緒にやる。中身は §3 仕様 2 補足)。

**仕様 2 補足: トリガから離れたものを行へ戻す (段 3)**

| 今 | 移す先 | 消すもの |
|---|---|---|
| `chain_sections::draw_parallel_out_section` (`chain_sections.rs:112-205`、`mod.rs:310`) | plugin 行の `Out▾` toggle (aux 出力を持つ行だけ。SC▾ と同じ寸法で SC の左、`plugin_row.rs:125-145` と同じ idiom)。開くと行の直下に `[展開]` + port ごとの「Out N → 出力先 ▾」の行 | `chain_sections.rs` の該当関数、`mod.rs:310` の呼び出し |
| 開閉状態 | `peph.open_aux_out_panel: Option<u64>` を新設 (`state/project.rs:666` の `open_sidechain_panel` の隣、session-only) | — |
| 行の高さ | `chain_list::build_list_rows` (`chain_list.rs:266-321`) に aux 出力パネルの高さを足す。SC の `sc_panel_h` (:98-103) と同じ | — |
| データ | `ChainEntry` (`chain_rows.rs:13-41`) に `aux_output_count: u8` を足す。port の配線は `view_model.rs:291-329` の `parallel_output_entries` を `aux_out_entry(device_id)` に縮める。今の FN-NESTING baseline (`arch_lint_baseline.txt:239`) も一緒に解消する | — |
| `chain_sections::draw_failed_load_section` (`chain_sections.rs:35-105`、`mod.rs:309`) | plugin 行の中。失敗した行は Par/GUI の位置に `[再読込]` を置き、行の直下に理由 1 行 (11px text_dim、幅で切る) をいつも出す | 同じ失敗が行の「[未ロード] 名前」と別セクションの 2 か所に出る重複 (map §4-7) が消える。`chain_sections.rs` はファイルごと消える |

- マスターでは今どおり Parallel Out を出さない (`view_model.rs:293-295`)。
- **段 3 の実装**: 出力のパネルは `aux_out_panel.rs` (`chain_sections.rs` を `git mv` して書き直した)。中身は欄の形で、
  1 行目「子トラック [展開]」、展開済みなら port ごとに「出力 N [宛先 ▾]」。`Out▾` は配線済み (`ChainEntry::aux_out_wired`)
  で ON 色、幅は SC▾ + 6px (「Out▴」が切れない幅)。`[再読込]` の幅は文言に合わせて広げる (Par / GUI の 44px では
  全角 3 文字が「…」で切れた)。読み込みに失敗した行では ⌨ も出さない (host に instance が無い)。SC パネルも同じ欄の形
  (「入力 N [source ▾] [tap ▾]」)。

### 仕様 3 クリップの設定をクリップの帯へ集める (段 2)

`track_inspector/clip_band/` (新規ディレクトリ)

| ファイル | 中身 | 元 |
|---|---|---|
| `mod.rs` | 帯の振り分け。クリップの種類を `ClipContent` の variant から `ClipBandKind` に写し、見出しは種類名、対象はクリップ名。共通の欄 (名前 / 再生) | 新規 |
| `audio.rs` | 伸縮・ゲイン・パン・ピッチ・フォルマント・フェード | `audio_event_section.rs` |
| `image.rs` | X/Y/W/H/不透明度/回転 (A つき)・フェード・反転 | `image_event_section.rs` |
| `voice.rs` | 歌声の声 (キャラ / スタイル / 取り直す) | `device_panel/clip_voice.rs` |
| `talk.rs` | 読み上げ (ここから読む / +字幕デバイス / 話者 / スケール 4 つ) | `device_panel/talk.rs` |
| `subtitle.rs` | 字幕 (フォント / 揃え / 位置 / 大きさ / 不透明度 / フェード、詳細に塗り・縁・影・回転) | `device_panel/text_event.rs` |
| `launch.rs` | セルのローンチ (小見出し「ローンチ」) と列の帯の中身 | `launch_section.rs` |
| `voice_picker.rs` | キャラ → スタイルの 2 段 dropdown (下の注) | 新規 |

`voice_picker.rs`: キャラ → スタイルの 2 段 dropdown と、選んだ結果を決める処理は、今 `clip_voice.rs:70-141` と `talk.rs:151-222` に同じものが 2 本ある。1 本にする。関数 budget も効く (`draw_talk` は 264 ncloc で天井の 300 に近い)。

**共通の欄** (どの種類でも、`ClipContent::Midi` / `Video` / `Automation` を含む)

- **名前**: 単一選択のときだけ。text_input で、既存の改名の流れ (`BeginRenameClip` / `RenameClipChanged` / `CommitRenameClip`、`handler/tracks.rs:1278-1340`) を使う。§6-11 の「面の札」を付ける。
  - テキストクリップには名前の欄を出さない。テキストクリップの名前は本文そのものなので (`tracks.rs:1270-1276` の `clip_rename_current`)、同じ値の編集面は本文の欄 (セリフ / 文字) 1 つにする。
- **再生**: `[ミュート]` (+ 音声だけ `[逆再生]`)。
  - ミュートは全種類で `DiscreteClipEdit::Muted` 1 本にする。`clip.muted` が全種類共通の SSoT (`handler/clip_events.rs:58-76`)。字幕の `TextMuted` 経由のトグル (`text_event.rs:60-78`) は消える。
- **色**: タイトルの色見本 (仕様 8)。行は作らない。
- 映像クリップは共通の欄だけになる。速度は端の Shift ドラッグが唯一の編集面なので (`common/src/model/content.rs:1051-1053`)、ここには出さない。

**セル / 列**

- セルを選んでいるときは、クリップの帯の中に小見出し「ローンチ」とローンチの欄を出す (`launch_section.rs:158-` の `draw_cell_rows`)。
- 列を選んでいるときは帯「列」(対象 = 列名) を出す (`launch_section.rs:118-155` の `draw_scene_follow_rows`)。
- 表示の条件は今の `live_launcher_cells()` / `selected_scene_ids` のまま (`launch_section.rs:97-111`)。

**消すもの**

- `device_panel/mod.rs:21, 25, 26`: mod 宣言。
- `device_panel/mod.rs:52-54`: 呼び出し。
- `opened_plugin_is` の gate のうち声 / 読み上げ / 字幕の分: `clip_voice.rs:14`、`talk.rs:17`、`text_event.rs:29-31`。
- 字幕デバイスの Par:
  - 中身が無くなるので、`ChainEntry::shows_param_panel` (`chain_rows.rs:198-200`) を字幕デバイスで false にする。そのために `ChainEntry` に `is_subtitle` を足す。
  - `handler/devices.rs:509-519` の字幕の分岐を消す。
  - 保存時に、Par を持たない device の鍵を `open_rack_panels` から落とす (`handler/view_state.rs:100-112` の filter に足す)。
- `InspectorScrubField::Text { device_id, .. }` / `Talk { device_id, .. }` の `device_id` (`app_types.rs:413-415, 421-422`): 編集面が 1 つになるので `Text { field }` / `Talk { kind }` に縮める。
  - `view/modulation.rs` の `scrub_field_mod` の `F::Text { field: …, .. }` (:183-196) はそのまま通る。

### 仕様 4 帯ごとに独立して開閉

- `InspectorFolds` が帯ごとの集合なので、互いに影響しない。排他の開閉は作らない。

### 仕様 5 プロジェクトに保存、`*` を付けない

- §2.5 の通り、`cur.view` に置き、ViewState で保存する。
- テスト (`daw_gui/tests/inspector_bands.rs`、新規): 帯を開閉したあと次の 2 つを確かめる。
  - `song_doc.is_dirty() == false` と、undo の段数が変わらないこと。
  - `snapshot_view_state` → `restore_view_state` で状態が戻ること。知らない id は捨てること (`daw_gui/tests/view_state.rs:85-` の round-trip テストにも 1 ケース足す)。

### 仕様 6 種類ごとに共通

- 鍵が種類しか持たない (§2.5)。
- テスト: 音声クリップ A で帯を畳む → 音声クリップ B を選ぶと畳まれたまま → 画像クリップを選ぶと開いている。トラックの帯: Synth で Rack を畳む → Audio に移っても畳まれたまま。

### 仕様 7 詳細 (段 2)

- `InspectorDetail::LaunchMidi`: 「MIDI 割り当て (n 件)」 (`launch_section.rs:490-603`)。セルの場合も列の場合も同じ中身なので鍵を共有する。
- `InspectorDetail::SubtitleStyle`: 「字幕の細かい設定」。中身は回転と、小見出し「塗り」「縁」「影」の R/G/B/A・太さ・X/Y・ぼかし (`text_event.rs:187-205` の 17 行ぶん)。主の欄に残すのは文字・フォント・揃え・X/Y/W/H・大きさ・不透明度・フェード。
- 詳細の行は、その小まとまりの末尾に置く。開いて動くのは詳細の行より下だけになる。

### 仕様 8 タイトル (段 1)

- `daw_gui/src/view/track_inspector/title.rs` (新規) と `daw_gui/src/handler/inspector_title.rs` (新規) を作る。後者は `fn inspector_title(&self) -> InspectorTitle` で、`view_model.rs:271-283` の `selected_track_label` を置き換える。

```rust
pub struct InspectorTitle {
    pub main: String,                                 // 「Drums」「3 クリップ」「列「Verse」」「3 トラック」「Vocal」
    pub main_swatch: Option<([f32; 3], ColorPickerTarget)>, // 単一のクリップ / 列 / トラックだけ
    pub sub: String,                                  // 「音声クリップ · トラック Audio」「MIDI トラック」…
    pub sub_swatch: Option<([f32; 3], ColorPickerTarget)>,  // クリップのとき = トラック色 (ColorPickerTarget::Track)
}
```

- どれを出すかの優先順: クリップ (セル含む) → 列 → トラック。
- 数: `inspector_target_refs().len()` (`selection_view.rs:941`) / `selected_scene_ids` / `selected_track_ids`。
- クリップ名は `widgets/arrangement/view_build.rs:625` の `clip_display_label` を使う (アレンジの表示と同じ 1 本)。
- 色は次を使う。
  - `track_color::effective_clip_color` / `effective_track_color` (`view/track_color.rs:52-58`)
  - 列の色は `ColorPickerTarget::Scene` (`color_target.rs:33`)
- 今のトラック色見本 (`mod.rs:229-252`) はタイトルの 1 行目に統合する。
  - トラックを選んでいるときは 1 行目がトラックの色見本。
  - クリップや列を選んでいるときは、2 行目にトラック名と小さな色見本を出す。
  - トラック色を変える操作はタイトルの中に必ず残る (スクロールしない場所)。
- 高さはどの選択でも `TITLE_H` で同じにする。選択を変えても帯の起点が上下しない。

### 仕様 9 欄の揃え (段 3)

**form へ移す**: すべて `form::cols` / `form::label` / `FORM_ROW_PITCH` に揃える。対象と、今のラベル幅と送り:

| 対象 | 今の寸法 |
|---|---|
| `clip_band/*` (音声 60 / 画像 50) | ラベル 60 / 50 |
| 字幕 | ラベル 60 |
| 読み上げ | ラベル 48、部品 20 / 送り 24 (`talk.rs:263-293`) |
| 入力 | 6:4 横並び (`input_section.rs:44-53`) を 2 行に |
| 口パク | ラベル 40 |
| SC パネル | ラベル 34 (`plugin_row.rs:182`) |
| Par の中の `group_transform` | ラベル 64 (`group_transform.rs:29`) |
| Par の中の `video_fx` | ラベル 88 (`video_fx.rs:20`) |
| Par の中の `plugin_params` | ラベル 96 (`plugin_params.rs:33`) |
| Par の中の `lipsync` | 見出し + 全幅 dropdown を「口パク先」の 1 行に |
| ローンチ | (下の注) |

- ローンチ: 量子化・モード・長さ・動作 [ループ][レガート] は form の行にする (段 2 で実施。欄名は、同じ帯の先頭の「再生 [ミュート]」と並ばないよう「動作」にした)。フォローアクションの 2〜3 行は複合の操作なので 3 列の格子のまま、小見出し「フォローアクション」の下に置き、左端をラベル列の左端に揃える (§8-6)。
- A 列: A を持つ欄 (画像・字幕・Group Transform) も持たない欄も、値の右端を `auto_x - FORM_GAP` に揃える。
- 同じトグル部品の形の不揃いを直す (map §4-8)。
  - ミュートの幅 (音声は半幅、画像 / 字幕は全幅) → 共通の「再生」行 1 本。
  - 伸縮だけラベルが上の行 (`audio_event_section.rs:116-126`) → 左ラベルに。
  - 「長さ (拍)」だけ別の格子 (`launch_section.rs:207-225`) → form の行に。

**Par の見た目**

- 中身の左端: `layout::par_rect` で `x + ROW_NAME_X`。plugin 系は `plugin_row.rs:38-41`、内蔵は `native_row.rs:164-166` と `native_row.rs:251-253` (Limiter)。
- 行の背景と hover を見出しの 26px だけにする。
  - `chain_list.rs:461-467` の `draw_row_bg` に渡す矩形を、展開を含む `content` から `Rect { h: base_row_h(&r.kind), ..content }` に変える。
  - 見た目の hover は `hovered && ui.hovers(head)`。
  - Q の宛先 (`inspector_hovered_row`) は今どおり Par を含む行全体のまま (`bypass_toggle.rs:91-96` の「Par パネル込み」の規約を変えない)。
  - 同件: マスターの Limiter 行 (`native_row.rs:212-214` の `row` は `par_h` を含む) も同じに直す。
- Par の中の小見出しは `form::sub_heading` (= 「ROOM ――」形)。
- 行名と同じ見出しは消す。
  - `video_fx.rs:16` (`def.name`)
  - `plugin_params.rs:21-27` (plugin 名)
  - `group_transform.rs:17-24` (「Group Transform」)
  - `lipsync.rs:41-48` の見出しは欄のラベルに変える。
- 内蔵 device の Par の背景 (`native_row.rs:172-177` の `p.panel`) は残す。帯の本文の面と同じ色なので、見た目は「背景なし」と揃う。つまみのリングのくり抜き色に使っているので消さない。
- **段 3 の実装**: 展開 (Par / SC / 出力) は、余白の press を行幅いっぱい (`full`) で名乗り (行の左 8px の隙間を
  掴んでも行が動かない)、中身は `layout::expansion_inset(full)` に描く (`par_rect` は Par が最初の展開のときの形)。
  Par の欄の値の列は帯と同じ x (§2.3 `cols_nested`)。Par の上下の余白 (`PAR_PAD`) は `device_panel::draw_device_panel`
  が持ち、各セクションは欄を積むだけ (見出しを描かない)。Group Transform の欄名も日本語 (X / Y / 回転 ° / 拡大 X・Y /
  基準点 X・Y / 不透明度)。Parallel の Selector の欄は「アクティブ」「フェード」(帯域名 Low / Mid / High は chain 名と
  同じ語のまま)。口パクの帯 (形状 → 画像) も欄の形にした。

### 仕様 10 組み込みの Comp / EQ のオフの見た目 (段 3)

- 行は今どおりいつも出す (`native_row.rs:45-107`)。
- オフのとき (`entry.bypassed`):
  - 名前は `text_dim`。今は `text_faint` で (:105)、これは palette の定義で「disabled」の層 (ui `theme.rs` の `text_faint`)。
  - 小表示の位置 (:89-97) には、灰色の EQ カーブ / GR バー (:110-131) の代わりに「オフ」の札を描く。札は `inset_bg` の窪み + 10px `text_dim` の中央寄せ。
  - 札の矩形のダブルクリックで ON/OFF は今どおり (Q15 の決定、`native_row.rs:5-7`)。
- 同件 (同じ根: 「オフ」を disabled の色で描いている)。

| 箇所 | 直し方 |
|---|---|
| `native_row.rs:105` (内蔵の名前) | text_dim + 札 |
| `native_row.rs:229-243` (master Limiter) | text_dim + 札 |
| `plugin_row.rs:159-165` (plugin のバイパス) | text_dim + 名前の右に札 |
| Parallel のヘッダ (`parallel_header.rs` のバイパス表示) | text_dim + 札 |
| モジュレーションの変調器の見出し (`modulation_rack/mod.rs` の `src.enabled`、段 3 の同件チェックで追加) | text_dim + メーターの位置に札 |
| モジュレーションの変調先の行 (`row.effective` / `row.enabled`、同上) | 効いていない行は text_dim、その 1 本がバイパスなら depth 欄の左に札 |

- トラック自体が無効 (r.md #131、`plugin_row.rs:161` の `disabled`) は本当に使えない状態なので、`text_faint` のまま区別する。
  Selector の非アクティブ chain (`chain_row.rs` の `inactive`) は「今鳴っていない」の表示でバイパスではないので変えない。
  内蔵 Par の中の GR の値 (`native_panel/limiter.rs`) は音のパラメータの格子の中なので変えない。
- ミキサーの帯とマスターパネルの共有部品 (`view/native_device`) は変えない。Rack の行の小表示だけを変える。

### 仕様 11 言葉 (段 1 で見出し、段 3 で欄名)

**規則**

- 見出し、欄名、トグルの文言は日本語にする。
- 次は英語のまま残す。
  - 記号: Par / SC / Mix / dB / A / st / px / M / S / X / Y / W / H / R / G / B
  - 「Rack」
  - 選択肢の値で、手順書や他の DAW と同じ名前で通じるモード名: Raw / Repitch / Stretch / Slice / Trigger / Gate / Toggle / Repeat / Linear / Exp / S-Curve / Pre-FX / Post-FX / Post-Fdr / Split の値 / Match
- 値は「見出し・欄名」ではないので変えない (§8-7)。

**主な対応** (`vocab.rs`)

| 今 | 新 |
|---|---|
| N tracks selected / (no track) (`view_model.rs:274, 281`) | N トラック / トラックなし |
| Audio Event / Image Event | 帯「音声クリップ」/「画像クリップ」 |
| (無し) | 帯「MIDI クリップ」「映像クリップ」「テキストクリップ」 |
| Clip Voice / 読み上げ (Talk) / Text Event | 小見出し「歌声」「読み上げ」「字幕」 |
| Modulation | 帯「モジュレーション」 |
| 口パク (口形状 → 画像) / 口パク出力先 | 帯「口パク」/ Par の欄「口パク先」 |
| 列「x」のフォローアクション | 帯「列」対象「x」+ 小見出し「フォローアクション」 |
| Post-Fader (`native_row.rs:197`) | フェーダー後 |
| Reverse / Mute / Flip H / Flip V | 逆再生 / ミュート / 左右 / 上下 (欄名「反転」) |
| Stretch / Gain dB / Pan / Pitch st / Formant st | 伸縮 / ゲイン dB / パン / ピッチ st / フォルマント st |
| Fade In / Fade Out / In Curve / Out Curve | フェードイン / フェードアウト (カーブは同じ行) |
| Opacity / Rot (°) / Size (px) | 不透明度 / 回転 ° / 大きさ px |
| Text / Font / Align | 文字 (読み上げがあれば「セリフ」) / フォント / 揃え |
| Fill / Out / Sh の R/G/B/A, Out W, Sh X/Y/Blur | 小見出し「塗り」「縁」「影」+ R/G/B/A / 太さ px / X px / Y px / ぼかし px |
| 声一覧を再取得 / talk 声一覧を再取得 | 声の一覧を取り直す |
| + Plugin / + FX / + chain | + プラグイン / + エフェクト / + チェーン |

- 仕様 9 のテスト: `vocab` の欄名を全部 `Ui::measure_text(LABEL_FONT)` で測り、`FORM_LABEL_W - 4` に収まることを `#[test]` で確かめる。同じ測り方で、ラベルが欄に食い込む問題 (撮影で見えた「Formant st」の窮屈さ) を機械で止める。

### 仕様 12 幅と表示 (段 1)

- 実装は §2.6 の通り。
- 描画 (`track_inspector::draw`):
  - 右端の `RESIZE_HANDLE_W` の列を scroll viewport の**外**に取り、`ui.resize_handle(…, Orientation::Horizontal)` を置く。scroll_area の縦スクロールバーは viewport の右端にあるので (`scroll_area.rs:225`)、重ねると掴み合いになるため。
  - 位置の写像は `next = drag.pointer.0 - rect.x` で、`InspectorEvent::SetWidth { w, commit: released }`。
  - 1px の `border` の縦線 (`master_panel.rs:151-158` と同じ) を描く。
- 隠しているときも `track_inspector::draw` は毎フレーム呼ぶ。早く抜ける前に hover の宛先を None で公開する (§6-3)。

### 仕様 13 複数トラック (段 1)

- タイトルの 1 行目は「N トラック」(仕様 8)。
- 帯の対象表示:

| 帯 | 単一選択 | 複数選択 |
|---|---|---|
| 入力 | トラック名 | 「選択中の N トラック」(`input_target_track_ids`、`audio_rec.rs:83-92`) |
| Rack / 口パク / モジュレーション | トラック名 | 「<カーソルのトラック名> のみ」(`parallel.rs:383` / `view_model.rs:341-345` の通り、カーソルの 1 本) |

- 無効なトラックでは、Rack の対象に「(無効中)」を添える (`chain_list.rs:109-110` の文言を移す)。

### 仕様 14 入力の帯で意味の無い欄を出さない (段 2)

**判定**: `handler/audio_rec.rs` に `pub fn recordable_inputs(&self, track: &Track) -> RecordableInputs { midi: bool, audio: bool }` を新設する。トラックの「種類」は持たず、中身から導く (memory「役割判定しない」の port 直結の考え方)。

- **MIDI を録れる**:
  - chain (Parallel の中も) に `PortConfig::has_note_input` の device がある (`common/src/port_config.rs:28-29`)。
  - または、MIDI クリップを持つ (段 2 で追加: 楽器を挿していない MIDI トラックで MIDI が消えないように)。
  - または、組み込み以外の device もクリップも子トラックもまだ無い空のトラックである (段 2 で「子トラック」を追加:
    組み込みだけのグループのバスを空のトラックと数えない)。
- **音声を録れる**:
  - 子トラックを持たない (他のトラックの `parent_group_id` がこのトラックを指していない、`track.rs:136-142`)。
  - かつ、映像系の中身 (Image / Video / Text のクリップ、`PortConfig::is_video` の device) を持たない。
- マスターはどちらも false。

**表示**

- 入力の帯は、対象のどれかが録れるときだけ出す。
- 入力の選択肢 (`audio_input_choices`、`audio_rec.rs:117-138`) には録れるものだけを並べる。いま選ばれている入力が
  録れない種類なら「(録れない)」を添えて残す (デバイスに無い ch の「(無い入力)」と同じく、黙って付け替えない)。
- 欄名は「入力元」(帯の名前「入力」と同じ語を並べない)。
- モニターの行は、音声入力を選んでいるときだけ出す。出さないときは何も空けない (`input_section.rs:44-53` の 6:4 の固定幅をやめ、仕様 9 の 2 行にする)。
- 複数選択で書き込むときは、録れるトラックにだけ書く。

**テスト**: `audio_rec.rs` の `#[test]` で次のトラックの組を表にして確かめる。

- Synth
- Vocal
- Audio
- Image + 映像 FX
- グループのバス
- 楽器付きグループ (paraout)
- 空の新規トラック
- マスター

### 仕様 15 Parallel の主従 (段 3)

| 行 | 位置 (階層) | 形 |
|---|---|---|
| トラックの chain の末尾の「+ プラグイン」(`chain_row.rs:278-298`、depth 0) | 主 | 全幅のまま |
| chain の中の「+ プラグイン」(depth ≥ 1) | device の名前の左端 (`content.x + ROW_NAME_X`、chain の色帯の内側) | 文字幅 + 16px の小さいボタン、11px (`button_at_sized`) |
| 「+ チェーン」(`chain_row.rs:261-275`) | chain 行の色見本の左端 (chain 行と同じ階層) | 「+ チェーン」の小さいボタン、11px |

- これで「chain の中に足す」は device と同じ段、「Parallel に chain を足す」は chain と同じ段に並ぶ。
- **段 3 の実装**: 「+ チェーン」の左端は色見本の左端ではなく色見本の右 (`content.x + BAR_W`) にした。色見本の左端から
  置くと Parallel の括弧の帯 (色見本と同じ x) をボタンが切り、括弧が途中で途切れて見えたため (撮影で確認)。chain 行の
  見える面も色見本の右から始まるので、段は chain 行と揃う。

---

## 4. 編集面の移動 (移動元 → 移動先 → 消すもの)

| 編集面 | 移動元 | 移動先 | 移した後に消すもの |
|---|---|---|---|
| 歌声の声 (キャラ / スタイル / 取り直す) | `device_panel/clip_voice.rs:7-163` (VOICEVOX の Par、gate :14-24) | `clip_band/voice.rs` (MIDI クリップの帯、`track.is_voicevox_vocal()` のとき、小見出し「歌声」) | `clip_voice.rs`、`device_panel/mod.rs:21, 53`、見出し「Clip Voice」(:47-54) |
| 読み上げ (ここから読む / +字幕デバイス / セリフ / 話者 / スケール) | `device_panel/talk.rs:7-314` (VOICEVOX の Par、gate :17-34) | `clip_band/talk.rs` (テキストクリップの帯、VOICEVOX があるとき、小見出し「読み上げ」)。セリフはクリップの帯の先頭の本文の欄 1 本 | `talk.rs`、`device_panel/mod.rs:25, 54`、`talk.rs:93-96` の「字幕があればセリフを隠す」二重表示よけ (本文の欄が 1 本になるので不要)、`InspectorScrubField::Talk` の `device_id` |
| 字幕 (ミュート / 文字 / フォント / 揃え / 数値 25 / カーブ 2) | `device_panel/text_event.rs:7-427` (字幕デバイスの Par、gate :23-31) | `clip_band/subtitle.rs` (テキストクリップの帯、字幕デバイスがあるとき、小見出し「字幕」)。ミュートは共通の「再生」行、文字は本文の欄、細かい数値は詳細 `SubtitleStyle`、フェードは `form::fade_row` | `text_event.rs`、`device_panel/mod.rs:26, 52`、字幕デバイスの Par ボタン (`chain_rows.rs:198-206`)、`handler/devices.rs:509-519` の字幕の分岐、`InspectorScrubField::Text` の `device_id`、`DiscreteClipEdit::TextMuted` を使う欄 |
| 音声 (Audio Event) | `audio_event_section.rs` (最上位) | `clip_band/audio.rs` | 改名して移す (`git mv` で履歴を残す) |
| 画像 (Image Event) | `image_event_section.rs` (最上位) | `clip_band/image.rs`。PiP の 6 行は欄の表を回す loop にして、手で写した 6 本をやめる (`draw_pip_rows` 260 ncloc が縮む) | 同上 |
| セルのローンチ / MIDI 割り当て | `launch_section.rs:158-` / :490-603 | クリップの帯の小見出し「ローンチ」/ 詳細 `LaunchMidi` | `launch_section.rs` (改名して移す)。古いコメント :8-13 (「行の高さは状態で変えない」は ジャンプ行 :438-456 と食い違う) と :37-39 (既定 14px は今 12px) を直す |
| 列のフォローアクション | `launch_section.rs:118-155` | 帯「列」 | 同上 |
| Parallel Out | `chain_sections.rs:112-205` | plugin 行の `Out▾` → 行の直下 | `chain_sections.rs`、`mod.rs:310`、`view_model.rs:285-290` の doc の「inspector's "Parallel Out" section」 |
| 読み込み失敗 | `chain_sections.rs:35-105` | plugin 行の中 (理由 1 行 + 再読込) | `chain_sections.rs`、`mod.rs:309`、`chain_rows.rs:29-32` の doc |
| トラック色 | `mod.rs:229-252` (タイトルの右端) | タイトルの色見本 (トラックのとき 1 行目、クリップ / 列のとき 2 行目) | 旧スウォッチの描画 |

---

## 5. 段の分け方

全体は worktree で進める (1〜2 ファイルの修正ではない)。検証は変更に関係するテストを名指しで回す。全件 (`make test-nolaunch`) は統合後に 1 回だけ回す (CLAUDE.md「全件は統合後に 1 回」)。実機の見た目の sign-off は段 3 の後にまとめて 1 回頼む。

### 段 1 骨格 (仕様 1・2 の並び・4・5・6・8・12・13・11 の見出し)

**作るもの**

1. core: `widgets/resize_handle.rs` + `master_panel.rs:128-150` を置き換え。`popup.rs` の dropdown の stale close (§2.7)。
2. 状態:
   - `state/inspector_view.rs`、`ProjectView.inspector_folds`
   - `ViewState` の 2 フィールド + snapshot / restore / 新規時のクリア
   - `UiPrefs` / `AppConfig` の `inspector_open` / `inspector_w`
   - `event_inspector.rs` + `handler/inspector_view.rs`
   - ショートカット `I` + View メニュー
3. view:
   - `layout.rs`、`band.rs` (詳細の行も段 1 で作る。使うのは段 2)、`vocab.rs` (見出しの分)、`title.rs` + `handler/inspector_title.rs`
   - `mod.rs` を「タイトル + 帯の列」に作り直す。選択の帯の中身は今の `launch_section` / `audio_event_section` / `image_event_section` をそのまま包む。声 / 読み上げ / 字幕はまだ Par の中。
   - root.rs の幅と表示、右端のリサイズ帯
   - hover の公開を 1 か所に (§6-3)、scroll の床 (§6-1)、EQ スペクトラム要求の gate (§6-4)、畳むときの編集の確定 (§6-5)
4. テストの追従: 窓幅計算の定数 `INSPECTOR_W` → `INSPECTOR_DEFAULT_W`。対象は `automation_hover_visual.rs:42`、`mixer_pan_readout_visual.rs:33`、`master_native_blocks_visual.rs:215`、`chain_split_click.rs:87-89`、`native_rack_visual.rs:23`、`param_gesture_surfaces.rs:30`、`inspector_visual.rs:27`。「Rack」の glyph は帯の見出しに残るので、`native_rack_visual.rs:321-336` はそのまま通る想定。

**完了条件**

- `cargo check -p daw_gui`、`make clippy` (gates 込み)、`make arch-lint` が exit 0。
- 名指しのテスト:
  - `cargo test -p daw_gui --test inspector_bands` (新規)
  - `--test view_state`、`--test native_rack_visual`、`--test param_gesture_surfaces`、`--test chain_split_click`、`--test inspector_visual -- --nocapture`
  - `cargo test -p daw_gui --lib app_config`
  - `cargo test -p daw-ui-core resize_handle`、`cargo test -p daw-ui-core dropdown`
- `inspector_bands` で確かめること:
  - 見出しのクリックで開閉し、見出しの glyph の y が変わらない (先頭近く / 末尾までスクロールした状態の 2 通り)。
  - `*` が付かず、undo の段数が変わらない。保存して戻すと開閉が戻る。
  - 種類ごとに共通 (仕様 6)。
  - Rack を畳むと `wanted_device_scopes()` が空で、`inspector_hovered_row` が None。モジュレーションを畳むと `inspector_hovered_mod` が None。
  - 隠すと hover が None で、開いていた dropdown が閉じる。右の領域がインスペクタの幅ぶん広がる。
  - 右端のドラッグで幅が 360〜640 にクランプされ、離したときに `app_config` に書かれる (`AppDirs::under(tempdir)`)。
  - 3 トラックの選択で、タイトルが「3 トラック」、入力の対象が「選択中の 3 トラック」、Rack の対象が「… のみ」。

**撮る状態** (`inspector_visual.rs` に足す。文言の assert は置かず、stderr に期待の見出しを出す今の流儀のまま)

- `s1_1_midi_track`
- `s1_2_audio_clip` (タイトル 2 行 + 選択の帯)
- `s1_3_multi_tracks`
- `s1_4_bands_collapsed` (Rack とモジュレーションを畳む)
- `s1_5_width_520`
- `s1_6_hidden`
- `s1_7_light_theme` (見出しの面のコントラスト)

### 段 2 クリップの帯への集約 (仕様 3・7・14、MIDI / 映像の共通欄、ローンチ)

**作るもの**

- `clip_band/` 一式と `voice_picker.rs`
- §4 の移動と削除
- 共通の欄 (名前 / 再生) と改名の面の札 (§6-11)
- 詳細 2 つ
- `recordable_inputs` と入力の帯の作り直し
- 字幕デバイスの Par の撤去
- `InspectorScrubField` の鍵の縮小

**完了条件**

- 段 1 のテストに加えて、次を名指しで回す。
  - `--test inspector_bands` に足すケース:
    - 歌声の声がクリップの帯に出て、VOICEVOX の Par には口パク先だけが残る。
    - テキストクリップの本文の欄が 1 本だけ。
    - 字幕デバイスの行に Par が無い。
    - 帯を畳むと、入力中のセリフが確定する (§6-5)。
  - `cargo test -p daw_gui --lib recordable_inputs`
  - `cargo test -p daw_gui --test clip_rename_smoke`: daw_gui を `--script` で起動する target (CLAUDE.md の判定基準に当たる)。窓なしで音も出ないので、許可なしで回してよい。
  - `cargo test -p daw_gui --test app_state` の split_fidelity (`split_fidelity.rs:519-` は `inspector_audio_event_summary` を読む)
- `make arch-lint`: 新しいファイルの budget と、`draw_talk` / `draw_audio_event_section` の関数長が下がっていること。

**撮る状態** (`inspector_visual.rs` の `populate` に次を足す)

- 足す素材:
  - 映像トラック (実ファイル無しの `VideoContent`)
  - 読み上げトラック (VOICEVOX + 字幕 + Text クリップ)
  - ランチャーのセル 1 つと列 1 つ
- 撮る状態:
  - `s2_1_audio_clip`
  - `s2_2_vocal_clip` (歌声の小見出し)
  - `s2_3_vocal_par_open` (Par に声が無い)
  - `s2_4_talk_clip` (読み上げ + 字幕、詳細は閉)
  - `s2_5_subtitle_details_open`
  - `s2_6_image_clip`
  - `s2_7_midi_clip` (Synth の Lead、共通の欄だけ)
  - `s2_8_video_clip`
  - `s2_9_launcher_cell` (ローンチ + MIDI 割り当ては閉)
  - `s2_10_scene_column`
  - `s2_11_image_track` / `s2_12_group_track` (入力の帯が出ない)

### 段 3 中身の揃え (仕様 9・10・11 の欄名・15、Par の見た目、Parallel Out と読み込み失敗の移動)

**作るもの**

- `form.rs` と、すべての欄の form 化 (§3 仕様 9)
- Par の inset、小見出し、重複見出しの削除、行の背景を見出しだけに
- オフの見た目と同件 4 か所
- 欄名の日本語化と `vocab` のはみ出しテスト
- `+ プラグイン` / `+ チェーン`
- `Out▾` パネルと、読み込み失敗を行へ移すこと (`chain_sections.rs` を消す)
- 古いコメントと文書の更新 (§9)

**完了条件**

- 段 1・2 のテストに加えて、次を確かめる。
  - `native_rack_visual` / `param_gesture_surfaces` の Par の矩形を `layout::par_rect` に替えて通る。
  - 幅 360 で、Parallel の深さ 2 にある内蔵 Par の数値欄 (52px) が隣と重ならない (§6-10)。
  - 字幕・画像・音声・Par の欄の値の左端の x が全部同じ (glyph か rect の左端で比べる)。
  - `vocab` のラベルが全部ラベル列に収まる。
  - `cargo test -p daw_gui --test app_state plugin_load_failure`
- 統合の直前に `make test-nolaunch` を 1 回回す (回す前に一言断る)。

**撮る状態**

- `s3_1_midi_track`
- `s3_2_par_open` (EQ / Reverb、inset と小見出し)
- `s3_3_audio_clip`
- `s3_4_image_video_fx` (Color Grade の Par に見出しが重ならない)
- `s3_5_group_parallel` (+ チェーン / + プラグイン)
- `s3_6_master` (オフの札、フェーダー後)
- `s3_7_paraout_open`: aux 出力を持つ plugin は builtin の `PluginInstance` に `aux_output_count` を `edit_song` で立てて作る
- `s3_8_failed_load`: `plugin_load_failure` テストと同じ経路で `load_error` を立てる
- `s3_9_light_theme`

ここまで揃ったら、実機の sign-off を 1 回だけ頼む。release / debug のどちらで確かめるかを明記する。

---

## 6. 危ない点と対処

1. **末尾までスクロールした状態で畳むと帯が動く** (lag-by-one と offset のクランプ)
   - scroll_area は毎フレーム `offset` を `content_h - 窓の高さ` でクランプする (`scroll_area.rs:204-205, 245-246`)。末尾で帯を畳むと次のフレームで offset が減り、帯ごと下へずれる。
   - 対処: 開閉の Edit で「床」を記録する。`peph.inspector_scroll_floor = Some(ScrollFloor { subject, floor: offset + vp_h })` で、`offset` は `ui.scroll_offset("inspector_body")`、`subject` は選択の主 (タイトルの主語) とカーソルのトラック。
   - 描画は `content_h = max(inspector_body_h, floor)` にする。床は毎フレーム `min(floor, offset + vp_h)` に下げる (上へスクロールすると空白が消えていく)。`subject` が変わるか、実測が床を超えたら捨てる。
   - Par の開閉 (`ToggleRackPanel`) も同じ床を使う。今もある同じ現象がまとめて直る。
   - テストは段 1 の完了条件 (末尾の状態で見出しの y が不変)。

2. **hit-test**
   - 見出しのクリックは子 widget より先に `primary_click` を呼ぶ。後勝ちで子が press を取る (`click.rs:144-161`)。
   - 見出しの面の上に置く「+」dropdown は、`dropdown.rs:75` の `primary_click` が後から名乗る。
   - リサイズ帯は scroll viewport の外の列に置き、縦スクロールバーと重ねない (仕様 12)。
   - popup が開いている間は見出しを反応させない。

3. **Q の宛先 (hover) が残る**
   - `inspector_hovered_row` は chain list を描いたときだけ書かれる (`chain_list.rs:182-188`)。`inspector_hovered_mod` も同じ。帯を畳むかインスペクタを隠すと書く人がいなくなり、見えない行が Q で切り替わる。
   - 対処: マスターパネルの `draw` → `publish_master_panel_hover` (`master_panel.rs:97-114`、§18-AB) と同じく、`track_inspector::draw` の末尾で 1 回だけ公開する。中身の関数は hover を返すだけにする。描かれなかった帯は None。

4. **見えない EQ の Par にスペクトラムを計算させ続ける**
   - `wanted_device_scopes` (`rack_view.rs:47-53`) は Par の開閉しか見ていない。
   - 対処: インスペクタが隠れているときと Rack の帯が畳まれているときは空を返す。テストは `rack_view.rs:97-` の隣に 1 ケース。

5. **入力中の文字が確定せずに消える**
   - text_input は `blurred` で確定する (`talk.rs:127-131`、`text_event.rs:100-105`)。帯を畳んだ次のフレームでは text_input が描かれず、blur を報告できない。
   - 当初の対処案: `handler/inspector_view.rs` の開閉で、先に `CommitClipTextContentEdit` / `CommitRenameClip` を呼ぶ。
   - 実装 (段 1 で変えた): 開閉の handler では何もしない。見出しを押した press のフレームで、入力欄は「欄の外の press」で blur して確定する (`text_input.rs:220` の `blurred`)。見出しの開閉は release のフレームなので、畳む時点で確定は済んでいる (`handler/inspector_view.rs` の module doc)。

6. **持ち主の消えた dropdown の popup** — §2.7 (b) で core 側を直す。

7. **既存テストが文言と寸法に依存している**

   | テスト | 依存しているもの |
   |---|---|
   | `native_rack_visual.rs` | :32-37 `PAD` / `ROW_H` / `NAME_TOP`、:186 Par の矩形の式、:231 `"+ Plugin"` |
   | `param_gesture_surfaces.rs` | :160-167 Par の矩形の式、:422 `"Gain dB"` |
   | `chain_split_click.rs` | `"No split"` / `"3 bands"` (値なので変えない) |
   | `inspector_visual.rs` | 期待の見出し (assert ではない) |

   対処: `layout` と `vocab` の `pub` を読むように替える。JS のシナリオ (`daw_gui/tests/scripts`) と script API はインスペクタの文言に依存していない (grep で 0 件)。

8. **保存形式**
   - ViewState に足すのは `#[serde(default)]` の `Vec<String>` 2 本だけ。
   - 旧ファイルは「全部開・詳細は閉」で開く。新しい id を古い版が読んでも捨てるだけ (§2.5)。
   - プロジェクトのバージョン番号は上げない (Song の形は変わらない)。
   - app_config も `#[serde(default)]` で、`load` で幅をクランプする。

9. **サイズ budget** — §7。

10. **幅の下限と内蔵 Par の格子**
    - 格子は Par の幅を 6 等分し、数値欄は 52px (`native_panel/layout.rs:21, 70-75`)。
    - 幅 360 で Par を 8px 内側へ寄せると、深さ 0 で列 54.7px、Parallel の深さ 2 で約 52.7px になり、もう余裕がない。
    - 対処: 下限を今の 360 にする。360 より狭くするには格子を折り返す (段の数が幅で変わる) 必要があり、`panel_height` が幅に依存するようになるので、この作業には入れない (§8-5)。

11. **改名の入力欄が 2 つ同時に出る**
    - アレンジは `peph.clip_rename == Some(key)` のとき、クリップの上に入力欄を重ねる (`arrangement_view.rs:348-372`)。インスペクタの名前の欄から改名を始めると、両方に入力欄が出る。
    - 対処: `peph.clip_rename: Option<ClipKey>` (`state/project.rs:676`) を `Option<(ClipKey, RenameSurface)>` にする。`RenameSurface` は `Arrangement` / `Inspector`。
    - 札を付ける側:
      - F2 とアレンジの右クリック (`arrangement_view.rs:295`、`root.rs:1110`)
      - script の `dispatchRenameClip` (`script.rs:1135`)
      - インスペクタの名前の欄
    - 入力欄を描くのは、札が自分の面のときだけ。Esc の取消 (`root.rs:1237`) は札に関係なく効く。
    - **段 2 の実装 (札は作らなかった)**: インスペクタの名前の欄は打っている間の文字を入力欄自身に持たせ、
      確定 (Enter / 欄の外のクリック) のフレームで `BeginRenameClip` → `RenameClipChanged` → `CommitRenameClip` を
      1 つの Edit で通す。`peph.clip_rename` はフレームをまたいで立たないので、アレンジの改名欄は出ない
      (`clip_band/mod.rs::name_row`、テスト `inspector_bands::name_field_renames_without_the_arrangement_rename_box`)。

12. **undo に積まない**
    - 開閉と幅の変更は `edit_song` を通さない (§2.5)。段 1 のテストで undo の段数が変わらないことを確かめる。

13. **編集の束ね (bracket) の鍵**
    - `InspectorScrubField::Text` / `Talk` から `device_id` を外すと、同時に描かれる欄の鍵が重ならないことの保証は「インスペクタに字幕の欄は 1 組しかない」が担う。
    - `scrub_field` の doc (`mod.rs:99-101`) の規約どおり、テストで 1 ケース (字幕の X をドラッグ中に Y の bracket が閉じない) を足す。

---

## 7. サイズ budget の見込み (1 ファイル 1,000 / 1 関数 300 / ネスト 6、ncloc)

| ファイル / 関数 | 今 | 見込みと対処 |
|---|---|---|
| `track_inspector/mod.rs` | 329 行 (raw) | タイトルを外し、帯の列にするので縮む |
| `launch_section.rs` → `clip_band/launch.rs` | 807 行 (raw) | form 化で縮む。足すのは詳細の包みだけ |
| `device_panel/talk.rs::draw_talk` → `clip_band/talk.rs` | 264 ncloc | `voice_picker` に切り出し、form 化で 150 前後 |
| `audio_event_section.rs::draw_audio_event_section` → `clip_band/audio.rs` | 291 ncloc | `form::fade_row` と共通の欄に出すので 150 前後 |
| `image_event_section.rs::draw_pip_rows` → `clip_band/image.rs` | 260 ncloc | 欄の表を回す loop で 80 前後 |
| `device_panel/text_event.rs` → `clip_band/subtitle.rs` | 427 行 (raw) | 主の欄と詳細を関数で分ける |
| `chain_list.rs` / `plugin_row.rs` | 511 / 231 行 (raw) | `Out▾` パネルは `aux_out_panel.rs` (新規) に置く。`chain_sections.rs` (205) は消える |
| `app.rs::dispatch_app_event` | 1496 (天井 1605) | +1 行 (委譲 arm) |
| `view/root.rs::dispatch_shortcuts` | 450 (天井 530) | +5 行 |
| `handler/project.rs` | 958 ncloc | +1 行 (クリア) のみ |
| `ui/crates/ui/src/ui.rs` | 1559 (天井あり) | 触らない (新しい部品は別ファイル) |
| `modulation_rack/bodies.rs` | 1020 行 (raw) | 触らない (§8-6) |

新しいファイルはどれも 300 ncloc 程度に収める。各段の終わりに `python scripts/loc_budget.py --report` と `make arch-lint` で確かめる。

---

## 8. 計画で決めたこと (委任判断の中身。実装前に見直せるように一覧にする)

1. **帯の面の色は `control` (段 4 で `header` から変えた、§2.2)。** 仕様の例は `panel_raised` だが、Rack の行と同じ色になって境目が消えるため。
2. **帯の対象は、タイトルの 1 行目と同じ語なら省く (段 4 で変えた)。** トラックの帯は単一選択ならトラック名、複数なら「選択中の N トラック」/「… のみ」。段 3 までは「いつも出す」だったが、1 本のトラックを選んだだけでトラック名が 4 回並んだ (レビューの指摘)。タイトルはスクロールしないので、省いても対象は常に見えている。複数選択の対象は 1 行目と違う語なので残る。
3. **何も選んでいないとき、選択の帯は出さない。** 使わない欄の空白を残さない (仕様 14 と同じ考え)。帯を畳んでいれば、クリップを選んだときの Rack のずれは見出し 1 本ぶん (24 + 6px) で済む。
4. **トラック色の操作はタイトルに置く。** クリップや列を選んでいるときは 2 行目の小さな色見本 (スクロールしない場所なので必ず見える)。
5. **幅の下限 360 / 上限 640 / 既定 360、キーは I。** 下限の理由は §6-10、キーは Logic と同じ。
6. **モジュレーションの変調器の本体 (Shp / Jit / Smo / Steps / Dly / Fade / φ / Free の密な行) と、映像 FX の Par の param 名 (Exposure …) は、行の形も名前も変えない。** どちらも device (変調器 / 映像効果) そのもののパラメータで、名前は効果の定義が持ち、オートメーションのレーン名・変調先の名前と同じ語 — Par の中だけ訳すと 1 つの param に 2 つの呼び名ができる。内蔵 Par の格子 (HP / Thr / ROOM) と同じ扱い。揃えるのは左端と小見出しと開閉マーク (枠の無い glyph) だけ。
   - **フォローアクションと MIDI 割り当ては段 4 で欄の行に直した。** 段 3 までは 3 列の格子 ([有効][クリップ終端▾][1] / [なし▾][100][なし▾]) だったが、欄名が無く「1」「100」が何か読めず、右端も欄の列からはみ出した (レビューの指摘)。これらは音のパラメータではなくクリップ / 列の設定なので仕様 9 の例外に当たらない。発動 [有効][クリップ終端▾] / 回数 or 間隔 / アクション A / A の確率 % / アクション B、ジャンプの飛び先は同じ行の右半分 (行が増えない)。MIDI 割り当ては 鳴らす [セル][列] / 止める [行][全部] / アレンジへ戻す [行][全部] / 消去。
7. **選択肢の値 (Repitch / Trigger / Linear / Pre-FX / Split の値 / Match) は英語のまま。** 仕様 11 の対象は見出しと欄名なので。ただしカーブの表記は 1 本 (「S-Curve」) に揃える。
8. **「オフ」の見せ方。** 組み込み Comp / EQ だけでなく、バイパス中の行すべて (plugin / Parallel / Limiter) を同じ規則にする: 名前を text_dim + 「オフ」の札。トラック無効 (r.md #131) は text_faint のまま区別する。
9. **読み込み失敗も行の中へ移す。** 仕様 2 の「トリガから離れて出ているもの」と同じ型だと判断した。
10. **入力の帯の判定** (§3 仕様 14 の規則): 型を持たず、port とクリップの中身から導く。
11. **MIDI 割り当ての詳細の鍵は、セルと列で共有する** (中身が同じ部品のため)。
12. **詳細は 2 つだけ** (MIDI 割り当て、字幕の細かい設定)。音声・画像・読み上げの欄は主の欄に残す。

---

## 9. 文書・コメントの更新 (段 3 の最後)

- `docs/plan_rack_native_devices.md:17, 30, 64`: Q1/Q2「固定 360px・ドラッグで変えない」は r.md #158 で置き換わった (幅は 360〜640 で可変、隠せる) と書く。`view/root.rs:30` のコメントも同じ。
- `daw_gui/CLAUDE.md:14-20`: 「1 本の縦 scroll に帯を積む、帯の中は今の `(app, ui, area, pad, y)` 契約、hover の宛先は `track_inspector::draw` の末尾で 1 回だけ公開する」を 1 行で足す。散文の再掲はしない。
- `track_inspector/mod.rs:1-5` の module doc (「Chain」見出し、「+ Instrument / + Effect / + MIDI FX」は古い)。
- `launch_section.rs:8-13, 37-39` の古いコメント (移すときに直す)。
- `view_model.rs:285-290`、`chain_rows.rs:29-32` の「セクション」への言及。
- この計画書は `docs/plan_rmd_158_inspector.md` に置く。r.md は編集しない。

---

## 10. 段 4 レビューの指摘の修正 (見た目 / 正しさと操作 / 規約と構造)

直したもの (撮影は `inspector_visual.rs` の `s4_*`、テストは `inspector_bands.rs` / `native_rack_visual.rs`):

- **タイトル**: 2 行目のトラック名と色見本は、クリップを選んでいればクリップの居るトラック (カーソルではない)。
  名前の無いクリップは「(名前なし)」、2 行目に種類と「セル」(ランチャーのセルのとき)。マスターは 2 行目を出さない。
  色見本の当たりは `primary_click` だけ (button を下に敷くと枠が 1px はみ出した)。
- **帯の対象**: タイトルの 1 行目と同じ語なら省く (§8-2)。入力の帯の対象は書き込み先 (`input_target_track_ids`)
  の名前。Rack の見出しに「読み込み失敗 N」(警告色、畳んでも見える)、畳んだ Parallel / 閉じた chain の行に
  「失敗 N」— 読み込み失敗を行の中へ移したことで、隠れた行の失敗が見えなくなっていた。
- **見出しの面**は `control` (§2.2 / §8-1)。
- **入力元**: MIDI を録れないトラックの既定 (MIDI) は「未選択」(「MIDI (録れない)」の警告にしない)。
- **フォローアクション / MIDI 割り当て**は欄の行 (§8-6)。MIDI 割り当ての詳細は学習の待ち受け中に「· 学習中」を
  添える (畳んでいても次の MIDI 入力が割り当てられることが見える)。Learn の「シーン」は帯と同じ「列」。
- **オフの札**は全部の行で名前の直後 (内蔵 device / Limiter もダブルクリックで札の上からも ON に戻る)。文字は
  `text_dim` を窪みの上で 4.5:1 まで寄せた色 (ライトで 3.8:1 だった)。
- **字幕の欄名**: アルファは「不透明度」(自動化の「A」と並んで紛れた)、W / H は「幅」「高さ」。
- **+ チェーン**は chain 行と同じ幅の枠だけの行 (chain の中の小さい「+ プラグイン」と形で分ける)。
- **行の直下に重ねて開いた展開** (出力のパネル → Par など) の 2 つ目から上端に罫線。
- **モジュレーションの開閉マーク**は枠の無い glyph (帯 / Parallel / chain と同じ形)。
- **複数選択**: 歌声の声と読み上げの話者 / スケールは選んでいる同じ種類のクリップ全部へ (`SetClipVoice` /
  `SetClipTalkParam` が `clips: Vec<ClipKey>`、1 undo step)。割れていれば dropdown は空 / 数値は「—」。名前・本文・
  「ここから読む」はクリップ 1 つの値なので 1 つだけ選んでいるときだけ。
- **窓に字幕の片が無いテキストクリップ**: 編集バッファの対象を `Some` に揃える (`None` に戻すと毎フレーム resync が
  積まれ、再描画が止まらなかった)。
- **幅のドラッグ**: `ResizeDrag::handle_origin` (帯の中の掴んだ位置を保った帯の左上) で縁を出す (掴んだ瞬間に最大
  5px 跳んでいた)。マスターパネルのセクション境界も `resize_handle` (Vertical) にし、設定 / ラウドネスの浮動窓の
  右端・下端も `resize_handle` (カーソル / popup の間は掴まない) にした。隅 (2 軸) は `CursorIcon` に斜めの形が
  無いので今のまま。
- **SSoT**: 行の寸法 (`layout::RACK_INDENT` / 見出しの隙間 / タイトルの余白 / `BAND_ADD_W`)、内蔵 Par の小見出しの
  送り (`native_panel::layout::SECTION_H` = `SUB_HEAD_H`)、展開の y (`layout::expansion_rect`、描画と `par_rect` が
  同じ 1 本)、文言 (タイトルの語・フォローアクション・MIDI 割り当て・X / Y / 幅 / 高さ を `vocab`)、「字幕は Par を
  持たない」(`common::plugin_db::is_subtitle` + `AppData::rack_panel_key_valid`、ViewState の保存と復元が同じ判定)、
  「マスターは Out▾ を出さない」(`aux_out_entry` の 1 本)。
- **消し忘れ**: `PluginParamsInspector.plugin_name`、`AppEvent::SetClipTextMuted` (誰も送っていない別名)、
  `let active = true`、`F::Text { .. }` の `..`。

直さなかったもの (理由):

- **入力の帯が操作の結果で消えて Rack が上へずれる** (空のトラックに映像 FX を足す等): 開閉の操作ではなく、トラックの
  中身が変わって入力の意味が無くなった結果。仕様 14 は意味の無い欄の空白を残さないことを求め、押した「+ プラグイン」は
  行が 1 本増えるのでどのみち動く。高さを保つと、説明の無い空白が帯の上に残る。
- **モジュレーションの変調器の本体と映像 FX の Par の param 名の英語** (§8-6): device そのもののパラメータ名
  (オートメーション / 変調先の名前と同じ語)。
- **帯の中の小見出しと欄名の区別が罫線だけ**: 小見出しの形 (10px・text_dim・罫線) は仕様 9 が `native_panel/cell.rs` の
  「ROOM ――」に揃えると決めたもの。見出しの面を `control` にして帯の見出しとの差は付いた。
