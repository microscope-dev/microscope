//! A recording's system calls: what `record` keeps and `report --syscalls`
//! prints, with decoded arguments held to strace's own.

mod support;

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use regex::Regex;
use support::{ExternalProcess, Scenario, ScratchDir};

/// How long a test waits for a line it expects.
const DEADLINE: Duration = Duration::from_secs(30);

fn fixture() -> PathBuf {
    Scenario::fixture("syscalls")
}

fn microscope() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_microscope"));
    command.stdin(Stdio::null());
    command
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Runs `microscope` with `arguments` in `directory`, which must succeed,
/// and returns its standard output and error.
fn succeeds(directory: &Path, arguments: &[&str]) -> (String, String) {
    let output = microscope()
        .args(arguments)
        .current_dir(directory)
        .output()
        .expect("run microscope");
    let (stdout, stderr) = (text(&output.stdout), text(&output.stderr));
    assert!(output.status.success(), "{arguments:?}: {stderr}");
    (stdout, stderr)
}

/// Records `program` with the ptrace engine into `trace.usprof` in
/// `directory`, with `arguments`: its exit code and what it said.
fn record(
    directory: &Path,
    arguments: &[&str],
    program: &Path,
    program_arguments: &[&str],
) -> (Option<i32>, String) {
    let output = microscope()
        .args(["record", "--syscall-engine", "ptrace", "-o", "trace.usprof"])
        .args(arguments)
        .arg("--")
        .arg(program)
        .args(program_arguments)
        .current_dir(directory)
        .output()
        .expect("run microscope record");
    let said = text(&output.stderr);
    assert!(directory.join("trace.usprof").exists(), "{said}");
    (output.status.code(), said)
}

fn read(directory: &Path) -> microscope::profiler::Profile {
    microscope::profiler::Profile::read(&directory.join("trace.usprof"))
        .expect("the capture reads")
        .profile
}

/// A log line without its time: the thread and the call.
fn untimed(line: &str) -> Option<&str> {
    let (time, rest) = line.trim_start().split_once(' ')?;
    time.parse::<f64>().ok()?;
    Some(rest.trim_start())
}

