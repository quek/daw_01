# plan: ピアノロール縦グリッドをスナップ値に一致させる (FIXME #38)

## ゴール

MIDI エディタ (ピアノロール) の縦グリッド線を、現在のスナップ値に一致させる。
現状の縦線は **小節線 + 拍線の 2 段固定** (`bar_beat_grid`) で、スナップを 1/16 等に
しても拍より細かい線は出ない。これを **小節線 (最も濃) > 拍線 (中) > スナップ細分線
(最も淡)** の 3 段にし、3 段目をスナップ値に追従させる。

対象は **縦 (時間軸) の線のみ**。横 (音程方向) は本件対象外で既存の白鍵/黒鍵レーン
背景のまま。適用範囲は **ピアノロールのみ**。アレンジビューの `bar_beat_grid` は本件で
変更しない (FIXME は MIDI エディタ限定)。

## 確定設計 (インタビュー済、再議論しない)

- 修正の主体は **gui_01 のグリッド widget** を拡張する側。daw_01 は現在のスナップ値を
  descriptor に変換して widget に渡す配線を担う。よって本 plan は
  **(A) gui_01 要望仕様** + **(B) daw_01 側の受け渡し配線** に分けて書く。
- 線の段階は 3 段。スナップ細分線は **「拍より細かいスナップ (1/8, 1/16, 1/32, …) の
  ときだけ」追加** する (= 追加方式。小節/拍線を置き換えない)。スナップ OFF や
  1/4・小節など **拍以上に粗いスナップ** では 3 段目を足さず現状 (小節+拍) のまま。
- スナップ種別ごとの線:
  - **直線 (1/2..1/128)** → その細分の直線格子。
  - **三連 (T)** → 三連格子 (literal。直線格子では表せないので独立に等分)。
  - **付点 (.)** → その付点が乗る「内包する直線格子」を出す (1/8. → 1/16 線、
    1/16. → 1/32 線、すなわち base 直線分割の **2 倍密度**)。
  - **Adaptive** → ズームに応じた密度 (`SnapConfig::beat_unit` が選ぶ unit をそのまま使う)。
- **ズームアウトで細線がピクセル上で詰まりすぎたら、その段は自動で消える** (1 段粗い
  格子に落ちる)。最小ピクセル間隔の閾値を設ける (既存 `min_beat_line_px` と同方式)。

---

## ★ 設計補正: descriptor は `divisions_per_beat` でなく `interval_beats`

下記 (A)(B) は agent が `divisions_per_beat` (1 拍あたり分割数) で設計しているが、
これは **1/4T (四分三連 = 0.667 拍間隔 = 1 拍 1.5 本) が非整数で表現できず**、回避策
`div*3/4` は密度を 2 倍に誤る (1/4T を 1/8T 相当で描く) バグになる。**実装では以下の
`interval_beats` モデルを採用する** (下記 (A)(B) の divisions 記述はこれで読み替える)。

- **descriptor = `interval_beats: f64`** (subgrid 線の間隔, 拍単位)。widget は小節原点
  からの倍数位置 `m * interval_beats` (m=1,2,…) に線を打ち、**拍/小節線と一致する位置は
  スキップ** (重複描画回避)。`min_sub_line_px` 退避と min-gap 間引きは
  `px_per_interval = px_per_beat * interval_beats` で判定。
- **「拍より細かいときだけ追加」= `interval_beats < 1.0` のときだけ `Some`**。
  `>= 1.0` (1/4, 1/2, 1bar, 1/2T=1.333 拍 等) は `None` で 3 段目なし。
  → 1/2T が subgrid 無しなのは**このルールの自然な帰結**で妥協ではない。
- **daw_01 の写像 (`piano_roll_subgrid_divisions` 改め `piano_roll_subgrid_interval`)**:
  - Straight / Triplet / Adaptive → `interval = cfg.beat_unit(zoom)` (snap unit をそのまま)。
    例: 1/16→0.25, 1/8T→0.333, 1/4T→0.667, 1/2T→1.333(→None)。`match` 分岐不要。
  - Dotted{div} → **内包する直線格子** `interval = 2.0 / div` 拍 (1/8.→0.25=1/16線,
    1/16.→0.125=1/32線)。付点だけ beat_unit でなくこの写像。
  - `interval < 1.0 - 1e-6` のときだけ `Some(interval)`、それ以外 `None`。
