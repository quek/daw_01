# r.md #129 Rack 内蔵デバイス設計（組み込み Comp / EQ をチェーン上の正式なデバイスにする）

> **この文書の位置付け**
> - 状態: 設計確定、実装前。前提 HEAD は `76837672`（clean）。本文の file:line はすべてこの HEAD の行。
> - **r.md の番号は再利用されている。** コミット `76837672` と `scripts/arch_lint_baseline.txt:114` にある「r.md #129」は旧項目（プロジェクトタブ、`docs/plan_project_tabs.md`）を指し、本書の #129 とは別物。
> - 本書は `docs/plan_rack_native_devices.md` の正本。置き換える既存の決定は §16 に列挙する。
> - 「（未実行）」はコードを読んで導いた結論で、実行による確認はしていない。
> - 作業単位の略号: S0 / S1a / S1b / S1c / F / E / A / C / M / R / 統合（§19）。

---

## 1. 目的

1. 組み込み Comp / EQ（master は Bus Comp / Tone EQ）を、チェーン上の正式なデバイス `Device::Native(NativeDevice)` にする。
   - plugin と同じく固有 id を持ち、次の対象になる: 行 / Par / D&D / オートメーション / 変調 / MIDI Learn / 外部サイドチェイン。
   - plugin との違いは 3 点だけ: 削除できない、Parallel に入れられない、普通のドラッグで他トラックへ移せない。
2. Rack を固定幅 360px にし、デバイスの GUI（Par）を収める。Rack 行 / Par / Mixer 帯 / マスターパネル / オートメーションは、すべて `Song` 内の同じ `NativeDevice` の値を読む。
3. strip 専用に複製されていた経路を、デバイス一般の経路 1 本にまとめる。
   - 対象: 値 IPC / GR テレメトリ / 自動 ON / hover と Q / 構造の正規化 / ジェスチャー / 束縛の運搬と掃除。
   - これで周辺にある既存の穴（§18）を塞ぐ。

---

## 2. 確定要件（Q1–Q19、見える挙動で）

ユーザーと grill-me で 1 問ずつ確定した。**変更しない。**

| # | 見える挙動 |
|---|---|
| Q1/Q2 | Rack の幅はドラッグで変えられない。固定 360px（現在 280px）、中身の幅 336px |
| Q3 | master の Rack にも組み込み行が出る。Limiter は Rack の末尾、フェーダーの後ろにあたる位置に固定行として出て、動かせず消せない |
| Q4 | 組み込みの Comp と EQ は別々の行で、plugin と同じく D&D で自由に並べ替えられる。楽器より上に置いた Comp は楽器の音を処理しない（plugin と同じ規則） |
| Q5 | 組み込み行はチェーンの最上位にしか置けない（Parallel の中へは落ちない）。× ボタンは無く、右クリックにも削除 / 切り取り / Parallel にまとめるは出ない。複数選択して Delete すると組み込みだけが残る。Ctrl+ドラッグ / コピー貼付 / 複製で増やしたものは「追加の Comp / EQ」になる（同じトラックでも他トラックでも）。普通のドラッグでは他トラックへ移らない |
| Q6 | 新しいデバイス（+Plugin / トラックへのドロップ / 追加 Comp 等）は「組み込み以外で一番下にあるデバイスの直後」に入る。組み込み以外が 1 つも無ければ、通常トラックでは組み込みの上、master では組み込みの下。既存プロジェクトを開いたときは今と同じ音になる並び: 通常トラックは devices → Comp → EQ、master は Bus Comp → Tone EQ → fx chain、Limiter はフェーダーの後 |
| Q7 | 追加できるのは 4 種。**Comp**（LEV/CMP/LIM + SC フィルタ + Thr/Rat/Atk/Rel/SC/Gain）/ **EQ**（HP/LP + LF/LMF/HMF/HF の 6 バンド）/ **Bus Comp**（Thr / Ratio 段階 / Attack 段階 / Release 段階 + Auto / Makeup）/ **Tone EQ**（Low / LoMid / High の固定周波数ゲインのみ）。どのトラックにもどれでも足せる。組み込みは、通常 / group / return が Comp + EQ、master が Bus Comp + Tone EQ（+ 固定 Limiter） |
| Q8 | 追加の入口は plugin picker。4 種が「内蔵」として一覧の先頭にまとまって出る。検索で絞れ、Ctrl+クリックで続けて足せる |
| Q9 | Par を閉じた行は「名前 + 小表示 + [Par]」。EQ / Tone EQ の小表示は幅 80px ほどのミニ EQ カーブ、Comp / Bus Comp は横向きの GR バー。OFF の行は名前と小表示が薄くなる |
| Q10 | 組み込み行に鍵マークは付けない（× の有無だけで見分ける）。名前は種類名 + 番号で、組み込みは「Comp」、足した分は「Comp 2」「Comp 3」。そのトラックに同じ種類がまだ無ければ番号は付かない。番号は足したときに決まり、並べ替えても変わらない。オートメーションレーン名にも出る |
| Q11 | Par は行ごとに独立していて、何枚でも同時に開ける。plugin / 映像 FX / VOICEVOX の Par も同じ（今は Rack 全体で 1 枚） |
| Q12 | Par の中身は幅 336px、6 列 × 約 56px の格子で、左詰め。<br>・**EQ**: 上に全幅 × 72px のカーブ。下の列は HP/LF/LMF/HMF/HF/LP、行は Freq / Gain / Q。HP・LP は Freq + [ON]、LF・HF は Freq / Gain / [Bell]、LMF・HMF は Freq / Gain / Q<br>・**Comp**: [LEV\|CMP\|LIM] と GR バー（dB 数値）、列 Thr/Rat/Atk/Rel/SC/Gain、SC 列の下に [Listen]<br>・**Bus Comp**: Thr / Ratio [2\|4\|10] / Atk / Rel / Makeup + GR バー<br>・**Tone EQ**: Low / LoMid / High + カーブ<br>・**master Limiter**: Ceiling + GR バー<br>数値はつまみの下に常に出る。ドラッグで変更、クリックで数値入力、ダブルクリックで既定値 |
| Q13 | EQ カーブ上に各バンドの点が出て、ドラッグで調整できる。左右 = Freq、上下 = Gain、ホイール = Q（Q を持つバンドのみ）。HP/LP の点は 0dB 線上を左右にだけ動く。Tone EQ の点は上下にだけ動く。OFF のバンドやデバイスの点を触ると自動で ON になる。点を動かすと、下のつまみと数値も同時に動く |
| Q14 | Par の EQ カーブの背後に「その EQ を通った後の音」のスペクトラムが薄く出る。計算するのは Par が開いている間だけ |
| Q15 | 行に ON ボタンは置かない。ON/OFF は Q キー（行に hover）/ 右クリック / 行の小表示ダブルクリックで切り替える。4 種とも、組み込み・追加分とも、つまみやカーブ点に触ると自動で ON になる（Q で OFF にした直後でも）。plugin は今のまま（触っても ON にならない） |
| Q16 | Mixer タブ上部の Comp/EQ 帯は残る。出るのは組み込みの Comp/EQ だけ（値は Rack と同じ）。追加分は Rack にだけ出る。帯の上下の並びは、Rack での組み込み 2 つの前後に合わせて入れ替わる。常設の小表示（GR + カーブ）も組み込みだけ |
| Q17 | マスターパネルの COMP/EQ/LIM は残る。出るのは master の組み込み Bus Comp / Tone EQ / Limiter（値は同じ）。Bus Comp と Tone EQ の上下は Rack の前後に合わせ、Limiter は常に一番下 |
| Q18 | どの Par を開いていたかは、プロジェクトの表示状態として保存される（`*` は立たない）。plugin / 映像 FX / VOICEVOX も同じ。行を移動・コピーしたときは、移動先・コピー先を閉じた状態から始める |
| Q19 | Comp / Bus Comp は、組み込み・追加分とも SC▾ を持つ。plugin と同じ手順で他トラックの音を外部サイドチェインとして配線できる。配線するとその音で検出する（SC フィルタと Listen も配線した音にかかる）。配線しなければ自分の音で検出する |

---

## 3. 参照製品の根拠

一次情報は原文テキストを行単位で照合済み（調査レポート `wf/reference.md`）。

| 要件 | 製品と一次情報 | 本件への読み替え |
|---|---|---|
| 削除できない内蔵デバイス（Q5） | **Renoise** Effect Chains「Two devices are always present in every effect chain and cannot be removed or repositioned: the Pre and Post-Mixer devices.」 https://tutorials.renoise.com/wiki/Effect_Chains / Lua API `delete_device_at`「The mixer device at index 1 can not be deleted」 https://renoise.github.io/xrnx/API/renoise/renoise.Track.html | 削除不可はそのまま採る。位置固定は採らない（Q4） |
| 内蔵モジュールを D&D で並べ替え（Q4） | **Cubase Pro 11** Channel Strip「You can change the position of specific modules in the signal flow via drag and drop.」 https://archive.steinberg.help/cubase_pro/v11/en/cubase_nuendo/topics/mixconsole/mixconsole_channel_strip_modules_r.html / **Cakewalk** ProChannel「Click a module's gripper and drag the module up/down」 https://legacy.cakewalk.com/Documentation?product=Cakewalk&language=3&help=ProChannel.01.html（.02 / .03 も同系） | 組み込みも plugin と同じ行 D&D にする |
| Comp/EQ の順序入れ替えの需要（Q4） | **Fender Studio Pro**（旧 Studio One）Fat Channel XT「Swap Comp/EQ Order」 https://fenderstudiopromanual.fender.com/en/Content/Built-In_Effects_Topics/Mixing.htm / **Reason 13.4** 「Dyn Post EQ」 https://docs.reasonstudios.com/reason13/the-main-mixer | Comp 行と EQ 行を分け、それぞれ単独で動かせるようにする |
| 縦置きのデバイスリスト（原文「Live/Bitwig/Renoise の縦置きイメージ」） | **Bitwig 5.3** Mix View「The devices section provides a list of all the top-level devices on each track.」 https://www.bitwig.com/userguide/latest/the_mix_view/ 、Inspector にも同じ一覧 https://www.bitwig.com/userguide/latest/other_mixing_interfaces/ / **Renoise** Mixer「Track Effects devices are shown in the Mixer rack above the track levels.」 https://tutorials.renoise.com/wiki/Mixer / **Live 12** の Device View は横並び（公式の縦表示は無い） https://www.ableton.com/en/manual/working-with-instruments-and-effects/ | Rack（インスペクタ）の縦リストに組み込み行を入れる |
| 行の小表示（Q9） | **Bitwig**「This includes EQ curves (for EQ+, EQ-5 …) or gain reduction amounts (for Compressor+ …)」（the_mix_view、公式画像 3201/3203 で行の右端に出ることを目視） / **Cakewalk** ProChannel 折り畳み時「QuadCurve Equalizer graph. Shows the equalization curve.」 | EQ はカーブ、Comp は GR バー |
| デバイスごとの展開（Q11） | **Fender Studio Pro** Micro View（Insert をダブルクリックで下へ展開、取得テキスト `fsp2_Built-In_Effects_Topics_Effect_Micro.txt:9-11`）/ **Cakewalk** モジュールごとの Minimize/Restore / **Renoise** デバイスごとの最小化 | Par を行ごとに独立させる |
| 展開すると幅いっぱいを使う（Q12） | **Cakewalk**「When expanding ProChannel in the Inspector, ProChannel fills the entire width of the Inspector.」 | Par は中身の幅 336px 全体を使う |
| インスペクタは固定幅（Q1/Q2） | **Bitwig**「This panel is not resizable.」（the_window_body、`bw_the_window_body.txt:25`） | 固定 360px |
| Ctrl+ドロップでコピー（Q5） | **Renoise** Mixer「Holding "Left Control" while dropping the effect will create a copy of the device.」 / **Bitwig** ALT ドラッグでコピー | daw_01 既存の Ctrl コピーのまま |
| GR 表示 | **Reason**「The right LED meter shows the gain reduction applied by the compressor.」/ **Studio Pro** Channel Strip の GR LED | 行と Par に GR バーを出す |
| EQ のグラフ表示 | **Renoise** EQ 5/10 の Graph Only / Sliders Only / Full Display https://tutorials.renoise.com/wiki/Audio_Effects / **REAPER** User Guide v7.79 §6.12 p.118 の埋め込み UI | Par にカーブを置き、点を直接操作する |

- **前例が無い組み合わせ**: 「削除できない内蔵 + 同種を複数追加 + 並べ替え可」をそのまま備える製品は、確認した範囲に無い（reference 観察 1）。Cubase（固定セット + D&D）と Cakewalk（Inspector 内の縦モジュール + 折り畳みグラフ、ただし削除可で 1 インスタンスまで）を合わせた形になる。
- **Renoise との違い**: Renoise は「デバイスを移動してもオートメーションは付いてこない」（Mixer「When moving a device, the original Automation(s) will be removed.」）。daw_01 は既存どおり束縛を運ぶ（§8.8）。

---

## 4. 全体像

### 4.1 データフロー

```
[load]
  migrate_legacy_song ─→ native_migration::migrate_strips_to_native (strip/master_strip → 実 id の組み込み device, target 書換)
  → VALUE_MIGRATIONS → deserialize → SONG_MIGRATIONS
  → normalize_after_load: sanitize_ranges → ensure_ids(採番 → normalize_native_devices) → prune_dangling_param_targets
  → SongDoc::replace_song (enforce_edit_invariants、baseline 確定前なので * は立たない)

[つまみ / 数値欄 / カーブ点 (Rack Par・Mixer 帯・マスターパネル)]
  view::native_device::native_knob(surface = ParamSurface::X, owner, device, param)
    ├ AppEvent::Device(DeviceEvent::NativeEdit{device_id, NativeEdit::Params[..]})
    │    → handler/native_edit.rs: edit_song_checked(NativeEdit::apply  ← 自動 ON の SSoT)
    │    → SongDoc: enforce_edit_invariants
    │    → AudioCommand::SetNativeDevice (値だけ、即時) + フレーム末 LoadSong (Recompile)
    │    → note_touched_target → last_touched = "Comp 2: Thr"
    ├ push_param_gesture(surface, owner_id, target, dragging)   ← active_param_gestures[(owner,target)] = surface
    └ push_mod_depth_bracket(surface, owner_id, target)

[Q] view::bypass_toggle::dispatch → hovered_bypass_target → SetDevicesBypassed / MasterLimiterEdit::On
[Listen ▶] DeviceEvent::SetScListen → sc_listen.rs (bypass 中なら有効化) → AudioCommand::SetScListen
[Par 開閉] DeviceEvent::ToggleRackPanel → ProjectView.open_rack_panels (ViewState に保存、* なし)
          → sync_device_scopes (フレーム末、差分) → AudioCommand::SetDeviceScopes

[daw_audio 1 buffer]
  compile (off-RT): ChainOp::Native{device_id, native_slot} + NativeScratch(DSP / SC 受け皿 / Listen 受け皿)
                    NodeOp::NativeSidechainTap / ApplyDelay(BusScAlign) / master_limiter_latency を焼く
  RT: pass1 leaf program → pass2 [SC staging → BusScAlign → group program] → master program(組み込み Bus/Tone + fx)
      → master_gain → MasterLimiter → publish_meters(peak + native GR 面 + limiter GR) / device scope ring

[GUI poller] read_native_meters → TrackPeaksTick{native_gr, master_limiter_gr_db}
             DeviceScopeReader → SpectrumAnalyzer → DeviceSpectrumTick
```

### 4.2 統一名の表（全単位はこの名前を使う）

| 概念 | 名前 | 置き場 | 作る単位 |
|---|---|---|---|
| チェーン上の内蔵デバイス | `Device::Native(NativeDevice)` | `common/src/model/device.rs`、`model/native.rs` | F |
| 種類 | `NativeKind { Comp, Eq, BusComp, ToneEq }` | `model/native.rs` | F |
| 値 | `NativeParams { Comp(CompSettings), Eq(EqSettings), BusComp(BusCompSettings), ToneEq(ToneEqSettings) }` | `model/native.rs`、`model/native/{comp,eq,bus_comp,tone_eq}.rs` | F |
| パラメーター住所 | `NativeParamId { On(NativeKind), Comp(CompParam), Eq{band,param}, BusComp(BusCompParam), ToneEq(ToneEqBand) }` | `model/native_param.rs` | F |
| Limiter | `Song.master_limiter: MasterLimiterSettings`、`MasterLimiterParam { On, Ceiling }` | `model/master_limiter.rs` | F |
| 値域 | `ParamRange { Linear, Log, LogWithOff, Toggle, Stepped{count} }` | `model/param_range.rs` | F |
| オートメーション住所 | `AutomationTarget::NativeParam{device_id, param}`、`AutomationTarget::MasterLimiter(MasterLimiterParam)` | `model/automation.rs` | F |
| 束縛参照 | `AutomationTarget::bound_node_id(_mut)` | `model/param_address.rs` | F |
| lane/routing の置き場 | `Song::param_stores(_mut)`、`push_lane`、`bound_owner_track` | `model/param_address.rs` | F |
| 編集後の不変条件 | `Song::enforce_edit_invariants()` = `normalize_native_devices()` \| `prune_dangling_param_targets()` | `native/chain_rules.rs`、`param_address.rs` | F |
| 挿入位置 | `Song::default_insert_index(dest) -> Option<usize>`、`default_insert_index_in(devices, is_master)`、GUI `InsertAt { Index(u32), Default }` | `chain_rules.rs`、`daw_gui/src/device_addr.rs` | F（`InsertAt` の型は S1c） |
| 番号 | `Song::next_native_ordinal`、`Song::assign_native_ordinals(dest, devices, OrdinalPolicy)`、`Song::prepare_device_copies` | `chain_rules.rs` | F |
| ガード | 述語 `Song::is_builtin_native(id)` / `Song::can_relocate(id, dest, copy)`、GUI `device_guard::{DeviceOp, permitted_ids, any_permitted, rejected_builtin_count}` | `chain_rules.rs`、`handler/device_guard.rs` | F / R |
| 配線の依存 | `routing_deps::{TrackDeps, EdgeScope, AuxConsumer, aux_consumers}` | `common/src/routing_deps.rs` | F |
| 値 IPC | `AudioCommand::SetNativeDevice{project, device_id, bypassed, params}`、`SetMasterLimiter{project, limiter}` | `protocol.rs` | F |
| Listen | `AudioCommand::SetScListen{project, device_id: Option<u64>}`、GUI `ProjectEphemeral.sc_listen_device`、engine `ProjectShared.sc_listen_device: AtomicU64` | | F / E |
| スペクトラム | `AudioCommand::SetDeviceScopes{project, device_ids}`、`common/src/device_scope_bridge.rs`、`AppEvent::DeviceSpectrumTick` | | F / E / R |
| GR | `ProjectTelemetry::{publish_native_meters, read_native_meters, master_limiter_gr_db}`、`AppEvent::TrackPeaksTick{native_gr, master_limiter_gr_db}`、`TransportState.native_gr: NativeGrDisplay` | | F / E |
| engine op | `ChainOp::Native{device_id, native_slot}`、`NodeOp::NativeSidechainTap`、`DelayKey::BusScAlign` | `daw_audio/src/graph/*` | E |
| engine の値解決 | `daw_audio::automation::{resolve_native_device, resolve_master_limiter}` | | F（型追従）/ E |
| DSP | `common::dsp`（旧 `channel_strip_dsp`）、`daw_audio::native_dsp` | | F / E |
| GUI の編集 | `DeviceEvent::NativeEdit{device_id, edit}`、`NativeEdit { Params, EqBandOn, EqBell, CompMode }`、`MasterLimiterEdit { On, Ceiling }` | `event_device.rs`、`event_native.rs` | F |
| ジェスチャー所有者 | `RecordingState.active_param_gestures: HashMap<(u32, AutomationTarget), ParamSurface>`、`push_param_gesture`、`sweep_param_gestures` | `state/recording.rs`、`view/param_gesture.rs` | F |
| live 値 | `LiveParamScope`、`live_param_value(_on)`、`live_native_param`、`live_native_device` | `handler/view_model.rs` | F |
| 共有描画部品 | `view::native_device::{native_knob, native_knob_with_value, limiter_knob, NativeKnobSpec, ParamOwner, wid, draw_eq_curve, CurveAxes, curve_handles, draw_gr_vertical, draw_gr_horizontal, draw_gr_segments, gr_text}` | `daw_gui/src/view/native_device/` | C |
| Par の開閉 | `RackPanelKey { Device(u64), MasterLimiter }`、`ViewState.open_rack_panels`、`DeviceEvent::ToggleRackPanel` | `view_state.rs`、`handler/rack_view.rs` | F / R |
| Q の宛先 | `BypassTarget { Device(u64), MasterLimiter }`、`hovered_bypass_target`、`bypass_toggle_event` | `handler/bypass_target.rs` | F |
| 値表示 | `ScrubableNumberFormat::Choices{labels}`、`automation_value_display`、`native_param_label` →「Comp 2: Thr」 | ui core、`automation_value.rs` | A（`native_param_label` の型は F） |
| MIDI Learn | `BindingTarget::NativeParam{device_id, param}`、`BindingTarget::MasterLimiter(..)` | `model/midi_bind.rs` | F（型）/ A |

`RackPanelKey` と `BypassTarget` は同じ形をしているが役割が違う。
- `RackPanelKey`: 保存形。common に置き、serde を持つ。
- `BypassTarget`: GUI 内で使う操作の宛先。serde は持たない。

widget id の鍵には `RackPanelKey` を使う。同じ形の enum を 3 つ目として作らない。

---

## 5. model（common）

### 5.1 ファイル配置

| ファイル | 中身 | WIRE_SOURCES |
|---|---|---|
| `common/src/model/native.rs`（新） | `NativeDevice` / `NativeParams` / `NativeKind` と値 API。`mod native { comp; eq; bus_comp; tone_eq; chain_rules; }` | 登録 |
| `model/native/comp.rs`（新） | `CompSettings` / `CompParam` / `CompMode` と定数（`channel_strip.rs:220-307, 408-480` から移設） | 登録 |
| `model/native/eq.rs`（新） | `EqBand` / `EqParam` / `EqBandSettings` / `EqSettings` と定数（`channel_strip.rs:128-216, 311-405` から移設） | 登録 |
| `model/native/bus_comp.rs`（新） | `BusCompSettings` / `BusCompParam` / `BusCompRatio` / `BusCompAttack` / `BusCompRelease` と Auto Release 定数（`master_strip.rs:19-140, 259-288` を改名して移設） | 登録 |
| `model/native/tone_eq.rs`（新） | `ToneEqSettings` / `ToneEqBand`（`master_strip.rs:142-185, 290-320`） | 登録 |
| `model/native/chain_rules.rs`（新） | 組み込みの配置 / 番号 / 挿入位置 / ガード述語（§5.7） | 対象外 |
| `model/native_param.rs`（新） | `NativeParamId` とメソッド、`native_param_label` | 登録 |
| `model/master_limiter.rs`（新） | `MasterLimiterSettings` / `MasterLimiterParam` / `MASTER_LIMITER_LOOKAHEAD_MS` / `limiter_lookahead_samples` / `MASTER_LIMITER_RELEASE_MS`（`master_strip.rs:322-351`） | 登録 |
| `model/param_range.rs`（新） | `ParamRange`（`channel_strip.rs:17-124` から移設、Toggle / Stepped を追加。wire に載らない） | 対象外 |
| `model/param_address.rs`（新） | `Song::{param_stores(_mut), push_lane, bound_owner_track, prune_dangling_param_targets, automation_lane_by_key(_mut)}`、`AutomationTarget::bound_node_id(_mut)` | 対象外 |
| `common/src/routing_deps.rs`（新） | §5.9 | 対象外 |
| `model/track/channel_strip.rs` / `model/master_strip.rs` | **削除**（中身は上へ移す） | 行を削除 |

**改名表**（D2。Bus Comp / Tone EQ は全トラックに足せるので master の名前をやめる）

| 旧 | 新 |
|---|---|
| `MasterCompSettings` / `MasterEqSettings` | `BusCompSettings` / `ToneEqSettings` |
| `MasterRatio` / `MasterAttack` / `MasterRelease` | `BusCompRatio` / `BusCompAttack` / `BusCompRelease`（`LABELS` を持つ） |
| `MasterEqBand` | `ToneEqBand`（variant 名は変えない） |
| `MasterStripParam` | `BusCompParam` / `ToneEqBand` / `MasterLimiterParam` に分割 |
| `MASTER_AUTO_RELEASE_{MIN,MAX,TRACK}_MS` / `master_auto_release_ms` | `BUS_COMP_AUTO_RELEASE_*` / `bus_comp_auto_release_ms` |
| `MASTER_EQ_LIMIT_DB` | `TONE_EQ_LIMIT_DB` |
| `MASTER_GR_METER_RANGE_DB`（`master_strip.rs:353`）と `GR_METER_RANGE_DB`（`channel_strip.rs:307`） | `GR_METER_RANGE_DB` 1 本（どちらも 20.0） |
| `common::channel_strip_dsp` / `master_eq_stages` / `master_eq_magnitude_db` | `common::dsp` / `tone_eq_stages` / `tone_eq_magnitude_db` |

### 5.2 Device / NativeDevice / NativeKind / NativeParams

**`Device`**（`common/src/model/device.rs:20-25` を置き換え）

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum Device {
    Parallel(Parallel),
    /// r.md #129: daw_audio が in-process で処理する内蔵 device。externally tagged `{"Native": {..}}`。
    Native(NativeDevice),
    #[serde(untagged)] // arch-lint: allow-untagged (fallback variant 1 本、判別は Parallel / Native タグ)
    Plugin(PluginInstance),
}
```

- serde の untagged variant は末尾に置く必要があるので、`Native` は `Plugin` より前に置く。これは device.rs:16-19 の規約「足すならタグ付き」に従っている。
- bincode では `Plugin` の index が 1 から 2 に変わる。WIRE_SOURCES の fingerprint がこれを検出する。

**`NativeDevice`**（`Copy`。`AuxInputRoute` は Copy なので成り立つ: `model/modulation.rs:98`）

```rust
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct NativeDevice {
    #[serde(default)] pub id: u64,                                                          // 0 = 未採番 sentinel
    #[serde(default, skip_serializing_if = "std::ops::Not::not")] pub builtin: bool,
    #[serde(default)] pub ordinal: u16,                                                     // 1 = 番号なし / 2.. / 0 = 未採番
    #[serde(default, skip_serializing_if = "std::ops::Not::not")] pub bypassed: bool,       // ON/OFF の唯一の SSoT
    pub params: NativeParams,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub aux_input: Option<AuxInputRoute>, // Comp/BusComp のみ意味を持つ
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum NativeParams { Comp(CompSettings), Eq(EqSettings), BusComp(BusCompSettings), ToneEq(ToneEqSettings) }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Encode, Decode)]
pub enum NativeKind { Comp, Eq, BusComp, ToneEq }   // variant 名は JSON `{"On":"Comp"}` に出る
```

**API**（`display_name` 以外は確保なし、RT から呼べる）

| 関数 | 仕様 |
|---|---|
| `NativeKind::ALL` | `[Comp, Eq, BusComp, ToneEq]`（picker の並び） |
| `NativeKind::BUILTIN_TRACK` / `BUILTIN_MASTER` | `[Comp, Eq]` / `[BusComp, ToneEq]` |
| `NativeKind::label()` | `"Comp"` / `"EQ"` / `"Bus Comp"` / `"Tone EQ"` |
| `NativeKind::undo_label()` | `"コンプ変更"` / `"EQ 変更"` / `"バスコンプ変更"` / `"トーン EQ 変更"` |
| `NativeKind::ports()` | 4 種とも audio in/out のみ（`PortConfig { has_audio_input: true, has_audio_output: true, ..Default::default() }`）。program.rs:799-808 の直結規則でそのまま扱える |
| `NativeKind::has_gain_reduction()` / `accepts_sidechain()` | Comp \| BusComp |
| `NativeKind::accepts_listen()` | Comp のみ（Q12 で [Listen] があるのは Comp だけ） |
| `NativeKind::picker_id()` / `from_picker_id(&str)` | `common::plugin_db::{NATIVE_COMP_PICKER_ID, NATIVE_EQ_PICKER_ID, NATIVE_BUS_COMP_PICKER_ID, NATIVE_TONE_EQ_PICKER_ID}` = `builtin://daw_01.native.{comp,eq,bus_comp,tone_eq}`（plugin_db.rs:243 の `PARALLEL_PICKER_ID` と同じ流儀。定数は plugin_db.rs に 1 回だけ置く） |
| `NativeParams::kind()` / `default_of(kind)` | 既定値は組み込みも picker 追加も同じ。違うのは `bypassed` だけ |
| `NativeParams::get(p) -> Option<f32>` | `On` / 種類違い / `!p.exists()` なら None。段階式は段 index を返す |
| `NativeParams::set(p, v) -> bool` | `On` / 種類違い / `!p.exists()` / 非有限値は書かずに false。`p.range().clamp(v)` し、段階式は段 enum へ丸める（master_strip.rs:411-422 から移設） |
| `NativeParams::sanitize()` | **フィールド単位**で回す（住所の集合ではない）。Comp: 6 フィールド。Eq: `EqBand::ALL` × {freq, gain, q} の 18 組。LF/HF の q は Bell のとき DSP が使う（channel_strip_dsp.rs:231-244）ので含める。BusComp: threshold / makeup。ToneEq: 3 ゲイン。非有限値はそのフィールドの既定値、有限値は clamp。冪等 |
| `NativeDevice::new_builtin(kind, id)` | `builtin=true, ordinal=1, bypassed=true, params=default_of(kind)`（今の既定 `on: false` と同じ音: channel_strip.rs:347, 431、master_strip.rs:330） |
| `NativeDevice::new_added(kind, id, ordinal)` | `builtin=false, bypassed=false` |
| `NativeDevice::kind()` | `params.kind()` |
| `NativeDevice::param(p) -> Option<f32>` | `On(k)` で k が自分の種類なら、bypassed のとき `Some(0.0)`、そうでなければ `Some(1.0)`。それ以外は `params.get(p)` |
| `NativeDevice::set_param(p, v) -> bool` | `On(k)` で k が自分の種類なら `bypassed = v < 0.5`。それ以外は `params.set(p, v)`。**自動 ON はしない**（§10.1 の `NativeEdit::apply` が担う） |
| `NativeDevice::replace_values(bypassed, params) -> bool` | 種類違いは false。`params.sanitize()` 後に置き換え、変化があれば true（`SetNativeDevice` の受信側と engine が使う） |
| `NativeDevice::sanitize()` | `params.sanitize()`。構造（builtin / ordinal / aux）には触らない |
| `NativeDevice::display_name() -> Cow<'static, str>` | `ordinal <= 1` なら `kind.label()`、それ以外は `"{label} {ordinal}"` |
| `NativeDevice::can_activate(lanes, routings) -> bool` | `!bypassed` \|\| enabled な lane の target が `NativeParam{id, On(kind)}` \|\| enabled な routing の target が同じ。**compile / SC 会計 / tap の emit は、処理しうるかをこの 1 本で判定する** |
| `NativeDevice::sidechain_input() -> Option<&AuxInputRoute>` | `accepts_sidechain` のときだけ `aux_input.as_ref()` |
| `GR_METER_RANGE_DB: f32 = 20.0` | 重複 2 つを 1 本にする |

**Settings**
- 各 container に `#[serde(default)]` を付ける。
- `CompSettings.on`（channel_strip.rs:410）、`CompSettings.sc_listen`（:425）、`EqSettings.on`（:335）、`MasterCompSettings.on`（master_strip.rs:260）、`MasterEqSettings.on`（:291）は**削除**する。
- EQ バンドごとの `EqBandSettings.on`（channel_strip.rs:314）は残す。
- `CompSettings::effective()`（:474）/ `CompMode::overrides`（:287）/ `EqBand::{has_q_knob, has_bell_switch, freq_range}` はそのまま移す。
- `EqBand::ALL`（:145、DSP の段順 `[Hp, Lp, Lf, Lmf, Hmf, Hf]`）は残す。加えて `EqBand::BY_FREQ = [Hp, Lf, Lmf, Hmf, Hf, Lp]` を新設する（Q12 の列順とカーブ点の順）。
- `BusCompRatio::LABELS = ["2:1","4:1","10:1"]`、`BusCompAttack::LABELS = ["0.1ms","0.3ms","1ms","3ms","10ms","30ms"]`、`BusCompRelease::LABELS = ["0.1s","0.3s","0.6s","1.2s","Auto"]`。

### 5.3 NativeParamId / MasterLimiterParam / ParamRange

```rust
// common/src/model/native_param.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum NativeParamId {
    On(NativeKind),                        // 値域 Toggle (= !bypassed)。種類を持つので song を引かずに色・照合が決まる
    Comp(CompParam),                       // channel_strip.rs:219-229 の 6 値
    Eq { band: EqBand, param: EqParam },   // 実在は 12 組 (exists())
    BusComp(BusCompParam),                 // { Threshold, Ratio, Attack, Release, Makeup }
    ToneEq(ToneEqBand),                    // { Low, LoMid, High }
}
impl NativeParamId {
    pub fn kind(self) -> NativeKind;
    /// Eq: Hp/Lp=Freq、Lf/Hf=Freq/Gain、Lmf/Hmf=Freq/Gain/Q。他の種類は常に true。
    pub fn exists(self) -> bool;
    pub fn range(self) -> ParamRange;
    pub fn knob_label(self) -> &'static str;            // "Thr" / "Freq" / "Ratio" / "On"
    pub fn lane_label(self) -> &'static str;            // "Thr" / "HMF Gain" / "Ratio" / "Low" / "On"
    /// その種類の全住所。On を先頭に、Q12 の列順。
    /// Comp=[On,Thr,Rat,Atk,Rel,SC,Gain] / Eq=[On,HP Freq,LF Freq,LF Gain,LMF Freq,LMF Gain,LMF Q,HMF Freq,HMF Gain,HMF Q,HF Freq,HF Gain,LP Freq]
    /// BusComp=[On,Thr,Ratio,Atk,Rel,Makeup] / ToneEq=[On,Low,LoMid,High]
    pub fn all_of(kind: NativeKind) -> &'static [NativeParamId];
    pub fn default_plain(self) -> Option<f32>;          // On は None
    pub fn step_labels(self) -> Option<&'static [&'static str]>; // BusComp(Ratio/Attack/Release) だけ Some
    pub fn eq_band(self) -> Option<EqBand>;
}
/// レーン名 / gesture 名の SSoT。plugin と同じ形 (handler/view_model.rs:509-545, :530)。
pub fn native_param_label(device_name: &str, p: NativeParamId) -> String; // "{device_name}: {lane_label}"

// common/src/model/master_limiter.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum MasterLimiterParam { On, Ceiling }
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(default)]
pub struct MasterLimiterSettings { pub on: bool /*既定 false*/, pub ceiling_db: f32 /*既定 -1.0*/ }
impl MasterLimiterParam { pub fn range(self) -> ParamRange; pub fn label(self) -> &'static str; pub fn default_plain(self) -> f32; }
impl MasterLimiterSettings {
    pub fn param(&self, p: MasterLimiterParam) -> f32;
    pub fn set_param(&mut self, p: MasterLimiterParam, v: f32) -> bool; // 非有限は false。On は 0.5 閾値
    pub fn sanitize(&mut self);                                          // 非有限 ceiling は既定値
}
```

