//! Launches debuggees in an editor terminal via DAP `runInTerminal`.
//!
//! Spawns `microscope dap-launcher` in the client's terminal, identifies its PID over
//! a private `0o700` Unix socket via `SO_PEERCRED`, attaches before `exec`, and
//! releases the launcher with a 1-byte handshake.

use std::fs::DirBuilder;
use std::io::{self, Write as _};
use std::os::unix::fs::DirBuilderExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use microscope::{DebuggerHandle, ExecutionId, ProcessId};
use serde_json::{Map, Value, json};
use tokio::net::UnixListener;

use super::config::{Console, Launch};
use super::protocol::ErrorBody;
use super::session::Client;

const LAUNCHER_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) async fn launch(
    client: &Client,
    supported: bool,
    handle: &DebuggerHandle,
    launch: &Launch,
    stop_on_entry: bool,
) -> Result<ExecutionId, ErrorBody> {
    if !supported {
        return Err(ErrorBody::shown(
            "this client cannot run programs in a terminal; set \"console\" to \
                 \"internalConsole\"",
        ));
    }

    let program = handle.launched().to_owned();
    let failed = |error: &dyn std::fmt::Display| {
        ErrorBody::shown(format!("failed to launch {}: {error}", program.display()))
    };

    let socket = PrivateSocket::bind("launcher").map_err(|error| failed(&error))?;
    let adapter = std::env::current_exe().map_err(|error| failed(&error))?;
    let mut args = vec![
        adapter.display().to_string(),
        "dap-launcher".to_owned(),
        "--connect".to_owned(),
        socket.path().display().to_string(),
        "--".to_owned(),
        program.display().to_string(),
    ];
    args.extend(
        launch
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned()),
    );
    let cwd = match &launch.working_directory {
        Some(directory) => directory.clone(),
        None => std::env::current_dir().map_err(|error| failed(&error))?,
    };

    let env = launch
        .environment
        .iter()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value
                    .as_ref()
                    .map_or(Value::Null, |value| value.to_string_lossy().into()),
            )
        })
        .collect::<Map<_, _>>();
    let arguments = json!({
        "kind": if launch.console == Console::External { "external" } else { "integrated" },
        "title": program.file_name().map(|name| name.to_string_lossy()),
        "cwd": cwd,
        "args": args,
        "env": env,
    });
    let ran = client.request("runInTerminal", arguments).await?;
    if let Err(message) = ran {
        return Err(failed(&format!(
            "the client could not run it in a terminal: {message}"
        )));
    }

    let (mut launcher, process) = tokio::time::timeout(LAUNCHER_TIMEOUT, socket.accept_process())
        .await
        .map_err(|_| {
            failed(&format!(
                "its launcher did not start within {} seconds",
                LAUNCHER_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|error| failed(&error))?;

    handle
        .launch_by_exec(process, stop_on_entry, move || {
            let _ = launcher.write_all(b"x");
        })
        .await
        .map_err(|error| failed(&error))
}

/// Unix listener inside a `0o700` temp directory so only the same user can connect.
pub(super) struct PrivateSocket {
    directory: PathBuf,
    name: &'static str,
    pub(super) listener: UnixListener,
}

impl PrivateSocket {
    pub(super) fn bind(name: &'static str) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir();

        let directory = loop {
            let directory = base.join(format!(
                "microscope-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));

            match DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => break directory,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        };

        let socket = Self {
            listener: UnixListener::bind(directory.join(name))?,
            directory,
            name,
        };

        Ok(socket)
    }

    pub(super) fn path(&self) -> PathBuf {
        self.directory.join(self.name)
    }

    async fn accept_process(&self) -> io::Result<(std::os::unix::net::UnixStream, ProcessId)> {
        let (stream, _) = self.listener.accept().await?;
        let process = stream
            .peer_cred()?
            .pid()
            .and_then(|pid| u64::try_from(pid).ok())
            .ok_or_else(|| io::Error::other("its launcher's process is unknown"))?;
        let stream = stream.into_std()?;
        stream.set_nonblocking(false)?;
        Ok((stream, ProcessId::new(process)))
    }
}

impl Drop for PrivateSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
