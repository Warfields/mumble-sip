mod audio;
mod config;
mod db;
mod mumble;
mod session;
mod sip;
mod sms;

use std::sync::Arc;

use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info, warn};

use crate::audio::tts::PocketTtsRuntime;
use crate::config::Config;
use crate::db::{CallerStore, SqliteCallerStore};
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

    let tts_runtime = if config.tts.enabled {
        match PocketTtsRuntime::new(config.tts.clone(), config.audio.sample_rate) {
            Ok(runtime) => {
                let runtime = Arc::new(runtime);
                info!(
                    "Waiting for Pocket-TTS service to become available (timeout: {:.0}s)...",
                    config.tts.startup_timeout_ms as f64 / 1000.0
                );
                match runtime.startup().await {
                    Ok(()) => info!(
                        "Pocket-TTS service located at {}:{}",
                        config.tts.host, config.tts.port
                    ),
                    Err(err) => warn!(
                        "Pocket-TTS service startup check failed (continuing with chime fallback): {}",
                        err
                    ),
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

    // Initialize caller database
    let caller_store: Arc<dyn CallerStore> =
        Arc::new(SqliteCallerStore::new(&config.database.url()).await?);

    // Initialize pjsua and get event receiver — done last so we only accept
    // calls after TTS and the database are ready.
    let (_endpoint, mut sip_events) = PjsuaEndpoint::init(&config.sip_config())?;

    // Create session manager
    // TODO: Initialize SMS backend when implemented
    let sms_sender: Option<Arc<dyn sms::SmsSender>> = None;

    let session_mgr = Arc::new(SessionManager::new(
        config.clone(),
        tts_runtime,
        sms_sender,
        caller_store,
    ));

    info!("mumble-sip bridge started, waiting for calls...");

    let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to register SIGINT handler");

    // Main event loop: handle SIP events and signals
    loop {
        tokio::select! {
            event = sip_events.recv() => {
                let Some(event) = event else {
                    info!("SIP event channel closed, shutting down");
                    break;
                };
                match event {
                    SipEvent::IncomingCall { call_id, mumble_server, caller_number, .. } => {
                        let mgr = session_mgr.clone();
                        tokio::spawn(async move {
                            if let Err(e) = mgr.on_incoming_call(call_id, mumble_server, caller_number).await {
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
            _ = sigterm.recv() => { info!("Received SIGTERM, shutting down"); break; }
            _ = sigint.recv()  => { info!("Received SIGINT, shutting down");  break; }
        }
    }

    // Hang up all active calls.
    _endpoint.hangup_all_calls();

    // Drain: wait for each call to reach DISCONNECTED via its existing on_call_state
    // callback, or give up after 10s so pjsua_destroy() handles any stragglers.
    let shutdown_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if session_mgr.active_call_count() == 0 {
            break;
        }
        tokio::select! {
            event = sip_events.recv() => {
                match event {
                    Some(SipEvent::CallStateChanged { call_id, state }) if state == 6 => {
                        session_mgr.on_call_disconnected(call_id);
                    }
                    None => break,
                    _ => {}
                }
            }
            _ = tokio::time::sleep_until(shutdown_deadline) => {
                warn!("Shutdown timeout: {} call(s) still active, forcing cleanup",
                      session_mgr.active_call_count());
                break;
            }
        }
    }

    // Flush any pending DB writes before the runtime shuts down.
    if tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session_mgr.drain_pending_writes(),
    )
    .await
    .is_err()
    {
        warn!("Timed out waiting for pending DB writes to complete");
    } else {
        info!("All DB writes drained");
    }

    // _endpoint drops here → PjsuaEndpoint::drop() → pjsua_destroy()
    Ok(())
}
