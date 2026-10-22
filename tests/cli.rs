mod support;

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use object::{Object, ObjectSection, ObjectSymbol};

const BASIC: &str = "build/test-programs/basic";
const SEGV_CORE: &str = "build/test-programs/crash-gcc-o0-segv.core";
const FRAMES_CORE: &str = "build/test-programs/frames-gcc-o0.core";

/// A profile converts to pprof, which Go's own tool reads, and to folded
/// stacks, each holding every stack's weight; a name with no format is
/// refused.
/// `record --symbols later` keeps frames as addresses. `report` gives
/// them the lines this host's files hold and notes the modules it cannot,
/// and `symbolize` writes in what a debug directory holds.
#[test]
fn symbolize_writes_in_symbols_a_recording_left_for_later() {
    let scratch = support::ScratchDir::new("cli-symbolize");
    let program = scratch.path().join("basic");
    std::fs::copy(
        fixture("build/test-programs/split/basic-build-id.full"),
        &program,
    )
    .expect("copy the program");
    let profile = scratch.path().join("basic.usprof");
    let recorded = microscope_command()
        .args([
            "record",
            "--symbols",
            "later",
            "--function",
            "breakpoint_target",
            "-o",
        ])
        .arg(&profile)
        .arg("--")
        .arg(&program)
        .stdin(Stdio::null())
        .output()
        .expect("run microscope record");
    assert!(recorded.status.success(), "{recorded:?}");
    std::fs::remove_file(&program).expect("delete the program");
    let profile = profile.to_string_lossy().into_owned();

    let unnamed = assert_success(microscope(&["report", &profile]));
    assert!(
        unnamed.contains(
            "basic keeps its addresses: its file is gone, and no debug directory or server has \
             its build id"
        ),
        "{unnamed}"
    );
    assert!(!unnamed.contains("breakpoint_target"), "{unnamed}");

    let root = fixture("build/test-programs/split/debug-root");
    let symbolized = microscope(&[
        "symbolize",
        &profile,
        "--debug-dir",
        &root.to_string_lossy(),
    ]);
    assert!(symbolized.status.success(), "{symbolized:?}");
    let said = String::from_utf8_lossy(&symbolized.stderr);
    assert!(said.contains("basic: 4 frames from "), "{said}");
    let named = assert_success(microscope(&["report", &profile]));
    assert!(!named.contains("keeps its addresses"), "{named}");
    assert_in_order(&named, &["breakpoint_target", "main"]);
}

#[test]
fn convert_writes_what_go_and_flame_graph_tools_read() {
    let scratch = support::ScratchDir::new("cli-convert");
    let profile = scratch.path().join("leaf.usprof");
    let recorded = microscope_command()
        .args(["record", "--function", "leaf", "-o"])
        .arg(&profile)
        .arg("--")
        .arg(fixture("build/test-programs/profile-stacks-gcc-o0"))
        .stdin(Stdio::null())
        .output()
        .expect("run microscope record");
    assert!(recorded.status.success());
    let profile = profile.to_string_lossy().into_owned();

    let pprof = scratch.path().join("leaf.pb.gz");
    let converted = microscope(&["convert", &profile, &pprof.to_string_lossy()]);
    assert!(converted.status.success());
    assert!(String::from_utf8_lossy(&converted.stderr).contains("without times, threads"));

    let raw = Command::new(fixture("build/test-programs/pprof"))
        .args(["-raw", "-symbolize=none"])
        .arg(&pprof)
        .output()
        .expect("run Go's pprof");
    let raw = String::from_utf8_lossy(&raw.stdout).into_owned();
    let samples: u64 = raw
        .lines()
        .skip_while(|line| *line != "executions/count")
        .skip(1)
        .take_while(|line| !line.starts_with("Locations"))
        .filter_map(|line| line.trim().split_once(':')?.0.parse::<u64>().ok())
        .sum();
    assert_eq!(samples, 6, "{raw}");
    assert!(raw.contains(" leaf "), "{raw}");
    let back = assert_success(microscope(&["report", &pprof.to_string_lossy()]));
    assert_in_order(
        &back,
        &[
            "6 executions",
            "which keeps no times or threads",
            "100.00% 100.00%  leaf",
        ],
    );

    let folded = scratch.path().join("leaf.folded");
    assert_success(microscope(&[
        "convert",
        &profile,
        &folded.to_string_lossy(),
    ]));
    let folded = std::fs::read_to_string(&folded).expect("the folded stacks");
    let mut weight = 0;
    for line in folded.lines() {
        let (stack, count) = line.rsplit_once(' ').expect("a weight");
        assert!(stack.ends_with(";leaf"), "{line}");
        weight += count.parse::<u64>().expect("a number");
    }

    assert_eq!(weight, 6, "{folded}");

    let refused = microscope(&["convert", &profile, "out.bin"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("choose one with --to"));
}

/// A Chrome trace holds the program's zones, labels, and marks.
#[test]
fn convert_writes_instrumentation_as_a_chrome_trace() {
    let scratch = support::ScratchDir::new("cli-chrome");
    let profile = scratch.path().join("zones.usprof");
    let recorded = microscope_command()
        .args(["record", "-o"])
        .arg(&profile)
        .arg("--")
        .arg(fixture("build/test-programs/probes"))
        .arg("zones")
        .stdin(Stdio::null())
        .output()
        .expect("run microscope record");
    assert!(recorded.status.success());
    let trace = scratch.path().join("zones.json");
    assert!(
        microscope(&[
            "convert",
            &profile.to_string_lossy(),
            &trace.to_string_lossy()
        ])
        .status
        .success()
    );
    let trace: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&trace).expect("the trace")).expect("JSON");
    let events = trace["traceEvents"].as_array().expect("events");
    let named = |phase: &str, name: &str| {
        events
            .iter()
            .filter(|event| event["ph"] == phase && event["name"] == name)
            .count()
    };

    assert_eq!(named("X", "outer"), 3);
    assert_eq!(named("X", "round=1"), 1);
    assert_eq!(named("i", "done"), 1);
    assert!(
        events
            .iter()
            .all(|event| event["ph"] != "X" || event["dur"].as_f64() >= Some(0.0))
    );
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(name)
}

/// A microscope command that reads the tests' settings file rather than the
/// developer's, so that neither theirs nor their locale can change a test's
/// outcome.
fn microscope_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_microscope"));
    command.env(
        "MICROSCOPE_CONFIG",
        fixture("tests/support/settings/ascii.toml"),
    );
    command
}

/// Runs microscope in the repository with no standard input.
fn microscope(arguments: &[&str]) -> Output {
    microscope_command()
        .args(arguments)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::null())
        .output()
        .expect("run microscope")
}

/// Runs `commands` in batch mode after the other `arguments`.
fn batch_output(arguments: &[&str], commands: &[&str]) -> Output {
    let mut all = vec!["--batch"];
    for command in commands {
        all.extend(["--eval", command]);
    }

    all.extend_from_slice(arguments);
    microscope(&all)
}

/// Runs `commands` in batch mode and returns the output of the session,
/// which must succeed.
fn batch(arguments: &[&str], commands: &[&str]) -> String {
    assert_success(batch_output(arguments, commands))
}

/// Runs `commands` through microscope's standard input, which reports each
/// failed command and carries on, and returns stdout and stderr.
fn piped(arguments: &[&str], commands: &[&str]) -> (String, String) {
    let mut child = microscope_command()
        .args(arguments)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start microscope");
    let mut stdin = child.stdin.take().expect("microscope's stdin");
    for command in commands {
        writeln!(stdin, "{command}").expect("write a command");
    }

    drop(stdin);
    let output = child.wait_with_output().expect("wait for microscope");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    (assert_success(output), stderr)
}

fn assert_success(output: Output) -> String {
    assert!(
        output.status.success(),
        "microscope failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8 output")
}

fn assert_failure(output: &Output, expected: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success() && stderr.contains(expected),
        "expected failure containing {expected:?}:\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
}

fn assert_no_sgr(output: &str) {
    assert!(
        !output.contains("\x1b["),
        "unexpected terminal styling in {output:?}"
    );
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

/// Returns the index of the first output line at or after `from` that
/// satisfies `predicate`.
fn line_index(lines: &[&str], from: usize, predicate: impl Fn(&str) -> bool) -> usize {
    lines[from..]
        .iter()
        .position(|line| predicate(line))
        .map_or_else(
            || panic!("no matching line after {from} in:\n{}", lines.join("\n")),
            |index| from + index,
        )
}

fn symbol_address(executable: &std::path::Path, name: &str) -> u64 {
    let data = fs::read(executable).expect("read fixture");
    let object = object::File::parse(data.as_slice()).expect("parse fixture");

    object
        .symbols()
        .find_map(|symbol| {
            (symbol.name().ok() == Some(name) && symbol.is_definition()).then(|| symbol.address())
        })
        .unwrap_or_else(|| panic!("fixture has no defined symbol named {name:?}"))
}

fn frames_line(needle: &str) -> usize {
    fs::read_to_string(fixture("tests/fixtures/c/frames.c"))
        .expect("read frames fixture")
        .lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("frames.c has no line containing {needle:?}"))
        + 1
}

/// Returns the state letter of a live process, or `None` once it is gone.
fn process_state(pid: u32) -> Option<char> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;

    // The command name may contain anything, so the state follows its last
    // closing parenthesis.
    stat.rsplit_once(')')?.1.trim_start().chars().next()
}

/// The harness's deadline for anything a test waits to observe. It bounds
/// a hung session, never a wait a passing test makes: loading a large
/// program, such as a tokio fixture, takes seconds on a loaded machine.
const DEADLINE: Duration = Duration::from_secs(30);

/// A microscope process whose output a test reads as it arrives. It is killed
/// if the test ends first, which also kills an inferior it launched or attached
/// to, since microscope traces with `PTRACE_O_EXITKILL`.
struct Microscope {
    child: Child,
    stdout: mpsc::Receiver<String>,
    transcript: String,
    stderr: Option<thread::JoinHandle<String>>,
}

impl Microscope {
    /// Starts microscope with piped standard streams.
    fn spawn(command: &mut Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run microscope");
        let stdout = child.stdout.take().expect("stdout pipe");
        let mut stderr = child.stderr.take().expect("stderr pipe");
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        });

        Self {
            child,
            stdout: receiver,
            transcript: String::new(),
            stderr: Some(stderr),
        }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }

    fn send(&mut self, text: &str) {
        self.child
            .stdin
            .as_mut()
            .expect("microscope stdin")
            .write_all(text.as_bytes())
            .expect("write to microscope");
    }

    fn close_stdin(&mut self) {
        drop(self.child.stdin.take());
    }

    /// Reads standard output until a line satisfies `predicate`.
    fn line(&mut self, what: &str, predicate: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let line = self
                .stdout
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!(
                        "microscope never printed {what} ({error}):\n{}",
                        self.transcript
                    )
                });
            self.transcript.push_str(&line);
            self.transcript.push('\n');
            if predicate(&line) {
                return line;
            }
        }
    }

    /// Waits for microscope to exit and returns everything it printed.
    fn finish(mut self) -> std::process::Output {
        let mut status = None;
        support::wait_until("microscope exits", || {
            status = self.child.try_wait().expect("poll microscope");
            status.is_some()
        });
        let deadline = Instant::now() + DEADLINE;
        loop {
            match self
                .stdout
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(line) => {
                    self.transcript.push_str(&line);
                    self.transcript.push('\n');
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    panic!("microscope's output never closed:\n{}", self.transcript)
                }
            }
        }

        let stderr = self
            .stderr
            .take()
            .expect("stderr reader")
            .join()
            .expect("read microscope stderr");

        std::process::Output {
            status: status.expect("exit status"),
            stdout: std::mem::take(&mut self.transcript).into_bytes(),
            stderr: stderr.into_bytes(),
        }
    }
}

impl Drop for Microscope {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn help_is_task_oriented_and_progressive() {
    let short = assert_success(microscope(&["-h"]));
    assert_in_order(
        &short,
        &[
            "Debug Linux x86-64 programs, processes, and core dumps",
            "Usage:",
            "Tools:",
            "Target:",
            "Startup:",
            "Common forms:",
        ],
    );
    for invocation in [
        "microscope EXECUTABLE [-- ARGS...]",
        "microscope --attach PID|NAME [EXECUTABLE]",
        "microscope --core CORE [EXECUTABLE]",
        "microscope --launch NAME",
    ] {
        assert!(short.contains(invocation), "{short}");
    }

    for advanced in ["--sysroot", "--module-path", "--source-map"] {
        assert!(!short.contains(advanced), "{short}");
    }

    let long = assert_success(microscope(&["--help"]));
    assert_in_order(
        &long,
        &[
            "Tools:",
            "Target:",
            "Startup:",
            "Core dump files:",
            "Debug information:",
            "Launch environment:",
            "Display:",
        ],
    );
    for advanced in ["--sysroot", "--module-path", "--source-map"] {
        assert!(long.contains(advanced), "{long}");
    }

    assert!(!long.contains("microscope dap ["), "{long}");

