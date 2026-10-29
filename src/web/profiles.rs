//! Profiles the server serves to every tab, whatever it debugs. Each is
//! named by a digest of its bytes, so a link to one opens the same profile
//! whenever a server serves it. Every answer is a query of
//! [`microscope::profiler`], which the CLI's reports read too.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

use microscope::profiler::{
    DiffCost, GapReason, Granularity, KeyKind, Keys, Lanes, Look, MarkerKind, NO_KEY, NO_TASK,
    Preset, Profile, Query, RecordOptions, SampleSource, Side, Source, Transform, Watch,
    diff_costs, diff_trees, record_watched,
};
use serde_json::{Value, json};
use tokio::sync::{oneshot, watch};

use microscope::syscalls::InFlight;

use super::protocol::{
    self, ErrorKind, ProfileBaseWeight, ProfileCost, ProfileFunctions, ProfileGranularity,
    ProfileInfo, ProfileKey, ProfileKeyKind, ProfileLane, ProfileList, ProfileLook, ProfileQuery,
    ProfileSetting, ProfileSource, ProfileSummary, ProfileTimeline, ProfileTree, RecordPreset,
    RecordProfile, RecordStarted, Request, Role, TransformOp,
};

use super::session::Failure;

/// The most slices a timeline is cut into.
const MOST_BUCKETS: u32 = 4096;

/// How often a recording publishes its profile so far.
pub(super) const LIVE_EVERY: Duration = Duration::from_secs(2);

/// The profiles one server serves.
pub struct Profiles {
    cwd: PathBuf,
    entries: RwLock<BTreeMap<String, Arc<Entry>>>,
    list: watch::Sender<Arc<ProfileList>>,
    recordings: Mutex<BTreeMap<String, Recorder>>,

    /// Counts recordings, so each gets an id of its own.
    recorded: AtomicU64,
}

struct Entry {
    info: ProfileInfo,
    profile: Arc<Profile>,

    /// Its system call trace, read once it is asked for.
    syscalls: std::sync::OnceLock<Arc<super::syscalls::Trace>>,

    /// The calls a live trace's threads were blocked in, and when it saw
    /// them, on its clock.
    in_flight: (Vec<InFlight>, u64),
}

struct Recorder {
    stop: Arc<AtomicBool>,

    /// Its thread, once started.
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

    /// Serves the profile in `bytes`, read from `path`, returning its id.
    pub fn add(&self, path: &Path, bytes: &[u8]) -> anyhow::Result<String> {
        let id = format!("{:016x}", twox_hash::XxHash3_64::oneshot(bytes));
        Ok(self.add_read(path, Profile::import(bytes)?, id))
    }

    /// Serves the segments of a continuous recording in `directory` as one
    /// profile, as they are now, returning its id.
    pub fn add_segments(&self, directory: &Path) -> anyhow::Result<String> {
        let read = Profile::read_segments(directory)?;
        let id = format!(
            "{:016x}",
            twox_hash::XxHash3_64::oneshot(directory.as_os_str().as_encoded_bytes())
        );
        Ok(self.add_read(directory, read, id))
    }

