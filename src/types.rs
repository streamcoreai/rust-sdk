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

/// Server-reported state of the voice agent pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Listening,
    Thinking,
    Speaking,
}

impl AgentState {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "listening" => Some(Self::Listening),
            "thinking" => Some(Self::Thinking),
            "speaking" => Some(Self::Speaking),
            _ => None,
        }
    }
}

impl std::fmt::Display for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Listening => write!(f, "listening"),
            Self::Thinking => write!(f, "thinking"),
            Self::Speaking => write!(f, "speaking"),
        }
    }
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
    #[serde(default)]
    pub state: String,
}

/// Configuration for a [`Client`](crate::Client).
#[derive(Debug, Clone)]
pub struct Config {
    /// WHIP signaling endpoint URL.
    /// Defaults to `"http://localhost:8080/whip"`.
    pub whip_endpoint: String,

    /// Optional JWT token for authenticating with the WHIP endpoint.
    pub token: Option<String>,

    /// Token endpoint URL. If set, the client will POST to this URL to fetch
    /// a JWT before each WHIP connection. Overrides `token` when both are set.
    pub token_url: Option<String>,

    /// API key sent as Bearer header when fetching from `token_url`.
    pub api_key: Option<String>,

    /// ICE server URLs for the WebRTC connection.
    /// Defaults to `["stun:stun.l.google.com:19302"]`.
    pub ice_servers: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            whip_endpoint: "http://localhost:8080/whip".into(),
            token: None,
            token_url: None,
            api_key: None,
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

    /// Called when the server reports an agent state transition.
    pub on_agent_state_change: Option<Box<dyn Fn(AgentState) + Send + Sync>>,

    /// Called for every raw data channel message.
    pub on_data_channel_message: Option<Box<dyn Fn(DataChannelMessage) + Send + Sync>>,
}

impl Default for EventHandler {
    fn default() -> Self {
        Self {
            on_status_change: None,
            on_transcript: None,
            on_error: None,
            on_agent_state_change: None,
            on_data_channel_message: None,
        }
    }
}
