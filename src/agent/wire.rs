use std::fmt;

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD},
};
use ring::{
    digest::{Context, SHA256, digest},
    signature::{Ed25519KeyPair, KeyPair as _},
};
use serde::{Deserialize, Serialize};

pub const SIGNATURE_HEADER: &str = "microscope-signature";
pub const FLUSH_HEADER: &str = "x-microscope-flush";
pub const ENROLL_PATH: &str = "/api/agent/v1/enroll";
pub const REFRESH_PATH: &str = "/api/agent/v1/refresh";
pub const GOODBYE_PATH: &str = "/api/agent/v1/goodbye";
pub const INGEST_PATH: &str = "/api/agent/v1/ingest";
pub const POLL_PATH: &str = "/api/agent/v1/poll";

const CONTEXT: &[u8] = b"microscope agent request v1";
const ED25519: &str = "ed25519";

pub const ENROLL_KEY_PREFIX: &str = "mse_";
const ENROLL_KEY_BODY: usize = 48;
const ENROLL_KEY_CHECK: usize = 8;
const ENROLL_KEY_LEN: usize = ENROLL_KEY_PREFIX.len() + ENROLL_KEY_BODY + ENROLL_KEY_CHECK;
const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

#[must_use]
pub fn signed_message(origin: &str, path: &str, body: &[u8]) -> Vec<u8> {
    assert!(!origin.contains('\0') && !path.contains('\0'));

    let mut message = Vec::with_capacity(
        ED25519.len() + CONTEXT.len() + origin.len() + path.len() + body.len() + 9,
    );
    message.extend_from_slice(ED25519.as_bytes());
    message.push(0);
    message.extend_from_slice(CONTEXT);
    message.push(0);
    message.extend_from_slice(origin.as_bytes());
    message.push(0);
    message.extend_from_slice(b"POST ");
    message.extend_from_slice(path.as_bytes());
    message.push(0);
    message.extend_from_slice(body);
    message
}

pub struct KeyPair {
    seed: [u8; 32],
    pair: Ed25519KeyPair,
}

impl KeyPair {
    pub fn generate() -> anyhow::Result<Self> {
        let mut seed = [0; 32];
        getrandom::fill(&mut seed)?;
        Self::from_seed(seed).ok_or_else(|| anyhow::anyhow!("ring refused a random Ed25519 seed"))
    }

    #[must_use]
    pub fn from_seed(seed: [u8; 32]) -> Option<Self> {
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed).ok()?;
        Some(Self { seed, pair })
    }

    #[must_use]
    pub const fn seed(&self) -> &[u8; 32] {
        &self.seed
    }

    #[must_use]
    pub fn public_key(&self) -> PublicKey {
        let mut key = [0; 32];
        key.copy_from_slice(self.pair.public_key().as_ref());
        PublicKey(key)
    }

    #[must_use]
    pub fn signature_header(&self, origin: &str, path: &str, body: &[u8]) -> String {
        let signature = self.pair.sign(&signed_message(origin, path, body));
        format!("{ED25519} {}", URL_SAFE_NO_PAD.encode(signature.as_ref()))
    }

    #[must_use]
    pub fn encode(&self) -> String {
        format!("{ED25519} {}\n", URL_SAFE_NO_PAD.encode(self.seed))
    }

    #[must_use]
    pub fn decode(text: &str) -> Option<Self> {
        let encoded = text.trim_end_matches('\n').strip_prefix("ed25519 ")?;
        let seed = URL_SAFE_NO_PAD.decode(encoded).ok()?;
        Self::from_seed(seed.try_into().ok()?)
    }
}

impl fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "key pair {}", self.public_key().fingerprint())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PublicKey([u8; 32]);

impl PublicKey {
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let mut context = Context::new(&SHA256);
        context.update(ED25519.as_bytes());
        context.update(&[0]);
        context.update(&self.0);
        format!("SHA256:{}", STANDARD_NO_PAD.encode(context.finish()))
    }
}

