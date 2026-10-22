//! The debugger's state as the page receives it: where the program stopped,
//! its breakpoints and where they landed, and the stops so far.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use microscope::{
    BreakpointLocation, BreakpointSpec, DebuggerHandle, ExecutionContext, ImageAddress,
    InferiorState, LoadBias, ModuleId, ModuleImage, StackFrame, StackFrameId, StateSnapshot,
    StopContext, StopReason, VirtualAddress,
};

use super::protocol::{self, Inferior, Place, State, StopEntry, Thread};
use crate::cli::format;
use crate::cli::terminal::Renderer;

/// The most stops a state lists.
const STOP_HISTORY: usize = 100;

/// The module images a debugger has loaded, kept once fetched.
pub struct Images {
    handle: DebuggerHandle,
    cache: Mutex<HashMap<ModuleId, Arc<ModuleImage>>>,

    /// The source files a runtime recorded for frames the page was shown,
    /// such as a Python program's, which no debug information names. The
    /// page may read these, and debug information's files, and no others.
    recorded: Mutex<BTreeSet<PathBuf>>,
}

impl Images {
    pub fn new(handle: DebuggerHandle) -> Self {
        Self {
            handle,
            cache: Mutex::default(),
            recorded: Mutex::default(),
        }
    }

    /// Notes the file a runtime recorded for `frame`, at `place`, as one the
    /// page was shown. A runtime's `<string>` and the like name no file.
    pub fn note_recorded(
        &self,
        frame: &StackFrame,
        place: Option<&crate::present::frames::FramePlace>,
    ) {
        if let Some(place) = place.filter(|_| frame.recorded.is_some())
            && !place.path.to_string_lossy().starts_with('<')
        {
            self.recorded
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(place.path.clone());
        }
    }

    /// Whether a frame the page was shown runs code from the file at the
    /// `path` its runtime recorded.
    pub fn recorded(&self, path: &Path) -> bool {
        self.recorded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(path)
    }

    /// The files frames the page was shown recorded, as `recorded` takes them.
    pub fn recorded_paths(&self) -> Vec<PathBuf> {
        self.recorded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    /// Forgets every image: a new process numbers its modules again.
    pub fn clear(&self) {
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    pub async fn get(&self, module: ModuleId) -> Option<Arc<ModuleImage>> {
        if let Some(image) = self
            .cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&module)
        {
            return Some(Arc::clone(image));
        }

        let image = self.handle.loaded_module_image(module).await.ok()?;
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(module, Arc::clone(&image));
        Some(image)
    }

    /// Each loaded module's load bias and image; before the program runs,
    /// only the executable, at no bias.
    pub async fn loaded(&self) -> Vec<(LoadBias, Arc<ModuleImage>)> {
        let records = self
            .handle
            .loaded_modules()
            .await
            .map(|snapshot| snapshot.modules)
            .unwrap_or_default();
        let mut loaded = Vec::new();
        for record in records.iter() {
            if let Some(image) = self.get(record.module.id).await {
                loaded.push((record.module.load_bias, image));
            }
        }

        if loaded.is_empty() {
            loaded.push((LoadBias::default(), self.handle.module_image()));
        }

        loaded
    }

    /// The images that have source files: the executable's and each loaded
    /// module's, once each.
    pub async fn with_sources(&self) -> Vec<Arc<ModuleImage>> {
        let mut images = vec![self.handle.module_image()];
        for (_, image) in self.loaded().await {
            if !images.iter().any(|known| known.path() == image.path()) {
                images.push(image);
            }
        }

        images
    }

    /// Where a frame's code is.
    pub async fn place_of_frame(&self, frame: &StackFrame) -> Place {
        let image = match frame.module {
            Some(module) => self.get(module).await,
            None => None,
        };

        let place = crate::present::frames::place(frame, image.as_deref());
        self.note_recorded(frame, place.as_ref());

        Place {
            address: frame
                .instruction
                .map_or_else(String::new, |address| hex(address.get())),
            function: Some(crate::present::frames::name(frame, image.as_deref())),
            line: place.as_ref().map(|place| place.line),
            path: place.map(|place| place.path.display().to_string()),
        }
    }
}

/// The symbol naming code no debug information describes.
fn symbol_name(image: &ModuleImage, address: ImageAddress) -> Option<String> {
    let symbol = image.symbolize(address)?;
    Some(format::code_name(None, Some(&symbol)))
}

pub fn hex(address: u64) -> String {
    format!("{address:#x}")
}

/// A watchpoint as tabs show it.
fn watchpoint(watchpoint: &microscope::Watchpoint) -> protocol::Watchpoint {
    protocol::Watchpoint {
        id: watchpoint.id.get(),
        access: watch_access(watchpoint.access),
        expression: watchpoint.expression.as_ref().map(ToString::to_string),
        address: hex(watchpoint.address.get()),
        bytes: watchpoint.byte_size,
        scope: match &watchpoint.scope {
            microscope::WatchScope::ThreadLocal { thread } => format!("thread {thread}'s instance"),
            microscope::WatchScope::Frame { thread, activation } => {
                format!("thread {thread}'s frame at {activation}, until it returns")
            }
            _ => String::new(),
        },
        condition: watchpoint.condition.as_ref().map(ToString::to_string),
        hit_condition: watchpoint.hit_condition.as_ref().map(ToString::to_string),
        hits: watchpoint.hit_count,
    }
}

pub const fn watch_access(access: microscope::WatchAccess) -> protocol::WatchAccess {
    match access {
        microscope::WatchAccess::Change => protocol::WatchAccess::Change,
        microscope::WatchAccess::Write => protocol::WatchAccess::Write,
        microscope::WatchAccess::Read => protocol::WatchAccess::Read,
        microscope::WatchAccess::ReadWrite => protocol::WatchAccess::ReadWrite,
    }
}

/// Who last ran the program, and how, for the stop that run ends at.
#[derive(Debug, Clone)]
pub struct Cause {
    pub name: String,
    pub action: String,
}

/// Builds each [`State`] for one debugger, remembering its stops.
pub struct Describer {
    pub id: String,
    pub target: protocol::Target,
    pub images: Arc<Images>,
    pub handle: DebuggerHandle,

