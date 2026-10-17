//! Bounded identity fields from kernel proc status records. Names need not
//! be UTF-8; only the numeric fields are decoded.

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
