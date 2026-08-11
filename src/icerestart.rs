//! ICE restart support (RFC 9725 §4.4, RFC 8840).
//!
//! When the local address changes — a machine moving networks, a VPN toggle, a
//! NAT rebind that does not recover — the gathered candidates are dead and the
//! connection cannot heal on its own. Re-POSTing an offer would allocate a new
//! session on the server, losing the conversation history and replaying the
//! greeting. An ICE restart instead swaps only the ICE generation on the
//! existing session, so nothing above the transport notices.
//!
//! The wire format is an SDP fragment rather than a full description, so these
//! helpers translate both ways: the new local offer becomes a fragment to
//! PATCH, and the server's reply is folded back into the answer already held.

/// Media type of an ICE restart body.
pub const ICE_FRAGMENT_CONTENT_TYPE: &str = "application/trickle-ice-sdpfrag";

/// ICE credentials and candidates read out of an SDP or fragment.
#[derive(Debug, Default, Clone)]
pub struct IceDetails {
    pub ufrag: String,
    pub pwd: String,
    pub candidates: Vec<String>,
}

/// Splits an SDP or fragment into lines, tolerating LF-only bodies.
fn split_sdp_lines(sdp: &str) -> impl Iterator<Item = &str> {
    sdp.split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.is_empty())
}

/// Reads the ICE credentials and candidates out of an SDP or fragment.
/// Credentials may sit at session or media level; the first of each wins, which
/// is what a bundled description means anyway.
pub fn parse_ice_details(sdp: &str) -> IceDetails {
    let mut details = IceDetails::default();

    for line in split_sdp_lines(sdp) {
        if let Some(v) = line.strip_prefix("a=ice-ufrag:") {
            if details.ufrag.is_empty() {
                details.ufrag = v.to_string();
            }
        } else if let Some(v) = line.strip_prefix("a=ice-pwd:") {
            if details.pwd.is_empty() {
                details.pwd = v.to_string();
            }
        } else if let Some(v) = line.strip_prefix("a=candidate:") {
            details.candidates.push(v.to_string());
        }
    }

    details
}

/// Renders a local offer as the sdpfrag to PATCH: the credentials, then the
/// bundle-master m-line with its mid and candidates. Shaped after the request
/// example in RFC 9725 §4.4.2.
pub fn ice_fragment_from_sdp(local_sdp: &str) -> String {
    let mut ufrag = String::new();
    let mut pwd = String::new();
    let mut m_line = String::new();
    let mut mid = String::new();
    let mut candidates: Vec<&str> = Vec::new();
    let mut media_index: i32 = -1;

    for line in split_sdp_lines(local_sdp) {
        if line.starts_with("m=") {
            media_index += 1;
            if media_index == 0 {
                m_line = line.to_string();
            }
            continue;
        }
        // Session-level attributes and the first media section's are both
        // usable; later sections are bundled onto the first.
        if media_index > 0 {
            continue;
        }

        if let Some(v) = line.strip_prefix("a=ice-ufrag:") {
            if ufrag.is_empty() {
                ufrag = v.to_string();
            }
        } else if let Some(v) = line.strip_prefix("a=ice-pwd:") {
            if pwd.is_empty() {
                pwd = v.to_string();
            }
        } else if let Some(v) = line.strip_prefix("a=mid:") {
            if mid.is_empty() {
                mid = v.to_string();
            }
        } else if let Some(v) = line.strip_prefix("a=candidate:") {
            candidates.push(v);
        }
    }

    let mut out: Vec<String> = Vec::with_capacity(candidates.len() + 5);
    if !ufrag.is_empty() {
        out.push(format!("a=ice-ufrag:{ufrag}"));
    }
    if !pwd.is_empty() {
        out.push(format!("a=ice-pwd:{pwd}"));
    }
    if !m_line.is_empty() {
        out.push(m_line);
    }
    if !mid.is_empty() {
        out.push(format!("a=mid:{mid}"));
    }
    for c in candidates {
        out.push(format!("a=candidate:{c}"));
    }
    out.push("a=end-of-candidates".to_string());

    out.join("\r\n") + "\r\n"
}

