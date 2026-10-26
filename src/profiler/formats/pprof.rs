//! pprof's `profile.proto`, gzipped as Go writes it: read into a capture,
//! and written from a call tree, so that what Go's tools show of an
//! exported profile is what microscope's views show.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::Read as _;

use super::proto::{Fields, Message};
use crate::analysis::{CallTree, KeyKind, Keys};
use crate::capture::Capture;
use crate::capture::builder::{CaptureBuilder, LineInput};
use crate::capture::records::{FrameKind, NONE, Truncation};
use crate::model::{Pid, Tid};

/// The most bytes a gzipped profile may grow to.
const MOST_BYTES: u64 = 256 << 20;

/// Where frames go whose location has no address of its own.
const UNADDRESSED: u64 = 1 << 63;

/// Whether `bytes` begin as gzip does.
pub fn is_gzip(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x1f, 0x8b])
}

/// A profile's fields, each message still in its bytes.
#[derive(Default)]
struct Raw<'a> {
    strings: Vec<Cow<'a, str>>,
    types: Vec<(u64, u64)>,
    samples: Vec<&'a [u8]>,
    mappings: Vec<&'a [u8]>,
    locations: Vec<&'a [u8]>,
    functions: Vec<&'a [u8]>,
    time: i64,
    duration: i64,
    default_type: u64,
    comments: Vec<u64>,
}

impl<'a> Raw<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, String> {
        let mut raw = Self::default();
        for field in Fields::new(bytes) {
            match field? {
                (1, value) => {
                    let mut kind = (0, 0);
                    for field in Fields::new(value.bytes("a sample type")?) {
                        match field? {
                            (1, value) => kind.0 = value.number("a sample type's name")?,
                            (2, value) => kind.1 = value.number("a sample type's unit")?,
                            _ => {}
                        }
                    }

                    raw.types.push(kind);
                }
                (2, value) => raw.samples.push(value.bytes("a sample")?),
                (3, value) => raw.mappings.push(value.bytes("a mapping")?),
                (4, value) => raw.locations.push(value.bytes("a location")?),
                (5, value) => raw.functions.push(value.bytes("a function")?),
                (6, value) => raw
                    .strings
                    .push(String::from_utf8_lossy(value.bytes("a string")?)),
                (9, value) => raw.time = value.number("the time")?.cast_signed(),
                (10, value) => raw.duration = value.number("the duration")?.cast_signed(),
                (13, value) => value.numbers(&mut raw.comments, "a comment")?,
                (14, value) => raw.default_type = value.number("the default sample type")?,
                _ => {}
            }
        }

        if raw.strings.first().is_none_or(|first| !first.is_empty()) {
            return Err("its string table does not begin with the empty string".to_owned());
        }

        Ok(raw)
    }

    fn string(&self, index: u64) -> Result<&str, String> {
        usize::try_from(index)
            .ok()
            .and_then(|index| self.strings.get(index))
            .map(AsRef::as_ref)
            .ok_or_else(|| format!("string {index} is past the string table"))
    }
}

/// A function, by its strings.
#[derive(Default, Clone, Copy)]
struct Function {
    name: u64,
    system_name: u64,
    file: u64,
    line: u64,
}

