//! Tracks anonymous executable mappings, incremental `/tmp/perf-<pid>.map` reads,
//! and GDB JIT descriptors, rebuilding symbol chunks (`CHUNK`) only for address
//! ranges touched by samples.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::backend::perf::{self, jit::MapError};
use crate::debug_info::gdb_jit::{self, Registered};
use crate::debug_info::perf_map::{self, Jitted};

const CODE_READ: u64 = 64 * 1024;

const MAP_READ: u64 = 16 << 20;

const CODES_READ: u64 = 64 << 20;

const OBJECTS_READ: u64 = 64 << 20;

const QUIET: u64 = 20_000_000;

const CHUNK: u64 = 1 << 18;

#[derive(Debug, Clone, Default)]
struct Range {
    end: u64,

    built: BTreeMap<u64, (u64, u64)>,
}

#[derive(Debug, Default)]
pub struct Jit {
    ranges: BTreeMap<u64, Range>,

    functions: BTreeMap<u64, Named>,

    read: u64,
    path: Option<String>,

    read_to_end: u64,

    objects: BTreeMap<u64, JitCode>,
    descriptors: Option<Vec<u64>>,

    images: BTreeMap<u64, (u64, Vec<u8>)>,
    headers: Vec<u8>,

    pub names_only: bool,

    problem: Option<String>,

    quiet: u64,
}

#[derive(Debug)]
struct Named {
    function: Jitted,
    read: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitCode {
    pub start: u64,
    pub end: u64,

    pub path: String,
    pub bytes: Arc<[u8]>,
}

pub struct Stale {
    pub start: u64,
    pub end: u64,

    pub range: u64,
    pub path: String,
    pub functions: Vec<Jitted>,
}

#[derive(Default)]
pub struct Refreshed {
    pub stale: Vec<Stale>,
    pub objects: Vec<JitCode>,
}

impl Jit {
    pub fn mapped(&mut self, start: u64, end: u64, anonymous: bool, time: u64) {
        let overlapped: Vec<(u64, u64)> = self
            .ranges
            .range(..end)
            .rev()
            .take_while(|(_, held)| held.end > start)
            .map(|(&range, held)| (range, held.end))
            .collect();

        for (range, held) in overlapped {
            self.ranges.remove(&range);
            let remnants = [(range, start.min(held)), (end.max(range), held)];

            for (remnant, until) in remnants {
                if remnant < until {
                    self.ranges.insert(
                        remnant,
                        Range {
                            end: until,
                            ..Range::default()
                        },
                    );
                }
            }
        }

        self.functions.retain(|&function, named| {
            named.function.entry.end <= start || function >= end || named.read > time
        });

        let objects = self.objects.len();
        self.objects
            .retain(|_, object| object.end <= start || object.start >= end);
        if self.objects.len() != objects {
            self.headers.clear();
        }
        if !anonymous {
            self.descriptors = None;
        }
        self.quiet = 0;

        if anonymous {
            self.ranges.insert(
                start,
                Range {
                    end,
                    ..Range::default()
                },
            );
        }
    }

    pub fn exec(&mut self) {
        *self = Self {
            read: self.read,
            path: self.path.take(),
            ..Self::default()
        };
    }

    pub fn needs(&self, address: u64) -> bool {
        self.range(address).is_some_and(|(start, range)| {
            let (chunk, end) = self.chunk(start, range.end, address);
            let built = range
                .built
                .get(&chunk)
                .filter(|(built, _)| *built == end)
                .map(|&(_, read)| read);
            let named = self
                .functions
                .range(..=address)
                .next_back()
                .filter(|(_, named)| address < named.function.entry.end);
            match named {
                Some((_, named)) => built.is_none_or(|built| named.read > built),
                None => !self.covers(address),
            }
        })
    }

    /// Aligns a rebuild window to `CHUNK` boundaries without splitting a known
    /// function across two chunks, bounding re-indexing to the touched region.
    fn chunk(&self, start: u64, end: u64, address: u64) -> (u64, u64) {
        let cut = |boundary: u64| {
            if boundary <= start {
                return start;
            }
            if boundary >= end {
                return end;
            }
            self.functions
                .range(start..boundary)
                .next_back()
                .map_or(boundary, |(_, named)| {
                    named.function.entry.end.clamp(boundary, end)
                })
        };

        let mut lower = address & !(CHUNK - 1);
        while cut(lower) > address {
            lower -= CHUNK;
        }
        let mut upper = lower.saturating_add(CHUNK);
        while cut(upper) <= address {
            upper = upper.saturating_add(CHUNK);
        }
        (cut(lower), cut(upper))
    }