**オートメーション対象の決定表**

| 旧住所 | 新住所 | 対象 | 根拠 |
|---|---|---|---|
| `TrackBuiltinParam::StripCompOn` / `StripEqOn` | `On(Comp)` / `On(Eq)` | ○ | Q → last_touched → A でレーンができる（handler/mixer.rs:465-475、view/root.rs:612-615）。native は遅延 0 |
| `MasterStripParam::CompOn` / `EqOn` | `On(BusComp)` / `On(ToneEq)` | ○ | 4 種で揃える |
| `StripComp{p}` | `Comp(p)` | ○ | |
| `StripEq{band,param}`（18 組） | `Eq{band,param}`（12 組） | ○ | HP/LP の Gain・Q と LF/HF の Q にはノブも点も無い（strip_sections.rs:475-509、Q12）。sanitize の対象には残す |
| `MasterStrip` の Comp 系 5 つ | `BusComp(..)` | ○ | Ratio / Attack / Release は Stepped |
| `MasterStrip` `EqGain(b)` | `ToneEq(b)` | ○ | |
| `MasterStrip` `LimiterCeiling` / `LimiterOn` | `MasterLimiter(Ceiling)` / `MasterLimiter(On)` | ○ | On の遅延会計は compile 時に焼く（§8.3.4） |
| CompMode / バンドの ON / Bell | なし | × | 今と同じく対象外（event.rs:2161-2172）。`NativeEdit` で編集する |
| SC Listen | なし | × | Song の外に置く（§8.7 / §10.14） |

**`ParamRange`**（channel_strip.rs:17-124 を移設し、境界を f64 にする）

```rust
pub enum ParamRange { Linear{lo:f64,hi:f64}, Log{lo:f64,hi:f64}, LogWithOff{lo:f64,hi:f64}, Toggle, Stepped{count:u8} }
impl ParamRange {
    pub const OFF_SPAN: f64 = 0.02;
    pub fn to_norm(self, plain: f64) -> f64;   // 既存 3 種は channel_strip.rs:39-54 と同式。Toggle: >=0.5→1/0。Stepped: plain/(count-1)
    pub fn from_norm(self, norm: f64) -> f64;  // Toggle: >=0.5→1/0。Stepped: n*(count-1) (連続。段丸めは clamp)
    pub fn clamp(self, plain: f32) -> f32;     // Toggle: 0/1。Stepped: round して 0..=count-1
    pub fn is_affine(self) -> bool;            // Linear のみ
    pub fn is_invertible(self) -> bool;        // Linear | Log
    pub fn display_range(self) -> (f64, f64);
}
```

- 非有限値の処理は `clamp` の責務にせず、各 `sanitize` が持つ。
- device.rs:262 の `SPLIT_FREQ_RANGE` と :265 の `SELECTOR_FADE_RANGE` は `super::ParamRange` のまま使える（境界リテラルを f64 にするだけ）。

**`NativeParamId::range()`**

| 住所 | ParamRange |
|---|---|
| `On(_)` | Toggle |
| `Comp(p)` | `CompParam::range`（channel_strip.rs:233-241） |
| `Eq{band,param}` | `EqParam::range(band)`（:200-206） |
| `BusComp`: Threshold / Ratio / Attack / Release / Makeup | Linear{-30,0} / Stepped{3} / Stepped{6} / Stepped{5} / Linear{-5,15} |
| `ToneEq(_)` | Linear{-6,6} |
| `MasterLimiter(On)` / `MasterLimiter(Ceiling)` | Toggle / Linear{-6,0} |

### 5.4 Track / Song / ViewState

- **`Track.strip`**（track.rs:67-73, :265）と `pub mod channel_strip; pub use`（track.rs:6-10）を削除する。
- **`Song.master_strip`**（model.rs:624-630, :740）を削除し、`#[serde(default)] pub master_limiter: MasterLimiterSettings` を足す。doc には信号順を書く:「合算 → master_fx_chain（組み込み Bus Comp / Tone EQ を含む）→ master_gain → Limiter」。
- **`Song::master_limiter_latency_active(&self) -> bool`** を新設する。定義は `master_limiter.on` \|\| `song_lanes` に enabled な `MasterLimiter(On)` レーンがある \|\| `song_mod_routings` に同じ target の enabled な routing がある。PDC と DSP 遅延の SSoT。
- **`ViewState`**（common/src/model/view_state.rs:126-127 の `collapsed_parallel_nodes` の後）に次を足す。

```rust
/// Rack の Par パネル 1 枚の鍵 (r.md #129 Q18)。serde 専用で IPC を渡らない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RackPanelKey { Device(u64), MasterLimiter }
// ViewState
/// 開いている Rack Par (plugin / 映像 FX / VOICEVOX / 字幕 / Transform / Native / Limiter)。
/// 「見方の都合」なので dirty は立てないが保存する。save 時に sort、存在しない device は落とす。
#[serde(default)]
pub open_rack_panels: Vec<RackPanelKey>,
```

view_state.rs は WIRE_SOURCES の対象外（ファイル doc :1-3）。

### 5.5 Device 走査ヘルパー（device.rs）

| 関数 | 変更 |
|---|---|
| `Device::{id, bypassed, set_bypassed}`（:440-460） | Native の arm を足す |
| `as_plugin(_mut)` / `as_parallel(_mut)`（:462-488） | `Native(_) => None` |
| 新設 `as_native(_mut)` / `is_builtin_native()` / `impl From<NativeDevice> for Device` | |
| 新設 `Device::aux_input(port) -> Option<&AuxInputRoute>` / `aux_input_slot_mut(port) -> Option<&mut Option<AuxInputRoute>>` / `aux_input_port_count() -> u8` | plugin: `min(aux_input_count, MAX_AUX_IN)` と `aux_inputs[port]`（`aux_input_count: u8`、model.rs:2788）。slot_mut は port+1 まで伸ばす。Native: `accepts_sidechain` なら port 0 だけ。Parallel: 0 |
| `routes_any_aux_output()`（track.rs:438-446 が参照） | Native は false |
| `any_plugin`（:517-525）/ `PluginIter::next`（:543-559）/ `for_each_plugin_mut`（:563-574）/ `plugins()`（:530）/ `Song::all_plugins`（:947） | Native は読み飛ばす（host に載らない。`compute_slot_reconcile_actions` も同じ） |
| `for_each_node_id_mut`（:626-639） | `Native(n) => f(&mut n.id)`。不変版 `for_each_node_id` を新設 |
| `find_device_in` / `device_in(_mut)` / `remove_device_in` / `chain_*` / `for_each_chain*` / `for_each_parallel*` | 変更不要（`d.id()` と `if let Parallel` で書かれている） |
| 新設 `for_each_native(_mut)` / `any_native` / `native_in(_mut)(devices, id)` | 確保なし、RT から呼べる。Parallel の中も辿る |
| 新設 `for_each_aux_input(devices, f(device_id, port, &AuxInputRoute))` / `for_each_aux_slot_mut(devices, f(device_id, port, &mut Option<AuxInputRoute>))` | plugin と Native を 1 本で辿る |
| 新設 `remove_natives_in(devices: &mut Vec<Device>)` | Parallel の中も含めて Native を除く（Bounce In Place / Glue の pre_fx 専用、§10.15） |
| 新設 `Song::{native_by_id(_mut), builtin_native(owner, kind) -> Option<&NativeDevice>, builtin_natives(owner) -> impl Iterator<Item=&NativeDevice>}` | `builtin_natives` はチェーン順（最上位のみ）。owner は `Track::id` か `MASTER_TRACK_ID`（`fx_chain_by_track_id` model.rs:1672 と同じ sentinel 分岐） |
| 新設 `Song::set_aux_input(device_id, port, source: Option<TapSource>) -> bool` | devices.rs:726-744 の規則を model に移す（自トラックが source なら PreFx 固定、tap_point は既存を引き継ぐ）。§5.9 の `would_cycle` なら false。plugin と Native で共有する |

### 5.6 IdAllocators

- `IdAllocators::alloc_device_id` を新設し、`Song::alloc_device_id`（model.rs:849-853）はこれに委譲する。借用を分けた `let Song { tracks, master_fx_chain, ids, .. } = self` の形で、正規化から採番できるようにするため。

### 5.7 組み込みの規則（`common/src/model/native/chain_rules.rs`、wire ではない）

```rust
impl Song {
    pub fn normalize_native_devices(&mut self) -> bool;                       // 冪等
    pub fn enforce_edit_invariants(&mut self) -> bool;                        // normalize_native_devices() | prune_dangling_param_targets()
    pub fn default_insert_index(&self, dest: ChainRef) -> Option<usize>;      // None = dest が無い
    pub fn next_native_ordinal(&self, owner: u32, kind: NativeKind) -> u16;
    pub fn assign_native_ordinals(&self, dest_owner: u32, devices: &mut [Device], policy: OrdinalPolicy);
    pub fn prepare_device_copies(&mut self, dest_owner: u32, devices: &mut [Device]);
    pub fn is_builtin_native(&self, id: u64) -> bool;
    pub fn can_relocate(&self, id: u64, dest: ChainRef, copy: bool) -> bool;
}
pub enum OrdinalPolicy { Fresh, KeepIfFree }
pub fn default_insert_index_in(devices: &[Device], is_master: bool) -> usize;
```

**`default_insert_index_in`（Q6）**
- 最上位で最後にある「組み込み以外」の index + 1 を返す。
- 組み込み以外が 0 個なら、通常トラックは最初の組み込みの index（chain が空なら 0）、master は最後の組み込みの index + 1。
- `Song::default_insert_index(dest)` は、`ChainRef::Track(t)` なら上の関数、`ChainRef::Chain(c)`（Parallel の中）なら chain の末尾（len）を返す。

| devices | is_master | 結果 |
|---|---|---|
| `[Synth, Comp(b), EQ(b)]` | false | 1 |
| `[Comp(b), Synth, EQ(b)]` | false | 2 |
| `[Comp(b), EQ(b)]` | false | 0 |
| `[Comp(b), EQ(b), Rev]` | false | 3 |
| `[Bus(b), Tone(b)]` | true | 2 |
| `[Bus(b), Tone(b), Rev]` | true | 3 |

**番号（Q10、K9）**
- `taken(kind)` は、dest トラックの木全体（Parallel の中を含む）でその種類が使っている ordinal と、その一括処理で既に振った ordinal の和集合。
- `next(taken)` は、taken が空なら 1、それ以外は `min{ n ≥ 2 | n ∉ taken }`。
- `Fresh` は常に next を使う。`KeepIfFree` は `1 ≤ o` かつ `o ∉ taken` なら o を保ち、それ以外は next。
- 使い分け: picker 追加 → `next_native_ordinal`、コピー → `Fresh`、トラックを跨ぐ移動 → `KeepIfFree`。同じトラック内の並べ替えと group / ungroup では番号を触らない。
- 例: Bus Comp 2 だけがあるトラックに Bus Comp を足すと 3。空のトラックに足すと 1（番号なし）。一括コピー 2 個は 2, 3。master で足した「Comp」（1）を組み込み Comp のある通常トラックへ移すと 2。

**`prepare_device_copies(dest_owner, devices)`**
1. `for_each_node_id_mut` で全ノードに新しい id を振る。
2. Native は `builtin=false, ordinal=0` にし、`sanitize()` する。
3. `assign_native_ordinals(dest, devices, Fresh)` を呼ぶ。

**一括で 1 回だけ**、splice の前に呼ぶ。

**`can_relocate(id, dest, copy)`** は `!dest_inside_device(id, dest)` && (`copy` \|\| !組み込み \|\| dest が所属トラックの最上位)。
- 前半は Parallel を自分の中の chain へ落とす循環の禁止。今は chain_list.rs:119-129 と device_relocate.rs:430-439 の 2 か所にあり、ここに 1 本化する。
- 後半が Q5。

**`normalize_native_devices`**（chain ごとに借用を分ける）
1. Parallel の中にある builtin を降格する（`builtin=false`、位置はそのまま、ordinal は手順 5 で直す）。
2. 最上位で、役割違いの builtin（通常トラックは {Comp, Eq} 以外、master は {BusComp, ToneEq} 以外）と、同じ種類の 2 個目以降の builtin を降格する。
3. 足りない種類を `NativeDevice::new_builtin(kind, ids.alloc_device_id())` で補う。通常トラックは末尾に `BUILTIN_TRACK` 順、master は先頭に `BUILTIN_MASTER` 順。
4. `!accepts_sidechain` の device の `aux_input` を None にする。
5. ordinal を直す。builtin は 1。それ以外は pre-order で走査し、有効なもの（1 以上、重複なし、同じ種類の builtin の 1 と衝突しない）を taken に入れ、無効なものに `next(taken)` を振る。

**正規化はガードの代わりにしない。** 補充は既定値・bypass で行われ、そのレーンは prune で消える。つまり補充が起きた時点で、どこかのガードが漏れたというバグの症状になる。これは §15.7 の R-2 テストで検出する。

### 5.8 不変条件を保つ単一の口

**`Song::enforce_edit_invariants()` を SongDoc の 5 つの口で呼ぶ**（K5）

| 口 | 位置 | 呼び方 |
|---|---|---|
| `SongDoc::new` | song_doc.rs:164 | baseline 確定前（`*` は立たない） |
| `edit_impl` | :216-265 | closure の後に `changed \|= song.enforce_edit_invariants()`（同じ undo step） |
| `normalize` | :272-281 | 同上 |
| `normalize_checked` | :294-305 | 同上 |
| `replace_song` | :502-511 | baseline 確定前 |

- 次の口からは**呼ばない**: `edit_playback`（:315、ランチャーの再生状態だけ）、`write_back_plugin_state`（:345、blob だけ）、`rewrite_history`（:381、履歴の path 移行）、undo/redo（snapshot は正規化済み）。
- SongDoc を通らない 2 か所だけ明示的に呼ぶ: `isolated_track_song`（bounce.rs:182）と `normalize_after_load`（model.rs:1475）。
- これで `Track::default()` を使う 25 か所（テストを除く）で新規トラックに組み込みが入る。
- handler 側の明示的な prune 呼び出し 7 か所と `remap_device_refs_after_remove` は削除する（§14.2）。

**`Song::prune_dangling_param_targets(&mut self) -> bool`**（`model/param_address.rs`。model.rs:1486-1536 の `prune_dangling_mod_targets` を置き換える）

変化が無くなるまで次を繰り返す（固定点、冪等、確保は off-RT のみ）。

1. 全チェーンを 1 回辿り、node 表 `HashMap<u64, (owner: u32, NodeKind { Plugin, Native(NativeKind), Parallel, Chain })>` と、生きている ModSource / routing の集合を作る。
2. 各 store（track の `automation_lanes` / `mod_routings`、master の `song_lanes` / `song_mod_routings`）の lane と routing を、次の条件で残すか判定する。

| target | 残す条件 |
|---|---|
| `PluginParam{device_id}` | node 表で Plugin、かつ owner がこの store |
| `NativeParam{device_id, param}` | node 表で Native、owner が一致、`param.kind() == kind`、`param.exists()` |
| `ChainGain` / `ChainPan{chain_id}` | Chain、owner 一致 |
| `ParallelOutGain` / `ParallelSplitFreq` / `ParallelSelect{parallel_id}` | Parallel、owner 一致（split のモードは問わない。既存方針 model.rs:1492-1495） |
| `MasterLimiter(_)` / `SongTempo` / `SongTimeSigNumerator` | store が master |
| `ModSourceParam` / `ModRoutingDepth` | 現行（model.rs:1508-1516）。routing は `live_sources.contains(source_id)` も見る |
| その他（Volume / Pan / Mute / SendGain / Image / Text / Group） | 変更なし |

3. `midi_bindings` の `BindingTarget::{PluginParam, NativeParam}` は同じ node 表で判定する。`MasterLimiter` は常に残す。
4. ModRoutingDepth の連鎖も同じループで落ちる。

**`AutomationTarget::bound_node_id(&self) -> Option<u64>` / `bound_node_id_mut`**（K6）
- `PluginParam` / `NativeParam` / `Chain*` / `Parallel*` で Some を返す。
- 網羅 match で書き、`_` の arm を書かない。

### 5.9 配線の依存（`common/src/routing_deps.rs`、wire ではない）

**問題**
- 依存辺は children / SC / send の 3 種類（daw_audio/src/graph/compile.rs:305-337）。
- 循環すると `GraphError::Cycle`（:364）になり、空の schedule になって master が無音になる（project_ctl.rs:316-329）。
- GUI の SC 候補は自分自身を除くだけ（handler/parallel.rs:550-580）、send は自分宛てを弾くだけ（handler/mixer.rs:583-605）。
- Q19 で全 group / return の組み込み Comp に SC▾ が付くと、「子 A の Comp の SC に親 group G を選ぶ」だけで循環する。

```rust
pub enum EdgeScope { Active, Structural }
pub struct TrackDeps { ids: Vec<u32>, index: HashMap<u32, usize>, deps: Vec<Vec<usize>> }
impl TrackDeps {
    /// compile.rs:305-337 の辺の定義をここへ移す (SSoT)。children → group / SC consumer → source track
    /// (TapSource::Chain は chain_owner_track、同じ track の chain は辺にしない compile.rs:316-317) / send src → dest。
    /// Active: plugin は !bypassed、native は can_activate。Structural: 配線がある consumer は全部。
    pub fn build(song: &Song, scope: EdgeScope) -> Self;
    pub fn dependency_order(&self) -> Result<Vec<u32>, DependencyCycle>;
    pub fn would_cycle(&self, consumer: u32, producer: u32) -> bool;   // consumer==producer || producer が consumer に推移的に依存
}
pub struct AuxConsumer<'a> {
    pub device_id: u64,
    pub inactive: bool,                        // plugin: bypassed / native: !can_activate
    pub audio_io: bool,                        // audio in+out を持つ (input delay の対象)
    pub routes: &'a [Option<AuxInputRoute>],   // native は std::slice::from_ref(&nd.aux_input)
    pub native: bool,
    pub top_index: u32,                        // この consumer を含む最上位 device の index (§8.3.3 の pass 判定)
}
/// 非 RT、信号順。
pub fn aux_consumers<'a>(devices: &'a [Device], lanes: &'a [AutomationLane], routings: &'a [ModRouting]) -> AuxConsumerIter<'a>;
impl Song {
    pub fn all_aux_consumers(&self) -> impl Iterator<Item = (u32 /*owner*/, AuxConsumer<'_>)>;
    pub fn can_add_send(&self, src: u32, dest: u32) -> bool;          // Structural で !would_cycle
    pub fn drop_cyclic_aux_routes(&mut self, track_ids: &[u32]) -> bool; // 1 本ずつ判定して落とす
}
```

- engine の `graph/compile/deps.rs` は `TrackDeps::build(song, EdgeScope::Active).dependency_order()` を使う。辺の定義を 2 本持たない。
- **Song 側のガードは Structural で判定する**（bypass 中の配線を後で ON にすると循環しうるため）。
  - `set_aux_input` は `would_cycle` なら false。
  - `add_send` は `can_add_send` で拒否する。
  - 貼り付けやトラックを跨ぐ移動で持ち込まれた aux 経路は、`resolve_aux_refs_after_paste` と跨ぐ移動の後で `drop_cyclic_aux_routes` を呼んで落とす。
- 読み込んだファイルに既存の循環がある場合は今と同じ（無音 + warn）。

### 5.10 MIDI binding の型（`model/midi_bind.rs:73-124`、WIRE_SOURCES 登録済み common/build.rs:36）

`BindingTarget` に次を足す。

```rust
NativeParam { device_id: u64, param: NativeParamId },
MasterLimiter(MasterLimiterParam),
```

Learn と適用は §8.9。

### 5.11 DSP 部品（`common/src/channel_strip_dsp.rs` → `common/src/dsp.rs`）

- `pub mod dsp;` にする（lib.rs:6）。Biquad は Parallel の帯域分割も使うので、native 専用の名前にしない（daw_audio/src/graph/band_split.rs:23）。
- `eq_stages` の `on` 判定（:217-221）を削除する。
- `master_eq_stages` を `tone_eq_stages(&ToneEqSettings, sr)` にし、`on` 判定（:328-332）を削除する。
- `master_eq_magnitude_db` を `tone_eq_magnitude_db`（:347-351）にする。
- `db_to_amp` を追加する。
- テスト（:375-492）の `eq.on = true` を削除する。
- 公開 API: `Biquad, BiquadState, eq_stages, eq_magnitude_db, tone_eq_stages, tone_eq_magnitude_db, sc_filter, sc_filter_q, comp_static_gain_db, smoothing_coeff, amp_to_db, db_to_amp, bus_comp_auto_release_ms, limiter_gain_db`。GUI のカーブ描画と daw_audio が同じ関数を使う。

### 5.12 WIRE_SOURCES（common/build.rs:18-55）

- 削除: `src/model/track/channel_strip.rs`、`src/model/master_strip.rs`
- 追加（F）: `src/model/native.rs`、`src/model/native/{comp,eq,bus_comp,tone_eq}.rs`、`src/model/native_param.rs`、`src/model/master_limiter.rs`、`src/device_scope_bridge.rs`
- 追加（S1a）: `src/model/sections.rs`、`src/model/plugin_instance.rs`
- arch-lint に `WIRE-SOURCES` 検査を足す（F、`scripts/arch_lint.sh`）。common/src 下で `Encode` を derive / impl しているファイルが build.rs に全部載っているかを見る。書き方は POSIX ブラケット式 + canary。repr(C) の shmem 型（`device_scope_bridge.rs` など）はこの検査で拾えないので手動列挙のまま。現時点の登録は漏れが無いことを確認済みなので、誤検知 0 から始まる。

---

## 6. 保存と migration

### 6.1 版

- `CURRENT_VERSION` を 38 → **39**（model.rs:266）。version 履歴の doc（:205-265）に v39 の段落を足す。
- 保存形は `{"Native":{…}}`。

### 6.2 `common/src/project/native_migration.rs`（新設）

```rust
/// migrate_legacy_song (project.rs:512-518) の末尾 (migrate_flat_ids_to_allocators の後) から、版に依存せず呼ぶ。
pub(crate) fn migrate_strips_to_native(song: &mut Value);
/// トラック 1 本分。クリップボードの旧形式 (§6.6) でも再利用する。
pub(crate) fn migrate_strips_in_track_value(track: &mut Value, next_id: &mut u64);
```

- **版に依存しない理由**: `migrate_legacy_song` は全 load 経路（ファイル / `appLoadSongJson` script.rs:1132, :1136 / `loadSongFromObject` script.rs:637, :645 / export_bench.rs:36）が通る唯一の口（project.rs:506-518）。VALUE_MIGRATIONS はファイル load 専用（project.rs:850-854）。旧形と新形は重ならないので冪等になる。

**処理**
1. **判定**: `tracks[].strip` / `master_strip` / `TrackBuiltin` の `StripEqOn|StripCompOn|StripEq|StripComp` / `MasterStrip` のどれも無ければ return。
2. **id の起点**: `next = max(ids.next_device_id, max_node_id(全 tracks[].devices と master_fx_chain を再帰) + 1, 1)`。`ids.next_device_id` が遅れているファイルでも衝突しない。
3. **トラックごと**（`migrate_strips_in_track_value`）:
   1. `devices` が無ければ `[]` を作る（`skip_serializing_if = Vec::is_empty` なのでキーが無いことがある: track.rs:63）。
   2. `strip` を取り出す（無ければ既定値）。`comp.on` / `eq.on` を抜いて `bypassed = !on` にする。`sc_listen` は元々 JSON に無い（serde(skip): channel_strip.rs:424）。
   3. 最上位に同じ種類の組み込みがあれば（**混在形**。生産者は tests/scripts/glue_bake_parity.js:252）、params と bypassed を上書きしてその id を使う。無ければ末尾に `Native Comp builtin` → `Native Eq builtin` を追加し、`next` から id を振る。
   4. そのトラックの `automation_lanes[*].target` / `mod_routings[*].target` を §6.3 の表で書き換える（実 id を入れる）。
   5. トラックのレーンに紛れた `MasterStrip` の target は削除する（残すと unknown variant で deserialize が落ちる）。
4. **master**:
   1. `master_fx_chain` が無ければ作る（model.rs:609）。
   2. `master_strip.limiter` を `song.master_limiter` に移す。
   3. comp / eq を BusComp / ToneEq の builtin として index 0 / 1 に入れる（混在形なら上書き）。
   4. `song_lanes` / `song_mod_routings` を書き換え、song 側にある `Strip*` は削除する。
5. `ids.next_device_id = next`（`ids` が無ければ作る）。

- helper は `max_node_id` / `take_section_on` / `install_builtin` / `rewrite_targets(list, |t| Rewrite::{Keep, Replace, Drop})` / `legacy_track_target` / `legacy_master_target` の 6 本。各 60 行以内、nesting 6 段未満にする（既存 `migrate_legacy_clip_content` の 7/3 違反 baseline:315 を増やさない）。
- 旧型は Rust の型として残さない。
- migration は決定的なので、v38 を開いて保存し、もう一度開いても同じ id になる（dirty-on-open 契約、r.md #9）。

### 6.3 target の書き換え表（実 id を入れる。sentinel は作らない、K3）

`comp` / `eq` / `bus` / `tone` は、そのトラック（または master）の組み込み device id。

| 旧 JSON | 新 JSON |
|---|---|
| `{"TrackBuiltin":"StripCompOn"}` | `{"NativeParam":{"device_id":comp,"param":{"On":"Comp"}}}` |
| `{"TrackBuiltin":"StripEqOn"}` | `{"NativeParam":{"device_id":eq,"param":{"On":"Eq"}}}` |
| `{"TrackBuiltin":{"StripComp":{"param":P}}}` | `{"NativeParam":{"device_id":comp,"param":{"Comp":P}}}` |
| `{"TrackBuiltin":{"StripEq":{"band":B,"param":P}}}` | `{"NativeParam":{"device_id":eq,"param":{"Eq":{"band":B,"param":P}}}}`。(B,P) が実在しない 6 組の lane / routing は **削除**（ノブが無く GUI から作れない組） |
| `{"MasterStrip":"CompOn"}` / `"EqOn"` | `{"NativeParam":{"device_id":bus,"param":{"On":"BusComp"}}}` / `{..tone..{"On":"ToneEq"}}` |
| `{"MasterStrip":"CompThreshold"}`（Ratio / Attack / Release / Makeup も同様） | `{"NativeParam":{"device_id":bus,"param":{"BusComp":"Threshold"}}}` 等 |
| `{"MasterStrip":{"EqGain":B}}` | `{"NativeParam":{"device_id":tone,"param":{"ToneEq":B}}}` |
| `{"MasterStrip":"LimiterCeiling"}` / `"LimiterOn"` | `{"MasterLimiter":"Ceiling"}` / `{"MasterLimiter":"On"}` |

- `AutomationTarget::NativeParam.device_id` に `serde(default)` は付けない（型付き model に「未解決参照」の状態を作らない）。
- 旧 master On は `Linear{0,1}` の連続写像（master_strip.rs:210-212）で、新しい `Toggle` とは中間値の norm が違う。ただし master の On レーンは GUI から作れなかったので（handler/mixer.rs:509-519、master_strip_ui.rs:404 に gesture が無い）、実データへの影響は無い。

### 6.4 load の順序（`load_project` と `normalize_after_load` model.rs:1464-1484）

1. `migrate_legacy_song`（→ `migrate_strips_to_native`）
2. VALUE_MIGRATIONS
3. deserialize
4. SONG_MIGRATIONS（字幕 device の補完 common/src/project.rs:673 は `default_insert_index_in` にする。§6.5）
5. `normalize_after_load`
   1. `sanitize_ranges`（model.rs:1379-1430）に全 native の `sanitize` と `master_limiter.sanitize()` を足す。**daw_audio の LoadSong でも走る**（project_ctl.rs:799）ので、値の clamp だけにし、構造には触らない。
   2. `ensure_ids`
      - Transform 補完（model.rs:1752）を `default_insert_index_in` にする。
      - device id の採番（:1854-1874）で、id 0 に採番する前に `next = max(next_device_id, 既存最大 id + 1)` を取る。補完した組み込みが既存 id と衝突して plugin が再採番され、PluginParam レーンが外れる事故を防ぐ。
      - その後に `normalize_native_devices()` を呼ぶ（K4）。
      - :1945 の early return は helper `patch_remapped_track_refs` に切り出して解消する。Native の aux の track id 張り替え（:1967-1973）は `for_each_aux_slot_mut` にする。
   3. `ensure_midi_binding_inputs`
   4. `prune_dangling_param_targets`（:1475 の単独呼び出しを置き換える）
   5. 以降は現行どおり
6. GUI の `migrate_legacy_vocal_tracks`（handler/project.rs:645 → :1161）は `default_insert_index_in` にする（§6.5）。
7. `SongDoc::replace_song` → `enforce_edit_invariants`（ここまでで正規化済みなので no-op）→ baseline 確定。

script 経路（`migrate_legacy_song` + `ensure_ids` だけ）でも、組み込みの補完と実 id の target が揃う。

### 6.5 既存の「末尾 push」を Q6 の位置へ

末尾に push すると EQ の後ろに音源が来て、EQ が楽器の音を処理しなくなる（Q4 の直結規則）。つまり**既存曲の音が変わる**。

| 箇所 | 変更 |
|---|---|
| handler/project.rs:1161（`migrate_legacy_vocal_tracks`、VOICEVOX 旧 vocal の移行） | `insert(default_insert_index_in(&track.devices, false), ..)` |
| common/src/project.rs:673（字幕 device の移行） | 同上 |
| common/src/model.rs:1752（`ensure_ids` の Transform 補完） | 同上 |

### 6.6 クリップボードの旧形式（`daw_gui/src/clipboard.rs`）

**事実**
- `TracksCopy` は raw の `Track` を持つ（clipboard.rs:306-325）。`from_json` は直接 deserialize する（:358-364）。
- 版を上げない方針（:21-31）なので、旧ビルドでコピーしたトラックを新ビルドに貼ると次のどちらかになる。
  - `strip` キーが黙って捨てられ、設定が消える。
  - `StripComp` 等のレーンで decode に失敗し、no-op になる。

**設計**
- `ClipboardEnvelope::from_json` を `Value` 経由にする。magic を照合 → payload が `Tracks` なら各 `track` に `migrate_strips_in_track_value(track, &mut next_id)` を適用 → `from_value`。
- `next_id` は payload 内の最大 node id + 1。貼り付け時に id は振り直されるので、衝突しない。
- `Devices` / 点 / クリップの payload は strip を含まないので対象外。
- `sanitize_devices` / `sanitize_tracks`（:581-665、:657-661 は plugin だけ）で `for_each_native_mut(.., NativeDevice::sanitize)` を呼ぶ（信頼境界）。

### 6.7 保存時の ViewState

- `snapshot_view_state`（handler/view_state.rs:32-119）: 存在する device の `RackPanelKey::Device(id)` と `MasterLimiter` を sort して書く（`plugin_editor_windows` と同じ規則 :76-83）。
- `restore_view_state`（:136-152）: 冒頭で `open_rack_panels.clear()` し（`view=None` の旧ファイルでも前プロジェクトの状態を持ち越さない）、存在するものだけ入れる。

---

## 7. オートメーション / 変調

### 7.1 正規化の単一の表（`common/src/automation.rs`）

```rust
/// target の plain 値域と写像の種類。正規化の唯一の表。`_` を書かない網羅 match。
pub fn target_range(target: &AutomationTarget, plugin_range: Option<(f64, f64)>) -> ParamRange;
pub fn plain_to_norm_ranged(t, plain, pr) -> f32 { target_range(t, pr).to_norm(plain) as f32 }
pub fn norm_to_plain_ranged(t, norm, pr) -> f64 { target_range(t, pr).from_norm(f64::from(norm)) }
pub fn norm_mapping_is_affine(t) -> bool     { target_range(t, None).is_affine() }
pub fn norm_mapping_is_invertible(t) -> bool { target_range(t, None).is_invertible() }
```

