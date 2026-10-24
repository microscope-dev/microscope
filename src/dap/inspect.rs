//! Requests that inspect a stop: threads, stacks, scopes, variables, and
//! expressions.

use std::sync::Arc;

use microscope::{
    ExecutionContext, StackFrame, StopContext, StopReason, UnwindTermination, VariableKind,
};
use serde_json::{Map, Value, json};

use super::handles::{Location, Variables};
use super::protocol::{
    CompletionsArguments, ErrorBody, EvaluateArguments, ExceptionInfoArguments, LocationsArguments,
    ScopesArguments, SetExpressionArguments, SetVariableArguments, StackFrameFormat,
    StackTraceArguments, ValueFormat, VariablesArguments,
};

use super::session::{Session, Stop, error, parse, signal_text};
use super::values::{self, Options};
use crate::present::{self, Filter, Item, ListError, Listed, Presenter, Window, complete};

/// The most children one `variables` request returns.
const MAX_CHILDREN: u64 = 1024;

/// The most completions one request offers.
const MAX_COMPLETIONS: usize = 1000;

/// How many tasks one debugger request lists.
const TASK_PAGE: usize = 1024;

/// A thread's name in a list of tasks, which it is in because it stopped
/// running none.
fn thread_name(
    snapshot: &microscope::StateSnapshot,
    context: ExecutionContext,
    stopped: Option<String>,
) -> Result<String, ErrorBody> {
    let Some(thread) = context.as_thread() else {
        return Err(ErrorBody::new(
            "internal: an entry without a task is a thread",
        ));
    };

    let detail = stopped
        .map(|detail| format!(" — {detail}"))
        .unwrap_or_default();

    // A runtime's labels follow the thread's id, as `main (7, isolate 1)`.
    let listed = snapshot.threads.iter().find(|listed| listed.id == thread);
    let mut within = vec![thread.to_string()];
    within.extend(
        listed
            .into_iter()
            .flat_map(|listed| listed.labels.iter())
            .map(ToString::to_string),
    );
    let within = within.join(", ");

    Ok(listed
        .and_then(|listed| listed.name.as_deref())
        .map_or_else(
            || match within.split_once(", ") {
                Some((_, labels)) => format!("Thread {thread} ({labels}){detail}"),
                None => format!("Thread {thread}{detail}"),
            },
            |name| format!("{name} ({within}){detail}"),
        ))
}

/// The stop reasons at which VS Code focuses the first frame with a source
/// whatever its hint, as its `Thread.getTopStackFrame` does.
const FOCUSED_REGARDLESS: [&str; 3] = ["breakpoint", "step", "function breakpoint"];

impl Session {
    pub(super) async fn threads(&mut self) -> Result<Value, ErrorBody> {
        if let Some(stop) = self.stop.clone()
            && let Some(threads) = self.task_threads(&stop).await?
        {
            return Ok(json!({"threads": threads}));
        }

        let handle = self.target_handle().ok();
        let snapshot = match &handle {
            Some(handle) => Some(handle.snapshot().await.map_err(error)?),
            None => None,
        };

        let mut threads = Vec::new();

        if let Some(snapshot) = &snapshot {
            for thread in snapshot.threads.iter() {
                // A thread the stop found stopped for a reason of its own,
                // such as a breakpoint it hit too, says what stopped it.
                let stopped = match &thread.state {
                    microscope::ThreadState::Stopped {
                        reason: Some(reason),
                    } if self.stop.is_some() => Some(self.stopped_detail(reason)),
                    _ => None,
                };

                let context = ExecutionContext::Thread(thread.id);
                threads.push(json!({
                    "id": self.thread_ids.id(context)?,
                    "name": thread_name(snapshot, context, stopped)?,
                }));
            }
        }

        if !threads.is_empty() {
            return Ok(json!({"threads": threads}));
        }

        // Clients need a thread to show, and to pause, even before the
        // program starts.
        let name = handle.map_or_else(
            || "program".to_owned(),
            |handle| {
                handle.executable().file_name().map_or_else(
                    || handle.executable().display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                )
            },
        );

        Ok(json!({"threads": [{"id": 1, "name": name}]}))
    }

