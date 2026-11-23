//! Everything that knows 1NCE's URLs, field names and quirks. Callers see the
//! carrier kind's shapes (pulzr-package-sdk connectivity.schema.json); 1NCE's own
//! records are cached by the host, per account, for the times below.

use crate::{Call, Failure, Result};
use serde_json::{json, Map, Value};

const API: &str = "https://api.1nce.com/management-api";

// How long 1NCE's own records are worth remembering. The SIM record is the
// long one because it is the hot one: it carries the address the device UI
// proxy resolves on every asset of a page, and an address survives
// re-attachment. A SIM changed in 1NCE's own portal reads stale for this long;
// a change made through Pulzr drops it at once (`forget`).
const SIM_TTL: u64 = 300;
const LIST_TTL: u64 = 60;
const QUOTA_TTL: u64 = 120;
// Yesterday's rows never change and today's moves slowly.
const DAILY_TTL: u64 = 900;
// Prices change when more is ordered; an hour is the host's ceiling.
const PRICES_TTL: u64 = 3600;
// Pages of 100 to walk: a stop, not a limit anyone should reach.
const LIST_PAGES: u32 = 25;
// Pages of 10, 1NCE's ceiling for orders.
const ORDER_PAGES: u32 = 50;
// Live attachment is one call per SIM; the host makes a batch at once.
const STATUS_BATCH: usize = 100;
const DEFAULT_TERM_MONTHS: i64 = 120;
const APN: &str = "iot.1nce.net";
// A token is refreshed a minute before it expires.
const TOKEN_MARGIN: u64 = 60;

pub const CAPABILITIES: &[&str] = &[
    "sim.inventory",
    "sim.status",
    "sim.usage",
    "sim.suspend",
    "sim.activate",
    "sim.topup",
    "sim.reset",
    "sim.lock",
    "sim.location",
    "apn.profile",
];

pub fn dispatch(op: &str, args: &Value, call: &mut Call) -> Result<Value> {
    let iccid = || -> Result<String> {
        let i = args.get("iccid").and_then(Value::as_str).unwrap_or("");
        if i.is_empty() || i.len() > 22 || !i.chars().all(|c| c.is_ascii_digit()) {
            return Err(Failure::new("An ICCID is up to 22 digits.", 400));
        }
        Ok(i.to_string())
    };
    match op {
        "check" => check(call),
        "list_sims" => list_sims(call, args.get("cursor").and_then(Value::as_str), args),
        "sim" => Ok(sim(&record(call, &iccid()?)?)),
        "usage" => usage(
            call,
            &iccid()?,
            str_arg(args, "start")?,
            str_arg(args, "end")?,
        ),
        "set_state" => {
            let active = args.get("active").and_then(Value::as_bool).unwrap_or(false);
            update(
                call,
                &iccid()?,
                &json!({"status": if active { "Enabled" } else { "Disabled" }}),
            )
        }
        "update" => update(call, &iccid()?, args.get("changes").unwrap_or(&json!({}))),
        "topup" => topup(call, &iccid()?),
        "reset" => reset(call, &iccid()?),
        "apn_profile" => Ok(
            json!({"apn": APN, "username": null, "password": null, "pin": null, "roaming": true}),
        ),
        "quota" => quota(
            call,
            &iccid()?,
            args.get("kind").and_then(Value::as_str).unwrap_or("data"),
        ),
        "statuses" => statuses(
            call,
            args.get("iccids")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        ),
        "connection" => Ok(connection(&get(
            call,
            &format!("/v1/sims/{}/status", iccid()?),
            0,
        )?)),
        "cell" => Ok(cell(&get(
            call,
            &format!("/v1/sims/{}/connectivity_info", iccid()?),
            0,
        )?)),
        "events" => events(
            call,
            &iccid()?,
            args.get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(25)
                .min(100),
        ),
        "prices" => prices(call),
        _ => Err(Failure::new(format!("1NCE does not support {op}."), 501)),
    }
}

fn str_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Failure::new(format!("{name} is required."), 400))
}

// ── transport ──────────────────────────────────────────────────────────────

