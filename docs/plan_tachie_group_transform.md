# 立ち絵 group transform（親グループトラックで立ち絵パーツをまとめて動かす）

## 0. ゴール

立ち絵素材をパーツ（目 / 口 / 髪 / 体 / 腕 …）ごとに別 image トラックに置き、
それらを「親グループトラック」の 2D アフィン transform でまとめて移動・回転・拡縮する。
パーツ個別の automation（口パク・まばたき・手振り）と立ち絵全体のまとめ移動を二層に分離する。

業界標準（After Effects の親子付け / Null Object、AviUtl 拡張編集のグループ制御、
Live2D の親デフォーマ）と同じ「親 = 描画を持たない transform-only ノード」アーキテクチャ。

---

## 1. 確定要件（前セッションでユーザーと合議済み）

- **パーツ分割**: 立ち絵パーツを各々別の image トラックに置く。
- **親グループトラック**: 既存の `Track.parent_group_id` で子をぶら下げる。
  audio sub-mix 階層（既存）を visual transform にも **共有**する（Single Source of Truth）。
  立ち絵パーツは無音なので audio graph には空 sub-mix ができるだけで害なし（§8 で検証）。
- **親グループ = 描画を持たない transform-only ノード**。2D アフィン transform ＋ 全体 Opacity を持つ:
  位置 X/Y・回転・拡縮 ScaleX/ScaleY・任意アンカー AnchorX/AnchorY・Opacity
  （AE の Transform プロパティ群と同構成。Opacity は行列に乗せず合成後 alpha に適用）。
  全 property を automation lane で動かせる。
- **アンカー** = 回転・スケール共通の中心。任意位置指定可（preview 上ドラッグ + 数値、automation 可）。
- **子の個別動作**（口パク=画像差し替え、まばたき、手振り=回転）は各子トラックの既存
  automation で表現。合成 *前* に各子へ適用されるので親 transform と干渉しない。
- **伝播**: 親→子の一方向のみ（Live2D 準拠）。子は親に影響しない。

---

## 2. 合成方式 = アプローチ X の採用判断

ユーザー選択: **アプローチ X（composite-then-transform）**。

> 子パーツを z 順に **1 枚のオフスクリーンテクスチャ**へ合成し、その合成済み 1 枚の矩形画像に
> 親 transform をかける。各パーツに個別に行列をかける方式（アプローチ Y）ではない。

### なぜ X か（Y を却下する理由）

- アプローチ Y（親行列を各子に個別適用）では、**親の非一様スケール × 子の回転**で
  **shear（せん断歪み）** が混入する。`R_child · S_parent(非一様)` は回転を挟むため
  対角スケールが斜め方向にずれ、矩形が平行四辺形に潰れる。
  After Effects 公式コミュニティが「子レイヤーが歪む。これを直接補正する transform は存在しない」
  と明言している（§10 引用）。
- アプローチ X は **1 枚に合成してから 1 つの行列**をかけるので、合成済みテクスチャは
  常に「軸整列した矩形コンテンツ」。これに `T·R·S` をかけても像は **回転した矩形**にしかならず
  （`R·S` は矩形を保つ。shear は `S` が回転を挟むときだけ生じる）、原理的に shear が出ない。
  非一様スケールも歪まず自由。AviUtl 拡張編集のグループ制御の実挙動と同型。

### X が実現可能な根拠（gui_01 調査済み）

- gui_01 は render-to-texture を実装・検証済み:
  `crates/renderer/src/pipelines/text_effect.rs` が
  「オフスクリーン target 生成 → render pass で描画 → テクスチャ再 sampling」を実装している
  （text effect = glyph をオフスクリーン合成 → blur → composite → 1 枚として base scene に push）。
- 低レベル primitive `crates/renderer/src/texture_store.rs:182` `create_render_target` が
  `TEXTURE_BINDING | RENDER_ATTACHMENT` テクスチャと `(TextureHandle, wgpu::TextureView)` を返す。

### X の制約（立ち絵では非問題、§8 で詳述）

- グループの z が **1 枚に潰れる**（グループ内部に背景動画等を z 的に挟めない）。
- **合成解像度の管理**（親拡大時のボケ回避に合成キャンバスを十分大きく確保する必要）。

---

## 3. 業界標準の合成式・伝播モデル

列ベクトル・左乗算（OpenGL / glm / 数学標準）。点 `p` に対し `p' = M · p`。

### local 行列（親自身の transform）

```
M_local = T(pos + anchor) · R(rot) · S(sx, sy) · T(-anchor)
```

- `T(-anchor)`: アンカー点を原点へ移動
- `S(sx, sy)`: アンカー中心に拡縮（非一様可）
- `R(rot)`: アンカー中心に回転
- `T(pos + anchor)`: アンカー点を最終位置へ。`pos = 0` ならアンカーは元位置に留まり、
  その点を中心にコンテンツが拡縮・回転する（AE のアンカー＝home の挙動）。

### world 行列（親子の合成、トップダウン再帰）

```
M_world = M_parent · M_local
```

