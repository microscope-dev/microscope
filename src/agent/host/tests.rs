use super::{Root, boot_id, capabilities, container, count_cpus, host_ids, hostname, system};
use crate::agent::scratch::Scratch;

fn tree(files: &[(&str, &str)]) -> Scratch {
    let scratch = Scratch::new();
    for (path, text) in files {
        let path = scratch.path().join(path.trim_start_matches('/'));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    scratch
}

#[test]
fn the_system_reads_from_proc_sys_and_os_release() {
    let scratch = tree(&[
        ("/proc/sys/kernel/hostname", "jlinux1\n"),
        ("/proc/sys/kernel/osrelease", "7.2.11-200.fc44.x86_64\n"),
        (
            "/proc/sys/kernel/random/boot_id",
            "0C4F8E57-2B8A-4D3E-9F61-0D2C5A1B7E93\n",
        ),
        (
            "/etc/os-release",
            "NAME=\"Fedora Linux\"\nVERSION_ID=44\nID=fedora\nPRETTY_NAME=\"Fedora Linux 44 (Workstation Edition)\"\n",
        ),
        ("/sys/devices/system/cpu/online", "0-15,32-47\n"),
        (
            "/proc/meminfo",
            "MemTotal:       131072000 kB\nMemFree: 1 kB\n",
        ),
    ]);
    let root = Root::at(scratch.path());
    assert_eq!(hostname(&root).as_deref(), Some("jlinux1"));
    assert_eq!(
        boot_id(&root).as_deref(),
        Some("0c4f8e57-2b8a-4d3e-9f61-0d2c5a1b7e93")
    );

    let system = system(&root);
    assert_eq!(system.kernel.as_deref(), Some("7.2.11-200.fc44.x86_64"));
    assert_eq!(
        system.os.as_deref(),
        Some("Fedora Linux 44 (Workstation Edition)")
    );
    assert_eq!(system.os_id.as_deref(), Some("fedora"));
    assert_eq!(system.os_version.as_deref(), Some("44"));
    assert_eq!(system.cpus, Some(32));
    assert_eq!(system.memory_mib, Some(128_000));
}

#[test]
fn a_bare_root_reports_what_it_can() {
    let scratch = Scratch::new();
    let root = Root::at(scratch.path());
    assert_eq!(hostname(&root), None);
    assert_eq!(boot_id(&root), None);
    assert_eq!(system(&root).kernel, None);
    assert_eq!(capabilities(&root), None);
    assert_eq!(container(&root, false), None);
    assert!(host_ids(&root).is_empty());
}

#[test]
fn cpu_lists_count_ranges_and_refuse_nonsense() {
    assert_eq!(count_cpus("0"), Some(1));
    assert_eq!(count_cpus("0-3,8,10-11"), Some(7));
    assert_eq!(count_cpus("3-1"), None);
    assert_eq!(count_cpus("a"), None);
}

#[test]
fn capabilities_say_whether_bpf_and_perf_are_allowed() {
    let read = |cap_eff: &str| {
        let scratch = tree(&[(
            "/proc/self/status",
            &format!("Name: microscope\nCapEff:\t{cap_eff}\n"),
        )]);
        capabilities(&Root::at(scratch.path())).unwrap()
    };
    assert_eq!(read("0000000000000000"), Vec::<&str>::new());
    assert_eq!(read("000000c000080000"), ["bpf", "perfmon", "sys_ptrace"]);
    assert_eq!(read("0000000000200000"), ["bpf", "perfmon", "sys_admin"]);
    assert_eq!(
        read("000001ffffffffff"),
        ["bpf", "perfmon", "sys_ptrace", "sys_admin"]
    );
}

#[test]
fn containers_are_recognized_by_their_markers() {
    let docker = tree(&[("/.dockerenv", ""), ("/proc/1/cgroup", "0::/\n")]);
    assert_eq!(
        container(&Root::at(docker.path()), false).unwrap().runtime,
        "docker"
    );
    let pod = tree(&[("/proc/1/cgroup", "0::/kubepods/besteffort/pod1234/abc\n")]);
    assert_eq!(
        container(&Root::at(pod.path()), false).unwrap().runtime,
        "kubernetes"
    );
    let podman = tree(&[("/run/.containerenv", "")]);
    assert_eq!(
        container(&Root::at(podman.path()), false).unwrap().runtime,
        "podman"
    );
    let host = tree(&[("/proc/1/cgroup", "0::/init.scope\n")]);
    assert_eq!(container(&Root::at(host.path()), false), None);
    assert_eq!(
        container(&Root::at(host.path()), true).unwrap().runtime,
        "kubernetes"
    );
}

#[test]
fn host_ids_are_keyed_hashes_and_placeholders_are_dropped() {
    let scratch = tree(&[
        ("/etc/machine-id", "4f2c9a7e1b0d4c3e8a5f6b7c8d9e0f1a\n"),
        (
            "/sys/class/dmi/id/product_uuid",
            "EC2A1B2C-3D4E-5F60-7182-93A4B5C6D7E8\n",
        ),
    ]);
    let ids = host_ids(&Root::at(scratch.path()));
    assert_eq!(ids.len(), 2);
    assert!(ids[0].starts_with("machine-id:"));
    assert!(ids[1].starts_with("smbios-uuid:"));
    assert!(
        !ids.iter()
            .any(|id| id.contains("4f2c9a7e") || id.contains("ec2a1b2c"))
    );
    assert_eq!(ids, host_ids(&Root::at(scratch.path())));

    assert_eq!(
        ids[0],
        "machine-id:936c3ed45158f7381e210efbb97366e2fb65121ed4abc7f06848b541520282d4"
    );

    for placeholder in [
        "03000200-0400-0500-0006-000700080009",
        "FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF",
        "00000000-0000-0000-0000-000000000000",
        "not-a-uuid",
    ] {
        let scratch = tree(&[("/sys/class/dmi/id/product_uuid", placeholder)]);
        assert!(
            host_ids(&Root::at(scratch.path())).is_empty(),
            "{placeholder}"
        );
    }
    let empty = tree(&[("/etc/machine-id", "uninitialized\n")]);
    assert!(host_ids(&Root::at(empty.path())).is_empty());
}
