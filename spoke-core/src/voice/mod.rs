// Voice session layer — LiveKit Rust SDK + CPAL audio pipeline.
// Voice join/leave is signaled via org.spoke.voice.* Matrix events.

pub mod audio;
pub mod events;

use std::sync::{Arc, atomic::Ordering};

use anyhow::Result;
use futures::StreamExt;
use livekit::{
    Room, RoomEvent, RoomOptions,
    prelude::{LocalAudioTrack, LocalTrack, RemoteTrack, TrackSource},
    options::TrackPublishOptions,
    webrtc::audio_stream::native::NativeAudioStream,
};
use tokio::sync::mpsc;
use tracing::warn;

use audio::{AudioCapture, AudioOutput};

// ── Public types ──────────────────────────────────────────────────────────────

/// Events emitted by an active `VoiceSession` toward the UI layer.
#[derive(Debug)]
pub enum VoiceEvent {
    /// The list of remote participant display names has changed.
    ParticipantsUpdated(Vec<String>),
    /// A non-fatal error occurred in the voice session.
    Error(String),
}

/// An active LiveKit voice session with mic capture and speaker playback.
pub struct VoiceSession {
    room: Arc<Room>,
    capture: AudioCapture,
    _output: Option<AudioOutput>,
    /// Handles to tasks feeding remote audio into the output ring buffer.
    _output_handles: Vec<tokio::task::JoinHandle<()>>,
    /// Handle to the room-event dispatch task.
    _event_handle: tokio::task::JoinHandle<()>,
}

impl VoiceSession {
    /// Connect to a LiveKit room, start mic capture, and begin receiving audio.
    pub async fn connect(
        url: &str,
        token: &str,
        event_tx: mpsc::UnboundedSender<VoiceEvent>,
    ) -> Result<Self> {
        // Connect to the LiveKit room.
        let (room, mut events) =
            Room::connect(url, token, RoomOptions::default()).await?;
        let room = Arc::new(room);

        // Start microphone capture.
        let capture = AudioCapture::start()?;

        // Publish the local audio track.
        let local_track = LocalAudioTrack::create_audio_track(
            "microphone",
            capture.rtc_source(),
        );
        room.local_participant()
            .publish_track(
                LocalTrack::Audio(local_track),
                TrackPublishOptions {
                    source: TrackSource::Microphone,
                    ..Default::default()
                },
            )
            .await?;

        // Create speaker output (best-effort; log and continue if unavailable).
        let output = match AudioOutput::new() {
            Ok(o) => Some(o),
            Err(e) => {
                warn!("audio output unavailable: {e}");
                None
            }
        };

        // Spawn the room-event loop.
        let room_clone = room.clone();
        let output_buf = output.as_ref().map(|o| o.buf.clone());
        let output_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        let event_handle = {
            // We collect output track handles as events arrive; store them in a
            // Vec inside the task (the outer vec is for the struct only).
            let tx = event_tx.clone();
            let room_ev = room_clone.clone();
            tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    match event {
                        RoomEvent::TrackSubscribed { track, .. } => {
                            if let RemoteTrack::Audio(audio_track) = track {
                                let buf = output_buf.clone();
                                let handle = tokio::spawn(async move {
                                    let rtc = audio_track.rtc_track();
                                    // Request 48 kHz mono from LiveKit's jitter buffer.
                                    let mut stream =
                                        NativeAudioStream::new(rtc, 48_000, 1);
                                    while let Some(frame) = stream.next().await {
                                        if let Some(ref b) = buf {
                                            let mut guard = b.lock().unwrap();
                                            for &s in frame.data.iter() {
                                                guard.push_back(
                                                    s as f32 / i16::MAX as f32,
                                                );
                                            }
                                            // Cap buffer to ~2 seconds.
                                            while guard.len() > 192_000 {
                                                guard.pop_front();
                                            }
                                        }
                                    }
                                });
                                // Note: we can't mutate output_handles from inside
                                // the spawned task, so just detach; the task ends
                                // when the audio stream closes.
                                drop(handle); // detach — task runs to completion
                            }
                        }

                        RoomEvent::ParticipantConnected(_)
                        | RoomEvent::ParticipantDisconnected(_) => {
                            let names: Vec<String> = room_ev
                                .remote_participants()
                                .values()
                                .map(|p| p.name().to_owned())
                                .collect();
                            let _ = tx.send(VoiceEvent::ParticipantsUpdated(names));
                        }

                        _ => {}
                    }
                }
            })
        };

        Ok(Self {
            room,
            capture,
            _output: output,
            _output_handles: output_handles,
            _event_handle: event_handle,
        })
    }

    /// Disconnect from the LiveKit room and release audio resources.
    pub async fn disconnect(&self) {
        self._event_handle.abort();
        if let Err(e) = self.room.close().await {
            warn!("room close: {e}");
        }
    }

    /// Mute or unmute the local microphone.
    /// When muted, silence frames are fed to LiveKit instead of real audio.
    pub fn set_muted(&self, muted: bool) {
        self.capture.muted.store(muted, Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.capture.muted.load(Ordering::Relaxed)
    }
}