    fn add_read(
        &self,
        path: &Path,
        mut read: microscope::profiler::ReadProfile,
        id: String,
    ) -> String {
        if read.truncated {
            eprintln!(
                "warning: {} was cut short; serving the part that was complete",
                path.display()
            );
        }

        // A profile recorded with its symbols left for later gets those
        // this host's files and debug directories hold.
        for module in read
            .profile
            .symbolize(&microscope::DebugFileOptions::default())
        {
            if let Err(why) = &module.source {
                eprintln!("note: {} keeps its addresses: {why}", module.module);
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
            (Vec::new(), 0),
        );
        id
    }

    /// Serves `profile` as `info` says, replacing what its id served.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the list is sent under the lock, so changes publish in order"
    )]
    pub(super) fn publish(
        &self,
        info: ProfileInfo,
        profile: Profile,
        in_flight: (Vec<InFlight>, u64),
    ) {
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        let id = info.id.clone();
        entries.insert(
            id,
            Arc::new(Entry {
                info,
                profile: Arc::new(profile),
                syscalls: std::sync::OnceLock::new(),
                in_flight,
            }),
        );
        let profiles = entries.values().map(|entry| entry.info.clone()).collect();
        self.list.send_replace(Arc::new(ProfileList { profiles }));
    }

    /// The profiles, now and as they change.
    pub fn subscribe(&self) -> watch::Receiver<Arc<ProfileList>> {
        self.list.subscribe()
    }

    /// A profile's file, by its id, and the name to save it under.
    pub fn file(&self, id: &str) -> Option<(String, std::io::Result<Vec<u8>>)> {
        let entry = self.entry(id).ok()?;
        let name = if std::path::Path::new(&entry.info.name)
            .extension()
            .is_some_and(|extension| extension == "usprof")
        {
            entry.info.name.clone()
        } else {
            format!("{}.usprof", entry.info.name)
        };

        Some((name, entry.profile.to_bytes()))
    }

    /// Ends every recording, killing its program, and waits for each.
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

    /// Answers a request about a profile, with the bytes that go before the
    /// answer. Aggregation reads every sample a query selects, so it runs
    /// off the server's threads.
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

    /// Starts recording `spec`, answering once its program has started.
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

        let traced = spec
            .syscalls
            .as_ref()
            .map(|syscalls| {
                if spec.function.is_some() || spec.window.is_some() {
                    return Err(Failure::new(
                        ErrorKind::Invalid,
                        "a system call trace samples no function and keeps no window",
                    ));
                }

                super::live::options(syscalls)
            })
            .transpose()?;
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
        let program = std::iter::once(&spec.program)
            .chain(&spec.arguments)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        let run = {
            let (id, stop) = (id.clone(), Arc::clone(&stop));
            move || match traced {
                Some(traced) => {
                    let publisher = super::live::Publisher {
                        profiles: &profiles,
                        id: &id,
                        name: &name,
                        started: Some(started),
                    };

                    super::live::record(publisher, command, &program, &traced, &stop);
                }
                None => profiles.run_recording(&id, &name, command, &options, &stop, started),
            }
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

    /// Records on this thread, publishing each profile under `id`.
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
                    self.publish(info(revision, None), profile, (Vec::new(), 0));
                    revision += 1;
                    if let Some(started) = started.take() {
                        let _ = started.send(Ok(()));
                    }
                },
                stop,
                // The page saves what it shows: with a window, the window.
                save: None,
            },
        );

        match recorded {
            Ok(recording) => {
                self.publish(
                    info(revision, Some(recording.status.to_string())),
                    recording.profile,
                    (Vec::new(), 0),
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
                        (Vec::new(), 0),
                    );
                }
            }
        }
    }

    /// Runs `run` as the recording `id` on the thread `spawn` starts, and
    /// lists it until `run` returns. Returns the thread when it is no
    /// longer listed, having ended or been stopped by a shutdown, for the
    /// caller to join.
    fn start_recorder(
        self: &Arc<Self>,
        id: &str,
        stop: Arc<AtomicBool>,
        run: impl FnOnce() + Send + 'static,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<JoinHandle<()>>,
    ) -> std::io::Result<Option<JoinHandle<()>>> {
        // Listed before it starts, so that it is unlisted only once it ends.
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

    /// A query's base, read with the query's own filters and transforms.
    fn base(&self, query: &ProfileQuery) -> Result<Option<(Arc<Entry>, ProfileQuery)>, Failure> {
        let Some(base) = &query.base else {
            return Ok(None);
        };

        if base.profile != query.profile && (query.threads.is_some() || query.tasks.is_some()) {
            return Err(Failure::new(
                ErrorKind::Invalid,
                "a diff with another profile cannot keep a thread or task filter, \
                 whose rows are this profile's",
            ));
        }

        let mut of_base = query.clone();
        of_base.profile.clone_from(&base.profile);
        of_base.range = base.range;
        of_base.base = None;
        if base.profile != query.profile {
            of_base.threads = None;
            of_base.tasks = None;
        }

        Ok(Some((self.entry(&base.profile)?, of_base)))
    }

    fn tree(&self, query: &ProfileQuery) -> Result<(Value, Vec<u8>), Failure> {
        let entry = self.entry(&query.profile)?;
        let granularity = granularity(query.granularity);
        let keys = entry.profile.keys(granularity);
        let tree = keys.aggregate(&resolve(&entry.profile, &keys, query)?);
        let base = self.base(query)?;
        let base_keys = base
            .as_ref()
            .map(|(base, _)| base.profile.keys(granularity));
        let mut bytes = Vec::new();
        let (nodes, base_weight, keys) = if let (Some((base, of_base)), Some(base_keys)) =
            (&base, &base_keys)
        {
            let base_tree = base_keys.aggregate(&resolve_base(&base.profile, base_keys, of_base)?);
            let diff = diff_trees((&keys, &tree), (base_keys, &base_tree));
            let mut named = Named::with_base(&keys, base_keys);
            let columns = [&diff.own, &diff.total, &diff.base_own, &diff.base_total];

            for weights in &columns[..2] {
                put_weights(&mut bytes, weights);
            }

            for (key, side) in diff.key.iter().zip(&diff.side) {
                bytes.extend_from_slice(&named.index_on(*side, *key).to_le_bytes());
            }

            for depth in &diff.depth {
                bytes.extend_from_slice(&depth.to_le_bytes());
            }

            for weights in &columns[2..] {
                put_weights(&mut bytes, weights);
            }

            let weight = ProfileBaseWeight {
                weight: base_tree.weight,
                samples: base_tree.samples,
            };

            (diff.len(), Some(weight), named.keys)
        } else {
            let mut named = Named::new(&keys);
            put_weights(&mut bytes, &tree.own);
            put_weights(&mut bytes, &tree.total);
            for key in &tree.key {
                bytes.extend_from_slice(&named.index(*key).to_le_bytes());
            }

            for depth in &tree.depth {
                bytes.extend_from_slice(&depth.to_le_bytes());
            }

            (tree.len(), None, named.keys)
        };

        let answer = ProfileTree {
            nodes: u32::try_from(nodes).unwrap_or(u32::MAX),
            weight: tree.weight,
            samples: tree.samples,
            keys,
            base: base_weight,
        };

        Ok((to_value(&answer), bytes))
    }

    fn functions(&self, query: &ProfileQuery) -> Result<ProfileFunctions, Failure> {
        let entry = self.entry(&query.profile)?;
        let granularity = granularity(query.granularity);
        let keys = entry.profile.keys(granularity);
        let costs = keys.functions(&resolve(&entry.profile, &keys, query)?);
        let Some((base, of_base)) = self.base(query)? else {
            let mut named = Named::new(&keys);
            let rows = costs
                .rows
                .iter()
                .map(|cost| ProfileCost {
                    key: named.index(cost.key),
                    own: cost.own,
                    total: cost.total,
                    base_own: 0,
                    base_total: 0,
                })
                .collect();
            return Ok(ProfileFunctions {
                weight: costs.weight,
                rows,
                keys: named.keys,
                base: None,
            });
        };

        let base_keys = base.profile.keys(granularity);
        let base_costs = base_keys.functions(&resolve_base(&base.profile, &base_keys, &of_base)?);
        let mut diff = diff_costs((&keys, &costs), (&base_keys, &base_costs));

        // Most changed share of the samples first.
        let share = |weight: u64, of: u64| {
            if of == 0 {
                0.0
            } else {
                exact(weight) / exact(of)
            }
        };

        let change = |cost: &DiffCost| {
            (share(cost.own, costs.weight) - share(cost.base_own, base_costs.weight)).abs()
        };

        diff.sort_by(|a, b| change(b).total_cmp(&change(a)));
        let mut named = Named::with_base(&keys, &base_keys);
        let rows = diff
            .iter()
            .map(|cost| ProfileCost {
                key: named.index_on(cost.side, cost.key),
                own: cost.own,
                total: cost.total,
                base_own: cost.base_own,
                base_total: cost.base_total,
            })
            .collect();

        Ok(ProfileFunctions {
            weight: costs.weight,
            rows,
            keys: named.keys,
            base: Some(ProfileBaseWeight {
                weight: base_costs.weight,
                samples: 0,
            }),
        })
    }

    fn answer(&self, request: Request) -> Result<(Value, Vec<u8>), Failure> {
        match request {
            Request::Profile(protocol::ProfileRef { profile, .. }) => {
                Ok(json(&summary(&*self.entry(&profile)?)))
            }
            Request::ProfileTree(query) => self.tree(&query),
            Request::ProfileFunctions(query) => self.functions(&query).map(|answer| json(&answer)),
            Request::ProfileTimeline(timeline) => {
                let entry = self.entry(&timeline.query.profile)?;
                Ok(json(&timeline_of(&entry.profile, &timeline)?))
            }
            Request::ProfileZones(query) => {
                let entry = self.entry(&query.profile)?;
                Ok(zones(&entry.profile, &query))
            }
            Request::ProfileLines(query) => {
                let entry = self.entry(&query.query.profile)?;
                Ok(json(&source(&entry.profile, &query)?))
            }
            Request::ProfileCode(query) => {
                let entry = self.entry(&query.query.profile)?;
                Ok(json(&code(&entry.profile, &query)?))
            }
            Request::ProfileZoneStats(query) => {
                let entry = self.entry(&query.profile)?;
                Ok(json(&zone_stats(&entry.profile, &query)))
            }
            Request::SyscallInFlight(query) => {
                let entry = self.entry(&query.profile)?;
                let (calls, at) = &entry.in_flight;
                Ok(json(&super::syscalls::in_flight(
                    calls,
                    *at,
                    entry.info.recording,
                )))
            }
            request => {
                let Some(id) = super::syscalls::profile_of(&request) else {
                    return Err(Failure::new(
                        ErrorKind::Failed,
                        "internal: only profile requests come here",
                    ));
                };

                let id = id.to_owned();
                let entry = self.entry(&id)?;
                let trace = entry
                    .syscalls
                    .get_or_init(|| Arc::new(super::syscalls::Trace::of(&entry.profile)));
                super::syscalls::answer(&entry.profile, trace, request)
                    .map(|value| (value, Vec::new()))
            }
        }
    }
}

