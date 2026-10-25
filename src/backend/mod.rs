#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("microscope currently supports only Linux x86-64");

mod linux;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

use crate::debug_info::DebugInfo;
use crate::protocol::{CoreDumpOptions, DebuggerEvent, Request};
use crate::{Error, ProcessId, Result};

pub use linux::ControllerJoin;
pub use linux::PostMortemSession;
pub use linux::RunningTls;
pub use linux::bpf;
#[cfg(test)]
pub use linux::native_tracee;
pub use linux::perf;
pub use linux::reaped;
pub use linux::resume_address;
#[cfg(any(test, feature = "sim"))]
pub use linux::sim_edge;
pub use linux::syscalls as syscall_engines;
pub use linux::{Capacity, GroupBackend, GroupNotice, Joined, Started, spawn_group};

/// Identifies a file independently of the path used to open it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub inode: u64,
}

/// An executable's contents and identity, read once when a session starts
/// and kept only until its debug information is read.
pub struct ExecutableSource {
    pub display_path: Arc<PathBuf>,

    /// The path a launch runs: the one it was named by, links kept, as a
    /// shell runs it. A virtual environment's interpreter is a link that
    /// finds its environment from the path it runs as.
    pub launch_path: Arc<PathBuf>,
    pub data: Arc<[u8]>,
    pub identity: FileIdentity,

    /// For an attach target, the start time that keeps a recycled process ID
    /// from being mistaken for the process whose executable was read.
    pub process_start_time: Option<u64>,

    /// Where the session finds separate debug files.
    pub debug_files: crate::debug_info::DebugFileSearch,
}

impl ExecutableSource {
    /// What a session keeps of the executable: its identity and the facts
    /// process control needs, but none of its bytes.
    pub fn described(self) -> Executable {
        Executable {
            c_library: linux::CLibrary::of_executable(&self.data),
            display_path: self.display_path,
            launch_path: self.launch_path,
            identity: self.identity,
            process_start_time: self.process_start_time,
            debug_files: self.debug_files,
        }
    }
}

/// What a session keeps of its executable once its debug information is
/// read.
#[derive(Clone)]
pub struct Executable {
    pub display_path: Arc<PathBuf>,
    pub launch_path: Arc<PathBuf>,
    pub identity: FileIdentity,
    pub process_start_time: Option<u64>,
    pub debug_files: crate::debug_info::DebugFileSearch,

    /// The C library the executable runs on, whose structures locate TLS.
    c_library: linux::CLibrary,
}

pub fn executable_source(path: &Path) -> Result<ExecutableSource> {
    let named = std::path::absolute(program_file(path)?)?;
    let path = script_interpreter(&named)?.canonicalize()?;
    let mut source = read_executable_source(&path, path.clone(), None)?;
    source.launch_path = Arc::new(named);
    Ok(source)
}

/// The most interpreters a script's interpreter line leads through, as
/// the kernel follows them, before the program that runs is reached.
const MAX_INTERPRETERS: usize = 4;

/// The program the kernel runs when it executes `program`: `program`
/// itself, or for a script, the interpreter its `#!` line names, and that
/// interpreter's own if it is a script too. The kernel reads the first
/// word after `#!` as the interpreter's path, without searching `PATH`;
/// the rest of the line and the script's arguments are its business, and
/// it builds the command line itself.
fn script_interpreter(program: &Path) -> Result<PathBuf> {
    let mut current = program.to_owned();

    for _ in 0..=MAX_INTERPRETERS {
        let Some(interpreter) = interpreter_line(&current)? else {
            return Ok(current);
        };

        if !interpreter.is_file() {
            return Err(Error::UnsupportedFile(
                format!(
                    "{} names the interpreter {}, which does not exist",
                    current.display(),
                    interpreter.display()
                )
                .into(),
            ));
        }

        current = interpreter;
    }

    Err(Error::UnsupportedFile(
        format!(
            "its interpreters lead through more than {MAX_INTERPRETERS} scripts, which the \
             kernel does not run"
        )
        .into(),
    ))
}

