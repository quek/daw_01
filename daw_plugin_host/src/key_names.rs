//! r.md #138: plugin が申告する鍵盤ごとの名前 (Kick / Snare …) を、ピアノロールが描く形
//! ([`KeyName`] の key 昇順・重複なし) に揃える。CLAP / VST3 の取得部はそれぞれの
//! `LoadedPlugin::key_names` にあり、ここは format に依らない整形だけを持つ。

use common::protocol::KeyName;

/// plugin が返した 1 件。`port` / `channel` の `-1` は「全 port / 全 channel」
/// (CLAP `ext/note-name.h` の `clap_note_name`)。
pub struct RawKeyName {
    pub port: i16,
    pub key: i16,
    pub channel: i16,
    pub name: String,
}

/// daw_01 が音源へ note を送るのは port 0 / channel 0 だけなので、そこに効く名前だけを採る。
/// 同じ key に複数あれば、port / channel を名指しした方を `-1` (全部) より優先し、
/// 同格なら先に来た方を採る。key `-1` (全 key) と空の名前は鍵盤ごとの名前ではないので捨てる。
pub fn normalize(raw: impl IntoIterator<Item = RawKeyName>) -> Vec<KeyName> {
    let mut best: [Option<(u8, String)>; 128] = std::array::from_fn(|_| None);
    for r in raw {
        if !matches!(r.port, -1 | 0) || !matches!(r.channel, -1 | 0) {
            continue;
        }
        let Ok(key) = u8::try_from(r.key) else { continue };
        let Some(slot) = best.get_mut(usize::from(key)) else { continue };
        let name = r.name.trim();
        if name.is_empty() {
            continue;
        }
        let rank = u8::from(r.port == -1) + u8::from(r.channel == -1);
        if slot.as_ref().is_none_or(|(have, _)| rank < *have) {
            *slot = Some((rank, name.to_string()));
        }
    }
    best.into_iter()
        .enumerate()
        .filter_map(|(key, slot)| {
            slot.map(|(_, name)| KeyName { key: u8::try_from(key).unwrap_or(u8::MAX), name })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(port: i16, key: i16, channel: i16, name: &str) -> RawKeyName {
        RawKeyName { port, key, channel, name: name.to_string() }
    }

    #[test]
    fn keeps_port0_channel0_names_sorted_by_key() {
        let got = normalize([
            raw(-1, 38, -1, "Snare"),
            raw(0, 36, 0, "Kick"),
            raw(1, 40, 0, "other port"),
            raw(0, 41, 9, "other channel"),
            raw(0, -1, 0, "every key"),
            raw(0, 200, 0, "out of range"),
            raw(0, 42, 0, "  "),
        ]);
        assert_eq!(
            got,
            vec![
                KeyName { key: 36, name: "Kick".into() },
                KeyName { key: 38, name: "Snare".into() },
            ]
        );
    }

    #[test]
    fn specific_port_channel_beats_wildcard() {
        let got = normalize([
            raw(-1, 36, -1, "Generic"),
            raw(0, 36, 0, "Kick"),
            raw(0, 36, 0, "Kick 2"),
        ]);
        assert_eq!(got, vec![KeyName { key: 36, name: "Kick".into() }]);
    }
}
