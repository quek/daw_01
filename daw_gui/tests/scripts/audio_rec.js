// r.md #156: オーディオ入力の録音を実プロセスで一周させる。`tests/audio_rec_smoke.rs` が段ごとに呼ぶ:
//
//   target/debug/daw_gui.exe --script daw_gui/tests/scripts/audio_rec.js --arg stage=<段> ...
//
// 成功 = exit 0、失敗 = throw して exit 1 (観測値は例外メッセージに載せる)。
// 操作は GUI と同じ `AppEvent` の経路 (`daw.app*` / `daw.setTrackInput` 等) を通す。headless には
// テレメトリの poller が居ないので、engine の transport は `daw.appTick()` で GUI に観測させる。
// デバイスは合成デバイス (`daw.setAudioDevice({driver: "synthetic"})`) だけを開く。音は出ない。
//
// 段:
//   old      版 45 のプロジェクトを開いただけでは dirty にならない
//   ramp     入力設定 → モニター (オフ / 自動 / オン) → 書き出し → count-in 付きの録音 → undo / redo →
//            保存して開き直して再生。入力は RAMP (ch ごとに振幅の違うのこぎり波)
//   loop     ループ録音: 後の周が前の周を上書きする
//   loopback 位置の補正: 鳴らしたクリックを LOOPBACK で録り、録れた位置が曲の位置と一致する
//
// 共通の引数: dir=<書いてよいフォルダ>、renderAhead=0|1 (先読み。省略 = 環境設定のまま)、
// bufferFrames=<合成デバイスのバッファ長。省略 = 既定>、sampleRate=<合成デバイスのレート。省略 = 48000>

// デバイスのレート (= engine のレート = 録音の WAV のレート)。曲は 120 BPM なので 1 拍 = SR / 2 フレーム。
const SR = Number(daw.scriptArgs.sampleRate || 48000);
const FRAMES_PER_BEAT = SR / 2;

function check(cond, msg) {
  if (!cond) {
    throw new Error("r.md #156 audio rec FAILED: " + msg);
  }
}

function activeTab() {
  return JSON.parse(daw.tabsJson()).find((t) => t.active);
}

function rec() {
  return JSON.parse(daw.audioRecStateJson());
}

function song() {
  return JSON.parse(daw.inspectSongJson());
}

// engine の transport を GUI に観測させながら待つ (GUI の poller と同じ 30ms 刻み)。
function tickFor(ms) {
  for (let t = 0; t < ms; t += 30) {
    daw.sleepMs(30);
    daw.appTick();
  }
}

function tickUntil(cond, timeoutMs, what) {
  for (let t = 0; t < timeoutMs; t += 30) {
    daw.sleepMs(30);
    daw.appTick();
    if (cond()) return;
  }
  throw new Error(
    "r.md #156 audio rec FAILED: " + what + " — rec=" + daw.audioRecStateJson() +
      " transport=" + JSON.stringify(daw.transportState()),
  );
}

// 合成デバイスを開く。先読みの on / off は引数どおりにして、実際に替わったことまで見る。
function openSynthetic(device) {
  daw.sleepMs(1000);
  if (daw.scriptArgs.renderAhead !== undefined) {
    daw.appSetRenderAhead(daw.scriptArgs.renderAhead === "1");
  }
  const spec = { driver: "synthetic", device: device, sampleRate: SR };
  if (daw.scriptArgs.bufferFrames !== undefined) {
    spec.bufferFrames = Number(daw.scriptArgs.bufferFrames);
  }
  const info = daw.setAudioDevice(spec);
  check(
    info !== null && info.driver === "synthetic" && info.fallbackReason === null && info.sampleRate === SR,
    "合成デバイスが開けていない: " + JSON.stringify(info),
  );
  check(info.activeInputs.count === 4, "入力 4 ch が開いていない: " + JSON.stringify(info));
  daw.sleepMs(500);
  if (daw.scriptArgs.renderAhead !== undefined) {
    const ahead = JSON.parse(daw.perfJson()).renderAhead;
    check(ahead === (daw.scriptArgs.renderAhead === "1"), "先読みが替わっていない: renderAhead=" + ahead);
  }
  return info;
}

function track(id, name, clips) {
  return {
    id: id,
    name: name,
    volume: 1.0,
    pan: 0.0,
    muted: false,
    solo: false,
    clips: clips,
    next_clip_id: clips.length + 1,
  };
}

