use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use tracing::{error, info};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_OPUS};
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage as DCMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_remote::TrackRemote;

use crate::audio::AudioState;
use crate::types::*;
use crate::whip;

/// Errors that can occur during client operations.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("WebRTC error: {0}")]
    WebRTC(#[from] webrtc::Error),
    #[error("WHIP error: {0}")]
    Whip(#[from] whip::WhipError),
    #[error("Audio error: {0}")]
    Audio(String),
}

/// Manages a WebRTC connection to a Voice Agent server via WHIP signaling.
pub struct Client {
    pub config: Config,
    events: Arc<EventHandler>,
    state: Arc<Mutex<ClientState>>,
    pc: Mutex<Option<Arc<RTCPeerConnection>>>,
    session_url: Mutex<String>,

    /// The outbound audio track. Write RTP packets here to send audio to the server.
    /// Available after [`connect`](Client::connect) returns.
    pub local_track: Arc<TrackLocalStaticRTP>,

    /// Notifies when the remote audio track is available.
    pub remote_track_notify: Arc<Notify>,

    /// The inbound audio track from the agent.
    /// Check after `remote_track_notify` fires.
    pub remote_track: Arc<Mutex<Option<Arc<TrackRemote>>>>,

    /// Internal audio encoding/decoding state.
    pub(crate) audio: AudioState,
}

struct ClientState {
    status: ConnectionStatus,
    transcript: Vec<TranscriptEntry>,
    assist_buf: String,
}

impl Client {
    /// Create a new voice agent client.
    pub fn new(config: Config, events: EventHandler) -> Self {
        let local_track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_string(),
                clock_rate: 48000,
                channels: 1,
                ..Default::default()
            },
            "audio".into(),
            "streamcoreai-client".into(),
        ));

        Self {
            config,
            events: Arc::new(events),
            state: Arc::new(Mutex::new(ClientState {
                status: ConnectionStatus::Idle,
                transcript: Vec::new(),
                assist_buf: String::new(),
            })),
            pc: Mutex::new(None),
            session_url: Mutex::new(String::new()),
            local_track,
            remote_track_notify: Arc::new(Notify::new()),
            remote_track: Arc::new(Mutex::new(None)),
            audio: AudioState::new().expect("failed to initialize Opus encoder"),
        }
    }

    /// Current connection status.
    pub fn status(&self) -> ConnectionStatus {
        self.state.lock().unwrap().status
    }

    /// Copy of the current conversation transcript.
    pub fn transcript(&self) -> Vec<TranscriptEntry> {
        self.state.lock().unwrap().transcript.clone()
    }

    /// Establish a WebRTC connection to the voice agent server using WHIP.
    pub async fn connect(&self) -> Result<(), ClientError> {
        self.set_status(ConnectionStatus::Connecting);

        let mut m = MediaEngine::default();
        m.register_default_codecs()?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut m)?;

        let api = APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .build();

        let ice_servers: Vec<RTCIceServer> = self
            .config
            .ice_servers
            .iter()
            .map(|url| RTCIceServer {
                urls: vec![url.clone()],
                ..Default::default()
            })
            .collect();

        let pc = Arc::new(
            api.new_peer_connection(RTCConfiguration {
                ice_servers,
                ..Default::default()
            })
            .await?,
        );

        // Add local audio track.
        pc.add_track(Arc::clone(&self.local_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;

        // Create data channel for events.
        let dc = pc.create_data_channel("events", None).await?;
        self.setup_data_channel(dc);

        // Handle remote track.
        let remote_track = Arc::clone(&self.remote_track);
        let notify = Arc::clone(&self.remote_track_notify);
        pc.on_track(Box::new(move |track, _, _| {
            let remote_track = Arc::clone(&remote_track);
            let notify = Arc::clone(&notify);
            Box::pin(async move {
                *remote_track.lock().unwrap() = Some(track);
                notify.notify_one();
            })
        }));

        // Handle connection state changes.
        let events = Arc::clone(&self.events);
        let state = Arc::clone(&self.state);
        pc.on_peer_connection_state_change(Box::new(move |s| {
            let events = Arc::clone(&events);
            let state = Arc::clone(&state);
            Box::pin(async move {
                let new_status = match s {
                    RTCPeerConnectionState::Connected => ConnectionStatus::Connected,
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                        ConnectionStatus::Disconnected
                    }
                    RTCPeerConnectionState::Disconnected => ConnectionStatus::Disconnected,
                    _ => return,
                };
                state.lock().unwrap().status = new_status;
                if let Some(ref cb) = events.on_status_change {
                    cb(new_status);
                }
            })
        }));

        // Create offer.
        let offer = pc.create_offer(None).await?;
        pc.set_local_description(offer).await?;

        // Wait for ICE gathering to complete.
        let gather_done = Arc::new(Notify::new());
        let gather_notify = Arc::clone(&gather_done);
        pc.on_ice_gathering_state_change(Box::new(move |state| {
            let gather_notify = Arc::clone(&gather_notify);
            Box::pin(async move {
                if state == webrtc::ice_transport::ice_gatherer_state::RTCIceGathererState::Complete
                {
                    gather_notify.notify_one();
                }
            })
        }));
        gather_done.notified().await;

        let local_desc = pc.local_description().await.ok_or_else(|| {
            webrtc::Error::new("local description not available after ICE gathering".to_string())
        })?;

        // WHIP exchange.
        let result = whip::whip_offer(&self.config.whip_endpoint, &local_desc.sdp).await?;
        *self.session_url.lock().unwrap() = result.session_url;

        let answer = RTCSessionDescription::answer(result.answer_sdp)?;
        pc.set_remote_description(answer).await?;

        *self.pc.lock().unwrap() = Some(pc);

        Ok(())
    }

    /// Tear down the WebRTC connection and free resources.
    pub async fn disconnect(&self) {
        let session_url = {
            let mut url = self.session_url.lock().unwrap();
            std::mem::take(&mut *url)
        };
        whip::whip_delete(&session_url).await;

        if let Some(pc) = self.pc.lock().unwrap().take() {
            let _ = pc.close().await;
        }
        self.set_status(ConnectionStatus::Idle);
    }

    fn set_status(&self, s: ConnectionStatus) {
        self.state.lock().unwrap().status = s;
        if let Some(ref cb) = self.events.on_status_change {
            cb(s);
        }
    }

    fn setup_data_channel(&self, dc: Arc<RTCDataChannel>) {
        let events = Arc::clone(&self.events);
        let state = Arc::clone(&self.state);

        dc.on_message(Box::new(move |msg: DCMessage| {
            let events = Arc::clone(&events);
            let state = Arc::clone(&state);
            Box::pin(async move {
                let text = String::from_utf8_lossy(&msg.data);
                let dc_msg: crate::types::DataChannelMessage = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => {
                        error!("failed to parse DC message: {e}");
                        return;
                    }
                };

                if let Some(ref cb) = events.on_data_channel_message {
                    cb(dc_msg.clone());
                }

                handle_dc_message(&state, &events, dc_msg);
            })
        }));
    }
}

