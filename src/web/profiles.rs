use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

use microscope::capture_view::{
    AnswerOptions, CaptureView, ViewError, end_text, protocol as view_protocol,
};
use microscope::profiler::{
    CodeLine, Cuts, LineCost, Preset, Profile, RecordOptions, SampleSource, SyscallEngine,
    SyscallLevel, Watch, record_watched,
};
use microscope::syscalls::ProcessEnd;
use serde_json::{Value, json};
use tokio::sync::{oneshot, watch};

use super::protocol::{
    self, ErrorKind, ProfileInfo, ProfileList, RecordPreset, RecordProfile, RecordStarted, Request,
    Role, SyscallsEngine, SyscallsLevel,
};

use super::session::Failure;

pub(super) const LIVE_EVERY: Duration = Duration::from_secs(2);

const MOST_SOURCE_BYTES: u64 = 4 << 20;
const SOURCE_MARGIN: u64 = 5;

pub struct Profiles {
    cwd: PathBuf,
    entries: RwLock<BTreeMap<String, Arc<Entry>>>,
    list: watch::Sender<Arc<ProfileList>>,
    recordings: Mutex<BTreeMap<String, Recorder>>,
    recorded: AtomicU64,
}

struct Entry {
    info: ProfileInfo,
    profile: Arc<Profile>,
    view: Arc<CaptureView>,
}

struct Recorder {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Profiles {
    pub fn new(cwd: PathBuf) -> Arc<Self> {
        let (list, _) = watch::channel(Arc::new(ProfileList {
            profiles: Vec::new(),
        }));

        Arc::new(Self {
            cwd,
            entries: RwLock::new(BTreeMap::new()),
            list,
            recordings: Mutex::new(BTreeMap::new()),
            recorded: AtomicU64::new(0),
        })
    }

    pub fn add(&self, path: &Path, bytes: &[u8]) -> anyhow::Result<String> {
        let id = format!("{:016x}", twox_hash::XxHash3_64::oneshot(bytes));
        Ok(self.add_read(
            path,
            Profile::from_bytes(bytes)?,
            id,
            Some(bytes.len() as u64),
        ))
    }

    pub fn add_segments(&self, directory: &Path) -> anyhow::Result<String> {
        let read = Profile::read_segments(directory)?;
        let id = format!(
            "{:016x}",
            twox_hash::XxHash3_64::oneshot(directory.as_os_str().as_encoded_bytes())
        );
        Ok(self.add_read(directory, read, id, None))
    }

    fn add_read(
        &self,
        path: &Path,
        mut read: microscope::profiler::ReadProfile,
        id: String,
        bytes: Option<u64>,
    ) -> String {
        if read.truncated {
            eprintln!(
                "warning: {} was cut short; serving the part that was complete",
                path.display()
            );
        }

        for module in read
            .profile
            .symbolize(&microscope::DebugFileOptions::default())
        {
            if let Err(why) = &module.source {
                eprintln!(
                    "note: {} could not be symbolized, so its addresses stay: {why}",
                    module.module
                );
            }
        }

        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );

