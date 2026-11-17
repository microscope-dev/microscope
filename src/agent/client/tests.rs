use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    body::Bytes,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::signature::{ED25519, UnparsedPublicKey};
use serde_json::{Value, json};

use super::{Client, Reply, outcome};
use crate::agent::{
    machine::Outcome,
    wire::{EnrollKey, ErrorBody, ErrorCode, KeyPair, PreviousRun, Server, signed_message},
};

const KEY: &str = "mse_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaasnb47oni";
const BOOT: &str = "0c4f8e57-2b8a-4d3e-9f61-0d2c5a1b7e93";

#[derive(Clone, Debug)]
struct Seen {
    path: String,
    body: Value,
    authorization: Option<String>,
    verified: bool,
}

type Script = Arc<Mutex<Vec<(StatusCode, Value)>>>;

struct Fake {
    _runtime: tokio::runtime::Runtime,
    origin: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

fn verify(
    origin: &str,
    path: &str,
    headers: &HeaderMap,
    body: &[u8],
    public_key: Option<&[u8]>,
) -> bool {
    let Some(header) = headers
        .get("microscope-signature")
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(("ed25519", encoded)) = header.split_once(' ') else {
        return false;
    };
    let (Ok(signature), Some(key)) = (URL_SAFE_NO_PAD.decode(encoded), public_key) else {
        return false;
    };
    UnparsedPublicKey::new(&ED25519, key)
        .verify(&signed_message(origin, path, body), &signature)
        .is_ok()
}

fn fake(public_key: [u8; 32], script: Vec<(StatusCode, Value)>) -> Fake {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let script: Script = Arc::new(Mutex::new(script));

    let handler = |path: &'static str| {
        let (seen, script, origin) = (seen.clone(), script.clone(), origin.clone());
        post(move |headers: HeaderMap, body: Bytes| async move {
            let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            seen.lock().unwrap().push(Seen {
                path: path.to_owned(),
                verified: verify(&origin, path, &headers, &body, Some(&public_key)),
                body: parsed,
                authorization: headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned),
            });
            let (status, body) = script.lock().unwrap().remove(0);
            let mut response: Response = if body.is_null() {
                status.into_response()
            } else {
                (
                    status,
                    [("content-type", "application/json")],
                    body.to_string(),
                )
                    .into_response()
            };
            if status == StatusCode::TOO_MANY_REQUESTS {
                response
                    .headers_mut()
                    .insert("retry-after", "42".parse().unwrap());
            }
            response
        })
    };
    let app = Router::new()
        .route("/api/agent/v1/enroll", handler("/api/agent/v1/enroll"))
        .route("/api/agent/v1/refresh", handler("/api/agent/v1/refresh"))
        .route("/api/agent/v1/goodbye", handler("/api/agent/v1/goodbye"));
    runtime.spawn(async move { axum::serve(listener, app).await });
    Fake {
        _runtime: runtime,
        origin,
        seen,
    }
}

fn token(agent_id: i64) -> (StatusCode, Value) {
    (
        StatusCode::CREATED,
        json!({ "agent_id": agent_id, "token": "msa1.x", "expires_in": 900, "refresh_after": 600 }),
    )
}

fn refused(status: StatusCode, error: &str) -> (StatusCode, Value) {
    (
        status,
        json!({ "error": error, "message": format!("said {error}") }),
    )
}

fn client(fake: &Fake) -> Client {
    Client::new(Server::parse(&fake.origin).unwrap().0, None).unwrap()
}

fn pair() -> KeyPair {
    KeyPair::from_seed([9; 32]).unwrap()
}

fn public(pair: &KeyPair) -> [u8; 32] {
    let encoded = serde_json::to_value(pair.public_key()).unwrap()["key"]
        .as_str()
        .unwrap()
        .to_owned();
    URL_SAFE_NO_PAD.decode(encoded).unwrap().try_into().unwrap()
}

#[test]
fn enrollment_is_signed_by_the_new_key_and_carries_the_enrollment_key() {
    let pair = pair();
    let fake = fake(public(&pair), vec![token(7)]);
    let mut client = client(&fake);
    let inventory = br#"{"hostname":"h","agent_version":"0.1.0"}"#;

    let reply = client.enroll(
        &pair,
        &EnrollKey::parse(KEY).unwrap(),
        BOOT,
        inventory,
        Some(3),
    );
    assert!(
        matches!(reply, Reply::Token(ref token) if token.agent_id == 7),
        "{reply:?}"
    );

    let seen = fake.seen.lock().unwrap()[0].clone();
    assert!(seen.verified);
    assert_eq!(
        seen.authorization.as_deref(),
        Some(format!("Enroll {KEY}").as_str())
    );
    assert_eq!(seen.body["boot_id"], BOOT);
    assert_eq!(seen.body["inventory"]["hostname"], "h");
    assert_eq!(seen.body["public_key"]["alg"], "ed25519");
    assert_eq!(
        seen.body["previous"],
        json!({ "agent_id": 3, "reason": "duplicate" })
    );
    assert!((seen.body["time"].as_i64().unwrap() - super::unix_now()).abs() <= 2);
    assert_eq!(
        seen.body["inventory_hash"],
        crate::agent::wire::inventory_hash(inventory)
    );
}