- gui_01 `SubGridSpec` は `divisions_per_beat` を `interval_beats` に置換。`PianoRollView`
  の `sub_grid_divisions: Option<f64>` も `sub_grid_interval_beats: Option<f64>` に。
- テスト期待値は間隔ベースに読み替え (1/16→0.25, 1/8T→0.333, 1/4T→0.667(描画される),
  1/2T→None, 1/8.→0.25)。1/4T が**正しく描画される**ことを必ず回帰に入れる。

---

## (A) gui_01 要望仕様

> 提出先: `docs/gui_01_conversation.md` に `[要望]` エントリとして追記する。
> `関連仕様: daw_01/docs/plan_pianoroll_snap_grid.md` を必須で含める。
> 段階分割せず最終形態を全部書く (v1/v2 に割らない)。

### A-1. descriptor 型の新設 (`crates/ui/src/widgets/time_grid.rs`)

`bar_beat_grid` に「拍をさらに細分する 3 段目の格子」を表す descriptor を渡す。
スナップ種別 (直線/三連/付点/Adaptive) を gui_01 側で **再分類しない** よう、daw_01 が
解決済みの「1 拍を何分割するか + 等分の起点」を **prescriptive な値** として渡す形にする
(役割判定をライブラリに作らない方針)。

```rust
/// `bar_beat_grid` に渡す「拍をさらに細分する 3 段目 (subdivision) 格子」記述子。
/// 拍 (beat) を等間隔に `divisions_per_beat` 本で分割した縦線を追加する。
/// 直線/三連/付点の区別はここに持ち込まず、 caller が解決した「1 拍あたりの分割数」
/// だけを受け取る (= literal な等分指示)。 `None` で 3 段目なし (= 現状の小節+拍のみ)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SubGridSpec {
    /// 1 拍を何等分するか。 例: 1/8 snap → 2、 1/16 → 4、 1/16. (付点) → 8、
    /// 1/8T (三連) → 3。 `0` / `1` 以下は「拍と同密度以下」 = 3 段目不要なので
    /// caller 側で `None` にする想定 (defensive に `<= 1` は描画 skip)。
    pub divisions_per_beat: f64,
    /// subdivision 線の色 (最も淡い段)。
    pub color: Color,
    /// subdivision 線の太さ (px)。
    pub line_width: f32,
}
```

`BarBeatGridStyle` (現 `:64-84`) に subdivision 用の閾値を 1 つ追加する (既存 default
構築 `..BarBeatGridStyle::default()` の caller は無修正で済む、非破壊):

```rust
pub struct BarBeatGridStyle {
    pub bar_color: Color,
    pub beat_color: Color,
    pub bar_line_width: f32,
    pub beat_line_width: f32,
    pub min_beat_line_px: f32,
    /// subdivision (3 段目) 線を描画する最小 1 subdivision 表示幅 (px)。
    /// `px_per_subdivision < min_sub_line_px` ならその frame は subdivision を
    /// 描かず 2 段 (bar+beat) に落ちる。 0.0 以下なら常に描画。 default 6.0
    /// (beat 線より少し広めにして、 細密スナップで線が潰れる前に消す)。
    pub min_sub_line_px: f32,
}
```

`Default` は既存値 + `min_sub_line_px: 6.0`。

### A-2. `bar_beat_grid` シグネチャ拡張 (`time_grid.rs:237`)

`sub` 引数を末尾に **追加** する (非破壊にしたいので `Option<SubGridSpec>`)。
アレンジビュー側の既存 caller は `None` を渡すだけ。

```rust
pub fn bar_beat_grid(
    &mut self,
    id: impl Hash,
    rect: Rect,
    mapping: TimeMapping,
    viewport: ViewportState1D,
    style: BarBeatGridStyle,
    sub: Option<SubGridSpec>,   // ← 追加
) {
```

### A-3. 描画ロジック (`time_grid.rs:254-309` の `with_widget_node` クロージャ)

