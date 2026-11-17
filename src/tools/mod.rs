pub mod bpf_verified;
pub mod dump;
pub mod overhead;
pub mod snapshot_pause;
pub mod syscall_overhead;
pub mod unwind_oracle;

use std::path::{Path, PathBuf};

use rayon::prelude::*;

use serde::{Deserialize, Serialize};

use crate::timings::{Options, Recording, Report};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Measured {
    pub outcome: Result<(), String>,

    pub report: Report,

    pub retained_heap_bytes: Option<i64>,

    #[serde(default)]
    pub retained_parts: Vec<(String, i64)>,

    #[serde(default)]
    pub unkept_in_cache: Option<String>,
}

pub fn measure_load(path: &Path, instructions: bool) -> anyhow::Result<Measured> {
    let recording = Recording::start(Options { instructions })?;
    let before = crate::timings::alloc::totals();
    let loaded = crate::debug_info::load_module(
        path,
        crate::ModuleImageId::new(0),
        &crate::debug_info::DebugFileSearch::new(&crate::protocol::DebugFileOptions::default()),
    );
    let retained = crate::timings::alloc::totals().since(&before).live_bytes;
    let report = recording.finish()?;
    let counted = report.summary.allocations.is_some();
    let mut retained_parts = Vec::new();
    let mut unkept_in_cache = None;
    let outcome = loaded.map(|loaded| {
        unkept_in_cache = loaded.image.unkept_in_cache().map(ToString::to_string);
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
        unkept_in_cache,
    })
}

#[derive(Debug, Default)]
pub struct Warmed {
    pub loaded: usize,

    pub failed: Vec<(PathBuf, String)>,

    pub unkept: Vec<(PathBuf, String)>,
}

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
                    .map(|info| {
                        info.image
                            .unkept_in_cache()
                            .map(|why| (path.clone(), why.to_string()))
                    })
                    .map_err(|error| (path.clone(), error.to_string()))
            })
            .collect::<Vec<_>>()
    })?;

    let mut warmed = Warmed::default();
    for load in loads {
        match load {
            Ok(None) => warmed.loaded += 1,
            Ok(Some(unkept)) => warmed.unkept.push(unkept),
            Err(failure) => warmed.failed.push(failure),
        }
    }

    Ok(warmed)
}

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

    if read.is_ok() && header.starts_with(b"\x7fELF") && matches!(header[16..18], [2 | 3, 0]) {
        modules.push(path.to_owned());
    }

    Ok(())
}
