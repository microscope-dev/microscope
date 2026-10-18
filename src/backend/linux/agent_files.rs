//! Private agent artifacts in the target's mount namespace. Directory file
//! descriptors keep cleanup possible after the target exits or execs.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;

use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, open, openat};
use nix::sys::stat::{FileStat, Mode, fstat, fstatat, mkdirat};
use nix::sys::statvfs::{FsFlags, fstatvfs};
use nix::unistd::{Pid, UnlinkatFlags, unlinkat};

/// The lifecycle owner calls these only at an all-stop. Acknowledgement
/// proves a script was consumed; exit proves no pending script can be.
/// The simulator supplies its own implementation, without host file access.
pub(super) trait AgentFiles: std::fmt::Debug + Send {
    /// Used only to build a script subsequently owned by `script`.
    /// Never publish an independent library-load request from this path.
    fn library(&self) -> Result<&str, Arc<str>>;
    fn script(&mut self, generation: u64, source: &str) -> Result<String, Arc<str>>;
    /// Revalidates through `thread`, a stopped thread of the target that
    /// will read the next script. The thread the owner was created for may
    /// since have exited, taking its procfs directory with it.
    fn anchor(&mut self, thread: Pid) -> Result<(), Arc<str>>;
    /// The caller proves this generation no longer needs its files: it
    /// was never published, or its script was read and library loaded.
    fn acknowledge(&mut self, generation: u64) -> Result<(), Arc<str>>;
    fn close(&mut self) -> Result<(), Arc<str>>;
    /// The caller has reaped the target or proved its old address space
    /// gone through exec. Never use this merely because detach was requested.
    fn close_after_target_exit(&mut self) -> Result<(), Arc<str>>;
}

#[derive(Debug)]
struct Files {
    process: OwnedFd,
    parent: OwnedFd,
    directory: OwnedFd,
    name: String,
    library: String,
    owned: BTreeMap<String, Option<Identity>>,
    scripts: BTreeMap<u64, String>,
    state: State,
}

#[derive(Debug)]
enum State {
    Open,
    Closing,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    const fn of(stat: &FileStat) -> Self {
        Self {
            device: stat.st_dev,
            inode: stat.st_ino,
        }
    }
}

fn problem(error: impl std::fmt::Display) -> Arc<str> {
    format!("Python agent file operation failed: {error}").into()
}

pub(super) fn create(pid: Pid, library: &[u8]) -> Result<Box<dyn AgentFiles>, Arc<str>> {
    Ok(Box::new(Files::new(pid, library)?))
}

const DIRECTORY: OFlag = OFlag::O_DIRECTORY.union(OFlag::O_CLOEXEC);

impl Files {
    fn new(pid: Pid, library: &[u8]) -> Result<Self, Arc<str>> {
        let mut files = Self::new_directory(pid)?;
        if let Err(reason) = files.write("hook.so", library) {
            return Err(match files.close() {
                Ok(()) => reason,
                Err(cleanup) => {
                    format!("{reason}; cleaning up the failed installation also failed: {cleanup}")
                        .into()
                }
            });
        }
        Ok(files)
    }