    for subcommand in ["dap", "views", "config"] {
        assert!(short.contains(subcommand), "{short}");
        let own = assert_success(microscope(&[subcommand, "--help"]));
        assert!(own.contains(&format!("microscope {subcommand}")), "{own}");
    }
}

#[test]
fn an_empty_command_prints_short_help() {
    let empty = microscope(&[]);
    let short_help = microscope(&["-h"]);

    assert!(
        empty.status.success(),
        "{}",
        String::from_utf8_lossy(&empty.stderr)
    );
    assert_eq!(empty.stdout, short_help.stdout);
    assert_eq!(empty.stderr, short_help.stderr);
}

#[test]
fn command_line_errors_explain_themselves() {
    let missing_process = i32::MAX.to_string();
    for (arguments, expected) in [
        (
            &["--attach", &missing_process, "--batch"][..],
            "pass EXECUTABLE explicitly",
        ),
        (
            &["--core", SEGV_CORE, "--attach", "1"],
            "cannot be used with",
        ),
        (
            &["--core", SEGV_CORE, "--cwd", "/", "--batch"],
            "cannot be used with",
        ),
        (&["--allow-module-mismatch", BASIC], "--core <CORE>"),
        (&["--sysroot", "/", BASIC], "--core <CORE>"),
        (&["--module-path", "/", BASIC], "--core <CORE>"),
        (
            &["--env", "novalue", BASIC],
            "expected NAME=VALUE, found 'novalue'",
        ),
        (
            &[BASIC, "--source-map", "/nonexistent"],
            "2 values required for '--source-map <FROM> <TO>'",
        ),
        (
            &[
                "--core",
                "build/test-programs/core-foreign/crash.core",
                "--module-path",
                "absent",
                "--batch",
            ],
            "cannot search absent for core dump modules: No such file or directory",
        ),
        (
            &[
                "--core",
                "build/test-programs/core-missing-executable/crash.core",
                "--batch",
            ],
            "no longer exists; supply the executable explicitly",
        ),
        (
            &["--core", "build/test-programs/crash-gcc-o0", "--batch"],
            "invalid core dump: the ELF file is not a core dump",
        ),
        (
            &[
                "--core",
                SEGV_CORE,
                "build/test-programs/crash-gcc-o0-rebuilt",
                "--batch",
            ],
            "does not match the image recorded in the core dump: the build-id note differs; allow module mismatches",
        ),
    ] {
        assert_failure(&microscope(arguments), expected);
    }
}

#[test]
fn pid_only_attach_discovers_the_executable_and_quit_detaches() {
    let mut target = support::ExternalProcess::spawn(&fixture("build/test-programs/attach"));
    let process = target.process_id().get().to_string();
    batch(&["--attach", &process], &["quit"]);
    target.release();
    assert_eq!(target.wait().code(), Some(23));
}

/// A panic that ends the controller releases an attached process as a
/// detach would, whether the panic finds it stopped or running: no trap
/// of a breakpoint and no watchpoint is left to kill it, and it runs on to
/// its usual exit. Development builds panic on request.
#[test]
fn a_controller_panic_releases_an_attached_process_unharmed() {
    for (after, command) in [("backtrace", "backtrace"), ("continue", "continue")] {
        let mut target = support::ExternalProcess::spawn(&fixture("build/test-programs/attach"));
        let process = target.process_id().get().to_string();
        let output = microscope_command()
            .env("MICROSCOPE_TEST_PANIC_AFTER", after)
            .args(["--batch", "--attach", &process])
            .args(["--eval", "break attach_breakpoint"])
            .args(["--eval", "awatch attach_value"])
            .args(["--eval", command])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stdin(Stdio::null())
            .output()
            .expect("run microscope");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{after}: {stderr}");
        assert!(stderr.contains("a panic injected"), "{after}: {stderr}");
        target.release();
        assert_eq!(target.wait().code(), Some(23), "{after}: {stderr}");
    }
}

/// `--attach NAME` attaches to the one process of that name, or that runs a
/// script of that name, and refuses to choose among several, listing each
/// with what matched.
#[test]
fn attach_by_name_needs_exactly_one_process_of_that_name() {
    let directory = support::ScratchDir::new("cli-attach-name");
    let name = format!("named-{}", std::process::id());
    let copy = directory.path().join(&name);
    fs::copy(fixture("build/test-programs/attach"), &copy).expect("copy the fixture");
    let mut target = support::ExternalProcess::spawn(&copy);
    let stdout = batch(&["--attach", &name], &["quit"]);
    assert!(stdout.is_empty(), "{stdout}");
    target.release();
    assert_eq!(target.wait().code(), Some(23));

    // A script argument names a process as `node server.js` is named.
    let script = format!("app-{}.js", std::process::id());
    let mut runner = support::ExternalProcess::spawn_command(
        Command::new(fixture("build/test-programs/attach")).arg(directory.path().join(&script)),
    );
    let stdout = batch(&["--attach", &script], &["quit"]);
    assert!(stdout.is_empty(), "{stdout}");
    runner.release();
    assert_eq!(runner.wait().code(), Some(23));

    let first = support::ExternalProcess::spawn(&copy);
    let second = support::ExternalProcess::spawn(&copy);
    let output = batch_output(&["--attach", &name], &["quit"]);

    for process in [&first, &second] {
        assert_failure(
            &output,
            &format!("  {}  {}  (command)", process.process_id(), copy.display()),
        );
    }

    assert_failure(
        &output,
        &format!("2 processes match '{name}'; attach to one by its id:\n"),
    );
    assert_failure(
        &batch_output(&["--attach", "no-such-process-name"], &["quit"]),
        "no process matches 'no-such-process-name'",
    );
}

#[test]
fn color_follows_the_choice_and_the_environment_on_both_streams() {
    // Redirected output is plain unless color is forced, and `never` wins
    // over the environment.
    assert_no_sgr(&batch(&[BASIC], &["help"]));
    let colored_help = assert_success(microscope(&["--color", "always", "-h"]));
    assert!(colored_help.contains("\x1b["), "{colored_help:?}");
    let plain_help = microscope_command()
        .env("CLICOLOR_FORCE", "1")
        .args(["--color", "never", "-h"])
        .output()
        .expect("run microscope help with color disabled");
    assert_no_sgr(&assert_success(plain_help));

    let never = microscope_command()
        .env("CLICOLOR_FORCE", "1")
        .args(["--batch", "--color", "never", "--eval", "help"])
        .arg(fixture(BASIC))
        .output()
        .expect("run microscope with color disabled");
    assert_no_sgr(&assert_success(never));

    let forced = batch_output(
        &["--color", "always", BASIC],
        &["help", "break breakpoint_target", "run", "invalid"],
    );
    let stdout = String::from_utf8_lossy(&forced.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&forced.stderr).into_owned();
    assert!(!forced.status.success(), "{stdout}");

    // Help styles command names and aliases, and nothing else.
    assert!(stdout.contains("\x1b[1m\x1b[94mbreak\x1b[0m"), "{stdout:?}");
    assert!(stdout.contains("\x1b[94mb\x1b[0m"), "{stdout:?}");
    assert!(
        !stdout.contains("\x1b[94m\x1b[0m"),
        "empty aliases emitted styling: {stdout:?}"
    );

    for plain in ["commands", "Set a breakpoint", "Use `help <command>`"] {
        assert!(!stdout.contains(&format!("\x1b[2m{plain}")), "{stdout:?}");
    }

    // Source metadata is styled, and the source's keywords, but not its
    // names.
    let current = stdout
        .lines()
        .find(|line| line.contains("uint64_t breakpoint_target("))
        .expect("current source line");
    let (margin, source) = current.split_once("| ").expect("source separator");
    assert!(
        margin.contains("\x1b["),
        "metadata was not styled: {current:?}"
    );
    assert!(
        source.contains("uint64_t breakpoint_target(\x1b[") && source.contains("void\x1b[0m)"),
        "the keyword was not highlighted: {current:?}"
    );

    // A failed command ends the batch, naming the command.
    assert!(
        stderr.contains("\x1b[1m\x1b[91merror\x1b[0m: --eval #4: unknown command 'invalid'"),
        "{stderr:?}"
    );
}

#[test]
fn batch_mode_executes_a_command_file() {
    let stdout = assert_success(microscope(&[
        "--batch",
        "-c",
        "tests/fixtures/basic.microscope",
        BASIC,
    ]));
    assert_eq!(stdout.matches("stopped at breakpoint").count(), 2);
    assert!(stdout.contains("breakpoint_target at"));
    assert!(stdout.contains("basic.c:"));
    assert!(stdout.contains("inferior exited with status 0"));
}

#[test]
fn batch_mode_streams_commands_from_stdin() {
    let (stdout, _) = piped(
        &["--batch", BASIC],
        &["break breakpoint_target", "run", "quit"],
    );
    assert!(stdout.contains("breakpoint 1 set"));
    assert!(stdout.contains("stopped at breakpoint"));
}

#[test]
fn piped_input_reports_errors_and_runs_to_its_end() {
    let (stdout, stderr) = piped(
        &[BASIC],
        &["invalid", "break breakpoint_target", "run", "registers"],
    );
    assert!(
        stderr.contains("stdin:1: unknown command 'invalid'"),
        "{stderr}"
    );
    assert!(stdout.starts_with("debugging "), "{stdout}");
    assert!(stdout.lines().any(|line| line.starts_with("rax ")));
    assert!(stdout.lines().any(|line| line.starts_with("orig_rax ")));
}

#[test]
fn command_errors_show_the_usage_or_the_reason() {
    let (stdout, stderr) = piped(
        &[BASIC],
        &[
            "run ignored",
            "break",
            // Only `info symbol` and `info view` take an argument, and they
            // require one.
            "info symbol",
            "info view",
            "info breakpoints 0x10",
            "info core",
            "handle SIGNOPE nostop",
            "handle SIGUSR1 sometimes",
            "handle",
            "clear",
            "cls",
        ],
    );

    let info =
        "usage: info breakpoints|watchpoints|signals|modules|calls|core|symbol|view [argument...]";
    let clear = "cannot clear screen: stdout is not an ANSI terminal";
    assert_in_order(
        &stderr,
        &[
            "usage: run\n",
            "no frame is selected to take a line from (the inferior has not been launched); \
             name a location",
            info,
            info,
            info,
            "no core dump is open",
            "unknown signal 'SIGNOPE'",
            "unknown signal action 'sometimes'",
            "usage: handle <signal> [action] [action] [action]",
            clear,
            clear,
        ],
    );
    assert!(!stdout.contains("\x1b[2J\x1b[H"), "{stdout:?}");
}

#[test]
fn help_lists_every_command_and_details_one_by_name_or_alias() {
    let stdout = batch(
        &[BASIC],
        &[
            "help",
            "help clear",
            "help del",
            "help fin",
            "help break",
            "help where",
            "help continue",
        ],
    );
    for expected in [
        "  break        b           Set a breakpoint",
        "  finish       fin, f      Run until the selected frame returns",
        "  continue     c           Continue execution",
        "  delete       del, d      Delete breakpoints, watchpoints wID, and catchpoints cID",
        "  clear        cls         Clear and redraw the terminal",
        "  help         h, ?        Show command help",
        "  Clear and redraw the terminal\n  aliases: cls",
        "delete <ids...>",
        "aliases: del, d",
        "  Run until the selected frame returns to its caller\n  aliases: fin, f",
        "  aliases: b\n  usage: break [location] [if condition...] [hits hit-condition] [log message] [disabled]",
        "  Show the selected frame's execution location",
        "  Continue execution\n  aliases: c",
    ] {
        assert!(stdout.contains(expected), "{expected:?} in:\n{stdout}");
    }

    let (overview, _) = stdout
        .split_once("Use `help <command>` for aliases and usage.")
        .expect("the overview's last line");
    assert!(!overview.contains("<function"), "{overview}");
    for absent in [
        "break\n  Set a breakpoint",
        "finish\n  Run until the selected frame returns",
        "usage: where",
        "  Show the selected frame's execution location\n  aliases:",
        "usage: continue",
    ] {
        assert!(!stdout.contains(absent), "{absent:?} in:\n{stdout}");
    }
}

#[test]
fn batch_mode_prints_every_location_of_an_inline_breakpoint() {
    let stdout = batch(&["build/test-programs/inline-gcc-o2"], &["break leaf"]);
    assert!(
        stdout.contains("breakpoint 1 set at 6 locations"),
        "{stdout}"
    );

    // Inline copies of one line differ by address.
    assert_eq!(
        stdout
            .matches(" leaf at tests/fixtures/c/inline.c:")
            .count(),
        6,
        "{stdout}"
    );
    assert_eq!(stdout.matches("  0x").count(), 6, "{stdout}");
}

/// Each other thread a stop finds at a breakpoint gets a line of its own
/// after the stop's, naming its hit and place, as the threads list has it.
#[test]
fn every_co_hit_prints_a_line_of_its_own() {
    let mut commands = vec!["break contended", "run", "threads"];
    for _ in 0..10 {
        commands.extend(["continue", "threads"]);
    }

    let stdout = batch(&["build/test-programs/hit-count-threads"], &commands);
    let place = " in contended at tests/fixtures/c/hit-count-threads.c:16";
    let mut co_hits = 0;
    for stop in stdout.split("\nstopped at breakpoint 1 ").skip(1) {
        let stopped = stop
            .split_once("[thread ")
            .and_then(|(_, rest)| rest.split_once(' '))
            .map(|(thread, _)| thread)
            .expect("the stop names its thread");
        let mut listed = BTreeSet::new();
        let mut reported = BTreeSet::new();
        for line in stop.lines() {
            if let Some(rest) = line.strip_prefix("thread ") {
                let (thread, rest) = rest.split_once(' ').expect("a co-hit names its thread");
                let hit = rest
                    .strip_prefix("also stopped at breakpoint 1 (hit ")
                    .and_then(|rest| rest.strip_suffix(&format!("){place}")))
                    .unwrap_or_else(|| panic!("{line:?}"));
                reported.insert((thread.to_owned(), hit.to_owned()));
            } else if let Some((thread, hit)) = line
                .strip_prefix("  ")
                .and_then(|rest| rest.split_once(' '))
                .and_then(|(thread, rest)| {
                    let (_, hit) = rest.split_once(": stopped at breakpoint 1 (hit ")?;
                    Some((thread, hit.strip_suffix(')')?))
                })
            {
                listed.insert((thread.to_owned(), hit.to_owned()));
            } else if let Some(selected) = line.strip_prefix("* ") {
                assert!(selected.starts_with(&format!("{stopped} ")), "{stop}");
            }
        }

        assert_eq!(reported, listed, "{stop}");
        co_hits += reported.len();
    }

    // The workers call `contended` together, so many of their hits come
    // at the same stop.
    assert!(co_hits > 0, "{stdout}");
}

/// A thread whose runtime cannot be read says so, and why, rather than
/// looking like one that runs nothing.
#[test]
fn threads_say_when_what_they_run_is_unknown() {
    let stdout = batch(
        &["build/test-programs/tokio-workers-lines"],
        &["break truth_reached", "run", "threads"],
    );
    let threads = stdout
        .lines()
        .filter(|line| line.contains(" stopped"))
        .filter(|line| line.starts_with("  ") || line.starts_with("* "))
        .collect::<Vec<_>>();
    assert_eq!(threads.len(), 5, "{stdout}");
    for thread in threads {
        assert!(
            thread.ends_with(
                " — unknown: what the thread does for tokio cannot be read: the program \
                 describes no type \
                 std::sys::thread_local::native::eager::Storage<tokio::runtime::context::Context>"
            ),
            "{stdout}"
        );
    }
}

/// tokio's tasks are listed by number, whatever order its lists keep
/// them in, each saying what it does and the thread it is on, and where
/// a suspended one waits in the program's own code, past tokio's, and for
/// what: the first task spawned waits for a message, the second sleeps.
#[test]
fn tokio_tasks_are_listed_by_number() {
    let stdout = batch(
        &["build/test-programs/tokio-workers-o0"],
        &["break truth_reached", "run", "tasks"],
    );
    let tasks = stdout
        .lines()
        .filter_map(|line| {
            line.strip_prefix("  [")
                .or_else(|| line.strip_prefix("* ["))
        })
        .map(|line| {
            let (number, rest) = line.split_once(']').expect("a task number");
            (number.parse::<u64>().expect("a number"), rest)
        })
        .collect::<Vec<_>>();
    let numbers = tasks.iter().map(|(number, _)| *number).collect::<Vec<_>>();
    assert_eq!(numbers, (3..=12).collect::<Vec<_>>(), "{stdout}");
    for (number, rest) in &tasks {
        let expected = match number {
            3 => " — receiving; senders: 1",
            4 => " — sleeping until +",
            11 => " — running a blocking closure (thread ",
            12 => " — queued in the blocking pool",
            _ => " — ",
        };

        assert!(rest.contains(expected), "{stdout}");
        if *number < 11 {
            assert!(
                rest.starts_with(" leaf at tests/fixtures/rust/tokio/workers/src/main.rs:"),
                "{stdout}"
            );
        }
    }
}

/// `task` alone shows the selected task and where it was created: the
/// call that created a goroutine, and the line tokio recorded spawning a
/// task from, in a build with `tokio_unstable`.
#[test]
fn a_task_says_where_it_was_created() {
    let created = |program: &str, commands: &[&str], source: &str| {
        let stdout = batch(&[program], commands);
        let line = stdout
            .lines()
            .find_map(|line| line.strip_prefix("    created "))
            .unwrap_or_else(|| panic!("a creation: {stdout}"))
            .to_owned();
        let (place, number) = line
            .rsplit_once(':')
            .unwrap_or_else(|| panic!("a line: {line}"));
        let number = number.parse::<usize>().expect("a line number");
        let text = std::fs::read_to_string(fixture(source)).expect("the fixture's source");
        let creating = text.lines().nth(number - 1).expect("the line").to_owned();
        (place.to_owned(), creating)
    };

    let source = "tests/fixtures/rust/tokio/workers/src/main.rs";
    let (place, creating) = created(
        "build/test-programs/tokio-workers-unstable",
        &["break truth_reached", "run", "task 4", "task"],
        source,
    );
    assert_eq!(place, format!("at {source}"));
    assert!(creating.contains("tokio::spawn("), "{creating}");
    let source = "tests/fixtures/go/workers/main.go";
    let (place, creating) = created(
        "build/test-programs/workers-go-o0",
        &["break main.worker", "run", "goroutine"],
        source,
    );
    assert_eq!(place, format!("by main.main at {source}"));
    assert!(creating.trim_start().starts_with("go "), "{creating}");
}

/// A task's panic stop says where it panicked, on which thread, and in
/// which task, then the message, and shows the source there.
#[test]
fn a_task_panic_says_where_and_in_which_task() {
    let stdout = batch(
        &["build/test-programs/tokio-panics-o0", "--", "format"],
        &["run", "threads"],
    );
    let (header, rest) = stdout
        .split_once(":\npanicked: formatted 7 times\n")
        .unwrap_or_else(|| panic!("a panic stop: {stdout}"));
    let header = header.lines().last().expect("the stop's header");
    let source = "tests/fixtures/rust/tokio/panics/src/main.rs";
    let line = support::source_line(source, "// PANIC: format");
    assert!(
        header.starts_with(&format!(
            "stopped as an exception was raised in formatted at {source}:{line} [thread "
        )),
        "{stdout}"
    );

    // The task is the one the thread list puts on the thread.
    let (thread, task) = header
        .rsplit_once("[thread ")
        .and_then(|(_, rest)| rest.split_once(" of "))
        .and_then(|(thread, rest)| Some((thread, rest.split_once("] ")?.1)))
        .unwrap_or_else(|| panic!("a thread and task: {header}"));
    assert!(task.starts_with('[') && task.ends_with(']'), "{header}");
    assert!(
        rest.lines()
            .any(|line| line.starts_with(&format!("* {thread} "))
                && line.ends_with(&format!(" — {task}"))),
        "{stdout}"
    );
    assert!(
        rest.contains(&format!(
            "=> {line} |     panic!(\"formatted {{count}} times\");"
        )),
        "{stdout}"
    );
}

/// `finish` from a task's own async function says the task finished, and
/// what the function's last poll returned, named for the function.
#[test]
fn finishing_a_tasks_function_says_the_task_finished() {
    let source = "tests/fixtures/rust/tokio/steps/src/main.rs";
    let line = support::source_line(source, "// STEP: task-last");
    let stdout = batch(
        &["build/test-programs/tokio-steps-o0"],
        &[
            &format!("break main.rs:{line}"),
            "run",
            "delete 1",
            "finish",
        ],
    );
    let (_, finished) = stdout
        .split_once("deleted breakpoint 1\n")
        .unwrap_or_else(|| panic!("a finish: {stdout}"));
    let header = finished.lines().next().expect("the stop's header");
    let task = header
        .strip_prefix("stopped as task ")
        .and_then(|rest| rest.split_once(" finished, after frame return in "))
        .map_or_else(|| panic!("the task finished: {stdout}"), |(task, _)| task);
    assert!(header.ends_with(&format!(" [{task}]")), "{stdout}");
    assert!(
        finished.contains("returned (Poll<u64>) task = Ready("),
        "{stdout}"
    );
}

/// A backtrace folds each run of a runtime's frames into a line that says
/// which frames it holds, and everything past the dispatch that polls the
/// task; `bt -r` shows every frame, with its whole name. Frame numbers
/// count every frame either way, and the selected frame is always shown.
#[test]
fn a_runtimes_frames_fold_into_a_line() {
    let stdout = batch(
        &["build/test-programs/tokio-workers-o0"],
        &["break task_reached", "run", "bt", "bt -r", "frame 20", "bt"],
    );

    // Each backtrace ends saying where unwinding stopped.
    let [folded, raw, selected, _] = stdout.split("\nunwind stopped").collect::<Vec<_>>()[..]
    else {
        panic!("three backtraces: {stdout}");
    };

    // `frame` shows the frame it selects before its source.
    let selected = selected
        .split_once("=> ")
        .and_then(|(_, rest)| rest.split_once('\n'))
        .map_or(selected, |(_, rest)| rest);
    let frames = |text: &str| {
        text.lines()
            .filter(|line| line.starts_with('#') || line.starts_with("    … #"))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };

    let (folded, raw, selected) = (frames(folded), frames(raw), frames(selected));
    let last = raw.len() - 1;
    assert!(raw.iter().all(|line| line.starts_with('#')), "{stdout}");
    assert!(
        raw.iter()
            .any(|line| line.contains(" in run<alloc::sync::Arc<")),
        "{stdout}"
    );
    assert_in_order(
        &folded.join("\n"),
        &[
            "#0 ",
            " in task_reached at ",
            "#1 ",
            " in leaf at ",
            "#2 ",
            " in middle at ",
            "#3 ",
            " in top at ",
            &format!(
                "\n    … #4–#{last}: {} frames of the runtime; `bt -r` shows them",
                last - 3
            ),
        ],
    );
    assert_eq!(folded.len(), 5, "{stdout}");

    // The selected frame stands alone, with its neighbours folded around it.
    assert!(
        selected.iter().any(|line| line.starts_with("#20 ")),
        "{stdout}"
    );
    assert!(
        selected.iter().any(|line| line.starts_with("    … #4–#")),
        "{stdout}"
    );
    assert!(selected.len() < raw.len(), "{stdout}");
}

/// The backtrace of a thread that blocks on a future shows the future's
/// awaits before the frame that drives it, under a line saying so; where
/// optimization lost the future, a line says why instead.
#[test]
fn a_thread_blocked_on_a_future_shows_its_awaits() {
    let backtrace = |program: &str, mode: &str| {
        let mut microscope = Microscope::spawn(
            microscope_command()
                .args([fixture(program).as_os_str(), "--".as_ref(), mode.as_ref()])
                .current_dir(env!("CARGO_MANIFEST_DIR")),
        );
        microscope.send("break truth_reached\nrun\n");
        let driver = microscope.line("the driving thread", |line| {
            line.starts_with("TRUTH\tdriver\t")
        });
        let driver = driver.rsplit('\t').next().expect("a thread id").to_owned();
        microscope.line("the checkpoint", |line| {
            line.starts_with("stopped at breakpoint 1")
        });
        microscope.send(&format!("thread {driver}\nbt\nquit\n"));
        microscope.close_stdin();
        let stdout = assert_success(microscope.finish());
        let (_, trace) = stdout
            .split_once(&format!("selected thread {driver}"))
            .unwrap_or_else(|| panic!("the driver's backtrace: {stdout}"));

        trace.to_owned()
    };

    let source = "at tests/fixtures/rust/tokio/drivers/src/main.rs:";
    let trace = backtrace("build/test-programs/tokio-drivers-o0", "current");
    assert_in_order(
        &trace,
        &[
            "\n    in the future the next frame drives:\n",
            " awaiting tokio::sync::oneshot::Receiver<u32> from tokio-drivers-o0 — empty\n",
            &format!(" in async waiting {source}28\n"),
            &format!(" in async driven {source}36\n"),
            "    on the thread's stack:\n#17 ",
            " in {closure#0}<…> at ",
            "\nunwind stopped",
        ],
    );
    let trace = backtrace("build/test-programs/tokio-drivers-o3", "handle");
    assert!(
        trace.contains(
            " drives is not shown in full: `f`, which holds the future, is unavailable: \
             the value is optimized out\n"
        ),
        "{trace}"
    );
}

#[test]
fn library_breakpoints_resolve_at_runtime_and_frames_show_their_own_sources() {
    let stdout = batch(
        &["build/test-programs/module-frames-gcc-o0"],
        &[
            "break main",
            "run",
            "break dso_apply",
            "continue",
            "break module_callback",
            "continue",
            "backtrace",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "breakpoint 2 set at dso_apply at tests/fixtures/c/module-frames/library.c:5 in \
             libmodule-frames.so\n",
            "stopped at breakpoint 2 (hit 1)",
            "module-frames/library.c:5",
            "stopped at breakpoint 3 (hit 1)",
            "#0  0x",
            " in module_callback at ",
            "#1  0x",
            " in dso_apply at ",
            "tests/fixtures/c/module-frames/library.c:6\n",
            "#2  0x",
            " in main at ",
        ],
    );
}

#[test]
fn batch_mode_sets_lists_and_deletes_source_and_file_function_breakpoints() {
    let stdout = batch(
        &[BASIC],
        &[
            "break basic.c:11",
            "break basic.c:breakpoint_target",
            "breakpoints",
            "del 1",
            "d all",
            "info breakpoints",
        ],
    );
    assert!(
        stdout.contains("1   +      0  main at tests/fixtures/c/basic.c:11\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("2   +      0  breakpoint_target at tests/fixtures/c/basic.c:6\n"),
        "{stdout}"
    );
    assert!(stdout.contains("deleted breakpoint 1"), "{stdout}");
    assert!(stdout.contains("deleted 1 breakpoint"), "{stdout}");
    assert!(stdout.ends_with("no breakpoints\n"), "{stdout}");
}

#[test]
fn batch_mode_sets_skips_and_amends_breakpoint_hit_conditions() {
    let stdout = batch(
        &["build/test-programs/hit-counts-gcc-o0"],
        &[
            "break counted ==3",
            "break shared %4",
            "run",
            "breakpoints",
            "continue",
            "ignore 1 5",
            "hits 2 always",
            "info breakpoints",
            "delete 2",
            "continue",
            "hits 1 ==2",
            "continue",
            "breakpoints",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "breakpoint 1 set at counted at tests/fixtures/c/hit-counts.c:11, stops at hits ==3\n",
            "breakpoint 2 set at 2 locations, stops at hits %4\n",
            // The fourth hit is the second inline site of the second call.
            "stopped at breakpoint 2 (hit 4) in ",
            "1   +      2  counted at tests/fixtures/c/hit-counts.c:11  hits ==3\n",
            "2   +      4  shared, 2 locations                          hits %4\n",
            "stopped at breakpoint 1 (hit 3) in ",
            "breakpoint 1 ignores its next 5 hits\n",
            "breakpoint 2 stops at every hit, hit 4 times so far\n",
            "1   +      3  counted at tests/fixtures/c/hit-counts.c:11  hits >=9\n",
            "2   +      4  shared, 2 locations\n",
            "deleted breakpoint 2\n",
            "stopped at breakpoint 1 (hit 9) in ",
            "breakpoint 1 stops at hits ==2 (no later hit can stop), hit 9 times so far\n",
            "inferior exited with status 0\n",
            "1   +     40  counted at tests/fixtures/c/hit-counts.c:11  hits ==2 (no later hit can stop)\n",
        ],
    );
}

/// Runs `commands` in batch mode with the settings file `settings`.
fn batch_with_settings(settings: &str, arguments: &[&str], commands: &[&str]) -> String {
    let directory = support::ScratchDir::new("cli-settings");
    let path = directory.path().join("config.toml");

    // How long a run took depends on the machine's load, as the settings
    // every other test uses say.
    let settings = match settings.split_once("[stop]\n") {
        Some((before, after)) => format!("{before}[stop]\nelapsed = false\n{after}"),
        None => format!("{settings}\n[stop]\nelapsed = false\n"),
    };

    std::fs::write(&path, settings).expect("write the settings");
    let mut all = vec!["--batch"];
    for command in commands {
        all.extend(["--eval", command]);
    }

    all.extend_from_slice(arguments);
    assert_success(
        microscope_command()
            .env("MICROSCOPE_CONFIG", &path)
            .args(all)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stdin(Stdio::null())
            .output()
            .expect("run microscope"),
    )
}

#[test]
fn words_c_reserves_for_types_name_a_rust_programs_variables() {
    let output = batch(
        &["build/test-programs/strings-rust-o0"],
        &["break strings_target", "run", "whatis long", "p *long"],
    );
    assert_in_order(
        &output,
        &["type = &alloc::string::String", "(String) *long = \"zzz"],
    );
}

#[test]
fn c_base_types_cast_in_a_program_that_describes_none() {
    let output = batch(
        &["build/test-programs/strings-go-o0"],
        &[
            "break main.stringsTarget",
            "run",
            "p (unsigned char)300",
            "p ((char*)name.str)[1]",
            "whatis (long long unsigned)1",
            "p sizeof(long double)",
        ],
    );
    assert_in_order(
        &output,
        &[
            "(unsigned char) (unsigned char)300 = 44 ','",
            "(char) ((char*)name.str)[1] = 111 'o'",
            "type = unsigned long long",
            "(integer) sizeof(long double) = 16",
        ],
    );
}

#[test]
fn function_pointers_show_their_signature_and_the_function_they_enter() {
    let output = batch(
        &["build/test-programs/variables-gcc-o0"],
        &[
            "break variables.c:68",
            "run",
            "p function_pointer",
            "ptype function_pointer",
            "p *function_pointer",
            "whatis &function_pointer",
        ],
    );
    assert_in_order(
        &output,
        &[
            "(int (*)(int)) function_pointer = 0x",
            " <pointer_identity>\n",
            "type = int (*)(int)\n",
            "(int (int)) *function_pointer = <unavailable",
            "type = int (**)(int)\n",
        ],
    );

    // A pointer into another module names the function there.
    let shared = batch(
        &["build/test-programs/globals-shared"],
        &["break after_load", "run", "up", "p touch", "whatis *touch"],
    );
    assert_in_order(
        &shared,
        &[
            "(touch_fn) touch = 0x",
            " <dso_touch>\n",
            "type = int32_t (void)\n",
        ],
    );
}

/// Function pointers' types read as gdb reads them, declarators and all,
/// and their values name the function they enter, compared over GCC's and
/// Clang's debug information.
#[test]
fn function_types_read_as_gdb_reads_them() {
    let compact = |text: &str| text.split_whitespace().collect::<String>();
    for compiler in ["gcc", "clang"] {
        let program = format!("build/test-programs/function-types-{compiler}-o0");
        let oracle = std::fs::read_to_string(format!("{program}.gdb-function-types"))
            .expect("the gdb oracle; run `just build-test-programs`");
        let rows: Vec<Vec<&str>> = oracle
            .lines()
            .map(|line| line.split('\t').collect())
            .collect();
        let mut commands = vec!["break main".to_owned(), "run".to_owned()];
        for row in &rows {
            commands.push(format!("whatis {}", row[0]));
            commands.push(format!("print {}", row[0]));
        }

        let commands: Vec<&str> = commands.iter().map(String::as_str).collect();
        let output = batch(&[&program], &commands);
        let mut types = output
            .lines()
            .filter_map(|line| line.strip_prefix("type = "));
        let mut values = output
            .lines()
            .filter(|line| line.starts_with('(') && line.contains(" = "));
        for row in &rows {
            let [name, gdb_type, gdb_function] = row[..] else {
                panic!("malformed oracle row {row:?}");
            };

            let ours = types.next().expect("a type per whatis");
            assert_eq!(compact(ours), compact(gdb_type), "{compiler} whatis {name}");
            let value = values.next().expect("a value per print");
            let function = value
                .strip_suffix('>')
                .and_then(|value| value.rsplit_once(" <"))
                .map_or("", |(_, function)| function);

            // gdb names a pointer to data by its symbol too; microscope names
            // only the functions pointers to code enter.
            let expected = if name == "unary_pointer" {
                ""
            } else {
                gdb_function
            };

            assert_eq!(function, expected, "{compiler} print {name}: {value}");
        }
    }
}

/// A PLT stub is named after the function it jumps to, so a breakpoint can
/// name it and a backtrace through it reads as gdb's does.
#[test]
fn plt_stubs_are_named_after_their_targets() {
    let output = batch(
        &["build/test-programs/disassembly-gcc-o0"],
        &["break printf@plt", "run", "bt"],
    );
    assert_in_order(
        &output,
        &[
            "stopped at breakpoint 1 (hit 1) in printf@plt at 0x",
            "#0  0x",
            " in printf@plt from disassembly-gcc-o0\n#1  0x",
            " in main at tests/fixtures/c/disassembly/main.c:",
        ],
    );
}

/// mold and lld describe no PLT stub with call-frame information. A stub
/// pushes nothing before its jump through the GOT, or a slot index on a
/// lazy path, so the backtrace counts what the stub pushed before the stop,
/// at its first instruction and past it.
#[test]
fn backtraces_cross_plt_stubs_without_call_frame_information() {
    let output = batch(
        &["build/test-programs/disassembly-gcc-mold"],
        &["break printf@plt", "run", "bt", "stepi", "bt"],
    );
    assert_in_order(
        &output,
        &[
            "stopped at breakpoint 1 (hit 1) in printf@plt at 0x",
            "#0  0x",
            " in printf@plt from disassembly-gcc-mold\n#1  0x",
            " in main at tests/fixtures/c/disassembly/main.c:",
            " in _start+0x",
            "unwind stopped: the outermost frame has no caller",
            "stopped after instruction step in printf@plt+0x6 at 0x",
            " in printf@plt+0x6 from disassembly-gcc-mold\n#1  0x",
            " in main at tests/fixtures/c/disassembly/main.c:",
            " in _start+0x",
            "unwind stopped: the outermost frame has no caller",
        ],
    );
}

/// glibc defines `timer_create` twice, the old one as
/// `timer_create@GLIBC_2.2.5`, whose version names it apart. A breakpoint
/// on the plain name takes both, and one on the versioned name the old.
#[test]
fn symbol_versions_tell_apart_definitions_of_one_name() {
    let output = batch(
        &[BASIC],
        &[
            "break main",
            "run",
            "break timer_create",
            "break timer_create@GLIBC_2.2.5",
        ],
    );

    // The default version keeps the plain name, and the old one is named
    // by its version alone.
    let location = |name: &str| {
        output
            .lines()
            .find_map(|line| {
                let rest = line.split(&format!("{name} at ")).nth(1)?;
                rest.split_whitespace().next().map(str::to_owned)
            })
            .unwrap_or_else(|| panic!("no {name:?} in {output}"))
    };

    let (current, old) = (
        location("timer_create"),
        location("timer_create@GLIBC_2.2.5"),
    );
    assert_ne!(current, old, "{output}");
    assert_in_order(
        &output,
        &[
            "breakpoint 2 set at 2 locations",
            &format!("\n  timer_create at {current} in libc.so.6"),
            &format!("\n  timer_create@GLIBC_2.2.5 at {old} in libc.so.6"),
            &format!("breakpoint 3 set at timer_create@GLIBC_2.2.5 at {old} in libc.so.6"),
        ],
    );
    let described = batch(
        &[BASIC],
        &[
            "break main",
            "run",
            &format!("info symbol {old}"),
            &format!("info symbol {current}"),
        ],
    );
    assert_in_order(
        &described,
        &[
            "\ntimer_create@GLIBC_2.2.5 in section .text",
            "\ntimer_create in section .text",
        ],
    );
}

/// Code without debug information is broken at by its symbols: a function
/// the program imports waits for its library, an indirect function stops
/// in the implementation chosen for the machine, and C++ names are read
/// demangled, with or without their scopes and parameters.
#[test]
fn functions_without_debug_information_are_found_by_their_symbols() {
    const MEASURE: &str = "build/test-programs/measure-gcc-nodebug";
    let output = batch(
        &[MEASURE],
        &[
            "break printf",
            "break measure",
            "run",
            "break strlen",
            "continue",
            "address printf",
            "disassemble printf 1",
        ],
    );
    assert_in_order(
        &output,
        &[
            "breakpoint 1 set, pending until a module with its code loads",
            "breakpoint 2 set at measure at 0x",
            "stopped at breakpoint 2 (hit 1) in measure at 0x",
            "breakpoint 3 set at 2 locations",
            "\n  strlen at 0x",
            " in ld-linux-x86-64.so.2",
            "\n  __strlen_",
            " in libc.so.6",
            "stopped at breakpoint 3 (hit 1) in __strlen_",
            "printf: 0x",
            " <printf>:",
        ],
    );

    // A name nothing imports or defines is a mistake, not a wait.
    let refused = batch_output(&[MEASURE], &["break printff"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("no function named 'printff' was found")
    );

    let output = batch(
        &["build/test-programs/overloads-cpp-gcc-nodebug"],
        &[
            "break pick",
            "break shapes::Widget::pick()",
            "break pick(double)",
        ],
    );
    assert_in_order(
        &output,
        &[
            "breakpoint 1 set at 3 locations",
            "\n  pick(int) at 0x",
            "\n  pick(double) at 0x",
            "\n  shapes::Widget::pick() const at 0x",
            "breakpoint 2 set at shapes::Widget::pick() const at 0x",
            "breakpoint 3 set at pick(double) at 0x",
        ],
    );
}

#[test]
fn pp_lays_values_out_to_the_width_and_print_formats_combine() {
    let records = ["build/test-programs/records-c-gcc-o0"];
    let commands = ["break inspect_records", "run", "pp *records", "up", "pp"];
    let wide = batch(&records, &commands);
    assert!(
        wide.contains(
            "(outer_record[2]) *records = [\n\
             \x20 {inner = {signed_value = 1, unsigned_value = 2}, values = [3, 4]},\n\
             \x20 {inner = {signed_value = 5, unsigned_value = 6}, values = [43, 44]},\n\
             ]\n"
        ),
        "{wide}"
    );

    // With no expression, every local of the frame, expanded.
    assert!(
        wide.contains("(bit_fields) bits = {negative = -3, first = 5, second = 42}\n"),
        "{wide}"
    );
    let narrow = batch_with_settings("[print]\nwidth = 40\n", &records, &commands);
    assert!(
        narrow.contains(
            "(outer_record[2]) *records = [\n\
             \x20 {\n\
             \x20   inner = {\n\
             \x20     signed_value = 1,\n\
             \x20     unsigned_value = 2,\n\
             \x20   },\n\
             \x20   values = [3, 4],\n\
             \x20 },\n"
        ),
        "{narrow}"
    );

    let (stdout, stderr) = piped(
        &records,
        &[
            "break inspect_records",
            "run",
            "print/x *record",
            "pp/x record->values",
            "print/pl *record",
            "print/q *record",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "(outer_record) *record = {inner = {signed_value = 0xfffffff9, unsigned_value = 0x9}, \
             values = [0x14, 0x16]}\n",
            "(int32_t[2]) record->values = [0x14, 0x16]\n",
        ],
    );
    assert_in_order(
        &stderr,
        &[
            "/p prints a value laid out and /l on one line; choose one",
            "unknown format '/q'; print takes /x, /d, /r, /p, and /l",
        ],
    );

    // `[print]` chooses what `print` does, and its formats override it.
    let configured = batch_with_settings(
        "[print]\nstyle = \"pretty\"\nradix = \"hexadecimal\"\nwidth = 40\n",
        &records,
        &[
            "break inspect_records",
            "run",
            "print record->inner",
            "print/l record->inner",
            "print/d record->values",
        ],
    );
    assert_in_order(
        &configured,
        &[
            "(inner_record) record->inner = {\n  signed_value = 0xfffffff9,\n  unsigned_value = 0x9,\n}\n",
            "(inner_record) record->inner = {signed_value = 0xfffffff9, unsigned_value = 0x9}\n",
            "(int32_t[2]) record->values = [20, 22]\n",
        ],
    );
}

/// `catch` lists the exceptions runtimes report and whether each stops,
/// and chooses whether one does: a Go panic the program recovers from
/// stops once `catch raised on` asks it to.
#[test]
fn catch_chooses_which_exceptions_stop() {
    let (stdout, stderr) = piped(
        &["build/test-programs/failing-go-o0", "--", "recovered"],
        &[
            "catch",
            "catch raised on",
            "run",
            "catch nope",
            "catch raised maybe",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "unhandled        on   Unhandled Go panics\n",
            "runtime-fatal    on   Fatal Go runtime errors\n",
            "raised           off  Every Go panic\n",
            "rust-panic       on   Rust panics\n",
            "python-raised    off  Every Python exception\n",
            "python-uncaught  off  Uncaught Python exceptions\n",
            "raised  on   Every Go panic\n",
            "as an exception was raised",
            "panic: runtime error: invalid memory address or nil pointer dereference",
        ],
    );
    assert_in_order(
        &stderr,
        &[
            "unknown exception 'nope'; runtimes report unhandled, runtime-fatal, raised",
            "usage: catch [exception] [on|off]",
        ],
    );
}

/// `print/x` writes a number a view presents in hexadecimal, as it would
/// the number stored, and leaves a value presented as anything else as its
/// view presents it.
#[test]
fn print_x_writes_numbers_views_present_in_hexadecimal() {
    let stdout = batch(
        &["build/test-programs/containers-rust-o0"],
        &[
            "break barrier",
            "run",
            "up",
            "print/x counter",
            "print/x signed_counter",
            "print/x boxed",
            "print/x *rc",
            "print/x arc",
            "print counter",
            "print len(*arc)",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "(Atomic<u32>) counter = 0x5\n",
            "(Atomic<i64>) signed_counter = 0xfffffffffffffff7\n",
            "boxed = 0x2a\n",
            "(u64) *rc = 0x7\n",
            "arc = \"shared\"\n",
            "(Atomic<u32>) counter = 5\n",
            "(integer) len(*arc) = 6\n",
        ],
    );
}

/// A long sequence of leaves fills its lines, and a map puts each entry on
/// a line of its own.
#[test]
fn pp_fills_long_sequences_and_breaks_maps_by_width() {
    let commands = ["break barrier", "run", "up", "pp many"];
    let rust = ["build/test-programs/containers-rust-o0"];
    let cpp = ["build/test-programs/containers-cpp-gcc-o0"];
    let narrow = "[print]\nwidth = 30\n";
    assert!(batch_with_settings(narrow, &rust, &commands).contains(
        "(Vec<u32, alloc::alloc::Global>) many = len=300 [\n\
             \x20 0, 1, 2, 3, 4, 5, 6, 7, 8,\n\
             \x20 9, 10, 11, 12, 13, 14, 15,\n"
    ));
    let wide = batch(&rust, &commands);
    assert!(
        wide.contains(
            "  249, 250, 251, 252, 253, 254, 255, <44 omitted>,\n\
             ]\n"
        ),
        "{wide}"
    );

    let commands = ["break barrier", "run", "up", "pp ordered", "pp/x forward"];
    let ordered = "ordered = len=3 {\n  1: 10,\n  2: 20,\n  3: 30,\n}\n";
    let narrow = batch_with_settings(narrow, &cpp, &commands);
    assert_in_order(&narrow, &[ordered, "forward = len=2 [\n  0x4, 0x5,\n]\n"]);
    let wide = batch(&cpp, &commands);
    assert_in_order(&wide, &[ordered, "forward = len=2 [0x4, 0x5]\n"]);
}

/// A stop says where in words, then prints the sections `[stop] show`
/// names, in order, and `context` prints them again.
#[test]
fn a_stop_prints_the_sections_its_settings_name_and_context_prints_them_again() {
    let settings = "[ui]\nunicode = \"never\"\n[stop]\n\
        show = [\"source\", \"locals\", \"displays\", \"registers\", \"disassembly\", \
        \"backtrace\", \"threads\"]\n\
        backtrace-frames = 1\n\
        disassembly-instructions = 2\n";
    let stdout = batch_with_settings(
        settings,
        &["build/test-programs/hit-counts-gcc-o0"],
        &[
            "break caller",
            "display/x call",
            "display missing + 1",
            "display",
            "run",
            "undisplay 2",
            "context",
        ],
    );
    let stop = [
        "stopped at breakpoint 1 (hit 1) in caller at tests/fixtures/c/hit-counts.c:20\n",
        // The header names the line, so the source does not again.
        "    19 | __attribute__((noinline)) void caller(uint64_t call) {\n\
         +=> 20 |     counted(call);\n",
        "(uint64_t) call = 1\n",
        "1: (uint64_t) call = 0x1\n",
        "2: missing + 1: ",
        "\nrip ",
        "=> 0x",
        "#0  ",
        " more frames; `bt` shows every one\n",
        "* ",
    ];

    let mut expected = vec![
        "breakpoint 1 set at caller at tests/fixtures/c/hit-counts.c:20\n",
        "display 1: /x call\n",
        "1: /x call\n2: missing + 1\n",
    ];
    expected.extend(stop);
    expected.extend([
        "removed 1 display\n",
        "tests/fixtures/c/hit-counts.c:20\n    17 | }\n",
        "(uint64_t) call = 1\n",
        "1: (uint64_t) call = 0x1\n",
        "\nrip ",
    ]);

    assert_in_order(&stdout, &expected);
    assert_eq!(stdout.matches("2: missing + 1:").count(), 1, "{stdout}");
}

/// A stop in a process of several threads names the one it is in.
#[test]
fn a_stop_names_its_thread_when_the_process_has_several() {
    let stdout = batch(
        &["build/test-programs/hit-count-threads"],
        &["break contended", "run"],
    );
    let header = stdout
        .lines()
        .find(|line| line.starts_with("stopped at breakpoint 1 (hit 1) in contended at "))
        .unwrap_or_else(|| panic!("no stop in:\n{stdout}"));

    // Every worker is running when the first reaches the breakpoint.
    assert!(header.ends_with(" of 5]"), "{header}");
    assert!(
        header.contains("hit-count-threads.c:16 [thread "),
        "{header}"
    );
}

/// A value that differs from what the last stop showed of the same
/// activation is marked; a caller's value is compared only with itself.
#[test]
fn a_stop_marks_the_values_that_changed_in_the_same_activation() {
    let settings = "[stop]\nshow = [\"locals\"]\n";
    let program = ["build/test-programs/hit-counts-gcc-o0"];
    let stdout = batch_with_settings(
        settings,
        &program,
        &["break caller", "run", "continue", "up", "context"],
    );
    assert_in_order(
        &stdout,
        &[
            "(uint64_t) call = 1\n",
            "(uint64_t) call = 2*\n",
            // main's own `call` was not shown before, so it is not marked
            // although caller's read 1.
            "#1 ",
            "(uint64_t) call = 2\n",
        ],
    );

    let stdout = batch_with_settings(
        settings,
        &program,
        &["break hit-counts.c:27", "run", "next", "next"],
    );
    assert_in_order(
        &stdout,
        &["(uint64_t) call = 1\n", "(uint64_t) call = 2*\n"],
    );
    assert!(!stdout.contains("expected = 0*"), "{stdout}");
}

/// A prefix that names one command runs it, an alias wins over a prefix,
/// and a prefix of several names them.
#[test]
fn unique_prefixes_run_their_command_and_aliases_win() {
    let directory = support::ScratchDir::new("cli-prefixes");
    let path = directory.path().join("config.toml");
    std::fs::write(&path, "[aliases]\nfini = \"info breakpoints\"\n").expect("write the settings");
    let mut child = microscope_command()
        .env("MICROSCOPE_CONFIG", &path)
        .arg(BASIC)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start microscope");
    let mut stdin = child.stdin.take().expect("microscope's stdin");
    for command in ["break main", "fini", "watchp", "dis", "undisp all"] {
        writeln!(stdin, "{command}").expect("write a command");
    }

    drop(stdin);
    let output = child.wait_with_output().expect("wait for microscope");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = assert_success(output);
    assert_in_order(
        &stdout,
        &[
            "breakpoint 1 set",
            "Id  On",
            "no watchpoints",
            "removed 0 displays",
        ],
    );
    assert!(
        stderr.contains("ambiguous command 'dis': disable, disassemble, display"),
        "{stderr}"
    );
}

/// `edit` opens the editor the settings name at the selected line.
#[test]
fn edit_opens_the_editor_at_the_selected_line() {
    let stdout = batch_with_settings(
        "[ui]\neditor = \"echo editing {path} at {line}\"\n",
        &["build/test-programs/hit-counts-gcc-o0"],
        &["break counted", "run", "edit", "up", "edit"],
    );
    let source = format!(
        "{}/tests/fixtures/c/hit-counts.c",
        env!("CARGO_MANIFEST_DIR")
    );
    assert_in_order(
        &stdout,
        &[
            &format!("editing {source} at 11\n"),
            &format!("editing {source} at 20\n"),
        ],
    );
}

#[test]
fn saved_breakpoint_commands_recreate_the_breakpoints() {
    let directory = support::ScratchDir::new("cli-save-breakpoints");
    let file = directory.path().join("breakpoints.microscope");
    let file_name = file.to_str().expect("a UTF-8 path");
    let stdout = batch(
        &["build/test-programs/hit-counts-gcc-o0"],
        &[
            "break counted hits ==3 if call > 1",
            r#"break shared log "total \"{shared_total}\"" disabled"#,
            "tbreak caller",
            &format!("save breakpoints {file_name}"),
        ],
    );
    assert!(
        stdout.contains(&format!("saved 3 breakpoints to {file_name}")),
        "{stdout}"
    );
    let text = std::fs::read_to_string(&file).expect("the file was written");
    assert_eq!(
        text,
        "break counted hits ==3 if call > 1\n\
         break shared log \"total \\\"{shared_total}\\\"\" disabled\n\
         tbreak caller\n"
    );

    let table = |stdout: String| stdout[stdout.find("Id  On").expect("a table")..].to_owned();
    let program = "build/test-programs/hit-counts-gcc-o0";
    let restored = table(batch(&["-c", file_name, program], &["breakpoints"]));
    let mut commands = text.lines().collect::<Vec<_>>();
    commands.push("breakpoints");
    assert_eq!(restored, table(batch(&[program], &commands)));
    assert!(
        restored.contains("2   -      0  shared, 2 locations"),
        "{restored}"
    );
}

#[test]
fn rbreak_breaks_at_matching_functions_and_misspelled_names_suggest_near_ones() {
    let (stdout, stderr) = piped(
        &["build/test-programs/hit-counts-gcc-o0"],
        &[
            "rbreak ^(counted|caller)$",
            "rbreak ^zzz",
            "rbreak (",
            "break countd",
            "break hit-count.c:11",
            "run",
            // libc is loaded now, with far more functions than one pattern
            // may break at.
            "rbreak .",
            "breakpoints",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "breakpoint 1 set at caller at tests/fixtures/c/hit-counts.c:20\n",
            "breakpoint 2 set at counted at tests/fixtures/c/hit-counts.c:11\n",
            "stopped at breakpoint 1 (hit 1) in ",
        ],
    );
    assert!(!stdout.contains("breakpoint 3 set"), "{stdout}");
    assert_in_order(
        &stderr,
        &[
            "no function matches '^zzz'",
            "invalid pattern '(': ",
            "no function named 'countd' was found; did you mean 'counted'?",
            "no source file matching 'hit-count.c' was found; did you mean 'hit-counts.c'?",
            "'.' matches ",
            " functions; rbreak sets at most 200, so narrow the pattern",
        ],
    );
}

#[test]
fn breakpoint_and_watchpoint_tables_show_each_ones_state_place_and_options() {
    let stdout = batch(
        &["build/test-programs/hit-counts-gcc-o0"],
        &[
            "break counted hits ==3 if call > 1",
            "break shared log \"total {shared_total}\"",
            "break caller",
            "tbreak counted",
            "disable 2",
            "run",
            "watch last_call if last_call > 5",
            "disable w1",
            "breakpoints",
            "watchpoints",
        ],
    );
    let (_, tables) = stdout
        .split_once("disabled watchpoint 1\n")
        .expect("tables");
    let path = "tests/fixtures/c/hit-counts.c";
    let lines = tables.lines().collect::<Vec<_>>();
    assert_eq!(
        lines[..3],
        [
            "Id  On  Hits  Where                                        Options",
            &format!("1   +      0  counted at {path}:11  hits ==3  if call > 1"),
            "2   -      0  shared, 2 locations                          log \"total {shared_total}\"",
        ],
        "{tables}"
    );

    // Several locations are listed beneath, each with its address.
    assert!(lines[3].starts_with("              |- 0x"), "{tables}");
    assert!(
        lines[3].ends_with(&format!("  shared at {path}:16")),
        "{tables}"
    );
    assert!(lines[4].starts_with("              `- 0x"), "{tables}");
    assert_eq!(
        lines[5..7],
        [
            format!("3   +      1  caller at {path}:20"),
            format!("4   +      0  counted at {path}:11  temporary"),
        ],
        "{tables}"
    );
    assert_eq!(
        lines[7], "Id  On  Hits  Watching   Where                      Options",
        "{tables}"
    );
    assert!(
        lines[8].starts_with("1   -      0  last_call  8 bytes at 0x"),
        "{tables}"
    );
    assert!(lines[8].ends_with("  change  if last_call > 5"), "{tables}");
}

#[test]
fn breakpoints_take_options_inline_and_lines_from_the_selected_frame() {
    let (stdout, stderr) = piped(
        &["build/test-programs/hit-counts-gcc-o0"],
        &[
            "break counted hits ==3 if call > 1",
            "run",
            "break",
            "break +1",
            "break 20",
            "break counted if",
            "break +x",
            "break 0",
            "delete 9 2",
            "delete 2-3 4",
            "watch last_call",
            "watch shared_total",
            "unwatch 1-2",
            "break caller hits ==5 log \"caller {call} of {last_call}\"",
            "continue",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "breakpoint 1 set at ",
            ", stops at hits ==3 where call > 1\n",
            "stopped at breakpoint 1 (hit 3) in ",
            "breakpoint 2 set at counted at tests/fixtures/c/hit-counts.c:11\n",
            "breakpoint 3 set at counted at tests/fixtures/c/hit-counts.c:12\n",
            "breakpoint 4 set at caller at tests/fixtures/c/hit-counts.c:20\n",
            "deleted breakpoints 2, 3, 4\n",
            "deleted watchpoints 1, 2\n",
            "breakpoint 5 set at ",
            ", stops at hits ==5, logs \"caller {call} of {last_call}\"\n",
            // Its fifth hit is the fifth call after the stop at the third.
            "caller 8 of 7\n",
            "inferior exited with status 0\n",
        ],
    );
    assert_in_order(
        &stderr,
        &[
            "usage: break [location] [if condition...] [hits hit-condition] [log message] [disabled]",
            "invalid line offset '+x'",
            "source line numbers are one-based",
            // A list naming a missing id deletes nothing.
            "breakpoint 9 was not found",
        ],
    );
}

#[test]
fn breakpoints_and_watchpoints_are_disabled_enabled_and_advanced_past() {
    let (stdout, stderr) = piped(
        &["build/test-programs/hit-counts-gcc-o0"],
        &[
            "tbreak counted ==3",
            "break caller",
            "disable 2",
            "breakpoints",
            "run",
            "breakpoints",
            "enable 2",
            "continue",
            "adv counted",
            "print call",
            "watch last_call",
            "disable w1 2",
            "watchpoints",
            "enable all",
            // A list naming a missing id changes nothing, so the watchpoint
            // still stops the next continue.
            "disable 9 w1",
            "disable 3-1",
            "disable x",
            "advance",
            "continue",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "temporary breakpoint 1 set at counted at tests/fixtures/c/hit-counts.c:11, stops at \
             hits ==3\n",
            "disabled breakpoint 2\n",
            "1   +      0  counted at tests/fixtures/c/hit-counts.c:11  hits ==3  temporary\n",
            "2   -      0  caller at tests/fixtures/c/hit-counts.c:20\n",
            "stopped at breakpoint 1 (hit 3) in ",
            "deleted temporary breakpoint 1\n",
            "2   -      0  caller at tests/fixtures/c/hit-counts.c:20\n",
            "enabled breakpoint 2\n",
            // Hits while disabled are not counted.
            "stopped at breakpoint 2 (hit 1) in ",
            "stopped after advance in counted at tests/fixtures/c/hit-counts.c:11\n",
            "(uint64_t) call = 4\n",
            "disabled breakpoint 2 and watchpoint 1\n",
            "1   -      0  last_call  8 bytes at 0x",
            "  change\n",
            "enabled breakpoint 2 and watchpoint 1\n",
            "stopped by watchpoint 1 (change, hit 1) on last_call",
            "\n  old: 3\n  new: 4\n",
        ],
    );
    assert_in_order(
        &stderr,
        &[
            "breakpoint 9 was not found",
            "range 3-1 is empty",
            "usage: disable <ids...>",
            "usage: advance <function|0xaddress|file:line|file:function>",
        ],
    );
}

#[test]
fn batch_mode_sets_and_clears_breakpoint_conditions() {
    let stdout = batch(
        &["build/test-programs/hit-counts-gcc-o0"],
        &[
            "break counted",
            "condition 1 call % 10 == 0 && last_call == call - 1",
            "run",
            "print call",
            "breakpoints",
            "condition 1",
            "continue",
            "print call",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "breakpoint 1 stops where call % 10 == 0 && last_call == call - 1 holds\n",
            "stopped at breakpoint 1 (hit 10) in ",
            "(uint64_t) call = 10\n",
            "1   +     10  counted at tests/fixtures/c/hit-counts.c:11  if call % 10 == 0 && last_call == call - 1\n",
            "breakpoint 1 stops unconditionally\n",
            "stopped at breakpoint 1 (hit 11) in ",
            "(uint64_t) call = 11\n",
        ],
    );
}

#[test]
fn batch_mode_sets_amends_and_skips_watchpoint_conditions() {
    let stdout = batch(
        &["build/test-programs/hit-counts-gcc-o0"],
        &[
            "break caller",
            "run",
            "delete all",
            "watch last_call if call % 10 == 0",
            "continue",
            "ignore w1 15",
            "watchpoints",
            "continue",
            "condition w1",
            "hits w1 ==32",
            "continue",
            "hits w1 always",
            "condition w1 no_such_value > 1",
            "continue",
            "unwatch all",
            "continue",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "watchpoint 1 set on last_call: 8 bytes at 0x",
            " using 1 hardware slot, stops where call % 10 == 0\n",
            "stopped by watchpoint 1 (change, hit 10) on last_call in counted at \
             tests/fixtures/c/hit-counts.c:12",
            "\n  old: 9\n  new: 10\n",
            "watchpoint 1 ignores its next 15 hits\n",
            "1   +     10  last_call  8 bytes at 0x",
            "  change  hits >=26  if call % 10 == 0\n",
            "stopped by watchpoint 1 (change, hit 30) on last_call",
            "\n  old: 29\n  new: 30\n",
            "watchpoint 1 stops unconditionally\n",
            "watchpoint 1 stops at hits ==32, hit 30 times so far\n",
            "stopped by watchpoint 1 (change, hit 32) on last_call",
            "\n  old: 31\n  new: 32\n",
            "watchpoint 1 stops at every hit, hit 32 times so far\n",
            "watchpoint 1 stops where no_such_value > 1 holds\n",
            "warning: the condition of watchpoint 1 could not be evaluated: \
             no variable is named `no_such_value` here\n",
            "stopped by watchpoint 1 (change, hit 33) on last_call",
            "deleted 1 watchpoint\n",
            "inferior exited with status 0\n",
        ],
    );
}

#[test]
fn watchpoint_condition_commands_explain_rejected_input() {
    let (_, stderr) = piped(
        &["build/test-programs/hit-counts-gcc-o0"],
        &[
            "break caller",
            "run",
            "condition w9 x > 1",
            "hits w9 >=2",
            "ignore w9 2",
            "hits wx >=2",
            "watch last_call if",
            "watch last_call when call > 2",
            "watch last_call if call = 3",
        ],
    );
    assert_in_order(
        &stderr,
        &[
            "watchpoint 9 was not found",
            "watchpoint 9 was not found",
            "watchpoint 9 was not found",
            "usage: hits <id> <hit-condition|always>",
            "usage: watch [-w] <expression|0xaddress:byte-count> [if condition...]",
            "usage: watch [-w] <expression|0xaddress:byte-count> [if condition...]",
            "invalid condition: conditions and log messages cannot assign; compare with `==`",
        ],
    );
}

#[test]
fn hit_condition_commands_explain_rejected_input() {
    let (stdout, stderr) = piped(
        &["build/test-programs/hit-counts-gcc-o0"],
        &[
            "hits 1 always",
            "ignore 1 2",
            "condition 4 x > 1",
            "condition",
            "break counted 5",
            "break counted =<5",
            "break counted",
            "hits 1 ==0",
            "ignore 1 many",
            "hits one >=2",
            "condition 1 call = 3",
            // Ignoring zero hits stops at the next one.
            "hits 1 ==9",
            "ignore 1 0",
            "run",
        ],
    );
    assert_in_order(
        &stderr,
        &[
            "breakpoint 1 was not found",
            "breakpoint 1 was not found",
            "breakpoint 4 was not found",
            "usage: condition <id> [expression...]",
            "invalid hit condition: a bare count is ambiguous; write ==5 to stop only at that \
             hit or >=5 to stop at it and every later hit",
            "invalid hit condition: '=<5' is not an operator",
            "invalid hit condition: no hit can satisfy ==0",
            "usage: ignore <id> <count>",
            "usage: hits <id> <hit-condition|always>",
            "invalid condition: conditions and log messages cannot assign; compare with `==`",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "breakpoint 1 stops at its next hit\n",
            "stopped at breakpoint 1 (hit 1) in ",
        ],
    );
}

#[test]
fn strings_print_as_quoted_escaped_text() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/c/strings.c");
    let line = std::fs::read_to_string(&path)
        .expect("source")
        .lines()
        .position(|line| line.contains("strings stop here"))
        .expect("marker")
        + 1;
    let stdout = batch(
        &["build/test-programs/strings-c-gcc-o0"],
        &[&format!("break strings.c:{line}"), "run", "print"],
    );
    assert_in_order(
        &stdout,
        &[
            "(const char *) greeting = 0x",
            " \"hello, world\"\n",
            "(const char *) escaped = 0x",
            r#" "tab\there \"quoted\" \\ é\x80""#,
            "(const char *) long_text = 0x",
            &format!(" \"{}\"...\n", "x".repeat(256)),
            "(const char *) edge = 0x",
            " \"eeeee\"... <unreadable at 0x",
            "(char[16]) buffer = \"abc\"\n",
            "(char[4]) unterminated = \"wxyz\"\n",
            "(const char *) null_text = 0x0000000000000000\n",
            "(const char *) invalid = 0x0000000000000001 \"\"... <unreadable at 0x1>\n",
            "(unsigned char[3]) bytes = \"A\\xff\"\n",
        ],
    );
}

/// Characters wider than a byte print as their numbers and what they are,
/// and pointers to text of them as the text.
#[test]
fn wide_characters_print_as_their_numbers_and_characters() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cpp/strings.cpp");
    let line = std::fs::read_to_string(&path)
        .expect("source")
        .lines()
        .position(|line| line.contains("strings stop here"))
        .expect("marker")
        + 1;
    let stdout = batch(
        &[
            "--color",
            "never",
            "build/test-programs/strings-cpp-clang-o0",
        ],
        &[&format!("break strings.cpp:{line}"), "run", "print"],
    );
    assert_in_order(
        &stdout,
        &[
            "(wchar_t) wide = 233 'é'\n",
            "(char8_t) eight = 97 'a'\n",
            "(char16_t) sixteen = 955 'λ'\n",
            "(char32_t) thirty_two = 129408 '🦀'\n",
            // A surrogate is a code unit, not a character.
            "(char16_t) surrogate = 55296\n",
            "(const char16_t *) sixteen_text = 0x",
            " \"λx\"\n",
            "(const char8_t *) eight_text = 0x",
            " \"éight\"\n",
        ],
    );
    let stdout = batch(
        &["--color", "never", "build/test-programs/strings-rust-o0"],
        &["break strings_target", "run", "print letter"],
    );
    assert!(stdout.contains("(char) letter = 955 'λ'\n"), "{stdout}");
}

/// `print` shows a value as its view presents it, with its elements up to
/// the inspection's budget; `print/r` and `set views off` show it as
/// stored; and `info view` says which view presents it, or why none does.
#[test]
fn views_present_values_raw_is_one_step_away_and_info_view_explains() {
    let stdout = batch(
        &["--color", "never", "build/test-programs/containers-rust-o0"],
        &[
            "break barrier",
            "run",
            "up",
            "print ints",
            "print many",
            "print/r ints",
            "print words",
            "info view ints",
            "info view past_capacity.value.0",
            "set views off",
            "print ints",
            "info view ints",
            "set views on",
            "print ints[1] + len(ints)",
            "print",
        ],
    );

    let raw_ints = "(Vec<i32, alloc::alloc::Global>) ints = {buf = {inner = {ptr = ";
    assert_in_order(
        &stdout,
        &[
            "(Vec<i32, alloc::alloc::Global>) ints = len=3 [1, 2, 3]\n",
            "(Vec<u32, alloc::alloc::Global>) many = len=300 [0, 1, 2, 3, ",
            " 254, 255, <44 omitted>]\n",
            raw_ints,
            "}, len = 3}\n",
            "(Vec<alloc::string::String, alloc::alloc::Global>) words = len=2 [\"one\", \"two\"]\n",
            "`ints` has type Vec<i32, alloc::alloc::Global>\n",
            "presented by rust-std.views:",
            " `rust alloc::vec::Vec<T, _>`\nas len=3 [1, 2, 3]\nviews tried, in order:\n",
            "`past_capacity.value.0` has type Vec<i32, alloc::alloc::Global>\n",
            "binds, but shows the value as stored: check `len <= capacity` failed: \
             `len` is 9, `capacity` is 8\n",
            "values show as stored\n",
            raw_ints,
            "views are off, so it shows as stored; `set views on` turns them on\n",
            "values show as their views present them\n",
            "(integer) ints[1] + len(ints) = 5\n",
            "(PathBuf) path = \"/tmp/microscope\"\n",
            "(VecDeque<i32, alloc::alloc::Global>) ring = len=4 [1, 2, 3, 4]\n",
        ],
    );
}

/// Values are presented with the session's view files first, then the
/// project's in `.microscope/views` where the program runs and the user's, then
/// the program's own;
/// `views clear` forgets the session's, and a file with an error is
/// reported without ending the session.
/// Files given together to `views load` stack as `--views` does: later
/// files come first.
#[test]
fn view_files_loaded_together_stack_as_on_the_command_line() {
    let directory = support::ScratchDir::new("cli-view-order");
    let first = directory.path().join("first.views");
    let second = directory.path().join("second.views");
    for path in [&first, &second] {
        fs::write(path, "microscope-views 1\n").expect("write a view file");
    }

    let (first, second) = (first.display().to_string(), second.display().to_string());
    let load_both = format!("views load {first} {second}");
    let output = batch(
        &[
            "--views",
            &first,
            "--views",
            &second,
            "build/test-programs/basic",
        ],
        &["views", "views clear", &load_both, "views"],
    );
    let listings = output
        .split("values are presented with, in order:")
        .skip(1)
        .map(|listing| listing.lines().take(3).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    assert_eq!(listings.len(), 2, "{output}");
    assert!(listings[0][1].contains("second.views"), "{output}");
    assert_eq!(listings[0], listings[1], "{output}");
}

/// The project root is the nearest directory with `.microscope/` or `.git`, so
/// a repository inside another project, as a git worktree often is, is a
/// project of its own and never reads the outer one's settings or state.
#[test]
fn a_repository_inside_a_project_is_its_own_project() {
    let directory = support::ScratchDir::new("cli-project-root");
    let outer = directory.path().join("outer");
    let inner = outer.join("worktrees/inner");
    let subdirectory = inner.join("src");
    fs::create_dir_all(outer.join(".microscope/state")).expect("make the outer project");
    fs::create_dir_all(&subdirectory).expect("make the inner repository");

    // A worktree's `.git` is a file.
    fs::write(inner.join(".git"), "gitdir: elsewhere\n").expect("mark the repository");
    let root = |start: &std::path::Path| {
        let output = microscope_command()
            .current_dir(start)
            .env("XDG_CONFIG_HOME", directory.path().join("config"))
            .args(["config", "path"])
            .output()
            .expect("run microscope");
        assert!(output.status.success(), "{output:?}");

        String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    };

    assert_eq!(
        root(&subdirectory),
        format!("project root: {}", inner.display())
    );

    // From the outer project's own directories, it is still the root.
    assert_eq!(
        root(&outer.join("worktrees")),
        format!("project root: {}", outer.display())
    );
}

#[test]
fn view_files_come_from_the_session_the_project_and_the_user() {
    let directory = support::ScratchDir::new("cli-view-files");
    let project = directory.path().join(".microscope/views");
    let user = directory.path().join("config/microscope/views");
    fs::create_dir_all(&project).expect("make the project's view directory");
    fs::create_dir_all(&user).expect("make the user's view directory");
    fs::write(
        project.join("points.views"),
        "microscope-views 1\nview c point {\n    format y as hex\n}\n",
    )
    .expect("write the project's views");
    fs::write(
        user.join("mine.views"),
        "microscope-views 1\nview c point {\n    show empty(\"the user's\")\n}\nview c intvec {\n    show empty(\"the user's vector\")\n}\n",
    )
    .expect("write the user's views");
    let session = directory.path().join("session.views");
    fs::write(
        &session,
        "microscope-views 1\nview c point {\n    show empty(\"the session's\")\n}\n",
    )
    .expect("write the session's views");
    let broken = directory.path().join("broken.views");
    fs::write(
        &broken,
        "microscope-views 1\nview c point {\n    show nothing\n}\n",
    )
    .expect("write a broken view file");
    let load_broken = format!("views load {}", broken.display());

    // The project is the one microscope runs in, from any of its directories,
    // wherever the program runs.
    let subdirectory = directory.path().join("src");
    fs::create_dir_all(&subdirectory).expect("make a project subdirectory");
    let output = microscope_command()
        .current_dir(&subdirectory)
        .env("XDG_CONFIG_HOME", directory.path().join("config"))
        .arg("--batch")
        .arg("--cwd")
        .arg("/")
        .arg("--views")
        .arg(&session)
        .args(["-e", "break barrier", "-e", "run", "-e", "up"])
        .args(["-e", "print here", "-e", "views clear", "-e", "print here"])
        .args([
            "-e",
            "print numbers",
            "-e",
            &load_broken,
            "-e",
            "print here",
        ])
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/build/test-programs/embedded-views"
        ))
        .stdin(Stdio::null())
        .output()
        .expect("run microscope");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = assert_success(output);
    assert_in_order(
        &stdout,
        &[
            "here = the session's",
            "forgot the loaded view files",
            "here = {x: 1, y: 0x2}",
            // The user's view comes before the program's own.
            "numbers = the user's vector",
            "loaded 1 view file",
            "here = {x: 1, y: 0x2}",
        ],
    );
    assert!(
        stderr.contains("broken.views:3:10: expected a shape"),
        "{stderr}"
    );
}