    /// Set as a tab runs the program; taken by the stop that ends the run.
    pub cause: Arc<Mutex<Option<Cause>>>,
    stops: Vec<StopEntry>,
    pub ended: Ended,

    /// Counts the changes tabs make to the program's values.
    pub writes: Arc<AtomicU64>,

    /// Counts changes to settings that publish no event, such as signal
    /// policies.
    pub settings: Arc<AtomicU64>,
}

impl Describer {
    pub fn new(
        id: String,
        target: protocol::Target,
        handle: DebuggerHandle,
        cause: Arc<Mutex<Option<Cause>>>,
    ) -> Self {
        Self {
            id,
            target,
            images: Arc::new(Images::new(handle.clone())),
            handle,
            cause,
            stops: Vec::new(),
            ended: Ended::default(),
            writes: Arc::default(),
            settings: Arc::default(),
        }
    }

    pub async fn describe(&mut self, snapshot: &StateSnapshot) -> State {
        let inferior = match &snapshot.inferior {
            InferiorState::NotRunning => match (self.ended.detached, &self.ended.exited) {
                (Some(pid), _) => Inferior::Detached { pid },
                (None, Some(description)) => Inferior::Exited {
                    description: description.clone(),
                },
                (None, None) => Inferior::NotStarted,
            },
            InferiorState::Running { process_id, .. } => Inferior::Running {
                pid: process_id.get(),
            },
            InferiorState::Stopped {
                process_id,
                stop_id,
                thread_id,
                reason,
            } => {
                // The controller selects a frame other than the innermost
                // where a runtime's agent stopped the thread.
                let selected = match snapshot.selected {
                    Some(ExecutionContext::Thread(selected)) if selected == *thread_id => {
                        snapshot.selected_frame.unwrap_or(StackFrameId::INNERMOST)
                    }
                    _ => StackFrameId::INNERMOST,
                };
                let entry = self
                    .stop_entry(stop_id.get(), thread_id.get(), reason, selected)
                    .await;

                Inferior::Stopped {
                    pid: process_id.get(),
                    stop: stop_id.get(),
                    thread: thread_id.get(),
                    reason: entry.reason,
                    frame: entry.frame,
                    place: entry.place,
                }
            }
        };

        let mut breakpoints = Vec::with_capacity(snapshot.breakpoints.len());
        for breakpoint in snapshot.breakpoints.iter() {
            breakpoints.push(self.breakpoint(breakpoint).await);
        }

        State {
            session: Some(self.id.clone()),
            target: Some(self.target.clone()),
            busy: None,
            revision: snapshot.revision,
            inferior,
            threads: snapshot
                .threads
                .iter()
                .map(|thread| Thread {
                    id: thread.id.get(),
                    name: thread.name.as_deref().map(str::to_owned),
                    stopped: matches!(thread.state, microscope::ThreadState::Stopped { .. }),
                })
                .collect(),
            breakpoints,
            stops: self.stops.clone(),
            writes: self.writes.load(Ordering::Relaxed),
            watchpoints: snapshot.watchpoints.iter().map(watchpoint).collect(),
            settings: self.settings.load(Ordering::Relaxed),
            group: None,
        }
    }