// 録音を始めて `ms` だけ録り、止めて、クリップが置かれるまで待つ。録音中の最後の状態を返す。
function recordFor(ms) {
  daw.appRecord();
  tickUntil(() => rec().live, 8000, "録音が始まらない");
  tickFor(ms);
  const last = rec();
  check(last.live, "録音が途中で止まった: " + JSON.stringify(last));
  daw.appStop();
  tickUntil(
    () => {
      const s = rec();
      return !s.requested && s.pendingTakes === 0 && s.region === null;
    },
    8000,
    "録音が閉じない (クリップが置かれない)",
  );
  return last;
}

// トラック 1 のクリップを始まりの順に、素材と一緒に並べる。
function recordedClips(s) {
  return s.tracks[0].clips
    .map((clip) => {
      const events = s.clip_contents[String(clip.content_id)].events;
      check(events.length === 1, "クリップの中の event が 1 つでない: " + JSON.stringify(events));
      return { clip: clip, event: events[0], source: s.audio_sources[String(events[0].source_id)] };
    })
    .sort((a, b) => a.clip.start_beat - b.clip.start_beat);
}

function dbNear(db, want) {
  return db !== null && Math.abs(db - want) < 0.5;
}

// ---- 段 "old": 版 45 のプロジェクトを開いただけでは dirty にならない ----------------
function stageOld() {
  daw.appOpenProject(daw.scriptArgs.old);
  // 開いた後の正規化・asset decode・engine への同期が一巡するまで。
  tickFor(1000);
  const tab = activeTab();
  check(tab.path !== null && tab.path.endsWith("old.daw"), "開けていない: " + JSON.stringify(tab));
  check(!tab.dirty, "開いただけで dirty になった: " + JSON.stringify(tab));
  const t = song().tracks[0];
  // 旧ファイルの入力は MIDI、モニターは自動 (どちらも既定値なので書き出されない)。
  check(t.name === "Old", "トラックが読めていない: " + JSON.stringify(t.name));
  check(t.input === undefined || t.input === "Midi", "入力が MIDI でない: " + JSON.stringify(t.input));
  check(t.monitor === undefined || t.monitor === "Auto", "モニターが自動でない: " + JSON.stringify(t.monitor));
}

// ---- 段 "ramp" -------------------------------------------------------------------
// RAMP の入力 ch 0 は振幅 0.5 (= -6.02 dBFS)、ch 1 は 0.25。
const RAMP_DB = -6.02;

