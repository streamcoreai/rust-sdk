use serde::{Deserialize, Serialize};

/// Current state of the voice agent connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus {
    Idle,
    Connecting,
    Connected,
    Error,
    Disconnected,
}

impl std::fmt::Display for ConnectionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => write!(f, "idle"),
            Self::Connecting => write!(f, "connecting"),
            Self::Connected => write!(f, "connected"),
            Self::Error => write!(f, "error"),
            Self::Disconnected => write!(f, "disconnected"),
        }
    }
}

/// A single transcript message in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub role: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
}

/// A message received on the WebRTC data channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataChannelMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub r#final: bool,
    #[serde(default)]
    pub message: String,
}

/// Configuration for a [`Client`](crate::Client).
#[derive(Debug, Clone)]
pub struct Config {
    /// WHIP signaling endpoint URL.
    /// Defaults to `"http://localhost:8080/whip"`.
    pub whip_endpoint: String,

    /// ICE server URLs for the WebRTC connection.
    /// Defaults to `["stun:stun.l.google.com:19302"]`.
    pub ice_servers: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            whip_endpoint: "http://localhost:8080/whip".into(),
            ice_servers: vec!["stun:stun.l.google.com:19302".into()],
        }
    }
}

/// Callbacks for voice agent events.
///
/// All callbacks are optional. Wrap your handlers in `Some(...)`.
pub struct EventHandler {
    /// Called when the connection status changes.
    pub on_status_change: Option<Box<dyn Fn(ConnectionStatus) + Send + Sync>>,

    /// Called when a new or updated transcript entry is received.
    pub on_transcript: Option<Box<dyn Fn(TranscriptEntry, Vec<TranscriptEntry>) + Send + Sync>>,

    /// Called when an error occurs.
    pub on_error: Option<Box<dyn Fn(String) + Send + Sync>>,

    /// Called for every raw data channel message.
    pub on_data_channel_message: Option<Box<dyn Fn(DataChannelMessage) + Send + Sync>>,
}

impl Default for EventHandler {
    fn default() -> Self {
        Self {
            on_status_change: None,
            on_transcript: None,
            on_error: None,
            on_data_channel_message: None,
        }
    }
}
