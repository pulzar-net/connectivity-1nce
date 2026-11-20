//! The driver against recorded 1NCE responses. Every value is invented: no
//! real ICCID, address, account or credential appears here. Requests go to a
//! recording, never to 1NCE.

use carrier_1nce::{handle, Host};
use serde_json::{json, Value};
use std::collections::HashMap;

const API: &str = "https://api.1nce.com/management-api";
const ICCID: &str = "8988000000000000001";

struct Replay {
    responses: HashMap<(String, String), (u64, Value)>,
    sent: Vec<Value>,
    state: HashMap<String, (String, u32)>,
}

impl Replay {
    fn new() -> Self {
        let sim = json!({"iccid": ICCID, "status": {"id": 1, "description": "Enabled"}, "ip_address": "10.0.0.7",
                         "imei": "null", "imei_lock": false, "label": "rooftop", "current_quota": 400.0});
        let list: Vec<Value> = (0..3)
            .map(|i| {
                let mut s = sim.clone();
                s["iccid"] = json!(format!("898800000000000000{i}"));
                s
            })
            .collect();
        let mut r = Replay {
            responses: HashMap::new(),
            sent: Vec::new(),
            state: HashMap::new(),
        };
        r.on(
            "POST",
            "/oauth/token",
            200,
            json!({"access_token": "tok", "expires_in": 3600}),
        );
        r.on("GET", &format!("/v1/sims/{ICCID}"), 200, sim);
        r.on("GET", "/v1/sims?page=1&pageSize=100", 200, json!(list));
        r.on("PUT", &format!("/v1/sims/{ICCID}"), 200, Value::Null);
        r.on("POST", &format!("/v1/sims/{ICCID}/reset"), 200, json!({}));
        r.on(
            "POST",
            &format!("/v1/sims/{ICCID}/topup?payment_method=monthlyinvoice"),
            200,
            json!({}),
        );
        r.on(
            "GET",
            &format!("/v1/sims/{ICCID}/quota/data"),
            200,
            json!({"volume": 400.0, "total_volume": 500.0}),
        );
        r.on(
            "GET",
            &format!("/v1/sims/{ICCID}/status"),
            200,
            json!({"status": "ONLINE", "pdp_context": {
            "rat_type": {"description": "LTE"}, "ue_ip_address": "10.0.0.7"}}),
        );
        r.on(
            "GET",
            &format!("/v1/sims/{ICCID}/connectivity_info"),
            200,
            json!({"subscriber_info": {"success": false, "error": "x"}}),
        );
        r.on(
            "GET",
            &format!("/v1/sims/{ICCID}/usage?start_dt=2026-01-01&end_dt=2026-01-03"),
            200,
            json!({"stats": [
            {"date": "2026-01-02", "data": {"volume": 2, "volume_tx": 1, "volume_rx": 1}},
            {"date": "TOTAL", "data": {"volume": 2}},
            {"date": "2026-01-01", "data": {"volume": 1}}]}),
        );
        r
    }

    fn on(&mut self, method: &str, path: &str, status: u64, body: Value) {
        self.responses
            .insert((method.into(), format!("{API}{path}")), (status, body));
    }

    fn one(&mut self, req: &Value) -> Value {
        self.sent.push(req.clone());
        let key = (
            req["method"].as_str().unwrap_or("GET").to_string(),
            req["url"].as_str().unwrap_or("").to_string(),
        );
        match self.responses.get(&key) {
            Some((status, body)) => json!({"status": status, "headers": {},
                                           "body": if body.is_null() { String::new() } else { body.to_string() }}),
            None => json!({"status": 404, "headers": {}, "body": ""}),
        }
    }

    fn methods(&self) -> Vec<(String, String)> {
        self.sent
            .iter()
            .map(|r| {
                (
                    r["method"].as_str().unwrap_or("").into(),
                    r["url"]
                        .as_str()
                        .unwrap_or("")
                        .trim_start_matches(API)
                        .into(),
                )
            })
            .collect()
    }
}

