// CPAL ↔ LiveKit audio bridge.
//
// AudioCapture: mic → NativeAudioSource (→ LiveKit track)
// AudioOutput:  LiveKit NativeAudioStream frames → speaker ring buffer → cpal output
//
// IMPORTANT: cpal::Stream deliberately opts out of Send (to support Android's AAudio).
// We work around this by building cpal streams on dedicated OS threads that own
// them for their entire lifetime. The thread blocks on a kill-channel recv() and
// exits (dropping the stream) when the AudioCapture/AudioOutput struct is dropped.

use std::{
    borrow::Cow,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::{AudioSourceOptions, RtcAudioSource};
use tracing::warn;

// ── Mic capture ───────────────────────────────────────────────────────────────

/// Captures microphone audio and feeds it into a LiveKit `NativeAudioSource`.
pub struct AudioCapture {
    /// The LiveKit audio source — clone this to create a `LocalAudioTrack`.
    pub source: NativeAudioSource,
    /// Set to `true` to send silence instead of real mic audio.
    pub muted: Arc<AtomicBool>,
    /// Dropping this ends the mic capture thread and stops the cpal stream.
    _kill: std::sync::mpsc::Sender<()>,
}

impl AudioCapture {
    pub fn start() -> Result<Self> {
        // ── Step 1: Discover device config (no ownership of non-Send types) ──
        let (sample_rate, channels) = {
            let host = cpal::default_host();
            let dev = host
                .default_input_device()
                .ok_or_else(|| anyhow::anyhow!("no default input device"))?;
            let cfg = dev.default_input_config()?;
            (cfg.sample_rate().0, cfg.channels() as u32)
        };

        // ── Step 2: Create the LiveKit audio source ───────────────────────────
        let source = NativeAudioSource::new(
            AudioSourceOptions::default(),
            sample_rate,
            channels,
            200, // 200 ms internal buffer
        );
        let source_clone = source.clone();
        let muted = Arc::new(AtomicBool::new(false));
        let muted_clone = muted.clone();

        // ── Step 3: Channels ─────────────────────────────────────────────────
        let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<i16>>(8);
        let (kill_tx, kill_rx) = std::sync::mpsc::channel::<()>();
        // Signals back whether the stream started successfully.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

        // ── Step 4: Build+own the cpal stream on a dedicated thread ──────────
        // cpal::Stream is intentionally !Send; we never move it.
        std::thread::spawn(move || {
            let host = cpal::default_host();
            let dev = match host.default_input_device() {
                Some(d) => d,
                None => {
                    let _ = ready_tx.send(Err("no default input device".into()));
                    return;
                }
            };
            let cfg = match dev.default_input_config() {
                Ok(c) => c,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("input config: {e}")));
                    return;
                }
            };
            let stream_cfg: cpal::StreamConfig = cfg.into();
            let stream = match dev.build_input_stream(
                &stream_cfg,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let samples: Vec<i16> = data
                        .iter()
                        .map(|&s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
                        .collect();
                    let _ = pcm_tx.try_send(samples);
                },
                |e| warn!("cpal input error: {e}"),
                None,
            ) {
                Ok(s) => s,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("build input stream: {e}")));
                    return;
                }
            };
            if let Err(e) = stream.play() {
                let _ = ready_tx.send(Err(format!("play input stream: {e}")));
                return;
            }
            let _ = ready_tx.send(Ok(()));
            // Block until AudioCapture is dropped (kill_tx dropped → recv Err).
            let _ = kill_rx.recv();
            // `stream` is dropped here, stopping mic capture.
        });

        ready_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("input thread died before ready"))?
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        // ── Step 5: Feeder task: PCM → LiveKit NativeAudioSource ─────────────
        // spawn_blocking is used so the brief recv() doesn't starve the executor.
        let rt_handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            loop {
                match pcm_rx.recv() {
                    Ok(samples) => {
                        let samples_per_channel = (samples.len() as u32) / channels.max(1);
                        let data: Vec<i16> = if muted_clone.load(Ordering::Relaxed) {
                            vec![0i16; samples.len()]
                        } else {
                            samples
                        };
                        let frame = AudioFrame {
                            data: Cow::Owned(data),
                            sample_rate,
                            num_channels: channels,
                            samples_per_channel,
                        };
                        let _ = rt_handle.block_on(source_clone.capture_frame(&frame));
                    }
                    Err(_) => break, // stream thread exited → pcm_tx dropped
                }
            }
        });

        Ok(Self {
            source,
            muted,
            _kill: kill_tx,
        })
    }

    /// Returns the `RtcAudioSource` to pass to `LocalAudioTrack::create_audio_track`.
    pub fn rtc_source(&self) -> RtcAudioSource {
        RtcAudioSource::Native(self.source.clone())
    }
}

