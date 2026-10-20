//! The commands the service sends: list this user's processes, and record
//! one of them for a while.

use std::os::unix::fs::MetadataExt as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use microscope::profiler::{Preset, RecordOptions, Watch, record_attached};
use tokio::sync::{Notify, oneshot};

use super::outbox::{Key, Outbox};
use super::proto::Process;

/// The longest recording the service may ask for.
pub const MAX_SECONDS: u32 = 600;

/// How many processes a list holds at most, which keeps it well under the
/// largest message.
const MAX_PROCESSES: usize = 4096;

/// How much of a command line a list holds.
const MAX_COMMAND_LINE: usize = 1024;

/// How many recordings may run at once.
const MAX_RECORDINGS: usize = 4;

/// How long a recording may take to start before the command fails. The
/// service waits ten seconds for the answer.
const START_TIMEOUT: Duration = Duration::from_secs(8);

/// The processes this user owns, which are the ones it may record.
pub fn processes() -> Vec<Process> {
    let me = rustix::process::geteuid().as_raw();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };

    let mut processes: Vec<Process> = entries
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let owner = entry.metadata().ok()?.uid();
            (owner == me).then(|| process(pid)).flatten()
        })
        .collect();
    processes.sort_by_key(|process| process.pid);
    processes.truncate(MAX_PROCESSES);
    processes
}

fn process(pid: u32) -> Option<Process> {
    let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let arguments = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let mut command_line: String = arguments
        .split(|&byte| byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(String::from_utf8_lossy)
        .collect::<Vec<_>>()
        .join(" ");
    if let Some((end, _)) = command_line.char_indices().nth(MAX_COMMAND_LINE) {
        command_line.truncate(end);
    }
    Some(Process {
        pid,
        name: name.trim_end().to_owned(),
        command_line,
    })
}

/// The recordings the service started, which go on when the connection
/// that asked for them drops.
#[derive(Debug, Default)]
pub struct Recorder {
    running: Mutex<Vec<Running>>,
}

#[derive(Debug)]
struct Running {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl Recorder {
    /// Starts recording `pid` for `seconds`, then queues the recording in
    /// `outbox` under the returned key and wakes `uploads`. It answers once
    /// the recording has started, or failed to.
    pub async fn start(
        &self,
        pid: u32,
        seconds: u32,
        outbox: &Outbox,
        uploads: &Arc<Notify>,
    ) -> Result<Key, String> {
        if !(1..=MAX_SECONDS).contains(&seconds) {
            return Err(format!("a recording lasts 1 to {MAX_SECONDS} seconds"));
        }
        let owner = std::fs::metadata(format!("/proc/{pid}"))
            .map_err(|_| format!("there is no process {pid}"))?
            .uid();
        if owner != rustix::process::geteuid().as_raw() {
            return Err(format!("process {pid} belongs to another user"));
        }
        let name = process(pid).map_or_else(|| pid.to_string(), |process| process.name);

        let key = Key::random().map_err(|error| error.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let (started, answer) = oneshot::channel();
        {
            let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
            running.retain(|recording| !recording.thread.is_finished());
            if running.len() >= MAX_RECORDINGS {
                return Err(format!(
                    "this machine is already making {MAX_RECORDINGS} recordings"
                ));
            }

            let thread = std::thread::Builder::new()
                .name(format!("record {pid}"))
                .spawn({
                    let stop = Arc::clone(&stop);
                    let outbox = outbox.clone();
                    let uploads = Arc::clone(uploads);
                    move || record(pid, seconds, &stop, key, &name, &outbox, &uploads, started)
                })
                .map_err(|error| format!("starting the recording: {error}"))?;
            running.push(Running {
                stop: Arc::clone(&stop),
                thread,
            });
        }

        match tokio::time::timeout(START_TIMEOUT, answer).await {
            Ok(Ok(Ok(()))) => Ok(key),
            Ok(Ok(Err(error))) => Err(error),
            Ok(Err(_)) => Err("the recording ended before it started".to_owned()),
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err(format!(
                    "the recording did not start within {} s",
                    START_TIMEOUT.as_secs()
                ))
            }
        }
    }

    /// Stops every recording, waiting for each to be queued.
    pub fn stop_all(&self) {
        let running =
            std::mem::take(&mut *self.running.lock().unwrap_or_else(PoisonError::into_inner));
        for recording in &running {
            recording.stop.store(true, Ordering::Relaxed);
        }
        for recording in running {
            let _ = recording.thread.join();
        }
    }
}

#[expect(clippy::too_many_arguments, reason = "a thread's whole state")]
fn record(
    pid: u32,
    seconds: u32,
    stop: &AtomicBool,
    key: Key,
    name: &str,
    outbox: &Outbox,
    uploads: &Notify,
    started: oneshot::Sender<Result<(), String>>,
) {
    let options = RecordOptions::preset(Preset::Development);
    let deadline = std::time::Instant::now() + Duration::from_secs(u64::from(seconds));
    let mut started = Some(started);
    let mut publish = |_| {
        if let Some(started) = started.take() {
            let _ = started.send(Ok(()));
        }
    };

    // The recording stops on its own at its deadline, or early on `stop`.
    let timer_stop = AtomicBool::new(false);
    let recorded = std::thread::scope(|scope| {
        scope.spawn(|| {
            while std::time::Instant::now() < deadline
                && !stop.load(Ordering::Relaxed)
                && !timer_stop.load(Ordering::Relaxed)
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            timer_stop.store(true, Ordering::Relaxed);
        });
        let mut watch = Watch {
            every: Duration::MAX,
            publish: &mut publish,
            stop: &timer_stop,
            save: None,
        };
        let recorded = record_attached(pid, &options, &mut watch);
        timer_stop.store(true, Ordering::Relaxed);
        recorded
    });

    let profile = match recorded {
        Ok(attachment) => attachment.profile,
        Err(error) => {
            if let Some(started) = started.take() {
                let _ = started.send(Err(format!("recording process {pid} failed: {error}")));
            } else {
                eprintln!("microscope: recording process {pid} failed: {error}");
            }
            return;
        }
    };
    drop(started);

    let queued = profile
        .write(&outbox.partial(key))
        .map_err(anyhow::Error::from)
        .and_then(|()| outbox.commit(key, name));
    match queued {
        Ok(()) => {
            eprintln!(
                "microscope: recorded {} samples of process {pid}",
                profile.samples()
            );
            uploads.notify_one();
        }
        Err(error) => {
            eprintln!("microscope: queueing the recording of process {pid} failed: {error:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn processes_include_this_one() {
        let me = std::process::id();
        let list = processes();
        let this = list.iter().find(|process| process.pid == me).unwrap();
        assert!(!this.name.is_empty());
        assert!(!this.command_line.is_empty());
        assert!(list.windows(2).all(|pair| pair[0].pid < pair[1].pid));
    }

    #[test]
    fn processes_of_other_users_are_left_out() {
        // init belongs to root, which tests do not run as.
        if rustix::process::geteuid().is_root() {
            return;
        }
        assert!(processes().iter().all(|process| process.pid != 1));
    }
}
