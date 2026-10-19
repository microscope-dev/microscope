//! Loads through the image cache, against the loads that wrote it.

use std::fs;
use std::path::{Path, PathBuf};

use super::*;
use crate::cache::{DEFAULT_CAPACITY, ImageCache, Key};
use crate::tools::dump::{Options, Section};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("build/test-programs")
        .join(name)
}

/// A directory removed when the test ends.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("microscope-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// A copy of `from`, a file or a directory, at `name`.
    fn copy(&self, from: &Path, name: &str) -> PathBuf {
        let to = self.join(name);
        let status = std::process::Command::new("cp")
            .arg("-r")
            .arg(from)
            .arg(&to)
            .status()
            .unwrap();
        assert!(status.success());
        to
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn search(directories: &[&Path]) -> super::super::DebugFileSearch {
    super::super::DebugFileSearch::new(&crate::DebugFileOptions {
        directories: directories.iter().map(|path| path.to_path_buf()).collect(),
        ..crate::DebugFileOptions::default()
    })
}

fn load(
    path: &Path,
    id: u32,
    search: &super::super::DebugFileSearch,
    cache: Option<&ImageCache>,
) -> (DebugInfo, CacheOutcome) {
    let data = fs::read(path).expect("run `just build-test-programs`");
    crate::pool::install(|| {
        load_debug_info(
            path,
            &data,
            crate::ModuleImageId::new(id),
            search,
            LoadLimits::default(),
            cache,
        )
    })
    .unwrap()
    .unwrap()
}

fn answers(info: &DebugInfo) -> String {
    let mut out = Vec::new();

    // The bytes agree, so this checks what binding adds, not every
    // address: the slowest sections, which ask about each, are left out.
    let options = Options {
        sections: Section::ALL
            .into_iter()
            .filter(|section| !matches!(section, Section::Addresses | Section::Breakpoints))
            .collect(),
        variable_addresses: 0,
    };

    crate::tools::dump::dump_info(info, &options, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

/// A load that hits the cache reads the bytes the load that missed wrote,
/// and answers every question as a load without a cache does, whether the
/// image came from the program's own file, a separate debug file, or one
/// that could not be used, and whether or not its DWARF shares a
/// supplementary file.
#[test]
fn a_cached_load_answers_as_an_uncached_one() {
    let split = fixture("split/basic-build-id");
    let root = fixture("split/debug-root");
    let altlink = fixture("split/altlink-root");

    // Binding is the same for every program, so small ones suffice: a
    // dump's cost grows with the program.
    let cases = [
        (fixture("variables-cpp-clang-o2"), search(&[])),
        (fixture("variables-rust-o2"), search(&[])),
        (fixture("callers-go-stripped"), search(&[])),
        (split.clone(), search(&[&root])),
        (split, search(&[&altlink])),
        // DWARF sharing a dwz supplementary file, in a separate debug file
        // and in the program's own.
        (
            fixture("dwz/gcc-o2/split/shapes"),
            search(&[&fixture("dwz/gcc-o2/debug-root")]),
        ),
        (fixture("dwz/clang-o2/dwz/shapes"), search(&[])),
    ];

    for (index, (path, search)) in cases.iter().enumerate() {
        let dir = ScratchDir::new(&format!("cached-load-{index}"));
        let cache = ImageCache::open(&dir.join("images"), DEFAULT_CAPACITY).unwrap();

        let (uncached, outcome) = load(path, 0, search, None);
        assert_eq!(outcome, CacheOutcome::Off);
        let (missed, outcome) = load(path, 0, search, Some(&cache));
        assert_eq!(outcome, CacheOutcome::Miss, "{}", path.display());
        let (hit, outcome) = load(path, 0, search, Some(&cache));
        assert_eq!(outcome, CacheOutcome::Hit, "{}", path.display());

        assert_eq!(uncached.image.image_bytes(), missed.image.image_bytes());
        assert_eq!(uncached.image.image_bytes(), hit.image.image_bytes());
        assert_eq!(
            uncached.image.separate_debug_file(),
            hit.image.separate_debug_file()
        );
        assert_eq!(answers(&uncached), answers(&hit), "{}", path.display());
    }
}

/// An image's bytes name none of what binds it to a session: a copy of a
/// program at another path, given another identifier, with its debug file
/// found in another directory, reads the same entry, bound to its own
/// path, identifier, and debug file.
#[test]
fn one_image_serves_every_binding() {
    let dir = ScratchDir::new("cached-bindings");
    let cache = ImageCache::open(&dir.join("images"), DEFAULT_CAPACITY).unwrap();
    std::fs::create_dir_all(dir.join("a")).unwrap();
    std::fs::create_dir_all(dir.join("b")).unwrap();
    let first = dir.copy(&fixture("split/basic-build-id"), "a/program");
    let second = dir.copy(&fixture("split/basic-build-id"), "b/renamed");
    let first_root = dir.copy(&fixture("split/debug-root"), "a/root");
    let second_root = dir.copy(&fixture("split/debug-root"), "b/root");

    let (one, outcome) = load(&first, 3, &search(&[&first_root]), Some(&cache));
    assert_eq!(outcome, CacheOutcome::Miss);
    let (two, outcome) = load(&second, 9, &search(&[&second_root]), Some(&cache));
    assert_eq!(outcome, CacheOutcome::Hit);
    let (uncached, _) = load(&first, 0, &search(&[&first_root]), None);
    assert_eq!(one.image.image_bytes(), uncached.image.image_bytes());
    assert_eq!(two.image.image_bytes(), uncached.image.image_bytes());

    for (info, path, root, id) in [
        (&one, &first, &first_root, 3),
        (&two, &second, &second_root, 9),
    ] {
        assert_eq!(info.image.path(), path);
        assert_eq!(info.image.id(), crate::ModuleImageId::new(id));
        let debug = info.image.debug_file().expect("the debug file is used");
        assert!(debug.starts_with(root), "{}", debug.display());
        let ty = info.image.types().next().expect("the program has types");
        assert_eq!(ty.reference().image, crate::ModuleImageId::new(id));
    }
}

/// The cache never hides a debug file: one found after a load without it
/// is another input, so another entry, and the entry without it serves
/// again once it is gone.
#[test]
fn a_debug_file_found_later_is_another_entry() {
    let dir = ScratchDir::new("cached-debug-later");
    let cache = ImageCache::open(&dir.join("images"), DEFAULT_CAPACITY).unwrap();
    let program = dir.copy(&fixture("split/basic-build-id"), "program");
    let root = dir.join("root");

    let (alone, outcome) = load(&program, 0, &search(&[&root]), Some(&cache));
    assert_eq!(outcome, CacheOutcome::Miss);
    assert_eq!(alone.image.separate_debug_file(), None);
    dir.copy(&fixture("split/debug-root"), "root");
    let (found, outcome) = load(&program, 0, &search(&[&root]), Some(&cache));
    assert_eq!(outcome, CacheOutcome::Miss);
    assert!(found.image.debug_file().is_some());
    assert!(found.image.functions().len() > alone.image.functions().len());
    let (again, outcome) = load(&program, 0, &search(&[&root]), Some(&cache));
    assert_eq!(outcome, CacheOutcome::Hit);
    assert!(again.image.debug_file().is_some());

    std::fs::remove_dir_all(&root).unwrap();
    let (gone, outcome) = load(&program, 0, &search(&[&root]), Some(&cache));
    assert_eq!(outcome, CacheOutcome::Hit);
    assert_eq!(gone.image.image_bytes(), alone.image.image_bytes());
}

/// A supplementary file is an input too: a debug file refused for want of
/// its supplementary file is read once the file is found.
#[test]
fn a_supplementary_file_found_later_is_another_entry() {
    let dir = ScratchDir::new("cached-supplementary-later");
    let cache = ImageCache::open(&dir.join("images"), DEFAULT_CAPACITY).unwrap();
    let program = fixture("dwz/gcc-o2/split/shapes");
    let root = dir.copy(&fixture("dwz/gcc-o2/debug-root"), "root");
    let moved = dir.join("shapes.dwz");
    std::fs::rename(root.join(".dwz/shapes"), &moved).unwrap();

    let (refused, outcome) = load(&program, 0, &search(&[&root]), Some(&cache));
    assert_eq!(outcome, CacheOutcome::Miss);
    assert!(matches!(
        refused.image.separate_debug_file(),
        Some(crate::DebugFile::Unusable { .. })
    ));
    std::fs::rename(&moved, root.join(".dwz/shapes")).unwrap();
    let (found, outcome) = load(&program, 0, &search(&[&root]), Some(&cache));
    assert_eq!(outcome, CacheOutcome::Miss);
    assert!(found.image.debug_file().is_some());
    let (again, outcome) = load(&program, 0, &search(&[&root]), Some(&cache));
    assert_eq!(outcome, CacheOutcome::Hit);
    assert_eq!(again.image.image_bytes(), found.image.image_bytes());
}

/// An entry that cannot be bound, as an image built without the debug
/// file its key names cannot, is unusable: the load replaces it with what
/// it builds.
#[test]
fn an_entry_that_disagrees_with_its_binding_is_replaced() {
    let dir = ScratchDir::new("cached-unbound");
    let cache = ImageCache::open(&dir.join("images"), DEFAULT_CAPACITY).unwrap();
    let program = fixture("split/basic-build-id");
    let root = fixture("split/debug-root");
    let (alone, _) = load(&program, 0, &search(&[]), None);
    assert_eq!(alone.image.separate_debug_file(), None);

    let data = fs::read(&program).unwrap();
    let object = object::File::parse(data.as_slice()).unwrap();
    let debug = search(&[&root]).find(&program, &object).found.unwrap();
    let key = Key::of(&[&data, &debug.data]);
    cache.put(key, alone.image.tables()).unwrap();

    let (replaced, outcome) = load(&program, 0, &search(&[&root]), Some(&cache));
    assert_eq!(outcome, CacheOutcome::Corrupt);
    assert!(replaced.image.debug_file().is_some());
    let (hit, outcome) = load(&program, 0, &search(&[&root]), Some(&cache));
    assert_eq!(outcome, CacheOutcome::Hit);
    assert_eq!(hit.image.image_bytes(), replaced.image.image_bytes());
}

/// An entry names its inputs' bytes, not their build-id or path: a program
/// rebuilt in place, or changed in any byte, is another entry, and the
/// original's still serves an unchanged copy.
#[test]
fn a_changed_program_is_another_entry() {
    let dir = ScratchDir::new("cached-changed");
    let cache = ImageCache::open(&dir.join("images"), DEFAULT_CAPACITY).unwrap();
    let program = dir.copy(&fixture("split/basic-build-id.full"), "program");
    let original = dir.copy(&fixture("split/basic-build-id.full"), "original");
    let (before, outcome) = load(&program, 0, &search(&[]), Some(&cache));
    assert_eq!(outcome, CacheOutcome::Miss);

    // Bytes past every section and segment change nothing the loader reads,
    // not even the build-id.
    let mut data = fs::read(&program).unwrap();
    data.extend_from_slice(b"rebuilt");
    fs::write(&program, &data).unwrap();
    let (after, outcome) = load(&program, 0, &search(&[]), Some(&cache));
    assert_eq!(outcome, CacheOutcome::Miss);
    assert_eq!(after.image.image_bytes(), before.image.image_bytes());
    let (_, outcome) = load(&original, 0, &search(&[]), Some(&cache));
    assert_eq!(outcome, CacheOutcome::Hit);
}
