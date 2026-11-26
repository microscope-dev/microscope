use std::{
    fmt::Write as _,
    os::fd::AsFd as _,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Condvar, Mutex, PoisonError},
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result};
use nix::{
    poll::{PollFd, PollFlags, PollTimeout, poll},
    sys::{
        signal::{SigSet, Signal},
        signalfd::{SfdFlags, SignalFd},
    },
};

use super::{
    client::{self, Client, PollReply, Reply},
    cloud::{self, Cloud},
    config::{self, Config},
    control::{ControlAction, ControlEffect, ControlMachine, ControlOutcome},
    host::{self, Root},
    inventory,
    machine::{Action, Effect, Instant, Machine, Outcome, State},
    recorder::{self, Pipeline, PipelineControl},
    store::{Enrollment, Status, Store, Stored},
    wire::{KeyPair, PreviousRun},
};

const CONFIG_CHECK: Duration = Duration::from_secs(30);

pub struct Signals {
    fd: SignalFd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    Stop,
    Reload,
}

impl Signals {
    pub fn block() -> Result<Self> {
        let mut set = SigSet::empty();
        set.add(Signal::SIGTERM);
        set.add(Signal::SIGINT);
        set.add(Signal::SIGHUP);
        set.thread_block()
            .context("cannot block SIGTERM, SIGINT, and SIGHUP")?;
        let fd = SignalFd::with_flags(&set, SfdFlags::SFD_CLOEXEC | SfdFlags::SFD_NONBLOCK)
            .context("cannot open a signalfd")?;
        Ok(Self { fd })
    }

    #[must_use]
    pub fn wait(&self, timeout: Duration) -> Option<Wake> {
        let timeout = PollTimeout::try_from(timeout).unwrap_or(PollTimeout::MAX);
        let mut fds = [PollFd::new(self.fd.as_fd(), PollFlags::POLLIN)];
        if poll(&mut fds, timeout).unwrap_or(0) == 0 {
            return None;
        }
        let mut wake = None;
        while let Ok(Some(info)) = self.fd.read_signal() {
            let signal = i32::try_from(info.ssi_signo)
                .ok()
                .and_then(|n| Signal::try_from(n).ok());
            wake = match signal {
                Some(Signal::SIGHUP) if wake.is_none() => Some(Wake::Reload),
                Some(Signal::SIGHUP) => wake,
                _ => Some(Wake::Stop),
            };
        }
        wake
    }
}

struct ControlBridgeState {
    stop: bool,
    token: Option<String>,
    need_token: bool,
    revoked: bool,
    reset_seq: u64,
    pipeline: Option<PipelineControl>,
}

