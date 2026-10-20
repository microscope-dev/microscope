//! One tab's WebSocket.
//!
//! A connection sends `hello`, the current state, recent output, then
//! everything as it happens. Each request runs in a task of its own, so a
//! slow one, such as loading a large program, never holds up the state,
//! output, or other answers the tab is waiting for.
//!
//! Every message is JSON text but one: a drawing's numbers and bytes, and a
//! profile's call tree, go in a binary frame just before its answer, the request's id as 8 bytes,
//! little-endian, then the bytes.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket};
use tokio::sync::{broadcast, mpsc};

use super::profiles::{self, Profiles};
use super::protocol::{
    self, Envelope, ErrorBody, ErrorKind, Hello, ProfileList, Role, ServerMessage,
};

use super::session::Session;

/// Answers waiting to be sent; a tab that stops reading slows only its own
/// requests.
const PENDING_ANSWERS: usize = 64;

pub async fn serve(
    mut socket: WebSocket,
    session: Arc<Session>,
    profiles: Arc<Profiles>,
    role: Role,
) {
    let mut joined = session.join(role);
    let mut listed = profiles.subscribe();
    let connection = joined.connection;
    let hello = ServerMessage::Hello(Hello {
        version: protocol::VERSION,
        connection,
        role,
        name: joined.name.clone(),
        cwd: session.cwd().display().to_string(),
        share: session.shares(),
    });
    let state = ServerMessage::State(Arc::clone(&joined.state.borrow_and_update()));
    let list = ServerMessage::Profiles(ProfileList::clone(&listed.borrow_and_update()));
    let mut opening = vec![encode(&hello), encode(&state), encode(&list)];
    opening.extend(joined.history.iter().map(|text| Utf8Bytes::from(&**text)));

    for text in opening {
        if socket.send(Message::Text(text)).await.is_err() {
            session.leave(connection);
            return;
        }
    }

    let (answers, mut pending) = mpsc::channel::<Message>(PENDING_ANSWERS);

    loop {
        // Biased, in this order: requests are read even during a flood of
        // output, and a request's state change goes out before its answer.
        let outgoing = tokio::select! {
            biased;
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    dispatch(&session, &profiles, connection, role, text.as_str(), answers.clone());
                    continue;
                }
                Some(Ok(Message::Binary(_))) => Message::Text(encode(&error(0, ErrorKind::Invalid, "messages are JSON text"))),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
            },
            changed = joined.state.changed() => {
                if changed.is_err() {
                    break;
                }

                let state = Arc::clone(&joined.state.borrow_and_update());
                Message::Text(encode(&ServerMessage::State(state)))
            }
            changed = listed.changed() => {
                if changed.is_err() {
                    break;
                }

                let list = ProfileList::clone(&listed.borrow_and_update());
                Message::Text(encode(&ServerMessage::Profiles(list)))
            }
            Some(answer) = pending.recv() => answer,
            message = joined.messages.recv() => match message {
                Ok(text) => Message::Text(Utf8Bytes::from(&*text)),
                // Output was dropped; the state, which matters, is sent whole.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
        };

        if socket.send(outgoing).await.is_err() {
            break;
        }
    }

    session.leave(connection);
}

fn dispatch(
    session: &Arc<Session>,
    profiles: &Arc<Profiles>,
    connection: u32,
    role: Role,
    text: &str,
    answers: mpsc::Sender<Message>,
) {
    let envelope = match serde_json::from_str::<Envelope>(text) {
        Ok(envelope) => envelope,
        Err(problem) => {
            // Answer with the id when the request names one.
            let id = serde_json::from_str::<serde_json::Value>(text)
                .ok()
                .and_then(|value| value.get("id")?.as_u64())
                .unwrap_or(0);
            let answer =
                Message::Text(encode(&error(id, ErrorKind::Invalid, &problem.to_string())));
            tokio::spawn(async move { answers.send(answer).await });
            return;
        }
    };

    let session = Arc::clone(session);
    let profiles = Arc::clone(profiles);

    tokio::spawn(async move {
        let id = envelope.id;
        let answered = match envelope.request {
            protocol::Request::Draw(draw) => session.draw(&draw).await,
            request if profiles::is_profile_request(&request) => {
                profiles.handle(role, request).await
            }
            request => session
                .handle(connection, role, request)
                .await
                .map(|result| (result, Vec::new())),
        };

        let answer = match answered {
            Ok((result, bytes)) => {
                if !bytes.is_empty() {
                    let mut frame = Vec::with_capacity(8 + bytes.len());
                    frame.extend_from_slice(&id.to_le_bytes());
                    frame.extend_from_slice(&bytes);
                    if answers
                        .send(Message::Binary(Bytes::from(frame)))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }

                ServerMessage::Result { id, result }
            }
            Err(failure) => ServerMessage::Error {
                id,
                error: failure.body(),
            },
        };

        let _ = answers.send(Message::Text(encode(&answer))).await;
    });
}

fn error(id: u64, kind: ErrorKind, message: &str) -> ServerMessage {
    ServerMessage::Error {
        id,
        error: ErrorBody {
            kind,
            message: message.to_owned(),
        },
    }
}

fn encode(message: &ServerMessage) -> Utf8Bytes {
    super::session::encode(message).into()
}
