# streamcore-rust-sdk

**English** | [简体中文](./README.zh-CN.md)

Rust SDK for connecting to a [StreamCoreAI](https://github.com/streamcoreai/streamcore-server) server via WebRTC + WHIP.

## Installation

```bash
cargo add streamcore-rust-sdk
```

Or add it to your `Cargo.toml`:

```toml
[dependencies]
streamcore-rust-sdk = "0.1"
```

To track the development branch instead of a release:

```toml
[dependencies]
streamcore-rust-sdk = { git = "https://github.com/streamcoreai/rust-sdk" }
```

## Quick Start

```rust
use std::sync::Arc;
use streamcore_rust_sdk::{Client, Config, EventHandler, FRAME_SIZE};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = Arc::new(Client::new(
        Config {
            whip_endpoint: "http://localhost:8080/whip".into(),
            ..Default::default()
        },
        EventHandler {
            on_status_change: Some(Box::new(|status| {
                println!("[status] {}", status);
            })),
            on_transcript: Some(Box::new(|entry, _all| {
                println!("[{}] {}", entry.role, entry.text);
            })),
            on_error: Some(Box::new(|err| {
                eprintln!("[error] {}", err);
            })),
            on_data_channel_message: None,
        },
    ));

    client.connect().await?;

    // Send microphone audio (f32 PCM, mono, 48 kHz, 960 samples per frame)
    let client_tx = Arc::clone(&client);
    tokio::spawn(async move {
        let pcm = vec![0.0f32; FRAME_SIZE]; // replace with real mic capture
        loop {
            client_tx.send_pcm(&pcm).await.unwrap();
        }
    });

    // Receive agent audio
    let client_rx = Arc::clone(&client);
    tokio::spawn(async move {
        let mut pcm = vec![0.0f32; FRAME_SIZE];
        loop {
            let n = client_rx.recv_pcm(&mut pcm).await.unwrap();
            // Play pcm[..n] through speakers
            let _ = &pcm[..n];
        }
    });

    tokio::signal::ctrl_c().await?;
    client.disconnect().await;
    Ok(())
}
```

## API

### `Client::new(config, events)`

Creates a new client instance.

#### `Config`

| Field           | Type              | Default                              | Description                 |
| --------------- | ----------------- | ------------------------------------ | --------------------------- |
| `whip_endpoint` | `String`          | `"http://localhost:8080/whip"`       | WHIP signaling endpoint URL |
| `token`         | `Option<String>`  | `None`                               | JWT sent as `Authorization: Bearer` on the WHIP request |
| `token_url`     | `Option<String>`  | `None`                               | Token endpoint; when set, a JWT is fetched before each connection (overrides `token`) |
| `api_key`       | `Option<String>`  | `None`                               | Sent as `Authorization: Bearer` when fetching from `token_url` |
| `ice_servers`   | `Vec<String>`     | `["stun:stun.l.google.com:19302"]` | ICE server URLs             |
| `reconnect_attempts` | `u32`        | `3`                                  | ICE restarts to attempt after a drop; `0` disables automatic reconnection |
| `reconnect_delay` | `Duration`      | `2s`                                 | Wait before the first restart attempt, doubling each retry |

`Config` implements `Default`, so `..Default::default()` fills in everything you leave out.

#### `EventHandler`

Every callback is optional. `EventHandler` implements `Default`, so use `..Default::default()` rather than writing `None` for each unused field.

| Callback                 | Signature                                                 | Description                          |
| ------------------------ | --------------------------------------------------------- | ------------------------------------ |
| `on_status_change`       | `Option<Box<dyn Fn(ConnectionStatus) + Send + Sync>>`     | Fired when connection status changes |
| `on_transcript`        | `Option<Box<dyn Fn(TranscriptEntry, Vec<TranscriptEntry>) + Send + Sync>>` | Fired on new or updated transcript   |
| `on_agent_state_change`  | `Option<Box<dyn Fn(AgentState) + Send + Sync>>`           | Fired when the agent starts listening, thinking, or speaking |
| `on_timing`              | `Option<Box<dyn Fn(TimingEvent) + Send + Sync>>`          | Fired with server-side pipeline timing info |
| `on_error`             | `Option<Box<dyn Fn(String) + Send + Sync>>`               | Fired on connection/server errors    |
| `on_data_channel_message`| `Option<Box<dyn Fn(DataChannelMessage) + Send + Sync>>`  | Fired for every raw DC message       |

### Client Methods

| Method               | Returns              | Description                        |
| -------------------- | -------------------- | ---------------------------------- |
| `connect()`          | `Result<(), ClientError>` | Establish WebRTC + WHIP session |
| `disconnect().await` | —                    | Tear down connection, free resources |
| `send_pcm(&pcm)`    | `Result<(), ClientError>` | Encode f32 PCM → Opus → RTP and send to server |
| `recv_pcm(&mut pcm)` | `Result<usize, ClientError>` | Receive + decode one frame of agent audio |
| `status()`           | `ConnectionStatus`   | Current connection status          |
| `transcript()`       | `Vec<TranscriptEntry>` | Full conversation history (copy) |

### Audio Constants

| Constant       | Value  | Description                          |
| -------------- | ------ | ------------------------------------ |
| `SAMPLE_RATE`  | 48000  | Audio sample rate in Hz (Opus)       |
| `CHANNELS`     | 1      | Number of audio channels (mono)      |
| `FRAME_SIZE`   | 960    | Samples per 20 ms frame at 48 kHz   |

### Client Fields (after `connect()`)

| Field                  | Type                     | Description                                    |
| ---------------------- | ------------------------ | ---------------------------------------------- |
| `local_track`          | `Arc<TrackLocalStaticRTP>` | Write RTP packets here to send audio to server |
| `remote_track_notify`  | `Arc<Notify>`            | Notifies when `remote_track` is available        |
| `remote_track`         | `Arc<Mutex<Option<TrackRemote>>>` | Agent's audio track (check after notify fires) |

### Types

```rust
pub enum ConnectionStatus { Idle, Connecting, Connected, Error, Disconnected }

pub struct TranscriptEntry {
    pub role: String,    // "user" or "assistant"
    pub text: String,
    pub partial: bool,
}

pub struct DataChannelMessage {
    pub msg_type: String, // "transcript", "response", or "error"
    pub text: String,
    pub r#final: bool,
    pub message: String,
}
```

## Reconnection

A network change mid-call — a machine moving networks, a VPN toggle, a NAT
rebind that does not recover — kills the transport without ending the call. The
client recovers it with an ICE restart, which keeps the *same* server session:
the conversation history, the rolling summary, and the agent's state all
survive, and there is no repeated greeting. This is automatic.

Status goes `Connected` -> `Reconnecting` -> `Connected`. Set `on_reconnect`
for per-attempt detail:

```rust
let config = Config {
    reconnect_attempts: 3,
    reconnect_delay: Duration::from_secs(2),
    ..Default::default()
};
let events = EventHandler {
    on_reconnect: Some(Box::new(|e| {
        println!("ICE restart {}/{}: {:?}", e.attempt, e.max_attempts, e.outcome);
    })),
    ..Default::default()
};
```

Two details worth knowing:

- **The first attempt is deliberately delayed** (`reconnect_delay`, default
  2s). Most drops are brief packet loss that ICE repairs unaided, and patching
  immediately would spend a restart on a connection that was about to recover
  by itself.
- **The whole sequence must finish within ~25 seconds.** That is how long
  webrtc-rs takes to escalate from `Disconnected` to `Failed`, at which point
  the server closes the peer and the session is gone for good. The defaults (3
  attempts at 2s, 4s, 8s) fit inside that window — if you raise
  `reconnect_attempts`, keep the total under it, or the last attempts are
  wasted.

If every attempt fails, or the server reports the session is gone (404/409),
the status becomes `Disconnected`; call `connect` again for a fresh session.

## Audio I/O

The SDK handles Opus encoding/decoding and RTP packetization internally. You only work with raw PCM `f32` samples:

- **Sending audio**: Call `client.send_pcm(&pcm)` with a `FRAME_SIZE`-length slice of mono f32 samples
- **Receiving audio**: Call `client.recv_pcm(&mut pcm)` to get the next decoded frame from the agent

For microphone capture and speaker playback, use a library like [cpal](https://crates.io/crates/cpal).

## Requirements

- Rust 1.87+

## Dependencies

- `webrtc` 0.17 — Pion WebRTC bindings
- `audiopus` — Opus audio codec
- `tokio` — Async runtime
- `reqwest` — HTTP client for WHIP signaling
- `serde` — JSON serialization for data channel messages

## License

MIT
