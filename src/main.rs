//! The `microscope` command-line debugger.

// Production code returns errors instead of panicking; see dev/assertions.md.
// Tests may unwrap.
#![cfg_attr(
    not(test),
    deny(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

mod args;
mod cli;
mod dap;
mod present;
mod termination;
mod web;

use std::process::ExitCode;

use anyhow::{Context, Result, anyhow};
use tokio::signal::unix::SignalKind;

// Each thread counts its allocations for `--timings`.
#[global_allocator]
static ALLOCATOR: microscope::timings::alloc::Counting<microscope::timings::alloc::Selected> =
    microscope::timings::alloc::selected();
use microscope::Debugger;
use microscope::timings;

use args::{ConfigArgs, ConfigCommand, Tool, ViewsArgs, ViewsCommand, parse_args};
use cli::session::{Session, Target};
use cli::terminal::{ColorChoice, Role};
use cli::{Cli, LaunchSettings, Renderers};

fn main() -> ExitCode {
    // SAFETY: the process has no other thread yet.
    #[allow(unsafe_code, reason = "mimalloc's options are set unsynchronized")]
    unsafe {
        microscope::timings::alloc::return_freed_memory_promptly();
    }

    // The terminal launcher must stay single-threaded, so it runs before the
    // runtime starts its threads.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|command| command == "dap-launcher")
    {
        return dap::launcher::run(std::env::args_os().skip(1));
    }

    // So is the helper that opens browsers, which forks.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|command| command == web::terminal::HELPER)
    {
        return web::terminal::run_helper(std::env::args_os().skip(2));
    }

    // Both of microscope's own variables are removed from the environment here,
    // before microscope starts any other thread, so every program it starts
    // goes without them. Removing them is unsafe only while another thread
    // may read the environment, and none exists yet.
    #[cfg(debug_assertions)]
    start_flight_recording();

    // SAFETY: the process has no other thread yet.
    #[allow(unsafe_code, reason = "the environment can only be edited unsafely")]
    unsafe {
        cli::config::take_environment();
    }

    // Tokio tasks only move messages and serve clients; heavy work runs on
    // the controller, loader, and blocking threads. A worker per CPU would
    // only add threads to a large host.
    let workers = std::thread::available_parallelism().map_or(1, |cpus| cpus.get().min(4));
    match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(async_main()),
        Err(error) => {
            eprintln!("error: cannot start the async runtime: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Streams a development build's flight recording to the file named by
/// `MICROSCOPE_FLIGHT_RECORDING`, or to a new file under the recorder's `runs`
/// directory. An empty variable turns recording off, as does running under
/// nextest without the variable, so test runs do not crowd out recordings of
/// development runs.
///
/// The variable is removed, so programs the debugger launches see the same
/// environment, and so the same stack layout, as under a release build.
#[cfg(debug_assertions)]
#[allow(unsafe_code, reason = "the environment can only be edited unsafely")]
fn start_flight_recording() {
    use microscope::flight_recorder;

    const VARIABLE: &str = "MICROSCOPE_FLIGHT_RECORDING";
    let path = std::env::var_os(VARIABLE);

    // SAFETY: `main` calls this before microscope starts any other thread, so
    // none can read the environment meanwhile.
    unsafe { std::env::remove_var(VARIABLE) };

    let started = match path {
        Some(path) if path.is_empty() => return,
        Some(path) => flight_recorder::stream_to(std::path::Path::new(&path)),
        None if std::env::var_os("NEXTEST").is_some() => return,
        None => flight_recorder::stream_run().map(drop),
    };

    if let Err(error) = started {
        eprintln!("warning: cannot write the flight recording: {error}");
    }
}

async fn async_main() -> ExitCode {
    let args = parse_args();
    if let Err(error) =
        microscope::cache::configure(args.no_cache.then_some(microscope::cache::Setting::Off))
    {
        eprintln!("warning: {error}; loading debug information without it");
    }

    match &args.tool {
        Some(Tool::Dap(dap_args)) => {
            let code = match dap::run(dap_args).await {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("error: {error:#}");
                    1
                }
            };

            // Reading stdin blocks a runtime thread that would keep the
            // runtime from shutting down after the client left.
            std::process::exit(code);
        }
        Some(Tool::Web(web_args)) => {
            let code = match web::run(web_args).await {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("error: {error:#}");
                    1
                }
            };

            // Tabs still connected would keep the runtime from shutting down.
            std::process::exit(code);
        }
        Some(Tool::Views(views)) => {
            return match run_views(views).await {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => ExitCode::FAILURE,
                Err(error) => {
                    eprintln!("error: {error:#}");
                    ExitCode::FAILURE
                }
            };
        }
        Some(Tool::Record(record)) => return cli::profile::run_record(record),
        Some(Tool::Report(report)) => return cli::profile::run_report(report),
        Some(Tool::Convert(convert)) => return cli::profile::run_convert(convert),
        Some(Tool::Symbolize(symbolize)) => return cli::profile::run_symbolize(symbolize),
        Some(Tool::Syscalls(syscalls)) => return cli::syscalls::run(syscalls),
        Some(Tool::Config(config)) => {
            return match run_config(config) {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => ExitCode::FAILURE,
                Err(error) => {
                    eprintln!("error: {error:#}");
                    ExitCode::FAILURE
                }
            };
        }
        None => {}
    }

    let recording = match &args.timings {
        Some(_) => match timings::Recording::start(timings::Options { instructions: true }) {
            Ok(recording) => Some(recording),
            Err(error) => {
                eprintln!("error: {error}");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };

    let early = Renderers::detect(args.color.unwrap_or_default(), args.batch);
    let session = match cli::session::prepare(&args, early.stderr) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("{}: {error:#}", early.stderr.paint(Role::Error, "error"));
            return ExitCode::FAILURE;
        }
    };

    let renderers = Renderers::configured(&session.settings, args.batch);

    let result = run(session, renderers).await;

    if let (Some(recording), Some(path)) = (recording, &args.timings)
        && let Err(error) = write_timings(recording, path)
    {
        eprintln!(
            "{}: cannot write the timings to {}: {error}",
            renderers.stderr.paint(Role::Error, "error"),
            path.display()
        );
        return ExitCode::FAILURE;
    }

    match result {
        Ok(None) => ExitCode::SUCCESS,
        // A read of standard input the session left would keep the runtime
        // from shutting down. A shell reports a process a signal ended as
        // 128 plus its number.
        Ok(Some(signal)) => std::process::exit(128 + signal.as_raw_value()),
        // A closed stdout pipe, such as `microscope ... | head`, is a normal
        // end.
        Err(error) if cli::is_broken_pipe(&error) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!(
                "{}: {error:#}",
                renderers.stderr.paint(Role::Error, "error")
            );
            ExitCode::FAILURE
        }
    }
}

/// Ends `recording` and writes what it collected to `path`.
fn write_timings(recording: microscope::timings::Recording, path: &std::path::Path) -> Result<()> {
    std::fs::write(path, recording.finish()?.to_json()?)?;
    Ok(())
}

/// Runs a debugging session and shuts its debugger down, which detaches
/// an attached process, however the session ends. Returns the signal that
/// ended it, if one did.
async fn run(session: Session, renderers: Renderers) -> Result<Option<SignalKind>> {
    // SIGTERM, as `kill` and service managers send it, and SIGHUP, as a
    // closed terminal or SSH connection sends it, end the session as `quit`
    // does. Left to their default action, they would leave an attached
    // process with its traps. One that comes while the target opens ends
    // the session once it has, so nothing is left half attached.
    let terminated = termination::first_of([SignalKind::terminate(), SignalKind::hangup()])
        .context("failed to listen for SIGTERM and SIGHUP")?;
    let opened = open_target(&session).await?;
    let handle = opened
        .handle()
        .with_source_paths(session.source_paths.clone());
    let mut cli = Cli::new(
        handle,
        renderers,
        session.settings.clone(),
        session.launch.clone(),
    );
    if let Opened::Group(group) = &opened {
        cli = cli.with_group(group.handle(), session.source_paths.clone());
    }

    // The session is a task of its own so that a panic in it comes back
    // here as an error, and the debugger still shuts down.
    let result = tokio::spawn(async move { cli.run(&session, terminated).await })
        .await
        .unwrap_or_else(|error| Err(anyhow!("internal error: {error}")));
    let shutdown = opened
        .shutdown()
        .await
        .context("failed to shut down debugger");

    let signal = result?;
    shutdown?;
    Ok(signal)
}

/// What a session debugs: one process, as a core dump is, or a group of the
/// processes a program starts.
enum Opened {
    Single(Debugger),
    Group(microscope::ProcessGroup),
}

impl Opened {
    fn handle(&self) -> microscope::DebuggerHandle {
        match self {
            Self::Single(debugger) => debugger.handle(),
            Self::Group(group) => group.root(),
        }
    }

    async fn shutdown(self) -> microscope::Result<()> {
        match self {
            Self::Single(debugger) => debugger.shutdown().await,
            Self::Group(group) => group.shutdown().await,
        }
    }
}

/// Opens the session's target: a launch or an attach as a process group
/// that follows as the settings say, and a core dump alone.
async fn open_target(session: &Session) -> Result<Opened> {
    let policy = session.settings.config.follow.policy(&[]);
    match &session.target {
        Target::Program(executable) => {
            let program = microscope::Program::load(executable, &session.debug_files)
                .with_context(|| {
                    format!("failed to initialize debugger for {}", executable.display())
                })?;
            Ok(Opened::Group(
                microscope::ProcessGroup::new(program, policy).with_context(|| {
                    format!("failed to initialize debugger for {}", executable.display())
                })?,
            ))
        }
        Target::Attach {
            process,
            executable,
        } => microscope::ProcessGroup::attach(
            *process,
            executable.as_deref(),
            policy,
            &session.debug_files,
        )
        .await
        .map(Opened::Group)
        .with_context(|| match executable {
            Some(_) => format!("failed to attach to process {process}"),
            None => format!(
                "failed to attach to process {process}; if automatic /proc executable discovery is unavailable, pass EXECUTABLE explicitly"
            ),
        }),
        Target::Core(_) => open_debugger(&session.target, &session.debug_files)
            .await
            .map(Opened::Single),
    }
}

/// The largest recording `microscope views replay` reads: far more than the
/// largest inspection's runs record.
const MAX_RECORDING_BYTES: u64 = 64 * 1024 * 1024;

/// Replays recorded kernel runs, saying for each whether it reproduced.
fn replay_views(recording: &std::path::Path, kernel: Option<&std::path::Path>) -> Result<bool> {
    use std::io::Read as _;
    let mut text = String::new();
    std::fs::File::open(recording)
        .and_then(|file| file.take(MAX_RECORDING_BYTES + 1).read_to_string(&mut text))
        .with_context(|| format!("failed to read {}", recording.display()))?;

    if text.len() as u64 > MAX_RECORDING_BYTES {
        anyhow::bail!(
            "{} is larger than a recording may be, {MAX_RECORDING_BYTES} bytes",
            recording.display()
        );
    }

    let wasm = kernel
        .map(|path| {
            std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))
        })
        .transpose()?;
    let runs = microscope::replay_kernel_runs(&text, wasm.as_deref())
        .map_err(|error| anyhow::anyhow!("{}: {error}", recording.display()))?;
    let mut reproduced = true;

    for (index, run) in runs.iter().enumerate() {
        match &run.outcome {
            Ok(events) => println!(
                "run {}: kernel `{}` reproduced {events} of {} events",
                index + 1,
                run.kernel,
                run.events
            ),
            Err(difference) => {
                reproduced = false;
                println!(
                    "run {}: kernel `{}` differs: {difference}",
                    index + 1,
                    run.kernel
                );
            }
        }
    }

    Ok(reproduced && !runs.is_empty())
}

