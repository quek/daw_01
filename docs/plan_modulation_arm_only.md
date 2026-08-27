# plan_modulation_arm_only — 変調ルート指定を ◉ (arm) 一本に統一する

r.md #78「Modulation のルート指定がプラグインが多い、プラグインのパラメータが多いといった理由で
リストに表示しきれていない」。grill-me (2026-08-27) で最終形まで確定。

理想とベストプラクティスを追求する。実装コストは無視して大胆に破壊して作り直す。

**理想**: 変調の割り当ては「効かせたいツマミの所へ行って、そこで源を指定する」という 1 つの原理に
統一され、ツマミが daw_01 の画面にあろうとプラグイン自身の窓の中にあろうと同じ操作で届く。
結果として**長い候補リストを出す必要が構造的に消える**。

## 調査で確定した現状 (一次情報)

### 「表示しきれない」の真因は候補数ではなく widget

- 変調ルートの追加 UI はラック展開時の flat `ui.dropdown` 1 個だけだった
  ([modulation_rack.rs:847-888](../daw_gui/src/view/track_inspector/modulation_rack.rs))。
  候補は `cursor_modulatable_targets()` が「トラックの全 device の全 param」をフィルタ無しで
  1 本の `Vec` に積んだもの。
- `ui.dropdown` の popup 高さは `items.len() * 24px` で**切り詰めない**
  ([dropdown.rs:105](../ui/crates/ui/src/widgets/dropdown.rs)、
  [popup.rs:53-86](../ui/crates/ui/src/popup.rs) の doc に「popup_h は切り詰めない (極端 case では
  末尾 item が画面外で不可視)」と明記)。hit-test は `item_rect.contains(px, py)` のスクリーン座標
  ([menu.rs:170-212](../ui/crates/ui/src/widgets/menu.rs)) なので、**画面外に落ちた項目は原理的に
  クリックできない**。1080p で 45 項目が上限。
- 実測でプラグイン 1 個が 47,137 param を報告した例がある (ユーザーの plugin_host ログ)。
  「候補を絞る小細工」では解けない規模。

### ◉ (arm) が届かない領域が 1 つだけあった

- ◉ + ドラッグは mixer の音量/パン・group transform 8 個・映像 FX param・`Par` パネルの plugin
  param・画像/テキスト数値欄で効く。
- **埋め込み GUI を持つプラグインだけ届かない**。`toggle_slot_gui`
  ([devices.rs:505-541](../daw_gui/src/handler/devices.rs)) は `has_embedded_gui == true` の枝で
  エディタ窓を開くだけで `open_plugin_params` を立てないので、`inspector_plugin_params` が `None` を
  返し ([automation_lanes.rs:623-627](../daw_gui/src/handler/automation_lanes.rs))、daw_gui 側に
  ドラッグできるツマミが 1 つも描かれない。つまり Serum / MPhaser 系は**壊れたリスト一択**だった。
- BPM 欄も `modulation` 引数が `None` で、master カーソル時のリストからしか指定できなかった。

### プラグイン窓の中のツマミは既に daw_gui へ届いている

- プラグインの GUI で knob を触ると host が `PluginEvent::PluginParamTouched` を送り
  ([protocol.rs:761-774](../common/src/protocol.rs))、daw_gui が `ParamGestureBegin` 経由で
  `last_touched_param` を更新する ([ipc.rs:300-330](../daw_gui/src/handler/ipc.rs))。
  消費者は `A` キー (automation lane 追加) と MIDI Learn だけで、**modulation は未使用**だった。

### 表示名が device 名を持っていなかった (= r.md #72 と同根)

- `plugin_param_name` は `info.name` だけを返し、device 名も `info.module` も付けなかった。
  一方 `set_plugin_param_on_track` だけが `format!("{module} {name}")` を手組みしていた。
  同じデータから 2 通りの名前が作られ、どちらも device 名を持たない。
- device 名の解決経路が 3 分岐 (plugin DB の `resolve_plugin_name` / その完全重複 `AppData::resolve_name`
  / 映像 FX の `video_fx::def_by_id().name`)。`PluginInstance` 自体は名前を持たない。

### 他 DAW の一次情報

- **ターゲット先行が主流**。REAPER は FX 窓の Param ボタンが常に last touched parameter を対象に
  固定し、そこから Parameter modulation を開く (ReaperUserGuide §19.4)。Surge XT は param 右クリック
  → "Add Modulation From"、Serum は knob 右クリック → "Mod Source" サブメニュー。
  理由は候補数の非対称性 — 「1 param に対する源」は数個、「1 源に対する param」は数千。
