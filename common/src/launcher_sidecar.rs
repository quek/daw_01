//! r.md #87 (`docs/plan_rmd_87_clip_launcher.md` §2.5 / §3.6): **ランチャー走行状態の
//! sidecar**。
//!
//! 動画書き出し (`daw_gui::render_video`) は live の
//! [`AudioBridge::launcher_rows`](crate::audio_bridge) 平面を読めない — オフラインの
//! フレーム描画中、オーディオエンジンはもう走っていない。そこでオフラインの音声書き出し
//! (`daw_audio::export`) が **行ごとの走行状態の遷移列**を WAV の隣の sidecar へ焼き、
//! 動画側は各フレームの拍でそれを差す。[`crate::mod_sidecar::ModEnvSidecar`] が
//! 変調エンベロープでやっているのと**同じ形**で、理由も同じ。
//!
//! # なぜ GUI 側で解き直さないのか
//!
//! フォローアクションの遷移 (どの列へ / いつ移るか) を決めているのは
//! `daw_audio::launcher::LauncherRuntime` で、乱数も量子化も列の連鎖もそこに閉じている。
//! GUI 側で同じ式を書くと**遷移の実装が 2 本**になり、片方だけ直したときに
//! 「音は Scene2 へ移ったのに絵は Scene1 をループし続ける」で出る (= SSoT 違反)。
//! ここを通せば、音と絵は**同じ 1 本の走行**を見る。
//!
//! # 中身
//!
//! 状態は区分定数 (遷移のあった瞬間にしか変わらない) なので、**遷移だけ**を
//! 昇順で並べる。行の状態は「その行の、`beat` 以下で最後の遷移」で決まる。
//! 書き出しの先頭で全行ぶんの初期状態が必ず 1 件ずつ入るので、
//! sidecar が非空なら行の状態は常に確定する (`Song` へのフォールバックが要らない)。
//!
//! Layout (little-endian):
//! `MAGIC u32 | n_events u32 | events[n_events]`、
//! 1 event = `beat f64 | row_key u64 | state u32 | clip_id u32 | launch_beat f64`。

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::model::RowPlayback;

const MAGIC: u32 = 0x4c_4e_43_48; // "LNCH"

/// 1 event のバイト長 (`beat` 8 + `row_key` 8 + `state` 4 + `clip_id` 4 + `launch_beat` 8)。
const EVENT_BYTES: usize = 32;

/// 行の走行状態 1 つ。
///
/// `lane_id == 0` が**トラック行**、それ以外はそのトラックのオートメーションレーン行。
/// マスター行のレーンは `track_id == `[`crate::model::MASTER_TRACK_ID`]。
/// (`daw_audio::launcher::RowKey` / `daw_gui::launcher_time::RowId` と同じ規約)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LauncherRowState {
    pub track_id: u32,
    pub lane_id: u32,
    /// その瞬間の主導権。走行中のフォローアクションの遷移がここに出る。
    pub playback: RowPlayback,
    /// [`RowPlayback::Launcher`] のとき、そのセルを撃った song-absolute 拍
    /// (= 位相の原点)。他の状態では `0.0`。
    pub launch_beat: f64,
}

/// 走行状態の遷移 1 件 (内部表現 = ファイル表現)。
#[derive(Debug, Clone, Copy, PartialEq)]
struct Event {
    /// 遷移が起きる song 拍。**昇順**。
    beat: f64,
    row_key: u64,
    state: u32,
    clip_id: u32,
    launch_beat: f64,
}

/// `state` の符号化。shmem publish (`crate::audio_bridge::LAUNCHER_STATE_*`) とは
/// **別の名前空間**にしてある — sidecar はファイル形式なので、shmem 側の定数を
/// 後から動かしても既存ファイルが化けない。
const STATE_ARRANGER: u32 = 0;
const STATE_LAUNCHER: u32 = 1;
const STATE_STOPPED: u32 = 2;

