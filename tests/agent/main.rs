//! `microscope agent` against a fake service: the real binary, connected
//! over a real WebSocket to a server each test scripts.

mod commands;
mod connecting;
mod login;
mod service;
mod uploads;

use std::io::{BufRead as _, BufReader};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long any one thing a test waits for may take.
pub(crate) const DEADLINE: Duration = Duration::from_secs(20);

/// The token tests give the agent.
pub(crate) const TOKEN: &str = "msd_test-token";

/// A directory removed when dropped, which serves as the agent's home.
pub(crate) struct Scratch(PathBuf);

impl Scratch {
    pub(crate) fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "microscope-agent-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn config(&self) -> PathBuf {
        self.0.join("config/microscope")
    }

    pub(crate) fn outbox(&self) -> PathBuf {
        self.0.join("state/microscope/outbox")
    }

    /// Writes agent.toml and a private token, as `agent login` would.
    pub(crate) fn log_in(&self, settings: &str) {
        std::fs::create_dir_all(self.config()).unwrap();
        std::fs::write(self.config().join("agent.toml"), settings).unwrap();
        let token = self.config().join("agent-token");
        std::fs::write(&token, format!("{TOKEN}\n")).unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    /// `microscope` with its user directories here.
    pub(crate) fn microscope(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_microscope"));
        command
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env_remove("MICROSCOPE_CONFIG")
            .stdin(Stdio::null());
        command
    }

    /// Queues `bytes` in the outbox as `agent record --upload` would, and
    /// returns their key in hex.
    pub(crate) fn queue(&self, bytes: &[u8], name: &str) -> String {
        let key = format!("{:032x}", rand_u128());
        std::fs::create_dir_all(self.outbox()).unwrap();
        std::fs::write(self.outbox().join(format!("{key}.name")), name).unwrap();
        std::fs::write(self.outbox().join(format!("{key}.usprof")), bytes).unwrap();
        key
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn rand_u128() -> u128 {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).unwrap();
    u128::from_le_bytes(bytes)
}

/// Bytes that start as a capture file does, which is all the agent checks.
pub(crate) fn capture(len: usize) -> Vec<u8> {
    let mut bytes = b"USCOPEPF\x01\0\0\0\0\0\0\0".to_vec();
    let seed = rand_u128().to_le_bytes();
    bytes.extend((0..len.saturating_sub(bytes.len())).map(|i| seed[i % 16] ^ i.to_le_bytes()[0]));
    bytes
}

/// A running `microscope agent`, killed when dropped.
pub(crate) struct Agent {
    child: Child,
    lines: mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl Agent {
    /// Starts the agent in `scratch` with `args` after `agent`.
    pub(crate) fn start(scratch: &Scratch, args: &[&str]) -> Self {
        let mut child = scratch
            .microscope()
            .arg("agent")
            .args(["--hello-timeout", "2"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            seen: Vec::new(),
        }
    }

    /// Waits for a line of the agent's standard error that holds `text`,
    /// and returns it.
    pub(crate) fn line(&mut self, text: &str) -> String {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) if line.contains(text) => {
                    self.seen.push(line.clone());
                    return line;
                }
                Ok(line) => self.seen.push(line),
                Err(error) => panic!(
                    "the agent never said {text:?} ({error}); it said {:#?}",
                    self.seen
                ),
            }
        }
    }

    /// Waits for the agent to exit, and returns how, with everything it
    /// said.
    pub(crate) fn exit(mut self) -> (ExitStatus, String) {
        let deadline = Instant::now() + DEADLINE;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "the agent never exited: {:#?}",
                self.seen
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        // The reader thread ends with the pipe.
        while let Ok(line) = self.lines.recv_timeout(Duration::from_secs(5)) {
            self.seen.push(line);
        }
        (status, self.seen.join("\n"))
    }

    /// Asks the agent to stop, as Ctrl-C does.
    pub(crate) fn interrupt(&self) {
        let pid = rustix::process::Pid::from_raw(self.child.id().cast_signed()).unwrap();
        rustix::process::kill_process(pid, rustix::process::Signal::INT).unwrap();
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Waits until `done` holds.
pub(crate) fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}
