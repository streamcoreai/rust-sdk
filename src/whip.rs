use reqwest::Client;
use serde::Deserialize;
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
    #[error("token fetch failed: {0}")]
    TokenFetch(String),
}

/// Perform a WHIP signaling exchange per RFC 9725 §4.2:
/// POST an SDP offer, receive a 201 Created with SDP answer and Location header.
pub async fn whip_offer(
    endpoint: &str,
    offer_sdp: &str,
    token: Option<&str>,
) -> Result<WhipResult, WhipError> {
    let client = Client::new();
    let mut req = client
        .post(endpoint)
        .header("Content-Type", "application/sdp");
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

    let answer_sdp = resp.text().await?;

    Ok(WhipResult {
        answer_sdp,
        session_url,
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
