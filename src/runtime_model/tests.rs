use std::path::{Path, PathBuf};

/// Every Rust source under `directory`, with the code compiled only for
/// tests removed.
fn sources(directory: &Path) -> Vec<(PathBuf, String)> {
    let mut pending = vec![directory.to_owned()];
    let mut sources = Vec::new();

    while let Some(path) = pending.pop() {
        if path.is_dir() {
            let entries = std::fs::read_dir(&path).expect("read a source directory");
            pending.extend(entries.map(|entry| entry.expect("a directory entry").path()));
            continue;
        }

        let test_file = path.file_name().is_some_and(|name| name == "tests.rs");
        if test_file || path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }

        let source = std::fs::read_to_string(&path).expect("read a source file");
        let source = source.split("#[cfg(test)]").next().unwrap_or_default();
        sources.push((path, source.to_owned()));
    }

    sources.sort();
    sources
}

fn source_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// The code of a source, without its comments and the text of its string
/// literals, which may name what the code may not use.
fn code(source: &str) -> String {
    let mut code = String::with_capacity(source.len());

    for line in source.lines() {
        let line = line.split("//").next().unwrap_or_default();
        let mut quoted = false;
        let mut escaped = false;

        for character in line.chars() {
            match (quoted, escaped, character) {
                (true, false, '\\') => escaped = true,
                (true, true, _) => escaped = false,
                (_, false, '"') => {
                    quoted = !quoted;
                    code.push('"');
                }
                (true, false, _) => {}
                (false, _, character) => code.push(character),
            }
        }

        code.push('\n');
    }

    code
}

/// A runtime model is pure: it reaches a program only through the traits
/// the debugger implements for it, so none of its code may reach for
/// process control, debug-information parsing, I/O, clocks, or threads.
/// A model may name a runtime's types and functions in its strings, but
/// never use the runtime's crate.
#[test]
fn runtime_model_stays_pure() {
    const FORBIDDEN: [&str; 13] = [
        "crate::backend",
        "crate::debug_info",
        "crate::sim",
        "nix::",
        "gimli",
        "object::",
        "use tokio",
        "std::fs",
        "std::env",
        "std::process",
        "std::thread",
        "std::time",
        "libc",
    ];
    let sources = sources(&source_root().join("runtime_model"));

    for (path, source) in &sources {
        let code = code(source);
        for forbidden in FORBIDDEN {
            assert!(
                !code.contains(forbidden),
                "{} uses `{forbidden}`",
                path.display()
            );
        }

        // The tokio model's own module is named for the runtime it reads.
        assert!(
            code.match_indices("tokio::")
                .all(|(at, _)| code[..at].ends_with("self::") || code[..at].ends_with("super::")),
            "{} uses `tokio::`",
            path.display()
        );
    }

    assert!(sources.len() >= 3, "the runtime models' sources were found");

    // The check sees through neither comments nor strings.
    assert!(code("let x = tokio::spawn(f);").contains("tokio::spawn"));
    assert!(!code("let x = \"tokio::spawn\"; // tokio::spawn").contains("tokio::"));
}

/// Each runtime is named in its own model only. Run control, unwinding,
/// the protocol, and the clients speak of tasks and code roles, and learn
/// what a runtime is through its model, never by recognizing its names.
#[test]
fn languages_stay_at_their_seams() {
    // Each row names a runtime in the way only its model, and the
    // debug-information provider's code roles, should.
    const RUNTIMES: [(&str, &[&str]); 3] = [
        (
            "runtime_model/go",
            &["\"runtime.", "allgs", "goroutine", "goid"],
        ),
        (
            "runtime_model/tokio",
            &[
                "ownedtasks",
                "\"tokio::runtime",
                "current_task_id",
                "context::context",
            ],
        ),
        ("runtime_model/rust", &["rust_panic", "rust_begin_unwind"]),
    ];
    let root = source_root();
    for directory in [
        "backend",
        "cli",
        "dap",
        "eval",
        "web",
        "present",
        "runtime_model",
        "unwind",
        "analysis",
        "capture",
        "profiler",
        "protocol.rs",
        "lib.rs",
    ] {
        let path = root.join(directory);
        let found = if path.is_dir() {
            sources(&path)
        } else {
            let source = std::fs::read_to_string(&path).expect("read a source file");
            let source = source
                .split("#[cfg(test)]")
                .next()
                .unwrap_or_default()
                .to_owned();
            vec![(path, source)]
        };

        for (path, source) in found {
            // Documentation may name a runtime as an example; code may not.
            let written = source
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            let code = written.to_lowercase();

            for (home, names) in RUNTIMES {
                if path.starts_with(root.join(home)) {
                    continue;
                }

                for name in names {
                    // Go's symbols are lowercase; V8's inspector protocol has
                    // a `Runtime` domain, which names no Go function.
                    let seen = if *name == "\"runtime." {
                        written.contains(name)
                    } else {
                        code.contains(name)
                    };
                    assert!(
                        !seen,
                        "{} names a runtime with `{name}`; ask its model instead",
                        path.display()
                    );
                }
            }
        }
    }
}

#[test]
fn thread_identity_evidence_is_read_once_and_forgotten_after_memory_changes() {
    let memory = super::StopMemory::default();
    let calls = std::cell::Cell::new(0);
    let read = || {
        calls.set(calls.get() + 1);
        vec![(
            crate::ThreadId::new(5000),
            super::TargetThreadId::new(7).unwrap(),
        )]
    };

    let first = memory.thread_identities(&[crate::ThreadId::new(5000)], read);
    let second = memory.thread_identities(&[crate::ThreadId::new(5000)], read);
    assert!(std::sync::Arc::ptr_eq(&first, &second));
    assert_eq!(calls.get(), 1);
    memory.forget();
    assert_eq!(
        &*memory.thread_identities(&[crate::ThreadId::new(5000)], read),
        &*first
    );
    assert_eq!(calls.get(), 2);
}

#[test]
fn partial_identity_evidence_can_be_retried_at_the_same_stop() {
    let memory = super::StopMemory::default();
    let host = crate::ThreadId::new(5000);
    let target = super::TargetThreadId::new(7).unwrap();
    assert!(memory.thread_identities(&[host], Vec::new).is_empty());
    assert_eq!(
        &*memory.thread_identities(&[host], || vec![(host, target)]),
        &[(host, target)]
    );
}
