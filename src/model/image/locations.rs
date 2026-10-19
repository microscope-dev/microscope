//! The functions and lines a breakpoint location names in one image.
//!
//! A function location is first a function's whole name, as the debug
//! information spells it. Failing that, a function a package defines is
//! named as its language's programmers write it: by its local name within
//! the package (see [`crate::type_identity::functions`]), qualified by the
//! package's import path or name, or not qualified at all. A qualified
//! match wins over an unqualified one. Wrappers are never named, and a
//! location naming functions of more than one package or local name is
//! ambiguous rather than bound to all of them.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::type_identity::{NameSyntax, functions};
use crate::{Error, Result};

use crate::image::packages::PackageView;

use super::{
    CodeRole, Function, FunctionId, FunctionInfo, LineNumber, ModuleImage, SourceFileId,
    SourceLanguage,
};

/// A unit of code that a language names by an import path and, in its
/// own source, by a shorter name, such as a Go package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageInfo {
    /// The path other code imports the package by, such as `net/http`.
    pub path: Arc<str>,

    /// The name the package's code declares, such as `http`.
    pub name: Arc<str>,
}

/// Each function's package path and local name, function by function,
/// for those a package defines.
pub(super) fn packaged_names<'a>(
    functions: &'a [FunctionInfo],
    packages: &[PackageInfo],
) -> Vec<Option<(&'a str, String)>> {
    let paths = packages
        .iter()
        .map(|package| &*package.path)
        .collect::<BTreeSet<_>>();

    functions
        .iter()
        .map(|function| {
            if function.language == SourceLanguage::Ada {
                return functions::ada_packaged_name(&function.name);
            }

            functions::packaged_name(&function.name, NameSyntax::of(function.language), |path| {
                paths.contains(path)
            })
        })
        .collect()
}

/// The functions of `view` whose local name, qualified by their package's
/// path or name, is `plain`.
fn qualified(view: PackageView<'_>, plain: &str) -> Vec<FunctionId> {
    let mut found = Vec::new();
    for (dot, _) in plain.match_indices('.') {
        let (qualifier, local) = (&plain[..dot], &plain[dot + 1..]);
        for function in view.with_local_name(local) {
            let Some((package, _)) = view.packaged_name(function) else {
                debug_assert!(false, "validation checked {function:?} is packaged");
                continue;
            };

            if package == qualifier || view.package_name(package) == Some(qualifier) {
                found.push(function);
            }
        }
    }

    found
}

/// What makes two matched functions the same one: a package and local
/// name, which instantiations and inlined copies share, or else the
/// function itself.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Identity<'a> {
    Packaged(&'a str, &'a str),
    Function(FunctionId),
}

impl ModuleImage {
    /// Finds the functions with code that a breakpoint location names,
    /// keeping only those declared in `file` when it is given.
    ///
    /// The location is first a whole name, which every function so named
    /// matches, as overloads do in gdb. Failing that, a function a package
    /// defines matches its local name within the package, such as
    /// `Stack.Push` for `main.(*Stack[go.shape.int]).Push`, qualified by the
    /// package's import path or name, or else unqualified; receivers may
    /// be written with or without `(*…)`. Wrappers never match. A function
    /// that a package defines counts as declared in a file when any copy of
    /// it is, since copies inlined into other units may not say where they
    /// come from.
    ///
    /// Fails with [`Error::AmbiguousFunction`], naming each candidate by
    /// its package and local name, when the matches are more than one such
    /// function, and with [`Error::FunctionNotFound`] when nothing matches.
    pub fn functions_located(
        &self,
        location: &str,
        file: Option<SourceFileId>,
    ) -> Result<Vec<Function<'_>>> {
        let mut found = self
            .functions_named(location)
            .filter(|function| Self::locatable(*function))
            .collect::<Vec<_>>();
        if found.is_empty() {
            found = self.packaged_functions(location);
        }

