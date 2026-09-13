# Glue (`J`) = 焼き込み

## 1. 何が問題だったか

`J` は「選択範囲の clip を 1 つにまとめる」操作だが、旧実装は **音声を焼かず**、
元 clip の `AudioEvent` を新しい content に**並べて入れるだけ**だった。結果、結合後の
1 クリップの中に event が N 個残り、アレンジは event ごとにフェードハンドルを描くので
(`ClipContent::event_fades()` → `ClipView::fades` → `draw_fade_handle_overlay`)、
**結合前の各クリップの継ぎ目のハンドルがそのまま見えていた**。

`split_clip_at_beat` の doc が謳う「分割してから結合し直せば元に戻る」も、実際には
event が 2 つ残るので戻っていなかった。

## 2. 決めたこと

**audio の Glue は Reaper / Live 流の焼き込みにする。** 選択範囲をオフラインで
1 本の WAV へレンダリングし、範囲全体を覆う **1 clip / 1 event** に置き換える。
継ぎ目のフェードは音ごと焼き込まれて消える。

| kind | `J` の動作 |
|---|---|
| **Audio** | 選択範囲を pre-FX でレンダリング → 1 clip / 1 event へ置換 (**焼き込み**) |
| MIDI / 歌唱 | 従来どおり note を非破壊で merge (WAV に焼くと note 編集が消える) |
| Video / Image / Text | 従来どおり event を非破壊で merge (音声ではないので焼けない) |

Bounce In Place との役割の重なりは許容する — Bounce は 1 クリップ、Glue は
**範囲 × レーン**が単位で、隙間と複数クリップを 1 本に畳む。

## 3. 何を焼くか (= pre-FX)

`Song::isolated_track(track_id)` が組む「そのトラックだけ」の song を engine に `LoadSong`
してから、`BounceClipFxOnline { scope: RenderScope::Sources }` で範囲を render する。
**Song は書き換えず、どの段を通すかは scope が compile で program の形に焼く**
(Song を書き換えて処理を消すと、音源を含む Parallel などが正確に焼けない)。焼くのは**素材の音だけ**:

- 音声入力を持つ device と内蔵 device は op を出さない (latency も数えない)。Parallel は
  素材の音だけを描く形 (分割と chain の gain / pan / mute / solo・出力 trim を通さない) にする
- **トラックのフェーダーの段 (volume / pan / mute / solo) を通さない** (`mixer::pass_strip`) —
  オートメーションレーンと `mod_routings` (LFO 等の変調) も段ごと通らない。焼き込むと再生時に
  同じものがもう一度掛かって二重に効く (pan 則は中央でも -3dB)
- master の段 (fx chain / 音量 / Limiter) を通さない
- 他のトラックを落とし、それを指す参照 (send / SC / パラアウト / follower / レーン / 変調) は
  編集後の不変条件と同じ掃除 (`Song::prune_dangling_refs`) が外す。自トラックを読む配線は残る
- **ランチャーの主導権をアレンジへ戻す** (`RowPlayback::Arranger`) — 行がランチャー側の
  ままだと offline 走査はセルの音を再現し、アレンジのクリップが鳴らない
- clip / event の mute は**そのまま効く** (= 聞こえている音を焼く)

Bounce (with FX) は同じ isolate で `RenderScope::PostFx` (device チェーンまで) を焼き、
フェーダーから先は新しいトラックへ写す。規則の正本は `common/src/model/bounce_ops.rs`。

走査は cold (`RenderSpan::RangeCold`) = 「範囲の頭で再生を押した音」。plugin を通さない
ので tail を積み上げる意味がなく、トラック数ぶん曲頭から空走査するのを避ける。

焼いた WAV の登録と、それを指す event の組み立ては `Song::add_baked_audio` が SSoT
(bounce と共通)。**source 範囲は書き出し窓ちょうどに切り、`StretchMode` は `Raw`** —
理由は同関数の doc。

## 4. 手順 (1 undo step / 全か無か)

1. `action_glue_selected_clips` — 選択範囲 × レーンから**トラックごとの kind** を判定
   (song は変更しない)。audio のトラックだけ render job を作る。
2. job を **1 本ずつ順に** engine へ (`LoadSong(isolated)` → `SetRenderMode(Offline)` →
   `BounceClipFxOnline { warm: false }`)。完了通知ごとに次の job を撃つ。
3. 全 job 完了 → **1 回の `edit_song`** で
   境界 split → 範囲内 clip の除去 → 焼いた WAV を指す 1 clip の配置 → 非 audio トラックの
   従来 merge、をまとめて行う。したがって `J` は **1 undo step**。
   非同期の完了から走るので、undo bracket は `enter_own_gesture` で退避して戻す
   (進行中のユーザーのドラッグを横取りしない)。
4. 途中で失敗したら**何も変更しない** (出力ファイルは削除、engine の song を復元)。
   **境界 split も「結合するトラック」だけに掛ける** — 混在で断ったトラックや render に
   失敗したトラックまで切ると「何も結合していないのにクリップだけ切り刻まれる」。

audio トラックが 1 つも無い選択 (MIDI だけ等) は render を挟まず同期で完了する。

**焼き込み中は engine を占有している** (`offline_render_busy`)。この間は編集 /
再生 / 別の走査 (書き出し・ラウドネス解析) を止める — 編集を通すと frame flush の
`LoadSong(full song)` が render 中の isolated song を差し替え、焼く対象が途中で変わる。
プロジェクト差し替え (`reset_song_scoped_state`) と daw_audio 切断
(`abort_inflight_renders_on_disconnect`) では待ちを畳む (前の曲の範囲を新しい曲へ
適用しない / 永久に「焼き込み中」で固まらない)。

出力 WAV の名前は `bounce_output_path` が**空ファイルを作って予約する**。名前が
「クリップ名 + ミリ秒」だけだと、同名トラックを同じミリ秒で採番して同じファイルを掴み、
後の render が前を上書きして**別トラックの音が鳴る**。

## 5. 不変条件との関係

- **live と export は同じ render 関数** — 焼き込みは engine の `run_export`
  (= `render_master_buffer`) を通る。GUI 側に第 2 の audio renderer を作らない。
- **安定 id addressing** — render 中の編集で index はずれるので、適用は
  `track_id` / `clip_id` と選択範囲 (拍) で再解決する。対象が消えていたらそのトラックは
  skip する。
- **Song 編集の副作用は単一の口** — 適用は `edit_song` 1 回。
