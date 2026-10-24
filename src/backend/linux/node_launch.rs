//! The launch preload through which a launched Node opens its inspector and
//! waits for microscope (F31): a private directory holding `preload.js`,
//! which a launch appends to `NODE_OPTIONS` as `--require`, and
//! `MICROSCOPE_TRACER`, the tracer thread's ID, which only a node that
//! thread traces matches. The preload puts the variables back before the
//! program runs, unless the launch follows the processes the program
//! starts: then they stay for child nodes.

use std::ffi::OsString;
use std::io::Read as _;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::OpenOptionsExt as _;
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

fn problem(error: impl std::fmt::Display) -> Arc<str> {
    format!("the Node launch preload failed: {error}").into()
}

/// Why the inspector of the node `pid`, which microscope launched or
/// followed, may never start, as its command line and first environment
/// tell, if they do.
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

/// A launch's preload directory, removed when the launch ends.
#[derive(Debug)]
pub(super) struct NodeLaunch {
    directory: PathBuf,
}

impl NodeLaunch {
    /// Prepares the preload for a launch of `executable`, when it is Node or
    /// the launch follows the processes it starts, and sets the variables
    /// that lead each node `tracer` traces to it in `environment`, which
    /// holds the launch's own changes to the debugger's environment.
    pub(super) fn prepare(
        executable: &Path,
        environment: &mut Vec<(OsString, Option<OsString>)>,
        follows: bool,
        tracer: u32,
    ) -> Result<Option<Self>, Arc<str>> {
        if !follows && !launches_node(executable) {
            return Ok(None);
        }

        let launch = Self::create()?;
        let set = environment
            .iter()
            .rev()
            .find(|(name, _)| name == "NODE_OPTIONS")
            .map_or_else(
                || std::env::var_os("NODE_OPTIONS"),
                |(_, value)| value.clone(),
            );
        let mut options = OsString::new();
        let mut original = OsString::from("0");
        if let Some(set) = set {
            if !set.is_empty() {
                options.push(&set);
                options.push(" ");
            }
            original = OsString::from("1");
            original.push(&set);
        }
        options.push("--require ");
        options.push(quoted(&launch.directory.join("preload.js")));

        environment.push(("NODE_OPTIONS".into(), Some(options)));
        environment.push((ORIGINAL_OPTIONS.into(), Some(original)));
        environment.push((TRACER.into(), Some(tracer.to_string().into())));
        if follows {
            environment.push((FOLLOW.into(), Some("1".into())));
        }

        Ok(Some(launch))
    }

    fn create() -> Result<Self, Arc<str>> {
        let mut random = [0; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut random))
            .map_err(problem)?;

        let directory =
            Path::new("/tmp").join(format!("{PREFIX}{:032x}", u128::from_le_bytes(random)));
        nix::unistd::mkdir(&directory, Mode::S_IRWXU).map_err(problem)?;

        let made = || -> Result<(), Arc<str>> {
            let held = open(
                &directory,
                OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .map_err(problem)?;
            let stat = fstat(&held).map_err(problem)?;
            if stat.st_uid != rustix::process::geteuid().as_raw() || stat.st_mode & 0o777 != 0o700 {
                return Err("the Node launch directory is not private to the debugger".into());
            }
            std::fs::write(directory.join("preload.js"), PRELOAD).map_err(problem)
        };

        match made() {
            Ok(()) => Ok(Self { directory }),
            Err(reason) => {
                let _ = std::fs::remove_dir_all(&directory);
                Err(reason)
            }
        }
    }
}

impl Drop for NodeLaunch {
    fn drop(&mut self) {
        // A node that inherited the option after this cannot load the
        // preload and fails to start, as an unreadable --require does.
        let _ = std::fs::remove_dir_all(&self.directory);
    }
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
