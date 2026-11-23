use std::path::{Component, Path};

use crate::capture::Capture;
use crate::capture::records::{FrameKind, NONE, Truncation};

const C_LIBRARY: [&str; 5] = [
    "libc.so",
    "libpthread.so",
    "ld-linux",
    "ld-musl",
    "libc.musl",
];

fn function_name(capture: &Capture, function: u32) -> &str {
    capture
        .functions
        .get(function as usize)
        .map_or("", |row| capture.string(row.name.get()))
}

fn address_name(capture: &Capture, frame: u32) -> String {
    let record = capture.frame(frame);
    let module = record.module.get();
    if module == NONE {
        return format!("{:#x}", record.address.get());
    }

    let path = capture.string(capture.module(module).path.get());
    let name = Path::new(path)
        .file_name()
        .map_or_else(|| path.into(), |name| name.to_string_lossy());
    format!("{name}+{:#x}", record.address.get())
}

pub(super) fn stack_names(capture: &Capture, stack: u32) -> (Vec<String>, Option<String>) {
    let mut functions = Vec::new();
    let mut truncated = None;
    for frame in capture.stack_frames(stack) {
        let record = capture.frame(frame);
        if record.kind == FrameKind::Truncated as u8 {
            let reason = Truncation::from_number(record.address.get())
                .map_or("unknown", Truncation::describe);
            truncated = Some(reason.to_owned());
            continue;
        }

        let lines = capture.frame_lines(frame);
        if lines.is_empty() {
            functions.push(address_name(capture, frame));
        }
        for line in lines {
            functions.push(function_name(capture, line.function.get()).to_owned());
        }
    }
    (functions, truncated)
}

fn rust_standard_library_function(name: &str) -> bool {
    let standard = |path: &str| {
        let path = path.trim_start_matches('&');
        let path = path.strip_prefix("mut ").unwrap_or(path);
        let path = path.strip_prefix("dyn ").unwrap_or(path);
        ["std::", "core::", "alloc::"]
            .iter()
            .any(|krate| path.starts_with(krate))
    };
    let Some(inner) = name.strip_prefix('<') else {
        return standard(name);
    };

    let mut depth = 0_usize;
    let mut trait_at = None;
    let mut end = inner.len();
    for (at, byte) in inner.bytes().enumerate() {
        match byte {
            b'<' => depth += 1,
            b'>' if depth == 0 => {
                end = at;
                break;
            }
            b'>' => depth -= 1,
            b' ' if depth == 0
                && trait_at.is_none()
                && inner.get(at..).is_some_and(|rest| rest.starts_with(" as ")) =>
            {
                trait_at = Some(at);
            }
            _ => {}
        }
    }

    trait_at.map_or_else(
        || standard(inner.get(..end).unwrap_or(inner)),
        |at| {
            standard(inner.get(..at).unwrap_or_default())
                && standard(inner.get(at + 4..end).unwrap_or_default())
        },
    )
}

fn rust_standard_library(path: &Path) -> bool {
    let parts = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    parts.iter().enumerate().any(|(at, part)| {
        let toolchain = match at.checked_sub(3).and_then(|start| parts.get(start..at)) {
            Some([_, "rustc", commit]) => commit.len() == 40,
            Some(["rustlib", "src", "rust"]) => true,
            _ => at == 2 && matches!(parts.as_slice(), ["rustc", commit, ..] if commit.len() == 40),
        };
        *part == "library"
            && toolchain
            && parts.get(at + 1).is_some_and(|krate| {
                matches!(
                    *krate,
                    "core" | "alloc" | "std" | "panic_unwind" | "panic_abort"
                )
            })
    })
}

pub(super) struct SyscallPlace<'a> {
    pub(super) function: &'a str,
    pub(super) file: &'a str,
    pub(super) line: u32,
}

pub(super) fn syscall_place(capture: &Capture, row: u32) -> Option<SyscallPlace<'_>> {
    let interpreted = |frame: &u32| {
        capture
            .frames
            .get(*frame as usize)
            .is_some_and(|frame| frame.kind == FrameKind::Interpreted as u8)
    };
    let place = |frame: u32| {
        let line = capture.frame_lines(frame).iter().find(|line| {
            line.file.get() != NONE
                && !rust_standard_library(Path::new(capture.string(line.file.get())))
        })?;
        Some(SyscallPlace {
            function: function_name(capture, line.function.get()),
            file: capture.string(line.file.get()),
            line: line.number.get(),
        })
    };

    capture
        .stack_frames(row)
        .filter(interpreted)
        .find_map(place)
        .or_else(|| capture.stack_frames(row).find_map(place))
}

fn caller_of_c_library(capture: &Capture, row: u32) -> Option<String> {
    let names: Vec<String> = capture
        .stack_frames(row)
        .filter(|&frame| {
            let record = capture.frame(frame);
            let module = record.module.get();
            record.kind != FrameKind::Truncated as u8
                && (module == NONE || {
                    let path = capture.string(capture.module(module).path.get());
                    let name = path.rsplit('/').next().unwrap_or(path);
                    !C_LIBRARY.iter().any(|prefix| name.starts_with(prefix))
                })
        })
        .flat_map(|frame| {
            let lines = capture.frame_lines(frame);
            if lines.is_empty() {
                return vec![address_name(capture, frame)];
            }
            lines
                .iter()
                .map(|line| function_name(capture, line.function.get()).to_owned())
                .collect()
        })
        .collect();

    let own = |name: &&String| !rust_standard_library_function(name);
    names.iter().find(own).or_else(|| names.first()).cloned()
}

pub(super) fn syscall_site_name(capture: &Capture, row: u32) -> String {
    let site = syscall_place(capture, row)
        .map(|place| {
            let file = place.file.rsplit('/').next().unwrap_or(place.file);
            format!("{} · {file}:{}", place.function, place.line)
        })
        .or_else(|| caller_of_c_library(capture, row));
    site.unwrap_or_else(|| {
        stack_names(capture, row)
            .0
            .into_iter()
            .next()
            .unwrap_or_else(|| "?".to_owned())
    })
}
