//! The C ABI, for Swift (or anything else that can call C).
//!
//! Strings in are UTF-8 and borrowed; strings out are owned by Rust and must
//! be handed back to [`sabre_string_free`].

use std::ffi::{c_char, CStr, CString};
use std::panic::catch_unwind;

/// Start the server (see [`crate::start`]). Returns JSON: either
/// `{"port":1234,"token":"…","base_url":"http://127.0.0.1:1234/…"}` or
/// `{"error":"…"}`. Never null.
///
/// # Safety
/// `file_root` must be a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn sabre_start(file_root: *const c_char, cache_bytes: u64) -> *mut c_char {
    let root = if file_root.is_null() {
        Err("file_root is null".to_string())
    } else {
        CStr::from_ptr(file_root).to_str().map(str::to_owned).map_err(|e| e.to_string())
    };
    let result = root.and_then(|root| {
        catch_unwind(|| crate::start(root, cache_bytes as usize))
            .unwrap_or_else(|_| Err("sabre panicked while starting".into()))
    });
    into_c(start_json(result))
}

/// Stop the server, if one is running.
#[no_mangle]
pub extern "C" fn sabre_stop() {
    let _ = catch_unwind(crate::stop);
}

/// Free a string returned by this library.
///
/// # Safety
/// `s` must have come from this library, and not been freed already.
#[no_mangle]
pub unsafe extern "C" fn sabre_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}

pub(crate) fn start_json(result: Result<crate::Endpoint, String>) -> String {
    match result {
        Ok(ep) => serde_json::json!({
            "port": ep.port, "token": ep.token, "base_url": ep.base_url(),
        }),
        Err(e) => serde_json::json!({ "error": e }),
    }.to_string()
}

fn into_c(s: String) -> *mut c_char {
    // JSON never contains a raw NUL, so this cannot fail.
    CString::new(s).unwrap_or_default().into_raw()
}
