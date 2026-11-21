mod support;

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use regex::Regex;
use support::{ExternalProcess, Scenario, ScratchDir};

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

fn untimed(line: &str) -> Option<&str> {
    let (time, rest) = line.trim_start().split_once(' ')?;
    time.parse::<f64>().ok()?;
    Some(rest.trim_start())
}

fn log(directory: &Path, arguments: &[&str]) -> String {
    let duration = Regex::new(r" <[\d.]+ (ns|µs|ms|s)>$").expect("regex");
    let (log, _) = succeeds(
        directory,
        &[&["report", "trace.usprof", "--syscalls"][..], arguments].concat(),
    );
    // What was lost is read from the profile; the report's notes about it are
    // not calls, and a count in front of a note reads like a time.
    let note = Regex::new(
        r"^(--- lost \d+ events ---|records the kernel had no room|calls never returned where|calls were over the cap|calls came while the kernel's ring)",
    )
    .expect("regex");
    log.lines()
        .filter_map(untimed)
        .filter(|line| !note.is_match(line))
        .map(|line| duration.replace(line, "").into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

fn unpathed(text: &str) -> String {
    Regex::new(r"\b(\d+|AT_FDCWD)<(?:[^<>]|<[^<>]*>)*>")
        .expect("regex")
        .replace_all(text, "$1")
        .into_owned()
}

fn unthreaded(line: &str) -> &str {
    line.split_once(' ').map_or(line, |(_, rest)| rest)
}

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

type SiteRow = (String, f64, u64, u64);

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

    assert_eq!(files, [(3, 1)], "{all}");
}

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

            std::thread::sleep(Duration::from_millis(10));
        }
    }

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
    let busiest = Regex::new(
        r"most time:  (execve [\d.]+ ms in 1 call, )?(clock_)?nanosleep 3\d\.\d{3} ms in 1 call, ",
    )
    .expect("regex");
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

mod bpf {
    use std::collections::BTreeSet;
    use std::io::{Read as _, Write as _};

    use microscope::profiler::{Profile, StackEnding};
    use microscope::syscalls::{EventFlags, Outcome, ProcessEnd, SyscallEvent, TraceEvent};

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

    fn privileged() -> Command {
        let mut command = Command::new(launcher());
        command
            .env_clear()
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .stdin(Stdio::null());
        command
    }

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