現状は `bar_segs` / `beat_segs` の 2 本を `beat_index_start..=beat_index_end` の
整数 beat ループで作っている (`:268-294`)。ここに **subdivision ループ** を足す。

実装方針:
1. `input_hash` (現 `:246-253`) に `sub` を含める (`divisions_per_beat.to_bits()`,
   `line_width.to_bits()`, color の各成分 bits)。`sub` が変われば再描画が必要。
2. `px_per_beat` (現 `:264`) を流用し、`sub` が `Some` のとき
   `px_per_sub = px_per_beat / divisions_per_beat as f32` を計算。
   `divisions_per_beat <= 1.0` または `px_per_sub < style.min_sub_line_px` なら
   subdivision は描かない (= 自動で 2 段に落ちる、ズームアウト退避)。
3. subdivision 線は **beat 境界・bar 境界と重なる位置には描かない** (重複描画と
   濃度の二重がけを避ける)。`beat_index_start..=beat_index_end` の各 beat について
   `k in 1..divisions` の内側点 `s = (bi + k/divisions) * spb` を打つ
   (`k = 0` は beat 線そのものなので除外)。`divisions` は
   `divisions_per_beat.round() as i64`。三連 (3) や付点格子 (8 等) も整数等分なので
   このループで literal に表現できる。
4. subdivision 線は `bar_segs`/`beat_segs` より **先に** push する (z 順で下、最も淡い
   段が最背面)。現状の push 順は `beat_segs` (`:295`) → `bar_segs` (`:302`)。
   subdivision は **その前** に push する:

```rust
// (sub があり、 px_per_sub が閾値以上のときだけ)
let mut sub_segs: Vec<LineSegment> = Vec::new();
if let Some(spec) = sub {
    let divisions = spec.divisions_per_beat.round() as i64;
    let px_per_sub = px_per_beat / spec.divisions_per_beat as f32;
    if divisions > 1
        && (style.min_sub_line_px <= 0.0 || px_per_sub >= style.min_sub_line_px)
    {
        for bi in beat_index_start..=beat_index_end {
            for k in 1..divisions {
                let s = (bi as f64 + k as f64 / divisions as f64) * spb;
                if s < viewport.view_start || s > view_end { continue; }
                let local_x = viewport.unit_to_px(s, rect.w);
                let x = rect.x + local_x;
                if x < rect.x || x > rect.x + rect.w { continue; }
                sub_segs.push(LineSegment {
                    a: [x, rect.y],
                    b: [x, rect.y + rect.h],
                    color: spec.color,
                });
            }
        }
    }
}
if !sub_segs.is_empty() {
    ui.push_lines(LineBatch {
        segments: sub_segs.into(),
        line_width_px: spec.line_width,   // spec を上の if 外に持ち上げて保持
        clip_rect: Some(rect),
    });
}
// 既存: beat_segs → bar_segs の push はそのまま (subdivision の後)
```

> 注: `spec.line_width` を push 時に使うため、`spec` は `Option` の外で保持するか
> `sub_segs` を作る if の戻り値で line_width を持ち回ること。

### A-4. 既存テスト + 新規テスト

- 既存 `grid_segment_counts` ヘルパ (`time_grid.rs:402`) は `bar_beat_grid` を 5 引数で
  呼んでいる。第 6 引数 `None` を足す (非破壊呼び出しの確認)。
- 新規 test:
  - `sub = Some { divisions_per_beat: 4.0 }` で 1 bar ズーム時に subdivision 線が
    `beats * 3` 本程度出る (4 等分 = beat 内側に 3 本)。
  - `px_per_sub < min_sub_line_px` のズームアウトで subdivision が 0 本に落ち、
    bar/beat 線は残る (自動退避)。
  - `divisions_per_beat <= 1.0` で subdivision 0 本 (= 拍以上の粗さでは足さない)。
  - 三連 `divisions_per_beat: 3.0` で beat 内側に 2 本 (3 等分) 出る。

### A-5. アレンジビュー呼び出しは無改修

`arrangement.rs` から `bar_beat_grid` を呼んでいる箇所は **第 6 引数 `None`** を渡す
だけにする (本件はピアノロール限定)。これは要望本文にも明記する。

---

