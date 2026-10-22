//! What a command-line session debugs and how it starts: the settings in
//! effect, whether the project is trusted, and the target that the command
//! line or a launch configuration names.

use std::ffi::OsString;
use std::io::{self, IsTerminal as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use microscope::{CoreDumpOptions, ProcessId, SourcePathMap};

use super::LaunchSettings;
use super::config::{
    self, Answer, AttachTarget, Files, Launch, LaunchTarget, Origin, Settings, StartupCommand,
    Trust, TrustStore, UserFile,
};

use super::terminal::{ColorEnvironment, Renderer, Role};
use crate::Args;

/// What a session opens.
pub enum Target {
    Program(PathBuf),
    Attach {
        process: ProcessId,
        executable: Option<PathBuf>,
    },
    Core(CoreDumpOptions),
}

/// Everything a command-line session starts with.
pub struct Session {
    pub settings: Settings,
    pub target: Target,

    /// How `run` launches the program.
    pub launch: LaunchSettings,

    /// The settings files' startup commands, then the launch
    /// configuration's.
    pub startup: Vec<StartupCommand>,
    pub command_files: Vec<PathBuf>,
    pub commands: Vec<String>,
    pub batch: bool,

    /// The session's view files, those named last first.
    pub views: Vec<PathBuf>,
    pub source_paths: SourcePathMap,

    /// Where modules' separate debug files are found.
    pub debug_files: microscope::DebugFileOptions,
}

impl Session {
    /// The process a session attached to, for its banner.
    pub const fn attached(&self) -> Option<ProcessId> {
        match self.target {
            Target::Attach { process, .. } => Some(process),
            _ => None,
        }
    }
}

/// Reads the settings, decides whether to trust the project, and resolves
/// the target. `warnings` renders what is said before the settings exist.
/// `settings` with what the command line and environment choose over them.
fn flagged_settings(mut settings: Settings, args: &Args) -> Settings {
    if let Some(choice) = args.color {
        settings.override_color(choice, Origin::Flag("--color"));
    } else if let Some((choice, variable)) = ColorEnvironment::current().choice() {
        settings.override_color(choice, Origin::Environment(variable));
    }

    if let Some(syntax) = args.disassembly_syntax {
        settings.override_syntax(syntax);
    }
    settings.override_follow(&args.follow);
    settings
}

pub fn prepare(args: &Args, warnings: Renderer) -> Result<Session> {
    let start = std::env::current_dir().context("cannot read the working directory")?;
    let files = read_files(&start, args)?;
    let interactive = !args.batch && io::stdin().is_terminal() && io::stdout().is_terminal();
    let trusted = decide_trust(&files, args.trust_project, interactive, warnings)?;
    let settings = flagged_settings(files.settings(trusted)?, args);

    let mut startup = settings.startup.clone();
    let mut views = Vec::new();
    let mut launch = LaunchSettings {
        arguments: args.arguments.clone(),
        environment: args
            .environment
            .iter()
            .map(|(name, value)| (name.clone(), Some(value.clone())))
            .collect(),
        working_directory: args.cwd.clone(),
    };

    let explicit = args.executable.is_some() || args.attach.is_some() || args.core.is_some();
    let chosen = match (&args.launch, explicit) {
        (Some(name), _) => Some(chosen_launch(&settings, &files, trusted, name)?),
        (None, true) => None,
        (None, false) => match settings.config.launch.as_slice() {
            [] => bail!(
                "nothing to debug: name an EXECUTABLE, --attach PID, or --core CORE, or describe the program in a launch configuration"
            ),
            [only] => Some(only),
            several => bail!(
                "the project has {} launch configurations; choose one with --launch NAME:\n{}",
                several.len(),
                several
                    .iter()
                    .map(|launch| format!("  {}", launch.name))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        },
    };

    let target = match chosen {
        Some(chosen) => {
            let origin = settings.origin(&format!("launch.{}", chosen.name));
            for (index, text) in chosen.startup.iter().enumerate() {
                startup.push(StartupCommand {
                    label: format!("launch '{}' startup command {}", chosen.name, index + 1),
                    origin: origin.clone(),
                    text: text.clone(),
                });
            }

            views.extend(chosen.views.iter().map(|path| settings.root.join(path)));
            launch_target(chosen, &settings.root, args, &mut launch)?
        }
        None => explicit_target(args)?,
    };

    views.extend(args.views.iter().cloned());

    let mut source_paths = SourcePathMap::new();
    for [from, to] in args.source_map.as_chunks::<2>().0 {
        source_paths
            .push(from, to)
            .context("invalid --source-map rule")?;
    }

    for rule in &settings.config.source_map {
        source_paths
            .push(&rule.from, settings.root.join(&rule.to))
            .context("invalid [[source-map]] rule")?;
    }

    let debug_info = &settings.config.debug_info;
    let debug_files = microscope::DebugFileOptions {
        directories: args
            .debug_directories
            .iter()
            .cloned()
            .chain(
                debug_info
                    .directories
                    .iter()
                    .map(|directory| settings.root.join(directory)),
            )
            .collect(),
        debuginfod: args.debuginfod || debug_info.debuginfod,
        ..microscope::DebugFileOptions::default()
    };

    microscope::pool::configure(None, debug_info.jobs).context("cannot load debug information")?;

    Ok(Session {
        debug_files,
        settings,
        target,
        launch,
        startup,
        command_files: args.command_files.clone(),
        commands: args.commands.clone(),
        batch: args.batch,
        views,
        source_paths,
    })
}

/// Reads the settings files a session's flags choose.
pub fn read_files(start: &Path, args: &Args) -> Result<Files> {
    Files::read(
        start,
        &UserFile::choose(args.no_config, args.config.as_deref()),
    )
    .map_err(|error| anyhow!("{error}\n(--no-config starts without settings files)"))
}

/// The launch configuration `--launch` names.
fn chosen_launch<'a>(
    settings: &'a Settings,
    files: &Files,
    trusted: bool,
    name: &str,
) -> Result<&'a Launch> {
    settings.launch(name).map_err(|message| {
        if !trusted && files.acting().is_some_and(|acting| acting.contains("launch")) {
            anyhow!(
                "the launch configurations of the project at {} are not trusted; run `microscope config trust` to trust them",
                files.root.display()
            )
        } else {
            anyhow!(message)
        }
    })
}

