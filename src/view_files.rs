//! The view files a client loads for a session, besides those it is
//! given.
//!
//! They are the project's, every `*.views` file in `.microscope/views` at the
//! project root, and the user's, in `$XDG_CONFIG_HOME/microscope/views` or
//! `~/.config/microscope/views`, each directory's in name order. A project's
//! views come before the user's, being about the project's own types.

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use crate::view::syntax::MAX_FILE_BYTES;

/// One view file's name, as errors and `info view` give it, and text, with
/// the kernels and renderers beside it that its views call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewFile {
    pub name: String,
    pub text: String,
    pub kernels: Vec<KernelFile>,
    pub renderers: Vec<RendererFile>,
}

/// A renderer beside a view file, `NAME.js` for the renderer `NAME`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RendererFile {
    pub name: String,
    pub path: String,
    pub source: String,
}

/// A kernel beside a view file, `NAME.wasm` for the kernel `NAME`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelFile {
    pub name: String,
    pub path: String,
    pub module: Vec<u8>,
}

/// The project's and then the user's view files, and why any could not be
/// read. A directory that does not exist holds none.
#[must_use]
pub fn discover(project_root: &Path) -> (Vec<ViewFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut errors = Vec::new();
    let directories = [
        Some(project_root.join(".microscope/views")),
        user_directory(),
    ];

    for directory in directories.into_iter().flatten() {
        let (found, failed) = read_directory(&directory);
        files.extend(found);
        errors.extend(failed);
    }

    (files, errors)
}

/// Where the user's view files are.
#[must_use]
pub fn user_directory() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config.join("microscope/views"))
}

/// Reads one view file and the kernels and renderers beside it that its
/// views call.
///
/// It reads at most one byte more than a view file may hold, so that the
/// parser refuses one too long without reading all of it. A kernel or
/// renderer with no file beside it may be a built-in one.
pub fn read(path: &Path) -> Result<ViewFile, String> {
    let name = path.display().to_string();
    let bytes = read_at_most(path, MAX_FILE_BYTES)?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{name}: the file is not UTF-8"))?;
    let file = crate::view::syntax::parse(&name, &text);
    let mut called = file
        .views
        .iter()
        .flat_map(|view| view.kernel_names())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    called.sort();
    called.dedup();

    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let mut kernels = Vec::new();

    for kernel in called {
        let path = directory.join(format!("{kernel}.wasm"));
        if !path.exists() {
            continue;
        }

        kernels.push(KernelFile {
            name: kernel,
            path: path.display().to_string(),
            module: read_at_most(&path, crate::view::kernel::MAX_MODULE_BYTES)?,
        });
    }

    let mut drawn = file
        .views
        .iter()
        .flat_map(|view| view.renderer_names())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    drawn.sort();
    drawn.dedup();
    let mut renderers = Vec::new();

    for renderer in drawn {
        let path = directory.join(format!("{renderer}.js"));
        if !path.exists() {
            continue;
        }

        let source = read_at_most(&path, crate::view::MAX_RENDERER_BYTES)?;
        let name = path.display().to_string();
        renderers.push(RendererFile {
            name: renderer,
            source: String::from_utf8(source)
                .map_err(|_| format!("{name}: the file is not UTF-8"))?,
            path: name,
        });
    }

    Ok(ViewFile {
        name,
        text,
        kernels,
        renderers,
    })
}

/// A file's bytes, up to one more than `limit`, so that what reads them
/// can refuse one too long without reading all of it.
fn read_at_most(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let name = path.display();
    let file = fs::File::open(path).map_err(|error| format!("{name}: {error}"))?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{name}: {error}"))?;
    Ok(bytes)
}

/// Every `*.views` file in a directory, in name order.
fn read_directory(directory: &Path) -> (Vec<ViewFile>, Vec<String>) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (Vec::new(), Vec::new());
        }
        Err(error) => {
            return (
                Vec::new(),
                vec![format!("{}: {error}", directory.display())],
            );
        }
    };

    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "views")
        })
        .collect::<Vec<_>>();
    paths.sort();
    let mut files = Vec::new();
    let mut errors = Vec::new();

    for path in paths {
        match read(&path) {
            Ok(file) => files.push(file),
            Err(error) => errors.push(error),
        }
    }

    (files, errors)
}
