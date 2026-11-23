use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use clap::Args;
use microscope::agent::wire::Server;
use microscope::credentials::{self, Credentials};
use serde::{Deserialize, Serialize};

const LOGIN_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_SERVER: &str = "https://microscope.dev";

#[derive(Args)]
pub struct LoginArgs {
    /// Microscope server URL (defaults to `MICROSCOPE_SERVER` or <https://microscope.dev>).
    #[arg(long, value_name = "URL")]
    pub server: Option<String>,

    /// Sign in directly with a personal API token (msu_...) instead of opening a browser.
    #[arg(long, value_name = "TOKEN")]
    pub token: Option<String>,

    /// Do not attempt to open a browser automatically.
    #[arg(long)]
    pub no_browser: bool,

    /// Extra PEM root certificates for the server.
    #[arg(long, value_name = "FILE")]
    pub ca_file: Option<PathBuf>,
}

#[derive(Args)]
pub struct LogoutArgs {
    /// Extra PEM root certificates for the server.
    #[arg(long, value_name = "FILE")]
    pub ca_file: Option<PathBuf>,
}

#[derive(Serialize)]
struct StartRequest {
    label: String,
}

#[derive(Deserialize)]
struct StartResponse {
    poll_token: String,
    user_code: String,
    verify_url: String,
    expires_in_secs: u64,
    poll_interval_ms: u64,
}

#[derive(Serialize)]
struct PollRequest<'a> {
    poll_token: &'a str,
}

#[derive(Deserialize)]
struct PollResponse {
    status: String,
    token: Option<String>,
    username: Option<String>,
    email: Option<String>,
}

#[derive(Deserialize)]
struct MeResponse {
    username: String,
    email: String,
}

pub fn run_login(args: &LoginArgs) -> ExitCode {
    match login(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

pub fn run_logout(args: &LogoutArgs) -> ExitCode {
    match logout(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

pub fn run_whoami() -> ExitCode {
    if let Some(creds) = credentials::load() {
        match (&creds.username, &creds.email) {
            (Some(user), Some(email)) => {
                println!("{user} <{email}> ({})", creds.server);
            }
            (Some(user), None) => println!("{user} ({})", creds.server),
            _ => println!("authenticated ({})", creds.server),
        }
        ExitCode::SUCCESS
    } else {
        eprintln!("not logged in; run `microscope login`");
        ExitCode::FAILURE
    }
}

pub fn http_agent(timeout: Duration, ca_file: Option<&Path>) -> Result<ureq::Agent> {
    let mut builder = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .max_redirects(0)
        .user_agent(format!("microscope-cli/{}", env!("CARGO_PKG_VERSION")));
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
        builder = builder.tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::new_with_certs(&certs))
                .build(),
        );
    }
    Ok(builder.build().new_agent())
}

pub fn http_agent_for_url(
    timeout: Duration,
    ca_file: Option<&Path>,
    url: &str,
    server: &Server,
) -> Result<ureq::Agent> {
    let same_origin = url.starts_with('/')
        || credentials::normalize_origin(url)
            .starts_with(&credentials::normalize_origin(server.origin()));
    http_agent(timeout, same_origin.then_some(ca_file).flatten())
}

fn login(args: &LoginArgs) -> Result<()> {
    let env_server = std::env::var(credentials::SERVER_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty());
    let server_raw = args
        .server
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .or(env_server.as_deref())
        .unwrap_or(DEFAULT_SERVER);
    let (server, _) = Server::parse(server_raw).map_err(|e| anyhow::anyhow!("{e}"))?;
    let agent = http_agent(LOGIN_TIMEOUT, args.ca_file.as_deref())?;

    let env_token = std::env::var(credentials::TOKEN_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty());
    let env_token_for_origin = match env_server.as_deref() {
        Some(env_srv)
            if credentials::normalize_origin(env_srv)
                != credentials::normalize_origin(server_raw) =>
        {
            None
        }
        _ => env_token.as_deref(),
    };
    if let Some(token) = args
        .token
        .as_deref()
        .or(env_token_for_origin)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return login_with_token(&agent, &server, token);
    }

    login_via_browser(&agent, &server, args.no_browser)
}

fn login_with_token(agent: &ureq::Agent, server: &Server, token: &str) -> Result<()> {
    let mut resp = agent
        .get(server.url("/api/cli/whoami"))
        .header("authorization", &format!("Bearer {token}"))
        .call()
        .context("could not reach server to verify token")?;
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .with_config()
        .limit(64 * 1024)
        .read_to_string()
        .unwrap_or_default();
    if status != 200 {
        bail!("server rejected token ({status}): {text}");
    }
    let me: MeResponse =
        serde_json::from_str(&text).context("unexpected /api/cli/whoami response")?;
    let path = credentials::save(&Credentials {
        server: server.origin().to_owned(),
        token: token.to_owned(),
        username: Some(me.username.clone()),
        email: Some(me.email.clone()),
    })?;
    println!(
        "Logged in as {} <{}> on {} (saved to {})",
        me.username,
        me.email,
        server.origin(),
        path.display()
    );
    Ok(())
}

