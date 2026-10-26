//! Agent bootstrap for CPython 3.12 and 3.13, which lack PEP 768 script requests.
//! A private directory puts `sitecustomize.py` first on `PYTHONPATH` and holds a FIFO
//! for script paths, opened read/write by microscope. A saved PID selects the launched process.
//!
//! The bootstrap restores the environment and path before running the program, unless
//! following children. Each debugged child interpreter gets its own `wake-PID` FIFO
//! in the launch directory; forked interpreters switch to theirs after forking.

use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::sys::stat::{Mode, fstat};
use nix::unistd::Pid;

/// The bootstrap's source.
const SITECUSTOMIZE: &str = include_str!("../../runtime_model/python/launch.py");

/// Where the bootstrap finds the FIFO, and the `PYTHONPATH` to put back.
const WAKE: &str = "MICROSCOPE_PYTHON_WAKE";
const ORIGINAL_PATH: &str = "MICROSCOPE_PYTHON_PATH";

/// Set when the variables stay for child interpreters.
const FOLLOW: &str = "MICROSCOPE_PYTHON_FOLLOW";

/// The longest script path one request writes, which one write to a FIFO
/// keeps whole.
const MAX_REQUEST: usize = 4096;

fn problem(error: impl std::fmt::Display) -> Arc<str> {
    format!("the Python launch bootstrap failed: {error}").into()
}

/// A launch's bootstrap directory and the FIFO microscope holds open.
#[derive(Debug)]
pub(super) struct PythonLaunch {
    directory: PathBuf,
    channel: std::fs::File,

    /// Child FIFO: removes only this file, not the launch's directory.
    member: Option<PathBuf>,
}

impl PythonLaunch {
    /// Prepares CPython 3.12/3.13's bootstrap and adds its variables to the launch's
    /// `environment` overrides, preserving any existing `PYTHONPATH` override.
    pub(super) fn prepare(
        executable: &Path,
        environment: &mut Vec<(OsString, Option<OsString>)>,
        follows: bool,
    ) -> Result<Option<Self>, Arc<str>> {
        if !launches_bootstrapped_python(executable) {
            return Ok(None);
        }

        let launch = Self::create()?;
        let set = environment
            .iter()
            .rev()
            .find(|(name, _)| name == "PYTHONPATH")
            .map_or_else(
                || std::env::var_os("PYTHONPATH"),
                |(_, value)| value.clone(),
            );
        let mut path = launch.directory.as_os_str().to_owned();
        let mut original = OsString::from("0");
        if let Some(set) = set {
            if !set.is_empty() {
                path.push(":");
                path.push(&set);
            }
            original = OsString::from("1");
            original.push(&set);
        }

        environment.push(("PYTHONPATH".into(), Some(path)));
        environment.push((ORIGINAL_PATH.into(), Some(original)));
        environment.push((
            WAKE.into(),
            Some(launch.directory.join("wake").into_os_string()),
        ));
        if follows {
            environment.push((FOLLOW.into(), Some("1".into())));
        }

        Ok(Some(launch))
    }

    /// Creates a CPython 3.12/3.13 child's FIFO in the inherited bootstrap directory.
    /// Must run before the child resumes so its bootstrap finds the FIFO.
    pub(super) fn join(executable: &Path, pid: Pid) -> Result<Option<Self>, Arc<str>> {
        if !launches_bootstrapped_python(executable) {
            return Ok(None);
        }

        let environment = std::fs::read(format!("/proc/{pid}/environ")).map_err(problem)?;
        let prefix = format!("{WAKE}=");
        let wake = environment
            .split(|&byte| byte == 0)
            .find_map(|entry| entry.strip_prefix(prefix.as_bytes()))
            .map(|path| PathBuf::from(std::ffi::OsStr::from_bytes(path)))
            .ok_or(
                "the interpreter was not started with microscope's launch bootstrap, which \
                 CPython 3.12 and 3.13 need for the Python agent; launch the program that \
                 starts it with microscope",
            )?;
        let directory = wake
            .parent()
            .filter(|directory| directory.parent() == Some(Path::new("/tmp")))
            .filter(|directory| {
                directory.file_name().is_some_and(|name| {
                    name.to_string_lossy()
                        .starts_with(crate::runtime_model::PYTHON_BOOTSTRAP_PREFIX)
                })
            })
            .ok_or("the interpreter's bootstrap variables do not name microscope's directory")?
            .to_owned();

        let held = open(
            &directory,
            OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(problem)?;
        let stat = fstat(&held).map_err(problem)?;
        if stat.st_uid != rustix::process::geteuid().as_raw() || stat.st_mode & 0o777 != 0o700 {
            return Err("the Python launch directory is not private to the debugger".into());
        }

        let member = directory.join(format!("wake-{pid}"));
        nix::unistd::mkfifo(&member, Mode::S_IRUSR | Mode::S_IWUSR).map_err(problem)?;
        let channel: OwnedFd = match open(
            &member,
            OFlag::O_RDWR | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(channel) => channel,
            Err(error) => {
                let _ = std::fs::remove_file(&member);
                return Err(problem(error));
            }
        };

        Ok(Some(Self {
            directory,
            channel: channel.into(),
            member: Some(member),
        }))
    }

    fn create() -> Result<Self, Arc<str>> {
        let parent = Path::new("/tmp");
        let mut random = [0; 16];

        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut random))
            .map_err(problem)?;

