use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use pjsip_sys::*;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::audio::bridge::AudioBridge;
use crate::audio::sounds::{self, SoundEvent};
use crate::audio::tts::PocketTtsRuntime;
use crate::config::Config;
use crate::mumble::control::{MumbleClient, MumbleEvent, MumbleSender};
use crate::sip;
use crate::sip::callbacks;
use crate::sip::media_port;

/// Wrapper to make raw pjmedia_port pointer Send+Sync.
/// Safety: the pointer is only accessed from pjsip's media thread (via callbacks)
/// or when we explicitly destroy it during session teardown (under Mutex).
struct SendablePort(*mut pjmedia_port);
unsafe impl Send for SendablePort {}
unsafe impl Sync for SendablePort {}

/// A single active call session, bridging one SIP call to one Mumble connection.
struct CallSession {
    /// Raw pointer to the custom pjmedia_port (we own this memory).
    media_port: SendablePort,
    /// Task handles for cleanup.
    encoder_handle: JoinHandle<()>,
    decoder_handle: JoinHandle<()>,
    voice_forward_handle: JoinHandle<()>,
    mumble_event_handle: JoinHandle<()>,
    tts_announce_handle: JoinHandle<()>,
    tts_text_handle: JoinHandle<()>,
    /// Sender handle for sending control messages to Mumble.
    mumble_sender: MumbleSender,
    /// Notify the event handler of channel changes from DTMF navigation.
    channel_watch_tx: tokio::sync::watch::Sender<u32>,
    /// Per-call sound effect sender for immediate local feedback.
    sound_tx: mpsc::UnboundedSender<Vec<i16>>,
    /// Current Mumble channel ID.
    current_channel_id: u32,
    /// Highest channel ID on this server (from connect-time handshake).
    max_channel_id: u32,
}

/// Deferred cleanup data for conference bridge ports.
/// Raw pointers are safe to send because they're only accessed after
/// a delay that ensures pjsip's clock thread is done with them.
struct DeferredCleanup {
    port: *mut pjmedia_port,
    pool: Option<*mut pj_pool_t>,
}
unsafe impl Send for DeferredCleanup {}

impl DeferredCleanup {
    /// Run the deferred cleanup after a delay, on a dedicated thread.
    fn spawn_deferred(self) {
        std::thread::spawn(move || {
            self.run();
        });
    }

    fn run(self) {
        std::thread::sleep(std::time::Duration::from_millis(250));
        sip::ensure_pj_thread_registered();
        unsafe {
            media_port::destroy_custom_port(self.port);
            if let Some(pool) = self.pool {
                pj_pool_release(pool);
            }
        }
    }
}

/// Manages all active call sessions.
pub struct SessionManager {
    sessions: Arc<Mutex<HashMap<pjsua_call_id, CallSession>>>,
    tts_runtime: Option<Arc<PocketTtsRuntime>>,
    config: Config,
}

