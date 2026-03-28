use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::sync::Mutex;

use audiopus::coder::{Decoder as OpusDecoder, Encoder as OpusEncoder};
use audiopus::{Application, Channels, SampleRate};
use webrtc::rtp::header::Header as RtpHeader;
use webrtc::rtp::packet::Packet as RtpPacket;
use webrtc::track::track_local::TrackLocalWriter;

use crate::client::ClientError;
use crate::Client;

/// Audio sample rate in Hz (48 kHz, required by Opus).
pub const SAMPLE_RATE: u32 = 48_000;

/// Number of audio channels (mono).
pub const CHANNELS: usize = 1;

/// Number of samples per 20 ms frame at 48 kHz.
pub const FRAME_SIZE: usize = 960;

const MAX_OPUS_BYTES: usize = 1500;
const RTP_PAYLOAD_TYPE: u8 = 111;
const RTP_SSRC: u32 = 0xDEAD_BEEF;

/// Internal audio encoding/decoding state.
pub(crate) struct AudioState {
    encoder: OpusEncoder,
    decoder: Mutex<Option<OpusDecoder>>,
    seq: AtomicU16,
    ts: AtomicU32,
}

// Safety: OpusEncoder/OpusDecoder wrap raw pointers but are only accessed
// through &self methods (encoder) or behind a Mutex (decoder). We guarantee
// no concurrent unsynchronised access.
unsafe impl Send for AudioState {}
unsafe impl Sync for AudioState {}

impl AudioState {
    pub(crate) fn new() -> Result<Self, ClientError> {
        let encoder = OpusEncoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)
            .map_err(|e| ClientError::Audio(format!("opus encoder: {e}")))?;
        Ok(Self {
            encoder,
            decoder: Mutex::new(None),
            seq: AtomicU16::new(0),
            ts: AtomicU32::new(0),
        })
    }
}

impl Client {
    /// Encode a 20 ms frame of f32 PCM audio (mono, 48 kHz, 960 samples) and
    /// send it to the voice agent server as an RTP/Opus packet.
    pub async fn send_pcm(&self, pcm: &[f32]) -> Result<(), ClientError> {
        let mut opus_buf = vec![0u8; MAX_OPUS_BYTES];

        let n = self
            .audio
            .encoder
            .encode_float(pcm, &mut opus_buf)
            .map_err(|e| ClientError::Audio(format!("opus encode: {e}")))?;

        let seq = self.audio.seq.fetch_add(1, Ordering::Relaxed);
        let ts = self
            .audio
            .ts
            .fetch_add(FRAME_SIZE as u32, Ordering::Relaxed);

        let pkt = RtpPacket {
            header: RtpHeader {
                version: 2,
                payload_type: RTP_PAYLOAD_TYPE,
                sequence_number: seq,
                timestamp: ts,
                ssrc: RTP_SSRC,
                ..Default::default()
            },
            payload: bytes::Bytes::copy_from_slice(&opus_buf[..n]),
        };

        self.local_track.write_rtp(&pkt).await?;
        Ok(())
    }

    /// Block until a frame of audio is received from the agent, decode the Opus
    /// payload, and write f32 PCM samples into `pcm`.
    ///
    /// The `pcm` slice should have capacity for at least [`FRAME_SIZE`] (960) samples.
    /// Returns the number of decoded samples.
    pub async fn recv_pcm(&self, pcm: &mut [f32]) -> Result<usize, ClientError> {
        // Get (or wait for) the remote track.
        let track = {
            let guard = self.remote_track.lock().unwrap();
            guard.clone()
        };

        let track = match track {
            Some(t) => t,
            None => {
                self.remote_track_notify.notified().await;
                self.remote_track
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or_else(|| ClientError::Audio("remote track not available".into()))?
            }
        };

        let mut buf = vec![0u8; 1500];

        loop {
            let (rtp_pkt, _) = track
                .read(&mut buf)
                .await
                .map_err(|e| ClientError::Audio(format!("track read: {e}")))?;

            if rtp_pkt.payload.is_empty() {
                continue;
            }

            // Lazy-init decoder on first call.
            let mut dec_guard = self.audio.decoder.lock().unwrap();
            if dec_guard.is_none() {
                let dec = OpusDecoder::new(SampleRate::Hz48000, Channels::Mono)
                    .map_err(|e| ClientError::Audio(format!("opus decoder: {e}")))?;
                *dec_guard = Some(dec);
            }
            let dec = dec_guard.as_mut().unwrap();

            let pkt_data: audiopus::packet::Packet<'_> = rtp_pkt
                .payload
                .as_ref()
                .try_into()
                .map_err(|e: audiopus::Error| ClientError::Audio(format!("opus packet: {e}")))?;
            let mut_pcm: audiopus::MutSignals<'_, f32> = pcm
                .try_into()
                .map_err(|e: audiopus::Error| ClientError::Audio(format!("opus signal: {e}")))?;

            match dec.decode_float(Some(pkt_data), mut_pcm, false) {
                Ok(n) => return Ok(n),
                Err(e) => {
                    // Log and try next packet, matching Go SDK behaviour.
                    tracing::warn!("opus decode: {e}");
                    continue;
                }
            }
        }
    }
}
