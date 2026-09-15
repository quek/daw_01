# r.md #130 グローバルトランスポーズ

索引: [plan_rmd_130_133_index.md](plan_rmd_130_133_index.md) (分担・統合順・共通規則)。
調査: `scratchpad/rep/130-transpose.md` (コード地図) / `r130.md` (Cubase / Logic / Studio One / FL / Live / Bitwig / REAPER / Reason)。

## 理想

移調は **Song が持つ非破壊の曲パラメータ**で、ノートのデータは書き換えない。テンポと同じ扱い
(トランスポートの数字 / オートメーション / 変調 / MIDI Learn) を受ける。音程を生む全経路
(プラグインへのノート / VOICEVOX 歌唱 / オーディオクリップ / 演奏入力 / MIDI 書き出し) は
**同じ 1 つの評価関数**から移調量を読み、**発音した鍵盤を台帳に持つ**ので、途中で値が変わっても音が残らない。

## 確定仕様 (2026-09-15 ユーザー承認)

| # | 論点 | 決定 |
|---|---|---|
| Q1 | 追従する対象 | **MIDI ノート・VOICEVOX 歌声・オーディオクリップ (長さを変えず音程だけ)** の全部。トラック単位の「移調に追従しない」スイッチで外す |
| Q2 | 表示と演奏 | **移調楽器として扱う**。ピアノロールは書いた音で表示、鍵盤で C を弾くと (移調 +2 なら) D が鳴り、録音されるのは C。再生は弾いたときの音と同じになる |
| Q3 | 再生中に値が変わったとき | 鳴っているノートは**その場で止めて新しい音程で鳴らし直す** (オートメーションで途中から変わる場合も同じ) |
| Q4 | 幅 | **±24 半音**。結果が 0..=127 を外れるノートは鳴らさない |
| Q5 | UI | トランスポートの **Key の右隣**に `Transpose [+2]`。BPM と同じ操作 (縦ドラッグ 1 半音 / クリックで入力 / ダブルクリックで 0 / ◉ で変調・オートメーション対象)。**0 以外は数字に色**を付ける |
| Q6 | 追従スイッチ | **トラックヘッダの右クリックメニュー**にチェック付き「移調に追従」(既定 ON)。右クリックしたトラックが選択に含まれれば選択全体。追従しないトラックは**ヘッダの名前の横に小さな印** |
| Q7 | MIDI 書き出し (SMF) | **鳴る音**で書き出す (追従しないトラックは書いた音) |
| Q8 | バウンス (In Place / with FX) | **移調 0 で焼き**、できたオーディオは他と同じく移調に追従する。「外に出すもの = 鳴る音 / プロジェクトの中に作るもの = 書いた音」 |

### main が決めた細部 (ユーザーに報告済み)
- **グループ継承**: 実効的な追従 = 自分 && 祖先グループ全部 (`Song::track_visually_silenced` の祖先走査と同じ形)。
- **オーディオの再生方式**: Raw / Repitch / Slice でも移調は**長さと位置を変えない** (テープ式に読み速度を変えない)。
  波形描画 (`event_wave_spans`) は移調の影響を受けない (長さが変わらないので「絵と音の一致」は保たれる)。
- **ARA (Melodyne)**: プラグインがソースを読むので追従できない。そのトラックはヘッダの「追従しない」印を自動で出す。
- **ランチャーのパッド割り当て** (MIDI バインディング): 生のノート番号のまま (移調しない)。
- **Key 表示 / スケール**: 書いた音の空間のまま (表示・スナップ・スケール補正は変えない)。
- **WAV 書き出し / ラウドネス解析**: ライブと同じ `render_master_buffer` なので移調込み (不変条件 6)。
- **読み上げ (talk) のトリガー** (`sequencer.rs` の `key: 0` の合成 note_on) は移調しない。

## 設計

### model (`common`)
- `Song.transpose: i8` (±24、`#[serde(default)]`、`sanitize_ranges` で丸め)。**v41** (索引の事前割当)。
- `AutomationTarget::SongTranspose` を追加 (`SongTempo` / `SongTimeSigNumerator` の網羅 match を全部辿る。
  調査 §7 に列挙)。値は半音 (連続値を丸める)、補間はステップ相当で評価して丸める。ランチャーのセルを置けるかは
  `SongTempo` と同じ扱い。変調対象 (`song_mod_routings`)、MIDI Learn (`BindingTarget::SongTranspose`) も同形。
