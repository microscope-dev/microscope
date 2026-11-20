use std::{
    collections::BTreeMap,
    net::IpAddr,
    path::{Path, PathBuf},
};

use microscope_data::object::TimeUnit;
use ring::digest::{SHA256, digest};
use serde::Deserialize;

use super::wire::{EnrollKey, EnrollKeyError, Server, ServerError};
use crate::profiler::Preset;

pub const DEFAULT_PATH: &str = "/etc/microscope/agent.toml";
pub const DEFAULT_STATE_DIR: &str = "/var/lib/microscope/agent";
const MAX_TAGS: usize = 32;
pub const DEFAULT_CHUNK_MS: u64 = 5_000;
pub const DEFAULT_POST_MS: u64 = 10_000;
pub const DEFAULT_SPOOL_MIB: u32 = 256;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    enroll: Option<String>,
    #[serde(default)]
    tags: BTreeMap<String, String>,
    state_dir: Option<PathBuf>,
    #[serde(rename = "ca_file")]
    certificate_authority: Option<PathBuf>,
    cloud_metadata: Option<bool>,
    report_network: Option<bool>,
    #[serde(default)]
    limits: Limits,
    record: Option<RecordFile>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordFile {
    process: Option<String>,
    service: Option<String>,
    project: Option<String>,
    preset: Option<String>,
    time_unit: Option<String>,
    chunk_ms: Option<u64>,
    post_ms: Option<u64>,
    spool_mib: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordConfig {
    pub process: String,
    pub service: String,
    pub project: String,
    pub preset: Preset,
    pub time_unit: TimeUnit,
    pub chunk_ms: u64,
    pub post_ms: u64,
    pub spool_mib: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub cpu_percent: Option<u8>,
    pub memory_mib: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub path: Option<PathBuf>,
    pub fingerprint: [u8; 32],
    pub server: Server,
    pub enroll_key: EnrollKey,
    pub tags: BTreeMap<String, String>,
    pub state_dir: PathBuf,
    pub ca_file: Option<PathBuf>,
    pub cloud_metadata: bool,
    pub report_network: bool,
    pub node: Option<String>,
    pub node_ips: Vec<IpAddr>,
    pub limits: Limits,
    pub record: Option<RecordConfig>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {error}")]
    Read {
        path: PathBuf,
        error: std::io::Error,
    },
    #[error("{path}: {error}")]
    Parse {
        path: PathBuf,
        error: Box<toml::de::Error>,
    },
    #[error("no server: set `enroll` in {DEFAULT_PATH} or MICROSCOPE_ENROLL")]
    NoServer,
    #[error("the server URL: {0}")]
    Server(#[from] ServerError),
    #[error("no enrollment key: `enroll` is https://mse_…@host, copied from Add machines")]
    NoKey,
    #[error("the enrollment key: {0}")]
    Key(#[from] EnrollKeyError),
    #[error(
        "{name} must be 1 to 64 characters, with no control characters or spaces at either end"
    )]
    Name { name: &'static str },
    #[error("{0}")]
    Tags(&'static str),
    #[error("{name} must be true or false, not {value:?}")]
    Bool { name: &'static str, value: String },
    #[error("MICROSCOPE_NODE_IPS has {0:?}, which is not an IP address")]
    NodeIp(String),
    #[error("limits.cpu_percent must be 1 to 100")]
    Cpu,
    #[error("limits.memory_mib must be 16 to 65536")]
    Memory,
    #[error("record.{name} is invalid: {value:?}")]
    Record { name: &'static str, value: String },
}

pub trait Environment {
    fn get(&self, name: &str) -> Option<String>;
}

pub struct Process;

impl Environment for Process {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|value| !value.is_empty())
    }
}

impl Environment for BTreeMap<&str, &str> {
    fn get(&self, name: &str) -> Option<String> {
        BTreeMap::get(self, name).map(|value| (*value).to_owned())
    }
}

fn read(path: Option<&Path>) -> Result<(Option<PathBuf>, File, [u8; 32]), ConfigError> {
    let (path, required) = path.map_or_else(
        || (PathBuf::from(DEFAULT_PATH), false),
        |path| (path.to_path_buf(), true),
    );
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((None, File::default(), sha256(b"")));
        }
        Err(error) => return Err(ConfigError::Read { path, error }),
    };
    let file = toml::from_str(&text).map_err(|error| ConfigError::Parse {
        path: path.clone(),
        error: Box::new(error),
    })?;
    Ok((Some(path), file, sha256(text.as_bytes())))
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hash = [0; 32];
    hash.copy_from_slice(digest(&SHA256, bytes).as_ref());
    hash
}

pub fn state_dir(path: Option<&Path>, env: &dyn Environment) -> Result<PathBuf, ConfigError> {
    let (_, file, _) = read(path)?;
    Ok(env
        .get("MICROSCOPE_STATE_DIR")
        .map(PathBuf::from)
        .or(file.state_dir)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR)))
}

