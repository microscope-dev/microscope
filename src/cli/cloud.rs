use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use microscope::agent::wire::{SanitizerPolicyDirective, Server};
use microscope::credentials::{self, Credentials};
use microscope::sanitize::{Action, Builtin, HashKey, Rule, RuleSpec, Sanitizer, sanitize_capture};
use microscope_data::capture::{Capture, file::write as write_capture};
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};

use super::auth::http_agent;

pub const DEFAULT_SERVER: &str = "https://microscope.dev";
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
pub const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Deserialize)]
pub struct WhoamiResponse {
    pub username: String,
    pub email: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrgRef {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiProject {
    pub id: i64,
    pub name: String,
    pub org: Option<OrgRef>,
    pub permission: String,
    #[serde(default)]
    pub is_unassigned: bool,
}

impl ApiProject {
    #[must_use]
    pub fn org_id(&self) -> Option<i64> {
        self.org.as_ref().map(|o| o.id)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceLoginStart {
    pub poll_token: String,
    pub user_code: String,
    pub verify_url: String,
    pub expires_in_secs: u64,
    pub poll_interval_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceLoginPoll {
    pub status: String,
    pub token: Option<String>,
    pub username: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UploadCaptureOptions {
    pub project_id: Option<i64>,
    pub project: Option<String>,
    pub org_id: Option<i64>,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadCaptureResult {
    pub capture_id: i64,
    pub session_id: i64,
    pub url: String,
    pub org_id: i64,
    pub project_id: i64,
    pub project_name: String,
    pub name: String,
    pub byte_len: i64,
    pub expires_at: i64,
    pub redacted_count: u64,
}

#[derive(Serialize)]
struct PolicyRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    org_id: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PolicyResponse {
    project_id: i64,
    project_name: String,
    max_bytes: usize,
    sanitizer: SanitizerPolicyDirective,
}

#[derive(Serialize)]
struct InitRequest<'a> {
    project_id: i64,
    name: &'a str,
    byte_len: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitResponse {
    upload_id: i64,
    upload_url: String,
    #[serde(default, rename = "uploadHeaders")]
    headers: std::collections::BTreeMap<String, String>,
}

#[derive(Serialize)]
struct CommitRequest<'a> {
    upload_id: i64,
    sha256: &'a str,
    name: &'a str,
}

#[must_use]
pub fn default_server_url() -> String {
    std::env::var(credentials::SERVER_ENV)
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_SERVER.to_owned())
}

pub fn resolve_target(
    server_arg: Option<&str>,
    token_arg: Option<&str>,
) -> Result<(Server, String)> {
    let env_server = std::env::var(credentials::SERVER_ENV).ok();
    let env_token = std::env::var(credentials::TOKEN_ENV).ok();
    let saved = credentials::load();
    resolve_target_with(
        server_arg,
        token_arg,
        env_server.as_deref(),
        env_token.as_deref(),
        saved.as_ref(),
    )
}

pub fn resolve_target_with(
    server_arg: Option<&str>,
    token_arg: Option<&str>,
    env_server: Option<&str>,
    env_token: Option<&str>,
    saved_creds: Option<&Credentials>,
) -> Result<(Server, String)> {
    let server_raw = server_arg
        .filter(|v| !v.trim().is_empty())
        .or_else(|| env_server.filter(|v| !v.trim().is_empty()))
        .or_else(|| saved_creds.map(|c| c.server.as_str()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no server URL: pass --server, set MICROSCOPE_SERVER, or run `microscope login`"
            )
        })?;
    let (server, _) = Server::parse(server_raw).map_err(|e| anyhow::anyhow!("{e}"))?;
    let target_origin = credentials::normalize_origin(server_raw);
    let saved_for_origin = saved_creds
        .filter(|c| credentials::normalize_origin(&c.server) == target_origin)
        .map(|c| c.token.as_str());
    let env_token_for_origin = match env_server.filter(|v| !v.trim().is_empty()) {
        Some(env_srv) if credentials::normalize_origin(env_srv) != target_origin => None,
        _ => env_token.filter(|v| !v.trim().is_empty()),
    };

    let token = token_arg
        .filter(|v| !v.trim().is_empty())
        .or(env_token_for_origin)
        .or(saved_for_origin)
        .ok_or_else(|| {
            if env_token.is_some_and(|v| !v.trim().is_empty()) && env_token_for_origin.is_none() {
                anyhow::anyhow!(
                    "MICROSCOPE_TOKEN is paired with a different MICROSCOPE_SERVER; pass --token for {server_raw}"
                )
            } else if saved_creds.is_some() && saved_for_origin.is_none() {
                anyhow::anyhow!(
                    "saved login credentials belong to a different server; run `microscope login --server {server_raw}`"
                )
            } else {
                anyhow::anyhow!(
                    "not signed in: run `microscope login` or pass --token / MICROSCOPE_TOKEN"
                )
            }
        })?;
    Ok((server, token.trim().to_owned()))
}

pub fn fetch_whoami(agent: &ureq::Agent, server: &Server, token: &str) -> Result<WhoamiResponse> {
    let mut resp = agent
        .get(server.url("/api/cli/whoami"))
        .header("authorization", &format!("Bearer {token}"))
        .call()
        .context("could not reach Microscope server")?;
    let status = resp.status().as_u16();
    let text = read_body(&mut resp, 64 * 1024);
    if status != 200 {
        bail!("authentication failed ({status}): {}", extract_error(&text));
    }
    serde_json::from_str(&text).context("unexpected /api/cli/whoami response")
}

pub fn fetch_projects(
    agent: &ureq::Agent,
    server: &Server,
    token: &str,
) -> Result<Vec<ApiProject>> {
    let mut resp = agent
        .get(server.url("/api/projects"))
        .header("authorization", &format!("Bearer {token}"))
        .call()
        .context("could not fetch projects")?;
    let status = resp.status().as_u16();
    let text = read_body(&mut resp, 1024 * 1024);
    if status != 200 {
        bail!(
            "could not list projects ({status}): {}",
            extract_error(&text)
        );
    }
    serde_json::from_str(&text).context("unexpected /api/projects response")
}

pub fn start_device_login(agent: &ureq::Agent, server: &Server) -> Result<DeviceLoginStart> {
    #[derive(Serialize)]
    struct StartReq {
        label: String,
    }
    let hostname = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "workstation".to_owned());
    let body = serde_json::to_vec(&StartReq { label: hostname })?;
    let mut resp = agent
        .post(server.url("/api/cli/login/start"))
        .header("content-type", "application/json")
        .send(&body)
        .context("could not start sign-in")?;
    let status = resp.status().as_u16();
    let text = read_body(&mut resp, 64 * 1024);
    if status != 200 && status != 201 {
        bail!(
            "server refused sign-in ({status}): {}",
            extract_error(&text)
        );
    }
    let mut started: DeviceLoginStart =
        serde_json::from_str(&text).context("unexpected login start response")?;
    if started.verify_url.starts_with('/') {
        started.verify_url = server.url(&started.verify_url);
    }
    Ok(started)
}

pub fn poll_device_login(
    agent: &ureq::Agent,
    server: &Server,
    poll_token: &str,
) -> Result<DeviceLoginPoll> {
    #[derive(Serialize)]
    struct PollReq<'a> {
        poll_token: &'a str,
    }
    let body = serde_json::to_vec(&PollReq { poll_token })?;
    let mut resp = agent
        .post(server.url("/api/cli/login/poll"))
        .header("content-type", "application/json")
        .send(&body)
        .context("could not check sign-in status")?;
    let status = resp.status().as_u16();
    let text = read_body(&mut resp, 64 * 1024);
    if status != 200 {
        bail!("sign-in check failed ({status}): {}", extract_error(&text));
    }
    let polled: DeviceLoginPoll =
        serde_json::from_str(&text).context("unexpected login poll response")?;
    if polled.status == "approved"
        && let Some(token) = polled.token.clone()
    {
        credentials::save(&Credentials {
            server: server.origin().to_owned(),
            token,
            username: polled.username.clone(),
            email: polled.email.clone(),
        })?;
    }
    Ok(polled)
}

pub fn build_sanitizer(policy: &SanitizerPolicyDirective, project_name: &str) -> Result<Sanitizer> {
    let mut builtins: Vec<Builtin> = Builtin::DEFAULT.to_vec();
    for name in &policy.enabled_builtins {
        let b = name.parse::<Builtin>().map_err(|e| {
            anyhow::anyhow!("server sanitizer policy enabled unknown builtin {name:?}: {e}")
        })?;
        if !builtins.contains(&b) {
            builtins.push(b);
        }
    }

    let key_bytes = match policy.hash_key.as_deref() {
        Some(b64) => {
            let decoded = URL_SAFE_NO_PAD
                .decode(b64)
                .context("server sanitizer policy returned invalid base64 hash_key")?;
            anyhow::ensure!(
                !decoded.is_empty(),
                "server sanitizer policy returned empty hash_key"
            );
            decoded
        }
        None => b"microscope-workstation-local".to_vec(),
    };

    let mut custom = Vec::new();
    for rule in &policy.rules {
        let applies = rule
            .project
            .as_deref()
            .is_none_or(|p| p.eq_ignore_ascii_case(project_name));
        if !applies {
            continue;
        }
        let action = Action::parse(&rule.action).ok_or_else(|| {
            anyhow::anyhow!(
                "server sanitizer policy rule {:?} has unsupported action {:?}",
                rule.name,
                rule.action
            )
        })?;
        let compiled = Rule::new(&RuleSpec {
            name: rule.name.clone(),
            fields: rule.fields.clone(),
            pattern: rule.pattern.clone(),
            action,
        })
        .with_context(|| {
            format!(
                "server sanitizer policy rule {:?} failed to compile",
                rule.name
            )
        })?;
        custom.push(compiled);
    }

    Ok(Sanitizer::new(&builtins, custom, HashKey::new(&key_bytes)))
}

#[expect(
    clippy::too_many_lines,
    reason = "sequential 4-step upload protocol (policy, init, PUT, commit)"
)]
pub fn upload_capture(
    server: &Server,
    token: &str,
    ca_file: Option<&Path>,
    capture: Capture,
    opts: &UploadCaptureOptions,
) -> Result<UploadCaptureResult> {
    let control = http_agent(CONTROL_TIMEOUT, ca_file)?;

    let policy_body = serde_json::to_vec(&PolicyRequest {
        project_id: opts.project_id,
        project: opts.project.as_deref(),
        org_id: opts.org_id,
    })?;
    let mut policy_resp = control
        .post(server.url("/api/captures/v1/policy"))
        .header("authorization", &format!("Bearer {token}"))
        .header("content-type", "application/json")
        .send(&policy_body)
        .context("could not fetch project sanitizer policy")?;
    let status = policy_resp.status().as_u16();
    let policy_text = read_body(&mut policy_resp, 256 * 1024);
    if status != 200 {
        bail!(
            "cannot upload to target project ({status}): {}",
            extract_error(&policy_text)
        );
    }
    let policy: PolicyResponse = serde_json::from_str(&policy_text)
        .context("unexpected /api/captures/v1/policy response")?;

    let sanitizer = build_sanitizer(&policy.sanitizer, &policy.project_name)?;
    let cleaned = sanitize_capture(capture, &sanitizer);
    let local_redacted = cleaned.total_fired();

    let mut encoded = Vec::new();
    write_capture(&cleaned.capture, &mut encoded).context("could not encode sanitized capture")?;
    if encoded.len() > policy.max_bytes {
        bail!(
            "capture is {} bytes, exceeding server limit of {} bytes",
            encoded.len(),
            policy.max_bytes
        );
    }

    let sha256 = sha256_hex(&encoded);
    let byte_len = i64::try_from(encoded.len()).context("capture size overflow")?;

    let init_body = serde_json::to_vec(&InitRequest {
        project_id: policy.project_id,
        name: &opts.name,
        byte_len,
    })?;
    let mut init_resp = control
        .post(server.url("/api/captures/v1/init"))
        .header("authorization", &format!("Bearer {token}"))
        .header("content-type", "application/json")
        .send(&init_body)
        .context("could not initialize capture upload")?;
    let status = init_resp.status().as_u16();
    let init_text = read_body(&mut init_resp, 64 * 1024);
    if status != 200 {
        bail!(
            "upload init failed ({status}): {}",
            extract_error(&init_text)
        );
    }
    let init: InitResponse =
        serde_json::from_str(&init_text).context("unexpected /api/captures/v1/init response")?;

    let put_url = if init.upload_url.starts_with('/') {
        server.url(&init.upload_url)
    } else {
        init.upload_url
    };
    let uploader = super::auth::http_agent_for_url(UPLOAD_TIMEOUT, ca_file, &put_url, server)?;
    let has_content_type = init
        .headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case("content-type"));
    let mut put_req = uploader.put(&put_url);
    if !has_content_type {
        put_req = put_req.header("content-type", "application/octet-stream");
    }
    for (k, v) in &init.headers {
        put_req = put_req.header(k.as_str(), v.as_str());
    }
    let mut put_resp = put_req
        .send(&encoded[..])
        .context("could not upload capture bytes")?;
    let put_status = put_resp.status().as_u16();
    if !(200..300).contains(&put_status) {
        let put_text = read_body(&mut put_resp, 64 * 1024);
        bail!(
            "capture upload PUT failed ({put_status}): {}",
            extract_error(&put_text)
        );
    }

    let commit_body = serde_json::to_vec(&CommitRequest {
        upload_id: init.upload_id,
        sha256: &sha256,
        name: &opts.name,
    })?;
    let mut commit_resp = control
        .post(server.url("/api/captures/v1/commit"))
        .header("authorization", &format!("Bearer {token}"))
        .header("content-type", "application/json")
        .send(&commit_body)
        .context("could not commit capture upload")?;
    let status = commit_resp.status().as_u16();
    let commit_text = read_body(&mut commit_resp, 64 * 1024);
    if status != 200 && status != 201 {
        bail!(
            "capture commit failed ({status}): {}",
            extract_error(&commit_text)
        );
    }
    let mut result: UploadCaptureResult = serde_json::from_str(&commit_text)
        .context("unexpected /api/captures/v1/commit response")?;
    if result.url.starts_with('/') {
        result.url = server.url(&result.url);
    }
    result.redacted_count = result.redacted_count.max(local_redacted);
    Ok(result)
}

