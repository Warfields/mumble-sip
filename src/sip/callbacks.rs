use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use pjsip_sys::*;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Events sent from pjsua's callback thread to the tokio async world.
#[derive(Debug)]
pub enum SipEvent {
    IncomingCall {
        acc_id: pjsua_acc_id,
        call_id: pjsua_call_id,
        /// Mumble server from X-Mumble-Server header, if present.
        mumble_server: Option<String>,
        /// Caller phone number extracted from the SIP From URI.
        caller_number: Option<String>,
    },
    CallStateChanged {
        call_id: pjsua_call_id,
        state: u32,
    },
    CallMediaActive {
        call_id: pjsua_call_id,
        conf_port_id: pjsua_conf_port_id,
        pool: *mut pj_pool_t,
    },
}

// Safety: SipEvent contains raw pointers only in CallMediaActive, which is
// created on pjsip's thread and consumed on tokio's main task. The pool pointer
// is only used for cleanup (pj_pool_release) on a pj-registered thread.
unsafe impl Send for SipEvent {}

/// Global sender for bridging pjsua callbacks into tokio.
static EVENT_TX: OnceLock<mpsc::UnboundedSender<SipEvent>> = OnceLock::new();

/// Wrapper to make raw pjmedia_port pointer Send+Sync for storage in a static.
/// Safety: the pointer is only written from tokio and read from pjsip's callback thread,
/// synchronized via Mutex.
struct SendablePort(*mut pjmedia_port);
unsafe impl Send for SendablePort {}
unsafe impl Sync for SendablePort {}

/// Global registry of media ports waiting to be connected to the conference bridge.
/// Entries are inserted by SessionManager (from tokio) and consumed by the
/// on_call_media_state callback (from pjsip's thread).
static PENDING_PORTS: OnceLock<Mutex<HashMap<pjsua_call_id, SendablePort>>> = OnceLock::new();

fn pending_ports() -> &'static Mutex<HashMap<pjsua_call_id, SendablePort>> {
    PENDING_PORTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a media port to be connected when the call's media becomes active.
///
/// # Safety
/// The `port` pointer must remain valid until it is consumed by the callback
/// or removed via `unregister_pending_port`.
pub fn register_pending_port(call_id: pjsua_call_id, port: *mut pjmedia_port) {
    pending_ports().lock().unwrap().insert(call_id, SendablePort(port));
}

/// Remove a pending port registration (e.g. on call teardown before media was connected).
pub fn unregister_pending_port(call_id: pjsua_call_id) {
    pending_ports().lock().unwrap().remove(&call_id);
}

/// Initialize the global callback channel. Must be called before pjsua_init.
pub(super) fn init_callbacks() -> mpsc::UnboundedReceiver<SipEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    EVENT_TX
        .set(tx)
        .expect("SIP callbacks already initialized");
    rx
}

/// Get the pjsua_callback struct with our trampoline functions set.
pub(super) fn get_callback_struct() -> pjsua_callback {
    let mut cb: pjsua_callback = unsafe { std::mem::zeroed() };
    cb.on_incoming_call = Some(on_incoming_call);
    cb.on_call_state = Some(on_call_state);
    cb.on_call_media_state = Some(on_call_media_state);
    cb
}

fn send_event(event: SipEvent) {
    if let Some(tx) = EVENT_TX.get() {
        if tx.send(event).is_err() {
            warn!("SIP event channel closed, dropping event");
        }
    }
}

/// Extract the value of a custom SIP header from an incoming request.
///
/// # Safety
/// `rdata` must be a valid pointer to a pjsip_rx_data struct.
unsafe fn extract_header_value(rdata: *mut pjsip_rx_data, header_name: &str) -> Option<String> {
    if rdata.is_null() {
        return None;
    }

    let msg = unsafe { (*rdata).msg_info.msg };
    if msg.is_null() {
        return None;
    }

    let name_bytes = header_name.as_bytes();
    let name = pj_str_t {
        ptr: name_bytes.as_ptr() as *mut _,
        slen: name_bytes.len() as pj_ssize_t,
    };

    let hdr = unsafe { pjsip_msg_find_hdr_by_name(msg, &name, std::ptr::null()) };
    if hdr.is_null() {
        return None;
    }

    // Cast to generic string header to get the value
    let generic_hdr = hdr as *const pjsip_generic_string_hdr;
    let value = unsafe { &(*generic_hdr).hvalue };
    if value.slen <= 0 || value.ptr.is_null() {
        return None;
    }

    let slice = unsafe { std::slice::from_raw_parts(value.ptr as *const u8, value.slen as usize) };
    Some(String::from_utf8_lossy(slice).trim().to_string())
}