/// Folds the server's reply fragment into the answer already held, producing a
/// full SDP that `set_remote_description` accepts.
///
/// Only the ICE generation changes: credentials are replaced wherever they
/// appear, stale candidates are dropped, and the new ones are inserted into the
/// first media section. Everything else — m-lines, payload types, the DTLS
/// fingerprint, SSRCs — is carried over verbatim, because a restart is not
/// meant to renegotiate any of it.
///
/// Returns `None` if the fragment carries no credentials, which would mean it
/// is not a restart reply at all.
pub fn apply_ice_fragment(remote_sdp: &str, fragment: &str) -> Option<String> {
    let details = parse_ice_details(fragment);
    if details.ufrag.is_empty() || details.pwd.is_empty() {
        return None;
    }

    let mut out: Vec<String> = Vec::new();
    let mut media_index: i32 = -1;
    let mut inserted = false;

    // Rendered once and spliced in at the end of the first media section.
    let candidate_lines: Vec<String> = {
        let mut lines: Vec<String> = details
            .candidates
            .iter()
            .map(|c| format!("a=candidate:{c}"))
            .collect();
        if !lines.is_empty() {
            lines.push("a=end-of-candidates".to_string());
        }
        lines
    };

    for line in split_sdp_lines(remote_sdp) {
        if line.starts_with("m=") {
            // Leaving the first media section — the new candidates belong at
            // its end, after the attributes it already carries.
            if media_index == 0 && !inserted {
                inserted = true;
                out.extend(candidate_lines.iter().cloned());
            }
            media_index += 1;
            out.push(line.to_string());
        } else if line.starts_with("o=") {
            out.push(bump_sdp_origin(line));
        } else if line.starts_with("a=ice-ufrag:") {
            out.push(format!("a=ice-ufrag:{}", details.ufrag));
        } else if line.starts_with("a=ice-pwd:") {
            out.push(format!("a=ice-pwd:{}", details.pwd));
        } else if line.starts_with("a=candidate:") || line == "a=end-of-candidates" {
            // Previous ICE generation — dropped.
        } else {
            out.push(line.to_string());
        }
    }
    if !inserted {
        out.extend(candidate_lines);
    }

    Some(out.join("\r\n") + "\r\n")
}

