# plan: 複数選択クリップのインスペクタ一括編集 (FIXME #46)

## ゴール

複数のクリップを選択した状態で、インスペクタの項目 (影/色/位置/不透明度/フォント等) を
編集すると **選択中の全クリップにまとめて適用** できるようにする。値が割れている項目は
「—」(mixed) で表示し、編集で全選択へ broadcast する。歌詞クリップの影の一括指定が主目的だが、
全クリップ種別・全項目を対象にする。

## 確定設計 (インタビュー済、再議論しない)

- インスペクタの編集対象は **`selected_clips` 全体** (アンカー 1 個だけではない)。
- 値が揃っていればその値、割れていれば **「—」** を表示。編集で全選択へ broadcast。
- 種類混在時は **その項目を持つクリップだけに適用** (影→テキストクリップ全部、
  共通項目 X/Y/W/H/opacity/rotation/fade→テキスト+画像、持たないクリップは無視)。
- **全項目**が一括対象 (一部に限定しない)。
- mixed 項目を scrub すると **全クリップが 1 つの絶対値にスナップ** する (scrubable_number は
  絶対値を emit)。影/色ではこれが正しい。X/Y では全クリップが同座標に揃う。
- 既存の `scrubable_number` / `mutate_*_events_in_clip` / inspector-scrub の undo bracket
  idiom を再利用。新規 edit-buffer は作らない。

## 確定アーキテクチャ (検証済の核)

broadcast は **インスペクタの view クロージャ側** に置く。handler 側に置いてはいけない:
`runner.rs` (~472-538) は `SetClipImageX/Y/W/H`・`SetClipImageRotation`・
`SetClipTextX/Y/W/H`・`SetClipTextRotation` を **1 個の drag.target** で発火するので、
handler 側で broadcast するとキャンバス上で全選択クリップが一緒にドラッグされてしまう。
`mutate_audio/image/text_events_in_clip` は誤った `ClipContent` variant で `false` を返す
(app.rs ~2289/~12167/~12248) ので、盲目ループでも variant-safe (役割判定不要)。

## (A) gui_01 要望仕様 (「—」表示のための placeholder)

> 提出先: `docs/gui_01_conversation.md` に `[要望]` として追記。
> 関連仕様: `docs/plan_batch_inspector.md` を必須で含める。

`scrubable_number_at` に `placeholder: Option<&str>` 引数を追加する (末尾、非破壊)。

- placeholder が `Some(s)` かつ **idle** (`!was_editing && drag_anchor.is_none()`) のときだけ、
  `format_value(displayed_value, ...)` の代わりに `s` を描画する (mixed 表示「—」用)。
- ユーザーが編集を開始 (短クリック) したら、内側 `text_input` は `format_value(value, ...)`
  (scrubable_number.rs ~360) から seed する (= 渡された base `value` から編集開始)。よって
  placeholder は編集中は抑制。
- `placeholder.is_some()` (+ 文字列の hash) を cached node の `input_hash`
  (scrubable_number.rs ~410-420) に **fold** する。選択変更で「—」⇔数値が切り替わるときに
  stale が残らないように。
- **全 call site を `None` に更新**: track_inspector.rs `scrub_field` (~93)、Group Transform の
  直接呼び出し (~1084)、gui_01 の examples、テストハーネス `run_frame` (scrubable_number.rs
  ~510-519)。

## (B) daw_01 側配線

### B-1. 編集対象 SSoT: `inspector_target_refs()`

アンカーは構築上 `selected_clips` の末尾にいる (select_clip/set_clip_selection app.rs
~10901/~10919) ので「アンカー補完/dedup」は dead code。最小実装:

```rust
fn inspector_target_refs(&self) -> Vec<ClipRef> {
    let mut refs = self.selected_clip_refs();          // app.rs ~10873-10878
    if refs.is_empty() { refs.extend(self.selected_clip_ref()); }
    refs
}
```

### B-2. `scrub_field` の本体変更 (シグネチャだけでなく body)

`value: f64` → `Option<f64>` (mixed = None 表示用、placeholder へ渡す)。on_change で
targets をループ:

```rust
let targets = app.inspector_target_refs();
for t in &targets { app.handle_event(make_event_for(t, v)); }
```

`make_event` は現状 single target を焼き込んでいる (track_inspector.rs ~648/689/730/1378)
ので、target を引数に取る形へリファクタ: `impl Fn(ClipRef, f64) -> AppEvent`。
`is_image_clip` で分岐する event (SetClipMuted/Fade beats) は誤 undo を避けるため対象 kind を
除外。scrubable 系は variant-self-filter で無害。

### B-3. undo は Batch イベント方式 (重要)

