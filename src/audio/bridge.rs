use bytes::Bytes;
use opus::{Application, Channels, Decoder, Encoder};
use ringbuf::traits::{Consumer, Observer, Producer};
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

/// Opus frame duration in milliseconds.
/// Mumble uses 10ms frames (iFrameSize = SAMPLE_RATE / 100 = 480).
const FRAME_DURATION_MS: usize = 10;

/// Samples per Opus frame at 48kHz mono (10ms).
const SAMPLES_PER_FRAME: usize = 48000 * FRAME_DURATION_MS / 1000; // 480

/// Maximum encoded Opus frame size in bytes.
const MAX_OPUS_FRAME_SIZE: usize = 4000;

/// Audio bridge that runs two async tasks:
/// 1. Encoder: reads PCM from ring buffer → encodes Opus → sends to Mumble
/// 2. Decoder: receives Opus from Mumble → decodes to PCM → writes to ring buffer
pub struct AudioBridge;

impl AudioBridge {
    /// Spawn the encoder task: reads PCM from the SIP→Mumble ring buffer,
    /// encodes to Opus, and sends via the provided sender.
    ///
    /// Returns a JoinHandle for the spawned task.
    pub fn spawn_encoder(
        mut pcm_consumer: ringbuf::HeapCons<i16>,
        opus_tx: mpsc::UnboundedSender<(u64, Bytes)>,
        sample_rate: u32,
        opus_bitrate: u32,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut encoder = match Encoder::new(sample_rate, Channels::Mono, Application::Voip) {
                Ok(mut enc) => {
                    if let Err(e) = enc.set_bitrate(opus::Bitrate::Bits(opus_bitrate as i32)) {
                        warn!("Failed to set Opus bitrate to {}: {}", opus_bitrate, e);
                    }
                    enc
                }
                Err(e) => {
                    error!("Failed to create Opus encoder: {}", e);
                    return;
                }
            };

            let mut seq_num: u64 = 0;
            let mut pcm_buf = vec![0i16; SAMPLES_PER_FRAME];
            let mut opus_buf = vec![0u8; MAX_OPUS_FRAME_SIZE];

            // Poll at 5ms for low latency — encode and send as soon as a
            // full frame is available. Mumble handles jitter on its end.
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_millis(5));

            loop {
                interval.tick().await;

                // Need a full frame to encode
                if pcm_consumer.occupied_len() < SAMPLES_PER_FRAME {
                    continue;
                }

                let read = pcm_consumer.pop_slice(&mut pcm_buf);
                if read < SAMPLES_PER_FRAME {
                    pcm_buf[read..].fill(0);
                }

                // Encode to Opus
                match encoder.encode(&pcm_buf, &mut opus_buf) {
                    Ok(encoded_len) => {
                        let opus_data = Bytes::copy_from_slice(&opus_buf[..encoded_len]);
                        if opus_tx.send((seq_num, opus_data)).is_err() {
                            debug!("Opus output channel closed, stopping encoder");
                            break;
                        }
                        // Mumble seq_num is a frame counter — the server multiplies
                        // it by frame size internally for jitter buffer timestamps.
                        seq_num += 1;
                    }
                    Err(e) => {
                        warn!("Opus encode error: {}", e);
                    }
                }
            }
        })
    }

    /// Spawn the decoder task: receives Opus frames and decodes them to PCM,
    /// writing into the Mumble→SIP ring buffer.
    ///
    /// Returns a JoinHandle for the spawned task.
    pub fn spawn_decoder(
        mut opus_rx: mpsc::UnboundedReceiver<Bytes>,
        mut pcm_producer: ringbuf::HeapProd<i16>,
        sample_rate: u32,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut decoder = match Decoder::new(sample_rate, Channels::Mono) {
                Ok(dec) => dec,
                Err(e) => {
                    error!("Failed to create Opus decoder: {}", e);
                    return;
                }
            };

            let mut pcm_buf = vec![0i16; SAMPLES_PER_FRAME];

            while let Some(opus_data) = opus_rx.recv().await {
                match decoder.decode(&opus_data, &mut pcm_buf, false) {
                    Ok(decoded_samples) => {
                        let written = pcm_producer.push_slice(&pcm_buf[..decoded_samples]);
                        if written < decoded_samples {
                            // Ring buffer full — dropping oldest samples is acceptable
                            // for real-time audio
                        }
                    }
                    Err(e) => {
                        warn!("Opus decode error: {}", e);
                    }
                }
            }

            debug!("Opus decoder stopped");
        })
    }
}