struct PollHandle {
    bridge: Arc<(Mutex<ControlBridgeState>, Condvar)>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PollHandle {
    fn start(config: &Config, pipeline: Option<PipelineControl>) -> Result<Self> {
        let client = Client::new(config.server.clone(), config.ca_file.as_deref())?;
        let (projects, services) = config.record.as_ref().map_or_else(
            || (Vec::new(), Vec::new()),
            |rec| {
                if rec.process.is_empty() {
                    (Vec::new(), Vec::new())
                } else {
                    (vec![rec.project.clone()], vec![rec.service.clone()])
                }
            },
        );
        let limits = config.limits.to_wire();
        let bridge = Arc::new((
            Mutex::new(ControlBridgeState {
                stop: false,
                token: None,
                need_token: false,
                revoked: false,
                reset_seq: 0,
                pipeline,
            }),
            Condvar::new(),
        ));
        let worker_bridge = Arc::clone(&bridge);
        let handle = thread::Builder::new()
            .name("microscope-agent-poll".into())
            .spawn(move || {
                poll_loop(&worker_bridge, &client, projects, services, limits);
            })?;
        Ok(Self {
            bridge,
            thread: Some(handle),
        })
    }

    fn set_token(&self, token: Option<String>) {
        let (lock, cvar) = &*self.bridge;
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        guard.token = token;
        if guard.token.is_some() {
            guard.need_token = false;
        }
        drop(guard);
        cvar.notify_all();
    }

    fn reset_for_enrollment(&self, token: Option<String>, pipeline: Option<PipelineControl>) {
        let (lock, cvar) = &*self.bridge;
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        guard.token = token;
        guard.pipeline = pipeline;
        guard.revoked = false;
        if guard.token.is_some() {
            guard.need_token = false;
        }
        guard.reset_seq = guard.reset_seq.saturating_add(1);
        drop(guard);
        cvar.notify_all();
    }

    fn set_pipeline(&self, pipeline: Option<PipelineControl>) {
        let (lock, cvar) = &*self.bridge;
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        guard.pipeline = pipeline;
        drop(guard);
        cvar.notify_all();
    }

    fn take_revoked(&self) -> bool {
        let (lock, _) = &*self.bridge;
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let rev = guard.revoked;
        guard.revoked = false;
        rev
    }

    fn take_need_token(&self) -> bool {
        let (lock, _) = &*self.bridge;
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let needed = guard.need_token;
        guard.need_token = false;
        needed
    }

    fn stop(&mut self) {
        let (lock, cvar) = &*self.bridge;
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        guard.stop = true;
        guard.token = None;
        guard.pipeline = None;
        drop(guard);
        cvar.notify_all();
        let _ = self.thread.take();
    }
}

impl Drop for PollHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "runs the long-poll control loop with revocation recovery and pipeline sync"
)]
fn poll_loop(
    bridge: &(Mutex<ControlBridgeState>, Condvar),
    client: &Client,
    projects: Vec<String>,
    services: Vec<String>,
    limits: super::wire::AgentLimits,
) {
    let started = std::time::Instant::now();
    let now = || Instant(started.elapsed());
    let mut machine = ControlMachine::new(now(), projects, services);
    machine.set_limits(Some(limits));
    let (lock, cvar) = bridge;
    let mut last_reset_seq = 0_u64;

    loop {
        let (stop, token, pipeline, reset_seq) = {
            let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            (
                guard.stop,
                guard.token.clone(),
                guard.pipeline.clone(),
                guard.reset_seq,
            )
        };
        if stop {
            return;
        }
        if reset_seq != last_reset_seq {
            last_reset_seq = reset_seq;
            machine.reset(now());
        }
        if let Some(p) = &pipeline {
            let done = p.completed_flush_id();
            if done > machine.last_flush_id() {
                machine.mark_flush_done(done, now());
            }
            let empty_ack = p.empty_flush_id();
            if empty_ack > machine.empty_flush_id() {
                machine.mark_empty_flush(empty_ack, now());
            }
            let (pv, sv) = p.applied_policy_versions();
            machine.set_applied_policy(pv, sv);
            let (projs, svcs) = p.active_targets();
            if !projs.is_empty() || !svcs.is_empty() {
                machine.set_targets(projs, svcs);
            }
        }
        if token.is_some() && machine.needs_token() {
            machine.token_refreshed(now());
        }

        match machine.next(now(), token.is_some()) {
            ControlAction::Revoked => {
                let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                if guard.stop {
                    return;
                }
                if guard.reset_seq == last_reset_seq {
                    let _ = cvar.wait_timeout(guard, Duration::from_millis(200));
                }
            }
            ControlAction::NeedToken => {
                let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                if guard.stop {
                    return;
                }
                if guard.token.is_none() {
                    guard.need_token = true;
                }
                let _ = cvar.wait_timeout(guard, Duration::from_millis(200));
            }
            ControlAction::Wait { until } => {
                let wait = now().until(until).min(Duration::from_millis(200));
                let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                if guard.stop {
                    return;
                }
                let _ = cvar.wait_timeout(guard, wait);
            }
            ControlAction::Poll(req) => {
                let Some(tok) = token else {
                    continue;
                };
                let req_reset_seq = last_reset_seq;
                let reply = client.poll(&tok, &req);
                let stale = {
                    let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                    guard.stop || guard.reset_seq != req_reset_seq
                };
                if stale {
                    continue;
                }
                let outcome = match reply {
                    PollReply::Directive(resp) => ControlOutcome::Directive(resp),
                    PollReply::Unauthorized => {
                        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                        if guard.stop || guard.reset_seq != req_reset_seq {
                            continue;
                        }
                        if guard.token.as_deref() == Some(tok.as_str()) {
                            guard.token = None;
                            guard.need_token = true;
                        }
                        drop(guard);
                        ControlOutcome::Unauthorized
                    }
                    PollReply::Refused { retry_after, .. } => ControlOutcome::Retry { retry_after },
                    PollReply::Unreachable(_) => ControlOutcome::Retry { retry_after: None },
                };
                let effects = machine.on(outcome, now(), jitter());
                for effect in effects {
                    match effect {
                        ControlEffect::Revoked => {
                            let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                            if !guard.stop && guard.reset_seq == req_reset_seq {
                                guard.revoked = true;
                                guard.token = None;
                            }
                            drop(guard);
                        }
                        ControlEffect::SetWatch(watch) => {
                            if let Some(p) = &pipeline {
                                p.apply_watch(watch);
                            }
                        }
                        ControlEffect::Flush(flush) => {
                            if let Some(p) = &pipeline {
                                p.request_flush(flush.id, flush.deadline_ms);
                            } else {
                                machine.mark_empty_flush(flush.id, now());
                            }
                        }
                        ControlEffect::ApplyPolicy(policy) => {
                            if let Some(p) = &pipeline {
                                p.apply_policy(policy);
                            } else {
                                machine
                                    .set_applied_policy(policy.version, policy.sanitizer_version);
                            }
                        }
                        ControlEffect::Bursts(bursts) => {
                            if let Some(p) = &pipeline {
                                p.apply_bursts(bursts);
                            }
                        }
                    }
                }
            }
        }
    }
}

