//! The browser debugger, `microscope web`.
//!
//! One server debugs one program at a time and serves it to every tab that
//! joins: the page, `/api/login`, which turns a join link's token into a
//! cookie, and `/api/ws`, the WebSocket each tab talks over. Like the CLI
//! and the DAP adapter, it is a client of [`microscope::DebuggerHandle`].

mod assets;
mod auth;
mod connection;
mod describe;
mod draw;
mod frame;
mod inspect;
mod live;
mod lowlevel;
mod picker;
mod profiles;
mod protocol;
mod session;
mod syscalls;
pub mod terminal;
mod values;

use std::ffi::OsString;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use axum::Router;
use axum::extract::{State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use microscope::{CoreDumpOptions, ProcessId};

use auth::{Origins, Tokens};
use protocol::Role;
use session::{LaunchSpec, Session, Start};

/// The port tried first, so links stay the same from run to run.
const DEFAULT_PORT: u16 = 7341;

/// The largest message a tab may send.
const MAX_MESSAGE: usize = 1024 * 1024;

/// Serves a browser debugger.
#[derive(clap::Args)]
#[expect(clippy::struct_excessive_bools, reason = "each is a flag")]
#[command(after_help = "\
Common forms:
  microscope web                       choose what to debug in the page
  microscope web EXECUTABLE [-- ARGS]  load a program, which runs at the first continue
  microscope web PROFILE.usprof        explore a profile `microscope record` wrote
  microscope web --attach PID
  microscope web --core CORE [EXECUTABLE]")]
pub struct WebArgs {
    /// Executable to launch, or the one that wrote --core, or a profile to
    /// explore.
    #[arg(value_name = "EXECUTABLE")]
    executable: Option<PathBuf>,

    /// More profiles to serve beside the first, to compare it with.
    #[arg(value_name = "PROFILE", conflicts_with_all = ["attach", "core"])]
    more_profiles: Vec<PathBuf>,

    /// Attach to a running process.
    #[arg(short = 'p', long, value_name = "PID", conflicts_with_all = ["core", "executable"])]
    attach: Option<u64>,

    /// Open a core dump.
    #[arg(long, value_name = "CORE")]
    core: Option<PathBuf>,

    /// Start the program at once instead of at the first continue.
    #[arg(long, conflicts_with_all = ["attach", "core"])]
    run: bool,

    /// Let a process attached to run on at once instead of staying stopped
    /// at the attach until a tab continues it; this applies to processes
    /// the page attaches to as well.
    #[arg(long)]
    resume: bool,

    /// Stop at the program's first instruction when it starts.
    #[arg(long, conflicts_with_all = ["attach", "core"])]
    stop_at_entry: bool,

    /// Run the launched program in DIR.
    #[arg(long, value_name = "DIR", conflicts_with_all = ["attach", "core"])]
    cwd: Option<PathBuf>,

    /// Set NAME to VALUE in the launched program's environment. May be repeated.
    #[arg(
        long = "env",
        value_name = "NAME=VALUE",
        value_parser = crate::parse_environment_variable,
        conflicts_with_all = ["attach", "core"]
    )]
    environment: Vec<(OsString, OsString)>,

    /// Listen on 127.0.0.1:PORT. By default 7341, or any free port if it is taken.
    #[arg(long, value_name = "PORT", conflicts_with = "listen")]
    port: Option<u16>,

    /// Listen on ADDRESS instead, such as 0.0.0.0:7341. Anyone who can
    /// reach it and has a link can use the session; prefer an SSH tunnel.
    #[arg(long, value_name = "ADDRESS")]
    listen: Option<SocketAddr>,

    /// Serve the page through a reverse proxy at URL, such as
    /// `https://proxy.example/debug/7/`: every route lies under its path,
    /// requests may name its host and origin, and links name it. The proxy
    /// passes `Host` through, and controls who reaches it.
    #[arg(long, value_name = "URL")]
    public_url: Option<auth::PublicUrl>,

    /// Listen on the Unix domain socket PATH instead of TCP, for a proxy on
    /// this machine. PATH must not exist; the server removes it when it
    /// stops. Requires --public-url, the only address pages are served at.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with_all = ["port", "listen"],
        requires = "public_url"
    )]
    listen_unix: Option<PathBuf>,

    /// Serve profiles to a proxy that has already decided who may see them.
    /// Every request that passes the Host and Origin checks for --public-url
    /// is read-only, with no token or cookie. Requires --public-url, and
    /// serves only profiles.
    #[arg(long, requires = "public_url")]
    trust_proxy: bool,

    /// Also accept pages served from ORIGIN, such as the development
    /// server's `http://127.0.0.1:5173`. May be repeated.
    #[arg(long = "allow-origin", value_name = "ORIGIN", hide = true)]
    allow_origins: Vec<String>,

    /// Look up --core's files under DIR, a copy of the dumping machine's
    /// files, as the terminal's --sysroot does.
    #[arg(long, value_name = "DIR", requires = "core")]
    sysroot: Option<PathBuf>,

    /// Search DIR for --core's files missing or different at their recorded
    /// paths, as the terminal's --module-path does. May be repeated.
    #[arg(long = "module-path", value_name = "DIR", requires = "core")]
    module_paths: Vec<PathBuf>,

    /// Search DIR for the separate debug files of modules stripped of their
    /// debug information, before the system's directories. May be repeated.
    #[arg(long = "debug-directory", value_name = "DIR")]
    debug_directories: Vec<PathBuf>,

    /// Download debug files no directory holds from the debuginfod servers
    /// `DEBUGINFOD_URLS` lists.
    #[arg(long)]
    debuginfod: bool,

    /// Present values with the views in FILE, and draw them with the
    /// renderers beside it, before the project's and the user's. May be
    /// repeated.
    #[arg(long = "views", value_name = "FILE")]
    views: Vec<PathBuf>,

    /// Arguments passed to the launched program.
    #[arg(last = true, value_name = "ARGS", conflicts_with_all = ["attach", "core"])]
    arguments: Vec<OsString>,
}