- **Bitwig だけソース先行** (modulation routing button → 対象をクリックしてドラッグ)。
  daw_01 の ◉ はこれ。Bitwig は「1 回の arm で好きなだけ割り当てられる」。
- 候補一覧に検索ボックスを持つのは調べた 5 製品中 REAPER の Envelopes/Automation 窓だけ。
  Ardour は 37 件を超えると "Parameters 1 - 32" という無意味なラベルに落ちる (追随すべきでない失敗例)。

## 確定仕様 (grill-me 2026-08-27) — 見える挙動

1. **「+ route from this…」ドロップダウンを削除**。ルート指定は ◉ 一本。
   → `cursor_modulatable_targets()` は消滅。
2. **◉ 待受中にプラグイン自身の窓の中でツマミを触ると、その param に即つながる**。
   深さは既定 (1.0 の片振り = 現在位置から上端まで目いっぱい)。ツマミが既に上端なら
   `±` ボタンで両振りに切り替える。
3. **1 本つながったら ◉ は自動解除**。
   - プラグイン窓経由 = 触った瞬間。
   - daw_01 側のツマミ = **ドラッグを離した瞬間**。ドラッグ中は毎フレーム
     `AddModRouting` + `SetModRoutingDepth` を撃つので、繋いだ瞬間に解除すると自分のドラッグを切る。
4. **Esc で解除**。ソース削除・プロジェクト切替でも自動解除。
5. **ステータスバーに待受中を常時表示**。ラックの ◉ ボタンはカーソルトラック所有のソースしか
   出ないので、トラックを移ると消える = 唯一の表示にできない。
   ソース色は暗テーマ前提のパレットなので**文字には使わず**、小さなチップ (塗り矩形) にだけ載せ、
   文字はテーマの `text` に固定する ([[feedback_ui_indicator_contrast_on_variable_bg]])。
6. ラック展開時、ドロップダウン跡地には**何も出さない**。
7. **BPM 欄を ◉ 対応にする**。リスト撤去で唯一の取り残しになるため。
   engine ([engine.rs:1240-1248](../daw_audio/src/engine.rs) の `current_bpm`) も書き出し
   ([export.rs:602-614](../daw_audio/src/export.rs)) も既に `SongTempo` 変調を焼いており、
   「効かないのに変調できそうに見える」という旧コメントの前提が stale だった。
