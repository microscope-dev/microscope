//! A Python program's own source files: those under the directories Python
//! searches for its modules before the standard library and installed
//! packages. No debug information names them, so a front end lists them,
//! and reads them, as these say.

use std::collections::{BTreeSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path, PathBuf};

/// The most files a listing holds.
const MOST_FILES: usize = 20_000;

/// The most directory entries a listing reads, so that a program run from
/// a home directory lists part of it rather than reading all of it.
const MOST_ENTRIES: usize = 200_000;

/// Where a Python program's own modules are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The script Python runs, when it runs one from a file.
    pub script: Option<PathBuf>,

    /// The directories Python searches first, absolute: the script's, or
    /// the working directory for `-c`, `-m`, or standard input; then each
    /// of `PYTHONPATH`'s, unless `-E` or `-I` ignores it.
    pub directories: Vec<PathBuf>,
}

/// The Python files under a project's directories, whether that is all of
/// them, and the directories that could not be read, which may hold more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub files: Vec<PathBuf>,
    pub complete: bool,
    pub unreadable: Vec<PathBuf>,
}

/// What an interpreter's command line runs.
enum Runs<'a> {
    Script(&'a OsStr),
    Directory,
}

/// What a project takes from an interpreter's command line.
struct CommandLine<'a> {
    runs: Runs<'a>,

    /// Whether `-E` or `-I` has Python ignore `PYTHONPATH`.
    ignores_environment: bool,
}

impl Project {
    /// The project of an interpreter run with `arguments` in `cwd`, with
    /// `PYTHONPATH` set to `pythonpath`, as Python begins `sys.path`.
    pub fn of(arguments: &[OsString], cwd: &Path, pythonpath: Option<&OsStr>) -> Self {
        let cwd = lexical(cwd);
        let line = command_line(arguments);

        let (script, first) = match line.runs {
            Runs::Script(script) => {
                let script = lexical(&cwd.join(script));
                if script.is_dir() {
                    // A directory runs its __main__.py.
                    let main = script.join("__main__.py");
                    (main.is_file().then_some(main), script)
                } else {
                    let directory = script.parent().map_or_else(|| cwd.clone(), Path::to_owned);
                    (Some(script), directory)
                }
            }
            Runs::Directory => (None, cwd.clone()),
        };

        let mut directories = vec![first];
        if !line.ignores_environment
            && let Some(pythonpath) = pythonpath
        {
            for entry in std::env::split_paths(pythonpath) {
                let entry = lexical(&cwd.join(entry));
                if !entry.as_os_str().is_empty() && !directories.contains(&entry) {
                    directories.push(entry);
                }
            }
        }

        Self {
            script,
            directories,
        }
    }

    /// The Python files under the project's directories, in the order of
    /// their paths, skipping caches, hidden directories, virtual
    /// environments, and installed packages, and following no link. A
    /// listing cut short keeps the files nearest its directories.
    pub fn files(&self) -> Listing {
        self.files_within(MOST_FILES, MOST_ENTRIES)
    }

    /// The listing `files` makes, of at most `most_files` files and from at
    /// most `most_entries` directory entries.
    fn files_within(&self, most_files: usize, most_entries: usize) -> Listing {
        let mut files = BTreeSet::new();
        let mut entries = 0;
        let mut unreadable = Vec::new();
        let mut walked = BTreeSet::new();
        let mut queue = self.directories.iter().cloned().collect::<VecDeque<_>>();

        while let Some(directory) = queue.pop_front() {
            if !walked.insert(directory.clone()) {
                continue;
            }

            // A directory that is not there holds no files; one that
            // cannot be read may.
            let read = match std::fs::read_dir(&directory) {
                Ok(read) => read,
                Err(error) => {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        unreadable.push(directory);
                    }
                    continue;
                }
            };

            // Read no more of a directory than the listing may take.
            let mut children = Vec::new();
            let mut whole = true;
            for child in read.take(most_entries.saturating_sub(entries).saturating_add(1)) {
                match child {
                    Ok(child) => children.push(child),
                    Err(_) => whole = false,
                }
            }

            children.sort_by_key(std::fs::DirEntry::file_name);
            for child in children {
                entries += 1;
                if entries > most_entries || files.len() >= most_files {
                    return Listing {
                        files: files.into_iter().collect(),
                        complete: false,
                        unreadable,
                    };
                }

                let Ok(kind) = child.file_type() else {
                    whole = false;
                    continue;
                };
                let path = child.path();
                if kind.is_dir() && !skipped(&path) {
                    queue.push_back(path);
                } else if kind.is_file() && is_python(&path) {
                    files.insert(path);
                }
            }

            if !whole {
                unreadable.push(directory);
            }
        }

