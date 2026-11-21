// r.md #156: オーディオデバイスの切り替えを実プロセスで検証する (`docs/plan_audio_io.md`)。
//
//   cargo run -p daw_gui --features script -- \
//     --script daw_gui/tests/scripts/audio_device_switch.js [--arg "asio=FL Studio ASIO|Realtek ASIO"]
//
// 成功 = exit 0、失敗 = throw して exit 1。窓なしの script は無音 (`AudioSession::mute_output`)。
//
// 切り替えるたびに確かめること:
//  - デバイス情報が設定どおり (種類・レート・バッファ・入力 ch・既定へ戻した理由の有無)。
//  - engine がそのレートとバッファで回っている (metrics 面の値 = 実際のコールバックの値)。
//  - 再生すると再生位置が進む (= コールバックが engine を呼んでいる)。
//
// 引数なしで回るのは、どの PC にもあるもの (OS 標準の既定・合成デバイス・存在しないデバイス) だけ。
// `--arg asio=<名前>|<名前>` を渡すと、その ASIO ドライバーにも切り替える (ドライバーは排他のことが多いので、
// 使用中の実機を指さないこと)。

function fail(step, msg, info) {
  throw new Error(`audio device [${step}] FAILED: ${msg} — info=${JSON.stringify(info)}`);
}

// 切り替えた後の共通の検査。`want` = { driver, fallback (理由が付くはずか), sampleRate?, bufferFrames?, inputs? }。
function verify(step, info, want) {
  if (info === null) fail(step, "デバイス情報が無い (daw_audio と繋がっていない)", info);
  daw.log(`[${step}] ${JSON.stringify(info)}`);
  if (info.driver !== want.driver) fail(step, `driver が ${want.driver} でない`, info);
  if ((info.fallbackReason !== null) !== want.fallback) {
    fail(step, want.fallback ? "既定へ戻した理由が付いていない" : "設定どおりに開けていない", info);
  }
  if (!(info.sampleRate > 0) || !(info.bufferFrames > 0)) fail(step, "レート / バッファが 0", info);
  if (want.sampleRate !== undefined && info.sampleRate !== want.sampleRate) {
    fail(step, `レートが ${want.sampleRate} でない`, info);
  }
  if (want.bufferFrames !== undefined && info.bufferFrames !== want.bufferFrames) {
    fail(step, `バッファが ${want.bufferFrames} でない`, info);
  }
  if (want.inputs !== undefined && info.activeInputs.count !== want.inputs) {
    fail(step, `開いた入力 ch が ${want.inputs} 本でない`, info);
  }
  if (info.outputChannels.length < 1) fail(step, "出力 ch が無い", info);

  // 再生位置が進むこと。再生中の 2 点で比べる (停止中の値は、起こし直す前の engine が最後に書いたものが
  // 残っていることがある)。進む量はデバイスのレートどおり (±30%: sleep の粒度と publish の周期ぶん)。
  daw.play();
  daw.sleepMs(300);
  const p1 = daw.transportState();
  daw.sleepMs(500);
  const p2 = daw.transportState();
  const advanced = p2.playhead - p1.playhead;
  if (!p2.playing || !(advanced > 0)) {
    fail(step, `再生位置が進まない (playing=${p2.playing} playhead ${p1.playhead} → ${p2.playhead})`, info);
  }
  const expected = info.sampleRate * 0.5;
  if (advanced < expected * 0.7 || advanced > expected * 1.3) {
    fail(step, `再生位置の進み ${advanced} が 0.5 秒ぶん (${expected}) から外れている`, info);
  }
  // engine が実際に回っているレートとバッファ (コールバックが publish した値)。
  const m = JSON.parse(daw.metricsJson());
  if (m.sampleRate !== info.sampleRate) fail(step, `engine のレート ${m.sampleRate} がデバイスと違う`, info);
  // OS 標準はバッファ長を選べず、申告も開いた後の観測値なので、ここでは合成 / ASIO だけ突き合わせる。
  if (info.driver !== "system" && m.bufferFrames !== Math.min(info.bufferFrames, 1024)) {
    fail(step, `engine のバッファ ${m.bufferFrames} がデバイスと違う`, info);
  }
  // 入力を開いていれば、定常運転で入力を捨てたり埋めたりしていないこと (同じ時計のデバイス)。
  if (info.driver !== "system" && info.activeInputs.count > 0) {
    const a = daw.audioInputStats();
    daw.sleepMs(500);
    const b = daw.audioInputStats();
    if (b.droppedFrames !== a.droppedFrames || b.filledFrames !== a.filledFrames) {
      fail(step, `入力が欠けている (${JSON.stringify(a)} → ${JSON.stringify(b)})`, info);
    }
  }
  daw.stop();
  daw.sleepMs(200);
  if (daw.transportState().playing) fail(step, "Stop で止まらない", info);
}

// stream が定常運転に入るまで待つ。
daw.sleepMs(1000);