struct Agent {
    config: Config,
    store: Store,
    client: Client,
    root: Root,
    boot_id: String,
    cloud: Option<Cloud>,
    machine: Machine,
    key: Option<KeyPair>,
    token: Option<String>,
    previous_run: Option<PreviousRun>,
    status: Status,
    started: std::time::Instant,
    last_logged: Option<(&'static str, Option<String>)>,
    pipeline: Option<Pipeline>,
    poll: Option<PollHandle>,
}

fn log(message: &str) {
    super::log(format_args!("{message}"));
}

fn jitter() -> f64 {
    let mut bytes = [0; 4];
    let _ = getrandom::fill(&mut bytes);
    f64::from(u32::from_le_bytes(bytes)) / f64::from(u32::MAX)
}

fn random_boot_id() -> String {
    let mut bytes = [0_u8; 16];
    let _ = getrandom::fill(&mut bytes);
    let hex = super::wire::hex(&bytes);
    format!(
        "{}-{}-{}-{}-{}",
        hex.get(..8).unwrap_or_default(),
        hex.get(8..12).unwrap_or_default(),
        hex.get(12..16).unwrap_or_default(),
        hex.get(16..20).unwrap_or_default(),
        hex.get(20..).unwrap_or_default()
    )
}

fn route_target(config: &Config) -> String {
    let origin = config.server.origin();
    let (scheme, host) = origin.split_once("://").unwrap_or(("https", origin));
    let has_port = host.rsplit_once(':').is_some_and(|(before, port)| {
        !before.ends_with(':')
            && port.bytes().all(|b| b.is_ascii_digit())
            && (!host.starts_with('[') || before.ends_with(']'))
    });
    if has_port {
        host.to_owned()
    } else if scheme == "http" {
        format!("{host}:80")
    } else {
        format!("{host}:443")
    }
}

impl Agent {
    fn start(config: Config) -> Result<Self> {
        let store = Store::open(&config.state_dir)?;
        let client = Client::new(config.server.clone(), config.ca_file.as_deref())?;
        let root = Root::system();
        let boot_id = host::boot_id(&root).unwrap_or_else(random_boot_id);

        let previous_run = match store.marker()? {
            None => PreviousRun::Clean,
            Some(marked) if marked == boot_id => PreviousRun::AgentDied,
            Some(_) => PreviousRun::HostDied,
        };
        store.mark_running(&boot_id)?;

        let key = match store.key()? {
            Stored::Found(key) => Some(key),
            Stored::Missing => None,
            Stored::Damaged => {
                log("the stored key is damaged; making a new one");
                store.forget()?;
                None
            }
        };
        let enrolled = match (store.enrollment()?, &key) {
            (Stored::Found(enrollment), Some(_)) if enrollment.server == config.server.origin() => {
                Some(enrollment.agent_id)
            }
            (Stored::Found(enrollment), _) => {
                log(&format!(
                    "the stored key belongs to {}; enrolling with {} instead",
                    enrollment.server, config.server
                ));
                store.forget()?;
                None
            }
            _ => None,
        };
        let key = if enrolled.is_some() { key } else { None };

        let cloud = if config.cloud_metadata {
            cloud::detect(&root)
                .and_then(|provider| cloud::fetch(provider, cloud::METADATA, cloud::TIMEOUT))
        } else {
            None
        };

        let started = std::time::Instant::now();
        let status = Status {
            server: config.server.origin().to_owned(),
            agent_id: enrolled,
            fingerprint: key.as_ref().map(|key| key.public_key().fingerprint()),
            state: "starting".to_owned(),
            since: client::unix_now(),
            pid: std::process::id(),
            running: true,
            ..Status::default()
        };

        let pipeline = if let Some(record) = config.record.as_ref() {
            let host_name = config
                .node
                .clone()
                .unwrap_or_else(|| recorder::host_name_from(Path::new("/")));
            Some(Pipeline::start(
                &config,
                record,
                host_name,
                enrolled,
                enrolled.is_some(),
            )?)
        } else {
            None
        };

        let poll = Some(PollHandle::start(
            &config,
            pipeline.as_ref().map(Pipeline::control),
        )?);

        Ok(Self {
            machine: Machine::new(Instant(Duration::ZERO), enrolled, config.server.loopback()),
            config,
            store,
            client,
            root,
            boot_id,
            cloud,
            key,
            token: None,
            previous_run: Some(previous_run),
            status,
            started,
            last_logged: None,
            pipeline,
            poll,
        })
    }

