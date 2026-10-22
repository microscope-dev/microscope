//! A process group's members, each shown in a child session of its own.
//!
//! The adapter that launched or attached the group's program hosts every
//! member. For each member that joins, its session asks the client to start
//! a child session whose configuration names the member and a socket of
//! this adapter's; the editor connects that session back to the socket, as
//! the VS Code extension's descriptor factory and nvim-dap's adapter
//! function do, and this adapter serves it from the member's handle. No
//! process changes tracer, so nothing waits for a session to attach.

use std::path::PathBuf;

use microscope::{GroupHandle, Member};
use serde_json::{Map, Value, json};
use tokio::io::BufReader;
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

use super::Log;
use super::config::MEMBER_KEY;
use super::terminal::PrivateSocket;

/// What a session connected to a group's socket shows: one of the group's
/// members, which its `attach` names.
#[derive(Clone)]
pub struct Route {
    pub group: GroupHandle,
    pub socket: PathBuf,
}

/// The socket the sessions of a group's members connect to, and the
/// sessions it serves.
pub(super) struct Routes {
    socket: PathBuf,

    /// Dropped to end the accept loop and every member's session.
    closing: watch::Sender<()>,
    task: JoinHandle<()>,
}

impl Routes {
    /// Listens for the sessions of `group`'s members.
    pub(super) fn open(group: GroupHandle, log: Log) -> std::io::Result<Self> {
        let listener = PrivateSocket::bind("group")?;
        let socket = listener.path();
        let (closing, closed) = watch::channel(());
        let route = Route {
            group,
            socket: socket.clone(),
        };
        let task = tokio::spawn(accept(listener, route, log, closed));
        Ok(Self {
            socket,
            closing,
            task,
        })
    }

    /// Ends every member's session, once each has told its client.
    pub(super) async fn close(self) {
        drop(self.closing);
        let _ = self.task.await;
    }

    /// The `startDebugging` arguments for `member`'s session, with the
    /// settings the root's configuration passes on.
    pub(super) fn start_arguments(
        &self,
        inherited: &Map<String, Value>,
        program: &str,
        member: &Member,
    ) -> Value {
        let mut configuration = inherited.clone();
        let process = member
            .process
            .map_or_else(String::new, |process| format!(" process {process}"));
        configuration.insert(
            "name".to_owned(),
            format!("{program} [{}]{process}", member.number).into(),
        );
        if let Some(process) = member.process {
            configuration.insert("pid".to_owned(), process.get().into());
        }
        configuration.insert(
            MEMBER_KEY.to_owned(),
            json!({"socket": self.socket, "number": member.number}),
        );
        json!({"request": "attach", "configuration": configuration})
    }
}

/// Serves each session that connects until the group's session closes the
/// routes, then waits for those sessions to end.
async fn accept(listener: PrivateSocket, route: Route, log: Log, closed: watch::Receiver<()>) {
    let mut sessions = JoinSet::new();
    let mut closing = closed.clone();
    let mut connections = 0_u64;
    loop {
        let stream = tokio::select! {
            _ = closing.changed() => break,
            accepted = listener.listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(_) => break,
            },
        };

        let (read, write) = stream.into_split();
        let mut ended = closed.clone();
        connections += 1;
        // The members' sessions share the root's log, each line labelled
        // so that a replay of the root's traffic passes over them.
        sessions.spawn(super::serve(
            BufReader::new(read),
            write,
            None,
            log.labelled(format!("[member session {connections}]")),
            async move {
                let _ = ended.changed().await;
            },
            Some(route.clone()),
        ));
    }

    while sessions.join_next().await.is_some() {}
}
