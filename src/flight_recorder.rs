//! A record of what the debugger did, kept by development builds only.
//!
//! Every client request, native control call, wait status, classification,
//! and published event is appended to a bounded in-memory ring as one line,
//! stamped with the time since the first record and the thread that made it.
//! The test harness writes the ring to a file when a test fails, and the
//! `microscope` binary streams every line to a file as it is recorded, so the
//! recording survives a killed process. Release builds compile none of this.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::fs::{self, File};
use std::io::{self, Seek as _, SeekFrom, Write as _};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock, PoisonError};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// How many lines the ring keeps before dropping the oldest.
const CAPACITY: usize = 16 * 1024;

/// The longest message a line keeps after its time and thread; longer ones,
/// such as large event payloads, are cut without formatting the rest.
const MAX_LINE: usize = 1024;

/// How large a streamed recording may grow before it restarts from the ring.
const MAX_STREAM_BYTES: u64 = 64 * 1024 * 1024;

/// How many recordings of past runs [`stream_run`] keeps.
const KEPT_RUNS: usize = 20;

struct Recorder {
    lines: VecDeque<String>,
    dropped: u64,
    stream: Option<Stream>,
}

struct Stream {
    path: PathBuf,
    file: File,
    written: u64,

    /// How many times the stream reached its size limit and restarted.
    restarts: u64,
}

static RECORDER: Mutex<Recorder> = Mutex::new(Recorder {
    lines: VecDeque::new(),
    dropped: 0,
    stream: None,
});
static START: OnceLock<Instant> = OnceLock::new();

thread_local! {
    /// Whether this thread holds the recorder, so a panic while it does
    /// cannot wait for it.
    static HOLDING: Cell<bool> = const { Cell::new(false) };

    /// The lines this thread captures instead of recording to the ring.
    static CAPTURED: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

fn with_recorder<T>(change: impl FnOnce(&mut Recorder) -> T) -> T {
    struct Holding;

    impl Drop for Holding {
        fn drop(&mut self) {
            HOLDING.set(false);
        }
    }

    // Pushing a line cannot leave the ring inconsistent.
    let mut recorder = RECORDER.lock().unwrap_or_else(PoisonError::into_inner);
    HOLDING.set(true);
    let _holding = Holding;
    change(&mut recorder)
}

/// Appends one line. Use the crate's `record!` macro, which compiles away in
/// release builds.
pub(crate) fn record(message: fmt::Arguments<'_>) {
    let mut body = Bounded(String::new());
    if body.write_fmt(message).is_err() {
        body.0.push_str(" …");
    }

    let mut body = Some(body.0);
    CAPTURED.with_borrow_mut(|captured| {
        if let Some(lines) = captured {
            lines.extend(body.take());
        }
    });
    let Some(body) = body else {
        return;
    };

    let current = thread::current();
    let thread = current.name().map_or("unnamed", |name| {
        name.strip_prefix("microscope-").unwrap_or(name)
    });
    with_recorder(|recorder| recorder.push(thread, &body));
}

/// A message that refuses text past [`MAX_LINE`], which stops formatting
/// there.
struct Bounded(String);

impl fmt::Write for Bounded {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let room = MAX_LINE.saturating_sub(self.0.len());
        if text.len() <= room {
            self.0.push_str(text);
            return Ok(());
        }

        let mut end = room;
        while !text.is_char_boundary(end) {
            end -= 1;
        }

        self.0.push_str(&text[..end]);
        Err(fmt::Error)
    }
}

impl Recorder {
    /// Stamps `body` while holding the recorder, so time never runs backward
    /// from one line to the next, and appends it.
    fn push(&mut self, thread: &str, body: &str) {
        let elapsed = START.get_or_init(Instant::now).elapsed();
        let mut line = String::with_capacity(body.len() + 24);
        let _ = writeln!(line, "{:>11.6} {thread:<10} {body}", elapsed.as_secs_f64());
        if self.lines.len() == CAPACITY {
            self.lines.pop_front();
            self.dropped += 1;
        }

        self.lines.push_back(line);
        self.stream();
    }

