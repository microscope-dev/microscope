use microscope::{DebugFile, DebugInformation, ModuleImage};

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

#[must_use]
pub fn interprets_python(image: &ModuleImage) -> bool {
    image.python_runtime().is_some()
        || ["Py_BytesMain", "Py_Main"]
            .into_iter()
            .any(|entry| image.imports_function(entry))
}