/// Runs `microscope views check`, `explain`, or `replay`.
async fn run_views(args: &ViewsArgs) -> Result<bool> {
    let (program, views) = match &args.command {
        ViewsCommand::Check { program, views } | ViewsCommand::Explain { program, views, .. } => {
            (program, views)
        }
        ViewsCommand::Replay { recording, kernel } => {
            return replay_views(recording, kernel.as_deref());
        }
    };

    let debugger = Debugger::new(program)
        .with_context(|| format!("failed to initialize debugger for {}", program.display()))?;
    let renderers = Renderers::detect(ColorChoice::Auto, true);
    let root = cli::config::project_root(&std::env::current_dir().unwrap_or_default());
    let console = Cli::new(
        debugger.handle(),
        renderers,
        cli::config::Settings::defaults(root.clone()),
        LaunchSettings::default(),
    );

    // A file that could not be used fails either command, as a view that
    // binds nothing fails a check.
    let usable = console.load_views(&root, views).await;
    let handle = debugger.handle();
    let reported: Result<bool> = async {
        Ok(match &args.command {
            ViewsCommand::Check { .. } => {
                let check = handle.check_views().await?;
                let (report, failed) = cli::format::view_check(&check, renderers.stdout);
                println!("{report}");
                !failed
            }
            ViewsCommand::Replay { .. } => anyhow::bail!("internal: a replay needs no program"),
            ViewsCommand::Explain { name, .. } => {
                let types = handle.explain_type(name).await?;
                println!(
                    "{}",
                    cli::format::type_views(name, &types, renderers.stdout)
                );
                !types.is_empty()
            }
        })
    }
    .await;
    let shutdown = debugger
        .shutdown()
        .await
        .context("failed to shut down the debugger");
    let succeeded = reported?;
    shutdown?;

    Ok(succeeded && usable)
}