/// `microscope views check` says which view presents each of a program's types
/// that a pattern names, with no process, and fails when a view it was
/// given presents nothing or binds nothing it names; `views explain` says
/// why for one type.
#[test]
fn views_check_and_explain_a_programs_types_without_a_process() {
    let directory = support::ScratchDir::new("cli-views-check");
    let views = directory.path().join("app.views");
    fs::write(
        &views,
        "microscope-views 1\nview c point {\n    show value(z)\n}\nview c widget {\n    show empty(\"w\")\n}\n",
    )
    .expect("write the views to check");
    let program = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/build/test-programs/embedded-views"
    );
    let run = |arguments: &[&std::ffi::OsStr]| {
        microscope_command()
            .current_dir(directory.path())
            .env("XDG_CONFIG_HOME", directory.path().join("config"))
            .arg("views")
            .args(arguments)
            .stdin(Stdio::null())
            .output()
            .expect("run microscope views")
    };

    let clean = run(&["check".as_ref(), program.as_ref()]);
    assert_in_order(
        &assert_success(clean),
        &[
            "presented:",
            "intvec by embedded-views.views[0]:4 `c intvec`",
        ],
    );
    let failed = run(&[
        "check".as_ref(),
        program.as_ref(),
        "--views".as_ref(),
        views.as_os_str(),
    ]);
    assert!(!failed.status.success(), "{failed:?}");
    let report = String::from_utf8_lossy(&failed.stdout).into_owned();
    assert_in_order(
        &report,
        &[
            "not presented, though views name them:",
            "point",
            "app.views:2 `c point`: line 3: `z`: `z` is neither a member",
            "views that present no type:",
            "app.views:5 `c widget`",
        ],
    );

    // A view file that cannot be used fails an explanation too.
    let broken = directory.path().join("broken.views");
    fs::write(
        &broken,
        "microscope-views 1\nview c intvec {\n    show nothing\n}\n",
    )
    .expect("write a broken view file");
    let refused = run(&[
        "explain".as_ref(),
        program.as_ref(),
        "intvec".as_ref(),
        "--views".as_ref(),
        broken.as_os_str(),
    ]);
    assert!(!refused.status.success(), "{refused:?}");
    let explained = run(&["explain".as_ref(), program.as_ref(), "intvec".as_ref()]);
    assert_in_order(
        &assert_success(explained),
        &[
            "intvec in ",
            "presented by embedded-views.views[0]:4 `c intvec`",
            "views tried, in order:",
            "embedded-views.views[0]:4 `c intvec`: binds",
        ],
    );
}

