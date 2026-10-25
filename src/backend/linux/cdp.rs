//! The CDP client that drives Node's inspector over its WebSocket, with
//! workers reached through `NodeWorker` tunnels.
//!
//! One thread owns the socket: it connects, completes the handshake, writes
//! queued requests, and reads replies and events into the caller's sink. The
//! caller never touches the network, so it never blocks on the target.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

use rustix::event::{EventfdFlags, PollFd, PollFlags};
use rustix::io::Errno;
use rustix::net::{AddressFamily, SocketFlags, SocketType};
use serde_json::{Value, json};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{HandshakeError, Message, WebSocket};

/// The largest message, and frame, the transport accepts from the target.
pub(super) const MAX_MESSAGE: usize = 64 << 20;

/// The most workers nested inside one another that a session may name.
pub(super) const MAX_SESSION_DEPTH: usize = 8;

/// Requests waiting for the transport's thread to write them.
const QUEUE_LENGTH: usize = 256;

/// Requests and worker envelopes queued or waiting for replies.
const MAX_OUTSTANDING: usize = 4096;

const SEND_TO_WORKER: &str = "NodeWorker.sendMessageToWorker";
const FROM_WORKER: &str = "NodeWorker.receivedMessageFromWorker";

/// The isolate a message goes to or comes from, as the `NodeWorker`
/// session ids from the main isolate down. The empty path is the main isolate.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct Session(Arc<[Arc<str>]>);

impl Session {
    pub(super) fn main() -> Self {
        Self(Arc::new([]))
    }

    /// The worker that `session_id` names inside this session's isolate.
    pub(super) fn worker(&self, session_id: &str) -> Self {
        Self(
            self.0
                .iter()
                .cloned()
                .chain(std::iter::once(Arc::from(session_id)))
                .collect(),
        )
    }

    pub(super) fn path(&self) -> &[Arc<str>] {
        &self.0
    }
}

/// A request's id, unique across its connection and never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct RequestId(u64);

impl RequestId {
    pub(super) const fn number(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(super) enum CdpError {
    #[error("the inspector answered error {code}: {message}")]
    Protocol { code: i64, message: Arc<str> },
    #[error("the inspector connection closed: {0}")]
    Closed(Arc<str>),
    #[error("the inspector sent a malformed message: {0}")]
    Malformed(Arc<str>),
    #[error("{QUEUE_LENGTH} requests are already waiting to be written")]
    QueueFull,
    #[error(
        "the inspector has reached its limit of {MAX_OUTSTANDING} outstanding requests and worker envelopes"
    )]
    OutstandingFull,
    #[error("the session nests more than {MAX_SESSION_DEPTH} workers")]
    TooDeep,
}

/// What the transport delivers to its sink.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Incoming {
    /// The handshake completed.
    Connected,
    Reply {
        id: RequestId,
        session: Session,
        result: Result<Value, CdpError>,
    },
    Event {
        session: Session,
        method: Arc<str>,
        params: Value,
    },
    /// A message the transport could not take as a reply or an event.
    Malformed { session: Session, reason: Arc<str> },
    /// The last message, after a reply to every outstanding request.
    Closed { reason: Arc<str> },
}

/// A connection to one inspector. Dropping it closes the connection.
pub(super) struct Connection {
    shared: Arc<Shared>,
    queue: SyncSender<Outgoing>,
    #[cfg(test)]
    thread: Option<thread::JoinHandle<()>>,
}

struct Shared {
    /// Why the connection ended, once it has. A request is queued only while
    /// this is `None`, so none is queued after the last replies.
    ended: Mutex<Option<Arc<str>>>,
    closing: AtomicBool,
    next_id: AtomicU64,
    /// Includes queued requests and every unanswered worker envelope.
    outstanding: AtomicUsize,
    /// An eventfd that wakes the transport's thread from `poll`.
    wake: OwnedFd,
}

/// One request, wrapped in a `sendMessageToWorker` for each worker hop.
struct Outgoing {
    id: u64,
    session: Session,
    /// The id of each wrapper and the session it was sent on.
    hops: Vec<(u64, Session)>,
    text: String,
}

impl Connection {
    /// Starts connecting at once and returns without waiting: the handshake
    /// runs on the transport's thread, because the target serves it only while
    /// the caller keeps running. The sink runs on that thread; when it returns
    /// false the transport stops.
    pub(super) fn open(
        address: SocketAddr,
        path: &str,
        sink: impl FnMut(Incoming) -> bool + Send + 'static,
    ) -> io::Result<Self> {
        let wake = rustix::event::eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?;
        let shared = Arc::new(Shared {
            ended: Mutex::new(None),
            closing: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            outstanding: AtomicUsize::new(0),
            wake,
        });

        let (queue, outgoing) = mpsc::sync_channel(QUEUE_LENGTH);
        let mut transport = Transport {
            sink: Box::new(sink),
            pending: BTreeMap::new(),
            stopped: false,
        };
        let path = path.to_owned();
        let thread_shared = Arc::clone(&shared);

        let thread = thread::Builder::new().name("cdp".into()).spawn(move || {
            let reason = transport.serve(address, &path, &thread_shared, &outgoing);
            transport.finish(reason, &thread_shared, &outgoing);
        })?;

        #[cfg(not(test))]
        drop(thread);

        Ok(Self {
            shared,
            queue,
            #[cfg(test)]
            thread: Some(thread),
        })
    }

