# plan: 修飾なしドラッグで範囲選択 (FIXME #45)

## ゴール

アレンジビュー (クリップ) とピアノロール (ノート) で、**空き場所を修飾キーなしでドラッグ**
したら範囲選択 (marquee) を起動する。現状は marquee 起動に Shift が必須。クリップ/ノートの
上のドラッグは従来どおり「移動」。標準 DAW (REAPER/Live/Bitwig) と同じ操作感にする。

## 確定設計 (インタビュー済、再議論しない)

- marquee は **空き zone の plain drag で起動**。clip/note の上の plain drag は移動のまま
  (= 起動条件は「ヒットしていない空き zone」)。ruler/header/automation lane header は対象外。
- 選択意味論: **無修飾 = REPLACE (旧選択破棄)、Shift = UNION (追加)、Ctrl = XOR (トグル)**。
  これは既存の automation point lasso と同一規則。
- アレンジ (クリップ) とピアノロール (ノート) の両方に適用。
- 修正の主体は **gui_01 の arrangement / piano_roll widget**。daw_01 は既存の
  `SetClipSelection(next)` / `SetNoteSelection(next)` (full-replace) をそのまま受けるだけ。

## (A) gui_01 要望仕様

> 提出先: `docs/gui_01_conversation.md` に `[要望]` として追記。
> 関連仕様: `docs/plan_drag_marquee_select.md` を必須で含める。
> 段階分割せず最終形態を全部書く。

### しきい値は 4px (重要)

クリック降格しきい値は両 widget とも **4px** (arrangement.rs ~5855 Move→click、
piano_roll.rs ~1803、lasso は arrangement.rs ~7414)。「16px」は古いコメント由来の誤り。
既存 4px をそのまま使う。テストのクリックケースは sub-4px。

### A-1. arrangement marquee 起動ゲート (置換 ~7742-7752)

```
marquee_press = primary_just_pressed
    && pointer.pos が `lanes` 内
    && !modifiers.alt
    && !press_in_automation_lane
    && !splitter_press
    && clip_hit(...).is_none()
    && no_session   // press 時点で clip_drag/audio_drag/track_volume_drag/track_reorder/
                    // track_row_resize_drag/header/playhead_drag/loop_drag/automation_* drag
                    // のいずれも無い (~5420-5433 のリストを複製)
```

- `|| shift_rect_active` の continuation は **保持** (rename 可だが drag_start.is_some() を
  読む ~7743-7746 の項を残す。drag 中に primary が押されていないフレームでも primitive に
  feed する)。
- `!modifiers.ctrl` の項は **削除** する (Ctrl+空き = XOR。クリップの Ctrl/Ctrl+Shift clone は
  clip HIT のときのみ起動するので、空き zone ゲートから !ctrl を外しても安全)。
- `clip_hit().is_none()` は **load-bearing**: クリップ MOVE は `(!shift || ctrl)` でゲート
  (~4869) されているため Shift+press on clip は clip_drag を起動しない。hit-test が無いと
  Shift+クリップ press が誤って marquee を起動する。MOVE と**同じ** `clip_hit`
  (`style.resize_handle_px` も同じ) を使い、端 press が両方起動しないようにする。

### A-2. arrangement commit 分岐 (~7756-7779)

`drag.finished` で `clip_to_rect` + `rects_intersect` (~7759-7767 再利用) から
`inside: Vec<ClipKey>` を作り:

- `shift` → prev 順を保つ UNION (lasso ~7427-7433 と同様)
- `ctrl` → XOR (~7436-7443 と同様)
- それ以外 → REPLACE = inside

`prev != next` ガード + `SelectClips { prev, next }` を維持。`prev = selected_clips.to_vec()`。
修飾キーの取得元は `take_drag_rect_in_rect` の `DragRect.modifiers` (~7756 で読む)。
(automation lasso は自前の `AutomationLassoSession.start_modifiers` ~5462 を使うが、それとは
別機構。混同しない。)

### A-3. arrangement 二重 emit 抑制 (必須)

daw_01 は `next` のみを full-replace で消費する (arrangement_view.rs ~1132 SetClipSelection)。
空き release の既存 clear (~7640-7656) と新 marquee commit が**同一フレームで両方** Edit::mutate
を push すると undo が二重 push される。対策: marquee ブロックを clear より**上に移動**して
`marquee_committed: bool` を立てる、または clear を drag_rect の `DragRectState`
(active/just-finished) で guard する。`!shift` 項を保持し Shift+空き短クリックは union no-op
(lasso ~7414-7420 をミラー)。純 sub-4px 無修飾 press は marquee の zero-rect REPLACE で clear。

