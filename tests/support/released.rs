use std::io::Write as _;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use microscope::profiler::{Profile, RecordOptions, Recording, Save, Watch, record_watched};

pub const READY_DEADLINE: Duration = Duration::from_secs(30);

const ENOUGH_SNAPSHOTS: u64 = 10;
const SNAPSHOT_DEADLINE: Duration = Duration::from_secs(100);

pub const ALL: usize = usize::MAX;

const EVERY: Duration = Duration::from_millis(20);

pub fn record_released(
    command: Command,
    options: &RecordOptions,
    lines: impl FnMut(&Profile, Duration) -> usize,
) -> Recording {
    record_released_saving(command, options, lines, None)
}

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

pub fn probes_read(profile: &Profile) -> bool {
    profile.probe_counts().read > 0
}

pub fn each_read() -> impl FnMut(&Profile, Duration) -> usize {
    |profile, waited| {
        if waited > READY_DEADLINE {
            ALL
        } else {
            usize::try_from(profile.probe_counts().read).expect("a count")
        }
    }
}

pub fn record_probed(command: Command, options: &RecordOptions) -> Recording {
    let recording = record_released(command, options, each_read());
    assert!(recording.status.success(), "{}", recording.status);
    super::assert_probes_read(&recording.profile);
    recording
}
