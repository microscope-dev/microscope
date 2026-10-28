//! What each frame stands for, as dense keys.

use std::borrow::Cow;

use foldhash::HashMap;

use super::{CallTree, Costs, Query, index};
use crate::CodeRole;
use crate::capture::Capture;
use crate::capture::records::{FrameKind, NONE, Truncation, role_of};

/// How finely frames are told apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Granularity {
    /// By function, each inlined function a frame of its own.
    #[default]
    Function,

    /// By function, without the functions inlined into it.
    PhysicalFunction,

    /// By source line of each function.
    Line,

    /// By instruction.
    Address,
}

/// What a key stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyKind {
    /// A function, by its row in the capture's functions.
    Function { function: u32 },

    /// A line of a function: its file, or [`NONE`], and number.
    Line { function: u32, file: u32, line: u32 },

    /// A frame no symbols describe, or any frame at address granularity.
    Address { frame: u32 },

    /// Where a stack ended early, and the module at fault, or [`NONE`].
    Truncation { reason: u8, module: u32 },

    /// The root of an async task's logical stack, by the outermost code of
    /// its own: a function, or a frame no symbols describe, the other
    /// [`NONE`].
    Task { function: u32, frame: u32 },

    /// A task of a language runtime, by its row, under which a query by
    /// task roots each path: [`NONE`] for what no task snapshot named.
    Group { task: u32 },

    /// A label, by its row in [`Keys::labels`], under which a query by
    /// label roots each path: [`NONE`] for a key no label has.
    Label { label: u32 },
}

/// A label: a key, and its value, or `None` for its absence.
pub type Label<'a> = (&'a str, Option<&'a str>);

/// Each frame's keys, innermost first, and what each key is.
pub struct Keys<'a> {
    capture: &'a Capture,
    granularity: Granularity,
    starts: Vec<u32>,
    flat: Vec<u32>,
    kinds: Vec<KeyKind>,
    roles: Vec<CodeRole>,

    /// Whether each key is of an interpreted frame, by key; keys made
    /// after the frames' are of none.
    interpreted: Vec<bool>,

    /// Each key's task root, or [`NONE`] for a key that roots none.
    tasks: Vec<u32>,

    /// The group key of each task, by its row, and last, of none.
    groups: Vec<u32>,
    labels: Vec<Label<'a>>,
    label_rows: HashMap<Label<'a>, u32>,

    /// The key of each label, by its row, and last, of a key no label has.
    label_roots: Vec<u32>,

    /// Each thread's label spans, by its row: start, end, and label, by
    /// start.
    spans: Vec<Vec<(i64, i64, u32)>>,

    /// Each task's labels, by its row.
    task_labels: Vec<Vec<u32>>,
}