    fn new_directory(pid: Pid) -> Result<Self, Arc<str>> {
        // All later proc lookups are relative to this descriptor, so PID
        // reuse cannot redirect a credential check or root lookup.
        let process =
            open(format!("/proc/{pid}").as_str(), DIRECTORY, Mode::empty()).map_err(problem)?;
        credentials(&process)?;
        // `root` is intentionally a procfs magic link into the target.
        let root = openat(&process, "root", DIRECTORY, Mode::empty()).map_err(problem)?;
        // A symlink /tmp could resolve in the debugger's namespace.
        let parent =
            openat(&root, "tmp", DIRECTORY | OFlag::O_NOFOLLOW, Mode::empty()).map_err(problem)?;
        trusted_parent(
            &fstat(&parent).map_err(problem)?,
            rustix::process::geteuid().as_raw(),
        )?;
        executable_mount(fstatvfs(&parent).map_err(problem)?.flags())?;
        let mut random = [0; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut random))
            .map_err(problem)?;
        let name = format!(
            "{}{:032x}",
            crate::runtime_model::PYTHON_SCRIPT_PREFIX,
            u128::from_le_bytes(random)
        );
        mkdirat(&parent, name.as_str(), Mode::S_IRWXU).map_err(problem)?;
        let directory = match openat(
            &parent,
            name.as_str(),
            DIRECTORY | OFlag::O_NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(directory) => directory,
            Err(error) => {
                let cleanup = unlinkat(&parent, name.as_str(), UnlinkatFlags::RemoveDir);
                return Err(match cleanup {
                    Ok(()) => problem(error),
                    Err(cleanup) => problem(format!(
                        "{error}; removing the new directory also failed: {cleanup}"
                    )),
                });
            }
        };
        let files = Self {
            process,
            parent,
            directory,
            library: format!("/tmp/{name}/hook.so"),
            name,
            owned: BTreeMap::new(),
            scripts: BTreeMap::new(),
            state: State::Open,
        };
        files.validate()?;
        Ok(files)
    }

    fn validate(&self) -> Result<(), Arc<str>> {
        if !matches!(self.state, State::Open) {
            return Err("the Python agent file owner is closing or closed".into());
        }
        credentials(&self.process)?;
        let root = openat(&self.process, "root", DIRECTORY, Mode::empty()).map_err(problem)?;
        let parent =
            openat(&root, "tmp", DIRECTORY | OFlag::O_NOFOLLOW, Mode::empty()).map_err(problem)?;
        let directory = openat(
            &parent,
            self.name.as_str(),
            DIRECTORY | OFlag::O_NOFOLLOW,
            Mode::empty(),
        )
        .map_err(problem)?;
        for (held, current) in [(&self.parent, &parent), (&self.directory, &directory)] {
            let held = fstat(held).map_err(problem)?;
            let current = fstat(current).map_err(problem)?;
            if (held.st_dev, held.st_ino) != (current.st_dev, current.st_ino) {
                return Err("the target's root or agent directory changed; its agent paths are no longer valid".into());
            }
        }
        let own = rustix::process::geteuid().as_raw();
        trusted_parent(&fstat(&parent).map_err(problem)?, own)?;
        private_directory(&fstat(&directory).map_err(problem)?, own)?;
        executable_mount(fstatvfs(&directory).map_err(problem)?.flags())
    }

    fn write(&mut self, name: &str, bytes: &[u8]) -> Result<(), Arc<str>> {
        self.write_owned(name, |file| file.write_all(bytes))
    }

    /// The closure is the file-write boundary, also used to force partial
    /// writes in tests. An open failure owns nothing and deletes nothing.
    fn write_owned(
        &mut self,
        name: &str,
        write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
    ) -> Result<(), Arc<str>> {
        let fd = openat(
            &self.directory,
            name,
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::S_IRUSR | Mode::S_IWUSR,
        )
        .map_err(problem)?;
        // Record creation before asking for metadata, so even a failed
        // fstat leaves a bounded, explicitly unidentifiable artifact.
        self.owned.insert(name.to_owned(), None);
        let identity = Identity::of(&fstat(&fd).map_err(problem)?);
        self.owned.insert(name.to_owned(), Some(identity));
        let mut file = std::fs::File::from(fd);
        let written = file
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .and_then(|()| write(&mut file))
            .and_then(|()| file.sync_all());
        // A successful write is not enough on filesystems that defer an
        // error until close. close consumes the descriptor even on error.
        let closed = nix::unistd::close(file).map_err(std::io::Error::from);
        if let Err(error) = written.and(closed) {
            let reason = problem(error);
            if let Err(cleanup) = self.remove_owned(name) {
                return Err(format!(
                    "{reason}; removing its incomplete file also failed: {cleanup}"
                )
                .into());
            }
            return Err(reason);
        }
        Ok(())
    }

    fn remove_directory(&mut self) -> Result<(), Arc<str>> {
        let held = fstat(&self.directory).map_err(problem)?;
        match fstatat(
            &self.parent,
            self.name.as_str(),
            AtFlags::AT_SYMLINK_NOFOLLOW,
        ) {
            Ok(named) if (held.st_dev, held.st_ino) != (named.st_dev, named.st_ino) => {
                return Err(
                    "the agent directory was replaced; refusing to remove another directory".into(),
                );
            }
            Ok(_) => {}
            Err(Errno::ENOENT) => return self.directory_already_absent(),
            Err(error) => return Err(problem(error)),
        }
        match unlinkat(&self.parent, self.name.as_str(), UnlinkatFlags::RemoveDir) {
            Ok(()) => {
                self.state = State::Closed;
                Ok(())
            }
            Err(Errno::ENOENT) => self.directory_already_absent(),
            Err(Errno::ENOTEMPTY) => Err(problem(format!(
                "agent directory {} contains entries this owner did not create; they were retained",
                self.name
            ))),
            Err(error) => Err(problem(error)),
        }
    }

    fn directory_already_absent(&mut self) -> Result<(), Arc<str>> {
        if fstat(&self.directory).map_err(problem)?.st_nlink != 0 {
            return Err(
                "the agent directory was renamed; its new path is unknown and cannot be cleaned up"
                    .into(),
            );
        }
        self.state = State::Closed;
        Ok(())
    }

    fn remove_owned(&mut self, name: &str) -> Result<(), Arc<str>> {
        let Some(expected) = self.owned.get(name).copied() else {
            return Ok(());
        };
        match fstatat(&self.directory, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Err(Errno::ENOENT) => {
                self.owned.remove(name);
                return Ok(());
            }
            Err(error) => return Err(problem(error)),
            Ok(stat) if expected != Some(Identity::of(&stat)) => {
                return Err(format!("the identity of owned agent file {name} is unknown or changed; refusing to remove it").into());
            }
            Ok(_) => {}
        }
        // All target threads are stopped. Identity guards detect changes
        // made before this operation; they do not form a security boundary
        // against another process with our own UID racing name operations.
        match unlinkat(&self.directory, name, UnlinkatFlags::NoRemoveDir) {
            Ok(()) | Err(Errno::ENOENT) => {
                self.owned.remove(name);
                Ok(())
            }
            Err(error) => Err(problem(error)),
        }
    }
}

