//! Dense `u32` key table indexing frames, tasks, labels, and syscalls for a `Capture`.
//!
//! Building `Keys` once per `Granularity` lets aggregation, filtering, and
//! timelines operate on compact integer keys and defer string formatting until
//! presentation.

use std::borrow::Cow;

use foldhash::HashMap;

use super::{CallTree, Costs, Query, index};
use crate::CodeRole;
use crate::capture::Capture;
use crate::capture::records::{FrameKind, NONE, Truncation, role_of};

/// `Function` expands full inline chains, whereas `PhysicalFunction` keeps only
/// the outermost (non-inlined) frame in `frame_lines`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Granularity {
    #[default]
    Function,

    PhysicalFunction,

    Line,

    Address,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyKind {
    Function { function: u32 },

    Line { function: u32, file: u32, line: u32 },

    Address { frame: u32 },

    Truncation { reason: u8, module: u32 },

    Task { function: u32, frame: u32 },

    Group { task: u32 },

    Label { label: u32 },

    Syscall { abi: u8, number: u32 },
}

pub type Label<'a> = (&'a str, Option<&'a str>);

pub struct Keys<'a> {
    capture: &'a Capture,
    granularity: Granularity,
    starts: Vec<u32>,
    flat: Vec<u32>,
    kinds: Vec<KeyKind>,
    roles: Vec<CodeRole>,

    interpreted: Vec<bool>,

    tasks: Vec<u32>,

    groups: Vec<u32>,
    labels: Vec<Label<'a>>,
    label_rows: HashMap<Label<'a>, u32>,

    label_roots: Vec<u32>,

    syscalls: HashMap<KeyKind, u32>,

    spans: Vec<Vec<(i64, i64, u32)>>,

    task_labels: Vec<Vec<u32>>,

    shared: std::sync::OnceLock<std::collections::HashSet<String>>,
}

/// Maps duplicate function records (e.g. from reloaded modules) to a single canonical row.
fn canonical_functions(capture: &Capture) -> Vec<u32> {
    let mut first = HashMap::default();
    (0..)
        .zip(&capture.functions)
        .map(|(function, record)| {
            let path = match record.module.get() {
                NONE => NONE,
                module => capture.module(module).path.get(),
            };
            let identity = (
                record.name.get(),
                path,
                record.file.get(),
                record.line.get(),
                record.role,
            );
            *first.entry(identity).or_insert(function)
        })
        .collect()
}

fn frame_keys(
    capture: &Capture,
    granularity: Granularity,
    canonical: &[u32],
    frame: u32,
    key: &mut impl FnMut(KeyKind) -> u32,
    flat: &mut Vec<u32>,
) {
    let record = capture.frame(frame);
    if record.kind == FrameKind::Truncated as u8 {
        let reason = Truncation::from_number(record.address.get()).map_or(0, |reason| reason as u8);
        flat.push(key(KeyKind::Truncation {
            reason,
            module: record.module.get(),
        }));
        return;
    }

    let lines = capture.frame_lines(frame);
    let named = FrameKind::from_number(record.kind).is_some_and(FrameKind::named);
    let lines = match granularity {
        Granularity::Address if named => lines,
        Granularity::Address => &[][..],
        Granularity::PhysicalFunction => lines.last().map_or(&[][..], std::slice::from_ref),
        Granularity::Function | Granularity::Line => lines,
    };

    if lines.is_empty() {
        flat.push(key(KeyKind::Address { frame }));
    }

    let by_line =
        granularity == Granularity::Line || (granularity == Granularity::Address && named);
    for line in lines {
        #[expect(
            clippy::indexing_slicing,
            reason = "`canonical` has an entry per function, and `validate` checks a line's"
        )]
        let function = canonical[line.function.get() as usize];
        flat.push(key(if by_line {
            KeyKind::Line {
                function,
                file: line.file.get(),
                line: line.number.get(),
            }
        } else {
            KeyKind::Function { function }
        }));
    }
}

fn short_file(file: &str) -> &str {
    if file.starts_with("node:") {
        return file;
    }

    file.rsplit('/').next().unwrap_or(file)
}