/// Adds the keys of `frame`, innermost first, to `flat`, each made by
/// `key`.
fn frame_keys(
    capture: &Capture,
    granularity: Granularity,
    frame: u32,
    key: &mut impl FnMut(KeyKind) -> u32,
    flat: &mut Vec<u32>,
) {
    let record = &capture.frames[frame as usize];
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
        // A frame keyed by a string has no address to show: its line
        // stands for one.
        Granularity::Address if named => lines,
        Granularity::Address => &[][..],
        // The function the code is compiled into is the last line.
        Granularity::PhysicalFunction => lines.last().map_or(&[][..], std::slice::from_ref),
        Granularity::Function | Granularity::Line => lines,
    };

    if lines.is_empty() {
        flat.push(key(KeyKind::Address { frame }));
    }

    let by_line =
        granularity == Granularity::Line || (granularity == Granularity::Address && named);
    for line in lines {
        let function = line.function.get();
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

/// The file an anonymous function is declared in, as its name shows it: a
/// path by its last part, and a name that is no path, such as Node's
/// `node:internal/modules/cjs/loader`, whole.
fn short_file(file: &str) -> &str {
    if file.starts_with("node:") {
        return file;
    }

    file.rsplit('/').next().unwrap_or(file)
}

/// Whether each of `keys` keys is of an interpreted frame, given each
/// frame's keys, which frame `i`'s are `flat[starts[i]..starts[i + 1]]`.
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

/// The role of a key's code; keys of no code are ordinary.
fn code_role(capture: &Capture, kind: KeyKind) -> CodeRole {
    let number = match kind {
        KeyKind::Function { function } | KeyKind::Line { function, .. } => {
            capture.functions[function as usize].role
        }
        KeyKind::Address { frame } => capture.frames[frame as usize].role,
        KeyKind::Truncation { .. }
        | KeyKind::Task { .. }
        | KeyKind::Group { .. }
        | KeyKind::Label { .. } => 0,
    };

    role_of(number).unwrap_or_default()
}

impl<'a> Keys<'a> {
    pub(crate) fn new(capture: &'a Capture, granularity: Granularity) -> Self {
        let mut kinds = Vec::new();
        let mut known: HashMap<KeyKind, u32> = HashMap::default();
        let mut key = |kind: KeyKind| {
            *known.entry(kind).or_insert_with(|| {
                kinds.push(kind);
                index(kinds.len() - 1)
            })
        };

        let mut starts = Vec::with_capacity(capture.frames.len() + 1);
        let mut flat = Vec::with_capacity(capture.frames.len());
        for frame in 0..index(capture.frames.len()) {
            starts.push(index(flat.len()));
            frame_keys(capture, granularity, frame, &mut key, &mut flat);
        }

        starts.push(index(flat.len()));

        let interpreted = interpreted(capture, &starts, &flat, kinds.len());
        let mut roles: Vec<CodeRole> = kinds.iter().map(|kind| code_role(capture, *kind)).collect();

        // A task is rooted by function, whatever the granularity.
        let mut tasks = vec![NONE; kinds.len()];
        for (code, kind) in kinds.clone().into_iter().enumerate() {
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
                | KeyKind::Label { .. } => {
                    continue;
                }
            };

            tasks[code] = *known.entry(task).or_insert_with(|| {
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
            spans: found.spans,
            task_labels: found.tasks,
        }
    }

    /// Every label, by row.
    #[must_use]
    pub fn labels(&self) -> &[Label<'a>] {
        &self.labels
    }

    /// The row of a label, if any span or task has it, or for `None`, if
    /// any has its key.
    #[must_use]
    pub fn label(&self, key: &str, value: Option<&str>) -> Option<u32> {
        self.label_rows.get(&(key, value)).copied()
    }

    /// The key a query by label roots a label's paths at, by its row, or
    /// [`NONE`] for a key no label has.
    #[must_use]
    pub fn label_key(&self, label: u32) -> u32 {
        self.label_roots
            .get(label as usize)
            .copied()
            .unwrap_or(self.label_roots[self.label_roots.len() - 1])
    }

    /// A thread's label spans, by start: start, end, and label.
    pub(crate) fn spans(&self, thread: u32) -> &[(i64, i64, u32)] {
        self.spans.get(thread as usize).map_or(&[], Vec::as_slice)
    }

    /// A task's labels, by its row.
    pub(crate) fn task_labels(&self, task: u32) -> &[u32] {
        self.task_labels
            .get(task as usize)
            .map_or(&[], Vec::as_slice)
    }

    #[must_use]
    pub const fn granularity(&self) -> Granularity {
        self.granularity
    }

    /// How many keys there are.
    #[must_use]
    pub const fn count(&self) -> usize {
        self.kinds.len()
    }

    /// A frame's keys, innermost first.
    #[must_use]
    pub fn frame(&self, frame: u32) -> &[u32] {
        let frame = frame as usize;
        &self.flat[self.starts[frame] as usize..self.starts[frame + 1] as usize]
    }

    #[must_use]
    pub fn kind(&self, key: u32) -> KeyKind {
        self.kinds[key as usize]
    }

    /// The role of a key's code.
    #[must_use]
    pub fn role(&self, key: u32) -> CodeRole {
        self.roles[key as usize]
    }

    /// Whether a key is of a frame an interpreter runs.
    #[must_use]
    pub fn interpreted(&self, key: u32) -> bool {
        self.interpreted.get(key as usize).copied().unwrap_or(false)
    }

    /// The root of a task whose outermost code of its own is `key`'s.
    #[must_use]
    pub fn task(&self, key: u32) -> Option<u32> {
        Some(self.tasks[key as usize]).filter(|task| *task != NONE)
    }

    /// The key a query by task roots `task`'s paths at, by its row, or
    /// [`NONE`] for what no task snapshot named.
    #[must_use]
    pub fn group(&self, task: u32) -> u32 {
        self.groups
            .get(task as usize)
            .copied()
            .unwrap_or(self.groups[self.groups.len() - 1])
    }

    /// The key of `kind`, if any frame has it.
    pub fn find(&self, kind: KeyKind) -> Option<u32> {
        self.kinds
            .iter()
            .position(|known| *known == kind)
            .map(index)
    }

    /// Every key a view names `name`.
    #[must_use]
    pub fn named(&self, name: &str) -> Vec<u32> {
        (0..index(self.count()))
            .filter(|key| self.name(*key) == name)
            .collect()
    }

    /// The call tree of the samples `query` selects.
    #[must_use]
    pub fn aggregate(&self, query: &Query) -> CallTree {
        super::aggregate(self.capture, self, query)
    }

    /// What each key costs under `query`, most own weight first.
    #[must_use]
    pub fn functions(&self, query: &Query) -> Costs {
        super::functions(self.capture, self, query)
    }

    /// The weight of what `query` keeps in each of `buckets` slices of its
    /// range, per lane with any.
    #[must_use]
    pub fn density(
        &self,
        query: &Query,
        buckets: usize,
        lanes: super::Lanes,
    ) -> Vec<(u32, Vec<u64>)> {
        super::density(self.capture, self, query, buckets, lanes)
    }

    /// How a view names a key: a function's name, an address by module and
    /// offset, or a truncation's reason.
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
                    // Code with no line, as a library without debug
                    // information has, is its function alone.
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
                let task = &capture.tasks[task as usize];
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
            KeyKind::Label { label: NONE } => "[no such label]".to_owned(),
            KeyKind::Label { label } => match self.labels[label as usize] {
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

    /// A function's name as views show it. V8 names an anonymous function
    /// "", so code an interpreter runs, which is in no module, is named for
    /// where it is declared instead, as V8's stack traces name it. A
    /// native function keeps its name, even an empty one.
    fn function_name(&self, function: u32) -> Cow<'a, str> {
        let capture = self.capture;
        let record = &capture.functions[function as usize];
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

    /// The source file and line a key names, if any: a line's own, or where
    /// a function is declared.
    #[must_use]
    pub fn location(&self, key: u32) -> Option<(&'a str, u32)> {
        let capture = self.capture;
        let (file, line) = match self.kind(key) {
            KeyKind::Line { file, line, .. } => (file, line),
            KeyKind::Function { function } => {
                let function = &capture.functions[function as usize];
                (function.file.get(), function.line.get())
            }
            KeyKind::Task { function, .. } if function != NONE => {
                let function = &capture.functions[function as usize];
                (function.file.get(), function.line.get())
            }

            KeyKind::Address { .. }
            | KeyKind::Truncation { .. }
            | KeyKind::Task { .. }
            | KeyKind::Group { .. }
            | KeyKind::Label { .. } => {
                return None;
            }
        };

        (file != NONE).then(|| (capture.string(file), line))
    }

    /// The module a key's code belongs to, by its file name.
    #[must_use]
    pub fn module(&self, key: u32) -> Option<&'a str> {
        let capture = self.capture;
        let module = match self.kind(key) {
            KeyKind::Function { function }
            | KeyKind::Line { function, .. }
            | KeyKind::Task {
                function,
                frame: NONE,
            } => capture.functions[function as usize].module.get(),
            KeyKind::Address { frame } | KeyKind::Task { frame, .. } => {
                capture.frames[frame as usize].module.get()
            }
            KeyKind::Truncation { module, .. } => module,
            KeyKind::Group { .. } | KeyKind::Label { .. } => NONE,
        };

        self.module_name(module)
    }

    fn address_name(&self, frame: u32) -> String {
        let record = &self.capture.frames[frame as usize];
        let address = record.address.get();
        self.module_name(record.module.get()).map_or_else(
            || format!("{address:#x}"),
            |module| format!("{module}+{address:#x}"),
        )
    }

    fn module_name(&self, module: u32) -> Option<&'a str> {
        let capture = self.capture;
        (module != NONE).then(|| {
            let path = capture.string(capture.modules[module as usize].path.get());
            path.rsplit('/').next().unwrap_or(path)
        })
    }
}

/// A capture's labels, which spans of a thread's time and tasks carry.
struct Found<'a> {
    labels: Vec<Label<'a>>,
    rows: HashMap<Label<'a>, u32>,
    spans: Vec<Vec<(i64, i64, u32)>>,
    tasks: Vec<Vec<u32>>,
}

