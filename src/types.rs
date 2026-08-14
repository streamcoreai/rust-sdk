use serde::{Deserialize, Serialize};

/// Current state of the voice agent connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus {
    Idle,
    Connecting,
    Connected,
    /// The transport dropped and an ICE restart is in flight. The session, and
    /// with it the conversation, is still alive on the server — this is not a
    /// terminal state and usually resolves back to `Connected`.
    Reconnecting,
    Error,
    Disconnected,
}

impl std::fmt::Display for ConnectionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => write!(f, "idle"),
            Self::Connecting => write!(f, "connecting"),
            Self::Connected => write!(f, "connected"),
            Self::Reconnecting => write!(f, "reconnecting"),
            Self::Error => write!(f, "error"),
            Self::Disconnected => write!(f, "disconnected"),
        }
    }
}

/// Which recovery mechanism an attempt used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectPhase {
    /// Keeps the existing transport alive. Only possible while the connection
    /// is `Disconnected`; nothing above the transport notices.
    IceRestart,
    /// Rebuilds the transport and reattaches it to the same server-side
    /// conversation. The only option once the connection has `Failed`.
    Resume,
}

/// Where a recovery attempt got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectOutcome {
    Attempting,
    Recovered,
    /// The call works but the server could not resume the session, so the
    /// agent has forgotten the conversation and will not know what was already
    /// said. Worth surfacing to the user rather than only logging.
    RecoveredWithoutHistory,
    Failed,
}

/// Progress of the recovery sequence.
#[derive(Debug, Clone)]
pub struct ReconnectEvent {
    /// 1-based attempt number, counted within the phase.
    pub attempt: u32,
    /// Attempts this phase makes before moving on or giving up.
    pub max_attempts: u32,
    /// Which mechanism this attempt used.
    pub phase: ReconnectPhase,
    pub outcome: ReconnectOutcome,
    /// Why the attempt failed, when `outcome` is `Failed`.
    pub error: Option<String>,
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

/// A single latency measurement from the server pipeline.
#[derive(Debug, Clone)]
pub struct TimingEvent {
    pub stage: String,
    pub ms: i64,
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
    pub stage: String,
    #[serde(default)]
    pub ms: i64,
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

    /// Who is on the call: an app user ID, or the number a phone call came
    /// from. The server passes it to an external agent, which can then remember
    /// a caller across separate calls.
    ///
    /// With `token_url` set it goes in the token request body and the server
    /// signs it into the token. Otherwise it goes as a header, which the server
    /// only trusts when there is no signed claim.
    pub resource_id: Option<String>,

    /// ICE server URLs for the WebRTC connection.
    /// Defaults to `["stun:stun.l.google.com:19302"]`.
    pub ice_servers: Vec<String>,

    /// How many ICE restarts to attempt before giving up on a dropped
    /// connection. Attempts are spaced by `reconnect_delay` doubling each
    /// time, and the whole sequence must finish inside the ~25 seconds it
    /// takes the connection to go from disconnected to failed — past that the
    /// server has closed the peer and only a fresh `connect` recovers.
    /// Defaults to 3. Zero disables automatic reconnection.
    pub reconnect_attempts: u32,

    /// Wait before the first ICE restart attempt, doubling for each retry.
    /// The initial wait matters: most disconnected transitions are brief
    /// packet loss that ICE repairs on its own, and patching immediately would
    /// spend a restart on a connection that was about to recover by itself.
    /// Defaults to 2s.
    pub reconnect_delay: std::time::Duration,

    /// How many times to redial with the session's resume token after ICE
    /// restart is no longer possible — which is the case as soon as the
    /// connection reaches `Failed`, and the only case for a process that was
    /// suspended or offline longer than ICE tolerates.
    ///
    /// A redial builds a new transport but reattaches to the same
    /// conversation, so history and the agent's memory survive. The deadline
    /// is the server's `session_grace_ms` (30s by default) measured from the
    /// drop, and the ICE restart phase spends from the same budget.
    /// Defaults to 2. Zero disables the resume phase.
    pub resume_attempts: u32,

    /// Wait before the first resume redial, doubling for each retry. Shorter
    /// than `reconnect_delay` because by this point the connection is known
    /// dead — there is nothing left to wait out. Defaults to 1s.
    pub resume_delay: std::time::Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            whip_endpoint: "http://localhost:8080/whip".into(),
            token: None,
            token_url: None,
            api_key: None,
            resource_id: None,
            ice_servers: vec!["stun:stun.l.google.com:19302".into()],
            reconnect_attempts: 3,
            reconnect_delay: std::time::Duration::from_secs(2),
            resume_attempts: 2,
            resume_delay: std::time::Duration::from_secs(1),
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

    /// Called when a timing/latency event is received from the server.
    pub on_timing: Option<Box<dyn Fn(TimingEvent) + Send + Sync>>,

    /// Called when the server reports an agent state transition.
    pub on_agent_state_change: Option<Box<dyn Fn(AgentState) + Send + Sync>>,

    /// Called for every raw data channel message.
    pub on_data_channel_message: Option<Box<dyn Fn(DataChannelMessage) + Send + Sync>>,

    /// Called for each ICE restart attempt and once when the outcome is known,
    /// so a UI can distinguish a recoverable drop from a lost call.
    pub on_reconnect: Option<Box<dyn Fn(ReconnectEvent) + Send + Sync>>,
}

impl Default for EventHandler {
    fn default() -> Self {
        Self {
            on_status_change: None,
            on_transcript: None,
            on_error: None,
            on_timing: None,
            on_agent_state_change: None,
            on_data_channel_message: None,
            on_reconnect: None,
        }
    }
}
