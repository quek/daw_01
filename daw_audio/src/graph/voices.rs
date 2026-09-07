//! r.md #117 (`docs/plan_per_note_modulation.md` §2): plugin instance ごとの **ボイス表**。
//!
//! その plugin が受けた note-on / note-off を `run_plugin` が記録し、 per-note 変調の評価点
//! (`fill_pd_param_events`) が「鳴っているノート = 起点 (拍 / 秒) + note-off の秒」 を読む。
//! 終了は plugin の `NoteEnd` 通知 (CLAP) / note-off から [`VOICE_TAIL_SECS`] 経過 (VST3 など
//! 通知が無い plugin) / 同じ note_id の再 note-on。 容量 [`MAX_VOICES`]、 溢れたら最古を捨てる。
//! RT 規約: 固定長配列のみ (確保・ロック・I/O なし)。 再 compile 跨ぎは device id で移送。

/// 1 plugin あたり同時に追うノート数。
pub const MAX_VOICES: usize = 64;
/// note-off 後、 `NoteEnd` が来ないときにボイスを捨てるまでの秒 (リリースの余韻ぶん)。
pub const VOICE_TAIL_SECS: f64 = 10.0;

/// 鳴っているノート 1 つ。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Voice {
    pub note_id: u32,
    pub key: u8,
    pub channel: u8,
    /// note-on の曲位置 (拍 / 絶対秒) = per-note ソースの起点。
    pub on_beat: f64,
    pub on_secs: f64,
    /// note-off の絶対秒 (`None` = 押している)。
    pub off_secs: Option<f64>,
    pub velocity: f32,
    /// 追加順 (奪う候補 = 最小)。
    seq: u64,
}

/// plugin 1 つぶんのボイス表。
pub struct VoiceTable {
    pub device_id: u64,
    voices: [Voice; MAX_VOICES],
    len: usize,
    next_seq: u64,
}

const EMPTY_VOICE: Voice = Voice {
    note_id: 0,
    key: 0,
    channel: 0,
    on_beat: 0.0,
    on_secs: 0.0,
    off_secs: None,
    velocity: 0.0,
    seq: 0,
};

impl VoiceTable {
    pub fn new(device_id: u64) -> Self {
        Self { device_id, voices: [EMPTY_VOICE; MAX_VOICES], len: 0, next_seq: 0 }
    }

    /// 鳴っているノート (追加順)。
    pub fn iter(&self) -> impl Iterator<Item = &Voice> {
        self.voices[..self.len].iter()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// note-on: 同じ `note_id` が居れば置き換え (再トリガ)、 満杯なら最古を捨てる。
    pub fn note_on(&mut self, note_id: u32, key: u8, channel: u8, on_beat: f64, on_secs: f64, velocity: f32) {
        let v = Voice { note_id, key, channel, on_beat, on_secs, off_secs: None, velocity, seq: self.next_seq };
        self.next_seq = self.next_seq.wrapping_add(1);
        if let Some(i) = self.voices[..self.len].iter().position(|x| x.note_id == note_id && x.key == key) {
            self.voices[i] = v;
            return;
        }
        if self.len == MAX_VOICES {
            let oldest = (0..self.len).min_by_key(|&i| self.voices[i].seq).unwrap_or(0);
            self.remove_at(oldest);
        }
        self.voices[self.len] = v;
        self.len += 1;
    }

    /// note-off: `(note_id, key)` で引き、 無ければ `key` だけで (id を知らない送り手への保険。
    /// 停止時の flush も note-on と同じ id を運ぶ)。
    /// 押していない (既に離した) ものには効かない。
    pub fn note_off(&mut self, note_id: u32, key: u8, off_secs: f64) {
        let held = &self.voices[..self.len];
        let idx = held
            .iter()
            .position(|x| x.off_secs.is_none() && x.note_id == note_id && x.key == key)
            .or_else(|| held.iter().position(|x| x.off_secs.is_none() && x.key == key));
        if let Some(i) = idx {
            self.voices[i].off_secs = Some(off_secs);
        }
    }

    /// plugin の `NoteEnd`: ボイスを表から外す。
    pub fn note_end(&mut self, note_id: u32, key: u8) {
        let held = &self.voices[..self.len];
        let idx = held
            .iter()
            .position(|x| x.note_id == note_id && x.key == key)
            .or_else(|| held.iter().position(|x| x.off_secs.is_some() && x.key == key));
        if let Some(i) = idx {
            self.remove_at(i);
        }
    }

    /// note-off から [`VOICE_TAIL_SECS`] 過ぎたボイスを捨てる (毎 buffer)。
    pub fn expire(&mut self, now_secs: f64) {
        let mut i = 0;
        while i < self.len {
            match self.voices[i].off_secs {
                Some(off) if now_secs - off > VOICE_TAIL_SECS => self.remove_at(i),
                _ => i += 1,
            }
        }
    }

    fn remove_at(&mut self, i: usize) {
        self.len -= 1;
        self.voices[i] = self.voices[self.len];
    }

    /// 再 compile 跨ぎの移送 (RT 上、 コピーのみ)。
    pub fn adopt_state_from(&mut self, old: &Self) {
        self.voices = old.voices;
        self.len = old.len;
        self.next_seq = old.next_seq;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_on_off_end_and_expire_keep_the_table_consistent() {
        let mut t = VoiceTable::new(7);
        t.note_on(1, 60, 0, 0.0, 0.0, 1.0);
        t.note_on(2, 64, 0, 0.5, 0.25, 0.5);
        assert_eq!(t.len(), 2);
        // 再トリガは置き換え (起点が更新される)。
        t.note_on(1, 60, 0, 1.0, 0.5, 1.0);
        assert_eq!(t.len(), 2);
        assert_eq!(t.iter().find(|v| v.note_id == 1).unwrap().on_beat, 1.0);
        // note-off は起点を残して off_secs を記録。 flush (note_id 0) は key で引く。
        t.note_off(0, 64, 1.0);
        assert_eq!(t.iter().find(|v| v.note_id == 2).unwrap().off_secs, Some(1.0));
        t.note_off(0, 64, 2.0);
        assert_eq!(t.iter().find(|v| v.note_id == 2).unwrap().off_secs, Some(1.0), "離した後は上書きしない");
        // NoteEnd で消える。 余韻の期限でも消える。
        t.note_end(2, 64);
        assert_eq!(t.len(), 1);
        t.note_off(1, 60, 3.0);
        t.expire(3.0 + VOICE_TAIL_SECS);
        assert_eq!(t.len(), 1, "期限ちょうどは残る");
        t.expire(3.0 + VOICE_TAIL_SECS + 0.01);
        assert!(t.is_empty());
    }

    #[test]
    fn full_table_steals_the_oldest_voice() {
        let mut t = VoiceTable::new(1);
        for i in 0..MAX_VOICES as u32 {
            t.note_on(100 + i, (i % 128) as u8, 0, f64::from(i), 0.0, 1.0);
        }
        t.note_on(999, 1, 0, 99.0, 0.0, 1.0);
        assert_eq!(t.len(), MAX_VOICES);
        assert!(t.iter().all(|v| v.note_id != 100), "最古 (100) が奪われた");
        assert!(t.iter().any(|v| v.note_id == 999));
    }
}