/// Increments the session version in an `o=` line, which is how JSEP marks a
/// description as a new revision of the same session.
fn bump_sdp_origin(line: &str) -> String {
    let rest = line.strip_prefix("o=").unwrap_or(line);
    let mut fields: Vec<&str> = rest.split_whitespace().collect();
    if fields.len() < 6 {
        return line.to_string();
    }
    let bumped = match fields[2].parse::<u64>() {
        Ok(v) => (v + 1).to_string(),
        Err(_) => return line.to_string(),
    };
    fields[2] = &bumped;
    format!("o={}", fields.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REMOTE_ANSWER: &str = concat!(
        "v=0\r\n",
        "o=- 4611731400430051336 2 IN IP4 127.0.0.1\r\n",
        "s=-\r\n",
        "t=0 0\r\n",
        "a=group:BUNDLE 0 1\r\n",
        "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n",
        "c=IN IP4 0.0.0.0\r\n",
        "a=mid:0\r\n",
        "a=ice-ufrag:oldU\r\n",
        "a=ice-pwd:oldPassword0000000000\r\n",
        "a=fingerprint:sha-256 AA:BB:CC\r\n",
        "a=candidate:1 1 udp 2130706431 192.0.2.10 41000 typ host\r\n",
        "a=end-of-candidates\r\n",
        "a=rtpmap:111 opus/48000/2\r\n",
        "a=ssrc:12345 cname:stream\r\n",
        "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n",
        "a=mid:1\r\n",
        "a=ice-ufrag:oldU\r\n",
        "a=ice-pwd:oldPassword0000000000\r\n",
    );

    const SERVER_FRAGMENT: &str = concat!(
        "a=ice-ufrag:newU\r\n",
        "a=ice-pwd:newPassword111111111\r\n",
        "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n",
        "a=mid:0\r\n",
        "a=candidate:1 1 udp 2130706431 198.51.100.1 39132 typ host\r\n",
    );

    #[test]
    fn parses_credentials_and_candidates() {
        let details = parse_ice_details(SERVER_FRAGMENT);
        assert_eq!(details.ufrag, "newU");
        assert_eq!(details.pwd, "newPassword111111111");
        assert_eq!(details.candidates.len(), 1);
    }

    #[test]
    fn parses_lf_only_bodies() {
        let details = parse_ice_details("a=ice-ufrag:newU\na=ice-pwd:pw\n");
        assert_eq!(details.ufrag, "newU");
        assert_eq!(details.pwd, "pw");
    }

    #[test]
    fn builds_a_fragment_from_the_bundle_master_only() {
        let local = concat!(
            "v=0\r\n",
            "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n",
            "a=mid:0\r\n",
            "a=ice-ufrag:localU\r\n",
            "a=ice-pwd:localPassword222222\r\n",
            "a=candidate:1 1 udp 2130706431 198.51.100.7 51000 typ host\r\n",
            "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n",
            "a=mid:1\r\n",
            "a=candidate:9 1 udp 1 10.0.0.1 1 typ host\r\n",
        );
        let frag = ice_fragment_from_sdp(local);

        assert!(frag.contains("a=ice-ufrag:localU"));
        assert!(frag.contains("a=ice-pwd:localPassword222222"));
        assert!(frag.contains("m=audio 9 UDP/TLS/RTP/SAVPF 111"));
        assert!(frag.contains("a=mid:0"));
        assert!(frag.contains("198.51.100.7"));
        assert!(frag.ends_with("a=end-of-candidates\r\n"));
        // Bundled sections are not described.
        assert!(!frag.contains("m=application"));
        assert!(!frag.contains("10.0.0.1"));
    }

    #[test]
    fn folds_a_fragment_into_the_stored_answer() {
        let applied = apply_ice_fragment(REMOTE_ANSWER, SERVER_FRAGMENT).unwrap();

        assert!(!applied.contains("oldU"));
        assert!(!applied.contains("oldPassword0000000000"));
        // Both bundled sections must agree on the new credentials.
        assert_eq!(applied.matches("a=ice-ufrag:newU").count(), 2);
        assert_eq!(applied.matches("a=ice-pwd:newPassword111111111").count(), 2);

        // Stale candidates go, new ones arrive.
        assert!(!applied.contains("192.0.2.10"));
        assert!(applied.contains("a=candidate:1 1 udp 2130706431 198.51.100.1 39132 typ host"));

        // Everything the transport is not responsible for survives verbatim.
        for must in [
            "a=mid:0",
            "a=mid:1",
            "a=ssrc:12345 cname:stream",
            "a=fingerprint:sha-256 AA:BB:CC",
            "a=rtpmap:111 opus/48000/2",
            "a=group:BUNDLE 0 1",
            "m=application 9 UDP/DTLS/SCTP webrtc-datachannel",
        ] {
            assert!(applied.contains(must), "rewrite dropped {must}");
        }

        // A new revision of the same session.
        assert!(applied.contains("o=- 4611731400430051336 3 IN IP4 127.0.0.1"));
    }

    #[test]
    fn candidates_land_in_the_first_media_section() {
        let applied = apply_ice_fragment(REMOTE_ANSWER, SERVER_FRAGMENT).unwrap();
        let audio = applied.find("m=audio").unwrap();
        let app = applied.find("m=application").unwrap();
        let candidate = applied.find("a=candidate:").unwrap();
        assert!(candidate > audio && candidate < app);
    }

    #[test]
    fn rejects_a_fragment_without_credentials() {
        let trickle = "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=candidate:1 1 udp 1 1.2.3.4 1 typ host\r\n";
        assert!(apply_ice_fragment(REMOTE_ANSWER, trickle).is_none());
    }

    #[test]
    fn leaves_a_malformed_origin_alone() {
        assert_eq!(bump_sdp_origin("o=- 123"), "o=- 123");
        assert_eq!(
            bump_sdp_origin("o=- 123 abc IN IP4 127.0.0.1"),
            "o=- 123 abc IN IP4 127.0.0.1"
        );
    }
}
