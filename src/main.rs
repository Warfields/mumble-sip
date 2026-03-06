mod audio;
mod mumble;
mod sip;

fn main() {
    // Phase 1 validation: verify pjsip-sys links and works
    unsafe {
        let status = pjsip_sys::pjsua_create();
        if status == 0 {
            println!("pjsua_create() succeeded");
            pjsip_sys::pjsua_destroy();
            println!("pjsua_destroy() succeeded");
        } else {
            eprintln!("pjsua_create() failed with status: {}", status);
            std::process::exit(1);
        }
    }
}
