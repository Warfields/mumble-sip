use std::sync::OnceLock;

use pjsip_sys::*;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Events sent from pjsua's callback thread to the tokio async world.
#[derive(Debug)]
pub enum SipEvent {
    IncomingCall {
        acc_id: pjsua_acc_id,
        call_id: pjsua_call_id,
        /// Mumble server from X-Mumble-Server header, if present.
        mumble_server: Option<String>,
    },
    CallStateChanged {
        call_id: pjsua_call_id,
        state: u32,
    },
    CallMediaStateChanged {
        call_id: pjsua_call_id,
    },
}

/// Global sender for bridging pjsua callbacks into tokio.
static EVENT_TX: OnceLock<mpsc::UnboundedSender<SipEvent>> = OnceLock::new();

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

    send_event(SipEvent::IncomingCall {
        acc_id,
        call_id,
        mumble_server,
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
unsafe extern "C" fn on_call_media_state(call_id: pjsua_call_id) {
    debug!("Call {} media state changed", call_id);

    send_event(SipEvent::CallMediaStateChanged { call_id });
}
