mod support;

use std::process::{Command, Stdio};

use microscope::profiler::{CallTree, Granularity, Keys, Profile, Query, StackView};
use microscope::syscalls::TraceEvent;
use support::{Scenario, ScratchDir};

#[test]
fn kernel_sampling_without_privileges_explains_how_to_enable_it() {
    let status = std::fs::read_to_string("/proc/self/status").expect("own capabilities");
    let caps = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:\t"))
        .and_then(|value| u64::from_str_radix(value, 16).ok())
        .expect("capability bits");
    if caps & (1 << 21) != 0 || caps & ((1 << 38) | (1 << 39)) == ((1 << 38) | (1 << 39)) {
        return;
    }
    let scratch = ScratchDir::new("profiler-kernel-denied");
    let output = Command::new(env!("CARGO_BIN_EXE_microscope"))
        .args([
            "record",
            "--kernel-stacks",
            "-o",
            "denied.usprof",
            "--",
            "/bin/true",
        ])
        .current_dir(scratch.path())
        .stdin(Stdio::null())
        .output()
        .expect("request kernel sampling without privileges");
    assert!(!output.status.success());
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.contains("CAP_PERFMON"), "{message}");
    assert!(
        message.contains("perf_event_paranoid") || message.contains("CAP_BPF"),
        "{message}"
    );
    assert!(!scratch.path().join("denied.usprof").exists());
}

#[test]
#[ignore = "needs CAP_BPF and CAP_PERFMON; use MICROSCOPE_BPF_LAUNCHER"]
fn kernel_read_samples_cover_cpu_time_and_keep_the_calling_user_stack() {
    let launcher = std::env::var_os("MICROSCOPE_BPF_LAUNCHER").expect("the BPF launcher");
    let scratch = ScratchDir::new("profiler-kernel");
    let mut profiles = Vec::new();
    for kernel in [false, true] {
        let path = scratch.path().join(format!("{kernel}.usprof"));
        let mut command = Command::new(&launcher);
        command
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .args(["record", "--preset", "dev", "-o"])
            .arg(&path);
        if kernel {
            command.arg("--kernel-stacks");
        }
        let output = command
            .arg("--")
            .arg(Scenario::fixture("profile-kernel"))
            .args(["200000", "65536"])
            .stdin(Stdio::null())
            .output()
            .expect("record the read loop");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let profile = Profile::read(&path).expect("read the recording").profile;
        support::profile::assert_loss_explained(&profile);
        profiles.push(profile);
    }

    let default = &profiles[0];
    assert!(default.stacks().iter().all(|stack| {
        stack
            .functions
            .iter()
            .all(|name| !name.starts_with("[kernel"))
    }));
    let kernel = &profiles[1];
    let stacks = kernel.stacks();
    let reads: Vec<_> = stacks
        .iter()
        .filter(|stack| {
            stack
                .functions
                .first()
                .is_some_and(|name| name == "[kernel: read]")
        })
        .collect();
    assert!(!reads.is_empty(), "no kernel read stacks: {stacks:?}");
    assert!(
        reads
            .iter()
            .all(|stack| stack.functions.iter().any(|name| name == "main")),
        "kernel samples lost main: {reads:?}"
    );
    assert_eq!(kernel.top(StackView::Tasks)[0].name, "[kernel: read]");
    assert_eq!(kernel.top(StackView::Native)[0].name, "[kernel: read]");
    let sampled: u64 = stacks.iter().map(|stack| stack.weight).sum();
    let counted = kernel.counted();
    assert!(counted > 0);
    assert!(
        u128::from(sampled) * 10 >= u128::from(counted) * 9,
        "sampled {sampled} of {counted} ns"
    );
    let callers = Command::new(env!("CARGO_BIN_EXE_microscope"))
        .args(["report", "--focus", "[kernel: read]", "--invert", "--tree"])
        .arg(scratch.path().join("true.usprof"))
        .output()
        .expect("report the read's callers");
    let callers = String::from_utf8_lossy(&callers.stdout);
    assert!(
        callers.lines().any(|line| line.ends_with(" main")),
        "{callers}"
    );

    let (used, _) = kernel.recorder_cpu();
    let setup = kernel.recorder_setup();
    assert!(
        setup > default.recorder_setup() && setup < used,
        "{setup} of {used} ns starting up"
    );
    eprintln!(
        "kernel coverage: {sampled}/{counted} ns, {} samples, {} kernel reads",
        stacks.len(),
        reads.len()
    );
}

