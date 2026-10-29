//! Decodes the records perf events write, for the one sample layout the
//! recorder asks for, and the records BPF programs write beside them
//! through output events, which their identifiers tell apart. Decoding is
//! pure: a record that does not fit its own size is malformed, never read
//! past.

use super::sys::sample;
use crate::model::{Pid, Tid};
use crate::unwind::DwarfRegister;

/// The sample fields every event asks for, in the kernel's order.
pub const SAMPLE_TYPE: u64 = sample::IDENTIFIER
    | sample::TID
    | sample::TIME
    | sample::CPU
    | sample::PERIOD
    | sample::CALLCHAIN
    | sample::REGS_USER
    | sample::STACK_USER;

/// The fields of [`SAMPLE_TYPE`] the `sample_id` every other record ends
/// with holds: an event that asks for these alone writes records that end
/// as a sampling event's do.
pub const SAMPLE_ID_TYPE: u64 = sample::IDENTIFIER | sample::TID | sample::TIME | sample::CPU;

/// The `sample_id` every other record ends with: tid, time, cpu, id.
const SAMPLE_ID: usize = 32;

/// The general registers, by perf's numbering: ax, bx, cx, dx, si, di, bp,
/// sp, ip, flags, then r8 to r15.
pub const REGISTERS: u64 = 0x3ff | (0xff << 16);

/// The DWARF number of each register [`REGISTERS`] asks for, in the order
/// the kernel writes them.
pub const DWARF_REGISTERS: [DwarfRegister; 18] =
    DwarfRegister::each([0, 3, 2, 1, 4, 5, 6, 7, 16, 49, 8, 9, 10, 11, 12, 13, 14, 15]);

/// The DWARF number of the flags register.
pub const DWARF_FLAGS: DwarfRegister = DwarfRegister::new(49);

const RECORD_MMAP2: u32 = 10;
const RECORD_COMM: u32 = 3;
const RECORD_EXIT: u32 = 4;
const RECORD_FORK: u32 = 7;
const RECORD_LOST: u32 = 2;
const RECORD_THROTTLE: u32 = 5;
const RECORD_SAMPLE: u32 = 9;
const RECORD_LOST_SAMPLES: u32 = 13;
const RECORD_SWITCH: u32 = 14;
const MISC_COMM_EXEC: u16 = 1 << 13;
const MISC_SWITCH_OUT: u16 = 1 << 13;
const MISC_SWITCH_OUT_PREEMPT: u16 = 1 << 14;
const MISC_MMAP_BUILD_ID: u16 = 1 << 14;
const MISC_CPUMODE_MASK: u16 = 7;
const MISC_KERNEL: u16 = 1;

/// Callchain entries at or above this mark a context, not an address.
const CONTEXT_MAX: u64 = (-4095_i64).cast_unsigned();
const CONTEXT_USER: u64 = (-512_i64).cast_unsigned();

/// Little-endian words in a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Words<'a>(&'a [u8]);

impl Words<'_> {
    pub fn iter(self) -> impl Iterator<Item = u64> {
        self.0
            .as_chunks::<8>()
            .0
            .iter()
            .map(|word| u64::from_le_bytes(*word))
    }
}

/// One sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample<'a> {
    pub pid: Pid,
    pub tid: Tid,
    pub time: u64,
    pub cpu: u32,
    pub period: u64,
    pub kernel: bool,

    /// The kernel's frame-pointer walk, context marks included.
    pub chain: Words<'a>,

    /// [`REGISTERS`] in the kernel's order, or none when the sample
    /// carries no user registers.
    pub registers: Words<'a>,

    /// The bytes of stack the kernel copied from the stack pointer up.
    pub stack: &'a [u8],

    /// The bytes of stack the sample asked for, which the copy may fall
    /// short of.
    pub stack_asked: u64,
}

