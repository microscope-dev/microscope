//! Completion for the line editor, from a context the REPL sends it with
//! each reply, so that completing never waits on the debugger, except for
//! a value's fields, which the line names.

use std::sync::Arc;

use super::Cli;
use super::commands::{COMMANDS, Command, INFO_SUBCOMMANDS, resolve_command};
use crate::present::complete::Completing;

/// What the line editor completes from.
#[derive(Clone, Debug, Default)]
pub struct Context {
    /// The settings' aliases.
    pub aliases: Arc<[String]>,

    /// The functions of the loaded modules, or the program's.
    pub functions: Arc<[String]>,

    /// The names of their source files.
    pub files: Arc<[String]>,

    /// Breakpoint ids, and watchpoint ids with their `w`.
    pub breakpoints: Arc<[String]>,
    pub watchpoints: Arc<[String]>,
    pub displays: Arc<[String]>,

    /// The selected frame's variables.
    pub names: Arc<[String]>,

    /// The globals of the loaded modules.
    pub globals: Arc<[String]>,
}

/// What completion knows of the code, kept while the same modules are
/// loaded.
#[derive(Clone, Debug)]
pub struct Code {
    modules: Vec<microscope::ModuleId>,
    functions: Arc<[String]>,
    files: Arc<[String]>,
    globals: Arc<[String]>,
}

/// The selected frame's variables, kept for one stop and frame.
#[derive(Clone, Debug)]
pub struct Names {
    frame: (microscope::StopId, Option<microscope::StackFrameId>),
    names: Arc<[String]>,
}

impl Cli {
    /// What completion knows now: the code of the loaded modules, built
    /// once for each set of them, and the selected frame's names, read once
    /// for each stop and frame.
    pub async fn completion_context(&self) -> Context {
        let snapshot = self.debugger.snapshot().await.ok();
        let code = self.completion_code().await;
        let names = match snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.stop_id.map(|stop| (stop, snapshot.selected_frame)))
        {
            Some(frame) => self.frame_names(frame).await,
            None => Arc::default(),
        };

        let (breakpoints, watchpoints) =
            snapshot.as_ref().map_or_else(Default::default, |snapshot| {
                (
                    snapshot
                        .breakpoints
                        .iter()
                        .map(|breakpoint| breakpoint.id.to_string())
                        .collect(),
                    snapshot
                        .watchpoints
                        .iter()
                        .map(|watchpoint| format!("w{}", watchpoint.id))
                        .collect(),
                )
            });

