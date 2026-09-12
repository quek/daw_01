// `docs/plan_project_tabs.md` §5.7: プロジェクトタブを実プロセス (daw_gui + daw_audio +
// daw_plugin_host) で検証する。
//
//   cargo run -p daw_gui --features script -- \
//     --script daw_gui/tests/scripts/project_tabs.js
//
// 検証: 2 つのタブに別々の曲を載せ、両方を再生すると **それぞれの engine slot が
// 独立に走る** (Q1: 背景タブも鳴り続ける、transport は project ごと)。切替は
// 走行状態に影響せず、閉じると slot が消える。
//
// 起動時の default song はクリップが無く曲末 auto-stop に邪魔されない。

function check(cond, msg, extra) {
  if (!cond) {
    throw new Error(`project_tabs FAILED: ${msg} — ${JSON.stringify(extra)}`);
  }
}

function tabs() {
  return JSON.parse(daw.tabsJson());
}

// CPAL stream が定常運転に入るまで待つ。
daw.sleepMs(1500);

// ---- 1. 起動直後は 1 タブ、停止 ----
let t = tabs();
check(t.length === 1 && t[0].active && !t[0].playing, "起動直後は 1 タブ停止", t);

// ---- 2. 2 つ目のタブ (engine slot が増える) ----
daw.newTab();
daw.sleepMs(300);
t = tabs();
check(t.length === 2 && t[1].active, "newTab で 2 タブ、新しい方がアクティブ", t);

// ---- 3. タブ 2 (アクティブ) を再生 → タブ 1 は止まったまま ----
daw.play();
daw.sleepMs(500);
t = tabs();
check(t[1].playing && !t[0].playing, "アクティブなタブだけが走る", t);
const s2 = daw.transportState();
check(s2.playing && s2.playhead > 0, "transportState はアクティブなタブの slot", s2);

// ---- 4. タブ 1 へ切り替えても タブ 2 は鳴り続ける (Q1) ----
daw.switchTab(0);
daw.sleepMs(300);
t = tabs();
check(t[0].active && t[1].playing && !t[0].playing, "背景タブは走り続ける", t);
const s1 = daw.transportState();
check(!s1.playing, "切替後の transportState はタブ 1 (停止)", s1);

// ---- 5. タブ 1 も再生 → 両方走る、プレイヘッドは独立 ----
daw.play();
daw.sleepMs(400);
t = tabs();
check(t[0].playing && t[1].playing, "両方のタブが同時に走る", t);
const s1b = daw.transportState();
daw.switchTab(1);
daw.sleepMs(100);
const s2b = daw.transportState();
check(s2b.playhead > s1b.playhead, "先に走り出したタブ 2 の方が進んでいる", { s1b, s2b });

// ---- 6. タブ 2 を停止しても タブ 1 は走る ----
daw.stop();
daw.sleepMs(300);
t = tabs();
check(!t[1].playing && t[0].playing, "Stop はアクティブなタブだけ", t);

// ---- 7. タブ 2 を閉じる → 1 タブ、タブ 1 は走ったまま ----
daw.closeTab();
daw.sleepMs(300);
t = tabs();
check(t.length === 1 && t[0].active && t[0].playing, "閉じても残ったタブは走り続ける", t);

daw.stop();
daw.sleepMs(200);
