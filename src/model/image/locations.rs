use std::collections::BTreeSet;
use std::sync::Arc;

use crate::type_identity::{NameSyntax, functions};
use crate::{Error, Result};

use crate::image::packages::PackageView;

use super::{
    CodeRole, Function, FunctionId, FunctionInfo, LineNumber, ModuleImage, SourceFileId,
    SourceLanguage,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageInfo {
    pub path: Arc<str>,

    pub name: Arc<str>,
}

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

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Identity<'a> {
    Packaged(&'a str, &'a str),
    Function(FunctionId),
}

impl ModuleImage {
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

    #[must_use]
    pub fn function_package(&self, function: FunctionId) -> Option<&str> {
        Some(self.packages().packaged_name(function)?.0)
    }

    fn packages(&self) -> PackageView<'_> {
        PackageView::new(&self.tables)
    }

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

    fn locatable(function: Function<'_>) -> bool {
        function.role() != CodeRole::Wrapper && function.instances().next().is_some()
    }

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
