use bytes::Bytes;
use opus::{Application, Channels, Decoder, Encoder};
use ringbuf::traits::{Consumer, Observer, Producer};
use std::collections::{HashMap, VecDeque};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::time::{Duration, Instant, MissedTickBehavior};
use tracing::{debug, error, trace, warn};

/// Opus frame duration in milliseconds.
/// Mumble uses 10ms frames (iFrameSize = SAMPLE_RATE / 100 = 480).
pub const FRAME_DURATION_MS: u32 = 10;

/// Maximum encoded Opus frame size in bytes.
const MAX_OPUS_FRAME_SIZE: usize = 4000;
/// How often to log dropped frames to avoid log spam.
const DROP_LOG_EVERY_FRAMES: u64 = 100;
/// Average absolute sample threshold for transmission gating, matching mumble-web2.
const SILENCE_AVG_ABS_THRESHOLD: f32 = 0.001 * (i16::MAX as f32);
/// Keep transmitting briefly after dropping below threshold.
const SILENCE_HOLD_FRAMES_MAX: u32 = 200 / FRAME_DURATION_MS; // 200ms hold
/// Remove idle speaker decoders after this long without packets.
const SPEAKER_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

pub fn frame_samples(sample_rate: u32) -> usize {
    ((sample_rate as usize) * (FRAME_DURATION_MS as usize)) / 1000
}

/// Audio bridge that runs two async tasks:
/// 1. Encoder: reads PCM from ring buffer → encodes Opus → sends to Mumble
/// 2. Decoder: receives Opus from Mumble → decodes to PCM → writes to ring buffer
pub struct AudioBridge;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransmitState {
    Transmitting,
    Terminator,
    Silent,
}

struct SpeakerState {
    decoder: Decoder,
    pending_packets: VecDeque<Bytes>,
    last_packet_at: Instant,
    dropped_packets: u64,
}

impl AudioBridge {
    /// Spawn the encoder task: reads PCM from the SIP→Mumble ring buffer,
    /// encodes to Opus, and sends via the provided sender.
    ///
    /// Returns a JoinHandle for the spawned task.
    pub fn spawn_encoder(
        mut pcm_consumer: ringbuf::HeapCons<i16>,
        opus_tx: mpsc::Sender<(u64, Bytes, bool)>,
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

            let frame_samples = frame_samples(sample_rate);
            let mut seq_num: u64 = 0;
            let mut dropped_frames: u64 = 0;
            let mut pcm_buf = vec![0i16; frame_samples];
            let silence_buf = vec![0i16; frame_samples];
            let mut opus_buf = vec![0u8; MAX_OPUS_FRAME_SIZE];
            let mut was_transmitting = false;
            let mut hold_frames = 0u32;

            // Poll at 5ms for low latency — encode and send as soon as a
            // full frame is available. Mumble handles jitter on its end.
            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(5));

