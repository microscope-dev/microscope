//! The interactive line-editing REPL.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use rustyline::config::Configurer as _;
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::{ColorMode, CompletionType, Editor};

use super::commands::{Command, resolve_command};
use super::complete;
use super::config::state_directory;
use super::terminal::Role;
use super::{Cli, Renderers};

enum Input {
    /// A line, and the terminal's width and height when it was entered.
    Line(String, Option<(usize, usize)>),

    /// The answer to a question, empty when it was interrupted.
    Answer(String),

    /// Asks for the members of the value an expression names, to complete
    /// one.
    Members(String, mpsc::Sender<Vec<String>>),
    Eof,
    Failed(String),
}

/// What the line editor does after its input is handled.
enum Reply {
    /// Reads the next line after the prompt, completing from the context.
    Read(Arc<complete::Context>, String),
    Quit,

    /// Asks a question and sends the answer.
    Ask(String),
}

/// What comes after a line.
enum Next {
    Read,
    Quit,
    Ask(String),
}

/// How long completing waits for a value's members.
const MEMBERS_TIMEOUT: Duration = Duration::from_secs(2);

/// The line editor's helper, which completes from the context the last
/// reply sent.
struct LineHelper {
    context: Arc<complete::Context>,
    input: tokio::sync::mpsc::Sender<Input>,
}

impl rustyline::completion::Completer for LineHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        position: usize,
        _: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        let mut members = |base: &str| {
            let (send, receive) = mpsc::channel();
            if self
                .input
                .blocking_send(Input::Members(base.to_owned(), send))
                .is_err()
            {
                return Vec::new();
            }

            receive.recv_timeout(MEMBERS_TIMEOUT).unwrap_or_default()
        };

        let (start, mut candidates) =
            complete::complete(line, position, &self.context, &mut members);

        // A word completed alone is followed by the next, except a file's
        // line or function.
        if let [candidate] = candidates.as_mut_slice()
            && !candidate.ends_with(':')
        {
            candidate.push(' ');
        }

        Ok((start, candidates))
    }
}

impl rustyline::hint::Hinter for LineHelper {
    type Hint = String;
}

// The line itself is not highlighted, so command input stays unstyled.
impl rustyline::highlight::Highlighter for LineHelper {}

impl rustyline::validate::Validator for LineHelper {}

impl rustyline::Helper for LineHelper {}

/// Runs the REPL until `quit` or end-of-input. With `confirm`, `quit` asks
/// before killing a program that is still alive.
///
/// Rustyline blocks, so it runs on its own thread. After sending each line
/// the editor waits for a reply, which keeps it from reading the terminal
/// while a command, and possibly the inferior, is running.
/// What a line typed at the prompt does: runs its command, or one an empty
/// line repeats, or asks before a quit would kill the program.
async fn entered_line(
    cli: &Cli,
    entered: &str,
    confirm: bool,
    last_repeatable: &mut Option<String>,
) -> Next {
    // An empty line repeats the last repeatable command.
    let command = if entered.is_empty() {
        last_repeatable.clone().unwrap_or_default()
    } else {
        let expanded = cli.expand_alias(entered);
        let repeatable = expanded
            .as_deref()
            .unwrap_or(entered)
            .split_whitespace()
            .next()
            .and_then(|word| resolve_command(word).ok())
            .is_some_and(|spec| spec.repeatable);
        *last_repeatable = repeatable.then(|| entered.to_owned());
        entered.to_owned()
    };

    if confirm
        && quits(cli, &command)
        && let Some(process) = cli.live_process().await
    {
        Next::Ask(format!(
            "the program is still running (process {process}); kill it and quit? (y or n) "
        ))
    } else {
        run_line(cli, &command).await
    }
}

