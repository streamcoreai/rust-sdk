use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;

use crate::icerestart::ICE_FRAGMENT_CONTENT_TYPE;

#[derive(Debug)]
pub struct WhipResult {
    pub answer_sdp: String,
    pub session_url: String,
    /// ETag identifying the ICE session (RFC 9725 §4.3.1); required to PATCH it.
    pub etag: String,
    /// Single-use credential for reattaching a later redial to this
    /// conversation. Empty when the server cannot resume — a realtime
    /// (speech-to-speech) session, whose history lives inside the provider.
    pub resume_token: String,
    /// "new", "resumed", or "expired". Anything but "resumed" on a redial
    /// means the agent has no memory of the earlier conversation.
    pub resume_status: String,
}

/// The server's reply to an ICE restart.
#[derive(Debug)]
pub struct WhipRestartResult {
    /// The server's sdpfrag, to fold into the stored remote description.
    pub fragment: String,
    /// The rotated tag identifying the new ICE session.
    pub etag: String,
}

#[derive(Debug, Error)]
pub enum WhipError {
    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("unexpected status {status}: {body}")]
    UnexpectedStatus { status: u16, body: String },
    #[error("token fetch failed: {0}")]
    TokenFetch(String),
    #[error("ICE restart failed ({status}): {body}")]
    RestartRejected {
        status: u16,
        body: String,
        /// The tag the server reported as current, present on a 412.
        current_etag: String,
    },
}

impl WhipError {
    /// Whether another ICE restart against the same session could still
    /// succeed. A 404 means the session was reaped, 409 that it has no peer to
    /// restart, and 405 that the server declines restarts entirely — only a
    /// redial recovers from those.
    pub fn restart_retryable(&self) -> bool {
        match self {
            WhipError::RestartRejected { status, .. } => !matches!(status, 404 | 409 | 405),
            _ => true,
        }
    }
}

/// Perform a WHIP signaling exchange per RFC 9725 §4.2:
/// POST an SDP offer, receive a 201 Created with SDP answer and Location header.
///
/// `resume_token` is a StreamCore extension: it asks the server to reattach
/// this new transport to the conversation a previous connection was having,
/// rather than starting a fresh one. Check `resume_status` on the result — a
/// token the server no longer recognises still yields a working call, but one
/// whose agent remembers nothing.
pub async fn whip_offer(
    endpoint: &str,
    offer_sdp: &str,
    token: Option<&str>,
    resume_token: Option<&str>,
) -> Result<WhipResult, WhipError> {
    let client = Client::new();
    let mut req = client
        .post(endpoint)
        .header("Content-Type", "application/sdp");
    if let Some(rt) = resume_token {
        if !rt.is_empty() {
            req = req.query(&[("resume", rt)]);
        }
    }
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {}", t));
    }
    let resp = req.body(offer_sdp.to_string()).send().await?;

    let status = resp.status().as_u16();
    if status != 201 {
        let body = resp.text().await.unwrap_or_default();
        return Err(WhipError::UnexpectedStatus { status, body });
    }

    // RFC 9725 §4.2: Location header points to the WHIP session URL.
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let session_url = if location.starts_with("http") {
        location
    } else if let Ok(parsed) = reqwest::Url::parse(endpoint) {
        format!(
            "{}://{}{}",
            parsed.scheme(),
            parsed.host_str().unwrap_or(""),
            location
        )
    } else {
        location
    };

    let header = |name: &str| -> String {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let etag = header("etag");
    let resume_token = header("x-resume-token");
    let resume_status = header("x-resume-status");

    let answer_sdp = resp.text().await?;

    Ok(WhipResult {
        answer_sdp,
        session_url,
        etag,
        resume_token,
        resume_status,
    })
}

/// Send an ICE restart to the session URL per RFC 9725 §4.4.2.
///
/// `etag` is sent as `If-Match` so a restart racing another one is rejected
/// rather than applied to a generation that no longer exists.
pub async fn whip_restart_ice(
    session_url: &str,
    fragment: &str,
    etag: Option<&str>,
    token: Option<&str>,
) -> Result<WhipRestartResult, WhipError> {
    let mut req = Client::new()
        .patch(session_url)
        .header("Content-Type", ICE_FRAGMENT_CONTENT_TYPE);
    if let Some(tag) = etag {
        if !tag.is_empty() {
            req = req.header("If-Match", tag);
        }
    }
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {}", t));
    }

    let resp = req.body(fragment.to_string()).send().await?;

    let status = resp.status().as_u16();
    let new_etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if status != 200 {
        let body = resp.text().await.unwrap_or_default();
        return Err(WhipError::RestartRejected {
            status,
            body,
            current_etag: new_etag,
        });
    }

    let fragment = resp.text().await?;
    Ok(WhipRestartResult {
        fragment,
        etag: if new_etag.is_empty() {
            etag.unwrap_or_default().to_string()
        } else {
            new_etag
        },
    })
}

/// Fetch a JWT from a token endpoint.
/// If `api_key` is provided, it is sent as a Bearer Authorization header.
pub async fn fetch_token(token_url: &str, api_key: Option<&str>) -> Result<String, WhipError> {
    #[derive(Deserialize)]
    struct TokenResponse {
        token: String,
    }

    let mut req = Client::new().post(token_url);
    if let Some(key) = api_key {
        req = req.header("Authorization", format!("Bearer {}", key));
    }

    let resp = req.send().await?;

    let status = resp.status().as_u16();
    if status != 200 {
        let body = resp.text().await.unwrap_or_default();
        return Err(WhipError::TokenFetch(format!(
            "status {}: {}",
            status, body
        )));
    }

    let data: TokenResponse = resp
        .json()
        .await
        .map_err(|e| WhipError::TokenFetch(e.to_string()))?;
    Ok(data.token)
}

/// Terminate a WHIP session per RFC 9725 §4.2.
/// Best-effort — errors are silently ignored.
pub async fn whip_delete(session_url: &str, token: Option<&str>) {
    if session_url.is_empty() {
        return;
    }
    let mut req = Client::new().delete(session_url);
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {}", t));
    }
    let _ = req.send().await;
}
