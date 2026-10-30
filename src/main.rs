//! The `microscope` command-line debugger.

// Production code returns errors instead of panicking; see dev/assertions.md.
// Tests may unwrap.
#![cfg_attr(
    not(test),
    deny(
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

mod cli;
mod dap;
mod present;
mod web;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};

// Each thread counts its allocations for `--timings`.
#[global_allocator]
static ALLOCATOR: microscope::timings::alloc::Counting<microscope::timings::alloc::Selected> =
    microscope::timings::alloc::selected();
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use microscope::Debugger;
use microscope::timings;

use cli::session::{Session, Target};
use cli::terminal::{ColorChoice, Role};
use cli::{Cli, DisassemblySyntax, LaunchSettings, Renderers};

const COMMON_FORMS: &str = "\
Common forms:
  microscope EXECUTABLE [-- ARGS...]
  microscope --attach PID|NAME [EXECUTABLE]
  microscope --core CORE [EXECUTABLE]
  microscope --launch NAME              a launch configuration of the project

Use `microscope --help` for every option, and `microscope config` for settings.";

#[derive(Parser)]
#[expect(clippy::struct_excessive_bools, reason = "each is a flag")]
#[command(
    version,
    about = "Debug Linux x86-64 programs, processes, and core dumps",
    after_help = COMMON_FORMS,
    subcommand_help_heading = "Tools",
    subcommand_value_name = "TOOL",
    subcommand_negates_reqs = true,
    args_conflicts_with_subcommands = true
)]
struct Args {
    /// Native executable to launch.
    ///
    /// With --attach or --core, the executable is normally discovered but
    /// can be supplied when automatic discovery is unavailable.
    #[arg(value_name = "EXECUTABLE", help_heading = "Target")]
    executable: Option<PathBuf>,