            loop {
                interval.tick().await;

                // Need a full frame to encode
                if pcm_consumer.occupied_len() < frame_samples {
                    continue;
                }

                let read = pcm_consumer.pop_slice(&mut pcm_buf);
                if read < frame_samples {
                    pcm_buf[read..].fill(0);
                }

                let avg_abs = pcm_buf
                    .iter()
                    .map(|s| (*s as i32).abs() as f32)
                    .sum::<f32>()
                    / frame_samples as f32;
                let above_threshold = avg_abs >= SILENCE_AVG_ABS_THRESHOLD;

                let transmit_state = if above_threshold {
                    hold_frames = 0;
                    was_transmitting = true;
                    TransmitState::Transmitting
                } else if was_transmitting && hold_frames < SILENCE_HOLD_FRAMES_MAX {
                    hold_frames += 1;
                    TransmitState::Transmitting
                } else if was_transmitting {
                    was_transmitting = false;
                    hold_frames = 0;
                    TransmitState::Terminator
                } else {
                    TransmitState::Silent
                };

                let (frame_to_encode, end_of_transmission) = match transmit_state {
                    TransmitState::Transmitting => (&pcm_buf[..], false),
                    TransmitState::Terminator => (&silence_buf[..], true),
                    TransmitState::Silent => continue,
                };

                // Encode to Opus.
                match encoder.encode(frame_to_encode, &mut opus_buf) {
                    Ok(encoded_len) => {
                        let opus_data = Bytes::copy_from_slice(&opus_buf[..encoded_len]);
                        match opus_tx.try_send((seq_num, opus_data, end_of_transmission)) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => {
                                dropped_frames += 1;
                                if dropped_frames % DROP_LOG_EVERY_FRAMES == 0 {
                                    warn!(
                                        "Dropped {} SIP->Mumble Opus frames due to full queue",
                                        dropped_frames
                                    );
                                }
                            }
                            Err(TrySendError::Closed(_)) => {
                                debug!("Opus output channel closed, stopping encoder");
                                break;
                            }
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

    /// Spawn a mixed decoder task:
    /// - decodes each speaker using its own Opus decoder state
    /// - mixes active speakers into one 10ms PCM frame
    /// - mixes in queued sound effects (join/leave chimes, etc.)
    /// - outputs exactly one mixed frame per tick to match realtime playout
    pub fn spawn_mixed_decoder(
        mut opus_rx: mpsc::Receiver<(u32, Bytes)>,
        mut sound_rx: mpsc::UnboundedReceiver<Vec<i16>>,
        mut pcm_producer: ringbuf::HeapProd<i16>,
        sample_rate: u32,
        jitter_frames: usize,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let frame_samples = frame_samples(sample_rate);
            let mut speakers: HashMap<u32, SpeakerState> = HashMap::new();
            let mut mix_accum = vec![0i32; frame_samples];
            let mut mixed_frame = vec![0i16; frame_samples];
            let mut decode_buf = vec![0i16; frame_samples];
            let max_packets_per_speaker = (jitter_frames * 2).max(4);
            let mut dropped_samples: u64 = 0;
            let mut next_drop_log: u64 = sample_rate as u64; // ~1s of audio.
            let mut sound_queue: VecDeque<i16> = VecDeque::new();
            let mut sound_closed = false;
            let mut interval =
                tokio::time::interval(Duration::from_millis(FRAME_DURATION_MS as u64));
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    biased;
                    _ = interval.tick() => {
                        mix_accum.fill(0);
                        let mut active_speakers: usize = 0;
                        let now = Instant::now();
                        let mut stale_speakers = Vec::new();

                        for (session_id, state) in speakers.iter_mut() {
                            let Some(opus_data) = state.pending_packets.pop_front() else {
                                if now.duration_since(state.last_packet_at) > SPEAKER_IDLE_TIMEOUT {
                                    stale_speakers.push(*session_id);
                                }
                                continue;
                            };

                            match state.decoder.decode(&opus_data, &mut decode_buf, false) {
                                Ok(decoded_samples) => {
                                    if decoded_samples == 0 {
                                        continue;
                                    }
                                    active_speakers += 1;
                                    let mix_len = decoded_samples.min(frame_samples);
                                    for idx in 0..mix_len {
                                        mix_accum[idx] += decode_buf[idx] as i32;
                                    }
                                }
                                Err(e) => {
                                    warn!("Opus decode error for speaker {}: {}", session_id, e);
                                }
                            }
                        }

                        for session_id in stale_speakers {
                            speakers.remove(&session_id);
                        }

                        // Mix in queued sound effect samples
                        let has_sound = !sound_queue.is_empty();
                        if has_sound {
                            let drain_count = frame_samples.min(sound_queue.len());
                            for sample in mix_accum.iter_mut().take(drain_count) {
                                *sample += sound_queue.pop_front().unwrap() as i32;
                            }
                        }

                        let active_sources = active_speakers + if has_sound { 1 } else { 0 };
                        if active_sources == 0 {
                            continue;
                        }

                        for idx in 0..frame_samples {
                            mixed_frame[idx] = mix_accum[idx].clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                        }

                        let written = pcm_producer.push_slice(&mixed_frame);
                        if written < frame_samples {
                            dropped_samples += (frame_samples - written) as u64;
                            if dropped_samples >= next_drop_log {
                                warn!(
                                    "Dropped {} mixed Mumble->SIP PCM samples due to full ring buffer",
                                    dropped_samples
                                );
                                next_drop_log += sample_rate as u64;
                            }
                        }
                    }
                    maybe_packet = opus_rx.recv() => {
                        let Some((session_id, opus_data)) = maybe_packet else {
                            break;
                        };

                        let state = match speakers.entry(session_id) {
                            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                            std::collections::hash_map::Entry::Vacant(entry) => {
                                let decoder = match Decoder::new(sample_rate, Channels::Mono) {
                                    Ok(dec) => dec,
                                    Err(e) => {
                                        warn!("Failed to create Opus decoder for speaker {}: {}", session_id, e);
                                        continue;
                                    }
                                };
                                entry.insert(SpeakerState {
                                    decoder,
                                    pending_packets: VecDeque::new(),
                                    last_packet_at: Instant::now(),
                                    dropped_packets: 0,
                                })
                            }
                        };

                        if state.pending_packets.len() >= max_packets_per_speaker {
                            state.pending_packets.pop_front();
                            state.dropped_packets += 1;
                            if state.dropped_packets % DROP_LOG_EVERY_FRAMES == 0 {
                                warn!(
                                    "Speaker {}: dropped {} queued Opus packets in jitter queue",
                                    session_id,
                                    state.dropped_packets
                                );
                            }
                        }

                        state.pending_packets.push_back(opus_data);
                        state.last_packet_at = Instant::now();
                    }
                    maybe_sound = sound_rx.recv(), if !sound_closed => {
                        match maybe_sound {
                            Some(pcm) => {
                                trace!("Queued sound effect ({} samples)", pcm.len());
                                sound_queue.extend(pcm);
                            }
                            None => {
                                sound_closed = true;
                            }
                        }
                    }
                }
            }

            debug!("Mixed Opus decoder stopped");
        })
    }
}