    /// Writes the newest line to the stream, or, once the stream reaches its
    /// size limit, rewrites it with the ring so it ends with the newest lines.
    fn stream(&mut self) {
        let Some(mut stream) = self.stream.take() else {
            return;
        };

        let line = self.lines.back().map_or("", String::as_str);
        if stream.written + line.len() as u64 <= MAX_STREAM_BYTES {
            // One write per line, so a killed process loses nothing it
            // recorded.
            let _ = stream.file.write_all(line.as_bytes());
            stream.written += line.len() as u64;
        } else {
            stream.restarts += 1;
            let text = self.contents(&stream.path, stream.restarts);
            let rewritten = stream
                .file
                .set_len(0)
                .and_then(|()| stream.file.seek(SeekFrom::Start(0)))
                .and_then(|_| stream.file.write_all(text.as_bytes()));
            stream.written = text.len() as u64;
            if rewritten.is_err() {
                // Stop rather than leave a file that silently lacks lines.
                return;
            }
        }

        self.stream = Some(stream);
    }

    fn contents(&self, path: &Path, restarts: u64) -> String {
        let mut text = format!(
            "# microscope flight recording {}\n# process {}, written at unix time {}\n",
            path.display(),
            std::process::id(),
            unix_seconds(),
        );
        if restarts > 0 {
            let _ = writeln!(
                text,
                "# the recording reached its size limit {restarts} times and kept the newest lines",
            );
        }

        if self.dropped > 0 {
            let _ = writeln!(text, "# the ring dropped the {} oldest lines", self.dropped);
        }

        text.push_str("# each line: seconds since the first record, thread, what happened\n");
        for line in &self.lines {
            text.push_str(line);
        }

        text
    }
}

/// The directory recordings are written under: `target/flight-recorder` of
/// the source tree this build came from, which git ignores.
#[must_use]
pub fn directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target/flight-recorder")
}

/// Writes the lines the ring holds to `path`, replacing any file there.
pub fn dump(path: &Path) -> io::Result<()> {
    let text = with_recorder(|recorder| recorder.contents(path, 0));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(path, text)
}

/// Writes the lines the ring holds to `path`, then every line recorded from
/// now on as it is recorded.
pub fn stream_to(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = File::create(path)?;

    // Holding the ring while writing it keeps any line from falling between
    // the copy and the stream.
    with_recorder(|recorder| {
        let text = recorder.contents(path, 0);
        file.write_all(text.as_bytes())?;
        recorder.stream = Some(Stream {
            path: path.to_owned(),
            file,
            written: text.len() as u64,
            restarts: 0,
        });
        Ok(())
    })
}

/// Streams this process's recording to a new file under `runs/`, points
/// `latest.log` at it, and removes all but the newest recordings of past
/// runs. Returns the new recording's path.
pub fn stream_run() -> io::Result<PathBuf> {
    let runs = directory().join("runs");
    fs::create_dir_all(&runs)?;
    prune(&runs, KEPT_RUNS - 1)?;
    let path = runs.join(format!("{}-{}.log", unix_seconds(), std::process::id()));
    stream_to(&path)?;

    // Replace the link atomically, since concurrent runs race for it.
    let latest = directory().join("latest.log");
    let staged = directory().join(format!(".latest-{}.log", std::process::id()));
    let _ = fs::remove_file(&staged);
    std::os::unix::fs::symlink(&path, &staged)?;
    fs::rename(&staged, &latest)?;

    Ok(path)
}

/// Removes the oldest recordings in `runs` until at most `keep` remain.
/// Names start with a timestamp, so name order is age order.
fn prune(runs: &Path, keep: usize) -> io::Result<()> {
    let mut recordings = fs::read_dir(runs)?
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "log"))
        .collect::<Vec<_>>();
    recordings.sort();
    let excess = recordings.len().saturating_sub(keep);
    for path in &recordings[..excess] {
        // Another run may be pruning the same file.
        let _ = fs::remove_file(path);
    }

    Ok(())
}

