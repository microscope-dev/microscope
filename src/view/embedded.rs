//! The view files and kernels a module carries in its
//! `.debug_microscope_views` section.
//!
//! The section holds records, each a kind byte, a format byte, a 32-bit
//! little-endian length, and that many bytes. Zero bytes between records
//! are padding, as compilers and linkers leave between the contributions
//! of separate objects. A record of view text is kind 1, format 1. A
//! kernel is kind 2, format 1: a 16-bit length and the kernel's name, a
//! 32-bit length and its source or a link to it, and its module. A
//! renderer is kind 3, format 1: a 16-bit length and the renderer's name,
//! then its JavaScript.

use std::sync::Arc;

use super::ViewSet;
use super::syntax::{Error, MAX_FILE_BYTES};

/// The section's name.
pub const SECTION: &str = ".debug_microscope_views";

/// The kind of a record of view text, and the format this microscope reads.
pub const VIEW_TEXT: u8 = 1;

pub const VIEW_TEXT_FORMAT: u8 = 1;

/// The kind of a kernel's record, and the format this microscope reads.
pub const KERNEL: u8 = 2;

pub const KERNEL_FORMAT: u8 = 1;

/// The kind of a renderer's record, and the format this microscope reads.
pub const RENDERER: u8 = 3;

pub const RENDERER_FORMAT: u8 = 1;

/// How many records one module may carry.
const MAX_RECORDS: usize = 256;

/// A kernel a record carries: its name, its source or a link to it, and
/// its module.
struct KernelRecord<'a> {
    origin: String,
    name: &'a str,
    source: &'a str,
    module: &'a [u8],
}

/// A renderer a record carries: its name and its JavaScript.
struct RendererRecord<'a> {
    origin: String,
    name: &'a str,
    source: &'a str,
}

/// The views of a module named `module` whose section holds `bytes`, and
/// the kernels and renderers they may call, with why any record was left
/// out. Each record is named `MODULE.views[N]`, counting records from 0.
#[must_use]
pub fn view_set(module: &str, bytes: &[u8]) -> ViewSet {
    let Records {
        files,
        kernels,
        renderers,
        errors,
    } = records(module, bytes);
    let mut set = ViewSet::new(
        files
            .iter()
            .map(|(name, text)| (name.as_str(), text.as_str())),
    );

    for kernel in &kernels {
        set.add_kernels(
            &kernel.origin,
            [(kernel.name, kernel.source, kernel.module)],
        );
    }

    for renderer in &renderers {
        set.add_renderers(&renderer.origin, [(renderer.name, renderer.source)]);
    }

    // The records' own errors come first.
    set.errors.splice(0..0, errors);
    set
}

/// A kernel record's name, source, and module.
fn kernel(payload: &[u8]) -> Result<(&str, &str, &[u8]), String> {
    let cut = || "the kernel's record is cut short".to_owned();
    let (length, rest) = payload.split_first_chunk::<2>().ok_or_else(cut)?;
    let (name, rest) = rest
        .split_at_checked(usize::from(u16::from_le_bytes(*length)))
        .ok_or_else(cut)?;
    let (length, rest) = rest.split_first_chunk::<4>().ok_or_else(cut)?;
    let length = usize::try_from(u32::from_le_bytes(*length)).map_err(|_| cut())?;
    let (source, module) = rest.split_at_checked(length).ok_or_else(cut)?;
    let name =
        std::str::from_utf8(name).map_err(|_| "the kernel's name is not UTF-8".to_owned())?;
    let source =
        std::str::from_utf8(source).map_err(|_| "the kernel's source is not UTF-8".to_owned())?;
    Ok((name, source, module))
}

/// A renderer record's name and JavaScript.
fn renderer(payload: &[u8]) -> Result<(&str, &str), String> {
    let cut = || "the renderer's record is cut short".to_owned();
    let (length, rest) = payload.split_first_chunk::<2>().ok_or_else(cut)?;
    let (name, source) = rest
        .split_at_checked(usize::from(u16::from_le_bytes(*length)))
        .ok_or_else(cut)?;
    let name =
        std::str::from_utf8(name).map_err(|_| "the renderer's name is not UTF-8".to_owned())?;
    let source = std::str::from_utf8(source)
        .map_err(|_| "the renderer's JavaScript is not UTF-8".to_owned())?;
    Ok((name, source))
}

