//! The Debug Adapter Protocol server, `microscope dap`.
//!
//! Editors such as VS Code, Neovim, Zed, Helix, and Emacs talk to it over
//! stdin and stdout, or over TCP with `--port`. Like the CLI, it is one
//! client of [`microscope::DebuggerHandle`].

mod breakpoints;
mod config;
mod forks;
mod handles;
mod inspect;
pub mod launcher;
mod memory;
pub mod output;
mod protocol;
mod session;
mod signals;
mod sources;
mod terminal;
mod threads;
mod transport;
mod values;
mod watch;

use std::fs::File;
use std::io::Write as _;
use std::net::SocketAddr;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncWrite, BufReader};
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use protocol::{Incoming, MessageError, Outgoing};
use session::{Client, Inbound, Session};

/// Serves the Debug Adapter Protocol for one client at a time.
#[derive(clap::Args)]
pub struct DapArgs {
    /// Listen on 127.0.0.1:PORT instead of using stdin and stdout.
    #[arg(long, value_name = "PORT", conflicts_with = "listen")]
    port: Option<u16>,

    /// Listen on ADDRESS, such as 127.0.0.1:4711, instead of using stdin
    /// and stdout. Clients are served one at a time.
    #[arg(long, value_name = "ADDRESS")]
    listen: Option<SocketAddr>,

    /// Append every message to FILE.
    #[arg(long, value_name = "FILE")]
    log: Option<PathBuf>,
}

/// Appends messages to the `--log` file.
#[derive(Clone, Default)]
struct Log(Option<Arc<Mutex<File>>>);

impl Log {
    fn record(&self, direction: &str, text: &str) {
        if let Some(file) = &self.0
            && let Ok(mut file) = file.lock()
        {
            let _ = writeln!(file, "{direction} {text}");
        }
    }
}

/// Runs the adapter until its client disconnects, or with `--port` and
/// `--listen` until it is interrupted.
pub async fn run(args: &DapArgs) -> Result<()> {
    let log = match &args.log {
        Some(path) => Log(Some(Arc::new(Mutex::new(
            File::options()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("failed to open log {}", path.display()))?,
        )))),
        None => Log::default(),
    };

    let address = args.listen.or_else(|| {
        args.port
            .map(|port| SocketAddr::from(([127, 0, 0, 1], port)))
    });

    match address {
        Some(address) => listen(address, log).await,
        None => serve_stdio(log).await,
    }
}

/// Completes when the adapter is asked to stop: an editor closing the
/// session may send SIGTERM, SIGINT, or SIGHUP instead of disconnecting.
/// The signals are caught from this call on, before the future is polled.
pub fn termination() -> impl Future<Output = ()> {
    let signals = [
        SignalKind::terminate(),
        SignalKind::interrupt(),
        SignalKind::hangup(),
    ]
    .map(|kind| signal(kind).ok());
    terminated(signals)
}

async fn terminated(signals: [Option<tokio::signal::unix::Signal>; 3]) {
    let [mut terminate, mut interrupt, mut hangup] = signals;
    let wait = |signal: Option<tokio::signal::unix::Signal>| async move {
        match signal {
            Some(mut signal) => {
                signal.recv().await;
            }
            None => std::future::pending().await,
        }
    };

    tokio::select! {
        () = wait(terminate.take()) => {}
        () = wait(interrupt.take()) => {}
        () = wait(hangup.take()) => {}
    }
}

/// Serves one client over stdin and stdout. Anything else that writes to
/// stdout or stderr, such as a panic message, is redirected to the client's
/// console so it cannot corrupt the protocol stream.
async fn serve_stdio(log: Log) -> Result<()> {
    let protocol_out = nix::unistd::dup(std::io::stdout()).context("failed to duplicate stdout")?;
    let (console_read, console_write) = output::pipe().context("failed to create a pipe")?;
    nix::unistd::dup2_stdout(&console_write).context("failed to redirect stdout")?;
    nix::unistd::dup2_stderr(&console_write).context("failed to redirect stderr")?;
    drop(console_write);
    let writer = tokio::fs::File::from_std(File::from(protocol_out));
    let reader = BufReader::new(tokio::io::stdin());
    serve(reader, writer, Some(console_read), log, termination()).await;
    Ok(())
}