    /// Queues a request; requests made before `Connected` are sent after the
    /// handshake, in order. Fails without blocking when the queue or the
    /// outstanding-request limit is full, or the connection has closed.
    pub(super) fn send(
        &self,
        session: &Session,
        method: &str,
        params: Value,
    ) -> Result<RequestId, CdpError> {
        if session.0.len() > MAX_SESSION_DEPTH {
            return Err(CdpError::TooDeep);
        }

        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let mut request = serde_json::Map::new();
        request.insert("id".into(), id.into());
        request.insert("method".into(), method.into());
        request.insert("params".into(), params);
        let mut text = Value::Object(request).to_string();

        // Wrap from the innermost worker out, each hop on its parent session.
        let mut hops = Vec::with_capacity(session.0.len());
        for (depth, session_id) in session.0.iter().enumerate().rev() {
            let hop = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
            let params = json!({"sessionId": &**session_id, "message": text});
            text = json!({"id": hop, "method": SEND_TO_WORKER, "params": params}).to_string();
            hops.push((hop, Session(session.0[..depth].into())));
        }

        let outgoing = Outgoing {
            id,
            session: session.clone(),
            hops,
            text,
        };

        let ended = lock(&self.shared.ended);
        if let Some(reason) = ended.as_ref() {
            return Err(CdpError::Closed(Arc::clone(reason)));
        }
        let slots = session.0.len() + 1;
        self.shared
            .outstanding
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                (used + slots <= MAX_OUTSTANDING).then_some(used + slots)
            })
            .map_err(|_| CdpError::OutstandingFull)?;
        let queued = self.queue.try_send(outgoing);
        if queued.is_err() {
            self.shared.outstanding.fetch_sub(slots, Ordering::Relaxed);
        }
        drop(ended);

        match queued {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(CdpError::QueueFull),
            Err(TrySendError::Disconnected(_)) => {
                return Err(CdpError::Closed("the transport thread ended".into()));
            }
        }

        self.shared.wake();
        Ok(RequestId(id))
    }

    /// Ends the connection without waiting. The transport's thread then
    /// answers every outstanding request with `Closed` and delivers `Closed`.
    pub(super) fn close(&self) {
        self.shared.closing.store(true, Ordering::Release);
        self.shared.wake();
    }

    /// Closes the connection and waits for the transport's thread to end.
    #[cfg(test)]
    fn join(&mut self) {
        self.close();
        if let Some(thread) = self.thread.take() {
            assert!(thread.join().is_ok(), "the transport thread panicked");
        }
    }
}

impl Drop for Connection {
    // Waiting here could deadlock a caller whose sink is blocked on it.
    fn drop(&mut self) {
        self.close();
    }
}

impl Shared {
    fn wake(&self) {
        // Only an overflowing counter fails, and the thread drains it in each poll.
        let _ = rustix::io::write(&self.wake, &1_u64.to_ne_bytes());
    }

    fn closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The socket handed to tungstenite. It re-arms TCP quick acknowledgement
/// after every read, because Linux falls back to delayed ACKs, which cost
/// 41 ms per round trip against the inspector (plans/node.html F10).
struct QuickAck(TcpStream);

impl Read for QuickAck {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.0.read(buffer)?;
        if read > 0 {
            rustix::net::sockopt::set_tcp_quickack(&self.0, true)?;
        }
        Ok(read)
    }
}

impl Write for QuickAck {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// A request or wrapper awaiting its reply.
struct Pending {
    session: Session,
    /// The final worker, including for an envelope sent on its parent.
    target: Session,
    /// The caller's request this entry belongs to; its own id unless a hop.
    owner: u64,
}

struct Transport {
    sink: Box<dyn FnMut(Incoming) -> bool + Send>,
    pending: BTreeMap<u64, Pending>,
    stopped: bool,
}

impl Transport {
    /// Runs the connection until it ends and returns why.
    fn serve(
        &mut self,
        address: SocketAddr,
        path: &str,
        shared: &Shared,
        outgoing: &Receiver<Outgoing>,
    ) -> Arc<str> {
        let stream = match connect(address, shared) {
            Ok(Some(stream)) => stream,
            Ok(None) => return closed_by_caller(),
            Err(error) => return format!("cannot connect to {address}: {error}").into(),
        };
        let mut socket = match handshake(stream, address, path, shared) {
            Ok(Some(socket)) => socket,
            Ok(None) => return closed_by_caller(),
            Err(reason) => return reason,
        };

        if !self.deliver(Incoming::Connected) {
            return receiver_stopped();
        }

        loop {
            if shared.closing() {
                return closed_by_caller();
            }

            let blocked = match self.write_requests(&mut socket, outgoing) {
                Ok(waiting) => waiting,
                Err(error) => return format!("cannot write a request: {error}").into(),
            };

            let flags = if blocked {
                PollFlags::IN | PollFlags::OUT
            } else {
                PollFlags::IN
            };
            match wait(socket.get_ref().0.as_fd(), flags, shared) {
                Ok(false) => continue,
                Ok(true) => {}
                Err(error) => return format!("cannot poll the inspector socket: {error}").into(),
            }

            loop {
                match socket.read() {
                    Ok(Message::Text(text)) => {
                        if !self.receive(text.as_str(), shared) {
                            return receiver_stopped();
                        }
                    }
                    Ok(Message::Binary(_)) => {
                        let malformed = Incoming::Malformed {
                            session: Session::main(),
                            reason: "a binary frame".into(),
                        };
                        if !self.deliver(malformed) {
                            return receiver_stopped();
                        }
                    }
                    Ok(Message::Close(frame)) => {
                        return match frame {
                            Some(frame) => format!(
                                "the inspector closed the connection: {} {}",
                                frame.code, frame.reason
                            )
                            .into(),
                            None => "the inspector closed the connection".into(),
                        };
                    }
                    // tungstenite answers pings itself.
                    Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
                    Err(tungstenite::Error::Io(error))
                        if error.kind() == io::ErrorKind::WouldBlock =>
                    {
                        break;
                    }
                    Err(
                        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed,
                    ) => {
                        return "the inspector closed the connection".into();
                    }
                    Err(error) => {
                        return format!("the inspector connection failed: {error}").into();
                    }
                }
            }
        }
    }

