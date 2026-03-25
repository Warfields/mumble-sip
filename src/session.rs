use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use pjsip_sys::*;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::audio::bridge::AudioBridge;
use crate::audio::sounds::{self, SoundEvent};
use crate::audio::tts::PocketTtsRuntime;
use crate::config::Config;
use crate::db::{self, CallerStore};
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
    /// Caller's phone number (for persisting last channel on disconnect).
    caller_number: Option<String>,
    /// Effective Mumble server host (for per-server last-channel lookup).
    server_host: String,
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
    caller_store: Arc<dyn CallerStore>,
    config: Config,
    /// Pending DB writes spawned during disconnect, so shutdown can await them.
    pending_writes: Arc<Mutex<Vec<JoinHandle<()>>>>,
    /// Call IDs currently in setup (between answer and session insertion).
    /// Each entry is generation-scoped: the setup task owns its entry and is
    /// the only one that removes it (after cleanup).  `cancel_pending_setup`
    /// cancels the token but leaves the entry so the generation check can
    /// prevent a stale task from clobbering a recycled call_id's state.
    pending_setups: Arc<Mutex<HashMap<pjsua_call_id, (u64, CancellationToken)>>>,
    /// Monotonic generation counter — each setup attempt gets a unique value.
    next_setup_generation: AtomicU64,
}