/// An OAuth token for the account, from its state or fresh. The credentials
/// are placeholders the host fills in; this module never holds them.
fn token(call: &mut Call, fresh: bool) -> Result<String> {
    if !fresh {
        if let Some(t) = call.host.state_get("token") {
            return Ok(t);
        }
    }
    let response = call.host.send(&json!({
        "method": "POST",
        "url": format!("{API}/oauth/token"),
        "headers": {"Authorization": "Basic {{basic:client_id:client_secret}}",
                    "Content-Type": "application/json"},
        "body": "{\"grant_type\":\"client_credentials\"}",
    }));
    let status = response.get("status").and_then(Value::as_u64).unwrap_or(0);
    if status == 0 {
        return Err(Failure::new("1NCE unreachable", 502));
    }
    if status >= 400 {
        return Err(Failure::new(format!("1NCE auth failed: {status}"), 502));
    }
    let body: Value =
        serde_json::from_str(response.get("body").and_then(Value::as_str).unwrap_or(""))
            .map_err(|_| Failure::new("1NCE auth answered with something that is not JSON", 502))?;
    let value = body
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| Failure::new("1NCE auth answered without a token", 502))?
        .to_string();
    let ttl = body
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(3600)
        .saturating_sub(TOKEN_MARGIN)
        .max(1);
    call.remember("token", json!(value), ttl);
    Ok(value)
}

fn request(path: &str, method: &str, token: &str, body: Option<&Value>, cache: u64) -> Value {
    let mut r = json!({
        "method": method,
        "url": format!("{API}{path}"),
        "headers": {"Authorization": format!("Bearer {token}"), "Accept": "application/json"},
    });
    if let Some(b) = body {
        r["headers"]["Content-Type"] = json!("application/json");
        r["body"] = json!(b.to_string());
    }
    if cache > 0 {
        r["cache"] = json!(cache);
    }
    r
}

fn read(response: &Value) -> Result<Value> {
    let status = response.get("status").and_then(Value::as_u64).unwrap_or(0);
    let text = response.get("body").and_then(Value::as_str).unwrap_or("");
    match status {
        0 => Err(Failure::new(
            format!(
                "1NCE unreachable: {}",
                response
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("no answer")
            ),
            502,
        )),
        404 => Err(Failure::new("Not found in 1NCE", 404)),
        s if s >= 400 => Err(Failure::new(
            format!(
                "1NCE error {s}: {}",
                text.chars().take(200).collect::<String>()
            ),
            502,
        )),
        _ if text.trim().is_empty() => Ok(json!({})),
        _ => serde_json::from_str(text)
            .map_err(|_| Failure::new("1NCE answered with something that is not JSON", 502)),
    }
}

/// One call to 1NCE, with the token, once more with a fresh one if it was
/// refused as expired.
fn send(
    call: &mut Call,
    method: &str,
    path: &str,
    body: Option<&Value>,
    cache: u64,
) -> Result<Value> {
    let t = token(call, false)?;
    let response = call.host.send(&request(path, method, &t, body, cache));
    if response.get("status").and_then(Value::as_u64) == Some(401) {
        call.forget("token");
        let t = token(call, true)?;
        return read(&call.host.send(&request(path, method, &t, body, 0)));
    }
    read(&response)
}

fn get(call: &mut Call, path: &str, cache: u64) -> Result<Value> {
    send(call, "GET", path, None, cache)
}

// ── shapes ─────────────────────────────────────────────────────────────────

/// 1NCE returns some fields as {id, description} and some as a bare string.
fn text(v: Option<&Value>) -> Value {
    match v {
        Some(Value::Object(o)) => o.get("description").cloned().unwrap_or(Value::Null),
        Some(other) => other.clone(),
        None => Value::Null,
    }
}

fn field(raw: &Value, name: &str) -> Value {
    raw.get(name).cloned().unwrap_or(Value::Null)
}

/// One SIM, in the contract's names, with everything 1NCE says carried.
fn sim(raw: &Value) -> Value {
    let imei = match raw.get("imei") {
        Some(Value::String(s)) if s != "null" => json!(s),
        _ => Value::Null,
    };
    json!({
        "iccid": field(raw, "iccid"),
        "status": text(raw.get("status")),
        "activatedAt": field(raw, "activation_date"),
        "currentQuotaMb": field(raw, "current_quota"),
        "quotaStatus": text(raw.get("quota_status")),
        "smsRemaining": field(raw, "current_quota_SMS"),
        "smsStatus": text(raw.get("quota_status_SMS")),
        "ipAddress": field(raw, "ip_address"),
        "imsi": field(raw, "imsi"),
        "msisdn": field(raw, "msisdn"),
        "imei": imei,
        "imeiLock": field(raw, "imei_lock"),
        "label": field(raw, "label"),
    })
}

