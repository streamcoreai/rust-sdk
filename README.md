# streamcore-rust-sdk

Rust SDK for connecting to a [StreamCoreAI](https://github.com/streamcoreai/streamcore-server) server via WebRTC + WHIP.

## Installation

```bash
[dependencies]
streamcoreai-voice-agent-sdk = { git = "https://github.com/streamcoreai/rust-sdk" }
```

Or add to your `Cargo.toml`:

```toml
streamcore-rust-sdk = "0.1"
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

| Field           | Type           | Default                              | Description                 |
| --------------- | -------------- | ------------------------------------ | --------------------------- |
| `whip_endpoint` | `String`       | `"http://localhost:8080/whip"`       | WHIP signaling endpoint URL |
| `ice_servers`   | `Vec<String>`  | `["stun:stun.l.google.com:19302"]` | ICE server URLs             |

#### `EventHandler`

| Callback                 | Signature                                                 | Description                          |
| ------------------------ | --------------------------------------------------------- | ------------------------------------ |
| `on_status_change`       | `Option<Box<dyn Fn(ConnectionStatus) + Send + Sync>>`     | Fired when connection status changes |
| `on_transcript`        | `Option<Box<dyn Fn(TranscriptEntry, Vec<TranscriptEntry>)>>` | Fired on new or updated transcript   |
| `on_error`             | `Option<Box<dyn Fn(String) + Send + Sync>>`               | Fired on connection/server errors    |
| `on_data_channel_message`| `Option<Box<dyn Fn(DataChannelMessage)>>`                | Fired for every raw DC message       |

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