立ち絵は親 1 段だが、グループのネスト（グループの中のグループ）も同式で再帰可能。

### Opacity は行列に乗せない（AE 準拠）

- group opacity は transform 行列に含めず、**合成済みテクスチャの alpha** として別経路で乗算。
- 子 opacity は合成 *前* に各子 quad の alpha として焼き込む。
- → group opacity（`GroupTransform.opacity`, §4.1, Phase 1 確定）は「グループ quad の
  `TexturedQuad.alpha`」に適用する。子 opacity は合成前に各子 quad alpha へ焼き込む。

### 伝播は一方向（親→子のみ、Live2D 準拠）

子の automation は子トラックのレーンにのみ存在し、合成前に子へ適用。親の変化は子へ伝播するが、
子は親へ影響しない。

---

## 4. データモデル（common/src — 調査で全行確認済み）

すべて bincode `Encode`/`Decode` 由来の **IPC 型**を含むため、変更後は
**`cargo build --workspace`**（daw_audio.exe が古い protocol のまま LoadSong decode 失敗 →
「再生が止まる」誤認症状を防ぐ。`feedback_workspace_build_for_protocol_changes`）。

### 4.1 `GroupTransform` 構造体（新規）

`ImageEvent`（`common/src/model.rs:1875-1916`, `rotation_radians: f32` serde-default 済み）を範に取る。

```rust
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct GroupTransform {
    pub x: f32,              // 位置 X（normalized 0..1 project 空間、AnchorをposでずらすオフセットはAE同様）
    pub y: f32,              // 位置 Y
    pub rotation_radians: f32, // clockwise positive（ImageEvent と同符号）
    pub scale_x: f32,        // 倍率 1.0 = 等倍
    pub scale_y: f32,        // 倍率（非一様可）
    pub anchor_x: f32,       // アンカー（合成キャンバスの normalized 0..1、default 0.5 = 中央）
    pub anchor_y: f32,
    pub opacity: f32,        // 0..1。行列には乗せず合成済みグループ quad の alpha に適用（AE 準拠）
}
// 手書き Default: x=0, y=0, rotation=0, scale=1.0, anchor=0.5, opacity=1.0
// AE の Transform プロパティ群（Anchor/Position/Scale/Rotation/Opacity）と同構成。
```

`Track` には derive に `Eq`/`Hash` が無い（`f32` 含むため）ので `GroupTransform` も同様。

### 4.1.1 transform 値の二層格納と「グループトラックにクリップを置くか」

- **親グループに画像/動画/テキストの表示クリップは置かない**（transform-only ノード）。
  立ち絵素材クリップは子トラックにある。
- ⚠ **GroupTransform は `TrackBuiltin`（volume/pan）型 = クリップ非依存のトラックレベルパラメータ**。
  `ImageBuiltin`/`TextBuiltin`（= image/text clip が存在する時間範囲だけ override、`model.rs:2109-2119`）
  とは異なる。グループトラックに表示クリップが無くても lane は有効で、**グループが子を描画している間
  ずっと適用**される（音量 automation がクリップ無しでトラック音量を動かすのと同型）。
- transform 値の二層:
  - **静的基準値**（アニメさせない transform）→ `Track.group_transform` struct（§4.2）＋ inspector の
    数値/ノブ。親グループのタイムライン上は空のまま。
  - **時間変化（automation、任意）** → 他トラックと同じ `Track.automation_lanes[].clips`
    （`AutomationLane{ target: GroupTransform(param), default_value, clips }`, `model.rs:1074, 2274`）に
    **automation クリップ**を置く。クリップは任意で、無くても lane の `default_value` で一定値駆動できる。
- 解決順（各 param ごと、clip 非依存）: GroupTransform(param) lane があれば現 beat で評価
  （automation クリップ内ならカーブ、外なら `default_value`）、lane が無ければ `group_transform` の基準値。
- 親グループは表示クリップが無くても arrangement に行として現れ（`parent_group_id` インデント）、
  選択・inspector 数値編集・preview ドラッグ・レーン追加（animation）が可能。

### 4.2 `Track.group_transform`（新規フィールド）

- `Track`: `common/src/model.rs:1000-1089`。`parent_group_id: Option<u32>` が 1043-1049、
  最終フィールド `color: Option<[f32;3]>` が 1081-1088。
- **`color` の後ろに append** する（mid-struct 挿入は `..Default::default()` を持たない
  positional `Track{}` literal を壊すため末尾追加）:
  ```rust
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub group_transform: Option<GroupTransform>,
  ```
- `Track::default`（`model.rs:1137-1161`, 手書き全列挙）に `group_transform: None` を追加（~1158）。

### 4.3 `AutomationTarget::GroupTransform`（新規 variant）

- enum: `common/src/model.rs:2092-2121`。現 6 variant
  `TrackBuiltin / PluginParam / SongTempo / SongTimeSigNumerator / ImageBuiltin / TextBuiltin`。
  derive に **`Eq` + `Hash`**（HashMap/HashSet キーに使う、`automation.rs:382`）。
- `TextBuiltin` の後ろに append:
  ```rust
  GroupTransform(GroupTransformParam),
  ```
