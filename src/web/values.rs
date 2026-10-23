//! A stop's values for the page: scopes, the value tree, watches, and
//! assignments, presented as the debug adapter presents them.
//!
//! A row that expands carries a handle, which belongs to the connection
//! that asked and the row's stop. The page keeps no handle across stops: it
//! remembers which paths were open and opens them again in the next stop's
//! rows. Handles count up for the whole server, so a handle never names two
//! things and an answer cached by its handle is never wrong.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use microscope::{
    DebuggerHandle, DereferenceReference, Evaluation, EvaluationMode, Expression, InspectionLimits,
    StopContext, StopId, ValueChildrenReference, VariableKind, VariableState,
};

use super::describe::Images;
use super::inspect;
use super::protocol::{self, ErrorKind, FrameAt, Row, Scope, ScopeKey, Scopes};
use super::session::Failure;
use crate::present::{self, Code, Expand, Item, ListError, Listed, Presenter, Window, complete};

/// The most rows one `children` request returns.
const MOST_CHILDREN: u64 = 500;

/// The most statics a scope lists.
const MOST_STATICS: u64 = 500;

/// The most handles one connection holds at one stop.
const MOST_HANDLES: usize = 100_000;

/// The most completions one request offers.
const MOST_COMPLETIONS: usize = 1000;

/// What a handle expands.
#[derive(Clone)]
enum Node {
    Children {
        context: StopContext,
        reference: Arc<ValueChildrenReference>,
        path: Option<Expression>,
    },
    Pointee {
        context: StopContext,
        reference: Box<DereferenceReference>,
        name: String,
        path: Option<Expression>,
    },
    Range {
        context: StopContext,
        expression: Expression,
    },
}

/// Every connection's handles.
#[derive(Default)]
pub struct Handles {
    tables: Mutex<HashMap<u32, Table>>,
    next: AtomicU64,
}

/// One connection's handles, all at one stop of one session.
struct Table {
    session: String,
    stop: StopId,
    nodes: HashMap<u64, Node>,
}

impl Handles {
    /// Forgets a connection that left.
    pub fn forget(&self, connection: u32) {
        self.tables
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&connection);
    }

    fn insert(
        &self,
        connection: u32,
        session: &str,
        stop: StopId,
        node: Node,
    ) -> Result<u64, Failure> {
        let mut tables = self.tables.lock().unwrap_or_else(PoisonError::into_inner);
        let table = tables.entry(connection).or_insert_with(|| Table {
            session: session.to_owned(),
            stop,
            nodes: HashMap::new(),
        });

        // A new stop, or a new session, makes the old handles stale.
        if table.session != session || table.stop != stop {
            *table = Table {
                session: session.to_owned(),
                stop,
                nodes: HashMap::new(),
            };
        }

        if table.nodes.len() >= MOST_HANDLES {
            return Err(Failure::new(
                ErrorKind::Failed,
                "too many values are open at this stop; collapse some",
            ));
        }

        let handle = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        table.nodes.insert(handle, node);
        drop(tables);

        Ok(handle)
    }

    fn get(&self, connection: u32, session: &str, handle: u64) -> Result<Node, Failure> {
        self.tables
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&connection)
            .filter(|table| table.session == session)
            .and_then(|table| table.nodes.get(&handle).cloned())
            .ok_or_else(|| {
                Failure::new(
                    ErrorKind::StaleStop,
                    format!("value {handle} belongs to an earlier stop, or never existed"),
                )
            })
    }
}

/// Reads one connection's values from the current session.
pub struct Reader<'a> {
    pub handle: DebuggerHandle,
    pub images: Arc<Images>,
    pub code: Code,
    pub handles: &'a Handles,
    pub connection: u32,

    /// The session whose stops the handles belong to.
    pub session: String,
}