        self.publish(
            ProfileInfo {
                id: id.clone(),
                name,
                recording: false,
                revision: 0,
                ended: None,
            },
            read.profile,
            Some(view_protocol::ProfileFile {
                bytes,
                chunks: read.chunks as u64,
                truncated: read.truncated,
            }),
        );
        id
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "the list is sent under the lock, so changes publish in order"
    )]
    pub(super) fn publish(
        &self,
        info: ProfileInfo,
        profile: Profile,
        file: Option<view_protocol::ProfileFile>,
    ) {
        let view = Arc::new(CaptureView::new(
            profile.shared_capture(),
            info.id.clone(),
            info.name.clone(),
            file,
        ));
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        let id = info.id.clone();
        entries.insert(
            id,
            Arc::new(Entry {
                info,
                profile: Arc::new(profile),
                view,
            }),
        );
        let profiles = entries.values().map(|entry| entry.info.clone()).collect();
        self.list.send_replace(Arc::new(ProfileList { profiles }));
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<ProfileList>> {
        self.list.subscribe()
    }

    pub fn file(&self, id: &str) -> Option<(String, std::io::Result<Vec<u8>>)> {
        let entry = self.entry(id).ok()?;
        let name = if Path::new(&entry.info.name)
            .extension()
            .is_some_and(|extension| extension == "usprof")
        {
            entry.info.name.clone()
        } else {
            format!("{}.usprof", entry.info.name)
        };

        Some((name, entry.profile.to_bytes()))
    }

    pub fn snapshot(&self, id: &str) -> Result<(String, Arc<Profile>), Failure> {
        let entry = self.entry(id)?;
        Ok((entry.info.name.clone(), Arc::clone(&entry.profile)))
    }

    pub fn shutdown(&self) {
        let recordings = std::mem::take(
            &mut *self
                .recordings
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for recorder in recordings.values() {
            recorder.stop.store(true, Ordering::Relaxed);
        }

        for thread in recordings
            .into_values()
            .filter_map(|recorder| recorder.thread)
        {
            let _ = thread.join();
        }
    }

    pub async fn handle(
        self: &Arc<Self>,
        role: Role,
        request: Request,
    ) -> Result<(Value, Vec<u8>), Failure> {
        match request {
            Request::RecordProfile(spec) => {
                let profile = self.record(role, spec).await?;
                Ok(json(&RecordStarted { profile }))
            }
            Request::StopRecording(protocol::ProfileRef { profile, .. }) => {
                controls(role)?;
                let recordings = self
                    .recordings
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let recorder = recordings.get(&profile).ok_or_else(|| {
                    Failure::new(ErrorKind::Invalid, format!("{profile} is not recording"))
                })?;
                recorder.stop.store(true, Ordering::Relaxed);
                drop(recordings);

                Ok(json(&Value::Null))
            }
            request => {
                let profiles = Arc::clone(self);
                tokio::task::spawn_blocking(move || profiles.answer(request))
                    .await
                    .map_err(|_| Failure::new(ErrorKind::Failed, "the query failed"))?
            }
        }
    }

    async fn record(self: &Arc<Self>, role: Role, spec: RecordProfile) -> Result<String, Failure> {
        controls(role)?;
        let program = PathBuf::from(&spec.program);
        let mut command = Command::new(&program);
        command
            .args(&spec.arguments)
            .envs(spec.environment.iter().map(|(name, value)| (name, value)))
            .current_dir(
                spec.cwd
                    .as_ref()
                    .map_or_else(|| self.cwd.clone(), |cwd| self.cwd.join(cwd)),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let options = sampling(&spec)?;
        let count = self.recorded.fetch_add(1, Ordering::Relaxed);
        let id = format!(
            "{:016x}",
            twox_hash::XxHash3_64::oneshot(
                format!(
                    "{} {count} {:?}",
                    spec.program,
                    std::time::SystemTime::now()
                )
                .as_bytes()
            )
        );
        let name = program.file_name().map_or_else(
            || spec.program.clone(),
            |name| name.to_string_lossy().into_owned(),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let (started, starting) = oneshot::channel::<Result<(), String>>();
        let profiles = Arc::clone(self);
        let run = {
            let (id, stop) = (id.clone(), Arc::clone(&stop));
            move || profiles.run_recording(&id, &name, command, &options, &stop, started)
        };

        let unlisted = self
            .start_recorder(&id, stop, run, |run| {
                std::thread::Builder::new()
                    .name("microscope-web-record".into())
                    .spawn(run)
            })
            .map_err(|error| Failure::new(ErrorKind::Failed, error.to_string()))?;
        if let Some(thread) = unlisted {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }

        match starting.await {
            Ok(Ok(())) => Ok(id),
            Ok(Err(message)) => Err(Failure::new(ErrorKind::Failed, message)),
            Err(_) => Err(Failure::new(
                ErrorKind::Failed,
                "the recording ended before it began",
            )),
        }
    }

    fn run_recording(
        &self,
        id: &str,
        name: &str,
        command: Command,
        options: &RecordOptions,
        stop: &AtomicBool,
        started: oneshot::Sender<Result<(), String>>,
    ) {
        let mut started = Some(started);
        let mut revision = 0;
        let info = |revision, ended: Option<String>| ProfileInfo {
            id: id.to_owned(),
            name: name.to_owned(),
            recording: ended.is_none(),
            revision,
            ended,
        };

        let recorded = record_watched(
            command,
            options,
            &mut Watch {
                every: LIVE_EVERY,
                publish: &mut |profile| {
                    self.publish(info(revision, None), profile, None);
                    revision += 1;
                    if let Some(started) = started.take() {
                        let _ = started.send(Ok(()));
                    }
                },
                stop,
                save: None,
            },
        );

        match recorded {
            Ok(recording) => {
                self.publish(
                    info(revision, Some(exit_text(recording.status))),
                    recording.profile,
                    None,
                );
            }
            Err(error) => {
                if let Some(started) = started.take() {
                    let _ = started.send(Err(error.to_string()));
                } else if let Ok(entry) = self.entry(id) {
                    let profile = Profile::clone(&entry.profile);
                    self.publish(
                        info(revision, Some(format!("failed: {error}"))),
                        profile,
                        None,
                    );
                }
            }
        }
    }

    fn start_recorder(
        self: &Arc<Self>,
        id: &str,
        stop: Arc<AtomicBool>,
        run: impl FnOnce() + Send + 'static,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<JoinHandle<()>>,
    ) -> std::io::Result<Option<JoinHandle<()>>> {
        self.recordings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.to_owned(), Recorder { stop, thread: None });
        let profiles = Arc::clone(self);
        let listed = id.to_owned();
        let started = spawn(Box::new(move || {
            run();
            profiles
                .recordings
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&listed);
        }));
        let mut recordings = self
            .recordings
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let thread = match started {
            Ok(thread) => thread,
            Err(error) => {
                recordings.remove(id);
                return Err(error);
            }
        };

        Ok(match recordings.get_mut(id) {
            Some(recorder) => {
                recorder.thread = Some(thread);
                None
            }
            None => Some(thread),
        })
    }

    fn entry(&self, id: &str) -> Result<Arc<Entry>, Failure> {
        self.entries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
            .ok_or_else(|| Failure::new(ErrorKind::Invalid, format!("no profile {id}")))
    }

    fn answer(&self, request: Request) -> Result<(Value, Vec<u8>), Failure> {
        let (profile_id, method, params) = match request {
            Request::ProfileCode(query) => {
                let _span = microscope::span!("web profile code");
                let entry = self.entry(&query.query.profile)?;
                return Ok(json(&code(&entry, &query)?));
            }
            Request::Profile(q) => (q.profile.clone(), "profile", to_value(&q)),
            Request::ProfileInspect(q) => (q.profile.clone(), "profileInspect", to_value(&q)),
            Request::ProfileTree(q) => (q.profile.clone(), "profileTree", to_value(&q)),
            Request::ProfileFunctions(q) => (q.profile.clone(), "profileFunctions", to_value(&q)),
            Request::ProfileTimeline(q) => {
                (q.query.profile.clone(), "profileTimeline", to_value(&q))
            }
            Request::ProfileZones(q) => (q.profile.clone(), "profileZones", to_value(&q)),
            Request::ProfileZoneStats(q) => (q.profile.clone(), "profileZoneStats", to_value(&q)),
            Request::ProfileLines(q) => (q.query.profile.clone(), "profileLines", to_value(&q)),
            Request::SyscallSummary(q) => {
                (q.query.link.profile.clone(), "syscallSummary", to_value(&q))
            }
            Request::SyscallHeatmap(q) => {
                (q.query.link.profile.clone(), "syscallHeatmap", to_value(&q))
            }
            Request::SyscallDetail(q) => {
                (q.query.link.profile.clone(), "syscallDetail", to_value(&q))
            }
            Request::SyscallLog(q) => (q.query.link.profile.clone(), "syscallLog", to_value(&q)),
            Request::SyscallCall(q) => (q.profile.clone(), "syscallCall", to_value(&q)),
            Request::SyscallFiles(q) => (q.link.profile.clone(), "syscallFiles", to_value(&q)),
            Request::SyscallNetwork(q) => (q.link.profile.clone(), "syscallNetwork", to_value(&q)),
            Request::SyscallProcesses(q) => {
                (q.link.profile.clone(), "syscallProcesses", to_value(&q))
            }
            Request::SyscallInFlight(q) => (q.profile.clone(), "syscallInFlight", to_value(&q)),
            _ => {
                return Err(Failure::new(
                    ErrorKind::Failed,
                    "internal: only profile requests come here",
                ));
            }
        };

        let entry = self.entry(&profile_id)?;
        let base_lookup = |id: &str| self.entry(id).ok().map(|entry| Arc::clone(&entry.view));
        let opts = AnswerOptions {
            live: entry.info.recording,
            base: Some(&base_lookup),
            source_lines: Some(&source_lines),
            ..AnswerOptions::default()
        };
        entry.view.answer(method, params, opts).map_err(from_view)
    }
}

fn from_view(error: ViewError) -> Failure {
    match error {
        ViewError::Invalid(message) => Failure::new(ErrorKind::Invalid, message),
        ViewError::Failed(message) => Failure::new(ErrorKind::Failed, message),
    }
}

fn exit_text(status: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt as _;

    match (status.code(), status.signal()) {
        (Some(code), _) => end_text(ProcessEnd::Exited(code)),
        (None, Some(signal)) => end_text(ProcessEnd::Killed {
            signal,
            core: status.core_dumped(),
        }),
        (None, None) => status.to_string(),
    }
}

fn sampling(spec: &RecordProfile) -> Result<RecordOptions, Failure> {
    let mut options = RecordOptions::preset(match spec.preset {
        RecordPreset::Production => Preset::Production,
        RecordPreset::Development => Preset::Development,
        RecordPreset::Maximum => Preset::Maximum,
    });
    if let Some(symbol) = spec
        .function
        .clone()
        .filter(|symbol| !symbol.trim().is_empty())
    {
        options.source = SampleSource::Function { symbol };
    }

    if let Some(level) = spec.syscalls {
        options.syscalls.level = match level {
            SyscallsLevel::Off => SyscallLevel::Off,
            SyscallsLevel::Counts => SyscallLevel::Counts,
            SyscallsLevel::Calls => SyscallLevel::Calls,
            SyscallsLevel::Full => SyscallLevel::Full,
        };
        options.syscalls.asked = true;
    }

    if spec.syscall_engine == Some(SyscallsEngine::Ptrace) {
        options.syscalls.engine = SyscallEngine::Ptrace;
    }

    options
        .syscalls
        .check()
        .map_err(|why| Failure::new(ErrorKind::Invalid, why))?;

    if let Some(seconds) = spec.window {
        options.cuts = Cuts::Window(
            Duration::try_from_secs_f64(seconds)
                .ok()
                .filter(|window| !window.is_zero())
                .ok_or_else(|| {
                    Failure::new(ErrorKind::Invalid, "a window is a number of seconds")
                })?,
        );
    }

    Ok(options)
}

pub const fn is_profile_request(request: &Request) -> bool {
    matches!(
        request,
        Request::Profile(_)
            | Request::ProfileInspect(_)
            | Request::ProfileTree(_)
            | Request::ProfileFunctions(_)
            | Request::ProfileTimeline(_)
            | Request::ProfileZones(_)
            | Request::ProfileZoneStats(_)
            | Request::ProfileLines(_)
            | Request::ProfileCode(_)
            | Request::RecordProfile(_)
            | Request::StopRecording(_)
            | Request::SyscallSummary(_)
            | Request::SyscallHeatmap(_)
            | Request::SyscallDetail(_)
            | Request::SyscallLog(_)
            | Request::SyscallCall(_)
            | Request::SyscallFiles(_)
            | Request::SyscallNetwork(_)
            | Request::SyscallProcesses(_)
            | Request::SyscallInFlight(_)
    )
}

fn controls(role: Role) -> Result<(), Failure> {
    if role == Role::Control {
        Ok(())
    } else {
        Err(Failure::new(
            ErrorKind::Forbidden,
            "this link can only view; ask for a control link to record",
        ))
    }
}

fn source_files(costs: Vec<LineCost>) -> Vec<protocol::ProfileSourceFile> {
    let mut files: BTreeMap<Option<String>, Vec<[u64; 3]>> = BTreeMap::new();
    for line in costs {
        files
            .entry(line.file)
            .or_default()
            .push([u64::from(line.line), line.own, line.total]);
    }

    files
        .into_iter()
        .map(|(path, lines)| {
            let lowest = lines.iter().map(|line| line[0]).min().unwrap_or(1);
            let highest = lines.iter().map(|line| line[0]).max().unwrap_or(1);
            let first = lowest.saturating_sub(SOURCE_MARGIN).max(1);
            let read = path.as_deref().map_or_else(
                || Err("the debug information has no source file for these lines".to_owned()),
                |path| source_lines(path, first, highest + SOURCE_MARGIN),
            );
            let (text, unread) = match read {
                Ok(text) => (Some(text), None),
                Err(why) => (None, Some(why)),
            };

            protocol::ProfileSourceFile {
                path,
                lines,
                first: u32::try_from(first).unwrap_or(u32::MAX),
                text,
                unread,
            }
        })
        .collect()
}

fn source_lines(path: &str, first: u64, last: u64) -> Result<Vec<String>, String> {
    let size = std::fs::metadata(path)
        .map_err(|error| format!("{path} cannot be read: {error}"))?
        .len();
    if size > MOST_SOURCE_BYTES {
        return Err(format!("{path} is larger than {MOST_SOURCE_BYTES} bytes"));
    }

    let bytes = std::fs::read(path).map_err(|error| format!("{path} cannot be read: {error}"))?;
    let skip = usize::try_from(first - 1).unwrap_or(usize::MAX);
    let take = usize::try_from(last - first + 1).unwrap_or(usize::MAX);

    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .skip(skip)
        .take(take)
        .map(str::to_owned)
        .collect())
}

