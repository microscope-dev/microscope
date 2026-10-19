//! Developer tools behind `microscope-tools`: a canonical dump of every answer
//! a program's debug information gives, one measured load for benchmarks,
//! and filling the image cache.

pub mod dump;
pub mod overhead;
pub mod syscall_overhead;

use std::path::{Path, PathBuf};

use rayon::prelude::*;

use serde::{Deserialize, Serialize};

use crate::timings::{Options, Recording, Report};

/// One load of one program, measured in a process of its own.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Measured {
    /// Whether the load succeeded, or why it failed.
    pub outcome: Result<(), String>,

    /// What the load cost.
    pub report: Report,

    /// The heap the loaded debug information holds, net of what the load
    /// freed.
    pub retained_heap_bytes: Option<i64>,

    /// What freeing each part of the loaded debug information returns, in
    /// order: the variable provider, the unwinder, then the image and what
    /// they shared with it.
    #[serde(default)]
    pub retained_parts: Vec<(String, i64)>,
}

/// Loads `path` as the debugger loads a module, recording what it costs.
pub fn measure_load(path: &Path, instructions: bool) -> anyhow::Result<Measured> {
    let recording = Recording::start(Options { instructions })?;
    let before = crate::timings::alloc::totals();
    let loaded = crate::debug_info::load_module(
        path,
        crate::ModuleImageId::new(0),
        &crate::debug_info::DebugFileSearch::default(),
    );
    let retained = crate::timings::alloc::totals().since(&before).live_bytes;
    let report = recording.finish()?;
    let counted = report.summary.allocations.is_some();
    let mut retained_parts = Vec::new();
    let outcome = loaded.map(|loaded| {
        let live = || crate::timings::alloc::totals().live_bytes;
        let mut freed = |name: &str, part: Box<dyn FnOnce() + '_>| {
            let held = live();
            part();
            retained_parts.push((name.to_owned(), held - live()));
        };

        let crate::debug_info::DebugInfo {
            image,
            unwind,
            variables,
        } = loaded;
        freed("variables", Box::new(move || drop(variables)));
        freed("unwind", Box::new(move || drop(unwind)));
        freed("image", Box::new(move || drop(image)));
    });

    Ok(Measured {
        outcome: outcome.map_err(|error| error.to_string()),
        report,
        retained_heap_bytes: counted.then_some(retained),
        retained_parts: if counted { retained_parts } else { Vec::new() },
    })
}

/// What [`warm`] did.
#[derive(Debug, Default)]
pub struct Warmed {
    /// How many modules loaded.
    pub loaded: usize,

    /// The modules that did not, and why.
    pub failed: Vec<(PathBuf, String)>,
}

/// Loads every program and shared library under `paths`, in parallel, as
/// the debugger would, so that the process's image cache holds them.
pub fn warm(paths: &[PathBuf]) -> anyhow::Result<Warmed> {
    let mut modules = Vec::new();
    for path in paths {
        collect_modules(path, &mut modules)?;
    }

    modules.sort();
    let search = crate::debug_info::DebugFileSearch::new(&crate::DebugFileOptions::default());
    let loads = crate::pool::install(|| {
        modules
            .par_iter()
            .map(|path| {
                crate::debug_info::load_module(path, crate::ModuleImageId::new(0), &search)
                    .map(drop)
                    .map_err(|error| (path.clone(), error.to_string()))
            })
            .collect::<Vec<_>>()
    })?;

    let mut warmed = Warmed::default();
    for load in loads {
        match load {
            Ok(()) => warmed.loaded += 1,
            Err(failure) => warmed.failed.push(failure),
        }
    }

    Ok(warmed)
}

/// Adds `path`, or every file under it, that is an ELF program or shared
/// library: not a relocatable object or a core dump.
fn collect_modules(path: &Path, modules: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)? {
            collect_modules(&entry?.path(), modules)?;
        }

        return Ok(());
    }

    if !metadata.is_file() {
        return Ok(());
    }

    let mut header = [0_u8; 18];
    let read = std::io::Read::read_exact(&mut std::fs::File::open(path)?, &mut header);

    // ET_EXEC and ET_DYN, little-endian.
    if read.is_ok() && header.starts_with(b"\x7fELF") && matches!(header[16..18], [2 | 3, 0]) {
        modules.push(path.to_owned());
    }

    Ok(())
}
