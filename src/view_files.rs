use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use crate::view::syntax::MAX_FILE_BYTES;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewFile {
    pub name: String,
    pub text: String,
    pub kernels: Vec<KernelFile>,
    pub renderers: Vec<RendererFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RendererFile {
    pub name: String,
    pub path: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelFile {
    pub name: String,
    pub path: String,
    pub module: Vec<u8>,
}

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

#[must_use]
pub fn user_directory() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config.join("microscope/views"))
}

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

fn read_at_most(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let name = path.display();
    let file = fs::File::open(path).map_err(|error| format!("{name}: {error}"))?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{name}: {error}"))?;
    Ok(bytes)
}

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