        Listing {
            files: files.into_iter().collect(),
            complete: unreadable.is_empty(),
            unreadable,
        }
    }

    /// Whether `path` is one of the project's files: a Python file under
    /// one of its directories, through none that a listing skips, and no
    /// link out of it. A link within a directory holds, though a listing,
    /// which follows none, leaves it out: a frame may run a file through
    /// one.
    pub fn holds(&self, path: &Path) -> bool {
        if !path.is_absolute() || !is_python(path) {
            return false;
        }

        let path = lexical(path);
        let Ok(real) = path.canonicalize() else {
            return false;
        };
        if !std::fs::metadata(&real).is_ok_and(|metadata| metadata.is_file()) {
            return false;
        }

        self.directories.iter().any(|root| {
            let Some(between) = path.strip_prefix(root).ok().and_then(Path::parent) else {
                return false;
            };
            let mut directory = root.clone();
            for component in between.components() {
                directory.push(component);
                if skipped(&directory) {
                    return false;
                }
            }

            root.canonicalize().is_ok_and(|root| real.starts_with(root))
        })
    }
}

/// What `arguments`, CPython's command line after the interpreter, run:
/// options come first, `-c` and `-m` end them with what they run, `-W`
/// and `-X` take a value, and the first word that is no option is the
/// script, or `-` for standard input.
fn command_line(arguments: &[OsString]) -> CommandLine<'_> {
    let mut ignores_environment = false;
    let mut rest = arguments.iter();
    let runs = 'options: loop {
        let Some(argument) = rest.next() else {
            break Runs::Directory;
        };
        let bytes = argument.as_bytes();

        match bytes {
            b"-" => break Runs::Directory,
            b"--" => {
                break rest
                    .next()
                    .filter(|script| script.as_bytes() != b"-")
                    .map_or(Runs::Directory, |script| Runs::Script(script));
            }
            b"--check-hash-based-pycs" => {
                rest.next();
            }
            _ if bytes.starts_with(b"--") => {}
            [b'-', flags @ ..] => {
                for (at, flag) in flags.iter().enumerate() {
                    match flag {
                        b'c' | b'm' => break 'options Runs::Directory,
                        b'E' | b'I' => ignores_environment = true,
                        b'W' | b'X' => {
                            if at + 1 == flags.len() {
                                rest.next();
                            }
                            break;
                        }
                        _ => {}
                    }
                }
            }
            _ => break Runs::Script(argument),
        }
    };

    CommandLine {
        runs,
        ignores_environment,
    }
}

fn is_python(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "py")
}

/// Whether a listing passes over the directory at `path`: a cache, a
/// hidden directory, installed packages, or a virtual environment.
fn skipped(path: &Path) -> bool {
    let name = path.file_name().map(OsStr::as_bytes).unwrap_or_default();
    name.starts_with(b".")
        || matches!(name, b"__pycache__" | b"node_modules" | b"site-packages")
        || path.join("pyvenv.cfg").exists()
}