pub fn post_json_authed(
    agent: &ureq::Agent,
    server: &Server,
    token: &str,
    path: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value> {
    let payload = serde_json::to_vec(body)?;
    let mut resp = agent
        .post(server.url(path))
        .header("authorization", &format!("Bearer {token}"))
        .header("content-type", "application/json")
        .send(&payload)
        .with_context(|| format!("request to {path} failed"))?;
    let status = resp.status().as_u16();
    let text = read_body(&mut resp, 32 * 1024 * 1024);
    if status != 200 {
        bail!("{path} failed ({status}): {}", extract_error(&text));
    }
    serde_json::from_str(&text).with_context(|| format!("unexpected JSON from {path}"))
}

pub fn get_json_authed(
    agent: &ureq::Agent,
    server: &Server,
    token: &str,
    path: &str,
) -> Result<serde_json::Value> {
    let mut resp = agent
        .get(server.url(path))
        .header("authorization", &format!("Bearer {token}"))
        .call()
        .with_context(|| format!("request to {path} failed"))?;
    let status = resp.status().as_u16();
    let text = read_body(&mut resp, 8 * 1024 * 1024);
    if status != 200 {
        bail!("{path} failed ({status}): {}", extract_error(&text));
    }
    serde_json::from_str(&text).with_context(|| format!("unexpected JSON from {path}"))
}

fn read_body(resp: &mut ureq::http::Response<ureq::Body>, limit: u64) -> String {
    resp.body_mut()
        .with_config()
        .limit(limit)
        .read_to_string()
        .unwrap_or_default()
}

pub fn extract_error(text: &str) -> String {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(text)
        && let Some(msg) = val.get("error").and_then(serde_json::Value::as_str)
    {
        return msg.to_owned();
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "empty response".to_owned()
    } else {
        trimmed.chars().take(240).collect()
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let hash = digest(&SHA256, bytes);
    hash.as_ref()
        .iter()
        .fold(String::with_capacity(64), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
}
