# 1 buffer の処理を依存グラフで並列に流す (bus のプラグインを 1 スレッドに詰まらせない)

## 0. 現状と根

`render_master_buffer` は 2 段:

- **pass 1** — 全 track の `process_track_owned` (leaf のチェーン / group-with-instrument の prefix) を
  worker pool が claim-by-index で並列に回す (`audio_worker.rs::run_work_loop`)。
- **pass 2** — `execute_schedule_post_dispatch` が `Schedule::nodes` を **callback スレッド 1 本で直列に**
  歩く。group / return / パラアウト先 (bus) のチェーン (`ProcessGroupFx` → `run_group_fx_chain`) も、
  合流 (`Mix` / `MixSend` / `ParallelOutTap`)・PDC (`ApplyDelay`)・サイドチェインの staging・master の合流も
  ここ。bus のプラグインは常に sync slot 0 で dispatch される。

**根:** 依存関係が無い bus 同士 (別々の group、別々の return) も 1 本のスレッドで順番に処理しているうえ、
bus は **pass 1 の全 track が終わるまで始められない** (重い leaf 1 本が、無関係な group の開始まで遅らせる)。
return にリバーブ、group にバスコンプを積むほど callback スレッドが詰まる。

## 1. 方針

1 buffer の仕事を **依存グラフの job** として compile 時に組み、Ardour の `Graph` と同型の
ready-queue 実行器で **pass 1 と pass 2 を 1 本のグラフとして** 流す。

参照実装: Ardour `libs/ardour/graph.cc` — 完了した node が後続の refcount を減らし、0 になった node を
trigger queue に積む (`Graph::trigger`)。`Graph::run_one` は queue から 1 つ取り、残りの仕事の数だけ
idle thread を `_execution_sem.signal()` で起こす (`wakeup = min(idle_cnt + 1, work_avail)`)。仕事が無い
thread は `_execution_sem.wait()` で **寝る (spin しない)**。terminal の refcount が 0 になったら
`_callback_done_sem.signal()` で process cycle が抜ける。

**音は今の直列と bit 一致する。** 基準の順序 (直列トレース) を

```
T = [ Process(0), Process(1), …, Process(n-1) ]  ++  [ nodes のうち ProcessTrack 以外 (今の pass 2 の順) ]
```

と定め、T の上で **同じ資源を読み書きする op の相対順だけ** を辺にする。辺を守る任意の順序で実行した結果は
T の順で実行した結果と一致する (浮動小数の演算は op 内で閉じ、非衝突 op 同士は互いの入力を変えない)。

## 2. compile (off-thread、`compile_schedule` の末尾)

### 2.1 資源と op の読み書き

| 資源 | 意味 |
|---|---|
| `Scratch(i)` | track i の `TrackScratch` (出力 / pre-fader / pre-FX / MIDI バス / ランプ) |
| `Program(i)` / `Program(master)` | その program (device の走行状態、chain / Parallel の snapshot、内蔵 device の SC 受け皿、**所有する plugin の ProcessData shmem**) |
| `Master` | master バス |

device id → 持ち主の program は compile 時に引ける (plugin の shmem は持ち主 program の資源として数える)。
`delay_lines[k]` / `follower_slots[k]` は 1 op 専用なので資源にしない。

| op | 読む | 書く |
|---|---|---|
| `Process(i)` (pass 1) | — | `Scratch(i)` `Program(i)` |
| `Mix{dst: TrackScratch(t)}` / `MixAdditive` | srcs の `Scratch` | `Scratch(t)` |
| `Mix{dst: Master}` | srcs の `Scratch` | `Master` |
| `ProcessGroupFx(i)` | — | `Scratch(i)` `Program(i)` |
| `ApplyDelay(TrackScratch(i))` | — | `Scratch(i)` |
| `SidechainTap{src, device}` | src (Scratch か Program) | `Program(owner(device))` |
| `NativeSidechainTap{src, owner}` | src | `Program(owner)` |
| `ParallelOutTap{device, dst}` | `Program(owner(device))` | `Scratch(dst)` |
| `MixSend{src, dst}` | `Scratch(src)` | `Scratch(dst)` |
| `EnvelopeFollow{src}` | src | — |