8. **表示名を「デバイス名: パラメータ名」に**。CLAP が `module` を報告していれば
   「デバイス名: module/パラメータ名」。ラベルの SSoT は `automation_target_label` 1 本なので、
   ラックの接続行・arrangement のオートメーションレーン名・status message が同時に直る
   (= **r.md #72 も解決**)。
9. **接続行はそのソースを参照する全 routing をトラック横断で並べる**。
   他トラック宛は「Drums ▸ MPhaser: Dry/Wet」のようにトラック名付き。
   ◉ は他トラックのツマミにも効くので、旧実装では「ソース所有トラック ≠ 対象トラック」の routing が
   どちらのインスペクタにも出ず**削除できない孤児**になっていた。
10. **daw-ui の popup に最大高さ + スクロール**、カスケードに画面端 flip/clamp を入れる
    (クラスごと修正。ラック内でもトラックが 45 本を超えるとエンベロープフォロワの
    「どのトラックの音を拾うか」が同じ症状になる)。

### 入口を 2 つに保つ根拠 (到達範囲の差)

同じことをする入口を増やさない。◉ の 2 経路は**到達範囲が実際に違う**:

| 経路 | 届く範囲 | 深さの決まり方 |
|---|---|---|
| per-control ドラッグ | daw_gui が描いているツマミ | ドラッグ量がそのまま |
| プラグイン窓の touch | **プラグイン自身の窓の中のツマミ** (overlay を描けない唯一の領域) | 既定 1.0 |

「最後に触ったパラメータへ繋ぐ」ボタンは**置かない** (grill-me で確定)。◉ の窓経由と到達範囲が
同じで、入口だけ増えるため。

## 実装

すべて `connect_armed_mod_source_to(track_id, target)` に集まる
([handler/modulation.rs](../daw_gui/src/handler/modulation.rs))。待受中でなければ no-op、
繋いだら自動解除 + status message。

- `handler/view_model.rs`
  - `device_display_name(device_id)` 新設 — device 表示名の SSoT (plugin DB / 映像 FX マニフェストの
    2 系統をここ 1 箇所に閉じ込める)。
  - `plugin_param_name(target)` — device 名 + `module` を前置き。映像 FX param も解決する
    (旧実装は `ipc.plugin_params` しか見ず「Param N」に落ちていた)。`track_id` 引数は
    実装が `let _ =` で捨てていた嘘なので削除。
  - `automation_target_label(target)` — 同上。
  - `mod_source_routings(source_id) -> Vec<ModRoutingRow>` — 旧 `cursor_mod_routings` を置換。
  - `track_display_name(track_id)`。
  - `cursor_modulatable_targets` 撤去。
- `handler/modulation.rs` — `connect_armed_mod_source_to` / `armed_mod_source_label` /
  `remove_mod_source` で待受解除。
- `handler/tracks.rs` — `delete_track_inner` が subtree 所有の `mod_sources` を道連れに掃除。
  同じ孤児の別経路: ソースはラックで所有トラックの下にしか列挙されないので、 所有トラックだけ
  消すと**どの画面にも出ず削除できない**まま生き残ったトラックを変調し続ける
  (LFO / Random / MSEG / Steps は song 位置の純関数なので値を出し続ける)。
- `handler/ipc.rs` — `PluginParamTouched` で `connect_armed_mod_source_to`。
- `handler/mixer.rs` — `resolve_name` を free 関数へ委譲 (完全重複の解消)。
- `handler/automation_lanes.rs` — `set_plugin_param_on_track` の手組み `format!` を撤去。
- `view/modulation.rs` — depth ドラッグの立ち下がり edge で待受解除。
- `view/status_bar.rs` — 待受チップ + 文言。
- `view/root.rs` — Esc gate に待受解除 branch (rename / audio editor の後、window を閉じるより前)。
- `view/transport.rs` — BPM 欄に modulation を wire。stale コメントを撤回。
- `view/track_inspector/modulation_rack.rs` — dropdown 削除、接続行を `mod_source_routings` へ。
- `ui/crates/ui/src/{popup.rs,widgets/menu.rs,widgets/dropdown.rs}` — popup の最大高さ +
  スクロール + カスケードの画面端 clamp/flip。

## 保留 (今回の範囲外・別途 r.md へ)

- **SendGain / Mute の変調**: model 上は正規のターゲットで GC まであるのに、engine は
  send gain に automation lane しか適用しない ([execute.rs](../daw_audio/src/graph/execute.rs) の
  send 段)、`Mute` も同様 ([daw_audio/src/automation.rs:92-126](../daw_audio/src/automation.rs) が
  `fill_builtin` を Volume/Pan にしか呼ばない)。**理想は「engine 側で効かせる」**であって
  「候補から隠す」ではない。
- **CLAP `MODULATABLE` を持たない param**: `clap_plugin.rs:657` が非破壊 `param_mod` と破壊的 fold を
  この flag で分岐するので、非 modulatable に繋ぐとツマミ自体が動く。VST3 backend はこの flag を
  一度も立てないため、フィルタ条件に使うと VST3 が全滅する。挙動は従来どおり (dropdown 時代も
  同じ param に繋げた) なので回帰ではない。
- **クリップの音量/ピッチ/フェード、テキストの色・outline・shadow** は引き続き変調対象外。
- **エンベロープフォロワの `tap.source_track` が削除済みトラックを指す**ケース。engine は
  tap buf を解決できず scalar 0 になる (クラッシュはしない) が、ラックの track dropdown は
  `position(...).unwrap_or(0)` で**先頭トラックを選択中のように表示する** (嘘の表示)。
  所有トラック側の孤児は今回潰したが、tap 側は別の穴として残る。
- VST3 の `unitId` + `IUnitInfo` 階層 (CLAP の `module` 相当) は今も破棄している
  ([vst3_plugin.rs:1011-1013](../daw_plugin_host/src/vst3_plugin.rs))。◉ 一本にした結果、
  階層を要する候補一覧が無くなったので当面不要。

関連: [plan_modulation.md](plan_modulation.md)、
[plan_modulation_routing_redesign.md](plan_modulation_routing_redesign.md) (§6 の dropdown を本書が置換)、
[plan_fixme_56_modulators.md](plan_fixme_56_modulators.md)、
[plan_automation.md](plan_automation.md) (§「Parameter Picker 方式は不採用」— 本書はその決定を強化する)。