/// A quota block. `volume` is what is left and `total_volume` what was bought.
fn quota_shape(raw: &Value) -> Value {
    let remaining = raw.get("volume").and_then(Value::as_f64);
    let total = raw.get("total_volume").and_then(Value::as_f64);
    let used = match (remaining, total) {
        (Some(r), Some(t)) => Some(((t - r) * 1000.0).round() / 1000.0),
        _ => None,
    };
    let percent = match (used, total) {
        (Some(u), Some(t)) if t != 0.0 => Some((u / t * 1000.0).round() / 10.0),
        _ => None,
    };
    json!({
        "remaining": field(raw, "volume"),
        "total": field(raw, "total_volume"),
        "used": used,
        "usedPercent": percent,
        "expiryDate": field(raw, "expiry_date"),
        "lastAdded": field(raw, "last_volume_added"),
        "lastChangedAt": field(raw, "last_status_change_date"),
        "thresholdPercent": field(raw, "threshold_percentage"),
        "peakThroughput": field(raw, "peak_throughput"),
    })
}

fn at<'a>(v: &'a Value, path: &[&str]) -> &'a Value {
    let mut cur = v;
    for p in path {
        cur = cur.get(*p).unwrap_or(&Value::Null);
    }
    cur
}

fn or_null(v: &Value) -> Value {
    match v {
        Value::Null => Value::Null,
        Value::String(s) if s.is_empty() => Value::Null,
        Value::Number(n) if n.as_f64() == Some(0.0) => Value::Null,
        other => other.clone(),
    }
}

/// The live attachment, flattened out of /status. An offline SIM has none.
fn connection(status: &Value) -> Value {
    if !status.is_object() {
        return Value::Null;
    }
    let location = at(status, &["location"]);
    let pdp = at(status, &["pdp_context"]);
    let operator = at(location, &["operator"]);
    let country = if operator.get("country").is_some() {
        at(operator, &["country"])
    } else {
        at(location, &["country"])
    };
    let code = country
        .get("iso_code")
        .and_then(Value::as_str)
        .map(str::to_uppercase)
        .filter(|s| !s.is_empty());
    let mcc = if pdp.get("mcc").is_some_and(|v| !v.is_null() && v != "") {
        field(pdp, "mcc")
    } else {
        field(country, "mcc")
    };
    json!({
        "state": field(status, "status"),
        "services": status.get("services").cloned().filter(|v| !v.is_null()).unwrap_or(json!([])),
        "operator": field(operator, "name"),
        "country": field(country, "name"),
        "countryCode": code,
        "radio": field(at(pdp, &["rat_type"]), "description"),
        "sessionFor": field(pdp, "duration"),
        "sessionSince": field(pdp, "created"),
        "cellId": or_null(at(pdp, &["ci"])),
        "lac": or_null(at(pdp, &["lac"])),
        "mcc": mcc,
        "mnc": field(pdp, "mnc"),
        "sessionIp": field(pdp, "ue_ip_address"),
        "imeisv": field(pdp, "imeisv"),
        "lastSeen": field(location, "last_updated"),
        "lastSeenGprs": field(location, "last_updated_gprs"),
    })
}

/// The serving cell, when 1NCE managed to ask the network for it.
fn cell(connectivity: &Value) -> Value {
    let info = at(connectivity, &["subscriber_info"]);
    if info.get("success").and_then(Value::as_bool) != Some(true) {
        let reason = if info.get("error").is_some_and(|v| !v.is_null()) {
            field(info, "error")
        } else {
            field(info, "state")
        };
        return json!({"available": false, "reason": reason});
    }
    let location = at(info, &["location"]);
    let cgi = at(location, &["cell_global_id"]);
    json!({
        "available": true,
        "mcc": field(cgi, "mcc"), "mnc": field(cgi, "mnc"), "lac": field(cgi, "lac"),
        "cellId": field(cgi, "cid"),
        "ageSeconds": field(location, "age_of_location"),
        "state": field(info, "state"),
    })
}

