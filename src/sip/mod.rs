pub mod callbacks;
pub mod media_port;

use std::cell::RefCell;
use std::ffi::CString;

use pjsip_sys::*;
use tracing::{debug, error, info, warn};

use crate::sip::callbacks::SipEvent;

/// Register the current (external) thread with pjlib so that pjsua API calls
/// can be made from it. Uses thread-local storage to ensure each thread is
/// registered at most once. The pj_thread_desc must live as long as the thread.
pub fn ensure_pj_thread_registered() {
    thread_local! {
        static REGISTERED: RefCell<Option<Box<pj_thread_desc>>> = const { RefCell::new(None) };
    }

    REGISTERED.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_some() {
            return; // Already registered
        }

        unsafe {
            if pj_thread_is_registered() != 0 {
                return; // Already known to pjlib
            }

            let mut desc = Box::new(std::mem::zeroed::<pj_thread_desc>());
            let mut thread: *mut pj_thread_t = std::ptr::null_mut();
            let name = CString::new("tokio-worker").unwrap();
            let status = pj_thread_register(name.as_ptr(), desc.as_mut_ptr(), &mut thread);
            if status != 0 {
                warn!("pj_thread_register failed: {}", status);
                return;
            }
            // Keep the desc alive for the lifetime of the thread
            *slot = Some(desc);
        }
    });
}

/// Helper to create a pj_str_t from a Rust string.
/// The CString must be kept alive as long as the pj_str_t is in use.
fn pj_str_from_cstring(cstr: &CString) -> pj_str_t {
    pj_str_t {
        ptr: cstr.as_ptr() as *mut _,
        slen: cstr.as_bytes().len() as pj_ssize_t,
    }
}

/// Configuration for the SIP endpoint.
#[derive(Clone, Debug)]
pub struct SipConfig {
    pub listen_port: u16,
    pub account_uri: String,
    pub registrar: String,
    pub username: String,
    pub password: String,
    pub max_concurrent_calls: u32,
}

/// Wraps the global pjsua singleton lifecycle.
pub struct PjsuaEndpoint {
    _acc_id: pjsua_acc_id,
}

impl PjsuaEndpoint {
    /// Initialize pjsua, create transport, register account, and start.
    ///
    /// Returns a receiver for SIP events (incoming calls, state changes, etc.)
    pub fn init(
        config: &SipConfig,
    ) -> anyhow::Result<(Self, tokio::sync::mpsc::UnboundedReceiver<SipEvent>)> {
        let event_rx = callbacks::init_callbacks();

        unsafe {
            // Create pjsua
            check_status(pjsua_create(), "pjsua_create")?;

            // Configure pjsua
            let mut ua_cfg: pjsua_config = std::mem::zeroed();
            pjsua_config_default(&mut ua_cfg);
            ua_cfg.max_calls = config.max_concurrent_calls;
            ua_cfg.cb = callbacks::get_callback_struct();

            let mut log_cfg: pjsua_logging_config = std::mem::zeroed();
            pjsua_logging_config_default(&mut log_cfg);
            log_cfg.console_level = 3; // Reduce pjsip console noise

            let mut media_cfg: pjsua_media_config = std::mem::zeroed();
            pjsua_media_config_default(&mut media_cfg);
            media_cfg.clock_rate = 48000; // Match Mumble's Opus sample rate
            media_cfg.snd_clock_rate = 48000;
            media_cfg.no_vad = 1; // Disable VAD — we always stream

            check_status(
                pjsua_init(&ua_cfg, &log_cfg, &media_cfg),
                "pjsua_init",
            )?;
            debug!("pjsua initialized");

            // Create UDP transport
            let mut transport_cfg: pjsua_transport_config = std::mem::zeroed();
            pjsua_transport_config_default(&mut transport_cfg);
            transport_cfg.port = config.listen_port as u32;

            let mut transport_id: pjsua_transport_id = 0;
            check_status(
                pjsua_transport_create(
                    pjsip_transport_type_e_PJSIP_TRANSPORT_UDP,
                    &transport_cfg,
                    &mut transport_id,
                ),
                "pjsua_transport_create",
            )?;
            info!("SIP UDP transport created on port {}", config.listen_port);

            // Start pjsua
            check_status(pjsua_start(), "pjsua_start")?;
            debug!("pjsua started");

            // Register SIP account
            let acc_uri_cstr = CString::new(config.account_uri.as_str())?;
            let registrar_cstr = CString::new(config.registrar.as_str())?;
            let realm_cstr = CString::new("*")?;
            let username_cstr = CString::new(config.username.as_str())?;
            let password_cstr = CString::new(config.password.as_str())?;
            let scheme_cstr = CString::new("digest")?;

            let mut acc_cfg: pjsua_acc_config = std::mem::zeroed();
            pjsua_acc_config_default(&mut acc_cfg);
            acc_cfg.id = pj_str_from_cstring(&acc_uri_cstr);
            acc_cfg.reg_uri = pj_str_from_cstring(&registrar_cstr);
            acc_cfg.cred_count = 1;
            acc_cfg.cred_info[0].realm = pj_str_from_cstring(&realm_cstr);
            acc_cfg.cred_info[0].scheme = pj_str_from_cstring(&scheme_cstr);
            acc_cfg.cred_info[0].username = pj_str_from_cstring(&username_cstr);
            acc_cfg.cred_info[0].data_type = 0; // Plaintext password
            acc_cfg.cred_info[0].data = pj_str_from_cstring(&password_cstr);

            let mut acc_id: pjsua_acc_id = 0;
            check_status(
                pjsua_acc_add(&acc_cfg, 1, &mut acc_id),
                "pjsua_acc_add",
            )?;
            info!("SIP account registered (acc_id={})", acc_id);

            Ok((PjsuaEndpoint { _acc_id: acc_id }, event_rx))
        }
    }
}

impl Drop for PjsuaEndpoint {
    fn drop(&mut self) {
        info!("Shutting down pjsua");
        unsafe {
            pjsua_destroy();
        }
    }
}

fn check_status(status: pj_status_t, context: &str) -> anyhow::Result<()> {
    if status == 0 {
        Ok(())
    } else {
        // Get error message from pjsip
        let mut buf = [0u8; 256];
        unsafe {
            let err_str = pj_strerror(
                status,
                buf.as_mut_ptr() as *mut _,
                buf.len() as pj_size_t,
            );
            let msg = std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                err_str.ptr as *const u8,
                err_str.slen as usize,
            ));
            error!("{} failed (status={}): {}", context, status, msg);
            Err(anyhow::anyhow!("{} failed: {}", context, msg))
        }
    }
}
