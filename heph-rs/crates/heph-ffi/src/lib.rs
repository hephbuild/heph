//! FFI bindings for Heph libraries

use std::ffi::CString;
use std::os::raw::c_char;

mod uuid;
mod tref;
mod kv;

/// Free a string allocated by Rust
///
/// # Safety
/// The pointer must have been allocated by Rust via CString::into_raw()
/// and must not have been freed already.
#[no_mangle]
pub unsafe extern "C" fn heph_free_string(s: *mut c_char) {
    if !s.is_null() {
        let _ = CString::from_raw(s);
    }
}

/// Test function - returns "Hello from Rust!"
#[no_mangle]
pub extern "C" fn heph_hello() -> *mut c_char {
    let msg = "Hello from Rust!";
    CString::new(msg).unwrap().into_raw()
}
