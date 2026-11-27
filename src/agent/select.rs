//! Scans `/proc` and container/systemd metadata to classify running processes
//! against local and remote selection rules (with pod and environment opt-out
//! taking precedence).

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read as _,
    os::unix::fs::MetadataExt as _,
    path::{Component, Path, PathBuf},
};

use nix::{
    fcntl::{OFlag, openat, readlinkat},
    sys::stat::Mode,
};

use super::host::Root;

const MAX_ANNOTATION_BYTES: u64 = 256 * 1024;
const MAX_OCI_CONFIG_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleKind {
    Process,
    Container,
    Image,
    Unit,
}

impl RuleKind {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Container => "container",
            Self::Image => "image",
            Self::Unit => "unit",
        }
    }

    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "process" => Some(Self::Process),
            "container" => Some(Self::Container),
            "image" => Some(Self::Image),
            "unit" => Some(Self::Unit),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleAction {
    Record,
    Exclude,
}

impl RuleAction {
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "record" | "include" => Some(Self::Record),
            "exclude" | "ignore" => Some(Self::Exclude),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionRule {
    pub kind: RuleKind,
    pub pattern: String,
    pub action: RuleAction,
    pub project: Option<String>,
    pub service: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessMetadata {
    pub pid: u32,
    pub comm: String,
    pub exe_path: Option<String>,
    pub exe_name: Option<String>,
    pub build_id: Option<String>,
    pub systemd_unit: Option<String>,
    pub container_id: Option<String>,
    pub container_name: Option<String>,
    pub container_image: Option<String>,
    pub k8s_pod_name: Option<String>,
    pub k8s_namespace: Option<String>,
    pub pod_record: Option<bool>,
    pub pod_project: Option<String>,
    pub pod_service: Option<String>,
    pub env_record: Option<bool>,
    pub env_project: Option<String>,
    pub env_service: Option<String>,
}

impl ProcessMetadata {
    #[must_use]
    pub fn program_name(&self) -> &str {
        self.exe_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.comm)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedProcess {
    pub pid: u32,
    pub program: String,
    pub project: Option<String>,
    pub service: String,
    pub service_explicit: bool,
    pub matched_by: String,
    pub meta: ProcessMetadata,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectionDecision {
    Selected(Box<SelectedProcess>),
    Excluded,
    Unmatched,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanResult {
    pub selected: Vec<SelectedProcess>,
    pub excluded_pids: BTreeSet<u32>,
}

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub runtime_dirs: Vec<PathBuf>,
    pub pod_annotations_dir: Option<PathBuf>,
    pub self_pid: u32,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            runtime_dirs: default_runtime_dirs(),
            pod_annotations_dir: None,
            self_pid: std::process::id(),
        }
    }
}

#[must_use]
pub fn default_runtime_dirs() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/run/containerd/io.containerd.runtime.v2.task/k8s.io"),
        PathBuf::from("/run/containerd/io.containerd.runtime.v2.task/default"),
        PathBuf::from("/run/containers/storage/overlay-containers"),
        PathBuf::from("/var/lib/docker/containers"),
    ]
}

#[derive(Default)]
pub struct BuildIdCache {
    entries: BTreeMap<(u64, u64), Option<String>>,
}

impl BuildIdCache {
    pub fn get_or_read(&mut self, exe_path: &Path) -> Option<String> {
        let meta = fs::metadata(exe_path).ok()?;
        let key = (meta.dev(), meta.ino());
        if let Some(cached) = self.entries.get(&key) {
            return cached.clone();
        }
        let parsed = read_elf_build_id(exe_path);
        if self.entries.len() > 2048 {
            self.entries.clear();
        }
        self.entries.insert(key, parsed.clone());
        parsed
    }
}

fn read_elf_build_id(path: &Path) -> Option<String> {
    microscope_data::symbols::read_elf_build_id_from_file(path)
        .ok()
        .flatten()
        .map(|bid| bid.as_str().to_owned())
}

#[must_use]
pub fn glob_matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0_usize, 0_usize);
    let mut star_pattern: Option<usize> = None;
    let mut star_text = 0_usize;

    while let Some(&tc) = t.get(ti) {
        if p.get(pi).is_some_and(|&pc| pc == '?' || pc == tc) {
            pi += 1;
            ti += 1;
        } else if p.get(pi) == Some(&'*') {
            star_pattern = Some(pi);
            pi += 1;
            star_text = ti;
        } else if let Some(sp) = star_pattern {
            pi = sp + 1;
            star_text += 1;
            ti = star_text;
        } else {
            return false;
        }
    }
    while p.get(pi) == Some(&'*') {
        pi += 1;
    }
    pi == p.len()
}