impl AgentFiles for Files {
    fn library(&self) -> Result<&str, Arc<str>> {
        self.validate()?;
        Ok(&self.library)
    }

    fn script(&mut self, generation: u64, source: &str) -> Result<String, Arc<str>> {
        self.validate()?;
        if !self.scripts.is_empty() {
            return Err("the previous Python agent script has not been acknowledged".into());
        }
        // A failed write may have left one owned incomplete file. Retire
        // it before preparing another; repeated failures cannot grow an
        // unbounded collection of artifacts.
        for name in self
            .owned
            .clone()
            .into_keys()
            .filter(|name| name != "hook.so")
        {
            self.remove_owned(&name)?;
        }
        let name = crate::runtime_model::python_script_name(generation);
        self.write(&name, source.as_bytes())?;
        let path = format!("/tmp/{}/{name}", self.name);
        debug_assert!(crate::runtime_model::is_python_script_filename(&path));
        self.scripts.insert(generation, name);
        Ok(path)
    }

    fn anchor(&mut self, thread: Pid) -> Result<(), Arc<str>> {
        let process =
            open(format!("/proc/{thread}").as_str(), DIRECTORY, Mode::empty()).map_err(problem)?;
        let previous = std::mem::replace(&mut self.process, process);
        let validated = self.validate();
        if validated.is_err() {
            self.process = previous;
        }
        validated
    }

    fn acknowledge(&mut self, generation: u64) -> Result<(), Arc<str>> {
        // The generation is consumed even if its file cannot be removed.
        // That file stays owned, bounding preparation, and close reports it.
        if let Some(name) = self.scripts.remove(&generation) {
            self.remove_owned(&name)?;
        }
        Ok(())
    }

    fn close(&mut self) -> Result<(), Arc<str>> {
        if matches!(self.state, State::Closed) {
            return Ok(());
        }
        if !self.scripts.is_empty() {
            return Err("preserving Python agent files: a request is still unacknowledged".into());
        }
        self.state = State::Closing;
        let mut problems = Vec::new();
        for name in self.owned.clone().into_keys() {
            if let Err(reason) = self.remove_owned(&name) {
                problems.push(reason.to_string());
            }
        }
        if problems.is_empty()
            && let Err(reason) = self.remove_directory()
        {
            problems.push(reason.to_string());
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; ").into())
        }
    }

