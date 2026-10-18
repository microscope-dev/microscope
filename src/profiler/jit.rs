//! Each recorded process's anonymous code, and what names the functions a
//! compiler wrote there: the perf map it keeps, whose functions in one
//! anonymous range become a module of its own (see
//! [`crate::debug_info::perf_map`]), and the objects it registers through
//! the GDB JIT interface, each a module of its own with the call-frame
//! information it carries (see [`crate::debug_info::gdb_jit`]).
//!
//! The map is append-only and written as it is read, so it is read on
//! from where it was last read, and only when a sample lands in anonymous
//! code nothing names. A function's code is read with it: its frames are
//! proved from the code as it is then. The registered objects are read
//! again then too, when their descriptors changed since they were last
//! read. A range that holds registered objects is theirs alone.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::backend::perf::{self, jit::MapError};
use crate::debug_info::gdb_jit::{self, Registered};
use crate::debug_info::perf_map::{self, Jitted};

/// The most of one function's code read: its frames are proved from its
/// entry, so a frame further in than this has none.
const CODE_READ: u64 = 64 * 1024;

/// The most bytes of registered objects read at once.
const OBJECTS_READ: u64 = 64 << 20;

/// One anonymous executable mapping.
#[derive(Debug, Clone, Copy)]
struct Range {
    end: u64,

    /// Whether its module holds every function the map named in it.
    current: bool,
}

/// What a process's anonymous code is known to hold.
#[derive(Debug, Default)]
pub struct Jit {
    ranges: BTreeMap<u64, Range>,

    /// Each function the map named, by start, a later one replacing those
    /// it overlaps.
    functions: BTreeMap<u64, Jitted>,

    /// How many bytes of the map are read, and the path the process
    /// names it by, once it has been read.
    read: u64,
    path: Option<String>,

    /// Each registered object, by where its code starts; the descriptors
    /// they are registered with, until the process maps something; and
    /// the descriptors' bytes as they were when the objects were read.
    objects: BTreeMap<u64, JitCode>,
    descriptors: Option<Vec<u64>>,
    headers: Vec<u8>,

    /// Why the map, or a registered object, was last not read, until it is
    /// taken.
    problem: Option<String>,
}

/// The code of an object a compiler registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitCode {
    pub start: u64,
    pub end: u64,

    /// The object's name, and the object.
    pub path: String,
    pub bytes: Arc<[u8]>,
}

/// An anonymous range whose module is out of date, with the functions in
/// it.
pub struct Stale {
    pub start: u64,
    pub end: u64,
    pub path: String,
    pub functions: Vec<Jitted>,
}

/// The modules a refresh found out of date: each anonymous range's whose
/// functions a perf map names, and each registered object's.
#[derive(Default)]
pub struct Refreshed {
    pub stale: Vec<Stale>,
    pub objects: Vec<JitCode>,
}

impl Jit {
    /// Records a mapping of `start..end`, which replaces whatever it
    /// overlaps, and is anonymous code when `anonymous`.
    pub fn mapped(&mut self, start: u64, end: u64, anonymous: bool) {
        self.ranges
            .retain(|&range, held| held.end <= start || range >= end);

        // The code of an object it overlaps is gone, or another's: what the
        // compiler registers there is read and made again. What it maps may
        // hold a descriptor.
        let objects = self.objects.len();
        self.objects
            .retain(|_, object| object.end <= start || object.start >= end);
        if self.objects.len() != objects {
            self.headers.clear();
        }
        self.descriptors = None;

        if anonymous {
            self.ranges.insert(
                start,
                Range {
                    end,
                    current: false,
                },
            );
        }
    }

    /// Whether the anonymous range that holds `address`, if any, needs its
    /// module made again: the map named functions in it since, or none
    /// covers `address`, so the map may have more.
    pub fn needs(&self, address: u64) -> bool {
        self.range(address)
            .is_some_and(|(_, range)| !range.current || !self.covers(address))
    }

    fn range(&self, address: u64) -> Option<(u64, Range)> {
        self.ranges
            .range(..=address)
            .next_back()
            .filter(|(_, range)| address < range.end)
            .map(|(&start, &range)| (start, range))
    }