        Context {
            aliases: self.settings.config.aliases.keys().cloned().collect(),
            functions: code.functions,
            files: code.files,
            breakpoints,
            watchpoints,
            displays: self
                .displays
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .list
                .iter()
                .map(|display| display.id.to_string())
                .collect(),
            names,
            globals: code.globals,
        }
    }

    /// What completion knows of the loaded modules' code.
    async fn completion_code(&self) -> Code {
        let modules = match self.debugger.loaded_modules().await {
            Ok(loaded) => loaded
                .modules
                .iter()
                .map(|record| record.module.id)
                .collect(),
            Err(_) => Vec::new(),
        };

        let cached = self
            .completion_code
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .filter(|code| code.modules == modules);
        if let Some(code) = cached {
            return code;
        }

        let code = self.code_names(modules).await;
        *self
            .completion_code
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(code.clone());
        code
    }

    /// The variables of the selected frame, `frame` at its stop.
    async fn frame_names(
        &self,
        frame: (microscope::StopId, Option<microscope::StackFrameId>),
    ) -> Arc<[String]> {
        let cached = self
            .completion_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .filter(|names| names.frame == frame);
        if let Some(names) = cached {
            return names.names;
        }

        let names: Arc<[String]> = self.debugger.variables().await.map_or_else(
            |_| Arc::default(),
            |variables| {
                variables
                    .variables
                    .iter()
                    .map(|variable| variable.name.to_string())
                    .collect()
            },
        );

        *self
            .completion_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Names {
            frame,
            names: names.clone(),
        });

        names
    }

    /// The functions, source file names, and globals of the loaded
    /// modules, or of the program before it runs.
    async fn code_names(&self, modules: Vec<microscope::ModuleId>) -> Code {
        let mut functions = std::collections::BTreeSet::new();
        let mut files = std::collections::BTreeSet::new();
        let mut globals = std::collections::BTreeSet::new();

        for image in self.loaded_images().await {
            for function in image.functions() {
                functions.insert(function.name().to_string());
            }

            for symbol in image.symbols() {
                if symbol.kind() == microscope::SymbolKind::Function && symbol.extent().is_some() {
                    functions.insert(symbol.name().to_string());
                }
            }

            for file in image.source_files() {
                if let Some(name) = file.path.file_name().and_then(|name| name.to_str()) {
                    files.insert(name.to_owned());
                }
            }

            for global in image.globals() {
                globals.insert(global.qualified_name.to_string());
            }
        }

        Code {
            modules,
            functions: functions.into_iter().collect(),
            files: files.into_iter().collect(),
            globals: globals.into_iter().collect(),
        }
    }

    /// The names of the members of the value `base` names, or of what it
    /// points to; none when it names no aggregate.
    pub async fn member_names(&self, base: &str) -> Vec<String> {
        let Ok(expression) = microscope::Expression::parse(base) else {
            return Vec::new();
        };

        let Ok(microscope::Evaluation::Value { value, .. }) =
            self.debugger.evaluate(&expression).await
        else {
            return Vec::new();
        };

        let mut state = value.state;

        if let microscope::VariableState::Available {
            children: microscope::ValueChildren::NotApplicable,
            dereference: microscope::DereferenceState::Available(reference),
            ..
        } = &state
            && let Ok(pointee) = self.debugger.dereference(reference.clone()).await
        {
            state = pointee.state;
        }

        let microscope::VariableState::Available {
            children: microscope::ValueChildren::Available(children),
            ..
        } = state
        else {
            return Vec::new();
        };

        let Ok(page) = self
            .debugger
            .value_children(
                children,
                microscope::ValueChildQuery {
                    offset: 0,
                    limit: 256,
                },
            )
            .await
        else {
            return Vec::new();
        };

        page.children
            .iter()
            .filter_map(|child| match &child.relationship {
                microscope::ValueChildRelationship::Member(member) if !member.artificial => {
                    member.name.as_deref().map(str::to_owned)
                }
                microscope::ValueChildRelationship::Field { name } => Some(name.to_string()),
                _ => None,
            })
            .collect()
    }
}

const BREAK_OPTIONS: &[&str] = &["if", "hits", "log", "disabled"];
const SIGNAL_ACTIONS: &[&str] = &["stop", "nostop", "print", "noprint", "pass", "nopass"];

/// What `catch`'s argument at `argument` may be: an exception filter, then
/// whether it stops.
fn exception_words(argument: usize, first: Option<&str>) -> Vec<String> {
    if argument == 0 {
        let mut words: Vec<String> = microscope::ExceptionStops::filters()
            .iter()
            .map(|filter| filter.id.to_owned())
            .collect();
        words.push("syscall".to_owned());
        words
    } else if first == Some("syscall") {
        let mut words = words_of(&["entry", "exit", "if"]);
        words.extend(
            microscope::syscalls::table::calls(microscope::syscalls::Abi::X86_64)
                .iter()
                .map(|&(_, name)| name.to_owned()),
        );
        words
    } else {
        words_of(&["on", "off"])
    }
}

/// The settings `set` turns on or off.
const TOGGLES: [&str; 3] = ["views", "step-runtime", "runtime-frames"];