function stageRamp() {
  // 曲: トラック 1 = 録音先 (空)、トラック 2 = 1 拍だけの空の MIDI クリップ (曲末を 1 拍目に置く)。
  const base = {
    bpm: 120.0,
    time_sig: [4, 4],
    length_beats: 16.0,
    tracks: [track(1, "Rec", []), track(2, "End", [{ id: 1, start_beat: 0.0, length_beats: 1.0, content_id: 10 }])],
    next_track_id: 3,
    clip_contents: { "10": { notes: [] } },
    next_content_id: 11,
  };

  // -- デバイスに無い入力は録音しない (OS 標準の既定は入力を開かない) --
  daw.sleepMs(1000);
  daw.appLoadSongJson(JSON.stringify(base));
  daw.sleepMs(300);
  daw.setTrackInput(1, "mono", 0);
  daw.toggleTrackArmed(1);
  daw.appRecord();
  tickFor(300);
  let st = rec();
  check(st.requested && st.pendingTakes === 0 && st.region === null, "無い入力を録ろうとした: " + JSON.stringify(st));
  check(st.status.indexOf("入力がデバイスに無い") >= 0, "無い入力の理由が出ていない: " + JSON.stringify(st));
  daw.appStop();
  tickUntil(() => !rec().requested, 5000, "録音が閉じない");
  check(song().tracks[0].clips.length === 0, "無い入力でクリップができた");
  daw.toggleTrackArmed(1);

  const info = openSynthetic("ramp");
  check(info.inputLatencyFrames === 0 && info.outputLatencyFrames === 0, "RAMP のレイテンシが 0 でない");

  // -- 入力 --
  daw.setTrackInput(1, "stereo", 0);
  daw.sleepMs(300);
  const input = song().tracks[0].input;
  check(JSON.stringify(input) === JSON.stringify({ AudioStereo: { left: 0 } }), "入力: " + JSON.stringify(input));

  // -- モニター (止まったまま)。自動 = 録音待機の間だけ、オン = 常に、オフ = 流さない --
  check(daw.masterPeakDbfs(300) === null, "自動・待機なしで入力が聞こえる");
  check(daw.trackPeak(1, 300) === 0, "自動・待機なしでトラックのメーターが振れる");
  daw.toggleTrackArmed(1);
  daw.sleepMs(300);
  let db = daw.masterPeakDbfs(300);
  check(dbNear(db, RAMP_DB), "自動・待機中に入力が聞こえない: " + db);
  daw.setTrackMonitor(1, "off");
  daw.sleepMs(300);
  check(daw.masterPeakDbfs(300) === null, "オフなのに入力が聞こえる");
  // 入力メーター: モニターがオフでも、録音待機のトラックのメーターには入力レベルが出る。
  const meter = daw.trackPeak(1, 300);
  check(Math.abs(meter - 0.5) < 0.02, "オフ・待機中の入力メーター: " + meter);
  daw.toggleTrackArmed(1);
  daw.setTrackMonitor(1, "on");
  daw.sleepMs(300);
  db = daw.masterPeakDbfs(300);
  check(dbNear(db, RAMP_DB), "オン・待機なしで入力が聞こえない: " + db);

  // -- 書き出しに入力音は入らない (モニターがオンのまま書き出す。中身は Rust 側が見る) --
  daw.exportWavRange(daw.scriptArgs.dir + "/export.wav", 0.0, 4.0, 60000);
  daw.sleepMs(300);
  db = daw.masterPeakDbfs(300);
  check(dbNear(db, RAMP_DB), "書き出しの後、モニターが戻らない: " + db);

  // -- 録音 (count-in 1 小節 = 2 秒。モニターはオフ、録音待機) --
  daw.setTrackMonitor(1, "off");
  daw.toggleTrackArmed(1);
  daw.setCountInBars(1);
  daw.sleepMs(300);
  daw.appRecord();
  tickFor(600);
  st = rec();
  // count-in の間は録音が始まっていない: 頼んだ録音は待っていて、帯はまだ伸びない。
  check(
    st.requested && !st.live && st.pendingTakes === 1 && st.region !== null && st.region.span === null,
    "count-in の間の状態: " + JSON.stringify(st),
  );
  tickUntil(() => rec().live, 8000, "count-in が明けない");
  tickFor(1300);
  st = rec();
  // 曲末 (1 拍) を越えても録音は続く。帯は録り始めからプレイヘッドまで (Tick の刻みぶんの粗さ)。engine が
  // 知らせた区間の頭は正確に拍 0 で、描くときはこちらを帯の頭に使う。
  check(st.live, "曲末で録音が止まった: " + JSON.stringify(st));
  check(st.region.span !== null && st.region.span[0] < 0.2 && st.region.span[1] > 2.0, "帯: " + JSON.stringify(st));
  check(
    st.region.segment !== null && st.region.segment.startBeat === 0 && st.region.segment.fileFrame === 0,
    "録音中の区間: " + JSON.stringify(st),
  );
  // 帯の中の波形: 山が届いていて、高さは入力の振幅 0.5 (= 255 段階の 128)。
  const peaks = st.region.peaks;
  check(
    peaks.length === 1 && peaks[0].trackId === 1 && peaks[0].count > 50 && Math.abs(peaks[0].max - 128) <= 1,
    "録音中の波形: " + JSON.stringify(st),
  );
  daw.appStop();
  tickUntil(
    () => {
      const s = rec();
      return !s.requested && s.pendingTakes === 0 && s.region === null;
    },
    8000,
    "録音が閉じない (クリップが置かれない)",
  );

  // -- クリップ: 拍 0 から、録った長さぶん。素材は 32-bit float ステレオ・デバイスのレート --
  let s = song();
  let clips = recordedClips(s);
  check(clips.length === 1, "クリップが 1 つでない: " + JSON.stringify(s.tracks[0].clips));
  const c = clips[0];
  daw.log("RAMP_CLIP=" + JSON.stringify(c));
  check(c.clip.start_beat === 0, "クリップの始まりが拍 0 でない: " + c.clip.start_beat);
  check(c.source.sample_rate === SR && c.source.channels === 2, "素材の形式: " + JSON.stringify(c.source));
  check(c.source.frames > SR && c.source.frames < SR * 4, "録れた長さ: " + c.source.frames);
  check(
    c.event.source_start_frames === 0 && c.event.source_end_frames === c.source.frames && c.event.stretch_mode === "Raw",
    "event: " + JSON.stringify(c.event),
  );
  check(
    Math.abs(c.clip.length_beats - c.source.frames / FRAMES_PER_BEAT) < 1e-6,
    "クリップの長さ: " + c.clip.length_beats,
  );
  // 止めた位置まで録れている (録音は拍 0 から)。
  const stopped = daw.transportState();
  daw.log("RAMP_STOP=" + JSON.stringify(stopped));
  check(c.source.frames === stopped.playhead, "録れた長さ " + c.source.frames + " が止めた位置 " + stopped.playhead + " と違う");

  // -- undo で消え、redo で戻る (録音 1 回 = 1 step) --
  daw.appUndo();
  s = song();
  check(s.tracks[0].clips.length === 0, "undo でクリップが消えない");
  check(s.audio_sources === undefined || Object.keys(s.audio_sources).length === 0, "undo で素材が残った");
  daw.appRedo();
  check(song().tracks[0].clips.length === 1, "redo でクリップが戻らない");

  // -- 保存して開き直しても鳴る --
  const project = daw.scriptArgs.dir + "/proj/rec.daw";
  daw.appSaveAs(project);
  tickFor(500);
  let tab = activeTab();
  check(tab.path !== null && !tab.dirty, "保存できていない: " + JSON.stringify(tab));
  // いまのタブを閉じてから開く (同じプロジェクトを開いたままだと、そのタブへ切り替わるだけ)。
  daw.newTab();
  daw.switchTab(0);
  daw.closeTab();
  daw.appOpenProject(project);
  tickFor(1500);
  tab = activeTab();
  check(tab.path !== null && tab.path.endsWith("rec.daw") && !tab.dirty, "開き直せていない: " + JSON.stringify(tab));
  s = song();
  clips = recordedClips(s);
  check(clips.length === 1 && clips[0].source.frames === c.source.frames, "開き直したクリップ: " + JSON.stringify(clips));
  check(JSON.stringify(s.tracks[0].input) === JSON.stringify({ AudioStereo: { left: 0 } }), "入力が保存されていない");
  check(s.tracks[0].monitor === "Off", "モニターが保存されていない: " + JSON.stringify(s.tracks[0].monitor));
  daw.appPlay();
  tickFor(300);
  db = daw.masterPeakDbfs(500);
  daw.appStop();
  tickFor(300);
  // モニターはオフなので、聞こえているのは録ったクリップ (RAMP の振幅 0.5)。
  check(dbNear(db, RAMP_DB), "開き直したプロジェクトで録音が鳴らない: " + db);
}