    fn below(scratch: &ScratchDir, name: &str) -> PathBuf {
        let directory = scratch.path().join(name);
        std::fs::create_dir(&directory).expect("make a directory");
        directory
    }
    fn comparable(trace: &str) -> String {
        let ids = Regex::new(r"\b(st_uid|st_gid|si_uid)=\d+").expect("regex");
        normalized(&ids.replace_all(trace, "$1=ID"))
    }
    fn engines(filter: &str, mode: &str) -> (String, String) {
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
    fn threads(trace: &str) -> BTreeSet<Vec<String>> {
        // A result is aligned past the thread id, so its padding depends on how
        // wide the id is; it goes with the id.
        let padding = Regex::new(r"\) +=").expect("regex");
        let mut threads: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for line in trace.lines() {
            let (thread, rest) = line.split_once(' ').unwrap_or(("", line));
            let rest = padding.replace(rest, ") =");
            threads.entry(thread).or_default().push(comparable(&rest));
        }

        threads.into_values().collect()
    }

    #[test]
    fn threads_compare_alike_whatever_the_width_of_their_ids() {
        let trace = |tid: &str, other: &str| {
            let pad = " ".repeat(8 - tid.len());
            format!(
                "{tid} getppid() = 1\n{other} exit(0){pad}                = ?\n\
                 {tid} exit_group(0){pad}          = ?\n"
            )
        };
        assert_eq!(
            threads(&trace("4192564", "4192565")),
            threads(&trace("50001", "50002")),
        );
    }
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_launched_programs_calls_begin_at_its_exec() {
        let scratch = ScratchDir::new("syscalls-gate");
        let record = |name: &str, arguments: &[&str]| {
            let directory = below(&scratch, name);
            let output = Command::new(launcher())
                .env_clear()
                .arg("MICROSCOPE_TEST_GATE_PAUSE_MS=2000")
                .arg(env!("CARGO_BIN_EXE_microscope"))
                .args(["record", "-o", "trace.usprof"])
                .args(arguments)
                .arg("--")
                .arg(fixture())
                .arg("files")
                .current_dir(&directory)
                .stdin(Stdio::null())
                .output()
                .expect("run microscope record");
            assert!(
                directory.join("trace.usprof").exists(),
                "{}",
                text(&output.stderr)
            );
            directory
        };

        let streamed = log(&record("streamed", &[]), &["trace=all"]);
        assert!(
            streamed
                .lines()
                .next()
                .and_then(|line| line.split_once(' '))
                .is_some_and(|(_, call)| call.starts_with("execve(")),
            "{streamed}"
        );

        let counted = read(&record("counted", &["--syscalls", "counts"]))
            .syscalls()
            .stats()
            .per_call()
            .iter()
            .map(|(call, stats)| (call.display_name().into_owned(), stats.calls))
            .collect::<Vec<_>>();
        let ptrace = below(&scratch, "ptrace");
        recorded(
            &ptrace,
            &["--syscall-engine", "ptrace"],
            &fixture(),
            &["files"],
        );
        let traced = read(&ptrace)
            .syscalls()
            .stats()
            .per_call()
            .iter()
            .map(|(call, stats)| (call.display_name().into_owned(), stats.calls))
            .collect::<Vec<_>>();
        assert_eq!(counted, traced);
    }

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
        let lost = profile.syscalls().lost;
        let lost = lost.dropped + lost.skipped;
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
        all_shown_right(&actual, &expected, lost, context);
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

            all_shown_right(&calls, &expected_calls, lost, context);
        }
    }

    // A record the kernel had no room for is counted as lost: everything shown
    // must be right, and no more may be missing than were lost.
    fn all_shown_right<V: PartialEq>(
        shown: &BTreeMap<u32, V>,
        expected: &BTreeMap<u32, V>,
        lost: u64,
        context: &str,
    ) {
        assert!(
            shown
                .iter()
                .all(|(tid, value)| expected.get(tid) == Some(value)),
            "{context}"
        );
        let missing = expected
            .keys()
            .filter(|tid| !shown.contains_key(tid))
            .count() as u64;
        assert!(missing <= lost, "{missing} missing, {lost} lost: {context}");
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
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn functions_see_what_tracepoints_see() {
        let scratch = ScratchDir::new("syscalls-hooks");
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

        // Two runs interleave their threads as each was scheduled, so compare
        // each thread's own calls.
        let fentry = threads(&by("fentry", &["--syscall-filter", filter]));
        let tracepoint = threads(&by("tp_btf", &[]));
        assert_eq!(
            fentry,
            tracepoint,
            "only fentry:\n{:#?}\nonly tp_btf:\n{:#?}",
            fentry.difference(&tracepoint).collect::<Vec<_>>(),
            tracepoint.difference(&fentry).collect::<Vec<_>>()
        );
    }
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
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_recording_holds_its_calls_beside_its_samples() {
        let scratch = ScratchDir::new("syscalls-record");

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
        assert!(!calls.is_empty());
        assert!(
            calls.iter().all(|call| call.call.name() == Some("openat")
                && call.outcome.raw().is_some_and(|ret| ret < 0)),
            "{calls:#?}"
        );
    }

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

        let quick: Vec<_> = counted
            .syscalls()
            .events
            .into_iter()
            .filter(|event| matches!(&event.event, TraceEvent::Call(call) if !call.flags.kept()))
            .collect();
        assert!(quick.is_empty(), "{quick:#?}");

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

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_counting_recording_counts_each_threads_calls() {
        let directory = ScratchDir::new("syscalls-record-threads");

        // At `prod` (`SyscallLevel::Counts`), stats aggregate per process (`thread = Tid(0)`).
        let prod = record_load(
            directory.path(),
            "prod.usprof",
            &["--preset", "prod"],
            3,
            100,
        );
        let prod_syscalls = prod.syscalls();
        let prod_reads: BTreeMap<_, u64> = prod_syscalls
            .counted
            .by_call
            .iter()
            .filter(|((_, _, call), _)| call.name() == Some("read"))
            .map(|(&(process, thread, _), stats)| ((process, thread), stats.calls))
            .collect();
        assert_eq!(prod_reads.len(), 1, "{prod_reads:?}");
        let (&(prod_proc, prod_tid), &prod_count) = prod_reads.iter().next().expect("one row");
        assert_eq!(prod_tid.get(), 0, "prod stats collapse to process level");
        assert!(prod_count >= 300, "{prod_reads:?}");
        let prod_total = prod_syscalls
            .stats()
            .per_call()
            .into_iter()
            .find(|(call, _)| call.name() == Some("read"))
            .map(|(_, stats)| stats.calls);
        assert_eq!(prod_total, Some(prod_count));
        let _ = prod_proc;

        // At `dev` (`SyscallLevel::Calls`), per-thread stats remain intact (including capped counts).
        let dev = record_load(
            directory.path(),
            "dev.usprof",
            &["--preset", "dev", "--syscall-cap", "1"],
            3,
            100,
        );
        let dev_syscalls = dev.syscalls();
        let dev_stats = dev_syscalls.stats();
        let dev_reads: BTreeMap<_, u64> = dev_stats
            .by_call
            .iter()
            .filter(|((_, _, call), _)| call.name() == Some("read"))
            .map(|(&(process, thread, _), stats)| ((process, thread), stats.calls))
            .collect();
        let processes: BTreeSet<_> = dev_reads.keys().map(|&(process, _)| process).collect();
        assert_eq!(processes.len(), 1, "{dev_reads:?}");
        let process = processes.into_iter().next().expect("one process");
        let workers: Vec<u64> = dev_reads
            .iter()
            .filter(|&(&(_, thread), _)| thread.get() != process.get())
            .map(|(_, &count)| count)
            .collect();
        assert_eq!(workers, [100, 100], "{dev_reads:?}");
        assert!(
            dev_reads
                .get(&(process, microscope::syscalls::Tid::new(process.get())))
                .is_some_and(|&count| count >= 100),
            "{dev_reads:?}"
        );
        let dev_total = dev_stats
            .per_call()
            .into_iter()
            .find(|(call, _)| call.name() == Some("read"))
            .map(|(_, stats)| stats.calls);
        assert_eq!(dev_total, Some(dev_reads.values().sum()));
    }

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

        let counts = profile.syscalls().stats().per_call();
        let closes = counts
            .iter()
            .find(|(call, _)| call.name() == Some("close"))
            .map_or(0, |(_, stats)| stats.calls);
        // On a miss, say whether the closes went missing from the kernel's
        // counts or from the streamed calls.
        let syscalls = profile.syscalls();
        let in_counts = syscalls
            .counted
            .per_call()
            .iter()
            .find(|(call, _)| call.name() == Some("close"))
            .map_or(0, |(_, stats)| stats.calls);
        let fewest = if syscalls.lost.skipped == 0 {
            280_000
        } else {
            0
        };
        assert!(
            (fewest..280_020).contains(&closes),
            "{closes} closes: {in_counts} counted in the kernel, {} streamed; lost {:?}",
            closes - in_counts,
            syscalls.lost
        );
    }

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

    fn counted(profile: &microscope::profiler::Profile, name: &str) -> u64 {
        profile
            .syscalls()
            .stats()
            .per_call()
            .iter()
            .find(|(call, _)| call.name() == Some(name))
            .map_or(0, |(_, stats)| stats.calls)
    }

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

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_counting_recording_keeps_ten_examples_a_minute_of_each_kind() {
        let directory = ScratchDir::new("syscalls-record-examples");
        let program = ["0", "20000", "12"].map(str::to_owned);
        let profile = record_fixture(
            directory.path(),
            "prod.usprof",
            &["--preset", "prod"],
            "syscall-exceptions",
            &program,
        );
        assert_eq!(profile.setting("syscall examples"), Some("10/min"));

        let kept = kept(&profile);
        let of = |name: &str, why: &str| -> u64 {
            kept.iter()
                .filter(|((call, reason, _), _)| call == name && *reason == why)
                .map(|(_, count)| count)
                .sum()
        };
        assert!((10..=20).contains(&of("close", "failed")), "{kept:#?}");
        assert!(
            (10..=12).contains(&of("clock_nanosleep", "slow")),
            "{kept:#?}"
        );

        let counts = profile.syscalls().stats().per_call();
        let count = |name: &str| {
            counts
                .iter()
                .find(|(call, _)| call.name() == Some(name))
                .map(|(_, stats)| (stats.calls, stats.errors))
        };
        assert_eq!(count("close").map(|(_, errors)| errors), Some(20_000));
        assert_eq!(count("clock_nanosleep"), Some((12, 0)));
        assert_eq!(profile.syscalls_lost().capped, 0);
    }

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_capped_recording_keeps_its_cap_from_each_process_and_counts_the_rest() {
        let directory = ScratchDir::new("syscalls-record-stream-cap");
        let load = Scenario::fixture("syscall-load");
        let load = load.to_str().expect("a UTF-8 path");
        let script = format!("{load} 1 9000 9000 & {load} 1 9000 9000; wait");
        let record = |file: &str, arguments: &[&str]| {
            let recorded = privileged()
                .args(["record", "-o", file, "--syscalls", "calls"])
                .args(arguments)
                .args(["--", "/bin/sh", "-c", &script])
                .current_dir(directory.path())
                .output()
                .expect("run microscope record");
            assert!(recorded.status.success(), "{}", text(&recorded.stderr));
            microscope::profiler::Profile::read(&directory.path().join(file))
                .expect("the capture reads")
                .profile
        };
        let loads = |profile: &microscope::profiler::Profile| {
            let mut closes: BTreeMap<microscope::syscalls::Pid, u64> = BTreeMap::new();
            for ((process, _, call), stats) in &profile.syscalls().stats().by_call {
                if call.name() == Some("close") {
                    *closes.entry(*process).or_default() += stats.calls;
                }
            }
            closes.retain(|_, closes| *closes >= 1000);
            closes
        };

        let capped = record("capped.usprof", &["--syscall-cap", "500"]);
        let whole = record("whole.usprof", &[]);
        let skipped = capped.syscalls_lost().skipped + whole.syscalls_lost().skipped;
        assert_eq!(capped.setting("syscall cap"), Some("500/s"));
        assert!(capped.syscalls_lost().capped > 0);

        let processes = loads(&capped);
        assert_eq!(processes.len(), 2, "{processes:?}");
        let mut by_process: BTreeMap<microscope::syscalls::Pid, (u64, u64, u64)> = BTreeMap::new();
        for event in capped.syscalls().events {
            if let TraceEvent::Call(call) = &event.event
                && processes.contains_key(&call.process)
            {
                let (streamed, first, last) =
                    by_process.entry(call.process).or_insert((0, u64::MAX, 0));
                *streamed += 1;
                *first = (*first).min(call.entered.unwrap_or_else(|| call.time()));
                *last = (*last).max(call.time());
            }
        }

        for (process, (streamed, first, last)) in &by_process {
            let seconds = last / 1_000_000_000 - first / 1_000_000_000 + 1;
            assert!(
                (1_200..=500 * seconds).contains(streamed),
                "{process:?} streamed {streamed} in {seconds} s: {by_process:?}"
            );
        }

        let mut capped: Vec<u64> = processes.into_values().collect();
        let mut whole: Vec<u64> = loads(&whole).into_values().collect();
        capped.sort_unstable();
        whole.sort_unstable();
        if skipped == 0 {
            assert_eq!(capped, whole);
        } else {
            assert!(
                capped.iter().chain(&whole).all(|&closes| closes <= 9_050),
                "{capped:?} {whole:?}"
            );
        }
    }

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
                // pthread_join waits on a futex, finds the thread's id already
                // changed, or finds the thread gone and makes no call, as each
                // run happens to schedule it.
                .filter(|(name, ..)| name != "futex")
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
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn continuous_counts_split_into_segments_that_sum_to_every_call() {
        let directory = ScratchDir::new("syscalls-continuous");
        let segments = directory.path().join("segments");

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

    fn saved_by_a_call(
        directory: &Path,
        environment: &[&str],
        arguments: &[&str],
        program: &[&str],
    ) -> (String, microscope::profiler::Profile) {
        let recorded = Command::new(launcher())
            .env_clear()
            .args(environment)
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

        assert_eq!(said.matches("saved ").count(), 1, "{said}");
        let saved = microscope::profiler::Profile::read(&directory.join("window-1.usprof"))
            .unwrap_or_else(|error| panic!("{error}: {said}"))
            .profile;
        (said, saved)
    }

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_failing_call_saves_the_window_that_holds_it() {
        let directory = ScratchDir::new("syscalls-save-on-errno");
        let (said, saved) = saved_by_a_call(
            directory.path(),
            &[],
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

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_slow_call_saves_the_window_that_holds_it() {
        let directory = ScratchDir::new("syscalls-save-on-slow");
        // The trace hands each slow call over late, as a loaded machine can, so
        // the trigger arrives after the program has ended and must still save.
        let (said, saved) = saved_by_a_call(
            directory.path(),
            &["MICROSCOPE_TEST_TRIGGER_DELAY_MS=300"],
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

            let (before, end) = (written(), Instant::now() + DEADLINE);
            while written() <= before {
                assert!(Instant::now() < end, "no segment after {before}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        drop(input);
        assert!(recording.wait().expect("wait").success());

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
            found.push((
                byte,
                syscalls.lost.dropped + syscalls.lost.unseen + syscalls.lost.skipped,
            ));
        }

        let read: usize = found.iter().map(|&(byte, _)| byte).sum();
        let lost: u64 = found.iter().map(|&(_, lost)| lost).sum();
        let holding = found.iter().filter(|&&(byte, _)| byte > 0).count();
        assert!(read <= 3 && read as u64 + lost >= 3, "{found:?}");
        assert!(holding >= 2 || lost > 0, "{found:?}");
    }

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_burst_of_copied_stacks_loses_no_calls() {
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

        let trace = unpathed(&log(directory.path(), &["trace=read,exit_group"]));
        let lost = read(directory.path()).syscalls().lost.dropped;
        assert!(
            trace.contains(r#"read(0, "x", 1) = 1 /* entry unseen */"#),
            "{trace}\n{lines:?}"
        );
        assert!(trace.contains("exit_group(23) = ?"), "{trace}");
        assert!(
            trace.ends_with("+++ exited with 23 +++"),
            "{lost} lost: {trace}"
        );
    }

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_call_whose_entry_or_exit_the_kernel_skipped_is_shown() {
        const SKIP_ENTRY: u64 = 0x5ca1_ab1e;
        const SKIP_EXIT: u64 = 0x5ca1_ab1f;
        for (hook, arguments) in [
            ("fentry", &["--syscall-filter", "trace=getpgid"][..]),
            (
                "tp_btf",
                &["--syscall-filter", "trace=getpgid,rt_sigreturn"],
            ),
            ("tp_btf", &["--preset", "prod"]),
        ] {
            let directory = ScratchDir::new("syscalls-skipped");
            let code = recorded(directory.path(), arguments, &fixture(), &["skipped"]);
            assert_eq!(code, Some(0), "{arguments:?}");

            let profile = read(directory.path());
            assert_eq!(profile.setting("syscall hook"), Some(hook), "{arguments:?}");
            let counted = profile
                .syscalls()
                .stats()
                .per_call()
                .into_iter()
                .find(|(call, _)| call.name() == Some("getpgid"))
                .map(|(_, stats)| stats.calls);
            assert_eq!(counted, Some(5), "{arguments:?}");
            if arguments.contains(&"prod") {
                continue;
            }

            let events = profile.syscalls().events;
            let getpgids: Vec<_> = events
                .iter()
                .filter_map(|event| match &event.event {
                    TraceEvent::Call(call) if call.call.name() == Some("getpgid") => Some(call),
                    _ => None,
                })
                .collect();
            let of = |mark: u64| -> Vec<&SyscallEvent> {
                getpgids
                    .iter()
                    .copied()
                    .filter(|call| call.args[1] == mark)
                    .collect()
            };
            let threads = |calls: &[&SyscallEvent]| -> BTreeSet<_> {
                calls.iter().map(|call| call.thread).collect()
            };

            let entries = of(SKIP_ENTRY);
            assert_eq!(
                (entries.len(), threads(&entries).len()),
                (2, 2),
                "{arguments:?}: {getpgids:#?}"
            );
            for call in &entries {
                assert!(
                    call.flags.contains(EventFlags::ENTRY_UNSEEN)
                        && call.entered.is_none()
                        && matches!(call.outcome, Outcome::Returned(_)),
                    "{arguments:?}: {call:#?}"
                );
            }

            let exits = of(SKIP_EXIT);
            assert_eq!(
                (exits.len(), threads(&exits)),
                (2, threads(&entries)),
                "{arguments:?}: {getpgids:#?}"
            );
            for call in &exits {
                assert!(
                    !call.flags.contains(EventFlags::ENTRY_UNSEEN)
                        && call.entered.is_some()
                        && call.exited.is_none()
                        && call.outcome == Outcome::Unavailable,
                    "{arguments:?}: {call:#?}"
                );
            }

            assert!(
                matches!(&of(0)[..], [call] if !call.flags.contains(EventFlags::ENTRY_UNSEEN)
                    && call.exited.is_some()),
                "{arguments:?}: {getpgids:#?}"
            );
            assert_eq!(getpgids.len(), 5, "{arguments:?}: {getpgids:#?}");
        }
    }

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_record_another_program_makes_meanwhile_leaves_a_call_whole() {
        const INTERLEAVE: u64 = 0x5ca1_ab20;
        for (hook, arguments) in [
            ("fentry", &["--syscall-filter", "trace=getpgid"][..]),
            ("tp_btf", &[]),
        ] {
            let directory = ScratchDir::new("syscalls-interleaved");
            let code = recorded(directory.path(), arguments, &fixture(), &["interleaved"]);
            assert_eq!(code, Some(0), "{arguments:?}");

            let profile = read(directory.path());
            assert_eq!(profile.setting("syscall hook"), Some(hook), "{arguments:?}");
            let events = profile.syscalls().events;
            let calls: Vec<_> = events
                .iter()
                .filter_map(|event| match &event.event {
                    TraceEvent::Call(call)
                        if matches!(call.call.name(), Some("getpgid" | "getpid")) =>
                    {
                        Some(call)
                    }
                    _ => None,
                })
                .collect();
            assert!(
                matches!(&calls[..], [marked, plain]
                    if marked.call.name() == Some("getpgid")
                        && marked.args[1] == INTERLEAVE
                        && matches!(marked.outcome, Outcome::Returned(_))
                        && marked.duration().is_some()
                        && plain.call.name() == Some("getpgid")
                        && plain.args[1] == 0),
                "{arguments:?}: {calls:#?}"
            );
        }
    }

    #[cfg(feature = "tools")]
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn each_program_verifies_within_its_budget() {
        let budget = |program: &str| match program.trim_end_matches(|c: char| c.is_ascii_digit()) {
            "sys_enter" | "exit_finish" => Some(55_000),
            "enter_" => Some(160_000),
            "sys_exit" => Some(18_000),
            "exit_" => Some(300_000),
            "walk_stack" | "python_sample" | "python_sample with kernel samples" => Some(100_000),
            "signal_deliver" => Some(1_000),
            "terminal_enter" => Some(52_000),
            "process_end" | "process_exec" | "process_exit" | "process_fork" | "in_flight" => {
                Some(2_000)
            }
            _ => None,
        };

        for (filter, expected) in [
            ("trace=all", ["sys_enter", "sys_exit", "terminal_enter"]),
            (
                "trace=getpgid,exit_group",
                ["enter_0", "exit_0", "terminal_enter"],
            ),
        ] {
            let output = Command::new(launcher())
                .env_clear()
                .arg(env!("CARGO_BIN_EXE_microscope-tools"))
                .args(["bpf-verified", "--syscall-filter", filter])
                .output()
                .expect("run microscope-tools bpf-verified");
            assert!(output.status.success(), "{}", text(&output.stderr));
            let verified: BTreeMap<String, u64> =
                serde_json::from_slice(&output.stdout).expect("JSON of each program's count");

            for program in expected {
                assert!(
                    verified.contains_key(program),
                    "{filter}: no {program} in {verified:?}"
                );
            }

            let over: Vec<String> = verified
                .iter()
                .filter_map(|(program, &count)| match budget(program) {
                    Some(most) if count <= most => None,
                    Some(most) => Some(format!("{program}: {count} > {most}")),
                    None => Some(format!("{program}: {count}, and no budget")),
                })
                .collect();
            assert!(over.is_empty(), "{filter}: {over:#?}");
        }
    }

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_waiting_calls_stack_is_taken_as_it_begins() {
        const ROUNDS: usize = 20;
        for arguments in [
            &["--syscall-filter", "trace=read"][..],
            &["--syscalls", "calls"],
        ] {
            let directory = ScratchDir::new("syscalls-waiting");
            let mut target = ExternalProcess::spawn_command(
                Command::new(fixture()).args(["migrate", &ROUNDS.to_string()]),
            );
            let pid = target.process_id().get().to_string();
            let mut running = Running::spawn(
                privileged()
                    .args(["record", "-o", "trace.usprof", "--pid", &pid])
                    .args(arguments)
                    .current_dir(directory.path()),
            );
            running.line(|line| line.contains(&format!("recording process {pid}")));

            let stdout = target.take_stdout();
            let (sender, said) = mpsc::channel();
            std::thread::spawn(move || {
                let mut line = String::new();
                let _ = BufReader::new(stdout).read_line(&mut line);
                let _ = sender.send(line);
            });
            target.release();
            let blocked = said
                .recv_timeout(DEADLINE)
                .expect("the fixture's next line");
            assert_eq!(blocked, "blocked\n", "{arguments:?}");
            running.interrupt();
            let (code, lines) = running.finish();
            assert_eq!(code, Some(0), "{lines:?}");

            let profile = read(directory.path());
            let syscalls = profile.syscalls();
            let functions = |stack: Option<u32>| {
                stack.map_or_else(Vec::new, |stack| profile.syscall_stack(stack).0)
            };
            let calls = |name: &str| -> Vec<(Outcome, Vec<String>)> {
                syscalls
                    .events
                    .iter()
                    .filter_map(|event| match &event.event {
                        TraceEvent::Call(call)
                            if call.call.name() == Some(name)
                                && (name != "read" || call.args[0] != 0) =>
                        {
                            Some((call.outcome, functions(event.stack)))
                        }
                        _ => None,
                    })
                    .collect()
            };

            let reads = calls("read");
            let returned = reads
                .iter()
                .filter(|(outcome, _)| *outcome == Outcome::Returned(1))
                .count();
            let unfinished = reads
                .iter()
                .filter(|(outcome, _)| *outcome == Outcome::Unfinished)
                .count();
            let lost = syscalls.lost.dropped + syscalls.lost.skipped;
            assert!(
                returned <= ROUNDS && returned as u64 + lost >= ROUNDS as u64,
                "{arguments:?}: {reads:#?}"
            );
            assert_eq!(unfinished, 1, "{arguments:?}: {reads:#?}");

            for (outcome, functions) in &reads {
                assert!(
                    functions.iter().any(|function| function == "wait_here")
                        || (functions.is_empty() && lost > 0),
                    "{arguments:?}: a read {outcome:?} in {functions:?}"
                );
            }

            if arguments.contains(&"calls") {
                let pauses = calls("pause");
                assert!(
                    matches!(&pauses[..], [(Outcome::Unfinished, functions)]
                        if functions.iter().any(|function| function == "migrate")),
                    "{pauses:?}"
                );
            }
        }
    }
}

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
fn events_first(events: &[microscope::profiler::RecordedEvent]) -> microscope::syscalls::Pid {
    events
        .iter()
        .find_map(|recorded| match &recorded.event {
            microscope::syscalls::TraceEvent::Call(call) => Some(call.process),
            _ => None,
        })
        .expect("a call")
}
