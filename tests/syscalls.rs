//! `microscope syscalls`: what it prints, held to strace's own output where
//! strace prints the same thing.

mod support;

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use regex::Regex;
use support::{ExternalProcess, Scenario, ScratchDir};

/// How long a test waits for a line it expects.
const DEADLINE: Duration = Duration::from_secs(30);

fn fixture() -> PathBuf {
    Scenario::fixture("syscalls")
}

fn microscope(arguments: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_microscope"));
    command.arg("syscalls").args(arguments);
    command.stdin(Stdio::null());
    command
}

/// Runs the fixture in `mode` under `microscope syscalls --engine ptrace`.
fn trace(directory: &Path, arguments: &[&str], mode: &str) -> Output {
    microscope(&["--engine", "ptrace"])
        .args(arguments)
        .arg("--")
        .arg(fixture())
        .arg(mode)
        .current_dir(directory)
        .output()
        .expect("run microscope")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// What differs between two runs of one program: addresses, ids, and
/// random bytes, and the padding a pid or address of another width
/// moves: after `-f`'s pid, and before the result. A loss is left out:
/// the kernel counts runs it skipped on any process's behalf, so one can
/// be reported while every call of the program's was seen, and a real
/// loss leaves the traces differing anyway.
fn normalized(trace: &str) -> String {
    let addresses = Regex::new(r"0x[0-9a-f]+").expect("regex");
    let ids = Regex::new(r"\b\d{4,}\b").expect("regex");
    let random = Regex::new(r#"getrandom\("[^"]*""#).expect("regex");
    let pid = Regex::new(r"^\d{4,} +").expect("regex");
    let padding = Regex::new(r"\) +=").expect("regex");

    trace
        .lines()
        .filter(|line| !line.starts_with("--- lost "))
        .map(|line| {
            let line = random.replace_all(line, "getrandom(\"\"");
            let line = pid.replace(&line, "N ");
            let varied = addresses.is_match(&line) || ids.is_match(&line) || line.starts_with("N ");
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
    assert_ne!(
        normalized("close(3)                                = 0"),
        normalized("close(3)                               = 0"),
    );
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

/// The fixture's trace by strace and by microscope, each with `arguments`, in
/// one directory.
fn both(arguments: &[&str], mode: &str) -> (String, String) {
    let directory = ScratchDir::new("syscalls-strace");
    let theirs = directory.path().join("strace.out");
    let ours = directory.path().join("microscope.out");

    let strace = Command::new("strace")
        .args(arguments)
        .arg("-o")
        .arg(&theirs)
        .arg(fixture())
        .arg(mode)
        .current_dir(directory.path())
        .stdin(Stdio::null())
        .output()
        .expect("run strace");

    let traced = trace(
        directory.path(),
        &[arguments, &["-o", ours.to_str().expect("path")]].concat(),
        mode,
    );

    assert_eq!(
        strace.status.code(),
        traced.status.code(),
        "exit codes differ; microscope said {}",
        text(&traced.stderr)
    );

    let read = |path: &Path| std::fs::read_to_string(path).expect("read trace");
    (read(&theirs), read(&ours))
}

#[test]
fn files_trace_as_strace_prints_them() {
    for arguments in [
        &["-yy"][..],
        &["-e", "trace=%file,%desc", "-y"],
        &["-X", "raw"],
        &[
            "-X",
            "verbose",
            "-e",
            "trace=openat,fstat,rseq,prlimit64,pipe2",
        ],
    ] {
        let (theirs, ours) = both(arguments, "files");
        assert_same(&ours, &theirs, &format!("with {arguments:?}"));
    }
}

/// Times, numbers, and dumps of the data that moved print as strace's,
/// except for the times' values.
#[test]
fn times_numbers_and_dumps_print_as_strace_prints_them() {
    let times = Regex::new(r"\d{2}:\d{2}:\d{2}(\.\d{6})?|\d+\.\d{6}").expect("regex");
    let untimed = |trace: &str| times.replace_all(trace, "T").into_owned();
    for arguments in [
        &[
            "-e",
            "trace=pipe2,write,read,writev,readv",
            "-e",
            "read=3",
            "-e",
            "write=4",
        ][..],
        &["-tt", "-T", "-n", "-e", "trace=pipe2,write,read"],
        &["-t", "-e", "trace=write"],
        &["-ttt", "-e", "trace=readv"],
        &["-r", "-e", "trace=read,write"],
    ] {
        let (theirs, ours) = both(arguments, "dumps");
        assert_same(
            &untimed(&ours),
            &untimed(&theirs),
            &format!("with {arguments:?}"),
        );
    }
}

/// `--syscall-times` and `--relative-timestamps` print times to the
/// precision asked, as strace does, padded and aligned as strace's. When
/// the short and long forms are mixed, the last one given wins.
#[test]
fn times_print_to_each_precision_as_strace_prints_them() {
    // A time's digits differ between runs; how many there are must not.
    let times = Regex::new(r"<\d+(\.\d+)?>|^ *\d+(\.\d+)? ").expect("regex");
    let shaped = |trace: &str| {
        trace
            .lines()
            .map(|line| {
                times.replace_all(line, |found: &regex::Captures<'_>| {
                    found[0].replace(|c: char| c.is_ascii_digit(), "t")
                })
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let mut sets: Vec<Vec<String>> = ["s", "ms", "us", "ns", "NS"]
        .iter()
        .map(|precision| {
            vec![
                format!("--syscall-times={precision}"),
                format!("--relative-timestamps={precision}"),
            ]
        })
        .collect();
    sets.push(vec![
        "--syscall-times".into(),
        "--relative-timestamps".into(),
    ]);
    sets.push(vec![
        "--syscall-times=ns".into(),
        "-T".into(),
        "-r".into(),
        "--relative-timestamps=ms".into(),
    ]);

    for set in sets {
        let mut arguments: Vec<&str> = set.iter().map(String::as_str).collect();
        arguments.extend(["-e", "trace=pipe2,write,read"]);
        let (theirs, ours) = both(&arguments, "dumps");
        assert!(theirs.contains('<'), "strace printed no times: {theirs}");
        assert_same(
            &shaped(&ours),
            &shaped(&theirs),
            &format!("with {arguments:?}"),
        );
    }
}

/// `--absolute-timestamps` takes strace's formats and precisions, in any
/// order and case, each one over the last, and -t's wall clock comes
/// before -r's time, which it puts in `(+ )`.
#[test]
fn wall_clocks_print_to_each_format_and_precision_as_strace_prints_them() {
    let times = Regex::new(r"^[\d:.]+ (\(\+ *[\d.]+\) )?|^ *[\d.]+ ").expect("regex");
    let shaped = |trace: &str| {
        trace
            .lines()
            .map(|line| {
                times.replace(line, |found: &regex::Captures<'_>| {
                    found[0].replace(|c: char| c.is_ascii_digit(), "t")
                })
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    for set in [
        &["--absolute-timestamps"][..],
        &["--timestamps=ns"],
        &["--absolute-timestamps=unix"],
        &["--absolute-timestamps=none"],
        &["--absolute-timestamps=ms"],
        &["--absolute-timestamps=precision:us"],
        &["--absolute-timestamps=format:unix,precision:ns"],
        &["--absolute-timestamps=ns,UNIX"],
        &["--absolute-timestamps=unix,ns,ms"],
        &["--absolute-timestamps=unix", "--absolute-timestamps=ns"],
        &["--absolute-timestamps=ns", "--absolute-timestamps"],
        &["--absolute-timestamps=ns", "--absolute-timestamps=none"],
        &["--absolute-timestamps=none", "--absolute-timestamps=ms"],
        &["--absolute-timestamps=unix,ms", "-r"],
        &["--absolute-timestamps=none", "-r"],
        &["-t", "-r"],
        &["-ttt", "--relative-timestamps=ns"],
    ] {
        let mut arguments = set.to_vec();
        arguments.extend(["-e", "trace=pipe2,write,read"]);
        let (theirs, ours) = both(&arguments, "dumps");
        assert_same(
            &shaped(&ours),
            &shaped(&theirs),
            &format!("with {arguments:?}"),
        );
    }

    // strace refuses these; so must microscope.
    let directory = ScratchDir::new("syscalls-stamps");
    for set in [
        &["--absolute-timestamps=xs"][..],
        &["--absolute-timestamps=format:ns"],
        &["--absolute-timestamps=precision:unix"],
        &["-tt", "--absolute-timestamps=ms"],
        &["--absolute-timestamps=unix", "-t"],
    ] {
        let traced = trace(directory.path(), set, "dumps");
        assert!(!traced.status.success(), "{set:?} was taken");
    }
}

/// `-ff` writes each thread's calls to a file of its own, as strace does.
#[test]
fn ff_writes_each_threads_calls_to_its_own_file() {
    let directory = ScratchDir::new("syscalls-ff");
    let arguments = ["-ff", "-e", "trace=write,getppid,wait4"];
    let run = |name: &str, command: &mut Command| {
        let status = command
            .args(arguments)
            .arg("-o")
            .arg(directory.path().join(name))
            .arg("--")
            .arg(fixture())
            .arg("family")
            .current_dir(directory.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .expect("run the tracer");
        assert!(status.success(), "{name}");

        // Each file's text, in the order its thread began.
        let mut files: Vec<(u32, String)> = std::fs::read_dir(directory.path())
            .expect("list")
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let file = entry.file_name().into_string().ok()?;
                let id = file.strip_prefix(&format!("{name}."))?.parse().ok()?;
                Some((id, std::fs::read_to_string(entry.path()).ok()?))
            })
            .collect();

        files.sort_unstable();

        files.into_iter().map(|(_, text)| text).collect::<Vec<_>>()
    };

    let theirs = run("strace", &mut Command::new("strace"));
    let ours = run("microscope", &mut microscope(&["--engine", "ptrace"]));

    assert_eq!(ours.len(), theirs.len(), "{ours:#?}\n---\n{theirs:#?}");
    for (ours, theirs) in ours.iter().zip(&theirs) {
        assert_same(ours, theirs, "in one thread's file");
    }
}

/// `--sample 1/N` prints one call in N while the table counts them all,
/// and `--slower-than` prints only calls that took that long.
#[test]
fn sampling_and_a_duration_floor_choose_which_calls_print() {
    let directory = ScratchDir::new("syscalls-sample");
    let calls = |output: &Output| {
        text(&output.stderr)
            .lines()
            .filter(|line| line.starts_with("close("))
            .count()
    };

    let every = trace(directory.path(), &["-e", "trace=close"], "files");
    let sampled = trace(
        directory.path(),
        &[
            "-e",
            "trace=close",
            "--sample",
            "1/2",
            "-C",
            "-U",
            "name,calls",
        ],
        "files",
    );

    assert_eq!(calls(&every), 7, "{}", text(&every.stderr));
    assert_eq!(calls(&sampled), 4, "{}", text(&sampled.stderr));

    let summary = text(&sampled.stderr);
    assert!(
        summary
            .lines()
            .any(|line| line.split_whitespace().collect::<Vec<_>>() == ["close", "7"]),
        "{summary}"
    );

    let slow = trace(
        directory.path(),
        &["-e", "trace=close", "--slower-than", "1h"],
        "files",
    );

    let lines = text(&slow.stderr);
    assert_eq!(calls(&slow), 0, "{lines}");
    assert!(lines.contains("+++ exited with 3 +++"), "{lines}");

    assert!(
        text(&trace(directory.path(), &["--sample", "2"], "files").stderr)
            .contains("--sample takes 1/N, not 2")
    );
}

/// `-kk` unwinds each call's stack, and each signal's, to the frames
/// strace's does: the same modules at the same addresses, and in the
/// program, which has debug information, the same functions, offsets, and
/// lines. A library's function may go by another of the names its address
/// has, and where unwinding stops short, each says why in words of its
/// own.
#[test]
fn stacks_unwind_to_the_frames_strace_finds() {
    let frame = Regex::new(
        r"^ > (\S+?)\(([^+)]*)(?:\+(0x[0-9a-f?]+))?\) \[(0x[0-9a-f?]+)\](?: (\S+):(\d+))?$",
    )
    .expect("regex");
    let address = Regex::new(r"(?m)^ > .*\[(0x[0-9a-f]+)\]").expect("regex");
    let program = fixture();
    let program = program.to_str().expect("path");

    for (arguments, mode) in [
        (&["-kk", "-e", "trace=write,read,readv"][..], "dumps"),
        (&["-f", "-kk", "-e", "trace=write,getppid"], "family"),
    ] {
        let (theirs, ours) = both(arguments, mode);
        let (theirs, ours) = (normalized(&theirs), normalized(&ours));

        // strace can't unwind the dynamic loader's `_start`, which has no
        // call-frame information, and says so; microscope ends the stack
        // there, at the process's entry point.
        let mut previous = "";
        let theirs = theirs
            .lines()
            .filter(|line| {
                let past_entry = previous.contains("(_start+")
                    && line.starts_with(" > ")
                    && !frame.is_match(line);
                previous = line;
                !past_entry
            })
            .collect::<Vec<_>>()
            .join("\n");
        let context = || format!("with {arguments:?}:\n{ours}\n---\n{theirs}");
        assert_eq!(
            ours.lines().count(),
            theirs.lines().count(),
            "{}",
            context()
        );

        let mut frames = 0;
        for (our, their) in ours.lines().zip(theirs.lines()) {
            if our.starts_with(" > [stack ends: ") {
                assert!(
                    their.starts_with(" > ") && !frame.is_match(their),
                    "{our} {their}: {}",
                    context()
                );
                continue;
            }

            let (Some(our), Some(their)) = (frame.captures(our), frame.captures(their)) else {
                assert_eq!(our, their, "{}", context());
                continue;
            };

            frames += 1;
            let field = |captures: &regex::Captures<'_>, index| {
                captures.get(index).map(|found| found.as_str().to_owned())
            };

            // Module and address, which normalizing leaves as `0x?`, by
            // the text before normalizing them away.
            assert_eq!(field(&our, 1), field(&their, 1), "{}", context());
            if field(&their, 1).as_deref() != Some(program) {
                continue;
            }

            assert_eq!(field(&our, 2), field(&their, 2), "{}", context());
            assert_eq!(field(&our, 6), field(&their, 6), "{}", context());
            let (our_file, their_file) = (field(&our, 5), field(&their, 5));

            // strace names a file as its debug information does; microscope
            // resolves it against the compilation's directory.
            assert!(
                match (our_file.as_deref(), their_file.as_deref()) {
                    (Some(our), Some(their)) => our.ends_with(their),
                    (our, their) => our == their,
                },
                "{our_file:?} {their_file:?}: {}",
                context()
            );
        }

        assert!(frames > 0, "{}", context());

        // The addresses themselves, frame by frame.
        let (theirs, ours) = both(arguments, mode);

        let addresses = |trace: &str| {
            address
                .captures_iter(trace)
                .map(|captures| captures[1].to_owned())
                .collect::<Vec<_>>()
        };

        assert_eq!(addresses(&ours), addresses(&theirs), "with {arguments:?}");
    }
}

#[test]
fn signals_and_compat_calls_trace_as_strace_prints_them() {
    let (theirs, ours) = both(&["-e", "trace=kill,rt_sigaction"], "signal");
    assert_same(&ours, &theirs, "for signals");

    let (theirs, ours) = both(&["-e", "trace=getpid@32,getpid@x32,getpid"], "compat");
    assert_same(&ours, &theirs, "for compat");
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

#[test]
fn children_and_threads_are_followed_unless_asked_not_to() {
    let directory = ScratchDir::new("syscalls-family");
    let followed = trace(
        directory.path(),
        &["-e", "trace=write,wait4,getppid,exit_group"],
        "family",
    );

    assert_eq!(followed.status.code(), Some(0));
    let lines = text(&followed.stderr);
    let parent = Regex::new(r"(?m)^\[pid +(\d+)\] wait4\((\d+), \[\{WIFEXITED\(s\) && WEXITSTATUS\(s\) == 4\}\], 0, NULL\) = (\d+)$")
        .expect("regex");

    let captures = parent
        .captures(&lines)
        .unwrap_or_else(|| panic!("no wait4 in\n{lines}"));

    let child = &captures[2];
    assert_eq!(&captures[3], child);

    for expected in [
        format!(r#"[pid {child:>5}] write(1, "child\n", 6)"#),
        format!("[pid {child:>5}] +++ exited with 4 +++"),
        format!("microscope: Process {child} attached"),
    ] {
        assert!(lines.contains(&expected), "no {expected:?} in\n{lines}");
    }

    assert!(
        Regex::new(r"(?m)^\[pid +\d+\] getppid\(\) += \d+$")
            .expect("regex")
            .is_match(&lines),
        "{lines}"
    );

    assert!(
        lines.trim_end().ends_with("] +++ exited with 0 +++"),
        "{lines}"
    );

    let alone = trace(
        directory.path(),
        &["--no-follow", "-e", "trace=write,wait4,getppid"],
        "family",
    );

    let lines = text(&alone.stderr);
    assert!(!lines.contains("child"), "{lines}");
    assert!(lines.contains("wait4("), "{lines}");
    assert!(
        lines.contains("getppid()"),
        "threads belong to the process\n{lines}"
    );
}

#[test]
fn the_summary_counts_calls_errors_and_errnos() {
    let directory = ScratchDir::new("syscalls-summary");
    let summary = trace(
        directory.path(),
        &[
            "-c",
            "-e",
            "trace=openat,unlink,write",
            "-U",
            "name,calls,errors,errnos",
            "-S",
            "name",
        ],
        "files",
    );

    assert_eq!(summary.status.code(), Some(3));
    let lines = text(&summary.stderr);

    let rows: Vec<Vec<&str>> = lines
        .lines()
        .map(|line| line.split_whitespace().collect())
        .collect();

    assert_eq!(rows[0], ["syscall", "calls", "errors", "errnos"], "{lines}");

    let openat = rows
        .iter()
        .find(|row| row[0] == "openat")
        .expect("openat row");

    assert!(
        openat.contains(&"ENOENT×1") || openat.iter().any(|cell| cell.starts_with("ENOENT×")),
        "{lines}"
    );

    assert!(
        rows.iter().any(|row| row[..2] == ["unlink", "1"]),
        "{lines}"
    );

    assert!(rows.iter().any(|row| row[..2] == ["write", "1"]), "{lines}");
    assert_eq!(rows.last().expect("total")[0], "total");
    assert!(!lines.contains("openat("), "-c prints no calls\n{lines}");
}

/// `-c --syscall-times=ns` gives the table's times in nanoseconds, where
/// strace's are microseconds and print a call under one as 0.
#[test]
fn the_summary_times_calls_in_nanoseconds_when_asked() {
    let directory = ScratchDir::new("syscalls-summary-ns");
    let table = |arguments: &[&str]| {
        let summary = trace(
            directory.path(),
            &[
                &["-c", "-e", "trace=openat,close,write", "-S", "name"],
                arguments,
            ]
            .concat(),
            "files",
        );
        (summary.status.code(), text(&summary.stderr))
    };

    let (code, lines) = table(&[
        "--syscall-times=ns",
        "-U",
        "total-time,min-time,max-time,avg-time,p50,calls,name",
    ]);
    assert_eq!(code, Some(3), "{lines}");
    let rows: Vec<&str> = lines.lines().collect();
    assert_eq!(
        rows[0], "       seconds   min nsecs   max nsecs  nsecs/call   p50 nsecs     calls syscall",
        "{lines}"
    );

    let row = Regex::new(r"^ *(\d+\.\d{9})(( +\d+){4}) +\d+ \S+$").expect("regex");
    let mut times = Vec::new();
    for line in rows.iter().filter(|line| !line.starts_with('-')).skip(1) {
        let captures = row
            .captures(line)
            .unwrap_or_else(|| panic!("{line:?} is no row of nanoseconds:\n{lines}"));
        let name = line.rsplit(' ').next().unwrap_or_default();
        assert_eq!(
            line.len() - name.len(),
            rows[0].len() - "syscall".len(),
            "{line:?} is out of line:\n{lines}"
        );
        times.extend(
            captures[2]
                .split_whitespace()
                .map(|cell| cell.parse::<u64>().expect("a number")),
        );
    }

    // Microseconds shown as nanoseconds would all be whole thousands.
    assert!(
        times.iter().any(|time| time % 1_000 != 0),
        "no time finer than a microsecond:\n{lines}"
    );

    for precision in [
        &["--syscall-times=us"][..],
        &["--syscall-times"],
        &["-T"],
        &[],
    ] {
        let (code, lines) = table(precision);
        assert_eq!(code, Some(3), "{lines}");
        assert!(
            lines.starts_with("% time     seconds  usecs/call     calls    errors syscall\n"),
            "with {precision:?}:\n{lines}"
        );
    }

    let (code, lines) = table(&["--syscall-times=ms"]);
    assert_eq!(code, Some(1), "{lines}");
    assert!(
        lines.contains("-c's table shows microseconds or nanoseconds"),
        "{lines}"
    );
}

/// A program with no lines, as a release build ships, names a call's site
/// by its own function that called the C library, not by the C library's
/// system call wrapper, which every call of the kind shares.
#[test]
fn a_call_in_a_program_without_lines_is_placed_in_its_own_function() {
    let directory = ScratchDir::new("syscalls-nolines");
    let captured = microscope(&["--engine", "ptrace", "-e", "trace=openat", "-Z", "-k"])
        .args(["-o", "trace.usprof", "--"])
        .arg(Scenario::fixture("syscalls-nolines"))
        .arg("files")
        .current_dir(directory.path())
        .output()
        .expect("run microscope");
    assert_eq!(
        captured.status.code(),
        Some(3),
        "{}",
        text(&captured.stderr)
    );

    let profile = microscope::profiler::Profile::read(&directory.path().join("trace.usprof"))
        .expect("the capture reads")
        .profile;
    let stacks: Vec<u32> = profile
        .syscalls()
        .events
        .iter()
        .filter(|event| matches!(event.event, microscope::syscalls::TraceEvent::Call(_)))
        .filter_map(|event| event.stack)
        .filter(|&stack| {
            let (functions, _) = profile.syscall_stack(stack);
            functions.iter().any(|function| function == "files")
        })
        .collect();

    // The failed open of "missing", made in `files` through the C library.
    assert_eq!(stacks.len(), 1, "{}", text(&captured.stderr));
    let stack = stacks[0];
    let (functions, _) = profile.syscall_stack(stack);
    assert_ne!(
        functions[0], "files",
        "the call is made in the C library: {functions:?}"
    );
    assert_eq!(
        profile.syscall_site(stack).as_deref(),
        Some("files"),
        "{functions:?}"
    );
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
        let captured = microscope(&["--engine", "ptrace", "-e", "trace=openat", "-Z", "-k"])
            .args(["-o", "trace.usprof", "--"])
            .arg(Scenario::fixture(fixture))
            .current_dir(directory.path())
            .output()
            .expect("run microscope");
        assert_eq!(
            captured.status.code(),
            Some(3),
            "{}",
            text(&captured.stderr)
        );

        let profile = microscope::profiler::Profile::read(&directory.path().join("trace.usprof"))
            .expect("the capture reads")
            .profile;
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
        assert_eq!(stacks.len(), 1, "{fixture}: {}", text(&captured.stderr));
        let (functions, _) = profile.syscall_stack(stacks[0]);
        assert_eq!(
            profile.syscall_site(stacks[0]).as_deref(),
            Some(site),
            "{fixture}: {functions:?}"
        );
    }
}

#[test]
fn a_capture_holds_what_the_trace_shows_and_report_summarizes_it() {
    let directory = ScratchDir::new("syscalls-capture");
    let selection = ["-e", "trace=openat,unlink,write", "-Z"];
    let shown = trace(directory.path(), &selection, "files");
    let captured = trace(
        directory.path(),
        &[&selection[..], &["-k", "-o", "trace.usprof"]].concat(),
        "files",
    );

    assert_eq!(captured.status.code(), Some(3));
    assert_eq!(text(&captured.stderr), "", "a capture says nothing more");

    let path = directory.path().join("trace.usprof");
    let profile = microscope::profiler::Profile::read(&path)
        .expect("the capture reads")
        .profile;

    let syscalls = profile.syscalls();
    let calls: Vec<_> = syscalls
        .events
        .iter()
        .filter_map(|event| match &event.event {
            microscope::syscalls::TraceEvent::Call(call) => Some((call, event.stack)),
            _ => None,
        })
        .collect();

    // -Z shows only the calls that failed, and the capture holds those.
    let failed = text(&shown.stderr)
        .lines()
        .filter(|line| line.contains(" = -1 "))
        .count();

    assert!(failed > 0);
    assert_eq!(calls.len(), failed, "{}", text(&shown.stderr));
    let mut in_main = 0;

    for (call, stack) in &calls {
        assert!(call.outcome.raw().is_some_and(|ret| ret < 0), "{call:?}");
        let (functions, _) = profile.syscall_stack(stack.expect("a stack"));
        in_main += usize::from(functions.iter().any(|function| function == "main"));
    }

    // The dynamic loader's failed opens come before main.
    assert!(in_main > 0, "no stack reaches main");

    assert!(
        syscalls
            .events
            .iter()
            .any(|event| matches!(&event.event, microscope::syscalls::TraceEvent::Exit { .. })),
        "the process's end is noted"
    );

    let report = Command::new(env!("CARGO_BIN_EXE_microscope"))
        .args(["report", "trace.usprof"])
        .current_dir(directory.path())
        .output()
        .expect("run microscope report");

    let lines = text(&report.stdout);
    assert!(report.status.success(), "{}", text(&report.stderr));

    assert!(
        lines.contains(&format!("{failed} system calls, {failed} failed")),
        "{lines}"
    );

    assert!(
        lines
            .lines()
            .any(|line| line.split_whitespace().last() == Some("openat")),
        "{lines}"
    );
}

/// Runs `microscope report` on `trace.usprof` in `directory`.
fn report(directory: &Path, arguments: &[&str]) -> String {
    let report = Command::new(env!("CARGO_BIN_EXE_microscope"))
        .arg("report")
        .args(arguments)
        .arg("trace.usprof")
        .current_dir(directory)
        .output()
        .expect("run microscope report");
    assert!(report.status.success(), "{}", text(&report.stderr));
    text(&report.stdout)
}

/// A site in `report`'s table of call sites: its name, seconds, calls,
/// and errors.
type SiteRow = (String, f64, u64, u64);

/// The rows of `report`'s table of call sites, and the calls of its total.
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

/// `report` ranks a trace's call sites by their time, as the browser's
/// table of calls by site does, each with its calls and errors, and shows
/// `--limit` of them, counting the rest in the total.
#[test]
fn report_ranks_call_sites_by_time() {
    let directory = ScratchDir::new("syscalls-sites");
    let captured = trace(
        directory.path(),
        &["-e", "trace=openat,close", "-k", "-o", "trace.usprof"],
        "files",
    );
    assert_eq!(
        captured.status.code(),
        Some(3),
        "{}",
        text(&captured.stderr)
    );

    let made = microscope::profiler::Profile::read(&directory.path().join("trace.usprof"))
        .expect("the capture reads")
        .profile
        .syscall_count();
    let all = report(directory.path(), &["--limit", "1000"]);
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

    let cut = report(directory.path(), &["--limit", "2"]);
    let (shown, total) = site_rows(&cut);
    assert_eq!(shown, rows[..2], "{cut}");
    assert_eq!(total, made, "{cut}");
    assert!(
        cut.contains(&format!("and {} more sites,", rows.len() - 2)),
        "{cut}"
    );

    let unstacked = trace(
        directory.path(),
        &["-e", "trace=openat", "-o", "trace.usprof"],
        "files",
    );
    assert_eq!(unstacked.status.code(), Some(3));
    let unstacked = report(directory.path(), &[]);
    assert!(!unstacked.contains("By call site"), "{unstacked}");
    assert!(
        unstacked.contains("The calls have no stacks"),
        "{unstacked}"
    );
}

/// A program without lines has its calls grouped by the function that
/// called the C library.
#[test]
fn report_groups_calls_without_lines_by_their_function() {
    let directory = ScratchDir::new("syscalls-sites-nolines");
    let captured = microscope(&["--engine", "ptrace", "-e", "trace=openat", "-k"])
        .args(["-o", "trace.usprof", "--"])
        .arg(Scenario::fixture("syscalls-nolines"))
        .arg("files")
        .current_dir(directory.path())
        .output()
        .expect("run microscope");
    assert_eq!(
        captured.status.code(),
        Some(3),
        "{}",
        text(&captured.stderr)
    );

    let all = report(directory.path(), &[]);
    let (rows, _) = site_rows(&all);
    let files: Vec<(u64, u64)> = rows
        .iter()
        .filter(|row| row.0 == "files")
        .map(|row| (row.2, row.3))
        .collect();

    // Its three opens, one failed.
    assert_eq!(files, [(3, 1)], "{all}");
}

/// A sample of a trace's pprof profile as Go's tool reads it: its calls
/// and nanoseconds, and its locations' functions and places, innermost
/// first.
struct PprofSample {
    calls: u64,
    nanoseconds: u64,
    path: Vec<(String, String)>,
}

/// The samples of the pprof profile at `path`, and Go's tool's text.
fn pprof_samples(path: &Path) -> (Vec<PprofSample>, String) {
    let raw = Command::new(Scenario::fixture("pprof"))
        .args(["-raw", "-symbolize=none"])
        .arg(path)
        .output()
        .expect("run Go's pprof");
    let raw = text(&raw.stdout);
    let locations: BTreeMap<&str, (String, String)> = raw
        .lines()
        .skip_while(|line| *line != "Locations")
        .skip(1)
        .take_while(|line| *line != "Mappings")
        .filter_map(|line| {
            let (id, rest) = line.trim().split_once(": ")?;
            let words: Vec<&str> = rest.split_whitespace().collect();
            let place = words[words.len() - 2].to_owned();
            Some((id, (words[2..words.len() - 2].join(" "), place)))
        })
        .collect();
    let samples = raw
        .lines()
        .skip_while(|line| *line != "calls/count time/nanoseconds")
        .skip(1)
        .take_while(|line| *line != "Locations")
        .map(|line| {
            let (values, ids) = line.split_once(':').expect("a sample");
            let values: Vec<u64> = values
                .split_whitespace()
                .map(|value| value.parse().expect("a value"))
                .collect();
            PprofSample {
                calls: values[0],
                nanoseconds: values[1],
                path: ids
                    .split_whitespace()
                    .map(|id| locations[id].clone())
                    .collect(),
            }
        })
        .collect();
    (samples, raw)
}

/// The calls of the fixture's opens, traced with their stacks, converted
/// to each of `outputs` in a new directory: the directory, how many calls
/// were made, and how long the failed open of "missing" took.
fn converted_opens(outputs: &[&str]) -> (ScratchDir, u64, u64) {
    let directory = ScratchDir::new("syscalls-convert");
    let captured = trace(
        directory.path(),
        &["-e", "trace=openat", "-k", "-o", "trace.usprof"],
        "files",
    );
    assert_eq!(
        captured.status.code(),
        Some(3),
        "{}",
        text(&captured.stderr)
    );

    let profile = microscope::profiler::Profile::read(&directory.path().join("trace.usprof"))
        .expect("the capture reads")
        .profile;
    let took: Vec<u64> = profile
        .syscalls()
        .events
        .iter()
        .filter_map(|event| match (&event.event, event.stack) {
            (microscope::syscalls::TraceEvent::Call(call), Some(stack))
                if profile.syscall_site_name(stack) == "files · syscalls.c:24" =>
            {
                call.duration()
            }
            _ => None,
        })
        .collect();
    assert_eq!(took.len(), 1, "{}", text(&captured.stderr));

    for output in outputs {
        let converted = Command::new(env!("CARGO_BIN_EXE_microscope"))
            .args(["convert", "trace.usprof", output])
            .current_dir(directory.path())
            .output()
            .expect("run microscope convert");
        assert!(converted.status.success(), "{}", text(&converted.stderr));
    }

    (directory, profile.syscall_count(), took[0])
}

/// A trace converts to pprof, which Go's own tool reads, and to folded
/// stacks, each call a sample of its stack with the call innermost.
#[test]
fn convert_writes_each_call_as_a_sample_of_its_stack() {
    let (directory, made, took) = converted_opens(&["trace.pb.gz", "trace.folded"]);
    let (samples, raw) = pprof_samples(&directory.path().join("trace.pb.gz"));
    assert_eq!(
        samples.iter().map(|sample| sample.calls).sum::<u64>(),
        made,
        "{raw}"
    );
    assert!(
        samples.iter().all(|sample| sample.path[0].0 == "openat"),
        "{raw}"
    );

    let in_files = |sample: &&PprofSample| sample.path.iter().any(|(name, _)| name == "files");
    assert_eq!(
        samples
            .iter()
            .filter(in_files)
            .map(|sample| sample.calls)
            .sum::<u64>(),
        3,
        "{raw}"
    );

    let missing: Vec<(u64, u64)> = samples
        .iter()
        .filter(|sample| {
            sample
                .path
                .iter()
                .any(|(name, place)| name == "files" && place.ends_with("/syscalls.c:24:0"))
        })
        .map(|sample| (sample.calls, sample.nanoseconds))
        .collect();
    assert_eq!(missing, [(1, took)], "{raw}");

    let folded =
        std::fs::read_to_string(directory.path().join("trace.folded")).expect("the folded stacks");
    let mut weights = (0, 0);
    for line in folded.lines() {
        let (stack, weight) = line.rsplit_once(' ').expect("a weight");
        let weight: u64 = weight.parse().expect("a number");
        assert!(stack.ends_with(";openat"), "{folded}");
        weights.0 += weight;
        if stack.contains(";files;") {
            weights.1 += weight;
        }
    }

    assert_eq!(weights, (made, 3), "{folded}");
}

/// A trace converts to a Chrome trace, each call a span naming its site.
#[test]
fn convert_writes_each_call_as_a_chrome_span() {
    let (directory, made, _) = converted_opens(&["trace.json"]);
    let chrome: serde_json::Value = serde_json::from_slice(
        &std::fs::read(directory.path().join("trace.json")).expect("the trace"),
    )
    .expect("JSON");
    let calls: Vec<&serde_json::Value> = chrome["traceEvents"]
        .as_array()
        .expect("events")
        .iter()
        .filter(|event| event["cat"] == "syscall")
        .collect();
    assert_eq!(calls.len() as u64, made, "{chrome}");
    assert!(
        calls.iter().all(|call| call["ph"] == "X"
            && call["name"] == "openat"
            && call["dur"].as_f64() >= Some(0.0)),
        "{chrome}"
    );
    let missing: Vec<&&serde_json::Value> = calls
        .iter()
        .filter(|call| call["args"]["site"] == "files · syscalls.c:24")
        .collect();
    assert_eq!(missing.len(), 1, "{chrome}");
    assert_eq!(missing[0]["args"]["result"], -2, "{chrome}");
}

#[test]
fn ndjson_carries_names_paths_errnos_and_stacks() {
    let directory = ScratchDir::new("syscalls-ndjson");
    let traced = trace(
        directory.path(),
        &[
            "--format",
            "ndjson",
            "-Z",
            "-k",
            "-e",
            "trace=openat",
            "-o",
            "-",
        ],
        "files",
    );

    assert_eq!(traced.status.code(), Some(3));

    let records: Vec<serde_json::Value> = text(&traced.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{line}: {error}")))
        .collect();

    let missing = records
        .iter()
        .find(|record| record["path"] == "missing")
        .unwrap_or_else(|| panic!("no call on missing in {records:?}"));

    assert_eq!(missing["type"], "call");
    assert_eq!(missing["name"], "openat");
    assert_eq!(missing["errno"], "ENOENT");
    assert_eq!(missing["return"], -2);
    assert_eq!(missing["args"]["filename"], "\"missing\"");
    assert!(missing["duration_ns"].is_u64());

    let frames = missing["stack"].as_array().expect("a stack");
    let files = frames
        .iter()
        .find(|frame| frame["symbol"] == "files")
        .unwrap_or_else(|| panic!("no frame in files: {frames:?}"));

    assert_eq!(files["module"], fixture().to_str().expect("path"));
    assert!(
        files["address"]
            .as_str()
            .is_some_and(|address| address.starts_with("0x"))
    );

    assert!(
        files["file"]
            .as_str()
            .is_some_and(|file| file.ends_with("syscalls.c"))
    );

    assert!(files["line"].is_u64());
    assert!(
        records
            .iter()
            .all(|record| record["type"] != "call" || record["errno"].is_string()),
        "-Z shows only failures: {records:?}"
    );

    assert!(
        records
            .iter()
            .any(|record| record["type"] == "exit" && record["code"] == 3)
    );
}

/// A thread other than the leader that executes a program takes the
/// leader's id, and the leader's blocked call ends unfinished: it is
/// reported, not lost.
#[test]
fn a_leaders_blocked_call_ends_when_another_thread_executes() {
    let traced = microscope(&[
        "--engine",
        "ptrace",
        "-f",
        "--format",
        "ndjson",
        "-e",
        "trace=pause",
        "-o",
        "-",
        "--",
    ])
    .arg(Scenario::fixture("thread-exec"))
    .output()
    .expect("run microscope");

    assert_eq!(traced.status.code(), Some(0), "{}", text(&traced.stderr));

    let records: Vec<serde_json::Value> = text(&traced.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{line}: {error}")))
        .collect();

    let pauses: Vec<_> = records
        .iter()
        .filter(|record| record["type"] == "call" && record["name"] == "pause")
        .collect();

    assert_eq!(pauses.len(), 1, "{records:?}");
    assert!(pauses[0]["tid"].is_u64(), "{records:?}");
    assert_eq!(pauses[0]["tid"], pauses[0]["pid"], "{records:?}");
    assert_eq!(pauses[0]["unfinished"], true, "{records:?}");
}

/// A cgroup below the test's own, removed when dropped.
struct TestCgroup(PathBuf);

impl TestCgroup {
    fn new(name: &str) -> Self {
        let own = std::fs::read_to_string("/proc/self/cgroup").expect("the test's cgroup");
        let own = own
            .lines()
            .find_map(|line| line.strip_prefix("0::/"))
            .expect("the unified hierarchy");

        let path = Path::new("/sys/fs/cgroup")
            .join(own)
            .join(format!("{name}-{}", std::process::id()));

        std::fs::create_dir(&path).expect("make a cgroup below the test's own");
        Self(path)
    }

    /// Removes it once its processes have left, failing at the deadline.
    fn remove(&self) {
        let end = Instant::now() + DEADLINE;

        // Polls for an emptiness the kernel reports only so.
        while let Err(error) = std::fs::remove_dir(&self.0) {
            assert!(Instant::now() < end, "the cgroup stayed: {error}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for TestCgroup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
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
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_blocked_call_is_reported_in_flight_until_it_returns() {
    let mut command = microscope(&[
        "--engine",
        "ptrace",
        "-e",
        "trace=read",
        "--in-flight",
        "50ms",
        "--",
    ]);

    command
        .arg(fixture())
        .arg("block")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());

    let mut running = Running::spawn(&mut command);
    let line = running.line(|line| line.contains("in flight"));
    assert!(line.starts_with("read(0, "), "{line}");

    // Closing standard input returns the read, and the program exits.
    drop(running.child.stdin.take());

    let (code, lines) = running.finish();
    assert_eq!(code, Some(0), "{lines:?}");
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("read(0, \"\", 1)") && line.ends_with("= 0")),
        "{lines:?}"
    );
}

#[test]
fn attaching_needs_consent_and_detaching_leaves_the_process_running() {
    let mut target = ExternalProcess::spawn(&Scenario::fixture("attach"));
    let pid = target.process_id().get().to_string();

    let refused = microscope(&["--engine", "ptrace", "-p", &pid])
        .output()
        .expect("run microscope");

    assert!(!refused.status.success());
    let message = text(&refused.stderr);
    assert!(message.contains("--stop-every-call"), "{message}");

    let mut running = Running::spawn(&mut microscope(&[
        "--engine",
        "ptrace",
        "--stop-every-call",
        "-p",
        &pid,
    ]));

    running.line(|line| line == format!("microscope: Process {pid} attached"));
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(running.child.id()).expect("pid")),
        nix::sys::signal::Signal::SIGINT,
    )
    .expect("interrupt microscope");

    let (code, lines) = running.finish();
    assert_eq!(code, Some(0), "{lines:?}");

    assert!(
        lines.contains(&format!("microscope: Process {pid} detached")),
        "{lines:?}"
    );

    // Untraced again, the process runs to its own end.
    target.release();
    let status = target.wait();
    assert_eq!(status.code(), Some(23));
}

#[test]
fn the_bpf_engine_is_the_default_and_never_falls_back() {
    let directory = ScratchDir::new("syscalls-bpf");
    let output = microscope(&["--"])
        .arg(fixture())
        .arg("files")
        .current_dir(directory.path())
        .output()
        .expect("run microscope");
    let message = text(&output.stderr);
    assert!(!output.status.success(), "{message}");

    assert!(
        message.contains("needs CAP_BPF and CAP_PERFMON") && message.contains("--engine ptrace"),
        "{message}"
    );

    assert!(
        !directory.path().join("present").exists(),
        "the program never ran"
    );

    let nothing = microscope(&["--engine", "ptrace"])
        .output()
        .expect("run microscope");

    assert!(text(&nothing.stderr).contains("never traces the whole host"));
    let _ = std::io::stdout().flush();
}

/// A cgroup target is checked before anything is loaded or run.
#[test]
fn a_cgroup_target_is_one_and_named_rightly() {
    for (arguments, wanted) in [
        (
            &["--cgroup", "/tmp"][..],
            "/tmp is not a cgroup below /sys/fs/cgroup",
        ),
        (&["--container", "abc"], "at least 12 hexadecimal digits"),
        (
            &["--cgroup", "/sys/fs/cgroup", "-p", "1"],
            "give one target",
        ),
        (
            &["--cgroup", "/sys/fs/cgroup", "--no-follow"],
            "--no-follow cannot narrow",
        ),
        (
            &["--cgroup", "/sys/fs/cgroup", "-k"],
            "-k cannot unwind a cgroup",
        ),
    ] {
        let output = microscope(arguments).output().expect("run microscope");
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

    fn microscope(arguments: &[&str]) -> Command {
        let mut command = Command::new(launcher());
        command
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .arg("syscalls")
            .args(arguments)
            .stdin(Stdio::null());

        command
    }

    /// What the engines may differ in besides what any two runs do: the
    /// environment and credentials the launcher passes on.
    fn comparable(trace: &str) -> String {
        let vars = Regex::new(r"/\* \d+ vars \*/").expect("regex");
        let ids = Regex::new(r"\b(st_uid|st_gid|si_uid)=\d+").expect("regex");
        let trace = vars.replace_all(trace, "/* vars */");

        normalized(&ids.replace_all(&trace, "$1=ID"))
    }

    /// The fixture's trace in `mode` by `command` with `arguments`, run in
    /// `directory`: its exit code, its errors, and the trace.
    fn traced(
        directory: &Path,
        mut command: Command,
        arguments: &[&str],
        mode: &str,
    ) -> (Option<i32>, String, String) {
        let out = directory.join("trace.out");

        // A launcher that raises capabilities passes on no loader
        // variables, so no run gets any.
        let output = command
            .env_clear()
            .args(arguments)
            .arg("-o")
            .arg(&out)
            .arg("--")
            .arg(fixture())
            .arg(mode)
            .current_dir(directory)
            .output()
            .expect("run microscope");

        let read = std::fs::read_to_string(&out).unwrap_or_default();
        (output.status.code(), text(&output.stderr), read)
    }

    /// The fixture's trace in `mode` by each engine, with `arguments`: the
    /// ptrace engine's, then the BPF engine's.
    fn engines(arguments: &[&str], mode: &str) -> (String, String) {
        let directory = ScratchDir::new("syscalls-engines");
        let directory = directory.path();
        let (ptrace_code, ptrace_errors, ptrace) = traced(
            directory,
            super::microscope(&["--engine", "ptrace"]),
            arguments,
            mode,
        );

        let (bpf_code, bpf_errors, bpf) = traced(directory, microscope(&[]), arguments, mode);
        assert_eq!(
            ptrace_code, bpf_code,
            "exit codes differ: ptrace said {ptrace_errors}, BPF said {bpf_errors}"
        );
        (ptrace, bpf)
    }

    /// Each thread's lines, in order, apart from how threads interleave.
    fn threads(trace: &str) -> BTreeSet<Vec<String>> {
        // A file's lines begin with the thread's id once there are two.
        let prefix = Regex::new(r"^(\d+) +").expect("regex");
        let mut threads: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut thread = String::new();

        for line in trace.lines() {
            // Losses are left out, as `normalized` says why.
            if prefix.replace(line, "").starts_with("--- lost ") {
                continue;
            }

            // A stack's frames follow their call's line, unprefixed.
            if !line.starts_with(" > ") {
                thread = prefix
                    .captures(line)
                    .map(|captures| captures[1].to_owned())
                    .unwrap_or_default();
            }

            threads
                .entry(thread.clone())
                .or_default()
                .push(comparable(&prefix.replace(line, "")));
        }

        threads.into_values().collect()
    }

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn the_engines_trace_a_program_alike() {
        for arguments in [
            &["-yy"][..],
            &["-e", "trace=%file,%desc", "-y"],
            &["-X", "raw"],
            &[
                "-X",
                "verbose",
                "-e",
                "trace=openat,fstat,rseq,prlimit64,pipe2",
            ],
        ] {
            let (ptrace, bpf) = engines(arguments, "files");
            assert_same(
                &comparable(&bpf),
                &comparable(&ptrace),
                &format!("with {arguments:?}"),
            );
        }

        let (ptrace, bpf) = engines(&["-e", "trace=getpid@32,getpid@x32,getpid"], "compat");
        assert_same(&comparable(&bpf), &comparable(&ptrace), "for compat");
        let (ptrace, bpf) = engines(&["-e", "trace=kill,rt_sigaction"], "signal");
        assert_same(&comparable(&bpf), &comparable(&ptrace), "for signals");
    }

    /// A short list attaches to its calls' kernel functions, which see the
    /// calls the tracepoints do; only the tracepoints, which every call on
    /// the host passes, say what they cost.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn functions_see_what_tracepoints_see_and_only_tracepoints_charge_everyone() {
        let directory = ScratchDir::new("syscalls-attach");
        let directory = directory.path();
        let chosen = ["-e", "trace=openat,read,write,close,unlink"];

        let by = |attach: &str| {
            let (code, errors, trace) = traced(
                directory,
                microscope(&["--attach", attach]),
                &chosen,
                "files",
            );
            assert_eq!(code, Some(3), "{errors}");
            (errors, trace)
        };

        let (fentry_errors, fentry) = by("fentry");
        let (tracepoint_errors, tracepoint) = by("tracepoint");

        assert_same(&comparable(&fentry), &comparable(&tracepoint), "by attach");
        assert!(!fentry_errors.contains("33 ns"), "{fentry_errors}");

        assert!(
            tracepoint_errors.contains("tracing through tp_btf (asked for)"),
            "{tracepoint_errors}"
        );

        // A call no function describes cannot be had through functions.
        let (code, errors, _) = traced(
            directory,
            microscope(&["--attach", "fentry"]),
            &["-e", "trace=getpid@32"],
            "compat",
        );

        assert_eq!(code, Some(1), "{errors}");
        assert!(
            errors.contains("--attach fentry cannot be used"),
            "{errors}"
        );
    }

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn noreturn_calls_need_only_entry_and_sigreturn_still_returns() {
        let directory = ScratchDir::new("syscalls-noreturn");
        for (filter, mode, wanted) in [
            ("exit_group", "signal", "exit_group(0)"),
            ("exit", "family", "exit(0)"),
            ("read,exit_group", "dumps", "read("),
            ("rt_sigreturn", "signal", "rt_sigreturn("),
        ] {
            for attach in ["auto", "fentry", "tracepoint"] {
                let (code, errors, trace) = traced(
                    directory.path(),
                    microscope(&["--attach", attach, "-f"]),
                    &["-e", &format!("trace={filter}")],
                    mode,
                );
                if filter == "rt_sigreturn" && attach == "fentry" && code != Some(0) {
                    assert!(
                        errors.contains("rt_sigreturn has no kernel function"),
                        "{errors}"
                    );
                    continue;
                }
                assert_ne!(code, Some(2), "{filter}, {attach}: {errors}");
                assert!(
                    trace.contains(wanted),
                    "{filter}, {attach}: {errors}{trace}"
                );
                if filter == "rt_sigreturn" {
                    assert!(!trace.contains("= ?"), "{trace}");
                } else if filter.contains("exit_group") {
                    assert!(trace.contains("exit_group(0)"), "{trace}");
                    assert!(trace.contains("= ?"), "{trace}");
                }
            }
        }

        for filter in ["exit", "exit_group"] {
            let mut summaries = Vec::new();
            for attach in ["auto", "tracepoint"] {
                let (code, errors, trace) = traced(
                    directory.path(),
                    microscope(&["--attach", attach, "-f", "-c", "-U", "name,calls"]),
                    &["-e", &format!("trace={filter}")],
                    "family",
                );
                assert_eq!(code, Some(0), "{errors}");
                let row = trace
                    .lines()
                    .find(|line| line.starts_with(filter))
                    .expect("a counted call");
                assert!(!row.ends_with(" 0"), "{trace}");
                summaries.push(row.to_owned());
            }
            assert_eq!(summaries[0], summaries[1]);
        }
    }

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_failed_attachment_prints_no_summary() {
        let output = microscope(&[
            "-c",
            "--attach",
            "fentry",
            "-e",
            "trace=getpid@32",
            "--",
            "/bin/true",
        ])
        .output()
        .expect("run microscope");
        let errors = text(&output.stderr);
        assert!(!output.status.success(), "{errors}");
        assert!(
            errors.contains("--attach fentry cannot be used"),
            "{errors}"
        );
        assert!(!errors.contains("% time"), "{errors}");
        assert!(!errors.contains("total"), "{errors}");
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
        let (ptrace, bpf) = engines(&["-yy", "-e", "signal=!SIGCHLD"], "family");
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

    /// Each call's line and the lines of its stack.
    fn stacks(trace: &str) -> Vec<(String, Vec<String>)> {
        let mut calls: Vec<(String, Vec<String>)> = Vec::new();
        for line in comparable(trace).lines() {
            match calls.last_mut() {
                Some((_, frames)) if line.starts_with(" > ") => frames.push(line.to_owned()),
                _ => calls.push((line.to_owned(), Vec::new())),
            }
        }

        calls
    }

    /// `record --syscalls` traces with the BPF engine beside the samples:
    /// the calls a ptrace trace finds, each with its stack.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_recording_holds_its_calls_beside_its_samples() {
        let directory = ScratchDir::new("syscalls-record");
        let directory = directory.path();

        // Each call by number, and whether it failed: what two runs share.
        let calls = |file: &str| {
            let profile = microscope::profiler::Profile::read(&directory.join(file))
                .expect("the capture reads")
                .profile;
            let mut calls = BTreeMap::new();
            for event in profile.syscalls().events {
                if let microscope::syscalls::TraceEvent::Call(call) = &event.event {
                    assert!(event.stack.is_some(), "{call:?} has no stack");
                    let failed = call.outcome.raw().is_some_and(|ret| ret < 0);
                    *calls.entry((call.call.number, failed)).or_insert(0) += 1;
                }
            }

            calls
        };

        let traced = super::microscope(&["--engine", "ptrace", "-f", "-k"])
            .args(["-e", "trace=write,getppid", "-o", "traced.usprof", "--"])
            .arg(fixture())
            .arg("family")
            .current_dir(directory)
            .output()
            .expect("run microscope syscalls");

        assert!(
            directory.join("traced.usprof").exists(),
            "{}",
            text(&traced.stderr)
        );

        let recorded = Command::new(launcher())
            .env_clear()
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .args([
                "record",
                "--syscalls",
                "write,getppid",
                "-o",
                "recorded.usprof",
            ])
            .arg("--")
            .arg(fixture())
            .arg("family")
            .current_dir(directory)
            .stdin(Stdio::null())
            .output()
            .expect("run microscope record");

        assert_eq!(
            recorded.status.code(),
            traced.status.code(),
            "{}",
            text(&recorded.stderr)
        );

        assert_eq!(calls("recorded.usprof"), calls("traced.usprof"));

        let report = Command::new(env!("CARGO_BIN_EXE_microscope"))
            .args(["report", "recorded.usprof"])
            .current_dir(directory)
            .output()
            .expect("run microscope report");

        let lines = text(&report.stdout);
        assert!(lines.contains(" samples, "), "{lines}");
        assert!(lines.contains(" system calls, "), "{lines}");
    }

    /// Copied and unwound by call-frame information, the BPF engine's
    /// stacks are the ptrace engine's, frame for frame. Walked by frame
    /// pointers alone, each is the start of the ptrace engine's, and says
    /// why it ends where it is shorter: never a frame the other lacks.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn the_engines_unwind_stacks_alike() {
        let arguments = [
            "-e",
            "trace=write,read,readv,getppid",
            "-e",
            "signal=!SIGCHLD",
        ];

        for mode in ["dumps", "family", "signal"] {
            let directory = ScratchDir::new("syscalls-stacks");
            let directory = directory.path();
            let (_, errors, ptrace) = traced(
                directory,
                super::microscope(&["--engine", "ptrace", "-kk"]),
                &arguments,
                mode,
            );

            assert!(ptrace.contains(" > "), "{errors}{ptrace}");

            let (_, errors, copied) =
                traced(directory, microscope(&["-k=dwarf", "-k"]), &arguments, mode);

            let (ours, theirs) = (threads(&copied), threads(&ptrace));
            assert_eq!(
                ours,
                theirs,
                "copied in {mode}: {errors}\nonly BPF:\n{:#?}\nonly ptrace:\n{:#?}",
                ours.difference(&theirs).collect::<Vec<_>>(),
                theirs.difference(&ours).collect::<Vec<_>>()
            );

            let (_, errors, walked) = traced(directory, microscope(&["-kk"]), &arguments, mode);
            let (walked, whole) = (stacks(&walked), stacks(&ptrace));
            assert_eq!(walked.len(), whole.len(), "walked in {mode}: {errors}");
            for ((call, frames), (whole_call, whole_frames)) in walked.iter().zip(&whole) {
                assert_eq!(call, whole_call);
                let named = frames
                    .iter()
                    .filter(|frame| !frame.starts_with(" > [stack ends: "))
                    .count();

                assert!(
                    whole_frames.starts_with(&frames[..named])
                        && (named == whole_frames.len() || frames.len() == named + 1),
                    "walked in {mode}, {call}:\n{}\n---\n{}",
                    frames.join("\n"),
                    whole_frames.join("\n")
                );
            }
        }
    }

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn frame_pointer_breaks_name_the_module_and_suggest_dwarf() {
        for (mode, count) in [("stacks", 32), ("thread-stacks", 64)] {
            for method in ["-k", "-k=dwarf"] {
                let output = microscope(&["-f", method, "-e", "trace=getppid", "--"])
                    .arg(Scenario::fixture("syscalls-rust-release"))
                    .arg(mode)
                    .output()
                    .expect("run microscope");
                let trace = text(&output.stderr);
                assert!(output.status.success(), "{trace}");
                assert_eq!(trace.matches("getppid()").count(), count, "{trace}");
                if method == "-k" {
                    assert!(!trace.contains("stack copy exhausted"), "{trace}");
                    assert_eq!(
                        trace.matches("frame pointer chain broken in ").count(),
                        count,
                        "{trace}"
                    );
                    assert!(
                        trace.contains(&format!("{count} stacks ended early because ")),
                        "{trace}"
                    );
                    assert!(trace.contains("syscalls-rust-release doesn't keep frame pointers; -k=dwarf unwinds them"), "{trace}");
                    assert_eq!(trace.matches("-k=dwarf unwinds them").count(), 1, "{trace}");
                } else {
                    assert!(trace.contains("syscalls::main"), "{trace}");
                    assert!(!trace.contains("frame pointer chain broken"), "{trace}");
                    assert!(!trace.contains("-k=dwarf unwinds them"), "{trace}");
                }
            }
        }

        let output = microscope(&["-k", "-e", "trace=read", "--"])
            .arg(fixture())
            .arg("dumps")
            .output()
            .expect("run microscope");
        let trace = text(&output.stderr);
        assert!(output.status.success(), "{trace}");
        assert!(!trace.contains("stack copy exhausted"), "{trace}");
        assert!(trace.contains("frame pointer chain broken in "), "{trace}");
    }

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn dwarf_stack_copy_exhaustion_is_not_a_frame_pointer_break() {
        let output = microscope(&["-k=dwarf", "-e", "trace=getppid", "--"])
            .arg(Scenario::fixture("syscalls-rust-release"))
            .arg("deep-stack")
            .output()
            .expect("run microscope");
        let trace = text(&output.stderr);
        assert!(output.status.success(), "{trace}");
        assert!(trace.contains("stack copy exhausted"), "{trace}");
        assert!(!trace.contains("frame pointer chain broken"), "{trace}");
        assert!(!trace.contains("-k=dwarf unwinds them"), "{trace}");
    }

    /// The kernel counts what `-c` alone shows, which the ptrace engine
    /// counts from every call it sees.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn the_kernel_counts_calls_as_the_ptrace_engine_does() {
        let row =
            Regex::new(r"^\s*[\d.]+\s+[\d.]+\s+\d+\s+(\d+)\s+(\d+\s+)?(\S+)$").expect("regex");

        let rows = |summary: &str| -> Vec<String> {
            summary
                .lines()
                .filter_map(|line| {
                    let captures = row.captures(line)?;
                    Some(format!(
                        "{} {} {}",
                        &captures[3],
                        &captures[1],
                        captures.get(2).map_or("", |errors| errors.as_str().trim())
                    ))
                })
                .collect()
        };

        for mode in ["files", "family"] {
            let (ptrace, bpf) = engines(&["-c", "-S", "name"], mode);
            assert!(!rows(&ptrace).is_empty(), "{ptrace}");
            assert_eq!(
                rows(&bpf),
                rows(&ptrace),
                "in {mode}:\n{bpf}\n---\n{ptrace}"
            );
        }
    }

    /// A getppid takes the BPF engine well under a microsecond, which
    /// strace's table shows as 0 and `--syscall-times=ns` shows as it is.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_call_under_a_microsecond_counts_its_nanoseconds() {
        let directory = ScratchDir::new("syscalls-bpf-ns");
        let output = microscope(&[
            "-c",
            "--syscall-times=ns",
            "-e",
            "trace=getppid",
            "-U",
            "name,calls,min-time,p50",
            "--",
        ])
        .env_clear()
        .arg(fixture())
        .args(["churn", "20"])
        .current_dir(directory.path())
        .output()
        .expect("run microscope");
        let table = text(&output.stderr);
        assert_eq!(output.status.code(), Some(0), "{table}");
        let row = table
            .lines()
            .find(|line| line.starts_with("getppid "))
            .unwrap_or_else(|| panic!("no getppid row:\n{table}"));
        let numbers: Vec<u64> = row
            .split_whitespace()
            .skip(1)
            .map(|cell| cell.parse().expect("a number"))
            .collect();
        let [calls, min, p50] = numbers[..] else {
            panic!("{row:?}");
        };
        assert_eq!(calls, 20, "{table}");
        assert!((1..1_000).contains(&min), "min {min} ns:\n{table}");
        assert!(p50 > 0, "{table}");
    }

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_blocked_call_is_reported_in_flight_while_the_program_runs() {
        let mut command = microscope(&["-e", "trace=read", "--in-flight", "50ms", "--"]);
        command
            .arg(fixture())
            .arg("block")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());

        let mut running = Running::spawn(&mut command);
        let line = running.line(|line| line.contains("in flight"));
        assert!(line.starts_with("read(0, "), "{line}");

        drop(running.child.stdin.take());

        let (code, lines) = running.finish();
        assert_eq!(code, Some(0), "{lines:?}");

        // Its return is seen, or the trace says it lost what it could not
        // read: the kernel skips a program's run on a CPU where another run
        // of it was preempted, and fails to take its task storage lock where
        // another program holds it.
        assert!(
            lines.iter().any(|line| {
                (line.starts_with("read(0, \"\", 1)") && line.ends_with("= 0"))
                    || line.starts_with("--- lost ")
            }),
            "{lines:?}"
        );
    }

    /// A cgroup is traced as it is, with ids as its processes see them, and
    /// the trace lasts as long as it does. Its process is alone in a pid
    /// namespace, where it is 1 and its child 2; its parent, outside the
    /// cgroup, is not traced.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_cgroup_is_traced_as_its_processes_see_themselves() {
        let mut fixture = Command::new(fixture())
            .arg("namespaced")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start the fixture");

        let mut stdout = BufReader::new(fixture.stdout.take().expect("stdout"));
        let mut line = String::new();
        stdout.read_line(&mut line).expect("the child's pid");

        let child = line
            .trim()
            .strip_prefix("child ")
            .expect("a pid")
            .to_owned();

        let cgroup = TestCgroup::new("microscope-traced");
        std::fs::write(cgroup.0.join("cgroup.procs"), &child).expect("move the child");

        let path = cgroup.0.to_str().expect("a path").to_owned();
        let mut running = Running::spawn(&mut microscope(&[
            "--cgroup",
            &path,
            "-e",
            "trace=read,getpid",
        ]));

        running.line(|line| line == format!("microscope: Tracing cgroup {path}"));

        let mut input = fixture.stdin.take().expect("stdin");
        input.write_all(b"x").expect("write a byte");
        drop(input);

        assert!(fixture.wait().expect("wait").success());
        cgroup.remove();

        let (code, lines) = running.finish();
        assert_eq!(code, Some(0), "{lines:?}");

        // Once there are two, each line names its process.
        let named = Regex::new(r"^\[pid +(\d+)\] ").expect("regex");

        let pids: BTreeSet<&str> = lines
            .iter()
            .filter_map(|line| Some(named.captures(line)?.get(1)?.as_str()))
            .collect();
        assert_eq!(pids, BTreeSet::from(["1", "2"]), "{lines:?}");

        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("[pid     2] getpid() ") && line.ends_with("= 2")),
            "{lines:?}"
        );

        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("read(0, \"x\", 1)")),
            "{lines:?}"
        );
    }

    /// Over its budget, a trace gives up detail a step a second; once it
    /// samples, the kernel counts the calls it does not send, so the table
    /// still counts every one.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn over_budget_a_trace_samples_and_still_counts_every_call() {
        let mut command = microscope(&[
            "--budget",
            "0.0001",
            "-e",
            "trace=getppid",
            "-C",
            "-U",
            "name,calls",
            "--",
        ]);

        command
            .arg(fixture())
            .arg("spin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());

        let mut running = Running::spawn(&mut command);
        running.line(|line| line.contains("budget: over budget") && line.contains("cut at 16"));
        running.line(|line| line.contains("budget: over budget") && line.contains("one call in 2"));

        running
            .child
            .stdin
            .take()
            .expect("stdin")
            .write_all(b"x")
            .expect("end the spin");

        let mut asked = String::new();
        BufReader::new(running.child.stdout.take().expect("stdout"))
            .read_line(&mut asked)
            .expect("the count");

        let asked = asked
            .trim()
            .strip_prefix("asked ")
            .expect("a count")
            .to_owned();

        let (code, lines) = running.finish();
        assert_eq!(code, Some(0), "{lines:?}");

        let asked: u64 = asked.parse().expect("a number");

        let shown = lines
            .iter()
            .filter(|line| line.starts_with("getppid()"))
            .count() as u64;

        assert!(shown < asked, "{shown} of {asked}");

        // Before it sampled, the ring overflowed, and what it lost is said.
        let lost: u64 = lines
            .iter()
            .filter_map(|line| {
                line.strip_prefix("--- lost ")?
                    .split(' ')
                    .next()?
                    .parse::<u64>()
                    .ok()
            })
            .sum();

        let table = lines
            .iter()
            .find_map(
                |line| match line.split_whitespace().collect::<Vec<_>>()[..] {
                    ["getppid", calls] => calls.parse::<u64>().ok(),
                    _ => None,
                },
            )
            .expect("a row");

        assert!(
            table <= asked && asked <= table + lost,
            "{table} counted and {lost} lost of {asked}"
        );
    }

    /// Counted continuously, every segment holds its own span's calls: the
    /// kernel counts into one half of its statistics while microscope reads and
    /// clears the other, so the segments together count every call.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn continuous_counts_split_into_segments_that_sum_to_every_call() {
        let directory = ScratchDir::new("syscalls-continuous");
        let segments = directory.path().join("segments");

        // Made here, so that the test can clean up what a launcher running
        // as another user writes in it.
        std::fs::create_dir(&segments).expect("make the directory");

        // Relative to the working directory, which a launcher shares.
        let mut command = microscope(&[
            "-c",
            "--continuous",
            "segments",
            "--segment",
            "200ms",
            "-e",
            "trace=getppid",
            "--",
        ]);
        command
            .arg(fixture())
            .arg("spin")
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        let mut running = Running::spawn(&mut command);

        let written = || {
            std::fs::read_dir(&segments).map_or(0, |entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "usprof"))
                    .count()
            })
        };

        let end = Instant::now() + DEADLINE;

        // Polls for files the trace writes and says nothing of.
        while written() < 3 {
            assert!(Instant::now() < end, "only {} segments", written());
            std::thread::sleep(Duration::from_millis(10));
        }

        running
            .child
            .stdin
            .take()
            .expect("stdin")
            .write_all(b"x")
            .expect("end the spin");
        let mut asked = String::new();
        BufReader::new(running.child.stdout.take().expect("stdout"))
            .read_line(&mut asked)
            .expect("the count");
        let asked = asked
            .trim()
            .strip_prefix("asked ")
            .expect("a count")
            .to_owned();

        let (code, lines) = running.finish();
        assert_eq!(code, Some(0), "{lines:?}");

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

    /// A call that fails as a trigger names saves the calls before it.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_failing_call_saves_the_window_before_it() {
        let directory = ScratchDir::new("syscalls-trigger");
        std::fs::create_dir_all(directory.path().join("segments/windows"))
            .expect("make the directories");
        let output = microscope(&[
            "--continuous",
            "segments",
            "--trigger-errno",
            "EBADF",
            "-e",
            "trace=write,close",
            "--",
        ])
        .arg(fixture())
        .arg("fails")
        .current_dir(directory.path())
        .output()
        .expect("run microscope");
        let message = text(&output.stderr);
        assert_eq!(output.status.code(), Some(0), "{message}");

        let saved = message
            .lines()
            .find_map(|line| {
                line.strip_prefix("microscope: close failed with errno 9: saved the last ")?
                    .split_once(" calls to ")
            })
            .unwrap_or_else(|| panic!("{message}"));

        let window = Command::new(env!("CARGO_BIN_EXE_microscope"))
            .arg("report")
            .arg(directory.path().join(saved.1))
            .output()
            .expect("run microscope report");
        let table = text(&window.stdout);
        let calls = |name: &str| {
            table.lines().find_map(|line| {
                let cells: Vec<&str> = line.split_whitespace().collect();
                (cells.last() == Some(&name)).then(|| cells[cells.len() - 2].to_owned())
            })
        };

        // The window holds the write before the failed close.
        assert_eq!(calls("write").as_deref(), Some("1"), "{table}");
        assert!(calls("close").is_some(), "{table}");
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
                "--syscalls",
                "read",
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
            found.push((byte, syscalls.lost));
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
    /// ring by default, however little of it the reader drains meanwhile:
    /// every call is shown, each with its stack.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn a_burst_of_copied_stacks_loses_no_calls() {
        // 6,600 calls in about 10 ms, whose records take some 57 MB: more
        // than three times what a 16 MiB ring holds, and less than 64 MiB.
        let rounds = 2_200;
        let directory = ScratchDir::new("syscalls-burst");
        let directory = directory.path();
        let output = microscope(&["-k=dwarf", "--attach", "tracepoint"])
            .env_clear()
            .args(["-e", "trace=openat,read,close", "-o", "trace.out", "--"])
            .arg(Scenario::fixture("syscall-load"))
            .args(["1", &rounds.to_string()])
            .current_dir(directory)
            .output()
            .expect("run microscope");

        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        let trace = std::fs::read_to_string(directory.join("trace.out")).expect("the trace");
        let calls = stacks(&trace);
        let lost: Vec<&str> = trace
            .lines()
            .filter(|line| line.starts_with("--- lost "))
            .collect();

        // A call whose stack was lost says so: `[stack ends: no stack]`.
        let bare: Vec<&String> = calls
            .iter()
            .filter(|(call, frames)| frames.is_empty() && !call.starts_with("+++ "))
            .map(|(call, _)| call)
            .collect();
        assert!(bare.is_empty(), "calls without stacks: {bare:?}");

        // The fixture's calls, and not the loader's, are made in one_round.
        let shown = calls
            .iter()
            .filter(|(_, frames)| frames.iter().any(|frame| frame.contains("(one_round+")))
            .count();
        let opens = calls
            .iter()
            .filter(|(call, _)| call.starts_with("openat(AT_FDCWD, \"/dev/zero\""))
            .count();
        assert_eq!(
            (shown, opens),
            (rounds * 3, rounds),
            "calls shown with their stacks, and opens; {lost:?}"
        );
    }

    /// Attaching needs no consent, since the process never stops: it runs
    /// on, traced, to its own end, which ends the trace.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON; run with just test-bpf"]
    fn an_attached_process_runs_on_traced_to_its_end() {
        let mut target = ExternalProcess::spawn(&Scenario::fixture("attach"));
        let pid = target.process_id().get().to_string();
        let mut running = Running::spawn(&mut microscope(&["-p", &pid]));
        running.line(|line| line == format!("microscope: Process {pid} attached"));
        target.release();

        // The read began before the trace did.
        running.line(|line| line.starts_with("read(0, \"x\", 1)") && line.contains("entry unseen"));
        running.line(|line| line.starts_with("exit_group(23)"));
        let (code, lines) = running.finish();
        assert_eq!(code, Some(0), "{lines:?}");
        assert!(
            lines.contains(&"+++ exited with 23 +++".to_owned()),
            "{lines:?}"
        );
        target.reaped();
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
    let output = microscope(&["--engine", "ptrace", "-f", "-o", "churn.usprof", "--"])
        .arg(fixture())
        .args(["churn", &count.to_string()])
        .current_dir(scratch.path())
        .output()
        .expect("run microscope");
    assert!(output.status.success(), "{}", text(&output.stderr));
    let read = microscope::profiler::Profile::read(&scratch.path().join("churn.usprof"))
        .expect("read the trace");
    let events = read.profile.syscalls().events;
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