// ── operations ─────────────────────────────────────────────────────────────

/// Proof the credentials work: a fresh token, not a held one.
fn check(call: &mut Call) -> Result<Value> {
    call.forget("token");
    token(call, true)?;
    Ok(json!(CAPABILITIES))
}

fn list_url(page: u32, size: u64) -> String {
    format!("/v1/sims?page={page}&pageSize={size}")
}

/// Every SIM on the account, following 1NCE's pagination to the end: 100 is
/// its page ceiling, and a short page is the last.
fn list_sims(call: &mut Call, cursor: Option<&str>, args: &Value) -> Result<Value> {
    if let Some(c) = cursor.filter(|c| !c.is_empty()) {
        let page: u32 = c
            .parse()
            .map_err(|_| Failure::new("cursor is a page number", 400))?;
        let size = args
            .get("pageSize")
            .and_then(Value::as_u64)
            .unwrap_or(100)
            .clamp(1, 100);
        let batch = get(call, &list_url(page, size), 0)?;
        let items: Vec<Value> = batch
            .as_array()
            .map(|a| a.iter().map(sim).collect())
            .unwrap_or_default();
        return Ok(json!({"items": items, "cursor": null}));
    }
    let mut items = Vec::new();
    for page in 1..=LIST_PAGES {
        let batch = get(call, &list_url(page, 100), LIST_TTL)?;
        let entries = batch.as_array().cloned().unwrap_or_default();
        let n = entries.len();
        items.extend(entries.iter().map(sim));
        if n < 100 {
            break;
        }
    }
    Ok(json!({"items": items, "cursor": null}))
}

fn record(call: &mut Call, iccid: &str) -> Result<Value> {
    get(call, &format!("/v1/sims/{iccid}"), SIM_TTL)
}

/// Drop what is cached about one SIM and the list it appears in, after
/// anything here changed it. 1NCE is the record; the cache is never patched.
fn forget(call: &mut Call, iccid: &str) {
    for path in [
        format!("/v1/sims/{iccid}"),
        format!("/v1/sims/{iccid}/quota/data"),
        format!("/v1/sims/{iccid}/quota/sms"),
    ] {
        call.invalidate.push(format!("{API}{path}"));
    }
    for page in 1..=LIST_PAGES {
        call.invalidate
            .push(format!("{API}{}", list_url(page, 100)));
    }
}

/// Status, IMEI lock and label: what 1NCE lets us change. A PUT of the whole
/// record (1NCE answers PATCH with 405, and a partial PUT could reset status),
/// read live first, since a cached record sent back would re-assert a status
/// somebody changed in between.
fn update(call: &mut Call, iccid: &str, changes: &Value) -> Result<Value> {
    let allowed = ["status", "imei_lock", "label"];
    let changes = changes.as_object().cloned().unwrap_or_default();
    if let Some(k) = changes.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(Failure::new(
            format!("{k} is not something 1NCE lets us change."),
            400,
        ));
    }
    let current = get(call, &format!("/v1/sims/{iccid}"), 0)?;
    let mut rec = Map::new();
    rec.insert("status".into(), text(current.get("status")));
    rec.insert("imei_lock".into(), field(&current, "imei_lock"));
    rec.insert(
        "label".into(),
        current
            .get("label")
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or(json!("")),
    );
    rec.extend(changes);
    send(
        call,
        "PUT",
        &format!("/v1/sims/{iccid}"),
        Some(&Value::Object(rec)),
        0,
    )?;
    forget(call, iccid);
    // 1NCE answers a PUT with an empty body; the console needs the new row.
    Ok(sim(&get(call, &format!("/v1/sims/{iccid}"), 0)?))
}

fn quota(call: &mut Call, iccid: &str, kind: &str) -> Result<Value> {
    if kind != "data" && kind != "sms" {
        return Err(Failure::new("A quota is data or sms.", 400));
    }
    Ok(quota_shape(&get(
        call,
        &format!("/v1/sims/{iccid}/quota/{kind}"),
        QUOTA_TTL,
    )?))
}