fn encode_state(p: RowPlayback) -> (u32, u32) {
    match p {
        RowPlayback::Arranger => (STATE_ARRANGER, 0),
        RowPlayback::Launcher { clip_id } => (STATE_LAUNCHER, clip_id),
        RowPlayback::LauncherStopped => (STATE_STOPPED, 0),
    }
}

fn decode_state(state: u32, clip_id: u32) -> RowPlayback {
    match state {
        STATE_LAUNCHER => RowPlayback::Launcher { clip_id },
        STATE_STOPPED => RowPlayback::LauncherStopped,
        // 未知の値 (= 世代違いの子プロセスが書いた) はアレンジ扱い。
        _ => RowPlayback::Arranger,
    }
}

/// 書き出し 1 回ぶんの、行ごとの走行状態の遷移列。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LauncherSidecar {
    events: Vec<Event>,
}

impl LauncherSidecar {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 遷移を 1 件積む。**`beat` は昇順で渡すこと** (書き出しの走査順)。
    ///
    /// 同じ行の状態が変わっていない buffer では呼ばないこと — 呼んでも結果は
    /// 変わらないが、ファイルが buffer 数 × 行数まで膨らむ。
    pub fn push(&mut self, beat: f64, row: LauncherRowState) {
        let (state, clip_id) = encode_state(row.playback);
        self.events.push(Event {
            beat,
            row_key: (u64::from(row.track_id) << 32) | u64::from(row.lane_id),
            state,
            clip_id,
            launch_beat: row.launch_beat,
        });
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// 記録されている遷移の件数 (テスト / ログ用)。
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// 拍 `beat` の時点の **全行の状態**を `out` に詰める (行ごとに 1 件)。
    ///
    /// 「その行の、`beat` 以下で最後の遷移」を採る。`beat` が最初の遷移より手前
    /// (= 書き出し範囲の外) なら、その行はまだ現れないので `out` に入らない。
    ///
    /// 呼び側は各フレームで 1 回呼ぶ。遷移は発火したときにしか積まれないので
    /// 走査量は「実際に起きた遷移の数」で、buffer 数には比例しない。
    pub fn sample_at(&self, beat: f64, out: &mut Vec<LauncherRowState>) {
        out.clear();
        for e in &self.events {
            if e.beat > beat {
                // 昇順なので、ここから先は全部未来。
                break;
            }
            let row = LauncherRowState {
                #[allow(clippy::cast_possible_truncation)]
                track_id: (e.row_key >> 32) as u32,
                #[allow(clippy::cast_possible_truncation)]
                lane_id: (e.row_key & 0xFFFF_FFFF) as u32,
                playback: decode_state(e.state, e.clip_id),
                launch_beat: e.launch_beat,
            };
            match out.iter_mut().find(|r| {
                r.track_id == row.track_id && r.lane_id == row.lane_id
            }) {
                // 後の遷移が前を上書きする (= 最後の 1 件が残る)。
                Some(slot) => *slot = row,
                None => out.push(row),
            }
        }
    }

    /// WAV のパスから sidecar のパス (`foo.wav` → `foo.launcher`)。
    #[must_use]
    pub fn sidecar_path(wav: &Path) -> PathBuf {
        wav.with_extension("launcher")
    }

    pub fn write(&self, path: &Path) -> io::Result<()> {
        let mut f = io::BufWriter::new(std::fs::File::create(path)?);
        f.write_all(&MAGIC.to_le_bytes())?;
        f.write_all(&u32::try_from(self.events.len()).unwrap_or(u32::MAX).to_le_bytes())?;
        for e in &self.events {
            f.write_all(&e.beat.to_le_bytes())?;
            f.write_all(&e.row_key.to_le_bytes())?;
            f.write_all(&e.state.to_le_bytes())?;
            f.write_all(&e.clip_id.to_le_bytes())?;
            f.write_all(&e.launch_beat.to_le_bytes())?;
        }
        f.flush()
    }

    pub fn read(path: &Path) -> io::Result<Self> {
        let bytes = std::fs::read(path)?;
        let mut c = io::Cursor::new(bytes.as_slice());
        let mut b4 = [0u8; 4];
        let mut b8 = [0u8; 8];
        c.read_exact(&mut b4)?;
        if u32::from_le_bytes(b4) != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "launcher: bad magic"));
        }
        c.read_exact(&mut b4)?;
        let n = u32::from_le_bytes(b4) as usize;
        // 壊れた / 途中で切れたファイルで巨大な確保をしない (`n` は信頼境界の値)。
        let cap = n.min(bytes.len().saturating_sub(8) / EVENT_BYTES);
        let mut events = Vec::with_capacity(cap);
        for _ in 0..n {
            c.read_exact(&mut b8)?;
            let beat = f64::from_le_bytes(b8);
            c.read_exact(&mut b8)?;
            let row_key = u64::from_le_bytes(b8);
            c.read_exact(&mut b4)?;
            let state = u32::from_le_bytes(b4);
            c.read_exact(&mut b4)?;
            let clip_id = u32::from_le_bytes(b4);
            c.read_exact(&mut b8)?;
            let launch_beat = f64::from_le_bytes(b8);
            events.push(Event { beat, row_key, state, clip_id, launch_beat });
        }
        Ok(Self { events })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(track_id: u32, playback: RowPlayback, launch_beat: f64) -> LauncherRowState {
        LauncherRowState { track_id, lane_id: 0, playback, launch_beat }
    }

    /// 区分定数の意味論 — 「その行の、`beat` 以下で最後の遷移」。行が混ざる並びで
    /// 上書きが行ごとに閉じていることまで見る (ここが崩れると、動画書き出しで
    /// 別の行のセルが映る)。
    #[test]
    fn 行ごとに最後の遷移が残る() {
        let mut s = LauncherSidecar::new();
        s.push(0.0, row(1, RowPlayback::Launcher { clip_id: 10 }, 0.0));
        s.push(0.0, row(2, RowPlayback::Arranger, 0.0));
        s.push(4.0, row(2, RowPlayback::Launcher { clip_id: 20 }, 4.0));
        s.push(8.0, row(1, RowPlayback::Launcher { clip_id: 11 }, 8.0));
        s.push(12.0, row(1, RowPlayback::LauncherStopped, 0.0));

        let mut out = Vec::new();
        s.sample_at(-1.0, &mut out);
        assert!(out.is_empty(), "最初の遷移より手前は「まだ何も起きていない」");

        s.sample_at(0.0, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], row(1, RowPlayback::Launcher { clip_id: 10 }, 0.0));
        assert_eq!(out[1], row(2, RowPlayback::Arranger, 0.0));

        s.sample_at(7.9, &mut out);
        assert_eq!(out[0].playback, RowPlayback::Launcher { clip_id: 10 }, "track 1 は据え置き");
        assert_eq!(out[1], row(2, RowPlayback::Launcher { clip_id: 20 }, 4.0));

        s.sample_at(100.0, &mut out);
        assert_eq!(out[0].playback, RowPlayback::LauncherStopped);
        assert_eq!(out[0].launch_beat, 0.0);
        assert_eq!(out[1].playback, RowPlayback::Launcher { clip_id: 20 });
    }

    #[test]
    fn 往復で同じ内容になる() {
        let dir = std::env::temp_dir();
        let wav = dir.join("daw01_launcher_sidecar_roundtrip.wav");
        let path = LauncherSidecar::sidecar_path(&wav);
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("launcher"));
        let mut s = LauncherSidecar::new();
        s.push(0.0, row(1, RowPlayback::Launcher { clip_id: 10 }, 0.25));
        s.push(2.5, LauncherRowState {
            track_id: 3,
            lane_id: 7,
            playback: RowPlayback::LauncherStopped,
            launch_beat: 0.0,
        });
        s.write(&path).unwrap();
        let back = LauncherSidecar::read(&path).unwrap();
        assert_eq!(s, back);
        let _ = std::fs::remove_file(&path);
    }
}
