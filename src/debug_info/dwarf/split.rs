//! Split DWARF (`.dwo`) discovery, cache fingerprinting, and unit loading.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use gimli::{DwarfSections, EndianSlice, RunTimeEndian, Section as _};
use object::Object as _;
use rayon::prelude::*;

use super::{Reader, Sections, text};

/// Main `gimli::Dwarf` paired with any loaded `.dwo` split-unit sections.
///
/// `split_of` maps a `gimli::Unit` back to its owning `.dwo` by binary-searching
/// which `.debug_info.dwo` slice's host pointer range (`as_ptr_range`) contains
/// the unit's root DIE.
pub(in crate::debug_info) struct DwarfSet<'data> {
    main: gimli::Dwarf<Reader<'data>>,

    split: Vec<(std::ops::Range<usize>, gimli::Dwarf<Reader<'data>>)>,
}

impl<'data> std::ops::Deref for DwarfSet<'data> {
    type Target = gimli::Dwarf<Reader<'data>>;

    fn deref(&self) -> &Self::Target {
        &self.main
    }
}

impl<'data> From<gimli::Dwarf<Reader<'data>>> for DwarfSet<'data> {
    fn from(main: gimli::Dwarf<Reader<'data>>) -> Self {
        Self {
            main,
            split: Vec::new(),
        }
    }
}

impl<'data> DwarfSet<'data> {
    pub(super) fn split_of(
        &self,
        unit: &gimli::Unit<Reader<'data>>,
    ) -> Option<&gimli::Dwarf<Reader<'data>>> {
        if self.split.is_empty() {
            return None;
        }
        let at = unit.header.range_from(unit.header.root_offset()..).ok()?;
        let at = at.slice().as_ptr() as usize;
        let after = self.split.partition_point(|(range, _)| range.start <= at);
        let (range, dwarf) = self.split.get(after.checked_sub(1)?)?;
        range.contains(&at).then_some(dwarf)
    }

    pub(super) const fn main(&self) -> &gimli::Dwarf<Reader<'data>> {
        &self.main
    }
}

pub(super) fn is_skeleton(unit: &gimli::Unit<Reader<'_>>) -> bool {
    match unit.header.type_() {
        gimli::UnitType::Skeleton(_) => true,
        gimli::UnitType::Compilation => unit.header.version() < 5 && unit.dwo_id.is_some(),
        _ => false,
    }
}

pub(super) type Found = Result<(PathBuf, Vec<u8>), PathBuf>;

