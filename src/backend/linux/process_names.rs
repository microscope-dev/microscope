use std::path::Path;

use crate::{NameMatch, NamedProcess, ProcessId, Result};

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

fn arguments(command_line: &[u8]) -> Vec<String> {
    command_line
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(|argument| String::from_utf8_lossy(argument).into_owned())
        .collect()
}

fn name_match(name: &str, command: &str, arguments: &[String]) -> Option<NameMatch> {
    if name.is_empty() {
        return None;
    }

    if command == name {
        return Some(NameMatch::Command);
    }

    let (first, rest) = arguments.split_first()?;

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
            ("node", "node", &node, Some(NameMatch::Command)),
            ("node", "MainThread", &node, Some(NameMatch::Program)),
            ("server.js", "MainThread", &node, Some(NameMatch::Script)),
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
