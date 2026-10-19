//! Settings files: where they are found, how they layer, how strictly they
//! are read, launch configurations, and trusting a project.

mod support;

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const BASIC: &str = "build/test-programs/basic";
const ENVIRONMENT: &str = "build/test-programs/process-environment";

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(name)
}

/// A scratch project with a user settings file beside it, and a state
/// directory for what microscope records.
struct Project {
    directory: support::ScratchDir,
}

impl Project {
    fn new(name: &str) -> Self {
        let directory = support::ScratchDir::new(name);
        fs::create_dir_all(directory.path().join("project/.microscope")).expect("make the project");
        fs::write(directory.path().join("user.toml"), "").expect("write the user's file");
        Self { directory }
    }

    fn root(&self) -> PathBuf {
        self.directory.path().join("project")
    }

    fn user(&self, text: &str) {
        fs::write(self.directory.path().join("user.toml"), text).expect("write the user's file");
    }

    fn project(&self, text: &str) {
        fs::write(self.root().join(".microscope/config.toml"), text)
            .expect("write the project's file");
    }

    fn local(&self, text: &str) {
        fs::write(self.root().join(".microscope/config.local.toml"), text)
            .expect("write the local file");
    }

    /// microscope run in the project, with its own user file and state.
    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_microscope"));
        command
            .args(arguments)
            .current_dir(self.root())
            .env("MICROSCOPE_CONFIG", self.directory.path().join("user.toml"))
            .env("XDG_STATE_HOME", self.directory.path().join("state"))
            .env_remove("NO_COLOR")
            .env_remove("CLICOLOR")
            .env_remove("CLICOLOR_FORCE");
        command
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.command(arguments)
            .stdin(Stdio::null())
            .output()
            .expect("run microscope")
    }
}

fn succeeded(output: &Output) -> (String, String) {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "microscope failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    (stdout, stderr)
}

fn failed(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !output.status.success(),
        "microscope succeeded:\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    stderr
}

/// Asserts each expected text appears, in order, after the previous one.
fn assert_in_order(output: &str, expected: &[&str]) {
    let mut rest = output;
    for text in expected {
        let found = rest
            .find(text)
            .unwrap_or_else(|| panic!("{text:?} does not follow in:\n{output}"));
        rest = &rest[found + text.len()..];
    }
}

/// `output` without the "(ran 1.42s)" a stop after a slow run ends with.
fn without_elapsed(output: &str) -> String {
    let mut kept = String::new();
    let mut rest = output;
    while let Some(start) = rest.find(" (ran ") {
        kept.push_str(&rest[..start]);
        rest = &rest[start..];
        rest = rest.find(')').map_or("", |end| &rest[end + 1..]);
    }

    kept.push_str(rest);
    kept
}

fn show_line<'a>(shown: &'a str, key: &str) -> &'a str {
    shown
        .lines()
        .find(|line| line.starts_with(&format!("{key} = ")))
        .unwrap_or_else(|| panic!("no {key} in:\n{shown}"))
}

