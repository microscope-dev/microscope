mod support;

use std::{
    collections::{BTreeMap, VecDeque},
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use microscope::agent::wire::signed_message;
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use ring::signature::{ED25519, UnparsedPublicKey};
use serde_json::{Value, json};

const KEY: &str = "mse_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaasnb47oni";
const OTHER_KEY: &str = "mse_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbblgterh7w";
const WAIT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
struct Seen {
    path: &'static str,
    body: Value,
    authorization: Option<String>,
    verified: bool,
    public_key: Option<String>,
}

#[derive(Default)]
struct Books {
    keys: BTreeMap<i64, Vec<u8>>,
    hashes: BTreeMap<i64, String>,
    overrides: VecDeque<(&'static str, StatusCode, Value)>,
    next_id: i64,
}

#[derive(Clone)]
struct Fake {
    origin: String,
    books: Arc<Mutex<Books>>,
    seen: Arc<Mutex<mpsc::Sender<Seen>>>,
}

fn verified(origin: &str, path: &str, headers: &HeaderMap, body: &[u8], key: &[u8]) -> bool {
    let Some(("ed25519", encoded)) = headers
        .get("microscope-signature")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
    else {
        return false;
    };
    URL_SAFE_NO_PAD.decode(encoded).is_ok_and(|signature| {
        UnparsedPublicKey::new(&ED25519, key)
            .verify(&signed_message(origin, path, body), &signature)
            .is_ok()
    })
}

fn token(agent_id: i64, status: StatusCode) -> Response {
    let body = json!({ "agent_id": agent_id, "token": format!("msa1.{agent_id}"), "expires_in": 900, "refresh_after": 1 });
    (
        status,
        [("content-type", "application/json")],
        body.to_string(),
    )
        .into_response()
}

fn answer(status: StatusCode, body: &Value) -> Response {
    if status == StatusCode::NO_CONTENT {
        return status.into_response();
    }
    (
        status,
        [("content-type", "application/json")],
        body.to_string(),
    )
        .into_response()
}

fn handle(fake: &Fake, path: &'static str, headers: &HeaderMap, bytes: &Bytes) -> Response {
    let body: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    let known = body["agent_id"]
        .as_i64()
        .and_then(|id| fake.books.lock().unwrap().keys.get(&id).cloned());
    let public_key = body["public_key"]["key"].as_str().map(str::to_owned);
    let key = match path {
        "/api/agent/v1/enroll" => public_key
            .as_ref()
            .and_then(|key| URL_SAFE_NO_PAD.decode(key).ok()),
        _ => known,
    };
    let seen = Seen {
        path,
        verified: key
            .as_ref()
            .is_some_and(|key| verified(&fake.origin, path, headers, bytes, key)),
        authorization: headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        body: body.clone(),
        public_key,
    };
    let _ = fake.seen.lock().unwrap().send(seen.clone());
    decide(&mut fake.books.lock().unwrap(), path, &seen, &body, key)
}

fn decide(
    books: &mut Books,
    path: &'static str,
    seen: &Seen,
    body: &Value,
    key: Option<Vec<u8>>,
) -> Response {
    if let Some(at) = books.overrides.iter().position(|(on, ..)| *on == path) {
        let (_, status, reply) = books.overrides.remove(at).unwrap();
        return answer(status, &reply);
    }
    match path {
        "/api/agent/v1/enroll"
            if seen.verified && seen.authorization.as_deref() == Some(&format!("Enroll {KEY}")) =>
        {
            books.next_id += 1;
            let id = books.next_id;
            books.keys.insert(id, key.unwrap());
            books.hashes.insert(
                id,
                body["inventory_hash"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            );
            token(id, StatusCode::CREATED)
        }
        "/api/agent/v1/enroll" => answer(
            StatusCode::UNAUTHORIZED,
            &json!({ "error": "enroll_key", "message": "no" }),
        ),
        "/api/agent/v1/refresh" if seen.verified => {
            let id = body["agent_id"].as_i64().unwrap();
            let hash = body["inventory_hash"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            if books.hashes.get(&id) != Some(&hash) && body.get("inventory").is_none() {
                return answer(
                    StatusCode::CONFLICT,
                    &json!({ "error": "inventory", "message": "send it" }),
                );
            }
            books.hashes.insert(id, hash);
            token(id, StatusCode::OK)
        }
        "/api/agent/v1/refresh" => answer(
            StatusCode::UNAUTHORIZED,
            &json!({ "error": "signature", "message": "no" }),
        ),
        _ => StatusCode::NO_CONTENT.into_response(),
    }
}

struct Server {
    _runtime: tokio::runtime::Runtime,
    fake: Fake,
    events: mpsc::Receiver<Seen>,
}

impl Server {
    fn start() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let (sender, events) = mpsc::channel();
        let fake = Fake {
            origin: format!("http://{}", listener.local_addr().unwrap()),
            books: Arc::default(),
            seen: Arc::new(Mutex::new(sender)),
        };
        let route = |path: &'static str| {
            post(
                move |State(fake): State<Fake>, headers: HeaderMap, bytes: Bytes| async move {
                    handle(&fake, path, &headers, &bytes)
                },
            )
        };
        let app = Router::new()
            .route("/api/agent/v1/enroll", route("/api/agent/v1/enroll"))
            .route("/api/agent/v1/refresh", route("/api/agent/v1/refresh"))
            .route("/api/agent/v1/goodbye", route("/api/agent/v1/goodbye"))
            .with_state(fake.clone());
        runtime.spawn(async move { axum::serve(listener, app).await });
        Self {
            _runtime: runtime,
            fake,
            events,
        }
    }

    fn next(&self, path: &str) -> Seen {
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let seen = self
                .events
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no {path} within {WAIT:?}"));
            if seen.path == path {
                return seen;
            }
        }
    }

    fn answer_next(&self, path: &'static str, status: StatusCode, body: Value) {
        self.fake
            .books
            .lock()
            .unwrap()
            .overrides
            .push_back((path, status, body));
    }

    fn enroll(&self, key: &str) -> String {
        let with_key = self.fake.origin.replacen("://", &format!("://{key}@"), 1);
        format!("enroll = \"{with_key}\"\n")
    }
}

struct Agent {
    child: Option<Child>,
    state: PathBuf,
    config: PathBuf,
}

fn start(scratch: &support::ScratchDir, config: &str) -> Agent {
    let state = scratch.path().join("state");
    let path = scratch.path().join("agent.toml");
    std::fs::write(&path, config).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_microscope"))
        .args(["agent", "run", "--config"])
        .arg(&path)
        .env("MICROSCOPE_STATE_DIR", &state)
        .env("MICROSCOPE_CLOUD_METADATA", "false")
        .env_remove("KUBERNETES_SERVICE_HOST")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    Agent {
        child: Some(child),
        state,
        config: path,
    }
}

impl Agent {
    fn signal(&self, signal: Signal) {
        let id = self.child.as_ref().unwrap().id();
        kill(Pid::from_raw(i32::try_from(id).unwrap()), signal).unwrap();
    }

    fn exit_within(mut self, limit: Duration) -> (std::process::ExitStatus, String) {
        let mut child = self.child.take().unwrap();
        let started = Instant::now();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let status = child.wait();
            let mut stderr = String::new();
            let _ = std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut stderr);
            let _ = sender.send((status, stderr, child));
        });
        let (status, stderr, _child) = receiver
            .recv_timeout(limit)
            .unwrap_or_else(|_| panic!("still running after {limit:?}"));
        assert!(started.elapsed() <= limit, "took {:?}", started.elapsed());
        (status.unwrap(), stderr)
    }

    fn status(&self) -> (i32, String) {
        let output = Command::new(env!("CARGO_BIN_EXE_microscope"))
            .args(["agent", "status", "--config"])
            .arg(&self.config)
            .env("MICROSCOPE_STATE_DIR", &self.state)
            .output()
            .unwrap();
        (
            output.status.code().unwrap(),
            String::from_utf8(output.stdout).unwrap(),
        )
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn an_agent_enrolls_refreshes_and_says_goodbye_on_sigterm() {
    let server = Server::start();
    let scratch = support::ScratchDir::new("agent-lifecycle");
    let agent = start(
        &scratch,
        &format!("{}[tags]\nenvironment = \"test\"\n", server.enroll(KEY)),
    );

    let enroll = server.next("/api/agent/v1/enroll");
    assert!(enroll.verified, "{enroll:?}");
    assert_eq!(
        enroll.authorization.as_deref(),
        Some(format!("Enroll {KEY}").as_str())
    );
    assert_eq!(
        enroll.body["inventory"]["tags"],
        json!({ "environment": "test" })
    );
    assert_eq!(enroll.body["inventory"]["key_algs"], json!(["ed25519"]));
    assert_eq!(enroll.body.get("previous"), None);

    let refresh = server.next("/api/agent/v1/refresh");
    assert!(refresh.verified);
    assert_eq!(refresh.body["agent_id"], 1);
    assert_eq!(refresh.body.get("previous_run"), None);
    assert_eq!(refresh.body.get("inventory"), None);
    server.next("/api/agent/v1/refresh");

    assert_eq!(mode(&agent.state), 0o700);
    assert_eq!(mode(&agent.state.join("key")), 0o600);
    assert!(agent.state.join("running").exists());
    let (code, status) = agent.status();
    assert_eq!(code, 0, "{status}");
    assert!(status.contains("agent        1\n"), "{status}");
    assert!(status.contains("state        enrolled since"), "{status}");
    assert!(status.contains("key          SHA256:"), "{status}");

    agent.signal(Signal::SIGTERM);
    let goodbye = server.next("/api/agent/v1/goodbye");
    assert_eq!(goodbye.authorization.as_deref(), Some("Bearer msa1.1"));
    let state = agent.state.clone();
    let config = agent.config.clone();
    let (exit, stderr) = agent.exit_within(Duration::from_secs(3));
    assert!(exit.success(), "{exit:?} {stderr}");
    assert!(
        stderr.contains("enrolled with http://127.0.0.1"),
        "{stderr}"
    );
    assert!(!state.join("running").exists());

    let after = Agent {
        child: None,
        state,
        config,
    };
    let (code, status) = after.status();
    assert_eq!(code, 3);
    assert!(status.contains("state        not running"), "{status}");
}

#[test]
fn a_restart_keeps_the_agent_and_a_kill_is_reported_as_a_death() {
    let server = Server::start();
    let scratch = support::ScratchDir::new("agent-restart");
    let config = server.enroll(KEY);

    let first = start(&scratch, &config);
    let enrolled = server.next("/api/agent/v1/enroll");
    server.next("/api/agent/v1/refresh");
    first.signal(Signal::SIGTERM);
    server.next("/api/agent/v1/goodbye");
    first.exit_within(Duration::from_secs(3));

    let second = start(&scratch, &config);
    let refresh = server.next("/api/agent/v1/refresh");
    assert_eq!(refresh.body["agent_id"], 1);
    assert_eq!(refresh.body["previous_run"], "clean");
    assert!(refresh.verified);
    second.signal(Signal::SIGKILL);
    second.exit_within(Duration::from_secs(3));

    let third = start(&scratch, &config);
    let refresh = server.next("/api/agent/v1/refresh");
    assert_eq!(refresh.body["agent_id"], 1);
    assert_eq!(refresh.body["previous_run"], "agent_died");
    let again = server.next("/api/agent/v1/refresh");
    assert_eq!(again.body.get("previous_run"), None);
    assert!(enrolled.public_key.is_some());
    third.signal(Signal::SIGINT);
    server.next("/api/agent/v1/goodbye");
    assert!(third.exit_within(Duration::from_secs(3)).0.success());
}

#[test]
fn reenroll_makes_a_new_key_that_points_back() {
    let server = Server::start();
    let scratch = support::ScratchDir::new("agent-reenroll");
    let agent = start(&scratch, &server.enroll(KEY));

    let first = server.next("/api/agent/v1/enroll");
    server.answer_next(
        "/api/agent/v1/refresh",
        StatusCode::CONFLICT,
        json!({ "error": "reenroll", "message": "copied" }),
    );
    server.next("/api/agent/v1/refresh");
    let second = server.next("/api/agent/v1/enroll");
    assert_ne!(first.public_key, second.public_key);
    assert_eq!(
        second.body["previous"],
        json!({ "agent_id": 1, "reason": "duplicate" })
    );
    assert!(second.verified);

    let refresh = server.next("/api/agent/v1/refresh");
    assert_eq!(refresh.body["agent_id"], 2);
    assert!(refresh.verified);
    drop(agent);
}

#[test]
fn a_revoked_agent_drops_its_key_and_starts_over_when_told() {
    let server = Server::start();
    let scratch = support::ScratchDir::new("agent-revoked");
    let agent = start(&scratch, &server.enroll(KEY));

    let first = server.next("/api/agent/v1/enroll");
    server.answer_next(
        "/api/agent/v1/refresh",
        StatusCode::UNAUTHORIZED,
        json!({ "error": "revoked", "message": "an owner revoked it" }),
    );
    server.next("/api/agent/v1/refresh");
    agent.signal(Signal::SIGHUP);

    let second = server.next("/api/agent/v1/enroll");
    assert_ne!(first.public_key, second.public_key);
    assert_eq!(second.body.get("previous"), None);
    agent.signal(Signal::SIGTERM);
    let (exit, stderr) = agent.exit_within(Duration::from_secs(3));
    assert!(exit.success());
    assert!(
        stderr.contains("revoked: the server answered 401: an owner revoked it"),
        "{stderr}"
    );
}

#[test]
fn a_wrong_enrollment_key_waits_for_a_fixed_config() {
    let server = Server::start();
    let scratch = support::ScratchDir::new("agent-blocked");
    let agent = start(&scratch, &server.enroll(OTHER_KEY));

    let refused = server.next("/api/agent/v1/enroll");
    assert_eq!(
        refused.authorization.as_deref(),
        Some(format!("Enroll {OTHER_KEY}").as_str())
    );
    std::fs::write(&agent.config, server.enroll(KEY)).unwrap();
    agent.signal(Signal::SIGHUP);

    let accepted = server.next("/api/agent/v1/enroll");
    assert_eq!(
        accepted.authorization.as_deref(),
        Some(format!("Enroll {KEY}").as_str())
    );
    assert_eq!(server.next("/api/agent/v1/refresh").body["agent_id"], 1);
    drop(agent);
}

#[test]
fn a_host_clock_hours_off_is_corrected_from_the_server() {
    let server = Server::start();
    let scratch = support::ScratchDir::new("agent-clock");
    let server_time = microscope::agent::client::unix_now() + 3 * 3600;
    server.answer_next(
        "/api/agent/v1/enroll",
        StatusCode::UNAUTHORIZED,
        json!({ "error": "clock", "message": "skew", "time": server_time }),
    );
    let agent = start(&scratch, &server.enroll(KEY));

    let skewed = server.next("/api/agent/v1/enroll");
    let corrected = server.next("/api/agent/v1/enroll");
    assert!((skewed.body["time"].as_i64().unwrap() - server_time).abs() > 3600);
    assert!((corrected.body["time"].as_i64().unwrap() - server_time).abs() <= 2);
    assert!(corrected.verified);
    let refresh = server.next("/api/agent/v1/refresh");
    assert!((refresh.body["time"].as_i64().unwrap() - server_time).abs() <= 3);
    drop(agent);
}

#[test]
fn a_bad_config_fails_at_start_with_a_reason() {
    let scratch = support::ScratchDir::new("agent-bad-config");
    let path = scratch.path().join("agent.toml");
    std::fs::write(&path, "enroll = \"http://mse_x@microscope.dev\"\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_microscope"))
        .args(["agent", "run", "--config"])
        .arg(&path)
        .env_remove("MICROSCOPE_ENROLL")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("must start with https://"), "{stderr}");
}
