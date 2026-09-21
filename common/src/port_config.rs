//! プラグインの port 構成。 capability（生成器/音源/エフェクト/映像効果）の
//! **Single Source of Truth** となる bool 群を運ぶ。
//!
//! probe subprocess（`daw_plugin_host --probe-vst3` / `--probe-clap`）が
//! [`PortConfig::to_line`] で stdout に 1 行出力し、 rescan 側（`daw_gui`）が
//! [`PortConfig::parse_line`] で復元して `PluginEntry` の bool に格納する。
//! VST3 / CLAP どちらの probe も同じ型・同じ行形式を使う（DRY）。
//!
//! 内蔵映像効果用に `has_video_input` /
//! `has_video_output` を追加。映像 device は GUI 描画パスで処理されるため、
//! audio engine / plugin host から見ると `slot_to_plugin_id` 未登録の index で、
//! `process_track_owned` がそのまま skip する (= 音声バスを素通り)。

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    bincode::Encode,
    bincode::Decode,
)]
pub struct PortConfig {
    /// note/event 入力ポートを持つ（MIDI/note を受け取れる）。
    pub has_note_input: bool,
    /// note/event 出力ポートを持つ = **生成器になれる**。
    pub has_note_output: bool,
    /// audio 出力ポートを持つ。
    pub has_audio_output: bool,
    /// audio 入力ポートを持つ = **audio を処理できる (= エフェクト)**。
    /// 音源 (synth) と audio エフェクトの区別に必須: 実 plugin は note_in を
    /// 持つ audio エフェクト (MIDI 制御付き) が多く、note 系 3 bool だけでは
    /// 「audio を生成する音源」と「audio を加工するエフェクト」を区別できない。
    /// audio_in を持たず audio_out を持つ = 音源、audio_in を持つ = エフェクト。
    #[serde(default)]
    pub has_audio_input: bool,
    /// 映像 (RGBA テクスチャ) 入力ポートを持つ。内蔵映像効果
    /// (`builtin.video.*`) はこれと [`has_video_output`](Self::has_video_output)
    /// の両方を立て、audio/note 系は全て false にする。
    #[serde(default)]
    pub has_video_input: bool,
    /// 映像 (RGBA テクスチャ) 出力ポートを持つ。
    #[serde(default)]
    pub has_video_output: bool,
}

impl PortConfig {
    /// 映像 device か (映像 in/out のいずれかを持つ)。GUI 描画パスで
    /// 処理する device の判定 (audio engine / plugin host はこれを load しない)。
    #[must_use]
    pub fn is_video(&self) -> bool {
        self.has_video_input || self.has_video_output
    }

    /// port 構成が「まだ解決されていない」 (= 全 false の初期値)。
    #[must_use]
    pub fn is_unresolved(&self) -> bool {
        *self == PortConfig::default()
    }

    /// device が持つべき port 構成を決める **唯一の規則**。
    ///
    /// - 既に解決済み (= どれかの port が true) ならそれを保つ。 保存済み project /
    ///   picker で挿した device の構成であり、 plugin DB が未 scan・scan 失敗・
    ///   plugin 未インストールでも壊れないための durable な値。
    /// - 未解決 (全 false) のときだけ plugin DB から導出する。 DB にも無ければ
    ///   未解決のまま (= 役割導出は後続の rescan に委ねる)。
    ///
    /// この規則を 2 箇所で別々に書いていたのが r.md #9 の一因だった:
    /// `SlotPluginLoaded` の backfill だけが DB を優先していたため、 DB と保存値が
    /// 食い違う環境ではプラグインの load 応答が届くたびに `PluginInstance` が
    /// 書き換わり、 **保存済みプロジェクトを開いただけで `*`** が付いた。
    #[must_use]
    pub fn resolve(existing: PortConfig, from_db: Option<PortConfig>) -> PortConfig {
        if existing.is_unresolved() {
            from_db.unwrap_or(existing)
        } else {
            existing
        }
    }
}

/// probe subprocess (`daw_plugin_host --probe-vst3` / `--probe-clap`) が 1 回の
/// instantiate で調べた、**プラグインのクラス単位の capability**。
///
/// probe は stdout に [`PluginProbe::to_line`] で 1 行出し、rescan 側 (`daw_gui`) が
/// [`PluginProbe::parse_line`] で復元して [`crate::plugin_db::PluginEntry`] に格納する。
/// VST3 / CLAP どちらの probe も同じ型・同じ行形式を使う。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PluginProbe {
    pub ports: PortConfig,
    /// 埋め込みエディタ窓 (Win32 HWND) を持つか。
    ///
    /// **instance ごとに調べてはいけない。** VST3 で答えを得る唯一の手段は
    /// `IEditController::createView("editor")` = **エディタ実体の生成** で、実測
    /// (Analog Lab V、2026-09-21、`daw_plugin_host --load-bench ... gui` の有無で A/B):
    /// **1 本あたり +130 MiB / +860 ms**。40 本の曲なら 5.2 GB と 34 秒を、エディタ窓を
    /// 一度も開かなくても払うことになる。
    ///
    /// これはプラグインの**クラス**の性質なので、scan 時に 1 回だけ調べて plugin DB に
    /// 持つ (Reaper が `reaper-vstplugins64.ini` でやっているのと同じ)。load 経路
    /// (`set_slot_plugin`) からは問い合わせない。
    pub has_embedded_gui: bool,
}