| target | ParamRange | 現行との関係 |
|---|---|---|
| Volume / SendGain / ChainGain / ParallelOutGain | Linear{0,2} | 同じ（automation.rs:61, :70, :72-74） |
| Pan / ChainPan | Linear{-1,1} | 同じ |
| Mute | Toggle | 同じ（:63-69, :227） |
| ParallelSplitFreq | `SPLIT_FREQ_RANGE` | 写像は同じ。**affine は false に訂正**（今は `_ => true` に落ちている: :248） |
| ParallelSelect | Linear{0,1} | 同じ |
| NativeParam{param} | `param.range()` | track 由来は旧 Strip* と同じ。Stepped は旧 Linear{0,count-1} と to_norm が同じで、affine / invertible も旧 `!is_stepped` と同じ |
| MasterLimiter(p) | `p.range()` | 同じ |
| PluginParam | `Some((min,max))` で max>min なら Linear{min,max}、それ以外は Linear{0,1} | 同じ（:54-59, :102） |
| SongTempo / SongTimeSigNumerator | Linear{1,400} / Linear{1,32} | 同じ |
| Image / Text / Group の Rotation | Linear{-π,π} | 同じ |
| Text FontSize | Linear{1,4096} | 同じ |
| Group ScaleX / ScaleY | Log{0.1,10} | 同じ |
| Image / Text / Group のその他 | Linear{0,1} | 同じ（:148 の末尾 clamp） |
| ModSourceParam | `mod_param_range` が Some なら Log{min,max}、None なら Linear{0,1} | 同じ |
| ModRoutingDepth | Linear{-1,1} | 同じ |

### 7.2 置き場の解決（`common/src/model/param_address.rs`）

```rust
impl Song {
    /// owner (track id か MASTER_TRACK_ID) の store。置き場規則の唯一の実装。確保なし (RT 可)。
    /// 0 は解釈しない (legacy の 0 → MASTER は mod_source_owner model.rs:929-937 の責務)。
    pub fn param_stores(&self, owner: u32) -> Option<(&[AutomationLane], &[ModRouting])>;
    pub fn param_stores_mut(&mut self, owner: u32) -> Option<(&mut Vec<AutomationLane>, &mut Vec<ModRouting>)>;
    /// lane id を owner の allocator で必ず再採番して積む。戻り値は新 id。
    pub fn push_lane(&mut self, owner: u32, lane: AutomationLane) -> Option<u32>;
    /// id で束縛する target の store の持ち主。target だけでは決まらない住所は None。
    pub fn bound_owner_track(&self, target: &AutomationTarget) -> Option<u32>;
    /// model.rs:1602-1660 から移し、param_stores(_mut) の上に作り直す。
    pub fn automation_lane_by_key(&self, track_id: u32, lane_id: u32) -> Option<&AutomationLane>;
    pub fn automation_lane_by_key_mut(&mut self, track_id: u32, lane_id: u32) -> Option<&mut AutomationLane>;
}
```

| target | `bound_owner_track` |
|---|---|
| PluginParam{id} / NativeParam{id} / Parallel*{pid} | `device_owner_track(id)`（device.rs:908-914） |
| ChainGain / ChainPan{cid} | `chain_owner_track(ChainRef::Chain(cid))`（device.rs:893） |
| ModSourceParam / ModRoutingDepth | `mod_source_owner` / `mod_routing_owner` |
| MasterLimiter / SongTempo / SongTimeSigNumerator | Some(MASTER) |
| Volume / Pan / Mute / SendGain / Image / Text / Group | None（呼び出し側の track が持ち主） |

置き場の分岐を複製している 18 か所は、すべてこの口に寄せる（§14.3）。レーンと routing の置き場の規則は PluginParam と同じ: トラックの device → そのトラック、master fx chain の device → song_lanes / song_mod_routings。

### 7.3 engine での解決（`daw_audio/src/automation.rs`）

```rust
/// この buffer で効く native device の値 (レーン → 変調の順に重ねる。block-rate)。
/// store = song.param_stores(owner)、rows = owner の行 (master は rows.master_rows() execute.rs:1100)。
/// 確保・ロック無し (NativeDevice: Copy)。store が空なら *device を返す。
pub fn resolve_native_device(song: &Song, device: &NativeDevice, owner: u32, rows: TrackRows<'_>,
    playhead_beats: f64, recording_lanes: &HashSet<(u32, AutomationTarget)>, mod_plane: ModTickPlaneRef<'_>) -> NativeDevice;
/// store を解決済みで受け取る版 (RT はプログラム実行ごとに 1 回だけ param_stores を引く、§8.4.1)。
pub fn resolve_native_device_in(stores: (&[AutomationLane], &[ModRouting]), device: &NativeDevice, owner: u32,
    rows: TrackRows<'_>, playhead_beats: f64, recording_lanes: &HashSet<(u32, AutomationTarget)>, mod_plane: ModTickPlaneRef<'_>) -> NativeDevice;
/// master のフェーダー後リミッター (On / Ceiling)。store は song 側。
pub fn resolve_master_limiter(song: &Song, rows: TrackRows<'_>, playhead_beats: f64,
    recording_lanes: &HashSet<(u32, AutomationTarget)>, mod_plane: ModTickPlaneRef<'_>) -> MasterLimiterSettings;
```

- 手順は `resolve_track_strip`（automation.rs:202-257）と同じ。
  1. **レーン**: enabled で、`(owner, target)` が録音中でなく、`target == NativeParam{device.id, p}` のもの。値は `lane_value(.., phase_at_frame(rows.lane(li), 0), playhead)` で求め、`set_param(p, v)` する。On は `bypassed` を書く。
  2. **変調**: 同じ target の routing は `routings[..i]` に既出なら飛ばし、`apply_modulation_with` で畳んで `set_param` する。段階式は `set` が段へ丸める。
- `resolve_track_strip` / `resolve_master_strip`（automation.rs:189-316）は削除する。
- `fill_pd_param_events` の分岐（:364-374）は `let Some((lanes, routings)) = song.param_stores(track_id) else { return };` にする。
- `program.rs::track_stores`（:554-567）は削除し、呼び出し（:535, :582, :617）は `song.param_stores(track_id).unwrap_or((&[], &[]))` にする。
- `launcher/runtime.rs::row_of`（:1262-1277）は `automation_lane_by_key` を使う（トラック行の `lane_id==0` の分岐は残す）。
- `mod_graph.rs::song_has_lane`（:488-495）は `param_stores(if owner==0 { MASTER } else { owner })` にする。

### 7.4 ラベルと値表示

**song に依存しないラベル**（`daw_gui/src/automation_label.rs:42-68` を置き換え）

```rust
T::NativeParam { param, .. } => native_param_label(param.kind().label(), *param),  // "Comp: Thr"
T::MasterLimiter(p) => format!("Limiter: {}", p.label()),                        // "Limiter: Ceiling"
```

**song に依存するラベル**
- `AppData::plugin_param_name`（handler/view_model.rs:521-545）を `device_param_name` に改名し、arm を足す。

| target | 表示名 |
|---|---|
| NativeParam{id,p} | `native_param_label(&song.native_by_id(id)?.display_name(), p)` →「Comp 2: Thr」（K16） |
| ChainGain / ChainPan{cid} | `"{chain.name}: Gain"` / `"{chain.name}: Pan"` |
| ParallelOutGain / ParallelSplitFreq / ParallelSelect{pid} | `"{parallel.name}: Out"` / `"{parallel.name}: Split {edge}"` / `"{parallel.name}: Active"` |

- 呼び出し元: view_model.rs:557（`automation_target_label`）、:574（深さの行き先名）、widgets/arrangement/view_build.rs:213, :309。
- ipc.rs:346-351 の名前解決は削除する（handler が `automation_target_label` で作る。§7.6）。
- `ParamGestureBegin` から display_name を削除しても Chain / Parallel の名前が「Chain 1234 Pan」に退行しないよう、この表で Chain / Parallel 名を補う。

**レーン表示表**（view_build.rs:852-890 を置き換え）
- NativeParam の arm は `label = device_param_name(引数).map_or_else(純ラベル, intern)`。
- 色は song を引かずに種類で決める（view_build.rs:759-764 は song 無しで呼ばれる）。

| 種類 | 色 |
|---|---|
| `On(Comp|BusComp)` / `Comp(_)` / `BusComp(_)` / `MasterLimiter(_)` | (0.95, 0.65, 0.35) |
| `On(Eq|ToneEq)` / `Eq{..}` / `ToneEq(_)` | (0.40, 0.85, 0.80) |

**値表示**（`daw_gui/src/automation_value.rs:200-211, :229-283`）
- `range` は「表示単位での範囲」（:42-43）。plain と表示の単位が同じ arm だけが `target_range(..).display_range()` を引く。Volume / 回転 / Text px は現行のまま。

| 住所 | unit / format / range |
|---|---|
| On(_) | "" / Integer / display_range |
| Comp: Threshold, Makeup | dB / Decimal(1) |
| Comp: Ratio | ":1" / Decimal(1) |
| Comp: Attack, Release | ms / Significant{3} |
| Comp: ScFreq | Hz / Significant{3}（0 のときは "OFF"） |
| Eq: Freq / Gain / Q | Hz Significant{3} / dB Decimal(1) / "" Decimal(2) |
| BusComp: Threshold, Makeup | dB / Decimal(1) |
| BusComp: Ratio, Attack, Release | "" / **`Choices{labels: p.step_labels().unwrap()}`** |
| ToneEq / MasterLimiter(Ceiling) | dB / Decimal(1) |
| MasterLimiter(On) | "" / Integer |

**daw-ui core の `ScrubableNumberFormat`**（ui/crates/ui/src/widgets/scrubable_number.rs:44-85）に variant を足す。

```rust
/// 段 index ↔ caller が渡したラベル (SignedLabeled scrubable_number.rs:63-84 と同じくドメイン非依存)。
/// format: labels[round(v).clamp(0, len-1)]
/// parse: 前後空白を除き ASCII 大小無視の完全一致 → index。一致しなければ「先頭の数字部分が等しいラベル」→ index。どちらも無ければ None。
Choices { labels: &'static [&'static str] },
```

`format_master_value`（master_strip_ui.rs:413-432）は削除し、表示の SSoT を `automation_value_display` 1 本にする（K17）。

### 7.5 現在値と live 値

**現在値の唯一の口**（新設 `daw_gui/src/handler/param_value.rs`）

```rust
impl AppData {
    /// target の現在値 (plain)。レーン既定値・録音点・A キーが共有する。`_` 無しの網羅 match。
    /// id で束縛する target は **track を引く前に** id で解決する。None = 束縛先が実在しない。
    pub(crate) fn target_plain_value(&self, owner: u32, target: &AutomationTarget) -> Option<f64>;
    /// = target_plain_value(..).unwrap_or_else(|| 種類ごとの常識値)。Text の常識値は automation_lanes.rs:1386-1402 から移す。
    pub(crate) fn lane_default_for_target(&self, touched: &TouchedParam) -> f64;
}
```

| target | 解決 |
|---|---|
| NativeParam | `song.native_by_id(id)?.param(p)` |
| MasterLimiter | `song.master_limiter.param(p)` |
| ChainGain / ChainPan | `song.chain_by_id(cid)?`（device.rs:877） |
| ParallelOutGain / SplitFreq / Select | `song.parallel_by_id(pid)?`（SplitFreq は既定周波数に fallback: automation_lanes.rs:1285-1291） |
| Volume / Pan / Mute / SendGain | `song.track_by_id(owner)?` |
| PluginParam | `plugin_param_values`（tick.rs:542-549） |
| Image / Text | tick.rs:554-614 |
| Group | automation_lanes.rs:1433-1450 |
| Mod 系 | automation_lanes.rs:1323-1331 |
| Tempo / TimeSig | 現行 |

- 削除: `current_plain_value`（tick.rs:514-617。`_ => None` の :615 で Mute / SendGain / Chain* / Parallel* / Strip* / MasterStrip / Mod* / GroupTransform が録音されていなかった）、`track_builtin_plain_value`（automation_lanes.rs:1248-1304。track 不在で 0.0 を返し、master の Chain* / Parallel* で A を押すと無音レーンができていた）。
- `record_automation_points_for_tick`（tick.rs:263）は `target_plain_value` を呼ぶ。

**live 値の唯一の口**（handler/view_model.rs:128-173 を置き換え、K15）

```rust
pub(crate) struct LiveParamScope { running: Vec<crate::launcher_time::RunningRow> }
impl AppData {
    pub(crate) fn live_param_scope(&self) -> LiveParamScope;   // { running: self.launcher_running_rows() }
    /// owner id から store を解く (MASTER_TRACK_ID → song_lanes)。scope を内部で 1 回組む。
    pub(crate) fn live_param_value(&self, owner_id: u32, target: &AutomationTarget, fallback: f32) -> f32;
    /// scope を呼び側が 1 回だけ組む版。
    pub(crate) fn live_param_value_on(&self, scope: &LiveParamScope, owner_id: u32, target: &AutomationTarget, fallback: f32) -> f32;
    /// 本体。store を解決済みで受け取る (トラックを並べる描画で O(N²) にしない)。
    pub(crate) fn live_lane_value(&self, scope: &LiveParamScope, owner: ParamOwner<'_>, target: &AutomationTarget, fallback: f32) -> f32;
    pub(crate) fn live_native_param(&self, scope: &LiveParamScope, owner: ParamOwner<'_>, dev: &NativeDevice, p: NativeParamId) -> f32;
    /// 行ミニ表示 / Par のカーブ用。engine の resolve_native_device の GUI 版 (レーンの値だけ、変調は build_mod が別途)。
    pub(crate) fn live_native_device(&self, scope: &LiveParamScope, owner: ParamOwner<'_>, dev: &NativeDevice) -> NativeDevice;
}
```

- 録音中（active ∪ latched）は fallback を返し、それ以外は enabled レーンの値を返す（view_model.rs:149-172 と同じ規則）。
- `ParamOwner<'a> { id: u32, lanes: &'a [AutomationLane], routings: &'a [ModRouting] }` は `view::native_device` に置き、`ParamOwner::resolve(song, owner_id) -> Option<Self>` で作る（§10.4）。

| 呼び出し元 | owner の出所 |
|---|---|
| view_model.rs:135, :235（`track_mix`） | `live_lane_value(scope, ParamOwner::of_track(t), ..)` |
| view/mixer_strips.rs:1000 | `src_track.id` |
| view/track_inspector/chain_list.rs:635-636 | `song.chain_owner_track(ChainRef::Chain(chain_id))`（master 行も追従するようになる） |
| view/track_inspector/parallel_header.rs:150, :252, :362 | `song.device_owner_track(parallel_id)` |

同じ関数内の `build_mod` とジェスチャーの track_id も、`cursor_track_id()` ではなくこの owner にする（chain_list.rs:630、parallel_header.rs）。

### 7.6 ジェスチャーの所有者（面つき map、K13）

**なぜ面を含めるか**
- `push_param_gesture_edges` の `was` は、共有集合 `active_param_gestures` から引かれる（view/param_gesture.rs:19-43）。
- 同じ `(track, target)` を 2 つの面が描くと、ドラッグしていない面が毎フレーム End を出す。結果として undo が 2 フレームごとに 1 step 積まれ、録音も途切れる。
- `ScrubGesture::ModDepth{track_id,target}`（state/ui_ephemeral.rs:30）も所有者に面が無く、非アクティブな面が close を積んで ◉ を解除してしまう（scrub_gesture.rs:45-60, :94-99）。
- アレンジのヘッダ音量（arrangement_view.rs:236-256）と Mixer フェーダー（mixer_strips.rs:806-813）で、**この壊れ方は既に起きている**。
- Q13 の点ドラッグは Freq と Gain の 2 target を同時に持つ。所有者を単一の Option にすると、2 本目の Begin が 1 本目を閉じる。既存の集合は複数 key を同時に持つ前提で書かれている（handler/param_gesture.rs:59-63: 空になった時点で bracket を閉じる）。

**状態**

```rust
// daw_gui/src/state/recording.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParamSurface { MixerStrip, Rack, MasterPanel, Transport, ArrangementHeader, PluginWindow, VideoPreview }
impl ParamSurface {
    /// 描画の存在で寿命を回収するか。PluginWindow (IPC の End) と VideoPreview (preview 窓の drag end) は false。
    pub fn swept(self) -> bool;
}
pub struct RecordingState {
    /// 触っているパラメーター → その gesture を開いた面。旧 HashSet (:73-74) を置換。
    pub active_param_gestures: HashMap<(u32, AutomationTarget), ParamSurface>,
    /* latched_param_gestures / recording_last_beat / last_sent_recording_lanes は不変 */
}
// daw_gui/src/state/project.rs の ProjectEphemeral (scrub_gesture_seen :707 の隣)
pub param_gesture_seen: HashSet<(u32, AutomationTarget)>,
// daw_gui/src/state/ui_ephemeral.rs:30
ScrubGesture::ModDepth { surface: ParamSurface, track_id: u32, target: AutomationTarget },
```

**イベント**（event.rs:524-537 を置き換え）: `ParamGestureBegin { surface, track_id, target }` / `ParamGestureEnd { surface, track_id, target }`。`display_name` は削除する。

**handler**（handler/param_gesture.rs:19-68）
- `begin_param_gesture(surface, track_id, target)`
  - key に所有者が既にいれば何もしない。
  - いなければ map に insert し、`peph.param_gesture_seen` にも insert する（同じフレームの sweep で閉じないため。scrub_gesture.rs:77 と同じ）。
  - その後は現行の処理（`begin_gesture` / latched / last_touched（名前は `automation_target_label`）/ `sync_recording_lanes_with_audio`）。
- `end_param_gesture(surface, track_id, target)`: `map.get(&key) == Some(&surface)` のときだけ remove し、現行の処理をする。

**view**（view/param_gesture.rs を書き直す）

```rust
/// 1 面・1 param につき毎フレーム 1 回 (dragging=false でも)。dragging はその面でその param を動かす全 widget の OR。
pub(crate) fn push_param_gesture(ui: &mut Ui<'_, AppData>, app: &AppData, surface: ParamSurface,
    track_id: u32, target: AutomationTarget, dragging: bool);
//  (Some(surface), true)  → seen の印 edit
//  (Some(surface), false) → ParamGestureEnd
//  (None, true)           → ParamGestureBegin
//  それ以外 (別の面が所有 / 誰も所有せず非ドラッグ) → 何もしない
/// swept() な面が持ち、seen に無い key を end_param_gesture で閉じ、seen を空にする。
pub(crate) fn sweep_param_gestures(app: &mut AppData);
```

- `sweep_param_gestures` は `view::scrub_gesture::sweep`（scrub_gesture.rs:107-112）の末尾から呼ぶ。runner.rs:1516 には行を足さない。
- `push_mod_depth_bracket(ui, app, surface, track_id, &target, mod_dragging)`（view/modulation.rs:142-159）は、所有者を `ScrubGesture::ModDepth{surface, ..}` にする。
- **アレンジのヘッダ音量**（arrangement_view.rs:229-256 を置き換え）: ドラッグ中だけ `push_param_gesture(ui, app, ParamSurface::ArrangementHeader, t, Volume, true)` を呼ぶ。離したフレームでは push しないので、sweep がそのフレームで End を出す。`peph.arrange_dragging_track_volume`（state/project.rs:527-531, :974）は削除する。
- **plugin 窓**（ipc.rs:354, :388）は `ParamSurface::PluginWindow`、**PiP ドラッグ**（automation_lanes.rs:130-295、:176/:208/:246/:276）は `VideoPreview`。取り出しも自分が所有する key だけにする。
- 子プロセス切断時の clear（automation_lanes.rs:962）は map の clear にする。

### 7.7 last_touched / A キー / 変調 routing の置き場

- `note_touched_mod_target`（handler/modulation.rs:334-353）を `note_touched_target(target, fallback_owner)` に一般化し、param_value.rs へ移す。持ち主は `bound_owner_track(&target).unwrap_or(fallback_owner)`、名前は `automation_target_label`。

**呼び出し元**

| 経路 | target |
|---|---|
| `NativeEdit::Params`（On 以外の最後の param） | `NativeParam{id, p}` |
| `set_devices_bypassed`（devices.rs:681-696）で対象が native 1 台 | `NativeParam{id, On(kind)}`（K20: A キーで On のレーンを作れるようにする） |
| `MasterLimiterEdit::On` / `Ceiling` | `MasterLimiter(On)` / `MasterLimiter(Ceiling)` |
| 変調の深さ | 既存 |

**A キー** `add_automation_from_last_touched`（automation_lanes.rs:1138-1243 → param_value.rs）
- 持ち主 = `bound_owner_track(&touched.target).unwrap_or(touched.track_id)`。
- id で束縛する target で None になったら、「対象が削除されました」として last_touched を消す。
- 既存レーンは `param_stores(owner)` で探し、新規は `push_lane(owner, AutomationLane::new(target, lane_default_for_target))` で作る。
- native の値は `params` にあるので、PluginParam 式の隠しレーン（automation_lanes.rs:764-793）は作らない。

**変調 routing の置き場**
- `add_mod_routing`（handler/modulation.rs:480）/ `remove_mod_routing`（:528）/ `set_mod_routing_depth`（:541）/ `connect_armed_mod_source_to`（:366）の冒頭で `let owner = song.bound_owner_track(&target).unwrap_or(track_id);` とする。
- view から渡る track_id は、同じフレームで device を他トラックへ運んだ後だと古いことがあるため。

### 7.8 束縛の運搬と掃除

| 経路 | 最終形 |
|---|---|
| 他トラックへの移動（device_relocate.rs:523-535 → `move_device_bindings` :558-616） | 運ぶ device 以下の node id を `for_each_node_id` で全部集める（Native / Chain / Parallel を含む）。`bound_node_id()` がその集合に入る lane / routing を抜き、`push_lane(dest, ..)` と `param_stores_mut(dest)` で積む。深さの連鎖（:579-593）は抜いた**全 routing** に掛ける。`outcome.moved_devices` は node id 全部。extract 3 本（:618-674）と push 2 本（:742-772）は削除する |
| ジェスチャーの付け替え（`rekey_param_gestures` :877-907） | 判定を `key.0 == src && key.1.bound_node_id().is_some_and(\|id\| ids.contains(&id))` にし、map の値（面）を保ったまま付け替える。`latched_param_gestures` / `recording_last_beat` も同じ |
| 番号 | トラックを跨ぐ移動は `assign_native_ordinals(dest, .., KeepIfFree)` |
| Par の開閉（Q18） | 移動したサブツリーの node id を `open_rack_panels` から外す（同じトラック内の並べ替えを含む）。コピーは新 id なので閉じている |
| コピー（:473-501）/ 貼付（:272-338） | レーンは複製しない（現行どおり）。値は `params` ごと複製される。`prepare_device_copies` で追加分になる |
| トラックの複製 / 貼付（`remap_pasted_device_refs` tracks.rs:415-461） | :419-444 を `if let Some(id) = target.bound_node_id_mut() && let Some(&n) = remap.get(id) { *id = n }` に置き換える。組み込みは組み込みのまま新しい id になる。aux の chain 参照（:451-460）と track 参照（:495-500）は `for_each_aux_slot_mut` で Native も含める |
| device の削除（devices.rs:936-972） | 同じ closure の後に SongDoc の `enforce_edit_invariants` が prune する。:976-981 のループと `remap_device_refs_after_remove`（:987-1033）は削除する |
| Parallel の解除（parallel.rs:79-98） | 同上（今は掃除していなかった） |
| undo / redo | Song の snapshot なので追加処理は無い。`after_undo_redo`（handler/project.rs:262）で `prune_device_session_refs`（§10.2）を呼ぶ |

### 7.9 MIDI Learn（K 追加設計）

**事実**: Learn の対象は last_touched が PluginParam / Volume / Pan のときだけ。それ以外は `_ => {}` から armed track の Volume に落ちる（handler/midi.rs:137-162）。native のノブを触ってから Learn を押すと、黙って別の対象に bind される。

**設計**
- `midi_learn_binding_target` に `AutomationTarget::NativeParam` / `MasterLimiter` の arm を足す。
- `apply_midi_value_to_target`:
  - 通常の param: `target_range(..).from_norm(v)` → `apply_native_edit(id, NativeEdit::Params[(p, plain)])`（ノブと同じ自動 ON と IPC）。
  - `On(_)`: 64 以上で `set_devices_bypassed([id], false)`、64 未満で true。
  - Limiter: `apply_master_limiter_edit`。
- 掃除は `prune_dangling_param_targets` が `midi_bindings` も対象にする（§5.8）。
- Learn ボタンの表示（view/transport.rs:788-789）に、「Learn Param」と同じ扱いの arm を足す。

---

## 8. engine（daw_audio）

### 8.1 信号モデル

- **直結規則**: 4 種とも「audio_in と audio_out を持つ置換型」で、MIDI には触れない（`NativeKind::ports()`）。
  - RT はバスをその場で置き換える。これは `run_plugin` の「audio_in があれば置換」（program.rs:799-803）と同じ規則。
  - その位置のバスには clip の音しか無く、楽器の出力は後から加算される（:804-807）。したがって「楽器より上の Comp は楽器の音を処理しない」（Q4）は構造的に成り立つ。
- **TapPoint**

| TapPoint | 定義 |
|---|---|
| `PreFx` | device チェーンに入る前（変更なし） |
| `PostFx` | トラックの device チェーン全体を**並び順どおりに通った後**、フェーダーの前。チェーンには組み込みも含む |
| `PostFader` | 変更なし |

  - modulation.rs:28-29 の doc は既に「device chain 適用後・fader 前」なので、「組み込み native も device」と 1 行補うだけでよい。
  - `tap_bufref`（compile.rs:864-871: PostFx → PreFaderScratch）と TrackScratch の doc（mixer.rs:97-103）の文言も合わせる。
- **パラアウトの pass1_end 分割**（program_build.rs:92-99、track.rs:438-446）: Native の `routes_any_aux_output()` は false。移行直後の組み込み（末尾）は分割点より後ろ（pass 2）に入り、今の strip の位置（execute.rs:852-865）と同じ音になる。ユーザーが楽器より上に動かした native は prefix（pass 1）で走る。

### 8.2 型

```rust
// daw_audio/src/graph/program.rs
pub enum ChainOp {
    …既存…,
    /// 組み込み DSP を現在のバスへその場で適用する (audio 置換・MIDI 素通し)。
    /// bypass 中でも op は出す (実効 ON/OFF は block 頭で解決し、切替は crossfade)。
    Native { device_id: u64, native_slot: u32 },
}
pub struct ChainProgram {
    …既存…,
    pub natives: Vec<NativeScratch>,
    /// crossfade 中の dry 退避。op は直列なので 1 組。natives が空なら確保しない。
    pub native_dry_l: Vec<f32>, pub native_dry_r: Vec<f32>,
    /// 旧 track_needs_prefx_snapshot / track_needs_prefader_snapshot を compile 時に焼いた値。
    pub snapshot_pre_fx: bool, pub snapshot_post_fx: bool,
    /// SC Listen: この buffer で検出信号を書いた Comp の slot。トラック出力 (PostFx 点) で消費。
    pub listen_pending: Option<u32>,
}
pub struct ProgramCtx<'a> {
    …既存…,
    pub native: NativeIo<'a>,
    /// その program の owner の device 列と store (program 実行ごとに 1 回解決。op ごとに track を探索しない)。
    pub owner_devices: &'a [Device],
    pub owner_stores: (&'a [AutomationLane], &'a [ModRouting]),
}

// daw_audio/src/graph/native.rs (新設)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScMode {
    /// 自分の入力で検出 (未配線 / SC を受けない種類 / !can_activate / 自 track の PostFx・PostFader = feedback / 行き先が無い)
    #[default] None,
    /// 自 track の Pre-FX (同じ pass の ctx.own_pre_fx、lag 0)。build 時に決まる。
    OwnPreFx,
    /// NodeOp::NativeSidechainTap が staging した ScStage。emit_sidechain_taps が決める。
    Staged,
}
pub struct NativeScratch {
    pub device_id: u64,
    pub builtin: bool,
    pub dsp: crate::native_dsp::NativeDsp,
    pub fade: BypassFade,          // wet 量 0..=1。compile 時は Song の静的 !bypassed で初期化
    pub sc_mode: ScMode,
    pub sc: Option<ScStage>,       // sc_mode == Staged のときだけ Some (MAX_FRAMES × 2ch、compile 時確保)
    pub listen: Option<ListenBuf>, // Comp のときだけ Some (MAX_FRAMES × 2ch、compile 時確保)
    pub gr_db: f32,                // 直前 buffer の GR (≤0)。非 active なら 0
    pub meter: bool,               // GR 面へ publish するか (compile 時に上限まで割り当て)
}
pub struct ScStage { l: Vec<f32>, r: Vec<f32>, frames: u32 }
pub struct ListenBuf { l: Vec<f32>, r: Vec<f32> }
pub struct BypassFade { mix: f32 }   // NATIVE_BYPASS_FADE_MS = 5.0 で線形に動かす
#[derive(Clone, Copy, Default)]
pub struct NativeIo<'a> {
    pub sc_listen: u64,                           // Listen 中の Comp の device_id (0 = 無し)。オフライン描画は常に 0
    pub scopes: Option<DeviceScopeTap<'a>>,       // scope project の live 描画だけ Some
}
#[derive(Clone, Copy)]
pub struct DeviceScopeTap<'a> {
    pub bridge: &'a common::device_scope_bridge::DeviceScopeBridgeHandle,
    pub watch: &'a [u64; common::device_scope_bridge::MAX_DEVICE_SCOPES],
}

// daw_audio/src/native_dsp/mod.rs (新設。mixer/channel_strip.rs と mixer/master_strip.rs を置換)
pub enum NativeDsp { Comp(CompState), Eq(EqState), BusComp(BusCompState), ToneEq(ToneEqState) }
pub struct NativeBlock<'a> {
    pub l: &'a mut [f32], pub r: &'a mut [f32], pub n: usize, pub sample_rate: f32,
    pub sidechain: Option<(&'a [f32], &'a [f32])>,               // 長さが n に足りない分は 0 扱い
    pub listen_out: Option<(&'a mut [f32], &'a mut [f32])>,      // Comp 以外では常に None
}
impl NativeDsp {
    pub fn new(kind: NativeKind) -> Self;
    pub fn kind(&self) -> NativeKind;
    pub fn reset(&mut self);                                     // biquad 遅延・平滑値・係数キャッシュを無音状態に戻す
    /// 戻り値は GR (dB, ≤0)。EQ 系は 0。params の種類が dsp と違えば素通しして 0。
    /// listen_out が Some なら、SC フィルタ後の検出信号をそこへ書く (バスは通常どおり処理する)。
    pub fn process(&mut self, params: &NativeParams, b: NativeBlock<'_>) -> f32;
    pub fn adopt_state_from(&mut self, old: &NativeDsp) -> bool; // 同じ variant のときだけ
}
// native_dsp/limiter.rs
pub struct MasterLimiterState { /* 旧 master_strip.rs:42-52 の limiter 部分 */ }
impl MasterLimiterState {
    pub fn new() -> Self;                 // ルックアヘッドリングは 192kHz 分を 1 回だけ確保 (旧 :24-26)
    pub fn reset(&mut self);              // リング fill(0) + 利得 / GR / cached_look_sr を 0 (確保なし)
    pub fn process(&mut self, s: &MasterLimiterSettings, latency_active: bool,
                   l: &mut [f32], r: &mut [f32], n: usize, sample_rate: f32);
    pub fn gain_reduction_db(&self) -> f32;
}
```

- **骨格からの逸脱**: `ChainOp::Native { device_id, native_slot }` にした（骨格は `state`）。
  - `ChainOp` は `Clone + PartialEq` の値型で状態を持てない（program.rs:43-77）。既存 op も scratch の番号（`parallel_slot` / `voice_slot`）を持つ形になっている。
  - SC を Staged にするかは tap を解決できたかで決まる。解決できる場所は `emit_sidechain_taps` だけで、build の時点では `ChainMap` も `id_to_idx` も無い（compile.rs:687-724, :177-182）。

**DSP 本体の移し先**

| 移し先 | 移す元 |
|---|---|
| `native_dsp/comp.rs` | `mixer/channel_strip.rs:20-66, 110-153` |
| `native_dsp/eq.rs` | 同 `:24-30, 56-60, 99-106` |
| `native_dsp/bus_comp.rs` | `mixer/master_strip.rs:35-40, 125-166` |
| `native_dsp/tone_eq.rs` | 同 `:31-34, 108-122` |
| `native_dsp/limiter.rs` | 同 `:42-52, 168-247` |

### 8.3 compile（off-RT）

#### 8.3.1 program の組み立て（`program_build.rs`）

- `program_latency`（:45-68）に `Device::Native(_) => acc` を足す（native の遅延は 0）。
- `Builder::emit_device`（:127-152）に `Device::Native(nd)` の arm を足す。
  1. `nd.id == 0`（未採番）なら op を出さない。
  2. `native_slot = natives.len()` とし、`NativeScratch::new(nd)` を push する。
     - `sc_mode` は、`nd.aux_input` の tap が `Track(自 track_id)` かつ `PreFx` なら `OwnPreFx`、それ以外は `None`。この判定は `can_activate` を見ない（snapshot は bypass と無関係に取られ、staging も依存辺も要らない）。
     - `listen` は kind が Comp のときだけ確保する。
  3. `ChainOp::Native { device_id: nd.id, native_slot }` を push する。
  4. `native_slots.insert(nd.id, slot)` を記録する。
  5. latency 0 を返す。
- natives が 1 つでもあれば、`finish` で `native_dry_l/r` を `MAX_FRAMES`（process_data.rs:14 の 1024）分確保する。
- `BuiltProgram`（:24-33）に `native_slots: HashMap<u64, u32>` を足す。

#### 8.3.2 `build_all_programs`（compile.rs:687-724）

`ChainMap` を登録した後に次の 2 つを行う。ここに置くので、`n == 0` の早期 return（compile.rs:164-174）にも効く。

1. **`assign_native_meters(&mut built, &mut master_built)`**
   - `kind.has_gain_reduction()` を満たす native に `meter = true` を付ける。
   - 割り当て順は、組み込み（track 順 → master）→ 追加分（同じ順）。`MAX_NATIVE_METERS` を超えた分は false。
   - pass 1 が処理するのは `min(tracks, MAX_TRACKS=32)`（audio_bridge.rs:21、execute.rs:996）で、組み込み Comp は最大 33 個。したがって Mixer 帯とマスターパネルの GR は必ず出る。
2. **`bake_snapshot_needs(song, &mut built)`**
   - `snapshot_pre_fx = any_tap_at(song, id, PreFx)`
   - `snapshot_post_fx = any_tap_at(song, id, PostFx) || sends に PreFader がある`
   - `any_tap_at` は RT（mix.rs:73-88）から compile へ移し、`all_aux_consumers()` で走査する。

