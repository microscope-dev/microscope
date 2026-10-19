//! Numeric fields from kernel stat/status records. Command names need not
//! be UTF-8; only the selected numeric fields are decoded.

use std::io::Read as _;
use std::os::fd::OwnedFd;

use nix::fcntl::{OFlag, openat};
use nix::sys::stat::Mode;

use crate::runtime_model::TargetThreadId;

const MAX_STATUS_BYTES: usize = 64 * 1024;
const MAX_ID_VALUES: usize = 64;

fn read(file: impl std::io::Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take((MAX_STATUS_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;

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

    Ok(bytes)
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

fn valid_pid(value: u32) -> bool {
    value != 0 && i32::try_from(value).is_ok()
}

fn field<'a>(status: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
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