impl SessionManager {
    pub fn new(
        config: Config,
        tts_runtime: Option<Arc<PocketTtsRuntime>>,
        caller_store: Arc<dyn CallerStore>,
    ) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            tts_runtime,
            pending_writes: Arc::new(Mutex::new(Vec::new())),
            pending_setups: Arc::new(Mutex::new(HashMap::new())),
            next_setup_generation: AtomicU64::new(0),
            caller_store,
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
        // Check max concurrent calls (count both fully-established sessions
        // and calls still in setup, e.g. playing intro / connecting to Mumble).
        {
            let sessions = self.sessions.lock().unwrap();
            let pending = self.pending_setups.lock().unwrap();
            let total = sessions.len() + pending.len();
            if total >= self.config.sip.max_concurrent_calls as usize {
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

        // Always set Mumble username to a nickname — never leak phone numbers.
        let mut last_channel_id = None;
        let mut should_play_intro = false;
        const SECS_PER_DAY: u64 = 86_400;
        let intro_replay_secs = self.config.audio.intro_replay_after_days * SECS_PER_DAY;
        mumble_config.username = match caller_number.as_deref() {
            Some(number) => match self.caller_store.get_or_create_caller(number).await {
                Ok(info) => {
                    last_channel_id = self
                        .caller_store
                        .get_last_channel_id(number, &mumble_config.host)
                        .await
                        .unwrap_or_else(|e| {
                            warn!("Failed to look up last channel for {number}: {e}");
                            None
                        });
                    // Play intro for brand-new callers or callers who haven't
                    // called in longer than the configured replay period.
                    let now = db::now_epoch();
                    should_play_intro =
                        info.is_new || (now.saturating_sub(info.last_seen) >= intro_replay_secs);
                    info.nickname
                }
                Err(e) => {
                    warn!("Caller store lookup failed for {number}: {e}");
                    db::generate_nickname()
                }
            },
            None => db::generate_nickname(),
        };
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

        // Create audio ring buffers (buffer ~200ms of audio)
        let buffer_samples = (sample_rate as usize) / 5;
        let buffers = media_port::create_audio_buffers(buffer_samples);

        // Create the custom media port (uses the ring buffer producer/consumer halves
        // that face pjsip's media thread).  Wrapped in SendablePort immediately so the
        // raw pointer is Send-safe across awaits (e.g. the intro sleep).
        let media_port = SendablePort(unsafe {
            media_port::create_custom_port(
                std::ptr::null_mut(), // null pool — we allocate on the heap ourselves
                sample_rate,
                samples_per_frame,
                buffers.sip_to_mumble_prod,
                buffers.mumble_to_sip_cons,
            )?
        });

        // Channel to feed decoded Opus into the decoder
        let (opus_to_decoder_tx, opus_to_decoder_rx) =
            mpsc::channel::<(u32, Bytes)>(queue_capacity);

        // Sound effect channel: sends pre-decoded PCM to the decoder for mixing
        let (sound_tx, sound_rx) = mpsc::unbounded_channel::<Vec<i16>>();

        // Spawn mixed decoder: per-speaker decode + 10ms mixdown into Mumble→SIP ring buffer.
        let decoder_handle = AudioBridge::spawn_mixed_decoder(
            opus_to_decoder_rx,
            sound_rx,
            buffers.mumble_to_sip_prod,
            sample_rate,
            jitter_frames,
        );

        // Register the media port so the pjsip callback thread can attach it
        // to the conference bridge when media becomes active
        callbacks::register_pending_port(call_id, media_port.0);

        // Register a one-shot notifier so we know when the audio path is live
        // (i.e. on_call_media_state has attached the port to the conference).
        let media_ready_rx = callbacks::register_media_ready(call_id);

        // Register a cancellation token so on_call_disconnected can signal us
        // if the caller hangs up at any point before we insert the session.
        // This covers the media-ready wait, intro sleep, Mumble connect, and
        // any other await points between answering and session creation.
        //
        // The generation scopes this entry to *this* setup attempt.  If pjsua
        // recycles the call_id before our cleanup finishes, the new call gets
        // a different generation and our cleanup will not clobber its state.
        let cancel_token = CancellationToken::new();
        let generation = self.next_setup_generation.fetch_add(1, Ordering::Relaxed);
        self.pending_setups
            .lock()
            .unwrap()
            .insert(call_id, (generation, cancel_token.clone()));

        // Answer the SIP call — re-register thread since we may have crossed an await boundary
        sip::ensure_pj_thread_registered();
        let answer_failed = unsafe {
            let status = pjsua_call_answer(call_id, 200, std::ptr::null(), std::ptr::null());
            if status != 0 {
                error!("Failed to answer call {}: status={}", call_id, status);
                true
            } else {
                false
            }
        };

        if answer_failed {
            // Call vanished (e.g. duplicate INVITE already rejected by pjsip).
            // Clean up everything we just set up.
            let owns = self.abort_setup(call_id, generation, &decoder_handle, media_port.0);
            if owns {
                callbacks::unregister_media_ready(call_id);
            }
            return Ok(());
        }

        // Wait for the audio path to become active before sending any sound
        // effects.  Without this, the decoder mixes into the ring buffer
        // before pjsip's get_frame callback starts draining it, and the
        // ~200ms buffer overflows — truncating the intro.
        tokio::select! {
            result = media_ready_rx => {
                if result.is_err() {
                    // Sender dropped (e.g. pjsip shut down) — abort setup.
                    warn!("Media ready channel dropped for call {}, aborting setup", call_id);
                    self.abort_setup(call_id, generation, &decoder_handle, media_port.0);
                    return Ok(());
                }
            }
            _ = cancel_token.cancelled() => {
                info!("Call {} disconnected while waiting for media, aborting setup", call_id);
                let owns = self.abort_setup(call_id, generation, &decoder_handle, media_port.0);
                if owns {
                    callbacks::unregister_media_ready(call_id);
                }
                return Ok(());
            }
        }

        // Play the intro for first-time callers or those who haven't called recently.
        // This plays *after* the media bridge is attached so pjsip's get_frame
        // callback is actively draining the ring buffer — no samples are lost.
        // It plays *before* connecting to Mumble so the caller hears it in
        // isolation, without Mumble chatter competing for their attention, and
        // so Mumble users don't see the caller until the intro finishes.
        if should_play_intro {
            info!("Playing intro for caller (new or returning after absence)");
            let intro_duration = sounds::sound_duration(SoundEvent::Intro, sample_rate);
            let _ = sound_tx.send(sounds::get_sound(SoundEvent::Intro));

            tokio::select! {
                _ = tokio::time::sleep(intro_duration) => {}
                _ = cancel_token.cancelled() => {
                    info!("Call {} disconnected during intro playback, aborting setup", call_id);
                    self.abort_setup(call_id, generation, &decoder_handle, media_port.0);
                    return Ok(());
                }
            }
        }

        // Connect to Mumble.  Wrap in select! so cancellation is detected
        // promptly instead of waiting for the TCP/TLS handshake to finish.
        let mumble_result = tokio::select! {
            result = MumbleClient::connect(&mumble_config) => result,
            _ = cancel_token.cancelled() => {
                info!("Call {} disconnected during Mumble connect, aborting setup", call_id);
                self.abort_setup(call_id, generation, &decoder_handle, media_port.0);
                return Ok(());
            }
        };

        let mut mumble = match mumble_result {
            Ok(client) => client,
            Err(e) => {
                error!("Failed to connect to Mumble for call {}: {}", call_id, e);
                let owns = self.abort_setup(call_id, generation, &decoder_handle, media_port.0);
                if owns {
                    // abort_setup already called ensure_pj_thread_registered
                    unsafe {
                        pjsua_call_hangup(call_id, 503, std::ptr::null(), std::ptr::null());
                    }
                }
                return Err(e);
            }
        };

        // Get a clonable sender handle for sending voice/control to Mumble
        let mumble_sender = mumble.sender();
        let mut initial_channel_id = mumble.channel_id();
        let max_channel_id = mumble.max_channel_id();

        // Rejoin the caller's last channel on this server (if it still exists).
        if let Some(ch) = last_channel_id
            && ch <= max_channel_id
            && ch != initial_channel_id
        {
            if let Err(e) = mumble_sender.join_channel(ch) {
                warn!("Failed to rejoin last channel {ch}: {e}");
            } else {
                info!("Rejoining caller's last channel {ch}");
                initial_channel_id = ch;
            }
        }

        let initial_channel_name = mumble.channel_names().get(&initial_channel_id).cloned();

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

        let (announcement_tx, mut announcement_rx) =
            mpsc::unbounded_channel::<(u32, Option<String>)>();
        let (text_tts_tx, mut text_tts_rx) = mpsc::unbounded_channel::<(Option<String>, String)>();

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
                    } => match opus_tx.try_send((session_id, opus_data)) {
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
                    },
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

        // Initial connect announcement behavior.
        if tts_enabled && announce_on_connect {
            let _ = announcement_tx.send((initial_channel_id, initial_channel_name));
        } else {
            let _ = sound_tx.send(sounds::get_sound(SoundEvent::SelfJoinedChannel));
        }

        // Atomically transition from pending → active.  If the cancel token
        // was signalled between the last select! and here (i.e. on_call_disconnected
        // ran while we were spawning tasks), we must tear down instead of inserting
        // an orphaned session that no future disconnect event will clean up.
        let owns = self.remove_pending_setup(call_id, generation);

        if cancel_token.is_cancelled() {
            warn!(
                "Call {} disconnected after Mumble connect but before session insertion; tearing down",
                call_id
            );
            encoder_handle.abort();
            decoder_handle.abort();
            voice_forward_handle.abort();
            mumble_event_handle.abort();
            tts_announce_handle.abort();
            tts_text_handle.abort();

            if !owns {
                // Another setup has recycled this call_id — we must not touch
                // call_id-keyed global state.  Defer media-port destruction
                // since pjsip's clock thread may still reference it.
                warn!(
                    "Call {} recycled during late-cancel teardown; deferring media-port destroy",
                    call_id
                );
                DeferredCleanup {
                    port: media_port.0,
                    pool: None,
                }
                .spawn_deferred();
                return Ok(());
            }

            let active_media = callbacks::take_active_media(call_id);
            if let Some(active_media) = active_media {
                callbacks::register_pending_cleanup(
                    active_media.conf_port_id,
                    media_port.0,
                    Some(active_media.pool),
                );
                sip::ensure_pj_thread_registered();
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
                warn!(
                    "Call {} has no active media state during late-cancel teardown; skipping immediate media-port destroy",
                    call_id
                );
            }
            return Ok(());
        }

        // Store the session
        let session = CallSession {
            media_port,
            encoder_handle,
            decoder_handle,
            voice_forward_handle,
            mumble_event_handle,
            tts_announce_handle,
            tts_text_handle,
            mumble_sender,
            channel_watch_tx,
            sound_tx,
            current_channel_id: initial_channel_id,
            max_channel_id,
            caller_number: caller_number.clone(),
            server_host: mumble_config.host.clone(),
        };

        self.sessions.lock().unwrap().insert(call_id, session);
        info!("Session created for call {}", call_id);

        Ok(())
    }

