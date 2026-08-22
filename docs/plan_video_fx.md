<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan_video_fx.md — ビルトイン GPU 映像効果（FIXME #54）

> FIXME #54: 「動画・画像に色調整や座標変換などのエフェクトをかけたい。ビルトインプラグインとして
> トラックの FX チェーンに刺す。オートメーション可能。他トラックの音声でモジュレーション可能
> （エンベロープフォロワー）。キックにあわせて動画にエフェクトをかけたい。」
>
> 関連: [plan_modulation.md](plan_modulation.md)（変調基盤＝この効果の param を駆動）、
> [plan_linear_chain.md](plan_linear_chain.md)（device chain）、
> [plan_tachie_group_transform.md](plan_tachie_group_transform.md)（Transform に統合される既存機能）。

## 0. 設計決定サマリ（/grill-me 2026-06-13）

| # | 決定 |
|---|---|
| Q1 | 効果範囲＝**代表的 NLE の全カテゴリ**（色だけでなくモザイク/ノイズ/スケッチ等まで網羅）。色は新規、変換は既存統合 |
| 框組 | 各効果 = **WGSL シェーダパス + 宣言的パラメータ表**（ISF/OBS 流）。共有プリミティブ=分離ブラー・フィードバック履歴ターゲット |
| Q8 | **座標変換をチェーンの Transform 効果に一本化**（立ち絵グループ変形を統合、per-instance 画像配置ハンドルは素材レイアウトとして存置） |
| Q9 | 効果は**トラック単位のみ**（クリップ単位は持たない。画像/動画ごとに別処理は別トラック＝レイヤーへ） |
| 適用対象 | トラックの**合成画（動画＋PiP 画像＋テキストを 1 枚の RGBA 層）**にチェーン順で作用 → 親/マスターへ合成。マスター映像チェーンも持つ |
| Q2-Q7 | 変調は [plan_modulation.md](plan_modulation.md)。共有フォロワー・帯域・3 段タップ・加算＋極性・音声もサンプル精度・書き出しは実音焼き込み |

## 1. 効果フレームワーク（shader + manifest）

調査結論: **ISF（Interactive Shader Format）= 「GLSL fragment shader + 入力宣言 JSON」** が、まさに
「WGSL パス + 宣言的 param」のモデル。各効果を個別に Rust ハードコードせず、**汎用フレームワーク**で
param ↔ uniform 配線・オートメーション・変調を **1 か所に統一**する（SSoT・拡張容易）。

```rust
// daw_gui/src/video_fx/ (新規)
pub struct VideoFxDef {
    pub id: &'static str,            // "builtin.video.color" 等。PluginInstance.plugin_id に対応
    pub category: VideoFxCategory,   // §6 の ISF タクソノミー
    pub params: &'static [VideoFxParam],  // 宣言的パラメータ表（manifest）
    pub passes: &'static [VideoFxPass],   // 1..N の WGSL パス
    pub needs_history: bool,         // フィードバック履歴ターゲットを要するか（§1.2）
}
pub struct VideoFxParam {
    pub id: u32,                     // PluginParam.param_id に対応
    pub name: &'static str,
    pub kind: ParamKind,             // Scalar{min,max,default,unit} | Bool | Color | Enum | Texture(LUT)
}
pub struct VideoFxPass {
    pub wgsl: &'static str,          // fragment shader（uniform = param manifest）
    pub kind: PassKind,              // Simple | SeparableBlur{axis} | History | Downsample/Upsample
}
```

### 1.2 共有 GPU プリミティブ（調査が指定した「設計に織り込む 2 つ」）

1. **分離ブラーパス**（H/V 2 パス）を 1 実装。bloom/glow/soft-vignette/unsharp の土台。
2. **フィードバック履歴ターゲット**（前フレーム出力を保持する persistent ping-pong）。
   echo/残像トレイル/VHS/フィードバックグリッチの土台。これだけが「ステートレス 1 パス」から
   外れる構造要件なので、最初に設計に入れる。

