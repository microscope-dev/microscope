//! Which bytes to copy from behind a call's pointer arguments, and when.
//!
//! The rules follow each call's spec. An engine that reads the traced process's memory
//! itself, as the ptrace engine does, supplies a reader; the BPF engine's
//! programs follow the same rules in the kernel.

use std::sync::Arc;

use super::blobs;
use super::model::{Abi, Capture, CaptureState, Outcome, ReadAt, SyscallId};
use super::specs::{self, Arg, Count, Direction, MemKind, Struct};

/// How much to copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Bytes of a string or buffer (`-s`), past which it is truncated.
    pub string: usize,

    /// Bytes of a buffer whose every byte is wanted, as `-e read=` wants.
    pub dump: usize,

    /// Elements of an array of strings, as `argv`.
    pub elements: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            string: 32,
            dump: 0,
            elements: 64,
        }
    }
}

/// The longest path a call takes.
pub const PATH_MAX: usize = 4096;

/// Elements of an array of structures copied at most.
const MOST_ELEMENTS: usize = 64;

/// Elements of an iovec array copied at most.
const MOST_IOVECS: usize = 16;

/// Pointers of an environment counted at most.
const MOST_COUNTED: usize = 4096;

/// Reads the traced process's memory.
pub trait Memory {
    /// Reads up to `buffer.len()` bytes at `address`: how many it read,
    /// which an unmapped page may cut short.
    fn read(&mut self, address: u64, buffer: &mut [u8]) -> std::io::Result<usize>;
}

/// When an argument is best read.
fn read_at(call: &str, direction: Direction) -> ReadAt {
    // An `execve` that succeeds leaves no old memory to read at its exit.
    if matches!(direction, Direction::In) || call.starts_with("execve") {
        ReadAt::Entry
    } else {
        ReadAt::Exit
    }
}

fn bytes(
    memory: &mut dyn Memory,
    address: u64,
    wanted: usize,
    limit: usize,
) -> (Vec<u8>, CaptureState) {
    let take = wanted.min(limit);
    let mut buffer = vec![0; take];

    match memory.read(address, &mut buffer) {
        Ok(read) if read == take || take == 0 => {
            let state = if wanted > take {
                CaptureState::Truncated {
                    wanted: u32::try_from(wanted).unwrap_or(u32::MAX),
                }
            } else {
                CaptureState::Complete
            };

            (buffer, state)
        }
        Ok(read) if read > 0 => {
            buffer.truncate(read);
            (
                buffer,
                CaptureState::Truncated {
                    wanted: u32::try_from(wanted).unwrap_or(u32::MAX),
                },
            )
        }
        _ => (Vec::new(), CaptureState::Unreadable),
    }
}

/// A NUL-terminated string of at most `limit` bytes, read a page at a time
/// so that a string ending before an unmapped page is still read.
fn string(memory: &mut dyn Memory, address: u64, limit: usize) -> (Vec<u8>, CaptureState) {
    let mut out = Vec::new();
    let mut at = address;

    loop {
        let to_page = 4096 - usize::try_from(at % 4096).unwrap_or(0);
        let take = to_page.min(limit + 1 - out.len());
        let mut chunk = vec![0; take];
        let read = match memory.read(at, &mut chunk) {
            Ok(read) => read,
            Err(_) if out.is_empty() => return (Vec::new(), CaptureState::Unreadable),
            Err(_) => 0,
        };

        chunk.truncate(read);
        if let Some(end) = chunk.iter().position(|&byte| byte == 0) {
            out.extend_from_slice(&chunk[..end]);
            return (out, CaptureState::Complete);
        }

        out.extend_from_slice(&chunk);
        if out.len() > limit {
            out.truncate(limit);
            return (out, CaptureState::Truncated { wanted: u32::MAX });
        }

        if read < take {
            // The string runs into memory that cannot be read.
            return if out.is_empty() {
                (Vec::new(), CaptureState::Unreadable)
            } else {
                (out, CaptureState::Truncated { wanted: u32::MAX })
            };
        }

        at += read as u64;
    }
}