#[must_use]
pub fn fingerprint(path: Option<&Path>) -> Option<[u8; 32]> {
    read(path).ok().map(|(_, _, fingerprint)| fingerprint)
}

pub fn load(path: Option<&Path>, env: &dyn Environment) -> Result<Config, ConfigError> {
    let (path, file, fingerprint) = read(path)?;

    let enroll = env
        .get("MICROSCOPE_ENROLL")
        .or(file.enroll)
        .ok_or(ConfigError::NoServer)?;
    let (server, key) = Server::parse(&enroll)?;
    let enroll_key = EnrollKey::parse(&key.ok_or(ConfigError::NoKey)?)?;

    let mut tags = file.tags;
    if let Some(text) = env.get("MICROSCOPE_TAGS") {
        tags.extend(parse_tags(&text)?);
    }
    check_tags(&tags)?;

    let node = name("MICROSCOPE_NODE_NAME", env.get("MICROSCOPE_NODE_NAME"))?;
    let node_ips = env
        .get("MICROSCOPE_NODE_IPS")
        .map(|text| {
            text.split(',')
                .map(str::trim)
                .filter(|ip| !ip.is_empty())
                .map(|ip| ip.parse().map_err(|_| ConfigError::NodeIp(ip.to_owned())))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();

    let limits = file.limits;
    if limits
        .cpu_percent
        .is_some_and(|cpu| !(1..=100).contains(&cpu))
    {
        return Err(ConfigError::Cpu);
    }
    if limits
        .memory_mib
        .is_some_and(|memory| !(16..=65536).contains(&memory))
    {
        return Err(ConfigError::Memory);
    }

    let record = load_record(file.record, env)?;

    Ok(Config {
        path,
        fingerprint,
        server,
        enroll_key,
        tags,
        state_dir: env
            .get("MICROSCOPE_STATE_DIR")
            .map(PathBuf::from)
            .or(file.state_dir)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR)),
        ca_file: env
            .get("MICROSCOPE_CA_FILE")
            .map(PathBuf::from)
            .or(file.certificate_authority),
        cloud_metadata: boolean(env, "MICROSCOPE_CLOUD_METADATA", file.cloud_metadata)?,
        report_network: boolean(env, "MICROSCOPE_REPORT_NETWORK", file.report_network)?,
        node,
        node_ips,
        limits,
        record,
    })
}