/// How a recording the page asked for samples.
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

    if let Some(seconds) = spec.window {
        options.window = Some(
            std::time::Duration::try_from_secs_f64(seconds)
                .ok()
                .filter(|window| !window.is_zero())
                .ok_or_else(|| {
                    Failure::new(ErrorKind::Invalid, "a window is a number of seconds")
                })?,
        );
    }

    Ok(options)
}

/// Whether a request is about a profile.
pub const fn is_profile_request(request: &Request) -> bool {
    matches!(
        request,
        Request::Profile(_)
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
            | Request::SyscallRate(_)
            | Request::SyscallHeatmap(_)
            | Request::SyscallDetail(_)
            | Request::SyscallLog(_)
            | Request::SyscallCall(_)
            | Request::SyscallLanes(_)
            | Request::SyscallFiles(_)
            | Request::SyscallNetwork(_)
            | Request::SyscallProcesses(_)
            | Request::SyscallInFlight(_)
    )
}

/// Fails unless `role` may run programs.
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

/// Each span sampling paused for `reason`, from `start`.
fn paused(profile: &Profile, reason: GapReason, start: i64) -> Vec<[i64; 2]> {
    profile
        .paused(reason)
        .into_iter()
        .map(|(from, to)| [from - start, to - start])
        .collect()
}