struct App {
    session: Arc<Session>,
    profiles: Arc<profiles::Profiles>,
    origins: Origins,
    /// The TCP port the cookie is named for; 0 on a Unix socket.
    port: u16,
    public: Option<auth::PublicUrl>,
    trust_proxy: bool,
}

impl App {
    /// The role this request's cookie grants, if it came from this server's
    /// own pages.
    fn authorize(&self, headers: &HeaderMap) -> Option<Role> {
        if !self.is_own_page(headers) {
            return None;
        }

        // The proxy has already decided who may see the page.
        if self.trust_proxy {
            return Some(Role::View);
        }

        let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
        self.session
            .tokens()
            .role(auth::cookie(cookies, self.port)?)
    }

    fn is_own_page(&self, headers: &HeaderMap) -> bool {
        let text = |name| {
            headers
                .get(name)
                .and_then(|value: &HeaderValue| value.to_str().ok())
        };

        self.origins
            .allows(text(header::HOST), text(header::ORIGIN))
    }

    fn known_host(&self, headers: &HeaderMap) -> bool {
        headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|host| self.origins.knows(host))
    }

    /// The path every route lies under.
    fn base(&self) -> &str {
        self.public
            .as_ref()
            .map_or("/", |public| public.base.as_str())
    }
}

/// Runs the server until it is interrupted.
pub async fn run(args: &WebArgs) -> Result<()> {
    // Ctrl+C is caught from here on. One that comes while the program starts
    // ends the server once it has, so nothing is left half launched.
    let terminated = crate::dap::termination();
    let cwd = std::env::current_dir().context("the working directory")?;
    let profiles = profiles::Profiles::new(cwd.clone());
    let (start, profile) = what_to_serve(args, &profiles)?;
    if args.trust_proxy {
        anyhow::ensure!(profile.is_some(), "--trust-proxy serves only profiles");
    }

    let listener = bind(args).await?;
    let address = listener.address().context("the listening address")?;
    let origins = Origins::new(address, args.allow_origins.clone(), args.public_url.clone());
    let link_base = match (&args.public_url, origins.primary()) {
        (Some(public), _) => public.url(),
        (None, Some(host)) => format!("http://{host}/"),
        // A Unix socket listener requires --public-url, so one arm holds.
        (None, None) => anyhow::bail!("no address to serve pages at"),
    };
    let tokens = Tokens::mint().context("failed to mint access tokens")?;
    let debug_files = microscope::DebugFileOptions {
        directories: args.debug_directories.clone(),
        debuginfod: args.debuginfod,
        ..microscope::DebugFileOptions::default()
    };

    let session = Session::new(cwd, link_base, tokens, debug_files, args.views.clone());
    session.resume_attached(args.resume);
    if args.trust_proxy {
        session.stop_sharing();
    }
    let app = Arc::new(App {
        session: Arc::clone(&session),
        profiles,
        origins,
        port: address.map_or(0, |address| address.port()),
        public: args.public_url.clone(),
        trust_proxy: args.trust_proxy,
    });

    // The page a profile opens at, relative to the public URL's path.
    let page = profile.map_or_else(|| "/".to_owned(), |id| format!("/profile/{id}"));
    let keys = if args.trust_proxy {
        announce_ready(&app, &page)?;
        None
    } else {
        announce_link(&session, &app, &page)?
    };

    // The page is served while the program loads, which can take a while,
    // and a tab that comes meanwhile sees it loading.
    let starting = start.map(|start| {
        session.loading(&start);
        let session = Arc::clone(&session);
        tokio::spawn(async move {
            // The page shows a failure to start, as the terminal does.
            if let Err(failure) = session.start(start, false).await {
                eprintln!("error: {}", failure.body().message);
            }
        })
    });

    let app_profiles = Arc::clone(&app.profiles);
    let router = router(app);
    let served = tokio::select! {
        served = serve(listener, router) => served.context("the server failed"),
        () = terminated => Ok(()),
    };

    if let Some(starting) = starting {
        // Whatever it started is ended below, not left half launched.
        // Loading cannot be interrupted, so a large program is waited for.
        if !starting.is_finished() {
            eprintln!("microscope web: stopping once the program has loaded");
        }

        let _ = starting.await;
    }

    session.shutdown().await;

    // Recordings end with the server, and so do the programs they run.
    let profiles = Arc::clone(&app_profiles);
    let _ = tokio::task::spawn_blocking(move || profiles.shutdown()).await;
    drop(keys);

    // The socket is the file this server created, so it goes with the server.
    if let Some(path) = &args.listen_unix {
        let _ = std::fs::remove_file(path);
    }

    served
}