- `GroupTransformParam`（新規、`ImageBuiltinParam` の Copy sub-enum idiom `model.rs:2155-2175` を範に）:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
  pub enum GroupTransformParam { X, Y, Rotation, ScaleX, ScaleY, AnchorX, AnchorY, Opacity }
  ```
  ⚠ enum 全体が `Eq + Hash` なので param も `Eq + Hash + Copy` 必須。`f32` を持たない純粋な
  tag enum なので問題なし。

### 4.4 automation 正規化（`common/src/automation.rs`）

- `plain_to_norm`（36-66）/ `norm_to_plain`（70-96）。`norm_to_plain` は **top-level wildcard が無い**
  ので `GroupTransform` arm は **必須**（追加しないとコンパイルエラー = landing 検知になる）。
- 追加する分岐:
  - `X / Y / AnchorX / AnchorY / Opacity`: **identity**（既に normalized 0..1。clamp 緩め。
    Opacity は ImageBuiltin Opacity と同 idiom）。
  - `Rotation`: `(plain + PI) / (2 * PI)` ⇔ `n * 2 * PI - PI`（ImageBuiltin/TextBuiltin Rotation と同式）。
  - `ScaleX / ScaleY`: **log space** `0.1..10`。
    - `plain_to_norm`: `n = ln(plain / 0.1) / ln(10 / 0.1)`（= `log_{100}(plain/0.1)`）。
    - `norm_to_plain`: `plain = 0.1 * (10 / 0.1).powf(n)` = `0.1 * 100^n`。
    - ⚠ **完全な逆関数**であること（round-trip test 追加）。さもないと automation point が drift。
- ⚠ **`plain_to_norm` は 2 箇所に複製**がある:
  `daw_gui/src/view/arrangement_view.rs:1789-1829`（fn at 1793）。
  **両方**に GroupTransform arm を入れないと UI 表示と engine 正規化が乖離する。

### 4.5 バージョン migration（`common/src`）

- `CURRENT_VERSION`: `common/src/model.rs:84`（現 `18`）→ **`19`** に bump、doc 履歴行（62-83）追記。
- **pinning test**: `model.rs:2604-2611` が `CURRENT_VERSION == 18` を assert → `19` に更新必須。
- `common/src/project.rs:53-101` `load()` は **serde default 依存**で per-version migration table 無し。
  新 `Track` フィールド（serde default None）も append enum variant も **forward-compatible**。
  既存 `.daw` は無影響（enum 末尾追加 = forward-migrate のみ）。
- forward-compat test を `load_accepts_v4_with_default_routing_fields`（project.rs:330-362）に倣って追加。

### 4.6 daw_audio は変更不要（確認済み）

- `daw_audio/src/automation.rs:90-96` は `lane.target` match に `_ => continue`（95 行）があり
  `GroupTransform` を黙ってスキップ。`daw_audio/src/graph/compile.rs` は `AutomationTarget` を
  参照しない。**group transform は純粋に visual で、daw_gui のみが評価する**。

---

## 5. daw_gui 合成パス

### 5.0 重要: 合成パスは **2 つ**ある（preview と export、両方に group arm が要る）

プロジェクト規約上 **export は preview と byte 単位で一致**させる（`render_video.rs:462-465`）。
片方だけに group arm を入れると preview と export が無言で乖離する。

| パス | エントリ | 1 source → | レンダラ |
|---|---|---|---|
| PREVIEW | `view/runner.rs::drive_preview_playback`（1097-）→ `Vec<CompositeLayer>` → `preview_window.rs::render_placeholder`（375-464）が `TexturedQuad` 化 | `CompositeLayer` | `Renderer`（on-screen swapchain） |
| EXPORT | `render_video.rs::build_frame_scene`（466-621）が `Scene` へ直接 `push_textured_quad` | `TexturedQuad` / `GlyphArea` | `OffscreenRenderer`（CPU readback） |

両パスとも `active_{image,text,video}_sources_at` で kind 別に source を収集し、
`song.tracks.iter().rev()`（bottom track first）で 1 emitting track ＝ 1 z スロットを振る。
**`parent_group_id` は現状どの合成パスからも参照されていない**（audio / mixer のみが参照）。

### 5.1 グループ認識と frame 収集の改修

- `active_image_sources_at`（`image_compose.rs:65-140`）, `resolve_image_fields`（146-188）は
  **source トラックのレーンのみ**を見る。親グループのレーン継承の概念は無い。
- 返す frame 構造体 `ActiveImageFrame`（`image_compose.rs:21-55`）/ `ActiveTextFrame` /
  `ActiveVideoFrame` は **owning track id を持たない**。グループ単位で frame を仕分けるため、
  これらに `owning_track_id`（または `parent_group_id`）フィールドを追加する。
  → これらは daw_gui ローカル struct（IPC 非経由）なので自由に拡張可。
- 子が muted / alpha=0 で merge 前に落とされている点に注意（グループ合成は実際に active な子のみ）。

### 5.2 グループ合成（merge site の hook）

PREVIEW hook: `runner.rs:1213-1263`（image loop。z sort は現状 `let _ = ();` の no-op @1262）。
EXPORT hook: `render_video.rs:537-559`（image）, 479-532（video）, 567-620（text）。

各 merge site で:

1. active frame を `parent_group_id` で **partition**（グループ所属 / 非所属）。
2. **visual グループ（§5.6 `group_has_visual_content` を満たすグループ）**についてのみ:
   - 子 quad（位置・回転・opacity を焼き込んだ `TexturedQuad`）を **1 枚のオフスクリーンテクスチャ**
     へ合成（要 gui_01 Request A、§6）。合成キャンバスのサイズは §8 参照。
   - 合成済み 1 枚に **親 transform を CPU 側で rect に変換**して（§5.3）、
     グループの z スロットに **1 つの** `CompositeLayer` / `TexturedQuad` を push。
3. 非グループの image / video / text は従来パスのまま（1 source = 1 quad）。

### 5.3 親 transform → `TexturedQuad` への CPU 側マッピング（重要な設計確定）

合成済みテクスチャは「軸整列した矩形コンテンツ」なので、親の `T·R·S`（非一様 scale + 任意 anchor）は
**`TexturedQuad`（rect + rotation_pivot + rotation_radians）だけで完全に表現できる**。
理由: `R·S(unit square)` の像は常に「回転した矩形」であり、`TexturedQuad`（軸整列 rect を pivot 周りに
回転）も「回転した矩形」。両者は等価。具体的な対応:

- 合成キャンバスサイズ `(CW, CH)`（screen px）とする。
- **scale**: `rect.w = sx · CW`, `rect.h = sy · CH`（非一様スケールは rect の w/h に吸収）。
- **anchor → pivot**: アンカー normalized `(ax, ay)` → scale 後の rect 内 px
  `rotation_pivot = (ax · rect.w, ay · rect.h)`（rect 左上相対 px。Request B の仕様そのもの）。
- **position**: アンカー点が `pos + anchor` のスクリーン位置に来るよう
  `rect.x = anchor_screen_x + pos_screen_x − ax · rect.w`（y も同様）。
- **rotation**: `rotation_radians = rot`（pivot 周り）。
- **opacity**: group opacity（導入する場合）→ グループ quad の `alpha`。

→ **レンダラ側に必要な transform 追加は「任意 pivot 回転」のみ**（Request B）。非一様 scale・
位置・アンカー位置決めはすべて daw_gui の CPU 側 rect 計算で処理する。

### 5.4 選択オーバーレイ（UI）

`runner.rs:1290-1311` の selection overlay は content-kind match（`image_events()` else
`text_events()`）。グループトラックは clip/event を持たない transform-only ノードなので、
**グループ選択用の新 branch**（合成 bounding box + anchor ハンドルを描く）が要る。

### 5.5 automation lane 作成 UI（track inspector）— 「触れる対象」問題の解決

⚠ グループトラックには触れるフェーダーが無いが、これは **image inspector が既に解決済みの問題**。
image トラックもフェーダーを持たず、`track_inspector.rs:505-580` で各 field（X/Y/W/H/Opacity/Rotation）を
**「ラベル + 数値入力 + 横の "A" トグルボタン」** の行で描く。「A」トグルが
`AddImageAutomationLane{field}` / `RemoveImageAutomationLane{field}`（`app.rs:2763` AppEvent →
`add_image_automation_lane` `app.rs:7452`）で lane を**直接作成/削除**する。lane 有無で点灯
（`summary.x_automated`, `app.rs:1721-1736`）。グローバル touch+A（`last_touched_param` → `A` キー
shortcut, `app.rs:7934`）も併存するが、フェーダー無し param では専用「A」トグルが主経路。

→ グループ transform も同 idiom。**visual グループ（§5.6 の `group_has_visual_content` を満たす
グループ）選択時のみ**、inspector に Group Transform セクションを描く（純 audio バスには出さない）:

```
Group Transform
  X        [ 0.50 ] [A]      ← 数値入力 = group_transform.x を編集（触れる対象）
  Y        [ 0.50 ] [A]         「A」= GroupTransform(param) lane を直接作成/削除
  Rotation [ 0.0° ] [A]
  ScaleX   [ 1.00 ] [A]
  ScaleY   [ 1.00 ] [A]
  AnchorX  [ 0.50 ] [A]      ← preview 上ドラッグでも編集
  AnchorY  [ 0.50 ] [A]
  Opacity  [ 1.00 ] [A]      ← 立ち絵全体のフェード in/out（quad alpha）