impl Host for Replay {
    fn send(&mut self, request: &Value) -> Value {
        match request {
            Value::Array(batch) => json!(batch.iter().map(|r| self.one(r)).collect::<Vec<_>>()),
            r => self.one(r),
        }
    }

    fn state_get(&mut self, key: &str) -> Option<String> {
        self.state.get(key).map(|(v, _)| v.clone())
    }

    fn state_set(&mut self, key: &str, value: Option<&str>, ttl_s: u32) {
        match value {
            Some(v) if !v.is_empty() => {
                self.state.insert(key.into(), (v.into(), ttl_s));
            }
            _ => {
                self.state.remove(key);
            }
        }
    }
}

fn call(replay: &mut Replay, op: &str, args: Value, config: Value) -> Value {
    handle(
        &json!({"op": op, "args": args, "config": config, "now": 0}),
        replay,
    )
}

#[test]
fn check_proves_the_credentials_with_placeholders_only() {
    let mut r = Replay::new();
    let a = call(&mut r, "check", json!({}), json!({}));
    assert!(a["ok"].as_array().unwrap().contains(&json!("sim.reset")));
    let auth = &r.sent[0];
    assert_eq!(
        auth["headers"]["Authorization"],
        "Basic {{basic:client_id:client_secret}}"
    );
    assert_eq!(
        r.state["token"],
        ("tok".to_string(), 3600 - 60),
        "the token is kept in state, short of its expiry"
    );
}

#[test]
fn a_sim_is_normalized_and_cached() {
    let mut r = Replay::new();
    let a = call(&mut r, "sim", json!({"iccid": ICCID}), json!({}));
    assert_eq!(a["ok"]["ipAddress"], "10.0.0.7");
    assert_eq!(a["ok"]["status"], "Enabled");
    assert!(a["ok"]["imei"].is_null(), "\"null\" is how 1NCE says unset");
    let get = r
        .sent
        .iter()
        .find(|q| q["url"] == format!("{API}/v1/sims/{ICCID}"))
        .unwrap();
    assert_eq!(get["cache"], 300);
    assert_eq!(get["headers"]["Authorization"], "Bearer tok");
}

#[test]
fn a_held_token_is_used() {
    let mut r = Replay::new();
    r.state.insert("token".into(), ("held".into(), 60));
    handle(&json!({"op": "sim", "args": {"iccid": ICCID}}), &mut r);
    assert!(!r
        .methods()
        .contains(&("POST".into(), "/oauth/token".into())));
    assert_eq!(r.sent[0]["headers"]["Authorization"], "Bearer held");
}

#[test]
fn an_expired_token_is_fetched_again_once() {
    let mut r = Replay::new();
    r.on("GET", &format!("/v1/sims/{ICCID}"), 401, json!({}));
    r.state.insert("token".into(), ("old".into(), 60));
    let a = handle(&json!({"op": "sim", "args": {"iccid": ICCID}}), &mut r);
    assert_eq!(
        r.methods().iter().filter(|m| m.1 == "/oauth/token").count(),
        1
    );
    assert_eq!(
        a["error"]["status"], 502,
        "still refused after a fresh token: an error, not a loop"
    );
}

#[test]
fn the_account_list_ends_on_a_short_page() {
    let mut r = Replay::new();
    let a = call(&mut r, "list_sims", json!({}), json!({}));
    assert_eq!(a["ok"]["items"].as_array().unwrap().len(), 3);
    assert!(a["ok"]["cursor"].is_null());
    assert_eq!(
        r.methods()
            .iter()
            .filter(|m| m.1.starts_with("/v1/sims?"))
            .count(),
        1
    );
}

#[test]
fn usage_drops_the_total_row_and_sorts() {
    let mut r = Replay::new();
    let a = call(
        &mut r,
        "usage",
        json!({"iccid": ICCID, "start": "2026-01-01", "end": "2026-01-03"}),
        json!({}),
    );
    let dates: Vec<&str> = a["ok"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["date"].as_str().unwrap())
        .collect();
    assert_eq!(dates, ["2026-01-01", "2026-01-02"]);
}

