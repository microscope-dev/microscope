//! Numeric fields from kernel stat/status records. Command names need not
//! be UTF-8; only the selected numeric fields are decoded.

use std::io::Read as _;
use std::os::fd::OwnedFd;

use nix::fcntl::{OFlag, openat};
use nix::sys::stat::Mode;

use crate::runtime_model::TargetThreadId;

const MAX_STATUS_BYTES: usize = 64 * 1024;
const MAX_ID_VALUES: usize = 64;

/// Room for a whole record of most files: from an empty buffer, reading to
/// the end begins with reads of a few bytes each.
const RECORD_BYTES: usize = 4096;

fn read(file: impl std::io::Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(RECORD_BYTES);
    file.take((MAX_STATUS_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    Ok(bounded(bytes))
}

/// The record a kept `/proc` file holds now, read from its start. Each read
/// at offset 0 has the kernel write the record anew (`seq_read_iter`), and
/// it copies all of a record that fits, so a read shorter than the buffer
/// is the whole record; one that fills it reads on from where it stopped.
fn reread(file: &std::fs::File) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt as _;

    let mut bytes = vec![0; RECORD_BYTES];
    let mut length = 0;
    loop {
        let room = bytes.get_mut(length..).unwrap_or_default();
        match file.read_at(room, length as u64) {
            Ok(0) => break,
            Ok(read) => {
                length += read;
                if length < bytes.len() || length > MAX_STATUS_BYTES {
                    break;
                }
                bytes.resize((bytes.len() * 2).min(MAX_STATUS_BYTES + 1), 0);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    bytes.truncate(length);
    Ok(bounded(bytes))
}

fn bounded(mut bytes: Vec<u8>) -> Vec<u8> {
    if bytes.len() > MAX_STATUS_BYTES {
        bytes.truncate(MAX_STATUS_BYTES);
        // A truncated final number is not identity evidence. Earlier
        // complete fields remain usable, even with a huge CPU mask.
        let end = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |at| at + 1);
        bytes.truncate(end);
    }

    bytes
}

pub(super) fn read_path(path: impl AsRef<std::path::Path>) -> std::io::Result<Vec<u8>> {
    read(std::fs::File::open(path)?)
}

pub(super) fn effective_uids(process: &OwnedFd) -> std::io::Result<(u32, u32)> {
    let fd = openat(
        process,
        "status",
        OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;

    uid_pair(&read(std::fs::File::from(fd))?).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the target's effective and filesystem UIDs cannot be read",
        )
    })
}

pub(super) fn target_thread_id(process: u32, thread: u32) -> Option<TargetThreadId> {
    target_thread_ids(process, &[thread])
        .into_iter()
        .next()
        .map(|(_, target)| target)
}

/// Anchor one proc mount while validating its namespace and each member.
/// The caller bounds the controlled thread catalog.
pub(super) fn target_thread_ids(process: u32, threads: &[u32]) -> Vec<(u32, TargetThreadId)> {
    namespace().map_or_else(Vec::new, |root| {
        threads
            .iter()
            .filter_map(|&thread| {
                if !valid_pid(process) || !valid_pid(thread) {
                    return None;
                }
                let status = read_at(&root, &format!("{thread}/status"))?;
                Some((thread, thread_in_status(&status, process, thread)?))
            })
            .collect()
    })
}

fn read_at(root: &OwnedFd, name: &str) -> Option<Vec<u8>> {
    let file = openat(
        root,
        name,
        OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::empty(),
    )
    .ok()?;
    read(std::fs::File::from(file)).ok()
}

fn namespace() -> Option<OwnedFd> {
    let root = nix::fcntl::open(
        "/proc",
        OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW,
        Mode::empty(),
    )
    .ok()?;
    let own = read_at(&root, "self/status")?;

    // NSpid starts in the namespace of the proc mount. A different proc
    // view could make the debugger's numeric TID name another process.
    own_namespace(&own, std::process::id()).then_some(root)
}

/// Numeric stat fields are separated from an arbitrary byte-string comm.
fn stat_field(stat: &[u8], index: usize) -> Option<&[u8]> {
    let open = stat.iter().position(|byte| *byte == b'(')?;
    let close = stat.iter().rposition(|byte| *byte == b')')?;
    if open == 0
        || close <= open
        || !stat.get(open.checked_sub(1)?)?.is_ascii_whitespace()
        || !stat.get(close.checked_add(1)?)?.is_ascii_whitespace()
    {
        return None;
    }

    let pid = u32::try_from(decimal(stat.get(..open)?.trim_ascii())?).ok()?;
    if !valid_pid(pid) {
        return None;
    }

    let fields = stat.get(close.checked_add(1)?..)?;
    let state = fields
        .split(u8::is_ascii_whitespace)
        .find(|field| !field.is_empty())?;
    if state.len() != 1 || !state.first()?.is_ascii_alphabetic() {
        return None;
    }

    let field = fields
        .split_inclusive(u8::is_ascii_whitespace)
        .filter(|field| !field.trim_ascii().is_empty())
        .nth(index)?;

    // Do not accept a truncated final number as identity evidence.
    field
        .last()?
        .is_ascii_whitespace()
        .then(|| field.trim_ascii())
}

fn decimal(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

pub(super) fn stat_start_time(stat: &[u8]) -> Option<u64> {
    decimal(stat_field(stat, 19)?)
}

pub(super) fn stat_state(stat: &[u8]) -> Option<u8> {
    stat_field(stat, 0)?.first().copied()
}

pub(super) fn stat_exited(stat: &[u8]) -> bool {
    const EXITING: u64 = 0x4;
    matches!(stat_state(stat), Some(b'Z' | b'X'))
        || stat_field(stat, 6)
            .and_then(decimal)
            .is_some_and(|flags| flags & EXITING != 0)
}

pub(super) fn process_start_time(process: i32) -> Option<u64> {
    if process <= 0 {
        return None;
    }
    stat_start_time(&read_path(format!("/proc/{process}/stat")).ok()?)
}

pub(super) fn thread_start_time(
    process: crate::model::Pid,
    thread: crate::model::Tid,
) -> Option<u64> {
    if !valid_pid(process.get()) || !valid_pid(thread.get()) {
        return None;
    }
    stat_start_time(
        &read_path(format!(
            "/proc/{}/task/{}/stat",
            process.get(),
            thread.get()
        ))
        .ok()?,
    )
}

pub(super) fn process_exited(process: crate::model::Pid) -> bool {
    if !valid_pid(process.get()) {
        return true;
    }
    exited_record(read_path(format!("/proc/{}/stat", process.get())))
}

pub(super) fn gone(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(nix::libc::ESRCH)
}

fn exited_record(record: std::io::Result<Vec<u8>>) -> bool {
    match record {
        Ok(stat) => stat_exited(&stat),
        Err(error) => gone(&error),
    }
}

pub(super) fn tgid(status: &[u8]) -> Option<u32> {
    let value = u32::try_from(decimal(field(status, b"Tgid:")?.trim_ascii())?).ok()?;
    valid_pid(value).then_some(value)
}

/// The process that started the one a `/proc/PID/status` describes.
pub(super) fn parent_pid(status: &[u8]) -> Option<u32> {
    let value = u32::try_from(decimal(field(status, b"PPid:")?.trim_ascii())?).ok()?;
    valid_pid(value).then_some(value)
}

pub(super) fn tracer_pid(status: &[u8]) -> Option<u32> {
    let value = u32::try_from(decimal(field(status, b"TracerPid:")?.trim_ascii())?).ok()?;
    (value == 0 || valid_pid(value)).then_some(value)
}

pub(super) fn hex_field(status: &[u8], key: &[u8]) -> Option<u64> {
    let value = field(status, key)?.trim_ascii();
    if value.len() != 16 || !value.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    u64::from_str_radix(std::str::from_utf8(value).ok()?, 16).ok()
}

/// A thread asleep in a system call, as its `/proc/PID/task/TID/syscall`
/// record shows it: the system call's number and arguments, its stack
/// pointer, and its instruction pointer. Of another process's thread, the
/// kernel writes the record only while the thread is off its CPU and stays
/// off it while the record is made (`task_current_syscall`); a thread that
/// reads its own record finds the read it makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asleep {
    pub stack_pointer: u64,
    record: Vec<u8>,
}

#[cfg(test)]
impl Asleep {
    /// The thread a `syscall` record describes, if it is asleep in one.
    pub fn from_record(record: &[u8]) -> Option<Self> {
        asleep(record)
    }
}

/// The thread a `syscall` record describes, if it is asleep in a system
/// call: not `running`, nor blocked outside one (`-1 sp pc`).
pub(super) fn asleep(record: &[u8]) -> Option<Asleep> {
    let fields: Vec<&[u8]> = record.trim_ascii().split(|byte| *byte == b' ').collect();
    let [number, arguments @ .., stack_pointer, _instruction] = fields.as_slice() else {
        return None;
    };
    let hex = |field: &[u8]| {
        let digits = field.strip_prefix(b"0x")?;
        (!digits.is_empty() && digits.iter().all(u8::is_ascii_hexdigit))
            .then(|| u64::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok())
            .flatten()
    };
    decimal(number)?;
    if arguments.len() != 6 || arguments.iter().any(|argument| hex(argument).is_none()) {
        return None;
    }
    Some(Asleep {
        stack_pointer: hex(stack_pointer)?,
        record: record.to_vec(),
    })
}

/// How many times the thread a `status` record describes has given up its
/// CPU, of its own accord and not.
pub(super) fn context_switches(status: &[u8]) -> Option<(u64, u64)> {
    let count = |key: &[u8]| decimal(field(status, key)?.trim_ascii());
    Some((
        count(b"voluntary_ctxt_switches:")?,
        count(b"nonvoluntary_ctxt_switches:")?,
    ))
}

/// A thread's `syscall` and `status` files, opened once and read again
/// from the start for each record, which costs one system call where
/// opening the file by its path costs four. A `/proc` file holds the
/// thread's `struct pid`, not its number, and a read finds the task
/// through it (`proc_single_show`): once the thread is gone, every read
/// fails with `ESRCH`, even after a later thread is given its id. Both
/// files are opened through one descriptor of the thread's directory,
/// which holds the same, so they name the thread its start time does.
/// Its `schedstat`, the shortest record a thread has, tells whether the
/// thread is still there, and so whether the start time is still its own;
/// a kernel without `CONFIG_SCHED_INFO` has none, and `status` tells it.
#[derive(Debug)]
pub struct ThreadFiles {
    start: u64,
    syscall: std::fs::File,
    status: std::fs::File,
    schedstat: Option<std::fs::File>,

    /// Whether a read of them failed, as every read does once the thread
    /// is gone: they are of no use, and a later thread may have its id.
    failed: std::cell::Cell<bool>,
}

impl ThreadFiles {
    /// Opens the files of thread `thread` of process `process`.
    pub fn open(process: u32, thread: u32) -> std::io::Result<Self> {
        if !valid_pid(process) || !valid_pid(thread) {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
        }
        let directory = nix::fcntl::open(
            format!("/proc/{process}/task/{thread}").as_str(),
            OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW,
            Mode::empty(),
        )?;
        let file = |name: &str| -> std::io::Result<std::fs::File> {
            let fd = openat(
                &directory,
                name,
                OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
                Mode::empty(),
            )?;
            Ok(std::fs::File::from(fd))
        };
        let start = stat_start_time(&reread(&file("stat")?)?).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the thread's stat names no start time",
            )
        })?;
        Ok(Self {
            start,
            syscall: file("syscall")?,
            status: file("status")?,
            schedstat: file("schedstat").ok(),
            failed: std::cell::Cell::new(false),
        })
    }

    /// When the thread started, in clock ticks since boot.
    pub const fn start(&self) -> u64 {
        self.start
    }

    /// Whether a read of the files failed, which every read does once the
    /// thread is gone.
    const fn failed(&self) -> bool {
        self.failed.get()
    }

    /// Whether the thread is still there, so that a thread given its id
    /// later is not taken for it. One read of its shortest record.
    pub fn live(&self) -> bool {
        !self.failed()
            && self
                .reread(self.schedstat.as_ref().unwrap_or(&self.status))
                .is_ok()
    }

    /// The thread, if it is asleep in a system call.
    pub fn asleep(&self) -> std::io::Result<Option<Asleep>> {
        Ok(asleep(&self.reread(&self.syscall)?))
    }

    /// How many times the thread has given up its CPU, of its own accord
    /// and not.
    pub fn switches(&self) -> std::io::Result<(u64, u64)> {
        context_switches(&self.reread(&self.status)?)
            .ok_or_else(|| std::io::Error::other("the status names no context switches"))
    }

    fn reread(&self, file: &std::fs::File) -> std::io::Result<Vec<u8>> {
        reread(file).inspect_err(|_| self.failed.set(true))
    }
}