/// Each setting comes from the first of a flag, the environment, the local
/// file, the project's, the user's, and the default that sets it, and
/// tables merge key by key.
#[test]
fn settings_layer_flags_environment_local_project_user_and_defaults() {
    let project = Project::new("config-precedence");
    project.user(
        "[ui]\ncolor = \"always\"\n[source]\ncontext = [1, 1]\n\
         [disassembly]\nsyntax = \"att\"\n[print]\nmax-depth = 9\n",
    );
    project.project("[source]\ncontext = [2, 2]\n[disassembly]\nshow-bytes = false\n");
    project.local("[source]\ncontext = [0, 0]\n");
    let basic = fixture(BASIC);
    let basic = basic.to_str().expect("UTF-8 path");

    let session = |extra: &[&str], no_color: bool| {
        let mut arguments = vec!["--batch", "-e", "break main", "-e", "run", "-e", "list"];
        arguments.extend(["-e", "disassemble main 1"]);
        arguments.extend_from_slice(extra);
        arguments.push(basic);
        let mut command = project.command(&arguments);
        if no_color {
            command.env("NO_COLOR", "1");
        }

        succeeded(
            &command
                .stdin(Stdio::null())
                .output()
                .expect("run microscope"),
        )
        .0
    };

    // The local file's context, the project's hidden bytes, and the flag's
    // syntax over the user's.
    let plain = session(&["--disassembly-syntax", "intel"], true);

    // After the breakpoint's line and the stop's header, `list`'s. A stop
    // after a run of a second or more, as under load, says how long it ran.
    let plain = without_elapsed(&plain);
    let listing = plain
        .split("basic.c:10\n")
        .nth(3)
        .expect("the listing after the stop's");

    // The line has a breakpoint, which the margin marks.
    assert!(
        listing
            .trim_start_matches(['+', '\u{25cf}'])
            .starts_with("=> 10 |"),
        "{plain}"
    );
    assert!(
        !listing.contains("   9 |") && !listing.contains("  11 |"),
        "{plain}"
    );
    assert!(plain.contains("<main>: push rbp"), "{plain}");

    // NO_COLOR outranks the user's `color = "always"`, which applies
    // without it, even in batch mode.
    assert!(!plain.contains("\x1b["), "{plain}");
    let colored = session(&[], false);
    assert!(colored.contains("\x1b["), "{colored}");
    assert!(colored.contains("%rbp"), "{colored}");

    let (shown, _) = succeeded(&project.run(&["config", "show"]));
    let root = project.root();
    let user = project.directory.path().join("user.toml");
    for (key, origin) in [
        (
            "source.context",
            format!(
                "local {}",
                root.join(".microscope/config.local.toml").display()
            ),
        ),
        (
            "disassembly.show-bytes",
            format!("project {}", root.join(".microscope/config.toml").display()),
        ),
        ("print.max-depth", format!("user {}", user.display())),
        ("disassembly.syntax", format!("user {}", user.display())),
        ("ui.prompt", "default".to_owned()),
    ] {
        let line = show_line(&shown, key);
        assert!(line.ends_with(&format!("# {origin}")), "{line}");
    }

    assert!(
        show_line(&shown, "source.context").contains("[0, 0]"),
        "{shown}"
    );
}

/// An invalid file stops startup with its position and the nearest valid
/// key, `--no-config` starts anyway, and `config check` fails the same
/// way; `config init` writes a valid file once.
#[test]
fn invalid_settings_stop_startup_at_their_position_with_a_suggestion() {
    let project = Project::new("config-errors");
    project.project("[ui]\nprompt = \"> \"\ncolour = \"never\"\n");
    let file = project.root().join(".microscope/config.toml");
    let expected = format!(
        "{}:3:1: unknown key 'colour' in [ui]; did you mean 'color'?",
        file.display()
    );
    let basic = fixture(BASIC);
    let basic = basic.to_str().expect("UTF-8 path");

    let stderr = failed(&project.run(&["--batch", "-e", "quit", basic]));
    assert!(stderr.contains(&expected), "{stderr}");
    assert!(stderr.contains("--no-config"), "{stderr}");
    succeeded(&project.run(&["--no-config", "--batch", "-e", "quit", basic]));
    let stderr = failed(&project.run(&["config", "check"]));
    assert!(stderr.contains(&expected), "{stderr}");

    project.project("");
    fs::remove_file(project.directory.path().join("user.toml")).expect("remove the user's file");
    let (written, _) = succeeded(&project.run(&["config", "init"]));
    assert!(written.starts_with("wrote "), "{written}");
    let (checked, _) = succeeded(&project.run(&["config", "check"]));
    assert!(checked.contains("user.toml: ok"), "{checked}");
    let stderr = failed(&project.run(&["config", "init"]));
    assert!(stderr.contains("already exists"), "{stderr}");
}