// ---- 段 "loop": ループ録音 --------------------------------------------------------
function stageLoop() {
  openSynthetic("ramp");
  daw.appLoadSongJson(
    JSON.stringify({ bpm: 120.0, time_sig: [4, 4], length_beats: 16.0, tracks: [track(1, "Rec", [])], next_track_id: 2 }),
  );
  daw.sleepMs(300);
  daw.setTrackInput(1, "mono", 1);
  daw.setTrackMonitor(1, "off");
  daw.toggleTrackArmed(1);
  // 2 拍 (1 秒) のループを 2 周半。
  daw.setLoop(0.0, 2.0);
  daw.sleepMs(300);
  const last = recordFor(2500);
  // 折り返すたびに帯は折り返し先から伸び直す。いまの区間は 3 周目 (WAV の 2 周ぶんより後ろが、拍 0 から)。
  check(last.region.span[0] < 0.2 && last.region.span[1] < 2.1, "折り返した後の帯: " + JSON.stringify(last));
  check(
    last.region.segment.startBeat === 0 && last.region.segment.fileFrame >= 2 * SR,
    "折り返した後の区間: " + JSON.stringify(last),
  );

  // 後の周が前の周を上書きする: 最後の周 (途中まで) が頭から、その後ろに 1 つ前の周の残り。
  const s = song();
  const clips = recordedClips(s);
  daw.log("LOOP_CLIPS=" + JSON.stringify(clips));
  check(clips.length === 2, "クリップが 2 つでない: " + JSON.stringify(s.tracks[0].clips));
  const [head, tail] = clips;
  check(head.clip.start_beat === 0, "最後の周が拍 0 から始まっていない: " + head.clip.start_beat);
  check(
    Math.abs(tail.clip.start_beat - (head.clip.start_beat + head.clip.length_beats)) < 1e-6,
    "2 つのクリップが隣り合っていない: " + JSON.stringify([head.clip, tail.clip]),
  );
  // ループの端は 1 buffer の粒度で折り返す (engine の折り返しと同じ)。
  const end = tail.clip.start_beat + tail.clip.length_beats;
  check(end >= 2.0 && end < 2.1, "前の周の終わり: " + end);
  check(head.event.source_id === tail.event.source_id, "2 つのクリップの素材が別");
  check(Object.keys(s.audio_sources).length === 1, "素材が 1 つでない");
  // 最後の周は WAV の 3 周目 (1 周 = 2 拍 = SR フレーム。2 周ぶんより後ろ)、残りは 2 周目。
  check(head.event.source_start_frames >= 2 * SR, "最後の周の素材位置: " + head.event.source_start_frames);
  check(
    tail.event.source_start_frames >= SR && tail.event.source_end_frames <= head.event.source_start_frames,
    "前の周の素材位置: " + JSON.stringify(tail.event),
  );
  check(head.source.sample_rate === SR, "素材のレート: " + JSON.stringify(head.source));
  // undo 1 回で、録った全部の周が消える。
  daw.appUndo();
  check(song().tracks[0].clips.length === 0, "undo で全部の周が消えない");
}