async fn open_debugger(
    target: &Target,
    debug_files: &microscope::DebugFileOptions,
) -> Result<Debugger> {
    match target {
        Target::Core(options) => {
            let options = microscope::CoreDumpOptions {
                debug_files: debug_files.clone(),
                ..options.clone()
            };

            Debugger::open_core(&options)
                .with_context(|| format!("failed to open core dump {}", options.core.display()))
        }

        Target::Attach {
            process,
            executable: Some(executable),
        } => Debugger::attach_with(*process, Some(executable), debug_files)
            .await
            .with_context(|| format!("failed to attach to process {process}")),
        Target::Attach {
            process,
            executable: None,
        } => Debugger::attach_with(*process, None, debug_files)
            .await
            .with_context(|| {
                format!(
                    "failed to attach to process {process}; if automatic /proc executable discovery is unavailable, pass EXECUTABLE explicitly"
                )
            }),
        Target::Program(executable) => Debugger::new_with(executable, debug_files).with_context(|| {
            format!("failed to initialize debugger for {}", executable.display())
        }),
    }
}

/// Prints the project root and the files a session started in `start`
/// reads and writes.
fn print_config_paths(start: &std::path::Path, user: Option<&std::path::Path>) {
    let exists = |path: &std::path::Path| if path.exists() { "" } else { " (missing)" };
    let root = cli::config::project_root(start);
    println!("project root: {}", root.display());

    match user {
        Some(path) => println!("user:         {}{}", path.display(), exists(path)),
        None => println!("user:         none (MICROSCOPE_CONFIG is empty)"),
    }

    let mut files = vec![
        ("project:", root.join(".microscope/config.toml")),
        ("local:", root.join(".microscope/config.local.toml")),
        ("views:", root.join(".microscope/views")),
        (
            "breakpoints:",
            root.join(".microscope/state/breakpoints.toml"),
        ),
    ];

    if let Some(state) = cli::config::state_directory() {
        files.push(("trust:", state.join("trust.toml")));
        files.push(("history:", state.join("history")));
    }

    for (name, path) in files {
        println!("{name:<13} {}{}", path.display(), exists(&path));
    }
}

