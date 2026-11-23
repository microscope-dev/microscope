use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result};
use microscope_data::sanitize::EncodedPost;

use super::{
    machine::Outcome,
    wire::{
        ENROLL_PATH, EnrollKey, ErrorBody, ErrorCode, FLUSH_HEADER, GOODBYE_PATH, INGEST_PATH,
        INGEST_SYMBOLS_PATH, IngestResponse, KeyPair, POLL_PATH, PollRequest, PollResponse,
        PreviousRun, REFRESH_PATH, SIGNATURE_HEADER, SYMBOL_BUILD_ID_HEADER,
        SYMBOL_ENCODING_HEADER, SYMBOL_PART_HEADER, SYMBOL_PROJECT_HEADER,
        SYMBOL_PROJECT_ID_HEADER, SYMBOL_SERVICE_HEADER, SYMBOL_SHA256_HEADER,
        SYMBOL_TOTAL_BYTES_HEADER, SYMBOL_TOTAL_PARTS_HEADER, SYMBOL_UPLOAD_ID_HEADER, Server,
        SymbolUploadResponse, TokenResponse, inventory_hash,
    },
};

pub const TIMEOUT: Duration = Duration::from_secs(5);
pub const INGEST_TIMEOUT: Duration = Duration::from_secs(15);
pub const POLL_TIMEOUT: Duration = Duration::from_secs(65);
pub const GOODBYE_TIMEOUT: Duration = Duration::from_secs(2);
const BODY_LIMIT: u64 = 64 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum PollReply {
    Directive(Box<PollResponse>),
    Unauthorized,
    Refused {
        status: u16,
        retry_after: Option<Duration>,
        message: String,
    },
    Unreachable(String),
}

#[derive(serde::Deserialize)]
struct IngestErrorBody {
    error: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum IngestReply {
    Accepted {
        last_chunk: u64,
        project_id: Option<i64>,
        missing_symbols: Vec<String>,
    },
    Overlap {
        last_chunk: u64,
    },
    Unauthorized,
    Refused {
        status: u16,
        error: Option<String>,
        retry_after: Option<Duration>,
        message: String,
    },
    Unreachable(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum SymbolPartReply {
    Continue { upload_id: i64, next_part: u32 },
    Complete,
    AlreadyPresent,
    Unauthorized,
    Refused { status: u16, message: String },
    Unreachable(String),
}

pub struct SymbolPartHeader<'a> {
    pub build_id: &'a str,
    pub part: u32,
    pub total_parts: u32,
    pub total_bytes: u64,
    pub sha256: &'a str,
    pub encoding: &'a str,
    pub upload_id: Option<i64>,
    pub project_id: Option<i64>,
    pub project: Option<&'a str>,
    pub service: Option<&'a str>,
}

#[derive(Debug)]
pub enum Reply {
    Token(TokenResponse),
    Done,
    Refused {
        status: u16,
        error: Option<ErrorBody>,
        retry_after: Option<Duration>,
    },
    Unreachable(String),
}

impl Reply {
    const fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Refused {
                error: Some(body), ..
            } => Some(body.error),
            _ => None,
        }
    }
}

#[must_use]
pub fn outcome(reply: &Reply) -> (Outcome, Option<String>) {
    match reply {
        Reply::Token(token) => (
            Outcome::Token {
                agent_id: token.agent_id,
                refresh_after: Duration::from_secs(u64::from(token.refresh_after)),
            },
            None,
        ),
        Reply::Done => (Outcome::Retry { retry_after: None }, None),
        Reply::Unreachable(error) => (
            Outcome::Retry { retry_after: None },
            Some(format!("cannot reach the server: {error}")),
        ),
        Reply::Refused {
            status,
            error,
            retry_after,
        } => {
            let message = error.as_ref().map_or_else(
                || format!("the server answered {status}"),
                |body| format!("the server answered {status}: {}", body.message),
            );
            let outcome = match error.as_ref().map(|body| body.error) {
                Some(ErrorCode::EnrollKey) => Outcome::EnrollKey,
                Some(ErrorCode::Revoked) => Outcome::Revoked,
                Some(ErrorCode::Reenroll) => Outcome::Reenroll,
                _ => Outcome::Retry {
                    retry_after: *retry_after,
                },
            };
            (outcome, Some(message))
        }
    }
}

