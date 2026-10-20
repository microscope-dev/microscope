//! One connection to the service: hello, then uploads and commands until
//! either side ends it.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use prost::bytes::Bytes;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
use tokio::net::TcpStream;
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode, header};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use super::commands::{self, Recorder};
use super::outbox::{Key, Outbox, Watched};
use super::proto::{
    AgentMessage, CommandError, CommandResult, Hello, ProcessList, Protocol, RecordStarted,
    RecordingKind, ServerMessage, UploadBegin, UploadChunk, UploadEnd, agent_message, command,
    command_result, goodbye, server_message,
};
use super::wire::{Codec, MAX_COMPRESSED_BYTES};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The largest chunk the agent sends, whatever the service allows.
const MAX_CHUNK_BYTES: usize = 256 << 10;

/// How long connecting, TLS and the upgrade included, may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long closing the connection may take.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// How often watched directories are scanned for new segments.
const SCAN_EVERY: Duration = Duration::from_secs(2);

/// How long a recording the service could not take waits before the next
/// try.
const HOLD: Duration = Duration::from_secs(60);

/// A capture file's first bytes.
const CAPTURE_MAGIC: &[u8; 8] = b"USCOPEPF";

/// How long the agent waits on the service.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// For the Welcome that answers Hello.
    pub hello: Duration,
    /// For any message at all. The service pings every 20 seconds.
    pub idle: Duration,
}

/// How a connection ended.
#[derive(Debug)]
pub enum End {
    /// The agent was asked to stop.
    Shutdown,
    /// The connection failed or closed; connect again after a wait.
    Retry(String),
    /// The service refused the agent, and connecting again will not help.
    Fatal(String),
}

/// What outlives each connection.
#[derive(Debug)]
pub struct Agent {
    pub server: String,
    pub endpoint: String,
    pub token: String,
    pub hostname: String,
    pub allow_remote: bool,
    pub timeouts: Timeouts,
    pub outbox: Outbox,
    pub watched: Vec<Watched>,
    /// Woken when a recording is queued.
    pub uploads: Arc<Notify>,
    pub recorder: Arc<Recorder>,
    /// Recordings the service could not take, until they may be tried again.
    pub held: Mutex<BTreeMap<Key, Instant>>,
}

/// The upload under way on a connection.
#[derive(Debug)]
struct Upload {
    key: Key,
    name: String,
    file: tokio::fs::File,
    size: u64,
    stage: Stage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Waiting for the service to say where to start.
    Begun,
    /// Sending: every byte before `acked` is stored, and every byte before
    /// `sent` is sent.
    Sending { acked: u64, sent: u64 },
    /// Every byte is sent, and the service is checking them.
    Ended,
}

/// What the service said in its Welcome.
#[derive(Debug, Clone, Copy)]
struct Limits {
    chunk: u64,
    window: u64,
}

impl Agent {
    /// Connects, says hello, and serves the connection until it ends.
    /// Returns when the service welcomed the agent, if it did.
    pub async fn serve(&self, shutdown: &mut watch::Receiver<bool>) -> (End, Option<Instant>) {
        let mut socket = tokio::select! {
            connected = self.connect() => match connected {
                Ok(socket) => socket,
                Err(end) => return (end, None),
            },
            _ = shutdown.changed() => return (End::Shutdown, None),
        };
        let mut codec = match Codec::new() {
            Ok(codec) => codec,
            Err(error) => return (End::Fatal(error.to_string()), None),
        };

        let limits = tokio::select! {
            welcomed = self.hello(&mut socket, &mut codec) => match welcomed {
                Ok(limits) => limits,
                Err(end) => return (end, None),
            },
            _ = shutdown.changed() => {
                close(&mut socket, CloseCode::Away, "shutting down").await;
                return (End::Shutdown, None);
            }
        };

        let welcomed = Instant::now();
        let end = self.run(&mut socket, &mut codec, limits, shutdown).await;
        (end, Some(welcomed))
    }