fn rule_matches(rule: &SelectionRule, meta: &ProcessMetadata) -> bool {
    match rule.kind {
        RuleKind::Process => {
            meta.exe_name
                .as_deref()
                .is_some_and(|n| glob_matches(&rule.pattern, n))
                || (!meta.comm.is_empty() && glob_matches(&rule.pattern, &meta.comm))
        }
        RuleKind::Container => meta
            .container_name
            .as_deref()
            .is_some_and(|c| glob_matches(&rule.pattern, c)),
        RuleKind::Image => meta
            .container_image
            .as_deref()
            .is_some_and(|img| glob_matches(&rule.pattern, img)),
        RuleKind::Unit => meta
            .systemd_unit
            .as_deref()
            .is_some_and(|u| glob_matches(&rule.pattern, u)),
    }
}

#[must_use]
pub fn classify_selection(
    meta: ProcessMetadata,
    local_rules: &[SelectionRule],
    remote_rules: &[SelectionRule],
    allow_remote_rules: bool,
) -> SelectionDecision {
    if meta.pod_record == Some(false) || meta.env_record == Some(false) {
        return SelectionDecision::Excluded;
    }

    let active_remote: &[SelectionRule] = if allow_remote_rules {
        remote_rules
    } else {
        &[]
    };
    for rule in local_rules.iter().chain(active_remote.iter()) {
        if rule.action == RuleAction::Exclude && rule_matches(rule, &meta) {
            return SelectionDecision::Excluded;
        }
    }

    let program = meta.program_name().to_owned();
    if program.is_empty() {
        return SelectionDecision::Unmatched;
    }

    if meta.pod_record == Some(true) || meta.pod_project.is_some() {
        let project = meta.pod_project.clone().or_else(|| {
            local_rules
                .iter()
                .chain(active_remote.iter())
                .find(|r| r.action == RuleAction::Record && rule_matches(r, &meta))
                .and_then(|r| r.project.clone())
        });
        let explicit_service = meta
            .pod_service
            .clone()
            .or_else(|| meta.container_name.clone());
        let service_explicit = explicit_service.is_some();
        let service = explicit_service.unwrap_or_else(|| program.clone());
        return SelectionDecision::Selected(Box::new(SelectedProcess {
            pid: meta.pid,
            program,
            project,
            service,
            service_explicit,
            matched_by: "pod annotation".to_owned(),
            meta,
        }));
    }

    if meta.env_record == Some(true) || meta.env_project.is_some() {
        let project = meta.env_project.clone().or_else(|| {
            local_rules
                .iter()
                .chain(active_remote.iter())
                .find(|r| r.action == RuleAction::Record && rule_matches(r, &meta))
                .and_then(|r| r.project.clone())
        });
        let service_explicit = meta.env_service.is_some();
        let service = meta.env_service.clone().unwrap_or_else(|| program.clone());
        let matched_by = if meta.env_project.is_some() {
            "env: MICROSCOPE_PROJECT"
        } else {
            "env: MICROSCOPE_RECORD"
        }
        .to_owned();
        return SelectionDecision::Selected(Box::new(SelectedProcess {
            pid: meta.pid,
            program,
            project,
            service,
            service_explicit,
            matched_by,
            meta,
        }));
    }

    for rule in local_rules.iter().chain(active_remote.iter()) {
        if rule.action == RuleAction::Record && rule_matches(rule, &meta) {
            let explicit_service = rule
                .service
                .clone()
                .or_else(|| meta.pod_service.clone())
                .or_else(|| meta.env_service.clone())
                .or_else(|| meta.container_name.clone());
            let service_explicit = explicit_service.is_some();
            let service = explicit_service.unwrap_or_else(|| program.clone());
            let matched_by = format!("rule: {} {}", rule.kind.name(), rule.pattern);
            return SelectionDecision::Selected(Box::new(SelectedProcess {
                pid: meta.pid,
                program,
                project: rule.project.clone(),
                service,
                service_explicit,
                matched_by,
                meta,
            }));
        }
    }

    SelectionDecision::Unmatched
}