    pub fn unseen(&self, address: u64, time: u64) -> bool {
        self.range(address).is_none() && !self.covers(address) && time >= self.quiet
    }

    fn range(&self, address: u64) -> Option<(u64, &Range)> {
        self.ranges
            .range(..=address)
            .next_back()
            .filter(|(_, range)| address < range.end)
            .map(|(&start, range)| (start, range))
    }

    fn covers(&self, address: u64) -> bool {
        let function = self
            .functions
            .range(..=address)
            .next_back()
            .is_some_and(|(_, named)| address < named.function.entry.end);
        function
            || self
                .objects
                .range(..=address)
                .next_back()
                .is_some_and(|(_, object)| address < object.end)
    }

    pub fn refresh(
        &mut self,
        pid: u32,
        time: u64,
        addresses: impl IntoIterator<Item = u64>,
        descriptors: impl FnOnce() -> Vec<u64>,
    ) -> Refreshed {
        let mut uncovered = false;
        let mut needed = BTreeSet::new();
        let mut unknown = false;
        for address in addresses {
            if self.needs(address) {
                needed.extend(self.range(address).map(|(start, _)| (start, address)));
                uncovered |= !self.covers(address);
            } else {
                unknown |= self.range(address).is_none() && !self.covers(address);
            }
        }

        let mut refreshed = Refreshed::default();
        if (uncovered || unknown) && time >= self.quiet {
            let read_map = uncovered && time > self.read_to_end;
            let lines = read_map && self.read_on(pid);
            let descriptors = self.descriptors.get_or_insert_with(descriptors).clone();
            if !descriptors.is_empty() {
                refreshed.objects = self.read_objects(pid, &descriptors);
            }
            if !lines && refreshed.objects.is_empty() && (read_map || !uncovered) {
                self.quiet = time.saturating_add(QUIET);
            }
        }

        let chunks: BTreeSet<(u64, u64, u64)> = needed
            .into_iter()
            .filter_map(|(range, address)| {
                let end = self.ranges.get(&range)?.end;
                let (start, end) = self.chunk(range, end, address);
                Some((range, start, end))
            })
            .collect();
        for (range, start, end) in chunks {
            let Some(held) = self.ranges.get_mut(&range) else {
                continue;
            };
            let newest = self
                .functions
                .range(start..end)
                .map(|(_, named)| named.read)
                .max()
                .unwrap_or(0);
            if held.built.get(&start) == Some(&(end, newest)) {
                continue;
            }

            let objects = self
                .objects
                .range(..end)
                .next_back()
                .is_some_and(|(_, object)| object.end > start);
            let functions: Vec<Jitted> = match &self.path {
                Some(_) if !objects => self
                    .functions
                    .range(start..end)
                    .map(|(_, named)| named.function.clone())
                    .collect(),
                _ => Vec::new(),
            };
            held.built
                .retain(|&other, &mut (until, _)| until <= start || other >= end);
            held.built.insert(start, (end, newest));
            if let (false, Some(path)) = (functions.is_empty(), &self.path) {
                refreshed.stale.push(Stale {
                    start,
                    end,
                    range,
                    path: path.clone(),
                    functions,
                });
            }
        }

        refreshed
    }