/// Reads a pprof profile, gzipped or not. The values of its default sample
/// type become the samples' weights; its samples carry no times or
/// threads, and the capture says so.
pub fn import(bytes: &[u8]) -> Result<Capture, String> {
    let bytes = if is_gzip(bytes) {
        let mut plain = Vec::new();
        flate2::read::MultiGzDecoder::new(bytes)
            .take(MOST_BYTES + 1)
            .read_to_end(&mut plain)
            .map_err(|error| format!("it does not decompress: {error}"))?;
        if plain.len() as u64 > MOST_BYTES {
            return Err(format!("it decompresses to more than {MOST_BYTES} bytes"));
        }

        Cow::Owned(plain)
    } else {
        Cow::Borrowed(bytes)
    };

    let raw = Raw::parse(&bytes)?;
    let mut builder = CaptureBuilder::new();
    let weigh = weighing(&raw, &mut builder)?;
    let modules = mappings(&raw, &mut builder)?;
    let functions = functions(&raw)?;
    let frames = locations(&raw, &mut builder, &modules, &functions)?;
    let (time, end) = (raw.time, raw.time.saturating_add(raw.duration.max(0)));
    let thread = builder.thread(Pid::new(0), Tid::new(0), time);
    builder.thread_name(thread, "every thread");
    let mut tasks: BTreeMap<String, u32> = BTreeMap::new();
    let mut ids = Vec::new();
    let mut values = Vec::new();
    let mut total = 0_u64;

    for sample in &raw.samples {
        ids.clear();
        values.clear();
        let mut labels = Vec::new();

        for field in Fields::new(sample) {
            match field? {
                (1, value) => value.numbers(&mut ids, "a sample's location")?,
                (2, value) => value.numbers(&mut values, "a sample's value")?,
                (3, value) => labels.push(label(&raw, value.bytes("a label")?)?),
                _ => {}
            }
        }

        let Some(weight) = weigh.weight(&values)? else {
            continue;
        };

        total = total
            .checked_add(weight)
            .ok_or_else(|| format!("the samples weigh more than {} in all", u64::MAX))?;

        let mut stack = Vec::with_capacity(ids.len());
        for id in &ids {
            let frame = frames
                .get(id)
                .ok_or_else(|| format!("a sample names location {id}, which it lacks"))?;
            stack.push(*frame);
        }

        if stack.is_empty() {
            stack.push(builder.truncation(Truncation::NoStack, NONE));
        }

        let stack = builder.stack(&stack);
        let task = if labels.is_empty() {
            NONE
        } else {
            labels.sort();
            let text = labels.join("\n");
            let next = tasks.len() as u64 + 1;
            *tasks.entry(text).or_insert_with_key(|text| {
                let task = builder.task("pprof labels", next, time);
                builder.describe_task(task, NONE, Some(text));
                task
            })
        };

        builder.sample(thread, time, weight, stack, task);
    }

    builder.thread_exit(Tid::new(0), end);
    builder.setting("sample times", "none");

    let comments: Result<Vec<&str>, String> = raw
        .comments
        .iter()
        .map(|comment| raw.string(*comment))
        .collect();
    let comments = comments?.join("\n");
    if !comments.is_empty() {
        builder.setting("comments", &comments);
    }

    Ok(builder.finish())
}

/// Which of a sample's values is its weight, and in what multiple of it.
struct Weighing {
    chosen: usize,
    scale: u64,
}

impl Weighing {
    /// A sample's weight, or `None` when it weighs nothing.
    fn weight(&self, values: &[u64]) -> Result<Option<u64>, String> {
        let value = values
            .get(self.chosen)
            .ok_or_else(|| {
                format!(
                    "a sample has {} values, not {}",
                    values.len(),
                    self.chosen + 1
                )
            })?
            .cast_signed();
        if value < 0 {
            return Err("a sample's value is negative, as a diff's are".to_owned());
        }

        value
            .cast_unsigned()
            .checked_mul(self.scale)
            .map(|weight| (weight > 0).then_some(weight))
            .ok_or_else(|| "a sample's value overflows nanoseconds".to_owned())
    }
}

/// The weight of the profile's default sample type, or of its last, as
/// Go's tools choose, which the capture's settings record.
fn weighing(raw: &Raw<'_>, builder: &mut CaptureBuilder) -> Result<Weighing, String> {
    let chosen = raw
        .types
        .iter()
        .position(|(name, _)| raw.default_type != 0 && *name == raw.default_type)
        .or_else(|| raw.types.len().checked_sub(1))
        .ok_or_else(|| "it has no sample types".to_owned())?;

    let (name, unit) = raw.types[chosen];
    let (name, unit) = (raw.string(name)?, raw.string(unit)?);
    let (weight, scale) = match unit {
        "nanoseconds" => ("nanoseconds", 1),
        "microseconds" => ("nanoseconds", 1_000),
        "milliseconds" => ("nanoseconds", 1_000_000),
        "seconds" => ("nanoseconds", 1_000_000_000),
        "count" | "" => (name, 1),
        unit => (unit, 1),
    };

    builder.setting("source", &format!("pprof's {name} ({unit})"));
    builder.setting("weight", weight);
    Ok(Weighing { chosen, scale })
}

/// Each mapping's module, by its id; the first names the program.
fn mappings(raw: &Raw<'_>, builder: &mut CaptureBuilder) -> Result<BTreeMap<u64, u32>, String> {
    let mut modules = BTreeMap::new();

    for mapping in &raw.mappings {
        let (mut id, mut start, mut limit, mut file, mut build_id) = (0, 0, 0, 0, 0);

        for field in Fields::new(mapping) {
            match field? {
                (1, value) => id = value.number("a mapping's id")?,
                (2, value) => start = value.number("a mapping's start")?,
                (3, value) => limit = value.number("a mapping's limit")?,
                (5, value) => file = value.number("a mapping's file")?,
                (6, value) => build_id = value.number("a mapping's build id")?,
                _ => {}
            }
        }

        let path = raw.string(file)?;
        if modules.is_empty() && !path.is_empty() {
            builder.setting("program", path);
        }

        let build_id = Some(raw.string(build_id)?).filter(|id| !id.is_empty());
        let module = builder.module(path, build_id, [0; 16], limit.saturating_sub(start));
        modules.insert(id, module);
    }

    Ok(modules)
}