// ---- 段 "loopback": 位置の補正 ----------------------------------------------------
// トラック 2 のクリック (WAV の `click` フレーム目に 1 サンプル) を鳴らし、LOOPBACK で戻ってきた音をトラック 1 に
// 録る。入出力のレイテンシ (と先読み) を正しく引けていれば、録れた WAV の同じフレームにクリックがある。
function stageLoopback() {
  const info = openSynthetic("loopback");
  check(
    info.inputLatencyFrames === 64 && info.outputLatencyFrames === 128 + info.bufferFrames,
    "LOOPBACK のレイテンシの申告: " + JSON.stringify(info),
  );
  const click = {
    source_id: 1,
    event_start_in_clip_beats: 0.0,
    event_length_beats: 4.0,
    source_start_frames: 0,
    source_end_frames: 96000,
    gain_db: 0.0,
    pan: 0.0,
    pitch_semitones: 0.0,
    formant_semitones: 0.0,
    stretch_mode: "Raw",
    fade_in_beats: 0.0,
    fade_out_beats: 0.0,
    fade_in_curve: "Linear",
    fade_out_curve: "Linear",
    reversed: false,
    muted: false,
  };
  daw.appLoadSongJson(
    JSON.stringify({
      bpm: 120.0,
      time_sig: [4, 4],
      length_beats: 16.0,
      tracks: [
        track(1, "Rec", []),
        track(2, "Click", [{ id: 1, start_beat: 0.0, length_beats: 4.0, content_id: 1 }]),
      ],
      next_track_id: 3,
      clip_contents: { "1": { events: [click] } },
      next_content_id: 2,
      audio_sources: { "1": { path: { Absolute: daw.scriptArgs.wav }, sample_rate: 48000, channels: 1, frames: 96000 } },
      next_audio_source_id: 2,
    }),
  );
  // クリックの WAV の decode が engine に届くまで。
  daw.sleepMs(1000);
  daw.setTrackInput(1, "stereo", 0);
  // 録った音を出力へ戻さない (戻すと LOOPBACK で回り続ける)。
  daw.setTrackMonitor(1, "off");
  daw.toggleTrackArmed(1);
  daw.sleepMs(300);
  recordFor(1500);

  const s = song();
  const clips = recordedClips(s);
  check(clips.length === 1, "クリップが 1 つでない: " + JSON.stringify(s.tracks[0].clips));
  daw.log("LOOPBACK_CLIP=" + JSON.stringify(clips[0]));
  check(clips[0].clip.start_beat === 0, "クリップの始まりが拍 0 でない: " + clips[0].clip.start_beat);
  // 中身は Rust 側が見る (保存して bundle の samples/ に入った WAV を開く)。
  daw.appSaveAs(daw.scriptArgs.dir + "/proj/loopback.daw");
  tickFor(500);
  const tab = activeTab();
  check(tab.path !== null && !tab.dirty, "保存できていない: " + JSON.stringify(tab));
}

const stages = { old: stageOld, ramp: stageRamp, loop: stageLoop, loopback: stageLoopback };
const stage = stages[daw.scriptArgs.stage];
check(stage !== undefined, "unknown stage: " + daw.scriptArgs.stage);
stage();