    /// Attach to a running process, by its id or by its name, which must
    /// name exactly one process.
    ///
    /// The executable is discovered through /proc by default.
    #[arg(
        short = 'p',
        long,
        value_name = "PID|NAME",
        conflicts_with = "core",
        help_heading = "Target"
    )]
    attach: Option<String>,

    /// Inspect a core dump.
    ///
    /// The executable recorded in the dump is used by default.
    #[arg(long, value_name = "CORE", help_heading = "Target")]
    core: Option<PathBuf>,

    /// Which processes the program starts are debugged too, over [follow]:
    /// `auto`, `all`, or `none`, or a program to debug by file name, as
    /// `node` or 'py*'. May be repeated.
    #[arg(
        long = "follow",
        value_name = "MODE|PROGRAM",
        conflicts_with = "core",
        help_heading = "Target"
    )]
    follow: Vec<String>,

    /// Start the project's launch configuration NAME, from its
    /// .microscope/config.toml. A project with one starts it by default.
    #[arg(
        short = 'l',
        long,
        value_name = "NAME",
        conflicts_with_all = ["executable", "attach", "core"],
        help_heading = "Target"
    )]
    launch: Option<String>,

    /// Execute commands from a file. May be repeated.
    #[arg(
        short = 'c',
        long = "command",
        value_name = "FILE",
        help_heading = "Startup"
    )]
    command_files: Vec<PathBuf>,

    /// Execute one command. May be repeated.
    #[arg(
        short = 'e',
        long = "eval",
        value_name = "COMMAND",
        help_heading = "Startup"
    )]
    commands: Vec<String>,

    /// Execute commands without starting the interactive REPL.
    #[arg(long, help_heading = "Startup")]
    batch: bool,

    /// Read FILE instead of the user's settings file.
    #[arg(
        long,
        value_name = "FILE",
        hide_short_help = true,
        help_heading = "Settings"
    )]
    config: Option<PathBuf>,

    /// Read no settings files, neither the user's nor the project's.
    #[arg(
        long,
        conflicts_with = "config",
        hide_short_help = true,
        help_heading = "Settings"
    )]
    no_config: bool,

    /// Use the project's startup commands, aliases, and launch
    /// configurations in this session without being asked.
    #[arg(long, hide_short_help = true, help_heading = "Settings")]
    trust_project: bool,

    /// Look up the core dump's recorded module paths inside DIR, a copy of the
    /// files of the machine that wrote it, instead of on this machine.
    #[arg(
        long,
        value_name = "DIR",
        requires = "core",
        hide_short_help = true,
        help_heading = "Core dump files"
    )]
    sysroot: Option<PathBuf>,

    /// Search DIR for core dump modules missing from their recorded paths or
    /// not matching the dump, by file name and then by build-id. May be repeated.
    #[arg(
        long = "module-path",
        value_name = "DIR",
        requires = "core",
        hide_short_help = true,
        help_heading = "Core dump files"
    )]
    module_paths: Vec<PathBuf>,

    /// Use module files that cannot be proven to match the core dump.
    #[arg(
        long,
        requires = "core",
        hide_short_help = true,
        help_heading = "Core dump files"
    )]
    allow_module_mismatch: bool,

    /// Read source files recorded under FROM from TO instead, such as for a
    /// program built elsewhere. May be repeated; earlier rules are tried first.
    #[arg(
        long,
        num_args = 2,
        value_names = ["FROM", "TO"],
        hide_short_help = true,
        help_heading = "Debug information"
    )]
    source_map: Vec<PathBuf>,

    /// Search DIR for the separate debug files of modules stripped of their
    /// debug information, before the system's directories. May be repeated.
    #[arg(
        long = "debug-directory",
        value_name = "DIR",
        hide_short_help = true,
        help_heading = "Debug information"
    )]
    debug_directories: Vec<PathBuf>,

    /// Download debug files no directory holds from the debuginfod servers
    /// `DEBUGINFOD_URLS` lists.
    #[arg(long, hide_short_help = true, help_heading = "Debug information")]
    debuginfod: bool,

    /// Read debug information afresh, neither reading nor writing the image
    /// cache in `MICROSCOPE_CACHE_DIR` or `~/.cache/microscope/images`.
    #[arg(long, hide_short_help = true, help_heading = "Debug information")]
    no_cache: bool,

    /// Present values with the views in FILE, ahead of the project's, the
    /// user's, the program's own, and the built-in ones. May be repeated;
    /// later files come first.
    #[arg(
        long = "views",
        value_name = "FILE",
        hide_short_help = true,
        help_heading = "Debug information"
    )]
    views: Vec<PathBuf>,

    /// Run the launched program in DIR instead of the current directory.
    #[arg(
        long,
        value_name = "DIR",
        conflicts_with_all = ["attach", "core"],
        hide_short_help = true,
        help_heading = "Launch environment"
    )]
    cwd: Option<PathBuf>,

    /// Set NAME to VALUE in the launched program's environment. May be repeated.
    #[arg(
        long = "env",
        value_name = "NAME=VALUE",
        value_parser = parse_environment_variable,
        conflicts_with_all = ["attach", "core"],
        hide_short_help = true,
        help_heading = "Launch environment"
    )]
    environment: Vec<(OsString, OsString)>,

    /// Arguments passed to the launched program.
    #[arg(
        last = true,
        value_name = "ARGS",
        conflicts_with_all = ["attach", "core"],
        hide_short_help = true,
        help_heading = "Launch environment"
    )]
    arguments: Vec<OsString>,

    /// Control colored terminal output, over [ui] color.
    #[arg(long, value_enum, hide_short_help = true, help_heading = "Display")]
    color: Option<ColorChoice>,

    /// The assembly syntax `disassemble` renders, over [disassembly] syntax.
    #[arg(long, value_enum, hide_short_help = true, help_heading = "Display")]
    disassembly_syntax: Option<DisassemblySyntax>,

    /// Write where the session's time and memory went to FILE: JSON with a
    /// summary, totals per phase, and Chrome trace events, which
    /// ui.perfetto.dev opens.
    #[arg(
        long,
        value_name = "FILE",
        hide_short_help = true,
        help_heading = "Diagnostics"
    )]
    timings: Option<PathBuf>,

    #[command(subcommand)]
    tool: Option<Tool>,
}

#[derive(Subcommand)]
enum Tool {
    /// Serve the Debug Adapter Protocol to an editor.
    #[command(version)]
    Dap(dap::DapArgs),

    /// Check and explain how views present program types.
    Views(ViewsArgs),

    /// Serve a debugger to web browsers.
    Web(Box<web::WebArgs>),

    /// Show, check, and create settings files, and trust projects.
    Config(ConfigArgs),

    /// Record where a program spends its time, from outside it.
    Record(Box<cli::profile::RecordArgs>),

    /// Summarize a recorded profile.
    Report(cli::profile::ReportArgs),

    /// Write a profile in another format: pprof, folded stacks, or a
    /// Chrome trace, or read one of theirs.
    Convert(cli::profile::ConvertArgs),

