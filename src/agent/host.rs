use std::{os::unix::fs::MetadataExt as _, path::PathBuf};

use ring::hmac;
use serde::Serialize;

const HOST_ID_MESSAGE: &[u8] = b"microscope host id v1";
const PROC_PID_INIT_INO: u64 = 0xEFFF_FFFC;

const CAP_SYS_PTRACE: u32 = 19;
const CAP_SYS_ADMIN: u32 = 21;
const CAP_PERFMON: u32 = 38;
const CAP_BPF: u32 = 39;

const PLACEHOLDER_UUIDS: [&str; 4] = [
    "00000000-0000-0000-0000-000000000000",
    "ffffffff-ffff-ffff-ffff-ffffffffffff",
    "03000200-0400-0500-0006-000700080009",
    "00020003-0004-0005-0006-000700080009",
];

pub struct Root(PathBuf);

impl Root {
    #[must_use]
    pub fn system() -> Self {
        Self(PathBuf::from("/"))
    }

    #[cfg(test)]
    #[must_use]
    pub fn at(path: &std::path::Path) -> Self {
        Self(path.to_path_buf())
    }

    #[must_use]
    pub fn path(&self, path: &str) -> PathBuf {
        self.0.join(path.trim_start_matches('/'))
    }

    #[must_use]
    pub fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.path(path))
            .ok()
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
    }

    #[must_use]
    pub fn exists(&self, path: &str) -> bool {
        self.path(path).exists()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct System {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    pub arch: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u64>,
}

#[must_use]
pub fn hostname(root: &Root) -> Option<String> {
    root.read("/proc/sys/kernel/hostname")
}

#[must_use]
pub fn boot_id(root: &Root) -> Option<String> {
    root.read("/proc/sys/kernel/random/boot_id")
        .filter(|id| id.len() == 36)
        .map(|id| id.to_ascii_lowercase())
}

#[must_use]
pub fn system(root: &Root) -> System {
    let release = root
        .read("/etc/os-release")
        .or_else(|| root.read("/usr/lib/os-release"))
        .unwrap_or_default();
    let field = |name: &str| {
        release.lines().find_map(|line| {
            let value = line.strip_prefix(name)?.strip_prefix('=')?;
            let value = value.trim().trim_matches('"').trim_matches('\'');
            (!value.is_empty()).then(|| value.to_owned())
        })
    };

    System {
        kernel: root.read("/proc/sys/kernel/osrelease"),
        os: field("PRETTY_NAME").or_else(|| field("NAME")),
        os_id: field("ID"),
        os_version: field("VERSION_ID"),
        arch: std::env::consts::ARCH,
        cpus: root
            .read("/sys/devices/system/cpu/online")
            .and_then(|online| count_cpus(&online)),
        memory_mib: root.read("/proc/meminfo").and_then(|meminfo| {
            meminfo.lines().find_map(|line| {
                let kib: u64 = line
                    .strip_prefix("MemTotal:")?
                    .trim()
                    .strip_suffix("kB")?
                    .trim()
                    .parse()
                    .ok()?;
                Some(kib / 1024)
            })
        }),
    }
}

fn count_cpus(online: &str) -> Option<u32> {
    online.split(',').try_fold(0_u32, |count, range| {
        let (low, high) = range.split_once('-').unwrap_or((range, range));
        let (low, high): (u32, u32) = (low.trim().parse().ok()?, high.trim().parse().ok()?);
        count.checked_add(high.checked_sub(low)?.checked_add(1)?)
    })
}

#[must_use]
pub fn capabilities(root: &Root) -> Option<Vec<&'static str>> {
    let status = root.read("/proc/self/status")?;
    let hex = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))?
        .trim();
    let effective = u64::from_str_radix(hex, 16).ok()?;
    let has = |cap: u32| effective & (1 << cap) != 0;

    let mut names = Vec::new();
    if has(CAP_BPF) || has(CAP_SYS_ADMIN) {
        names.push("bpf");
    }
    if has(CAP_PERFMON) || has(CAP_SYS_ADMIN) {
        names.push("perfmon");
    }
    if has(CAP_SYS_PTRACE) {
        names.push("sys_ptrace");
    }
    if has(CAP_SYS_ADMIN) {
        names.push("sys_admin");
    }
    Some(names)
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Container {
    pub runtime: &'static str,
    pub host_pids: bool,
}

#[must_use]
pub fn container(root: &Root, in_kubernetes: bool) -> Option<Container> {
    let cgroup = root.read("/proc/1/cgroup").unwrap_or_default();
    let runtime = if in_kubernetes || cgroup.contains("kubepods") {
        "kubernetes"
    } else if root.exists("/.dockerenv") || cgroup.contains("docker") {
        "docker"
    } else if root.exists("/run/.containerenv") || cgroup.contains("libpod") {
        "podman"
    } else if cgroup.contains("containerd")
        || root
            .read("/proc/1/environ")
            .is_some_and(|env| env.contains("container="))
    {
        "other"
    } else {
        return None;
    };

    let host_pids = std::fs::metadata(root.path("/proc/self/ns/pid"))
        .is_ok_and(|namespace| namespace.ino() == PROC_PID_INIT_INO);
    Some(Container { runtime, host_pids })
}

#[must_use]
pub fn host_ids(root: &Root) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(id) = root.read("/etc/machine-id").and_then(|id| hex16(&id)) {
        ids.push(format!("machine-id:{}", keyed(&id)));
    }
    if let Some(id) = root
        .read("/sys/class/dmi/id/product_uuid")
        .map(|uuid| uuid.to_ascii_lowercase())
        .filter(|uuid| !PLACEHOLDER_UUIDS.contains(&uuid.as_str()))
        .and_then(|uuid| hex16(&uuid.replace('-', "")))
    {
        ids.push(format!("smbios-uuid:{}", keyed(&id)));
    }
    ids
}

fn hex16(text: &str) -> Option<[u8; 16]> {
    if text.len() != 32 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    (bytes != [0; 16]).then_some(bytes)
}

fn keyed(raw: &[u8; 16]) -> String {
    let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, raw), HOST_ID_MESSAGE);
    super::wire::hex(tag.as_ref())
}

#[cfg(test)]
mod tests;
