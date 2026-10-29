//! Symbols for a profile recorded without them, as on a production host
//! that keeps its debug information elsewhere: wherever the profile is
//! opened, each module is found by its recorded file, while that is still
//! the bytes recorded, or else by its build id, in the debug directories or
//! from a debuginfod server.

use std::path::PathBuf;

use super::Profile;
use super::record::{OwnedLine, symbolize};
use crate::capture::CaptureBuilder;
use crate::capture::records::{FrameKind, NONE};
use crate::debug_info::{DebugFileSearch, DebugInfo};
use crate::{ImageAddress, ModuleImageId};

/// What became of one module's symbols.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleSymbols {
    /// The module's path when it was mapped.
    pub module: String,

    /// The file its symbols were read from, or why none was found.
    pub source: Result<PathBuf, String>,

    /// How many of its frames gained lines.
    pub frames: u64,
}

impl Profile {
    /// The modules some code frame of which has no lines, by row. A module
    /// the kernel maps without a file, as `[vdso]`, has none to look for.
    fn unsymbolized_modules(&self) -> Vec<u32> {
        let capture = &self.capture;
        let mut modules = capture
            .frames
            .iter()
            .enumerate()
            .filter(|(frame, row)| {
                row.kind == FrameKind::Native as u8
                    && row.module.get() != NONE
                    && capture
                        .frame_symbols
                        .get(*frame)
                        .is_none_or(|range| range.count.get() == 0)
            })
            .map(|(_, row)| row.module.get())
            .filter(|&module| {
                capture.modules.get(module as usize).is_some_and(|row| {
                    !capture.string(row.path.get()).starts_with('[')
                })
            })
            .collect::<Vec<_>>();
        modules.sort_unstable();
        modules.dedup();
        modules
    }

    /// Whether the recording left symbolizing for later.
    #[must_use]
    pub fn symbols_later(&self) -> bool {
        self.capture.setting("symbols") == Some("later")
    }

    /// Gives every code frame without lines the lines of its module, found
    /// where `options` say to look, and says what each such module's
    /// search found. Frames of a module that cannot be found keep their
    /// addresses.
    pub fn symbolize(&mut self, options: &crate::DebugFileOptions) -> Vec<ModuleSymbols> {
        let modules = self.unsymbolized_modules();
        if modules.is_empty() {
            return Vec::new();
        }

        let search = &DebugFileSearch::new(options);
        let capture = &self.capture;
        let mut builder = CaptureBuilder::new();
        builder.absorb(capture);
        let mut found = Vec::with_capacity(modules.len());

        for (number, module) in modules.into_iter().enumerate() {
            let row = &capture.modules[module as usize];
            let path = capture.string(row.path.get()).to_owned();
            let build_id = (row.build_id.get() != NONE).then(|| capture.string(row.build_id.get()));
            let id = ModuleImageId::new(u32::try_from(number).unwrap_or(u32::MAX));
            let located = locate(&path, build_id, row.digest, id, search);
            let frames = located.as_ref().map_or(0, |(_, info)| {
                let new = builder.module(&path, build_id, row.digest, row.size.get());
                symbolize_module(&mut builder, new, info)
            });
            found.push(ModuleSymbols {
                module: path,
                source: located.map(|(file, _)| file),
                frames,
            });
        }

        builder.setting("symbols", "recorded");
        self.capture = builder.finish();
        found
    }
}

/// Gives each frame of `module` in `builder` without lines its lines in
/// `info`, returning how many gained some.
fn symbolize_module(builder: &mut CaptureBuilder, module: u32, info: &DebugInfo) -> u64 {
    let frames: Vec<(u32, u64)> = builder
        .frames()
        .iter()
        .enumerate()
        .filter(|(_, frame)| frame.kind == FrameKind::Native as u8 && frame.module.get() == module)
        .filter_map(|(index, frame)| Some((u32::try_from(index).ok()?, frame.address.get())))
        .filter(|&(frame, _)| !builder.symbolized(frame))
        .collect();
    let mut symbolized = 0;

    for (frame, address) in frames {
        let lines = symbolize(&info.image, ImageAddress::new(address));
        if lines.is_empty() {
            continue;
        }

        let inputs: Vec<_> = lines.iter().map(OwnedLine::input).collect();
        builder.symbolize(frame, module, &inputs);
        symbolized += 1;
    }

    symbolized
}

/// The debug information of the module recorded at `path` with `digest`:
/// from that file while it holds the same bytes, and otherwise from the
/// file its build id names.
fn locate(
    path: &str,
    build_id: Option<&str>,
    digest: [u8; 16],
    id: ModuleImageId,
    search: &DebugFileSearch,
) -> Result<(PathBuf, DebugInfo), String> {
    let path = PathBuf::from(path);
    let mut why = match crate::image::backing::read_input(&path) {
        Ok((data, _)) if twox_hash::XxHash3_128::oneshot(&data).to_le_bytes() == digest => {
            return crate::debug_info::load_module_bytes(&path, &data, id, search)
                .map(|info| (path, info))
                .map_err(|error| error.to_string());
        }
        Ok(_) => "its file has changed since it was recorded".to_owned(),
        Err(_) => "its file is gone".to_owned(),
    };

    let Some(build_id) = build_id.and_then(unhex) else {
        why.push_str(", and it has no build id");
        return Err(why);
    };

    let Some(file) = search.with_build_id(&build_id) else {
        why.push_str(", and no debug directory or server has its build id");
        return Err(why);
    };

    crate::debug_info::load_module_bytes(&file.path, &file.data, id, search)
        .map(|info| (file.path, info))
        .map_err(|error| error.to_string())
}

/// The bytes a hex string spells.
fn unhex(text: &str) -> Option<Vec<u8>> {
    (text.len().is_multiple_of(2) && !text.is_empty()).then_some(())?;
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A module the kernel maps without a file, as `[vdso]`, has no file
    /// or build id to find its lines by: it keeps its addresses without a
    /// word, where a module whose file is gone says why.
    #[test]
    fn the_kernels_own_mappings_are_not_looked_for() {
        let mut builder = CaptureBuilder::new();
        let thread = builder.thread(crate::model::Pid::new(1), crate::model::Tid::new(1), 0);
        let frames: Vec<u32> = ["[vdso]", "/gone/basic"]
            .into_iter()
            .map(|path| {
                let module = builder.module(path, None, [0; 16], 4096);
                builder.frame(FrameKind::Native, module, 0x10, 0)
            })
            .collect();
        let stack = builder.stack(&frames);
        builder.sample(thread, 0, 1, stack, NONE);
        let mut profile = Profile {
            capture: builder.finish(),
        };

        let found: Vec<_> = profile
            .symbolize(&crate::DebugFileOptions::default())
            .into_iter()
            .map(|module| module.module)
            .collect();
        assert_eq!(found, ["/gone/basic"]);
    }
}
