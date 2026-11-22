use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const CREDENTIALS_ENV: &str = "MICROSCOPE_CREDENTIALS_FILE";
pub const SERVER_ENV: &str = "MICROSCOPE_SERVER";
pub const TOKEN_ENV: &str = "MICROSCOPE_TOKEN";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    pub server: String,
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

#[must_use]
pub fn normalize_origin(url: &str) -> String {
    url.trim().trim_end_matches('/').to_ascii_lowercase()
}

#[must_use]
pub fn credentials_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(CREDENTIALS_ENV).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("microscope").join("credentials.json"))
}

#[must_use]
pub fn load() -> Option<Credentials> {
    if let (Ok(server), Ok(token)) = (std::env::var(SERVER_ENV), std::env::var(TOKEN_ENV)) {
        let server = server.trim();
        let token = token.trim();
        if !server.is_empty() && !token.is_empty() {
            return Some(Credentials {
                server: server.trim_end_matches('/').to_owned(),
                token: token.to_owned(),
                username: None,
                email: None,
            });
        }
    }
    let path = credentials_path()?;
    load_from(&path)
}

#[must_use]
pub fn load_from(path: &Path) -> Option<Credentials> {
    let text = fs::read_to_string(path).ok()?;
    let mut creds: Credentials = serde_json::from_str(&text).ok()?;
    creds.server = creds.server.trim().trim_end_matches('/').to_owned();
    creds.token = creds.token.trim().to_owned();
    if creds.server.is_empty() || creds.token.is_empty() {
        return None;
    }
    Some(creds)
}

pub fn save(creds: &Credentials) -> anyhow::Result<PathBuf> {
    let path = credentials_path()
        .ok_or_else(|| anyhow::anyhow!("cannot determine credentials file location"))?;
    save_to(&path, creds)?;
    Ok(path)
}

pub fn save_to(path: &Path, creds: &Credentials) -> anyhow::Result<()> {
    let Some(dir) = path.parent() else {
        anyhow::bail!("credentials path has no parent directory");
    };
    fs::create_dir_all(dir)?;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    let payload = serde_json::to_vec_pretty(creds)?;
    let tmp = dir.join(format!(".credentials.{}.tmp", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&payload)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn clear() -> anyhow::Result<bool> {
    let Some(path) = credentials_path() else {
        return Ok(false);
    };
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

#[must_use]
pub fn bearer_for_server(server_url: &str) -> Option<String> {
    let creds = load()?;
    if normalize_origin(server_url) == normalize_origin(&creds.server) {
        Some(creds.token)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_roundtrip_and_origin_matching() {
        let dir = std::env::temp_dir().join(format!("microscope-creds-{}", std::process::id()));
        let path = dir.join("credentials.json");
        let creds = Credentials {
            server: "https://App.Microscope.dev/".to_owned(),
            token: "msu_testtoken".to_owned(),
            username: Some("alice".to_owned()),
            email: Some("alice@example.com".to_owned()),
        };
        save_to(&path, &creds).unwrap();
        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded.server, "https://App.Microscope.dev");
        assert_eq!(loaded.token, "msu_testtoken");
        assert_eq!(
            normalize_origin("https://app.microscope.dev/"),
            normalize_origin(&loaded.server)
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