    fn write_requests(
        &mut self,
        socket: &mut WebSocket<impl Read + Write>,
        outgoing: &Receiver<Outgoing>,
    ) -> tungstenite::Result<bool> {
        // Take new requests only once earlier ones are written, so the
        // bounded queue holds back a target that stops reading.
        match socket.flush() {
            Ok(()) => {}
            Err(tungstenite::Error::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {
                return Ok(true);
            }
            Err(error) => return Err(error),
        }
        while let Ok(request) = outgoing.try_recv() {
            self.register(&request);
            match socket.write(Message::text(request.text)) {
                Ok(()) => {}
                Err(tungstenite::Error::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {
                    return Ok(true);
                }
                Err(error) => return Err(error),
            }
        }
        match socket.flush() {
            Ok(()) => Ok(false),
            Err(tungstenite::Error::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {
                Ok(true)
            }
            Err(error) => Err(error),
        }
    }

    fn register(&mut self, request: &Outgoing) {
        let owner = request.id;
        self.pending.insert(
            owner,
            Pending {
                session: request.session.clone(),
                target: request.session.clone(),
                owner,
            },
        );
        for (hop, session) in &request.hops {
            self.pending.insert(
                *hop,
                Pending {
                    session: session.clone(),
                    target: request.session.clone(),
                    owner,
                },
            );
        }
    }

    /// Delivers what one received text holds. Returns false once the sink stops.
    fn receive(&mut self, text: &str, shared: &Shared) -> bool {
        let incoming = match decode(text) {
            Decoded::Event {
                session,
                method,
                params,
            } => {
                let detached = (method.as_ref() == "NodeWorker.detachedFromWorker")
                    .then(|| params["sessionId"].as_str().map(|id| session.worker(id)))
                    .flatten();
                self.deliver(Incoming::Event {
                    session,
                    method,
                    params,
                });
                if let Some(worker) = detached {
                    self.detach_worker(&worker, shared);
                }
                return !self.stopped;
            }
            Decoded::Malformed { session, reason } => Incoming::Malformed { session, reason },
            Decoded::Reply {
                session,
                id,
                result,
            } => match self.reply(session, id, result, shared) {
                Some(incoming) => incoming,
                None => return !self.stopped,
            },
        };
        self.deliver(incoming)
    }

    fn detach_worker(&mut self, worker: &Session, shared: &Shared) {
        let mut requests = Vec::new();
        let before = self.pending.len();
        self.pending.retain(|&id, pending| {
            if !pending.target.path().starts_with(worker.path()) {
                return true;
            }
            if pending.owner == id {
                requests.push((id, pending.session.clone()));
            }
            false
        });
        shared
            .outstanding
            .fetch_sub(before - self.pending.len(), Ordering::Relaxed);
        for (id, session) in requests {
            self.deliver(Incoming::Reply {
                id: RequestId(id),
                session,
                result: Err(CdpError::Closed("the worker session detached".into())),
            });
        }
    }

    /// Matches a reply to its request by session and id. A wrapper's reply
    /// yields nothing unless it failed, which fails the request it carried.
    fn reply(
        &mut self,
        session: Session,
        id: u64,
        result: Result<Value, CdpError>,
        shared: &Shared,
    ) -> Option<Incoming> {
        let owner = match self.pending.get(&id) {
            Some(pending) if pending.session == session => pending.owner,
            _ => {
                return Some(Incoming::Malformed {
                    reason: format!("a reply to no outstanding request: id {id}").into(),
                    session,
                });
            }
        };

        self.pending.remove(&id);
        shared.outstanding.fetch_sub(1, Ordering::Relaxed);
        if owner == id {
            return Some(Incoming::Reply {
                id: RequestId(id),
                session,
                result,
            });
        }

        let Err(error) = result else {
            return None;
        };
        let before = self.pending.len();
        let request = self.pending.remove(&owner);
        self.pending.retain(|_, pending| pending.owner != owner);
        shared
            .outstanding
            .fetch_sub(before - self.pending.len(), Ordering::Relaxed);
        let request = request?;
        Some(Incoming::Reply {
            id: RequestId(owner),
            session: request.session,
            result: Err(error),
        })
    }

    /// Answers every request still outstanding with `Closed`, then delivers
    /// `Closed` itself.
    fn finish(&mut self, reason: Arc<str>, shared: &Shared, outgoing: &Receiver<Outgoing>) {
        // Once this is set no request can be queued, so the drain sees them all.
        lock(&shared.ended).get_or_insert_with(|| Arc::clone(&reason));
        while let Ok(request) = outgoing.try_recv() {
            self.register(&request);
        }

        let pending = std::mem::take(&mut self.pending);
        shared
            .outstanding
            .fetch_sub(pending.len(), Ordering::Relaxed);
        for (id, entry) in pending {
            if entry.owner != id {
                continue;
            }
            let reply = Incoming::Reply {
                id: RequestId(id),
                session: entry.session,
                result: Err(CdpError::Closed(Arc::clone(&reason))),
            };
            if !self.deliver(reply) {
                return;
            }
        }
        self.deliver(Incoming::Closed { reason });
    }

    fn deliver(&mut self, incoming: Incoming) -> bool {
        if !self.stopped && !(self.sink)(incoming) {
            self.stopped = true;
        }
        !self.stopped
    }
}

fn closed_by_caller() -> Arc<str> {
    "closed by microscope".into()
}

fn receiver_stopped() -> Arc<str> {
    "the receiver stopped".into()
}

/// Connects without blocking past a close. Returns `None` once closing.
fn connect(address: SocketAddr, shared: &Shared) -> io::Result<Option<TcpStream>> {
    let family = if address.is_ipv4() {
        AddressFamily::INET
    } else {
        AddressFamily::INET6
    };
    let socket = rustix::net::socket_with(
        family,
        SocketType::STREAM,
        SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
        None,
    )?;
    match rustix::net::connect(&socket, &address) {
        Ok(()) | Err(Errno::INPROGRESS) => {}
        Err(error) => return Err(error.into()),
    }

    while !wait(socket.as_fd(), PollFlags::OUT, shared)? {
        if shared.closing() {
            return Ok(None);
        }
    }
    rustix::net::sockopt::socket_error(&socket)??;

    let stream = TcpStream::from(socket);
    stream.set_nodelay(true)?;
    Ok(Some(stream))
}

/// Completes the WebSocket handshake. Returns `None` once closing.
fn handshake(
    stream: TcpStream,
    address: SocketAddr,
    path: &str,
    shared: &Shared,
) -> Result<Option<WebSocket<QuickAck>>, Arc<str>> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let url = format!("ws://{address}{path}");
    let mut attempt = tungstenite::client::client_with_config(url, QuickAck(stream), Some(config));

    loop {
        match attempt {
            Ok((socket, _)) => return Ok(Some(socket)),
            Err(HandshakeError::Failure(error)) => {
                return Err(format!("the WebSocket handshake failed: {error}").into());
            }
            Err(HandshakeError::Interrupted(middle)) => {
                // The request fits a new socket's empty send buffer, so only
                // the response can make the handshake wait.
                loop {
                    match wait(middle.get_ref().get_ref().0.as_fd(), PollFlags::IN, shared) {
                        Ok(true) => break,
                        Ok(false) if shared.closing() => return Ok(None),
                        Ok(false) => {}
                        Err(error) => {
                            return Err(format!("cannot poll the inspector socket: {error}").into());
                        }
                    }
                }
                attempt = middle.handshake();
            }
        }
    }
}

/// Waits until `socket` is ready for `flags` or the caller wakes the thread.
/// Returns whether the socket is ready; never ready once closing.
fn wait(socket: BorrowedFd<'_>, flags: PollFlags, shared: &Shared) -> io::Result<bool> {
    // An earlier wait may have drained the wake that `close` sent.
    if shared.closing() {
        return Ok(false);
    }
    let mut fds = [
        PollFd::from_borrowed_fd(socket, flags),
        PollFd::new(&shared.wake, PollFlags::IN),
    ];
    loop {
        match rustix::event::poll(&mut fds, None) {
            Ok(_) => break,
            Err(Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }

    if !fds[1].revents().is_empty() {
        let mut count = [0; 8];
        // The eventfd is non-blocking; a racing drain leaves nothing to read.
        let _ = rustix::io::read(&shared.wake, &mut count);
    }
    Ok(!fds[0].revents().is_empty())
}

/// One received message, with any `NodeWorker` tunnels taken off.
#[derive(Debug)]
enum Decoded {
    Reply {
        session: Session,
        id: u64,
        result: Result<Value, CdpError>,
    },
    Event {
        session: Session,
        method: Arc<str>,
        params: Value,
    },
    Malformed {
        session: Session,
        reason: Arc<str>,
    },
}

fn decode(text: &str) -> Decoded {
    decode_in(text, Session::main())
}

fn decode_in(text: &str, session: Session) -> Decoded {
    let malformed = |session, reason: &str| Decoded::Malformed {
        session,
        reason: reason.into(),
    };

    let Ok(Value::Object(mut message)) = serde_json::from_str::<Value>(text) else {
        return malformed(session, "a message that is not a JSON object");
    };

    if let Some(id) = message.get("id") {
        let Some(id) = id.as_u64() else {
            return malformed(session, "a reply whose id is not an unsigned integer");
        };
        let result = match (message.remove("error"), message.remove("result")) {
            (Some(error), _) => match (
                error.get("code").and_then(Value::as_i64),
                error.get("message").and_then(Value::as_str),
            ) {
                (Some(code), Some(text)) => Err(CdpError::Protocol {
                    code,
                    message: text.into(),
                }),
                _ => Err(CdpError::Malformed(
                    "an error reply without an integer code and a message".into(),
                )),
            },
            (None, Some(result)) => Ok(result),
            (None, None) => Err(CdpError::Malformed(
                "a reply with neither a result nor an error".into(),
            )),
        };
        return Decoded::Reply {
            session,
            id,
            result,
        };
    }

    let Some(Value::String(method)) = message.remove("method") else {
        return malformed(session, "a message with neither an id nor a method");
    };
    let params = message.remove("params").unwrap_or(Value::Null);

    if method != FROM_WORKER {
        return Decoded::Event {
            session,
            method: method.into(),
            params,
        };
    }

    let (Some(worker), Some(inner)) = (
        params.get("sessionId").and_then(Value::as_str),
        params.get("message").and_then(Value::as_str),
    ) else {
        return malformed(
            session,
            "a worker message without a sessionId and a message",
        );
    };
    if session.0.len() >= MAX_SESSION_DEPTH {
        return malformed(session, "a worker message nested too deeply");
    }
    let worker = session.worker(worker);
    decode_in(inner, worker)
}

/// Decodes arbitrary bytes as one received message, for the fuzz harness.
#[cfg(feature = "fuzzing")]
pub(super) fn fuzz(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let session = match decode(text) {
        Decoded::Reply { session, .. }
        | Decoded::Event { session, .. }
        | Decoded::Malformed { session, .. } => session,
    };
    assert!(session.path().len() <= MAX_SESSION_DEPTH);
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::{Duration, Instant};

    use super::*;

    const DEADLINE: Duration = Duration::from_secs(10);

    type Server = WebSocket<TcpStream>;

    /// A fake inspector that serves one connection with `script`.
    fn serve(
        script: impl FnOnce(TcpStream) + Send + 'static,
    ) -> (SocketAddr, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("local address");
        let thread = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            script(stream);
        });
        (address, thread)
    }

    fn accept(stream: TcpStream) -> Server {
        tungstenite::accept(stream).expect("server handshake")
    }

    /// The next request the client sent, as JSON.
    fn request(server: &mut Server) -> Value {
        loop {
            if let Message::Text(text) = server.read().expect("a request") {
                return serde_json::from_str(text.as_str()).expect("a JSON request");
            }
        }
    }

    fn reply(server: &mut Server, message: &Value) {
        server
            .send(Message::text(message.to_string()))
            .expect("send");
    }

    /// Wraps `message` as the parent session receives it from worker `session`.
    fn tunnel(session: &str, message: &Value) -> Value {
        json!({"method": FROM_WORKER, "params": {"sessionId": session, "message": message.to_string()}})
    }

    /// The message inside a `sendMessageToWorker` to `session`.
    fn untunnel(request: &Value, session: &str) -> Value {
        assert_eq!(request["method"], SEND_TO_WORKER);
        assert_eq!(request["params"]["sessionId"], session);
        let inner = request["params"]["message"]
            .as_str()
            .expect("a tunneled message");
        serde_json::from_str(inner).expect("a JSON tunneled message")
    }

    /// Reads until the client goes away, so no write races a reset.
    fn linger(mut server: Server) {
        while server.read().is_ok() {}
    }

    struct Client {
        connection: Connection,
        incoming: Receiver<Incoming>,
    }

    impl Client {
        fn open(address: SocketAddr) -> Self {
            let (sender, incoming) = mpsc::channel();
            let connection = Connection::open(address, "/0f2b5c1e-uuid", move |message| {
                sender.send(message).is_ok()
            })
            .expect("open");
            Self {
                connection,
                incoming,
            }
        }

        fn next(&self) -> Incoming {
            self.incoming
                .recv_timeout(DEADLINE)
                .expect("an incoming message")
        }

        fn send(&self, session: &Session, method: &str, params: Value) -> RequestId {
            self.connection.send(session, method, params).expect("send")
        }

        /// Closes the connection and checks the transport delivered nothing more.
        fn finish(mut self) {
            self.connection.join();
            assert_eq!(
                self.connection.shared.outstanding.load(Ordering::Relaxed),
                0
            );
            loop {
                match self.incoming.recv_timeout(DEADLINE) {
                    Ok(Incoming::Closed { .. } | Incoming::Reply { .. }) => {}
                    Ok(other) => panic!("{other:?} after close"),
                    Err(RecvTimeoutError::Disconnected) => return,
                    Err(RecvTimeoutError::Timeout) => panic!("the sink outlived the thread"),
                }
            }
        }
    }

    fn reply_to(client: &Client) -> (RequestId, Session, Result<Value, CdpError>) {
        match client.next() {
            Incoming::Reply {
                id,
                session,
                result,
            } => (id, session, result),
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    #[test]
    fn request_and_error_round_trip() {
        let (address, server) = serve(|stream| {
            let mut server = accept(stream);
            let first = request(&mut server);
            assert_eq!(first["method"], "Runtime.evaluate");
            assert_eq!(first["params"], json!({"expression": "1 + 1"}));
            reply(
                &mut server,
                &json!({"id": first["id"], "result": {"value": 2}}),
            );

            let second = request(&mut server);
            assert_eq!(second["method"], "Foo.bar");
            let error = json!({"code": -32601, "message": "'Foo.bar' wasn't found"});
            reply(&mut server, &json!({"id": second["id"], "error": error}));
            reply(
                &mut server,
                &json!({"method": "Debugger.resumed", "params": {}}),
            );
            linger(server);
        });

        let client = Client::open(address);
        let first = client.send(
            &Session::main(),
            "Runtime.evaluate",
            json!({"expression": "1 + 1"}),
        );
        assert_eq!(client.next(), Incoming::Connected);
        assert_eq!(
            reply_to(&client),
            (first, Session::main(), Ok(json!({"value": 2})))
        );

        let second = client.send(&Session::main(), "Foo.bar", json!({}));
        let error = CdpError::Protocol {
            code: -32601,
            message: "'Foo.bar' wasn't found".into(),
        };
        assert_eq!(reply_to(&client), (second, Session::main(), Err(error)));
        assert_eq!(
            client.next(),
            Incoming::Event {
                session: Session::main(),
                method: "Debugger.resumed".into(),
                params: json!({}),
            }
        );

        client.finish();
        server.join().expect("server");
    }

    #[test]
    fn requests_before_the_handshake_are_sent_after_it_in_order() {
        let (address, server) = serve(|stream| {
            thread::sleep(Duration::from_millis(300));
            let mut server = accept(stream);
            let mut last = 0;
            for method in ["A.one", "A.two", "A.three"] {
                let request = request(&mut server);
                assert_eq!(request["method"], method);
                let id = request["id"].as_u64().expect("an id");
                assert!(id > last, "ids out of order");
                last = id;
                reply(
                    &mut server,
                    &json!({"id": id, "result": {"method": method}}),
                );
            }
            linger(server);
        });

        let started = Instant::now();
        let client = Client::open(address);
        let ids = ["A.one", "A.two", "A.three"]
            .map(|method| client.send(&Session::main(), method, json!({})));
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "open or send waited for the handshake"
        );

        assert_eq!(client.next(), Incoming::Connected);
        for (id, method) in ids.into_iter().zip(["A.one", "A.two", "A.three"]) {
            assert_eq!(
                reply_to(&client),
                (id, Session::main(), Ok(json!({"method": method})))
            );
        }

        client.finish();
        server.join().expect("server");
    }

    #[test]
    fn nested_worker_requests_are_wrapped_and_unwrapped() {
        let (address, server) = serve(|stream| {
            let mut server = accept(stream);
            let outer = request(&mut server);
            let middle = untunnel(&outer, "outer");
            let inner = untunnel(&middle, "inner");
            assert_eq!(inner["method"], "Runtime.evaluate");
            assert_eq!(inner["params"], json!({"expression": "name"}));

            let ids =
                [&outer, &middle, &inner].map(|message| message["id"].as_u64().expect("an id"));
            assert!(ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2]);

            // The inner id, answered on the wrong session, is no reply.
            reply(
                &mut server,
                &json!({"id": inner["id"], "result": {"wrong": true}}),
            );
            reply(&mut server, &json!({"id": outer["id"], "result": {}}));
            reply(
                &mut server,
                &tunnel("outer", &json!({"id": middle["id"], "result": {}})),
            );
            let answer = json!({"id": inner["id"], "result": {"value": "worker"}});
            reply(&mut server, &tunnel("outer", &tunnel("inner", &answer)));
            let paused = json!({"method": "Debugger.paused", "params": {"reason": "other"}});
            reply(&mut server, &tunnel("outer", &tunnel("inner", &paused)));
            linger(server);
        });

        let client = Client::open(address);
        let worker = Session::main().worker("outer").worker("inner");
        let id = client.send(&worker, "Runtime.evaluate", json!({"expression": "name"}));

        assert_eq!(client.next(), Incoming::Connected);
        assert!(matches!(
            client.next(),
            Incoming::Malformed { session, .. } if session == Session::main()
        ));
        assert_eq!(
            reply_to(&client),
            (id, worker.clone(), Ok(json!({"value": "worker"})))
        );
        assert_eq!(
            client.next(),
            Incoming::Event {
                session: worker,
                method: "Debugger.paused".into(),
                params: json!({"reason": "other"}),
            }
        );

        client.finish();
        server.join().expect("server");
    }

    #[test]
    fn a_failed_tunnel_fails_the_request_inside_it() {
        let (address, server) = serve(|stream| {
            let mut server = accept(stream);
            let outer = request(&mut server);
            untunnel(&outer, "gone");
            let error = json!({"code": -32602, "message": "No session with given id"});
            reply(&mut server, &json!({"id": outer["id"], "error": error}));
            linger(server);
        });

        let client = Client::open(address);
        let worker = Session::main().worker("gone");
        let id = client.send(&worker, "Runtime.evaluate", json!({}));

        assert_eq!(client.next(), Incoming::Connected);
        let error = CdpError::Protocol {
            code: -32602,
            message: "No session with given id".into(),
        };
        assert_eq!(reply_to(&client), (id, worker, Err(error)));
        assert_eq!(
            client.connection.shared.outstanding.load(Ordering::Relaxed),
            0
        );

        client.finish();
        server.join().expect("server");
    }

    #[test]
    fn a_disconnect_answers_every_outstanding_request() {
        let (address, server) = serve(|stream| {
            let mut server = accept(stream);
            for _ in 0..3 {
                request(&mut server);
            }
            drop(server);
        });

        let client = Client::open(address);
        let ids = ["A.one", "A.two", "A.three"]
            .map(|method| client.send(&Session::main(), method, json!({})));
        assert_eq!(client.next(), Incoming::Connected);
        for id in ids {
            let (replied, session, result) = reply_to(&client);
            assert_eq!((replied, session), (id, Session::main()));
            assert!(matches!(result, Err(CdpError::Closed(_))), "{result:?}");
        }
        assert!(matches!(client.next(), Incoming::Closed { .. }));
        assert_eq!(
            client.incoming.recv_timeout(DEADLINE),
            Err(RecvTimeoutError::Disconnected)
        );
        assert!(matches!(
            client
                .connection
                .send(&Session::main(), "A.four", json!({})),
            Err(CdpError::Closed(_))
        ));

        client.finish();
        server.join().expect("server");
    }

    #[test]
    fn a_detached_worker_answers_its_requests_and_releases_nested_envelopes() {
        let (address, server) = serve(|stream| {
            let mut server = accept(stream);
            let outer = request(&mut server);
            let nested = untunnel(&outer, "gone");
            let _ = untunnel(&nested, "inner");
            let answered = request(&mut server);
            let inner = untunnel(&answered, "gone");
            // The request can complete before its envelope is acknowledged.
            reply(
                &mut server,
                &tunnel(
                    "gone",
                    &json!({"id": inner["id"], "result": {"done": true}}),
                ),
            );
            reply(
                &mut server,
                &json!({"method": "NodeWorker.detachedFromWorker", "params": {"sessionId": "gone"}}),
            );
            let live = request(&mut server);
            reply(
                &mut server,
                &json!({"id": live["id"], "result": {"live": true}}),
            );
            linger(server);
        });
        let client = Client::open(address);
        assert_eq!(client.next(), Incoming::Connected);
        let gone = Session::main().worker("gone");
        let nested = gone.worker("inner");
        let pending = client.send(&nested, "Runtime.evaluate", json!({}));
        let completed = client.send(&gone, "Runtime.evaluate", json!({}));
        assert_eq!(
            reply_to(&client),
            (completed, gone, Ok(json!({"done": true})))
        );
        assert!(
            matches!(client.next(), Incoming::Event { method, .. } if &*method == "NodeWorker.detachedFromWorker")
        );
        let (id, session, result) = reply_to(&client);
        assert_eq!((id, session), (pending, nested));
        assert!(matches!(result, Err(CdpError::Closed(_))), "{result:?}");
        assert_eq!(
            client.connection.shared.outstanding.load(Ordering::Relaxed),
            0
        );
        let live = client.send(&Session::main(), "Runtime.evaluate", json!({}));
        assert_eq!(
            reply_to(&client),
            (live, Session::main(), Ok(json!({"live": true})))
        );
        client.finish();
        server.join().expect("server");
    }

    #[test]
    fn malformed_input_leaves_the_connection_working() {
        let (address, server) = serve(|stream| {
            let mut server = accept(stream);
            let request = request(&mut server);
            server.send(Message::text("not json")).expect("send");
            server.send(Message::binary(vec![1, 2, 3])).expect("send");
            reply(&mut server, &json!({"id": 999_999, "result": {}}));
            reply(&mut server, &json!({"neither": "id nor method"}));
            reply(&mut server, &tunnel("worker", &json!({"id": 1})));
            reply(
                &mut server,
                &json!({"id": request["id"], "result": {"value": 1}}),
            );
            linger(server);
        });

        let client = Client::open(address);
        let id = client.send(&Session::main(), "Runtime.evaluate", json!({}));
        assert_eq!(client.next(), Incoming::Connected);
        for _ in 0..5 {
            let next = client.next();
            assert!(matches!(next, Incoming::Malformed { .. }), "{next:?}");
        }
        assert_eq!(
            reply_to(&client),
            (id, Session::main(), Ok(json!({"value": 1})))
        );

        client.finish();
        server.join().expect("server");
    }

    #[test]
    fn a_round_trip_takes_under_a_millisecond() {
        const ROUND_TRIPS: usize = 200;

        let (address, server) = serve(|stream| {
            let mut server = accept(stream);
            for _ in 0..ROUND_TRIPS {
                let request = request(&mut server);
                let payload = json!({"id": request["id"], "result": {}}).to_string();
                let length = u8::try_from(payload.len()).expect("a short reply");
                assert!(length < 126);

                // Two writes with Nagle on, so the payload waits for the header's
                // ACK. With TCP_NODELAY here, no delayed ACK would ever show.
                let stream = server.get_mut();
                stream.write_all(&[0x81, length]).expect("write");
                stream.write_all(payload.as_bytes()).expect("write");
            }
            linger(server);
        });

        let client = Client::open(address);
        assert_eq!(client.next(), Incoming::Connected);

        let mut times = Vec::with_capacity(ROUND_TRIPS);
        for _ in 0..ROUND_TRIPS {
            let started = Instant::now();
            let id = client.send(&Session::main(), "Runtime.evaluate", json!({}));
            assert_eq!(reply_to(&client), (id, Session::main(), Ok(json!({}))));
            times.push(started.elapsed());
        }
        times.sort();
        let median = times[ROUND_TRIPS / 2];
        assert!(
            median < Duration::from_millis(1),
            "median round trip {median:?}"
        );

        client.finish();
        server.join().expect("server");
    }

    #[test]
    fn close_ends_the_thread_and_answers_outstanding_requests() {
        let (address, server) = serve(|stream| linger(accept(stream)));

        let mut client = Client::open(address);
        assert_eq!(client.next(), Incoming::Connected);
        let id = client.send(&Session::main(), "Runtime.evaluate", json!({}));
        client.connection.close();
        client.connection.close();
        client.connection.join();

        let (replied, _, result) = reply_to(&client);
        assert_eq!(replied, id);
        assert!(matches!(result, Err(CdpError::Closed(_))), "{result:?}");
        assert!(matches!(client.next(), Incoming::Closed { .. }));
        assert_eq!(
            client.incoming.recv_timeout(DEADLINE),
            Err(RecvTimeoutError::Disconnected)
        );

        // Nextest runs each test in its own process, so no other test owns a
        // thread. A joined thread leaves the task list only once the kernel
        // reaps it, a moment after its join returns.
        let running = || {
            std::fs::read_dir("/proc/self/task")
                .expect("tasks")
                .filter_map(|task| std::fs::read_to_string(task.ok()?.path().join("comm")).ok())
                .any(|comm| comm.trim() == "cdp")
        };
        let started = Instant::now();
        while running() {
            assert!(
                started.elapsed() < DEADLINE,
                "a transport thread is still running"
            );
            std::thread::sleep(Duration::from_millis(1));
        }

        client.finish();
        server.join().expect("server");
    }

    #[test]
    fn dropping_the_connection_closes_it() {
        let (address, server) = serve(|stream| linger(accept(stream)));

        let client = Client::open(address);
        assert_eq!(client.next(), Incoming::Connected);
        let Client {
            connection,
            incoming,
        } = client;
        drop(connection);

        assert!(matches!(
            incoming.recv_timeout(DEADLINE),
            Ok(Incoming::Closed { .. })
        ));
        assert_eq!(
            incoming.recv_timeout(DEADLINE),
            Err(RecvTimeoutError::Disconnected)
        );
        server.join().expect("server");
    }

    #[test]
    fn a_full_queue_fails_without_blocking() {
        // The server never completes the handshake, so nothing leaves the queue.
        let (address, server) = serve(|mut stream| {
            let mut buffer = [0; 4096];
            while stream.read(&mut buffer).is_ok_and(|read| read > 0) {}
        });

        let client = Client::open(address);
        for _ in 0..QUEUE_LENGTH {
            client.send(&Session::main(), "A.queued", json!({}));
        }
        assert_eq!(
            client
                .connection
                .send(&Session::main(), "A.dropped", json!({})),
            Err(CdpError::QueueFull)
        );

        client.connection.close();
        for _ in 0..QUEUE_LENGTH {
            assert!(matches!(reply_to(&client).2, Err(CdpError::Closed(_))));
        }
        assert!(matches!(client.next(), Incoming::Closed { .. }));

        client.finish();
        server.join().expect("server");
    }

    #[test]
    fn queued_requests_progress_after_write_backpressure() {
        #[derive(Default)]
        struct Stream {
            blocked: bool,
            bytes: Vec<u8>,
        }
        impl Read for Stream {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::WouldBlock.into())
            }
        }
        impl Write for Stream {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.blocked {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let stream = Stream {
            blocked: true,
            ..Stream::default()
        };
        let mut socket = WebSocket::from_raw_socket(
            stream,
            tungstenite::protocol::Role::Client,
            Some(WebSocketConfig::default().write_buffer_size(0)),
        );
        let (sender, outgoing) = mpsc::sync_channel(2);
        for id in 1..=2 {
            sender
                .send(Outgoing {
                    id,
                    session: Session::main(),
                    hops: Vec::new(),
                    text: json!({"id":id,"method":"A.queued"}).to_string(),
                })
                .expect("queue");
        }
        let mut transport = Transport {
            sink: Box::new(|_| true),
            pending: BTreeMap::new(),
            stopped: false,
        };
        assert!(
            transport
                .write_requests(&mut socket, &outgoing)
                .expect("blocked write")
        );
        assert_eq!(transport.pending.len(), 1);
        socket.get_mut().blocked = false;
        assert!(
            !transport
                .write_requests(&mut socket, &outgoing)
                .expect("writable again")
        );
        assert_eq!(
            transport.pending.len(),
            2,
            "the second request must be written before polling"
        );
        let mut server = WebSocket::from_raw_socket(
            io::Cursor::new(socket.into_inner().bytes),
            tungstenite::protocol::Role::Server,
            None,
        );
        for id in 1..=2 {
            let message = server
                .read()
                .expect("written request")
                .into_text()
                .expect("text");
            assert_eq!(
                serde_json::from_str::<Value>(&message).expect("JSON")["id"],
                id
            );
        }
    }

    #[test]
    fn outstanding_requests_are_bounded_after_the_queue_drains() {
        let (received, requests) = mpsc::channel();
        let (answer, answers) = mpsc::channel();
        let (address, server) = serve(move |stream| {
            let mut server = accept(stream);
            let mut ids = Vec::new();
            for _ in 0..MAX_OUTSTANDING {
                let message = request(&mut server);
                ids.push(message["id"].clone());
                received.send(()).expect("received request");
            }
            answers.recv_timeout(DEADLINE).expect("answer one");
            reply(&mut server, &json!({"id": ids[0], "result": {}}));
            let last = request(&mut server);
            reply(&mut server, &json!({"id": last["id"], "result": {}}));
            linger(server);
        });
        let client = Client::open(address);
        assert_eq!(client.next(), Incoming::Connected);
        let mut ids = Vec::new();
        for _ in 0..MAX_OUTSTANDING {
            ids.push(client.send(&Session::main(), "A.pending", json!({})));
            requests
                .recv_timeout(DEADLINE)
                .expect("server read request");
        }
        assert_eq!(
            client
                .connection
                .send(&Session::main(), "A.excess", json!({})),
            Err(CdpError::OutstandingFull)
        );
        answer.send(()).expect("allow reply");
        assert_eq!(reply_to(&client).0, ids[0]);
        let last = client.send(&Session::main(), "A.recovered", json!({}));
        assert_eq!(reply_to(&client).0, last);
        client.finish();
        server.join().expect("server");
    }

    #[test]
    fn outstanding_worker_envelopes_are_bounded_until_they_reply() {
        let (received, requests) = mpsc::channel();
        let (answer, answers) = mpsc::channel();
        let (address, server) = serve(move |stream| {
            let mut server = accept(stream);
            let mut wrappers = Vec::new();
            for _ in 0..MAX_OUTSTANDING - 1 {
                let outer = request(&mut server);
                let inner = untunnel(&outer, "worker");
                wrappers.push(outer["id"].clone());
                reply(
                    &mut server,
                    &tunnel("worker", &json!({"id": inner["id"], "result": {}})),
                );
                received.send(()).expect("received request");
            }
            answers.recv_timeout(DEADLINE).expect("answer wrapper");
            reply(&mut server, &json!({"id": wrappers[0], "result": {}}));
            let last = request(&mut server);
            reply(&mut server, &json!({"id": last["id"], "result": {}}));
            linger(server);
        });
        let client = Client::open(address);
        assert_eq!(client.next(), Incoming::Connected);
        let worker = Session::main().worker("worker");
        // Leave one slot for the last worker's owner, which its reply frees.
        for _ in 0..MAX_OUTSTANDING - 1 {
            let id = client.send(&worker, "A.pending", json!({}));
            requests
                .recv_timeout(DEADLINE)
                .expect("server read request");
            assert_eq!(reply_to(&client).0, id);
        }
        assert_eq!(
            client.connection.send(&worker, "A.excess", json!({})),
            Err(CdpError::OutstandingFull)
        );
        // A main-isolate request uses the remaining slot, unlike a worker request.
        let main = client.send(&Session::main(), "A.last", json!({}));
        answer.send(()).expect("allow wrapper reply");
        assert_eq!(reply_to(&client).0, main);
        client.finish();
        server.join().expect("server");
    }

    #[test]
    fn a_refused_connection_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("local address");
        drop(listener);

        let client = Client::open(address);
        let Incoming::Closed { reason } = client.next() else {
            panic!("expected Closed");
        };
        assert!(reason.starts_with("cannot connect"), "{reason}");
        client.finish();
    }

    #[test]
    fn sessions_nest_only_so_deep() {
        let (address, server) = serve(|stream| linger(accept(stream)));
        let client = Client::open(address);
        assert_eq!(client.next(), Incoming::Connected);

        let mut session = Session::main();
        for _ in 0..=MAX_SESSION_DEPTH {
            session = session.worker("w");
        }
        assert_eq!(
            client.connection.send(&session, "A.deep", json!({})),
            Err(CdpError::TooDeep)
        );

        let mut message = json!({"method": "Debugger.paused"});
        for _ in 0..=MAX_SESSION_DEPTH {
            message = tunnel("w", &message);
        }
        assert!(matches!(
            decode(&message.to_string()),
            Decoded::Malformed { session, .. } if session.path().len() == MAX_SESSION_DEPTH
        ));

        client.finish();
        server.join().expect("server");
    }
}
