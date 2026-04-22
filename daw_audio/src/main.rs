use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use anyhow::{Context, Result};
use common::audio_bridge::{AudioBridgeHandle, CHANNELS, MAX_FRAMES};
use common::protocol::{ChildKind, MainToChild};
use common::win_sem::Semaphore;
use common::wire::read_msg;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::net::windows::named_pipe::NamedPipeClient;

#[repr(u8)]
#[derive(Clone, Copy)]
enum PlayState {
    Silence = 0,
    TestTone = 1,
    Plugin = 2,
}

impl PlayState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::TestTone,
            2 => Self::Plugin,
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

    let mut pipe = common::client::perform_handshake(&pipe_name, ChildKind::Audio).await?;
    tracing::info!("daw_audio handshake complete");

    let session = common::client::read_session(&mut pipe).await?;
    tracing::info!(?session, "audio session ready");

    let bridge = Arc::new(
        AudioBridgeHandle::open(&session.shmem_id).context("failed to open audio shmem")?,
    );
    let request_sem = Arc::new(
        Semaphore::open(&session.request_sem_id).context("failed to open request semaphore")?,
    );
    let ready_sem = Arc::new(
        Semaphore::open(&session.ready_sem_id).context("failed to open ready semaphore")?,
    );

    let initial = if std::env::var_os("DAW_AUDIO_TEST_TONE").is_some() {
        PlayState::TestTone
    } else {
        PlayState::Silence
    };
    let play_state = Arc::new(AtomicU8::new(initial as u8));

    let _stream = start_output_stream(
        Arc::clone(&play_state),
        Arc::clone(&bridge),
        Arc::clone(&request_sem),
        Arc::clone(&ready_sem),
    )
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
                state.store(PlayState::Plugin as u8, Ordering::Relaxed);
            }
            Ok(MainToChild::Stop) => {
                tracing::info!("received Stop");
                state.store(PlayState::Silence as u8, Ordering::Relaxed);
            }
            Ok(MainToChild::Ack) => {
                tracing::warn!("unexpected Ack outside of handshake");
            }
            Ok(MainToChild::Session(_)) => {
                tracing::warn!("received Session after initial handshake (ignored)");
            }
            Ok(MainToChild::LoadSong(_)) => {
                // Song state lives in daw_plugin_host; daw_audio does not use it.
            }
            Err(e) => {
                tracing::info!(error = ?e, "receive loop ending");
                break;
            }
        }
    }
}

fn start_output_stream(
    play_state: Arc<AtomicU8>,
    bridge: Arc<AudioBridgeHandle>,
    request_sem: Arc<Semaphore>,
    ready_sem: Arc<Semaphore>,
) -> Result<cpal::Stream> {
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
    let stream = build_stream(
        &device,
        &config,
        sample_rate,
        channels,
        play_state,
        bridge,
        request_sem,
        ready_sem,
    )?;
    stream.play().context("failed to start stream")?;
    Ok(stream)
}

#[allow(clippy::too_many_arguments)]
fn build_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_rate: u32,
    channels: u16,
    play_state: Arc<AtomicU8>,
    bridge: Arc<AudioBridgeHandle>,
    request_sem: Arc<Semaphore>,
    ready_sem: Arc<Semaphore>,
) -> Result<cpal::Stream> {
    use std::f32::consts::TAU;
    let freq_hz: f32 = 440.0;
    let amplitude: f32 = 0.1;
    let phase_inc = freq_hz * TAU / sample_rate as f32;
    let channels_usize = channels as usize;
    let bridge_channels = CHANNELS as usize;
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
                        for frame in data.chunks_mut(channels_usize) {
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
                    PlayState::Plugin => {
                        let frames = (data.len() / channels_usize).min(MAX_FRAMES as usize);
                        bridge.set_frames_requested(frames as u32);
                        if request_sem.release().is_err() || ready_sem.wait().is_err() {
                            for s in data.iter_mut() {
                                *s = 0.0;
                            }
                            return;
                        }
                        // Copy interleaved stereo from shmem into the CPAL buffer,
                        // expanding to `channels_usize` if the device has more than 2.
                        unsafe {
                            let src = bridge.samples_ptr();
                            for i in 0..frames {
                                let l = *src.add(i * bridge_channels);
                                let r = *src.add(i * bridge_channels + 1);
                                let out = data.as_mut_ptr().add(i * channels_usize);
                                *out = l;
                                if channels_usize > 1 {
                                    *out.add(1) = r;
                                }
                                for c in 2..channels_usize {
                                    *out.add(c) = 0.0;
                                }
                            }
                        }
                        let filled = frames * channels_usize;
                        for s in &mut data[filled..] {
                            *s = 0.0;
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
