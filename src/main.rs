mod audio;
mod config;
mod mumble;
mod session;
mod sip;

use std::sync::Arc;

use tracing::{error, info, warn};

use crate::audio::tts::PocketTtsRuntime;
use crate::config::Config;
use crate::session::SessionManager;
use crate::sip::PjsuaEndpoint;
use crate::sip::callbacks::SipEvent;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    // Load config
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());
    let config = Config::load(&config_path)?;
    if let Err(err) = config.validate() {
        error!("Configuration validation failed: {}", err);
        return Err(err);
    }
    info!("Configuration loaded from {}", config_path);

    // Initialize pjsua and get event receiver
    let (_endpoint, mut sip_events) = PjsuaEndpoint::init(&config.sip_config())?;

    let tts_runtime = if config.tts.enabled {
        match PocketTtsRuntime::new(config.tts.clone(), config.audio.sample_rate) {
            Ok(runtime) => {
                let runtime = Arc::new(runtime);
                if let Err(err) = runtime.startup().await {
                    warn!(
                        "Pocket-TTS sidecar startup failed (continuing with chime fallback): {}",
                        err
                    );
                }
                Some(runtime)
            }
            Err(err) => {
                warn!(
                    "Failed to initialize Pocket-TTS runtime (continuing with chime fallback): {}",
                    err
                );
                None
            }
        }
    } else {
        None
    };

    // Create session manager
    let session_mgr = Arc::new(SessionManager::new(config.clone(), tts_runtime));

    info!("mumble-sip bridge started, waiting for calls...");

    // Main event loop: handle SIP events
    while let Some(event) = sip_events.recv().await {
        match event {
            SipEvent::IncomingCall {
                call_id,
                mumble_server,
                caller_number,
                ..
            } => {
                let mgr = session_mgr.clone();
                tokio::spawn(async move {
                    if let Err(e) = mgr
                        .on_incoming_call(call_id, mumble_server, caller_number)
                        .await
                    {
                        error!("Failed to handle incoming call {}: {}", call_id, e);
                    }
                });
            }
            SipEvent::CallStateChanged { call_id, state } => {
                // PJSIP_INV_STATE_DISCONNECTED = 6
                if state == 6 {
                    session_mgr.on_call_disconnected(call_id);
                }
            }
            SipEvent::DtmfDigit { call_id, digit } => {
                session_mgr.on_dtmf_digit(call_id, digit);
            }
        }
    }

    info!("SIP event channel closed, shutting down");
    Ok(())
}
