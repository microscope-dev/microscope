//! Launching a program in the client's terminal with `runInTerminal`.
//!
//! The client runs `microscope dap-launcher` in a terminal. The launcher
//! connects to a socket in a directory only this user can enter, which
//! identifies its process, and waits; once the adapter traces that process,
//! it is released to execute the program, whose exec completes the launch.

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

/// How long the launcher may take to start once the client ran it.
const LAUNCHER_TIMEOUT: Duration = Duration::from_secs(10);

/// Launches the program through a launcher the client runs in a terminal,
/// returning the launch's execution.
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

    let program = handle.executable().to_owned();
    let failed = |error: &dyn std::fmt::Display| {
        ErrorBody::shown(format!("failed to launch {}: {error}", program.display()))
    };

    let socket = LauncherSocket::bind().map_err(|error| failed(&error))?;
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

    let (mut launcher, process) = tokio::time::timeout(LAUNCHER_TIMEOUT, socket.accept())
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
            // A launcher that is gone exits without running the program.
            let _ = launcher.write_all(b"x");
        })
        .await
        .map_err(|error| failed(&error))
}

/// The socket a launcher connects to, in a directory only this user can
/// enter, removed when dropped.
struct LauncherSocket {
    directory: PathBuf,
    listener: UnixListener,
}

impl LauncherSocket {
    fn bind() -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir();

        let directory = loop {
            let directory = base.join(format!(
                "microscope-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));

            // Creating the directory, never reusing one, keeps another
            // user's directory of the same name from being trusted.
            match DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => break directory,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        };

        let socket = Self {
            listener: UnixListener::bind(directory.join("launcher"))?,
            directory,
        };

        Ok(socket)
    }

    fn path(&self) -> PathBuf {
        self.directory.join("launcher")
    }

    /// Accepts the launcher's connection, returning it and its process.
    async fn accept(&self) -> io::Result<(std::os::unix::net::UnixStream, ProcessId)> {
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

impl Drop for LauncherSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
