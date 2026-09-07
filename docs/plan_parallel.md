# plan: Parallel (ネスト可能な並列 device chain) + サイドチェイン統合 (r.md #110)

2026-09-06 開始。Live の **Audio Effect Rack** / Bitwig の **FX Layer** / Reason の **Rack** 相当を
device chain に入れる。呼称: track の device 列 = **Rack** (インスペクタ見出し、Reason と同じ)、入れ子の並列容器 = **Parallel** (UI 表記 / 型名 / picker とも)。
chain UI は全面作り直し。本ファイルがこの機能の SSoT。

## 1. 一次情報

- **Live manual §24.3/24.4** (Instrument, Drum and Effect Racks)
  - 「各 chain は同じ入力を同時に受け、自分の device で直列処理し、全 chain の出力が mix される」
  - Chain List の各 chain: volume / pan スライダ、Chain Activator、Solo、Hot-Swap、Rename、色。
  - 「**Device View は一度に 1 本の device chain しか表示できない**ので Chain List は
    navigation の役目も持つ。リストの選択が隣の Devices view に何を出すかを決める」
  - 「Parallel の中の Parallel は括弧の中の括弧」「Devices view を出すと end bracket が視覚的に切り離される」
  - Group (右クリック / Ctrl+G) で選択 device を Parallel に、Ungroup で解体。
- **Bitwig user guide ch.16** (Modulators, Device Nesting, and More)
  - 各 layer は「track header と同じ内蔵チャンネルストリップ」(名前 / gain / pan / M / S)、
    「chain preview = device 数ぶんの小さい四角の silhouette」。
  - 「chain を click するとその chain だけ展開、中の device の上に下向き括弧が付き親と同色」
  - 「layer が増えたら chain list が縦スクロール」。sidechain 選択は track のほか同 track 内の
    layer を選べる。
- 両者とも container = 「縦の chain list (固定幅) | 選択 chain の device」。ネストは横に伸びるだけで
  幅は縮まない。ツリー展開ではない。

## 2. 確定仕様 (ユーザー確認済み 2026-09-06)

| # | 問い | 決定 |
|---|---|---|
| 1 | 表示場所 | **インスペクタのまま、縦回転型**。幅 280px 不変。Live の Chain List を縦のまま置き、「隣」を「下」にする。括弧は開始行 / 終了行 (全幅)、選択 chain の device 区間は左端の細い色帯。インデント無し。 |
| 2 | サイドチェイン | **Sidechain セクション撤去**。aux 入力 port を持つ device の行にだけ `SC` を出し、展開で port ごと `[source ▾][tap ▾]` (multi-port)。source = 他 track + **同 track の Parallel 内 chain**。 |
| 3 | 呼称 | track の device 列 = **Rack** (見出し、Reason)。入れ子の並列容器 = **Parallel** (Ctrl+G / picker / 型名)。「FX Layer」「Rack (容器の意味で)」は使わない。 |

以下は調査と既存 idiom から決めた (確認不要の設計判断):

- chain の控えは Live/Bitwig 共通集合: 名前 / 色 / gain / pan / mute / solo。gain / pan は
  automation・変調の対象 (Live の chain volume と同じ)。
- Parallel 自体に Mix (dry/wet) は置かない (Live Parallel / Bitwig FX Layer どちらも無い。Dry chain を
  1 本置くのが両者の idiom)。Chain Select Zone / Macro は対象外 (Macro は既存の変調が担う)。
- 空 chain = 素通し (dry)。Parallel の各 chain は **audio と MIDI の両方**を入力にとる。
- 作り方: (a) `+ Plugin` picker に builtin 「Parallel」、(b) 選択 device を右クリック / Ctrl+G で
  Group、(c) Ungroup = 全 chain の device を chain 順に直列へ (Live と同じ)。
- 並列 chain 間の latency 差は PDC で揃える (Live / Bitwig と同じ)。
- master bus / group bus も同じ `Vec<Device>` なので Parallel を置ける。

## 3. データモデル (`common/src/model/device.rs` 新設)