fn load_record(
    file: Option<RecordFile>,
    env: &dyn Environment,
) -> Result<Option<RecordConfig>, ConfigError> {
    let file = file.unwrap_or_default();
    let process = name(
        "record.process",
        env.get("MICROSCOPE_RECORD_PROCESS").or(file.process),
    )?;
    let Some(process) = process else {
        return Ok(None);
    };
    let service = name(
        "record.service",
        env.get("MICROSCOPE_RECORD_SERVICE").or(file.service),
    )?
    .unwrap_or_else(|| process.clone());
    let project = name(
        "record.project",
        env.get("MICROSCOPE_RECORD_PROJECT").or(file.project),
    )?
    .unwrap_or_else(|| "default".to_owned());

    let preset = match env
        .get("MICROSCOPE_RECORD_PRESET")
        .or(file.preset)
        .as_deref()
    {
        None | Some("prod" | "production") => Preset::Production,
        Some("dev" | "development") => Preset::Development,
        Some("max" | "maximum") => Preset::Maximum,
        Some(other) => {
            return Err(ConfigError::Record {
                name: "preset",
                value: other.to_owned(),
            });
        }
    };

    let time_unit = match env
        .get("MICROSCOPE_RECORD_TIME_UNIT")
        .or(file.time_unit)
        .as_deref()
    {
        None | Some("1us" | "us") => TimeUnit::Us,
        Some("1ns" | "ns") => TimeUnit::Ns,
        Some("1ms" | "ms") => TimeUnit::Ms,
        Some(other) => {
            return Err(ConfigError::Record {
                name: "time_unit",
                value: other.to_owned(),
            });
        }
    };

    let chunk_ms = parse_u64_range(
        env.get("MICROSCOPE_RECORD_CHUNK_MS"),
        file.chunk_ms,
        DEFAULT_CHUNK_MS,
        200..=60_000,
        "chunk_ms",
    )?;
    let post_ms = parse_u64_range(
        env.get("MICROSCOPE_RECORD_POST_MS"),
        file.post_ms,
        DEFAULT_POST_MS,
        200..=300_000,
        "post_ms",
    )?;
    let spool_mib = u32::try_from(parse_u64_range(
        env.get("MICROSCOPE_RECORD_SPOOL_MIB"),
        file.spool_mib.map(u64::from),
        u64::from(DEFAULT_SPOOL_MIB),
        1..=65_536,
        "spool_mib",
    )?)
    .unwrap_or(DEFAULT_SPOOL_MIB);

    Ok(Some(RecordConfig {
        process,
        service,
        project,
        preset,
        time_unit,
        chunk_ms,
        post_ms,
        spool_mib,
    }))
}

fn parse_u64_range(
    env_val: Option<String>,
    file_val: Option<u64>,
    default: u64,
    range: std::ops::RangeInclusive<u64>,
    field: &'static str,
) -> Result<u64, ConfigError> {
    let val = match env_val {
        Some(text) => text.parse::<u64>().map_err(|_| ConfigError::Record {
            name: field,
            value: text,
        })?,
        None => file_val.unwrap_or(default),
    };
    if !range.contains(&val) {
        return Err(ConfigError::Record {
            name: field,
            value: val.to_string(),
        });
    }
    Ok(val)
}

fn name(name: &'static str, value: Option<String>) -> Result<Option<String>, ConfigError> {
    match value {
        None => Ok(None),
        Some(value)
            if (1..=64).contains(&value.chars().count())
                && !value.chars().any(char::is_control)
                && value.trim() == value =>
        {
            Ok(Some(value))
        }
        Some(_) => Err(ConfigError::Name { name }),
    }
}

fn boolean(
    env: &dyn Environment,
    name: &'static str,
    file: Option<bool>,
) -> Result<bool, ConfigError> {
    match env.get(name).as_deref() {
        None => Ok(file.unwrap_or(true)),
        Some("true" | "1") => Ok(true),
        Some("false" | "0") => Ok(false),
        Some(value) => Err(ConfigError::Bool {
            name,
            value: value.to_owned(),
        }),
    }
}

fn parse_tags(text: &str) -> Result<BTreeMap<String, String>, ConfigError> {
    text.split(',')
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            pair.split_once('=')
                .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
                .ok_or(ConfigError::Tags(
                    "MICROSCOPE_TAGS is key=value pairs split by commas",
                ))
        })
        .collect()
}

fn check_tags(tags: &BTreeMap<String, String>) -> Result<(), ConfigError> {
    if tags.len() > MAX_TAGS {
        return Err(ConfigError::Tags("at most 32 tags"));
    }
    for (key, value) in tags {
        if key == "microscope.project" {
            return Err(ConfigError::Tags(
                "tag key \"microscope.project\" is reserved; configure record.project instead",
            ));
        }
        let key_ok = (1..=63).contains(&key.len())
            && key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'));
        if !key_ok {
            return Err(ConfigError::Tags(
                "tag keys are 1 to 63 letters, digits, or . _ - /",
            ));
        }

        let value_ok = (1..=256).contains(&value.chars().count())
            && !value.chars().any(|c| c.is_control() || c == ',');
        if !value_ok {
            return Err(ConfigError::Tags(
                "tag values are 1 to 256 characters with no control characters or commas",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