/// Records every panic, on any thread, before the previous hook reports it,
/// so a recording shows where a crashed controller stopped. Installs the
/// hook once per process.
pub fn record_panics() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // A panic while this thread records cannot wait for itself.
            if !HOLDING.get() {
                record(format_args!("panic: {info}"));
            }

            previous(info);
        }));
    });
}

/// Keeps the lines this thread records apart from the ring until dropped.
///
/// Captured lines carry no time or thread name, so the same actions always
/// produce the same lines. Other threads record to the ring as before.
pub struct Capture {
    /// Capturing is a property of the thread that started it.
    _thread: PhantomData<*const ()>,
}

impl Capture {
    /// Starts capturing this thread's lines.
    ///
    /// # Panics
    ///
    /// If this thread is already capturing.
    #[must_use]
    pub fn start() -> Self {
        CAPTURED.with_borrow_mut(|captured| {
            assert!(captured.is_none(), "this thread is already capturing");
            *captured = Some(Vec::new());
        });
        Self {
            _thread: PhantomData,
        }
    }

    /// Removes and returns the lines captured since the last call.
    #[must_use]
    pub fn take(&self) -> Vec<String> {
        CAPTURED
            .with_borrow_mut(|captured| captured.as_mut().map(std::mem::take).unwrap_or_default())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        CAPTURED.set(None);
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A capturing thread's lines go to its capture alone, without time or
    /// thread, while other threads keep recording to the ring; once the
    /// capture ends, the thread records to the ring again.
    #[test]
    fn a_capture_takes_only_its_own_threads_lines() {
        let capture = Capture::start();
        record(format_args!("captured {}", 1));
        thread::spawn(|| record(format_args!("from another thread")))
            .join()
            .expect("record from another thread");
        record(format_args!("captured {}", 2));
        assert_eq!(capture.take(), ["captured 1", "captured 2"]);
        assert_eq!(capture.take(), Vec::<String>::new());
        drop(capture);
        record(format_args!("after the capture"));

        let ring = with_recorder(|recorder| recorder.lines.iter().cloned().collect::<Vec<_>>());
        assert!(
            !ring.iter().any(|line| line.contains("captured")),
            "{ring:?}"
        );
        for expected in [" from another thread\n", " after the capture\n"] {
            assert!(ring.iter().any(|line| line.ends_with(expected)), "{ring:?}");
        }
    }

    /// A stream that outgrows its limit restarts from the ring, so it keeps
    /// the newest lines, and a long line stops formatting at the limit.
    #[test]
    fn a_full_stream_keeps_the_newest_lines() {
        let path = std::env::temp_dir().join(format!(
            "microscope-flight-recorder-{}.log",
            std::process::id()
        ));
        stream_to(&path).expect("stream");
        let long = "x".repeat(4 * MAX_LINE);
        let lines = MAX_STREAM_BYTES / MAX_LINE as u64 + CAPACITY as u64;
        for line in 0..lines {
            record(format_args!("{line} {long}"));
        }

        record(format_args!("last"));
        with_recorder(|recorder| recorder.stream = None);

        let contents = fs::read_to_string(&path).expect("read the recording");
        fs::remove_file(&path).expect("remove the recording");
        assert!(
            contents.len() as u64 <= MAX_STREAM_BYTES,
            "{}",
            contents.len()
        );
        assert!(contents.contains("reached its size limit 1 times"));
        let mut recorded = contents.lines().filter(|line| !line.starts_with('#'));
        let first = recorded.next().expect("a recorded line");
        assert!(first.ends_with(" …"), "{first}");
        let kept = first.matches('x').count();
        assert!(kept < MAX_LINE, "{kept}");
        assert!(contents.ends_with(" last\n"));
    }
}