    fn close_after_target_exit(&mut self) -> Result<(), Arc<str>> {
        self.scripts.clear();
        self.close()
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        let result = self.close();
        if result.is_err() {
            record!("agent file owner {}: {result:?}", self.library);
        }
    }
}

fn credentials(process: &OwnedFd) -> Result<(), Arc<str>> {
    let (effective, filesystem) = super::proc_status::effective_uids(process).map_err(problem)?;
    let own = rustix::process::geteuid().as_raw();
    if effective != own || filesystem != own {
        return Err("the target's effective or filesystem user differs from the debugger's; private agent files would be unreadable".into());
    }
    Ok(())
}

/// A writable shared parent must prevent other users from renaming our
/// directory. Its owner is trusted only when it is us or root.
fn trusted_parent(stat: &FileStat, own: u32) -> Result<(), Arc<str>> {
    if (stat.st_uid != 0 && stat.st_uid != own)
        || (stat.st_mode & 0o022 != 0 && stat.st_mode & 0o1000 == 0)
    {
        return Err("the target's agent parent directory has an untrusted owner or is writable by other users without the sticky bit".into());
    }
    Ok(())
}

fn private_directory(stat: &FileStat, own: u32) -> Result<(), Arc<str>> {
    if stat.st_uid != own || stat.st_mode & 0o777 != 0o700 {
        return Err("the target's agent directory must be owned by the debugger with private 0700 permissions".into());
    }
    Ok(())
}

