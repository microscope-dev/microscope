mod support;

use std::process::{Command, Stdio};

use microscope::profiler::{Profile, StackView};
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
            "--kernel",
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
            command.arg("--kernel");
        }
        let output = command
            .arg("--")
            .arg(Scenario::fixture("profile-kernel"))
            .arg("2000000")
            .stdin(Stdio::null())
            .output()
            .expect("record the read loop");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let profile = Profile::read(&path).expect("read the recording").profile;
        assert_eq!(profile.lost_samples(), 0);
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
    assert_eq!(kernel.top(StackView::Logical)[0].name, "[kernel: read]");
    assert_eq!(kernel.top(StackView::Native)[0].name, "[kernel: read]");
    let sampled: u64 = stacks.iter().map(|stack| stack.weight).sum();
    let counted = kernel.counted();
    assert!(counted > 0);
    assert!(
        u128::from(sampled) * 10 >= u128::from(counted) * 9,
        "sampled {sampled} of {counted} ns"
    );
    eprintln!(
        "kernel coverage: {sampled}/{counted} ns, {} samples, {} kernel reads",
        stacks.len(),
        reads.len()
    );
}