impl Sample<'_> {
    /// The walk's user addresses, without context marks.
    pub fn user_chain(&self) -> impl Iterator<Item = u64> + '_ {
        let mut user = !self.kernel;
        self.chain.iter().filter(move |address| {
            if *address >= CONTEXT_MAX {
                user = *address == CONTEXT_USER;
                false
            } else {
                user
            }
        })
    }
}

/// A mapping of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mmap<'a> {
    pub pid: Pid,
    pub start: u64,
    pub length: u64,
    pub file_offset: u64,

    /// The file's inode, unless the kernel gave its build id instead.
    pub inode: Option<u64>,
    pub executable: bool,
    pub path: &'a [u8],
    pub time: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Record<'a> {
    Sample(Sample<'a>),

    /// The bytes a BPF program wrote through an output event, on the CPU
    /// whose ring holds them, as the thread it ran for.
    Output {
        pid: Pid,
        tid: Tid,
        time: u64,
        cpu: u32,
        data: &'a [u8],
    },
    Mmap(Mmap<'a>),

    /// A thread's name, set or, when `exec`, changed by an exec.
    Comm {
        pid: Pid,
        tid: Tid,
        name: &'a [u8],
        exec: bool,
        time: u64,
    },
    Fork {
        pid: Pid,
        ppid: Pid,
        tid: Tid,

        /// The thread that made it, whose name it starts with.
        ptid: Tid,
        time: u64,
    },
    Exit {
        pid: Pid,
        tid: Tid,
        time: u64,
    },

    /// Records or samples the kernel dropped.
    Lost {
        count: u64,
        time: u64,
    },
    Throttle {
        time: u64,
    },

    /// A thread left its CPU, or came back to one. One that left while it
    /// could still run was `preempted`; one that did not blocked.
    Switch {
        pid: Pid,
        tid: Tid,
        out: bool,
        preempted: bool,
        time: u64,
    },

    /// A record the recorder does not use.
    Other {
        time: Option<u64>,
    },
}

impl Record<'_> {
    /// When the kernel wrote the record.
    pub const fn time(&self) -> Option<u64> {
        match self {
            Self::Sample(sample) => Some(sample.time),
            Self::Mmap(mmap) => Some(mmap.time),
            Self::Output { time, .. }
            | Self::Comm { time, .. }
            | Self::Fork { time, .. }
            | Self::Exit { time, .. }
            | Self::Lost { time, .. }
            | Self::Switch { time, .. }
            | Self::Throttle { time } => Some(*time),
            Self::Other { time } => *time,
        }
    }
}

/// A record whose bytes contradict its header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

/// Reads fields in order from a record's body.
struct Fields<'a> {
    bytes: &'a [u8],
}

impl<'a> Fields<'a> {
    const fn take(&mut self, length: usize) -> Result<&'a [u8], Malformed> {
        if length > self.bytes.len() {
            return Err(Malformed);
        }

        let (taken, rest) = self.bytes.split_at(length);
        self.bytes = rest;
        Ok(taken)
    }

    fn u64(&mut self) -> Result<u64, Malformed> {
        let word = self.take(8)?;
        Ok(u64::from_le_bytes(word.try_into().map_err(|_| Malformed)?))
    }

    /// Two u32 fields in one word.
    fn pair(&mut self) -> Result<(u32, u32), Malformed> {
        let word = self.take(8)?;
        let (low, high) = word.split_at(4);
        Ok((
            u32::from_le_bytes(low.try_into().map_err(|_| Malformed)?),
            u32::from_le_bytes(high.try_into().map_err(|_| Malformed)?),
        ))
    }

    fn words(&mut self, count: u64) -> Result<Words<'a>, Malformed> {
        let length = usize::try_from(count)
            .ok()
            .and_then(|count| count.checked_mul(8))
            .ok_or(Malformed)?;
        Ok(Words(self.take(length)?))
    }
}

/// The `N` bytes at `offset`, or `Malformed` if the body ends before them.
fn field<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], Malformed> {
    bytes
        .get(offset..)
        .and_then(<[u8]>::first_chunk::<N>)
        .copied()
        .ok_or(Malformed)
}