    /// The tasks of a stop as the client's threads, or `None` when the
    /// program has none or the client's threads are the system's.
    ///
    /// DAP cannot page threads, so the list is ordered by what a user looks
    /// for first: the task that stopped, the tasks on threads, any thread
    /// that stopped running none, and the program's own threads, then the
    /// program's tasks, and with `runtimeTasks` the runtime's. It is cut
    /// at `maxTasks`, and a last entry counts the rest.
    async fn task_threads(&mut self, stop: &Stop) -> Result<Option<Vec<Value>>, ErrorBody> {
        let Some(listing) = self.thread_listing().filter(|listing| listing.tasks) else {
            return Ok(None);
        };

        let handle = self.target_handle()?;

        // The runtime's own tasks are left out before paging when they are
        // not shown, unless the stop is in one.
        let mut tasks = all_tasks(&handle, !listing.runtime_tasks).await?;
        if let ExecutionContext::Task(stopped) = stop.context
            && !tasks.iter().any(|task| task.id == stopped)
        {
            tasks = all_tasks(&handle, false).await?;
        }

        if tasks.is_empty() {
            return Ok(None);
        }

        // A closed client has no use for the list either.
        let _ = self.forget_tasks(&tasks).await;
        let snapshot = handle.snapshot().await.map_err(error)?;
        let stopped = |thread: microscope::ThreadId| {
            snapshot
                .threads
                .iter()
                .find(|listed| listed.id == thread)
                .and_then(|listed| match &listed.state {
                    microscope::ThreadState::Stopped { reason } => reason.clone(),
                    microscope::ThreadState::Running => None,
                })
        };

        let rank = |context: ExecutionContext, on_thread: bool, internal: bool| {
            if context == stop.context {
                0
            } else if on_thread {
                1
            } else if internal {
                3
            } else {
                2
            }
        };

        let mut entries = tasks
            .iter()
            .filter(|task| {
                listing.runtime_tasks
                    || !task.internal
                    || stop.context == ExecutionContext::Task(task.id)
            })
            .map(|task| {
                let context = ExecutionContext::Task(task.id);
                (
                    rank(context, task.thread.is_some(), task.internal),
                    Some(task),
                    context,
                )
            })
            .collect::<Vec<_>>();

        // A thread that stopped for a reason of its own but runs no task is
        // there too, so its stop can be inspected, and so is every thread
        // of the program's own; only a runtime's idle threads are not.
        for thread in snapshot.threads.iter() {
            let runs_task = matches!(
                thread.activity,
                Some(microscope::ThreadActivity::Task { .. })
            );
            let own = matches!(thread.activity, Some(microscope::ThreadActivity::Outside));
            if !runs_task && (own || stopped(thread.id).is_some()) {
                let context = ExecutionContext::Thread(thread.id);
                entries.push((rank(context, true, false), None, context));
            }
        }

        entries.sort_by_key(|(rank, _, _)| *rank);
        let omitted = entries.len().saturating_sub(listing.max_tasks);
        entries.truncate(listing.max_tasks);

        let mut threads = Vec::with_capacity(entries.len() + 1);
        for (_, task, context) in entries {
            let thread = task.map_or_else(|| context.as_thread(), |task| task.thread);
            let reason = thread
                .and_then(stopped)
                .map(|reason| self.stopped_detail(&reason));
            let name = match task {
                Some(task) => self.task_name(stop, task, reason).await,
                None => thread_name(&snapshot, context, reason)?,
            };

            threads.push(json!({"id": self.thread_ids.id(context)?, "name": name}));
        }

        if omitted > 0 {
            let noun = format!("more {}", tasks[0].noun);
            threads.push(json!({
                "id": self.thread_ids.placeholder()?,
                "name": format!(
                    "{} not shown; maxTasks lists {}",
                    crate::cli::format::plural(omitted as u64, &noun),
                    listing.max_tasks
                ),
            }));
        }

        Ok(Some(threads))
    }

    /// A task's name as a thread: its number, the function the program
    /// wrote that it is in, what it does or why it stopped, and its thread.
    async fn task_name(
        &mut self,
        stop: &Stop,
        task: &microscope::TaskSnapshot,
        stopped: Option<String>,
    ) -> String {
        let place = self
            .backtrace(stop, ExecutionContext::Task(task.id))
            .await
            .ok()
            .and_then(|trace| crate::cli::format::task_function(task, &trace))
            .unwrap_or_else(|| "?".to_owned());
        let detail = stopped
            .or_else(|| task.detail.as_deref().map(str::to_owned))
            .map(|detail| format!(" — {detail}"))
            .unwrap_or_default();
        let labels = crate::cli::format::task_labels(task)
            .map(|labels| format!(" {labels}"))
            .unwrap_or_default();
        let thread = task
            .thread
            .map(|thread| format!(" (thread {thread})"))
            .unwrap_or_default();

        format!("[{}] {place}{detail}{labels}{thread}", task.id.number)
    }

    /// What stopped a thread, in a thread's name.
    fn stopped_detail(&self, reason: &StopReason) -> String {
        match reason {
            StopReason::Breakpoint { hits, .. } => {
                let (ids, _) = self.breakpoints.hit(hits);
                let ids = ids.iter().map(ToString::to_string).collect::<Vec<_>>();
                format!("at breakpoint {}", ids.join(", "))
            }
            StopReason::Watchpoint { hits } => {
                let ids = self.data.hit(hits);
                let ids = ids.iter().map(ToString::to_string).collect::<Vec<_>>();
                format!("at data breakpoint {}", ids.join(", "))
            }
            other => format!("stopped: {}", super::session::stop_kind(other)),
        }
    }