**discrete な undoable イベントをループ handle_event してはいけない**
(SetClipReversed/Muted/StretchMode/TextMuted/TextAlign/Fade*Curve — すべて is_undoable
app.rs ~2485-2533、~4305-4318 で auto-push)。ループすると N 個のスナップが積まれる。
既存の Batch idiom (SetClipGainDbBatch/SetClipFadeBeatsBatch/SetClipFadeCurveBatch
~2545-2547) に倣い、`Vec<ClipRef>` を持つ `SetClip*Batch` イベントを追加し is_undoable に
入れて **1 スナップで全 batch をカバー**、handler 内部でループする。

scrubable な数値項目 (Gain/Pan/Pitch/Fade beats/全 Text num/Image X..Rotation — is_undoable
ではない) は、`scrub_field` の Begin/EndInspectorScrub bracket (単一 InspectorScrubField で
keyed、~1327) が drag stroke 全体で 1 undo step を与えるので、on_change 内で target ごとに
handle_event をループしてよい。

### B-4. mixed-kind の resync thrash 防止

現状は 1 section だけ描画される (inspector_audio/image/text_event_summary() が **アンカー**が
その variant でなければ None を返す app.rs ~2092/~2168/~2532)。summary を「選択に 1 つでも
その kind があれば Some」に変えると audio+image+text 選択で 3 section 全部が描画され、各 section
が毎フレーム `ResyncClip*EditBuffers` (track_inspector.rs ~274/~571/~1152) を `summary.target`
へ push して `clip_edit_buffer_target` が ping-pong → Text content/font buffer が使用不能に。

**対策**: 3 つの `ResyncClip*EditBuffers` push を **アンカーの content kind に一致する section
だけ** が push するよう gate (例: `if app.is_text_clip(anchor) { push ResyncClipTextEditBuffers }`)。
multi-section 描画 + anchor-gated resync を採る。

### B-5. mixed 検出と summary 構造

各 summary builder が「その kind を持つ選択クリップ全部」を畳んで field ごとに
「全部同じ値 → その値 / 割れている → None (mixed)」を出す。

- `InspectorAudioEventSummary` / `InspectorImageEventSummary` は Copy 派生 (app.rs ~214/~245)。
  field を `Option<T>` にしても Copy は維持 (Option<f32> は Copy)。
- `InspectorTextEventSummary` は Clone のみ (HashSet+TextEvent、~375)。mixed 状態は既存 struct
  内で表現し、Copy を要求しない。
- audio の代表 event: 非アンカークリップは event index 0、アンカーは audio_editor 選択 event
  (~2102-2107)。1-event クリップでは無関係。
- automation の 'A' トグルと Add/Remove*AutomationLane は **アンカートラック限定** のまま
  (per-track。選択ループに含めない)。

### B-6. f32 == 畳み込みの注意

clamp/wrap により、直後に batch したばかりの field が clip ごとに違って見えることがある
(fade beats は per-clip length で clamp app.rs ~12076/~12339、rotation rem_euclid ~12204、
X/Y/W/H は 0..1 clamp)。許容するが notes に明記。位置/サイズは clamp が同一なので綺麗に畳まれる。

## エッジケース

- 単一選択 (selected_clips に 1 個): 従来どおり 1 クリップ編集 (broadcast 先が 1 個)。
- 種類混在で共通でない項目 (影): テキストクリップだけに適用、画像/オーディオは無視。
- mixed scrub: 全クリップが 1 絶対値にスナップ (確定設計)。
- fade beats を長さの違う複数クリップに batch → clamp 差で再度 mixed 表示になり得る (許容)。

## ビルド/検証

- **`cargo build --workspace` 必須** (gui_01 path-dep の widget シグネチャ変更で daw_gui
  再コンパイル)。ただし bincode 変更ではないので daw_audio decode には無影響。
- `cargo clippy --workspace -- -D warnings`、`cargo test -p daw_gui` (mixed 畳み込み/broadcast)。
- 実機: 複数歌詞クリップを選択 → 影オフセット等を編集 → 全クリップに反映、割れている項目が
  「—」表示になることを目視。
- `/review` を commit 前に実行。commit 後 `cargo build --workspace --release` green 確認。

## 待機中の進め方

daw_01 側の batch イベント / broadcast / inspector_target_refs / resync-gating / mixed 検出は
**gui_01 placeholder landing 前に実装可能**。「—」描画 (placeholder=Some("—") を渡す) のみ
landing 後に wire。それまで mixed 項目はアンカー値表示にフォールバック (parked)。
non-exhaustive match (scrubable_number_at の arity 変化) が出たら通知を待たず wire。