fn summary(entry: &Entry) -> ProfileSummary {
    let profile = &entry.profile;
    let (start, end) = profile.span();
    ProfileSummary {
        id: entry.info.id.clone(),
        name: entry.info.name.clone(),
        program: profile.setting("program").map(str::to_owned),
        unit: profile
            .setting("weight")
            .unwrap_or("nanoseconds")
            .to_owned(),
        source: profile.setting("source").map(str::to_owned),
        timed: profile.timed(),
        samples: profile.samples() as u64,
        weight: profile.total_weight(),
        syscalls: profile.syscall_count(),
        syscalls_lost: profile.syscalls_lost(),
        duration: end - start,
        threads: profile
            .threads()
            .into_iter()
            .map(|thread| protocol::ProfileThread {
                row: thread.row,
                pid: thread.pid,
                tid: thread.tid,
                name: thread.name,
                samples: thread.samples,
                weight: thread.weight,
                blocked: thread.blocked,
                preempted: thread.preempted,
            })
            .collect(),
        lost: profile.lost_samples(),
        throttles: profile.throttles(),
        recorder_cpu: profile.recorder_cpu().0,
        recorder_loading: profile.recorder_cpu().1,
        gaps: paused(profile, GapReason::Budget, start),
        behind: paused(profile, GapReason::Behind, start),
        budget_changes: markers(profile.budget_changes(), start),
        tasks: tasks(profile),
        interpreted: profile.interpreted(),
        snapshots: profile.task_snapshots().0,
        snapshot_pause: profile.task_snapshots().1,
        task_samples: profile.task_samples(),
        snapshot_problems: markers(profile.task_snapshot_problems(), start),
        waited: profile.task_waits().iter().map(|wait| wait.weight).sum(),
        probes: {
            let counts = profile.probe_counts();
            protocol::ProfileProbes {
                events: counts.events,
                overwritten: counts.overwritten,
                no_ring: counts.no_ring,
                interrupted: counts.interrupted,
                malformed: counts.malformed,
                plotted: counts.plotted,
            }
        },
        zone_sites: profile
            .zone_sites()
            .into_iter()
            .map(|site| protocol::ProfileZoneSite {
                name: site.name,
                label: site.label,
                count: site.count,
                total: site.total,
                longest: site.longest,
                flagged: site.flagged,
            })
            .collect(),
        label_keys: {
            let keys = profile.keys(Granularity::Function);
            let mut names: Vec<String> = keys
                .labels()
                .iter()
                .filter(|(_, value)| value.is_none())
                .map(|(key, _)| (*key).to_owned())
                .collect();
            names.sort();
            names
        },
        probe_problems: markers(profile.probe_problems(), start),
        truncations: profile
            .truncations()
            .into_iter()
            .map(|(what, count)| protocol::ProfileCount { what, count })
            .collect(),
        settings: profile
            .settings()
            .into_iter()
            .map(|(name, value)| ProfileSetting {
                name: name.to_owned(),
                value: value.to_owned(),
            })
            .collect(),
    }
}