#[must_use]
pub fn evaluate_selection(
    meta: ProcessMetadata,
    local_rules: &[SelectionRule],
    remote_rules: &[SelectionRule],
    allow_remote_rules: bool,
) -> Option<SelectedProcess> {
    match classify_selection(meta, local_rules, remote_rules, allow_remote_rules) {
        SelectionDecision::Selected(chosen) => Some(*chosen),
        SelectionDecision::Excluded | SelectionDecision::Unmatched => None,
    }
}

pub fn scan_selected_processes(
    root: &Root,
    options: &ScanOptions,
    local_rules: &[SelectionRule],
    remote_rules: &[SelectionRule],
    allow_remote_rules: bool,
    build_ids: &mut BuildIdCache,
) -> ScanResult {
    let proc_dir = root.path("/proc");
    let Ok(entries) = fs::read_dir(&proc_dir) else {
        return ScanResult::default();
    };
    let mut pids: Vec<u32> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| pid != options.self_pid)
        .collect();
    pids.sort_unstable();

    let mut container_cache: BTreeMap<String, ContainerMeta> = BTreeMap::new();
    let mut result = ScanResult::default();
    for pid in pids {
        let Some(meta) = read_process_metadata(root, pid, options, &mut container_cache) else {
            continue;
        };
        match classify_selection(meta, local_rules, remote_rules, allow_remote_rules) {
            SelectionDecision::Selected(mut chosen) => {
                let exe_link = root.path(&format!("/proc/{pid}/exe"));
                chosen.meta.build_id = build_ids.get_or_read(&exe_link);
                result.selected.push(*chosen);
            }
            SelectionDecision::Excluded => {
                result.excluded_pids.insert(pid);
            }
            SelectionDecision::Unmatched => {}
        }
    }
    result
}

#[derive(Clone, Default)]
struct ContainerMeta {
    container_name: Option<String>,
    container_image: Option<String>,
    k8s_pod_name: Option<String>,
    k8s_namespace: Option<String>,
    pod_record: Option<bool>,
    pod_project: Option<String>,
    pod_service: Option<String>,
}

fn read_process_metadata(
    root: &Root,
    pid: u32,
    options: &ScanOptions,
    container_cache: &mut BTreeMap<String, ContainerMeta>,
) -> Option<ProcessMetadata> {
    let pdir = root.path(&format!("/proc/{pid}"));
    let comm = fs::read_to_string(pdir.join("comm"))
        .ok()?
        .trim()
        .to_owned();
    if comm.is_empty() {
        return None;
    }

    let exe_link = pdir.join("exe");
    let (exe_path, exe_name) = fs::read_link(&exe_link).map_or((None, None), |target| {
        let raw = target.to_string_lossy().into_owned();
        let cleaned = raw.strip_suffix(" (deleted)").unwrap_or(&raw).to_owned();
        let base = Path::new(&cleaned)
            .file_name()
            .and_then(|f| f.to_str())
            .map(str::to_owned);
        (Some(cleaned), base)
    });

    let build_id = None;

    let (env_record, env_project, env_service) = read_allowed_env(&pdir.join("environ"));

    let cgroup_text = fs::read_to_string(pdir.join("cgroup")).unwrap_or_default();
    let (systemd_unit, container_id) = parse_cgroup(&cgroup_text);

    let mut cmeta = container_id
        .as_ref()
        .map_or_else(ContainerMeta::default, |cid| {
            container_cache.get(cid).cloned().unwrap_or_else(|| {
                let loaded = load_container_meta(root, cid, &options.runtime_dirs);
                container_cache.insert(cid.clone(), loaded.clone());
                loaded
            })
        });

    let downward_paths =
        downward_annotation_paths(root, pid, options.pod_annotations_dir.as_deref());
    for path in downward_paths {
        if let Some(text) = read_bounded_regular_file(&path, MAX_ANNOTATION_BYTES) {
            apply_downward_annotations(&text, &mut cmeta);
        }
    }

    Some(ProcessMetadata {
        pid,
        comm,
        exe_path,
        exe_name,
        build_id,
        systemd_unit,
        container_id,
        container_name: cmeta.container_name,
        container_image: cmeta.container_image,
        k8s_pod_name: cmeta.k8s_pod_name,
        k8s_namespace: cmeta.k8s_namespace,
        pod_record: cmeta.pod_record,
        pod_project: cmeta.pod_project,
        pod_service: cmeta.pod_service,
        env_record,
        env_project,
        env_service,
    })
}

