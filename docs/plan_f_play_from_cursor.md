# plan: f キーでカーソル位置へ snap してプレイヘッド移動+再生 (FIXME #44)

## ゴール

`f` キーで、マウスカーソル直下の拍に現在のスナップで吸着してプレイヘッドを移動し、その位置から
再生する。アレンジビューとピアノロールの両方で、カーソルがその面にあるとき有効。

## 確定設計 (インタビュー済、再議論しない)

- 有効面: **アレンジビュー + ピアノロール grid** (カーソルがその面にあるとき)。
- 位置は **song 全体の beat** に換算 (ピアノロールは clip_start_beat を足す)。
- snap は **現在のスナップ設定に従う** (G トグル on/off + Alt 一時解除を尊重)。
- 再生中: **シームレス seek して継続** (停止しない)。停止中: その位置から再生開始。
- カーソルがどの grid 上にもないとき: **無反応** (no-op、Play も seek もしない)。
- daw_01 のみ (gui_01 変更なし)。

## (B) daw_01 実装 (本件は daw_01 のみ)

### B-1. piano roll の snap は song-absolute 空間で行う (重要・バグ回避)

既存のピアノロールは **song-absolute 空間で snap してから clip_start_beat を引く**
(piano_roll_view.rs ~320-324)、AddNote も同様 (~338-341)。`snap_beat` は grid 線を song
beat 0 起点に置くので、clip_start_beat が snap unit の倍数でないとき **clip-local snap は
song-absolute snap と食い違う**。f のプレイヘッドは song-absolute なので song-absolute grid で
snap する必要がある。

実装: piano_roll_view.rs ~315-325 で `beat_raw` (~320 の `- clip_start_beat` する**前**の値)
を新フィールド `pianoroll_hover_beat_song_raw` にミラーする。dispatch 側で直接 snap:

```rust
let song_beat = snap::piano_roll_snap_config(app)
    .snap_beat(raw_song, alt, app.pianoroll_zoom_x);   // clip_start_beat 演算なし
```

zoom は `app.pianoroll_zoom_x` を **`.max(4.0)` せず** 渡す (hover ミラー ~323 が素のフィールドを
使う。render zoom ~91 は `.max(4.0)`)。

### B-2. arrangement branch

既存の song-absolute `app.arrangement_hover_beat_raw` を読み、
`arrange_snap_config(app).snap_beat(raw_abs, alt, app.arrange_zoom_x.max(1.0))` で snap。
`.max(1.0)` は arrangement_view.rs ~367/~956 と一致させる (Adaptive grid unit が依存)。
raw/snapped hover ミラーは arrangement_view.rs ~947-998。

### B-3. hover ミラーの diff 条件を広げる

piano_roll_view.rs ~326 の diff 条件 (`if app.pianoroll_hover_beat != hover_beat`) に新 raw
フィールドの比較を **OR で追加** し、同じ Edit::mutate で両方を push。grid_rect 外では
snapped と同様 None にリセット (and_then None)。これをしないと raw フィールドが更新されない。

### B-4. `action_play_from_cursor(beat)`

```rust
self.playhead_beat = Some(beat.max(0.0) as f32);
let sr  = common::audio_bridge::SAMPLE_RATE as f64;
let bpm = self.song.bpm.max(1.0) as f64;
let samples = (beat * 60.0 / bpm * sr).max(0.0) as u64;   // stop() ~6953-6955 / SetPlayheadBeat ~1120-1122 と同一
self.send_audio(MainToChild::SeekTo { samples });
if !self.is_playing { self.play(); }   // 停止中のみ。play() を再利用
```

`play()` を**そのまま再利用**し、3 ゲート (export ~6882-6885 / asset_decode ~6889-6893 /
pending_plugin_loads ~6900-6907) と playback_origin_beat capture を継承する。
**再生中は SeekTo だけ** (play() も stop() も呼ばない)。

### B-5. AppEvent と dispatch

- `AppEvent::PlayFromCursor { beat: f64 }` (view 側で snap+ルーティング+song-absolute を解決、
  handler は set-playhead + seek/play のみ)。**is_undoable に入れない** (app.rs ~2485 whitelist、
  Play/Stop/PlayToggle も非 undoable)。
- 新 AppData フィールド + この AppEvent は GUI プロセス内のみ (IPC 非経由) ⇒ protocol 理由の
  `cargo build --workspace` 不要 (だが習慣どおりビルド + `clippy -p daw_gui -D warnings`)。
- `m.bind("daw.play_from_cursor", "F")` を shortcuts.rs ~40 以降に追加 (F は空き:
  F1=toggle_help ~32、F2=rename_clip ~83)。`dispatch_shortcuts` (root.rs ~560、引数
  `bottom_rect: Rect`) の `is_pianoroll_active`/`pointer_in_bottom` 算出後 (~668-672) に dispatch
  し、G/X/1/2/3 と同様に pianoroll vs arrangement をルーティング。Alt は
  `ui.pointer().modifiers.alt` でライブ取得。

### 検証済の健全性

seek-and-continue は健全 (engine.rs ~710-713 が毎 process_buffer で pending_seek を swap、
already-playing+Play は `_ => {}` arm で playhead は seek 値を保持、~766-775 で resync)。
ミラー読みは 1 フレーム stale (既存 E/Alt+E split と同じ、キー押下では無視できる)。

## エッジケース

- どの grid 上にもカーソルが無い: 無反応 (guard で Edit を push せず return)。
- text_input フォーカス中: gui_01 が単キーを抑制 (F は素通りしない)。
- **再生中の origin 非更新 (確定・要実機確認)**: f-while-playing は bare SeekTo のみで
  playback_origin_beat を更新しない ⇒ その後 Stop すると **元の play origin に戻る**
  (既存の ruler-click-during-play arrangement_view.rs ~1116 と同じ挙動)。停止中の f は
  play() で f の beat を origin として capture する。この非対称は許容 (実機で確認)。

## ビルド/検証

- `cargo build --workspace` + `cargo clippy -p daw_gui -- -D warnings`。
- 実機: 停止中 f → snap されたカーソル拍から再生開始。再生中 f → シームレス seek+継続
  (daw_audio ログで `received SeekTo`、is_playing が true のまま)。**clip_start_beat が grid に
  乗らないピアノロールクリップで、ruler が見せる song-absolute grid 線と同じ位置に
  プレイヘッドが乗ること** (clip-local バグの回帰確認)。G-off / Alt 押下で両面とも un-snap。
  カーソルが grid 外で無反応。
- `/review` を commit 前に実行。commit 後 `cargo build --workspace --release` green 確認。

## 待機中の進め方

gui_01 非依存なので即着手・即完了可能。
