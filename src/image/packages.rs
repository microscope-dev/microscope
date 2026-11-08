//! Packages, and the functions they define by their names within them,
//! which breakpoint locations name as their language's programmers write
//! them (see `crate::model::image::locations`).

use zerocopy::little_endian::U32;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::index::{self, NameEntry};
use super::strings::{StrId, Strings, StringsBuilder};
use super::{Builder, Image, NONE, Record, TableKind};
use crate::FunctionId;

/// One package, ordered by its path's bytes.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct PackageRecord {
    pub path: U32,
    pub name: U32,
}

impl Record for PackageRecord {
    const KIND: TableKind = TableKind::Packages;
}

/// One function's package path and local name, or [`NONE`] for both when
/// no package defines it, function by function.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct PackagedRecord {
    pub package: U32,
    pub local: U32,
}

impl Record for PackagedRecord {
    const KIND: TableKind = TableKind::PackagedNames;
}

/// Why packages could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the packages do not fit an image")]
pub struct TooMany;

/// Adds `packages`, as (path, name) pairs, and each function's package
/// and local name in `names`, function by function, to `builder`.
pub fn add_to<'s>(
    builder: &mut Builder<'_>,
    strings: &mut StringsBuilder,
    packages: impl IntoIterator<Item = (&'s str, &'s str)>,
    names: &[Option<(&str, String)>],
) -> Result<(), TooMany> {
    let mut push = |text: &str| strings.push(text).ok_or(TooMany);

    // A path named twice keeps its last name.
    let mut paths = packages.into_iter().collect::<Vec<_>>();
    paths.reverse();
    paths.sort_by_key(|(path, _)| *path);
    paths.dedup_by_key(|(path, _)| *path);
    let packages = paths
        .into_iter()
        .map(|(path, name)| {
            Ok(PackageRecord {
                path: push(path)?.0.into(),
                name: push(name)?.0.into(),
            })
        })
        .collect::<Result<Vec<_>, TooMany>>()?;
    let mut locals = Vec::new();
    let mut records = Vec::with_capacity(names.len());
    for (function, name) in names.iter().enumerate() {
        records.push(match name {
            Some((package, local)) => {
                let local_id = push(local)?;
                let function = u32::try_from(function).map_err(|_| TooMany)?;
                locals.push((local.as_str(), local_id, function));
                PackagedRecord {
                    package: push(package)?.0.into(),
                    local: local_id.0.into(),
                }
            }
            None => PackagedRecord {
                package: NONE.into(),
                local: NONE.into(),
            },
        });
    }

    builder
        .owned_table(packages)
        .owned_table(records)
        .owned_shared(TableKind::LocalNames, index::names(locals));
    Ok(())
}

/// The packages of a validated image.
#[derive(Debug, Clone, Copy)]
pub struct PackageView<'a> {
    strings: Strings<'a>,
    packages: &'a [PackageRecord],
    packaged: &'a [PackagedRecord],
    locals: &'a [NameEntry],
}

impl<'a> PackageView<'a> {
    pub fn new(image: &'a Image) -> Self {
        Self {
            strings: image.strings(),
            packages: image.table(),
            packaged: image.table(),
            locals: image.shared(TableKind::LocalNames),
        }
    }

    /// The name the package at `path` declares.
    pub fn package_name(self, path: &str) -> Option<&'a str> {
        let index = self
            .packages
            .binary_search_by(|package| {
                self.strings
                    .bytes(StrId(package.path.get()))
                    .cmp(path.as_bytes())
            })
            .ok()?;
        Some(
            self.strings
                .get(StrId(self.packages.get(index)?.name.get())),
        )
    }

    /// The path of the package defining `function`, and the function's
    /// name within it.
    pub fn packaged_name(self, function: FunctionId) -> Option<(&'a str, &'a str)> {
        let record = self.packaged.get(function.index())?;
        (record.local.get() != NONE).then(|| {
            (
                self.strings.get(StrId(record.package.get())),
                self.strings.get(StrId(record.local.get())),
            )
        })
    }

    /// The functions whose local name is `local`, in order.
    pub fn with_local_name(self, local: &str) -> impl Iterator<Item = FunctionId> + 'a {
        index::named(self.strings, self.locals, local).map(FunctionId::new)
    }
}

/// Checks the packages and packaged names.
pub(super) fn validate(image: &Image) -> Result<(), String> {
    let strings = image.strings();
    let packages = image.table::<PackageRecord>();
    if !packages.iter().all(|package| {
        strings.contains(StrId(package.path.get())) && strings.contains(StrId(package.name.get()))
    }) || !packages.is_sorted_by(|earlier, later| {
        strings.bytes(StrId(earlier.path.get())) < strings.bytes(StrId(later.path.get()))
    }) {
        return Err("a package is malformed or out of order".into());
    }

    let functions = image.table::<super::functions::FunctionRecord>().len();
    let records = image.table::<PackagedRecord>();
    if !(records.is_empty() || records.len() == functions)
        || !records.iter().all(|record| {
            if record.local.get() == NONE {
                record.package.get() == NONE
            } else {
                strings.contains(StrId(record.local.get()))
                    && strings.contains(StrId(record.package.get()))
            }
        })
    {
        return Err("a packaged name is malformed".into());
    }

    // The index names each packaged function once, by its local name.
    let locals = image.shared::<NameEntry>(TableKind::LocalNames);
    let named = records
        .iter()
        .filter(|record| record.local.get() != NONE)
        .count();
    let mut seen = vec![false; records.len()];
    if !index::valid_names(&strings, locals, records.len())
        || locals.len() != named
        || !locals.iter().all(|entry| {
            let function = entry.value.get() as usize;
            seen.get_mut(function)
                .is_some_and(|seen| !std::mem::replace(seen, true))
                && records
                    .get(function)
                    .is_some_and(|record| record.local == entry.name)
        })
    {
        return Err("the local name index disagrees".into());
    }

    Ok(())
}
