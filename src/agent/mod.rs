//! `microscope agent`: keeps this machine connected to microscope's service,
//! uploads the recordings queued in its outbox, and, when allowed, records
//! the processes the service asks for.
//!
//! The agent never touches a debugger; it records the way `microscope
//! record --pid` does. It is meant for developers' workstations.

mod backoff;
mod commands;
pub mod outbox;
mod proto;
mod session;
pub mod settings;
mod wire;

use std::collections::BTreeMap;
use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::sync::{Notify, watch};

use self::backoff::Backoff;
use self::commands::Recorder;
use self::outbox::{Outbox, Watched};
use self::session::{Agent, End, Timeouts};
use self::settings::{Paths, Settings};

/// A connection that lasted this long starts the backoff over.
const STEADY: Duration = Duration::from_secs(60);

#[derive(Debug, clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct AgentArgs {
    #[command(subcommand)]
    command: Option<AgentCommand>,

    /// The service to connect to, over agent.toml's `server`.
    #[arg(long, value_name = "URL")]
    server: Option<String>,

    /// Also upload each segment `record --continuous DIR` writes to DIR,
    /// with agent.toml's `watch`.
    #[arg(long, value_name = "DIR")]
    watch: Vec<PathBuf>,

    /// Run the commands the service sends, such as recording one of this
    /// user's processes, as agent.toml's `allow-remote` does.
    #[arg(long)]
    allow_remote: bool,

    /// How long to wait for the service's welcome, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = 10.0, hide = true)]
    hello_timeout: f64,

    /// How long the service may be silent before the agent reconnects, in
    /// seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = 60.0, hide = true)]
    idle_timeout: f64,
}

#[derive(Debug, clap::Subcommand)]
enum AgentCommand {
    /// Save the device token the service made for this machine, reading it
    /// from standard input.
    Login(LoginArgs),
}

#[derive(Debug, clap::Args)]
struct LoginArgs {
    /// The service the token is for, such as `https://example.com/`; it
    /// becomes agent.toml's `server`.
    #[arg(long, value_name = "URL")]
    server: Option<String>,
}

/// Runs `microscope agent`, returning its exit code.
pub async fn main(args: &AgentArgs) -> i32 {
    match run(args).await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error:#}");
            1
        }
    }
}

async fn run(args: &AgentArgs) -> Result<()> {
    let paths = Paths::locate()?;
    if let Some(AgentCommand::Login(login_args)) = &args.command {
        return login(&paths, login_args).await;
    }

    let settings = Settings::read(&paths.settings)?;
    let server = args
        .server
        .clone()
        .or(settings.server)
        .context("no service to connect to; run `microscope agent login --server URL`")?;
    let endpoint = settings::endpoint(&server)?;
    if settings::exposes_token(&endpoint) {
        eprintln!(
            "microscope: warning: {server} is not https, so the device token and recordings cross the network unencrypted"
        );
    }
    let token = settings::read_token(&paths.token)?;
    settings::check_token(&token)?;

    let outbox = Outbox::open(&paths.outbox)?;
    let watched = settings
        .watch
        .iter()
        .chain(&args.watch)
        .map(|dir| Watched::new(dir, &paths.watched))
        .collect();
    let allow_remote = args.allow_remote || settings.allow_remote;
    if allow_remote {
        eprintln!("microscope: the service may record this user's processes");
    }

    let agent = Agent {
        server,
        endpoint,
        token,
        hostname: hostname(),
        allow_remote,
        timeouts: Timeouts {
            hello: seconds(args.hello_timeout, "--hello-timeout")?,
            idle: seconds(args.idle_timeout, "--idle-timeout")?,
        },
        outbox,
        watched,
        uploads: Arc::new(Notify::new()),
        recorder: Arc::new(Recorder::default()),
        held: Mutex::new(BTreeMap::new()),
    };

    // The handlers are in place before the agent says anything, so a
    // signal never finds the default action that kills it.
    let signals = signals().context("handling signals")?;
    let (stop, mut shutdown) = watch::channel(false);
    tokio::spawn(async move {
        signalled(signals).await;
        let _ = stop.send(true);
    });

    let ended = serve(&agent, &mut shutdown).await;

    // Recordings under way stop, and wait in the outbox for the next run.
    let recorder = Arc::clone(&agent.recorder);
    let _ = tokio::task::spawn_blocking(move || recorder.stop_all()).await;
    if let Ok(pending) = agent.outbox.pending()
        && !pending.is_empty()
    {
        eprintln!(
            "microscope: {} recordings wait in the outbox for the next connection",
            pending.len()
        );
    }
    ended
}