/// A view's drawings are only drawn by the web page, but `views check`
/// and `views explain` show them, and the renderers they name, and a
/// drawing that does not bind fails the check.
#[test]
fn views_check_and_explain_show_drawings_and_their_renderers() {
    let directory = support::ScratchDir::new("cli-views-drawings");
    let views = directory.path().join("plots.views");
    fs::write(
        &views,
        "microscope-views 1\nextend rust chess::Board {\n    visualize \"plot\" { values = nothing }\n    visualize \"line-plot\" { values = colors }\n}\n",
    )
    .expect("write the views");
    fs::write(
        directory.path().join("plot.js"),
        "microscope.draw(() => null);\n",
    )
    .expect("write the renderer");
    let program = concat!(env!("CARGO_MANIFEST_DIR"), "/build/test-programs/chess");
    let run = |arguments: &[&std::ffi::OsStr]| {
        microscope_command()
            .current_dir(directory.path())
            .env("XDG_CONFIG_HOME", directory.path().join("config"))
            .arg("views")
            .args(arguments)
            .stdin(Stdio::null())
            .output()
            .expect("run microscope views")
    };

    // The program carries its views and the renderer they name.
    let clean = run(&["check".as_ref(), program.as_ref()]);
    assert_in_order(
        &assert_success(clean),
        &[
            "extended, with no view presenting them:",
            "Board by chess.views[0]:4 `extend rust chess::Board`",
            "renderers, which only the web page runs:",
            "chess-board (chess.views[1]):",
            "    // Draws a chess position",
        ],
    );
    let explained = run(&[
        "explain".as_ref(),
        program.as_ref(),
        "chess::Board".as_ref(),
    ]);
    assert_in_order(
        &assert_success(explained),
        &[
            "no view presents it; an `extend` binds",
            "drawings, which only the web page draws:",
            "chess-board(squares, turn) by chess.views[0]:4 `extend rust chess::Board`, with the renderer from chess.views[1]",
            "bits(values, origin) by chess.views[0]:4 `extend rust chess::Board`, with the renderer from built-in",
        ],
    );
    let failed = run(&[
        "check".as_ref(),
        program.as_ref(),
        "--views".as_ref(),
        views.as_os_str(),
    ]);
    assert!(!failed.status.success(), "{failed:?}");
    assert_in_order(
        &String::from_utf8_lossy(&failed.stdout),
        &[
            "drawings that do not bind:",
            "plot by ",
            "plots.views:2 `extend rust chess::Board`, line 3: `nothing` is neither a member of `Board`",
            "renderers, which only the web page runs:",
            "plot (",
            "plot.js):",
            "    microscope.draw(() => null);",
            "chess-board (chess.views[1]):",
        ],
    );
}

/// A kernel beside a view file is loaded with it, as `NAME.wasm`; the runs
/// a presentation takes are recorded, and replay with no program; and
/// `views check` shows each kernel as the source it is built from.
#[test]
fn kernels_beside_view_files_present_values_and_their_runs_replay() {
    let directory = support::ScratchDir::new("cli-kernels");
    let root = env!("CARGO_MANIFEST_DIR");
    let views = directory.path().join("forest.views");
    fs::write(
        &views,
        "microscope-views 1\nview c tree {\n    show sequence(count) for at in kernel(\"preorder\", root, offsetof(node, child), offsetof(node, sibling))\n        => ((node *)at)->value * 10\n}\n",
    )
    .expect("write the views");
    fs::copy(
        format!("{root}/build/test-programs/tutorial-tree.wasm"),
        directory.path().join("preorder.wasm"),
    )
    .expect("put the kernel beside the views");
    let junk = directory.path().join("junk.views");
    fs::write(
        &junk,
        "microscope-views 1\nview c intvec {\n    show sequence(n) for at in kernel(\"junk\") => at\n}\n",
    )
    .expect("write views that call a broken kernel");
    fs::write(directory.path().join("junk.wasm"), b"\0asm junk").expect("write a broken kernel");
    let runs = directory.path().join("family.runs");
    let record = format!("views record {} family", runs.display());
    let load_junk = format!("views load {}", junk.display());
    let program = format!("{root}/build/test-programs/tutorial");
    let output = microscope_command()
        .current_dir(root)
        .env("XDG_CONFIG_HOME", directory.path().join("config"))
        .arg("--batch")
        .arg("--views")
        .arg(&views)
        .args(["-e", "break barrier", "-e", "run", "-e", "up"])
        .args([
            "-e",
            "print family",
            "-e",
            &record,
            "-e",
            "info view family",
        ])
        .args(["-e", &load_junk])
        .arg(&program)
        .stdin(Stdio::null())
        .output()
        .expect("run microscope");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = assert_success(output);
    assert_in_order(
        &stdout,
        &[
            "family = len=5 [10, 20, 30, 40, 50]",
            "recorded 2 kernel runs to",
            "presented by ",
            "forest.views:2 `c tree`",
        ],
    );
    assert!(
        stderr.contains("junk.wasm:0:0: kernel `junk`: it is not a module a kernel may be"),
        "{stderr}"
    );
    let replay = |arguments: &[&std::ffi::OsStr]| {
        microscope_command()
            .args(["views".as_ref(), "replay".as_ref(), runs.as_os_str()])
            .args(arguments)
            .stdin(Stdio::null())
            .output()
            .expect("run microscope views replay")
    };

    let kernel = directory.path().join("preorder.wasm");
    assert_in_order(
        &assert_success(replay(&["--kernel".as_ref(), kernel.as_os_str()])),
        &[
            "run 1: kernel `preorder` reproduced",
            "run 2: kernel `preorder` reproduced",
        ],
    );

    // A run replays only with the kernel it recorded.
    let unknown = replay(&[]);
    assert!(!unknown.status.success(), "{unknown:?}");
    assert!(
        String::from_utf8_lossy(&unknown.stderr).contains("no built-in kernel is named `preorder`"),
        "{unknown:?}"
    );
    let other = replay(&[
        "--kernel".as_ref(),
        format!("{root}/views/kernels/rust-btree.wasm").as_ref(),
    ]);
    assert!(!other.status.success(), "{other:?}");
    assert!(
        String::from_utf8_lossy(&other.stdout).contains("differs"),
        "{other:?}"
    );
    let check = microscope(&["views", "check", "build/test-programs/tutorial"]);
    assert_in_order(
        &assert_success(check),
        &[
            "kernels, and what they are built from:",
            "  tree (tutorial.views[1]):",
            "    // The values of a tree whose nodes keep their children in a list, each",
        ],
    );
}

/// A program built with line tables only describes no variables or types,
/// so nothing is presented, and microscope says so rather than guessing.
#[test]
fn a_program_without_variable_information_presents_nothing() {
    let output = batch_output(
        &["build/test-programs/containers-rust-limited"],
        &["break barrier", "run", "up", "print text"],
    );
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no variable is named `text` here"),
        "{stderr}"
    );
    let check = microscope(&[
        "views",
        "check",
        "build/test-programs/containers-rust-limited",
    ]);
    assert_in_order(
        &assert_success(check),
        &["no view's pattern names any type"],
    );
}

/// `print` shows a map's entries as `key: value` and a linked structure's
/// elements, and says why a broken one shows as stored.
#[test]
fn views_print_maps_and_linked_structures() {
    let stdout = batch(
        &[
            "--color",
            "never",
            "build/test-programs/containers-cpp-gcc-o0",
        ],
        &[
            "break barrier",
            "run",
            "up",
            "print ordered",
            "print named",
            "print linked_words",
            "print forward[1] * len(forward)",
            "print looped",
            "info view overcounted",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            ") ordered = len=3 {1: 10, 2: 20, 3: 30}\n",
            ") named = len=2 {\"one\": 1, \"two\": 2}\n",
            ") linked_words = len=2 [\"a\", \"b\"]\n",
            "(integer) forward[1] * len(forward) = 10\n",
            ") looped = {",
            " <view libstdc++.views:",
            "`c++ std::list<T, _>`: cycle at element 3: it leads back to a node already visited>\n",
            "binds, but shows the value as stored: the view declares 4 elements and generates 2\n",
        ],
    );
}

/// `ptype` names a type with its path and lists its arguments, whatever
/// the producer called it.
#[test]
fn ptype_shows_qualified_names_and_template_arguments() {
    for (program, commands, expected) in [
        (
            "build/test-programs/templates-cpp-clang-o0",
            &[
                "break templates_target",
                "run",
                "ptype std::`vector<int>`",
                "ptype `Fixed<3, short>`",
            ][..],
            &[
                "type = class std::vector<int, std::allocator<int> > {",
                "arguments: int, std::allocator<int>\n",
                "type = struct Fixed<3, short> {\n    short[3] items;\n}\narguments: 3, short\n",
            ][..],
        ),
        (
            "build/test-programs/generics-rust-o0",
            &[
                "break generics_target",
                "run",
                "ptype alloc::vec::`Vec<i32>`",
            ][..],
            &[
                "type = struct alloc::vec::Vec<i32, alloc::alloc::Global> {",
                "arguments: i32, alloc::alloc::Global\n",
            ][..],
        ),
    ] {
        let stdout = batch(&[program], commands);
        assert_in_order(&stdout, expected);
    }
}

#[test]
fn set_changes_values_the_program_then_uses() {
    let stdout = batch(
        &["build/test-programs/variables-gcc-o0"],
        &[
            "break pointer_target",
            "run",
            "up",
            "set signed_int = signed_int + 1",
            "set var boolean = false",
            "delete all",
            "continue",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "(int) signed_int = -1234566\n",
            "(_Bool) boolean = false\n",
            "inferior exited with status 1\n",
        ],
    );
}

#[test]
fn print_and_p_render_scalars_and_print_lists_every_variable() {
    let stdout = batch(
        &["build/test-programs/variables-gcc-o0"],
        &["break variables.c:108", "run", "p signed_int", "print"],
    );
    assert_eq!(
        stdout.matches("(int) signed_int = -1234567").count(),
        2,
        "{stdout}"
    );

    for expected in [
        "(char) character = 65 'A'",
        "(float) single = 1.25",
        "(double) double_precision = -2.5",
        "(long double) extended = 3.125",
    ] {
        assert!(stdout.contains(expected), "{expected:?} in:\n{stdout}");
    }
}

/// A frame with no variables says so, rather than printing nothing.
#[test]
fn print_says_when_a_frame_has_no_variables() {
    for command in ["print", "pp"] {
        let stdout = batch(
            &["build/test-programs/fatal-signal"],
            &["break main", "run", command],
        );
        assert!(stdout.contains("no variables in scope"), "{stdout}");
    }
}

/// An array of several dimensions prints as rows of rows, as its program
/// writes it, and a row the elements shown end inside of is closed.
#[test]
fn arrays_of_several_dimensions_print_as_nested_rows() {
    let arguments = ["--color", "never", "build/test-programs/records-c-gcc-o0"];
    let commands = ["break main", "run", "print matrix"];
    assert!(
        batch(&arguments, &commands).contains("(int32_t[2][3]) matrix = [[1, 2, 3], [4, 5, 6]]\n")
    );

    let stdout = batch_with_settings("[print]\nmax-elements = 4\n", &arguments, &commands);
    assert!(
        stdout.contains("(int32_t[2][3]) matrix = [[1, 2, 3], [4], <2 omitted>]\n"),
        "{stdout}"
    );
}

