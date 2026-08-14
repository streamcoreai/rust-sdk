use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::sync::Notify;
use tracing::{error, info, warn};
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
use crate::icerestart::{apply_ice_fragment, ice_fragment_from_sdp};
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
    session_url: Arc<Mutex<String>>,
    /// Most recently used JWT (either the static `config.token` or one fetched
    /// from `config.token_url` during `connect`). `disconnect` reuses this so
    /// the WHIP DELETE is properly authenticated; otherwise servers enforcing
    /// Bearer auth on `/whip` reject the teardown and skip server-side
    /// finalization (billing, transcript persistence, etc.).
    last_token: Arc<Mutex<Option<String>>>,
    /// ETag of the current ICE session, sent as `If-Match` on a restart.
    etag: Arc<Mutex<String>>,
    /// Set by `disconnect` so an in-flight restart abandons itself rather than
    /// PATCHing a session that is about to be deleted.
    cancelled: Arc<AtomicBool>,
    /// Single-use credential that reattaches a redial to this conversation.
    /// Re-issued by the server on every response.
    resume_token: Arc<Mutex<String>>,
    /// The server's answer on the last POST: "new", "resumed", or "expired".
    resume_status: Arc<Mutex<String>>,
    /// Guards the recovery sequence so it is never doubled up.
    reconnecting: Arc<AtomicBool>,
    /// Bumped by `connect` and `disconnect` so an in-flight recovery abandons
    /// itself rather than fighting the connection replacing it.
    generation: Arc<AtomicU64>,

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

/// Everything the ICE restart sequence needs, in a form the peer connection's
/// state callback can own.
///
/// The peer connection is held weakly on purpose: the callback is registered
/// *on* that connection, so a strong reference here would keep it alive
/// forever.
#[derive(Clone)]
struct ReconnectCtx {
    pc: Weak<RTCPeerConnection>,
    /// Held weakly so the callback registered on the peer connection does not
    /// keep the whole client alive. Phase two needs it to rebuild the
    /// transport from scratch.
    client: Weak<Client>,
    session_url: Arc<Mutex<String>>,
    etag: Arc<Mutex<String>>,
    resume_token: Arc<Mutex<String>>,
    resume_status: Arc<Mutex<String>>,
    token: Arc<Mutex<Option<String>>>,
    fallback_token: Option<String>,
    events: Arc<EventHandler>,
    state: Arc<Mutex<ClientState>>,
    cancelled: Arc<AtomicBool>,
    reconnecting: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    max_attempts: u32,
    delay: Duration,
    resume_attempts: u32,
    resume_delay: Duration,
}

impl ReconnectCtx {
    fn set_status(&self, status: ConnectionStatus) {
        self.state.lock().unwrap().status = status;
        if let Some(ref cb) = self.events.on_status_change {
            cb(status);
        }
    }

    fn emit(
        &self,
        attempt: u32,
        max_attempts: u32,
        phase: ReconnectPhase,
        outcome: ReconnectOutcome,
        error: Option<String>,
    ) {
        if let Some(ref cb) = self.events.on_reconnect {
            cb(ReconnectEvent {
                attempt,
                max_attempts,
                phase,
                outcome,
                error,
            });
        }
    }

    fn token(&self) -> Option<String> {
        self.token
            .lock()
            .unwrap()
            .clone()
            .or_else(|| self.fallback_token.clone())
    }
}

/// Recover a dropped connection, in two phases.
///
/// **ICE restart** first, while the peer connection is merely `Disconnected`.
/// It keeps the transport itself alive — same `RTCPeerConnection`, same DTLS,
/// same tracks — so nothing above it notices.
///
/// **Resume redial** when that is no longer possible: once the connection has
/// `Failed` the server has closed its peer and there is nothing left to
/// restart. A fresh peer connection then POSTs with the session's resume
/// token, and the server reattaches it to the same conversation. The transport
/// is new; the history, the rolling summary and the agent's memory of the call
/// are not.
///
/// The phases share a deadline: the server holds an abandoned conversation for
/// `session_grace_ms` (30s by default), and time spent on restarts is time not
/// available for redials.
async fn run_reconnect(ctx: ReconnectCtx) {
    // Only one sequence at a time.
    if ctx
        .reconnecting
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    ctx.set_status(ConnectionStatus::Reconnecting);
    let generation = ctx.generation.load(Ordering::SeqCst);

    if !recover_by_ice_restart(&ctx, generation).await
        && generation == ctx.generation.load(Ordering::SeqCst)
    {
        recover_by_resume(&ctx, generation).await;
    }

    ctx.reconnecting.store(false, Ordering::SeqCst);
}