/// Prints where the server listens and the link that opens the page. Returns
/// the key listener a terminal can use to open the link.
fn announce_link(session: &Session, app: &App, page: &str) -> Result<Option<terminal::Keys>> {
    let link = session.join_link(Role::Control, page);
    // Before any debugger exists: see `terminal`.
    let keys = if terminal::interactive() {
        match terminal::start_opener() {
            Ok(opener) => Some(terminal::Keys::listen(link.clone(), opener)?),
            Err(error) => {
                eprintln!("warning: pressing o cannot open a browser: {error}");
                None
            }
        }
    } else {
        None
    };

    let mut stdout = std::io::stdout().lock();
    if let Some(primary) = app.origins.primary() {
        writeln!(stdout, "microscope web: serving http://{primary}")?;
    }

    if let Some(public) = &app.public {
        writeln!(stdout, "pages are served through {}", public.url())?;
    }

    writeln!(stdout, "open {link}")?;
    writeln!(stdout, "anyone with this link can control the program")?;
    if keys.is_some() {
        writeln!(stdout, "press o to open it in your browser")?;
    }

    stdout.flush()?;
    Ok(keys)
}

/// Prints the one line a supervisor reads on stdout: where the first profile's
/// page lies under the public URL. No join link is printed, since the proxy
/// decides who sees the page.
fn announce_ready(app: &App, page: &str) -> Result<()> {
    if let Some(public) = &app.public {
        eprintln!("microscope web: pages are served through {}", public.url());
    }

    let mut stdout = std::io::stdout().lock();
    writeln!(
        stdout,
        "ready {}{}",
        app.base(),
        page.trim_start_matches('/')
    )?;
    stdout.flush()?;
    Ok(())
}