    /// Give a profile recorded with `--symbols later` its source lines.
    Symbolize(cli::profile::SymbolizeArgs),

    /// Trace the system calls a program makes, as strace does.
    Syscalls(Box<cli::syscalls::SyscallsArgs>),
}

fn parse_environment_variable(text: &str) -> std::result::Result<(OsString, OsString), String> {
    match text.split_once('=') {
        Some((name, value)) if !name.is_empty() => Ok((name.into(), value.into())),
        _ => Err(format!("expected NAME=VALUE, found '{text}'")),
    }
}

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

    match tokio::runtime::Runtime::new() {
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
        Ok(()) => ExitCode::SUCCESS,
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

fn parse_args() -> Args {
    let mut arguments = std::env::args_os().collect::<Vec<_>>();

    // Alone, microscope starts the project's launch configuration, or, with
    // none, shows its short help.
    if arguments.len() == 1 && !project_launches() {
        arguments.push("-h".into());
    }

    let color = cli::help::color_choice(&arguments);
    let command = cli::help::configure(Args::command(), color);
    let matches = command.get_matches_from(arguments);
    Args::from_arg_matches(&matches).unwrap_or_else(|error| error.exit())
}

/// Whether the project of the working directory describes how to start
/// its program, so that `microscope` alone starts it. Settings that cannot be
/// read describe nothing here; starting reports why.
fn project_launches() -> bool {
    let Ok(start) = std::env::current_dir() else {
        return false;
    };

    let user = cli::config::UserFile::choose(false, None);
    cli::config::Files::read(&start, &user)
        .and_then(|files| files.settings(true))
        .is_ok_and(|settings| !settings.config.launch.is_empty())
}

/// Ends `recording` and writes what it collected to `path`.
fn write_timings(recording: microscope::timings::Recording, path: &std::path::Path) -> Result<()> {
    std::fs::write(path, recording.finish()?.to_json()?)?;
    Ok(())
}

async fn run(session: Session, renderers: Renderers) -> Result<()> {
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

    let result = cli.run(&session).await;
    let shutdown = opened
        .shutdown()
        .await
        .context("failed to shut down debugger");

    result?;
    shutdown
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

/// Checks which views present a program's types, from its debug
/// information alone.
#[derive(clap::Args)]
struct ViewsArgs {
    #[command(subcommand)]
    command: ViewsCommand,
}

#[derive(clap::Subcommand)]
enum ViewsCommand {
    /// Report which view presents each of the program's types that a view's
    /// pattern names, and the loaded views that present no type. Fails
    /// when a view file, or a view loaded for the session or carried by
    /// the program, cannot be used.
    Check {
        /// The program whose types to check.
        program: PathBuf,

        /// Also present values with the views in FILE. May be repeated.
        #[arg(long = "views", value_name = "FILE")]
        views: Vec<PathBuf>,
    },

    /// Explain which view presents a type of the program, and why each
    /// view tried before it did not bind.
    Explain {
        /// The program that defines the type.
        program: PathBuf,

        /// The type, as an expression names one: `intvec`, `std::vector<int>`.
        #[arg(value_name = "TYPE")]
        name: String,

        /// Also present values with the views in FILE. May be repeated.
        #[arg(long = "views", value_name = "FILE")]
        views: Vec<PathBuf>,
    },

    /// Replay kernel runs that `views record` recorded, with no program.
    /// Fails unless every run does again what it did.
    Replay {
        /// The recorded runs.
        recording: PathBuf,

        /// Replay with the kernel module in FILE, rather than the built-in
        /// kernel each run names.
        #[arg(long = "kernel", value_name = "FILE")]
        kernel: Option<PathBuf>,
    },
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

/// Shows, checks, and creates settings files, and trusts projects.
#[derive(clap::Args)]
struct ConfigArgs {
    /// Read FILE instead of the user's settings file.
    #[arg(long, value_name = "FILE", global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(clap::Subcommand)]
enum ConfigCommand {
    /// Print the project root and the settings and state files a session
    /// in this directory reads, and which exist.
    Path,

    /// Print every setting in effect and where it comes from.
    Show,

    /// Check every settings file, failing at the first error, as for a
    /// project's CI.
    Check,

    /// Write the user's settings file with every setting at its default,
    /// commented out. Refuses to overwrite one.
    Init,

    /// Trust the project's startup commands, aliases, and launch
    /// configurations until they change.
    Trust,

    /// Forget that the project is trusted.
    Untrust,

    /// List the trusted projects.
    Trusted,
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