```

- 各数値フィールド = `Track.group_transform.<field>`（静的基準値）の編集 = 「触れるもの」。
  編集で `last_touched_param = GroupTransform(param)` も更新（touch+A も効く）。
- 各「A」トグル = 新規 AppEvent `AddGroupAutomationLane{param}` / `RemoveGroupAutomationLane{param}`
  （image の event を mirror）で `GroupTransform(param)` lane を直接作成/削除。
- preview 上のアンカー/位置ドラッグ → 同 lane へ書込（image の `seed_image_automation_*`
  `app.rs:7575, 7617` を mirror）。
- → 「A キーで触れる対象が無い」問題は発生しない（image と同じ専用「A」トグル方式）。

### 5.6 visual グループ判定（audio グループとの区別）— ① 派生判定（2026-05-31 確定）

グループは派生（`parent_group_id` を誰かが指せばグループ。`compile.rs:72`, `app.rs:1346`
`is_group_track`）で audio/visual の型区別を持たない。SSoT を維持したまま「audio 専用サブミックスに
立ち絵 transform を出さない」ため、**visual グループか否かを中身から派生判定**する（新フィールド無し）:

```
fn group_has_visual_content(&self, group_track_id: u32) -> bool
  = group の subtree（parent_group_id 再帰、`group_descendant_ids` app.rs:6191 を再利用）に
    image/video/text クリップを持つ track が 1 つでもあれば true、
    または group 自身が既に group_transform データ（非 default base or GroupTransform lane）を持つ。