## (B) daw_01 側の受け渡し配線

> gui_01 の上記 API が landing したら着手する。landing 前は parked。
> diagnostic (non-exhaustive な引数不足エラー = `bar_beat_grid` の arity 変化) が
> 出たら通知を待たず wire を開始する。

### B-1. スナップ choice → `SubGridSpec` の descriptor 変換 (`daw_gui/src/view/snap.rs`)

`SNAP_LABELS` (`:19-25`) / `choice_to_mode` (`:32-56`) は既存。ここに **スナップ値から
「1 拍あたりの subdivision 分割数」を返す純関数** を新設する。SSoT は
`SnapMode`/`SnapConfig::beat_unit` 側に寄せ、daw_01 は unit (拍長) から分割数を導出する。

設計の核: **subdivision 分割数 = 1 拍 (= 1.0 beat) を snap unit で割った商**。
ただし「拍より細かいスナップのときだけ」なので `unit < 1.0 beat` (= 分割数 > 1) の
ときのみ `Some` を返す。

```rust
/// 現在の snap 設定から、 ピアノロール 3 段目グリッドの「1 拍あたり分割数」を返す。
/// `None` = subdivision なし (snap OFF / 1/4・1/2・bar など拍以上に粗い)。
/// `zoom_x_px_per_beat` は Adaptive の unit 解決に必要 (snap.rs の beat_unit と同じ)。
pub fn piano_roll_subgrid_divisions(app: &AppData, zoom_x_px_per_beat: f32) -> Option<f64> {
    let cfg = piano_roll_snap_config(app);          // 既存 :86-93
    // snap 無効 (alt は描画では常に false) なら subdivision なし。
    let unit = cfg.beat_unit(zoom_x_px_per_beat)?;  // 拍単位の 1 unit 長 (snap.rs:100)
    // 1 拍 = 1.0 beat。 unit が 1 拍以上 (= 1/4, 1/2, 1bar, Adaptive で粗いとき) は
    // 拍線で足りるので 3 段目を足さない。
    let divisions = 1.0 / unit;
    if divisions > 1.0 + 1e-6 {
        Some(divisions)
    } else {
        None
    }
}
```

この 1 関数で **全種別が literal に解決される** ことの確認 (再分類不要):
- 直線 1/8 → `beat_unit = 0.5` → `divisions = 2.0` (beat を 2 等分)。
- 直線 1/16 → `0.25` → `4.0`。直線 1/32 → `0.125` → `8.0`。
- 三連 1/8T → `(8/3)/8 = 0.3333…` → `divisions = 3.0` (beat を 3 等分 = 三連格子 literal)。
- 三連 1/16T → `(8/3)/16 = 0.1667` → `6.0`。
- 付点 1/8. → `6/8 = 0.75` → `divisions = 1.333…`。

> ⚠ 付点の落とし穴 (要設計判断、notes 参照): 付点 1/8. の `beat_unit = 0.75 beat` を
> そのまま `1/unit` すると `divisions ≈ 1.333` で **整数等分にならない**。確定設計は
> 「付点は **内包する直線格子** を出す (1/8. → 1/16 線、1/16. → 1/32 線)」なので、
> 付点のときは unit 由来でなく **base 直線分割の 2 倍密度** を返す必要がある。
> よって付点は `beat_unit` 経由でなく `cfg.mode` を直接見て分岐する:

```rust
use daw_ui_core::SnapMode;

pub fn piano_roll_subgrid_divisions(app: &AppData, zoom_x_px_per_beat: f32) -> Option<f64> {
    let cfg = piano_roll_snap_config(app);
    if !cfg.is_active(false) { return None; }
    let divisions = match cfg.mode {
        // 付点は「内包する直線格子」: 1/N. → 1/(2N) 線 = base 直線 (4/N 拍) の
        // 2 倍密度。 1 拍 / (4/N / 2) = N/2。 例 1/8.(div=8) → 4.0 (= 1/16 線)。
        SnapMode::Dotted { div } => f64::from(div) / 2.0,
        // それ以外 (Straight / Triplet / Adaptive) は unit から literal 等分。
        // Bars / Off は beat_unit が拍以上 or None なので下の閾値で弾かれる。
        _ => {
            let unit = cfg.beat_unit(zoom_x_px_per_beat)?;
            1.0 / unit
        }
    };
    if divisions > 1.0 + 1e-6 { Some(divisions) } else { None }
}
```

