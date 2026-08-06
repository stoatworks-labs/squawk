//! API-level tests for the server.
//!
//! These drive the real router with `tower::oneshot`, so they cover the handlers,
//! the cascade rules and the serde shapes the browser UI depends on — the last of
//! which is not otherwise checked anywhere, and is exactly what breaks silently when
//! a field is renamed in `squawk-core`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use squawk_core::{Config, Endpoint, KeyTarget, Partyline};
use squawk_server::{app, AppState};
use tower::ServiceExt;

fn base_config() -> Config {
    let mut cfg = Config::default();
    cfg.partylines.push(Partyline::new("prod", "Production"));
    cfg.partylines.push(Partyline::new("stage", "Stage Crew"));
    for id in ["a", "b"] {
        let mut e = Endpoint::new(id, id);
        e.assign(KeyTarget::Partyline("prod".into())).unwrap();
        cfg.endpoints.push(e);
    }
    cfg
}

async fn call(state: &AppState, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let res = app(state.clone()).oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

fn state() -> AppState {
    AppState::new(base_config(), None)
}

/// Keys on one endpoint, as `(slot, target)` pairs.
fn keys_of<'a>(body: &'a Value, endpoint: &str) -> Vec<(u64, &'a Value)> {
    body["config"]["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == endpoint)
        .expect("endpoint present")["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| (k["slot"].as_u64().unwrap(), &k["target"]))
        .collect()
}

#[tokio::test]
async fn state_describes_the_system_the_ui_needs() {
    let (status, body) = call(&state(), "GET", "/api/state", None).await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(body["total_streams"], 2);
    assert_eq!(body["aes67_streams"], 2);
    // 2 streams at 48000/48 = 1000 packets per second each.
    assert_eq!(body["packets_per_second"], 2000);
    // "stage" has nobody on it, which is a warning and not an error.
    let problems = body["problems"].as_array().unwrap();
    assert!(problems.iter().all(|p| p["severity"] == "warning"));
    assert!(problems.iter().any(|p| p["kind"] == "empty-partyline"));
}

#[tokio::test]
async fn assigning_gives_the_endpoint_a_key_in_the_next_free_slot() {
    let s = state();
    let (status, body) = call(
        &s,
        "POST",
        "/api/assign",
        Some(json!({ "endpoint": "a", "target": { "partyline": "stage" } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let keys = keys_of(&body, "a");
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[1].0, 1, "should land in slot 1");
    assert_eq!(keys[1].1["partyline"], "stage");
}

#[tokio::test]
async fn a_second_key_at_the_same_target_is_refused() {
    // The mix-minus breaker — the API must not let the UI create one.
    let s = state();
    let (status, body) = call(
        &s,
        "POST",
        "/api/assign",
        Some(json!({ "endpoint": "a", "target": { "partyline": "prod" } })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap().contains("mix-minus"),
        "the error should say why, got: {}",
        body["error"]
    );
}

#[tokio::test]
async fn assigning_a_direct_target_creates_both_halves() {
    // A direct key with no key back carries silence, so a UI that could create one
    // could create a dead button.
    let s = state();
    let (status, body) = call(
        &s,
        "POST",
        "/api/assign",
        Some(json!({ "endpoint": "a", "target": { "direct": "b" } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(keys_of(&body, "a")[1].1["direct"], "b");
    assert_eq!(keys_of(&body, "b")[1].1["direct"], "a");
    // And therefore no one-way-direct warning.
    assert!(!body["problems"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["kind"] == "one-way-direct"));
}

#[tokio::test]
async fn removing_a_partyline_takes_its_keys_with_it() {
    let s = state();
    let (status, body) = call(&s, "DELETE", "/api/partylines/prod", None).await;
    assert_eq!(status, StatusCode::OK);

    assert!(keys_of(&body, "a").is_empty(), "orphaned key survived the cascade");
    assert!(keys_of(&body, "b").is_empty());
    // A cascade that left the keys behind would leave a config the engine refuses.
    assert!(body["problems"]
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["severity"] == "warning"));
}

#[tokio::test]
async fn removing_an_endpoint_takes_direct_keys_pointing_at_it() {
    let s = state();
    call(
        &s,
        "POST",
        "/api/assign",
        Some(json!({ "endpoint": "a", "target": { "direct": "b" } })),
    )
    .await;

    let (status, body) = call(&s, "DELETE", "/api/endpoints/b", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        keys_of(&body, "a").len(),
        1,
        "a's direct key to the deleted endpoint should have gone"
    );
    assert_eq!(keys_of(&body, "a")[0].1["partyline"], "prod");
}

#[tokio::test]
async fn unassign_removes_one_key_by_slot() {
    let s = state();
    let (status, body) = call(
        &s,
        "POST",
        "/api/unassign",
        Some(json!({ "endpoint": "a", "slot": 0 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(keys_of(&body, "a").is_empty());
    assert_eq!(keys_of(&body, "b").len(), 1, "b should be untouched");
}

#[tokio::test]
async fn an_endpoint_cannot_take_an_eleventh_key() {
    let s = state();
    for i in 0..squawk_core::MAX_KEYS {
        let pl = format!("pl{i}");
        call(
            &s,
            "POST",
            "/api/partylines",
            Some(json!({ "id": pl, "name": pl })),
        )
        .await;
        let (status, _) = call(
            &s,
            "POST",
            "/api/assign",
            Some(json!({ "endpoint": "b", "target": { "partyline": pl } })),
        )
        .await;
        // b starts with one key, so the last assign is the one that overflows.
        if i < squawk_core::MAX_KEYS - 1 {
            assert_eq!(status, StatusCode::OK, "assign {i} should have fitted");
        } else {
            assert_eq!(status, StatusCode::BAD_REQUEST, "assign {i} should have overflowed");
        }
    }
}

#[tokio::test]
async fn duplicate_ids_are_refused() {
    let s = state();
    let (status, _) = call(
        &s,
        "POST",
        "/api/partylines",
        Some(json!({ "id": "prod", "name": "Another Production" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = call(
        &s,
        "POST",
        "/api/endpoints",
        Some(json!({ "id": "a", "name": "Another A" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn changing_an_endpoint_to_mobile_moves_it_off_the_aes67_leg() {
    let s = state();
    let (status, body) = call(
        &s,
        "POST",
        "/api/endpoint-settings",
        Some(json!({ "endpoint": "a", "kind": "mobile" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Still mixed as a stream, but no longer transmitted as AES67.
    assert_eq!(body["total_streams"], 2);
    assert_eq!(body["aes67_streams"], 1);
    assert_eq!(body["packets_per_second"], 1000);
}

#[tokio::test]
async fn talk_is_accepted_and_never_written_to_config() {
    let s = state();
    let (status, _) = call(
        &s,
        "POST",
        "/api/talk",
        Some(json!({ "endpoint": "a", "slot": 0, "on": true })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Talk is runtime state: nobody wants yesterday's latched keys back on boot.
    let (_, body) = call(&s, "GET", "/api/state", None).await;
    let key = &body["config"]["endpoints"][0]["keys"][0];
    assert!(key.get("talk_on").is_none());
    assert!(key.get("talking").is_none());
}

#[tokio::test]
async fn the_ui_is_served_at_the_root() {
    let s = state();
    let res = app(s)
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("Assignment matrix"));
    assert!(html.contains("Simulated audio"), "the honesty note must survive");
}

#[tokio::test]
async fn meters_report_mix_minus_through_the_api() {
    // The end-to-end assertion: key a talker up and the other member hears them while
    // the talker's own feed stays at the floor.
    let s = state();
    call(
        &s,
        "POST",
        "/api/talk",
        Some(json!({ "endpoint": "a", "slot": 0, "on": true })),
    )
    .await;

    // Let the host thread run a few publish intervals.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let (status, body) = call(&s, "GET", "/api/meters", None).await;
    assert_eq!(status, StatusCode::OK);

    let level = |id: &str| -> f64 {
        body["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["id"] == id)
            .unwrap_or_else(|| panic!("no output {id}"))["db"]
            .as_f64()
            .unwrap()
    };

    assert_eq!(body["talking"].as_array().unwrap(), &[json!("a:0")]);
    assert!(level("a:0") < -119.0, "talker heard themselves: {}", level("a:0"));
    assert!(level("b:0") > -30.0, "listener heard nothing: {}", level("b:0"));
}