    fn now(&self) -> Instant {
        Instant(self.started.elapsed())
    }

    fn inventory(&self) -> Vec<u8> {
        let mut gathered = inventory::gather(
            &self.config,
            &self.root,
            self.cloud.as_ref(),
            &route_target(&self.config),
        );
        inventory::encode(&mut gathered)
    }

    fn save_status(&mut self, error: Option<String>) {
        let state = self.machine.name();
        if self.status.state != state {
            self.status.since = client::unix_now();
        }
        self.status.state = state.to_owned();
        self.status.agent_id = self.machine.agent_id().or(self.status.agent_id);
        self.status.fingerprint = self.key.as_ref().map(|key| key.public_key().fingerprint());
        if error.is_some() || matches!(self.machine.state(), State::Enrolled { .. }) {
            self.status.last_error.clone_from(&error);
        }
        if let Err(store_error) = self.store.save_status(&self.status) {
            log(&format!(
                "cannot write {}: {store_error}",
                self.store.dir().join("status.json").display()
            ));
        }

        let logged = (state, error);
        if self.last_logged.as_ref().map(|(state, _)| *state) != Some(logged.0) {
            match (&logged.0, &logged.1, self.machine.agent_id()) {
                (_, Some(error), _) => log(&format!("{state}: {error}")),
                (&"enrolled", None, Some(agent_id)) => log(&format!(
                    "enrolled with {} as agent {agent_id}, key {}",
                    self.config.server,
                    self.status.fingerprint.as_deref().unwrap_or("?")
                )),
                _ => log(state),
            }
        }
        self.last_logged = Some(logged);
    }