    /// Common cleanup for aborting a call that is still in setup.
    /// Removes the pending-setup entry, aborts the decoder, re-registers the
    /// pjsip thread, and cleans up the media port.  Returns `true` when this
    /// generation still owns the `call_id` (callers may need this for
    /// stage-specific cleanup like `unregister_media_ready` or `call_hangup`).
    fn abort_setup(
        &self,
        call_id: pjsua_call_id,
        generation: u64,
        decoder_handle: &JoinHandle<()>,
        port: *mut pjmedia_port,
    ) -> bool {
        let owns = self.remove_pending_setup(call_id, generation);
        decoder_handle.abort();
        sip::ensure_pj_thread_registered();
        Self::cleanup_media_port(call_id, port, owns);
        owns
    }

    /// Check whether this setup generation still owns the `call_id` in
    /// `pending_setups` (i.e. no new call has recycled it).
    fn owns_call_id(&self, call_id: pjsua_call_id, generation: u64) -> bool {
        let map = self.pending_setups.lock().unwrap();
        matches!(map.get(&call_id), Some(entry) if entry.0 == generation)
    }

    /// Remove the `pending_setups` entry only if the generation matches.
    /// Returns `true` if the entry was removed (i.e. this generation owned it).
    fn remove_pending_setup(&self, call_id: pjsua_call_id, generation: u64) -> bool {
        let mut map = self.pending_setups.lock().unwrap();
        if matches!(map.get(&call_id), Some(entry) if entry.0 == generation) {
            map.remove(&call_id);
            true
        } else {
            false
        }
    }