#[expect(
    clippy::indexing_slicing,
    reason = "`Keys::new` gives `starts` an entry per frame and one past, each within \
              `flat`, whose keys are all below `keys`"
)]
fn interpreted(capture: &Capture, starts: &[u32], flat: &[u32], keys: usize) -> Vec<bool> {
    let mut interpreted = vec![false; keys];
    for (frame, record) in capture.frames.iter().enumerate() {
        if record.kind == FrameKind::Interpreted as u8 {
            for &key in &flat[starts[frame] as usize..starts[frame + 1] as usize] {
                interpreted[key as usize] = true;
            }
        }
    }

    interpreted
}

fn code_role(capture: &Capture, kind: KeyKind) -> CodeRole {
    let number = match kind {
        KeyKind::Function { function } | KeyKind::Line { function, .. } => {
            capture.function(function).role
        }
        KeyKind::Address { frame } => capture.frame(frame).role,
        KeyKind::Truncation { .. }
        | KeyKind::Task { .. }
        | KeyKind::Group { .. }
        | KeyKind::Label { .. }
        | KeyKind::Syscall { .. } => 0,
    };

    role_of(number).unwrap_or_default()
}

impl<'a> Keys<'a> {
    #[must_use]
    pub fn new(capture: &'a Capture, granularity: Granularity) -> Self {
        let _span = crate::span!("analysis keys");
        let mut kinds = Vec::new();
        let mut known: HashMap<KeyKind, u32> = HashMap::default();
        let mut key = |kind: KeyKind| {
            *known.entry(kind).or_insert_with(|| {
                kinds.push(kind);
                index(kinds.len() - 1)
            })
        };

        let canonical = canonical_functions(capture);
        let mut starts = Vec::with_capacity(capture.frames.len() + 1);
        let mut flat = Vec::with_capacity(capture.frames.len());
        for frame in 0..index(capture.frames.len()) {
            starts.push(index(flat.len()));
            frame_keys(capture, granularity, &canonical, frame, &mut key, &mut flat);
        }

        starts.push(index(flat.len()));

        let interpreted = interpreted(capture, &starts, &flat, kinds.len());
        let mut roles: Vec<CodeRole> = kinds.iter().map(|kind| code_role(capture, *kind)).collect();

        let mut tasks = vec![NONE; kinds.len()];
        for (slot, kind) in tasks.iter_mut().zip(kinds.clone()) {
            let task = match kind {
                KeyKind::Function { function } | KeyKind::Line { function, .. } => KeyKind::Task {
                    function,
                    frame: NONE,
                },
                KeyKind::Address { frame } => KeyKind::Task {
                    function: NONE,
                    frame,
                },
                KeyKind::Truncation { .. }
                | KeyKind::Task { .. }
                | KeyKind::Group { .. }
                | KeyKind::Label { .. }
                | KeyKind::Syscall { .. } => {
                    continue;
                }
            };

            *slot = *known.entry(task).or_insert_with(|| {
                kinds.push(task);
                roles.push(CodeRole::Ordinary);
                index(kinds.len() - 1)
            });
        }

        let mut add = |kind: KeyKind| {
            kinds.push(kind);
            roles.push(CodeRole::Ordinary);
            index(kinds.len() - 1)
        };

        let groups = (0..index(capture.tasks.len()))
            .chain([NONE])
            .map(|task| add(KeyKind::Group { task }))
            .collect();

        let found = labels(capture);
        let label_roots = (0..index(found.labels.len()))
            .chain([NONE])
            .map(|label| add(KeyKind::Label { label }))
            .collect();

        let mut syscalls = HashMap::default();
        for call in &capture.syscalls {
            let kind = KeyKind::Syscall {
                abi: call.abi,
                number: call.number.get(),
            };
            syscalls.entry(kind).or_insert_with(|| add(kind));
        }

        tasks.resize(kinds.len(), NONE);
        Self {
            capture,
            granularity,
            starts,
            flat,
            kinds,
            roles,
            interpreted,
            tasks,
            groups,
            labels: found.labels,
            label_rows: found.rows,
            label_roots,
            syscalls,
            spans: found.spans,
            task_labels: found.tasks,
            shared: std::sync::OnceLock::new(),
        }
    }

    #[must_use]
    pub fn labels(&self) -> &[Label<'a>] {
        &self.labels
    }

