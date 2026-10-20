//! Where the agent keeps its settings, token, and outbox.
//!
//! The settings are the user's own file, `agent.toml` beside `config.toml`.
//! They are not in `config.toml`, whose project files a repository can ship,
//! since a project must never choose where a user's recordings go.

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::cli::config;

/// `agent.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Settings {
    /// The service's URL, such as `https://example.com/`.
    pub server: Option<String>,

    /// Whether the agent runs the commands the service sends, such as
    /// recording a process.
    pub allow_remote: bool,

    /// Directories of `record --continuous` segments to upload.
    pub watch: Vec<PathBuf>,
}

/// The agent's files.
#[derive(Clone, Debug)]
pub struct Paths {
    pub settings: PathBuf,
    pub token: PathBuf,
    pub outbox: PathBuf,
    /// What the agent remembers about each watched directory.
    pub watched: PathBuf,
}

impl Paths {
    pub fn locate() -> Result<Self> {
        let config = config::user_directory()
            .context("neither XDG_CONFIG_HOME nor HOME is set, so there is no agent.toml")?;
        let state = config::state_directory()
            .context("neither XDG_STATE_HOME nor HOME is set, so there is no outbox")?;
        Ok(Self {
            settings: config.join("agent.toml"),
            token: config.join("agent-token"),
            outbox: state.join("outbox"),
            watched: state.join("watched"),
        })
    }
}

impl Settings {
    /// Reads `path`, or the defaults if there is no such file.
    pub fn read(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("reading {}", path.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = toml::to_string(self).context("writing agent.toml")?;
        config::write_atomically(path, &text).map_err(anyhow::Error::msg)
    }
}

/// The WebSocket URL of the agent endpoint of the service at `server`: an
/// `http` URL becomes `ws`, and `https` becomes `wss`.
pub fn endpoint(server: &str) -> Result<String> {
    let (scheme, rest) = server
        .split_once("://")
        .with_context(|| format!("{server} is not a URL, such as https://example.com/"))?;
    let scheme = match scheme {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        _ => anyhow::bail!("{server} is not an http or https URL"),
    };
    anyhow::ensure!(
        !rest.is_empty() && !rest.starts_with('/') && !rest.contains(['?', '#']),
        "{server} is not a service's URL, such as https://example.com/"
    );
    let base = rest.trim_end_matches('/');
    Ok(format!("{scheme}://{base}/v1/agent"))
}

/// Whether connecting to `endpoint` would send the device token
/// unencrypted to another machine.
pub fn exposes_token(endpoint: &str) -> bool {
    let Some(rest) = endpoint.strip_prefix("ws://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let host = authority
        .rsplit_once(':')
        .filter(|(_, port)| port.bytes().all(|byte| byte.is_ascii_digit()))
        .map_or(authority, |(host, _)| host);
    !matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// Reads the device token `login` wrote.
pub fn read_token(path: &Path) -> Result<String> {
    let token = match std::fs::read_to_string(path) {
        Ok(token) => token,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("this machine has no device token; run `microscope agent login`")
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let mode = std::fs::metadata(path)
        .with_context(|| format!("reading {}", path.display()))?
        .permissions()
        .mode();
    let shared = mode & 0o077;
    anyhow::ensure!(
        shared == 0,
        "{} may be read by other users; make it private with `chmod 600 {}`",
        path.display(),
        path.display()
    );
    Ok(token.trim().to_owned())
}

/// Checks that `token` looks like a device token, without saying whether
/// the service accepts it.
pub fn check_token(token: &str) -> Result<()> {
    anyhow::ensure!(!token.is_empty(), "the device token is empty");
    anyhow::ensure!(
        token.bytes().all(|byte| byte.is_ascii_graphic()),
        "a device token is one word of printable characters"
    );
    Ok(())
}

/// Writes `token` to `path`, readable by the user alone.
pub fn write_token(path: &Path, token: &str) -> Result<()> {
    let parent = path.parent().context("the token file has no directory")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;

    // The partial file is private from its creation, so the token is never
    // readable by others, even for a moment.
    let partial = parent.join(".agent-token.partial");
    let _ = std::fs::remove_file(&partial);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&partial)
        .with_context(|| format!("creating {}", partial.display()))?;
    writeln!(file, "{token}").with_context(|| format!("writing {}", partial.display()))?;
    file.sync_all()
        .with_context(|| format!("writing {}", partial.display()))?;
    std::fs::rename(&partial, path).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_follow_the_servers_scheme() {
        for (server, want) in [
            ("http://localhost:8080/", "ws://localhost:8080/v1/agent"),
            ("http://localhost:8080", "ws://localhost:8080/v1/agent"),
            ("https://example.com/", "wss://example.com/v1/agent"),
            (
                "https://example.com/base/",
                "wss://example.com/base/v1/agent",
            ),
            ("wss://example.com", "wss://example.com/v1/agent"),
        ] {
            assert_eq!(endpoint(server).unwrap(), want, "{server}");
        }
    }

    #[test]
    fn other_servers_are_refused() {
        for server in [
            "example.com",
            "ftp://example.com/",
            "https://",
            "https:///path",
            "https://example.com/?x=1",
        ] {
            assert!(endpoint(server).is_err(), "{server}");
        }
    }

    #[test]
    fn only_plain_connections_to_other_machines_expose_the_token() {
        for (endpoint, exposed) in [
            ("ws://localhost:8080/v1/agent", false),
            ("ws://127.0.0.1/v1/agent", false),
            ("ws://[::1]:8080/v1/agent", false),
            ("wss://example.com/v1/agent", false),
            ("ws://example.com/v1/agent", true),
            ("ws://10.0.0.2:8080/v1/agent", true),
        ] {
            assert_eq!(exposes_token(endpoint), exposed, "{endpoint}");
        }
    }

    #[test]
    fn tokens_are_one_printable_word() {
        assert!(check_token("msd_abc-DEF_123").is_ok());
        for token in ["", "two words", "tab\there", "caf\u{e9}"] {
            assert!(check_token(token).is_err(), "{token:?}");
        }
    }

    #[test]
    fn settings_read_their_defaults_when_missing() {
        let dir = std::env::temp_dir().join(format!("microscope-settings-{}", std::process::id()));
        let settings = Settings::read(&dir.join("agent.toml")).unwrap();
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn settings_refuse_unknown_keys() {
        assert!(toml::from_str::<Settings>("servr = \"x\"").is_err());
        let settings: Settings =
            toml::from_str("server = \"http://h/\"\nallow-remote = true\nwatch = [\"/d\"]")
                .unwrap();
        assert!(settings.allow_remote);
        assert_eq!(settings.watch, [PathBuf::from("/d")]);
    }
}