fn functions(raw: &Raw<'_>) -> Result<BTreeMap<u64, Function>, String> {
    let mut functions = BTreeMap::new();

    for function in &raw.functions {
        let (mut id, mut read) = (0, Function::default());

        for field in Fields::new(function) {
            match field? {
                (1, value) => id = value.number("a function's id")?,
                (2, value) => read.name = value.number("a function's name")?,
                (3, value) => read.system_name = value.number("a function's system name")?,
                (4, value) => read.file = value.number("a function's file")?,
                (5, value) => read.line = value.number("a function's line")?,
                _ => {}
            }
        }

        functions.insert(id, read);
    }

    Ok(functions)
}

/// Each location's frame, by its id. A frame is its address in the
/// process, as pprof keeps no image addresses, with its lines innermost
/// first, as pprof lists them.
fn locations(
    raw: &Raw<'_>,
    builder: &mut CaptureBuilder,
    modules: &BTreeMap<u64, u32>,
    functions: &BTreeMap<u64, Function>,
) -> Result<BTreeMap<u64, u32>, String> {
    let mut frames = BTreeMap::new();
    let mut used = std::collections::BTreeSet::new();

    for location in &raw.locations {
        let (mut id, mut mapping, mut address) = (0, 0, 0);
        let mut lines: Vec<(u64, u64, u64)> = Vec::new();

        for field in Fields::new(location) {
            match field? {
                (1, value) => id = value.number("a location's id")?,
                (2, value) => mapping = value.number("a location's mapping")?,
                (3, value) => address = value.number("a location's address")?,
                (4, value) => {
                    let mut line = (0, 0, 0);
                    for field in Fields::new(value.bytes("a line")?) {
                        match field? {
                            (1, value) => line.0 = value.number("a line's function")?,
                            (2, value) => line.1 = value.number("a line's number")?,
                            (3, value) => line.2 = value.number("a line's column")?,
                            _ => {}
                        }
                    }

                    lines.push(line);
                }
                _ => {}
            }
        }

        let module = match mapping {
            0 => NONE,
            mapping => *modules
                .get(&mapping)
                .ok_or_else(|| format!("location {id} names mapping {mapping}, which it lacks"))?,
        };

        // Two locations at one address, or none, are still two frames.
        if address == 0 || !used.insert(address) {
            address = UNADDRESSED | id;
        }

        let frame = builder.frame(FrameKind::Native, NONE, address, 0);
        if !lines.is_empty() {
            let mut names = Vec::with_capacity(lines.len());

            for &(function, line, column) in &lines {
                let function = functions.get(&function).ok_or_else(|| {
                    format!("location {id} names function {function}, which it lacks")
                })?;
                let name = match (
                    raw.string(function.name)?,
                    raw.string(function.system_name)?,
                ) {
                    ("", "") => format!("[function at {line}]"),
                    ("", name) | (name, _) => name.to_owned(),
                };

                let file = raw.string(function.file)?;
                names.push((name, file, line, column, function.line));
            }

            let lines: Vec<LineInput<'_>> = names
                .iter()
                .map(|(name, file, line, column, declared)| LineInput {
                    function: name,
                    file: (!file.is_empty()).then_some(*file),
                    line: u32::try_from(*line).unwrap_or(u32::MAX),
                    column: u32::try_from(*column).unwrap_or(u32::MAX),
                    declared: (!file.is_empty())
                        .then(|| (*file, u32::try_from(*declared).unwrap_or(u32::MAX))),
                    role: 0,
                })
                .collect();
            builder.symbolize(frame, module, &lines);
        }

        if frames.insert(id, frame).is_some() {
            return Err(format!("two locations are numbered {id}"));
        }
    }

    Ok(frames)
}

/// A sample's label as microscope's tasks keep them, `key=value`.
fn label(raw: &Raw<'_>, bytes: &[u8]) -> Result<String, String> {
    let (mut key, mut text, mut number, mut unit) = (0, 0, None, 0);
    for field in Fields::new(bytes) {
        match field? {
            (1, value) => key = value.number("a label's key")?,
            (2, value) => text = value.number("a label's value")?,
            (3, value) => number = Some(value.number("a label's number")?.cast_signed()),
            (4, value) => unit = value.number("a label's unit")?,
            _ => {}
        }
    }

    let key = raw.string(key)?;
    Ok(match number {
        Some(number) => match raw.string(unit)? {
            "" => format!("{key}={number}"),
            unit => format!("{key}={number} {unit}"),
        },
        None => format!("{key}={}", raw.string(text)?),
    })
}

