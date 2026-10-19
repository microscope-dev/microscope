//! A task's future, which its cell holds while the task has not finished.
//!
//! A task's vtable polls it through `raw::poll::<T, S>`, whose name in the
//! debug information spells the future's type `T` and the scheduler's `S`.
//! The task's cell is the `Cell<T, S>` of the same arguments, and its
//! `core.stage` holds the future in its `Running` variant.

use std::sync::Arc;

use super::{TokioRuntime, records};
use crate::runtime_model::futures::{self, AsyncFrameKind};
use crate::runtime_model::records::Sum;
use crate::runtime_model::{RuntimeImage, RuntimeStop};
use crate::{ImageAddress, TypeReference, VirtualAddress};

/// Where the future is in the cells one poll function polls.
#[derive(Debug, Clone)]
pub(super) struct FutureLayout {
    /// The cell's stage, from its header.
    stage: u64,
    stages: Sum,

    /// The future, from the stage.
    future: u64,
    ty: TypeReference,
}

impl FutureLayout {
    fn bind(image: &dyn RuntimeImage, poll: ImageAddress) -> Result<Self, Arc<str>> {
        let name = image
            .function_name(poll)
            .ok_or("the debug information names no function that polls the task")?;
        let arguments = name
            .strip_prefix("poll")
            .filter(|arguments| arguments.starts_with('<'))
            .ok_or_else(|| format!("{name} polls no task"))?;
        let cell = records::named(
            image,
            &format!("tokio::runtime::task::core::Cell{arguments}"),
        )?;
        let header = records::field(image, cell, &["header"])?;
        let stage = records::field(image, cell, &["core", "stage", "stage", "__0", "value"])?;
        let stages = records::sum(image, stage.ty)?;
        let running = stages.variant("Running")?.payload;
        let future = records::field(image, running.ty, &["__0"])?;

        Ok(Self {
            stage: stage
                .offset
                .checked_sub(header.offset)
                .ok_or("a task's stage lies before its header")?,
            future: records::within(running.offset, future.offset)?,
            stages,
            ty: future.ty,
        })
    }
}

/// The functions that poll a future no task holds, each with the variable
/// that holds it: the thread parker that multi-thread runtimes' `block_on`
/// and every `Handle::block_on` park on, and the current-thread scheduler's
/// `block_on`, which polls the future between its tasks.
const DRIVERS: [(&str, &str); 3] = [
    (
        "tokio::runtime::scheduler::current_thread::CoreGuard::block_on::{closure#0}",
        "future",
    ),
    ("tokio::runtime::park::CachedParkThread::block_on", "f"),
    (
        "tokio::runtime::scheduler::current_thread::CurrentThread::block_on",
        "future",
    ),
];

/// The variable that holds the future `frame` drives, if its function is
/// one of tokio's functions that drive one. Only the function's debug
/// information can read the variable.
pub(super) fn driven_future(frame: &crate::StackFrame) -> Option<&'static str> {
    let function = frame.function.as_ref()?;
    let demangled = crate::demangle::demangle(function.linkage_name.as_deref()?)?;
    let path = crate::demangle::rust_path(&demangled)?;
    DRIVERS
        .iter()
        .find(|(driver, _)| *driver == path)
        .map(|(_, variable)| *variable)
}

impl TokioRuntime {
    /// The future of the task whose header is at `header`, and its type,
    /// or why the task has none.
    pub(super) fn future(
        &self,
        stop: &dyn RuntimeStop,
        header: VirtualAddress,
    ) -> Result<(VirtualAddress, TypeReference), Arc<str>> {
        let tasks = self.task_layout()?;
        let unreadable = || Arc::<str>::from(format!("the task at {header:#x} is unreadable"));

        let vtable =
            records::pointer(stop, header.wrapping_add(tasks.vtable)).ok_or_else(unreadable)?;
        let poll =
            records::pointer(stop, vtable.wrapping_add(tasks.poll)).ok_or_else(unreadable)?;
        let poll = stop.load_bias().wrapping_unload(poll);

        let layout = self
            .futures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(poll)
            .or_insert_with(|| FutureLayout::bind(self.image.as_ref(), poll))
            .clone()?;

        let stage = header.wrapping_add(layout.stage);
        let variant = layout.stages.active(stop, stage)?;
        match &*variant.name {
            "Running" => Ok((stage.wrapping_add(layout.future), layout.ty)),
            "Finished" => Err("the task has finished, and holds its output".into()),
            _ => Err("the task's output has been taken".into()),
        }
    }

    /// Where the function that runs a task's outermost coroutine begins,
    /// once the future that holds it is past.
    pub(super) fn entry(
        &self,
        stop: &dyn RuntimeStop,
        header: VirtualAddress,
    ) -> Option<VirtualAddress> {
        let (future, ty) = self.future(stop, header).ok()?;
        let chain = futures::walk(self.image.as_ref(), stop, future, ty);
        let outermost = chain.frames.last()?;
        let AsyncFrameKind::Coroutine { .. } = outermost.kind else {
            return None;
        };

        let body = *self
            .bodies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(outermost.ty)
            .or_insert_with(|| self.image.coroutine_body(outermost.ty))
            .as_ref()?;
        Some(stop.load_bias().wrapping_load(body))
    }
}
