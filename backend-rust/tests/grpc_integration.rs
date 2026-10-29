//! gRPC-Web Admin service — dashboard path. REST `/api/*` stays; these cover
//! the SPA transport: auth (NOT_FOUND without key), happy-path GetStats JSON.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use apex_analytics::app::{build_app, AppState, VersionConfig};
use apex_analytics::db::Db;
use http_body_util::BodyExt;
use tower::ServiceExt;

const ADMIN_KEY: &str = "test-admin-key-grpc-000000";

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_db_path() -> String {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!("dottie_grpc_{}_{}.db", std::process::id(), n));
    let p = path.to_string_lossy().into_owned();
    for f in [p.clone(), format!("{p}-wal"), format!("{p}-shm")] {
        let _ = std::fs::remove_file(&f);
    }
    p
}

fn cleanup(p: &str) {
    for f in [p.to_string(), format!("{p}-wal"), format!("{p}-shm")] {
        let _ = std::fs::remove_file(&f);
    }
}

async fn fresh_app() -> (axum::Router, String) {
    let p = temp_db_path();
    let db = Db::connect(&p, HashSet::new(), HashSet::new())
        .await
        .expect("connect db");
    let state = AppState::new(
        db,
        ADMIN_KEY.into(),
        VersionConfig {
            latest: "1.0".into(),
            min: "1.0".into(),
            download_url: "https://example.invalid".into(),

            default_provider: "dottiepro".into(),


            provider_models: Vec::new(),


            vision_model: "grok-4.3".into(),


            local_model: "ggml-org/gemma-4-E4B-it-GGUF".into(),


            local_model_high_mem: "ggml-org/gemma-4-12B-it-GGUF".into(),


            stt_model_url_base: "https://example.invalid".into(),
        },
    );
    (build_app(state), p)
}

/// gRPC-Web data frame: flag 0 + big-endian length + protobuf payload.
fn frame(msg: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + msg.len());
    out.push(0);
    out.extend_from_slice(&(msg.len() as u32).to_be_bytes());
    out.extend_from_slice(msg);
    out
}

fn grpc_req(method: &str, body: Vec<u8>, auth: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(format!("/apex.admin.v1.Admin/{method}"))
        .header("content-type", "application/grpc-web+proto")
        .header("x-grpc-web", "1");
    if let Some(a) = auth {
        b = b.header("authorization", format!("Bearer {a}"));
    }
    b.body(Body::from(body)).unwrap()
}

/// Parse gRPC-Web response: data payload + trailers grpc-status.
async fn parse_grpc_web(resp: axum::response::Response) -> (u16, i32, Vec<u8>) {
    let status = resp.status().as_u16();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let buf = bytes.as_ref();
    let mut i = 0usize;
    let mut data = Vec::new();
    let mut grpc_status = -1i32;
    while i + 5 <= buf.len() {
        let flags = buf[i];
        let len = u32::from_be_bytes([buf[i + 1], buf[i + 2], buf[i + 3], buf[i + 4]]) as usize;
        i += 5;
        if i + len > buf.len() {
            break;
        }
        let payload = &buf[i..i + len];
        i += len;
        if flags & 0x80 != 0 {
            let text = String::from_utf8_lossy(payload);
            for line in text.split(['\r', '\n']) {
                if let Some(v) = line.strip_prefix("grpc-status:") {
                    grpc_status = v.trim().parse().unwrap_or(-1);
                }
            }
        } else {
            data = payload.to_vec();
        }
    }
    (status, grpc_status, data)
}

/// Decode protobuf field 1 string (JsonBody.json).
fn decode_json_field1(buf: &[u8]) -> String {
    let mut i = 0usize;
    while i < buf.len() {
        let (key, ni) = decode_varint(buf, i);
        i = ni;
        let field = key >> 3;
        let wire = key & 7;
        if wire == 2 {
            let (len, nj) = decode_varint(buf, i);
            let len = len as usize;
            i = nj;
            let s = String::from_utf8_lossy(&buf[i..i + len]).into_owned();
            i += len;
            if field == 1 {
                return s;
            }
        } else if wire == 0 {
            let (_, nj) = decode_varint(buf, i);
            i = nj;
        } else {
            break;
        }
    }
    String::new()
}

