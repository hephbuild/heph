use std::ffi::{CStr, CString};
use std::os::raw::c_char;

/// Parse a target reference and return package:target format
/// Returns NULL on error
#[no_mangle]
pub extern "C" fn heph_tref_parse(input: *const c_char) -> *mut c_char {
    if input.is_null() {
        return std::ptr::null_mut();
    }

    let c_str = unsafe { CStr::from_ptr(input) };
    let input_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };

    match heph_tref::TargetRef::parse(input_str) {
        Ok(tref) => {
            let formatted = tref.format();
            CString::new(formatted).unwrap().into_raw()
        }
        Err(_) => std::ptr::null_mut(),
    }
}

/// Parse a target reference in package context
#[no_mangle]
pub extern "C" fn heph_tref_parse_in_package(
    input: *const c_char,
    pkg: *const c_char,
) -> *mut c_char {
    if input.is_null() || pkg.is_null() {
        return std::ptr::null_mut();
    }

    let input_str = unsafe {
        match CStr::from_ptr(input).to_str() {
            Ok(s) => s,
            Err(_) => return std::ptr::null_mut(),
        }
    };

    let pkg_str = unsafe {
        match CStr::from_ptr(pkg).to_str() {
            Ok(s) => s,
            Err(_) => return std::ptr::null_mut(),
        }
    };

    match heph_tref::TargetRef::parse_in_package(input_str, pkg_str) {
        Ok(tref) => {
            let formatted = tref.format();
            CString::new(formatted).unwrap().into_raw()
        }
        Err(_) => std::ptr::null_mut(),
    }
}

/// Get the package from a parsed target reference
#[no_mangle]
pub extern "C" fn heph_tref_get_package(input: *const c_char) -> *mut c_char {
    if input.is_null() {
        return std::ptr::null_mut();
    }

    let c_str = unsafe { CStr::from_ptr(input) };
    let input_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };

    match heph_tref::TargetRef::parse(input_str) {
        Ok(tref) => CString::new(tref.package).unwrap().into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Get the target name from a parsed target reference
#[no_mangle]
pub extern "C" fn heph_tref_get_target(input: *const c_char) -> *mut c_char {
    if input.is_null() {
        return std::ptr::null_mut();
    }

    let c_str = unsafe { CStr::from_ptr(input) };
    let input_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };

    match heph_tref::TargetRef::parse(input_str) {
        Ok(tref) => CString::new(tref.target).unwrap().into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}