/// `report --syscalls` of `trace.usprof` in `directory` with `arguments`,
/// each line without its time or duration.
fn log(directory: &Path, arguments: &[&str]) -> String {
    let duration = Regex::new(r" <[\d.]+ (ns|µs|ms|s)>$").expect("regex");
    let (log, _) = succeeds(
        directory,
        &[&["report", "trace.usprof", "--syscalls"][..], arguments].concat(),
    );
    log.lines()
        .filter_map(untimed)
        .map(|line| duration.replace(line, "").into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `text` without the paths shown beside descriptors.
fn unpathed(text: &str) -> String {
    Regex::new(r"\b(\d+|AT_FDCWD)<(?:[^<>]|<[^<>]*>)*>")
        .expect("regex")
        .replace_all(text, "$1")
        .into_owned()
}

/// A log line without its thread.
fn unthreaded(line: &str) -> &str {
    line.split_once(' ').map_or(line, |(_, rest)| rest)
}

/// What differs between two runs of one program: addresses, ids, the
/// size of the environment, and random bytes, and the padding a pid or
/// address of another width moves: after the pid, and before the result.
fn normalized(trace: &str) -> String {
    let addresses = Regex::new(r"0x[0-9a-f]+").expect("regex");
    let ids = Regex::new(r"\b\d{4,}\b").expect("regex");
    let vars = Regex::new(r"/\* \d+ vars \*/").expect("regex");
    let random = Regex::new(r#"getrandom\("[^"]*""#).expect("regex");
    let pid = Regex::new(r"^\d+ +").expect("regex");
    let pid_result = Regex::new(r"^((?:N )?get(?:pid|ppid|tid)\(\) += )[1-9]\d*\b").expect("regex");
    let padding = Regex::new(r"\) +=").expect("regex");

    trace
        .lines()
        .map(|line| {
            let line = random.replace_all(line, "getrandom(\"\"");
            let line = vars.replace_all(&line, "/* vars */");
            let line = pid.replace(&line, "N ");
            let varied = addresses.is_match(&line)
                || ids.is_match(&line)
                || line.starts_with("N ")
                || pid_result.is_match(&line);
            let line = pid_result.replace(&line, "${1}N");
            let line = addresses.replace_all(&line, "0x?");
            let line = ids.replace_all(&line, "N");
            if varied {
                padding.replace(&line, ") =").into_owned()
            } else {
                line.into_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// strace pads a call to its result's column from the call's real width,
/// so ids and addresses of another width move the result; anything else
/// that moves it is a difference.
#[test]
fn traces_that_differ_only_in_an_ids_width_normalize_alike() {
    assert_eq!(
        normalized("dup2(5<pipe:[999999]>, 9)          = 9<pipe:[999999]>"),
        normalized("dup2(5<pipe:[1000000]>, 9)         = 9<pipe:[1000000]>"),
    );
    assert_eq!(
        normalized("mmap(0x7f12, 4096)                      = 0x7f12"),
        normalized("mmap(0x7f1234, 4096)                    = 0x7f1234"),
    );
    assert_eq!(
        normalized("9999  write(1, \"child\\n\", 6) = 6"),
        normalized("10000 write(1, \"child\\n\", 6) = 6"),
    );
    assert_eq!(
        normalized("execve(\"a\", [\"a\"], 0x7ffd /* 42 vars */) = 0"),
        normalized("execve(\"a\", [\"a\"], 0x2000 /* 43 vars */) = 0"),
    );
    assert_ne!(
        normalized("close(3)                                = 0"),
        normalized("close(3)                               = 0"),
    );
}

#[test]
fn low_pids_normalize_only_in_pid_contexts() {
    for (ours, theirs) in [
        (
            "458   write(1, \"child\\n\", 6)            = 6",
            "351   write(1, \"child\\n\", 6)            = 6",
        ),
        ("458   +++ exited with 4 +++", "351   +++ exited with 4 +++"),
        (
            "513   getppid()                         = 376",
            "371   getppid()                         = 4194300",
        ),
        (
            "1     getpid()                          = 1",
            "12    getpid()                          = 12",
        ),
    ] {
        assert_eq!(normalized(ours), normalized(theirs), "{ours}\n{theirs}");
    }

    for (ours, theirs) in [
        (
            "write(1, \"child\\n\", 6) = 6",
            "write(1, \"child\\n\", 7) = 6",
        ),
        ("read(3, \"abc\", 3) = 3", "read(3, \"abc\", 3) = 2"),
        ("close(3) = 0", "close(4) = 0"),
        ("+++ exited with 4 +++", "+++ exited with 5 +++"),
        (
            "getppid() = -1 ESRCH (No such process)",
            "getppid() = -2 ESRCH (No such process)",
        ),
    ] {
        assert_ne!(normalized(ours), normalized(theirs), "{ours}\n{theirs}");
    }
}

/// Fails at the first line where the traces differ, showing both.
fn assert_same(ours: &str, theirs: &str, context: &str) {
    let (ours, theirs) = (normalized(ours), normalized(theirs));
    let (ours, theirs): (Vec<&str>, Vec<&str>) = (ours.lines().collect(), theirs.lines().collect());

    for (index, (our, their)) in ours.iter().zip(&theirs).enumerate() {
        assert_eq!(our, their, "line {} differs {context}", index + 1);
    }

    assert_eq!(
        ours.len(),
        theirs.len(),
        "lengths differ {context}:\n{}\n---\n{}",
        ours.join("\n"),
        theirs.join("\n")
    );
}

/// The fixture's calls in `mode` as strace decodes them, and as a ptrace
/// recording's report does, with the paths it adds to descriptors left
/// out: strace runs as it does by default, with only `filter` given.
fn decoded(filter: &str, mode: &str) -> (String, String) {
    let directory = ScratchDir::new("syscalls-strace");
    let theirs = directory.path().join("strace.out");
    let strace = Command::new("strace")
        .args(["-f", "-e", filter, "-o"])
        .arg(&theirs)
        .arg(fixture())
        .arg(mode)
        .current_dir(directory.path())
        .stdin(Stdio::null())
        .output()
        .expect("run strace");
    assert!(strace.status.code().is_some(), "{}", text(&strace.stderr));

    record(directory.path(), &[], &fixture(), &[mode]);
    let ours = unpathed(&log(directory.path(), &[filter]));
    (std::fs::read_to_string(&theirs).expect("read strace"), ours)
}

/// Every call of a program, from the loader on, decodes its arguments
/// and result as strace does: paths, flags, structures, buffers, and
/// errors.
#[test]
fn files_and_buffers_decode_as_strace_decodes_them() {
    for (filter, mode) in [
        ("trace=all", "files"),
        ("trace=pipe2,write,read,writev,readv", "dumps"),
    ] {
        let (theirs, ours) = decoded(filter, mode);
        assert_same(&ours, &theirs, &format!("for {mode} with {filter}"));
    }
}

/// Signals, with their senders, and the 32-bit and x32 calls decode as
/// strace decodes them.
#[test]
fn signals_and_compat_calls_decode_as_strace_decodes_them() {
    for (filter, mode) in [
        ("trace=kill,rt_sigaction", "signal"),
        ("trace=getpid@32,getpid@x32,getpid", "compat"),
    ] {
        let (theirs, ours) = decoded(filter, mode);
        assert_same(&ours, &theirs, &format!("for {mode} with {filter}"));
    }
}

/// Every class names the calls strace's does. The sweep makes every
/// 64-bit call with invalid arguments, each in a child, under strace
/// tracing one class; calls that ignore their arguments are left out.
#[test]
fn classes_name_the_calls_strace_names() {
    use microscope::syscalls::filter::Filter;
    use microscope::syscalls::{Abi, table};
    use std::collections::BTreeSet;

    const UNSAFE: [&str; 7] = [
        "exit",
        "exit_group",
        "rt_sigreturn",
        "pause",
        "fork",
        "vfork",
        "vhangup",
    ];

    let calls: Vec<(u32, &str)> = table::calls(Abi::X86_64)
        .iter()
        .copied()
        .filter(|(_, name)| !UNSAFE.contains(name))
        .collect();
    let numbers: Vec<String> = calls.iter().map(|(number, _)| number.to_string()).collect();
    let directory = ScratchDir::new("syscalls-classes");
    let named = Regex::new(r"^(?:\d+ +)?([a-z_0-9]+)\(").expect("regex");
    let mut wrong = Vec::new();

    for class in [
        "file", "desc", "net", "process", "signal", "ipc", "memory", "creds", "clock", "stat",
        "lstat", "fstat", "%stat", "statfs", "fstatfs", "%statfs", "pure",
    ] {
        let out = directory.path().join("strace.out");
        let status = Command::new("strace")
            .args(["-f", "-qq", "-e", &format!("trace=%{class}"), "-o"])
            .arg(&out)
            .arg(Scenario::fixture("syscall-sweep"))
            .args(&numbers)
            .current_dir(directory.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .expect("run strace");

        assert!(status.success());

        let traced = std::fs::read_to_string(&out).expect("read strace");

        // strace calls numbers it has no name for `syscall_0x...`, and
        // traces them under every class.
        let theirs: BTreeSet<&str> = traced
            .lines()
            .filter_map(|line| Some(named.captures(line)?.get(1)?.as_str()))
            .filter(|name| !name.starts_with("syscall_"))
            .collect();

        let filter = Filter::parse(&[format!("trace=%{class}")], &|_| None).expect("class");
        let ours: BTreeSet<&str> = calls
            .iter()
            .filter(|&&(number, _)| {
                filter
                    .calls
                    .contains(microscope::syscalls::SyscallId::new(Abi::X86_64, number))
            })
            .map(|&(_, name)| name)
            .collect();

        // The sweep itself forks and waits.
        let theirs: BTreeSet<&str> = theirs
            .into_iter()
            .filter(|name| {
                ours.contains(name) || !matches!(*name, "clone" | "wait4" | "execve" | "exit_group")
            })
            .collect();

        let missing: Vec<_> = theirs.difference(&ours).collect();
        let extra: Vec<_> = ours.difference(&theirs).collect();
        if !missing.is_empty() || !extra.is_empty() {
            wrong.push(format!(
                "%{class}: strace also names {missing:?}; microscope also names {extra:?}"
            ));
        }
    }

    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// A recording follows the program's children and threads: the child's
/// write and exit are in the log under its own id, which its parent's
/// wait returned.
#[test]
fn children_and_threads_are_followed() {
    let directory = ScratchDir::new("syscalls-family");
    record(directory.path(), &[], &fixture(), &["family"]);
    let lines = unpathed(&log(
        directory.path(),
        &["trace=write,wait4,getppid,exit_group"],
    ));
    let parent = Regex::new(
        r"(?m)^\d+ wait4\((\d+), \[\{WIFEXITED\(s\) && WEXITSTATUS\(s\) == 4\}\], 0, NULL\) = (\d+)$",
    )
    .expect("regex");
    let captures = parent
        .captures(&lines)
        .unwrap_or_else(|| panic!("no wait4 in\n{lines}"));
    let child = &captures[2];
    assert_eq!(&captures[1], child);

    for expected in [
        format!(r#"{child} write(1, "child\n", 6) = 6"#),
        format!("{child} +++ exited with 4 +++"),
    ] {
        assert!(lines.contains(&expected), "no {expected:?} in\n{lines}");
    }

    assert!(
        Regex::new(r"(?m)^\d+ getppid\(\) = \d+$")
            .expect("regex")
            .is_match(&lines),
        "{lines}"
    );
    assert!(lines.ends_with("+++ exited with 0 +++"), "{lines}");
}

/// The summary counts each call and its errors, the fixture's failed open
/// of "missing" among them, and totals every call.
#[test]
fn the_summary_counts_calls_and_errors() {
    let directory = ScratchDir::new("syscalls-summary");
    record(directory.path(), &[], &fixture(), &["files"]);
    let (table, _) = succeeds(
        directory.path(),
        &["report", "trace.usprof", "--syscalls", "--summary"],
    );
    let rows: Vec<Vec<&str>> = table
        .lines()
        .skip_while(|line| !line.starts_with("% time"))
        .take_while(|line| !line.is_empty())
        .map(|line| line.split_whitespace().collect())
        .collect();
    let row = |name: &str| {
        rows.iter()
            .find(|row| row.last() == Some(&name))
            .unwrap_or_else(|| panic!("no {name} row\n{table}"))
    };

    // % time, seconds, usecs/call, calls, errors when there are any, then
    // the name.
    let openat = row("openat");
    assert_eq!(openat.len(), 6, "{table}");
    let errors: u64 = openat[4].parse().expect("an error count");
    assert!(errors > 0, "{table}");
    assert_eq!(row("unlink")[3], "1", "{table}");
    assert_eq!(row("write")[3], "1", "{table}");

    let calls: u64 = rows
        .iter()
        .filter(|row| row[0].parse::<f64>().is_ok() && row.last() != Some(&"total"))
        .map(|row| row[3].parse::<u64>().expect("a call count"))
        .sum();
    assert_eq!(row("total")[3], calls.to_string(), "{table}");
    assert_eq!(calls, read(directory.path()).syscall_count(), "{table}");
}

/// A Rust program's call is placed in its own function, past the standard
/// library's frames that every call of a kind passes through: by std's
/// lines in a debug build, and by std's names in a release build, which
/// has no lines.
#[test]
fn a_rust_programs_call_is_placed_past_the_standard_library() {
    for (fixture, site) in [
        (
            "syscalls-rust-o0",
            "syscalls::open_settings · syscalls.rs:9",
        ),
        ("syscalls-rust-release", "syscalls::open_settings"),
    ] {
        let directory = ScratchDir::new(fixture);
        record(
            directory.path(),
            &["--syscall-filter", "trace=openat status=failed"],
            &Scenario::fixture(fixture),
            &[],
        );

        let profile = read(directory.path());
        let stacks: Vec<u32> = profile
            .syscalls()
            .events
            .iter()
            .filter(|event| matches!(event.event, microscope::syscalls::TraceEvent::Call(_)))
            .filter_map(|event| event.stack)
            .filter(|&stack| {
                let (functions, _) = profile.syscall_stack(stack);
                functions
                    .iter()
                    .any(|function| function.ends_with("open_settings"))
            })
            .collect();
        assert_eq!(stacks.len(), 1, "{fixture}");
        let (functions, _) = profile.syscall_stack(stacks[0]);
        assert_eq!(
            profile.syscall_site(stacks[0]).as_deref(),
            Some(site),
            "{fixture}: {functions:?}"
        );
    }
}

/// A recording's filter keeps only the calls it selects, each with its
/// stack, and the process's end; the report counts and lists those.
#[test]
fn a_recording_keeps_the_failed_calls_its_filter_selects() {
    let directory = ScratchDir::new("syscalls-capture");
    record(
        directory.path(),
        &[
            "--syscall-filter",
            "trace=openat,unlink,write status=failed",
        ],
        &fixture(),
        &["files"],
    );

    let profile = read(directory.path());
    let syscalls = profile.syscalls();
    let calls: Vec<_> = syscalls
        .events
        .iter()
        .filter_map(|event| match &event.event {
            microscope::syscalls::TraceEvent::Call(call) => Some((call, event.stack)),
            _ => None,
        })
        .collect();

    // The loader's failed opens, and the fixture's open of "missing".
    assert!(calls.len() > 1, "{calls:?}");
    let mut in_main = 0;
    for (call, stack) in &calls {
        assert!(call.outcome.raw().is_some_and(|ret| ret < 0), "{call:?}");
        let (functions, _) = profile.syscall_stack(stack.expect("a stack"));
        in_main += usize::from(functions.iter().any(|function| function == "main"));
    }
    assert_eq!(in_main, 1, "only the open of missing is the program's own");
    assert!(
        syscalls
            .events
            .iter()
            .any(|event| matches!(&event.event, microscope::syscalls::TraceEvent::Exit { .. })),
        "the process's end is noted"
    );

    let lines = unpathed(&log(directory.path(), &[]));
    let failed = lines.lines().filter(|line| line.contains(" = -1 ")).count();
    assert_eq!(failed, calls.len(), "{lines}");
    assert!(
        lines.contains(r#"openat(AT_FDCWD, "missing", O_RDONLY) = -1 ENOENT"#),
        "{lines}"
    );
}

/// A site in the summary's table of call sites: its name, seconds,
/// calls, and errors.
type SiteRow = (String, f64, u64, u64);

/// The rows of the summary's table of call sites, and the calls of its
/// total.
fn site_rows(report: &str) -> (Vec<SiteRow>, u64) {
    let number = |cell: &str| {
        let cell = cell.trim();
        if cell.is_empty() {
            0
        } else {
            cell.parse()
                .unwrap_or_else(|_| panic!("{cell:?} is no count\n{report}"))
        }
    };

    // After the heading, the columns' names, and a rule.
    let mut lines = report
        .lines()
        .skip_while(|line| *line != "By call site:")
        .skip(3);
    let rows = lines
        .by_ref()
        .take_while(|line| !line.starts_with("------"))
        .map(|line| {
            let seconds = line[7..18].trim().parse().expect("seconds");
            (
                line[51..].to_owned(),
                seconds,
                number(&line[19..28]),
                number(&line[29..38]),
            )
        })
        .collect();
    let total = lines.next().unwrap_or_else(|| panic!("no total\n{report}"));
    (rows, number(&total[19..28]))
}

/// The summary ranks a recording's call sites by their time, as the
/// browser's table of calls by site does, each with its calls and errors,
/// and shows `--limit` of them, counting the rest in the total.
#[test]
fn the_summary_ranks_call_sites_by_time() {
    let directory = ScratchDir::new("syscalls-sites");
    record(
        directory.path(),
        &["--syscall-filter", "trace=openat,close"],
        &fixture(),
        &["files"],
    );

    let made = read(directory.path()).syscall_count();
    let summary = |limit: &str| {
        succeeds(
            directory.path(),
            &[
                "report",
                "trace.usprof",
                "--syscalls",
                "--summary",
                "--limit",
                limit,
            ],
        )
        .0
    };
    let all = summary("1000");
    let (rows, total) = site_rows(&all);
    assert_eq!(total, made, "{all}");
    assert_eq!(rows.iter().map(|row| row.2).sum::<u64>(), made, "{all}");
    assert!(
        rows.windows(2).all(|pair| pair[0].1 >= pair[1].1),
        "the sites that took longest come first\n{all}"
    );

    let site = |line: u32| {
        let name = format!("files · syscalls.c:{line}");
        rows.iter()
            .find(|row| row.0 == name)
            .map_or_else(|| panic!("no row {name}\n{all}"), |row| (row.2, row.3))
    };

    // The open of "present", the failed open of "missing", and each close.
    assert_eq!(site(19), (1, 0), "{all}");
    assert_eq!(site(24), (1, 1), "{all}");
    for line in [23, 37, 38, 39, 40] {
        assert_eq!(site(line), (1, 0), "{all}");
    }

    let cut = summary("2");
    let (shown, total) = site_rows(&cut);
    assert_eq!(shown, rows[..2], "{cut}");
    assert_eq!(total, made, "{cut}");
    assert!(
        cut.contains(&format!("and {} more sites,", rows.len() - 2)),
        "{cut}"
    );
}

/// A program without lines has its calls grouped by the function that
/// called the C library.
#[test]
fn the_summary_groups_calls_without_lines_by_their_function() {
    let directory = ScratchDir::new("syscalls-sites-nolines");
    record(
        directory.path(),
        &["--syscall-filter", "trace=openat"],
        &Scenario::fixture("syscalls-nolines"),
        &["files"],
    );

    let (all, _) = succeeds(
        directory.path(),
        &["report", "trace.usprof", "--syscalls", "--summary"],
    );
    let (rows, _) = site_rows(&all);
    let files: Vec<(u64, u64)> = rows
        .iter()
        .filter(|row| row.0 == "files")
        .map(|row| (row.2, row.3))
        .collect();

    // Its three opens, one failed.
    assert_eq!(files, [(3, 1)], "{all}");
}

/// A thread other than the leader that executes a program takes the
/// leader's id, and the leader's blocked call ends unfinished: it is
/// recorded, not lost.
#[test]
fn a_leaders_blocked_call_ends_when_another_thread_executes() {
    let directory = ScratchDir::new("syscalls-thread-exec");
    record(
        directory.path(),
        &["--syscall-filter", "trace=pause"],
        &Scenario::fixture("thread-exec"),
        &[],
    );

    let profile = read(directory.path());
    let pauses: Vec<_> = profile
        .syscalls()
        .events
        .iter()
        .filter_map(|event| match &event.event {
            microscope::syscalls::TraceEvent::Call(call) if call.call.name() == Some("pause") => {
                Some(call.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(pauses.len(), 1, "{pauses:?}");
    assert_eq!(
        pauses[0].thread.get(),
        pauses[0].process.get(),
        "{pauses:?}"
    );
    assert_eq!(
        pauses[0].outcome,
        microscope::syscalls::Outcome::Unfinished,
        "{pauses:?}"
    );
}

/// A microscope run killed and reaped when dropped, whose standard error the
/// test reads line by line.
struct Running {
    child: Child,
    lines: mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl Running {
    fn spawn(command: &mut Command) -> Self {
        let mut child = command
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn microscope");

        let stderr = child.stderr.take().expect("stderr");
        let (sender, lines) = mpsc::channel();

        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { return };
                if sender.send(line).is_err() {
                    return;
                }
            }
        });

        Self {
            child,
            lines,
            seen: Vec::new(),
        }
    }

    /// Waits for a line that `wanted` accepts, failing at the deadline.
    fn line(&mut self, wanted: impl Fn(&str) -> bool) -> String {
        let end = Instant::now() + DEADLINE;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if wanted(&line) {
                        return line;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("no such line; saw:\n{}", self.seen.join("\n"))
                }
            }
        }
    }

    /// Waits for microscope to exit, failing at the deadline.
    fn finish(mut self) -> (Option<i32>, Vec<String>) {
        let end = Instant::now() + DEADLINE;
        loop {
            if let Some(status) = self.child.try_wait().expect("wait") {
                let rest: Vec<String> = self.lines.iter().collect();
                self.seen.extend(rest);
                return (status.code(), std::mem::take(&mut self.seen));
            }

            assert!(
                Instant::now() < end,
                "microscope did not exit; saw:\n{}",
                self.seen.join("\n")
            );

            // Polls for an exit the test has caused and can only observe so.
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Interrupts microscope, as Ctrl-C does.
    fn interrupt(&self) {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(i32::try_from(self.child.id()).expect("pid")),
            nix::sys::signal::Signal::SIGINT,
        )
        .expect("interrupt microscope");
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A ptrace recording attached to a running process detaches when it
/// ends, and the process runs on, untraced, to its own end.
#[test]
fn a_ptrace_recording_detaches_and_leaves_the_process_running() {
    let directory = ScratchDir::new("syscalls-ptrace-attach");
    let mut target = ExternalProcess::spawn(&Scenario::fixture("attach"));
    let pid = target.process_id().get().to_string();
    let mut running = Running::spawn(
        microscope()
            .args([
                "record",
                "--syscall-engine",
                "ptrace",
                "--syscalls",
                "calls",
                "-o",
                "trace.usprof",
                "--pid",
                &pid,
            ])
            .current_dir(directory.path()),
    );

    running.line(|line| line.contains(&format!("recording process {pid}")));
    running.interrupt();
    let (code, lines) = running.finish();
    assert_eq!(code, Some(0), "{lines:?}");
    assert_eq!(
        read(directory.path()).setting("syscall engine"),
        Some("ptrace"),
        "{lines:?}"
    );

    target.release();
    let status = target.wait();
    assert_eq!(status.code(), Some(23));
}

/// `record` traces calls with ptrace without privileges, beside its
/// samples and on their clock: each round's calls are traced, each falls
/// inside the recording's span, and the report warns that ptrace stretched
/// them. The load spins between rounds, so it is sampled too.
#[test]
fn a_ptrace_recording_holds_samples_and_calls_on_one_clock() {
    let directory = ScratchDir::new("syscalls-record-ptrace");
    let file = directory.path().join("ptrace.usprof");
    let recorded = Command::new(env!("CARGO_BIN_EXE_microscope"))
        .args([
            "record",
            "--syscall-engine",
            "ptrace",
            "--syscalls",
            "calls",
            "-o",
        ])
        .arg(&file)
        .arg("--")
        .arg(Scenario::fixture("syscall-load"))
        .args(["1", "200", "1000"])
        .stdin(Stdio::null())
        .output()
        .expect("run microscope record");
    assert!(recorded.status.success(), "{}", text(&recorded.stderr));

    let profile = microscope::profiler::Profile::read(&file)
        .expect("the capture reads")
        .profile;
    assert_eq!(profile.setting("syscall engine"), Some("ptrace"));
    assert!(profile.samples() > 50, "{} samples", profile.samples());
    let counts = profile.syscalls().stats().per_call();
    // The rounds' calls, and the loader's few.
    for name in ["read", "close"] {
        let calls = counts
            .iter()
            .find(|(call, _)| call.name() == Some(name))
            .map_or(0, |(_, stats)| stats.calls);
        assert!((200..210).contains(&calls), "{calls} {name}");
    }

    let (start, end) = profile.span();
    let (start, end) = (start.cast_unsigned(), end.cast_unsigned());
    let calls: Vec<u64> = profile
        .syscalls()
        .events
        .iter()
        .filter_map(|event| match &event.event {
            microscope::syscalls::TraceEvent::Call(call) => Some(call.time()),
            _ => None,
        })
        .collect();
    assert!(calls.len() >= 600);
    assert!(
        calls.iter().all(|time| (start..=end).contains(time)),
        "calls outside {start}..{end}"
    );

    let report = Command::new(env!("CARGO_BIN_EXE_microscope"))
        .arg("report")
        .arg(&file)
        .output()
        .expect("run microscope report");
    let report = text(&report.stdout);
    assert!(
        report.contains("Traced with ptrace, which stops the program"),
        "{report}"
    );
}

/// The names of the calls `profile` kept that `wanted` selects.
fn kept(
    profile: &microscope::profiler::Profile,
    wanted: impl Fn(&microscope::syscalls::SyscallEvent) -> bool,
) -> Vec<&'static str> {
    profile
        .syscalls()
        .events
        .iter()
        .filter_map(|event| match &event.event {
            microscope::syscalls::TraceEvent::Call(call) if wanted(call) => call.call.name(),
            _ => None,
        })
        .collect()
}

/// A window saved while the program runs holds the calls the program made
/// before it, from its exec on, and the recording's last save holds them
/// still.
#[test]
fn a_saved_window_leaves_its_calls_to_the_next() {
    let ready = |call: &microscope::syscalls::SyscallEvent| {
        call.call.name() == Some("write") && call.args[0] == 1 && call.args[2] == 6
    };
    let exec = |call: &microscope::syscalls::SyscallEvent| {
        call.call.name() == Some("execve")
            && matches!(call.outcome, microscope::syscalls::Outcome::Returned(0))
    };

    let mut windows = Vec::new();
    let mut program = Command::new(fixture());
    program.arg("block");
    let (profile, stderr) = support::held::recorded_until(
        support::held::unprivileged(),
        &program,
        &["--syscall-engine", "ptrace"],
        1,
        |window| {
            let shown = usize::from(!kept(window, ready).is_empty());
            if shown == 1 {
                windows.push(kept(window, exec).len());
            }
            shown
        },
    );

    assert_eq!(windows, [1], "{stderr}");
    assert_eq!(kept(&profile, ready).len(), 1, "{stderr}");
    assert_eq!(kept(&profile, exec).len(), 1, "{stderr}");
}

/// `syscall-exceptions` makes 3 getppid calls, 2 closes of -1 that fail
/// with EBADF from `failed`, and one 30 ms nanosleep from `slow`.
/// `record --print` streams each call as `report --syscalls` prints it,
/// `record` names the failures and the slowest site when it ends, and the
/// report's filter, `--focus`, and `--summary` pick from the same calls.
#[test]
fn a_recordings_calls_print_live_and_in_its_report_alike() {
    let directory = ScratchDir::new("syscalls-report-log");
    let fixture = Scenario::fixture("syscall-exceptions");
    let fixture = fixture.to_str().expect("a UTF-8 path");
    let (_, said) = succeeds(
        directory.path(),
        &[
            "record",
            "--syscall-engine",
            "ptrace",
            "--syscalls",
            "calls",
            "--print",
            "-o",
            "calls.usprof",
            "--",
            fixture,
            "3",
            "2",
            "1",
        ],
    );
    assert!(said.contains("system calls, "), "{said}");
    assert!(said.contains("close EBADF × 2"), "{said}");
    let busiest =
        Regex::new(r"most time:  (clock_)?nanosleep 3\d\.\d{3} ms in 1 call, ").expect("regex");
    assert!(busiest.is_match(&said), "{said}");
    assert!(said.contains("slow · syscall-exceptions.c:16"), "{said}");

    let (log, _) = succeeds(directory.path(), &["report", "calls.usprof", "--syscalls"]);
    let printed: Vec<&str> = said.lines().filter_map(untimed).collect();
    let logged: Vec<&str> = log.lines().filter_map(untimed).collect();
    assert!(logged.len() > 20, "{log}");
    assert_eq!(printed, logged);
    let times: Vec<f64> = log
        .lines()
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect();
    assert!(times.is_sorted(), "{log}");

    let calls = |found: &str, call: &str| found.lines().filter(|line| line.contains(call)).count();
    assert_eq!(calls(&log, "getppid() = "), 3, "{log}");
    assert_eq!(calls(&log, "close(-1) = -1 EBADF"), 2, "{log}");

    let (failed, _) = succeeds(
        directory.path(),
        &[
            "report",
            "calls.usprof",
            "--syscalls",
            "trace=close status=failed",
        ],
    );
    // The exit line stays, as it does in every log.
    let failed: Vec<&str> = failed.lines().filter_map(untimed).collect();
    assert_eq!(failed.len(), 3, "{failed:?}");
    assert_eq!(
        calls(&failed.join("\n"), "close(-1) = -1 EBADF"),
        2,
        "{failed:?}"
    );
    let (focused, _) = succeeds(
        directory.path(),
        &["report", "calls.usprof", "--syscalls", "--focus", "slow"],
    );
    let focused: Vec<&str> = focused.lines().filter_map(untimed).collect();
    assert_eq!(focused.len(), 1, "{focused:?}");
    assert!(focused[0].contains(" clock_nanosleep"), "{focused:?}");

    let (table, _) = succeeds(
        directory.path(),
        &["report", "calls.usprof", "--syscalls", "--summary"],
    );
    assert!(
        table
            .lines()
            .any(|line| line.ends_with(" close") && line.split_whitespace().any(|cell| cell == "2")),
        "{table}"
    );
}

/// What `--print` and `report --syscalls` refuse, and why.
#[test]
fn printing_calls_needs_calls_to_print() {
    let directory = ScratchDir::new("syscalls-report-refusals");
    let (_, _) = succeeds(
        directory.path(),
        &[
            "record",
            "--syscalls",
            "off",
            "-o",
            "off.usprof",
            "--",
            "true",
        ],
    );
    for (arguments, wanted) in [
        (
            &["record", "--syscalls", "counts", "--print", "--", "true"][..],
            "--print shows each call, which --syscalls counts does not keep",
        ),
        (
            &["report", "off.usprof", "--syscalls"],
            "the recording kept no system calls",
        ),
        (
            &[
                "report",
                "off.usprof",
                "--syscalls",
                "trace=read",
                "--summary",
            ],
            "--summary counts every call",
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_microscope"))
            .args(arguments)
            .current_dir(directory.path())
            .stdin(Stdio::null())
            .output()
            .expect("run microscope");
        let message = text(&output.stderr);
        assert!(!output.status.success(), "{arguments:?}: {message}");
        assert!(message.contains(wanted), "{arguments:?}: {message}");
    }
}

/// The BPF engine, held to the ptrace engine's output for the same
/// program. It needs `CAP_BPF` and `CAP_PERFMON`, so these run only
/// through `just test-bpf`, which starts microscope with the `microscope-bpf`
/// wrapper that `nixosModules.microscope-bpf` installs.
mod bpf {
    use std::collections::BTreeSet;
    use std::io::{Read as _, Write as _};

    use microscope::profiler::{Profile, StackEnding};
    use microscope::syscalls::{Outcome, ProcessEnd, TraceEvent};

    use super::*;

    fn launcher() -> PathBuf {
        std::env::var_os("MICROSCOPE_BPF_LAUNCHER").map_or_else(
            || {
                panic!(
                    "MICROSCOPE_BPF_LAUNCHER names no launcher: run these with `just test-bpf`, \
                     which starts microscope through the microscope-bpf wrapper"
                )
            },
            PathBuf::from,
        )
    }

    /// microscope through the launcher, without the environment, which a
    /// launcher that raises capabilities does not pass on.
    fn privileged() -> Command {
        let mut command = Command::new(launcher());
        command
            .env_clear()
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .stdin(Stdio::null());
        command
    }

    /// Records `program` into `trace.usprof` in `directory` with
    /// `arguments`, by BPF unless they choose ptrace: the exit code.
    fn recorded(
        directory: &Path,
        arguments: &[&str],
        program: &Path,
        program_arguments: &[&str],
    ) -> Option<i32> {
        let mut command = if arguments.contains(&"ptrace") {
            let mut command = super::microscope();
            command.env_clear();
            command
        } else {
            privileged()
        };

        let output = command
            .args(["record", "-o", "trace.usprof"])
            .args(arguments)
            .arg("--")
            .arg(program)
            .args(program_arguments)
            .current_dir(directory)
            .output()
            .expect("run microscope record");
        assert!(
            directory.join("trace.usprof").exists(),
            "{}",
            text(&output.stderr)
        );
        output.status.code()
    }

    /// A directory named `name` below `scratch`.
    fn below(scratch: &ScratchDir, name: &str) -> PathBuf {
        let directory = scratch.path().join(name);
        std::fs::create_dir(&directory).expect("make a directory");
        directory
    }
    /// What the engines may differ in besides what any two runs do: the
    /// credentials the launcher passes on.
    fn comparable(trace: &str) -> String {
        let ids = Regex::new(r"\b(st_uid|st_gid|si_uid)=\d+").expect("regex");
        normalized(&ids.replace_all(trace, "$1=ID"))
    }
    /// The fixture's calls in `mode` as each engine recorded them, as
    /// `report --syscalls FILTER` prints them: the ptrace engine's, then
    /// the BPF engine's.
    fn engines(filter: &str, mode: &str) -> (String, String) {
        // One directory, so that the paths beside descriptors match.
        let scratch = ScratchDir::new("syscalls-engines");
        let directory = scratch.path();
        let program = [mode];
        let ptrace_code = recorded(
            directory,
            &["--syscall-engine", "ptrace"],
            &fixture(),
            &program,
        );
        let ptrace = log(directory, &[filter]);
        let bpf_code = recorded(directory, &[], &fixture(), &program);
        assert_eq!(ptrace_code, bpf_code, "exit codes differ");
        (ptrace, log(directory, &[filter]))
    }
    /// Each thread's lines, in order, apart from how threads interleave.
    fn threads(trace: &str) -> BTreeSet<Vec<String>> {
        let mut threads: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for line in trace.lines() {
            let (thread, rest) = line.split_once(' ').unwrap_or(("", line));
            threads.entry(thread).or_default().push(comparable(rest));
        }

        threads.into_values().collect()
    }
    /// Each engine records a program's calls alike, decoded alike.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn the_engines_trace_a_program_alike() {
        for (filter, mode) in [
            ("trace=all", "files"),
            ("trace=getpid@32,getpid@x32,getpid", "compat"),
            ("trace=kill,rt_sigaction", "signal"),
        ] {
            let (ptrace, bpf) = engines(filter, mode);
            assert_same(
                &comparable(&bpf),
                &comparable(&ptrace),
                &format!("for {mode} with {filter}"),
            );
        }
    }
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn handled_signals_keep_their_number_and_sender() {
        let directory = ScratchDir::new("syscalls-signal");
        let code = recorded(directory.path(), &[], &fixture(), &["signal"]);
        assert_eq!(code, Some(0));
        let trace = log(directory.path(), &["trace=kill"]);
        let kill = Regex::new(r"kill\((\d+), SIGUSR1\) = 0").expect("kill regex");
        let pid = &kill.captures(&trace).expect("successful kill")[1];
        let delivered: Vec<&str> = trace
            .lines()
            .map(unthreaded)
            .filter(|line| line.starts_with("--- "))
            .collect();
        assert_eq!(delivered.len(), 1, "{trace}");
        assert!(
            delivered[0].starts_with(&format!(
                "--- SIGUSR1 {{si_signo=SIGUSR1, si_code=SI_USER, si_pid={pid}, si_uid="
            )),
            "wrong signal or sender: {trace}"
        );
        assert!(trace.ends_with("+++ exited with 0 +++"), "{trace}");
    }
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn fatal_signals_do_not_report_group_exit_wakeups_as_deliveries() {
        for (mode, signal) in [("signal-term", "SIGTERM"), ("signal-kill", "SIGKILL")] {
            let directory = ScratchDir::new("syscalls-fatal-signal");
            recorded(directory.path(), &[], &fixture(), &[mode]);
            let trace = log(directory.path(), &["trace=kill"]);
            assert!(trace.contains(&format!(", {signal}) = 0")), "{trace}");
            assert!(
                trace.ends_with(&format!("+++ killed by {signal} +++")),
                "{trace}"
            );
            assert!(!trace.contains(" --- "), "fabricated delivery: {trace}");
        }
    }
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn an_ignored_signal_is_not_a_delivery() {
        let directory = ScratchDir::new("syscalls-ignored-signal");
        let code = recorded(directory.path(), &[], &fixture(), &["signal-ignore"]);
        assert_eq!(code, Some(0));
        let trace = log(directory.path(), &["trace=kill"]);
        assert!(trace.contains(", SIGUSR1) = 0"), "{trace}");
        assert!(trace.ends_with("+++ exited with 0 +++"), "{trace}");
        assert!(
            !trace.contains(" --- "),
            "ignored signal delivered: {trace}"
        );
    }
    /// A worker thread's exec takes the leader's id; its call is recorded
    /// once, with its arguments, whether it succeeds or fails late.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_workers_exec_is_paired_once_with_its_captured_arguments() {
        let exec = Regex::new(
            r#"^\d+ execveat\(\d+, "", \["true"\], 0x[0-9a-f]+ /\* 0 vars \*/, AT_EMPTY_PATH\) = (.+)$"#,
        )
        .expect("exec pattern");
        for late_failure in [false, true] {
            let directory = ScratchDir::new("syscalls-worker-exec");
            let program: &[&str] = if late_failure { &["late-failure"] } else { &[] };
            recorded(
                directory.path(),
                &[],
                &Scenario::fixture("thread-exec"),
                program,
            );
            let trace = unpathed(&log(
                directory.path(),
                &["trace=execve,execveat,exit_group"],
            ));
            let execs: Vec<&str> = trace
                .lines()
                .filter(|line| line.contains("execveat("))
                .collect();
            assert_eq!(execs.len(), 1, "{trace}");
            let result = &exec.captures(execs[0]).unwrap_or_else(|| panic!("{trace}"))[1];
            let wanted = if late_failure {
                "-1 EINVAL (Invalid argument)"
            } else {
                "0"
            };
            assert_eq!(result, wanted, "{trace}");
            assert!(!trace.contains("<unfinished"), "{trace}");

            let profile = read(directory.path());
            let events = profile.syscalls().events;
            let call = events
                .iter()
                .find_map(|event| match &event.event {
                    TraceEvent::Call(call) if call.call.name() == Some("execveat") => Some(call),
                    _ => None,
                })
                .expect("the exec");
            assert_eq!(call.thread.get(), call.process.get(), "{trace}");
            assert!(
                call.entered
                    .zip(call.exited)
                    .is_some_and(|(entered, exited)| exited > entered),
                "{call:?}"
            );

            let ends: Vec<&ProcessEnd> = events
                .iter()
                .filter_map(|event| match &event.event {
                    TraceEvent::Exit { end, .. } => Some(end),
                    _ => None,
                })
                .collect();
            assert_eq!(ends.len(), 1, "{trace}");
            match (late_failure, ends[0]) {
                (true, ProcessEnd::Killed { signal, .. }) => assert_eq!(*signal, 11),
                (false, ProcessEnd::Exited(code)) => assert_eq!(*code, 0),
                (_, end) => panic!("{end:?}: {trace}"),
            }
        }
    }
    /// Records `group-exits` in `mode`, through each kernel function the
    /// filter names when `fentry`, else through the tracepoints, and checks
    /// each thread and process ended once, as the fixture saw it.
    fn group_exits(mode: &str, fentry: bool, workers: u32, rounds: u32) {
        let filter = "trace=exit,exit_group,kill";
        let directory = ScratchDir::new("syscalls-group-exits");
        let arguments: &[&str] = if fentry {
            &["--syscall-filter", filter]
        } else {
            &[]
        };
        let output = privileged()
            .args(["record", "-o", "trace.usprof"])
            .args(arguments)
            .arg("--")
            .arg(fixture())
            .args([
                "group-exits",
                mode,
                &workers.to_string(),
                &rounds.to_string(),
            ])
            .current_dir(directory.path())
            .output()
            .expect("record terminating groups");
        let truth = text(&output.stdout);
        assert!(output.status.success(), "{}", text(&output.stderr));

        let profile = read(directory.path());
        let hook = if fentry { "fentry" } else { "tp_btf" };
        assert_eq!(profile.setting("syscall hook"), Some(hook));
        let trace = log(directory.path(), &[filter]);
        let context = format!("{mode}, {hook}:\n{truth}\n{trace}");
        let lost = profile.syscalls().lost.dropped;
        check_group_exits(mode, &truth, &trace, &context, workers, rounds, lost);
    }
    fn check_group_exits(
        mode: &str,
        truth: &str,
        trace: &str,
        context: &str,
        workers: u32,
        rounds: u32,
        lost: u64,
    ) {
        let number = |value: &str| value.parse::<u32>().expect("fixture number");
        let mut expected = BTreeMap::new();
        let mut owners = BTreeMap::new();
        let mut members = 0;
        let mut reaped = 0;
        for line in truth.lines() {
            let fields: Vec<_> = line.split_whitespace().collect();
            match fields.as_slice() {
                ["root", tid] => {
                    expected.insert(number(tid), vec!["exited with 0".to_owned()]);
                }
                ["member", pid, tid] => {
                    let end = match mode {
                        "term" => "killed by SIGTERM",
                        "kill" => "killed by SIGKILL",
                        "return" => "exited with 0",
                        _ => "exited with 37",
                    };
                    assert!(expected.insert(number(tid), vec![end.to_owned()]).is_none());
                    owners.insert(number(tid), number(pid));
                    members += 1;
                }
                ["wait", pid, status] => {
                    let status = number(status);
                    let end = match status & 0x7f {
                        0 => format!("exited with {}", status >> 8),
                        15 => "killed by SIGTERM".to_owned(),
                        9 => "killed by SIGKILL".to_owned(),
                        signal => panic!("unexpected signal {signal}: {context}"),
                    };
                    assert!(expected.insert(number(pid), vec![end]).is_some());
                    reaped += 1;
                }
                _ => panic!("unexpected fixture output: {context}"),
            }
        }
        assert_eq!(
            (members, reaped),
            ((workers + 1) * rounds, rounds),
            "{context}"
        );
        let ending = Regex::new(r"^(\d+) +\+\+\+ (.+) \+\+\+$").expect("exit regex");
        let mut actual: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        for line in trace.lines() {
            if let Some(found) = ending.captures(line) {
                let tid = number(&found[1]);
                if let Some(&owner) = owners.get(&tid)
                    && owner != tid
                {
                    assert!(
                        !actual.contains_key(&owner),
                        "worker after process exit: {context}"
                    );
                }
                actual.entry(tid).or_default().push(found[2].to_owned());
            }
        }
        assert_eq!(actual, expected, "{context}");
        if mode == "parallel" {
            let terminal = Regex::new(r"^(\d+) +(exit|exit_group)\((\d+)\) += \?$")
                .expect("terminal call regex");
            let mut calls: BTreeMap<u32, Vec<(String, u32)>> = BTreeMap::new();
            for line in trace.lines() {
                if let Some(found) = terminal.captures(line) {
                    calls
                        .entry(number(&found[1]))
                        .or_default()
                        .push((found[2].to_owned(), number(&found[3])));
                }
            }
            let expected_calls: BTreeMap<_, _> = expected
                .keys()
                .map(|&tid| {
                    let call = match owners.get(&tid) {
                        Some(&owner) => ("exit".to_owned(), if owner == tid { 23 } else { 37 }),
                        None => ("exit_group".to_owned(), 0),
                    };
                    (tid, vec![call])
                })
                .collect();

            // A call is lost, and counted, when the kernel cannot take a
            // task storage lock another BPF program holds on its CPU. Every
            // call shown is the right one; those missing are counted lost.
            assert!(
                calls
                    .iter()
                    .all(|(tid, shown)| expected_calls.get(tid) == Some(shown)),
                "{context}"
            );
            let missing = expected_calls
                .keys()
                .filter(|tid| !calls.contains_key(tid))
                .count() as u64;
            assert!(
                missing <= lost,
                "{missing} terminal calls missing, {lost} lost: {context}"
            );
        }
    }

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn group_termination_reports_each_thread_and_one_process_exit() {
        for mode in ["term", "kill", "group", "worker-group", "return"] {
            group_exits(mode, false, 7, 8);
        }
    }
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn group_exit_status_matches_the_status_reaped_by_the_parent() {
        for mode in ["leader-first", "parallel"] {
            group_exits(mode, true, 7, 8);
        }
    }
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn simultaneous_worker_exits_are_not_skipped() {
        for fentry in [false, true] {
            group_exits("parallel", fentry, 63, 100);
        }
    }
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn structured_group_exits_distinguish_threads_from_processes() {
        let directory = ScratchDir::new("syscalls-group-exit-kinds");
        let output = privileged()
            .args(["record", "-o", "trace.usprof", "--"])
            .arg(fixture())
            .args(["group-exits", "parallel", "7", "1"])
            .current_dir(directory.path())
            .output()
            .expect("record group exit kinds");
        let truth = text(&output.stdout);
        let context = format!("{}\n{truth}", text(&output.stderr));
        assert!(output.status.success(), "{context}");
        let number = |value: &str| value.parse::<u32>().expect("fixture number");
        let mut owners = BTreeMap::new();
        for line in truth.lines() {
            let fields: Vec<_> = line.split_whitespace().collect();
            match fields.as_slice() {
                ["root", pid] => {
                    owners.insert(number(pid), number(pid));
                }
                ["member", pid, tid] => {
                    owners.insert(number(tid), number(pid));
                }
                ["wait", _, _] => {}
                _ => panic!("unexpected fixture output: {context}"),
            }
        }
        assert_eq!(owners.len(), 9, "{context}");
        let expected: BTreeMap<_, _> = owners
            .iter()
            .map(|(&tid, &pid)| (tid, vec![if tid == pid { "exit" } else { "thread_exit" }]))
            .collect();
        let mut actual: BTreeMap<u32, Vec<&str>> = BTreeMap::new();
        for event in read(directory.path()).syscalls().events {
            let (tid, pid, kind) = match event.event {
                TraceEvent::Exit { process, .. } => (process.get(), process.get(), "exit"),
                TraceEvent::ThreadExit {
                    thread, process, ..
                } => (thread.get(), process.get(), "thread_exit"),
                _ => continue,
            };
            assert_eq!(Some(pid), owners.get(&tid).copied(), "{context}");
            actual.entry(tid).or_default().push(kind);
        }
        assert_eq!(actual, expected, "{context}");
    }
    /// A leader that exits first leaves a zombie while its worker runs on,
    /// and the recording follows the worker to the process's end.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_zombie_leader_does_not_end_its_running_workers_trace() {
        let directory = ScratchDir::new("syscalls-zombie-leader");
        let mut recording = privileged()
            .args(["record", "-o", "trace.usprof", "--"])
            .arg(fixture())
            .arg("leader-waits")
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run microscope record");
        let mut stdout = BufReader::new(recording.stdout.take().expect("stdout"));
        let mut line = String::new();
        while !line.contains("leader gone") {
            line.clear();
            let read = stdout.read_line(&mut line).expect("read the fixture");
            assert!(read > 0, "the fixture ended before its leader");
        }

        recording
            .stdin
            .take()
            .expect("stdin")
            .write_all(b"x")
            .expect("release the worker");
        let output = recording.wait_with_output().expect("wait for record");
        let code = output.status.code();
        assert!(matches!(code, Some(23 | 37)), "{}", text(&output.stderr));

        let lines = log(directory.path(), &["trace=read,getppid,exit"]);
        assert!(lines.contains("getppid()"), "{lines}");
        assert_eq!(
            lines.matches("+++ exited with 37 +++").count(),
            if code == Some(37) { 2 } else { 1 },
            "{lines}"
        );
        if code == Some(23) {
            assert_eq!(
                lines.matches("+++ exited with 23 +++").count(),
                1,
                "{lines}"
            );
        }
    }
    /// A short filter attaches to its calls' kernel functions, which see the
    /// calls the tracepoints do, and the capture names the hook that ran.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn functions_see_what_tracepoints_see() {
        let scratch = ScratchDir::new("syscalls-hooks");
        // Calls that name no file, which need no descriptors followed.
        let filter = "trace=rt_sigprocmask,clone,clone3,wait4,exit,exit_group";
        let by = |hook: &str, arguments: &[&str]| {
            let directory = below(&scratch, hook);
            assert_eq!(
                recorded(&directory, arguments, &fixture(), &["family"]),
                Some(0)
            );
            assert_eq!(read(&directory).setting("syscall hook"), Some(hook));
            unpathed(&log(&directory, &[filter]))
        };

        let fentry = by("fentry", &["--syscall-filter", filter]);
        let tracepoint = by("tp_btf", &[]);
        assert_same(&comparable(&fentry), &comparable(&tracepoint), "by hook");
    }
    /// A call that never returns is recorded at its entry, and one with no
    /// kernel function of its own falls back to the tracepoints; either
    /// hook counts the same calls.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn noreturn_calls_need_only_entry_and_sigreturn_still_returns() {
        for (filter, mode, wanted) in [
            ("exit_group", "signal", "exit_group(0)"),
            ("exit", "family", "exit(0)"),
            ("read,exit_group", "dumps", "read("),
            ("rt_sigreturn", "signal", "rt_sigreturn("),
        ] {
            let filter = format!("trace={filter}");
            let scratch = ScratchDir::new("syscalls-noreturn");
            for (hook, arguments) in [
                ("filtered", &["--syscall-filter", filter.as_str()][..]),
                ("all", &[]),
            ] {
                let directory = below(&scratch, hook);
                recorded(&directory, arguments, &fixture(), &[mode]);
                let trace = log(&directory, &[&filter]);
                assert!(trace.contains(wanted), "{filter}, {hook}: {trace}");
                if filter.contains("rt_sigreturn") {
                    assert!(!trace.contains("= ?"), "{trace}");
                } else if filter.contains("exit_group") {
                    assert!(trace.contains("exit_group(0) = ?"), "{trace}");
                }
            }

            if filter == "trace=rt_sigreturn" {
                let hooked = read(&scratch.path().join("filtered"));
                assert_eq!(hooked.setting("syscall hook"), Some("tp_btf"));
            }
        }

        for name in ["exit", "exit_group"] {
            let scratch = ScratchDir::new("syscalls-noreturn-counts");
            let filter = format!("trace={name}");
            let counts: Vec<u64> = [&["--syscall-filter", filter.as_str()][..], &[]]
                .into_iter()
                .enumerate()
                .map(|(index, arguments)| {
                    let directory = below(&scratch, &index.to_string());
                    let code = recorded(&directory, arguments, &fixture(), &["family"]);
                    assert_eq!(code, Some(0));
                    counted(&read(&directory), name)
                })
                .collect();
            assert!(counts[0] > 0, "{name}: {counts:?}");
            assert_eq!(counts[0], counts[1], "{name}");
        }
    }
    /// Children and threads, each followed alike. A process that is not
    /// traced is never sent the SIGCHLD its default ignores, which ptrace
    /// makes the kernel queue, so the comparison leaves it out. Joining
    /// the thread waits on a futex, finds it gone with EAGAIN, or finds it
    /// gone before calling at all, as the thread's exit races the join, so
    /// that futex is left out too.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn the_engines_follow_children_and_threads_alike() {
        let (ptrace, bpf) = engines("signal=!SIGCHLD", "family");
        let joined = Regex::new(r"^futex\(.*FUTEX_WAIT_BITSET").expect("regex");
        let threads = |trace: &str| {
            threads(trace)
                .into_iter()
                .map(|lines| {
                    lines
                        .into_iter()
                        .filter(|line| !joined.is_match(line))
                        .collect::<Vec<_>>()
                })
                .collect::<BTreeSet<_>>()
        };

        let (ours, theirs) = (threads(&bpf), threads(&ptrace));
        assert_eq!(
            ours,
            theirs,
            "only BPF:\n{:#?}\nonly ptrace:\n{:#?}",
            ours.difference(&theirs).collect::<Vec<_>>(),
            theirs.difference(&ours).collect::<Vec<_>>()
        );
    }

    /// Each thread's calls named in `names`, in order, with their stacks'
    /// functions and how each stack ends, keyed by the names of the
    /// thread's calls.
    fn stacks(
        profile: &Profile,
        names: &[&str],
    ) -> BTreeMap<Vec<String>, Vec<(Vec<String>, StackEnding)>> {
        let mut threads: BTreeMap<u32, Vec<(String, Vec<String>, StackEnding)>> = BTreeMap::new();
        for event in profile.syscalls().events {
            let (TraceEvent::Call(call), Some(stack)) = (&event.event, event.stack) else {
                continue;
            };
            let name = call.call.display_name().into_owned();
            if names.contains(&name.as_str()) {
                let (functions, ending) = profile.syscall_stack(stack);
                threads
                    .entry(call.thread.get())
                    .or_default()
                    .push((name, functions, ending));
            }
        }

        let mut keyed = BTreeMap::new();
        for calls in threads.into_values() {
            let key: Vec<String> = calls.iter().map(|(name, ..)| name.clone()).collect();
            let stacks = calls
                .into_iter()
                .map(|(_, functions, ending)| (functions, ending))
                .collect();
            assert!(
                keyed.insert(key, stacks).is_none(),
                "two threads made one sequence of calls"
            );
        }

        keyed
    }
    /// A BPF recording holds the calls a ptrace recording finds, each with
    /// its stack, beside its samples.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_recording_holds_its_calls_beside_its_samples() {
        let scratch = ScratchDir::new("syscalls-record");

        // Each call by number, and whether it failed: what two runs share.
        let calls = |directory: &Path| {
            let mut calls = BTreeMap::new();
            for event in read(directory).syscalls().events {
                if let TraceEvent::Call(call) = &event.event {
                    assert!(event.stack.is_some(), "{call:?} has no stack");
                    let failed = call.outcome.raw().is_some_and(|ret| ret < 0);
                    *calls.entry((call.call.number, failed)).or_insert(0) += 1;
                }
            }

            calls
        };

        let filter = ["--syscall-filter", "trace=write,getppid"];
        let ptrace = below(&scratch, "ptrace");
        let ptrace_code = recorded(
            &ptrace,
            &[&["--syscall-engine", "ptrace"][..], &filter].concat(),
            &fixture(),
            &["family"],
        );
        let bpf = below(&scratch, "bpf");
        let bpf_code = recorded(&bpf, &filter, &fixture(), &["family"]);
        assert_eq!(bpf_code, ptrace_code);
        assert_eq!(calls(&bpf), calls(&ptrace));

        let (lines, _) = succeeds(&bpf, &["report", "trace.usprof"]);
        assert!(lines.contains(" samples, "), "{lines}");
        assert!(lines.contains(" system calls, "), "{lines}");
    }
    /// Records `syscall-load` on `threads` threads of `rounds` rounds each
    /// with `arguments`, into `file` in `directory`, and reads it.
    fn record_load(
        directory: &Path,
        file: &str,
        arguments: &[&str],
        threads: u32,
        rounds: u32,
    ) -> microscope::profiler::Profile {
        let program = [threads.to_string(), rounds.to_string()];
        record_fixture(directory, file, arguments, "syscall-load", &program)
    }

    /// Records `fixture` run with `program` with `arguments`, into `file`
    /// in `directory`, and reads it.
    fn record_fixture(
        directory: &Path,
        file: &str,
        arguments: &[&str],
        fixture: &str,
        program: &[String],
    ) -> microscope::profiler::Profile {
        let recorded = Command::new(launcher())
            .env_clear()
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .args(["record", "-o", file])
            .args(arguments)
            .arg("--")
            .arg(Scenario::fixture(fixture))
            .args(program)
            .current_dir(directory)
            .stdin(Stdio::null())
            .output()
            .expect("run microscope record");
        assert!(recorded.status.success(), "{}", text(&recorded.stderr));
        microscope::profiler::Profile::read(&directory.join(file))
            .expect("the capture reads")
            .profile
    }

    /// Each kept call's site, and how many calls it made.
    fn sites(profile: &microscope::profiler::Profile) -> BTreeMap<String, u64> {
        let mut sites = BTreeMap::new();
        for event in profile.syscalls().events {
            if let (microscope::syscalls::TraceEvent::Call(_), Some(stack)) =
                (&event.event, event.stack)
            {
                *sites.entry(profile.syscall_site_name(stack)).or_insert(0) += 1;
            }
        }

        sites
    }

    /// Worker threads make their calls through glibc's cancellable
    /// wrappers, which keep no frame pointer. A recording copies each
    /// call's stack, as it does each sample's, so every call names the
    /// line that made it rather than the wrapper.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_recordings_calls_name_the_lines_that_made_them_on_every_thread() {
        let directory = ScratchDir::new("syscalls-record-sites");
        let profile = record_load(directory.path(), "load.usprof", &[], 4, 300);
        assert_eq!(profile.setting("syscalls"), Some("calls"));
        let sites = sites(&profile);
        for line in [26, 30, 31] {
            assert_eq!(
                sites.get(&format!("one_round · syscall-load.c:{line}")),
                Some(&1200),
                "{sites:#?}"
            );
        }
    }

    /// The filter keeps what its qualifiers say, all of them, while the
    /// kernel still follows every descriptor the kept calls name.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_recording_keeps_only_the_calls_its_filter_selects() {
        let directory = ScratchDir::new("syscalls-record-filter");
        let profile = record_load(
            directory.path(),
            "failed.usprof",
            &["--syscall-filter", "trace=openat status=failed"],
            1,
            50,
        );
        assert_eq!(
            profile.setting("syscall filter"),
            Some("trace=openat status=failed")
        );

        let calls: Vec<_> = profile
            .syscalls()
            .events
            .into_iter()
            .filter_map(|event| match event.event {
                microscope::syscalls::TraceEvent::Call(call) => Some(call),
                _ => None,
            })
            .collect();
        // The loader's searches for libraries fail; the program's own
        // opens of /dev/zero do not.
        assert!(!calls.is_empty());
        assert!(
            calls.iter().all(|call| call.call.name() == Some("openat")
                && call.outcome.raw().is_some_and(|ret| ret < 0)),
            "{calls:#?}"
        );
    }

    /// The production preset counts every call in the kernel and keeps
    /// none that succeeded quickly: the same counts a recording of every
    /// call makes of the same program.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_production_recording_counts_every_call_and_keeps_none() {
        let directory = ScratchDir::new("syscalls-record-counts");
        let counted = record_load(
            directory.path(),
            "prod.usprof",
            &["--preset", "prod"],
            2,
            100,
        );
        let streamed = record_load(directory.path(), "dev.usprof", &[], 2, 100);
        assert_eq!(counted.setting("syscalls"), Some("counts"));
        assert!(
            !sites(&counted)
                .keys()
                .any(|site| site.contains("syscall-load.c")),
            "{:#?}",
            sites(&counted)
        );

        let per_call = |profile: &microscope::profiler::Profile| -> BTreeMap<String, u64> {
            profile
                .syscalls()
                .stats()
                .per_call()
                .into_iter()
                .filter(|(call, _)| matches!(call.name(), Some("openat" | "read" | "close")))
                .map(|(call, stats)| (call.name().unwrap_or("?").to_owned(), stats.calls))
                .collect()
        };
        let counts = per_call(&counted);
        assert!(counts["read"] >= 200, "{counts:?}");
        assert_eq!(counts, per_call(&streamed));
    }

    /// Counting, the kernel keeps each thread's calls apart: each worker
    /// made its rounds' reads, and the threads' counts add up to the
    /// process's.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_counting_recording_counts_each_threads_calls() {
        let directory = ScratchDir::new("syscalls-record-threads");
        let profile = record_load(
            directory.path(),
            "prod.usprof",
            &["--preset", "prod"],
            3,
            100,
        );
        let syscalls = profile.syscalls();
        let reads: BTreeMap<_, u64> = syscalls
            .counted
            .by_call
            .iter()
            .filter(|((_, _, call), _)| call.name() == Some("read"))
            .map(|(&(process, thread, _), stats)| ((process, thread), stats.calls))
            .collect();

        let processes: BTreeSet<_> = reads.keys().map(|&(process, _)| process).collect();
        assert_eq!(processes.len(), 1, "{reads:?}");
        let process = processes.into_iter().next().expect("one process");
        let workers: Vec<u64> = reads
            .iter()
            .filter(|&(&(_, thread), _)| thread.get() != process.get())
            .map(|(_, &count)| count)
            .collect();
        assert_eq!(workers, [100, 100], "{reads:?}");
        assert!(
            reads
                .get(&(process, microscope::syscalls::Tid::new(process.get())))
                .is_some_and(|&count| count >= 100),
            "{reads:?}"
        );

        let total = syscalls.stats().per_call();
        let read = total
            .iter()
            .find(|(call, _)| call.name() == Some("read"))
            .map(|(_, stats)| stats.calls);
        assert_eq!(read, Some(reads.values().sum()));
    }

    /// Over its budget, a recording gives up system call detail a rung a
    /// period, in order, before it gives up samples; once it keeps only
    /// the calls that failed or were slow, the kernel counts the rest.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn over_budget_a_recording_sheds_calls_before_samples() {
        let directory = ScratchDir::new("syscalls-record-ladder");
        let program = ["4", "70000", "25000"].map(str::to_owned);
        let profile = record_fixture(
            directory.path(),
            "prod.usprof",
            &["--preset", "prod", "--syscalls", "calls"],
            "syscall-load",
            &program,
        );

        let changes = profile.budget_changes();
        let gave_up: Vec<&str> = changes
            .iter()
            .filter_map(|(_, text)| text.strip_prefix("gave up "))
            .collect();
        let rungs = [
            "system call stacks",
            "15 of every 16 system calls",
            "every system call that neither failed nor was slow",
        ];
        assert!(gave_up.len() >= rungs.len(), "{changes:#?}");
        for (text, rung) in gave_up.iter().zip(rungs) {
            assert!(text.starts_with(rung), "{changes:#?}");
        }

        // Past the last rung, only kept calls stream.
        let (shed_at, _) = changes
            .iter()
            .find(|(_, text)| text.starts_with("gave up every system call"))
            .expect("the last rung");
        let late: Vec<_> = profile
            .syscalls()
            .events
            .into_iter()
            .filter_map(|event| match event.event {
                microscope::syscalls::TraceEvent::Call(call)
                    if call.time() > shed_at.cast_unsigned() + 500_000_000 =>
                {
                    Some(call)
                }
                _ => None,
            })
            .filter(|call| matches!(call.call.name(), Some("openat" | "read" | "close")))
            .collect();
        assert!(
            late.iter().all(|call| call.flags.kept()),
            "{:?}",
            late.first()
        );

        // Shedding never sheds counts, nor does a full ring: each of the
        // 280k rounds' closes is counted, with the loader's few.
        let counts = profile.syscalls().stats().per_call();
        let closes = counts
            .iter()
            .find(|(call, _)| call.name() == Some("close"))
            .map_or(0, |(_, stats)| stats.calls);
        assert!((280_000..280_020).contains(&closes), "{closes}");
    }

    /// On the load where the old `--budget 1` cost 37% of a CPU, 100k
    /// calls a second, `--preset prod` settles near 1% once its ladder has
    /// shed what it must. Debug builds poll at a fixed cost of about 0.6%,
    /// so this allows 2%.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_production_recording_holds_its_budget_under_a_busy_load() {
        let directory = ScratchDir::new("syscalls-record-budget");
        let mut recording = Command::new(launcher())
            .env_clear()
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .args([
                "record",
                "-o",
                "prod.usprof",
                "--preset",
                "prod",
                "--syscalls",
                "calls",
                "--",
            ])
            .arg(Scenario::fixture("syscall-load"))
            .args(["4", "133000", "25000"])
            .current_dir(directory.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run microscope record");

        // The launcher runs microscope in its own process: its CPU time,
        // in clock ticks, by the monotonic clock the capture's times use.
        let pid = recording.id();
        let mut used = Vec::new();
        while recording.try_wait().expect("microscope runs").is_none() {
            let now = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC)
                .expect("a monotonic clock");
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            let ticks: Option<u64> = stat.rsplit_once(") ").map(|(_, fields)| {
                fields
                    .split(' ')
                    .skip(11)
                    .take(2)
                    .map(|field| field.parse::<u64>().unwrap_or(0))
                    .sum()
            });
            if let Some(ticks) = ticks {
                used.push((now.tv_sec() * 1_000_000_000 + now.tv_nsec(), ticks));
            }
            std::thread::sleep(Duration::from_millis(250));
        }

        let output = recording.wait_with_output().expect("microscope ends");
        assert!(output.status.success(), "{}", text(&output.stderr));
        let profile = microscope::profiler::Profile::read(&directory.path().join("prod.usprof"))
            .expect("the capture reads")
            .profile;
        let changes = profile.budget_changes();

        // From 2 s after the ladder's last step down to 1 s before the
        // program ended, which leaves out writing the capture.
        let settled = changes
            .iter()
            .filter(|(_, text)| text.starts_with("gave up"))
            .map(|(time, _)| time + 2_000_000_000)
            .max()
            .expect("the ladder gave something up");
        let ended = used.last().map_or(0, |(time, _)| time - 1_000_000_000);
        let window: Vec<_> = used
            .iter()
            .filter(|(time, _)| (settled..=ended).contains(time))
            .collect();
        let (Some((began, before)), Some((until, after))) = (window.first(), window.last()) else {
            panic!("no time left after the ladder settled: {changes:#?}");
        };
        assert!(until - began >= 3_000_000_000, "{changes:#?}");

        #[expect(clippy::cast_precision_loss, reason = "a few hundred ticks")]
        let share = (after - before) as f64 / 100.0 / ((until - began) as f64 / 1e9);
        assert!(
            share < 0.02,
            "microscope used {:.2}% of a CPU: {changes:#?}",
            share * 100.0
        );
    }

    /// How many calls named `name` the profile counted.
    fn counted(profile: &microscope::profiler::Profile, name: &str) -> u64 {
        profile
            .syscalls()
            .stats()
            .per_call()
            .iter()
            .find(|(call, _)| call.name() == Some(name))
            .map_or(0, |(_, stats)| stats.calls)
    }

    /// A recording attached to a running process traces its calls from
    /// then on, each with its site, beside its samples.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_recording_attached_to_a_process_holds_its_calls() {
        let directory = ScratchDir::new("syscalls-record-pid");
        let mut program = Command::new(fixture())
            .arg("spin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start the fixture");
        let mut recording = Command::new(launcher())
            .env_clear()
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .args([
                "record",
                "--syscalls",
                "calls",
                "-o",
                "attached.usprof",
                "--pid",
            ])
            .arg(program.id().to_string())
            .current_dir(directory.path())
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run microscope record");

        // Once the recording says it began, the program's calls are traced.
        let mut said = BufReader::new(recording.stderr.take().expect("stderr")).lines();
        let begun = said
            .by_ref()
            .map_while(Result::ok)
            .find(|line| line.starts_with("microscope: recording process "));
        assert!(begun.is_some(), "the recording never began");
        std::thread::sleep(Duration::from_millis(200));
        program
            .stdin
            .take()
            .expect("stdin")
            .write_all(b"x")
            .expect("end the spin");
        let output = program.wait_with_output().expect("the fixture ends");
        assert!(output.status.success());
        let asked: u64 = text(&output.stdout)
            .trim()
            .strip_prefix("asked ")
            .and_then(|count| count.parse().ok())
            .expect("a count");
        let rest: Vec<String> = said.map_while(Result::ok).collect();
        assert!(
            recording.wait().expect("microscope ends").success(),
            "{rest:?}"
        );

        let profile =
            microscope::profiler::Profile::read(&directory.path().join("attached.usprof"))
                .expect("the capture reads")
                .profile;
        let traced = counted(&profile, "getppid");
        assert!((1..=asked).contains(&traced), "{traced} of {asked}");
        assert!(profile.samples() > 0);
        let sites = sites(&profile);
        assert!(sites.keys().any(|site| site.contains("spin")), "{sites:#?}");
    }

    /// A debugging session's `profile start` records its program's calls
    /// from the moment it returns: every round the program runs after is
    /// counted.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_debugging_session_profiles_its_programs_calls() {
        let directory = ScratchDir::new("syscalls-session");
        let file = directory.path().join("session.usprof");
        let session = Command::new(launcher())
            .env_clear()
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .arg("--batch")
            .args([
                "--eval",
                "break work",
                "--eval",
                "run",
                "--eval",
                "profile start",
            ])
            .args([
                "--eval",
                "delete 1",
                "--eval",
                "continue",
                "--eval",
                "profile stop",
            ])
            .arg("--eval")
            .arg(format!("profile save {}", file.display()))
            .arg(Scenario::fixture("syscall-load"))
            .args(["--", "1", "2000", "0"])
            .current_dir(directory.path())
            .stdin(Stdio::null())
            .output()
            .expect("run a microscope session");
        let stdout = text(&session.stdout);
        assert!(
            session.status.success(),
            "{stdout}{}",
            text(&session.stderr)
        );
        assert!(stdout.contains("Profiling process "), "{stdout}");

        let profile = microscope::profiler::Profile::read(&file)
            .expect("the capture reads")
            .profile;
        assert_eq!(counted(&profile, "openat"), 2000);
        assert_eq!(counted(&profile, "read"), 2000);
        let sites = sites(&profile);
        assert!(
            sites.keys().any(|site| site.contains("one_round")),
            "{sites:#?}"
        );
    }

    /// Each call a counting recording kept, by name, why it was kept, and
    /// its site.
    fn kept(profile: &microscope::profiler::Profile) -> BTreeMap<(String, &str, String), u64> {
        use microscope::syscalls::{EventFlags, Outcome, TraceEvent};

        let mut kept = BTreeMap::new();
        for event in profile.syscalls().events {
            let TraceEvent::Call(call) = &event.event else {
                continue;
            };

            let failed = call.flags.contains(EventFlags::KEPT_FAILED);
            let slow = call.flags.contains(EventFlags::KEPT_SLOW);
            assert!(failed != slow, "{call:?}");
            if failed {
                assert!(matches!(call.outcome, Outcome::Failed(_)), "{call:?}");
            } else {
                assert!(
                    call.duration().is_some_and(|ns| ns >= 10_000_000),
                    "{call:?}"
                );
            }

            let site = event.stack.map_or_else(
                || "no stack".to_owned(),
                |row| profile.syscall_site_name(row),
            );
            let why = if failed { "failed" } else { "slow" };
            let name = call.call.name().unwrap_or("?").to_owned();
            *kept.entry((name, why, site)).or_insert(0) += 1;
        }

        kept
    }

    /// Counting, a recording keeps whole each call that failed or took
    /// longer than the threshold, with the line that made it, and counts
    /// every call, the kept ones once.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_counting_recording_keeps_the_calls_that_failed_or_were_slow() {
        let directory = ScratchDir::new("syscalls-record-exceptions");
        let program = ["1000", "5", "3"].map(str::to_owned);
        let profile = record_fixture(
            directory.path(),
            "prod.usprof",
            &["--preset", "prod"],
            "syscall-exceptions",
            &program,
        );
        assert_eq!(profile.setting("syscall slow"), Some("10ms"));

        let kept = kept(&profile);
        // Each function is named for why its calls are kept.
        let site = |name: &str, why: &'static str, line: u32| {
            kept.get(&(
                name.to_owned(),
                why,
                format!("{why} · syscall-exceptions.c:{line}"),
            ))
        };
        assert_eq!(site("close", "failed", 12), Some(&5), "{kept:#?}");
        assert_eq!(site("clock_nanosleep", "slow", 16), Some(&3), "{kept:#?}");
        assert!(
            !kept.keys().any(|(name, ..)| name == "getppid"),
            "{kept:#?}"
        );

        let counts = profile.syscalls().stats().per_call();
        let count = |name: &str| {
            counts
                .iter()
                .find(|(call, _)| call.name() == Some(name))
                .map(|(_, stats)| (stats.calls, stats.errors))
        };
        assert_eq!(count("getppid"), Some((1000, 0)));
        assert_eq!(count("close").map(|(_, errors)| errors), Some(5));
        assert_eq!(count("clock_nanosleep"), Some((3, 0)));
        assert_eq!(profile.syscalls_lost().capped, 0);
    }

    /// A burst of failures past the cap keeps at most what every CPU's
    /// bucket holds, and counts the rest as capped: kept and capped add up
    /// to every failure.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_counting_recording_caps_the_calls_it_keeps() {
        let directory = ScratchDir::new("syscalls-record-cap");
        let program = ["0", "20000", "0"].map(str::to_owned);
        let profile = record_fixture(
            directory.path(),
            "prod.usprof",
            &["--preset", "prod"],
            "syscall-exceptions",
            &program,
        );
        assert_eq!(profile.setting("syscall cap"), Some("2000/s"));

        let kept: u64 = kept(&profile).values().sum();
        let lost = profile.syscalls_lost();
        let failed: u64 = profile
            .syscalls()
            .stats()
            .per_call()
            .values()
            .map(|stats| stats.errors)
            .sum();
        // A record the kernel had no room for, or a run of the programs it
        // skipped on a busy machine, is counted dropped.
        assert!(failed >= 20_000, "{failed}");
        assert!(kept + lost.capped <= failed, "{kept} kept, {lost:?}");
        assert!(
            kept + lost.capped + lost.dropped >= failed,
            "{kept} kept, {lost:?}"
        );
        assert!((1..=2_500).contains(&kept), "{kept} kept, {lost:?}");
        assert_eq!(lost.unseen, 0);
    }

    /// Copied and unwound by call-frame information, the BPF engine's
    /// stacks are the ptrace engine's, frame for frame. Walked by frame
    /// pointers alone, each is the start of the ptrace engine's, and says
    /// why it ends where it is shorter: never a frame the other lacks.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn the_engines_unwind_stacks_alike() {
        let names = ["write", "read", "readv", "getppid"];
        let filter = "trace=write,read,readv,getppid";
        for mode in ["dumps", "family", "signal"] {
            let scratch = ScratchDir::new("syscalls-stacks");
            let by = |name: &str, arguments: &[&str]| {
                let directory = below(&scratch, name);
                let arguments = [arguments, &["--syscall-filter", filter]].concat();
                recorded(&directory, &arguments, &fixture(), &[mode]);
                stacks(&read(&directory), &names)
            };

            let whole = by("ptrace", &["--syscall-engine", "ptrace"]);
            assert!(
                whole
                    .values()
                    .flatten()
                    .any(|(functions, _)| !functions.is_empty()),
                "{whole:?}"
            );

            let copied = by("copied", &[]);
            assert_eq!(copied, whole, "copied in {mode}");

            let walked = by("walked", &["--stack-bytes", "0"]);
            assert_eq!(
                walked.keys().collect::<Vec<_>>(),
                whole.keys().collect::<Vec<_>>(),
                "walked in {mode}"
            );
            for (thread, calls) in &walked {
                for ((frames, ending), (whole_frames, _)) in calls.iter().zip(&whole[thread]) {
                    assert!(
                        whole_frames.starts_with(frames)
                            && (frames.len() == whole_frames.len()
                                || matches!(ending, StackEnding::Truncated(_))),
                        "walked in {mode}, {thread:?}:\n{frames:?} {ending:?}\n---\n{whole_frames:?}"
                    );
                }
            }
        }
    }
    /// The getppid calls of `mode` in `program`, recorded with
    /// `arguments`: each stack's functions and how it ends.
    fn getppid_stacks(
        program: &Path,
        mode: &str,
        arguments: &[&str],
        name: &str,
    ) -> Vec<(Vec<String>, StackEnding)> {
        let directory = ScratchDir::new("syscalls-frame-pointers");
        let filter = format!("trace={name}");
        let arguments = [arguments, &["--syscall-filter", &filter]].concat();
        recorded(directory.path(), &arguments, program, &[mode]);
        let profile = read(directory.path());
        profile
            .syscalls()
            .events
            .iter()
            .filter_map(|event| match (&event.event, event.stack) {
                (TraceEvent::Call(call), Some(stack)) if call.call.display_name() == name => {
                    Some(profile.syscall_stack(stack))
                }
                _ => None,
            })
            .collect()
    }

    /// Walked by frame pointers alone, the stacks of a program that keeps
    /// none end where the chain breaks, and say so; copied, they unwind to
    /// their thread's first frame.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn frame_pointer_breaks_end_stacks_that_a_copy_unwinds() {
        let broken = |ending: &StackEnding| matches!(ending, StackEnding::Truncated(why) if why.contains("frame pointer"));
        let program = Scenario::fixture("syscalls-rust-release");
        for (mode, count, outer) in [
            ("stacks", 32, "syscalls::main"),
            ("thread-stacks", 64, "start_thread"),
        ] {
            let walked = getppid_stacks(&program, mode, &["--stack-bytes", "0"], "getppid");
            assert_eq!(walked.len(), count, "{walked:?}");
            assert!(
                walked.iter().all(|(_, ending)| broken(ending)),
                "{walked:?}"
            );

            let copied = getppid_stacks(&program, mode, &[], "getppid");
            assert_eq!(copied.len(), count, "{copied:?}");
            assert!(
                copied.iter().all(|(_, ending)| !broken(ending)),
                "{copied:?}"
            );
            assert!(
                copied.iter().all(|(functions, _)| {
                    functions
                        .first()
                        .is_some_and(|name| name == "syscalls::parent")
                        && functions.iter().any(|name| name == outer)
                }),
                "{copied:?}"
            );
        }

        let walked = getppid_stacks(&fixture(), "dumps", &["--stack-bytes", "0"], "read");
        assert!(
            walked.iter().any(|(_, ending)| broken(ending)),
            "{walked:?}"
        );
    }
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn stack_copy_exhaustion_is_not_a_frame_pointer_break() {
        let stacks = getppid_stacks(
            &Scenario::fixture("syscalls-rust-release"),
            "deep-stack",
            &["--stack-bytes", "8192"],
            "getppid",
        );
        let why = |ending: &StackEnding| match ending {
            StackEnding::Truncated(why) => why.clone(),
            StackEnding::Complete => String::new(),
        };
        assert!(
            stacks
                .iter()
                .any(|(_, ending)| why(ending).contains("stack copy exhausted")),
            "{stacks:?}"
        );
        assert!(
            stacks
                .iter()
                .all(|(_, ending)| !why(ending).contains("frame pointer")),
            "{stacks:?}"
        );
    }
    /// The kernel counts at `--syscalls counts` what the ptrace engine
    /// counts from every call it sees.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn the_kernel_counts_calls_as_the_ptrace_engine_does() {
        let counts = |directory: &Path| -> Vec<(String, u64, u64)> {
            read(directory)
                .syscalls()
                .stats()
                .per_call()
                .iter()
                .map(|(call, stats)| (call.display_name().into_owned(), stats.calls, stats.errors))
                .collect()
        };

        for mode in ["files", "family"] {
            let scratch = ScratchDir::new("syscalls-counts");
            let ptrace = below(&scratch, "ptrace");
            let bpf = below(&scratch, "bpf");
            let ptrace_code = recorded(
                &ptrace,
                &["--syscall-engine", "ptrace"],
                &fixture(),
                &[mode],
            );
            let bpf_code = recorded(&bpf, &["--syscalls", "counts"], &fixture(), &[mode]);
            assert_eq!(bpf_code, ptrace_code);
            assert_eq!(read(&bpf).setting("syscalls"), Some("counts"));
            let theirs = counts(&ptrace);
            assert!(!theirs.is_empty());
            assert_eq!(counts(&bpf), theirs, "in {mode}");
        }
    }
    /// Counted continuously, every segment holds its own span's calls: the
    /// kernel counts into one half of its statistics while microscope reads
    /// and clears the other, so the segments together count every call.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn continuous_counts_split_into_segments_that_sum_to_every_call() {
        let directory = ScratchDir::new("syscalls-continuous");
        let segments = directory.path().join("segments");

        // Made here, so that the test can clean up what a launcher running
        // as another user writes in it.
        std::fs::create_dir(&segments).expect("make the directory");
        let mut recording = Command::new(launcher())
            .env_clear()
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .args([
                "record",
                "--syscalls",
                "counts",
                "--syscall-filter",
                "trace=getppid",
            ])
            .args(["--continuous", "segments", "--segment", "200ms", "--"])
            .arg(fixture())
            .arg("spin")
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run microscope record");

        let written = || {
            std::fs::read_dir(&segments).map_or(0, |entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "usprof"))
                    .count()
            })
        };

        // Polls for files the recording writes and says nothing of.
        let end = Instant::now() + DEADLINE;
        while written() < 3 {
            assert!(Instant::now() < end, "only {} segments", written());
            std::thread::sleep(Duration::from_millis(10));
        }

        recording
            .stdin
            .take()
            .expect("stdin")
            .write_all(b"x")
            .expect("end the spin");
        let output = recording.wait_with_output().expect("microscope ends");
        assert!(output.status.success(), "{}", text(&output.stderr));
        let asked = text(&output.stdout);
        let asked = asked
            .trim()
            .strip_prefix("asked ")
            .unwrap_or_else(|| panic!("a count: {asked}"));

        let report = Command::new(env!("CARGO_BIN_EXE_microscope"))
            .arg("report")
            .arg(&segments)
            .output()
            .expect("run microscope report");
        let table = text(&report.stdout);
        assert!(
            table
                .lines()
                .any(|line| line.split_whitespace().last() == Some("getppid")
                    && line.split_whitespace().any(|cell| cell == asked)),
            "{asked} calls: {table}"
        );
    }

    /// Runs `record` with `arguments` on `syscall-exceptions` with
    /// `program`, keeping a window into `window.usprof`, and returns what it
    /// said and the window a call saved.
    fn saved_by_a_call(
        directory: &Path,
        arguments: &[&str],
        program: &[&str],
    ) -> (String, microscope::profiler::Profile) {
        let recorded = Command::new(launcher())
            .env_clear()
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .args(["record", "--window", "10s", "-o", "window.usprof"])
            .args(arguments)
            .arg("--")
            .arg(Scenario::fixture("syscall-exceptions"))
            .args(program)
            .current_dir(directory)
            .stdin(Stdio::null())
            .output()
            .expect("run microscope record");
        let said = text(&recorded.stderr);
        assert!(recorded.status.success(), "{said}");

        // One call saves; the rest come within the window's span after it.
        assert_eq!(said.matches("saved ").count(), 1, "{said}");
        let saved = microscope::profiler::Profile::read(&directory.join("window-1.usprof"))
            .unwrap_or_else(|error| panic!("{error}: {said}"))
            .profile;
        (said, saved)
    }

    /// A call that fails as `--save-on-errno` names saves the window,
    /// which holds it, and says why.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_failing_call_saves_the_window_that_holds_it() {
        let directory = ScratchDir::new("syscalls-save-on-errno");
        let (said, saved) = saved_by_a_call(
            directory.path(),
            &["--syscalls", "calls", "--save-on-errno", "EBADF"],
            &["100", "3", "10"],
        );
        assert!(
            said.contains("microscope: close failed with errno 9: saved "),
            "{said}"
        );
        assert_eq!(saved.setting("trigger"), Some("close failed with errno 9"));
        assert!(counted(&saved, "close") >= 1);
        assert_eq!(counted(&saved, "getppid"), 100);
    }

    /// A counting recording keeps the slow call that saves its window.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_slow_call_saves_the_window_that_holds_it() {
        let directory = ScratchDir::new("syscalls-save-on-slow");
        let (said, saved) = saved_by_a_call(
            directory.path(),
            &["--preset", "prod", "--save-on-slow", "25ms"],
            &["100", "0", "10"],
        );
        assert!(said.contains("microscope: clock_nanosleep took "), "{said}");
        let reason = saved.setting("trigger").unwrap_or_default();
        assert!(reason.starts_with("clock_nanosleep took "), "{reason}");

        let kept = kept(&saved);
        assert!(
            kept.keys()
                .any(|(name, why, _)| name == "clock_nanosleep" && *why == "slow"),
            "{kept:#?}"
        );
    }

    /// A continuous recording cuts its calls with its samples: each
    /// segment holds the calls of its own span.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_continuous_recording_cuts_its_calls_into_segments() {
        let directory = ScratchDir::new("syscalls-record-continuous");
        let segments = directory.path().join("segments");
        std::fs::create_dir(&segments).expect("make the directory");

        let mut recording = Command::new(launcher())
            .env_clear()
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .args([
                "record",
                "--syscall-filter",
                "trace=read",
                "--continuous",
                "segments",
                "--segment",
                "0.2",
                "--",
            ])
            .arg(fixture())
            .arg("block")
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run microscope record");

        let mut stderr = recording.stderr.take().expect("stderr");
        let mut stdout = BufReader::new(recording.stdout.take().expect("stdout"));
        let mut ready = String::new();
        stdout.read_line(&mut ready).expect("ready");

        let written = || {
            std::fs::read_dir(&segments).map_or(0, |entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "usprof"))
                    .count()
            })
        };

        let mut input = recording.stdin.take().expect("stdin");

        for _ in 0..3 {
            if let Err(error) = input.write_all(b"x") {
                let mut said = String::new();
                let _ = stderr.read_to_string(&mut said);
                panic!("a byte: {error}: the recording ended: {said}");
            }

            input.flush().expect("sent");

            // Polls for a file the recording writes and says nothing of.
            let (before, end) = (written(), Instant::now() + DEADLINE);
            while written() <= before {
                assert!(Instant::now() < end, "no segment after {before}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        drop(input);
        assert!(recording.wait().expect("wait").success());

        // Each segment's reads of a byte, and what it lost.
        let mut found = Vec::new();

        for entry in std::fs::read_dir(&segments)
            .expect("the segments")
            .flatten()
        {
            let profile = microscope::profiler::Profile::read(&entry.path())
                .expect("a segment reads")
                .profile;
            let syscalls = profile.syscalls();
            let byte = syscalls
                .events
                .iter()
                .filter(|event| {
                    matches!(&event.event, microscope::syscalls::TraceEvent::Call(call)
                        if call.call.name() == Some("read")
                            && call.outcome == microscope::syscalls::Outcome::Returned(1)
                            && call.args[0] == 0)
                })
                .count();
            found.push((byte, syscalls.lost.dropped + syscalls.lost.unseen));
        }

        // A read whose return went unseen, as when another tracer attached
        // to its function while it was blocked, is counted lost.
        let read: usize = found.iter().map(|&(byte, _)| byte).sum();
        let lost: u64 = found.iter().map(|&(_, lost)| lost).sum();
        let holding = found.iter().filter(|&&(byte, _)| byte > 0).count();
        assert!(read <= 3 && read as u64 + lost >= 3, "{found:?}");
        assert!(holding >= 2 || lost > 0, "{found:?}");
    }

    /// A burst of calls that each copy 8 KiB of their stack fits in the
    /// ring, however little of it the reader drains meanwhile: every call
    /// is kept, each with its stack.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_burst_of_copied_stacks_loses_no_calls() {
        // 690 calls in about a millisecond on one CPU, whose records take
        // some 5.9 MB: more than twice what a 2 MiB ring holds, and less
        // than 8 MiB.
        let rounds = 230;
        let directory = ScratchDir::new("syscalls-burst");
        let profile = record_fixture(
            directory.path(),
            "trace.usprof",
            &["--stack-bytes", "8192"],
            "syscall-load",
            &["1".to_owned(), rounds.to_string()],
        );
        assert_eq!(profile.setting("syscall hook"), Some("tp_btf"));

        let calls: Vec<Option<u32>> = profile
            .syscalls()
            .events
            .iter()
            .filter(|event| matches!(event.event, TraceEvent::Call(_)))
            .map(|event| event.stack)
            .collect();
        // A call the process exited inside, as exit_group, ends after its
        // memory, and its stack, are gone.
        let bare: Vec<String> = profile
            .syscalls()
            .events
            .iter()
            .filter_map(|event| match &event.event {
                TraceEvent::Call(call)
                    if event.stack.is_none() && call.outcome != Outcome::ProcessExited =>
                {
                    Some(format!("{call:?}"))
                }
                _ => None,
            })
            .collect();
        assert!(bare.is_empty(), "calls without stacks: {bare:#?}");

        // The fixture's calls, and not the loader's, are made in one_round.
        let shown = calls
            .iter()
            .flatten()
            .filter(|&&stack| {
                profile
                    .syscall_stack(stack)
                    .0
                    .iter()
                    .any(|function| function == "one_round")
            })
            .count();
        let opens = unpathed(&log(directory.path(), &["trace=openat"]))
            .matches(r#"openat(AT_FDCWD, "/dev/zero""#)
            .count();
        assert_eq!(
            (shown, opens),
            (rounds * 3, rounds),
            "{:?}",
            profile.syscalls().lost
        );
    }
    /// A recording attached to a process never stops it: the process runs
    /// on, traced, to its own end, which ends the recording.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn an_attached_process_runs_on_traced_to_its_end() {
        let directory = ScratchDir::new("syscalls-attached");
        let mut target = ExternalProcess::spawn(&Scenario::fixture("attach"));
        let pid = target.process_id().get().to_string();
        let mut running = Running::spawn(
            privileged()
                .args(["record", "-o", "trace.usprof", "--pid", &pid])
                .current_dir(directory.path()),
        );
        running.line(|line| line.contains(&format!("recording process {pid}")));
        target.release();
        let (_, lines) = running.finish();
        target.reaped();

        // The read began before the recording did.
        let trace = unpathed(&log(directory.path(), &["trace=read,exit_group"]));
        assert!(
            trace.contains(r#"read(0, "x", 1) = 1 /* entry unseen */"#),
            "{trace}\n{lines:?}"
        );
        assert!(trace.contains("exit_group(23) = ?"), "{trace}");
        assert!(trace.ends_with("+++ exited with 23 +++"), "{trace}");
    }
}

/// Each thread and child is announced, as what it is, before anything it
/// did: its first stop can reach the tracer before the event of the call
/// that made it, and a short life can end before that event is handled.
#[test]
fn threads_and_children_are_announced_before_what_they_do() {
    use microscope::syscalls::TraceEvent;

    let scratch = ScratchDir::new("syscalls-churn");
    let count = 300;
    let (code, said) = record(
        scratch.path(),
        &[],
        &fixture(),
        &["churn", &count.to_string()],
    );
    assert_eq!(code, Some(0), "{said}");
    let events = read(scratch.path()).syscalls().events;
    let mut announced = BTreeMap::new();
    let mut problems = Vec::new();

    for recorded in &events {
        match &recorded.event {
            TraceEvent::Spawn { child, thread, .. } => {
                if announced.insert(*child, *thread).is_some() {
                    problems.push(format!("{child} announced twice"));
                }
            }

            TraceEvent::Exit { process, .. }
                if *process != events_first(&events)
                    && !announced.contains_key(&process.leader()) =>
            {
                problems.push(format!("{process} exited before it was announced"));
            }

            TraceEvent::Call(call)
                if call.thread != call.process.leader()
                    && announced.get(&call.thread) != Some(&true) =>
            {
                problems.push(format!(
                    "thread {} called before it was announced",
                    call.thread
                ));
            }
            _ => {}
        }
    }

    let threads = announced.values().filter(|thread| **thread).count();
    assert_eq!(
        (threads, announced.len() - threads),
        (count, count),
        "{problems:?}"
    );

    assert!(problems.is_empty(), "{problems:?}");
}
/// The first process the trace saw: the one launched.
fn events_first(events: &[microscope::profiler::RecordedEvent]) -> microscope::syscalls::Pid {
    events
        .iter()
        .find_map(|recorded| match &recorded.event {
            microscope::syscalls::TraceEvent::Call(call) => Some(call.process),
            _ => None,
        })
        .expect("a call")
}
