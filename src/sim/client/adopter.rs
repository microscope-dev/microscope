//! A session adopting a child the first session held, as a client starts
//! one for each `startDebugging` request: it attaches to the child, then
//! continues it, pauses it, or waits for it, and shuts down, which detaches
//! it, at a stop or while it runs.

use std::cell::RefCell;
use std::rc::Rc;

use tokio::sync::{broadcast, oneshot};

use super::protocol;
use crate::backend::ControllerMessage;
use crate::protocol::Request;
use crate::sim::choices::{Choices, Stream};
use crate::sim::marks::{Mark, Marks};
use crate::sim::report::Failure;
use crate::{
    DebuggerEvent, DebuggerHandle, Error, ExceptionDisposition, ExecutionId, HeldProcess,
    InferiorState, ResumeScope,
};

pub struct Adopter {
    pub handle: DebuggerHandle,
    pub choices: Rc<RefCell<Choices>>,
    pub marks: Rc<RefCell<Marks>>,

    /// What the session did, for the trace.
    pub notes: Rc<RefCell<Vec<String>>>,

    /// The child the session adopts.
    pub child: HeldProcess,

    /// The most requests the session makes before it shuts down.
    pub requests: u64,
}

impl Adopter {
    fn draw(&self, bound: u64) -> u64 {
        self.choices.borrow_mut().below(Stream::Client, bound)
    }

    fn note(&self, note: impl Into<String>) {
        self.notes.borrow_mut().push(note.into());
    }

    /// Runs the session to its end: the attach, requests, then a shutdown.
    pub async fn run(self) -> Result<(), Failure> {
        let mut events = self.handle.subscribe();
        self.attach().await?;

        let mut continued = false;

        for _ in 0..self.requests {
            let snapshot = self
                .handle
                .snapshot()
                .await
                .map_err(|error| protocol(format!("snapshot failed: {error}")))?;

            match snapshot.inferior {
                InferiorState::NotRunning => break,
                InferiorState::Running { execution_id, .. } => match execution_id {
                    Some(execution) if self.draw(3) != 0 => {
                        self.wait_for(execution, &mut events).await?;
                    }
                    _ => self.pause().await?,
                },
                InferiorState::Stopped {
                    process_id,
                    stop_id,
                    ..
                } => {
                    if self.draw(3) == 0 {
                        break;
                    }

                    let execution = self
                        .handle
                        .continue_execution(
                            stop_id,
                            ResumeScope::Process(process_id),
                            ExceptionDisposition::Pass,
                        )
                        .await
                        .map_err(|error| {
                            protocol(format!("continue from {stop_id} failed: {error}"))
                        })?;
                    self.note(format!("continued from {stop_id} as execution {execution}"));
                    continued = true;
                    self.wait_for(execution, &mut events).await?;
                }
            }
        }

        if !continued {
            self.marks
                .borrow_mut()
                .hit(Mark::AdoptedChildDetachedAtOnce);
        }

        self.shutdown().await
    }

    /// Attaches to the held child, which nothing else can take or end
    /// meanwhile.
    async fn attach(&self) -> Result<(), Failure> {
        let (reply, answer) = oneshot::channel();
        self.handle
            .requests
            .send(ControllerMessage::Request(Request::Attach {
                process_id: self.child.process_id,
                held: true,
                reply,
            }))
            .await
            .map_err(|_| protocol("the request queue closed before the attach"))?;

        let stop = answer
            .await
            .map_err(|_| protocol("the attach was never answered"))?
            .map_err(|error| protocol(format!("attaching to a held child failed: {error}")))?;

        self.marks.borrow_mut().hit(Mark::Adopted);
        self.note(format!(
            "attached to held child {} at stop {stop}",
            self.child.process_id
        ));
        Ok(())
    }

    /// Pauses the child, which may stop or end first.
    async fn pause(&self) -> Result<(), Failure> {
        match self.handle.pause().await {
            Ok(reason) => self.note(format!("paused: {reason:?}")),
            Err(Error::AlreadyStopped | Error::NotRunning | Error::EventStreamLagged(_)) => {}
            Err(error) => return Err(protocol(format!("pause failed: {error}"))),
        }

        Ok(())
    }

    /// Waits until `execution` stops or ends.
    async fn wait_for(
        &self,
        execution: ExecutionId,
        events: &mut broadcast::Receiver<DebuggerEvent>,
    ) -> Result<(), Failure> {
        loop {
            match events.recv().await {
                Ok(
                    DebuggerEvent::InferiorStopped {
                        execution_id: Some(ended),
                        ..
                    }
                    | DebuggerEvent::InferiorExited {
                        execution_id: Some(ended),
                        ..
                    },
                ) if ended == execution => return Ok(()),
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let snapshot = self
                        .handle
                        .snapshot()
                        .await
                        .map_err(|error| protocol(format!("snapshot failed: {error}")))?;
                    if !matches!(
                        snapshot.inferior,
                        InferiorState::Running { execution_id: Some(running), .. }
                            if running == execution
                    ) {
                        return Ok(());
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(protocol("the event channel closed"));
                }
            }
        }
    }

    /// Shuts the session down, which detaches a child still attached.
    async fn shutdown(&self) -> Result<(), Failure> {
        self.note("shutdown");

        let (reply, answer) = oneshot::channel();
        self.handle
            .requests
            .send(ControllerMessage::Request(Request::Shutdown { reply }))
            .await
            .map_err(|_| protocol("the request queue closed before shutdown"))?;

        answer
            .await
            .map_err(|_| protocol("shutdown was never answered"))?
            .map_err(|error| protocol(format!("shutdown failed: {error}")))
    }
}
