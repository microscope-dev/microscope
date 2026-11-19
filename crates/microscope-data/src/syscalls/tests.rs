use std::path::{Path, PathBuf};

fn sources(directory: &Path) -> Vec<(PathBuf, String)> {
    let mut sources = Vec::new();

    for entry in std::fs::read_dir(directory).expect("read the sources") {
        let path = entry.expect("a directory entry").path();
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

#[test]
fn the_model_stays_pure() {
    const FORBIDDEN: [&str; 12] = [
        "crate::backend",
        "crate::debug_info",
        "crate::sim",
        "nix::",
        "libc",
        "tokio",
        "std::fs",
        "std::env",
        "std::process",
        "std::thread",
        "std::time",
        "aya",
    ];
    let sources = sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src/syscalls"));

    for (path, source) in &sources {
        for forbidden in FORBIDDEN {
            assert!(
                !source.contains(forbidden),
                "{} uses `{forbidden}`",
                path.display()
            );
        }
    }

    assert!(sources.len() >= 8, "the model's sources were found");
}

#[test]
fn interpreted_frames_interleave_with_native_ones_where_they_go() {
    use std::sync::Arc;

    use super::{CallStack, InterpretedCall};

    let call = |function: &str, before| InterpretedCall {
        before,
        function: function.into(),
        file: None,
        line: 0,
        first_line: None,
    };
    let stack = CallStack {
        addresses: Arc::from([10, 11, 12]),
        truncated: None,
        interpreted: Arc::from([call("a", 1), call("b", 1), call("c", 2), call("d", 3)]),
    };
    let shown: Vec<String> = stack
        .frames()
        .map(|frame| match frame {
            Ok(index) => index.to_string(),
            Err(call) => call.function.to_string(),
        })
        .collect();
    assert_eq!(shown, ["0", "a", "b", "1", "c", "2", "d"]);
}