fn decode_varint(buf: &[u8], mut i: usize) -> (u32, usize) {
    let mut result = 0u32;
    let mut shift = 0u32;
    while i < buf.len() {
        let b = buf[i];
        i += 1;
        result |= u32::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return (result, i);
        }
        shift += 7;
    }
    (result, i)
}

#[tokio::test]
async fn get_stats_ok_with_admin_key() {
    let (app, path) = fresh_app().await;
    let resp = app
        .oneshot(grpc_req("GetStats", frame(&[]), Some(ADMIN_KEY)))
        .await
        .expect("oneshot");
    let (http, grpc, data) = parse_grpc_web(resp).await;
    cleanup(&path);
    assert_eq!(http, StatusCode::OK.as_u16());
    assert_eq!(grpc, 0, "grpc-status should be OK");
    let json = decode_json_field1(&data);
    let v: serde_json::Value = serde_json::from_str(&json).expect("json");
    assert!(v.get("users").is_some(), "stats shape: {v}");
    assert!(v.get("dau").is_some());
}

#[tokio::test]
async fn get_stats_not_found_without_key() {
    let (app, path) = fresh_app().await;
    let resp = app
        .oneshot(grpc_req("GetStats", frame(&[]), None))
        .await
        .expect("oneshot");
    // Unary errors surface grpc-status on response headers (empty body).
    let grpc = resp
        .headers()
        .get("grpc-status")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(-1);
    cleanup(&path);
    assert_eq!(grpc, 5, "expected NOT_FOUND without admin key");
}

#[tokio::test]
async fn rest_stats_still_works() {
    let (app, path) = fresh_app().await;
    let req = Request::builder()
        .method("GET")
        .uri("/api/stats")
        .header("authorization", format!("Bearer {ADMIN_KEY}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    cleanup(&path);
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert!(v.get("users").is_some());
}

/// StreamOpts { interval_ms: 1000 } — field 1 varint.
fn encode_stream_opts(interval_ms: u32) -> Vec<u8> {
    // key = (1<<3)|0 = 8, then varint interval
    let mut out = vec![8];
    let mut n = interval_ms;
    while n > 0x7f {
        out.push((n as u8 & 0x7f) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
    out
}

#[tokio::test]
async fn stream_stats_emits_first_chunk() {
    use tokio::time::{timeout, Duration};
    let (app, path) = fresh_app().await;
    // interval 1000ms; first snapshot is immediate (before sleep).
    let resp = app
        .oneshot(grpc_req(
            "StreamStats",
            frame(&encode_stream_opts(1000)),
            Some(ADMIN_KEY),
        ))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    // Collect body with a short timeout — stream stays open, so we only wait
    // for the first framed message.
    let body = resp.into_body();
    let collected = timeout(Duration::from_secs(2), async {
        use http_body_util::BodyExt;
        // Read until we have at least one data frame (5 + payload).
        let mut acc = Vec::new();
        let mut body = body;
        while acc.len() < 6 {
            match body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        acc.extend_from_slice(&data);
                    }
                }
                Some(Err(e)) => panic!("body err: {e}"),
                None => break,
            }
        }
        acc
    })
    .await;
    cleanup(&path);
    let bytes = collected.expect("timed out waiting for first stream chunk");
    assert!(bytes.len() >= 5, "expected framed data, got {} bytes", bytes.len());
    // First frame should be a data frame (flag 0).
    assert_eq!(bytes[0], 0, "first frame should be data");
    let len = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
    assert!(bytes.len() >= 5 + len);
    let payload = &bytes[5..5 + len];
    let json = decode_json_field1(payload);
    let v: serde_json::Value = serde_json::from_str(&json).expect("json");
    assert!(v.get("users").is_some(), "stream stats shape: {v}");
}