/// The record at the start of `bytes` and its length, or `None` at the
/// end, where no output events write.
pub fn next(bytes: &[u8]) -> Option<Result<(Record<'_>, usize), Malformed>> {
    next_of(bytes, &[])
}

/// The record at the start of `bytes` and its length, or `None` at the
/// end, where the output events whose identifiers are `outputs` write.
pub fn next_of<'a>(
    bytes: &'a [u8],
    outputs: &[u64],
) -> Option<Result<(Record<'a>, usize), Malformed>> {
    next_with_period(bytes, outputs, None)
}

/// Decodes a fixed-period event that omits `PERIOD`, or the usual layout.
pub fn next_with_period<'a>(
    bytes: &'a [u8],
    outputs: &[u64],
    period: Option<u64>,
) -> Option<Result<(Record<'a>, usize), Malformed>> {
    if bytes.is_empty() {
        return None;
    }

    if bytes.len() < 8 {
        return Some(Err(Malformed));
    }

    let Some(&[k0, k1, k2, k3, m0, m1, s0, s1]) = bytes.first_chunk::<8>() else {
        return Some(Err(Malformed));
    };

    let kind = u32::from_le_bytes([k0, k1, k2, k3]);
    let misc = u16::from_le_bytes([m0, m1]);
    let size = usize::from(u16::from_le_bytes([s0, s1]));
    if size < 8 || size > bytes.len() {
        return Some(Err(Malformed));
    }

    let body = &bytes[8..size];
    Some(decode(kind, misc, body, outputs, period).map(|record| (record, size)))
}

fn decode<'a>(
    kind: u32,
    misc: u16,
    body: &'a [u8],
    outputs: &[u64],
    period: Option<u64>,
) -> Result<Record<'a>, Malformed> {
    if kind == RECORD_SAMPLE {
        let identifier = u64::from_le_bytes(field(body, 0)?);
        if outputs.contains(&identifier) {
            return decode_output(body);
        }

        return decode_sample(body, misc & MISC_CPUMODE_MASK == MISC_KERNEL, period)
            .map(Record::Sample);
    }

    // Every other record ends with its sample_id.
    let Some(split) = body.len().checked_sub(SAMPLE_ID) else {
        return Ok(Record::Other { time: None });
    };

    let (body, id) = body.split_at(split);
    let time = u64::from_le_bytes(field(id, 8)?);
    let mut fields = Fields { bytes: body };

    Ok(match kind {
        RECORD_MMAP2 => {
            let (pid, _tid) = fields.pair()?;
            let start = fields.u64()?;
            let length = fields.u64()?;
            let file_offset = fields.u64()?;
            let device = fields.take(24)?;
            let inode = if misc & MISC_MMAP_BUILD_ID == 0 {
                Some(u64::from_le_bytes(field(device, 8)?))
            } else {
                None
            };

            let (protection, _flags) = fields.pair()?;
            let path = fields.bytes;
            let path = &path[..path
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(path.len())];

            Record::Mmap(Mmap {
                pid: Pid::new(pid),
                start,
                length,
                file_offset,
                inode,
                executable: protection & 4 != 0,
                path,
                time,
            })
        }
        RECORD_COMM => {
            let (pid, tid) = fields.pair()?;
            let name = fields.bytes;
            let name = &name[..name
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(name.len())];

            Record::Comm {
                pid: Pid::new(pid),
                tid: Tid::new(tid),
                name,
                exec: misc & MISC_COMM_EXEC != 0,
                time,
            }
        }
        RECORD_FORK | RECORD_EXIT => {
            let (pid, parent) = fields.pair()?;
            let (tid, parent_tid) = fields.pair()?;
            let (pid, tid) = (Pid::new(pid), Tid::new(tid));

            if kind == RECORD_FORK {
                Record::Fork {
                    pid,
                    ppid: Pid::new(parent),
                    tid,
                    ptid: Tid::new(parent_tid),
                    time,
                }
            } else {
                Record::Exit { pid, tid, time }
            }
        }
        RECORD_LOST => {
            let _id = fields.u64()?;
            Record::Lost {
                count: fields.u64()?,
                time,
            }
        }
        RECORD_LOST_SAMPLES => Record::Lost {
            count: fields.u64()?,
            time,
        },
        RECORD_THROTTLE => Record::Throttle { time },
        // The thread is the sample_id's, which a switch record is all of.
        RECORD_SWITCH => Record::Switch {
            pid: Pid::new(u32::from_le_bytes(field(id, 0)?)),
            tid: Tid::new(u32::from_le_bytes(field(id, 4)?)),
            out: misc & MISC_SWITCH_OUT != 0,
            preempted: misc & MISC_SWITCH_OUT_PREEMPT != 0,
            time,
        },
        _ => Record::Other { time: Some(time) },
    })
}

