//! r.md #138: 音源プラグインが申告する鍵盤ごとの名前 (Kick / Snare …) の、ピアノロールが
//! 描く形。plugin host の `PluginEvent::PluginKeyNames` を受けて daw_gui が device ごとに
//! 持つ (`ProjectIpc::plugin_key_names`、session-only — Song には載せない)。

use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use common::protocol::KeyName;

/// 名前の列の左端 (鍵盤左端から)。左側は既存の C / root ラベル (`C4` 等) の場所。
pub(super) const NAME_COLUMN_X: f32 = 30.0;
/// 名前の列の右余白。
const NAME_RIGHT_PAD: f32 = 6.0;
/// 名前で広げる鍵盤幅の上限。長い名前は鍵盤の右端で切る。
const MAX_KEYBOARD_W: f32 = 200.0;

#[derive(Debug)]
pub struct KeyNames {
    /// index = MIDI key (0..=127)。
    names: Vec<Option<Box<str>>>,
    /// 中身の hash (ピアノロールの背景 cache の key に入れる)。
    hash: u64,
    /// 最も長い名前の描画幅 (px)。font size は固定なので 1 回測れば足りる。
    max_name_w: OnceLock<f32>,
}

impl KeyNames {
    /// host の送った表から作る。名前が 1 つも無ければ `None` (= 今の鍵盤のまま)。
    #[must_use]
    pub fn from_wire(list: Vec<KeyName>) -> Option<Self> {
        let mut names: Vec<Option<Box<str>>> = vec![None; 128];
        let mut any = false;
        for KeyName { key, name } in list {
            if let Some(slot) = names.get_mut(usize::from(key)) {
                *slot = Some(name.into_boxed_str());
                any = true;
            }
        }
        if !any {
            return None;
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        names.hash(&mut h);
        Some(Self { names, hash: h.finish(), max_name_w: OnceLock::new() })
    }

    #[must_use]
    pub fn get(&self, key: u8) -> Option<&str> {
        self.names.get(usize::from(key))?.as_deref()
    }

    #[must_use]
    pub(super) fn cache_key(&self) -> u64 {
        self.hash
    }

    /// 名前が読める鍵盤幅。`base` (名前が無いときの幅) より狭くはしない。
    pub(super) fn keyboard_w(&self, base: f32, measure: impl FnOnce(&Self) -> f32) -> f32 {
        let w = *self.max_name_w.get_or_init(|| measure(self));
        (NAME_COLUMN_X + w + NAME_RIGHT_PAD).clamp(base, MAX_KEYBOARD_W.max(base))
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &str> {
        self.names.iter().filter_map(|n| n.as_deref())
    }
}