/// Connects, again and again, until the agent is stopped or refused.
async fn serve(agent: &Agent, shutdown: &mut watch::Receiver<bool>) -> Result<()> {
    let mut backoff = Backoff::default();
    loop {
        let (end, welcomed) = agent.serve(shutdown).await;
        let why = match end {
            End::Shutdown => return Ok(()),
            End::Fatal(why) => anyhow::bail!(why),
            End::Retry(why) => why,
        };

        if welcomed.is_some_and(|at| at.elapsed() >= STEADY) {
            backoff.reset();
        }
        let wait = backoff.next();
        eprintln!(
            "microscope: {why}; connecting again in {:.1} s",
            wait.as_secs_f64()
        );
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = shutdown.changed() => return Ok(()),
        }
    }
}

async fn login(paths: &Paths, args: &LoginArgs) -> Result<()> {
    let mut settings = Settings::read(&paths.settings)?;
    let server = args
        .server
        .clone()
        .or_else(|| settings.server.clone())
        .context("name the service the token is for with --server URL")?;
    settings::endpoint(&server)?;

    let token = tokio::task::spawn_blocking(|| {
        let stdin = std::io::stdin();
        if stdin.is_terminal() {
            eprint!("Paste the device token the service made for this machine: ");
            let _ = std::io::stderr().flush();
        }
        let mut line = String::new();
        stdin.lock().read_line(&mut line).map(|_| line)
    })
    .await?
    .context("reading the device token from standard input")?;
    let token = token.trim();
    settings::check_token(token)?;

    settings::write_token(&paths.token, token)?;
    settings.server = Some(server.clone());
    settings.write(&paths.settings)?;
    eprintln!(
        "microscope: saved the device token for {server} to {}; start the agent with `microscope agent`",
        paths.token.display()
    );
    Ok(())
}

/// How long `record --upload` waits for the agent to upload the recording.
const UPLOAD_WAIT: Duration = Duration::from_secs(30);

/// Queues the recording at `path` in the agent's outbox, under `name`, and
/// waits for the agent to say where the service stored it: the URL, or
/// `None` if the agent has not uploaded it yet.
pub fn upload(path: &std::path::Path, name: &str) -> Result<Option<String>> {
    let paths = Paths::locate()?;
    let outbox = Outbox::open(&paths.outbox)?;
    let key = outbox.add(path, name)?;

    let deadline = std::time::Instant::now() + UPLOAD_WAIT;
    while std::time::Instant::now() < deadline {
        if let Some(url) = outbox.url(key) {
            return Ok(Some(url));
        }
        if let Some(why) = outbox.error(key) {
            anyhow::bail!("the service refused the recording: {why}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(None)
}

fn seconds(value: f64, flag: &str) -> Result<Duration> {
    Duration::try_from_secs_f64(value).with_context(|| format!("{flag} is a number of seconds"))
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().to_owned())
        .unwrap_or_default()
}

type Signals = (tokio::signal::unix::Signal, tokio::signal::unix::Signal);

fn signals() -> std::io::Result<Signals> {
    use tokio::signal::unix::{SignalKind, signal};
    Ok((
        signal(SignalKind::interrupt())?,
        signal(SignalKind::terminate())?,
    ))
}

/// Waits for the first SIGINT or SIGTERM.
async fn signalled((mut interrupt, mut terminate): Signals) {
    tokio::select! {
        _ = interrupt.recv() => {}
        _ = terminate.recv() => {}
    }
}
