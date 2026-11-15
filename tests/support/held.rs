use std::io::{BufRead as _, Write as _};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Instant;

use microscope::profiler::{Profile, StackEnding};
use microscope::syscalls::TraceEvent;
use microscope::syscalls::filter::paths;
use serde_json::{Value, json};

use super::released::READY_DEADLINE;

pub fn launcher() -> PathBuf {
    std::env::var_os("MICROSCOPE_BPF_LAUNCHER").map_or_else(
        || {
            panic!(
                "MICROSCOPE_BPF_LAUNCHER names no launcher: run these with `just test-bpf`, \
                 which records through the microscope-bpf wrapper"
            )
        },
        PathBuf::from,
    )
}

pub fn bpf() -> Command {
    let mut command = Command::new(launcher());
    command.arg(env!("CARGO_BIN_EXE_microscope"));
    command
}

pub fn unprivileged() -> Command {
    Command::new(env!("CARGO_BIN_EXE_microscope"))
}

pub fn recorded_until(
    mut microscope: Command,
    program: &Command,
    arguments: &[&str],
    release: usize,
    mut lines: impl FnMut(&Profile) -> usize,
) -> (Profile, String) {
    let scratch = super::ScratchDir::new("held");
    let output = scratch.path().join("held.usprof");
    let mut child = microscope
        .args(["record", "--window", "3600", "--output"])
        .arg(&output)
        .args(arguments)
        .arg("--")
        .arg(program.get_program())
        .args(program.get_args())
        .envs(
            program
                .get_envs()
                .filter_map(|(name, value)| Some((name, value?))),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run microscope");

    let (said, heard) = std::sync::mpsc::channel();
    let stderr = std::io::BufReader::new(child.stderr.take().expect("its errors"));
    let reader = std::thread::spawn(move || {
        let mut all = String::new();
        for line in stderr.lines() {
            let line = line.expect("microscope's errors");
            all.push_str(&line);
            all.push('\n');
            let _ = said.send(line);
        }
        all
    });
    let deadline = Instant::now() + READY_DEADLINE;
    let next = |what: &str| loop {
        let Ok(line) = heard.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        else {
            return None;
        };
        if let Some(rest) = line.strip_prefix(what) {
            return Some(rest.to_owned());
        }
    };

    let pid = next("microscope: keeping the last ")
        .and_then(|rest| {
            let (_, pid) = rest.split_once("`kill -USR2 ")?;
            pid.split_once('`')?.0.parse().ok()
        })
        .map(nix::unistd::Pid::from_raw);
    let mut input = child.stdin.take().expect("its input");
    let mut given = 0;
    while let Some(pid) = pid
        && given < release
    {
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGUSR2).expect("ask for the window");
        let Some(saved) = next("microscope: saved ") else {
            break;
        };
        let path = saved
            .split_once(" samples of the window to ")
            .expect("where the window went")
            .1;
        let window = Profile::read(std::path::Path::new(path))
            .expect("read the window")
            .profile;
        let wanted = lines(&window).min(release);
        while given < wanted {
            let _ = input.write_all(b"go\n");
            given += 1;
        }
    }
    drop(input);

    let status = child.wait().expect("microscope finishes");
    let stderr = reader.join().expect("microscope's errors");
    assert!(status.success(), "{stderr}");
    let profile = Profile::read(&output).expect("read the profile").profile;
    (profile, stderr)
}

pub fn records(profile: &Profile) -> Vec<Value> {
    profile
        .syscalls()
        .events
        .iter()
        .filter_map(|event| {
            let TraceEvent::Call(call) = &event.event else {
                return None;
            };

            let mut stack = Vec::new();
            if let Some(row) = event.stack {
                for frame in profile.syscall_frames(row) {
                    if !frame.interpreted {
                        stack.push(json!({ "symbol": frame.function }));
                        continue;
                    }

                    let mut object = json!({ "function": frame.function });
                    if let Some((file, line)) = frame.place {
                        object["file"] = file.into();
                        if line != 0 {
                            object["line"] = line.into();
                        }
                    }
                    stack.push(object);
                }

                if let StackEnding::Truncated(reason) = profile.syscall_stack(row).1 {
                    stack.push(json!({ "ends": reason }));
                }
            }

            Some(json!({
                "path": paths(call).into_iter().next(),
                "tid": call.thread.get(),
                "stack": stack,
            }))
        })
        .collect()
}