#### 8.3.3 SC 会計（`graph/compile/sidechain.rs`）

**consumer の列挙**
- common の `aux_consumers(devices, lanes, routings)`（§5.9）が、plugin と native を同じ `AuxConsumer` として信号順に出す。`inactive` なもの（plugin は bypassed、native は `!can_activate`）は数えない。
- 次の 5 つの走査をこれに置き換える:
  - `collect_chain_taps`（旧 :97-115、`!p.bypassed` → `!c.inactive`）
  - 依存辺（旧 :305-337 → `TrackDeps`）
  - `compute_input_delays`（旧 :737-767）
  - `compute_path_latency` の SC ループ（旧 :1061-1065）
  - `emit_sidechain_taps`（旧 `emit_aux_input_taps` :873-915）

**consumer が走る pass で lag を決める**（既存の穴 §18-L を塞ぐ）

```rust
enum ScPass { Pass1, Pass2 }
fn consumer_pass(bus: bool, split: Option<u32>, top_index: u32) -> ScPass {
    // leaf は全部 pass 1。group-with-instrument は prefix (top_index < split) が pass 1。
    // それ以外の bus (group / return / パラアウト先 / GWI の suffix) は pass 2。
    if !bus || split.is_some_and(|s| top_index < s) { ScPass::Pass1 } else { ScPass::Pass2 }
}
// lag(consumer) = Pass1 → buffer_frames (post-dispatch staging を次 buffer で消費)、Pass2 → 0
```

| 値 | 式 | 適用先 |
|---|---|---|
| `input_delay_per_track[i]`（pass 1 用） | pass 1 consumer の `src_latency + buffer_frames` の最大。自トラックの source は除く（現行の規則） | 既存の execute.rs:308-314（変更なし） |
| `bus_sc_delay[i]`（pass 2 用） | `max(0, pass2 consumer の src_latency の最大 − non_sc_input(i))`。`non_sc_input(i) = max(group_input, send_input)` | 下の `ApplyDelay` |
| `compute_path_latency` の `sidechain_input` | 上と同じ consumer ごとの lag で計算する（式の形は現行のまま） | — |

2 つの遅延を掛けた後の実際の入力 latency は `max(non_sc, sc)` に一致するので、申告値と実際の値が揃う。

**tap の emit と pass 2 の遅延補償**
- `emit_sidechain_taps(chain, owner_track_id, program: &mut BuiltProgram, owner_idx, lanes, routings, id_to_idx, chains, nodes)`
  - **Plugin**: 既存の `NodeOp::SidechainTap { src, device_id, aux_in_port }`。
  - **Native**: 次の 4 条件がすべて満たされるときだけ `NodeOp::NativeSidechainTap { src, owner: owner_idx, native_slot }` を push し、同じ場所で `natives[slot].sc_mode = Staged; sc = Some(ScStage::new())`（off-RT で確保）を設定する。
    1. `accepts_sidechain`
    2. source が自トラックではない
    3. `tap_bufref_for` で解決できる
    4. `program.native_slots` に id がある（bypass 中の Parallel の中の native には op が無い: program_build.rs:146）
  - tap を出す位置は plugin と同じ。track は ProcessTrack / ProcessGroupFx の前（compile.rs:387）、master は master Mix の後（:490）。
- PDC で ops を組み直すループ（compile.rs:584-637）で `NodeOp::ProcessGroupFx { track_idx, .. }` に当たり、`bus_sc_delay[track_idx] > 0` なら、その直前に `ApplyDelay { buf: TrackScratch(track_idx), line_idx, frames }` を積む。
  - `delay_keys` には `DelayKey::BusScAlign { track_id }`（schedule.rs:165-172 に新設）を積む。
  - 実行は既存の `ApplyDelay` の arm（execute.rs:620-644）。`run_group_fx_chain` の pre-FX snapshot はその後に取られるので、`OwnPreFx` も揃う。

#### 8.3.4 Schedule（schedule.rs:179-247）

- `master_limiter_latency: bool = song.master_limiter_latency_active()` を足す。
- 設定するのは Schedule を作る 2 か所（compile.rs:165-173 の早期 return と :663-682）。
- `master_output_latency`（compile.rs:832-846）の limiter 項は、`song.master_strip.limiter.on`（:838）ではなくこの値だけを見る。

### 8.4 RT 実行

#### 8.4.1 `run_chain_program` の arm（program.rs:306-446）

```rust
ChainOp::Native { native_slot, .. } => {
    let Some(ns) = natives.get_mut(*native_slot as usize) else { continue };
    super::native::run_native(ns, *native_slot, (native_dry_l, native_dry_r), listen_pending, track_id, bus_l, bus_r, n, ctx);
}
```

**`run_native` の手順**（確保・ロック・I/O なし）
1. `native_in(ctx.owner_devices, device_id)` で device を引く。無い、または kind が `dsp.kind()` と違えば `gr_db = 0` にして素通しする（防御用の分岐。構造の変更は song と schedule が同じ bundle で届く: engine.rs:149-196）。
2. 値を解決する: `let v = resolve_native_device_in(ctx.owner_stores, dev, track_id, ctx.rows, ctx.playhead_beats, ctx.recording_lanes, ctx.mod_plane)`。
3. `let active = !v.bypassed; let (from, to) = fade.advance(active, n, sr)`
   - `from == 0 && to == 0`: `gr_db = 0`、scope に書いて return（bypass 中の実質コストはここまで）。
   - `from == 0`: `dsp.reset()`（今は再 ON 時に古いフィルタ状態のまま再開している。§18-E）。
   - `from != 1 || to != 1`: バスを `native_dry_*` へ退避する。
4. SC を解決する: `None` → None、`OwnPreFx` → `ctx.own_pre_fx`（None なら自分の入力。GWI の pass 1: execute.rs:322）、`Staged` → `sc.l/r[..frames]`。
5. Listen: `let listen = dsp.kind() == Comp && ctx.native.sc_listen == device_id && active;` のとき、`listen_out` に `ns.listen` のバッファを渡し、`*listen_pending = Some(native_slot)` にする。
6. `gr = dsp.process(&v.params, NativeBlock { .. })`
7. フェード中なら `bus = dry + (wet − dry)·w(i)`（w は from → to の線形補間）。
8. `gr_db = if active { gr } else { 0 }`
9. scope: `ctx.native.scopes` の `watch[k] == device_id` があれば、`bridge.write_block(k, bus_l, bus_r)` で最終出力を書く。bypass 中の EQ も素通しの音を書く。

**Listen の置換**（K25d。Song の外にある「聴き方」の状態）
- 置換はトラックの**チェーン出力（PostFx 点）**で行う。
- 実行位置は `apply_listen_override(program, bus_l, bus_r, n)`（`listen_pending.take()` なら `natives[slot].listen` を bus にコピー）で、次の 3 か所で呼ぶ。
  - `process_track_owned` の pre-fader snapshot の前
  - `run_group_fx_chain` の同じ位置
  - master は `process_master_fx_chain` の後、master_gain の前
- `listen_pending` はトラックの program 実行開始時（GWI では pass 1 の開始時）に None にし、PostFx 点で消費する。
- 旧 Listen は「後段を通さずに素で聴く」ために EQ を飛ばしていた（daw_audio/src/mixer/channel_strip.rs:89-93）。旧チェーンは inserts → Comp → EQ なので、実際には検出信号がそのまま Pan / Fader に届いていた。並べ替えのできるチェーンで同じ意図を保つにはこの形になる。後段の device 自体は普通に走り、状態も保たれる。

#### 8.4.2 SC の staging（`execute_schedule_post_dispatch` execute.rs:544-773 に arm を追加）

```rust
NodeOp::NativeSidechainTap { src, owner, native_slot } =>
    super::native::stage_native_sidechain(scratch, track_programs, master_program, *src, *owner, *native_slot, n),
```

- `mix.rs:23-68` の `resolve_tap_buffers` を 2 つに分け、plugin の `SidechainTap` と共用する: `resolve_scratch_tap(scratch, src)`（TrackScratch / PreFader / PreFx）と `resolve_program_tap(chains, parallels, src)`（Chain* / Parallel*）。
- 借用は 3 通り。
  1. 読み元が scratch → 書き先の program だけを `&mut` で取る。
  2. 読み元と書き先が同じ program → `let ChainProgram { chains, parallels, natives, .. } = p` とフィールドで分解する。
  3. 別の program → `track_programs.get_disjoint_mut([o, owner])`（master は `Schedule` の別フィールド）。
- `ScStage::stage(l, r, n)` は `frames = n` を記録する。消費側は `min(n, frames)` まで読み、残りは 0 とみなす。

#### 8.4.3 group / return の pass 2（`run_group_fx_chain` execute.rs:782-907）

- 引数に `native_io: NativeIo<'_>` を足し、`ProgramCtx.native` に載せる。
- `apply_channel_strip` の呼び出し（:852-865）を削除する。device チェーン（:842-850）は既に `ctx.mod_plane` を持っている（:837）ので、組み込みにも変調が効くようになる（§18-A）。
- `fill_track_param_ramps` には `ModTickPlaneRef::default()`（:903-904）ではなく `mod_plane` を渡す（group の volume / pan の変調）。
- pre-FX の条件（:817）を `scratch.force_prefx_snapshot || program.snapshot_pre_fx` にする。
- pre-fader の条件（:869-877）を `scratch.force_prefader_snapshot || program.snapshot_post_fx` にする（§18-C）。
- PostFx 点で `apply_listen_override`。

#### 8.4.4 master（`render_master_buffer` execute.rs:962-1160）

- `resolve_master_strip` + `process_pre`（:1092-1114）を削除する。組み込みの Bus Comp / Tone EQ は `master_fx_chain` の device として `process_master_fx_chain`（:1119-1137、引数に `native_io`）の中で走る。
- その後に `apply_listen_override`。
- master_gain（:1139-1147）はそのまま。
- Limiter（:1149-1159 を置き換え）:

```rust
let lim = crate::automation::resolve_master_limiter(song, rows.master_rows(), playhead_beats, recording_lanes, mod_plane);
master_limiter.process(&lim, schedule.master_limiter_latency, &mut master_l[..n], &mut master_r[..n], n, sr_f32);
```

- `MasterLimiterState::process` の規則:
  - `latency_active == false`: 遅延もゲインも掛けず素通し。状態は捨てる（`cached_look_sr = 0`）。
  - `true` かつ `!s.on`: 遅延だけ通し、ゲインは掛けない（PDC の会計と常に一致する）。
  - `true` かつ `s.on`: 先読みリミッター。
- 食い違ったコメント（execute.rs:1151-1152、mixer/master_strip.rs:170-171）は上の条件付きの記述に置き換える。
- 引数の `master_strip`（:985-988）を `master_limiter: &mut MasterLimiterState` と `native_io: NativeIo<'_>` に置き換える。
- doc（:949-960）と module doc（:4-10）の手順を「… → master fx chain（組み込みを含む）→ master gain → master limiter」に書き直す。

#### 8.4.5 leaf（`process_track_owned` execute.rs:135-437）

- 引数に `native_io` を足す。
- `apply_channel_strip`（:377-392）を削除する。
- snapshot の条件を焼いた値に置き換え、RT で Song を歩かないようにする（§18-B）。pre-FX（:322-324）は `!skip_strip && (force || program.snapshot_pre_fx)`、pre-fader（:400-407）は `force || program.snapshot_post_fx`。
- PostFx 点で `apply_listen_override`。

#### 8.4.6 worker への受け渡し（audio_worker.rs）

- `dispatch_and_wait`（:268-288）に `native_io: NativeIo<'_>` を足す。
- `DispatchShared`（:90-136）に `sc_listen: AtomicU64`、`scope_bridge_ptr: AtomicPtr<DeviceScopeBridgeHandle>`、`scope_watch_ptr: AtomicPtr<[u64; MAX_DEVICE_SCOPES]>`（null = 無し）を足す。
- `run_work_loop`（:537-620）で `NativeIo` に組み直し、`process_track_owned`（:695）に渡す。形は既存の `recording_lanes_ptr`（:119, :590-600）と同じ。
- scope リングの slot ごとの書き手は、その device の op 1 つだけ（device id は一意）なので、worker 間で競合しない。

### 8.5 状態の引き継ぎとプロジェクトの切替

**`ChainProgram::adopt_state_from`**（program.rs:223-255）に次を足す。

```rust
for ns in &mut self.natives {
    if let Some(o) = old.natives.iter_mut().find(|o| o.device_id == ns.device_id) {
        if ns.dsp.adopt_state_from(&o.dsp) {           // 同じ種類のときだけ
            ns.fade = o.fade; ns.gr_db = o.gr_db;
            if let (Some(a), Some(b)) = (ns.sc.as_mut(), o.sc.as_mut()) { std::mem::swap(a, b); } // leaf の 1 buffer 遅れを失わない
        }
    }
}
```

- RT 上（engine.rs:514）で行うのは、固定長 struct のコピーと Vec の swap だけ。旧 Vec は旧 schedule と一緒に recycle されて off-thread で解放される。
- つまみのドラッグ中は編集のたびに LoadSong → Recompile になる（project_ctl.rs:864-872）ので、この引き継ぎは必須。
- program 間の対応付けは track_id で行う（schedule.rs:297-302）。追加分を他トラックへ移すと状態は引き継がれない（処理する信号自体が変わるので正しい）。
- `TrackScratch.strip` を撤去するので、トラックの並べ替えで DSP 状態が位置 index に残る穴（engine.rs:469-477）も消える。
- **同じタブで別ファイルを開いたとき**: `refresh_bundle` の `reset_song_scoped_state` の分岐（engine.rs:482-511）で `self.master_limiter.reset()` を呼ぶ（§18-M。192kHz 換算 961 × 2 の fill だけで、確保なし）。

### 8.6 値だけを送る IPC と LoadSong

- `SetTrackStrip` / `SetMasterStrip`（protocol.rs:416-426、`project()` の arm :648-649）を削除する。
- `SetNativeDevice { project, device_id, bypassed, params }` / `SetMasterLimiter { project, limiter }` を新設する。
- `song_values::apply`（song_values.rs:35-44）は `song.native_by_id_mut(device_id)` → `replace_values(bypassed, params)`（種類が違えば何もしない）。`sanitize_strip` / `sanitize_master_strip`（:100-146）は削除する。
- `project_ctl.rs:674-697` の列挙を差し替える。
- **2 通の順序**: GUI の編集は `edit_song_checked` を通るので、値の IPC（即時）とフレーム末の LoadSong（`Topology::Recompile`）の 2 通が届く。値の IPC は最後に publish された Song の複製に書かれ（project_ctl.rs:395-418）、両方とも IPC スレッド上で直列に publish され、RT は bundle を畳み込む（engine.rs:430-436）。どちらの順に着いても、最後は LoadSong の Song と一致する。
- **track を持たない理由**: device id は song 全体で一意（device.rs:5-6。重複は ensure_ids が再採番: model.rs:1854-1874）。前例の `SetChainGain` も track で引き当てていない（song_values.rs:57-61）。

### 8.7 SC Listen（engine 側）

- `AudioCommand::SetScListen { project, device_id: Option<u64> }` を受けたら、`ProjectShared.sc_listen_device: AtomicU64`（engine_shared.rs:279- に追加、0 = 無し）に store する。IPC スレッドが書き、RT が読む。
- **engine は `SetScListen` と `CloseProject` 以外で値を変えない。** LoadSong や project_id の切替でも解除しない。
  - respawn 後の新しい engine が最初に受ける LoadSong は、必ず `project_switched` になる（project_ctl.rs:809-813）。そこで解除すると、`restore_tabs_after_respawn`（daw_gui/src/handler/tabs.rs:279-308）の再送が無効になる。
- Song に無い id もそのまま保持する。compile 後に一致した op があれば効く。
- `ProjectRt::render_buffer` の頭で 1 回 load し、`NativeIo.sc_listen` に載せる（watch の load と同じ場所: engine.rs:897-899）。
- 書き出し / ラウドネス解析 / bounce は常に `NativeIo::default()` なので、Listen の音が WAV に焼き込まれることは構造的に無い。今は `sc_listen` が bincode で Song に載り、書き出しにも届いている（channel_strip.rs:420-425。§18-F）。

### 8.8 CPU 方針

1. bypass 中でフェードが落ち着いた native は、解決の直後に抜ける（§8.4.1 手順 3）。
2. Comp / Bus Comp のサンプルごとの処理（旧 mixer/channel_strip.rs:135, 148、master_strip.rs:152, 161）。block の頭で `knee_floor_amp = db_to_amp(thr − COMP_KNEE_DB/2)`、`makeup_amp`、`attack_c` / `release_c` を求めておく。

   ```
   det = max(|dl|,|dr|); if !det.is_finite() { det = 0 }                            // NaN 封じ込め (§18-H)
   target = if det <= knee_floor_amp { 0 } else { comp_static_gain_db(amp_to_db(det), thr, ratio) }
   gain_db = target + (gain_db − target)·coeff; if gain_db > −1e-6 { gain_db = 0 }  // denormal を断つ
   g = if gain_db == 0 { makeup_amp } else { db_to_amp(gain_db + makeup) }
   ```

   `comp_static_gain_db` は `over <= −half_knee` で厳密に 0 を返す（channel_strip_dsp.rs:298-299）。threshold の下限 −60dB は `amp_to_db` の下限 −120（:319-321）より上なので、この近道でも値は変わらない。
3. EQ / Tone EQ: 係数を組み直すときに「IDENTITY でない段」のビットマスクを作り、恒等の段は回さない。
4. Limiter: `10f32.powf`（master_strip.rs:213）を `db_to_amp` にする。
5. automation: レーンも routing も空の store は解決を省く。

(2)〜(4) で出る丸めの差は、§15 の golden の許容誤差に収まる。

### 8.9 書き出し / ラウドネス解析 / 決定性

- 書き出しとラウドネス解析は live と同じ `render_master_buffer` を通る（不変条件 6）。変更は 2 か所だけ。
  - `master_strip` を `MasterLimiterState::new()` にする（export.rs:494-496）。
  - 呼び出し（:762-790）に `master_limiter` と `NativeIo::default()` を渡す。
- 自前の schedule を compile するので、`NativeScratch` は新品で `adopt_state_from` を通らない。「書き出しクリーンスタート」（plugin の deactivate / activate）と組み合わせても決定的。
- GUI 側の Bounce In Place / Glue の song 加工は §10.15。

### 8.10 protocol まとめ（`common/src/protocol.rs`）

```rust
AudioCommand::SetNativeDevice  { project: ProjectKey, device_id: u64, bypassed: bool, params: NativeParams } // 約 100 byte、Copy
AudioCommand::SetMasterLimiter { project: ProjectKey, limiter: MasterLimiterSettings }
AudioCommand::SetScListen      { project: ProjectKey, device_id: Option<u64> }   // Comp 以外は engine が無視
AudioCommand::SetDeviceScopes  { project: ProjectKey, device_ids: Vec<u64> }     // ≤ MAX_DEVICE_SCOPES。engine も切り詰め・重複除去
AudioSession { …, device_scope_shmem_id: String }                                // protocol.rs:146-161
// 削除: SetTrackStrip / SetMasterStrip
```

- 新しい command はすべて project 宛て（`project()` が Some）。
- 数個の u64 と数十〜百 byte の Copy 型だけで、bulk ではない（不変条件 2）。サンプル列は shmem で運ぶ（§11.2）。

---

## 9. GUI（daw_gui）

### 9.1 イベント

**`AppEvent::Device(DeviceEvent)` に集約する**（K22。新設 `daw_gui/src/event_device.rs`、前例 event_launcher.rs / event_tabs.rs）

- `AppData::handle_event` は 1603 ncloc で、天井が 1605（scripts/arch_lint_baseline.txt:156）。arm を足す前に分割する。
- **S1c（挙動は変えない移設）**: event.rs:832, 840, 846, 850, 853, 855, 863, 869, 878, 886, 895, 903, 977, 984-1003 の 26 variant を、名前を変えずに `DeviceEvent` へ移す。`CloseAllPluginEditors`（:837）/ `SelectPluginFromDb` / `OpenPluginPicker` は `AppEvent` に残す。
- **S1c**: `AddParallel { chain, index }` を `AddParallel { chain, at: InsertAt }` にし、`RelocateDevices.dest_index: u32`（device_addr.rs:65-76）を `InsertAt` にする。この段階では `InsertAt::Index` だけを使う。

```rust
// daw_gui/src/device_addr.rs
/// 新規デバイ

スの挿入位置。`Default` は **実行時の Song** から Q6 規則で解決する (deferred 実行でも古くならない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertAt { Index(u32), Default }
```

- `Default` を使う理由: plugin がある曲では `RelocateDevices` が `DeferredEdit` として後で実行される（device_relocate.rs:33-39）。ドロップ時点で数えた index は古くなりうるので、`relocate_in_song` / `add_parallel` の closure の中で `song.default_insert_index(dest)` を呼んで解決する。

**F が足す variant**

```rust
pub enum DeviceEvent {
    …移設 26 variant…,
    /// picker の内蔵 4 種。chain の Q6 位置へ追加分として挿す。open_panel = Shift なしなら true。
    AddNative { chain: ChainRef, kind: NativeKind, open_panel: bool },
    /// 組み込み・追加分共通の値編集。Song 編集 + 値 IPC + 自動 ON。
    NativeEdit { device_id: u64, edit: NativeEdit },
    /// master の固定 Limiter (チェーン外)。
    MasterLimiterEdit(MasterLimiterEdit),
    /// 聴き方の都合。Song に書かない (bypass 中の有効化だけは Song 編集、§10.14)。
    SetScListen { device_id: Option<u64> },
    /// Par の開閉。見方の都合 (Song に書かない)。
    ToggleRackPanel(RackPanelKey),
}
```