```

gate site（すべて同述語を SSoT で共有）:
- **inspector（§5.5）**: `is_group_track(id) && group_has_visual_content(id)` の時のみ Group Transform
  セクション表示。純 audio バス（ドラムバス等）は通常の bus/mixer コントロールのみで transform は出ない。
- **合成パス（§5.1-5.2）**: 述語を満たすグループのみオフスクリーン合成（視覚子が無ければ frame が
  0 個 → 自動 no-op だが、partition 段で同述語を使い無駄な target alloc を避ける）。
- **選択オーバーレイ（§5.4）**: visual グループ選択時のみ bounding box + anchor ハンドルを描く。

判定は毎フレーム計算可能な軽量派生（track 数 × subtree、立ち絵規模では無視できる）。重ければ
view 側で 1 frame キャッシュ（gui_01 immediate-mode の定石）。立ち絵パーツは無音なので
audio バス併存は無害（§8.2 検証済み）、arrangement tree も `parent_group_id` の 1 本のまま
立ち絵グループ構造がそのまま timeline に見える。

---

## 6. gui_01 要望（docs/gui_01_conversation.md へ提出。最終形態を伝える）

採番: 現行 live 最大 `#060`、archive 最大 `#062` → 新規は **#063 / #064**。
両エントリに `関連仕様: docs/plan_tachie_group_transform.md` を必須付与（`feedback_gui_01_link_plan_ref`）。

### ⚠ 前セッションの想定からの scope 修正（recon で判明）

前セッションでは要望①を「`create_offscreen_target(w,h)` の薄いラッパ」としていたが、調査の結果
**それだけでは daw_gui から使えない**。`create_offscreen_target` は `TextureView` を返すが、daw_gui には
**その view へ Scene を描き込む public API が無い**（render pass の pipeline / bind group / sampler は
すべて renderer 内部。`Renderer::render` は swapchain 専用、`OffscreenRenderer::render_to_rgba` は
CPU readback）。compose-into-target 能力は現状 `TextEffectCompositor` 内に閉じている。
→ 要望①を **「Scene を GPU 常駐の sampleable テクスチャへ合成する高レベル primitive」** に格上げする。

### 要望 #063 — Request A: Scene → offscreen texture 合成 primitive

最終形態として、daw_gui が「子 quad 群を組んだ `Scene` を渡すと、合成済みの **GPU 常駐
sampleable `TextureHandle`** が返る」public メソッドが欲しい。`Renderer<W>`（preview）と
`OffscreenRenderer`（export）の **両方**に。想定シグネチャ:

```rust
// Renderer<W> と OffscreenRenderer 双方
pub fn composite_scene_to_texture(
    &mut self, scene: &Scene, width: u32, height: u32,
) -> Result<TextureHandle, RenderError>;
```

- 内部実装は `text_effect.rs` の前例（`texture_store.create_render_target` で
  `TEXTURE_BINDING | RENDER_ATTACHMENT` target → `begin_render_pass`（`LoadOp::Clear(TRANSPARENT)` /
  `StoreOp::Store`）→ rect/line/glyph/texture run を draw → 返した handle を sample）をそのまま流用可能。
- format は `Rgba8UnormSrgb`（`create_texture` / text_effect OFFSCREEN_FORMAT と一致、sRGB 正しい blend）。
- **realtime 制約**: preview で毎フレーム呼ばれる（60fps）。毎フレーム target を alloc/destroy するのは
  無駄。**サイズキーでの内部キャッシュ**（target テクスチャの使い回し）を gui_01 側で実装してほしい
  （SSoT で renderer がライフサイクルを所有するのが理想）。daw_gui 側でキャッシュを二重持ちしたくない。
- 低レベル `create_render_target` を public 化（`create_offscreen_target`）するかは gui_01 にお任せ。
  daw_gui が必要なのは上記の高レベル composite メソッド。

### 要望 #064 — Request B: `TexturedQuad` 任意アンカー回転 pivot

現状 `texture.wgsl`（63-73）は回転中心が rect 中心固定
（`cx = left + w*0.5, cy = top + h*0.5`）。グループ quad を任意アンカー周りに回転させたい。

