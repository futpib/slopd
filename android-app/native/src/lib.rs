//! Small FFI adapter; the existing iroh-ssh library owns the tunnel protocol.
use serde_json::{Value, json};
use std::{
    ffi::{CStr, CString, c_char},
    sync::LazyLock,
    time::Duration,
};

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Tokio runtime")
});

async fn request(text: &str) -> Result<Value, String> {
    let request: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    match request["method"].as_str() {
        Some("connect") => {
            let endpoint = request["endpoint"].as_str().ok_or("missing endpoint")?;
            let relays = request["relays"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(|value| value.as_str().ok_or("invalid relay").map(str::to_owned))
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?
                .unwrap_or_default();
            let port = iroh_ssh::bridge::connect(
                endpoint.to_owned(),
                iroh_ssh::bridge::ConnectOptions {
                    relay_urls: relays,
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| e.to_string())?;
            Ok(json!({"port":port}))
        }
        Some("disconnect") => {
            let port = request["port"]
                .as_u64()
                .filter(|p| *p > 0 && *p <= u16::MAX as u64)
                .ok_or("invalid port")?;
            iroh_ssh::bridge::disconnect(port as u16)
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({}))
        }
        _ => Err("unknown transport method".into()),
    }
}

/// # Safety
/// `input` must be a valid NUL-terminated UTF-8 C string for the duration of this call.
/// The caller owns the returned string and must release it with `slopd_iroh_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopd_iroh_request(input: *const c_char) -> *mut c_char {
    let result = std::panic::catch_unwind(|| {
        // SAFETY: the FFI caller supplies a live NUL-terminated string.
        let text = unsafe { CStr::from_ptr(input) }
            .to_str()
            .map_err(|e| e.to_string())?;
        RUNTIME.block_on(async {
            tokio::time::timeout(Duration::from_secs(30), request(text))
                .await
                .map_err(|_| "Iroh transport timed out".to_owned())?
        })
    })
    .unwrap_or_else(|_| Err("Iroh transport panic".into()));
    let response = match result {
        Ok(value) => json!({"ok":true,"result":value}),
        Err(error) => json!({"ok":false,"error":error}),
    };
    CString::new(response.to_string())
        .expect("JSON has no NUL bytes")
        .into_raw()
}

/// # Safety
/// `pointer` must be a live string returned by `slopd_iroh_request`, freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopd_iroh_free(pointer: *mut c_char) {
    // SAFETY: ownership of this allocated string is returned by the FFI caller.
    drop(unsafe { CString::from_raw(pointer) });
}

#[cfg(target_os = "android")]
#[jni::jni_mangle("io.github.futpib.slopd_control.MainActivity")]
pub fn initVerifier<'caller>(
    mut env: jni::EnvUnowned<'caller>,
    _this: jni::objects::JObject<'caller>,
    context: jni::objects::JObject<'caller>,
) {
    env.with_env(|env| rustls_platform_verifier::android::init_with_env(env, context))
        .resolve::<jni::errors::ThrowRuntimeExAndDefault>();
}