/// Phase one: keep the transport alive with an ICE restart.
///
/// Returns true when the drop is dealt with — recovered, or abandoned because
/// the client itself went away. False means phase two should take over.
async fn recover_by_ice_restart(ctx: &ReconnectCtx, generation: u64) -> bool {
    if ctx.max_attempts == 0 {
        return false;
    }
    let mut delay = ctx.delay;

    for attempt in 1..=ctx.max_attempts {
        tokio::time::sleep(delay).await;
        delay *= 2;

        if ctx.cancelled.load(Ordering::SeqCst)
            || generation != ctx.generation.load(Ordering::SeqCst)
        {
            return true;
        }
        let Some(pc) = ctx.pc.upgrade() else {
            return false;
        };

        match pc.connection_state() {
            RTCPeerConnectionState::Closed => return true,
            // The server has already closed its peer; every further restart
            // would 404.
            RTCPeerConnectionState::Failed => return false,
            RTCPeerConnectionState::Connected => {
                // Healed on its own while we waited.
                ctx.set_status(ConnectionStatus::Connected);
                ctx.emit(
                    attempt,
                    ctx.max_attempts,
                    ReconnectPhase::IceRestart,
                    ReconnectOutcome::Recovered,
                    None,
                );
                return true;
            }
            _ => {}
        }

        ctx.emit(
            attempt,
            ctx.max_attempts,
            ReconnectPhase::IceRestart,
            ReconnectOutcome::Attempting,
            None,
        );

        match restart_ice(ctx, &pc).await {
            Ok(()) => {
                // The restart is applied; ICE still has to complete its
                // checks, so the move back to Connected comes from the state
                // callback.
                ctx.emit(
                    attempt,
                    ctx.max_attempts,
                    ReconnectPhase::IceRestart,
                    ReconnectOutcome::Recovered,
                    None,
                );
                return true;
            }
            Err(err) => {
                // 404/409/405 mean the session cannot be restarted at all —
                // stop burning the shared deadline and let resume try.
                if !err.restart_retryable() {
                    return false;
                }
                // A 412 means another restart landed first; adopt the ETag the
                // server reported as current and try again against it.
                if let whip::WhipError::RestartRejected {
                    ref current_etag, ..
                } = err
                {
                    if !current_etag.is_empty() {
                        *ctx.etag.lock().unwrap() = current_etag.clone();
                    }
                }
                warn!("ICE restart attempt {} failed: {}", attempt, err);
            }
        }
    }
    false
}