    /// Clean up a media port that may or may not have been attached to the
    /// conference bridge.  Only touches call_id-keyed global state
    /// (`PENDING_PORTS`, `ACTIVE_MEDIA`) when `owns_call` is true (the
    /// caller still owns the call_id in `pending_setups`).  When a new
    /// call has recycled the call_id, only the port pointer is cleaned up
    /// via deferred destruction.
    ///
    /// Caller must have already called `sip::ensure_pj_thread_registered()`.
    fn cleanup_media_port(call_id: pjsua_call_id, port: *mut pjmedia_port, owns_call: bool) {
        if !owns_call {
            // Another setup now owns this call_id — we must not touch any
            // call_id-keyed global state.  Defer port destruction to be safe
            // in case pjsip's clock thread is still referencing it.
            warn!(
                "Call {} recycled during cleanup; deferring media-port destroy",
                call_id
            );
            DeferredCleanup { port, pool: None }.spawn_deferred();
            return;
        }

        let was_pending = callbacks::unregister_pending_port(call_id).is_some();
        let active_media = callbacks::take_active_media(call_id);

        if was_pending {
            // Port was never attached to the conference bridge — safe to
            // destroy immediately.
            unsafe {
                media_port::destroy_custom_port(port);
            }
        } else if let Some(active) = active_media {
            // Port is on the conference bridge — remove asynchronously.
            callbacks::register_pending_cleanup(active.conf_port_id, port, Some(active.pool));
            let status = unsafe { pjsua_conf_remove_port(active.conf_port_id) };
            if status != 0 {
                if let Some((port, pool)) = callbacks::take_pending_cleanup(active.conf_port_id) {
                    DeferredCleanup { port, pool }.spawn_deferred();
                }
            }
        } else {
            // Indeterminate: on_call_media_state consumed the pending entry
            // but hasn't published active media yet — the callback thread may
            // still be attaching the port to the conference bridge.  Defer
            // destruction to avoid use-after-free.
            warn!(
                "Call {} media port in indeterminate state during cleanup; deferring destroy",
                call_id
            );
            DeferredCleanup { port, pool: None }.spawn_deferred();
        }
    }

    /// Number of currently active call sessions tracked by this manager.
    pub fn active_call_count(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }

    /// Await all pending DB writes spawned during disconnect.
    /// Call this before shutting down the runtime to avoid losing data.
    pub async fn drain_pending_writes(&self) {
        let handles: Vec<_> = self.pending_writes.lock().unwrap().drain(..).collect();
        for handle in handles {
            let _ = handle.await;
        }
    }

    /// Drop completed DB-write join handles so the manager only retains
    /// in-flight work during normal runtime.
    fn reap_completed_writes(&self) {
        self.pending_writes
            .lock()
            .unwrap()
            .retain(|handle| !handle.is_finished());
    }

    /// Handle a DTMF digit: `1` = intro, `*` = previous channel, `#` = next channel.
    pub fn on_dtmf_digit(&self, call_id: pjsua_call_id, digit: char) {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(session) = sessions.get_mut(&call_id) else {
            return;
        };

        match digit {
            '1' => {
                info!("Call {}: DTMF '1' → playing intro", call_id);
                let _ = session.sound_tx.send(sounds::get_sound(SoundEvent::Intro));
            }
            '*' | '#' => {
                let new_channel_id = if digit == '*' {
                    session.current_channel_id.saturating_sub(1)
                } else {
                    (session.current_channel_id + 1).min(session.max_channel_id)
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
            _ => return,
        }
    }

    /// If the call has no active session yet (still in setup), cancel the
    /// pending setup so it cleans up instead of creating an orphaned session.
    /// The entry is **not** removed — the setup task owns it and will remove
    /// it after cleanup, using its generation to avoid clobbering a recycled
    /// call_id.  Returns `true` if a pending setup was found and cancelled.
    fn cancel_pending_setup(&self, call_id: pjsua_call_id) -> bool {
        let map = self.pending_setups.lock().unwrap();
        if let Some(entry) = map.get(&call_id) {
            info!(
                "Call {} disconnected during setup, signalling cancellation",
                call_id
            );
            entry.1.cancel();
            true
        } else {
            false
        }
    }

    /// Handle call disconnection — tear down the session.
    pub fn on_call_disconnected(&self, call_id: pjsua_call_id) {
        sip::ensure_pj_thread_registered();
        let session = self.sessions.lock().unwrap().remove(&call_id);
        let Some(session) = session else {
            // Call may still be in setup (e.g. intro playback). Cancel the
            // setup task so it cleans up instead of creating an orphaned session.
            self.cancel_pending_setup(call_id);
            return;
        };

        info!("Tearing down session for call {}", call_id);
        self.reap_completed_writes();

        // Persist the caller's last channel for next reconnect.
        if let Some(ref phone) = session.caller_number {
            let store = self.caller_store.clone();
            let phone = phone.clone();
            let host = session.server_host.clone();
            let channel_id = session.current_channel_id;
            let handle = tokio::spawn(async move {
                if let Err(e) = store.set_last_channel_id(&phone, &host, channel_id).await {
                    warn!("Failed to persist last channel for {phone}: {e}");
                }
            });
            self.pending_writes.lock().unwrap().push(handle);
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::{CallerInfo, CallerStore};
    use async_trait::async_trait;
    use std::time::Duration;

    /// Minimal CallerStore that does nothing — tests here don't exercise DB paths.
    struct StubCallerStore;

    #[async_trait]
    impl CallerStore for StubCallerStore {
        async fn get_or_create_caller(&self, phone_number: &str) -> anyhow::Result<CallerInfo> {
            Ok(CallerInfo {
                phone_number: phone_number.to_string(),
                nickname: "test_caller".to_string(),
                last_seen: 0,
                is_new: true,
            })
        }
        async fn set_nickname(&self, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn set_last_channel_id(&self, _: &str, _: &str, _: u32) -> anyhow::Result<()> {
            Ok(())
        }
        async fn get_last_channel_id(&self, _: &str, _: &str) -> anyhow::Result<Option<u32>> {
            Ok(None)
        }
    }

    fn test_config() -> Config {
        test_config_with_max_calls(10)
    }

    fn test_config_with_max_calls(max: u32) -> Config {
        toml::from_str(&format!(
            r#"
[sip]
listen_port = 5060
account_uri = "sip:test@localhost"
max_concurrent_calls = {max}

[mumble]
default_host = "localhost"
"#,
        ))
        .expect("test config should parse")
    }

    fn test_session_manager() -> SessionManager {
        SessionManager::new(test_config(), None, Arc::new(StubCallerStore))
    }

    fn test_session_manager_with_max_calls(max: u32) -> SessionManager {
        SessionManager::new(
            test_config_with_max_calls(max),
            None,
            Arc::new(StubCallerStore),
        )
    }

    #[test]
    fn cancel_pending_setup_cancels_token_but_preserves_entry() {
        let mgr = test_session_manager();
        let token = CancellationToken::new();

        mgr.pending_setups
            .lock()
            .unwrap()
            .insert(42, (0, token.clone()));
        assert!(!token.is_cancelled());

        let found = mgr.cancel_pending_setup(42);
        assert!(found);
        assert!(token.is_cancelled());
        // Entry is preserved — the setup task owns removal.
        assert_eq!(mgr.pending_setups.lock().unwrap().len(), 1);
    }

    #[test]
    fn cancel_pending_setup_returns_false_for_unknown_call() {
        let mgr = test_session_manager();
        assert!(!mgr.cancel_pending_setup(999));
    }

    #[test]
    fn remove_pending_setup_checks_generation() {
        let mgr = test_session_manager();
        let token = CancellationToken::new();

        mgr.pending_setups
            .lock()
            .unwrap()
            .insert(42, (5, token.clone()));

        // Wrong generation — should not remove.
        assert!(!mgr.remove_pending_setup(42, 99));
        assert_eq!(mgr.pending_setups.lock().unwrap().len(), 1);

        // Correct generation — should remove.
        assert!(mgr.remove_pending_setup(42, 5));
        assert!(mgr.pending_setups.lock().unwrap().is_empty());
    }

    #[test]
    fn owns_call_id_checks_generation() {
        let mgr = test_session_manager();
        let token = CancellationToken::new();

        mgr.pending_setups.lock().unwrap().insert(42, (7, token));

        assert!(mgr.owns_call_id(42, 7));
        assert!(!mgr.owns_call_id(42, 99));
        assert!(!mgr.owns_call_id(999, 7));
    }

    #[tokio::test]
    async fn disconnect_during_intro_aborts_setup() {
        // Simulates the select! pattern used during intro playback: a long
        // sleep races against a cancellation token. Cancelling the token
        // causes the setup to abort before Mumble connect.
        let mgr = Arc::new(test_session_manager());
        let call_id: pjsua_call_id = 7;
        let generation: u64 = 0;

        let token = CancellationToken::new();
        mgr.pending_setups
            .lock()
            .unwrap()
            .insert(call_id, (generation, token.clone()));

        let mgr2 = mgr.clone();
        let setup_task = tokio::spawn(async move {
            // Simulate intro sleep with cancellation check (mirrors on_incoming_call).
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(60)) => {
                    panic!("should not reach post-intro setup");
                }
                _ = token.cancelled() => {
                    // Cleanup path — task removes its own entry.
                    mgr2.remove_pending_setup(call_id, generation);
                }
            }
        });

        // Simulate disconnect arriving during intro playback.
        tokio::time::sleep(Duration::from_millis(5)).await;
        let cancelled = mgr.cancel_pending_setup(call_id);
        assert!(cancelled, "should find and cancel the pending setup");

        setup_task.await.unwrap();

        assert!(
            mgr.sessions.lock().unwrap().is_empty(),
            "no session should exist after disconnect during intro"
        );
        assert!(mgr.pending_setups.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn disconnect_during_mumble_connect_aborts_via_select() {
        // Simulates a disconnect arriving while MumbleClient::connect is
        // in progress.  The select! on the connect future detects
        // cancellation promptly without waiting for the connect to finish.
        let mgr = Arc::new(test_session_manager());
        let call_id: pjsua_call_id = 8;
        let generation: u64 = 0;

        let token = CancellationToken::new();
        mgr.pending_setups
            .lock()
            .unwrap()
            .insert(call_id, (generation, token.clone()));

        let mgr2 = mgr.clone();
        let setup_task = tokio::spawn(async move {
            // Simulate Mumble connect wrapped in select! (mirrors on_incoming_call).
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(60)) => {
                    panic!("should not reach post-connect setup");
                }
                _ = token.cancelled() => {
                    // Cleanup — task removes its own entry.
                    mgr2.remove_pending_setup(call_id, generation);
                }
            }
        });

        // Disconnect arrives while "Mumble connect" is in progress.
        tokio::time::sleep(Duration::from_millis(5)).await;
        let cancelled = mgr.cancel_pending_setup(call_id);
        assert!(cancelled);

        setup_task.await.unwrap();

        assert!(
            mgr.sessions.lock().unwrap().is_empty(),
            "no session should exist after disconnect during Mumble connect"
        );
        assert!(mgr.pending_setups.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stale_task_does_not_clobber_recycled_call_id() {
        // Simulates the race: old task is cancelled mid-connect, pjsua
        // recycles the call_id for a new call before the old task cleans up.
        // The old task's generation check must prevent it from removing the
        // new call's pending_setups entry.
        let mgr = Arc::new(test_session_manager());
        let call_id: pjsua_call_id = 5;
        let old_gen: u64 = 0;
        let new_gen: u64 = 1;

        let old_token = CancellationToken::new();
        mgr.pending_setups
            .lock()
            .unwrap()
            .insert(call_id, (old_gen, old_token.clone()));

        let mgr2 = mgr.clone();
        let old_task = tokio::spawn(async move {
            // Simulate a long Mumble connect.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                _ = old_token.cancelled() => {}
            }

            // Old task wakes up and tries cleanup.
            // It should NOT remove the entry because generation changed.
            let owns = mgr2.owns_call_id(call_id, old_gen);
            assert!(!owns, "old task should not own the call_id anymore");
            mgr2.remove_pending_setup(call_id, old_gen);
        });

        // Disconnect the old call.
        tokio::time::sleep(Duration::from_millis(5)).await;
        mgr.cancel_pending_setup(call_id);

        // New call arrives with the same call_id before old task finishes.
        let new_token = CancellationToken::new();
        mgr.pending_setups
            .lock()
            .unwrap()
            .insert(call_id, (new_gen, new_token.clone()));

        old_task.await.unwrap();

        // The new call's entry must still be intact.
        assert!(
            mgr.owns_call_id(call_id, new_gen),
            "new call's pending_setups entry must survive old task cleanup"
        );
        assert!(
            !new_token.is_cancelled(),
            "new call's token must not be cancelled"
        );
    }

    #[tokio::test]
    async fn normal_setup_completes_when_no_disconnect() {
        // Verifies that the cancellation mechanism does not interfere with
        // normal call setup when no disconnect occurs.
        let mgr = Arc::new(test_session_manager());
        let call_id: pjsua_call_id = 10;
        let generation: u64 = 0;
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let token = CancellationToken::new();
        mgr.pending_setups
            .lock()
            .unwrap()
            .insert(call_id, (generation, token.clone()));

        let mgr2 = mgr.clone();
        let token2 = token.clone();
        let completed2 = completed.clone();
        let setup_task = tokio::spawn(async move {
            // Simulate intro + Mumble connect without disconnect.
            tokio::time::sleep(Duration::from_millis(10)).await;

            if token2.is_cancelled() {
                return;
            }

            // "Session insertion" — remove token and mark complete.
            mgr2.remove_pending_setup(call_id, generation);
            completed2.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        setup_task.await.unwrap();

        assert!(
            completed.load(std::sync::atomic::Ordering::SeqCst),
            "setup should complete normally when no disconnect occurs"
        );
        assert!(mgr.pending_setups.lock().unwrap().is_empty());
    }

    #[test]
    fn pending_setups_count_toward_max_concurrent_calls() {
        let mgr = test_session_manager_with_max_calls(1);

        // Simulate one call in setup (not yet a full session).
        let token = CancellationToken::new();
        mgr.pending_setups.lock().unwrap().insert(100, (0, token));

        // The concurrency gate should now count this pending call.
        let total = mgr.sessions.lock().unwrap().len() + mgr.pending_setups.lock().unwrap().len();
        assert!(
            total >= mgr.config.sip.max_concurrent_calls as usize,
            "pending setup should count toward the concurrency limit"
        );
    }
}