fn login_via_browser(agent: &ureq::Agent, server: &Server, no_browser: bool) -> Result<()> {
    let hostname = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "workstation".to_owned());
    let start_body = serde_json::to_vec(&StartRequest { label: hostname })?;
    let mut start_resp = agent
        .post(server.url("/api/cli/login/start"))
        .header("content-type", "application/json")
        .send(&start_body)
        .context("could not start CLI login")?;
    let status = start_resp.status().as_u16();
    let start_text = start_resp
        .body_mut()
        .with_config()
        .limit(64 * 1024)
        .read_to_string()
        .unwrap_or_default();
    if status != 200 && status != 201 {
        bail!("server refused login start ({status}): {start_text}");
    }
    let started: StartResponse =
        serde_json::from_str(&start_text).context("unexpected login start response")?;

    let verify_url = if started.verify_url.starts_with('/') {
        server.url(&started.verify_url)
    } else {
        started.verify_url
    };
    eprintln!("Open this URL in your browser to approve CLI sign-in:");
    eprintln!("  {verify_url}");
    eprintln!("Confirmation code: {}", started.user_code);

    if !no_browser {
        let _ = std::process::Command::new("xdg-open")
            .arg(&verify_url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }

    let deadline = Instant::now() + Duration::from_secs(started.expires_in_secs.clamp(30, 900));
    let interval = Duration::from_millis(started.poll_interval_ms.clamp(250, 5_000));
    let poll_body = serde_json::to_vec(&PollRequest {
        poll_token: &started.poll_token,
    })?;

    loop {
        if Instant::now() >= deadline {
            bail!("login timed out before approval");
        }
        std::thread::sleep(interval);

        let Ok(mut resp) = agent
            .post(server.url("/api/cli/login/poll"))
            .header("content-type", "application/json")
            .send(&poll_body)
        else {
            continue;
        };
        let poll_status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .with_config()
            .limit(64 * 1024)
            .read_to_string()
            .unwrap_or_default();
        if poll_status != 200 {
            bail!("login poll failed ({poll_status}): {body}");
        }
        let polled: PollResponse =
            serde_json::from_str(&body).context("unexpected login poll response")?;
        match polled.status.as_str() {
            "pending" => {}
            "approved" => {
                let Some(token) = polled.token else {
                    bail!("server approved login without issuing a token");
                };
                let path = credentials::save(&Credentials {
                    server: server.origin().to_owned(),
                    token,
                    username: polled.username.clone(),
                    email: polled.email.clone(),
                })?;
                println!(
                    "Logged in as {} on {} (saved to {})",
                    polled.username.as_deref().unwrap_or("user"),
                    server.origin(),
                    path.display()
                );
                return Ok(());
            }
            "expired" => bail!("login request expired before approval"),
            other => bail!("unexpected login status {other:?}"),
        }
    }
}

fn logout(args: &LogoutArgs) -> Result<()> {
    let env_creds = match (
        std::env::var(credentials::SERVER_ENV),
        std::env::var(credentials::TOKEN_ENV),
    ) {
        (Ok(server), Ok(token)) if !server.trim().is_empty() && !token.trim().is_empty() => Some((
            server.trim().trim_end_matches('/').to_owned(),
            token.trim().to_owned(),
        )),
        _ => None,
    };

    if let Some((env_server, env_token)) = env_creds {
        if let Ok((server, _)) = Server::parse(&env_server)
            && let Ok(agent) = http_agent(LOGIN_TIMEOUT, args.ca_file.as_deref())
        {
            let _ = agent
                .post(server.url("/api/cli/logout"))
                .header("authorization", &format!("Bearer {env_token}"))
                .send(&[][..]);
        }
        println!("Revoked token from MICROSCOPE_TOKEN (saved credentials file left unchanged).");
        return Ok(());
    }

    if let Some(creds) = credentials::load()
        && let Ok((server, _)) = Server::parse(&creds.server)
        && let Ok(agent) = http_agent(LOGIN_TIMEOUT, args.ca_file.as_deref())
    {
        let _ = agent
            .post(server.url("/api/cli/logout"))
            .header("authorization", &format!("Bearer {}", creds.token))
            .send(&[][..]);
    }
    if credentials::clear()? {
        println!("Logged out.");
    } else {
        println!("Already logged out.");
    }
    Ok(())
}