/// Serves `router` on whichever listener was bound.
async fn serve(listener: Listener, router: Router) -> std::io::Result<()> {
    match listener {
        Listener::Tcp(listener) => axum::serve(listener, router).await,
        Listener::Unix(listener) => axum::serve(listener, router).await,
    }
}

/// Every route, under the public URL's path when there is one.
fn router(app: Arc<App>) -> Router {
    let base = app.base().trim_end_matches('/').to_owned();
    let root_app = Arc::clone(&app);
    let api = Router::new()
        .route("/api/ws", get(socket))
        .route("/api/login", post(login))
        .route("/api/check", post(check))
        .route("/api/profile/{id}", post(download))
        .route("/visualizer-frame", get(frame::frame))
        .fallback(get(page))
        .with_state(app);

    // Behind a proxy, nothing outside the public URL's path is served.
    if base.is_empty() {
        api
    } else {
        // nest serves the path without its slash but not with it, which is
        // the URL itself: its page is the root's.
        let root = move |headers: HeaderMap| page(State(root_app), headers, Uri::from_static("/"));
        Router::new()
            .nest(&base, api)
            .route(&format!("{base}/"), get(root))
    }
}

/// What to debug, or the profile to serve, by its id: the executable
/// argument may name either.
fn what_to_serve(
    args: &WebArgs,
    profiles: &profiles::Profiles,
) -> Result<(Option<Start>, Option<String>)> {
    let profile = match &args.executable {
        Some(path) if args.core.is_none() => open_profile(profiles, path)?,
        _ => None,
    };

    for path in &args.more_profiles {
        anyhow::ensure!(
            profile.is_some(),
            "{} is not a profile, so it takes its arguments after --",
            args.executable.as_deref().unwrap_or(path).display()
        );
        open_profile(profiles, path)?
            .with_context(|| format!("{} is not a profile", path.display()))?;
    }

    Ok(if profile.is_some() {
        (None, profile)
    } else {
        (start_from(args), None)
    })
}

/// Serves `path` when it is a profile, returning its id; a program is not.
fn open_profile(profiles: &profiles::Profiles, path: &std::path::Path) -> Result<Option<String>> {
    if path.is_dir() {
        let id = profiles
            .add_segments(path)
            .with_context(|| format!("reading {}", path.display()))?;
        return Ok(Some(id));
    }

    // Only a regular file is read here: a program may be one that blocks
    // its reader, such as a FIFO, which the page is served while loading.
    if !std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return Ok(None);
    }

    let mut magic = [0; 8];
    let read = std::fs::File::open(path)
        .and_then(|mut file| std::io::Read::read(&mut file, &mut magic))
        .unwrap_or(0);
    if microscope::profiler::Format::of_file(path, &magic[..read]).is_none() {
        return Ok(None);
    }

    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let id = profiles
        .add(path, &bytes)
        .with_context(|| format!("reading {}", path.display()))?;

    Ok(Some(id))
}

fn start_from(args: &WebArgs) -> Option<Start> {
    if let Some(pid) = args.attach {
        return Some(Start::Attach(ProcessId::new(pid)));
    }

    if let Some(core) = &args.core {
        let mut options = CoreDumpOptions::new(core.clone());
        options.executable.clone_from(&args.executable);
        options.sysroot.clone_from(&args.sysroot);
        options.module_paths.clone_from(&args.module_paths);
        return Some(Start::Core(options));
    }

    let program = args.executable.clone()?;

    Some(Start::Launch {
        spec: LaunchSpec {
            program,
            arguments: args.arguments.clone(),
            environment: args.environment.clone(),
            cwd: args.cwd.clone(),
            stop_at_entry: args.stop_at_entry,
        },
        run: args.run,
    })
}

/// Where the server accepts connections.
enum Listener {
    Tcp(tokio::net::TcpListener),
    Unix(tokio::net::UnixListener),
}

impl Listener {
    /// The TCP address, which a Unix socket does not have.
    fn address(&self) -> std::io::Result<Option<SocketAddr>> {
        match self {
            Self::Tcp(listener) => listener.local_addr().map(Some),
            Self::Unix(_) => Ok(None),
        }
    }
}

