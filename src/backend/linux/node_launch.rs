//! Launch preload that opens Node's inspector and waits for microscope.
//! `NODE_OPTIONS` prepends `--require` with a private `preload.js`;
//! `MICROSCOPE_TRACER` restricts it to nodes traced by that thread ID.
//! The preload restores the environment before the program runs, except
//! when following child processes, which must inherit it.

use std::ffi::OsString;
use std::io::Read as _;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nix::fcntl::{OFlag, open};
use nix::sys::stat::{Mode, fstat};

const PRELOAD: &str = include_str!("../../runtime_model/node/preload.js");

/// The directory's name, under `/tmp`, before its random part.
const PREFIX: &str = ".microscope-node-";

const TRACER: &str = "MICROSCOPE_TRACER";
const ORIGINAL_OPTIONS: &str = "MICROSCOPE_NODE_OPTIONS";
const FOLLOW: &str = "MICROSCOPE_NODE_FOLLOW";
const OWNERSHIP: &str = "MICROSCOPE_NODE_OWNERSHIP";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ownership {
    #[default]
    Pending,
    Created,
    Borrowed,
}

/// Checks a preload console acknowledgement against this process's launch nonce.
pub(super) fn acknowledge(
    pid: nix::unistd::Pid,
    target: &str,
    params: &serde_json::Value,
) -> Ownership {
    let Ok(environment) = std::fs::read(format!("/proc/{pid}/environ")) else {
        return Ownership::Pending;
    };
    acknowledgement(&environment, target, params)
}

fn acknowledgement(environment: &[u8], target: &str, params: &serde_json::Value) -> Ownership {
    let Some(nonce) = environment.split(|&byte| byte == 0).find_map(|variable| {
        variable
            .strip_prefix(OWNERSHIP.as_bytes())?
            .strip_prefix(b"=")
    }) else {
        return Ownership::Pending;
    };
    let Some(arguments) = params["args"].as_array() else {
        return Ownership::Pending;
    };
    let strings = arguments
        .iter()
        .map(|argument| {
            (argument["type"] == "string")
                .then(|| argument["value"].as_str())
                .flatten()
        })
        .collect::<Option<Vec<_>>>();
    let Some(strings) = strings else {
        return Ownership::Pending;
    };
    let [
        "microscope ownership",
        reported_nonce,
        reported_target,
        ownership,
    ] = strings.as_slice()
    else {
        return Ownership::Pending;
    };
    if params["type"] != "debug"
        || nonce.len() != 32
        || reported_nonce.as_bytes() != nonce
        || *reported_target != target
    {
        return Ownership::Pending;
    }
    match *ownership {
        "created" => Ownership::Created,
        "borrowed" => Ownership::Borrowed,
        _ => Ownership::Pending,
    }
}

fn problem(error: impl std::fmt::Display) -> Arc<str> {
    format!("the Node launch preload failed: {error}").into()
}

/// Why a launched or followed node's inspector may never start, if its
/// command line or initial environment reveals a cause.
pub(super) fn unstarted(pid: i32) -> Option<&'static str> {
    let read = |name| std::fs::read(format!("/proc/{pid}/{name}")).ok();
    unstarted_by(&read("cmdline")?, &read("environ")?)
}

fn unstarted_by(command: &[u8], environment: &[u8]) -> Option<&'static str> {
    let variables = environment.split(|&byte| byte == 0);
    let value = |name: &str| {
        variables
            .clone()
            .find_map(|variable| variable.strip_prefix(name.as_bytes())?.strip_prefix(b"="))
    };
    if value(TRACER).is_none() {
        return Some(
            "this node did not load microscope's launch preload, as when the program that \
             started it sets its NODE_OPTIONS",
        );
    }

    // Node's own options: NODE_OPTIONS, then the arguments before the
    // script. One that takes the next argument as its value ends the
    // search early, which can only miss a flag.
    let arguments = command.split(|&byte| byte == 0).skip(1);
    let options = value("NODE_OPTIONS")
        .into_iter()
        .flat_map(|options| options.split(u8::is_ascii_whitespace))
        .chain(arguments.take_while(|argument| argument.starts_with(b"-") && *argument != b"--"));
    let (mut permission, mut allowed) = (false, false);
    for option in options {
        let name = option.split(|&byte| byte == b'=').next().unwrap_or(option);
        permission |= name == b"--permission" || name == b"--experimental-permission";
        allowed |= name == b"--allow-inspector";
    }
    (permission && !allowed).then_some(
        "this node runs with --permission, under which Node opens no inspector without \
         --allow-inspector",
    )
}

/// A launch's preload directory. Followed launches retain a cached copy
/// because a released descendant can still inherit its require path.
#[derive(Debug)]
pub(super) struct NodeLaunch {
    directory: PathBuf,
    persistent: bool,
}