/// Where the word at `position` in `line` starts, and the words that
/// could replace it. `fields` gives the members of the value an expression
/// names, or of what it points to.
pub fn complete(
    line: &str,
    position: usize,
    context: &Context,
    fields: &mut dyn FnMut(&str) -> Vec<String>,
) -> (usize, Vec<String>) {
    let before = &line[..position];
    let start = before
        .rfind(char::is_whitespace)
        .map_or(0, |index| index + 1);
    let word = &before[start..];
    let words = before[..start].split_whitespace().collect::<Vec<_>>();
    let Some(first) = words.first() else {
        // A command's own aliases abbreviate it, so only its name is offered.
        let names = COMMANDS
            .iter()
            .map(|spec| spec.name.to_owned())
            .chain(context.aliases.iter().cloned());
        return (start, matching(word, names));
    };

    let Ok(spec) = resolve_command(first.split_once('/').map_or(first, |(name, _)| name)) else {
        return (start, Vec::new());
    };

    let argument = words.len() - 1;
    let ids = |all: bool, watches: bool| {
        let ids = if watches {
            &context.watchpoints
        } else {
            &context.breakpoints
        };

        ids.iter()
            .cloned()
            .chain((all && argument == 0).then(|| "all".to_owned()))
            .collect::<Vec<_>>()
    };

    let candidates = match spec.command {
        command if argument == 0 && takes_location(command) => {
            return location(word, start, context);
        }
        Command::Break | Command::Tbreak => {
            return expression(before, context, fields, BREAK_OPTIONS);
        }
        Command::Address => context.functions.to_vec(),
        Command::Delete | Command::Enable | Command::Disable => {
            let mut ids = ids(true, false);
            ids.extend(context.watchpoints.iter().cloned());
            ids
        }
        Command::Ignore | Command::Hits | Command::Condition if argument == 0 => {
            let mut ids = ids(false, false);
            ids.extend(context.watchpoints.iter().cloned());
            ids
        }
        Command::Unwatch => ids(true, true),
        Command::Undisplay => context
            .displays
            .iter()
            .cloned()
            .chain((argument == 0).then(|| "all".to_owned()))
            .collect(),
        Command::Info if argument == 0 => words_of(&INFO_SUBCOMMANDS),
        Command::Info if words.get(1) == Some(&"symbol") => context.functions.to_vec(),
        Command::Info if words.get(1) == Some(&"view") => {
            return expression(before, context, fields, &[]);
        }
        Command::Save if argument == 0 => words_of(&["breakpoints"]),
        Command::Views if argument == 0 => {
            words_of(&["load", "clear", "check", "explain", "record"])
        }
        Command::Handle if argument == 0 => microscope::signal_codes()
            .filter_map(microscope::signal_name)
            .collect(),
        Command::Handle => words_of(SIGNAL_ACTIONS),
        Command::Catch => exception_words(argument, words.get(1).copied()),
        Command::Help if argument == 0 => {
            COMMANDS.iter().map(|spec| spec.name.to_owned()).collect()
        }
        Command::Set if argument == 0 && !word.contains(['=', '.']) => {
            let mut candidates = words_of(&["var", "views", "step-runtime", "runtime-frames"]);
            candidates.extend(context.names.iter().cloned());
            candidates
        }
        Command::Set if TOGGLES.contains(words.get(1).unwrap_or(&"")) => words_of(&["on", "off"]),
        Command::Condition | Command::Ignore | Command::Hits => {
            return expression(before, context, fields, &[]);
        }

        Command::Print
        | Command::Pp
        | Command::Display
        | Command::Whatis
        | Command::Ptype
        | Command::Set
        | Command::Watch
        | Command::AccessWatch
        | Command::ReadWatch => {
            let options: &[&str] = match spec.command {
                Command::Watch | Command::AccessWatch | Command::ReadWatch if argument != 0 => {
                    &["if"]
                }
                _ => &[],
            };

            return expression(before, context, fields, options);
        }
        _ => Vec::new(),
    };

    (start, matching(word, candidates))
}

/// Whether a command's first argument is a location.
const fn takes_location(command: Command) -> bool {
    matches!(
        command,
        Command::Break | Command::Tbreak | Command::Advance | Command::Jump | Command::Disassemble
    )
}

