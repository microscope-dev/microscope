use std::os::unix::fs::PermissionsExt as _;

use super::*;
use crate::image::TableKind;

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
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A small valid image whose bytes say `views`.
fn image(views: &[u8]) -> Image {
    let mut builder = crate::image::Builder::new(crate::TargetDescription::X86_64);
    builder.bytes(TableKind::EmbeddedViews, views.to_vec());
    builder.seal(Limits::default()).unwrap()
}

fn bytes_of(lookup: Lookup) -> Vec<u8> {
    match lookup {
        Lookup::Hit { image, .. } => image.as_bytes().to_vec(),
        other => panic!("{other:?}"),
    }
}

/// Files in the cache's directory, by name.
fn files(cache: &ImageCache) -> Vec<String> {
    let mut names = std::fs::read_dir(cache.directory())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn set_modified(path: &Path, seconds: i64) {
    let time = nix::sys::time::TimeSpec::new(seconds, 0);
    nix::sys::stat::utimensat(
        nix::fcntl::AT_FDCWD,
        path,
        &time,
        &time,
        nix::sys::stat::UtimensatFlags::FollowSymlink,
    )
    .unwrap();
}

/// A key names its inputs' exact bytes in order, so inputs split
/// differently, reordered, or with an empty one added are other keys; an
/// entry reads back as it was written.
#[test]
fn an_entry_reads_back_under_the_key_of_exactly_its_inputs() {
    let keys = [
        Key::of(&[b"ab", b"c"]),
        Key::of(&[b"a", b"bc"]),
        Key::of(&[b"abc"]),
        Key::of(&[b"c", b"ab"]),
        Key::of(&[b"ab", b"c", b""]),
        Key::of(&[]),
    ];

    for (index, key) in keys.iter().enumerate() {
        assert!(!keys[..index].contains(key), "{index}");
    }

    assert_eq!(Key::of(&[b"ab", b"c"]), keys[0]);

    let dir = ScratchDir::new("cache-read-back");
    let cache = ImageCache::open(&dir.join("images"), DEFAULT_CAPACITY).unwrap();
    assert!(matches!(cache.get(keys[0]), Lookup::Miss));
    let (first, second) = (image(b"first"), image(b"second"));
    cache.put(keys[0], &first).unwrap();
    cache.put(keys[1], &second).unwrap();
    assert_eq!(bytes_of(cache.get(keys[0])), first.as_bytes());
    assert_eq!(bytes_of(cache.get(keys[1])), second.as_bytes());

    // A later write replaces the entry whole.
    cache.put(keys[0], &second).unwrap();
    assert_eq!(bytes_of(cache.get(keys[0])), second.as_bytes());
    let mut expected = [format!("{}.image", keys[0]), format!("{}.image", keys[1])];
    expected.sort();
    assert_eq!(files(&cache), expected);
    let mode = std::fs::metadata(cache.directory())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700, "the cache is its user's alone");
}

/// An entry damaged in any way is a miss that says why, and is removed so
/// that the next load writes a good one.
#[test]
fn a_damaged_entry_is_reported_and_removed() {
    let dir = ScratchDir::new("cache-damaged");
    let cache = ImageCache::open(&dir.join("images"), DEFAULT_CAPACITY).unwrap();
    let good = image(b"views").as_bytes().to_vec();
    let mut flipped = good.clone();
    flipped[good.len() / 2] ^= 1;
    let damaged = [
        ("flipped", flipped),
        ("truncated", good[..good.len() / 2].to_vec()),
        ("empty", Vec::new()),
        ("not an image", b"#!/bin/sh\n".repeat(20)),
    ];

    for (name, bytes) in damaged {
        let key = Key::of(&[name.as_bytes()]);
        std::fs::write(cache.path(key), bytes).unwrap();
        assert!(matches!(cache.get(key), Lookup::Corrupt(_)), "{name}");
        assert!(!cache.path(key).exists(), "{name}");
        assert!(matches!(cache.get(key), Lookup::Miss), "{name}");
    }
}

/// A reader removes only the file it found unusable: one another writer
/// renamed into place since is a good entry, and stays.
#[test]
fn a_reader_removes_only_the_entry_it_found_unusable() {
    let dir = ScratchDir::new("cache-replaced");
    let cache = ImageCache::open(&dir.join("images"), DEFAULT_CAPACITY).unwrap();
    let key = Key::of(&[b"program"]);
    std::fs::write(cache.path(key), b"damaged").unwrap();
    let (_, found) = crate::image::backing::read_input(&cache.path(key)).unwrap();
    let good = image(b"views");
    cache.put(key, &good).unwrap();
    cache.discard(key, &found);
    assert_eq!(bytes_of(cache.get(key)), good.as_bytes());
    let Lookup::Hit { stamp, .. } = cache.get(key) else {
        panic!("a hit")
    };

    cache.discard(key, &stamp);
    assert!(matches!(cache.get(key), Lookup::Miss));
}

