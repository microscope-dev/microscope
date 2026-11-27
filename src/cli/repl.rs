//! Interactive `rustyline` REPL loop for the CLI frontend.
//!
//! Blocking terminal I/O runs on a dedicated OS thread communicating with the async
//! `Cli` loop over channels so group notices and async member-name completions work
//! without stalling the Tokio runtime.

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
    Line(String, Option<(usize, usize)>),

    Answer(String),

    Members(String, mpsc::Sender<Vec<String>>),
    Eof,
    Failed(String),
}

enum Reply {
    Read(Arc<complete::Context>, String),
    Quit,

    Ask(String),
}

enum Next {
    Read,
    Quit,
    Ask(String),
}

const MEMBERS_TIMEOUT: Duration = Duration::from_secs(2);

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

impl rustyline::highlight::Highlighter for LineHelper {}

impl rustyline::validate::Validator for LineHelper {}

impl rustyline::Helper for LineHelper {}

async fn entered_line(
    cli: &Cli,
    entered: &str,
    confirm: bool,
    last_repeatable: &mut Option<String>,
) -> Next {
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

    // Buffer asynchronous group notices while `rustyline` owns the prompt so they
    // don't interleave with the user's editable line; flush them as soon as input arrives.
    let mut at_prompt = true;
    let mut notices = Vec::new();
    loop {
        let input = tokio::select! {
            input = inputs.recv() => match input {
                Some(input) => input,
                None => break,
            },
            notice = cli.next_group_notice() => {
                if at_prompt
                    && let Some(line) = notice
                {
                    notices.push(line);
                }
                continue;
            }
        };
        let next = match input {
            Input::Members(base, reply) => {
                let _ = reply.send(cli.member_names(&base).await);
                continue;
            }
            Input::Line(text, dimensions) => {
                flush_notices(&mut at_prompt, &mut notices);
                if let Some((columns, rows)) = dimensions {
                    cli.set_dimensions(columns, rows);
                }
                entered_line(cli, text.trim(), confirm, &mut last_repeatable).await
            }
            Input::Answer(answer) => {
                flush_notices(&mut at_prompt, &mut notices);
                if answer.trim().to_ascii_lowercase().starts_with('y') {
                    run_line(cli, "quit").await
                } else {
                    Next::Read
                }
            }
            Input::Eof => {
                flush_notices(&mut at_prompt, &mut notices);
                Next::Quit
            }
            Input::Failed(error) => {
                flush_notices(&mut at_prompt, &mut notices);
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

fn flush_notices(at_prompt: &mut bool, notices: &mut Vec<String>) {
    *at_prompt = false;
    for notice in std::mem::take(notices) {
        println!("{notice}");
    }
}

async fn run_line(cli: &Cli, line: &str) -> Next {
    let keep_running = cli.run_line(line).await.unwrap_or_else(|error| {
        cli.report_error(&error);
        true
    });
    if keep_running { Next::Read } else { Next::Quit }
}

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
                Err(ReadlineError::Eof) => Input::Eof,
                Err(error) => Input::Failed(error.to_string()),
            },
        };

        if input.blocking_send(answer).is_err() {
            return None;
        }
    }
}

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