    fn covers(&self, address: u64) -> bool {
        let function = self
            .functions
            .range(..=address)
            .next_back()
            .is_some_and(|(_, function)| address < function.entry.end);
        function
            || self
                .objects
                .range(..=address)
                .next_back()
                .is_some_and(|(_, object)| address < object.end)
    }

    /// Reads the map of process `pid` on, and the objects registered with
    /// the `descriptors` it gives, when some sampled `addresses`, which lie
    /// in no module the recording holds, are code nothing names, and
    /// returns the modules out of date. Each range's module is made again
    /// from what it returns, which marks it current. An address in no
    /// anonymous range the recording saw mapped may be in code whose
    /// mapping it lost, which a registered object still names.
    pub fn refresh(
        &mut self,
        pid: u32,
        addresses: impl IntoIterator<Item = u64>,
        descriptors: impl FnOnce() -> Vec<u64>,
    ) -> Refreshed {
        let mut uncovered = false;
        let mut needed = false;
        let mut unknown = false;
        for address in addresses {
            if self.needs(address) {
                needed = true;
                uncovered |= !self.covers(address);
            } else {
                unknown |= self.range(address).is_none() && !self.covers(address);
            }
        }

        let mut refreshed = Refreshed::default();
        if uncovered {
            self.read_on(pid);
        }

        if uncovered || unknown {
            let descriptors = self.descriptors.get_or_insert_with(descriptors).clone();
            if !descriptors.is_empty() {
                refreshed.objects = self.read_objects(pid, &descriptors);
            }
        }

        if !needed {
            return refreshed;
        }

        for (&start, range) in &mut self.ranges {
            if range.current {
                continue;
            }

            range.current = true;
            let objects = self.objects.range(start..range.end).next().is_some();
            let functions: Vec<Jitted> = match &self.path {
                Some(_) if !objects => self
                    .functions
                    .range(start..range.end)
                    .map(|(_, function)| function.clone())
                    .collect(),
                _ => Vec::new(),
            };
            if let (false, Some(path)) = (functions.is_empty(), &self.path) {
                refreshed.stale.push(Stale {
                    start,
                    end: range.end,
                    path: path.clone(),
                    functions,
                });
            }
        }

        refreshed
    }

    /// Reads again the objects registered with `descriptors` when any of
    /// them changed since they were read, and returns each object not
    /// known before.
    fn read_objects(&mut self, pid: u32, descriptors: &[u64]) -> Vec<JitCode> {
        let read = |address: u64, size: usize| {
            let mut bytes = vec![0; size];
            let read = perf::read_memory(pid, address, &mut bytes).ok()?;
            (read == size).then_some(bytes)
        };
        let mut headers = Vec::new();
        for &descriptor in descriptors {
            headers.extend(read(descriptor, gdb_jit::DESCRIPTOR_BYTES).unwrap_or_default());
        }

        if headers == self.headers {
            return Vec::new();
        }

        let mut registered = Registered::default();
        let mut budget = OBJECTS_READ;
        for &descriptor in descriptors {
            gdb_jit::registered(descriptor, &mut budget, read, &mut registered);
        }

        if let Some(problem) = registered.problems.first() {
            self.problem = Some(problem.clone());
        }

        let mut objects = BTreeMap::new();
        for object in registered.objects {
            if let Some(code) = object.code() {
                let path = object.name().to_string_lossy().into_owned();
                objects.insert(
                    code.start,
                    JitCode {
                        start: code.start,
                        end: code.end,
                        path,
                        bytes: object.bytes.into(),
                    },
                );
            }
        }

        let new = objects
            .iter()
            .filter(|(start, object)| self.objects.get(start) != Some(object))
            .map(|(_, object)| object.clone())
            .collect();
        self.objects = objects;
        self.headers = headers;
        new
    }

    /// Why the map was not read, the first time it is asked after a read
    /// found a map that cannot be used.
    pub const fn take_problem(&mut self) -> Option<String> {
        self.problem.take()
    }

