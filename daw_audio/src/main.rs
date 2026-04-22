use anyhow::{Context, Result};
use common::protocol::ChildKind;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

#[tokio::main]
async fn main() -> Result<()> {
    common::logging::init_tracing();
    tracing::info!("daw_audio started");

    let pipe_name = std::env::args()
        .nth(1)
        .context("expected pipe name as first argument")?;

    common::client::perform_handshake(&pipe_name, ChildKind::Audio).await?;
    tracing::info!("daw_audio handshake complete");

    let _stream = start_output_stream().context("failed to start audio stream")?;
    tracing::info!("audio stream running; awaiting shutdown");

    std::future::pending::<()>().await;
    Ok(())
}

fn start_output_stream() -> Result<cpal::Stream> {
    let test_tone = std::env::var_os("DAW_AUDIO_TEST_TONE").is_some();

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
        test_tone,
        "opening output stream"
    );

    if sample_format != cpal::SampleFormat::F32 {
        anyhow::bail!("unsupported sample format: {sample_format:?}, expected F32");
    }

    let config: cpal::StreamConfig = supported.into();

    let stream = if test_tone {
        build_test_tone_stream(&device, &config, sample_rate, channels)?
    } else {
        build_silence_stream(&device, &config)?
    };

    stream.play().context("failed to start stream")?;
    Ok(stream)
}

fn build_silence_stream(device: &cpal::Device, config: &cpal::StreamConfig) -> Result<cpal::Stream> {
    let stream = device
        .build_output_stream(
            config,
            |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                for sample in data.iter_mut() {
                    *sample = 0.0;
                }
            },
            |err| tracing::error!(?err, "audio stream error"),
            None,
        )
        .context("failed to build silence stream")?;
    Ok(stream)
}

fn build_test_tone_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_rate: u32,
    channels: u16,
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
            },
            |err| tracing::error!(?err, "audio stream error"),
            None,
        )
        .context("failed to build test tone stream")?;
    Ok(stream)
}