/// Opening a cache evicts the entries used longest ago, and files writers
/// left behind, until it fits; reading an entry counts as using it, and
/// files the cache did not write are never touched.
#[test]
fn opening_evicts_the_entries_used_longest_ago() {
    let dir = ScratchDir::new("cache-evicts");
    let directory = dir.join("images");
    let cache = ImageCache::open(&directory, DEFAULT_CAPACITY).unwrap();
    let entry = image(b"views");
    let length = entry.as_bytes().len() as u64;
    let keys = (0..4_u8).map(|n| Key::of(&[&[n]])).collect::<Vec<_>>();

    for (age, key) in keys.iter().enumerate() {
        cache.put(*key, &entry).unwrap();
        set_modified(&cache.path(*key), 1_000 + i64::try_from(age).unwrap() * 10);
    }

    let left = directory.join(format!(".{}.1.0.tmp", keys[0]));
    std::fs::write(&left, vec![0; usize::try_from(length).unwrap()]).unwrap();
    set_modified(&left, 1_005);
    std::fs::write(directory.join("notes"), b"mine").unwrap();

    // Keys 0 and 1 are the oldest, but 0 is read now.
    assert!(matches!(cache.get(keys[0]), Lookup::Hit { .. }));

    let cache = ImageCache::open(&directory, 2 * length).unwrap();
    let mut expected = vec![
        format!("{}.image", keys[0]),
        format!("{}.image", keys[3]),
        "notes".to_owned(),
    ];
    expected.sort();
    assert_eq!(files(&cache), expected);
    let cache = ImageCache::open(&directory, 0).unwrap();
    assert_eq!(files(&cache), ["notes"]);
}

/// Writers and readers of one key at once never see part of an entry, and
/// leave no temporary files.
#[test]
fn concurrent_writers_and_readers_see_whole_entries() {
    let dir = ScratchDir::new("cache-concurrent");
    let cache = ImageCache::open(&dir.join("images"), DEFAULT_CAPACITY).unwrap();
    let key = Key::of(&[b"shared"]);
    let entries = [image(b"one"), image(b"two")];
    std::thread::scope(|scope| {
        for thread in 0..8 {
            let (cache, entries) = (&cache, &entries);
            scope.spawn(move || {
                for round in 0..50 {
                    cache.put(key, &entries[(thread + round) % 2]).unwrap();
                    let read = bytes_of(cache.get(key));
                    assert!(
                        entries.iter().any(|entry| entry.as_bytes() == read),
                        "a whole entry"
                    );
                }
            });
        }
    });
    assert_eq!(files(&cache), [format!("{key}.image")]);
}

/// A cache that cannot be written fails each write and misses each read,
/// which leaves loads uncached; one that cannot be created says so.
#[test]
fn an_unwritable_cache_fails_writes_and_misses() {
    let dir = ScratchDir::new("cache-unwritable");
    let cache = ImageCache::open(&dir.join("images"), DEFAULT_CAPACITY).unwrap();
    std::fs::set_permissions(cache.directory(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let key = Key::of(&[b"program"]);
    let error = cache.put(key, &image(b"views")).unwrap_err();
    assert_eq!(error.error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(matches!(cache.get(key), Lookup::Miss));
    assert!(files(&cache).is_empty());

    std::fs::write(dir.join("file"), b"").unwrap();
    assert!(matches!(
        ImageCache::open(&dir.join("file/images"), DEFAULT_CAPACITY),
        Err(CacheError::Open { .. })
    ));
}

#[test]
fn the_first_source_that_names_a_cache_wins() {
    let os = |text: &'static str| Some(OsStr::new(text));
    let dir = |text: &str| Some(PathBuf::from(text));
    let setting = Some(Setting::Directory("/chosen".into()));
    assert_eq!(
        resolve(setting, os("/variable"), os("/xdg"), os("/home"), true),
        dir("/chosen")
    );
    assert_eq!(
        resolve(Some(Setting::Off), os("/variable"), None, None, false),
        None
    );
    assert_eq!(
        resolve(None, os("/variable"), os("/xdg"), None, true),
        dir("/variable")
    );
    assert_eq!(resolve(None, os(""), os("/xdg"), None, false), None);
    assert_eq!(resolve(None, None, os("/xdg"), os("/home"), true), None);
    assert_eq!(
        resolve(None, None, os("/xdg"), os("/home"), false),
        dir("/xdg/microscope/images")
    );

    // A relative XDG directory is ignored, as the specification says.
    assert_eq!(
        resolve(None, None, os("xdg"), os("/home"), false),
        dir("/home/.cache/microscope/images")
    );
    assert_eq!(resolve(None, None, None, None, false), None);
}