impl NodeLaunch {
    /// Prepares a preload for Node or a launch that follows child processes.
    /// Adds launch environment overrides for nodes traced by `tracer`.
    pub(super) fn prepare(
        executable: &Path,
        environment: &mut Vec<(OsString, Option<OsString>)>,
        follows: bool,
        tracer: u32,
    ) -> Result<Option<Self>, Arc<str>> {
        if !follows && !launches_node(executable) {
            return Ok(None);
        }

        let launch = Self::create(follows)?;
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).map_err(problem)?;
        let ownership = format!("{:032x}", u128::from_le_bytes(nonce));
        let set = environment
            .iter()
            .rev()
            .find(|(name, _)| name == "NODE_OPTIONS")
            .map_or_else(
                || std::env::var_os("NODE_OPTIONS"),
                |(_, value)| value.clone(),
            );
        let mut options = OsString::from("--require ");
        options.push(quoted(&launch.directory.join("preload.js")));
        let mut original = OsString::from("0");
        if let Some(set) = set {
            if !set.is_empty() {
                options.push(" ");
                options.push(&set);
            }
            original = OsString::from("1");
            original.push(&set);
        }

        environment.push(("NODE_OPTIONS".into(), Some(options)));
        environment.push((ORIGINAL_OPTIONS.into(), Some(original)));
        environment.push((TRACER.into(), Some(tracer.to_string().into())));
        environment.push((OWNERSHIP.into(), Some(ownership.into())));
        if follows {
            environment.push((FOLLOW.into(), Some("1".into())));
        }

        Ok(Some(launch))
    }

    fn create(follows: bool) -> Result<Self, Arc<str>> {
        use sha2::{Digest as _, Sha256};
        use std::fmt::Write as _;

        let cached = if follows {
            let root = std::env::var_os("XDG_CACHE_HOME")
                .filter(|path| Path::new(path).is_absolute())
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".cache")))
                .ok_or("following Node needs HOME or an absolute XDG_CACHE_HOME for its preload")?
                .join("microscope/node");
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&root)
                .map_err(problem)?;
            let mut key = String::with_capacity(64);
            for byte in Sha256::digest(PRELOAD.as_bytes()) {
                write!(&mut key, "{byte:02x}").map_err(problem)?;
            }
            Some(root.join(key))
        } else {
            None
        };
        if let Some(directory) = cached.as_ref().filter(|directory| directory.exists()) {
            validate_directory(directory)?;
            validate_preload(directory)?;
            return Ok(Self {
                directory: directory.clone(),
                persistent: true,
            });
        }
        let mut random = [0; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut random))
            .map_err(problem)?;

        let parent = cached
            .as_ref()
            .and_then(|directory| directory.parent())
            .unwrap_or_else(|| Path::new("/tmp"));
        let directory = parent.join(format!("{PREFIX}{:032x}", u128::from_le_bytes(random)));
        nix::unistd::mkdir(&directory, Mode::S_IRWXU).map_err(problem)?;

        let mut launch = Self {
            directory,
            persistent: false,
        };
        validate_directory(&launch.directory)?;
        std::fs::write(launch.directory.join("preload.js"), PRELOAD).map_err(problem)?;
        if let Some(cached) = cached {
            // Publish the complete directory atomically: a concurrent launch
            // must never load a partially written preload.
            match std::fs::rename(&launch.directory, &cached) {
                Ok(()) => {
                    launch.directory = cached;
                    launch.persistent = true;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::DirectoryNotEmpty
                    ) =>
                {
                    validate_directory(&cached)?;
                    validate_preload(&cached)?;
                    return Ok(Self {
                        directory: cached,
                        persistent: true,
                    });
                }
                Err(error) => return Err(problem(error)),
            }
        }
        Ok(launch)
    }
}