impl Serialize for PublicKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Wire<'a> {
            alg: &'a str,
            key: String,
        }
        Wire {
            alg: ED25519,
            key: URL_SAFE_NO_PAD.encode(self.0),
        }
        .serialize(serializer)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct EnrollKey(String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EnrollKeyError {
    #[error("that is not an enrollment key; they start with mse_ and are 60 characters")]
    NotAKey,
    #[error("the enrollment key is mistyped")]
    Mistyped,
}

impl EnrollKey {
    pub fn parse(text: &str) -> Result<Self, EnrollKeyError> {
        let rest = text
            .strip_prefix(ENROLL_KEY_PREFIX)
            .ok_or(EnrollKeyError::NotAKey)?;
        if text.len() != ENROLL_KEY_LEN || !rest.bytes().all(|byte| BASE32.contains(&byte)) {
            return Err(EnrollKeyError::NotAKey);
        }

        let (body, check) = text.split_at(ENROLL_KEY_PREFIX.len() + ENROLL_KEY_BODY);
        let hash = digest(&SHA256, body.as_bytes());
        if base32_of_five(hash.as_ref().get(..5).unwrap_or_default()) != check {
            return Err(EnrollKeyError::Mistyped);
        }
        Ok(Self(text.to_owned()))
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for EnrollKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("mse_[redacted]")
    }
}

fn base32_of_five(bytes: &[u8]) -> String {
    let bits = bytes
        .iter()
        .fold(0_u64, |bits, byte| bits << 8 | u64::from(*byte));
    (0..8)
        .rev()
        .map(|shift| {
            let index = usize::try_from(bits >> (shift * 5) & 31).unwrap_or_default();
            BASE32.get(index).map_or('?', |byte| char::from(*byte))
        })
        .collect()
}

#[derive(Clone, PartialEq, Eq)]
pub struct Server {
    origin: String,
    loopback: bool,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ServerError {
    #[error("the server URL must start with https://")]
    Scheme,
    #[error("the server URL has no host")]
    NoHost,
    #[error("the server URL must be just a scheme and host, with no path, query, or fragment")]
    Path,
}

impl Server {
    pub fn parse(url: &str) -> Result<(Self, Option<String>), ServerError> {
        let (scheme, rest) = url.split_once("://").ok_or(ServerError::Scheme)?;
        let (authority, path) = rest
            .find(['/', '?', '#'])
            .map_or((rest, ""), |at| rest.split_at(at));
        if !path.is_empty() && path != "/" {
            return Err(ServerError::Path);
        }

        let (key, host) = match authority.rsplit_once('@') {
            Some((key, host)) => (Some(key.to_owned()), host),
            None => (None, authority),
        };
        if host.is_empty() || host.contains(char::is_whitespace) {
            return Err(ServerError::NoHost);
        }

        let name = host
            .strip_prefix('[')
            .and_then(|bracketed| bracketed.split_once(']'))
            .map_or_else(|| host.split(':').next().unwrap_or(host), |(name, _)| name);
        let loopback = name == "localhost"
            || name
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        match scheme {
            "https" => {}
            "http" if loopback => {}
            _ => return Err(ServerError::Scheme),
        }

        let origin = format!("{scheme}://{}", host.to_ascii_lowercase());
        Ok((Self { origin, loopback }, key))
    }

    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }

    #[must_use]
    pub const fn loopback(&self) -> bool {
        self.loopback
    }
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.origin)
    }
}

impl fmt::Display for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.origin)
    }
}

#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        for nibble in [byte >> 4, byte & 15] {
            text.push(char::from(
                DIGITS.get(usize::from(nibble)).copied().unwrap_or(b'0'),
            ));
        }
    }
    text
}

#[must_use]
pub fn inventory_hash(inventory: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(digest(&SHA256, inventory))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviousRun {
    Clean,
    AgentDied,
    HostDied,
}

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub agent_id: i64,
    pub token: String,
    pub expires_in: u32,
    pub refresh_after: u32,
}

#[derive(Debug, Deserialize)]
pub struct IngestResponse {
    pub last_chunk: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_preset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_time_unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_remote_rules: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollRequest {
    #[serde(default)]
    pub known_version: u64,
    #[serde(default)]
    pub last_flush_id: u64,
    #[serde(default)]
    pub empty_flush_id: u64,
    #[serde(default)]
    pub policy_version: u64,
    #[serde(default)]
    pub sanitizer_version: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<AgentLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchDirective {
    pub interval_ms: u64,
    pub lease_ms: u64,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlushDirective {
    pub id: u64,
    pub deadline_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingRuleDirective {
    pub id: i64,
    pub kind: String,
    pub pattern: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectPolicyDirective {
    pub project: String,
    pub preset: String,
    pub time_unit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SanitizerRuleDirective {
    pub name: String,
    pub fields: Vec<String>,
    pub pattern: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SanitizerPolicyDirective {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_builtins: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<SanitizerRuleDirective>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDirective {
    pub version: u64,
    pub sanitizer_version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_unit: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<RecordingRuleDirective>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_policies: Vec<ProjectPolicyDirective>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sanitizer: Option<SanitizerPolicyDirective>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BurstDirective {
    pub id: u64,
    pub duration_ms: u64,
    pub preset: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends_at_ns: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollResponse {
    pub version: u64,
    #[serde(default)]
    pub revoked: bool,
    #[serde(default)]
    pub watch: Option<WatchDirective>,
    #[serde(default)]
    pub flush: Option<FlushDirective>,
    #[serde(default)]
    pub policy: Option<PolicyDirective>,
    #[serde(default)]
    pub burst: Option<BurstDirective>,
    #[serde(default)]
    pub poll_after_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    Signature,
    Clock,
    EnrollKey,
    Quota,
    Revoked,
    Reenroll,
    Inventory,
    Token,
    Throttled,
    Busy,
    Off,
    Internal,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub struct ErrorBody {
    pub error: ErrorCode,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub time: Option<i64>,
}

#[cfg(test)]
mod tests;