/// `path` with its `.` and `..` components resolved by their names alone,
/// as Python's `os.path.abspath` makes the paths it records.
fn lexical(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(arguments: &[&str], pythonpath: Option<&str>) -> Project {
        let arguments = arguments.iter().map(OsString::from).collect::<Vec<_>>();
        Project::of(
            &arguments,
            Path::new("/work/app"),
            pythonpath.map(OsStr::new),
        )
    }

    fn paths(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    /// The script is the first word that is no option, and its directory is
    /// searched first.
    #[test]
    fn a_scripts_directory_is_searched_first() {
        let script = project(&["-B", "-W", "error", "tools/../run.py", "-c"], None);
        assert_eq!(script.script, Some(PathBuf::from("/work/app/run.py")));
        assert_eq!(script.directories, paths(&["/work/app"]));

        let joined = project(&["-Werror", "-Xdev", "-u", "--", "/srv/main.py"], None);
        assert_eq!(joined.script, Some(PathBuf::from("/srv/main.py")));
        assert_eq!(joined.directories, paths(&["/srv"]));
    }

    /// `-c`, `-m`, and standard input search the working directory first,
    /// whatever follows them.
    #[test]
    fn a_command_or_module_searches_the_working_directory() {
        for arguments in [
            &["-c", "print(1)"][..],
            &["-Bc", "x.py"][..],
            &["-m", "pkg.main", "x.py"][..],
            &["-"][..],
            &[][..],
        ] {
            let project = project(arguments, None);
            assert_eq!(project.script, None, "{arguments:?}");
            assert_eq!(project.directories, paths(&["/work/app"]), "{arguments:?}");
        }
    }

    /// `PYTHONPATH`'s directories follow, relative ones in the working
    /// directory, unless the command line ignores the environment.
    #[test]
    fn pythonpath_follows_unless_ignored() {
        let searched = project(&["main.py"], Some("lib:/opt/shared::lib"));
        assert_eq!(
            searched.directories,
            paths(&["/work/app", "/work/app/lib", "/opt/shared"])
        );

        for ignoring in ["-E", "-I", "-sI"] {
            let ignored = project(&[ignoring, "main.py"], Some("lib"));
            assert_eq!(ignored.directories, paths(&["/work/app"]), "{ignoring}");
        }
    }

    /// A listing holds the Python files under the project, nearest first,
    /// and none of its caches, hidden directories, or environments; a read
    /// is allowed for exactly those.
    #[test]
    fn a_listing_and_a_read_take_the_same_files() {
        let root =
            std::env::temp_dir().join(format!("microscope-python-files-{}", std::process::id()));
        let file = |relative: &str| {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().expect("a directory")).expect("a directory");
            std::fs::write(&path, "").expect("a file");
            path
        };
        let main = file("main.py");
        let module = file("pkg/module.py");
        let skipped = [
            file("pkg/__pycache__/module.py"),
            file(".git/hook.py"),
            file("env/lib/thing.py"),
            file("node_modules/x/y.py"),
        ];
        file("env/pyvenv.cfg");
        file("notes.txt");

        let project = Project {
            script: Some(main.clone()),
            directories: vec![root.clone()],
        };
        let listing = project.files();
        assert!(listing.complete);
        assert_eq!(listing.files, vec![main.clone(), module.clone()]);
        assert!(project.holds(&main) && project.holds(&module));
        for path in &skipped {
            assert!(!project.holds(path), "{}", path.display());
        }
        assert!(!project.holds(&root.join("notes.txt")));
        assert!(!project.holds(&root.join("pkg/../../elsewhere.py")));

        // A listing cut short says so, and keeps the nearest files.
        let short = project.files_within(1, 100);
        assert_eq!((short.files, short.complete), (vec![main], false));
        assert!(!project.files_within(100, 3).complete);

        std::fs::remove_dir_all(&root).expect("remove the scratch directory");
    }

    /// A directory that cannot be read leaves its files out of a listing
    /// that says it is incomplete; one that is not there holds no files.
    #[test]
    fn a_listing_that_cannot_read_a_directory_is_incomplete() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = std::env::temp_dir().join(format!(
            "microscope-python-unreadable-{}",
            std::process::id()
        ));
        let locked = root.join("locked");
        std::fs::create_dir_all(&locked).expect("a directory");
        std::fs::write(root.join("main.py"), "").expect("a file");
        let project = Project {
            script: None,
            directories: vec![root.clone(), root.join("absent")],
        };
        assert!(
            project.files().complete,
            "a directory that is not there holds no files"
        );

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("lock it");
        // Root reads any directory.
        let readable = std::fs::read_dir(&locked).is_ok();
        let listing = project.files();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
            .expect("unlock it");
        std::fs::remove_dir_all(&root).expect("remove the scratch directory");
        assert_eq!(listing.files, vec![root.join("main.py")]);
        assert_eq!(listing.complete, readable);
        assert_eq!(listing.unreadable.is_empty(), readable);
    }
}