#[must_use]
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

pub struct Client {
    agent: ureq::Agent,
    ingest: ureq::Agent,
    poll: ureq::Agent,
    goodbye: ureq::Agent,
    server: Server,
    offset: i64,
}

fn agent(timeout: Duration, ca_file: Option<&Path>) -> Result<ureq::Agent> {
    let mut config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .max_redirects(0)
        .user_agent(format!(
            "microscope-agent/{}",
            super::inventory::AGENT_VERSION
        ));
    if let Some(path) = ca_file {
        let pem = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let certs = ureq::tls::parse_pem(&pem)
            .filter_map(|item| match item {
                Ok(ureq::tls::PemItem::Certificate(cert)) => Some(Ok(cert.to_owned())),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("{} is not PEM certificates", path.display()))?;
        anyhow::ensure!(
            !certs.is_empty(),
            "{} holds no certificates",
            path.display()
        );
        config = config.tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::new_with_certs(&certs))
                .build(),
        );
    }
    Ok(config.build().new_agent())
}

fn json<T: serde::Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

impl Client {
    pub fn new(server: Server, ca_file: Option<&Path>) -> Result<Self> {
        Ok(Self {
            agent: agent(TIMEOUT, ca_file)?,
            ingest: agent(INGEST_TIMEOUT, ca_file)?,
            poll: agent(POLL_TIMEOUT, ca_file)?,
            goodbye: agent(GOODBYE_TIMEOUT, ca_file)?,
            server,
            offset: 0,
        })
    }

    pub fn poll(&self, token: &str, request: &PollRequest) -> PollReply {
        let authorization = format!("Bearer {token}");
        let body = json(request);
        let mut response = match self
            .poll
            .post(self.server.url(POLL_PATH))
            .header("content-type", "application/json")
            .header("authorization", &authorization)
            .send(body.as_bytes())
        {
            Ok(response) => response,
            Err(error) => return PollReply::Unreachable(error.to_string()),
        };

        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse().ok())
            .map(Duration::from_secs);
        let text = response
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_string()
            .unwrap_or_default();