/// Markers as a page shows them, from the profile's `start`.
fn markers(markers: Vec<(i64, String)>, start: i64) -> Vec<protocol::ProfileMarker> {
    markers
        .into_iter()
        .map(|(time, text)| protocol::ProfileMarker {
            time: time - start,
            text,
        })
        .collect()
}

/// A profile's tasks, named as views name their groups, most weight
/// first.
fn tasks(profile: &Profile) -> Vec<protocol::ProfileTask> {
    let keys = profile.keys(Granularity::Function);
    let mut tasks: Vec<protocol::ProfileTask> = profile
        .tasks()
        .into_iter()
        .map(|task| protocol::ProfileTask {
            row: task.row,
            name: keys.name(keys.group(task.row)),
            weight: task.weight,
            waited: task.waited,
        })
        .collect();

    tasks.sort_by(|a, b| (b.weight, b.waited, a.row).cmp(&(a.weight, a.waited, b.row)));
    tasks
}

const fn granularity(granularity: ProfileGranularity) -> Granularity {
    match granularity {
        ProfileGranularity::Function => Granularity::Function,
        ProfileGranularity::Physical => Granularity::PhysicalFunction,
        ProfileGranularity::Line => Granularity::Line,
        ProfileGranularity::Address => Granularity::Address,
    }
}

/// The analysis's query for a tab's, its transforms' names resolved to
/// keys. A name no key has is an error rather than a transform that
/// silently does nothing, and so is a focus on a name several keys share.
fn resolve(profile: &Profile, keys: &Keys<'_>, query: &ProfileQuery) -> Result<Query, Failure> {
    resolve_in(profile, keys, query, false)
}

/// Resolves a query in a diff's base, where a name the base lacks is no
/// error: a focus on it keeps nothing, and the others change nothing.
fn resolve_base(
    profile: &Profile,
    keys: &Keys<'_>,
    query: &ProfileQuery,
) -> Result<Query, Failure> {
    resolve_in(profile, keys, query, true)
}

