//! Incremental reader for `/tmp/perf-<pid>.map` JIT symbol maps.
//!
//! Resolves the target's inner-namespace PID via `/proc/<pid>/status` (`NSpid`) and rejects
//! stale map files whose creation time predates the target process's start time.

use std::io::{Read as _, Seek as _};
use std::path::PathBuf;

use super::decode;

pub fn is_anonymous_code(mmap: &decode::Mmap<'_>) -> bool {
    mmap.executable
        && (mmap.path.is_empty() || mmap.path == b"//anon" || mmap.path.starts_with(b"[anon:"))
}

#[derive(Debug)]
pub enum MapError {
    Absent,

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

pub fn read_map(pid: u32, from: u64, limit: u64) -> Result<(String, Vec<u8>), MapError> {
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
        .and_then(|_| file.take(limit).read_to_end(&mut bytes))
        .map_err(MapError::Unreadable)?;

    Ok((named, bytes))
}

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

fn began(pid: u32) -> Option<std::time::SystemTime> {
    use nix::time::{ClockId, clock_gettime};

    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;

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
    fn a_process_began_when_it_was_spawned() {
        let slack = std::time::Duration::from_millis(50);
        let before = std::time::SystemTime::now();
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawn sleep");
        let after = std::time::SystemTime::now();
        let began = began(child.id());
        child.kill().expect("kill sleep");
        child.wait().expect("reap sleep");

        let began = began.expect("its start");
        assert!(
            before - slack <= began && began <= after + slack,
            "{began:?} not within {before:?}..{after:?}"
        );
    }

    #[test]
    fn a_map_made_after_its_process_began_is_read_past_where_asked() {
        let pid = std::process::id();
        let own = own_pid(pid).expect("its own pid");
        let path = format!("/tmp/perf-{own}.map");
        if std::path::Path::new(&path).exists() {
            assert!(matches!(read_map(pid, 0, u64::MAX), Err(MapError::Stale)));
            if std::fs::remove_file(&path).is_err() {
                return;
            }
        }

        std::fs::write(&path, b"1000 10 first\n2000 1").expect("write the map");

        let read = read_map(pid, 5, u64::MAX);
        std::fs::remove_file(&path).expect("remove the map");
        let (named, bytes) = read.expect("a map made after the process began");
        assert_eq!(named, path);
        assert_eq!(bytes, b"10 first\n2000 1");
        assert!(matches!(read_map(pid, 0, u64::MAX), Err(MapError::Absent)));
    }
}