/// A launch configuration starts by name, with its arguments, environment,
/// directory, and startup commands; the command line overrides them; and
/// with several, microscope alone lists them rather than choosing.
#[test]
fn launch_configurations_start_by_name_and_are_never_guessed() {
    let project = Project::new("config-launch");
    fs::create_dir_all(project.root().join("work")).expect("make the program's directory");
    let program = fixture(ENVIRONMENT);
    project.project(&format!(
        "[[launch]]\nname = \"environment\"\nprogram = \"{}\"\n\
         args = [\"--not-an-option\", \"two words\"]\nenv = {{ MICROSCOPE_FIXTURE_VALUE = \"a=b\" }}\n\
         cwd = \"work\"\nstartup = [\"run\"]\n",
        program.display()
    ));
    let (stdout, _) = succeeded(&project.run(&["--batch", "--trust-project", "-l", "environment"]));
    let work = project
        .root()
        .join("work")
        .canonicalize()
        .expect("canonical directory");
    assert!(
        stdout.contains(&format!(
            "argument 1: --not-an-option\nargument 2: two words\nvalue: a=b\nremoved: absent\ndirectory: {}\n",
            work.display()
        )),
        "{stdout}"
    );

    // The only configuration starts alone, and the command line's
    // arguments and environment replace and override its own.
    let (stdout, _) = succeeded(&project.run(&[
        "--batch",
        "--trust-project",
        "--env",
        "MICROSCOPE_FIXTURE_VALUE=c",
        "--",
        "one",
    ]));
    assert!(stdout.contains("argument 1: one\nvalue: c\n"), "{stdout}");

    project.local(&format!(
        "[[launch]]\nname = \"basic\"\nprogram = \"{}\"\n",
        fixture(BASIC).display()
    ));
    let stderr = failed(&project.run(&["--batch", "--trust-project"]));
    assert!(
        stderr.contains("the project has 2 launch configurations; choose one with --launch NAME:\n  environment\n  basic"),
        "{stderr}"
    );
    let stderr = failed(&project.run(&["--batch", "--trust-project", "-l", "enviroment"]));
    assert!(
        stderr
            .contains("no launch configuration is named 'enviroment'; did you mean 'environment'?"),
        "{stderr}"
    );
}

/// Startup commands run from every file in order, then the launch
/// configuration's, then `-e`, after `[signals]` applies, and an alias
/// stands for its command.
#[test]
fn startup_commands_accumulate_from_every_file_in_order() {
    let project = Project::new("config-startup");
    project.user(
        "[signals]\nSIGUSR1 = \"nostop noprint\"\n[startup]\ncommands = [\"handle SIGUSR1\"]\n\
         [aliases]\nsignal = \"handle\"\n",
    );
    project.project(&format!(
        "[startup]\ncommands = [\"handle SIGUSR2\"]\n\
         [[launch]]\nname = \"basic\"\nprogram = \"{}\"\nstartup = [\"handle SIGTERM\"]\n",
        fixture(BASIC).display()
    ));
    project.local("[startup]\ncommands = [\"handle SIGHUP\"]\n");

    let (stdout, _) =
        succeeded(&project.run(&["--batch", "--trust-project", "-e", "signal SIGQUIT"]));
    assert_in_order(
        &stdout,
        &[
            "SIGUSR1   no    no     yes",
            "SIGUSR2",
            "SIGHUP",
            "SIGTERM",
            "SIGQUIT",
        ],
    );
}

/// A project's startup commands, aliases, and launch configurations apply
/// only once trusted: a session that cannot ask fails and says how, the
/// flag trusts it for one session, `config trust` until they change, and
/// `trust = "never"` leaves them out and says so.
#[test]
fn a_project_acts_only_once_trusted() {
    let project = Project::new("config-trust");
    project.project("[startup]\ncommands = [\"handle SIGUSR2\"]\n[ui]\nprompt = \"> \"\n");
    let basic = fixture(BASIC);
    let basic = basic.to_str().expect("UTF-8 path");
    let session = ["--batch", "-e", "quit", basic];

    let stderr = failed(&project.run(&session));
    assert!(stderr.contains("have not been trusted"), "{stderr}");
    assert!(
        stderr.contains("commands = [\"handle SIGUSR2\"]"),
        "{stderr}"
    );
    assert!(stderr.contains("--trust-project"), "{stderr}");
    assert!(stderr.contains("microscope config trust"), "{stderr}");

    let (stdout, _) = succeeded(&project.run(&[&["--trust-project"][..], &session].concat()));
    assert!(stdout.contains("SIGUSR2"), "{stdout}");

    let (trusted, _) = succeeded(&project.run(&["config", "trust"]));
    assert!(trusted.contains("handle SIGUSR2"), "{trusted}");
    let (listed, _) = succeeded(&project.run(&["config", "trusted"]));
    assert!(
        listed.contains(&project.root().display().to_string()),
        "{listed}"
    );
    let (stdout, _) = succeeded(&project.run(&session));
    assert!(stdout.contains("SIGUSR2"), "{stdout}");

    // A presentation setting changes without asking again; a command does.
    project.project("[startup]\ncommands = [\"handle SIGUSR2\"]\n[ui]\nprompt = \">> \"\n");
    succeeded(&project.run(&session));
    project.project("[startup]\ncommands = [\"handle SIGHUP\"]\n");
    let stderr = failed(&project.run(&session));
    assert!(stderr.contains("handle SIGHUP"), "{stderr}");

    project.user("[projects]\ntrust = \"never\"\n");
    let (stdout, stderr) = succeeded(&project.run(&session));
    assert!(!stdout.contains("SIGHUP"), "{stdout}");
    assert!(
        stderr.contains(
            "left out the startup commands, aliases, and launch configurations of the project"
        ),
        "{stderr}"
    );

    // A project cannot trust itself.
    project.project("[projects]\ntrust = \"always\"\n");
    let stderr = failed(&project.run(&session));
    assert!(
        stderr.contains("may be set only in the user's file"),
        "{stderr}"
    );
}