付点の確認: 1/4.(`div=4`) → `4/2 = 2.0` (= 1/8 線、内包する直線格子)。
1/8.(`div=8`) → `4.0` (= 1/16 線)。1/16.(`div=16`) → `8.0` (= 1/32 線)。
1/32.(`div=32`) → `16.0` (= 1/64 線)。すべて確定設計どおり。

> Triplet も整数等分になる確認: `(8.0/3.0)/div` を `1/unit` すると `3*div/8`。
> 1/2T(div=2)→0.75→`Some` だが `divisions = 1/0.75 = 1.333`… **三連も非整数**。
> 三連 1 拍内の本数は「1 拍 = quarter note を 3 等分」が literal なので分割数は常に
> **3 の倍数** が正しい。`1/unit` では 1/8T(div=8) で `unit=0.3333→divisions=3.0` と
> 合うが、1/2T(div=2)/1/4T(div=4) では非整数になる。よって **三連も `mode` 直接分岐**
> にして `divisions = div * 3 / 4` の形 (1 拍 = 4/div 直線 unit を 3 等分) で整数を出す:
>
> - 1/4T(div=4) → `4*3/4 = 3.0` (1 拍 = 1/4 直線を三連 → beat を 3 等分)。
> - 1/8T(div=8) → `6.0`。1/16T(div=16) → `12.0`。1/2T(div=2) → `1.5` (< 整数だが
>   1 拍に 1.5 本 = 2 拍で 3 本 = half-note triplet。これは beat 内に整数本が乗らない
>   ケース。下記 edge case 参照)。

最終形 (3 種別を明示分岐):

```rust
let divisions = match cfg.mode {
    SnapMode::Dotted { div }  => f64::from(div) / 2.0,        // 内包直線格子
    SnapMode::Triplet { div } => f64::from(div) * 3.0 / 4.0, // 1拍=4/div直線を3等分
    SnapMode::Straight { .. } | SnapMode::Adaptive => {
        let unit = cfg.beat_unit(zoom_x_px_per_beat)?;
        1.0 / unit
    }
    SnapMode::Bars { .. } | SnapMode::Off => return None,    // 拍以上 / 無効
};
```

### B-2. `PianoRollView` 構築箇所では何も変えない

`bar_beat_grid` は `piano_roll` widget 内部で呼ばれており、daw_01 が直接呼ぶのは
`ui.piano_roll(...)` (`piano_roll_view.rs:283-291`) のみ。よって **descriptor は
`piano_roll` widget の引数として渡す**必要がある。2 つの選択肢があるが確定設計の
「daw_01 はスナップ値を渡す配線」に従い:

**`PianoRollView` に subdivision 用フィールドを 1 つ足し、widget 内部で
`bar_beat_grid` に転送する** 形にする (これも gui_01 要望 A に含める)。

#### A への追補 (PianoRollView 拡張)

`PianoRollView` (`piano_roll.rs:314-369`) に追加 (非破壊、Default を持たない struct
だが daw_01 は全フィールド明示構築なので 1 行追加で済む):

```rust
/// (FIXME #38) 3 段目 subdivision グリッドの「1 拍あたり分割数」。
/// `None` で subdivision なし (= 現状の bar+beat 2 段)。 widget は内部で
/// `SubGridSpec` を組み立て `bar_beat_grid` に転送する。 色/太さ/閾値は
/// `PianoRollStyle` から取る (caller は分割数だけ渡す)。
pub sub_grid_divisions: Option<f64>,
```

`PianoRollStyle` (`piano_roll.rs:476-489` 付近) に色を追加:

```rust
/// (FIXME #38) 3 段目 subdivision 線の色 (最も淡い段)。 bar_line / beat_line と
/// 同系統で beat_line よりさらに淡く。 default rgba(1,1,1,0.06)。
pub sub_line: Color,
pub sub_line_width_px: f32,   // default 1.0
```