- `Track.follow_transpose: bool` (既定 true、serde default true)。
- **評価の SSoT**: common に 1 関数 (`transpose_at(song, beat, mod scalars) -> i32` 相当) と
  「トラックが実効的に追従するか」の 1 関数。engine / GUI / export すべてがこれを通す。

### engine (`daw_audio`)
- **発音鍵盤の台帳を SSoT にする**: `sequencer.rs:263-267` の note-off が `note.pitch` をその時点で再計算している
  既存の欠陥 (再生中にノートの音程を変えると旧鍵盤が停止まで残る) を、**送った鍵盤で消す**形に直す。
  これは移調の前提であり、既存の ↑↓ 音程変更にも効く同じ根の修正。
- note-on の鍵盤 = `pitch + 移調量` (追従トラックのみ)。0..=127 外は発音しない。
- **Q3**: buffer ごとに追従トラックの実効移調量を比べ、変わったら台帳の旧鍵盤を off → 鳴っているノートを新鍵盤で chase-on。
  RT 制約 (確保・ロック・I/O 禁止、Song を線形走査しない方針 `c619dee3`) を守る。
- 値だけの更新は `SetSongBpm` と同じ軽量 IPC (`AudioCommand::SetSongTranspose` 等) で即時反映。
- プレビュー系 (MIDI 入力モニター / 仮想鍵盤 / 鍵盤レーン / ナッジ試聴) も移調する。**GUI 側の台帳は送った鍵盤を持つ**
  (台帳に生 pitch を置いて off で移調し直すと、途中で移調が変わったとき一致しない)。
- オーディオクリップ: 追従トラックの event pitch に移調量を加える。Raw / Repitch / Slice でも長さと位置を変えない方式で鳴らす
  (必要なら移調中はスペクトル方式で処理する。1 トラックあたりの Stretch エンジン数の上限 `audio_clip_renderer.rs:609` に注意)。
- 書き出し末尾の all-notes-off / ランチャー区間の切れ目の消音は、既存どおり台帳の鍵盤で消える。

### VOICEVOX
- `collect_sing_metadata` (`handler/voicevox.rs:219-243`) で `NoteMetadata.pitch = pitch + ノート開始拍の移調量` (追従トラックのみ)。
  範囲外は歌わない (0 は休符と衝突するので 0 に丸めない)。キャッシュキーは pitch を含むので自動で再合成・元に戻せば即ヒット。
- 口パクの入力 fingerprint / query も同じ値でそろえる (開いただけで `*` にならない冪等性、r.md #9)。

### GUI
- トランスポート (`view/transport.rs::draw_tempo_and_key`、Key の右): Q5。scrub は `StreamGesture` で 1 undo、
  `push_param_gesture` / `build_mod` / `push_mod_depth_bracket` は BPM と同じ idiom。非 0 の色はテーマ Palette から
  (固定色を書かない。memory `project_palette_values_are_linear`)。
- ヘッダ右クリックメニュー (`view/track_header_menu.rs`、base で切り出し済み): Q6。印は widget の `ArrangementTrack` に
  実効状態を渡して描く (DAW 固有 widget なので `common::model` 直結で可、不変条件 8)。
- 録音 / ステップ入力は押した生の pitch を書く (Q2)。
- SMF 書き出し (`midi_export.rs:170-177`): Q7。
- バウンス (`handler/bounce.rs:244-268`): 焼く isolated Song は移調 0 (移調のオートメーション・変調も外す)。できた
  トラック / クリップの追従は元と同じ (Q8)。

## テスト (高いレイヤーで、本番の算術を写すだけのテストは書かない)
- engine: 再生中に移調を変えると旧鍵盤の off と新鍵盤の on が出て、停止時に残りが無い (シーケンサ / mixer の単体で)。
- 既存欠陥の回帰: 再生中にノートの pitch を変えても旧鍵盤が残らない。
- export: 移調 +2 で追従しないトラックだけ鍵盤が変わらない (イベント列で)。
- SMF 書き出しの音程、バウンスで移調 0 になること、load / save 往復 (serde default)。

## 完了条件
索引の共通規則どおり (全件系を回さない、daw_gui を起動しない、branch に commit、逸脱を報告)。