fn resolve_in(
    profile: &Profile,
    keys: &Keys<'_>,
    query: &ProfileQuery,
    base: bool,
) -> Result<Query, Failure> {
    let (start, _) = profile.span();
    let mut transforms = Vec::new();
    for transform in &query.transforms {
        if transform.op == TransformOp::CollapseCycles {
            transforms.push(Transform::CollapseCycles);
            continue;
        }

        let named = keys.named(&transform.name);
        if named.is_empty() && base {
            if transform.op == TransformOp::Focus {
                transforms.push(Transform::Focus(NO_KEY));
            }

            continue;
        }

        if named.is_empty() {
            return Err(Failure::new(
                ErrorKind::Invalid,
                match keys.similar(&transform.name, 5).as_slice() {
                    [] => format!("nothing in the profile is named {}", transform.name),
                    similar => format!(
                        "nothing in the profile is named {}; names that contain it: {}",
                        transform.name,
                        similar
                            .iter()
                            .map(|&key| keys.exact_name(key))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                },
            ));
        }

        if transform.op == TransformOp::Focus && named.len() > 1 {
            return Err(Failure::new(
                ErrorKind::Invalid,
                format!("{} things are named {}", named.len(), transform.name),
            ));
        }

        transforms.extend(named.into_iter().map(|key| match transform.op {
            TransformOp::Focus => Transform::Focus(key),
            TransformOp::Merge => Transform::Merge(key),
            TransformOp::Drop => Transform::Drop(key),
            TransformOp::Collapse => Transform::CollapseRecursion(key),
            TransformOp::CollapseCycles => Transform::CollapseCycles,
        }));
    }

    Ok(Query {
        source: match query.source {
            ProfileSource::Samples => Source::Samples,
            ProfileSource::Waits => Source::Waits,
        },
        range: query
            .range
            .map(|[from, to]| (start.saturating_add(from), start.saturating_add(to))),
        threads: query.threads.clone(),
        tasks: query
            .tasks
            .as_ref()
            .map(|tasks| tasks.iter().map(|task| task.unwrap_or(NO_TASK)).collect()),
        labels: query
            .labels
            .iter()
            .map(|label| {
                label
                    .split_once('=')
                    .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    .ok_or_else(|| {
                        Failure::new(ErrorKind::Invalid, format!("{label} is not key=value"))
                    })
            })
            .collect::<Result<_, _>>()?,
        by_label: query.by_label.clone(),
        transforms,
        invert: query.invert,
        look: match query.look {
            ProfileLook::Logical => Look::Logical,
            ProfileLook::Native => Look::Native,
            ProfileLook::Python => Look::Python,
        },
        by_task: query.by_task,
    })
}

/// Each thread's, or task's, weight over time, and its time off a CPU.
fn timeline_of(
    profile: &Profile,
    timeline: &protocol::ProfileTimelineQuery,
) -> Result<ProfileTimeline, Failure> {
    let keys = profile.keys(granularity(timeline.query.granularity));
    let mut resolved = resolve(profile, &keys, &timeline.query)?;
    let (start, end) = profile.span();
    let range = resolved
        .range
        .unwrap_or_else(|| (start, end.saturating_add(1)));
    resolved.range = Some(range);
    let buckets = timeline.buckets.clamp(1, MOST_BUCKETS) as usize;
    let by_task = timeline.task_lanes || timeline.query.source == ProfileSource::Waits;
    let of = if by_task {
        Lanes::Tasks
    } else {
        Lanes::Threads
    };

    let lane = |id: u32, weights| ProfileLane {
        thread: (!by_task).then_some(id),
        task: (by_task && id != NO_TASK).then_some(id),
        weights,
        blocked: Vec::new(),
        preempted: Vec::new(),
    };

    let mut lanes: std::collections::BTreeMap<u32, ProfileLane> = keys
        .density(&resolved, buckets, of)
        .into_iter()
        .map(|(id, weights)| (id, lane(id, weights)))
        .collect();

    // Time off a CPU is a thread's.
    if !by_task {
        for off in profile.off_cpu_lanes(&resolved, buckets) {
            let lane = lanes
                .entry(off.thread)
                .or_insert_with(|| lane(off.thread, Vec::new()));
            lane.blocked = off.blocked;
            lane.preempted = off.preempted;
        }
    }

    let lanes = lanes.into_values().collect();
    Ok(ProfileTimeline {
        range: [range.0 - start, range.1 - start],
        lanes,
    })
}

/// The most of a source file served, and how many lines around a
/// function's own are.
const MOST_SOURCE_BYTES: u64 = 4 << 20;
const SOURCE_MARGIN: u64 = 5;

/// A function query's selection, its transforms left out.
fn selection(profile: &Profile, query: &protocol::ProfileFunctionQuery) -> Result<Query, Failure> {
    let mut plain = query.query.clone();
    plain.transforms.clear();
    plain.base = None;
    let keys = profile.keys(granularity(plain.granularity));
    resolve(profile, &keys, &plain)
}

/// A function's samples on its source lines, with the text around them.
fn source(
    profile: &Profile,
    query: &protocol::ProfileFunctionQuery,
) -> Result<protocol::ProfileLines, Failure> {
    let costs = profile.line_costs(&selection(profile, query)?, &query.function);
    let mut files: BTreeMap<Option<String>, Vec<[u64; 3]>> = BTreeMap::new();
    for line in costs.lines {
        files
            .entry(line.file)
            .or_default()
            .push([u64::from(line.line), line.own, line.total]);
    }

    let files = files
        .into_iter()
        .map(|(path, lines)| {
            let lowest = lines.iter().map(|line| line[0]).min().unwrap_or(1);
            let highest = lines.iter().map(|line| line[0]).max().unwrap_or(1);
            let first = lowest.saturating_sub(SOURCE_MARGIN).max(1);
            let read = path.as_deref().map_or_else(
                || Err("its lines name no file".to_owned()),
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
        .collect();

    Ok(protocol::ProfileLines {
        weight: costs.weight,
        files,
    })
}

/// Lines `first` to `last` of a source file, as it reads now.
pub(super) fn source_lines(path: &str, first: u64, last: u64) -> Result<Vec<String>, String> {
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

/// A function's samples on its instructions, or why its code is unknown.
fn code(
    profile: &Profile,
    query: &protocol::ProfileFunctionQuery,
) -> Result<protocol::ProfileCode, Failure> {
    let selected = selection(profile, query)?;
    Ok(match profile.disassemble(&selected, &query.function) {
        Ok(code) => protocol::ProfileCode {
            weight: code.weight,
            module: Some(code.module),
            blocks: code
                .blocks
                .into_iter()
                .map(|block| protocol::ProfileCodeBlock {
                    symbol: block.symbol,
                    instructions: block
                        .instructions
                        .into_iter()
                        .map(|instruction| protocol::ProfileInstruction {
                            address: instruction.address,
                            text: instruction.text,
                            target: instruction.target,
                            own: instruction.own,
                            total: instruction.total,
                        })
                        .collect(),
                    stopped: block.stopped,
                })
                .collect(),
            unplaced: code.unplaced.into_iter().map(Into::into).collect(),
            unavailable: None,
        },
        Err(why) => protocol::ProfileCode {
            weight: 0,
            module: None,
            blocks: Vec::new(),
            unplaced: Vec::new(),
            unavailable: Some(why),
        },
    })
}

/// The most marks a zones answer holds.
const MOST_MARKS: usize = 4096;

/// A range from the profile's start, as the profile's clock has it, or the
/// whole profile.
fn absolute(profile: &Profile, range: Option<[i64; 2]>) -> (i64, i64) {
    let (start, end) = profile.span();
    range.map_or_else(
        || (start, end.saturating_add(1)),
        |[from, to]| (start.saturating_add(from), start.saturating_add(to)),
    )
}

/// Zones folded to a width, and the program's marks, in a range.
fn zones(profile: &Profile, query: &protocol::ProfileZonesQuery) -> (Value, Vec<u8>) {
    let (start, _) = profile.span();

    // Without a range, the zones' own, with a margin.
    let range = match (query.range, profile.zone_extent()) {
        (None, Some((first, last))) => {
            let margin = ((last - first) / 50).max(1);
            (first - margin, last + margin)
        }
        (range, _) => absolute(profile, range),
    };

    // Zones under three pixels wide fold together.
    let fold = (range.1 - range.0) * 3 / i64::from(query.width.max(1));
    let blocks = profile.zone_blocks(range, query.threads.as_deref(), fold.max(1));
    let mut names: Vec<String> = Vec::new();
    let mut indices: BTreeMap<(u32, u32), u32> = BTreeMap::new();
    let mut name = |site: u32, value: u32| {
        *indices.entry((site, value)).or_insert_with(|| {
            names.push(profile.zone_name(site, value));
            u32::try_from(names.len() - 1).unwrap_or(u32::MAX)
        })
    };

    let mut bytes = Vec::with_capacity(blocks.len() * 36);
    for block in &blocks {
        bytes.extend_from_slice(&exact_time(block.start - start).to_le_bytes());
    }

    for block in &blocks {
        bytes.extend_from_slice(&exact_time(block.end - start).to_le_bytes());
    }

    let columns: [&dyn Fn(&microscope::profiler::ZoneBlock) -> u32; 2] =
        [&|block| block.thread, &|block| block.depth];
    for column in columns {
        for block in &blocks {
            bytes.extend_from_slice(&column(block).to_le_bytes());
        }
    }

    for block in &blocks {
        bytes.extend_from_slice(&name(block.site, block.value).to_le_bytes());
    }

    for block in &blocks {
        bytes.extend_from_slice(&block.count.to_le_bytes());
    }

    for block in &blocks {
        bytes.extend_from_slice(&block.flags.to_le_bytes());
    }

    let marks = profile
        .program_markers()
        .into_iter()
        .filter(|marker| range.0 <= marker.time && marker.time < range.1)
        .filter_map(|marker| {
            Some(protocol::ProfileMark {
                thread: marker.thread?,
                time: marker.time - start,
                kind: match marker.kind {
                    MarkerKind::Frame => protocol::ProfileMarkKind::Frame,
                    MarkerKind::Trigger => protocol::ProfileMarkKind::Trigger,
                    _ => protocol::ProfileMarkKind::Message,
                },
                name: marker.name,
                text: marker.text,
            })
        })
        .take(MOST_MARKS)
        .collect();
    let plots = profile
        .plot_series(range, query.threads.as_deref(), query.width.max(1) as usize)
        .into_iter()
        .map(|series| protocol::ProfilePlot {
            name: profile.string(series.name).to_owned(),
            values: series.values,
            unshown: series.unshown,
            bins: series
                .bins
                .iter()
                .map(|bin| {
                    let time = exact_time(bin.time - start);
                    [time, bin.first, bin.last, bin.low, bin.high]
                })
                .collect(),
            held: series.held,
            continues: series.continues,
        })
        .collect();

    let answer = protocol::ProfileZones {
        range: [range.0 - start, range.1 - start],
        blocks: u32::try_from(blocks.len()).unwrap_or(u32::MAX),
        names,
        marks,
        plots,
    };

    (to_value(&answer), bytes)
}

fn zone_stats(
    profile: &Profile,
    query: &protocol::ProfileZoneStatsQuery,
) -> protocol::ProfileZoneStats {
    let (start, _) = profile.span();
    let range = absolute(profile, query.range);
    let stats = profile.zone_stats(&query.name, range, query.threads.as_deref());
    let value = |value: u32| profile.label_value(value).map(str::to_owned);

    protocol::ProfileZoneStats {
        count: stats.count,
        total: stats.total,
        flagged: stats.flagged,
        min: stats.min,
        median: stats.median,
        p90: stats.p90,
        p99: stats.p99,
        max: stats.max,
        histogram: stats
            .histogram
            .iter()
            .map(|&bucket| bucket.into())
            .collect(),
        longest: stats
            .longest
            .iter()
            .map(|zone| protocol::ProfileZoneAt {
                thread: zone.thread,
                start: zone.start - start,
                end: zone.end - start,
                value: value(zone.value),
            })
            .collect(),
        values: stats
            .values
            .iter()
            .map(|&(text, total, count)| protocol::ProfileLabelValue {
                value: value(text).unwrap_or_default(),
                total,
                count,
            })
            .collect(),
    }
}

/// A time as a page's numbers hold it, exactly to 2^53 nanoseconds.
#[expect(
    clippy::cast_precision_loss,
    reason = "no view can show a part in 2^53"
)]
const fn exact_time(time: i64) -> f64 {
    time as f64
}

/// The keys an answer names, each once, in the order first named.
struct Named<'a, 'k> {
    keys_of: &'a Keys<'k>,

    /// A diff's base's keys, which its nodes from the base name.
    base_of: Option<&'a Keys<'k>>,
    indices: BTreeMap<(bool, u32), u32>,
    keys: Vec<ProfileKey>,
}

impl<'a, 'k> Named<'a, 'k> {
    const fn new(keys_of: &'a Keys<'k>) -> Self {
        Self {
            keys_of,
            base_of: None,
            indices: BTreeMap::new(),
            keys: Vec::new(),
        }
    }

    const fn with_base(keys_of: &'a Keys<'k>, base_of: &'a Keys<'k>) -> Self {
        Self {
            keys_of,
            base_of: Some(base_of),
            indices: BTreeMap::new(),
            keys: Vec::new(),
        }
    }

    fn index(&mut self, key: u32) -> u32 {
        self.index_on(Side::Current, key)
    }

    fn index_on(&mut self, side: Side, key: u32) -> u32 {
        let from_base = side == Side::Base;
        if let Some(index) = self.indices.get(&(from_base, key)) {
            return *index;
        }

        let keys = match (side, self.base_of) {
            (Side::Base, Some(base)) => base,
            _ => self.keys_of,
        };

        let index = u32::try_from(self.keys.len()).unwrap_or(u32::MAX);
        let location = keys.location(key);
        self.keys.push(ProfileKey {
            name: keys.exact_name(key),
            kind: match keys.kind(key) {
                KeyKind::Function { .. } => ProfileKeyKind::Function,
                KeyKind::Line { .. } => ProfileKeyKind::Line,
                KeyKind::Address { .. } => ProfileKeyKind::Address,
                KeyKind::Truncation { .. } => ProfileKeyKind::Truncation,
                KeyKind::Task { .. } => ProfileKeyKind::Task,
                KeyKind::Group { .. } => ProfileKeyKind::Group,
                KeyKind::Label { .. } => ProfileKeyKind::Label,
            },
            module: keys.module(key).map(str::to_owned),
            file: location.map(|(file, _)| file.to_owned()),
            line: location.map(|(_, line)| line),
        });
        self.indices.insert((from_base, key), index);
        index
    }
}

/// Weights as a column of little-endian `f64`s.
fn put_weights(bytes: &mut Vec<u8>, weights: &[u64]) {
    for weight in weights {
        bytes.extend_from_slice(&exact(*weight).to_le_bytes());
    }
}

/// A weight as a page's numbers hold it: exactly up to 2^53, nanoseconds
/// of more than a hundred days, and beyond to within a part in 2^53.
#[expect(
    clippy::cast_precision_loss,
    reason = "no view can show a part in 2^53"
)]
const fn exact(weight: u64) -> f64 {
    weight as f64
}

/// An answer that is only JSON.
fn json(value: &impl serde::Serialize) -> (Value, Vec<u8>) {
    (to_value(value), Vec::new())
}

fn to_value(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or_else(|error| json!({ "error": error.to_string() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recording can end before the thread that started it lists it, as
    /// one whose program fails to start does. It is not listed after.
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
                .is_empty(),
            "an ended recording is not listed"
        );
        profiles.shutdown();
    }
}
