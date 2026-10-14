//! r.md #138: plugin が申告する鍵盤ごとの名前 (Kick / Snare …) を、ピアノロールが描く形
//! ([`KeyName`] の key 昇順・重複なし) に揃える。整形は format に依らない 1 本 ([`normalize`])。
//! CLAP の取得 ([`from_clap`]) もここに置く (`clap_plugin.rs` のサイズ budget のため)。VST3 の取得は `vst3_units.rs`。

use clap_sys::ext::note_name::{CLAP_EXT_NOTE_NAME, clap_note_name, clap_plugin_note_name};
use clap_sys::plugin::clap_plugin;
use common::protocol::KeyName;

/// plugin が返した 1 件。`port` / `channel` の `-1` は「全 port / 全 channel」
/// (CLAP `ext/note-name.h` の `clap_note_name`)。
pub struct RawKeyName {
    pub port: i16,
    pub key: i16,
    pub channel: i16,
    pub name: String,
}

/// CLAP の `clap.note-name` (`ext/note-name.h`) の `count` / `get` で読む [main-thread]。
/// 拡張が無い / plugin が null なら空。
pub fn from_clap(plugin: *const clap_plugin) -> Vec<KeyName> {
    if plugin.is_null() {
        return Vec::new();
    }
    let Some(get_ext) = (unsafe { (*plugin).get_extension }) else {
        return Vec::new();
    };
    let ext = unsafe { get_ext(plugin, CLAP_EXT_NOTE_NAME.as_ptr()) } as *const clap_plugin_note_name;
    let Some(ext) = (unsafe { ext.as_ref() }) else {
        return Vec::new();
    };
    let (Some(count_fn), Some(get_fn)) = (ext.count, ext.get) else {
        return Vec::new();
    };
    // count は untrusted。鍵盤 128 × channel 16 × 数 port を十分に覆う上限。
    const MAX_NOTE_NAMES: u32 = 8192;
    let count = unsafe { count_fn(plugin) }.min(MAX_NOTE_NAMES);
    let raw = (0..count).filter_map(|i| {
        // SAFETY: clap_note_name is plain old data; zeroed start is legal.
        let mut nn: clap_note_name = unsafe { std::mem::zeroed() };
        if !unsafe { get_fn(plugin, i, &raw mut nn) } {
            return None;
        }
        Some(RawKeyName {
            port: nn.port,
            key: nn.key,
            channel: nn.channel,
            name: crate::vst3_scan::c_array_to_string(&nn.name),
        })
    });
    normalize(raw)
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