    pub(super) async fn stack_trace(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<StackTraceArguments>(arguments, "stackTrace arguments")?;
        let stop = self.current_stop()?;
        let context = self.thread_ids.context(arguments.thread_id)?;
        let trace = self.backtrace(&stop, context).await?;

        // Where a stack goes on on another, a label heads each run of
        // frames saying whose stack it is on, and a stack cut short says
        // so instead of looking complete.
        let switches = trace
            .frames
            .windows(2)
            .any(|pair| pair[0].segment != pair[1].segment);
        let mut entries = Vec::with_capacity(trace.frames.len() + 2);
        let mut segment = None;
        let iterators = trace.loop_iterators();

        for (frame, iterates) in trace.frames.iter().zip(iterators) {
            if switches && segment != Some(frame.segment) {
                segment = Some(frame.segment);
                entries.push(Err(
                    crate::cli::format::stack_label(frame.segment).to_owned()
                ));
            }

            for note in trace
                .unfollowed
                .iter()
                .filter(|note| note.driver == frame.id)
            {
                entries.push(Err(format!(
                    "<{}>",
                    crate::present::frames::unfollowed(note, "the next frame")
                )));
            }

            entries.push(Ok((frame, iterates)));
        }

        if trace.termination != UnwindTermination::Complete {
            entries.push(Err(format!("<backtrace stopped: {}>", trace.termination)));
        }

        let total = entries.len();
        let start = usize::try_from(arguments.start_frame.unwrap_or(0)).unwrap_or(0);
        let levels = arguments
            .levels
            .and_then(|levels| usize::try_from(levels).ok())
            .filter(|levels| *levels != 0)
            .unwrap_or(usize::MAX);
        let format = arguments.format.unwrap_or_default();
        let mut frames = Vec::new();

        for entry in entries.into_iter().skip(start).take(levels) {
            let (frame, iterates) = match entry {
                Ok(entry) => entry,
                Err(label) => {
                    frames.push(json!({
                        "id": self.references.label()?,
                        "name": label,
                        "line": 0,
                        "column": 0,
                        "presentationHint": "label",
                    }));
                    continue;
                }
            };

            let mut body = self.stack_frame(stop.id, context, frame).await?;

            // Clients focus the first frame whose source is not
            // deemphasized, so the frames above the one the debugger
            // selected at the stop are. At a breakpoint or a step, VS Code
            // focuses the first frame with any source, so there they have
            // none, as where an agent's or an interpreter's frames are
            // above the program's.
            if context == stop.context && frame.id < stop.selected && body["source"].is_object() {
                if FOCUSED_REGARDLESS.contains(&stop.told) {
                    if let Some(body) = body.as_object_mut() {
                        body.remove("source");
                    }
                } else {
                    body["source"]["presentationHint"] = "deemphasize".into();
                }
            }

            // An iterator recedes behind the loop whose body it runs.
            if let Some(level) = iterates {
                body["name"] = format!(
                    "{} [iterator of #{level}'s loop]",
                    body["name"].as_str().unwrap_or_default()
                )
                .into();
                body["presentationHint"] = "subtle".into();
            }

            self.decorate(
                &mut body,
                frame,
                &format,
                StopContext {
                    stop: stop.id,
                    execution: context,
                    frame: frame.id,
                },
            )
            .await;
            frames.push(body);
        }

        Ok(json!({"stackFrames": frames, "totalFrames": total}))
    }

    async fn stack_frame(
        &mut self,
        stop: microscope::StopId,
        execution: ExecutionContext,
        frame: &StackFrame,
    ) -> Result<Value, ErrorBody> {
        let id = self.references.frame(StopContext {
            stop,
            execution,
            frame: frame.id,
        })?;
        let image = match frame.module {
            Some(module) => self.image(module).await,
            None => None,
        };

        let mut name = crate::present::frames::name(frame, image.as_deref());

        match frame.kind {
            microscope::FrameKind::Inline => name.push_str(" [inlined]"),
            microscope::FrameKind::TailCall => name.push_str(" [tail call]"),
            microscope::FrameKind::Async { .. } => name.insert_str(0, "async "),
            microscope::FrameKind::Physical
            | microscope::FrameKind::Signal
            | microscope::FrameKind::Awaited { .. }
            | microscope::FrameKind::Interpreted { .. } => {}
        }

        let mut body = json!({
            "id": id,
            "name": name,
            "line": 0,
            "column": 0,
        });

        if let Some(address) = frame.instruction {
            body["instructionPointerReference"] = format!("{address:#x}").into();
        }

        if let Some(module) = frame.module {
            body["moduleId"] = module.get().to_string().into();
        }

        match crate::present::frames::place(frame, image.as_deref()) {
            Some(place) => {
                // A runtime's own module, as Node's `node:fs`, is named
                // without a path no editor could open.
                let names_file = frame
                    .recorded
                    .as_ref()
                    .and_then(|recorded| recorded.place.as_ref())
                    .is_none_or(microscope::RecordedPlace::names_file);
                body["source"] = if names_file {
                    super::sources::source_json(&self.local_path(&place.path))
                } else {
                    json!({
                        "name": place.path.display().to_string(),
                        "presentationHint": "deemphasize",
                    })
                };
                body["line"] = self.line_to_client(place.line).into();
                body["column"] = place
                    .column
                    .map_or(0, |column| self.column_to_client(column))
                    .into();
            }
            None => body["presentationHint"] = "subtle".into(),
        }

        // A runtime's machinery, an interpreter's included, and the
        // wrappers a compiler writes recede, by the frame's role, which a
        // function only symbols describe has too.
        if frame.role != microscope::CodeRole::Ordinary {
            body["presentationHint"] = "subtle".into();
        }

        Ok(body)
    }

