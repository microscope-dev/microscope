//! Hex dumps of the data a call read or wrote, as strace's `-e read=` and
//! `-e write=` print them.

use std::fmt::Write as _;

use microscope::syscalls::blobs;
use microscope::syscalls::{CaptureState, Outcome, SyscallEvent};

/// Which way data moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Read,
    Write,
}

/// How a call carries its data: one buffer, or an array of iovecs, as its
/// second argument; or a message header, whose buffers no engine captures.
enum Shape {
    Buffer,
    Iovecs,
    Message,
}

fn shape(name: &str) -> Option<(Direction, Shape)> {
    Some(match name {
        "read" | "pread64" | "recvfrom" => (Direction::Read, Shape::Buffer),
        "readv" | "preadv" | "preadv2" => (Direction::Read, Shape::Iovecs),
        "recvmsg" | "recvmmsg" => (Direction::Read, Shape::Message),
        "write" | "pwrite64" | "sendto" => (Direction::Write, Shape::Buffer),
        "writev" | "pwritev" | "pwritev2" => (Direction::Write, Shape::Iovecs),
        "sendmsg" | "sendmmsg" => (Direction::Write, Shape::Message),
        _ => return None,
    })
}

/// The descriptor a call moves data through, and which way, when it is a
/// call strace dumps.
#[must_use]
pub fn moves(event: &SyscallEvent) -> Option<(i64, Direction)> {
    let (direction, _) = shape(event.call.name()?)?;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a descriptor is the low 32 bits"
    )]
    let fd = i64::from(event.args[0] as i32);
    Some((fd, direction))
}

/// The lines that dump what a call moved: none when it moved nothing. A
/// buffer captured short says how many bytes it leaves out, and one that
/// could not be read says so, rather than dumping less in silence.
#[must_use]
pub fn dump(event: &SyscallEvent) -> Vec<String> {
    let Some((_, shape)) = event.call.name().and_then(shape) else {
        return Vec::new();
    };

    let Outcome::Returned(moved) = event.outcome else {
        return Vec::new();
    };

    let Ok(moved) = usize::try_from(moved) else {
        return Vec::new();
    };

    if moved == 0 {
        return Vec::new();
    }

    let mut lines = Vec::new();
    let Some(capture) = event.capture(1) else {
        lines.push(" * not dumped: the buffer was not captured".to_owned());
        return lines;
    };

    match (shape, capture.state) {
        (Shape::Message, _) => {
            lines.push(" * not dumped: microscope captures no message's buffers".to_owned());
        }
        (_, CaptureState::Unreadable) => {
            lines.push(format!(" * not dumped: {moved} bytes were unreadable"));
        }
        (_, CaptureState::NotRequested) => {
            lines.push(" * not dumped: the buffer was not captured".to_owned());
        }
        (Shape::Buffer, _) => buffer(&mut lines, &capture.bytes, moved),
        (Shape::Iovecs, _) => {
            let mut left = moved;
            for (index, element) in blobs::iovecs(&capture.bytes).iter().enumerate() {
                if left == 0 {
                    break;
                }

                let length = usize::try_from(element.len).unwrap_or(usize::MAX).min(left);
                left -= length;
                lines.push(format!(" * {length} bytes in buffer {index}"));
                buffer(&mut lines, element.bytes, length);
            }

            if left > 0 {
                lines.push(format!(" * {left} more bytes in buffers not captured"));
            }
        }
    }

    lines
}

/// Dumps the first `length` bytes of a buffer of which `bytes` were
/// captured.
fn buffer(lines: &mut Vec<String>, bytes: &[u8], length: usize) {
    let shown = &bytes[..bytes.len().min(length)];
    for (row, chunk) in shown.chunks(16).enumerate() {
        let mut line = format!(" | {:05x}  ", row * 16);
        for column in 0..16 {
            match chunk.get(column) {
                Some(byte) => {
                    let _ = write!(line, "{byte:02x} ");
                }
                None => line.push_str("   "),
            }

            if column == 7 {
                line.push(' ');
            }
        }

        line.push(' ');

        for column in 0..16 {
            line.push(match chunk.get(column) {
                Some(&byte) if (0x20..0x7f).contains(&byte) => char::from(byte),
                Some(_) => '.',
                None => ' ',
            });
        }

        line.push_str(" |");
        lines.push(line);
    }

    if shown.len() < length {
        lines.push(format!(
            " * {} more bytes not captured",
            length - shown.len()
        ));
    }
}
