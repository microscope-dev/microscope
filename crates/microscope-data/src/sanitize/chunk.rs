use std::collections::BTreeMap;

use crate::capture::Capture;
use crate::capture::records::{FrameKind, NONE, captures as parse_captures};
use crate::image::{StrId, Strings, StringsBuilder};
use crate::object::{
    ChunkData, DEFAULT_ZSTD_LEVEL, DimensionRow, ObjectError, ObjectKind, ObjectLevel,
    ObjectWriter, PartKind, PartSpec, SanitizerCountRow, Signal, TimeUnit, encode_tables,
    merge_chunks, round_capture_times,
};
use crate::syscalls::{Abi, CaptureState, ReadAt, SyscallId, specs};

use super::{FieldClass, Outcome, Sanitizer, Tally};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizedChunk {
    data: ChunkData,
    time_unit: TimeUnit,
}

impl SanitizedChunk {
    #[must_use]
    pub const fn data(&self) -> &ChunkData {
        &self.data
    }

    #[must_use]
    pub const fn time_unit(&self) -> TimeUnit {
        self.time_unit
    }

    #[must_use]
    pub fn into_data(self) -> ChunkData {
        self.data
    }

    #[must_use]
    pub fn re_sanitize(self, sanitizer: &Sanitizer) -> Self {
        sanitize_chunk(self.data, self.time_unit, sanitizer)
    }