    fn apply(&mut self, reply: &Reply, refreshing: bool) {
        let (outcome, error) = client::outcome(reply);
        if let Reply::Token(token) = reply {
            self.token = Some(token.token.clone());
            if !refreshing
                && self.pipeline.is_none()
                && let Some(record) = self.config.record.as_ref()
            {
                let host_name = self
                    .config
                    .node
                    .clone()
                    .unwrap_or_else(|| recorder::host_name_from(Path::new("/")));
                match Pipeline::start(&self.config, record, host_name, Some(token.agent_id), false)
                {
                    Ok(pipeline) => self.pipeline = Some(pipeline),
                    Err(error) => {
                        log(&format!("cannot start the recording pipeline: {error:#}"));
                    }
                }
            }
            if let Some(pipeline) = &self.pipeline {
                pipeline.set_auth(Some(token.agent_id), Some(token.token.clone()));
            }
            if let Some(poll) = &self.poll {
                if refreshing {
                    poll.set_token(Some(token.token.clone()));
                } else {
                    poll.reset_for_enrollment(
                        Some(token.token.clone()),
                        self.pipeline.as_ref().map(Pipeline::control),
                    );
                }
            }
            self.status.last_refresh = Some(client::unix_now());
            if refreshing {
                self.previous_run = None;
            }
            let enrollment = Enrollment {
                agent_id: token.agent_id,
                server: self.config.server.origin().to_owned(),
            };
            if !refreshing && let Err(store_error) = self.store.save_enrollment(&enrollment) {
                log(&format!("cannot save the enrollment: {store_error}"));
            }
        }

        let now = self.now();
        if self.machine.on(outcome, now, jitter()) == Effect::ForgetKey {
            self.key = None;
            self.token = None;
            if let Some(pipeline) = &self.pipeline {
                pipeline.set_auth(None, None);
            }
            if let Some(poll) = &self.poll {
                poll.set_token(None);
            }
            if let Err(store_error) = self.store.forget() {
                log(&format!("cannot delete the key: {store_error}"));
            }
        }
        self.save_status(error);
    }

    fn enroll(&mut self, previous: Option<i64>) {
        let key = match self.key.take() {
            Some(key) => key,
            None => match KeyPair::generate().and_then(|key| {
                self.store.save_key(&key)?;
                Ok(key)
            }) {
                Ok(key) => key,
                Err(error) => {
                    self.apply(
                        &Reply::Unreachable(format!("cannot make a key: {error}")),
                        false,
                    );
                    return;
                }
            },
        };
        let inventory = self.inventory();
        let reply = self.client.enroll(
            &key,
            &self.config.enroll_key,
            &self.boot_id,
            &inventory,
            previous,
        );
        self.key = Some(key);
        self.previous_run = None;
        self.apply(&reply, false);
    }

    fn refresh(&mut self, agent_id: i64) {
        let Some(key) = self.key.take() else {
            self.apply(
                &Reply::Refused {
                    status: 409,
                    error: None,
                    retry_after: None,
                },
                true,
            );
            return;
        };
        let inventory = self.inventory();
        let reply =
            self.client
                .refresh(&key, agent_id, &self.boot_id, &inventory, self.previous_run);
        self.key = Some(key);
        self.apply(&reply, true);
    }