    /// Adds what a client's frame format asks for to a frame's name: its
    /// parameters, line, and module.
    async fn decorate(
        &mut self,
        body: &mut Value,
        frame: &StackFrame,
        format: &StackFrameFormat,
        context: StopContext,
    ) {
        let mut name = body["name"].as_str().unwrap_or_default().to_owned();

        if format.parameters == Some(true) {
            let names = format.parameter_names.unwrap_or(true);
            let types = format.parameter_types.unwrap_or(false);
            let values = format.parameter_values.unwrap_or(true);
            let hex = format.hex.unwrap_or(self.display.hex);

            let parameters = self.frame_variables(context).await.map_or_else(
                |_| Vec::new(),
                |snapshot| {
                    snapshot
                        .variables
                        .iter()
                        .filter(|variable| variable.kind == VariableKind::Parameter)
                        .map(|variable| {
                            let mut text = String::new();
                            if types {
                                text.push_str(
                                    variable.type_info.as_ref().map_or("?", |info| &info.name),
                                );
                                text.push(' ');
                            }

                            if names {
                                text.push_str(&variable.name);
                            }

                            if values {
                                if names {
                                    text.push_str(" = ");
                                }

                                text.push_str(&present::text(
                                    variable.type_info.as_ref(),
                                    &variable.state,
                                    hex,
                                ));
                            }

                            text.trim().to_owned()
                        })
                        .collect::<Vec<_>>()
                },
            );
            name = format!("{name}({})", parameters.join(", "));
        }

        if format.line == Some(true) && body["line"].as_u64().is_some_and(|line| line != 0) {
            name = format!("{name} Line {}", body["line"]);
        }

        if format.module == Some(true)
            && let Some(module) = frame.module
            && let Some(image) = self.image(module).await
            && let Some(file) = image.path().file_name()
        {
            name = format!("{name} [{}]", file.to_string_lossy());
        }

        body["name"] = name.into();
    }

    pub(super) async fn scopes(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<ScopesArguments>(arguments, "scopes arguments")?;
        let context = self.frame_context(arguments.frame_id)?;
        let mut scopes = Vec::new();

        match self.frame_variables(context).await {
            Ok(snapshot) => {
                let count = |kind| {
                    snapshot
                        .variables
                        .iter()
                        .filter(|variable| present::in_scope(kind, variable.kind))
                        .count()
                };

                // Locals always show; other scopes only when they have variables.
                for (kind, name) in present::SCOPES {
                    let variables = count(kind);
                    if variables == 0 && kind != VariableKind::Local {
                        continue;
                    }
                    let reference = self
                        .references
                        .variables(Variables::Scope { context, kind })?;
                    let mut scope = json!({
                        "name": name,
                        "variablesReference": reference,
                        "namedVariables": variables,
                        "expensive": false,
                    });
                    let hint = match kind {
                        VariableKind::Parameter => Some("arguments"),
                        VariableKind::Local => Some("locals"),
                        _ => None,
                    };
                    if let Some(hint) = hint {
                        scope["presentationHint"] = hint.into();
                    }
                    scopes.push(scope);
                }
            }
            Err(error) => scopes.push(json!({
                "name": format!("No variables: {}", error.format),
                "variablesReference": 0,
                "expensive": false,
            })),
        }

        let frame = self.frame_of(context).await;
        if let Some((module, file)) = frame
            .as_ref()
            .and_then(|frame| Some((frame.module?, frame.source.as_ref()?.file)))
        {
            let reference = self.references.variables(Variables::Statics {
                context,
                module,
                file,
            })?;
            scopes.push(json!({
                "name": "Statics",
                "variablesReference": reference,
                "expensive": true,
            }));
        }

        // A suspended task's frame and an interpreted one have no
        // registers.
        if frame.is_none_or(|frame| frame.kind.has_registers()) {
            let reference = self
                .references
                .variables(Variables::Registers { context })?;
            scopes.push(json!({
                "name": "Registers",
                "presentationHint": "registers",
                "variablesReference": reference,
                "expensive": true,
            }));
        }

        Ok(json!({"scopes": scopes}))
    }

    fn frame_context(&self, frame: i64) -> Result<StopContext, ErrorBody> {
        self.current_stop()?;
        self.references
            .frame_context(frame)
            .ok_or_else(|| stale("frame", frame))
    }