/// The row of the label `key=value`, and of its key's absence.
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

/// Every label: each a key and a value, and each key's absence.
fn labels(capture: &Capture) -> Found<'_> {
    let mut found = Found {
        labels: Vec::new(),
        rows: HashMap::default(),
        spans: vec![Vec::new(); capture.threads.len()],
        tasks: vec![Vec::new(); capture.tasks.len()],
    };

    for range in &capture.zone_ranges {
        let start = range.start.get() as usize;
        for zone in &capture.zones[start..start + range.count.get() as usize] {
            let site = &capture.sites[zone.site.get() as usize];
            if site.kind != microscope_probes::abi::SITE_LABEL {
                continue;
            }

            let (key, value) = (
                capture.string(site.name.get()),
                capture.string(zone.value.get()),
            );
            let label = row(&mut found, key, value);
            found.spans[range.thread.get() as usize].push((
                zone.start.get(),
                zone.end.get(),
                label,
            ));
        }
    }

    // Of spans that begin together, the inner, shorter one comes after,
    // and of two alike, the deeper, as each thread's depths came in order.
    for spans in &mut found.spans {
        spans.sort_by_key(|&(start, end, _)| (start, std::cmp::Reverse(end)));
    }

    for (task, record) in capture.tasks.iter().enumerate() {
        if record.labels.get() == NONE {
            continue;
        }

        for line in capture.string(record.labels.get()).lines() {
            if let Some((key, value)) = line.split_once('=') {
                let label = row(&mut found, key, value);
                found.tasks[task].push(label);
            }
        }
    }

    found
}