/// Extract the caller's phone number from a SIP URI string.
///
/// Handles formats like:
/// - `<sip:+17204919941@192.168.50.100>` → `7204919941`
/// - `"Display" <sip:1234@host>` → `1234`
/// - `sip:user@host` → `user`
fn extract_phone_number(remote_info: &str) -> Option<String> {
    // Find the user part of the SIP URI: between "sip:" and "@"
    let sip_start = remote_info.find("sip:")? + 4;
    let at_pos = remote_info[sip_start..].find('@')?;
    let user_part = &remote_info[sip_start..sip_start + at_pos];

    // Strip leading '+' and any country code prefix, keeping just digits
    let digits: String = user_part.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }

    // Strip leading '1' country code if the number is 11 digits (US/CA)
    if digits.len() == 11 && digits.starts_with('1') {
        Some(digits[1..].to_string())
    } else {
        Some(digits)
    }
}

/// C callback: incoming SIP call.
unsafe extern "C" fn on_incoming_call(
    acc_id: pjsua_acc_id,
    call_id: pjsua_call_id,
    rdata: *mut pjsip_rx_data,
) {
    info!("Incoming call (acc_id={}, call_id={})", acc_id, call_id);

    // Extract X-Mumble-Server header
    let mumble_server = unsafe { extract_header_value(rdata, "X-Mumble-Server") };
    if let Some(ref server) = mumble_server {
        info!("X-Mumble-Server: {}", server);
    }

    // Extract caller number from call info
    let caller_number = unsafe {
        let mut ci: pjsua_call_info = std::mem::zeroed();
        if pjsua_call_get_info(call_id, &mut ci) == 0 {
            let remote = std::slice::from_raw_parts(
                ci.remote_info.ptr as *const u8,
                ci.remote_info.slen as usize,
            );
            let remote_str = String::from_utf8_lossy(remote);
            let number = extract_phone_number(&remote_str);
            if let Some(ref num) = number {
                info!("Caller number: {}", num);
            }
            number
        } else {
            None
        }
    };

    send_event(SipEvent::IncomingCall {
        acc_id,
        call_id,
        mumble_server,
        caller_number,
    });
}

/// C callback: call state changed.
unsafe extern "C" fn on_call_state(call_id: pjsua_call_id, _e: *mut pjsip_event) {
    let mut ci: pjsua_call_info = unsafe { std::mem::zeroed() };
    let status = unsafe { pjsua_call_get_info(call_id, &mut ci) };
    if status != 0 {
        warn!("Failed to get call info for call_id={}", call_id);
        return;
    }

    let state = ci.state;
    debug!("Call {} state changed to {}", call_id, state);

    send_event(SipEvent::CallStateChanged {
        call_id,
        state: state as u32,
    });
}

/// C callback: call media state changed.
/// Runs on pjsip's thread, so we can safely call pjsua_conf_* functions here.
unsafe extern "C" fn on_call_media_state(call_id: pjsua_call_id) {
    debug!("Call {} media state changed", call_id);

    // Take the pending port for this call (if any)
    let sendable = pending_ports().lock().unwrap().remove(&call_id);
    let Some(SendablePort(port)) = sendable else {
        debug!("No pending media port for call {}", call_id);
        return;
    };

    unsafe {
        let call_conf_port = pjsua_call_get_conf_port(call_id);
        if call_conf_port < 0 {
            warn!("Call {} has no conference port yet", call_id);
            // Put it back so we can try again on the next media state change
            pending_ports().lock().unwrap().insert(call_id, SendablePort(port));
            return;
        }

        // Create a memory pool for this port's conference bridge slot
        let pool_name = std::ffi::CString::new(format!("mumb-call{}", call_id)).unwrap();
        let pool = pjsua_pool_create(pool_name.as_ptr(), 4096, 4096);
        if pool.is_null() {
            error!("Failed to create pool for call {} media port", call_id);
            return;
        }

        // Add our custom port to the conference bridge
        let mut port_id: pjsua_conf_port_id = 0;
        let status = pjsua_conf_add_port(pool, port, &mut port_id);
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

        info!(
            "Call {} audio bridge connected (conf_port={})",
            call_id, port_id
        );

        // Notify the session manager about the conf port ID and pool for cleanup
        send_event(SipEvent::CallMediaActive {
            call_id,
            conf_port_id: port_id,
            pool,
        });
    }
}