    #[expect(clippy::indexing_slicing, reason = "label rows are rows of `labels`")]
    pub fn label_of(&self, label: u32) -> Label<'a> {
        self.labels[label as usize]
    }

    #[must_use]
    pub fn label(&self, key: &str, value: Option<&str>) -> Option<u32> {
        self.label_rows.get(&(key, value)).copied()
    }

    #[must_use]
    pub fn label_key(&self, label: u32) -> u32 {
        self.label_roots
            .get(label as usize)
            .copied()
            .or_else(|| self.label_roots.last().copied())
            .unwrap_or(NONE)
    }

    pub fn syscall(&self, abi: u8, number: u32) -> u32 {
        self.syscalls
            .get(&KeyKind::Syscall { abi, number })
            .copied()
            .unwrap_or(NONE)
    }

    pub fn spans(&self, thread: u32) -> &[(i64, i64, u32)] {
        self.spans.get(thread as usize).map_or(&[], Vec::as_slice)
    }

    pub fn task_labels(&self, task: u32) -> &[u32] {
        self.task_labels
            .get(task as usize)
            .map_or(&[], Vec::as_slice)
    }

    #[must_use]
    pub const fn granularity(&self) -> Granularity {
        self.granularity
    }

    #[must_use]
    pub const fn count(&self) -> usize {
        self.kinds.len()
    }

    #[must_use]
    #[expect(
        clippy::indexing_slicing,
        reason = "`starts` has an entry per frame of the capture and one past, each within `flat`"
    )]
    pub fn frame(&self, frame: u32) -> &[u32] {
        let frame = frame as usize;
        &self.flat[self.starts[frame] as usize..self.starts[frame + 1] as usize]
    }

    #[must_use]
    #[expect(clippy::indexing_slicing, reason = "keys are rows of `kinds`")]
    pub fn kind(&self, key: u32) -> KeyKind {
        self.kinds[key as usize]
    }

    #[must_use]
    #[expect(clippy::indexing_slicing, reason = "`roles` has an entry per key")]
    pub fn role(&self, key: u32) -> CodeRole {
        self.roles[key as usize]
    }

    #[must_use]
    pub fn interpreted(&self, key: u32) -> bool {
        self.interpreted.get(key as usize).copied().unwrap_or(false)
    }

    #[must_use]
    #[expect(clippy::indexing_slicing, reason = "`tasks` has an entry per key")]
    pub fn task(&self, key: u32) -> Option<u32> {
        Some(self.tasks[key as usize]).filter(|task| *task != NONE)
    }

    #[must_use]
    pub fn group(&self, task: u32) -> u32 {
        self.groups
            .get(task as usize)
            .copied()
            .or_else(|| self.groups.last().copied())
            .unwrap_or(NONE)
    }

    pub fn find(&self, kind: KeyKind) -> Option<u32> {
        self.kinds
            .iter()
            .position(|known| *known == kind)
            .map(index)
    }

    #[must_use]
    pub fn named(&self, name: &str) -> Vec<u32> {
        let named: Vec<u32> = (0..index(self.count()))
            .filter(|key| self.name(*key) == name)
            .collect();
        if !named.is_empty() {
            return named;
        }
        let qualified: Vec<u32> = (0..index(self.count()))
            .filter(|key| self.qualified(*key) == name)
            .collect();
        if !qualified.is_empty() || name.is_empty() {
            return qualified;
        }
        (0..index(self.count()))
            .filter(|key| {
                self.name(*key)
                    .strip_prefix(name)
                    .is_some_and(|rest| rest.starts_with('('))
            })
            .collect()
    }

    #[must_use]
    pub fn similar(&self, name: &str, limit: usize) -> Vec<u32> {
        let wanted = name.to_lowercase();
        if wanted.is_empty() {
            return Vec::new();
        }
        let mut found: Vec<(usize, u32)> = (0..index(self.count()))
            .filter(|&key| {
                matches!(
                    self.kind(key),
                    KeyKind::Function { .. } | KeyKind::Line { .. }
                )
            })
            .filter_map(|key| {
                let name = self.name(key);
                name.to_lowercase()
                    .contains(&wanted)
                    .then_some((name.len(), key))
            })
            .collect();
        found.sort_unstable();
        found.into_iter().take(limit).map(|(_, key)| key).collect()
    }

    #[must_use]
    pub fn qualified(&self, key: u32) -> String {
        let mut parts = Vec::new();
        if let Some(module) = self.module(key) {
            parts.push(module.to_owned());
        }
        if let Some((file, line)) = self.location(key) {
            parts.push(format!("{file}:{line}"));
        }
        format!("{} [{}]", self.name(key), parts.join(", "))
    }

    #[must_use]
    pub fn exact_name(&self, key: u32) -> String {
        let shared = self.shared.get_or_init(|| {
            let mut seen = std::collections::HashSet::new();
            (0..index(self.count()))
                .map(|key| self.name(key))
                .filter(|name| !seen.insert(name.clone()))
                .collect()
        });
        let name = self.name(key);
        if shared.contains(&name) {
            self.qualified(key)
        } else {
            name
        }
    }

    #[must_use]
    pub fn aggregate(&self, query: &Query) -> CallTree {
        super::aggregate(self.capture, self, query)
    }

    #[must_use]
    pub fn syscalls(&self, query: &Query) -> Vec<bool> {
        super::syscalls(self.capture, self, query)
    }

    #[must_use]
    pub fn functions(&self, query: &Query) -> Costs {
        super::functions(self.capture, self, query)
    }

    #[must_use]
    pub fn density(
        &self,
        query: &Query,
        buckets: usize,
        lanes: super::Lanes,
    ) -> Vec<(u32, Vec<u64>)> {
        super::density(self.capture, self, query, buckets, lanes)
    }

    pub fn name(&self, key: u32) -> String {
        let capture = self.capture;
        match self.kind(key) {
            KeyKind::Function { function } => self.function_name(function).into_owned(),
            KeyKind::Line {
                function,
                file,
                line,
            } => {
                let name = self.function_name(function);
                match file {
                    NONE if line == 0 => name.into_owned(),
                    NONE => format!("{name}:{line}"),
                    file => {
                        let path = capture.string(file);
                        let file = path.rsplit('/').next().unwrap_or(path);
                        format!("{name} {file}:{line}")
                    }
                }
            }
            KeyKind::Address { frame } => self.address_name(frame),
            KeyKind::Task { function, frame } => {
                let root = match function {
                    NONE => self.address_name(frame),
                    function => self.function_name(function).into_owned(),
                };

                format!("[task] {root}")
            }
            KeyKind::Group { task: NONE } => "[no task known]".to_owned(),
            KeyKind::Group { task } => {
                let task = capture.task(task);
                let name = format!(
                    "{} {}",
                    capture.string(task.runtime.get()),
                    task.number.get()
                );
                match task.labels.get() {
                    NONE => name,
                    labels => {
                        let labels: Vec<&str> = capture.string(labels).lines().collect();
                        format!("{name} {{{}}}", labels.join(", "))
                    }
                }
            }
            KeyKind::Syscall { abi, number } => {
                let call = crate::syscalls::SyscallId {
                    abi: crate::syscalls::Abi::from_index(abi),
                    number,
                };
                format!("[syscall] {}", call.display_name())
            }
            KeyKind::Label { label: NONE } => "[no such label]".to_owned(),
            KeyKind::Label { label } => match self.label_of(label) {
                (key, Some(value)) => format!("{key}={value}"),
                (key, None) => format!("[no {key}]"),
            },
            KeyKind::Truncation { reason, module } => {
                let reason = Truncation::from_number(u64::from(reason))
                    .map_or("unknown", Truncation::describe);
                self.module_name(module).map_or_else(
                    || format!("[{reason}]"),
                    |module| format!("[{reason}: {module}]"),
                )
            }
        }
    }

    fn function_name(&self, function: u32) -> Cow<'a, str> {
        let capture = self.capture;
        let record = capture.function(function);
        let name = capture.string(record.name.get());
        if !name.is_empty() || record.module.get() != NONE {
            return Cow::Borrowed(name);
        }

        match (record.file.get(), record.line.get()) {
            (NONE, _) => Cow::Borrowed("<anonymous>"),
            (file, 0) => format!("<anonymous {}>", short_file(capture.string(file))).into(),
            (file, line) => {
                format!("<anonymous {}:{line}>", short_file(capture.string(file))).into()
            }
        }
    }

    #[must_use]
    pub fn location(&self, key: u32) -> Option<(&'a str, u32)> {
        let capture = self.capture;
        let (file, line) = match self.kind(key) {
            KeyKind::Line { file, line, .. } => (file, line),
            KeyKind::Function { function } => {
                let function = capture.function(function);
                (function.file.get(), function.line.get())
            }
            KeyKind::Task { function, .. } if function != NONE => {
                let function = capture.function(function);
                (function.file.get(), function.line.get())
            }

            KeyKind::Address { .. }
            | KeyKind::Truncation { .. }
            | KeyKind::Task { .. }
            | KeyKind::Group { .. }
            | KeyKind::Label { .. }
            | KeyKind::Syscall { .. } => {
                return None;
            }
        };

        (file != NONE).then(|| (capture.string(file), line))
    }

    #[must_use]
    pub fn module(&self, key: u32) -> Option<&'a str> {
        let capture = self.capture;
        let module = match self.kind(key) {
            KeyKind::Function { function }
            | KeyKind::Line { function, .. }
            | KeyKind::Task {
                function,
                frame: NONE,
            } => capture.function(function).module.get(),
            KeyKind::Address { frame } | KeyKind::Task { frame, .. } => {
                capture.frame(frame).module.get()
            }
            KeyKind::Truncation { module, .. } => module,
            KeyKind::Group { .. } | KeyKind::Label { .. } | KeyKind::Syscall { .. } => NONE,
        };

        self.module_name(module)
    }

    fn address_name(&self, frame: u32) -> String {
        let record = self.capture.frame(frame);
        let address = record.address.get();
        self.module_name(record.module.get()).map_or_else(
            || format!("{address:#x}"),
            |module| format!("{module}+{address:#x}"),
        )
    }

    fn module_name(&self, module: u32) -> Option<&'a str> {
        let capture = self.capture;
        (module != NONE).then(|| {
            let path = capture.string(capture.module(module).path.get());
            path.rsplit('/').next().unwrap_or(path)
        })
    }
}