pub(super) fn read_files(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    units: &[gimli::Unit<Reader<'_>>],
    program: &Path,
) -> Vec<(usize, Found)> {
    let directory = program.parent().unwrap_or_else(|| Path::new("."));
    let named: Vec<(usize, Vec<PathBuf>)> = units
        .iter()
        .enumerate()
        .filter(|(_, unit)| is_skeleton(unit))
        .filter_map(|(index, unit)| {
            let name = unit.dwo_name().ok()??;
            let name = dwarf.attr_string(unit, name).ok()?;
            let name = PathBuf::from(&*text(name));
            let compiled = unit.comp_dir.map(|dir| PathBuf::from(&*text(dir)));
            Some((index, candidates(directory, compiled.as_deref(), &name)))
        })
        .collect();

    named
        .into_par_iter()
        .filter_map(|(index, paths)| {
            let first = paths.first()?.clone();
            let found = paths.into_iter().find_map(|path| {
                let bytes = crate::image::backing::read_named(&path).ok()?;
                Some((path, bytes))
            });
            Some((index, found.ok_or(first)))
        })
        .collect()
}

pub(super) fn fingerprint(object: &object::File<'_>, program: &Path) -> Option<Vec<u8>> {
    use std::os::unix::fs::MetadataExt as _;

    let endian = if object.is_little_endian() {
        RunTimeEndian::Little
    } else {
        RunTimeEndian::Big
    };
    let (sections, _) = super::load_sections(object, super::LoadLimits::default()).ok()?;
    let dwarf = sections.borrow(|section| EndianSlice::new(section, endian));
    let mut headers = dwarf.units();
    let first = headers.next().ok()??;
    if !is_skeleton(&dwarf.unit(first).ok()?) {
        return None;
    }
    let mut all = vec![first];
    while let Ok(Some(header)) = headers.next() {
        all.push(header);
    }

    let directory = program.parent().unwrap_or_else(|| Path::new("."));
    let found: Vec<Vec<u8>> = all
        .into_par_iter()
        .filter_map(|header| {
            let unit = dwarf.unit(header).ok()?;
            if !is_skeleton(&unit) {
                return None;
            }
            let name = dwarf.attr_string(&unit, unit.dwo_name().ok()??).ok()?;
            let name = PathBuf::from(&*text(name));
            let compiled = unit.comp_dir.map(|dir| PathBuf::from(&*text(dir)));
            let paths = candidates(directory, compiled.as_deref(), &name);
            let mut entry = Vec::new();
            match paths
                .iter()
                .find_map(|path| std::fs::metadata(path).ok().map(|meta| (path, meta)))
            {
                Some((path, meta)) => {
                    entry.extend_from_slice(path.as_os_str().as_encoded_bytes());
                    for value in [meta.len(), meta.ino(), meta.mtime().cast_unsigned()] {
                        entry.extend_from_slice(&value.to_le_bytes());
                    }
                    entry.extend_from_slice(&meta.mtime_nsec().to_le_bytes());
                }
                None => entry.extend_from_slice(b"missing\0"),
            }
            entry.push(0);
            Some(entry)
        })
        .collect();
    if found.is_empty() {
        return None;
    }
    let mut fingerprint = b"split dwarf\0".to_vec();
    for entry in found {
        fingerprint.extend_from_slice(&entry);
    }
    Some(fingerprint)
}

fn candidates(directory: &Path, compiled: Option<&Path>, name: &Path) -> Vec<PathBuf> {
    if name.is_absolute() {
        return vec![name.to_owned()];
    }
    let mut paths = Vec::with_capacity(3);
    match compiled {
        Some(compiled) if compiled.is_absolute() => paths.push(compiled.join(name)),
        Some(compiled) => paths.push(directory.join(compiled).join(name)),
        None => {}
    }
    paths.push(directory.join(name));
    if let Some(file) = name.file_name() {
        paths.push(directory.join(file));
    }
    paths.dedup();
    paths
}

pub(super) fn sections(bytes: &[u8]) -> Option<Sections<'_>> {
    let object = object::File::parse(bytes).ok()?;
    let mut wanted = Vec::new();
    DwarfSections::load(|id| {
        wanted.push(id);
        Ok::<_, ()>(())
    })
    .ok()?;

    let found = wanted
        .iter()
        .map(|id| id.dwo_name().and_then(|name| object.section_by_name(name)))
        .collect();
    let most = super::LoadLimits::default().most_decompressed;
    let mut data = crate::debug_info::compressed::read(found, most)
        .ok()?
        .into_iter();

    DwarfSections::load(|_| Ok::<_, ()>(data.next().unwrap_or(Cow::Borrowed(&[])))).ok()
}

pub(super) fn with_split_units<'data>(
    main: gimli::Dwarf<Reader<'data>>,
    mut units: Vec<gimli::Unit<Reader<'data>>>,
    files: &'data [(usize, Sections<'data>)],
    endian: RunTimeEndian,
) -> (DwarfSet<'data>, Vec<gimli::Unit<Reader<'data>>>) {
    let read: Vec<_> = files
        .par_iter()
        .filter_map(|(index, sections)| {
            let skeleton = units.get(*index)?;
            let mut dwo = sections.borrow(|section| EndianSlice::new(section, endian));
            dwo.make_dwo(&main);

            dwo.debug_line_str = main.debug_line_str;
            dwo.populate_abbreviations_cache(gimli::AbbreviationsCacheStrategy::Duplicates);
            let mut headers = dwo.units();
            let unit = loop {
                let header = headers.next().ok()??;
                if !matches!(
                    header.type_(),
                    gimli::UnitType::SplitCompilation(_) | gimli::UnitType::Compilation
                ) {
                    continue;
                }
                let unit = dwo.unit(header).ok()?;
                if unit.dwo_id == skeleton.dwo_id {
                    break unit;
                }
            };
            let mut unit = unit;
            unit.copy_relocated_attributes(skeleton);
            if unit.line_program.is_none() {
                unit.line_program.clone_from(&skeleton.line_program);
            }
            if unit.comp_dir.is_none() {
                unit.comp_dir = skeleton.comp_dir;
            }
            let info = dwo.debug_info.reader().slice().as_ptr_range();
            let range = info.start as usize..info.end as usize;
            Some((*index, unit, range, dwo))
        })
        .collect();

    let mut split = Vec::with_capacity(read.len());
    for (index, unit, range, dwo) in read {
        if let Some(slot) = units.get_mut(index) {
            *slot = unit;
        }
        split.push((range, dwo));
    }
    split.sort_unstable_by_key(|(range, _)| range.start);
    (DwarfSet { main, split }, units)
}
