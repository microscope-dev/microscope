//! What a module's debug information leaves out, as every front end says
//! it: when a session starts, when a library loads, and in module lists.

use microscope::{DebugFile, DebugInformation, ModuleImage};

/// What became of the module's DWARF, when not all of it could be read.
#[must_use]
pub fn dwarf_problem(image: &ModuleImage) -> Option<String> {
    match image.debug_information() {
        DebugInformation::Absent | DebugInformation::Loaded => None,
        DebugInformation::Incomplete { reason } => {
            Some(format!("its debug information is incomplete: {reason}"))
        }
        DebugInformation::Unusable { reason } => Some(format!(
            "its debug information cannot be read, so only its symbols describe its code: {reason}"
        )),
    }
}

/// One warning, naming the module, for each part of its debug information
/// that could not be used. A program, unlike a library, also warns when it
/// has none: what it is debugged for is its own code, unless it is an
/// interpreter, such as CPython's, which runs the code it is debugged for.
#[must_use]
pub fn warnings(image: &ModuleImage, program: bool) -> Vec<String> {
    let mut problems = Vec::new();
    if let Some(DebugFile::Unusable { path, reason }) = image.separate_debug_file() {
        problems.push(format!(
            "cannot use its debug file {}: {reason}",
            path.display()
        ));
    }

    problems.extend(dwarf_problem(image));
    if program
        && problems.is_empty()
        && image.debug_information() == DebugInformation::Absent
        && image.functions().len() == 0
        && !interprets_python(image)
    {
        problems.push(
            "it has no debug information, so only its symbols describe its code: source lines, \
             variables, and types are unavailable"
                .to_owned(),
        );
    }

    problems
        .into_iter()
        .map(|problem| format!("{}: {problem}", image.path().display()))
        .collect()
}

/// Whether the program is CPython's interpreter: it carries the runtime, or
/// its `main` hands the command line to the runtime's library.
#[must_use]
pub fn interprets_python(image: &ModuleImage) -> bool {
    image.python_runtime().is_some()
        || ["Py_BytesMain", "Py_Main"]
            .into_iter()
            .any(|entry| image.imports_function(entry))
}