fn word(memory: &mut dyn Memory, address: u64, size: usize) -> Option<u64> {
    let mut buffer = [0; 8];
    let read = memory.read(address, &mut buffer[..size]).ok()?;
    (read == size).then(|| u64::from_le_bytes(buffer))
}

/// An array of strings, as `argv`: its first `most` elements, each cut at
/// `limit` bytes, and its length when its end was found.
fn strings(
    memory: &mut dyn Memory,
    address: u64,
    abi: Abi,
    most: usize,
    limit: usize,
    count_only: bool,
) -> (Vec<u8>, CaptureState) {
    let size = usize::from(abi.word_bytes());
    let mut pointers = Vec::new();
    let mut ended = false;
    let most = if count_only { MOST_COUNTED } else { most };

    for index in 0..most {
        let Some(pointer) = word(memory, address + (index * size) as u64, size) else {
            if index == 0 {
                return (Vec::new(), CaptureState::Unreadable);
            }

            break;
        };

        if pointer == 0 {
            ended = true;
            break;
        }

        pointers.push(pointer);
    }

    let mut blob = Vec::new();
    blobs::start_strings(
        &mut blob,
        ended.then(|| u32::try_from(pointers.len()).unwrap_or(u32::MAX)),
    );

    if !count_only {
        for &pointer in &pointers {
            let (text, _) = string(memory, pointer, limit);
            blobs::push_string(&mut blob, &text);
        }
    }

    let state = if ended {
        CaptureState::Complete
    } else {
        CaptureState::Truncated { wanted: u32::MAX }
    };

    (blob, state)
}

fn iovecs(
    memory: &mut dyn Memory,
    address: u64,
    count: usize,
    budget: usize,
    abi: Abi,
    limit: usize,
) -> (Vec<u8>, CaptureState) {
    let size = usize::from(abi.word_bytes());
    let shown = count.min(MOST_IOVECS);
    let mut blob = Vec::new();
    let mut left = budget;

    for index in 0..shown {
        let at = address + (index * size * 2) as u64;
        let (Some(base), Some(len)) =
            (word(memory, at, size), word(memory, at + size as u64, size))
        else {
            if index == 0 {
                return (Vec::new(), CaptureState::Unreadable);
            }

            break;
        };

        let wanted = usize::try_from(len).unwrap_or(usize::MAX).min(left);
        left -= wanted;
        let (data, _) = bytes(memory, base, wanted, limit);
        blobs::push_iovec(&mut blob, base, len, &data);
    }

    let state = if shown < count {
        CaptureState::Truncated { wanted: u32::MAX }
    } else {
        CaptureState::Complete
    };

    (blob, state)
}

fn as_count(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// The most bytes one capture holds, which the BPF engine's scratch record
/// bounds.
pub const MOST_BYTES: usize = 4096;

/// Bytes kept of each element of an array of strings or of iovecs.
pub const MOST_ELEMENT_BYTES: usize = 510;

/// How a copy reads its argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyKind {
    Path,
    String,

    /// An array of strings, its elements read.
    Strings,

    /// An array of strings only counted, as an environment.
    StringsCounted,
    Bytes,
    Iovec,
    Sockaddr,
    Struct,

    /// A `select` set, as long as its first argument says.
    FdSet,
}

/// How to copy the memory behind one argument. Both engines follow the
/// same list: the ptrace engine here, the BPF engine's programs from the
/// table microscope fills with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArgCopy {
    pub arg: u8,
    pub kind: CopyKind,
    pub when: ReadAt,

    /// Whether the call writes it, so that a failed call leaves nothing.
    pub writes: bool,
    pub count: Count,

    /// One structure's bytes.
    pub size: u32,

    /// The most bytes kept: of a string, a buffer, the whole array, or
    /// each element of an array of strings or iovecs.
    pub limit: u32,
}

