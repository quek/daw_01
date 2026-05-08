//! Plugin format tag shared between host-side crates.
//!
//! CLAP and VST3 descriptors have disjoint identifier schemes (CLAP uses
//! reverse-DNS strings, VST3 uses 16-byte UUIDs rendered as hex), so an
//! explicit tag makes sure we can route `SetSlotPlugin` and project-file
//! entries to the right backend without relying on heuristics.
//!
//! `Builtin` is reserved for daw_01-bundled instrument / FX (PR-V1, see
//! `docs/plan_voicevox_synth.md`). The "path" of a Builtin entry is a
//! URI like `builtin://voicevox` — it is never opened as a file; the
//! plugin host parses it and dispatches to a Rust constructor in
//! `daw_plugin_host::builtin`.

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode, Serialize, Deserialize,
)]
pub enum PluginFormat {
    Clap,
    Vst3,
    /// daw_01-bundled instrument / FX. Identifier is a URI like
    /// `builtin://voicevox`; the host crate's `builtin` module owns the
    /// Rust implementation. State save / restore goes through the same
    /// `LoadedPlugin::state_save` / `state_load` plumbing as external
    /// CLAP / VST3 plugins, so projects survive crate-level refactors.
    Builtin,
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
            PluginFormat::Builtin => "Builtin",
        }
    }

    /// File extension used by plugins of this format on Windows.
    /// VST3 may be a single DLL or a bundle directory; both end in `.vst3`.
    /// `Builtin` plugins are not file-backed, so this returns an empty
    /// string — callers should special-case `Builtin` before reaching for
    /// the extension.
    #[allow(dead_code)]
    pub fn extension(self) -> &'static str {
        match self {
            PluginFormat::Clap => "clap",
            PluginFormat::Vst3 => "vst3",
            PluginFormat::Builtin => "",
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
