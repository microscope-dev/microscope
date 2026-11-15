use std::path::PathBuf;

use microscope::{GroupHandle, Member};
use serde_json::{Map, Value, json};
use tokio::io::BufReader;
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

use super::Log;
use super::config::MEMBER_KEY;
use super::terminal::PrivateSocket;

#[derive(Clone)]
pub struct Route {
    pub group: GroupHandle,
    pub socket: PathBuf,
}

pub(super) struct Routes {
    socket: PathBuf,

    closing: watch::Sender<()>,
    task: JoinHandle<()>,
}

impl Routes {
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

    pub(super) async fn close(self) {
        drop(self.closing);
        let _ = self.task.await;
    }

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