/// Per-day traffic, oldest first. Without a range 1NCE returns today only,
/// and its TOTAL pseudo-row is not a day.
fn usage(call: &mut Call, iccid: &str, start: &str, end: &str) -> Result<Value> {
    let body = get(
        call,
        &format!("/v1/sims/{iccid}/usage?start_dt={start}&end_dt={end}"),
        DAILY_TTL,
    )?;
    let mut out: Vec<Value> = body
        .get("stats")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|row| row.get("date").and_then(Value::as_str) != Some("TOTAL"))
        .map(|row| {
            let num = |v: &Value| {
                v.as_f64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    .unwrap_or(0.0)
            };
            let data = at(row, &["data"]);
            json!({
                "date": field(row, "date"),
                "mb": num(at(data, &["volume"])), "txMb": num(at(data, &["volume_tx"])),
                "rxMb": num(at(data, &["volume_rx"])), "cost": num(at(data, &["cost"])),
                "sms": num(at(row, &["sms", "volume"])),
            })
        })
        .collect();
    out.sort_by(|a, b| {
        a["date"]
            .as_str()
            .unwrap_or("")
            .cmp(b["date"].as_str().unwrap_or(""))
    });
    Ok(json!(out))
}

/// Buy a SIM more data: a real order. The platform decides whether top-ups
/// are allowed at all; the payment method is the provider's configuration.
fn topup(call: &mut Call, iccid: &str) -> Result<Value> {
    let method = call
        .config
        .get("topupPaymentMethod")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty() && m.chars().all(|c| c.is_ascii_alphanumeric()))
        .ok_or_else(|| Failure::new("This carrier has no top-up payment method configured.", 409))?
        .to_string();
    send(
        call,
        "POST",
        &format!("/v1/sims/{iccid}/topup?payment_method={method}"),
        None,
        0,
    )?;
    forget(call, iccid);
    Ok(json!({}))
}

/// Force a network re-attach: the remote equivalent of a power cycle.
fn reset(call: &mut Call, iccid: &str) -> Result<Value> {
    send(call, "POST", &format!("/v1/sims/{iccid}/reset"), None, 0)?;
    forget(call, iccid);
    Ok(json!({}))
}

/// Each SIM's attachment, by ICCID. A SIM whose own call fails is null,
/// unknown, never OFFLINE: "could not ask" and "not attached" send an
/// operator to different places.
fn statuses(call: &mut Call, iccids: Vec<Value>) -> Result<Value> {
    let t = token(call, false)?;
    let ids: Vec<String> = iccids
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    let mut out = Map::new();
    for chunk in ids.chunks(STATUS_BATCH) {
        let batch: Vec<Value> = chunk
            .iter()
            .map(|i| request(&format!("/v1/sims/{i}/status"), "GET", &t, None, 0))
            .collect();
        let answers = call.host.send(&json!(batch));
        for (i, iccid) in chunk.iter().enumerate() {
            let state = answers
                .get(i)
                .and_then(|r| read(r).ok())
                .map(|b| field(&b, "status"))
                .unwrap_or(Value::Null);
            out.insert(iccid.clone(), state);
        }
    }
    Ok(Value::Object(out))
}

/// Attach, detach and location updates, newest first. 1NCE refuses every
/// spelling of `sort`, so ordering is here.
fn events(call: &mut Call, iccid: &str, limit: u64) -> Result<Value> {
    let body = get(
        call,
        &format!("/v1/sims/{iccid}/events?pageSize={limit}"),
        0,
    )?;
    let mut out: Vec<Value> = body.get("events").and_then(Value::as_array).cloned().unwrap_or_default()
        .iter()
        .map(|e| json!({
            "at": field(e, "timestamp"), "type": field(e, "event_type"),
            "operator": field(e, "operator"), "country": field(e, "country"), "imei": field(e, "imei"),
            "alert": e.get("alert").is_some_and(|a| a.as_bool().unwrap_or(!a.is_null())),
            "severity": field(e, "event_severity"),
        }))
        .collect();
    out.sort_by(|a, b| {
        b["at"]
            .as_str()
            .unwrap_or("")
            .cmp(a["at"].as_str().unwrap_or(""))
    });
    Ok(json!(out))
}

// ── what the SIMs cost ─────────────────────────────────────────────────────
//
// For Pulzr staff's direct cost of an organization, read from what was
// ordered. Read only: no order is ever placed here but a top-up.

fn amount(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) if !s.is_empty() => s.parse().ok(),
        _ => None,
    }
}