        match status {
            200 => serde_json::from_str::<PollResponse>(&text).map_or_else(
                |error| PollReply::Unreachable(format!("bad poll answer: {error}")),
                |resp| PollReply::Directive(Box::new(resp)),
            ),
            401 => PollReply::Unauthorized,
            _ => PollReply::Refused {
                status,
                retry_after,
                message: text,
            },
        }
    }

    #[must_use]
    pub fn ingest(&self, token: &str, post: &EncodedPost) -> IngestReply {
        self.ingest_with_flush(token, post, None)
    }

    pub fn ingest_with_flush(
        &self,
        token: &str,
        post: &EncodedPost,
        flush_id: Option<u64>,
    ) -> IngestReply {
        let authorization = format!("Bearer {token}");
        let mut req = self
            .ingest
            .post(self.server.url(INGEST_PATH))
            .header("content-type", "application/octet-stream")
            .header("authorization", &authorization);
        if let Some(id) = flush_id {
            req = req.header(FLUSH_HEADER, id.to_string());
        }
        let mut response = match req.send(post.as_bytes()) {
            Ok(response) => response,
            Err(error) => return IngestReply::Unreachable(error.to_string()),
        };

        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse().ok())
            .map(Duration::from_secs);
        let text = response
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_string()
            .unwrap_or_default();

        let error = serde_json::from_str::<IngestErrorBody>(&text)
            .ok()
            .map(|body| body.error);

        match status {
            200 | 201 => serde_json::from_str::<IngestResponse>(&text).map_or_else(
                |error| IngestReply::Unreachable(format!("bad ingest answer: {error}")),
                |body| IngestReply::Accepted {
                    last_chunk: body.last_chunk,
                    project_id: body.project_id,
                    missing_symbols: body.missing_symbols,
                },
            ),
            409 => serde_json::from_str::<IngestResponse>(&text).map_or_else(
                |_| IngestReply::Refused {
                    status,
                    error,
                    retry_after,
                    message: text,
                },
                |body| IngestReply::Overlap {
                    last_chunk: body.last_chunk,
                },
            ),
            401 => IngestReply::Unauthorized,
            _ => IngestReply::Refused {
                status,
                error,
                retry_after,
                message: text,
            },
        }
    }

    #[must_use]
    pub fn upload_symbol_part(
        &self,
        token: &str,
        header: &SymbolPartHeader<'_>,
        piece: &[u8],
    ) -> SymbolPartReply {
        let authorization = format!("Bearer {token}");
        let mut req = self
            .ingest
            .post(self.server.url(INGEST_SYMBOLS_PATH))
            .header("content-type", "application/octet-stream")
            .header("authorization", &authorization)
            .header(SYMBOL_BUILD_ID_HEADER, header.build_id)
            .header(SYMBOL_PART_HEADER, header.part.to_string())
            .header(SYMBOL_TOTAL_PARTS_HEADER, header.total_parts.to_string())
            .header(SYMBOL_TOTAL_BYTES_HEADER, header.total_bytes.to_string())
            .header(SYMBOL_SHA256_HEADER, header.sha256)
            .header(SYMBOL_ENCODING_HEADER, header.encoding);
        if let Some(id) = header.upload_id {
            req = req.header(SYMBOL_UPLOAD_ID_HEADER, id.to_string());
        }
        if let Some(pid) = header.project_id {
            req = req.header(SYMBOL_PROJECT_ID_HEADER, pid.to_string());
        }
        if let Some(project) = header.project.filter(|p| !p.is_empty()) {
            req = req.header(SYMBOL_PROJECT_HEADER, project);
        }
        if let Some(service) = header.service.filter(|s| !s.is_empty()) {
            req = req.header(SYMBOL_SERVICE_HEADER, service);
        }

        let mut response = match req.send(piece) {
            Ok(resp) => resp,
            Err(error) => return SymbolPartReply::Unreachable(error.to_string()),
        };
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_string()
            .unwrap_or_default();

        match status {
            200 | 201 => serde_json::from_str::<SymbolUploadResponse>(&text).map_or_else(
                |e| SymbolPartReply::Unreachable(format!("bad symbol answer: {e}")),
                |body| match body.status.as_str() {
                    "complete" => SymbolPartReply::Complete,
                    "already_present" => SymbolPartReply::AlreadyPresent,
                    "continue" => SymbolPartReply::Continue {
                        upload_id: body.upload_id.unwrap_or(0),
                        next_part: body.next_part.unwrap_or(header.part + 1),
                    },
                    _ => SymbolPartReply::Refused {
                        status,
                        message: text,
                    },
                },
            ),
            401 => SymbolPartReply::Unauthorized,
            _ => SymbolPartReply::Refused {
                status,
                message: text,
            },
        }
    }

    #[must_use]
    pub const fn server(&self) -> &Server {
        &self.server
    }

    #[must_use]
    pub fn now(&self) -> i64 {
        unix_now().saturating_add(self.offset)
    }

    fn post(
        &self,
        agent: &ureq::Agent,
        path: &str,
        body: &[u8],
        key: Option<&KeyPair>,
        authorization: Option<&str>,
    ) -> Reply {
        let mut request = agent
            .post(self.server.url(path))
            .header("content-type", "application/json");
        if let Some(key) = key {
            request = request.header(
                SIGNATURE_HEADER,
                key.signature_header(self.server.origin(), path, body),
            );
        }
        if let Some(authorization) = authorization {
            request = request.header("authorization", authorization);
        }

        let mut response = match request.send(body) {
            Ok(response) => response,
            Err(error) => return Reply::Unreachable(error.to_string()),
        };
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse().ok())
            .map(Duration::from_secs);
        let text = response
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_string()
            .unwrap_or_default();

        match status {
            200 | 201 => serde_json::from_str(&text).map_or_else(
                |error| Reply::Unreachable(format!("a bad answer: {error}")),
                Reply::Token,
            ),
            204 => Reply::Done,
            _ => Reply::Refused {
                status,
                error: serde_json::from_str(&text).ok(),
                retry_after,
            },
        }
    }

    fn adjusted_clock(&mut self, reply: &Reply) -> bool {
        let Reply::Refused {
            error:
                Some(ErrorBody {
                    error: ErrorCode::Clock,
                    time: Some(server_time),
                    ..
                }),
            ..
        } = reply
        else {
            return false;
        };
        let offset = server_time.saturating_sub(unix_now());
        if offset == self.offset {
            return false;
        }
        self.offset = offset;
        true
    }

    #[must_use]
    pub const fn clock_offset(&self) -> i64 {
        self.offset
    }

    pub fn enroll(
        &mut self,
        key: &KeyPair,
        enroll_key: &EnrollKey,
        boot_id: &str,
        inventory: &[u8],
        previous: Option<i64>,
    ) -> Reply {
        let authorization = format!("Enroll {}", enroll_key.expose());
        let previous = previous.map_or_else(String::new, |agent_id| {
            format!(r#","previous":{{"agent_id":{agent_id},"reason":"duplicate"}}"#)
        });
        let mut clock_retried = false;
        loop {
            let body = format!(
                r#"{{"public_key":{},"time":{},"boot_id":{},"inventory_hash":{},"inventory":{}{previous}}}"#,
                json(&key.public_key()),
                self.now(),
                json(boot_id),
                json(&inventory_hash(inventory)),
                String::from_utf8_lossy(inventory),
            );
            let reply = self.post(
                &self.agent,
                ENROLL_PATH,
                body.as_bytes(),
                Some(key),
                Some(&authorization),
            );
            if !clock_retried && self.adjusted_clock(&reply) {
                clock_retried = true;
                continue;
            }
            return reply;
        }
    }

    pub fn refresh(
        &mut self,
        key: &KeyPair,
        agent_id: i64,
        boot_id: &str,
        inventory: &[u8],
        previous_run: Option<PreviousRun>,
    ) -> Reply {
        let mut with_inventory = false;
        let mut clock_retried = false;
        loop {
            let previous_run = previous_run.map_or_else(String::new, |run| {
                format!(r#","previous_run":{}"#, json(&run))
            });
            let inventory_field = if with_inventory {
                format!(r#","inventory":{}"#, String::from_utf8_lossy(inventory))
            } else {
                String::new()
            };
            let body = format!(
                r#"{{"agent_id":{agent_id},"time":{},"boot_id":{},"inventory_hash":{}{inventory_field}{previous_run}}}"#,
                self.now(),
                json(boot_id),
                json(&inventory_hash(inventory)),
            );
            let reply = self.post(&self.agent, REFRESH_PATH, body.as_bytes(), Some(key), None);

            if reply.code() == Some(ErrorCode::Inventory) && !with_inventory {
                with_inventory = true;
                continue;
            }
            if !clock_retried && self.adjusted_clock(&reply) {
                clock_retried = true;
                continue;
            }
            return reply;
        }
    }

    #[must_use]
    pub fn goodbye(&self, token: &str) -> Reply {
        self.post(
            &self.goodbye,
            GOODBYE_PATH,
            b"",
            None,
            Some(&format!("Bearer {token}")),
        )
    }
}

#[cfg(test)]
mod tests;