    pub(super) async fn variables_request(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<VariablesArguments>(arguments, "variables arguments")?;
        self.current_stop()?;
        let variables = self
            .references
            .variables_of(arguments.variables_reference)
            .cloned()
            .ok_or_else(|| stale("variables", arguments.variables_reference))?;
        let options = self.value_options(arguments.format.as_ref());
        let window = Window {
            start: u64::try_from(arguments.start.unwrap_or(0)).unwrap_or(0),
            count: arguments
                .count
                .and_then(|count| u64::try_from(count).ok())
                .filter(|count| *count != 0)
                .unwrap_or(u64::MAX)
                .min(MAX_CHILDREN),
            filter: match arguments.filter.as_deref() {
                Some("indexed") => Some(Filter::Indexed),
                Some("named") => Some(Filter::Named),
                _ => None,
            },
        };

        // A client pages by the count the list was given, so a list of one
        // kind of row has none of the other to show. A view's list has both,
        // and the window chooses between them.
        let wanted = window.filter.map(|filter| filter == Filter::Indexed);
        if wanted.is_some_and(|wanted| variables.indexed() == Some(!wanted)) {
            return Ok(json!({"variables": []}));
        }

        let list = arguments.variables_reference;
        let context = variables.context();
        let rows = self.rows(variables, window, options).await?;

        // Data breakpoints name rows by their list and name.
        for row in &rows {
            if let (Some(name), Some(path)) = (
                row.get("name").and_then(Value::as_str),
                row.get("evaluateName")
                    .and_then(Value::as_str)
                    .and_then(|path| microscope::Expression::parse(path).ok()),
            ) {
                self.references
                    .record_path(list, name.to_owned(), context, path);
            }
        }

        Ok(json!({"variables": rows}))
    }

    /// The rows of one list of variables.
    async fn rows(
        &mut self,
        variables: Variables,
        window: Window,
        options: Options,
    ) -> Result<Vec<Map<String, Value>>, ErrorBody> {
        let context = variables.context();
        let handle = self.target_handle()?;
        let code = self.code();
        let presenter = presenter(&handle, &code, options);
        let listed = match variables {
            Variables::Scope { context, kind } => {
                return self.scope_rows(context, kind, window, options).await;
            }
            Variables::Registers { context } => {
                let registers = handle.at(context).registers().await.map_err(error)?;
                return Ok(window
                    .slice(registers.registers.iter())
                    .map(|value| {
                        values::register(
                            value,
                            registers.target.byte_order,
                            context.frame == microscope::StackFrameId::INNERMOST,
                        )
                    })
                    .collect());
            }

            Variables::Statics {
                context,
                module,
                file,
            } => {
                let image = self
                    .image(module)
                    .await
                    .ok_or_else(|| ErrorBody::new("the frame's module is no longer loaded"))?;
                presenter
                    .statics(context, &image, module, file, window)
                    .await
            }

            Variables::Children {
                context,
                reference,
                path,
                ..
            } => {
                presenter
                    .children(context, reference, path.as_ref(), window)
                    .await
            }

            Variables::Pointee {
                context,
                reference,
                name,
                path,
            } => {
                presenter
                    .pointee(context, reference, &name, path, window)
                    .await
            }

            Variables::Range {
                context,
                expression,
            } => presenter.range(context, &expression, window).await,
        }
        .map_err(list_error)?;

        self.present_all(listed, context, options)
    }

    /// Presents a frame's parameters or locals.
    async fn scope_rows(
        &mut self,
        context: StopContext,
        kind: VariableKind,
        window: Window,
        options: Options,
    ) -> Result<Vec<Map<String, Value>>, ErrorBody> {
        let snapshot = self.frame_variables(context).await?;
        let module = self
            .frame_of(context)
            .await
            .filter(|frame| frame.source.is_some())
            .and_then(|frame| frame.module);
        let handle = self.target_handle()?;
        let code = self.code();
        let listed = presenter(&handle, &code, options)
            .scope(context, &snapshot, kind, module, window)
            .await;

        self.present_all(listed, context, options)
    }

    /// The module and source file of a frame's location, when it has one.
    async fn frame_of(&mut self, context: StopContext) -> Option<StackFrame> {
        let stop = self.current_stop().ok()?;
        let trace = self.backtrace(&stop, context.execution).await.ok()?;
        trace
            .frames
            .iter()
            .find(|frame| frame.id == context.frame)
            .cloned()
    }

    /// Presents what the shared presenter listed as the client's variables.
    fn present_all(
        &mut self,
        listed: Vec<Listed>,
        context: StopContext,
        options: Options,
    ) -> Result<Vec<Map<String, Value>>, ErrorBody> {
        listed
            .into_iter()
            .map(|entry| match entry {
                Listed::Value(row) => Ok(values::variable(
                    *row,
                    context,
                    options,
                    &mut self.references,
                )?),
                Listed::Truncated(description) => Ok(values::truncation(description)),
            })
            .collect()
    }

    fn present(
        &mut self,
        item: Item<'_>,
        context: StopContext,
        options: Options,
    ) -> Result<Map<String, Value>, ErrorBody> {
        let handle = self.target_handle()?;
        let code = self.code();
        let row = presenter(&handle, &code, options).row(item, context);

        Ok(values::variable(
            row,
            context,
            options,
            &mut self.references,
        )?)
    }

    /// How to show values: as the client accepts them, and in
    /// hexadecimal when the request's format or the session says so.
    fn value_options(&self, format: Option<&ValueFormat>) -> Options {
        let support = self.support();
        Options {
            types: support.variable_type,
            memory: support.memory_references,
            hex: format
                .and_then(|format| format.hex)
                .unwrap_or(self.display.hex),
        }
    }