    fn reload(&mut self, path: Option<&Path>) {
        let config = match config::load(path, &config::Process) {
            Ok(config) => config,
            Err(error) => {
                log(&format!("kept the old config: {error}"));
                return;
            }
        };
        if config.server.origin() != self.config.server.origin()
            && matches!(self.machine.state(), State::Enrolled { .. })
        {
            log("the server changed; restart the agent to enroll with the new one");
        }
        if config.server.origin() == self.config.server.origin()
            || !matches!(self.machine.state(), State::Enrolled { .. })
        {
            self.client = match Client::new(config.server.clone(), config.ca_file.as_deref()) {
                Ok(client) => client,
                Err(error) => {
                    log(&format!("kept the old config: {error:#}"));
                    return;
                }
            };
            let same_state_dir = config.state_dir == self.config.state_dir;
            let pipeline_changed = config.record != self.config.record
                || config.limits != self.config.limits
                || config.sanitize != self.config.sanitize
                || config.server.origin() != self.config.server.origin()
                || config.ca_file != self.config.ca_file
                || !same_state_dir
                || config.node != self.config.node
                || config.tags != self.config.tags
                || (config.record.is_some() && self.pipeline.is_none());
            self.config = config;
            if !same_state_dir {
                match Store::open(&self.config.state_dir) {
                    Ok(store) => self.store = store,
                    Err(err) => log(&format!(
                        "could not open state dir {}: {err}",
                        self.config.state_dir.display()
                    )),
                }
            }
            if pipeline_changed {
                if let Some(mut old) = self.pipeline.take() {
                    old.stop();
                }
                if let Some(record) = self.config.record.as_ref() {
                    let host_name = self
                        .config
                        .node
                        .clone()
                        .unwrap_or_else(|| recorder::host_name_from(Path::new("/")));
                    let enrolled = self.machine.agent_id();
                    match Pipeline::start(
                        &self.config,
                        record,
                        host_name,
                        enrolled,
                        same_state_dir && enrolled.is_some(),
                    ) {
                        Ok(pipeline) => {
                            if let Some(agent_id) = enrolled {
                                pipeline.set_auth(Some(agent_id), self.token.clone());
                            }
                            self.pipeline = Some(pipeline);
                        }
                        Err(error) => {
                            log(&format!("cannot restart the recording pipeline: {error:#}"));
                        }
                    }
                }
                if let Some(mut poll) = self.poll.take() {
                    poll.stop();
                }
                match PollHandle::start(&self.config, self.pipeline.as_ref().map(Pipeline::control))
                {
                    Ok(poll) => {
                        poll.set_token(self.token.clone());
                        self.poll = Some(poll);
                    }
                    Err(error) => {
                        log(&format!("cannot restart control poll: {error:#}"));
                        self.poll = None;
                    }
                }
            } else if let Some(poll) = &self.poll {
                poll.set_pipeline(self.pipeline.as_ref().map(Pipeline::control));
            }
        }
        self.machine.config_changed(self.now());
        self.save_status(None);
    }

    fn apply_control_revoked(&mut self) {
        let now = self.now();
        if self.machine.on(Outcome::Revoked, now, jitter()) == Effect::ForgetKey {
            self.key = None;
            self.token = None;
            if let Some(mut pipeline) = self.pipeline.take() {
                pipeline.stop();
            }
            if let Some(poll) = &self.poll {
                poll.set_token(None);
                poll.set_pipeline(None);
            }
            if let Err(store_error) = self.store.forget() {
                log(&format!("cannot delete the key: {store_error}"));
            }
        }
        self.save_status(Some(
            "this agent was revoked; it needs a new enrollment key to come back".to_owned(),
        ));
    }

    fn run(&mut self, signals: &Signals, path: Option<&Path>) {
        let mut fingerprint = self.config.fingerprint;
        loop {
            if self.poll.as_ref().is_some_and(PollHandle::take_revoked) {
                self.apply_control_revoked();
            }
            let now = self.now();
            let wait = match self.machine.next(now) {
                Action::Enroll { previous } => {
                    self.enroll(previous);
                    continue;
                }
                Action::Refresh { agent_id } => {
                    self.refresh(agent_id);
                    continue;
                }
                Action::Wait { until } => now.until(until),
                Action::WaitForConfig => CONFIG_CHECK,
            };

            let watching = matches!(self.machine.state(), State::Blocked { .. } | State::Revoked);
            let wait = if watching {
                wait.min(CONFIG_CHECK)
            } else {
                wait.min(Duration::from_millis(200))
            };
            match signals.wait(wait) {
                Some(Wake::Stop) => return,
                Some(Wake::Reload) => self.reload(path),
                None if watching => {
                    let current = config::fingerprint(path).unwrap_or(fingerprint);
                    if current != fingerprint {
                        fingerprint = current;
                        self.reload(path);
                    }
                }
                None => {
                    if self.poll.as_ref().is_some_and(PollHandle::take_revoked) {
                        self.apply_control_revoked();
                        continue;
                    }
                    let need_refresh = self
                        .pipeline
                        .as_ref()
                        .is_some_and(Pipeline::take_need_token)
                        || self.poll.as_ref().is_some_and(PollHandle::take_need_token);
                    if need_refresh {
                        self.machine.request_refresh(self.now());
                    }
                }
            }
        }
    }