```rust
pub enum Device { Plugin(PluginInstance), Parallel(Parallel) }

pub struct Parallel {
    pub id: u64,                 // Song-global next_device_id (plugin / parallel / chain で 1 つの id 空間)
    pub name: String,            // 既定 "Parallel"
    pub chains: Vec<ParallelChain>,
    pub bypassed: bool,          // r.md #105 と同じ意味 (全体素通し)
    pub out_gain: f32,           // §4.3c
    pub gain_match: bool,        // §4.3c
    pub split: Split,            // §4.3d (r.md #112)
}

pub struct ParallelChain {
    pub id: u64,                 // 同上 (AudioTap / automation が指す)
    pub name: String,            // 既定 "Chain N"
    pub color: Option<[f32; 3]>,
    pub gain: f32,               // linear 0..=MAX_TRACK_GAIN、既定 1.0
    pub pan: f32,                // -1..=1
    pub muted: bool,
    pub solo: bool,
    pub devices: Vec<Device>,
}
```

- `Track.devices: Vec<Device>` / `Song.master_fx_chain: Vec<Device>`。
- serde: `Device` は `#[serde(untagged)]`、**Parallel を先に試す** (`chains` 必須で Plugin JSON では
  失敗する → Plugin へ落ちる)。旧 `.daw` の plugin 配列はそのまま読める。`CURRENT_VERSION 35→36`。
- wire: `Device` / `Parallel` / `ParallelChain` に `Encode` / `Decode`。`device.rs` を
  `common/build.rs::WIRE_SOURCES` に登録 (不変条件 7)。
- `ensure_ids`: plugin / parallel / chain を再帰で採番 (sentinel 0 と重複を再採番)。
- アドレス: 挿入先は `ChainRef { Track(u32) | Chain(u64) }` + index。device / parallel / chain は
  全部 id で引く (`Song::find_device(id) -> (ChainRef, index)`、`chain_devices(ChainRef)`、
  `chain_owner_track(chain_id)`)。positional index は表示順のみ (不変条件 1)。
- 走査 helper: `for_each_plugin(&[Device], f)` / `plugins(&[Device]) -> Vec<&PluginInstance>`
  (pre-order = 信号順)。`.devices.iter()` で PluginInstance を直接舐めている全 45 箇所を置換。
- 既存の `Track::paraout_split_device()` は **top-level index** で返す (routed aux out を持つ
  plugin が Parallel の中にあれば、その Parallel を含む top-level device の直後)。

### AudioTap

```rust
pub enum TapSource { Track(u32), Chain(u64) }
pub struct AudioTap { pub source: TapSource, pub tap_point: TapPoint }
```

- 旧 JSON `{ "source_track": N, "tap_point": .. }` は `project.rs` の Value-level migration で
  `{ "source": { "Track": N }, .. }` に書き換える (`source_track` + `tap_point` を持つ object を
  再帰で全部)。
- chain source の tap point: `PreFx` = Parallel 入力 (chain 入力)、`PostFx` = chain の device 通過後・
  gain/pan 前、`PostFader` = gain/pan/mute 後。
- envelope follower (`ModSource`) も同じ `AudioTap` なので chain を変調源にできる。

### automation

- `TrackBuiltinParam::ChainGain { chain_id: u64 }` / `ChainPan { chain_id: u64 }`。レーンは所有
  track (master なら song lanes)。`SendGain` と同じ per-sample ramp。

### PluginInstance

- `aux_input_count: u8` 追加 (host が `SlotPluginLoaded` で報告、`aux_output_count` と対称)。
  SC UI はこれが 1 以上の device にだけ出る。

## 4. Engine (`daw_audio`)

### 4.1 chain program (再帰を RT から消す)

3 つに重複していた device walk (`process_track_owned` / `run_group_fx_chain` /
`process_master_fx_chain`) を **1 本の `run_chain_program`** に統合する。ツリーは compile 時
(off-RT) にフラットな命令列へ落とす:

```rust
pub enum ChainOp {
    Plugin { device_id: u64, ports: PortConfig },
    ParallelBegin { parallel_slot: u32 },                // 現バス (audio L/R + MIDI) を parallel 入力に退避、sum を 0 に
    ChainBegin { parallel_slot: u32, chain_slot: u32 }, // バス := parallel 入力のコピー
    ChainEnd {
        parallel_slot: u32, chain_slot: u32,
        delay: Option<(u32 /*line*/, u32 /*frames*/)>, // 並列 PDC
        snapshot_post_fx: bool, snapshot_post_fader: bool, // chain tap 用
    },                                            // gain/pan/mute 適用 → sum に加算、MIDI merge
    ParallelEnd { parallel_slot: u32 },                   // バス := sum (audio)、MIDI := merged or 入力
}
pub struct ChainProgram { pub ops: Vec<ChainOp>, pub pass1_end: usize }
```