    fn read_objects(&mut self, pid: u32, descriptors: &[u64]) -> Vec<JitCode> {
        let read = |address: u64, size: usize| {
            let mut bytes = vec![0; size];
            let read = perf::read_memory(pid, address, &mut bytes).ok()?;
            (read == size).then_some(bytes)
        };
        let mut headers = Vec::new();

        for &descriptor in descriptors {
            gdb_jit::witness(descriptor, read, &mut headers);
        }

        if headers == self.headers {
            return Vec::new();
        }

        let mut registered = Registered {
            known: self
                .images
                .iter()
                .filter(|(_, (start, _))| self.objects.contains_key(start))
                .map(|(&image, (_, outline))| (image, outline.clone()))
                .collect(),
            ..Registered::default()
        };
        let mut budget = OBJECTS_READ;

        for &descriptor in descriptors {
            gdb_jit::registered(descriptor, &mut budget, read, &mut registered);
        }

        if let Some(problem) = registered.problems.first() {
            self.problem = Some(problem.clone());
        }

        let mut objects = BTreeMap::new();
        let mut images = BTreeMap::new();
        let mut new = Vec::new();
        for image in registered.kept {
            if let Some(kept) = self.images.remove(&image) {
                if let Some(object) = self.objects.get(&kept.0) {
                    objects.insert(kept.0, object.clone());
                }
                images.insert(image, kept);
            }
        }

        for object in registered.objects {
            let path = object.name().to_string_lossy().into_owned();
            let Some(code) = object.code() else {
                self.problem
                    .get_or_insert_with(|| format!("registered object {path} places no code"));
                continue;
            };
            let size = object.bytes.len() as u64;
            let outline = gdb_jit::outline(size, |at, length| {
                let at = usize::try_from(at).ok()?;
                object
                    .bytes
                    .get(at..at.checked_add(length)?)
                    .map(<[u8]>::to_vec)
            })
            .unwrap_or_default();
            let code = JitCode {
                start: code.start,
                end: code.end,
                path,
                bytes: object.bytes.into(),
            };
            if self.objects.get(&code.start) != Some(&code) {
                new.push(code.clone());
            }
            images.insert(object.address, (code.start, outline));
            objects.insert(code.start, code);
        }

        self.objects = objects;
        self.images = images;
        self.headers = headers;
        new
    }

    pub const fn take_problem(&mut self) -> Option<String> {
        self.problem.take()
    }

    fn read_on(&mut self, pid: u32) -> bool {
        self.read_on_within(pid, MAP_READ, CODES_READ)
    }

    fn read_on_within(&mut self, pid: u32, limit: u64, mut codes: u64) -> bool {
        let (path, bytes) = match perf::jit::read_map(pid, self.read, limit) {
            Ok(read) => read,
            Err(MapError::Absent) => return false,
            Err(problem) => {
                self.problem = Some(problem.to_string());
                return false;
            }
        };

        self.path = Some(path);
        let read = super::session::monotonic();
        let (entries, used) = perf_map::parse(&bytes);
        if used == 0 && bytes.len() as u64 == limit {
            self.problem = Some(format!("a line of it is longer than {limit} bytes"));
        }
        if (bytes.len() as u64) < limit {
            self.read_to_end = read;
        }

        self.read += used as u64;
        let any = !entries.is_empty();
        let mut unread = 0_usize;

        for entry in entries {
            let length = (entry.end - entry.start).min(CODE_READ);
            let code = if self.names_only {
                None
            } else if length <= codes {
                codes -= length;
                let mut code = vec![0; usize::try_from(length).unwrap_or(0)];
                perf::read_memory(pid, entry.start, &mut code)
                    .ok()
                    .and_then(|read| code.get(..read.min(code.len())).map(Arc::from))
            } else {
                unread += 1;
                None
            };

            self.name(Jitted { entry, code }, read);
        }

        if unread > 0 {
            self.problem.get_or_insert_with(|| {
                format!("the code of {unread} of its functions was past what one read takes")
            });
        }

        any
    }