async fn listen(address: SocketAddr, log: Log) -> Result<()> {
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to listen on {address}"))?;
    eprintln!("microscope dap listening on {}", listener.local_addr()?);
    let stop = termination();
    tokio::pin!(stop);

    loop {
        let (stream, peer) = tokio::select! {
            () = &mut stop => return Ok(()),
            accepted = listener.accept() => accepted.context("failed to accept a client")?,
        };

        log.record("--", &format!("client {peer} connected"));
        let (read, write) = stream.into_split();

        // A session ends early when the adapter is asked to stop.
        let (stopped, stopping) = tokio::sync::oneshot::channel::<()>();
        let session = serve(BufReader::new(read), write, None, log.clone(), async {
            let _ = stopping.await;
        });
        tokio::pin!(session);

        let interrupted = tokio::select! {
            () = &mut session => false,
            () = &mut stop => {
                let _ = stopped.send(());
                session.await;
                true
            }
        };

        log.record("--", &format!("client {peer} disconnected"));
        if interrupted {
            return Ok(());
        }
    }
}

/// Serves one client: reads its messages, runs its session, and writes the
/// session's messages back in order.
async fn serve<R, W>(
    reader: R,
    writer: W,
    console: Option<OwnedFd>,
    log: Log,
    shutdown: impl std::future::Future<Output = ()>,
) where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (outgoing, messages) = mpsc::channel(256);
    let client = Client::new(outgoing);
    let writer = tokio::spawn(write_messages(
        writer,
        messages,
        log.clone(),
        client.responses(),
    ));

    let (inbound, inbox) = mpsc::channel(64);
    let cancelled = session::Cancelled::default();
    let reader = tokio::spawn(read_messages(
        reader,
        inbound,
        log,
        client.clone(),
        cancelled.clone(),
    ));
    let console = console.and_then(|read| output::spawn(read, "console", client.clone()).ok());

    Session::new(client, cancelled).run(inbox, shutdown).await;
    reader.abort();
    if let Some(console) = console {
        console.abort();
        let _ = console.await;
    }

    let _ = reader.await;
    let _ = writer.await;
}

/// Reads the client's messages until the stream ends or breaks framing,
/// then fails the reverse requests still awaiting a response.
async fn read_messages<R: AsyncBufRead + Unpin>(
    reader: R,
    inbound: mpsc::Sender<Inbound>,
    log: Log,
    client: Client,
    cancelled: session::Cancelled,
) {
    let responses = client.responses();
    read_frames(reader, inbound, log, client, cancelled, &responses).await;
    if let Ok(mut responses) = responses.lock() {
        responses.close();
    }
}

async fn read_frames<R: AsyncBufRead + Unpin>(
    mut reader: R,
    inbound: mpsc::Sender<Inbound>,
    log: Log,
    client: Client,
    cancelled: session::Cancelled,
    responses: &Mutex<session::Responses>,
) {
    loop {
        let frame = match transport::read_frame(&mut reader).await {
            Ok(Some(frame)) => frame,
            Ok(None) => return,
            Err(error) => {
                log.record("!!", &error.to_string());
                return;
            }
        };

        if frame.origin {
            // Only a web page sends Origin; it must not drive a debugger.
            log.record("!!", "refused a message with an Origin header");
            return;
        }

        log.record("<-", &String::from_utf8_lossy(&frame.body));
        let message = match protocol::parse(&frame.body) {
            Ok(Incoming::Request {
                seq,
                command,
                arguments,
            }) => {
                // A cancellation must be seen before the session reaches the
                // request it cancels, so the reader records it.
                if command == "cancel"
                    && let Some(request) = arguments.get("requestId").filter(|id| !id.is_null())
                    && let Ok(mut cancelled) = cancelled.lock()
                {
                    cancelled.insert(request.to_string());
                }

                Inbound::Request {
                    seq,
                    command,
                    arguments,
                }
            }

            Ok(Incoming::Response {
                request_seq: Some(seq),
                result,
            }) => {
                if let Ok(mut responses) = responses.lock() {
                    responses.settle(seq, result);
                }

                continue;
            }
            Ok(Incoming::Response { .. }) => continue,
            Err(error) => {
                if let Some((seq, command)) = MessageError::request_seq(&frame.body) {
                    Inbound::Malformed {
                        seq,
                        command,
                        message: error.to_string(),
                    }
                } else {
                    let _ = client
                        .important(format!("microscope ignored a malformed message: {error}"))
                        .await;
                    continue;
                }
            }
        };

        if inbound.send(message).await.is_err() {
            return;
        }
    }
}

/// Numbers and writes the session's messages until every sender is gone.
async fn write_messages<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut messages: mpsc::Receiver<Outgoing>,
    log: Log,
    responses: Arc<Mutex<session::Responses>>,
) {
    let mut seq = 0_u64;

    while let Some(message) = messages.recv().await {
        seq += 1;
        if let Outgoing::Request { ticket, .. } = &message
            && let Ok(mut responses) = responses.lock()
        {
            responses.sent(*ticket, seq);
        }

        let body: Value = message.to_json(seq);
        let text = body.to_string();
        log.record("->", &text);
        if transport::write_frame(&mut writer, text.as_bytes())
            .await
            .is_err()
        {
            return;
        }
    }
}