/// The interpreter a script's `#!` line names, or `None` for a file that
/// is no script. As the kernel reads it, the line is at most the first 256
/// bytes, and the path is its first word.
fn interpreter_line(path: &Path) -> Result<Option<PathBuf>> {
    use std::io::Read as _;
    use std::os::unix::ffi::OsStrExt as _;

    // Reading a FIFO would take bytes its reader needs; the kernel runs
    // only regular files anyway.
    if !fs::metadata(path)?.is_file() {
        return Ok(None);
    }

    let mut head = Vec::with_capacity(256);
    fs::File::open(path)?.take(256).read_to_end(&mut head)?;
    let Some(line) = head.strip_prefix(b"#!") else {
        return Ok(None);
    };

    let line = line.split(|byte| *byte == b'\n').next().unwrap_or_default();
    let word = line
        .split(|byte| matches!(byte, b' ' | b'\t'))
        .find(|word| !word.is_empty());

    Ok(word.map(|word| PathBuf::from(std::ffi::OsStr::from_bytes(word))))
}

/// The file a program names: its path, or for a name with no directory and
/// no file of that name in the current directory, the first executable file
/// of that name on `PATH`, as gdb finds it.
fn program_file(path: &Path) -> Result<std::borrow::Cow<'_, Path>> {
    let alone = matches!(
        path.components().collect::<Vec<_>>().as_slice(),
        [std::path::Component::Normal(_)]
    );
    if !alone || fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return Ok(path.into());
    }

    std::env::var_os("PATH")
        .and_then(|directories| {
            std::env::split_paths(&directories)
                .map(|directory| directory.join(path))
                // As a shell finds it: a file this user may run.
                .find(|candidate| {
                    fs::metadata(candidate).is_ok_and(|metadata| metadata.is_file())
                        && nix::unistd::access(candidate, nix::unistd::AccessFlags::X_OK).is_ok()
                })
        })
        .map(Into::into)
        .ok_or_else(|| Error::ProgramNotFound(path.to_owned()))
}

/// Reads a running process's executable through `/proc`, which works even
/// when the file has since been replaced or unlinked.
pub fn process_executable_source(process: ProcessId) -> Result<ExecutableSource> {
    let raw = i32::try_from(process.get())
        .ok()
        .filter(|raw| *raw > 0)
        .ok_or(Error::InvalidProcessId(process.get()))?;
    let (proc_exe, display) = process_exe_link(raw)?;
    let start_time =
        process_start_time(raw).ok_or(Error::ProcessIdentityUnavailable(process.get()))?;

    read_executable_source(&proc_exe, display, Some(start_time))
}

/// Finds a process's `exe` link and the path it names. A leader that exited
/// before the rest of its process has none, so a live thread's is used.
fn process_exe_link(process: i32) -> std::io::Result<(PathBuf, PathBuf)> {
    let leader = PathBuf::from(format!("/proc/{process}/exe"));
    let error = match fs::read_link(&leader) {
        Ok(display) => return Ok((leader, display)),
        Err(error) => error,
    };

    let Ok(mut threads) = fs::read_dir(format!("/proc/{process}/task")) else {
        return Err(error);
    };

    threads
        .find_map(|thread| {
            let link = thread.ok()?.path().join("exe");
            let display = fs::read_link(&link).ok()?;
            Some((link, display))
        })
        .ok_or(error)
}

fn read_executable_source(
    read_path: &Path,
    display_path: PathBuf,
    process_start_time: Option<u64>,
) -> Result<ExecutableSource> {
    let (data, stamp) = crate::image::backing::read_input(read_path)?;
    let display_path = Arc::new(display_path);

    Ok(ExecutableSource {
        launch_path: Arc::clone(&display_path),
        display_path,
        data: data.into(),
        identity: FileIdentity { inode: stamp.inode },
        process_start_time,
        debug_files: crate::debug_info::DebugFileSearch::default(),
    })
}

/// A thread's start time in clock ticks since boot.
pub fn thread_start_time(process: crate::model::Pid, thread: crate::model::Tid) -> Option<u64> {
    linux::thread_start_time(process, thread)
}

pub fn process_exited(process: crate::model::Pid) -> bool {
    linux::process_exited(process)
}