        let directory = parent.join(format!(
            "{}{:032x}",
            crate::runtime_model::PYTHON_BOOTSTRAP_PREFIX,
            u128::from_le_bytes(random)
        ));
        nix::unistd::mkdir(&directory, Mode::S_IRWXU).map_err(problem)?;

        let made = || -> Result<std::fs::File, Arc<str>> {
            let held = open(
                &directory,
                OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .map_err(problem)?;
            let stat = fstat(&held).map_err(problem)?;
            if stat.st_uid != rustix::process::geteuid().as_raw() || stat.st_mode & 0o777 != 0o700 {
                return Err("the Python launch directory is not private to the debugger".into());
            }

            std::fs::write(directory.join("sitecustomize.py"), SITECUSTOMIZE).map_err(problem)?;
            let wake = directory.join("wake");
            nix::unistd::mkfifo(&wake, Mode::S_IRUSR | Mode::S_IWUSR).map_err(problem)?;

            // Read and write, so that opening it waits for no reader, and
            // a request no thread took can be read back out.
            let channel: OwnedFd = open(
                &wake,
                OFlag::O_RDWR | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
                Mode::empty(),
            )
            .map_err(problem)?;

            Ok(channel.into())
        };

        match made() {
            Ok(channel) => Ok(Self {
                directory,
                channel,
                member: None,
            }),
            Err(reason) => {
                let _ = std::fs::remove_dir_all(&directory);
                Err(reason)
            }
        }
    }

    /// Names the process the launch made, the only one whose bootstrap
    /// takes the requests, before it runs.
    pub(super) fn launched(&self, pid: Pid) -> Result<(), Arc<str>> {
        let staged = self.directory.join("pid.new");
        std::fs::write(&staged, pid.as_raw().to_string()).map_err(problem)?;
        std::fs::rename(&staged, self.directory.join("pid")).map_err(problem)
    }

    /// Asks the bootstrap to run the script at `path`.
    pub(super) fn request(&self, path: &str) -> Result<(), Arc<str>> {
        let mut line = path.as_bytes().to_vec();
        line.push(b'\n');
        if line.len() > MAX_REQUEST || path.contains('\n') || !path.starts_with('/') {
            return Err("the Python agent's script path does not fit a bootstrap request".into());
        }

        match (&self.channel).write(&line) {
            Ok(written) if written == line.len() => Ok(()),
            Ok(_) => Err("the Python launch bootstrap took only part of a request".into()),
            Err(error) if error.raw_os_error() == Some(Errno::EAGAIN as i32) => Err(
                "the Python launch bootstrap has not read the requests before; it runs only \
                 where the interpreter imports sitecustomize from PYTHONPATH"
                    .into(),
            ),
            Err(error) => Err(problem(error)),
        }
    }

    /// Reads back every request no thread has taken, and says whether the
    /// one for `path` was among them.
    pub(super) fn withdraw(&self, path: &str) -> Result<bool, Arc<str>> {
        let mut unread = Vec::new();
        let mut buffer = [0; 4096];

        loop {
            match (&self.channel).read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => unread.extend_from_slice(&buffer[..read]),
                Err(error) if error.raw_os_error() == Some(Errno::EAGAIN as i32) => break,
                Err(error) => return Err(problem(error)),
            }
        }

        Ok(unread
            .split(|&byte| byte == b'\n')
            .any(|line| line == path.as_bytes()))
    }
}

impl Drop for PythonLaunch {
    fn drop(&mut self) {
        // A child that inherited the path finds no bootstrap and goes on.
        let _ = match &self.member {
            Some(member) => std::fs::remove_file(member),
            None => std::fs::remove_dir_all(&self.directory),
        };
    }
}

/// Whether `executable` is CPython 3.12 or 3.13, as the libpython it needs
/// or its own exported version says. Any other program is launched as it
/// is. Only the headers, the dynamic section, and the dynamic symbols are
/// read, since every launch of any program asks.
fn launches_bootstrapped_python(executable: &Path) -> bool {
    use object::read::elf::{Dyn as _, SectionHeader as _};
    use object::{Object as _, ObjectSection as _, ObjectSymbol as _, ReadRef as _};

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
                && entry.string(endian, strings).is_ok_and(|name| {
                    [b"libpython3.12".as_slice(), b"libpython3.13"]
                        .iter()
                        .any(|prefix| name.starts_with(prefix))
                })
        })
    });
    if library {
        return true;
    }

    // A static build exports its version as `Py_Version`: CPython links
    // its executable to export every symbol, for its extensions.
    elf.dynamic_symbols()
        .find(|symbol| symbol.name() == Ok("Py_Version"))
        .and_then(|symbol| {
            let section = elf.section_by_index(symbol.section_index()?).ok()?;
            let (offset, _) = section.file_range()?;
            let within = symbol.address().checked_sub(section.address())?;
            let bytes = data.read_bytes_at(offset.checked_add(within)?, 4).ok()?;
            Some(u32::from_le_bytes(bytes.try_into().ok()?))
        })
        .is_some_and(|version| {
            let version = crate::image::python::PyVersion(version);
            version.major() == 3 && (12..=13).contains(&version.minor())
        })
}

#[cfg(test)]
mod tests;