- `Schedule.track_programs: Vec<ChainProgram>` (track index 順) + `master_program`。
  `ProcessTrack` / `ProcessGroupFx { start_device }` / master fx は全部 program を走らせる
  (`start_device` は op index に変換した `pass1_end`)。
- scratch は compile 時に **Parallel / chain ごとに 1 slot** 確保して `RtBundle` で配送
  (`ParallelScratch { in_l, in_r, in_midi, sum_l, sum_r, merged_midi, midi_replaced }`、
  `ChainScratch { gain_ramp, pan_ramp, post_fx_l/r, post_fader_l/r }`)。RT 確保ゼロ。
  MAX_FRAMES = 1024 なので chain 1 本 ≈ 24KB。
- 既存の port 直結規則はそのまま (note_in に MIDI、audio_in に audio、note_out で MIDI 置換、
  audio_out は audio_in ありなら置換 / 無しなら加算)。
- **MIDI merge 規則**: chain 内で note_out device が MIDI バスを置換した chain だけが merged に
  寄与する。1 本も置換しなければ Parallel 出力 MIDI = 入力 MIDI (素通し)。置換した chain が
  あれば time 順 merge。
- chain の `effective_mute = muted || (parallel 内に solo あり && !solo)` は RT で Song snapshot から
  live-read (track の M/S と同じ、再 compile なし)。
- group bus 経路は MIDI バス空で program を走らせる (旧 walker が `!has_audio_output` を skip して
  いた振る舞いは「空 MIDI で dispatch」に統一)。

### 4.2 PDC

- `chain_latency(&[Device])` を再帰化: Plugin = 報告値、Parallel = max(chain latency)。
- Parallel 内で latency が最大 chain より小さい chain には `ChainEnd.delay` を入れる。DelayLine は
  `DelayKey::Chain { chain_id }` で recompile を跨いで走行状態を移送。
- chain tap の source latency = 「所有 track の入力 latency + その chain 出力までの累積」を compile 中に
  計算して `compute_path_latency` に渡す。

### 4.3 sidechain / follower の chain source

- `BufRef::ChainPostFx(slot)` / `ChainPostFader(slot)` / `ChainInput(parallel_slot)` を追加。
  `SidechainTap` / `EnvelopeFollow` の src に使える。
- 同 track の chain を同 track の device が tap する場合は **1 buffer 遅れ** (leaf 宛 tap と同じ
  staging 規則)。依存グラフの自己辺にはしない (cycle ではない)。他 track の chain は所有
  track と同じ扱い。

### 4.3b 自 track の入力 (Pre-FX) を SC の key にする

source 候補に「このトラックの入力 (Pre-FX)」 (= `TapSource::Track(自 track)` + `PreFx` 固定)。
他 track / chain の tap は schedule の `SidechainTap` (1 buffer lag、`compute_input_delays` で
main を揃える) だが、 自 track の Pre-FX は **同じ pass で捕捉した `pre_fx_l/r` snapshot を
`run_plugin` が直接 aux port に載せる** (`ChainOp::Plugin::own_prefx_ports`、lag 0、依存辺なし、
input delay 不要)。 compile は自 track 宛の route を `SidechainTap` から除外する。 自 track の
Post-FX / Post-Fader は出力側 (feedback) なので選べない (setter が Pre-FX に固定)。

### 4.3c 出力 trim と gain match (ヘッダ行)

`Parallel { out_gain, gain_match }`。ヘッダ行 (╭ 行) の右に **出力 trim knob** と **Match トグル**
(終了行には置かない = 高さを増やさない)。engine は `ParallelEnd` op で `sum × out_gain_ramp ×
match_gain` を出力する。gain match は Parallel の入力と和 (chain gain 込み) の mean square を時定数
0.5 s の一次 IIR で追い、`sqrt(in/out)` を ±12 dB に収めて buffer 内で線形に追従させる (無音の間は
保持、off に戻すと 1.0 へ)。音声の純関数なので export でも同じ。**既定 off** — 帯域分割や Dry +
Wet のように和がそのまま正しい使い方では自動補正が逆に壊すので、Parallel ごとの opt-in。
out_gain は automation / 変調の対象 (`TrackBuiltinParam::ParallelOutGain`、住所は `Parallel::id`)、
値のみ IPC は `SetParallelOutGain` / `SetParallelGainMatch`。

### 4.3d 入力の配り方 `Split` と 3 バンド周波数分割 (r.md #112)