fn downward_annotation_paths(root: &Root, pid: u32, extra_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(dir) = extra_dir {
        paths.push(dir.join(pid.to_string()));
        paths.push(dir.join("annotations"));
    }
    paths.push(root.path(&format!("/proc/{pid}/root/etc/podinfo/annotations")));
    paths
}

fn read_allowed_env(path: &Path) -> (Option<bool>, Option<String>, Option<String>) {
    let Ok(bytes) = fs::read(path) else {
        return (None, None, None);
    };
    let mut record = None;
    let mut project = None;
    let mut service = None;
    for entry in bytes.split(|&b| b == 0) {
        let Ok(text) = std::str::from_utf8(entry) else {
            continue;
        };
        if let Some(val) = text.strip_prefix("MICROSCOPE_RECORD=") {
            record = parse_bool_flag(val);
        } else if let Some(val) = text.strip_prefix("MICROSCOPE_PROJECT=") {
            let trimmed = val.trim();
            if !trimmed.is_empty() && trimmed.len() <= 128 && !trimmed.chars().any(char::is_control)
            {
                project = Some(trimmed.to_owned());
            }
        } else if let Some(val) = text.strip_prefix("MICROSCOPE_SERVICE=") {
            let trimmed = val.trim();
            if !trimmed.is_empty() && trimmed.len() <= 128 && !trimmed.chars().any(char::is_control)
            {
                service = Some(trimmed.to_owned());
            }
        }
    }
    (record, project, service)
}

fn parse_bool_flag(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Some(true),
        "false" | "0" | "no" => Some(false),
        _ => None,
    }
}

#[must_use]
pub fn parse_cgroup(cgroup: &str) -> (Option<String>, Option<String>) {
    let mut unit = None;
    let mut container_id = None;
    for line in cgroup.lines() {
        let path = line.rsplit(':').next().unwrap_or("");
        for segment in path.split('/').filter(|s| !s.is_empty()) {
            if unit.is_none() && segment.ends_with(".service") {
                unit = Some(segment.to_owned());
            }
            if container_id.is_none() {
                container_id = extract_container_id(segment);
            }
        }
    }
    (unit, container_id)
}

fn extract_container_id(segment: &str) -> Option<String> {
    let stripped = segment.strip_suffix(".scope").unwrap_or(segment);
    for prefix in [
        "cri-containerd-",
        "crio-",
        "docker-",
        "libpod-",
        "containerd-",
    ] {
        if let Some(rest) = stripped.strip_prefix(prefix)
            && is_hex_container_id(rest)
        {
            return Some(rest.to_owned());
        }
    }
    if is_hex_container_id(stripped) {
        return Some(stripped.to_owned());
    }
    None
}