struct Found<'a> {
    labels: Vec<Label<'a>>,
    rows: HashMap<Label<'a>, u32>,
    spans: Vec<Vec<(i64, i64, u32)>>,
    tasks: Vec<Vec<u32>>,
}

fn row<'a>(found: &mut Found<'a>, key: &'a str, value: &'a str) -> u32 {
    let mut row = |label| {
        *found.rows.entry(label).or_insert_with(|| {
            found.labels.push(label);
            index(found.labels.len() - 1)
        })
    };

    row((key, None));
    row((key, Some(value)))
}

fn labels(capture: &Capture) -> Found<'_> {
    let mut found = Found {
        labels: Vec::new(),
        rows: HashMap::default(),
        spans: vec![Vec::new(); capture.threads.len()],
        tasks: vec![Vec::new(); capture.tasks.len()],
    };

    for range in &capture.zone_ranges {
        for zone in capture.range_zones(range) {
            let site = capture.site(zone.site.get());
            if site.kind != microscope_probes::abi::SITE_LABEL {
                continue;
            }

            let (key, value) = (
                capture.string(site.name.get()),
                capture.string(zone.value.get()),
            );
            let label = row(&mut found, key, value);

            if let Some(spans) = found.spans.get_mut(range.thread.get() as usize) {
                spans.push((zone.start.get(), zone.end.get(), label));
            }
        }
    }

    for spans in &mut found.spans {
        // Place outer spans before inner spans at the same start time so `rfind`
        // in `LabelQuery::judge` picks the innermost active label.
        spans.sort_by_key(|&(start, end, _)| (start, std::cmp::Reverse(end)));
    }

    let mut tasks = std::mem::take(&mut found.tasks);
    for (labels, record) in tasks.iter_mut().zip(&capture.tasks) {
        if record.labels.get() == NONE {
            continue;
        }

        for line in capture.string(record.labels.get()).lines() {
            if let Some((key, value)) = line.split_once('=') {
                labels.push(row(&mut found, key, value));
            }
        }
    }

    found.tasks = tasks;

    found
}