`PianoRollStyle::Default` (`:585-` 付近、`bar_line:` `:600` / `beat_line:` `:601` の隣)
に `sub_line: Color::rgba(1.0, 1.0, 1.0, 0.06)`, `sub_line_width_px: 1.0` を足す
(`bar_line = 0.30`, `beat_line = 0.12` の濃度関係を維持し 3 段目を最も淡く)。

#### widget 内部での転送 (`piano_roll.rs:1957-1964` + `:2019-2025`)

`grid_style_pr` 構築 (`:1957`) で `min_sub_line_px` は default のまま。
`bar_beat_grid` 呼び出し (`:2019`) に `sub` を渡す:

```rust
let sub_grid_spec = view.sub_grid_divisions.map(|d| SubGridSpec {
    divisions_per_beat: d,
    color: style.sub_line,
    line_width: style.sub_line_width_px,
});
// :2019 の呼び出し
hctx.bar_beat_grid(
    ("pr_grid", id_for_inner),
    grid,
    mapping,
    sample_viewport,
    grid_style_pr,
    sub_grid_spec,   // ← 追加
);
```

> `view` は `view_copy` として heavy クロージャに move 済 (`:1978`)。`sub_grid_spec` は
> heavy ブロックに入る前に計算して move するか、`view_copy.sub_grid_divisions` を
> クロージャ内で読む。`style_copy` も move 済 (`:1977`) なので色はそこから取る。

### B-3. daw_01 `PianoRollView` 構築に 1 行追加 (`piano_roll_view.rs:146-166`)

`snap: snap::piano_roll_snap_config(app),` の隣 (`:159`) に追加:

```rust
sub_grid_divisions: snap::piano_roll_subgrid_divisions(app, app.pianoroll_zoom_x),
```

`app.pianoroll_zoom_x` (= `app.rs:1082`, default 64.0 `:1661`) は既に
`piano_roll_view.rs` 内 (`:317`, `:335`) で snap 計算に使われている zoom 値。同じ値を
渡すことで widget 内部の `zoom_x_px_per_beat` (`piano_roll.rs:1560`) と整合する。

> ⚠ 整合性チェック: widget 内部の `zoom_x_px_per_beat` は
> `grid.w / view.len_beats` から再計算される (`:1558-1560`)。daw_01 の
> `pianoroll_zoom_x` は `len_beats = grid_rect.w / zoom_x` の逆算元 (`:149`) なので
> 両者は一致する。Adaptive のとき descriptor 側 (daw_01) と subdivision 描画側 (gui_01)
> が同じ zoom で同じ unit を選ぶことを保証するため、**daw_01 が渡す zoom と widget 内部
> zoom がズレないこと** を確認 (現状の snap_beat 呼び出しと同じ前提なので OK)。

### B-4. テスト (`daw_gui/src/view/snap.rs` の `#[cfg(test)]`)

`piano_roll_subgrid_divisions` の純関数テストを追加:
- snap OFF (`enabled=false`) → `None`。
- 1/4 (choice 1) → `None` (拍と同密度、足さない)。
- 1/2 (choice 0) → `None` (拍より粗い)。
- 1/8 (choice 2) → `Some(2.0)`。1/16 (choice 3) → `Some(4.0)`。1/32 → `Some(8.0)`。
- 1/8T (choice 8) → `Some(6.0)`。1/4T (choice 7) → `Some(3.0)`。
- 1/8. (choice 13) → `Some(4.0)` (1/16 線)。1/16. (choice 14) → `Some(8.0)` (1/32 線)。
- 1 bar (choice 17) → `None`。
- Adaptive (choice 16): zoom 大で `Some(N)`、zoom 極小で unit が 1 拍以上になり `None`。

---

## エッジケース

1. **拍以上に粗いスナップ (1/4 / 1/2 / 1bar / Adaptive 粗ズーム / OFF)**: `None` を返し
   3 段目を足さない。確定設計どおり現状 (bar+beat) のまま。
2. **付点で非整数になる罠**: `1/unit` をそのまま使うと付点は `1.333` 等の非整数等分に
   なる。確定設計の「内包する直線格子」に従い `Dotted{div}` は `div/2` で **整数の直線
   格子密度** を返す (B-1 参照)。これにより付点でも整数等分線が出る。