// ---- 0. 起動直後: OS 標準の既定 (検証用の起動は隔離した設定なので、保存された実機は開かない) ----
const start = daw.audioDeviceInfo();
verify("start", start, { driver: "system", fallback: false, inputs: 0 });

const list = daw.audioDevices();
daw.log(`devices: ${JSON.stringify(list)}`);
if (list.systemOutputs.length < 1) throw new Error("OS 標準の出力デバイスが 1 つも列挙されない");

// ---- 1. 合成 (ramp): 入力 4 ch、レート・バッファは指定どおり ----
verify("ramp", daw.setAudioDevice({ driver: "synthetic", device: "ramp", sampleRate: 44100, bufferFrames: 256 }), {
  driver: "synthetic",
  fallback: false,
  sampleRate: 44100,
  bufferFrames: 256,
  inputs: 4,
});

// ---- 2. 合成 (loopback): 既定のレート・バッファ、レイテンシの申告 ----
const loop = daw.setAudioDevice({ driver: "synthetic", device: "loopback" });
verify("loopback", loop, { driver: "synthetic", fallback: false, sampleRate: 48000, bufferFrames: 128, inputs: 4 });
if (loop.inputLatencyFrames !== 64 || loop.outputLatencyFrames !== 128 + 128) {
  fail("loopback", "レイテンシの申告が 入力 64 / 出力 128 + バッファ でない", loop);
}

// ---- 3. デバイスの 1 回が engine の作業場 (1024) を超える: 割って描く ----
verify("big buffer", daw.setAudioDevice({ driver: "synthetic", device: "ramp", bufferFrames: 2048 }), {
  driver: "synthetic",
  fallback: false,
  bufferFrames: 2048,
  inputs: 4,
});

// ---- 4. 入力を使わない ----
verify("no input", daw.setAudioDevice({ driver: "synthetic", device: "ramp", inputCount: 0 }), {
  driver: "synthetic",
  fallback: false,
  inputs: 0,
});

// ---- 4b. ドライバー起点の開き直し (ASIO の reset 要求と同じ経路): daw_audio はそのまま、stream だけ組み直す ----
// 合成の "reopen" は動き出した 1 秒後に開き直しを 1 回求める。開いた回数が名前に入る。
const reopenFirst = daw.setAudioDevice({ driver: "synthetic", device: "reopen" });
if (reopenFirst.outputName !== "Synthetic (reopen #1)") fail("reopen", "1 回目の名前でない", reopenFirst);
daw.play();
daw.sleepMs(2500);
const reopened = daw.audioDeviceInfo();
if (reopened.outputName !== "Synthetic (reopen #2)") fail("reopen", "開き直していない", reopened);
// 開き直した後も同じ engine が鳴り続けている (再生は止まっておらず、入力も欠けない)。
verify("reopen (after)", reopened, { driver: "synthetic", fallback: false, inputs: 4 });

// ---- 5. ASIO (渡された名前だけ) ----
const asio = (daw.scriptArgs.asio || "").split("|").filter((n) => n.length > 0);
for (const name of asio) {
  const info = daw.setAudioDevice({ driver: "asio", device: name });
  verify(`asio ${name}`, info, { driver: "asio", fallback: false });
  if (!info.hasControlPanel) fail(`asio ${name}`, "コントロールパネルが無いことになっている", info);
  // 既定へ戻してから次へ (ドライバーを続けて開き直す経路と、ASIO → 標準 → ASIO の経路の両方を通す)。
  verify(`asio ${name} → system`, daw.setAudioDevice({}), { driver: "system", fallback: false, inputs: 0 });
}
if (asio.length >= 2) {
  // ASIO から別の ASIO へ直接。
  verify("asio → asio (1)", daw.setAudioDevice({ driver: "asio", device: asio[0] }), { driver: "asio", fallback: false });
  verify("asio → asio (2)", daw.setAudioDevice({ driver: "asio", device: asio[1] }), { driver: "asio", fallback: false });
}

// ---- 6. 保存されたデバイスがこの PC に無い: OS 標準の既定へ戻り、理由が付く ----
const missingAsio = daw.setAudioDevice({
  driver: "asio",
  id: "{00000000-0000-0000-0000-000000000000}",
  device: "No Such ASIO Driver",
});
verify("missing asio", missingAsio, { driver: "system", fallback: true, inputs: 0 });

const missingOutput = daw.setAudioDevice({ driver: "system", id: "no-such-endpoint", device: "No Such Output" });
verify("missing output", missingOutput, { driver: "system", fallback: true, inputs: 0 });

const missingSynthetic = daw.setAudioDevice({ driver: "synthetic", device: "no-such" });
verify("missing synthetic", missingSynthetic, { driver: "system", fallback: true, inputs: 0 });

// ---- 7. 既定へ戻す ----
verify("back to default", daw.setAudioDevice({}), { driver: "system", fallback: false, inputs: 0 });