fn limit(bytes: usize) -> u32 {
    u32::try_from(bytes.min(MOST_BYTES)).unwrap_or(u32::MAX)
}

/// What to copy of a call's arguments, in argument order.
#[must_use]
pub fn copies(call: SyscallId, limits: Limits, verbose: bool) -> Vec<ArgCopy> {
    let Some(spec) = specs::spec(call) else {
        return Vec::new();
    };

    let name = call.name().unwrap_or_default();
    let buffer = limits.string.max(limits.dump);
    let mut copies = Vec::new();

    for (index, arg) in spec.args.iter().enumerate() {
        #[expect(clippy::cast_possible_truncation, reason = "six arguments")]
        let arg_index = index as u8;
        let (direction, kind, count, size, most) = match *arg {
            // A string's terminator, and the byte that shows it was cut,
            // fit in the most bytes.
            Arg::Path => (
                Direction::In,
                CopyKind::Path,
                Count::One,
                0,
                PATH_MAX.min(MOST_BYTES - 2),
            ),
            Arg::Str => (
                Direction::In,
                CopyKind::String,
                Count::One,
                0,
                limits.string.min(MOST_BYTES - 2),
            ),
            Arg::StrArray if index == 2 && !verbose => {
                (Direction::In, CopyKind::StringsCounted, Count::One, 0, 4)
            }
            Arg::StrArray => (
                Direction::In,
                CopyKind::Strings,
                Count::One,
                0,
                // One byte past the limit shows the element was longer.
                (limits.string + 1).min(MOST_ELEMENT_BYTES),
            ),
            Arg::Mem {
                direction,
                kind,
                count,
            } => match kind {
                MemKind::Bytes => (direction, CopyKind::Bytes, count, 1, buffer),
                MemKind::Struct(Struct::Iovec) => (
                    direction,
                    CopyKind::Iovec,
                    count,
                    0,
                    buffer.min(MOST_ELEMENT_BYTES),
                ),
                MemKind::Struct(Struct::Sockaddr) => {
                    let size = Struct::Sockaddr.size();
                    (direction, CopyKind::Sockaddr, count, size, size as usize)
                }
                MemKind::Struct(Struct::Dirents) => {
                    (direction, CopyKind::Bytes, count, 1, MOST_BYTES)
                }

                // Its length grows with the kernel's features.
                MemKind::Struct(Struct::Rseq) => (direction, CopyKind::Bytes, count, 1, 64),
                MemKind::Struct(Struct::FdSet) => (direction, CopyKind::FdSet, count, 0, 128),
                MemKind::Struct(structure) => {
                    let size = structure.size();
                    (
                        direction,
                        CopyKind::Struct,
                        count,
                        size,
                        MOST_ELEMENTS * size as usize,
                    )
                }
            },
            _ => continue,
        };

        copies.push(ArgCopy {
            arg: arg_index,
            kind,
            when: read_at(name, direction),
            writes: direction.writes(),
            count,
            size,
            limit: limit(most),
        });
    }

    copies
}

