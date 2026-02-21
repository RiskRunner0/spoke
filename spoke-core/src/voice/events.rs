// Matrix signaling events for Spoke voice.
// These are sent to the room when a user joins/leaves/mutes voice.

use matrix_sdk::ruma::events::macros::EventContent;

/// Sent when a local user joins the voice channel.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, EventContent)]
#[ruma_event(type = "org.spoke.voice.join", kind = MessageLike)]
pub struct VoiceJoinEventContent {
    /// Opaque session identifier (UUID) so other clients can correlate events.
    pub session_id: String,
}

/// Sent when a local user leaves the voice channel.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, EventContent)]
#[ruma_event(type = "org.spoke.voice.leave", kind = MessageLike)]
pub struct VoiceLeaveEventContent {}

/// Sent when the local user toggles microphone mute state.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, EventContent)]
#[ruma_event(type = "org.spoke.voice.mute", kind = MessageLike)]
pub struct VoiceMuteEventContent {
    pub muted: bool,
}
