//! 子プロセス側の pipe 接続 + handshake (`docs/plan_arch_refactor.md` §3)。
//!
//! channel は宛先別に型付けされる: daw_audio は `AudioEvent` を送り
//! `AudioCommand` を受ける、daw_plugin_host は `PluginEvent` を送り
//! `PluginCommand` を受ける。Hello には [`PROTOCOL_FINGERPRINT`] を載せ、
//! 親 (daw_gui) がビルド世代の一致を検証する。

use anyhow::{Context, Result};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

use crate::audio_device::{AudioDeviceConfig, AudioDeviceInfo};
use crate::protocol::{
    AudioCommand, AudioEvent, AudioSession, PluginCommand, PluginEvent, PROTOCOL_FINGERPRINT,
};
use crate::wire::{read_msg, write_msg};

/// daw_audio: pipe を開き Hello (fingerprint) を送って `AudioCommand::Ack` を待つ。
///
/// 続きは [`read_open_device`] → (デバイスを開く) → [`send_device_info`] → [`read_audio_session`]。
pub async fn perform_audio_handshake(pipe_name: &str) -> Result<NamedPipeClient> {
    let mut client = ClientOptions::new()
        .open(pipe_name)
        .with_context(|| format!("failed to open pipe {pipe_name}"))?;

    let hello = AudioEvent::Hello {
        pid: std::process::id(),
        protocol_fingerprint: PROTOCOL_FINGERPRINT,
    };
    write_msg(&mut client, &hello).await?;
    tracing::info!(?hello, "sent Hello");

    let ack: AudioCommand = read_msg(&mut client).await?;
    anyhow::ensure!(
        ack == AudioCommand::Ack,
        "expected Ack from parent, got {:?}",
        ack
    );
    tracing::info!("received Ack from parent");
    Ok(client)
}

/// daw_plugin_host: pipe を開き Hello (fingerprint) を送って
/// `PluginCommand::Ack` を待つ。
pub async fn perform_plugin_handshake(pipe_name: &str) -> Result<NamedPipeClient> {
    let mut client = ClientOptions::new()
        .open(pipe_name)
        .with_context(|| format!("failed to open pipe {pipe_name}"))?;

    let hello = PluginEvent::Hello {
        pid: std::process::id(),
        protocol_fingerprint: PROTOCOL_FINGERPRINT,
    };
    write_msg(&mut client, &hello).await?;
    tracing::info!(?hello, "sent Hello");

    let ack: PluginCommand = read_msg(&mut client).await?;
    anyhow::ensure!(
        ack == PluginCommand::Ack,
        "expected Ack from parent, got {:?}",
        ack
    );
    tracing::info!("received Ack from parent");
    Ok(client)
}

fn validate_session(s: &AudioSession) -> Result<()> {
    anyhow::ensure!(
        s.sample_rate > 0
            && s.max_frames > 0
            && s.max_frames <= crate::audio_bridge::MAX_FRAMES
            && s.channels > 0,
        "invalid audio session: {s:?}"
    );
    Ok(())
}

/// daw_audio: 次のメッセージが `AudioCommand::OpenDevice` であることを期待して読む (r.md #156: 開くデバイスは
/// handshake で届く)。
pub async fn read_open_device(client: &mut NamedPipeClient) -> Result<AudioDeviceConfig> {
    match read_msg::<_, AudioCommand>(client).await? {
        AudioCommand::OpenDevice { config } => {
            tracing::info!(?config, "received audio device config");
            Ok(config)
        }
        other => anyhow::bail!("expected OpenDevice, got {:?}", other),
    }
}

/// daw_audio: 開いたデバイスを親へ報告する (親はこの `sample_rate` で session を作る)。
pub async fn send_device_info(client: &mut NamedPipeClient, info: &AudioDeviceInfo) -> Result<()> {
    write_msg(client, &AudioEvent::DeviceInfo(Box::new(info.clone()))).await
}

/// daw_audio: 次のメッセージが `AudioCommand::Session` であることを期待して
/// 読む。
pub async fn read_audio_session(client: &mut NamedPipeClient) -> Result<AudioSession> {
    match read_msg::<_, AudioCommand>(client).await? {
        AudioCommand::Session(s) => {
            tracing::info!(?s, "received audio session");
            validate_session(&s)?;
            Ok(s)
        }
        other => anyhow::bail!("expected Session, got {:?}", other),
    }
}

/// daw_plugin_host: 次のメッセージが `PluginCommand::Session` であることを
/// 期待して読む。
pub async fn read_plugin_session(client: &mut NamedPipeClient) -> Result<AudioSession> {
    match read_msg::<_, PluginCommand>(client).await? {
        PluginCommand::Session(s) => {
            tracing::info!(?s, "received audio session");
            validate_session(&s)?;
            Ok(s)
        }
        other => anyhow::bail!("expected Session, got {:?}", other),
    }
}
