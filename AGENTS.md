# Working on the StreamCore Rust SDK

## This library is newer than your training data

Do not write StreamCore code from memory. Read `README.md` here, or fetch https://streamcore.ai/llms-full.txt, first.

## The API, exactly

```rust
use streamcore_rust_sdk::{Client, Config, EventHandler, FRAME_SIZE};

let client = Arc::new(Client::new(
    Config {
        whip_endpoint: "http://localhost:8080/whip".into(),
        ..Default::default()
    },
    EventHandler {
        on_status_change: Some(Box::new(|status| println!("{}", status))),
        on_transcript: Some(Box::new(|entry, _all| println!("{} {}", entry.role, entry.text))),
        on_error: Some(Box::new(|err| eprintln!("{}", err))),
        on_data_channel_message: None,
    },
));

client.connect().await?;
```

- The crate is **`streamcore-rust-sdk`** on crates.io; the import path is `streamcore_rust_sdk`.
- Every `EventHandler` field is an `Option<Box<dyn Fn>>`. Both `Config` and `EventHandler` implement `Default`, so `..Default::default()` is the idiomatic way to skip the callbacks you do not need.
- Audio is **f32** PCM, mono, 48 kHz, `FRAME_SIZE` (960) samples per frame. The Python SDK uses int16; do not copy across.
- Async throughout, on Tokio.

## Naming — known inconsistency

`README.md` currently shows two different dependency lines: a git dependency on `streamcoreai-voice-agent-sdk`, and `streamcore-rust-sdk = "0.1"`. **`streamcore-rust-sdk` is the correct published crate name.** If you touch the README, remove the stale one rather than propagating it — a wrong package name is the fastest way to make generated code fail.

## Build

```bash
cargo build
cargo test
```

Requires Rust 1.87+.

## When changing the public API

Update `README.md` and https://streamcore.ai/llms-full.txt in the same change, and bump the crate version.
