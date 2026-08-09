# クリップの内容ウィンドウ (r.md #44)

リンクしたクリップ (= `content_id` 共有) でも **開始・終了は clip ごとに独立** という
`plan_clip_share_clone.md` §2.1 の仕様
(「共有可能な clip 内容 … length は持たない (各 clip 側の length_beats が表示範囲を決める)」)
を、モデルと再生の両側で成立させる。

## 1. いまの壊れ方

| # | 事象 | 場所 |
|---|---|---|
| A | audio clip の端 trim が **共有 content を直接書き換える** (`source_start/end_frames` / `event_length_beats`)。 リンク相手の波形と再生範囲まで変わる | `daw_gui/src/handler/clips.rs` `resize_clip` の trim 経路 |
| B | audio engine が **clip 窓で gate していない**。 鳴る範囲は content の event 長だけで決まり、`clip.length_beats` は無視される | `daw_audio/src/audio_clip_renderer.rs` `compile` / `render_audio_events` |
| C | 左端 trim が content を **絶対時間に留めない** (MIDI は clip ごと右へずれる)。 audio だけ「source を head-chop する」補償で辻褄を合わせていた | 同 `resize_clip` / `trim_audio_event` |

A + B の合成で「リンクしたクリップの開始・終了が共有されている」ように見える。
MIDI / 映像 / 画像 / 字幕は clip 窓で gate 済みなので A の影響を受けない。

## 2. 最終形 — clip は content への「窓」

REAPER の item (`POSITION` / `LENGTH` / `SOFFS`)、Ableton の clip start/end marker と同じモデル。

```
content (共有・不変)     |----[a]----[b]----[c]----|      content-local 拍
clip 1  window                |=========|                  offset=2, length=5
clip 2  window                          |=======|          offset=7, length=4
song                    ---------------------------------> song-absolute 拍
```

- `Clip.content_offset_beats` (新規、既定 0) = **clip の左端が content のどの拍に当たるか**。
- `Clip::content_origin_beat()` = `start_beat - content_offset_beats`
  = content-local 拍 0 が置かれる song-absolute 拍。**content ↔ song の唯一の換算口**。
- clip が見せる content の窓 = `[content_offset_beats, content_offset_beats + length_beats)`。
- **trim は clip 側 3 フィールド (`start_beat` / `length_beats` / `content_offset_beats`) だけを
  書き換える。content には一切触れない。**
  - 右端 trim: `length_beats` のみ
  - 左端 trim: `start_beat += δ`, `length_beats -= δ`, `content_offset_beats += δ`
  - 左端を外へ伸ばすと `content_offset_beats` は負になり得る (= 先頭に無音/空白が付く)。
    content を壊さないので、伸ばし直せば元の中身がそのまま復帰する。
- `AutomationClip` も同一フィールドを持つ (`Clip` と同形なので対称に扱う)。

Stretch (Shift+端 drag) は content を書き換える操作なので従来どおり共有時は fork する。

## 3. 再生・描画の gate

| 対象 | いま | あるべき |
|---|---|---|
| MIDI (sequencer) | `note.start_beat ∈ [0, length)` | `∈ [offset, offset+length)`、絶対位置は `content_origin_beat() + note.start_beat` |
| audio (clip renderer) | gate 無し | event の**時間写像は据え置き**のまま、出力範囲を clip 窓と交差させる (`gate_start_beat` / `gate_end_beat`) |
| 映像 / 画像 / 字幕 | clip 範囲で gate 済 | `clip_local = playhead - start_beat + offset` に変更するだけ |
| automation | `local = song_beat - clip.start_beat` | `song_beat_to_content(song_beat)` |

audio を「source 窓を切り詰める」のではなく「出力範囲を交差させる」形にするのが要点。
warp marker / slice onset / reverse / spectral stretch はすべて event 内部の写像に依存するので、
`source_start_frames` を動かすと壊れる (いまの `trim_audio_event` が抱えていた本質的な問題)。

## 4. 波形・プレビューの座標系

widget (`daw_gui/src/widgets/arrangement`) は **clip 窓ローカル**だけを知る。
view 組み立て側が `event.event_start_in_clip_beats - content_offset_beats` を渡すので、
widget 側の x 写像 (`clip_len_beats` で割る 1 本) は変更不要。fade の
`EventFade::start_in_clip_beats` も同じ変換を通す。

piano roll は従来どおり content 全体を表示する (REAPER の MIDI editor と同じ)。
song-absolute 化は `clip.start_beat` ではなく `content_origin_beat()` を使う。

## 5. split の扱い

split は **content を 2 つに焼き直す破壊的操作** なので、窓モデルでも従来どおり両断片を
fork する (跨ぐ note の「前半は歌詞あり / 後半は継続で歌詞なし」という VOICEVOX 向けの
分割規則は、窓 gate では表現できない — 窓は発音開始が窓内の note しか鳴らさないため、
跨ぐ note が後半から消えてしまう)。

窓の導入で変わるのは **切る位置の座標系** だけ:

- 切断位置は clip-local ではなく **content-local** (`content_offset_beats + (beat - start)`)
- 左断片: `content_id` を fork した前半へ、`content_offset_beats` は据え置き
- 右断片: `content_offset_beats = 0.0` (content 側が切断位置ぶん左シフト済のため)

`Song::split_clips_at` (セクション境界) / `AppData::split_clip_at_beat` (E キー) の
両方に同じ規律を当てる。

## 5.1 glue の扱い

glue は複数 clip を 1 つの新 content へ焼き込む。窓の導入で、**窓の外の note / event を
含めてはいけない**(左端 trim で隠した中身が結合で復活してしまう)。
`crop_audio_event` / `crop_video_event` で窓と交差する部分だけを切り出し、
combined-local への写像も `content_origin_beat()` 基準にする。

## 6. 永続化

- `CURRENT_VERSION` 31 → 32。`content_offset_beats` は `#[serde(default)]` = 0 なので
  v31 以前はそのまま 0 で読める (= 既存 project の見た目は不変)。
- bincode derive のフィールドが増えるので **`make build` で 3 exe すべて再生成**が必要
  (`feedback_workspace_build_for_protocol_changes`)。`common/build.rs` の `WIRE_SOURCES` は
  `model/content.rs` / `model/automation.rs` を既に含むので fingerprint は自動で変わる。

## 7. 受け入れ基準

1. リンクした audio clip の片方を右端 trim → 相方の波形・再生範囲が変わらない
2. リンクした audio clip の片方を左端 trim → 相方が変わらず、自分は content が絶対時間に留まる
3. 左端を戻すと trim で隠れた中身がそのまま復帰する (content が破壊されていない)
4. MIDI clip の左端 trim でノートが右へずれない (隠れるだけ)
5. audio clip の `length_beats` を縮めると **音も**そこで切れる (いままで切れなかった)
6. v31 project を読み込んでも見た目・音が変わらない (`content_offset_beats = 0`)