#[expect(
    clippy::too_many_lines,
    reason = "maps disassembled instructions, source files, and line tables into the web protocol"
)]
fn code(
    entry: &Entry,
    query: &protocol::ProfileFunctionQuery,
) -> Result<protocol::ProfileCode, Failure> {
    let mut plain = query.query.clone();
    plain.transforms.clear();
    plain.base = None;
    plain.syscall_leaves = false;
    let view_query: view_protocol::ProfileQuery = serde_json::from_value(to_value(&plain))
        .map_err(|error| Failure::new(ErrorKind::Invalid, error.to_string()))?;
    let selected = entry.view.resolve_query(&view_query).map_err(from_view)?;
    let code = match entry.profile.disassemble(&selected, &query.function) {
        Ok(code) => code,
        Err(why) => {
            return Ok(protocol::ProfileCode {
                weight: 0,
                module: None,
                blocks: Vec::new(),
                unplaced: Vec::new(),
                unavailable: Some(why),
                functions: Vec::new(),
                files: Vec::new(),
                unlined: None,
                inlined_into: None,
            });
        }
    };

    let mut owners: BTreeMap<&str, usize> = BTreeMap::new();
    for instruction in code.blocks.iter().flat_map(|block| &block.instructions) {
        let own = if code.inlined_into.is_some() {
            instruction
                .lines
                .iter()
                .find(|line| line.function == query.function)
        } else {
            instruction.lines.last()
        };
        if let Some(file) = own.and_then(|line| line.file.as_deref()) {
            *owners.entry(file).or_default() += 1;
        }
    }
    let own_file = owners
        .into_iter()
        .max_by_key(|&(file, count)| (count, std::cmp::Reverse(file)))
        .map(|(file, _)| file.to_owned());

    let mut files = source_files(code.line_costs());
    files.sort_by_key(|file| file.path != own_file);
    let file_index: BTreeMap<String, u32> = files
        .iter()
        .enumerate()
        .filter_map(|(index, file)| Some((file.path.clone()?, u32::try_from(index).ok()?)))
        .collect();

    let mut functions: Vec<String> = Vec::new();
    let mut function_index: BTreeMap<String, u32> = BTreeMap::new();
    let mut line = |line: CodeLine| {
        let next = u32::try_from(functions.len()).unwrap_or(u32::MAX);
        let function = *function_index
            .entry(line.function)
            .or_insert_with_key(|name| {
                functions.push(name.clone());
                next
            });
        let file = line.file.and_then(|file| file_index.get(&file).copied());
        protocol::ProfileCodeLine(function, file, line.line)
    };

    let blocks = code
        .blocks
        .into_iter()
        .map(|block| protocol::ProfileCodeBlock {
            symbol: block.symbol,
            instructions: block
                .instructions
                .into_iter()
                .map(|instruction| protocol::ProfileInstruction {
                    address: instruction.address,
                    tokens: instruction
                        .tokens
                        .into_iter()
                        .map(|token| {
                            protocol::ProfileToken(
                                super::lowlevel::token_kind(token.kind).to_owned(),
                                token.text.to_string(),
                            )
                        })
                        .collect(),
                    target: instruction.target,
                    own: instruction.own,
                    total: instruction.total,
                    lines: instruction.lines.into_iter().map(&mut line).collect(),
                })
                .collect(),
            stopped: block.stopped,
        })
        .collect();

    Ok(protocol::ProfileCode {
        weight: code.weight,
        module: Some(code.module),
        blocks,
        unplaced: code.unplaced.into_iter().map(Into::into).collect(),
        unavailable: None,
        functions,
        files,
        unlined: code.unlined,
        inlined_into: code.inlined_into,
    })
}

