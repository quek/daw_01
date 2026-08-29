# daw-ui 既知の罠 (低頻度・サブシステム別)

実ビルド / 実動作で踏んだ落とし穴のうち、**該当サブシステムを触るときにしか要らない**もの。
毎セッション読む頻度の高いもの (winit 0.30 の入力・drag、shortcut の属性、immediate-mode +
Edit queue 等) は [../CLAUDE.md](../CLAUDE.md)「既知の罠」に残してある。

設計の正本は [plan.html](plan.html)。

## line pipeline: **sub-pixel の quad は rasterizer が落とす**

`push_lines` の 1 segment は cap を持たない素の quad (`pipelines/line.wgsl` は `along` が
0/1 のみで線方向へは 1px も伸ばさない)。 **長さが 1px を切る segment は pixel 中心を掴めず
描かれない**。 円弧を折れ線近似するときに固定角度で刻むとこれを踏む: 半径 14px の knob を
2° 刻みにすると 1 segment の弦長が `14 × 0.035 = 0.49px` で、 弧のあちこちに穴が空く。

- 値弧のように「下地が同じ色の面」 の上に描く分には斑点として微かに出るだけだが、
  **下地を隠す目的で描く弧では致命的** (daw_01: knob の可動範囲外を面の色で塗り潰しても、
  穴から本体の縁と枠が漏れてリングが切れて見えなかった)。
- 対処は `widgets/knob.rs` の `push_arc`: 刻みを **半径に反比例** させて弦長を
  `ARC_CHORD_PX` 前後に保ち、 さらに span を **均等割り** する (単純な足し込みだと最後に
  「余り」 の短い segment が 1 本出て、 そこだけ sub-pixel になる)。 joint は butt 継ぎなので
  各 segment の終端を半 step 重ねて楔形の隙間も潰す。 副次的に instance 数も 1/6 に減る。
- 新しく円弧 / 曲線を折れ線化する widget を足すときは、 **角度ではなく弦長で刻む**。

## TSF (Windows IME / `ITextStoreACP` — M15)

text_input を TSF text store として OS IME に公開し、rtry (Try-Code TIP) のまぜ書き `GetText` / MS-IME 再変換を成立させる経路 (`crates/platform/src/tsf/`、Windows 限定)。設計は [plan_tsf_ime.html](plan_tsf_ime.html)。
- **`AssociateFocus` 必須 (`SetFocus` だけでは不可)**: `ITfThreadMgr::SetFocus(doc_mgr)` は thread の focus doc を設定するだけで **document を HWND に束縛しない**。window が OS focus を得ると msctf は CUAS の既定 document を使い、TIP の編集が我々の `ITextStoreACP` に届かない。症状: rtry ログ `ShiftStart(-10) shifted=0` / `TSF read failed, using postbuf fallback`、まぜ書きが postbuf の backspace 再現で「ねこ→ね」 のようにズレる (= store が空に見えている)。`ITfThreadMgr::AssociateFocus(hwnd, doc_mgr)` で束縛して解決。focus 取得時に `AssociateFocus` + `SetFocus` の両方を呼ぶ (前者は次の focus 変化で効くため後者で即時反映)。
- **winit はデフォルト IME 無効**: 生成時に `set_ime_allowed(window, false)` される。focus 中に app が `set_ime_allowed(true)` を呼ばないと IME を ON にできない (TSF doc focus だけでは不足)。既存の「app が `ime_request()` で IME enable を駆動」 contract は不変で、TSF は純粋に additive (daw_01/mixer/piano_roll は無改修で TSF を得る)。
- **STA apartment は winit が保証**: winit が `OleInitialize` で event loop スレッドを STA 化するので `CoInitializeEx(APARTMENTTHREADED)` は `S_FALSE` (既に STA) を返す = 正常 (`did_coinit` で balance)。`RPC_E_CHANGED_MODE` (既に MTA) 時のみ TSF を諦め winit IMM に fallback。TSF COM は STA / `Rc` 保持で **非 Send** なので `WinitWindow` (Send 要求) に持たせず UI スレッド thread-local (`TsfSlot`: Untried/Failed/Active、初期化は 1 度きり試行) に置く。
- **ACP ⇔ byte と invariant 型の Default**: TSF は UTF-16 code-unit offset (ACP)、widget は UTF-8 byte。`AcpMap` で相互変換 (サロゲートは char 先頭へ丸め)。**`#[derive(Default)]` で空 `Vec` になり `len()-1` が underflow した実バグ** → sentinel `[0]` を持つ Default を手実装 + `saturating_sub` 防御。invariant を持つ型は `build()` だけでなく **Default/空構築もテストする**。
- **COM shim の lint**: windows API の wildcard import / `#[implement]` マクロ生成 (`inline_always`) / ACP i32 cast / out-param raw pointer は不可避なので COM module 単位で `#![allow(...)]`。
- **検証は実機 + rtry ログ必須**: 単体テストは純粋ロジック (AcpMap/DocState) のみ。COM 経路は `examples/text_input_ime` (IMM 不介入＝TSF のみ) を rtry 有効化で起動し、gui_01 側 trace + rtry の `%TEMP%\rtry_debug.log` (`text before cursor = '...'` が非空か) で往復を確認する。

## wgpu (29.x 系)

