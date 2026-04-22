use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use anyhow::{Context, Result};
use common::protocol::{ChildKind, MainToChild};
use common::wire::read_msg;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::net::windows::named_pipe::NamedPipeClient;

#[repr(u8)]
#[derive(Clone, Copy)]
enum PlayState {
    Silence = 0,
    TestTone = 1,
}

impl PlayState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::TestTone,
            _ => Self::Silence,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    common::logging::init_tracing();
    tracing::info!("daw_audio started");

    let pipe_name = std::env::args()
        .nth(1)
        .context("expected pipe name as first argument")?;

    let pipe = common::client::perform_handshake(&pipe_name, ChildKind::Audio).await?;
    tracing::info!("daw_audio handshake complete");

    let initial = if std::env::var_os("DAW_AUDIO_TEST_TONE").is_some() {
        PlayState::TestTone
    } else {
        PlayState::Silence
    };
    let play_state = Arc::new(AtomicU8::new(initial as u8));

    let _stream = start_output_stream(Arc::clone(&play_state))
        .context("failed to start audio stream")?;
    tracing::info!("audio stream running");

    recv_loop(pipe, play_state).await;
    tracing::info!("daw_audio exiting");
    Ok(())
}

async fn recv_loop(mut pipe: NamedPipeClient, state: Arc<AtomicU8>) {
    loop {
        match read_msg::<_, MainToChild>(&mut pipe).await {
            Ok(MainToChild::Play) => {
                tracing::info!("received Play");
                state.store(PlayState::TestTone as u8, Ordering::Relaxed);
            }
            Ok(MainToChild::Stop) => {
                tracing::info!("received Stop");
                state.store(PlayState::Silence as u8, Ordering::Relaxed);
            }
            Ok(MainToChild::Ack) => {
                tracing::warn!("unexpected Ack outside of handshake");
            }
            Err(e) => {
                tracing::info!(error = ?e, "receive loop ending");
                break;
            }
        }
    }
}

fn start_output_stream(play_state: Arc<AtomicU8>) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no default output device")?;
    let device_name = device.name().unwrap_or_else(|_| "<unknown>".into());
    let supported = device
        .default_output_config()
        .context("failed to query default output config")?;

    let sample_rate = supported.sample_rate().0;
    let channels = supported.channels();
    let sample_format = supported.sample_format();

    tracing::info!(
        device = %device_name,
        sample_rate,
        channels,
        ?sample_format,
        "opening output stream"
    );

    if sample_format != cpal::SampleFormat::F32 {
        anyhow::bail!("unsupported sample format: {sample_format:?}, expected F32");
    }

    let config: cpal::StreamConfig = supported.into();
    let stream = build_stream(&device, &config, sample_rate, channels, play_state)?;
    stream.play().context("failed to start stream")?;
    Ok(stream)
}

fn build_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_rate: u32,
    channels: u16,
    play_state: Arc<AtomicU8>,
) -> Result<cpal::Stream> {
    use std::f32::consts::TAU;
    let freq_hz: f32 = 440.0;
    let amplitude: f32 = 0.1;
    let phase_inc = freq_hz * TAU / sample_rate as f32;
    let channels = channels as usize;
    let mut phase: f32 = 0.0;

    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                let state = PlayState::from_u8(play_state.load(Ordering::Relaxed));
                match state {
                    PlayState::Silence => {
                        for s in data.iter_mut() {
                            *s = 0.0;
                        }
                    }
                    PlayState::TestTone => {
                        for frame in data.chunks_mut(channels) {
                            let sample = phase.sin() * amplitude;
                            for s in frame.iter_mut() {
                                *s = sample;
                            }
                            phase += phase_inc;
                            if phase >= TAU {
                                phase -= TAU;
                            }
                        }
                    }
                }
            },
            |err| tracing::error!(?err, "audio stream error"),
            None,
        )
        .context("failed to build output stream")?;
    Ok(stream)
}