**新設 `daw_gui/src/event_native.rs`**（wire を渡らない。`lib.rs:25-30` に `pub mod event_native;`）

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum NativeEdit {
    /// 連続 / 段階 (plain 単位)。カーブ点のドラッグは Freq+Gain を 1 イベントで運ぶ。
    Params(Vec<(NativeParamId, f32)>),
    EqBandOn { band: EqBand, on: bool },   // どのバンドでも受け付ける (UI が出すのは HP/LP だけ)
    EqBell { band: EqBand, bell: bool },   // LF / HF のみ有効
    CompMode(CompMode),
}
impl NativeEdit {
    pub fn param(p: NativeParamId, v: f32) -> Self;
    /// 自動 ON の唯一の SSoT。戻り値 = 実際に変わったか (値 IPC と dirty の根拠)。
    pub fn apply(&self, dev: &mut NativeDevice) -> bool;
    pub fn undo_label(&self) -> &'static str;   // 種類の NativeKind::undo_label()
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MasterLimiterEdit { On(bool), Ceiling(f32) }
impl MasterLimiterEdit { pub fn apply(self, l: &mut MasterLimiterSettings) -> bool; }
```

**undo ラベル**
- `DeviceEvent::undo_label()` を用意し、`AppEvent::undo_label`（event.rs:1851）側は `E::Device(ev) => ev.undo_label()` の 1 arm にする。

| イベント | ラベル |
|---|---|
| `NativeEdit` | `edit.undo_label()` |
| `MasterLimiterEdit` | 「マスターリミッター変更」 |
| `AddNative` | 「Comp 追加」「EQ 追加」「Bus Comp 追加」「Tone EQ 追加」 |
| `SetDevicesBypassed` | 「デバイスを無効化」/「デバイスを有効化」（event.rs:2008-2009 の「プラグインを…」を置換） |
| `SetScListen` | 「デバイスを有効化」（snapshot が積まれたときだけ記録される） |
| `ToggleRackPanel` | Song を変えないので snapshot を積まない |

**削除**
- `AppEvent::{StripEdit, MasterStripEdit}`（event.rs:1121-1129）
- それらの undo ラベル（:1961-1969）
- 型 `StripEdit` / `StripSwitch` / `MasterSection`（:2102-2181）
- app.rs:1361-1366 の arm

**残す**: `StripSection` と `AppEvent::ToggleStripSection`（Mixer 帯を全 ch 一括で開閉する。doc の `UiPrefs` を `ProjectView` に直す）。

---

## 10. GUI 本体

### 10.1 自動 ON の規則（`NativeEdit::apply`、K19 / K21）

1. `Params` の各 `(p, v)` について:
   - `p` が `On(_)` なら **無視**する。bypass を書くのは `SetDevicesBypassed` だけ（前例: devices.rs:677-696）。
   - `dev.param(p)` が None（種類違い / 実在しない住所）なら無視する。
   - それ以外は `changed |= dev.set_param(p, v)` し、`touched = true` にする。
   - `p.eq_band()` が `Some(band)` なら `band.on = true` にする（Q13 の「OFF のバンドを触ると ON」を、つまみにも同じ規則で当てる）。
2. `EqBandOn{band, on}`: EQ なら書き込み、`touched`。`on == false` のときはバンドを ON にしない（明示的な OFF 操作だから）。
3. `EqBell{band, bell}`: EQ かつ LF / HF のときだけ書き込み、`touched`、`band.on = true`。
4. `CompMode(m)`: Comp のときだけ書き込み、`touched`。
5. `touched` なら `changed |= replace(&mut dev.bypassed, false)`（Q15: Q で OFF にした直後でも、触れば ON）。

`MasterLimiterEdit::apply`:
- `Ceiling(v)`: clamp して書き込み、`on = true`。
- `On(b)`: 明示的な操作なので自動 ON はしない。

plugin の `SetPluginParam` は bypass に触らない（今のまま）。

### 10.2 handler（新設・変更の関数）

| ファイル | 関数 | 責務 |
|---|---|---|
| `handler/device_event.rs`（S1c 新設） | `AppData::handle_device_event(ev)` | `DeviceEvent` の dispatch。app.rs:1202-1206, 1211-1251, 1301-1321 を移す（`CloseAllPluginEditors` :1208-1210 は残す） |
| `handler/native_edit.rs`（F） | `apply_native_edit(device_id, &NativeEdit)` | ① `owner = song.device_owner_track(id)?` ② `edit_song_checked(\|s\| s.native_by_id_mut(id).is_some_and(\|d\| edit.apply(d)))` ③ 変わったら `send_native_value(id)` ④ `Params` なら `note_touched_target(NativeParam{id, On 以外の最後の p}, owner)` |
| 同 | `apply_master_limiter_edit(MasterLimiterEdit)` | `edit_song_checked` → 変わったら `SetMasterLimiter` を送る（On / Ceiling どちらでも。遅延の変化は同じフレームの LoadSong が焼く）→ `note_touched_target(MasterLimiter(p), MASTER)` |
| 同 | `send_native_value(device_id)` | `SetNativeDevice` を送る唯一の口 |
| 同 | `add_native(chain, kind, open_panel)` | `ensure_first_track()` → `edit_song_checked` の closure で `owner = chain_owner_track(chain)?`、`id = alloc_device_id()`、`ordinal = next_native_ordinal(owner, kind)`、`at = default_insert_index(chain)?`、`insert_device(chain, at, Native(new_added(kind, id, ordinal)))`、id を返す → `open_panel` なら `open_rack_panels.insert(Device(id))` → `set_device_selection(vec![id])`、`device_anchor = Some(id)` |
| `handler/sc_listen.rs`（F） | `request_sc_listen(Option<u64>)` | `Some(id)`: Comp でなければ return → bypass 中なら `set_devices_bypassed(&[id], false)` → `set_sc_listen(Some(id))`。`None`: `set_sc_listen(None)` |
| 同 | `set_sc_listen(Option<u64>)` | `peph.sc_listen_device` への書き込みと `AudioCommand::SetScListen` の送信を行う唯一の口（自動 ON はしない） |
| 同 | `prune_sc_listen()` | Song に居なくなった id なら `set_sc_listen(None)` |
| `handler/bypass_target.rs`（F） | `hovered_bypass_target(mixer_active) -> Option<BypassTarget>` | `master_panel_hovered` を先に見て、次に `mixer_active` なら `mixer_hovered_native.map(Device)` |
| 同 | `bypass_toggle_event(BypassTarget) -> AppEvent` | `Device(id)` → `SetDevicesBypassed{[id], !all_devices_bypassed(&[id])}`、`MasterLimiter` → `MasterLimiterEdit(On(!song.master_limiter.on))` |
| `handler/param_value.rs`（S1c 移設、F / A 変更） | `target_plain_value` / `lane_default_for_target` / `add_automation_from_last_touched` / `note_touched_target` | §7.5 / §7.7。automation_lanes.rs:1138-1452 をここへ移す |
| `handler/device_guard.rs`（R） | `DeviceOp { Remove, Cut, Group, Relocate{dest, copy} }`、`permitted_ids(song, ids, op) -> Vec<u64>`、`any_permitted(song, ids, op) -> bool`（確保なし）、`rejected_builtin_count` | Remove / Cut / Group は `is_builtin_native` を落とす。Relocate は `can_relocate` で絞る |
| `handler/rack_view.rs`（R） | `toggle_rack_panel(key)`、`rack_panel_open(key)`、`close_rack_panels_of(ids)`、`wanted_device_scopes() -> Vec<u64>`、`sync_device_scopes()` | §10.5 / §11.2 |
| `handler/device_relocate.rs:379-406` | `prune_device_selection` → **`prune_device_session_refs`** | 末尾で `prune_sc_listen()` を呼ぶ。呼び出し元: devices.rs:984、parallel.rs:95、tracks.rs:960、project.rs:321（undo/redo） |
| `handler/devices.rs:681-696` | `set_devices_bypassed` | native 1 台なら `note_touched_target(On(kind))` を足す。値 IPC は**足さない**（bypass は LoadSong で届く） |
| `handler/devices.rs:473-532` | `toggle_slot_gui` | 先頭で native なら `toggle_rack_panel(Device(id))`。映像 FX / GUI 無し plugin の分岐も `toggle_rack_panel` にし、相互排他（:487, :509）を削除 |
| `handler/devices.rs:718-750` | `set_sidechain_source` | `Song::set_aux_input` を呼ぶ（Native port 0 を含む。循環なら拒否） |
| `handler/modulation.rs:710-730` | `set_aux_input_tap_point` | `Device::aux_input_slot_mut` |
| `handler/parallel.rs:533-548` | `sidechain_ports` | `aux_input_port_count` |
| `handler/parallel.rs:550-580` | `sidechain_source_choices(device_id)` | Structural の `would_cycle` を除外（§5.9） |
| `handler/mixer.rs:583-605` | send 宛先の候補と `add_send` | `can_add_send` |
| `handler/mixer.rs:376-520` | `apply_strip_edit` / `apply_master_strip_edit` | **削除** |
| `handler/project.rs:38-126` | `reset_song_scoped_state` | `set_sc_listen(None)`、`rack_panel_heights.clear()` |
| `handler/tabs.rs:287-300` | `restore_tabs_after_respawn` | Audio の分岐の `with_project` 内で `app.set_sc_listen(app.cur.peph.sc_listen_device)`、`app.cur.peph.device_scopes_sent.clear()` |

### 10.3 表示状態

**F が全単位分のフィールドを定義する**（後続の単位は state を編集しない）

| 変更 | 置き場 | 保存 / dirty |
|---|---|---|
| 新 `open_rack_panels: BTreeSet<RackPanelKey>` | `ProjectView`（state/project.rs:215-238） | ViewState に保存、`*` なし（Q18） |
| 削除 `open_video_fx_params`（:630）/ `open_plugin_params`（:634）/ 初期化（:991-992） | `ProjectEphemeral` | — |
| 削除 `inspector_device_panel_h`（:595, :984）→ 新 `rack_panel_heights: HashMap<RackPanelKey, f32>`（plugin / 映像 FX / VOICEVOX / 字幕 / Transform の Par の実測高。native と Limiter は式で決まるので入れない） | `ProjectEphemeral` | session |
| 置換 `inspector_hovered_device: Option<u64>`（:462）→ `inspector_hovered_row: Option<BypassTarget>` | 同 | session |
| 置換 `mixer_hovered_strip_section`（:457）→ `mixer_hovered_native: Option<u64>` | 同 | session |
| 置換 `master_hovered_section`（:470）→ `master_panel_hovered: Option<BypassTarget>` | 同 | session |
| 新 `sc_listen_device: Option<u64>` | 同 | 保存しない、undo なし |
| 新 `device_scopes_sent: Vec<u64>` | 同 | session |
| 新 `param_gesture_seen: HashSet<(u32, AutomationTarget)>` | 同（`scrub_gesture_seen` :707 の隣） | session |
| 削除 `arrange_dragging_track_volume`（:527-531, :974） | 同 | — |
| 置換 `active_param_gestures` → `HashMap<.., ParamSurface>`（state/recording.rs:73-74） | `RecordingState` | session |
| `ScrubGesture::ModDepth{surface, track_id, target}`（ui_ephemeral.rs:30）、`ScrubGesture::GroupTransform{device_id, param}`（:34） | `state/ui_ephemeral.rs` | session |
| `track_peak_display: Vec<(f32,f32)>`、`native_gr: NativeGrDisplay`、`master_limiter_gr: f32`、`device_spectra: HashMap<u64, Arc<[f32]>>`（旧 3 要素タプルと `master_strip_gr`: transport.rs:84-91 を置換） | `TransportState` | session |
| `strip_comp_open` / `strip_eq_open`（:226, :228） | `ProjectView` | 変更なし（doc だけ「組み込み Comp/EQ の帯」に直す） |

3 要素タプルを 2 要素にする箇所: state/project.rs:815, :823、TransportState::new（transport.rs:151-152, :163-164）、`resize_track_peak_display`（notes.rs:237-240）、view_build.rs:148-152、view_model.rs:242-243。`TrackMixEntry.gain_reduction_db`（app_types.rs:53-55, :85）と view_model.rs:261 も削除する。

### 10.4 共有描画部品 `daw_gui/src/view/native_device/`（C）

DAW 固有なので view/ に置く（不変条件 8。daw-ui core には入れない）。Rack / Mixer 帯 / マスターパネルが同じ関数を呼ぶ。

**`mod.rs`**
```rust
pub fn wid<K: Hash>(s: ParamSurface, o: RackPanelKey, part: &'static str, key: K) -> (ParamSurface, RackPanelKey, &'static str, K);
/// パラメーターの持ち主。store を解決済みで持つ (ツマミごとに track を線形探索しない)。
#[derive(Clone, Copy)]
pub struct ParamOwner<'a> { pub id: u32, pub lanes: &'a [AutomationLane], pub routings: &'a [ModRouting] }
impl<'a> ParamOwner<'a> {
    pub fn resolve(song: &'a Song, owner_id: u32) -> Option<Self>;   // param_stores
    pub fn of_track(t: &'a Track) -> Self;
    pub fn master(song: &'a Song) -> Self;
}
```

**`knob.rs`**
```rust
pub struct NativeKnobSpec<'a> {
    pub surface: ParamSurface, pub owner: ParamOwner<'a>, pub device: &'a NativeDevice, pub param: NativeParamId,
    pub rect: Rect, pub surface_bg: Color,
    pub dimmed: bool,            // モードに上書きされたノブ (CompMode::overrides) / OFF のフィルタバンド
    pub external_drag: bool,     // 同じ面で同じ param を動かす別 widget (カーブ点 / ホイール Q) のドラッグ状態
    pub scope: &'a LiveParamScope,
}
pub struct NativeKnobResponse { pub hovered: bool, pub dragging: bool, pub displayed_plain: f64 }
pub fn native_knob(app: &AppData, ui: &mut Ui<'_, AppData>, spec: &NativeKnobSpec) -> NativeKnobResponse;
/// つまみ + 下の数値欄 (Q12)。
pub fn native_knob_with_value(app: &AppData, ui: &mut Ui<'_, AppData>, spec: &NativeKnobSpec, value_rect: Rect) -> NativeKnobResponse;
pub fn limiter_knob(app: &AppData, ui: &mut Ui<'_, AppData>, surface: ParamSurface, rect: Rect, surface_bg: Color,
    scope: &LiveParamScope, value_rect: Option<Rect>) -> NativeKnobResponse;   // Ceiling
```

`native_knob` の処理（旧 `strip_knob` strip_sections.rs:645-718 を一般化）:
1. `target = NativeParam{dev.id, param}`、`plain = app.live_native_param(scope, owner, dev, param)`、`norm = plain_to_norm(target, plain)`、`default_norm` は `param.default_plain()` から求める。
2. `m = build_mod(app, target, norm, ModControlDomain::Norm, owner.id)`（view/modulation.rs:74。master は view_model.rs:605-613 で song 側を引く）。
3. `resp = ui.knob_at(wid(surface, Device(dev.id), "knob", param), rect, norm, default_norm, style, on_change, Some(m.modulation()))`。
   - `on_change` は `DeviceEvent::NativeEdit{id, NativeEdit::param(param, norm_to_plain)}`。
   - style は Gain 系（EQ Gain / Tone EQ / Bus Comp Makeup）なら BIPOLAR、それ以外は UNIPOLAR。
4. `push_param_gesture(ui, app, surface, owner.id, target, resp.dragging || field.dragging || spec.external_drag)`。**1 面・1 param につき 1 回だけ**呼ぶ。別々に呼ぶと、片方の「非ドラッグ」がもう片方を End で閉じてしまう。
5. `push_mod_depth_bracket(ui, app, surface, owner.id, &target, resp.mod_dragging)`。

数値欄は `scrubable_number_at` を使う:
- 範囲と書式は `automation_value_display(&target, None)`。
- `modulation: None`。◉ arm 中は `read_only`（scrubable_number.rs:210）。
- クリックで数値入力、ダブルクリックで既定値。text の確定は 1 イベントで完結する（mixer_strips.rs:722-733 と同じ作法）。

**`curve.rs`**（描画と点ドラッグで同じ写像を使う）
```rust
pub enum EqCurveSource<'a> { Channel(&'a EqSettings), Tone(&'a ToneEqSettings) }
pub struct CurveAxes { pub f_min: f32, pub f_max: f32, pub db_range: f32 }
impl CurveAxes {
    /// f は master_meter::spectrum::F_MIN/F_MAX (spectrum.rs:24-25) を SSoT に。db: Channel = 18 (旧 strip_sections.rs:73)、Tone = TONE_EQ_LIMIT_DB。
    pub fn for_source(src: &EqCurveSource) -> Self;
    pub fn freq_to_x / x_to_freq / db_to_y / y_to_db;
}
pub struct CurveLook<'a> { pub active: bool, pub spectrum_db: Option<&'a [f32]> }
/// 応答は common::dsp::{eq_stages, eq_magnitude_db, tone_eq_stages, tone_eq_magnitude_db} (daw_audio と同じ関数)。
/// spectrum_db は SpectrumAnalyzer の SPECTRUM_BANDS=768 対数帯 (spectrum.rs:22)。ui.spectrum_analyzer (ui/.../spectrum.rs:90) を
/// bg/border/grid/label 透明・fill = strip_eq_curve α0.15 で先に描き、0dB 線と 100/1k/10k 線、合成カーブの順に重ねる。
/// bypass は形を変えず線と面を薄くするだけ。点数 = (rect.w*0.5).round().clamp(24, 256)。
pub fn draw_eq_curve(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect, src: &EqCurveSource, look: &CurveLook);
pub enum CurveBand { Eq(EqBand), Tone(ToneEqBand) }
pub enum HandleAxes { Both, Horizontal /*HP/LP*/, Vertical /*Tone*/ }
pub struct CurveHandle { pub band: CurveBand, pub pos: (f32, f32), pub axes: HandleAxes, pub wheel_q: bool, pub on: bool }
pub fn curve_handles(src: &EqCurveSource, axes: &CurveAxes, rect: Rect) -> Vec<CurveHandle>;  // EqBand::BY_FREQ 順
```

旧 `draw_eq_curve`（strip_sections.rs:276-316）と master の `draw_eq` 内のカーブ（master_strip_ui.rs:206-244）、周波数範囲の定数 3 か所（strip_sections.rs:75-77 / master_strip_ui.rs:63-65 / spectrum.rs:24-25）をここへまとめる。

**`gr.rs`**
```rust
pub fn draw_gr_vertical(app, ui, id, rect, gr_db: f32, active: bool, bg: Color);                           // 旧 strip_sections.rs:255-274
pub fn draw_gr_horizontal(app, ui, id, rect, gr_db: f32, active: bool, range_db: f32, value_font: Option<f32>); // 旧 :427-463
pub const LIMITER_GR_SEGMENTS: usize = 12;                                                                   // 旧 master_strip_ui.rs:71 (1 seg = 1dB)
pub fn draw_gr_segments(app, ui, id, rect, gr_db: f32, active: bool, segments: usize);                     // 旧 :259-278
pub fn gr_text(gr_db: f32) -> String;                                                                        // 旧 strip_sections.rs:448-454
```

針メーター（master_strip_ui.rs:162-174）はマスターパネル専用のまま残す。

### 10.5 Rack（`view/track_inspector/`、R）

**幅**: `INSPECTOR_W: f32 = 360.0`（view/root.rs:30。参照 :115, :125, :129, :131 は式のまま）。

**画面**
```
| Serum                    [SC▾][⌨][GUI][x] |  plugin 行
| Comp 2  [▓▓░░ 80px]      [SC▾][Par][x]    |  追加分
| Comp    [▓▓░░ 80px]      [SC▾][Par][ ]    |  組み込み (× の列は空ける)
|  (Par)                                     |
| EQ      [~~curve 80×18~~]     [Par][ ]    |
| + Plugin                                   |
master:
| Bus Comp [▓░░]    [SC▾][Par][ ] |
| Tone EQ  [~~~]         [Par][ ] |
| Ozone              [GUI][x]     |
| + FX                            |  ← drag_list はここまで
| ───────── Post-Fader ────────── |  区切り (drag_list の外、入力を取らない)
| Limiter  [▓░░]         [Par][ ] |  固定行 (drag_list の外)
```

**行モデル**（S1c で `app_types.rs:217-361` を新設 `daw_gui/src/chain_rows.rs` へ移し、`pub use`。前例 app_types.rs:19）

```rust
pub enum ChainRowKind { Plugin(ChainEntry), Native(NativeRowEntry), ParallelBegin{..}, SplitParams{..}, Chain{..}, AddChain{..}, AddPlugin{..}, ParallelEnd{..} }
pub struct NativeRowEntry { pub device_id: u64, pub kind: NativeKind, pub builtin: bool, pub ordinal: u16, pub bypassed: bool, pub sc_wired: bool }
impl ChainRow {
    pub fn select_id(&self) -> Option<u64>;              // + Native
    pub fn drag_id(&self) -> Option<u64>;                // + Native (組み込みも掴める)
    pub fn panel_key(&self) -> Option<RackPanelKey>;     // Plugin(shows_param_panel) / Native => Device(id)
    pub fn bypass_target(&self) -> Option<BypassTarget>; // Plugin / ParallelBegin / Native => Device(id)
}
```

- `AppData::chain_rows()`（handler/parallel.rs:373-392）/ `push_chain_rows`（:394-415）に `Device::Native` の arm を 1 つ足す。master の末尾行（区切り / Limiter）は行モデルに入れない。

**`chain_list.rs`（S1c で分割した後）**
- **行高** = `base_row_h(kind)`（Native は `ROW_H`）+ SC パネル高 + `rack_panel_h(key)`。
  - `rack_panel_h` は、Native なら `native_panel::panel_height(kind)`、それ以外なら `rack_panel_heights.get(key)`。未測定なら 120 で描いて実測する。
  - 実測値 `Some(0.0)` はそのまま使う。今の「1.0 以下なら毎フレーム 280」（:94-98）は廃止する。
- **slot**: 掴めるのは Plugin / Native / ParallelBegin。slot の位置は `build_list_rows`（:282-327）と同じ。
- **`valid_drop`**: drag_list を呼ぶ前に `ctrl`、外部運搬の `DEVICE_DRAG_KIND` payload（`ui.drag_payload::<DeviceDragPayload>(DEVICE_DRAG_KIND)`: drag_drop.rs:51）、内部運搬の id 列（`OnceCell` で 1 度だけ作る）を用意する。判定は `device_guard::any_permitted(song, ids, Relocate{dest: slot_targets[slot].0, copy: ctrl})`。これで次の slot が有効にならない: 組み込みを Parallel の中へ落とす slot、非 Ctrl で他トラックへ運ぶ slot。`valid_drop` は slot ごとに毎フレーム呼ばれる（drag_list.rs:146-158）ので確保しない。
- **hover の publish**: drag_list の hover 行の `bypass_target()` と master 末尾行の hover を合わせ、変化したときだけ `inspector_hovered_row` に 1 回書く（:158-167 を置換）。
- **master 末尾**（`cursor_track_id == MASTER_TRACK_ID` のとき、`list_rect` の直後に `draw_master_tail(app, ui, x, w, y) -> f32`）:
  - `draw_post_fader_divider`: 高さ 16、1px 線の中央に「Post-Fader」を font 10・`text_dim` で描く。入力は取らない。
  - `draw_master_limiter_row`: `ROW_H`、背景は `draw_row_bg`、「Limiter」+ GR セグメント 80×8（`draw_gr_segments`、12 段）+ `[Par]` → `ToggleRackPanel(MasterLimiter)`。× 列は空け、SC は出さない。右クリックは「有効化 / 無効化」だけ。小表示のダブルクリックで `MasterLimiterEdit::On(!on)`。Par が開いていれば直下に `native_panel::limiter`。
  - drag_list の外に描くので、掴めず、落とし先にもならないことが構造で決まる。

**`native_row.rs`（新設）`draw_native_row`**（右から左、深さ 0 の行幅 336）

| 要素 | 幅 | 内容 |
|---|---|---|
| × 列 | 26 | 追加分だけ `[x]` → `RemoveDevices`。組み込みは空ける（Q10 は × の有無で見分ける。ボタン列の x を plugin 行と揃える） |
| `[Par]` | 44（+2） | `ToggleRackPanel(Device(id))` |
| `[SC▾]` | 34（+2） | Comp / BusComp だけ。`open_sidechain_panel` を toggle。配線済みなら ON 色 |
| 小表示 | 80（+4） | EQ / Tone EQ は `draw_eq_curve`（80×18、`live_native_device` の値、`active = !bypassed`）。Comp / Bus Comp は `draw_gr_horizontal`（80×8、`transport.native_gr.get(id)`） |
| 名前 | 残り | `display_name()`。深さ 0 で Comp 系 130px、EQ 系 166px、深さ 1 段ごとに −4。OFF は `text_faint`、小表示は α 0.45 |

- 小表示で `ui.take_double_click_in_rect(mini)`（ui.rs:1825）が取れたら `SetDevicesBypassed{[id], !bypassed}`（Q15）。単クリックは drag_list の click（選択）に流す。

**`row_menu.rs`（S1c で chain_list.rs:231-278, 864-950 から移設し、項目を型にする）**

```rust
enum DeviceMenuItem { Bypass, Group, Ungroup, Rename, Color, Copy, Cut, Paste, Duplicate, AddChain, Delete }
fn menu_items(row: &ChainRow, song: &Song) -> &'static [DeviceMenuItem];
fn apply(app: &mut AppData, item: DeviceMenuItem, row_id: u64, anchor: Rect);
```

| 行 | 項目 |
|---|---|
| Plugin / 追加 Native | Bypass, Group, Copy, Cut, Paste, Duplicate, Delete |
| **組み込み Native** | Bypass, Copy, Paste, Duplicate（Q5） |
| Parallel | Bypass, Ungroup, Rename, Color, Copy, Cut, Duplicate, Delete |
| Chain | Rename, Color, Duplicate, AddChain, Delete |

今は index を直書きしており、配列が variant ごとにずれうる（chain_list.rs:878-937）ので型にする。選択に組み込みが混ざっていても、Delete / Cut / Group は handler 側で組み込みを落とす。

**展開部の余白で press を取る**
- SC パネルと Par パネルの背景は、描画の最初に次を行う: `primary_just_pressed` で、rect 内にあり、`!popup_open` なら `ui.claim_press(("rack_expansion_bg", key))`（click.rs:59）。
- 子 widget はその後に名乗るので子が勝つ（click.rs:56-58）。drag_list は次のフレームで `press_taken_from` によりセッションを捨てる（drag_list.rs:183-186）。
- 行を掴めるのはヘッダ 26px だけになる。今の plugin パネルの「余白を縦に動かすと行が動く」も直る。

**Par の開閉（Q11 / Q18）**
- `toggle_rack_panel(key)` で集合を反転する。閉じたら `rack_panel_heights.remove(&key)`。
- `device_panel/mod.rs` の契約を `(app, ui, ctx: PanelCtx { device_id, x, w, y }) -> f32` に変える。
  - `plugin_params.rs` / `video_fx.rs` / `group_transform.rs` / `text_event.rs` / `talk.rs` / `clip_voice.rs` / `lipsync.rs` の widget id と bracket の鍵に device_id を入れる。
  - `ScrubGesture::GroupTransform{device_id, param}`、`InspectorScrubField::Text{device_id, field}` / `Talk{device_id, kind}`（app_types.rs:517-554）。
  - view/modulation.rs:168-192 の `F::Text(..)` 7 arm も直す。
- `subtitle_param_panel_open()` / `voicevox_param_panel_open()` / `open_param_panel_plugin_id`（automation_lanes.rs:704-725）は削除する。
- `inspector_plugin_params(device_id)` / `inspector_video_fx_params(device_id)` / `inspector_group_transform_summary(device_id)`（:473-504, :509-550, :627-702）は引数を取る形にする。
- 同じ種類の Par を 2 枚同時に開いても、片方のドラッグがもう片方の bracket を 1 フレームで閉じる事故（track_inspector/mod.rs:79-84）は起きない。

### 10.6 Par パネル（`view/track_inspector/native_panel/`、R）

**レイアウト定数**（`layout.rs`。描画と `panel_height` が同じ定数を読む）
- `PAD=6`、`HEAD=14`（font 10）、`KNOB=24`、`KNOB_GAP=2`、`NUM_H=16`
- `CELL = KNOB + KNOB_GAP + NUM_H`、`ROW_GAP=4`、`CURVE_H=72`、`BAR_H=18`、`SWITCH=44×16`
- 列幅 = `ctx.w / 6.0`（深さ 0 で 56。Parallel 内の追加分は幅に追従する）

| 種類 | `panel_height(kind)` | 中身 |
|---|---|---|
| EQ（`eq.rs`） | `PAD + CURVE_H + ROW_GAP + HEAD + 3·(CELL + ROW_GAP) + PAD` | `eq_graph`（全幅 × 72、§10.7）→ 見出し `HP LF LMF HMF HF LP` → Freq 行 6 セル → Gain 行（HP・LP は `[ON]` = `EqBandOn`、LF〜HF はセル）→ Q 行（LF・HF は `[Bell]` = `EqBell`、LMF・HMF は Q セル、HP・LP は空き） |
| Comp（`comp.rs`） | `PAD + BAR_H + ROW_GAP + HEAD + CELL + KNOB_GAP + NUM_H + PAD` | 上段 `[LEV\|CMP\|LIM]`（3×40、`CompMode`）+ 横 GR バー + `gr_text` → 見出し `Thr Rat Atk Rel SC Gain` → 6 セル（`CompMode::overrides` のセルは dimmed: strip_sections.rs:699-708 と同じ）→ SC 列の下に `[Listen]`（点灯 = `sc_listen_device == Some(id) && !bypassed`。押すと `SetScListen{ if 点灯 { None } else { Some(id) } }`） |
| Bus Comp（`bus_comp.rs`） | `PAD + BAR_H + ROW_GAP + HEAD + CELL + PAD` | 全幅 GR バー → 見出し `Thr Ratio Atk Rel Makeup` → Thr / Atk / Rel / Makeup はセル（Atk / Rel は段階）、Ratio 列は `[2\|4\|10]`（3×18、`NativeEdit::param(BusComp(Ratio), idx)`）+ 値ラベル |
| Tone EQ（`tone_eq.rs`） | `PAD + CURVE_H + ROW_GAP + HEAD + CELL + PAD` | `eq_graph`（点は上下のみ）→ 見出し `Low LoMid High` → 3 セル |
| Limiter（`limiter.rs`） | `PAD + BAR_H + ROW_GAP + HEAD + CELL + PAD` | `draw_gr_segments`（12）+ dB → 見出し `Ceiling` → `limiter_knob` 1 セル |

**`cell.rs`**: `param_cell(app, ui, ctx, owner, dev, param, col, row_y, external_drag)` は格子の位置決めだけを行い、`native_knob_with_value(surface = ParamSurface::Rack)` を呼ぶ。数値欄は 52×16・font 10。

### 10.7 EQ カーブの点操作（`native_panel/eq_graph.rs`、R）

1. `draw_eq_curve(rect, src = live_native_device の params, look { active, spectrum_db: app.device_spectrum_db(id) })`
2. `curve_handles` の各点を `ui.xy_point_at(wid(Rack, Device(id), "pt", band), bounds=rect, pos, axes, wheel_enabled = handle.wheel_q, style, on_change)`（§10.16）で描く。
   - HP / LP: `XyAxes{x:true, y:false}`（0dB 線上）。LF〜HF: 両軸。Tone EQ: `{x:false, y:true}`。
   - ホイールは `EqBand::has_q_knob()`（channel_strip.rs:165）の LMF / HMF だけ有効。
3. 点の px を `CurveAxes` で Freq（log）/ Gain（linear）に戻し、変わった軸だけを `NativeEdit::Params` 1 イベントにまとめて発行する。OFF のバンドや bypass 中のデバイスは `apply` が自動で ON にする。
4. ホイール 1 notch で `Q *= 2^(±1/8)`、`EQ_Q_MIN..=EQ_Q_MAX`（channel_strip.rs:212, 214）に clamp する。
5. ジェスチャー:
   - 点のドラッグ中はそのバンドの Freq / Gain（Tone は Gain、HP/LP は Freq）について、ホイールの 400ms 窓（`XyPointResponse.wheel_active`、K18）の間は Q について、`external_drag = true` を下のセルへ渡す。
   - つまみと点のドラッグは OR されて `push_param_gesture` 1 回にまとまる。
   - Freq と Gain の Begin が同じフレームに 2 本来ても、`begin_gesture` は後勝ちで（song_doc.rs:548-553）、End は集合が空になった時点で閉じる。undo は 1 step。
6. 描画順は graph → セルなので、点の状態は同じフレームのセルに反映される。

### 10.8 Mixer 帯（`view/strip_sections.rs`、M）

- **引き方**: `song.builtin_natives(owner)`（チェーン順）から Comp と Eq を取る。追加分は引かない（Q16）。
- **並び**: `comp_first` = チェーン上で Comp が Eq より前か。開閉は種類ごとに `strip_comp_open` / `strip_eq_open` を使う。`head_height`（:103-107）/ `extra_head_height`（mixer_strips.rs:172-174）/ root.rs:159-169 の式は変わらない。
- **`draw_head(app, ui, owner: ParamOwner, rect, pad, bg, scope: &LiveParamScope)`**: `BandCtx { app, owner, bg, comp: Option<&NativeDevice>, eq: Option<&NativeDevice>, comp_first, scope }`。
- **常設サムネイル**（:214-238）: カーブを全幅、GR 縦バーは**左端 8px に固定**（今と同じ。全 ch で GR の位置が揃う性質を保つ）。当たり判定も今と同じで、左 = Comp、残り = EQ、クリックで `ToggleStripSection`（全 ch 一括の開閉）。面の色は各デバイスの `!bypassed`。GR は `transport.native_gr.get(comp.id)`。バイパスは Q が担う（ダブルクリックは割り当てない: :207-210）。
- **部品の差し替え**（寸法は変えない）

| 旧 | 新 |
|---|---|
| `strip_knob`（:645-718） | `native_knob(surface: MixerStrip)`（ノブ 20px） |
| モード 3 択（:330-354） | `NativeEdit::CompMode` |
| HP/LP ●（:487-497）/ [B]（:529-540） | `EqBandOn` / `EqBell` |
| ▶ Listen（:398-425） | `SetScListen`（点灯とトグルは Par と同じ規則） |
| GR 行（:427-463）/ GR 縦（:255-274）/ カーブ（:276-316） | `gr.rs` / `curve.rs` |
| widget id（:258, :340, :377, :390, :411, :432, :456, :481, :490, :498, :519, :533, :544, :573） | `wid(MixerStrip, Device(..), part, key)` |

- 組み込みが一時的に見つからなければ、そのセクションは面だけ塗り、ノブも hover も出さない。高さは確保し、panic しない。
- **`publish_hover`**（:173-188）: `mixer_hovered_native = Some(device_id)`。自分が立てた値だけを消す規則は今のまま。
- **`mixer_strips.rs`**
  - `mixer_strips::draw`（:200 付近）で `LiveParamScope` を 1 回だけ組み、借用で渡す。
  - `drag_flags`（:449-459）と呼び出し（:376, :424）を削除する。
  - `draw_strip` の `gain_reduction_db` / `was_dragging_vol` / `was_dragging_pan`（:473-475, :497-498, :399, :436, :544）を削除する。
  - Pan / Volume / Send のジェスチャーは `push_param_gesture(MixerStrip)`（:726, :806, :1028。`was` の :995-998 は削除）、深さは :664, :814。

### 10.9 マスターパネル（`view/master_strip_ui.rs` / `view/master_panel.rs`、M）

- **引き方**: `song.builtin_natives(MASTER_TRACK_ID)` から BusComp / ToneEq を取り、`song.master_limiter` を読む。
- **並び**: Bus Comp と Tone EQ はチェーン上の前後、Limiter は常に一番下（Q17）。高さが足りないときに何を描くかは今の優先度（Bus Comp > Tone EQ > Limiter: :83-112）で決め、描くと決めたものを上の並びで配置する。
- **中身**: 寸法は今のまま（針メーター 72 / カーブ 40 / Limiter 12 セグメント: :36-72）。
  - ノブは `native_knob(MasterPanel, ParamOwner::master(song))` / `limiter_knob`。今の変調の穴（:404 の `None`）とジェスチャーの欠落がこれで塞がる。
  - カーブは `draw_eq_curve(Tone)`、GR は `native_gr.get(bus.id)` と `transport.master_limiter_gr`。
  - `format_master_value`（:413-432）と `master_knob`（:369-411）は削除する。
- **hover の publish を 1 か所にする**（stale を構造的に消す）
  - `master_strip_ui::draw(app, ui, rect) -> Option<BypassTarget>` は hover を返すだけにする。
  - `master_panel` の中身を `fn draw_panel(..) -> Option<BypassTarget>` にする。早期 return（:99-106、:376-378 の `rest_w < READOUT_MIN_W`、:411-414 の `strip_h == 0`、scroll_area 経由の :154-162）はすべて None を返す。
  - `pub fn draw(app, ui, rect) { let h = draw_panel(..); publish_master_panel_hover(app, ui, h); }` とし、差分があるときだけ Edit を積む。
- `desired_height()`（:76-79）と :411 の分割は変わらない。

### 10.10 plugin picker（R）

- `PluginPickEntry::build_all(db: Option<&PluginDatabase>)`（app_types.rs:178-199）: 内蔵 4 種を `NativeKind::ALL` の固定順で先頭に置き、その後に Parallel と DB の項目を名前順で並べる。
- `PluginCategory::Native`（タグ「内蔵」、色は `tag_fx`）を足し、`fn matches_filter(self, filter)` で `Fx` フィルタが Native にも一致するようにする（app_types.rs:111-146）。
- `rebuild_picker_entries`（handler/mixer.rs:831-837）: DB が無くても `build_all(None)` する（今は全消去している）。
- `refresh_picker_visible`（:839-879）: master でも Native を出す。フィルタは `matches_filter`。
- `select_plugin_from_db`（:35-60）: DB を引く前に分岐する。
  - `PARALLEL_PICKER_ID` → `AddParallel{dest, at: Default}`
  - `NativeKind::from_picker_id(&id)` → `AddNative{dest, kind, open_panel: open_gui}`
  - plugin の挿入位置（:113-122）は closure の中で `song.default_insert_index(dest)`
- `keep_open`（Ctrl+クリック）はそのまま（Q8）。Shift =「GUI を開かない」（plugin_picker.rs:234-238）を「Par を開かない」に写す。
- タグ色の match は関数外の `fn tag_color(category, theme)` へ出す（`draw` の nesting 7/34 を下げる）。

### 10.11 D&D・削除ガード・挿入位置（R。述語は F）

| 入口 | handler | ガード |
|---|---|---|
| plugin 行の ×（chain_list.rs:477）/ メニューの削除（:896）/ Delete キー（root.rs:958-962 → selection_view.rs:191） | `remove_devices` / `_inner`（devices.rs:863-985） | `permitted_ids(Remove)`。組み込みしか残らなければ何もせず status「組み込みの Comp / EQ は削除できません」を出す（undo も round-trip も積まない）。`remove_devices_inner` でも再度フィルタする。:913-921 は node id 単位で `open_rack_panels` / `rack_panel_heights` から外す |
| chain 行の ×（:595）/ chain メニューの削除（:935）/ Parallel の ×（parallel_header.rs:337）/ Parallel メニューの削除（chain_list.rs:917） | 同上 | 組み込みは Parallel の中に居ないので、規則を通すだけ |
| Ctrl+X（clipboard_ops.rs:161）/ メニューの切り取り（chain_list.rs:888, :915） | `cut_devices` / `_inner`（device_relocate.rs:160-184） | `permitted_ids(Cut)` をクリップボード書き込み（`serialize_devices_to_envelope` :212）と削除の両方に使う。組み込みはクリップボードにも載せない |
| Ctrl+G（root.rs:748-756）/ メニューのまとめる（chain_list.rs:886） | `group_devices`（parallel.rs:33-76） | closure の中で `permitted_ids(Group)` → `ids[0]` と dest を決める。空なら status「組み込みの Comp / EQ は Parallel にまとめられません」 |
| 行 D&D の内部 drop（chain_list.rs:181-196）/ 外部 drop（:212-225）/ 横へ持ち出してヘッダへ drop（:198-210 → arrangement_view.rs:131-145） | `relocate_devices` / `relocate_in_song`（device_relocate.rs:29-54, 442-552） | `valid_drop` と handler が同じ規則（`can_relocate`）を使う。落ちた数は `RelocateOutcome.rejected_builtin` に入れ、status「組み込みの Comp / EQ は他の場所へ移動できません（Ctrl でコピー）」。copy 分岐は `prepare_device_copies`、トラックを跨ぐ move は `assign_native_ordinals(KeepIfFree)` + `close_rack_panels_of` |
| 複製 D / Alt+D（clipboard_ops.rs:365-386）/ メニューの複製（chain_list.rs:895, 916, 940-950） | `relocate_in_song` の copy | 全て可。`dest_index: InsertAt::Index(i+1)` |
| 貼り付け（clipboard_ops.rs:278、chain_list.rs:891-894） | `paste_devices`（device_relocate.rs:272-338） | 全て可。選択が無いときの位置（:291-294）は closure の中で `default_insert_index`。`assign_fresh_ids`（:425-427）を `prepare_device_copies` に置き換え。aux の解決は `for_each_aux_slot_mut` + `drop_cyclic_aux_routes` |
| Parallel 解除（chain_list.rs:908） | `ungroup_parallel`（parallel.rs:79-98） | 規則のみ（中の追加分は最上位へ出てよい） |
| chain の複製（chain_list.rs:933） | `duplicate_parallel_chain`（parallel.rs:119-153） | `prepare_device_copies` |
| トラックの複製 / 貼り付け | handler/tracks.rs:415-461 | 組み込みは組み込みのまま新 id。旧形式のクリップボードは §6.6 |
| `+ Plugin` 行 / AddPlugin slot（parallel.rs:386, :482） | — | 変えない（ユーザーが明示した位置） |

- `draw_device_drag_preview`（root.rs:324-337）の文言を「プラグイン N」から「デバイス N」にする。

### 10.12 Q と hover（K24）

- 判定順は現行どおり: master パネル → Mixer（`mixer_active` でゲート）→ 変調ラック → Rack の行 → レーン → ノート → クリップ（root.rs:1092-1095, :481-565, :567-618）。
- **S1c**: root.rs:464-618（`q_device_targets` / `dispatch_toggle_mute` / `toggle_hovered_strip_section`）を新設 `view/bypass_toggle.rs` へ移し、root.rs:1092-1095 を `bypass_toggle::dispatch(app, ui, mixer_active, is_pianoroll_active)` の 1 呼び出しにする。
- **F**: `dispatch` は `take_shortcut("daw.toggle_mute")` のとき次を行う。
  - `app.hovered_bypass_target(mixer_active)` が Some なら `bypass_toggle_event` を push する。
  - None なら `dispatch_toggle_mute` に進む。その Rack 分岐（root.rs:497-507）は `inspector_hovered_row` を読み、`Device(id)` は既存の「選択 device があればそれら、無ければその行」で `SetDevicesBypassed`、`MasterLimiter` は `MasterLimiterEdit::On(!on)`。
- Rack の Par パネルの上で Q を押すと、drag_list の hover 行はパネル込みの高さなので、その device の bypass になる。数値入力中は daw-ui が単キーを止める。

### 10.13 SC 配線（Q19）と循環の除外（R。model は F）

- Comp / Bus Comp 行の `[SC▾]` → 既存の SC パネル（`sidechain_ports(id)` = port 0 の 1 本）→ `SetSidechainSource{port 0, source}` / `SetAuxInputTapPoint`。手順は plugin と同じ。
- 候補（`sidechain_source_choices`）と send の宛先候補から、Structural で循環になる行き先を除く。直接の編集も `set_aux_input` / `can_add_send` が拒否し、undo は増えない。
- `tap_source_choices`（follower 用）は依存辺を作らないので今のまま。
- SC パネルの開閉 `open_sidechain_panel: Option<u64>` は今のまま（要件は Par の独立だけを定めている）。

### 10.14 SC Listen（GUI、K25）

- **状態**: `ProjectEphemeral.sc_listen_device: Option<u64>`。Option 1 個なので「プロジェクト内で同時に 1 つ」が型で保証される（handler/mixer.rs:390-415, :454-463 の排他コードは消える）。
- **押したとき**: 点灯していれば `SetScListen{None}`。点灯していなければ `request_sc_listen(Some(id))` を呼び、bypass 中なら有効化（undo 1 step）してから Listen。今の「押すと Comp が自動で ON」（event.rs:2137-2140）を保つ。
- **点灯条件**: `sc_listen_device == Some(id) && !bypassed`。Listen 中に Q で OFF にすると engine は素通しし、▶ は消灯する。ON に戻すと Listen も戻る。
- **掃除**:
  - device が消えたとき: `prune_device_session_refs`（削除・切り取り・Parallel 解除・トラック削除・undo/redo）
  - 同じタブで別ファイルを開いたとき: `reset_song_scoped_state` が None を送る（LoadSong より先に届く）
  - respawn: `restore_tabs_after_respawn` で OpenProject の後、LoadSong の前に再送する
- 移動では id が変わらないので聴き続ける。コピー先には付いてこない。別トラックを表示しても Listen は続く（solo と同じ持続）。

### 10.15 Bounce In Place / Glue（`isolated_track_song` handler/bounce.rs:133-184、F。呼び出し元 bounce.rs:356、glue.rs:366）

| 対象 | 処理 |
|---|---|
| master | `master_fx_chain.clear()`（:136。組み込みも消える。engine は組み込みの有無を前提にしない）。`master_limiter = MasterLimiterSettings { on: false, ..Default::default() }` を明示する（既定値が変わっても r.md #92 の修正が壊れないように）。`song_lanes` / `song_mod_routings` から `MasterLimiter(_)` を retain で外す（On レーンで再び ON にならないように）。旧 :141-147 を置換 |
| トラック（両モード共通） | `for_each_aux_slot_mut(&mut kept.devices, \|_,_,s\| *s = None)`（plugin の `aux_inputs.clear()` :157-162 と同じ理由） |
| `pre_fx == true` | `remove_natives_in(&mut kept.devices)`（K28）。レーン / routing / On の再有効化は、:182 の `prune_dangling_param_targets` が機械的に消す。これで「内蔵ストリップが中和されず二重に掛かる」穴（critic 2-d-1）が塞がる |
| `pre_fx == false`（with FX） | native はそのまま焼き込む（plugin と同じ） |

doc（:104-132）の r.md #92 の記述を `master_limiter` / native に更新する。

### 10.16 daw-ui core（`ui/crates/ui/src/`、R。ドメイン知識を持たない）

1. **`widgets/xy_point.rs`（新設）**
   ```rust
   Ui::xy_point_at(id, bounds: Rect, pos: (f32, f32), axes: XyAxes, wheel_enabled: bool,
                   style: &XyPointStyle, on_change: impl Fn((f32, f32)) -> Edit<M>)
       -> XyPointResponse { position, hovered, dragging, wheel: f32, wheel_active: bool }
   ```
   - modal popup の下では何もしない。
   - press が当たり円の中なら `claim_press(wid)` し、掴み位置のずれを保持する（`take_drag_in_rect` は claim しないので使わない: ui.rs:2404-2473）。
   - Ctrl で 1/10（`FINE_DRAG_SCALE`）。Esc で press 時の位置へ戻す（knob.rs:320-331 と同じ契約）。
   - 軸ロックと `bounds` での clamp。
   - `wheel_enabled` なら毎フレーム `claim_wheel_in_rect(hit)` を呼び、`take_scroll_in_rect(hit)`（wheel.rs:19）で取る。
   - `wheel_active` = 最後のホイールから 400ms 以内（前例 knob.rs:337）。active の間は `request_redraw()`（ui.rs:1773）で失効フレームを起こす。
   - `XyAxes` / `XyPointStyle` / `XyPointResponse` を widgets/mod.rs と lib.rs で re-export する。
2. **`wheel.rs`**: `Ui::claim_wheel_in_rect(rect)` と `pub(crate) fn wheel_claimed_at_pointer() -> bool` を足す。前フレームに claim された矩形の上では、祖先の `scroll_area` がホイールを消費しない（lag-by-one）。状態は UiHost の `Vec<Rect>` 2 本で、フレーム頭に入れ替える。ui.rs の増分は 5 行。
3. **`widgets/scroll_area.rs:106`**: 条件を `(need_v || need_h) && !self.wheel_claimed_at_pointer()` にする（scroll_area は中身の closure より前にホイールを消費する: :102-110）。
4. **`widgets/scrubable_number.rs`**: `ScrubableNumberFormat::Choices{labels}`（§7.4、A）。
5. **`widgets/reorderable_list.rs:209`**: 「inspector の chain は幅 260px 前後」を「狭い縦リストでは」にする（core から daw_01 の寸法を消す）。

### 10.17 undo / dirty の境界

| 操作 | undo | `*` | 根拠・備考 |
|---|---|---|---|
| つまみ / 数値欄 / カーブ点のドラッグ | 1 gesture = 1 step | 立つ | `begin_gesture`（handler/param_gesture.rs:27）。複数の面に表示されていても途切れない |
| ホイールで Q | 400ms 窓で 1 step | 立つ | K18 |
| 数値入力の確定 / ダブルクリックで既定値 | 1 step | 立つ | |
| Q / 右クリック / 小表示ダブルクリックでの ON/OFF | 1 step「デバイスを無効化 / 有効化」 | 立つ | |
| bypass 中に Listen を押す | 有効化の 1 step だけ | 立つ | Listen 自体は Song の外 |
| active 中に Listen を押す / 解除 | なし | 立たない | |
| Par の開閉 | なし | 立たない | ViewState（Q18） |
| AddNative（+ Par を自動で開く） | 1 step | 立つ | undo 後も集合に id が残るが、描画と保存は存在する id だけ |
| 移動 / コピー / 削除 / Ctrl+G | 1 step | 立つ | 組み込みしか選んでいなければ `edit_song_checked` が false で step を積まない |
| 開いた直後 / 新規曲での組み込み補完 | なし | 立たない | `new` / `replace_song` の baseline 確定前 |
| 編集後の正規化 | その編集と同じ step | その編集に従う | §5.8 |

### 10.18 プロジェクトタブ（ProjectKey）

| 状態 | 持ち主（タブごと） | 休止中のタブ | respawn |
|---|---|---|---|
| `sc_listen_device` | GUI `ProjectEphemeral` / engine `ProjectShared` | Listen は続く | `with_project` 内で `set_sc_listen(現在値)`（tabs.rs:287-300） |
| `open_rack_panels` | `ProjectView` | そのまま | 不要 |
| `device_scopes_sent` / watch | `ProjectEphemeral` / `ProjectShared.device_scope_watch` | engine は scope project だけ計算する（tabs.rs:104 `SetScopeProject`）。休止中のタブの watch は保持だけ | `device_scopes_sent.clear()`（次のフレームで差分として再送） |
| `native_gr` / `master_limiter_gr` / `device_spectra` | `TransportState` / telemetry slot | poller はアクティブなタブの slot だけ読む（app.rs:1370-1376） | slot は engine が作り直す |
| hover 系 | `ProjectEphemeral` | 描画されていないタブの値は Q の解決に使わない | 不要 |

engine 側の watch と Listen は、`SetDeviceScopes` / `SetScListen` / `CloseProject` 以外では変えない。watch は「現在の song の id」を指すので、同じタブで別ファイルを開いたときは次のフレームの差分送信で正しい集合になる。

### 10.19 script API（M）

`daw_gui/src/script.rs` に足す:
- `daw.nativeDevices(trackId)` → `[{id, kind, builtin, ordinal, bypassed, index}]`（master は `MASTER_TRACK_ID`）
- `daw.nativeEdit(deviceId, editJson)` → `DeviceEvent::NativeEdit`
- `daw.nativeGainReduction(deviceId)`
- `daw.masterLimiterGainReduction()`

script.rs:1297 は `RelocateDevices{ dest_index: InsertAt::Index(dest_index) }`（S1c）。`daw.deviceChain` / `device_id_at` は `plugins()` 基準（script.rs:1257、device_addr.rs:50-60）なので影響しない。

---

## 11. テレメトリ

### 11.1 GR（`common/src/audio_bridge.rs` `ProjectTelemetry` :124-202）

- **削除**: `track_gr_db`（:134-138）、`master_gr_db`（:139-145）、`set_track_gr_db` / `set_master_gr_db` / `master_gr_db`（:317-333, :348-354）
- **追加**
  ```rust
  pub const MAX_NATIVE_METERS: usize = 256;
  pub native_meter_ids: [AtomicU64; MAX_NATIVE_METERS],   // device_id、0 = 空き (mod_slot_ids は AtomicU32 なので流用しない)
  pub native_meter_gr: [AtomicU32; MAX_NATIVE_METERS],    // f32::to_bits (dB ≤ 0)
  pub native_meter_generation: AtomicU64,                 // seqlock (mod plane と同じ作り :412-462)
  pub master_limiter_gr_db: AtomicU32,
  pub fn publish_native_meters(&self, it: impl Iterator<Item = (u64, f32)>);  // RT。残り slot は id 0
  pub fn read_native_meters(&self, out: &mut Vec<(u64, f32)>) -> bool;       // GUI。seqlock が破れたら false
  pub fn set_master_limiter_gr_db(&self, db: f32); pub fn master_limiter_gr_db(&self) -> f32;
  ```
- `track_meters`（:397-410）は `(L, R)` の組を返す。`clear_track_meters`（:335-346）は `clear_meters` に改名し、GR 面と limiter も 0 にする（呼び出し元: `reset` :283-296、daw_audio/src/main.rs:1132）。
- **publish（E）**: `ProjectRt::publish_track_telemetry`（engine.rs:585-604）を `publish_meters(slot, n_tracks)` に改名する。GR は `track_programs[..n_tracks]` と master program の `natives.iter().filter(|n| n.meter)` だけから出す（処理していない program の GR が前の値のまま残らない）。engine.rs:1085-1089 の limiter GR もここにまとめる。slot の並びは compile 順で、読み手は id で引く（不変条件 1）。
- **GUI（F）**
  - poller（daw_gui/src/main.rs:451-452, :578-591）: `peaks_buf` / `native_buf` を使い回す。`native_gr: active.read_native_meters(&mut native_buf).then(|| native_buf.clone())`、`master_limiter_gr_db: active.master_limiter_gr_db()`。
  - `AppEvent::TrackPeaksTick { project, tracks: Vec<(f32,f32)>, native_gr: Option<Vec<(u64,f32)>>, master_limiter_gr_db: f32 }`（event.rs:1139-1150）。None は前回値を保つ。
  - `on_track_peaks_tick(peaks, native: Option<&[(u64,f32)]>, limiter_gr_db)`（handler/mixer.rs:809-829、RELEASE 0.85 は今と同じ）。
  - `NativeGrDisplay { entries: Vec<(u64, f32)> /*id 昇順、正の減衰量*/ }`。
    - `get(id) -> f32`: 二分探索、無ければ 0。
    - `update(&[(u64,f32)], release)`: plane にある id は `update_peak(prev, (-gr).max(0), release)`、無い id は 0 へ減衰させ、`quantize(v / GR_METER_RANGE_DB, METER_STEPS) == 0` で捨てる。
    - `iter()`
  - idle digest（handler/activity.rs:125-135, :143-149）に `native_gr.iter()` と `master_limiter_gr` を `GR_METER_RANGE_DB` で正規化して混ぜる。

### 11.2 スペクトラム（Q14）

**shmem**: 新設 `common/src/device_scope_bridge.rs`（repr(C)、WIRE_SOURCES に手動登録）。`scope_bridge.rs` と同じく GUI が create し、audio が open する。

```rust
/// 同時に描画しうる EQ Par の上限。EQ Par 1 枚 ≥ 約 260px なので縦 3840px の画面でも 15 枚まで。
pub const MAX_DEVICE_SCOPES: usize = 16;
/// 192kHz × ポーラ省電力間隔 250ms (scope_bridge.rs:26-28) = 48000 < 65536。1 slot 約 512KB、全体 約 8.4MB。
pub const DEVICE_SCOPE_FRAMES: usize = 1 << 16;
#[repr(C)] pub struct DeviceScopeSlot {
    project: AtomicU64, device_id: AtomicU64,   // 見出し。device_id 0 = 空き
    header_generation: AtomicU64,               // 見出しを書き換えるたびに +1
    write_frames: AtomicU64,
    samples: [[AtomicU32; 2]; DEVICE_SCOPE_FRAMES],
}
#[repr(C)] pub struct DeviceScopeBridge { sample_rate: AtomicU32, _pad: AtomicU32, slots: [DeviceScopeSlot; MAX_DEVICE_SCOPES] }
impl DeviceScopeBridgeHandle {
    pub fn create(os_id: &str) -> Result<Self>; pub fn open(os_id: &str) -> Result<Self>;
    pub fn set_sample_rate(&self, sr: u32);
    pub fn set_slot(&self, k: usize, project: ProjectKey, device_id: u64);   // RT (見出しの書き手は scope project の render だけ)
    pub fn write_block(&self, k: usize, l: &[f32], r: &[f32]);               // RT (slot ごとの書き手は 1 op)
}
pub struct DeviceScopeReader { cursors: [(u64, u64); MAX_DEVICE_SCOPES] }
impl DeviceScopeReader {
    /// 見出し → サンプル → 見出し の順に読み、世代が変わっていたら破棄してカーソルを張り直す。
    pub fn read(&mut self, h: &DeviceScopeBridgeHandle, k: usize, out: &mut Vec<[f32; 2]>) -> Option<(ProjectKey, u64, ReadOutcome)>;
}
pub fn device_scope_shmem_id(parent_pid: u32) -> String;
```

**engine（E）**
- `SetDeviceScopes` を受けたら、切り詰めと重複除去をして `ProjectShared.device_scope_watch: ArcSwap<[u64; MAX_DEVICE_SCOPES]>` に store する。
- `DeviceCtx`（engine.rs:354-364）に `device_scope: Option<DeviceScopeCtx<'a> { bridge, headers: &'a mut [(ProjectKey, u64); 16] }>` を足す。`DeviceRt::process_buffer`（engine.rs:1419-1514）は `is_scope` の project にだけ Some を渡す（:1490-1497 と同じ判定）。見出し表は `DeviceRt` のフィールドに持つ。`process_buffer` の引数に `&DeviceScopeBridgeHandle`（main.rs:1009）を足す。
- `ProjectRt::render_buffer` は、`recording_lanes` の load（engine.rs:897-899）と同じ場所で自分の watch を load する。見出し表と違う slot だけ `set_slot` し、`NativeIo.scopes` を組む。この処理は helper `native_io_for_buffer` に切り出して nesting を増やさない。
- open は daw_audio/src/main.rs:103 の隣。書き出しでは `scopes: None`。

**GUI（F は受け口、R は送信と描画）**
- bootstrap（daw_gui/src/bootstrap.rs:580-600）で create し、`AudioSession.device_scope_shmem_id` を渡す。
- テレメトリポーラ（main.rs:437-620）が `DeviceScopeReader` で読み、`(project, device_id)` ごとに `master_meter::SpectrumAnalyzer`（spectrum.rs:44-293）を持つ。見出しから消えた id の analyzer は捨てる。
- 結果は `AppEvent::DeviceSpectrumTick { project, spectra: Vec<(u64, Arc<[f32]>)> }`（768 帯の `display_db`）で送る。handler は tick の project が現タブのときだけ `TransportState.device_spectra` に入れる。読み出しは `AppData::device_spectrum_db(id) -> Option<&[f32]>`。
- `wanted_device_scopes()`: `chain_rows()` の Native 行のうち、kind が Eq / ToneEq で `open_rack_panels` に入っているものを、上から最大 16 個。折り畳まれた Parallel の中は行が無いので外れる。
- `sync_device_scopes()`: `wanted != device_scopes_sent` なら `SetDeviceScopes` を送って記憶する。呼ぶのは runner のフレーム末（view/runner.rs:1553 の `drain_pending_gui_opens` の隣、+1 行）。headless テストでは明示的に呼ぶ。
- 上限を超えた Par にはスペクトラムを描かない。

---

## 12. 不変条件の守り方とエッジケース

### 12.1 不変条件

| # | 条件 | 守り方 |
|---|---|---|
| 1 | 安定 id | op / tap / GR 面 / scope 見出し / 値 IPC / Listen / Par の鍵 / hover / widget id / ジェスチャー所有者は、すべて device id（と ProjectKey、target）で指す。`ChainRow.index` はフレーム内の drop 解決だけに使う。`InsertAt::Default` は実行時に解決する |
| 2 | blob-less | 値 IPC は約 100 byte の Copy、`SetDeviceScopes` は 16 個以下の u64。サンプルは shmem |
| 3 | 宛先の型 | 新しい command はすべて `AudioCommand`、project 宛て。GUI 内は `DeviceEvent` の sub-enum 1 本 |
| 4 | RT で確保・ロックしない | 確保はすべて compile 時（`NativeScratch` / `ScStage` / `ListenBuf` / dry / `BusScAlign` の DelayLine）。RT は swap / コピー / atomic / `ArcSwap::load` のみ。既存の RT 確保（§18-B）も消える |
| 5 | edit_song が単一の口 | Song の変更は handler の `edit_song(_checked)` だけ。挿入位置・番号・ガード・正規化・prune も closure の中や SongDoc の口で Song から求める。Par / Listen / scope は Song の外 |
| 6 | live と export が同じ render | 組み込みは `run_chain_program` の op、limiter は `render_master_buffer` の中。違うのは `NativeIo`（Listen / scope）だけで、export は既定値 |
| 7 | WIRE_SOURCES | §5.12 + arch-lint の検査 |
| 8 | daw-ui core にドメイン知識なし | `xy_point` / wheel claim / `Choices` は数値・矩形・ラベル配列だけ。EQ の知識は `view/native_device` |
| 9 | サイズ budget | §17 |

### 12.2 エッジケース

| 状況 | 扱い |
|---|---|
| 組み込みを Parallel の chain の slot へ非 Ctrl で drag | `any_permitted` が false なので slot が valid にならない。handler も同じ規則で落とす |
| 組み込み + plugin を選んで plugin 行を掴み、Parallel へ | plugin だけ移り、組み込みは残る + status |
| 組み込みだけを選んで Delete / Ctrl+X / Ctrl+G | 何もせず status。undo / round-trip / クリップボードは不変 |
| 他トラックのヘッダへ組み込みを非 Ctrl で drop | 拒否 + status。Ctrl なら `Default` の位置へ追加分としてコピー |
| ガードが漏れた | 正規化が既定値・bypass で補い、レーンは prune される。これはバグの症状として G1 テストで検出する |
| 楽器より上に置いた Comp | UI は特別なことをしない（直結規則） |
| group-with-instrument | 追加分は楽器の後・組み込みの前に入る（`default_insert_index_in` は楽器を「組み込み以外」に数える）。移行直後の組み込みは pass 2 |
| 映像や字幕だけのトラック | 全トラックに組み込みが付く（トラックは v16 から audio/visual 統合: track.rs:43-51、全トラックが strip を持っていた現行 :67-73 と一致）。映像 FX は `if let Device::Parallel` で辿る（video_fx/mod.rs:265-270）ので Native は見えず、映像はそのまま通る |
| vocal 判定 / 群配置 / 口パク | `plugins()` だけを見るので影響しない |
| 同じ種類の組み込みが最上位に 2 つ | 正規化が 2 個目を降格する。表示側は先に見つかった方を使い、panic しない |
| 組み込みを並べ替えた直後 | 次のフレームで Mixer 帯とマスターパネルの上下が入れ替わる。widget id は device id なので入力状態は移らない |
| Mixer 帯と Rack Par に同じつまみ | 所有者は面つき。ドラッグしていない面は何もしない。undo 1 step、録音も途切れない |
| ドラッグ中につまみが消える（Delete / Ctrl+Tab / Ctrl+Z） | 次のフレーム末に sweep が閉じる。PluginWindow / VideoPreview は対象外 |
| Q で OFF にした直後につまみや点を触る | ON に戻る。plugin は戻らない |
| On のレーンがあるのに Q で OFF | 録音中でなければレーンが勝つ（今の strip と同じ: automation.rs:217-231）。行の薄い表示は静的な値 |
| 静的には bypass で On レーンがある native | `can_activate` なので SC の依存辺・tap・PDC に数える。レーンで ON になった瞬間から外部 SC で検出する |
| Q で ON にした直後、LoadSong が届く前 | `sc_mode` は直前の compile の値。On レーンが無い bypass 状態からだと Staged が未設定なので、1 フレーム以内は自分の音で検出する |
| Limiter の ON をオートメーション | 遅延は compile 時に焼いてあり、OFF の間は遅延だけを通す。PDC は食い違わない |
| 段階式を変調 | norm で加算 → 連続の plain → `set` が段へ丸める |
| NaN / inf が IPC / LoadSong / クリップボードで来る | `sanitize` はフィールド単位（LF/HF の q を含む）、`set` は非有限を書かない。DSP の検出器は det を 0 に落とし、`gain_db` が非有限なら block 末に 0 に戻す |
| SR 変更 / n == 0 | 係数キャッシュは `(settings, sr)` をキーに組み直す。limiter の先読み長も張り直す。n == 0 は素通し |
| kind の食い違い（防御） | 素通し、GR 0。状態の引き継ぎは同じ種類のときだけ |
| 値 IPC がまだ LoadSong で届いていない device を指す | `song_values` は何もしない。後続の LoadSong が値を運ぶ |
| SC が自トラックの PostFx / PostFader | `ScMode::None`（feedback なので自分で検出） |
| GWI の prefix にある `OwnPreFx` | `ctx.own_pre_fx` が None なので自分で検出 |
| bypass 中の Parallel の中の native | op も slot も無いので tap を出さない。GR も出ない |
| staging した長さが n より短い | 足りない分は 0 |
| GR 面の上限 256 を超える | 組み込みを優先。組み込み Comp は最大 33 個なので必ず出る |
| scope watch が 16 を超える | GUI が上から 16 個に絞り、engine も切り詰める |
| Par を開いたまま undo で device が消える / 戻る | 集合は触らない。描画は表示中の行だけ、保存は存在する id だけ。戻れば開いた状態で出る |
| 折り畳んだ Parallel の中の device の Par | 行が無いので描かず、scope からも外れる |
| 字幕の Par を Text クリップ未選択で開く | 実測高 `Some(0.0)` を保持するので、空の 280px が居座らない |
| インスペクタがあふれていて EQ の点の上でホイール | 前フレームの `claim_wheel_in_rect` があるので scroll_area は消費せず、点が Q を受ける。点の外では通常どおりスクロール |
| 移動 / コピー後の Par（Q18） | 移動したサブツリーの id を集合から外す（同じトラック内の並べ替えを含む）。コピー先は新 id なので閉じている |
| ordinal | 同じ種類が 0 個なら番号なし、あれば 2 以上の空き番号。並べ替えでは変わらない。トラックを跨ぐ移動は空いていれば保つ |
| 他プロジェクトから device を貼り付け | `prepare_device_copies` で追加分になり、番号を振り直す。存在しない SC 配線と循環する配線は落とす |
| plugin DB が無い環境 | picker は内蔵 4 種と Parallel を出す。追加は DB を引かない |
| 書き出し中の編集 | `edit_song_checked` が false を返し、値 IPC も送らない |
| migration で devices / master_fx_chain / ids のキーが無い | 作る |
| v38 に `ids.next_device_id` が遅れている | `max(next_device_id, 最大 node id + 1)` から採番する |
| 旧ビルドのクリップボードのトラック | §6.6 で移行する |
| `device_chain_smoke` への影響 | `daw.deviceChain` / `device_id_at` は `plugins()` 基準。`relocateDevices` の dest index（device_chain_smoke.js:165-182）はすべて組み込みより前なので、assert は変わらない |

---

## 13. 変更・新設・削除するファイル（全列挙）

関数単位の詳細は各節にある。ここでは単位ごとにファイルを列挙する。

### 13.1 common

| 区分 | ファイル（行） | 内容 | 単位 |
|---|---|---|---|
| 新 | `model/native.rs`、`model/native/{comp,eq,bus_comp,tone_eq,chain_rules}.rs`、`model/native_param.rs`、`model/master_limiter.rs`、`model/param_range.rs`、`model/param_address.rs`、`routing_deps.rs`、`device_scope_bridge.rs`、`project/native_migration.rs` | §5 / §6 / §11 | F |
| 新 | `model/sections.rs`、`model/section_ops.rs`、`model/load_normalize.rs`、`model/plugin_instance.rs`、`model/source_pools.rs` | §17 の機械分割 | S1a |
| 削除 | `model/track/channel_strip.rs`、`model/master_strip.rs` | 移設 | F |
| 変更 | `model/device.rs`（:20-25, :440-488, :517-574, :626-639 ほか） | §5.2 / §5.5 | F |
| 変更 | `model/automation.rs`（:42-45, :145-153） | §5.3 | F |
| 変更 | `model/track.rs`（:6-10, :67-73, :265） | strip を削除 | F |
| 変更 | `model/view_state.rs`（:126-127 の後） | `RackPanelKey`、`open_rack_panels` | F |
| 変更 | `model/midi_bind.rs`（:73-124） | `BindingTarget` の 2 variant | F |
| 変更 | `model.rs`（:15-43 mod、:266、:205-265 doc、:624-630、:740、:849-853、:1379-1430、:1464-1484、:1486-1536 削除、:1602-1660 移設、:1706-1733、:1743-1761、:1854-1995） | §5 / §6.4（分割後のファイル上で） | S1a → F |
| 変更 | `model/ids.rs` | `alloc_device_id` | F |
| 変更 | `automation.rs`（:49-149, :212-272, :283-361、テスト :1427-1493） | `target_range` | F |
| 変更 | `project.rs`（:512-518、:673） | migration の呼び出し、字幕の挿入位置 | F |
| 変更 | `protocol.rs`（:146-161, :416-426, :648-649） | §8.10 | F |
| 変更 | `audio_bridge.rs`（:124-202, :283-296, :317-354, :397-410、tests） | §11.1 | F |
| 改名 | `channel_strip_dsp.rs` → `dsp.rs`、`lib.rs:6` | §5.11 | F |
| 変更 | `plugin_db.rs:243` の後 | picker id 定数 4 本 | F |
| 変更 | `mod_graph.rs`（:488-495、テスト :1646, :1648） | `param_stores`、prune の新しい名前 | F |
| 変更 | `build.rs`（:18-55） | §5.12 | S1a / F |
| 新 | `tests/fixtures/v38_strips.daw` | §15.1 | S0 |

### 13.2 daw_audio（E。F は型追従だけ）

| 区分 | ファイル（行） | 内容 |
|---|---|---|
| 新 | `graph/native.rs`、`native_dsp/{mod,comp,eq,bus_comp,tone_eq,limiter,tests}.rs`、`native_dsp/golden_v38.txt`（S0） | §8.2 / §15.2 |
| 削除 | `mixer/channel_strip.rs`、`mixer/master_strip.rs` | 移設 |
| 分割 | `graph/compile.rs` → `graph/compile/{mod,deps,emit,pdc,sidechain,tests}.rs` | S1b |
| 変更 | `graph/compile/*`（旧 :97-115, :164-174, :305-337, :584-637, :649-657, :663-682, :687-724, :737-767, :832-846, :864-915, :1022-1065） | §8.3 |
| 変更 | `graph/program.rs`（:10-25, :43-77, :187-217, :223-255, :259-276, :306-446、:554-567 削除、テスト :862, :908, :984, :1046, :1073, :1170） | §8.2 / §8.4.1 / §8.5 |
| 変更 | `graph/program_build.rs`（:24-33, :45-68, :127-152, :204-213） | §8.3.1 |
| 変更 | `graph/schedule.rs`（:109-117 の後、:165-172, :179-266） | `NativeSidechainTap` / `BusScAlign` / `master_limiter_latency` |
| 変更 | `graph/mix.rs`（:23-68 分割、:70-100 を compile へ移設） | §8.4.2 |
| 変更 | `graph/execute.rs`（:4-10, :26-34, :135-169, :322-328, :333-347, :377-410, :451-491, :499-525, :544-773, :782-907, :949-1160、テスト :1282, :1358, :1678-1698） | §8.4 |
| 変更 | `graph/band_split.rs:23` | `common::dsp` |
| 変更 | `automation.rs`（:16-18, :189-316, :364-374、テスト :1004-1070） | §7.3 |
| 変更 | `audio_worker.rs`（:90-136, :141-175, :268-288, :300-353, :537-620, :695-715） | §8.4.6 |
| 変更 | `engine.rs`（:269-272, :354-364, :387, :482-511, :579-604, :895-900, :994-1015, :1085-1089, :1235-1267, :1419-1498） | §8.5 / §11 |
| 変更 | `engine_shared.rs`（:279-405, :425-456） | `sc_listen_device` / `device_scope_watch` |
| 変更 | `export.rs`（:494-496, :762-790） | §8.9 |
| 変更 | `project_ctl.rs`（:583-782, :674-697） | 値 IPC、`SetScListen` / `SetDeviceScopes` の arm |
| 変更 | `song_values.rs`（:35-44、:100-146 削除、テスト :223-244） | §8.6 |
| 変更 | `launcher/runtime.rs`（:1262-1277） | `automation_lane_by_key`（F） |
| 変更 | `main.rs`（:99-105 の後、:1009、:1132） | scope の open、引数、`clear_meters` |

### 13.3 daw_gui

| 区分 | ファイル | 単位 |
|---|---|---|
| 新 | `event_device.rs`、`chain_rows.rs`、`handler/device_event.rs`、`handler/recovery.rs`、`handler/param_value.rs`、`view/bypass_toggle.rs`、`view/track_inspector/{plugin_row,chain_row,row_menu,audio_event_section,image_event_section,mouth_map_section}.rs` | S1c |
| 新 | `event_native.rs`、`handler/{native_edit,sc_listen,bypass_target}.rs` | F |
| 新 | `view/native_device/{mod,knob,curve,gr}.rs` | C |
| 新 | `handler/{device_guard,rack_view}.rs`、`view/track_inspector/native_row.rs`、`view/track_inspector/native_panel/{mod,layout,cell,eq,eq_graph,comp,bus_comp,tone_eq,limiter}.rs` | R |
| 変更 | `event.rs`、`app.rs`、`lib.rs`、`app_types.rs`、`device_addr.rs`、`handler/{mod,project,automation_lanes,mixer,selection_view}.rs`、`view/{root,arrangement_view,clipboard_ops,color_picker_overlay}.rs`、`view/track_inspector/{mod,chain_list,chain_sections,parallel_header}.rs`、`device_panel/{plugin_params,video_fx}.rs`、`script.rs`（DeviceEvent への機械置換と分割） | S1c |
| 変更 | `state/{project,recording,ui_ephemeral,transport}.rs`、`handler/{param_gesture,tabs,bounce,project,activity,notes,view_model,tick,modulation,automation,select_all,ipc,automation_lanes,device_relocate,devices,parallel,tracks,mixer,midi}.rs`、`view/{param_gesture,scrub_gesture,modulation,bypass_toggle,arrangement_view,mixer_strips,strip_sections,master_strip_ui,master_panel,transport}.rs`、`track_inspector/{mod,chain_list,parallel_header,modulation_rack/{mod,bodies},device_panel/{group_transform,plugin_params,video_fx}}.rs`、`automation_label.rs`、`automation_value.rs`、`widgets/arrangement/view_build.rs`、`main.rs`、`app_types.rs`、`clipboard.rs`（型追従と単一の口） | F |
| 変更 | `automation_label.rs`、`automation_value.rs`、`widgets/arrangement/view_build.rs`、`handler/{tick,param_value,modulation,midi,view_model}.rs`、`view/transport.rs` | A |
| 変更 | `view/{strip_sections,master_strip_ui,master_panel,mixer_strips}.rs`、`handler/master_panel.rs:18`（doc）、`script.rs` | M |
| 変更 | `view/track_inspector/**`、`chain_rows.rs`、`app_types.rs`、`view/{plugin_picker,root,runner,clipboard_ops}.rs`、`handler/{devices,device_relocate,parallel,tracks,mixer,view_state,automation_lanes,mod}.rs`、`clipboard.rs`、`main.rs`、`bootstrap.rs` | R |
| テスト削除 | `tests/channel_strip_edit.rs`、`tests/master_strip_edit.rs`（F が `app_state/native_edit.rs` に統合）、`tests/channel_strip_visual.rs`（M が `mixer_native_band_visual.rs` に置換） | F / M |

### 13.4 ui / docs / scripts

| ファイル | 単位 |
|---|---|
| `ui/crates/ui/src/widgets/xy_point.rs`（新）、`wheel.rs`、`ui.rs`、`widgets/scroll_area.rs:106`、`widgets/mod.rs`、`lib.rs`、`widgets/reorderable_list.rs:209`、`ui/CLAUDE.md` | R |
| `ui/crates/ui/src/widgets/scrubable_number.rs`（:44-85 と format / parse） | A |
| `scripts/arch_lint.sh`（WIRE-SOURCES 検査） | F |
| `scripts/arch_lint_baseline.txt` | 統合 |
| `tests/scripts/glue_bake_parity.js`（:247-297）、`tests/scripts/native_chain_smoke.js`（新） | M |
| docs（§16） | S0 / E / M / R / 統合 |

---

## 14. 同種の対象の grep 全件表

### 14.1 strip の住所・値・DSP・GR（grep: `master_strip|MasterStrip|ChannelStrip|StripState|apply_channel_strip|resolve_track_strip|resolve_master_strip|strip_gr|track_gr_db|master_gr_db|clear_track_meters|SetTrackStrip|SetMasterStrip|sanitize_strip|channel_strip_dsp|StripEdit|MasterStripEdit|StripSwitch|MasterSection|sc_listen|StripComp|StripEq|comp\.on|eq\.on|limiter\.on`）

| 場所 | 置き換え | 単位 |
|---|---|---|
| common/src/model/automation.rs:42-45, :145-153 | §5.3 | F |
| common/src/automation.rs:84-101, :236-248, :260-270, :315-332 | `target_range` | F |
| channel_strip.rs:504-527 / master_strip.rs:192-255, :376-422 / tests :432-478, :579-598 | `NativeDevice::param/set_param`。テストは F-C6 に置換（Listen のテストは削除） | F |
| channel_strip_dsp.rs:217-221, :328-332, :347-351, :385-486 | §5.11 | F |
| common/src/protocol.rs:423-426, :648-649 | §8.10 | F |
| common/src/audio_bridge.rs:138, :145, :285, :319-354, :407 | §11.1 | F |
| common/build.rs:26-29 | §5.12 | F |
| daw_audio/src/automation.rs:17, :189-316, :1004-1068 | §7.3 | F / E |
| daw_audio/src/engine.rs:270-272, :387, :589, :1014, :1086-1089 | `master_limiter` / `publish_meters` | F / E |
| daw_audio/src/main.rs:1132 | `clear_meters` | F |
| daw_audio/src/export.rs:494-496, :789 | `MasterLimiterState::new()` | F |
| daw_audio/src/graph/band_split.rs:23 | `common::dsp` | F |
| daw_audio/src/graph/compile.rs:838, :1649-1661（テスト） | `schedule.master_limiter_latency` / E-T8 | E |
| daw_audio/src/graph/execute.rs:34, :382-392, :854-865, :986-988, :1093-1114, :1149-1159, :1698 | §8.4 | F（呼び出し削除）/ E |
| daw_audio/src/mixer.rs:10-15, :121, :125, :165-166, :231-281 / mixer/{channel_strip,master_strip}.rs | 削除 / `native_dsp/` | F / E |
| daw_audio/src/project_ctl.rs:678-679 / song_values.rs:38-44, :100-146, :227-243 | §8.6 | F |
| daw_gui/src/automation_label.rs:44-68 / automation_value.rs:203-283 / view_build.rs:854-890 | §7.4 | F（arm）/ A（Choices） |
| daw_gui/src/handler/automation_lanes.rs:1299-1301, :1333-1335 | `target_plain_value` | S1c 移設 / F |
| daw_gui/src/handler/tick.rs:514-617（Strip 系 arm が無く録音されない） | `target_plain_value` | A |
| daw_gui/src/handler/mixer.rs:376-520 / :809-829 | 削除 / §11.1 | F |
| daw_gui/src/event.rs:1121-1133, :1145, :1961-1969, :2102-2181 / app.rs:1361-1368, :1372-1376 | §9.1 | F |
| daw_gui/src/state/project.rs:457, :470, :815, :823, :959, :962 / state/transport.rs:84-91, :151-152, :163-164 | §10.3 | F |
| daw_gui/src/handler/activity.rs:125-135, :143-149 | §11.1 | F |
| daw_gui/src/view/root.rs:459-476, :567-618, :1092-1095 | §10.12 | S1c / F |
| daw_gui/src/view/strip_sections.rs:24-37, :72-83, :85-551, :553-559, :645-718 | §10.8 | F（型置換）/ M |
| daw_gui/src/view/master_strip_ui.rs:22-25, :85-432 / master_panel.rs:98-197, :296-421 | §10.9 | F / M |
| daw_gui/src/handler/bounce.rs:141-147, :155-162, :182 | §10.15 | F |
| daw_gui/src/handler/master_panel.rs:18（doc）/ widgets/arrangement/mod.rs:332（doc）/ view/runner.rs:1123（`{ .. }`） | doc のみ / 変更なし | M |
| daw_gui/src/text_compose.rs:381（`#[cfg(test)]`） | 削除 | F |
| daw_gui/tests/channel_strip_edit.rs、master_strip_edit.rs、channel_strip_visual.rs | §13.3 | F / M |
| daw_gui/tests/scripts/glue_bake_parity.js:247-297 | 新形式に書き直し、§8 を追加 | M |
| script.rs / smoke_test.rs / loudness 系 | 0 件 | — |

### 14.2 `prune_dangling_mod_targets`（呼び出し 11 件 + doc 2 件）

| 場所 | 置き換え |
|---|---|
| common/src/model.rs:1475 | `prune_dangling_param_targets()`（明示的な呼び出しとして残す） |
| daw_gui/src/handler/bounce.rs:182 | 同上（明示的な呼び出しとして残す） |
| daw_gui/src/handler/devices.rs:1031、mixer.rs:634、modulation.rs:415、:455、:538、tracks.rs:239、:827 | **削除**（SongDoc の `enforce_edit_invariants` が担う） |
| common/src/mod_graph.rs:1646, :1648（テスト） | 新しい名前 |
| model.rs:2440、tracks.rs:537（doc） | 新しい名前 |

### 14.3 置き場（track か song か）の分岐の複製（18 か所 → `param_stores` 系）

| 場所 | 置き換え |
|---|---|
| daw_audio/src/graph/program.rs:554-567（:535, :582, :617） | `param_stores` |
| daw_audio/src/automation.rs:364-374 | `param_stores` |
| daw_audio/src/launcher/runtime.rs:1262-1277 | `automation_lane_by_key` |
| common/src/model.rs:1605-1616, :1650-1660 | param_address.rs で作り直す |
| common/src/mod_graph.rs:488-495 | `param_stores` |
| daw_gui/src/handler/view_model.rs:605-613 / :139-172 | `param_stores` / `live_param_value(owner)` |
| daw_gui/src/handler/modulation.rs:466-472, :493-514 | `param_stores(_mut)` |
| daw_gui/src/handler/automation.rs:295-310 | `param_stores_mut` |
| daw_gui/src/handler/automation_lanes.rs:523-530, :639-646 / :582-608, :764-790 / :1149-1177, :1211-1238 | `param_stores` / `param_stores_mut` + `push_lane` / `bound_owner_track` |
| daw_gui/src/handler/select_all.rs:57-70 | `param_stores` |
| daw_gui/src/handler/tick.rs:303-332, :371-449, :633-645 | `param_stores(_mut)` + `push_lane` |
| daw_gui/src/handler/device_relocate.rs:707-717, :742-772 | `param_stores_mut` / `push_lane` |
| daw_gui/src/handler/devices.rs:1009-1019 | 削除（prune に吸収） |

呼び出し側から store を渡す形（video_fx/mod.rs:185-223, :313-316）は複製ではないので残す。

### 14.4 device 束縛の手書き match → `bound_node_id(_mut)`

device_relocate.rs:628-634、:646-655、:668-673、:883-890 / devices.rs:998-1007（削除）/ tracks.rs:419-444。

### 14.5 ジェスチャーの発行と `was_*` / `active_param_gestures`

| helper | 場所 | 面 |
|---|---|---|
| `push_param_gesture_edges` → `push_param_gesture` | mixer_strips.rs:726, :806, :1028 | MixerStrip |
| 同 | strip_sections.rs:696 | 削除（`native_knob`） |
| 同 | parallel_header.rs:184, :294, :379 / chain_list.rs:653, :670 | Rack |
| 同 | transport.rs:402, :437 | Transport |
| 手書きの edge | arrangement_view.rs:229-256 | ArrangementHeader |
| `push_mod_depth_bracket` | mixer_strips.rs:664, :814 | MixerStrip |
| 同 | strip_sections.rs:697 | 削除 |
| 同 | parallel_header.rs:186, :301 / track_inspector/mod.rs:159 / modulation_rack/mod.rs:557 / bodies.rs:136 / group_transform.rs:137 / plugin_params.rs:112 / video_fx.rs:83 | Rack |
| 同 | transport.rs:388 | Transport |
| IPC | ipc.rs:354, :388 | PluginWindow |
| PiP | automation_lanes.rs:176, :208, :246, :276 | VideoPreview |
| `was_*` の算出（削除） | mixer_strips.rs:376, :407-408, :424, :444-445, :449-459, :497-498, :995-998 / chain_list.rs:638, :655 / parallel_header.rs:161, :265, :364 / transport.rs:399-401, :434-436 / strip_sections.rs:671-674 | — |
| `active_param_gestures` の書き込み | param_gesture.rs:28, :60-61 / automation_lanes.rs:176, :208, :246, :276, :962 / device_relocate.rs:899 | map 化 |
| 同 読み出し | automation_lanes.rs:134, :201, :269, :292 / tick.rs:230, :238, :466 / view_model.rs:157 | map 化 |
| `ScrubGesture::ModDepth` | scrub_gesture.rs:94 / ui_ephemeral.rs:30 | `{surface, ..}` |

### 14.6 `live_param_value(_on)` の呼び出し

view_model.rs:135, :235 / mixer_strips.rs:1000 / chain_list.rs:635, :636 / parallel_header.rs:150, :252, :362（§7.5 の表）。

### 14.7 組み込みを消せる・包める・運べる経路

§10.11 の表のとおり。本番で `remove_device` を呼ぶのは devices.rs:34、device_relocate.rs:513、parallel.rs:57、:84 の 4 か所だけ（grep 確認済み）。

### 14.8 新規デバイスの挿入位置（grep: `insert_device(|devices.push(|master_fx_chain.push(|chain.splice(`、テスト除く）

| 場所 | 変更 |
|---|---|
| handler/mixer.rs:43-52（picker の Parallel）/ :113-122（picker の plugin） | `AddParallel{at: Default}` / closure 内で `default_insert_index` |
| handler/parallel.rs:26（`add_parallel`） | `InsertAt` を closure 内で解決 |
| handler/device_relocate.rs:291-294（選択なしの貼り付け） | closure 内で `default_insert_index` |
| view/arrangement_view.rs:138-143（ヘッダへの drop） | `InsertAt::Default` |
| **handler/project.rs:1161 / common/src/project.rs:673 / common/src/model.rs:1752** | `default_insert_index_in`（§6.5） |
| device_relocate.rs:314, :492, :540、parallel.rs:69, :89-90, :386, :482 | 変えない（指定された位置 / 元の位置） |

### 14.9 aux 入力（SC）→ `for_each_aux_slot_mut` / `aux_input_slot_mut` / `Song::set_aux_input`

handler/devices.rs:720-750 / modulation.rs:715-725 / device_relocate.rs:778-798, :803-825 / tracks.rs:451-460, :495-500 / parallel.rs:526-545 / clipboard.rs:657-661 / bounce.rs:157-162。

### 14.10 SC consumer の走査（engine）

compile.rs:104-108（chain tap）、:318-326（依存辺）、:750-752（input delay）、:882-914（emit）、:1061-1065（path latency）、mix.rs:79-80（`any_tap_at`、RT で確保）→ `aux_consumers` / `TrackDeps`。program_build.rs:205 `own_prefx_ports` は plugin 用に残す。compile.rs:245 `aux_outputs` は変更なし。

### 14.11 `Device` の網羅 match（本番）

device.rs（`id` / `bypassed` / `set_bypassed` / `for_each_node_id_mut` ほか）、program_build.rs:47-66, :129-150、handler/parallel.rs（`push_chain_rows`）。この 3 ファイルだけ。

### 14.12 RT で変調を渡していない / RT での確保

| 場所 | 扱い |
|---|---|
| execute.rs:864 | 呼び出しごと削除 |
| execute.rs:904 | `mod_plane` を渡す |
| audio_worker.rs:569 | 正当（面が null のときの既定値） |
| execute.rs:324, :406, :817 → mix.rs:79 → device.rs:532（`vec!`） | `program.snapshot_pre_fx` / `snapshot_post_fx` |

### 14.13 単一 Par 状態の参照（grep: `open_plugin_params|open_video_fx_params|inspector_device_panel_h|inspector_hovered_device`、テストとスクリプトは 0 件）

state/project.rs:462, :595, :630, :634, :960, :984, :991, :992 / handler/devices.rs:487-493, :509-515, :916-921 / handler/project.rs:92-93 / handler/automation_lanes.rs:479, :510, :628, :707 / chain_list.rs:85-98, :159-167, :436-446 / root.rs:469-471。doc のみ: app_types.rs:229, :1122、state/project.rs:465, :592-593, :633、automation_lanes.rs:476, :506, :622、devices.rs:482, :498。

### 14.14 `DeviceEvent` への機械置換（S1c、event.rs 以外）

| ファイル | 行 |
|---|---|
| src/app.rs | 1202, 1211, 1214, 1217, 1220, 1223, 1226, 1229, 1232, 1239, 1246, 1249, 1301, 1307-1314, 1317-1321 |
| src/handler/mixer.rs | 52 |
| src/handler/selection_view.rs | 191 |
| src/view/arrangement_view.rs | 139 |
| src/view/clipboard_ops.rs | 379 |
| src/view/color_picker_overlay.rs | 112, 115 |
| src/view/root.rs | 503, 754 |
| src/view/track_inspector/chain_list.rs | 177, 189, 218, 477, 493, 512, 595, 610, 625, 648, 665, 713, 753, 840, 858, 884, 886, 896, 906, 908, 917, 933, 934, 935, 944, 1044 |
| src/view/track_inspector/chain_sections.rs | 97, 159, 193 |
| src/view/track_inspector/device_panel/plugin_params.rs / video_fx.rs | 124 / 96 |
| src/view/track_inspector/parallel_header.rs | 74, 174, 218, 280, 337, 354, 374, 398 |
| doc のみ | src/handler/devices.rs:336, :858、src/state/project.rs:118 |
| tests/app_state/device_relocate.rs | 124, 193, 243, 263, 299, 359, 384, 427, 468, 491, 532, 552, 626, 653 |
| tests/app_state/group_track_lifecycle.rs | 194, 383, 466, 528 |
| tests/app_state/modulation_id_hygiene.rs | 203 |
| tests/app_state/parallel.rs | 43, 61, 74, 76, 117, 121, 124, 128, 135, 146, 151, 165, 177, 192, 194, 198, 219, 228, 233, 251, 255, 256, 268, 280, 283, 308, 309, 328, 330, 333, 351, 358, 371, 374, 391, 394, 412, 428, 432, 433, 437 |
| tests/app_state/pending_state_queue.rs | 74, 103, 234 |
| tests/app_state/plugin_load_failure.rs | 198, 242 |
| tests/chain_split_click.rs | 34 |
| src/script.rs | 1297（`InsertAt::Index`） |

### 14.15 280 / インスペクタ幅の前提

- コード: view/root.rs:30、chain_list.rs:4, :97、reorderable_list.rs:209
- 設計書: plan_parallel.md:29、plan_master_meters.md:177-178
- テスト:
  - tests/chain_split_click.rs:83-86（`x < 300.0` → `x < INSPECTOR_W`。約 345 で失敗していた: critic (1)-1）
  - tests/automation_hover_visual.rs:41（`920 + INSPECTOR_W as u32`）
  - tests/mixer_pan_readout_visual.rs:32（`680 + INSPECTOR_W as u32`）
  - tests/channel_strip_visual.rs:27（M が置換）
- `build_root` を使う残り 7 本（about_visual / arr_widget / dirty_guard_click / launcher_thumbnail_visual / loudness_report_visual / master_panel_visual / theme_visual）は R で名指し実行する。

### 14.16 デバイス数に依存するテスト（組み込みが全トラックの末尾と master の先頭に入るため）

45 行 / 9 ファイル: app_tests.rs（12: 例 :948-978）、tests/app_state/{device_relocate 3（例 :474）, group_track_lifecycle 3（:186, :223, :527）, modulation_id_hygiene 1（:211）, open_stays_clean 1, parallel 15（例 :46, :159）, pending_state_queue 5（:279-297）, track_delete 1}、tests/chain_split_click.rs（4）。F が id 基準か `plugins()` 基準に直す。`fake_plugin_loaded` / `device_id_at` / `daw.deviceChain` は `plugins()` 基準なので影響しない。

### 14.17 `Track::default()` で組み込みの無いトラックを作る経路

テストを除く daw_gui 25 か所 + `track_with`。列挙では守らず、SongDoc の `enforce_edit_invariants` が構造的に覆う（§5.8）。

---

## 15. テスト計画

**方針**
- 自明なテスト（ラベル文字列、`kind()`、`From`、`project()` の arm、`program_latency` の Native arm、改名だけ、本番の算術をテストに写しただけの突き合わせ）は書かない。
- 起動を伴う target（`grep -l CARGO_BIN_EXE_daw_gui daw_gui/tests/*.rs`、現在 13 本）は単位ごとには回さない。

### 15.1 S0（改修前の記録）

- `common/tests/fixtures/v38_strips.daw`: HEAD で `#[ignore]` の記録テスト（common/src/model/tests.rs）を使い、`save` で書き出す。中身:
  - 通常トラック: strip comp/eq ON、HMF Gain レーン、`StripComp{Ratio}` routing とその深さのレーン
  - GWI トラック（strip ON）
  - return トラック
  - Parallel 内の plugin
  - master strip 全 ON と `CompRatio` / `LimiterOn` レーン
  - `ids.next_device_id` が遅れているケース
- `daw_audio/src/native_dsp/golden_v38.txt`: 旧 `mixer/{channel_strip,master_strip}.rs` の tests に `#[ignore] fn record_golden()` を置いて記録する。刺激・統計・シナリオは E-T1。記録器は E が旧 module と一緒に削除し、fixture だけを残す（REUSE の blanket 指定は `make license-check` で確認）。

### 15.2 F

| # | 置き場 | assert |
|---|---|---|
| F-C1 | common model tests | `{"Native":{..}}` → Native、旧 plugin 配列 → Plugin（untagged fallback の順序に依存するため） |
| F-C2 | 同 | `normalize_native_devices`: Parallel 内の builtin の降格 / 役割違いの降格 / 同種 2 個目の降格 / 欠けの補充（通常は末尾に Comp, Eq、master は先頭に Bus, Tone）/ EQ の aux None / ordinal の修復（「同種 builtin の 1 と衝突する added の 1」を含む）。2 回目は false |
| F-C3 | 同 | `default_insert_index_in` の 6 例（§5.7 の表）+ Chain 宛ては len |
| F-C4 | 同 | 番号: Bus Comp 2 だけ → 3、空 → 1、一括コピー 2 個 → 2, 3、トラックを跨ぐ移動で空いていれば保つ・衝突すれば next |
| F-C5 | 同 | `can_relocate`: 組み込みの他トラックへの move は false、Parallel chain への move は false、同じトラックの最上位は true、copy はどこでも true、Parallel を自分の中の chain へは copy でも false |
| F-C6 | 同 | `NativeParams`: 4 種の全住所で、1 つを書いても他の読みが変わらない / `set(BusComp(Ratio), 1.4)` → 4:1 で読みは 1.0、Release 4.0 → Auto / 種類違い・非実在の組・非有限は false で不変 / sanitize: `lf.q = NaN` かつ `lf.bell`、`hp.gain_db = inf`、範囲外の Freq → 全フィールドが有限で値域内、2 回目は不変 |
| F-C7 | common automation tests（:1427-1493 を置換） | 全 target を列挙（NativeParam は `all_of` × 4、MasterLimiter を含む）: `is_invertible` なら 3 点が 1e-9 で往復、norm は単調非減少 / `is_affine` なら中点が一致、Log / LogWithOff / Toggle には一致しない例がある、ParallelSplitFreq は false |
| F-C8 | 同 | `can_activate`: bypassed + enabled な On レーン → true、disabled → false、On への routing → true |
| F-C9 | param_address tests | prune で消える: 消えた native / 別トラックの device を指すレーン / 種類違い / `Eq{Hp,Gain}` / 消えた chain の ChainGain / Parallel の id を指す PluginParam / それらの routing を深さに持つ routing / 消えた device の midi binding。残る: 実在する組み込み、master_fx_chain を指す song_lanes、MasterLimiter。2 回目は false |
| F-C10 | common project tests | v38 fixture を `load_project` → 組み込みの位置（通常は末尾 Comp → Eq、GWI も同じ、master は先頭）/ `bypassed = !on` / params 一致 / 各レーン・routing・深さが実 id の NativeParam / Ceiling・LimiterOn → MasterLimiter / 実在しない EQ 組の lane が消える / `next_device_id` が遅れていても衝突しない / normalize 2 回目で不変 / save → load で等値 / 混在形（glue_bake_parity 形）の合流 / devices キーが無い JSON |
| F-C11 | 同 | 同じ song を `migrate_legacy_song` + `from_value` + `ensure_ids`（script と同じ経路）で読むと同じ解決になる |
| F-C12 | routing_deps tests | G/A の循環を検出 / bypass 中の配線は Structural だけで数える / 自分の chain は辺にしない / `set_aux_input` は循環を拒否、自トラックを source にすると PreFx に固定 / `can_add_send` |
| F-G1 | `event_native.rs` cfg(test) | 4 種で bypass 中の `Params` → `bypassed=false` / `Params[(On(k),0)]` は無視して false / HP Freq → `hp.on`、`lp.on` は不変 / `EqBandOn{Lp,false}` → lp OFF、device ON / LMF OFF で LMF Gain → on / 同じ値の再送 → false / 種類違い・`EqBell{Hmf}`・Comp 以外の `CompMode` → false / `MasterLimiterEdit::Ceiling(5)` → clamp + on、`On(false)` の後は OFF のまま |
| F-G2 | `tests/app_state/native_edit.rs` | ① 組み込み Comp への NativeEdit → `SetNativeDevice` 1 通、同じ値は 0 通、undo 1 回で戻る、dirty ② 「Comp 2」への編集が組み込みを変えない ③ master Bus Comp → last_touched の owner が MASTER ④ `SetDevicesBypassed([native])` → last_touched が `On(kind)`、値 IPC 0 通、`flush_song_sync` 後の LoadSong で bypassed ⑤ Listen: active 中は dirty も undo も不変で IPC、bypass 中は有効化で undo +1、Listen 中の device を削除 → None + IPC、新規は None ⑥ GR: plane `[(c2,-6)]` → `native_gr.get(c2)==6`、None tick は不変、減衰して消える、`tick_visual_fingerprint` が変わる ⑦ `ToggleStripSection` は dirty にしない（channel_strip_edit.rs:173-187 を移植）⑧ 追加 EQ の削除でレーン・routing・深さが消え、undo で全部戻る（T18）⑨ Parallel の解除で ParallelOutGain / ChainGain のレーンが消える（T21）⑩ AddTrack で新しいトラックに組み込み 2 個（bypassed）が同じ undo step で入る |
| F-G3 | `tests/app_state/open_stays_clean.rs` | v38 fixture を開いた直後 `!is_dirty()`、新規タブも同じ |
| F-G4 | `view/param_gesture.rs` / `scrub_gesture.rs` cfg(test)（scrub_gesture.rs:114-186 と同形） | Begin(Rack) → End(MixerStrip) は no-op / End(Rack) で外れる / Begin 直後の同じフレームの sweep では閉じない / 在席印の無い sweep で閉じる / PluginWindow・VideoPreview は sweep で消えない / `ModDepth{Rack}` は MixerStrip の非アクティブな push で閉じず、◉ も残る |
| F-G5 | `handler/bypass_target.rs` cfg(test) | master hover `Device(id)` → `SetDevicesBypassed{[id], !cur}`、`MasterLimiter` → `On(!on)` / Mixer hover があっても `mixer_active=false` なら None |
| F-G6 | `handler/bounce.rs` cfg(test) | `isolated_track_song(t, true)`: kept に Native 0 個、NativeParam のレーン・routing 0、`master_limiter.on == false`、song 側に MasterLimiter / NativeParam なし / `(t, false)`: native の params・bypassed は元と一致、aux None |
| F-G7 | `tests/app_state/project_tabs.rs` | 2 タブで Listen → audio を respawn → 送信列が `OpenProject` → `SetScListen(Some)` の順で、LoadSong より前 |
| F-G8 | `handler/view_model.rs` cfg(test) | 再生中で song_lanes に NativeParam レーン → `live_native_param` がレーン値、停止中は model 値 / master chain の ChainGain も追従 |

回すもの: `cargo test -p common --lib`、`cargo test -p daw_audio --lib -- song_values`、`cargo test -p daw_gui --features daw_gui/script --lib -- event_native view::param_gesture view::scrub_gesture handler::bypass_target handler::bounce handler::view_model`、`--test app_state --test chain_split_click --test channel_strip_visual`。

### 15.3 E

| # | assert |
|---|---|
| T1 | golden（`native_dsp/tests.rs`）。刺激は 48kHz 2 s（xorshift の非相関ノイズを −40 / −12 / 0 dBFS で 0.25 s ごとに階段状、50Hz+1k+8k、インパルス列）、4096 サンプル窓の `rms_l/r, peak_l/r, sig_l/r(PRBS 重み), gr`。シナリオ: Comp 3 モード × SC {OFF, 150, 6k} / EQ 全バンド ±9 × Bell の有無・HP120・LP9k / 旧ストリップ Comp+EQ ON / Bus Comp ratio × release {300, Auto} × attack（Auto は buffer 256 / 512 / 1024）/ Tone ±6 / master 完全形（+6dB 入力、ceiling −1）。旧ストリップ由来は `build_program` + `run_chain_program` + `MasterLimiterState`（latency active）の全経路で走らせる。許容 `\|Δ\| ≤ 1e-6 + 1e-5·\|ref\|`、`\|Δgr\| ≤ 1e-4 dB` |
| T2 | device_id による状態引き継ぎ: GR を深く出している Comp で 20 block → plugin を差し込んで並べ替えた program に adopt → 1 block 目の GR が 0.05dB 以内で連続する。同じ id でも種類が違えば無音状態から始まる |
| T3 | crossfade: true→false→true で block 境界の隣接差が閾値以下 / 落ち着いた bypass では入力と bit 一致、GR 0 / 再開直後の出力が reset した単独 DSP と一致 |
| T4 | 既存の plugin SC テスト（compile 旧 :2103-2189, :2283-2514, :2559, :2603）を「宛先が native comp」でも回す table 駆動: tap の位置、`Staged` + `sc.is_some()`、input delay、master latency、Cycle が plugin と同じ / bypass 中で On レーンの無い native は循環しない / On レーンありなら数える |
| T5 | staging の 3 通りの借用（scratch / 同じ program の chain / 別 program（master を含む））で正しいバッファ。post-dispatch を通して group の native comp が SC で検出する |
| T6 | group の pass 2 で、組み込み Comp の threshold への routing で GR が変わる / volume の変調も効く / 録音中のレーンは解決しない / On レーンで active が切り替わる |
| T7 | Listen（K25d）: `[Comp A (Listen), EQ]` のトラック出力（PostFx）= A の検出信号、EQ の状態は進む / Listen が EQ の id → 出力不変 / A が bypass → 出力不変 / `NativeIo::default()`（export）→ 出力不変 |
| T8 | `on=false` + On レーン → `master_latency_samples` に先読み分、解決値 OFF の間は遅延だけでゲインなし / 静的 off でレーン無し → 遅延 0 / `reset()` 後のリングは無音 / `n==0` の早期 return でも `master_limiter_latency` が入る |
| T9 | GR 面の seqlock: `[11,4,9]` → `[9,11]` と publish すると 4 が消える、書き込み中は読み直す、`clear_meters` で全部 0 |
| T10 | scope の世代: device A のフレームを書き、見出しを B に変えて書くと、reader は A のフレームを 1 つも返さない。project が違えば別物 |
| T11 | `SetNativeDevice`: NaN・範囲外は丸める / 種類違いの params は何もしない / master fx chain の device にも届く |
| T12 | `make test-rt`（`cargo test -p daw_audio --features rt-assert`）: natives を含む `run_chain_program`（Staged / OwnPreFx / crossfade / Listen / scope）、`stage_native_sidechain`、`publish_meters`、PreFx tap を持つ track の `process_track_owned`、`render_buffer` の見出し同期で確保・解放 0 |
| T13 | v38 fixture → op 列: 通常 `[P…, N(comp), N(eq)]`、GWI の組み込みは `pass1_end` より後、master `[N(bus), N(tone), P…]`、`master_limiter_latency` は旧 `limiter.on` と一致 |
| T14 | return R の native comp の SC 元 S が latency L、R への send 元 A が 0 → `ProcessGroupFx(R)` の直前に `ApplyDelay{TrackScratch(R), L}`、master Mix の前の A に L。実行すると S と A に同じインパルスを入れて R の検出信号と main が同じサンプルに揃う。GWI prefix 宛ては `input_delay = L + buffer_frames` |
| T15 | pre-fader send の無い group でも、PostFx を source にした follower / SC が今の buffer のチェーン後の信号を読む |
| T16 | 同じ v38 fixture の offline render 2 回が bit 一致。`sc_listen_device` を立てても書き出しは変わらない |

回すもの: `cargo check -p daw_audio --all-targets`、`cargo test -p daw_audio --lib -- native_dsp graph automation song_values`、`make test-rt`、`cargo test -p common --lib -- audio_bridge device_scope_bridge`。

### 15.4 A

| # | 置き場 | assert |
|---|---|---|
| A-1（T19） | `handler/param_value.rs` cfg(test) | 「Comp 2」の Thr を NativeEdit → last_touched のラベルが「Comp 2: Thr」→ A → track store にレーン、`default_value == param(Thr)` / master Bus Comp は song_lanes / 移動後の A は移動先 |
| A-2（T20） | 同 | Touch で再生中に `ParamGestureBegin{Rack, NativeParam}` → `record_automation_points_for_tick` が 1 点挿す / SendGain と ChainGain でも点が入る |
| A-3（T26） | 同 | master に Parallel、chain gain 0.5 で A → `default_value == 0.5` |
| A-4（T27） | 同 | device を他トラックへ移した直後に旧 track_id の `AddModRouting` → 移動先の store に積まれる |
| A-5 | 同 | MIDI Learn: native Thr を触って Learn → `BindingTarget::NativeParam` / CC127 → 上限、`bypassed=false`、`SetNativeDevice` 1 通 / device を削除すると binding が消える |
| A-6（T28） | `automation_value.rs` cfg(test) | BusComp(Ratio): 1.0 → "4:1"、`parse("10:1") == 2.0`、"abc" → None |
| A-7 | `scrubable_number.rs` cfg(test) | `Choices`: "auto" → Auto の index、"3" → "3ms" の index（先頭数字の一致）、範囲外 → None |

回すもの: `cargo check -p daw_gui --all-targets --features daw_gui/script`、`cargo test -p daw-ui-core --lib -- scrubable_number`、`cargo test -p daw_gui --features daw_gui/script --lib -- automation_value handler::param_value handler::midi`。

### 15.5 C

単独のテストは持たない（振る舞いは M / R の visual テストで検証する）。`cargo check -p daw_gui --all-targets --features daw_gui/script`。

### 15.6 M

| # | 置き場 | assert |
|---|---|---|
| M-1 | `tests/mixer_native_band_visual.rs`（旧 channel_strip_visual） | EQ カーブがパラメーターに追従する / dark・light 両方でカーブが帯の背景に沈まない（画素差 > 20）/ セクションを開くと既存 strip が下がる / 組み込み EQ を Comp の前へ `RelocateDevices` → 「LEV」グリフの y > 「HP」グリフの y / 「Comp 2」を足して GR tick を Comp 2 にだけ入れる → 最初の strip の「LEV」は 1 個、`strip_gr` 色の矩形は 0 |
| M-2 | `tests/master_native_blocks_visual.rs` | Tone EQ を Bus Comp の前へ → カーブの y < 針メーターの y、どちらの並びでも「Ceiling」が最下 / Bus Comp 上に hover → `master_panel_hovered == Some(Device(bus))`、`ToggleMasterPanel` 後のフレーム → None、`strip_h == 0` の高さ → None、`rest_w < READOUT_MIN_W` → None |
| M-3 | `tests/param_gesture_surfaces.rs` | Mixer の Comp セクションと Rack の組み込み Comp の Par を同時に表示し、Mixer の Thr を press → 移動 × 4 → release。press 後のどのフレームでも `(track, NativeParam{comp,Thr})` が保持され、release 後の undo は +1 だけ / Mixer が見える状態でアレンジのヘッダ音量を同じ手順 → 保持、undo +1 / Latch で再生中にマスターパネルの Bus Comp Thr → song_lanes に NativeParam レーンと点 |
| M-4 | `tests/scripts/glue_bake_parity.js`（起動あり） | §7 を新形式に（`master_fx_chain` の組み込み Bus Comp を ON・thr −30・R10、`master_limiter{on, ceiling −20}`）/ §8 を追加: トラック 1 の組み込み Comp を ON（thr −30, ratio 10）で Glue → integrated の差 ≤ 0.5 LU |
| M-5 | `tests/scripts/native_chain_smoke.js` + `tests/native_chain_smoke.rs`（起動あり） | 大きい clip + 組み込み Comp（thr −40, ratio 10）で 1 秒再生 → GR > 3dB / Listen: bypass だったら `bypassed=false` になり master peak が変わる、書き出した WAV に Listen は乗らない / Limiter ceiling −6 に +6dB → master peak ≤ −5.9 dBFS |

回すもの（起動なし）: `--test mixer_native_band_visual --test master_native_blocks_visual --test param_gesture_surfaces --test master_panel_visual --test mixer_pan_readout_visual`。`strip_sections::THUMB_H` は pub のまま保つ（mixer_pan_readout_visual.rs:43）。

### 15.7 R

| # | 置き場 | assert |
|---|---|---|
| R-1 | `tests/app_state/native_rack.rs` | 挿入位置: `SelectPluginFromDb{NATIVE_COMP_PICKER_ID}` を `[Parallel A, Comp(b), EQ(b)]` に → `[A, Comp 2, Comp(b), EQ(b)]` / `[Comp(b), EQ(b), A]` → A の直後 / 組み込み以外 0 個 → 通常は上、master は下 / `PARALLEL_PICKER_ID` と `RelocateDevices{InsertAt::Default}` も同じ / deferred: Default の Relocate を積み、`AllPluginStates` の前に AddNative を挟む → Relocate の結果が組み込みより上 / GWI トラックで AddNative → 楽器の直後 |
| R-2（G1） | 同 | 通常トラックと master で、組み込みを含む選択に `RemoveDevices` / Cut / Group / 他トラックへの `RelocateDevices(copy=false)` / Parallel chain への Relocate → 組み込みの id・params・bypassed・レーンが残り、`!song.clone().normalize_native_devices()`（補充が起きていない）/ 組み込みだけの Remove → Song・undo 長・round-trip キューが不変で status / Cut の JSON に組み込み id が無い |
| R-3（G2） | 同 | `RelocateDevices(copy=true)` で組み込み → `builtin=false`、番号は K9、params 一致、レーンは複製されない（T17） |
| R-4（T16） | 同 | Parallel 内の追加 Comp を他トラックへ → レーン / routing / 深さ / 同じ Parallel の ChainGain 深さが移動先へ再採番され、gesture map の key も付け替わる（面は保持）/ master で足した「Comp」を通常トラックへ → 2 |
| R-5（T22） | 同 | トラックの複製 → 複製側のレーンが複製側の native id を指す、組み込みは組み込みのまま |
| R-6 | 同 | Par: native Comp と EQ を同時に開ける / snapshot → 別 app で restore → 同じ集合、dirty なし / move で閉じる、copy 先は閉じている、削除で外れる、`view=None` の restore で空 |
| R-7 | 同 | SC 配線: native Comp に `SetSidechainSource{0, Track(t2)}` → Some、`SetAuxInputTapPoint` が効く、EQ では何もしない / 子 A の Comp の候補に親 G が出ず、G を直接指定しても拒否され undo 不変 |
| R-8 | 同 | picker: `build_all(None)` の先頭 4 件が固定順 / master でも見える / クエリ "comp" と "f comp" で絞れる / `keep_open` で開いたまま |
| R-9 | 同 | scope: EQ の Par を開いて `sync_device_scopes()` → `SetDeviceScopes[eq]` 1 通、2 回目は送らない、Parallel を折り畳む → `[]` / タブ A で開く → B → A で A の再送なし |
| R-10 | 同 | Q: `inspector_hovered_row = MasterLimiter` で dispatch → `master_limiter.on` が反転 / `mixer_hovered_native` があればそちらが優先 |
| R-11 | `tests/native_rack_visual.rs` | 新規トラックの Rack に「Comp」「EQ」が順に並ぶ、追加で「x」の数 = 追加分の数 / EQ の [Par] を click → `HP LF LMF HMF HF LP` が同じ y で x 昇順、全グリフが `[pad, INSPECTOR_W − pad]` 内、次の行 y ≥ 行 y + ROW_H + `panel_height(Eq)` / Comp と EQ の Par を同時に開いて重ならない / master で「Bus Comp」「Tone EQ」「+ FX」「Post-Fader」「Limiter」が y 昇順、Limiter 行に「x」が無く、20px ドラッグしても順序不変 / LMF の点（`curve_handles` と layout 定数で座標を求める）を右 40・上 20 → release → LMF の Freq と Gain が増え、数値欄のグリフが変わり、undo 1 回で両方戻る / EQ を 12 個、Par 3 枚であふれさせ、LMF の点上でホイール → Q が変わりスクロール量不変、点の外ではスクロール / Comp Par の見出しを press → 20px 下 → release で順序不変 / dark・light で、スペクトラムの塗りが最大の背景に対する EQ 点の `contrast_ratio`（theme.rs:37）と、OFF 行の名前と背景の比が閾値以上 |
| R-12 | daw-ui core cfg(test) | `xy_point`: Esc で開始位置へ戻る、press を名乗るので親 drag_list のセッションが次フレームで捨てられる、軸ロック、hover していないホイールは返さない、modal の下では press を取らない / `wheel` + `scroll_area`: 前フレームに claim した矩形の上ではホイールが残り、claim は 1 フレーム遅れで失効する |
| R-13 | `clipboard.rs` cfg(test) | v38 形の `TracksCopy`（strip comp ON + `StripComp` レーン）→ 組み込み Comp が `bypassed=false`、params 保持、レーンがその id を指す |

回すもの: `cargo test -p daw-ui-core --lib -- xy_point wheel scroll_area`、`cargo test -p daw_gui --features daw_gui/script --lib -- clipboard handler::rack_view`、`--test app_state --test native_rack_visual --test chain_split_click --test automation_hover_visual --test mixer_pan_readout_visual --test about_visual --test arr_widget --test dirty_guard_click --test launcher_thumbnail_visual --test loudness_report_visual --test master_panel_visual --test theme_visual`。

### 15.8 統合

1. `make clippy` → `make arch-lint` → `make test-nolaunch` → `make build`
2. 許可を得てから: `DAW01_ALLOW_LAUNCH=1 cargo test -p daw_gui --features daw_gui/script --test glue_bake_parity --test native_chain_smoke --test loudness_analysis_smoke --test device_chain_smoke --test track_duplicate_smoke --test project_tabs_smoke`
3. 実機 sign-off（§20）

video preview / texture には触れないので smoke-test は対象外。

---

## 16. 既存設計書の更新（廃止する決定を明記）

原文は再掲しない。1 行の要約と本書へのリンクに置き換える。

| ファイル:行 | 廃止・変更する決定 | 置き換え | 単位 |
|---|---|---|---|
| docs/plan_channel_strip.md:24-32（§1 信号経路） | **廃止**「固定順で、並べ替えはできない」「挿したプラグインは必ずコンプより前」 | 組み込みはチェーン上の device で並べ替え可（Q4）。挿入位置は Q6 | M |
| 同:34-36 | **廃止** 実行場所 `mixer.rs` の `apply_strip` 直前、状態は `TrackScratch` | `ChainOp::Native`、状態は `ChainProgram.natives`（§8） | M |
| 同:42-43（§2） | **変更**「縦の並び = Comp → EQ 固定」 | Rack の前後に合わせて入れ替わる（Q16）。組み込みだけを出す | M |
| 同:97-100（§3） | **記述の訂正**「ダブルクリック = バイパス」（実装は Q が担う: strip_sections.rs:207-210） | Q / Rack の小表示ダブルクリック（Q15） | M |
| 同:101-105 | **一般化** 自動 ON の規則 | 4 種共通、`NativeEdit::apply` が SSoT（§10.1） | M |
| 同:144-145 / :216-217 | **廃止（Rack Par について）**「カーブ上のノードをドラッグする編集は持たない」 | Rack Par は点操作を持つ（Q13）。Mixer サムネイルは持たない | M |
| 同:93 / :218 | **維持**（Mixer サムネイルにはスペクトラムを重ねない） | Rack Par は重ねる（Q14） | M |
| 同:198-205（§8） | **廃止** `Track.strip: ChannelStrip`。**訂正** 開閉は `UiPrefs` → `ProjectView` | `Device::Native`（§5）。値は `bypassed` が SSoT | M |
| 同:209-212（§9） | **廃止** per-track の GR スロット / 「スペクトラムは送らない」 | device id キーの GR 面、device scope（§11） | M |
| 同:220-222（§10） | **廃止**「EQ / Comp / inserts の順序入れ替え」を非対象から削除。master の EQ/Comp は既に別物 | — | M |
| docs/plan_master_strip.md:30-43（§1） | **廃止**「固定順で、並べ替えもトグルも持たない」 | Bus Comp / Tone EQ は master_fx_chain 上の device で並べ替え可。**Limiter のフェーダー後固定は維持**（Q3） | M |
| 同:47-60（§2） | **変更**「リミッター ON のときだけ遅延」 | 遅延の有無は `master_limiter_latency_active()`（静的 on または On レーン / 変調）で compile 時に決まる。OFF 解決中は遅延だけ通す | M |
| 同:115 / :194 | **廃止**「外部サイドチェーンを持たない」 | Bus Comp も SC▾ を持つ（Q19）。検出フィルタは持たないまま | M |
| 同:161-173（§6） | **廃止** `Song.master_strip` / `SetMasterStrip` / GR 2 本 / `MasterStripState` | `master_limiter` + native device / `SetNativeDevice` / `SetMasterLimiter` / device 面 / `MasterLimiterState` | M |
| 同:175-186（§7） | **根拠が消滅**（「devices を分類できないので通常トラックは内蔵が後」はユーザーが位置を決める形で解消） | 既定の位置は Q6 | M |
| 同:188-197（§8） | 「順序の入れ替え」「外部サイドチェーン」を非対象から削除。「SC Listen はモニターセクションへ移すのが筋」は `NativeIo` で聴き方として Song から出したので実質的に解消 | — | M |
| docs/plan_parallel.md:29 | **廃止**「幅 280px 不変」 | 360px 固定（Q1/Q2）。組み込みは Parallel に入れない（Q5） | R |
| docs/plan_master_meters.md:177-178 | 図の「280 固定」 | 360 固定 | R |
| docs/plan_project_tabs.md:171 | `ProjectRt.master_strip` | `master_limiter`、`ProjectShared` に `sc_listen_device` / `device_scope_watch` | E |
| DESIGN.md:121 | `mixer.rs (strip 適用)` | `native_dsp/` と `graph/native.rs`（チェーン op） | E |
| ui/CLAUDE.md | 追記 | `claim_wheel_in_rect` の 1 フレーム遅れの罠、`xy_point_at` の作法（claim_press / Esc / wheel_active） | R |
| CLAUDE.md:208（不変条件 6） | 追記（**ユーザー承認後**） | 「聴き方・見方の状態（SC Listen / device scope）は `NativeIo` 引数で渡し、export は既定値」（arch-lint の無い不変条件なので本文が唯一の強制手段） | 統合 |
| docs/plan_rack_native_devices.md（新） | 本書 | — | S0 |

---

## 17. サイズ budget と分割（足す前に割る）

実測は `scripts/loc_budget.py`（ncloc）、天井は `scripts/arch_lint_baseline.txt`。

| ファイル / 関数 | 現在（天井） | 対処 | 分割後の見込み | 単位 |
|---|---|---|---|---|
| common/src/model.rs | 1,694（1,810 :120） | `sections.rs` / `section_ops.rs` / `load_normalize.rs` / `plugin_instance.rs` / `source_pools.rs` へ（wire でないロジックは wire ファイルに置かない: build.rs:1-11）。`ensure_ids` 187 行は `patch_remapped_track_refs` の切り出しで約 140 | 約 740。baseline :120 を削除 | S1a |
| daw_audio/src/graph/compile.rs::compile_schedule | 328（362 :167）、nest 7/20（:295） | `compile/{mod,deps,emit,pdc,sidechain,tests}.rs` へ | 組み立てのみ約 90。:167 / :295 を削除 | S1b |
| compile.rs::compute_path_latency | nest 7/12（:296） | SC ループを `sidechain::sidechain_input_latency` へ | 6 未満 | S1b / E |
| app.rs::AppData::handle_event | 1,603（1,605 :156） | デバイス系 arm を `handler/device_event.rs` へ、Strip の arm を削除 | 約 1,550。天井を下げる | S1c |
| event.rs | 990 | デバイス系 variant とラベルを `event_device.rs` へ | 約 850 | S1c |
| app_types.rs | 1,219（1,219 :114） | :217-361 を `chain_rows.rs` へ | 約 1,115 | S1c |
| view/track_inspector/chain_list.rs | 956 | `plugin_row.rs` / `chain_row.rs` / `row_menu.rs`（R で `native_row.rs`） | 約 420 | S1c |
| view/track_inspector/mod.rs / draw | 1,002（2,214 :91）/ 867（2,063 :153、nest 8/119 :204） | `audio_event_section.rs` / `image_event_section.rs` / `mouth_map_section.rs` へ `(app, ui, area, pad, y) -> f32` で移し、`let … else { return y }` で 1 段浅くする | 約 200 / 約 80 | S1c |
| view/root.rs / dispatch_shortcuts | 957 / 521（530 :159、nest 7/17 :226） | Q の節を `view/bypass_toggle.rs` へ | 約 830 | S1c |
| handler/automation_lanes.rs | 1,185（1,187 :107） | :1138-1452 を `handler/param_value.rs` へ。`lane_default_for_target`（nest 7/36 :249）は Image / Text を `image_field_value` / `text_field_value` に分けて 6 段以下 | 約 930。:107 / :249 を削除 | S1c / A |
| handler/project.rs | 1,074（1,188 :105） | `maybe_autosave`（:808）〜`remove_recovery_files_of`（:1025-1055）を `handler/recovery.rs` へ | 約 875 | S1c |
| handler/tick.rs::current_plain_value / ensure_recording_lane_clip | nest 7/12（:259）/ 8/64（:239） | 削除 / 分岐 2 本を `push_lane` 1 本に | 解消 / 減 | A |
| ui/crates/ui/src/ui.rs | 1,611（1,625） | ロジックは wheel.rs、ui.rs は +5 行 | 約 1,616 | R |
| view/runner.rs | 1,457（1,618） | +1 行（`sync_device_scopes`）。sweep は scrub_gesture::sweep の中から呼ぶ | 1,458 | R |
| view/arrangement_view.rs / draw | 996 / 573（577） | ヘッダ音量 edge 18 行 → 3 行、ドロップの index 算出 5 行 → 1 行 | 約 975 | F / S1c |
| view/plugin_picker.rs::draw | nest 7/34 | タグ色を関数外へ | 7 / 約 30 | R |
| engine.rs::ProjectRt::render_buffer | 255、nest 7/22（:294） | `native_io_for_buffer` へ切り出し、GR publish を集約 | ±0、nest 不変 | E |
| daw_audio/src/main.rs::build_stream | nest 8/32（:280） | open 1 行、引数 1 個 | nest 不変 | E |
| handler/ipc.rs::dispatch_plugin_event | nest 9/25（:224） | 名前解決 3 行を削除 | 減 | F |
| view/mixer_strips.rs::draw_strip | 224、nest 7/6（:212） | `was_*` 引数と `drag_flags` を削除 | 締まる | M |
| handler/automation.rs | 1,396（1,410 :103） | :293-310 を 1 行に | 減 | F |
| handler/mixer.rs / strip_sections.rs / master_strip_ui.rs | 653 / 539 / 322 | 約 −150 / −220 / −120 | — | F / M |
| handler/tracks.rs / devices.rs / device_relocate.rs / parallel.rs | 955 / 723 / 706 / 616 | 約 ±20 / 約 −70（device_relocate） | budget 内 | F / R |
| view_model.rs / view_build.rs / scrubable_number.rs / launcher/runtime.rs | 764 / 787 / 777 / 977 | +35 / −21 / +25 / 減 | budget 内 | F / A |
| common/src/automation.rs / daw_audio/src/automation.rs / audio_bridge.rs / program.rs / execute.rs / audio_worker.rs | 508 / 434 / 393 / 659 / 800 / 744 | 約 −40 / ±0 / +50 / +8 / −35 / +20 | budget 内 | F / E |
| 新規 `view/native_device/*` / `native_panel/*` / `handler/native_edit.rs` / `event_native.rs` | — | 合計約 470 / 各 300 未満 / 約 150 / 約 90 | — | C / R / F |

- **baseline の編集は統合時の 1 回だけ**。解消した行は「削除してよい」と案内が出るだけで exit を落とさない（scripts/arch_lint.sh:416-421, :657-660）。
- 解消が見込まれる行: :91, :153, :204（track_inspector/mod.rs）/ :105（handler/project.rs）/ :107, :249（automation_lanes）/ :120（model.rs）/ :159, :226（dispatch_shortcuts）/ :167, :295, :296（compile）/ handle_event の天井の引き下げ。

---

## 18. 既存の穴

### 18.1 この設計で必然的に塞がるもの

| 記号 | 穴（file:line） | 塞ぎ方 | critic 2-d |
|---|---|---|---|
| A | group / return で内蔵ストリップと volume / pan の変調が効かない（execute.rs:863-864, :903-904）。GUI からは routing を作れてしまう | §8.4.3（**既存曲で該当 routing があると音が変わる**。sign-off） | 2 |
| B | RT でヒープ確保: `track_needs_*_snapshot` → `any_tap_at` → `all_plugins` → `plugins()` の `vec!`（execute.rs:324, :406, :817 → mix.rs:79 → device.rs:532） | compile 時に焼いたフラグ（§8.3.2） | — |
| C | group / return の PostFx tap が pre-fader snapshot を要求しない（leaf :404-407 にある条件が group :869-877 に無い） | `snapshot_post_fx` に揃える | — |
| D | 内蔵ストリップの DSP 状態がトラック index に残る（`TrackScratch.strip`、engine.rs:469-477） | 撤去、device_id で引き継ぎ | 5 |
| E | 再 ON のとき古い biquad 状態から再開する（本番コードに reset の呼び出しが無い） | OFF → ON で `reset`（§8.4.1） | — |
| F | SC Listen の音が Song / bincode に載って書き出しにも届きうる（channel_strip.rs:420-425）。切り替えで `*` が立ち undo に積まれる（handler/mixer.rs:408、event.rs:2155） | `NativeIo` と `ProjectEphemeral` に分離（§8.7 / §10.14） | 3 |
| G | Limiter の ON をオートメーションすると遅延と PDC の会計が食い違う（master_strip.rs:184-191 と compile.rs:838-842） | compile 時に焼く（§8.3.4） | — |
| H | Comp の検出が NaN を受けると `gain_db` が NaN のまま戻らない（channel_strip_dsp.rs:319-321） | §8.8 | — |
| I | LoadSong の経路で strip / master_strip の値域チェックが無い（model.rs:1379-1427） | `sanitize_ranges` で native / limiter を丸める | 4 |
| J | Bounce In Place / Glue で内蔵ストリップが中和されない（bounce.rs:133-170） | §10.15 | 1 |
| K | コメントの食い違い（execute.rs:1151-1152、mixer/master_strip.rs:170-171、execute.rs:949-957, :4-10） | §8.4.4 | — |
| L | bus 宛ての SC で main 側に遅延が掛からない（execute.rs:534）のに path latency には SC を含める（compile.rs:1023-1065, :1092）。GWI prefix 宛て tap の lag を 0 と数える（:748, :1022）。Q19 で全 group / return の組み込み Comp が踏む | consumer 単位の pass と `BusScAlign`（§8.3.3） | — |
| M | 同じタブで別ファイルを開くと master の DSP 状態が持ち越される（engine.rs:272、:482-511 に reset が無い） | `master_limiter.reset()`（§8.5） | — |
| N | `norm_mapping_is_affine` の `_ => true` に ParallelSplitFreq（Log）が落ち、曲線が直線で描かれる（common/src/automation.rs:248、curve.rs:92-100） | `target_range`（§7.1） | 6 |
| O | Parallel / chain を削除・解除しても ChainGain / ParallelOutGain 系のレーンが残る（devices.rs:979-1007 は PluginParam だけ、parallel.rs:79-98 は掃除しない） | SongDoc の `prune_dangling_param_targets`（§5.8） | 7 |
| P | param パネルの実測高が 1 以下だと毎フレーム 280px で展開（chain_list.rs:94-98） | `rack_panel_heights`（`Some(0.0)` を保持） | 8 |
| Q | マスターパネルのノブが gesture edge を出さず、変調も作れない（master_strip_ui.rs:404） | `native_knob`（§10.9） | 9 |
| R | 同じパラメーターを 2 面に描くとジェスチャーが毎フレーム往復する。アレンジのヘッダ音量 × Mixer フェーダーで発生済み（arrangement_view.rs:236-256、mixer_strips.rs:806-813）。plugin 窓 × Rack param パネルも同じ | 面つき所有者（§7.6） | — |
| S | 録音時の現在値が `_ => None` で、Mute / SendGain / Chain* / Parallel* / Strip* / Mod* / GroupTransform が Touch/Latch/Write で録音されない（tick.rs:263-266, :615） | `target_plain_value`（§7.5） | — |
| T | master の Chain* / Parallel* で A を押すと既定値 0 のレーンができて無音になる（automation_lanes.rs:1254-1256） | 同上 | — |
| U | master chain のノブが再生中のレーン値を出さない（`live_param_value(&Track)`: chain_list.rs:635-636 ほか） | owner id 版（§7.5） | — |
| V | 変調 routing が古い track に積まれる（view/modulation.rs:102-111、handler/modulation.rs:480-526） | `bound_owner_track`（§7.7） | — |
| W | 段階式の表示が面ごとに違う（レーン見出しは `1`、マスターパネルは `4:1`: master_strip_ui.rs:414-431） | `Choices`（§7.4） | — |
| X | MIDI Learn が native の param を armed track の Volume に誤って bind する（handler/midi.rs:137-162） | §7.9 | — |
| Y | 旧ビルドでコピーしたトラックを貼ると strip の設定が消える / decode 失敗（clipboard.rs:306-325, :358-364） | §6.6 | — |
| Z | SC / send の配線で循環を選べ、選ぶと master が無音になる（parallel.rs:550-580、mixer.rs:583-605 → compile.rs:364 → project_ctl.rs:316-329） | §5.9 / §10.13（**plugin の候補からも消える**。sign-off） | — |
| AA | 同じ種類の Par を 2 枚開くと片方のドラッグがもう片方の bracket を閉じる（track_inspector/mod.rs:79-84） | widget id と bracket の鍵に device_id | — |
| AB | マスターパネルの hover がパネルを閉じても残る（master_panel.rs:99-106, :376-378, :411-414） | publish を 1 か所に（§10.9） | — |

critic 2-d の 9 件（Bounce / group の変調 / Listen の dirty / LoadSong の値域 / TrackScratch.strip / affine 誤判定 / chain 削除後のレーン / param パネルの 280 / マスターパネルの gesture）は、**すべて上の表で塞がる**。

### 18.2 本件外（列挙のみ、直さない）

1. トラックや chain を消しても、aux route の `TapSource::{Track, Chain}` と follower の tap が dangling のまま残る（handler/tracks.rs:48-140 と devices.rs に掃除が無い）。
2. `pre_fx` の Bounce In Place で Parallel の split / gain / pan が焼き込まれる（bounce.rs:163-177）。
3. Text の px 系は正規化が 0..1 の恒等（common/src/automation.rs:126）なのに、表示レンジは px（automation_value.rs:356-373）で食い違っている。
4. `state/project.rs:295-299` の `arrange_header_w` のコメント「session-only」が古い（実際は保存される: handler/view_state.rs:94, :167）。

---

## 19. 並列実装の作業単位と統合順

### 19.1 前提

- **統合先**は統合ブランチ `r129-native`。main へは全単位の統合・gates・実機 sign-off の後に 1 回だけ入れる（手順は `reference_worktree_merge_to_main`）。
  - F から E が入るまでの間、組み込み DSP は素通しの無音変化になる。これは統合ブランチ上だけの中間状態。
- **排他の範囲**: 「同じファイルを複数の単位で編集しない」は、**同時に走る単位どうし**で排他にする。S0 / S1 / F は直列の前段なので、F が型追従で触ったファイルを後続単位が編集するのは直列の編集にあたる（§19.3）。
- worktree を作った直後に `make fetch-ffmpeg` と子 exe のビルドを行う。
- 各単位の検証は `cargo check` と関連テストの名指し実行だけ。`make clippy` / `arch-lint` / `test-nolaunch` は統合時に 1 回。
- 長く走る agent の前に WIP commit する。巨大ファイルを agent に並行して分割編集させない。

### 19.2 単位表

| 単位 | 内容 | 前提 | 統合順 | 回すもの |
|---|---|---|---|---|
| **S0** 記録 | 本書、v38 fixture、DSP golden（§15.1） | HEAD 76837672 | 1 | `cargo test -p common --lib -- --ignored record_v38_fixture`、`cargo test -p daw_audio --lib -- --ignored record_golden`（生成物を commit） |
| **S1a** model 分割 | 挙動を変えない（§17） | S0 | 2（S1b / S1c と順不同） | `cargo check -p common -p daw_audio -p daw_plugin_host --all-targets`、`cargo check -p daw_gui --all-targets --features daw_gui/script`、`cargo test -p common --lib` |
| **S1b** compile 分割 | 挙動を変えない | S0 | 2 | `cargo check -p daw_audio --all-targets`、`cargo test -p daw_audio --lib -- graph::compile` |
| **S1c** daw_gui 機械分割 | `DeviceEvent` への移設（新 variant なし）、`InsertAt`（Index のみ）、`chain_rows.rs`、chain_list / track_inspector/mod.rs / root.rs の Q 節 → `view/bypass_toggle.rs`、`handler/project.rs` → `recovery.rs`、automation_lanes.rs:1138-1452 → `param_value.rs`、`row_menu` の型化 | S0 | 2 | `cargo check -p daw_gui --all-targets --features daw_gui/script`、`cargo test -p daw_gui --features daw_gui/script --test app_state --test chain_split_click` |
| **F** 基盤 | common 全部、daw_gui のハブ（event / state / 編集・Listen・Q・ガード述語の単一の口 / ジェスチャー所有者 / live 値 / telemetry の受け口 / tabs / bounce / 挿入位置 3 か所 / クリップボードの sanitize）、全 crate の型追従 | S1a, S1b, S1c | 3 | §15.2 |
| **E** engine | ChainOp::Native、native_dsp、SC 会計、Listen の置換、Limiter の遅延焼き込み、GR / scope の publish、group の変調、routing_deps の利用 | F | 4（A / C と順不同） | §15.3 |
| **A** オートメーション GUI | `target_plain_value` の完成と録音、A キーの owner、`device_param_name`、変調 routing の owner、MIDI Learn、`Choices` | F | 4 | §15.4 |
| **C** 共有描画部品 | `view/native_device/{mod,knob,curve,gr}.rs` | F | 4（最初に統合） | §15.5 |
| **M** Mixer 帯 / マスターパネル | §10.8 / §10.9 / §10.19、docs の channel_strip / master_strip | F, C | 5（R と順不同） | §15.6 |
| **R** Rack | §10.5–§10.7 / §10.10 / §10.11 / §10.13 / §10.16、picker、device scope の送受と FFT、クリップボード旧形式、束縛の運搬の一般化、テーマのコントラスト、docs の parallel / master_meters、ui/CLAUDE.md | F, C | 5 | §15.7 |
| **統合** | baseline の整理、gates、起動テスト、sign-off、CLAUDE.md（承認後）、memory | 全単位 | 6 | §15.8 |

### 19.3 同時に走る単位のファイル排他

| 単位 | 触るファイル |
|---|---|
| S1a | common/src/model.rs、model/{sections,section_ops,load_normalize,plugin_instance,source_pools}.rs（新）、model/ids.rs、common/build.rs |
| S1b | daw_audio/src/graph/compile.rs → graph/compile/* |
| S1c | §13.3 の S1c 行、tests/app_state/{device_relocate,group_track_lifecycle,modulation_id_hygiene,parallel,pending_state_queue,plugin_load_failure}.rs、tests/chain_split_click.rs |
| E | daw_audio/src/** 全部、docs/plan_project_tabs.md、DESIGN.md |
| A | daw_gui/src/{automation_label,automation_value}.rs、widgets/arrangement/view_build.rs、handler/{tick,param_value,modulation,midi,view_model}.rs、view/transport.rs、ui/crates/ui/src/widgets/scrubable_number.rs |
| C | daw_gui/src/view/native_device/*（新）、daw_gui/src/view/mod.rs |
| M | daw_gui/src/view/{strip_sections,master_strip_ui,master_panel,mixer_strips}.rs、handler/master_panel.rs、script.rs、tests/{mixer_native_band_visual,master_native_blocks_visual,param_gesture_surfaces,native_chain_smoke}.rs（新）、tests/master_panel_visual.rs、tests/scripts/{glue_bake_parity,native_chain_smoke}.js、docs/plan_{channel_strip,master_strip}.md |
| R | daw_gui/src/view/track_inspector/**、chain_rows.rs、app_types.rs、view/{plugin_picker,root,runner,clipboard_ops}.rs、handler/{device_guard,rack_view}.rs（新）、handler/{devices,device_relocate,parallel,tracks,mixer,view_state,automation_lanes,mod}.rs、clipboard.rs、main.rs、bootstrap.rs、ui/crates/ui/src/{widgets/xy_point.rs,wheel.rs,ui.rs,widgets/scroll_area.rs,widgets/mod.rs,lib.rs,widgets/reorderable_list.rs}、ui/CLAUDE.md、docs/plan_{parallel,master_meters}.md、tests/app_state/{main,native_rack}.rs、tests/{native_rack_visual,chain_split_click,automation_hover_visual,mixer_pan_readout_visual,theme_visual}.rs |

- 並列の組は S1a / S1b / S1c（3）と E / A / C（3）。R と M は C の統合後に分岐する（E / A と時間的に重なってよい）。
- R と M の間は重なりが無い。`mixer_pan_readout_visual.rs` は R が編集して M は回すだけ、`master_panel_visual.rs` は M が編集して R は回すだけ。
- A と R は `handler/modulation.rs` / `automation_lanes.rs` / `view_model.rs` で重ならないように分けてある。modulation と view_model は A、automation_lanes は R（S1c で param_value.rs へ移した後に残る部分だけ）。

### 19.4 F が触る範囲（後続の単位はこれを基点にする）

| crate | F が入れる範囲（機能ロジックは後続単位） | 後で編集する単位 |
|---|---|---|
| common | §13.1 の F 行すべて、`scripts/arch_lint.sh` の WIRE-SOURCES 検査 | — |
| daw_audio | strip 呼び出しの削除、Native の arm（遅延 0・op なし）、`master_limiter` への置換、`SetNativeDevice` / `SetMasterLimiter` の適用（最終形）、`SetScListen` / `SetDeviceScopes` の受理だけ、telemetry API への最小追従、`param_stores` への置換 | E |
| daw_gui event / state | `event.rs` / `event_device.rs` / `event_native.rs`（新）/ `app.rs` の新 variant と新フィールド、`state/{project,recording,ui_ephemeral,transport}.rs` の全フィールド | — |
| daw_gui handler（最終形） | `native_edit.rs` / `sc_listen.rs` / `bypass_target.rs`（新）、`param_gesture.rs`、`tabs.rs`、`bounce.rs`、`project.rs`、`activity.rs`、`notes.rs` | — |
| daw_gui view（最終形） | `param_gesture.rs`、`scrub_gesture.rs`、`modulation.rs`、`bypass_toggle.rs`、`arrangement_view.rs`（ヘッダ音量） | — |
| daw_gui handler（型追従） | `view_model.rs`（`LiveParamScope` / `live_*` / `param_stores`）、`tick.rs` / `modulation.rs` / `automation.rs` / `select_all.rs` / `ipc.rs` / `automation_lanes.rs` / `device_relocate.rs` / `devices.rs` / `parallel.rs` / `tracks.rs` / `mixer.rs`（store 分岐 18 か所、prune 呼び出しの削除、`prune_device_session_refs`、`InsertAt::Default` の解決、strip 編集の削除、peaks の受け口、aux の slot 化、`set_devices_bypassed` の last_touched） | A（tick / modulation / view_model）、R（その他） |
| daw_gui 表示 | `automation_label.rs` / `automation_value.rs` / `view_build.rs` の網羅 arm（`Choices` の arm だけ A） | A |
| daw_gui view（機械追従） | `mixer_strips` / `strip_sections` / `master_strip_ui` / `master_panel` / `transport` / track_inspector 配下の面引数の追加、`was_*` の削除、strip 型 → 組み込み native の読み出し（描画は既存のまま） | M / R / A |
| daw_gui その他 | `main.rs`（meters の読み出し）、`app_types.rs`（`gain_reduction_db` の削除）、`clipboard.rs`（native の sanitize）、tests（§14.16 の 45 行、`app_state/native_edit.rs`、`channel_strip_visual.rs` の型置換、`text_compose.rs:381`） | R / M |

---

## 20. sign-off が要る判断と骨格からの逸脱

**要件から導いた判断（実機でユーザーの sign-off を得る）**

1. **SC Listen の置換範囲**: その Comp を含むトラックの**チェーン出力**を検出信号で置き換える（engine 節の「その Comp の出力だけ」は採らない）。現行の意図「後段を通さずに素で聴く」を、並べ替えのできるチェーンに一般化したもの。
2. **bypass 中に Listen を押す**: 現行どおりそのデバイスを有効化し、undo に 1 step 積む。Listen 自体は undo / dirty に載らない。
3. **Limiter の On をオートメーション可能にした**。On のレーンや変調がある間は、OFF の区間でも 5ms の遅延を通す。
4. **配線候補の変化**: plugin の SC と send でも、循環になる候補が一覧から消え、その編集は拒否される（現行では選べて、選ぶと master が無音になる）。
5. **group / return の変化**: 組み込みと volume / pan に変調が効くようになる。既存の曲で該当する routing があると音が変わる（今まで効かなかったのはバグ）。
6. **MIDI Learn** を native の param と Limiter に広げた（要件に明記は無い。「plugin と同じ正式な device」から導いた）。
7. **番号**: トラックを跨いで移動したとき、空いていれば番号を保つ。
8. **見せ方**:
   - 組み込み行は × の列を空けて、ボタン列を plugin 行と揃える。
   - picker から足すと、Shift なしなら Par を自動で開く。
   - master の Limiter の上に「Post-Fader」の区切り線を置く。
   - ホイールで Q を変えるときは 400ms 窓で 1 step。
   - SC / Par パネルの余白の press を横取りする（既存の plugin / 映像 FX パネルも、余白の縦ドラッグで行が動かなくなる）。
   - OFF の EQ カーブは形を保って薄く描く。
   - Mixer 帯とマスターパネルのノブも、再生中はオートメーション値に追従する。
9. **bypass の切り替えに 5ms の crossfade** を入れ、OFF → ON では DSP を無音状態から再開する。
10. **EQ の「触ったバンドを ON」を全バンドに広げた**。LF〜HF に OFF にする UI は無いので、旧ファイルでこれらが OFF になっている場合の唯一の復帰経路になる。

**骨格からの逸脱（理由付き）**

| 骨格 | 本書 | 理由 |
|---|---|---|
| `ChainOp::Native { device_id, state }` | `{ device_id, native_slot }`。SC の受け方は `NativeScratch::sc_mode` | `ChainOp` は値型。Staged にするかは emit 時にしか決まらない（§8.2） |
| `aux_input` の追加 IPC | なし（値 IPC に載せず、LoadSong で構造として届く） | 配線は依存辺と PDC を変える構造の変更 |
| 値だけ更新する IPC | `SetNativeDevice` は track を持たない | device id は Song 全体で一意 |
| `AutomationTarget::MasterLimiter(MasterLimiterParam)` | `MasterLimiterParam { On, Ceiling }` | 遅延を compile 時に焼くので On の住所を置ける（K2） |
| `common::channel_strip_dsp` | `common::dsp` | Parallel の帯域分割とも共用する |
| Par の開閉 = device id の集合 | `RackPanelKey { Device(u64), MasterLimiter }` の集合 | master Limiter の Par は device ではない |
| （骨格に無し） | `NodeOp::NativeSidechainTap`、`DelayKey::BusScAlign`、`routing_deps.

rs`、`event_native.rs`、`Song::enforce_edit_invariants` | 既存の穴（§18 L / Z / O）と、自動 ON の SSoT を 1 か所に置くために必要 |

---

## 21. landing 後の記録（メインセッションが行う。r.md は編集しない）

memory `project_rack_native_devices.md` に次を書き、MEMORY.md に 1 行を足す。

- 組み込みは `Device::Native`。ON/OFF の SSoT は `bypassed`、オートメーションの住所は `NativeParamId::On(kind)`。
- Listen は GUI の `ProjectEphemeral` が SSoT で、engine は自分から解除しない。置換はトラックのチェーン出力で行い、export は `NativeIo::default()`。
- ジェスチャーの所有者は面つき map（`HashMap<(u32, AutomationTarget), ParamSurface>`）。1 面・1 param につき push はフレームに 1 回で、dragging は OR する。
- Limiter の遅延は「静的 on または On レーン / 変調」で compile 時に焼く。
- 配線の候補は Structural な循環を除外する（`routing_deps::TrackDeps`）。
- 編集後の不変条件（組み込みの正規化と dangling の prune）は SongDoc の 5 つの口で `enforce_edit_invariants` が担う。handler に prune を書かない。

---

## 付録: 文書内参照の表記

- `§18-A` のような表記は §18.1 表の同じ記号の行を指す。
- §11.2 は device scope の shmem（スペクトラム）を指す。