    /// Evaluates an expression for a watch, a hover, the clipboard, or the
    /// debug console. A console line is a command when it starts with a
    /// command's name, unless it is an expression whose first name the frame
    /// knows, so variables such as `x`, `n`, or `list` read as themselves.
    pub(super) async fn evaluate(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<EvaluateArguments>(arguments, "evaluate arguments")?;
        let context = match arguments.frame_id {
            Some(frame) => Some(self.frame_context(frame)?),
            None => self.stop.as_ref().map(Stop::innermost),
        };

        let expression = arguments.expression.trim();
        let repl = arguments.context.as_deref() == Some("repl");
        let command = repl
            .then(|| crate::cli::commands::line_command(expression))
            .flatten();
        let parsed = match (microscope::Expression::parse(expression), command) {
            (Ok(parsed), _) => parsed,
            (Err(_), Some(_)) => return self.console_line(expression, context).await,
            (Err(failure), None) => return Err(expression_failure(expression, &failure, repl)),
        };

        let Some(context) = context else {
            if command.is_some() {
                return self.console_line(expression, None).await;
            }

            return Err(ErrorBody::not_stopped());
        };

        let options = self.value_options(arguments.format.as_ref());
        let handle = self.target_handle()?;
        let mode = if repl {
            microscope::EvaluationMode::Assign
        } else {
            microscope::EvaluationMode::Read
        };

        let evaluation = match handle
            .at(context)
            .evaluate_with(&parsed, mode, microscope::InspectionLimits::default())
            .await
        {
            Ok(evaluation) => evaluation,
            // The frame does not know the command's name, so it is the command.
            Err(microscope::Error::Expression(failure))
                if command.is_some_and(|(_, name)| present::names_only(&failure, name)) =>
            {
                return self.console_line(expression, Some(context)).await;
            }
            Err(microscope::Error::Expression(failure)) => {
                return Err(expression_failure(expression, &failure, repl));
            }
            Err(other) => return Err(error(other)),
        };

        if parsed.assignment_target().is_some() {
            self.forget_reads();
            self.invalidate_values().await;
        }

        // Hover, clipboard, and console show the runtime's multiline text; a
        // watch shows one line.
        let whole = match (&evaluation, arguments.context.as_deref()) {
            (microscope::Evaluation::Value { value, .. }, Some("hover" | "clipboard" | "repl")) => {
                match &value.state {
                    microscope::VariableState::Available {
                        presentation: Some(presentation),
                        ..
                    } => presentation.detail.clone(),
                    _ => None,
                }
            }
            _ => None,
        };

        let mut body = match evaluation {
            microscope::Evaluation::Value { value, .. } => self.present(
                Item {
                    name: expression,
                    path: Some(parsed),
                    raw: false,
                    type_info: value.type_info.as_ref(),
                    state: &value.state,
                    declaration: None,
                },
                context,
                options,
            )?,
            microscope::Evaluation::Range(page) => {
                let length = page.children.len();
                let reference = self.references.variables(Variables::Range {
                    context,
                    expression: parsed,
                })?;
                let mut body = Map::new();
                body.insert("value".to_owned(), format!("[<{length} elements>]").into());
                body.insert("variablesReference".to_owned(), reference.into());
                body.insert("indexedVariables".to_owned(), length.into());
                body
            }
            _ => {
                return Err(ErrorBody::new(
                    "the evaluation produced an unknown kind of result",
                ));
            }
        };

        // An evaluation result names its value `result`.
        body.remove("name");
        body.remove("evaluateName");
        let value = body.remove("value").unwrap_or_default();
        let value = whole.map_or(value, |whole| whole.to_string().into());
        body.insert("result".to_owned(), value);

        Ok(Value::Object(body))
    }

    /// Runs a console command in the frame the client focuses, as the
    /// evaluation of its line.
    async fn console_line(
        &mut self,
        line: &str,
        context: Option<StopContext>,
    ) -> Result<Value, ErrorBody> {
        let console = self.console()?;
        if let Some(context) = context {
            let handle = self.target_handle()?;
            handle
                .select_context(context.execution)
                .await
                .map_err(error)?;
            handle.select_frame(context.frame).await.map_err(error)?;
        }

        let output = console
            .console(line)
            .await
            .map_err(|error| ErrorBody::new(format!("{error:#}")))?
            .ok_or_else(|| ErrorBody::new(format!("'{line}' is not a command")))?;

        // `set` changes values the client shows.
        if crate::cli::commands::line_command(line)
            .is_some_and(|(spec, _)| spec.command == crate::cli::commands::Command::Set)
        {
            self.forget_reads();
            self.invalidate_values().await;
        }

        Ok(json!({"result": output, "variablesReference": 0}))
    }

