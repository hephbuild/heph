use std::ffi::CString;
use std::os::raw::c_char;

/// Generate a new UUID v4
#[no_mangle]
pub extern "C" fn heph_uuid_new() -> *mut c_char {
    let uuid = heph_uuid::new();
    CString::new(uuid).unwrap().into_raw()
}