/// What a section's records hold, and why any was left out.
struct Records<'a> {
    files: Vec<(String, String)>,
    kernels: Vec<KernelRecord<'a>>,
    renderers: Vec<RendererRecord<'a>>,
    errors: Vec<Error>,
}

/// The view files, kernels, and renderers in a section's records, and why
/// any was left out.
fn records<'a>(module: &str, bytes: &'a [u8]) -> Records<'a> {
    let mut files = Vec::new();
    let mut kernels = Vec::new();
    let mut renderers = Vec::new();
    let mut errors = Vec::new();
    let mut position = 0;
    let mut index = 0;
    let error = |index: usize, message: String| Error {
        source: Arc::from(format!("{module}.views[{index}]")),
        line: 0,
        column: 0,
        message,
    };

    while position < bytes.len() {
        if bytes[position] == 0 {
            position += 1;
            continue;
        }

        if index == MAX_RECORDS {
            errors.push(error(
                index,
                format!("a module may carry at most {MAX_RECORDS} records of views"),
            ));
            break;
        }

        let Some(header) = bytes.get(position..position + 6) else {
            errors.push(error(index, "the record's header is cut short".to_owned()));
            break;
        };

        let kind = header[0];
        let format = header[1];
        let length = u32::from_le_bytes([header[2], header[3], header[4], header[5]]);
        let start = position + 6;
        let Some(payload) = usize::try_from(length)
            .ok()
            .and_then(|length| bytes.get(start..start.checked_add(length)?))
        else {
            errors.push(error(
                index,
                format!("the record claims {length} bytes, past the section's end"),
            ));
            break;
        };

        position = start + payload.len();

        match (kind, format) {
            // A record is no larger than a view file may be, so a module
            // cannot make the debugger copy more.
            (VIEW_TEXT, VIEW_TEXT_FORMAT) if payload.len() > MAX_FILE_BYTES => errors.push(error(
                index,
                format!("the record's views are larger than a view file may be, {MAX_FILE_BYTES} bytes"),
            )),
            (VIEW_TEXT, VIEW_TEXT_FORMAT) => match std::str::from_utf8(payload) {
                Ok(text) => files.push((format!("{module}.views[{index}]"), text.to_owned())),
                Err(_) => errors.push(error(index, "the record's views are not UTF-8".to_owned())),
            },
            (VIEW_TEXT, format) => errors.push(error(
                index,
                format!("the record's views are in format {format}, and this microscope reads format {VIEW_TEXT_FORMAT}"),
            )),
            (KERNEL, KERNEL_FORMAT) => match kernel(payload) {
                // A kernel's source is no larger than a view file may be.
                Ok((_, source, _)) if source.len() > MAX_FILE_BYTES => errors.push(error(
                    index,
                    format!("the kernel's source is larger than a view file may be, {MAX_FILE_BYTES} bytes"),
                )),
                Ok((name, source, wasm)) => kernels.push(KernelRecord {
                    origin: format!("{module}.views[{index}]"),
                    name,
                    source,
                    module: wasm,
                }),
                Err(message) => errors.push(error(index, message)),
            },
            (KERNEL, format) => errors.push(error(
                index,
                format!("the kernel is in format {format}, and this microscope reads format {KERNEL_FORMAT}"),
            )),
            (RENDERER, RENDERER_FORMAT) => match renderer(payload) {
                Ok((name, source)) => renderers.push(RendererRecord {
                    origin: format!("{module}.views[{index}]"),
                    name,
                    source,
                }),
                Err(message) => errors.push(error(index, message)),
            },
            (RENDERER, format) => errors.push(error(
                index,
                format!("the renderer is in format {format}, and this microscope reads format {RENDERER_FORMAT}"),
            )),
            (kind, _) => errors.push(error(
                index,
                format!("the record is of kind {kind}, which this microscope does not read"),
            )),
        }

        index += 1;
    }

    Records {
        files,
        kernels,
        renderers,
        errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(kind: u8, format: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![kind, format];
        bytes.extend(u32::try_from(payload.len()).expect("small").to_le_bytes());
        bytes.extend(payload);
        bytes
    }

    fn kernel_payload(name: &str, source: &str, module: &[u8]) -> Vec<u8> {
        let mut bytes = u16::try_from(name.len())
            .expect("short")
            .to_le_bytes()
            .to_vec();
        bytes.extend(name.as_bytes());
        bytes.extend(u32::try_from(source.len()).expect("short").to_le_bytes());
        bytes.extend(source.as_bytes());
        bytes.extend(module);
        bytes
    }

    fn renderer_payload(name: &str, source: &str) -> Vec<u8> {
        let mut bytes = u16::try_from(name.len())
            .expect("short")
            .to_le_bytes()
            .to_vec();
        bytes.extend(name.as_bytes());
        bytes.extend(source.as_bytes());
        bytes
    }

    #[test]
    fn records_are_read_past_padding_and_bad_ones_are_named() {
        let view = b"microscope-views 1\nview c intvec {\n    show empty(\"v\")\n}\n";
        let mut bytes = vec![0, 0];
        bytes.extend(record(VIEW_TEXT, VIEW_TEXT_FORMAT, view));
        bytes.extend([0; 3]);
        bytes.extend(record(KERNEL, KERNEL_FORMAT, b"\0asm"));
        bytes.extend(record(VIEW_TEXT, 2, view));
        bytes.extend(record(9, 1, b""));
        bytes.extend(record(VIEW_TEXT, VIEW_TEXT_FORMAT, view));
        bytes.extend(record(
            VIEW_TEXT,
            VIEW_TEXT_FORMAT,
            &vec![b' '; MAX_FILE_BYTES + 1],
        ));
        let module = include_bytes!("../../views/kernels/rust-btree.wasm");
        bytes.extend(record(
            KERNEL,
            KERNEL_FORMAT,
            &kernel_payload("tree", "tree.zig", module),
        ));
        bytes.extend(record(
            KERNEL,
            KERNEL_FORMAT,
            &kernel_payload("junk", "junk.c", b"\0asm"),
        ));
        bytes.extend(record(KERNEL, 2, &kernel_payload("later", "", module)));
        bytes.extend(record(
            KERNEL,
            KERNEL_FORMAT,
            &kernel_payload("../tree", "", module),
        ));
        let board =
            "microscope.draw(() => microscope.picture({ width: 8, height: 8, shapes: [] }));";
        bytes.extend(record(
            RENDERER,
            RENDERER_FORMAT,
            &renderer_payload("board", board),
        ));
        bytes.extend(record(RENDERER, 2, &renderer_payload("later", board)));
        bytes.extend(record(RENDERER, RENDERER_FORMAT, &[9, 0, b'b']));
        bytes.extend(record(
            RENDERER,
            RENDERER_FORMAT,
            &renderer_payload("../board", board),
        ));
        bytes.extend([VIEW_TEXT, VIEW_TEXT_FORMAT, 200, 0, 0, 0, b'u']);
        let set = view_set("app", &bytes);
        assert_eq!(set.views().len(), 2);
        assert_eq!(&*set.views()[0].source, "app.views[0]");
        assert_eq!(&*set.views()[1].source, "app.views[4]");
        let tree = set.kernel("tree").expect("a kernel");
        assert_eq!(tree.source(), "tree.zig");
        assert!(set.kernel("junk").is_none());
        let renderer = set.renderer("board").expect("a renderer");
        assert_eq!(&*renderer.source, board);
        assert_eq!(&*renderer.origin, "app.views[10]");
        assert_eq!(set.renderers().count(), 1);
        let errors = set
            .errors()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            errors,
            [
                "app.views[1]:0:0: the kernel's record is cut short",
                "app.views[2]:0:0: the record's views are in format 2, and this microscope reads format 1",
                "app.views[3]:0:0: the record is of kind 9, which this microscope does not read",
                "app.views[5]:0:0: the record's views are larger than a view file may be, 262144 bytes",
                "app.views[8]:0:0: the kernel is in format 2, and this microscope reads format 1",
                "app.views[11]:0:0: the renderer is in format 2, and this microscope reads format 1",
                "app.views[12]:0:0: the renderer's record is cut short",
                "app.views[14]:0:0: the record claims 200 bytes, past the section's end",
                "app.views[7]:0:0: kernel `junk`: it is not a module a kernel may be: unexpected end-of-file (at offset 0x4)",
                "app.views[9]:0:0: kernel `../tree`: a kernel's name is 1 to 64 letters, digits, `_`, and `-`",
                "app.views[13]:0:0: renderer `../board`: a renderer's name is 1 to 64 letters, digits, `_`, and `-`",
            ]
        );
    }
}