最終形態:

```rust
// scene.rs TexturedQuad に追加
pub rotation_pivot: Option<(f32, f32)>, // rect 左上相対 px。None = 中心 (w/2, h/2)
```

- **default は rect 中心**（Phase 76 byte 互換維持。素朴な `f32 = 0.0` だと既存の回転 quad の
  pivot が左上に飛ぶ silent regression）。`Option<(f32,f32)>` で `None = 中心` が安全。
- 配線（gui_01 側、参考）: `texture.rs::enqueue_run`（250-270）の `misc` に **空きスロットが 2 つ**
  ある（`misc: [alpha, theta, 0.0, 0.0]`）ので `misc.z/.w` に pivot を packing すれば
  **新 vertex attribute も stride 変更も不要**。`texture.wgsl` 67-68 を
  `cx = left + in.misc.z; cy = top + in.misc.w;` に変更。
- `TexturedQuad::new`（scene.rs:326）と test（scene.rs:529-541）の更新が要る点を申し添える。
- `GlyphArea` も `rotation_radians`（center pivot）を持つ。本機能は `TexturedQuad` 経路のみで足りるが、
  parity を取るなら GlyphArea にも pivot を入れてよい（任意）。

---

## 7. 影響ファイル一覧（行番号は recon で確認）

### common（IPC 型 → `cargo build --workspace`）
- `common/src/model.rs`: `GroupTransform` struct 新規、`Track.group_transform` 追加（color 後 ~1088）、
  `Track::default`（~1158）、`AutomationTarget::GroupTransform` + `GroupTransformParam`（2092-2175）、
  `CURRENT_VERSION` 18→19（84）、pinning test（2604-2611）。
- `common/src/automation.rs`: `plain_to_norm`（36-66）/ `norm_to_plain`（70-96）に GroupTransform arm。
- `common/src/project.rs`: forward-compat test 追加（330-362 に倣う）。migration code 自体は不要。

### daw_gui — 合成パス
- `daw_gui/src/image_compose.rs`: `ActiveImageFrame` に owning_track_id（21-55）、
  グループ partition ロジック（65-140）。`resolve_image_fields` は子レーン用のまま（146-188）。
- `daw_gui/src/view/runner.rs`: PREVIEW merge（image 1213-1263 / video 1180-1207 / text 1269-1311）に
  group 合成 hook、selection overlay（1290-1311）に group branch、z sort no-op（1262）見直し。
- `daw_gui/src/view/preview_window.rs`: `CompositeLayer`（109-124）に pivot/scale 拡張、
  `render_placeholder`（375-464）の `TexturedQuad` 組立に pivot + group rect。
- `daw_gui/src/render_video.rs`: `build_frame_scene`（466-621）に preview と同じ group arm（export parity）。
- （video/text frame 構造体も owning_track_id 追加で parity）。

### daw_gui — UI
- `daw_gui/src/app.rs`: `automation_target_display_name`（lane label, 367-414）に GroupTransform arm、
  新規派生述語 `group_has_visual_content`（§5.6。`group_descendant_ids` 6191 を再利用）、
  新規 AppEvent `AddGroupAutomationLane{param}` / `RemoveGroupAutomationLane{param}`
  （`AddImageAutomationLane` `app.rs:2763` / `add_image_automation_lane` `7452` を mirror）、
  `group_transform` 数値編集 + `last_touched_param` 更新、preview drag seed
  （`seed_image_automation_*` `7575/7617` を mirror）、`lane_default_for_target`（8042-8068）に
  GroupTransform arm。他 AutomationTarget 参照箇所の監査（7445-7627 / 8068 / 12742-12765,
  image_compose 152-172, text_compose 167-171, mixer_strips, transport）。
- `daw_gui/src/view/arrangement_view.rs`: `lane_display`（1721-1787, 非網羅 match → arm 必須）、
  **複製 `plain_to_norm`（1789-1829）**、automation lane 表示、preview 上のアンカー/transform ドラッグ。
- `daw_gui/src/view/track_inspector.rs`: グループトラック選択時の Group Transform セクション
  （§5.5。各 param = 数値入力 + 「A」トグル。image inspector `505-580` を mirror）。

### gui_01（別 session、要望提出のみ）
- `crates/renderer/src/device.rs`（Renderer）, `offscreen.rs`（OffscreenRenderer）: Request A。
- `crates/renderer/src/scene.rs`（TexturedQuad）, `pipelines/texture.rs`, `pipelines/texture.wgsl`: Request B。

---

## 8. 制約・設計上の注意

### 8.1 アプローチ X の構造的制約（立ち絵では非問題）
- **z が 1 枚に潰れる**: グループ内部に外部 source（背景動画等）を z 的に挟めない。立ち絵は
  パーツ群が 1 つの被写体なので問題にならない。グループは song.tracks 順での 1 z スロットを占める。
