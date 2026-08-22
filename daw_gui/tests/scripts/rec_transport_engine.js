// r.md #51: audio engine 側のトランスポート契約を実プロセスで検証する。
//
//   cargo run -p daw_gui --features script -- \
//     --script daw_gui/tests/scripts/rec_transport_engine.js
//
// 成功 = exit 0、失敗 = throw して exit 1 (script harness に log API は無いので、
// 失敗時の観測値は例外メッセージに載せる)。
//
// 検証するのは「engine が transport 状態の所有者になっている」こと:
//  1. Play で `playing` が立ち、プレイヘッドが進む。
//  2. Stop で `playing` が落ちる。
//  3. StartRecording{preroll} で count-in に入り、その間 `recordingLive` は
//     false。明けると `recordingLive` が立つ。
//  4. count-in 中に Stop すると preroll ごと捨てられて **曲は鳴り出さない**
//     (旧実装は preroll だけ 0 にして Play を残していたので、取り消したのに
//      再生が始まった)。
//  5. パンチアウト (StopRecording) は transport を止めない。
//
// 起動時の default song はクリップが無い = `song_ended` が常に false なので、
// 曲末 auto-stop に邪魔されずに transport の素の挙動だけを見られる。

function fmt(label, s) {
  return (
    `${label}{playing=${s.playing} recordingLive=${s.recordingLive} ` +
    `preroll=${s.prerollRemaining} playhead=${s.playhead}}`
  );
}

function check(cond, msg, label, s) {
  if (!cond) {
    throw new Error(`r.md #51 engine contract FAILED: ${msg} — ${fmt(label, s)}`);
  }
}

// CPAL stream が定常運転に入るまで待つ (起動直後は callback が前詰めで発火する)。
daw.sleepMs(1500);

// ---- 1. Play で走り出し、プレイヘッドが進む ----------------------------
const idle = daw.transportState();
check(!idle.playing, "起動直後は playing=false のはず", "idle", idle);

daw.play();
daw.sleepMs(500);
const playing = daw.transportState();
check(playing.playing, "Play で playing=true になるはず", "play", playing);
check(
  playing.playhead > idle.playhead,
  "再生中はプレイヘッドが進むはず",
  "play",
  playing,
);

// ---- 2. Stop で止まる --------------------------------------------------
daw.stop();
daw.sleepMs(300);
const stopped = daw.transportState();
check(!stopped.playing, "Stop で playing=false になるはず", "stop", stopped);

// ---- 3. count-in 中は録音実体が立たない --------------------------------
// 48kHz で 0.5 秒ぶんの preroll。
daw.startRecording(24000);
daw.play();
daw.sleepMs(200);
const countIn = daw.transportState();
check(countIn.playing, "count-in 中も transport は走るはず", "countIn", countIn);
check(
  !countIn.recordingLive,
  "count-in 中に録音実体が立ってはいけない",
  "countIn",
  countIn,
);
check(
  countIn.prerollRemaining > 0,
  "count-in 中は preroll が残っているはず",
  "countIn",
  countIn,
);

daw.sleepMs(800);
const live = daw.transportState();
check(live.recordingLive, "count-in 明けに録音実体が立つはず", "live", live);
check(
  live.prerollRemaining === 0,
  "count-in 明けは preroll を使い切っているはず",
  "live",
  live,
);

// ---- 5. パンチアウトは transport を止めない ----------------------------
daw.stopRecording();
daw.sleepMs(300);
const punched = daw.transportState();
check(
  !punched.recordingLive,
  "パンチアウトで録音実体は落ちるはず",
  "punchOut",
  punched,
);
check(
  punched.playing,
  "パンチアウトで transport を止めてはいけない",
  "punchOut",
  punched,
);

daw.stop();
daw.sleepMs(300);
const afterStop = daw.transportState();
check(!afterStop.playing, "Stop で止まるはず", "afterStop", afterStop);

// ---- 4. count-in 中の停止は「取り消し」で、曲が鳴り出さない ------------
daw.startRecording(48000); // 1 秒
daw.play();
daw.sleepMs(200);
const countIn2 = daw.transportState();
check(
  countIn2.prerollRemaining > 0,
  "2 回目も count-in に入るはず",
  "countIn2",
  countIn2,
);

daw.stop();
daw.stopRecording();
// 元の preroll が明けるはずだった時刻を十分に過ぎるまで待つ。
daw.sleepMs(1200);
const cancelled = daw.transportState();
check(
  !cancelled.playing,
  "count-in を取り消した後に曲が鳴り出してはいけない",
  "cancelled",
  cancelled,
);
check(
  !cancelled.recordingLive,
  "取り消し後に録音実体が立ってはいけない",
  "cancelled",
  cancelled,
);
check(
  cancelled.prerollRemaining === 0,
  "取り消しで preroll は捨てられるはず",
  "cancelled",
  cancelled,
);