    /// Where a location reference leads: where a variable is declared, or
    /// where the function a pointer enters is declared, or else the line
    /// of its first instruction.
    pub(super) fn locations(&self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<LocationsArguments>(arguments, "locations arguments")?;
        let reference = arguments.location_reference;
        let (image, location) = match self
            .references
            .location_of(reference)
            .ok_or_else(|| stale("location", reference))?
        {
            Location::Code(address) => {
                let code = self.code();
                let entry = code.function_entry(microscope::VirtualAddress::new(*address));
                let (image, address) = entry.ok_or_else(|| {
                    ErrorBody::new(format!("no loaded module has code at {address:#x}"))
                })?;
                let located = image.locate(address);
                let location = located
                    .function
                    .and_then(|function| function.declaration)
                    .or(located.source)
                    .ok_or_else(|| {
                        ErrorBody::new(format!(
                            "the code at {:#x} has no source location",
                            address.get()
                        ))
                    })?;
                (Arc::clone(image), location)
            }
            Location::Declared { module, location } => (
                self.loaded_image(*module)
                    .ok_or_else(|| ErrorBody::new("the module is no longer loaded"))?,
                location.clone(),
            ),
        };

        let file = image
            .source_file(location.file)
            .ok_or_else(|| ErrorBody::new("the source file is missing from its module"))?;
        let path = self.local_path(&file.path);
        let mut body = json!({
            "source": super::sources::source_json(&path),
            "line": self.line_to_client(location.line.get()),
        });

        if let Some(column) = location.column {
            body["column"] = self.column_to_client(column.get()).into();
        }

        Ok(body)
    }

    pub(super) fn exception_info(&self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<ExceptionInfoArguments>(arguments, "exceptionInfo arguments")?;
        let stop = self.current_stop()?;
        let context = self.thread_ids.context(arguments.thread_id)?;
        if context != stop.context && context != ExecutionContext::Thread(stop.thread) {
            return Err(ErrorBody::new(format!(
                "thread {} did not cause the stop",
                arguments.thread_id
            )));
        }

        let (id, description) = match &stop.reason {
            StopReason::Exception(info)
            | StopReason::CoreDump {
                exception: Some(info),
            } => (signal_text(info.code), info.description.to_string()),
            StopReason::CoreDump { exception: None } => (
                "core dump".to_owned(),
                "the core dump records no signal".to_owned(),
            ),
            StopReason::Exec { followed: true } => (
                "exec".to_owned(),
                "the process executed a program".to_owned(),
            ),
            StopReason::Exec { followed: false } => (
                "exec".to_owned(),
                "the process executed a program microscope cannot load".to_owned(),
            ),
            StopReason::Unclassifiable { description } => {
                ("unclassifiable stop".to_owned(), description.to_string())
            }
            StopReason::WatchpointArmFailed { description, .. } => {
                ("watchpoint failure".to_owned(), description.to_string())
            }
            StopReason::Syscall(stop) => (
                "system call".to_owned(),
                crate::present::syscalls::caught(stop),
            ),
            StopReason::LanguageException(exception) => {
                let id = super::session::language_exception_text(exception.kind).to_owned();
                let mut body = json!({
                    "exceptionId": id,
                    "description": exception.message.as_ref(),
                    "breakMode": if exception.kind == microscope::LanguageExceptionKind::Raised {
                        "always"
                    } else {
                        "unhandled"
                    },
                    "details": {"message": exception.message.as_ref(), "typeName": id},
                });
                if let Some(value) = &exception.value {
                    body["details"]["evaluateName"] = value.as_ref().into();
                }

                return Ok(body);
            }
            StopReason::ProgramBreakpoint {
                address: Some(address),
            } => (
                "program breakpoint".to_owned(),
                format!("the program executed a breakpoint instruction at {address}"),
            ),
            _ => return Err(ErrorBody::new("the stop was not caused by an exception")),
        };

        Ok(json!({
            "exceptionId": id,
            "description": description,
            "breakMode": "always",
            "details": {"message": description, "typeName": id},
        }))
    }
}

impl Session {
    pub(super) async fn set_variable(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<SetVariableArguments>(arguments, "setVariable arguments")?;
        self.current_stop()?;
        let (context, path) = self
            .references
            .child_path(arguments.variables_reference, &arguments.name)
            .ok_or_else(|| {
                ErrorBody::new(format!(
                    "{} cannot be changed: it has no name the debugger can evaluate",
                    arguments.name
                ))
            })?;
        let options = self.value_options(arguments.format.as_ref());
        let register = matches!(
            self.references.variables_of(arguments.variables_reference),
            Some(Variables::Registers { .. })
        );
        let mut set = self
            .assign(context, &arguments.name, path, &arguments.value, options)
            .await?;

        // A register's row shows its bytes, as the registers list does.
        if register
            && let Some(value) = set["value"]
                .as_str()
                .and_then(|value| value.parse::<u64>().ok())
        {
            let bits = set["type"]
                .as_str()
                .and_then(|kind| kind.strip_prefix('u')?.parse::<usize>().ok())
                .unwrap_or(64);
            set["value"] = format!("{value:#0width$x}", width = bits / 4 + 2).into();
        }

        Ok(set)
    }

    pub(super) async fn set_expression(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<SetExpressionArguments>(arguments, "setExpression arguments")?;
        let stop = self.current_stop()?;
        let context = match arguments.frame_id {
            Some(frame) => self.frame_context(frame)?,
            None => stop.innermost(),
        };

        let expression = arguments.expression.trim();
        let parsed = microscope::Expression::parse(expression)
            .map_err(|failure| error(microscope::Error::Expression(failure)))?;
        let options = self.value_options(arguments.format.as_ref());

        self.assign(context, expression, parsed, &arguments.value, options)
            .await
    }