fn decode_sample(body: &[u8], kernel: bool, period: Option<u64>) -> Result<Sample<'_>, Malformed> {
    let mut fields = Fields { bytes: body };
    let _identifier = fields.u64()?;
    let (pid, tid) = fields.pair()?;
    let time = fields.u64()?;
    let (cpu, _) = fields.pair()?;
    let period = match period {
        Some(period) => period,
        None => fields.u64()?,
    };
    let chain_length = fields.u64()?;
    let chain = fields.words(chain_length)?;
    let abi = fields.u64()?;
    let registers = if abi == 0 {
        Words(&[])
    } else {
        fields.words(u64::from(REGISTERS.count_ones()))?
    };

    let size = fields.u64()?;
    let stack = if size == 0 {
        &[][..]
    } else {
        let data = fields.take(usize::try_from(size).map_err(|_| Malformed)?)?;
        let used = usize::try_from(fields.u64()?).map_err(|_| Malformed)?;
        data.get(..used).ok_or(Malformed)?
    };

    Ok(Sample {
        pid: Pid::new(pid),
        tid: Tid::new(tid),
        time,
        cpu,
        period,
        kernel,
        chain,
        registers,
        stack,
        stack_asked: size,
    })
}

/// Copies the records of `bytes` to `out`, each sample of the usual layout
/// that an event whose identifier is in `sampling` wrote keeping at most
/// `keep` bytes of its stack copy, a multiple of 8, and returns how many
/// samples it shortened. A record that cannot be decoded is copied as it
/// is, with all that follows it.
pub fn shorten_stacks(bytes: &[u8], sampling: &[u64], keep: usize, out: &mut Vec<u8>) -> u64 {
    let mut shortened = 0;
    let mut rest = bytes;
    while let Some(header) = rest.first_chunk::<8>() {
        let size = usize::from(u16::from_le_bytes([header[6], header[7]]));
        let Some(record) = rest.get(..size).filter(|_| size >= 8) else {
            break;
        };

        rest = &rest[size..];
        let kind = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let short = (kind == RECORD_SAMPLE)
            .then(|| short_sample(record, sampling, keep))
            .flatten();
        if let Some((stack, end)) = short {
            // The stack's size, `keep` bytes of it, and how many of those
            // the kernel filled.
            let used = field(record, end - 8).map_or(0, u64::from_le_bytes);
            let length = stack + 8 + keep + 8 + (record.len() - end);
            let Ok(length) = u16::try_from(length) else {
                out.extend_from_slice(record);
                continue;
            };
            out.extend_from_slice(&header[..6]);
            out.extend_from_slice(&length.to_le_bytes());
            out.extend_from_slice(&record[8..stack]);
            out.extend_from_slice(&(keep as u64).to_le_bytes());
            out.extend_from_slice(&record[stack + 8..stack + 8 + keep]);
            out.extend_from_slice(&used.min(keep as u64).to_le_bytes());
            out.extend_from_slice(&record[end..]);
            shortened += 1;
        } else {
            out.extend_from_slice(record);
        }
    }

    out.extend_from_slice(rest);
    shortened
}