impl PluginProbe {
    /// probe subprocess の stdout 1 行へ整形。 [`PluginProbe::parse_line`] と対。
    #[must_use]
    pub fn to_line(&self) -> String {
        format!(
            "note_in={} note_out={} audio_out={} audio_in={} video_in={} video_out={} embed_gui={}",
            self.ports.has_note_input,
            self.ports.has_note_output,
            self.ports.has_audio_output,
            self.ports.has_audio_input,
            self.ports.has_video_input,
            self.ports.has_video_output,
            self.has_embedded_gui
        )
    }

    /// probe subprocess の stdout から復元。 7 キーが揃わない / 値が `true`/`false`
    /// でない行は `None`（呼び元は scan-time の暫定値を残す fallback）。旧 4-キー /
    /// 6-キー行は `None` を返すので `PORT_PROBE_VERSION` bump で再 probe される。
    #[must_use]
    pub fn parse_line(s: &str) -> Option<PluginProbe> {
        let mut probe = PluginProbe::default();
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
                    probe.ports.has_note_input = b;
                    seen |= 1;
                }
                "note_out" => {
                    probe.ports.has_note_output = b;
                    seen |= 2;
                }
                "audio_out" => {
                    probe.ports.has_audio_output = b;
                    seen |= 4;
                }
                "audio_in" => {
                    probe.ports.has_audio_input = b;
                    seen |= 8;
                }
                "video_in" => {
                    probe.ports.has_video_input = b;
                    seen |= 16;
                }
                "video_out" => {
                    probe.ports.has_video_output = b;
                    seen |= 32;
                }
                "embed_gui" => {
                    probe.has_embedded_gui = b;
                    seen |= 64;
                }
                _ => {}
            }
        }
        (seen == 0b111_1111).then_some(probe)
    }
}

#[cfg(test)]
mod tests {
    use super::{PluginProbe, PortConfig};

    fn probe(ports: PortConfig, has_embedded_gui: bool) -> PluginProbe {
        PluginProbe { ports, has_embedded_gui }
    }

    #[test]
    fn round_trip() {
        for p in [
            probe(
                PortConfig {
                    has_note_input: true,
                    has_note_output: true,
                    has_audio_output: true,
                    has_audio_input: true,
                    has_video_input: false,
                    has_video_output: false,
                },
                true,
            ),
            probe(
                PortConfig {
                    has_note_input: true,
                    has_note_output: false,
                    has_audio_output: true,
                    has_audio_input: false,
                    has_video_input: false,
                    has_video_output: false,
                },
                false,
            ),
            // 純映像 device (note/audio 全 false、video in/out)。
            probe(
                PortConfig {
                    has_note_input: false,
                    has_note_output: false,
                    has_audio_output: false,
                    has_audio_input: false,
                    has_video_input: true,
                    has_video_output: true,
                },
                false,
            ),
            PluginProbe::default(),
        ] {
            assert_eq!(PluginProbe::parse_line(&p.to_line()), Some(p));
        }
    }

    #[test]
    fn parse_is_order_independent() {
        // 1 行に 7 キー揃っていれば順不同で復元できる。
        let p = PluginProbe::parse_line(
            "embed_gui=true video_out=true audio_in=true audio_out=false note_out=true note_in=true video_in=false",
        )
        .unwrap();
        assert_eq!(
            p,
            probe(
                PortConfig {
                    has_note_input: true,
                    has_note_output: true,
                    has_audio_output: false,
                    has_audio_input: true,
                    has_video_input: false,
                    has_video_output: true,
                },
                true,
            )
        );
    }

    #[test]
    fn parse_rejects_incomplete_or_malformed() {
        assert_eq!(PluginProbe::parse_line("note_in=true note_out=true"), None); // audio/video 欠落
        // 旧 6-キー行 (embed_gui 無し) は None。 PORT_PROBE_VERSION bump で再 probe される。
        assert_eq!(
            PluginProbe::parse_line(
                "note_in=true note_out=true audio_out=true audio_in=false video_in=false video_out=false"
            ),
            None
        );
        assert_eq!(
            PluginProbe::parse_line("note_in=yes note_out=true audio_out=true"),
            None
        ); // 値不正
        assert_eq!(PluginProbe::parse_line(""), None);
        assert_eq!(PluginProbe::parse_line("garbage"), None);
    }
}
