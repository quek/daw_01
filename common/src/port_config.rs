//! FIXME #29: プラグインの port 構成。 capability（生成器/音源/エフェクト）の
//! **Single Source of Truth** となる 3 bool を運ぶ。
//!
//! probe subprocess（`daw_plugin_host --probe-vst3` / `--probe-clap`）が
//! [`PortConfig::to_line`] で stdout に 1 行出力し、 rescan 側（`daw_gui`）が
//! [`PortConfig::parse_line`] で復元して `PluginEntry` の 3 bool に格納する。
//! VST3 / CLAP どちらの probe も同じ型・同じ行形式を使う（DRY）。

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortConfig {
    /// note/event 入力ポートを持つ（MIDI/note を受け取れる）。
    pub has_note_input: bool,
    /// note/event 出力ポートを持つ = **生成器になれる**。
    pub has_note_output: bool,
    /// audio 出力ポートを持つ = **音源になれる**。
    pub has_audio_output: bool,
}

impl PortConfig {
    /// probe subprocess の stdout 1 行へ整形。 [`PortConfig::parse_line`] と対。
    #[must_use]
    pub fn to_line(&self) -> String {
        format!(
            "note_in={} note_out={} audio_out={}",
            self.has_note_input, self.has_note_output, self.has_audio_output
        )
    }

    /// probe subprocess の stdout から復元。 3 キーが揃わない / 値が `true`/`false`
    /// でない行は `None`（呼び元は scan-time の暫定値を残す fallback）。
    #[must_use]
    pub fn parse_line(s: &str) -> Option<PortConfig> {
        let mut cfg = PortConfig::default();
        let mut seen = 0u8;
        for tok in s.split_whitespace() {
            let (k, v) = tok.split_once('=')?;
            let b = match v {
                "true" => true,
                "false" => false,
                _ => return None,
            };
            match k {
                "note_in" => {
                    cfg.has_note_input = b;
                    seen |= 1;
                }
                "note_out" => {
                    cfg.has_note_output = b;
                    seen |= 2;
                }
                "audio_out" => {
                    cfg.has_audio_output = b;
                    seen |= 4;
                }
                _ => {}
            }
        }
        (seen == 0b111).then_some(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::PortConfig;

    #[test]
    fn round_trip() {
        for cfg in [
            PortConfig { has_note_input: true, has_note_output: true, has_audio_output: true },
            PortConfig { has_note_input: true, has_note_output: false, has_audio_output: true },
            PortConfig { has_note_input: true, has_note_output: true, has_audio_output: false },
            PortConfig::default(),
        ] {
            assert_eq!(PortConfig::parse_line(&cfg.to_line()), Some(cfg));
        }
    }

    #[test]
    fn parse_tolerates_surrounding_log_noise_only_on_the_line() {
        // 1 行に 3 キー揃っていれば順不同で復元できる。
        let cfg = PortConfig::parse_line("audio_out=false note_out=true note_in=true").unwrap();
        assert_eq!(
            cfg,
            PortConfig { has_note_input: true, has_note_output: true, has_audio_output: false }
        );
    }

    #[test]
    fn parse_rejects_incomplete_or_malformed() {
        assert_eq!(PortConfig::parse_line("note_in=true note_out=true"), None); // audio_out 欠落
        assert_eq!(PortConfig::parse_line("note_in=yes note_out=true audio_out=true"), None); // 値不正
        assert_eq!(PortConfig::parse_line(""), None);
        assert_eq!(PortConfig::parse_line("garbage"), None);
    }
}
