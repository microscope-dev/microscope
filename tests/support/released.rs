//! Recordings that hold their program until the profile so far shows what
//! the test needs. The program parks on its standard input, and the test
//! writes it a line once the profile is ready, or at a deadline, after which
//! the test fails on what the profile lacks.
//!
//! The profile is the one place microscope says what it has seen: that it
//! has read the program's instrumentation (`ProbeCounts::read`), taken so
//! many snapshots, or sampled a line. A fixture never polls for it itself.

use std::io::Write as _;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use microscope::profiler::{Profile, RecordOptions, Recording, Watch, record_watched};

/// How long a recording holds its program for its profile to be ready
/// before it releases it anyway, and the test fails on what it lacks.
pub const READY_DEADLINE: Duration = Duration::from_secs(30);

/// How often the profile so far is published to be checked.
const EVERY: Duration = Duration::from_millis(20);

/// Records `command`, whose standard input this takes, holding it until
/// `release` says to let it go, given the profile so far and how long it
/// has been held; then writes it a line. The recording, once it ended.
pub fn record_released(
    mut command: Command,
    options: &RecordOptions,
    mut release: impl FnMut(&Profile, Duration) -> bool,
) -> Recording {
    let (reader, mut writer) = std::io::pipe().expect("a pipe");
    command.stdin(reader);

    let stop = AtomicBool::new(false);
    let mut released = false;
    let started = Instant::now();
    record_watched(
        command,
        options,
        &mut Watch {
            every: EVERY,
            publish: &mut |profile| {
                if !released && release(&profile, started.elapsed()) {
                    // A program that ended on its own has nothing to let go.
                    if let Err(error) = writer.write_all(b"\n") {
                        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe, "{error}");
                    }
                    released = true;
                }
            },
            stop: &stop,
            save: None,
        },
    )
    .expect("record")
}

/// Releases once the profile is `ready`, or at [`READY_DEADLINE`].
pub fn when(mut ready: impl FnMut(&Profile) -> bool) -> impl FnMut(&Profile, Duration) -> bool {
    move |profile, waited| ready(profile) || waited > READY_DEADLINE
}

/// Whether microscope has read the program's instrumentation, so that
/// what it marks from now on is recorded.
pub fn probes_read(profile: &Profile) -> bool {
    profile.probe_counts().read > 0
}

/// Records the probes fixture, or another program that parks on its
/// standard input until microscope has read its instrumentation, and
/// releases it then. The recording, which must have succeeded and read
/// all of the program's instrumentation.
pub fn record_probed(command: Command, options: &RecordOptions) -> Recording {
    let recording = record_released(command, options, when(probes_read));
    assert!(recording.status.success(), "{}", recording.status);
    super::assert_probes_read(&recording.profile);
    recording
}