fn prices(call: &mut Call) -> Result<Value> {
    let products: Map<String, Value> = match get(call, "/v1/products", PRICES_TTL) {
        Ok(Value::Array(list)) => list
            .into_iter()
            .filter_map(|p| p.get("id").map(|id| (id.to_string(), p.clone())))
            .collect(),
        _ => Map::new(),
    };
    let mut orders = Vec::new();
    for page in 1..=ORDER_PAGES {
        let batch = get(
            call,
            &format!("/v1/orders?page={page}&pageSize=10&sort=order_date"),
            PRICES_TTL,
        )?;
        let entries = batch.as_array().cloned().unwrap_or_default();
        let n = entries.len();
        orders.extend(entries);
        if n < 10 {
            break;
        }
    }
    orders.sort_by(|a, b| {
        a["order_date"]
            .as_str()
            .unwrap_or("")
            .cmp(b["order_date"].as_str().unwrap_or(""))
    });

    let (mut sims, mut topups) = (Map::new(), Map::new());
    let (mut latest_sim, mut latest_topup) = (Value::Null, Value::Null);
    for order in orders {
        let kind = order
            .get("order_type")
            .and_then(Value::as_str)
            .unwrap_or("");
        let is_sim = kind == "FIRST_ORDER" || kind == "ADDITIONAL_ORDER";
        let is_topup = kind == "TOPUP" || kind == "BULK_TOPUP";
        if !(is_sim || is_topup)
            || order.get("order_status").and_then(Value::as_str) == Some("CANCELLED")
        {
            continue;
        }
        let mut listed = order
            .get("sims")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if listed.is_empty() {
            if let Some(number) = order.get("order_number").and_then(amount) {
                listed = get(call, &format!("/v1/orders/{}", number as i64), PRICES_TTL)?
                    .get("sims")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
            }
        }
        let iccids: Vec<String> = listed
            .iter()
            .filter_map(|s| s.get("iccid").and_then(Value::as_str).map(String::from))
            .collect();
        let order_products = order
            .get("products")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let quantity = |p: &Value| p.get("quantity").and_then(amount).unwrap_or(0.0);
        let units = if !iccids.is_empty() {
            iccids.len() as f64
        } else {
            order_products.iter().map(quantity).sum()
        };
        let mut total = order.get("invoice_amount").and_then(amount);
        if total.is_none() && is_sim {
            // No invoice amount yet: the catalog's one-time charges instead.
            let charges: Vec<f64> = order_products
                .iter()
                .flat_map(|p| {
                    let product = products.get(
                        &p.get("product_id")
                            .map(Value::to_string)
                            .unwrap_or_default(),
                    );
                    let q = quantity(p);
                    product
                        .and_then(|pr| pr.get("one_time_charges"))
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .map(move |c| c.get("amount_net").and_then(amount).unwrap_or(0.0) * q)
                        .collect::<Vec<_>>()
                })
                .collect();
            total = if charges.is_empty() {
                None
            } else {
                Some(charges.iter().sum())
            };
        }
        let (Some(total), true) = (total, units > 0.0) else {
            continue;
        };
        let term = order_products
            .iter()
            .filter_map(|p| {
                products
                    .get(
                        &p.get("product_id")
                            .map(Value::to_string)
                            .unwrap_or_default(),
                    )
                    .and_then(|pr| pr.get("min_contract_duration"))
                    .and_then(amount)
                    .filter(|t| *t > 0.0)
            })
            .next()
            .map(|t| t as i64)
            .unwrap_or(DEFAULT_TERM_MONTHS);
        let number = match order.get("order_number") {
            Some(Value::String(s)) => s.clone(),
            Some(v) if !v.is_null() => v.to_string(),
            _ => "None".into(),
        };
        let entry = json!({
            "orderNumber": number,
            "orderDate": field(&order, "order_date"),
            "unitPrice": (total / units * 10000.0).round() / 10000.0,
            "currency": field(&order, "currency"),
            "termMonths": term,
        });
        let target = if is_sim { &mut sims } else { &mut topups };
        for iccid in iccids {
            target.insert(iccid, entry.clone());
        }
        if is_sim {
            latest_sim = entry
        } else {
            latest_topup = entry
        }
    }
    Ok(
        json!({"sims": sims, "topups": topups, "latestSim": latest_sim, "latestTopup": latest_topup}),
    )
}