/// Where the stack copy of `record`, a whole sample record, begins, at its
/// size, and where it ends, after how much of it the kernel filled, if an
/// event in `sampling` wrote it and it copied more than `keep` bytes.
fn short_sample(record: &[u8], sampling: &[u64], keep: usize) -> Option<(usize, usize)> {
    let word = |at: usize| field(record, at).ok().map(u64::from_le_bytes);
    if !sampling.contains(&word(8)?) {
        return None;
    }

    // The header, then identifier, pid and tid, time, cpu, and period.
    let chain: usize = 8 + 8 * 5;
    let abi = chain.checked_add(8 + 8 * usize::try_from(word(chain)?).ok()?)?;
    let registers = if word(abi)? == 0 {
        0
    } else {
        8 * REGISTERS.count_ones() as usize
    };
    let stack = abi + 8 + registers;
    let size = usize::try_from(word(stack)?).ok()?;
    let end = stack.checked_add(8 + size + 8)?;
    (size > keep && end <= record.len()).then_some((stack, end))
}

/// A record an output event wrote: [`super::OUTPUT_SAMPLE_TYPE`]'s fields.
fn decode_output(body: &[u8]) -> Result<Record<'_>, Malformed> {
    let mut fields = Fields { bytes: body };
    let _identifier = fields.u64()?;
    let (pid, tid) = fields.pair()?;
    let time = fields.u64()?;
    let (cpu, _) = fields.pair()?;
    let size = fields.take(4)?;
    let size = u32::from_le_bytes(size.try_into().map_err(|_| Malformed)?);
    let data = fields.take(usize::try_from(size).map_err(|_| Malformed)?)?;

    Ok(Record::Output {
        pid: Pid::new(pid),
        tid: Tid::new(tid),
        time,
        cpu,
        data,
    })
}

