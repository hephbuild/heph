use heph_kv::{sqlite::SqliteKvStore, KvStore};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;
use std::sync::Arc;

/// Opaque handle for KV store
pub struct KvStoreHandle {
    store: Arc<SqliteKvStore>,
}

/// Open a SQLite KV store at the given path
/// Returns NULL on error
#[no_mangle]
pub extern "C" fn heph_kv_open(path: *const c_char) -> *mut KvStoreHandle {
    if path.is_null() {
        return ptr::null_mut();
    }

    let c_str = unsafe { CStr::from_ptr(path) };
    let path_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };

    match SqliteKvStore::open(path_str) {
        Ok(store) => {
            let handle = Box::new(KvStoreHandle {
                store: Arc::new(store),
            });
            Box::into_raw(handle)
        }
        Err(_) => ptr::null_mut(),
    }
}

/// Open an in-memory SQLite KV store
/// Returns NULL on error
#[no_mangle]
pub extern "C" fn heph_kv_open_memory() -> *mut KvStoreHandle {
    match SqliteKvStore::in_memory() {
        Ok(store) => {
            let handle = Box::new(KvStoreHandle {
                store: Arc::new(store),
            });
            Box::into_raw(handle)
        }
        Err(_) => ptr::null_mut(),
    }
}

/// Close and free a KV store handle
#[no_mangle]
pub extern "C" fn heph_kv_close(handle: *mut KvStoreHandle) {
    if !handle.is_null() {
        unsafe {
            let _ = Box::from_raw(handle);
        }
    }
}

/// Get a value by key
/// Returns NULL if key not found or on error
/// Caller must free the returned string with heph_free_string
#[no_mangle]
pub extern "C" fn heph_kv_get(handle: *mut KvStoreHandle, key: *const c_char) -> *mut c_char {
    if handle.is_null() || key.is_null() {
        return ptr::null_mut();
    }

    let handle = unsafe { &*handle };
    let key_str = unsafe {
        match CStr::from_ptr(key).to_str() {
            Ok(s) => s,
            Err(_) => return ptr::null_mut(),
        }
    };

    match handle.store.get(key_str.as_bytes()) {
        Ok(Some(value)) => match String::from_utf8(value) {
            Ok(s) => CString::new(s).unwrap().into_raw(),
            Err(_) => ptr::null_mut(),
        },
        _ => ptr::null_mut(),
    }
}

/// Set a key-value pair
/// Returns 0 on success, -1 on error
#[no_mangle]
pub extern "C" fn heph_kv_set(
    handle: *mut KvStoreHandle,
    key: *const c_char,
    value: *const c_char,
) -> i32 {
    if handle.is_null() || key.is_null() || value.is_null() {
        return -1;
    }

    let handle = unsafe { &*handle };
    let key_str = unsafe {
        match CStr::from_ptr(key).to_str() {
            Ok(s) => s,
            Err(_) => return -1,
        }
    };
    let value_str = unsafe {
        match CStr::from_ptr(value).to_str() {
            Ok(s) => s,
            Err(_) => return -1,
        }
    };

    match handle.store.set(key_str.as_bytes(), value_str.as_bytes()) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

/// Delete a key
/// Returns 0 on success, -1 on error
#[no_mangle]
pub extern "C" fn heph_kv_delete(handle: *mut KvStoreHandle, key: *const c_char) -> i32 {
    if handle.is_null() || key.is_null() {
        return -1;
    }

    let handle = unsafe { &*handle };
    let key_str = unsafe {
        match CStr::from_ptr(key).to_str() {
            Ok(s) => s,
            Err(_) => return -1,
        }
    };

    match handle.store.delete(key_str.as_bytes()) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

/// Check if a key exists
/// Returns 1 if exists, 0 if not exists, -1 on error
#[no_mangle]
pub extern "C" fn heph_kv_exists(handle: *mut KvStoreHandle, key: *const c_char) -> i32 {
    if handle.is_null() || key.is_null() {
        return -1;
    }

    let handle = unsafe { &*handle };
    let key_str = unsafe {
        match CStr::from_ptr(key).to_str() {
            Ok(s) => s,
            Err(_) => return -1,
        }
    };

    match handle.store.exists(key_str.as_bytes()) {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(_) => -1,
    }
}