/// Returns field 22 of `/proc/<pid>/stat`, in clock ticks since boot.
pub fn process_start_time(process: i32) -> Option<u64> {
    linux::process_start_time(process)
}

pub enum ControllerMessage {
    Request(Request),
    Wait(linux::WaitEvent),
    /// The controller reached an outstanding agent request deadline.
    AgentTimeout,

    /// A message from a runtime's inspector connection.
    Inspector(linux::InspectorMessage),
}

impl ControllerMessage {
    /// Whether the controller serves the message before inspection queued
    /// ahead of it: run control, and the process events it classifies. A
    /// burst of inspection never delays a step.
    pub(crate) const fn preempts_inspection(&self) -> bool {
        matches!(
            self,
            Self::AgentTimeout
                | Self::Wait(_)
                | Self::Request(
                    Request::Continue { .. }
                        | Request::Step { .. }
                        | Request::Pause { .. }
                        | Request::Kill { .. }
                        | Request::Terminate { .. }
                        | Request::Shutdown { .. }
                )
        )
    }

    /// Whether the message only reads the stop it names, so it may wait
    /// behind later run control and then fail as a stale request does.
    pub(crate) const fn reads_one_stop(&self) -> bool {
        matches!(
            self,
            Self::Request(
                Request::Variables { .. }
                    | Request::Evaluate {
                        mode: crate::EvaluationMode::Read,
                        ..
                    }
                    | Request::ExpressionType { .. }
                    | Request::ExplainView { .. }
                    | Request::RecordKernels { .. }
                    | Request::Dereference { .. }
                    | Request::ValueChildren { .. }
                    | Request::VisualizerInputs { .. }
                    | Request::Numbers { .. }
                    | Request::Backtrace { .. }
                    | Request::Registers { .. }
                    | Request::ReadMemory { .. }
                    | Request::Disassemble { .. }
                    | Request::DescribeAddress { .. }
                    | Request::StoppedLocation { .. }
            )
        )
    }
}

/// The channels a controller serves.
pub struct ControllerChannels {
    /// Lets the controller's waiter thread queue native events.
    pub sender: mpsc::Sender<ControllerMessage>,
    pub receiver: mpsc::Receiver<ControllerMessage>,
    pub events: EventSender,
}

/// Publishes debugger events to every subscribed client, recording each in
/// the flight recorder of a development build.
pub struct EventSender {
    sender: broadcast::Sender<DebuggerEvent>,

    /// A simulated session's copy of every event, for its auditor, which
    /// must never fall behind as a subscriber can.
    #[cfg(any(test, feature = "sim"))]
    tap: Option<mpsc::UnboundedSender<DebuggerEvent>>,
}

impl EventSender {
    pub fn send(
        &self,
        event: DebuggerEvent,
    ) -> std::result::Result<usize, broadcast::error::SendError<DebuggerEvent>> {
        record!("event {event:?}");

        // An auditor that has gone takes nothing.
        #[cfg(any(test, feature = "sim"))]
        if let Some(tap) = &self.tap {
            let _ = tap.send(event.clone());
        }

        self.sender.send(event)
    }

    /// Publishes to `sender`'s subscribers, and copies every event into
    /// `tap`.
    #[cfg(any(test, feature = "sim"))]
    pub const fn tapped(
        sender: broadcast::Sender<DebuggerEvent>,
        tap: mpsc::UnboundedSender<DebuggerEvent>,
    ) -> Self {
        Self {
            sender,
            tap: Some(tap),
        }
    }
}

impl From<broadcast::Sender<DebuggerEvent>> for EventSender {
    fn from(sender: broadcast::Sender<DebuggerEvent>) -> Self {
        Self {
            sender,
            #[cfg(any(test, feature = "sim"))]
            tap: None,
        }
    }
}

/// Starts the controller for a live session of `executable`.
pub fn spawn_controller(
    executable: Executable,
    debug_info: DebugInfo,
    channels: ControllerChannels,
) -> Result<(
    tokio::sync::watch::Receiver<Arc<crate::ModuleImage>>,
    ControllerJoin,
)> {
    linux::spawn_controller(executable, debug_info, channels)
}