Song / tempo / 変調面 / 行の供給元 / 録音中レーン / solo の表 (`Schedule::solo`) は buffer の間不変
(共有読み取り)。solo の表を program の中に置くと、program を書く手 (サイドチェインの staging) と表を読む手
(`MixSend`) が同じ資源を奪い合い、依存の無い bus 同士が直列化される (実際に起きたのでテストで守る) — しかも
書き手が `&mut` を持つ program の一部を別スレッドから `&` で読むのはエイリアシング違反になる。

### 2.2 辺

T を先頭から走査し、資源ごとに `last_writer` と「その後の reader 集合」を持つ:

- op が資源を **読む** → `last_writer → op`
- op が資源を **書く** → `last_writer → op` と `reader 集合 → op`、以後 `last_writer = op`、reader 集合を空に

### 2.3 job への縮約

「前駆がちょうど 1 つで、その前駆の後続もちょうど 1 つ」の op を前駆の job に畳む
(例: `Mix(G) → ApplyDelay(BusScAlign G) → ProcessGroupFx(G)` は 1 job)。細かい op ごとに queue を
往復させない。

### 2.4 出力 (`Schedule::graph`)

- `jobs[j] = { ops: 範囲 (job_ops の), succ: 範囲 (succs の), preds: 前駆数 }`、`roots` (前駆 0)
- RT の作業領域も compile 時に確保: `pending[j]: AtomicU32`、ready queue (容量 = job 数)

## 3. RT 実行器 (`audio_worker.rs`)

- 1 buffer の文脈は 1 本の構造体 (`graph::step::RenderCtx`、dispatch 窓の間だけ callback スレッドのスタックに
  生きる) のポインタで worker へ渡す。worker は `inside` を立ててから読み、callback スレッドはグラフが終わったら
  ポインタを消して `inside` が 0 になるまで待つ (Dekker 型: 立てる → 読む / 消す → 数える)。project ごとの
  schedule が差し替わっても、戻った後に worker が触ることはない。
- 取り方: 最初から走れる job (`roots`、track 本体の大半) は数も並びも compile 時に決まっているので queue に積まず
  `fetch_add` で取らせる (取り合いで失敗しない)。途中で走れるようになった job だけ ready queue (容量 = job 数の
  配列 + `tail` 予約 + `head` CAS、未書き込みの席は番兵) に積む。job を終えた runner は、待ちが無くなった後続の
  1 つを自分で続けて実行し、残りを積んで寝ている runner をその数だけ起こす。
- **master バスへ書く job (合流) は callback スレッドだけが実行する** (専用の待ち行列)。master バスはグラフの後で
  callback スレッドが master の段で読むので、そこで合流すれば値が冷えない。最後の track を終えた worker に続けて
  合流させると、曲の出力全部を読む重い直列の手が冷えた core に回り、手そのものが遅くなる (§5)。
- 終わりの判定は **後続の無い job の残り数** (全 job はそのどれかの祖先)。job ごとに全 runner が書く数を作らない。
- 寝起き: runner ごとの event と、**寝ている runner の bit 列** (`idle`)。
  - 仕事が無くなった runner は自分の bit を立ててから queue を見直し、何も無ければ自分の event で寝る (spin しない)。
    見直しで仕事を見つけたら bit を取り戻す (起こす側に先に下ろされていたら、その signal を消費してから続ける)。
  - 起こす側は job を積んで (buffer の頭では文脈を置いて) から bit を読み、**自分で下ろせた bit** の runner の event
    だけを signal する。起こす数と寝ている runner が 1 対 1 に対応するので、取りこぼしも余分な起こしも無い
    (semaphore で「寝ている数」を数える方式は、自分宛ての event で起きた runner の分の permit が残り、次の buffer で
    空振りの起床を生んだ)。1 回の `ReleaseSemaphore` で全員を起こすと kernel が全員を ready にし終えるまで起こす側が
    止まる (8 本で 6.5 µs 実測) ので、起こすのは 1 本ずつ。
  - 前の buffer の worker は `inside` を持ったまま bit を立ててから抜ける (callback スレッドは `inside` が 0 になって
    から戻る) ので、buffer の頭で寝ている worker は全部 bit を立て終えている。
  - callback スレッドは buffer 周期の 1/50 まで回って待ち (グラフが終わるまで他にすることが無い。起床遅延 ~5 µs を
    buffer ごとに払わない)、それを過ぎたら自分の bit を立てて event で bounded に寝る (合流の job を積んだ worker と
    グラフを終えた worker が起こす)。