    /// Assigns a value and presents the result as both requests answer.
    async fn assign(
        &mut self,
        context: StopContext,
        name: &str,
        path: microscope::Expression,
        value: &str,
        options: Options,
    ) -> Result<Value, ErrorBody> {
        let handle = self.target_handle()?;
        let assignment = microscope::Expression::parse(&format!("{path} = {value}"))
            .map_err(|failure| error(microscope::Error::Expression(failure)))?;
        let evaluation = handle
            .at(context)
            .evaluate_with(
                &assignment,
                microscope::EvaluationMode::Assign,
                microscope::InspectionLimits::default(),
            )
            .await
            .map_err(error)?;
        let microscope::Evaluation::Value {
            value: assigned, ..
        } = evaluation
        else {
            return Err(ErrorBody::new("the assignment produced no value"));
        };

        self.forget_reads();
        let mut body = self.present(
            Item {
                name,
                path: Some(path),
                raw: false,
                type_info: assigned.type_info.as_ref(),
                state: &assigned.state,
                declaration: None,
            },
            context,
            options,
        )?;

        for key in ["name", "evaluateName", "presentationHint"] {
            body.remove(key);
        }

        self.invalidate_values().await;
        Ok(Value::Object(body))
    }

    /// Completes the debug console's line: a command as its first word, a
    /// command's subcommand or signal, or a name, member, or register
    /// inside an expression.
    pub(super) async fn completions(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<CompletionsArguments>(arguments, "completions arguments")?;

        // Columns count characters; a column past the text completes all of it.
        let column = usize::try_from(arguments.column).unwrap_or(0);
        let offset = if self.support().columns_start_at1 {
            column.saturating_sub(1)
        } else {
            column
        };

        let typed = arguments
            .text
            .char_indices()
            .nth(offset)
            .map_or(arguments.text.as_str(), |(index, _)| {
                &arguments.text[..index]
            });
        let context = match arguments.frame_id {
            Some(frame) => self.references.frame_context(frame),
            None => self.stop.as_ref().map(Stop::innermost),
        };

        let (completing, partial, start) = complete::completing(typed);
        let command = typed.split_whitespace().next().unwrap_or_default();
        let variables = match (context, completing) {
            (Some(context), complete::Completing::Name { .. }) => {
                self.frame_variables(context).await.ok()
            }
            _ => None,
        };

        let handle = self.target_handle().ok();
        let code = self.code();
        let candidates = match &handle {
            Some(handle) => {
                complete::candidates(
                    &presenter(handle, &code, Options::default()),
                    completing,
                    partial,
                    command,
                    context,
                    variables.as_deref(),
                )
                .await
            }
            None => Vec::new(),
        };

        let start = u64::try_from(typed[..start].chars().count()).unwrap_or(0)
            + u64::from(self.support().columns_start_at1);
        let length = partial.chars().count();
        let targets = complete::matching(candidates, partial, MAX_COMPLETIONS)
            .into_iter()
            .map(|(label, kind)| {
                json!({
                    "label": label,
                    "type": kind,
                    "start": start,
                    "length": length,
                })
            })
            .collect::<Vec<_>>();

        Ok(json!({"targets": targets}))
    }
}

/// An expression's mistake: in the console, pointing at the text it is
/// about, as the terminal debugger prints it; elsewhere, on one line.
fn expression_failure(
    text: &str,
    failure: &microscope::ExpressionError,
    console: bool,
) -> ErrorBody {
    if console {
        ErrorBody::new(crate::cli::format::expression_error(text, failure))
    } else {
        error(microscope::Error::Expression(failure.clone()))
    }
}

fn stale(kind: &str, id: i64) -> ErrorBody {
    ErrorBody::new(format!(
        "{kind} reference {id} is stale: it belongs to an earlier stop, or never existed"
    ))
}

const fn presenter<'a>(
    handle: &'a microscope::DebuggerHandle,
    code: &'a present::Code,
    options: Options,
) -> Presenter<'a> {
    Presenter {
        handle,
        code,
        hex: options.hex,
    }
}

fn list_error(failure: ListError) -> ErrorBody {
    match failure {
        ListError::Debugger(failure) => error(failure),
        ListError::Changed(message) => ErrorBody::new(message),
    }
}

/// Every task at the stop, or every one that runs the program's code.
async fn all_tasks(
    handle: &microscope::DebuggerHandle,
    program_only: bool,
) -> Result<Vec<microscope::TaskSnapshot>, ErrorBody> {
    let mut tasks = Vec::new();
    let mut from = None;

    loop {
        let page = if program_only {
            handle.program_tasks(from, TASK_PAGE).await
        } else {
            handle.tasks(from, TASK_PAGE).await
        }
        .map_err(error)?;
        tasks.extend(page.tasks.iter().cloned());
        match page.next {
            Some(next) => from = Some(next),
            None => return Ok(tasks),
        }
    }
}