/// Runs `microscope config` and returns whether it succeeded.
fn run_config(args: &ConfigArgs) -> Result<bool> {
    use cli::config::{self, Files, TrustStore, UserFile};
    let start = std::env::current_dir().context("cannot read the working directory")?;
    let user = UserFile::choose(false, args.config.as_deref());
    let user_path = match &user {
        UserFile::Named(path) => Some(path.clone()),
        UserFile::Standard => {
            config::user_directory().map(|directory| directory.join("config.toml"))
        }
        UserFile::Disabled => None,
    };

    match args.command {
        ConfigCommand::Path => {
            print_config_paths(&start, user_path.as_deref());
            Ok(true)
        }
        ConfigCommand::Show | ConfigCommand::Check => {
            let files = Files::read(&start, &user)?;
            let settings = files.settings(true)?;

            if matches!(args.command, ConfigCommand::Show) {
                println!("{}", settings.show());
            } else {
                for file in &files.read {
                    println!("{}: ok", file.path.display());
                }

                if files.read.is_empty() {
                    println!("no settings files exist");
                }
            }

            Ok(true)
        }
        ConfigCommand::Init => {
            let path = user_path.context("no user settings file: set XDG_CONFIG_HOME or HOME")?;
            if path.exists() {
                anyhow::bail!("{} already exists; it was left as it is", path.display());
            }

            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("cannot create {}", parent.display()))?;
            }

            std::fs::write(&path, config::commented_defaults())
                .with_context(|| format!("cannot write {}", path.display()))?;
            println!("wrote {}", path.display());
            Ok(true)
        }
        ConfigCommand::Trust => {
            let files = Files::read(&start, &user)?;
            let Some(acting) = files.acting() else {
                println!(
                    "the project at {} has no startup commands, aliases, or launch configurations to trust",
                    files.root.display()
                );
                return Ok(true);
            };

            let mut store = TrustStore::load()?;
            store
                .trust(&files.root, &acting)
                .map_err(anyhow::Error::msg)?;
            println!(
                "trusted the project at {}:
",
                files.root.display()
            );
            for line in acting.lines() {
                println!("    {line}");
            }

            Ok(true)
        }
        ConfigCommand::Untrust => {
            let root = config::project_root(&start);
            let mut store = TrustStore::load()?;

            if store.untrust(&root).map_err(anyhow::Error::msg)? {
                println!("forgot the project at {}", root.display());
            } else {
                println!("the project at {} was not trusted", root.display());
            }

            Ok(true)
        }
        ConfigCommand::Trusted => {
            let store = TrustStore::load()?;
            let mut any = false;

            for root in store.roots() {
                println!("{}", root.display());
                any = true;
            }

            if !any {
                println!("no project is trusted");
            }

            Ok(true)
        }
    }
}