/// The target a launch configuration names, with the command line's
/// arguments, environment, and directory applied to a launched program.
fn launch_target(
    chosen: &Launch,
    root: &Path,
    args: &Args,
    launch: &mut LaunchSettings,
) -> Result<Target> {
    let launched = |flag| -> Result<()> {
        bail!(
            "{flag} applies only to a launched program, and launch configuration '{}' does not launch one",
            chosen.name
        )
    };

    Ok(match chosen.target(root)? {
        LaunchTarget::Program(program) => {
            if args.arguments.is_empty() {
                launch.arguments = chosen.args.iter().map(OsString::from).collect();
            }

            let mut environment = chosen
                .env
                .iter()
                .map(|(name, value)| (OsString::from(name), Some(OsString::from(value))))
                .collect::<Vec<_>>();
            environment.append(&mut launch.environment);
            launch.environment = environment;

            if launch.working_directory.is_none() {
                launch.working_directory = Some(
                    chosen
                        .cwd
                        .as_ref()
                        .map_or_else(|| root.to_path_buf(), |cwd| root.join(cwd)),
                );
            }

            Target::Program(program)
        }
        other => {
            if !args.arguments.is_empty() {
                launched("an argument after --")?;
            }

            if !args.environment.is_empty() {
                launched("--env")?;
            }

            if args.cwd.is_some() {
                launched("--cwd")?;
            }

            match other {
                LaunchTarget::Attach(target, executable) => Target::Attach {
                    process: attach_process(&target)?,
                    executable,
                },
                LaunchTarget::Core(core, executable) => Target::Core(CoreDumpOptions {
                    core,
                    executable,
                    sysroot: chosen.sysroot.as_ref().map(|path| root.join(path)),
                    module_paths: chosen
                        .module_path
                        .iter()
                        .map(|path| root.join(path))
                        .collect(),
                    allow_module_mismatch: chosen.allow_module_mismatch,
                    debug_files: microscope::DebugFileOptions::default(),
                }),
                LaunchTarget::Program(_) => bail!("a launched program was matched above"),
            }
        }
    })
}

