//! Finding processes by name, for attach.

use std::path::Path;

use crate::{NameMatch, NamedProcess, ProcessId, Result};

/// The processes `name` names, by what [`name_match`] matches. This
/// process is never one of them.
pub fn processes_named(name: &str) -> Result<Vec<NamedProcess>> {
    let own = std::process::id();
    let mut found = Vec::new();
    let entries = std::fs::read_dir("/proc")?;
    for entry in entries.filter_map(std::result::Result::ok) {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };

        if pid == own {
            continue;
        }

        // A process that ended meanwhile matches nothing.
        let command = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
        let command_line = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
        let arguments = arguments(&command_line);

        if let Some(matched) = name_match(name, command.trim_end_matches('\n'), &arguments) {
            found.push(NamedProcess {
                process_id: ProcessId::new(u64::from(pid)),
                command_line: arguments.join(" ").into(),
                matched,
            });
        }
    }

    found.sort_unstable_by_key(|process| process.process_id.get());
    Ok(found)
}

/// The arguments of a `/proc/PID/cmdline`, without the empty ones a
/// program that rewrote its title leaves behind.
fn arguments(command_line: &[u8]) -> Vec<String> {
    command_line
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(|argument| String::from_utf8_lossy(argument).into_owned())
        .collect()
}

/// How `name` names a process whose kernel command name is `command` and
/// whose arguments are `arguments`:
///
/// - its command name, as `pgrep -x` matches it;
/// - the file name of its first argument, which outlasts the kernel's
///   fifteen-character command name;
/// - a title the program set over its arguments, such as npm's `npm start`,
///   by the whole title or its first word;
/// - the file name of a later argument that is no option, such as the
///   script `node server.js` runs.
///
/// The first of these that matches is the reason given.
fn name_match(name: &str, command: &str, arguments: &[String]) -> Option<NameMatch> {
    if name.is_empty() {
        return None;
    }

    if command == name {
        return Some(NameMatch::Command);
    }

    let (first, rest) = arguments.split_first()?;

    // A title replaces the arguments with one string of words.
    if first.contains(char::is_whitespace) {
        let first_word = first.split_whitespace().next().map(file_name);
        return (first == name || first_word.as_deref() == Some(name)).then_some(NameMatch::Title);
    }

    if file_name(first) == name {
        return Some(NameMatch::Program);
    }

    rest.iter()
        .filter(|argument| !argument.starts_with('-'))
        .any(|argument| file_name(argument) == name)
        .then_some(NameMatch::Script)
}

/// The last component of a path, or the whole text when it has none.
fn file_name(path: &str) -> String {
    Path::new(path).file_name().map_or_else(
        || path.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &[&str]) -> Vec<String> {
        line.iter().map(|&argument| argument.to_owned()).collect()
    }

    #[test]
    fn names_match_by_command_program_title_and_script() {
        let node = args(&["/usr/bin/node", "--enable-source-maps", "src/server.js"]);
        for (name, command, arguments, expected) in [
            // The kernel's name wins, even where an argument matches too.
            ("node", "node", &node, Some(NameMatch::Command)),
            // Node 24 renames its main thread, so only the program matches.
            ("node", "MainThread", &node, Some(NameMatch::Program)),
            ("server.js", "MainThread", &node, Some(NameMatch::Script)),
            // An option's value is not a script.
            ("--enable-source-maps", "MainThread", &node, None),
            ("server", "MainThread", &node, None),
            ("src", "MainThread", &node, None),
            (
                "npm",
                "npm start",
                &args(&["npm start"]),
                Some(NameMatch::Title),
            ),
            (
                "npm start",
                "npm start",
                &args(&["npm start"]),
                Some(NameMatch::Command),
            ),
            ("start", "npm start", &args(&["npm start"]), None),
            ("node", "bash", &args(&[]), None),
            ("", "", &args(&["x"]), None),
        ] {
            assert_eq!(
                name_match(name, command, arguments),
                expected,
                "{name:?} against {command:?} {arguments:?}"
            );
        }
    }

    #[test]
    fn a_rewritten_title_leaves_no_empty_arguments() {
        assert_eq!(arguments(b"npm start\0\0\0\0"), args(&["npm start"]));
        assert_eq!(arguments(b"node\0app.js\0"), args(&["node", "app.js"]));
    }
}
