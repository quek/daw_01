# r.md #51 — 録音とトランスポートの一本化

## 症状 (r.md #51)

Rec ボタンだけで録音を開始すると、GUI が「再生中」だと思っていない。そのため録音中なのに
プレイヘッドが動かない / オートメーションが記録されない / 曲末に達しても自動で止まらない。

## 真因 — 「走っているか」の所有者が居ない

`transport.is_playing = true` の代入は `handler/transport.rs::play()` の 1 箇所しかなく、
`handler/midi.rs::start_recording()` は `AudioCommand::Play` を送るのに何も立てない。逆に
`stop_recording()` は `AudioCommand::Stop` を **一切送らない**。つまり開始の口が 2 本・停止の口が
2 本あり、それぞれ相手の仕事をしない。

さらに根が深いのは、**engine の再生状態が GUI へ一度も返っていない**ことだった:

- `common/src/audio_bridge.rs` に `playing` フィールドが無く、`AudioEvent` にも該当 variant が無い。
- そのため GUI の `is_playing` は「Play を送った記憶」でしかなく、engine が曲末で自己停止しても
  (`daw_audio/src/engine.rs` の `song_ended` 判定) GUI は知らない。
- その代償として **曲末判定が GUI 側にも複製**されている (`handler/tick.rs` が `song_ended` を再計算)。
  述語がずれた瞬間に「音は止まったのに録音だけ続く」になる。

## 理想 (この plan が実装する形)

**「トランスポートが走っているか」の所有者は daw_audio の engine ただ 1 つ。GUI は観測して表示する。**

- engine が毎 buffer `playing` / `recording_live` を shmem に publish し、GUI は 30Hz poll で観測する
  (playhead / peak / preroll と同じ面)。`transport.is_playing` は **観測値のミラー** になり、
  writer は Tick ハンドラ 1 箇所だけになる。
- トランスポートを動かす口は `play()` / `stop()` の 2 つだけ。録音はその上に乗るモードで、
  独自に `AudioCommand::Play` を送らない。
- 曲末の停止判定は engine だけが持つ (GUI 側の複製を撤去)。engine が止めたら GUI は観測して追従する。
- **count-in 明けの「録音実体の開始」も engine が決める**。GUI は「録音したい」という意思
  (`recording.requested`) だけを持ち、実際に書いてよいか (`recording.live`) は engine の観測値。
  旧実装は preroll ミラーの `0` で判定していたが、`0` が「まだ始まっていない」と「終わった」の
  両方を意味するため、押した直後の stale な Tick で **count-in を丸ごと飛ばす**穴があった。

### 状態の所有者

| 事実 | 所有者 | GUI での姿 |
|---|---|---|
| transport が走っているか | daw_audio engine | `transport.is_playing` (観測ミラー) |
| count-in 残り | daw_audio engine | `transport.preroll_remaining` (観測ミラー) |
| ノートを書いてよいか | daw_audio engine | `recording.live` (観測ミラー) |
| 録音したいか (Rec 点灯) | daw_gui | `recording.requested` |
| どこへ戻って止まるか | daw_gui | `transport.playback_origin_beat` |

## 決定した挙動 (ユーザー確認済み)

| 場面 | 決定 | 根拠 |
|---|---|---|
| 停止中に Rec | 再生も同時に開始し、プレイヘッドが進む | REAPER §2.4 / Cubase「Recording starts from the current cursor position」 |
| 録音中に曲末へ到達 | **止まらず走り続ける** (曲の後ろに録れる) | 参照 5 製品とも曲末で録音を止めない |
| 録音中に Rec 再押下 | 録音だけ終了し再生は続く (パンチアウト) | Cubase「To stop recording and continue playback, click Record」 |
| 録音中に停止 | 録音も再生も終了、プレイヘッドは録音開始位置へ | 既存の r.md #50 停止ホーム契約と同じ |
| 再生中に Rec (パンチイン) | count-in なしで即録音、曲は途切れない | Cubase: count-in は「停止状態から録音を始めたとき」の機能 |
| count-in 中に停止 | その場で取り消し、何も録音せず完全停止 | — |
| 録音中にプラグイン追加 | トランスポートを止めない (テイクを切らない) | REAPER: 走行中の arm 追加を明示的に許可 |
| 録音待機トラックが 0 本 | 警告して録音を始めない | トラック状態を勝手に変えない |
| 弾いた音のモニター | 録音待機トラックは transport 状態に関係なく常に鳴る | 一般的なインプットモニター |

## 変更

### プロトコル / shmem

- `AudioBridge` に `playing` / `recording_live` を追加 (`u32` の 0/1、既存 peak と同じ atomics 面)。
  `audio_bridge.rs` は `common/build.rs` の `WIRE_SOURCES` に入っているので fingerprint が変わる
  = 3 プロセスを揃えて `make build` が必須。