fn json(value: &impl serde::Serialize) -> (Value, Vec<u8>) {
    (to_value(value), Vec::new())
}

fn to_value(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or_else(|error| json!({ "error": error.to_string() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recording_that_ends_at_once_is_not_left_listed() {
        let profiles = Profiles::new(PathBuf::from("/"));
        let (ended, end) = std::sync::mpsc::channel();
        let unlisted = profiles
            .start_recorder(
                "quick",
                Arc::new(AtomicBool::new(false)),
                || {},
                |run| {
                    let thread = std::thread::spawn(move || {
                        run();
                        let _ = ended.send(());
                    });
                    end.recv_timeout(Duration::from_secs(10))
                        .expect("the recording ends");
                    Ok(thread)
                },
            )
            .expect("the recording starts");
        if let Some(thread) = unlisted {
            thread.join().expect("the recording's thread ends");
        }
        assert!(
            profiles
                .recordings
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty()
        );
    }

    #[test]
    fn publish_shares_capture_without_cloning() {
        let profiles = Profiles::new(PathBuf::from("/"));
        let profile = Profile::of_stacks("nanoseconds", &[(&[("main", "main.rs", 1)], 10)]);
        profiles.publish(
            ProfileInfo {
                id: "p1".into(),
                name: "p1.usprof".into(),
                recording: false,
                revision: 0,
                ended: None,
            },
            profile,
            None,
        );
        let Ok(entry) = profiles.entry("p1") else {
            panic!("published profile");
        };
        assert!(std::ptr::eq(entry.profile.capture(), entry.view.capture()));
    }
}
