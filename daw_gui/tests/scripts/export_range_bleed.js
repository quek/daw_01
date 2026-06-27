// headless export-range "head bleed" harness.
//   --arg play=1|0    roll the transport past bar 3.1 before exporting
//                     (reproduces a synth holding a LIVE voice). default 1.
//   --arg reinit=1|0  reinit (deactivate→activate) all plugins before export
//                     (the fix). default 1.
//
// daw_audio logs `TEMP diag: export buffer0 plugin out` per plugin on the first
// render buffer. plugin_id=9 = VCV Rack 2.
//   no-play  no-reinit -> pure cold content at 3.1 (what SHOULD be there)
//   play     no-reinit -> content + carried live voice (the bug)
//   play     reinit    -> should match no-play if reinit clears the live voice

var project = daw.scriptArgs.project;
var out = daw.scriptArgs.output;
var doPlay = daw.scriptArgs.play !== "0";
var doReinit = daw.scriptArgs.reinit !== "0";

daw.loadSongFile(project);
daw.sleepMs(8000);

if (doPlay) {
  daw.play();
  daw.sleepMs(5000);
  daw.stop();
  daw.sleepMs(300);
}

if (doReinit) {
  daw.reinitForExport(30000);
}

daw.exportWavRange(out, 164571, 3620571, 120000);