impl Drop for NodeLaunch {
    fn drop(&mut self) {
        if !self.persistent {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }
}

fn validate_directory(directory: &Path) -> Result<(), Arc<str>> {
    let held = open(
        directory,
        OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(problem)?;
    let stat = fstat(&held).map_err(problem)?;
    if stat.st_uid != rustix::process::geteuid().as_raw() || stat.st_mode & 0o777 != 0o700 {
        return Err("the Node launch directory is not private to the debugger".into());
    }
    Ok(())
}

fn validate_preload(directory: &Path) -> Result<(), Arc<str>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(directory.join("preload.js"))
        .map_err(problem)?;
    if !file.metadata().map_err(problem)?.is_file() {
        return Err("the cached Node preload is not a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(PRELOAD.len() as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(problem)?;
    if bytes != PRELOAD.as_bytes() {
        return Err("the cached Node preload differs from its content identity".into());
    }
    Ok(())
}

/// `path` as a word of `NODE_OPTIONS`, which Node splits at spaces outside
/// double quotes and reads `\` as escaping the next character within them.
fn quoted(path: &Path) -> OsString {
    let mut word = vec![b'"'];
    for &byte in path.as_os_str().as_bytes() {
        if matches!(byte, b'"' | b'\\') {
            word.push(b'\\');
        }
        word.push(byte);
    }
    word.push(b'"');
    OsString::from_vec(word)
}

/// Whether `executable` is Node: it exports Node-API, or needs the shared
/// libnode that does. Only the headers, the dynamic section, and the
/// dynamic symbols are read, since every launch asks.
fn launches_node(executable: &Path) -> bool {
    use object::read::elf::{Dyn as _, SectionHeader as _};
    use object::{Object as _, ObjectSymbol as _};

    // Without O_NONBLOCK, opening a FIFO would wait for a writer.
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(executable)
    else {
        return false;
    };
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }
    let data = object::ReadCache::new(file);
    let Ok(elf) = object::read::elf::ElfFile64::<object::Endianness, _>::parse(&data) else {
        return false;
    };
    let endian = elf.endian();
    let sections = elf.elf_section_table();
    let library = sections.iter().any(|section| {
        let Ok(Some((entries, link))) = section.dynamic(endian, &data) else {
            return false;
        };
        let Ok(strings) = sections.strings(endian, &data, link) else {
            return false;
        };
        entries.iter().any(|entry| {
            entry.d_tag(endian) == object::elf::DT_NEEDED
                && entry
                    .string(endian, strings)
                    .is_ok_and(|name| name.starts_with(b"libnode.so"))
        })
    });

    library
        || elf
            .dynamic_symbols()
            .any(|symbol| symbol.name_bytes() == Ok(b"napi_module_register".as_slice()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn acknowledgement_requires_the_launch_nonce_and_native_target() {
        let nonce = "0123456789abcdef0123456789abcdef";
        let environment = format!("{OWNERSHIP}={nonce}\0");
        let event = |nonce: &str, target: &str, ownership: &str| {
            let arguments = ["microscope ownership", nonce, target, ownership]
                .map(|value| serde_json::json!({"type": "string", "value": value}));
            serde_json::json!({
                "type": "debug",
                "args": arguments,
            })
        };
        for (reported_nonce, target, ownership, expected) in [
            (nonce, "native-target", "created", Ownership::Created),
            (nonce, "native-target", "borrowed", Ownership::Borrowed),
            (nonce, "stale-target", "created", Ownership::Pending),
            (
                "wrong nonce",
                "native-target",
                "created",
                Ownership::Pending,
            ),
            (nonce, "native-target", "unknown", Ownership::Pending),
        ] {
            assert_eq!(
                acknowledgement(
                    environment.as_bytes(),
                    "native-target",
                    &event(reported_nonce, target, ownership)
                ),
                expected
            );
        }
        assert_eq!(
            acknowledgement(
                b"",
                "native-target",
                &event(nonce, "native-target", "created")
            ),
            Ownership::Pending
        );
        assert_eq!(
            acknowledgement(
                environment.as_bytes(),
                "native-target",
                &serde_json::json!({})
            ),
            Ownership::Pending
        );
    }

    #[test]
    fn inherited_options_work_after_the_followed_launch_ends() {
        let root = std::env::var_os("MICROSCOPE_NODE").expect("the pinned Node matrix");
        let node = Path::new(&root).join("nixpkgs-24/node");
        let mut environment = vec![("NODE_OPTIONS".into(), Some("--no-warnings".into()))];
        let launch = NodeLaunch::prepare(&node, &mut environment, true, u32::MAX)
            .expect("prepare the launch")
            .expect("a Node preload");
        drop(launch);
        let mut command = std::process::Command::new(node);
        for (name, value) in environment {
            if let Some(value) = value {
                command.env(name, value);
            } else {
                command.env_remove(name);
            }
        }
        let output = command
            .args(["-p", "process.env.NODE_OPTIONS"])
            .output()
            .expect("start the descendant");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "--no-warnings"
        );
    }

    #[test]
    fn an_inspector_that_may_never_start_is_explained() {
        let environment = |options: &str| format!("A=1\0{TRACER}=7\0NODE_OPTIONS={options}\0");
        let cases = [
            ("node\0app.js\0", "", None),
            ("node\0--permission\0app.js\0", "", Some("--permission")),
            (
                "node\0--experimental-permission\0app.js\0",
                "",
                Some("--permission"),
            ),
            (
                "node\0app.js\0",
                "--require x --permission",
                Some("--permission"),
            ),
            ("node\0--permission\0--allow-inspector\0app.js\0", "", None),
            ("node\0--allow-inspector\0app.js\0", "--permission", None),
            // The script's own arguments are not Node's.
            ("node\0app.js\0--permission\0", "", None),
            ("node\0--\0--permission\0", "", None),
            ("node\0--permission-audit\0app.js\0", "", None),
        ];
        for (command, options, expected) in cases {
            let why = unstarted_by(command.as_bytes(), environment(options).as_bytes());
            assert_eq!(
                why.map(|why| why.contains(expected.unwrap_or_default())),
                expected.map(|_| true),
                "{command:?} {options:?}: {why:?}"
            );
        }
        let why = unstarted_by(b"node\0app.js\0", b"NODE_OPTIONS=--permission\0");
        assert!(why.is_some_and(|why| why.contains("preload")), "{why:?}");
    }

    #[test]
    fn a_path_is_quoted_as_node_options_reads_it() {
        assert_eq!(
            quoted(Path::new(r#"/tmp/a b"c\d"#)),
            OsStr::new(r#""/tmp/a b\"c\\d""#)
        );
    }
}