    async fn connect(&self) -> Result<Socket, End> {
        let mut request = self
            .endpoint
            .as_str()
            .into_client_request()
            .map_err(|error| End::Fatal(format!("{}: {error}", self.endpoint)))?;
        let authorization = HeaderValue::from_str(&format!("Bearer {}", self.token))
            .map_err(|_| End::Fatal("the device token is not a valid header".to_owned()))?;
        request
            .headers_mut()
            .insert(header::AUTHORIZATION, authorization);
        request.headers_mut().insert(
            header::USER_AGENT,
            HeaderValue::from_static(concat!("microscope/", env!("CARGO_PKG_VERSION"))),
        );

        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_COMPRESSED_BYTES))
            .max_frame_size(Some(MAX_COMPRESSED_BYTES));
        let tls = tls().map_err(|error| End::Fatal(format!("setting up TLS failed: {error}")))?;
        let connecting = tokio_tungstenite::connect_async_tls_with_config(
            request,
            Some(config),
            false,
            Some(Connector::Rustls(tls)),
        );

        match tokio::time::timeout(CONNECT_TIMEOUT, connecting).await {
            Ok(Ok((socket, _))) => Ok(socket),
            Ok(Err(tungstenite::Error::Http(response))) => Err(refused(response.status())),
            Ok(Err(error)) => Err(End::Retry(format!(
                "connecting to {} failed: {error}",
                self.server
            ))),
            Err(_) => Err(End::Retry(format!(
                "connecting to {} timed out",
                self.server
            ))),
        }
    }

    async fn hello(&self, socket: &mut Socket, codec: &mut Codec) -> Result<Limits, End> {
        let hello = agent_message::Body::Hello(Hello {
            protocol: Protocol::V1.into(),
            agent_version: env!("CARGO_PKG_VERSION").to_owned(),
            hostname: self.hostname.clone(),
            accepts_commands: self.allow_remote,
        });
        send(socket, codec, hello).await?;

        let message = tokio::time::timeout(self.timeouts.hello, receive(socket, codec))
            .await
            .map_err(|_| End::Retry("the service did not answer hello".to_owned()))??;
        match message.body {
            Some(server_message::Body::Welcome(welcome)) => {
                eprintln!(
                    "microscope: connected to {} as {}",
                    self.server, welcome.account_email
                );
                let chunk = u64::from(welcome.max_chunk_bytes).clamp(1, MAX_CHUNK_BYTES as u64);
                Ok(Limits {
                    chunk,
                    window: welcome.window_bytes.max(chunk),
                })
            }
            Some(server_message::Body::Goodbye(goodbye)) => Err(farewell(&goodbye)),
            _ => {
                close(socket, CloseCode::Protocol, "expected welcome").await;
                Err(End::Retry(
                    "the service sent something other than a welcome".to_owned(),
                ))
            }
        }
    }

    async fn run(
        &self,
        socket: &mut Socket,
        codec: &mut Codec,
        limits: Limits,
        shutdown: &mut watch::Receiver<bool>,
    ) -> End {
        let (results, mut answered) = mpsc::channel(16);
        let mut upload: Option<Upload> = None;
        let mut heard = Instant::now();
        let mut scan = tokio::time::interval(SCAN_EVERY);
        scan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            if let Err(end) = self.advance(socket, codec, limits, &mut upload).await {
                return end;
            }

            tokio::select! {
                message = socket.next() => {
                    heard = Instant::now();
                    let message = match message {
                        Some(Ok(message)) => message,
                        Some(Err(error)) => return End::Retry(format!("the connection failed: {error}")),
                        None => return End::Retry("the service closed the connection".to_owned()),
                    };
                    let message = match message {
                        Message::Binary(frame) => match codec.decode::<ServerMessage>(&frame) {
                            Ok(message) => message,
                            Err(error) => {
                                close(socket, CloseCode::Protocol, "undecodable message").await;
                                return End::Retry(format!("the service sent a bad message: {error}"));
                            }
                        },
                        Message::Close(_) => return End::Retry("the service closed the connection".to_owned()),
                        Message::Text(_) => {
                            close(socket, CloseCode::Protocol, "text message").await;
                            return End::Retry("the service sent a text message".to_owned());
                        }
                        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                    };
                    if let Err(end) = self.handle(socket, message, &mut upload, &results).await {
                        return end;
                    }
                }
                Some(result) = answered.recv() => {
                    if let Err(end) = send(socket, codec, agent_message::Body::CommandResult(result)).await {
                        return end;
                    }
                }
                () = self.uploads.notified() => {}
                _ = scan.tick() => self.scan(),
                () = tokio::time::sleep_until(heard + self.timeouts.idle) => {
                    return End::Retry("the service stopped answering".to_owned());
                }
                _ = shutdown.changed() => {
                    close(socket, CloseCode::Away, "shutting down").await;
                    return End::Shutdown;
                }
            }
        }
    }

    /// Queues the segments watched directories gained.
    fn scan(&self) {
        let mut added = 0;
        for watched in &self.watched {
            match watched.scan(&self.outbox) {
                Ok(count) => added += count,
                Err(error) => eprintln!(
                    "microscope: watching {} failed: {error:#}",
                    watched.dir().display()
                ),
            }
        }
        if added > 0 {
            self.uploads.notify_one();
        }
        self.outbox.prune();
    }

    /// Begins the next upload, or sends as much of the current one as the
    /// window allows.
    async fn advance(
        &self,
        socket: &mut Socket,
        codec: &mut Codec,
        limits: Limits,
        upload: &mut Option<Upload>,
    ) -> Result<(), End> {
        if upload.is_none() {
            *upload = self.begin(socket, codec).await?;
        }
        let Some(current) = upload else {
            return Ok(());
        };

        while let Stage::Sending { acked, sent } = current.stage {
            if sent == current.size {
                send(
                    socket,
                    codec,
                    agent_message::Body::UploadEnd(UploadEnd {
                        upload_key: Bytes::copy_from_slice(&current.key.0),
                    }),
                )
                .await?;
                current.stage = Stage::Ended;
                break;
            }

            let len = limits.chunk.min(current.size - sent);
            if sent - acked + len > limits.window {
                break;
            }
            let mut data = vec![0; usize::try_from(len).unwrap_or(MAX_CHUNK_BYTES)];
            if let Err(error) = current.file.read_exact(&mut data).await {
                eprintln!(
                    "microscope: reading {} failed: {error}",
                    self.outbox.recording(current.key).display()
                );
                self.hold(current.key);
                *upload = None;
                return Ok(());
            }
            send(
                socket,
                codec,
                agent_message::Body::UploadChunk(UploadChunk {
                    upload_key: Bytes::copy_from_slice(&current.key.0),
                    offset: sent,
                    data: data.into(),
                }),
            )
            .await?;
            current.stage = Stage::Sending {
                acked,
                sent: sent + len,
            };
        }
        Ok(())
    }

    /// Begins uploading the oldest recording in the outbox that may be
    /// tried now, if there is one.
    async fn begin(&self, socket: &mut Socket, codec: &mut Codec) -> Result<Option<Upload>, End> {
        let pending = match self.outbox.pending() {
            Ok(pending) => pending,
            Err(error) => {
                eprintln!("microscope: {error:#}");
                return Ok(None);
            }
        };

        let now = Instant::now();
        for key in pending {
            let held = self
                .held
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&key)
                .is_some_and(|&until| until > now);
            if held {
                continue;
            }

            let path = self.outbox.recording(key);
            let inspected = tokio::task::spawn_blocking(move || inspect(&path)).await;
            let file = match inspected {
                Ok(Ok(file)) => file,
                Ok(Err(Inspected::Invalid(why))) => {
                    let name = self.outbox.name(key);
                    eprintln!("microscope: not uploading {name}: {why}");
                    if let Err(error) = self.outbox.fail(key, &why) {
                        eprintln!("microscope: {error:#}");
                        self.hold(key);
                    }
                    continue;
                }
                Ok(Err(Inspected::Io(error))) => {
                    // It may have been deleted since the listing.
                    eprintln!(
                        "microscope: reading {} failed: {error}",
                        self.outbox.recording(key).display()
                    );
                    self.hold(key);
                    continue;
                }
                Err(error) => return Err(End::Fatal(format!("inspecting a recording: {error}"))),
            };

            let opened = match tokio::fs::File::open(self.outbox.recording(key)).await {
                Ok(opened) => opened,
                Err(error) => {
                    eprintln!(
                        "microscope: reading {} failed: {error}",
                        self.outbox.recording(key).display()
                    );
                    self.hold(key);
                    continue;
                }
            };

            let name = self.outbox.name(key);
            send(
                socket,
                codec,
                agent_message::Body::UploadBegin(UploadBegin {
                    upload_key: Bytes::copy_from_slice(&key.0),
                    kind: RecordingKind::Usprof.into(),
                    size: file.size,
                    sha256: Bytes::copy_from_slice(&file.sha256),
                    capture_revision: file.revision,
                    name: name.clone(),
                }),
            )
            .await?;
            return Ok(Some(Upload {
                key,
                name,
                file: opened,
                size: file.size,
                stage: Stage::Begun,
            }));
        }
        Ok(None)
    }

    fn hold(&self, key: Key) {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, Instant::now() + HOLD);
    }

    async fn handle(
        &self,
        socket: &mut Socket,
        message: ServerMessage,
        upload: &mut Option<Upload>,
        results: &mpsc::Sender<CommandResult>,
    ) -> Result<(), End> {
        let current = |key: &[u8]| {
            upload
                .as_ref()
                .is_some_and(|upload| Key::from_slice(key) == Some(upload.key))
        };

        match message.body {
            Some(server_message::Body::UploadResume(resume)) if current(&resume.upload_key) => {
                let Some(upload) = upload.as_mut() else {
                    return Ok(());
                };
                if upload.stage != Stage::Begun || resume.offset > upload.size {
                    close(socket, CloseCode::Protocol, "unexpected resume").await;
                    return Err(End::Retry(
                        "the service resumed an upload wrongly".to_owned(),
                    ));
                }
                if let Err(error) = upload
                    .file
                    .seek(std::io::SeekFrom::Start(resume.offset))
                    .await
                {
                    eprintln!("microscope: reading {} failed: {error}", upload.name);
                    self.hold(upload.key);
                    return Err(End::Retry("reading a recording failed".to_owned()));
                }
                upload.stage = Stage::Sending {
                    acked: resume.offset,
                    sent: resume.offset,
                };
            }
            Some(server_message::Body::UploadAck(ack)) if current(&ack.upload_key) => {
                if let Some(Upload {
                    stage: Stage::Sending { acked, sent },
                    ..
                }) = upload.as_mut()
                {
                    *acked = ack.offset.min(*sent).max(*acked);
                }
            }
            Some(server_message::Body::UploadDone(done)) if current(&done.upload_key) => {
                if let Some(upload) = upload.take() {
                    eprintln!("microscope: uploaded {}: {}", upload.name, done.url);
                    if let Err(error) = self.outbox.finish(upload.key, &done.url) {
                        eprintln!("microscope: {error:#}");
                    }
                }
            }
            Some(server_message::Body::UploadFailed(failed)) if current(&failed.upload_key) => {
                if let Some(upload) = upload.take() {
                    if failed.retryable {
                        eprintln!(
                            "microscope: the service could not take {} now: {}; trying again in a minute",
                            upload.name, failed.message
                        );
                        self.hold(upload.key);
                    } else {
                        eprintln!(
                            "microscope: the service refused {}: {}",
                            upload.name, failed.message
                        );
                        if let Err(error) = self.outbox.fail(upload.key, &failed.message) {
                            eprintln!("microscope: {error:#}");
                            self.hold(upload.key);
                        }
                    }
                }
            }
            // About an upload that is no longer under way, as after a
            // retry.
            Some(
                server_message::Body::UploadResume(_)
                | server_message::Body::UploadAck(_)
                | server_message::Body::UploadDone(_)
                | server_message::Body::UploadFailed(_)
                | server_message::Body::Welcome(_),
            )
            | None => {}
            Some(server_message::Body::Goodbye(goodbye)) => return Err(farewell(&goodbye)),
            Some(server_message::Body::Command(command)) => {
                self.command(command, results);
            }
        }
        Ok(())
    }

    /// Runs a command on a task of its own, which answers through
    /// `results`. A command still running when the connection ends answers
    /// no one.
    fn command(&self, command: super::proto::Command, results: &mpsc::Sender<CommandResult>) {
        let id = command.id;
        let results = results.clone();
        let error = move |message: String| CommandResult {
            id,
            result: Some(command_result::Result::Error(CommandError { message })),
        };

        if !self.allow_remote {
            let refused = error(
                "this machine runs no commands from the service; set allow-remote in agent.toml"
                    .to_owned(),
            );
            tokio::spawn(async move { results.send(refused).await });
            return;
        }

        match command.kind {
            Some(command::Kind::ListProcesses(_)) => {
                tokio::spawn(async move {
                    let result = match tokio::task::spawn_blocking(commands::processes).await {
                        Ok(processes) => CommandResult {
                            id,
                            result: Some(command_result::Result::Processes(ProcessList {
                                processes,
                            })),
                        },
                        Err(failed) => error(format!("listing processes failed: {failed}")),
                    };
                    results.send(result).await
                });
            }
            Some(command::Kind::Record(record)) => {
                let recorder = Arc::clone(&self.recorder);
                let outbox = self.outbox.clone();
                let uploads = Arc::clone(&self.uploads);
                tokio::spawn(async move {
                    let result = match recorder
                        .start(record.pid, record.seconds, &outbox, &uploads)
                        .await
                    {
                        Ok(key) => {
                            eprintln!(
                                "microscope: recording process {} for {} s, as the service asked",
                                record.pid, record.seconds
                            );
                            CommandResult {
                                id,
                                result: Some(command_result::Result::RecordStarted(
                                    RecordStarted {
                                        upload_key: Bytes::copy_from_slice(&key.0),
                                    },
                                )),
                            }
                        }
                        Err(message) => error(message),
                    };
                    results.send(result).await
                });
            }
            None => {
                let unknown = error("this agent does not know that command; upgrade it".to_owned());
                tokio::spawn(async move { results.send(unknown).await });
            }
        }
    }
}