/// The target the command line names.
fn explicit_target(args: &Args) -> Result<Target> {
    if let Some(core) = &args.core {
        return Ok(Target::Core(CoreDumpOptions {
            core: core.clone(),
            executable: args.executable.clone(),
            sysroot: args.sysroot.clone(),
            module_paths: args.module_paths.clone(),
            allow_module_mismatch: args.allow_module_mismatch,
            debug_files: microscope::DebugFileOptions::default(),
        }));
    }

    if let Some(attach) = &args.attach {
        let target = attach
            .parse()
            .map_or_else(|_| AttachTarget::Name(attach.clone()), AttachTarget::Pid);
        return Ok(Target::Attach {
            process: attach_process(&target)?,
            executable: args.executable.clone(),
        });
    }

    let Some(executable) = args.executable.clone() else {
        bail!("the command line names no program, process, or core");
    };

    Ok(Target::Program(executable))
}

/// The process an attach names: its id, or the one process of its name.
fn attach_process(target: &AttachTarget) -> Result<ProcessId> {
    let name = match target {
        AttachTarget::Pid(pid) => return Ok(ProcessId::new(*pid)),
        AttachTarget::Name(name) => name,
    };

    let found = microscope::processes_named(name)?;
    match found.as_slice() {
        [] => bail!("no process matches '{name}'"),
        [process] => Ok(process.process_id),
        several => {
            let width = several
                .iter()
                .map(|process| process.process_id.to_string().len())
                .max()
                .unwrap_or(0);
            let lines: Vec<String> = several
                .iter()
                .map(|process| {
                    let matched = match process.matched {
                        microscope::NameMatch::Command => "command",
                        microscope::NameMatch::Program => "program",
                        microscope::NameMatch::Title => "title",
                        microscope::NameMatch::Script => "script",
                    };
                    format!(
                        "  {:>width$}  {}  ({matched})",
                        process.process_id, process.command_line
                    )
                })
                .collect();
            bail!(
                "{} processes match '{name}'; attach to one by its id:\n{}",
                several.len(),
                lines.join("\n")
            )
        }
    }
}

/// Whether the project's acting settings apply: by the user's policy, the
/// recorded trust, `--trust-project`, or asking. A session that cannot ask
/// fails rather than guess either way.
fn decide_trust(files: &Files, flag: bool, interactive: bool, renderer: Renderer) -> Result<bool> {
    let Some(acting) = files.acting() else {
        return Ok(true);
    };

    let root = &files.root;
    let warn = |message: String| {
        eprintln!("{}: {message}", renderer.paint(Role::Warning, "warning"));
    };

    let left_out = |why: &str| {
        format!(
            "left out the startup commands, aliases, and launch configurations of the project at {}, {why}",
            root.display()
        )
    };

    match files.trust_policy() {
        Trust::Always => return Ok(true),
        Trust::Never => {
            warn(left_out("since [projects] trust is \"never\""));
            return Ok(false);
        }
        Trust::Ask => {}
    }

    if flag {
        return Ok(true);
    }

    let mut store = TrustStore::load()?;
    if store.trusts(root, &acting) {
        return Ok(true);
    }

    if !interactive {
        bail!(
            "the project at {} has startup commands, aliases, or launch configurations that have not been trusted:\n\n{}\nPass --trust-project to use them in this session, or run `microscope config trust` to trust them until they change.",
            root.display(),
            indent(&acting)
        );
    }

    let answer = config::ask_trust(root, &acting, &mut io::stdin().lock(), &mut io::stderr())
        .context("cannot ask whether to trust the project")?;

    match answer {
        Answer::Yes => {
            if let Err(error) = store.trust(root, &acting) {
                warn(format!(
                    "trusted the project for this session only: {error}"
                ));
            }

            Ok(true)
        }
        Answer::Once => Ok(true),
        Answer::No => {
            warn(left_out("since they were not trusted"));
            Ok(false)
        }
    }
}

fn indent(text: &str) -> String {
    text.lines().fold(String::new(), |mut indented, line| {
        indented.push_str("    ");
        indented.push_str(line);
        indented.push('\n');
        indented
    })
}