/// Floats print in every format their programs hold them in, each with
/// its own precision: a quad precision tenth is a tenth.
#[test]
fn floats_of_every_format_print_exactly() {
    for fixture in ["floats-c-gcc", "floats-c-clang"] {
        let stdout = batch(
            &[&format!("build/test-programs/{fixture}")],
            &["break floats_target", "run", "up", "print"],
        );
        assert_in_order(
            &stdout,
            &[
                "(_Float16) half = 1.5\n",
                "(__bf16) brain = -3.14\n",
                "(float) single = 0.25\n",
                "(double) precision = 2.5\n",
                "(long double) extended = 3.125\n",
                // GCC names `__float128` `_Float128`.
                "128) quad = 0.1\n",
            ],
        );
    }

    let stdout = batch(
        &["build/test-programs/floats-rust"],
        &["break floats_target", "run", "print half", "print quad"],
    );
    assert_in_order(&stdout, &["(f16) half = 1.5\n", "(f128) quad = 0.1\n"]);
    let stdout = batch(
        &["build/test-programs/floats-zig"],
        &["break floats.zig:8", "run", "print half", "print quad"],
    );
    assert_in_order(&stdout, &["(f16) half = 1.5\n", "(f128) quad = 0.1\n"]);
}

#[test]
fn info_symbol_and_x_describe_memory_by_address() {
    let executable = fixture("build/test-programs/variables-gcc-nopie");
    let main = symbol_address(&executable, "main");
    let data = symbol_address(&executable, "pointer_parameter_value");

    // _dl_relocate_static_pie is five bytes long and followed by padding.
    let padding = symbol_address(&executable, "_dl_relocate_static_pie") + 8;
    let commands = [
        "break main".to_owned(),
        "run".to_owned(),
        format!("info symbol {:#x}", main + 4),
        format!("info symbol {:#x}", data + 2),
        format!("info symbol {padding:#x}"),
        "info symbol 0x8".to_owned(),
        format!("x {data:#x} 4"),
    ];
    let commands = commands.iter().map(String::as_str).collect::<Vec<_>>();
    let stdout = batch(&["build/test-programs/variables-gcc-nopie"], &commands);
    let path = executable.canonicalize().expect("canonical fixture path");
    let path = path.display();
    for expected in [
        format!("main+0x4 in section .text of {path}"),
        format!("pointer_parameter_value+0x2 in section .data of {path}"),
        format!("no symbol contains {padding:#x} in section .text of {path}"),
        "no loaded module contains 0x8".to_owned(),
    ] {
        assert!(
            stdout.lines().any(|line| line == expected),
            "missing {expected:?} in:\n{stdout}"
        );
    }

    assert!(
        stdout.contains(&format!("{data:#018x}: 2a 00 00 00")),
        "{stdout}"
    );
    assert!(stdout.contains("|*...            |"), "{stdout}");
}

#[test]
fn print_dereferences_pointer_chains_and_points_at_what_cannot_be() {
    let (stdout, stderr) = piped(
        &["build/test-programs/variables-gcc-o0"],
        &[
            "break variables.c:68",
            "run",
            "p pointer",
            "p *pointer",
            "p **pointer_pointer",
            "p *null_pointer",
            "p *invalid_pointer",
            "p *void_pointer",
            "p *pointee",
            "p **pointer",
        ],
    );
    assert!(stdout.contains("(int *) pointer = 0x"), "{stdout}");
    assert!(stdout.contains("(int) *pointer = 42"), "{stdout}");
    assert!(stdout.contains("(int) **pointer_pointer = 42"), "{stdout}");

    // A failed dereference is labeled with the pointee type (int), not the
    // operand's pointer type (int *).
    assert!(
        stdout.contains("(int) *null_pointer = <unavailable: cannot dereference a null pointer>"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("(int *) *null_pointer"),
        "failed dereference must not render the pointer's own type: {stdout}"
    );
    assert!(
        stdout.contains("*invalid_pointer = <unavailable:"),
        "{stdout}"
    );

    // Dereferencing what cannot be dereferenced is the expression's error,
    // pointed at.
    assert_in_order(
        &stderr,
        &[
            "`void_pointer` points at void",
            "\n     ^^^^^^^^^^^^\n",
            "`pointee` is not a pointer",
            "\n     ^^^^^^^\n",
            "`*pointer` is not a pointer",
            "\n     ^^^^^^^^\n",
        ],
    );
}

#[test]
fn print_resolves_recursive_types_from_dwarf_four_and_five_type_units() {
    for program in [
        "build/test-programs/types-cpp-gcc-dwarf4",
        "build/test-programs/types-cpp-gcc-dwarf5",
    ] {
        let stdout = batch(
            &[program],
            &[
                "break types.cpp:31",
                "run",
                "p recursive.value",
                "p mutual.peer",
            ],
        );
        assert!(
            stdout.contains("(volatile alias_chain) recursive.value = 9"),
            "{program}: {stdout}"
        );
        assert!(
            stdout.contains("(right *) mutual.peer = 0x"),
            "{program}: {stdout}"
        );
    }
}

#[test]
fn print_renders_aggregates_slices_and_ranges() {
    let stdout = batch(
        &["build/test-programs/records-c-gcc-o0"],
        &[
            "break inspect_records",
            "run",
            "print global_record",
            "print *bits",
            "print *records",
            "print huge_array[1048576]",
            "print huge_array[3..7]",
            "print (*records)[1].values[1]",
        ],
    );
    for expected in [
        "global_record = {inner = {signed_value = -7, unsigned_value = 9}, values = [20, 22]}",
        "*bits = {negative = -3, first = 5, second = 42}",
        "values = [43, 44]",
        "(unsigned char) huge_array[1048576] = 0",
        "huge_array[3..7] = [0, 0, 0, 0]",
        "(int32_t) (*records)[1].values[1] = 44",
    ] {
        assert!(stdout.contains(expected), "{expected:?} in:\n{stdout}");
    }

    let stdout = batch(
        &["build/test-programs/variables-rust-o0"],
        &["break variables.rs:67", "run", "print slice"],
    );
    assert!(stdout.contains("(&[i32]) slice = [20, 22]"), "{stdout}");
}

#[test]
fn print_renders_symbolic_enums_variants_and_raw_unions() {
    let stdout = batch(
        &["build/test-programs/enums-rust-o0"],
        &[
            "break inspect_enum",
            "run",
            "print *value",
            "print *fieldless",
            "print *wide",
            "frame 1",
            "print",
        ],
    );

    // A sum type shows as its active variant.
    for expected in [
        "*value = Integer(42)",
        "*fieldless = Negative (-3)",
        "*wide = Huge (1267650600228229401496703205385)",
        "value = Integer(42)",
        "optional = Some(0x",
        "empty = None",
        "done = Ok(())",
        "failed = Err(5)",
    ] {
        assert!(stdout.contains(expected), "{expected:?} in:\n{stdout}");
    }

    let stdout = batch(
        &["build/test-programs/enums-c-gcc-o0"],
        &["break inspect_enums", "run", "print *raw"],
    );
    assert!(
        stdout.contains("*raw = {integer = 42, floating = ")
            && stdout.contains("} <active member unknown>"),
        "{stdout}"
    );
}

#[test]
fn globals_lists_metadata_and_print_accepts_exact_qualification() {
    let stdout = batch(
        &["build/test-programs/globals-c-gcc-o0"],
        &[
            "globals duplicate",
            "break main.c:9",
            "run",
            "print one.c::duplicate",
            "continue",
        ],
    );
    assert!(stdout.contains("duplicate ("), "{stdout}");
    assert!(stdout.contains("duplicate = 201"), "{stdout}");
}

#[test]
fn batch_mode_lists_threads_and_steps_one_instruction() {
    let stdout = batch(
        &[BASIC],
        &["break breakpoint_target", "run", "threads", "stepi"],
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("* ") && line.contains(" stopped")),
        "{stdout}"
    );
    assert!(stdout.contains("stopped after instruction step"));
}

/// Runs `commands` at the workers fixture's checkpoint, where its workers
/// are parked.
fn at_go_checkpoint(commands: &[&str]) -> String {
    let mut all = vec!["break main.reached", "run"];
    all.extend_from_slice(commands);
    batch(&["build/test-programs/workers-go-o0"], &all)
}

#[test]
fn goroutines_are_listed_by_the_code_the_program_wrote() {
    const WORKERS: &str = "tests/fixtures/go/workers/main.go";
    let worker = support::source_line(WORKERS, "for job := range jobs");
    let reached = support::source_line(WORKERS, "func reached(");
    let stdout = at_go_checkpoint(&["goroutines"]);
    let lines = stdout.lines().collect::<Vec<_>>();

    // Main runs on the stopped thread, and is selected through it.
    let main = lines
        .iter()
        .find(|line| line.starts_with("* [1] main.reached at "))
        .unwrap_or_else(|| panic!("{stdout}"));
    assert!(
        main.contains(&format!("workers/main.go:{reached} — running (thread ")),
        "{main}"
    );

    // A parked worker is where it waits, not in the runtime parking it.
    let workers = lines
        .iter()
        .filter(|line| line.contains("main.worker at "))
        .collect::<Vec<_>>();
    assert_eq!(workers.len(), 4, "{stdout}");
    for line in workers {
        assert!(line.starts_with("  ["), "{line}");
        assert!(
            line.ends_with(&format!("workers/main.go:{worker} — chan receive")),
            "{line}"
        );
    }

    assert!(!stdout.contains("in runtime.gopark"), "{stdout}");
    assert!(
        lines
            .last()
            .is_some_and(|line| line.contains("`goroutines -a` lists them")),
        "{stdout}"
    );

    let all = at_go_checkpoint(&["tasks -a"]);
    let listed = |output: &str| {
        output
            .lines()
            .filter(|line| line.starts_with("  [") || line.starts_with("* ["))
            .count()
    };

    assert!(listed(&all) > listed(&stdout), "{all}");
    assert!(!all.contains("lists them"), "{all}");

    let grouped = at_go_checkpoint(&["goroutines -g"]);
    assert!(
        grouped
            .lines()
            .any(|line| line.starts_with("4 goroutines in main.worker at ")
                && line.contains(&format!("workers/main.go:{worker}: "))),
        "{grouped}"
    );
}

/// `step goroutine` follows the line's `go` statement into the goroutine
/// it starts, which is then selected.
#[test]
fn step_goroutine_enters_the_goroutine_the_line_starts() {
    const STEPS: &str = "tests/fixtures/go/steps/main.go";
    let go = support::source_line(STEPS, "// STEP: go");
    let spawned = support::source_line(STEPS, "// STEP: spawned");
    let stdout = batch(
        &["build/test-programs/steps-go-o0"],
        &[
            &format!("break main.go:{go}"),
            "run",
            // The line's breakpoint is in the goroutine's wrapper too.
            "delete 1",
            "step goroutine",
            "goroutine",
        ],
    );
    assert_in_order(
        &stdout,
        &[&format!(
            "stopped after new task step in main.spawned at {STEPS}:{spawned} "
        )],
    );
    let selected = stdout
        .lines()
        .find(|line| line.starts_with("* [") && line.contains("main.spawned at "))
        .unwrap_or_else(|| panic!("{stdout}"));
    assert!(!selected.starts_with("* [1] "), "{selected}");

    let failure = batch_output(
        &["build/test-programs/steps-go-o0"],
        &[&format!("break main.go:{go}"), "run", "step sideways"],
    );
    assert_failure(&failure, "no call on this line calls sideways");
}

/// A program stripped of its debug information has goroutines nobody can
/// read, which `goroutines` says rather than listing none.
#[test]
fn a_stripped_programs_goroutines_are_unavailable() {
    let failure = batch_output(
        &["build/test-programs/callers-go-stripped"],
        &["break main.reached", "run", "goroutines"],
    );
    assert_failure(
        &failure,
        "goroutines could not be read: the program has no debug information describing Go's runtime",
    );
}

/// A `-trimpath` build records its sources without the directory it was
/// built in, which the missing source's reason says, with the way to map
/// them; `--source-map` then finds them.
#[test]
fn a_trimpath_programs_sources_say_why_they_are_missing() {
    const SERVER: &str = "tests/fixtures/go/server/main.go";
    let stdout = batch(
        &["build/test-programs/server-go-trimpath"],
        &["break main.greet", "run"],
    );

    // The import path the build recorded depends on where the checkout
    // that built it is, so it is read from the breakpoint's location.
    let greet = support::source_line(SERVER, "func greet(");
    let recorded = stdout
        .split_whitespace()
        .find_map(|word| word.strip_suffix(&format!(":{greet}")))
        .filter(|path| path.ends_with(SERVER))
        .unwrap_or_else(|| panic!("no recorded path in {stdout}"));
    let import = recorded
        .strip_prefix("./")
        .and_then(|path| path.strip_suffix(SERVER))
        .and_then(|path| path.strip_suffix('/'))
        .unwrap_or_else(|| panic!("{recorded} is no import path"));
    assert!(
        stdout.contains(&format!(
            "source unavailable: source file {recorded} does not exist; it was recorded \
             without the directory the program was built in, as `-trimpath` builds record \
             paths, so it was looked for in the current directory; a source map can say where \
             it is"
        )),
        "{stdout}"
    );

    let root = env!("CARGO_MANIFEST_DIR");
    let mapped = batch(
        &[
            "--source-map",
            import,
            root,
            "build/test-programs/server-go-trimpath",
        ],
        &["break main.greet", "run"],
    );
    assert!(mapped.contains(&format!("=> {greet} |")), "{mapped}");
}

#[test]
fn a_goroutine_is_selected_or_inspected_by_its_id() {
    let stdout = at_go_checkpoint(&[
        "goroutines -t",
        "goroutine 1 backtrace",
        "threads",
        "goroutine 1",
        "goroutine",
    ]);

    // Each goroutine's frames follow it, the runtime's own folded but
    // where it parked.
    assert_in_order(
        &stdout,
        &[
            "main.worker at ",
            "\n    #0 ",
            "in runtime.gopark",
            "\n        … #1–#2: 2 frames of the runtime",
            "\n    #3 ",
            "in main.worker at ",
            "unwind stopped",
        ],
    );

    // A command runs in the goroutine without keeping it selected.
    assert_in_order(
        &stdout,
        &[
            "#0 ",
            "in main.reached",
            "in main.checkpoint",
            "in runtime.main",
        ],
    );
    let threads = stdout
        .lines()
        .filter(|line| line.contains(" stopped"))
        .collect::<Vec<_>>();
    assert!(
        threads
            .iter()
            .any(|line| line.starts_with("* ") && line.ends_with(" — [1]")),
        "{stdout}"
    );
    assert!(
        threads.iter().any(|line| line.ends_with(" — idle")),
        "{stdout}"
    );
    assert_in_order(&stdout, &["selected goroutine 1", "* [1] main.reached at "]);

    let failure = batch_output(
        &["build/test-programs/workers-go-o0"],
        &["break main.reached", "run", "goroutine 1 next"],
    );
    assert_failure(&failure, "goroutine 1 runs only commands that inspect");
}

#[test]
fn a_cores_goroutines_show_their_labels_and_the_runtimes_their_entries() {
    let stdout = batch(
        &[
            "--core",
            "build/test-programs/panic-go-o0.core",
            "build/test-programs/panic-go-o0",
        ],
        &["goroutines -a"],
    );
    assert!(
        stdout.lines().any(|line| line.contains("] main.worker at ")
            && line.ends_with(r#"— chan receive {job: resize, tenant: "a b"}"#)),
        "{stdout}"
    );

    // The runtime numbers its goroutines in batches, so its own ids vary.
    assert!(
        stdout.lines().any(|line| line.starts_with("  [")
            && line.ends_with("] runtime.forcegchelper — force gc (idle)")),
        "{stdout}"
    );
}

#[test]
fn backtraces_say_whose_stack_each_run_of_frames_is_on() {
    let stdout = batch(
        &["build/test-programs/stacks-go-o0"],
        &["break runtime.readmemstats_m", "run", "backtrace"],
    );
    assert_in_order(
        &stdout,
        &[
            "\n    on the runtime's stack:\n#0 ",
            "in runtime.readmemstats_m",
            "in runtime.systemstack",
            "\n    on the task's stack:\n#3 ",
            "in runtime.ReadMemStats",
            "in main.stats",
            "unwind stopped: the outermost frame has no caller",
        ],
    );
}

#[test]
fn finish_shows_what_the_function_returned() {
    for fixture in [
        "build/test-programs/values-go-o0",
        "build/test-programs/values-go-o2",
    ] {
        let stdout = batch(
            &[fixture],
            &["break main.returning", "run", "finish", "print"],
        );
        assert_in_order(
            &stdout,
            &[
                "stopped after frame return",
                "returned (int) count = 42",
                "returned (bool) ok = true",
                "returned (string) text = \"go\"",
                "returned (error) failure = nil",
                // The frame returned to lists them among its variables.
                "returned (int) count = 42",
            ],
        );
    }
}

/// C and C++ return by the System V convention: in registers, in st0, or
/// in memory whose address is returned. A small C++ class GCC does not say
/// how calls pass is unknown, for that reason.
#[test]
fn finish_shows_what_a_c_or_cpp_function_returned() {
    let stdout = batch(
        &["build/test-programs/returns-c-gcc-o2"],
        &[
            "break r_double",
            "break r_long_double",
            "break r_big",
            "run",
            "finish",
            "continue",
            "finish",
            "continue",
            "finish",
            "print",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "returned (double) r_double = -0.375",
            "returned (long double) r_long_double = 2.5",
            "returned (big) r_big = {<3 fields>}",
            "(int) n = 1",
            "returned (big) r_big = {<3 fields>}",
        ],
    );

    let stdout = batch(
        &["build/test-programs/returns-cpp-gcc-o2"],
        &["break r_plain", "run", "finish"],
    );
    assert!(
        stdout.contains(
            "returned (Plain) r_plain = <unavailable: unsupported variable feature: \
             returning this type by the function's calling convention>"
        ),
        "{stdout}"
    );
}

#[test]
fn a_watched_goroutine_local_names_its_goroutine_and_follows_its_stack() {
    let stdout = batch(
        &["build/test-programs/watched-go-o0"],
        &[
            "break main.bump",
            "run",
            "up",
            "watch counter",
            "delete 1",
            "continue",
            "continue",
            "continue",
            "continue",
            "info watchpoints",
            "continue",
            "continue",
        ],
    );

    // The address the watch is at, from its line.
    let watched = |prefix: &str| {
        let line = stdout
            .lines()
            .find(|line| line.starts_with(prefix) && line.contains("counter"))
            .unwrap_or_else(|| panic!("no {prefix:?}:\n{stdout}"));
        assert!(line.contains(" below the top of task "), "{line}");
        let (_, after) = line.split_once("8 bytes at ").expect("a size");
        after
            .split_whitespace()
            .next()
            .expect("an address")
            .to_owned()
    };

    let set = watched("watchpoint 1 set on counter: ");
    let listed = watched("1 ");

    // Each round's deep calls grew the stack, which the runtime moved.
    assert_ne!(set, listed, "{stdout}");
    assert_in_order(
        &stdout,
        &[
            "new: 10",
            "deleted watchpoint 1 counter: its frame or block is no longer active",
            "inferior exited with status 0",
        ],
    );
}

#[test]
fn backtraces_mark_the_iterators_of_a_loop_whose_body_runs() {
    let stdout = batch(
        &["build/test-programs/ranges-go-o0"],
        &["break main.counted-range1", "run", "backtrace", "frame 1"],
    );
    assert_in_order(
        &stdout,
        &[
            "in main.counted-range1 at",
            "#1 ",
            "in main.Count.func1 (the iterator of #2's loop) at",
            "#2 ",
            "in main.counted at",
            "#1 ",
            "in main.Count.func1 (the iterator of #2's loop) at",
        ],
    );
}

#[test]
fn stops_and_list_show_source_from_any_working_directory() {
    let output = microscope_command()
        .current_dir("/")
        .args([
            "--batch",
            "--eval",
            "break breakpoint_target",
            "--eval",
            "run",
            "--eval",
            "l",
        ])
        .arg(fixture(BASIC))
        .output()
        .expect("run microscope");
    let stdout = assert_success(output);
    let line = format!("{}:6", fixture("tests/fixtures/c/basic.c").display());

    // The breakpoint, the stop, and the listing; a root of `/` holds every
    // path, so they stay absolute.
    assert_eq!(stdout.matches(&line).count(), 3, "{stdout}");
    assert_eq!(
        stdout
            .matches("=> 6 |     return microscope_value;")
            .count(),
        2,
        "{stdout}"
    );
    assert!(stdout.contains("   8 |"), "{stdout}");
}

#[test]
fn source_maps_read_sources_recorded_under_another_directory() {
    let commands = ["break breakpoint_target", "run", "list"];
    let unmapped = batch(&["build/test-programs/basic-relocated"], &commands[..2]);
    assert!(
        unmapped.contains(
            "source unavailable: source file /nonexistent/microscope/tests/fixtures/c/basic.c does not exist"
        ),
        "{unmapped}"
    );

    let repository = env!("CARGO_MANIFEST_DIR");
    let stdout = batch(
        &[
            "--source-map",
            "/nonexistent/microscope",
            repository,
            "build/test-programs/basic-relocated",
        ],
        &commands,
    );

    // The mapped file is the repository's, shown relative to it.
    assert!(
        stdout.contains("\ntests/fixtures/c/basic.c:6\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("=> 6 |     return microscope_value;"),
        "{stdout}"
    );
}

#[test]
fn ctrl_c_pauses_a_running_inferior_before_accepting_more_commands() {
    let mut microscope =
        Microscope::spawn(microscope_command().arg(fixture("build/test-programs/spin")));
    microscope.send("break main\nrun\nthreads\n");
    microscope.line("the breakpoint stop", |line| {
        line.starts_with("stopped at breakpoint 1")
    });
    let thread = microscope.line("the stopped thread", |line| line.starts_with("* "));
    let inferior: u32 = thread
        .split_whitespace()
        .nth(1)
        .and_then(|id| id.parse().ok())
        .unwrap_or_else(|| panic!("no thread ID in {thread:?}"));

    // main's loop is a jump to itself, so the breakpoint must go before the
    // inferior can run it. Only the continue can make the stopped inferior
    // runnable again, so once it is, Ctrl-C interrupts the loop.
    microscope.send("delete 1\ncontinue\n");
    support::wait_until("the inferior runs main's loop", || {
        process_state(inferior) == Some('R')
    });
    kill(
        Pid::from_raw(i32::try_from(microscope.id()).expect("debugger PID fits i32")),
        Signal::SIGINT,
    )
    .expect("pause microscope");
    microscope.line("the pause", |line| {
        line.starts_with("inferior paused in main at ")
    });

    // End of input shuts the session down, killing and reaping the inferior.
    microscope.send("where\n");
    microscope.close_stdin();
    let stdout = assert_success(microscope.finish());
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("main at ") && line.contains("spin.c:10 ")),
        "the pause did not land in main's loop:\n{stdout}"
    );
    assert_eq!(
        process_state(inferior),
        None,
        "inferior {inferior} survived debugger shutdown"
    );
}

#[test]
fn continuing_past_a_sigint_stop_discards_the_interrupt() {
    // A terminal Ctrl-C signals the inferior as well as microscope. Like gdb,
    // the CLI must not deliver that SIGINT, which would terminate the inferior.
    let stdout = batch(&["build/test-programs/interrupt"], &["run", "continue"]);
    assert!(stdout.contains("stopped by SIGINT"), "{stdout}");
    assert!(stdout.contains("inferior exited with status 0"), "{stdout}");
}

#[test]
fn core_dumps_are_inspected_in_batch_mode_without_executing() {
    let output = batch_output(
        &["--core", SEGV_CORE],
        &[
            "where",
            "bt",
            "threads",
            "print depth",
            "print crash_library_tls",
            "info core",
        ],
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = assert_success(output);
    assert!(stdout.starts_with("crash_segv at "), "{stdout}");
    assert!(stdout.contains("#1 "), "{stdout}");
    assert!(stdout.contains("in main at "), "{stdout}");
    assert!(
        stdout.contains("unwind stopped: the outermost frame has no caller"),
        "{stdout}"
    );
    assert_eq!(
        stdout
            .matches("process terminated by SIGSEGV (SEGV_MAPERR) at 0x0 (0xb)")
            .count(),
        1,
        "only the faulting thread carries the reason: {stdout}"
    );
    assert!(stdout.contains("(int32_t) depth = 3"), "{stdout}");
    assert!(stdout.contains("crash_library_tls = 654"), "{stdout}");
    assert!(
        stdout.contains("signal: SIGSEGV (SEGV_MAPERR) at 0x0"),
        "{stdout}"
    );
    assert!(
        stdout.contains("crash-gcc-o0 module 0 verified by build-id"),
        "{stdout}"
    );
    assert_no_sgr(&stdout);

    let commands = [
        "run",
        "continue",
        "step",
        "next",
        "finish",
        "stepi",
        "break main",
    ];
    let (_, stderr) = piped(&["--core", SEGV_CORE], &commands);
    assert_eq!(
        stderr
            .matches("a post-mortem core dump cannot execute, be modified, or hold breakpoints")
            .count(),
        commands.len(),
        "{stderr}"
    );
}

#[test]
fn core_banner_reports_the_signal_and_every_unproven_module() {
    let stdout = assert_success(microscope(&["--core", SEGV_CORE]));
    let mut lines = stdout.lines();
    let banner = lines.next().unwrap_or_default();
    assert!(
        banner.starts_with("opened core dump ") && banner.contains("of crash-gcc-o0 (process "),
        "{stdout}"
    );
    assert_eq!(
        lines.next(),
        Some("process terminated by SIGSEGV (SEGV_MAPERR) at 0x0 (0xb)")
    );
    assert!(!stdout.contains("warning"), "{stdout}");

    // Warnings go to stderr, keeping stdout clean.
    let missing = microscope(&[
        "--core",
        "build/test-programs/core-missing-library/crash.core",
    ]);
    let stderr = String::from_utf8_lossy(&missing.stderr).into_owned();
    assert!(
        stderr.contains("core-missing-library/libcrash.so is missing; its frames and unsaved memory are unavailable"),
        "{stderr}"
    );
    assert!(!assert_success(missing).contains("warning"));
    let batch = microscope(&[
        "--core",
        "build/test-programs/core-missing-library/crash.core",
        "--batch",
    ]);
    assert!(String::from_utf8_lossy(&batch.stderr).contains("libcrash.so is missing"));
    assert_eq!(assert_success(batch), "");
}

#[test]
fn allowed_module_mismatches_are_named_and_reported() {
    let allowed = batch_output(
        &[
            "--core",
            SEGV_CORE,
            "build/test-programs/crash-gcc-o0-rebuilt",
            "--allow-module-mismatch",
        ],
        &["info core"],
    );
    let stderr = String::from_utf8_lossy(&allowed.stderr).into_owned();
    assert!(
        stderr.contains("crash-gcc-o0-rebuilt does not match the dump (the build-id note differs); using its metadata anyway"),
        "{stderr}"
    );

    // The line names the file used when it is not at the recorded path.
    let stdout = assert_success(allowed);
    let rebuilt = fs::canonicalize(fixture("build/test-programs/crash-gcc-o0-rebuilt"))
        .expect("canonical fixture");
    assert!(
        stdout.contains(&format!(
            "crash-gcc-o0 module 0 from {} mismatched: the build-id note differs",
            rebuilt.display()
        )),
        "{stdout}"
    );
}

#[test]
fn core_module_searches_name_the_files_they_use_and_those_still_missing() {
    // Without the C library, its recorded build-id says what to supply.
    let directory = support::ScratchDir::new("cli-module-path");
    for name in ["crash-gcc-o0", "libcrash.so"] {
        fs::copy(
            fixture(&format!("build/test-programs/{name}")),
            directory.path().join(name),
        )
        .expect("copy module");
    }

    let module_path = directory.path().to_str().expect("UTF-8 scratch path");
    let partial = batch_output(
        &[
            "--core",
            "build/test-programs/core-foreign/crash.core",
            "--module-path",
            module_path,
        ],
        &["info core"],
    );
    let stderr = String::from_utf8_lossy(&partial.stderr).into_owned();
    let stdout = assert_success(partial);

    let libc = object::File::parse(
        fs::read(fixture("build/test-programs/libc-foreign.so.6"))
            .expect("read foreign C library")
            .as_slice(),
    )
    .expect("parse foreign C library")
    .build_id()
    .expect("read build-id")
    .expect("build-id note")
    .iter()
    .fold(String::new(), |mut text, byte| {
        write!(text, "{byte:02x}").expect("writing to a String cannot fail");
        text
    });
    assert!(
        stderr.contains(&format!(
            "core-foreign/libc.so.6 is missing; its frames and unsaved memory are unavailable (build-id {libc})"
        )),
        "{stderr}"
    );
    assert!(
        stdout.contains(&format!("core-foreign/libc.so.6 missing (build-id {libc})")),
        "{stdout}"
    );
    let canonical = directory
        .path()
        .canonicalize()
        .expect("canonical scratch path");
    for name in ["crash-gcc-o0", "libcrash.so"] {
        let line = stdout
            .lines()
            .find(|line| line.contains(&format!("core-foreign/{name} module")))
            .unwrap_or_else(|| panic!("no {name} line:\n{stdout}"));
        assert!(
            line.ends_with(&format!(
                " from {} verified by build-id",
                canonical.join(name).display()
            )),
            "{line}"
        );
    }
}

#[test]
fn backtraces_and_where_name_code_without_debug_info_by_symbol_and_module() {
    let stdout = batch(
        &["--core", "build/test-programs/elf-symbols-stripped.core"],
        &["bt", "where"],
    );
    let lines = stdout.lines().collect::<Vec<_>>();
    let frame = |needle: &str| {
        lines
            .iter()
            .find(|line| line.starts_with('#') && line.contains(needle))
            .unwrap_or_else(|| panic!("no frame containing {needle:?}:\n{stdout}"))
    };

    // An unsized symbol says so; code no symbol names stays unknown, even
    // where stripping removed a static function between named ones.
    assert!(
        frame(" in asm_unsized+0x6 ")
            .ends_with(" (unsized symbol) from libelf-symbols-stripped.so"),
        "{stdout}"
    );
    assert!(frame(" in asm_sized+0x6 ").ends_with(" from libelf-symbols-stripped.so"));
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.ends_with(" in <unknown> from libelf-symbols-stripped.so"))
            .count(),
        2,
        "{stdout}"
    );
    assert!(!stdout.contains("lib_static_helper"), "{stdout}");

    // Frames with source keep showing it instead of a module.
    assert!(frame(" in chain_gap at ").ends_with("tests/fixtures/c/elf-symbols/main.c:69"));
    assert!(frame(" in _start+0x").ends_with(" from elf-symbols-stripped"));
    let location = lines.last().expect("where output");
    assert!(
        location.starts_with("lib_fault at 0x")
            && location.ends_with(" from libelf-symbols-stripped.so"),
        "{stdout}"
    );
}