/// The TLS settings for `wss` URLs: the web's usual roots, with ring.
fn tls() -> Result<Arc<rustls::ClientConfig>, rustls::Error> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Why the service refused the upgrade.
fn refused(status: StatusCode) -> End {
    match status {
        StatusCode::UNAUTHORIZED => End::Fatal(
            "the service refused this machine's device token; make a new one and run `microscope agent login`"
                .to_owned(),
        ),
        StatusCode::TOO_MANY_REQUESTS => {
            End::Retry("this account has as many agents connected as the service allows".to_owned())
        }
        StatusCode::SERVICE_UNAVAILABLE => End::Retry("the service is busy".to_owned()),
        status => End::Retry(format!("the service refused the connection with {status}")),
    }
}

fn farewell(goodbye: &super::proto::Goodbye) -> End {
    let message = if goodbye.message.is_empty() {
        match goodbye.reason() {
            goodbye::Reason::ShuttingDown => "the service is restarting",
            goodbye::Reason::ProtocolUnsupported => {
                "the service no longer supports this agent; upgrade microscope"
            }
            goodbye::Reason::Unauthenticated => "the service revoked this machine's device token",
            goodbye::Reason::ProtocolError | goodbye::Reason::Unspecified => {
                "the service closed the connection"
            }
        }
        .to_owned()
    } else {
        format!("the service said goodbye: {}", goodbye.message)
    };
    if goodbye.retry {
        End::Retry(message)
    } else {
        End::Fatal(message)
    }
}