    /// The history's entry for a stop, made when the stop is first seen.
    async fn stop_entry(
        &mut self,
        stop: u64,
        thread: u64,
        reason: &StopReason,
        selected: StackFrameId,
    ) -> StopEntry {
        if let Some(entry) = self.stops.iter().rev().find(|entry| entry.stop == stop) {
            return entry.clone();
        }

        let context = StopContext {
            stop: microscope::StopId::new(stop),
            execution: microscope::ThreadId::new(thread).into(),
            frame: StackFrameId::INNERMOST,
        };

        let (frame, place) = match self.handle.at(context).backtrace().await {
            Ok(trace) => {
                let shown = trace
                    .frames
                    .iter()
                    .find(|frame| frame.id == selected)
                    .or_else(|| trace.frames.first());
                match shown {
                    Some(shown) => (
                        shown.id.get(),
                        Some(self.images.place_of_frame(shown).await),
                    ),
                    None => (0, None),
                }
            }
            Err(_) => (0, None),
        };

        let cause = self
            .cause
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let entry = StopEntry {
            stop,
            thread,
            reason: protocol::StopReason {
                kind: reason_kind(reason).to_owned(),
                description: plain(reason),
            },
            frame,
            place,
            by: cause.as_ref().map(|cause| cause.name.clone()),
            action: cause.map(|cause| cause.action),
        };

        self.stops.push(entry.clone());
        if self.stops.len() > STOP_HISTORY {
            self.stops.remove(0);
        }

        entry
    }

    async fn breakpoint(&self, breakpoint: &microscope::Breakpoint) -> protocol::Breakpoint {
        let mut places = Vec::with_capacity(breakpoint.locations.len());
        for location in breakpoint.locations.iter() {
            places.push((location, self.breakpoint_place(breakpoint, location).await));
        }

        let (places, pending) = breakpoint_places(breakpoint, places);

        protocol::Breakpoint {
            id: breakpoint.id.get(),
            location: breakpoint.spec.to_string(),
            condition: breakpoint.condition.as_ref().map(ToString::to_string),
            hit_condition: breakpoint
                .hit_condition
                .map(|condition| condition.to_string()),
            log_message: breakpoint.log_message.as_ref().map(ToString::to_string),
            hits: breakpoint.hit_count,
            places,
            pending,
        }
    }

    /// Where one of a breakpoint's locations is: its address and source
    /// line, with a line breakpoint keeping the file it names.
    async fn breakpoint_place(
        &self,
        breakpoint: &microscope::Breakpoint,
        resolved: &microscope::ResolvedBreakpointLocation,
    ) -> Place {
        let (image, address, shown) = match (&resolved.location, resolved.library) {
            (BreakpointLocation::Image(address), _) => (
                Some(self.handle.module_image()),
                *address,
                // Before the program runs, there is no virtual address yet.
                self.main_bias()
                    .await
                    .map_or_else(|| address.get(), |bias| bias.wrapping_load(*address).get()),
            ),
            (BreakpointLocation::Virtual(address), Some(library)) => {
                let bias = self.bias_of(library).await;
                match (self.images.get(library).await, bias) {
                    (Some(image), Some(bias)) => {
                        (Some(image), bias.wrapping_unload(*address), address.get())
                    }
                    _ => (None, ImageAddress::new(0), address.get()),
                }
            }
            (BreakpointLocation::Virtual(address), None) => {
                return self.place_of_address(*address).await;
            }

            // A runtime's location has no address: it is where the runtime
            // says it is.
            (BreakpointLocation::Managed(managed), _) => {
                return match &managed.place {
                    microscope::ManagedPlace::Line { path, line } => Place {
                        address: String::new(),
                        function: None,
                        path: Some(path.to_string()),
                        line: Some(u64::from(*line)),
                    },
                    microscope::ManagedPlace::Function { name } => Place {
                        address: String::new(),
                        function: Some(name.to_string()),
                        path: None,
                        line: None,
                    },
                };
            }
        };

        let mut place = Place {
            address: hex(shown),
            function: None,
            path: None,
            line: None,
        };

        let Some(image) = image else {
            return place;
        };

        let located = image.locate(address);
        place.function = located
            .function
            .map(|function| function.name.to_string())
            .or_else(|| symbol_name(&image, address));
        if let Some(location) = image.source_location(address)
            && let Some(file) = image.source_file(location.file)
        {
            let (path, line) = match &breakpoint.spec {
                BreakpointSpec::Source { path, line } => image
                    .source_file_matching(path)
                    .ok()
                    .and_then(|named| Some((named, image.breakpoint_line(named.id, *line)?)))
                    .map_or((file, location.line), |(named, line)| (named, line)),
                _ => (file, location.line),
            };

            place.path = Some(path.path.display().to_string());
            place.line = Some(line.get());
        }

        place
    }