/// Decodes every record of `data` until one is malformed.
#[cfg(feature = "fuzzing")]
pub fn fuzz(data: &[u8]) {
    let mut offset = 0;

    while let Some(Ok((record, length))) = next_of(&data[offset..], &[7]) {
        if let Record::Sample(sample) = record {
            let _ = sample.user_chain().count() + sample.registers.iter().count();
        }

        let _ = record.time();
        offset += length;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_callchain_addresses_never_enter_the_user_walk() {
        let mut rest = Vec::new();
        for word in [
            1000_u64,
            5,
            (-128_i64).cast_unsigned(),
            0xffff_ffff_8100_0000,
            (-512_i64).cast_unsigned(),
            0x0040_1000,
            0x0040_2000,
            0,
            0,
        ] {
            rest.extend_from_slice(&word.to_le_bytes());
        }
        let bytes = sample_record(1, &rest);
        let Some(Ok((Record::Sample(sample), _))) = next(&bytes) else {
            panic!("sample");
        };
        assert_eq!(
            sample.user_chain().collect::<Vec<_>>(),
            [0x0040_1000, 0x0040_2000]
        );
    }

    fn decoded(bytes: &[u8]) -> (Sample<'_>, usize) {
        let Some(Ok((Record::Sample(sample), length))) = next(bytes) else {
            panic!("a sample");
        };
        (sample, length)
    }

    /// A shortened sample keeps every field but the end of its stack copy,
    /// and how much of the copy the kernel filled shrinks with it. Records
    /// of other events, and samples that copied no more, are left alone.
    #[test]
    fn shortening_a_sample_keeps_all_but_the_end_of_its_stack() {
        let sample = |stack: &[u8], used: u64| {
            let mut rest = Vec::new();
            // The period, a chain of two, and every register.
            for word in [1_000_000_u64, 2, 0x40_1000, 0x40_2000, 2] {
                rest.extend_from_slice(&word.to_le_bytes());
            }
            for register in 0..u64::from(REGISTERS.count_ones()) {
                rest.extend_from_slice(&(100 + register).to_le_bytes());
            }
            rest.extend_from_slice(&(stack.len() as u64).to_le_bytes());
            rest.extend_from_slice(stack);
            rest.extend_from_slice(&used.to_le_bytes());
            sample_record(1, &rest)
        };
        let stack: Vec<u8> = (0..64).collect();
        let long = sample(&stack, 40);
        let short = sample(&stack[..16], 16);
        let other = sample_record(2, b"abcdefgh");

        let mut bytes = long.clone();
        bytes.extend_from_slice(&short);
        bytes.extend_from_slice(&other);
        let mut out = Vec::new();
        assert_eq!(shorten_stacks(&bytes, &[1], 16, &mut out), 1);

        let (was, _) = decoded(&long);
        let (now, length) = decoded(&out);
        assert_eq!(length, long.len() - 48);
        assert_eq!(now.stack, &stack[..16]);
        assert_eq!(now.stack_asked, 16);
        assert_eq!(
            (
                now.pid,
                now.tid,
                now.time,
                now.cpu,
                now.period,
                now.chain,
                now.registers
            ),
            (
                was.pid,
                was.tid,
                was.time,
                was.cpu,
                was.period,
                was.chain,
                was.registers
            )
        );
        assert_eq!(&out[length..], [short, other].concat());

        // A copy the kernel filled less of than is kept keeps all it filled.
        let mut out = Vec::new();
        shorten_stacks(&sample(&stack, 8), &[1], 16, &mut out);
        assert_eq!(decoded(&out).0.stack, &stack[..8]);

        // A record cut short is copied as it is.
        let mut out = Vec::new();
        assert_eq!(shorten_stacks(&long[..40], &[1], 16, &mut out), 0);
        assert_eq!(out, &long[..40]);
    }

    /// A sample record, as an event of `identifier` writes it: the fields
    /// before its own, then `rest`.
    fn sample_record(identifier: u64, rest: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&identifier.to_le_bytes());
        body.extend_from_slice(&7_u32.to_le_bytes());
        body.extend_from_slice(&8_u32.to_le_bytes());
        body.extend_from_slice(&1000_u64.to_le_bytes());
        body.extend_from_slice(&3_u32.to_le_bytes());
        body.extend_from_slice(&0_u32.to_le_bytes());
        body.extend_from_slice(rest);
        let mut record = Vec::new();
        record.extend_from_slice(&RECORD_SAMPLE.to_le_bytes());
        record.extend_from_slice(&0_u16.to_le_bytes());
        record.extend_from_slice(&u16::try_from(8 + body.len()).expect("small").to_le_bytes());
        record.extend_from_slice(&body);
        record
    }

    /// An output event's record is told from a sample by its identifier,
    /// and carries the program's bytes, padding and all; one whose bytes
    /// overrun the record is malformed.
    #[test]
    fn an_output_events_records_are_told_from_samples_by_their_identifier() {
        let mut raw = 4_u32.to_le_bytes().to_vec();
        raw.extend_from_slice(b"abcd");
        let record = sample_record(42, &raw);
        let Some(Ok((
            Record::Output {
                pid,
                tid,
                time,
                cpu,
                data,
            },
            length,
        ))) = next_of(&record, &[41, 42])
        else {
            panic!("an output record");
        };

        assert_eq!(length, record.len());
        assert_eq!((pid.get(), tid.get(), time, cpu), (7, 8, 1000, 3));
        assert_eq!(data, b"abcd");

        // Read as a sample, its bytes are a sample's, which they do not fit.
        assert!(!matches!(
            next(&record),
            Some(Ok((Record::Output { .. }, _)))
        ));

        let mut overrun = 9_u32.to_le_bytes().to_vec();
        overrun.extend_from_slice(b"abcd");
        assert_eq!(
            next_of(&sample_record(42, &overrun), &[42]),
            Some(Err(Malformed))
        );
    }
}