pub async fn run(cli: &Cli, confirm: bool) -> Result<()> {
    let (input_sender, mut inputs) = tokio::sync::mpsc::channel(1);
    let (ack_sender, acknowledgements) = mpsc::channel::<Reply>();
    let renderers = cli.renderers;
    let prompt = cli.prompt();
    let history_size = cli.settings.config.history.size;
    let context = Arc::new(cli.completion_context().await);
    let editor = thread::Builder::new()
        .name("microscope-line-editor".to_owned())
        .spawn(move || {
            line_editor(
                &input_sender,
                &acknowledgements,
                renderers,
                prompt,
                history_size,
                context,
            );
        })?;

    let mut outcome = Ok(());
    let mut last_repeatable = None;
    let mut group_events = cli.group_events();

    // What the group does while the editor waits for a line prints before
    // the next command's output; while a command runs, the command reports
    // it. Rustyline's external printer would print it above the prompt at
    // once, but loses typed input that arrives in one read with it.
    let mut at_prompt = true;
    let mut notices = Vec::new();
    loop {
        let input = tokio::select! {
            input = inputs.recv() => match input {
                Some(input) => input,
                None => break,
            },
            event = next_group_event(&mut group_events) => {
                if at_prompt
                    && let Some(line) = cli.group_notice(&event)
                {
                    notices.push(line);
                }
                continue;
            }
        };
        at_prompt = false;
        for notice in std::mem::take(&mut notices) {
            println!("{notice}");
        }

        let next = match input {
            Input::Line(text, dimensions) => {
                if let Some((columns, rows)) = dimensions {
                    cli.set_dimensions(columns, rows);
                }
                entered_line(cli, text.trim(), confirm, &mut last_repeatable).await
            }
            Input::Answer(answer) => {
                if answer.trim().to_ascii_lowercase().starts_with('y') {
                    run_line(cli, "quit").await
                } else {
                    Next::Read
                }
            }
            Input::Members(base, reply) => {
                let _ = reply.send(cli.member_names(&base).await);
                continue;
            }
            Input::Eof => Next::Quit,
            Input::Failed(error) => {
                outcome = Err(anyhow!(error));
                break;
            }
        };

        let reply = match next {
            Next::Read => {
                at_prompt = true;
                Reply::Read(Arc::new(cli.completion_context().await), cli.prompt())
            }
            Next::Quit => Reply::Quit,
            Next::Ask(question) => Reply::Ask(question),
        };

        let quit = matches!(reply, Reply::Quit);
        if ack_sender.send(reply).is_err() {
            outcome = Err(anyhow!(
                "line editor stopped before command acknowledgement"
            ));
            break;
        }

        if quit {
            break;
        }
    }

    tokio::task::spawn_blocking(move || editor.join())
        .await
        .context("failed to join line editor task")?
        .map_err(|_| anyhow!("line editor thread panicked"))?;
    outcome
}

/// Runs one line, and says whether the session goes on.
async fn run_line(cli: &Cli, line: &str) -> Next {
    let keep_running = cli.run_line(line).await.unwrap_or_else(|error| {
        cli.report_error(&error);
        true
    });
    if keep_running { Next::Read } else { Next::Quit }
}

/// Whether `line` runs `quit`, as written or through an alias.
fn quits(cli: &Cli, line: &str) -> bool {
    let expanded = cli.expand_alias(line);
    expanded
        .as_deref()
        .unwrap_or(line)
        .split_whitespace()
        .next()
        .and_then(|word| resolve_command(word).ok())
        .is_some_and(|spec| spec.command == Command::Quit)
}

/// The next event of the session's group, or never, for a session of one
/// process.
async fn next_group_event(
    events: &mut Option<tokio::sync::broadcast::Receiver<microscope::GroupEvent>>,
) -> microscope::GroupEvent {
    loop {
        let Some(receiver) = events.as_mut() else {
            return std::future::pending().await;
        };

        match receiver.recv().await {
            Ok(event) => return event,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => *events = None,
        }
    }
}