    fn stop(&mut self) {
        if let Some(mut poll) = self.poll.take() {
            poll.stop();
        }
        if let Some(mut pipeline) = self.pipeline.take() {
            pipeline.stop();
        }
        if let Some(token) = self.token.take() {
            match self.client.goodbye(&token) {
                Reply::Done => {}
                reply => log(&format!(
                    "goodbye was not delivered: {:?}",
                    client::outcome(&reply).1
                )),
            }
        }
        "stopped".clone_into(&mut self.status.state);
        self.status.since = client::unix_now();
        self.status.running = false;
        if let Err(error) = self.store.save_status(&self.status) {
            log(&format!("cannot write the status: {error}"));
        }
        if let Err(error) = self.store.mark_stopped() {
            log(&format!("cannot remove the run marker: {error}"));
        }
        log("stopped");
    }
}

pub fn run(path: Option<&Path>) -> Result<ExitCode> {
    let signals = Signals::block()?;
    let config = config::load(path, &config::Process)?;
    let mut agent = Agent::start(config)?;
    agent.save_status(None);
    agent.run(&signals, path);
    agent.stop();
    Ok(ExitCode::SUCCESS)
}

#[must_use]
pub fn utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

fn ago(secs: i64, now: i64) -> String {
    let elapsed = now.saturating_sub(secs).max(0);
    let text = match elapsed {
        0..60 => format!("{elapsed} s"),
        60..3600 => format!("{} min", elapsed / 60),
        3600..86_400 => format!("{} h", elapsed / 3600),
        _ => format!("{} d", elapsed / 86_400),
    };
    format!("{}, {text} ago", utc(secs))
}

#[must_use]
pub fn describe(status: &Status, alive: bool, now: i64) -> String {
    let state = if alive {
        status.state.clone()
    } else if status.running {
        format!(
            "not running (it was {} and did not stop cleanly)",
            status.state
        )
    } else {
        "not running".to_owned()
    };
    let rows = [
        ("server", status.server.clone()),
        (
            "agent",
            status
                .agent_id
                .map_or_else(|| "not enrolled".to_owned(), |id| id.to_string()),
        ),
        (
            "key",
            status
                .fingerprint
                .clone()
                .unwrap_or_else(|| "none yet".to_owned()),
        ),
        ("state", format!("{state} since {}", ago(status.since, now))),
        (
            "last refresh",
            status
                .last_refresh
                .map_or_else(|| "never".to_owned(), |at| ago(at, now)),
        ),
        (
            "last error",
            status
                .last_error
                .clone()
                .unwrap_or_else(|| "none".to_owned()),
        ),
    ];
    rows.iter().fold(String::new(), |mut text, (name, value)| {
        let _ = writeln!(text, "{name:<13}{value}");
        text
    })
}

#[expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "`microscope agent status` answers on the terminal that ran it"
)]
pub fn status(path: Option<&Path>, json: bool) -> Result<ExitCode> {
    let dir: PathBuf = config::state_dir(path, &config::Process)?;
    let file = dir.join("status.json");
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("no agent has run here: {} does not exist", file.display());
            return Ok(ExitCode::from(3));
        }
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", file.display())),
    };
    let status: Status =
        serde_json::from_str(&text).with_context(|| format!("{} is damaged", file.display()))?;
    let alive = status.running && Path::new(&format!("/proc/{}", status.pid)).exists();

    if json {
        let mut value = serde_json::to_value(&status)?;
        if let Some(object) = value.as_object_mut() {
            object.insert("running".to_owned(), alive.into());
        }
        println!("{value}");
    } else {
        print!("{}", describe(&status, alive, client::unix_now()));
    }
    Ok(if alive {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(3)
    })
}

#[cfg(test)]
mod tests;