async fn send(
    socket: &mut Socket,
    codec: &mut Codec,
    body: agent_message::Body,
) -> Result<(), End> {
    let frame = codec
        .encode(&AgentMessage { body: Some(body) })
        .map_err(|error| End::Fatal(format!("encoding a message failed: {error}")))?;
    socket
        .send(Message::Binary(frame.into()))
        .await
        .map_err(|error| End::Retry(format!("the connection failed: {error}")))
}

/// The next message, skipping pings and pongs.
async fn receive(socket: &mut Socket, codec: &mut Codec) -> Result<ServerMessage, End> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Binary(frame))) => {
                return codec.decode(&frame).map_err(|error| {
                    End::Retry(format!("the service sent a bad message: {error}"))
                });
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
            Some(Ok(Message::Text(_))) => {
                return Err(End::Retry("the service sent a text message".to_owned()));
            }
            Some(Ok(Message::Close(_))) | None => {
                return Err(End::Retry("the service closed the connection".to_owned()));
            }
            Some(Err(error)) => return Err(End::Retry(format!("the connection failed: {error}"))),
        }
    }
}

async fn close(socket: &mut Socket, code: CloseCode, reason: &'static str) {
    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, socket.close(Some(frame))).await;
}

/// A recording ready to upload.
#[derive(Debug)]
struct Inspection {
    size: u64,
    sha256: [u8; 32],
    revision: u32,
}

#[derive(Debug)]
enum Inspected {
    /// It is not something the service takes.
    Invalid(String),
    Io(std::io::Error),
}

/// Reads the whole recording at `path` for its digest, and its header for
/// the capture's revision.
fn inspect(path: &Path) -> Result<Inspection, Inspected> {
    let mut file = std::fs::File::open(path).map_err(Inspected::Io)?;
    let mut hasher = Sha256::new();
    let mut header = Vec::with_capacity(12);
    let mut size = 0_u64;
    let mut buffer = vec![0; 1 << 16];
    loop {
        let read = file.read(&mut buffer).map_err(Inspected::Io)?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        if header.len() < 12 {
            let wanted = (12 - header.len()).min(chunk.len());
            header.extend_from_slice(&chunk[..wanted]);
        }
        hasher.update(chunk);
        size += read as u64;
    }

    let revision = header
        .strip_prefix(CAPTURE_MAGIC)
        .and_then(<[u8]>::first_chunk::<4>)
        .map(|revision| u32::from_le_bytes(*revision))
        .ok_or_else(|| Inspected::Invalid("it is not a microscope recording".to_owned()))?;
    Ok(Inspection {
        size,
        sha256: hasher.finalize().into(),
        revision,
    })
}