fn is_hex_container_id(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn open_confined_regular_file(path: &Path) -> Option<File> {
    let parent = path.parent()?;
    let leaf = path.file_name()?;
    let root_fd = openat(
        nix::fcntl::AT_FDCWD,
        parent,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
        Mode::empty(),
    )
    .ok()?;
    let mut dir_fd = root_fd.try_clone().ok()?;
    let mut pending: std::collections::VecDeque<PathBuf> =
        std::collections::VecDeque::from([PathBuf::from(leaf)]);
    let mut symlinks_followed = 0_u8;

    while let Some(next_path) = pending.pop_front() {
        let mut components: Vec<std::ffi::OsString> = Vec::new();
        for comp in next_path.components() {
            match comp {
                Component::Normal(seg) => components.push(seg.to_os_string()),
                Component::CurDir => {}
                Component::RootDir | Component::Prefix(_) | Component::ParentDir => return None,
            }
        }
        if components.is_empty() {
            return None;
        }
        let last_idx = components.len() - 1;
        for (idx, seg) in components.iter().enumerate() {
            let has_more_here = idx < last_idx;
            let is_last = !has_more_here && pending.is_empty();
            let flags = if is_last {
                OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC
            } else {
                OFlag::O_RDONLY
                    | OFlag::O_DIRECTORY
                    | OFlag::O_NOFOLLOW
                    | OFlag::O_NONBLOCK
                    | OFlag::O_CLOEXEC
            };
            match openat(&dir_fd, seg.as_os_str(), flags, Mode::empty()) {
                Ok(fd) => {
                    if is_last {
                        let file = File::from(fd);
                        return file.metadata().ok()?.file_type().is_file().then_some(file);
                    }
                    dir_fd = fd;
                }
                Err(nix::errno::Errno::ELOOP | nix::errno::Errno::ENOTDIR) => {
                    symlinks_followed = symlinks_followed.saturating_add(1);
                    if symlinks_followed > 8 {
                        return None;
                    }
                    let link_target = readlinkat(&dir_fd, seg.as_os_str()).ok()?;
                    let target_path = PathBuf::from(link_target);
                    if target_path.is_absolute() {
                        return None;
                    }
                    if let Some(tail) = components.get(idx + 1..)
                        && !tail.is_empty()
                    {
                        let mut rest = PathBuf::new();
                        for rem in tail {
                            rest.push(rem);
                        }
                        pending.push_front(rest);
                    }
                    pending.push_front(target_path);
                    break;
                }
                Err(_) => return None,
            }
        }
    }
    None
}

fn read_bounded_regular_file(path: &Path, max_bytes: u64) -> Option<String> {
    let file = open_confined_regular_file(path)?;
    let meta = file.metadata().ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    let cap = usize::try_from(meta.len().min(max_bytes)).unwrap_or(0);
    let mut buf = Vec::with_capacity(cap);
    file.take(max_bytes).read_to_end(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}

fn load_container_meta(root: &Root, container_id: &str, runtime_dirs: &[PathBuf]) -> ContainerMeta {
    let mut meta = ContainerMeta::default();
    for dir in runtime_dirs {
        let base = if dir.is_absolute() {
            root.path(&dir.to_string_lossy())
        } else {
            dir.clone()
        };
        for rel in ["config.json", "userdata/config.json"] {
            let candidate = base.join(container_id).join(rel);
            let Some(text) = read_bounded_regular_file(&candidate, MAX_OCI_CONFIG_BYTES) else {
                continue;
            };
            let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            if let Some(anns) = val
                .get("annotations")
                .and_then(serde_json::Value::as_object)
            {
                apply_oci_annotations(anns, &mut meta);
                return meta;
            }
        }
    }
    meta
}

fn apply_oci_annotations(
    anns: &serde_json::Map<String, serde_json::Value>,
    meta: &mut ContainerMeta,
) {
    let get_str = |key: &str| {
        anns.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    if meta.k8s_pod_name.is_none() {
        meta.k8s_pod_name =
            get_str("io.kubernetes.pod.name").or_else(|| get_str("io.kubernetes.cri.sandbox-name"));
    }
    if meta.k8s_namespace.is_none() {
        meta.k8s_namespace = get_str("io.kubernetes.pod.namespace")
            .or_else(|| get_str("io.kubernetes.cri.sandbox-namespace"));
    }
    if meta.container_name.is_none() {
        meta.container_name = get_str("io.kubernetes.container.name")
            .or_else(|| get_str("io.kubernetes.cri.container-name"));
    }
    if meta.container_image.is_none() {
        meta.container_image = get_str("io.kubernetes.cri.image-name")
            .or_else(|| get_str("io.kubernetes.cri-o.ImageName"))
            .or_else(|| get_str("org.opencontainers.image.ref.name"));
    }
    if let Some(v) = get_str("microscope.dev/record") {
        meta.pod_record = parse_bool_flag(&v);
    }
    if let Some(v) = get_str("microscope.dev/project") {
        meta.pod_project = Some(v);
    }
    if let Some(v) = get_str("microscope.dev/service") {
        meta.pod_service = Some(v);
    }
    if let Some(nested_raw) = get_str("io.kubernetes.cri-o.Annotations")
        && let Ok(serde_json::Value::Object(nested)) =
            serde_json::from_str::<serde_json::Value>(&nested_raw)
    {
        if let Some(v) = nested
            .get("microscope.dev/record")
            .and_then(serde_json::Value::as_str)
        {
            meta.pod_record = parse_bool_flag(v);
        }
        if let Some(v) = nested
            .get("microscope.dev/project")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            meta.pod_project = Some(v.to_owned());
        }
        if let Some(v) = nested
            .get("microscope.dev/service")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            meta.pod_service = Some(v.to_owned());
        }
    }
}

fn apply_downward_annotations(text: &str, meta: &mut ContainerMeta) {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, raw_val)) = trimmed.split_once('=') else {
            continue;
        };
        let val = unquote_annotation(raw_val.trim());
        match key.trim() {
            "microscope.dev/record" => meta.pod_record = parse_bool_flag(&val),
            "microscope.dev/project" if !val.is_empty() => meta.pod_project = Some(val),
            "microscope.dev/service" if !val.is_empty() => meta.pod_service = Some(val),
            _ => {}
        }
    }
}