/// Phase two: rebuild the transport and reattach it to the conversation.
///
/// This is the only recovery available once the connection has failed, and the
/// only one at all for a process that was suspended or offline for longer than
/// ICE tolerates.
async fn recover_by_resume(ctx: &ReconnectCtx, generation: u64) {
    let token = ctx.resume_token.lock().unwrap().clone();
    if ctx.resume_attempts == 0 || token.is_empty() {
        ctx.set_status(ConnectionStatus::Disconnected);
        return;
    }

    let mut delay = ctx.resume_delay;
    for attempt in 1..=ctx.resume_attempts {
        // The dead peer connection still holds a binding on the outbound
        // track; drop it before building its replacement.
        if let Some(client) = ctx.client.upgrade() {
            client.close_peer_connection().await;
        }

        tokio::time::sleep(delay).await;
        delay *= 2;

        if ctx.cancelled.load(Ordering::SeqCst)
            || generation != ctx.generation.load(Ordering::SeqCst)
        {
            return;
        }

        let Some(client) = ctx.client.upgrade() else {
            return; // the caller dropped the client
        };

        ctx.emit(
            attempt,
            ctx.resume_attempts,
            ReconnectPhase::Resume,
            ReconnectOutcome::Attempting,
            None,
        );

        if let Err(err) = client.establish(Some(token.clone())).await {
            if attempt == ctx.resume_attempts {
                ctx.set_status(ConnectionStatus::Disconnected);
                ctx.emit(
                    attempt,
                    ctx.resume_attempts,
                    ReconnectPhase::Resume,
                    ReconnectOutcome::Failed,
                    Some(err.to_string()),
                );
                return;
            }
            warn!("resume attempt {} failed: {}", attempt, err);
            continue;
        }

        let resumed = ctx.resume_status.lock().unwrap().as_str() == "resumed";
        if !resumed {
            warn!(
                "reconnected, but the server could not resume the session \
                 — the agent has no memory of the earlier conversation"
            );
        }
        ctx.emit(
            attempt,
            ctx.resume_attempts,
            ReconnectPhase::Resume,
            if resumed {
                ReconnectOutcome::Recovered
            } else {
                ReconnectOutcome::RecoveredWithoutHistory
            },
            None,
        );
        return;
    }
}