fn words_of(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

/// The candidates that begin with `word`, sorted, once each.
fn matching(word: &str, candidates: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut matching = candidates
        .into_iter()
        .filter(|candidate| candidate.starts_with(word))
        .collect::<Vec<_>>();
    matching.sort();
    matching.dedup();
    matching
}

/// Completes a location: a function, a file to follow with a line or
/// function, or a function within a file.
fn location(word: &str, start: usize, context: &Context) -> (usize, Vec<String>) {
    if let Some((file, function)) = word.split_once(':') {
        let start = start + file.len() + 1;
        return (start, matching(function, context.functions.iter().cloned()));
    }

    let candidates = context
        .functions
        .iter()
        .cloned()
        .chain(context.files.iter().map(|file| format!("{file}:")));

    (start, matching(word, candidates))
}

/// Completes the name that ends `line`, an expression: a member after
/// `.` or `->`, or a variable or one of `keywords`.
fn expression(
    line: &str,
    context: &Context,
    fields: &mut dyn FnMut(&str) -> Vec<String>,
    keywords: &[&str],
) -> (usize, Vec<String>) {
    let (completing, partial, start) = crate::present::complete::completing(line);

    let candidates = match completing {
        Completing::Member { base } => fields(base),
        Completing::Name { .. } => context
            .names
            .iter()
            .chain(context.globals.iter())
            .cloned()
            .chain(keywords.iter().map(|keyword| (*keyword).to_owned()))
            .collect(),
        _ => Vec::new(),
    };

    (start, matching(partial, candidates))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{Context, complete};

    fn names(words: &[&str]) -> Arc<[String]> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn candidates_come_from_the_context_the_line_is_in() {
        let context = Context {
            aliases: names(&["bb"]),
            functions: names(&["caller", "counted", "main"]),
            files: names(&["hit-counts.c", "main.c"]),
            breakpoints: names(&["1", "2"]),
            watchpoints: names(&["w1"]),
            displays: names(&["1"]),
            names: names(&["call", "record", "records"]),
            globals: names(&["global_record"]),
        };

        let mut asked = Vec::new();
        let mut fields = |base: &str| {
            asked.push(base.to_owned());
            vec!["inner".to_owned(), "values".to_owned()]
        };

        let mut at_end = |line: &str| complete(line, line.len(), &context, &mut fields);

        assert_eq!(
            at_end("dis"),
            (0, names(&["disable", "disassemble", "display"]).to_vec())
        );
        assert_eq!(
            at_end("b"),
            (
                0,
                names(&["backtrace", "bb", "break", "breakpoints"]).to_vec()
            )
        );
        assert_eq!(
            at_end("break c"),
            (6, names(&["caller", "counted"]).to_vec())
        );
        assert_eq!(
            at_end("tbreak ma"),
            (7, names(&["main", "main.c:"]).to_vec())
        );
        assert_eq!(
            at_end("break hit-counts.c:co"),
            (19, names(&["counted"]).to_vec())
        );
        assert_eq!(at_end("break main if ca"), (14, names(&["call"]).to_vec()));
        assert_eq!(at_end("break main h"), (11, names(&["hits"]).to_vec()));
        assert_eq!(
            at_end("delete "),
            (7, names(&["1", "2", "all", "w1"]).to_vec())
        );
        assert_eq!(at_end("unwatch "), (8, names(&["all", "w1"]).to_vec()));
        assert_eq!(at_end("info b"), (5, names(&["breakpoints"]).to_vec()));
        assert_eq!(at_end("p rec"), (2, names(&["record", "records"]).to_vec()));
        assert_eq!(
            at_end("p/x 1 + rec"),
            (8, names(&["record", "records"]).to_vec())
        );
        assert_eq!(at_end("p record.in"), (9, names(&["inner"]).to_vec()));
        assert_eq!(
            at_end("pp records[1 + call]->"),
            (22, names(&["inner", "values"]).to_vec())
        );
        assert_eq!(
            at_end("p 0..rec"),
            (5, names(&["record", "records"]).to_vec())
        );
        assert_eq!(
            at_end("handle SIGUSR1 no"),
            (15, names(&["nopass", "noprint", "nostop"]).to_vec())
        );
        assert_eq!(asked, ["record", "records[1 + call]"]);
    }
}