        let names = self.packages();
        let identity = |function: &Function<'_>| {
            names.packaged_name(function.id()).map_or_else(
                || Identity::Function(function.id()),
                |(package, local)| Identity::Packaged(package, local),
            )
        };

        if let Some(file) = file {
            let declared = found
                .iter()
                .filter(|function| {
                    function
                        .declaration()
                        .is_some_and(|declaration| declaration.file == file)
                })
                .map(identity)
                .collect::<BTreeSet<_>>();
            found.retain(|function| declared.contains(&identity(function)));
        }

        let packaged = found
            .iter()
            .filter_map(|function| names.packaged_name(function.id()))
            .collect::<BTreeSet<_>>();
        if packaged.len() > 1 {
            return Err(Error::AmbiguousFunction {
                name: location.to_owned(),
                candidates: packaged
                    .into_iter()
                    .map(|(package, local)| format!("{package}.{local}"))
                    .collect(),
            });
        }

        if found.is_empty() {
            return Err(Error::FunctionNotFound(location.to_owned()));
        }

        Ok(found)
    }

    /// The import path of the package that defines a function, for
    /// languages with packages.
    #[must_use]
    pub fn function_package(&self, function: FunctionId) -> Option<&str> {
        Some(self.packages().packaged_name(function)?.0)
    }

    fn packages(&self) -> PackageView<'_> {
        PackageView::new(&self.tables)
    }

    /// The location an unqualified `location` means within `package`'s
    /// scope: qualified by the package, when nothing in the image has the
    /// whole name or a qualified name it spells and the package has a
    /// function of that local name. Code stopped in a package names its
    /// own functions unqualified, as its source does.
    #[must_use]
    pub fn location_in_package(&self, location: &str, package: &str) -> Option<String> {
        if self
            .functions_named(location)
            .any(|function| Self::locatable(function))
        {
            return None;
        }

        let plain = functions::plain_location(location)?;
        let names = self.packages();
        let locatable = |function: &FunctionId| {
            self.function(*function)
                .is_some_and(|function| Self::locatable(function))
        };

        if qualified(names, &plain).iter().any(locatable) {
            return None;
        }

        names
            .with_local_name(&plain)
            .filter(locatable)
            .any(|function| {
                names
                    .packaged_name(function)
                    .is_some_and(|(defining, _)| defining == package)
            })
            .then(|| format!("{package}.{location}"))
    }

    /// The functions that a package defines and that `location` names by
    /// their local names: qualified ones if any, otherwise unqualified.
    fn packaged_functions(&self, location: &str) -> Vec<Function<'_>> {
        let Some(plain) = functions::plain_location(location) else {
            return Vec::new();
        };

        let functions = |ids: &mut dyn Iterator<Item = FunctionId>| {
            ids.filter_map(|function| self.function(function))
                .filter(|function| Self::locatable(*function))
                .collect::<Vec<_>>()
        };

        let names = self.packages();
        let found = functions(&mut qualified(names, &plain).into_iter());
        if found.is_empty() {
            functions(&mut names.with_local_name(&plain))
        } else {
            found
        }
    }

    /// Whether a location can name a function: it has code, and is not a
    /// wrapper, which only forwards to the function a location means.
    fn locatable(function: Function<'_>) -> bool {
        function.role() != CodeRole::Wrapper && function.instances().next().is_some()
    }

    /// Whether a line breakpoint in a file stays at the line it asks for:
    /// the file's functions are Go, whose compiler marks a statement on
    /// every line that has code, so a line without one has no code to
    /// stop at and moving to another line would stop somewhere else.
    #[must_use]
    pub fn keeps_line_breakpoints(&self, file: SourceFileId) -> bool {
        self.functions().any(|function| {
            function.language() == SourceLanguage::Go
                && function
                    .declaration()
                    .as_ref()
                    .is_some_and(|declaration| declaration.file == file)
        })
    }

    /// The nearest lines of a file before and after `line` that have
    /// statements.
    #[must_use]
    pub fn nearest_statement_lines(
        &self,
        file: SourceFileId,
        line: LineNumber,
    ) -> (Option<LineNumber>, Option<LineNumber>) {
        let before = line
            .get()
            .checked_sub(1)
            .and_then(|previous| self.previous_statement_line(file, previous));
        let after = line
            .get()
            .checked_add(1)
            .and_then(LineNumber::new)
            .and_then(|next| self.next_statement_line(file, next));

        (before, after)
    }
}