#[test]
fn quota_reads_remaining_not_used() {
    let mut r = Replay::new();
    let q = &call(
        &mut r,
        "quota",
        json!({"iccid": ICCID, "kind": "data"}),
        json!({}),
    )["ok"];
    assert_eq!(
        (
            q["remaining"].as_f64(),
            q["used"].as_f64(),
            q["usedPercent"].as_f64()
        ),
        (Some(400.0), Some(100.0), Some(20.0))
    );
}

#[test]
fn a_change_sends_the_whole_record_and_drops_the_cache() {
    let mut r = Replay::new();
    let a = call(
        &mut r,
        "set_state",
        json!({"iccid": ICCID, "active": false}),
        json!({}),
    );
    let put = r.sent.iter().find(|q| q["method"] == "PUT").unwrap();
    let body: Value = serde_json::from_str(put["body"].as_str().unwrap()).unwrap();
    assert_eq!(
        body,
        json!({"status": "Disabled", "imei_lock": false, "label": "rooftop"})
    );
    let dropped = a["invalidate"].as_array().unwrap();
    assert!(dropped.contains(&json!(format!("{API}/v1/sims/{ICCID}"))));
    assert!(dropped.contains(&json!(format!("{API}/v1/sims?page=1&pageSize=100"))));
}

#[test]
fn a_change_to_anything_else_is_refused() {
    let mut r = Replay::new();
    let a = call(
        &mut r,
        "update",
        json!({"iccid": ICCID, "changes": {"msisdn": "1"}}),
        json!({}),
    );
    assert_eq!(a["error"]["status"], 400);
    assert!(r.sent.is_empty());
}

#[test]
fn a_topup_needs_a_payment_method() {
    let mut r = Replay::new();
    let a = call(&mut r, "topup", json!({"iccid": ICCID}), json!({}));
    assert_eq!(a["error"]["status"], 409);
    assert!(r.sent.is_empty());
    let a = call(
        &mut r,
        "topup",
        json!({"iccid": ICCID}),
        json!({"topupPaymentMethod": "monthlyinvoice"}),
    );
    assert!(a.get("ok").is_some(), "{a}");
}

#[test]
fn statuses_ask_in_one_batch_and_unknown_is_null() {
    let mut r = Replay::new();
    let a = call(
        &mut r,
        "statuses",
        json!({"iccids": [ICCID, "8988000000000000009"]}),
        json!({}),
    );
    assert_eq!(a["ok"][ICCID], "ONLINE");
    assert!(a["ok"]["8988000000000000009"].is_null());
}

#[test]
fn connection_and_cell() {
    let mut r = Replay::new();
    let c = call(&mut r, "connection", json!({"iccid": ICCID}), json!({}));
    assert_eq!(
        (c["ok"]["radio"].as_str(), c["ok"]["sessionIp"].as_str()),
        (Some("LTE"), Some("10.0.0.7"))
    );
    let cell = call(&mut r, "cell", json!({"iccid": ICCID}), json!({}));
    assert_eq!(cell["ok"], json!({"available": false, "reason": "x"}));
}

#[test]
fn a_404_is_not_found_and_a_bad_iccid_is_refused() {
    let mut r = Replay::new();
    r.on("GET", &format!("/v1/sims/{ICCID}"), 404, json!({}));
    assert_eq!(
        call(&mut r, "sim", json!({"iccid": ICCID}), json!({}))["error"]["status"],
        404
    );
    let mut r = Replay::new();
    assert_eq!(
        call(&mut r, "sim", json!({"iccid": "../oauth"}), json!({}))["error"]["status"],
        400
    );
    assert!(r.sent.is_empty());
}

#[test]
fn the_apn_is_1nces() {
    let mut r = Replay::new();
    assert_eq!(
        call(&mut r, "apn_profile", json!({}), json!({}))["ok"]["apn"],
        "iot.1nce.net"
    );
}
