//! VOICEVOX 内蔵 instrument plugin (`docs/plan_voicevox_synth.md` PR-V2)。
//!
//! ## PR-V2.1 (本コミット): skeleton のみ
//!
//! 現状はまだ **process() で無音を返すだけ**。 plugin slot に load して
//! state save / restore できる、 + speaker_id / style_name の plugin
//! parameter を保持できる、 までを実装。 これにより:
//!
//! - PR-V3 (project file migration: `InstrumentSource::Vocal` → builtin
//!   plugin slot) で「移行先の slot」 が用意できる
//! - 既存 vocal block を残したままで PR-V2 を段階開発できる (= 機能復旧
//!   優先 / Single Source of Truth 原則)
//!
//! HTTP synthesis / 歌詞バッファ受け渡し / cache / process integration は
//! 後続 PR (PR-V2.2 〜 V2.5) で:
//!
//! - **PR-V2.2**: 歌詞付き MIDI events を builtin plugin に渡す host API
//!   (= LoadedPlugin trait 拡張 or 専用 sidecar)。 規格 (CLAP
//!   `NoteExpression` / VST3 `NoteExpression`) との互換は最後 (PR-V5)。
//! - **PR-V2.3**: `common::voicevox::synthesize_song` 経由の bulk synth
//!   + `common::voicevox_cache` の per-note cache 統合
//! - **PR-V2.4**: process() で cache から該当 note の audio を時間軸に
//!   合わせて mix
//! - **PR-V2.5**: state save / restore で cache を bincode embed
//!
//! ## 設計選択
//!
//! - **stateful struct**: `Vec<f32>` の output buffer + parameters は
//!   `&mut self` 直保持。 plugin host は `Box<dyn LoadedPlugin>` で
//!   move したあと audio thread に raw pointer snapshot で渡す
//!   (`PluginPtr` 経由)、 通常の CLAP / VST3 plugin と同 lifecycle
//! - **state schema は forward-compatible**: 現状は speaker_id +
//!   style_name のみだが、 後続 PR で歌詞 cache (= per-note WAV
//!   buffer) を埋め込む。 bincode v2 + struct field 追加は backward
//!   compat なので migration なしで読める

use anyhow::{Context, Result, bail};
use bincode::{Decode, Encode};
use common::plugin_db::BUILTIN_ID_VOICEVOX;
use common::plugin_format::PluginFormat;
use common::protocol::RenderMode;

use crate::plugin_instance::{AuxInputBuf, LoadedPlugin, TimedNoteEvent};

/// `VoicevoxBuiltin` の persistent state。 project file に bincode で
/// 埋め込む。 PR-V2.5 で歌詞 / cache フィールドを追加予定 (= bincode の
/// struct 拡張は backward compatible)。
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct VoicevoxState {
    /// VOICEVOX engine の `/singers` 経由で取得する speaker id。
    /// default = 6 (= ずんだもん「ノーマル」)、 user が GUI で変えると
    /// plugin parameter として保持される。
    pub speaker_id: u32,
    /// `style_name` は表示用 (= plugin GUI / Inspector で見える)。
    /// 内部処理は speaker_id だけで足りるが、 user に「どの speaker か」
    /// を見せるため keep。
    pub style_name: String,
}

impl Default for VoicevoxState {
    fn default() -> Self {
        Self {
            speaker_id: 6,
            style_name: "ノーマル".to_string(),
        }
    }
}

pub struct VoicevoxBuiltin {
    state: VoicevoxState,
    out_l: Vec<f32>,
    out_r: Vec<f32>,
    sample_rate: f64,
    activated: bool,
}

impl VoicevoxBuiltin {
    pub(super) fn new() -> Self {
        Self {
            state: VoicevoxState::default(),
            out_l: Vec::new(),
            out_r: Vec::new(),
            sample_rate: 0.0,
            activated: false,
        }
    }
}

impl LoadedPlugin for VoicevoxBuiltin {
    fn id(&self) -> &str {
        BUILTIN_ID_VOICEVOX
    }

    fn name(&self) -> &str {
        "VOICEVOX (builtin)"
    }

    fn format(&self) -> PluginFormat {
        PluginFormat::Builtin
    }

    fn activate(
        &mut self,
        sample_rate: f64,
        _min_frames: u32,
        max_frames: u32,
    ) -> Result<()> {
        self.sample_rate = sample_rate;
        let cap = max_frames as usize;
        self.out_l.clear();
        self.out_l.resize(cap, 0.0);
        self.out_r.clear();
        self.out_r.resize(cap, 0.0);
        self.activated = true;
        Ok(())
    }

    fn deactivate(&mut self) {
        self.activated = false;
    }

    fn start_processing(&mut self) -> Result<()> {
        Ok(())
    }

    fn stop_processing(&mut self) {}