### A-4. piano_roll marquee 起動ゲート (置換 ~2395-2399)

```
marquee_press = primary_just_pressed
    && pointer.pos が `grid` 内
    && !modifiers.alt
    && note_hit(notes, view, grid, px, py, style.resize_handle_px).is_none()
    && note_drag が press 時点で None
```

- `!editing_mode` と `|| shift_rect_active` は保持。
- `note_hit().is_none()` は load-bearing (note MOVE は !shift でゲート ~1467)。

### A-5. piano_roll commit 分岐 (~2402-2423)

- REPLACE は **空 set から** inside を作る (`selected.iter()` から始めない)
- `shift` → UNION、`ctrl` → XOR
- `prev != next` 比較の前に `sort_unstable`
- `Select { prev, next }` を維持

### A-6. piano_roll 二重 emit 抑制 (最難関)

空き clear は ~2219 (pending_click emit) で **marquee ブロック (~2380) より先に消費** される
ため、marquee からの前方 bool では 2219 に届かない。対策: **pending_click の計算地点
(~1897-1912) で抑制**する。`wid.child(b"rect_select")` の `DragRectState` を読み、
press 時に rect-select drag が active、またはこの release フレームで finishing なら
`pending_click = None` にする。`piano_roll_response_clears_selection_on_empty_click`
(~3772、同一フレーム press+release at empty) が **ちょうど 1 回** `Select{next:[]}` を
marquee zero-rect REPLACE 経路で emit することを確認。

### 影響なし (検証済)

- `ArrangementEditRequest` / `PianoRollEditRequest::Select` は `#[derive(Debug)]` の
  per-frame transient ADT (arrangement.rs ~563)。IPC/bincode なし ⇒ protocol 理由の
  `cargo build --workspace` 不要、daw_audio.exe 再ビルド不要、RT-audio 非接触。
- `response.rect_select_active` は daw_gui/src に live consumer 無し (grep 済) なので意味を
  変えても安全。
- `Modifiers` は ctrl/shift/alt/logo の **4 フィールド** (event.rs ~43-49)。

### テスト (gui_01)

各 widget で: plain-drag-empty→REPLACE / Shift→UNION / Ctrl→XOR / plain-drag-on-clip(note)→
MOVE で Select 無し / sub-4px 無修飾 empty press → **ちょうど 1 回** `Select{next:[]}`
(二重 emit ガードを固定)。`piano_roll_shift_drag_is_additive` (~4088) は維持。
doc-comment 更新: piano_roll.rs ~1346-1349 (「drag<4px」維持、Shift 行を plain=REPLACE/
Shift=UNION/Ctrl=XOR に書換)、~1462-1463、arrangement ~7714-7726 (press_in_automation_lane の
zone 除外注記は残す)。

## (B) daw_01 側配線

**ほぼなし。** `SetClipSelection` / `SetNoteSelection` が `next` を full-replace で受ける
ことを確認するのみ (arrangement_view.rs ~1132 / piano_roll_view.rs ~238、現状そのまま)。

## エッジケース

- Shift+空きドラッグ: union。Shift+空き短クリック (sub-4px): union no-op (選択維持)。
- Ctrl+空きドラッグ: XOR。
- 同一フレーム press+release (zero-rect): REPLACE で `Select{next:[]}` を 1 回。
- ruler/track header/automation lane header での drag: 対象外 (ゲートで除外)。

## ビルド/検証

- gui_01 (landing 後): `cargo test -p daw-ui` (新テスト群)、`cargo clippy -p daw-ui -- -D warnings`。
- daw_01: `cargo build --workspace` + `cargo clippy --workspace -- -D warnings`。
- **実機 smoke 必須** (unit で拾えない操作変更): 空きドラッグで矩形選択、Shift/Ctrl で
  追加/トグル、クリップ/ノート上ドラッグは移動のまま。二重起動チェックを守る。

## 待機中の進め方

daw_01 側は配線ゼロ。要望提出後 landing を待ち、landing 後に実機 smoke のみ。
non-exhaustive match 等の diagnostic が出たら通知を待たず確認。