impl SessionManager {
    pub fn new(config: Config, tts_runtime: Option<Arc<PocketTtsRuntime>>) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            tts_runtime,
            config,
        }
    }

    /// Handle an incoming SIP call: connect to Mumble and set up audio pipeline.
    pub async fn on_incoming_call(
        &self,
        call_id: pjsua_call_id,
        mumble_server: Option<String>,
        caller_number: Option<String>,
    ) -> anyhow::Result<()> {
        // Check max concurrent calls
        {
            let sessions = self.sessions.lock().unwrap();
            if sessions.len() >= self.config.sip.max_concurrent_calls as usize {
                warn!(
                    "Max concurrent calls ({}) reached, rejecting call {}",
                    self.config.sip.max_concurrent_calls, call_id
                );
                // Register thread with pjlib before calling pjsua (may be any tokio worker)
                sip::ensure_pj_thread_registered();
                unsafe {
                    pjsua_call_hangup(call_id, 486, std::ptr::null(), std::ptr::null());
                }
                return Ok(());
            }
        }

        let mut mumble_config = self.config.mumble_config(mumble_server.as_deref());
        if let Some(ref number) = caller_number {
            mumble_config.username = number.clone();
        }
        let sample_rate = self.config.audio.sample_rate;
        let configured_frame_ms = self.config.audio.frame_duration_ms;
        if configured_frame_ms != crate::audio::bridge::FRAME_DURATION_MS {
            warn!(
                "audio.frame_duration_ms={}ms is not supported by bridge, forcing {}ms",
                configured_frame_ms,
                crate::audio::bridge::FRAME_DURATION_MS
            );
        }
        let samples_per_frame = sample_rate * crate::audio::bridge::FRAME_DURATION_MS / 1000;
        let jitter_frames = (self.config.audio.jitter_buffer_ms
            / crate::audio::bridge::FRAME_DURATION_MS)
            .max(2) as usize;
        // Scale the opus→decoder channel by max_concurrent_calls to accommodate
        // bursts from multiple simultaneous Mumble speakers on a busy server.
        let queue_capacity = jitter_frames * self.config.sip.max_concurrent_calls as usize;

        info!(
            "Setting up session for call {} -> Mumble {}:{}",
            call_id, mumble_config.host, mumble_config.port
        );

        // Connect to Mumble
        let mut mumble = match MumbleClient::connect(&mumble_config).await {
            Ok(client) => client,
            Err(e) => {
                error!("Failed to connect to Mumble for call {}: {}", call_id, e);
                // Re-register thread — we may have resumed on a different worker after await
                sip::ensure_pj_thread_registered();
                unsafe {
                    pjsua_call_hangup(call_id, 503, std::ptr::null(), std::ptr::null());
                }
                return Err(e);
            }
        };

        // Get a clonable sender handle for sending voice/control to Mumble
        let mumble_sender = mumble.sender();
        let initial_channel_id = mumble.channel_id();
        let max_channel_id = mumble.max_channel_id();
        let initial_channel_name = mumble.channel_names().get(&initial_channel_id).cloned();

        // Create audio ring buffers (buffer ~200ms of audio)
        let buffer_samples = (sample_rate as usize) / 5;
        let buffers = media_port::create_audio_buffers(buffer_samples);

        // Create the custom media port (uses the ring buffer producer/consumer halves
        // that face pjsip's media thread)
        let media_port = unsafe {
            media_port::create_custom_port(
                std::ptr::null_mut(), // null pool — we allocate on the heap ourselves
                sample_rate,
                samples_per_frame,
                buffers.sip_to_mumble_prod,
                buffers.mumble_to_sip_cons,
            )?
        };

        // Spawn encoder: reads PCM from SIP→Mumble ring buffer, encodes Opus
        let (opus_encoded_tx, mut opus_encoded_rx) =
            mpsc::channel::<(u64, Bytes, bool)>(queue_capacity);
        let opus_bitrate = self.config.audio.opus_bitrate;
        let encoder_handle = AudioBridge::spawn_encoder(
            buffers.sip_to_mumble_cons,
            opus_encoded_tx,
            sample_rate,
            opus_bitrate,
        );

        // Spawn voice forwarder: sends encoded Opus to Mumble
        let voice_sender = mumble_sender.clone();
        let voice_forward_handle = tokio::spawn(async move {
            while let Some((seq_num, opus_data, end_of_transmission)) = opus_encoded_rx.recv().await
            {
                if let Err(e) = voice_sender.send_voice(seq_num, opus_data, end_of_transmission) {
                    debug!("Failed to send voice to Mumble: {}", e);
                    break;
                }
            }
        });

        // Channel to feed decoded Opus into the decoder
        let (opus_to_decoder_tx, opus_to_decoder_rx) =
            mpsc::channel::<(u32, Bytes)>(queue_capacity);

        // Sound effect channel: sends pre-decoded PCM to the decoder for mixing
        let (sound_tx, sound_rx) = mpsc::unbounded_channel::<Vec<i16>>();
        let (announcement_tx, mut announcement_rx) =
            mpsc::unbounded_channel::<(u32, Option<String>)>();
        let (text_tts_tx, mut text_tts_rx) = mpsc::unbounded_channel::<(Option<String>, String)>();

        // Spawn mixed decoder: per-speaker decode + 10ms mixdown into Mumble→SIP ring buffer.
        let decoder_handle = AudioBridge::spawn_mixed_decoder(
            opus_to_decoder_rx,
            sound_rx,
            buffers.mumble_to_sip_prod,
            sample_rate,
            jitter_frames,
        );

        let tts_enabled = self.config.tts.enabled;
        let announce_on_connect = self.config.tts.announce_on_connect;
        let announce_debounce = if tts_enabled {
            Duration::from_millis(self.config.tts.announcement_debounce_ms)
        } else {
            Duration::from_millis(0)
        };
        let tts_runtime = self.tts_runtime.clone();
        let tts_sound_tx = sound_tx.clone();
        let tts_announce_handle = tokio::spawn(async move {
            let mut last_announced_channel: Option<u32> = None;

            while let Some(first_announcement) = announcement_rx.recv().await {
                // Debounce rapid channel moves so we only announce the final room.
                let mut latest = first_announcement;
                let mut rx_closed = false;
                if !announce_debounce.is_zero() {
                    loop {
                        match tokio::time::timeout(announce_debounce, announcement_rx.recv()).await
                        {
                            // New channel-change arrived before timeout: update and
                            // restart debounce window from now.
                            Ok(Some(next)) => {
                                latest = next;
                            }
                            // Announcement channel closed: finish current pending item
                            // and then exit worker.
                            Ok(None) => {
                                rx_closed = true;
                                break;
                            }
                            // Quiet period reached: proceed with latest announcement.
                            Err(_) => break,
                        }
                    }
                }

                let (channel_id, channel_name) = latest;
                if last_announced_channel == Some(channel_id) {
                    continue;
                }
                last_announced_channel = Some(channel_id);

                if tts_enabled {
                    if let Some(runtime) = tts_runtime.as_ref() {
                        match runtime
                            .synth_channel_announcement(channel_name.as_deref(), channel_id)
                            .await
                        {
                            Ok(pcm) => {
                                let _ = tts_sound_tx.send(pcm);
                                continue;
                            }
                            Err(err) => {
                                warn!(
                                    "Failed to synthesize channel announcement for channel {}: {}",
                                    channel_id, err
                                );
                            }
                        }
                    } else {
                        warn!("TTS is enabled but runtime is unavailable");
                    }
                }

                if rx_closed {
                    break;
                }
            }
        });
        let text_tts_runtime = self.tts_runtime.clone();
        let text_tts_sound_tx = sound_tx.clone();
        let tts_text_handle = tokio::spawn(async move {
            while let Some((sender_name, message)) = text_tts_rx.recv().await {
                let phrase =
                    crate::audio::tts::text_message_phrase(sender_name.as_deref(), &message);
                if let Some(runtime) = text_tts_runtime.as_ref() {
                    match runtime.synthesize_phrase(&phrase).await {
                        Ok(pcm) => {
                            let _ = text_tts_sound_tx.send(pcm);
                            continue;
                        }
                        Err(err) => {
                            warn!(
                                "Failed to synthesize text message phrase '{}': {}",
                                phrase, err
                            );
                        }
                    }
                } else {
                    warn!("TTS is enabled but runtime is unavailable for text messages");
                }
                let _ = text_tts_sound_tx.send(sounds::get_sound(SoundEvent::TextMessage));
            }
        });

        // Spawn Mumble event handler: receives audio/state events and feeds them to the decoder
        let opus_tx = opus_to_decoder_tx.clone();
        let event_sound_tx = sound_tx.clone();
        let announcement_tx_events = announcement_tx.clone();
        let text_tts_tx_events = text_tts_tx.clone();
        let our_session_id = mumble.session_id();
        let mut channel_names = mumble.channel_names();
        let (channel_watch_tx, mut channel_watch_rx) =
            tokio::sync::watch::channel(initial_channel_id);
        let mumble_event_handle = tokio::spawn(async move {
            let mut dropped_decoder_frames: u64 = 0;
            // Track which channel each user is in so we can detect join/leave.
            let mut user_channels: HashMap<u32, u32> = HashMap::new();
            let mut our_channel_id = initial_channel_id;

            while let Some(event) = mumble.recv_event().await {
                // Check for channel updates from DTMF navigation
                if channel_watch_rx.has_changed().unwrap_or(false) {
                    our_channel_id = *channel_watch_rx.borrow_and_update();
                }

                match event {
                    MumbleEvent::AudioReceived {
                        session_id,
                        opus_data,
                        ..
                    } => {
                        match opus_tx.try_send((session_id, opus_data)) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => {
                                dropped_decoder_frames += 1;
                                if dropped_decoder_frames % 100 == 0 {
                                    warn!(
                                        "Call {}: dropped {} Mumble Opus frames before mixer due to full queue",
                                        call_id, dropped_decoder_frames
                                    );
                                }
                            }
                            Err(TrySendError::Closed(_)) => break,
                        }
                    }
                    MumbleEvent::UserChangedChannel {
                        session_id,
                        channel_id,
                    } => {
                        let old_channel = user_channels.insert(session_id, channel_id);

                        if session_id == our_session_id {
                            // We changed channel (server confirmed).
                            // Use the previously confirmed self-channel from user_channels
                            // so DTMF watch updates do not suppress announcements.
                            if tts_enabled && old_channel != Some(channel_id) {
                                let channel_name = channel_names.get(&channel_id).cloned();
                                let _ = announcement_tx_events.send((channel_id, channel_name));
                            }
                            our_channel_id = channel_id;
                        } else if channel_id == our_channel_id {
                            // Someone joined our channel
                            let _ = event_sound_tx
                                .send(sounds::get_sound(SoundEvent::UserJoinedChannel));
                        } else if old_channel == Some(our_channel_id) {
                            // Someone left our channel
                            let _ =
                                event_sound_tx.send(sounds::get_sound(SoundEvent::UserLeftChannel));
                        }
                    }
                    MumbleEvent::UserDisconnected { session_id } => {
                        if let Some(ch) = user_channels.remove(&session_id)
                            && ch == our_channel_id
                        {
                            let _ =
                                event_sound_tx.send(sounds::get_sound(SoundEvent::UserLeftChannel));
                        }
                    }
                    MumbleEvent::TextMessageReceived {
                        sender_name,
                        message,
                    } => {
                        if tts_enabled {
                            if text_tts_tx_events.send((sender_name, message)).is_err() {
                                let _ =
                                    event_sound_tx.send(sounds::get_sound(SoundEvent::TextMessage));
                            }
                        } else {
                            let _ = event_sound_tx.send(sounds::get_sound(SoundEvent::TextMessage));
                        }
                    }
                    MumbleEvent::ChannelState { channel_id, name } => {
                        channel_names.insert(channel_id, name);
                    }
                    MumbleEvent::Disconnected => {
                        info!("Mumble disconnected for call event loop");
                        break;
                    }
                    MumbleEvent::Connected { .. } => {}
                }
            }
        });

        // Register the media port so the pjsip callback thread can attach it
        // to the conference bridge when media becomes active
        callbacks::register_pending_port(call_id, media_port);

        // Answer the SIP call — re-register thread since we may have crossed an await boundary
        sip::ensure_pj_thread_registered();
        unsafe {
            let status = pjsua_call_answer(call_id, 200, std::ptr::null(), std::ptr::null());
            if status != 0 {
                error!("Failed to answer call {}: status={}", call_id, status);
            }
        }

        // Initial connect announcement behavior.
        if tts_enabled && announce_on_connect {
            let _ = announcement_tx.send((initial_channel_id, initial_channel_name));
        } else {
            let _ = sound_tx.send(sounds::get_sound(SoundEvent::SelfJoinedChannel));
        }

        // Store the session
        let session = CallSession {
            media_port: SendablePort(media_port),
            encoder_handle,
            decoder_handle,
            voice_forward_handle,
            mumble_event_handle,
            tts_announce_handle,
            tts_text_handle,
            mumble_sender,
            channel_watch_tx,
            sound_tx: sound_tx.clone(),
            current_channel_id: initial_channel_id,
            max_channel_id,
        };

        self.sessions.lock().unwrap().insert(call_id, session);
        info!("Session created for call {}", call_id);

        Ok(())
    }

    /// Handle a DTMF digit: `*` = previous channel, `#` = next channel.
    pub fn on_dtmf_digit(&self, call_id: pjsua_call_id, digit: char) {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(session) = sessions.get_mut(&call_id) else {
            return;
        };

        let new_channel_id = match digit {
            '*' => session.current_channel_id.saturating_sub(1),
            '#' => (session.current_channel_id + 1).min(session.max_channel_id),
            _ => return,
        };

        info!(
            "Call {}: DTMF '{}' → navigating from channel {} to {}",
            call_id, digit, session.current_channel_id, new_channel_id
        );

        // Always provide immediate audible feedback for DTMF navigation.
        let _ = session
            .sound_tx
            .send(sounds::get_sound(SoundEvent::SelfJoinedChannel));

        if let Err(e) = session.mumble_sender.join_channel(new_channel_id) {
            warn!("Failed to join channel {}: {}", new_channel_id, e);
        } else {
            session.current_channel_id = new_channel_id;
            // Notify the event handler of the new channel so it can correctly
            // attribute join/leave events.
            let _ = session.channel_watch_tx.send(new_channel_id);
        }
    }

    /// Handle call disconnection — tear down the session.
    pub fn on_call_disconnected(&self, call_id: pjsua_call_id) {
        sip::ensure_pj_thread_registered();
        let session = self.sessions.lock().unwrap().remove(&call_id);
        let Some(session) = session else {
            return;
        };

        info!("Tearing down session for call {}", call_id);

        // Resolve media ownership state before cleanup:
        // - pending: port was never attached to conference
        // - active: port is attached and must be removed asynchronously
        let pending_port = callbacks::unregister_pending_port(call_id);
        let active_media = callbacks::take_active_media(call_id);

        // Abort async tasks first — this drops the ring buffer halves owned
        // by tokio tasks, preventing further reads/writes.
        session.encoder_handle.abort();
        session.decoder_handle.abort();
        session.voice_forward_handle.abort();
        session.mumble_event_handle.abort();
        session.tts_announce_handle.abort();
        session.tts_text_handle.abort();

        if pending_port.is_some() {
            // Port was never attached to conference bridge — clean up directly.
            unsafe {
                media_port::destroy_custom_port(session.media_port.0);
            }
            return;
        }

        if let Some(active_media) = active_media {
            // Register cleanup to run exactly when REMOVE_PORT completes.
            callbacks::register_pending_cleanup(
                active_media.conf_port_id,
                session.media_port.0,
                Some(active_media.pool),
            );

            // Remove from conference bridge (asynchronous).
            let status = unsafe { pjsua_conf_remove_port(active_media.conf_port_id) };
            if status != 0 {
                warn!(
                    "Failed to remove conference port {} for call {} (status={}); using deferred fallback cleanup",
                    active_media.conf_port_id, call_id, status
                );
                if let Some((port, pool)) =
                    callbacks::take_pending_cleanup(active_media.conf_port_id)
                {
                    DeferredCleanup { port, pool }.spawn_deferred();
                }
            }
        } else {
            // Unknown state: callback consumed pending registration, but we didn't
            // observe an active conference slot. Avoid immediate destroy to prevent
            // use-after-free in pjmedia clock thread.
            warn!(
                "Call {} has no pending or active media state during teardown; skipping immediate media-port destroy",
                call_id
            );
        }
    }
}