- `AudioCommand::StartCountIn { samples }` を廃止し、
  `StartRecording { preroll_samples }` / `StopRecording` に置き換える。
  「録音中か」は engine 側の曲末 auto-stop 抑止にも要るので、count-in ではなく
  **録音そのもの**を運ぶ名前にする (`samples: 0` を cancel の合図に使う旧 idiom も消える)。

### daw_audio

- `EngineShared` に `recording_requested`。`StartRecording` で preroll と一緒に立て、`StopRecording` で落とす。
- `process_buffer` 末尾で `bridge.set_playing(self.playing)` /
  `set_recording_live(recording_requested && playing && preroll == 0)`。
- 曲末 auto-stop (`reached_end` && loop 無し) は `recording_requested` の間は発火しない。
- count-in ブロックの早期 return より **前** に Play/Stop edge と pending_seek を消費する
  (旧実装は count-in 中に Stop が届かず、取り消したのに本再生が始まる / 明けに録音が
  勝手に ON になる、という 2 つのバグの原因だった)。

### daw_gui

- `AppEvent::Tick` に `playing` / `recording_live` を追加。`on_tick` が
  `transport.is_playing` を書く **唯一の場所**になる。
- `play()` は `PlayOutcome`(Started / Queued / Refused) を返す。preroll 付きで呼べる内部
  `start_transport(preroll_samples)` を持ち、count-in の `StartRecording` と `Play` の順序を保証する。
- `stop()` は「止めてくれ」の要求 + 録音セッションのクローズ。`is_playing` は書かない (観測に任せる)。
- 観測した `playing` の true→false エッジで `on_transport_stopped()`:
  プレイヘッドを開始位置へ戻す / 録音セッションを閉じる / latched gesture を clear。
  手動停止・曲末・書き出し・パニック・子プロセス crash が全部ここへ収束する。
- 録音セッション:
  - `start_recording()`: armed 検査 → 停止中なら `start_transport(preroll)`、再生中ならそのまま
    (パンチイン、count-in なし) → `requested = true` + `StartRecording` 送信 + undo gesture を開く。
  - `close_recording_session()`: 押しっぱなしノートの長さを確定 → metronome 復帰 →
    `StopRecording` 送信 → undo gesture を閉じる。パンチアウトと停止の両方から呼ぶ (冪等)。
- モニター: `handle_midi_note_on/off` が armed track へ `PreviewNoteOn/Off` を送る
  (transport 状態に依存しない)。停止・disarm・パニックで held を off する。
- 録音ノートの note_off は **`note_id` で確定**する (安定 id = 不変条件 1)。旧実装は
  `start_beat` と pitch の値照合で再検索していたため、同位置に同ピッチが 2 本あると
  常に 1 本目に当たっていた。`docs/plan_fixme_83_note_overlap.md` の「録音 overdub は
  重なり解消の対象外」という決定はそのまま維持する (重なりを消すのではなく、
  **どのノートの note_off か**を id で確定させるのが正しい直し方)。
- undo 粒度: 録音 take 全体を `song_doc.begin_gesture()` / `end_gesture()` で bracket し、
  1 テイク = Ctrl+Z 1 回にする。

## 検証

- `daw_gui/tests/record_transport.rs` — GUI 側の状態遷移 (headless、決定的)。
  `AppEvent::Tick` を組み立てて engine の応答を模し、Rec 開始 / パンチイン /
  パンチアウト / 停止の観測 / count-in 中の入力破棄 / note_id 確定 / モニターを見る。
- `daw_audio/src/engine.rs` の `reached_transport_end` 単体テスト — 録音中の曲末抑止と
  ループ wrap の両立。
- `daw_gui/tests/scripts/rec_transport_engine.js` — **engine 側の契約を実プロセスで**
  確認する手動スクリプト (`daw_gui --script`、要オーディオデバイス)。
  Play/Stop の観測・count-in 明けの `recording_live` 立ち上がり・count-in 取り消しで
  曲が鳴り出さないこと・パンチアウトで止まらないことを実際の 3 プロセスで見る。
  `make test` には**入れない** — 実デバイス + 時間ベースの assertion なので、
  負荷で落ちる flake になる (既存の quiesce テストと同じ轍)。

## 撤去されるもの

- `handler/tick.rs` の `song_ended` 再計算 (engine と二重実装)。
- `handler/activity.rs::transport_rolling()` の「`is_playing` は Rec 単独で立たないから
  録音フラグも見る」という補償 (observed `is_playing` が録音中も真になるので不要)。
- `AudioCommand::SetAppActive` / `engine.rs::buffer_is_idle` のドキュメントにある
  「`transport.is_playing` を停止判定に使ってはならない」という但し書き (前提が消える)。
- `app.rs` の `preroll == 0` で `midi_recording_pending → midi_recording` に昇格する分岐。