// ── Speaker output ────────────────────────────────────────────────────────────

/// Receives i16 PCM frames (from remote LiveKit audio tracks) and plays them
/// through the default output device via a shared ring buffer.
///
/// Multiple remote tracks write into the same ring buffer — last-writer-wins
/// rather than proper mixing, which is acceptable for ≤ 2 participants (MVP).
pub struct AudioOutput {
    /// Push decoded samples here; the cpal output callback drains them.
    pub buf: Arc<Mutex<std::collections::VecDeque<f32>>>,
    /// Dropping this ends the output thread and stops the cpal stream.
    _kill: std::sync::mpsc::Sender<()>,
}

impl AudioOutput {
    pub fn new() -> Result<Self> {
        // ── Step 1: Discover output config ───────────────────────────────────
        let (sample_format, _channels, buffer_size) = {
            let host = cpal::default_host();
            let dev = host
                .default_output_device()
                .ok_or_else(|| anyhow::anyhow!("no default output device"))?;
            let cfg = dev.default_output_config()?;
            (cfg.sample_format(), cfg.channels() as u32, cfg.config())
        };

        // ── Step 2: Shared ring buffer ────────────────────────────────────────
        let buf: Arc<Mutex<std::collections::VecDeque<f32>>> =
            Arc::new(Mutex::new(std::collections::VecDeque::with_capacity(192_000)));

        let (kill_tx, kill_rx) = std::sync::mpsc::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

        // ── Step 3: Build+own the cpal output stream on a dedicated thread ────
        let buf_out = buf.clone();
        std::thread::spawn(move || {
            let host = cpal::default_host();
            let dev = match host.default_output_device() {
                Some(d) => d,
                None => {
                    let _ = ready_tx.send(Err("no default output device".into()));
                    return;
                }
            };
            let stream = match build_output_stream(sample_format, &buffer_size, &dev, buf_out) {
                Ok(s) => s,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("build output stream: {e}")));
                    return;
                }
            };
            if let Err(e) = stream.play() {
                let _ = ready_tx.send(Err(format!("play output stream: {e}")));
                return;
            }
            let _ = ready_tx.send(Ok(()));
            let _ = kill_rx.recv();
            // `stream` dropped here.
        });

        ready_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("output thread died before ready"))?
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        Ok(Self { buf, _kill: kill_tx })
    }

    /// Push a batch of i16 samples into the playback ring buffer.
    pub fn push_samples(&self, samples: &[i16]) {
        let mut guard = self.buf.lock().unwrap();
        for &s in samples {
            guard.push_back(s as f32 / i16::MAX as f32);
        }
        // Cap at ~2 seconds to prevent unbounded growth.
        while guard.len() > 192_000 {
            guard.pop_front();
        }
    }
}

fn build_output_stream(
    fmt: cpal::SampleFormat,
    config: &cpal::StreamConfig,
    device: &cpal::Device,
    buf: Arc<Mutex<std::collections::VecDeque<f32>>>,
) -> Result<cpal::Stream> {
    let stream = match fmt {
        cpal::SampleFormat::F32 => {
            let b = buf.clone();
            device.build_output_stream::<f32, _, _>(
                config,
                move |data: &mut [f32], _| {
                    let mut g = b.lock().unwrap();
                    for s in data.iter_mut() {
                        *s = g.pop_front().unwrap_or(0.0);
                    }
                },
                |e| warn!("cpal output error: {e}"),
                None,
            )?
        }
        cpal::SampleFormat::I16 => {
            let b = buf.clone();
            device.build_output_stream::<i16, _, _>(
                config,
                move |data: &mut [i16], _| {
                    let mut g = b.lock().unwrap();
                    for s in data.iter_mut() {
                        *s = g
                            .pop_front()
                            .map(|f| (f * i16::MAX as f32) as i16)
                            .unwrap_or(0);
                    }
                },
                |e| warn!("cpal output error: {e}"),
                None,
            )?
        }
        other => anyhow::bail!("unsupported output sample format: {other:?}"),
    };
    Ok(stream)
}
