//! Plugin format tag shared between host-side crates.
//!
//! CLAP and VST3 descriptors have disjoint identifier schemes (CLAP uses
//! reverse-DNS strings, VST3 uses 16-byte UUIDs rendered as hex), so an
//! explicit tag makes sure we can route `SetSlotPlugin` and project-file
//! entries to the right backend without relying on heuristics.

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode, Serialize, Deserialize,
)]
pub enum PluginFormat {
    Clap,
    Vst3,
}

impl Default for PluginFormat {
    /// Projects / plugin_db entries saved before VST3 support was added do
    /// not include a `format` field; serde's `default` lands here.
    fn default() -> Self {
        Self::Clap
    }
}

impl PluginFormat {
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            PluginFormat::Clap => "CLAP",
            PluginFormat::Vst3 => "VST3",
        }
    }

    /// File extension used by plugins of this format on Windows.
    /// VST3 may be a single DLL or a bundle directory; both end in `.vst3`.
    #[allow(dead_code)]
    pub fn extension(self) -> &'static str {
        match self {
            PluginFormat::Clap => "clap",
            PluginFormat::Vst3 => "vst3",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bincode_roundtrip<T>(v: &T) -> T
    where
        T: Encode + Decode<()>,
    {
        let cfg = bincode::config::standard();
        let bytes = bincode::encode_to_vec(v, cfg).unwrap();
        let (decoded, _) = bincode::decode_from_slice(&bytes, cfg).unwrap();
        decoded
    }

    #[test]
    fn default_is_clap() {
        assert_eq!(PluginFormat::default(), PluginFormat::Clap);
    }

    #[test]
    fn bincode_roundtrip_both_variants() {
        assert_eq!(bincode_roundtrip(&PluginFormat::Clap), PluginFormat::Clap);
        assert_eq!(bincode_roundtrip(&PluginFormat::Vst3), PluginFormat::Vst3);
    }

    #[test]
    fn json_default_when_missing() {
        // Simulates an old plugin_db.json entry with no `format` field.
        #[derive(Serialize, Deserialize)]
        struct Wrapper {
            #[serde(default)]
            format: PluginFormat,
        }
        let w: Wrapper = serde_json::from_str("{}").unwrap();
        assert_eq!(w.format, PluginFormat::Clap);
    }
}