    async fn place_of_address(&self, address: VirtualAddress) -> Place {
        let mut place = Place {
            address: hex(address.get()),
            function: None,
            path: None,
            line: None,
        };

        for (bias, image) in self.images.loaded().await {
            let Some(image_address) = bias.unload(address) else {
                continue;
            };

            if !image.contains_address(image_address) {
                continue;
            }

            let located = image.locate(image_address);
            place.function = located
                .function
                .map(|function| function.name.to_string())
                .or_else(|| symbol_name(&image, image_address));
            if let Some(location) = located.source
                && let Some(file) = image.source_file(location.file)
            {
                place.path = Some(file.path.display().to_string());
                place.line = Some(location.line.get());
            }

            break;
        }

        place
    }

    /// The executable's load bias, once it is loaded: modules are listed
    /// by identifier, and the executable's comes first.
    async fn main_bias(&self) -> Option<LoadBias> {
        let snapshot = self.handle.loaded_modules().await.ok()?;
        snapshot
            .modules
            .first()
            .map(|record| record.module.load_bias)
    }

    async fn bias_of(&self, module: ModuleId) -> Option<LoadBias> {
        let snapshot = self.handle.loaded_modules().await.ok()?;
        snapshot
            .modules
            .iter()
            .find(|record| record.module.id == module)
            .map(|record| record.module.load_bias)
    }
}

/// How the program last ended, which snapshots no longer say.
#[derive(Default)]
pub struct Ended {
    exited: Option<String>,
    detached: Option<u64>,
}

impl Ended {
    pub fn observe(&mut self, event: &microscope::DebuggerEvent) {
        use microscope::DebuggerEvent;
        match event {
            DebuggerEvent::InferiorExited { status, .. } => {
                self.exited = Some(plain(&StopReason::Exited(status.clone())));
            }
            DebuggerEvent::InferiorDetached { process_id, .. } => {
                self.detached = Some(process_id.get());
            }
            DebuggerEvent::InferiorLaunched { .. } | DebuggerEvent::InferiorAttached { .. } => {
                *self = Self::default();
            }
            _ => {}
        }
    }
}

/// Why the program stopped, as the CLI says it but without "inferior":
/// the page names the program.
pub fn plain(reason: &StopReason) -> String {
    let text = format::stop(reason, Renderer::new(false));
    text.strip_prefix("inferior ")
        .map_or_else(|| text.clone(), str::to_owned)
}

const fn reason_kind(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::Attach => "attach",
        StopReason::Entry => "entry",
        StopReason::Breakpoint { .. } => "breakpoint",
        StopReason::Watchpoint { .. } => "watchpoint",
        StopReason::WatchpointInvalidated { .. } => "watchpointInvalidated",
        StopReason::WatchpointArmFailed { .. } => "watchpointArmFailed",
        StopReason::Syscall(_) => "syscall",
        StopReason::Step { .. } => "step",
        StopReason::StepIncomplete { .. } => "stepIncomplete",
        StopReason::TaskEnded { .. } => "taskEnded",
        StopReason::FutureDropped { .. } => "futureDropped",
        StopReason::Pause => "pause",
        StopReason::Jump => "jump",
        StopReason::Assignment => "assignment",
        StopReason::Exception(_) => "exception",
        StopReason::LanguageException(_) => "languageException",
        StopReason::RuntimeEvent { .. } => "runtimeEvent",
        StopReason::ProgramBreakpoint { .. } => "programBreakpoint",
        StopReason::Exec { .. } => "exec",
        StopReason::ThreadExited { .. } => "threadExited",
        StopReason::Unclassifiable { .. } => "unclassifiable",
        StopReason::Exited(_) => "exited",
        StopReason::CoreDump { .. } => "coreDump",
    }
}