#[test]
fn a_refresh_sends_the_inventory_only_when_asked() {
    let pair = pair();
    let fake = fake(
        public(&pair),
        vec![refused(StatusCode::CONFLICT, "inventory"), token(7)],
    );
    let mut client = client(&fake);
    let reply = client.refresh(
        &pair,
        7,
        BOOT,
        br#"{"hostname":"h"}"#,
        Some(PreviousRun::AgentDied),
    );
    assert!(matches!(reply, Reply::Token(_)), "{reply:?}");

    let seen = fake.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert!(
        seen.iter()
            .all(|seen| seen.verified && seen.authorization.is_none())
    );
    assert_eq!(seen[0].body.get("inventory"), None);
    assert_eq!(seen[1].body["inventory"]["hostname"], "h");
    assert_eq!(seen[1].body["previous_run"], "agent_died");
    assert_eq!(seen[1].body["agent_id"], 7);
}

#[test]
fn a_wrong_clock_is_corrected_once_from_the_servers_time() {
    let pair = pair();
    let server_time = super::unix_now() + 3600;
    let clock = (
        StatusCode::UNAUTHORIZED,
        json!({ "error": "clock", "message": "skew", "time": server_time }),
    );
    let fake = fake(
        public(&pair),
        vec![clock.clone(), token(7), clock.clone(), clock],
    );
    let mut client = client(&fake);

    assert!(matches!(
        client.refresh(&pair, 7, BOOT, b"{}", None),
        Reply::Token(_)
    ));
    assert!((client.clock_offset() - 3600).abs() <= 2);
    let seen = fake.seen.lock().unwrap().clone();
    assert!((seen[1].body["time"].as_i64().unwrap() - server_time).abs() <= 2);

    let reply = client.refresh(&pair, 7, BOOT, b"{}", None);
    assert!(
        matches!(reply, Reply::Refused { status: 401, .. }),
        "{reply:?}"
    );
    assert_eq!(fake.seen.lock().unwrap().len(), 3);
}

#[test]
fn goodbye_carries_only_the_token() {
    let pair = pair();
    let fake = fake(public(&pair), vec![(StatusCode::NO_CONTENT, Value::Null)]);
    assert!(matches!(client(&fake).goodbye("msa1.tok"), Reply::Done));
    let seen = fake.seen.lock().unwrap()[0].clone();
    assert_eq!(seen.path, "/api/agent/v1/goodbye");
    assert_eq!(seen.authorization.as_deref(), Some("Bearer msa1.tok"));
    assert!(!seen.verified);
}

#[test]
fn every_answer_maps_to_an_outcome() {
    let pair = pair();
    let fake = fake(
        public(&pair),
        vec![
            refused(StatusCode::UNAUTHORIZED, "enroll_key"),
            refused(StatusCode::UNAUTHORIZED, "revoked"),
            refused(StatusCode::CONFLICT, "reenroll"),
            refused(StatusCode::FORBIDDEN, "quota"),
            refused(StatusCode::TOO_MANY_REQUESTS, "throttled"),
            (StatusCode::BAD_GATEWAY, Value::Null),
            refused(StatusCode::IM_A_TEAPOT, "something_new"),
        ],
    );
    let mut client = client(&fake);
    let mut outcomes = Vec::new();
    for _ in 0..7 {
        outcomes.push(outcome(&client.refresh(&pair, 7, BOOT, b"{}", None)).0);
    }
    assert_eq!(
        outcomes,
        [
            Outcome::EnrollKey,
            Outcome::Revoked,
            Outcome::Reenroll,
            Outcome::Retry { retry_after: None },
            Outcome::Retry {
                retry_after: Some(Duration::from_secs(42))
            },
            Outcome::Retry { retry_after: None },
            Outcome::Retry { retry_after: None },
        ]
    );
    let (_, message) = outcome(&Reply::Refused {
        status: 403,
        error: Some(ErrorBody {
            error: ErrorCode::Quota,
            message: "full".into(),
            time: None,
        }),
        retry_after: None,
    });
    assert_eq!(message.as_deref(), Some("the server answered 403: full"));
}

#[test]
fn an_unreachable_server_is_a_retry() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let mut client = Client::new(Server::parse(&origin).unwrap().0, None).unwrap();
    let reply = client.refresh(&pair(), 7, BOOT, b"{}", None);
    assert!(matches!(reply, Reply::Unreachable(_)), "{reply:?}");
    let (outcome, message) = outcome(&reply);
    assert_eq!(outcome, Outcome::Retry { retry_after: None });
    assert!(message.unwrap().starts_with("cannot reach the server"));
}

#[test]
fn a_ca_file_must_hold_certificates() {
    let scratch = crate::agent::scratch::Scratch::new();
    let empty = scratch.file("ca.pem", "");
    let server = Server::parse("https://a.test").unwrap().0;
    assert!(Client::new(server.clone(), Some(&empty)).is_err());
    assert!(Client::new(server, Some(&scratch.path().join("missing.pem"))).is_err());
}