- 手の実行は `graph::step::run_step` **1 本** (旧 pass 1 / pass 2 / 直列 fallback が共有)。
- plugin の dispatch は runner 自身の `SyncSlot` (callback スレッド = 0、worker i = i + 1) — 旧 pass 1 と同じ契約。
  bus の plugin も任意の slot で走る (旧 pass 1 の leaf が既にそうしている)。
- pool が無い (起動直後 / 書き出しで rig 無し) ときは T の順に `run_step` を直列に回す。
- stall: **どの runner も `POOL_WAIT_TIMEOUT_MS` の間 1 手も終えられなかった** ときだけ pool を stalled にし、
  この buffer の全 scratch と master を 0 にする (「stalled = 無音」の契約)。runner ごとに終えた手を数え
  (cache line を分けて自分だけが書く)、callback スレッドは寝て起きるたびに合計の進みを見る。1 回の待ちの時刻で
  打ち切ると、別々の slot の bounded な dispatch (各 `DISPATCH_TIMEOUT_MS`) が依存の鎖に沿って順に積み重なる
  だけの **生きた worker** を置いて戻り、スタック上の文脈を読ませてしまう (旧 pass 2 は callback スレッド上で
  直列だったので起きなかった)。

RT 規約: 確保・解放・ロックなし (作業領域は `Schedule` と一緒に届く)。待ちは同一プロセスの event と bounded な
terminal 待ちだけ (不変条件 4)。

## 4. live と export

どちらも `render_master_buffer` → 同じ実行器 (不変条件 6)。master の fx chain / SC Listen / master 音量 /
Limiter はグラフの後に callback スレッドで直列 (今と同じ)。

## 5. 実測 (plugin 無し、内蔵 device だけの書き出し経路、runner 9 本、1 buffer あたり、Ryzen 9 5950X)

| 曲 | 256 frame 旧 → 新 | 1024 frame 旧 → 新 |
|---|---|---|
| leaf 64 本 (bus 無し) | 0.055 → 0.048 ms | 0.127 → 0.121 ms |
| return 32 本 × 内蔵 EQ/Comp 8 段 | 1.35〜1.48 → 0.22 ms | 5.50 → 0.78 ms |
| return 8 本 × 16 段 | 0.75 → 0.12 ms | 2.78 → 0.41 ms |

旧実装は bus の処理が直列なので runner を増やしても直列 (1.33 / 5.24 ms) と変わらない。

leaf だけの曲は、合流を最後の track を終えた worker に続けて実行させていた間は旧実装より遅かった
(1024 frame で 0.148 ms)。手ごとに測ると track 本体の並列部分は旧 pass 1 と同じ (~59 µs) で、差は全部 **合流の手**
にあった: 直列なら 48 µs の合流が、worker で走ると 67〜80 µs (旧 pass 2 は callback スレッドで 55 µs)。合流を
callback スレッドに固定して解消した (§3)。

## 6. テスト

- **順序の入れ替え**: group の入れ子 / return への send (post / pre fader) / pass 1・pass 2 の consumer を持つ
  サイドチェイン (plugin の tap と内蔵 Comp の SC) / PDC の `ApplyDelay` (MixSrc / BusScAlign) / envelope
  follower を含む曲で、**辺を守る複数の順序** (T 自身・逆順寄りの位相順・決定論的に混ぜた位相順) で job を
  直列実行し、全部 T と bit 一致する。
- **pool と直列**: 同じ曲を worker pool 経路と直列経路で書き出して bit 一致 (多数 buffer、スレッド数を変えて)。
- **辺の検査**: T 上で資源が衝突する全 op 対が、グラフの到達可能性で順序付けられている。