impl Reader<'_> {
    const fn presenter(&self) -> Presenter<'_> {
        Presenter {
            handle: &self.handle,
            code: &self.code,
            hex: false,
        }
    }

    /// A row as the page receives it, with a handle when it expands.
    fn row(&self, context: StopContext, row: present::Row) -> Result<Row, Failure> {
        let path = row.evaluate_name().map(ToString::to_string);
        let mut children = None;
        let mut editable = false;
        let mut memory = None;
        let mut memory_bytes = None;
        let mut drawings = Vec::new();
        let mut detail = None;
        if let Some(details) = row.details {
            detail = details.detail.as_deref().map(str::to_owned);
            editable = details.editable;
            drawings = details.drawings.iter().map(ToString::to_string).collect();
            memory = details.memory.map(|address| format!("{address:#x}"));
            memory_bytes = details.memory_bytes;
            let (node, counts) = match details.expand {
                Some(Expand::Children {
                    reference, counts, ..
                }) => (
                    Some(Node::Children {
                        context,
                        reference,
                        path: row.path,
                    }),
                    counts,
                ),
                Some(Expand::Pointee(reference)) => (
                    Some(Node::Pointee {
                        context,
                        reference,
                        name: row.name.clone(),
                        path: row.path,
                    }),
                    present::Counts::default(),
                ),
                None => (None, present::Counts::default()),
            };

            if let Some(node) = node {
                children = Some(protocol::Children {
                    handle: self.handles.insert(
                        self.connection,
                        &self.session,
                        context.stop,
                        node,
                    )?,
                    indexed: counts.indexed,
                    named: counts.named,
                });
            }
        }

        Ok(Row {
            name: row.name,
            text: row.text,
            detail,
            type_name: row.type_name.map(|name| name.to_string()),
            path,
            children,
            editable,
            memory,
            memory_bytes,
            truncated: false,
            drawings,
        })
    }

    fn rows(&self, context: StopContext, listed: Vec<Listed>) -> Result<Vec<Row>, Failure> {
        listed
            .into_iter()
            .map(|entry| match entry {
                Listed::Value(row) => self.row(context, *row),
                Listed::Truncated(text) => Ok(Row {
                    name: "<truncated>".to_owned(),
                    text,
                    detail: None,
                    type_name: None,
                    path: None,
                    children: None,
                    editable: false,
                    memory: None,
                    memory_bytes: None,
                    truncated: true,
                    drawings: Vec::new(),
                }),
            })
            .collect()
    }

    pub async fn scopes(&self, at: FrameAt) -> Result<Scopes, Failure> {
        let context = inspect::context(&self.handle, at.stop, at.execution(), at.frame).await?;
        let trace = self
            .handle
            .at(inspect::innermost(at.stop, at.execution()))
            .backtrace()
            .await?;
        let frame = trace.frames.iter().find(|frame| frame.id == context.frame);
        let module = frame.and_then(|frame| frame.module);
        let file = frame.and_then(|frame| Some(frame.source.as_ref()?.file));
        let mut scopes = Vec::new();
        match self.handle.at(context).variables().await {
            Ok(snapshot) => {
                for (key, name, kind) in [
                    (ScopeKey::Args, "Arguments", VariableKind::Parameter),
                    (ScopeKey::Locals, "Locals", VariableKind::Local),
                ] {
                    let mut listed = self
                        .presenter()
                        .scope(context, &snapshot, kind, module, Window::ALL)
                        .await;
                    for row in &mut listed {
                        if let Listed::Value(row) = row {
                            self.add_pointee_drawings(row).await;
                        }
                    }

                    scopes.push(Scope {
                        key,
                        name: name.to_owned(),
                        rows: self.rows(context, listed)?,
                        problem: None,
                    });
                }
            }
            Err(error @ microscope::Error::StaleStop) => return Err(error.into()),
            Err(error) => scopes.push(Scope {
                key: ScopeKey::Locals,
                name: "Locals".to_owned(),
                rows: Vec::new(),
                problem: Some(error.to_string()),
            }),
        }

        if let (Some(module), Some(file)) = (module, file)
            && let Some(image) = self.images.get(module).await
        {
            let window = Window {
                count: MOST_STATICS,
                ..Window::ALL
            };

            let (rows, problem) = match self
                .presenter()
                .statics(context, &image, module, file, window)
                .await
            {
                Ok(listed) => (self.rows(context, listed)?, None),
                Err(failure) => (Vec::new(), Some(list_failure(failure).body().message)),
            };

            scopes.push(Scope {
                key: ScopeKey::Statics,
                name: "Statics".to_owned(),
                rows,
                problem,
            });
        }

        Ok(Scopes { scopes })
    }

    pub async fn children(&self, request: &protocol::ChildrenOf) -> Result<Vec<Row>, Failure> {
        let node = self
            .handles
            .get(self.connection, &self.session, request.handle)?;
        let window = Window {
            start: request.start,
            count: request.count.min(MOST_CHILDREN),
            filter: None,
        };

        let presenter = self.presenter();

        let (context, listed) = match node {
            Node::Children {
                context,
                reference,
                path,
            } => (
                context,
                presenter
                    .children(context, reference, path.as_ref(), window)
                    .await,
            ),
            Node::Pointee {
                context,
                reference,
                name,
                path,
            } => (
                context,
                presenter
                    .pointee(context, reference, &name, path, window)
                    .await,
            ),
            Node::Range {
                context,
                expression,
            } => (context, presenter.range(context, &expression, window).await),
        };

        self.rows(context, listed.map_err(list_failure)?)
    }

    /// Evaluates `text` in a frame, presenting its value as a row named by
    /// the text. Watches and hovers read; the console may assign.
    pub async fn evaluate(
        &self,
        at: FrameAt,
        text: &str,
        mode: EvaluationMode,
    ) -> Result<Row, Failure> {
        let context = inspect::context(&self.handle, at.stop, at.execution(), at.frame).await?;
        let expression = Expression::parse(text).map_err(expression_failure)?;
        self.evaluated(context, text, expression, mode).await
    }

    /// Evaluates an expression already parsed in `context`, and presents it.
    pub async fn evaluated(
        &self,
        context: StopContext,
        text: &str,
        expression: Expression,
        mode: EvaluationMode,
    ) -> Result<Row, Failure> {
        let evaluation = self
            .handle
            .at(context)
            .evaluate_with(&expression, mode, InspectionLimits::default())
            .await?;
        if let Evaluation::Value { value, .. } = &evaluation {
            let mut row = self.presenter().row(
                Item {
                    name: text,
                    path: Some(expression),
                    raw: false,
                    type_info: value.type_info.as_ref(),
                    state: &value.state,
                    declaration: None,
                },
                context,
            );

            self.add_pointee_drawings(&mut row).await;
            return self.row(context, row);
        }

        self.present(context, text, expression, evaluation)
    }

    /// Gives a pointer the drawings of what it points to, so that a frame's
    /// `&mut Board` is drawn as its board. Variables and watches read their
    /// pointees for this; a page of children never does.
    async fn add_pointee_drawings(&self, row: &mut present::Row) {
        let Some(details) = row.details.as_mut() else {
            return;
        };

        if !details.drawings.is_empty() {
            return;
        }

        let Some(Expand::Pointee(reference)) = &details.expand else {
            return;
        };

        if let Ok(pointee) = self.handle.dereference(reference.clone()).await
            && let VariableState::Available {
                presentation: Some(presentation),
                ..
            } = &pointee.state
        {
            details.drawings = present::drawings(presentation);
        }
    }

    /// Presents what `expression`, written as `text`, evaluated to.
    pub fn present(
        &self,
        context: StopContext,
        text: &str,
        expression: Expression,
        evaluation: Evaluation,
    ) -> Result<Row, Failure> {
        match evaluation {
            Evaluation::Value { value, .. } => self.row(
                context,
                self.presenter().row(
                    Item {
                        name: text,
                        path: Some(expression),
                        raw: false,
                        type_info: value.type_info.as_ref(),
                        state: &value.state,
                        declaration: None,
                    },
                    context,
                ),
            ),
            Evaluation::Range(page) => {
                let length = page.children.len() as u64;
                let handle = self.handles.insert(
                    self.connection,
                    &self.session,
                    context.stop,
                    Node::Range {
                        context,
                        expression,
                    },
                )?;

                Ok(Row {
                    name: text.to_owned(),
                    text: format!("[<{length} elements>]"),
                    detail: None,
                    type_name: None,
                    path: None,
                    children: Some(protocol::Children {
                        handle,
                        indexed: Some(length),
                        named: None,
                    }),
                    editable: false,
                    memory: None,
                    memory_bytes: None,
                    truncated: false,
                    drawings: Vec::new(),
                })
            }
            _ => Err(Failure::new(
                ErrorKind::Unsupported,
                "the evaluation produced an unknown kind of result",
            )),
        }
    }

    /// Assigns `value` to what `path` names, and presents the result.
    pub async fn set_value(&self, at: FrameAt, path: &str, value: &str) -> Result<Row, Failure> {
        let target = Expression::parse(path).map_err(expression_failure)?;
        if target.assignment_target().is_some() {
            return Err(Failure::new(
                ErrorKind::Invalid,
                "a value's path names it; it does not assign",
            ));
        }

        let context = inspect::context(&self.handle, at.stop, at.execution(), at.frame).await?;
        let assignment =
            Expression::parse(&format!("{target} = {value}")).map_err(expression_failure)?;
        let mut row = self
            .evaluated(context, path, assignment, EvaluationMode::Assign)
            .await?;
        row.path = Some(target.to_string());
        Ok(row)
    }

    /// Completes the text of a console line, in a frame when one is given.
    pub async fn complete(
        &self,
        text: &str,
        at: Option<FrameAt>,
    ) -> Result<protocol::Completions, Failure> {
        let context = match at {
            Some(at) => {
                Some(inspect::context(&self.handle, at.stop, at.execution(), at.frame).await?)
            }
            None => None,
        };

        let (completing, partial, start) = complete::completing(text);
        let command = text.split_whitespace().next().unwrap_or_default();
        let variables = match (context, completing) {
            (Some(context), complete::Completing::Name { .. }) => {
                self.handle.at(context).variables().await.ok()
            }
            _ => None,
        };

        let candidates = complete::candidates(
            &self.presenter(),
            completing,
            partial,
            command,
            context,
            variables.as_ref(),
        )
        .await;

        Ok(protocol::Completions {
            start: text[..start].chars().count() as u64,
            items: complete::matching(candidates, partial, MOST_COMPLETIONS)
                .into_iter()
                .map(|(label, kind)| protocol::Completion {
                    label,
                    kind: kind.to_owned(),
                })
                .collect(),
        })
    }
}

fn list_failure(failure: ListError) -> Failure {
    match failure {
        ListError::Debugger(error) => error.into(),
        ListError::Changed(message) => Failure::new(ErrorKind::Invalid, message),
    }
}

/// A malformed expression, said on one line.
pub fn expression_failure(failure: microscope::ExpressionError) -> Failure {
    microscope::Error::Expression(failure).into()
}