3. **三連で非整数になる罠**: 同上。`Triplet{div}` は `div*3/4`。1/8T 以降は整数 (6,12,…)
   になるが **1/2T(div=2)→1.5 / 1/4T(div=4)→3.0**。1/2T は「1 拍に 1.5 本」= half-note
   triplet で beat 内に整数本が乗らない。gui_01 側 `divisions.round()` で 2 に丸めると
   三連でなくなるため、**1/2T は subdivision を出さない** (`divisions` が beat あたり
   3 の倍数でない triplet は退避) か、grid を **bar 単位の 3 等分** にするかの判断が要る。
   → 確定設計の「三連格子 (literal)」を厳密に満たすには beat 単位等分では不足。
   **dropdown 最細の三連は 1/4T (div=4 → 3.0 = beat を 3 等分) から整数**。1/2T(7 でなく
   choice 7 は 1/2T) のみ非整数。実害確認のうえ、1/2T は `None` (3 段目なし) に倒すのが
   安全 (notes に明記)。
4. **ズームアウトで線が詰まる**: gui_01 側 `min_sub_line_px` (default 6.0) で
   `px_per_sub < 6.0` の frame は subdivision を描かず 2 段に落ちる。さらにズームアウト
   すると既存 `min_beat_line_px` (4.0) で beat 線も消え bar のみになる (既存挙動)。
5. **小節線/拍線との重複**: subdivision ループは beat 内側点 (`k in 1..divisions`) のみ
   打つので bar/beat 境界とは重ならない。push 順を subdivision → beat → bar にして濃い
   段を前面に保つ。
6. **snap 一時無効 (alt)**: 描画時の subdivision は alt を見ない (`is_active(false)`)。
   grid は「現在のスナップ設定」を表示するもので、drag 中の alt 一時解除では線を消さない。
7. **キャッシュ無効化**: `bar_beat_grid` は `piano_roll` の `hctx.cached(viewport_key,…)`
   (`piano_roll.rs:2017`) 内で呼ばれる。スナップ値変更で 3 段目が変わるので、
   **`viewport_key` に `sub_grid_divisions` を含めるか**、widget 内 `bar_beat_grid` の
   `input_hash` (gui_01 A-3) に `sub` を含めることで再描画させる。後者 (widget 内 hash)
   で対応するので daw_01 の `viewport_key` 改修は不要 — ただし `cached()` が
   `input_hash` をどう統合するか gui_01 側で確認が必要 (notes)。

---

## ビルド/検証

### gui_01 (要望 landing 後、gui_01 session 側で実施)
- `cargo test -p daw-ui` (新 subdivision test + 既存 grid test の 6 引数化)。
- `cargo clippy -p daw-ui -- -D warnings`。

### daw_01
1. `cargo build --workspace` — `PianoRollView`/`bar_beat_grid` の arity 変化が wire
   できているか (protocol 型ではないので daw_gui 単独ビルドでも検出可だが workspace で)。
2. `cargo test -p daw_gui` — `piano_roll_subgrid_divisions` の純関数テスト。
3. `cargo clippy --workspace -- -D warnings`。
4. **実機 smoke** (gui_01 landing 後、最終バッチで一度だけ): `cargo run -p daw_gui` で
   ピアノロールを開き、snap dropdown を 1/8→1/16→1/32 と細かくして縦の細線が増える/
   濃度が 3 段 (小節>拍>細分) になることを目視。1/4・1 bar で 3 段目が消えること、
   三連 (1/8T) で beat 3 等分線、付点 (1/8.) で 1/16 線になることを確認。ズームアウトで
   細線→拍線→小節線と段階的に消えること。
5. `/review` skill を commit 前に実行。
6. commit 後 `cargo build --workspace --release` で green 確認、
   `target/.release-build-failed` が無いこと。

### 待機中の進め方
gui_01 landing 待ちの間も daw_01 側の `piano_roll_subgrid_divisions` (B-1) +
純関数テスト (B-4) は **先に実装・テスト可能** (gui_01 API に依存しない純関数)。
`PianoRollView` への 1 行追加 (B-3) と widget 転送確認 (B-2) のみ landing 後に wire。
