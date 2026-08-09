// プロジェクトを続けて 2 つ開き、`AudioSourceId` の衝突で前 project の音源が
// 再利用されないことを daw_audio のログで確認するスクリプト。
//
// `AudioSourceId` は Song スコープの名前で project ごとに 1 から再採番される
// ので、別 project でも id 1 が普通に存在する。decode 済みバッファの再利用を
// id 一致だけで判断していると、project B の id 1 に project A の音源が入る。
//
//   cargo run -p daw_gui --features script -- \
//     --script daw_gui/tests/scripts/open_two_projects.js \
//     --arg songA=<A.daw> --arg songB=<B.daw>
//
// daw_audio のログ (`daw_audio::audio_clip_renderer: compiled audio schedule`)
// の読み方:
//   正: B の LoadSong 直後の partial は n_sources が **B 単独の未 decode 分だけ
//       減った値** になり、その後 decode worker の full compile が続く。
//   誤: B の LoadSong 直後の partial がいきなり n_sources 揃いで出て、以後
//       decode compile が続かない (= A のバッファを id 一致で流用した)。
const songA = daw.scriptArgs.songA;
const songB = daw.scriptArgs.songB;
if (!songA || !songB) {
  throw new Error("--arg songA=<A.daw> --arg songB=<B.daw> の両方が必要");
}

// CPAL stream を定常運転にしてから開く (起動直後は callback が前詰めで走る)。
daw.sleepMs(2000);
daw.loadSongFile(songA);
daw.sleepMs(4000);
daw.loadSongFile(songB);
daw.sleepMs(4000);
