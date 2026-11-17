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
pub fn unkept_images(images: &[std::sync::Arc<ModuleImage>]) -> Vec<String> {
    let unkept = images
        .iter()
        .filter_map(|image| {
            let unkept = image.unkept_in_cache()?;
            Some((
                image.path().display().to_string(),
                format!("in {}", unkept.directory.display()),
                unkept.error.clone(),
            ))
        })
        .collect::<Vec<_>>();
    unkept_notes(
        unkept
            .iter()
            .map(|(module, location, error)| (module.as_str(), location.as_str(), error.as_str())),
        "session",
    )
}

#[must_use]
pub fn unkept_notes<'a>(
    unkept: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>,
    next: &str,
) -> Vec<String> {
    let mut causes = std::collections::BTreeMap::<(&str, &str), Vec<&str>>::new();
    for (module, location, error) in unkept {
        causes.entry((location, error)).or_default().push(module);
    }

    let name = |module: &&str| {
        std::path::Path::new(module).file_name().map_or_else(
            || (*module).to_owned(),
            |name| name.to_string_lossy().into_owned(),
        )
    };

    causes
        .into_iter()
        .filter_map(|((location, error), modules)| match modules.as_slice() {
            [] => None,
            [module] => Some(format!(
                "{module}'s image was not kept in the image cache {location}, so the next \
                 {next} reads its debug information again: {error}"
            )),
            [first, second, rest @ ..] => {
                let named = match rest {
                    [] => format!("{} and {}", name(first), name(second)),
                    [third] => format!("{}, {}, and {}", name(first), name(second), name(third)),
                    more => format!("{}, {}, and {} more", name(first), name(second), more.len()),
                };
                Some(format!(
                    "{} images were not kept in the image cache {location}, so the next {next} \
                     reads their debug information again: {error}. They are {named}",
                    modules.len()
                ))
            }
        })
        .collect()
}

#[must_use]
pub fn interprets_python(image: &ModuleImage) -> bool {
    image.python_runtime().is_some()
        || ["Py_BytesMain", "Py_Main"]
            .into_iter()
            .any(|entry| image.imports_function(entry))
}
