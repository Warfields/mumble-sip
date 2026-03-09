mod bindings {
    #![allow(warnings)]
    #![allow(clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

pub use bindings::*;
