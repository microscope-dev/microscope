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

use microscope::profiler::{Profile, RecordOptions, Recording, Save, Watch, record_watched};

/// How long a recording holds its program for its profile to be ready
/// before it releases it anyway, and the test fails on what it lacks.
pub const READY_DEADLINE: Duration = Duration::from_secs(30);

/// The snapshots a recording must hold, past its deadline, before it is
/// released unready, and how long it may wait for them. The first stopped
/// snapshot's debugger reads every library's debug information, which
/// `just stress`, with no image cache and every CPU busy, can make take
/// most of the deadline; a recording released after one has shown nothing
/// of the program.
const ENOUGH_SNAPSHOTS: u64 = 10;
const SNAPSHOT_DEADLINE: Duration = Duration::from_secs(100);

/// Lines a release gives a program once it gives up on its profile: one
/// more, then the end of its input, which ends every wait still to come.
pub const ALL: usize = usize::MAX;

/// How often the profile so far is published to be checked.
const EVERY: Duration = Duration::from_millis(20);

/// Records `command`, whose standard input this takes, holding it until
/// `lines`, given the profile so far and how long it has been held, says
/// to let it go: as each profile is published, the program is written as
/// many lines as `lines` gives in all, or [`ALL`]. The recording, once it
/// ended.
pub fn record_released(
    command: Command,
    options: &RecordOptions,
    lines: impl FnMut(&Profile, Duration) -> usize,
) -> Recording {
    record_released_saving(command, options, lines, None)
}

/// [`record_released`], saving the profile so far as `save` says.
pub fn record_released_saving(
    mut command: Command,
    options: &RecordOptions,
    mut lines: impl FnMut(&Profile, Duration) -> usize,
    save: Option<Save<'_>>,
) -> Recording {
    let (reader, writer) = std::io::pipe().expect("a pipe");
    command.stdin(reader);

    let stop = AtomicBool::new(false);
    let mut writer = Some(writer);
    let mut written = 0;
    let started = Instant::now();
    record_watched(
        command,
        options,
        &mut Watch {
            every: EVERY,
            publish: &mut |profile| {
                let Some(input) = writer.as_mut() else {
                    return;
                };
                let wanted = lines(&profile, started.elapsed());
                let line = |input: &mut std::io::PipeWriter| {
                    // A program that ended on its own has nothing to let go.
                    if let Err(error) = input.write_all(b"\n") {
                        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe, "{error}");
                    }
                };
                if wanted == ALL {
                    line(input);
                    writer = None;
                    return;
                }
                while written < wanted {
                    line(input);
                    written += 1;
                }
            },
            stop: &stop,
            #[expect(
                clippy::unnecessary_struct_initialization,
                reason = "a closure behind &mut is invariant: this shortens the save's \
                          borrows to the watch's own"
            )]
            save: save.map(|save| Save {
                trigger: save.trigger,
                to: save.to,
            }),
        },
    )
    .expect("record")
}

/// Releases once, when the profile is `ready`, or at [`READY_DEADLINE`].
pub fn when(mut ready: impl FnMut(&Profile) -> bool) -> impl FnMut(&Profile, Duration) -> usize {
    let mut released = false;
    move |profile, waited| {
        released = released || ready(profile);
        if released {
            1
        } else if waited > READY_DEADLINE {
            ALL
        } else {
            0
        }
    }
}

/// Releases once, when the profile is `ready`; or past [`READY_DEADLINE`]
/// once there are enough snapshots to fail on, as a program whose
/// snapshots are late is held for them until [`SNAPSHOT_DEADLINE`].
pub fn when_snapshotted(
    mut ready: impl FnMut(&Profile) -> bool,
) -> impl FnMut(&Profile, Duration) -> usize {
    let mut released = false;
    move |profile, waited| {
        released = released || ready(profile);
        let snapshots = profile.task_snapshots().0 + profile.live_snapshots().0;
        let given_up = waited > READY_DEADLINE
            && (snapshots >= ENOUGH_SNAPSHOTS || waited > SNAPSHOT_DEADLINE);
        if released {
            1
        } else if given_up {
            ALL
        } else {
            0
        }
    }
}

/// Whether microscope has read the program's instrumentation, so that
/// what it marks from now on is recorded.
pub fn probes_read(profile: &Profile) -> bool {
    profile.probe_counts().read > 0
}

/// Releases a line for each module whose instrumentation microscope read,
/// as a program that executes another waits once in each, until
/// [`READY_DEADLINE`].
pub fn each_read() -> impl FnMut(&Profile, Duration) -> usize {
    |profile, waited| {
        if waited > READY_DEADLINE {
            ALL
        } else {
            usize::try_from(profile.probe_counts().read).expect("a count")
        }
    }
}

/// Records a program that parks on its standard input until microscope
/// has read its instrumentation, as the probes fixtures do, once in each
/// program it executes. The recording, which must have succeeded and read
/// all of the program's instrumentation.
pub fn record_probed(command: Command, options: &RecordOptions) -> Recording {
    let recording = record_released(command, options, each_read());
    assert!(recording.status.success(), "{}", recording.status);
    super::assert_probes_read(&recording.profile);
    recording
}
