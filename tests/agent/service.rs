//! A fake service: it accepts agents' `WebSocket`s and hands each connection
//! to the test, which plays the service's side message by message.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use prost::Message as _;
use tokio::sync::mpsc as async_mpsc;

use crate::DEADLINE;

#[allow(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    unreachable_pub,
    reason = "prost generates this code"
)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/microscope.agent.v1.rs"));
}

use proto::{AgentMessage, Hello, ServerMessage, Welcome, agent_message, server_message};

/// The limits the fake service's welcome gives unless a test says.
pub const CHUNK: u32 = 64 << 10;
pub const WINDOW: u64 = 256 << 10;

pub struct Service {
    pub url: String,
    connections: mpsc::Receiver<Connection>,
    refusals: Arc<Mutex<VecDeque<StatusCode>>>,
    _runtime: tokio::runtime::Runtime,
}

#[derive(Clone)]
struct Shared {
    connections: mpsc::Sender<Connection>,
    refusals: Arc<Mutex<VecDeque<StatusCode>>>,
}

impl Service {
    pub fn start() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (sender, connections) = mpsc::channel();
        let refusals = Arc::new(Mutex::new(VecDeque::new()));
        let shared = Shared {
            connections: sender,
            refusals: Arc::clone(&refusals),
        };

        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let router = Router::new()
            .route("/v1/agent", get(upgrade))
            .with_state(shared);
        runtime.spawn(async move { axum::serve(listener, router).await });

        Self {
            url,
            connections,
            refusals,
            _runtime: runtime,
        }
    }

    /// Answers the next attempt to connect with `status`, refusing it.
    pub fn refuse_next(&self, status: StatusCode) {
        self.refusals.lock().unwrap().push_back(status);
    }

    /// The next connection from an agent.
    pub fn accept(&self) -> Connection {
        self.connections
            .recv_timeout(DEADLINE)
            .expect("the agent never connected")
    }

    /// The next connection, after the agent said hello and was welcomed.
    pub fn welcomed(&self) -> Connection {
        let connection = self.accept();
        connection.hello();
        connection.welcome(CHUNK, WINDOW);
        connection
    }

    /// Checks that no agent connects for `wait`.
    pub fn no_connection_within(&self, wait: Duration) {
        assert!(
            self.connections.recv_timeout(wait).is_err(),
            "the agent connected again"
        );
    }
}

async fn upgrade(
    State(shared): State<Shared>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let refusal = shared.refusals.lock().unwrap().pop_front();
    if let Some(status) = refusal {
        return status.into_response();
    }
    let text = |name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let authorization = text(header::AUTHORIZATION);
    let user_agent = text(header::USER_AGENT);
    upgrade.on_upgrade(move |socket| serve(socket, authorization, user_agent, shared.connections))
}

/// What the test asks the fake service to send.
enum Out {
    Message(ServerMessage),
    Raw(Message),
}

/// What the agent did.
#[derive(Debug)]
pub enum In {
    Message(AgentMessage),
    /// The agent closed the connection, with this close code if it sent one.
    Closed(Option<u16>),
}

async fn serve(
    mut socket: WebSocket,
    authorization: Option<String>,
    user_agent: Option<String>,
    connections: mpsc::Sender<Connection>,
) {
    let (outgoing, mut out) = async_mpsc::unbounded_channel();
    let (sender, incoming) = mpsc::channel();
    let connection = Connection {
        authorization,
        user_agent,
        outgoing,
        incoming,
    };
    if connections.send(connection).is_err() {
        return;
    }

    loop {
        tokio::select! {
            message = socket.recv() => match message {
                Some(Ok(Message::Binary(frame))) => {
                    let _ = sender.send(In::Message(decode(&frame)));
                }
                Some(Ok(Message::Close(frame))) => {
                    let _ = sender.send(In::Closed(frame.map(|frame| frame.code)));
                    return;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => {
                    let _ = sender.send(In::Closed(None));
                    return;
                }
            },
            next = out.recv() => {
                let message = match next {
                    Some(Out::Message(message)) => Message::Binary(encode(&message).into()),
                    Some(Out::Raw(message)) => message,
                    // The test dropped the connection: it ends without a
                    // close frame, as when a network fails.
                    None => return,
                };
                if socket.send(message).await.is_err() {
                    let _ = sender.send(In::Closed(None));
                    return;
                }
            }
        }
    }
}

pub fn encode(message: &impl prost::Message) -> Vec<u8> {
    zstd::bulk::compress(&message.encode_to_vec(), 3).unwrap()
}

fn decode(frame: &[u8]) -> AgentMessage {
    let size = zstd::zstd_safe::get_frame_content_size(frame)
        .unwrap()
        .expect("the agent's frames declare their size");
    let bytes = zstd::bulk::decompress(frame, usize::try_from(size).unwrap()).unwrap();
    AgentMessage::decode(bytes.as_slice()).unwrap()
}

/// One agent's connection, played from the service's side. Dropping it
/// drops the connection.
pub struct Connection {
    pub authorization: Option<String>,
    pub user_agent: Option<String>,
    outgoing: async_mpsc::UnboundedSender<Out>,
    incoming: mpsc::Receiver<In>,
}

impl Connection {
    /// The agent's next message.
    pub fn recv(&self) -> agent_message::Body {
        match self.incoming.recv_timeout(DEADLINE) {
            Ok(In::Message(message)) => message.body.expect("a message with a body"),
            Ok(In::Closed(code)) => panic!("the agent closed the connection with {code:?}"),
            Err(error) => panic!("the agent sent nothing: {error}"),
        }
    }

    pub fn send(&self, body: server_message::Body) {
        let message = ServerMessage { body: Some(body) };
        self.outgoing.send(Out::Message(message)).unwrap();
    }

    /// Sends a WebSocket message as it is.
    pub fn send_raw(&self, message: Message) {
        self.outgoing.send(Out::Raw(message)).unwrap();
    }

    pub fn hello(&self) -> Hello {
        match self.recv() {
            agent_message::Body::Hello(hello) => hello,
            other => panic!("expected hello, got {other:?}"),
        }
    }

    pub fn welcome(&self, max_chunk_bytes: u32, window_bytes: u64) {
        self.send(server_message::Body::Welcome(Welcome {
            max_chunk_bytes,
            window_bytes,
            project_name: "acme/production".to_owned(),
            project_id: 7,
            ..Welcome::default()
        }));
    }

    /// Waits for the agent to close the connection, and returns its close
    /// code, skipping any messages before.
    pub fn closed(&self) -> Option<u16> {
        loop {
            match self.incoming.recv_timeout(DEADLINE) {
                Ok(In::Message(_)) => {}
                Ok(In::Closed(code)) => return code,
                Err(error) => panic!("the agent never closed the connection: {error}"),
            }
        }
    }

    /// Checks that the agent sends nothing for `wait`.
    pub fn nothing_within(&self, wait: Duration) {
        match self.incoming.recv_timeout(wait) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(other) => panic!("the agent sent {other:?}"),
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the connection ended"),
        }
    }
}