/// Asked in a terminal, `yes` trusts the project until its commands
/// change, so the next session does not ask.
#[test]
fn an_interactive_session_asks_once_whether_to_trust_a_project() {
    use expectrl::Expect as _;
    let project = Project::new("config-ask");
    project.project("[aliases]\nbm = \"break main\"\n");
    let start = |project: &Project| {
        let mut command = project.command(&[fixture(BASIC).to_str().expect("UTF-8 path")]);
        command.env("TERM", "dumb");
        let mut session = expectrl::session::Session::spawn(command).expect("spawn in a PTY");
        session.set_expect_timeout(Some(std::time::Duration::from_secs(5)));
        session
    };

    let mut session = start(&project);
    session
        .expect("bm = \"break main\"")
        .expect("the shown alias");
    session.expect("[y/o/N]: ").expect("the question");
    session.send_line("yes").expect("answer");
    session.expect("(microscope) ").expect("the prompt");
    session.send_line("quit").expect("quit");
    session.expect(expectrl::Eof).expect("exit");

    let record = fs::read_to_string(project.directory.path().join("state/microscope/trust.toml"))
        .expect("the trust record");
    assert!(
        record.contains("bm = \\\"break main\\\"") || record.contains("bm = \"break main\""),
        "{record}"
    );
    let mut session = start(&project);
    session
        .expect("(microscope) ")
        .expect("the prompt, without asking");
    session.send_line("quit").expect("quit");
    session.expect(expectrl::Eof).expect("exit");
}

/// `[step] runtime = "enter"` makes `step` stop in a runtime's own code,
/// here tokio's receive, and `set step-runtime off` makes the next step go
/// over it again.
#[test]
fn the_step_setting_enters_the_runtime_until_turned_off() {
    let project = Project::new("config-step-runtime");
    project.project("[step]\nruntime = \"enter\"\n");
    let shapes = fixture("build/test-programs/tokio-shapes-o0");
    let source = "tests/fixtures/rust/tokio/shapes/src/main.rs";

    // The first receive's marker is also the start of the second's.
    let at = |marker: &str| format!("break {source}:{}", support::source_line(source, marker));
    let (first, again) = (at("// INTO: recv"), at("// INTO: recv-again"));
    let output = project.run(&[
        "--batch",
        "-e",
        &first,
        "-e",
        &again,
        "-e",
        "run",
        "-e",
        "step",
        "-e",
        "set step-runtime off",
        "-e",
        "continue",
        "-e",
        "step",
        shapes.to_str().expect("UTF-8 path"),
    ]);
    let (stdout, _) = succeeded(&output);
    let after = support::source_line(source, "// STEP: recv-again-after");
    assert_in_order(
        &stdout,
        &[
            "stopped after source step in recv at ",
            "sync/mpsc/bounded.rs:",
            "steps pass over runtime code",
            "stopped at breakpoint 2",
            "stopped after source step in shapes at ",
            &format!("{source}:{after}"),
        ],
    );
}