async fn bind(args: &WebArgs) -> Result<Listener> {
    if let Some(path) = &args.listen_unix {
        // A file already there is never replaced, since it may belong to
        // something else.
        anyhow::ensure!(
            std::fs::symlink_metadata(path).is_err(),
            "{} already exists; remove it or choose another path",
            path.display()
        );
        let listener = tokio::net::UnixListener::bind(path)
            .with_context(|| format!("cannot listen on {}", path.display()))?;
        return Ok(Listener::Unix(listener));
    }

    if let Some(address) = args.listen {
        return tokio::net::TcpListener::bind(address)
            .await
            .map(Listener::Tcp)
            .with_context(|| format!("cannot listen on {address}"));
    }

    let loopback = |port| SocketAddr::from(([127, 0, 0, 1], port));
    if let Some(port) = args.port {
        return tokio::net::TcpListener::bind(loopback(port))
            .await
            .map(Listener::Tcp)
            .with_context(|| format!("cannot listen on port {port}"));
    }

    match tokio::net::TcpListener::bind(loopback(DEFAULT_PORT)).await {
        Ok(listener) => Ok(Listener::Tcp(listener)),
        Err(_) => tokio::net::TcpListener::bind(loopback(0))
            .await
            .map(Listener::Tcp)
            .context("cannot listen on any port"),
    }
}

async fn socket(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some(role) = app.authorize(&headers) else {
        return (StatusCode::FORBIDDEN, "join with a link first").into_response();
    };

    let session = Arc::clone(&app.session);
    let profiles = Arc::clone(&app.profiles);
    upgrade
        .max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| connection::serve(socket, session, profiles, role))
}

/// Trades a join link's token for the cookie that carries it.
async fn login(State(app): State<Arc<App>>, headers: HeaderMap, body: String) -> Response {
    if !app.is_own_page(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }

    if app.session.tokens().role(body.trim()).is_none() {
        return (StatusCode::FORBIDDEN, "this link is not for this server").into_response();
    }

    let cookie = auth::set_cookie(app.port, body.trim(), app.public.as_ref());

    HeaderValue::from_str(&cookie).map_or_else(
        |_| StatusCode::BAD_REQUEST.into_response(),
        |cookie| (StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response(),
    )
}

/// A profile's file, for saving. It is a POST, which carries the `Origin`
/// that shows the request came from this server's own page.
async fn download(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if app.authorize(&headers).is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }

    let Some((name, bytes)) = app.profiles.file(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let Ok(bytes) = bytes else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    // Only characters a file name and a header both take.
    let name: String = name
        .chars()
        .map(|char| {
            if char.is_ascii_alphanumeric() || "._-".contains(char) {
                char
            } else {
                '_'
            }
        })
        .collect();

    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        bytes,
    )
        .into_response()
}

/// Says whether this browser's cookie grants access, which a refused
/// WebSocket cannot tell the page.
async fn check(State(app): State<Arc<App>>, headers: HeaderMap) -> StatusCode {
    if app.authorize(&headers).is_some() {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::FORBIDDEN
    }
}

async fn page(State(app): State<Arc<App>>, headers: HeaderMap, uri: Uri) -> Response {
    if !app.known_host(&headers) {
        return (StatusCode::MISDIRECTED_REQUEST, "unknown host").into_response();
    }

    if uri.path().starts_with("/api/") {
        return StatusCode::NOT_FOUND.into_response();
    }

    let Some(asset) = assets::find(uri.path()) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let cache = if asset.immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };

    // Through a proxy the page may sit in a frame of the proxy's own pages.
    let csp = if app.public.is_some() {
        "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
         frame-ancestors 'self'; base-uri 'none'; form-action 'none'"
    } else {
        "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
         frame-ancestors 'none'; base-uri 'none'; form-action 'none'"
    };

    let body = if asset.page {
        assets::rooted(&asset.bytes, app.base())
    } else {
        asset.bytes.into_owned()
    };

    (
        [
            (header::CONTENT_TYPE, asset.media_type),
            (header::CACHE_CONTROL, cache),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CONTENT_SECURITY_POLICY, csp),
        ],
        body,
    )
        .into_response()
}