#[test]
#[ignore = "needs CAP_BPF and CAP_PERFMON; use MICROSCOPE_BPF_LAUNCHER"]
fn a_budget_keeps_a_share_of_each_process_samples_and_scales_them_to_its_cpu_time() {
    let launcher = std::env::var_os("MICROSCOPE_BPF_LAUNCHER").expect("the BPF launcher");
    let scratch = ScratchDir::new("profiler-kept-share");
    let path = scratch.path().join("kept.usprof");
    let output = Command::new(&launcher)
        .arg(env!("CARGO_BIN_EXE_microscope"))
        .args([
            "record", "--preset", "prod", "--rate", "9999", "--budget", "0.001",
        ])
        .args(["--syscalls", "off", "-o"])
        .arg(&path)
        .args(["--", "sh", "-c"])
        .arg("for child in 1 2 3; do timeout 2.5 sh -c 'while :; do :; done' & done; wait")
        .stdin(Stdio::null())
        .output()
        .expect("record three spinning children");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let profile = Profile::read(&path).expect("read the recording").profile;
    assert!(
        profile.gaps().is_empty(),
        "sampling paused: {:?}",
        profile.gaps()
    );
    let (raised, kept) = profile.kept_share().expect("the budget kept a share");
    assert!(kept * 3 < raised * 2, "kept {kept} of {raised} samples");
    let (total, counted) = (profile.total_weight(), profile.counted());
    assert!(
        total.abs_diff(counted) * 100 < counted,
        "scaled to {total} ns of {counted} ns"
    );
}

fn ending_paths(tree: &CallTree, keys: &Keys<'_>) -> Vec<(Vec<String>, u64)> {
    (0..tree.key.len())
        .filter(|&node| tree.own[node] > 0)
        .map(|node| {
            let mut path = Vec::new();
            let mut at = u32::try_from(node).expect("a node");
            while at != u32::MAX {
                path.push(keys.name(tree.key[at as usize]));
                at = tree.parent[at as usize];
            }
            path.reverse();
            (path, tree.own[node])
        })
        .collect()
}

#[test]
#[ignore = "needs CAP_BPF and CAP_PERFMON; use MICROSCOPE_BPF_LAUNCHER"]
fn samples_during_calls_and_calls_as_leaves_account_for_each_others_time() {
    let launcher = std::env::var_os("MICROSCOPE_BPF_LAUNCHER").expect("the BPF launcher");
    let scratch = ScratchDir::new("profiler-kernel-calls");
    let path = scratch.path().join("work.usprof");
    let output = Command::new(&launcher)
        .arg(env!("CARGO_BIN_EXE_microscope"))
        .args(["record", "--preset", "dev", "--kernel-stacks", "-o"])
        .arg(&path)
        .arg("--")
        .arg(Scenario::fixture("syscalls"))
        .arg("work")
        .stdin(Stdio::null())
        .output()
        .expect("record the work loop");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let profile = Profile::read(&path).expect("read the recording").profile;
    let keys = profile.keys(Granularity::Function);
    let calls: Vec<(String, Vec<String>)> = profile
        .syscalls()
        .events
        .iter()
        .filter_map(|event| match &event.event {
            TraceEvent::Call(call) => Some((
                call.call.display_name().into_owned(),
                event
                    .stack
                    .map(|stack| profile.syscall_stack(stack).0)
                    .unwrap_or_default(),
            )),
            _ => None,
        })
        .collect();
    let rows = |name: &str, caller: &str| -> Vec<u32> {
        (0u32..)
            .zip(&calls)
            .filter(|(_, (call, stack))| call == name && stack.iter().any(|frame| frame == caller))
            .map(|(row, _)| row)
            .collect()
    };
    let randoms = rows("getrandom", "draw");
    let rests = rows("clock_nanosleep", "rest");
    assert_eq!((randoms.len(), rests.len()), (25, 25), "{calls:?}");

    let during = |rows: Vec<u32>| {
        ending_paths(
            &keys.aggregate(&Query {
                during: Some(rows),
                ..Query::default()
            }),
            &keys,
        )
    };
    let drawn = during(randoms);
    assert!(!drawn.is_empty(), "no samples during getrandom");
    for (path, _) in &drawn {
        assert!(path.iter().any(|name| name == "draw"), "{path:?}");
        assert!(!path.iter().any(|name| name == "burn"), "{path:?}");
    }
    for (path, _) in during(rests) {
        assert!(path.iter().any(|name| name == "rest"), "{path:?}");
    }

    let leaves = ending_paths(
        &keys.aggregate(&Query {
            calls: true,
            ..Query::default()
        }),
        &keys,
    );
    let leaf = |name: &str| -> u64 {
        leaves
            .iter()
            .filter(|(path, _)| path.last().is_some_and(|last| last == name))
            .map(|(_, weight)| weight)
            .sum()
    };
    let slept = leaf("[syscall] clock_nanosleep");
    assert!(slept >= 25 * 2_000_000 * 9 / 10, "slept {slept} ns");
    for (path, _) in &leaves {
        if path
            .last()
            .is_some_and(|last| last == "[syscall] clock_nanosleep")
        {
            assert!(path.iter().any(|name| name == "rest"), "{path:?}");
        }
    }
    eprintln!(
        "slept {slept} ns; getrandom leaves {} ns beside {} ns sampled",
        leaf("[syscall] getrandom"),
        drawn.iter().map(|(_, weight)| weight).sum::<u64>()
    );
}
