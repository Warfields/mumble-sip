use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use pjsip_sys::*;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::audio::bridge::AudioBridge;
use crate::config::Config;
use crate::mumble::control::{MumbleClient, MumbleEvent};
use crate::sip;
use crate::sip::callbacks;
use crate::sip::media_port;

/// Wrapper to make raw pjmedia_port pointer Send.
/// Safety: the pointer is only accessed from pjsip's media thread (via callbacks)
/// or when we explicitly destroy it during session teardown (under Mutex).
struct SendablePort(*mut pjmedia_port);
unsafe impl Send for SendablePort {}

/// Wrapper to make raw pj_pool_t pointer Send.
struct SendablePool(*mut pj_pool_t);
unsafe impl Send for SendablePool {}

/// A single active call session, bridging one SIP call to one Mumble connection.
struct CallSession {
    /// Conference port ID for the custom media port.
    conf_port_id: Option<pjsua_conf_port_id>,
    /// Pool allocated for the conference port (must be released on teardown).
    conf_pool: Option<SendablePool>,
    /// Raw pointer to the custom pjmedia_port (we own this memory).
    media_port: SendablePort,
    /// Task handles for cleanup.
    encoder_handle: JoinHandle<()>,
    decoder_handle: JoinHandle<()>,
    voice_forward_handle: JoinHandle<()>,
    mumble_event_handle: JoinHandle<()>,
}

/// Manages all active call sessions.
pub struct SessionManager {
    sessions: Arc<Mutex<HashMap<pjsua_call_id, CallSession>>>,
    config: Config,
}

impl SessionManager {
    pub fn new(config: Config) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            config,
        }
    }

    /// Handle an incoming SIP call: connect to Mumble and set up audio pipeline.
    pub async fn on_incoming_call(
        &self,
        call_id: pjsua_call_id,
        mumble_server: Option<String>,
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

        let mumble_config = self.config.mumble_config(mumble_server.as_deref());
        let sample_rate = self.config.audio.sample_rate;
        let samples_per_frame = sample_rate * self.config.audio.frame_duration_ms / 1000;

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

        // Get a clonable sender handle for sending voice to Mumble
        let mumble_sender = mumble.sender();

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
        let (opus_encoded_tx, mut opus_encoded_rx) = mpsc::unbounded_channel::<(u64, Bytes)>();
        let encoder_handle =
            AudioBridge::spawn_encoder(buffers.sip_to_mumble_cons, opus_encoded_tx, sample_rate);

        // Spawn voice forwarder: sends encoded Opus to Mumble
        let voice_forward_handle = tokio::spawn(async move {
            while let Some((seq_num, opus_data)) = opus_encoded_rx.recv().await {
                if let Err(e) = mumble_sender.send_voice(seq_num, opus_data, false) {
                    debug!("Failed to send voice to Mumble: {}", e);
                    break;
                }
            }
        });

        // Channel to feed decoded Opus into the decoder
        let (opus_to_decoder_tx, opus_to_decoder_rx) = mpsc::unbounded_channel::<Bytes>();

        // Spawn decoder: decodes Opus from Mumble, writes PCM into Mumble→SIP ring buffer
        let decoder_handle =
            AudioBridge::spawn_decoder(opus_to_decoder_rx, buffers.mumble_to_sip_prod, sample_rate);

        // Spawn Mumble event handler: receives audio events and feeds them to the decoder
        let opus_tx = opus_to_decoder_tx.clone();
        let mumble_event_handle = tokio::spawn(async move {
            while let Some(event) = mumble.recv_event().await {
                match event {
                    MumbleEvent::AudioReceived { opus_data, .. } => {
                        if opus_tx.send(opus_data).is_err() {
                            break;
                        }
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

        // Store the session
        let session = CallSession {
            conf_port_id: None,
            conf_pool: None,
            media_port: SendablePort(media_port),
            encoder_handle,
            decoder_handle,
            voice_forward_handle,
            mumble_event_handle,
        };

        self.sessions.lock().unwrap().insert(call_id, session);
        info!("Session created for call {}", call_id);

        Ok(())
    }

    /// Record the conference port ID and pool assigned by the pjsip callback thread.
    pub fn on_call_media_active(
        &self,
        call_id: pjsua_call_id,
        conf_port_id: pjsua_conf_port_id,
        pool: *mut pj_pool_t,
    ) {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get_mut(&call_id) {
            session.conf_port_id = Some(conf_port_id);
            session.conf_pool = Some(SendablePool(pool));
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

        // Clean up any pending port registration that wasn't consumed
        callbacks::unregister_pending_port(call_id);

        // Remove from conference bridge
        if let Some(port_id) = session.conf_port_id {
            unsafe {
                pjsua_conf_remove_port(port_id);
            }
        }

        // Release the conference port pool
        if let Some(SendablePool(pool)) = session.conf_pool {
            unsafe {
                pj_pool_release(pool);
            }
        }

        // Destroy the custom media port
        unsafe {
            media_port::destroy_custom_port(session.media_port.0);
        }

        // Abort async tasks
        session.encoder_handle.abort();
        session.decoder_handle.abort();
        session.voice_forward_handle.abort();
        session.mumble_event_handle.abort();
    }
}
