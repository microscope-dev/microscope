//! The perf maps just-in-time compilers write for the code they generate,
//! read from outside the process that writes one: `/tmp/perf-PID.map` in
//! its own root, named by the pid its own namespace knows it by.

use std::io::{Read as _, Seek as _};
use std::path::PathBuf;

use super::decode;

/// Whether a mapping holds code no file holds: anonymous executable
/// memory, where a compiler writes the code it generates.
pub fn is_anonymous_code(mmap: &decode::Mmap<'_>) -> bool {
    mmap.executable
        && (mmap.path.is_empty() || mmap.path == b"//anon" || mmap.path.starts_with(b"[anon:"))
}

/// Why a process's perf map is not read.
#[derive(Debug)]
pub enum MapError {
    /// The process wrote none, or has exited.
    Absent,

    /// The file was made before the process began: an earlier process
    /// with its pid wrote it, and what it names is not this one's code.
    /// CPython appends to a map it finds and never deletes its own.
    Stale,
    Unreadable(std::io::Error),
}

impl std::fmt::Display for MapError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Absent => formatter.write_str("the process wrote none"),
            Self::Stale => formatter.write_str(
                "it was made before the process began, by an earlier process with its pid",
            ),
            Self::Unreadable(error) => write!(formatter, "it cannot be read: {error}"),
        }
    }
}

/// The path process `pid` names its perf map by, and the bytes of the
/// map past its first `from`.
pub fn read_map(pid: u32, from: u64) -> Result<(String, Vec<u8>), MapError> {
    let own = own_pid(pid).ok_or(MapError::Absent)?;
    let named = format!("/tmp/perf-{own}.map");
    let path = PathBuf::from(format!("/proc/{pid}/root{named}"));
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(MapError::Absent);
        }
        Err(error) => return Err(MapError::Unreadable(error)),
    };

    let made = file
        .metadata()
        .and_then(|metadata| metadata.created())
        .map_err(MapError::Unreadable)?;
    let began = began(pid).ok_or(MapError::Absent)?;
    if made < began {
        return Err(MapError::Stale);
    }

    let mut bytes = Vec::new();
    file.seek(std::io::SeekFrom::Start(from))
        .and_then(|_| file.read_to_end(&mut bytes))
        .map_err(MapError::Unreadable)?;
    Ok((named, bytes))
}

/// The pid process `pid` has in its own namespace.
fn own_pid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))?
        .split_whitespace()
        .last()?
        .parse()
        .ok()
}

/// When process `pid` began, as of the clock tick it began in.
fn began(pid: u32) -> Option<std::time::SystemTime> {
    use nix::time::{ClockId, clock_gettime};

    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;

    // Its fields after the name, which may hold anything but ends with
    // the last ')': the 22nd, the start, is the 20th after it.
    let ticks: u64 = stat
        .get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()?;
    let per_second = nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK)
        .ok()
        .flatten()
        .and_then(|ticks| u64::try_from(ticks).ok())
        .filter(|ticks| *ticks > 0)?;
    let since_boot = std::time::Duration::from_secs(ticks / per_second)
        + std::time::Duration::from_nanos((ticks % per_second) * 1_000_000_000 / per_second);

    // The wall clock's time at boot, from both clocks now.
    let duration = |clock| {
        let time = clock_gettime(clock).ok()?;
        Some(
            std::time::Duration::from_secs(u64::try_from(time.tv_sec()).ok()?)
                + std::time::Duration::from_nanos(u64::try_from(time.tv_nsec()).ok()?),
        )
    };
    let now = duration(ClockId::CLOCK_REALTIME)?;
    let boot = now.checked_sub(duration(ClockId::CLOCK_BOOTTIME)?)?;
    std::time::UNIX_EPOCH.checked_add(boot + since_boot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mmap(path: &[u8], executable: bool) -> decode::Mmap<'_> {
        decode::Mmap {
            pid: crate::model::Pid::new(1),
            start: 0x1000,
            length: 0x1000,
            file_offset: 0,
            inode: None,
            executable,
            path,
            time: 0,
        }
    }

    #[test]
    fn anonymous_code_is_executable_memory_no_file_holds() {
        assert!(is_anonymous_code(&mmap(b"//anon", true)));
        assert!(is_anonymous_code(&mmap(b"", true)));
        assert!(is_anonymous_code(&mmap(b"[anon:jit]", true)));
        assert!(!is_anonymous_code(&mmap(b"//anon", false)));
        assert!(!is_anonymous_code(&mmap(b"/usr/lib/libc.so.6", true)));
        assert!(!is_anonymous_code(&mmap(b"[vdso]", true)));
    }

    #[test]
    fn this_process_began_before_now_and_after_boot() {
        let began = began(std::process::id()).expect("its start");
        let now = std::time::SystemTime::now();
        assert!(began <= now);
        assert!(
            now.duration_since(began).expect("ordered") < std::time::Duration::from_hours(24 * 365)
        );
    }

    #[test]
    fn a_map_made_after_its_process_began_is_read_past_where_asked() {
        let pid = std::process::id();
        let own = own_pid(pid).expect("its own pid");
        let path = format!("/tmp/perf-{own}.map");
        if std::path::Path::new(&path).exists() {
            return;
        }

        std::fs::write(&path, b"1000 10 first\n2000 1").expect("write the map");
        let read = read_map(pid, 5);
        std::fs::remove_file(&path).expect("remove the map");
        let (named, bytes) = read.expect("a map made after the process began");
        assert_eq!(named, path);
        assert_eq!(bytes, b"10 first\n2000 1");
        assert!(matches!(read_map(pid, 0), Err(MapError::Absent)));
    }
}
