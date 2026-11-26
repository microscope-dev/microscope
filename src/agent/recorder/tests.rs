use std::{
    collections::{BTreeMap, BTreeSet},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use microscope_data::{
    object::{ObjectReader, TimeUnit, decode_tables},
    sanitize::{Action, Builtin, HashKey, Rule, RuleSpec, Sanitizer, sanitize_chunk},
};

use super::{
    PendingFlush, PlannedTarget, SharedState,
    cut::{
        CutOutcome, bucket_allowed_pids, bucket_targets, build_chunk_data,
        build_selected_chunk_data, finish_cut_locked, wall_clock_reference,
    },
    policy::{
        bpf_max_ring_bytes_for_cpus, build_record_options_for_cpus, effective_policy_for_process,
        max_concurrent_groups_for_cpus, page_size_bytes, syscalls_fit_memory_limit_for_cpus,
    },
};
use crate::{
    agent::{
        config::{Limits, RecordConfig},
        scratch::Scratch,
        select::{ProcessMetadata, SelectedProcess},
        split::filter_capture_by_pids,
        spool::{MAX_POST_BYTES, Spool},
        wire::BurstDirective,
    },
    profiler::{
        Cuts, Preset, Profile, RecordOptions, SMALL_STACK_COPY, Save, StackCopy, SyscallLevel,
        Watch, record_attached_many, record_attached_many_exact,
    },
};

#[test]
fn planted_secrets_in_real_recording_never_leave_the_agent() {
    let arg_secret = "supersecret_cli_pw_998877";
    let phrase_secret = "correct horse battery staple";
    let gh_secret = "ghp_0123456789abcdefghijklmnopqrstuvwxyz";
    let bearer_secret = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";

    let mut cmd = Command::new("/bin/sh");
    cmd.args([
        "-c",
        "true",
        "sh",
        &format!("--password={arg_secret}"),
        "--secret",
        phrase_secret,
        "--token",
        gh_secret,
        &format!("Authorization: Bearer {bearer_secret}"),
    ]);

    let mut options = RecordOptions::preset(Preset::Production);
    options.syscalls.level = SyscallLevel::Off;
    let recorded = crate::profiler::record(cmd, &options).expect("record /bin/sh");

    let record_cfg = RecordConfig {
        process: "sh".into(),
        service: "planted-svc".into(),
        project: "planted-proj".into(),
        preset: Preset::Production,
        time_unit: TimeUnit::Us,
        chunk_ms: 5_000,
        post_ms: 10_000,
        spool_mib: 64,
        rules: Vec::new(),
        runtime_dirs: Vec::new(),
        pod_annotations_dir: None,
    };
    let tags = BTreeMap::from([(
        "custom_tag".to_owned(),
        format!("Authorization: Bearer {bearer_secret}"),
    )]);

    let raw = build_chunk_data(
        recorded.profile,
        9,
        1,
        &record_cfg,
        "test-host",
        &tags,
        1_700_000_000_000_000_000,
    );
    let policy = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"planted-key"));
    let sanitized = sanitize_chunk(raw, TimeUnit::Us, &policy);

    let scratch = Scratch::new();
    let mut spool = Spool::open(&scratch.path().join("spool"), 1024 * 1024).expect("spool");
    spool
        .push(1, &sanitized, 9, 1_700_000_000_000_000_000)
        .expect("push");
    let batch = spool
        .peek_batch(MAX_POST_BYTES, 9, 1_700_000_000_000_000_000, &policy)
        .expect("peek")
        .expect("batch");

    let post_bytes = batch.post.as_bytes();
    let forbidden = [
        arg_secret,
        phrase_secret,
        "horse",
        "battery",
        "staple",
        gh_secret,
        bearer_secret,
    ];
    for secret in forbidden {
        assert!(
            !post_bytes
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "secret {secret} leaked in raw USCOPEOB post bytes"
        );
    }

    let reader = ObjectReader::open(post_bytes).expect("open post");
    let decoded = decode_tables(&reader.part_sections(0, None).expect("sections")).expect("dec");
    let strings = String::from_utf8_lossy(&decoded.capture.strings);
    let blobs = String::from_utf8_lossy(&decoded.capture.syscall_blobs);
    for secret in forbidden {
        assert!(
            !strings.contains(secret),
            "secret {secret} leaked in strings"
        );
        assert!(!blobs.contains(secret), "secret {secret} leaked in blobs");
        for dim in &decoded.dimensions {
            assert!(
                !dim.value.contains(secret),
                "secret {secret} leaked in dimension"
            );
        }
    }
    assert!(
        !decoded.sanitizer_counts.is_empty(),
        "expected sanitizer counts to record scrubbed secrets"
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "end-to-end integration test covering three programs, two projects, and spool post verification"
)]
fn three_programs_records_two_into_two_projects_and_never_mentions_third() {
    struct KillOnDrop(std::process::Child);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let a = KillOnDrop(
        Command::new("/bin/sh")
            .env("MICROSCOPE_PROJECT", "alpha")
            .args(["-c", "while :; do :; done"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn alpha"),
    );
    let b = KillOnDrop(
        Command::new("/bin/sh")
            .env("MICROSCOPE_PROJECT", "beta")
            .args(["-c", "while :; do :; done"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn beta"),
    );
    let c = KillOnDrop(
        Command::new("/bin/sh")
            .args(["-c", "while :; do :; done"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn third"),
    );

    let pid_a = a.0.id();
    let pid_b = b.0.id();
    let pid_c = c.0.id();

    let mut options = RecordOptions::preset(Preset::Production);
    options.syscalls.level = SyscallLevel::Off;
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_timer = Arc::clone(&stop);
    let stopper = thread::spawn(move || {
        let _ = started_rx.recv();
        thread::sleep(Duration::from_millis(400));
        stop_timer.store(true, Ordering::SeqCst);
    });
    let mut started_tx = Some(started_tx);
    let mut publish = |_: Profile| {
        if let Some(tx) = started_tx.take() {
            let _ = tx.send(());
        }
    };
    let mut watch = Watch {
        every: Duration::MAX,
        publish: &mut publish,
        stop: &stop,
        save: None,
    };
    let attached =
        record_attached_many(&[pid_a, pid_b], &options, &mut watch).expect("record two processes");
    let _ = stopper.join();
    drop(a);
    drop(b);
    drop(c);

    let sel_a = SelectedProcess {
        pid: pid_a,
        program: "prog-alpha".into(),
        project: Some("alpha".into()),
        service: "svc-alpha".into(),
        service_explicit: true,
        matched_by: "env: MICROSCOPE_PROJECT".into(),
        meta: ProcessMetadata {
            pid: pid_a,
            comm: "prog-alpha".into(),
            ..ProcessMetadata::default()
        },
    };
    let sel_b = SelectedProcess {
        pid: pid_b,
        program: "prog-beta".into(),
        project: Some("beta".into()),
        service: "svc-beta".into(),
        service_explicit: true,
        matched_by: "env: MICROSCOPE_PROJECT".into(),
        meta: ProcessMetadata {
            pid: pid_b,
            comm: "prog-beta".into(),
            ..ProcessMetadata::default()
        },
    };

    let scratch = Scratch::new();
    let mut spool = Spool::open(&scratch.path().join("spool"), 4 * 1024 * 1024).expect("spool");
    let policy = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"test-key"));
    let (now_ns, wall_offset) = wall_clock_reference();

    let cap_a = filter_capture_by_pids(attached.profile.capture(), &BTreeSet::from([pid_a]));
    let cap_b = filter_capture_by_pids(attached.profile.capture(), &BTreeSet::from([pid_b]));
    assert!(cap_a.validate().is_ok());
    assert!(cap_b.validate().is_ok());
    assert!(
        !cap_a.processes.iter().any(|p| p.pid.get() == pid_c)
            && !cap_b.processes.iter().any(|p| p.pid.get() == pid_c)
    );
    assert!(!cap_a.processes.iter().any(|p| p.pid.get() == pid_b));
    assert!(!cap_b.processes.iter().any(|p| p.pid.get() == pid_a));

    let raw_a = build_selected_chunk_data(
        cap_a,
        7,
        1,
        Some("alpha"),
        &[&sel_a],
        Preset::Production,
        TimeUnit::Ms,
        "host-1",
        &BTreeMap::new(),
        now_ns,
        wall_offset,
    );
    let raw_b = build_selected_chunk_data(
        cap_b,
        7,
        2,
        Some("beta"),
        &[&sel_b],
        Preset::Production,
        TimeUnit::Ms,
        "host-1",
        &BTreeMap::new(),
        now_ns,
        wall_offset,
    );
    spool
        .push(1, &sanitize_chunk(raw_a, TimeUnit::Ms, &policy), 7, now_ns)
        .expect("push a");
    spool
        .push(2, &sanitize_chunk(raw_b, TimeUnit::Ms, &policy), 7, now_ns)
        .expect("push b");

    let batch1 = spool
        .peek_batch(MAX_POST_BYTES, 7, now_ns, &policy)
        .expect("peek 1")
        .expect("batch 1");
    assert_eq!(batch1.first_chunk, 1);
    assert_eq!(batch1.last_chunk, 1);
    spool.ack(batch1.owner_gen, &batch1.numbers).expect("ack 1");

    let batch2 = spool
        .peek_batch(MAX_POST_BYTES, 7, now_ns, &policy)
        .expect("peek 2")
        .expect("batch 2");
    assert_eq!(batch2.first_chunk, 2);
    assert_eq!(batch2.last_chunk, 2);
    spool.ack(batch2.owner_gen, &batch2.numbers).expect("ack 2");

    for (post, wanted_proj, forbidden_proj) in [
        (batch1.post.as_bytes(), "alpha", "beta"),
        (batch2.post.as_bytes(), "beta", "alpha"),
    ] {
        let reader = ObjectReader::open(post).expect("open");
        let dec = decode_tables(&reader.part_sections(0, None).expect("sec")).expect("dec");
        assert!(
            dec.dimensions
                .iter()
                .any(|d| d.name == "microscope.project" && d.value == wanted_proj)
        );
        assert!(
            !dec.dimensions
                .iter()
                .any(|d| d.value.contains(forbidden_proj))
        );
        assert!(!dec.capture.processes.iter().any(|p| p.pid.get() == pid_c));
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "verifies concurrent bursts, divided CPU/memory limits, unhinted chunk bucketing, and derived service sanitization"
)]
fn roast_hardening_policy_limits_bursts_buckets_descendants_and_derived_service_sanitization() {
    let sel_alpha = SelectedProcess {
        pid: 100,
        program: "alpha-bin".into(),
        project: Some("alpha".into()),
        service: "alpha-svc".into(),
        service_explicit: true,
        matched_by: "rule: process alpha-*".into(),
        meta: ProcessMetadata {
            pid: 100,
            comm: "alpha-bin".into(),
            ..ProcessMetadata::default()
        },
    };
    let sel_beta = SelectedProcess {
        pid: 101,
        program: "beta-bin".into(),
        project: Some("beta".into()),
        service: "beta-svc".into(),
        service_explicit: true,
        matched_by: "rule: process beta-*".into(),
        meta: ProcessMetadata {
            pid: 101,
            comm: "beta-bin".into(),
            ..ProcessMetadata::default()
        },
    };
    let bursts = vec![
        BurstDirective {
            id: 1,
            duration_ms: 60_000,
            preset: "dev".into(),
            time_unit: Some("1us".into()),
            project: Some("alpha".into()),
            ends_at_ns: Some(2_000),
        },
        BurstDirective {
            id: 2,
            duration_ms: 60_000,
            preset: "max".into(),
            time_unit: Some("1ns".into()),
            project: Some("beta".into()),
            ends_at_ns: Some(2_000),
        },
    ];
    let limits = Limits {
        cpu_percent: Some(4),
        memory_mib: Some(32),
        ..Limits::default()
    };
    assert_eq!(
        effective_policy_for_process(
            &sel_alpha,
            Preset::Production,
            TimeUnit::Ms,
            None,
            &bursts,
            &limits,
            1_000,
        ),
        (Preset::Development, TimeUnit::Us)
    );
    assert_eq!(
        effective_policy_for_process(
            &sel_beta,
            Preset::Production,
            TimeUnit::Ms,
            None,
            &bursts,
            &limits,
            1_000,
        ),
        (Preset::Maximum, TimeUnit::Ns)
    );

    let opts_one =
        build_record_options_for_cpus(Preset::Development, 5_000, &limits, 1, 8, None, false);
    let opts_two =
        build_record_options_for_cpus(Preset::Development, 5_000, &limits, 2, 8, None, false);
    assert!((opts_one.budget.unwrap().cpu - 0.04).abs() < 1e-9);
    assert!((opts_two.budget.unwrap().cpu - 0.02).abs() < 1e-9);
    assert!(opts_two.ring_pages <= opts_one.ring_pages);
    assert!(opts_two.ring_pages < Preset::Maximum.ring_pages());

    let tight_limits = Limits {
        memory_mib: Some(16),
        ..Limits::default()
    };
    let opts_8 =
        build_record_options_for_cpus(Preset::Development, 5_000, &tight_limits, 1, 8, None, true);
    assert_eq!(opts_8.syscalls.max_ring_bytes, Some(512 * 1024));
    assert_eq!(
        bpf_max_ring_bytes_for_cpus(&tight_limits, 8),
        Some(512 * 1024)
    );
    let bpf_bytes_8 = usize::try_from(opts_8.syscalls.max_ring_bytes.unwrap()).unwrap() * 8;
    let perf_bytes_8 = (opts_8.ring_pages + 1) * page_size_bytes() * 8;
    let queue_bytes_8 = opts_8.max_queue_bytes.unwrap();
    assert!(
        bpf_bytes_8 + perf_bytes_8 + queue_bytes_8 <= 16 * 1024 * 1024,
        "8-CPU BPF ({bpf_bytes_8}) + perf ({perf_bytes_8}) + queue ({queue_bytes_8}) must stay within 16 MiB cap"
    );

    let groups_128 = 3_usize.min(max_concurrent_groups_for_cpus(&tight_limits, 128));
    let opts_128 = build_record_options_for_cpus(
        Preset::Maximum,
        5_000,
        &tight_limits,
        groups_128,
        128,
        None,
        true,
    );
    let mapped_bytes = groups_128 * 128 * (opts_128.ring_pages + 1) * page_size_bytes();
    assert!(
        mapped_bytes <= 16 * 1024 * 1024,
        "128-CPU 3-group ring mapping ({mapped_bytes} bytes) must stay within 16 MiB cap"
    );
    assert_eq!(opts_128.syscalls.level, SyscallLevel::Off);
    assert!(!syscalls_fit_memory_limit_for_cpus(&tight_limits, 128));
    assert!(
        opts_128
            .max_queue_bytes
            .is_some_and(|q| q <= 4 * 1024 * 1024)
    );
    assert_eq!(
        max_concurrent_groups_for_cpus(&tight_limits, 2048),
        0,
        "2048-CPU host cannot fit even minimum rings inside 16 MiB cap and must refuse"
    );

    let unhinted_a = PlannedTarget {
        selected: SelectedProcess {
            pid: 200,
            program: "prog-one".into(),
            project: None,
            service: "prog-one".into(),
            service_explicit: false,
            matched_by: "rule: process *".into(),
            meta: ProcessMetadata {
                pid: 200,
                comm: "prog-one".into(),
                build_id: Some("aaaa".into()),
                ..ProcessMetadata::default()
            },
        },
        preset: Preset::Production,
        time_unit: TimeUnit::Ms,
    };
    let unhinted_b = PlannedTarget {
        selected: SelectedProcess {
            pid: 201,
            program: "prog-two".into(),
            project: None,
            service: "prog-two".into(),
            service_explicit: false,
            matched_by: "rule: process *".into(),
            meta: ProcessMetadata {
                pid: 201,
                comm: "prog-two".into(),
                build_id: Some("bbbb".into()),
                ..ProcessMetadata::default()
            },
        },
        preset: Preset::Production,
        time_unit: TimeUnit::Ms,
    };
    let unhinted = [unhinted_a, unhinted_b];
    let buckets = bucket_targets(&unhinted);
    assert_eq!(
        buckets.len(),
        2,
        "unhinted processes with different programs must be split into separate chunks"
    );

    let secret_prog = SelectedProcess {
        pid: 300,
        program: "worker-SECRET_TOKEN_99".into(),
        project: Some("alpha".into()),
        service: "worker-SECRET_TOKEN_99".into(),
        service_explicit: false,
        matched_by: "env: MICROSCOPE_PROJECT".into(),
        meta: ProcessMetadata {
            pid: 300,
            comm: "worker-SECRET_TOKEN_99".into(),
            ..ProcessMetadata::default()
        },
    };
    let raw = build_selected_chunk_data(
        microscope_data::capture::Capture::default(),
        1,
        1,
        Some("alpha"),
        &[&secret_prog],
        Preset::Production,
        TimeUnit::Ms,
        "host",
        &BTreeMap::new(),
        1_000,
        0,
    );
    assert!(
        !raw.dimensions.iter().any(|d| d.name == "service.name"),
        "derived service name must not emit raw service.name dimension"
    );
    let custom_rule = Rule::new(&RuleSpec {
        name: "tok".into(),
        fields: vec!["syscall.string".into()],
        pattern: "SECRET_TOKEN_[0-9]+".into(),
        action: Action::Redact,
    })
    .unwrap();
    let san = Sanitizer::new(&Builtin::DEFAULT, vec![custom_rule], HashKey::new(b"k"));
    let clean = sanitize_chunk(raw, TimeUnit::Ms, &san);
    let post = microscope_data::sanitize::encode_post(
        &[clean],
        1,
        1_000,
        microscope_data::object::DEFAULT_ZSTD_LEVEL,
    )
    .unwrap();
    assert!(
        !post
            .as_bytes()
            .windows(b"SECRET_TOKEN_99".len())
            .any(|w| w == b"SECRET_TOKEN_99"),
        "derived service name must never bypass sanitizer"
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "spawns parent with excluded, other-project, and unmatched children, verifies exact PID recording, and reaps tree"
)]
fn excluded_other_project_and_unmatched_descendants_are_not_recorded_under_parent() {
    struct KillTreeOnDrop {
        parent: std::process::Child,
        children: Vec<u32>,
    }
    impl Drop for KillTreeOnDrop {
        fn drop(&mut self) {
            let mut to_kill = self.children.clone();
            to_kill.extend(crate::backend::perf::descendants(self.parent.id()));
            for pid in to_kill {
                if let Ok(raw) = i32::try_from(pid) {
                    let _ = nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(raw),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                }
            }
            let _ = self.parent.kill();
            let _ = self.parent.wait();
        }
    }
    let parent = Command::new("/bin/sh")
        .args([
            "-c",
            "sh -c 'while :; do :; done' & sh -c 'while :; do :; done' & sh -c 'while :; do :; done' & while :; do :; done",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn parent");
    let parent_pid = parent.id();
    let mut tree = KillTreeOnDrop {
        parent,
        children: Vec::new(),
    };
    for _ in 0..40 {
        let desc = crate::backend::perf::descendants(parent_pid);
        if desc.len() >= 3 {
            tree.children = desc;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        tree.children.len() >= 3,
        "expected parent {parent_pid} to spawn three live children (excluded, other-project, unmatched)"
    );
    let excluded_child = tree.children[0];
    let other_project_child = tree.children[1];
    let unmatched_child = tree.children[2];

    let sel_parent = SelectedProcess {
        pid: parent_pid,
        program: "parent-sh".into(),
        project: Some("alpha".into()),
        service: "alpha-parent".into(),
        service_explicit: true,
        matched_by: "env: MICROSCOPE_PROJECT".into(),
        meta: ProcessMetadata {
            pid: parent_pid,
            comm: "sh".into(),
            ..ProcessMetadata::default()
        },
    };
    let allowed = bucket_allowed_pids(&[&sel_parent]);
    assert!(allowed.contains(&parent_pid));
    assert!(!allowed.contains(&excluded_child));
    assert!(!allowed.contains(&other_project_child));
    assert!(!allowed.contains(&unmatched_child));

    let mut options = RecordOptions::preset(Preset::Production);
    options.syscalls.level = SyscallLevel::Off;
    options.ring_pages = 16;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_timer = Arc::clone(&stop);
    let stopper = thread::spawn(move || {
        thread::sleep(Duration::from_millis(250));
        stop_timer.store(true, Ordering::SeqCst);
    });
    let mut publish = |_: Profile| {};
    let mut watch = Watch {
        every: Duration::MAX,
        publish: &mut publish,
        stop: &stop,
        save: None,
    };
    let attached = record_attached_many_exact(&[parent_pid], &options, &mut watch)
        .expect("record exact parent pid only");
    let _ = stopper.join();
    drop(tree);

    let raw_cap = attached.profile.capture();
    for forbidden_pid in [excluded_child, other_project_child, unmatched_child] {
        assert!(
            !raw_cap
                .processes
                .iter()
                .any(|p| p.pid.get() == forbidden_pid),
            "exact agent recording must not collect unselected/excluded/other-project child {forbidden_pid}"
        );
    }
    let filtered = filter_capture_by_pids(raw_cap, &allowed);
    for forbidden_pid in [excluded_child, other_project_child, unmatched_child] {
        assert!(
            !filtered
                .processes
                .iter()
                .any(|p| p.pid.get() == forbidden_pid)
        );
    }
}

#[test]
fn exact_agent_attach_samples_new_threads_started_after_attach() {
    struct KillChildOnDrop(Option<std::process::Child>);
    impl Drop for KillChildOnDrop {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    let self_pid = std::process::id();
    let mut options = RecordOptions::preset(Preset::Development);
    options.syscalls.level = SyscallLevel::Off;
    options.ring_pages = 16;
    options.stack = StackCopy::Bytes(SMALL_STACK_COPY);

    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = Arc::clone(&stop);
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let (ids_tx, ids_rx) = std::sync::mpsc::channel::<(u32, u32)>();
    let driver = thread::spawn(move || {
        let _ = started_rx.recv();
        let child = Command::new("/bin/sh")
            .args(["-c", "while :; do :; done"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn post-attach child");
        let child_pid = child.id();
        let _child_guard = KillChildOnDrop(Some(child));
        let worker = thread::spawn(move || {
            let tid = u32::try_from(nix::unistd::gettid().as_raw()).unwrap_or(0);
            let _ = ids_tx.send((tid, child_pid));
            let mut acc = 0_u64;
            let until = std::time::Instant::now() + Duration::from_millis(350);
            while std::time::Instant::now() < until {
                acc = acc.wrapping_add(1);
                std::hint::black_box(acc);
            }
        });
        let _ = worker.join();
        stop_worker.store(true, Ordering::SeqCst);
    });

    let mut started_tx = Some(started_tx);
    let mut publish = |_: Profile| {
        if let Some(tx) = started_tx.take() {
            let _ = tx.send(());
        }
    };
    let mut watch = Watch {
        every: Duration::MAX,
        publish: &mut publish,
        stop: &stop,
        save: None,
    };
    let attached =
        record_attached_many_exact(&[self_pid], &options, &mut watch).expect("attach self");
    let _ = driver.join();
    let (spawned_tid, post_attach_child_pid) = ids_rx.recv().expect("worker tid and child pid");

    let raw_cap = attached.profile.capture();
    assert!(
        !raw_cap
            .processes
            .iter()
            .any(|p| p.pid.get() == post_attach_child_pid),
        "child process {post_attach_child_pid} forked after attach must not be collected in raw capture"
    );

    let allowed = BTreeSet::from([self_pid]);
    let filtered = filter_capture_by_pids(raw_cap, &allowed);
    let sampled_tids: BTreeSet<u32> = filtered
        .sample_ranges
        .iter()
        .filter(|r| r.count.get() > 0)
        .filter_map(|r| filtered.threads.get(r.thread.get() as usize))
        .map(|t| t.tid.get())
        .collect();
    assert!(
        sampled_tids.contains(&spawned_tid),
        "new thread {spawned_tid} spawned after attach must be sampled; saw tids {sampled_tids:?}"
    );
}

#[test]
fn spool_eviction_of_fresh_chunk_fails_pending_flush() {
    let scratch = Scratch::new();
    let policy = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"test-key"));
    let sel = SelectedProcess {
        pid: 100,
        program: "alpha-bin".into(),
        project: Some("alpha".into()),
        service: "alpha-svc".into(),
        service_explicit: true,
        matched_by: "env: MICROSCOPE_PROJECT".into(),
        meta: ProcessMetadata {
            pid: 100,
            comm: "alpha-bin".into(),
            ..ProcessMetadata::default()
        },
    };
    let make_chunk = |num: u64| {
        let raw = build_selected_chunk_data(
            microscope_data::capture::Capture::default(),
            1,
            num,
            Some("alpha"),
            &[&sel],
            Preset::Production,
            TimeUnit::Ms,
            "host-1",
            &BTreeMap::new(),
            1_000,
            0,
        );
        sanitize_chunk(raw, TimeUnit::Ms, &policy)
    };
    let mut spool = Spool::open(&scratch.path().join("tiny-spool"), 1).expect("spool");
    let c1 = spool.allocate_chunk().expect("alloc 1");
    spool.push(c1, &make_chunk(c1), 1, 1_000).expect("push 1");
    let c2 = spool.allocate_chunk().expect("alloc 2");
    spool.push(c2, &make_chunk(c2), 1, 1_000).expect("push 2");
    assert!(
        spool.highest_lost_chunk() >= c1,
        "pushing second chunk over 1-byte spool cap must record eviction of chunk {c1}"
    );

    let mut state = SharedState {
        server: "http://127.0.0.1".into(),
        token: None,
        agent_id: Some(1),
        need_token: false,
        spool_seq: 0,
        spool,
        attached: true,
        active_groups: 1,
        group_cut_seqs: vec![0],
        session_stop: None,
        group_triggers: Vec::new(),
        watch: None,
        watch_seq: 0,
        flush_requested: Some(PendingFlush {
            id: 42,
            deadline: std::time::Instant::now() + Duration::from_secs(10),
            target_group_cuts: vec![1],
            min_fresh_chunk: c1,
            required_chunk: None,
            any_failed: false,
        }),
        flush_completed: 0,
        flush_empty_acked: 0,
        flush_failed_id: 0,
        policy: None,
        policy_seq: 0,
        bursts: Vec::new(),
        burst_seq: 0,
        applied_policy_version: 0,
        applied_sanitizer_version: 0,
        active_projects: Vec::new(),
        active_services: Vec::new(),
        sanitizer: Arc::new(policy),
        project_sanitizers: BTreeMap::new(),
        symbol_queue: crate::agent::symbols::SymbolUploadQueue::default(),
    };
    finish_cut_locked(&mut state, 0, CutOutcome::Pushed(c2));
    assert_eq!(state.flush_failed_id, 42);
    assert_eq!(state.flush_completed, 0);
    assert!(state.flush_requested.is_none());
}

#[test]
#[ignore = "multi-process CPU budget benchmark; run in --release under microscope-bpf with -- --ignored --nocapture"]
fn measure_many_processes_under_one_cpu_budget() {
    let fixture =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("build/test-programs/hot-calls");
    assert!(
        fixture.exists(),
        "hot-calls fixture missing at {}; run `just build-test-programs` first",
        fixture.display()
    );
    for count in [1usize, 4, 8, 16] {
        let mut children: Vec<std::process::Child> = (0..count)
            .map(|_| {
                std::process::Command::new(&fixture)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .expect("spawn hot-calls")
            })
            .collect();
        let pids: Vec<u32> = children.iter().map(std::process::Child::id).collect();
        thread::sleep(Duration::from_millis(200));

        let mut options = RecordOptions::preset(Preset::Production);
        options.cuts = Cuts::Segments(Duration::from_secs(10));
        let stop = Arc::new(AtomicBool::new(false));
        let stop_timer = Arc::clone(&stop);
        let stopper = thread::spawn(move || {
            thread::sleep(Duration::from_secs(22));
            stop_timer.store(true, Ordering::SeqCst);
        });

        let trigger = AtomicBool::new(false);
        let (mut seg_idx, mut total_samples) = (0usize, 0usize);
        let (mut steady_net_ns, mut steady_wall_ns) = (0u64, 0u64);
        let (mut total_raised, mut total_kept) = (0u64, 0u64);
        let mut observed_pids: BTreeSet<u32> = BTreeSet::new();
        let mut collect = |profile: Profile| {
            let cap = profile.capture();
            total_samples += cap.samples.len();
            observed_pids.extend(cap.processes.iter().map(|p| p.pid.get()));
            let (cpu_ns, load_ns) = profile.recorder_cpu();
            let net_ns = cpu_ns
                .saturating_sub(load_ns)
                .saturating_sub(profile.recorder_setup());
            let (s, e) = profile.span();
            let span_ns = u64::try_from(e.saturating_sub(s)).unwrap_or(0);
            if let Some((raised, kept)) = profile.kept_share() {
                total_raised += raised;
                total_kept += kept;
            }
            if seg_idx == 1 && span_ns > 5_000_000_000 {
                steady_net_ns += net_ns;
                steady_wall_ns += span_ns;
            }
            seg_idx += 1;
        };
        let mut publish = |_: Profile| {};
        let mut watch = Watch {
            every: Duration::MAX,
            publish: &mut publish,
            stop: &stop,
            save: Some(Save {
                trigger: &trigger,
                to: &mut collect,
            }),
        };

        let attached =
            record_attached_many(&pids, &options, &mut watch).expect("record many processes");
        collect(attached.profile);
        let _ = stopper.join();
        for ch in &mut children {
            let _ = ch.kill();
            let _ = ch.wait();
        }

        let steady_pct = if steady_wall_ns > 0 {
            (Duration::from_nanos(steady_net_ns).as_secs_f64()
                / Duration::from_nanos(steady_wall_ns).as_secs_f64())
                * 100.0
        } else {
            0.0
        };
        let matched = pids.iter().filter(|p| observed_pids.contains(p)).count();
        eprintln!(
            "MULTI_PROC_BUDGET procs={count:>2} steady_cpu_pct={steady_pct:.3}% kept={total_kept}/{total_raised} samples={total_samples} observed_pids={matched}/{count}"
        );
        assert_eq!(matched, count, "expected all {count} PIDs to be observed");
        assert!(total_samples > 0, "expected samples for {count} PIDs");
    }
}