#[test]
fn frame_commands_move_through_caller_frames_and_show_their_source() {
    let stdout = batch(
        &["--core", FRAMES_CORE],
        &[
            "frame 5",
            "print depth",
            "up",
            "print depth",
            "where",
            "down 2",
            "list",
            "frame",
            "up 100",
            "down",
        ],
    );
    let lines = stdout.lines().collect::<Vec<_>>();
    let recursive_call = format!(
        "frames.c:{}",
        frames_line("int64_t below = frames_recurse(depth - 1, seed);")
    );
    let innermost_call = frames_line("return frames_keep(seed) + level;");

    let frame = line_index(&lines, 0, |line| {
        line.starts_with("#5 ")
            && line.contains(" in frames_recurse at ")
            && line.ends_with(&recursive_call)
    });

    // The frame's source follows it, marking its call.
    let marked = line_index(&lines, frame, |line| line.starts_with("=> "));
    assert!(lines[marked].contains("frames_recurse(depth - 1, seed)"));
    let depth = line_index(&lines, marked, |line| line == "(int64_t) depth = 1");
    let up = line_index(&lines, depth, |line| line.starts_with("#6 "));
    let depth = line_index(&lines, up, |line| line == "(int64_t) depth = 2");
    let location = line_index(&lines, depth, |line| {
        line.starts_with("frames_recurse at ") && line.contains(&format!("{recursive_call} (0x"))
    });
    let down = line_index(&lines, location, |line| {
        line.starts_with("#4 ") && line.ends_with(&format!("frames.c:{innermost_call}"))
    });
    let listed = line_index(&lines, down + 1, |line| {
        line.starts_with(&format!("=> {innermost_call} |"))
    });
    let shown = line_index(&lines, listed, |line| line.starts_with("#4 "));
    let outermost = line_index(&lines, shown, |line| {
        line.starts_with("#11 ") && line.contains(" in _start+0x")
    });
    line_index(&lines, outermost, |line| line.starts_with("#10 "));
}

/// An uncaught Python exception, chosen alone before the program runs,
/// installs the agent with no breakpoint, and stops as the exception leaves
/// the main thread's code: the stop says what it is and where it was
/// raised, and the program then ends as Python ends it. The interpreter's
/// own lack of debug information is no warning: the program is the script.
#[test]
fn an_uncaught_python_exception_stops_where_it_leaves_the_program() {
    let interpreter = std::path::PathBuf::from(
        std::env::var_os("MICROSCOPE_CPYTHON")
            .expect("MICROSCOPE_CPYTHON names the interpreters; run tests in the dev shell"),
    )
    .join("3.14/python");
    let interpreter = interpreter.to_str().expect("a UTF-8 path");
    let source = "def boom():\n    raise ValueError('boom')\n\nboom()\n";
    let program = [interpreter, "--", "-I", "-B", "-c", source];
    let (stdout, stderr) = piped(
        &program,
        &["catch python-uncaught on", "run", "bt", "continue"],
    );
    let lines = stdout.lines().collect::<Vec<_>>();
    let stopped = line_index(&lines, 0, |line| {
        line == "stopped by an unhandled exception in <module> at <string>:4:"
    });
    let message = line_index(&lines, stopped, |line| {
        line == "ValueError: boom, raised in boom at <string>:2"
    });
    assert_eq!(message, stopped + 1, "{stdout}");
    line_index(&lines, message, |line| {
        line == "source unavailable: the code was compiled from <string>, which names no file"
    });
    line_index(&lines, message, |line| {
        line.starts_with('#') && line.ends_with("in <module> at <string>:4")
    });
    line_index(&lines, message, |line| {
        line == "inferior exited with status 1"
    });
    assert!(stderr.contains("ValueError: boom"), "{stderr}");
    assert!(!stderr.contains("no debug information"), "{stderr}");
}

/// Python breakpoints set before the program runs are kept pending, as its
/// code loads only when it imports it, and stop in the program's own frame,
/// with the debugger's agent and the interpreter folded, from which `next`
/// steps the program's line; one in code the program never runs never
/// stops. A native breakpoint is still checked.
#[test]
fn python_breakpoints_wait_for_their_code_and_stop_in_the_programs_frame() {
    let interpreter = std::path::PathBuf::from(
        std::env::var_os("MICROSCOPE_CPYTHON")
            .expect("MICROSCOPE_CPYTHON names the interpreters; run tests in the dev shell"),
    )
    .join("3.14/python");
    let interpreter = interpreter.to_str().expect("a UTF-8 path");
    let program = [
        interpreter,
        "--",
        "-I",
        "-B",
        "tests/fixtures/python/breakpoints.py",
    ];
    let (stdout, stderr) = piped(
        &program,
        &[
            "break tests/fixtures/python/breakpoints.py:15 if index == 3",
            "break py:Shape.area",
            "break breakpoints_later.py:5",
            "break py:rea",
            "break missing.c:3",
            "break accumulate",
            "break mian",
            "run",
            "bt",
            "print total",
            "next",
            "print square",
            "continue",
            "continue",
            "info breakpoints",
            "continue",
        ],
    );
    let lines = stdout.lines().collect::<Vec<_>>();
    let mut at = 0;
    for number in 1..=4 {
        at = line_index(&lines, at, |line| {
            line.starts_with(&format!("breakpoint {number} set, pending until"))
        });
    }

    assert!(
        stderr.contains("no source file matching 'missing.c'"),
        "{stderr}"
    );
    // A bare name is a native function's; the refusal says how to name the
    // Python one.
    assert!(
        stderr.contains("no function named 'accumulate' was found")
            && stderr.contains("`py:accumulate`"),
        "{stderr}"
    );
    // A native name misspelled is still found near its own.
    assert!(
        stderr.contains(
            "did you mean 'main'? A Python function is named with `py:`, as in `py:mian`"
        ),
        "{stderr}"
    );

    let stopped = line_index(&lines, at, |line| {
        line == "stopped at breakpoint 1 (hit 4) in accumulate at tests/fixtures/python/breakpoints.py:15"
    });
    let folded = line_index(&lines, stopped, |line| {
        line.starts_with("    … #0–#")
            && line.ends_with("frames of the runtime; `bt -r` shows them")
    });
    let frame = line_index(&lines, folded, |line| {
        line.starts_with('#')
            && line.ends_with("in accumulate at tests/fixtures/python/breakpoints.py:15")
    });
    assert_eq!(
        frame,
        folded + 1,
        "only the folded hook is inner:
{stdout}"
    );
    let total = line_index(&lines, frame, |line| line == "(int) total = 3");
    let next = line_index(&lines, total, |line| {
        line == "stopped after source next in accumulate at tests/fixtures/python/breakpoints.py:16"
    });
    let square = line_index(&lines, next, |line| line == "(int) square = 4");
    let method = line_index(&lines, square, |line| {
        line.starts_with("stopped at breakpoint 2 (hit 1) in Shape.area at ")
    });
    let later = line_index(&lines, method, |line| {
        line == "stopped at breakpoint 3 (hit 1) in later at tests/fixtures/python/breakpoints_later.py:5"
    });
    let never = line_index(&lines, later, |line| {
        line.split_whitespace().collect::<Vec<_>>() == ["4", "+", "0", "function", "rea"]
    });
    let done = line_index(&lines, never, |line| line == "BREAKPOINTS DONE");
    line_index(&lines, done, |line| line == "inferior exited with status 0");
    assert!(!stdout.contains("microscope_python_hook"), "{stdout}");
}