    pub fn encode_single(&self, writer: u64, created_at: i64) -> Result<Vec<u8>, ObjectError> {
        encode_post(
            std::slice::from_ref(self),
            writer,
            created_at,
            DEFAULT_ZSTD_LEVEL,
        )
        .map(EncodedPost::into_bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedPost {
    bytes: Vec<u8>,
    first_chunk: u64,
    last_chunk: u64,
}

impl EncodedPost {
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    #[must_use]
    pub const fn first_chunk(&self) -> u64 {
        self.first_chunk
    }

    #[must_use]
    pub const fn last_chunk(&self) -> u64 {
        self.last_chunk
    }
}

pub fn encode_post(
    chunks: &[SanitizedChunk],
    writer: u64,
    created_at: i64,
    zstd_level: i32,
) -> Result<EncodedPost, ObjectError> {
    if chunks.is_empty() {
        return Err(ObjectError::Layout("cannot encode an empty post".into()));
    }

    let raw_chunks: Vec<ChunkData> = chunks.iter().map(|c| c.data.clone()).collect();
    let merged = if raw_chunks.len() == 1 {
        raw_chunks
            .into_iter()
            .next()
            .ok_or_else(|| ObjectError::Layout("missing chunk".into()))?
    } else {
        merge_chunks(&raw_chunks)?
    };

    let time_unit = chunks
        .iter()
        .map(|c| c.time_unit)
        .min()
        .unwrap_or(TimeUnit::Ms);
    let (starts_at, ends_at) = merged.time_bounds();
    let first_chunk = merged.chunks.iter().map(|c| c.number).min().unwrap_or(0);
    let last_chunk = merged.chunks.iter().map(|c| c.number).max().unwrap_or(0);

    let sections = encode_tables(&merged, time_unit, zstd_level, 0)?;
    let mut object = ObjectWriter::new(ObjectKind::Post, ObjectLevel::None, created_at, writer);
    object.add_part(&PartSpec {
        org_id: 0,
        project_id: 0,
        starts_at,
        ends_at,
        signal: Signal::Profile,
        part: PartKind::Chunks,
        sections,
    })?;
    let (bytes, _) = object.finish()?;

    Ok(EncodedPost {
        bytes,
        first_chunk,
        last_chunk,
    })
}

const fn abi_of(code: u8) -> Option<Abi> {
    match code {
        0 => Some(Abi::X86_64),
        1 => Some(Abi::X86_32),
        2 => Some(Abi::X32),
        _ => None,
    }
}

fn capture_field_class(abi: u8, number: u32, arg: u8) -> FieldClass {
    let spec_arg = abi_of(abi)
        .and_then(|abi| specs::spec(SyscallId::new(abi, number)))
        .and_then(|spec| spec.args.get(usize::from(arg)).copied());
    match spec_arg {
        Some(specs::Arg::Path) => FieldClass::SyscallPath,
        Some(specs::Arg::Str | specs::Arg::StrArray) => FieldClass::SyscallString,
        _ => FieldClass::SyscallPayload,
    }
}

fn setting_field_class(key: &str) -> Option<FieldClass> {
    if matches!(key, "arguments" | "command" | "argv")
        || key.starts_with("arguments ")
        || key.starts_with("command ")
    {
        return Some(FieldClass::ProcessArgs);
    }
    match key {
        "program" | "executable" => Some(FieldClass::ModulePath),
        _ => None,
    }
}

fn dimension_field_class(name: &str) -> Option<FieldClass> {
    match name {
        "service.name" | "microscope.project" | "process.executable.build_id" => None,
        "process.command_args" | "process.command_line" => Some(FieldClass::ProcessArgs),
        "process.executable.path" => Some(FieldClass::ModulePath),
        "process.executable.name" => Some(FieldClass::ProcessName),
        _ => Some(FieldClass::SyscallString),
    }
}

fn sanitize_string_id(
    old: &Capture,
    strings: &mut StringsBuilder,
    id: u32,
    class: Option<FieldClass>,
    sanitizer: &Sanitizer,
    tally: &mut Tally,
) -> (u32, bool) {
    if id == NONE {
        return (NONE, false);
    }

    let text = old.string(id);
    let Some(class) = class else {
        let new_id = strings.push(text).map_or(NONE, |StrId(i)| i);
        return (new_id, false);
    };

    match sanitizer.text(class, text, tally) {
        Outcome::Kept => (strings.push(text).map_or(NONE, |StrId(i)| i), false),
        Outcome::Replaced(changed) => (
            strings
                .push(changed.split('\0').next().unwrap_or_default())
                .map_or(NONE, |StrId(i)| i),
            false,
        ),
        Outcome::DropField => (NONE, false),
        Outcome::DropRecord => (NONE, true),
    }
}

fn arg_setting_suffix(key: &str, prefix: &str) -> Option<String> {
    if key == prefix {
        Some(String::new())
    } else {
        key.strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix(' '))
            .map(str::to_owned)
    }
}

fn sanitize_arguments_setting(
    old: &Capture,
    strings: &mut StringsBuilder,
    id: u32,
    sanitizer: &Sanitizer,
    tally: &mut Tally,
) -> (u32, Option<Vec<String>>) {
    if id == NONE {
        return (NONE, None);
    }

    let text = old.string(id);
    if let Ok(args) = serde_json::from_str::<Vec<String>>(text) {
        return match sanitizer.args(&args, tally) {
            Outcome::Kept => (strings.push(text).map_or(NONE, |StrId(i)| i), Some(args)),
            Outcome::Replaced(changed) => {
                let encoded = serde_json::to_string(&changed).unwrap_or_else(|_| "[]".to_owned());
                (
                    strings
                        .push(encoded.split('\0').next().unwrap_or_default())
                        .map_or(NONE, |StrId(i)| i),
                    Some(changed),
                )
            }
            Outcome::DropField | Outcome::DropRecord => (NONE, Some(Vec::new())),
        };
    }

    let out = match sanitizer.text(FieldClass::ProcessArgs, text, tally) {
        Outcome::Kept => strings.push(text).map_or(NONE, |StrId(i)| i),
        Outcome::Replaced(changed) => strings
            .push(changed.split('\0').next().unwrap_or_default())
            .map_or(NONE, |StrId(i)| i),
        Outcome::DropField | Outcome::DropRecord => NONE,
    };
    (out, None)
}

#[expect(
    clippy::too_many_lines,
    reason = "classifies and sanitizes every string and payload field across all capture tables"
)]
#[must_use]
pub fn sanitize_chunk(
    mut raw: ChunkData,
    time_unit: TimeUnit,
    sanitizer: &Sanitizer,
) -> SanitizedChunk {
    round_capture_times(&mut raw.capture, time_unit);

    let old = raw.capture;
    let mut strings = StringsBuilder::default();
    let mut tally = Tally::default();

    let copy = |strings: &mut StringsBuilder, id: u32| -> u32 {
        if id == NONE {
            NONE
        } else {
            strings.push(old.string(id)).map_or(NONE, |StrId(i)| i)
        }
    };

    let mut sanitized_args_by_scope: BTreeMap<String, (u32, Vec<String>)> = BTreeMap::new();
    for s in &old.settings {
        let key_str = old.string(s.key.get());
        if let Some(scope) =
            arg_setting_suffix(key_str, "arguments").or_else(|| arg_setting_suffix(key_str, "argv"))
        {
            let (new_val, parsed) = sanitize_arguments_setting(
                &old,
                &mut strings,
                s.value.get(),
                sanitizer,
                &mut tally,
            );
            if let Some(args) = parsed {
                sanitized_args_by_scope.insert(scope, (new_val, args));
            }
        }
    }

    let settings = old
        .settings
        .iter()
        .map(|s| {
            let key_str = old.string(s.key.get());
            let key = copy(&mut strings, s.key.get());
            let value = if let Some(scope) = arg_setting_suffix(key_str, "arguments")
                .or_else(|| arg_setting_suffix(key_str, "argv"))
                && let Some(&(cached, _)) = sanitized_args_by_scope.get(&scope)
            {
                cached
            } else if let Some(scope) = arg_setting_suffix(key_str, "command")
                && let Some((_, clean_args)) = sanitized_args_by_scope.get(&scope)
            {
                if clean_args.is_empty() {
                    NONE
                } else {
                    let joined = clean_args.join(" ");
                    let final_text =
                        match sanitizer.text(FieldClass::ProcessArgs, &joined, &mut tally) {
                            Outcome::Kept => Some(joined),
                            Outcome::Replaced(changed) => Some(changed),
                            Outcome::DropField | Outcome::DropRecord => None,
                        };
                    final_text.map_or(NONE, |t| {
                        strings
                            .push(t.split('\0').next().unwrap_or_default())
                            .map_or(NONE, |StrId(i)| i)
                    })
                }
            } else {
                match setting_field_class(key_str) {
                    Some(FieldClass::ProcessArgs) => {
                        sanitize_arguments_setting(
                            &old,
                            &mut strings,
                            s.value.get(),
                            sanitizer,
                            &mut tally,
                        )
                        .0
                    }
                    class => {
                        sanitize_string_id(
                            &old,
                            &mut strings,
                            s.value.get(),
                            class,
                            sanitizer,
                            &mut tally,
                        )
                        .0
                    }
                }
            };
            crate::capture::records::Setting {
                key: key.into(),
                value: value.into(),
            }
        })
        .collect();

    let modules = old
        .modules
        .iter()
        .map(|m| {
            let path = sanitize_string_id(
                &old,
                &mut strings,
                m.path.get(),
                Some(FieldClass::ModulePath),
                sanitizer,
                &mut tally,
            )
            .0;
            let build_id = copy(&mut strings, m.build_id.get());
            crate::capture::records::Module {
                path: path.into(),
                build_id: build_id.into(),
                ..*m
            }
        })
        .collect();

    let processes = old
        .processes
        .iter()
        .map(|p| {
            let name = sanitize_string_id(
                &old,
                &mut strings,
                p.name.get(),
                Some(FieldClass::ProcessName),
                sanitizer,
                &mut tally,
            )
            .0;
            crate::capture::records::Process {
                name: name.into(),
                ..*p
            }
        })
        .collect();

    let threads = old
        .threads
        .iter()
        .map(|t| {
            let name = sanitize_string_id(
                &old,
                &mut strings,
                t.name.get(),
                Some(FieldClass::ThreadName),
                sanitizer,
                &mut tally,
            )
            .0;
            crate::capture::records::Thread {
                name: name.into(),
                ..*t
            }
        })
        .collect();

    let frames = old
        .frames
        .iter()
        .map(|f| {
            let address = match FrameKind::from_number(f.kind) {
                Some(kind) if kind.named() => {
                    let old_id = u32::try_from(f.address.get()).unwrap_or(NONE);
                    u64::from(copy(&mut strings, old_id))
                }
                _ => f.address.get(),
            };
            crate::capture::records::Frame {
                address: address.into(),
                ..*f
            }
        })
        .collect();

    let lines = old
        .lines
        .iter()
        .map(|l| {
            let file = sanitize_string_id(
                &old,
                &mut strings,
                l.file.get(),
                Some(FieldClass::SourcePath),
                sanitizer,
                &mut tally,
            )
            .0;
            crate::capture::records::Line {
                file: file.into(),
                ..*l
            }
        })
        .collect();

    let functions = old
        .functions
        .iter()
        .map(|f| {
            let name = copy(&mut strings, f.name.get());
            let file = sanitize_string_id(
                &old,
                &mut strings,
                f.file.get(),
                Some(FieldClass::SourcePath),
                sanitizer,
                &mut tally,
            )
            .0;
            crate::capture::records::Function {
                name: name.into(),
                file: file.into(),
                ..*f
            }
        })
        .collect();

    let sites = old
        .sites
        .iter()
        .map(|s| {
            let name = sanitize_string_id(
                &old,
                &mut strings,
                s.name.get(),
                Some(FieldClass::ZoneText),
                sanitizer,
                &mut tally,
            )
            .0;
            let file = sanitize_string_id(
                &old,
                &mut strings,
                s.file.get(),
                Some(FieldClass::SourcePath),
                sanitizer,
                &mut tally,
            )
            .0;
            crate::capture::records::Site {
                name: name.into(),
                file: file.into(),
                ..*s
            }
        })
        .collect();

    let mut zones = Vec::with_capacity(old.zones.len());
    let mut zone_ranges = Vec::with_capacity(old.zone_ranges.len());
    for range in &old.zone_ranges {
        let start = u32::try_from(zones.len()).unwrap_or(u32::MAX);
        for zone in old.range_zones(range) {
            let (value, drop_record) = sanitize_string_id(
                &old,
                &mut strings,
                zone.value.get(),
                Some(FieldClass::ZoneText),
                sanitizer,
                &mut tally,
            );
            if !drop_record {
                zones.push(crate::capture::records::Zone {
                    value: value.into(),
                    ..*zone
                });
            }
        }
        let count = u32::try_from(zones.len())
            .unwrap_or(u32::MAX)
            .saturating_sub(start);
        if count > 0 {
            zone_ranges.push(crate::capture::records::ZoneRange {
                thread: range.thread,
                depth: range.depth,
                start: start.into(),
                count: count.into(),
            });
        }
    }

    let markers = old
        .markers
        .iter()
        .filter_map(|m| {
            let (text, drop_record) = sanitize_string_id(
                &old,
                &mut strings,
                m.text.get(),
                Some(FieldClass::MarkerText),
                sanitizer,
                &mut tally,
            );
            (!drop_record).then_some(crate::capture::records::Marker {
                text: text.into(),
                ..*m
            })
        })
        .collect();

    let tasks = old
        .tasks
        .iter()
        .map(|t| {
            let runtime = copy(&mut strings, t.runtime.get());
            let labels = sanitize_string_id(
                &old,
                &mut strings,
                t.labels.get(),
                Some(FieldClass::ZoneText),
                sanitizer,
                &mut tally,
            )
            .0;
            crate::capture::records::Task {
                runtime: runtime.into(),
                labels: labels.into(),
                ..*t
            }
        })
        .collect();

    let task_waits = old
        .task_waits
        .iter()
        .map(|w| {
            let state = copy(&mut strings, w.state.get());
            crate::capture::records::TaskWait {
                state: state.into(),
                ..*w
            }
        })
        .collect();

    let mut syscalls = Vec::with_capacity(old.syscalls.len());
    let mut syscall_blobs = Vec::with_capacity(old.syscall_blobs.len());
    let mut kept_before = Vec::with_capacity(old.syscalls.len() + 1);
    kept_before.push(0_u32);

    for call in &old.syscalls {
        let bytes = old.blob(call.captures.get(), call.captures_len.get());
        if bytes.is_empty() {
            syscalls.push(crate::capture::records::Syscall {
                captures: NONE.into(),
                captures_len: 0.into(),
                ..*call
            });
            kept_before.push(u32::try_from(syscalls.len()).unwrap_or(u32::MAX));
            continue;
        }

        let Some(parsed) = parse_captures(bytes) else {
            syscalls.push(crate::capture::records::Syscall {
                captures: NONE.into(),
                captures_len: 0.into(),
                ..*call
            });
            kept_before.push(u32::try_from(syscalls.len()).unwrap_or(u32::MAX));
            continue;
        };

        let mut drop_call = false;
        let mut rebuilt = Vec::with_capacity(bytes.len());
        for cap in parsed {
            let class = capture_field_class(call.abi, call.number.get(), cap.arg);
            let sanitized_bytes = if cap.bytes.is_empty() {
                Vec::new()
            } else {
                match sanitizer.bytes(class, &cap.bytes, &mut tally) {
                    Outcome::Kept => cap.bytes.to_vec(),
                    Outcome::Replaced(changed) => changed,
                    Outcome::DropField => Vec::new(),
                    Outcome::DropRecord => {
                        drop_call = true;
                        break;
                    }
                }
            };

            let when = match cap.when {
                ReadAt::Entry => 0_u8,
                ReadAt::Exit => 1_u8,
            };
            let (state, wanted) = match cap.state {
                CaptureState::Complete => (0_u8, 0_u32),
                CaptureState::Truncated { wanted } => (1_u8, wanted),
                CaptureState::Unreadable => (2_u8, 0_u32),
                CaptureState::NotRequested => (3_u8, 0_u32),
            };
            let len = u32::try_from(sanitized_bytes.len()).unwrap_or(u32::MAX);
            rebuilt.extend_from_slice(&[cap.arg, when, state, 0]);
            rebuilt.extend_from_slice(&wanted.to_le_bytes());
            rebuilt.extend_from_slice(&len.to_le_bytes());
            rebuilt.extend_from_slice(&sanitized_bytes);
        }

        if !drop_call {
            let (captures, captures_len) = if rebuilt.is_empty() {
                (NONE, 0)
            } else {
                let start = u32::try_from(syscall_blobs.len()).unwrap_or(u32::MAX);
                let len = u32::try_from(rebuilt.len()).unwrap_or(u32::MAX);
                syscall_blobs.extend_from_slice(&rebuilt);
                (start, len)
            };
            syscalls.push(crate::capture::records::Syscall {
                captures: captures.into(),
                captures_len: captures_len.into(),
                ..*call
            });
        }
        kept_before.push(u32::try_from(syscalls.len()).unwrap_or(u32::MAX));
    }

    let mut syscall_notes = Vec::with_capacity(old.syscall_notes.len());
    for note in &old.syscall_notes {
        let (text, drop_text) = sanitize_string_id(
            &old,
            &mut strings,
            note.text.get(),
            Some(FieldClass::SyscallPath),
            sanitizer,
            &mut tally,
        );
        if drop_text {
            continue;
        }

        let old_blob = old.blob(note.blob.get(), note.blob_len.get());
        let (blob, blob_len) = if old_blob.is_empty() {
            (NONE, 0)
        } else {
            match sanitizer.bytes(FieldClass::SyscallPayload, old_blob, &mut tally) {
                Outcome::Kept => {
                    let start = u32::try_from(syscall_blobs.len()).unwrap_or(u32::MAX);
                    let len = u32::try_from(old_blob.len()).unwrap_or(u32::MAX);
                    syscall_blobs.extend_from_slice(old_blob);
                    (start, len)
                }
                Outcome::Replaced(changed) => {
                    let start = u32::try_from(syscall_blobs.len()).unwrap_or(u32::MAX);
                    let len = u32::try_from(changed.len()).unwrap_or(u32::MAX);
                    syscall_blobs.extend_from_slice(&changed);
                    (start, len)
                }
                Outcome::DropField => (NONE, 0),
                Outcome::DropRecord => continue,
            }
        };

        let mapped_calls = kept_before
            .get(note.calls.get() as usize)
            .copied()
            .unwrap_or_else(|| u32::try_from(syscalls.len()).unwrap_or(u32::MAX));
        syscall_notes.push(crate::capture::records::SyscallNote {
            text: text.into(),
            blob: blob.into(),
            blob_len: blob_len.into(),
            calls: mapped_calls.into(),
            ..*note
        });
    }

    let dimensions: Vec<DimensionRow> = raw
        .dimensions
        .into_iter()
        .filter_map(|dim| {
            let Some(class) = dimension_field_class(&dim.name) else {
                return Some(dim);
            };
            match sanitizer.text(class, &dim.value, &mut tally) {
                Outcome::Kept => Some(dim),
                Outcome::Replaced(value) => Some(DimensionRow { value, ..dim }),
                Outcome::DropField => Some(DimensionRow {
                    value: String::new(),
                    ..dim
                }),
                Outcome::DropRecord => None,
            }
        })
        .collect();

    let mut counts_map: BTreeMap<(String, u16), u64> = BTreeMap::new();
    for row in raw.sanitizer_counts {
        *counts_map.entry((row.rule, row.field)).or_default() += row.fired;
    }
    for (rule, class, fired) in sanitizer.fired_by_field(&tally) {
        if fired > 0 {
            *counts_map
                .entry((rule.to_owned(), class.number()))
                .or_default() += fired;
        }
    }
    let sanitizer_counts = counts_map
        .into_iter()
        .map(|((rule, field), fired)| SanitizerCountRow { rule, field, fired })
        .collect();

    let rule_names: String = sanitizer.rules().collect::<Vec<_>>().join(",");
    let chunks = raw
        .chunks
        .into_iter()
        .map(|mut chunk| {
            chunk.time_unit = time_unit;
            if chunk.rules.is_empty() {
                chunk.rules.clone_from(&rule_names);
            }
            chunk
        })
        .collect();

    let capture = Capture {
        strings: strings.into_bytes(),
        settings,
        modules,
        processes,
        threads,
        mappings: old.mappings,
        frames,
        stacks: old.stacks,
        samples: old.samples,
        sample_ranges: old.sample_ranges,
        frame_symbols: old.frame_symbols,
        lines,
        functions,
        accounting: old.accounting,
        off_cpu: old.off_cpu,
        off_cpu_ranges: old.off_cpu_ranges,
        sites,
        zones,
        zone_ranges,
        plots: old.plots,
        markers,
        tasks,
        task_waits,
        gaps: old.gaps,
        syscalls,
        syscall_blobs,
        syscall_notes,
        syscall_stats: old.syscall_stats,
        syscall_errnos: old.syscall_errnos,
    };

    if cfg!(debug_assertions)
        && let Err(error) = capture.validate()
    {
        debug_assert!(false, "sanitized capture is invalid: {error}");
    }

    let _ = Strings(&capture.strings);
    SanitizedChunk {
        data: ChunkData {
            capture,
            chunks,
            dimensions,
            sanitizer_counts,
        },
        time_unit,
    }
}
