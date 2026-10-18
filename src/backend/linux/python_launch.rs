//! The launch bootstrap through which CPython 3.12 and 3.13, which have no
//! PEP 768 request to run a script, take the agent's scripts: a private
//! directory holding `sitecustomize.py`, which a launch puts first on
//! `PYTHONPATH`; the FIFO beside it, which microscope holds open to read
//! and write and writes each script's path to; and the launched process's
//! ID, which only that process's bootstrap answers to. The bootstrap puts
//! the variables and the path back before the program runs.

use std::ffi::{OsStr, OsString};
use std::io::{Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
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
}

impl PythonLaunch {
    /// Prepares the bootstrap for a launch of `executable`, when it is
    /// CPython 3.12 or 3.13, and sets the variables that lead its
    /// interpreter to it in `environment`, which holds the launch's own
    /// changes to the debugger's environment.
    pub(super) fn prepare(
        executable: &Path,
        environment: &mut Vec<(OsString, Option<OsString>)>,
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
        Ok(Some(launch))
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
            Ok(channel) => Ok(Self { directory, channel }),
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
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// Whether `executable` is CPython 3.12 or 3.13, as its own symbols or the
/// libpython it needs say. Any other program is launched as it is.
fn launches_bootstrapped_python(executable: &Path) -> bool {
    use object::{Object as _, ObjectSymbol as _};

    let Ok(bytes) = std::fs::read(executable) else {
        return false;
    };
    let Ok(file) = object::File::parse(bytes.as_slice()) else {
        return false;
    };
    let needed = needed_libraries(&bytes);
    let library = needed.iter().any(|name| {
        let name = name.as_bytes();
        [b"libpython3.12".as_slice(), b"libpython3.13"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
    });
    if library {
        return true;
    }

    // A static build carries its version as `Py_Version`.
    file.symbols()
        .chain(file.dynamic_symbols())
        .find(|symbol| symbol.name() == Ok("Py_Version"))
        .and_then(|symbol| {
            let section = file.section_by_index(symbol.section_index()?).ok()?;
            let data = object::ObjectSection::data_range(&section, symbol.address(), 4).ok()??;
            Some(u32::from_le_bytes(data.try_into().ok()?))
        })
        .is_some_and(|version| {
            let version = crate::image::python::PyVersion(version);
            version.major() == 3 && (12..=13).contains(&version.minor())
        })
}

/// The libraries an ELF file's dynamic section names as needed.
fn needed_libraries(bytes: &[u8]) -> Vec<&OsStr> {
    use object::read::elf::{Dyn as _, FileHeader as _, SectionHeader as _};

    let Ok(header) = object::elf::FileHeader64::<object::Endianness>::parse(bytes) else {
        return Vec::new();
    };
    let Ok(endian) = header.endian() else {
        return Vec::new();
    };
    let Ok(sections) = header.sections(endian, bytes) else {
        return Vec::new();
    };
    let mut needed = Vec::new();
    for section in sections.iter() {
        let Ok(Some((entries, link))) = section.dynamic(endian, bytes) else {
            continue;
        };
        let Ok(strings) = sections.strings(endian, bytes, link) else {
            continue;
        };
        for entry in entries {
            if entry.d_tag(endian) == object::elf::DT_NEEDED
                && let Ok(name) = entry.string(endian, strings)
            {
                needed.push(OsStr::from_bytes(name));
            }
        }
    }
    needed
}

#[cfg(test)]
mod tests;