/// Opens a core dump and starts the controller serving its stopped snapshot.
pub fn open_core(
    options: &CoreDumpOptions,
    channels: ControllerChannels,
) -> Result<PostMortemSession> {
    linux::open_core(options, channels)
}

/// The processes a name names.
pub fn processes_named(name: &str) -> Result<Vec<crate::NamedProcess>> {
    linux::processes_named(name)
}

/// Finds a signal's exception code by name.
pub fn signal_named(name: &str) -> Option<u64> {
    linux::Signal::named(name).map(linux::Signal::code)
}

/// Names the signal with an exception code.
pub fn signal_name(code: u64) -> Option<String> {
    linux::Signal::from_code(code).map(linux::Signal::name)
}

/// The exception codes of every signal, in order.
pub fn signal_codes() -> impl Iterator<Item = u64> {
    linux::Signal::all().map(linux::Signal::code)
}

/// Each thread's id and thread pointer in process `pid`, which may be
/// running, read from its C library's lists of its threads without
/// stopping it, or why they cannot be.
pub fn running_thread_pointers(pid: u32) -> std::result::Result<Vec<(u32, u64)>, String> {
    linux::running_thread_pointers(pid)
}

/// The target namespace id of this process's live thread, when proven.
pub fn target_thread_ids(
    process: u32,
    threads: &[u32],
) -> Vec<(u32, crate::runtime_model::TargetThreadId)> {
    linux::target_thread_ids(process, threads)
}

pub fn target_thread_id(process: u32, thread: u32) -> Option<crate::runtime_model::TargetThreadId> {
    linux::target_thread_id(process, thread)
}

/// Describes the platform's hardware watchpoint support.
pub fn watchpoint_capabilities() -> crate::WatchpointCapabilities {
    linux::watchpoint_capabilities()
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_core_dump(data: &[u8]) {
    linux::fuzz_core_dump(data);
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_cdp_message(data: &[u8]) {
    linux::fuzz_cdp_message(data);
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_debug_register_plan(data: &[u8]) {
    linux::fuzz_debug_register_plan(data);
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_glibc_tls(data: &[u8]) {
    linux::fuzz_glibc_tls(data);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a script under a fresh directory and reads its interpreter.
    fn interpreter_of(head: &[u8]) -> Option<PathBuf> {
        let directory = std::env::temp_dir().join(format!(
            "microscope-interpreter-{}-{}",
            std::process::id(),
            head.len()
        ));
        fs::create_dir_all(&directory).expect("make a directory");
        let script = directory.join("script");
        fs::write(&script, head).expect("write a script");
        let interpreter = interpreter_line(&script).expect("read the script");
        fs::remove_dir_all(&directory).expect("remove the directory");
        interpreter
    }

    #[test]
    fn an_interpreter_is_the_first_word_after_the_marker() {
        for (head, interpreter) in [
            (
                &b"#!/usr/bin/env node\nconsole.log(1)\n"[..],
                Some("/usr/bin/env"),
            ),
            (
                b"#! \t/usr/bin/env -S node --no-warnings\n",
                Some("/usr/bin/env"),
            ),
            (b"#!/bin/sh", Some("/bin/sh")),
            (b"#!\n", None),
            (b"#!  \n/bin/sh\n", None),
            (b"echo hi\n", None),
            (b"\x7fELF\x02\x01\x01", None),
            (b"", None),
        ] {
            assert_eq!(
                interpreter_of(head).as_deref(),
                interpreter.map(Path::new),
                "{}",
                String::from_utf8_lossy(head)
            );
        }
    }

    /// The kernel reads only the first 256 bytes, so a path that runs past
    /// them is cut where the kernel cuts it.
    #[test]
    fn an_interpreter_line_ends_where_the_kernel_stops_reading() {
        let mut head = b"#!/".to_vec();
        head.extend(std::iter::repeat_n(b'a', 400));
        let interpreter = interpreter_of(&head).expect("an interpreter");
        assert_eq!(interpreter.as_os_str().len(), 254);
    }
}