fn line_editor(
    input: &tokio::sync::mpsc::Sender<Input>,
    acknowledgements: &mpsc::Receiver<Reply>,
    renderers: Renderers,
    mut prompt: String,
    history_size: u32,
    context: Arc<complete::Context>,
) {
    let warn = |message: String| {
        eprintln!(
            "{}: {message}",
            renderers.stderr.paint(Role::Warning, "warning")
        );
    };

    let mut editor = match Editor::<LineHelper, DefaultHistory>::new() {
        Ok(editor) => editor,
        Err(error) => {
            let _ = input.blocking_send(Input::Failed(error.to_string()));
            return;
        }
    };

    // Rustyline needs a helper to select the styled prompt, and completes
    // through it.
    editor.set_helper(Some(LineHelper {
        context,
        input: input.clone(),
    }));
    editor.set_completion_type(CompletionType::List);
    editor.set_color_mode(if renderers.stdout.is_colored() {
        ColorMode::Forced
    } else {
        ColorMode::Disabled
    });
    let history = history_path(state_directory());
    let keep = usize::try_from(history_size).unwrap_or(usize::MAX);
    if let Err(error) = editor.set_max_history_size(keep) {
        warn(format!("failed to limit command history: {error}"));
    }

    if keep != 0
        && history.exists()
        && let Err(error) = editor.load_history(&history)
    {
        warn(format!(
            "failed to load command history {}: {error}",
            history.display()
        ));
    }

    loop {
        let styled_prompt = renderers.stdout.paint(Role::Prompt, &prompt).to_string();
        match editor.readline(&(prompt.as_str(), styled_prompt.as_str())) {
            Ok(line) => {
                if !line.trim().is_empty()
                    && let Err(error) = editor.add_history_entry(line.as_str())
                {
                    warn(format!("failed to record command history: {error}"));
                }

                let dimensions = editor
                    .dimensions()
                    .map(|(columns, rows)| (usize::from(columns), usize::from(rows)));
                if input.blocking_send(Input::Line(line, dimensions)).is_err() {
                    break;
                }

                match follow_replies(&mut editor, input, acknowledgements) {
                    Some(next) => prompt = next,
                    None => break,
                }
            }
            Err(ReadlineError::Interrupted) => {}
            Err(ReadlineError::Eof) => {
                let _ = input.blocking_send(Input::Eof);
                let _ = acknowledgements.recv();
                break;
            }
            Err(error) => {
                let _ = input.blocking_send(Input::Failed(error.to_string()));
                break;
            }
        }
    }

    if keep != 0
        && let Err(error) = persist_history(&mut editor, &history)
    {
        warn(error);
    }
}

/// Follows the replies to a line, asking the questions they ask, until one
/// says to read the next line, which returns the prompt to read it after,
/// or to stop.
fn follow_replies(
    editor: &mut Editor<LineHelper, DefaultHistory>,
    input: &tokio::sync::mpsc::Sender<Input>,
    replies: &mpsc::Receiver<Reply>,
) -> Option<String> {
    loop {
        let answer = match replies.recv() {
            Ok(Reply::Read(context, prompt)) => {
                if let Some(helper) = editor.helper_mut() {
                    helper.context = context;
                }

                return Some(prompt);
            }
            Ok(Reply::Quit) | Err(_) => return None,
            Ok(Reply::Ask(question)) => match editor.readline(&question) {
                Ok(answer) => Input::Answer(answer),
                Err(ReadlineError::Interrupted) => Input::Answer(String::new()),
                // End of input never asks, so it answers yes.
                Err(ReadlineError::Eof) => Input::Eof,
                Err(error) => Input::Failed(error.to_string()),
            },
        };

        if input.blocking_send(answer).is_err() {
            return None;
        }
    }
}

/// Locates the history file in the user's state directory, falling back to
/// the current directory.
fn history_path(state: Option<PathBuf>) -> PathBuf {
    state.map_or_else(
        || PathBuf::from(".microscope_history"),
        |state| state.join("history"),
    )
}

fn persist_history(
    editor: &mut Editor<LineHelper, DefaultHistory>,
    path: &Path,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "failed to create history directory {}: {error}",
                parent.display()
            )
        })?;
    }

    if path.exists() {
        editor.append_history(path)
    } else {
        editor.save_history(path)
    }
    .map_err(|error| format!("failed to save command history {}: {error}", path.display()))
}