fn handle_dc_message(
    state: &Arc<Mutex<ClientState>>,
    events: &Arc<EventHandler>,
    msg: crate::types::DataChannelMessage,
) {
    let mut st = state.lock().unwrap();

    match msg.msg_type.as_str() {
        "transcript" => {
            if msg.r#final {
                let pending = std::mem::take(&mut st.assist_buf);
                st.transcript.retain(|e| {
                    !((e.role == "user" && e.partial) || (e.role == "assistant" && e.partial))
                });
                if !pending.is_empty() {
                    st.transcript.push(TranscriptEntry {
                        role: "assistant".into(),
                        text: pending,
                        partial: false,
                    });
                }
                st.transcript.push(TranscriptEntry {
                    role: "user".into(),
                    text: msg.text.clone(),
                    partial: false,
                });
            } else {
                st.transcript.retain(|e| !(e.role == "user" && e.partial));
                st.transcript.push(TranscriptEntry {
                    role: "user".into(),
                    text: msg.text.clone(),
                    partial: true,
                });
            }

            if let Some(ref cb) = events.on_transcript {
                let entry = st.transcript.last().unwrap().clone();
                let all = st.transcript.clone();
                cb(entry, all);
            }
        }
        "response" => {
            st.assist_buf.push_str(&msg.text);
            let current = st.assist_buf.clone();

            st.transcript
                .retain(|e| !(e.role == "assistant" && e.partial));
            st.transcript.push(TranscriptEntry {
                role: "assistant".into(),
                text: current,
                partial: true,
            });

            if let Some(ref cb) = events.on_transcript {
                let entry = st.transcript.last().unwrap().clone();
                let all = st.transcript.clone();
                cb(entry, all);
            }
        }
        "error" => {
            if let Some(ref cb) = events.on_error {
                cb(msg.message.clone());
            }
        }
        _ => {
            info!("unknown DC message type: {}", msg.msg_type);
        }
    }
}
