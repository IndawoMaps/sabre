//! JNI entry points for `com.sabremaps.mobile.SabreNative`:
//!
//! ```kotlin
//! object SabreNative {
//!     external fun start(fileRoot: String, cacheBytes: Long): String
//!     external fun stop()
//! }
//! ```

use jni::objects::{JClass, JString};
use jni::sys::{jlong, jstring};
use jni::JNIEnv;

#[no_mangle]
pub extern "system" fn Java_com_sabremaps_mobile_SabreNative_start<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass<'a>,
    file_root: JString<'a>,
    cache_bytes: jlong,
) -> jstring {
    let root: Result<String, String> = env.get_string(&file_root)
        .map(Into::into)
        .map_err(|e| e.to_string());
    let result = root.and_then(|root| {
        std::panic::catch_unwind(|| crate::start(root, cache_bytes.max(0) as usize))
            .unwrap_or_else(|_| Err("sabre panicked while starting".into()))
    });
    env.new_string(crate::ffi::start_json(result))
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "system" fn Java_com_sabremaps_mobile_SabreNative_stop<'a>(
    _env: JNIEnv<'a>,
    _class: JClass<'a>,
) {
    let _ = std::panic::catch_unwind(crate::stop);
}
