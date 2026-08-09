// 同じプロジェクトを続けて N 回開いても、毎回すべての device が plugin_host に
// load できることを検証する headless 回帰スクリプト。
//
//   cargo build -p daw_gui --features script
//   target/debug/daw_gui.exe --script daw_gui/tests/scripts/reopen_same_project.js \
//     --arg song=<X.daw> [--arg rounds=3] [--arg waitMs=8000]
//
// exit 0 = pass / JS error (exit 1) = fail。
//
// ■ 何の回帰か
// `ProcessData` shmem の名前が `(plugin_host pid, 安定 device_id)` の純関数だった
// ため、同じプロジェクトを開き直すと **必ず同じ名前**で再作成しようとしていた。
// Windows の named section は全プロセスがハンドルを閉じるまで名前が生き続け、
// daw_audio の解放は RT の bundle 差し替え + off-thread drain を経る非同期処理で
// 完了時刻に上限が無い。よって plugin_host の create が daw_audio の解放より速いと
//   `shmem create failed: shmem daw_01_process_data_<pid>_<device_id> already exists`
// で `SlotPluginLoadFailed` になり、その device はそのセッション中ずっと無音だった
// (ユーザー報告: 「プロジェクトを開き直すと VOICEVOX が鳴らなくなる」)。
// 修正は名前に instantiation ごとの incarnation を焼き込むこと
// (`common::plugin_ref::process_data_shmem_id`)。
//
// ■ 判定方法
// ログ grep ではなく `daw.pendingPluginLoadsJson()` / `daw.takePluginLoadEventsJson()`
// で load 応答そのものを見る:
//   - `loadSongFile` 直後の pending = この round で load を要求した device 全部
//   - 待機後: failed が 0 件 / pending が空 / 要求した全 device が loaded に居る
// (daw_audio 側の `plugin shmem registered` は、失敗すれば `SlotPluginLoadFailed`
//  か pending の残留として必ずここに現れる。)
//
// ■ レースを決定論に落とす (修正前は必ず落ち、修正後は必ず通ることの確認手順)
// 素の実行では「daw_audio の drain が間に合ったか」次第で結果が揺れる。daw_audio に
// debug ビルド限定のフォールトインジェクションがあるので、それを使って常に
// 「daw_audio が旧 mapping を握ったまま」の状態を作る:
//
//   DAW01_TEST_SHMEM_HOLD_MS=4000 target/debug/daw_gui.exe --script ... --arg song=...
//
// 遅延を入れるのは create 側ではなく **保持側** であることに注意 (create を遅らせると
// 解放が先に間に合って逆に失敗しにくくなる)。詳細は
// `daw_audio::hold_released_entry_for_test`。
const song = daw.scriptArgs.song;
if (!song) {
  throw new Error("--arg song=<X.daw> が必要");
}
const rounds = Number(daw.scriptArgs.rounds || "3");
const waitMs = Number(daw.scriptArgs.waitMs || "8000");

// CPAL stream を定常運転にしてから開く (起動直後は callback が前詰めで走る)。
daw.sleepMs(2000);

const problems = [];
for (let round = 1; round <= rounds; round++) {
  // 前 round の残りを捨ててから開く。
  daw.takePluginLoadEventsJson();
  daw.loadSongFile(song);
  // GUI の File→Open と同じ順序 (teardown → restore) を通った直後の pending が
  // 「この round で load を要求した device」。
  const requested = JSON.parse(daw.pendingPluginLoadsJson());
  daw.sleepMs(waitMs);
  const events = JSON.parse(daw.takePluginLoadEventsJson());
  const stillPending = JSON.parse(daw.pendingPluginLoadsJson());

  if (requested.length === 0) {
    problems.push(
      "round " + round + ": load 要求が 0 件 — テストが何も検証していない " +
        "(plugin DB 未ロード / device 無しのプロジェクト?)"
    );
    continue;
  }
  for (let i = 0; i < events.failed.length; i++) {
    const f = events.failed[i];
    problems.push(
      "round " + round + ": device " + f.device_id + " (" + f.plugin_id +
        ") の load が失敗: " + f.reason
    );
  }
  if (stillPending.length > 0) {
    problems.push(
      "round " + round + ": load 応答が来ない device: " + stillPending.join(", ")
    );
  }
  for (let i = 0; i < requested.length; i++) {
    if (events.loaded.indexOf(requested[i]) < 0) {
      problems.push(
        "round " + round + ": device " + requested[i] +
          " の SlotPluginLoaded を観測できなかった"
      );
    }
  }
}

if (problems.length > 0) {
  throw new Error(
    "reopen_same_project: " + problems.length + " 件の問題\n  " +
      problems.join("\n  ")
  );
}