- **合成解像度**: 親拡大時のボケを避けるには合成キャンバスを十分大きく確保する必要。
  - 案 A（簡素）: 合成キャンバス = project resolution。親 scale>1 で resample（ボケ）。Phase 1 はこれ。
  - 案 B（高品質）: 子の bounding box union サイズ（テクセル密度を稼ぐ）or project_res × supersample。
  - → Phase 1 は案 A、supersample は tunable な後続改善（§9 Phase 5）。

### 8.2 audio reuse が無害である検証（daw_audio 調査済み）
- グループの役割は **派生**（`Track::kind` は v16 で廃止。あるトラックを別トラックが
  `parent_group_id` で指せばそれがグループ）。`compile.rs:62-86` が `is_group` HashSet と
  `children_of` map を構築。
- 無音 image トラックを `parent_group_id` でぶら下げると:
  - `compile.rs:224-263`: グループは `Mix(children unity-gain → group scratch)` + `ProcessGroupFx`。
  - `engine.rs:1742-1755`: `mix_into_track_scratch` は dst を **zero-fill してから**加算 → 無音確定。
  - `engine.rs:1114-1133`: 子を持つトラックは自前 clip/instrument を **force-silence**（has_children 短絡）。
  - `engine.rs:1965-2024`: 空 fx_chain の `ProcessGroupFx` は 0 回ループ + strip を無音に適用。alloc/crash 無し。
  - `compile.rs:554-576`: PDC は無音子（latency 0）を max に入れるだけで 0 寄与。
  - **結論**: 「空 sub-mix ができるだけで害なし」は **検証済み**。dropout/NaN/追加コスト無し。
- **唯一の前提**: `parent_group_id` は **存在するトラック id** を指し、**cycle を作らない**こと。
  違反すると `compile.rs` が `DanglingReference` / `Cycle` を返し、`engine.rs:388-398` が
  `Schedule::empty()` を入れて **master 全体が無音**になる。既存の group 作成/reparent mutator
  （app.rs 8203-8353 / 8430-8445, arrangement_view drag 1269-1283）が既にこの不変条件を維持しているので、
  visual transform 側もこの mutator 群を通す限り安全。
- **coupling point**: 「visual グループ親」になったトラックは派生規則上 **audio グループ bus** 扱いで
  自前 audio clip が無音化される（engine.rs:1123-1133）。無音 image トラックでは no-op だが、
  「自前 audio を出しつつ visual 親」を将来やるなら has_children 短絡の見直しが要る。

### 8.3 同期・正規化の落とし穴
- **preview と export の 2 実装を同期**: group arm を片方だけに入れると無言で乖離。
- **`plain_to_norm` の 2 複製**（common + arrangement_view）を両方更新。
- **scale log 正規化の逆関数**性（round-trip test）。

---

## 9. 実装フェーズ

> **進捗 (2026-05-31)**:
> - Phase 1 ✅ gui_01 `#063`(composite_scene_to_texture) / `#064`(rotation_pivot) **landing 済 (Resolved)**。
>   実装: 両 method は `Renderer`/`OffscreenRenderer`、`rotation_pivot: Option<(f32,f32)>` (None=中心)。
> - Phase 2 ✅ common model + daw_gui match 4 箇所 wire。CURRENT_VERSION 19。
> - Phase 3 ✅ **preview 合成パス** — `group_compose.rs`(resolve + affine→quad math + types)、
>   `image_compose` owning_track_id、`preview_window` group_layers 合成（`composite_scene_to_texture`
>   → 親 affine quad）、`runner` 子の group partition。既存 3 TexturedQuad に `rotation_pivot:None`。
> - Phase 4 ✅ **export parity** — `render_video.rs build_frame_scene` に同 partition +
>   `OffscreenRenderer.composite_scene_to_texture`（preview と同一ロジック）。
> - Phase 5 ✅ **UI** — track_inspector の Group Transform セクション（8 param 数値 + 「A」トグル、
>   image inspector mirror）、AppEvent `AddGroupAutomationLane`/`Remove`/`GroupTransformEditChanged`/
>   `CommitGroupTransformEdit`/`ResyncGroupEditBuffers`、`group_has_visual_content` 派生 gate(§5.6)、
>   `inspector_group_transform_summary`、commit/resync/set_field handler。
> - **検証**: workspace build / clippy -D warnings / common 191 + daw_gui 102 tests（group_compose
>   affine 4 件含む）/ **video preview smoke test PASSED**（既存パス非回帰、IPC v19 protocol OK）。**全て未 commit。**
> - 残: preview 上のアンカー/transform ドラッグ + group 選択オーバーレイ（数値編集で代替可、後続）、
>   実 立ち絵グループでの目視確認（手動）、合成キャンバス supersample（§8.1 案 B）。

1. **gui_01 要望提出**（#063 Request A / #064 Request B）。`docs/gui_01_conversation.md`。
   interim 実装に走らない（`feedback_gui_01_request_before_interim`）。landing は diagnostic 自動検知。
2. **common モデル**: `GroupTransform` / `Track.group_transform` / `AutomationTarget::GroupTransform` /
   `GroupTransformParam` / automation 正規化（両複製）/ CURRENT_VERSION 19 / pinning + forward-compat test。
   `cargo build --workspace` + `cargo test`。