/// Copies the arguments a call reads at `phase`. At exit, `outcome` says
/// how the call ended, which output arguments depend on: a failed call
/// wrote nothing.
#[must_use]
pub fn capture(
    call: SyscallId,
    args: &[u64; 6],
    phase: ReadAt,
    outcome: Option<Outcome>,
    limits: Limits,
    verbose: bool,
    memory: &mut dyn Memory,
) -> Vec<Capture> {
    let returned = outcome.and_then(|outcome| match outcome {
        Outcome::Returned(value) => Some(value),
        _ => None,
    });
    let result = || returned.map_or(0, |value| as_count(value.max(0).unsigned_abs()));
    let mut captured = Vec::new();

    for copy in copies(call, limits, verbose) {
        let address = args[usize::from(copy.arg)];
        if copy.when != phase || address == 0 {
            continue;
        }

        // What a call writes is there only once it succeeded.
        if phase == ReadAt::Exit && copy.writes && returned.is_none() {
            continue;
        }

        let length = match copy.count {
            Count::One => 1,
            Count::Arg(n) => as_count(args[usize::from(n)] & 0xffff_ffff_ffff),
            Count::Return => result(),
            Count::PointedBy(n) => word(memory, args[usize::from(n)], 4).map_or(0, as_count),
        };

        let limit = copy.limit as usize;
        let (blob, state) = match copy.kind {
            CopyKind::Path | CopyKind::String => string(memory, address, limit),
            CopyKind::Strings | CopyKind::StringsCounted => strings(
                memory,
                address,
                call.abi,
                limits.elements.min(MOST_ELEMENTS),
                limit,
                copy.kind == CopyKind::StringsCounted,
            ),
            CopyKind::Bytes => bytes(memory, address, length, limit),
            CopyKind::Iovec => {
                // What a reading call filled is its result; a write sends
                // all it names.
                let budget = if copy.writes { result() } else { usize::MAX };
                iovecs(memory, address, length, budget, call.abi, limit)
            }
            CopyKind::Sockaddr => {
                let wanted = if copy.count == Count::One {
                    copy.size as usize
                } else {
                    length
                };

                bytes(memory, address, wanted, limit)
            }
            CopyKind::Struct => bytes(
                memory,
                address,
                length.saturating_mul(copy.size as usize),
                limit,
            ),
            CopyKind::FdSet => {
                // The set covers the descriptors below the first argument.
                let wanted = as_count(args[0] & 0xffff_ffff).div_ceil(64) * 8;
                bytes(memory, address, wanted, limit)
            }
        };

        captured.push(Capture {
            arg: copy.arg,
            when: phase,
            state,
            bytes: Arc::from(blob),
        });
    }

    captured
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// Memory of a few mapped pages.
    struct Pages(BTreeMap<u64, Vec<u8>>);

    impl Memory for Pages {
        fn read(&mut self, address: u64, buffer: &mut [u8]) -> std::io::Result<usize> {
            let mut read = 0;
            while read < buffer.len() {
                let at = address + read as u64;
                let Some(page) = self.0.get(&(at & !4095)) else {
                    break;
                };

                let offset = usize::try_from(at % 4096).unwrap_or(0);
                let take = (4096 - offset).min(buffer.len() - read);
                buffer[read..read + take].copy_from_slice(&page[offset..offset + take]);
                read += take;
            }

            if read == 0 && !buffer.is_empty() {
                return Err(std::io::Error::from_raw_os_error(14));
            }

            Ok(read)
        }
    }

    fn pages(contents: &[(u64, &[u8])]) -> Pages {
        let mut map = BTreeMap::new();
        for &(address, bytes) in contents {
            for (offset, &byte) in bytes.iter().enumerate() {
                let at = address + offset as u64;
                map.entry(at & !4095).or_insert_with(|| vec![0; 4096])
                    [usize::try_from(at % 4096).unwrap_or(0)] = byte;
            }
        }

        Pages(map)
    }

    fn id(name: &str) -> SyscallId {
        SyscallId::named(Abi::X86_64, name).expect(name)
    }

    #[test]
    fn paths_are_read_at_entry_and_outputs_at_exit_only_on_success() {
        let mut memory = pages(&[(0x1000, b"/etc/hosts\0"), (0x3000, b"hello")]);
        let open = [(-100_i64).cast_unsigned(), 0x1000, 0, 0, 0, 0];
        let entry = capture(
            id("openat"),
            &open,
            ReadAt::Entry,
            None,
            Limits::default(),
            false,
            &mut memory,
        );
        assert_eq!(entry.len(), 1);
        assert_eq!(
            (&*entry[0].bytes, entry[0].state),
            (&b"/etc/hosts"[..], CaptureState::Complete)
        );
        assert!(
            capture(
                id("openat"),
                &open,
                ReadAt::Exit,
                Some(Outcome::Returned(3)),
                Limits::default(),
                false,
                &mut memory
            )
            .is_empty()
        );

        let read = [3, 0x3000, 4096, 0, 0, 0];
        let done = capture(
            id("read"),
            &read,
            ReadAt::Exit,
            Some(Outcome::Returned(5)),
            Limits::default(),
            false,
            &mut memory,
        );
        assert_eq!(&*done[0].bytes, b"hello");
        let failed = capture(
            id("read"),
            &read,
            ReadAt::Exit,
            Some(Outcome::of_return(-11)),
            Limits::default(),
            false,
            &mut memory,
        );
        assert!(failed.is_empty());
    }

    #[test]
    fn strings_stop_at_unmapped_pages_and_limits() {
        // A string that ends just before an unmapped page.
        let mut memory = pages(&[(0x1ffa, b"short\0")]);
        let (text, state) = string(&mut memory, 0x1ffa, PATH_MAX);
        assert_eq!((&text[..], state), (&b"short"[..], CaptureState::Complete));

        // One that runs into it.
        let mut memory = pages(&[(0x1ffa, b"abcdef")]);
        let (text, state) = string(&mut memory, 0x1ffa, PATH_MAX);
        assert_eq!(
            (&text[..], state),
            (&b"abcdef"[..], CaptureState::Truncated { wanted: u32::MAX })
        );
        let (_, state) = string(&mut memory, 0x9000, PATH_MAX);
        assert_eq!(state, CaptureState::Unreadable);
        let mut memory = pages(&[(0x1000, b"0123456789\0")]);
        let (text, state) = string(&mut memory, 0x1000, 4);
        assert_eq!(
            (&text[..], state),
            (&b"0123"[..], CaptureState::Truncated { wanted: u32::MAX })
        );
    }

    #[test]
    fn argv_iovecs_and_value_result_sockaddrs_follow_their_counts() {
        let argv: Vec<u8> = [0x2000u64, 0x2010, 0]
            .iter()
            .flat_map(|p| p.to_le_bytes())
            .collect();
        let mut memory = pages(&[
            (0x1000, &argv),
            (0x2000, b"ls\0"),
            (0x2010, b"-l\0"),
            (0x3000, b"/bin/ls\0"),
        ]);
        let exec = [0x3000, 0x1000, 0, 0, 0, 0];
        let entry = capture(
            id("execve"),
            &exec,
            ReadAt::Entry,
            None,
            Limits::default(),
            false,
            &mut memory,
        );
        let (count, elements) = blobs::strings(&entry[1].bytes);
        assert_eq!((count, elements), (Some(2), vec![&b"ls"[..], &b"-l"[..]]));

        let vector: Vec<u8> = [0x5000u64, 3, 0x6000, 10]
            .iter()
            .flat_map(|p| p.to_le_bytes())
            .collect();
        let mut memory = pages(&[(0x4000, &vector), (0x5000, b"abc"), (0x6000, b"defghijklm")]);
        let readv = [3, 0x4000, 2, 0, 0, 0];
        let exit = capture(
            id("readv"),
            &readv,
            ReadAt::Exit,
            Some(Outcome::Returned(5)),
            Limits::default(),
            false,
            &mut memory,
        );
        let read = blobs::iovecs(&exit[0].bytes);
        assert_eq!(
            read.iter().map(|iovec| iovec.bytes).collect::<Vec<_>>(),
            [&b"abc"[..], &b"de"[..]]
        );

        let address = [2u8, 0, 0, 80, 127, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
        let mut memory = pages(&[(0x7000, &address), (0x8000, &16u32.to_le_bytes())]);
        let accept = [3, 0x7000, 0x8000, 0, 0, 0];
        let exit = capture(
            id("accept"),
            &accept,
            ReadAt::Exit,
            Some(Outcome::Returned(4)),
            Limits::default(),
            false,
            &mut memory,
        );
        assert_eq!(
            exit.iter()
                .find(|capture| capture.arg == 1)
                .map(|capture| capture.bytes.len()),
            Some(16)
        );
    }
}