それ以外は宣言的 uniform を持つステートレス fragment パス = ISF モデルそのまま。

## 2. チェーン表現（統合 device chain・SSoT）

ユーザー要求「FX チェーンに刺す」＋既存 port 直結哲学に従い、**`Track.devices: Vec<PluginInstance>`
の 1 本に統合**。映像効果は `PluginInstance { format: Builtin, ports: 映像 ports }`。

- `PluginFormat::Builtin` は既存（Silence/Voicevox）。映像効果も Builtin だが **処理場所が違う**:
  音声 Builtin は daw_plugin_host、**映像 Builtin は GUI 描画パス**（wgpu context は GUI が所有）。
- `PortConfig`（port_config.rs）に映像 port を追加: `has_video_input`/`has_video_output`
  （音声/note port は false）。port 直結哲学のまま「映像→映像」を順に繋ぐ。音声 device は映像 bus を
  素通り、映像 device は音声 bus を素通り。**両ドメインの順序は各々独立、ドメイン跨ぎの相対順は無意味**。
- **param は既存 `AutomationTarget::PluginParam { device_index, param_id }` を再利用**（critique 由来の
  改良: workflow 案の `VideoFx` 専用 variant より SSoT）。`device_index` は統合 `Track.devices` 上の位置。
  評価の **consumer をドメインで分岐**: 映像 Builtin → GUI で評価（毎フレーム）、音声プラグイン →
  plugin_host へ event 送信（既存）。
- UI: chain インスペクタは 1 リストに映像/音声 device を表示（ドメインを badge で区別）。

## 3. 適用対象・合成（compositing）

音声 FX の対称形。トラック映像効果は **そのトラックの合成画 1 枚（RGBA）** にチェーン順で作用。

```
[トラックの素材]  動画クリップ(letterbox) + PiP 画像 + テキスト
       │  既存 active_*_at で集約・per-event fade/opacity 適用
       ▼
[トラック合成画]  1 枚の RGBA オフスクリーンテクスチャ
       │  ← 映像 device チェーンを順に GPU パス適用（色→ブラー→歪み...）
       ▼
[親/マスターへ合成]  z 順で master canvas へ。立ち絵グループは入れ子:
                    子トラック効果 → 子を群へ合成 → 群/親の効果＋Transform
       ▼
[マスター映像チェーン]  最終 master canvas に作用（音声 master_fx_chain の対称）
```

- per-image を別処理したい場合は別トラック（Q9）。映像の重ねは元々レイヤー＝トラック単位なので自然。
- 立ち絵グループの入れ子合成（group_compose.rs）に効果適用点を挿入。Transform（§5）は群/親トラックの
  チェーン上の効果として作用。
- **Generate 系**（lens flare 生成・gradient・noise 生成）は「入力を持たず生成」するため、純生成は
  **source clip 扱い**でチェーン効果に含めない。ただし既存画にブレンドする overlay 系（light leak・
  lens flare の重ね）は blend 付き効果として可。

## 4. 効果カタログ（ISF タクソノミー・リアルタイム実現可能のみ）

D 階層（オプティカルフロー/真のデノイズ/AI マット/スタイル転送/モーショントラッキング = リアルタイム
不可）は対象外。フレームワーク上で順次追加するが、**完全目標セット**を以下に列挙
（`feedback_enumerate_complete_feature_set`）。★ = 音反応の花形。

