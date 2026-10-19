//! What each process's descriptors refer to, which `-y` prints beside
//! each descriptor.
//!
//! A table starts from what the engine reads in `/proc`
//! and follows the calls that open, duplicate, and close descriptors.
//! A descriptor whose origin the trace did not see is unknown, never a
//! guess, and a gap in the trace marks the process's table stale until it
//! is read again.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::model::{Outcome, Pid, SyscallEvent, TraceEvent};
use super::sockaddr::{self, SocketAddress};

/// What one descriptor refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description {
    /// The text `-y` prints between `<` and `>`: a path, `pipe:[88213]`,
    /// or `TCP:[10.0.3.4:51022->10.0.3.9:5432]`.
    pub text: Arc<str>,

    /// What kind it is, for the files and network views.
    pub kind: Kind,

    /// Closed by a successful `execve`.
    pub cloexec: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    File,
    Socket(Socket),
    Pipe,

    /// A character or block device, which `-yy` numbers.
    Device(super::model::Device),
    Other,
}

/// What the trace has seen of a socket.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Socket {
    pub family: u16,

    /// `SOCK_STREAM` or the like, without its flags.
    pub kind: u32,
    pub local: Option<String>,
    pub remote: Option<String>,
}

impl Socket {
    /// strace's name for the protocol, as in `TCP:[...]`.
    const fn protocol(&self) -> &'static str {
        const STREAM: u32 = 1;
        const DGRAM: u32 = 2;
        match (self.family, self.kind) {
            (sockaddr::AF_INET, STREAM) => "TCP",
            (sockaddr::AF_INET, DGRAM) => "UDP",
            (sockaddr::AF_INET6, STREAM) => "TCPv6",
            (sockaddr::AF_INET6, DGRAM) => "UDPv6",
            (sockaddr::AF_UNIX, _) => "UNIX",
            (sockaddr::AF_NETLINK, _) => "NETLINK",
            _ => "socket",
        }
    }

    fn text(&self) -> String {
        match (&self.local, &self.remote) {
            (None, None) => format!("{}:[]", self.protocol()),
            (local, remote) => format!(
                "{}:[{}{}{}]",
                self.protocol(),
                local.as_deref().unwrap_or(""),
                if remote.is_some() { "->" } else { "" },
                remote.as_deref().unwrap_or("")
            ),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Process {
    fds: BTreeMap<i64, Description>,

    /// Descriptors the engine read just before the call that made them,
    /// whose reading the call's own arguments must not replace, with what
    /// each replaced.
    fresh: BTreeMap<i64, (Description, Option<Description>)>,

    /// A gap may have hidden calls that changed the table.
    stale: bool,

    /// Whether every descriptor was read from the system, or inherited
    /// from a process whose were.
    read: bool,
}

/// Every traced process's descriptors.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FdTable {
    processes: BTreeMap<Pid, Process>,
}

/// The calls the table follows, which an engine stops at whenever `-y`
/// needs the table, whether or not the selection shows them.
pub const FOLLOWED: &[&str] = &[
    "open",
    "creat",
    "openat",
    "openat2",
    "open_by_handle_at",
    "open_tree",
    "fspick",
    "fsopen",
    "fsmount",
    "close",
    "close_range",
    "dup",
    "dup2",
    "dup3",
    "fcntl",
    "socket",
    "connect",
    "bind",
    "accept",
    "accept4",
    "pipe",
    "pipe2",
    "socketpair",
    "eventfd",
    "eventfd2",
    "epoll_create",
    "epoll_create1",
    "timerfd_create",
    "signalfd",
    "signalfd4",
    "inotify_init",
    "inotify_init1",
    "memfd_create",
    "pidfd_open",
    "io_uring_setup",
    "userfaultfd",
    "perf_event_open",
    "fanotify_init",
    "landlock_create_ruleset",
    "memfd_secret",
    "pidfd_getfd",
    "chdir",
    "fchdir",
    "execve",
    "execveat",
];

/// [`FOLLOWED`] under every ABI that has them.
#[must_use]
pub fn followed() -> std::collections::BTreeSet<super::model::SyscallId> {
    super::model::Abi::ALL
        .iter()
        .flat_map(|&abi| {
            FOLLOWED
                .iter()
                .filter_map(move |name| super::model::SyscallId::named(abi, name))
        })
        .collect()
}

const O_CLOEXEC: u64 = 0o2_000_000;
const SOCK_CLOEXEC: u64 = 0o2_000_000;
const F_DUPFD: u64 = 0;
const F_DUPFD_CLOEXEC: u64 = 1030;
const F_SETFD: u64 = 2;
const FD_CLOEXEC: u64 = 1;
const CLOSE_RANGE_CLOEXEC: u64 = 4;
const EINPROGRESS: u16 = 115;
const AT_FDCWD: i64 = -100;

/// A path a call named, made absolute against the directory it is
/// relative to when the table knows it. `.` components go; `..` stay,
/// since a symbolic link may stand before one.
fn resolve(process: &Process, dirfd: i64, path: &str) -> String {
    if path.starts_with('/') {
        return path.to_owned();
    }

    let Some(base) = process
        .fds
        .get(&dirfd)
        .map(|description| description.text.clone())
        .filter(|text| text.starts_with('/'))
    else {
        return path.to_owned();
    };

    let mut joined = base.trim_end_matches('/').to_owned();

    for part in path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
    {
        joined.push('/');
        joined.push_str(part);
    }

    if joined.is_empty() {
        joined.push('/');
    }

    joined
}

impl FdTable {
    /// Records what the engine read from `/proc` for a process.
    pub fn seed(&mut self, process: Pid, fds: impl IntoIterator<Item = (i64, Description)>) {
        self.processes.insert(
            process,
            Process {
                fds: fds.into_iter().collect(),
                read: true,
                ..Process::default()
            },
        );
    }

    /// Records one descriptor, as an engine that read `/proc/PID/fd/N`
    /// right after the call made it does.
    pub fn set(&mut self, process: Pid, fd: i64, description: Description) {
        self.processes
            .entry(process)
            .or_default()
            .fds
            .insert(fd, description);
    }

    /// A path a call of `process` named, made absolute against `dirfd`
    /// (`AT_FDCWD` for the working directory) when the table knows it.
    #[must_use]
    pub fn resolve(&self, process: Pid, dirfd: i64, path: &str) -> String {
        self.processes
            .get(&process)
            .map_or_else(|| path.to_owned(), |known| resolve(known, dirfd, path))
    }

    /// What a descriptor refers to, when the trace knows.
    #[must_use]
    pub fn describe(&self, process: Pid, fd: i64) -> Option<&Description> {
        self.processes.get(&process)?.fds.get(&fd)
    }

    /// Whether the table holds every descriptor of a process, so that one
    /// it lacks is not open: true once the process was read from the
    /// system or inherited such a table, until a gap.
    #[must_use]
    pub fn knows(&self, process: Pid) -> bool {
        self.processes
            .get(&process)
            .is_some_and(|process| process.read && !process.stale)
    }

    /// What a descriptor referred to before the call about to be applied,
    /// which the engine already read the descriptors it made for.
    #[must_use]
    pub fn describe_before(&self, process: Pid, fd: i64) -> Option<&Description> {
        let process = self.processes.get(&process)?;
        match process.fresh.get(&fd) {
            Some((_, replaced)) => replaced.as_ref(),
            None => process.fds.get(&fd),
        }
    }

    /// Processes whose tables a gap made stale, to read again.
    pub fn stale(&self) -> impl Iterator<Item = Pid> + '_ {
        self.processes
            .iter()
            .filter(|(_, process)| process.stale)
            .map(|(&pid, _)| pid)
    }

    /// Every descriptor of a process, in order.
    pub fn of(&self, process: Pid) -> impl Iterator<Item = (i64, &Description)> {
        self.processes
            .get(&process)
            .into_iter()
            .flat_map(|process| {
                process
                    .fds
                    .range(0..)
                    .map(|(&fd, description)| (fd, description))
            })
    }

    /// Follows what a call did to its process's descriptors.
    pub fn apply_call(&mut self, call: &SyscallEvent) {
        let fresh = self
            .processes
            .get_mut(&call.process)
            .map(|process| std::mem::take(&mut process.fresh))
            .unwrap_or_default();
        self.call(call);

        if let Some(process) = self.processes.get_mut(&call.process) {
            for (fd, (read, _)) in fresh {
                let Some(made) = process.fds.get_mut(&fd) else {
                    continue;
                };

                // A socket's call knows its family and endpoints,
                // which its link does not.
                if matches!(made.kind, Kind::Socket(_)) {
                    continue;
                }

                if read.text.is_empty() {
                    // The engine read what it is, not its path.
                    made.cloexec = read.cloexec;
                    if let Kind::Device(device) = read.kind {
                        made.kind = Kind::Device(device);
                    }
                } else {
                    *made = read;
                }
            }
        }
    }

    pub fn apply(&mut self, event: &TraceEvent) {
        match event {
            TraceEvent::Call(call) => self.apply_call(call),
            TraceEvent::Spawn {
                parent,
                child,
                thread: false,
                ..
            } => {
                let copy = self.processes.get(parent).cloned().unwrap_or_default();
                self.processes.insert(Pid::led_by(*child), copy);
            }
            TraceEvent::Exec { process, .. } => {
                if let Some(table) = self.processes.get_mut(process) {
                    table.fds.retain(|_, description| !description.cloexec);
                }
            }
            TraceEvent::Exit { process, .. } => {
                self.processes.remove(process);
            }
            TraceEvent::Lost { .. } => {
                for process in self.processes.values_mut() {
                    process.stale = true;
                }
            }

            TraceEvent::Descriptor {
                process,
                fd,
                text,
                cloexec,
                device,
                made,
                ..
            } => self.descriptor(*process, *fd, text, *cloexec, *device, *made),
            TraceEvent::Spawn { .. }
            | TraceEvent::Signal { .. }
            | TraceEvent::ThreadExit { .. } => {}
        }
    }

    /// Records what the engine read of one descriptor.
    fn descriptor(
        &mut self,
        process: Pid,
        fd: i64,
        text: &Arc<str>,
        cloexec: bool,
        device: Option<super::model::Device>,
        made: bool,
    ) {
        let table = self.processes.entry(process).or_default();

        // A socket's link names only its inode; what the trace saw
        // of its endpoints says more.
        let known_socket = matches!(
            table.fds.get(&fd),
            Some(Description {
                kind: Kind::Socket(_),
                ..
            })
        );
        if known_socket && text.starts_with("socket:") {
            if let Some(description) = table.fds.get_mut(&fd) {
                description.cloexec = cloexec;
            }

            return;
        }

        let kind = device.map_or_else(
            || {
                if text.starts_with("pipe:") {
                    Kind::Pipe
                } else if text.starts_with("socket:") {
                    Kind::Socket(Socket::default())
                } else if text.starts_with('/') {
                    Kind::File
                } else {
                    Kind::Other
                }
            },
            Kind::Device,
        );
        let description = Description {
            text: text.clone(),
            kind,
            cloexec,
        };

        let replaced = table.fds.insert(fd, description.clone());
        if made {
            table.fresh.insert(fd, (description, replaced));
        } else {
            // The engine reads a process's descriptors together.
            table.read = true;
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per call that changes descriptors"
    )]
    fn call(&mut self, event: &SyscallEvent) {
        let Some(name) = event.call.name() else {
            return;
        };

        // A non-blocking connect that is still in progress has its peer.
        let in_progress =
            name == "connect" && event.outcome == Outcome::Failed(super::model::Errno(EINPROGRESS));
        let result = match event.outcome {
            Outcome::Returned(result) => result,
            _ if in_progress => 0,
            _ => return,
        };

        let process = self.processes.entry(event.process).or_default();
        let a = event.args;
        let fd = |word: u64| super::filter::fd_value(word);
        let string = |arg: u8| {
            event.capture(arg).map(|capture| {
                String::from_utf8_lossy(capture.bytes.split(|&b| b == 0).next().unwrap_or_default())
                    .into_owned()
            })
        };

        let file =
            |process: &Process, dirfd: i64, path: Option<String>, cloexec: bool| Description {
                text: path
                    .map_or_else(|| "?".to_owned(), |path| resolve(process, dirfd, &path))
                    .into(),
                kind: Kind::File,
                cloexec,
            };
        let anonymous = |text: &str, cloexec: bool| Description {
            text: text.into(),
            kind: Kind::Other,
            cloexec,
        };

        match name {
            "open" | "creat" => {
                let cloexec = name == "open" && a[1] & O_CLOEXEC != 0;
                let made = file(process, AT_FDCWD, string(0), cloexec);
                process.fds.insert(result, made);
            }
            "openat" => {
                let made = file(process, fd(a[0]), string(1), a[2] & O_CLOEXEC != 0);
                process.fds.insert(result, made);
            }
            "openat2" | "open_by_handle_at" | "open_tree" | "fspick" | "fsopen" | "fsmount" => {
                let path = if name == "openat2" || name == "open_tree" || name == "fspick" {
                    string(1)
                } else {
                    None
                };

                let made = file(process, fd(a[0]), path, false);
                process.fds.insert(result, made);
            }
            "chdir" => {
                if let Some(path) = string(0) {
                    let directory = resolve(process, AT_FDCWD, &path);
                    process
                        .fds
                        .insert(AT_FDCWD, file(process, AT_FDCWD, Some(directory), false));
                }
            }
            "fchdir" => match process.fds.get(&fd(a[0])).cloned() {
                Some(directory) => {
                    process.fds.insert(AT_FDCWD, directory);
                }
                None => {
                    process.fds.remove(&AT_FDCWD);
                }
            },
            "close" => {
                process.fds.remove(&fd(a[0]));
            }
            "close_range" => {
                let (first, last) = (a[0], a[1]);
                if a[2] & CLOSE_RANGE_CLOEXEC != 0 {
                    for (&number, description) in &mut process.fds {
                        if (first..=last).contains(&number.unsigned_abs()) {
                            description.cloexec = true;
                        }
                    }
                } else {
                    process
                        .fds
                        .retain(|&number, _| !(first..=last).contains(&number.unsigned_abs()));
                }
            }
            "dup" | "dup2" | "dup3" => {
                let copy = process.fds.get(&fd(a[0])).cloned();
                let cloexec = name == "dup3" && a[2] & O_CLOEXEC != 0;

                match copy {
                    Some(description) => {
                        process.fds.insert(
                            result,
                            Description {
                                cloexec,
                                ..description
                            },
                        );
                    }
                    None => {
                        process.fds.remove(&result);
                    }
                }
            }
            "fcntl" if a[1] == F_DUPFD || a[1] == F_DUPFD_CLOEXEC => {
                let copy = process.fds.get(&fd(a[0])).cloned();
                if let Some(description) = copy {
                    process.fds.insert(
                        result,
                        Description {
                            cloexec: a[1] == F_DUPFD_CLOEXEC,
                            ..description
                        },
                    );
                }
            }
            "fcntl" if a[1] == F_SETFD => {
                if let Some(description) = process.fds.get_mut(&fd(a[0])) {
                    description.cloexec = a[2] & FD_CLOEXEC != 0;
                }
            }
            "socket" => {
                #[expect(clippy::cast_possible_truncation, reason = "the kernel reads ints")]
                let socket = Socket {
                    family: a[0] as u16,
                    kind: (a[1] & 0xf) as u32,
                    ..Socket::default()
                };

                process.fds.insert(
                    result,
                    Description {
                        text: socket.text().into(),
                        kind: Kind::Socket(socket),
                        cloexec: a[1] & SOCK_CLOEXEC != 0,
                    },
                );
            }
            "connect" | "bind" => Self::address(process, fd(a[0]), event, 1, name == "connect"),
            "accept" | "accept4" => {
                let listener = process.fds.get(&fd(a[0])).cloned();
                let mut socket = match listener.map(|description| description.kind) {
                    Some(Kind::Socket(socket)) => Socket {
                        remote: None,
                        ..socket
                    },
                    _ => Socket::default(),
                };

                socket.remote = event
                    .capture(1)
                    .and_then(|capture| SocketAddress::parse(&capture.bytes))
                    .map(|address| address.endpoint());
                let cloexec = name == "accept4" && a[3] & SOCK_CLOEXEC != 0;
                process.fds.insert(
                    result,
                    Description {
                        text: socket.text().into(),
                        kind: Kind::Socket(socket),
                        cloexec,
                    },
                );
            }
            "pipe" | "pipe2" | "socketpair" => {
                let arg = if name == "socketpair" { 3 } else { 0 };
                let cloexec = match name {
                    "pipe2" => a[1] & O_CLOEXEC != 0,
                    "socketpair" => a[1] & SOCK_CLOEXEC != 0,
                    _ => false,
                };

                if let Some(capture) = event.capture(arg)
                    && capture.bytes.len() >= 8
                {
                    for half in capture.bytes[..8].as_chunks::<4>().0 {
                        let number = i64::from(i32::from_le_bytes(*half));
                        let (text, kind) = if name == "socketpair" {
                            (
                                "UNIX:[]",
                                Kind::Socket(Socket {
                                    family: sockaddr::AF_UNIX,
                                    ..Socket::default()
                                }),
                            )
                        } else {
                            ("pipe:[]", Kind::Pipe)
                        };

                        process.fds.insert(
                            number,
                            Description {
                                text: text.into(),
                                kind,
                                cloexec,
                            },
                        );
                    }
                }
            }
            "eventfd" | "eventfd2" => {
                process.fds.insert(
                    result,
                    anonymous(
                        "anon_inode:[eventfd]",
                        name == "eventfd2" && a[1] & O_CLOEXEC != 0,
                    ),
                );
            }
            "epoll_create" | "epoll_create1" => {
                process.fds.insert(
                    result,
                    anonymous(
                        "anon_inode:[eventpoll]",
                        name == "epoll_create1" && a[0] & O_CLOEXEC != 0,
                    ),
                );
            }
            "timerfd_create" => {
                process.fds.insert(
                    result,
                    anonymous("anon_inode:[timerfd]", a[1] & O_CLOEXEC != 0),
                );
            }
            "signalfd" | "signalfd4" => {
                process.fds.insert(
                    result,
                    anonymous(
                        "anon_inode:[signalfd]",
                        name == "signalfd4" && a[3] & O_CLOEXEC != 0,
                    ),
                );
            }
            "inotify_init" | "inotify_init1" => {
                process.fds.insert(
                    result,
                    anonymous(
                        "anon_inode:inotify",
                        name == "inotify_init1" && a[0] & O_CLOEXEC != 0,
                    ),
                );
            }
            "memfd_create" => {
                let text = format!("/memfd:{} (deleted)", string(0).unwrap_or_default());
                process.fds.insert(result, anonymous(&text, a[1] & 1 != 0));
            }
            "pidfd_open" => {
                process
                    .fds
                    .insert(result, anonymous("anon_inode:[pidfd]", true));
            }
            "io_uring_setup" => {
                process
                    .fds
                    .insert(result, anonymous("anon_inode:[io_uring]", false));
            }

            // What these made, the engine reads from the system when it can;
            // otherwise it is unknown, never mistaken for closed.
            "userfaultfd"
            | "perf_event_open"
            | "fanotify_init"
            | "landlock_create_ruleset"
            | "memfd_secret"
            | "pidfd_getfd" => {
                process
                    .fds
                    .entry(result)
                    .or_insert_with(|| anonymous("?", false));
            }
            _ => {}
        }
    }

    fn address(process: &mut Process, fd: i64, event: &SyscallEvent, arg: u8, remote: bool) {
        let Some(address) = event
            .capture(arg)
            .and_then(|capture| SocketAddress::parse(&capture.bytes))
        else {
            return;
        };

        if let Some(description) = process.fds.get_mut(&fd)
            && let Kind::Socket(socket) = &mut description.kind
        {
            if remote {
                socket.remote = Some(address.endpoint());
            } else {
                socket.local = Some(address.endpoint());
            }

            description.text = socket.text().into();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syscalls::model::{Abi, Capture, CaptureState, EventFlags, ReadAt, SyscallId, Tid};

    fn call(name: &str, args: &[u64], result: i64, captured: Vec<(u8, &[u8])>) -> TraceEvent {
        let mut words = [0; 6];
        words[..args.len()].copy_from_slice(args);
        TraceEvent::Call(SyscallEvent {
            thread: Tid::new(7),
            process: Pid::new(7),
            call: SyscallId::named(Abi::X86_64, name).expect(name),
            args: words,
            captured: captured
                .into_iter()
                .map(|(arg, bytes)| Capture {
                    arg,
                    when: ReadAt::Exit,
                    state: CaptureState::Complete,
                    bytes: bytes.into(),
                })
                .collect(),
            outcome: Outcome::of_return(result),
            entered: Some(1),
            exited: Some(2),
            stack: None,
            flags: EventFlags::default(),
        })
    }

    fn text(table: &FdTable, fd: i64) -> Option<String> {
        table
            .describe(Pid::new(7), fd)
            .map(|description| description.text.to_string())
    }

    #[test]
    fn opens_dups_closes_and_exec_follow_the_kernel() {
        let mut table = FdTable::default();
        table.apply(&call(
            "openat",
            &[u64::MAX - 99, 0, O_CLOEXEC],
            3,
            vec![(1, b"/etc/hosts\0")],
        ));
        table.apply(&call("dup3", &[3, 9, 0], 9, vec![]));
        table.apply(&call(
            "openat",
            &[u64::MAX - 99, 0, 0],
            -2,
            vec![(1, b"/missing\0")],
        ));
        assert_eq!(text(&table, 3).as_deref(), Some("/etc/hosts"));
        assert_eq!(text(&table, 9).as_deref(), Some("/etc/hosts"));
        assert_eq!(text(&table, 4), None);

        table.apply(&TraceEvent::Exec {
            thread: Tid::new(7),
            process: Pid::new(7),
            time: 3,
            path: "/bin/true".into(),
        });
        assert_eq!(text(&table, 3), None, "close-on-exec");
        assert_eq!(text(&table, 9).as_deref(), Some("/etc/hosts"));

        table.apply(&call(
            "close_range",
            &[5, u64::from(u32::MAX), 0],
            0,
            vec![],
        ));
        assert_eq!(text(&table, 9), None);

        // What the engine read from /proc just before the call says more
        // than the call's relative path.
        table.apply(&TraceEvent::Descriptor {
            process: Pid::new(7),
            fd: 4,
            time: 4,
            text: "/home/user/notes".into(),
            cloexec: false,
            device: None,
            made: true,
        });
        assert_eq!(
            table.describe_before(Pid::new(7), 4),
            None,
            "made by the next call"
        );
        table.apply(&call(
            "openat",
            &[u64::MAX - 99, 0, 0],
            4,
            vec![(1, b"notes\0")],
        ));
        assert_eq!(text(&table, 4).as_deref(), Some("/home/user/notes"));
        table.apply(&call(
            "openat",
            &[u64::MAX - 99, 0, 0],
            5,
            vec![(1, b"notes\0")],
        ));
        assert_eq!(text(&table, 5).as_deref(), Some("notes"), "only once");
    }

    #[test]
    fn sockets_gather_their_endpoints_and_children_inherit() {
        let mut table = FdTable::default();
        table.apply(&call("socket", &[2, 1, 0], 5, vec![]));
        let address = [2, 0, 0x15, 0x38, 10, 0, 3, 9, 0, 0, 0, 0, 0, 0, 0, 0];
        table.apply(&call("connect", &[5, 0, 16], -111, vec![(1, &address)]));
        assert_eq!(
            text(&table, 5).as_deref(),
            Some("TCP:[]"),
            "a failed call changes nothing"
        );
        table.apply(&call("connect", &[5, 0, 16], -115, vec![(1, &address)]));
        assert_eq!(text(&table, 5).as_deref(), Some("TCP:[->10.0.3.9:5432]"));

        table.apply(&TraceEvent::Spawn {
            parent_thread: Tid::new(7),
            parent: Pid::new(7),
            child: Tid::new(8),
            thread: false,
            time: 4,
        });
        assert_eq!(
            table
                .describe(Pid::new(8), 5)
                .map(|d| d.text.to_string())
                .as_deref(),
            Some("TCP:[->10.0.3.9:5432]")
        );

        let fds = [3, 0, 0, 0, 4, 0, 0, 0];
        table.apply(&call("pipe2", &[0, 0], 0, vec![(0, &fds)]));
        assert_eq!(text(&table, 4).as_deref(), Some("pipe:[]"));

        table.apply(&TraceEvent::Lost { time: 5, count: 1 });
        assert_eq!(
            table.stale().collect::<Vec<_>>(),
            [Pid::new(7), Pid::new(8)]
        );
    }
}
