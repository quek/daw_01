# r.md #113: PC キーボードによる仮想鍵盤

grill-me (2026-09-06) で確定した最終形。

## 見える挙動

- **`K` で仮想鍵盤ウィンドウを開閉** (View メニュー「仮想鍵盤」も同じ)。閉じ方は `K` / ✕ / Esc。
  閉じた瞬間に PC キーの横取りは終わり、鳴っている音は全部止まる。
- **窓が開いている間だけ** PC のキーが音になる。配列は REAPER / FL Studio 式の 2 段:

  ```
    2 3   5 6 7   9 0            上段: Q=C  2=C# W=D  3=D# E=E  R=F  5=F# T=G  6=G# Y=A  7=A# U=B
   Q W E R T Y U I O P                 I=C  9=C# O=D  0=D# P=E   (1 オクターブ上)
    S D   G H J   L ;            下段: Z=C  S=C# X=D  D=D# C=E  V=F  G=F# B=G  H=G# N=A  J=A# M=B
   Z X C V B N M , . /                 ,=C  L=C# .=D  ;=D# /=E   (= 上段の Q..E と同じ音)
  ```

  `[` / `]` でオクターブ下げ / 上げ、`{` / `}` (= Shift+`[` / `]`) でベロシティ −10 / +10。
  既定は下段 `Z` = C3 (MIDI 48、上段 `Q` = C4 = 60)、ベロシティ 100。
- **鍵盤に使うキーだけ鍵盤が取る**。重なる既存ショートカット (S ソロ / Q ミュート / D 複製 /
  E 分割 / J / X / Z / G / B / P / R / 2 / 3) は窓が開いている間は効かない。A / F / L / 1 /
  Space / Ctrl 系など重ならないものは普段どおり。Ctrl / Alt / Win 付きは横取りしない
  (Ctrl+Z の undo 等はそのまま)。
- **宛先はカーソルトラック (= 選択中のトラック)**。録音待機 (R) は不要 (2026-09-06 実機確認で
  変更: 当初は MIDI キーボードと同じ「R のトラック」だった)。選択が無ければ鳴らず、窓の中に
  「トラックを選択してください」と出る。録音は、録音実体が走っていて **そのトラックが R** の
  ときだけ書き込む (録音自体は従来どおり R が要る)。停止中は step 入力 (MIDI 入力と同じ順序)。
  ランチャーの pad binding は通らない (MIDI デバイスの学習結果なので)。
- **窓の中身**: 2 オクターブ半のピアノ鍵盤 (白鍵 / 黒鍵) に PC キー名を印字。押している鍵
  (PC キー / マウス) が光る。マウスで鍵を押す / 押したまま横へ滑らせる (glissando) でも鳴る
  (ピアノロール左の鍵盤と同じ操作)。上部に「Oct C3」(− / + ボタン) と「Vel 100」
  (ドラッグ / ホイール / クリックで数値入力) と宛先トラックの表示。
- **窓の振る舞い**: タイトルバードラッグで移動、リサイズ無し (固定サイズ)、開いたまま
  背後のアレンジ等をマウスで操作できる (編集履歴ウィンドウと同じ true-floating)。
- **永続**: 位置・オクターブ・ベロシティは app_config (この人の作業のしかた)。開閉は保存
  しない (起動時は常に閉)。
- **安全弁**: テキスト入力中はキーは文字になり鳴らない。窓が非アクティブになったら
  (Alt+Tab) 押していた音を全部止める。OS の auto-repeat は無視 (押しっぱなしで連打しない)。
  オクターブを変えても押している最中の音はそのままの高さで鳴り続け、離したときに正しく止まる。

## 構造

```
winit KeyboardInput (Pressed / Released)
   │
   ▼  daw-ui core `Ui::frame` 冒頭 (shortcut 層より前)
key grab (`UiHost::set_key_grab`, `crates/ui/src/key_grab.rs`)
   │  横取り対象: 宣言された physical key の Released 全部 + command 修飾なしの Pressed
   │  typing_lock 中 / 真のモーダル中は Pressed を横取りしない (Released は常に = stuck 防止)
   ▼  `Ui::take_grabbed_keys()`
view/virtual_keyboard.rs (窓が開いているフレームだけ take)
   │  Edit::mutate → AppEvent::VirtualKeyboard(VirtualKeyboardEvent::Key{..})
   ▼
handler/virtual_keyboard.rs
   │  key → semitone (virtual_keyboard.rs の表、純関数) → pitch = base + semitone
   │  held: Vec<(PhysicalKey, pitch)>  (離したときの pitch はここから引く)
   ▼
monitor_note_on_track(cursor) / record_midi_note_on_tracks(&[cursor]) / step_input_note_on
   ← MIDI デバイス (midi.rs) と同じ部品。違うのは宛先の決め方 (R 全部 vs カーソル 1 本) だけ
```

- daw-ui core は「宣言されたキーの生 press / release を横取りして渡す」だけで、鍵盤の
  意味は知らない (不変条件 8)。
- 状態: `AppData.virtual_keyboard` (`state/virtual_keyboard.rs`、session-only: open / held /
  mouse_pitch) と `UiPrefs.virtual_keyboard_{rect,base_pitch,velocity}` (app_config 永続)。
- Song は触らない (録音は既存の `record_midi_note_on/off`)。
