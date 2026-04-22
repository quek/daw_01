use std::ffi::{CStr, c_char, c_void};

use clap_sys::host::clap_host;
use clap_sys::version::CLAP_VERSION;

const NAME: &CStr = c"daw_01";
const VENDOR: &CStr = c"daw_01";
const URL: &CStr = c"";
const VERSION: &CStr = c"0.1.0";

pub struct Host {
    pub clap: clap_host,
}

impl Host {
    pub fn new() -> Box<Self> {
        Box::new(Self {
            clap: clap_host {
                clap_version: CLAP_VERSION,
                host_data: std::ptr::null_mut(),
                name: NAME.as_ptr(),
                vendor: VENDOR.as_ptr(),
                url: URL.as_ptr(),
                version: VERSION.as_ptr(),
                get_extension: Some(get_extension),
                request_restart: Some(request_restart),
                request_process: Some(request_process),
                request_callback: Some(request_callback),
            },
        })
    }
}

unsafe extern "C" fn get_extension(
    _host: *const clap_host,
    _id: *const c_char,
) -> *const c_void {
    std::ptr::null()
}

unsafe extern "C" fn request_restart(_host: *const clap_host) {
    tracing::info!("host callback: request_restart");
}

unsafe extern "C" fn request_process(_host: *const clap_host) {
    tracing::info!("host callback: request_process");
}

unsafe extern "C" fn request_callback(_host: *const clap_host) {
    tracing::info!("host callback: request_callback");
}
