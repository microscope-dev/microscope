//! Launch and attach configurations.
//!
//! Unknown keys, such as the `name`, `type`, and `__sessionId` that clients
//! add, are ignored. Invalid values are reported with the path of the key
//! that holds them.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

use microscope::{
    AssemblySyntax, CoreDumpOptions, DebugFileOptions, ProcessId, SignalPolicy, SourcePathMap,
};
use serde::Deserialize;
use serde::de::{self, DeserializeOwned, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

/// How the session obtains its target.
#[derive(Debug)]
pub enum Start {
    Launch(Launch),
    Attach {
        process: ProcessId,
        executable: Option<PathBuf>,

        /// The group member this session shows, which the adapter that
        /// started the session hosts.
        member: Option<MemberRoute>,
    },
    Core(CoreDumpOptions),
}

/// Where a child session for a group's member connects, and which member
/// it shows: what the adapter hosting the group puts in the configuration
/// it asks the client to start.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemberRoute {
    /// The socket of the adapter that hosts the group.
    pub socket: PathBuf,
    pub number: microscope::MemberNumber,
}

/// The key of a configuration that names a [`MemberRoute`]; the editors
/// route a session whose configuration has it to the socket it names.
pub const MEMBER_KEY: &str = "__microscopeMember";

/// A program to launch.
#[derive(Debug, Clone)]
pub struct Launch {
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
    pub environment: Vec<(OsString, Option<OsString>)>,
    pub working_directory: Option<PathBuf>,
    pub console: Console,
}

/// Where a launched program's standard streams go.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum Console {
    /// The debug console, through the adapter.
    #[default]
    #[serde(rename = "internalConsole")]
    Internal,

    /// A terminal in the client's window.
    #[serde(rename = "integratedTerminal")]
    Integrated,

    /// A terminal in its own window.
    #[serde(rename = "externalTerminal")]
    External,
}

/// A validated launch or attach configuration.
#[derive(Debug)]
pub struct Configuration {
    pub start: Start,
    pub stop_on_entry: bool,
    pub source_paths: SourcePathMap,
    pub syntax: AssemblySyntax,

    /// Signal handling applied over the exception filters: each signal's
    /// actions, as the console's `handle` command takes them.
    pub signals: Vec<(u64, Vec<String>)>,

    /// View files to present values with, ahead of the project's and the
    /// user's, and where the project's are: in `.microscope/views` under it.
    pub view_files: Vec<PathBuf>,
    pub working_directory: Option<PathBuf>,

    /// Which processes the program starts are debugged too, each in a
    /// child session.
    pub follow: microscope::FollowPolicy,

    /// The settings a child session's configuration carries over.
    pub inherited: Map<String, Value>,
    pub threads: ThreadListing,

    /// Where modules' separate debug files are found.
    pub debug_files: DebugFileOptions,

    /// Whether steps stop in a language runtime's own code.
    pub step_into_runtime: bool,

    /// The source files steps pass through, as `skipFiles` names them.
    pub step_skips: microscope::StepSkips,
}

/// What the client's threads are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadListing {
    /// Whether a program with a runtime's tasks, such as Go's goroutines,
    /// shows them as its threads, rather than its system threads.
    pub tasks: bool,

    /// Whether the tasks a runtime runs for its own work are listed.
    pub runtime_tasks: bool,

    /// The most tasks listed; an entry after them says how many more
    /// there are.
    pub max_tasks: usize,
}

