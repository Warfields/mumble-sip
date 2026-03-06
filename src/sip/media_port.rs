use std::ffi::CString;
use std::ptr;

use pjsip_sys::*;
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::HeapRb;

/// Shared ring buffer pair for passing PCM between pjsip's media thread and tokio.
///
/// These are lock-free SPSC (single-producer, single-consumer) ring buffers.
pub struct AudioBuffers {
    /// PCM samples flowing from SIP caller → Mumble (written by put_frame, read by encoder task)
    pub sip_to_mumble_prod: ringbuf::HeapProd<i16>,
    pub sip_to_mumble_cons: ringbuf::HeapCons<i16>,
    /// PCM samples flowing from Mumble → SIP caller (written by decoder task, read by get_frame)
    pub mumble_to_sip_prod: ringbuf::HeapProd<i16>,
    pub mumble_to_sip_cons: ringbuf::HeapCons<i16>,
}

/// Create a pair of ring buffers for audio bridging.
/// `capacity_samples` is per-direction buffer size in samples.
pub fn create_audio_buffers(capacity_samples: usize) -> AudioBuffers {
    let sip_to_mumble = HeapRb::<i16>::new(capacity_samples);
    let (sip_to_mumble_prod, sip_to_mumble_cons) = sip_to_mumble.split();

    let mumble_to_sip = HeapRb::<i16>::new(capacity_samples);
    let (mumble_to_sip_prod, mumble_to_sip_cons) = mumble_to_sip.split();

    AudioBuffers {
        sip_to_mumble_prod,
        sip_to_mumble_cons,
        mumble_to_sip_prod,
        mumble_to_sip_cons,
    }
}

/// Per-port user data stored in pjmedia_port.port_data.pdata.
/// This is accessed from pjsip's media thread, so the ring buffer halves
/// must be the correct SPSC endpoints.
struct PortUserData {
    /// put_frame writes here (SIP → Mumble direction)
    sip_to_mumble: ringbuf::HeapProd<i16>,
    /// get_frame reads from here (Mumble → SIP direction)
    mumble_to_sip: ringbuf::HeapCons<i16>,
}

/// Create a custom pjmedia_port backed by ring buffers.
///
/// The returned port must be added to the conference bridge with `pjsua_conf_add_port`.
///
/// # Safety
/// The `pool` must be a valid pjsip memory pool. The returned port is allocated on the
/// heap and must be cleaned up via `destroy_custom_port`.
pub unsafe fn create_custom_port(
    pool: *mut pj_pool_t,
    clock_rate: u32,
    samples_per_frame: u32,
    sip_to_mumble: ringbuf::HeapProd<i16>,
    mumble_to_sip: ringbuf::HeapCons<i16>,
) -> anyhow::Result<*mut pjmedia_port> {
    // Allocate the port struct on the heap (not in pjsip pool, so we control lifetime)
    let port = Box::into_raw(Box::new(pjmedia_port::default()));

    // Initialize port info
    let name = CString::new("mumble-bridge")?;
    let name_str = pj_str_t {
        ptr: name.as_ptr() as *mut _,
        slen: name.as_bytes().len() as pj_ssize_t,
    };

    // Signature: arbitrary unique value
    let signature = 0x4D554D42; // "MUMB" in hex

    unsafe {
        pjmedia_port_info_init(
            &mut (*port).info,
            &name_str,
            signature,
            clock_rate,
            1, // mono
            16, // 16-bit PCM
            samples_per_frame,
        );
    }

    // Store ring buffer halves in port_data
    let user_data = Box::into_raw(Box::new(PortUserData {
        sip_to_mumble,
        mumble_to_sip,
    }));
    unsafe {
        (*port).port_data.pdata = user_data as *mut _;
    }

    // Set callbacks
    unsafe {
        (*port).put_frame = Some(port_put_frame);
        (*port).get_frame = Some(port_get_frame);
        (*port).on_destroy = Some(port_on_destroy);
    }

    // Keep the CString alive by leaking it (it's a static port name)
    std::mem::forget(name);

    Ok(port)
}

/// Clean up a custom port. Called automatically by pjmedia when port is removed.
pub unsafe fn destroy_custom_port(port: *mut pjmedia_port) {
    if !port.is_null() {
        let pdata = unsafe { (*port).port_data.pdata };
        if !pdata.is_null() {
            unsafe { drop(Box::from_raw(pdata as *mut PortUserData)) };
        }
        unsafe { drop(Box::from_raw(port)) };
    }
}

/// put_frame: called by pjsip media thread when there's audio FROM the SIP caller.
/// We push the PCM samples into the sip_to_mumble ring buffer.
unsafe extern "C" fn port_put_frame(
    this_port: *mut pjmedia_port,
    frame: *mut pjmedia_frame,
) -> pj_status_t {
    let frame = unsafe { &*frame };

    // Only process audio frames
    if frame.type_ != pjmedia_frame_type_PJMEDIA_FRAME_TYPE_AUDIO {
        return 0;
    }

    let user_data = unsafe { &mut *((*this_port).port_data.pdata as *mut PortUserData) };

    let samples_count = frame.size / std::mem::size_of::<i16>();
    let samples =
        unsafe { std::slice::from_raw_parts(frame.buf as *const i16, samples_count) };

    // Push samples into ring buffer. If full, samples are dropped (acceptable for real-time audio).
    let _ = user_data.sip_to_mumble.push_slice(samples);

    0 // PJ_SUCCESS
}

/// get_frame: called by pjsip media thread when it needs audio TO SEND to the SIP caller.
/// We pull PCM samples from the mumble_to_sip ring buffer.
unsafe extern "C" fn port_get_frame(
    this_port: *mut pjmedia_port,
    frame: *mut pjmedia_frame,
) -> pj_status_t {
    let frame = unsafe { &mut *frame };
    let user_data = unsafe { &mut *((*this_port).port_data.pdata as *mut PortUserData) };

    let samples_count = frame.size / std::mem::size_of::<i16>();
    let buf = unsafe {
        std::slice::from_raw_parts_mut(frame.buf as *mut i16, samples_count)
    };

    let read = user_data.mumble_to_sip.pop_slice(buf);

    if read == 0 {
        // No data available — fill with silence
        buf.fill(0);
        frame.type_ = pjmedia_frame_type_PJMEDIA_FRAME_TYPE_NONE;
    } else {
        // If we got fewer samples than requested, zero-fill the rest
        if read < samples_count {
            buf[read..].fill(0);
        }
        frame.type_ = pjmedia_frame_type_PJMEDIA_FRAME_TYPE_AUDIO;
    }

    0 // PJ_SUCCESS
}

/// Destructor callback — clean up the user data.
unsafe extern "C" fn port_on_destroy(this_port: *mut pjmedia_port) -> pj_status_t {
    let pdata = unsafe { (*this_port).port_data.pdata };
    if !pdata.is_null() {
        unsafe {
            drop(Box::from_raw(pdata as *mut PortUserData));
            (*this_port).port_data.pdata = ptr::null_mut();
        }
    }
    0
}