/// A program named without a directory is found as a shell finds it, on
/// `PATH`, when the current directory has no file of that name.
#[test]
fn a_program_named_alone_is_found_on_path() {
    let interpreters = std::path::PathBuf::from(
        std::env::var_os("MICROSCOPE_CPYTHON")
            .expect("MICROSCOPE_CPYTHON names the interpreters; run tests in the dev shell"),
    )
    .join("3.14");
    let path = std::env::join_paths(std::iter::once(interpreters).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .expect("a PATH");
    let found = microscope_command()
        .env("PATH", &path)
        .args([
            "--batch",
            "--eval",
            "run",
            "python",
            "--",
            "-c",
            "print('found on PATH')",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::null())
        .output()
        .expect("run microscope");
    let stdout = assert_success(found);
    assert!(stdout.contains("found on PATH"), "{stdout}");

    // A directory of that name is no program, and leaves PATH to find it.
    let directory = support::ScratchDir::new("cli-path-directory");
    std::fs::create_dir(directory.path().join("python")).expect("make a directory");
    let shadowed = microscope_command()
        .env("PATH", &path)
        .args([
            "--batch",
            "--eval",
            "run",
            "python",
            "--",
            "-c",
            "print('found on PATH')",
        ])
        .current_dir(directory.path())
        .stdin(Stdio::null())
        .output()
        .expect("run microscope");
    let stdout = assert_success(shadowed);
    assert!(stdout.contains("found on PATH"), "{stdout}");

    let missing = microscope_command()
        .env("PATH", &path)
        .args(["--batch", "--eval", "run", "no-such-program-anywhere"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::null())
        .output()
        .expect("run microscope");
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(!missing.status.success(), "{stderr}");
    assert!(
        stderr.contains("no-such-program-anywhere")
            && stderr.contains("current directory")
            && stderr.contains("PATH"),
        "{stderr}"
    );
}

/// A program run through a link runs by the link's name: a multi-call
/// binary, as coreutils on NixOS and busybox are, chooses what to be from
/// its `argv[0]`, so `echo` reached through a link to it must stay `echo`.
#[test]
fn a_link_to_a_multi_call_binary_runs_as_the_link_names() {
    let echo = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("echo"))
        .find(|candidate| candidate.is_file())
        .expect("echo on PATH");
    let binary = echo.canonicalize().expect("echo's file");
    let directory = support::ScratchDir::new("cli-multi-call");
    let link = directory.path().join("echo");
    std::os::unix::fs::symlink(&binary, &link).expect("link to echo's file");

    let output = microscope_command()
        .args(["--batch", "--eval", "run"])
        .arg(&link)
        .args(["--", "ran-as-echo"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::null())
        .output()
        .expect("run microscope");
    let stdout = assert_success(output);
    assert!(
        stdout.contains("ran-as-echo"),
        "{} {stdout}",
        binary.display()
    );
}

/// A virtual environment's interpreter, a link to the one it was made from,
/// runs as the environment's: Python finds its environment from the path it
/// is run as, which an activated environment puts first on `PATH`.
#[test]
fn a_virtual_environments_python_runs_in_its_environment() {
    let directory = support::ScratchDir::new("cli-venv");
    let environment = directory.path().join("venv");
    let interpreter = std::path::PathBuf::from(
        std::env::var_os("MICROSCOPE_CPYTHON")
            .expect("MICROSCOPE_CPYTHON names the interpreters; run tests in the dev shell"),
    )
    .join("3.14/python");
    let made = Command::new(&interpreter)
        .args(["-I", "-m", "venv", "--without-pip"])
        .arg(&environment)
        .output()
        .expect("make a virtual environment");
    assert!(made.status.success(), "{made:?}");
    let path = std::env::join_paths(std::iter::once(environment.join("bin")).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .expect("a PATH");

    let output = microscope_command()
        .env("PATH", &path)
        .args([
            "--batch",
            "--eval",
            "run",
            "python",
            "--",
            "-c",
            "import sys; print('prefix', sys.prefix)",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::null())
        .output()
        .expect("run microscope");
    let stdout = assert_success(output);
    assert!(
        stdout.contains(&format!("prefix {}", environment.display())),
        "{stdout}"
    );
}

/// A Python program's core, whose Python frames call each other through
/// the interpreter's frames on some of its threads.
const PYTHON_CORE: &str = "build/test-programs/python-locals-3.14t.core";

#[test]
fn python_frames_show_their_source_and_fold_the_interpreters_frames() {
    let log = fs::read_to_string(format!("{PYTHON_CORE}.log"))
        .expect("the core's log; run `just build-test-programs`");
    let report = support::truth::Report::parse(&log);
    let parked = report.checkpoint("parked").expect("the core's checkpoint");
    let frame = parked
        .iter()
        .find(|fields| fields[0] == "frame" && fields[3] == "containers")
        .expect("the frame of containers");
    let (thread, line) = (&frame[1], &frame[5]);
    let place = format!("in containers at tests/fixtures/python/locals.py:{line}");

    let thread = format!("thread {thread}");
    let stdout = batch(&["--core", PYTHON_CORE], &[&thread, "bt"]);
    let lines = stdout.lines().collect::<Vec<_>>();
    let shown = line_index(&lines, 0, |shown| shown.ends_with(&place));
    let level = lines[shown]
        .split_whitespace()
        .next()
        .and_then(|level| level.strip_prefix('#'))
        .expect("the frame's level");
    line_index(&lines, 0, |line| {
        line.contains("frames of the runtime; `bt -r` shows them")
    });
    assert!(
        !stdout.contains("_PyEval_EvalFrameDefault"),
        "the interpreter's frames fold:\n{stdout}"
    );

    // Moving between frames passes over the frames a backtrace folds,
    // until they are shown.
    let frame = format!("frame {level}");
    let stdout = batch(
        &["--core", PYTHON_CORE],
        &[
            &thread,
            &frame,
            "where",
            "up",
            "up",
            "down",
            "set runtime-frames on",
            "up",
        ],
    );
    let lines = stdout.lines().collect::<Vec<_>>();
    let selected = line_index(&lines, 0, |shown| {
        shown.starts_with(&format!("#{level} ")) && shown.ends_with(&place)
    });
    let marked = line_index(&lines, selected, |shown| {
        shown.starts_with(&format!("=> {line} |"))
    });
    assert!(lines[marked].ends_with("gate.arrive()"), "{stdout}");
    let location = line_index(&lines, marked, |shown| {
        format!("in {shown}").ends_with(&place)
    });
    let run = line_index(&lines, location, |shown| {
        shown.contains(" in Thread.run at ")
    });
    let bootstrap = line_index(&lines, run, |shown| {
        shown.starts_with('#') && shown.contains(" in Thread._bootstrap_inner at ")
    });
    let back = line_index(&lines, bootstrap, |shown| {
        shown.starts_with('#') && shown.contains(" in Thread.run at ")
    });
    line_index(&lines, back, |shown| {
        shown.starts_with('#') && shown.contains(" in _PyEval_EvalFrameDefault+")
    });

    // Expressions read the frame's objects as Python finds them.
    let stdout = batch(
        &["--core", PYTHON_CORE],
        &[
            &thread,
            &frame,
            "print numbers[-1]",
            "print by_name[\"b\"]",
            "print numbers[0] + len(by_name) * 10",
            "whatis numbers",
        ],
    );
    let lines = stdout.lines().collect::<Vec<_>>();
    let last = line_index(&lines, 0, |shown| shown == "(int) numbers[-1] = 3");
    let found = line_index(&lines, last, |shown| {
        shown == "(str) by_name[\"b\"] = 'two'"
    });
    let sum = line_index(&lines, found, |shown| {
        shown == "(integer) numbers[0] + len(by_name) * 10 = 31"
    });
    line_index(&lines, sum, |shown| shown == "type = list");
}

#[test]
fn frame_commands_report_the_ends_of_the_stack() {
    let (_, stderr) = piped(
        &["--core", FRAMES_CORE],
        &["down", "frame 99", "up x", "up 100", "up"],
    );
    assert_in_order(
        &stderr,
        &[
            "the innermost frame is selected",
            "frame 99 does not exist; the backtrace has 12 frames",
            "usage: up [count]",
            "the outermost frame is selected",
        ],
    );
}

#[test]
fn disassembly_and_finish_follow_the_selected_frame() {
    // An outer frame's function is shown around its call, marking where
    // execution returns.
    let stdout = batch(&["--core", FRAMES_CORE], &["frame 3", "disassemble"]);
    let lines = stdout.lines().collect::<Vec<_>>();
    let function = line_index(&lines, 0, |line| {
        line == "function frames_keep in frames-gcc-o0:"
    });
    let marked = line_index(&lines, function, |line| line.starts_with("=> "));
    assert!(
        lines[marked - 1].contains("call") && lines[marked - 1].ends_with("<frames_relay>"),
        "{stdout}"
    );

    let stdout = batch(
        &["build/test-programs/frames-gcc-o0"],
        &[
            &format!(
                "break frames.c:{}",
                frames_line("frames_sink = leaf_local;")
            ),
            "run",
            "frame 5",
            "finish",
            "print depth",
            "backtrace",
        ],
    );
    let lines = stdout.lines().collect::<Vec<_>>();
    let finished = line_index(&lines, 0, |line| line == "(int64_t) depth = 2");
    line_index(&lines, finished, |line| {
        line.starts_with("#0 ") && line.contains(" in frames_recurse at ")
    });

    // Depths 2 and 3 remain below main.
    line_index(&lines, finished, |line| {
        line.starts_with("#2 ") && line.contains(" in main at ")
    });
}

#[test]
fn a_line_breakpoint_inside_an_inline_body_stops_in_the_inline_frame() {
    let stdout = batch(
        &["build/test-programs/variables-inline-gcc-o0"],
        &[
            "break variables-inline.c:6",
            "run",
            "backtrace",
            "up",
            "disassemble",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "#0  ",
            " in inline_target at ",
            "variables-inline.c:6",
            "#1  ",
            " in inline_caller at ",
            "variables-inline.c:12",
            "function inline_caller in variables-inline-gcc-o0:",
        ],
    );
}

#[test]
fn backtraces_demangle_rust_symbols() {
    let stdout = batch(
        &["--core", "build/test-programs/crash-rust-nodebug.core"],
        &["bt"],
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.contains(" in crash::crash_now+0x")
                && line.ends_with(" from crash-rust-nodebug")),
        "{stdout}"
    );
}

#[test]
fn where_reports_instructions_outside_every_module_by_address() {
    let stdout = batch(
        &["--core", "build/test-programs/null-call.core"],
        &["where"],
    );
    assert_eq!(
        stdout.trim_end(),
        "<unknown> at 0x0000000000000000 outside every loaded module"
    );
}

#[test]
fn watch_script_reports_values_lists_and_deletes_watchpoints() {
    let stdout = assert_success(microscope(&[
        "--batch",
        "-c",
        "tests/fixtures/watch.microscope",
        "build/test-programs/watch-gcc-o0",
    ]));
    assert_no_sgr(&stdout);

    let set = stdout
        .lines()
        .find(|line| line.starts_with("watchpoint 1 set on watch_i32: 4 bytes at 0x"))
        .unwrap_or_else(|| panic!("missing set confirmation:\n{stdout}"));
    assert!(set.ends_with("using 1 hardware slot"), "{set}");
    assert!(
        stdout.lines().any(
            |line| line.starts_with("1   +      0  watch_i32  4 bytes at 0x")
                && line.ends_with("  change")
        ),
        "{stdout}"
    );
    assert!(
        stdout.lines().any(
            |line| line.starts_with("2   +      0  watch_u64  8 bytes at 0x")
                && line.ends_with("  write")
        ),
        "{stdout}"
    );
    for (old, new) in [(0, 1), (1, 2), (2, 42)] {
        assert!(
            stdout.contains(&format!("\n  old: {old}\n  new: {new}\n")),
            "missing {old} -> {new}:\n{stdout}"
        );
    }

    // `watch` skips the store of 2 over 2; `watch -w` reports the failed
    // compare-exchange's store of the value already there.
    assert_eq!(
        stdout
            .matches("stopped by watchpoint 1 (change, hit ")
            .count(),
        3
    );
    assert_eq!(
        stdout
            .matches("stopped by watchpoint 2 (write, hit ")
            .count(),
        2
    );
    assert!(
        stdout.contains("\n  value: 4919131752989213764 (unchanged)\n"),
        "{stdout}"
    );
    assert!(stdout.contains("deleted watchpoint 1"));
    assert!(stdout.contains("deleted 1 watchpoint\n"), "{stdout}");
    assert!(
        stdout.contains("watch.c:"),
        "watch stops show source:\n{stdout}"
    );
    assert!(stdout.contains("inferior exited with status 0"), "{stdout}");
}

#[test]
fn access_and_location_watchpoints_render_their_kind_and_slots() {
    let stdout = batch(
        &["build/test-programs/watch-gcc-o0"],
        &[
            "break read_access",
            "run",
            "awatch watch_i32",
            "watch watch_packed.field",
            "info watchpoints",
            "continue",
            "unwatch all",
        ],
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("1   +      0  watch_i32 ")
                && line.ends_with("  read/write")),
        "{stdout}"
    );
    assert!(
        stdout.contains("watchpoint 2 set on watch_packed.field: 4 bytes at 0x")
            && stdout.contains("using 3 hardware slots"),
        "{stdout}"
    );
    assert!(
        stdout.contains("stopped by watchpoint 1 (read/write, hit 1) on watch_i32"),
        "{stdout}"
    );
    assert!(stdout.contains("\n  value: 42 (unchanged)\n"), "{stdout}");
    assert!(stdout.contains("deleted 2 watchpoints"), "{stdout}");
}

#[test]
fn signed_watched_values_render_with_their_sign() {
    let stdout = batch(
        &["build/test-programs/watch-gcc-o0"],
        &[
            "break read_access",
            "run",
            "watch watch_sink",
            "continue",
            "continue",
        ],
    );
    assert!(stdout.contains("\n  new: 42\n"), "{stdout}");
    assert!(stdout.contains("\n  old: 42\n  new: -42\n"), "{stdout}");
}

#[test]
fn watch_commands_explain_rejected_input() {
    let usage = "usage: watch [-w] <expression|0xaddress:byte-count>";
    let (_, stderr) = piped(
        &["build/test-programs/watch-gcc-o0"],
        &[
            "watch",
            "watch -w",
            "break scalar_stores",
            "run",
            "rwatch watch_i32",
            "watch 0x1000:many",
            "watch -x watch_i32",
            "watch watch_array[0..2]",
        ],
    );

    assert_in_order(
        &stderr,
        &[
            usage,
            usage,
            "read watchpoints are unsupported by this target's debug hardware",
            usage,
            usage,
            "a range cannot be watched; watch one element",
        ],
    );
}

#[test]
fn disassemble_renders_the_stopped_function_with_named_targets_and_source_lines() {
    let stdout = batch(
        &["build/test-programs/disassembly-gcc-o0"],
        &["break main", "run", "disassemble"],
    );
    let listing = stdout
        .split_once("function main in disassembly-gcc-o0:\n")
        .unwrap_or_else(|| panic!("no function header in:\n{stdout}"))
        .1;
    let lines = listing.lines().collect::<Vec<_>>();
    let stopped = lines
        .iter()
        .position(|line| line.starts_with("=> "))
        .unwrap_or_else(|| panic!("no stopped instruction in:\n{listing}"));

    // The stop follows the frame setup, under its source line.
    assert!(lines[stopped].contains(" <main+0x4>: "), "{listing}");
    assert!(
        lines[stopped].ends_with(" <disasm_marked_data>") && lines[stopped].contains("call 0x"),
        "{listing}"
    );
    assert!(
        lines[stopped - 1].ends_with("tests/fixtures/c/disassembly/main.c:17"),
        "{listing}"
    );
    for expected in [
        "call 0x",
        " <disasm_helper>",
        "# 0x",
        " <disasm_counter>",
        " <printf@plt>",
        "push rbp",
    ] {
        assert!(
            listing.contains(expected),
            "missing {expected:?} in:\n{listing}"
        );
    }

    assert_no_sgr(&stdout);
}

#[test]
fn disassemble_reports_data_inside_code_and_unproven_addresses() {
    let executable = fixture("build/test-programs/disassembly-clang-o2-nopie");
    let hidden = symbol_address(&executable, "disasm_hidden_data");
    let inside = format!("disassemble {:#x} 2", hidden + 4);
    let stdout = batch(
        &[
            "--disassembly-syntax",
            "att",
            "build/test-programs/disassembly-clang-o2-nopie",
        ],
        &[
            "break main",
            "run",
            "disassemble disasm_marked_data",
            "disassemble disasm_hidden_data",
            &inside,
        ],
    );

    for expected in [
        format!(
            "this instruction overlaps {:#x}, where ",
            symbol_address(&executable, "disasm_marked_resume")
        ),
        "proves an instruction begins; decoding resumes there".to_owned(),
        format!(
            "this instruction extends past the end of the range at {:#x}",
            hidden + 10
        ),
        format!(
            "{:#x} lies inside the instruction at {:#x} when decoding from {hidden:#x}; it is probably not an instruction start",
            hidden + 4,
            hidden + 2
        ),
        "mov $1, %eax".to_owned(),
        "movabs".to_owned(),
    ] {
        assert!(
            stdout.contains(&expected),
            "missing {expected:?} in:\n{stdout}"
        );
    }
}

#[test]
fn disassemble_names_the_targets_indirect_branches_read_at_the_stop() {
    let executable = fixture("build/test-programs/disassembly-clang-o2-nopie");
    let address = |name| symbol_address(&executable, name);
    let data = fs::read(&executable).expect("read fixture");
    let object = object::File::parse(data.as_slice()).expect("parse fixture");
    let section = |name| {
        object
            .section_by_name(name)
            .unwrap_or_else(|| panic!("no section {name}"))
            .address()
    };

    let (plt, got_plt) = (section(".plt"), section(".got.plt"));
    let commands = [
        format!("break {:#x}", address("disasm_call_method")),
        format!("break {:#x}", address("disasm_call_register")),
        format!("break {:#x}", address("disasm_return")),
        "run\ndisassemble".to_owned(),
        format!("disassemble {:#x} 1", plt + 0x10),
        format!(
            "continue\ndisassemble {:#x} 1",
            address("disasm_call_register")
        ),
        format!("continue\ndisassemble {:#x} 1", address("disasm_return")),
        "disassemble disasm_indirect_forms".to_owned(),
    ];
    let commands = commands
        .iter()
        .flat_map(|command| command.lines())
        .collect::<Vec<_>>();
    let stdout = batch(
        &["build/test-programs/disassembly-clang-o2-nopie"],
        &commands,
    );
    let lines = stdout.lines().collect::<Vec<_>>();
    let find = |needle: &str, stopped: bool| {
        lines
            .iter()
            .find(|line| line.contains(needle) && line.starts_with("=> ") == stopped)
            .unwrap_or_else(|| panic!("no line with {needle:?} in:\n{stdout}"))
            .to_owned()
    };

    // At the stop, registers hold the target or address its slot.
    assert!(
        find("call qword ptr [rax+8]", true).ends_with(&format!(
            "  # {:#x} <disasm_methods+0x8> -> {:#x} <disasm_target_two>",
            address("disasm_methods") + 8,
            address("disasm_target_two")
        )),
        "{stdout}"
    );
    assert!(
        find("call rcx", true).ends_with(&format!(
            "call rcx  # -> {:#x} <disasm_target_four>",
            address("disasm_target_four")
        )),
        "{stdout}"
    );
    let ret = find("ret  # ", true);
    assert!(ret.contains(" <main+0x"), "{ret}");

    // Away from the stop, only slots the instruction addresses are read.
    assert!(find("call rcx", false).ends_with("call rcx"), "{stdout}");
    assert!(
        find(
            &format!("# {:#x} <disasm_slot>", address("disasm_slot")),
            false
        )
        .ends_with(&format!(
            "-> {:#x} <disasm_target_one>",
            address("disasm_target_one")
        )),
        "{stdout}"
    );
    let got = find(" <.got+0x", false);
    assert!(
        got.contains(" -> 0x7") && got.ends_with(" in libc.so.6>"),
        "{got}"
    );

    // Lazy binding has not yet filled the stub's slot, which is named once.
    let stub = find("jmp qword ptr [rip+", false);
    assert!(
        stub.ends_with(&format!(
            "jmp qword ptr [rip+{:#x}]  # {:#x} <.got.plt+0x18> -> {:#x} <printf@plt+0x6>",
            got_plt + 0x18 - (plt + 0x16),
            got_plt + 0x18,
            plt + 0x16
        )),
        "{stub}"
    );
    assert!(
        find("jmp qword ptr [0]", false).ends_with("  # 0x0 -> memory inaccessible at 0x0"),
        "{stdout}"
    );

    // Far transfers, interrupt returns, and branches whose operand size
    // differs between Intel and AMD processors say they are not computed.
    for form in ["jmp far fword ptr [rax]", "jmp rax", "iretq"] {
        assert!(
            find(form, false).ends_with(&format!(
                "{form}  # -> not computed for this form of branch"
            )),
            "{stdout}"
        );
    }

    assert_no_sgr(&stdout);
}

#[test]
fn disassemble_says_when_a_restarted_system_call_hides_a_stopped_target() {
    let target = support::ExternalProcess::spawn(&fixture("build/test-programs/attach-restart"));

    // Attaching must interrupt pause(2).
    support::wait_for_system_call(target.process_id(), 34);
    let process = target.process_id().get().to_string();
    let output = batch_output(&["--attach", &process], &["disassemble"]);
    drop(target);
    let stdout = assert_success(output);
    let stopped = stdout
        .lines()
        .find(|line| line.starts_with("=> "))
        .unwrap_or_else(|| panic!("no stopped instruction in:\n{stdout}"));
    assert!(
        stopped.ends_with(
            "jmp rax  # -> unknown: a restarted system call replaces a register it uses"
        ),
        "{stdout}"
    );
}

#[test]
fn disassemble_shows_a_stop_outside_every_function_and_rejects_bad_requests() {
    // A call through a null pointer stops where no module or function is.
    let stdout = batch(&["build/test-programs/null-call"], &["run", "disassemble"]);
    assert!(
        stdout.contains("memory inaccessible at 0x0"),
        "missing unreadable stop in:\n{stdout}"
    );

    let count = "instruction count must be between 1 and 4096";
    let (_, stderr) = piped(
        &["build/test-programs/disassembly-gcc-o0"],
        &[
            "break main",
            "run",
            "disassemble 0x8",
            "disassemble main 0",
            "disassemble main 4097",
            "disassemble missing_function",
        ],
    );
    assert_in_order(
        &stderr,
        &[
            "no function or code symbol contains 0x8; give an instruction count to disassemble from it",
            count,
            count,
            "no symbol named 'missing_function'",
        ],
    );
}

#[test]
fn disassemble_reads_code_from_core_dumps() {
    let stdout = batch(
        &["--core", "build/test-programs/crash-gcc-o2-nopie-segv.core"],
        &["disassemble", "disassemble main"],
    );
    assert!(
        stdout.contains("function crash_segv in crash-gcc-o2-nopie:"),
        "{stdout}"
    );
    assert!(
        stdout.lines().any(|line| line.starts_with("=> 0x")),
        "{stdout}"
    );

    // The optimized main has a cold range of its own.
    assert!(
        stdout.contains("range 1 of 2: 0x4010a0..0x4010a5"),
        "{stdout}"
    );
    assert!(stdout.contains(" <main.cold>: "), "{stdout}");
}

#[test]
fn run_starts_the_program_with_the_command_line_arguments_directory_and_environment() {
    let directory = support::ScratchDir::new("cli-launch-directory");
    let directory_argument = directory.path().to_str().expect("UTF-8 scratch path");
    let output = batch_output(
        &[
            "--cwd",
            directory_argument,
            "--env",
            "MICROSCOPE_FIXTURE_VALUE=a=b",
            "build/test-programs/process-environment",
            "--",
            "--not-an-option",
            "two words",
        ],
        &["run"],
    );
    let stdout = assert_success(output);
    let directory = directory
        .path()
        .canonicalize()
        .expect("canonical scratch directory");
    assert!(
        stdout.contains(&format!(
            "argument 1: --not-an-option\nargument 2: two words\nvalue: a=b\nremoved: absent\ndirectory: {}\n",
            directory.display()
        )),
        "{stdout}"
    );
    assert!(stdout.contains("exited with status 3"), "{stdout}");
}

#[test]
fn registers_show_the_selected_frames_own_values() {
    let stdout = batch(
        &["--core", FRAMES_CORE],
        &["registers", "frame 5", "registers"],
    );
    let lines = stdout.lines().collect::<Vec<_>>();
    let innermost = line_index(&lines, 0, |line| line.starts_with("rdi "));
    assert!(!lines[innermost].contains("<not saved>"), "{stdout}");
    let selected = line_index(&lines, innermost, |line| line.starts_with("#5 "));

    // A callee may overwrite rdi without saving it, so frame 5's value is
    // unknown, while its instruction pointer is its own return address.
    let rdi = line_index(&lines, selected, |line| line.starts_with("rdi "));
    assert!(lines[rdi].ends_with("<not saved>"), "{stdout}");
    let rip = line_index(&lines, selected, |line| line.starts_with("rip "));
    let frame_address = lines[selected]
        .split_whitespace()
        .find(|word| word.starts_with("0x"))
        .expect("frame address");
    assert!(lines[rip].ends_with(frame_address), "{stdout}");
}

#[test]
fn signal_fixtures_unblock_inherited_signals() {
    for (program, status) in [("signal-policy", 63), ("watch-gcc-o0", 0)] {
        let output = Command::new("env")
            .arg("--block-signal=USR1")
            .arg(fixture(&format!("build/test-programs/{program}")))
            .stdin(Stdio::null())
            .output()
            .expect("run the signal fixture with SIGUSR1 blocked");
        assert_eq!(output.status.code(), Some(status), "{program}: {output:?}");
    }
}

#[test]
fn handle_shows_and_changes_signal_policies_like_gdb() {
    let stdout = batch(
        &["build/test-programs/signal-policy"],
        &[
            "handle SIGUSR1",
            "handle usr1 nostop",
            "handle SIGALRM print nopass",
            "handle SIG35 noprint",
            "run",
        ],
    );
    let lines = stdout.lines().collect::<Vec<_>>();
    let shown = |name: &str, stop: &str, print: &str, pass: &str| {
        format!("{name:<8}  {stop:<4}  {print:<5}  {pass}")
    };

    // `nostop` keeps printing; `noprint` implies `nostop`.
    for expected in [
        shown("SIGUSR1", "yes", "yes", "yes"),
        shown("SIGUSR1", "no", "yes", "yes"),
        shown("SIGALRM", "no", "yes", "no"),
        shown("SIG35", "no", "no", "yes"),
    ] {
        assert!(
            lines.contains(&expected.as_str()),
            "{expected:?}:\n{stdout}"
        );
    }

    // Received signals are reported in order before the program exits with
    // every handler but the discarded SIGALRM's.
    let usr1 = line_index(&lines, 0, |line| {
        line.starts_with("thread ")
            && line.contains(" received SIGUSR1 (SI_TKILL) sent by process ")
    });
    let alarm = line_index(&lines, usr1, |line| line.contains(" received SIGALRM "));
    let exited = line_index(&lines, alarm, |line| line.contains("exited with status 61"));
    assert!(exited > alarm);
}

#[test]
fn info_signals_lists_every_signals_policy() {
    let stdout = batch(&[BASIC], &["info signals"]);
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "signal    stop  print  pass");
    assert_eq!(lines.len(), 65, "{stdout}");
    assert!(lines.contains(&"SIGINT    yes   yes    no"), "{stdout}");
    assert!(lines.contains(&"SIGURG    no    no     yes"), "{stdout}");
    assert!(lines.contains(&"SIG64     yes   yes    yes"), "{stdout}");
}

#[test]
fn the_terminal_launcher_explains_why_it_cannot_run_the_program() {
    let directory = support::ScratchDir::new("cli-launcher");
    let socket = directory.path().join("launcher");
    let output = microscope(&[
        "dap-launcher",
        "--connect",
        socket.to_str().expect("path"),
        "--",
        "/bin/true",
    ]);
    assert_eq!(output.status.code(), Some(127));
    assert_failure(
        &output,
        "microscope: cannot run /bin/true: No such file or directory",
    );

    // The adapter went away before it traced the launcher.
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
    let adapter = thread::spawn(move || drop(listener.accept().expect("accept")));
    let output = microscope(&[
        "dap-launcher",
        "--connect",
        socket.to_str().expect("path"),
        "--",
        "/bin/true",
    ]);
    adapter.join().expect("adapter");
    assert_eq!(output.status.code(), Some(127));
    assert_failure(
        &output,
        "microscope: cannot run /bin/true: the debugger stopped before it could debug it",
    );
}

/// A development build streams its flight recording as it goes, so even a
/// killed session leaves the requests, native calls, wait statuses, and
/// events that led up to the kill.
#[cfg(debug_assertions)]
#[test]
fn a_killed_session_leaves_its_flight_recording() {
    let scratch = support::ScratchDir::new("flight-recording");
    let recording = scratch.path().join("recording.log");

    // microscope keeps reading its open standard input after the script.
    let microscope = Microscope::spawn(
        microscope_command()
            .env("MICROSCOPE_FLIGHT_RECORDING", &recording)
            .args(["--eval", "break main", "--eval", "run"])
            .arg(fixture("build/test-programs/basic")),
    );

    let mut contents = String::new();
    support::wait_until("the recording shows the stop", || {
        contents = fs::read_to_string(&recording).unwrap_or_default();
        contents.contains("event InferiorStopped")
    });

    // Dropping kills microscope and reaps it.
    drop(microscope);

    let position = |needle: &str| {
        contents
            .find(needle)
            .unwrap_or_else(|| panic!("the recording lacks {needle:?}:\n{contents}"))
    };

    let order = [
        "request add breakpoint",
        "request launch",
        "spawn ",
        "stopped by SIGTRAP",
        "install breakpoint",
        "PTRACE_CONT",
        "classified",
        "event InferiorStopped",
    ]
    .map(position);
    assert!(order.is_sorted(), "out of order {order:?}:\n{contents}");

    // The inferior dies with the debugger that traced it.
    let inferior = contents
        .lines()
        .find_map(|line| line.split_once("spawned ")?.1.parse::<u32>().ok())
        .expect("the recording names the inferior");

    // Once microscope is gone, init reaps the inferior, if it ever does.
    support::wait_until(&format!("inferior {inferior} dies with microscope"), || {
        matches!(process_state(inferior), None | Some('Z'))
    });
}

#[test]
fn expressions_print_compute_and_point_at_their_errors() {
    let (stdout, stderr) = piped(
        &["build/test-programs/expressions-c-gcc-o0-pie"],
        &[
            "break barrier",
            "run",
            "up",
            "print f.u8 + 10",
            "print (u8)(f.u8 + 10)",
            "print f.u32 * f.u32",
            "print/x f.i32",
            "print/x -1",
            "print f.arr[1..3]",
            "print f.head->next->value",
            "print gone + 1",
            "print f.arr[9]",
            "print 1 / 0",
            "print f.nope",
            "print (1 < 2 < 3)",
            "print \"é\" == f.zzz",
            "whatis f.u8 + 1",
            "whatis f.inner",
            "whatis (short)f.i32",
            "ptype struct inner",
            "ptype f.color",
            "print 017",
        ],
    );

    for expected in [
        "(integer) f.u8 + 10 = 260",
        "(u8) (u8)(f.u8 + 10) = 4",
        "(integer) f.u32 * f.u32 = 16000000000000000000",
        "(int32_t) f.i32 = 0xfffeee90",
        "(integer) -1 = -0x1",
        "20, 30",
        "(int) f.head->next->value = 2",
        "type = integer",
        "type = inner\n",
        "type = short int\n",
        "type = struct inner {\n    short int s;\n    long long int ll;\n}",
        "type = enum color {RED = 0, GREEN = 5, BLUE = 7}",
    ] {
        assert!(
            stdout.contains(expected),
            "missing `{expected}` in:\n{stdout}\nstderr:\n{stderr}"
        );
    }

    for expected in [
        "no variable is named `gone` here\n    gone + 1\n    ^^^^\n",
        "index 9 is outside the source bounds starting at 0 with 5 elements\n    f.arr[9]\n    ^^^^^^^^\n",
        "division by zero\n    1 / 0\n    ^^^^^\n",
        "has no member named 'nope'\n    f.nope\n      ^^^^\n",
        "comparisons do not chain\n    (1 < 2 < 3)\n           ^\nhint: join comparisons with `&&`",
        "no member named 'zzz'\n    \"é\" == f.zzz\n             ^^^\n",
        "an integer cannot begin with 0\n    017\n    ^^^\nhint: write 0o17 for octal or 17 for decimal",
    ] {
        assert!(
            stderr.contains(expected),
            "missing `{expected}` in:\n{stderr}"
        );
    }
}

/// Code in the vDSO is shown as code in any library is: named by a symbol
/// when one covers it, and always by the module the kernel names `[vdso]`.
#[test]
fn vdso_code_is_shown_in_the_module_the_kernel_names() {
    let core = batch(
        &["--core", "build/test-programs/vdso-gcc-o0-clock.core"],
        &["info core", "bt", "where"],
    );
    let lines = core.lines().collect::<Vec<_>>();
    assert!(
        lines
            .iter()
            .any(|line| line.contains(" [vdso] module ") && line.ends_with(" read from the dump")),
        "{core}"
    );
    let innermost = lines
        .iter()
        .position(|line| line.starts_with("#0 "))
        .unwrap_or_else(|| panic!("{core}"));
    assert!(
        lines[innermost].ends_with(" in <unknown> from [vdso]"),
        "{core}"
    );
    assert!(
        lines[innermost + 1].contains(" in clock_gettime+0x")
            && lines[innermost + 1].contains(" from libc.so"),
        "{core}"
    );
    assert!(
        lines[innermost + 2].contains(" in vdso_clock at "),
        "{core}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("<unknown> at 0x") && line.ends_with(" from [vdso]")),
        "{core}"
    );

    let live = batch(
        &["build/test-programs/vdso-gcc-o0", "--", "time"],
        &["run", "bt"],
    );
    let innermost = live
        .lines()
        .find(|line| line.starts_with("#0 "))
        .unwrap_or_else(|| panic!("{live}"));
    assert!(
        (innermost.contains(" in time+0x") || innermost.contains(" in __vdso_time+0x"))
            && innermost.ends_with(" from [vdso]"),
        "{live}"
    );
}

/// `info modules` says what describes each module, and for one stripped of
/// its debug information which separate file it came from, or why the
/// one found could not be used.
#[test]
fn info_modules_names_separate_debug_files_and_why_one_is_unusable() {
    const PROGRAM: &str = "build/test-programs/split/basic-build-id";
    let output = batch(
        &[
            "--debug-directory",
            "build/test-programs/split/debug-root",
            PROGRAM,
        ],
        &[
            "info modules",
            "break breakpoint_target",
            "run",
            "info modules",
        ],
    );
    assert_in_order(
        &output,
        &[
            "not loaded",
            "debug",
            "basic-build-id",
            "\n  debug information from ",
            ".debug",
            "breakpoint 1 set at breakpoint_target at ",
            "stopped at breakpoint 1",
            "\n0x",
            "debug",
            "basic-build-id",
            "\n  debug information from ",
            "split/debug-root/.build-id/",
            "symbols  ",
            "libc.so.6",
        ],
    );

    let output = batch(
        &[
            "--debug-directory",
            "build/test-programs/split/altlink-root",
            PROGRAM,
        ],
        &["info modules"],
    );
    assert_in_order(
        &output,
        &[
            "not loaded",
            "symbols",
            "basic-build-id",
            "\n  cannot use the debug file ",
            ": its dwz supplementary file ../../.dwz/microscope-fixture (build-id 01020304) \
             was not found",
        ],
    );
}

/// A file that is no program the debugger can debug fails to load, saying
/// what it is instead.
#[test]
fn files_that_are_no_program_to_debug_say_what_they_are() {
    let scratch = support::ScratchDir::new("no-program");
    let text = scratch.path().join("notes.txt");
    fs::write(&text, "not a program\n").expect("write a text file");
    let basic = fs::read(fixture(BASIC)).expect("read basic");
    let truncated = scratch.path().join("truncated");
    fs::write(&truncated, &basic[..basic.len() / 2]).expect("write half a program");

    // `e_machine` names AArch64.
    let mut foreign = basic.clone();
    foreign[18..20].copy_from_slice(&183_u16.to_le_bytes());
    let aarch64 = scratch.path().join("aarch64");
    fs::write(&aarch64, foreign).expect("write a foreign program");

    for (program, reason) in [
        (text.display().to_string(), "not an ELF file".to_owned()),
        (
            truncated.display().to_string(),
            format!(
                "the file is truncated: its ELF headers describe {} bytes, but it has {}",
                basic.len(),
                basic.len() / 2
            ),
        ),
        (
            "build/test-programs/basic.o".to_owned(),
            "it is a relocatable object file, whose DWARF has relocations only a linker applies; \
             debug the executable or shared library it is linked into"
                .to_owned(),
        ),
        (
            SEGV_CORE.to_owned(),
            "it is a core dump, not a program; open it as a core dump".to_owned(),
        ),
        (
            aarch64.display().to_string(),
            "it is a program for AArch64, and this debugger debugs x86-64 programs".to_owned(),
        ),
    ] {
        assert_failure(
            &batch_output(&[&program], &["info modules"]),
            &format!("error: failed to initialize debugger for {program}: {reason}\n"),
        );
    }
}

/// A batch run whose program replaces itself with another, which
/// microscope does not follow, fails rather than exiting 0 with the user's
/// code never debugged, and says why after the stop's own line.
#[test]
fn a_batch_run_fails_when_the_program_execs_one_it_does_not_follow() {
    let output = batch_output(&["build/test-programs/thread-exec"], &["run"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("inferior executed a program microscope cannot load"),
        "the stop is reported: {stdout}"
    );
    assert_failure(
        &output,
        "error: the inferior replaced its program with another, which microscope does not \
         follow; launch that program itself\n",
    );
}

/// Debug information that cannot be used is warned of as the session
/// starts, for the program, and as a library loads, and `info modules`
/// says why beneath each module; a program with none says so.
#[test]
fn unusable_debug_information_is_warned_of_and_listed() {
    let scratch = support::ScratchDir::new("unusable-debug-information");
    let program = scratch.path().join("module-frames");
    let library = scratch.path().join("libmodule-frames.so");
    support::corrupt_section(
        &fixture("build/test-programs/module-frames-gcc-o0"),
        &program,
        ".debug_line",
    );
    support::corrupt_section(
        &fixture("build/test-programs/libmodule-frames.so"),
        &library,
        ".debug_abbrev",
    );
    let output = batch_output(
        &[program.to_str().expect("a UTF-8 path")],
        &["break dso_apply", "run", "info modules"],
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = assert_success(output);
    let unreadable = "its debug information cannot be read, so only its symbols describe its code: \
                      malformed DWARF in the";
    assert!(
        stderr.starts_with(&format!(
            "warning: {}: {unreadable} line program at .debug_line+0x0 of the unit at \
             .debug_info+0x0: ",
            program.display()
        )),
        "{stderr}"
    );
    assert_in_order(
        &stdout,
        &[
            &format!(
                "warning: {}: {unreadable} abbreviations at .debug_abbrev+0x0 of the unit at \
                 .debug_info+0x0: ",
                library.display()
            ),
            "stopped at breakpoint 1",
            &format!(
                "symbols  {}\n  {unreadable} line program at .debug_line+0x0",
                program.display()
            ),
            &format!(
                "symbols  {}\n  {unreadable} abbreviations at .debug_abbrev+0x0",
                library.display()
            ),
        ],
    );

    let stripped = "build/test-programs/split/basic-build-id";
    let output = batch_output(&[stripped], &["info modules"]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_success(output);
    assert_eq!(
        stderr,
        format!(
            "warning: {}: it has no debug information, so only its symbols describe its code: \
             source lines, variables, and types are unavailable\n",
            fixture(stripped)
                .canonicalize()
                .expect("the program")
                .display()
        )
    );
}

/// `jump` moves the stopped thread to a line of its function without
/// running it, by number, by offset, or as `file:line`, and refuses a
/// location outside the function; `set var` assigns registers.
#[test]
fn jump_moves_the_thread_to_resume_elsewhere_in_its_function() {
    let line = |marker: &str| support::source_line("tests/fixtures/c/jump.c", marker);
    let (output, errors) = piped(
        &["build/test-programs/jump"],
        &[
            &format!("break jump.c:{}", line("jump: start")),
            "run",
            &format!("jump {}", line("jump: target")),
            "print status",
            "jump elsewhere",
            "set var $rax = 5",
            "print $rax",
            "jump +1",
            "continue",
        ],
    );
    assert_in_order(
        &output,
        &[
            "stopped at breakpoint 1 (hit 1) in checked",
            "stopped where the thread was moved to resume in checked at tests/fixtures/c/jump.c:17",
            " => 17 |     status += 100; // jump: target",
            "(volatile int) status = 0",
            "(u64) $rax = 5",
            "stopped where the thread was moved to resume in checked at tests/fixtures/c/jump.c:18",
            // Neither `status = value` nor `status += 100` ran.
            "inferior exited with status 0",
        ],
    );

    assert_in_order(
        &errors,
        &["error: stdin:5: elsewhere has no code in the function the thread is stopped in"],
    );
}

#[test]
fn step_goes_into_one_call_of_the_line() {
    let line = |marker: &str| support::source_line("tests/fixtures/c/step-targets.c", marker);
    let next = format!(
        "stopped after source step in main at tests/fixtures/c/step-targets.c:{}",
        line("targets: recursive")
    );
    let (output, errors) = piped(
        &["build/test-programs/step-targets-gcc-o0"],
        &[
            &format!("break step-targets.c:{}", line("targets: calls")),
            "run",
            "info calls",
            "step add",
            "print a",
            "finish",
            "next",
            "info calls",
            "step strlen",
            "step nothing",
        ],
    );
    assert_in_order(
        &output,
        &[
            "stopped at breakpoint 1 (hit 1) in main",
            // GCC computes the arguments last to first.
            "calls on this line:",
            "  inc",
            "  twice",
            "  add",
            "stopped after source step in add at tests/fixtures/c/step-targets.c:15",
            "(int) a = 2",
            "returned (int) add = 4",
            "calls on this line:",
            "  (indirect)",
            "  strlen",
            // Stepping into code without source goes on to the next line.
            &next,
        ],
    );

    assert_in_order(
        &errors,
        &["error: stdin:10: no call on this line calls nothing; `info calls` lists them"],
    );
}

/// A backtrace shows the functions that left by tail calls between a frame
/// and its caller, and selecting one shows what was passed to it.
#[test]
fn backtraces_show_the_functions_that_left_by_tail_calls() {
    let line = |marker: &str| support::source_line("tests/fixtures/c/tail-frames.c", marker);
    let (output, errors) = piped(
        &["build/test-programs/tail-frames-gcc-o2"],
        &[
            &format!("break tail-frames.c:{}", line("frames: leaf")),
            "run",
            "bt",
            "frame 1",
            "print value",
            "print $pc",
        ],
    );
    assert_in_order(
        &output,
        &[
            "stopped at breakpoint 1 (hit 1) in leaf",
            &format!(
                " in middle [tail call] at tests/fixtures/c/tail-frames.c:{}",
                line("frames: middle")
            ),
            &format!(
                " in top [tail call] at tests/fixtures/c/tail-frames.c:{}",
                line("frames: top")
            ),
            &format!(
                " in main at tests/fixtures/c/tail-frames.c:{}",
                line("frames: call top")
            ),
            "(int) value = 6",
            "(u64) $pc = <unavailable: a tail call discarded the frame>",
        ],
    );

    assert!(errors.is_empty(), "{errors}");
}

/// A session reads and writes the image cache `MICROSCOPE_CACHE_DIR` names,
/// unless `--no-cache` says not to; a cache it cannot use leaves it
/// uncached, with a warning.
#[test]
fn sessions_cache_images_unless_told_not_to() {
    let directory = support::ScratchDir::new("cli-image-cache");
    let entries = |cache: &std::path::Path| {
        std::fs::read_dir(cache).map_or(0, |entries| {
            entries
                .filter(|entry| {
                    entry
                        .as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .ends_with(".image")
                })
                .count()
        })
    };

    let session = |cache: &std::path::Path, arguments: &[&str]| {
        let mut all = vec!["--batch", "--eval", "break main"];
        all.extend_from_slice(arguments);
        all.push(BASIC);
        microscope_command()
            .env("MICROSCOPE_CACHE_DIR", cache)
            .args(&all)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stdin(Stdio::null())
            .output()
            .expect("run microscope")
    };

    let off = directory.path().join("off");
    let stdout = assert_success(session(&off, &["--no-cache"]));
    assert!(stdout.contains("breakpoint 1"), "{stdout}");
    assert!(!off.exists(), "an uncached session makes no cache");

    let on = directory.path().join("on");
    assert_success(session(&on, &[]));
    assert_eq!(entries(&on), 1, "the program's image");
    let stdout = assert_success(session(&on, &[]));
    assert!(stdout.contains("breakpoint 1"), "{stdout}");
    assert_eq!(entries(&on), 1, "read again, not written again");

    let file = directory.path().join("file");
    std::fs::write(&file, b"").unwrap();
    let output = session(&file.join("images"), &[]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        stderr.contains("warning: cannot use the image cache"),
        "{stderr}"
    );
    assert!(assert_success(output).contains("breakpoint 1"));
}

/// `--timings` reports where a session's stops and requests spend their
/// time, beside its loads: each stop from its last thread stopping to its
/// publication, the modules it refreshed, and every request.
#[test]
fn timings_report_each_stop_and_request() {
    let directory = support::ScratchDir::new("cli-timings");
    let report = directory.path().join("timings.json");
    let report_arg = report.display().to_string();
    assert_success(microscope(&[
        "--batch",
        "--timings",
        &report_arg,
        "--eval",
        "break breakpoint_target",
        "--eval",
        "run",
        "--eval",
        "backtrace",
        BASIC,
    ]));
    let report: microscope::timings::Report =
        serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    let phases = report
        .phases
        .iter()
        .map(|phase| phase.name.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for phase in ["load", "stop", "modules", "load_modules", "request"] {
        assert!(phases.contains(phase), "{phase} in {phases:?}");
    }
}

/// A debugging session profiles its program from outside as it runs: a
/// stopped program adds no samples, and what it runs after does.
#[test]
fn a_session_profiles_its_program_while_it_runs() {
    let scratch = support::ScratchDir::new("cli-profile");
    let stopped = scratch.path().join("stopped.usprof");
    let ran = scratch.path().join("ran.usprof");
    let stdout = batch(
        &[
            "build/test-programs/profile-stacks-gcc-o0",
            "--",
            "20000000",
        ],
        &[
            "break leaf",
            "run",
            "profile start",
            &format!("profile save {}", stopped.display()),
            "delete 1",
            "continue",
            &format!("profile save {}", ran.display()),
            "profile stop",
        ],
    );
    assert!(stdout.contains("Profiling process "), "{stdout}");
    assert!(
        stdout.contains(&format!("Saved 0 samples to {}.", stopped.display())),
        "{stdout}"
    );
    let profiled = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Profiled ")?.strip_suffix(" samples."))
        .and_then(|count| count.parse::<u64>().ok())
        .expect("a count of samples");
    assert!(profiled > 0, "{stdout}");
    let report = assert_success(microscope(&["report", &ran.to_string_lossy()]));
    assert!(report.contains("leaf"), "{report}");
}

#[test]
fn a_signal_saves_a_recordings_window_as_it_goes_on() {
    let scratch = support::ScratchDir::new("cli-window");
    let profile = scratch.path().join("phases.usprof");
    let mut recording = microscope_command()
        .args(["record", "--window", "0.1", "-o"])
        .arg(&profile)
        .arg("--")
        .arg(fixture("build/test-programs/profile-phases"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run microscope record");
    let mut lines = BufReader::new(recording.stderr.take().expect("stderr")).lines();
    let mut line = || lines.next().expect("a line").expect("read a line");
    assert!(line().contains("saves it"));

    // Until a saved window holds the first phase, which runs until the
    // program is released.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut signals = 0;
    loop {
        assert!(Instant::now() < deadline, "no window held the first phase");
        kill(Pid::from_raw(recording.id().cast_signed()), Signal::SIGUSR2).expect("signal");
        signals += 1;
        let window = scratch.path().join(format!("phases-{signals}.usprof"));
        assert!(line().ends_with(&window.display().to_string()));
        if every_function(&window).contains("first") {
            break;
        }
    }

    let mut stdin = recording.stdin.take().expect("stdin");
    stdin.write_all(b"x").expect("release the program");
    drop(stdin);
    assert!(recording.wait().expect("wait").success());
    let last = every_function(&profile);
    assert!(last.contains("second") && !last.contains("first"), "{last}");
}

/// A profile's whole call tree. The default report lists only the top
/// functions, which unsymbolized vDSO addresses, a row each, can fill.
fn every_function(profile: &Path) -> String {
    let profile = profile.to_string_lossy();
    assert_success(microscope(&[
        "report",
        "--tree",
        "--min-percent",
        "0",
        &profile,
    ]))
}

/// A continuous recording writes a segment file as each segment ends, or
/// as a signal ends one early, and a report of the directory reads them
/// all as one profile.
#[test]
fn a_continuous_recording_writes_segments_that_report_reads_as_one() {
    let scratch = support::ScratchDir::new("cli-continuous");
    let directory = scratch.path().join("segments");
    let mut recording = microscope_command()
        .args([
            "record",
            "--segment",
            "3600",
            "--keep",
            "1h",
            "--continuous",
        ])
        .arg(&directory)
        .arg("--")
        .arg(fixture("build/test-programs/profile-phases"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run microscope record");
    let mut lines = BufReader::new(recording.stderr.take().expect("stderr")).lines();
    let line = lines.next().expect("a line").expect("read a line");
    assert!(line.contains("writing a segment every 3600 s"), "{line}");
    let segments = || {
        std::fs::read_dir(&directory)
            .expect("the directory")
            .filter(|entry| {
                entry.as_ref().is_ok_and(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "usprof")
                })
            })
            .count()
    };

    kill(Pid::from_raw(recording.id().cast_signed()), Signal::SIGUSR2).expect("signal");
    let deadline = Instant::now() + Duration::from_secs(20);
    while segments() == 0 {
        assert!(Instant::now() < deadline, "the signal wrote no segment");
        std::thread::yield_now();
    }

    // The second phase runs 300 ms, and the last segment holds it.
    let mut stdin = recording.stdin.take().expect("stdin");
    stdin.write_all(b"x").expect("release the program");
    drop(stdin);
    assert!(recording.wait().expect("wait").success());
    assert_eq!(segments(), 2);

    // The second phase's time is mostly in the clock it reads, so it is a
    // caller in the call tree rather than a function of its own.
    let report = assert_success(microscope(&[
        "report",
        "--tree",
        &directory.to_string_lossy(),
    ]));
    assert!(report.contains("second"), "{report}");
}

/// A program whose recorder dies goes on writing its instrumentation only
/// until it stops hearing from it, then switches every site off.
#[test]
fn instrumentation_switches_itself_off_once_its_recorder_dies() {
    let scratch = support::ScratchDir::new("cli-probes-quiet");
    let mut program = support::ExternalProcess::spawn_command(
        Command::new(fixture("build/test-programs/probes")).arg("quiet"),
    );
    let pid = program.process_id().get().to_string();
    let mut recorder = microscope_command()
        .args(["record", "--pid", &pid, "--probe-timeout", "0.2", "-o"])
        .arg(scratch.path().join("quiet.usprof"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("run microscope record");
    let mut lines = BufReader::new(program.take_stdout()).lines();
    let mut line = || lines.next().and_then(Result::ok);
    assert_eq!(line().as_deref(), Some("listening"));
    recorder.kill().expect("kill microscope");
    recorder.wait().expect("reap microscope");
    assert_eq!(line().as_deref(), Some("quiet"));
    assert!(program.wait().success());
}

#[test]
fn record_writes_a_profile_that_report_summarizes() {
    let scratch = support::ScratchDir::new("cli-record");
    let profile = scratch.path().join("leaf.usprof");
    let program = fixture("build/test-programs/profile-stacks-gcc-o0");
    let recorded = microscope_command()
        .args(["record", "--function", "leaf", "-o"])
        .arg(&profile)
        .arg("--")
        .arg(&program)
        .stdin(Stdio::null())
        .output()
        .expect("run microscope record");
    assert!(
        recorded.status.success(),
        "{}",
        String::from_utf8_lossy(&recorded.stderr)
    );
    assert!(String::from_utf8_lossy(&recorded.stderr).contains("recorded 6 samples"));

    let top = assert_success(microscope(&["report", &profile.to_string_lossy()]));
    assert_in_order(
        &top,
        &[
            "6 samples, 6 executions",
            "    own   total  function",
            "100.00% 100.00%  leaf",
            "  0.00% 100.00%  middle",
            "  0.00% 100.00%  outer",
        ],
    );
    let tree = assert_success(microscope(&[
        "report",
        "--tree",
        &profile.to_string_lossy(),
    ]));
    for line in [
        " 50.00%   0.00%  _start",
        " 50.00%   0.00%                  descend",
        " 50.00%  50.00%                        leaf",
        " 50.00%   0.00%  __GI___clone3",
        " 50.00%  50.00%            leaf",
    ] {
        assert!(
            tree.lines().any(|candidate| candidate == line),
            "{line:?} in\n{tree}"
        );
    }
}

#[test]
fn catch_syscall_stops_where_strace_would_print_and_reads_the_call() {
    let stdout = batch(
        &["build/test-programs/catch-gcc-o0"],
        &[
            "catch syscall write if $arg0 == 1",
            "run",
            "print $arg0",
            "continue",
            "print $ret",
            "info catchpoints",
            "delete c1",
            "info catch",
            "continue",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "catchpoint c1: write at entry and exit, if $arg0 == 1",
            r#"stopped at catchpoint c1 as the call began: write(1, "to stdout\n", 10)"#,
            "(integer) $arg0 = 1",
            r#"stopped at catchpoint c1 as the call returned: write(1, "to stdout\n", 10) = 10"#,
            "(integer) $ret = 10",
            // Both writes count, at their entries and exits.
            "c1: write at entry and exit, if $arg0 == 1, 4 hits",
            "deleted catchpoint c1",
            "no catchpoints",
            "inferior exited with status 0",
        ],
    );
}

/// A client that falls behind the debugger's events, as one does when a
/// stop loads more modules at once than the events it holds, as CPython's
/// JIT can register, still learns how its execution ended.
#[test]
fn a_run_that_misses_events_still_stops() {
    let output = microscope_command()
        .args(["--batch", "--eval", "break main", "--eval", "run", BASIC])
        .env("MICROSCOPE_EVENT_CAPACITY", "2")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::null())
        .output()
        .expect("run microscope");
    let stdout = assert_success(output);
    assert!(stdout.contains("stopped at breakpoint 1"), "{stdout}");
}

/// Writes an executable script whose first line is `line`.
fn script(path: &std::path::Path, line: &str) {
    use std::os::unix::fs::PermissionsExt as _;

    fs::write(path, format!("{line}\n")).expect("write a script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("make it executable");
}

/// A script runs as the kernel runs it: its interpreter, named directly,
/// through env, or through another script, is followed to exec-target,
/// whose breakpoint set before the run is hit, and which sees the same
/// arguments and exits as it does without microscope.
#[test]
fn scripts_run_their_interpreters_and_stop_in_the_program_they_reach() {
    let scratch = support::ScratchDir::new("scripts");
    let target = fixture("build/test-programs/exec-target");
    let env = which("env");

    let direct = scratch.path().join("direct");
    script(&direct, &format!("#!{}", target.display()));
    let through_env = scratch.path().join("through-env");
    script(
        &through_env,
        &format!("#!{} {}", env.display(), target.display()),
    );
    let nested = scratch.path().join("nested");
    script(&nested, &format!("#! \t{}", direct.display()));

    for program in [&direct, &through_env, &nested] {
        let untraced = Command::new(program)
            .arg("last word")
            .output()
            .expect("run the script");
        let printed = String::from_utf8_lossy(&untraced.stdout).into_owned();
        assert!(printed.ends_with("last word\n"), "{printed}");
        let status = untraced.status.code().expect("an exit code");

        let shown = program.display().to_string();
        let output = batch(
            &[&shown, "--", "last word"],
            &["break target_work", "run", "print argc", "continue"],
        );
        assert_in_order(
            &output,
            &[
                "stopped at breakpoint 1 (hit 1) in target_work",
                &format!("= {}", status - 40),
                &printed,
                &format!("inferior exited with status {status}\n"),
            ],
        );
    }

    // A script whose interpreter is missing says so before anything runs.
    let missing = scratch.path().join("missing");
    script(&missing, "#!/nonexistent/node --flag");
    let shown = missing.display().to_string();
    assert_failure(
        &batch_output(&[&shown], &["run"]),
        &format!("{shown} names the interpreter /nonexistent/node, which does not exist"),
    );
}

/// The file `name` resolves to on `PATH`.
fn which(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("no {name} on PATH"))
}

/// In a program that executes another, a breakpoint nothing loaded has
/// waits for it, with a notice, while a misspelling of a name it has is
/// still refused. A run goes through the exec, saying so, unless
/// `catch exec` stops it there.
/// The processes a launched program forks join the session with its
/// breakpoints. A run stops in, and selects, the one that stopped; each
/// member's commands act on it alone; and the run ends with the program.
#[test]
fn fork_children_join_the_session_and_each_stops_where_it_hits() {
    let stdout = batch(
        &["build/test-programs/tree", "--", "2", "direct", "serial"],
        &[
            "break child_work",
            "run",
            "processes",
            "process 1",
            "backtrace",
            "continue",
            "continue",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "breakpoint 1 set at child_work",
            "[2] process ",
            " (tree 2 direct serial) joined, started by [1]",
            "in [2] process ",
            "stopped at breakpoint 1 (hit 1) in child_work",
            "  [1] process ",
            "* [2] process ",
            "stopped at breakpoint 1 (hit 1)",
            "selected [1]",
            // The program waits for its child, which only it does.
            " in wait4",
            "[2] exited with status 0",
            "[3] process ",
            "joined, started by [1]",
            "in [3] process ",
            "stopped at breakpoint 1 (hit 1) in child_work",
            "[3] exited with status 0",
            "inferior exited with status 0",
        ],
    );
    // The program's own exit ends the run, and says so once.
    assert!(!stdout.contains("[1] exited"), "{stdout}");
}

/// `--follow none` leaves every child to run without microscope, which
/// says so, and the program's breakpoints stay its own.
#[test]
fn follow_none_leaves_fork_children_to_run_on_their_own() {
    let stdout = batch(
        &[
            "--follow",
            "none",
            "build/test-programs/tree",
            "--",
            "2",
            "direct",
        ],
        &["break child_work", "run"],
    );
    assert_eq!(
        stdout.matches(", which runs without microscope").count(),
        2,
        "{stdout}"
    );
    assert!(!stdout.contains("joined"), "{stdout}");
    assert!(!stdout.contains("stopped at breakpoint"), "{stdout}");
    assert!(stdout.contains("inferior exited with status 0"), "{stdout}");
}

/// `detach process N` lets a stopped member run on alone, without the
/// group's breakpoints, while the rest of the group stays debugged.
#[test]
fn a_detached_member_runs_on_alone() {
    let stdout = batch(
        &["build/test-programs/tree", "--", "1", "direct"],
        &[
            "break child_work",
            "run",
            "detach process 2",
            "processes",
            "continue",
        ],
    );
    assert_in_order(
        &stdout,
        &[
            "in [2] process ",
            "stopped at breakpoint 1 (hit 1) in child_work",
            "detached [2]",
            "* [1] process ",
            "  [2] process ",
            "  detached",
            // tree exits with the number of children that went wrong.
            "inferior exited with status 0",
        ],
    );
}

#[test]
fn a_program_that_executes_another_is_followed_into_it() {
    let (stdout, stderr) = piped(
        &["build/test-programs/exec-other"],
        &[
            "break befor_exec",
            "break target_work",
            "run",
            "continue",
            "catch exec",
            "run",
            "continue",
        ],
    );
    assert!(
        stderr.contains("no function named 'befor_exec' was found; did you mean 'before_exec'?"),
        "{stderr}"
    );
    assert_in_order(
        &stdout,
        &[
            "breakpoint 1 ",
            "(pending: nothing loaded has it, and the program may execute one that does)",
            " executed ",
            "exec-target",
            "stopped at breakpoint 1 (hit 1) in target_work",
            "inferior exited with status 42",
            "the inferior stops each time it executes a program",
            "stopped as the inferior executed a program",
            "stopped at breakpoint 1 (hit 1) in target_work",
        ],
    );
}