impl Default for ThreadListing {
    fn default() -> Self {
        Self {
            tasks: true,
            runtime_tasks: false,
            max_tasks: 1000,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Threads {
    Tasks,
    System,
}

/// The keys of a configuration that a child session's carries over: how
/// the session presents and handles a program, which a fork does not
/// change, and the adapter's `type`, by which a client such as nvim-dap
/// finds the adapter to start for the child.
const INHERITED: [&str; 13] = [
    "type",
    "debugDirectories",
    "debuginfod",
    "sourceMap",
    "viewFiles",
    "disassemblySyntax",
    "signals",
    "threads",
    "runtimeTasks",
    "maxTasks",
    "stepIntoRuntime",
    "skipFiles",
    "cwd",
];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[expect(clippy::struct_excessive_bools, reason = "each is a launch option")]
struct Arguments {
    program: Option<PathBuf>,

    #[serde(default)]
    args: Vec<String>,
    cwd: Option<PathBuf>,

    #[serde(default)]
    env: BTreeMap<String, Option<String>>,

    #[serde(default)]
    stop_on_entry: bool,

    #[serde(default, deserialize_with = "source_map")]
    source_map: Vec<[String; 2]>,
    disassembly_syntax: Option<Syntax>,

    #[serde(default)]
    signals: BTreeMap<String, Actions>,

    #[serde(default)]
    console: Console,
    pid: Option<Pid>,
    core_file: Option<PathBuf>,
    sysroot: Option<PathBuf>,

    #[serde(default)]
    module_paths: Vec<PathBuf>,

    #[serde(default)]
    allow_module_mismatch: bool,

    #[serde(default)]
    view_files: Vec<PathBuf>,

    #[serde(default)]
    debug_directories: Vec<PathBuf>,

    #[serde(default)]
    debuginfod: bool,

    follow: Option<FollowSetting>,
    follow_forks: Option<bool>,

    #[serde(rename = "__microscopeMember")]
    member: Option<MemberRoute>,
    threads: Option<Threads>,

    #[serde(default)]
    runtime_tasks: bool,
    max_tasks: Option<u32>,

    #[serde(default)]
    step_into_runtime: bool,

    #[serde(default)]
    skip_files: Vec<String>,
}

/// Which processes the program starts are debugged: a mode, or a table as
/// the settings file's `[follow]`.
#[derive(Deserialize)]
#[serde(untagged)]
enum FollowSetting {
    Mode(crate::cli::config::FollowMode),
    Table(crate::cli::config::Follow),
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Syntax {
    Intel,
    Att,
}

/// A process id: VS Code's `${command:pickProcess}` produces a string.
#[derive(Deserialize)]
#[serde(untagged)]
enum Pid {
    Number(u64),
    Text(String),
}

/// One signal action or several.
#[derive(Deserialize)]
#[serde(untagged)]
enum Actions {
    One(String),
    Many(Vec<String>),
}

/// Parses a `launch` request's configuration.
pub fn launch(arguments: Value) -> Result<Configuration, String> {
    let inherited = inherited(&arguments, false);
    let parsed = parse::<Arguments>(arguments, "launch configuration")?;
    if parsed.pid.is_some() || parsed.core_file.is_some() || parsed.member.is_some() {
        return Err(format!(
            "invalid launch configuration: `pid`, `{MEMBER_KEY}`, and `coreFile` belong to \
             attach configurations"
        ));
    }

    let program = parsed
        .program
        .clone()
        .ok_or_else(|| "invalid launch configuration: missing field `program`".to_owned())?;
    let launch = Launch {
        program,
        arguments: parsed.args.iter().map(OsString::from).collect(),
        environment: parsed
            .env
            .iter()
            .map(|(name, value)| (name.into(), value.as_ref().map(OsString::from)))
            .collect(),
        working_directory: parsed.cwd.clone(),
        console: parsed.console,
    };

    common(
        parsed,
        Start::Launch(launch),
        inherited,
        "launch configuration",
    )
}

/// Parses an `attach` request's configuration: a process or a core dump.
pub fn attach(arguments: Value) -> Result<Configuration, String> {
    let inherited = inherited(&arguments, true);
    let parsed = parse::<Arguments>(arguments, "attach configuration")?;

    let start = match (&parsed.pid, &parsed.core_file) {
        (Some(_), Some(_)) => {
            return Err(
                "invalid attach configuration: give either `pid` or `coreFile`, not both"
                    .to_owned(),
            );
        }
        (None, None) => {
            return Err("invalid attach configuration: give `pid` or `coreFile`".to_owned());
        }
        (Some(pid), None) => {
            let number = match pid {
                Pid::Number(number) => Some(*number),
                Pid::Text(text) => text.trim().parse().ok(),
            }
            .filter(|number| *number != 0)
            .ok_or_else(|| {
                "invalid attach configuration at pid: expected a positive process id".to_owned()
            })?;
            Start::Attach {
                process: ProcessId::new(number),
                executable: parsed.program.clone(),
                member: parsed.member.clone(),
            }
        }
        (None, Some(_)) if parsed.member.is_some() => {
            return Err(format!(
                "invalid attach configuration: `{MEMBER_KEY}` needs `pid`"
            ));
        }
        (None, Some(core)) => Start::Core(CoreDumpOptions {
            core: core.clone(),
            executable: parsed.program.clone(),
            sysroot: parsed.sysroot.clone(),
            module_paths: parsed.module_paths.clone(),
            allow_module_mismatch: parsed.allow_module_mismatch,
            debug_files: debug_files(&parsed),
        }),
    };

    common(parsed, start, inherited, "attach configuration")
}

/// Where a configuration's separate debug files are found.
fn debug_files(parsed: &Arguments) -> DebugFileOptions {
    DebugFileOptions {
        directories: parsed.debug_directories.clone(),
        debuginfod: parsed.debuginfod,
        ..DebugFileOptions::default()
    }
}

/// The settings of `arguments` a child session carries over. An attach
/// configuration's `program` names the executable of its children too.
fn inherited(arguments: &Value, attach: bool) -> Map<String, Value> {
    let program = attach.then_some("program");
    INHERITED
        .into_iter()
        .chain(program)
        .filter_map(|key| Some((key.to_owned(), arguments.get(key)?.clone())))
        .collect()
}

fn common(
    parsed: Arguments,
    start: Start,
    inherited: Map<String, Value>,
    what: &str,
) -> Result<Configuration, String> {
    let debug_files = debug_files(&parsed);
    let mut source_paths = SourcePathMap::new();

    for [from, to] in parsed.source_map {
        source_paths
            .push(&from, &to)
            .map_err(|error| format!("invalid {what} at sourceMap: {error}"))?;
    }

    let mut signals = Vec::new();

    for (name, actions) in parsed.signals {
        let code = microscope::signal_named(&name)
            .ok_or_else(|| format!("invalid {what} at signals.{name}: unknown signal"))?;
        let actions = match actions {
            Actions::One(action) => vec![action],
            Actions::Many(actions) => actions,
        };

        let mut policy = SignalPolicy {
            stop: false,
            print: false,
            pass: false,
        };

        for action in &actions {
            crate::cli::commands::apply_signal_action(&mut policy, action)
                .map_err(|error| format!("invalid {what} at signals.{name}: {error}"))?;
        }

        signals.push((code, actions));
    }

    let follow = match (parsed.follow, parsed.follow_forks) {
        (Some(_), Some(_)) => {
            return Err(format!(
                "invalid {what}: give `follow` or `followForks`, not both"
            ));
        }
        (Some(FollowSetting::Mode(mode)), None) => crate::cli::config::Follow {
            mode,
            ..crate::cli::config::Follow::default()
        },
        (Some(FollowSetting::Table(follow)), None) => follow,
        // `followForks` is what `follow` was before processes were
        // followed into the programs they execute.
        (None, Some(forks)) => crate::cli::config::Follow {
            mode: if forks {
                crate::cli::config::FollowMode::All
            } else {
                crate::cli::config::FollowMode::None
            },
            ..crate::cli::config::Follow::default()
        },
        (None, None) => crate::cli::config::Follow::default(),
    }
    .policy(&[]);

    let max_tasks = match parsed.max_tasks {
        Some(0) => return Err(format!("invalid {what} at maxTasks: expected at least 1")),
        Some(max) => usize::try_from(max).unwrap_or(usize::MAX),
        None => ThreadListing::default().max_tasks,
    };

    Ok(Configuration {
        debug_files,
        threads: ThreadListing {
            tasks: !matches!(parsed.threads, Some(Threads::System)),
            runtime_tasks: parsed.runtime_tasks,
            max_tasks,
        },
        start,
        stop_on_entry: parsed.stop_on_entry,
        view_files: parsed.view_files,
        working_directory: parsed.cwd,
        follow,
        step_into_runtime: parsed.step_into_runtime,
        step_skips: microscope::StepSkips::parse(&parsed.skip_files)
            .map_err(|error| format!("skipFiles: {error}"))?,
        inherited,
        source_paths,
        syntax: match parsed.disassembly_syntax {
            None | Some(Syntax::Intel) => AssemblySyntax::Intel,
            Some(Syntax::Att) => AssemblySyntax::Att,
        },
        signals,
    })
}

/// Deserializes request arguments, naming the path of an invalid value.
pub fn parse<T: DeserializeOwned>(arguments: Value, what: &str) -> Result<T, String> {
    serde_path_to_error::deserialize(arguments).map_err(|error| {
        let path = error.path().to_string();
        if path == "." {
            format!("invalid {what}: {}", error.inner())
        } else {
            format!("invalid {what} at {path}: {}", error.inner())
        }
    })
}

/// Reads source path rules as `[["from", "to"], …]` or `{"from": "to", …}`,
/// keeping their order: earlier rules are tried first.
fn source_map<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<[String; 2]>, D::Error> {
    struct Rules;

    impl<'de> Visitor<'de> for Rules {
        type Value = Vec<[String; 2]>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a list of [from, to] pairs or an object mapping from to to")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
            let mut rules = Vec::new();
            while let Some(rule) = sequence.next_element::<[String; 2]>()? {
                rules.push(rule);
            }

            Ok(rules)
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut rules = Vec::new();
            while let Some(from) = map.next_key::<String>()? {
                let to = map.next_value::<String>()?;
                rules.push([from, to]);
            }

            Ok(rules)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }
    }

    deserializer.deserialize_any(Rules)
}

#[cfg(test)]
mod tests {
    use super::*;
    use microscope::{FollowMode, StopScope};
    use serde_json::json;

    #[test]
    fn launch_configurations_read_every_field_and_keep_rule_order() {
        let configuration = launch(json!({
            "type": "microscope", "request": "launch", "name": "x", "__sessionId": "y",
            "program": "/bin/true", "args": ["a", "b c"], "cwd": "/tmp",
            "env": {"KEEP": "1", "DROP": null},
            "stopOnEntry": true,
            "sourceMap": {"/z": "/one", "/a": "/two"},
            "disassemblySyntax": "att",
            "signals": {"SIGUSR1": "nostop", "alrm": ["stop", "nopass"]},
            "console": "internalConsole",
            "threads": "system", "runtimeTasks": true, "maxTasks": 5,
            "stepIntoRuntime": true,
        }))
        .expect("valid configuration");
        let Start::Launch(launch) = &configuration.start else {
            panic!("not a launch");
        };

        assert_eq!(launch.program, PathBuf::from("/bin/true"));
        assert_eq!(launch.arguments, ["a", "b c"]);
        assert_eq!(
            launch.environment,
            [("DROP".into(), None), ("KEEP".into(), Some("1".into()))]
        );
        assert!(configuration.stop_on_entry);
        assert_eq!(configuration.syntax, AssemblySyntax::Att);

        // Object rules keep their written order, not their sorted order.
        assert_eq!(
            configuration
                .source_paths
                .candidates(std::path::Path::new("/z/f.c")),
            [PathBuf::from("/one/f.c"), PathBuf::from("/z/f.c")]
        );
        assert_eq!(configuration.signals.len(), 2);
        assert_eq!(
            configuration.threads,
            ThreadListing {
                tasks: false,
                runtime_tasks: true,
                max_tasks: 5,
            }
        );
        assert!(configuration.step_into_runtime);
    }

    #[test]
    fn invalid_configurations_name_the_offending_key() {
        for (arguments, expected) in [
            (
                json!({}),
                "invalid launch configuration: missing field `program`",
            ),
            (
                json!({"program": "p", "env": {"PATH": 1}}),
                "invalid launch configuration at env.PATH: invalid type: integer `1`, expected a string",
            ),
            (
                json!({"program": "p", "args": "a b"}),
                "invalid launch configuration at args: invalid type: string \"a b\", expected a sequence",
            ),
            (
                json!({"program": "p", "signals": {"SIGNOPE": "stop"}}),
                "invalid launch configuration at signals.SIGNOPE: unknown signal",
            ),
            (
                json!({"program": "p", "signals": {"SIGUSR1": "halt"}}),
                "invalid launch configuration at signals.SIGUSR1: unknown signal action 'halt'; use stop, nostop, print, noprint, pass, or nopass",
            ),
            (
                json!({"program": "p", "console": "pty"}),
                "invalid launch configuration at console: unknown variant `pty`, expected one of `internalConsole`, `integratedTerminal`, `externalTerminal`",
            ),
            (
                json!({"program": "p", "sourceMap": [["/a"]]}),
                "invalid launch configuration at sourceMap[0]: invalid length 1, expected an array of length 2",
            ),
            (
                json!({"program": "p", "maxTasks": 0}),
                "invalid launch configuration at maxTasks: expected at least 1",
            ),
            (
                json!({"program": "p", "threads": "fibers"}),
                "invalid launch configuration at threads: unknown variant `fibers`, expected `tasks` or `system`",
            ),
        ] {
            assert_eq!(launch(arguments).map(|_| ()), Err(expected.to_owned()));
        }

        for (arguments, expected) in [
            (
                json!({}),
                "invalid attach configuration: give `pid` or `coreFile`",
            ),
            (
                json!({"pid": 1, "coreFile": "c"}),
                "invalid attach configuration: give either `pid` or `coreFile`, not both",
            ),
            (
                json!({"pid": "abc"}),
                "invalid attach configuration at pid: expected a positive process id",
            ),
        ] {
            assert_eq!(attach(arguments).map(|_| ()), Err(expected.to_owned()));
        }

        assert_eq!(
            attach(json!({"coreFile": "c", "__microscopeMember": {"socket": "/s", "number": 2}}))
                .map(|_| ()),
            Err("invalid attach configuration: `__microscopeMember` needs `pid`".to_owned())
        );
        let configuration = attach(json!({"pid": "42"})).expect("string pid");
        assert!(matches!(
            configuration.start,
            Start::Attach { process, executable: None, member: None } if process == ProcessId::new(42)
        ));
    }

    #[test]
    fn a_child_session_carries_over_what_presents_and_handles_the_program() {
        let shared = json!({
            "type": "microscope", "sourceMap": [["/a", "/b"]], "viewFiles": ["v.toml"],
            "disassemblySyntax": "att", "signals": {"SIGUSR1": "nostop"}, "cwd": "/w",
        });
        let mut parent = shared.clone();
        parent["program"] = json!("/bin/p");
        parent["stopOnEntry"] = json!(true);
        parent["args"] = json!(["x"]);
        parent["follow"] = json!("all");
        let launched = launch(parent.clone()).expect("launch");
        assert_eq!(launched.follow.mode, microscope::FollowMode::All);
        parent.as_object_mut().expect("object").remove("follow");
        assert_eq!(Value::Object(launched.inherited), shared);

        parent["pid"] = json!(7);
        parent.as_object_mut().expect("object").remove("args");
        let attached = attach(parent).expect("attach");
        let mut expected = shared;
        expected["program"] = json!("/bin/p");
        assert_eq!(Value::Object(attached.inherited.clone()), expected);

        let mut child = Value::Object(attached.inherited);
        child["pid"] = json!(8);
        child[MEMBER_KEY] = json!({"socket": "/run/s", "number": 3});
        let child = attach(child).expect("a child's configuration is valid");
        assert!(matches!(
            child.start,
            Start::Attach { process, member: Some(MemberRoute { number: 3, .. }), .. }
                if process == ProcessId::new(8)
        ));
    }

    /// `follow` takes a mode or a table as the settings file's; the older
    /// `followForks` maps onto it; and following is the default, as in the
    /// command line.
    #[test]
    fn follow_takes_a_mode_a_table_or_the_older_follow_forks() {
        let follow = |extra: Value| {
            let mut arguments = json!({"program": "/bin/p"});
            for (key, value) in extra.as_object().expect("object") {
                arguments[key] = value.clone();
            }
            launch(arguments).map(|configuration| configuration.follow)
        };
        let policy = |mode, programs: &[&str], stop| microscope::FollowPolicy {
            mode,
            programs: programs.iter().map(|&program| program.to_owned()).collect(),
            stop,
        };
        assert_eq!(
            follow(json!({})),
            Ok(policy(FollowMode::Auto, &[], StopScope::Group))
        );
        assert_eq!(
            follow(json!({"follow": "none"})),
            Ok(policy(FollowMode::None, &[], StopScope::Group))
        );
        assert_eq!(
            follow(json!({"follow": {"programs": ["worker*"], "stop": "process"}})),
            Ok(policy(FollowMode::Auto, &["worker*"], StopScope::Process))
        );
        assert_eq!(
            follow(json!({"followForks": true})),
            Ok(policy(FollowMode::All, &[], StopScope::Group))
        );
        assert_eq!(
            follow(json!({"followForks": false})),
            Ok(policy(FollowMode::None, &[], StopScope::Group))
        );
        assert_eq!(
            follow(json!({"follow": "all", "followForks": true})),
            Err(
                "invalid launch configuration: give `follow` or `followForks`, not both".to_owned()
            )
        );
        assert!(follow(json!({"follow": "some"})).is_err());
    }

    /// The VS Code extension's manifest documents the configurations.
    fn manifest() -> Value {
        serde_json::from_str(include_str!("../../editors/vscode/package.json")).expect("manifest")
    }

    #[test]
    fn every_key_the_vscode_extension_documents_is_read() {
        let debugger = &manifest()["contributes"]["debuggers"][0];
        for (request, base) in [
            ("launch", json!({"program": "p"})),
            ("attach", json!({"pid": 1})),
        ] {
            let properties = debugger["configurationAttributes"][request]["properties"]
                .as_object()
                .expect("properties");
            for (key, schema) in properties {
                // A value of the wrong type must be refused at its key.
                let wrong = if schema["type"] == "boolean" {
                    json!("yes")
                } else {
                    json!(true)
                };

                let mut arguments = if matches!(key.as_str(), "pid" | "coreFile") {
                    json!({})
                } else {
                    base.clone()
                };

                arguments[key] = wrong;
                let error = match request {
                    "launch" => launch(arguments),
                    _ => attach(arguments),
                }
                .expect_err(key);
                assert!(error.contains(&format!(" at {key}")), "{key}: {error}");
            }
        }
    }

    #[test]
    fn the_vscode_extension_configurations_are_valid() {
        let debugger = &manifest()["contributes"]["debuggers"][0];
        let snippets = debugger["configurationSnippets"]
            .as_array()
            .expect("snippets")
            .iter()
            .map(|snippet| snippet["body"].clone());
        let initial = debugger["initialConfigurations"]
            .as_array()
            .expect("initial configurations")
            .clone();
        for configuration in initial.into_iter().chain(snippets) {
            let configuration = expand(configuration);
            let parsed = match configuration["request"].as_str() {
                Some("launch") => launch(configuration.clone()),
                _ => attach(configuration.clone()),
            };

            assert!(parsed.is_ok(), "{configuration}: {parsed:?}");
        }
    }

    /// Expands what VS Code would in a configuration's strings: a snippet's
    /// quoting, escapes, and placeholders, and the variables it substitutes.
    fn expand(value: Value) -> Value {
        match value {
            Value::String(text) => {
                let text = text
                    .strip_prefix("^\"")
                    .and_then(|text| text.strip_suffix('"'))
                    .unwrap_or(&text)
                    .replace("\\$", "$");
                let text = with_defaults(&text)
                    .replace("${workspaceFolder}", "/workspace")
                    .replace("${file}", "/workspace/main.py")
                    .replace("${command:pickProcess}", "1234");
                assert!(!text.contains('$'), "unexpanded {text}");
                Value::String(text)
            }
            Value::Array(values) => Value::Array(values.into_iter().map(expand).collect()),
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(key, value)| (key, expand(value)))
                    .collect(),
            ),
            other => other,
        }
    }

    /// Replaces each snippet placeholder, `${1:text}`, with its text.
    fn with_defaults(text: &str) -> String {
        let mut filled = String::new();
        let mut rest = text;

        while let Some(start) = rest.find("${") {
            filled.push_str(&rest[..start]);
            let inner = &rest[start + 2..];
            let placeholder = inner
                .split_once(':')
                .filter(|(index, _)| index.bytes().all(|byte| byte.is_ascii_digit()));
            if let Some((default, after)) = placeholder.and_then(|(_, text)| text.split_once('}')) {
                filled.push_str(default);
                rest = after;
            } else {
                filled.push_str("${");
                rest = inner;
            }
        }

        filled.push_str(rest);
        filled
    }
}