- リサイズ中の surface 再構成で `SurfaceError::Outdated` が稀に発生。`render` の戻りがエラーでもログに出して次フレームまで生かす設計。
- **offscreen rendering** (Phase 18 で `OffscreenRenderer` を実装した際に確定):
  - `Maintain::Wait` は 28 以前の API。29 では **`PollType::wait_indefinitely()`** に置換 (`device.poll(PollType::wait_indefinitely()).unwrap()` が定型)。`Maintain` を import すると型エラー。
  - `compatible_surface: None` で adapter 取得可 (native は OK、WebGL2 のみ surface 必須)。プラグイン UI 埋め込みや snapshot 用途で window なしに使える。
  - `copy_texture_to_buffer` の引数は **`TexelCopyTextureInfo` / `TexelCopyBufferInfo` / `TexelCopyBufferLayout`** (29 の新名称)。`ImageCopyTexture` 等の旧名は使えない。
  - `bytes_per_row` は **`COPY_BYTES_PER_ROW_ALIGNMENT` (= 256) の倍数必須**。`unpadded.div_ceil(256) * 256` で staging buffer に padding し、readback 後に row 単位で詰め直す。`Queue::write_texture` には適用されない。
  - `map_async` + `poll(Wait)` 順序: コールバック登録 → `device.poll(PollType::wait_indefinitely())` の順。逆にするとコールバックが永遠に呼ばれない。
  - `DeviceDescriptor` の `trace: Trace::Off` / `experimental_features: ExperimentalFeatures::disabled()` フィールドが 29 で必須 (省略不可)。`device.rs` / `offscreen.rs` 双方で全フィールド明示。
  - sRGB 二重変換は **起きない**: `Rgba8UnormSrgb` で render → そのまま PNG `ColorType::Rgba` に渡せる (PNG decoder は sRGB 仮定でデコードするので一致)。バイト単位で snapshot 比較するなら `Rgba8Unorm` (linear) を選ぶ判断もあり。
- **uniform buffer の LAST WRITE WINS trap** (M14 Phase 78 で発覚): pipeline instance が **1 つだけ** uniform buffer を保持して同 encoder 内で複数の draw call から `queue.write_buffer` で値を書き換えると、 GPU は submit 時に **最後の write** の値を全 draw が読む (= 各 draw が異なる uniform を期待しても全部同じ値を見る)。 `queue.write_buffer` は deferred で encoder の draw 順とは無関係に submit 直前に 1 度書く。 対処: (a) **per-call で `device.create_buffer`** して各 draw が独自 buffer を参照 (`pipelines/text_effect.rs::run_blur_pass` / `run_composite_pass` 参照)、 (b) **dynamic offset uniform** で 1 buffer + 複数 offset、 (c) **encoder.copy_buffer_to_buffer** で encoder 内に書き込みを order する。 multi-pass / per-instance uniform を扱う pipeline では **(a) を default 設計** にする。 symptom: 複数 effect で全部が最後の効果に化ける、 視覚的に「動いてるように見える」 が pixel 単位 verify で破綻が見つかる ← この性質ゆえ visual smoke「見える」 で OK にせず pixel verify を徹底すること (memory: `feedback_verify_actual_content`)。
- **LAST WRITE WINS の対: 別 submit なら安全** (M14 Phase 93 で確定): 上の trap は **1 つの submit (encoder) 内**で buffer を多重 write して多重 draw が読む場合のみ起きる。 `queue.write_buffer` は次の `queue.submit` で「その submit までに積まれた write を、 command 実行の **前** に flush」 するので、 `write(A) → submit(A) → write(B) → submit(B)` は各 submit が個別の値を読む。 = **別 submit ごとに begin_frame/upload/render/submit を完結させる経路 (例: `composite_scene_to_texture` を呼ぶ毎に独自 encoder を submit) は、 既存 pipeline (rect/line/glyph/texture) を main `render()` と流用しても screen uniform を破壊しない**。 専用 pipeline を増やす (= GlyphPipeline の FontSystem 二重ロード等) 必要はない。 multi-submit-per-frame な新経路を足すときは「同 submit 内に複数 write が無いか」 だけ確認すればよい。

## text_input のタイポグラフィは caller が持つ (`TextInputStyle`)

- **`text_input_at` / `text_input_at_focused` は `&TextInputStyle` (font_size / pad_x) を取る**。
  旧実装はこの 2 つを `draw_text_input` に 14.0 / 8.0 で埋め込んでいたため、
  (a) `scrubable_number_at` が `style.font_size` を渡していても **click で編集モードに入った
  瞬間だけ文字が 14px に跳ねる** (inspector の 11px 欄で実際に起きていた)、(b) 狭い欄に入力を
  置けない (`pad 8 + "L100"@14px = 37px` 必要)、という 2 つの欠陥があった。色は palette が SSoT
  なので style には入れない (r.md #48)。
- **cursor 高は `font_size * 1.2` を rect 中央に置く** (`caret_rect`)。旧 `rect.h - 8.0` は
  font 14 / 高さ 22-28px の欄でしか成立せず、小さい欄では文字より短い caret になる。描画と
  IME 候補位置要求 (`request_ime`) が同じ helper を共有する。
- **テキストは rect で clip する**。横スクロールを持たない widget なので、欄からはみ出した
  グリフは隣の widget の上に重なって描かれてしまう (mixer strip の 30px 欄で顕在化)。
