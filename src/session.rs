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
use crate::sip::media_port;

/// Wrapper to make raw pjmedia_port pointer Send.
/// Safety: the pointer is only accessed from pjsip's media thread (via callbacks)
/// or when we explicitly destroy it during session teardown (under Mutex).
struct SendablePort(*mut pjmedia_port);
unsafe impl Send for SendablePort {}

/// A single active call session, bridging one SIP call to one Mumble connection.
struct CallSession {
    /// Conference port ID for the custom media port.
    conf_port_id: Option<pjsua_conf_port_id>,
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

        // Answer the SIP call
        unsafe {
            let status = pjsua_call_answer(call_id, 200, std::ptr::null(), std::ptr::null());
            if status != 0 {
                error!("Failed to answer call {}: status={}", call_id, status);
            }
        }

        // Store the session
        let session = CallSession {
            conf_port_id: None,
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

    /// Handle call media state change — attach the custom media port to the conference bridge.
    pub fn on_call_media_state(&self, call_id: pjsua_call_id) {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(session) = sessions.get_mut(&call_id) else {
            warn!("Media state changed for unknown call {}", call_id);
            return;
        };

        if session.conf_port_id.is_some() {
            debug!("Call {} media port already connected", call_id);
            return;
        }

        unsafe {
            let call_conf_port = pjsua_call_get_conf_port(call_id);
            if call_conf_port < 0 {
                warn!("Call {} has no conference port yet", call_id);
                return;
            }

            // Add our custom port to the conference bridge
            let mut port_id: pjsua_conf_port_id = 0;
            let status = pjsua_conf_add_port(
                std::ptr::null_mut(), // use default pool
                session.media_port.0,
                &mut port_id,
            );
            if status != 0 {
                error!("Failed to add media port to conference: status={}", status);
                return;
            }

            // Connect bidirectionally: call <-> custom port
            let status = pjsua_conf_connect(call_conf_port, port_id);
            if status != 0 {
                error!("Failed to connect call->bridge: status={}", status);
            }
            let status = pjsua_conf_connect(port_id, call_conf_port);
            if status != 0 {
                error!("Failed to connect bridge->call: status={}", status);
            }

            session.conf_port_id = Some(port_id);
            info!(
                "Call {} audio bridge connected (conf_port={})",
                call_id, port_id
            );
        }
    }

    /// Handle call disconnection — tear down the session.
    pub fn on_call_disconnected(&self, call_id: pjsua_call_id) {
        let session = self.sessions.lock().unwrap().remove(&call_id);
        let Some(session) = session else {
            return;
        };

        info!("Tearing down session for call {}", call_id);

        // Remove from conference bridge
        if let Some(port_id) = session.conf_port_id {
            unsafe {
                pjsua_conf_remove_port(port_id);
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
