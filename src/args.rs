//! Command-line arguments and parsing for the microscope binary.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::cli::DisassemblySyntax;
use crate::cli::terminal::ColorChoice;
use crate::{cli, dap, web};

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
pub struct Args {
    /// Native executable to launch.
    ///
    /// With --attach or --core, the executable is normally discovered but
    /// can be supplied when automatic discovery is unavailable.
    #[arg(value_name = "EXECUTABLE", help_heading = "Target")]
    pub executable: Option<PathBuf>,

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
    pub attach: Option<String>,

    /// Inspect a core dump.
    ///
    /// The executable recorded in the dump is used by default.
    #[arg(long, value_name = "CORE", help_heading = "Target")]
    pub core: Option<PathBuf>,

    /// Which processes the program starts are debugged too, over [follow]:
    /// `auto`, `all`, or `none`, or a program to debug by file name, as
    /// `node` or 'py*'. May be repeated.
    #[arg(
        long = "follow",
        value_name = "MODE|PROGRAM",
        conflicts_with = "core",
        help_heading = "Target"
    )]
    pub follow: Vec<String>,

    /// Start the project's launch configuration NAME, from its
    /// .microscope/config.toml. A project with one starts it by default.
    #[arg(
        short = 'l',
        long,
        value_name = "NAME",
        conflicts_with_all = ["executable", "attach", "core"],
        help_heading = "Target"
    )]
    pub launch: Option<String>,

    /// Execute commands from a file. May be repeated.
    #[arg(
        short = 'c',
        long = "command",
        value_name = "FILE",
        help_heading = "Startup"
    )]
    pub command_files: Vec<PathBuf>,

    /// Execute one command. May be repeated.
    #[arg(
        short = 'e',
        long = "eval",
        value_name = "COMMAND",
        help_heading = "Startup"
    )]
    pub commands: Vec<String>,

    /// Execute commands without starting the interactive REPL.
    #[arg(long, help_heading = "Startup")]
    pub batch: bool,

    /// Read FILE instead of the user's settings file.
    #[arg(
        long,
        value_name = "FILE",
        hide_short_help = true,
        help_heading = "Settings"
    )]
    pub config: Option<PathBuf>,

    /// Read no settings files, neither the user's nor the project's.
    #[arg(
        long,
        conflicts_with = "config",
        hide_short_help = true,
        help_heading = "Settings"
    )]
    pub no_config: bool,

    /// Use the project's startup commands, aliases, and launch
    /// configurations in this session without being asked.
    #[arg(long, hide_short_help = true, help_heading = "Settings")]
    pub trust_project: bool,

    /// Look up the core dump's recorded module paths inside DIR, a copy of the
    /// files of the machine that wrote it, instead of on this machine.
    #[arg(
        long,
        value_name = "DIR",
        requires = "core",
        hide_short_help = true,
        help_heading = "Core dump files"
    )]
    pub sysroot: Option<PathBuf>,

    /// Search DIR for core dump modules missing from their recorded paths or
    /// not matching the dump, by file name and then by build-id. May be repeated.
    #[arg(
        long = "module-path",
        value_name = "DIR",
        requires = "core",
        hide_short_help = true,
        help_heading = "Core dump files"
    )]
    pub module_paths: Vec<PathBuf>,

    /// Use module files that cannot be proven to match the core dump.
    #[arg(
        long,
        requires = "core",
        hide_short_help = true,
        help_heading = "Core dump files"
    )]
    pub allow_module_mismatch: bool,

    /// Read source files recorded under FROM from TO instead, such as for a
    /// program built elsewhere. May be repeated; earlier rules are tried first.
    #[arg(
        long,
        num_args = 2,
        value_names = ["FROM", "TO"],
        hide_short_help = true,
        help_heading = "Debug information"
    )]
    pub source_map: Vec<PathBuf>,

    /// Search DIR for the separate debug files of modules stripped of their
    /// debug information, before the system's directories. May be repeated.
    #[arg(
        long = "debug-directory",
        value_name = "DIR",
        hide_short_help = true,
        help_heading = "Debug information"
    )]
    pub debug_directories: Vec<PathBuf>,

    /// Download debug files no directory holds from the debuginfod servers
    /// `DEBUGINFOD_URLS` lists.
    #[arg(long, hide_short_help = true, help_heading = "Debug information")]
    pub debuginfod: bool,

    /// Read debug information afresh, neither reading nor writing the image
    /// cache in `MICROSCOPE_CACHE_DIR` or `~/.cache/microscope/images`.
    #[arg(long, hide_short_help = true, help_heading = "Debug information")]
    pub no_cache: bool,

    /// Present values with the views in FILE, ahead of the project's, the
    /// user's, the program's own, and the built-in ones. May be repeated;
    /// later files come first.
    #[arg(
        long = "views",
        value_name = "FILE",
        hide_short_help = true,
        help_heading = "Debug information"
    )]
    pub views: Vec<PathBuf>,

    /// Run the launched program in DIR instead of the current directory.
    #[arg(
        long,
        value_name = "DIR",
        conflicts_with_all = ["attach", "core"],
        hide_short_help = true,
        help_heading = "Launch environment"
    )]
    pub cwd: Option<PathBuf>,

    /// Set NAME to VALUE in the launched program's environment. May be repeated.
    #[arg(
        long = "env",
        value_name = "NAME=VALUE",
        value_parser = parse_environment_variable,
        conflicts_with_all = ["attach", "core"],
        hide_short_help = true,
        help_heading = "Launch environment"
    )]
    pub environment: Vec<(OsString, OsString)>,

    /// Arguments passed to the launched program.
    #[arg(
        last = true,
        value_name = "ARGS",
        conflicts_with_all = ["attach", "core"],
        hide_short_help = true,
        help_heading = "Launch environment"
    )]
    pub arguments: Vec<OsString>,

    /// Control colored terminal output, over [ui] color.
    #[arg(long, value_enum, hide_short_help = true, help_heading = "Display")]
    pub color: Option<ColorChoice>,

    /// The assembly syntax `disassemble` renders, over [disassembly] syntax.
    #[arg(long, value_enum, hide_short_help = true, help_heading = "Display")]
    pub disassembly_syntax: Option<DisassemblySyntax>,

    /// Write where the session's time and memory went to FILE: JSON with a
    /// summary, totals per phase, and Chrome trace events, which
    /// ui.perfetto.dev opens.
    #[arg(
        long,
        value_name = "FILE",
        hide_short_help = true,
        help_heading = "Diagnostics"
    )]
    pub timings: Option<PathBuf>,

    #[command(subcommand)]
    pub tool: Option<Tool>,
}

#[derive(Subcommand)]
pub enum Tool {
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

pub fn parse_environment_variable(text: &str) -> std::result::Result<(OsString, OsString), String> {
    match text.split_once('=') {
        Some((name, value)) if !name.is_empty() => Ok((name.into(), value.into())),
        _ => Err(format!("expected NAME=VALUE, found '{text}'")),
    }
}

/// Checks which views present a program's types, from its debug
/// information alone.
#[derive(clap::Args)]
pub struct ViewsArgs {
    #[command(subcommand)]
    pub command: ViewsCommand,
}

#[derive(clap::Subcommand)]
pub enum ViewsCommand {
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

/// Shows, checks, and creates settings files, and trusts projects.
#[derive(clap::Args)]
pub struct ConfigArgs {
    /// Read FILE instead of the user's settings file.
    #[arg(long, value_name = "FILE", global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(clap::Subcommand)]
pub enum ConfigCommand {
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

pub fn parse_args() -> Args {
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