fn unquote_annotation(raw: &str) -> String {
    let inner = raw
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(raw);
    inner.replace("\\\"", "\"").replace("\\\\", "\\")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::scratch::Scratch;

    fn write_proc(root: &Path, pid: u32, comm: &str, cgroup: &str, environ: &[(&str, &str)]) {
        let dir = root.join(format!("proc/{pid}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("comm"), format!("{comm}\n")).unwrap();
        fs::write(dir.join("cgroup"), cgroup).unwrap();
        let mut env_bytes = Vec::new();
        for (k, v) in environ {
            env_bytes.extend_from_slice(k.as_bytes());
            env_bytes.push(b'=');
            env_bytes.extend_from_slice(v.as_bytes());
            env_bytes.push(0);
        }
        fs::write(dir.join("environ"), env_bytes).unwrap();
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "sets up full fake /proc, container config.json, and rule selection test matrix"
    )]
    fn scans_pod_annotations_env_and_rules_without_touching_unselected() {
        let scratch = Scratch::new();
        let root_path = scratch.path();
        let cid = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        let oci_dir = root_path.join(format!(
            "run/containerd/io.containerd.runtime.v2.task/k8s.io/{cid}"
        ));
        fs::create_dir_all(&oci_dir).unwrap();
        fs::write(
            oci_dir.join("config.json"),
            serde_json::json!({
                "annotations": {
                    "io.kubernetes.pod.name": "checkout-abc",
                    "io.kubernetes.pod.namespace": "prod",
                    "io.kubernetes.container.name": "checkout-api",
                    "io.kubernetes.cri.image-name": "ghcr.io/acme/checkout:v2",
                    "microscope.dev/record": "true",
                    "microscope.dev/project": "checkout"
                }
            })
            .to_string(),
        )
        .unwrap();

        write_proc(
            root_path,
            101,
            "checkout-bin",
            &format!("0::/kubepods.slice/kubepods-burstable.slice/cri-containerd-{cid}.scope\n"),
            &[("DB_PASSWORD", "supersecret")],
        );
        write_proc(
            root_path,
            102,
            "billing-worker",
            "0::/system.slice/billing.service\n",
            &[
                ("MICROSCOPE_PROJECT", "billing"),
                ("AWS_SECRET_ACCESS_KEY", "never-read"),
            ],
        );
        write_proc(
            root_path,
            103,
            "third-program",
            "0::/system.slice/third.service\n",
            &[],
        );
        write_proc(
            root_path,
            104,
            "remote-app",
            "0::/system.slice/remote.service\n",
            &[],
        );

        let root = Root::at(root_path);
        let opts = ScanOptions {
            runtime_dirs: default_runtime_dirs(),
            pod_annotations_dir: None,
            self_pid: 99999,
        };
        let remote_rules = vec![SelectionRule {
            kind: RuleKind::Unit,
            pattern: "remote.service".into(),
            action: RuleAction::Record,
            project: Some("ops".into()),
            service: Some("remote-svc".into()),
        }];

        let mut cache = BuildIdCache::default();
        let scan = scan_selected_processes(&root, &opts, &[], &remote_rules, true, &mut cache);
        let selected = scan.selected;
        let pids: Vec<u32> = selected.iter().map(|s| s.pid).collect();
        assert_eq!(pids, vec![101, 102, 104]);
        assert_eq!(selected[0].project.as_deref(), Some("checkout"));
        assert_eq!(selected[0].matched_by, "pod annotation");
        assert!(selected[0].service_explicit);
        assert_eq!(
            selected[0].meta.k8s_pod_name.as_deref(),
            Some("checkout-abc")
        );
        assert_eq!(selected[1].project.as_deref(), Some("billing"));
        assert_eq!(selected[1].matched_by, "env: MICROSCOPE_PROJECT");
        assert!(!selected[1].service_explicit);
        assert_eq!(selected[2].project.as_deref(), Some("ops"));
        assert_eq!(selected[2].service, "remote-svc");
        assert!(selected[2].service_explicit);

        let refused = scan_selected_processes(&root, &opts, &[], &remote_rules, false, &mut cache);
        let refused_pids: Vec<u32> = refused.selected.iter().map(|s| s.pid).collect();
        assert_eq!(refused_pids, vec![101, 102]);

        let exclude_local = vec![SelectionRule {
            kind: RuleKind::Process,
            pattern: "checkout-*".into(),
            action: RuleAction::Exclude,
            project: None,
            service: None,
        }];
        let after_exclude = scan_selected_processes(
            &root,
            &opts,
            &exclude_local,
            &remote_rules,
            false,
            &mut cache,
        );
        assert_eq!(
            after_exclude
                .selected
                .iter()
                .map(|s| s.pid)
                .collect::<Vec<_>>(),
            vec![102]
        );
        assert!(after_exclude.excluded_pids.contains(&101));
    }

    #[test]
    fn downward_api_annotations_file_selects_and_excludes() {
        let scratch = Scratch::new();
        let root_path = scratch.path();
        write_proc(root_path, 201, "api-srv", "0::/\n", &[]);
        let podinfo = root_path.join("proc/201/root/etc/podinfo");
        fs::create_dir_all(&podinfo).unwrap();
        fs::write(
            podinfo.join("annotations"),
            "microscope.dev/record=\"true\"\nmicroscope.dev/project=\"payments\"\nmicroscope.dev/service=\"pay-api\"\n",
        )
        .unwrap();

        let root = Root::at(root_path);
        let opts = ScanOptions {
            runtime_dirs: Vec::new(),
            pod_annotations_dir: None,
            self_pid: 99999,
        };
        let mut cache = BuildIdCache::default();
        let scan = scan_selected_processes(&root, &opts, &[], &[], true, &mut cache);
        assert_eq!(scan.selected.len(), 1);
        assert_eq!(scan.selected[0].project.as_deref(), Some("payments"));
        assert_eq!(scan.selected[0].service, "pay-api");
        assert!(scan.selected[0].service_explicit);
    }

    #[test]
    fn downward_api_annotations_fifo_does_not_block_scanner() {
        let scratch = Scratch::new();
        let root_path = scratch.path();
        write_proc(root_path, 301, "tricky-srv", "0::/\n", &[]);
        let podinfo = root_path.join("proc/301/root/etc/podinfo");
        fs::create_dir_all(&podinfo).unwrap();
        let fifo = podinfo.join("annotations");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();

        let root = Root::at(root_path);
        let opts = ScanOptions {
            runtime_dirs: Vec::new(),
            pod_annotations_dir: None,
            self_pid: 99999,
        };
        let mut cache = BuildIdCache::default();
        let scan = scan_selected_processes(&root, &opts, &[], &[], true, &mut cache);
        assert!(scan.selected.is_empty());
    }

    #[test]
    fn downward_api_k8s_projected_symlinks_work_while_escaping_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;

        let scratch = Scratch::new();
        let root_path = scratch.path();
        write_proc(root_path, 401, "k8s-app", "0::/\n", &[]);
        let podinfo = root_path.join("proc/401/root/etc/podinfo");
        let data_ts = podinfo.join("..2026_11_22_00_00_00.123456");
        fs::create_dir_all(&data_ts).unwrap();
        fs::write(
            data_ts.join("annotations"),
            "microscope.dev/record=\"true\"\nmicroscope.dev/project=\"checkout\"\n",
        )
        .unwrap();
        symlink("..2026_11_22_00_00_00.123456", podinfo.join("..data")).unwrap();
        symlink("..data/annotations", podinfo.join("annotations")).unwrap();

        write_proc(root_path, 402, "escape-app", "0::/\n", &[]);
        let outside = root_path.join("outside-annotations");
        fs::write(&outside, "microscope.dev/record=\"true\"\n").unwrap();
        let esc_podinfo = root_path.join("proc/402/root/etc/podinfo");
        fs::create_dir_all(&esc_podinfo).unwrap();
        symlink(&outside, esc_podinfo.join("annotations")).unwrap();

        let root = Root::at(root_path);
        let opts = ScanOptions {
            runtime_dirs: Vec::new(),
            pod_annotations_dir: None,
            self_pid: 99999,
        };
        let mut cache = BuildIdCache::default();
        let scan = scan_selected_processes(&root, &opts, &[], &[], true, &mut cache);
        assert_eq!(
            scan.selected.iter().map(|s| s.pid).collect::<Vec<_>>(),
            vec![401],
            "K8s projected symlink must work while escaping symlink must be rejected"
        );
        assert_eq!(scan.selected[0].project.as_deref(), Some("checkout"));
    }
}