/// Partitions queried metadata without losing the arming failure or presenting
/// an unarmed runtime location as a ready placement.
fn breakpoint_places(
    breakpoint: &microscope::Breakpoint,
    places: Vec<(&microscope::ResolvedBreakpointLocation, Place)>,
) -> (Vec<Place>, Option<protocol::PendingBreakpoint>) {
    debug_assert_eq!(
        places.len(),
        breakpoint.locations.len(),
        "breakpoint metadata covers every resolved location"
    );
    let mut ready = Vec::new();
    let mut pending = Vec::new();
    for (resolved, place) in places {
        if let Some(issue) = breakpoint
            .pending_locations
            .iter()
            .find(|pending| pending.location == resolved.location)
        {
            pending.push(protocol::PendingBreakpointPlace {
                place,
                reason: issue.reason.to_string(),
            });
        } else {
            ready.push(place);
        }
    }

    let pending = crate::present::breakpoints::pending_message(breakpoint).map(|message| {
        protocol::PendingBreakpoint {
            message,
            places: pending,
        }
    });

    (ready, pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_breakpoints_keep_pending_locations_separate_from_ready_places() {
        let location = microscope::BreakpointLocation::Managed(microscope::ManagedLocation {
            runtime: microscope::RuntimeId::new(1),
            place: microscope::ManagedPlace::Line {
                path: "/src/app.py".into(),
                line: 3,
            },
        });
        let mut breakpoint = microscope::Breakpoint {
            id: microscope::BreakpointId::new(1),
            spec: microscope::BreakpointSpec::Function("managed".into()),
            locations: Arc::from([microscope::ResolvedBreakpointLocation {
                location: location.clone(),
                code_instances: Arc::from([]),
                library: None,
            }]),
            pending_locations: Arc::from([microscope::PendingBreakpointLocation {
                location,
                reason: "waiting for an acknowledgement".into(),
            }]),
            hit_condition: None,
            condition: None,
            log_message: None,
            hit_count: 0,
            enabled: true,
            temporary: false,
        };
        let source = Place {
            address: String::new(),
            function: None,
            path: Some("/src/app.py".into()),
            line: Some(3),
        };
        let (ready, pending) = breakpoint_places(
            &breakpoint,
            vec![(&breakpoint.locations[0], source.clone())],
        );
        assert!(ready.is_empty());
        let pending = pending.expect("visible pending state");
        assert!(pending.message.contains("acknowledgement"));
        assert_eq!(pending.places[0].place, source);
        let json = serde_json::to_value(&pending).expect("wire state");
        assert_eq!(
            json["places"][0]["reason"],
            "waiting for an acknowledgement"
        );

        let native = microscope::ResolvedBreakpointLocation {
            location: microscope::BreakpointLocation::Virtual(VirtualAddress::new(0x1234)),
            code_instances: Arc::from([]),
            library: None,
        };
        let mut locations = breakpoint.locations.to_vec();
        locations.push(native);
        breakpoint.locations = locations.into();
        let native = Place {
            address: "0x1234".into(),
            function: None,
            path: None,
            line: None,
        };
        let queried = || {
            vec![
                (&breakpoint.locations[0], source.clone()),
                (&breakpoint.locations[1], native.clone()),
            ]
        };

        let (ready, pending) = breakpoint_places(&breakpoint, queried());
        assert_eq!(ready, vec![native.clone()]);
        assert!(
            pending
                .expect("partial arming")
                .message
                .contains("some locations pending")
        );
        breakpoint.pending_locations = Arc::from([]);

        let (ready, pending) = breakpoint_places(
            &breakpoint,
            vec![
                (&breakpoint.locations[0], source.clone()),
                (&breakpoint.locations[1], native.clone()),
            ],
        );
        assert_eq!(ready, vec![source, native]);
        assert!(pending.is_none());

        let wire = protocol::Breakpoint {
            id: 1,
            location: "managed".into(),
            condition: None,
            hit_condition: None,
            log_message: None,
            hits: 0,
            places: ready,
            pending,
        };

        assert!(
            serde_json::to_value(&wire)
                .expect("wire state")
                .get("pending")
                .is_none()
        );
    }
}