fn executable_mount(flags: FsFlags) -> Result<(), Arc<str>> {
    if flags.contains(FsFlags::ST_NOEXEC) {
        return Err(
            "the target's agent directory is mounted noexec; its hook library cannot be loaded"
                .into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt as _;

    #[test]
    fn artifacts_are_private_acknowledged_and_removed_with_their_owner() {
        let pid = Pid::from_raw(i32::try_from(std::process::id()).expect("pid"));
        let mut files = create(pid, b"library").expect("private files");
        let library = std::path::PathBuf::from(files.library().expect("target-visible library"));
        let directory = library.parent().expect("directory").to_owned();
        assert_eq!(std::fs::metadata(&directory).unwrap().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&library).unwrap().mode() & 0o777, 0o600);
        let script = std::path::PathBuf::from(files.script(1, "source").expect("script"));
        assert_eq!(std::fs::metadata(&script).unwrap().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&script).unwrap(), "source");
        assert!(
            files.script(2, "other").is_err(),
            "a pending script is never overwritten"
        );
        files.acknowledge(0).expect("stale acknowledgement");
        assert!(script.exists());
        files.acknowledge(1).expect("acknowledge");
        assert!(!script.exists());
        assert!(library.exists());
        let unacknowledged =
            std::path::PathBuf::from(files.script(2, "second").expect("next generation"));
        files
            .close_after_target_exit()
            .expect("the caller proved target exit");
        drop(files);
        assert!(!unacknowledged.exists());
        assert!(!directory.exists());
    }

    fn files() -> Files {
        Files::new(Pid::this(), b"library").expect("private target files")
    }

    #[test]
    fn an_existing_script_name_is_never_deleted_or_acknowledged() {
        let mut files = files();
        let directory = std::path::Path::new(files.library().expect("target-visible library"))
            .parent()
            .unwrap()
            .to_owned();
        let occupied = directory.join("agent-1.py");
        std::fs::write(&occupied, "someone else's file").unwrap();
        assert!(files.script(1, "new source").is_err());
        assert_eq!(
            std::fs::read_to_string(&occupied).unwrap(),
            "someone else's file"
        );
        assert!(files.scripts.is_empty());
        files.acknowledge(1).unwrap();
        assert!(occupied.exists(), "an unowned file has no acknowledgement");
        std::fs::remove_file(&occupied).unwrap();
        let retry = files
            .script(1, "retry")
            .expect("no stale pending generation");
        assert_eq!(std::fs::read_to_string(retry).unwrap(), "retry");
        files.close_after_target_exit().unwrap();
        assert!(!directory.exists());
    }

    #[test]
    fn a_partial_file_write_is_removed_and_can_be_retried() {
        let mut files = files();
        let directory = std::path::Path::new(files.library().expect("target-visible library"))
            .parent()
            .unwrap()
            .to_owned();
        let reason = files
            .write_owned("agent-1.py", |file| {
                file.write_all(b"partial source")?;
                Err(std::io::Error::other("injected disk-full failure"))
            })
            .expect_err("a partial write fails");
        assert!(reason.contains("injected disk-full failure"));
        assert!(!directory.join("agent-1.py").exists());
        assert!(files.scripts.is_empty());
        let path = files
            .script(1, "complete source")
            .expect("retry after failure");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "complete source");
        files.close_after_target_exit().unwrap();
        assert!(!directory.exists());
    }

    #[test]
    fn a_script_symlink_is_not_followed_or_removed() {
        let mut files = files();
        let library = std::path::PathBuf::from(files.library().expect("target-visible library"));
        let link = library.parent().unwrap().join("agent-1.py");
        std::os::unix::fs::symlink(&library, &link).unwrap();
        assert!(files.script(1, "replace the library").is_err());
        assert!(link.is_symlink());
        assert_eq!(std::fs::read(&library).unwrap(), b"library");
        std::fs::remove_file(link).unwrap();
    }

    #[test]
    fn a_failed_write_and_failed_cleanup_are_both_reported() {
        let mut files = files();
        let path = std::path::Path::new(files.library().expect("target-visible library"))
            .parent()
            .unwrap()
            .join("incomplete");
        let reason = files
            .write_owned("incomplete", |_file| {
                // Replace our created file with a directory to force unlinkat's
                // cleanup to fail independently of the injected write error.
                std::fs::remove_file(&path)?;
                std::fs::create_dir(&path)?;
                Err(std::io::Error::other("injected write failure"))
            })
            .expect_err("write and cleanup fail");
        assert!(reason.contains("injected write failure"));
        assert!(reason.contains("removing its incomplete file also failed"));
        let next = files.script(2, "must not be published");
        if next.is_ok() {
            files.acknowledge(2).unwrap();
        }
        std::fs::remove_dir(path).unwrap();
        assert!(
            next.is_err(),
            "an unremoved incomplete file bounds further preparation"
        );
    }

    #[test]
    fn a_missing_target_is_an_error() {
        let reason = Files::new(Pid::from_raw(-1), b"library").unwrap_err();
        assert!(reason.contains("Python agent file operation failed"));
    }

    #[test]
    fn an_already_missing_script_can_still_be_acknowledged() {
        let mut files = files();
        let path = files.script(1, "source").unwrap();
        std::fs::remove_file(&path).unwrap();
        let result = files.acknowledge(1);
        let still_pending = !files.scripts.is_empty();
        // Leave the fixture clean even when exercising the broken draft.
        files.scripts.clear();
        drop(files);
        assert!(result.is_ok(), "already absent: {result:?}");
        assert!(
            !still_pending,
            "a consumed generation no longer blocks configuration"
        );
    }

    #[test]
    fn dropping_an_owner_does_not_remove_a_request_a_live_target_can_consume() {
        let mut files = files();
        let library = std::path::PathBuf::from(files.library().expect("target-visible library"));
        let directory = library.parent().unwrap().to_owned();
        let script = std::path::PathBuf::from(files.script(1, "source").unwrap());
        drop(files);
        let retained = (script.exists(), library.exists());
        // Clean up explicitly after checking the state, even on regression.
        let _ = std::fs::remove_file(script);
        let _ = std::fs::remove_file(library);
        let _ = std::fs::remove_dir(&directory);
        assert!(!directory.exists(), "test artifacts were removed");
        assert_eq!(
            retained,
            (true, true),
            "there was no acknowledgement or exit proof"
        );
    }

    #[test]
    fn closing_does_not_remove_a_replacement_directory() {
        let mut files = files();
        let directory = std::path::Path::new(files.library().unwrap())
            .parent()
            .unwrap()
            .to_owned();
        let moved = directory.with_extension("moved");
        std::fs::rename(&directory, &moved).unwrap();
        std::fs::create_dir(&directory).unwrap();
        assert!(files.library().is_err());
        assert!(files.script(1, "source").is_err());
        let closed = files.close();
        let replacement_retained = directory.is_dir();
        drop(files);
        let _ = std::fs::remove_file(moved.join("hook.so"));
        let _ = std::fs::remove_dir(&moved);
        let _ = std::fs::remove_dir(&directory);
        assert!(
            !moved.exists() && !directory.exists(),
            "test artifacts were removed"
        );
        assert!(
            closed.is_err(),
            "a moved owned directory needs an explicit cleanup error"
        );
        assert!(
            replacement_retained,
            "the directory at the original name belongs to someone else"
        );
    }

    #[test]
    fn closing_refuses_pending_files_and_is_idempotent_after_acknowledgement() {
        let mut files = files();
        let library = std::path::PathBuf::from(files.library().unwrap());
        let directory = library.parent().unwrap().to_owned();
        let script = std::path::PathBuf::from(files.script(1, "source").unwrap());
        assert!(files.close().unwrap_err().contains("unacknowledged"));
        assert!(script.exists() && library.exists());
        files.acknowledge(1).unwrap();
        files.close().unwrap();
        files.close().unwrap();
        assert!(!directory.exists());
        assert!(files.library().is_err());
        assert!(files.script(2, "source").is_err());
    }

    #[test]
    fn a_library_name_never_created_by_the_owner_is_preserved() {
        let mut files = Files::new_directory(Pid::this()).unwrap();
        let library = std::path::PathBuf::from(files.library().unwrap());
        let directory = library.parent().unwrap().to_owned();
        std::fs::write(&library, "foreign library").unwrap();
        assert!(files.write("hook.so", b"new library").is_err());
        drop(files);
        let kept = std::fs::read_to_string(&library);
        let _ = std::fs::remove_file(&library);
        let _ = std::fs::remove_dir(&directory);
        assert!(!directory.exists(), "test artifacts were removed");
        assert_eq!(kept.unwrap(), "foreign library");
    }

    #[test]
    fn a_noexec_mount_refuses_loading_the_native_hook() {
        assert!(executable_mount(FsFlags::empty()).is_ok());
        assert!(executable_mount(FsFlags::ST_NOSUID).is_ok());
        assert!(
            executable_mount(FsFlags::ST_NOEXEC | FsFlags::ST_NOSUID)
                .unwrap_err()
                .contains("noexec")
        );
    }

    #[test]
    fn a_directory_that_lost_its_private_mode_refuses_new_scripts() {
        let mut files = files();
        let directory = std::path::Path::new(files.library().unwrap())
            .parent()
            .unwrap()
            .to_owned();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777)).unwrap();
        let prepared = files.script(1, "source");
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        if prepared.is_ok() {
            files.acknowledge(1).unwrap();
        }
        files.close().unwrap();
        assert!(
            prepared.is_err(),
            "the directory was writable by other users"
        );
    }

    #[test]
    fn acknowledging_a_script_never_deletes_a_replacement_file() {
        let mut files = files();
        let library = std::path::PathBuf::from(files.library().unwrap());
        let directory = library.parent().unwrap().to_owned();
        let script = std::path::PathBuf::from(files.script(1, "source").unwrap());
        let replacement = directory.join("replacement");
        // Allocate its distinct inode while the owned script still exists.
        std::fs::write(&replacement, "foreign source").unwrap();
        std::fs::rename(replacement, &script).unwrap();
        let acknowledged = files.acknowledge(1);
        let kept = std::fs::read_to_string(&script);
        let _ = std::fs::remove_file(&script);
        files.close_after_target_exit().unwrap();
        assert!(
            acknowledged.is_err(),
            "identity changed before acknowledgement"
        );
        assert_eq!(kept.unwrap(), "foreign source");
    }

    #[test]
    fn a_failed_acknowledgement_reports_its_file_rather_than_a_pending_request() {
        let mut files = files();
        let library = std::path::PathBuf::from(files.library().unwrap());
        let script = std::path::PathBuf::from(files.script(1, "source").unwrap());
        let replacement = library.parent().unwrap().join("replacement");
        std::fs::write(&replacement, "foreign source").unwrap();
        std::fs::rename(replacement, &script).unwrap();
        let acknowledged = files.acknowledge(1);
        let pending = !files.scripts.is_empty();
        let prepared = files.script(2, "must not publish");
        if prepared.is_ok() {
            files.acknowledge(2).unwrap();
        }
        let closed = files.close();
        std::fs::remove_file(&script).unwrap();
        files.close().unwrap();
        assert!(acknowledged.is_err(), "the replacement was retained");
        assert!(!pending, "the caller proved the generation consumed");
        assert!(prepared.is_err(), "the retained file bounds preparation");
        let closed = closed.unwrap_err();
        assert!(
            !closed.contains("unacknowledged"),
            "cleanup names the retained file: {closed}"
        );
    }

    #[test]
    fn the_owner_outlives_the_thread_it_was_created_for() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            sender
                .send(rustix::thread::gettid().as_raw_nonzero())
                .unwrap();
            let _ = wait.recv();
        });
        let thread = Pid::from_raw(receiver.recv().unwrap().get());
        let mut files = Files::new(thread, b"library").expect("private files");
        drop(release);
        worker.join().unwrap();
        // The kernel releases the thread's root after the join wakes us.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::fs::read_link(format!("/proc/{thread}/root")).is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "{thread} never exited"
            );
            std::thread::yield_now();
        }
        assert!(files.library().is_err(), "validation needs a live thread");
        assert!(files.anchor(Pid::from_raw(-1)).is_err());
        files
            .anchor(Pid::this())
            .expect("another thread of the target");
        files
            .script(1, "source")
            .expect("a script after re-anchoring");
        files.acknowledge(1).unwrap();
        files.close().unwrap();
    }

    #[test]
    fn directory_trust_requires_a_known_owner_and_safe_parent_permissions() {
        let files = files();
        let mut stat = fstat(&files.parent).unwrap();
        for (owner, mode, allowed) in [
            (0, 0o1777, true),
            (1000, 0o1777, true),
            (1000, 0o755, true),
            (0, 0o3777, true),
            (1000, 0o777, false),
            (0, 0o770, false),
            (2000, 0o1777, false),
            (2000, 0o700, false),
        ] {
            stat.st_uid = owner;
            stat.st_mode = mode;
            assert_eq!(
                trusted_parent(&stat, 1000).is_ok(),
                allowed,
                "owner={owner} mode={mode:o}"
            );
        }
        for (owner, mode, allowed) in [
            (1000, 0o700, true),
            (1000, 0o2700, true),
            (1000, 0o755, false),
            (1000, 0o777, false),
            (1000, 0o600, false),
            (2000, 0o700, false),
        ] {
            stat.st_uid = owner;
            stat.st_mode = mode;
            assert_eq!(
                private_directory(&stat, 1000).is_ok(),
                allowed,
                "owner={owner} mode={mode:o}"
            );
        }
    }

    #[test]
    fn an_artifact_with_unavailable_identity_is_retained_and_bounds_preparation() {
        let mut files = files();
        let directory = std::path::Path::new(files.library().unwrap())
            .parent()
            .unwrap()
            .to_owned();
        let unknown = directory.join("unidentified.py");
        std::fs::write(&unknown, "source").unwrap();
        // Model the failed metadata read immediately after a successful
        // O_EXCL open, before any request could be handed to the caller.
        files.owned.insert("unidentified.py".into(), None);
        assert!(files.script(1, "more source").is_err());
        assert!(files.close().is_err());
        assert!(unknown.exists());
        std::fs::remove_file(unknown).unwrap();
        files.close().unwrap();
        assert!(!directory.exists());
    }

    #[test]
    fn failed_cleanup_prevents_publishing_more_paths_until_close_finishes() {
        let mut files = files();
        let library = std::path::PathBuf::from(files.library().unwrap());
        let directory = library.parent().unwrap().to_owned();
        let unowned = directory.join("unowned");
        std::fs::write(&unowned, "keep this").unwrap();
        let closed = files.close();
        let library_ready = files.library().is_ok();
        let prepared = files.script(1, "must not publish");
        if prepared.is_ok() {
            files.acknowledge(1).unwrap();
        }
        std::fs::remove_file(unowned).unwrap();
        files.close().unwrap();
        assert!(closed.is_err());
        assert!(!directory.exists(), "test artifacts were removed");
        assert!(!library_ready, "cleanup already removed the library");
        assert!(
            prepared.is_err(),
            "a closing owner cannot prepare another request"
        );
    }
}