    fn process(
        &mut self,
        frames: u32,
        _events: &[TimedNoteEvent],
        _input_audio: &[&[f32]],
        _aux_inputs: &[AuxInputBuf<'_>],
    ) -> Result<i32> {
        // PR-V2.1: 無音を返す。 PR-V2.4 で cache 引き → mix を実装。
        let n_l = (frames as usize).min(self.out_l.len());
        for v in &mut self.out_l[..n_l] {
            *v = 0.0;
        }
        let n_r = (frames as usize).min(self.out_r.len());
        for v in &mut self.out_r[..n_r] {
            *v = 0.0;
        }
        Ok(0)
    }

    fn output_buffer(&self, channel: usize) -> Option<&[f32]> {
        match channel {
            0 => Some(&self.out_l),
            1 => Some(&self.out_r),
            _ => None,
        }
    }

    fn drain_out_notes_into(&mut self, _out: &mut Vec<TimedNoteEvent>) {
        // VOICEVOX は MIDI 出力を持たない (= 純粋 instrument)。
    }

    fn set_render_mode(&mut self, _mode: RenderMode) -> bool {
        // synthesis は事前 bulk なので realtime / offline 区別なし
        // (= cache hit のみ、 再生時に追加 synth しない)。
        true
    }

    fn query_latency(&mut self) -> u32 {
        // PR-V2.4 で「cache まだ無い note を silence で埋める」 期間中の
        // latency 0 を返す。 真の synthesis は事前なので audio path の
        // latency は無し。
        0
    }

    fn state_save(&self) -> Result<Option<Vec<u8>>> {
        let cfg = bincode::config::standard();
        let bytes = bincode::encode_to_vec(&self.state, cfg)
            .context("VoicevoxBuiltin: encode state")?;
        Ok(Some(bytes))
    }

    fn state_load(&self, data: &[u8]) -> Result<()> {
        // LoadedPlugin::state_load は trait シグネチャ上 `&self` で、
        // 内部 mutability (Cell / RefCell) を入れるか PR-V2.5 で trait
        // を `&mut self` 化する。 現状は parse の妥当性だけ確認し、
        // self.state は default のまま (= 既存 user の speaker / style
        // 選択は復元されず default に戻る)。 影響は 2 fields のみで、
        // user が plugin GUI で再選択すれば復旧可能。 PR-V2.5 完了後は
        // full restore。
        let cfg = bincode::config::standard();
        let _: (VoicevoxState, usize) = bincode::decode_from_slice(data, cfg)
            .context("VoicevoxBuiltin: decode state (parse only — restore is PR-V2.5)")?;
        tracing::warn!(
            "VoicevoxBuiltin::state_load: PR-V2.5 待ちで state は default に戻ります (= 既存 speaker / style 選択が失われる)"
        );
        Ok(())
    }

    // --- Embedded GUI (PR-V2.4 で speaker picker / progress bar を追加) ----
    fn gui_is_embed_supported(&self) -> bool {
        false
    }

    fn gui_create_embedded(&mut self) -> Result<()> {
        bail!("VoicevoxBuiltin: GUI 未実装 (PR-V2.4 予定)")
    }

    fn gui_get_size(&self) -> Option<(u32, u32)> {
        None
    }

    fn gui_set_scale(&self, _scale: f64) -> Result<bool> {
        Ok(false)
    }

    fn gui_can_resize(&self) -> bool {
        false
    }

    fn gui_set_parent_hwnd(&self, _hwnd: u64) -> Result<()> {
        bail!("VoicevoxBuiltin: GUI 未実装")
    }

    fn gui_show(&self) -> Result<bool> {
        Ok(false)
    }

    fn gui_hide(&self) -> Result<()> {
        Ok(())
    }

    fn gui_set_size(&self, _width: u32, _height: u32) -> Result<()> {
        Ok(())
    }

    fn gui_destroy(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_is_zundamon_normal() {
        let s = VoicevoxState::default();
        assert_eq!(s.speaker_id, 6);
        assert_eq!(s.style_name, "ノーマル");
    }

    #[test]
    fn state_bincode_roundtrip() {
        let s = VoicevoxState {
            speaker_id: 3,
            style_name: "あまあま".to_string(),
        };
        let cfg = bincode::config::standard();
        let bytes = bincode::encode_to_vec(&s, cfg).unwrap();
        let (decoded, _): (VoicevoxState, usize) =
            bincode::decode_from_slice(&bytes, cfg).unwrap();
        assert_eq!(decoded, s);
    }

    #[test]
    fn voicevox_id_and_format() {
        let p = VoicevoxBuiltin::new();
        assert_eq!(p.id(), BUILTIN_ID_VOICEVOX);
        assert_eq!(p.format(), PluginFormat::Builtin);
        assert_eq!(p.name(), "VOICEVOX (builtin)");
    }

    #[test]
    fn voicevox_process_silent_for_now() {
        let mut p = VoicevoxBuiltin::new();
        p.activate(48000.0, 0, 256).unwrap();
        p.start_processing().unwrap();
        p.out_l[0] = 0.5;
        p.process(64, &[], &[], &[]).unwrap();
        // PR-V2.1: 無音返却 (= PR-V2.4 で cache hit に置換予定)。
        assert!(p.out_l[..64].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn voicevox_state_save_returns_bytes() {
        let p = VoicevoxBuiltin::new();
        let bytes = p.state_save().unwrap().expect("Some bytes");
        assert!(!bytes.is_empty());
        // bincode で読み戻し可能であることを確認。
        let cfg = bincode::config::standard();
        let (s, _): (VoicevoxState, usize) =
            bincode::decode_from_slice(&bytes, cfg).unwrap();
        assert_eq!(s, VoicevoxState::default());
    }
}