3. **daw_gui 合成パス（preview）**: frame struct owning_track_id、グループ partition、
   gui_01 Request A で子合成 → Request B 付き 1 quad push。`runner.rs` / `preview_window.rs`。
   smoke test（立ち絵グループ fixture）で視認。
4. **daw_gui 合成パス（export parity）**: `render_video.rs` に同 group arm。preview と一致確認。
5. **UI**: automation lane（app.rs / arrangement_view.rs。非網羅 match を順次 wire）、
   track_inspector の transform 数値編集、preview 上のアンカー/transform ドラッグ、選択オーバーレイ。
6. **品質改善**: 合成キャンバス supersample（案 B、§8.1）。

各フェーズ後 `cargo build --workspace` / `cargo clippy --workspace -- -D warnings` / `cargo test`、
commit 前に `cargo run -p daw_gui` smoke。

---

## 10. 一次情報（引用）

- After Effects レイヤープロパティ / アンカー / 親子付け:
  https://helpx.adobe.com/after-effects/using/layer-properties.html
- After Effects 非一様スケール × 子回転の skew（「補正する直接 transform は存在しない」）:
  https://community.adobe.com/t5/after-effects-discussions/child-layers-distorted/td-p/10652220
- AviUtl 拡張編集 グループ制御:
  https://aviutl.info/guru-puseigyo/
- Live2D 親子関係システム:
  https://docs.live2d.com/en/cubism-editor-manual/system-of-parent-child-relation/
- アフィン変換行列（T·R·S 合成、列ベクトル左乗算）:
  https://en.wikipedia.org/wiki/Transformation_matrix
- scene graph（トップダウン world 行列伝播）:
  https://learnopengl.com/Guest-Articles/2021/Scene/Scene-Graph

### daw_01 / gui_01 ソース根拠（recon 確認済み）
- daw_01: `common/src/model.rs:84`(CURRENT_VERSION), `1043`(parent_group_id), `1875-1916`(ImageEvent),
  `2092-2175`(AutomationTarget / ImageBuiltinParam), `2604-2611`(version pinning test);
  `common/src/automation.rs:36-96`(plain_to_norm/norm_to_plain); `common/src/project.rs:53-101,330-362`;
  `daw_gui/src/image_compose.rs:21-55,65-140,146-188`; `daw_gui/src/view/runner.rs:1097-1311`;
  `daw_gui/src/view/preview_window.rs:109-124,375-464`; `daw_gui/src/render_video.rs:466-621`;
  `daw_gui/src/app.rs:367-414`; `daw_gui/src/view/arrangement_view.rs:1721-1829`;
  `daw_audio/src/graph/compile.rs:62-263,554-576`; `daw_audio/src/engine.rs:388-398,1114-1133,1742-1755,1965-2024`;
  `daw_audio/src/automation.rs:90-96`.
- gui_01: `crates/renderer/src/texture_store.rs:182-241`(create_render_target);
  `crates/renderer/src/scene.rs:302-337`(TexturedQuad); `crates/renderer/src/device.rs:192-542`(Renderer);
  `crates/renderer/src/offscreen.rs:118-188`(OffscreenRenderer);
  `crates/renderer/src/pipelines/texture.rs:31-43,194-270`; `crates/renderer/src/pipelines/texture.wgsl:63-82`;
  `crates/renderer/src/pipelines/text_effect.rs:52,471-491,547-589,717-736,948-957`(render-to-texture 前例).

---

## 11. 確認したい点（実装前にユーザー判断が要る）

1. **gui_01 Request A の scope 格上げ**（§6）: 前セッション想定の `create_offscreen_target` 単体では
   daw_gui から描き込めないため、「Scene → 合成済み sampleable texture」の高レベル primitive
   `composite_scene_to_texture` として要望する。
   → ✅ **確定（2026-05-31, ユーザー承認）**: 格上げで進める。`create_offscreen_target` を
   内部に持つかは gui_01 に一任。
2. **visual グループ判定 / audio グループとの区別**（§5.6）: グループは派生で型区別が無いため、
   純 audio バスにも transform UI が出る問題。
   → ✅ **確定（2026-05-31, ユーザー選択）**: ① 派生判定（`group_has_visual_content`、SSoT 維持、
   新フィールド無し）。image/video/text 子孫を持つグループのみ transform 表示・合成対象。
3. **group opacity**: AE の Transform プロパティ群（Anchor/Position/Scale/Rotation/**Opacity**）と同様、
   `GroupTransform.opacity`（0..1, default 1.0）＋ `GroupTransformParam::Opacity` を持つ。合成時は行列に
   乗せずグループ quad の `alpha` に適用。子 opacity は合成前に各子 quad へ焼き込む。
   → ✅ **確定（2026-05-31, ユーザー承認）**: Phase 1 から含める。
4. **合成キャンバス解像度**（§8.1）: Phase 1 は案 A（project resolution で合成、親拡大はボケ許容）で
   進め、supersample（案 B）は後続 Phase で良いか。
   → ⏳ **未確定**（推奨: Phase 1 = 案 A、supersample は後続）。