/// One ICE restart round-trip against the existing peer connection.
async fn restart_ice(
    ctx: &ReconnectCtx,
    pc: &Arc<RTCPeerConnection>,
) -> Result<(), whip::WhipError> {
    let remote = pc.remote_description().await.ok_or_else(|| {
        whip::WhipError::TokenFetch("no remote description to restart against".into())
    })?;

    let offer = pc
        .create_offer(Some(
            webrtc::peer_connection::offer_answer_options::RTCOfferOptions {
                ice_restart: true,
                voice_activity_detection: false,
            },
        ))
        .await
        .map_err(|e| whip::WhipError::TokenFetch(format!("create restart offer: {e}")))?;

    // The promise must be taken after create_offer, which is what reopens
    // gathering — one taken earlier would resolve against the old generation.
    let mut gather_done = pc.gathering_complete_promise().await;
    pc.set_local_description(offer)
        .await
        .map_err(|e| whip::WhipError::TokenFetch(format!("set restart offer: {e}")))?;

    if tokio::time::timeout(Duration::from_secs(5), gather_done.recv())
        .await
        .is_err()
    {
        warn!("ICE re-gathering timed out, patching partial candidates");
    }

    let local = pc.local_description().await.ok_or_else(|| {
        whip::WhipError::TokenFetch("no local description after restart offer".into())
    })?;

    let session_url = ctx.session_url.lock().unwrap().clone();
    let etag = ctx.etag.lock().unwrap().clone();

    let result = whip::whip_restart_ice(
        &session_url,
        &ice_fragment_from_sdp(&local.sdp),
        Some(&etag),
        ctx.token().as_deref(),
    )
    .await?;

    let answer_sdp = apply_ice_fragment(&remote.sdp, &result.fragment).ok_or_else(|| {
        whip::WhipError::TokenFetch("ICE restart reply has no ICE credentials".into())
    })?;

    // The offer left us in have-local-offer; the connection only returns to
    // stable once this answer is applied.
    let answer = RTCSessionDescription::answer(answer_sdp)
        .map_err(|e| whip::WhipError::TokenFetch(format!("parse restart answer: {e}")))?;
    pc.set_remote_description(answer)
        .await
        .map_err(|e| whip::WhipError::TokenFetch(format!("set restart answer: {e}")))?;

    *ctx.etag.lock().unwrap() = result.etag;
    Ok(())
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
            session_url: Arc::new(Mutex::new(String::new())),
            last_token: Arc::new(Mutex::new(None)),
            etag: Arc::new(Mutex::new(String::new())),
            cancelled: Arc::new(AtomicBool::new(false)),
            resume_token: Arc::new(Mutex::new(String::new())),
            resume_status: Arc::new(Mutex::new(String::new())),
            reconnecting: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
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
    ///
    /// Takes `&Arc<Self>` because automatic recovery needs a handle back to
    /// the client: rebuilding the transport after a failure is not something
    /// a detached task can do with borrowed state. Callers already holding an
    /// `Arc<Client>` — which both examples do — call this unchanged.
    pub async fn connect(self: &Arc<Self>) -> Result<(), ClientError> {
        // A fresh connect abandons whatever came before, including any resume
        // token: this is a new conversation, not a continuation.
        self.cancelled.store(false, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.resume_token.lock().unwrap().clear();
        self.establish(None).await
    }

    /// Build a peer connection and complete WHIP signaling.
    ///
    /// With a resume token the server reattaches this new transport to the
    /// conversation the previous one was having, so the history, the rolling
    /// summary and the agent's memory of the call all survive. Without one it
    /// is an ordinary first connect.
    fn establish(
        self: &Arc<Self>,
        resume_token: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ClientError>> + Send>> {
        // Boxed rather than a plain `async fn` to break a cycle rustc cannot
        // reason through: establishing installs the state callback, which
        // drives recovery, which establishes again. Declaring the future
        // `Send` here lets each side be checked against the declaration
        // instead of chasing the cycle forever.
        let this = Arc::clone(self);
        Box::pin(async move {
            this.set_status(if resume_token.is_some() {
                ConnectionStatus::Reconnecting
            } else {
                ConnectionStatus::Connecting
            });

            let mut m = MediaEngine::default();
            m.register_default_codecs()?;

            let mut registry = Registry::new();
            registry = register_default_interceptors(registry, &mut m)?;

            let api = APIBuilder::new()
                .with_media_engine(m)
                .with_interceptor_registry(registry)
                .build();

            let ice_servers: Vec<RTCIceServer> = this
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
            pc.add_track(Arc::clone(&this.local_track) as Arc<dyn TrackLocal + Send + Sync>)
                .await?;

            // Create data channel for events.
            let dc = pc.create_data_channel("events", None).await?;
            this.setup_data_channel(dc);

            // Handle remote track.
            let remote_track = Arc::clone(&this.remote_track);
            let notify = Arc::clone(&this.remote_track_notify);
            pc.on_track(Box::new(move |track, _, _| {
                let remote_track = Arc::clone(&remote_track);
                let notify = Arc::clone(&notify);
                Box::pin(async move {
                    *remote_track.lock().unwrap() = Some(track);
                    notify.notify_one();
                })
            }));

            // Handle connection state changes.
            let ctx = ReconnectCtx {
                pc: Arc::downgrade(&pc),
                client: Arc::downgrade(&this),
                session_url: Arc::clone(&this.session_url),
                etag: Arc::clone(&this.etag),
                resume_token: Arc::clone(&this.resume_token),
                resume_status: Arc::clone(&this.resume_status),
                token: Arc::clone(&this.last_token),
                fallback_token: this.config.token.clone(),
                events: Arc::clone(&this.events),
                state: Arc::clone(&this.state),
                cancelled: Arc::clone(&this.cancelled),
                reconnecting: Arc::clone(&this.reconnecting),
                generation: Arc::clone(&this.generation),
                max_attempts: this.config.reconnect_attempts,
                delay: this.config.reconnect_delay,
                resume_attempts: this.config.resume_attempts,
                resume_delay: this.config.resume_delay,
            };
            pc.on_peer_connection_state_change(Box::new(move |s| {
                let ctx = ctx.clone();
                Box::pin(async move {
                    let new_status = match s {
                        RTCPeerConnectionState::Connected => ConnectionStatus::Connected,
                        RTCPeerConnectionState::Closed => ConnectionStatus::Disconnected,
                        RTCPeerConnectionState::Failed => {
                            // Straight to failed, or the restart phase ran out of
                            // time. ICE restart is off the table; only a resume
                            // redial can save the conversation now.
                            tokio::spawn(run_reconnect(ctx));
                            return;
                        }
                        RTCPeerConnectionState::Disconnected => {
                            // Transient. ICE often repairs this unaided, so the
                            // restart sequence waits before spending an attempt —
                            // but if the local address changed it never will, and
                            // this is the only window in which a restart can still
                            // work: at ~25s the server sees Failed, closes the
                            // peer, and the session becomes unrecoverable.
                            tokio::spawn(run_reconnect(ctx));
                            return;
                        }
                        _ => return,
                    };
                    ctx.set_status(new_status);
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
                    if state
                        == webrtc::ice_transport::ice_gatherer_state::RTCIceGathererState::Complete
                    {
                        gather_notify.notify_one();
                    }
                })
            }));
            gather_done.notified().await;

            let local_desc = pc.local_description().await.ok_or_else(|| {
                webrtc::Error::new(
                    "local description not available after ICE gathering".to_string(),
                )
            })?;

            // Fetch a fresh token from the token endpoint if configured. Any
            // resource_id goes in the body for the server to sign into it.
            let token = if let Some(url) = &this.config.token_url {
                Some(
                    whip::fetch_token(
                        url,
                        this.config.api_key.as_deref(),
                        this.config.resource_id.as_deref(),
                    )
                    .await?,
                )
            } else {
                this.config.token.clone()
            };

            // Cache the token so `disconnect` can authenticate the WHIP DELETE.
            *this.last_token.lock().unwrap() = token.clone();

            // WHIP exchange.
            let result = whip::whip_offer(
                &this.config.whip_endpoint,
                &local_desc.sdp,
                token.as_deref(),
                resume_token.as_deref(),
                this.config.resource_id.as_deref(),
            )
            .await?;
            *this.session_url.lock().unwrap() = result.session_url;
            *this.etag.lock().unwrap() = result.etag;
            *this.resume_token.lock().unwrap() = result.resume_token;
            *this.resume_status.lock().unwrap() = result.resume_status;

            let answer = RTCSessionDescription::answer(result.answer_sdp)?;
            pc.set_remote_description(answer).await?;

            *this.pc.lock().unwrap() = Some(pc);

            Ok(())
        })
    }

    /// Close the current peer connection without touching session state, so a
    /// redial can still use the session URL and resume token.
    async fn close_peer_connection(&self) {
        let pc = self.pc.lock().unwrap().take();
        if let Some(pc) = pc {
            // The state callback checks the live peer connection and so
            // ignores this teardown.
            let _ = pc.close().await;
        }
    }

    /// Tear down the WebRTC connection and free resources.
    pub async fn disconnect(&self) {
        // Abandons any in-flight recovery: it would otherwise resume a session
        // this call is about to DELETE.
        self.cancelled.store(true, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.etag.lock().unwrap().clear();
        self.resume_token.lock().unwrap().clear();
        self.resume_status.lock().unwrap().clear();

        let session_url = {
            let mut url = self.session_url.lock().unwrap();
            std::mem::take(&mut *url)
        };

        // Resolve the token used for the WHIP DELETE. Prefer the cached token
        // captured during `connect` (which may have come from `token_url`),
        // fall back to the static `config.token`, and as a last resort
        // re-fetch from `token_url` so teardown still authenticates.
        let mut token = self.last_token.lock().unwrap().take();
        if token.is_none() {
            token = self.config.token.clone();
        }
        if token.is_none() {
            if let Some(url) = &self.config.token_url {
                if let Ok(t) =
                    whip::fetch_token(url, self.config.api_key.as_deref(), None).await
                {
                    token = Some(t);
                }
            }
        }
        whip::whip_delete(&session_url, token.as_deref()).await;

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
        "timing" => {
            if let Some(ref cb) = events.on_timing {
                if !msg.stage.is_empty() {
                    cb(TimingEvent {
                        stage: msg.stage.clone(),
                        ms: msg.ms,
                    });
                }
            }
        }
        "state" => {
            if let Some(ref cb) = events.on_agent_state_change {
                if let Some(state) = AgentState::from_str(&msg.state) {
                    cb(state);
                }
            }
        }
        _ => {
            info!("unknown DC message type: {}", msg.msg_type);
        }
    }
}
