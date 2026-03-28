use reqwest::Client;
use thiserror::Error;

#[derive(Debug)]
pub struct WhipResult {
    pub answer_sdp: String,
    pub session_url: String,
}

#[derive(Debug, Error)]
pub enum WhipError {
    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("unexpected status {status}: {body}")]
    UnexpectedStatus { status: u16, body: String },
}

/// Perform a WHIP signaling exchange per RFC 9725 §4.2:
/// POST an SDP offer, receive a 201 Created with SDP answer and Location header.
pub async fn whip_offer(endpoint: &str, offer_sdp: &str) -> Result<WhipResult, WhipError> {
    let client = Client::new();
    let resp = client
        .post(endpoint)
        .header("Content-Type", "application/sdp")
        .body(offer_sdp.to_string())
        .send()
        .await?;

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
        format!("{}://{}{}", parsed.scheme(), parsed.host_str().unwrap_or(""), location)
    } else {
        location
    };

    let answer_sdp = resp.text().await?;

    Ok(WhipResult {
        answer_sdp,
        session_url,
    })
}

/// Terminate a WHIP session per RFC 9725 §4.2.
/// Best-effort — errors are silently ignored.
pub async fn whip_delete(session_url: &str) {
    if session_url.is_empty() {
        return;
    }
    let _ = Client::new().delete(session_url).send().await;
}