    /// Reads the lines written since the last read, with each new
    /// function's code.
    fn read_on(&mut self, pid: u32) {
        let (path, bytes) = match perf::jit::read_map(pid, self.read) {
            Ok(read) => read,
            Err(MapError::Absent) => return,
            Err(problem) => {
                self.problem = Some(problem.to_string());
                return;
            }
        };

        self.path = Some(path);
        let (entries, used) = perf_map::parse(&bytes);
        self.read += used as u64;
        for entry in entries {
            let length = (entry.end - entry.start).min(CODE_READ);
            let mut code = vec![0; usize::try_from(length).unwrap_or(0)];
            let code = perf::read_memory(pid, entry.start, &mut code)
                .ok()
                .map(|read| Arc::from(&code[..read.min(code.len())]));
            let overlapped: Vec<u64> = self
                .functions
                .range(..entry.end)
                .rev()
                .take_while(|(_, earlier)| earlier.entry.end > entry.start)
                .map(|(&start, _)| start)
                .collect();
            for start in overlapped {
                self.functions.remove(&start);
            }

            if let Some((_, range)) = self.ranges.range_mut(..=entry.start).next_back()
                && entry.start < range.end
            {
                range.current = false;
            }

            self.functions.insert(entry.start, Jitted { entry, code });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_info::perf_map::Entry;

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
        jit.mapped(0x1000, 0x2000, true);
        jit.mapped(0x3000, 0x4000, false);
        assert!(jit.needs(0x1000));
        assert!(!jit.needs(0x3000), "file-backed");
        assert!(!jit.needs(0x2000), "past the range");

        // Functions read since, as the map would give them.
        jit.functions.insert(0x1000, function(0x1000, 0x1010));
        jit.path = Some("/tmp/perf-1.map".into());
        let stale = jit.refresh(u32::MAX, [0x1008], Vec::new).stale;
        assert_eq!(stale.len(), 1);
        assert_eq!((stale[0].start, stale[0].end), (0x1000, 0x2000));
        assert!(!jit.needs(0x1008), "covered and current");
        assert!(jit.needs(0x1010), "uncovered: the map may name it since");
        assert!(jit.refresh(u32::MAX, [0x1008], Vec::new).stale.is_empty());

        // A mapping over the range replaces it.
        jit.mapped(0x1800, 0x2800, false);
        assert!(!jit.needs(0x1008));
    }

    /// A process's registered objects are read when a sample lands in code
    /// nothing names, whether or not its mapping was seen; again only once
    /// the descriptor changed, or a mapping replaced an object's code; and
    /// never in a process with no descriptor. This process registers one.
    #[test]
    fn registered_objects_are_read_when_unnamed_code_runs() {
        let image = crate::debug_info::gdb_jit::test_image(0x7000_1000, 64);
        let entry = Box::new([0, 0, image.as_ptr() as u64, image.len() as u64]);
        let mut descriptor = Box::new([1_u64, 0, std::ptr::from_ref(&*entry) as u64]);
        let at = std::ptr::from_ref(&*descriptor) as u64;
        let pid = std::process::id();
        let code = 0x7000_1008;

        let mut jit = Jit::default();
        assert!(jit.refresh(pid, [code], Vec::new).objects.is_empty());

        // What the process maps since may hold a descriptor.
        jit.mapped(0x5000, 0x6000, false);
        let objects = jit.refresh(pid, [code], || vec![at]).objects;
        assert_eq!(objects.len(), 1);
        assert_eq!(
            (objects[0].start, objects[0].end),
            (0x7000_1000, 0x7000_1040)
        );
        assert!(jit.refresh(pid, [code], || vec![at]).objects.is_empty());
        assert!(
            jit.refresh(pid, [0x7000_2000], || vec![at])
                .objects
                .is_empty(),
            "unchanged, so not read"
        );

        // A changed descriptor is read, and holds nothing new.
        descriptor[0] = 1 | (1 << 32);
        assert!(
            jit.refresh(pid, [0x7000_2000], || vec![at])
                .objects
                .is_empty()
        );
        assert_eq!(jit.objects.len(), 1);

        // A mapping over its code: what is there now is made again.
        jit.mapped(0x7000_0000, 0x7000_2000, true);
        assert_eq!(jit.refresh(pid, [code], || vec![at]).objects.len(), 1);
    }
}