`Parallel { split: Split }` — 入力を chain にどう配るか。 Bitwig は Multiband FX-2/3 / Loudness Split /
Mid-Side Split / Stereo Split を **別 container** にしているが、 ここでは 1 つの Parallel の
「配り方」 の切替にする (chain の gain / pan / M / S / SC / automation を container ごとに複製しない)。

```rust
pub enum Split {
    None,                                       // 全 chain に同じ入力 (従来)
    Frequency3 { low_hz: f32, high_hz: f32 },   // 3 バンド (#112)
    MidSide,                                    // Mid / Side (#112 追補)。 将来: Loudness / Stereo (L/R) …
    Selector { active_chain: u64, fade_ms: f32 }, // アクティブな 1 chain だけ (#114、Bitwig Instrument/FX Selector)
}
```

- **出力は chain の並び順に対応**: `Frequency3` なら chain 1 = Low、 2 = Mid、 3 = High。 出力数を
  超える chain (4 本目以降) は全帯域 (素通し) の入力を受ける (Dry chain を足す使い方)。 band chain
  を削除した帯域は無音 (ユーザーの明示操作)。
- **モード切替時の chain**: 出力数に足りなければ空 chain を補う (色は自動)。 **既定名** (`Chain N` /
  別モードの出力名 = `Split::is_generated_chain_name`) の chain は新モードの既定名
  (`Split::default_chain_name`: Low/Mid/High、 Mid/Side、 off なら `Chain N`) へ付け替え、 ユーザーが
  付けた名前と中身は据え置く。 off に戻しても chain は消さない。 モード切替は構造変更なので
  `LoadSong` (再 compile)、 周波数は値のみ IPC `SetParallelSplitFreq`。
- **順序**: `low_hz <= high_hz` を `Parallel::set_split_freq` (GUI と `song_values` の共通 setter)
  が保つ — 片方を相手より先へ動かすと相手が押される (Bitwig の分割点と同じく交差しない)。
  値域 `SPLIT_FREQ_RANGE` = 20 Hz〜20 kHz (対数)。 automation / 変調の対象
  (`TrackBuiltinParam::ParallelSplitFreq { parallel_id, edge }`、 Hz 有効数字 3 桁)。
- **engine** (`graph/band_split.rs`): 4 次 Linkwitz-Riley (Butterworth 2 次 × 2、 24 dB/oct) を
  2 段。 `Low = AP2(high)(LP4(low)(x))`、 `Mid = LP4(high)(HP4(low)(x))`、
  `High = HP4(high)(HP4(low)(x))`。 LR4 の LP + HP = 同じ ω0 / Q の 2 次オールパスなので、 低域に
  上側クロスオーバーのオールパスを掛けると 3 帯域の和は振幅平坦 (Rane Note 160 / KVR N-band LR)。
  `ParallelBegin` で分割 (係数は buffer 終端の ramp 値、 変わったときだけ組み直す)、 `ChainBegin`
  が帯域をバスへ載せる。 状態は `Parallel::id` で再 compile を跨いで移送 (gain match の追従値も同じ
  経路に乗せた)。 band chain の `PreFx` tap = その帯域 (`BufRef::ParallelBand`)。 latency 0。
- **MidSide**: chain 1 = Mid `(M, M)`、 2 = Side `(S, -S)` (`M = (L+R)/2`、 `S = (L-R)/2`)。 空 chain 2 本
  の和は `(M+S, M-S) = (L, R)` で厳密に元へ戻る。 状態なし、 param 行なし。 engine は
  `band_split::Splitter` enum (`Frequency3(BandSplit)` / `MidSide(MidSideSplit)`) で variant ごとの
  分割器を包み、 `ChainBegin` は `Split::output_of(k)` の出力番号で読む (`BufRef::ParallelOutput`)。
