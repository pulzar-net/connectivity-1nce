//! The 1NCE carrier package: a connectivity driver as a WebAssembly module,
//! carrier ABI 1 (pulzar-net/connectivity-sdk, `ABI.md`).
//!
//! The platform calls `pz_call` with one operation as JSON and reads one
//! answer back. Every request to 1NCE goes out through the host's `pz.http`,
//! which holds the organization's credentials: this module writes
//! placeholders where they go and never sees them.

#![forbid(unsafe_op_in_unsafe_fn)]

use serde_json::{json, Value};

mod onence;

/// An operation's failure, as the platform reports it to its caller.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub message: String,
    pub status: u16,
}

impl Failure {
    pub fn new(message: impl Into<String>, status: u16) -> Self {
        Failure {
            message: message.into(),
            status,
        }
    }
}

pub type Result<T> = std::result::Result<T, Failure>;

/// What one call has to work with, and what it leaves for the next.
pub struct Call<'a> {
    pub config: &'a Value,
    pub memo: Value,
    pub memo_changes: serde_json::Map<String, Value>,
    pub invalidate: Vec<String>,
    pub transport: &'a mut dyn Transport,
}

impl Call<'_> {
    pub fn remember(&mut self, key: &str, value: Value, ttl: u64) {
        self.memo[key] = value.clone();
        self.memo_changes
            .insert(key.into(), json!({"value": value, "ttl": ttl}));
    }

    pub fn forget(&mut self, key: &str) {
        if let Some(m) = self.memo.as_object_mut() {
            m.remove(key);
        }
        self.memo_changes.insert(key.into(), Value::Null);
    }
}

/// How requests leave: the host's `pz.http` in the module, a recording in
/// tests. A request or an array of them in; a response or an array back.
pub trait Transport {
    fn send(&mut self, request: &Value) -> Value;
}

/// One operation from the platform, answered.
pub fn handle(event: &Value, transport: &mut dyn Transport) -> Value {
    let empty = json!({});
    let mut call = Call {
        config: event.get("config").unwrap_or(&empty),
        memo: event
            .get("memo")
            .cloned()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({})),
        memo_changes: serde_json::Map::new(),
        invalidate: Vec::new(),
        transport,
    };
    let op = event.get("op").and_then(Value::as_str).unwrap_or("");
    let args = event.get("args").unwrap_or(&empty);
    let mut answer = match onence::dispatch(op, args, &mut call) {
        Ok(result) => json!({"ok": result}),
        Err(f) => json!({"error": {"message": f.message, "status": f.status}}),
    };
    if !call.memo_changes.is_empty() {
        answer["memo"] = Value::Object(call.memo_changes);
    }
    if !call.invalidate.is_empty() {
        answer["invalidate"] = json!(call.invalidate);
    }
    answer
}

// ── the ABI ────────────────────────────────────────────────────────────────

#[cfg(target_arch = "wasm32")]
mod abi {
    use super::*;

    #[link(wasm_import_module = "pz")]
    extern "C" {
        fn http(ptr: *const u8, len: usize) -> i32;
        fn http_take(ptr: *mut u8);
    }

    struct Host;

    impl Transport for Host {
        fn send(&mut self, request: &Value) -> Value {
            let bytes = serde_json::to_vec(request).unwrap_or_default();
            // SAFETY: the host reads `bytes` for `len` during the call.
            let len = unsafe { http(bytes.as_ptr(), bytes.len()) };
            if len < 0 {
                return json!({"status": 0, "error": "the host refused the request"});
            }
            let mut buf = vec![0u8; len as usize];
            // SAFETY: the host writes exactly the `len` it announced.
            unsafe { http_take(buf.as_mut_ptr()) };
            serde_json::from_slice(&buf)
                .unwrap_or_else(|_| json!({"status": 0, "error": "unreadable response"}))
        }
    }

    // The answer stays readable until the next call.
    static mut ANSWER: Vec<u8> = Vec::new();

    #[no_mangle]
    pub extern "C" fn pz_abi() -> i32 {
        1
    }

    #[no_mangle]
    pub extern "C" fn pz_alloc(len: usize) -> *mut u8 {
        let mut buf = Vec::<u8>::with_capacity(len);
        let ptr = buf.as_mut_ptr();
        std::mem::forget(buf);
        ptr
    }

    #[no_mangle]
    pub extern "C" fn pz_call(ptr: *mut u8, len: usize) -> u64 {
        // SAFETY: `ptr` is what pz_alloc gave the host for exactly `len`.
        let event = unsafe { Vec::from_raw_parts(ptr, len, len) };
        let answer = match serde_json::from_slice::<Value>(&event) {
            Ok(e) => handle(&e, &mut Host),
            Err(_) => json!({"error": {"message": "the event is not JSON", "status": 400}}),
        };
        let bytes = serde_json::to_vec(&answer).unwrap_or_default();
        // SAFETY: one call at a time per instance (the host's lock).
        unsafe {
            let slot = &mut *std::ptr::addr_of_mut!(ANSWER);
            *slot = bytes;
            ((slot.as_ptr() as u64) << 32) | slot.len() as u64
        }
    }
}
