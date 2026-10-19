//! Keeps the flight recordings of a failing test.
//!
//! A development build records scenarios in this process and DAP sessions
//! in each adapter process. When anything in a test panics, even after its
//! sessions ended, its recordings are kept under the recorder's `tests`
//! directory; otherwise they are discarded. Nextest runs each test in its own
//! process, so everything this process records belongs to one test.

#![allow(
    unused_imports,
    reason = "each test crate uses a subset of the harness"
)]

#[cfg(debug_assertions)]
pub use recording::{adapter_finished, recording_path, watch_adapter, watch_scenarios};

#[cfg(not(debug_assertions))]
pub use stubs::{adapter_finished, watch_adapter, watch_scenarios};

#[cfg(not(debug_assertions))]
mod stubs {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    pub const fn watch_scenarios() {}

    pub const fn watch_adapter(_command: &mut Command) -> Option<PathBuf> {
        None
    }

    pub const fn adapter_finished(_path: &Path) {}
}

#[cfg(debug_assertions)]
mod recording {
    use std::cell::Cell;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Mutex, PoisonError};

    /// Records the scenarios this test runs in this process.
    pub fn watch_scenarios() {
        with(|recordings| recordings.scenarios = true);
    }

    /// Has `command`, an adapter, stream its recording to a file named
    /// after the test, numbered when a test starts several. Returns that
    /// file.
    #[allow(
        clippy::unnecessary_wraps,
        reason = "release builds record nothing and return None"
    )]
    pub fn watch_adapter(command: &mut Command) -> Option<PathBuf> {
        let path = with(|recordings| {
            let path = match recordings.adapters.len() {
                0 => recordings.path(".adapter"),
                count => recordings.path(&format!(".adapter-{}", count + 1)),
            };

            recordings.adapters.push(Adapter {
                path: path.clone(),
                finished: None,
            });
            path
        });
        command.env("MICROSCOPE_FLIGHT_RECORDING", &path);
        Some(path)
    }

    /// Takes the recording of an adapter that exited out of the directory,
    /// to be written back only if the test fails later.
    pub fn adapter_finished(path: &Path) {
        with(|recordings| {
            if recordings.kept {
                return;
            }

            let Some(adapter) = recordings
                .adapters
                .iter_mut()
                .find(|adapter| adapter.path == path)
            else {
                return;
            };

            if let Ok(contents) = fs::read(path) {
                let _ = fs::remove_file(path);
                adapter.finished = Some(contents);
            }
        });
    }

    /// Where a failing test keeps a recording: `""` for the scenarios it
    /// ran here, `".adapter"` for its first adapter.
    pub fn recording_path(suffix: &str) -> PathBuf {
        with(|recordings| recordings.path(suffix))
    }

    struct Recordings {
        /// The path every recording of this test starts with.
        stem: PathBuf,

        /// Whether a scenario ran in this process.
        scenarios: bool,
        adapters: Vec<Adapter>,

        /// Whether the test failed, so recordings stay where they are.
        kept: bool,
    }

    struct Adapter {
        path: PathBuf,

        /// The recording of an adapter that exited, held until the test
        /// either fails or ends.
        finished: Option<Vec<u8>>,
    }

    static RECORDINGS: Mutex<Option<Recordings>> = Mutex::new(None);

    thread_local! {
        /// Whether this thread holds the recordings, so a panic while it
        /// does cannot wait for them.
        static HOLDING: Cell<bool> = const { Cell::new(false) };
    }

    impl Recordings {
        fn path(&self, suffix: &str) -> PathBuf {
            let mut path = self.stem.clone().into_os_string();
            path.push(format!("{suffix}.log"));
            path.into()
        }

        /// Writes every recording of this test where it belongs and names
        /// each on stderr.
        fn keep(&mut self) {
            self.kept = true;
            let mut kept = Vec::new();

            if self.scenarios {
                let path = self.path("");
                match microscope::flight_recorder::dump(&path) {
                    Ok(()) => kept.push(path),
                    Err(error) => eprintln!("cannot keep {}: {error}", path.display()),
                }
            }

            for adapter in &mut self.adapters {
                if let Some(contents) = adapter.finished.take()
                    && let Err(error) = fs::write(&adapter.path, contents)
                {
                    eprintln!("cannot keep {}: {error}", adapter.path.display());
                    continue;
                }

                kept.push(adapter.path.clone());
            }

            for path in kept {
                eprintln!("flight recording: {}", path.display());
            }
        }
    }

    fn with<T>(change: impl FnOnce(&mut Recordings) -> T) -> T {
        struct Holding;

        impl Drop for Holding {
            fn drop(&mut self) {
                HOLDING.set(false);
            }
        }

        let mut recordings = RECORDINGS.lock().unwrap_or_else(PoisonError::into_inner);
        HOLDING.set(true);
        let _holding = Holding;

        change(recordings.get_or_insert_with(start))
    }

    /// Names this test's recordings after it, as libtest names its thread,
    /// removes those an earlier run left, and keeps them on any panic.
    fn start() -> Recordings {
        let thread = std::thread::current();
        let test = thread.name().unwrap_or("unnamed");
        let directory = microscope::flight_recorder::directory()
            .join("tests")
            .join(env!("CARGO_CRATE_NAME"));

        for entry in fs::read_dir(&directory).into_iter().flatten().flatten() {
            let name = entry.file_name();
            let earlier = name
                .to_str()
                .and_then(|name| name.strip_prefix(test))
                .is_some_and(|rest| rest.starts_with('.'));
            if earlier {
                let _ = fs::remove_file(entry.path());
            }
        }

        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // The debugger's own hook records the panic before reporting it.
            previous(info);

            // Another thread may hold the recordings briefly, so wait for
            // them unless this thread holds them.
            if !HOLDING.get() {
                with(Recordings::keep);
            }
        }));

        Recordings {
            stem: directory.join(test),
            scenarios: false,
            adapters: Vec::new(),
            kept: false,
        }
    }
}