- **Selector** (r.md #114、 Bitwig Instrument Selector / FX Selector): 入力 (audio + MIDI) を **アクティブな
  1 chain だけ** が受け、 他は無音 + 新規 note 無し。 全 chain が出力 (`output_of(k) = k`、 chain 数に
  追従) で、 切替は `fade_ms` の線形クロスフェード (全 chain 同じ傾き → 途中も `Σw = 1`)。 非アクティブ
  chain も処理は続くので余韻は残り、 鳴っている note は note-on を受けた chain で note-off まで鳴り切る
  (`selector_split.rs` が `(note_id, key) → chain` の固定長表を持つ。 stop 時の flush は key で引く)。
  `active_chain` は安定 `ParallelChain::id` (並べ替えに追従、 消したら先頭)。 automation / 変調の的は
  `TrackBuiltinParam::ParallelSelect { parallel_id }` = 位置 `0..=1` を chain 数で等分
  (`Split::select_index` が GUI / engine 共通の写像、 基準値は bin の中央 `select_pos`)。 値のみ IPC は
  `SetParallelActiveChain` / `SetParallelSelectorFade`。 モード切替は chain を 2 本 (A/B) に補う。
- **UI**: ヘッダ行の Match の左に dropdown (`No split` / `3 bands` / `Mid/Side` / `Selector`)。 `Frequency3` なら
  ヘッダ直下に param 行 `Low [200 Hz] Mid [2.0k] High`、 `Selector` なら `Active [n] Fade [ms]`
  (`parallel_header.rs`、 inspector 共通の scrubable idiom: 対数目盛 / undo bracket / automation gesture /
  変調 overlay)。 chain 行は共通 (Selector では非アクティブ chain の名前だけ減光。 切替の編集面は
  `Active` 欄 1 つ)。

### 4.4 値のみ更新 (`song_values.rs`)

`SetChainGain` / `SetChainPan` / `SetChainMuted` / `SetChainSolo { track, chain_id, .. }` を
`AudioCommand` に追加 (再 compile なし)。 Parallel 側は `SetParallelOutGain` / `SetParallelGainMatch` /
`SetParallelSplitFreq`。

## 5. Plugin host

- `SlotPluginLoaded.aux_input_count` を追加 (`aux_input_channels.len()`)。それ以外は不変
  (host は device_id keyed で chain topology を知らない)。

## 6. GUI (`daw_gui`)

### 6.1 chain list (インスペクタ) — `view/track_inspector/chain_list/` 新設

行の種類 (ツリーを flatten した view-model `chain_rows()`):

```
Rack
│ EQ                     SC▾ GUI  x     ← Plugin 行 (既存の見た目 + SC)
╭ Parallel                            x     ← ParallelBegin 行 (名前 / 右クリック: Ungroup, 改名, 削除)
│ ▶Dry    □     g  p  M  S              ← 折り畳んだ chain 行 (▶ / 色 / 名前 / preview 四角 / gain / pan / M / S)
│ ▼Comp   ■     g  p  M  S              ← 展開中の chain (▼、既定は全部展開)
┃   Comp                 GUI  x         ← その chain の device は **chain 行の直下** (chain 色の帯)
┃   + Plugin                            ← その chain 末尾への追加
│ ▼Verb   ■■    g  p  M  S
┃   Verb                 GUI  x
┃   + Plugin
│  + chain                              ← 一番下
╰                                       ← ParallelEnd 行 (括弧は Parallel の色)
╴▶ Parallel 2                         x     ← 折り畳んだ Parallel は開始行 1 本だけ
│ Limiter                GUI  x
│ + Plugin
```

- 開閉: Parallel 行 / chain 行の左端の disclosure (▶/▼) で **それぞれ個別に開閉** (Bitwig の layer と
  同じ。既定 = 展開、閉じた id だけ ViewState `collapsed_parallel_nodes: Vec<u64>` に保存、見方の都合
  なので dirty 無し)。畳んだ Parallel は開始行 1 本、畳んだ chain は chain 行 1 本。
- chain 行: click = 選択集合 (last-wins の Devices 面)。ダブルクリック = 改名 (`text_input_at`)。
  右クリック = 改名 / 色 (color_picker) / 複製 / 削除 / chain 追加。
  gain / pan は mixer send と同じミニ knob + automation gesture idiom。
- 色は **作った時点で自動で別の色** (Bitwig): `track_color::PALETTE` を兄弟数から巡回し、兄弟にも
  祖先 (外側の chain / Parallel の色) にも無い色を取る (`auto_color`)。Parallel 自体も色を持ち
  (`Parallel.color`、括弧行の色)、chain はその Parallel とも別の色。ネストしても内外で被らない。
  picker でいつでも上書き可。
- Plugin 行の `Par` 展開 (param panel) は今までどおり行直下。`SC▾` は展開で port ごとに
  `In N: [source ▾][tap ▾]`。source 候補 = 「—」+ 他 track + 同 track の全 chain
  (`Parallel名 / Chain名`)。
- Parallel 行 / chain 行 / Plugin 行はすべて **同じ選択集合** (`selected_device_ids`、id 空間が 1 つ)。
  Del / Cut / Copy / Paste / Ctrl+G / Ungroup が効く。
- ドラッグ: Plugin 行 / Parallel 行 (= 中身ごと) は任意の chain の任意位置へ移動 / Ctrl でコピー。
  chain 行は同じ Parallel 内で並べ替え、他 Parallel へは移動。track 跨ぎは既存の DEVICE_DRAG_KIND 経路。
- 旧 `draw_sidechain_section` は撤去。`draw_parallel_out_section` / 読み込み失敗 / `Q` /
  resource monitor は再帰 plugin 列挙に置換。

### 6.2 widget — `ui/crates/ui/src/widgets/drag_list.rs` 新設 (domain-free)

`reorderable_list` は「一様行 + permutation」なので不採用。新 widget:

- 入力: 行ごとの高さ (`DragListRow { height, draggable, block_len }`、block_len = その行を掴んだ
  ときに一緒に運ぶ後続行数 = Parallel 行の中身)、挿入スロット (`DragListSlot { after_row, indent }`)、
  `valid(from_row, slot) -> bool`。
- 出力: `clicked` / `clicked_modifiers` / `hovered` / `dragging` / `dragged_out` /
  `dropped: Option<(row, slot)>` / `external_insert_slot` / `external_dropped_slot` / `row_rects`。
- 描画はすべて caller のクロージャ (widget は行の rect と drop indicator だけ)。

### 6.3 操作

- `+ Plugin` (chain ごと): `OpenPluginPicker { target: ChainRef }`。picker に builtin
  「Parallel」(`builtin://daw_01.parallel`) を列挙、選ぶと `Device::Parallel` (chain 1 本) を挿入
  (host には送らない)。
- Group (右クリック / Ctrl+G、last-wins が Devices 面のとき。それ以外は既存の track group):
  選択 device を順に抜き、先頭の位置に Parallel (chain "Chain 1" に格納) を挿す。
- Ungroup: Parallel を全 chain の device の直列連結に置換。
- clipboard: `ClipboardPayload::Devices` は `Device` (Parallel 込み) を運ぶ。chain 行の copy は
  `ClipboardPayload::Chains`、貼り先が Parallel 内なら chain 追加、track 直下なら新 Parallel に包む。
- undo: 全部 `edit_song()` 経由 (不変条件 5)。ラベル: Parallel 追加 / chain 追加 / chain 削除 /
  Group / Ungroup / chain 改名 / chain 色 / chain gain / pan / mute / solo。

### 6.4 影響する既存機能

- video FX: pre-order flatten で直列適用 (Parallel で映像を並列合成はしない)。
- VOICEVOX / subtitle / lipsync の device 実在判定: 再帰列挙。
- device relocate / paste / bounce / grouping / project migration / script API: `ChainRef` 化。

## 7. テスト

- common: serde 往復 (旧 plugin 配列 JSON → `Vec<Device>`、Parallel 入り)、AudioTap migration、
  ensure_ids の再帰採番、Group / Ungroup の純関数。
- daw_audio compile: Parallel の program 生成 (op 列)、並列 PDC の delay 挿入、chain tap の
  SidechainTap / latency、bypass Parallel の除外。execute: 2 chain の sum、mute / solo、空 chain の
  素通し、MIDI merge 規則、nested Parallel。
- daw_gui: `chain_rows()` の flatten、Group / Ungroup / relocate の app-state テスト
  (既存 `tests/app_state/device_relocate.rs` を ChainRef へ拡張)。
- 実機: Parallel 作成 → 2 chain (Dry / Comp) → Comp の SC に Dry chain → ducking。ネスト 2 段の表示。

## 8. 状況 (2026-09-06)

- 実装完了・実機 sign-off 済 (model / engine / plugin host / GUI / テスト)。`make clippy` /
  `make arch-lint` (新規違反 0) / common / daw_audio / daw_gui (lib + app_state + 関連 integration) /
  daw-ui-core すべて green。
- 実機で決めた変更: 容器の呼称 Rack → **Parallel** (見出しの「Rack」= track の device 列)、Parallel /
  chain の個別開閉 (▶/▼、枠なし)、展開中 chain の中身はその行の直下・`+ chain` は一番下、
  chain / Parallel の自動色 (色相差最大)、chain 行の x、chain 行の色帯を device 行の帯と揃える、
  「このトラックの入力 (Pre-FX)」を SC の source に (§4.3b)、chain を全部消した Parallel は素通し、
  chain の gain / pan を load 時に値域へ。
- `drag_list` は掴めない行でも click を返す (chain 行の選択に必要だった)。
- 保留: device 間の任意点を tap にする案は Live / Bitwig に無いので見送り (ユーザー判断)。
