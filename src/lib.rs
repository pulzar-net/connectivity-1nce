//! The 1NCE carrier driver: a Pulzar extension module of the carrier kind
//! (pulzar-net/pulzr-package-sdk, "Build a carrier driver"), on the platform's
//! extension ABI 2, written with `pulzr-package`.
//!
//! Each event is one operation, `{"op", "args", "config", "now"}`, answered
//! `{"ok": ...}` or `{"error": {"message", "status"}}`. Every request to 1NCE
//! goes out through the host's `pz.http`, which holds the provider's
//! credentials: this module writes placeholders where they go and never sees
//! them. The OAuth token is kept in the install's `pz.state`.

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

/// What one call has to work with.
pub struct Call<'a> {
    pub config: &'a Value,
    pub invalidate: Vec<String>,
    pub host: &'a mut dyn Host,
}

impl Call<'_> {
    pub fn remember(&mut self, key: &str, value: Value, ttl: u64) {
        let text = value
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string());
        self.host
            .state_set(key, Some(&text), ttl.min(u32::MAX as u64) as u32);
    }

    pub fn forget(&mut self, key: &str) {
        self.host.state_set(key, None, 0);
    }
}

/// The host functions this driver uses: the platform's `pz.http` and
/// `pz.state` in the module, a recording in tests. A request or an array of
/// them in, a response or an array back.
pub trait Host {
    fn send(&mut self, request: &Value) -> Value;
    fn state_get(&mut self, key: &str) -> Option<String>;
    fn state_set(&mut self, key: &str, value: Option<&str>, ttl_s: u32);
}

/// One operation from the platform, answered.
pub fn handle(event: &Value, host: &mut dyn Host) -> Value {
    let empty = json!({});
    let mut call = Call {
        config: event.get("config").unwrap_or(&empty),
        invalidate: Vec::new(),
        host,
    };
    let op = event.get("op").and_then(Value::as_str).unwrap_or("");
    let args = event.get("args").unwrap_or(&empty);
    let mut answer = match onence::dispatch(op, args, &mut call) {
        Ok(result) => json!({"ok": result}),
        Err(f) => json!({"error": {"message": f.message, "status": f.status}}),
    };
    if !call.invalidate.is_empty() {
        answer["invalidate"] = json!(call.invalidate);
    }
    answer
}

// ── the module ─────────────────────────────────────────────────────────────

#[cfg(target_arch = "wasm32")]
mod module {
    use super::*;
    use pulzr_package::host::{http, http_all, state_get, state_set, Request, Response};
    use pulzr_package::{Answer, Event, Package};

    struct Platform;

    fn request(v: &Value) -> Request {
        Request {
            method: v["method"].as_str().unwrap_or("GET").into(),
            url: v["url"].as_str().unwrap_or("").into(),
            headers: v["headers"].as_object().cloned().unwrap_or_default(),
            body: v["body"].as_str().map(str::to_string),
            cache: v["cache"].as_u64().map(|c| c as u32),
            timeout_ms: None,
        }
    }

    fn response(r: Response) -> Value {
        json!({"status": r.status, "headers": r.headers, "body": r.body, "error": r.error})
    }

    fn failed(e: String) -> Value {
        json!({"status": 0, "error": e})
    }

    impl Host for Platform {
        fn send(&mut self, req: &Value) -> Value {
            match req {
                Value::Array(batch) => {
                    let requests: Vec<Request> = batch.iter().map(request).collect();
                    match http_all(&requests) {
                        Ok(rs) => Value::Array(rs.into_iter().map(response).collect()),
                        Err(e) => Value::Array(batch.iter().map(|_| failed(e.clone())).collect()),
                    }
                }
                one => http(&request(one)).map(response).unwrap_or_else(failed),
            }
        }

        fn state_get(&mut self, key: &str) -> Option<String> {
            state_get(key).and_then(|v| String::from_utf8(v).ok())
        }

        fn state_set(&mut self, key: &str, value: Option<&str>, ttl_s: u32) {
            // A full state only costs a token fetch on the next call.
            let _ = state_set(key, value.unwrap_or("").as_bytes(), ttl_s);
        }
    }

    #[derive(Default)]
    struct Carrier;

    impl Package for Carrier {
        fn handle(&mut self, event: Event) -> Answer {
            let value = match event {
                Event::Other(v) => v,
                _ => json!({}),
            };
            let mut answer = Answer::default();
            if let Value::Object(members) = handle(&value, &mut Platform) {
                answer.extra = members;
            }
            answer
        }
    }

    pulzr_package::export!(Carrier);
}