fn valid_pid(value: u32) -> bool {
    value != 0 && i32::try_from(value).is_ok()
}

pub(super) fn field<'a>(status: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let mut values = status
        .split(|byte| *byte == b'\n')
        .filter_map(|line| line.strip_prefix(key));
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

fn numbers(status: &[u8], key: &[u8]) -> Option<Vec<u32>> {
    let mut values = Vec::new();
    for word in field(status, key)?
        .split(u8::is_ascii_whitespace)
        .filter(|word| !word.is_empty())
    {
        if values.len() == MAX_ID_VALUES {
            return None;
        }
        values.push(std::str::from_utf8(word).ok()?.parse().ok()?);
    }
    (!values.is_empty()).then_some(values)
}

fn uid_pair(status: &[u8]) -> Option<(u32, u32)> {
    let values = numbers(status, b"Uid:")?;
    let [_, effective, _, filesystem] = values.as_slice() else {
        return None;
    };
    Some((*effective, *filesystem))
}

fn namespace_ids(status: &[u8]) -> Option<Vec<u32>> {
    let ids = numbers(status, b"NSpid:")?;
    ids.iter().all(|id| valid_pid(*id)).then_some(ids)
}

fn own_namespace(status: &[u8], own: u32) -> bool {
    namespace_ids(status).is_some_and(|ids| ids == [own])
        && numbers(status, b"Tgid:").is_some_and(|ids| ids == [own])
}

