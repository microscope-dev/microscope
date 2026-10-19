//! What the picker offers: paths that complete a typed one, and the
//! processes one could attach to.

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use super::protocol::{PathEntry, PathKind, Process};

/// The most completions one answer carries.
const MAX_ENTRIES: usize = 200;

/// Expands a leading `~` and resolves the rest against `cwd`.
pub fn resolve(text: &str, cwd: &Path, home: Option<&Path>) -> PathBuf {
    match (text.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            home.join(rest.trim_start_matches('/'))
        }
        _ => cwd.join(text),
    }
}

/// The entries of the directory `text` names that its last component
/// begins, in the order a person scans them: directories, executables, then
/// other files, each alphabetically. Hidden entries appear only once the
/// typed name begins with a dot.
pub fn complete(text: &str, cwd: &Path, home: Option<&Path>) -> Vec<PathEntry> {
    let (directory, prefix) = text
        .rfind('/')
        .map_or(("", text), |slash| text.split_at(slash + 1));
    let Ok(entries) = std::fs::read_dir(resolve(directory, cwd, home)) else {
        return Vec::new();
    };

    let mut found = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !name.starts_with(prefix) || (name.starts_with('.') && !prefix.starts_with('.')) {
                return None;
            }

            // Follow symbolic links, as running the path would.
            let metadata = std::fs::metadata(entry.path()).ok()?;
            let kind = if metadata.is_dir() {
                PathKind::Directory
            } else if metadata.permissions().mode() & 0o111 != 0 {
                PathKind::Executable
            } else {
                PathKind::File
            };

            let slash = if kind == PathKind::Directory { "/" } else { "" };
            Some(PathEntry {
                text: format!("{directory}{name}{slash}"),
                kind,
            })
        })
        .collect::<Vec<_>>();

    found.sort_by(|a, b| {
        let rank = |kind| match kind {
            PathKind::Directory => 0,
            PathKind::Executable => 1,
            PathKind::File => 2,
        };

        (rank(a.kind), &a.text).cmp(&(rank(b.kind), &b.text))
    });

    found.truncate(MAX_ENTRIES);
    found
}

/// This user's processes other than the server itself, newest first.
pub fn processes(proc: &Path) -> Vec<Process> {
    let me = std::process::id();

    // The owner of this process's own directory is this user.
    let (Ok(entries), Ok(own)) = (
        std::fs::read_dir(proc),
        std::fs::metadata(proc.join("self")),
    ) else {
        return Vec::new();
    };

    let uid = own.uid();
    let mut found = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            if pid == me || entry.metadata().ok()?.uid() != uid {
                return None;
            }

            let path = entry.path();
            let command = std::fs::read(path.join("cmdline"))
                .ok()
                .map(|bytes| {
                    bytes
                        .split(|&byte| byte == 0)
                        .filter(|part| !part.is_empty())
                        .map(String::from_utf8_lossy)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|command| !command.is_empty())
                .or_else(|| {
                    let name = std::fs::read_to_string(path.join("comm")).ok()?;
                    Some(format!("[{}]", name.trim_end()))
                })?;
            Some(Process {
                pid: u64::from(pid),
                command,
            })
        })
        .collect::<Vec<_>>();
    found.sort_by_key(|process| std::cmp::Reverse(process.pid));
    found
}

/// Yama's `ptrace_scope`, when the kernel has Yama.
pub fn ptrace_scope() -> Option<u8> {
    std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope")
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory removed when the test ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("microscope-picker-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create scratch");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn completions_rank_directories_then_executables_and_hide_dot_files() {
        let scratch = Scratch::new("complete");
        let root = &scratch.0;
        std::fs::create_dir(root.join("build")).expect("dir");
        std::fs::create_dir(root.join("bin")).expect("dir");
        std::fs::write(root.join("build/kvstore"), "").expect("file");
        std::fs::set_permissions(
            root.join("build/kvstore"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod");
        std::fs::write(root.join("build/kv.o"), "").expect("file");
        std::fs::write(root.join("build/.kvhidden"), "").expect("file");
        std::fs::create_dir(root.join("build/kvdir")).expect("dir");

        let texts = |text: &str| {
            complete(text, root, None)
                .into_iter()
                .map(|entry| entry.text)
                .collect::<Vec<_>>()
        };

        assert_eq!(texts("b"), ["bin/", "build/"]);
        assert_eq!(
            texts("build/kv"),
            ["build/kvdir/", "build/kvstore", "build/kv.o"]
        );
        assert_eq!(texts("build/.kv"), ["build/.kvhidden"]);
        assert_eq!(texts("missing/"), Vec::<String>::new());
        let kinds = complete("build/kv", root, None)
            .into_iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [PathKind::Directory, PathKind::Executable, PathKind::File]
        );

        // A home directory, and an absolute path regardless of cwd.
        assert_eq!(
            complete("~/build/kvs", Path::new("/nonexistent"), Some(root))[0].text,
            "~/build/kvstore"
        );
        let absolute = format!("{}/build/kvs", root.display());
        assert_eq!(
            complete(&absolute, Path::new("/nonexistent"), None)[0].text,
            format!("{}/build/kvstore", root.display())
        );
    }

    #[test]
    fn processes_list_this_users_others_with_their_command_lines() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let listed = processes(Path::new("/proc"));
        let _ = child.kill();
        let _ = child.wait();
        let pid = u64::from(child.id());
        let found = listed
            .iter()
            .find(|process| process.pid == pid)
            .expect("the child is listed");
        assert_eq!(found.command, "sleep 30");
        assert!(
            listed
                .iter()
                .all(|process| process.pid != u64::from(std::process::id()))
        );
        assert!(listed.windows(2).all(|pair| pair[0].pid > pair[1].pid));
    }
}