    fn name(&mut self, function: Jitted, read: u64) {
        let entry = &function.entry;
        let overlapped: Vec<u64> = self
            .functions
            .range(..entry.end)
            .rev()
            .take_while(|(_, earlier)| earlier.function.entry.end > entry.start)
            .map(|(&start, _)| start)
            .collect();

        for start in overlapped {
            self.functions.remove(&start);
        }

        self.functions
            .insert(function.entry.start, Named { function, read });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_info::perf_map::Entry;

    #[repr(align(64))]
    struct Code<const N: usize>([u8; N]);

    struct Removed(String);

    impl Drop for Removed {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn function(start: u64, end: u64) -> Jitted {
        Jitted {
            entry: Entry {
                start,
                end,
                name: "f".into(),
            },
            code: None,
        }
    }

    #[test]
    fn anonymous_code_needs_a_module_until_its_functions_have_one() {
        let mut jit = Jit::default();
        jit.mapped(0x1000, 0x2000, true, 0);
        jit.mapped(0x3000, 0x4000, false, 0);
        assert!(jit.needs(0x1000));
        assert!(!jit.needs(0x3000), "file-backed");
        assert!(!jit.needs(0x2000), "past the range");

        jit.functions.insert(
            0x1000,
            Named {
                function: function(0x1000, 0x1010),
                read: 0,
            },
        );
        jit.path = Some("/tmp/perf-1.map".into());
        let stale = jit.refresh(u32::MAX, 0, [0x1008], Vec::new).stale;
        assert_eq!(stale.len(), 1);
        assert_eq!((stale[0].start, stale[0].end), (0x1000, 0x2000));
        assert!(!jit.needs(0x1008), "covered and current");
        assert!(jit.needs(0x1010), "uncovered: the map may name it since");
        assert!(
            jit.refresh(u32::MAX, 0, [0x1008], Vec::new)
                .stale
                .is_empty()
        );

        jit.mapped(0x1800, 0x2800, false, 0);
        assert!(!jit.needs(0x1808));
        assert!(jit.needs(0x1008));
    }

    #[test]
    fn a_new_program_forgets_the_old_ones_code() {
        let mut jit = Jit::default();
        jit.mapped(0x1000, 0x2000, true, 0);
        jit.functions.insert(
            0x1000,
            Named {
                function: function(0x1000, 0x1010),
                read: 0,
            },
        );
        jit.read = 40;
        jit.exec();
        assert!(!jit.needs(0x1008));
        assert!(!jit.covers(0x1008));
        assert_eq!(jit.read, 40);
    }

    #[test]
    fn registered_objects_are_read_when_unnamed_code_runs() {
        let image = crate::debug_info::gdb_jit::test_image(0x7000_1000, 64);
        let entry = Box::new([0, 0, image.as_ptr() as u64, image.len() as u64]);
        let mut descriptor = Box::new([1_u64, 0, std::ptr::from_ref(&*entry) as u64]);
        let at = std::ptr::from_ref(&*descriptor) as u64;
        let pid = std::process::id();
        let code = 0x7000_1008;

        let mut jit = Jit::default();
        let mut time = 0;
        let mut refresh = |jit: &mut Jit, address, descriptors: &dyn Fn() -> Vec<u64>| {
            time += QUIET;
            jit.refresh(pid, time, [address], descriptors).objects
        };
        assert!(refresh(&mut jit, code, &Vec::new).is_empty());

        jit.mapped(0x5000, 0x6000, false, 0);
        let objects = refresh(&mut jit, code, &|| vec![at]);
        assert_eq!(objects.len(), 1);
        assert_eq!(
            (objects[0].start, objects[0].end),
            (0x7000_1000, 0x7000_1040)
        );
        assert!(refresh(&mut jit, code, &|| vec![at]).is_empty());
        assert!(
            refresh(&mut jit, 0x7000_2000, &|| vec![at]).is_empty(),
            "unchanged, so not read"
        );

        descriptor[0] = 1 | (1 << 32);
        assert!(refresh(&mut jit, 0x7000_2000, &|| vec![at]).is_empty());
        assert_eq!(jit.objects.len(), 1);

        jit.mapped(0x7000_0000, 0x7000_2000, true, 0);
        assert_eq!(refresh(&mut jit, code, &|| vec![at]).len(), 1);
    }

    #[test]
    fn an_object_registered_at_a_reused_entry_is_read() {
        let first = crate::debug_info::gdb_jit::test_image(0x7000_1000, 64);
        let second = crate::debug_info::gdb_jit::test_image(0x7000_3000, 64);
        let mut entry = Box::new([0, 0, first.as_ptr() as u64, first.len() as u64]);
        let descriptor = Box::new([1_u64, 0, std::ptr::from_ref(&*entry) as u64]);
        let at = std::ptr::from_ref(&*descriptor) as u64;
        let pid = std::process::id();

        let mut jit = Jit::default();
        let mut time = 0;
        let mut refresh = |jit: &mut Jit, address| {
            time += QUIET;
            jit.refresh(pid, time, [address], || vec![at]).objects
        };
        let objects = refresh(&mut jit, 0x7000_1008);
        assert_eq!(objects.len(), 1);

        entry[2] = second.as_ptr() as u64;
        entry[3] = second.len() as u64;
        std::hint::black_box(&entry);
        let objects = refresh(&mut jit, 0x7000_3008);
        assert_eq!(
            objects
                .iter()
                .map(|object| (object.start, object.end))
                .collect::<Vec<_>>(),
            [(0x7000_3000, 0x7000_3040)]
        );
        drop(descriptor);
    }

    #[test]
    fn an_object_listed_as_it_was_is_not_read_again() {
        let mut first = crate::debug_info::gdb_jit::test_image(0x7000_1000, 64);
        let original = first.clone();
        let second = crate::debug_info::gdb_jit::test_image(0x7000_3000, 64);
        let mut later = Box::new([0, 0, second.as_ptr() as u64, second.len() as u64]);
        let mut entry = Box::new([0, 0, first.as_ptr() as u64, first.len() as u64]);
        let mut descriptor = Box::new([1_u64, 0, std::ptr::from_ref(&*entry) as u64]);
        let at = std::ptr::from_ref(&*descriptor) as u64;
        let pid = std::process::id();

        let mut jit = Jit::default();
        let mut time = 0;
        let mut refresh = |jit: &mut Jit, address| {
            time += QUIET;
            let objects = jit.refresh(pid, time, [address], || vec![at]).objects;
            objects
                .iter()
                .map(|object| object.start)
                .collect::<Vec<_>>()
        };
        assert_eq!(refresh(&mut jit, 0x7000_1008), [0x7000_1000]);

        first[64..128].fill(0x90);
        later[0] = std::ptr::from_ref(&*entry) as u64;
        descriptor[2] = std::ptr::from_ref(&*later) as u64;
        std::hint::black_box((&first, &descriptor));
        assert_eq!(refresh(&mut jit, 0x7000_3008), [0x7000_3000]);
        assert_eq!(
            jit.objects.get(&0x7000_1000).map(|object| &*object.bytes),
            Some(original.as_slice()),
            "read again"
        );

        first.copy_from_slice(&crate::debug_info::gdb_jit::test_image(0x7000_5000, 64));
        entry[1] = 1;
        std::hint::black_box((&first, &entry));
        assert_eq!(refresh(&mut jit, 0x7000_5008), [0x7000_5000]);
        assert!(!jit.objects.contains_key(&0x7000_1000));
    }

    #[test]
    fn a_read_of_the_map_is_bounded() {
        static CODE: Code<32> = Code([0xcc; 32]);
        let pid = std::process::id();
        let path = format!("/tmp/perf-{pid}.map");
        let start = CODE.0.as_ptr() as u64;
        let map = format!("{start:x} 10 first\n{:x} 10 second\n", start + 0x10);
        std::fs::write(&path, &map).expect("write the map");
        let _removed = Removed(path);
        let first = map.find('\n').expect("a line") as u64 + 1;

        let mut jit = Jit::default();
        assert!(jit.read_on_within(pid, first + 4, CODES_READ));
        assert_eq!(jit.functions.len(), 1);
        assert_eq!(jit.read, first);
        assert!(jit.read_on_within(pid, MAP_READ, CODES_READ));
        assert_eq!(jit.functions.len(), 2);
        assert!(jit.take_problem().is_none());

        let mut jit = Jit::default();
        assert!(jit.read_on_within(pid, MAP_READ, 0x10));
        let codes: Vec<bool> = jit
            .functions
            .values()
            .map(|named| named.function.code.is_some())
            .collect();
        assert_eq!(codes, [true, false]);
        let problem = jit.take_problem().expect("a problem");
        assert!(
            problem.contains("the code of 1 of its functions"),
            "{problem}"
        );

        let mut jit = Jit::default();
        assert!(!jit.read_on_within(pid, 4, CODES_READ));
        let problem = jit.take_problem().expect("a problem");
        assert!(problem.contains("longer than 4 bytes"), "{problem}");
    }

    #[test]
    fn a_range_is_made_again_only_for_a_function_its_module_lacks() {
        static CODE: Code<64> = Code([0xcc; 64]);
        let pid = std::process::id();
        let path = format!("/tmp/perf-{pid}.map");
        let start = CODE.0.as_ptr() as u64;
        let other = start + 0x20;
        std::fs::write(&path, format!("{start:x} 10 a\n")).expect("write the map");
        let _removed = Removed(path.clone());
        let append = |line: String| {
            let mut map = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open the map");
            std::io::Write::write_all(&mut map, line.as_bytes()).expect("append to the map");
        };
        let names = |refreshed: Refreshed| -> Vec<Vec<String>> {
            refreshed
                .stale
                .iter()
                .map(|stale| {
                    stale
                        .functions
                        .iter()
                        .map(|function| function.entry.name.to_string())
                        .collect()
                })
                .collect()
        };
        let now = super::super::session::monotonic;

        let mut jit = Jit {
            names_only: true,
            ..Jit::default()
        };
        jit.mapped(start, other, true, 0);
        jit.mapped(other, start + 0x40, true, 0);
        assert_eq!(
            names(jit.refresh(pid, now(), [start + 8], Vec::new)),
            [["a"]]
        );

        append(format!("{:x} 10 b\n{other:x} 10 c\n", start + 0x10));
        assert_eq!(
            names(jit.refresh(pid, now(), [other + 8], Vec::new)),
            [["c"]]
        );
        assert!(
            jit.refresh(pid, now(), [start + 8], Vec::new)
                .stale
                .is_empty(),
            "the module names a"
        );
        assert_eq!(
            names(jit.refresh(pid, now(), [start + 0x18], Vec::new)),
            [["a", "b"]]
        );

        append(format!("{start:x} 10 a2\n"));
        assert!(
            jit.refresh(pid, now(), [other + 0x18], Vec::new)
                .stale
                .is_empty()
        );
        assert_eq!(
            names(jit.refresh(pid, now(), [start + 8], Vec::new)),
            [["a2", "b"]]
        );
        assert!(
            jit.functions
                .values()
                .all(|named| named.function.code.is_none())
        );
    }

    #[test]
    fn a_new_function_makes_again_only_the_chunk_that_holds_it() {
        let pid = std::process::id();
        let path = format!("/tmp/perf-{pid}.map");
        let start = 0x7f00_0000_0000_u64;
        let (a, b, c, d) = (
            start + 0x100,
            start + CHUNK - 0x10,
            start + 2 * CHUNK + 0x100,
            start + 2 * CHUNK + 0x200,
        );
        std::fs::write(&path, format!("{a:x} 10 a\n{b:x} 40 b\n{c:x} 10 c\n"))
            .expect("write the map");
        let _removed = Removed(path.clone());
        let stale = |refreshed: Refreshed| -> Vec<(u64, u64, u64, Vec<String>)> {
            refreshed
                .stale
                .into_iter()
                .map(|stale| {
                    let names = stale
                        .functions
                        .iter()
                        .map(|function| function.entry.name.to_string())
                        .collect();
                    (stale.range, stale.start, stale.end, names)
                })
                .collect()
        };
        let now = super::super::session::monotonic;

        let mut jit = Jit {
            names_only: true,
            ..Jit::default()
        };
        jit.mapped(start, start + 3 * CHUNK, true, 0);
        let b_ends = b + 0x40;
        assert_eq!(
            stale(jit.refresh(pid, now(), [a + 8], Vec::new)),
            [(start, start, b_ends, vec!["a".into(), "b".into()])]
        );
        assert!(
            !jit.needs(b + 0x38),
            "b's end, past the chunk's, is in the module made"
        );
        assert_eq!(
            stale(jit.refresh(pid, now(), [c + 8], Vec::new)),
            [(
                start,
                start + 2 * CHUNK,
                start + 3 * CHUNK,
                vec!["c".into()]
            )]
        );

        let mut map = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open the map");
        std::io::Write::write_all(&mut map, format!("{d:x} 10 d\n").as_bytes())
            .expect("append to the map");
        assert_eq!(
            stale(jit.refresh(pid, now(), [d + 8, a + 8], Vec::new)),
            [(
                start,
                start + 2 * CHUNK,
                start + 3 * CHUNK,
                vec!["c".into(), "d".into()]
            )]
        );
    }

    #[test]
    fn the_map_is_read_on_only_for_samples_taken_after_it_was_last_read() {
        static CODE: Code<64> = Code([0xcc; 64]);
        let pid = std::process::id();
        let path = format!("/tmp/perf-{pid}.map");
        let start = CODE.0.as_ptr() as u64;
        let second = start + 0x20;
        std::fs::write(&path, format!("{start:x} 10 a\n{second:x} 10 b\n")).expect("write the map");
        let _removed = Removed(path.clone());
        let starts = |refreshed: Refreshed| -> Vec<u64> {
            refreshed.stale.iter().map(|stale| stale.start).collect()
        };

        let mut jit = Jit::default();
        jit.mapped(start, second, true, 0);
        jit.mapped(second, start + 0x40, true, 0);
        let taken = super::super::session::monotonic();
        assert_eq!(
            starts(jit.refresh(pid, taken, [start + 8], Vec::new)),
            [start]
        );
        assert_eq!(
            starts(jit.refresh(pid, taken, [second + 8], Vec::new)),
            [second]
        );

        let mut map = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open the map");
        std::io::Write::write_all(&mut map, format!("{:x} 10 c\n", start + 0x10).as_bytes())
            .expect("append to the map");
        assert_eq!(
            starts(jit.refresh(pid, taken, [start + 0x18], Vec::new)),
            Vec::<u64>::new()
        );
        assert_eq!(jit.functions.len(), 2);

        let later = super::super::session::monotonic();
        assert_eq!(
            starts(jit.refresh(pid, later, [start + 0x18], Vec::new)),
            [start]
        );
        assert_eq!(jit.functions.len(), 3);
    }

    #[test]
    fn a_registered_object_that_places_no_code_is_a_problem() {
        let image = crate::debug_info::gdb_jit::test_image(0x7000_1000, 0);
        let entry = Box::new([0, 0, image.as_ptr() as u64, image.len() as u64]);
        let descriptor = Box::new([1_u64, 0, std::ptr::from_ref(&*entry) as u64]);
        let at = std::ptr::from_ref(&*descriptor) as u64;
        let mut jit = Jit::default();
        let pid = std::process::id();
        assert!(
            jit.refresh(pid, 0, [0x7000_1008], || vec![at])
                .objects
                .is_empty()
        );
        let problem = jit.take_problem().expect("a problem");
        assert!(problem.contains("places no code"), "{problem}");
    }

    const PAGE: u64 = 4096;
    const UNIT: u64 = 16;
    const PAGES: usize = 3 * (CHUNK / PAGE) as usize;
    const BASE: u64 = 0x7f00_0000_0000;

    #[derive(Debug, Clone)]
    enum Step {
        Map {
            start: usize,
            pages: usize,
            anonymous: bool,
            time: u64,
        },
        Name {
            start: u64,
            units: u64,
            read: u64,
        },
    }

    fn step() -> impl proptest::strategy::Strategy<Value = Step> {
        use proptest::prelude::*;
        let page = 0..PAGES;
        let pages = prop_oneof![1..4_usize, 1..=PAGES];
        let unit = 0..(PAGES as u64 * PAGE / UNIT);
        let units = prop_oneof![1..8_u64, 1..(2 * CHUNK / UNIT)];
        prop_oneof![
            (page, pages, any::<bool>(), 0..4_u64).prop_map(|(start, pages, anonymous, time)| {
                Step::Map {
                    start,
                    pages,
                    anonymous,
                    time,
                }
            }),
            (unit, units, 0..4_u64).prop_map(|(start, units, read)| Step::Name {
                start,
                units,
                read,
            }),
        ]
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

        /// After any mappings and names, each page is in the anonymous
        /// range of the last mapping that covered it, if that was
        /// anonymous, as far as that mapping still holds it; each address
        /// is named by the last function over it unless a mapping since
        /// ended it; and each address's chunk holds it, every address of
        /// the chunk has that chunk, and no function begun in the range is
        /// cut by one.
        #[test]
        fn ranges_and_chunks_follow_the_last_mapping_of_each_page(
            steps in proptest::collection::vec(step(), 1..24),
        ) {
            use proptest::prelude::*;
            let mut jit = Jit::default();

            let mut mapped: Vec<Option<usize>> = vec![None; PAGES];
            let mut named: Vec<(u64, u64, u64)> = Vec::new();

            for (index, step) in steps.iter().enumerate() {
                match *step {
                    Step::Map { start, pages, anonymous, time } => {
                        let end = (start + pages).min(PAGES);
                        let (from, to) = (BASE + start as u64 * PAGE, BASE + end as u64 * PAGE);
                        jit.mapped(from, to, anonymous, time);
                        for page in &mut mapped[start..end] {
                            *page = Some(index);
                        }

                        named.retain(|&(start, end, read)| end <= from || start >= to || read > time);
                    }
                    Step::Name { start, units, read } => {
                        let start = BASE + start * UNIT;
                        let end = start + units * UNIT;
                        jit.name(function(start, end), read);
                        named.retain(|&(earlier, until, _)| until <= start || earlier >= end);
                        named.push((start, end, read));
                    }
                }
            }

            let anonymous = |page: usize| {
                mapped[page].is_some_and(|step| matches!(steps[step], Step::Map { anonymous: true, .. }))
            };

            let ranges: Vec<(u64, u64)> = jit.ranges.iter().map(|(&start, range)| (start, range.end)).collect();
            prop_assert!(ranges.iter().all(|(start, end)| start < end));
            prop_assert!(ranges.windows(2).all(|pair| pair[0].1 <= pair[1].0), "{:x?}", ranges);

            for page in 0..PAGES {
                let address = BASE + page as u64 * PAGE;
                let found = jit.range(address).map(|(start, range)| (start, range.end));
                let expected = anonymous(page).then(|| {
                    let same = |other: &usize| anonymous(*other) && mapped[*other] == mapped[page];
                    let first = (0..=page).rev().take_while(same).last().unwrap_or(page);
                    let last = (page..PAGES).take_while(same).last().unwrap_or(page);
                    (BASE + first as u64 * PAGE, BASE + (last + 1) as u64 * PAGE)
                });
                prop_assert_eq!(found, expected, "page {}", page);
            }

            let mut addresses: Vec<u64> = named
                .iter()
                .flat_map(|&(start, end, _)| [start - UNIT, start, end - UNIT, end])
                .chain((0..PAGES as u64).map(|page| BASE + page * PAGE))
                .collect();
            addresses.sort_unstable();
            addresses.dedup();

            for &address in &addresses {
                let covered = named.iter().any(|&(start, end, _)| start <= address && address < end);
                prop_assert_eq!(jit.covers(address), covered, "{:#x}", address);

                let Some((start, range)) = jit.range(address) else {
                    continue;
                };
                let (lower, upper) = jit.chunk(start, range.end, address);
                prop_assert!(start <= lower && lower <= address && address < upper && upper <= range.end,
                    "{:#x} in {:#x}..{:#x}", address, lower, upper);
                prop_assert_eq!(jit.chunk(start, range.end, lower), (lower, upper));
                prop_assert_eq!(jit.chunk(start, range.end, upper - 1), (lower, upper));

                for &(begins, ends, _) in named.iter().filter(|(begins, _, _)| (start..range.end).contains(begins)) {
                    for bound in [lower, upper].into_iter().filter(|&bound| bound != range.end) {
                        prop_assert!(bound <= begins || bound >= ends,
                            "{:#x}..{:#x} cuts {:#x}..{:#x}", lower, upper, begins, ends);
                    }
                }
            }
        }
    }

    #[test]
    fn code_nothing_names_is_read_for_at_most_once_a_while() {
        let image = crate::debug_info::gdb_jit::test_image(0x7000_1000, 64);
        let entry = Box::new([0, 0, image.as_ptr() as u64, image.len() as u64]);
        let mut descriptor = Box::new([1_u64, 0, 0]);
        let at = std::ptr::from_ref(&*descriptor) as u64;
        let pid = std::process::id();
        let code = 0x7000_1008;

        let mut jit = Jit::default();
        assert!(jit.refresh(pid, 0, [code], || vec![at]).objects.is_empty());
        descriptor[2] = std::ptr::from_ref(&*entry) as u64;
        assert!(
            jit.refresh(pid, QUIET - 1, [code], || vec![at])
                .objects
                .is_empty(),
            "quiet"
        );
        assert_eq!(
            jit.refresh(pid, QUIET, [code], || vec![at]).objects.len(),
            1
        );

        assert!(
            jit.refresh(pid, QUIET, [code], || vec![at])
                .objects
                .is_empty()
        );
        jit.mapped(0x7000_0000, 0x7000_2000, true, 0);
        assert_eq!(
            jit.refresh(pid, QUIET + 1, [code], || vec![at])
                .objects
                .len(),
            1
        );
    }
}