/// One of an exported profile's values: its type and unit, and the call
/// tree that weighs it.
pub struct Values<'a> {
    pub kind: &'a str,
    pub unit: &'a str,
    pub capture: &'a Capture,
    pub keys: &'a Keys<'a>,
    pub tree: &'a CallTree,
}

/// Call trees as a gzipped pprof profile: a sample for each path with its
/// own weight in each tree, a location for each function, file, and line.
pub fn export(values: &[Values<'_>], duration: i64) -> std::io::Result<Vec<u8>> {
    let mut strings = Strings::default();
    let mut profile = Message::default();

    for value in values {
        let (kind, unit) = (strings.of(value.kind), strings.of(value.unit));
        profile.message(1, |message| {
            message.number(1, kind);
            message.number(2, unit);
        });
    }

    let mut locations: BTreeMap<(String, String, u32), u64> = BTreeMap::new();
    let mut functions: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut samples: BTreeMap<Vec<u64>, Vec<u64>> = BTreeMap::new();
    let mut written = Message::default();
    let mut path = Vec::new();

    for (index, value) in values.iter().enumerate() {
        let tree = value.tree;
        let mut of_key: BTreeMap<u32, u64> = BTreeMap::new();
        for node in 0..tree.len() {
            if tree.own[node] == 0 {
                continue;
            }

            path.clear();
            let mut at = u32::try_from(node).unwrap_or(NONE);

            while at != NONE {
                let key = tree.key[at as usize];
                let next = locations.len() as u64 + 1;
                let location = *of_key.entry(key).or_insert_with(|| {
                    *locations
                        .entry(located(value.capture, value.keys, key))
                        .or_insert_with_key(|(name, file, line)| {
                            let next_function = functions.len() as u64 + 1;
                            let function = *functions
                                .entry((name.clone(), file.clone()))
                                .or_insert_with(|| {
                                    let (name, file) = (strings.of(name), strings.of(file));
                                    written.message(5, |message| {
                                        message.number(1, next_function);
                                        message.number(2, name);
                                        message.number(3, name);
                                        message.number(4, file);
                                    });
                                    next_function
                                });

                            written.message(4, |message| {
                                message.number(1, next);
                                message.message(4, |message| {
                                    message.number(1, function);
                                    message.number(2, u64::from(*line));
                                });
                            });
                            next
                        })
                });

                path.push(location);
                at = tree.parent[at as usize];
            }

            samples
                .entry(path.clone())
                .or_insert_with(|| vec![0; values.len()])[index] += tree.own[node];
        }
    }

    for (path, values) in &samples {
        profile.message(2, |message| {
            message.packed(1, path.iter().copied());
            message.packed(2, values.iter().copied());
        });
    }

    profile.bytes.extend_from_slice(&written.bytes);
    for string in &strings.list {
        profile.bytes(6, string.as_bytes());
    }

    profile.signed(10, duration);

    let mut out = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut out, &profile.bytes)?;
    out.finish()
}

/// A key's function name, file, and line, as pprof names a location.
fn located(capture: &Capture, keys: &Keys<'_>, key: u32) -> (String, String, u32) {
    if let KeyKind::Line {
        function,
        file,
        line,
    } = keys.kind(key)
    {
        let name = capture.string(capture.functions[function as usize].name.get());
        let file = if file == NONE {
            ""
        } else {
            capture.string(file)
        };

        return (name.to_owned(), file.to_owned(), line);
    }

    let (file, line) = keys.location(key).unwrap_or(("", 0));
    (keys.name(key), file.to_owned(), line)
}

/// A string table being written, the empty string first.
struct Strings {
    list: Vec<String>,
    index: BTreeMap<String, u64>,
}

impl Default for Strings {
    fn default() -> Self {
        Self {
            list: vec![String::new()],
            index: BTreeMap::from([(String::new(), 0)]),
        }
    }
}

impl Strings {
    fn of(&mut self, text: &str) -> u64 {
        if let Some(&index) = self.index.get(text) {
            return index;
        }

        let index = self.list.len() as u64;
        self.list.push(text.to_owned());
        self.index.insert(text.to_owned(), index);
        index
    }
}