fn thread_in_status(status: &[u8], process: u32, thread: u32) -> Option<TargetThreadId> {
    if numbers(status, b"Tgid:")? != [process] {
        return None;
    }

    let ids = namespace_ids(status)?;
    if ids.first().copied()? != thread {
        return None;
    }

    TargetThreadId::new(u64::from(*ids.last()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_thread_in_a_system_call_is_asleep_in_one() {
        let record = b"7 0x7ffd1000 0x1 0x2 0x3 0x4 0x5 0x7ffd0fd8 0x7f00deadbeef\n";
        let asleep = asleep(record).expect("asleep in poll");
        assert_eq!(asleep.stack_pointer, 0x7ffd_0fd8);
        assert_eq!(asleep, super::asleep(record).unwrap(), "the same record");
        assert_ne!(
            Some(asleep),
            super::asleep(b"7 0x7ffd1000 0x1 0x2 0x3 0x4 0x6 0x7ffd0fd8 0x7f00deadbeef\n"),
            "another argument is another sleep"
        );

        assert_eq!(super::asleep(b"running\n"), None);
        assert_eq!(super::asleep(b"-1 0x7ffd0fd8 0x7f00deadbeef\n"), None);
        assert_eq!(
            super::asleep(b"7 0x1 0x2 0x7ffd0fd8 0x7f00deadbeef\n"),
            None
        );
        assert_eq!(
            super::asleep(b"7 0x1 0x2 0x3 0x4 0x5 0x6 sp 0x7f00deadbeef\n"),
            None
        );
        assert_eq!(super::asleep(b""), None);
    }

    #[test]
    fn a_status_says_how_often_its_thread_gave_up_its_cpu() {
        let status = b"Name:\tnode\nState:\tS (sleeping)\nvoluntary_ctxt_switches:\t12\nnonvoluntary_ctxt_switches:\t3\n";
        assert_eq!(context_switches(status), Some((12, 3)));
        assert_eq!(context_switches(b"voluntary_ctxt_switches:\t12\n"), None);
    }

    /// A thread that spins is on its CPU, or waits for it, and is in no
    /// system call; one blocked in a system call is asleep in it, and has
    /// given up its CPU.
    #[test]
    fn a_blocked_thread_is_asleep_in_its_system_call() {
        let process = std::process::id();
        let spin = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (started, spinning) = std::sync::mpsc::channel();
        let spinner = std::thread::spawn({
            let spin = std::sync::Arc::clone(&spin);
            move || {
                started
                    .send(u32::try_from(nix::unistd::gettid().as_raw()).unwrap())
                    .unwrap();
                while spin.load(std::sync::atomic::Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            }
        });
        let spinner_id = spinning
            .recv_timeout(std::time::Duration::from_secs(20))
            .unwrap();
        // It may still be leaving the channel's send; once it spins, it
        // stays in no system call.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let files = ThreadFiles::open(process, spinner_id).unwrap();
        for _ in 0..100 {
            assert_eq!(files.asleep().unwrap(), None, "spinning");
        }
        spin.store(false, std::sync::atomic::Ordering::Relaxed);
        spinner.join().unwrap();

        let (ready, parked) = std::sync::mpsc::channel();
        let (reader, writer) = nix::unistd::pipe().unwrap();
        let blocked = std::thread::spawn(move || {
            ready
                .send(u32::try_from(nix::unistd::gettid().as_raw()).unwrap())
                .unwrap();
            let mut byte = [0];
            while nix::unistd::read(&reader, &mut byte).unwrap() == 1 {}
        });
        let thread = parked
            .recv_timeout(std::time::Duration::from_secs(20))
            .unwrap();
        let files = ThreadFiles::open(process, thread).unwrap();
        let asleep = || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                if let Some(asleep) = files.asleep().unwrap() {
                    break asleep;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the thread never blocked"
                );
                std::thread::yield_now();
            }
        };
        let first = asleep();
        assert!(first.record.starts_with(b"0 "), "asleep in read: {first:?}");
        let switches = files.switches().unwrap();
        assert!(switches.0 >= 1, "{switches:?}");

        // Each read of the kept files is of the thread as it is now.
        nix::unistd::write(&writer, b"x").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while files.switches().unwrap() == switches {
            assert!(std::time::Instant::now() < deadline, "never counted");
            std::thread::yield_now();
        }
        assert_eq!(asleep(), first, "asleep in the same read again");
        assert!(!files.failed());

        // Once the thread is gone, so are its files.
        // A joined thread may still be leaving: until the kernel releases
        // it, its files read it as it exits.
        drop(writer);
        blocked.join().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while files.asleep().is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "the thread never went away"
            );
            std::thread::yield_now();
        }
        for error in [files.asleep().unwrap_err(), files.switches().unwrap_err()] {
            assert_eq!(error.raw_os_error(), Some(nix::libc::ESRCH), "{error}");
        }
        assert!(files.failed());
        assert!(ThreadFiles::open(process, thread).is_err());
    }

    /// Without `schedstat`, as a kernel without `CONFIG_SCHED_INFO`
    /// leaves it, a thread's files still say whether it is there.
    #[test]
    fn a_thread_is_known_gone_without_its_schedstat() {
        let process = std::process::id();
        let (ready, started) = std::sync::mpsc::channel();
        let (reader, writer) = nix::unistd::pipe().unwrap();
        let blocked = std::thread::spawn(move || {
            ready
                .send(u32::try_from(nix::unistd::gettid().as_raw()).unwrap())
                .unwrap();
            let mut byte = [0];
            let _ = nix::unistd::read(&reader, &mut byte);
        });
        let thread = started
            .recv_timeout(std::time::Duration::from_secs(20))
            .unwrap();
        let files = ThreadFiles {
            schedstat: None,
            ..ThreadFiles::open(process, thread).unwrap()
        };
        assert!(files.live());

        drop(writer);
        blocked.join().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while files.live() {
            assert!(
                std::time::Instant::now() < deadline,
                "a thread gone is still live"
            );
            std::thread::yield_now();
        }
    }

    /// A record longer than the first buffer is read whole.
    #[test]
    fn a_record_is_reread_whole_whatever_its_length() {
        let status = std::fs::File::open("/proc/self/status").unwrap();
        let whole = read_path("/proc/self/status").unwrap();
        let again = reread(&status).unwrap();
        assert_eq!(field(&again, b"Name:"), field(&whole, b"Name:"));
        assert!(again.ends_with(b"\n"));

        let mut long = tempfile_with(&vec![b'x'; RECORD_BYTES * 3 + 5]);
        assert_eq!(reread(&long).unwrap().len(), RECORD_BYTES * 3 + 5);
        long = tempfile_with(&vec![b'x'; RECORD_BYTES]);
        assert_eq!(reread(&long).unwrap().len(), RECORD_BYTES);
        let huge = tempfile_with(&vec![b'9'; MAX_STATUS_BYTES + 1]);
        assert!(reread(&huge).unwrap().is_empty());
    }

    fn tempfile_with(bytes: &[u8]) -> std::fs::File {
        use std::io::Write as _;
        let path = std::env::temp_dir().join(format!(
            "microscope-reread-{}-{}",
            std::process::id(),
            bytes.len()
        ));
        let mut file = std::fs::File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        file.write_all(bytes).unwrap();
        file
    }

    #[test]
    fn unavailable_proc_metadata_does_not_prove_process_exit() {
        assert!(
            exited_record(Err(std::io::Error::from_raw_os_error(nix::libc::ESRCH))),
            "an opened proc inode can outlive its process"
        );
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::Interrupted,
            std::io::ErrorKind::UnexpectedEof,
        ] {
            assert!(!exited_record(Err(std::io::Error::from(kind))), "{kind:?}");
        }
        assert!(exited_record(Err(std::io::Error::from(
            std::io::ErrorKind::NotFound
        ))));
    }

    fn stat_record(comm: &[u8], flags: u64, start: &str) -> Vec<u8> {
        let mut record = b"42 (".to_vec();
        record.extend_from_slice(comm);
        record.extend_from_slice(b") R 1 42 42 0 -1 ");
        record.extend_from_slice(format!("{flags} ").as_bytes());
        record.extend_from_slice(b"0 0 0 0 0 0 0 0 0 0 0 0 ");
        record.extend_from_slice(start.as_bytes());
        record.push(b' ');
        record
    }

    #[test]
    fn stat_fields_reject_malformed_headers_and_incomplete_numbers() {
        let good = stat_record(b"name", 0, "123");
        assert_eq!(stat_start_time(&good), Some(123));

        for header in [
            "",
            "0 (name)",
            "-1 (name)",
            "2147483648 (name)",
            "42 name",
            "42(name)",
            "42 (name)",
        ] {
            let mut record = header.as_bytes().to_vec();
            record.extend_from_slice(b"R 1 42 42 0 -1 0 ");
            assert_eq!(stat_state(&record), None, "{header}");
        }

        for start in ["", "+1", "-1", "12x", "18446744073709551616"] {
            assert_eq!(
                stat_start_time(&stat_record(b"name", 0, start)),
                None,
                "{start}"
            );
        }

        assert_eq!(stat_start_time(&good[..good.len() - 1]), None);
        assert_eq!(stat_state(&good[..good.len() - 1]), Some(b'R'));
        assert_eq!(stat_state(b"42 (name) RR "), None);
        assert_eq!(stat_state(b"42 (name) 0 "), None);
    }

    #[test]
    fn status_numbers_require_one_bounded_complete_field() {
        let status = b"Name: \xff\xfe\xfd\nTgid: 42\nTracerPid: 0\nCapEff: ffffffffffffffff\n";
        assert_eq!(tgid(status), Some(42));
        assert_eq!(tracer_pid(status), Some(0));
        assert_eq!(hex_field(status, b"CapEff:"), Some(u64::MAX));

        for number in [
            "",
            "0",
            "+1",
            "-1",
            "2147483648",
            "4294967296",
            "4 2",
            "42x",
        ] {
            assert_eq!(
                tgid(format!("Tgid: {number}\n").as_bytes()),
                None,
                "{number}"
            );
        }

        assert_eq!(tgid(b"Tgid: 42\nTgid: 42\n"), None);
        assert_eq!(tracer_pid(b"TracerPid: 2147483648\n"), None);

        for value in [
            "",
            "1",
            "+000000000000001",
            "000000000000000g",
            "00000000000000000",
        ] {
            assert_eq!(
                hex_field(format!("SigBlk: {value}\n").as_bytes(), b"SigBlk:"),
                None,
                "{value}"
            );
        }

        assert_eq!(
            hex_field(
                b"SigBlk: 0000000000000001\nSigBlk: 0000000000000001\n",
                b"SigBlk:"
            ),
            None
        );
        assert_eq!(
            hex_field(b"SigBlk: 0000000000000001\n", b"SigBlk:"),
            Some(1)
        );
    }

    proptest::proptest! {
        #[test]
        fn arbitrary_command_bytes_do_not_change_numeric_stat_fields(
            comm in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..64),
            start in proptest::prelude::any::<u64>(),
            flags in proptest::prelude::any::<u64>(),
        ) {
            let record = stat_record(&comm, flags, &start.to_string());
            proptest::prop_assert_eq!(stat_start_time(&record), Some(start));
            proptest::prop_assert_eq!(stat_state(&record), Some(b'R'));
            proptest::prop_assert_eq!(stat_exited(&record), flags & 4 != 0);
        }
    }

    /// A process is exited once it begins to: its memory may be gone
    /// before it is a zombie.
    #[test]
    fn a_process_that_began_to_exit_has_exited() {
        // State, parent, group, session, terminal, foreground group, flags.
        let running = "42 (a (b)) R 1 42 42 0 -1 4194304 0 0";
        let exiting = "42 (a (b)) R 1 42 42 0 -1 4194308 0 0";
        let zombie = "42 (a (b)) Z 1 42 42 0 -1 4194308 0 0";
        assert!(!stat_exited(running.as_bytes()));
        assert!(stat_exited(exiting.as_bytes()));
        assert!(stat_exited(zombie.as_bytes()));
    }

    #[test]
    fn namespaces_match_the_controlled_process_and_thread_before_using_the_innermost_id() {
        for ids in ["5001", "5001 7", "5001 5001 7"] {
            let status = format!("Tgid:\t5000\nNSpid:\t{ids}\n");
            let expected = if ids == "5001" { 5001 } else { 7 };
            assert_eq!(
                thread_in_status(status.as_bytes(), 5000, 5001)
                    .unwrap()
                    .get(),
                expected
            );
            assert_eq!(thread_in_status(status.as_bytes(), 4999, 5001), None);
            assert_eq!(thread_in_status(status.as_bytes(), 5000, 7), None);
        }

        assert!(own_namespace(b"Tgid: 7\nNSpid: 7\n", 7));
        assert!(
            !own_namespace(b"Tgid: 7\nNSpid: 7 7\n", 7),
            "equal numbers in two namespaces are still different views"
        );
        assert!(!own_namespace(b"Tgid: 5000\nNSpid: 5000 7\n", 7));
    }

    #[test]
    fn malformed_or_missing_identity_fields_are_not_guessed() {
        for status in [
            "",
            "Tgid: 5000",
            "Tgid: 5000\nNSpid:",
            "Tgid: 5000\nNSpid: 0",
            "Tgid: 5000\nNSpid: 5001 -7",
            "Tgid: 5000\nNSpid: 5001 2147483648",
            "Tgid: 5000\nNSpid: 5001 X 7",
            "Tgid: 5000\nNSpid: 5001\nNSpid: 5001",
            "Tgid: 5000\nTgid: 5000\nNSpid: 5001",
        ] {
            assert_eq!(
                thread_in_status(status.as_bytes(), 5000, 5001),
                None,
                "{status:?}"
            );
        }

        let excessive = format!(
            "Tgid: 5000\nNSpid: {}",
            ["5001"; MAX_ID_VALUES + 1].join(" ")
        );
        assert_eq!(thread_in_status(excessive.as_bytes(), 5000, 5001), None);
        assert_eq!(TargetThreadId::new(0), None);
    }

    #[test]
    fn identity_decoding_does_not_require_a_utf8_thread_name() {
        let status = b"Name:\temoji_\xff\nTgid:\t5000\nNSpid:\t5001 7\nUid:\t0 1000 0 1000\n";
        assert_eq!(thread_in_status(status, 5000, 5001).unwrap().get(), 7);
        assert_eq!(uid_pair(status), Some((1000, 1000)));
    }

    #[test]
    fn effective_and_filesystem_uids_require_one_complete_record() {
        assert_eq!(uid_pair(b"Uid: 0 1000 0 1000"), Some((1000, 1000)));
        assert_eq!(uid_pair(b"Uid: 1000 1000 0 2000"), Some((1000, 2000)));
        for status in [
            "",
            "Uid:",
            "Uid: 0 1000 0",
            "Uid: 0 1000 0 1000 0",
            "Uid: 0 -1 0 1000",
            "Uid: 0 4294967296 0 1000",
            "Uid: 0 X 0 1000",
            "Uid: 0 1000 0 1000\nUid: 0 1000 0 1000",
        ] {
            assert_eq!(uid_pair(status.as_bytes()), None, "{status:?}");
        }
    }

    #[test]
    fn bounded_reads_discard_incomplete_identity_fields() {
        let prefix = b"Tgid: 5000\nNSpid: 5001 7\n";
        let mut input = prefix.to_vec();
        input.resize(MAX_STATUS_BYTES - 6, b' ');
        input.extend_from_slice(b"\nUid: 0 1000 0 1000\n");
        let bytes = read(std::io::Cursor::new(input)).unwrap();
        assert_eq!(thread_in_status(&bytes, 5000, 5001).unwrap().get(), 7);
        assert_eq!(uid_pair(&bytes), None);
        assert!(bytes.len() <= MAX_STATUS_BYTES);

        let huge = vec![b'9'; MAX_STATUS_BYTES + 1];
        assert!(read(std::io::Cursor::new(huge)).unwrap().is_empty());
    }

    proptest::proptest! {
        #[test]
        fn namespace_hierarchies_round_trip(
            process in 1u32..=i32::MAX as u32,
            ids in proptest::collection::vec(1u32..=i32::MAX as u32, 1..=MAX_ID_VALUES),
        ) {
            let status = format!("Tgid: {process}\nNSpid: {}\n", ids.iter().map(u32::to_string).collect::<Vec<_>>().join(" "));
            proptest::prop_assert_eq!(thread_in_status(status.as_bytes(), process, ids[0]).map(TargetThreadId::get), ids.last().copied().map(u64::from));
        }
    }

    #[test]
    fn the_native_reader_proves_the_proc_view_or_returns_unavailable() {
        let process = std::process::id();
        let thread = u32::try_from(nix::unistd::gettid().as_raw()).unwrap();
        let status = read(std::fs::File::open("/proc/self/status").unwrap()).unwrap();
        if own_namespace(&status, process) {
            assert_eq!(
                target_thread_id(process, thread).unwrap().get(),
                u64::from(thread)
            );
        } else {
            assert_eq!(target_thread_id(process, thread), None);
        }

        assert_eq!(target_thread_id(process.saturating_add(1), thread), None);

        let batch = target_thread_ids(process, &[0, thread, i32::MAX as u32]);
        if own_namespace(&status, process) {
            assert_eq!(
                batch,
                [(thread, TargetThreadId::new(u64::from(thread)).unwrap())]
            );
        } else {
            assert!(batch.is_empty());
        }

        assert_eq!(target_thread_id(process, 0), None);
        assert_eq!(target_thread_id(0, thread), None);
    }
}