| カテゴリ | 効果 | GPU 形 |
|---|---|---|
| **色補正/グレード** | 明度・コントラスト・露出・ガンマ・彩度・色相・カラーバランス(lift/gamma/gain)・カーブ(master+RGB)・色温度/ティント・チャンネルミキサー・白黒/セピア/デュオトーン・LUT・ビネット★ | A(1 パス) |
| **HSL 二次** | HSL キー・色置換・部分脱色 | A |
| **ブラー/シャープ** | ガウス・ボックス・方向/モーション・放射(回転/ズーム)・レンズ/被写界深度・チルトシフト・シャープ/アンシャープ・ブルーム/グロー★ | A/B(分離・downsample) |
| **歪み/ワープ** | Transform(§5)・コーナーピン・レンズ歪み・**モザイク/ピクセル化★**・ミラー・球面/バルジ・ツイル/渦・リップル/波・ディスプレイスメント・極座標・**カレイドスコープ★** | A/C |
| **スタイライズ** | グロー・**エッジ検出/スケッチ**・エンボス/レリーフ・ポスタライズ★・しきい値★・ハーフトーン/網点・カートゥーン/コミック・スキャンライン・ストロボ | A/B |
| **キーイング** | クロマ/カラーキー・ルマキー・スピル抑制 | A |
| **ノイズ/質感** | フィルムグレイン★・ノイズ・VHS/アナログ劣化★・Bad TV・光漏れ★ | C(時間/ノイズ) |
| **時間/フィードバック** | エコー/**残像トレイル★**・フレームフィードバック・モーションブラー合成 | C(履歴) |
| **音反応の花形（横断）** | **ズームパンチ・フラッシュ・シェイク・RGB スプリット・グロー点滅・グリッチバースト・残像パルス** | A/B/C |

出典: After Effects / Premiere / DaVinci Resolve / Final Cut の効果メニュー、OBS フィルタ、
TikTok Effect House post-process、ISF ~200 シェーダライブラリ、CapCut のビート同期効果群。

## 5. Transform 一本化（Q8）

座標変換の SSoT。現状 3 経路（①画像ごとハンドル ②立ち絵グループ 2D 変形 ③テキスト変形）のうち、
**トラック単位の「動かす変形」をチェーンの Transform 効果に一本化**。

- **Transform 効果** = チェーン上の 1 device。param: position(x,y)・scale(x,y)・rotation・anchor(x,y)・
  opacity・(任意で skew/flip)。既存 `texture.wgsl` の rotation+pivot インフラを流用。
- **立ち絵グループ変形を統合**: 既存 `GroupTransform`（model.rs）＋ `AutomationTarget::GroupTransform`
  を、群/親トラックのチェーン上の Transform 効果に作り直す。`plan_tachie_group_transform.md` の機能は
  Transform 効果として再表現（移行: 既存 `group_transform` フィールド → チェーン先頭 Transform 効果へ lift）。
- **per-instance 配置ハンドルは存置**: 個々の画像/テキストの x/y/w/h/rotation（`ImageEvent` 等）は
  「素材レイアウト＝どこに置くか」として残す。動かす（アニメ/自動化/変調）変形はチェーンの Transform。
- これで「動かす変形のやり方が 1 つ」（SSoT）。Transform 効果の param は §2 のとおり PluginParam として
  自動化・変調可能 → 「キックでズーム」が Transform.scale への ModRouting で実現。

## 6. オートメーション・モジュレーション

- 全映像効果 param は既存 lane で自動化可（`AutomationTarget::PluginParam`、GUI 評価）。
- 変調は [plan_modulation.md](plan_modulation.md) の `ModRouting` を param の lane に付与。
  映像 param は block 粒度（30Hz `AudioBridge.mod_scalars`）で消費（§4.2 of plan_modulation）。
- 代表ユースケース「キックにあわせて」: ModSource{tap: kick track, band: 低域, attack 短/release 中} を
  作り、Transform.scale（ズーム）/ Color.brightness（フラッシュ）/ Glow.intensity（点滅）/
  RGBSplit.offset 等に depth 付きで割当。

## 7. 書き出し

映像効果は `render_video.rs` のオフスクリーン経路でも同一適用（preview と同じパス実装）。
変調入力は [plan_modulation.md §7](plan_modulation.md) のとおり、音声を先に render して env sidecar を
焼き込み、frame ごとにサンプル → プレビューと一致。`daw_gui --smoke-test` 系の visual regression に
映像効果適用後の健全性チェックを追加（`feedback_verify_actual_content`）。

## 8. レンダリングパイプライン挿入点

- **preview**: `preview_window.rs` / `runner.rs` の合成前に、トラック合成画をオフスクリーンに描いて
  効果パスを適用 → master canvas へ。`gui_01` の `composite_scene_to_texture` / `CompositePool` を流用
  （既存の group 合成と同じ仕組み）。
- **export**: `render_video.rs::build_frame_scene` の per-track 合成に効果パスを挿入。
- 効果パス実行基盤を `daw_gui/src/video_fx/` に新設（パス列を順に ping-pong 適用、分離ブラー・履歴
  ターゲットを共有プリミティブとして提供）。texture pipeline（gui_01）を拡張 or 効果用に専用パイプライン。
- **gui_01 への要望**: 効果パス（任意 WGSL fragment + uniform + ping-pong + 履歴ターゲット）を流す
  汎用 API が gui_01 に無い場合、`docs/gui_01_conversation.md` に最終形態で要望
  （`feedback_gui_01_conversation` / `feedback_gui_01_scope_review` / `feedback_gui_01_link_plan_ref`：
  本ファイル参照を付す）。要望提出を interim 実装より先に（`feedback_gui_01_request_before_interim`）。

## 9. データモデル変更

- `PortConfig`（port_config.rs）: `has_video_input`/`has_video_output` 追加。
- 映像効果の **plugin DB 登録**: 内蔵 `VideoFxDef` 一覧をプラグインピッカに Builtin として列挙
  （カテゴリ別ブラウザ）。`PluginInstance.plugin_id = "builtin.video.<id>"`、`state` に param 値。
  ※ param 値の SSoT は automation lane の `default_value`（既存 knob 流儀）。`state` は最小限。
- マスター映像チェーン: `Song` に映像版 master chain を追加（音声 `master_fx_chain` の対称）。
  ※ 1 本に統合する案も検討（`master_fx_chain` を音声/映像混在 1 本に）— §2 のトラック方針と対称に
  するなら混在 1 本が SSoT。
- bincode derive / `cargo build --workspace`（`feedback_workspace_build_for_protocol_changes`）。

## 10. UI

- chain インスペクタに映像 device を表示（ドメイン badge）。プラグインピッカに内蔵映像効果を
  カテゴリ別に列挙。
- 効果 param 編集は既存 `scrubable_number` / image inspector mirror を流用
  （`feedback_reuse_inspector_idiom`、bespoke edit-buffer を作らない）。
- param への変調割当 UI は [plan_modulation.md §9](plan_modulation.md) のラックと共通。

## 11. Touch points

| 層 | ファイル | 変更 |
|---|---|---|
| Port | `common/src/port_config.rs` | `has_video_input/output` |
| Model | `common/src/model.rs` | マスター映像チェーン; Transform 統合（group_transform→チェーン）; bincode |
| Fx framework | `daw_gui/src/video_fx/`（新規） | `VideoFxDef`/param/pass; 効果パス実行基盤; 共有分離ブラー・履歴ターゲット; 全効果の WGSL |
| 合成 | `daw_gui/src/{preview_window,runner,render_video,group_compose,image_compose}.rs` | per-track 合成画 → 効果パス → master |
| Plugin DB/picker | `daw_gui/src/app.rs` ほか | 内蔵映像効果の列挙・追加 |
| 自動化評価 | `common/src/automation.rs` / GUI compose | 映像 PluginParam を GUI で評価 + §2 合成（plan_modulation） |
| gui_01 | `docs/gui_01_conversation.md` | 効果パス汎用 API 要望（必要時） |

## 12. Phasing

1. **効果パス基盤**（`video_fx/`）: 単一ステートレスパスをトラック合成画に適用できる最小実装 +
   共有分離ブラー + 履歴ターゲット。preview で 1 効果（例: Color 明度/コントラスト/彩度/色相）が動く。
2. **チェーン統合**: 映像 device を `Track.devices` に挿せる（port 追加・picker・GUI 評価・ドメイン分岐）。
3. **オートメーション**: 映像 PluginParam を lane で描ける。
4. **Transform 一本化**: Transform 効果 + 立ち絵グループ変形の移行。
5. **変調**: [plan_modulation.md](plan_modulation.md) と接続（「キックでズーム/フラッシュ」が動く）。
6. **カタログ拡充**: §4 を カテゴリ単位で波状に実装（色 → ブラー/グロー → 歪み/スタイライズ →
   ノイズ/フィードバック → キーイング）。各波で visual smoke test。
7. **マスター映像チェーン** + **書き出し一致検証**（preview=export）。

## 13. 実装進捗（2026-06-14、worktree `videofx-waves`）

Phase 1〜3・5（変調基盤）は先行 commit（`abcb5ef` 他）で landed。本セッションで以下を実装・検証:

- **Wave4a（効果実行基盤拡張）**: `apply_chain` に **SeparableBlur**（H/V 2 パス、1 軸ガウシアン）を
  実行配線。カタログに Gaussian Blur / Pixelate 追加。GPU pixel-verify テストで実 sample を確認。
- **Wave2（トラック合成 1 枚化）**: `composite_layers`/`group_layers`/`text_layers` を **`TrackComposite`
  に統合**。動画 + PiP 画像 + テキストを owning track ごとに 1 枚の RGBA（canvas 解像度）へ合成 →
  track 効果チェーンを 1 回適用 → 配置。**spatial 効果がトラックの最終見た目 1 枚に正しく作用**。
  preview / export は共通 `group_compose::composite_and_place`（SSoT）。効果も transform も無い
  plain トラックは合成往復しない fast-path（クリスプ・無回帰）。
- **Wave3（Transform 一本化）**: `builtin.video.transform` を **チェーン上の配置 device** として刺せる
  （どのトラックでも）。値・automation・変調は purpose-built な `GroupTransform` 系をそのまま使用
  （log スケール・AE 流アンカー・実績あり、**破壊的な値 migration なし**）。`resolve_track_transform`
  が device-gate で配置を効かせ、`ensure_ids`（v25）が旧 `group_transform` 持ちトラックに device を
  補完（additive、idempotent）。picker / inspector / preview drag / add-remove を device 連動に配線。
- **Wave1（マスター映像チェーン）**: `master_fx_chain` に映像 device を許可（混在 1 Vec）。全トラック
  合成後の master canvas 1 枚に master 効果を適用（preview = `render_placeholder`、export =
  `build_frame_scene` 末尾、同一 SSoT）。master 用 automation/変調は `song_lanes`/`song_mod_routings`。
  master picker の Video 除外を解除（Transform は除く）。
- **Wave4b（カタログ拡充）**: `P.time`（song 時間、preview/export 一致）を配線。**+9 効果**を追加
  （RGB Split・Threshold・Posterize・Edge Detect・Mirror・Chroma Key・Sharpen・Film Grain・
  Scanlines）。全効果の WGSL コンパイル+実行を GPU テスト（`all_catalog_effects_compile_and_run`）で検証。

検証: `cargo build/clippy --workspace` green、common 267 + daw_gui 132 lib tests + GPU executor 8 +
model migration test 全 pass、**`daw_gui --smoke-test` PASSED**（unique_colors 22844 / black 9%、
合成パイプライン非回帰）。

### 残（follow-up）

- **History / フィードバック系プリミティブ**（echo・残像トレイル★・VHS・モーションブラー合成）+
  **Bloom/Glow**（blur + 原画合成）: いずれも **2 入力**（前フレーム出力 or 原画 + 現フレーム）を要し、
  `apply_chain` の bind group を 2 texture に拡張 + per-effect-instance の **frame 跨ぎ永続ターゲット**
  （安定 `chain_key` で keying、`end_frame` の recycle 対象外）の新設が要る。SeparableBlur プリミティブ
  と broad なステートレスカタログは完了済。2 入力 engine 経路は単独の follow-up として実装する。
