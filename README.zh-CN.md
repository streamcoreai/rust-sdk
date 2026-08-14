# streamcore-rust-sdk

[English](./README.md) | **简体中文**

Rust SDK，通过 WebRTC + WHIP 连接 [StreamCoreAI](https://github.com/streamcoreai/streamcore-server) 服务端。

## 安装

```bash
cargo add streamcore-rust-sdk
```

或者加到你的 `Cargo.toml`：

```toml
[dependencies]
streamcore-rust-sdk = "0.1"
```

若想跟踪开发分支而非某个发布版本：

```toml
[dependencies]
streamcore-rust-sdk = { git = "https://github.com/streamcoreai/rust-sdk" }
```

## 快速开始

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

创建一个新的客户端实例。

#### `Config`

| 字段           | 类型              | 默认值                              | 说明                 |
| --------------- | ----------------- | ------------------------------------ | --------------------------- |
| `whip_endpoint` | `String`          | `"http://localhost:8080/whip"`       | WHIP 信令端点 URL |
| `token`         | `Option<String>`  | `None`                               | 在 WHIP 请求中以 `Authorization: Bearer` 发送的 JWT |
| `token_url`     | `Option<String>`  | `None`                               | token 端点；设置后每次连接前都会取一次 JWT（优先于 `token`） |
| `api_key`       | `Option<String>`  | `None`                               | 从 `token_url` 取 token 时以 `Authorization: Bearer` 发送 |
| `resource_id`   | `Option<String>`  | `None`                               | 通话里的是谁，会转发给外部 agent，使其把记忆划到人而非单次通话上。设置了 `token_url` 时随取 token 的请求体发送（由服务端签进 token），否则作为 `X-StreamCore-Resource-Id` 请求头发送 |
| `ice_servers`   | `Vec<String>`     | `["stun:stun.l.google.com:19302"]` | ICE 服务器 URL             |

`Config` 实现了 `Default`，所以 `..Default::default()` 会补全你没写的字段。

#### `EventHandler`

所有回调都是可选的。`EventHandler` 实现了 `Default`，所以请用 `..Default::default()` 而不是给每个不用的字段写 `None`。

| 回调                 | 签名                                                 | 说明                          |
| ------------------------ | --------------------------------------------------------- | ------------------------------------ |
| `on_status_change`       | `Option<Box<dyn Fn(ConnectionStatus) + Send + Sync>>`     | 连接状态变化时触发 |
| `on_transcript`        | `Option<Box<dyn Fn(TranscriptEntry, Vec<TranscriptEntry>) + Send + Sync>>` | 有新的或更新的转写时触发   |
| `on_agent_state_change`  | `Option<Box<dyn Fn(AgentState) + Send + Sync>>`           | 智能体开始聆听、思考或说话时触发 |
| `on_timing`              | `Option<Box<dyn Fn(TimingEvent) + Send + Sync>>`          | 携带服务端流水线耗时信息 |
| `on_error`             | `Option<Box<dyn Fn(String) + Send + Sync>>`               | 连接/服务端错误时触发    |
| `on_data_channel_message`| `Option<Box<dyn Fn(DataChannelMessage) + Send + Sync>>`  | 每条原始 DataChannel 消息都会触发       |

### 客户端方法

| 方法               | 返回值              | 说明                        |
| -------------------- | -------------------- | ---------------------------------- |
| `connect()`          | `Result<(), ClientError>` | 建立 WebRTC + WHIP 会话 |
| `disconnect().await` | —                    | 拆除连接、释放资源 |
| `send_pcm(&pcm)`    | `Result<(), ClientError>` | 将 f32 PCM 编码为 Opus → RTP 并发送到服务端 |
| `recv_pcm(&mut pcm)` | `Result<usize, ClientError>` | 接收并解码智能体音频的一帧 |
| `status()`           | `ConnectionStatus`   | 当前连接状态          |
| `transcript()`       | `Vec<TranscriptEntry>` | 完整对话历史（副本） |

### 音频常量

| 常量       | 值  | 说明                          |
| -------------- | ------ | ------------------------------------ |
| `SAMPLE_RATE`  | 48000  | 音频采样率（Hz，Opus）       |
| `CHANNELS`     | 1      | 音频声道数（单声道）      |
| `FRAME_SIZE`   | 960    | 48 kHz 下每 20 ms 帧的采样点数   |

### 客户端字段（`connect()` 之后）

| 字段                  | 类型                     | 说明                                    |
| ---------------------- | ------------------------ | ---------------------------------------------- |
| `local_track`          | `Arc<TrackLocalStaticRTP>` | 向此处写入 RTP 包即可把音频发给服务端 |
| `remote_track_notify`  | `Arc<Notify>`            | 当 `remote_track` 可用时通知        |
| `remote_track`         | `Arc<Mutex<Option<TrackRemote>>>` | 智能体的音频 track（收到通知后检查） |

### 类型

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

## 音频 I/O

SDK 内部处理 Opus 编解码与 RTP 打包。你只需要面对原始的 PCM `f32` 采样：

- **发送音频**：用长度为 `FRAME_SIZE` 的单声道 f32 切片调用 `client.send_pcm(&pcm)`
- **接收音频**：调用 `client.recv_pcm(&mut pcm)` 获取来自智能体的下一帧解码音频

麦克风采集与扬声器播放请使用 [cpal](https://crates.io/crates/cpal) 之类的库。

## 环境要求

- Rust 1.87+

## 依赖

- `webrtc` 0.17 —— Pion WebRTC 绑定
- `audiopus` —— Opus 音频编解码器
- `tokio` —— 异步运行时
- `reqwest` —— 用于 WHIP 信令的 HTTP 客户端
- `serde` —— DataChannel 消息的 JSON 序列化

## 许可证

MIT
